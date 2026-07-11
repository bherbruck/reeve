//! Embedded UI assets + openapi.json (spec/reeve/08-packaging.md
//! §10.1: the reeve-server binary embeds the UI dist, served by path
//! with index.html fallback for SPA deep links, and the openapi.json
//! the UI client is generated from, served at a stable path).
//!
//! ui/dist is embedded via rust-embed; build.rs tolerates a missing
//! dist (creates an empty dir) until Track D wires the UI build — an
//! empty embed serves nothing and every non-API GET 404s, exactly the
//! pre-UI behavior. openapi.json is embed-if-present (build.rs) the
//! same way.

use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

// build.rs: `pub(crate) const OPENAPI_JSON: Option<&str>`.
include!(concat!(env!("OUT_DIR"), "/openapi_embed.rs"));

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist"]
struct UiDist;

/// GET /api/openapi.json — the §10.1 stable path for the embedded API
/// document. 404 until Track D generates one (embed-if-present); no
/// auth: it is shape, not values (.env rule), and the UI needs it
/// before login.
pub async fn openapi() -> Response {
    match OPENAPI_JSON {
        Some(doc) => (
            [(header::CONTENT_TYPE, "application/json")],
            doc,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Content type from the asset extension — the handful of types a
/// vite dist actually contains (boring by design; unknown => octet
/// stream).
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("json" | "map" | "webmanifest") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Router fallback: embedded UI asset by path, index.html for
/// anything else (SPA deep links MUST NOT 404, CLAUDE.md "ui/") —
/// except /api/* and /v2/*, which are API namespaces where a miss is
/// a real 404, never a masked-as-HTML one.
pub async fn spa_fallback(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    // /install is API space too (spec/reeve/08-packaging.md §10.4:
    // without the embedded-agents feature the route MUST be absent —
    // 404, never masked by the SPA shell; curl|sh must not eat HTML).
    if path.starts_with("api/")
        || path.starts_with("v2/")
        || matches!(path, "api" | "v2" | "install")
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let candidate = if path.is_empty() { "index.html" } else { path };
    let (served, file, cache) = match UiDist::get(candidate) {
        Some(f) => {
            // Vite emits content-hashed asset names: safe to cache
            // hard; everything else (index.html) must revalidate.
            let hashed = candidate.starts_with("assets/");
            (candidate, f, if hashed { "public, max-age=31536000, immutable" } else { "no-cache" })
        }
        None => match UiDist::get("index.html") {
            Some(f) => ("index.html", f, "no-cache"),
            // No UI embedded (pre-Track-D builds): honest 404.
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type(served)),
    );
    headers.insert(header::CACHE_CONTROL, header::HeaderValue::from_static(cache));
    if method == Method::HEAD {
        return (headers, ()).into_response();
    }
    (headers, file.data).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn api_v2_and_install_misses_stay_404() {
        for path in ["/api/nope", "/v2/nope", "/api", "/v2", "/install"] {
            let res = spa_fallback(Method::GET, path.parse().unwrap()).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn non_get_is_404() {
        let res = spa_fallback(Method::POST, "/anything".parse().unwrap()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn deep_link_serves_index_when_ui_embedded() {
        // Pre-Track-D builds have no ui/dist: the fallback must then
        // 404 rather than serve an empty page; with a dist present it
        // must serve index.html for the deep link.
        let res = spa_fallback(Method::GET, "/devices/dev-1".parse().unwrap()).await;
        match UiDist::get("index.html") {
            Some(_) => assert_eq!(res.status(), StatusCode::OK),
            None => assert_eq!(res.status(), StatusCode::NOT_FOUND),
        }
    }
}
