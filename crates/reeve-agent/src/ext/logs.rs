//! ext-logs (REV-011) — capture & upload per-deployment compose logs.
//!
//! WHY: the one-line failure reason already rides in the Margo-native
//! `DeploymentStatus.error` (report.rs builds it from
//! [`AppReport::error`]) and is left there untouched. This module ships
//! the FULL combined `docker compose up`/`down` output the compose
//! provider captured, so an operator can see WHY a deployment failed
//! beyond that one line.
//!
//! Additivity (spec/reeve/01-framework.md §3.1 rule 4): the agent
//! uploads its OWN logs to `POST /api/reeve/v1/devices/{device_id}/logs`
//! (device auth) — a NEW reeve endpoint. Nothing crosses a Margo status
//! body; no Margo path is shadowed. A vanilla WFM/agent never uploads
//! and is unaffected (§3.2 degradation).
//!
//! ## Integration seam (docs/build-charter.md CODE BOUNDARY)
//!
//! Core NEVER calls this module. The compose provider CAPTURES combined
//! output as a plain provider capability ([`crate::provider::CapturedRun`]),
//! converge harvests it onto each [`AppReport`], and the binary shell
//! (main.rs) — behind the `ext-logs` feature — calls [`record_logs`]
//! over the returned reports AFTER converge, exactly where
//! `record_reports` runs. The upload is async (reqwest); converge is
//! sync — running it post-converge keeps that boundary clean and keeps
//! a log-upload problem from ever touching a converge decision.
//!
//! ## Offline-first (Law 5) + crash-only (Law 3)
//!
//! Every acted-on app's combined output is written to a LOCAL file
//! first (`data_dir/logs/<app>.log`, atomic temp+fsync+rename) so it
//! survives offline periods and a `kill -9` until it is uploaded. The
//! upload is best-effort: unreachable / rejected / no-endpoint is a
//! journaled continue, NEVER a convergence failure. Latest capture wins
//! (the local file is overwritten; the server keeps the most-recent N
//! per deployment under its own retention, REV-011).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reeve_types::reeve::logs::{DeployLogOutcome, DeployLogPhase, DeployLogUpload};
use tracing::{info, warn};

use crate::converge::AppReport;
use crate::provider::CapturedRun;
use crate::state::{AgentDb, Severity};

/// Agent-local capture dir under the data dir: one `<app>.log` per
/// deployment, latest-wins. The durable evidence that outlives an
/// offline window (Law 5) — an operator can also read it on the box.
pub const LOGS_DIR: &str = "logs";

/// Why an upload produced nothing (mirrors ext-secrets `ResolveError`).
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// Couldn't reach the endpoint (network down, DNS, timeout). Law 5:
    /// expected operation — the local file is kept, retried next pass.
    #[error("logs endpoint unreachable: {0}")]
    Unreachable(String),
    /// Reached it but the exchange was invalid (bad status). Same
    /// keep-local path, logged at Notable.
    #[error("logs endpoint error: {0}")]
    Protocol(String),
}

/// Client for `POST /api/reeve/v1/devices/{device_id}/logs`
/// (spec/reeve/01-framework.md §3.1; REV-011) over the
/// enrollment-issued device bearer token — the device uploads only as
/// ITSELF (the server 403s a `device_id` that isn't the token's).
pub struct LogUploader {
    base: String,
    device_token: String,
    device_id: String,
    client: reqwest::Client,
}

impl LogUploader {
    /// Construct from agent config. `None` when there is nowhere to
    /// upload: `dir://` sources have no server, and an unenrolled agent
    /// has no device credential / id — logs then live only in the local
    /// file (still captured, §3.2).
    pub fn from_config(
        server: &str,
        device_token: Option<String>,
        device_id: Option<String>,
    ) -> Option<Self> {
        if !(server.starts_with("https://") || server.starts_with("http://")) {
            return None;
        }
        Some(LogUploader {
            base: server.trim_end_matches('/').to_string(),
            device_token: device_token?,
            device_id: device_id?,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("static reqwest client config"),
        })
    }

    /// The reeve upload route for THIS device (REV-011).
    fn url(&self) -> String {
        format!("{}/api/reeve/v1/devices/{}/logs", self.base, self.device_id)
    }

    /// Upload one captured run. The response id is not needed by the
    /// agent (the server assigns and retains it), so we consume only the
    /// status.
    pub async fn upload(&self, upload: &DeployLogUpload) -> Result<(), UploadError> {
        let resp = self
            .client
            .post(self.url())
            .bearer_auth(&self.device_token)
            .json(upload)
            .send()
            .await
            .map_err(|e| UploadError::Unreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(UploadError::Protocol(format!("unexpected status {status}")));
        }
        Ok(())
    }
}

