-- V9: federation (C10; spec/reeve/06-federation.md REV-005,
-- docs/decisions/secrets.md D15 §federation). Schema law (storage.md
-- D16): explicit PRIMARY KEY on every table; timestamps integer unix
-- seconds. Like V6/V7/V8, these tables exist regardless of the
-- ext-federation feature: the CORE write gate (tree.rs) consults
-- tier_tokens so a --no-default-features binary still refuses writes
-- to layers delegated to a child tier (§8.4 single writer), and the
-- core render pipeline reads the new columns.

-- Tier credentials (§8.7): issued by an admin like join tokens, stored
-- hashed. Scope is enforced SERVER-SIDE on every sync/backfill call:
--   site          — the child gateway's site label. The child owns
--                   layers/<NN>-site.<site>; this tier therefore
--                   refuses its own writes there (§8.4: the root does
--                   not edit site layers owned by gateways), serves
--                   site.<site>-scoped secrets down (10-secrets
--                   §12.5), and accepts journal backfill only for
--                   devices this token forwarded.
--   sync_prefixes — JSON array of tree-path prefixes the child may
--                   sync (hub-owned layers + vendored packages);
--                   revision manifests and blobs are filtered to it.
CREATE TABLE tier_tokens (
    token_hash    TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    site          TEXT NOT NULL,
    sync_prefixes TEXT NOT NULL,
    created_by    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER,
    revoked_at    INTEGER
);

-- Status forwarding cursor at the CHILD (§8.3): highest journal seq
-- already delivered upstream, per device. The status_journal itself is
-- the outage buffer (bounded, gap-marked like any journal); reconnect
-- backfills everything past this cursor.
CREATE TABLE federation_forward (
    device_id     TEXT PRIMARY KEY REFERENCES devices(device_id) ON DELETE CASCADE,
    forwarded_seq INTEGER NOT NULL DEFAULT 0
);

-- Devices that appeared via forwarded ingest (§8.3): tier_origin is
-- the forwarding tier-token name ('airgap' for sneakernet status
-- imports); NULL = locally enrolled. Forwarded devices are visible in
-- status/journal surfaces but are NEVER rendered or served desired
-- state here — they converge against their own tier (§8.6).
ALTER TABLE devices ADD COLUMN tier_origin TEXT;

-- Scoped secret sync (10-secrets §12.5): rows pulled down from the
-- upstream tier carry origin 'upstream' (re-encrypted under THIS
-- tier's keyfile); NULL = authored locally. Prune-on-sync removes
-- upstream rows the parent no longer serves, never local ones.
ALTER TABLE secrets ADD COLUMN origin TEXT;

-- Two-stream render bookkeeping (§8.2: render input = latest synced
-- upstream revision + latest local revision + device context): the
-- upstream-stream LOCAL ROW id this device's manifest was rendered
-- against; NULL when no upstream revision existed. Compared alongside
-- rendered_revision for the no-op fast path.
ALTER TABLE device_manifests ADD COLUMN rendered_upstream INTEGER;
