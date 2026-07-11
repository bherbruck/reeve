//! C10 federation end-to-end (spec/reeve/06-federation.md REV-005):
//! TWO in-process reeve-servers — a root on a real localhost listener
//! and a gateway child whose sync client dials it — exercising:
//! - §8.2 revision sync (conditional GET head, blob fetch, verbatim
//!   append) and the two-stream render at the child;
//! - §8.4 ownership both directions (child refuses hub layers, root
//!   refuses delegated site layers);
//! - §8.3 status forwarding (same journal protocol, idempotent by
//!   `(deviceId, seq)`, tier-origin device rows at the parent);
//! - §8.2 resume after an interrupted blob fetch;
//! - §8.5 air-gap export/import (round-trip equals the network sync
//!   result; idempotent re-import; tampered archive rejected whole);
//! - 10-secrets §12.5 scoped secret sync, re-encrypted per tier.
#![cfg(feature = "ext-federation")]

use std::net::SocketAddr;
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

use reeve_server::config::{AuthMode, Config, FederationConfig};
use reeve_server::ext::federation::{
    self, ExportOptions, SIGNATURE_FILE, sync_tick,
};
use reeve_server::state::AppState;
use reeve_server::{auth, router};
use revision_store::Stream;

// ------------------------------------------------------------- harness

const SITE: &str = "plant-a";
const CHILD_TIER: &str = "plant-a-gw";

fn root_config(data_dir: &FsPath) -> Config {
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

fn child_config(data_dir: &FsPath, upstream: &str, token: &str) -> Config {
    Config {
        federation: Some(FederationConfig {
            upstream: upstream.to_string(),
            token: token.to_string(),
            site: SITE.to_string(),
            sync_interval_secs: 3600, // ticks are driven manually here
        }),
        ..root_config(data_dir)
    }
}

fn app(cfg: Config) -> (Router, AppState) {
    let state = reeve_server::bootstrap(cfg).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state)
}

/// Serve a router on a real TCP listener (the child's reqwest client
/// needs an actual socket).
async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    addr
}

/// Root + tier token + serving listener; child wired at it.
async fn root_and_child(
    root_dir: &FsPath,
    child_dir: &FsPath,
) -> (Router, AppState, Router, AppState) {
    let (root_router, root_state) = app(root_config(root_dir));
    let token = {
        let conn = root_state.db.lock().unwrap();
        federation::issue_tier_token(
            &conn,
            CHILD_TIER,
            SITE,
            &federation::DEFAULT_SYNC_PREFIXES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "test-admin",
            None,
        )
        .unwrap()
    };
    let addr = serve(root_router.clone()).await;
    let (child_router, child_state) =
        app(child_config(child_dir, &format!("http://{addr}"), &token));
    (root_router, root_state, child_router, child_state)
}

fn add_device(state: &AppState, id: &str, site: Option<&str>) {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at, site)
         VALUES (?1, 'box', 'x86_64', '0.1.0', 0, ?2)",
        params![id, site],
    )
    .unwrap();
}

