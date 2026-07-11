//! E1 core loop (docs/build-charter.md, Milestone-1 harness as an
//! automated test): author a layer via the server API -> render ->
//! agent polls the manifest -> pulls the bundle -> converges with a
//! FAKE provider -> reports status -> the server shows it. Plus the
//! idempotence and re-converge legs. ALL of this is CORE — it compiles
//! and passes under `--no-default-features` (the E2 conformance build).

use e2e::{Author, FakeProvider, TestAgent, author_web_app, boot, enroll_device};
use reeve_agent::PollOutcome;

/// THE headline: the whole desired-state loop, server + agent, no
/// mocks except the workload provider. Author -> render (hooked on the
/// commit) -> agent poll (accept, epoch 0) -> OCI pull + verify ->
/// converge (up -d web) -> status report -> `GET /api/devices` shows
/// `installed`.
#[tokio::test]
async fn core_loop_author_render_converge_report() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", Some("plant-a"));

    author_web_app(&author).await;

    let provider = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover(); // startup IS recovery (Law 3), idempotent no-op here

    let out = agent.tick(&provider).await;
    // First poll accepts the render at epoch 0 (no restore has fenced).
    let PollOutcome::Accepted { epoch_bump, manifest, .. } = &out.poll else {
        panic!("first poll must accept, got {:?}", out.poll);
    };
    assert!(!epoch_bump, "no epoch bump on a fresh server");
    assert_eq!(manifest.manifest_version.epoch(), 0);
    assert_eq!(out.acted, ["web"], "converge acted on exactly the web app");
    assert_eq!(provider.up_count("web"), 1, "web was up -d exactly once");

    // The rendered compose landed on the box with ${REEVE_REGISTRY}
    // resolved from server config — proof the pull delivered the real
    // per-device bundle, not a placeholder.
    let compose = agent.app_compose("web").expect("web compose in bundle");
    assert!(compose.contains("registry.example:5000/nginx:1.25"), "{compose}");

    // The server shows the reported status (store-and-forward completed
    // synchronously inside the tick).
    let state = author.deployment_state("dev-1").await;
    assert_eq!(state.as_deref(), Some("installed"), "server fleet view shows installed");
}

/// A second tick with nothing changed is a silent no-op (D5): the poll
/// returns 304 and converge acts on nothing — the agent does not
/// re-`up` an already-converged app.
#[tokio::test]
async fn converged_tick_is_silent_noop() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", None);
    author_web_app(&author).await;

    let provider = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();

    let first = agent.tick(&provider).await;
    assert_eq!(first.acted, ["web"]);
    assert_eq!(provider.up_count("web"), 1);

    let second = agent.tick(&provider).await;
    assert!(matches!(second.poll, PollOutcome::NotModified), "got {:?}", second.poll);
    assert!(second.acted.is_empty(), "converged pass acts on nothing");
    assert_eq!(provider.up_count("web"), 1, "no gratuitous re-up");

    let state = author.deployment_state("dev-1").await;
    assert_eq!(state.as_deref(), Some("installed"));
}

/// A REAL desired-state change (a device-layer param) bumps the
/// manifest by one counter step, ships a new bundle, and the agent
/// re-converges exactly that app.
#[tokio::test]
async fn real_change_reconverges() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", None);
    author_web_app(&author).await;

    let provider = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();
    let first = agent.tick(&provider).await;
    let PollOutcome::Accepted { manifest: m0, .. } = first.poll else {
        panic!("first accept");
    };
    assert_eq!(provider.up_count("web"), 1);

    // Change dev-1's own layer: a real content change for this device.
    let changed = author
        .put_layer("30-device.dev-1", &[("apps/web/params.yaml", "greeting: bumped\n")])
        .await;
    assert!(changed);

    let second = agent.tick(&provider).await;
    let PollOutcome::Accepted { manifest: m1, epoch_bump, .. } = second.poll else {
        panic!("second accept, got change");
    };
    assert!(!epoch_bump);
    assert_eq!(m1.manifest_version.epoch(), 0);
    assert_eq!(m1.manifest_version.counter(), m0.manifest_version.counter() + 1);
    assert_eq!(second.acted, ["web"], "re-converged the changed app");
    assert_eq!(provider.up_count("web"), 2, "app re-upped once for the change");
}

/// Multiple devices at different sites render different bundles from
/// the same tree; each agent converges its own render (the layer chain
/// is a render input). Proves per-device delivery end-to-end.
#[tokio::test]
async fn two_devices_get_distinct_renders() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let ta = enroll_device(&srv.state, "dev-a", Some("plant-a"));
    let tb = enroll_device(&srv.state, "dev-b", Some("plant-b"));
    author_web_app(&author).await;
    author
        .put_layer("20-site.plant-a", &[("apps/web/params.yaml", "greeting: hello-plant-a\n")])
        .await;

    let pa = FakeProvider::new();
    let mut agent_a = TestAgent::http(&srv.base(), "dev-a", &ta);
    agent_a.recover();
    agent_a.tick(&pa).await;

    let pb = FakeProvider::new();
    let mut agent_b = TestAgent::http(&srv.base(), "dev-b", &tb);
    agent_b.recover();
    agent_b.tick(&pb).await;

    assert_eq!(pa.up_count("web"), 1);
    assert_eq!(pb.up_count("web"), 1);
    let dep_a = std::fs::read_to_string(
        agent_a.store.current_path().join("apps/web/deployment.yaml"),
    )
    .unwrap();
    let dep_b = std::fs::read_to_string(
        agent_b.store.current_path().join("apps/web/deployment.yaml"),
    )
    .unwrap();
    assert!(dep_a.contains("hello-plant-a"), "site layer applied: {dep_a}");
    assert!(!dep_b.contains("hello-plant-a"), "other site unaffected");

    // Both devices show installed on the server fleet view.
    assert_eq!(author.deployment_state("dev-a").await.as_deref(), Some("installed"));
    assert_eq!(author.deployment_state("dev-b").await.as_deref(), Some("installed"));
}
