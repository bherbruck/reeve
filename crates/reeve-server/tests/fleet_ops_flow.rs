//! Deploy-to-scope + History/Undo end-to-end (REV-010 §11.4/§11.5) over
//! the real router: a deploy authors the right layer and re-renders only
//! the scope's devices; undeploy removes it; History carries human
//! summaries; Undo restores prior content as a NEW change.

use std::path::Path as FsPath;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
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
        admin_seed: None,
        logs_retain_per_deployment: 10,
    }
}

fn app(dir: &FsPath) -> (Router, AppState) {
    let state = reeve_server::bootstrap(config(dir)).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state)
}

fn add_device(state: &AppState, id: &str, site: Option<&str>) -> String {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at, site)
         VALUES (?1, 'box', 'x86_64', '0.1.0', 0, ?2)",
        params![id, site],
    )
    .unwrap();
    device_tokens::issue(&conn, id).unwrap()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn post_json(uri: &str, body: &Value) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

/// PUT a subtree (base64 files) — the tree authoring path.
async fn put_files(app: &Router, uri: &str, files: &[(&str, &str)]) {
    let files: serde_json::Map<String, Value> = files
        .iter()
        .map(|(p, c)| ((*p).to_string(), Value::String(B64.encode(c))))
        .collect();
    let req = Request::put(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "files": files }).to_string()))
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK, "PUT {uri}: {body}");
}

const PKG_MANIFEST: &str = "\
apiVersion: margo.org/v1-alpha1
kind: ApplicationDescription
metadata:
  id: web
  name: Web
  version: 1.0.0
  catalog:
    organization:
      - name: Reeve Tests
        site: https://example.com
deploymentProfiles:
  - type: compose
    id: web-compose
    components:
      - name: web-stack
        properties:
          packageLocation: ./compose.yml
";

const PKG_COMPOSE: &str = "\
services:
  web:
    image: ${REEVE_REGISTRY}/nginx:1.25
";

async fn vendor_package(app: &Router) {
    put_files(
        app,
        "/api/tree/packages/web/1.0.0",
        &[("margo.yaml", PKG_MANIFEST), ("compose.yml", PKG_COMPOSE)],
    )
    .await;
}

// -------------------------------------------------------- state probes

fn content_digest(state: &AppState, device: &str) -> Option<String> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT content_digest FROM device_manifests WHERE device_id = ?1",
        params![device],
        |r| r.get(0),
    )
    .ok()
}

