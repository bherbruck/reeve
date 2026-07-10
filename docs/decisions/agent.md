# reeve decisions — Agent (D4, D5)

Part of docs/decisions/; start at [00-INDEX.md](00-INDEX.md).

## D4. Enrollment ceremony
- Operator creates a join token in the UI/API: TTL + max-uses
  (default: 24h, 1 use). Token is random, stored hashed.
- `reeve-agent install --server https://reeve.example --token <JT>`:
  1. POST /api/reeve/v1/enroll { join_token, hostname, arch,
     agent_version } — a reeve surface (spec/reeve/01-framework.md §3.1 rule 4; never under
     Margo's /api/v1/). Margo's POST /api/v1/onboarding is NOT
     served; that replacement is recorded in D1 and spec/reeve/01-framework.md §3.8.
  2. Server: validates token, creates device row + initial desired
     state, issues device_id + device token. Atomic (one SQLite tx —
     the revision store lives in the same DB, D13). This ONE token
     is the device's credential for API, manifest poll, /v2 pulls,
     websocket, and secrets resolve (D1).
  3. Agent writes /etc/reeve-agent/agent.toml (0600, temp+rename),
     installs its systemd unit, starts.
- Idempotent re-run: same join token before expiry + same hostname
  returns the SAME device (no duplicate rows from a retried install).
- Re-enrollment after a wipe: operator issues a re-enroll token bound
  to the existing device_id; a fresh box resumes the old identity and
  desired state. Plain join token on a wiped box = new device, old
  one flagged stale in UI.

## D5. Compose provider semantics
- v1 shells out to `docker compose` v2. Boring, correct, debuggable
  by anyone ("we run exactly what you'd type"). Docker API later
  only if a named need appears.
- Project name = app dir name. Apply = `docker compose -f
  apps/<name>/compose.yml -p <name> up -d --remove-orphans`.
- Diff = content hash per app dir (recorded in agent.db) vs unpacked
  bundle. Hash unchanged => skip (no-op convergence is silent).
  Secrets are diffed separately by secrets_version (D15): bundle
  digest unchanged + secrets_version changed => re-resolve and
  rewrite env files only, `up -d` affected apps, no bundle re-pull.
- Removal: app dir gone from bundle => `docker compose -p <name> down`
  using the RETAINED last-applied copy in /var/lib/reeve-agent/
  applied/ (you can't down a stack whose file you deleted first).
  Down succeeds => applied copy removed. Order: down before delete.
- Post-apply status: `docker compose ps --format json` mapped to
  Margo deployment states; recorded in journal, reported per REV-004.
- Crash-only: convergence is resumable from any kill point — the
  journal records intent before action (app, hash, phase), and
  startup re-runs any incomplete phase. `up -d` and `down` are
  idempotent, which is why shelling out is safe.
- Journal phases (the crash-safety contract, per app): planned(app,
  hash) -> applying -> applied | failed; removal: removing ->
  removed. Terminal phases: applied, removed, failed. Startup
  re-runs any row not in a terminal phase; re-running any phase is
  a no-op when its postcondition already holds.