/// Classify a captured run into the wire enums: phase from which
/// compose verb ran, outcome from success + phase.
fn classify(cap: &CapturedRun) -> (DeployLogOutcome, DeployLogPhase) {
    let phase = if cap.phase_up {
        DeployLogPhase::Up
    } else {
        DeployLogPhase::Down
    };
    let outcome = if !cap.success {
        DeployLogOutcome::Failed
    } else if cap.phase_up {
        DeployLogOutcome::Applied
    } else {
        DeployLogOutcome::Removed
    };
    (outcome, phase)
}

/// Build the upload body for one captured run.
fn build_upload(report: &AppReport, cap: &CapturedRun, captured_at: String) -> DeployLogUpload {
    let (outcome, phase) = classify(cap);
    DeployLogUpload {
        deployment_id: report.deployment_id.clone(),
        app_id: report.app_id.clone(),
        outcome,
        phase,
        exit_code: cap.exit_code,
        truncated: cap.truncated,
        captured_at,
        text: cap.combined.clone(),
    }
}

/// Persist one app's combined output to `data_dir/logs/<app>.log`
/// atomically (temp+fsync+rename, dir fsync — Law 3). Latest-wins:
/// overwrites any prior capture for the app. Returns the file path.
fn write_local(data_dir: &Path, app: &str, text: &str) -> std::io::Result<PathBuf> {
    let dir = data_dir.join(LOGS_DIR);
    fs::create_dir_all(&dir)?;
    // App names are already filesystem-safe (they are dir names), but
    // clamp separators defensively so a log can never escape the dir.
    let safe = app.replace(['/', '\\'], "_");
    let path = dir.join(format!("{safe}.log"));
    let tmp = dir.join(format!(".{safe}.log.tmp"));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    File::open(&dir)?.sync_all()?;
    Ok(path)
}

