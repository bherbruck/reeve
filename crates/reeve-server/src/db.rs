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
    EmbeddedMigration {
        version: 5,
        name: "durability",
        sql: include_str!("migrations/V5__durability.sql"),
    },
    // The secrets table exists regardless of the ext-secrets feature
    // (like V5 for the changeset tier): schema is stable across feature
    // sets so a --no-default-features binary can still restore/verify a
    // target written by a full one (spec/reeve/07-durability.md §9.4).
    EmbeddedMigration {
        version: 6,
        name: "secrets",
        sql: include_str!("migrations/V6__secrets.sql"),
    },
    // Terminal session audit rows exist regardless of ext-terminal
    // (same rule as V6): schema stable across feature sets, and
    // startup can finalize dangling rows left by a full binary
    // (spec/reeve/03-terminal.md §5.4).
    EmbeddedMigration {
        version: 7,
        name: "terminal",
        sql: include_str!("migrations/V7__terminal.sql"),
    },
    // Rollout state + per-device render targets exist regardless of
    // ext-rollouts (same rule as V6/V7): the CORE render pipeline
    // honors device_render_targets rows, so a rollout paused by a full
    // binary stays a stable position (spec/reeve/09-rollouts.md §11.2)
    // under a core binary too.
    EmbeddedMigration {
        version: 8,
        name: "rollouts",
        sql: include_str!("migrations/V8__rollouts.sql"),
    },
    // Federation tier state exists regardless of ext-federation (same
    // rule as V6/V7/V8): the CORE tree write gate consults tier_tokens
    // (spec/reeve/06-federation.md §8.4 delegated-layer refusal), and
    // the core render pipeline reads devices.tier_origin and
    // device_manifests.rendered_upstream.
    EmbeddedMigration {
        version: 9,
        name: "federation",
        sql: include_str!("migrations/V9__federation.sql"),
    },
    // Operator fleet model (REV-010, spec/reeve/11-fleet-model.md): the
    // hierarchy-tier assignment columns (fleet/type + display_name/
    // pinned/decommissioned_at), and enrollment pre-assignment columns
    // on join_tokens. Additive; class/region (V2) are retained but
    // dormant after the taxonomy remap.
    EmbeddedMigration {
        version: 10,
        name: "fleet_model",
        sql: include_str!("migrations/V10__fleet_model.sql"),
    },
    // Canonical location groups + fleet->site containment (REV-010
    // amendment, spec/reeve/11-fleet-model.md §11.1/§11.3): a fleet->site
    // containment tree (a site belongs to exactly one fleet), so device
    // assignments can no longer mix a site with a fleet it doesn't belong
    // to. Device-type stays an orthogonal free column. Backfills groups
    // from existing devices.fleet/(fleet,site) so current assignments stay
    // valid.
    EmbeddedMigration {
        version: 11,
        name: "location_groups",
        sql: include_str!("migrations/V11__location_groups.sql"),
    },
    // Deploy-log storage (REV-011, server ext-logs): content-addressed
    // compose up/down output per deployment. Schema exists regardless of
    // the ext-logs feature (same rule as V6..V9): stable across feature
    // sets so a --no-default-features binary can restore a target written
    // by a full one. The feature gates only the LogStore module + routes.
    EmbeddedMigration {
        version: 12,
        name: "deploy_logs",
        sql: include_str!("migrations/V12__deploy_logs.sql"),
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

/// Highest migration version embedded in this binary — the schema
/// version stamped into snapshot generation ids
/// (spec/reeve/07-durability.md §9.2).
pub fn embedded_schema_version() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// Assert a (restored) database's schema is KNOWN TO THIS BINARY
/// (spec/reeve/07-durability.md §9.4): every applied migration must
/// exist in the embedded set with a matching checksum, and the max
/// version must not exceed what this binary ships. Returns the
/// database's schema version.
pub fn assert_schema_known(conn: &Connection) -> anyhow::Result<i64> {
    let mut stmt = conn
        .prepare("SELECT version, checksum FROM refinery_schema_history ORDER BY version")
        .context("restored DB has no refinery_schema_history — not a reeve database")?;
    let applied: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    if applied.is_empty() {
        bail!("restored DB has an empty migration history");
    }
    let mut max = 0;
    for (version, cs) in &applied {
        let Some(m) = MIGRATIONS.iter().find(|m| m.version == *version) else {
            bail!("restored DB schema version {version} is unknown to this binary");
        };
        if *cs != checksum(m.sql) {
            bail!(
                "restored DB migration V{version}__{} checksum mismatch — \
                 schema not produced by this binary's lineage",
                m.name
            );
        }
        max = max.max(*version);
    }
    Ok(max)
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
