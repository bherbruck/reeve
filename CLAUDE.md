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
- `spec/reeve/` — ours. Divergences from Margo, extensions Margo
  doesn't cover, decisions we made where the spec is silent. This is
  where "ours entirely" (below) gets written down.
- `reference/` — submodule, Margo sandbox. Reference implementation,
  not authoritative — spec/margo/ wins on conflict.

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
4. **State lives in engines with someone else's test suite.** Server
   runtime state: SQLite (WAL). Desired state: bare git repos on disk.
   Nothing load-bearing lives only in RAM. Config in files; settings in
   the DB. Never commit values, only shape (.env rule).
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
- **PATTERN-FAITHFUL:** per-device desired-state repo, pull-based agent,
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

## Layout
- crates/reeve-types    — Margo-shaped types (ApplicationDescription,
                          deployment profiles, status). serde only.
- crates/margo-package  — parse/validate app packages (dir or OCI ref).
- crates/desired-state  — THE crate: overlay tree -> rendered per-device
                          state. Pure functions. Zero I/O. Table-tested.
- crates/repo-store     — bare repos on disk via gix. commit/read/render
                          plumbing. No shelling out to git.
- crates/device-api     — axum routes: enroll, status ingest.
- crates/reeve-agent    — agent binary: fetch -> diff -> apply -> report.
- crates/reeve          — server binary: ties it together + embedded UI.
- ui/                   — embedded UI source. File names always
                          kebab-case (`app.tsx`, not `App.tsx`) — no
                          exceptions, including framework-default
                          scaffold names.

## Build order
reeve-types -> desired-state -> repo-store -> reeve-agent (compose
provider) -> device-api -> reeve -> UI. Milestone 1: full loop against a
local bare repo with `git daemon`, no server at all.

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
