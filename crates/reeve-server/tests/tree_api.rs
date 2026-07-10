//! Tree authoring API over the real router (docs/decisions/authoring.md
//! D14; spec/reeve/06-federation.md §8.2/§8.4): idempotent layer puts,
//! structural ownership refusal, package vendoring with margo-package
//! validation, and the read surface (history/diff/blame/file content).

use std::path::Path as FsPath;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use reeve_server::config::{AuthMode, Config};
use reeve_server::ownership::Ownership;
use reeve_server::{auth, router, state::AppState};

fn config(data_dir: &FsPath, auth: AuthMode) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth,
        session_ttl_secs: 3600,
        registry_endpoint: "registry.example:5000".to_string(),
    }
}

/// Bootstrap on a temp DB. REEVE_AUTH=none => anonymous acts as admin
/// (D1), which keeps authoring tests about authoring.
fn app(dir: &FsPath) -> (Router, AppState) {
    let state = reeve_server::bootstrap(config(dir, AuthMode::None)).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn send_raw(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

fn put_json(uri: &str, body: Value) -> Request<Body> {
    Request::put(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

/// files: rel path -> plaintext content (base64-encoded here).
fn layer_body(message: &str, files: &[(&str, &str)]) -> Value {
    let files: serde_json::Map<String, Value> = files
        .iter()
        .map(|(p, c)| ((*p).to_string(), Value::String(B64.encode(c))))
        .collect();
    json!({ "message": message, "files": files })
}

#[tokio::test]
async fn layer_put_is_idempotent_same_content_no_new_revision() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());

    let body = layer_body(
        "fleet v1",
        &[("apps/nginx/app.yaml", "enabled: true\n")],
    );
    let (status, first) = send(&app, put_json("/api/tree/layers/00-fleet", body.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["changed"], true);
    assert_eq!(first["stream"], "local");
    let rev1 = first["revision"].as_i64().unwrap();

    // Same content again (the IaC re-apply): SAME revision, no commit.
    let (status, second) = send(&app, put_json("/api/tree/layers/00-fleet", body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["changed"], false, "identical content => no new revision (D14)");
    assert_eq!(second["revision"].as_i64().unwrap(), rev1);

    // Changed content: a new revision.
    let (status, third) = send(
        &app,
        put_json(
            "/api/tree/layers/00-fleet",
            layer_body("fleet v2", &[("apps/nginx/app.yaml", "enabled: false\n")]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(third["changed"], true);
    assert!(third["revision"].as_i64().unwrap() > rev1);
}

#[tokio::test]
async fn layer_put_replaces_the_whole_layer_and_leaves_others_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());

    send(
        &app,
        put_json(
            "/api/tree/layers/00-fleet",
            layer_body("fleet", &[("apps/a/app.yaml", "a\n"), ("apps/b/app.yaml", "b\n")]),
        ),
    )
    .await;
    send(
        &app,
        put_json(
            "/api/tree/layers/20-site.plant-a",
            layer_body("site", &[("apps/c/app.yaml", "c\n")]),
        ),
    )
    .await;
    // Re-apply fleet WITHOUT apps/b: declarative replace removes it.
    let (_, resp) = send(
        &app,
        put_json(
            "/api/tree/layers/00-fleet",
            layer_body("fleet drop b", &[("apps/a/app.yaml", "a\n")]),
        ),
    )
    .await;
    let head = resp["revision"].as_i64().unwrap();

    let (status, body) = send(&app, get(&format!("/api/tree/revisions/{head}"))).await;
    assert_eq!(status, StatusCode::OK);
    let files = body["files"].as_object().unwrap();
    assert!(files.contains_key("layers/00-fleet/apps/a/app.yaml"));
    assert!(!files.contains_key("layers/00-fleet/apps/b/app.yaml"), "absent file removed");
    assert!(
        files.contains_key("layers/20-site.plant-a/apps/c/app.yaml"),
        "other layers untouched"
    );
}

#[tokio::test]
async fn writes_outside_the_ownership_set_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_, mut state) = app(dir.path());
    // Simulate the gateway tier C10 will configure: owns its site layer
    // and its locally-enrolled device layers only (federation §8.4).
    state.ownership = Arc::new(Ownership::Gateway {
        owned_prefixes: vec![
            "layers/20-site.plant-a".into(),
            "layers/30-device.".into(),
        ],
    });
    let app = router::build(state.clone());

    // Hub-owned layer: structurally refused.
    let (status, body) = send(
        &app,
        put_json(
            "/api/tree/layers/00-fleet",
            layer_body("nope", &[("apps/a/app.yaml", "a\n")]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("single writer"));

    // Package vendoring not in the ownership set either.
    let (status, _) = send(
        &app,
        put_json(
            "/api/tree/packages/nginx/1.0.0",
            layer_body("nope", &[("margo.yaml", "x")]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Nothing was committed.
    assert!(
        state
            .revisions
            .lock()
            .unwrap()
            .head(revision_store::Stream::Local)
            .unwrap()
            .is_none()
    );

    // Owned site layer still writable.
    let (status, resp) = send(
        &app,
        put_json(
            "/api/tree/layers/20-site.plant-a",
            layer_body("site", &[("apps/c/app.yaml", "c\n")]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["changed"], true);
}

#[tokio::test]
async fn malformed_layer_names_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    for bad in ["fleet", "0-fleet", "00-..%2Fevil"] {
        let (status, _) = send(
            &app,
            put_json(
                &format!("/api/tree/layers/{bad}"),
                layer_body("m", &[(".keep", "")]),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
    }
    // traversal in a file path
    let (status, _) = send(
        &app,
        put_json(
            "/api/tree/layers/00-fleet",
            layer_body("m", &[("../escape.yaml", "x")]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Load a real pinned-reference margo package dir as (rel path, bytes).
fn fixture_package_files(name: &str) -> Vec<(String, Vec<u8>)> {
    let root = FsPath::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference/poc/tests/artefacts")
        .join(name)
        .join("margo-package");
    assert!(
        root.is_dir(),
        "reference/ submodule missing — run `git submodule update --init --recursive`"
    );
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(&root).unwrap().to_str().unwrap().to_string();
                out.push((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    out
}

#[tokio::test]
async fn package_vendor_commits_validated_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());

    let files: serde_json::Map<String, Value> = fixture_package_files("nextcloud-compose")
        .into_iter()
        .map(|(p, c)| (p, Value::String(B64.encode(c))))
        .collect();
    let (status, body) = send(
        &app,
        put_json(
            "/api/tree/packages/nextcloud/1.0.0",
            json!({ "files": files }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true);
    let rev = body["revision"].as_i64().unwrap();

    let (status, tree) = send(&app, get(&format!("/api/tree/revisions/{rev}"))).await;
    assert_eq!(status, StatusCode::OK);
    let manifest = tree["files"].as_object().unwrap();
    assert!(manifest.contains_key("packages/nextcloud/1.0.0/margo.yaml"));
    assert!(
        manifest
            .keys()
            .any(|k| k.starts_with("packages/nextcloud/1.0.0/resources/")),
        "resources vendored too"
    );

    // File content round-trips byte-exact through the read surface.
    let (status, served) = send_raw(
        &app,
        get(&format!(
            "/api/tree/revisions/{rev}/files/packages/nextcloud/1.0.0/margo.yaml"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let original = fixture_package_files("nextcloud-compose")
        .into_iter()
        .find(|(p, _)| p == "margo.yaml")
        .unwrap()
        .1;
    assert_eq!(served, original);
}

#[tokio::test]
async fn invalid_package_is_refused_and_nothing_is_committed() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());

    // Unparseable margo.yaml: fails margo-package validation => 422.
    let (status, body) = send(
        &app,
        put_json(
            "/api/tree/packages/broken/0.1.0",
            layer_body("bad", &[("margo.yaml", "kind: [unclosed")]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("validation failed"));

    // Missing margo.yaml entirely.
    let (status, _) = send(
        &app,
        put_json(
            "/api/tree/packages/broken/0.1.0",
            layer_body("bad", &[("resources/readme.md", "hi")]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    assert!(
        state
            .revisions
            .lock()
            .unwrap()
            .head(revision_store::Stream::Local)
            .unwrap()
            .is_none(),
        "no revision from a refused vendor"
    );
}

#[tokio::test]
async fn history_diff_blame_and_404s() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());

    let (_, r1) = send(
        &app,
        put_json(
            "/api/tree/layers/00-fleet",
            layer_body("fleet v1", &[("apps/nginx/app.yaml", "enabled: true\n")]),
        ),
    )
    .await;
    let (_, r2) = send(
        &app,
        put_json(
            "/api/tree/layers/00-fleet",
            layer_body(
                "fleet v2",
                &[
                    ("apps/nginx/app.yaml", "enabled: false\n"),
                    ("apps/redis/app.yaml", "enabled: true\n"),
                ],
            ),
        ),
    )
    .await;
    let (rev1, rev2) = (r1["revision"].as_i64().unwrap(), r2["revision"].as_i64().unwrap());

    // History: newest first, author + message present (D13/D14).
    let (status, hist) = send(&app, get("/api/tree/revisions")).await;
    assert_eq!(status, StatusCode::OK);
    let hist = hist.as_array().unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0]["id"].as_i64().unwrap(), rev2);
    assert_eq!(hist[0]["message"], "fleet v2");
    assert_eq!(hist[0]["author"], "anonymous");
    assert_eq!(hist[0]["stream"], "local");
    assert_eq!(hist[1]["id"].as_i64().unwrap(), rev1);
    // limit honored
    let (_, limited) = send(&app, get("/api/tree/revisions?limit=1")).await;
    assert_eq!(limited.as_array().unwrap().len(), 1);

    // Diff rev1 -> rev2: nginx modified, redis added.
    let (status, diff) = send(&app, get(&format!("/api/tree/diff/{rev1}/{rev2}"))).await;
    assert_eq!(status, StatusCode::OK);
    let diff = diff.as_array().unwrap();
    assert_eq!(diff.len(), 2);
    assert_eq!(diff[0]["path"], "layers/00-fleet/apps/nginx/app.yaml");
    assert_eq!(diff[0]["change"], "modified");
    assert_eq!(diff[1]["path"], "layers/00-fleet/apps/redis/app.yaml");
    assert_eq!(diff[1]["change"], "added");

    // Blame: the nginx file changed at both revisions.
    let (status, blame) = send(
        &app,
        get("/api/tree/blame/layers/00-fleet/apps/nginx/app.yaml"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let blame = blame.as_array().unwrap();
    assert_eq!(blame.len(), 2);
    assert_eq!(blame[0]["revision"]["id"].as_i64().unwrap(), rev1);
    assert!(blame[1]["digest"].as_str().is_some());

    // File content at each revision reflects that revision.
    let (_, v1) = send_raw(
        &app,
        get(&format!(
            "/api/tree/revisions/{rev1}/files/layers/00-fleet/apps/nginx/app.yaml"
        )),
    )
    .await;
    assert_eq!(v1, b"enabled: true\n");

    // Unknowns 404.
    let (status, _) = send(&app, get("/api/tree/revisions/9999")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, get(&format!("/api/tree/diff/{rev1}/9999"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        get(&format!("/api/tree/revisions/{rev1}/files/no/such/file")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn roles_writes_need_operator_reads_need_viewer() {
    use device_api::Role;

    let dir = tempfile::tempdir().unwrap();
    let state =
        reeve_server::bootstrap(config(dir.path(), AuthMode::Password)).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    {
        let conn = state.db.lock().unwrap();
        auth::users::create(&conn, "viewer", "pw-viewer", Role::Viewer).unwrap();
        auth::users::create(&conn, "op", "pw-op", Role::Operator).unwrap();
    }
    let app = router::build(state.clone());

    async fn login(app: &Router, user: &str, pw: &str) -> String {
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "username": user, "password": pw }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    let viewer = login(&app, "viewer", "pw-viewer").await;
    let op = login(&app, "op", "pw-op").await;
    let body = layer_body("m", &[(".keep", "")]);

    // Anonymous (password mode): 401 for both surfaces.
    let (status, _) = send(&app, put_json("/api/tree/layers/00-fleet", body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(&app, get("/api/tree/revisions")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Viewer: reads yes, writes 403.
    let req = Request::put("/api/tree/layers/00-fleet")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &viewer)
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let req = Request::get("/api/tree/revisions")
        .header(header::COOKIE, &viewer)
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);

    // Operator: writes yes.
    let req = Request::put("/api/tree/layers/00-fleet")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &op)
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, resp) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["author"], Value::Null); // response carries revision, not author
    assert_eq!(resp["changed"], true);

    // ...and history attributes the write to the operator (D14 audit).
    let req = Request::get("/api/tree/revisions")
        .header(header::COOKIE, &viewer)
        .body(Body::empty())
        .unwrap();
    let (_, hist) = send(&app, req).await;
    assert_eq!(hist.as_array().unwrap()[0]["author"], "op");
}
