# reeve spec — Device Health & Status Journal (REV-004)

Part of the reeve specification; start at [00-INDEX.md](00-INDEX.md).

## 7. Device Health & Status Journal (REV-004)

Forensic, gap-free device history despite Law 5. The agent keeps a
local, crash-safe journal of every status report and health sample
with original timestamps; on reconnect it backfills the server,
which ingests late records at their original time. Health telemetry
rides as additive fields on the Margo-shaped status report.

Margo's deployment-status endpoint assumes a consistent connection
and explicitly defers intermittent-disconnection scenarios
(`spec/margo/…/deployment-status.md`); for reeve's topology that
gap is the common case. This extension makes history forensic —
reconstructed from records made at the time — rather than
gap-filled or interpolated.

Terms: **journal** — the agent-local, append-only record of status
reports and health samples; **health sample** — a point-in-time
telemetry reading; **backfill** — transmission of journal records
the server has not yet acknowledged; **original timestamp** — the
time a record was observed on the device, assigned when journaled,
never rewritten.

### 7.1 Agent journal

- The journal is SQLite (WAL) on the device — crash-safe by an
  engine with someone else's test suite (Law 4). Appends are
  transactional; `kill -9` mid-append loses at most the uncommitted
  record (Law 3).
- The agent MUST journal: every Margo `DeploymentStatusManifest` it
  reports (or would report while offline), every health sample, and
  agent lifecycle marks (start, converge begin/end, provider
  errors). Each record carries its original timestamp and a
  monotonic per-device sequence number.
- Journaling MUST NOT depend on connectivity: records are written
  locally first, always; transmission is store-and-forward.
- Retention is bounded (config; RECOMMENDED default 30 days or
  512 MiB, whichever first). Eviction is oldest-first and MUST NOT
  evict unacknowledged records unless the size bound forces it, in
  which case the agent MUST journal a gap mark so the server can
  distinguish "evicted" from "never happened".

### 7.2 Health samples

Sampled on a config interval (RECOMMENDED default 60 s): disk
usage/free per relevant filesystem, memory usage, load averages,
per-workload container restart counts (from the active Provider),
agent version, and clock skew versus the server (measured
opportunistically when connected; skew matters because original
timestamps are device-assigned). Fields are extensible; receivers
MUST ignore unknown sample fields.

### 7.3 Wire behavior

**Live path — additive fields on the Margo status report.** When
connected, the agent reports deployment status exactly as Margo
defines (`POST /api/v1/clients/{clientId}/deployments/
{deploymentId}/status`). rev-004/1 adds one additive object under
the `reeve` key of the `DeploymentStatusManifest` body:

```json
{
  "apiVersion": "deployment.margo.org/v1alpha1",
  "kind": "DeploymentStatusManifest",
  "...": "margo-defined fields unchanged",
  "reeve": {
    "observedAt": "2026-07-10T06:12:03Z",
    "seq": 48211,
    "health": { "disk": {"...": "..."}, "memory": {}, "load": [],
                 "restarts": {}, "agentVersion": "0.4.2",
                 "clockSkewMs": -120 }
  }
}
```

A vanilla WFM ignores `reeve` entirely and loses nothing Margo
defines. reeve-server uses `observedAt`/`seq` to place the report
in history and detect records it already holds.

**Backfill path — reeve surface.** On reconnect after any gap (and
periodically as a sweep), the agent backfills unacknowledged
journal records to `POST /api/reeve/v1/journal/{deviceId}` in
batches ordered by sequence number, authenticated with device-API
credentials. The server replies with the highest contiguously
ingested sequence number; that acknowledgement is what permits
journal eviction (Section 7.1). The protocol is idempotent by
`(deviceId, seq)` — the server MUST deduplicate, so resending after
a crash is harmless (Law 3).

**Late ingest.** The server MUST ingest late records at their
original timestamps. History queries MUST return the same answer
whether the device was connected all along or backfilled a week
later — forensic, not gap-filled. The server MUST NOT overwrite an
already-ingested `(deviceId, seq)` record and MUST record
server-receipt time alongside original time (the pair makes
tampering and skew visible).

### 7.4 Health classification: device-degraded vs link-degraded

The server-side model MUST distinguish:

- **Link-degraded**: no presence (02-channel Section 4.3) and no fresh reports
  — but subsequent backfill shows healthy samples during the
  window. The device was fine; the path was not.
- **Device-degraded**: samples (live or backfilled) breaching
  health thresholds, or a journal gap mark, or a device that is
  present yet reporting unhealthy samples.
- **Unknown**: offline window not yet backfilled — MUST be surfaced
  as unknown, never silently assumed healthy or dead.

Transitions are published as `health-state` events (04-status-stream Section 6.3)
with `kind` = `device` | `link`. Reclassification after backfill
(unknown resolving to healthy or degraded) is normal and MUST
update history retroactively — exactly what original-timestamp
ingest makes possible. Rollout health gates consume this
classification (09-rollouts Section 11.4).

### 7.5 Federation

Under 06-federation Section 8, a gateway tier journals what its local agents
report and backfills its upstream using this same protocol,
recursively — from upstream's perspective the gateway is an
agent-shaped source of `(deviceId, seq)` records with original
timestamps. Section 7.3's semantics apply unchanged at every tier.

### 7.6 Security

- Health payloads reveal device internals to the server — the
  existing trust relationship (it already holds capabilities and
  deploys workloads). Federation forwards them upstream; Section
  8.4's ownership model makes those tiers explicit.
- Original timestamps are device-asserted. Paired receipt times
  plus clock-skew samples bound how far a compromised device can
  quietly rewrite its own history; nothing can stop a device lying
  about itself.
- Journal eviction pressure is attacker-influenceable (spam samples
  to push out history); the gap-mark rule makes eviction visible.

