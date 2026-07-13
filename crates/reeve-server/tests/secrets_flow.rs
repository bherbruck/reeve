//! End-to-end secrets vault flow (C7, REV-009): operator write-only
//! API -> device resolve -> rotation propagation through the render
//! pipeline, over the real router.
//!
//! Spec sources: spec/reeve/10-secrets.md §12.2 (write-only storage),
//! §12.3 (device-scoped resolve), §12.4 (rotation bumps
//! secrets_version + manifestVersion with an unchanged bundle),
//! §12.6 (device isolation, omit-not-error); docs/decisions/secrets.md
//! D15.

#![cfg(feature = "ext-secrets")]

use std::path::Path as FsPath;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use http_body_util::BodyExt as _;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};
use tower::ServiceExt as _;

use reeve_server::config::{AuthMode, Config};
use reeve_server::{auth, device_tokens, router, state::AppState};
use reeve_types::reeve::manifest::StateManifest;
use reeve_types::reeve::secrets::SECRETS_RESOLVE_PATH;

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

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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

fn put_secret(name: &str, scope: &str, value: &str) -> Request<Body> {
    Request::put("/api/secrets")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "name": name, "scope": scope, "value": value }).to_string(),
        ))
        .unwrap()
}

fn resolve_as(token: &str, names: &[&str]) -> Request<Body> {
    Request::post(SECRETS_RESOLVE_PATH)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "secrets": names }).to_string()))
        .unwrap()
}

fn get_manifest(token: &str) -> Request<Body> {
    Request::get("/api/reeve/v1/manifest")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn put_files(uri: &str, files: &[(&str, &str)]) -> Request<Body> {
    let files: serde_json::Map<String, Value> = files
        .iter()
        .map(|(p, c)| ((*p).to_string(), Value::String(B64.encode(c))))
        .collect();
    Request::put(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "files": files }).to_string()))
        .unwrap()
}

// ------------------------------------------------------------ fixtures

