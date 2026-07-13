//! Status reporting — wire-exact Margo `DeploymentStatusManifest`
//! per app, store-and-forward (build item B3; backfill machinery
//! proper is B7).
//!
//! Normative sources:
//! - `spec/margo/system-design/specification/margo-management-interface/deployment-status.md`:
//!   POST `/api/v1/clients/{clientId}/deployments/{deploymentId}/status`,
//!   body `DeploymentStatusManifest` — reeve ingests on Margo's path
//!   and payload shape (spec/reeve/01-framework.md §3.8 closing
//!   note); authentication is the enrollment-issued device bearer
//!   token (§3.8 item 2 replacement).
//! - spec/reeve/05-health-journal.md §7.3: records are written
//!   locally FIRST ("journaling MUST NOT depend on connectivity"),
//!   each carrying its original timestamp and a monotonic sequence
//!   number; the live report carries one additive `reeve` object
//!   (`observedAt`, `seq`). Offline => rows stay unsent and go out
//!   next time, in sequence order (Law 5).

use std::sync::{Arc, Mutex};

use reeve_types::margo::status::{
    ComponentStatus, DEPLOYMENT_STATUS_API_VERSION, DEPLOYMENT_STATUS_KIND, DeploymentStatus,
    DeploymentStatusManifest, StatusError,
};
use reeve_types::reeve::health::{HealthSample, ReeveStatusExtension};
use tracing::{info, warn};

use crate::converge::AppReport;
use crate::state::{AgentDb, Severity};

/// Build the wire-exact Margo status body for one converge outcome
/// (`deployment-status.md` request body). The `reeve` extension is
/// NOT attached here — it carries the journal row's `seq` and
/// original timestamp, which exist only after [`record_reports`]
/// persists the row; [`StatusSink::send_unsent`] attaches it.
pub fn build_status_body(report: &AppReport) -> DeploymentStatusManifest {
    let error = report.error.as_ref().map(|msg| StatusError {
        // Codes are implementation-specific (deployment-status.md);
        // reserved codes 101-103 are gateway-only.
        code: Some("reeve-converge".to_string()),
        // "the source of the status.error attribute MUST be set to
        // the name of the deployment as defined in metadata.name".
        source: Some(report.deployment_name.clone()),
        message: Some(msg.clone()),
    });
    DeploymentStatusManifest {
        api_version: DEPLOYMENT_STATUS_API_VERSION.to_string(),
        kind: DEPLOYMENT_STATUS_KIND.to_string(),
        deployment_id: report.deployment_id.clone(),
        // deviceId is required only when reporting on behalf of a
        // child device (deployment-status.md); we report for
        // ourselves — the path's {clientId} names us.
        device_id: None,
        status: DeploymentStatus {
            state: report.state,
            error,
        },
        // One entry per deployment.yaml components[] (Margo MUST).
        // Convergence acts on the whole app dir, so every component
        // shares the app's state.
        components: report
            .components
            .iter()
            .map(|name| ComponentStatus {
                name: name.clone(),
                state: report.state,
                error: None,
            })
            .collect(),
        reeve: None,
    }
}

/// Persist one status row per converge outcome — locally FIRST,
/// unconditionally (§7.3: journaling MUST NOT depend on
/// connectivity). Transmission is a separate, failable step.
pub fn record_reports(db: &AgentDb, reports: &[AppReport]) {
    for report in reports {
        let body = build_status_body(report);
        match serde_json::to_string(&body) {
            Ok(json) => {
                if let Err(e) = db.record_status(&report.app_id, &report.deployment_id, &json) {
                    warn!(app = %report.app_id, error = %e, "could not journal status report");
                }
            }
            Err(e) => warn!(app = %report.app_id, error = %e, "could not serialize status report"),
        }
    }
}

