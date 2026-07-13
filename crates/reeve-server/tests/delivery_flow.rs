//! End-to-end desired-state delivery (C4): author -> render -> State
//! Manifest poll -> native OCI pull, over the real router.
//!
//! Spec sources: spec/reeve/08-packaging.md §10.2 (manifest poll, ETag
//! strong validator, per-device pull authorization),
//! docs/decisions/delivery.md D7/D13, docs/decisions/tree-render.md
//! D2/D3 (no-change re-render => no new bundle, no manifestVersion
//! bump; deterministic bundles).

use std::io::Read as _;
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
use reeve_types::reeve::manifest::{ManifestVersion, StateManifest, is_sha256_digest};
use revision_store::{Stream, digest_of};

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

/// Insert a device row (the enrollment flow is covered by
/// enroll_flow.rs; here the row + token are the fixture) and issue its
/// bearer token.
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

fn get_as(uri: &str, token: &str) -> Request<Body> {
    Request::get(uri)
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

/// Same valid compose package shape the desired-state table tests pin
/// (spec/margo application-description.linkml.yaml). The reference/
/// fixtures (nextcloud) carry a REMOTE packageLocation, which D11
/// correctly refuses to render (no fetch-at-render) — real_fixtures.rs
/// in margo-package covers parsing those; this one renders.
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

async fn author_web_app(app: &Router) {
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
}

fn parse_manifest(bytes: &[u8]) -> StateManifest {
    serde_json::from_slice(bytes).expect("StateManifest body")
}

fn etag_of(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::ETAG)
        .expect("ETag header")
        .to_str()
        .unwrap()
        .trim_matches('"')
        .to_string()
}

fn gunzip_untar(bytes: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut ar = tar::Archive::new(gz);
    let mut out = std::collections::BTreeMap::new();
    for entry in ar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).unwrap();
        out.insert(path, buf);
    }
    out
}

// --------------------------------------------------------------- tests

