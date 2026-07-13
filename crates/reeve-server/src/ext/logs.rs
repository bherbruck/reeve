//! ext-logs (REV-011) — per-deployment compose-log storage.
//!
//! WHY: the one-line failure reason already rides in the Margo-native
//! `DeploymentStatus.error` (reeve-types margo/status.rs) and is left
//! there untouched. This module stores the FULL `docker compose
//! up`/`down` output the agent captured so an operator can see WHY a
//! deployment failed beyond that one line.
//!
//! Additivity (spec/reeve/01-framework.md §3.1 rule 4): everything here
//! lives on NEW reeve endpoints — the agent uploads its own logs to
//! `POST /api/reeve/v1/devices/{device_id}/logs` (device auth), and
//! operators read them at `GET /api/devices/{device_id}/logs...`
//! (viewer+). Nothing crosses a Margo status body; no Margo path is
//! shadowed. A vanilla WFM/agent is unaffected (§3.2).
//!
//! ## THE seam — [`LogStore`]
//!
//! One trait, mirroring the Provider/Durability/Identity seams. The
//! default impl [`SqliteLogStore`] stores each log body as a
//! CONTENT-ADDRESSED blob in a dedicated `deploy_log_blobs` table
//! (D13 digest grammar via [`revision_store::digest_of`]) plus an index
//! row in `deploy_logs`. Retention keeps the most recent N per
//! (device, deployment), pruning older rows and garbage-collecting
//! unreferenced blobs on every insert — all in one transaction (Law 3).
//!
//! A future `LokiLogStore` implements the SAME trait — `put` pushes the
//! stream to Loki, `list`/`get` proxy Loki queries — and slots into
//! [`crate::state::AppState`] with ZERO changes to the routes below or
//! any caller. That is the whole point of the seam.
//!
//! Crash-only (Law 3): `put` is a single SQLite transaction (blob +
//! index row + prune). A kill -9 mid-upload leaves either no row or a
//! complete one; a duplicate re-upload after a crash simply adds
//! another (idempotency is not required — logs are append-only
//! forensic records, and retention bounds growth).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::Json;
use device_api::{DeviceIdentity, Identity, Role};
use reeve_types::reeve::logs::{
    DeployLogContent, DeployLogList, DeployLogMeta, DeployLogOutcome, DeployLogPhase,
    DeployLogUpload,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::Deserialize;
use tracing::warn;

use crate::db::now_secs;
use crate::state::AppState;

/// Boxed future so the trait stays dyn-compatible (mirrors
/// durability::BoxFut).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Max accepted upload body (§ device route). Larger bodies are
/// rejected at the framework layer (413) via `DefaultBodyLimit`.
pub const MAX_UPLOAD_BYTES: usize = 512 * 1024;

/// One fetched log: its listing metadata plus the raw stored body.
pub type FetchedLog = (DeployLogMeta, Vec<u8>);

// -------------------------------------------------------------- seam

/// THE deploy-log seam. A future Loki-backed store implements this and
/// drops into [`AppState`] with no route changes.
pub trait LogStore: Send + Sync {
    /// Store one uploaded log for `device_id`; returns the new log id.
    fn put(&self, device_id: &str, upload: DeployLogUpload) -> BoxFut<'_, anyhow::Result<String>>;

    /// List stored log metas for one (device, deployment), NEWEST FIRST.
    fn list(
        &self,
        device_id: &str,
        deployment_id: &str,
    ) -> BoxFut<'_, anyhow::Result<Vec<DeployLogMeta>>>;

    /// Fetch one log's meta + raw body by id, scoped to `device_id`
    /// (a log id belonging to another device reads as absent).
    fn get(
        &self,
        device_id: &str,
        log_id: &str,
    ) -> BoxFut<'_, anyhow::Result<Option<FetchedLog>>>;
}

// ----------------------------------------------------- sqlite impl

/// Default [`LogStore`]: content-addressed bodies + an index table in
/// THE shared server DB (Law 4). Retention keeps `retain_per_deployment`
/// most-recent rows per (device, deployment).
pub struct SqliteLogStore {
    db: Arc<Mutex<Connection>>,
    retain_per_deployment: u64,
}

impl SqliteLogStore {
    pub fn new(db: Arc<Mutex<Connection>>, retain_per_deployment: u64) -> Self {
        Self {
            db,
            // A zero retention would prune every row including the one
            // just inserted — clamp to at least 1 defensively (config
            // already rejects 0 at parse).
            retain_per_deployment: retain_per_deployment.max(1),
        }
    }
}

fn outcome_str(o: DeployLogOutcome) -> &'static str {
    match o {
        DeployLogOutcome::Applied => "applied",
        DeployLogOutcome::Failed => "failed",
        DeployLogOutcome::Removed => "removed",
    }
}

