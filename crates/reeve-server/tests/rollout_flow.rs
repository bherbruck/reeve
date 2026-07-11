//! Staged rollouts end-to-end (C9): cohort selection, wave-by-wave
//! State-Manifest advancement, health gates, auto-pause, holds, and
//! crash-resume — over the real router + engine tick.
//!
//! Spec sources: spec/reeve/09-rollouts.md §11.1–§11.6 (REV-008),
//! docs/decisions/tree-render.md D12 (labels select cohorts;
//! pinned/unaffected devices count converged and MUST be surfaced).
//!
//! The engine's interval task is not spawned here; tests drive
//! `rollouts::tick` explicitly so every wave/gate transition is
//! deterministic (the tick is a pure function of DB state — Law 3 —
//! which is also exactly why restart-resume works).
#![cfg(feature = "ext-rollouts")]

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
use reeve_server::ext::rollouts;
use reeve_server::{auth, device_tokens, router, state::AppState};
use reeve_types::reeve::events::{RolloutPhase, SseEvent};

// ------------------------------------------------------------- harness

fn config(data_dir: &FsPath) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth: AuthMode::None, // anonymous acts as admin (D1)
        session_ttl_secs: 3600,
        registry_endpoint: "registry.example:5000".to_string(),
        durability: reeve_server::config::DurabilityConfig::disabled(),
        zot: None,
        federation: None,
        install_open: false,
    }
}

fn app(dir: &FsPath) -> (Router, AppState) {
    let state = reeve_server::bootstrap(config(dir)).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state)
}

fn add_device(state: &AppState, id: &str, site: Option<&str>, labels: &Value) -> String {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at, site, labels)
         VALUES (?1, 'box', 'x86_64', '0.1.0', 0, ?2, ?3)",
        params![id, site, labels.to_string()],
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

fn post_json(uri: &str, token: Option<&str>, body: &Value) -> Request<Body> {
    let mut b = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
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

/// Same renderable compose package the delivery tests pin.
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

/// PUT one subtree and return the revision id from the response.
async fn put_tree(app: &Router, uri: &str, files: &[(&str, &str)]) -> i64 {
    let (status, body) = send(app, put_files(uri, files)).await;
    assert_eq!(status, StatusCode::OK, "PUT {uri}: {body}");
    body["revision"].as_i64().expect("revision id")
}

/// Vendor the package, then apply the fleet layer with `greeting`.
/// Returns the fleet revision id.
async fn author_fleet(app: &Router, greeting: &str) -> i64 {
    put_tree(
        app,
        "/api/tree/packages/web/1.0.0",
        &[("margo.yaml", PKG_MANIFEST), ("compose.yml", PKG_COMPOSE)],
    )
    .await;
    put_fleet(app, greeting).await
}

async fn put_fleet(app: &Router, greeting: &str) -> i64 {
    put_tree(
        app,
        "/api/tree/layers/00-fleet",
        &[
            ("apps/web/app.yaml", "package:\n  name: web\n  version: 1.0.0\n"),
            ("apps/web/params.yaml", &format!("greeting: {greeting}\n")),
        ],
    )
    .await
}

/// Margo status report with the rev-004/1 additive object (fresh
/// `received_at` is what gate math consumes).
fn status_body(deployment_id: &str, state: &str, seq: u64) -> Value {
    json!({
        "apiVersion": "deployment.margo.org/v1alpha1",
        "kind": "DeploymentStatusManifest",
        "deploymentId": deployment_id,
        "status": { "state": state },
        "components": [{ "name": "web-stack", "state": state }],
        "reeve": { "observedAt": "2026-07-10T00:00:00Z", "seq": seq }
    })
}

async fn report_status(app: &Router, token: &str, device: &str, state: &str, seq: u64) {
    let dep = desired_state::deployment_id(device, "web").to_string();
    let (status, body) = send(
        app,
        post_json(
            &format!("/api/v1/clients/{device}/deployments/{dep}/status"),
            Some(token),
            &status_body(&dep, state, seq),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "status ingest: {body}");
}

/// Default strict gate for tests: zero soak, zero timeout, zero
/// undetermined tolerance — every transition is driven by explicit
/// status reports and ticks.
fn strict_gate() -> Value {
    json!({ "soakSecs": 0, "gateTimeoutSecs": 0, "undeterminedAllowance": 0 })
}

async fn create_rollout(app: &Router, body: &Value) -> (StatusCode, Value) {
    send(app, post_json("/api/rollouts", None, body)).await
}

async fn rollout_status(app: &Router, id: &str) -> Value {
    let (status, body) = send(
        app,
        Request::get(format!("/api/rollouts/{id}")).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollout status: {body}");
    body
}

async fn post_action(app: &Router, id: &str, action: &str) -> (StatusCode, Value) {
    send(app, post_json(&format!("/api/rollouts/{id}/{action}"), None, &json!({}))).await
}

// -------------------------------------------------------- state probes

fn rendered_revision(state: &AppState, device: &str) -> Option<i64> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT rendered_revision FROM device_manifests WHERE device_id = ?1",
        params![device],
        |r| r.get(0),
    )
    .ok()
}

fn manifest_version(state: &AppState, device: &str) -> Option<i64> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT manifest_version FROM device_manifests WHERE device_id = ?1",
        params![device],
        |r| r.get(0),
    )
    .ok()
}

