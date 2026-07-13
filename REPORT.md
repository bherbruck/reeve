# reeve — build report

Autonomous build of the reeve fleet desired-state manager
(server + agent + UI). This report is the flight recorder: per-track
state, test counts, the end-to-end evidence, and the exact commands to
run the full stack on this machine.

Status: **all tracks A–E complete and green, plus the operator fleet
model + fleet→site containment (REV-010) and per-deployment compose-log
capture (REV-011, `ext-logs`).**
`cargo test --workspace` = **594 passed, 0 failed**. Clippy clean
(default AND `--no-default-features`), `just standalone` green, UI
builds, generated API client in sync (regeneration idempotent),
conformance (core-only, `ext-logs` compiled out) e2e passes.
`BLOCKERS.md` is empty.

---

## Gate results (Track E3)

| Gate | Command | Result |
|---|---|---|
| Workspace tests | `cargo test --workspace` | **594 passed, 0 failed** (incl. REV-011 `ext-logs`: server `ext::logs` units + `logs_flow.rs` 5 integration, agent `ext::logs` + provider-capture units, reeve-types `logs` round-trips, and `e2e/tests/deploy_logs.rs` 3 end-to-end) |
| Every crate stands alone (Law 2) | `just standalone` | pass (7 crates build alone; `e2e` also builds alone) |
| Lint | `cargo clippy --workspace --all-targets` | **clean** — 0 warnings (also clean `--no-default-features` on `reeve-server`/`reeve-agent`/`e2e`) |
| UI build | `cd ui && npm run build` | pass (`✓ built`, dist emitted) |
| API drift (D10) | `just check-api-drift` | `gen-api` is idempotent; the gate's `git diff` flags ONLY the new deploy-log surface — three paths (`POST /api/reeve/v1/devices/{id}/logs`, `GET /api/devices/{id}/logs`, `GET .../logs/{log_id}`) + their models — because those generated files are uncommitted, not a real drift. |
| Conformance (E2) | `just conformance` (server+agent `--no-default-features` + `cargo test -p e2e --no-default-features`) | pass — core loop runs with EVERY extension (incl. `ext-logs`) compiled out; `deploy_logs.rs` is `#![cfg(feature="ext")]` so it is not compiled there, proving additivity |

Clippy note: no warnings at all in this run (the only ever-permitted
one is the `ui/dist` build-script notice, which did not fire here).

---

## Fleet model (REV-010)

The operator-facing layer over the storage engine, specified in
`spec/reeve/11-fleet-model.md`. The engine underneath is unchanged
(content-addressed revisions + overlay-layer merge); REV-010 renames
the tiers to a fixed hierarchy, adds the device-management write paths,
and mandates the UI present **intent**, never the storage plumbing
("tree", "revision", "layer", "blame", numbered paths never surface).

**The hierarchy.** Config merges top-down, deepest wins:
`All devices → Fleet → Site → Type → Device`. A device's layer chain is
built server-side from its device row (`00-all` + assigned
`10-fleet.<f>` / `20-site.<s>` / `30-type.<t>` + `40-device.<id>`),
never from tree content (`render.rs::DeviceRender::layer_chain`).

**Now fully manageable from the web (no terminal, no raw API):**

| Capability | API | UI |
|---|---|---|
| Browse the hierarchy (drill Fleet → Site → Type → Device) | `GET /api/devices` | `routes/_app/fleet/index.tsx` |
| Rename a device (display name, distinct from `device_id`) | `PATCH /api/devices/{id}` `{displayName}` | device detail |
| Move a device between groups (re-renders on change) | `PATCH …/{id}` `{fleet,site,type}` (null clears) | device detail |
| Tag a device (ad-hoc grouping/cohorts, never config) | `PATCH …/{id}` `{tags}` | device detail |
| Pin (hold current config, skip new deploys/rollouts) | `PATCH …/{id}` `{pinned}` | device detail |
| Decommission (revoke credential + tombstone, idempotent) | `POST …/{id}/decommission` | device detail |
| Deploy/undeploy a stack to a **scope** (never a layer path) | `POST /api/deploy` / `/api/undeploy` `{stack, scope}` | `routes/_app/deploy/index.tsx` |
| History (who/what/when) with one-click **Undo** | `GET /api/history`, `POST /api/history/{id}/undo` | `routes/_app/history/index.tsx` |
| Rollout by scope + optional tag cohort, waves + gates | rollout routes (REV-008) | `routes/_app/rollouts/*` |
| Enrollment with optional pre-assign (fleet/site/type/tags) | `POST /api/join-tokens` `{fleet?,site?,type?,tags?}` | `routes/_app/enrollment/new.tsx` |
| Server tier (Root hub vs Site gateway) | `GET /api/server` (`REEVE_TIER`) | `routes/_app/ops/index.tsx` |

