//! API tests for the revision store: commit/read/tree/diff/blame,
//! idempotency (D13/D14), two independent streams (federation §8.2),
//! reopen persistence, and D6 open-mode pragmas.

use revision_store::{Change, Error, RevisionStore, Stream, digest_of};

fn store() -> (tempfile::TempDir, RevisionStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let s = RevisionStore::open(dir.path().join("store.db")).expect("open");
    (dir, s)
}

#[test]
fn commit_read_tree_roundtrip() {
    let (_dir, mut s) = store();
    let id = s
        .commit(
            [("a.txt", b"alpha".as_slice()), ("d/b.txt", b"beta")],
            "op",
            "initial",
            Stream::Local,
        )
        .unwrap();

    assert_eq!(s.read_at(id, "a.txt").unwrap().as_deref(), Some(b"alpha".as_slice()));
    assert_eq!(s.read_at(id, "d/b.txt").unwrap().as_deref(), Some(b"beta".as_slice()));
    assert_eq!(s.read_at(id, "missing").unwrap(), None);

    let tree = s.tree_at(id).unwrap();
    assert_eq!(tree.len(), 2);
    assert_eq!(tree["a.txt"], digest_of(b"alpha"));
    assert_eq!(tree["d/b.txt"], digest_of(b"beta"));

    let rev = s.revision(id).unwrap();
    assert_eq!(rev.stream, Stream::Local);
    assert_eq!(rev.parent, None);
    assert_eq!(rev.author, "op");
    assert_eq!(rev.message, "initial");
}

#[test]
fn digest_grammar_is_sha256_hex() {
    // D13: digest grammar "sha256:<hex>". Known SHA-256 of empty input.
    assert_eq!(
        digest_of(b""),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn identical_commit_is_idempotent() {
    let (_dir, mut s) = store();
    let files = [("a", b"1".as_slice()), ("b", b"2")];
    let id1 = s.commit(files, "op", "one", Stream::Local).unwrap();
    // Same content, different author/message: still no new revision.
    let id2 = s.commit(files, "other", "two", Stream::Local).unwrap();
    assert_eq!(id1, id2);

    // Changed content -> new revision parented on head.
    let id3 = s
        .commit([("a", b"1".as_slice()), ("b", b"changed")], "op", "three", Stream::Local)
        .unwrap();
    assert!(id3 > id1);
    assert_eq!(s.revision(id3).unwrap().parent, Some(id1));
}

#[test]
fn streams_are_independent_chains() {
    let (_dir, mut s) = store();
    let up1 = s.commit([("fleet.yaml", b"v1".as_slice())], "hub", "sync", Stream::Upstream).unwrap();
    let lo1 = s.commit([("site.yaml", b"s1".as_slice())], "gw", "site", Stream::Local).unwrap();
    let up2 = s.commit([("fleet.yaml", b"v2".as_slice())], "hub", "sync", Stream::Upstream).unwrap();

    // Parents chain within a stream, never across.
    assert_eq!(s.revision(up2).unwrap().parent, Some(up1));
    assert_eq!(s.revision(lo1).unwrap().parent, None);
    assert_eq!(s.head(Stream::Upstream).unwrap(), Some(up2));
    assert_eq!(s.head(Stream::Local).unwrap(), Some(lo1));

    // Idempotency is per stream: upstream content committed to local is new.
    let lo2 = s.commit([("fleet.yaml", b"v2".as_slice())], "gw", "copy", Stream::Local).unwrap();
    assert!(lo2 > up2);
}

#[test]
fn diff_reports_added_removed_modified() {
    let (_dir, mut s) = store();
    let a = s
        .commit(
            [("keep", b"same".as_slice()), ("mod", b"old"), ("gone", b"bye")],
            "op",
            "a",
            Stream::Local,
        )
        .unwrap();
    let b = s
        .commit(
            [("keep", b"same".as_slice()), ("mod", b"new"), ("fresh", b"hi")],
            "op",
            "b",
            Stream::Local,
        )
        .unwrap();

    let diff = s.diff(a, b).unwrap();
    assert_eq!(diff.len(), 3);
    assert_eq!(diff[0].path, "fresh");
    assert_eq!(diff[0].change, Change::Added { digest: digest_of(b"hi") });
    assert_eq!(diff[1].path, "gone");
    assert_eq!(diff[1].change, Change::Removed { digest: digest_of(b"bye") });
    assert_eq!(diff[2].path, "mod");
    assert_eq!(
        diff[2].change,
        Change::Modified { old: digest_of(b"old"), new: digest_of(b"new") }
    );

    assert_eq!(s.diff(a, a).unwrap(), vec![]);
}

#[test]
fn blame_lists_changing_revisions_only() {
    let (_dir, mut s) = store();
    let r1 = s.commit([("f", b"v1".as_slice()), ("other", b"x")], "alice", "add", Stream::Local).unwrap();
    let _r2 = s.commit([("f", b"v1".as_slice()), ("other", b"y")], "bob", "touch other", Stream::Local).unwrap();
    let r3 = s.commit([("f", b"v2".as_slice()), ("other", b"y")], "carol", "edit f", Stream::Local).unwrap();
    let r4 = s.commit([("other", b"y".as_slice())], "dave", "remove f", Stream::Local).unwrap();

    let blame = s.blame("f").unwrap();
    let ids: Vec<_> = blame.iter().map(|e| e.revision.id).collect();
    assert_eq!(ids, vec![r1, r3, r4]);
    assert_eq!(blame[0].revision.author, "alice");
    assert_eq!(blame[0].digest.as_deref(), Some(digest_of(b"v1").as_str()));
    assert_eq!(blame[1].revision.author, "carol");
    assert_eq!(blame[2].digest, None); // removal
}

#[test]
fn undo_is_a_new_revision_with_prior_content() {
    // D13: undo = new revision with prior content, not history rewrite.
    let (_dir, mut s) = store();
    let r1 = s.commit([("f", b"v1".as_slice())], "op", "one", Stream::Local).unwrap();
    let r2 = s.commit([("f", b"v2".as_slice())], "op", "two", Stream::Local).unwrap();
    let r3 = s.commit([("f", b"v1".as_slice())], "op", "undo", Stream::Local).unwrap();
    assert!(r3 > r2);
    assert_eq!(s.tree_at(r3).unwrap(), s.tree_at(r1).unwrap());
    // History intact.
    assert_eq!(s.read_at(r2, "f").unwrap().as_deref(), Some(b"v2".as_slice()));
}

#[test]
fn unknown_revision_errors() {
    let (_dir, s) = store();
    assert!(matches!(s.read_at(99, "x"), Err(Error::UnknownRevision(99))));
    assert!(matches!(s.tree_at(99), Err(Error::UnknownRevision(99))));
    assert!(matches!(s.revision(99), Err(Error::UnknownRevision(99))));
}

#[test]
fn reopen_preserves_everything() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.db");
    let id = {
        let mut s = RevisionStore::open(&db).unwrap();
        s.commit([("a", b"persist".as_slice())], "op", "m", Stream::Local).unwrap()
    };
    let s = RevisionStore::open(&db).unwrap();
    assert_eq!(s.head(Stream::Local).unwrap(), Some(id));
    assert_eq!(s.read_at(id, "a").unwrap().as_deref(), Some(b"persist".as_slice()));
    // Startup is idempotent: opening again created no new state.
    assert_eq!(s.head(Stream::Upstream).unwrap(), None);
}

#[test]
fn open_mode_matches_d6() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.db");
    let _s = RevisionStore::open(&db).unwrap();
    // Inspect via a second raw connection.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}
