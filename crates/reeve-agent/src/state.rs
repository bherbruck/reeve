//! agent.db — the agent's durable local state (rusqlite, WAL).
//!
//! Crash-only (CLAUDE.md Law 3): startup IS recovery — `open` is
//! idempotent, schema creation uses IF NOT EXISTS, every write is
//! one transaction, and `kill -9` at any point leaves a database the
//! next startup resumes from. Offline-first (Law 5): the
//! last-accepted manifest and the applied-state table are what the
//! agent continues from when the network is gone.
//!
//! Tables (docs/decisions/agent.md D5 journal-phase contract;
//! spec/reeve/08-packaging.md §10.2 anti-rollback persistence):
//! - `manifest_state` — single row: last ACCEPTED State Manifest
//!   (version, ETag, body). The monotonicity floor survives restarts.
//! - `journal` — append-only agent event journal (info | notable |
//!   security | error). SECURITY/NOTABLE events required by §10.2
//!   land here (and in stdout logs); REV-004 backfill (B7) will
//!   drain from it later.
//! - `applied_state` — per-app applied phase + content hash
//!   (D5: planned -> applying -> applied | failed; removing ->
//!   removed). B1 creates and reads it ("continue from applied");
//!   the compose provider (B3) drives the phases.
//! - `bundle_state` — single row: digest of the render bundle
//!   currently swapped into place (docs/decisions/tree-render.md D2:
//!   "applied bundle digest recorded in agent.db, not a loose
//!   file"). Written ONLY after the atomic dir swap (B2); startup
//!   recovery rolls it forward from disk if a `kill -9` landed
//!   between swap and record.
//! - `wire_journal` — THE per-device journal of
//!   spec/reeve/05-health-journal.md §7.1 (B7): one row per wire
//!   [`JournalRecord`] (status | health | lifecycle | gap), with ONE
//!   monotonic seq space shared with the live status path — the
//!   `seq` a live report carries in its `reeve` object and the `seq`
//!   the same record carries in a backfill batch are THE SAME
//!   number, so the server's `(deviceId, seq)` dedup works across
//!   both paths (§7.3). `AUTOINCREMENT` is the persistent monotonic
//!   counter: seqs survive eviction and are never reused. Lifecycle
//!   rows are mirrored in by trigger from `journal`; status rows are
//!   written alongside `status_reports` in one transaction; health
//!   rows come from the ext-health sampler.
//! - `journal_ack` — single row: the server-acknowledged watermark
//!   (§7.3 `JournalAck.ackedSeq`); what permits eviction (§7.1).

use std::path::Path;

use reeve_types::reeve::manifest::{ManifestVersion, StateManifest};
use rusqlite::{Connection, OptionalExtension, params};

/// Errors from the agent state database.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt stored manifest json: {0}")]
    CorruptManifest(#[from] serde_json::Error),
}

/// Journal entry severity. `security` and `notable` are the exact
/// event classes spec/reeve/08-packaging.md §10.2 requires the agent
/// to log (regression => SECURITY, epoch bump => NOTABLE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Notable,
    Security,
    Error,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Notable => "notable",
            Severity::Security => "security",
            Severity::Error => "error",
        }
    }
}

/// The last-accepted manifest, as persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedManifest {
    pub version: ManifestVersion,
    /// The manifest digest `sha256:<hex>` — sent back as
    /// `If-None-Match` (spec/reeve/08-packaging.md §10.2).
    pub etag: String,
    pub manifest: StateManifest,
}

/// One journal row (read-back shape; used by tests and, later, B7
/// backfill).
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub seq: i64,
    pub ts: String,
    pub severity: String,
    pub event: String,
    pub detail: String,
}

/// One applied-state row (docs/decisions/agent.md D5).
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedApp {
    pub app_id: String,
    pub content_hash: String,
    pub secrets_version: Option<String>,
    pub phase: String,
}

/// One unsent status-report row (store-and-forward seed for B7
/// backfill; spec/reeve/05-health-journal.md §7.3).
#[derive(Debug, Clone, PartialEq)]
pub struct StatusRow {
    pub seq: i64,
    /// Original timestamp — assigned when journaled, never rewritten
    /// (§7 "original timestamp"); becomes `reeve.observedAt`.
    pub ts: String,
    pub app_id: String,
    pub deployment_id: String,
    pub body_json: String,
}

