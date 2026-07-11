//! Self-update — the agent as a workload (build item B8; core,
//! unconditional).
//!
//! Normative source: spec/reeve/08-packaging.md §10.5:
//! - agent updates flow through the normal desired-state tree; the
//!   repo ships a Margo-shaped example package
//!   (`deploy/packages/reeve-agent/`) and NO side-band updater
//!   exists;
//! - update mechanics are A/B on the binary path: install the new
//!   binary beside the old (versioned filename), atomically swap a
//!   symlink, restart the unit ("rename-and-exec or service
//!   restart");
//! - a failed self-update MUST leave the previous binary running:
//!   the swap is the last step, and a new binary failing its first
//!   health window is rolled back to the retained previous binary by
//!   the supervising unit's failure handling ([`crate::systemd`]:
//!   `StartLimit*` + `OnFailure=` -> the rollback unit executes
//!   `<lib>/previous rollback`);
//! - `kill -9` at any point leaves either old-running or new-running
//!   — never neither (Law 3). Every step below is atomic
//!   (temp+fsync+rename) and idempotent, and the `current` symlink
//!   always names a complete binary.
//!
//! Recognition (our call, recorded in DECISIONS-MADE.md — the spec
//! fixes the mechanics, not the marker): an app dir in the render
//! bundle whose root (or `files/`) holds [`AGENT_UPDATE_FILE`] is an
//! agent-update app. It bypasses the workload [`crate::provider`]
//! entirely and is applied by [`AgentUpdater`] through the same D5
//! phase machine (planned -> applying -> applied | failed) in
//! [`crate::converge`].
//!
//! Binary delivery: the referenced binary is the OCI artifact of
//! §10.4 served on the /v2 routes; [`BinaryFetcher`] pulls the blob
//! by digest with the device credential (or reads it from a `dir://`
//! source — the air-gap media path). Fetching happens OUTSIDE
//! converge ([`prefetch`], called from the poll loop next to the
//! bundle pull) so converge keeps consuming only local state (Law 5).

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use reeve_types::margo::status::DeploymentState;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::bundle::BundleStore;
use crate::state::{AgentDb, Severity};

/// Marker + descriptor file inside a render-bundle app dir that
/// makes it an agent-update app (checked at the app-dir root, then
/// under `files/` — the path tree-authored config files render to,
/// docs/decisions/tree-render.md D2).
pub const AGENT_UPDATE_FILE: &str = "agent-update.yaml";
/// Symlink naming the binary the unit executes
/// (`ExecStart=<lib>/current`, [`crate::systemd`]).
pub const CURRENT_LINK: &str = "current";
/// Symlink naming the RETAINED previous binary — what §10.5 rollback
/// restores, and the binary the rollback unit executes (a broken new
/// binary cannot prevent its own rollback).
pub const PREVIOUS_LINK: &str = "previous";
/// Marker file recording a version rolled back by the health gate:
/// the agent refuses to re-apply that exact version until desired
/// state names a different one (bounds the update/rollback flap; the
/// Failed status it reports is what a rollout health gate observes,
/// spec/reeve/09-rollouts.md Section 11).
pub const HELD_MARKER: &str = "held";
/// Versioned binary file name prefix: `reeve-agent-<version>`
/// (§10.5 "installed beside old (versioned filename)").
pub const BINARY_PREFIX: &str = "reeve-agent-";

/// The parsed [`AGENT_UPDATE_FILE`]. Reeve-authored shape (wire-
/// adjacent, camelCase like every reeve extension surface):
///
/// ```yaml
/// version: "0.2.0"
/// binary:
///   url: /v2/reeve/agent/x86_64/blobs/sha256:<hex>
///   digest: sha256:<hex>
///   sizeBytes: 12345678   # advisory; digest is the integrity check
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUpdateSpec {
    /// Target agent version. Compared against the RUNNING agent's
    /// version — equality is the update's postcondition (§10.5: the
    /// agent reports its version in status; that is how staged
    /// rollout health gates observe success).
    pub version: String,
    pub binary: BinaryRef,
}

/// Where the new binary lives and what it must hash to.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryRef {
    /// Blob URL: absolute `http(s)://`, server-relative (`/v2/...`,
    /// joined to the configured server origin — the §10.4 artifact
    /// route), or a path inside a `dir://` source (air-gap media).
    pub url: String,
    /// `sha256:<hex>` — the sole integrity check (sizeBytes is
    /// advisory, mirroring `BundleRef`).
    pub digest: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Version strings become file names (`reeve-agent-<version>`), so
