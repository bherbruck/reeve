//! Device-facing desired-state delivery (C4):
//! - `GET /api/reeve/v1/manifest` — the device's State Manifest,
//!   conditional GET with ETag = manifest digest (`sha256:<hex>`, an
//!   RFC 9110 strong validator; spec/reeve/08-packaging.md §10.2).
//! - `GET /api/reeve/v1/capabilities` — server extension advertisement
//!   (spec/reeve/01-framework.md §3.3).
//! - `/v2/…` — native READ-ONLY OCI distribution pull for the server's
//!   own artifacts (docs/decisions/delivery.md D7): GET manifest / GET
//!   blob by digest. No push routes exist, ever.
//!
//! Auth (§10.2): every route here sits behind
//! `device_api::device_auth`; the one device credential authorizes a
//! device to poll exactly ITS OWN manifest and pull exactly the
//! artifacts its manifest references. Cross-device requests are 404
//! (not 403): the confidentiality boundary does not confirm existence
//! (§10.7).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use device_api::DeviceIdentity;
use reeve_types::reeve::capabilities::ServerCapabilities;
use rusqlite::{OptionalExtension as _, params};
use serde_json::json;
use tracing::warn;

use crate::render;
use crate::state::AppState;

/// Server capability advertisement (spec/reeve/01-framework.md §3.3):
/// extension list + server version, derived from compiled-in cargo
/// features. Each `ext-<name>` feature's module appends its
/// `rev-NNN/V` entry here under its `cfg!` gate, e.g.:
///
/// ```ignore
/// if cfg!(feature = "ext-health-journal") {
///     extensions.push(format_extension(rev::HEALTH_JOURNAL, 1));
/// }
/// ```
///
/// We advertise only what is actually compiled in (§3.3: a capability
/// advertised must be usable; a core --no-default-features build
/// advertises nothing and every agent degrades to pure Margo behavior).
pub fn server_capabilities() -> ServerCapabilities {
    use reeve_types::reeve::capabilities::{format_extension, rev};
    #[allow(unused_mut)]
    let mut extensions: Vec<String> = Vec::new();
    // rev-001/1 Persistent Agent Channel (spec/reeve/02-channel.md
    // §4.1: the agent MUST NOT attempt the channel unless this is
    // advertised; C8).
    if cfg!(feature = "ext-channel") {
        extensions.push(format_extension(rev::CHANNEL, 1));
    }
    // rev-002/1 Remote Terminal (spec/reeve/03-terminal.md §5, C8).
    if cfg!(feature = "ext-terminal") {
        extensions.push(format_extension(rev::TERMINAL, 1));
    }
    // rev-003/1 Live Status Stream (spec/reeve/04-status-stream.md
    // §6, C8). Consumed by UI clients, not agents — advertised for
    // completeness of the §3.3 extension index.
    if cfg!(feature = "ext-sse") {
        extensions.push(format_extension(rev::STATUS_STREAM, 1));
    }
    // rev-004/1 Health & Status Journal ingest
    // (spec/reeve/05-health-journal.md §7.3, C5): the journal routes
    // are unconditional core (router.rs), so this is always usable —
    // §3.3: advertise exactly what is compiled in.
    extensions.push(format_extension(rev::HEALTH_JOURNAL, 1));
    // rev-009/1 Secrets (spec/reeve/10-secrets.md §12.3, C7).
    if cfg!(feature = "ext-secrets") {
        extensions.push(format_extension(rev::SECRETS, 1));
    }
    // rev-011/1 Deploy logs (server ext-logs): advertised so a reeve
    // agent knows to upload captured compose output; a vanilla WFM and
    // a core --no-default-features build advertise nothing (§3.3).
    if cfg!(feature = "ext-logs") {
        extensions.push(format_extension(rev::DEPLOY_LOGS, 1));
    }
    ServerCapabilities {
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        extensions,
    }
}

/// GET /api/reeve/v1/capabilities (device-auth'd; anonymous pull of
/// anything device-facing is disabled by default, §10.2).
#[utoipa::path(
    get,
    path = "/api/reeve/v1/capabilities",
    tag = "device",
    responses(
        (status = 200, description = "Server extension advertisement (spec/reeve/01-framework.md §3.3)", body = ServerCapabilities),
        (status = 401, description = "Unauthenticated"),
    ),
)]
pub async fn capabilities() -> Json<ServerCapabilities> {
    Json(server_capabilities())
}

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "delivery route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn not_found(msg: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg }))).into_response()
}

