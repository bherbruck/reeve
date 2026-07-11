//! E1 restore-with-epoch-bump (docs/build-charter.md; spec/reeve/
//! 07-durability.md §9.5, spec/reeve/08-packaging.md §10.2): snapshot a
//! live server, let the agent advance its anti-rollback floor, then
//! restore an OLDER snapshot into a fresh data dir (disaster recovery).
//! The restore fences the epoch; the restored server re-renders under
//! it. The SAME agent — whose floor is now AHEAD of the restored
//! counter — accepts the post-restore manifest as a NOTABLE epoch bump
//! instead of rejecting it as a rollback. That is the whole point of
//! epoch fencing, proven end-to-end.
//!
//! Snapshot + restore + epoch fencing are CORE (only the changeset
//! CAPTURE tier is `ext-durability-changeset`), so this runs in the
//! conformance build too.

use e2e::{
    Author, FakeProvider, author_web_app, boot_snapshot, config_snapshot, enroll_device, open_agent,
    serve_router,
};
use reeve_agent::{PollOutcome, converge, poll_once, record_reports, resolve_desired};
use reeve_server::keyfile;
use reeve_types::reeve::manifest::ManifestVersion;

/// Poll `agent_data` against `base` and return the accepted manifest
/// version (asserting acceptance).
async fn poll_accept(agent_data: &std::path::Path, base: &str, token: &str) -> (ManifestVersion, bool) {
    let (mut db, store, source, _bundle) = open_agent(agent_data, base, token);
    store.recover(&mut db).unwrap();
    match poll_once(&mut db, &source).await {
        PollOutcome::Accepted { manifest, epoch_bump, .. } => {
            (manifest.manifest_version, epoch_bump)
        }
        other => panic!("expected accept against {base}, got {other:?}"),
    }
}

#[tokio::test]
async fn restore_bumps_epoch_and_agent_accepts_what_would_be_a_rollback() {
    // Phase 1: a snapshot-tier server, an app, an enrolled device.
    let srv = boot_snapshot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", None);
    author_web_app(&author).await;

    let agent_data = tempfile::tempdir().unwrap();

    // Agent accepts the first render at epoch 0, and converges.
    let provider = FakeProvider::new();
    let (v_snapshot, bump0) = {
        let (mut db, store, source, bundle) = open_agent(agent_data.path(), &srv.base(), &token);
        store.recover(&mut db).unwrap();
        let out = poll_once(&mut db, &source).await;
        let PollOutcome::Accepted { manifest, epoch_bump, .. } = out else {
            panic!("epoch-0 accept");
        };
        store.sync(&mut db, &bundle).await.unwrap();
        let desired = resolve_desired(&db, &store);
        let reports = converge(&mut db, agent_data.path(), &provider, &desired);
        record_reports(&db, &reports);
        (manifest.manifest_version, epoch_bump)
    };
    assert!(!bump0);
    assert_eq!(v_snapshot.epoch(), 0);

    // Take the snapshot HERE — it captures dev-1's manifest at
    // v_snapshot (counter C0).
    srv.state.durability.snapshot_now().await.unwrap().expect("generation cut");

    // Now push the agent's floor AHEAD: two real changes, two accepts.
    author.put_layer("30-device.dev-1", &[("apps/web/params.yaml", "greeting: change-1\n")]).await;
    let (v1, _) = poll_accept(agent_data.path(), &srv.base(), &token).await;
    author.put_layer("30-device.dev-1", &[("apps/web/params.yaml", "greeting: change-2\n")]).await;
    let (v_floor, _) = poll_accept(agent_data.path(), &srv.base(), &token).await;
    assert_eq!(v_floor.epoch(), 0);
    assert!(v_floor > v1 && v1 > v_snapshot, "floor advanced past the snapshot counter");

    // Phase 2: disaster recovery of the OLDER snapshot into a new data
    // dir. DR needs the keyfile too (§9.5/§9.6). restore-at-bootstrap
    // increments the fencing epoch at the target and stamps it in.
    let target = srv.target_dir.as_ref().unwrap().path();
    let dr_data = tempfile::tempdir().unwrap();
    std::fs::copy(
        srv.data_dir.path().join(keyfile::KEY_FILE_NAME),
        dr_data.path().join(keyfile::KEY_FILE_NAME),
    )
    .unwrap();
    let dr_cfg = config_snapshot(dr_data.path(), target);
    reeve_server::durability::restore::restore_at_bootstrap(&dr_cfg)
        .await
        .expect("restore-at-bootstrap");

    let (addr2, state2) = serve_router(config_snapshot(dr_data.path(), target)).await;
    let base2 = format!("http://{addr2}");
    let author2 = Author::new(&base2);
    {
        let conn = state2.db.lock().unwrap();
        let epoch: String = conn
            .query_row("SELECT value FROM settings WHERE key = 'server_epoch'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(epoch, "1", "restore fencing incremented the epoch");
    }

    // Increment-then-serve (§9.5): the restored server re-renders dev-1
    // under the fenced epoch on the next content change. The restored
    // counter is back at C0, so this new manifest packs (epoch 1, ~C0+1)
    // — a counter BELOW the agent's floor (epoch 0, C_floor).
    author2.put_layer("30-device.dev-1", &[("apps/web/params.yaml", "greeting: after-restore\n")]).await;

    // The SAME agent polls the restored server. Without fencing this is
    // a rollback (lower counter) and MUST be rejected; WITH the higher
    // epoch it is a notable, accepted bump.
    let (mut db, store, source, bundle) = open_agent(agent_data.path(), &base2, &token);
    store.recover(&mut db).unwrap();
    let out = poll_once(&mut db, &source).await;
    let PollOutcome::Accepted { manifest, epoch_bump, .. } = out else {
        panic!("post-restore manifest MUST be accepted, got {out:?}");
    };
    assert!(epoch_bump, "epoch increase is a notable bump");
    assert_eq!(manifest.manifest_version.epoch(), 1, "served under the fenced epoch");
    assert!(
        manifest.manifest_version.counter() <= v_floor.counter(),
        "the restored counter ({}) is at/below the pre-restore floor ({}) — \
         it would be a rollback at the same epoch, and is only safe BECAUSE the \
         epoch fenced",
        manifest.manifest_version.counter(),
        v_floor.counter()
    );

    // Journaled as notable, and the floor advanced to epoch 1.
    assert!(
        db.journal_entries()
            .unwrap()
            .iter()
            .any(|e| e.severity == "notable" && e.event == "manifest-epoch-bump"),
        "epoch bump journaled as notable, not a security regression"
    );
    assert_eq!(db.last_accepted().unwrap().unwrap().version.epoch(), 1);

    // …and it converges normally against the restored server.
    store.sync(&mut db, &bundle).await.unwrap();
    let desired = resolve_desired(&db, &store);
    let reports = converge(&mut db, agent_data.path(), &provider, &desired);
    record_reports(&db, &reports);
    assert!(provider.up_count("web") >= 1);
}