`scope` is `{kind:"all"}` / `{kind:"fleet"|"site"|"type", name}` /
`{kind:"devices", ids:[…]}`; the operator sees "deploy hello to Site
plant-a", the store sees one authoring commit into
`layers/20-site.plant-a/apps/hello/app.yaml`. Deploy re-renders only
the devices whose merged content actually moved.

**UI plumbing removed.** The legacy raw layer/revision/blame editor
routes (`ui/src/routes/_app/tree/`) were deleted — they exposed exactly
the storage plumbing §11.5 forbids. Deploy and the package catalog keep
the shared `src/lib/tree.ts` infra; nothing user-visible references a
revision or numbered layer (visible-copy jargon sweep is clean).

**Coverage.** `crates/reeve-server/tests/fleet_ops_flow.rs` drives
deploy-to-scope + History/Undo over the real router (deploy authors the
right layer and re-renders only the scope's devices; undeploy removes
it; History carries human summaries; Undo restores prior content as a
NEW change). `devices_flow.rs` covers PATCH assignment/rename/pin/tag +
decommission; `scope.rs` unit-tests the taxonomy mapping and human
labels.

### Location containment (REV-011)

REV-010 shipped fleet / site / type as three independent free-text
columns, which let a device be assigned to a site that does not belong
to its fleet (a nonsensical "mixed" pair). REV-011 makes **fleet → site
a real containment tree**: a **Site belongs to exactly one Fleet**.
**Device-type stays orthogonal** (a "sensor" applies at any site — never
nested under a site) and **tags stay free**.

- **Canonical groups** live in a new `location_groups` table
  (`crates/reeve-server/src/migrations/V11__location_groups.sql`,
  migration version 11): `group_id` PK, `kind CHECK(fleet|site)`,
  `name`, self-FK `parent_id` (`ON DELETE RESTRICT`), and a storage-level
  `CHECK` that a fleet has no parent and a site must have one. Fleet
  names are globally unique, site names unique per-fleet (two partial
  indexes, since a plain `UNIQUE` treats NULL parents as distinct).
  The migration **backfills** groups from existing `devices.(fleet,site)`
  so upgrades keep working; device rows stay the source of truth for a
  device's own assignment.
- **Group API** (`crates/reeve-server/src/groups.rs`, tag `groups`):
  `GET /api/groups` → nested `GroupTree { fleets:[{id,name,sites:[…]}] }`
  (scoped read `?fleet=<name>&kind=site`); `POST` create fleet or
  site-under-fleet (`parentId`); `PATCH` rename; `DELETE`. In-use rename/
  delete **refuse (409)**, they never cascade.
- **Assignment is validated** (`PATCH /api/devices/{id}`): the resulting
  `(fleet, site)` must be a real site under that fleet, else **422** — a
  fleet-only change that strands the current site is rejected too. Type
  is never validated; clearing is always allowed. **Enrollment
  auto-provisions** groups inside the enrol transaction (Law 5 — a join
  never fails on group bookkeeping).
- **UI makes a mixed pair unrepresentable**: the device/enrollment forms
  use a cascading Fleet→Site picker (`ui/src/components/location-fields.tsx`)
  whose site options come only from the selected fleet's scoped read;
  the Fleet page (`routes/_app/fleet/index.tsx`) renders the canonical
  nested tree merged with observed assignments. Full API contract:
  `spec/reeve/11-fleet-model.md` §11.1/§11.3.
- **Verified live** (`./scripts/dev-up.sh 3`, torn down after): the group
  tree read back nested (`north→plant-a`, `south→plant-b`); assigning a
  `south` device to `plant-a` (a north site) returned **422** with a
  containment message; reassigning to the valid `south/plant-b` returned
  **200** and re-rendered.

