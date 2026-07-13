//! ext-terminal (REV-002) — the agent side of the remote terminal
//! (build item B6).
//!
//! Normative source: spec/reeve/03-terminal.md §5 (guardrails-first;
//! CLAUDE.md "Remote terminal (guardrails)" is the MUST-level
//! summary). What this module implements:
//! - §5.1 transport: one 02-channel §4 sub-channel per session,
//!   purpose `rev-002/terminal`, opened by the SERVER (even id).
//!   `open.meta` carries only session bootstrap
//!   ([`TerminalOpenMeta`]: sessionId, PTY size, TERM); resize rides
//!   in-band ([`decode_terminal_payload`]).
//! - §5.2 enablement: DISABLED by default. The gate is fed ONLY from
//!   desired state — [`sync_enablement`] reads
//!   [`TERMINAL_CONFIG_PATH`] out of the swapped render bundle after
//!   every converge pass. No runtime toggle exists: no API, env var,
//!   or channel message flips it. While offline the last converged
//!   state governs (Law 5). Converging to a disabling commit
//!   terminates live sessions.
//! - §5.3 lifecycle: sessions are short-lived and explicitly
//!   initiated (the agent never opens one — it only accepts or
//!   rejects). Idle timeout and hard cap enforced agent-side
//!   (RECOMMENDED 5 min / 60 min, from [`TerminalConfig`]). Any leg
//!   failure kills the whole session: sub-channel `close`, channel
//!   teardown, or PTY child exit each end it, and `kill -9` of the
//!   agent leaves nothing resumable — the PTY dies with its process
//!   (Law 3; no session state is persisted anywhere).
//!   The PTY runs under the agent's own process identity — the same
//!   workload-execution identity the compose provider applies
//!   workloads with; there is no privilege-switching path here.
//! - §5.4 audit is reeve-server's duty (its SQLite DB); the agent
//!   contributes structured logs only.
//! - Resource limits (02-channel §4.7): frame size and the
//!   sub-channel cap are enforced by ext::channel; one PTY per
//!   sub-channel by construction here.
//!
//! Feature graph: `ext-terminal = ["ext-channel"]` — the terminal is
//! a sub-channel consumer, never its own socket (02-channel §4.2).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use reeve_types::reeve::channel::PURPOSE_TERMINAL;
use reeve_types::reeve::terminal::{
    TERMINAL_CONFIG_PATH, TerminalConfig, TerminalOpenMeta, TerminalPayload,
    decode_terminal_payload, encode_terminal_data,
};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::ext::channel::{SubChannelConsumer, SubChannelHandler, SubChannelRegistry, SubChannelTx};

/// Portable fallback shell — present on essentially every Linux box
/// (§5.3: `/bin/sh` is the floor).
pub const DEFAULT_SHELL: &str = "/bin/sh";

/// Shells preferred over the `/bin/sh` floor when enablement config
/// names no explicit shell. First one that exists on the device wins;
/// `/bin/sh` is the guaranteed fallback.
const PREFERRED_SHELLS: &[&str] = &["/bin/bash", "/usr/bin/bash"];

/// Resolve the shell to spawn: the enablement config's explicit `shell`
/// if set (per-scope — a fleet/site/device layer may set it, §11.1);
/// otherwise a nicer shell (bash) if the device has one, else the
/// portable `/bin/sh` floor.
fn resolve_shell(config: &TerminalConfig) -> String {
    if let Some(s) = config.shell.as_deref()
        && !s.trim().is_empty()
    {
        return s.to_string();
    }
    PREFERRED_SHELLS
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| p.to_string())
        .unwrap_or_else(|| DEFAULT_SHELL.to_string())
}

/// Watchdog tick for the §5.3 idle/hard-cap limits — granularity,
/// not a protocol value.
const WATCHDOG_TICK: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------
// Enablement gate (§5.2)
// ---------------------------------------------------------------

