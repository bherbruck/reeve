## 11. Staged Rollouts (REV-008)

Controlled propagation of a desired-state tree revision through the
deployment tree in waves over cohorts of devices, with health gates
between waves and automatic pause when a wave's failure threshold
trips. Mechanically a rollout is nothing but State-Manifest
advancement, wave by wave — per device, the manifest starts
pointing at the render of the rollout's revision — and agents
converge on their manifest exactly as always, never knowing a
rollout exists.

Advancing every device's manifest the moment a revision renders is
fine for ten boxes and reckless for ten thousand. The primitive
reeve already has — per-device State Manifests the server controls
(§10.2, pull-only from below) — makes staging natural: a device's
desired state is whatever its manifest points at, so controlling
propagation IS controlling manifest advancement. This section adds
exactly that control layer: no new agent behavior, no new wire
format. The rollout engine is WFM-internal bookkeeping over
information the server already has (§7 health, §4.3 presence, Margo
deployment status).

Terms: **rollout** — one controlled propagation of one desired-state
tree revision (as rendered per device) to a set of devices;
**cohort** — the targeted device set: an explicit list, a tree
selection, or a LABEL selection (DECISIONS.md D12 — labels group,
layers configure; labels select cohorts and filter UIs, they MUST
NOT select or inject configuration); **wave** — an ordered
partition of the cohort, wave N+1 starting only after wave N passes
its gate; **gate** — the health condition evaluated between waves;
**failure threshold** — the per-wave condition that trips
auto-pause.

### 11.1 Rollout model

- A rollout is defined by: the source tree revision (of the layer
  being changed), the cohort, an ordered list of waves (explicit
  partition or a strategy such as `1, 10%, 50%, rest` resolved to
  explicit device sets at creation), a gate policy, and a failure
  threshold.
- **Convergence target (normative, DECISIONS.md D12):** a device's
  target is ITS OWN RENDER of the rollout's tree revision. Pinned
  devices converge to renders still carrying the pin and count as
  CONVERGED in gate math. The rollout status API/UI MUST surface
  cohort members whose render is materially unchanged by the
  rollout ("pinned/unaffected: N") so a green wave cannot silently
  mean "nothing was actually deployed here".
- Rollout definitions and their full transition history are runtime
  state in reeve-server's SQLite DB — irreplaceable-in-flight data
  (§9.5).
- At most one active rollout MAY target a given device's manifest
  at a time; overlapping cohorts for the same layer MUST be
  rejected at creation (queue or fail — implementation choice;
  never interleave manifest advancement for one device from two
  rollouts).

### 11.2 Mechanics: manifest advancement

- Starting a wave means: for each device in the wave, advance that
  device's State Manifest to reference the render of the rollout's
  tree revision (bumping manifestVersion, §10.2). Nudges (§4.4)
  SHOULD be sent as usual; offline devices simply converge when
  they next poll (Law 5 — a rollout does not redefine convergence,
  it only times manifest movement).
- Manifest advancement per device is atomic (one SQLite tx); a
  server crash mid-wave leaves some devices advanced and some not —
  a resumable position, not corruption: on startup the rollout
  engine re-reads manifest state and continues (Law 3).
- Pausing (manual or auto) stops manifest advancement. Devices
  already advanced stay converged on the new state; devices not yet
  advanced stay on the old. A paused rollout is a stable,
  inspectable position — nothing drifts while humans decide.
- Aborting is pausing permanently (records retained). It does not
  move any manifest backward.

### 11.3 Gates

Between waves the gate MUST evaluate, per device of the completed
wave, over a configurable soak window (RECOMMENDED default 15 min):

- **Deployment status** (Margo): reported state for the affected
  deployment(s) is `installed`, not `failed`
  (`spec/margo/…/deployment-status.md` state enum).
- **Health** (§7.4): classification is healthy — specifically not
  device-degraded; link-degraded/unknown devices count per the
  offline policy below.
- **Presence** (§4.3) where available: distinguishes "converged and
  quiet" from "never heard back".

Offline policy: devices link-degraded/unknown for the whole soak
window count as `undetermined`, not failed. The gate passes when
(converged + healthy) ≥ pass fraction of the wave (RECOMMENDED
default 100% of determinable devices, with an undetermined
allowance for chronically-offline fleets — config). Gates MUST NOT
wait forever on offline devices: after the soak window plus a
timeout they resolve with what is determinable and report the
undetermined set.

Gate evaluation is server-side only, over data the server already
ingests. Nothing is asked of agents beyond their normal reporting.

### 11.4 Auto-pause

- Each wave carries a failure threshold (RECOMMENDED default: any
  device `failed`, i.e. threshold 1; config per rollout).
- When breached at any time during a wave or its soak — not only at
  gate time — the rollout MUST auto-pause: manifest advancement
  stops immediately, including for un-advanced devices of the
  current wave.
- Auto-pause is an event, not an error state that decays: paused is
  a first-class rollout state awaiting explicit human resume,
  re-scope, or abort.

### 11.5 No automatic rollback (v1)

- The rollout engine MUST NOT move any device manifest backward on
  its own — not on gate failure, not on threshold breach, not on
  abort. Automatic rollback under partial, possibly link-degraded
  information does more harm than a paused, inspectable fleet: an
  unreachable device that later backfills healthy history (§7.4)
  would have been "rolled back" for being offline.
- Rollback is a NEW rollout whose source is the prior tree revision
  content (a new revision with the old content — undo, D13),
  explicitly initiated by a human, with waves and gates like any
  rollout. History only ever moves forward, and every fleet-state
  change — including reversions — is a rollout with an author. This
  also keeps every device's manifestVersion strictly increasing
  (§10.2 anti-rollback) even when the CONTENT reverts.

### 11.6 Observability

Rollout and wave transitions (`started`, `gated`, `paused`,
`completed`, `failed`) are published as `rollout` events (§6.3) and
exposed in the REST API for the UI's rollout views (per-device:
advanced / converged / healthy / undetermined / failed).

### 11.7 Federation

Single-writer-per-layer (§8.4) holds unchanged: a rollout is
authored at exactly ONE tier — the tier owning the layer being
changed — and propagates downward only, through normal revision
sync. A gateway executes manifest advancement for its local devices
as the synced revisions reach it; it MUST NOT re-stage, reorder, or
gate a parent-tier rollout except by going offline (which merely
delays it, per Law 5). A gateway MAY author its own rollouts for
layers it owns.

### 11.8 Security

- Creating, resuming, and aborting rollouts MUST be authorized,
  attributable operations (audit-logged with author): a rollout is
  the mechanism by which arbitrary workload changes — including
  agent self-update (§10.5) — reach the fleet.
- Auto-pause thresholds are a safety mechanism, not a security
  boundary: a device can lie about its own health (§7.6) and
  thereby pass or trip gates for its wave; cohort/wave design
  SHOULD avoid letting any single device's self-report gate the
  whole fleet.
- Pause-not-rollback means a bad-but-not-failing commit stays
  deployed on early waves until humans act; the audit trail and §6
  events exist to make that window short.

