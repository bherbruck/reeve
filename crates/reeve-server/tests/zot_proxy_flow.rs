//! C11 zot /v2 reverse proxy over the real router
//! (docs/decisions/delivery.md D8; spec/reeve/08-packaging.md §10.2:
//! one /v2 route space, two backends, invisible to clients).
//!
//! Covered here:
//! - route split: reeve's OWN artifact repo still answers natively
//!   (and the mock zot sees NOTHING for `reeve/…` paths); any other
//!   repo reverse-proxies to zot;
//! - auth termination (D8): the device Bearer token is checked by the
//!   proxy and never forwarded — the backend sees the server's own
//!   injected Basic credential instead; anonymous callers get 401;
//! - pull only: PUT/POST/PATCH/DELETE => 405, backend untouched;
//! - proxy absent (REEVE_ZOT_URL unset) => proxied repos 404;
//! - streaming: a multi-MB blob body passes through chunked and
//!   byte-identical (functional assertion on content equality).

use std::net::SocketAddr;
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::IntoResponse;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use http_body_util::BodyExt as _;
use rusqlite::params;
use serde_json::json;
use tower::ServiceExt as _;

use reeve_server::config::{AuthMode, Config, ZotConfig};
use reeve_server::state::AppState;
use reeve_server::{auth, device_tokens, router};

// ------------------------------------------------------------- harness

const BLOB_SIZE: usize = 8 * 1024 * 1024; // multi-MB streaming check
const MANIFEST_BODY: &str = "zot-manifest-bytes";

fn config(data_dir: &FsPath, zot: Option<ZotConfig>) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth: AuthMode::None,
        session_ttl_secs: 3600,
        tier: reeve_server::config::ServerTier::Root,
        registry_endpoint: "registry.example:5000".to_string(),
        durability: reeve_server::config::DurabilityConfig::disabled(),
        zot,
        federation: None,
        install_open: false,
        admin_seed: None,
        logs_retain_per_deployment: 10,
    }
}

fn app(dir: &FsPath, zot: Option<ZotConfig>) -> (Router, AppState) {
    let state = reeve_server::bootstrap(config(dir, zot)).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state)
}

fn add_device(state: &AppState, id: &str) -> String {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at)
         VALUES (?1, 'box', 'x86_64', '0.1.0', 0)",
        params![id],
    )
    .unwrap();
    device_tokens::issue(&conn, id).unwrap()
}

/// One observed backend request: method, path?query, headers.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, String, HeaderMap)>>>);

impl Seen {
    fn requests(&self) -> Vec<(String, String, HeaderMap)> {
        self.0.lock().unwrap().clone()
    }
}

