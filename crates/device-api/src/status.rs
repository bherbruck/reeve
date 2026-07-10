//! Status ingest + journal backfill routes (C5).
//!
//! Two device-facing surfaces:
//!
//! - **Margo live path** — `POST /api/v1/clients/{clientId}/deployments/
//!   {deploymentId}/status`, the Margo deployment-status endpoint
//!   (`spec/margo/system-design/specification/margo-management-interface/
//!   deployment-status.md`). reeve keeps Margo's path and payload shape
//!   (spec/reeve/01-framework.md §3.8 closing paragraph); its
//!   authentication is the enrollment-issued device credential (§3.8
//!   item 2 — the replaced surface), not Margo's X.509 + RFC 9421.
//!   rev-004/1 adds one additive object under the `reeve` key
//!   (spec/reeve/05-health-journal.md §7.3) carrying `observedAt`/`seq`;
//!   a report without it is a vanilla Margo report and MUST still be
//!   accepted (spec/reeve/01-framework.md §3.2 degradation).
//!
//! - **reeve backfill path** — `POST /api/reeve/v1/journal/{deviceId}`
//!   (spec/reeve/05-health-journal.md §7.3): batches of journal records
//!   ordered by sequence number; the reply is the highest contiguously
//!   ingested sequence number. Idempotent by `(deviceId, seq)` — the
//!   server deduplicates, so resending after a crash is harmless (Law 3).
//!
//! Placement (Law 2): routes and wire handling live here; persistence
//! (journal table, current-state materialization, last-seen) lives
//! behind [`StatusIngest`], which reeve-server implements over its
//! SQLite DB. No SQLite in this crate.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use reeve_types::margo::status::{DEPLOYMENT_STATUS_KIND, DeploymentStatusManifest};
use reeve_types::reeve::health::{JournalAck, JournalBatch};

use crate::identity::DeviceIdentity;

/// Margo deployment-status route (`deployment-status.md` "Route and
/// HTTP Methods"), axum path syntax.
pub const MARGO_STATUS_ROUTE: &str =
    "/api/v1/clients/{client_id}/deployments/{deployment_id}/status";

/// reeve journal backfill route (spec/reeve/05-health-journal.md §7.3).
pub const JOURNAL_ROUTE: &str = "/api/reeve/v1/journal/{device_id}";

/// Why an ingest failed.
#[derive(Debug, thiserror::Error)]
pub enum StatusIngestError {
    /// Semantic error in the request content — Margo's 422
    /// (`deployment-status.md` response codes).
    #[error("invalid status report: {0}")]
    Invalid(String),
    /// Persistence failure — the agent retries; ingest is idempotent by
    /// `(deviceId, seq)` so a resend is harmless (§7.3, Law 3).
    #[error("status ingest failed: {0}")]
    Internal(String),
}

/// Persistence seam the routes call into. reeve-server implements this
/// over its SQLite DB (status_journal + deployment_status_current +
/// devices.last_seen_at); tests use a mock.
pub trait StatusIngest: Send + Sync {
    /// Ingest one live-path Margo `DeploymentStatusManifest`.
    ///
    /// `raw_body` is the verbatim request body: the journal stores the
    /// original bytes so unknown fields a newer agent sent survive
    /// forensically (§7.2 "fields are extensible"), not a lossy
    /// re-serialization of the parsed struct.
    fn ingest_status(
        &self,
        device_id: &str,
        deployment_id: &str,
        manifest: &DeploymentStatusManifest,
        raw_body: &str,
    ) -> Result<(), StatusIngestError>;

    /// Ingest a backfill batch; returns the highest contiguously
    /// ingested sequence number for this device (§7.3). An empty batch
    /// is a valid ack query.
    fn ingest_journal(
        &self,
        device_id: &str,
        batch: &JournalBatch,
    ) -> Result<JournalAck, StatusIngestError>;
}

/// Build the status-ingest router. The caller MUST wrap it in the
/// device-auth layer ([`crate::device_auth`]); handlers extract
/// [`DeviceIdentity`] and fail closed (401) if no middleware ran.
pub fn router(svc: Arc<dyn StatusIngest>) -> Router {
    Router::new()
        .route(MARGO_STATUS_ROUTE, post(deployment_status))
        .route(JOURNAL_ROUTE, post(journal_backfill))
        .with_state(svc)
}