/// One wire-journal row (spec/reeve/05-health-journal.md §7.1): the
/// stored form of a wire [`reeve_types::reeve::health::JournalRecord`].
/// `ts` is the original timestamp — assigned when journaled, never
/// rewritten; `kind` is one of `status | health | lifecycle | gap`.
#[derive(Debug, Clone, PartialEq)]
pub struct WireRecord {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub payload: Option<String>,
}

/// Evidence of a forced eviction of unacknowledged records
/// (spec/reeve/05-health-journal.md §7.1 gap mark).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapMark {
    /// First evicted unacknowledged seq.
    pub from_seq: i64,
    /// Last evicted seq.
    pub to_seq: i64,
    /// How many unacknowledged records were evicted.
    pub records: u64,
    /// The seq of the gap record itself.
    pub gap_seq: i64,
}

/// Handle on agent.db.
pub struct AgentDb {
    conn: Connection,
}

impl AgentDb {
    /// Open (creating if absent) the agent database. Idempotent —
    /// startup IS recovery (Law 3). WAL, foreign_keys ON,
    /// busy_timeout 5s.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            // Creating the data dir is part of idempotent startup.
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS manifest_state (
                id               INTEGER PRIMARY KEY CHECK (id = 1),
                -- ManifestVersion u64 bit-cast to i64. Compared only
                -- in Rust (u64 order != i64 order past bit 63).
                manifest_version INTEGER NOT NULL,
                etag             TEXT NOT NULL,
                manifest_json    TEXT NOT NULL,
                accepted_at      TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS journal (
                seq      INTEGER PRIMARY KEY AUTOINCREMENT,
                ts       TEXT NOT NULL,
                severity TEXT NOT NULL
                         CHECK (severity IN ('info','notable','security','error')),
                event    TEXT NOT NULL,
                detail   TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS applied_state (
                app_id          TEXT PRIMARY KEY,
                content_hash    TEXT NOT NULL,
                secrets_version TEXT,
                phase           TEXT NOT NULL
                                CHECK (phase IN ('planned','applying','applied',
                                                 'failed','removing','removed')),
                updated_at      TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS bundle_state (
                id         INTEGER PRIMARY KEY CHECK (id = 1),
                -- OCI manifest digest of the swapped-in render
                -- bundle, grammar sha256:<hex>.
                digest     TEXT NOT NULL,
                swapped_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS status_reports (
                -- NOT auto-assigned since B7: the seq is allocated by
                -- the wire_journal insert in the same transaction, so
                -- live and backfill paths share one seq space (§7.3).
                seq           INTEGER PRIMARY KEY AUTOINCREMENT,
                ts            TEXT NOT NULL,
                app_id        TEXT NOT NULL,
                deployment_id TEXT NOT NULL,
                -- Serialized Margo DeploymentStatusManifest WITHOUT
                -- the reeve extension; the sender attaches
                -- {observedAt: ts, seq} at transmission time
                -- (spec/reeve/05-health-journal.md §7.3).
                body_json     TEXT NOT NULL,
                sent          INTEGER NOT NULL DEFAULT 0 CHECK (sent IN (0, 1))
            );
            CREATE TABLE IF NOT EXISTS wire_journal (
                -- AUTOINCREMENT = the persistent monotonic per-device
                -- seq counter (§7.1): never reused, survives eviction
                -- via sqlite_sequence.
                seq     INTEGER PRIMARY KEY AUTOINCREMENT,
                -- Original timestamp (RFC 3339): assigned here, when
                -- journaled, never rewritten (§7 "original timestamp").
                ts      TEXT NOT NULL,
                kind    TEXT NOT NULL
                        CHECK (kind IN ('status','health','lifecycle','gap')),
                payload TEXT
            );
            CREATE TABLE IF NOT EXISTS journal_ack (
                id        INTEGER PRIMARY KEY CHECK (id = 1),
                acked_seq INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO journal_ack (id, acked_seq) VALUES (1, 0);
            -- Upgrade seam: a pre-B7 agent.db already handed out
            -- status seqs from status_reports' own counter. Seed the
            -- shared counter past them so the unified space never
            -- collides with seqs the server may already hold.
            INSERT INTO sqlite_sequence (name, seq)
            SELECT 'wire_journal', (SELECT MAX(seq) FROM status_reports)
            WHERE NOT EXISTS
                  (SELECT 1 FROM sqlite_sequence WHERE name = 'wire_journal')
              AND EXISTS (SELECT 1 FROM status_reports);
            -- Every agent journal entry IS a lifecycle mark (§7.1:
            -- start, converge begin/end, provider errors — all land
            -- in `journal`); the trigger mirrors them into the wire
            -- journal atomically with the insert that created them.
            CREATE TRIGGER IF NOT EXISTS journal_to_wire
            AFTER INSERT ON journal BEGIN
                INSERT INTO wire_journal (ts, kind, payload)
                VALUES (NEW.ts, 'lifecycle',
                        json_object('severity', NEW.severity,
                                    'event',    NEW.event,
                                    'detail',   NEW.detail));
            END;
            "#,
        )?;
        Ok(AgentDb { conn })
    }

    /// The last ACCEPTED manifest — the monotonicity floor
    /// (spec/reeve/08-packaging.md §10.2) and the state the agent
    /// continues from offline (Law 5). `None` before first accept.
    pub fn last_accepted(&self) -> Result<Option<AcceptedManifest>, StateError> {
        let row = self
            .conn
            .query_row(
                "SELECT manifest_version, etag, manifest_json FROM manifest_state WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((v, etag, json)) => Ok(Some(AcceptedManifest {
                version: ManifestVersion(v as u64),
                etag,
                manifest: serde_json::from_str(&json)?,
            })),
        }
    }