/// The enablement gate + live-session registry. One per agent,
/// shared between the sub-channel handler (accept/reject at open)
/// and the converge loop ([`sync_enablement`] after every pass).
///
/// Starts DISABLED — a freshly enrolled device MUST refuse
/// `rev-002/terminal` opens until a desired-state revision enables
/// it (§5.2).
pub struct TerminalGate {
    inner: Mutex<GateState>,
}

struct GateState {
    config: TerminalConfig,
    sessions: BTreeMap<u32, SessionHandle>,
}

/// The gate's grip on one live session: enough to kill it (§5.2
/// disablement, §5.3 teardown) and to say who it was.
struct SessionHandle {
    session_id: String,
    pid: Option<u32>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    close_reason: Arc<Mutex<Option<String>>>,
}

impl TerminalGate {
    /// A fresh gate: terminal DISABLED, no sessions (§5.2
    /// default-deny).
    pub fn new() -> Arc<Self> {
        Arc::new(TerminalGate {
            inner: Mutex::new(GateState {
                config: TerminalConfig::default(),
                sessions: BTreeMap::new(),
            }),
        })
    }

    /// Create a gate and register its `rev-002/terminal` handler on
    /// the channel's sub-channel registry — the one integration
    /// point (main.rs, before `channel::spawn`).
    pub fn install(registry: &mut SubChannelRegistry) -> Arc<Self> {
        let gate = Self::new();
        registry.register(PURPOSE_TERMINAL, Arc::new(TerminalHandler { gate: gate.clone() }));
        gate
    }

    /// Current enablement snapshot.
    pub fn config(&self) -> TerminalConfig {
        self.lock().config.clone()
    }

    /// Replace the enablement config. NOT public: the only writers
    /// are [`sync_enablement`] (desired state, §5.2) and tests.
    pub(crate) fn set_config(&self, config: TerminalConfig) {
        self.lock().config = config;
    }

    /// Number of live sessions.
    pub fn session_count(&self) -> usize {
        self.lock().sessions.len()
    }

    /// PID of the session on sub-channel `id`, if the platform
    /// reported one (tests assert the child actually dies).
    pub fn session_pid(&self, id: u32) -> Option<u32> {
        self.lock().sessions.get(&id).and_then(|s| s.pid)
    }

    /// Kill every live session (§5.2: converging to a disabling
    /// commit MUST terminate any live session). The PTY children get
    /// killed here; each session's reader/pump then observes EOF and
    /// closes its sub-channel with `reason`.
    pub fn kill_all(&self, reason: &str) {
        let mut state = self.lock();
        if state.sessions.is_empty() {
            return;
        }
        let sessions = std::mem::take(&mut state.sessions);
        drop(state);
        for (id, mut s) in sessions {
            info!(
                sub_channel = id,
                session_id = %s.session_id,
                %reason,
                "terminating terminal session"
            );
            set_reason(&s.close_reason, reason);
            s.killer.kill().ok();
        }
    }

    fn add(&self, id: u32, handle: SessionHandle) {
        self.lock().sessions.insert(id, handle);
    }

    fn remove(&self, id: u32) {
        self.lock().sessions.remove(&id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        // Sessions never panic while holding this lock; recover
        // rather than poison-cascade (crash-only posture).
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Re-evaluate enablement from the last converged desired state —
/// called after EVERY converge pass (§5.2: the agent evaluates
/// enablement from its last converged desired state, including while
/// offline; Law 5).
///
/// `bundle_root` is the swapped render bundle
/// ([`crate::BundleStore::current_path`]); the config item lives at
/// [`TERMINAL_CONFIG_PATH`] inside it. Absent bundle, absent file,
/// or unparseable file all evaluate to DISABLED (default-deny,
/// §5.2). Transitioning to disabled terminates live sessions.
pub fn sync_enablement(gate: &TerminalGate, bundle_root: &Path) {
    let config = load_config(bundle_root);
    let was = gate.config().enabled;
    if config.enabled != was {
        info!(
            enabled = config.enabled,
            "terminal enablement changed by desired state (spec/reeve/03-terminal.md §5.2)"
        );
    }
    gate.set_config(config.clone());
    if !config.enabled {
        gate.kill_all("terminal disabled in desired state");
    }
}

/// Read [`TERMINAL_CONFIG_PATH`] from the bundle. Anything short of
/// a well-formed `enabled: true` is DISABLED (§5.2 default-deny) —
/// a parse error is warned about, never trusted.
pub fn load_config(bundle_root: &Path) -> TerminalConfig {
    let path = bundle_root.join(TERMINAL_CONFIG_PATH);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return TerminalConfig::default();
    };
    match serde_yaml_ng::from_str::<TerminalConfig>(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "unparseable terminal config; treating as disabled (§5.2 default-deny)"
            );
            TerminalConfig::default()
        }
    }
}

