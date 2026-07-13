//! Converge — diff the swapped-in render bundle against applied
//! state and drive the [`Provider`] through the D5 phase machine
//! (build item B3; core, unconditional).
//!
//! Normative sources:
//! - docs/decisions/agent.md D5: diff = content hash per app dir vs
//!   the bundle; unchanged hash => silent skip. Journal phases
//!   planned -> applying -> applied | failed, removing -> removed;
//!   intent recorded BEFORE action; startup re-runs any row not in a
//!   terminal phase; re-running any phase is a no-op when its
//!   postcondition holds. Removal = down against the RETAINED
//!   last-applied copy, down before delete.
//! - docs/decisions/tree-render.md D2: one app dir = one unit of
//!   convergence; present dir = desired, absent dir = remove; agent-
//!   local `apps/<name>/env/<service>.env` lives OUTSIDE the hashed
//!   bundle dir. Env-targeted params inject
//!   `env_file: [env/<service>.env]` into EVERY compose service, so
//!   the agent MUST materialize an env file per service (empty ok)
//!   or `docker compose` fails.
//! - CLAUDE.md Law 5: converge consumes only local state (the
//!   swapped bundle + agent.db) — the first converge after restart
//!   works with the server unreachable.
//!
//! Crash-only mechanics (Law 3): the phase row is written (one
//! SQLite transaction) BEFORE every action; `kill -9` at any point
//! leaves a non-terminal phase that the next converge pass re-runs,
//! and every action (dir sync, `up -d`, `down`, retained copy) is
//! idempotent, so re-running is a no-op when the postcondition
//! already holds.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use reeve_types::margo::deployment::ApplicationDeployment;
use reeve_types::margo::status::DeploymentState;
use reeve_types::reeve::manifest::AppManifestEntry;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::bundle::BundleStore;
use crate::provider::{COMPOSE_FILE, Provider};
use crate::state::{AgentDb, Severity};

/// Live convergence dirs under the agent data dir:
/// `apps/<name>/{compose.yml, files/, deployment.yaml, env/}` — the
/// bundle app dir staged to a MUTABLE, stable path (bundle content
/// under `bundles/<hex>` is immutable and env files must live
/// outside it, docs/decisions/tree-render.md D2).
pub const APPS_DIR: &str = "apps";
/// Retained last-applied copies: `applied/<name>/` (docs/decisions/
/// agent.md D5: what removal `down`s against, removed after a
/// successful down).
pub const APPLIED_DIR: &str = "applied";
/// Rendered per-device desired-state document inside an app dir
/// (docs/decisions/tree-render.md D2: the STATUS contract).
pub const DEPLOYMENT_FILE: &str = "deployment.yaml";

/// What the device should be running, resolved from LOCAL state only
/// (Law 5).
#[derive(Debug)]
pub enum Desired {
    /// Nothing known yet (no accepted manifest AND no swapped
    /// bundle): converge does nothing — never remove workloads on
    /// ignorance.
    Unknown,
    /// A known desired set (possibly empty: an accepted manifest
    /// with `bundle: null` means "run nothing").
    Known { apps: Vec<DesiredApp> },
}

/// One desired app: its bundle dir + its State-Manifest entry (the
/// `secrets_version` / `deployment_id` side channel, docs/decisions/
/// delivery.md D13).
#[derive(Debug)]
pub struct DesiredApp {
    pub name: String,
    /// The app dir inside the swapped bundle (read-only source).
    pub dir: PathBuf,
    pub entry: Option<AppManifestEntry>,
}

/// Resolve desired state from agent.db + the bundle store — local
/// reads only, no network (Law 5):
/// - accepted manifest with `bundle: null` => empty desired set
///   (remove everything);
/// - a swapped bundle on disk => its `apps/` dirs (the LAST KNOWN
///   state even if a newer accept's pull hasn't landed yet);
/// - neither => [`Desired::Unknown`].
pub fn resolve_desired(db: &AgentDb, store: &BundleStore) -> Desired {
    let accepted = match db.last_accepted() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "cannot read accepted manifest; skipping converge");
            return Desired::Unknown;
        }
    };
    let manifest_apps: BTreeMap<String, AppManifestEntry> = accepted
        .as_ref()
        .map(|a| {
            a.manifest
                .apps
                .iter()
                .map(|e| (e.app_id.clone(), e.clone()))
                .collect()
        })
        .unwrap_or_default();

    // Explicit empty desired state: `bundle: null`, zero apps
    // (reeve-types StateManifest: the property is present-with-null,
    // never omitted). This is a POSITIVE statement to run nothing.
    if let Some(a) = &accepted
        && a.manifest.bundle.is_none()
    {
        return Desired::Known { apps: vec![] };
    }

    // Otherwise converge from the bundle actually swapped in — the
    // last known good state (Law 5). No bundle on disk => unknown.
    if store.current_digest().is_none() {
        return Desired::Unknown;
    }
    let apps_root = store.current_path().join(APPS_DIR);
    let mut apps = Vec::new();
    if apps_root.is_dir() {
        let mut names: Vec<String> = match fs::read_dir(&apps_root) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect(),
            Err(e) => {
                warn!(error = %e, "cannot list bundle apps; skipping converge");
                return Desired::Unknown;
            }
        };
        names.sort();
        for name in names {
            apps.push(DesiredApp {
                dir: apps_root.join(&name),
                entry: manifest_apps.get(&name).cloned(),
                name,
            });
        }
    }
    Desired::Known { apps }
}

