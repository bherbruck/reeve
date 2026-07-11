//! Status + journal ingest persistence (C5) — the reeve-server
//! implementation of `device_api::StatusIngest` over the single SQLite
//! DB (Law 4).
//!
//! Spec: spec/reeve/05-health-journal.md §7.3 —
//! - **Idempotent by `(deviceId, seq)`**: `INSERT OR IGNORE` on the
//!   `status_journal` PK; the server MUST NOT overwrite an
//!   already-ingested record, so a crash-resend is harmless (Law 3).
//! - **Late ingest at original timestamps**: `observed_at` is stored
//!   verbatim as the device asserted it; `received_at` (server clock)
//!   is recorded alongside — the pair makes tampering and skew visible.
//! - **Ack = highest contiguously ingested seq**: computed from the
//!   journal itself, so it survives restarts with zero extra state.
//!
//! Current-state materialization: `deployment_status_current` holds the
//! latest state per (device, deployment) for the UI, where "latest" is
//! the highest SEQ, not the latest ARRIVAL — an out-of-order backfilled
//! record never regresses current state. `devices.last_seen_at` is
//! touched on every ingest (presence input, presence.rs).
//!
//! Each ingest call is ONE transaction: kill -9 mid-batch leaves either
//! nothing or a prefix, and the resend converges (Law 3).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use device_api::status::{StatusIngest, StatusIngestError};
use reeve_types::margo::status::{
    DEPLOYMENT_STATUS_KIND, DeploymentState, DeploymentStatusManifest,
};
use reeve_types::reeve::events::{DeploymentStatusEvent, SseEvent};
use reeve_types::reeve::health::{JournalAck, JournalBatch, JournalRecordKind};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use crate::db;
use crate::events::EventHub;

/// `StatusIngest` over the server DB. Constructed in router assembly;
/// the routes live in device-api (Law 2 seam). Emits
/// `deployment-status` events (spec/reeve/04-status-stream.md §6.3:
/// "ingested manifest changes a deployment's overall state") on the
/// C8 event hub — droppable hints, never load-bearing.
pub struct SqliteStatusIngest {
    db: Arc<Mutex<Connection>>,
    events: EventHub,
}

impl SqliteStatusIngest {
    pub fn new(db: Arc<Mutex<Connection>>, events: EventHub) -> Self {
        Self { db, events }
    }
}

fn internal(e: impl std::fmt::Display) -> StatusIngestError {
    StatusIngestError::Internal(e.to_string())
}

/// Wire seq (u64) -> storage seq (i64). A seq beyond i64 is a semantic
/// error (422), not a silent truncation.
fn storage_seq(seq: u64) -> Result<i64, StatusIngestError> {
    i64::try_from(seq).map_err(|_| StatusIngestError::Invalid(format!("seq {seq} out of range")))
}

/// Lowercase wire name of a deployment state
/// (`deployment-status.md` state enum).
fn state_str(state: DeploymentState) -> &'static str {
    match state {
        DeploymentState::Pending => "pending",
        DeploymentState::Installing => "installing",
        DeploymentState::Installed => "installed",
        DeploymentState::Removing => "removing",
        DeploymentState::Removed => "removed",
        DeploymentState::Failed => "failed",
    }
}

/// Inverse of [`state_str`] — reading `deployment_status_current`
/// back into the Margo enum for event payloads (§6.3: `state` is the
/// Margo enum). `None` for a value outside the pinned set.
fn state_from_str(s: &str) -> Option<DeploymentState> {
    Some(match s {
        "pending" => DeploymentState::Pending,
        "installing" => DeploymentState::Installing,
        "installed" => DeploymentState::Installed,
        "removing" => DeploymentState::Removing,
        "removed" => DeploymentState::Removed,
        "failed" => DeploymentState::Failed,
        _ => return None,
    })
}

/// Stored overall state of one (device, deployment), if any.
fn stored_state(
    conn: &Connection,
    device_id: &str,
    deployment_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT state FROM deployment_status_current
         WHERE device_id = ?1 AND deployment_id = ?2",
        params![device_id, deployment_id],
        |r| r.get(0),
    )
    .optional()
}

