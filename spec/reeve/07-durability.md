# reeve spec — Durability & Restore Verification (REV-006)

Part of the reeve specification; start at [00-INDEX.md](00-INDEX.md).

## 9. Durability & Restore Verification (REV-006)

How reeve-server's state survives the loss of the machine it runs
on. ALL server state — tree history (revision store), enrollment,
settings, audit, journal ingest, rollout state — is ONE SQLite
database (docs/decisions/delivery.md D13), made durable ENTIRELY IN-BINARY
(docs/decisions/storage.md D16): a snapshot tier (minutes RPO, the generation
anchor) plus an optional changeset tier (seconds RPO, SQLite's
session extension), shipping to the same S3-compatible target
through one pipeline, proven by one mandatory verify-restore loop.
Zero durability sidecars exist. Disaster recovery is normal startup
with one precondition removed.

Law 4 covers process crashes; this section covers disk loss — and
the gap where most backup schemes actually fail: backups that were
never restore-tested. A backup is trustworthy only if
restore-tested; Section 9.4 makes that a MUST, continuously, with
the result on the dashboard.

Terms: **snapshot** — a consistent point-in-time copy of the SQLite
database via `VACUUM INTO`; **generation** — one snapshot plus the
changeset sequence chained to it; **changeset** — the SQLite
session extension's logical record of committed row changes;
**target** — the configured object-store destination; **RPO** —
maximum tolerated data-loss window; **verify-restore** —
downloading a generation, replaying it fully, and asserting the
result is a usable database.

### 9.1 State model

- **Everything is the one SQLite database**, WAL mode, single
  writer: the tree revision store (blobs + revisions, D13),
  enrollment, settings, secrets ciphertext (10-secrets §12), audit and
  terminal session records (03-terminal §5.4), status journal ingest (05-health-journal §7),
  rollout state (09-rollouts §11). The former parallel git-mirror/bundle
  durability path is DELETED — revision sync (06-federation §8.2) still gives
  every downstream tier a warm copy of the layers in its scope as a
  side effect, but it is no longer a durability mechanism this
  section depends on or maintains.
- Render bundles and other derived artifacts need no backup: they
  are reproducible from revisions (render is pure, D3) and
  re-materialize on demand.
- The secrets master key lives in a FILE OUTSIDE the DB
  (REEVE_DATA/secret.key — 10-secrets §12.2): snapshots ship ciphertext only,
  and restore therefore needs snapshot + keyfile, two artifacts
  from two places (§9.6).
- SQLite TRUNK ONLY — no forks (no libsql, no bedrock, no patched
  builds). The session extension used by §9.3 IS trunk SQLite
  (rusqlite `session` feature): capture is the engine's own
  change-log primitive with someone else's test suite (Law 4); what
  reeve writes is only the shell — extract, encrypt, upload,
  replay. No crate other than what this section defines MAY contain
  replication, backup, or restore logic. The seam for all of it is
  one `Durability` trait in one module of reeve-server — tiers
  `none` | `snapshot` | `snapshot+changeset` are config, not
  surgery; the seam is what makes the changeset tier reversible if
  it disappoints on the bench, and where a future engine-native CDC
  could slot.

### 9.2 Snapshot tier (the generation anchor — ships first)

- Every N minutes (config `durability.snapshot.interval`,
  RECOMMENDED default 15 min), produce a consistent snapshot via
  `VACUUM INTO` a temp path — safe under WAL with the writer live;
  no lock ceremony, no stop-the-world.
- Snapshots are AEAD-encrypted under the D15 external keyfile
  before upload (the keyfile already exists for secrets; one key
  custody story for everything shipped off-box).
- Upload via the `object_store` crate to an S3-compatible target:
  AWS S3, rustfs, MinIO, or a local filesystem path for air-gapped
  tiers (`durability.target.url`). One crate, four targets, zero
  bespoke transports.
- Upload MUST be atomic-or-absent: write to a temporary key (or
  multipart upload), then finalize to the well-known latest name
  (e.g. `reeve/<instance>/gen/<rfc3339>-<schema>.db` plus a
  `latest` pointer written last). A process killed at ANY byte of
  upload MUST NOT leave a corrupt or partial object where a restore
  would find it (Law 3 extended to the bucket).
- Each uploaded snapshot opens a new GENERATION; the changeset
  sequence (§9.3) chains to the current generation id.
