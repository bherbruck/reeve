## 6. Live Status Stream (REV-003)

A Server-Sent Events endpoint on reeve-server pushing state-change
notifications to the web UI: device presence, deployment status
transitions, terminal session events, health state changes,
verify-restore results, rollout progress. Events are
cache-invalidation hints, not a data channel; the UI's polling path
remains complete and correct without SSE.

Division of labor (CLAUDE.md "ui/"): SSE for one-way
server→browser status; websockets ONLY where genuinely
bidirectional — the terminal (Section 5.1). Events tell the UI
*that* something changed and which query caches to invalidate; the
data of record is always refetched from the REST API. An event is
therefore droppable: a lost event costs latency, never correctness.

### 6.1 Endpoint

- `GET /api/reeve/v1/events` — `text/event-stream`, same listener
  as everything else (Section 10.2). MUST NOT be served
  unauthenticated; authorization matches the corresponding REST
  reads (Section 6.4).
- Query parameter `types` (OPTIONAL, comma-separated) filters event
  types; unknown names are ignored.
- Consumers are browsers/UI clients. reeve-agent MUST NOT depend on
  this endpoint (agents have Section 4; Law 5 forbids agent
  correctness depending on any push path).

### 6.2 Delivery semantics

- Events carry a monotonically increasing per-stream `id:`. The
  server SHOULD honor `Last-Event-ID` on reconnect from a bounded
  in-memory replay buffer (RECOMMENDED: last 256 events or 60 s,
  whichever smaller). The buffer is best-effort, in-memory only —
  empty after restart, which is correct because clients refetch on
  reconnect (Law 3: no shutdown flushing, no persisted event log).
- If the server cannot replay from the client's `Last-Event-ID`, it
  MUST send a `reset` event first; on `reset` the client MUST treat
  all cached state as stale (invalidate everything, refetch).
- The server SHOULD send an SSE comment (`:ka`) at least every 30 s
  so proxies do not idle-close the stream.
- Delivery is at-most-once per stream. Producers MUST NOT rely on a
  UI having seen any event.
- Polling fallback: every UI view MUST remain correct with the
  stream absent — TanStack Query refetch intervals stay configured
  (RECOMMENDED defaults: 30 s lists, 10 s focused detail). UI code
  MUST NOT gate any action on stream state; connect/disconnect is
  invisible beyond a freshness indicator.

### 6.3 Event types (rev-003/1)

Payloads are JSON in `data:`, each including `ts` (RFC 3339 server
time). Unknown event types MUST be ignored by clients. New types
are registered by adding a row here from the defining section.

| event | Producer | Payload (beyond `ts`) | Emitted when |
|-------|----------|-----------------------|--------------|
| `reset` | §6.2 | — | replay not possible; client refetches all |
| `device-presence` | §4.3 | `deviceId`, `state` (`online`\|`offline`), `since` | channel opens/drops |
| `deployment-status` | Margo status ingest | `deviceId`, `deploymentId`, `state` (Margo enum) | ingested manifest changes a deployment's overall state |
| `terminal-session` | §5.4 | `sessionId`, `deviceId`, `phase` (`requested`\|`opened`\|`closed`\|`denied`), `user` | session lifecycle transition |
| `health-state` | §7.4 | `deviceId`, `state` (`healthy`\|`degraded`\|`unknown`), `kind` (`device`\|`link`) | health classification changes |
| `verify-restore` | §9.4 | `outcome` (`ok`\|`failed`), `snapshotTs`, `detail` | a verify-restore run completes |
| `durability-lag` | §9.3 | `generation`, `lastSeq`, `lagSeconds` | changeset upload lag crosses/clears a threshold — ops dashboard signal |
| `rollout` | §11.6 | `rolloutId`, `wave`, `phase` (`started`\|`gated`\|`paused`\|`completed`\|`failed`) | rollout/wave transition |
| `secret-rotation` | §12 | `secretName`, `scope`, `version`, `state` (`propagating`\|`converged`) | a secret version changes / all affected devices report converged |

Payloads identify entities; clients invalidate matching TanStack
Query keys and refetch truth. Payloads MUST NOT be treated as the
new entity state (optimistic hinting only).

### 6.4 Security

- The stream aggregates fleet-wide activity; it MUST be authorized
  with the same granularity as the corresponding REST reads — a
  user who cannot list a device MUST NOT receive its events;
  filtering is server-side.
- `terminal-session` events expose session metadata (who, which
  device, when) — intentional audit visibility per Section 5.4;
  they MUST NOT ever carry session content.
- The replay buffer holds recent payloads in RAM; payloads are
  entity identifiers and coarse states, never secrets — producers
  MUST keep it that way.

