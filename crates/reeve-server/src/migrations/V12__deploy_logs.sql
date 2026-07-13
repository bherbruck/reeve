-- V12: deploy-log storage (REV-011, server `ext-logs`).
-- Full `docker compose up`/`down` output stored per deployment so an
-- operator can see WHY a deployment failed beyond the one-line Margo
-- `status.error`. Schema law (storage.md D16): explicit PRIMARY KEY on
-- every table; timestamps integer unix seconds unless preserving a
-- device-supplied RFC 3339 value verbatim.
--
-- Schema exists regardless of the ext-logs feature (same rule as
-- V6/V7/V8): stable across feature sets, so a --no-default-features
-- binary can still restore/verify a target written by a full one
-- (spec/reeve/07-durability.md §9.4). The feature only gates the
-- LogStore module, its routes, and its wiring into AppState.

-- Content-addressed log bodies, dedicated to deploy logs (NOT the
-- revision-store `blobs` table): keeping them separate means the render
-- bundle GC never touches log bodies and vice versa. Digest is the D13
-- grammar `sha256:<hex>` (revision_store::digest_of). Identical output
-- across runs/devices dedupes to one row.
CREATE TABLE deploy_log_blobs (
    digest  TEXT PRIMARY KEY,
    content BLOB NOT NULL
);

-- One row per uploaded log run. blob_digest points at the body above;
-- retention prunes older rows per (device, deployment) on insert and
-- garbage-collects unreferenced blobs (LogStore::put, one transaction).
--   log_id      — opaque server-assigned handle (random hex), the PK
--                 and the GET-one route parameter.
--   outcome     — applied | failed | removed (reeve-types DeployLogOutcome).
--   phase       — up | down (reeve-types DeployLogPhase).
--   exit_code   — process exit code when known; NULL otherwise.
--   truncated   — 1 if the agent clipped the body before upload.
--   captured_at — device-assigned RFC 3339 timestamp, preserved verbatim.
--   received_at — server-receipt time (unix seconds); orders retention.
CREATE TABLE deploy_logs (
    log_id        TEXT    NOT NULL PRIMARY KEY,
    device_id     TEXT    NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
    deployment_id TEXT    NOT NULL,
    app_id        TEXT    NOT NULL,
    outcome       TEXT    NOT NULL,
    phase         TEXT    NOT NULL,
    exit_code     INTEGER,
    blob_digest   TEXT    NOT NULL REFERENCES deploy_log_blobs(digest),
    size_bytes    INTEGER NOT NULL,
    truncated     INTEGER NOT NULL,
    captured_at   TEXT    NOT NULL,
    received_at   INTEGER NOT NULL
);

-- Newest-first listing + retention pruning are both keyed by
-- (device, deployment) ordered on received_at.
CREATE INDEX deploy_logs_dev_deploy_idx
    ON deploy_logs (device_id, deployment_id, received_at DESC);
