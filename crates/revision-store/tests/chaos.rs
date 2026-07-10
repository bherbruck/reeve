//! Chaos check (Law 3, crash-only): SIGKILL the process mid-commit, reopen,
//! assert the store is consistent and the last good revision is intact.
//!
//! Pattern: this test re-invokes its own test binary with an env flag. The
//! child seeds one known-good revision, touches a marker file, then commits
//! large revisions in a tight loop forever. The parent waits for the marker,
//! lets the loop run, SIGKILLs the child mid-write, then reopens the
//! database and verifies integrity + a fresh commit.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use revision_store::{RevisionStore, Stream};

const CHAOS_DB_ENV: &str = "REVISION_STORE_CHAOS_DB";
const CHAOS_MARKER_ENV: &str = "REVISION_STORE_CHAOS_MARKER";

const SEED_PATH: &str = "seed.txt";
const SEED_CONTENT: &[u8] = b"known-good revision, must survive kill -9";

/// Child role: seed a good revision, signal readiness, then hammer commits
/// until killed. Never returns.
fn chaos_child(db: &str, marker: &str) -> ! {
    let mut store = RevisionStore::open(db).expect("child: open");
    store
        .commit([(SEED_PATH, SEED_CONTENT)], "chaos", "seed", Stream::Local)
        .expect("child: seed commit");
    // Atomic-enough signal for the parent: create-after-commit.
    std::fs::write(marker, b"ready").expect("child: marker");

    // Large distinct payloads so each transaction spends real time writing:
    // 256 files x 8 KiB, content varied per iteration (defeats the
    // idempotency short-circuit).
    let mut n: u64 = 0;
    loop {
        n += 1;
        let files: Vec<(String, Vec<u8>)> = (0..256)
            .map(|i| {
                let mut content = vec![0u8; 8192];
                content[..8].copy_from_slice(&n.to_le_bytes());
                content[8..16].copy_from_slice(&(i as u64).to_le_bytes());
                (format!("bulk/{i:03}.bin"), content)
            })
            .collect();
        store
            .commit(files, "chaos", &format!("bulk {n}"), Stream::Local)
            .expect("child: bulk commit");
    }
}

#[test]
fn chaos_kill_nine_mid_commit() {
    // Child re-entry point.
    if let (Ok(db), Ok(marker)) = (
        std::env::var(CHAOS_DB_ENV),
        std::env::var(CHAOS_MARKER_ENV),
    ) {
        chaos_child(&db, &marker);
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("chaos.db");
    let marker = dir.path().join("ready");

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .args(["chaos_kill_nine_mid_commit", "--exact", "--nocapture"])
        .env(CHAOS_DB_ENV, &db)
        .env(CHAOS_MARKER_ENV, &marker)
        .spawn()
        .expect("spawn child");

    // Wait for the seed revision, then let the bulk-commit loop run briefly
    // so the SIGKILL lands mid-transaction with high probability.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child never signalled readiness");
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("child exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(150));

    // SIGKILL on unix — no shutdown ceremony, mid-write by construction.
    child.kill().expect("kill -9 child");
    child.wait().expect("reap child");

    verify_consistent(&db);
}

/// Startup IS recovery: plain open must succeed and observe a consistent
/// store — last committed revision whole, no orphans, still writable.
fn verify_consistent(db: &PathBuf) {
    // Raw engine-level checks first.
    {
        let conn = rusqlite::Connection::open(db).expect("raw reopen");
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .expect("integrity_check");
        assert_eq!(integrity, "ok", "integrity_check failed after kill -9");
        let fk_violations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check()",
                [],
                |r| r.get(0),
            )
            .expect("foreign_key_check");
        assert_eq!(fk_violations, 0, "orphaned rows after kill -9");
        // No partial revision: every revision's manifest digests resolve.
        let dangling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM revision_files rf
                 LEFT JOIN blobs b ON b.digest = rf.digest
                 WHERE b.digest IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("dangling digests query");
        assert_eq!(dangling, 0, "revision references missing blob");
    }

    // API-level: seed revision intact, heads readable, store writable.
    let mut store = RevisionStore::open(db).expect("reopen after kill");
    let head = store
        .head(Stream::Local)
        .expect("head")
        .expect("at least the seed revision must have survived");

    // Revision 1 (the seed) is the known-good anchor.
    assert_eq!(
        store.read_at(1, SEED_PATH).expect("read seed").as_deref(),
        Some(SEED_CONTENT),
        "seed revision content lost"
    );

    // Whatever the head is, its full tree must be readable (a revision is
    // visible only when whole — one tx per commit).
    let tree = store.tree_at(head).expect("tree_at head");
    assert!(!tree.is_empty());
    for path in tree.keys() {
        assert!(
            store.read_at(head, path).expect("read head file").is_some(),
            "head revision {head} missing content for {path}"
        );
    }

    // Still writable: a fresh commit lands on top.
    let next = store
        .commit(
            [("post-crash.txt", b"resumed".as_slice())],
            "chaos",
            "post-crash",
            Stream::Local,
        )
        .expect("commit after crash");
    assert!(next > head);
    assert_eq!(store.revision(next).expect("revision").parent, Some(head));
}
