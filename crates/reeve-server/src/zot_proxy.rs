//! /v2 image reverse proxy to the zot sidecar (C11).
//!
//! Doc sources: docs/decisions/delivery.md D8 (THE decision — "zot
//! sidecar, proxied through reeve"; single endpoint preserved) and
//! spec/reeve/08-packaging.md §10.2 (one /v2 route space, one
//! listening socket; "when the registry sidecar is deployed — the
//! image proxy on the same /v2 space").
//!
//! Route split on the ONE /v2 space (D7/D8 scope split): reeve's OWN
//! artifact namespace (`reeve/…` — render bundles, vendored packages,
//! agent binaries) is served natively by delivery.rs and NEVER
//! proxied; every other repo reverse-proxies to zot. With no backend
//! configured (`REEVE_ZOT_URL` unset) the proxy is absent and those
//! repos fall through to the native 404.
//!
//! Contract (D8):
//! - Pull ONLY: GET/HEAD of `…/manifests/<ref>`, `…/blobs/<digest>`,
//!   `…/tags/list`. Push verbs (PUT/POST/PATCH/DELETE) are 405 —
//!   images are pushed/pinned at the hub's zot directly, never
//!   through the device-facing proxy.
//! - Auth termination: the device token is checked by
//!   `device_api::device_auth` in front of this handler (anonymous is
//!   401 there, §10.2 "anonymous pull MUST NOT be enabled by
//!   default") and is STRIPPED here — the proxy "terminates device
//!   auth and speaks its own credential to zot" (D8). Device tokens
//!   never reach the sidecar.
//! - Streaming: blob bodies stream through (hyper-util legacy client;
//!   the response body is handed to axum untouched) — a multi-GB
//!   image layer never buffers in reeve-server RAM.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use http_body_util::Empty;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::json;
use tracing::warn;

use crate::config::ZotConfig;
use crate::state::AppState;

/// The reverse-proxy handle: a pooled plain-http client plus the
/// backend location and the (env-sourced) credential. Built once at
/// bootstrap; cheap to clone (the client is an Arc'd pool).
#[derive(Clone)]
pub struct ZotProxy {
    client: Client<HttpConnector, Empty<Bytes>>,
    /// e.g. `http://127.0.0.1:5000`, no trailing slash (config.rs
    /// normalizes).
    base: String,
    /// REEVE_ZOT_USERNAME/PASSWORD. Env wins over the vault (recorded
    /// choice: explicit config beats stored state); `None` falls back
    /// to the vault's internal scope per request, so a vault rotation
    /// takes effect without a restart (crash-only: nothing cached).
    env_credentials: Option<(String, String)>,
}

impl ZotProxy {
    pub fn from_config(cfg: &ZotConfig) -> Self {
        ZotProxy {
            // The boring choice (recorded): hyper-util's legacy pooled
            // client over plain TCP — same hyper family axum already
            // pins, streams request/response bodies natively, no TLS
            // stack (the sidecar is co-located, D8).
            client: Client::builder(TokioExecutor::new()).build_http(),
            base: cfg.url.clone(),
            env_credentials: cfg.username.clone().zip(cfg.password.clone()),
        }
    }

    /// The Basic credential the proxy presents to zot (D8). Env pair
    /// first; else the vault's internal-scope `zot.upstream.username`/
    /// `zot.upstream.password` (spec/reeve/10-secrets.md §12.2 typed
    /// getter); else anonymous — zot only accepts connections from
    /// reeve-server (D8), so a credential-less deployment is legal.
    fn credentials(&self, state: &AppState) -> Option<(String, String)> {
        if let Some(c) = &self.env_credentials {
            return Some(c.clone());
        }
        vault_credentials(state)
    }
}

#[cfg(feature = "ext-secrets")]
fn vault_credentials(state: &AppState) -> Option<(String, String)> {
    let key = match crate::ext::secrets::vault_key(&state.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "zot proxy: vault keyfile unavailable; proceeding anonymously");
            return None;
        }
    };
    let conn = state.db.lock().expect("db mutex poisoned");
    match crate::ext::secrets::zot_upstream_credentials(&conn, &key) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "zot proxy: vault credential lookup failed; proceeding anonymously");
            None
        }
    }
}

#[cfg(not(feature = "ext-secrets"))]
fn vault_credentials(_state: &AppState) -> Option<(String, String)> {
    None
}

fn not_found(msg: &str) -> Response {
    // Same 404 shape as delivery.rs: the confidentiality boundary does
    // not confirm existence (§10.7 posture carried over).
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg }))).into_response()
}

/// Is `rest` (the path under `/v2/`) one of the three PULL shapes the
/// proxy relays (D8: pull only)? `<repo…>/manifests/<ref>`,
/// `<repo…>/blobs/<digest>`, `<repo…>/tags/list` — repo non-empty, no
/// empty segments. Everything else (`_catalog`, referrers, upload
/// sessions…) is not part of the device-facing surface.
fn is_pull_path(rest: &str) -> bool {
    let segs: Vec<&str> = rest.split('/').collect();
    if segs.len() < 3 || segs.iter().any(|s| s.is_empty()) {
        return false;
    }
    let n = segs.len();
    segs[n - 2] == "manifests"
        || segs[n - 2] == "blobs"
        || (segs[n - 2] == "tags" && segs[n - 1] == "list")
}