/// Deployed app ids in a device's current State Manifest.
fn manifest_apps(state: &AppState, device: &str) -> Vec<String> {
    let conn = state.db.lock().unwrap();
    let json: Option<String> = conn
        .query_row(
            "SELECT manifest_json FROM device_manifests WHERE device_id = ?1",
            params![device],
            |r| r.get(0),
        )
        .ok();
    json.and_then(|j| serde_json::from_str::<Value>(&j).ok())
        .and_then(|v| v.get("apps").and_then(|a| a.as_array()).cloned())
        .map(|apps| {
            apps.iter()
                .filter_map(|e| e.get("appId").and_then(|s| s.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

async fn deploy(app: &Router, stack: Value, scope: Value) -> (StatusCode, Value) {
    send(app, post_json("/api/deploy", &json!({ "stack": stack, "scope": scope }))).await
}

async fn undeploy(app: &Router, stack: Value, scope: Value) -> (StatusCode, Value) {
    send(app, post_json("/api/undeploy", &json!({ "stack": stack, "scope": scope }))).await
}

fn web_stack() -> Value {
    json!({ "package": "web", "version": "1.0.0" })
}

// --------------------------------------------------------------- tests

/// §11.4: deploy to a site authors that site's layer and re-renders only
/// the site's devices — the deploy is framed as "Site plant-a", never a
/// layer path.
#[tokio::test]
async fn deploy_to_site_authors_layer_and_renders_only_its_devices() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", Some("plant-a"));
    add_device(&state, "d2", Some("plant-b"));
    vendor_package(&app).await;
    // After vendoring, every device is rendered with zero apps.
    let d1_before = content_digest(&state, "d1");
    let d2_before = content_digest(&state, "d2");

    let (status, body) = deploy(&app, web_stack(), json!({ "kind": "site", "name": "plant-a" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["app"], "web");
    assert_eq!(body["scope"], "Site plant-a");
    assert_eq!(body["changed"], true);

    // Right layer authored (proves it went to 20-site.plant-a, not a
    // device or the base).
    let (status, rev) = send(&app, get(&format!("/api/tree/revisions/{}", body["revision"]))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        rev["files"]
            .as_object()
            .unwrap()
            .contains_key("layers/20-site.plant-a/apps/web/app.yaml"),
        "authored into the site layer: {rev}"
    );

    // Only the site's device got the app; the other is unchanged.
    assert_eq!(manifest_apps(&state, "d1"), ["web"]);
    assert!(manifest_apps(&state, "d2").is_empty());
    assert_ne!(content_digest(&state, "d1"), d1_before, "d1 re-rendered");
    assert_eq!(content_digest(&state, "d2"), d2_before, "d2 untouched");
}

/// §11.4: a devices-scoped deploy authors each device's own layer.
#[tokio::test]
async fn deploy_to_devices_scope_targets_each_device() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", None);
    add_device(&state, "d2", None);
    add_device(&state, "d3", None);
    vendor_package(&app).await;

    let (status, body) =
        deploy(&app, web_stack(), json!({ "kind": "devices", "ids": ["d1", "d2"] })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scope"], "2 devices");

    assert_eq!(manifest_apps(&state, "d1"), ["web"]);
    assert_eq!(manifest_apps(&state, "d2"), ["web"]);
    assert!(manifest_apps(&state, "d3").is_empty());

    // Unknown device in a devices scope is a 422.
    let (status, _) =
        deploy(&app, web_stack(), json!({ "kind": "devices", "ids": ["ghost"] })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// §11.4: undeploy is the same call removing the app from the scope;
/// idempotent (a second undeploy is a no-op change).
#[tokio::test]
async fn undeploy_removes_from_scope() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", Some("plant-a"));
    vendor_package(&app).await;
    deploy(&app, web_stack(), json!({ "kind": "site", "name": "plant-a" })).await;
    assert_eq!(manifest_apps(&state, "d1"), ["web"]);

    let (status, body) = undeploy(&app, web_stack(), json!({ "kind": "site", "name": "plant-a" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true);
    assert!(manifest_apps(&state, "d1").is_empty(), "app removed from the site");

    // Idempotent: nothing left to remove.
    let (status, body) = undeploy(&app, web_stack(), json!({ "kind": "site", "name": "plant-a" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changed"], false, "second undeploy is a no-op");
}

/// §11.5: History carries human summaries (never "revision N"); the
/// detail lists changed apps + scopes, not raw paths.
#[tokio::test]
async fn history_has_human_summaries_and_scoped_detail() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", Some("plant-a"));
    vendor_package(&app).await;
    let (_, dep) = deploy(&app, web_stack(), json!({ "kind": "site", "name": "plant-a" })).await;

    let (status, hist) = send(&app, get("/api/history")).await;
    assert_eq!(status, StatusCode::OK);
    let list = hist.as_array().unwrap();
    // Newest first: the deploy.
    assert_eq!(list[0]["summary"], "deployed web to Site plant-a");
    assert_eq!(list[0]["who"], "anonymous");
    assert!(list[0]["when"].as_str().is_some());

    let id = dep["revision"].as_i64().unwrap();
    let (status, detail) = send(&app, get(&format!("/api/history/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["summary"], "deployed web to Site plant-a");
    let change = &detail["changes"][0];
    assert_eq!(change["change"], "deployed");
    assert_eq!(change["app"], "web");
    assert_eq!(change["scope"], json!({ "kind": "site", "name": "plant-a" }));
    assert_eq!(change["scopeLabel"], "Site plant-a");

    // Unknown change is a 404.
    let (status, _) = send(&app, get("/api/history/99999")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// §11.5: Undo authors a NEW change restoring the content as of before
/// the undone one — the deploy is reverted, and manifestVersion climbs.
#[tokio::test]
async fn undo_restores_prior_content_as_a_new_change() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", None);
    vendor_package(&app).await;
    let before = content_digest(&state, "d1");

    let (_, dep) = deploy(&app, web_stack(), json!({ "kind": "all" })).await;
    let deploy_rev = dep["revision"].as_i64().unwrap();
    assert_eq!(manifest_apps(&state, "d1"), ["web"], "deployed");

    let (status, undo) = send(&app, post_json(&format!("/api/history/{deploy_rev}/undo"), &json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{undo}");
    assert_eq!(undo["changed"], true);
    let new_rev = undo["revision"].as_i64().unwrap();
    assert!(new_rev > deploy_rev, "undo is a NEW change on top, never a rewind");

    // Content restored to before the deploy.
    assert!(manifest_apps(&state, "d1").is_empty(), "undo removed the app");
    assert_eq!(content_digest(&state, "d1"), before, "content restored");

    // The undo itself shows up in History as the newest change.
    let (_, hist) = send(&app, get("/api/history")).await;
    let list = hist.as_array().unwrap();
    assert_eq!(list[0]["id"].as_i64().unwrap(), new_rev);
}
