# reeve spec — Persistent Agent Channel (REV-001)

Part of the reeve specification; start at [00-INDEX.md](00-INDEX.md).

## 4. Persistent Agent Channel (REV-001)

An OPTIONAL, outbound, persistent websocket from reeve-agent to
reeve-server providing: liveness as fact (open socket = reachable
*now*; drop = event), server→agent nudges that shorten convergence
latency without being required for correctness, and multiplexed
sub-channels for other extensions (the terminal, 03-terminal Section 5, is the
first consumer).

Margo's device API is request/response and its deployment-status
spec explicitly defers intermittent-connection scenarios
(`spec/margo/…/deployment-status.md`). reeve's Law 5 makes offline
the default. Polling has two costs — convergence latency bounded by
the poll interval, and "offline" indistinguishable from "between
polls" — and this channel fixes both, but only as an overlay:

> The polling loop is the correctness path. The channel is an
> optimization. Every behavior in this section MUST hold when the
> channel never connects at all.

Terms: **channel** — the single persistent websocket between one
agent and its server; **sub-channel** — a logically independent,
flow-multiplexed stream inside it, identified by numeric id, opened
for a named purpose; **nudge** — a server→agent hint that state
worth polling for has changed; **presence** — the server-side fact
"this device's channel is open".

### 4.1 Connection establishment

- ALWAYS initiated by the agent, outbound (NAT-native; the server
  never dials a device). Transport: websocket [RFC6455] over the
  same TLS listener as everything else (single socket, Section
  10.2). Endpoint: `GET /api/reeve/v1/channel` with standard
  upgrade.
- The upgrade request MUST be authenticated with the same device
  credentials as the device API (the enrollment-issued device
  credential — 01-framework Section 3.8 item 2). Unknown or
  unauthenticated clients MUST be rejected before upgrade. One
  channel per device: a new authenticated channel atomically
  replaces the old one (old socket closed; crash-only, no draining).
- The agent MUST NOT attempt the channel unless the server
  advertises `rev-001/1` (01-framework Section 3.3). Upgrade failure is
  feature-unavailable: log once, keep polling, retry on the
  reconnect schedule (Section 4.5).

### 4.2 Message framing

All messages are either **control frames** — websocket text, one
JSON object with a `type` field; unknown `type` values MUST be
ignored (01-framework Section 3.4) — or **data frames** — websocket binary,
first 4 bytes the sub-channel id (u32 big-endian), remainder opaque
payload owned by the sub-channel's registering extension.

Control frame types (rev-001/1):

| type | Direction | Fields | Meaning |
|------|-----------|--------|---------|
| `hello` | both, once at open | `protocol` (`"rev-001/1"`), `extensions` | version + sub-channel purposes supported |
| `nudge` | server → agent | `scope` (`"desired-state"` \| `"config"`), `hint` (opaque, OPTIONAL) | a new manifestVersion is available (bundle and/or secrets_version change); poll now (§4.4) |
| `open` | either | `id` (u32), `purpose` (e.g. `"rev-002/terminal"`), `meta` (OPTIONAL) | request sub-channel |
| `accept` | peer of `open` | `id` | sub-channel live |
| `reject` | peer of `open` | `id`, `reason` | refused; id released |
| `close` | either | `id`, `reason` (OPTIONAL) | closed; peers MUST discard in-flight data frames for `id` |
| `ping` / `pong` | either | `nonce` | application-level liveness probe (§4.3) |

Sub-channel ids are allocated by the side sending `open`: agent odd,
server even — allocation never collides.

Sub-channel semantics:

- `open` for an unsupported `purpose` MUST be answered with
  `reject`, never by tearing down the channel (01-framework Section 3.2).
- Data frames for an id not accepted-and-open MUST be discarded
  silently (frames race `close`; crash-only tolerance).
- Sub-channels carry bytes; content, flow control, and lifecycle
  beyond open/close belong to the registering extension. Registered
  purposes: `rev-002/terminal` (03-terminal Section 5).
- Channel teardown implicitly closes all sub-channels; extensions
  MUST treat sub-channel loss as a normal event, not corruption.
- Any extension needing bidirectional agent↔server bytes MUST use a
  sub-channel rather than a new listener or second websocket.
  (One-way server→UI events use SSE instead: 04-status-stream Section 6.)

### 4.3 Liveness as fact

- An open channel means: this device was reachable at last
  ping/pong. The server MUST maintain per-device presence (`online`
  + since / `offline` + last-seen) from channel state alone, and
  MUST NOT infer "device dead" from a closed channel — only "link
  down" (05-health-journal Section 7.4 makes the device- vs link-degraded
  distinction).
- Either side SHOULD `ping` when idle past its keepalive interval
  (RECOMMENDED 30 s) and MUST treat a missing `pong` within a
  timeout (RECOMMENDED 10 s) as a dead channel: close the socket,
  emit the presence event.
- Presence transitions are published to the UI as
  `device-presence` events (04-status-stream Section 6.3).

### 4.4 Nudges — optimization, never replacement

- When desired state changes for a device (its manifestVersion
  advances — new render bundle or secrets_version,
  including via 09-rollouts Section 11 wave advancement), the server SHOULD
  send `nudge` with scope `desired-state` on that device's channel.
- On `nudge` the agent SHOULD run its normal fetch-and-converge
  cycle immediately, subject to a local rate limit (RECOMMENDED: at
  most one nudge-triggered cycle per 5 s; coalesce bursts).
- The agent MUST NOT lengthen, suspend, or skip its polling
  schedule because the channel is open. Nudge delivery is
  best-effort: the server MUST NOT retry, queue for offline
  devices, or track acknowledgement. A lost nudge costs one poll
  interval of latency and nothing else. This is the normative
  encoding of Law 5: polling remains the correctness path.

### 4.5 Reconnect and backoff

- On any channel loss the agent MUST reconnect with jittered
  exponential backoff. RECOMMENDED: base 1 s, factor 2, full
  jitter, cap 5 min; a channel that lived ≥ 60 s resets backoff.
- Backoff state is in-memory only. After agent restart (including
  `kill -9`) reconnect begins fresh at base — startup IS recovery
  (Law 3); no persisted reconnect state exists to corrupt.
- The server MUST tolerate reconnect storms (e.g. after its own
  restart) without correctness impact; per-IP/device accept-rate
  limiting MAY be applied.

### 4.6 Offline semantics

- Channel absence changes NOTHING about convergence. The agent MUST
  converge identically (same repo fetch, same apply, same Margo
  status reports) whether the channel has been up for a week or has
  never connected.
- Features layered on the channel degrade per 01-framework Section 3.2: no
  presence (UI falls back to last-report recency), no nudges
  (latency = poll interval), no terminal.
- The agent MUST NOT block startup, enrollment, or its first
  converge on channel establishment.

### 4.7 Security

- The channel reuses device-API authentication; it grants no
  authority beyond the device API EXCEPT as a carrier for
  sub-channels, whose purposes carry their own authorization rules
  (the terminal's, 03-terminal Section 5, are strict).
- Nudges disclose "something changed" timing, which devices already
  learn by polling; `hint` MUST NOT carry secret material.
- A malicious agent with valid credentials can hold a socket open;
  resource limits SHOULD be enforced (max frame size RECOMMENDED
  1 MiB; per-device sub-channel cap RECOMMENDED 16).