fn phase_str(p: DeployLogPhase) -> &'static str {
    match p {
        DeployLogPhase::Up => "up",
        DeployLogPhase::Down => "down",
    }
}

fn outcome_from_str(s: &str) -> DeployLogOutcome {
    match s {
        "failed" => DeployLogOutcome::Failed,
        "removed" => DeployLogOutcome::Removed,
        _ => DeployLogOutcome::Applied,
    }
}

fn phase_from_str(s: &str) -> DeployLogPhase {
    match s {
        "down" => DeployLogPhase::Down,
        _ => DeployLogPhase::Up,
    }
}

/// Materialize a [`DeployLogMeta`] from an index row (column order:
/// log_id, deployment_id, app_id, outcome, phase, size_bytes,
/// truncated, captured_at).
fn meta_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeployLogMeta> {
    let outcome: String = row.get(3)?;
    let phase: String = row.get(4)?;
    Ok(DeployLogMeta {
        id: row.get(0)?,
        deployment_id: row.get(1)?,
        app_id: row.get(2)?,
        outcome: outcome_from_str(&outcome),
        phase: phase_from_str(&phase),
        size_bytes: row.get::<_, i64>(5)? as u64,
        truncated: row.get::<_, i64>(6)? != 0,
        captured_at: row.get(7)?,
    })
}

const META_COLS: &str =
    "log_id, deployment_id, app_id, outcome, phase, size_bytes, truncated, captured_at";

/// Random opaque log id (16 bytes hex). Not content-addressed: two
/// identical bodies are distinct log events (different capturedAt).
fn new_log_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("getrandom");
    hex::encode(buf)
}