/// Storage name of a journal record kind (mirrors the wire kebab-case
/// of `JournalRecordKind`; the V4 CHECK constraint pins the set).
fn kind_str(kind: JournalRecordKind) -> &'static str {
    match kind {
        JournalRecordKind::Status => "status",
        JournalRecordKind::Health => "health",
        JournalRecordKind::Lifecycle => "lifecycle",
        JournalRecordKind::Gap => "gap",
    }
}

/// Presence input: every ingest (live status, backfill batch, manifest
/// poll — delivery.rs) proves the device was reachable now.
pub fn touch_last_seen(conn: &Connection, device_id: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE devices SET last_seen_at = ?2 WHERE device_id = ?1",
        params![device_id, now],
    )?;
    Ok(())
}

/// Append one journal record; `false` if `(device_id, seq)` was already
/// ingested (the existing record is never overwritten — §7.3).
fn journal_insert(
    tx: &Transaction<'_>,
    device_id: &str,
    seq: i64,
    observed_at: &str,
    received_at: i64,
    kind: &str,
    payload: Option<&str>,
) -> rusqlite::Result<bool> {
    let n = tx.execute(
        "INSERT OR IGNORE INTO status_journal
             (device_id, seq, observed_at, received_at, kind, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![device_id, seq, observed_at, received_at, kind, payload],
    )?;
    Ok(n > 0)
}

/// One candidate row for `deployment_status_current`.
struct CurrentStatus<'a> {
    deployment_id: &'a str,
    state: &'a str,
    seq: Option<i64>,
    observed_at: Option<&'a str>,
    received_at: i64,
    payload: &'a str,
}

/// Upsert `deployment_status_current` under the max-seq rule:
/// - a seq'd report applies iff its seq >= the stored seq (or the
///   stored row is un-seq'd/absent) — current = max seq, not max
///   arrival;
/// - an un-seq'd (vanilla Margo) report applies only while the stored
///   row is also un-seq'd — arrival order is Margo's own connected
///   assumption, but it never regresses journal-sequenced state.
fn upsert_current(
    tx: &Transaction<'_>,
    device_id: &str,
    row: &CurrentStatus<'_>,
) -> rusqlite::Result<()> {
    // Terminal-removed CLEARS the current row (REV-010 §11.4 move/undeploy).
    // A deployment removed from a device's desired state converges, downs,
    // and reports a terminal `removed`; it must then DISAPPEAR from the
    // device's current deployments, not linger at "removed". The history
    // stays in `status_journal` — this only touches the derived current
    // table. The DELETE is guarded by the SAME seq-precedence rule as the
    // upsert below, so a stale/out-of-order `removed` never wipes a newer
    // state, and a `removed` for a (device, deployment) we never tracked is
    // a harmless no-op (0 rows).
    if row.state == state_str(DeploymentState::Removed) {
        tx.execute(
            "DELETE FROM deployment_status_current
             WHERE device_id = ?1 AND deployment_id = ?2
               AND ((?3 IS NOT NULL AND (seq IS NULL OR ?3 >= seq))
                    OR (?3 IS NULL AND seq IS NULL))",
            params![device_id, row.deployment_id, row.seq],
        )?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO deployment_status_current
             (device_id, deployment_id, state, seq, observed_at, received_at, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (device_id, deployment_id) DO UPDATE SET
             state = excluded.state,
             seq = excluded.seq,
             observed_at = excluded.observed_at,
             received_at = excluded.received_at,
             payload = excluded.payload
         WHERE (excluded.seq IS NOT NULL
                AND (deployment_status_current.seq IS NULL
                     OR excluded.seq >= deployment_status_current.seq))
            OR (excluded.seq IS NULL AND deployment_status_current.seq IS NULL)",
        params![
            device_id,
            row.deployment_id,
            row.state,
            row.seq,
            row.observed_at,
            row.received_at,
            row.payload
        ],
    )?;
    Ok(())
}

/// Highest contiguously ingested seq for a device (§7.3 ack): the end
/// of the run starting at the lowest journaled seq — the first hole
/// stops the ack, so eviction on the agent never outruns ingestion.
/// 0 when nothing is journaled.
pub fn acked_seq(conn: &Connection, device_id: &str) -> rusqlite::Result<u64> {
    let mut stmt =
        conn.prepare("SELECT seq FROM status_journal WHERE device_id = ?1 ORDER BY seq")?;
    let mut rows = stmt.query(params![device_id])?;
    let mut acked: Option<i64> = None;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        match acked {
            None => acked = Some(seq),
            Some(a) if seq == a + 1 => acked = Some(seq),
            Some(_) => break, // first hole
        }
    }
    Ok(acked.map(|a| a.max(0) as u64).unwrap_or(0))
}

