//! Content-addressed revision store on SQLite — no VCS anywhere.
//!
//! Implements docs/decisions/delivery.md D13: content-addressed blobs
//! (`sha256:<hex>` -> bytes) plus append-only revisions (monotonic id,
//! parent, author, message, root manifest of path -> blob digest).
//! Diff/undo/blame are queries computed on read; atomicity is one SQLite
//! transaction.
//!
//! Per spec/reeve/06-federation.md §8.2 every store holds exactly TWO
//! revision streams: the *upstream* stream (verbatim read-only copy of the
//! parent tier's published revisions) and the *local* stream (revisions for
//! the layers this tier owns). Streams are independent append-only chains
//! sharing one blob table and one monotonic id space.
//!
//! Open mode per docs/decisions/storage.md D6: WAL, foreign_keys ON,
//! busy_timeout 5s. Schema law per D16: every table has an explicit
//! PRIMARY KEY (a session-extension requirement made a house rule).
//!
//! Crash-only (Law 3): all writes happen inside a single transaction, so a
//! `kill -9` mid-commit leaves the last committed revision intact and the
//! store immediately usable on reopen — startup IS recovery, there is no
//! repair step.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};

/// Monotonic revision identifier, unique across both streams of a store.
pub type RevisionId = i64;

/// The two revision streams held by every store
/// (spec/reeve/06-federation.md §8.2, normative per-tier revision model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// Verbatim read-only copy of the parent tier's published revisions.
    Upstream,
    /// This tier's own revisions for the layers it owns.
    Local,
}

impl Stream {
    fn as_str(self) -> &'static str {
        match self {
            Stream::Upstream => "upstream",
            Stream::Local => "local",
        }
    }

    fn from_db(s: &str) -> Result<Self, Error> {
        match s {
            "upstream" => Ok(Stream::Upstream),
            "local" => Ok(Stream::Local),
            other => Err(Error::Corrupt(format!("unknown stream {other:?}"))),
        }
    }
}

/// Metadata for one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub id: RevisionId,
    pub stream: Stream,
    pub parent: Option<RevisionId>,
    pub author: String,
    pub message: String,
    /// UTC timestamp, ISO-8601 with milliseconds (`strftime` at insert).
    pub created_at: String,
}

/// One path's change between two revisions (diff is computed on read — D13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: String,
    pub change: Change,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added { digest: String },
    Removed { digest: String },
    Modified { old: String, new: String },
}

/// One revision at which a path changed (blame = SELECT — D13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameEntry {
    pub revision: Revision,
    /// Digest of the path at this revision; `None` means the revision
    /// removed the path.
    pub digest: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("unknown revision {0}")]
    UnknownRevision(RevisionId),
    #[error("store corrupt: {0}")]
    Corrupt(String),
    /// A verbatim append disagreed with what the stream already holds
    /// (spec/reeve/06-federation.md §8.2: an id/digest mismatch against
    /// an already-held revision MUST be surfaced as an error and MUST
    /// NOT be auto-resolved — single-writer was violated somewhere).
    #[error("verbatim append conflict at origin revision {origin_id}: {detail}")]
    VerbatimConflict { origin_id: RevisionId, detail: String },
    /// A verbatim append referenced a blob not present in the store —
    /// the revision's closure is incomplete, so it MUST NOT become
    /// visible (§8.2: visible only when the full closure is present).
    #[error("verbatim append missing blob {0} — closure incomplete")]
    MissingBlob(String),
}

/// One revision as published by a PARENT tier, appended verbatim to the
/// upstream stream (spec/reeve/06-federation.md §8.2: the synced stream
/// is a verbatim read-only copy — parent revision ids, parents,
/// authorship and timestamps preserved). The parent's ids live in the
/// `stream_origins` side table because local row ids are allocated from
/// this store's own monotonic space (shared with the local stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbatimRevision {
    /// The revision's id AT THE PARENT tier.
    pub origin_id: RevisionId,
    /// The parent-tier id of this revision's parent (chain pointer).
    pub origin_parent: Option<RevisionId>,
    pub author: String,
    pub message: String,
    /// The parent's timestamp, preserved verbatim.
    pub created_at: String,
    /// Full manifest: path -> blob digest. Every digest MUST already be
    /// present in the blob table (see [`RevisionStore::put_blob`]).
    pub files: BTreeMap<String, String>,
}

