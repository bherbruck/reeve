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
        // Durability status (C6, spec/reeve/07-durability.md §9.4:
        // "last verified restore" + degraded flag MUST be surfaced in
        // the API); viewer+, enforced in the handler.
        .route(
            "/api/durability/status",
            get(crate::durability::status_route),
        );
    // Secrets operator surface (C7, spec/reeve/10-secrets.md §12.2):
    // write-only — set/rotate (PUT, operator+), delete (operator+),
    // metadata list (viewer+). No value read-back route exists.
    #[cfg(feature = "ext-secrets")]
    let human = human
        .route(
            "/api/secrets",
            put(crate::ext::secrets::put_route).get(crate::ext::secrets::list_route),
        )
        .route(
            "/api/secrets/{scope}/{name}",
            delete(crate::ext::secrets::delete_route),
        );
    // Staged rollouts (C9, spec/reeve/09-rollouts.md §11.6/§11.8):
    // create/pause/resume/abort operator+, list/status viewer+ — all
    // enforced (and audited) inside the handlers.
    #[cfg(feature = "ext-rollouts")]
    let human = human
        .route(
            "/api/rollouts",
            post(crate::ext::rollouts::create_route).get(crate::ext::rollouts::list_route),
        )
        .route(
            "/api/rollouts/{rollout_id}",
            get(crate::ext::rollouts::status_route),
        )
        .route(
            "/api/rollouts/{rollout_id}/pause",
            post(crate::ext::rollouts::pause_route),
        )
        .route(
            "/api/rollouts/{rollout_id}/resume",
            post(crate::ext::rollouts::resume_route),
        )
        .route(
            "/api/rollouts/{rollout_id}/abort",
            post(crate::ext::rollouts::abort_route),
        );
    // Federation operator surface (C10, spec/reeve/06-federation.md
    // §8.7): tier-token create (admin) / list (viewer+) / revoke
    // (admin) — roles enforced in the handlers — plus the queryable
    // sync status (§8.2).
    #[cfg(feature = "ext-federation")]
    let human = human
        .route(
            "/api/tier-tokens",
            post(crate::ext::federation::create_token_route)
                .get(crate::ext::federation::list_tokens_route),
        )
        .route(
            "/api/tier-tokens/{name}",
            delete(crate::ext::federation::revoke_token_route),
        )
        .route(
            "/api/federation/status",
            get(crate::ext::federation::status_route),
        );
    // Live status stream (C8, spec/reeve/04-status-stream.md §6.1):
    // SSE, viewer+ (enforced in the handler), never unauthenticated —
    // inside the human_auth layer like every other human read.
    #[cfg(feature = "ext-sse")]
    let human = human.route(
        "/api/reeve/v1/events",
        get(crate::ext::sse::events_route),
    );
    // Remote terminal UI leg (C8, spec/reeve/03-terminal.md §5.1):
    // the one genuinely bidirectional UI websocket. Operator+ and
    // password/proxy-mode-only, enforced (and audited) in the handler.
    #[cfg(feature = "ext-terminal")]
    let human = human.route(
        "/api/reeve/v1/terminal/{device_id}",
        get(crate::ext::terminal::terminal_route),
    );
    let human = human.layer(middleware::from_fn_with_state(
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
    let ingest_svc: Arc<dyn device_api::StatusIngest> = Arc::new(
        crate::ingest::SqliteStatusIngest::new(state.db.clone(), state.events.clone()),
    );
    let status = device_api::status::router(ingest_svc).layer(middleware::from_fn_with_state(
        token_store.clone(),
        device_api::device_auth,
    ));

    let device = Router::new()
        .route("/api/reeve/v1/manifest", get(delivery::manifest))
        .route("/api/reeve/v1/capabilities", get(delivery::capabilities))
        .route("/v2/", get(delivery::v2_root));
    // Persistent agent channel (C8, spec/reeve/02-channel.md §4.1):
    // websocket upgrade behind the SAME device credential — unknown/
    // unauthenticated clients are rejected by device_auth BEFORE the
    // upgrade completes.
    #[cfg(feature = "ext-channel")]
    let device = device.route(
        reeve_types::reeve::channel::CHANNEL_PATH,
        get(crate::ext::channel::channel_route),
    );
    let device = device
        .route(
            "/v2/reeve/bundles/{device_id}/manifests/{digest}",
            get(delivery::v2_manifest),
        )
        .route(
            "/v2/reeve/bundles/{device_id}/blobs/{digest}",
            get(delivery::v2_blob),
        )
        // C11 image proxy (docs/decisions/delivery.md D8): the
        // catch-all leg of the ONE /v2 space — non-`reeve/*` repos
        // reverse-proxy to the zot sidecar (pull only; push verbs
        // 405 inside the handler). Behind the SAME device credential:
        // the proxy terminates device auth and injects its own Basic
        // credential toward zot (zot_proxy.rs).
        .route(
            "/v2/{*rest}",
            axum::routing::any(crate::zot_proxy::proxy_route),
        );
    // Secrets resolve endpoint (C7, spec/reeve/10-secrets.md §12.3):
    // the single plaintext egress, behind the same device credential —
    // a device can only ask as itself.
    #[cfg(feature = "ext-secrets")]
    let device = device.route(
        reeve_types::reeve::secrets::SECRETS_RESOLVE_PATH,
        post(crate::ext::secrets::resolve_route),
    );
    let device = device.layer(middleware::from_fn_with_state(
        token_store,
        device_api::device_auth,
    ));

    // Device-facing enrollment (D4; spec/reeve/01-framework.md §3.8
    // item 1): POST /api/reeve/v1/enroll, no device_auth layer — the
    // join token in the body is the credential.
    let enroll_svc: Arc<dyn device_api::EnrollmentService> = Arc::new(
        SqliteEnrollmentService::new(state.db.clone(), state.revisions.clone()),
    );

    // Tier-to-tier sync serving (C10, spec/reeve/06-federation.md §8.2
    // parent side): what a CHILD gateway calls with its tier credential
    // (the TierIdentity extractor authenticates and scopes every
    // handler, §8.7). Not device- or human-auth'd — a tier token is its
    // own principal. Agents never touch these routes (§8.6).
    #[cfg(feature = "ext-federation")]
    let tier = {
        let tier = Router::new()
            .route(
                "/api/reeve/v1/sync/head",
                get(crate::ext::federation::sync_head_route),
            )
            .route(
                "/api/reeve/v1/sync/revisions",
                get(crate::ext::federation::sync_revisions_route),
            )
            .route(
                "/api/reeve/v1/sync/blobs/{digest}",
                get(crate::ext::federation::sync_blob_route),
            )
            .route(
                "/api/reeve/v1/sync/journal/{device_id}",
                post(crate::ext::federation::sync_journal_route),
            );
        // Scoped secret sync (10-secrets §12.5) needs the vault.
        #[cfg(feature = "ext-secrets")]
        let tier = tier.route(
            "/api/reeve/v1/sync/secrets",
            get(crate::ext::federation::sync_secrets_route),
        );
        tier
    };

    let app = Router::new().merge(human).merge(device);
    #[cfg(feature = "ext-federation")]
    let app = app.merge(tier);
    app
        // Operational contract (CLAUDE.md): /healthz, no auth.
        .route("/healthz", get(healthz))
        // Embedded API document (C12, spec/reeve/08-packaging.md
        // §10.1: openapi.json "served at a stable path"). 404 until
        // Track D generates one (embed-if-present, build.rs). Shape,
        // not values — outside auth like /healthz.
        .route("/api/openapi.json", get(crate::assets::openapi))
        .with_state(state)
        .merge(device_api::enroll::router(enroll_svc))
        .merge(status)
        // Embedded UI (C12/§10.1, CLAUDE.md "ui/"): asset by path,
        // index.html for SPA deep links; /api/* and /v2/* misses stay
        // real 404s. Pre-Track-D builds embed nothing => 404 (same as
        // before this fallback existed).
        .fallback(crate::assets::spa_fallback)
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