/// The whole §10.2 flow: author -> render (hooked on the commit) ->
/// manifest poll (ETag, 304) -> OCI pull (manifest, layer blob, config
/// blob), with digest verification at every hop like the agent does.
#[tokio::test]
async fn author_render_manifest_pull_flow() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1", Some("plant-a"));

    author_web_app(&app).await;

    // Manifest poll: device-scoped, ETag = manifest digest.
    let (status, headers, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    assert_eq!(status, StatusCode::OK);
    let etag = etag_of(&headers);
    assert!(is_sha256_digest(&etag), "ETag {etag:?} must be sha256:<hex>");
    assert_eq!(etag, digest_of(&body), "ETag is the digest of the manifest bytes");

    let manifest = parse_manifest(&body);
    assert_eq!(manifest.manifest_version.epoch(), 0);
    let bundle = manifest.bundle.clone().expect("bundle present with one app");
    assert_eq!(
        bundle.media_type.as_deref(),
        Some("application/vnd.reeve.render-bundle.v1+tar+gzip")
    );
    assert_eq!(bundle.url, "/v2/reeve/bundles/dev-1");
    assert!(is_sha256_digest(&bundle.digest));
    assert_eq!(manifest.apps.len(), 1);
    assert_eq!(manifest.apps[0].app_id, "web");
    assert_eq!(
        manifest.apps[0].deployment_id.as_deref(),
        Some(desired_state::deployment_id("dev-1", "web").to_string().as_str())
    );

    // Conditional GET: quoted, multi-value list, weak, mismatch.
    for (inm, expect) in [
        (format!("\"{etag}\""), StatusCode::NOT_MODIFIED),
        (
            format!("\"sha256:{}\", \"{etag}\"", "b".repeat(64)),
            StatusCode::NOT_MODIFIED,
        ),
        ("*".to_string(), StatusCode::NOT_MODIFIED),
        (format!("W/\"{etag}\""), StatusCode::OK), // strong compare (§10.2)
        (format!("\"sha256:{}\"", "b".repeat(64)), StatusCode::OK),
    ] {
        let req = Request::get("/api/reeve/v1/manifest")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::IF_NONE_MATCH, inm.clone())
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, expect, "If-None-Match: {inm}");
    }

    // OCI pull, exactly as the agent's B2 contract drives it: GET the
    // image manifest by bundle.digest, verify bytes, select the layer.
    let (status, headers, oci_bytes) = send(
        &app,
        get_as(
            &format!("{}/manifests/{}", bundle.url, bundle.digest),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(digest_of(&oci_bytes), bundle.digest, "manifest bytes verify");

    let oci: Value = serde_json::from_slice(&oci_bytes).unwrap();
    assert_eq!(oci["schemaVersion"], 2);
    let layer = &oci["layers"][0];
    assert_eq!(
        layer["mediaType"],
        "application/vnd.reeve.render-bundle.v1+tar+gzip"
    );
    let layer_digest = layer["digest"].as_str().unwrap();

    let (status, _, tarball) = send(
        &app,
        get_as(&format!("{}/blobs/{layer_digest}", bundle.url), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(digest_of(&tarball), layer_digest, "layer bytes verify");
    assert_eq!(layer["size"].as_u64().unwrap(), tarball.len() as u64);

    // The bundle IS the D2 layout, rendered for THIS device.
    let files = gunzip_untar(&tarball);
    assert!(files.contains_key("manifest.yaml"));
    assert!(files.contains_key("apps/web/deployment.yaml"));
    assert!(files.contains_key("apps/web/application.yaml"));
    let compose = String::from_utf8(files["apps/web/compose.yml"].clone()).unwrap();
    assert!(
        compose.contains("registry.example:5000/nginx:1.25"),
        "${{REEVE_REGISTRY}} resolved from server config (D8): {compose}"
    );

    // Config blob (OCI empty descriptor) is pullable too.
    let config_digest = oci["config"]["digest"].as_str().unwrap();
    let (status, _, cfg_bytes) = send(
        &app,
        get_as(&format!("{}/blobs/{config_digest}", bundle.url), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg_bytes, b"{}");

    // /v2/ base endpoint answers authorized devices.
    let (status, _, _) = send(&app, get_as("/v2/", &token)).await;
    assert_eq!(status, StatusCode::OK);
}

/// §10.7: a device pulls only the artifacts ITS OWN manifest
/// references. Another device's repo — or a foreign digest in your own
/// repo — is 404; anonymous is 401 everywhere.
#[tokio::test]
async fn cross_device_pull_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token1 = add_device(&state, "dev-1", None);
    let token2 = add_device(&state, "dev-2", None);

    author_web_app(&app).await;

    let (status, _, body) = send(&app, get_as("/api/reeve/v1/manifest", &token1)).await;
    assert_eq!(status, StatusCode::OK);
    let bundle = parse_manifest(&body).bundle.expect("bundle");

    // dev-2 hitting dev-1's repo: 404 (existence not confirmed).
    let uri = format!("{}/manifests/{}", bundle.url, bundle.digest);
    let (status, _, _) = send(&app, get_as(&uri, &token2)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // dev-2 asking its OWN repo for dev-1's digest: also 404 — dev-2's
    // bundle digest differs (device_id is a render input).
    let uri2 = format!("/v2/reeve/bundles/dev-2/manifests/{}", bundle.digest);
    let (status, _, _) = send(&app, get_as(&uri2, &token2)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Blob route enforces the same boundary.
    let (status, _, oci_bytes) = send(&app, get_as(&uri, &token1)).await;
    assert_eq!(status, StatusCode::OK);
    let oci: Value = serde_json::from_slice(&oci_bytes).unwrap();
    let layer_digest = oci["layers"][0]["digest"].as_str().unwrap();
    let (status, _, _) = send(
        &app,
        get_as(&format!("/v2/reeve/bundles/dev-1/blobs/{layer_digest}"), &token2),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Anonymous pull MUST NOT be enabled by default (§10.2).
    for uri in [
        "/v2/",
        "/api/reeve/v1/manifest",
        "/api/reeve/v1/capabilities",
        uri.as_str(),
    ] {
        let (status, _, _) = send(&app, Request::get(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} must require device auth");
    }
}

/// D3: a re-render with no changes produces no new bundle and no
/// manifestVersion bump — via the manual kick, an identical PUT, and a
/// commit that only touches ANOTHER device's layer (provenance-only
/// change for this device).
#[tokio::test]
async fn no_change_rerender_does_not_bump() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1", None);

    author_web_app(&app).await;

    let (_, headers, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    let v0 = parse_manifest(&body).manifest_version;
    let etag0 = etag_of(&headers);

    // Manual kick: everything unchanged.
    let (status, report) = send_json(
        &app,
        Request::post("/api/render").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["rendered"], 0, "{report}");
    assert_eq!(report["unchanged"], 1, "{report}");

    // Identical layer PUT: no new revision (D14), no bump.
    let (status, res) = send_json(
        &app,
        put_files(
            "/api/tree/layers/00-all",
            &[("apps/web/app.yaml", "package:\n  name: web\n  version: 1.0.0\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["changed"], false);

    // A commit touching only another device's layer: new revision, but
    // dev-1's rendered content is unchanged => no bump for dev-1.
    let (status, res) = send_json(
        &app,
        put_files(
            "/api/tree/layers/40-device.dev-other",
            &[("apps/web/params.yaml", "greeting: someone-else\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["changed"], true);

    let (_, headers, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    assert_eq!(parse_manifest(&body).manifest_version, v0, "no bump");
    assert_eq!(etag_of(&headers), etag0, "same manifest bytes");

    // A REAL change for dev-1 bumps by exactly one counter step and
    // changes the bundle digest.
    let (status, res) = send_json(
        &app,
        put_files(
            "/api/tree/layers/40-device.dev-1",
            &[("apps/web/params.yaml", "greeting: bumped\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["changed"], true);

    let (_, headers, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    let m1 = parse_manifest(&body);
    assert_eq!(m1.manifest_version.epoch(), 0);
    assert_eq!(m1.manifest_version.counter(), v0.counter() + 1);
    assert!(v0.accepts_successor(m1.manifest_version), "strictly greater (§10.2)");
    assert_ne!(etag_of(&headers), etag0);
}

/// Margo DeploymentBundleRef null rule: a device with zero apps gets a
/// manifest with `bundle: null` — present, never omitted — and the
/// first manifest version is 1 (epoch 0, counter 1). Also exercises
/// on-demand render for a device enrolled after the last pass.
#[tokio::test]
async fn zero_apps_serves_null_bundle_and_first_version_is_one() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    // No authoring at all; device row exists, no render pass has seen it.
    let token = add_device(&state, "dev-empty", None);

    let (status, _, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    assert_eq!(status, StatusCode::OK);
    let raw: Value = serde_json::from_slice(&body).unwrap();
    assert!(raw.get("bundle").is_some(), "bundle key present");
    assert!(raw["bundle"].is_null(), "zero apps => bundle null");
    let manifest = parse_manifest(&body);
    assert_eq!(manifest.manifest_version, ManifestVersion(1), "first manifest MUST use 1");
    assert!(manifest.apps.is_empty());
}

/// Determinism (D2/D3): re-rendering from scratch with identical
/// declared inputs — tree revision, device context, generation —
/// reproduces the SAME bundle digest, byte-identical. (The device is
/// enrolled AFTER authoring so both renders are the device's first:
/// generation is a declared input recorded in manifest.yaml, so only
/// identical input sets are byte-identical.)
#[tokio::test]
async fn rerender_from_scratch_is_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());

    author_web_app(&app).await;
    let token = add_device(&state, "dev-1", None);

    let (_, _, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    let bundle_a = parse_manifest(&body).bundle.expect("bundle");

    // Forget the render (as if it never happened), keep the tree.
    {
        let conn = state.db.lock().unwrap();
        conn.execute("DELETE FROM device_manifests WHERE device_id = 'dev-1'", [])
            .unwrap();
    }
    let (status, _) = send_json(
        &app,
        Request::post("/api/render").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    let m = parse_manifest(&body);
    let bundle_b = m.bundle.expect("bundle");
    // Same OCI manifest digest => same layer blob => same tar bytes.
    // (generation restarts with the row, so manifest.yaml matches too)
    assert_eq!(bundle_a.digest, bundle_b.digest, "deterministic render+pack");
}

/// Crash-only (Law 3): a revision committed while the render pass never
/// ran (kill -9 between commit and render) is detected and rendered at
/// the NEXT startup — no HTTP request needed to heal it.
#[tokio::test]
async fn startup_reconcile_renders_unrendered_revision() {
    let dir = tempfile::tempdir().unwrap();
    let (app_a, state_a) = app(dir.path());
    let token = add_device(&state_a, "dev-1", None);

    author_web_app(&app_a).await;
    let (_, _, body) = send(&app_a, get_as("/api/reeve/v1/manifest", &token)).await;
    let v0 = parse_manifest(&body).manifest_version;

    // Commit straight into the revision store, bypassing the authoring
    // API's render hook — the "killed between commit and render" state.
    {
        let mut store = state_a.revisions.lock().unwrap();
        let head = store.head(Stream::Local).unwrap().unwrap();
        let tree = store.tree_at(head).unwrap();
        let mut manifest: std::collections::BTreeMap<String, Vec<u8>> = tree
            .iter()
            .map(|(p, d)| (p.clone(), store.blob(d).unwrap().unwrap()))
            .collect();
        manifest.insert(
            "layers/00-all/apps/web/params.yaml".to_string(),
            b"greeting: crashed-mid-render\n".to_vec(),
        );
        store
            .commit(manifest, "test", "crash sim", Stream::Local)
            .unwrap();
    }
    drop(app_a);
    drop(state_a);

    // Restart: bootstrap reconciles (renders) BEFORE serving.
    let (_, state_b) = app(dir.path());
    {
        let conn = state_b.db.lock().unwrap();
        let (version, json): (i64, String) = conn
            .query_row(
                "SELECT manifest_version, manifest_json FROM device_manifests
                 WHERE device_id = 'dev-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            ManifestVersion(version as u64) > v0,
            "startup pass bumped past {v0:?}"
        );
        assert!(json.contains("\"bundle\""));
    }
}

/// Capabilities (spec/reeve/01-framework.md §3.3): device-auth'd,
/// serverVersion + only compiled-in extensions.
#[tokio::test]
async fn capabilities_advertises_server_version() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1", None);

    let (status, caps) = send_json(&app, get_as("/api/reeve/v1/capabilities", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(caps["serverVersion"], env!("CARGO_PKG_VERSION"));
}

/// The layer chain from the device row (class/region/site) is a render
/// input: two devices at different sites render different bundles from
/// the same tree.
#[tokio::test]
async fn layer_chain_membership_shapes_the_render() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token_a = add_device(&state, "dev-a", Some("plant-a"));
    let token_b = add_device(&state, "dev-b", Some("plant-b"));

    author_web_app(&app).await;
    let (status, _) = send_json(
        &app,
        put_files(
            "/api/tree/layers/20-site.plant-a",
            &[("apps/web/params.yaml", "greeting: hello-plant-a\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let pull_compose = |token: String, device: &'static str| {
        let app = app.clone();
        async move {
            let (_, _, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
            let bundle = parse_manifest(&body).bundle.expect("bundle");
            let (_, _, oci) = send(
                &app,
                get_as(&format!("{}/manifests/{}", bundle.url, bundle.digest), &token),
            )
            .await;
            let oci: Value = serde_json::from_slice(&oci).unwrap();
            let layer = oci["layers"][0]["digest"].as_str().unwrap().to_string();
            let (_, _, tarball) =
                send(&app, get_as(&format!("{}/blobs/{layer}", bundle.url), &token)).await;
            let files = gunzip_untar(&tarball);
            (
                String::from_utf8(files["apps/web/deployment.yaml"].clone()).unwrap(),
                device,
            )
        }
    };

    let (dep_a, _) = pull_compose(token_a, "dev-a").await;
    let (dep_b, _) = pull_compose(token_b, "dev-b").await;
    assert!(dep_a.contains("hello-plant-a"), "site layer applied: {dep_a}");
    assert!(!dep_b.contains("hello-plant-a"), "other site unaffected: {dep_b}");
}

// ------------------------------------------------------ REV-010 §11.3
// Device management write paths: PATCH re-render on assignment change,
// pin holds a device at its current revision, decommission cuts serving.

/// Current manifestVersion of a device, or `None` if never rendered.
fn manifest_version(state: &AppState, device_id: &str) -> Option<i64> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT manifest_version FROM device_manifests WHERE device_id = ?1",
        params![device_id],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

/// Human PATCH (AuthMode::None => anonymous is admin, so no session).
async fn patch_device(app: &Router, id: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::patch(format!("/api/devices/{id}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send_json(app, req).await
}

/// A device with no site assignment renders the base greeting; PATCHing
/// its site re-renders it (§11.3); a later tag-only PATCH does not.
#[tokio::test]
async fn patch_assignment_rerenders_tag_change_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1", None); // unassigned

    author_web_app(&app).await;
    // Site override that changes the rendered app content.
    let (status, _) = send_json(
        &app,
        put_files(
            "/api/tree/layers/20-site.plant-a",
            &[("apps/web/params.yaml", "greeting: hello-plant-a\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Baseline render (poll drives ensure_current).
    let (status, _, _) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    assert_eq!(status, StatusCode::OK);
    let v1 = manifest_version(&state, "dev-1").expect("rendered");

    // Containment (§11.1): a site assignment is validated against the
    // fleet->site tree, so the fleet + site must exist as groups first.
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO location_groups (kind, name, parent_id) VALUES ('fleet','north',NULL)",
            [],
        )
        .unwrap();
        let fid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO location_groups (kind, name, parent_id) VALUES ('site','plant-a',?1)",
            params![fid],
        )
        .unwrap();
    }

    // Assignment change => re-render. The response reflects the new site.
    let (status, body) =
        patch_device(&app, "dev-1", json!({ "fleet": "north", "site": "plant-a" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["site"], "plant-a");
    let v2 = manifest_version(&state, "dev-1").expect("re-rendered");
    assert!(v2 > v1, "assignment change must bump manifestVersion: {v1} -> {v2}");

    // Verify the site layer actually applied to the served bundle.
    let (_, _, body) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    let bundle = parse_manifest(&body).bundle.expect("bundle");
    let (_, _, oci) = send(
        &app,
        get_as(&format!("{}/manifests/{}", bundle.url, bundle.digest), &token),
    )
    .await;
    let oci: Value = serde_json::from_slice(&oci).unwrap();
    let layer = oci["layers"][0]["digest"].as_str().unwrap().to_string();
    let (_, _, tarball) = send(&app, get_as(&format!("{}/blobs/{layer}", bundle.url), &token)).await;
    let files = gunzip_untar(&tarball);
    let dep = String::from_utf8(files["apps/web/deployment.yaml"].clone()).unwrap();
    assert!(dep.contains("hello-plant-a"), "site override applied after PATCH: {dep}");

    // Tag-only change => NO re-render (§11.3).
    let (status, body) = patch_device(&app, "dev-1", json!({ "tags": { "env": "prod" } })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tags"]["env"], "prod");
    assert_eq!(body["labels"]["env"], "prod", "tags mirror labels");
    let v3 = manifest_version(&state, "dev-1").expect("still rendered");
    assert_eq!(v3, v2, "a tag change must not re-render");

    // Clearing an assignment (null) is a change too.
    let (status, body) = patch_device(&app, "dev-1", json!({ "site": null })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["site"], Value::Null);
    let v4 = manifest_version(&state, "dev-1").expect("re-rendered");
    assert!(v4 > v3, "unassigning site re-renders back to base: {v3} -> {v4}");
}

/// A pinned device holds its manifest at its current revision through a
/// fleet-wide deploy; an unpinned peer moves (§11.3).
#[tokio::test]
async fn pinned_device_not_moved_by_deploy() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let _t1 = add_device(&state, "dev-pinned", Some("plant-a"));
    let _t2 = add_device(&state, "dev-free", Some("plant-a"));

    author_web_app(&app).await;
    reeve_server::render::render_all(&state).expect("baseline render");
    let pinned_v1 = manifest_version(&state, "dev-pinned").expect("rendered");
    let free_v1 = manifest_version(&state, "dev-free").expect("rendered");

    // Pin one device (no re-render needed — it holds where it is).
    let (status, body) = patch_device(&app, "dev-pinned", json!({ "pinned": true })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["pinned"], true);

    // Fleet-wide deploy: add a second app to the base layer, changing
    // the rendered content set for every device.
    let (status, _) = send_json(
        &app,
        put_files(
            "/api/tree/layers/00-all",
            &[
                ("apps/web/app.yaml", "package:\n  name: web\n  version: 1.0.0\n"),
                ("apps/web2/app.yaml", "package:\n  name: web\n  version: 1.0.0\n"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    reeve_server::render::render_all(&state).expect("deploy render");

    let pinned_v2 = manifest_version(&state, "dev-pinned").expect("rendered");
    let free_v2 = manifest_version(&state, "dev-free").expect("rendered");
    assert_eq!(pinned_v2, pinned_v1, "pinned device holds at its revision");
    assert!(free_v2 > free_v1, "unpinned peer moves with the deploy: {free_v1} -> {free_v2}");

    // Unpin: the next render pass catches it up to head.
    let (status, _) = patch_device(&app, "dev-pinned", json!({ "pinned": false })).await;
    assert_eq!(status, StatusCode::OK);
    reeve_server::render::render_all(&state).expect("catch-up render");
    let pinned_v3 = manifest_version(&state, "dev-pinned").expect("rendered");
    assert!(pinned_v3 > pinned_v2, "unpinning releases the hold: {pinned_v2} -> {pinned_v3}");
}

/// Decommission revokes the device credential and tombstones the device
/// so its desired state stops being served (§11.3). Idempotent.
#[tokio::test]
async fn decommission_revokes_and_stops_serving() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1", Some("plant-a"));
    author_web_app(&app).await;

    // Serving works before decommission.
    let (status, _, _) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(manifest_version(&state, "dev-1").is_some());

    // Decommission (operator+; AuthMode::None => admin).
    let req = Request::post("/api/devices/dev-1/decommission")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The credential no longer authenticates (D1 full cutoff).
    let (status, _, _) = send(&app, get_as("/api/reeve/v1/manifest", &token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked token must not poll");

    // The served manifest row is gone; the render pass skips it.
    assert!(manifest_version(&state, "dev-1").is_none(), "manifest tombstoned");
    reeve_server::render::render_all(&state).expect("render skips decommissioned");
    assert!(manifest_version(&state, "dev-1").is_none(), "stays gone after a render pass");

    // Idempotent.
    let req = Request::post("/api/devices/dev-1/decommission")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