/// constrain them to the same safe grammar as secret names.
fn is_safe_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 100
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// Read the agent-update descriptor from an app dir, if present.
/// `None` => a normal workload app; `Some(Err)` => the marker exists
/// but is unusable (the app must FAIL, not fall through to the
/// compose provider).
pub fn update_spec(app_dir: &Path) -> Option<Result<AgentUpdateSpec, String>> {
    let path = [
        app_dir.join(AGENT_UPDATE_FILE),
        app_dir.join("files").join(AGENT_UPDATE_FILE),
    ]
    .into_iter()
    .find(|p| p.is_file())?;
    let parse = || -> Result<AgentUpdateSpec, String> {
        let text = fs::read_to_string(&path).map_err(|e| format!("cannot read {AGENT_UPDATE_FILE}: {e}"))?;
        let spec: AgentUpdateSpec =
            serde_yaml_ng::from_str(&text).map_err(|e| format!("unparseable {AGENT_UPDATE_FILE}: {e}"))?;
        if !is_safe_version(&spec.version) {
            return Err(format!("unsafe version string {:?}", spec.version));
        }
        if !reeve_types::reeve::manifest::is_sha256_digest(&spec.binary.digest) {
            return Err(format!(
                "binary digest {:?} violates sha256:<hex> grammar",
                spec.binary.digest
            ));
        }
        Ok(spec)
    };
    Some(parse())
}

/// `sha256:<hex>` of a byte slice (same grammar as every digest in
/// the system).
fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// The A/B binary directory (`/usr/local/lib/reeve-agent` by
/// default, `AgentConfig::install_dir`):
///
/// ```text
/// <dir>/
///   reeve-agent-<vA>    # versioned binaries, immutable once staged
///   reeve-agent-<vB>
///   current -> reeve-agent-<vB>    # what the unit executes
///   previous -> reeve-agent-<vA>   # §10.5 retained binary
///   held                # version refused after a health-gate rollback
/// ```
///
/// Symlink targets are RELATIVE (bare file names) so a rooted test
/// layout behaves identically to `/`. Every mutation is
/// temp+fsync+rename (Law 3).
#[derive(Debug, Clone)]
pub struct BinDir {
    dir: PathBuf,
}

