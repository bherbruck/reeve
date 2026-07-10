## D6. Migrations & storage plumbing (mostly restating law as picks)
- refinery with embedded migrations; run at every startup, both
  binaries. Schema version asserted by verify-restore (REV-006).
- SQLite open mode everywhere: WAL, foreign_keys ON, busy_timeout
  5s, single writer connection + read pool.
- Every file write anywhere in the codebase: temp file, fsync,
  rename. No exceptions. Uploads: temp key, finalize (REV-006).
- SCHEMA LAW (D16): every tracked table MUST have an explicit
  PRIMARY KEY — universal, no rowid-only tables (session-extension
  requirement made a house rule; enforce in review).
- SCHEMA LAW (D16): a schema migration MUST immediately cut a new
  snapshot generation — changesets do not capture schema changes.
  Bootstrap sequence: migrate -> if migrated, snapshot -> resume
  changeset streaming. A changeset sequence never spans a schema
  version.

## D16. In-binary changeset streaming — litestream removed
DECIDED: litestream is REMOVED from the design entirely. Seconds-RPO
is provided in-binary via SQLite's session extension (trunk SQLite,
rusqlite `session` feature).
- Two durability tiers, both in-binary, one pipeline (SPEC §9):
  1. SNAPSHOT tier (the generation anchor, ships first): VACUUM INTO
     on interval, AEAD-encrypted under the D15 external keyfile,
     atomic upload (temp key + finalize), retention window.
  2. CHANGESET tier (seconds-RPO, fast-follow): the single writer
     connection (D6 — exactly what session capture requires) carries
     an attached session. Every N seconds or M commits (config, e.g.
     5s/100): extract changeset, compress, AEAD-encrypt under the
     same keyfile, upload with a strictly sequenced key chained to
     the current snapshot generation (generation id + monotonic
     seq). Atomic-or-absent; empty changeset => no upload.
- Restore = snapshot generation + changesets applied in sequence via
  changeset_apply. Conflicts are structurally impossible (own
  lineage onto own snapshot) — any reported conflict is CORRUPTION:
  abort loudly, never auto-resolve. Point-in-time = replay to seq K
  (`--to-seq` / `--to-time`; upload timestamps are the coarse
  index).
- verify-restore proves the WHOLE chain (snapshot + all changesets +
  schema + recency of last applied seq). One restore procedure for
  everything (SPEC §9.4).
- Crash-only: an unflushed in-memory session lost to kill -9 costs
  at most the configured interval (that IS the RPO); startup resumes
  from the last uploaded sequence. No session state outside the DB
  and the object store.
- Schema laws this adds live in D6 (universal explicit PRIMARY KEY;
  migration cuts a new generation).
- RATIONALE: the session extension is SQLite's own change-log
  primitive — capture is trunk code (Law 4: someone else's test
  suite); reeve writes only the shell (extract, encrypt, upload,
  replay). Litestream, post-D15, was the sole remaining plaintext
  escape (WAL frames to the object store outside our keyfile), the
  only foreign process on the durability path, and a second restore
  procedure verify-restore didn't govern. Changesets also replay
  TRANSACTIONS (logical row changes), so point-in-time restore lands
  on a transaction boundary of our own schema — coherent state, not
  whatever pages had flushed. The Durability trait seam (SPEC §9.1)
  is what makes this reversible if the changeset tier disappoints on
  the bench, and where a future engine-native CDC could slot.
