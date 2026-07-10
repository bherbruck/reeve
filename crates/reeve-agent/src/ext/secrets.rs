//! ext-secrets (REV-009) — secrets fetch + per-service env
//! materialization (build item B4).
//!
//! Normative sources:
//! - spec/reeve/10-secrets.md §12.3: at apply time the agent calls
//!   `POST /api/reeve/v1/secrets/resolve` with its device credential;
//!   plaintext exists only in server RAM, TLS in flight, and the
//!   device's env files at rest (0600, temp+rename, agent-local,
//!   OUTSIDE the hashed bundle dir). Env is materialized PER SERVICE
//!   (`apps/<name>/env/<service>.env`), only the values targeted at
//!   that component; rendered compose references them via `env_file`.
//! - spec/reeve/10-secrets.md §12.4 rotation: bundle digest unchanged
//!   with `secrets_version` changed => re-resolve, rewrite ONLY env
//!   files whose content differs, `up -d` affected apps, no bundle
//!   re-pull. Compose recreates only services whose resolved config
//!   changed — restart semantics delegated to compose's own diff
//!   (D15, Law 4).
//! - spec/reeve/10-secrets.md §12.3 offline (Law 5): the resolve
//!   endpoint being unreachable never blocks convergence of
//!   already-resolved apps — they apply from last materialized env
//!   files. An app that was NEVER materialized is deferred instead
//!   (there is no last known state to continue from).
//! - docs/decisions/secrets.md D15: secret values NEVER enter the
//!   journal, logs, or any persisted artifact other than the env
//!   files themselves.
//!
//! Placement: env files live in the MUTABLE staged app dir
//! `data_dir/apps/<name>/env/<service>.env` — the dir the compose
//! provider applies from (`-f apps/<name>/compose.yml`, cwd =
//! data_dir), so the render-emitted relative `env_file:
//! [env/<service>.env]` resolves to them, while the content-hashed
//! `bundles/<hex>` dirs stay untouched (docs/decisions/tree-render.md
//! D2). B3's staging already preserves `env/` across bundle
//! restages.
//!
//! Integration seam (docs/build-charter.md CODE BOUNDARY): core never
//! calls this module. The binary shell runs [`sync_env`] between
//! `resolve_desired` and `converge`; failure semantics are expressed
//! by mutating the [`Desired`] set (pinning an app's
//! `secrets_version` to its last-applied value, or deferring a
//! never-applied app), so core converge stays secrets-ignorant.
//!
//! Crash-only (Law 3): every env write is temp+fsync+rename; a crash
//! between an env rewrite and the converge that `up -d`s it leaves
//! `applied_state.secrets_version` at the old value, so the next pass
//! re-resolves (content-compare makes the rewrite a no-op) and
//! re-applies.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use reeve_types::margo::deployment::ApplicationDeployment;
use reeve_types::reeve::manifest::AppManifestEntry;
use reeve_types::reeve::secrets::{
    ResolvedSecret, SECRETS_RESOLVE_PATH, SecretsResolveRequest, SecretsResolveResponse,
};
use tracing::{info, warn};

use crate::converge::{APPS_DIR, DEPLOYMENT_FILE, Desired, DesiredApp, content_hash_dir};
use crate::provider::COMPOSE_FILE;
use crate::state::{AgentDb, Severity};

/// The in-band secret reference convention carried by parameter
/// values (spec/reeve/10-secrets.md §12; docs/decisions/secrets.md
/// D15 wire-exactness note): `${secret:<name>}`.
const REF_OPEN: &str = "${secret:";

/// Per-service env plan: `service -> var -> raw value` (raw values
/// may still contain `${secret:<name>}` references).
type EnvPlan = BTreeMap<String, BTreeMap<String, String>>;

/// Why a resolve call produced no values.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// Couldn't reach the endpoint (network down, DNS, timeout).
    /// Law 5: expected operation — the caller continues from last
    /// materialized env files.
    #[error("resolve endpoint unreachable: {0}")]
    Unreachable(String),
    /// Reached it but the exchange was invalid (bad status,
    /// unparseable body). Same continue-from-last-known path, logged
    /// at error severity.
    #[error("resolve protocol error: {0}")]
    Protocol(String),
}

/// Client for `POST /api/reeve/v1/secrets/resolve`
/// (spec/reeve/10-secrets.md §12.3) over the enrollment-issued
/// device bearer token (D1 provision-once).
pub struct SecretResolver {
    base: String,
    device_token: String,
    client: reqwest::Client,
}

impl SecretResolver {
    /// Construct from agent config values. `None` when there is no
    /// endpoint to call: `dir://` sources have no server (secrets on
    /// air-gap media are a gateway concern, §12.5), and an unenrolled
    /// agent has no device credential to ask with (§12.3: a device
    /// can only ask as itself).
    pub fn from_config(server: &str, device_token: Option<String>) -> Option<Self> {
        if !(server.starts_with("https://") || server.starts_with("http://")) {
            return None;
        }
        Some(SecretResolver {
            base: server.trim_end_matches('/').to_string(),
            device_token: device_token?,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("static reqwest client config"),
        })
    }

