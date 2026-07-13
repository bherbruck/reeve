//! End-to-end deploy-log capture (REV-011, ext-logs) driven through the
//! FULL loop: the REAL reeve-server + the REAL reeve-agent
//! (poll -> pull -> converge -> record_reports -> record_logs -> send),
//! with a [`FakeProvider`] that CAPTURES combined compose output exactly
//! as `CommandComposeProvider` does. This is the operator-facing promise:
//! when a deploy fails, the WHY (full `docker compose up` output) is
//! uploaded by the agent and readable back on the reeve human routes —
//! beyond the one-line reason that still rides in the Margo status body.
//!
//! Margo compliance (spec/reeve/01-framework.md §3.1): every log surface
//! here is a NEW reeve endpoint (device upload under
//! `/api/reeve/v1/devices/{id}/logs`; human read under
//! `/api/devices/{id}/logs...`). The Margo-native `DeploymentStatus.error`
//! one-liner is untouched and still observable as the deployment state.
//! Gated on the e2e `ext` feature: with ext-logs compiled out (the
//! conformance build) this file is not compiled at all, proving the
//! capability is additive and never load-bearing for the core loop.

#![cfg(feature = "ext")]

use e2e::{
    Author, FakeProvider, TestAgent, author_web_app, boot, boot_with_log_retain, enroll_device,
};
use reqwest::StatusCode;

/// Resolve the single deployment id the server currently lists for a
/// device — the key the ext-logs list route is scoped by. It is the
/// SAME `deployment_id` the agent stamped on both its Margo status
/// report and its uploaded log, so it round-trips.
async fn deployment_of(author: &Author, device_id: &str) -> (String, String) {
    let deps = author.device_deployments(device_id).await;
    let dep = deps.first().unwrap_or_else(|| panic!("a deployment for {device_id}: {deps:?}"));
    let id = dep["deploymentId"].as_str().expect("deploymentId").to_string();
    let state = dep["state"].as_str().expect("state").to_string();
    (id, state)
}

/// HEADLINE: a FAILED apply uploads its captured compose output; an
/// operator lists the failure log and reads the FULL failure text back —
/// while the Margo status still shows `failed` (the one-line reason path
/// is untouched).
#[tokio::test]
async fn failed_deploy_uploads_log_and_operator_reads_failure_text() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    author_web_app(&author).await;
    let token = enroll_device(&srv.state, "dev-1", None);

    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();

    let provider = FakeProvider::new();
    provider.error_app("web");
    provider.set_output(
        "web",
        "compose up web\n\
         Error: Head \"https://registry/v2/reeve/nginx\": denied: pull access denied\n\
         failed to solve: nginx:1.25: not found\n",
    );

    // One full loop pass: pull the bundle, apply web (fails), upload the
    // Margo status AND the captured deploy log.
    let out = agent.tick(&provider).await;
    assert!(out.acted.contains(&"web".to_string()), "web converged this pass: {out:?}");
    assert_eq!(provider.up_count("web"), 1);

    // Margo-native one-liner path: the deployment reads `failed`.
    let (deployment_id, state) = deployment_of(&author, "dev-1").await;
    assert!(state.eq_ignore_ascii_case("failed"), "Margo status is failed, got {state:?}");

    // reeve extension: the full compose output is listed on the human
    // route (newest first), tagged failed/up.
    let (st, body) = author
        .get_json(&format!("/api/devices/dev-1/logs?deployment={deployment_id}"))
        .await;
    assert_eq!(st, StatusCode::OK, "list logs: {body}");
    let logs = body["logs"].as_array().expect("logs array");
    assert_eq!(logs.len(), 1, "exactly one uploaded failure log: {logs:?}");
    assert_eq!(logs[0]["outcome"], "failed");
    assert_eq!(logs[0]["phase"], "up");
    assert_eq!(logs[0]["appId"], "web");
    // Meta list carries no body text (the read route does).
    assert!(logs[0].get("text").is_none());
    let log_id = logs[0]["id"].as_str().expect("log id").to_string();

    // Read it back: the FULL failure text — the WHY beyond the one-liner.
    let (st, body) = author.get_json(&format!("/api/devices/dev-1/logs/{log_id}")).await;
    assert_eq!(st, StatusCode::OK);
    let text = body["text"].as_str().expect("text");
    assert!(text.contains("pull access denied"), "failure detail present: {text:?}");
    assert!(text.contains("not found"), "full output retained: {text:?}");
    // The agent also kept a local copy that outlives an offline window.
    assert!(
        agent.path().join("logs/web.log").is_file(),
        "agent persisted the deploy log locally (Law 5)"
    );
}

/// A SUCCESSFUL deploy also stores a log (latest-wins forensic record),
/// tagged applied/up.
#[tokio::test]
async fn successful_deploy_also_stores_a_log() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    author_web_app(&author).await;
    let token = enroll_device(&srv.state, "dev-1", None);

    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();

    let provider = FakeProvider::new(); // no failure => converges installed
    let out = agent.tick(&provider).await;
    assert!(out.acted.contains(&"web".to_string()), "web converged: {out:?}");

    let (deployment_id, state) = deployment_of(&author, "dev-1").await;
    assert!(state.eq_ignore_ascii_case("installed"), "Margo status installed, got {state:?}");

    let (st, body) = author
        .get_json(&format!("/api/devices/dev-1/logs?deployment={deployment_id}"))
        .await;
    assert_eq!(st, StatusCode::OK, "list logs: {body}");
    let logs = body["logs"].as_array().expect("logs array");
    assert_eq!(logs.len(), 1, "the successful apply stored a log too");
    assert_eq!(logs[0]["outcome"], "applied");
    assert_eq!(logs[0]["phase"], "up");
    let log_id = logs[0]["id"].as_str().unwrap().to_string();

    let (st, body) = author.get_json(&format!("/api/devices/dev-1/logs/{log_id}")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["text"].as_str().unwrap().contains("compose up web"), "{body}");
}

/// Retention bounds the store: a persistently-failing app re-applies
/// every converge pass (phase `failed` != `applied`), so each tick
/// uploads a fresh log for the SAME (device, deployment) — the server
/// keeps at most N (config = 3) newest.
#[tokio::test]
async fn retention_keeps_at_most_n_deploy_logs() {
    let srv = boot_with_log_retain(3).await;
    let author = Author::new(&srv.base());
    author_web_app(&author).await;
    let token = enroll_device(&srv.state, "dev-1", None);

    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();

    let provider = FakeProvider::new();
    provider.error_app("web"); // up fails => re-applies + re-uploads each pass

    for i in 0..6 {
        provider.set_output("web", &format!("compose up web (attempt {i})\nError: still failing\n"));
        agent.tick(&provider).await;
    }
    assert_eq!(provider.up_count("web"), 6, "a failing app re-applies every pass");

    let (deployment_id, _) = deployment_of(&author, "dev-1").await;
    let (st, body) = author
        .get_json(&format!("/api/devices/dev-1/logs?deployment={deployment_id}"))
        .await;
    assert_eq!(st, StatusCode::OK, "list logs: {body}");
    let logs = body["logs"].as_array().expect("logs array");
    assert_eq!(logs.len(), 3, "retention keeps at most N=3, got {}", logs.len());

    // Newest-first: the surviving logs are the last three attempts.
    let newest_id = logs[0]["id"].as_str().unwrap();
    let (_, body) = author.get_json(&format!("/api/devices/dev-1/logs/{newest_id}")).await;
    assert!(body["text"].as_str().unwrap().contains("attempt 5"), "newest is the last attempt: {body}");
}