/// Same valid compose package shape delivery_flow.rs pins.
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
parameters:
  greeting:
    value: hello
    targets:
      - pointer: ENV.GREETING
        components: [\"web-stack\"]
";

const PKG_COMPOSE: &str = "\
services:
  web:
    image: ${REEVE_REGISTRY}/nginx:1.25
";

/// Author the web app at the fleet layer (every device gets it) and
/// override its `greeting` parameter at site plant-a with a secret
/// REFERENCE (§12.1: the reference string is the value; desired-state
/// passes it through verbatim, bundles stay secret-free).
async fn author(app: &Router) {
    let (status, _) = send_json(
        app,
        put_files(
            "/api/tree/packages/web/1.0.0",
            &[("margo.yaml", PKG_MANIFEST), ("compose.yml", PKG_COMPOSE)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send_json(
        app,
        put_files(
            "/api/tree/layers/00-all",
            &[("apps/web/app.yaml", "package:\n  name: web\n  version: 1.0.0\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send_json(
        app,
        put_files(
            "/api/tree/layers/20-site.plant-a",
            &[(
                "apps/web/params.yaml",
                "greeting: \"${secret:db-password}\"\n",
            )],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

fn parse_manifest(bytes: &[u8]) -> StateManifest {
    serde_json::from_slice(bytes).expect("StateManifest body")
}

// --------------------------------------------------------------- tests

/// §12.2: the operator API is write-only. Set -> version 1, rotate ->
/// version 2, metadata list carries no values, and NO route reads a
/// value back (not even to admin).
#[tokio::test]
async fn operator_api_is_write_only() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _state) = app(dir.path());

    let (status, body) = send_json(&app, put_secret("db-password", "fleet", "hunter2")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], 1);
    assert!(
        !body.to_string().contains("hunter2"),
        "PUT response must not echo the value"
    );

    // Rotate: same (name, scope) => version bump (§12.4).
    let (status, body) = send_json(&app, put_secret("db-password", "fleet", "sw0rdfish")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], 2);

    let (status, body) = send_json(&app, put_secret("api-key", "site.plant-a", "k1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], 1);

    // List: metadata only — never values.
    let (status, body) = send_json(
        &app,
        Request::get("/api/secrets").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secrets = body["secrets"].as_array().unwrap();
    assert_eq!(secrets.len(), 2);
    assert_eq!(secrets[0]["name"], "api-key");
    assert_eq!(secrets[0]["scope"], "site.plant-a");
    assert_eq!(secrets[1]["name"], "db-password");
    assert_eq!(secrets[1]["version"], 2);
    assert!(secrets[1]["rotated_at"].is_i64());
    let text = body.to_string();
    for value in ["hunter2", "sw0rdfish", "k1"] {
        assert!(!text.contains(value), "value {value:?} leaked into list");
    }

    // No read-back route exists: GET on the item path is 405 (the path
    // only serves DELETE), not a value.
    let (status, _, bytes) = send(
        &app,
        Request::get("/api/secrets/fleet/db-password")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(!String::from_utf8_lossy(&bytes).contains("sw0rdfish"));

    // Delete, idempotence surfaced as 404.
    let del = || {
        Request::delete("/api/secrets/site.plant-a/api-key")
            .body(Body::empty())
            .unwrap()
    };
    let (status, _) = send_json(&app, del()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send_json(&app, del()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Validation: bad scope / bad name are 422.
    let (status, _) = send_json(&app, put_secret("x", "galaxy", "v")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = send_json(&app, put_secret("has space", "fleet", "v")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// §12.3/§12.6: a device resolves down ITS OWN chain (deeper wins),
/// another device's device-scoped secrets and the reserved internal
/// scope are invisible, and unknown names are omitted — never an
/// error. Unauthenticated resolve is 401.
#[tokio::test]
async fn resolve_is_device_scoped_and_omits_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let tok_a = add_device(&state, "dev-a", Some("plant-a"));
    let tok_b = add_device(&state, "dev-b", Some("plant-b"));

    for (name, scope, value) in [
        ("db-password", "fleet", "fleet-val"),
        ("db-password", "site.plant-a", "site-a-val"),
        ("only-b", "device.dev-b", "b-val"),
        ("op-secret", "reeve-internal", "internal-val"),
    ] {
        let (status, _) = send_json(&app, put_secret(name, scope, value)).await;
        assert_eq!(status, StatusCode::OK, "{name}@{scope}");
    }

    // dev-a: site scope beats fleet; dev-b's secret + internal scope +
    // unknown names are ABSENT (indistinguishable by design, §12.6).
    let (status, body) = send_json(
        &app,
        resolve_as(&tok_a, &["db-password", "only-b", "op-secret", "nope"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["secrets"]["db-password"]["value"], "site-a-val");
    assert_eq!(body["secrets"]["db-password"]["version"], 1);
    let map = body["secrets"].as_object().unwrap();
    assert_eq!(map.len(), 1, "everything else omitted: {map:?}");

    // dev-b: falls through to fleet; sees its own device-scoped secret.
    let (status, body) = send_json(&app, resolve_as(&tok_b, &["db-password", "only-b"])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["secrets"]["db-password"]["value"], "fleet-val");
    assert_eq!(body["secrets"]["only-b"]["value"], "b-val");

    // No device credential => 401 (device_auth), no oracle.
    let (status, _, _) = send(
        &app,
        Request::post(SECRETS_RESOLVE_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "secrets": ["db-password"] }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// §12.4 rotation propagation: rotating a secret bumps the
/// manifestVersion and per-app secretsVersion of exactly the devices
/// whose rendered apps REFERENCE it — with the bundle digest
/// unchanged (no re-pull); non-referencing devices are untouched
/// (same ETag, 304 on conditional poll).
#[tokio::test]
async fn rotation_bumps_only_referencing_devices() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    // dev-1 sits at plant-a (gets the ${secret:db-password} override);
    // dev-2 at plant-b runs the same app with the plain default value.
    let tok_1 = add_device(&state, "dev-1", Some("plant-a"));
    let tok_2 = add_device(&state, "dev-2", Some("plant-b"));

    author(&app).await;

    // Create the secret: dev-1's app references it, so the CREATE
    // itself bumps dev-1 (hash goes from empty-resolution to v1).
    let (status, _) = send_json(&app, put_secret("db-password", "fleet", "hunter2")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, h1, b1) = send(&app, get_manifest(&tok_1)).await;
    assert_eq!(status, StatusCode::OK);
    let m1 = parse_manifest(&b1);
    let sv1 = m1.apps[0].secrets_version.clone().expect("referencing app carries secretsVersion");
    let bundle1 = m1.bundle.clone().expect("bundle").digest;
    let etag1 = h1[header::ETAG].to_str().unwrap().to_string();

    let (status, h2, b2) = send(&app, get_manifest(&tok_2)).await;
    assert_eq!(status, StatusCode::OK);
    let m2 = parse_manifest(&b2);
    assert_eq!(
        m2.apps[0].secrets_version, None,
        "non-referencing app has no secretsVersion"
    );
    let etag2 = h2[header::ETAG].to_str().unwrap().to_string();

    // Rotate (§12.4): PUT the same (name, scope) with a new value.
    let (status, body) = send_json(&app, put_secret("db-password", "fleet", "sw0rdfish")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], 2);

    // dev-1: manifestVersion bumped, secretsVersion changed, bundle
    // digest UNCHANGED (no bytes changed — no re-pull, §12.4).
    let (status, _, b1r) = send(&app, get_manifest(&tok_1)).await;
    assert_eq!(status, StatusCode::OK);
    let m1r = parse_manifest(&b1r);
    assert!(
        m1r.manifest_version.0 > m1.manifest_version.0,
        "rotation must bump the referencing device's manifestVersion"
    );
    let sv1r = m1r.apps[0].secrets_version.clone().expect("still referencing");
    assert_ne!(sv1r, sv1, "secretsVersion must change on rotate");
    assert_eq!(
        m1r.bundle.expect("bundle").digest,
        bundle1,
        "bundle digest unchanged: rotation is not a re-render of bytes"
    );

    // dev-2: completely untouched — conditional poll still 304s.
    let req = Request::get("/api/reeve/v1/manifest")
        .header(header::AUTHORIZATION, format!("Bearer {tok_2}"))
        .header(header::IF_NONE_MATCH, etag2.clone())
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED, "non-referencing device must not bump");

    // dev-1's old ETag no longer matches (its manifest moved).
    let req = Request::get("/api/reeve/v1/manifest")
        .header(header::AUTHORIZATION, format!("Bearer {tok_1}"))
        .header(header::IF_NONE_MATCH, etag1)
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);

    // The rendered artifacts never contain a value — the deployment
    // carries the reference string only (D15 "by construction").
    let (_, _, oci) = send(
        &app,
        Request::get(format!("/v2/reeve/bundles/dev-1/manifests/{bundle1}"))
            .header(header::AUTHORIZATION, format!("Bearer {tok_1}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(!String::from_utf8_lossy(&oci).contains("sw0rdfish"));

    // Deleting the secret bumps the referencing device again (its
    // references stop resolving — agents get told, §12.4).
    let (status, _) = send_json(
        &app,
        Request::delete("/api/secrets/fleet/db-password")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, b1d) = send(&app, get_manifest(&tok_1)).await;
    let m1d = parse_manifest(&b1d);
    assert!(m1d.manifest_version.0 > m1r.manifest_version.0);
    assert_ne!(m1d.apps[0].secrets_version, Some(sv1r));
}

/// Law 3 chaos: a vault write whose propagating render pass never ran
/// (kill -9 right after the transaction committed) is healed by
/// startup reconcile — the write and the render-dirty flag are one
/// transaction, and bootstrap runs the pass.
#[tokio::test]
async fn killed_propagation_is_healed_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let before = {
        let (app, state) = app(dir.path());
        let tok = add_device(&state, "dev-1", Some("plant-a"));
        author(&app).await;
        let (status, _, bytes) = send(&app, get_manifest(&tok)).await;
        assert_eq!(status, StatusCode::OK);
        let m = parse_manifest(&bytes);

        // Simulate the kill: write the secret EXACTLY as put_route's
        // transaction does (vault row + dirty flag) but never render.
        let key = reeve_server::ext::secrets::vault_key(&state.cfg.data_dir).unwrap();
        let conn = state.db.lock().unwrap();
        reeve_server::ext::secrets::put(&conn, &key, "db-password", "fleet", "hunter2").unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            params![reeve_server::render::RENDER_DIRTY_KEY],
        )
        .unwrap();
        m.manifest_version.0
        // process "dies": app + state dropped here
    };

    // Restart: bootstrap's reconcile sees the dirty flag and renders.
    let (app, state) = app(dir.path());
    let conn = state.db.lock().unwrap();
    let (mv, sv): (i64, Option<String>) = conn
        .query_row(
            "SELECT manifest_version, secrets_digest FROM device_manifests WHERE device_id = 'dev-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    drop(conn);
    drop(app);
    assert!(
        (mv as u64) > before,
        "startup reconcile must propagate the orphaned secrets write"
    );
    assert!(sv.is_some(), "secrets_digest recorded after the healing pass");
    let dirty: Option<String> = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![reeve_server::render::RENDER_DIRTY_KEY],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(dirty, None, "flag cleared once the pass completed");
}

/// rev-009/1 is advertised iff compiled in (01-framework §3.5).
#[tokio::test]
async fn capabilities_advertise_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let tok = add_device(&state, "dev-1", None);
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
        exts.iter().any(|e| e == "rev-009/1"),
        "rev-009/1 missing from {exts:?}"
    );
}