impl BinDir {
    pub fn new(dir: &Path) -> Self {
        BinDir { dir: dir.to_path_buf() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `reeve-agent-<version>` (§10.5 versioned filename).
    pub fn versioned_name(version: &str) -> String {
        format!("{BINARY_PREFIX}{version}")
    }

    pub fn versioned_path(&self, version: &str) -> PathBuf {
        self.dir.join(Self::versioned_name(version))
    }

    fn fsync_dir(&self) -> std::io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }

    /// True iff the versioned binary is present AND hashes to
    /// `digest` (a half-written file can never pass — staging is
    /// atomic, but external interference must not either).
    pub fn staged_ok(&self, version: &str, digest: &str) -> bool {
        let path = self.versioned_path(version);
        match fs::read(&path) {
            Ok(bytes) => digest_bytes(&bytes) == digest,
            Err(_) => false,
        }
    }

    /// Stage a verified binary beside the running one: verify
    /// `bytes` hash to `digest`, write to a temp name, fsync, chmod
    /// 0755, rename into place, fsync the dir. Idempotent: an
    /// already-staged matching binary is a no-op.
    pub fn stage(&self, version: &str, bytes: &[u8], digest: &str) -> Result<PathBuf, String> {
        let actual = digest_bytes(bytes);
        if actual != digest {
            return Err(format!("binary digest mismatch: expected {digest}, got {actual}"));
        }
        let final_path = self.versioned_path(version);
        if self.staged_ok(version, digest) {
            return Ok(final_path);
        }
        fs::create_dir_all(&self.dir).map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let tmp = self.dir.join(format!(".stage-{}.tmp", Self::versioned_name(version)));
        let write = || -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
            }
            fs::rename(&tmp, &final_path)?;
            self.fsync_dir()
        };
        write().map_err(|e| format!("cannot stage {}: {e}", final_path.display()))?;
        Ok(final_path)
    }

    fn link_target(&self, link: &str) -> Option<String> {
        fs::read_link(self.dir.join(link))
            .ok()
            .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned()))
    }

    /// File name the `current` symlink points at, if any.
    pub fn current_target(&self) -> Option<String> {
        self.link_target(CURRENT_LINK)
    }

    /// File name the `previous` symlink points at, if any.
    pub fn previous_target(&self) -> Option<String> {
        self.link_target(PREVIOUS_LINK)
    }

    /// Atomically (re)point `link` at `target_name`: symlink at a
    /// temp name, rename over the link, fsync the dir. `rename(2)`
    /// replaces the destination atomically — a reader (or `execve`)
    /// sees the old target or the new, never a missing link.
    fn atomic_symlink(&self, target_name: &str, link: &str) -> std::io::Result<()> {
        let tmp = self.dir.join(format!(".{link}.tmp"));
        let _ = fs::remove_file(&tmp); // stale temp from a crash
        #[cfg(unix)]
        std::os::unix::fs::symlink(target_name, &tmp)?;
        #[cfg(not(unix))]
        return Err(std::io::Error::other("A/B symlink swap requires unix"));
        #[cfg(unix)]
        {
            fs::rename(&tmp, self.dir.join(link))?;
            self.fsync_dir()
        }
    }

    /// The §10.5 A/B swap. Preconditions: the versioned binary is
    /// staged. Steps, each atomic, in this order (the crash-safety
    /// argument — kill -9 between ANY two lines leaves `current`
    /// naming a complete binary):
    /// 1. `previous` -> old `current` target (retain the running
    ///    binary; §10.5 rollback depends on it);
    /// 2. `current` -> `reeve-agent-<version>` (THE swap — last
    ///    step, per §10.5 "the swap is the last step").
    ///
    /// Idempotent: if `current` already names the target the whole
    /// call is a no-op — critically, `previous` is NOT re-flipped
    /// (re-running after a crash must never make previous == new and
    /// lose the retained old binary).
    pub fn swap_to(&self, version: &str) -> Result<(), String> {
        let new_name = Self::versioned_name(version);
        if !self.versioned_path(version).is_file() {
            return Err(format!("binary {new_name} is not staged"));
        }
        if self.current_target().as_deref() == Some(new_name.as_str()) {
            return Ok(()); // already swapped (crash between swap and restart)
        }
        if let Some(old) = self.current_target() {
            self.atomic_symlink(&old, PREVIOUS_LINK)
                .map_err(|e| format!("cannot retain previous binary: {e}"))?;
        }
        self.atomic_symlink(&new_name, CURRENT_LINK)
            .map_err(|e| format!("cannot swap current symlink: {e}"))?;
        Ok(())
    }

    /// Roll back to the retained previous binary (§10.5: "a new
    /// binary failing its first health window is rolled back to the
    /// retained previous binary"). Writes the [`HELD_MARKER`] naming
    /// the abandoned version BEFORE flipping (a crash between the
    /// two re-runs rollback; holding a version we still run is
    /// corrected by the running-version check in
    /// [`AgentUpdater::apply`]). Idempotent: already-rolled-back is
    /// a no-op. Returns the restored file name.
    pub fn rollback(&self) -> Result<String, String> {
        let prev = self
            .previous_target()
            .ok_or_else(|| "nothing to roll back to (no previous binary retained)".to_string())?;
        if !self.dir.join(&prev).is_file() {
            return Err(format!("retained binary {prev} is missing"));
        }
        let bad = self.current_target();
        if bad.as_deref() == Some(prev.as_str()) {
            return Ok(prev); // already rolled back
        }
        if let Some(bad_version) = bad.as_deref().and_then(|b| b.strip_prefix(BINARY_PREFIX)) {
            self.write_hold(bad_version)
                .map_err(|e| format!("cannot write hold marker: {e}"))?;
        }
        self.atomic_symlink(&prev, CURRENT_LINK)
            .map_err(|e| format!("cannot roll back current symlink: {e}"))?;
        Ok(prev)
    }

    fn write_hold(&self, version: &str) -> std::io::Result<()> {
        let tmp = self.dir.join(format!(".{HELD_MARKER}.tmp"));
        let mut f = File::create(&tmp)?;
        f.write_all(version.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, self.dir.join(HELD_MARKER))?;
        self.fsync_dir()
    }

    /// Version refused after a health-gate rollback, if any.
    pub fn held_version(&self) -> Option<String> {
        fs::read_to_string(self.dir.join(HELD_MARKER))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Clear the hold (desired state moved to a different version).
    pub fn clear_hold(&self) -> std::io::Result<()> {
        match fs::remove_file(self.dir.join(HELD_MARKER)) {
            Ok(()) => self.fsync_dir(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Delete versioned binaries referenced by neither `current` nor
    /// `previous` (A/B retains exactly one previous; older ones are
    /// garbage). Safe at any time — it never touches a referenced
    /// file. Returns the removed names.
    pub fn gc(&self) -> std::io::Result<Vec<String>> {
        let keep: Vec<String> = [self.current_target(), self.previous_target()]
            .into_iter()
            .flatten()
            .collect();
        let mut removed = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(BINARY_PREFIX) && !keep.contains(&name) && entry.path().is_file() {
                fs::remove_file(entry.path())?;
                removed.push(name);
            }
        }
        if !removed.is_empty() {
            self.fsync_dir()?;
        }
        Ok(removed)
    }
}

/// How the updater asks the supervisor to run the new binary. §10.5
/// allows "rename-and-exec or service restart"; implementations:
/// - [`ExitRestarter`] (production daemon): exit cleanly and let the
///   unit's `Restart=always` re-exec through the flipped `current`
///   symlink — no privileges needed (the agent user cannot call
///   systemctl), no shutdown ceremony (Law 3: the exit IS the
///   restart request; all state is already durable).
/// - [`SystemctlRestarter`] (root-run `rollback` subcommand):
///   `systemctl restart --no-block reeve-agent.service`.
pub trait UnitRestarter: Send + Sync {
    fn restart(&self) -> std::io::Result<()>;
}

/// Exit the process; the systemd unit restarts it through the
/// `current` symlink (see [`UnitRestarter`]).
pub struct ExitRestarter;

impl UnitRestarter for ExitRestarter {
    fn restart(&self) -> std::io::Result<()> {
        info!("agent update swapped; exiting for the supervisor to re-exec the new binary (spec/reeve/08-packaging.md §10.5)");
        std::process::exit(0);
    }
}

/// `systemctl restart --no-block <unit>` — used by the root-run
/// rollback path where systemd IS reachable.
pub struct SystemctlRestarter {
    pub unit: String,
}

impl UnitRestarter for SystemctlRestarter {
    fn restart(&self) -> std::io::Result<()> {
        let out = std::process::Command::new("systemctl")
            .args(["restart", "--no-block", &self.unit])
            .output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "systemctl restart {} failed ({}): {}",
                self.unit,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

/// Outcome of one agent-update apply attempt, mapped by converge to
/// the D5 phase machine and the Margo status vocabulary.
#[derive(Debug)]
pub struct UpdateOutcome {
    /// `Installed` => postcondition holds (running the target
    /// version) — converge records the terminal `applied` phase.
    /// `Installing` => in flight (binary not fetched yet, or swap
    /// done and restart pending) — phase stays `applying`,
    /// re-checked next pass. `Failed` => terminal for this content
    /// (held after rollback, unusable descriptor).
    pub state: DeploymentState,
    pub error: Option<String>,
    /// Journal-only progress note.
    pub detail: Option<String>,
}

impl UpdateOutcome {
    fn installed(detail: impl Into<String>) -> Self {
        UpdateOutcome {
            state: DeploymentState::Installed,
            error: None,
            detail: Some(detail.into()),
        }
    }
    fn installing(detail: impl Into<String>) -> Self {
        UpdateOutcome {
            state: DeploymentState::Installing,
            error: None,
            detail: Some(detail.into()),
        }
    }
    fn failed(msg: impl Into<String>) -> Self {
        UpdateOutcome {
            state: DeploymentState::Failed,
            error: Some(msg.into()),
            detail: None,
        }
    }
}

/// The agent-update apply path (provider-adjacent; converge routes
/// marked apps here instead of the workload [`crate::provider`]).
pub struct AgentUpdater {
    bin: BinDir,
    restarter: Box<dyn UnitRestarter>,
    running_version: String,
}

impl AgentUpdater {
    pub fn new(bin: BinDir, restarter: Box<dyn UnitRestarter>, running_version: &str) -> Self {
        AgentUpdater {
            bin,
            restarter,
            running_version: running_version.to_string(),
        }
    }

    pub fn bin_dir(&self) -> &BinDir {
        &self.bin
    }

    /// One idempotent convergence step toward "running
    /// `spec.version`". Consumes only local state (Law 5 — the
    /// binary was prefetched by [`prefetch`]); every early return is
    /// re-checked on the next converge pass.
    pub fn apply(&self, spec: &AgentUpdateSpec) -> UpdateOutcome {
        // Postcondition first: already running the target.
        if self.running_version == spec.version {
            if let Err(e) = self.bin.gc() {
                warn!(error = %e, "agent-update gc failed (non-fatal)");
            }
            return UpdateOutcome::installed(format!("running agent version {}", spec.version));
        }

        // Health-gate hold: this exact version was rolled back;
        // refuse until desired state names a different one (§10.5 —
        // the Failed status is what rollout gates observe).
        match self.bin.held_version() {
            Some(held) if held == spec.version => {
                return UpdateOutcome::failed(format!(
                    "agent update to {held} was rolled back by the unit health gate; \
                     holding version {} until desired state changes",
                    self.running_version
                ));
            }
            Some(_) => {
                if let Err(e) = self.bin.clear_hold() {
                    return UpdateOutcome::failed(format!("cannot clear stale update hold: {e}"));
                }
            }
            None => {}
        }

        // Swap already done, restart still pending (crash between
        // swap and restart, or a restart request that went nowhere).
        if self.bin.current_target().as_deref() == Some(BinDir::versioned_name(&spec.version).as_str()) {
            return match self.restarter.restart() {
                Ok(()) => UpdateOutcome::installing("swap done; restart requested"),
                Err(e) => UpdateOutcome::installing(format!("swap done; restart request failed ({e}); will retry")),
            };
        }

        // Binary not local yet: prefetch (poll loop) will land it.
        if !self.bin.staged_ok(&spec.version, &spec.binary.digest) {
            return UpdateOutcome::installing(format!(
                "agent binary {} not fetched yet (digest {})",
                spec.version, spec.binary.digest
            ));
        }

        // THE swap (§10.5: last step), then hand control to the
        // supervisor. `ExitRestarter` does not return.
        if let Err(e) = self.bin.swap_to(&spec.version) {
            return UpdateOutcome::failed(format!("A/B swap failed: {e}"));
        }
        info!(version = %spec.version, "agent binary swapped; requesting restart");
        match self.restarter.restart() {
            Ok(()) => UpdateOutcome::installing("swapped; restart requested"),
            Err(e) => UpdateOutcome::installing(format!("swapped; restart request failed ({e}); will retry")),
        }
    }
}

/// Fetches update binaries by URL + digest with the device
/// credential — the same source split as [`crate::bundle`]: HTTP(S)
/// against the server origin, or files inside a `dir://` source
/// (air-gap media, Milestone 1 harness).
pub enum BinaryFetcher {
    Http {
        origin: String,
        device_token: Option<String>,
        client: reqwest::Client,
    },
    Dir {
        base: PathBuf,
    },
}

impl BinaryFetcher {
    /// Build from the agent's `server` config value (mirrors
    /// `BundleSource::parse`). `None` for unparseable values.
    pub fn from_config(server: &str, device_token: Option<String>) -> Option<Self> {
        if let Some(path) = server.strip_prefix("dir://") {
            return Some(BinaryFetcher::Dir {
                base: PathBuf::from(path),
            });
        }
        if server.starts_with("https://") || server.starts_with("http://") {
            return Some(BinaryFetcher::Http {
                origin: server.trim_end_matches('/').to_string(),
                device_token,
                client: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(120))
                    .build()
                    .ok()?,
            });
        }
        None
    }

    /// Fetch raw blob bytes; the CALLER verifies the digest before
    /// staging ([`BinDir::stage`] re-verifies — defense in depth).
    pub async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        match self {
            BinaryFetcher::Http {
                origin,
                device_token,
                client,
            } => {
                let abs = if url.starts_with("https://") || url.starts_with("http://") {
                    url.to_string()
                } else {
                    format!("{origin}/{}", url.trim_start_matches('/'))
                };
                let mut req = client.get(&abs);
                if let Some(t) = device_token {
                    req = req.bearer_auth(t);
                }
                let resp = req.send().await.map_err(|e| format!("unreachable: {e}"))?;
                if resp.status().as_u16() != 200 {
                    return Err(format!("unexpected status {} from {abs}", resp.status().as_u16()));
                }
                Ok(resp.bytes().await.map_err(|e| format!("unreachable: {e}"))?.to_vec())
            }
            BinaryFetcher::Dir { base } => {
                let p = url.strip_prefix("dir://").unwrap_or(url);
                let path = Path::new(p);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    base.join(path)
                };
                fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))
            }
        }
    }
}