    /// Resolve `names` to plaintext values. The response lives in RAM
    /// only (§12.3); callers write it straight into env files and
    /// drop it.
    pub async fn resolve(
        &self,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, ResolvedSecret>, ResolveError> {
        let request = SecretsResolveRequest {
            secrets: names.iter().cloned().collect(),
        };
        let resp = self
            .client
            .post(format!("{}{SECRETS_RESOLVE_PATH}", self.base))
            .bearer_auth(&self.device_token)
            .json(&request)
            .send()
            .await
            .map_err(|e| ResolveError::Unreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ResolveError::Protocol(format!(
                "unexpected status {status} from {SECRETS_RESOLVE_PATH}"
            )));
        }
        let body: SecretsResolveResponse = resp
            .json()
            .await
            .map_err(|e| ResolveError::Protocol(format!("bad resolve body: {e}")))?;
        Ok(body.secrets)
    }
}

/// Outcome of one app's env sync — returned for observability and
/// tests; everything is already journaled. Apps that needed nothing
/// (converged, or no env-targeted parameters) produce no entry.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvSyncReport {
    pub app_id: String,
    /// Env files whose content was rewritten, relative to the staged
    /// app dir (e.g. `env/api.env`). §12.4: ONLY files whose content
    /// differs are rewritten.
    pub changed: Vec<String>,
    /// The app was removed from the desired set because it has
    /// secret references, no resolution, and no previously
    /// materialized state to continue from.
    pub deferred: bool,
    /// Resolve/materialization failure (never contains a value).
    pub error: Option<String>,
}

/// Materialize per-service env files for every desired app, resolving
/// `${secret:<name>}` references through `resolver`. Runs BEFORE a
/// converge pass and expresses failure by mutating `desired` (see
/// module docs) — infallible at the loop level (Law 5).
pub async fn sync_env(
    db: &AgentDb,
    data_dir: &Path,
    resolver: Option<&SecretResolver>,
    desired: &mut Desired,
) -> Vec<EnvSyncReport> {
    let Desired::Known { apps } = desired else {
        return Vec::new();
    };
    let applied: BTreeMap<String, crate::state::AppliedApp> = match db.applied_apps() {
        Ok(rows) => rows.into_iter().map(|r| (r.app_id.clone(), r)).collect(),
        Err(e) => {
            warn!(error = %e, "cannot read applied state; skipping env sync");
            return Vec::new();
        }
    };

    let mut reports = Vec::new();
    let mut deferred: BTreeSet<String> = BTreeSet::new();
    for app in apps.iter_mut() {
        match sync_app(db, data_dir, resolver, app, &applied).await {
            AppOutcome::Untouched => {}
            // A re-resolution that found every file identical is
            // silent, like a no-op converge (D5).
            AppOutcome::Changed(changed) if changed.is_empty() => {}
            AppOutcome::Changed(changed) => reports.push(EnvSyncReport {
                app_id: app.name.clone(),
                changed,
                deferred: false,
                error: None,
            }),
            AppOutcome::Held(msg) => {
                // Continue from last materialized env files (§12.3):
                // pin the manifest entry's secrets_version to the
                // last-applied one so converge neither records the
                // new version as satisfied nor blocks on it.
                let last = applied
                    .get(&app.name)
                    .filter(|row| row.phase != "removed")
                    .and_then(|row| row.secrets_version.clone());
                pin_secrets_version(app, last);
                reports.push(EnvSyncReport {
                    app_id: app.name.clone(),
                    changed: Vec::new(),
                    deferred: false,
                    error: Some(msg),
                });
            }
            AppOutcome::Deferred(msg) => {
                deferred.insert(app.name.clone());
                reports.push(EnvSyncReport {
                    app_id: app.name.clone(),
                    changed: Vec::new(),
                    deferred: true,
                    error: Some(msg),
                });
            }
        }
    }
    if !deferred.is_empty() {
        // Dropping a never-applied app from the desired set makes
        // converge ignore it entirely this pass (no applied row =>
        // absence is not a removal); it is retried next cycle.
        apps.retain(|a| !deferred.contains(&a.name));
    }
    reports
}

enum AppOutcome {
    /// Converged or nothing env-targeted: no writes, no journal.
    Untouched,
    /// Env files rewritten (possibly none, when re-resolution found
    /// identical content — still lets converge proceed normally).
    Changed(Vec<String>),
    /// Resolution failed but the app has materialized state: keep
    /// env files, pin secrets_version, let converge continue (Law 5).
    Held(String),
    /// Resolution failed and the app was never materialized: drop it
    /// from this pass's desired set.
    Deferred(String),
}

