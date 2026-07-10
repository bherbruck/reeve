-- V4: status ingest + journal (C5; spec/reeve/05-health-journal.md §7.3).
-- Schema law (storage.md D16): explicit PRIMARY KEY on every table.

-- Append-only, forensic device journal (§7.3 "late ingest"):
-- idempotency key is the PK (device_id, seq) — INSERT OR IGNORE makes
-- backfill resends harmless (Law 3) and MUST NOT overwrite an
-- already-ingested record. observed_at is the device-assigned original
-- RFC 3339 timestamp, preserved verbatim; received_at is server-receipt
-- time (unix seconds) — the pair makes tampering and skew visible.
-- kind mirrors reeve-types JournalRecordKind; gap marks carry no payload.
CREATE TABLE status_journal (
    device_id   TEXT    NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    observed_at TEXT    NOT NULL,
    received_at INTEGER NOT NULL,
    kind        TEXT    NOT NULL CHECK (kind IN ('status', 'health', 'lifecycle', 'gap')),
    payload     TEXT,
    PRIMARY KEY (device_id, seq)
);

-- Latest state per (device, deployment) for the UI — materialized from
-- the highest journaled seq, NOT the latest arrival: a late backfilled
-- record never regresses current state. seq/observed_at are NULL for
-- reports from vanilla Margo agents (no reeve extension object —
-- spec/reeve/01-framework.md §3.2 degradation).
CREATE TABLE deployment_status_current (
    device_id     TEXT    NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
    deployment_id TEXT    NOT NULL,
    state         TEXT    NOT NULL,
    seq           INTEGER,
    observed_at   TEXT,
    received_at   INTEGER NOT NULL,
    payload       TEXT    NOT NULL,
    PRIMARY KEY (device_id, deployment_id)
);