// ---------------------------------------------------------------
// Sub-channel handler (§5.1) — accept or reject an open
// ---------------------------------------------------------------

/// The `rev-002/terminal` [`SubChannelHandler`]: rejects unless the
/// current desired state enables the terminal, otherwise spawns one
/// PTY session bound to the sub-channel's lifetime.
pub struct TerminalHandler {
    gate: Arc<TerminalGate>,
}

impl SubChannelHandler for TerminalHandler {
    fn open(
        &self,
        id: u32,
        meta: Option<serde_json::Value>,
        tx: SubChannelTx,
    ) -> Result<Box<dyn SubChannelConsumer>, String> {
        let config = self.gate.config();
        if !config.enabled {
            // §5.2: refuse — enablement comes only from desired
            // state, and this device's doesn't grant it.
            return Err("terminal not enabled in desired state (spec/reeve/03-terminal.md §5.2)"
                .to_string());
        }
        let meta = meta.ok_or("terminal open requires meta (sessionId, size, TERM; §5.1)")?;
        let meta: TerminalOpenMeta = serde_json::from_value(meta)
            .map_err(|e| format!("malformed terminal open meta: {e}"))?;
        spawn_session(self.gate.clone(), id, meta, &config, tx)
    }
}

/// Set a session's close reason unless one is already recorded (the
/// first cause wins — e.g. "idle timeout" over the generic exit).
fn set_reason(slot: &Mutex<Option<String>>, reason: &str) {
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(reason.to_string());
    }
}

