//! REV-007 §10.4 `/install` bootstrap endpoint (cargo feature
//! `embedded-agents`; spec/reeve/08-packaging.md).
//!
//! The server embeds the reeve-agent binaries for both architectures
//! (build.rs: `REEVE_AGENT_BINARIES` or this workspace's own musl
//! release outputs; §10.4 version coherence asserted at build) and
//! serves them:
//! - as OCI artifacts on the native /v2 routes — repos
//!   `reeve/agent/<arch>` ("the agent is an artifact",
//!   docs/decisions/delivery.md D7); standard read-only distribution
//!   pull, no push routes;
//! - plus `GET /install`, a shell script that detects the machine
//!   architecture, pulls the matching agent BY DIGEST (baked into the
//!   script at build) from the same origin it will enroll against,
//!   and runs `reeve-agent install` pointed at that origin.
//!
//! Auth (§10.4): requires an enrollment credential by default — a
//! valid join token (`rvj_…`, checked but NOT consumed; consumption
//! is enrollment's job) as `Authorization: Bearer` or `?token=`. A
//! device credential (`rvd_…`) is also accepted: the §10.5 self-update
//! path pulls these same artifacts with the device token. A deployment
//! MAY open the endpoint on trusted networks: REEVE_INSTALL_OPEN=true.
//! Without the feature none of these routes exist (404) — invisible,
//! 01-framework §3.1 rule 4.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use revision_store::digest_of;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::render::{EMPTY_CONFIG_BLOB, OCI_EMPTY_MEDIA_TYPE, OCI_MANIFEST_MEDIA_TYPE};
use crate::state::AppState;

/// Media type of the agent binary layer (reeve-coined, additive —
/// nothing Margo-shaped carries it).
pub const AGENT_BINARY_MEDIA_TYPE: &str = "application/vnd.reeve.agent.binary.v1";

/// One embedded agent binary, wrapped as a stock OCI artifact
/// (image manifest + empty config + one binary layer) so oras/skopeo/
/// crane can pull it (§10.2).
pub struct ArchArtifact {
    pub arch: &'static str,
    pub binary: Cow<'static, [u8]>,
    pub blob_digest: String,
    pub manifest: Vec<u8>,
    pub manifest_digest: String,
}

impl ArchArtifact {
    pub fn new(arch: &'static str, binary: Cow<'static, [u8]>) -> Self {
        let blob_digest = digest_of(&binary);
        // Same manifest shape as the render-bundle artifacts
        // (render.rs): schemaVersion 2 + artifactType + empty config.
        // serde_json maps are sorted => deterministic bytes.
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST_MEDIA_TYPE,
            "artifactType": AGENT_BINARY_MEDIA_TYPE,
            "config": {
                "mediaType": OCI_EMPTY_MEDIA_TYPE,
                "digest": crate::render::empty_config_digest(),
                "size": EMPTY_CONFIG_BLOB.len(),
            },
            "layers": [{
                "mediaType": AGENT_BINARY_MEDIA_TYPE,
                "digest": blob_digest,
                "size": binary.len(),
            }],
            "annotations": {
                // §10.4 version coherence is asserted at build; the
                // annotation makes it auditable at pull time.
                "org.opencontainers.image.revision": env!("GIT_HASH"),
                "org.opencontainers.image.title": format!("reeve-agent-{arch}"),
            },
        }))
        .expect("static json");
        let manifest_digest = digest_of(&manifest);
        ArchArtifact { arch, binary, blob_digest, manifest, manifest_digest }
    }
}

// build.rs: `pub(crate) const AGENT_X86_64 / AGENT_AARCH64:
// Option<&[u8]>` — embed-if-present from REEVE_AGENT_BINARIES or the
// workspace's own musl release outputs.
include!(concat!(env!("OUT_DIR"), "/agent_binaries.rs"));

/// The artifacts this build embeds (either arch may be absent — the
/// build warns; /install then refuses that architecture at run time).
pub fn embedded_artifacts() -> Vec<ArchArtifact> {
    let mut v = Vec::new();
    if let Some(bin) = AGENT_X86_64 {
        v.push(ArchArtifact::new("x86_64", Cow::Borrowed(bin)));
    }
    if let Some(bin) = AGENT_AARCH64 {
        v.push(ArchArtifact::new("aarch64", Cow::Borrowed(bin)));
    }
    v
}

#[derive(Clone)]
struct InstallState {
    db: Arc<Mutex<Connection>>,
    artifacts: Arc<Vec<ArchArtifact>>,
    /// REEVE_INSTALL_OPEN=true — anonymous bootstrap on trusted
    /// networks (§10.4 "a deployment MAY open it").
    open: bool,
}

/// Routes for the embedded artifacts of THIS build, wired by
/// [`crate::run_with_options`] under the feature gate.
pub fn router_from_embedded(state: &AppState) -> Router {
    router(state, embedded_artifacts())
}

