//! Router assembly. One listening socket (spec/reeve/08-packaging.md
//! §10.2): /healthz outside auth; human routes behind the D1 identity
//! middleware; the enrollment route (device-api, D4) is authenticated
//! by the join token in its body; device delivery routes (manifest
//! poll, capabilities, native read-only /v2 pull — C4) behind
//! `device_api::device_auth` (anonymous pull disabled by default).

use std::sync::Arc;

use axum::routing::{delete, get, post, put};
use axum::{Json, Router, middleware};
use serde_json::json;

use crate::auth;
use crate::delivery;
use crate::device_tokens::SqliteDeviceTokenStore;
use crate::enroll::SqliteEnrollmentService;
use crate::join_tokens;
use crate::state::AppState;
use crate::tree;

pub fn build(state: AppState) -> Router {
    let human = Router::new()
        .route("/api/auth/login", post(auth::routes::login))
        .route("/api/auth/logout", post(auth::routes::logout))
        .route("/api/auth/setup", post(auth::routes::setup))
        .route("/api/auth/me", get(auth::routes::me))
        // Join-token management (D4): operator surface, admin/operator
        // role enforced inside the handlers.
        .route(
            "/api/join-tokens",
            post(join_tokens::create).get(join_tokens::index),
        )
        .route("/api/join-tokens/{token_hash}", delete(join_tokens::delete))
        // Tree authoring + inspection (D14): writes operator+, reads
        // viewer+, role enforced inside the handlers; ownership per
        // federation §8.2/§8.4 enforced structurally in tree.rs.
        .route("/api/tree/layers/{layer}", put(tree::put_layer))
        .route(
            "/api/tree/packages/{name}/{version}",
            put(tree::put_package),
        )
        .route("/api/tree/revisions", get(tree::list_revisions))
        .route("/api/tree/revisions/{id}", get(tree::get_revision))
        .route("/api/tree/revisions/{id}/files/{*path}", get(tree::file_at))
        .route("/api/tree/diff/{a}/{b}", get(tree::diff))
        .route("/api/tree/blame/{*path}", get(tree::blame))
        // Manual render kick (C4): re-render all devices at the current
        // head; operator+, enforced in the handler.
        .route("/api/render", post(render_kick))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::human_auth,
        ));

    // Device delivery surface (spec/reeve/08-packaging.md §10.2;
    // docs/decisions/delivery.md D7): the ONE device credential
    // authorizes the manifest poll and the /v2 pulls of exactly the
    // artifacts that manifest references (enforced in delivery.rs).
    let token_store: Arc<dyn device_api::DeviceTokenStore> =
        Arc::new(SqliteDeviceTokenStore::new(state.db.clone()));

    // Status ingest + journal backfill (C5; spec/reeve/05-health-journal.md
    // §7.3): routes live in device-api, persistence here. Same device
    // credential, own router because its state is the ingest seam.
    let ingest_svc: Arc<dyn device_api::StatusIngest> =
        Arc::new(crate::ingest::SqliteStatusIngest::new(state.db.clone()));
    let status = device_api::status::router(ingest_svc).layer(middleware::from_fn_with_state(
        token_store.clone(),
        device_api::device_auth,
    ));

    let device = Router::new()
        .route("/api/reeve/v1/manifest", get(delivery::manifest))
        .route("/api/reeve/v1/capabilities", get(delivery::capabilities))
        .route("/v2/", get(delivery::v2_root))
        .route(
            "/v2/reeve/bundles/{device_id}/manifests/{digest}",
            get(delivery::v2_manifest),
        )
        .route(
            "/v2/reeve/bundles/{device_id}/blobs/{digest}",
            get(delivery::v2_blob),
        )
        .layer(middleware::from_fn_with_state(
            token_store,
            device_api::device_auth,
        ));

    // Device-facing enrollment (D4; spec/reeve/01-framework.md §3.8
    // item 1): POST /api/reeve/v1/enroll, no device_auth layer — the
    // join token in the body is the credential.
    let enroll_svc: Arc<dyn device_api::EnrollmentService> = Arc::new(
        SqliteEnrollmentService::new(state.db.clone(), state.revisions.clone()),
    );

    Router::new()
        .merge(human)
        .merge(device)
        // Operational contract (CLAUDE.md): /healthz, no auth.
        .route("/healthz", get(healthz))
        .with_state(state)
        .merge(device_api::enroll::router(enroll_svc))
        .merge(status)
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// POST /api/render — operator+ manual kick: re-render every device at
/// the current local head (C4). No-change devices are not bumped (D3).
async fn render_kick(
    axum::extract::State(state): axum::extract::State<AppState>,
    identity: device_api::Identity,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(status) =
        crate::join_tokens::require_at_least(&state, &identity, device_api::Role::Operator)
    {
        return status.into_response();
    }
    match crate::render::render_all(&state) {
        Ok(report) => Json(json!({
            "rendered": report.rendered,
            "unchanged": report.unchanged,
            "failed": report
                .failed
                .iter()
                .map(|(d, e)| json!({ "device": d, "error": e }))
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "manual render kick failed");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