async fn sync_app(
    db: &AgentDb,
    data_dir: &Path,
    resolver: Option<&SecretResolver>,
    app: &DesiredApp,
    applied: &BTreeMap<String, crate::state::AppliedApp>,
) -> AppOutcome {
    let services = compose_services(&app.dir.join(COMPOSE_FILE));
    if services.is_empty() {
        return AppOutcome::Untouched; // nothing references env files
    }
    let Some(deployment) = read_deployment(&app.dir) else {
        return AppOutcome::Untouched; // converge/provider surface the real error
    };
    let component = deployment
        .spec
        .deployment_profile
        .components
        .first()
        .map(|c| c.name.clone());
    let plan = match plan_env(&deployment, component.as_deref(), &services) {
        Ok(Some(plan)) => plan,
        Ok(None) => return AppOutcome::Untouched, // no env-targeted parameters
        Err(msg) => {
            let _ = db.journal(
                Severity::Error,
                "secrets-env-invalid",
                &format!("{}: {msg}", app.name),
            );
            return hold_or_defer(app, applied, format!("invalid env parameter: {msg}"));
        }
    };

    let mut refs = BTreeSet::new();
    for vars in plan.values() {
        for raw in vars.values() {
            collect_secret_refs(raw, &mut refs);
        }
    }

    let entry_sv = app.entry.as_ref().and_then(|e| e.secrets_version.clone());
    let resolved = if refs.is_empty() {
        BTreeMap::new()
    } else {
        // Skip the network entirely when the app is fully converged
        // including secrets: applied at this exact bundle hash and
        // secrets_version, env files present. Rotation (§12.4) breaks
        // this check via secrets_version; a changed bundle breaks it
        // via the hash.
        if is_converged(data_dir, app, applied, &entry_sv, &services) {
            return AppOutcome::Untouched;
        }
        let outcome = match resolver {
            None => Err((
                Severity::Notable,
                "secrets-resolve-unavailable",
                "no resolve endpoint (dir:// source or not enrolled)".to_string(),
            )),
            Some(r) => match r.resolve(&refs).await {
                Ok(values) => {
                    let missing: Vec<&String> =
                        refs.iter().filter(|n| !values.contains_key(*n)).collect();
                    if missing.is_empty() {
                        Ok(values)
                    } else {
                        Err((
                            Severity::Error,
                            "secrets-missing",
                            format!("unresolved secret names: {missing:?}"),
                        ))
                    }
                }
                Err(ResolveError::Unreachable(reason)) => Err((
                    Severity::Notable,
                    "secrets-resolve-unreachable",
                    reason,
                )),
                Err(ResolveError::Protocol(reason)) => {
                    Err((Severity::Error, "secrets-resolve-failed", reason))
                }
            },
        };
        match outcome {
            Ok(values) => {
                // Audit metadata: names + versions, never values
                // (§12.6).
                let audit: Vec<String> = values
                    .iter()
                    .map(|(name, s)| format!("{name}@{}", s.version))
                    .collect();
                let _ = db.journal(
                    Severity::Info,
                    "secrets-resolved",
                    &format!("{}: {}", app.name, audit.join(" ")),
                );
                values
            }
            Err((severity, event, reason)) => {
                let _ = db.journal(severity, event, &format!("{}: {reason}", app.name));
                info!(app = %app.name, %reason, "secrets not resolved; continuing from last materialized env (Law 5)");
                return hold_or_defer(app, applied, reason);
            }
        }
    };

    // Substitute + write. Every compose service gets a file (empty
    // ok) — the rendered compose references env_file on all of them
    // (D2).
    let env_dir = data_dir.join(APPS_DIR).join(&app.name).join("env");
    let mut changed = Vec::new();
    for service in &services {
        let mut lines = String::new();
        if let Some(vars) = plan.get(service) {
            for (key, raw) in vars {
                let value = match substitute_secrets(raw, &resolved) {
                    Ok(v) => v,
                    Err(name) => {
                        // Unreachable when refs were fully resolved;
                        // defensive (a ref inside a resolved value is
                        // NOT re-substituted).
                        let _ = db.journal(
                            Severity::Error,
                            "secrets-missing",
                            &format!("{}: unresolved secret name {name:?}", app.name),
                        );
                        return hold_or_defer(app, applied, format!("unresolved secret {name:?}"));
                    }
                };
                lines.push_str(key);
                lines.push('=');
                lines.push_str(&value);
                lines.push('\n');
            }
        }
        let file = format!("{service}.env");
        match write_env_file(&env_dir.join(&file), &lines) {
            Ok(true) => changed.push(format!("env/{file}")),
            Ok(false) => {}
            Err(e) => {
                let _ = db.journal(
                    Severity::Error,
                    "secrets-env-write-failed",
                    &format!("{}: env/{file}: {e}", app.name),
                );
                return hold_or_defer(app, applied, format!("env write failed: {e}"));
            }
        }
    }
    if !changed.is_empty() {
        // File names only — never content (D15).
        let _ = db.journal(
            Severity::Info,
            "secrets-env-updated",
            &format!("{}: {}", app.name, changed.join(" ")),
        );
        info!(app = %app.name, files = changed.len(), "env files materialized");
    }
    AppOutcome::Changed(changed)
}

/// Failure disposition (§12.3 offline rule): previously materialized
/// => hold (keep env, pin version); never materialized => defer.
fn hold_or_defer(
    app: &DesiredApp,
    applied: &BTreeMap<String, crate::state::AppliedApp>,
    reason: String,
) -> AppOutcome {
    let has_state = applied
        .get(&app.name)
        .is_some_and(|row| row.phase != "removed");
    if has_state {
        AppOutcome::Held(reason)
    } else {
        AppOutcome::Deferred(reason)
    }
}

/// Pin the manifest entry's secrets_version to the last-applied one
/// so a failed resolve neither records the new version as satisfied
/// nor blocks a bundle-content apply (which correctly runs from the
/// last materialized env files, §12.3).
fn pin_secrets_version(app: &mut DesiredApp, last: Option<String>) {
    match (&mut app.entry, last) {
        (Some(entry), last) => entry.secrets_version = last,
        (entry @ None, Some(last)) => {
            *entry = Some(AppManifestEntry {
                app_id: app.name.clone(),
                deployment_id: None,
                secrets_version: Some(last),
            });
        }
        (None, None) => {}
    }
}