fn content_digest(state: &AppState, device: &str) -> Option<String> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT content_digest FROM device_manifests WHERE device_id = ?1",
        params![device],
        |r| r.get(0),
    )
    .ok()
}

/// (revision, rollout_id) of the device's render target, if held.
fn target_of(state: &AppState, device: &str) -> Option<(i64, String)> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT revision, rollout_id FROM device_render_targets WHERE device_id = ?1",
        params![device],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

fn rollout_state(state: &AppState, id: &str) -> (String, i64, Option<String>) {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT state, current_wave, pause_reason FROM rollouts WHERE rollout_id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

// --------------------------------------------------------------- tests

/// §11.1 cohort selectors over the API: explicit list, tree selection
/// (layer subtree), labels-as-grouping (D12). Unit coverage of the
/// resolution logic lives in ext/rollouts.rs; this exercises it with
/// real device rows through the create route.
#[tokio::test]
async fn cohort_selection_list_tree_and_labels() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", Some("plant-a"), &json!({"env": "prod"}));
    add_device(&state, "d2", Some("plant-b"), &json!({"env": "prod"}));
    add_device(&state, "d3", Some("plant-a"), &json!({"env": "dev"}));
    author_fleet(&app, "v1").await;
    let rev = put_fleet(&app, "v2").await;

    // Tree selection: everything under 20-site.plant-a.
    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev, "cohort": { "layers": ["20-site.plant-a"] }, "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["cohort"], json!(["d1", "d3"]));
    post_action(&app, body["rolloutId"].as_str().unwrap(), "abort").await;

    // Labels select cohorts (D12) — never configuration.
    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev, "cohort": { "labels": {"env": "prod"} }, "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["cohort"], json!(["d1", "d2"]));
    post_action(&app, body["rolloutId"].as_str().unwrap(), "abort").await;

    // Explicit list + union with labels, deduped.
    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev,
                 "cohort": { "devices": ["d3"], "labels": {"env": "prod"} },
                 "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["cohort"], json!(["d1", "d2", "d3"]));
}