impl StatusIngest for SqliteStatusIngest {
    fn ingest_status(
        &self,
        device_id: &str,
        deployment_id: &str,
        manifest: &DeploymentStatusManifest,
        raw_body: &str,
    ) -> Result<(), StatusIngestError> {
        let now = db::now_secs();
        let mut conn = self.db.lock().expect("db mutex poisoned");
        let tx = conn.transaction().map_err(internal)?;

        touch_last_seen(&tx, device_id, now).map_err(internal)?;

        // rev-004/1 additive object: place the report in history and
        // dedupe against records already held (§7.3 live path). The
        // journal stores the VERBATIM body — a lossy re-serialization
        // would drop unknown fields a newer agent sent.
        let seq = match &manifest.reeve {
            Some(ext) => {
                let seq = storage_seq(ext.seq)?;
                journal_insert(
                    &tx,
                    device_id,
                    seq,
                    &ext.observed_at,
                    now,
                    kind_str(JournalRecordKind::Status),
                    Some(raw_body),
                )
                .map_err(internal)?;
                Some(seq)
            }
            None => None, // vanilla Margo report (§3.2): no journal identity
        };

        let before = stored_state(&tx, device_id, deployment_id).map_err(internal)?;
        upsert_current(
            &tx,
            device_id,
            &CurrentStatus {
                deployment_id,
                state: state_str(manifest.status.state),
                seq,
                observed_at: manifest.reeve.as_ref().map(|e| e.observed_at.as_str()),
                received_at: now,
                payload: raw_body,
            },
        )
        .map_err(internal)?;

        tx.commit().map_err(internal)?;

        // §6.3 deployment-status: emitted only when the OVERALL state
        // actually changed (the max-seq rule may have kept the old
        // one). Read back what the transaction left current.
        let after = stored_state(&conn, device_id, deployment_id).map_err(internal)?;
        drop(conn);
        // A removal leaves `after` empty (the row was cleared); the state
        // that actually changed is `removed`. Map that case explicitly so
        // the UI still gets a deployment-status change for a disappearing
        // deployment (§6.3), not silence.
        if before != after
            && let Some(state) = after
                .as_deref()
                .and_then(state_from_str)
                .or_else(|| before.as_ref().map(|_| DeploymentState::Removed))
        {
            self.events.emit(SseEvent::DeploymentStatus(DeploymentStatusEvent {
                ts: EventHub::now_ts(),
                device_id: device_id.to_string(),
                deployment_id: deployment_id.to_string(),
                state,
            }));
        }
        Ok(())
    }