/// Where live status reports go: the Margo deployment-status
/// endpoint on the reeve server, device-token authenticated.
/// `None` for `dir://` sources (no server; rows accumulate for B7 /
/// media-based export) and for unenrolled agents (no device_id to
/// put in the path).
pub struct StatusSink {
    base: String,
    device_token: Option<String>,
    device_id: String,
    client: reqwest::Client,
    /// Latest local health sample, attached to outgoing reports as
    /// `reeve.health` (spec/reeve/05-health-journal.md §7.3 live
    /// path). Core owns the slot and reads it; the ext-health sampler
    /// (B7) is its only writer — empty, the field is simply absent,
    /// which is the whole degradation story (§3.2).
    health: SharedHealth,
}

/// Shared latest-health slot (writer: ext-health sampler; reader:
/// [`StatusSink`]). A plain `Mutex<Option<..>>` — one tiny value,
/// touched once a minute.
pub type SharedHealth = Arc<Mutex<Option<HealthSample>>>;

impl StatusSink {
    /// Construct from agent config values. HTTP(S) servers only.
    pub fn from_config(server: &str, device_token: Option<String>, device_id: Option<String>) -> Option<Self> {
        if !(server.starts_with("https://") || server.starts_with("http://")) {
            return None;
        }
        let device_id = device_id?;
        Some(StatusSink {
            base: server.trim_end_matches('/').to_string(),
            device_token,
            device_id,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("static reqwest client config"),
            health: SharedHealth::default(),
        })
    }

    /// Handle to the latest-health slot (the ext-health sampler
    /// clones this and writes samples into it).
    pub fn health_slot(&self) -> SharedHealth {
        self.health.clone()
    }

    /// The Margo route for one deployment's status
    /// (`deployment-status.md`).
    fn status_url(&self, deployment_id: &str) -> String {
        format!(
            "{}/api/v1/clients/{}/deployments/{}/status",
            self.base, self.device_id, deployment_id
        )
    }