/// Routes over explicit artifacts (tests inject dummies here — the
/// handlers and script generation are identical to production).
pub fn router(state: &AppState, artifacts: Vec<ArchArtifact>) -> Router {
    let ist = InstallState {
        db: state.db.clone(),
        artifacts: Arc::new(artifacts),
        open: state.cfg.install_open,
    };
    Router::new()
        .route("/install", get(install_script))
        .route("/v2/reeve/agent/{arch}/manifests/{reference}", get(v2_manifest))
        .route("/v2/reeve/agent/{arch}/blobs/{digest}", get(v2_blob))
        .with_state(ist)
}

#[derive(Debug, Deserialize, Default)]
struct TokenQuery {
    token: Option<String>,
}

/// Extract the presented credential: `Authorization: Bearer …` wins,
/// `?token=` accepted for the curl|sh flow (§10.4 — the script needs
/// the raw token baked in so `reeve-agent install` can enroll).
fn presented_token(headers: &HeaderMap, query: &TokenQuery) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| query.token.clone())
}

/// §10.4 default-closed authorization: valid join token (not
/// consumed) or valid device token; `open` admits anyone.
fn authorized(ist: &InstallState, token: Option<&str>) -> Result<bool, rusqlite::Error> {
    if ist.open {
        return Ok(true);
    }
    let Some(token) = token else { return Ok(false) };
    let hash = device_api::token_hash(token);
    let conn = ist.db.lock().expect("db mutex poisoned");
    if token.starts_with(crate::join_tokens::JOIN_TOKEN_PREFIX) {
        // Valid = exists, unrevoked, unexpired, uses remaining — the
        // same gates enrollment applies (enroll.rs), WITHOUT bumping
        // `uses`: the bootstrap script's pull must not burn the very
        // token its `reeve-agent install` step is about to consume.
        let ok: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM join_tokens
                 WHERE token_hash = ?1 AND revoked_at IS NULL
                   AND expires_at > ?2 AND uses < max_uses",
                params![hash, crate::db::now_secs()],
                |r| r.get(0),
            )
            .optional()?;
        return Ok(ok.is_some());
    }
    // Device credential (self-update pull path, §10.5).
    let ok: Option<String> = conn
        .query_row(
            "SELECT device_id FROM device_tokens
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![hash],
            |r| r.get(0),
        )
        .optional()?;
    Ok(ok.is_some())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(json!({ "error": "enrollment token required (spec/reeve/08-packaging.md §10.4)" })),
    )
        .into_response()
}

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "install route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn not_found(msg: &str) -> Response {
    (StatusCode::NOT_FOUND, axum::Json(json!({ "error": msg }))).into_response()
}

/// The request's origin as the script's server URL: one origin for
/// trust, transport and enrollment (§10.4). Plain-HTTP server behind
/// an optional TLS proxy — X-Forwarded-Proto wins, else http.
fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok())?;
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    Some(format!("{proto}://{host}"))
}

/// Render the §10.4 bootstrap script. Digests are baked in (binding
/// binary to script — §10.7); `token`, when presented, is baked in so
/// the pull authenticates and `reeve-agent install` can enroll.
fn render_script(artifacts: &[ArchArtifact], origin: &str, token: Option<&str>) -> String {
    let digest_for = |arch: &str| {
        artifacts
            .iter()
            .find(|a| a.arch == arch)
            .map(|a| a.blob_digest.clone())
            .unwrap_or_default()
    };
    format!(
        r#"#!/bin/sh
# reeve agent bootstrap (spec/reeve/08-packaging.md §10.4).
# Pulls the reeve-agent binary BY DIGEST from the server this script
# came from, verifies it, and runs `reeve-agent install` pointed at
# that same origin — one origin for trust, transport and enrollment.
set -eu

SERVER="{origin}"
TOKEN="{token}"
DIGEST_X86_64="{d_x86}"
DIGEST_AARCH64="{d_arm}"

case "$(uname -m)" in
    x86_64|amd64)  ARCH=x86_64;  DIGEST="$DIGEST_X86_64" ;;
    aarch64|arm64) ARCH=aarch64; DIGEST="$DIGEST_AARCH64" ;;
    *) echo "reeve install: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
if [ -z "$DIGEST" ]; then
    echo "reeve install: this server embeds no reeve-agent for $ARCH" >&2
    exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ -n "$TOKEN" ]; then
    curl -fsSL -H "Authorization: Bearer $TOKEN" \
        "$SERVER/v2/reeve/agent/$ARCH/blobs/$DIGEST" -o "$TMP/reeve-agent"
else
    curl -fsSL "$SERVER/v2/reeve/agent/$ARCH/blobs/$DIGEST" -o "$TMP/reeve-agent"
fi