- Retention: a configurable window (`durability.snapshot.retain`,
  RECOMMENDED default 7 days plus a minimum of 8 generations).
  Pruning removes whole generations (snapshot + its changesets),
  runs after successful upload, and MUST never prune the last
  known-verified generation (§9.4).
- Snapshot failure (produce, encrypt, upload, prune) is surfaced,
  not fatal: log, mark durability degraded in API/UI, retry next
  interval.

### 9.3 Changeset tier (seconds-RPO, in-binary — fast-follow)

Replaces the former litestream sidecar option (docs/decisions/storage.md D16
records why: post-D15, WAL-frame replication was the sole remaining
plaintext escape past the keyfile, the only foreign process on the
durability path, and a second restore procedure verify-restore
didn't govern).

- The single writer connection (docs/decisions/storage.md D6 — exactly what
  session capture requires) carries an attached session from the
  trunk SQLite session extension. Every N seconds or M commits
  (config `durability.changeset.interval` /
  `durability.changeset.commits`, RECOMMENDED defaults 5 s / 100),
  extract the changeset, compress, AEAD-encrypt under the same D15
  keyfile, and upload with a strictly sequenced key chained to the
  current snapshot generation (generation id + monotonic seq).
  Upload atomic-or-absent (§9.2 rules). An empty changeset produces
  NO upload.
- Changesets are LOGICAL, committed row changes — replay lands on a
  transaction boundary of reeve's own schema: coherent state, not
  whatever pages had flushed.
- Restore = chosen generation's snapshot + apply its changesets in
  sequence order via `changeset_apply`. Conflicts are structurally
  impossible (replaying own lineage onto own snapshot) — any
  conflict reported by `changeset_apply` is CORRUPTION and MUST
  abort the restore loudly; never auto-resolve.
- Point-in-time restore: replay up to sequence K — surfaced as
  `--to-seq` / `--to-time` (sequence upload timestamps are the
  coarse time index).
- Crash-only: an unflushed in-memory session lost to `kill -9`
  costs at most the configured interval — that IS the RPO. Startup
  resumes capture from the last uploaded sequence; no session state
  persists outside the DB and the object store (Law 3).
- A schema migration MUST immediately cut a new generation
  (changesets do not capture schema changes): bootstrap sequence is
  migrate → if migrated, snapshot → resume streaming (docs/decisions/storage.md
  D6). A changeset sequence never spans a schema version.
- The server MAY publish upload lag (age of the last uploaded
  sequence) as a `durability-lag` event (04-status-stream §6.3).

### 9.4 verify-restore (MUST)

- reeve-server MUST provide `reeve-server verify-restore` as a
  subcommand AND as a scheduled internal task
  (`durability.verify.interval`, RECOMMENDED default 24 h).
- A run MUST prove the WHOLE chain: download the latest
  generation's snapshot, decrypt, apply ALL its changesets in
  sequence order, open the result as SQLite (integrity check),
  assert the schema version is known to this binary, assert recency
  (last applied sequence age ≤ 2× the relevant interval, config),
  assert the restore-fencing epoch marker is present and readable
  at the target (§9.5), and record the result (when, which
  generation, last sequence, outcome, failure detail) in the live
  DB. One restore procedure for everything — there is no second
  path to rot.
- The result MUST be surfaced in the API and UI as "last verified
  restore: <when>", and published as a `verify-restore` event
  (04-status-stream §6.3). An unverified or stale-verified target is an
  operator-visible warning state.
- A deployment whose verify-restore has never succeeded MUST be
  treated (in UI/API status) as having NO durability tier, whatever
  the bucket contains.

### 9.5 Crash-only bootstrap, DR, and data value

- reeve-server starting with NO local database and a configured
  snapshot target MUST offer restore-from-latest as the startup
  path: fetch the latest generation, decrypt, replay changesets
  (§9.3), place the result as the local DB, run migrations
  idempotently (08-packaging §10.1), continue as a normal start.
  Whether restore is automatic or requires a confirmation flag
  (`--restore-from-target`) is an implementation choice, but the
  path MUST exist and MUST be the documented DR procedure. Disaster
  recovery is therefore normal startup with one precondition
  removed — no runbook of special cases, no restore mode that rots
  untested. Tree history restores WITH the snapshot (it is in the
  same DB); render bundles re-materialize on demand.
- Secrets restore requires the keyfile too (§9.1, 10-secrets §12.2) — the DR
  procedure MUST state both artifacts.
- Data loss on restore is bounded by the tier's RPO — snapshot
  interval (minutes) on the snapshot tier, changeset interval
  (seconds) with the changeset tier enabled; agents' journals
  re-backfill everything journaled since the restore point (05-health-journal §7.3) —
  agent-side store-and-forward is itself part of the durability
  story.
- **Restore fencing (normative):** restore-from-snapshot can
  resurrect manifest state older than what devices have already
  seen inside the RPO window; without fencing, 08-packaging §10.2's strict
  monotonicity would make affected devices reject the restored
  server as a rollback attacker. Therefore manifestVersion is the
  pair `(epoch, counter)`, compared lexicographically (08-packaging §10.2 defines
  the wire encoding), and a tiny epoch marker lives AT THE SNAPSHOT
  TARGET (not in the DB). The epoch is PER-TIER: each tier fences
  against its own snapshot target.
  - Restore ordering: increment the epoch marker at the target
    FIRST, then serve. A crash between increment and serve
    double-increments harmlessly; epoch REUSE is forbidden — a
    restored server MUST NOT serve under an epoch it has not
    freshly incremented.
  - verify-restore (§9.4) MUST assert the epoch marker is present
    and readable at the snapshot target.
  - Devices treat an epoch bump as a loggable notable event; a
    counter regression WITHIN an epoch remains a security event
    (08-packaging §10.2).

Data-value analysis — what the DB holds, split by fate on loss:

| Data | On loss | Why |
|------|---------|-----|
| Status journal ingest (05-health-journal §7) | **Reconstructible** | agents re-backfill from device journals with original timestamps; history converges again (bounded by agent retention) |
| Device capabilities cache | **Reconstructible** | devices MUST re-send on change per Margo; next report repopulates |
| Tree history (revision store, D13) | **IRREPLACEABLE** | the config source of truth and its full attributable history; now snapshot-covered like everything else |
| Render bundles / derived artifacts | **Reconstructible** | re-rendered from revisions (pure render, D3) |
| Presence / derived health | **Reconstructible** | recomputed from journal + channel state |
| Enrollment (device identity ↔ credential, 01-framework §3.8) | **IRREPLACEABLE** | losing it orphans every device; re-enrollment is manual, fleet-wide toil |
| Settings | **IRREPLACEABLE** | operator intent, recorded nowhere else (files hold shape, DB holds values) |
| Secrets (ciphertext, 10-secrets §12) | **IRREPLACEABLE** (with keyfile) | operator-entered values; ciphertext useless without REEVE_DATA/secret.key — back both up |
| Audit + terminal session records (03-terminal §5.4) | **IRREPLACEABLE** | forensic record; by definition cannot be regenerated |
| Rollout state/history (09-rollouts §11) | **IRREPLACEABLE in flight** | a lost in-flight rollout's position must not be guessed; history is audit-like |

Default RPO justification: the irreplaceable set changes at human
cadence (enrollments, settings edits, terminal sessions, rollout
steps) — minutes-scale RPO loses at most a few human actions, which
are visible and repeatable by the humans who took them. The
high-frequency data is exactly the reconstructible set. Hence the
15-minute default is sound for the snapshot tier; deployments where
losing even one enrollment or audit record is unacceptable enable
the changeset tier (§9.3) for seconds RPO — same binary, same
target, same verify-restore.

### 9.6 Security

- Snapshots contain the whole irreplaceable set — tree history,
  enrollment credential bindings, settings, audit trail, secrets
  CIPHERTEXT. Secret plaintext cannot leak via snapshots by
  construction: the master key lives outside the DB (10-secrets §12.2), so a
  stolen snapshot without the keyfile yields no secret values.
  `reeve-server init` MUST warn that the keyfile needs separate
  backup. Snapshots AND changesets are AEAD-encrypted under that
  keyfile before upload (§9.2, §9.3) — nothing reaches the target
  in plaintext, and there is no foreign process on the durability
  path to leak around the keyfile (D16). The target MUST still be
  treated as sensitive as the server itself: private bucket, scoped
  credentials (write for shipping, read for verify/restore, delete
  only for pruning).
- verify-restore MUST replay generations read-only, in a temp
  location, and clean up — never against the live DB path.
- Terminal audit records are metadata only (03-terminal §5.5 keeps content out
  of the DB), so snapshots cannot leak session content by
  construction.
- Pruning is destructive; credentials that can prune MUST NOT be
  able to rewrite existing objects (versioned or write-once-keyed
  layout RECOMMENDED) so a compromised server cannot silently
  corrupt history it already shipped.