/// §11.2 + §11.3: waves advance one at a time; each gate opens only on
/// fresh healthy (installed) reports; completion returns every device
/// to head-tracking. Also asserts the §11.6 rollout event stream.
#[tokio::test]
async fn wave_advancement_on_healthy_gate() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let t1 = add_device(&state, "d1", None, &json!({}));
    let t2 = add_device(&state, "d2", None, &json!({}));
    author_fleet(&app, "v1").await;
    let baseline_rev = rendered_revision(&state, "d1").unwrap();
    let rev = put_fleet(&app, "v2").await;
    // The authoring commit auto-rendered everyone at head (C4 hook).
    assert_eq!(rendered_revision(&state, "d1"), Some(rev));

    let mut rx = state.events.subscribe(None).rx;

    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev,
                 "cohort": { "devices": ["d1", "d2"] },
                 "waves": [["d1"], ["d2"]],
                 "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["rolloutId"].as_str().unwrap().to_string();

    // Creation held both at baseline, then wave 0 advanced d1 only.
    assert_eq!(rendered_revision(&state, "d1"), Some(rev), "wave-0 device at rollout revision");
    assert_eq!(rendered_revision(&state, "d2"), Some(baseline_rev), "wave-1 device held at baseline");
    assert_eq!(target_of(&state, "d1"), Some((rev, id.clone())));
    assert_eq!(target_of(&state, "d2"), Some((baseline_rev, id.clone())));

    // Healthy report from d1 -> wave 0 gate passes -> wave 1 advances.
    report_status(&app, &t1, "d1", "installed", 1).await;
    rollouts::tick(&state).unwrap(); // gate wave 0
    rollouts::tick(&state).unwrap(); // start + advance wave 1
    assert_eq!(rendered_revision(&state, "d2"), Some(rev));
    let (st, wave, _) = rollout_state(&state, &id);
    assert_eq!((st.as_str(), wave), ("active", 1));

    // Healthy report from d2 -> completion; holds released.
    report_status(&app, &t2, "d2", "installed", 1).await;
    rollouts::tick(&state).unwrap();
    let (st, _, _) = rollout_state(&state, &id);
    assert_eq!(st, "completed");
    assert_eq!(target_of(&state, "d1"), None, "completion returns devices to head-tracking");
    assert_eq!(target_of(&state, "d2"), None);

    // §11.6: started/gated per wave, completed at the end.
    let mut phases = Vec::new();
    while let Ok(stamped) = rx.try_recv() {
        if let SseEvent::Rollout(e) = stamped.event {
            assert_eq!(e.rollout_id, id);
            phases.push((e.wave, e.phase));
        }
    }
    assert_eq!(
        phases,
        vec![
            (0, RolloutPhase::Started),
            (0, RolloutPhase::Gated),
            (1, RolloutPhase::Started),
            (1, RolloutPhase::Gated),
            (1, RolloutPhase::Completed),
        ]
    );
}

/// §11.4 auto-pause: a `failed` deployment report trips the failure
/// threshold at any time; un-advanced devices stay held; resume after
/// recovery re-evaluates the gate over current data.
#[tokio::test]
async fn auto_pause_on_failed_status_then_resume() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let t1 = add_device(&state, "d1", None, &json!({}));
    let t2 = add_device(&state, "d2", None, &json!({}));
    author_fleet(&app, "v1").await;
    let baseline_rev = rendered_revision(&state, "d2").unwrap();
    let rev = put_fleet(&app, "v2").await;

    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev,
                 "cohort": { "devices": ["d1", "d2"] },
                 "waves": [["d1"], ["d2"]],
                 "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["rolloutId"].as_str().unwrap().to_string();

    // d1 fails -> auto-pause; the wave-1 device must NOT advance.
    report_status(&app, &t1, "d1", "failed", 1).await;
    rollouts::tick(&state).unwrap();
    let (st, wave, reason) = rollout_state(&state, &id);
    assert_eq!((st.as_str(), wave), ("paused", 0));
    assert!(reason.unwrap().contains("failure threshold"), "pause reason names the breach");
    assert_eq!(rendered_revision(&state, "d2"), Some(baseline_rev), "auto-pause stops advancement");

    // Ticking while paused moves nothing (§11.2: stable position).
    rollouts::tick(&state).unwrap();
    assert_eq!(rendered_revision(&state, "d2"), Some(baseline_rev));

    // d1 recovers (higher seq) -> human resume -> gate passes, rollout
    // runs to completion.
    report_status(&app, &t1, "d1", "installed", 2).await;
    let (status, _) = post_action(&app, &id, "resume").await;
    assert_eq!(status, StatusCode::OK);
    rollouts::tick(&state).unwrap(); // wave 1 starts + advances
    report_status(&app, &t2, "d2", "installed", 1).await;
    rollouts::tick(&state).unwrap();
    let (st, _, _) = rollout_state(&state, &id);
    assert_eq!(st, "completed");

    // §11.8/§11.1: the transition history is recorded with authors.
    let detail = rollout_status(&app, &id).await;
    let actions: Vec<&str> = detail["transitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        ["created", "wave-started", "auto-pause", "resumed", "wave-gated", "wave-started", "completed"]
    );
    assert_eq!(detail["transitions"][2]["author"], "engine");
    assert_eq!(detail["transitions"][3]["author"], "anonymous");
}

