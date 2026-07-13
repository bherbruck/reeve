//! `Provider` — the substrate-blind seam between converge and the
//! workload runtime (CLAUDE.md Substrate rules: no orchestrator
//! APIs, no cluster assumptions; compose first, systemd units
//! second, helm later/never).
//!
//! v1 ships ONE implementation, [`CommandComposeProvider`], which
//! shells out to `docker compose` v2 exactly per
//! docs/decisions/agent.md D5: boring, correct, debuggable by anyone
//! ("we run exactly what you'd type"). `up -d` and `down` are
//! idempotent, which is what makes shelling out crash-safe (Law 3:
//! converge re-runs any phase and the postcondition check is
//! docker's own).
//!
//! Command construction and `ps --format json` mapping are pure
//! functions so they are table-testable without docker
//! ([`up_args`]/[`down_args`]/[`ps_args`]/[`map_ps_json`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use reeve_types::margo::status::DeploymentState;
use serde::Deserialize;

/// A provider-level failure. One opaque message: converge maps it to
/// the D5 `failed` phase and the Margo status `error.message`; it
/// never inspects provider internals (substrate-blind).
#[derive(Debug, thiserror::Error)]
#[error("provider: {0}")]
pub struct ProviderError(pub String);

/// Tail cap for a captured compose lifecycle run — the last ~256 KiB
/// of combined `up`/`down` output. Compose output can be large (image
/// pulls, build logs); we keep the TAIL because the failure is almost
/// always at the end. Well under the server's 512 KiB accept cap
/// (spec/reeve/01-framework.md; ext-logs REV-011), so a captured run
/// always fits one upload.
pub const CAPTURE_TAIL_BYTES: usize = 256 * 1024;

/// The COMBINED stdout+stderr of one `docker compose up`/`down`
/// attempt, captured for the ext-logs extension (REV-011). This is
/// best-effort, transient debug state — it lives only in the provider's
/// interior slot until converge harvests it in the same pass, and it is
/// never load-bearing for convergence (a lost capture changes nothing;
/// Law 3 recovery re-runs the phase, not the log). The Margo one-line
/// reason still rides in [`ProviderError`] unchanged (additive).
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedRun {
    /// `true` = this was an `up` (apply); `false` = a `down` (remove).
    pub phase_up: bool,
    /// Combined stdout+stderr, tail-clipped to [`CAPTURE_TAIL_BYTES`].
    pub combined: String,
    /// Process exit code when the child ran (`None` if it could not be
    /// spawned — e.g. no docker on PATH).
    pub exit_code: Option<i32>,
    /// `true` when [`combined`](Self::combined) was clipped.
    pub truncated: bool,
    /// Whether the invocation exited zero.
    pub success: bool,
}

/// Keep the last `max` bytes of `s`, snapped to a char boundary.
/// Returns `(clipped, truncated)`.
fn tail_clip(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    (s[start..].to_string(), true)
}

/// Post-action workload status, already mapped to Margo's deployment
/// state vocabulary (`deployment-status.md` "Status Attributes";
/// reeve-types `DeploymentState`).
#[derive(Debug, Clone, PartialEq)]
pub struct AppStatus {
    pub state: DeploymentState,
    /// Free-form provider detail (journal/debug only, never parsed).
    pub detail: Option<String>,
}

/// The substrate-blind seam. One app dir = one unit of convergence
/// (docs/decisions/tree-render.md D2); the app/project name is the
/// dir's file name (D5).
///
/// Contract (Law 3): every method MUST be idempotent — re-invocation
/// when the postcondition already holds is a no-op. Implementations
/// MUST NOT keep state of their own; the runtime (docker, systemd)
/// is the only truth they consult.
pub trait Provider {
    /// Converge the workload described by `app_dir` (compose.yml +
    /// files/ + env/) into existence and return its observed status.
    fn apply(&self, app_dir: &Path) -> Result<AppStatus, ProviderError>;

    /// Tear down the workload whose LAST APPLIED definition lives in
    /// `retained_dir` (docs/decisions/agent.md D5: down before
    /// delete — you can't down a stack whose file you deleted first).
    fn remove(&self, retained_dir: &Path) -> Result<(), ProviderError>;

