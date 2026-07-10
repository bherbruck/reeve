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
pub struct RevisionStore {
    conn: Connection,
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
        Ok(Self { conn })
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

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

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
    }

    /// Current head (highest id) of a stream, if any revision exists.
    pub fn head(&self, stream: Stream) -> Result<Option<RevisionId>, Error> {
        let head: Option<RevisionId> = self
            .conn
            .query_row(
                "SELECT MAX(id) FROM revisions WHERE stream = ?1",
                params![stream.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(head)
    }

    /// Metadata for one revision.
    pub fn revision(&self, id: RevisionId) -> Result<Revision, Error> {
        self.conn
            .query_row(
                "SELECT id, stream, parent_id, author, message, created_at
                 FROM revisions WHERE id = ?1",
                params![id],
                revision_from_row,
            )
            .optional()?
            .ok_or(Error::UnknownRevision(id))
    }

    /// Content of `path` at `revision`. `Ok(None)` if the revision exists
    /// but does not contain the path; error if the revision is unknown.
    pub fn read_at(&self, revision: RevisionId, path: &str) -> Result<Option<Vec<u8>>, Error> {
        self.assert_revision(revision)?;
        let content: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT b.content
                 FROM revision_files rf JOIN blobs b ON b.digest = rf.digest
                 WHERE rf.revision_id = ?1 AND rf.path = ?2",
                params![revision, path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(content)
    }

    /// Full manifest (path -> digest) of a revision.
    pub fn tree_at(&self, revision: RevisionId) -> Result<BTreeMap<String, String>, Error> {
        self.assert_revision(revision)?;
        tree_at_tx(&self.conn, revision)
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
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.stream, r.parent_id, r.author, r.message, r.created_at,
                    rf.digest
             FROM revisions r
             LEFT JOIN revision_files rf
                    ON rf.revision_id = r.id AND rf.path = ?1
             ORDER BY r.id ASC",
        )?;
        let rows: Vec<(Revision, Option<String>)> = stmt
            .query_map(params![path], |row| {
                let digest: Option<String> = row.get(6)?;
                let rev = revision_from_row(row)?;
                Ok((rev, digest))
            })?
            .collect::<Result<Vec<_>, _>>()?;

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
        let content: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT content FROM blobs WHERE digest = ?1",
                params![digest],
                |row| row.get(0),
            )
            .optional()?;
        Ok(content)
    }

    fn assert_revision(&self, id: RevisionId) -> Result<(), Error> {
        let exists: Option<i64> = self
            .conn
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
