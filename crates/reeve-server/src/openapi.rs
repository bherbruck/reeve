//! The D10 API document (docs/decisions/ui.md): every annotated route
//! assembled into ONE OpenAPI document. The Rust types ARE the source
//! of truth — `reeve-server openapi` prints this document, `just
//! gen-api` feeds it to orval, and ui/src/api/ is generated from it
//! (never hand-written).
//!
//! Assembly: one `OpenApi` derive per feature boundary (the CODE
//! BOUNDARY rule — core never references ext items), merged under the
//! same `cfg` gates router.rs uses, so the document always describes
//! exactly the surface this binary serves. Output is deterministic
//! for a given feature set: utoipa preserves declaration order and
//! `serde_json::to_string_pretty` is stable.
//!
//! SSE event payloads (spec/reeve/04-status-stream.md §6.3) are
//! registered as components even though no REST path returns them —
//! D10: "event payload schemas are registered as OpenAPI components,
//! so the UI's invalidation handlers consume generated types too."

use utoipa::OpenApi;

/// Core surface: compiled into every binary, feature-independent.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "reeve",
        description = "Fleet desired-state manager (Margo-inspired). Human API consumed \
                       by the reeve UI, plus the device-facing wire surface.",
        version = env!("CARGO_PKG_VERSION"),
    ),
    paths(
        // auth (docs/decisions/auth.md D1)
        crate::auth::routes::login,
        crate::auth::routes::logout,
        crate::auth::routes::setup,
        crate::auth::routes::me,
        // join tokens (docs/decisions/agent.md D4)
        crate::join_tokens::create,
        crate::join_tokens::index,
        crate::join_tokens::delete,
        // tree authoring + inspection (D14)
        crate::tree::put_layer,
        crate::tree::put_package,
        crate::tree::list_revisions,
        crate::tree::get_revision,
        crate::tree::file_at,
        crate::tree::diff,
        crate::tree::blame,
        // deploy-to-scope + history/undo (REV-010 §11.4/§11.5)
        crate::deploy::deploy,
        crate::deploy::undeploy,
        crate::history::list,
        crate::history::detail,
        crate::history::undo,
        // devices (Track D + REV-010 §11.3 management writes)
        crate::devices::list,
        crate::devices::detail,
        crate::devices::patch,
        crate::devices::decommission,
        crate::devices::journal,
        // location groups (REV-010 fleet->site containment §11.1/§11.3)
        crate::groups::list,
        crate::groups::create,
        crate::groups::rename,
        crate::groups::delete,
        // render kick (C4) + durability (C6) + healthz
        crate::router::render_kick,
        crate::durability::status_route,
        crate::router::healthz,
        crate::router::server_info,
        // device-facing wire surface (part of the served contract)
        device_api::enroll::enroll_route,
        device_api::status::deployment_status,
        device_api::status::journal_backfill,
        crate::delivery::manifest,
        crate::delivery::capabilities,
    ),
    components(schemas(
        device_api::ErrorBody,
        // SSE event payloads (rev-003/1 table, §6.3) — D10 components.
        reeve_types::reeve::events::ResetEvent,
        reeve_types::reeve::events::DevicePresenceEvent,
        reeve_types::reeve::events::DeploymentStatusEvent,
        reeve_types::reeve::events::TerminalSessionEvent,
        reeve_types::reeve::events::HealthStateEvent,
        reeve_types::reeve::events::VerifyRestoreEvent,
        reeve_types::reeve::events::DurabilityLagEvent,
        reeve_types::reeve::events::RolloutEvent,
        reeve_types::reeve::events::SecretRotationEvent,
        reeve_types::reeve::events::FederationSyncEvent,
    )),
    tags(
        (name = "auth", description = "Login, logout, first-boot setup, whoami"),
        (name = "join-tokens", description = "Device enrollment join tokens (operator surface)"),
        (name = "tree", description = "Layered deployment tree: authoring, history, diff, blame, render"),
        (name = "deploy", description = "Deploy/undeploy a stack to a scope (spec/reeve/11-fleet-model.md §11.4)"),
        (name = "history", description = "Change history and Undo (spec/reeve/11-fleet-model.md §11.5)"),
        (name = "devices", description = "Device fleet: presence, deployment states, render provenance, journal"),
        (name = "groups", description = "Location groups: the fleet->site containment tree (spec/reeve/11-fleet-model.md §11.1)"),
        (name = "durability", description = "Durability tier status (spec/reeve/07-durability.md)"),
        (name = "secrets", description = "Write-only secrets vault metadata (spec/reeve/10-secrets.md)"),
        (name = "rollouts", description = "Staged rollouts (spec/reeve/09-rollouts.md)"),
        (name = "federation", description = "Tier tokens and federation sync status (spec/reeve/06-federation.md)"),
        (name = "events", description = "Live status stream, SSE (spec/reeve/04-status-stream.md)"),
        (name = "logs", description = "Per-deployment compose logs (REV-011 ext-logs)"),
        (name = "terminal", description = "Remote terminal bridge (spec/reeve/03-terminal.md)"),
        (name = "device", description = "Device-facing wire surface (enroll, manifest poll, status ingest)"),
        (name = "ops", description = "Operational endpoints"),
    ),
)]
struct CoreApi;

