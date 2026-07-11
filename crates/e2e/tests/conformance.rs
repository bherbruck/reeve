//! E2 conformance (docs/build-charter.md CODE BOUNDARY): the core loop
//! must stand alone with EVERY extension compiled out. The dedicated
//! entrypoint is `cargo test -p e2e --no-default-features`, which builds
//! a core-only reeve-server + reeve-agent and runs the ungated e2e
//! (core_loop / chaos / epoch_restore) against them — proving no
//! extension is load-bearing for the base loop.
//!
//! This file adds the additivity ASSERTION that the boundary is real:
//! capability advertisement is derived from compiled-in features
//! (01-framework §3.3 — "an agent literally cannot advertise what it
//! doesn't contain"), so a core build advertises ZERO extensions while
//! a full build advertises them all. The same test source proves both
//! directions depending on the feature set it was built with.

use e2e::{Author, author_web_app, boot, enroll_device};
use reeve_agent::source::ManifestSource;

/// The full desired-state loop runs regardless of feature set — the
/// same author -> render -> poll -> pull -> converge -> report path,
/// asserted here as the conformance entrypoint's headline (it also runs
/// as core_loop.rs; duplicated deliberately so `--no-default-features`
/// has a self-describing conformance test).
#[tokio::test]
async fn core_loop_runs_with_current_feature_set() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", None);
    author_web_app(&author).await;

    let provider = e2e::FakeProvider::new();
    let mut agent = e2e::TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();
    let out = agent.tick(&provider).await;
    assert!(matches!(out.poll, reeve_agent::PollOutcome::Accepted { .. }));
    assert_eq!(out.acted, ["web"]);
    assert_eq!(provider.up_count("web"), 1);
    assert_eq!(author.deployment_state("dev-1").await.as_deref(), Some("installed"));
}

/// Capability advertisement is a function of the compiled feature set.
/// The agent's own probe (source::probe_capabilities) is the client the
/// real agent uses at startup — drive it end-to-end.
#[tokio::test]
async fn capabilities_reflect_the_compiled_feature_set() {
    let srv = boot().await;
    let token = enroll_device(&srv.state, "dev-1", None);
    let source = ManifestSource::parse(&srv.base(), Some(token)).unwrap();

    let caps = source
        .probe_capabilities()
        .await
        .expect("reeve server advertises capabilities");
    assert_eq!(caps.server_version, env!("CARGO_PKG_VERSION"));

    let has = |rev: &str| caps.extensions.iter().any(|e| e.starts_with(rev));
    // rev-004/1 (health & status journal) is UNCONDITIONAL core — its
    // ingest routes are always compiled in — so both builds advertise
    // it. The gated extensions are the discriminator.
    assert!(has("rev-004"), "the core health-journal extension is always advertised");

    #[cfg(feature = "ext")]
    {
        // Full build: the ext-* family is compiled in and advertised.
        assert!(has("rev-001"), "ext-channel advertised in a full build");
        assert!(has("rev-002"), "ext-terminal advertised in a full build");
        assert!(has("rev-003"), "ext-sse advertised in a full build");
        assert!(has("rev-009"), "ext-secrets advertised in a full build");
    }
    #[cfg(not(feature = "ext"))]
    {
        // Core build (conformance): an agent literally cannot advertise
        // what it does not contain (01-framework §3.3) — every gated
        // extension is ABSENT from the advertisement.
        for rev in ["rev-001", "rev-002", "rev-003", "rev-009"] {
            assert!(!has(rev), "core build must NOT advertise {rev}, got {:?}", caps.extensions);
        }
    }
}
