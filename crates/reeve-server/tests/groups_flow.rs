//! Canonical location groups + fleet->site containment over the real
//! router (REV-010 amendment, spec/reeve/11-fleet-model.md §11.1/§11.3):
//! create fleets/sites, the tree + scoped-children reads, per-fleet name
//! uniqueness, delete-in-use refusal, and device-assignment validation
//! (a site under the wrong fleet is 422; a valid site works). Enrollment
//! pre-assign is validated too.

use std::path::Path as FsPath;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use rusqlite::params;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use reeve_server::config::{AuthMode, Config};
use reeve_server::{auth, device_tokens, join_tokens, router, state::AppState};

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

fn patch_json(uri: &str, body: &Value) -> Request<Body> {
    Request::patch(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

async fn create_fleet(app: &Router, name: &str) -> i64 {
    let (status, body) =
        send(app, post_json("/api/groups", &json!({ "kind": "fleet", "name": name }))).await;
    assert_eq!(status, StatusCode::CREATED, "create fleet {name}: {body}");
    body["id"].as_i64().unwrap()
}

async fn create_site(app: &Router, name: &str, parent_id: i64) -> (StatusCode, Value) {
    send(
        app,
        post_json(
            "/api/groups",
            &json!({ "kind": "site", "name": name, "parentId": parent_id }),
        ),
    )
    .await
}

// --------------------------------------------------------------- tests

/// The full tree + the scoped-children read; per-fleet uniqueness (409
/// under the same fleet, OK under a different fleet).
#[tokio::test]
async fn create_tree_scoped_read_and_uniqueness() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _state) = app(dir.path());

    let north = create_fleet(&app, "north").await;
    let south = create_fleet(&app, "south").await;

    // site under north
    let (status, body) = create_site(&app, "plant-a", north).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "site");
    assert_eq!(body["parentId"], north);

    // duplicate site name under the SAME fleet => 409
    let (status, _) = create_site(&app, "plant-a", north).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // same site name under a DIFFERENT fleet => OK (per-fleet uniqueness)
    let (status, _) = create_site(&app, "plant-a", south).await;
    assert_eq!(status, StatusCode::CREATED);

    // duplicate FLEET name => 409
    let (status, _) =
        send(&app, post_json("/api/groups", &json!({ "kind": "fleet", "name": "north" }))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // a site needs a valid parent fleet
    let (status, _) =
        send(&app, post_json("/api/groups", &json!({ "kind": "site", "name": "x" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "site without parent");
    let (status, _) = create_site(&app, "x", 99999).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "site under nonexistent fleet");
    // a fleet must not carry a parent
    let (status, _) = send(
        &app,
        post_json("/api/groups", &json!({ "kind": "fleet", "name": "z", "parentId": north })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "fleet with parent");

    // full-tree read
    let (status, tree) = send(&app, get("/api/groups")).await;
    assert_eq!(status, StatusCode::OK);
    let fleets = tree["fleets"].as_array().unwrap();
    assert_eq!(fleets.len(), 2, "north + south");
    // ordered by name: north first
    assert_eq!(fleets[0]["name"], "north");
    assert_eq!(fleets[0]["sites"][0]["name"], "plant-a");

    // scoped-children read: only north's subtree
    let (status, scoped) = send(&app, get("/api/groups?kind=site&fleet=north")).await;
    assert_eq!(status, StatusCode::OK);
    let sf = scoped["fleets"].as_array().unwrap();
    assert_eq!(sf.len(), 1);
    assert_eq!(sf[0]["name"], "north");
    assert_eq!(sf[0]["sites"].as_array().unwrap().len(), 1);

    // scoped read of an unknown fleet => 404
    let (status, _) = send(&app, get("/api/groups?fleet=ghost")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Device assignment is validated against the containment tree: a site
/// under the wrong fleet is 422; a valid site works; a fleet-only change
/// that strands the site is rejected.
#[tokio::test]
async fn device_assignment_enforces_containment() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1");

    let north = create_fleet(&app, "north").await;
    create_site(&app, "plant-a", north).await;
    create_fleet(&app, "south").await;

    // valid: site under its fleet
    let (status, body) =
        send(&app, patch_json("/api/devices/d1", &json!({ "fleet": "north", "site": "plant-a" }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["fleet"], "north");
    assert_eq!(body["site"], "plant-a");

    // wrong fleet: plant-a does not belong to south => 422
    let (status, _) =
        send(&app, patch_json("/api/devices/d1", &json!({ "fleet": "south", "site": "plant-a" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // unchanged
    let (_, d) = send(&app, get("/api/devices/d1")).await;
    assert_eq!(d["fleet"], "north");
    assert_eq!(d["site"], "plant-a");

    // fleet-only change that strands the existing site => 422
    let (status, _) = send(&app, patch_json("/api/devices/d1", &json!({ "fleet": "south" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "site plant-a not under south");

    // site with no fleet => 422
    add_device(&state, "d2");
    let (status, _) = send(&app, patch_json("/api/devices/d2", &json!({ "site": "plant-a" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // assigning an unknown site under a real fleet => 422 (strict, no free-add)
    let (status, _) =
        send(&app, patch_json("/api/devices/d2", &json!({ "fleet": "north", "site": "nope" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // clearing both is fine
    let (status, _) =
        send(&app, patch_json("/api/devices/d1", &json!({ "fleet": null, "site": null }))).await;
    assert_eq!(status, StatusCode::OK);
}

/// Delete is refused while devices (or, for a fleet, child sites) still
/// reference the group; works once nothing does.
#[tokio::test]
async fn delete_in_use_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1");

    let north = create_fleet(&app, "north").await;
    let (_, site) = create_site(&app, "plant-a", north).await;
    let site_id = site["id"].as_i64().unwrap();

    // assign the device
    let (status, _) =
        send(&app, patch_json("/api/devices/d1", &json!({ "fleet": "north", "site": "plant-a" }))).await;
    assert_eq!(status, StatusCode::OK);

    // fleet in use (has a device AND a child site) => 409
    let (status, _) =
        send(&app, Request::delete(format!("/api/groups/{north}")).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // site in use (a device references it) => 409
    let (status, _) =
        send(&app, Request::delete(format!("/api/groups/{site_id}")).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // reassign the device off the groups
    send(&app, patch_json("/api/devices/d1", &json!({ "fleet": null, "site": null }))).await;

    // now the site deletes
    let (status, _) =
        send(&app, Request::delete(format!("/api/groups/{site_id}")).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // and the (now childless, device-free) fleet deletes
    let (status, _) =
        send(&app, Request::delete(format!("/api/groups/{north}")).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // deleting an unknown group => 404
    let (status, _) =
        send(&app, Request::delete("/api/groups/99999").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Renaming an unused group works; renaming an in-use group is refused.
#[tokio::test]
async fn rename_respects_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());

    let north = create_fleet(&app, "north").await;

    // free rename of an empty fleet
    let (status, body) = send(&app, patch_json(&format!("/api/groups/{north}"), &json!({ "name": "northeast" }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "northeast");

    // now populate it and confirm rename is refused
    create_site(&app, "plant-a", north).await;
    add_device(&state, "d1");
    send(&app, patch_json("/api/devices/d1", &json!({ "fleet": "northeast", "site": "plant-a" }))).await;
    let (status, _) = send(&app, patch_json(&format!("/api/groups/{north}"), &json!({ "name": "west" }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "in-use fleet rename refused");
}

/// Enrollment pre-assign is validated too: a join token's fleet/site is
/// auto-provisioned as groups (under the fleet) so the device lands
/// correctly and the assignment is a valid containment.
#[tokio::test]
async fn enroll_pre_assign_provisions_groups() {
    use device_api::{EnrollRequest, EnrollmentService};

    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());

    // issue a join token pre-assigning fleet=north / site=plant-a
    let assign = join_tokens::PreAssign {
        fleet: Some("north".into()),
        site: Some("plant-a".into()),
        r#type: Some("hmi".into()),
        tags: None,
    };
    let jt = {
        let conn = state.db.lock().unwrap();
        join_tokens::issue_with(&conn, "op", 3600, 1, None, &assign).unwrap()
    };

    let svc = reeve_server::enroll::SqliteEnrollmentService::new(
        state.db.clone(),
        state.revisions.clone(),
    );
    let resp = svc
        .enroll(&EnrollRequest {
            join_token: jt,
            hostname: "edge-01".into(),
            arch: "x86_64".into(),
            agent_version: "0.1.0".into(),
        })
        .unwrap();

    // the device landed in the group
    let (status, d) = send(&app, get(&format!("/api/devices/{}", resp.device_id))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(d["fleet"], "north");
    assert_eq!(d["site"], "plant-a");
    assert_eq!(d["type"], "hmi");

    // and the groups now exist as a valid containment (site under fleet)
    let (_, tree) = send(&app, get("/api/groups")).await;
    let fleets = tree["fleets"].as_array().unwrap();
    assert_eq!(fleets.len(), 1);
    assert_eq!(fleets[0]["name"], "north");
    assert_eq!(fleets[0]["sites"][0]["name"], "plant-a");
}
