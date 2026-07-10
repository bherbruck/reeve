//! Status ingest + presence over the real router (C5).
//!
//! Spec sources: spec/margo/.../deployment-status.md (path + payload
//! shape, response codes), spec/reeve/05-health-journal.md §7.3 (reeve
//! additive fields, backfill batches, `(deviceId, seq)` idempotency,
//! original timestamps, ack = highest contiguous seq),
//! spec/reeve/01-framework.md §3.2 (vanilla reports must ingest).

use std::path::Path as FsPath;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use rusqlite::params;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use reeve_server::config::{AuthMode, Config};
use reeve_server::presence::{self, PresenceState};
use reeve_server::{auth, device_tokens, router, state::AppState};

// ------------------------------------------------------------- harness

fn config(data_dir: &FsPath) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth: AuthMode::None,
        session_ttl_secs: 3600,
        registry_endpoint: "registry.example:5000".to_string(),
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

fn post_json(uri: &str, token: Option<&str>, body: &Value) -> Request<Body> {
    let mut b = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// Margo-shaped status report (`deployment-status.md` example shape)
/// with the rev-004/1 `reeve` additive object.
fn status_manifest(deployment_id: &str, state: &str, seq: u64, observed_at: &str) -> Value {
    json!({
        "apiVersion": "deployment.margo.org/v1alpha1",
        "kind": "DeploymentStatusManifest",
        "deploymentId": deployment_id,
        "status": { "state": state },
        "components": [{ "name": "web-stack", "state": state }],
        "reeve": { "observedAt": observed_at, "seq": seq }
    })
}

fn status_uri(device: &str, deployment: &str) -> String {
    format!("/api/v1/clients/{device}/deployments/{deployment}/status")
}

fn current_row(state: &AppState, device: &str, deployment: &str) -> Option<(String, Option<i64>, Option<String>)> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT state, seq, observed_at FROM deployment_status_current
         WHERE device_id = ?1 AND deployment_id = ?2",
        params![device, deployment],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .ok()
}

fn journal_rows(state: &AppState, device: &str) -> Vec<(i64, String, String)> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT seq, observed_at, kind FROM status_journal
             WHERE device_id = ?1 ORDER BY seq",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![device], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn last_seen(state: &AppState, device: &str) -> Option<i64> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT last_seen_at FROM devices WHERE device_id = ?1",
        params![device],
        |r| r.get(0),
    )
    .unwrap()
}

// --------------------------------------------------------------- tests

/// Happy path: a live Margo-shaped report lands in the journal, the
/// current-state table, and touches last_seen (presence goes online).
#[tokio::test]
async fn live_ingest_updates_journal_current_and_presence() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1");

    assert!(last_seen(&state, "dev-1").is_none());
    let p = presence::device_presence(&state, "dev-1").unwrap().unwrap();
    assert_eq!(p.state, PresenceState::Offline);
    assert_eq!(p.since, None, "never seen");

    let body = status_manifest("dep-1", "installing", 1, "2026-07-10T06:00:00Z");
    let (status, _) = send(&app, post_json(&status_uri("dev-1", "dep-1"), Some(&token), &body)).await;
    assert_eq!(status, StatusCode::OK);

    assert!(last_seen(&state, "dev-1").is_some(), "ingest touches last_seen");
    let p = presence::device_presence(&state, "dev-1").unwrap().unwrap();
    assert_eq!(p.state, PresenceState::Online);
    assert_eq!(p.since, last_seen(&state, "dev-1"));

    let (st, seq, obs) = current_row(&state, "dev-1", "dep-1").expect("current row");
    assert_eq!(st, "installing");
    assert_eq!(seq, Some(1));
    assert_eq!(obs.as_deref(), Some("2026-07-10T06:00:00Z"));

    let journal = journal_rows(&state, "dev-1");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0], (1, "2026-07-10T06:00:00Z".to_string(), "status".to_string()));

    // Unknown device is unknown, not offline-with-a-guess.
    assert!(presence::device_presence(&state, "nope").unwrap().is_none());
}