/// D12/§11.1: a device whose render is materially unchanged by the
/// rollout (a device-layer pin overrides the change) counts CONVERGED
/// in gate math without any status report, never gets a manifest bump,
/// and is surfaced as pinned/unaffected in the status API.
#[tokio::test]
async fn pinned_device_counts_converged_and_is_surfaced() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let tn = add_device(&state, "d-n", None, &json!({}));
    let _tp = add_device(&state, "d-p", None, &json!({}));
    author_fleet(&app, "v1").await;
    // Device-layer pin: d-p's greeting never follows the fleet value.
    put_tree(
        &app,
        "/api/tree/layers/30-device.d-p",
        &[("apps/web/params.yaml", "greeting: pinned\n")],
    )
    .await;
    let pinned_version = manifest_version(&state, "d-p").unwrap();
    let pinned_digest = content_digest(&state, "d-p").unwrap();
    let rev = put_fleet(&app, "v2").await;
    // The fleet change did not move the pinned device even at head.
    assert_eq!(manifest_version(&state, "d-p"), Some(pinned_version));

    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev,
                 "cohort": { "devices": ["d-n", "d-p"] },
                 "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["rolloutId"].as_str().unwrap().to_string();

    // Both advanced (one wave); only the affected device needs to
    // report — the pinned one is already converged (D12 gate math),
    // which is exactly why allowance 0 still passes.
    report_status(&app, &tn, "d-n", "installed", 1).await;
    rollouts::tick(&state).unwrap();
    let (st, _, _) = rollout_state(&state, &id);
    assert_eq!(st, "completed");

    // Surfaced (§11.1 MUST): green must not silently mean "nothing was
    // actually deployed here".
    let detail = rollout_status(&app, &id).await;
    assert_eq!(detail["pinnedUnaffected"], 1);
    let devices = detail["waves"][0]["devices"].as_array().unwrap();
    let dp = devices.iter().find(|d| d["deviceId"] == "d-p").unwrap();
    assert_eq!(dp["unaffected"], true);
    assert_eq!(dp["status"], "converged");

    // The pin held through the whole rollout: no bump, same content.
    assert_eq!(manifest_version(&state, "d-p"), Some(pinned_version));
    assert_eq!(content_digest(&state, "d-p"), Some(pinned_digest));
    assert_eq!(rendered_revision(&state, "d-p"), Some(rev), "converged to ITS OWN render of the rollout revision");
}

/// Manual pause freezes advancement; abort is pausing permanently
/// (§11.2): records retained, holds retained, nothing moves backward.
#[tokio::test]
async fn manual_pause_resume_and_abort() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", None, &json!({}));
    add_device(&state, "d2", None, &json!({}));
    author_fleet(&app, "v1").await;
    let baseline_rev = rendered_revision(&state, "d2").unwrap();
    let rev = put_fleet(&app, "v2").await;

    // Patient gate (long timeout): the wave-0 gate WAITS for reports
    // instead of resolving, so pause/resume mechanics are isolated
    // from gate outcomes here.
    let (_, body) = create_rollout(
        &app,
        &json!({ "revision": rev,
                 "cohort": { "devices": ["d1", "d2"] },
                 "waves": [["d1"], ["d2"]],
                 "gate": { "soakSecs": 0, "gateTimeoutSecs": 3600, "undeterminedAllowance": 0 } }),
    )
    .await;
    let id = body["rolloutId"].as_str().unwrap().to_string();

    let (status, _) = post_action(&app, &id, "pause").await;
    assert_eq!(status, StatusCode::OK);
    let (st, _, reason) = rollout_state(&state, &id);
    assert_eq!(st, "paused");
    assert_eq!(reason.as_deref(), Some("manual pause"));
    // Pausing a paused rollout conflicts; resuming works.
    assert_eq!(post_action(&app, &id, "pause").await.0, StatusCode::CONFLICT);
    assert_eq!(post_action(&app, &id, "resume").await.0, StatusCode::OK);
    assert_eq!(rollout_state(&state, &id).0, "active");

    let (status, _) = post_action(&app, &id, "abort").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rollout_state(&state, &id).0, "aborted");

    // Aborted = permanent pause: ticks move nothing, holds stay, the
    // un-advanced device never sees the rollout revision.
    rollouts::tick(&state).unwrap();
    assert_eq!(rendered_revision(&state, "d2"), Some(baseline_rev));
    assert_eq!(target_of(&state, "d2"), Some((baseline_rev, id.clone())));
    // Terminal states refuse further transitions.
    assert_eq!(post_action(&app, &id, "resume").await.0, StatusCode::CONFLICT);
    assert_eq!(post_action(&app, &id, "abort").await.0, StatusCode::CONFLICT);
}