fn take_reason(slot: &Mutex<Option<String>>) -> Option<String> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Open the PTY, spawn the shell, and wire the three moving parts:
/// a blocking reader thread (PTY → sub-channel, and the child
/// reaper), a blocking writer thread (sub-channel → PTY), and an
/// async watchdog for the §5.3 limits. Every path out kills the
/// child and closes the sub-channel — sessions are short-lived and
/// nothing about them is resumable (§5.3, Law 3).
fn spawn_session(
    gate: Arc<TerminalGate>,
    id: u32,
    meta: TerminalOpenMeta,
    config: &TerminalConfig,
    tx: SubChannelTx,
) -> Result<Box<dyn SubChannelConsumer>, String> {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: meta.rows,
            cols: meta.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("cannot open pty: {e}"))?;

    let shell = resolve_shell(config);
    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", meta.term.as_deref().unwrap_or("xterm-256color"));
    let mut child = pty
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("cannot spawn {shell:?}: {e}"))?;
    // The slave fd stays with the child; ours closes now so the
    // master sees EOF when the child exits.
    drop(pty.slave);

    let pid = child.process_id();
    let gate_killer = child.clone_killer();
    let mut watchdog_killer = child.clone_killer();
    let mut closed_killer = child.clone_killer();
    let mut reader = pty
        .master
        .try_clone_reader()
        .map_err(|e| format!("cannot clone pty reader: {e}"))?;
    let mut writer = pty
        .master
        .take_writer()
        .map_err(|e| format!("cannot take pty writer: {e}"))?;

    info!(
        sub_channel = id,
        session_id = %meta.session_id,
        pid,
        %shell,
        cols = meta.cols,
        rows = meta.rows,
        "terminal session opened (spec/reeve/03-terminal.md §5.3)"
    );

    let alive = Arc::new(AtomicBool::new(true));
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let close_reason = Arc::new(Mutex::new(None::<String>));

    gate.add(
        id,
        SessionHandle {
            session_id: meta.session_id.clone(),
            pid,
            killer: gate_killer,
            close_reason: close_reason.clone(),
        },
    );

    // Writer thread: sub-channel data → PTY. Unbounded so the
    // consumer's sync `data()` never blocks the channel task; a
    // stalled child bounds this at the PTY buffer + queued input,
    // and the §5.3 limits bound the session itself.
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Some(bytes) = write_rx.blocking_recv() {
            if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
    });

    // Reader thread: PTY → pump. Owns the child so EOF (however
    // caused: exit, our kill, disable, channel teardown) always
    // reaps it — no zombies (§5.3: the session dies whole).
    let (read_tx, mut read_rx) = mpsc::channel::<Vec<u8>>(32);
    let activity_r = last_activity.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    touch(&activity_r);
                    if read_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        drop(read_tx);
        let status = child.wait();
        debug!(?status, "terminal child reaped");
    });

    // Pump task: forwards PTY output to the sub-channel, then owns
    // session teardown when the PTY side ends.
    let pump_gate = gate.clone();
    let pump_alive = alive.clone();
    let pump_reason = close_reason.clone();
    let pump_tx = tx.clone();
    let pump_session = meta.session_id.clone();
    tokio::spawn(async move {
        while let Some(chunk) = read_rx.recv().await {
            if !pump_tx.send(&encode_terminal_data(&chunk)).await {
                break; // channel gone; teardown will call closed()
            }
        }
        pump_alive.store(false, Ordering::SeqCst);
        pump_gate.remove(id);
        let reason = take_reason(&pump_reason).unwrap_or_else(|| "process exited".to_string());
        info!(sub_channel = id, session_id = %pump_session, %reason, "terminal session ended");
        pump_tx.close(Some(reason)).await;
    });

    // Watchdog: §5.3 both-sides limits — idle timeout and hard cap.
    let idle = Duration::from_secs(config.idle_timeout_secs.max(1));
    let hard_cap = Duration::from_secs(config.hard_cap_secs.max(1));
    let wd_alive = alive.clone();
    let wd_activity = last_activity.clone();
    let wd_reason = close_reason.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        loop {
            tokio::time::sleep(WATCHDOG_TICK).await;
            if !wd_alive.load(Ordering::SeqCst) {
                return;
            }
            let now = Instant::now();
            let reason = if now.duration_since(started) >= hard_cap {
                Some("session hard cap reached (§5.3)")
            } else if now.duration_since(idle_mark(&wd_activity)) >= idle {
                Some("idle timeout (§5.3)")
            } else {
                None
            };
            if let Some(reason) = reason {
                set_reason(&wd_reason, reason);
                watchdog_killer.kill().ok();
                return;
            }
        }
    });

    Ok(Box::new(TerminalConsumer {
        id,
        gate,
        master: pty.master,
        write_tx,
        alive,
        last_activity,
        close_reason,
        killer: move || {
            closed_killer.kill().ok();
        },
    }))
}

fn touch(last_activity: &Mutex<Instant>) {
    *last_activity.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
}

