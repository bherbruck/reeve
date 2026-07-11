//! End-to-end enrollment flow over the real router (docs/decisions/
//! agent.md D4): join-token management (operator surface, role-gated)
//! -> POST /api/reeve/v1/enroll -> the issued device token
//! authenticates against the device token store.

use std::path::Path;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use device_api::DeviceTokenStore as _;
use reeve_server::config::{AuthMode, Config};
use reeve_server::device_tokens::SqliteDeviceTokenStore;
use reeve_server::{auth, router, state::AppState};

fn config(data_dir: &Path, auth: AuthMode) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth,
        session_ttl_secs: 3600,
        registry_endpoint: "registry.example:5000".to_string(),
        durability: reeve_server::config::DurabilityConfig::disabled(),
        zot: None,
        federation: None,
        install_open: false,
    }
}

fn app(cfg: Config) -> (Router, AppState) {
    let state = reeve_server::bootstrap(cfg).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

fn json_post(uri: &str, body: Value) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Full flow under REEVE_AUTH=none (anonymous acts as admin, D1):
/// create join token -> enroll -> device token authenticates -> list
/// shows the consumed use -> revoke.
#[tokio::test]
async fn join_token_then_enroll_then_device_token_works() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(config(dir.path(), AuthMode::None));

    // operator creates a join token (defaults: 24h, 1 use)
    let (status, body) = send(&app, json_post("/api/join-tokens", json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let join_token = body["join_token"].as_str().unwrap().to_string();
    assert!(join_token.starts_with("rvj_"));
    assert_eq!(body["max_uses"], 1);
    let hash = body["token_hash"].as_str().unwrap().to_string();

    // device enrolls (D4 step 1)
    let (status, body) = send(
        &app,
        json_post(
            "/api/reeve/v1/enroll",
            json!({
                "join_token": join_token,
                "hostname": "edge-01",
                "arch": "aarch64",
                "agent_version": "0.1.0"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let device_id = body["device_id"].as_str().unwrap().to_string();
    let device_token = body["device_token"].as_str().unwrap().to_string();
    assert!(device_token.starts_with("rvd_"));

    // the issued credential authenticates (D1: the ONE token)
    let store = SqliteDeviceTokenStore::new(state.db.clone());
    assert_eq!(
        store
            .device_id_for_hash(&device_api::token_hash(&device_token))
            .unwrap(),
        Some(device_id.clone())
    );

    // list shows the consumed use
    let (status, body) = send(
        &app,
        Request::get("/api/join-tokens").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["uses"], 1);
    assert_eq!(body[0]["token_hash"], hash.as_str());

    // revoke is 204 and idempotent
    let (status, _) = send(
        &app,
        Request::delete(format!("/api/join-tokens/{hash}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// The enroll surface rejects a bogus token with 401 and never creates
/// state.
#[tokio::test]
async fn enroll_with_wrong_token_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(config(dir.path(), AuthMode::None));
    let (status, _) = send(
        &app,
        json_post(
            "/api/reeve/v1/enroll",
            json!({
                "join_token": "rvj_wrong",
                "hostname": "edge-01",
                "arch": "x86_64",
                "agent_version": "0.1.0"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Join-token management is role-gated (D4: admin/operator): anonymous
/// under password mode gets 401 — the operator surface never leaks to
/// the unauthenticated world.
#[tokio::test]
async fn join_token_management_requires_role() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(config(dir.path(), AuthMode::Password));

    let (status, _) = send(&app, json_post("/api/join-tokens", json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = send(
        &app,
        Request::get("/api/join-tokens").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = send(
        &app,
        Request::delete("/api/join-tokens/deadbeef")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Viewers are authenticated but below operator: 403.
#[tokio::test]
async fn viewer_cannot_manage_join_tokens() {
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;

    let dir = tempfile::tempdir().unwrap();
    let proxy = AuthMode::Proxy(reeve_server::config::ProxyConfig {
        user_header: "remote-user".into(),
        role_header: Some("remote-role".into()),
        trusted: vec!["10.0.0.0/8".parse().unwrap()],
    });
    let (app, _) = app(config(dir.path(), proxy));

    let mut req = json_post("/api/join-tokens", json!({}));
    req.headers_mut()
        .insert("remote-user", "eve".parse().unwrap());
    req.headers_mut()
        .insert("remote-role", "viewer".parse().unwrap());
    req.extensions_mut()
        .insert(ConnectInfo("10.1.2.3:5555".parse::<SocketAddr>().unwrap()));
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Re-enroll token creation validates the device exists.
#[tokio::test]
async fn reenroll_token_for_unknown_device_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(config(dir.path(), AuthMode::None));
    let (status, _) = send(
        &app,
        json_post("/api/join-tokens", json!({"device_id": "dev-nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
