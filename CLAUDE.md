# reeve — fleet desired-state manager (Margo-inspired)

reeve (server) compiles desired state; reeve-agent (per box) converges on it.
The server compiles a layered deployment tree into per-device git repos.
reeve-agent pulls its repo, converges the box, reports status.

## Before anything else
`spec/margo/` and `reference/` are git submodules (Margo spec pin,
Margo sandbox). If either is empty, run `git submodule update --init
--recursive` first — NEVER proceed against an empty spec/margo/.

## Spec layout
- `spec/margo/` — submodule, pinned Margo spec snapshot (PR2). Read
  only. Never hand-edit; re-pin by bumping the submodule commit.
- `spec/reeve/` — ours, split per concern (start: 00-INDEX.md).
  Wire shapes, protocol semantics, normative behavior — what an
  independent implementation needs. "Ours entirely" (below) gets
  written down here.
- `docs/decisions/` — our build choices (crates, sidecars, tools),
  D-numbered; start at its 00-INDEX.md.
- `reference/` — submodule, Margo sandbox. Reference implementation,
  not authoritative — spec/margo/ wins on conflict.

## Doc routing — implementing X, read Y
| Working on | Read |
|---|---|
| types / wire formats | spec 01-framework + decisions delivery.md |
| agent channel / terminal | spec 02-channel + 03-terminal + decisions auth.md |
| status, SSE, health | spec 04-status-stream + 05-health-journal |
| federation / tiers | spec 06-federation + decisions storage.md |
| durability / restore | spec 07-durability + decisions storage.md |
| tree, render, rollouts | spec 09-rollouts + decisions tree-render.md |
| secrets | spec 10-secrets + decisions secrets.md + auth.md |
| packaging / install | spec 08-packaging + decisions deploy.md |
| UI | decisions ui.md + spec 04-status-stream |
| enrollment / device auth | decisions auth.md + agent.md + spec 01-framework §3.8 |

## The Five Laws
1. **Spec-grounded.** Implement against the pinned Margo spec in
   `spec/margo/`. Never from memory of Margo. Where a type mirrors
   the spec, cite the spec file/section in a doc comment. See "Spec
   fidelity" below for exactly where spec-exactness is required vs.
   where our design wins.
2. **Every crate stands alone.** Each crate compiles and passes tests by
   itself (`cargo build -p <crate>`). No crate reaches into another's
   internals. Smallest useful unit per crate.
3. **Crash-only.** No shutdown ceremony anywhere. Startup IS recovery.
   `kill -9` mid-operation must leave resumable state. Writes are atomic
   (temp+rename) or transactional (SQLite). Idempotent startup, always.
4. **State lives in engines with someone else's test suite.** ALL
   server state — including desired-state history — is SQLite (WAL):
   a content-addressed revision store (blobs + append-only revisions,
   see docs/decisions/delivery.md D13). Change-log durability is the
   SQLite session extension (trunk, D16) — no VCS, no replication
   sidecar in the runtime. Nothing load-bearing lives only in RAM.
   Config in files; settings in the DB. Never commit values, only
   shape (.env rule).
5. **Offline-first agent.** reeve-agent assumes it is offline more than
   online. Every network call has a "couldn't reach — continue from last
   known state" path. Polling, outbound-only, NAT-native. This is the
   gap the Margo spec defers; we do it properly. On any conflict between
   spec text and this law, this law wins.

## Spec fidelity — where the line is
- **WIRE-EXACT:** `reeve-types` and `margo-package` MUST parse real
  Margo artifacts unmodified — the YAML in `spec/margo/` and
  `reference/` are the test fixtures. Field names, structure, semantics:
  exact. If we extend, extensions are additive and clearly marked, never
  redefinitions of spec fields.
- **PATTERN-FAITHFUL:** per-device desired state delivered Margo's way
  (State-Manifest poll + content-addressed pull, conditional GET,
  monotonic manifestVersion — docs/decisions/delivery.md D13), pull-based agent,
  workload/device management (WFM/DFM) split — keep Margo's shape, our
  implementation.