    /// Observe current workload status without changing anything.
    fn status(&self, app_dir: &Path) -> Result<AppStatus, ProviderError>;

    /// Per-workload container restart counts for health sampling
    /// (spec/reeve/05-health-journal.md §7.2: "per-workload container
    /// restart counts (from the active Provider)"). Sample fields are
    /// extensible/optional (§7.2), so a provider that cannot observe
    /// restarts returns `None` and the field is simply absent — the
    /// default. `docker compose ps` does not expose restart counts;
    /// a real compose implementation needs per-container `docker
    /// inspect` and can land without touching the trait.
    fn restart_counts(&self) -> Option<std::collections::BTreeMap<String, u64>> {
        None
    }

    /// Take (and clear) the combined output captured by the most recent
    /// `apply`/`remove` call — the ext-logs seam (REV-011). Converge
    /// harvests this immediately after each attempt and hands it to the
    /// ext-logs hook; a provider that captures nothing returns `None`
    /// (the default) and the extension simply has no log to store —
    /// convergence is identical either way (§3.2 degradation).
    ///
    /// Best-effort by contract: this is NOT convergence state (Law 3),
    /// so implementations keeping it in RAM is fine — a crash loses at
    /// most the last unsent log, never resumability.
    fn take_capture(&self) -> Option<CapturedRun> {
        None
    }
}

/// Compose file name inside an app dir (docs/decisions/tree-render.md
/// D2 bundle layout).
pub const COMPOSE_FILE: &str = "compose.yml";

/// `docker compose … up` argv (after the program name), exactly per
/// docs/decisions/agent.md D5:
/// `docker compose -f apps/<name>/compose.yml -p <name> up -d
/// --remove-orphans`.
pub fn up_args(compose_file: &str, project: &str) -> Vec<String> {
    vec![
        "compose".into(),
        "-f".into(),
        compose_file.into(),
        "-p".into(),
        project.into(),
        "up".into(),
        "-d".into(),
        "--remove-orphans".into(),
    ]
}

/// `docker compose … down` argv (docs/decisions/agent.md D5 removal:
/// `docker compose -p <name> down` using the retained copy's file).
pub fn down_args(compose_file: &str, project: &str) -> Vec<String> {
    vec![
        "compose".into(),
        "-f".into(),
        compose_file.into(),
        "-p".into(),
        project.into(),
        "down".into(),
    ]
}

/// `docker compose … ps --format json` argv (docs/decisions/agent.md
/// D5 post-apply status).
pub fn ps_args(compose_file: &str, project: &str) -> Vec<String> {
    vec![
        "compose".into(),
        "-f".into(),
        compose_file.into(),
        "-p".into(),
        project.into(),
        "ps".into(),
        "--format".into(),
        "json".into(),
    ]
}

/// One `docker compose ps --format json` entry — only the fields the
/// state mapping needs; everything else tolerated and ignored.
/// Compose v2 emits either one JSON array (≤ v2.20) or NDJSON, one
/// object per line (≥ v2.21); [`map_ps_json`] accepts both.
#[derive(Debug, Deserialize)]
struct PsEntry {
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "Health", default)]
    health: String,
    #[serde(rename = "ExitCode", default)]
    exit_code: i64,
}

/// Map `docker compose ps --format json` output to a Margo
/// deployment state (docs/decisions/agent.md D5: "post-apply status
/// … mapped to Margo deployment states"). Pure function; the table
/// test below is its spec.
///
/// Mapping (container state vocabulary is docker's; target
/// vocabulary is `deployment-status.md`):
/// - any `dead`, non-zero-`exited`, or `unhealthy` container =>
///   `failed`;
/// - else any `created`/`restarting`/`paused`/`removing` container
///   or `starting` health => `installing` (still converging);
/// - else (`running` healthy-or-unchecked, `exited` 0 — one-shot
///   jobs complete) => `installed`;
/// - no containers at all => `installed` (a compose file with zero
///   services is vacuously converged; genuine startup failures exit
///   through `up` or the rows above).
pub fn map_ps_json(output: &str) -> Result<DeploymentState, ProviderError> {
    let entries: Vec<PsEntry> = match serde_json::from_str::<Vec<PsEntry>>(output) {
        Ok(v) => v,
        Err(_) => output
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(serde_json::from_str::<PsEntry>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ProviderError(format!("unparseable `compose ps` json: {e}")))?,
    };
    let mut state = DeploymentState::Installed;
    for e in &entries {
        let failed = e.state == "dead"
            || e.health == "unhealthy"
            || (e.state == "exited" && e.exit_code != 0);
        let converging = matches!(e.state.as_str(), "created" | "restarting" | "paused" | "removing")
            || e.health == "starting";
        if failed {
            return Ok(DeploymentState::Failed);
        }
        if converging {
            state = DeploymentState::Installing;
        }
    }
    Ok(state)
}