impl LogStore for SqliteLogStore {
    fn put(&self, device_id: &str, upload: DeployLogUpload) -> BoxFut<'_, anyhow::Result<String>> {
        let device_id = device_id.to_string();
        Box::pin(async move {
            let log_id = new_log_id();
            let digest = revision_store::digest_of(upload.text.as_bytes());
            let size_bytes = upload.text.len() as i64;
            let mut conn = self.db.lock().expect("db mutex poisoned");
            // One transaction: blob + index row + retention prune (Law 3).
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO deploy_log_blobs (digest, content) VALUES (?1, ?2)
                 ON CONFLICT(digest) DO NOTHING",
                params![digest, upload.text.as_bytes()],
            )?;
            tx.execute(
                "INSERT INTO deploy_logs
                   (log_id, device_id, deployment_id, app_id, outcome, phase,
                    exit_code, blob_digest, size_bytes, truncated, captured_at, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    log_id,
                    device_id,
                    upload.deployment_id,
                    upload.app_id,
                    outcome_str(upload.outcome),
                    phase_str(upload.phase),
                    upload.exit_code,
                    digest,
                    size_bytes,
                    upload.truncated as i64,
                    upload.captured_at,
                    now_secs(),
                ],
            )?;
            // Retention: keep the most-recent N per (device, deployment).
            // Order by received_at then log_id so ties are deterministic.
            tx.execute(
                "DELETE FROM deploy_logs
                 WHERE device_id = ?1 AND deployment_id = ?2
                   AND log_id NOT IN (
                       SELECT log_id FROM deploy_logs
                       WHERE device_id = ?1 AND deployment_id = ?2
                       ORDER BY received_at DESC, rowid DESC
                       LIMIT ?3
                   )",
                params![device_id, upload.deployment_id, self.retain_per_deployment as i64],
            )?;
            // GC blobs no longer referenced by any index row.
            tx.execute(
                "DELETE FROM deploy_log_blobs
                 WHERE digest NOT IN (SELECT blob_digest FROM deploy_logs)",
                [],
            )?;
            tx.commit()?;
            Ok(log_id)
        })
    }

    fn list(
        &self,
        device_id: &str,
        deployment_id: &str,
    ) -> BoxFut<'_, anyhow::Result<Vec<DeployLogMeta>>> {
        let device_id = device_id.to_string();
        let deployment_id = deployment_id.to_string();
        Box::pin(async move {
            let conn = self.db.lock().expect("db mutex poisoned");
            let sql = format!(
                "SELECT {META_COLS} FROM deploy_logs
                 WHERE device_id = ?1 AND deployment_id = ?2
                 ORDER BY received_at DESC, rowid DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![device_id, deployment_id], meta_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    fn get(
        &self,
        device_id: &str,
        log_id: &str,
    ) -> BoxFut<'_, anyhow::Result<Option<FetchedLog>>> {
        let device_id = device_id.to_string();
        let log_id = log_id.to_string();
        Box::pin(async move {
            let conn = self.db.lock().expect("db mutex poisoned");
            let sql = format!(
                "SELECT {META_COLS}, b.content
                 FROM deploy_logs d JOIN deploy_log_blobs b ON b.digest = d.blob_digest
                 WHERE d.device_id = ?1 AND d.log_id = ?2"
            );
            let row = conn
                .query_row(&sql, params![device_id, log_id], |row| {
                    let meta = meta_from_row(row)?;
                    let content: Vec<u8> = row.get(8)?;
                    Ok((meta, content))
                })
                .optional()?;
            Ok(row)
        })
    }
}

// --------------------------------------------------------------- routes

fn internal_error(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "deploy-logs route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// POST /api/reeve/v1/devices/{device_id}/logs (DEVICE auth).
///
/// The agent uploads its OWN captured compose output; the path
/// `device_id` MUST match the authenticated device token, else 403.
/// Body is a [`DeployLogUpload`] JSON; bodies over [`MAX_UPLOAD_BYTES`]
/// are rejected 413 by the `DefaultBodyLimit` layer on the route.
#[utoipa::path(
    post,
    path = "/api/reeve/v1/devices/{device_id}/logs",
    tag = "logs",
    params(("device_id" = String, Path, description = "The uploading device (must match the token)")),
    request_body = reeve_types::reeve::logs::DeployLogUpload,
    responses(
        (status = 200, description = "Stored; returns the new log id", body = LogIdResponse),
        (status = 401, description = "Not a device credential"),
        (status = 403, description = "device_id does not match the token"),
        (status = 413, description = "Body exceeds the accept cap"),
        (status = 422, description = "Malformed upload body", body = device_api::ErrorBody),
    ),
)]
pub async fn upload_route(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    DeviceIdentity(token_device): DeviceIdentity,
    body: Bytes,
) -> Response {
    // A device can only upload its own (device_id must match the token).
    if token_device != device_id {
        return StatusCode::FORBIDDEN.into_response();
    }
    let upload: DeployLogUpload = match serde_json::from_slice(&body) {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "error": format!("invalid deploy-log body: {e}") })),
            )
                .into_response();
        }
    };
    match state.logs.put(&device_id, upload).await {
        Ok(id) => Json(LogIdResponse { id }).into_response(),
        Err(e) => internal_error(e),
    }
}

/// `?deployment=<id>` on the human list route.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub deployment: String,
}

/// GET /api/devices/{device_id}/logs?deployment=<id> (viewer+).
/// Lists stored log metas newest-first. `deployment` is required.
#[utoipa::path(
    get,
    path = "/api/devices/{device_id}/logs",
    operation_id = "list_deploy_logs",
    tag = "logs",
    params(
        ("device_id" = String, Path, description = "Device id"),
        ("deployment" = String, Query, description = "Deployment id to list logs for"),
    ),
    responses(
        (status = 200, description = "Log metas, newest first", body = reeve_types::reeve::logs::DeployLogList),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
    ),
)]
pub async fn list_route(
    State(state): State<AppState>,
    identity: Identity,
    Path(device_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    match state.logs.list(&device_id, &q.deployment).await {
        Ok(logs) => Json(DeployLogList { logs }).into_response(),
        Err(e) => internal_error(e),
    }
}

/// GET /api/devices/{device_id}/logs/{log_id} (viewer+).
///
/// Returns the full log. Defaults to JSON `{ meta, text }`; a request
/// with `Accept: text/plain` gets the raw body as `text/plain`.
#[utoipa::path(
    get,
    path = "/api/devices/{device_id}/logs/{log_id}",
    operation_id = "get_deploy_log",
    tag = "logs",
    params(
        ("device_id" = String, Path, description = "Device id"),
        ("log_id" = String, Path, description = "Log id from the list route"),
    ),
    responses(
        (status = 200, description = "The log (JSON meta+text, or text/plain body)", body = reeve_types::reeve::logs::DeployLogContent),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
        (status = 404, description = "No such log for this device"),
    ),
)]
pub async fn get_route(
    State(state): State<AppState>,
    identity: Identity,
    Path((device_id, log_id)): Path<(String, String)>,
    headers: header::HeaderMap,
) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let (meta, bytes) = match state.logs.get(&device_id, &log_id).await {
        Ok(Some(v)) => v,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return internal_error(e),
    };
    let wants_plain = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/plain"));
    if wants_plain {
        return (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            bytes,
        )
            .into_response();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Json(DeployLogContent { meta, text }).into_response()
}