/// §7.3: the server MUST NOT overwrite an already-ingested
/// `(deviceId, seq)` record — a resend (even a corrupted/altered one)
/// leaves the original timestamps and payload intact.
#[tokio::test]
async fn duplicate_seq_is_idempotent_and_preserves_original() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1");
    let uri = status_uri("dev-1", "dep-1");

    let first = status_manifest("dep-1", "installing", 7, "2026-07-10T06:00:00Z");
    let (status, _) = send(&app, post_json(&uri, Some(&token), &first)).await;
    assert_eq!(status, StatusCode::OK);

    // Same seq, different content and timestamp (crash-resend or worse).
    let second = status_manifest("dep-1", "failed", 7, "2026-07-10T09:99:99Z");
    let (status, _) = send(&app, post_json(&uri, Some(&token), &second)).await;
    assert_eq!(status, StatusCode::OK, "resend is harmless, not an error");

    let journal = journal_rows(&state, "dev-1");
    assert_eq!(journal.len(), 1, "no duplicate row");
    assert_eq!(journal[0].1, "2026-07-10T06:00:00Z", "original timestamp preserved");
    let payload: String = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT payload FROM status_journal WHERE device_id='dev-1' AND seq=7",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(payload.contains("installing"), "original payload preserved");
}

/// Current state = max seq, not max arrival: a late lower-seq report
/// (out-of-order live delivery) never regresses the current state, but
/// IS journaled at its original timestamp.
#[tokio::test]
async fn out_of_order_arrival_does_not_regress_current_state() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1");
    let uri = status_uri("dev-1", "dep-1");

    let newer = status_manifest("dep-1", "installed", 10, "2026-07-10T06:10:00Z");
    let (status, _) = send(&app, post_json(&uri, Some(&token), &newer)).await;
    assert_eq!(status, StatusCode::OK);

    let older = status_manifest("dep-1", "installing", 5, "2026-07-10T06:05:00Z");
    let (status, _) = send(&app, post_json(&uri, Some(&token), &older)).await;
    assert_eq!(status, StatusCode::OK);

    let (st, seq, _) = current_row(&state, "dev-1", "dep-1").unwrap();
    assert_eq!(st, "installed", "late arrival must not regress");
    assert_eq!(seq, Some(10));

    // Both are history, each at its original time (forensic, §7.3).
    let journal = journal_rows(&state, "dev-1");
    assert_eq!(
        journal,
        vec![
            (5, "2026-07-10T06:05:00Z".to_string(), "status".to_string()),
            (10, "2026-07-10T06:10:00Z".to_string(), "status".to_string()),
        ]
    );
}