/// Outcome of converging one app — the input to the status report
/// (spec/reeve/05-health-journal.md §7.3; report.rs turns this into
/// a wire-exact `DeploymentStatusManifest`).
#[derive(Debug, Clone, PartialEq)]
pub struct AppReport {
    pub app_id: String,
    /// From the rendered deployment.yaml's `id` (docs/decisions/
    /// tree-render.md D2: deployment.yaml is the STATUS contract),
    /// falling back to the State-Manifest entry, then the app name.
    pub deployment_id: String,
    /// `metadata.name` of the deployment (Margo `deployment-status.md`:
    /// error `source` is the deployment's metadata.name).
    pub deployment_name: String,
    pub state: DeploymentState,
    /// One component entry per deployment.yaml `components[]`
    /// (Margo: "MUST contain one entry for each component").
    pub components: Vec<String>,
    pub error: Option<String>,
    /// COMBINED `docker compose up`/`down` output captured for this
    /// attempt, harvested from [`Provider::take_capture`] — the
    /// ext-logs seam (REV-011). Always populated by the compose
    /// provider, `None` for the agent-update path and for providers
    /// that capture nothing. Core carries it as plain data; only the
    /// ext-logs hook (main.rs, behind `ext-logs`) reads it, so the
    /// `--no-default-features` build simply never looks at the field.
    pub captured: Option<crate::provider::CapturedRun>,
}

/// Status-contract fields read from an app dir's deployment.yaml.
#[derive(Debug, Default)]
struct DeployMeta {
    id: Option<String>,
    name: Option<String>,
    components: Vec<String>,
}

fn read_deploy_meta(app_dir: &Path) -> DeployMeta {
    let path = app_dir.join(DEPLOYMENT_FILE);
    let Ok(text) = fs::read_to_string(&path) else {
        return DeployMeta::default();
    };
    match serde_yaml_ng::from_str::<ApplicationDeployment>(&text) {
        Ok(d) => DeployMeta {
            id: d.id,
            name: Some(d.metadata.name),
            components: d
                .spec
                .deployment_profile
                .components
                .iter()
                .map(|c| c.name.clone())
                .collect(),
        },
        Err(e) => {
            warn!(path = %path.display(), error = %e, "unparseable deployment.yaml; status will carry no components");
            DeployMeta::default()
        }
    }
}

