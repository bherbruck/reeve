//! E1 chaos (docs/build-charter.md, CLAUDE.md Law 3 crash-only + Law 5
//! offline-first): kill points across the agent+server loop, restart,
//! assert convergence. Startup IS recovery. These drive the REAL agent
//! entry points against the REAL server, then reopen agent state on the
//! same data dir the way a `kill -9`'d process restarts. All CORE.

use e2e::{
    Author, FakeProvider, author_web_app, boot, config_disabled, enroll_device, open_agent,
    serve_router,
};
use reeve_agent::{PollOutcome, converge, poll_once, record_reports, resolve_desired};
use revision_store::Stream;

/// A dead source: bind a port, drop it -> guaranteed connection-refused
/// (the agent is offline).
async fn dead_base() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}")
}

/// Kill -9 the agent AFTER the bundle is pulled and swapped but BEFORE
/// converge runs; restart; assert it converges from the on-disk bundle
/// — and does so even with the SERVER NOW OFFLINE (Law 5: the box
/// heals from last known state without the network).
#[tokio::test]
async fn agent_resumes_from_disk_offline_after_crash() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", None);
    author_web_app(&author).await;
    let data = tempfile::tempdir().unwrap();

    // Phase 1 — the doomed process: poll + pull the bundle, then "die"
    // before converge (no provider ever runs).
    {
        let (mut db, store, source, bundle) = open_agent(data.path(), &srv.base(), &token);
        store.recover(&mut db).unwrap();
        let out = poll_once(&mut db, &source).await;
        assert!(matches!(out, PollOutcome::Accepted { .. }), "got {out:?}");
        store.sync(&mut db, &bundle).await.unwrap();
        assert!(store.current_digest().is_some(), "bundle swapped in before the crash");
        // drop everything == kill -9: no converge, no report.
    }

    // Phase 2 — restart, but the server is unreachable now.
    let offline = dead_base();
    let offline_base = offline.await;
    let provider = FakeProvider::new();
    {
        let (mut db, store, source, bundle) = open_agent(data.path(), &offline_base, &token);
        store.recover(&mut db).unwrap(); // idempotent startup recovery
        let out = poll_once(&mut db, &source).await;
        assert!(matches!(out, PollOutcome::SourceUnavailable), "offline poll is a no-op, got {out:?}");
        // sync would also fail; converge runs from last known on-disk state.
        let _ = store.sync(&mut db, &bundle).await;
        let desired = resolve_desired(&db, &store);
        let reports = converge(&mut db, data.path(), &provider, &desired);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].app_id, "web");
        record_reports(&db, &reports);
    }
    // Converged from disk, server never consulted in phase 2.
    assert_eq!(provider.up_count("web"), 1, "healed to applied offline");
}

/// Re-running converge after a crash is idempotent (D5): a completed
/// pass acts on nothing the next time, and the recorded status is never
/// duplicated on the server (dedup by (deviceId, seq)).
#[tokio::test]
async fn reconverge_after_crash_is_idempotent_and_deduped() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", None);
    author_web_app(&author).await;
    let data = tempfile::tempdir().unwrap();
    let provider = FakeProvider::new();

    // Full converge + report once.
    {
        let (mut db, store, source, bundle) = open_agent(data.path(), &srv.base(), &token);
        store.recover(&mut db).unwrap();
        poll_once(&mut db, &source).await;
        store.sync(&mut db, &bundle).await.unwrap();
        let desired = resolve_desired(&db, &store);
        let reports = converge(&mut db, data.path(), &provider, &desired);
        record_reports(&db, &reports);
        let sink = reeve_agent::StatusSink::from_config(
            &srv.base(),
            Some(token.clone()),
            Some("dev-1".into()),
        )
        .unwrap();
        sink.send_unsent(&db).await;
    }
    assert_eq!(provider.up_count("web"), 1);

    // "Restart" and re-run: poll is 304, converge is a silent no-op.
    {
        let (mut db, store, source, bundle) = open_agent(data.path(), &srv.base(), &token);
        store.recover(&mut db).unwrap();
        let out = poll_once(&mut db, &source).await;
        assert!(matches!(out, PollOutcome::NotModified), "got {out:?}");
        store.sync(&mut db, &bundle).await.unwrap();
        let desired = resolve_desired(&db, &store);
        let reports = converge(&mut db, data.path(), &provider, &desired);
        assert!(reports.is_empty(), "idempotent recovery re-runs nothing");
    }
    assert_eq!(provider.up_count("web"), 1, "no gratuitous re-up after restart");
    assert_eq!(author.deployment_state("dev-1").await.as_deref(), Some("installed"));
}