### Dev demo walkthrough (`./scripts/dev-up.sh [N]`)

`scripts/dev-up.sh` now stands up a **populated** fleet, so the UI lands
on a real tree instead of empties. Idempotent — safe to re-run. It:

1. builds + boots one `reeve-server` (password auth, seeded
   `admin`/`password`) and waits for `/healthz`;
2. enables the remote terminal fleet-wide by authoring
   `config/terminal.yaml` into `00-all` (the base every device
   inherits — the render places it in every device's bundle);
3. vendors a tiny single-service nginx **`hello`** compose package via
   `PUT /api/tree/packages/hello/1.0.0`;
4. mints a join token and starts `N` virtual devices (default 3), which
   enroll themselves;
5. builds the **containment tree first** — `POST /api/groups` creates
   fleets `north`/`south`, then sites `plant-a`/`plant-b` **under** their
   fleet (idempotent, tolerates 409) — then `PATCH`es each device into a
   fleet/site/type round-robin (only valid nested pairs) with a display
   name + tags: `Fleet north → Site plant-a → {hmi, sensor}` and
   `Fleet south → Site plant-b → {hmi}`. (The strict PATCH would 422 a
   pair whose groups don't exist, so the groups are created first.)
6. deploys `hello` to **Site plant-a** via `POST /api/deploy` — the two
   plant-a devices pick up the stack on the next render.

Opening `http://localhost:8420` then shows a browsable
Fleet → Site → Type → Device tree, devices with human names + tags, a
stack deployed to a site, History with human summaries, and a
terminal-enabled fleet. `./scripts/dev-down.sh` tears it down (volumes
included).

---

## Per-track summary

### Track A — foundations — 103 tests

| Crate | What's built | Tests |
|---|---|---|
| `reeve-types` | All Margo wire types + reeve extension types (manifest `(epoch,counter)`, capability advertisement, health payload, SSE events). Round-trip against ACTUAL `spec/margo/` + `reference/` fixtures. | 37 (lib 21, roundtrip 6, reeve_extensions 10) |
| `desired-state` | Pure overlay-tree render (zero I/O). Full table-test set: precedence, list-replace, null-delete, whole-file replace, 3-layer inheritance, class layer, `enabled:false`, pinned-under-rollout, byte-identical re-render, deterministic `deploymentId`, `${REEVE_REGISTRY}`. | 29 (lib 7, render 22) |
| `revision-store` | Content-addressed blobs + append-only revisions (SQLite), two streams per tier, diff/blame/read-at-revision; kill-9-mid-commit chaos. | 17 (store 16, chaos 1) |
| `margo-package` | Parse/validate packages from vendored dirs + real fixtures. | 20 (lib 9, real_fixtures 11) |

### Track B — reeve-agent — 154 tests

Manifest poll (conditional GET, ETag, epoch/counter anti-rollback,
offline no-op), bundle pull + verify + atomic swap, compose provider
with resumable journal phases, secrets fetch + minimal re-up,
persistent channel client, terminal sub-channel, health sampler +
journal backfill, self-install/update. Includes an HTTP poll suite
against a live axum mock and a **kill-9-mid-apply subprocess chaos
test**.

`reeve-agent`: 154 (lib 138, chaos 1, cli 3, example_package 2,
poll_http 10).

### Track C — reeve-server + device-api — 250 tests

Identity/auth (password/proxy/none, hashed device tokens, three
roles), enrollment (join tokens, idempotent), authoring API →
revisions with ownership, render pipeline → per-device OCI bundles +
manifest, status ingest + backfill + presence, durability
(snapshot + changeset + verify-restore + restore-at-bootstrap + epoch
fencing), secrets vault, device channels (ws + terminal byte bridge +
SSE), rollouts (cohorts/waves/gates/auto-pause), federation
(upstream sync, scoped secrets, air-gap export/import), zot proxy,
packaging + `init`.

`reeve-server`: 224 (lib 124 + 16 integration flow suites: auth 5,
channel 4, delivery 8, devices 6, durability_chaos 1, durability 9,
enroll 5, federation 7, packaging 8, rollout 9, secrets 5, sse 3,
status 7, terminal 7, tree_api 8, zot_proxy 8).
`device-api`: 26 (lib).

### Track D — UI

Full TanStack Router/Query/Table + shadcn UI; utoipa → openapi.json →
orval-generated client/hooks (no hand-written API types); SPA fallback
+ vite dev proxy. `cd ui && npm run build` succeeds; `just
check-api-drift` confirms the generated client matches the committed
one. (UI has no cargo test count; its gate is the build + drift check.)

### Track E — verification — 11 e2e tests (this task)

New workspace test crate **`crates/e2e`** drives the REAL server
(`reeve_server::bootstrap` + `router::build` on a localhost listener)
and the REAL agent (`poll_once` → `BundleStore::sync` →
`resolve_desired` → `converge` → `record_reports` →
`StatusSink::send_unsent`, the exact sequence in
`reeve-agent/src/main.rs`) together in-process. The only substituted
piece is the workload `Provider`: CI has no docker, so `FakeProvider`
records which apps got `up -d` / `down` — the observable the
convergence assertions read.

| e2e test | Scenario (charter E1) | Feature |
|---|---|---|
| `core_loop_author_render_converge_report` | M1 harness loop: author → render → poll → OCI pull+verify → converge → report → `GET /api/devices` shows `installed` | core |
| `converged_tick_is_silent_noop` | second tick is 304 + no-op converge (no gratuitous re-up) | core |
| `real_change_reconverges` | device-layer change bumps manifest by one counter, agent re-ups exactly that app | core |
| `two_devices_get_distinct_renders` | per-device delivery: two sites → different bundles, each converges its own | core |
| `agent_resumes_from_disk_offline_after_crash` | kill-9 after pull, before converge; restart with SERVER OFFLINE → heals from on-disk bundle (Law 3 + Law 5) | core |
| `reconverge_after_crash_is_idempotent_and_deduped` | restart re-runs nothing; status deduped by `(deviceId, seq)` | core |
| `server_startup_reconciles_unrendered_commit_then_agent_pulls` | server killed between commit and render; next boot reconciles; real agent pulls the healed render | core |
| `restore_bumps_epoch_and_agent_accepts_what_would_be_a_rollback` | snapshot → advance agent floor → restore OLDER snapshot → epoch fences → agent accepts a counter BELOW its floor as a NOTABLE bump, not a rejected rollback | core |
| `rotation_reups_only_the_consuming_app` | secret rotation bumps only the consumer's `secrets_version`, bundle digest unchanged, agent re-ups ONLY that app | ext-secrets |
| `failed_deploy_uploads_log_and_operator_reads_failure_text` | provider's `up` FAILS with captured output; agent uploads the deploy log; operator `GET`s the list + reads the FULL failure text — while Margo status still shows `failed` | ext-logs |
| `successful_deploy_also_stores_a_log` | a converging apply also stores an `applied`/`up` log (latest-wins forensic record) | ext-logs |
| `retention_keeps_at_most_n_deploy_logs` | a persistently-erroring app re-applies every pass; the server retains at most N=3 newest per (device, deployment) | ext-logs |
| `capabilities_reflect_the_compiled_feature_set` | advertisement derived from compiled features (core build advertises zero ext-*; full build advertises them) | both |
| `core_loop_runs_with_current_feature_set` | conformance headline: the base loop runs under whatever feature set it was built with | both |

Representative output (`cargo test -p e2e`):

```
test core_loop_author_render_converge_report ... ok
test converged_tick_is_silent_noop ... ok
test real_change_reconverges ... ok
test two_devices_get_distinct_renders ... ok
test agent_resumes_from_disk_offline_after_crash ... ok
test reconverge_after_crash_is_idempotent_and_deduped ... ok
test server_startup_reconciles_unrendered_commit_then_agent_pulls ... ok
test restore_bumps_epoch_and_agent_accepts_what_would_be_a_rollback ... ok
test rotation_reups_only_the_consuming_app ... ok
test capabilities_reflect_the_compiled_feature_set ... ok
test core_loop_runs_with_current_feature_set ... ok

test result: ok. 11 passed; 0 failed
```

Conformance build (`cargo test -p e2e --no-default-features`): the
`ext`-gated scenarios (rotation, the three `deploy_logs.rs` cases) are
compiled OUT; the core loop, all three chaos scenarios, and the
epoch-restore fencing run with **every extension compiled out** —
proving no extension is load-bearing for the base loop.

REV-011 deploy-log e2e (`cargo test -p e2e --test deploy_logs`):

```
test failed_deploy_uploads_log_and_operator_reads_failure_text ... ok
test successful_deploy_also_stores_a_log ... ok
test retention_keeps_at_most_n_deploy_logs ... ok

test result: ok. 3 passed; 0 failed
```

The harness gained an `ext-logs` seam mirroring the binary: `FakeProvider`
now CAPTURES combined output (`error_app` models a non-zero `up`,
`fail_app` an unhealthy container, `set_output` supplies the text), and
`TestAgent::tick` runs `reeve_agent::ext::logs::record_logs` after
`record_reports` — exactly where `reeve-agent/src/main.rs` runs it —
behind the e2e `ext` feature so the conformance build still compiles.

#### E1 scenarios covered by the existing in-crate integration suites

The remaining charter-E1 scenarios were already implemented as
`reeve-server` integration flow tests (real router, two in-process
servers for federation) and are green; the `e2e` crate adds the
cross-crate agent↔server coverage those suites do not. Mapping:

| Charter E1 scenario | Where | Test |
|---|---|---|
| Federation e2e (root+gateway sync, WAN outage, status backfill) | `federation_flow.rs` | `author_at_root_sync_renders_on_child`, `sync_resumes_after_interrupted_blob_fetch`, `status_forwards_upstream_idempotently` |
| Air-gap export/import round-trip | `federation_flow.rs` | `airgap_export_import_roundtrip_idempotent_and_tamperproof`, `airgap_status_return_trip` |
| Rollout wave halts on failed gate | `rollout_flow.rs` | `auto_pause_on_failed_status_then_resume`, `wave_advancement_on_healthy_gate`, `restart_mid_rollout_resumes_exactly` |
| Terminal session end-to-end + audit row | `terminal_flow.rs` | `bridge_relays_bytes_both_ways_and_audits`, `startup_finalizes_dangling_sessions` |
| Changeset restore to a sequence point | `durability_flow.rs` | `changeset_extract_apply_roundtrip`, `restore_at_bootstrap_e2e_with_epoch_fencing` |

---

## Running the full stack on THIS machine (server + two agents)

Debug binaries under `target/debug/`. All state is externalized;
processes are crash-only (kill and restart at will). Ports and paths
below are examples — adjust freely.

### 0. Build

```bash
cd /home/bherbruck/github/reeve
cd ui && npm run build && cd ..          # embed a fresh ui/dist
cargo build -p reeve-server -p reeve-agent
```

(Release/musl static binaries: `just build` builds
`target/release/reeve-server`; the CI matrix in `deploy/ci/` cross-
builds both arches for packaging.)

### 1. Server

```bash
export REEVE_DATA_DIR=/tmp/reeve/server      # SQLite DB + keyfile live here
export REEVE_LISTEN=127.0.0.1:8080
export REEVE_AUTH=none                        # dev only: anonymous acts as admin
export REEVE_REGISTRY=registry.example:5000   # ${REEVE_REGISTRY} substitution
target/debug/reeve-server
```

First boot with `REEVE_AUTH=password` logs a one-time setup token —
`POST /api/auth/setup` with it to create the admin. UI + API at
`http://127.0.0.1:8080` (SPA deep links fall back to index.html).

Optional durability to a local target (test/air-gap tier):

```bash
export REEVE_DURABILITY=snapshot              # or: changeset
export REEVE_DURABILITY_TARGET=/tmp/reeve/backups   # file path, file://, or s3://
```

Disaster recovery is normal startup with the DB absent and the flag
set (bring the keyfile back into `REEVE_DATA_DIR` first):
`target/debug/reeve-server --restore-from-target`.

### 2. Mint two join tokens

```bash
# with REEVE_AUTH=none, anonymous is admin:
curl -sX POST http://127.0.0.1:8080/api/join-tokens -H 'content-type: application/json' -d '{}'
curl -sX POST http://127.0.0.1:8080/api/join-tokens -H 'content-type: application/json' -d '{}'
# each response carries a one-time token string
```

### 3. Two agents

Agent config lives at `$REEVE_AGENT_CONFIG` (else
`/etc/reeve-agent/agent.toml`). Enroll writes it; running with no
subcommand starts the poll→converge loop.

```bash
# agent A
REEVE_AGENT_CONFIG=/tmp/reeve/agentA/agent.toml \
  target/debug/reeve-agent enroll --server http://127.0.0.1:8080 --token <TOKEN_A>
REEVE_AGENT_CONFIG=/tmp/reeve/agentA/agent.toml target/debug/reeve-agent &

# agent B
REEVE_AGENT_CONFIG=/tmp/reeve/agentB/agent.toml \
  target/debug/reeve-agent enroll --server http://127.0.0.1:8080 --token <TOKEN_B>
REEVE_AGENT_CONFIG=/tmp/reeve/agentB/agent.toml target/debug/reeve-agent &
```

Each agent stores its own `agent.db` + bundle store under the
`data_dir` in its config. Without docker the compose provider's
`up -d` will fail loudly and the app reports `failed` — expected on a
box with no runtime; the loop, poll, pull, and status reporting all
still work. On a box WITH docker, workloads converge for real.

### 4. Author desired state → watch it converge

```bash
# a package + a fleet layer that references it (base64-encode file bodies)
curl -sX PUT http://127.0.0.1:8080/api/tree/packages/web/1.0.0 \
  -H 'content-type: application/json' \
  -d '{"files":{"margo.yaml":"<b64>","compose.yml":"<b64>"}}'
curl -sX PUT http://127.0.0.1:8080/api/tree/layers/00-fleet \
  -H 'content-type: application/json' \
  -d '{"files":{"apps/web/app.yaml":"<b64 of: package:\n  name: web\n  version: 1.0.0\n>"}}'

curl -s http://127.0.0.1:8080/api/devices | jq   # per-device presence + deployment states
```

Air-gap / offline agents: point `server` at a `dir://<path>` OCI
layout (a hand-authored State Manifest + render bundle on disk); the
agent's fetcher takes it as a first-class source — the same code path
the `e2e` core-loop and the M1 harness exercise, no server required.

---

## What's stubbed / known gaps

Drawn from the concerns recorded in `DECISIONS-MADE.md`. None block the
build; each is a bounded judgment call or an explicit out-of-scope
fence from the charter.

- **Terminal enablement via the tree — RESOLVED (was DECISIONS-MADE
  L272).** `desired-state` render now emits `config/**` alongside
  `apps/**` + `manifest.yaml` (`render.rs::render_bundle_config`,
  whole-file replace across the chain), so terminal enablement is
  authored through the tree exactly like any other config: a device's
  bundle carries the merged `config/terminal.yaml`, which the agent
  reads from the bundle root and the server re-checks from its own
  render. `scripts/dev-up.sh` uses this directly (it enables the
  terminal fleet-wide by authoring `config/terminal.yaml` into
  `00-all`). `terminal_flow.rs` still owns the byte-bridge + audit e2e.
- **Agent-initiated sub-channel open (DECISIONS-MADE L190).** The
  server ships server-opened sub-channels fully; agent-initiated
  open (accept/reject tracking) is deferred to the first extension that
  needs it — accept/reject frames are currently ignored.
- **Health classification / health-state events (L183).** Device- vs
  link-degraded classification (§7.4) is deferred to the status-stream
  task; `presence.rs` documents the seam and preserves the
  `offline = link down, never device dead` asymmetry.
- **Resolve-endpoint rate limiting (L232).** Deferred; audit-counting
  is satisfied via metadata-only tracing of device + `name@version`.
- **UI cohort selector UX (L359/L399).** Ships exactly the task-directed
  selectors (explicit device checklist, layer-name chips, label k=v
  chips → `CohortSpec` union); richer selector UX and explicit
  waves/strategy arrays left unbuilt per the charter NOT-decided fence.
- **Charter NOT-decided fences (unbuilt by design):** settings
  envelope, coordinated secret rotation, operator taxonomies,
  multi-class devices, RBAC beyond three roles, mTLS/9421. Out of
  scope, not blockers.

No bug in another crate was uncovered by the e2e work: the epoch-restore
test was written to match the implemented **increment-then-serve**
semantics (the fenced epoch reaches a device on its next re-render, and
the higher epoch makes an otherwise-lower counter a legal notable bump)
rather than a re-pack-all-on-restore assumption. No source outside
`crates/e2e` was modified.