    /// Accept a manifest: persist it as the new floor AND journal
    /// the acceptance, atomically (one transaction — kill -9 between
    /// the two must be impossible, Law 3).
    pub fn record_accepted(
        &mut self,
        manifest: &StateManifest,
        etag: &str,
        severity: Severity,
        event: &str,
        detail: &str,
    ) -> Result<(), StateError> {
        let json = serde_json::to_string(manifest)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO manifest_state (id, manifest_version, etag, manifest_json, accepted_at)
             VALUES (1, ?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(id) DO UPDATE SET
                 manifest_version = excluded.manifest_version,
                 etag             = excluded.etag,
                 manifest_json    = excluded.manifest_json,
                 accepted_at      = excluded.accepted_at",
            params![manifest.manifest_version.0 as i64, etag, json],
        )?;
        tx.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?1, ?2, ?3)",
            params![severity.as_str(), event, detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Append one journal entry (its own implicit transaction).
    pub fn journal(
        &self,
        severity: Severity,
        event: &str,
        detail: &str,
    ) -> Result<(), StateError> {
        self.conn.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?1, ?2, ?3)",
            params![severity.as_str(), event, detail],
        )?;
        Ok(())
    }

    /// Current UTC time as an RFC 3339 / ISO-8601 string in the exact
    /// format the journal uses (SQLite `strftime`, millisecond `Z`).
    /// Used to stamp device-captured timestamps (e.g. ext-logs
    /// `capturedAt`, REV-011) with the same clock the rest of the DB
    /// records — no extra time crate needed.
    pub fn now_rfc3339(&self) -> Result<String, StateError> {
        let ts: String = self.conn.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            [],
            |r| r.get(0),
        )?;
        Ok(ts)
    }

    /// All journal entries in sequence order.
    pub fn journal_entries(&self) -> Result<Vec<JournalEntry>, StateError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, ts, severity, event, detail FROM journal ORDER BY seq")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(JournalEntry {
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    severity: r.get(2)?,
                    event: r.get(3)?,
                    detail: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The applied-state table — what "continue from last known
    /// state" (Law 5) continues from. B3 writes the phases; B1 only
    /// reads.
    pub fn applied_apps(&self) -> Result<Vec<AppliedApp>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT app_id, content_hash, secrets_version, phase
             FROM applied_state ORDER BY app_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AppliedApp {
                    app_id: r.get(0)?,
                    content_hash: r.get(1)?,
                    secrets_version: r.get(2)?,
                    phase: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Digest (`sha256:<hex>`) of the render bundle currently
    /// swapped into place, if any. `None` before the first pull.
    pub fn pulled_bundle(&self) -> Result<Option<String>, StateError> {
        Ok(self
            .conn
            .query_row(
                "SELECT digest FROM bundle_state WHERE id = 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Record the swapped-in bundle digest AND journal it, atomically
    /// (Law 3: one transaction). Called only AFTER the atomic dir
    /// swap — the swap is the commitment point; this record is the
    /// durable pointer to it (docs/decisions/tree-render.md D2).
    /// `event` is `bundle-swapped` on the pull path and
    /// `bundle-rolled-forward` when startup recovery completes an
    /// interrupted swap-then-record.
    pub fn record_bundle(
        &mut self,
        digest: &str,
        event: &str,
        detail: &str,
    ) -> Result<(), StateError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bundle_state (id, digest, swapped_at)
             VALUES (1, ?1, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(id) DO UPDATE SET
                 digest     = excluded.digest,
                 swapped_at = excluded.swapped_at",
            params![digest],
        )?;
        tx.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'info', ?1, ?2)",
            params![event, detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Clear the bundle record (the on-disk bundle vanished — startup
    /// recovery reconciles the DB to disk truth). NOTABLE: this only
    /// happens on external interference with the data dir.
    pub fn clear_bundle(&mut self, detail: &str) -> Result<(), StateError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM bundle_state WHERE id = 1", [])?;
        tx.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'notable', 'bundle-state-cleared', ?1)",
            params![detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record one D5 phase transition: upsert the applied-state row
    /// AND journal the transition, atomically (one transaction,
    /// Law 3 — the phase row and its journal evidence can never
    /// diverge). This is THE call that records intent BEFORE action
    /// (docs/decisions/agent.md D5): converge writes `planned` /
    /// `applying` / `removing` before it acts, and `applied` /
    /// `failed` / `removed` after.
    pub fn record_phase(
        &mut self,
        app_id: &str,
        content_hash: &str,
        secrets_version: Option<&str>,
        phase: &str,
        detail: &str,
    ) -> Result<(), StateError> {
        let severity = if phase == "failed" {
            Severity::Error
        } else {
            Severity::Info
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO applied_state (app_id, content_hash, secrets_version, phase, updated_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(app_id) DO UPDATE SET
                 content_hash    = excluded.content_hash,
                 secrets_version = excluded.secrets_version,
                 phase           = excluded.phase,
                 updated_at      = excluded.updated_at",
            params![app_id, content_hash, secrets_version, phase],
        )?;
        tx.execute(
            "INSERT INTO journal (ts, severity, event, detail)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?1, ?2, ?3)",
            params![severity.as_str(), format!("app-{phase}"), format!("{app_id}: {detail}")],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record one status report locally FIRST (store-and-forward,
    /// spec/reeve/05-health-journal.md §7.3: "journaling MUST NOT
    /// depend on connectivity"). The seq is allocated by the
    /// `wire_journal` append and shared with the live-send queue row
    /// — one transaction, one seq, two paths (§7.3: the server
    /// detects records it already holds by `(deviceId, seq)`, which
    /// only works if live and backfill agree on the seq). Returns
    /// that monotonic `seq`.
    pub fn record_status(
        &self,
        app_id: &str,
        deployment_id: &str,
        body_json: &str,
    ) -> Result<i64, StateError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO wire_journal (ts, kind, payload)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'status', ?1)",
            params![body_json],
        )?;
        let seq = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO status_reports (seq, ts, app_id, deployment_id, body_json, sent)
             SELECT seq, ts, ?2, ?3, payload, 0 FROM wire_journal WHERE seq = ?1",
            params![seq, app_id, deployment_id],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// Append one health sample to the wire journal (REV-004 §7.2;
    /// written by the ext-health sampler). Local-first, always —
    /// transmission is the backfill sweep's job (§7.1: "journaling
    /// MUST NOT depend on connectivity"). Returns the record's seq.
    pub fn record_health(&self, sample_json: &str) -> Result<i64, StateError> {
        self.conn.execute(
            "INSERT INTO wire_journal (ts, kind, payload)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'health', ?1)",
            params![sample_json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The server-acknowledged watermark (§7.3): every wire record
    /// with `seq <= watermark` is contiguously ingested upstream and
    /// therefore evictable (§7.1). 0 before the first ack.
    pub fn journal_watermark(&self) -> Result<i64, StateError> {
        Ok(self
            .conn
            .query_row("SELECT acked_seq FROM journal_ack WHERE id = 1", [], |r| r.get(0))?)
    }

    /// Persist a new ack watermark. Callers advance it monotonically
    /// (`max(old, ack)`); the raw set exists so tests can simulate a
    /// crash that lost the watermark write — resending below it is
    /// harmless either way (§7.3 idempotency).
    pub fn set_journal_watermark(&self, acked_seq: i64) -> Result<(), StateError> {
        self.conn.execute(
            "UPDATE journal_ack SET acked_seq = ?1 WHERE id = 1",
            params![acked_seq],
        )?;
        Ok(())
    }

    /// Unacknowledged wire records, oldest first (§7.3: "batches
    /// ordered by sequence number"), at most `limit`.
    pub fn unacked_wire_records(&self, limit: u32) -> Result<Vec<WireRecord>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, kind, payload FROM wire_journal
             WHERE seq > (SELECT acked_seq FROM journal_ack WHERE id = 1)
             ORDER BY seq LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok(WireRecord {
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    kind: r.get(2)?,
                    payload: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every wire record, in seq order (tests / local inspection).
    pub fn wire_records(&self) -> Result<Vec<WireRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, ts, kind, payload FROM wire_journal ORDER BY seq")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(WireRecord {
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    kind: r.get(2)?,
                    payload: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Bounded retention (spec/reeve/05-health-journal.md §7.1):
    /// - **age**: acknowledged records older than `retention_days`
    ///   are evicted silently (the server holds them);
    /// - **size**: if the journal still exceeds `max_bytes`, evict
    ///   oldest-first — acknowledged records freely, and
    ///   unacknowledged records ONLY under this size force, in which
    ///   case ONE gap mark is appended so the server can distinguish
    ///   "evicted" from "never happened".
    ///
    /// One transaction (Law 3). Status-queue rows whose journal
    /// identity was evicted are dropped with it (their seq no longer
    /// exists to report under). Returns the gap mark if one was
    /// journaled.
    pub fn evict_journal(
        &self,
        retention_days: u32,
        max_bytes: u64,
    ) -> Result<Option<GapMark>, StateError> {
        // Approximate fixed per-row cost (seq + ts + kind + b-tree)
        // on top of the payload bytes.
        const ROW_OVERHEAD: i64 = 64;
        let tx = self.conn.unchecked_transaction()?;
        let watermark: i64 =
            tx.query_row("SELECT acked_seq FROM journal_ack WHERE id = 1", [], |r| r.get(0))?;
        // Age retention — acked only (§7.1: unacknowledged records
        // are protected from everything but the size bound).
        tx.execute(
            "DELETE FROM wire_journal
             WHERE seq <= ?1
               AND ts <= strftime('%Y-%m-%dT%H:%M:%fZ','now', ?2)",
            params![watermark, format!("-{retention_days} days")],
        )?;
        // Size bound — walk oldest-first until under it.
        let total: i64 = tx.query_row(
            "SELECT COALESCE(SUM(COALESCE(length(payload), 0) + ?1), 0) FROM wire_journal",
            params![ROW_OVERHEAD],
            |r| r.get(0),
        )?;
        let max = i64::try_from(max_bytes).unwrap_or(i64::MAX);
        let mut gap = None;
        if total > max {
            let mut excess = total - max;
            let mut cutoff: Option<i64> = None;
            let mut first_forced: Option<i64> = None;
            let mut forced: u64 = 0;
            {
                let mut stmt = tx.prepare(
                    "SELECT seq, COALESCE(length(payload), 0) + ?1
                     FROM wire_journal ORDER BY seq",
                )?;
                let mut rows = stmt.query(params![ROW_OVERHEAD])?;
                while excess > 0 {
                    let Some(row) = rows.next()? else { break };
                    let seq: i64 = row.get(0)?;
                    excess -= row.get::<_, i64>(1)?;
                    cutoff = Some(seq);
                    if seq > watermark {
                        forced += 1;
                        first_forced.get_or_insert(seq);
                    }
                }
            }
            if let Some(cutoff) = cutoff {
                tx.execute("DELETE FROM wire_journal WHERE seq <= ?1", params![cutoff])?;
                if let Some(from_seq) = first_forced {
                    // §7.1 gap mark: unacknowledged records were
                    // forced out; the record of that fact takes the
                    // next seq like any other journal append.
                    tx.execute(
                        "INSERT INTO wire_journal (ts, kind, payload)
                         VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'gap',
                                 json_object('evictedFromSeq', ?1,
                                             'evictedToSeq',   ?2,
                                             'records',        ?3))",
                        params![from_seq, cutoff, forced as i64],
                    )?;
                    gap = Some(GapMark {
                        from_seq,
                        to_seq: cutoff,
                        records: forced,
                        gap_seq: tx.last_insert_rowid(),
                    });
                }
            }
        }
        // Mirror: a status-queue row whose wire record is gone has no
        // journal identity left to report under.
        tx.execute(
            "DELETE FROM status_reports
             WHERE seq NOT IN (SELECT seq FROM wire_journal)",
            [],
        )?;
        tx.commit()?;
        Ok(gap)
    }

    /// Unsent status reports in sequence order (§7.3: batches ordered
    /// by sequence number).
    pub fn unsent_statuses(&self) -> Result<Vec<StatusRow>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, app_id, deployment_id, body_json FROM status_reports
             WHERE sent = 0 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StatusRow {
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    app_id: r.get(2)?,
                    deployment_id: r.get(3)?,
                    body_json: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark one status report transmitted. Idempotent; resending
    /// after a crash is harmless because the server deduplicates by
    /// `(deviceId, seq)` (spec/reeve/05-health-journal.md §7.3).
    pub fn mark_status_sent(&self, seq: i64) -> Result<(), StateError> {
        self.conn.execute(
            "UPDATE status_reports SET sent = 1 WHERE seq = ?1",
            params![seq],
        )?;
        Ok(())
    }

    /// Upsert one applied-state row (exposed now so B3's provider
    /// has its contract; used by tests).
    pub fn record_applied(
        &self,
        app_id: &str,
        content_hash: &str,
        secrets_version: Option<&str>,
        phase: &str,
    ) -> Result<(), StateError> {
        self.conn.execute(
            "INSERT INTO applied_state (app_id, content_hash, secrets_version, phase, updated_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(app_id) DO UPDATE SET
                 content_hash    = excluded.content_hash,
                 secrets_version = excluded.secrets_version,
                 phase           = excluded.phase,
                 updated_at      = excluded.updated_at",
            params![app_id, content_hash, secrets_version, phase],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_types::reeve::manifest::{BundleRef, StateManifest};

    fn manifest(version: u64) -> StateManifest {
        StateManifest {
            manifest_version: ManifestVersion(version),
            bundle: Some(BundleRef {
                media_type: None,
                digest: format!("sha256:{}", "a".repeat(64)),
                size_bytes: Some(10),
                url: "/v2/x/blobs/sha256:...".into(),
            }),
            apps: vec![],
        }
    }

    #[test]
    fn open_is_idempotent_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        {
            let mut db = AgentDb::open(&path).unwrap();
            db.record_accepted(&manifest(7), "sha256:etag", Severity::Info, "accepted", "")
                .unwrap();
        } // dropped without any shutdown ceremony (Law 3)
        let db = AgentDb::open(&path).unwrap(); // startup IS recovery
        let got = db.last_accepted().unwrap().unwrap();
        assert_eq!(got.version, ManifestVersion(7));
        assert_eq!(got.etag, "sha256:etag");
        assert_eq!(db.journal_entries().unwrap().len(), 1);
        // Re-open once more: schema creation must be idempotent.
        drop(db);
        AgentDb::open(&path).unwrap();
    }

    #[test]
    fn last_accepted_none_before_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        assert!(db.last_accepted().unwrap().is_none());
    }

    #[test]
    fn record_accepted_overwrites_single_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.record_accepted(&manifest(1), "sha256:e1", Severity::Info, "accepted", "")
            .unwrap();
        db.record_accepted(&manifest(2), "sha256:e2", Severity::Info, "accepted", "")
            .unwrap();
        let got = db.last_accepted().unwrap().unwrap();
        assert_eq!(got.version, ManifestVersion(2));
        assert_eq!(got.etag, "sha256:e2");
        assert_eq!(db.journal_entries().unwrap().len(), 2);
    }

    #[test]
    fn manifest_version_roundtrips_past_bit_63() {
        // epoch 0x8000+ sets the sign bit of the i64 storage cast;
        // the bit-cast roundtrip must still be exact.
        let dir = tempfile::tempdir().unwrap();
        let mut db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        let v = ManifestVersion::pack(0xFFFF, 5).unwrap();
        db.record_accepted(&manifest(v.0), "sha256:e", Severity::Info, "accepted", "")
            .unwrap();
        assert_eq!(db.last_accepted().unwrap().unwrap().version, v);
    }

    #[test]
    fn journal_severities_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.journal(Severity::Security, "manifest-regression", "42 -> 41")
            .unwrap();
        db.journal(Severity::Notable, "epoch-bump", "0 -> 1").unwrap();
        let entries = db.journal_entries().unwrap();
        assert_eq!(entries[0].severity, "security");
        assert_eq!(entries[1].severity, "notable");
        assert!(entries[0].seq < entries[1].seq);
    }

    #[test]
    fn bundle_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        assert_eq!(db.pulled_bundle().unwrap(), None);
        let d1 = format!("sha256:{}", "1".repeat(64));
        let d2 = format!("sha256:{}", "2".repeat(64));
        db.record_bundle(&d1, "bundle-swapped", "first").unwrap();
        assert_eq!(db.pulled_bundle().unwrap().as_deref(), Some(d1.as_str()));
        db.record_bundle(&d2, "bundle-swapped", "second").unwrap();
        assert_eq!(db.pulled_bundle().unwrap().as_deref(), Some(d2.as_str()));
        db.clear_bundle("bundle dir vanished").unwrap();
        assert_eq!(db.pulled_bundle().unwrap(), None);
        let events: Vec<String> = db
            .journal_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(
            events,
            vec!["bundle-swapped", "bundle-swapped", "bundle-state-cleared"]
        );
    }

    #[test]
    fn record_phase_upserts_and_journals_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.record_phase("web", "sha256:h1", None, "planned", "hash sha256:h1")
            .unwrap();
        db.record_phase("web", "sha256:h1", None, "applying", "")
            .unwrap();
        db.record_phase("web", "sha256:h1", Some("sv1"), "applied", "")
            .unwrap();
        let apps = db.applied_apps().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].phase, "applied");
        assert_eq!(apps[0].secrets_version.as_deref(), Some("sv1"));
        let events: Vec<String> = db
            .journal_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(events, vec!["app-planned", "app-applying", "app-applied"]);
        // failed phase journals at error severity
        db.record_phase("web", "sha256:h1", None, "failed", "boom")
            .unwrap();
        let last = db.journal_entries().unwrap().pop().unwrap();
        assert_eq!(last.severity, "error");
        assert_eq!(last.event, "app-failed");
        // invalid phase rejected by CHECK, and the journal side of
        // the transaction must not land either (atomicity).
        let before = db.journal_entries().unwrap().len();
        assert!(db.record_phase("web", "sha256:h1", None, "exploded", "").is_err());
        assert_eq!(db.journal_entries().unwrap().len(), before);
    }

    #[test]
    fn status_reports_store_and_forward() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        let s1 = db.record_status("web", "dep-1", "{\"a\":1}").unwrap();
        let s2 = db.record_status("db", "dep-2", "{\"b\":2}").unwrap();
        assert!(s1 < s2, "seq must be monotonic");
        let unsent = db.unsent_statuses().unwrap();
        assert_eq!(unsent.len(), 2);
        assert_eq!(unsent[0].seq, s1);
        assert_eq!(unsent[0].app_id, "web");
        assert!(!unsent[0].ts.is_empty());
        db.mark_status_sent(s1).unwrap();
        let unsent = db.unsent_statuses().unwrap();
        assert_eq!(unsent.len(), 1);
        assert_eq!(unsent[0].seq, s2);
        // marking twice is harmless (crash-resend idempotency)
        db.mark_status_sent(s1).unwrap();
    }

    /// §7.1/§7.3: one monotonic seq space across ALL record kinds —
    /// the live status path and the backfill path carry the SAME seq
    /// for the same record, and lifecycle journal entries are
    /// mirrored in by trigger.
    #[test]
    fn wire_journal_unifies_seq_space() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.journal(Severity::Info, "agent-start", "v0.1.0").unwrap();
        let status_seq = db.record_status("web", "dep-1", "{\"a\":1}").unwrap();
        let health_seq = db.record_health("{\"load\":[0.5]}").unwrap();

        let records = db.wire_records().unwrap();
        assert_eq!(
            records.iter().map(|r| (r.seq, r.kind.as_str())).collect::<Vec<_>>(),
            vec![(1, "lifecycle"), (2, "status"), (3, "health")],
            "one contiguous seq space across kinds"
        );
        assert_eq!(status_seq, 2);
        assert_eq!(health_seq, 3);
        // The live-send queue row shares the wire seq AND timestamp.
        let unsent = db.unsent_statuses().unwrap();
        assert_eq!(unsent.len(), 1);
        assert_eq!(unsent[0].seq, status_seq);
        assert_eq!(unsent[0].ts, records[1].ts);
        assert_eq!(unsent[0].body_json, "{\"a\":1}");
        // Trigger mirrored the journal entry as a lifecycle payload.
        let lifecycle: serde_json::Value =
            serde_json::from_str(records[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(lifecycle["event"], "agent-start");
        assert_eq!(lifecycle["severity"], "info");
        assert_eq!(lifecycle["detail"], "v0.1.0");
    }

    /// The ack watermark (§7.3) gates what backfill re-reads, and it
    /// survives restart (Law 3).
    #[test]
    fn watermark_gates_unacked_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        {
            let db = AgentDb::open(&path).unwrap();
            assert_eq!(db.journal_watermark().unwrap(), 0);
            for i in 0..3 {
                db.record_health(&format!("{{\"i\":{i}}}")).unwrap();
            }
            assert_eq!(db.unacked_wire_records(10).unwrap().len(), 3);
            db.set_journal_watermark(2).unwrap();
            let unacked = db.unacked_wire_records(10).unwrap();
            assert_eq!(unacked.len(), 1);
            assert_eq!(unacked[0].seq, 3);
            // limit respected (batching)
            db.set_journal_watermark(0).unwrap();
            assert_eq!(db.unacked_wire_records(2).unwrap().len(), 2);
            db.set_journal_watermark(2).unwrap();
        } // no shutdown ceremony
        let db = AgentDb::open(&path).unwrap();
        assert_eq!(db.journal_watermark().unwrap(), 2, "watermark survives restart");
    }

    /// §7.1 age retention: ACKED records age out silently (no gap —
    /// the server holds them); unacked records are immune to age.
    #[test]
    fn age_eviction_takes_only_acked_and_leaves_no_gap() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        for i in 0..4 {
            db.record_health(&format!("{{\"i\":{i}}}")).unwrap();
        }
        db.set_journal_watermark(2).unwrap();
        // retention_days = 0 => everything already-written is "old".
        let gap = db.evict_journal(0, u64::MAX).unwrap();
        assert!(gap.is_none(), "acked age-out journals no gap");
        let seqs: Vec<i64> = db.wire_records().unwrap().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![3, 4], "unacked records immune to age");
        // Seq counter is NOT reset by eviction: next record continues.
        assert_eq!(db.record_health("{}").unwrap(), 5);
    }

    /// §7.1 size force: evicting unacknowledged records appends ONE
    /// gap mark recording the evicted range, and the mirrored
    /// status-queue row goes with its journal identity.
    #[test]
    fn forced_size_eviction_emits_gap_mark() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        let filler = format!("{{\"pad\":\"{}\"}}", "x".repeat(256));
        db.record_status("web", "dep-1", &filler).unwrap(); // seq 1
        for _ in 0..3 {
            db.record_health(&filler).unwrap(); // seq 2..4
        }
        // Nothing acked; bound small enough to force out seqs 1-2.
        let gap = db.evict_journal(30, 900).unwrap().expect("gap mark");
        assert_eq!(gap.from_seq, 1);
        assert_eq!(gap.to_seq, 2);
        assert_eq!(gap.records, 2);
        let records = db.wire_records().unwrap();
        assert_eq!(
            records.iter().map(|r| (r.seq, r.kind.as_str())).collect::<Vec<_>>(),
            vec![(3, "health"), (4, "health"), (5, "gap")]
        );
        let payload: serde_json::Value =
            serde_json::from_str(records[2].payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["evictedFromSeq"], 1);
        assert_eq!(payload["evictedToSeq"], 2);
        assert_eq!(payload["records"], 2);
        // The evicted status's live-queue row is gone too.
        assert!(db.unsent_statuses().unwrap().is_empty());
        // Within bounds now: idempotent re-run evicts nothing more.
        assert!(db.evict_journal(30, 900).unwrap().is_none());
        assert_eq!(db.wire_records().unwrap().len(), 3);
    }

    /// Upgrade seam: a pre-B7 database whose status_reports already
    /// used seqs must not hand the same seqs out again from the
    /// unified counter (the server may already hold them).
    #[test]
    fn wire_counter_seeds_past_legacy_status_seqs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        {
            let db = AgentDb::open(&path).unwrap();
            // Simulate legacy rows: direct insert bypassing the
            // unified allocator, as the pre-B7 code did.
            db.conn
                .execute_batch(
                    "DELETE FROM wire_journal;
                     DELETE FROM sqlite_sequence WHERE name = 'wire_journal';
                     INSERT INTO status_reports (seq, ts, app_id, deployment_id, body_json, sent)
                     VALUES (7, '2026-01-01T00:00:00Z', 'web', 'dep-1', '{}', 1);",
                )
                .unwrap();
        }
        let db = AgentDb::open(&path).unwrap(); // re-open runs the seed
        let seq = db.record_health("{}").unwrap();
        assert_eq!(seq, 8, "unified counter starts past legacy status seqs");
    }

    #[test]
    fn applied_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&dir.path().join("agent.db")).unwrap();
        db.record_applied("app-a", "sha256:h1", None, "applied").unwrap();
        db.record_applied("app-a", "sha256:h2", Some("sv1"), "applying")
            .unwrap();
        let apps = db.applied_apps().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].content_hash, "sha256:h2");
        assert_eq!(apps[0].phase, "applying");
        // invalid phase rejected by CHECK
        assert!(db.record_applied("app-b", "sha256:h", None, "exploded").is_err());
    }
}