/// Law 3: kill mid-rollout (simulated by dropping every handle and
/// re-bootstrapping the same data dir) resumes exactly — waves,
/// per-device assignment, holds and gate config all come back from
/// SQLite and the next tick continues the wave march.
#[tokio::test]
async fn restart_mid_rollout_resumes_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let (id, t1, t2, rev, baseline_rev) = {
        let (app, state) = app(dir.path());
        let t1 = add_device(&state, "d1", None, &json!({}));
        let t2 = add_device(&state, "d2", None, &json!({}));
        author_fleet(&app, "v1").await;
        let baseline_rev = rendered_revision(&state, "d1").unwrap();
        let rev = put_fleet(&app, "v2").await;
        let (status, body) = create_rollout(
            &app,
            &json!({ "revision": rev,
                     "cohort": { "devices": ["d1", "d2"] },
                     "waves": [["d1"], ["d2"]],
                     "gate": strict_gate() }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        (
            body["rolloutId"].as_str().unwrap().to_string(),
            t1,
            t2,
            rev,
            baseline_rev,
        )
        // <- server "dies" here, wave 0 advanced + soaking
    };

    let (app, state) = app(dir.path()); // startup IS recovery
    let (st, wave, _) = rollout_state(&state, &id);
    assert_eq!((st.as_str(), wave), ("active", 0), "rollout state survived the crash");
    assert_eq!(rendered_revision(&state, "d1"), Some(rev), "advanced device still advanced");
    assert_eq!(
        rendered_revision(&state, "d2"),
        Some(baseline_rev),
        "held device still held — startup render reconcile honors targets"
    );

    // The march continues from exactly where it stopped.
    report_status(&app, &t1, "d1", "installed", 1).await;
    rollouts::tick(&state).unwrap();
    rollouts::tick(&state).unwrap();
    assert_eq!(rendered_revision(&state, "d2"), Some(rev));
    report_status(&app, &t2, "d2", "installed", 1).await;
    rollouts::tick(&state).unwrap();
    assert_eq!(rollout_state(&state, &id).0, "completed");
    assert_eq!(target_of(&state, "d1"), None);
    assert_eq!(target_of(&state, "d2"), None);
}

/// §11.5: no automatic rollback, ever — recovery from a bad revision
/// is a NEW rollout whose revision carries the old content. Content
/// reverts; manifestVersion only ever climbs.
#[tokio::test]
async fn rollback_is_a_new_rollout_to_reverted_content() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let t1 = add_device(&state, "d1", None, &json!({}));
    author_fleet(&app, "v1").await;
    let digest_v1 = content_digest(&state, "d1").unwrap();
    let rev_bad = put_fleet(&app, "v2-bad").await;

    // Roll out the bad revision; it fails and is aborted.
    let (_, body) = create_rollout(
        &app,
        &json!({ "revision": rev_bad, "cohort": { "devices": ["d1"] }, "gate": strict_gate() }),
    )
    .await;
    let id_bad = body["rolloutId"].as_str().unwrap().to_string();
    assert_eq!(rendered_revision(&state, "d1"), Some(rev_bad));
    report_status(&app, &t1, "d1", "failed", 1).await;
    rollouts::tick(&state).unwrap();
    assert_eq!(rollout_state(&state, &id_bad).0, "paused");
    post_action(&app, &id_bad, "abort").await;
    // §11.5: abort moved nothing backward — the device still holds the
    // bad revision.
    assert_eq!(rendered_revision(&state, "d1"), Some(rev_bad));
    let version_after_bad = manifest_version(&state, "d1").unwrap();

    // Undo = a NEW revision with the OLD content (D13), rolled out like
    // any other. Creation takes over the aborted rollout's hold.
    let rev_fix = put_fleet(&app, "v1").await;
    assert!(rev_fix > rev_bad);
    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev_fix, "cohort": { "devices": ["d1"] }, "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "aborted rollout must not block: {body}");
    let id_fix = body["rolloutId"].as_str().unwrap().to_string();
    assert_eq!(rendered_revision(&state, "d1"), Some(rev_fix));
    report_status(&app, &t1, "d1", "installed", 2).await;
    rollouts::tick(&state).unwrap();
    assert_eq!(rollout_state(&state, &id_fix).0, "completed");

    // Content reverted byte-for-byte; the version counter never did.
    assert_eq!(content_digest(&state, "d1"), Some(digest_v1));
    assert!(
        manifest_version(&state, "d1").unwrap() > version_after_bad,
        "manifestVersion is strictly increasing even when content reverts (§11.5/§10.2)"
    );
}