/// POST /api/v1/clients/{clientId}/deployments/{deploymentId}/status —
/// Margo live path. Response codes per `deployment-status.md`: 200
/// added/updated, 400 malformed, 401 unauthenticated (middleware),
/// 403 wrong device, 422 semantic error.
pub async fn deployment_status(
    State(svc): State<Arc<dyn StatusIngest>>,
    DeviceIdentity(device_id): DeviceIdentity,
    Path((client_id, deployment_id)): Path<(String, String)>,
    body: String,
) -> Response {
    // The one device credential reports only its own status. Margo's
    // 403 is "client certificate is not trusted"; the bearer-token
    // analog is a token that does not belong to {clientId}.
    if client_id != device_id {
        return error_response(StatusCode::FORBIDDEN, "token does not match clientId");
    }

    let manifest: DeploymentStatusManifest = match serde_json::from_str(&body) {
        Ok(m) => m,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("malformed DeploymentStatusManifest: {e}"),
            );
        }
    };

    if manifest.kind != DEPLOYMENT_STATUS_KIND {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("kind must be {DEPLOYMENT_STATUS_KIND}"),
        );
    }
    if manifest.deployment_id != deployment_id {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "body deploymentId does not match path deploymentId",
        );
    }
    // Body deviceId is for reporting on behalf of a child device
    // (`deployment-status.md`); v1 has no hierarchical device ids
    // (spec/reeve/01-framework.md §3.7 known-unmodeled note), so a
    // mismatched claim is a semantic error, not silently accepted.
    if let Some(body_device) = &manifest.device_id
        && body_device != &device_id
    {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "child-device reporting is not supported (deviceId must match the caller)",
        );
    }

    match svc.ingest_status(&device_id, &deployment_id, &manifest, &body) {
        Ok(()) => (StatusCode::OK, Json(json!({}))).into_response(),
        Err(e) => ingest_error(e),
    }
}

/// POST /api/reeve/v1/journal/{deviceId} — backfill batch
/// (spec/reeve/05-health-journal.md §7.3). Replies [`JournalAck`]: the
/// highest contiguously ingested seq, which is what permits agent-side
/// journal eviction (§7.1).
pub async fn journal_backfill(
    State(svc): State<Arc<dyn StatusIngest>>,
    DeviceIdentity(device_id): DeviceIdentity,
    Path(path_device_id): Path<String>,
    Json(batch): Json<JournalBatch>,
) -> Response {
    if path_device_id != device_id {
        return error_response(StatusCode::FORBIDDEN, "token does not match deviceId");
    }
    match svc.ingest_journal(&device_id, &batch) {
        Ok(ack) => (StatusCode::OK, Json(ack)).into_response(),
        Err(e) => ingest_error(e),
    }
}

