//! The single server SQLite DB: open mode + embedded migrations.
//!
//! Open mode per docs/decisions/storage.md D6: WAL, foreign_keys ON,
//! busy_timeout 5s. Migrations run at every startup, idempotent and
//! resumable (Law 3: startup IS recovery).
//!
//! DECISION (recorded in build output): docs/decisions/storage.md D6
//! names refinery, but refinery-core 0.9.2 supports rusqlite <=0.39 and
//! the workspace pins rusqlite 0.40 (needed for the `session` feature,
//! D16) — the `links = "sqlite3"` native-lib conflict makes linking both
//! impossible. This module is a minimal embedded runner that keeps
//! refinery's `refinery_schema_history` table shape so a swap back to
//! real refinery (once it supports rusqlite 0.40) is a drop-in. Checksum
//! here is hex SHA-256 of the migration SQL (refinery uses a different
//! algorithm; a future swap re-baselines checksums).
//!
//! Schema law (D16): every table has an explicit PRIMARY KEY, and any
//! migration run must cut a new snapshot generation — [`migrate`] returns
//! `true` so the durability tier (C6) can do exactly that.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};
use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

struct EmbeddedMigration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

/// All server-table migrations, in order, append-only. The revision-store
/// tables are NOT here: that crate self-initializes its own DDL
/// idempotently on the same file (Law 2 — it stands alone).
const MIGRATIONS: &[EmbeddedMigration] = &[
    EmbeddedMigration {
        version: 1,
        name: "auth",
        sql: include_str!("migrations/V1__auth.sql"),
    },
    EmbeddedMigration {
        version: 2,
        name: "enrollment",
        sql: include_str!("migrations/V2__enrollment.sql"),
    },
    EmbeddedMigration {
        version: 3,
        name: "render",
        sql: include_str!("migrations/V3__render.sql"),
    },
    EmbeddedMigration {
        version: 4,
        name: "status",
        sql: include_str!("migrations/V4__status.sql"),
    },
];

/// Open the server DB with the D6 pragmas. Idempotent.
pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Connection> {
    let conn = Connection::open(path.as_ref())
        .with_context(|| format!("opening {}", path.as_ref().display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let _mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", "on")?;
    Ok(conn)
}

fn checksum(sql: &str) -> String {
    hex::encode(Sha256::digest(sql.as_bytes()))
}

/// Run all unapplied migrations. Each migration applies in ONE
/// transaction (DDL + history row), so kill -9 mid-migration leaves
/// either the previous schema or the next — never a torn one. Returns
/// `true` if anything was applied (D16: caller must cut a snapshot
/// generation).
pub fn migrate(conn: &mut Connection) -> anyhow::Result<bool> {
    // refinery-compatible history table (see module docs).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS refinery_schema_history (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_on TEXT NOT NULL,
            checksum   TEXT NOT NULL
        );",
    )?;

    let mut applied_any = false;
    for m in MIGRATIONS {
        let existing: Option<String> = conn
            .query_row(
                "SELECT checksum FROM refinery_schema_history WHERE version = ?1",
                params![m.version],
                |row| row.get(0),
            )
            .optional()?;

        match existing {
            Some(cs) => {
                // Applied before: verify the embedded SQL hasn't drifted.
                if cs != checksum(m.sql) {
                    bail!(
                        "migration V{}__{} checksum mismatch — embedded SQL \
                         changed after being applied; migrations are append-only",
                        m.version,
                        m.name
                    );
                }
            }
            None => {
                let tx = conn.transaction()?;
                tx.execute_batch(m.sql)
                    .with_context(|| format!("applying V{}__{}", m.version, m.name))?;
                tx.execute(
                    "INSERT INTO refinery_schema_history (version, name, applied_on, checksum)
                     VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3)",
                    params![m.version, m.name, checksum(m.sql)],
                )?;
                tx.commit()?;
                applied_any = true;
            }
        }
    }
    Ok(applied_any)
}

/// Seconds since the unix epoch — the timestamp unit for all server
/// tables (integers, no datetime library).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent_and_reports_first_apply() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open(dir.path().join("t.db")).unwrap();
        assert!(migrate(&mut conn).unwrap(), "first run applies");
        assert!(!migrate(&mut conn).unwrap(), "second run is a no-op");
        // tables exist
        let n: i64 = conn
            .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn checksum_drift_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open(dir.path().join("t.db")).unwrap();
        migrate(&mut conn).unwrap();
        conn.execute(
            "UPDATE refinery_schema_history SET checksum = 'tampered' WHERE version = 1",
            [],
        )
        .unwrap();
        assert!(migrate(&mut conn).unwrap_err().to_string().contains("checksum"));
    }

    #[test]
    fn shares_file_with_revision_store() {
        // Law 4: ONE SQLite file — server tables + revision store tables.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reeve.db");
        let mut conn = open(&path).unwrap();
        migrate(&mut conn).unwrap();
        let mut store = revision_store::RevisionStore::open(&path).unwrap();
        store
            .commit(
                [("a.txt", b"hello".as_slice())],
                "test",
                "m",
                revision_store::Stream::Local,
            )
            .unwrap();
        // both table families visible on one connection
        let n: i64 = conn
            .query_row("SELECT count(*) FROM revisions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
