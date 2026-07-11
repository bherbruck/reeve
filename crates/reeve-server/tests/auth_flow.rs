//! End-to-end auth-mode tests over the real router (docs/decisions/
//! auth.md D1): password login/session flow incl. first-boot setup,
//! proxy trusted-CIDR enforcement, none-mode anonymous-admin.

use std::net::SocketAddr;
use std::path::Path;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use reeve_server::config::{AuthMode, Config};
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

/// Bootstrap a real AppState + router on a temp DB.
fn app(cfg: Config) -> (Router, AppState, auth::BootstrapReport) {
    let state = reeve_server::bootstrap(cfg).expect("bootstrap");
    let report = auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state, report)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<(String, String)>, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers: Vec<(String, String)> = res
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, body)
}

fn json_post(uri: &str, body: Value) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn cookie_from(headers: &[(String, String)]) -> String {
    let set_cookie = headers
        .iter()
        .find(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.clone())
        .expect("set-cookie header");
    set_cookie.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn healthz_needs_no_auth() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _, _) = app(config(dir.path(), AuthMode::Password));
    let (status, _, body) = send(&app, Request::get("/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn password_mode_first_boot_setup_then_login_flow() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _, report) = app(config(dir.path(), AuthMode::Password));
    let setup_token = report.setup_token.expect("first boot mints a setup token");

    // anonymous before any login
    let (status, _, body) = send(
        &app,
        Request::get("/api/auth/me").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "anonymous");
    assert_eq!(body["effectiveRole"], Value::Null, "password-mode anonymous has no role");

    // wrong setup token refused
    let (status, _, _) = send(
        &app,
        json_post(
            "/api/auth/setup",
            json!({"setup_token": "rvs_wrong", "username": "admin", "password": "pw"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // correct setup token creates admin and logs in
    let (status, headers, body) = send(
        &app,
        json_post(
            "/api/auth/setup",
            json!({"setup_token": setup_token, "username": "admin", "password": "s3cret!"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["role"], "admin");
    let setup_cookie = cookie_from(&headers);

    // setup token is single-use
    let (status, _, _) = send(
        &app,
        json_post(
            "/api/auth/setup",
            json!({"setup_token": setup_token, "username": "admin2", "password": "pw"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // the setup session works
    let (status, _, body) = send(
        &app,
        Request::get("/api/auth/me")
            .header(header::COOKIE, &setup_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "human");
    assert_eq!(body["user"], "admin");
    assert_eq!(body["effectiveRole"], "admin");

    // wrong password refused
    let (status, _, _) = send(
        &app,
        json_post("/api/auth/login", json!({"username": "admin", "password": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // proper login issues a session cookie
    let (status, headers, _) = send(
        &app,
        json_post("/api/auth/login", json!({"username": "admin", "password": "s3cret!"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cookie = cookie_from(&headers);

    // logout kills the session
    let (status, _, _) = send(
        &app,
        Request::post("/api/auth/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, body) = send(
        &app,
        Request::get("/api/auth/me")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["kind"], "anonymous", "logged-out session must not authenticate");
}

#[tokio::test]
async fn password_mode_restart_with_users_mints_no_setup_token() {
    let dir = tempfile::tempdir().unwrap();
    let (app_a, _, report) = app(config(dir.path(), AuthMode::Password));
    let token = report.setup_token.unwrap();
    let (status, _, _) = send(
        &app_a,
        json_post(
            "/api/auth/setup",
            json!({"setup_token": token, "username": "admin", "password": "pw"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // "restart": bootstrap again on the same data dir (crash-only: startup
    // is recovery, idempotent)
    let (_, _, report2) = app(config(dir.path(), AuthMode::Password));
    assert!(report2.setup_token.is_none(), "users exist => no setup window");
}

fn proxy_mode() -> AuthMode {
    AuthMode::Proxy(reeve_server::config::ProxyConfig {
        user_header: "remote-user".into(),
        role_header: Some("remote-role".into()),
        trusted: vec!["10.0.0.0/8".parse().unwrap()],
    })
}

fn with_peer(mut req: Request<Body>, peer: &str) -> Request<Body> {
    let addr: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

#[tokio::test]
async fn proxy_mode_enforces_trusted_cidr() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _, _) = app(config(dir.path(), proxy_mode()));

    // trusted peer, user header => Human with role from role header
    let req = with_peer(
        Request::get("/api/auth/me")
            .header("remote-user", "alice")
            .header("remote-role", "operator")
            .body(Body::empty())
            .unwrap(),
        "10.1.2.3:5555",
    );
    let (status, _, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "human");
    assert_eq!(body["user"], "alice");
    assert_eq!(body["role"], "operator");

    // trusted peer, no role header => admin (proxy gates access)
    let req = with_peer(
        Request::get("/api/auth/me")
            .header("remote-user", "bob")
            .body(Body::empty())
            .unwrap(),
        "10.1.2.3:5555",
    );
    let (_, _, body) = send(&app, req).await;
    assert_eq!(body["role"], "admin");

    // untrusted peer => 401 even with the header
    let req = with_peer(
        Request::get("/api/auth/me")
            .header("remote-user", "mallory")
            .body(Body::empty())
            .unwrap(),
        "203.0.113.9:4444",
    );
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // no peer address at all => 401 (fail closed)
    let req = Request::get("/api/auth/me")
        .header("remote-user", "mallory")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // login surface does not exist in proxy mode
    let req = with_peer(
        json_post("/api/auth/login", json!({"username": "a", "password": "b"})),
        "10.1.2.3:5555",
    );
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn none_mode_is_anonymous_admin_with_loud_warning() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state, report) = app(config(dir.path(), AuthMode::None));

    // the loud warning is a startup notice (main logs it at WARN)
    assert!(
        report.notices.iter().any(|n| n.contains("AUTH IS DISABLED")),
        "none mode must produce a loud startup warning, got {:?}",
        report.notices
    );

    let (status, _, body) = send(
        &app,
        Request::get("/api/auth/me").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "anonymous");
    assert_eq!(body["effectiveRole"], "admin");

    // and the mode-aware policy agrees
    assert_eq!(
        state.effective_role(&device_api::Identity::Anonymous),
        Some(device_api::Role::Admin)
    );
}