/// Devices outside the cohort are untouched by the rollout: they track
/// head as always, carry no hold, and appear in no wave.
#[tokio::test]
async fn devices_outside_cohort_are_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", None, &json!({}));
    add_device(&state, "d2", None, &json!({}));
    add_device(&state, "d3", None, &json!({}));
    author_fleet(&app, "v1").await;
    let baseline_rev = rendered_revision(&state, "d3").unwrap();
    let rev = put_fleet(&app, "v2").await;

    let (_, body) = create_rollout(
        &app,
        &json!({ "revision": rev,
                 "cohort": { "devices": ["d1", "d3"] },
                 "waves": [["d1"], ["d3"]],
                 "gate": strict_gate() }),
    )
    .await;
    let id = body["rolloutId"].as_str().unwrap().to_string();

    // Outside the cohort: head-tracking (already at the new head via
    // the commit render hook), no hold, no wave membership.
    assert_eq!(rendered_revision(&state, "d2"), Some(rev));
    assert_eq!(target_of(&state, "d2"), None);
    let detail = rollout_status(&app, &id).await;
    for wave in detail["waves"].as_array().unwrap() {
        for d in wave["devices"].as_array().unwrap() {
            assert_ne!(d["deviceId"], "d2");
        }
    }
    // Meanwhile the cohort's later wave IS held.
    assert_eq!(rendered_revision(&state, "d3"), Some(baseline_rev));

    // A fresh commit while the rollout runs: outsiders follow head,
    // held cohort devices do not move (§11.2 — the rollout owns their
    // manifest timing).
    let rev3 = put_fleet(&app, "v3").await;
    assert_eq!(rendered_revision(&state, "d2"), Some(rev3));
    assert_eq!(rendered_revision(&state, "d3"), Some(baseline_rev));
    assert_eq!(rendered_revision(&state, "d1"), Some(rev));
}

/// Creation-time validation: overlapping active cohorts are rejected
/// (§11.1), unknown revisions and empty cohorts are 422.
#[tokio::test]
async fn creation_rejects_overlap_and_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    add_device(&state, "d1", None, &json!({}));
    add_device(&state, "d2", None, &json!({}));
    author_fleet(&app, "v1").await;
    let rev = put_fleet(&app, "v2").await;

    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev,
                 "cohort": { "devices": ["d1", "d2"] },
                 "waves": [["d1"], ["d2"]],
                 "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // §11.1: never interleave manifest advancement for one device from
    // two rollouts.
    let (status, body) = create_rollout(
        &app,
        &json!({ "revision": rev, "cohort": { "devices": ["d2"] }, "gate": strict_gate() }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, _) =
        create_rollout(&app, &json!({ "revision": 9999, "cohort": { "devices": ["d1"] } })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = create_rollout(
        &app,
        &json!({ "revision": rev, "cohort": { "labels": {"nope": "x"} } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "empty cohort");

    let (status, _) = create_rollout(
        &app,
        &json!({ "revision": rev, "cohort": { "devices": ["ghost"] } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "unknown device");

    // List surfaces both the running rollout and nothing else broken.
    let (status, list) = send(
        &app,
        Request::get("/api/rollouts").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["state"], "active");
    assert_eq!(list[0]["waveCount"], 2);
    assert_eq!(list[0]["deviceCount"], 2);
}