/// `POST .../logs` response body: the new log id.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct LogIdResponse {
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "on").unwrap();
        crate::db::migrate(&mut conn).unwrap();
        // A device row for the FK.
        conn.execute(
            "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at)
             VALUES ('dev-1', 'box', 'x86_64', '0.1.0', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at)
             VALUES ('dev-2', 'box', 'x86_64', '0.1.0', 0)",
            [],
        )
        .unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn upload(deployment: &str, outcome: DeployLogOutcome, text: &str) -> DeployLogUpload {
        DeployLogUpload {
            deployment_id: deployment.into(),
            app_id: "web".into(),
            outcome,
            phase: DeployLogPhase::Up,
            exit_code: if matches!(outcome, DeployLogOutcome::Failed) {
                Some(1)
            } else {
                None
            },
            truncated: false,
            captured_at: "2026-07-13T10:00:00Z".into(),
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn put_list_get_round_trip() {
        let store = SqliteLogStore::new(test_db(), 10);
        let id = store
            .put("dev-1", upload("web-deploy", DeployLogOutcome::Failed, "boom\n"))
            .await
            .unwrap();

        let metas = store.list("dev-1", "web-deploy").await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, id);
        assert_eq!(metas[0].outcome, DeployLogOutcome::Failed);
        assert_eq!(metas[0].size_bytes, 5);

        let (meta, bytes) = store.get("dev-1", &id).await.unwrap().unwrap();
        assert_eq!(meta.id, id);
        assert_eq!(bytes, b"boom\n");

        // Unknown id => None.
        assert!(store.get("dev-1", "deadbeef").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_is_device_scoped() {
        let store = SqliteLogStore::new(test_db(), 10);
        let id = store
            .put("dev-1", upload("d", DeployLogOutcome::Applied, "ok"))
            .await
            .unwrap();
        // dev-2 cannot read dev-1's log id.
        assert!(store.get("dev-2", &id).await.unwrap().is_none());
        assert!(store.get("dev-1", &id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn retention_prunes_to_n_and_gcs_blobs() {
        let db = test_db();
        let store = SqliteLogStore::new(db.clone(), 3);
        for i in 0..7 {
            store
                .put("dev-1", upload("web-deploy", DeployLogOutcome::Failed, &format!("run-{i}\n")))
                .await
                .unwrap();
        }
        let metas = store.list("dev-1", "web-deploy").await.unwrap();
        assert_eq!(metas.len(), 3, "keep only the most recent N");
        // Newest first: run-6, run-5, run-4 survive.
        let newest = store.get("dev-1", &metas[0].id).await.unwrap().unwrap().1;
        assert_eq!(newest, b"run-6\n");

        // Orphan blobs pruned: only the 3 surviving bodies remain.
        let blob_count: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM deploy_log_blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blob_count, 3);
    }

    #[tokio::test]
    async fn retention_is_per_deployment() {
        let store = SqliteLogStore::new(test_db(), 2);
        for i in 0..4 {
            store
                .put("dev-1", upload("deploy-a", DeployLogOutcome::Applied, &format!("a{i}")))
                .await
                .unwrap();
            store
                .put("dev-1", upload("deploy-b", DeployLogOutcome::Applied, &format!("b{i}")))
                .await
                .unwrap();
        }
        assert_eq!(store.list("dev-1", "deploy-a").await.unwrap().len(), 2);
        assert_eq!(store.list("dev-1", "deploy-b").await.unwrap().len(), 2);
    }
}