#[cfg(feature = "ext-secrets")]
#[derive(OpenApi)]
#[openapi(paths(
    crate::ext::secrets::put_route,
    crate::ext::secrets::list_route,
    crate::ext::secrets::delete_route,
    crate::ext::secrets::resolve_route,
))]
struct SecretsApi;

#[cfg(feature = "ext-rollouts")]
#[derive(OpenApi)]
#[openapi(paths(
    crate::ext::rollouts::create_route,
    crate::ext::rollouts::list_route,
    crate::ext::rollouts::status_route,
    crate::ext::rollouts::pause_route,
    crate::ext::rollouts::resume_route,
    crate::ext::rollouts::abort_route,
    crate::ext::rollouts::rollback_route,
))]
struct RolloutsApi;

#[cfg(feature = "ext-federation")]
#[derive(OpenApi)]
#[openapi(paths(
    crate::ext::federation::create_token_route,
    crate::ext::federation::list_tokens_route,
    crate::ext::federation::revoke_token_route,
    crate::ext::federation::status_route,
))]
struct FederationApi;

#[cfg(feature = "ext-logs")]
#[derive(OpenApi)]
#[openapi(paths(
    crate::ext::logs::upload_route,
    crate::ext::logs::list_route,
    crate::ext::logs::get_route,
))]
struct LogsApi;

#[cfg(feature = "ext-sse")]
#[derive(OpenApi)]
#[openapi(paths(crate::ext::sse::events_route))]
struct SseApi;

#[cfg(feature = "ext-terminal")]
#[derive(OpenApi)]
#[openapi(paths(crate::ext::terminal::terminal_route))]
struct TerminalApi;

/// The complete document for THIS binary's feature set.
pub fn doc() -> utoipa::openapi::OpenApi {
    // mut is unused only in a --no-default-features (all-ext-off) build.
    #[allow(unused_mut)]
    let mut doc = CoreApi::openapi();
    #[cfg(feature = "ext-secrets")]
    doc.merge(SecretsApi::openapi());
    #[cfg(feature = "ext-rollouts")]
    doc.merge(RolloutsApi::openapi());
    #[cfg(feature = "ext-federation")]
    doc.merge(FederationApi::openapi());
    #[cfg(feature = "ext-logs")]
    doc.merge(LogsApi::openapi());
    #[cfg(feature = "ext-sse")]
    doc.merge(SseApi::openapi());
    #[cfg(feature = "ext-terminal")]
    doc.merge(TerminalApi::openapi());
    doc
}

/// Pretty JSON with a trailing newline — the exact bytes `reeve-server
/// openapi` prints and `just gen-api` writes to ui/openapi.json (the
/// drift check diffs these bytes, so they must be deterministic).
pub fn json() -> String {
    let mut out = serde_json::to_string_pretty(&doc()).expect("openapi doc serializes");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_is_deterministic() {
        assert_eq!(json(), json());
    }

    #[test]
    fn document_covers_the_served_surface() {
        let doc: serde_json::Value = serde_json::from_str(&json()).unwrap();
        let paths = doc["paths"].as_object().unwrap();
        for p in [
            "/api/auth/login",
            "/api/auth/me",
            "/api/join-tokens",
            "/api/join-tokens/{token_hash}",
            "/api/tree/layers/{layer}",
            "/api/tree/revisions",
            "/api/tree/diff/{a}/{b}",
            "/api/devices",
            "/api/devices/{device_id}",
            "/api/devices/{device_id}/journal",
            "/api/groups",
            "/api/groups/{id}",
            "/api/render",
            "/api/durability/status",
            "/api/reeve/v1/enroll",
            "/api/reeve/v1/manifest",
            #[cfg(feature = "ext-secrets")]
            "/api/secrets",
            #[cfg(feature = "ext-rollouts")]
            "/api/rollouts",
            #[cfg(feature = "ext-federation")]
            "/api/tier-tokens",
            #[cfg(feature = "ext-sse")]
            "/api/reeve/v1/events",
            #[cfg(feature = "ext-terminal")]
            "/api/reeve/v1/terminal/{device_id}",
            #[cfg(feature = "ext-logs")]
            "/api/reeve/v1/devices/{device_id}/logs",
            #[cfg(feature = "ext-logs")]
            "/api/devices/{device_id}/logs/{log_id}",
        ] {
            assert!(paths.contains_key(p), "missing path {p}");
        }
        // D10: SSE payloads are components even though no path returns
        // them.
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        for s in [
            "ResetEvent",
            "DevicePresenceEvent",
            "DeploymentStatusEvent",
            "HealthStateEvent",
            "VerifyRestoreEvent",
            "DurabilityLagEvent",
            "RolloutEvent",
            "SecretRotationEvent",
            "FederationSyncEvent",
            "TerminalSessionEvent",
        ] {
            assert!(schemas.contains_key(s), "missing schema {s}");
        }
    }

    #[test]
    fn document_has_info_and_valid_openapi_version() {
        let doc: serde_json::Value = serde_json::from_str(&json()).unwrap();
        assert!(
            doc["openapi"].as_str().unwrap().starts_with("3."),
            "openapi version"
        );
        assert_eq!(doc["info"]["title"], "reeve");
    }
}