/// The v1 compose provider (docs/decisions/agent.md D5): shells out
/// to `docker compose` v2 with `base_dir` as the working directory,
/// so the argv matches D5's documented commands verbatim
/// (`-f apps/<name>/compose.yml`, relative). Project name = app dir
/// name.
pub struct CommandComposeProvider {
    /// Working directory for every invocation — the agent data dir
    /// (`/var/lib/reeve-agent`), under which `apps/<name>/` and
    /// `applied/<name>/` live.
    base_dir: PathBuf,
    /// The docker program. `"docker"` (PATH lookup) in production;
    /// tests point this at a stub script that records argv and emits
    /// canned `ps` JSON (tests MUST NOT require docker).
    program: PathBuf,
    /// Interior slot holding the combined output of the most recent
    /// `up`/`down` (the ext-logs capture, REV-011). Overwritten each
    /// lifecycle run; `take_capture` drains it. NOT convergence state
    /// (see [`Provider::take_capture`]).
    last_capture: Mutex<Option<CapturedRun>>,
}

impl CommandComposeProvider {
    pub fn new(base_dir: &Path) -> Self {
        CommandComposeProvider {
            base_dir: base_dir.to_path_buf(),
            program: PathBuf::from("docker"),
            last_capture: Mutex::new(None),
        }
    }

    /// Override the docker program (test seam; also lets an operator
    /// pin an absolute path later without a new provider).
    pub fn with_program(mut self, program: &Path) -> Self {
        self.program = program.to_path_buf();
        self
    }

    /// Project name = app dir name (D5).
    fn project_name(dir: &Path) -> Result<String, ProviderError> {
        dir.file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .ok_or_else(|| ProviderError(format!("app dir {} has no utf-8 name", dir.display())))
    }

    /// `-f` value: relative to `base_dir` when the app dir is under
    /// it (the D5 documented shape), absolute otherwise.
    fn compose_file(&self, dir: &Path) -> String {
        let dir = dir.strip_prefix(&self.base_dir).unwrap_or(dir);
        dir.join(COMPOSE_FILE).display().to_string()
    }

