//! End-to-end deploy-log flow (REV-011, ext-logs) over the real
//! router: agent uploads its own compose output (device auth), operator
//! lists + reads it (viewer+), device isolation on upload, retention,
//! and the oversized-body cap.
//!
//! Margo compliance: these are purely additive reeve endpoints — the
//! one-line failure reason still lives in the Margo status body, which
//! this feature never touches (spec/reeve/01-framework.md §3.1).

#![cfg(feature = "ext-logs")]

use std::path::Path as FsPath;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use rusqlite::params;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use reeve_server::config::{AuthMode, Config};
use reeve_server::{auth, device_tokens, router, state::AppState};

// ------------------------------------------------------------- harness

fn config(data_dir: &FsPath) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth: AuthMode::None, // anonymous acts as admin (D1)
        session_ttl_secs: 3600,
        tier: reeve_server::config::ServerTier::Root,
        registry_endpoint: "registry.example:5000".to_string(),
        durability: reeve_server::config::DurabilityConfig::disabled(),
        zot: None,
        federation: None,
        install_open: false,
        logs_retain_per_deployment: 3,
        admin_seed: None,
    }
}

fn app(dir: &FsPath) -> (Router, AppState) {
    let state = reeve_server::bootstrap(config(dir)).expect("bootstrap");
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

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, header::HeaderMap, Vec<u8>) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

async fn send_json(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let (status, _, bytes) = send(app, req).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn upload_req(token: &str, device_id: &str, body: Value) -> Request<Body> {
    Request::post(format!("/api/reeve/v1/devices/{device_id}/logs"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn upload_body(deployment: &str, outcome: &str, phase: &str, text: &str) -> Value {
    json!({
        "deploymentId": deployment,
        "appId": "web",
        "outcome": outcome,
        "phase": phase,
        "exitCode": if outcome == "failed" { json!(1) } else { Value::Null },
        "truncated": false,
        "capturedAt": "2026-07-13T10:00:00Z",
        "text": text,
    })
}

// --------------------------------------------------------------- tests

/// Full round trip: a device uploads its own failed-compose log; an
/// operator lists it (newest first) and reads the full text back.
#[tokio::test]
async fn upload_list_read_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let tok = add_device(&state, "dev-1");

    let (status, body) = send_json(
        &app,
        upload_req(
            &tok,
            "dev-1",
            upload_body("web-deploy", "failed", "up", "Error: pull access denied\n"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = body["id"].as_str().unwrap().to_string();
    assert!(!id.is_empty());

    // List (viewer+; anonymous is admin here).
    let (status, body) = send_json(
        &app,
        Request::get("/api/devices/dev-1/logs?deployment=web-deploy")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let logs = body["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["id"], id);
    assert_eq!(logs[0]["outcome"], "failed");
    assert_eq!(logs[0]["phase"], "up");
    assert_eq!(logs[0]["appId"], "web");
    assert_eq!(logs[0]["sizeBytes"], 26);
    // Meta list carries no body text.
    assert!(logs[0].get("text").is_none());

    // Read one (JSON meta+text).
    let (status, body) = send_json(
        &app,
        Request::get(format!("/api/devices/dev-1/logs/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["meta"]["id"], id);
    assert_eq!(body["text"], "Error: pull access denied\n");

    // Read one as text/plain.
    let (status, headers, bytes) = send(
        &app,
        Request::get(format!("/api/devices/dev-1/logs/{id}"))
            .header(header::ACCEPT, "text/plain")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE].to_str().unwrap().contains("text/plain"));
    assert_eq!(bytes, b"Error: pull access denied\n");

    // Unknown log id => 404.
    let (status, _, _) = send(
        &app,
        Request::get("/api/devices/dev-1/logs/deadbeef")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A device can only upload its OWN logs: the path device_id must match
/// the token, and an unauthenticated upload is 401.
#[tokio::test]
async fn upload_requires_matching_device_auth() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let tok_a = add_device(&state, "dev-a");
    let _tok_b = add_device(&state, "dev-b");

    // dev-a's token uploading as dev-b => 403.
    let (status, _, _) = send(
        &app,
        upload_req(&tok_a, "dev-b", upload_body("d", "failed", "up", "x")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // No credential => 401 (device_auth layer).
    let (status, _, _) = send(
        &app,
        Request::post("/api/reeve/v1/devices/dev-a/logs")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(upload_body("d", "failed", "up", "x").to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Matching device => OK.
    let (status, _) = send_json(
        &app,
        upload_req(&tok_a, "dev-a", upload_body("d", "failed", "up", "x")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Retention keeps only the most recent N (config = 3) per
/// (device, deployment); older uploads are pruned.
#[tokio::test]
async fn retention_bounds_stored_logs() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let tok = add_device(&state, "dev-1");

    for i in 0..6 {
        let (status, _) = send_json(
            &app,
            upload_req(
                &tok,
                "dev-1",
                upload_body("web-deploy", "failed", "up", &format!("run-{i}\n")),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, body) = send_json(
        &app,
        Request::get("/api/devices/dev-1/logs?deployment=web-deploy")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let logs = body["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 3, "retention keeps the most recent 3");
}

/// Oversized bodies are rejected (413) by the per-route body cap, and
/// malformed JSON is 422.
#[tokio::test]
async fn oversized_and_malformed_bodies_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let tok = add_device(&state, "dev-1");

    // > 512 KiB body => 413.
    let big = "x".repeat(600 * 1024);
    let (status, _, _) = send(
        &app,
        upload_req(&tok, "dev-1", upload_body("d", "failed", "up", &big)),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    // Malformed body => 422.
    let (status, _, _) = send(
        &app,
        Request::post("/api/reeve/v1/devices/dev-1/logs")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// rev-011/1 is advertised iff compiled in (spec/reeve/01-framework.md §3.3).
#[tokio::test]
async fn capabilities_advertise_deploy_logs() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let tok = add_device(&state, "dev-1");
    let (status, body) = send_json(
        &app,
        Request::get("/api/reeve/v1/capabilities")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let exts = body["extensions"].as_array().unwrap();
    assert!(
        exts.iter().any(|e| e == "rev-011/1"),
        "rev-011/1 missing from {exts:?}"
    );
}