/// Outcome of [`RevisionStore::append_verbatim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerbatimOutcome {
    /// Appended as a new local row.
    Appended(RevisionId),
    /// The identical revision (same origin id AND same content) was
    /// already held — idempotent re-sync / re-import (Law 3).
    AlreadyPresent(RevisionId),
}

impl VerbatimOutcome {
    pub fn local_id(self) -> RevisionId {
        match self {
            VerbatimOutcome::Appended(id) | VerbatimOutcome::AlreadyPresent(id) => id,
        }
    }
}

/// Compute the store's digest string for a byte slice:
/// `sha256:<lowercase hex>` (D13 digest grammar, RFC 9110 strong validator).
pub fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut s = String::with_capacity(7 + 64);
    s.push_str("sha256:");
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Content-addressed blob store + append-only revision log in one SQLite
/// file. Single writer (D14); wrap in your own pool for reads if needed.
///
/// Connection ownership comes in two flavors (both additive, D16 writer
/// unification for spec/reeve/07-durability.md §9.3 session capture —
/// changeset capture requires ALL writes on ONE connection):
/// - [`RevisionStore::open`] / [`RevisionStore::from_connection`]: the
///   store owns its connection (standalone use, this crate's tests).
/// - [`RevisionStore::from_shared`]: the store locks a caller-owned
///   `Arc<Mutex<Connection>>` per call — THE single writer connection
///   shared with the embedding server's own tables.
pub struct RevisionStore {
    conn: ConnHandle,
}

enum ConnHandle {
    Owned(Connection),
    Shared(Arc<Mutex<Connection>>),
}

// Schema: explicit PRIMARY KEY on every table (D16). Plain rowid tables
// (not WITHOUT ROWID) so the session extension can track them at the
// server level. CREATE TABLE IF NOT EXISTS is acceptable for this crate's
// own tests; refinery integration happens at server level.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS blobs (
    digest  TEXT PRIMARY KEY,
    content BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS revisions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    stream     TEXT    NOT NULL CHECK (stream IN ('upstream', 'local')),
    parent_id  INTEGER REFERENCES revisions(id),
    author     TEXT    NOT NULL,
    message    TEXT    NOT NULL,
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS revisions_stream_idx ON revisions (stream, id);
CREATE TABLE IF NOT EXISTS revision_files (
    revision_id INTEGER NOT NULL REFERENCES revisions(id),
    path        TEXT    NOT NULL,
    digest      TEXT    NOT NULL REFERENCES blobs(digest),
    PRIMARY KEY (revision_id, path)
);
CREATE TABLE IF NOT EXISTS stream_origins (
    revision_id   INTEGER PRIMARY KEY REFERENCES revisions(id),
    origin_id     INTEGER NOT NULL UNIQUE,
    origin_parent INTEGER
);
";