/// Content hash of one app dir: sha256 over the sorted relative
/// paths and bytes of every regular file (docs/decisions/agent.md
/// D5: "diff = content hash per app dir, recorded in agent.db, vs
/// unpacked bundle"). Grammar `sha256:<hex>` like every other digest
/// in the system.
pub fn content_hash_dir(dir: &Path) -> std::io::Result<String> {
    let mut files = walk_files(dir)?;
    files.sort();
    let mut h = Sha256::new();
    for rel in &files {
        let bytes = fs::read(dir.join(rel))?;
        // Unambiguous framing: path, NUL, length, bytes.
        h.update(rel.to_string_lossy().as_bytes());
        h.update([0u8]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(format!("sha256:{:x}", h.finalize()))
}

/// Relative paths of every regular file under `root` (recursive).
fn walk_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    fn inner(root: &Path, prefix: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in fs::read_dir(root.join(prefix))? {
            let entry = entry?;
            let rel = prefix.join(entry.file_name());
            let ty = entry.file_type()?;
            if ty.is_dir() {
                inner(root, &rel, out)?;
            } else if ty.is_file() {
                out.push(rel);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    inner(root, Path::new(""), &mut out)?;
    Ok(out)
}

/// Stage one bundle app dir to its mutable convergence dir
/// `data_dir/apps/<name>/`: copy every file from the (immutable)
/// bundle dir, prune files the bundle no longer has — EXCEPT `env/`,
/// which is agent-local state outside the hashed bundle
/// (docs/decisions/tree-render.md D2) — then ensure one
/// `env/<service>.env` per compose service (empty ok; rendered
/// compose files reference them and `docker compose` fails on a
/// missing env_file). Idempotent; the stable path keeps bind-mount
/// sources valid across bundle updates.
fn stage_app(bundle_app: &Path, staged: &Path) -> std::io::Result<()> {
    fs::create_dir_all(staged)?;
    let mut src_files = walk_files(bundle_app)?;
    src_files.sort();
    for rel in &src_files {
        let dst = staged.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(bundle_app.join(rel), &dst)?;
        // Durable before we declare the phase advanced (Law 3).
        File::open(&dst)?.sync_all()?;
    }
    let keep: BTreeSet<&PathBuf> = src_files.iter().collect();
    for rel in walk_files(staged)? {
        if keep.contains(&rel) || rel.starts_with("env") {
            continue;
        }
        fs::remove_file(staged.join(&rel))?;
    }
    ensure_env_files(staged)?;
    File::open(staged)?.sync_all()?;
    Ok(())
}

/// Materialize `env/<service>.env` for every service in the staged
/// compose.yml (empty ok, 0600 — the file will hold resolved secrets
/// later, docs/decisions/tree-render.md D2 / secrets D15). Never
/// truncates an existing file.
fn ensure_env_files(staged: &Path) -> std::io::Result<()> {
    let compose = staged.join(COMPOSE_FILE);
    let Ok(text) = fs::read_to_string(&compose) else {
        return Ok(()); // no compose file: nothing references env files
    };
    let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) else {
        return Ok(()); // provider will surface the real error
    };
    let Some(services) = doc.get("services").and_then(|s| s.as_mapping()) else {
        return Ok(());
    };
    let env_dir = staged.join("env");
    for (name, _) in services {
        let Some(name) = name.as_str() else { continue };
        let path = env_dir.join(format!("{name}.env"));
        if path.exists() {
            continue;
        }
        fs::create_dir_all(&env_dir)?;
        let file = File::create(&path)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

/// Copy a whole dir tree (used for the retained applied/ copy).
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for rel in walk_files(src)? {
        let to = dst.join(&rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src.join(&rel), &to)?;
        File::open(&to)?.sync_all()?;
    }
    Ok(())
}

/// Refresh the retained last-applied copy `applied/<name>/` from the
/// staged dir (docs/decisions/agent.md D5): copy-to-temp, swap.
/// A crash mid-refresh leaves either the old copy, no copy (removal
/// falls back to the staged dir), or the new copy — all recoverable.
fn retain_applied(data_dir: &Path, name: &str, staged: &Path) -> std::io::Result<()> {
    let applied_root = data_dir.join(APPLIED_DIR);
    fs::create_dir_all(&applied_root)?;
    let tmp = applied_root.join(format!(".tmp-{name}"));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    copy_dir(staged, &tmp)?;
    let final_dir = applied_root.join(name);
    if final_dir.exists() {
        fs::remove_dir_all(&final_dir)?;
    }
    fs::rename(&tmp, &final_dir)?;
    File::open(&applied_root)?.sync_all()?;
    Ok(())
}

/// One converge pass: diff desired (bundle) against applied
/// (agent.db), apply changed/new apps, remove vanished ones. Returns
/// one [`AppReport`] per app that was ACTED on — a fully converged
/// pass returns an empty vec (D5: no-op convergence is silent).
///
/// Runs identically at startup and after every poll; startup
/// recovery falls out of the diff (any non-terminal phase fails the
/// `phase == "applied"` check and re-runs, D5).
///
/// Convenience wrapper over [`converge_full`] with no agent-update
/// path — agent-update apps then fail with a clear message.
pub fn converge(
    db: &mut AgentDb,
    data_dir: &Path,
    provider: &dyn Provider,
    desired: &Desired,
) -> Vec<AppReport> {
    converge_full(db, data_dir, provider, None, desired)
}

/// [`converge`] plus the agent self-update apply path (B8,
/// spec/reeve/08-packaging.md §10.5): apps carrying
/// [`crate::update::AGENT_UPDATE_FILE`] are routed to `updater`
/// instead of the workload provider, through the same D5 phase
/// machine.
pub fn converge_full(
    db: &mut AgentDb,
    data_dir: &Path,
    provider: &dyn Provider,
    updater: Option<&crate::update::AgentUpdater>,
    desired: &Desired,
) -> Vec<AppReport> {
    let Desired::Known { apps } = desired else {
        return Vec::new();
    };
    let current: BTreeMap<String, crate::state::AppliedApp> = match db.applied_apps() {
        Ok(rows) => rows.into_iter().map(|r| (r.app_id.clone(), r)).collect(),
        Err(e) => {
            warn!(error = %e, "cannot read applied state; skipping converge");
            return Vec::new();
        }
    };
    let desired_names: BTreeSet<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    let mut reports = Vec::new();

    for app in apps {
        let hash = match content_hash_dir(&app.dir) {
            Ok(h) => h,
            Err(e) => {
                warn!(app = %app.name, error = %e, "cannot hash bundle app dir; skipping app");
                let _ = db.journal(
                    Severity::Error,
                    "app-hash-failed",
                    &format!("{}: {e}", app.name),
                );
                continue;
            }
        };
        let sv = app.entry.as_ref().and_then(|e| e.secrets_version.clone());
        if let Some(row) = current.get(&app.name)
            && row.phase == "applied"
            && row.content_hash == hash
            && row.secrets_version == sv
        {
            continue; // unchanged => silent skip (D5)
        }
        reports.push(apply_app(db, data_dir, provider, updater, app, &hash, sv.as_deref()));
    }

    // Absent dir = remove (D2). `removed` is terminal: those rows
    // are history, not work.
    for (name, row) in &current {
        if row.phase == "removed" || desired_names.contains(name.as_str()) {
            continue;
        }
        reports.push(remove_app(db, data_dir, provider, name, row));
    }
    reports
}

/// Drive one app through planned -> applying -> applied | failed
/// (docs/decisions/agent.md D5). Intent is recorded before every
/// action; if recording fails the action is NOT taken (an unrecorded
/// action could not be resumed after a crash, Law 3).
fn apply_app(
    db: &mut AgentDb,
    data_dir: &Path,
    provider: &dyn Provider,
    updater: Option<&crate::update::AgentUpdater>,
    app: &DesiredApp,
    hash: &str,
    secrets_version: Option<&str>,
) -> AppReport {
    let meta = read_deploy_meta(&app.dir);
    let deployment_id = app
        .entry
        .as_ref()
        .and_then(|e| e.deployment_id.clone())
        .or(meta.id)
        .unwrap_or_else(|| app.name.clone());
    let mut report = AppReport {
        app_id: app.name.clone(),
        deployment_id,
        deployment_name: meta.name.unwrap_or_else(|| app.name.clone()),
        state: DeploymentState::Pending,
        components: meta.components,
        error: None,
        captured: None,
    };
    let fail = |report: &mut AppReport, msg: String| {
        report.state = DeploymentState::Failed;
        report.error = Some(msg);
    };

    // Intent BEFORE action (D5).
    if let Err(e) = db.record_phase(&app.name, hash, secrets_version, "planned", &format!("hash {hash}")) {
        fail(&mut report, format!("cannot record planned phase: {e}"));
        return report;
    }
    // Agent self-update apps (spec/reeve/08-packaging.md §10.5,
    // B8): recognized by the AGENT_UPDATE_FILE marker, applied by
    // the A/B updater instead of the workload provider — no compose
    // staging, no retained copy (there is nothing to `down` later;
    // removal takes the no-copy path). Same phase machine: `applied`
    // is recorded ONLY once the running agent IS the target version,
    // so a crash/restart anywhere re-runs this branch (Law 3).
    if let Some(parsed) = crate::update::update_spec(&app.dir) {
        let spec = match parsed {
            Ok(spec) => spec,
            Err(e) => {
                let msg = format!("unusable agent-update descriptor: {e}");
                let _ = db.record_phase(&app.name, hash, secrets_version, "failed", &msg);
                fail(&mut report, msg);
                return report;
            }
        };
        let Some(updater) = updater else {
            let msg = "agent-update app present but no updater configured".to_string();
            let _ = db.record_phase(&app.name, hash, secrets_version, "failed", &msg);
            fail(&mut report, msg);
            return report;
        };
        if let Err(e) = db.record_phase(&app.name, hash, secrets_version, "applying", "agent-update") {
            fail(&mut report, format!("cannot record applying phase: {e}"));
            return report;
        }
        // NOTE: with the production ExitRestarter this call does not
        // return once the swap lands — the exit is the restart
        // request; the phase stays `applying` (non-terminal) and the
        // NEW binary's first converge pass finishes it (Law 3).
        let outcome = updater.apply(&spec);
        if let Some(detail) = &outcome.detail {
            let _ = db.journal(Severity::Info, "agent-update-progress", &format!("{}: {detail}", app.name));
        }
        match outcome.state {
            DeploymentState::Installed => {
                match db.record_phase(&app.name, hash, secrets_version, "applied", &format!("agent version {}", spec.version)) {
                    Ok(()) => {
                        info!(app = %app.name, version = %spec.version, "agent update converged");
                        report.state = DeploymentState::Installed;
                    }
                    Err(e) => fail(&mut report, format!("cannot record applied phase: {e}")),
                }
            }
            DeploymentState::Failed => {
                let msg = outcome.error.unwrap_or_else(|| "agent update failed".into());
                warn!(app = %app.name, error = %msg, "agent update failed");
                let _ = db.record_phase(&app.name, hash, secrets_version, "failed", &msg);
                fail(&mut report, msg);
            }
            // In flight (binary not fetched / restart pending):
            // phase stays `applying`, re-checked next pass.
            state => report.state = state,
        }
        return report;
    }
    let staged = data_dir.join(APPS_DIR).join(&app.name);
    if let Err(e) = stage_app(&app.dir, &staged) {
        let msg = format!("staging failed: {e}");
        let _ = db.record_phase(&app.name, hash, secrets_version, "failed", &msg);
        fail(&mut report, msg);
        return report;
    }
    if let Err(e) = db.record_phase(&app.name, hash, secrets_version, "applying", "") {
        fail(&mut report, format!("cannot record applying phase: {e}"));
        return report;
    }
    let apply_result = provider.apply(&staged);
    // Harvest the combined up-output for ext-logs BEFORE branching, so
    // both success and failure carry it (REV-011). No-op for providers
    // that capture nothing.
    report.captured = provider.take_capture();
    match apply_result {
        Ok(status) => {
            if let Err(e) = retain_applied(data_dir, &app.name, &staged) {
                // Leave the phase at `applying`: the retained copy is
                // part of the postcondition (removal depends on it);
                // the next pass re-runs (up -d is a no-op).
                let msg = format!("retaining applied copy failed: {e}");
                let _ = db.journal(Severity::Error, "app-retain-failed", &format!("{}: {msg}", app.name));
                fail(&mut report, msg);
                return report;
            }
            match db.record_phase(&app.name, hash, secrets_version, "applied", &format!("hash {hash}")) {
                Ok(()) => {
                    info!(app = %app.name, state = ?status.state, "app converged");
                    report.state = status.state;
                    if let Some(detail) = status.detail {
                        let _ = db.journal(Severity::Info, "app-status-detail", &format!("{}: {detail}", app.name));
                    }
                }
                Err(e) => fail(&mut report, format!("cannot record applied phase: {e}")),
            }
        }
        Err(e) => {
            let msg = e.to_string();
            warn!(app = %app.name, error = %msg, "provider apply failed");
            let _ = db.record_phase(&app.name, hash, secrets_version, "failed", &msg);
            fail(&mut report, msg);
        }
    }
    report
}

/// Drive one app through removing -> removed (docs/decisions/agent.md
/// D5): down against the retained applied/ copy (falling back to the
/// staged dir if a crash preempted retention), delete dirs only
/// after a successful down. A failed down leaves the phase at
/// `removing` — non-terminal, re-run next pass.
fn remove_app(
    db: &mut AgentDb,
    data_dir: &Path,
    provider: &dyn Provider,
    name: &str,
    row: &crate::state::AppliedApp,
) -> AppReport {
    let retained = data_dir.join(APPLIED_DIR).join(name);
    let staged = data_dir.join(APPS_DIR).join(name);
    // Down before delete: prefer the retained last-applied copy; a
    // staged-but-never-retained dir still names any containers a
    // partial `up` created.
    let down_dir = if retained.is_dir() {
        Some(retained.clone())
    } else if staged.is_dir() {
        Some(staged.clone())
    } else {
        None
    };
    let meta = down_dir.as_deref().map(read_deploy_meta).unwrap_or_default();
    let mut report = AppReport {
        app_id: name.to_string(),
        deployment_id: meta.id.unwrap_or_else(|| name.to_string()),
        deployment_name: meta.name.unwrap_or_else(|| name.to_string()),
        state: DeploymentState::Removing,
        components: meta.components,
        error: None,
        captured: None,
    };

    // Intent BEFORE action (D5). Keep the row's hash: it still names
    // what was applied.
    if let Err(e) = db.record_phase(name, &row.content_hash, row.secrets_version.as_deref(), "removing", "") {
        report.error = Some(format!("cannot record removing phase: {e}"));
        return report;
    }
    let down = match &down_dir {
        Some(dir) => provider.remove(dir),
        None => {
            // Nothing on disk to down with — either it was never
            // applied or external interference removed our copies.
            // Declare removed but leave the evidence in the journal.
            let _ = db.journal(
                Severity::Notable,
                "app-remove-no-copy",
                &format!("{name}: no retained or staged dir; skipping down"),
            );
            Ok(())
        }
    };
    // Harvest the combined down-output for ext-logs (REV-011); `None`
    // when nothing was `down`ed (no copy) or the provider captures
    // nothing.
    report.captured = provider.take_capture();
    match down {
        Ok(()) => {
            // Down succeeded => applied copy removed (D5), staged dir
            // (including agent-local env/) goes with it.
            for dir in [&retained, &staged] {
                if let Err(e) = fs::remove_dir_all(dir)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(dir = %dir.display(), error = %e, "could not delete app dir after down");
                }
            }
            match db.record_phase(name, &row.content_hash, row.secrets_version.as_deref(), "removed", "") {
                Ok(()) => {
                    info!(app = %name, "app removed");
                    report.state = DeploymentState::Removed;
                }
                Err(e) => {
                    report.error = Some(format!("cannot record removed phase: {e}"));
                }
            }
        }
        Err(e) => {
            let msg = e.to_string();
            warn!(app = %name, error = %msg, "provider down failed; will retry");
            let _ = db.journal(Severity::Error, "app-remove-failed", &format!("{name}: {msg}"));
            report.error = Some(msg);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AppStatus, ProviderError};
    use crate::state::Severity;
    use reeve_types::reeve::manifest::{BundleRef, ManifestVersion, StateManifest};
    use std::sync::Mutex;

    /// Recording fake provider (tests MUST NOT require docker).
    #[derive(Default)]
    struct FakeProvider {
        calls: Mutex<Vec<String>>,
        fail_apply: Mutex<BTreeSet<String>>,
    }

    impl FakeProvider {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn fail_apply_for(&self, app: &str) {
            self.fail_apply.lock().unwrap().insert(app.to_string());
        }
        fn clear_failures(&self) {
            self.fail_apply.lock().unwrap().clear();
        }
    }

    impl Provider for FakeProvider {
        fn apply(&self, app_dir: &Path) -> Result<AppStatus, ProviderError> {
            let name = app_dir.file_name().unwrap().to_str().unwrap().to_string();
            self.calls.lock().unwrap().push(format!("apply {name}"));
            if self.fail_apply.lock().unwrap().contains(&name) {
                return Err(ProviderError(format!("injected failure for {name}")));
            }
            Ok(AppStatus {
                state: DeploymentState::Installed,
                detail: None,
            })
        }
        fn remove(&self, retained_dir: &Path) -> Result<(), ProviderError> {
            let name = retained_dir.file_name().unwrap().to_str().unwrap();
            self.calls.lock().unwrap().push(format!("remove {name}"));
            Ok(())
        }
        fn status(&self, _app_dir: &Path) -> Result<AppStatus, ProviderError> {
            Ok(AppStatus {
                state: DeploymentState::Installed,
                detail: None,
            })
        }
    }

    const DEPLOYMENT_YAML: &str = "\
apiVersion: application.margo.org/v1alpha1
kind: ApplicationDeployment
id: 11111111-2222-3333-4444-555555555555
metadata:
  name: web-deploy
spec:
  applicationId: web
  deploymentProfile:
    type: docker-compose
    components:
      - name: web-stack
";

    const COMPOSE_YAML: &str = "\
services:
  api:
    image: example/api
    env_file: [env/api.env]
  worker:
    image: example/worker
    env_file: [env/worker.env]
";

    struct Harness {
        _data_dir: tempfile::TempDir,
        data_dir: PathBuf,
        db: AgentDb,
        store: BundleStore,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();
        let db = AgentDb::open(&data_dir.join("agent.db")).unwrap();
        let store = BundleStore::open(&data_dir).unwrap();
        Harness {
            data_dir,
            db,
            store,
            _data_dir: dir,
        }
    }

    /// Author a fake swapped-in bundle: `bundles/<tag>/apps/…` + the
    /// `bundle` symlink (what B2 leaves on disk).
    fn swap_bundle(h: &Harness, tag: &str, apps: &[(&str, &[(&str, &str)])]) {
        let root = h.data_dir.join("bundles").join(tag);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("manifest.yaml"), "deviceId: dev-1\n").unwrap();
        for (app, files) in apps {
            let app_dir = root.join("apps").join(app);
            fs::create_dir_all(&app_dir).unwrap();
            for (rel, content) in *files {
                let path = app_dir.join(rel);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }
        }
        let link = h.data_dir.join("bundle");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(Path::new("bundles").join(tag), &link).unwrap();
    }

    fn default_app_files() -> Vec<(&'static str, &'static str)> {
        vec![
            (COMPOSE_FILE, COMPOSE_YAML),
            (DEPLOYMENT_FILE, DEPLOYMENT_YAML),
            ("files/app.conf", "key = value\n"),
        ]
    }

    fn run(h: &mut Harness, provider: &dyn Provider) -> Vec<AppReport> {
        let desired = resolve_desired(&h.db, &h.store);
        converge(&mut h.db, &h.data_dir.clone(), provider, &desired)
    }

    fn phases(db: &AgentDb) -> BTreeMap<String, String> {
        db.applied_apps()
            .unwrap()
            .into_iter()
            .map(|a| (a.app_id, a.phase))
            .collect()
    }

    #[test]
    fn fresh_converge_applies_all_apps() {
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("db", &files), ("web", &files)]);
        let provider = FakeProvider::default();

        let reports = run(&mut h, &provider);
        assert_eq!(provider.calls(), vec!["apply db", "apply web"]);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].app_id, "db");
        assert_eq!(reports[0].state, DeploymentState::Installed);
        assert_eq!(reports[0].deployment_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(reports[0].deployment_name, "web-deploy");
        assert_eq!(reports[0].components, vec!["web-stack"]);
        assert_eq!(
            phases(&h.db),
            BTreeMap::from([("db".into(), "applied".into()), ("web".into(), "applied".into())])
        );
        // Staged convergence dirs + env files per compose service.
        for app in ["db", "web"] {
            let staged = h.data_dir.join(APPS_DIR).join(app);
            assert!(staged.join(COMPOSE_FILE).is_file());
            assert!(staged.join("files/app.conf").is_file());
            assert!(staged.join("env/api.env").is_file(), "{app} env/api.env");
            assert!(staged.join("env/worker.env").is_file());
            // Retained last-applied copy (D5).
            assert!(h.data_dir.join(APPLIED_DIR).join(app).join(COMPOSE_FILE).is_file());
        }
        // Phase journal: planned -> applying -> applied per app.
        let events: Vec<String> = h
            .db
            .journal_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(
            events,
            vec![
                "app-planned", "app-applying", "app-applied",
                "app-planned", "app-applying", "app-applied",
            ]
        );
    }

    #[test]
    fn unchanged_hash_is_a_silent_skip() {
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("web", &files)]);
        let provider = FakeProvider::default();
        run(&mut h, &provider);
        let journal_before = h.db.journal_entries().unwrap().len();

        let reports = run(&mut h, &provider);
        assert!(reports.is_empty(), "no-op convergence must be silent");
        assert_eq!(provider.calls(), vec!["apply web"], "no second provider call");
        assert_eq!(h.db.journal_entries().unwrap().len(), journal_before);
    }

    #[test]
    fn changed_app_reapplies_only_that_app() {
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("db", &files), ("web", &files)]);
        let provider = FakeProvider::default();
        run(&mut h, &provider);

        // New bundle: web's config changed, db identical.
        let changed: Vec<(&str, &str)> = vec![
            (COMPOSE_FILE, COMPOSE_YAML),
            (DEPLOYMENT_FILE, DEPLOYMENT_YAML),
            ("files/app.conf", "key = OTHER\n"),
        ];
        swap_bundle(&h, "bbb", &[("db", &files), ("web", &changed)]);

        let reports = run(&mut h, &provider);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].app_id, "web");
        assert_eq!(provider.calls(), vec!["apply db", "apply web", "apply web"]);
        assert_eq!(
            fs::read_to_string(h.data_dir.join("apps/web/files/app.conf")).unwrap(),
            "key = OTHER\n"
        );
    }

    #[test]
    fn removed_app_downs_retained_copy_then_deletes() {
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("db", &files), ("web", &files)]);
        let provider = FakeProvider::default();
        run(&mut h, &provider);

        // web vanishes from the bundle.
        swap_bundle(&h, "bbb", &[("db", &files)]);
        let reports = run(&mut h, &provider);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].app_id, "web");
        assert_eq!(reports[0].state, DeploymentState::Removed);
        // Removal used the retained copy and read its status contract.
        assert_eq!(reports[0].deployment_name, "web-deploy");
        assert_eq!(reports[0].components, vec!["web-stack"]);
        assert_eq!(
            provider.calls(),
            vec!["apply db", "apply web", "remove web"]
        );
        // Down before delete; dirs gone after success (D5).
        assert!(!h.data_dir.join(APPLIED_DIR).join("web").exists());
        assert!(!h.data_dir.join(APPS_DIR).join("web").exists());
        assert_eq!(phases(&h.db)["web"], "removed");
        // Terminal: a third pass does nothing.
        assert!(run(&mut h, &provider).is_empty());
        assert_eq!(provider.calls().len(), 3);
    }

    #[test]
    fn empty_manifest_removes_everything() {
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("web", &files)]);
        let provider = FakeProvider::default();
        run(&mut h, &provider);

        // Accepted manifest with bundle: null — positive "run nothing".
        h.db.record_accepted(
            &StateManifest {
                manifest_version: ManifestVersion(2),
                bundle: None,
                apps: vec![],
            },
            "sha256:etag",
            Severity::Info,
            "manifest-accepted",
            "",
        )
        .unwrap();
        let reports = run(&mut h, &provider);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, DeploymentState::Removed);
        assert_eq!(phases(&h.db)["web"], "removed");
    }

    #[test]
    fn unknown_desired_state_touches_nothing() {
        // No bundle, no accepted manifest: never remove on ignorance.
        let mut h = harness();
        h.db.record_applied("web", "sha256:h", None, "applied").unwrap();
        let provider = FakeProvider::default();
        let reports = run(&mut h, &provider);
        assert!(reports.is_empty());
        assert!(provider.calls().is_empty());
        assert_eq!(phases(&h.db)["web"], "applied");

        // Accepted manifest naming a bundle whose pull hasn't landed:
        // still unknown, still untouched.
        h.db.record_accepted(
            &StateManifest {
                manifest_version: ManifestVersion(1),
                bundle: Some(BundleRef {
                    media_type: None,
                    digest: format!("sha256:{}", "a".repeat(64)),
                    size_bytes: None,
                    url: "/v2/x".into(),
                }),
                apps: vec![],
            },
            "sha256:etag",
            Severity::Info,
            "manifest-accepted",
            "",
        )
        .unwrap();
        assert!(run(&mut h, &provider).is_empty());
        assert!(provider.calls().is_empty());
    }

    #[test]
    fn provider_failure_records_failed_and_retries_next_pass() {
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("web", &files)]);
        let provider = FakeProvider::default();
        provider.fail_apply_for("web");

        let reports = run(&mut h, &provider);
        assert_eq!(reports[0].state, DeploymentState::Failed);
        assert!(reports[0].error.as_deref().unwrap().contains("injected failure"));
        assert_eq!(phases(&h.db)["web"], "failed");

        // Next pass retries (phase != applied) and succeeds.
        provider.clear_failures();
        let reports = run(&mut h, &provider);
        assert_eq!(reports[0].state, DeploymentState::Installed);
        assert_eq!(phases(&h.db)["web"], "applied");
        assert_eq!(provider.calls(), vec!["apply web", "apply web"]);
    }

    #[test]
    fn non_terminal_phase_reruns_even_with_matching_hash() {
        // Crash recovery: a row stuck at `applying` (kill -9 landed
        // mid-provider-call) re-runs although the hash matches (D5:
        // startup re-runs any non-terminal phase).
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("web", &files)]);
        let hash = content_hash_dir(&h.store.current_path().join("apps/web")).unwrap();
        h.db.record_applied("web", &hash, None, "applying").unwrap();

        let provider = FakeProvider::default();
        let reports = run(&mut h, &provider);
        assert_eq!(reports.len(), 1);
        assert_eq!(provider.calls(), vec!["apply web"]);
        assert_eq!(phases(&h.db)["web"], "applied");
    }

    #[test]
    fn secrets_version_change_reapplies_without_hash_change() {
        // D5: bundle digest unchanged + secrets_version changed =>
        // re-up affected apps.
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("web", &files)]);
        let manifest = |sv: &str| StateManifest {
            manifest_version: ManifestVersion(if sv == "sv1" { 1 } else { 2 }),
            bundle: Some(BundleRef {
                media_type: None,
                digest: format!("sha256:{}", "a".repeat(64)),
                size_bytes: None,
                url: "/v2/x".into(),
            }),
            apps: vec![AppManifestEntry {
                app_id: "web".into(),
                deployment_id: None,
                secrets_version: Some(sv.into()),
            }],
        };
        h.db.record_accepted(&manifest("sv1"), "sha256:e1", Severity::Info, "a", "")
            .unwrap();
        let provider = FakeProvider::default();
        run(&mut h, &provider);
        assert_eq!(provider.calls(), vec!["apply web"]);

        // Same bundle, bumped secrets_version.
        h.db.record_accepted(&manifest("sv2"), "sha256:e2", Severity::Info, "a", "")
            .unwrap();
        let reports = run(&mut h, &provider);
        assert_eq!(reports.len(), 1);
        assert_eq!(provider.calls(), vec!["apply web", "apply web"]);
        // And now converged: silent.
        assert!(run(&mut h, &provider).is_empty());
    }

    #[test]
    fn staging_preserves_agent_local_env_but_prunes_stale_files() {
        let mut h = harness();
        let files = default_app_files();
        swap_bundle(&h, "aaa", &[("web", &files)]);
        let provider = FakeProvider::default();
        run(&mut h, &provider);

        // Simulate secrets materialized into env/ (agent-local, D2)
        // and a file the next bundle drops.
        let staged = h.data_dir.join("apps/web");
        fs::write(staged.join("env/api.env"), "SECRET=x\n").unwrap();
        let changed: Vec<(&str, &str)> = vec![
            (COMPOSE_FILE, COMPOSE_YAML),
            (DEPLOYMENT_FILE, DEPLOYMENT_YAML),
            // files/app.conf dropped
            ("files/new.conf", "fresh\n"),
        ];
        swap_bundle(&h, "bbb", &[("web", &changed)]);
        run(&mut h, &provider);

        assert!(!staged.join("files/app.conf").exists(), "stale file pruned");
        assert!(staged.join("files/new.conf").is_file());
        assert_eq!(
            fs::read_to_string(staged.join("env/api.env")).unwrap(),
            "SECRET=x\n",
            "agent-local env content survives restage"
        );
    }

    /// B8 (spec/reeve/08-packaging.md §10.5): an app carrying
    /// agent-update.yaml routes to the A/B updater through the D5
    /// phase machine — the compose provider is never consulted, the
    /// terminal `applied` phase is recorded only once the running
    /// agent IS the target version, and a converged pass is silent.
    #[test]
    fn agent_update_app_routes_to_updater_not_provider() {
        use crate::update::{AgentUpdater, BinDir, UnitRestarter};

        struct NoopRestarter;
        impl UnitRestarter for NoopRestarter {
            fn restart(&self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut h = harness();
        let digest = {
            use sha2::Digest as _;
            format!("sha256:{:x}", Sha256::digest(b"new-agent"))
        };
        let update_yaml = format!(
            "version: \"0.9.9\"\nbinary:\n  url: /v2/reeve/agent/blobs/{digest}\n  digest: {digest}\n"
        );
        let files: Vec<(&str, &str)> = vec![
            (crate::update::AGENT_UPDATE_FILE, &update_yaml),
            (DEPLOYMENT_FILE, DEPLOYMENT_YAML),
        ];
        swap_bundle(&h, "aaa", &[("reeve-agent", &files)]);
        let provider = FakeProvider::default();
        let bin = BinDir::new(&h.data_dir.join("lib"));
        bin.stage("0.1.0", b"old-agent", &{
            use sha2::Digest as _;
            format!("sha256:{:x}", Sha256::digest(b"old-agent"))
        })
        .unwrap();
        bin.swap_to("0.1.0").unwrap();
        bin.stage("0.9.9", b"new-agent", &digest).unwrap();

        // Pass 1: running 0.1.0 -> swap + restart requested; phase
        // stays `applying` (non-terminal, Law 3), provider untouched.
        let updater = AgentUpdater::new(bin.clone(), Box::new(NoopRestarter), "0.1.0");
        let desired = resolve_desired(&h.db, &h.store);
        let reports = converge_full(&mut h.db, &h.data_dir.clone(), &provider, Some(&updater), &desired);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, DeploymentState::Installing);
        assert_eq!(reports[0].deployment_name, "web-deploy", "status contract still read");
        assert!(provider.calls().is_empty(), "compose provider must never see the update app");
        assert_eq!(phases(&h.db)["reeve-agent"], "applying");
        assert_eq!(bin.current_target().as_deref(), Some("reeve-agent-0.9.9"));

        // Pass 2 ("after re-exec"): running the target => applied.
        let updater = AgentUpdater::new(bin.clone(), Box::new(NoopRestarter), "0.9.9");
        let desired = resolve_desired(&h.db, &h.store);
        let reports = converge_full(&mut h.db, &h.data_dir.clone(), &provider, Some(&updater), &desired);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, DeploymentState::Installed);
        assert_eq!(phases(&h.db)["reeve-agent"], "applied");

        // Pass 3: converged => silent (D5).
        let desired = resolve_desired(&h.db, &h.store);
        assert!(converge_full(&mut h.db, &h.data_dir.clone(), &provider, Some(&updater), &desired).is_empty());
        assert!(provider.calls().is_empty());
    }

    /// An unusable update descriptor FAILS the app — it must never
    /// fall through to the compose provider.
    #[test]
    fn broken_agent_update_descriptor_fails_without_provider() {
        let mut h = harness();
        let files: Vec<(&str, &str)> = vec![
            (crate::update::AGENT_UPDATE_FILE, "version: [not, a, string]\n"),
        ];
        swap_bundle(&h, "aaa", &[("reeve-agent", &files)]);
        let provider = FakeProvider::default();
        let reports = run(&mut h, &provider);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, DeploymentState::Failed);
        assert!(reports[0].error.as_deref().unwrap().contains("agent-update"));
        assert!(provider.calls().is_empty());
        assert_eq!(phases(&h.db)["reeve-agent"], "failed");
    }

    #[test]
    fn content_hash_is_stable_and_sensitive() {
        let a = tempfile::tempdir().unwrap();
        fs::create_dir_all(a.path().join("files")).unwrap();
        fs::write(a.path().join("compose.yml"), "services: {}\n").unwrap();
        fs::write(a.path().join("files/x.conf"), "1").unwrap();
        let h1 = content_hash_dir(a.path()).unwrap();
        assert!(reeve_types::reeve::manifest::is_sha256_digest(&h1));
        assert_eq!(content_hash_dir(a.path()).unwrap(), h1, "deterministic");
        fs::write(a.path().join("files/x.conf"), "2").unwrap();
        assert_ne!(content_hash_dir(a.path()).unwrap(), h1, "content-sensitive");
    }
}