    /// Run one docker invocation, capturing output. Non-zero exit is
    /// a [`ProviderError`] carrying stderr — the exact text an
    /// operator would have seen typing the command (D5: debuggable
    /// by anyone).
    fn run(&self, args: &[String]) -> Result<String, ProviderError> {
        let output = Command::new(&self.program)
            .args(args)
            .current_dir(&self.base_dir)
            .output()
            .map_err(|e| {
                ProviderError(format!("cannot run {}: {e}", self.program.display()))
            })?;
        if !output.status.success() {
            return Err(ProviderError(format!(
                "`docker {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run one compose LIFECYCLE invocation (`up`/`down`), recording the
    /// COMBINED stdout+stderr into [`last_capture`](Self::last_capture)
    /// for the ext-logs extension (REV-011), then returning the SAME
    /// `Result<stdout, ProviderError(stderr)>` shape `run` does — so the
    /// Margo one-line reason path is unchanged (additive). `ps` still
    /// goes through [`run`](Self::run) and never overwrites a capture,
    /// so a failed `up` leaves its own output in the slot.
    fn run_lifecycle(&self, args: &[String], phase_up: bool) -> Result<String, ProviderError> {
        match Command::new(&self.program)
            .args(args)
            .current_dir(&self.base_dir)
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                // Combine stdout then stderr (compose writes most
                // progress to stderr); ensure a line break between them.
                let mut combined = String::with_capacity(stdout.len() + stderr.len() + 1);
                combined.push_str(&stdout);
                if !stdout.is_empty() && !stdout.ends_with('\n') && !stderr.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
                let (combined, truncated) = tail_clip(&combined, CAPTURE_TAIL_BYTES);
                let success = output.status.success();
                if let Ok(mut slot) = self.last_capture.lock() {
                    *slot = Some(CapturedRun {
                        phase_up,
                        combined,
                        exit_code: output.status.code(),
                        truncated,
                        success,
                    });
                }
                if !success {
                    return Err(ProviderError(format!(
                        "`docker {}` failed ({}): {}",
                        args.join(" "),
                        output.status,
                        stderr.trim()
                    )));
                }
                Ok(stdout.into_owned())
            }
            Err(e) => {
                // Could not spawn docker at all: still record a capture
                // so the operator sees the spawn error, not silence.
                let msg = format!("cannot run {}: {e}", self.program.display());
                if let Ok(mut slot) = self.last_capture.lock() {
                    *slot = Some(CapturedRun {
                        phase_up,
                        combined: msg.clone(),
                        exit_code: None,
                        truncated: false,
                        success: false,
                    });
                }
                Err(ProviderError(msg))
            }
        }
    }
}

impl Provider for CommandComposeProvider {
    /// D5 apply: `up -d --remove-orphans`, then `ps --format json`
    /// mapped to a Margo state. `up` succeeding IS convergence
    /// success; a `ps` failure afterwards degrades to
    /// `installed`-with-detail rather than failing the apply (the
    /// action completed; observation is best-effort).
    fn apply(&self, app_dir: &Path) -> Result<AppStatus, ProviderError> {
        let project = Self::project_name(app_dir)?;
        let file = self.compose_file(app_dir);
        self.run_lifecycle(&up_args(&file, &project), true)?;
        match self.run(&ps_args(&file, &project)) {
            Ok(out) => match map_ps_json(&out) {
                Ok(state) => Ok(AppStatus { state, detail: None }),
                Err(e) => Ok(AppStatus {
                    state: DeploymentState::Installed,
                    detail: Some(format!("up succeeded; status unreadable: {e}")),
                }),
            },
            Err(e) => Ok(AppStatus {
                state: DeploymentState::Installed,
                detail: Some(format!("up succeeded; ps failed: {e}")),
            }),
        }
    }

    /// D5 removal: `down` against the retained last-applied copy.
    fn remove(&self, retained_dir: &Path) -> Result<(), ProviderError> {
        let project = Self::project_name(retained_dir)?;
        let file = self.compose_file(retained_dir);
        self.run_lifecycle(&down_args(&file, &project), false)?;
        Ok(())
    }

    fn status(&self, app_dir: &Path) -> Result<AppStatus, ProviderError> {
        let project = Self::project_name(app_dir)?;
        let file = self.compose_file(app_dir);
        let out = self.run(&ps_args(&file, &project))?;
        Ok(AppStatus {
            state: map_ps_json(&out)?,
            detail: None,
        })
    }

    fn take_capture(&self) -> Option<CapturedRun> {
        self.last_capture.lock().ok().and_then(|mut s| s.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Command construction IS D5's documented commands, verbatim.
    #[test]
    fn argv_matches_d5_exactly() {
        assert_eq!(
            up_args("apps/web/compose.yml", "web").join(" "),
            "compose -f apps/web/compose.yml -p web up -d --remove-orphans"
        );
        assert_eq!(
            down_args("applied/web/compose.yml", "web").join(" "),
            "compose -f applied/web/compose.yml -p web down"
        );
        assert_eq!(
            ps_args("apps/web/compose.yml", "web").join(" "),
            "compose -f apps/web/compose.yml -p web ps --format json"
        );
    }

    /// The ps-json -> Margo state mapping as a table. This table IS
    /// the spec for `map_ps_json`.
    #[test]
    fn ps_mapping_table() {
        use DeploymentState::*;
        let cases: &[(&str, DeploymentState)] = &[
            // NDJSON (compose >= 2.21), all running => installed
            (
                "{\"State\":\"running\",\"Health\":\"\",\"ExitCode\":0}\n\
                 {\"State\":\"running\",\"Health\":\"healthy\",\"ExitCode\":0}",
                Installed,
            ),
            // JSON array (compose <= 2.20)
            (r#"[{"State":"running","Health":"","ExitCode":0}]"#, Installed),
            // one-shot job done => installed
            (r#"[{"State":"exited","ExitCode":0}]"#, Installed),
            // non-zero exit => failed
            (r#"[{"State":"exited","ExitCode":1}]"#, Failed),
            // unhealthy dominates running
            (
                r#"[{"State":"running","Health":"unhealthy","ExitCode":0}]"#,
                Failed,
            ),
            ("{\"State\":\"dead\",\"ExitCode\":0}", Failed),
            // still converging
            (r#"[{"State":"restarting","ExitCode":0}]"#, Installing),
            (r#"[{"State":"created","ExitCode":0}]"#, Installing),
            (
                r#"[{"State":"running","Health":"starting","ExitCode":0}]"#,
                Installing,
            ),
            // failed dominates installing
            (
                "{\"State\":\"restarting\",\"ExitCode\":0}\n{\"State\":\"exited\",\"ExitCode\":2}",
                Failed,
            ),
            // no containers => vacuously installed
            ("", Installed),
            ("[]", Installed),
            // unknown extra fields tolerated
            (
                r#"[{"Name":"x","Service":"web","State":"running","ExitCode":0,"Publishers":[]}]"#,
                Installed,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(map_ps_json(input).unwrap(), *expected, "input={input:?}");
        }
        assert!(map_ps_json("not json").is_err());
    }

    /// Write a stub `docker` script that records its argv (one line
    /// per invocation) into `<dir>/argv.log` and emits
    /// `<dir>/ps.json` on `ps`. Exits `<dir>/exit-code` if present.
    fn write_stub_docker(dir: &Path) -> PathBuf {
        let path = dir.join("docker");
        fs::write(
            &path,
            "#!/bin/sh\n\
             here=$(dirname \"$0\")\n\
             echo \"$@\" >> \"$here/argv.log\"\n\
             if [ -f \"$here/exit-code\" ]; then\n\
               echo \"stub stdout before failure\"\n\
               echo \"stub docker error\" >&2\n\
               exit \"$(cat \"$here/exit-code\")\"\n\
             fi\n\
             case \" $* \" in\n\
               *\" up \"*) echo \"stub up stdout line\";;\n\
               *\" down \"*) echo \"stub down stdout line\";;\n\
               *\" ps \"*) cat \"$here/ps.json\";;\n\
             esac\n\
             exit 0\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn stub_argv(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("argv.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn compose_apply_runs_up_then_ps_and_maps_state() {
        let stub_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let docker = write_stub_docker(stub_dir.path());
        fs::write(
            stub_dir.path().join("ps.json"),
            "{\"State\":\"running\",\"Health\":\"healthy\",\"ExitCode\":0}\n",
        )
        .unwrap();
        let app_dir = data_dir.path().join("apps/web");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join(COMPOSE_FILE), "services: {}\n").unwrap();

        let provider = CommandComposeProvider::new(data_dir.path()).with_program(&docker);
        let status = provider.apply(&app_dir).unwrap();
        assert_eq!(status.state, DeploymentState::Installed);
        assert_eq!(status.detail, None);
        assert_eq!(
            stub_argv(stub_dir.path()),
            vec![
                "compose -f apps/web/compose.yml -p web up -d --remove-orphans",
                "compose -f apps/web/compose.yml -p web ps --format json",
            ]
        );
        // ext-logs (REV-011): the `up` stdout was CAPTURED, not
        // discarded, and the following `ps` did not overwrite it.
        let cap = provider.take_capture().expect("up capture");
        assert!(cap.phase_up);
        assert!(cap.success);
        assert_eq!(cap.exit_code, Some(0));
        assert!(!cap.truncated);
        assert!(
            cap.combined.contains("stub up stdout line"),
            "stdout retained in combined capture: {:?}",
            cap.combined
        );
        // Drained: a second take yields nothing.
        assert!(provider.take_capture().is_none());
    }

    #[test]
    fn capture_retains_both_streams_on_failure_and_is_tail_clipped() {
        let stub_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let docker = write_stub_docker(stub_dir.path());
        fs::write(stub_dir.path().join("exit-code"), "23").unwrap();
        let app_dir = data_dir.path().join("apps/web");
        fs::create_dir_all(&app_dir).unwrap();

        let provider = CommandComposeProvider::new(data_dir.path()).with_program(&docker);
        let err = provider.apply(&app_dir).unwrap_err();
        // Margo one-line reason is still stderr only (additive).
        assert!(err.0.contains("stub docker error"));
        // But the capture carries BOTH streams and the exit code.
        let cap = provider.take_capture().expect("failure capture");
        assert!(!cap.success);
        assert_eq!(cap.exit_code, Some(23));
        assert!(cap.combined.contains("stub stdout before failure"), "stdout");
        assert!(cap.combined.contains("stub docker error"), "stderr");

        // Tail clip keeps the last CAPTURE_TAIL_BYTES, flags truncation.
        let big = "x".repeat(CAPTURE_TAIL_BYTES + 10_000);
        let (clipped, truncated) = tail_clip(&big, CAPTURE_TAIL_BYTES);
        assert!(truncated);
        assert_eq!(clipped.len(), CAPTURE_TAIL_BYTES);
    }

    #[test]
    fn missing_docker_still_captures_the_spawn_error() {
        let data_dir = tempfile::tempdir().unwrap();
        let app_dir = data_dir.path().join("apps/web");
        fs::create_dir_all(&app_dir).unwrap();
        let provider = CommandComposeProvider::new(data_dir.path())
            .with_program(Path::new("/nonexistent/reeve-test-docker"));
        assert!(provider.apply(&app_dir).is_err());
        let cap = provider.take_capture().expect("spawn-error capture");
        assert!(!cap.success);
        assert_eq!(cap.exit_code, None);
        assert!(cap.combined.contains("cannot run"));
    }

    #[test]
    fn compose_remove_runs_down_against_retained_copy() {
        let stub_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let docker = write_stub_docker(stub_dir.path());
        let retained = data_dir.path().join("applied/web");
        fs::create_dir_all(&retained).unwrap();

        let provider = CommandComposeProvider::new(data_dir.path()).with_program(&docker);
        provider.remove(&retained).unwrap();
        assert_eq!(
            stub_argv(stub_dir.path()),
            vec!["compose -f applied/web/compose.yml -p web down"]
        );
    }

    #[test]
    fn compose_up_failure_surfaces_stderr() {
        let stub_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let docker = write_stub_docker(stub_dir.path());
        fs::write(stub_dir.path().join("exit-code"), "17").unwrap();
        let app_dir = data_dir.path().join("apps/web");
        fs::create_dir_all(&app_dir).unwrap();

        let provider = CommandComposeProvider::new(data_dir.path()).with_program(&docker);
        let err = provider.apply(&app_dir).unwrap_err();
        assert!(err.0.contains("stub docker error"), "{err}");
        assert!(err.0.contains("up"), "{err}");
    }

    #[test]
    fn compose_ps_failure_degrades_not_fails() {
        // up succeeds, ps emits garbage: apply must still succeed
        // (the action completed) with the problem in detail.
        let stub_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let docker = write_stub_docker(stub_dir.path());
        fs::write(stub_dir.path().join("ps.json"), "garbage").unwrap();
        let app_dir = data_dir.path().join("apps/web");
        fs::create_dir_all(&app_dir).unwrap();

        let provider = CommandComposeProvider::new(data_dir.path()).with_program(&docker);
        let status = provider.apply(&app_dir).unwrap();
        assert_eq!(status.state, DeploymentState::Installed);
        assert!(status.detail.unwrap().contains("status unreadable"));
    }

    #[test]
    fn missing_docker_binary_is_a_provider_error() {
        let data_dir = tempfile::tempdir().unwrap();
        let app_dir = data_dir.path().join("apps/web");
        fs::create_dir_all(&app_dir).unwrap();
        let provider = CommandComposeProvider::new(data_dir.path())
            .with_program(Path::new("/nonexistent/reeve-test-docker"));
        assert!(provider.apply(&app_dir).is_err());
    }
}