impl RevisionStore {
    /// Open (creating if absent) a revision store at `path`.
    ///
    /// Open mode per D6: WAL journal, foreign_keys ON, busy_timeout 5s.
    /// Idempotent — safe to call on every startup (Law 3).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // journal_mode returns the resulting mode as a row.
        let _mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
        conn.pragma_update(None, "foreign_keys", "on")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: ConnHandle::Owned(conn) })
    }

    /// Build a store over an already-open connection the store then OWNS.
    /// The caller is responsible for open mode (pragmas); the store only
    /// ensures its own schema (idempotent). Additive constructor for D16
    /// writer unification.
    pub fn from_connection(conn: Connection) -> Result<Self, Error> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: ConnHandle::Owned(conn) })
    }

    /// Build a store over a SHARED writer connection — THE single writer
    /// of docs/decisions/storage.md D6/D16, so the SQLite session
    /// extension attached to it captures revision writes too
    /// (spec/reeve/07-durability.md §9.3). The store locks per call and
    /// never holds the lock across calls; the caller is responsible for
    /// open mode (pragmas). Ensures the store schema (idempotent).
    pub fn from_shared(conn: Arc<Mutex<Connection>>) -> Result<Self, Error> {
        conn.lock()
            .expect("shared connection mutex poisoned")
            .execute_batch(SCHEMA)?;
        Ok(Self { conn: ConnHandle::Shared(conn) })
    }

    /// Run a read against the connection (locking if shared).
    fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T, Error>) -> Result<T, Error> {
        match &self.conn {
            ConnHandle::Owned(c) => f(c),
            ConnHandle::Shared(m) => f(&m.lock().expect("shared connection mutex poisoned")),
        }
    }

    /// Run a write against the connection (locking if shared).
    fn write<T>(&mut self, f: impl FnOnce(&mut Connection) -> Result<T, Error>) -> Result<T, Error> {
        match &mut self.conn {
            ConnHandle::Owned(c) => f(c),
            ConnHandle::Shared(m) => f(&mut m.lock().expect("shared connection mutex poisoned")),
        }
    }

    /// Commit a full tree manifest (path -> content) as a new revision on
    /// `stream`, parented on the stream's current head.
    ///
    /// Idempotent (D13/D14): if the manifest is byte-identical to the
    /// stream head's manifest, no new revision is created and the existing
    /// head id is returned. All writes happen in ONE transaction — a crash
    /// mid-commit leaves no partial revision.
    pub fn commit<P, B>(
        &mut self,
        files: impl IntoIterator<Item = (P, B)>,
        author: &str,
        message: &str,
        stream: Stream,
    ) -> Result<RevisionId, Error>
    where
        P: Into<String>,
        B: AsRef<[u8]>,
    {
        // Normalize: last write wins per path, deterministic order.
        let mut manifest: BTreeMap<String, (String, Vec<u8>)> = BTreeMap::new();
        for (path, content) in files {
            let content = content.as_ref().to_vec();
            let digest = digest_of(&content);
            manifest.insert(path.into(), (digest, content));
        }

        self.write(|conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Idempotency: identical content to the stream head => existing id.
        let head: Option<RevisionId> = tx
            .query_row(
                "SELECT MAX(id) FROM revisions WHERE stream = ?1",
                params![stream.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if let Some(head_id) = head {
            let head_tree = tree_at_tx(&tx, head_id)?;
            let same = head_tree.len() == manifest.len()
                && head_tree
                    .iter()
                    .all(|(p, d)| manifest.get(p).map(|(nd, _)| nd) == Some(d));
            if same {
                // Nothing to write; the open tx is dropped (rolled back).
                return Ok(head_id);
            }
        }

        {
            let mut insert_blob = tx.prepare_cached(
                "INSERT INTO blobs (digest, content) VALUES (?1, ?2)
                 ON CONFLICT (digest) DO NOTHING",
            )?;
            for (digest, content) in manifest.values() {
                insert_blob.execute(params![digest, content])?;
            }
        }

        tx.execute(
            "INSERT INTO revisions (stream, parent_id, author, message)
             VALUES (?1, ?2, ?3, ?4)",
            params![stream.as_str(), head, author, message],
        )?;
        let id = tx.last_insert_rowid();

        {
            let mut insert_file = tx.prepare_cached(
                "INSERT INTO revision_files (revision_id, path, digest)
                 VALUES (?1, ?2, ?3)",
            )?;
            for (path, (digest, _)) in &manifest {
                insert_file.execute(params![id, path, digest])?;
            }
        }

        tx.commit()?;
        Ok(id)
        })
    }

    /// Current head (highest id) of a stream, if any revision exists.
    pub fn head(&self, stream: Stream) -> Result<Option<RevisionId>, Error> {
        self.read(|conn| {
            let head: Option<RevisionId> = conn
                .query_row(
                    "SELECT MAX(id) FROM revisions WHERE stream = ?1",
                    params![stream.as_str()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            Ok(head)
        })
    }

    /// Metadata for one revision.
    pub fn revision(&self, id: RevisionId) -> Result<Revision, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT id, stream, parent_id, author, message, created_at
                 FROM revisions WHERE id = ?1",
                params![id],
                revision_from_row,
            )
            .optional()?
            .ok_or(Error::UnknownRevision(id))
        })
    }

    /// Content of `path` at `revision`. `Ok(None)` if the revision exists
    /// but does not contain the path; error if the revision is unknown.
    pub fn read_at(&self, revision: RevisionId, path: &str) -> Result<Option<Vec<u8>>, Error> {
        self.read(|conn| {
            assert_revision_on(conn, revision)?;
            let content: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT b.content
                     FROM revision_files rf JOIN blobs b ON b.digest = rf.digest
                     WHERE rf.revision_id = ?1 AND rf.path = ?2",
                    params![revision, path],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(content)
        })
    }

    /// Full manifest (path -> digest) of a revision.
    pub fn tree_at(&self, revision: RevisionId) -> Result<BTreeMap<String, String>, Error> {
        self.read(|conn| {
            assert_revision_on(conn, revision)?;
            tree_at_tx(conn, revision)
        })
    }

    /// Diff between two revisions' manifests, computed on read (D13).
    /// Entries are relative to `rev_a` (old) -> `rev_b` (new), sorted by path.
    pub fn diff(&self, rev_a: RevisionId, rev_b: RevisionId) -> Result<Vec<DiffEntry>, Error> {
        let a = self.tree_at(rev_a)?;
        let b = self.tree_at(rev_b)?;
        let mut out = Vec::new();
        for (path, old) in &a {
            match b.get(path) {
                None => out.push(DiffEntry {
                    path: path.clone(),
                    change: Change::Removed { digest: old.clone() },
                }),
                Some(new) if new != old => out.push(DiffEntry {
                    path: path.clone(),
                    change: Change::Modified { old: old.clone(), new: new.clone() },
                }),
                Some(_) => {}
            }
        }
        for (path, new) in &b {
            if !a.contains_key(path) {
                out.push(DiffEntry {
                    path: path.clone(),
                    change: Change::Added { digest: new.clone() },
                });
            }
        }
        out.sort_by(|x, y| x.path.cmp(&y.path));
        Ok(out)
    }

    /// Every revision (either stream, ascending id) at which `path` changed
    /// relative to that revision's parent in its own chain — blame as a
    /// query (D13). `digest: None` marks a removal.
    pub fn blame(&self, path: &str) -> Result<Vec<BlameEntry>, Error> {
        let rows: Vec<(Revision, Option<String>)> = self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT r.id, r.stream, r.parent_id, r.author, r.message, r.created_at,
                        rf.digest
                 FROM revisions r
                 LEFT JOIN revision_files rf
                        ON rf.revision_id = r.id AND rf.path = ?1
                 ORDER BY r.id ASC",
            )?;
            let rows = stmt
                .query_map(params![path], |row| {
                    let digest: Option<String> = row.get(6)?;
                    let rev = revision_from_row(row)?;
                    Ok((rev, digest))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        // id -> digest-at-that-revision, for parent lookups.
        let by_id: BTreeMap<RevisionId, Option<String>> = rows
            .iter()
            .map(|(rev, digest)| (rev.id, digest.clone()))
            .collect();

        let mut out = Vec::new();
        for (rev, digest) in rows {
            let parent_digest: Option<String> = rev
                .parent
                .and_then(|pid| by_id.get(&pid).cloned())
                .flatten();
            if digest != parent_digest {
                out.push(BlameEntry { revision: rev, digest });
            }
        }
        Ok(out)
    }

    /// Fetch a blob by digest, if present.
    pub fn blob(&self, digest: &str) -> Result<Option<Vec<u8>>, Error> {
        self.read(|conn| {
            let content: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT content FROM blobs WHERE digest = ?1",
                    params![digest],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(content)
        })
    }

    /// Whether a blob is already held. Sync clients use this to skip
    /// re-fetching content-addressed data (federation §8.2: resumable,
    /// idempotent by digest).
    pub fn has_blob(&self, digest: &str) -> Result<bool, Error> {
        self.read(|conn| {
            let n: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM blobs WHERE digest = ?1",
                    params![digest],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(n.is_some())
        })
    }

    /// Insert one content-addressed blob, verifying the claimed digest
    /// against the bytes (a fetch that delivered wrong bytes must fail
    /// HERE, not corrupt the store). Idempotent — its own transaction,
    /// so an interrupted multi-blob sync resumes by digest (federation
    /// §8.2: a sync killed mid-transfer resumes by fetching what is
    /// still missing; blobs without a referencing revision are inert).
    pub fn put_blob(&mut self, digest: &str, content: &[u8]) -> Result<(), Error> {
        let actual = digest_of(content);
        if actual != digest {
            return Err(Error::Corrupt(format!(
                "blob digest mismatch: claimed {digest}, content is {actual}"
            )));
        }
        self.write(|conn| {
            conn.execute(
                "INSERT INTO blobs (digest, content) VALUES (?1, ?2)
                 ON CONFLICT (digest) DO NOTHING",
                params![digest, content],
            )?;
            Ok(())
        })
    }

    /// Append one PARENT-published revision verbatim to `stream`
    /// (spec/reeve/06-federation.md §8.2, per-tier revision model).
    ///
    /// Rules (all enforced in ONE transaction — Law 3):
    /// - Idempotent: an identical revision (same origin id, parent,
    ///   author, message, timestamp AND file manifest) already held
    ///   returns [`VerbatimOutcome::AlreadyPresent`].
    /// - Divergence is an ERROR: the same origin id with ANY differing
    ///   content is [`Error::VerbatimConflict`] — never auto-resolved.
    /// - Append-only chain: `origin_parent` must equal the stream
    ///   head's origin id (or `None` on an empty stream).
    /// - Full closure: every referenced blob must already be present
    ///   ([`Error::MissingBlob`] otherwise) — a revision becomes
    ///   visible only when its closure is complete.
    pub fn append_verbatim(
        &mut self,
        stream: Stream,
        rev: &VerbatimRevision,
    ) -> Result<VerbatimOutcome, Error> {
        self.write(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

            // Already held?
            let existing: Option<(RevisionId, Option<RevisionId>)> = tx
                .query_row(
                    "SELECT so.revision_id, so.origin_parent
                     FROM stream_origins so JOIN revisions r ON r.id = so.revision_id
                     WHERE so.origin_id = ?1 AND r.stream = ?2",
                    params![rev.origin_id, stream.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((local_id, held_parent)) = existing {
                let held = tx.query_row(
                    "SELECT author, message, created_at FROM revisions WHERE id = ?1",
                    params![local_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?;
                let held_files = tree_at_tx(&tx, local_id)?;
                let same = held_parent == rev.origin_parent
                    && held.0 == rev.author
                    && held.1 == rev.message
                    && held.2 == rev.created_at
                    && held_files == rev.files;
                return if same {
                    Ok(VerbatimOutcome::AlreadyPresent(local_id))
                } else {
                    Err(Error::VerbatimConflict {
                        origin_id: rev.origin_id,
                        detail: "already-held revision differs (single-writer violation \
                                 or storage corruption)"
                            .to_string(),
                    })
                };
            }

            // Chain rule: must extend the current head.
            let head_origin: Option<RevisionId> = tx
                .query_row(
                    "SELECT so.origin_id
                     FROM revisions r JOIN stream_origins so ON so.revision_id = r.id
                     WHERE r.stream = ?1
                     ORDER BY r.id DESC LIMIT 1",
                    params![stream.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if rev.origin_parent != head_origin {
                return Err(Error::VerbatimConflict {
                    origin_id: rev.origin_id,
                    detail: format!(
                        "origin parent {:?} does not extend the stream head (origin {:?})",
                        rev.origin_parent, head_origin
                    ),
                });
            }

            // Full closure present?
            for digest in rev.files.values() {
                let held: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM blobs WHERE digest = ?1",
                        params![digest],
                        |row| row.get(0),
                    )
                    .optional()?;
                if held.is_none() {
                    return Err(Error::MissingBlob(digest.clone()));
                }
            }

            // Local parent pointer = the stream's current head row.
            let local_parent: Option<RevisionId> = tx
                .query_row(
                    "SELECT MAX(id) FROM revisions WHERE stream = ?1",
                    params![stream.as_str()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();

            tx.execute(
                "INSERT INTO revisions (stream, parent_id, author, message, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    stream.as_str(),
                    local_parent,
                    rev.author,
                    rev.message,
                    rev.created_at
                ],
            )?;
            let id = tx.last_insert_rowid();
            {
                let mut insert_file = tx.prepare_cached(
                    "INSERT INTO revision_files (revision_id, path, digest)
                     VALUES (?1, ?2, ?3)",
                )?;
                for (path, digest) in &rev.files {
                    insert_file.execute(params![id, path, digest])?;
                }
            }
            tx.execute(
                "INSERT INTO stream_origins (revision_id, origin_id, origin_parent)
                 VALUES (?1, ?2, ?3)",
                params![id, rev.origin_id, rev.origin_parent],
            )?;

            tx.commit()?;
            Ok(VerbatimOutcome::Appended(id))
        })
    }

    /// Head of a verbatim-synced stream as `(local row id, origin id)`,
    /// if any revision exists. `None` also when the stream head has no
    /// origin record (a stream never fed by [`append_verbatim`]).
    pub fn origin_head(
        &self,
        stream: Stream,
    ) -> Result<Option<(RevisionId, RevisionId)>, Error> {
        self.read(|conn| {
            let row: Option<(RevisionId, RevisionId)> = conn
                .query_row(
                    "SELECT r.id, so.origin_id
                     FROM revisions r JOIN stream_origins so ON so.revision_id = r.id
                     WHERE r.stream = ?1
                     ORDER BY r.id DESC LIMIT 1",
                    params![stream.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            Ok(row)
        })
    }

    /// The parent-tier origin id recorded for a local revision row, if
    /// it was appended verbatim.
    pub fn origin_of(&self, revision: RevisionId) -> Result<Option<RevisionId>, Error> {
        self.read(|conn| {
            let origin: Option<RevisionId> = conn
                .query_row(
                    "SELECT origin_id FROM stream_origins WHERE revision_id = ?1",
                    params![revision],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(origin)
        })
    }
}

fn assert_revision_on(conn: &Connection, id: RevisionId) -> Result<(), Error> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM revisions WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_some() {
        Ok(())
    } else {
        Err(Error::UnknownRevision(id))
    }
}

fn revision_from_row(row: &rusqlite::Row<'_>) -> Result<Revision, rusqlite::Error> {
    let stream_str: String = row.get(1)?;
    let stream = Stream::from_db(&stream_str).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("unknown stream {stream_str:?}").into(),
        )
    })?;
    Ok(Revision {
        id: row.get(0)?,
        stream,
        parent: row.get(2)?,
        author: row.get(3)?,
        message: row.get(4)?,
        created_at: row.get(5)?,
    })
}

/// Manifest query usable both inside a commit transaction and for reads.
fn tree_at_tx(
    conn: &Connection,
    revision: RevisionId,
) -> Result<BTreeMap<String, String>, Error> {
    let mut stmt = conn.prepare_cached(
        "SELECT path, digest FROM revision_files WHERE revision_id = ?1",
    )?;
    let mut out = BTreeMap::new();
    let rows = stmt.query_map(params![revision], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (path, digest) = row?;
        out.insert(path, digest);
    }
    Ok(out)
}