fn ingest_error(e: StatusIngestError) -> Response {
    match e {
        StatusIngestError::Invalid(msg) => error_response(StatusCode::UNPROCESSABLE_ENTITY, &msg),
        StatusIngestError::Internal(msg) => {
            tracing::error!(error = %msg, "status ingest failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "status ingest failed")
        }
    }
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware::{self, Next};
    use http_body_util::BodyExt as _;
    use std::sync::Mutex;
    use tower::ServiceExt as _;

    /// Captures what reached the persistence seam.
    #[derive(Default)]
    struct MockIngest {
        statuses: Mutex<Vec<(String, String, String)>>, // device, deployment, raw
        journals: Mutex<Vec<(String, usize)>>,          // device, record count
    }

    impl StatusIngest for MockIngest {
        fn ingest_status(
            &self,
            device_id: &str,
            deployment_id: &str,
            _manifest: &DeploymentStatusManifest,
            raw_body: &str,
        ) -> Result<(), StatusIngestError> {
            self.statuses.lock().unwrap().push((
                device_id.to_string(),
                deployment_id.to_string(),
                raw_body.to_string(),
            ));
            Ok(())
        }

        fn ingest_journal(
            &self,
            device_id: &str,
            batch: &JournalBatch,
        ) -> Result<JournalAck, StatusIngestError> {
            self.journals
                .lock()
                .unwrap()
                .push((device_id.to_string(), batch.records.len()));
            Ok(JournalAck { acked_seq: 42 })
        }
    }

    /// Test stand-in for the device-auth middleware.
    async fn fake_device_auth(mut req: Request<Body>, next: Next) -> Response {
        req.extensions_mut().insert(Identity::Device {
            device_id: "dev-1".to_string(),
        });
        next.run(req).await
    }

    fn app(svc: Arc<MockIngest>) -> Router {
        router(svc).layer(middleware::from_fn(fake_device_auth))
    }

    fn status_body(deployment_id: &str) -> String {
        serde_json::json!({
            "apiVersion": "deployment.margo.org/v1alpha1",
            "kind": "DeploymentStatusManifest",
            "deploymentId": deployment_id,
            "status": { "state": "installed" },
            "components": [{ "name": "web", "state": "installed" }],
            "reeve": { "observedAt": "2026-07-10T06:12:03Z", "seq": 7 }
        })
        .to_string()
    }

    fn post(uri: &str, body: String) -> Request<Body> {
        Request::post(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn live_path_happy() {
        let svc = Arc::new(MockIngest::default());
        let res = app(svc.clone())
            .oneshot(post(
                "/api/v1/clients/dev-1/deployments/dep-1/status",
                status_body("dep-1"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let calls = svc.statuses.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "dev-1");
        assert_eq!(calls[0].1, "dep-1");
        assert!(calls[0].2.contains("\"seq\":7"), "raw body reaches the seam");
    }

    #[tokio::test]
    async fn wrong_client_id_is_403() {
        let svc = Arc::new(MockIngest::default());
        let res = app(svc.clone())
            .oneshot(post(
                "/api/v1/clients/dev-OTHER/deployments/dep-1/status",
                status_body("dep-1"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(svc.statuses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deployment_id_mismatch_is_422() {
        let svc = Arc::new(MockIngest::default());
        let res = app(svc)
            .oneshot(post(
                "/api/v1/clients/dev-1/deployments/dep-1/status",
                status_body("dep-OTHER"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn wrong_kind_is_422() {
        let svc = Arc::new(MockIngest::default());
        let body = status_body("dep-1").replace("DeploymentStatusManifest", "SomethingElse");
        let res = app(svc)
            .oneshot(post("/api/v1/clients/dev-1/deployments/dep-1/status", body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn child_device_claim_is_422() {
        let svc = Arc::new(MockIngest::default());
        let mut v: serde_json::Value = serde_json::from_str(&status_body("dep-1")).unwrap();
        v["deviceId"] = serde_json::json!("child-9");
        let res = app(svc)
            .oneshot(post(
                "/api/v1/clients/dev-1/deployments/dep-1/status",
                v.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn malformed_body_is_400() {
        let svc = Arc::new(MockIngest::default());
        let res = app(svc)
            .oneshot(post(
                "/api/v1/clients/dev-1/deployments/dep-1/status",
                "{not json".to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn vanilla_margo_report_without_reeve_key_is_accepted() {
        // Degradation (spec/reeve/01-framework.md §3.2): absence of the
        // extension MUST NOT fail status ingest.
        let svc = Arc::new(MockIngest::default());
        let body = serde_json::json!({
            "apiVersion": "deployment.margo.org/v1alpha1",
            "kind": "DeploymentStatusManifest",
            "deploymentId": "dep-1",
            "status": { "state": "pending" },
            "components": []
        })
        .to_string();
        let res = app(svc)
            .oneshot(post("/api/v1/clients/dev-1/deployments/dep-1/status", body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn journal_backfill_acks() {
        let svc = Arc::new(MockIngest::default());
        let body = serde_json::json!({
            "records": [
                { "seq": 1, "observedAt": "2026-07-10T06:00:00Z", "kind": "lifecycle",
                  "payload": { "event": "start" } },
                { "seq": 2, "observedAt": "2026-07-10T06:01:00Z", "kind": "gap" }
            ]
        })
        .to_string();
        let res = app(svc.clone())
            .oneshot(post("/api/reeve/v1/journal/dev-1", body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let ack: JournalAck = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(ack.acked_seq, 42);
        assert_eq!(*svc.journals.lock().unwrap(), vec![("dev-1".to_string(), 2)]);
    }

    #[tokio::test]
    async fn journal_wrong_device_is_403() {
        let svc = Arc::new(MockIngest::default());
        let res = app(svc.clone())
            .oneshot(post(
                "/api/reeve/v1/journal/dev-OTHER",
                serde_json::json!({ "records": [] }).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(svc.journals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_auth_middleware_fails_closed() {
        let svc = Arc::new(MockIngest::default());
        let res = router(svc) // no auth layer at all
            .oneshot(post(
                "/api/v1/clients/dev-1/deployments/dep-1/status",
                status_body("dep-1"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn internal_error_is_500_without_detail() {
        struct FailIngest;
        impl StatusIngest for FailIngest {
            fn ingest_status(
                &self,
                _: &str,
                _: &str,
                _: &DeploymentStatusManifest,
                _: &str,
            ) -> Result<(), StatusIngestError> {
                Err(StatusIngestError::Internal("db down: secret".into()))
            }
            fn ingest_journal(
                &self,
                _: &str,
                _: &JournalBatch,
            ) -> Result<JournalAck, StatusIngestError> {
                Err(StatusIngestError::Internal("db down: secret".into()))
            }
        }
        let app = router(Arc::new(FailIngest)).layer(middleware::from_fn(fake_device_auth));
        let res = app
            .oneshot(post(
                "/api/v1/clients/dev-1/deployments/dep-1/status",
                status_body("dep-1"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
    }
}