/// RFC 9110 §8.8.3.2 STRONG comparison of an `If-None-Match` field
/// against our (always strong) ETag — per the delivery contract
/// (spec/reeve/08-packaging.md §10.2 "RFC 9110 strong validator"),
/// weak tags (`W/"…"`) never match. Handles multi-value lists and
/// repeated headers; naive comma split is sound because the digest
/// grammar `sha256:<hex>` contains no commas or quotes.
pub fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|candidate| {
            if candidate == "*" {
                return true;
            }
            if candidate.starts_with("W/") {
                return false; // weak never strong-matches
            }
            // Tolerate a missing-quote client; the agent sends quoted.
            let opaque = candidate.strip_prefix('"').unwrap_or(candidate);
            let opaque = opaque.strip_suffix('"').unwrap_or(opaque);
            opaque == etag
        })
}

/// GET /api/reeve/v1/manifest — the device-scoped State Manifest
/// (§10.2). Renders on demand when the device's row is behind the
/// local head (fresh enrollment, missed pass), then serves the stored
/// manifest bytes verbatim so the ETag is exact.
#[utoipa::path(
    get,
    path = "/api/reeve/v1/manifest",
    tag = "device",
    params(
        ("if-none-match" = Option<String>, Header, description = "RFC 9110 conditional GET against the manifest's strong ETag"),
    ),
    responses(
        (status = 200, description = "The device's current State Manifest (ETag header set)", body = reeve_types::reeve::manifest::StateManifest),
        (status = 304, description = "Not modified (If-None-Match matched)"),
        (status = 401, description = "Unauthenticated"),
        (status = 404, description = "Enrolled but never rendered", body = device_api::ErrorBody),
    ),
)]
pub async fn manifest(
    State(state): State<AppState>,
    DeviceIdentity(device_id): DeviceIdentity,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = render::ensure_current(&state, &device_id) {
        // Serving must degrade, not 500, if a stale row still exists;
        // only a device with NO manifest at all becomes an error below.
        warn!(device = %device_id, error = %e, "ensure_current failed; serving last stored manifest");
    }

    // Presence input (C5, presence.rs): the poll IS the liveness signal
    // until the persistent channel lands — an idle-but-healthy agent
    // polls forever with 304s and must still read as online.
    {
        let conn = state.db.lock().expect("db mutex poisoned");
        if let Err(e) = crate::ingest::touch_last_seen(&conn, &device_id, crate::db::now_secs()) {
            warn!(device = %device_id, error = %e, "last_seen touch failed");
        }
    }

    let row: Option<(String, String)> = {
        let conn = state.db.lock().expect("db mutex poisoned");
        match conn
            .query_row(
                "SELECT manifest_json, etag FROM device_manifests WHERE device_id = ?1",
                params![device_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        {
            Ok(r) => r,
            Err(e) => return internal(e),
        }
    };
    let Some((manifest_json, etag)) = row else {
        // Enrolled but unrenderable (broken tree on first render) —
        // the agent treats any non-200/304 as continue-from-last-known.
        return not_found("no manifest for this device");
    };

    let etag_header = format!("\"{etag}\"");
    if if_none_match_matches(&headers, &etag) {
        return ([(header::ETAG, etag_header)], StatusCode::NOT_MODIFIED).into_response();
    }
    (
        [
            (header::ETAG, etag_header),
            (
                header::CONTENT_TYPE,
                "application/json".to_string(),
            ),
        ],
        manifest_json,
    )
        .into_response()
}

/// GET /v2/ — OCI distribution base endpoint (spec conformance: 200
/// with an empty JSON body once authorized).
pub async fn v2_root() -> Response {
    Json(json!({})).into_response()
}

/// The per-device authorization set (§10.7): (bundle_digest,
/// layer_digest) this device's CURRENT manifest references — both may
/// be NULL for a zero-app device; `None` = no manifest row at all.
type ReferencedDigests = Option<(Option<String>, Option<String>)>;

/// Digests this device's CURRENT manifest references (§10.7).
fn referenced_digests(
    state: &AppState,
    device_id: &str,
) -> Result<ReferencedDigests, rusqlite::Error> {
    let conn = state.db.lock().expect("db mutex poisoned");
    conn.query_row(
        "SELECT bundle_digest, layer_digest FROM device_manifests WHERE device_id = ?1",
        params![device_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

fn blob_content(state: &AppState, digest: &str) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    let conn = state.db.lock().expect("db mutex poisoned");
    conn.query_row(
        "SELECT content FROM bundle_blobs WHERE digest = ?1",
        params![digest],
        |r| r.get(0),
    )
    .optional()
}

/// GET /v2/reeve/bundles/{device_id}/manifests/{digest} — the OCI
/// image manifest of the device's render bundle. A device may pull
/// only its own repo, and only the digest its State Manifest currently
/// references; anything else is 404 (§10.7).
pub async fn v2_manifest(
    State(state): State<AppState>,
    DeviceIdentity(caller): DeviceIdentity,
    Path((device_id, digest)): Path<(String, String)>,
) -> Response {
    if caller != device_id {
        return not_found("unknown repository");
    }
    let referenced = match referenced_digests(&state, &device_id) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let Some((Some(bundle_digest), _)) = referenced else {
        return not_found("unknown manifest");
    };
    if digest != bundle_digest {
        return not_found("unknown manifest");
    }
    match blob_content(&state, &digest) {
        Ok(Some(bytes)) => (
            [
                ("content-type", render::OCI_MANIFEST_MEDIA_TYPE.to_string()),
                ("docker-content-digest", digest.clone()),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => not_found("unknown manifest"),
        Err(e) => internal(e),
    }
}

/// GET /v2/reeve/bundles/{device_id}/blobs/{digest} — a blob of the
/// device's render-bundle artifact: its tar.gz layer or the shared
/// empty config blob. Same authorization rule as manifests.
pub async fn v2_blob(
    State(state): State<AppState>,
    DeviceIdentity(caller): DeviceIdentity,
    Path((device_id, digest)): Path<(String, String)>,
) -> Response {
    if caller != device_id {
        return not_found("unknown repository");
    }
    let referenced = match referenced_digests(&state, &device_id) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let Some((_, layer_digest)) = referenced else {
        return not_found("unknown blob");
    };
    let allowed = layer_digest.as_deref() == Some(digest.as_str())
        || (layer_digest.is_some() && digest == render::empty_config_digest());
    if !allowed {
        return not_found("unknown blob");
    }
    match blob_content(&state, &digest) {
        Ok(Some(bytes)) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => not_found("unknown blob"),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(header::IF_NONE_MATCH, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    const ETAG: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn strong_match_quoted() {
        assert!(if_none_match_matches(&headers(&[&format!("\"{ETAG}\"")]), ETAG));
    }

    #[test]
    fn strong_match_multi_value_list() {
        assert!(if_none_match_matches(
            &headers(&[&format!("\"sha256:{}\", \"{ETAG}\"", "b".repeat(64))]),
            ETAG
        ));
    }

    #[test]
    fn strong_match_repeated_headers() {
        let other = format!("\"sha256:{}\"", "c".repeat(64));
        assert!(if_none_match_matches(
            &headers(&[&other, &format!("\"{ETAG}\"")]),
            ETAG
        ));
    }

    #[test]
    fn star_matches_anything() {
        assert!(if_none_match_matches(&headers(&["*"]), ETAG));
    }

    #[test]
    fn weak_tag_never_strong_matches() {
        assert!(!if_none_match_matches(&headers(&[&format!("W/\"{ETAG}\"")]), ETAG));
    }

    #[test]
    fn mismatch_and_absent() {
        assert!(!if_none_match_matches(
            &headers(&[&format!("\"sha256:{}\"", "d".repeat(64))]),
            ETAG
        ));
        assert!(!if_none_match_matches(&HeaderMap::new(), ETAG));
    }

    #[test]
    fn unquoted_is_tolerated() {
        assert!(if_none_match_matches(&headers(&[ETAG]), ETAG));
    }

    #[test]
    fn capabilities_shape() {
        let caps = server_capabilities();
        assert_eq!(caps.server_version, env!("CARGO_PKG_VERSION"));
        // Advertise exactly what is compiled in (01-framework §3.3).
        let advertised = |e: &str| caps.extensions.contains(&e.to_string());
        assert_eq!(
            advertised("rev-001/1"),
            cfg!(feature = "ext-channel"),
            "rev-001 advertised iff ext-channel is compiled in: {:?}",
            caps.extensions
        );
        assert_eq!(
            advertised("rev-002/1"),
            cfg!(feature = "ext-terminal"),
            "rev-002 advertised iff ext-terminal is compiled in: {:?}",
            caps.extensions
        );
        assert_eq!(
            advertised("rev-003/1"),
            cfg!(feature = "ext-sse"),
            "rev-003 advertised iff ext-sse is compiled in: {:?}",
            caps.extensions
        );
        assert!(
            advertised("rev-004/1"),
            "journal ingest is core; rev-004 always advertised: {:?}",
            caps.extensions
        );
        assert_eq!(
            advertised("rev-009/1"),
            cfg!(feature = "ext-secrets"),
            "rev-009 advertised iff ext-secrets is compiled in: {:?}",
            caps.extensions
        );
    }
}