- **OURS ENTIRELY:** everything the spec doesn't nail down or gets wrong
  for our topology — the overlay tree (spec is silent on how desired
  state is derived), offline behavior (spec defers it), storage choices,
  crash-only posture, the systemd-unit provider. Write these decisions
  down in `spec/reeve/`, not as scattered comments.
- **Rule of thumb:** if it crosses the wire or lives in a file another
  Margo tool might read, spec-exact. If it's how we get there, ours.

## Substrate rules
- Services are substrate-blind: no orchestrator APIs, no cluster
  assumptions. reeve-agent applies workloads through the `Provider`
  trait — compose first, systemd units second, helm later/never.
- Operational contract baked in from line one: SIGTERM-clean, /healthz,
  structured logs to stdout, config via env/files, externalized state.

## ui/ — full web UI (vite + react + ts)
- TanStack Router (file-based routes in ui/src/routes/), TanStack
  Query for ALL server state, TanStack Table for tabular views.
  shadcn/ui + tailwind for components. No other state or routing
  libraries without asking.
- API types are GENERATED: axum routes annotated with utoipa ->
  openapi.json -> ui/src/api/ (generated client + React Query hooks).
  Never hand-write API types in TS. Regenerate after any route change.
- reeve-server serves /api/* from axum, embedded ui/dist assets by path
  (rust-embed), and falls back to index.html for all other GETs (SPA
  deep links must not 404). Dev mode: vite proxies /api to a running
  reeve-server.
- Live updates: SSE endpoint -> Query cache invalidation; polling
  fallback. SSE for one-way status; websockets ONLY where genuinely
  bidirectional (see Remote terminal below).
- File names always kebab-case (`app.tsx`, not `App.tsx`) — no
  exceptions, including framework-default scaffold names.
- CRUD: prefer DRY dedicated pages over modals. One shared form
  component per resource, reused by `new` and `edit` routes; `detail`
  is its own page. No create/edit/detail modals.

## Remote terminal (guardrails)
Full spec: `spec/reeve/03-terminal.md` (REV-002, Section 5). Summary here is
MUST-level:
- Terminal disabled by default; enabled only via desired state (a
  config commit with an author and a diff), never a runtime toggle.
- Sessions are short-lived, explicitly initiated — no
  standing/background sessions.
- Every session is audited in reeve-server's DB.
- The reeve-server bridge relays bytes only. It MUST NOT interpret
  session content, log secrets in plaintext, or execute anything
  server-side.

## Layout
- crates/reeve-types    — Margo-shaped types (ApplicationDescription,
                          deployment profiles, status). serde only.
- crates/margo-package  — parse/validate app packages (dir or OCI ref).
- crates/desired-state  — THE crate: overlay tree -> rendered per-device
                          state. Pure functions. Zero I/O. Table-tested.
- crates/revision-store — content-addressed blob + append-only revision
                          tables (rusqlite). No VCS anywhere; gix is
                          not in the workspace.
- crates/device-api     — axum routes: enroll, status ingest.
- crates/reeve-agent    — agent binary: fetch -> diff -> apply -> report.
- crates/reeve-server   — server binary: ties it together + embedded UI.
- ui/                   — full web UI (vite + react + ts). See "ui/" below.

## Build order
reeve-types -> desired-state -> revision-store -> reeve-agent (compose
provider) -> device-api -> reeve-server -> UI. Milestone 1 (PROPOSAL,
confirm harness): full agent loop against a LOCAL DIRECTORY source —
a hand-authored State Manifest JSON + render bundle as an OCI layout
dir on disk; the agent's fetcher takes `dir://` as a first-class
source (the same code path air-gap media apply uses later). No
server, no network. Chaos check (kill -9 mid-converge) runs against
this harness.

## Verification
- `cargo test --workspace` and `just standalone` (every crate builds
  alone) must pass before anything is called Done.
- desired-state: table tests (tree in, files out) ARE the spec for that
  crate. Write them before the implementation.
- Wire types: round-trip tests against actual spec/margo or
  reference YAML files, not hand-written approximations of them.
- Chaos check before calling anything Done: kill -9 the process
  mid-operation, restart, assert convergence. "It works" and "it's
  done" are different claims.