# Digest check binds the binary to this script (§10.7).
echo "${{DIGEST#sha256:}}  $TMP/reeve-agent" | sha256sum -c - >/dev/null
chmod +x "$TMP/reeve-agent"

if [ -n "$TOKEN" ]; then
    exec "$TMP/reeve-agent" install --server "$SERVER" --token "$TOKEN"
else
    exec "$TMP/reeve-agent" install
fi
"#,
        origin = origin,
        token = token.unwrap_or(""),
        d_x86 = digest_for("x86_64"),
        d_arm = digest_for("aarch64"),
    )
}

/// GET /install — the bootstrap script (§10.4).
async fn install_script(
    State(ist): State<InstallState>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    let token = presented_token(&headers, &query);
    match authorized(&ist, token.as_deref()) {
        Ok(true) => {}
        Ok(false) => return unauthorized(),
        Err(e) => return internal(e),
    }
    let Some(origin) = request_origin(&headers) else {
        return (StatusCode::BAD_REQUEST, "missing Host header").into_response();
    };
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        render_script(&ist.artifacts, &origin, token.as_deref()),
    )
        .into_response()
}

/// GET /v2/reeve/agent/{arch}/manifests/{reference} — OCI image
/// manifest of the agent artifact; `latest` or the manifest digest.
async fn v2_manifest(
    State(ist): State<InstallState>,
    Path((arch, reference)): Path<(String, String)>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    match authorized(&ist, presented_token(&headers, &query).as_deref()) {
        Ok(true) => {}
        Ok(false) => return unauthorized(),
        Err(e) => return internal(e),
    }
    let Some(artifact) = ist.artifacts.iter().find(|a| a.arch == arch) else {
        return not_found("unknown repository");
    };
    if reference != "latest" && reference != artifact.manifest_digest {
        return not_found("unknown manifest");
    }
    (
        [
            (header::CONTENT_TYPE, OCI_MANIFEST_MEDIA_TYPE.to_string()),
            (
                header::HeaderName::from_static("docker-content-digest"),
                artifact.manifest_digest.clone(),
            ),
        ],
        artifact.manifest.clone(),
    )
        .into_response()
}

/// GET /v2/reeve/agent/{arch}/blobs/{digest} — the binary layer (or
/// the shared empty config blob).
async fn v2_blob(
    State(ist): State<InstallState>,
    Path((arch, digest)): Path<(String, String)>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    match authorized(&ist, presented_token(&headers, &query).as_deref()) {
        Ok(true) => {}
        Ok(false) => return unauthorized(),
        Err(e) => return internal(e),
    }
    let Some(artifact) = ist.artifacts.iter().find(|a| a.arch == arch) else {
        return not_found("unknown repository");
    };
    if digest == artifact.blob_digest {
        return (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            artifact.binary.clone().into_owned(),
        )
            .into_response();
    }
    if digest == crate::render::empty_config_digest() {
        return (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            EMPTY_CONFIG_BLOB.to_vec(),
        )
            .into_response();
    }
    not_found("unknown blob")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy() -> ArchArtifact {
        ArchArtifact::new("x86_64", Cow::Owned(b"#!fake-elf".to_vec()))
    }

    #[test]
    fn artifact_manifest_is_stock_oci_and_content_addressed() {
        let a = dummy();
        assert_eq!(a.blob_digest, digest_of(b"#!fake-elf"));
        assert_eq!(a.manifest_digest, digest_of(&a.manifest));
        let m: serde_json::Value = serde_json::from_slice(&a.manifest).unwrap();
        assert_eq!(m["schemaVersion"], 2);
        assert_eq!(m["mediaType"], OCI_MANIFEST_MEDIA_TYPE);
        assert_eq!(m["layers"][0]["digest"], a.blob_digest);
        assert_eq!(m["layers"][0]["size"], 10);
        assert_eq!(m["annotations"]["org.opencontainers.image.revision"], env!("GIT_HASH"));
    }

    #[test]
    fn script_bakes_digest_origin_and_token() {
        let a = dummy();
        let s = render_script(
            std::slice::from_ref(&a),
            "https://reeve.site.example",
            Some("rvj_abc"),
        );
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains(&format!("DIGEST_X86_64=\"{}\"", a.blob_digest)));
        assert!(s.contains("DIGEST_AARCH64=\"\""), "absent arch refused at run time");
        assert!(s.contains("SERVER=\"https://reeve.site.example\""));
        assert!(s.contains("TOKEN=\"rvj_abc\""));
        assert!(s.contains("install --server \"$SERVER\" --token \"$TOKEN\""));
    }

    #[test]
    fn origin_prefers_forwarded_proto() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "reeve.example:8420".parse().unwrap());
        assert_eq!(request_origin(&h).unwrap(), "http://reeve.example:8420");
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(request_origin(&h).unwrap(), "https://reeve.example:8420");
    }
}