fn idle_mark(last_activity: &Mutex<Instant>) -> Instant {
    *last_activity.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------
// Sub-channel consumer — the receive side of one session
// ---------------------------------------------------------------

/// One live session's receive side, run sync on the channel task
/// (heavy work is offloaded: writes go to the writer thread; only
/// `resize` — an ioctl — runs inline).
struct TerminalConsumer<K: FnMut()> {
    id: u32,
    gate: Arc<TerminalGate>,
    /// Kept for `resize` (in-band control, §5.1); dropping it also
    /// EOFs the reader clone once the child is gone.
    master: Box<dyn MasterPty + Send>,
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    alive: Arc<AtomicBool>,
    last_activity: Arc<Mutex<Instant>>,
    close_reason: Arc<Mutex<Option<String>>>,
    killer: K,
}

impl<K: FnMut() + Send> SubChannelConsumer for TerminalConsumer<K> {
    fn data(&mut self, payload: &[u8]) {
        match decode_terminal_payload(payload) {
            Some(TerminalPayload::Data(bytes)) => {
                touch(&self.last_activity);
                // Writer thread gone = child gone; teardown is
                // already in flight — drop the bytes.
                let _ = self.write_tx.send(bytes.to_vec());
            }
            Some(TerminalPayload::Resize { cols, rows }) => {
                touch(&self.last_activity);
                if let Err(e) = self.master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    warn!(sub_channel = self.id, error = %e, "pty resize failed");
                }
            }
            // Tolerant reader: unknown in-band frames are ignored,
            // never fatal (01-framework §3.4).
            None => debug!(sub_channel = self.id, "undecodable terminal payload ignored"),
        }
    }

    /// Peer `close` or whole-channel teardown (§5.3: any leg failure
    /// closes the whole session): kill the PTY child — the reader
    /// thread then reaps it — and forget the session.
    fn closed(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        set_reason(&self.close_reason, "sub-channel closed");
        (self.killer)();
        self.gate.remove(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext::channel::{Outgoing, test_sub_channel};
    use reeve_types::reeve::channel::decode_data_frame;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn resolve_shell_prefers_explicit_then_bash_then_sh() {
        // Explicit config shell always wins (per-scope override, §11.1).
        let mut cfg = TerminalConfig { shell: Some("/usr/bin/fish".into()), ..Default::default() };
        assert_eq!(resolve_shell(&cfg), "/usr/bin/fish");
        // Blank/whitespace is treated as unset.
        cfg.shell = Some("   ".into());
        let resolved = resolve_shell(&cfg);
        assert!(resolved != "   ");
        // No explicit shell: bash if the device has it, else /bin/sh —
        // and always a real, present interpreter.
        cfg.shell = None;
        let auto = resolve_shell(&cfg);
        let bash_present = PREFERRED_SHELLS.iter().any(|p| std::path::Path::new(p).exists());
        if bash_present {
            assert!(PREFERRED_SHELLS.contains(&auto.as_str()), "should pick bash when present: {auto}");
        } else {
            assert_eq!(auto, DEFAULT_SHELL);
        }
        assert!(std::path::Path::new(&auto).exists(), "resolved shell must exist: {auto}");
    }

    fn enabled_config() -> TerminalConfig {
        TerminalConfig {
            enabled: true,
            ..TerminalConfig::default()
        }
    }

    fn open_meta() -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "sessionId": "sess-test-1", "cols": 80, "rows": 24, "term": "xterm"
        }))
    }

    fn handler(gate: &Arc<TerminalGate>) -> TerminalHandler {
        TerminalHandler { gate: gate.clone() }
    }

    /// Drain sub-channel output until `pred` matches the
    /// accumulated PTY byte stream (or the sub-channel closes / the
    /// timeout hits). Returns (bytes, close_reason).
    async fn collect_output(
        rx: &mut mpsc::Receiver<Outgoing>,
        mut until: impl FnMut(&[u8], &Option<Option<String>>) -> bool,
    ) -> (Vec<u8>, Option<Option<String>>) {
        let mut bytes = Vec::new();
        let mut closed = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !until(&bytes, &closed) {
            let out = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("terminal output within timeout")
                .expect("sub-channel pipe open");
            match out {
                Outgoing::Frame(Message::Binary(frame)) => {
                    let (_, payload) = decode_data_frame(&frame).expect("framed data");
                    match decode_terminal_payload(payload) {
                        Some(TerminalPayload::Data(b)) => bytes.extend_from_slice(b),
                        other => panic!("agent only sends data frames, got {other:?}"),
                    }
                }
                Outgoing::CloseSub { reason, .. } => closed = Some(reason),
                other_frame => {
                    let Outgoing::Frame(m) = other_frame else { unreachable!() };
                    panic!("unexpected non-binary frame {m:?}");
                }
            }
        }
        (bytes, closed)
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    /// Wait for the child to be fully gone — not just killed but
    /// REAPED (a zombie still has /proc/<pid> with state Z).
    async fn assert_no_zombie(pid: u32) {
        let stat = format!("/proc/{pid}/stat");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::read_to_string(&stat) {
                Err(_) => return, // gone entirely
                Ok(s) => {
                    // state is the field after the parenthesized comm
                    let state = s.rsplit(')').next().unwrap_or("").trim().chars().next();
                    assert!(
                        Instant::now() < deadline,
                        "child pid {pid} still present (state {state:?}) — zombie or unkilled"
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // ---- §5.2: disabled by default, enablement gate ---------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn disabled_by_default_rejects_open() {
        // A fresh gate — the freshly-enrolled-device posture — MUST
        // refuse rev-002/terminal opens (§5.2).
        let gate = TerminalGate::new();
        assert!(!gate.config().enabled, "gate must start disabled");
        let (tx, _rx) = test_sub_channel(2);
        let err = handler(&gate)
            .open(2, open_meta(), tx)
            .err()
            .expect("open must be rejected when disabled");
        assert!(err.contains("not enabled"), "reason names the gate: {err}");
        assert_eq!(gate.session_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn enablement_comes_only_from_the_rendered_bundle() {
        let gate = TerminalGate::new();
        let bundle = tempfile::tempdir().unwrap();

        // No config file in the bundle => disabled (§5.2).
        sync_enablement(&gate, bundle.path());
        assert!(!gate.config().enabled);

        // Unparseable config => disabled (default-deny), not enabled.
        let cfg_path = bundle.path().join(TERMINAL_CONFIG_PATH);
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(&cfg_path, "enabled: [not, a, bool]\n").unwrap();
        sync_enablement(&gate, bundle.path());
        assert!(!gate.config().enabled, "parse error must not enable");

        // A rendered `enabled: true` => enabled; opens accepted.
        std::fs::write(&cfg_path, "enabled: true\n").unwrap();
        sync_enablement(&gate, bundle.path());
        assert!(gate.config().enabled);
        let (tx, mut rx) = test_sub_channel(2);
        let mut consumer = handler(&gate)
            .open(2, open_meta(), tx)
            .expect("enabled gate accepts");
        assert_eq!(gate.session_count(), 1);
        let pid = gate.session_pid(2).expect("platform reports pid");

        // Disablement is the same mechanism in reverse; converging
        // to a disabling commit terminates the live session (§5.2).
        std::fs::write(&cfg_path, "enabled: false\n").unwrap();
        sync_enablement(&gate, bundle.path());
        assert!(!gate.config().enabled);
        assert_eq!(gate.session_count(), 0, "disable kills live sessions");
        let (_, closed) = collect_output(&mut rx, |_, closed| closed.is_some()).await;
        let reason = closed.flatten().expect("close carries a reason");
        assert!(reason.contains("disabled"), "reason says why: {reason}");
        assert_no_zombie(pid).await;
        consumer.closed(); // idempotent late teardown from the channel task
    }

    // ---- §5.1: meta validation ------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn open_without_usable_meta_is_rejected() {
        let gate = TerminalGate::new();
        gate.set_config(enabled_config());
        let h = handler(&gate);
        let (tx, _rx) = test_sub_channel(2);
        assert!(h.open(2, None, tx).is_err(), "meta is required (§5.1)");
        let (tx, _rx) = test_sub_channel(4);
        assert!(
            h.open(4, Some(serde_json::json!({"cols": 80})), tx).is_err(),
            "sessionId is required (§5.1)"
        );
        assert_eq!(gate.session_count(), 0);
    }

    // ---- §5.1/§5.3: a real PTY round trip --------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn pty_echo_round_trip_through_the_sub_channel() {
        let gate = TerminalGate::new();
        gate.set_config(enabled_config());
        let (tx, mut rx) = test_sub_channel(2);
        let mut consumer = handler(&gate)
            .open(2, open_meta(), tx)
            .expect("enabled gate accepts");

        // In-band input (UI keystrokes): run a command through the
        // shell and watch its output ride back as data frames.
        consumer.data(&encode_terminal_data(b"echo reeve-terminal-rtt\n"));
        let (bytes, _) =
            collect_output(&mut rx, |b, _| contains(b, "reeve-terminal-rtt")).await;
        assert!(contains(&bytes, "reeve-terminal-rtt"));

        // In-band resize is applied, not fatal (§5.1); prove the
        // session is still interactive afterwards.
        consumer.data(&reeve_types::reeve::terminal::encode_terminal_resize(120, 40));
        consumer.data(&[7, 7, 7]); // unknown in-band frame: ignored
        consumer.data(&encode_terminal_data(b"echo still-alive-$((6*7))\n"));
        let (bytes, _) = collect_output(&mut rx, |b, _| contains(b, "still-alive-42")).await;
        assert!(contains(&bytes, "still-alive-42"));

        // Child exit ends the session: sub-channel closes, session
        // forgotten (§5.3 — short-lived, nothing standing).
        let pid = gate.session_pid(2).expect("pid");
        consumer.data(&encode_terminal_data(b"exit\n"));
        let (_, closed) = collect_output(&mut rx, |_, closed| closed.is_some()).await;
        assert!(closed.is_some(), "process exit closes the sub-channel");
        assert_no_zombie(pid).await;
        assert_eq!(gate.session_count(), 0);
    }

    // ---- §5.3: close kills the child, no zombies -------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn close_kills_the_pty_child_and_reaps_it() {
        let gate = TerminalGate::new();
        gate.set_config(enabled_config());
        let (tx, mut rx) = test_sub_channel(2);
        let mut consumer = handler(&gate)
            .open(2, open_meta(), tx)
            .expect("enabled gate accepts");
        // Wait for the shell to be truly up (prompt output).
        consumer.data(&encode_terminal_data(b"echo ready-marker\n"));
        collect_output(&mut rx, |b, _| contains(b, "ready-marker")).await;
        let pid = gate.session_pid(2).expect("pid");

        // Peer close / channel teardown path (§5.3: any leg failure
        // closes the whole session).
        consumer.closed();
        assert_eq!(gate.session_count(), 0, "session forgotten at close");
        assert_no_zombie(pid).await;
    }

    // ---- §5.3: limits — idle timeout kills the session --------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn idle_timeout_ends_the_session() {
        let gate = TerminalGate::new();
        gate.set_config(TerminalConfig {
            enabled: true,
            idle_timeout_secs: 1,
            ..TerminalConfig::default()
        });
        let (tx, mut rx) = test_sub_channel(2);
        let _consumer = handler(&gate)
            .open(2, open_meta(), tx)
            .expect("enabled gate accepts");
        let pid = gate.session_pid(2).expect("pid");
        // No input at all: the watchdog must end it (§5.3 — both
        // sides enforce limits; sessions are short-lived).
        let (_, closed) = collect_output(&mut rx, |_, closed| closed.is_some()).await;
        let reason = closed.flatten().expect("close carries a reason");
        assert!(reason.contains("idle"), "reason names the limit: {reason}");
        assert_no_zombie(pid).await;
        assert_eq!(gate.session_count(), 0);
    }

    // ---- config parsing ----------------------------------------------------

    #[test]
    fn load_config_defaults_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        // Absent file: full default-deny posture.
        assert_eq!(load_config(dir.path()), TerminalConfig::default());
        let path = dir.path().join(TERMINAL_CONFIG_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "enabled: true\nshell: /bin/bash\nidleTimeoutSecs: 60\nhardCapSecs: 600\n",
        )
        .unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(
            cfg,
            TerminalConfig {
                enabled: true,
                shell: Some("/bin/bash".into()),
                idle_timeout_secs: 60,
                hard_cap_secs: 600,
            }
        );
    }
}