/// Mock zot: real listener (the proxy dials TCP), records every
/// request, answers the three pull shapes. The blob is streamed in
/// 64 KiB chunks to force chunked transfer through the proxy.
async fn mock_zot(blob: Arc<Vec<u8>>) -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let recorder = seen.clone();
    let handler = move |req: Request<Body>| {
        let recorder = recorder.clone();
        let blob = blob.clone();
        async move {
            let path = req.uri().path().to_string();
            let pq = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| path.clone());
            recorder
                .0
                .lock()
                .unwrap()
                .push((req.method().to_string(), pq, req.headers().clone()));
            if path.ends_with("/tags/list") {
                axum::Json(json!({ "name": "library/alpine", "tags": ["latest"] }))
                    .into_response()
            } else if path.contains("/manifests/") {
                (
                    [
                        ("content-type", "application/vnd.oci.image.manifest.v1+json"),
                        ("docker-content-digest", "sha256:feedface"),
                    ],
                    MANIFEST_BODY,
                )
                    .into_response()
            } else if path.contains("/blobs/") {
                let chunks: Vec<Result<Bytes, std::io::Error>> = blob
                    .chunks(64 * 1024)
                    .map(|c| Ok(Bytes::copy_from_slice(c)))
                    .collect();
                Body::from_stream(futures_util::stream::iter(chunks)).into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
    };
    let router = Router::new().fallback(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (addr, seen)
}

fn zot_config(addr: SocketAddr) -> ZotConfig {
    ZotConfig {
        url: format!("http://{addr}"),
        username: Some("zotuser".into()),
        password: Some("zotpass".into()),
    }
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

fn request(method: Method, uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

/// D8's core promise: the device token terminates at the proxy. No
/// header the backend saw may carry it.
fn assert_no_token_leak(seen: &Seen, token: &str) {
    for (method, path, headers) in seen.requests() {
        for (name, value) in &headers {
            let v = value.to_str().unwrap_or("");
            assert!(
                !v.contains(token),
                "device token leaked to zot in {method} {path} header {name}: {v}"
            );
        }
    }
}

// --------------------------------------------------------------- tests

/// Route split + credential injection: a non-`reeve/*` repo proxies to
/// zot with the server's Basic credential and without the device
/// token; response body/headers relay back.
#[tokio::test]
async fn proxied_repo_reaches_zot_with_injected_basic_auth() {
    let dir = tempfile::tempdir().unwrap();
    let (zot_addr, seen) = mock_zot(Arc::new(Vec::new())).await;
    let (app, state) = app(dir.path(), Some(zot_config(zot_addr)));
    let token = add_device(&state, "dev-1");

    let (status, headers, body) = send(
        &app,
        request(Method::GET, "/v2/library/alpine/manifests/latest", Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, MANIFEST_BODY.as_bytes());
    assert_eq!(
        headers.get("docker-content-digest").unwrap(),
        "sha256:feedface",
        "backend headers relay through"
    );

    let reqs = seen.requests();
    assert_eq!(reqs.len(), 1, "exactly one backend request");
    let (method, path, backend_headers) = &reqs[0];
    assert_eq!(method, "GET");
    assert_eq!(path, "/v2/library/alpine/manifests/latest");
    let expected_basic = format!("Basic {}", B64.encode("zotuser:zotpass"));
    assert_eq!(
        backend_headers.get(header::AUTHORIZATION).unwrap(),
        expected_basic.as_str(),
        "the proxy speaks its OWN credential to zot (D8)"
    );
    assert_no_token_leak(&seen, &token);
}

/// tags/list is part of the pull surface; query strings pass through.
#[tokio::test]
async fn tags_list_proxies_with_query() {
    let dir = tempfile::tempdir().unwrap();
    let (zot_addr, seen) = mock_zot(Arc::new(Vec::new())).await;
    let (app, state) = app(dir.path(), Some(zot_config(zot_addr)));
    let token = add_device(&state, "dev-1");

    let (status, _, body) = send(
        &app,
        request(Method::GET, "/v2/library/alpine/tags/list?n=10", Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["name"], "library/alpine");
    let reqs = seen.requests();
    assert_eq!(reqs[0].1, "/v2/library/alpine/tags/list?n=10");
    assert_no_token_leak(&seen, &token);
}

/// The `reeve/…` namespace is NEVER proxied (D7/D8 scope split): the
/// native routes answer (404 for unknown artifacts — §10.7 posture)
/// and the backend sees nothing.
#[tokio::test]
async fn native_namespace_never_reaches_zot() {
    let dir = tempfile::tempdir().unwrap();
    let (zot_addr, seen) = mock_zot(Arc::new(Vec::new())).await;
    let (app, state) = app(dir.path(), Some(zot_config(zot_addr)));
    let token = add_device(&state, "dev-1");

    for uri in [
        // Native route, unknown digest for this device: native 404.
        "/v2/reeve/bundles/dev-1/manifests/sha256:0000000000000000000000000000000000000000000000000000000000000000",
        // Native NAMESPACE without a native route yet (packages, agent
        // binaries): still never proxied.
        "/v2/reeve/packages/web/manifests/1.0.0",
        "/v2/reeve/agent/blobs/sha256:abc",
    ] {
        let (status, _, _) = send(&app, request(Method::GET, uri, Some(&token))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(
        seen.requests().is_empty(),
        "zot must never see reeve/* traffic: {:?}",
        seen.requests().iter().map(|r| &r.1).collect::<Vec<_>>()
    );
}

/// Pull only (D8): push verbs are 405 at the proxy, backend untouched.
#[tokio::test]
async fn push_verbs_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let (zot_addr, seen) = mock_zot(Arc::new(Vec::new())).await;
    let (app, state) = app(dir.path(), Some(zot_config(zot_addr)));
    let token = add_device(&state, "dev-1");

    for method in [Method::PUT, Method::POST, Method::PATCH, Method::DELETE] {
        let (status, headers, _) = send(
            &app,
            request(
                method.clone(),
                "/v2/library/alpine/manifests/latest",
                Some(&token),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(headers.get(header::ALLOW).unwrap(), "GET, HEAD");
    }
    assert!(seen.requests().is_empty(), "blocked verbs never proxied");
}

/// Anonymous pull MUST NOT be enabled (§10.2): no device token => 401
/// from device_auth, before any proxying.
#[tokio::test]
async fn anonymous_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let (zot_addr, seen) = mock_zot(Arc::new(Vec::new())).await;
    let (app, _state) = app(dir.path(), Some(zot_config(zot_addr)));

    let (status, _, _) = send(
        &app,
        request(Method::GET, "/v2/library/alpine/manifests/latest", None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(seen.requests().is_empty());
}

/// REEVE_ZOT_URL unset => the proxy is absent: proxied repos fall
/// through to the native 404 (and native repos keep working — the
/// route space itself never changes shape).
#[tokio::test]
async fn unset_config_means_404() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), None);
    let token = add_device(&state, "dev-1");

    let (status, _, _) = send(
        &app,
        request(Method::GET, "/v2/library/alpine/manifests/latest", Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // /v2/ base endpoint (native) still answers.
    let (status, _, _) = send(&app, request(Method::GET, "/v2/", Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
}

/// Non-pull paths on the proxied space (catalog, upload sessions,
/// referrers) are not part of the device-facing surface: 404, backend
/// untouched.
#[tokio::test]
async fn non_pull_paths_are_404() {
    let dir = tempfile::tempdir().unwrap();
    let (zot_addr, seen) = mock_zot(Arc::new(Vec::new())).await;
    let (app, state) = app(dir.path(), Some(zot_config(zot_addr)));
    let token = add_device(&state, "dev-1");

    for uri in [
        "/v2/_catalog",
        "/v2/library/alpine/blobs/uploads/",
        "/v2/library/alpine/referrers/sha256:abc",
    ] {
        let (status, _, _) = send(&app, request(Method::GET, uri, Some(&token))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(seen.requests().is_empty());
}

/// Streaming passthrough: a multi-MB blob served chunked by the
/// backend arrives byte-identical through the proxy (functional
/// equality — the no-buffering property is the hyper Incoming stream
/// handed straight to axum, zot_proxy.rs).
#[tokio::test]
async fn large_blob_streams_through_byte_identical() {
    let blob: Arc<Vec<u8>> = Arc::new(
        (0..BLOB_SIZE)
            .map(|i| (i % 251) as u8) // non-repeating-page pattern
            .collect(),
    );
    let dir = tempfile::tempdir().unwrap();
    let (zot_addr, seen) = mock_zot(blob.clone()).await;
    let (app, state) = app(dir.path(), Some(zot_config(zot_addr)));
    let token = add_device(&state, "dev-1");

    let (status, _, body) = send(
        &app,
        request(
            Method::GET,
            "/v2/library/alpine/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000",
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), BLOB_SIZE);
    assert_eq!(body, *blob, "chunked passthrough must be byte-identical");
    assert_no_token_leak(&seen, &token);
}