    fn ingest_journal(
        &self,
        device_id: &str,
        batch: &JournalBatch,
    ) -> Result<JournalAck, StatusIngestError> {
        let now = db::now_secs();
        let mut conn = self.db.lock().expect("db mutex poisoned");
        let tx = conn.transaction().map_err(internal)?;

        touch_last_seen(&tx, device_id, now).map_err(internal)?;

        // deployment_id -> state before this batch (first touch wins),
        // for §6.3 deployment-status change detection after commit.
        let mut before: BTreeMap<String, Option<String>> = BTreeMap::new();

        for record in &batch.records {
            let seq = storage_seq(record.seq)?;
            let payload = record.payload.as_ref().map(|v| v.to_string());
            journal_insert(
                &tx,
                device_id,
                seq,
                &record.observed_at,
                now,
                kind_str(record.kind),
                payload.as_deref(),
            )
            .map_err(internal)?;

            // A backfilled status record also feeds current-state
            // materialization (max-seq rule keeps late arrivals from
            // regressing anything newer). A payload that is not a
            // well-formed DeploymentStatusManifest is journaled as-is
            // but skipped here — the journal is forensic, the current
            // table is derived.
            if record.kind == JournalRecordKind::Status
                && let Some(payload_str) = &payload
                && let Ok(m) = serde_json::from_str::<DeploymentStatusManifest>(payload_str)
                && m.kind == DEPLOYMENT_STATUS_KIND
            {
                if !before.contains_key(&m.deployment_id) {
                    let prior =
                        stored_state(&tx, device_id, &m.deployment_id).map_err(internal)?;
                    before.insert(m.deployment_id.clone(), prior);
                }
                upsert_current(
                    &tx,
                    device_id,
                    &CurrentStatus {
                        deployment_id: &m.deployment_id,
                        state: state_str(m.status.state),
                        seq: Some(seq),
                        observed_at: Some(record.observed_at.as_str()),
                        received_at: now,
                        payload: payload_str,
                    },
                )
                .map_err(internal)?;
            }
        }

        tx.commit().map_err(internal)?;
        let acked = acked_seq(&conn, device_id).map_err(internal)?;

        // §6.3 deployment-status per deployment the batch touched,
        // only where the overall state actually moved.
        let mut changed: Vec<(String, DeploymentState)> = Vec::new();
        for (deployment_id, prior) in &before {
            let after = stored_state(&conn, device_id, deployment_id).map_err(internal)?;
            if after != *prior
                && let Some(state) = after
                    .as_deref()
                    .and_then(state_from_str)
                    .or_else(|| prior.as_ref().map(|_| DeploymentState::Removed))
            {
                changed.push((deployment_id.clone(), state));
            }
        }
        drop(conn);
        for (deployment_id, state) in changed {
            self.events.emit(SseEvent::DeploymentStatus(DeploymentStatusEvent {
                ts: EventHub::now_ts(),
                device_id: device_id.to_string(),
                deployment_id,
                state,
            }));
        }
        Ok(JournalAck { acked_seq: acked })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "on").unwrap();
        db::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at)
             VALUES ('dev-1', 'box', 'x86_64', '0.1.0', 0)",
            [],
        )
        .unwrap();
        conn
    }

    /// Test shorthand for [`upsert_current`].
    fn upsert(
        tx: &Transaction<'_>,
        deployment_id: &str,
        state: &str,
        seq: Option<i64>,
        observed_at: Option<&str>,
        received_at: i64,
        payload: &str,
    ) -> rusqlite::Result<()> {
        upsert_current(
            tx,
            "dev-1",
            &CurrentStatus { deployment_id, state, seq, observed_at, received_at, payload },
        )
    }

    fn insert_seqs(conn: &mut Connection, seqs: &[i64]) {
        let tx = conn.transaction().unwrap();
        for &s in seqs {
            journal_insert(&tx, "dev-1", s, "2026-07-10T00:00:00Z", 1, "lifecycle", None)
                .unwrap();
        }
        tx.commit().unwrap();
    }

    #[test]
    fn ack_is_end_of_first_contiguous_run() {
        let mut conn = test_conn();
        assert_eq!(acked_seq(&conn, "dev-1").unwrap(), 0, "empty journal acks 0");
        insert_seqs(&mut conn, &[1, 2, 3]);
        assert_eq!(acked_seq(&conn, "dev-1").unwrap(), 3);
        insert_seqs(&mut conn, &[5, 6]); // hole at 4
        assert_eq!(acked_seq(&conn, "dev-1").unwrap(), 3, "hole stops the ack");
        insert_seqs(&mut conn, &[4]); // hole filled
        assert_eq!(acked_seq(&conn, "dev-1").unwrap(), 6);
    }

    #[test]
    fn journal_insert_never_overwrites() {
        let mut conn = test_conn();
        let tx = conn.transaction().unwrap();
        assert!(journal_insert(&tx, "dev-1", 1, "T1", 10, "status", Some("first")).unwrap());
        assert!(!journal_insert(&tx, "dev-1", 1, "T2", 20, "status", Some("second")).unwrap());
        tx.commit().unwrap();
        let (obs, payload): (String, String) = conn
            .query_row(
                "SELECT observed_at, payload FROM status_journal WHERE device_id='dev-1' AND seq=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(obs, "T1", "original timestamp preserved");
        assert_eq!(payload, "first");
    }

    #[test]
    fn current_state_is_max_seq_not_max_arrival() {
        let mut conn = test_conn();
        let tx = conn.transaction().unwrap();
        upsert(&tx, "dep-1", "installed", Some(10), Some("T10"), 100, "p10")
            .unwrap();
        // Late arrival with a LOWER seq must not regress.
        upsert(&tx, "dep-1", "installing", Some(5), Some("T5"), 200, "p5")
            .unwrap();
        tx.commit().unwrap();
        let (state, seq): (String, i64) = conn
            .query_row(
                "SELECT state, seq FROM deployment_status_current
                 WHERE device_id='dev-1' AND deployment_id='dep-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), seq), ("installed", 10));
    }

    #[test]
    fn unseqd_report_never_regresses_seqd_state() {
        let mut conn = test_conn();
        let tx = conn.transaction().unwrap();
        upsert(&tx, "dep-1", "installed", Some(10), Some("T10"), 100, "p10")
            .unwrap();
        upsert(&tx, "dep-1", "failed", None, None, 200, "vanilla").unwrap();
        tx.commit().unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM deployment_status_current WHERE deployment_id='dep-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "installed");
    }

    /// Stored current state, if any (test probe).
    fn current(conn: &Connection, deployment_id: &str) -> Option<String> {
        stored_state(conn, "dev-1", deployment_id).unwrap()
    }

    #[test]
    fn terminal_removed_clears_the_current_row() {
        // REV-010 §11.4: a device that removes an app reports terminal
        // `removed`; the current row must DISAPPEAR, not linger at "removed".
        let mut conn = test_conn();
        let tx = conn.transaction().unwrap();
        upsert(&tx, "dep-1", "installed", Some(10), Some("T10"), 100, "p10").unwrap();
        // A later terminal removal clears it.
        upsert(&tx, "dep-1", "removed", Some(11), Some("T11"), 200, "gone").unwrap();
        tx.commit().unwrap();
        assert_eq!(current(&conn, "dep-1"), None, "removed row cleared from current");
    }

    #[test]
    fn stale_removed_does_not_clear_newer_state() {
        // The seq-precedence rule also guards the clear: an out-of-order
        // `removed` (lower seq) must not wipe a newer installed state.
        let mut conn = test_conn();
        let tx = conn.transaction().unwrap();
        upsert(&tx, "dep-1", "installed", Some(10), Some("T10"), 100, "p10").unwrap();
        upsert(&tx, "dep-1", "removed", Some(5), Some("T5"), 200, "stale-gone").unwrap();
        tx.commit().unwrap();
        assert_eq!(
            current(&conn, "dep-1").as_deref(),
            Some("installed"),
            "stale removed left the newer state intact"
        );
    }

    #[test]
    fn removed_for_untracked_deployment_is_a_noop() {
        // A `removed` for a (device, deployment) we never held clears
        // nothing and does not error (0 rows).
        let mut conn = test_conn();
        let tx = conn.transaction().unwrap();
        upsert(&tx, "never", "removed", Some(1), Some("T1"), 100, "gone").unwrap();
        tx.commit().unwrap();
        assert_eq!(current(&conn, "never"), None);
    }

    #[test]
    fn unseqd_reports_apply_in_arrival_order_while_unseqd() {
        let mut conn = test_conn();
        let tx = conn.transaction().unwrap();
        upsert(&tx, "dep-1", "installing", None, None, 100, "a").unwrap();
        upsert(&tx, "dep-1", "installed", None, None, 200, "b").unwrap();
        tx.commit().unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM deployment_status_current WHERE deployment_id='dep-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "installed");
    }
}