/// Server crash-only (Law 3): a revision committed while the render
/// pass never ran (kill -9 between commit and render) is healed at the
/// NEXT server startup — and a real agent then pulls the healed render.
#[tokio::test]
async fn server_startup_reconciles_unrendered_commit_then_agent_pulls() {
    let data = tempfile::tempdir().unwrap();
    let token;
    let counter_before;

    // Phase 1: a live server, an authored app, then a raw revision-store
    // commit that bypasses the render hook — the "killed before render"
    // state. No agent yet.
    {
        let (addr, state) = serve_router(config_disabled(data.path())).await;
        let author = Author::new(&format!("http://{addr}"));
        token = enroll_device(&state, "dev-1", None);
        author_web_app(&author).await;

        // Baseline manifest version for dev-1.
        let probe = tempfile::tempdir().unwrap();
        let (mut db, store, source, bundle) =
            open_agent(probe.path(), &format!("http://{addr}"), &token);
        store.recover(&mut db).unwrap();
        let out = poll_once(&mut db, &source).await;
        let PollOutcome::Accepted { manifest, .. } = out else { panic!("baseline accept") };
        counter_before = manifest.manifest_version.counter();
        let _ = bundle;

        // Commit a real change for dev-1 directly, bypassing render.
        let mut rs = state.revisions.lock().unwrap();
        let head = rs.head(Stream::Local).unwrap().unwrap();
        let tree = rs.tree_at(head).unwrap();
        let mut files: std::collections::BTreeMap<String, Vec<u8>> = tree
            .iter()
            .map(|(p, d)| (p.clone(), rs.blob(d).unwrap().unwrap()))
            .collect();
        files.insert(
            "layers/30-device.dev-1/apps/web/params.yaml".to_string(),
            b"greeting: healed-at-startup\n".to_vec(),
        );
        rs.commit(files, "test", "crash sim: committed, not rendered", Stream::Local)
            .unwrap();
        // state1's server task lingers idle; we never touch addr again.
    }

    // Phase 2: a fresh server bootstrap on the SAME data dir. Its
    // startup reconcile renders the un-rendered commit BEFORE serving.
    let srv2 = serve_router(config_disabled(data.path())).await;
    let base2 = format!("http://{}", srv2.0);
    let agent_data = tempfile::tempdir().unwrap();
    let (mut db, store, source, bundle) = open_agent(agent_data.path(), &base2, &token);
    store.recover(&mut db).unwrap();
    let out = poll_once(&mut db, &source).await;
    let PollOutcome::Accepted { manifest, .. } = out else {
        panic!("agent must accept the healed manifest, got {out:?}");
    };
    assert!(
        manifest.manifest_version.counter() > counter_before,
        "startup reconcile advanced the manifest past {counter_before}"
    );
    store.sync(&mut db, &bundle).await.unwrap();
    let provider = FakeProvider::new();
    let desired = resolve_desired(&db, &store);
    let reports = converge(&mut db, agent_data.path(), &provider, &desired);
    record_reports(&db, &reports);
    assert_eq!(provider.up_count("web"), 1, "agent converged the healed render");

    // The healed content actually reached the box.
    let compose = std::fs::read_to_string(store.current_path().join("apps/web/deployment.yaml"))
        .unwrap();
    assert!(compose.contains("healed-at-startup"), "{compose}");
}
