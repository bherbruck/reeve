-- V3: render pipeline + artifact serving (C4;
-- spec/reeve/08-packaging.md §10.2, docs/decisions/delivery.md D7/D13,
-- docs/decisions/tree-render.md D2/D3). Schema law (storage.md D16):
-- explicit PRIMARY KEY on every table; timestamps integer unix seconds.

-- Typed-by-convention key/value settings (Law 4: settings in the DB).
-- Keys used by the render pipeline:
--   server_epoch          — high 16 bits of every manifestVersion
--                           (spec/reeve/08-packaging.md §10.2); absent
--                           means 0; durability restore fencing (C6,
--                           spec/reeve/07-durability.md §9.5) bumps it.
--   last_rendered_local   — revision id of the local-stream head the
--                           last full render pass completed against
--                           (crash-only reconcile: a revision committed
--                           but un-rendered at kill time is detected and
--                           rendered at startup).
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- One row per device: its current State Manifest and the render
-- bookkeeping behind it (docs/decisions/delivery.md D13).
--   manifest_version  — the packed (epoch, counter) u64, bit-cast to
--                       INTEGER; bumped ONLY when the rendered content
--                       actually changes (D3 no-change-no-bump).
--   counter           — low 48 bits, monotonic per device.
--   generation        — D2 render generation counter (manifest.yaml
--                       provenance), incremented per material render.
--   content_digest    — sha256:<hex> over the rendered apps/** file set
--                       EXCLUDING manifest.yaml: the change detector.
--                       Provenance-only input changes (a revision that
--                       does not alter this device's apps) therefore
--                       produce no new bundle and no bump (D3).
--   bundle_digest     — digest of the OCI image manifest JSON the
--                       device pulls (StateManifest.bundle.digest);
--                       NULL when the device has zero apps (bundle is
--                       served as JSON null per Margo's
--                       DeploymentBundleRef rule).
--   layer_digest      — digest of the tar.gz render-bundle layer blob.
--   manifest_json     — the exact StateManifest bytes served by
--                       GET /api/reeve/v1/manifest.
--   etag              — sha256:<hex> of manifest_json (RFC 9110 strong
--                       validator, §10.2).
--   rendered_revision — local-stream revision id this device was last
--                       rendered (or verified unchanged) against.
CREATE TABLE device_manifests (
    device_id         TEXT PRIMARY KEY
                          REFERENCES devices(device_id) ON DELETE CASCADE,
    manifest_version  INTEGER NOT NULL,
    counter           INTEGER NOT NULL,
    generation        INTEGER NOT NULL,
    content_digest    TEXT NOT NULL,
    bundle_digest     TEXT,
    layer_digest      TEXT,
    manifest_json     TEXT NOT NULL,
    etag              TEXT NOT NULL,
    rendered_revision INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

-- Content-addressed OCI artifact blobs served on the native read-only
-- /v2 routes (docs/decisions/delivery.md D7): render-bundle layers
-- (tar.gz), OCI image manifests, and the shared empty config blob.
-- DECISION: a dedicated table rather than reusing revision-store blobs —
-- render artifacts are derived state with their own lifecycle (replaced
-- per render, purged when unreferenced at startup), while revision-store
-- blobs are authored history and append-only.
CREATE TABLE bundle_blobs (
    digest     TEXT PRIMARY KEY,
    content    BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