    /// Transmit every unsent status row, oldest first (§7.3: ordered
    /// by sequence number). Store-and-forward semantics:
    /// - network unreachable => stop, keep everything unsent, retry
    ///   next cycle (Law 5 — expected operation, INFO not ERROR);
    /// - HTTP 5xx => stop, keep unsent (server sick; retry later);
    /// - HTTP 4xx => journal at ERROR and mark sent — the server
    ///   REJECTED this exact body; resending it forever would wedge
    ///   the queue behind a poison row (the journal keeps the
    ///   evidence);
    /// - 2xx => mark sent. Re-sending after a crash between POST and
    ///   mark is harmless: the server deduplicates by
    ///   `(deviceId, seq)` (§7.3).
    pub async fn send_unsent(&self, db: &AgentDb) {
        let rows = match db.unsent_statuses() {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "cannot read unsent status reports");
                return;
            }
        };
        for row in rows {
            let mut body: DeploymentStatusManifest = match serde_json::from_str(&row.body_json) {
                Ok(b) => b,
                Err(e) => {
                    // Locally corrupt row: journal + mark so it can't
                    // wedge the queue.
                    warn!(seq = row.seq, error = %e, "corrupt stored status row; dropping");
                    let _ = db.journal(
                        Severity::Error,
                        "status-row-corrupt",
                        &format!("seq {}: {e}", row.seq),
                    );
                    let _ = db.mark_status_sent(row.seq);
                    continue;
                }
            };
            // The additive reeve object (§7.3): original timestamp +
            // monotonic seq, assigned at journaling time, never
            // rewritten — plus the latest health sample when the
            // ext-health sampler has produced one.
            body.reeve = Some(ReeveStatusExtension {
                observed_at: row.ts.clone(),
                seq: row.seq as u64,
                health: self.health.lock().ok().and_then(|h| h.clone()),
            });
            let mut req = self.client.post(self.status_url(&row.deployment_id)).json(&body);
            if let Some(token) = &self.device_token {
                req = req.bearer_auth(token);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    info!(reason = %e, "status endpoint unreachable; reports held for next cycle");
                    return;
                }
            };
            let status = resp.status();
            if status.is_success() {
                if let Err(e) = db.mark_status_sent(row.seq) {
                    warn!(seq = row.seq, error = %e, "sent but could not mark; will resend (server dedupes)");
                }
            } else if status.is_server_error() {
                warn!(seq = row.seq, status = %status, "status ingest server error; reports held");
                return;
            } else {
                warn!(seq = row.seq, status = %status, "status report rejected; dropping (journaled)");
                let _ = db.journal(
                    Severity::Error,
                    "status-report-rejected",
                    &format!("seq {} app {} -> HTTP {status}", row.seq, row.app_id),
                );
                let _ = db.mark_status_sent(row.seq);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_types::margo::status::DeploymentState;

    fn report() -> AppReport {
        AppReport {
            app_id: "web".into(),
            deployment_id: "11111111-2222-3333-4444-555555555555".into(),
            deployment_name: "web-deploy".into(),
            state: DeploymentState::Installed,
            components: vec!["web-stack".into(), "db-services".into()],
            error: None,
            captured: None,
        }
    }

    /// The body must match Margo's example manifest shape
    /// (`deployment-status.md` "Example Deployment Status Manifest").
    #[test]
    fn status_body_is_wire_exact() {
        let body = build_status_body(&report());
        let json: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(json["apiVersion"], "deployment.margo.org/v1alpha1");
        assert_eq!(json["kind"], "DeploymentStatusManifest");
        assert_eq!(json["deploymentId"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(json["status"]["state"], "installed");
        assert_eq!(json["components"][0]["name"], "web-stack");
        assert_eq!(json["components"][0]["state"], "installed");
        assert_eq!(json["components"][1]["name"], "db-services");
        // Self-report: no deviceId, no reeve ext yet, no error.
        assert!(json.get("deviceId").is_none());
        assert!(json.get("reeve").is_none());
        assert!(json["status"].get("error").is_none());
    }

    #[test]
    fn failed_report_carries_error_with_deployment_name_source() {
        let mut r = report();
        r.state = DeploymentState::Failed;
        r.error = Some("provider: boom".into());
        let json = serde_json::to_value(build_status_body(&r)).unwrap();
        assert_eq!(json["status"]["state"], "failed");
        assert_eq!(json["status"]["error"]["source"], "web-deploy");
        assert_eq!(json["status"]["error"]["message"], "provider: boom");
        assert_eq!(json["components"][0]["state"], "failed");
    }

    #[test]
    fn sink_requires_http_server_and_device_id() {
        assert!(StatusSink::from_config("dir:///opt/src", None, Some("dev-1".into())).is_none());
        assert!(StatusSink::from_config("https://reeve.example", None, None).is_none());
        assert!(
            StatusSink::from_config("https://reeve.example", Some("t".into()), Some("dev-1".into()))
                .is_some()
        );
    }

    /// End-to-end store-and-forward over a stub axum server: record
    /// while "offline", then send — order preserved, reeve ext
    /// attached, path + auth exact, rows marked sent.
    #[tokio::test]
    async fn send_unsent_posts_margo_path_with_reeve_ext() {
        use axum::extract::{Json, Path as AxPath, State};
        use axum::http::HeaderMap;
        use axum::routing::post;
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<(String, String, serde_json::Value)>>>;
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));

        async fn ingest(
            State(seen): State<Seen>,
            AxPath((client_id, deployment_id)): AxPath<(String, String)>,
            headers: HeaderMap,
            Json(body): Json<serde_json::Value>,
        ) -> &'static str {
            assert_eq!(
                headers.get("authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer tok-dev-1")
            );
            seen.lock().unwrap().push((client_id, deployment_id, body));
            "ok"
        }

        let app = axum::Router::new()
            .route(
                "/api/v1/clients/{client_id}/deployments/{deployment_id}/status",
                post(ingest),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        // Two reports recorded while "offline" — rows exist, unsent.
        let mut second = report();
        second.app_id = "db".into();
        second.deployment_id = "dep-2".into();
        record_reports(&db, &[report(), second]);
        assert_eq!(db.unsent_statuses().unwrap().len(), 2);

        let sink = StatusSink::from_config(
            &format!("http://{addr}"),
            Some("tok-dev-1".into()),
            Some("dev-1".into()),
        )
        .unwrap();
        sink.send_unsent(&db).await;

        assert!(db.unsent_statuses().unwrap().is_empty(), "all marked sent");
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            // Order by seq; path carries clientId + deploymentId.
            assert_eq!(seen[0].0, "dev-1");
            assert_eq!(seen[0].1, "11111111-2222-3333-4444-555555555555");
            assert_eq!(seen[1].1, "dep-2");
            // The additive reeve object rode along (§7.3).
            let reeve = &seen[0].2["reeve"];
            assert!(reeve["seq"].as_u64().unwrap() < seen[1].2["reeve"]["seq"].as_u64().unwrap());
            assert!(reeve["observedAt"].as_str().unwrap().contains('T'));
            // No sampler has filled the slot: health absent (§3.2).
            assert!(reeve.get("health").is_none());
            // Margo fields unchanged around it.
            assert_eq!(seen[0].2["kind"], "DeploymentStatusManifest");
        } // guard dropped before the next await (clippy: await_holding_lock)

        // Idempotent: nothing left, second call sends nothing.
        sink.send_unsent(&db).await;
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// §7.3 live path: the latest health sample rides on the status
    /// report as `reeve.health` once the sampler slot is filled.
    #[tokio::test]
    async fn send_unsent_attaches_latest_health_sample() {
        use axum::extract::{Json, State};
        use axum::routing::post;
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<serde_json::Value>>>;
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route(
                "/api/v1/clients/{c}/deployments/{d}/status",
                post(|State(seen): State<Seen>, Json(body): Json<serde_json::Value>| async move {
                    seen.lock().unwrap().push(body);
                    "ok"
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        record_reports(&db, &[report()]);

        let sink =
            StatusSink::from_config(&format!("http://{addr}"), None, Some("dev-1".into())).unwrap();
        *sink.health_slot().lock().unwrap() = Some(reeve_types::reeve::health::HealthSample {
            load: Some(vec![0.5, 0.4, 0.3]),
            agent_version: Some("0.1.0".into()),
            ..Default::default()
        });
        sink.send_unsent(&db).await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let health = &seen[0]["reeve"]["health"];
        assert_eq!(health["agentVersion"], "0.1.0");
        assert_eq!(health["load"][0], 0.5);
        // Margo fields still untouched around the additive object.
        assert_eq!(seen[0]["kind"], "DeploymentStatusManifest");
    }

    #[tokio::test]
    async fn offline_sink_keeps_rows_unsent() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        record_reports(&db, &[report()]);
        // Nothing listens on this port.
        let sink = StatusSink::from_config(
            "http://127.0.0.1:1",
            Some("t".into()),
            Some("dev-1".into()),
        )
        .unwrap();
        sink.send_unsent(&db).await;
        assert_eq!(
            db.unsent_statuses().unwrap().len(),
            1,
            "offline is a held report, not a lost one (Law 5)"
        );
    }

    #[tokio::test]
    async fn rejected_report_is_journaled_and_dropped() {
        use axum::http::StatusCode;
        use axum::routing::post;

        let app = axum::Router::new().route(
            "/api/v1/clients/{c}/deployments/{d}/status",
            post(|| async { StatusCode::UNPROCESSABLE_ENTITY }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        record_reports(&db, &[report()]);
        let sink =
            StatusSink::from_config(&format!("http://{addr}"), None, Some("dev-1".into())).unwrap();
        sink.send_unsent(&db).await;
        // Poison row does not wedge the queue; evidence journaled.
        assert!(db.unsent_statuses().unwrap().is_empty());
        let events: Vec<String> = db
            .journal_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert!(events.contains(&"status-report-rejected".to_string()));
    }
}