async fn send_json(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
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
// Same renderable package fixture family as delivery_flow.rs.

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

/// Author the fleet-wide web app at the ROOT: package vendored +
/// fleet layer referencing it (two local revisions).
async fn author_web_app(root: &Router) {
    let (status, body) = send_json(
        root,
        put_files(
            "/api/tree/packages/web/1.0.0",
            &[("margo.yaml", PKG_MANIFEST), ("compose.yml", PKG_COMPOSE)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send_json(
        root,
        put_files(
            "/api/tree/layers/00-fleet",
            &[("apps/web/app.yaml", "package:\n  name: web\n  version: 1.0.0\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn origin_head(state: &AppState) -> Option<(i64, i64)> {
    let store = state.revisions.lock().unwrap();
    store.origin_head(Stream::Upstream).unwrap()
}

fn local_head(state: &AppState) -> i64 {
    let store = state.revisions.lock().unwrap();
    store.head(Stream::Local).unwrap().unwrap_or(0)
}

fn manifest_json(state: &AppState, device: &str) -> Option<String> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT manifest_json FROM device_manifests WHERE device_id = ?1",
        params![device],
        |r| r.get(0),
    )
    .ok()
}

/// The child's upstream stream as (origin id, file manifest) pairs —
/// the §8.5 "equals the sync result" comparator.
fn upstream_stream(state: &AppState) -> Vec<(i64, std::collections::BTreeMap<String, String>)> {
    let store = state.revisions.lock().unwrap();
    let mut out = Vec::new();
    let mut cursor = store.head(Stream::Upstream).unwrap();
    while let Some(id) = cursor {
        let rev = store.revision(id).unwrap();
        cursor = rev.parent;
        let origin = store.origin_of(id).unwrap().expect("upstream rows carry origins");
        out.push((origin, store.tree_at(id).unwrap()));
    }
    out.reverse();
    out
}

// --------------------------------------------------------------- tests

/// §8.2 e2e: author at root -> child syncs -> child renders with both
/// streams -> the child device's manifest reflects root content.
#[tokio::test]
async fn author_at_root_sync_renders_on_child() {
    let root_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    let (root_router, root_state, _child_router, child_state) =
        root_and_child(root_dir.path(), child_dir.path()).await;

    author_web_app(&root_router).await;
    add_device(&child_state, "dev-1", Some(SITE));

    let report = sync_tick(&child_state).await.expect("sync tick");
    assert_eq!(report.appended, 2, "package + fleet layer revisions");
    let (up_row, up_origin) = origin_head(&child_state).expect("upstream stream populated");
    assert_eq!(up_origin, local_head(&root_state), "origin ids ARE the parent's ids");

    // Verbatim: parent authorship preserved on the synced stream.
    {
        let store = child_state.revisions.lock().unwrap();
        let rev = store.revision(up_row).unwrap();
        assert_eq!(rev.stream, Stream::Upstream);
        assert_eq!(rev.author, "anonymous", "root's author, verbatim");
    }

    // Two-stream render: the device manifest exists and names the app
    // that lives ONLY in the upstream stream (child authored nothing).
    let manifest = manifest_json(&child_state, "dev-1").expect("child rendered dev-1");
    assert!(manifest.contains("\"web\""), "manifest must reference the fleet app: {manifest}");
    assert_eq!(local_head(&child_state), 0, "child authored no local revision");

    // Second tick: conditional GET says current — nothing transferred.
    let report = sync_tick(&child_state).await.expect("second tick");
    assert_eq!(report.appended, 0);
    assert_eq!(report.blobs_fetched, 0);
    assert_eq!(origin_head(&child_state).unwrap().1, up_origin);

    // Root authors more; the child catches up incrementally.
    let (status, _) = send_json(
        &root_router,
        put_files(
            "/api/tree/layers/10-region.emea",
            &[("params.yaml", "params:\n  greeting: hej\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let report = sync_tick(&child_state).await.expect("third tick");
    assert_eq!(report.appended, 1);
    assert_eq!(origin_head(&child_state).unwrap().1, local_head(&root_state));
}

/// §8.4 both directions: the child refuses hub-owned layers (gateway
/// ownership set) and the root refuses the site layer it delegated
/// (tier-token scope enforcement) — plus the upstream stream is
/// unwritable structurally (ownership.rs unit tests cover that arm).
#[tokio::test]
async fn ownership_is_enforced_in_both_directions() {
    let root_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    let (root_router, _root_state, child_router, _child_state) =
        root_and_child(root_dir.path(), child_dir.path()).await;

    // Child: fleet layer is hub-owned — 403 (§8.4).
    let (status, body) = send_json(
        &child_router,
        put_files("/api/tree/layers/00-fleet", &[("x.yaml", "a: 1\n")]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Child: packages are hub-vendored under the default grant — 403.
    let (status, _) = send_json(
        &child_router,
        put_files("/api/tree/packages/web/1.0.0", &[("margo.yaml", PKG_MANIFEST)]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Child: its OWN site + device layers are writable.
    for layer in [format!("20-site.{SITE}"), "30-device.dev-9".to_string()] {
        let (status, body) = send_json(
            &child_router,
            put_files(&format!("/api/tree/layers/{layer}"), &[("params.yaml", "params: {}\n")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "layer {layer}: {body}");
    }

    // Root: the delegated site layer is refused (single writer, §8.4)…
    let (status, body) = send_json(
        &root_router,
        put_files(
            &format!("/api/tree/layers/20-site.{SITE}"),
            &[("params.yaml", "params: {}\n")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"].as_str().unwrap().contains(CHILD_TIER), "{body}");
    // …a sibling site label is NOT captured by the delegation…
    let (status, _) = send_json(
        &root_router,
        put_files("/api/tree/layers/20-site.plant-a2", &[("params.yaml", "params: {}\n")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // …and hub layers remain the root's to author.
    let (status, _) = send_json(
        &root_router,
        put_files("/api/tree/layers/00-fleet", &[("x.yaml", "a: 1\n")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// §8.3: journaled status forwards child -> parent with the same
/// protocol, idempotently; the device appears at the parent marked
/// with its tier origin and is NEVER rendered there.
#[tokio::test]
async fn status_forwards_upstream_idempotently() {
    use device_api::status::StatusIngest as _;
    use reeve_types::reeve::health::{JournalBatch, JournalRecord, JournalRecordKind};

    let root_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    let (_root_router, root_state, _child_router, child_state) =
        root_and_child(root_dir.path(), child_dir.path()).await;
    add_device(&child_state, "dev-1", Some(SITE));

    // Journal three records at the child (an agent's backfill).
    let ingest = reeve_server::ingest::SqliteStatusIngest::new(
        child_state.db.clone(),
        child_state.events.clone(),
    );
    let record = |seq: u64| JournalRecord {
        seq,
        observed_at: format!("2026-07-10T00:00:0{seq}Z"),
        kind: JournalRecordKind::Lifecycle,
        payload: Some(json!({ "event": "converge", "seq": seq })),
    };
    ingest
        .ingest_journal("dev-1", &JournalBatch { records: (1..=3).map(record).collect() })
        .unwrap();

    let count_at_root = || -> i64 {
        let conn = root_state.db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM status_journal WHERE device_id = 'dev-1'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };

    sync_tick(&child_state).await.expect("tick with forwarding");
    assert_eq!(count_at_root(), 3, "records forwarded");
    {
        let conn = root_state.db.lock().unwrap();
        let (origin, observed): (Option<String>, String) = conn
            .query_row(
                "SELECT d.tier_origin, j.observed_at
                 FROM devices d JOIN status_journal j ON j.device_id = d.device_id
                 WHERE d.device_id = 'dev-1' AND j.seq = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin.as_deref(), Some(CHILD_TIER), "tier-origin marked (§8.3)");
        assert_eq!(observed, "2026-07-10T00:00:01Z", "original timestamp preserved");
        // Forwarded devices are never rendered at the parent (§8.6).
        let manifests: i64 = conn
            .query_row("SELECT COUNT(*) FROM device_manifests WHERE device_id='dev-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(manifests, 0);
    }

    // Idempotent: another tick forwards nothing new.
    sync_tick(&child_state).await.expect("idempotent tick");
    assert_eq!(count_at_root(), 3);

    // New records catch up from the cursor.
    ingest
        .ingest_journal("dev-1", &JournalBatch { records: vec![record(4)] })
        .unwrap();
    sync_tick(&child_state).await.expect("catch-up tick");
    assert_eq!(count_at_root(), 4);
}

/// §8.2 resumability: a sync killed after fetching SOME blobs resumes
/// by fetching only what is still missing — simulated by pre-seeding
/// one content blob at the child (exactly the state an interrupted
/// fetch leaves, since blob inserts are individually durable).
#[tokio::test]
async fn sync_resumes_after_interrupted_blob_fetch() {
    let root_dir = tempfile::tempdir().unwrap();
    let child_a = tempfile::tempdir().unwrap();
    let child_b = tempfile::tempdir().unwrap();
    let (root_router, root_state, _ra, child_state_a) =
        root_and_child(root_dir.path(), child_a.path()).await;
    author_web_app(&root_router).await;

    // Baseline: a fresh child fetches N blobs.
    let baseline = sync_tick(&child_state_a).await.unwrap();
    assert!(baseline.blobs_fetched >= 2, "fixture has at least two blobs");

    // Second child, pre-seeded with one blob (the interrupted state).
    let token = {
        let conn = root_state.db.lock().unwrap();
        federation::issue_tier_token(
            &conn,
            "plant-a-gw-2",
            SITE,
            &federation::DEFAULT_SYNC_PREFIXES.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "test-admin",
            None,
        )
        .unwrap()
    };
    let addr = serve(router::build(root_state.clone())).await;
    let (_router_b, child_state_b) =
        app(child_config(child_b.path(), &format!("http://{addr}"), &token));
    let (seed_digest, seed_bytes) = {
        let store = root_state.revisions.lock().unwrap();
        let head = store.head(Stream::Local).unwrap().unwrap();
        let tree = store.tree_at(head).unwrap();
        let digest = tree.values().next().unwrap().clone();
        let bytes = store.blob(&digest).unwrap().unwrap();
        (digest, bytes)
    };
    {
        let mut store = child_state_b.revisions.lock().unwrap();
        store.put_blob(&seed_digest, &seed_bytes).unwrap();
        assert!(
            store.origin_head(Stream::Upstream).unwrap().is_none(),
            "blob without revision is invisible (closure rule)"
        );
    }

    let resumed = sync_tick(&child_state_b).await.expect("resumed sync");
    assert_eq!(
        resumed.blobs_fetched,
        baseline.blobs_fetched - 1,
        "already-held blob skipped (resume by digest)"
    );
    assert_eq!(resumed.appended, baseline.appended);
    assert_eq!(
        upstream_stream(&child_state_b),
        upstream_stream(&child_state_a),
        "resumed sync converges to the identical stream"
    );
}

/// §8.5: export at the root -> import at a fresh gateway equals the
/// network sync result; re-import is a no-op; a tampered archive is
/// rejected whole (nothing appended).
#[tokio::test]
async fn airgap_export_import_roundtrip_idempotent_and_tamperproof() {
    let root_dir = tempfile::tempdir().unwrap();
    let child_net = tempfile::tempdir().unwrap();
    let child_media = tempfile::tempdir().unwrap();
    let child_tampered = tempfile::tempdir().unwrap();
    let export_dir = tempfile::tempdir().unwrap();
    let (root_router, root_state, _r, child_state_net) =
        root_and_child(root_dir.path(), child_net.path()).await;
    author_web_app(&root_router).await;
    sync_tick(&child_state_net).await.unwrap();

    let archive = export_dir.path().join("media");
    federation::export_tree(&root_state, &archive, &ExportOptions::default()).unwrap();

    // Fresh air-gapped gateway (upstream URL never dialed).
    let (_r2, child_state_media) = app(child_config(
        child_media.path(),
        "http://192.0.2.1:1", // TEST-NET, never reached
        "rvt_unused",
    ));
    let report = federation::import_archive(&child_state_media, &archive, None).unwrap();
    assert_eq!(report.revisions_appended, 2);
    assert_eq!(
        upstream_stream(&child_state_media),
        upstream_stream(&child_state_net),
        "sneakernet import equals the network sync result (§8.5)"
    );
    // The import rendered too: same two-stream pipeline.
    add_device(&child_state_media, "dev-media", Some(SITE));
    reeve_server::render::ensure_current(&child_state_media, "dev-media").unwrap();
    assert!(manifest_json(&child_state_media, "dev-media").unwrap().contains("\"web\""));

    // Idempotent re-import (Law 3 applied to sneakernet).
    let again = federation::import_archive(&child_state_media, &archive, None).unwrap();
    assert_eq!(again.revisions_appended, 0);
    assert_eq!(again.revisions_already_present, 2);

    // Tamper with one content blob: rejected whole, nothing appended.
    let (_r3, child_state_t) = app(child_config(
        child_tampered.path(),
        "http://192.0.2.1:1",
        "rvt_unused",
    ));
    let tampered = export_dir.path().join("tampered");
    copy_dir(&archive, &tampered);
    let victim = std::fs::read_dir(tampered.join("blobs/sha256"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .max_by_key(|p| std::fs::metadata(p).unwrap().len())
        .unwrap();
    let mut bytes = std::fs::read(&victim).unwrap();
    bytes[0] ^= 1;
    std::fs::write(&victim, bytes).unwrap();
    let err = federation::import_archive(&child_state_t, &tampered, None).unwrap_err();
    assert!(err.to_string().contains("mismatch") || err.to_string().contains("tampered"), "{err}");
    assert!(origin_head(&child_state_t).is_none(), "tampered import wrote NOTHING");

    // Tamper with the signed index: signature refuses the archive.
    let resigned = export_dir.path().join("badindex");
    copy_dir(&archive, &resigned);
    let mut index = std::fs::read(resigned.join("index.json")).unwrap();
    let last = index.len() - 2;
    index[last] ^= 1;
    std::fs::write(resigned.join("index.json"), index).unwrap();
    let err = federation::import_archive(&child_state_t, &resigned, None).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("signature"), "{err}");

    // Wrong signer expectation refuses too (§8.7 key pinning).
    let err =
        federation::import_archive(&child_state_media, &archive, Some("bm90LXRoZS1rZXk="))
            .unwrap_err();
    assert!(err.to_string().contains("expect-signer"), "{err}");

    // The signature side-file exists next to a stock OCI layout.
    assert!(archive.join("oci-layout").exists());
    assert!(archive.join(SIGNATURE_FILE).exists());
}

fn copy_dir(from: &FsPath, to: &FsPath) {
    std::fs::create_dir_all(to).unwrap();
    for entry in walk(from) {
        let rel = entry.strip_prefix(from).unwrap();
        let dest = to.join(rel);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(&entry, &dest).unwrap();
    }
}

fn walk(dir: &FsPath) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// 10-secrets §12.5: the child pulls ONLY its subtree's secrets,
/// re-encrypted under its own keyfile; rotations and deletions
/// propagate on the next tick.
#[cfg(feature = "ext-secrets")]
#[tokio::test]
async fn scoped_secret_sync_is_subtree_only_and_re_encrypted() {
    use reeve_server::ext::secrets;
    use std::collections::BTreeSet;

    let root_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    let (_root_router, root_state, _r, child_state) =
        root_and_child(root_dir.path(), child_dir.path()).await;

    let root_key = secrets::vault_key(&root_state.cfg.data_dir).unwrap();
    {
        let conn = root_state.db.lock().unwrap();
        secrets::put(&conn, &root_key, "db-password", "fleet", "hunter2").unwrap();
        secrets::put(&conn, &root_key, "site-secret", &format!("site.{SITE}"), "ours").unwrap();
        secrets::put(&conn, &root_key, "other-site", "site.plant-b", "not-ours").unwrap();
        secrets::put(&conn, &root_key, "op-secret", secrets::INTERNAL_SCOPE, "never").unwrap();
    }

    sync_tick(&child_state).await.expect("secret sync tick");

    let child_key = secrets::vault_key(&child_state.cfg.data_dir).unwrap();
    assert_ne!(root_key, child_key, "per-tier keyfiles (D15)");
    {
        let conn = child_state.db.lock().unwrap();
        let rows: Vec<(String, String, Option<String>)> = {
            let mut stmt = conn
                .prepare("SELECT name, scope, origin FROM secrets ORDER BY name")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap();
            rows.collect::<Result<_, _>>().unwrap()
        };
        assert_eq!(
            rows.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>(),
            ["db-password", "site-secret"],
            "ONLY the subtree's secrets arrived (§12.5): {rows:?}"
        );
        assert!(rows.iter().all(|(_, _, o)| o.as_deref() == Some("upstream")));

        // Re-encrypted: the child's OWN key opens them; the ciphertext
        // is not the parent's row.
        let names = BTreeSet::from(["db-password".to_string(), "site-secret".to_string()]);
        let chain = secrets::device_chain("dev-x", None, None, Some(SITE));
        let got = secrets::resolve_values(&conn, &child_key, &chain, &names).unwrap();
        assert_eq!(got["db-password"].value, "hunter2");
        assert_eq!(got["db-password"].version, 1);
        assert_eq!(got["site-secret"].value, "ours");
        let child_ct: Vec<u8> = conn
            .query_row(
                "SELECT ciphertext FROM secrets WHERE name='db-password'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let root_conn = root_state.db.lock().unwrap();
        let root_ct: Vec<u8> = root_conn
            .query_row(
                "SELECT ciphertext FROM secrets WHERE name='db-password' AND scope='fleet'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(child_ct, root_ct, "re-encrypted under the child's keyfile");
    }

    // Rotation at the hub propagates with its version, verbatim.
    {
        let conn = root_state.db.lock().unwrap();
        secrets::put(&conn, &root_key, "db-password", "fleet", "sw0rdfish").unwrap();
        secrets::delete(&conn, "site-secret", &format!("site.{SITE}")).unwrap();
    }
    sync_tick(&child_state).await.expect("rotation tick");
    {
        let conn = child_state.db.lock().unwrap();
        let names = BTreeSet::from(["db-password".to_string(), "site-secret".to_string()]);
        let chain = secrets::device_chain("dev-x", None, None, Some(SITE));
        let got = secrets::resolve_values(&conn, &child_key, &chain, &names).unwrap();
        assert_eq!(got["db-password"].value, "sw0rdfish");
        assert_eq!(got["db-password"].version, 2, "hub version, verbatim");
        assert!(!got.contains_key("site-secret"), "deletion propagated (pruned)");
    }
}

/// §8.5 return trip: export-status at the child, import at the parent
/// — same idempotency as live forwarding.
#[tokio::test]
async fn airgap_status_return_trip() {
    use device_api::status::StatusIngest as _;
    use reeve_types::reeve::health::{JournalBatch, JournalRecord, JournalRecordKind};

    let root_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    let export_dir = tempfile::tempdir().unwrap();
    let (_root_router, root_state, _r, child_state) =
        root_and_child(root_dir.path(), child_dir.path()).await;
    add_device(&child_state, "dev-1", Some(SITE));

    let ingest = reeve_server::ingest::SqliteStatusIngest::new(
        child_state.db.clone(),
        child_state.events.clone(),
    );
    ingest
        .ingest_journal(
            "dev-1",
            &JournalBatch {
                records: vec![JournalRecord {
                    seq: 1,
                    observed_at: "2026-07-10T00:00:01Z".into(),
                    kind: JournalRecordKind::Lifecycle,
                    payload: Some(json!({ "event": "start" })),
                }],
            },
        )
        .unwrap();

    let archive = export_dir.path().join("status.tar");
    federation::export_status(&child_state, &archive).unwrap();
    let report = federation::import_archive(&root_state, &archive, None).unwrap();
    assert_eq!(report.journal_records, 1);
    let again = federation::import_archive(&root_state, &archive, None).unwrap();
    assert_eq!(again.journal_records, 1, "re-ingest is a (deviceId, seq) no-op");

    let conn = root_state.db.lock().unwrap();
    let (count, origin): (i64, Option<String>) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM status_journal WHERE device_id='dev-1'),
                    (SELECT tier_origin FROM devices WHERE device_id='dev-1')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1, "idempotent by (deviceId, seq) — §7.3 at every hop");
    assert_eq!(origin.as_deref(), Some("airgap"));
}