/// Fully converged including secrets: `applied` phase at this exact
/// bundle-dir hash and secrets_version, with every planned env file
/// present on disk.
fn is_converged(
    data_dir: &Path,
    app: &DesiredApp,
    applied: &BTreeMap<String, crate::state::AppliedApp>,
    entry_sv: &Option<String>,
    services: &[String],
) -> bool {
    let Some(row) = applied.get(&app.name) else {
        return false;
    };
    if row.phase != "applied" || row.secrets_version != *entry_sv {
        return false;
    }
    match content_hash_dir(&app.dir) {
        Ok(hash) if hash == row.content_hash => {}
        _ => return false,
    }
    let env_dir = data_dir.join(APPS_DIR).join(&app.name).join("env");
    services
        .iter()
        .all(|s| env_dir.join(format!("{s}.env")).is_file())
}

/// Parse the app's rendered deployment.yaml (docs/decisions/
/// tree-render.md D2: the parameters/targets source of truth on the
/// device).
fn read_deployment(app_dir: &Path) -> Option<ApplicationDeployment> {
    let text = fs::read_to_string(app_dir.join(DEPLOYMENT_FILE)).ok()?;
    match serde_yaml_ng::from_str(&text) {
        Ok(d) => Some(d),
        Err(e) => {
            warn!(app_dir = %app_dir.display(), error = %e, "unparseable deployment.yaml; env sync skipped");
            None
        }
    }
}

/// Service names from a rendered compose file (same read
/// `converge::ensure_env_files` performs).
fn compose_services(compose: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(compose) else {
        return Vec::new();
    };
    let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) else {
        return Vec::new();
    };
    let Some(services) = doc.get("services").and_then(|s| s.as_mapping()) else {
        return Vec::new();
    };
    services
        .iter()
        .filter_map(|(name, _)| name.as_str().map(str::to_string))
        .collect()
}

/// Per-service env plan: `service -> var -> raw value` (raw values
/// may contain `${secret:<name>}` references). `Ok(None)` when the
/// deployment has no env-shaped targets at all (mirrors render's
/// `has_env_parameters`: no `env_file` was injected).
///
/// Target matching (spec/reeve/10-secrets.md §12.3 "only the values
/// targeted at that component", grounded in the pinned fixtures):
/// a `targets[].components` entry naming the deployment-profile
/// COMPONENT (`nextcloud-compose` fixture: `nextcloud-stack`) applies
/// to every service of that component's compose file — Margo-exact,
/// since v1 has exactly one compose component per app (D2). An entry
/// naming a compose SERVICE narrows to that service (reeve's additive
/// per-service scoping, D15 balena-style). An empty `components`
/// list applies to all services.
fn plan_env(
    deployment: &ApplicationDeployment,
    component: Option<&str>,
    services: &[String],
) -> Result<Option<EnvPlan>, String> {
    let mut plan: EnvPlan = BTreeMap::new();
    let mut any = false;
    for (param_name, parameter) in &deployment.spec.parameters {
        for target in &parameter.targets {
            // Both spellings appear in the pinned fixtures:
            // `ENV.MYSQL_DATABASE` and `env.OTEL_...`.
            if !target.pointer.to_ascii_lowercase().starts_with("env.") {
                continue;
            }
            any = true;
            let var = &target.pointer["env.".len()..];
            if var.is_empty() || var.contains('=') || var.contains('\n') {
                return Err(format!(
                    "parameter {param_name:?}: env pointer {:?} is not a valid variable name",
                    target.pointer
                ));
            }
            let Some(value) = &parameter.value else {
                continue; // declared but unset: nothing to write
            };
            let raw = scalar_string(value).map_err(|kind| {
                format!("parameter {param_name:?}: {kind} value cannot target env")
            })?;
            if raw.contains('\n') {
                // The compose env_file format is line-oriented; a
                // newline would smuggle in extra variables.
                return Err(format!(
                    "parameter {param_name:?}: value contains a newline"
                ));
            }
            for service in services {
                let applies = target.components.is_empty()
                    || target.components.iter().any(|c| {
                        c == service || component.is_some_and(|comp| c == comp)
                    });
                if applies {
                    plan.entry(service.clone())
                        .or_default()
                        .insert(var.to_string(), raw.clone());
                }
            }
        }
    }
    Ok(if any { Some(plan) } else { None })
}

/// Render a scalar parameter value to its env string. Non-scalars
/// cannot be an env value.
fn scalar_string(value: &serde_yaml_ng::Value) -> Result<String, &'static str> {
    match value {
        serde_yaml_ng::Value::String(s) => Ok(s.clone()),
        serde_yaml_ng::Value::Number(n) => Ok(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Ok(b.to_string()),
        serde_yaml_ng::Value::Null => Err("null"),
        serde_yaml_ng::Value::Sequence(_) => Err("sequence"),
        serde_yaml_ng::Value::Mapping(_) => Err("mapping"),
        serde_yaml_ng::Value::Tagged(_) => Err("tagged"),
    }
}

/// Collect every `${secret:<name>}` reference in `text`.
fn collect_secret_refs(text: &str, out: &mut BTreeSet<String>) {
    let mut rest = text;
    while let Some(start) = rest.find(REF_OPEN) {
        let after = &rest[start + REF_OPEN.len()..];
        let Some(end) = after.find('}') else { break };
        out.insert(after[..end].to_string());
        rest = &after[end + 1..];
    }
}