/// Backfill batch (§7.3): gap records allowed, status records feed the
/// current table, ack is the highest CONTIGUOUS seq — a hole stops it,
/// filling the hole advances it, resending is idempotent.
#[tokio::test]
async fn journal_backfill_with_gap_and_holes() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1");
    let uri = "/api/reeve/v1/journal/dev-1";

    let status_payload = status_manifest("dep-1", "installed", 3, "2026-07-10T06:03:00Z");
    let batch = json!({ "records": [
        { "seq": 1, "observedAt": "2026-07-10T06:01:00Z", "kind": "lifecycle",
          "payload": { "event": "start" } },
        { "seq": 2, "observedAt": "2026-07-10T06:02:00Z", "kind": "gap" },
        { "seq": 3, "observedAt": "2026-07-10T06:03:00Z", "kind": "status",
          "payload": status_payload },
        // hole at 4-5
        { "seq": 6, "observedAt": "2026-07-10T06:06:00Z", "kind": "health",
          "payload": { "load": [0.5, 0.4, 0.3] } }
    ]});
    let (status, ack) = send(&app, post_json(uri, Some(&token), &batch)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["ackedSeq"], 3, "hole at 4 stops the ack: {ack}");

    // The backfilled status record materialized current state.
    let (st, seq, obs) = current_row(&state, "dev-1", "dep-1").expect("current from backfill");
    assert_eq!(st, "installed");
    assert_eq!(seq, Some(3));
    assert_eq!(obs.as_deref(), Some("2026-07-10T06:03:00Z"));

    // All four records journaled, gap included, original timestamps.
    let kinds: Vec<(i64, String)> = journal_rows(&state, "dev-1")
        .into_iter()
        .map(|(s, _, k)| (s, k))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (1, "lifecycle".to_string()),
            (2, "gap".to_string()),
            (3, "status".to_string()),
            (6, "health".to_string()),
        ]
    );

    // Fill the hole: ack advances through everything.
    let fill = json!({ "records": [
        { "seq": 4, "observedAt": "2026-07-10T06:04:00Z", "kind": "lifecycle",
          "payload": { "event": "converge-begin" } },
        { "seq": 5, "observedAt": "2026-07-10T06:05:00Z", "kind": "lifecycle",
          "payload": { "event": "converge-end" } }
    ]});
    let (status, ack) = send(&app, post_json(uri, Some(&token), &fill)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["ackedSeq"], 6);

    // Idempotent resend of the whole original batch: same ack, no dups.
    let (status, ack) = send(&app, post_json(uri, Some(&token), &batch)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["ackedSeq"], 6);
    assert_eq!(journal_rows(&state, "dev-1").len(), 6);

    // Empty batch is a valid ack query.
    let (status, ack) = send(&app, post_json(uri, Some(&token), &json!({ "records": [] }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["ackedSeq"], 6);
}

/// Auth boundary: no token is 401 (middleware); a valid token for the
/// WRONG device is 403 on both surfaces, and nothing is persisted.
#[tokio::test]
async fn wrong_device_token_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let _token1 = add_device(&state, "dev-1");
    let token2 = add_device(&state, "dev-2");

    let body = status_manifest("dep-1", "installed", 1, "2026-07-10T06:00:00Z");
    let uri = status_uri("dev-1", "dep-1");

    let (status, _) = send(&app, post_json(&uri, None, &body)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = send(&app, post_json(&uri, Some(&token2), &body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let batch = json!({ "records": [
        { "seq": 1, "observedAt": "2026-07-10T06:00:00Z", "kind": "gap" }
    ]});
    let (status, _) = send(&app, post_json("/api/reeve/v1/journal/dev-1", None, &batch)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) =
        send(&app, post_json("/api/reeve/v1/journal/dev-1", Some(&token2), &batch)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    assert!(journal_rows(&state, "dev-1").is_empty());
    assert!(current_row(&state, "dev-1", "dep-1").is_none());
    assert!(last_seen(&state, "dev-1").is_none());
}

/// Degradation (spec/reeve/01-framework.md §3.2): a vanilla Margo
/// report — no `reeve` key — MUST ingest: current state materializes
/// (arrival-ordered), presence updates; nothing is journaled (there is
/// no journal identity without a seq).
#[tokio::test]
async fn vanilla_margo_report_ingests_without_journal() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1");

    let body = json!({
        "apiVersion": "deployment.margo.org/v1alpha1",
        "kind": "DeploymentStatusManifest",
        "deploymentId": "dep-1",
        "status": { "state": "pending" },
        "components": [{ "name": "web-stack", "state": "pending" }]
    });
    let (status, _) = send(&app, post_json(&status_uri("dev-1", "dep-1"), Some(&token), &body)).await;
    assert_eq!(status, StatusCode::OK);

    let (st, seq, obs) = current_row(&state, "dev-1", "dep-1").unwrap();
    assert_eq!(st, "pending");
    assert_eq!(seq, None);
    assert_eq!(obs, None);
    assert!(journal_rows(&state, "dev-1").is_empty());
    assert!(last_seen(&state, "dev-1").is_some());
}

/// The manifest poll is also a presence signal: an idle agent that only
/// polls (304s forever) must read as online.
#[tokio::test]
async fn manifest_poll_touches_last_seen() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path());
    let token = add_device(&state, "dev-1");

    assert!(last_seen(&state, "dev-1").is_none());
    let req = Request::get("/api/reeve/v1/manifest")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(last_seen(&state, "dev-1").is_some());
    assert_eq!(
        presence::device_presence(&state, "dev-1").unwrap().unwrap().state,
        PresenceState::Online
    );
}