/// RFC 9110 §7.6.1 hop-by-hop fields (plus legacy `keep-alive`) —
/// never forwarded in either direction.
fn is_hop_by_hop(name: &axum::http::HeaderName) -> bool {
    name == header::CONNECTION
        || name == header::TE
        || name == header::TRAILER
        || name == header::TRANSFER_ENCODING
        || name == header::UPGRADE
        || name == header::PROXY_AUTHENTICATE
        || name == header::PROXY_AUTHORIZATION
        || name.as_str() == "keep-alive"
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let doomed: Vec<axum::http::HeaderName> = headers
        .keys()
        .filter(|n| is_hop_by_hop(n))
        .cloned()
        .collect();
    for name in doomed {
        headers.remove(name);
    }
}

/// `GET|HEAD /v2/{*rest}` — the catch-all leg of the one /v2 space.
/// Sits BEHIND `device_api::device_auth` (router.rs): an anonymous
/// caller never reaches this handler (401 in the middleware).
pub async fn proxy_route(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let rest = path.strip_prefix("/v2/").unwrap_or("");

    // Native namespace (D7): reeve's own artifact repos are served by
    // delivery.rs routes; any `reeve/…` path that reaches the
    // catch-all is an unknown native artifact — 404, NEVER proxied.
    if rest == "reeve" || rest.starts_with("reeve/") {
        return not_found("unknown repository");
    }

    // Proxy absent (REEVE_ZOT_URL unset): proxied repos fall through
    // to the same 404 the native side gives unknown repos.
    let Some(proxy) = state.zot.clone() else {
        return not_found("unknown repository");
    };

    // Pull only (D8): push verbs are blocked at the proxy — 405, not
    // 404, so a misdirected `docker push` fails loudly and correctly.
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
        )
            .into_response();
    }

    if !is_pull_path(rest) {
        return not_found("unknown repository");
    }

    // Build the backend request: same method/path/query, headers minus
    // hop-by-hop, minus Host (the client re-derives it), and minus the
    // device Authorization — replaced by the proxy's own credential.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(path.as_str());
    let uri = format!("{}{}", proxy.base, path_and_query);

    let mut backend = axum::http::Request::builder()
        .method(req.method().clone())
        .uri(&uri);
    {
        let headers = backend.headers_mut().expect("fresh builder");
        for (name, value) in req.headers() {
            if is_hop_by_hop(name)
                || name == header::HOST
                || name == header::AUTHORIZATION
                || name == header::CONTENT_LENGTH
            {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        if let Some((user, pass)) = proxy.credentials(&state) {
            let basic = format!("Basic {}", B64.encode(format!("{user}:{pass}")));
            match HeaderValue::from_str(&basic) {
                Ok(v) => {
                    headers.insert(header::AUTHORIZATION, v);
                }
                Err(e) => {
                    warn!(error = %e, "zot proxy: backend credential is not a valid header value");
                    return StatusCode::BAD_GATEWAY.into_response();
                }
            }
        }
    }
    let backend = match backend.body(Empty::<Bytes>::new()) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, uri = %uri, "zot proxy: building backend request failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    match proxy.client.request(backend).await {
        Ok(resp) => {
            // zot authenticates reeve-server, not devices (D8): a
            // backend 401/403 is OUR credential misconfigured — 502,
            // never relayed (a relayed 401 would read as "your device
            // token is bad", which it is not).
            if matches!(
                resp.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) {
                warn!(status = %resp.status(), uri = %uri, "zot proxy: backend rejected the proxy credential");
                return StatusCode::BAD_GATEWAY.into_response();
            }
            let (mut parts, body) = resp.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            // Body::new wraps the hyper Incoming stream — bytes flow
            // through chunk by chunk, never buffered whole.
            Response::from_parts(parts, Body::new(body))
        }
        Err(e) => {
            warn!(error = %e, uri = %uri, "zot proxy: backend unreachable");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_paths_recognized() {
        assert!(is_pull_path("library/alpine/manifests/latest"));
        assert!(is_pull_path("library/alpine/manifests/sha256:abc"));
        assert!(is_pull_path(
            "deep/nested/repo/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000"
        ));
        assert!(is_pull_path("library/alpine/tags/list"));
    }

    #[test]
    fn non_pull_paths_rejected() {
        assert!(!is_pull_path("")); // no repo
        assert!(!is_pull_path("manifests/latest")); // empty repo
        assert!(!is_pull_path("blobs/sha256:abc"));
        assert!(!is_pull_path("tags/list"));
        assert!(!is_pull_path("_catalog"));
        assert!(!is_pull_path("library/alpine")); // bare repo
        assert!(!is_pull_path("library/alpine/blobs/uploads/")); // push session
        assert!(!is_pull_path("library//manifests/latest")); // empty segment
        assert!(!is_pull_path("library/alpine/referrers/sha256:abc"));
    }

    #[test]
    fn hop_by_hop_set() {
        assert!(is_hop_by_hop(&header::CONNECTION));
        assert!(is_hop_by_hop(&header::TRANSFER_ENCODING));
        assert!(is_hop_by_hop(&axum::http::HeaderName::from_static(
            "keep-alive"
        )));
        assert!(!is_hop_by_hop(&header::ACCEPT));
        assert!(!is_hop_by_hop(&header::CONTENT_TYPE));
        assert!(!is_hop_by_hop(&header::RANGE));
    }
}