/// For every acted-on app that carries a captured compose run: persist
/// it locally (Law 5), then best-effort upload it (REV-011). Infallible
/// at the loop level — a log problem is journaled and swallowed, never
/// surfaced to convergence. Runs on the SAME cadence as
/// `record_reports`: converge returns only acted-on apps, so this fires
/// once per apply/remove — ALWAYS on failure, and on success too
/// (latest wins under server retention).
pub async fn record_logs(
    db: &AgentDb,
    data_dir: &Path,
    uploader: Option<&LogUploader>,
    reports: &[AppReport],
) {
    for report in reports {
        let Some(cap) = &report.captured else {
            continue; // agent-update path or a provider that captured nothing
        };
        // 1. Local first — survives offline/crash until uploaded.
        if let Err(e) = write_local(data_dir, &report.app_id, &cap.combined) {
            warn!(app = %report.app_id, error = %e, "could not write local deploy log");
        }
        // 2. Best-effort upload (never blocks/fails converge, Law 5).
        let captured_at = db.now_rfc3339().unwrap_or_default();
        let upload = build_upload(report, cap, captured_at);
        match uploader {
            None => {
                let _ = db.journal(
                    Severity::Notable,
                    "deploy-log-local-only",
                    &format!("{}: no logs endpoint (dir:// source or not enrolled)", report.app_id),
                );
            }
            Some(u) => match u.upload(&upload).await {
                Ok(()) => {
                    info!(app = %report.app_id, bytes = cap.combined.len(), truncated = cap.truncated, "deploy log uploaded");
                    let _ = db.journal(
                        Severity::Info,
                        "deploy-log-uploaded",
                        &format!("{}: {} bytes", report.app_id, cap.combined.len()),
                    );
                }
                Err(e) => {
                    // Journal + continue (Law 5): the local file is the
                    // fallback; a later pass re-captures on the next
                    // apply/remove.
                    info!(app = %report.app_id, reason = %e, "deploy log upload deferred; kept locally");
                    let _ = db.journal(
                        Severity::Notable,
                        "deploy-log-upload-deferred",
                        &format!("{}: {e}", report.app_id),
                    );
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_types::margo::status::DeploymentState;
    use std::sync::{Arc, Mutex};

    fn cap(phase_up: bool, success: bool, text: &str) -> CapturedRun {
        CapturedRun {
            phase_up,
            combined: text.to_string(),
            exit_code: Some(if success { 0 } else { 1 }),
            truncated: false,
            success,
        }
    }

    fn report_with(app: &str, deployment: &str, captured: Option<CapturedRun>) -> AppReport {
        AppReport {
            app_id: app.to_string(),
            deployment_id: deployment.to_string(),
            deployment_name: format!("{app}-deploy"),
            state: DeploymentState::Failed,
            components: vec![],
            error: Some("provider: boom".into()),
            captured,
        }
    }

    fn events(db: &AgentDb) -> Vec<String> {
        db.journal_entries().unwrap().into_iter().map(|e| e.event).collect()
    }

    #[test]
    fn classify_maps_phase_and_outcome() {
        assert_eq!(
            classify(&cap(true, true, "")),
            (DeployLogOutcome::Applied, DeployLogPhase::Up)
        );
        assert_eq!(
            classify(&cap(true, false, "")),
            (DeployLogOutcome::Failed, DeployLogPhase::Up)
        );
        assert_eq!(
            classify(&cap(false, true, "")),
            (DeployLogOutcome::Removed, DeployLogPhase::Down)
        );
        // A failed down is Failed, not Removed.
        assert_eq!(
            classify(&cap(false, false, "")),
            (DeployLogOutcome::Failed, DeployLogPhase::Down)
        );
    }

    #[test]
    fn write_local_is_atomic_and_latest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_local(dir.path(), "web", "first\n").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "first\n");
        // Overwrite: latest wins, no tmp residue.
        write_local(dir.path(), "web", "second\n").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "second\n");
        let names: Vec<String> = fs::read_dir(dir.path().join(LOGS_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["web.log"]);
    }

    fn uploader(base: &str) -> LogUploader {
        LogUploader::from_config(base, Some("tok-dev-1".into()), Some("dev-1".into())).unwrap()
    }

    #[test]
    fn from_config_requires_http_token_and_device_id() {
        assert!(LogUploader::from_config("dir:///opt", Some("t".into()), Some("d".into())).is_none());
        assert!(LogUploader::from_config("https://x", None, Some("d".into())).is_none());
        assert!(LogUploader::from_config("https://x", Some("t".into()), None).is_none());
        assert!(LogUploader::from_config("https://x", Some("t".into()), Some("d".into())).is_some());
    }

    type Seen = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

    async fn mock_logs_server(seen: Seen) -> String {
        use axum::extract::{Path as AxPath, State};
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::Json;

        async fn ingest(
            State(seen): State<Seen>,
            AxPath(device_id): AxPath<String>,
            headers: HeaderMap,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            assert_eq!(
                headers.get("authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer tok-dev-1"),
                "upload must carry the device bearer token"
            );
            seen.lock().unwrap().push((device_id, body));
            Json(serde_json::json!({ "id": "log-1" }))
        }

        let app = axum::Router::new()
            .route("/api/reeve/v1/devices/{device_id}/logs", post(ingest))
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// The upload hook posts a FAILED up-capture to the reeve device
    /// path, with the device token and the wire-exact body.
    #[tokio::test]
    async fn record_logs_posts_failure_with_right_shape() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let base = mock_logs_server(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();

        let reports = vec![report_with(
            "web",
            "web-deploy-id",
            Some(cap(true, false, "pull access denied\nError: failed\n")),
        )];
        record_logs(&db, dir.path(), Some(&uploader(&base)), &reports).await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "dev-1", "path device_id");
        let body = &seen[0].1;
        assert_eq!(body["deploymentId"], "web-deploy-id");
        assert_eq!(body["appId"], "web");
        assert_eq!(body["outcome"], "failed");
        assert_eq!(body["phase"], "up");
        assert_eq!(body["exitCode"], 1);
        assert_eq!(body["truncated"], false);
        assert!(body["capturedAt"].as_str().unwrap().contains('T'));
        assert!(body["text"].as_str().unwrap().contains("pull access denied"));
        // Local file persisted with the full combined output.
        assert!(
            fs::read_to_string(dir.path().join(LOGS_DIR).join("web.log"))
                .unwrap()
                .contains("Error: failed")
        );
        assert!(events(&db).contains(&"deploy-log-uploaded".to_string()));
    }

    /// Offline upload failure is a journaled continue: the local file is
    /// kept and nothing propagates (converge, which called this AFTER
    /// its work, is untouched).
    #[tokio::test]
    async fn offline_upload_keeps_local_file_and_does_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        // Nothing listens here.
        let dead = uploader("http://127.0.0.1:1");
        let reports = vec![report_with("web", "web-deploy-id", Some(cap(true, false, "boom\n")))];

        // Returns normally (no panic, no error type — Law 5).
        record_logs(&db, dir.path(), Some(&dead), &reports).await;

        assert_eq!(
            fs::read_to_string(dir.path().join(LOGS_DIR).join("web.log")).unwrap(),
            "boom\n",
            "offline is a held log, not a lost one"
        );
        assert!(events(&db).contains(&"deploy-log-upload-deferred".to_string()));
    }

    /// No endpoint (dir:// / unenrolled): still captured locally,
    /// journaled local-only, no upload attempted.
    #[tokio::test]
    async fn no_endpoint_captures_locally_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        let reports = vec![report_with("web", "web-deploy-id", Some(cap(false, true, "down ok\n")))];
        record_logs(&db, dir.path(), None, &reports).await;
        assert!(dir.path().join(LOGS_DIR).join("web.log").is_file());
        assert!(events(&db).contains(&"deploy-log-local-only".to_string()));
    }

    /// A report with no capture (agent-update path) is skipped entirely.
    #[tokio::test]
    async fn report_without_capture_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        let reports = vec![report_with("reeve-agent", "dep", None)];
        record_logs(&db, dir.path(), None, &reports).await;
        assert!(!dir.path().join(LOGS_DIR).exists());
        assert!(events(&db).is_empty());
    }
}