/// Substitute every `${secret:<name>}` occurrence with its resolved
/// value (embedded references supported: `postgres://u:${secret:pw}@h`).
/// Resolved values are inserted literally, never re-scanned.
/// `Err(name)` on an unresolved reference.
fn substitute_secrets(
    text: &str,
    resolved: &BTreeMap<String, ResolvedSecret>,
) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(REF_OPEN) {
        let after = &rest[start + REF_OPEN.len()..];
        let Some(end) = after.find('}') else {
            break; // unterminated: passes through literally below
        };
        let name = &after[..end];
        let Some(secret) = resolved.get(name) else {
            return Err(name.to_string());
        };
        out.push_str(&rest[..start]);
        out.push_str(&secret.value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Write one env file atomically iff its content differs: 0600 from
/// the first byte (created with the mode, not chmod'd after),
/// temp+fsync+rename, dir fsync (Law 3; spec/reeve/10-secrets.md
/// §12.3). Returns whether the file was (re)written.
fn write_env_file(path: &Path, content: &str) -> io::Result<bool> {
    if let Ok(existing) = fs::read_to_string(path)
        && existing == content
    {
        return Ok(false); // §12.4: rewrite ONLY files whose content differs
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("env path has no parent"))?;
    fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("env path has no file name"))?;
    let tmp = dir.join(format!(".{file_name}.tmp"));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        // `mode` applies only at creation; a leftover tmp from a
        // crash must be clamped too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converge::converge;
    use crate::provider::{AppStatus, Provider, ProviderError};
    use crate::state::AgentDb;
    use axum::Json;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use reeve_types::margo::status::DeploymentState;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    const COMPOSE_YAML: &str = "\
services:
  api:
    image: example/api
    env_file: [env/api.env]
  worker:
    image: example/worker
    env_file: [env/worker.env]
";

    /// One secret embedded in a connection string targeted at the
    /// `api` SERVICE, one plain value targeted at the COMPONENT
    /// (all services), one plain value with empty components (all
    /// services).
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
  parameters:
    databaseUrl:
      value: \"postgres://app:${secret:db-password}@db:5432/app\"
      targets:
        - pointer: ENV.DATABASE_URL
          components: [\"api\"]
    logLevel:
      value: info
      targets:
        - pointer: env.LOG_LEVEL
          components: [\"web-stack\"]
    poolSize:
      value: 4
      targets:
        - pointer: ENV.POOL_SIZE
          components: []
";

    const PLAIN_DEPLOYMENT_YAML: &str = "\
apiVersion: application.margo.org/v1alpha1
kind: ApplicationDeployment
metadata:
  name: db-deploy
spec:
  applicationId: db
  deploymentProfile:
    type: docker-compose
    components:
      - name: db-stack
  parameters:
    logLevel:
      value: warn
      targets:
        - pointer: ENV.LOG_LEVEL
          components: [\"db-stack\"]
";

    struct Harness {
        _tmp: tempfile::TempDir,
        data_dir: PathBuf,
        bundle_dir: PathBuf,
        db: AgentDb,
    }

    fn harness() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let bundle_dir = tmp.path().join("bundle-src");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&bundle_dir).unwrap();
        let db = AgentDb::open(&data_dir.join("agent.db")).unwrap();
        Harness {
            data_dir,
            bundle_dir,
            db,
            _tmp: tmp,
        }
    }

    fn write_app(h: &Harness, name: &str, deployment: &str) -> PathBuf {
        let dir = h.bundle_dir.join("apps").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(COMPOSE_FILE), COMPOSE_YAML).unwrap();
        fs::write(dir.join(DEPLOYMENT_FILE), deployment).unwrap();
        dir
    }

    fn desired_app(h: &Harness, name: &str, sv: Option<&str>) -> DesiredApp {
        DesiredApp {
            name: name.to_string(),
            dir: h.bundle_dir.join("apps").join(name),
            entry: Some(AppManifestEntry {
                app_id: name.to_string(),
                deployment_id: None,
                secrets_version: sv.map(str::to_string),
            }),
        }
    }

    fn env_path(h: &Harness, app: &str, service: &str) -> PathBuf {
        h.data_dir
            .join(APPS_DIR)
            .join(app)
            .join("env")
            .join(format!("{service}.env"))
    }

    /// Shared mutable secret table + request log for the mock
    /// resolve endpoint.
    type Vault = Arc<Mutex<BTreeMap<String, (String, u64)>>>;
    type Requests = Arc<Mutex<Vec<Vec<String>>>>;

    async fn mock_resolve_server(vault: Vault, requests: Requests) -> String {
        #[derive(Clone)]
        struct S {
            vault: Vault,
            requests: Requests,
        }
        async fn resolve(
            State(s): State<S>,
            headers: HeaderMap,
            Json(req): Json<SecretsResolveRequest>,
        ) -> Json<SecretsResolveResponse> {
            assert_eq!(
                headers.get("authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer tok-dev-1"),
                "resolve must carry the device bearer token (§12.3)"
            );
            s.requests.lock().unwrap().push(req.secrets.clone());
            let vault = s.vault.lock().unwrap();
            let secrets = req
                .secrets
                .iter()
                .filter_map(|name| {
                    vault.get(name).map(|(value, version)| {
                        (
                            name.clone(),
                            ResolvedSecret {
                                value: value.clone(),
                                version: *version,
                            },
                        )
                    })
                })
                .collect();
            Json(SecretsResolveResponse { secrets })
        }
        let app = axum::Router::new()
            .route(SECRETS_RESOLVE_PATH, post(resolve))
            .with_state(S { vault, requests });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn resolver(base: &str) -> SecretResolver {
        SecretResolver::from_config(base, Some("tok-dev-1".into())).unwrap()
    }

    /// Recording fake provider (tests MUST NOT require docker).
    #[derive(Default)]
    struct FakeProvider {
        calls: Mutex<Vec<String>>,
    }
    impl FakeProvider {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl Provider for FakeProvider {
        fn apply(&self, app_dir: &Path) -> Result<AppStatus, ProviderError> {
            let name = app_dir.file_name().unwrap().to_str().unwrap();
            self.calls.lock().unwrap().push(format!("apply {name}"));
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

    fn journal_events(db: &AgentDb) -> Vec<String> {
        db.journal_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect()
    }

    #[test]
    fn plan_env_scopes_per_component_and_per_service() {
        let deployment: ApplicationDeployment =
            serde_yaml_ng::from_str(DEPLOYMENT_YAML).unwrap();
        let services = vec!["api".to_string(), "worker".to_string()];
        let plan = plan_env(&deployment, Some("web-stack"), &services)
            .unwrap()
            .unwrap();
        // Service-scoped secret only on api.
        assert_eq!(
            plan["api"]["DATABASE_URL"],
            "postgres://app:${secret:db-password}@db:5432/app"
        );
        assert!(!plan["worker"].contains_key("DATABASE_URL"));
        // Component-scoped + empty-components on every service.
        for service in ["api", "worker"] {
            assert_eq!(plan[service]["LOG_LEVEL"], "info");
            assert_eq!(plan[service]["POOL_SIZE"], "4");
        }
    }

    #[test]
    fn plan_env_none_without_env_targets_and_err_on_bad_values() {
        let no_env: ApplicationDeployment = serde_yaml_ng::from_str(
            "apiVersion: application.margo.org/v1alpha1\nkind: ApplicationDeployment\nmetadata: {name: x}\nspec:\n  applicationId: x\n  deploymentProfile: {type: docker-compose, components: [{name: s}]}\n  parameters:\n    p:\n      value: v\n      targets: [{pointer: spec.other, components: []}]\n",
        )
        .unwrap();
        assert_eq!(plan_env(&no_env, Some("s"), &["a".into()]).unwrap(), None);

        let newline: ApplicationDeployment = serde_yaml_ng::from_str(
            "apiVersion: a\nkind: k\nmetadata: {name: x}\nspec:\n  applicationId: x\n  deploymentProfile: {type: docker-compose, components: [{name: s}]}\n  parameters:\n    p:\n      value: \"a\\nb\"\n      targets: [{pointer: ENV.X, components: []}]\n",
        )
        .unwrap();
        assert!(plan_env(&newline, Some("s"), &["a".into()]).is_err());

        let non_scalar: ApplicationDeployment = serde_yaml_ng::from_str(
            "apiVersion: a\nkind: k\nmetadata: {name: x}\nspec:\n  applicationId: x\n  deploymentProfile: {type: docker-compose, components: [{name: s}]}\n  parameters:\n    p:\n      value: [1, 2]\n      targets: [{pointer: ENV.X, components: []}]\n",
        )
        .unwrap();
        assert!(plan_env(&non_scalar, Some("s"), &["a".into()]).is_err());
    }

    #[test]
    fn secret_ref_collection_and_substitution() {
        let mut refs = BTreeSet::new();
        collect_secret_refs("a ${secret:x} b ${secret:y-2} ${secret:x}", &mut refs);
        assert_eq!(
            refs,
            BTreeSet::from(["x".to_string(), "y-2".to_string()])
        );
        let resolved = BTreeMap::from([(
            "x".to_string(),
            ResolvedSecret {
                value: "V".into(),
                version: 1,
            },
        )]);
        assert_eq!(
            substitute_secrets("pre-${secret:x}-post", &resolved).unwrap(),
            "pre-V-post"
        );
        assert_eq!(
            substitute_secrets("${secret:missing}", &resolved),
            Err("missing".to_string())
        );
        // Unterminated reference passes through literally.
        assert_eq!(
            substitute_secrets("${secret:x", &resolved).unwrap(),
            "${secret:x"
        );
    }

    #[tokio::test]
    async fn materializes_env_per_service_with_0600() {
        let h = harness();
        write_app(&h, "web", DEPLOYMENT_YAML);
        let vault: Vault = Arc::new(Mutex::new(BTreeMap::from([(
            "db-password".to_string(),
            ("hunter2".to_string(), 1),
        )])));
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let base = mock_resolve_server(vault, requests.clone()).await;
        let r = resolver(&base);

        let mut desired = Desired::Known {
            apps: vec![desired_app(&h, "web", Some("sv1"))],
        };
        let reports = sync_env(&h.db, &h.data_dir, Some(&r), &mut desired).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].changed,
            vec!["env/api.env".to_string(), "env/worker.env".to_string()]
        );
        assert!(!reports[0].deferred);
        assert!(reports[0].error.is_none());

        // Per-service targeting: the secret-bearing var only on api.
        assert_eq!(
            fs::read_to_string(env_path(&h, "web", "api")).unwrap(),
            "DATABASE_URL=postgres://app:hunter2@db:5432/app\nLOG_LEVEL=info\nPOOL_SIZE=4\n"
        );
        assert_eq!(
            fs::read_to_string(env_path(&h, "web", "worker")).unwrap(),
            "LOG_LEVEL=info\nPOOL_SIZE=4\n"
        );
        // 0600 (spec/reeve/10-secrets.md §12.3).
        #[cfg(unix)]
        for service in ["api", "worker"] {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(env_path(&h, "web", service))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{service}.env mode");
        }
        // One resolve call, names only.
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            &[vec!["db-password".to_string()]]
        );
        // Journal: resolution audit (names@versions) + file names —
        // and NEVER the value.
        let entries = h.db.journal_entries().unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.event == "secrets-resolved" && e.detail.contains("db-password@1"))
        );
        assert!(
            entries.iter().any(|e| e.event == "secrets-env-updated"),
            "changed files journaled"
        );
        assert!(
            entries.iter().all(|e| !e.detail.contains("hunter2")),
            "secret value leaked into the journal"
        );
    }

    #[tokio::test]
    async fn rotation_rewrites_only_changed_files_and_reups_only_consuming_apps() {
        let h = harness();
        write_app(&h, "web", DEPLOYMENT_YAML);
        write_app(&h, "db", PLAIN_DEPLOYMENT_YAML);
        let vault: Vault = Arc::new(Mutex::new(BTreeMap::from([(
            "db-password".to_string(),
            ("hunter2".to_string(), 1),
        )])));
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let base = mock_resolve_server(vault.clone(), requests.clone()).await;
        let r = resolver(&base);
        let provider = FakeProvider::default();
        let mut db = AgentDb::open(&h.data_dir.join("agent.db")).unwrap();

        // Initial converge: both apps applied at sv1/None.
        let mut desired = Desired::Known {
            apps: vec![
                desired_app(&h, "db", None),
                desired_app(&h, "web", Some("sv1")),
            ],
        };
        sync_env(&db, &h.data_dir, Some(&r), &mut desired).await;
        converge(&mut db, &h.data_dir, &provider, &desired);
        assert_eq!(provider.calls(), vec!["apply db", "apply web"]);

        // Converged: a second pass makes no resolve call, no writes,
        // no provider calls.
        let before = requests.lock().unwrap().len();
        let mut desired = Desired::Known {
            apps: vec![
                desired_app(&h, "db", None),
                desired_app(&h, "web", Some("sv1")),
            ],
        };
        let reports = sync_env(&db, &h.data_dir, Some(&r), &mut desired).await;
        assert!(reports.is_empty(), "converged apps are silent: {reports:?}");
        assert_eq!(requests.lock().unwrap().len(), before, "no resolve when converged");
        assert!(converge(&mut db, &h.data_dir, &provider, &desired).is_empty());

        // Rotate db-password (server bumps web's secrets_version to
        // sv2; bundle digest unchanged — no re-pull, §12.4).
        vault
            .lock()
            .unwrap()
            .insert("db-password".to_string(), ("sw0rdfish".to_string(), 2));
        let api_before = fs::metadata(env_path(&h, "web", "api")).unwrap().modified().unwrap();
        let worker_before = fs::metadata(env_path(&h, "web", "worker")).unwrap().modified().unwrap();

        let mut desired = Desired::Known {
            apps: vec![
                desired_app(&h, "db", None),
                desired_app(&h, "web", Some("sv2")),
            ],
        };
        let reports = sync_env(&db, &h.data_dir, Some(&r), &mut desired).await;
        // ONLY the file whose content differs is rewritten (§12.4).
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].app_id, "web");
        assert_eq!(reports[0].changed, vec!["env/api.env".to_string()]);
        assert!(
            fs::read_to_string(env_path(&h, "web", "api"))
                .unwrap()
                .contains("sw0rdfish")
        );
        assert_ne!(
            fs::metadata(env_path(&h, "web", "api")).unwrap().modified().unwrap(),
            api_before,
            "api.env rewritten"
        );
        assert_eq!(
            fs::metadata(env_path(&h, "web", "worker")).unwrap().modified().unwrap(),
            worker_before,
            "worker.env untouched"
        );
        // Converge re-ups ONLY the consuming app.
        let acted = converge(&mut db, &h.data_dir, &provider, &desired);
        assert_eq!(acted.len(), 1);
        assert_eq!(acted[0].app_id, "web");
        assert_eq!(
            provider.calls(),
            vec!["apply db", "apply web", "apply web"]
        );
        // Staged copy the provider applies from carries the new env.
        assert!(
            fs::read_to_string(env_path(&h, "web", "api"))
                .unwrap()
                .contains("sw0rdfish")
        );
    }

    #[tokio::test]
    async fn offline_rotation_keeps_env_and_pins_secrets_version() {
        let h = harness();
        write_app(&h, "web", DEPLOYMENT_YAML);
        let vault: Vault = Arc::new(Mutex::new(BTreeMap::from([(
            "db-password".to_string(),
            ("hunter2".to_string(), 1),
        )])));
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let base = mock_resolve_server(vault.clone(), requests.clone()).await;
        let provider = FakeProvider::default();
        let mut db = AgentDb::open(&h.data_dir.join("agent.db")).unwrap();

        // Applied online at sv1.
        let r = resolver(&base);
        let mut desired = Desired::Known {
            apps: vec![desired_app(&h, "web", Some("sv1"))],
        };
        sync_env(&db, &h.data_dir, Some(&r), &mut desired).await;
        converge(&mut db, &h.data_dir, &provider, &desired);
        let api_env = fs::read_to_string(env_path(&h, "web", "api")).unwrap();

        // Rotation arrives (sv2) but the resolve endpoint is gone.
        let dead = resolver("http://127.0.0.1:1");
        let mut desired = Desired::Known {
            apps: vec![desired_app(&h, "web", Some("sv2"))],
        };
        let reports = sync_env(&db, &h.data_dir, Some(&dead), &mut desired).await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].error.is_some());
        assert!(!reports[0].deferred, "materialized app is held, not deferred");
        // Env files untouched (Law 5: last materialized state).
        assert_eq!(
            fs::read_to_string(env_path(&h, "web", "api")).unwrap(),
            api_env
        );
        // secrets_version pinned to sv1: converge does NOT record
        // sv2 as satisfied, and acts on nothing.
        let Desired::Known { apps } = &desired else { panic!() };
        assert_eq!(
            apps[0].entry.as_ref().unwrap().secrets_version.as_deref(),
            Some("sv1")
        );
        assert!(converge(&mut db, &h.data_dir, &provider, &desired).is_empty());
        assert_eq!(
            db.applied_apps().unwrap()[0].secrets_version.as_deref(),
            Some("sv1"),
            "rotation not recorded as applied while unresolved"
        );
        assert!(journal_events(&db).contains(&"secrets-resolve-unreachable".to_string()));

        // Endpoint returns with the rotated value: next cycle
        // re-resolves, rewrites, re-ups, records sv2.
        vault
            .lock()
            .unwrap()
            .insert("db-password".to_string(), ("sw0rdfish".to_string(), 2));
        let mut desired = Desired::Known {
            apps: vec![desired_app(&h, "web", Some("sv2"))],
        };
        let reports = sync_env(&db, &h.data_dir, Some(&r), &mut desired).await;
        assert_eq!(reports[0].changed, vec!["env/api.env".to_string()]);
        let acted = converge(&mut db, &h.data_dir, &provider, &desired);
        assert_eq!(acted.len(), 1);
        assert_eq!(
            db.applied_apps().unwrap()[0].secrets_version.as_deref(),
            Some("sv2")
        );
    }

    #[tokio::test]
    async fn first_apply_is_deferred_when_secrets_unresolvable() {
        let h = harness();
        write_app(&h, "web", DEPLOYMENT_YAML);
        let provider = FakeProvider::default();
        let mut db = AgentDb::open(&h.data_dir.join("agent.db")).unwrap();

        // No resolver at all (dir:// source): an app with secret refs
        // and no prior state must not start with wrong/empty config.
        let mut desired = Desired::Known {
            apps: vec![desired_app(&h, "web", Some("sv1"))],
        };
        let reports = sync_env(&db, &h.data_dir, None, &mut desired).await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].deferred);
        let Desired::Known { apps } = &desired else { panic!() };
        assert!(apps.is_empty(), "unresolvable first apply dropped from the pass");
        assert!(converge(&mut db, &h.data_dir, &provider, &desired).is_empty());
        assert!(provider.calls().is_empty());
        assert!(db.applied_apps().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_secret_is_an_error_not_a_partial_write() {
        let h = harness();
        write_app(&h, "web", DEPLOYMENT_YAML);
        // Vault has no db-password.
        let vault: Vault = Arc::new(Mutex::new(BTreeMap::new()));
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let base = mock_resolve_server(vault, requests).await;
        let r = resolver(&base);
        let db = AgentDb::open(&h.data_dir.join("agent.db")).unwrap();

        let mut desired = Desired::Known {
            apps: vec![desired_app(&h, "web", Some("sv1"))],
        };
        let reports = sync_env(&db, &h.data_dir, Some(&r), &mut desired).await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].deferred);
        assert!(!env_path(&h, "web", "api").exists(), "no partial env written");
        assert!(journal_events(&db).contains(&"secrets-missing".to_string()));
    }

    #[tokio::test]
    async fn plain_env_params_need_no_resolver() {
        // dir:// harness path: env-targeted parameters without secret
        // references materialize from local state alone.
        let h = harness();
        write_app(&h, "db", PLAIN_DEPLOYMENT_YAML);
        let db = AgentDb::open(&h.data_dir.join("agent.db")).unwrap();

        let mut desired = Desired::Known {
            apps: vec![desired_app(&h, "db", None)],
        };
        let reports = sync_env(&db, &h.data_dir, None, &mut desired).await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].error.is_none());
        assert_eq!(
            fs::read_to_string(env_path(&h, "db", "api")).unwrap(),
            "LOG_LEVEL=warn\n"
        );
        assert_eq!(
            fs::read_to_string(env_path(&h, "db", "worker")).unwrap(),
            "LOG_LEVEL=warn\n"
        );
        // Idempotent: second pass writes nothing.
        let mut desired = Desired::Known {
            apps: vec![desired_app(&h, "db", None)],
        };
        let reports = sync_env(&db, &h.data_dir, None, &mut desired).await;
        assert!(reports.is_empty(), "{reports:?}");
    }

    #[test]
    fn env_write_is_atomic_content_compared_and_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env").join("api.env");
        assert!(write_env_file(&path, "A=1\n").unwrap());
        assert!(!write_env_file(&path, "A=1\n").unwrap(), "same content: no rewrite");
        assert!(write_env_file(&path, "A=2\n").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=2\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // No tmp residue.
        let names: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["api.env"]);
    }
}