/// Scan the swapped-in bundle for agent-update apps and stage any
/// binary not yet local. Runs in the poll loop beside the bundle
/// pull; converge itself never touches the network (Law 5). Every
/// failure is a journaled continue (offline => the update simply
/// stays `Installing` until the source is reachable).
pub async fn prefetch(
    db: &mut AgentDb,
    store: &BundleStore,
    bin: &BinDir,
    fetcher: Option<&BinaryFetcher>,
    running_version: &str,
) {
    if store.current_digest().is_none() {
        return;
    }
    let apps_root = store.current_path().join(crate::converge::APPS_DIR);
    let Ok(entries) = fs::read_dir(&apps_root) else { return };
    for entry in entries.flatten() {
        let app_dir = entry.path();
        if !app_dir.is_dir() {
            continue;
        }
        let Some(parsed) = update_spec(&app_dir) else { continue };
        let spec = match parsed {
            Ok(s) => s,
            Err(_) => continue, // converge reports the parse failure
        };
        if spec.version == running_version
            || bin.staged_ok(&spec.version, &spec.binary.digest)
            || bin.held_version().as_deref() == Some(spec.version.as_str())
        {
            continue;
        }
        let Some(fetcher) = fetcher else {
            let _ = db.journal(
                Severity::Error,
                "agent-update-no-fetcher",
                &format!("cannot fetch agent binary {}: no usable source", spec.version),
            );
            continue;
        };
        match fetcher.fetch(&spec.binary.url).await {
            Ok(bytes) => {
                let actual = digest_bytes(&bytes);
                if actual != spec.binary.digest {
                    let msg = format!(
                        "agent binary {}: digest mismatch (expected {}, got {actual})",
                        spec.version, spec.binary.digest
                    );
                    warn!("{msg}");
                    let _ = db.journal(Severity::Security, "agent-update-bad-digest", &msg);
                    continue;
                }
                match bin.stage(&spec.version, &bytes, &spec.binary.digest) {
                    Ok(path) => {
                        info!(version = %spec.version, path = %path.display(), "agent update binary staged");
                        let _ = db.journal(
                            Severity::Notable,
                            "agent-update-staged",
                            &format!("{} ({} bytes)", spec.version, bytes.len()),
                        );
                    }
                    Err(e) => {
                        let _ = db.journal(Severity::Error, "agent-update-stage-failed", &e);
                    }
                }
            }
            Err(e) => {
                // Offline is the normal case (Law 5): log and retry
                // next cycle.
                info!(version = %spec.version, error = %e, "agent update binary fetch failed; will retry");
                let _ = db.journal(
                    Severity::Info,
                    "agent-update-fetch-failed",
                    &format!("{}: {e}", spec.version),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeRestarter {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    impl FakeRestarter {
        fn new() -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                FakeRestarter {
                    calls: calls.clone(),
                    fail: false,
                },
                calls,
            )
        }
    }

    impl UnitRestarter for FakeRestarter {
        fn restart(&self) -> std::io::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(std::io::Error::other("no systemd here"));
            }
            Ok(())
        }
    }

    fn bin_dir() -> (tempfile::TempDir, BinDir) {
        let t = tempfile::tempdir().unwrap();
        let b = BinDir::new(&t.path().join("lib"));
        (t, b)
    }

    fn stage(b: &BinDir, version: &str, content: &[u8]) -> String {
        let digest = digest_bytes(content);
        b.stage(version, content, &digest).unwrap();
        digest
    }

    fn spec(version: &str, digest: &str) -> AgentUpdateSpec {
        AgentUpdateSpec {
            version: version.into(),
            binary: BinaryRef {
                url: format!("/v2/reeve/agent/blobs/{digest}"),
                digest: digest.into(),
                size_bytes: None,
            },
        }
    }

    /// Assert the crash-safety invariant: `current`, when present,
    /// resolves to an existing regular file (old-running or
    /// new-running, never neither — §10.5 / Law 3).
    fn assert_current_is_runnable(b: &BinDir) {
        if let Some(target) = b.current_target() {
            assert!(
                b.dir().join(&target).is_file(),
                "current -> {target} must be a complete binary"
            );
        }
    }

    #[test]
    fn stage_verifies_digest_and_is_idempotent() {
        let (_t, b) = bin_dir();
        let digest = digest_bytes(b"binary-v2");
        assert!(b.stage("0.2.0", b"binary-v2", "sha256:wrong").is_err());
        assert!(!b.staged_ok("0.2.0", &digest));
        b.stage("0.2.0", b"binary-v2", &digest).unwrap();
        assert!(b.staged_ok("0.2.0", &digest));
        // idempotent re-stage
        b.stage("0.2.0", b"binary-v2", &digest).unwrap();
        assert!(b.staged_ok("0.2.0", &digest));
        // no temp litter
        let names: Vec<_> = fs::read_dir(b.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["reeve-agent-0.2.0"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(b.versioned_path("0.2.0")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "staged binary must be executable");
        }
    }

    /// The B8 crash table: interrupt the A/B sequence after every
    /// step; at each cut point `current` names a complete binary and
    /// a re-run converges (§10.5: old-running or new-running, never
    /// neither).
    #[test]
    fn ab_swap_crash_table() {
        let (_t, b) = bin_dir();
        stage(&b, "0.1.0", b"old");
        b.swap_to("0.1.0").unwrap(); // install-time first swap
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_eq!(b.previous_target(), None, "first swap retains nothing");
        assert_current_is_runnable(&b);

        // -- cut 1: new binary staged, nothing flipped => old runs.
        stage(&b, "0.2.0", b"new");
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_current_is_runnable(&b);

        // -- cut 2: previous flipped, current not yet => old runs,
        //    and re-running the swap converges without damage.
        b.atomic_symlink("reeve-agent-0.1.0", PREVIOUS_LINK).unwrap();
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_current_is_runnable(&b);
        b.swap_to("0.2.0").unwrap(); // recovery re-run completes the swap

        // -- cut 3: fully swapped, restart pending => new binary
        //    named; previous retains old.
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.2.0"));
        assert_eq!(b.previous_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_current_is_runnable(&b);

        // -- re-running the completed swap is a no-op and MUST NOT
        //    re-flip previous (that would lose the retained binary).
        b.swap_to("0.2.0").unwrap();
        assert_eq!(b.previous_target().as_deref(), Some("reeve-agent-0.1.0"));

        // -- rollback (health gate): current back to old, hold set.
        b.rollback().unwrap();
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_eq!(b.held_version().as_deref(), Some("0.2.0"));
        assert_current_is_runnable(&b);
        // idempotent rollback
        b.rollback().unwrap();
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.1.0"));
    }

    #[test]
    fn swap_refuses_unstaged_binary() {
        let (_t, b) = bin_dir();
        assert!(b.swap_to("9.9.9").is_err());
        assert_eq!(b.current_target(), None);
    }

    #[test]
    fn rollback_without_previous_is_an_error() {
        let (_t, b) = bin_dir();
        stage(&b, "0.1.0", b"old");
        b.swap_to("0.1.0").unwrap();
        assert!(b.rollback().is_err(), "no previous binary retained yet");
    }

    #[test]
    fn gc_removes_only_unreferenced_binaries() {
        let (_t, b) = bin_dir();
        stage(&b, "0.1.0", b"a");
        stage(&b, "0.2.0", b"b");
        stage(&b, "0.3.0", b"c");
        b.swap_to("0.2.0").unwrap();
        b.swap_to("0.3.0").unwrap(); // previous = 0.2.0
        let mut removed = b.gc().unwrap();
        removed.sort();
        assert_eq!(removed, vec!["reeve-agent-0.1.0"]);
        assert!(b.versioned_path("0.2.0").is_file(), "previous retained");
        assert!(b.versioned_path("0.3.0").is_file(), "current retained");
    }

    #[test]
    fn updater_full_lifecycle_table() {
        let (_t, b) = bin_dir();
        stage(&b, "0.1.0", b"old");
        b.swap_to("0.1.0").unwrap();
        let digest = digest_bytes(b"new");
        let s = spec("0.2.0", &digest);

        // 1. binary not fetched => Installing, nothing flipped.
        let (r, calls) = FakeRestarter::new();
        let up = AgentUpdater::new(b.clone(), Box::new(r), "0.1.0");
        let out = up.apply(&s);
        assert_eq!(out.state, DeploymentState::Installing);
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // 2. binary staged => swap + restart requested; still
        //    Installing (postcondition = running the new version).
        stage(&b, "0.2.0", b"new");
        let out = up.apply(&s);
        assert_eq!(out.state, DeploymentState::Installing);
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.2.0"));
        assert_eq!(b.previous_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 3. still running old (restart lost) => only re-request
        //    restart; no re-swap, previous untouched.
        let out = up.apply(&s);
        assert_eq!(out.state, DeploymentState::Installing);
        assert_eq!(b.previous_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // 4. "after restart": running version == target => Installed.
        let (r2, calls2) = FakeRestarter::new();
        let up_new = AgentUpdater::new(b.clone(), Box::new(r2), "0.2.0");
        let out = up_new.apply(&s);
        assert_eq!(out.state, DeploymentState::Installed);
        assert_eq!(calls2.load(Ordering::SeqCst), 0, "no restart when converged");
    }

    #[test]
    fn held_version_is_refused_until_desired_state_changes() {
        let (_t, b) = bin_dir();
        stage(&b, "0.1.0", b"old");
        b.swap_to("0.1.0").unwrap();
        stage(&b, "0.2.0", b"new");
        b.swap_to("0.2.0").unwrap();
        b.rollback().unwrap(); // health gate rolled 0.2.0 back

        let digest2 = digest_bytes(b"new");
        let (r, calls) = FakeRestarter::new();
        let up = AgentUpdater::new(b.clone(), Box::new(r), "0.1.0");
        let out = up.apply(&spec("0.2.0", &digest2));
        assert_eq!(out.state, DeploymentState::Failed);
        assert!(out.error.as_deref().unwrap().contains("rolled back"));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no flapping after rollback");
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.1.0"));

        // A DIFFERENT version clears the hold and proceeds.
        let digest3 = digest_bytes(b"newer");
        stage(&b, "0.3.0", b"newer");
        let out = up.apply(&spec("0.3.0", &digest3));
        assert_eq!(out.state, DeploymentState::Installing);
        assert_eq!(b.held_version(), None);
        assert_eq!(b.current_target().as_deref(), Some("reeve-agent-0.3.0"));
    }

    #[test]
    fn update_spec_detection_and_validation() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("apps/reeve-agent");
        fs::create_dir_all(app.join("files")).unwrap();
        assert!(update_spec(&app).is_none(), "no marker => normal workload");

        let digest = format!("{}{}", "sha256:", "a".repeat(64));
        let yaml = format!(
            "version: \"0.2.0\"\nbinary:\n  url: /v2/reeve/agent/blobs/{digest}\n  digest: {digest}\n  sizeBytes: 5\n"
        );
        // marker under files/ (tree-authored config file path, D2)
        fs::write(app.join("files").join(AGENT_UPDATE_FILE), &yaml).unwrap();
        let spec = update_spec(&app).unwrap().unwrap();
        assert_eq!(spec.version, "0.2.0");
        assert_eq!(spec.binary.size_bytes, Some(5));

        // root marker wins over files/
        fs::write(app.join(AGENT_UPDATE_FILE), &yaml).unwrap();
        assert!(update_spec(&app).unwrap().is_ok());

        // bad digest grammar => Some(Err) — must FAIL, not fall
        // through to the compose provider.
        fs::write(
            app.join(AGENT_UPDATE_FILE),
            "version: \"0.2.0\"\nbinary:\n  url: /x\n  digest: notadigest\n",
        )
        .unwrap();
        assert!(update_spec(&app).unwrap().is_err());

        // unsafe version string (path traversal in a file name)
        fs::write(
            app.join(AGENT_UPDATE_FILE),
            format!("version: \"../evil\"\nbinary:\n  url: /x\n  digest: {digest}\n"),
        )
        .unwrap();
        assert!(update_spec(&app).unwrap().is_err());
    }

    #[tokio::test]
    async fn fetcher_dir_source_reads_and_prefetch_stages() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("media");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("agent-bin"), b"new-agent").unwrap();
        let digest = digest_bytes(b"new-agent");

        // bundle with an agent-update app
        let data = t.path().join("data");
        fs::create_dir_all(data.join("bundles/x/apps/reeve-agent")).unwrap();
        fs::write(
            data.join("bundles/x/apps/reeve-agent").join(AGENT_UPDATE_FILE),
            format!("version: \"0.2.0\"\nbinary:\n  url: agent-bin\n  digest: {digest}\n"),
        )
        .unwrap();
        std::os::unix::fs::symlink("bundles/x", data.join("bundle")).unwrap();

        let mut db = AgentDb::open(&data.join("agent.db")).unwrap();
        let store = BundleStore::open(&data).unwrap();
        let bin = BinDir::new(&t.path().join("lib"));
        let fetcher = BinaryFetcher::from_config(&format!("dir://{}", src.display()), None).unwrap();

        prefetch(&mut db, &store, &bin, Some(&fetcher), "0.1.0").await;
        assert!(bin.staged_ok("0.2.0", &digest), "prefetch stages the binary");

        // running version already at target => no work, no fetch needed
        prefetch(&mut db, &store, &bin, None, "0.2.0").await;
    }

    #[tokio::test]
    async fn fetcher_http_joins_origin_and_authenticates() {
        use axum::extract::Path as AxPath;
        use axum::routing::get;
        use axum::Router;

        let app = Router::new().route(
            "/v2/reeve/agent/blobs/{digest}",
            get(|headers: axum::http::HeaderMap, AxPath(digest): AxPath<String>| async move {
                assert_eq!(
                    headers.get("authorization").unwrap().to_str().unwrap(),
                    "Bearer tok-1"
                );
                assert!(digest.starts_with("sha256:"));
                b"agent-bytes".to_vec()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let fetcher =
            BinaryFetcher::from_config(&format!("http://{addr}/"), Some("tok-1".into())).unwrap();
        let digest = digest_bytes(b"agent-bytes");
        let bytes = fetcher
            .fetch(&format!("/v2/reeve/agent/blobs/{digest}"))
            .await
            .unwrap();
        assert_eq!(bytes, b"agent-bytes");
    }
}
