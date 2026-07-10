# The reeve Specification

```
Status:       Stable
Scope:        All reeve extensions to the pinned Margo specification
Margo pin:    spec/margo/ (submodule commit is authoritative)
Extensions:   REV-001 .. REV-008 (index in Section 3.5)
```

## Abstract

reeve implements the Margo specification pinned at `spec/margo/` and
extends it where Margo is silent (offline behavior, desired-state
derivation, federation) or where reeve needs capabilities Margo does
not define (remote terminal, health journal, staged rollouts). This
document is the complete reeve extension specification. Section 3 is
its load-bearing clause: every extension is additive, and vanilla
Margo tooling that knows nothing of this document MUST interoperate
unmodified with every Margo surface reeve serves or emits. reeve
also REPLACES three Margo device-facing surfaces outright
(onboarding, device-API auth, desired-state delivery); Section 3.8
enumerates them — replacement is declared there, never implied
elsewhere. Sections 4–12 define the extensions themselves,
identified REV-001 through REV-009.

## 1. Introduction

The founding constraint, restated from CLAUDE.md's "Spec fidelity"
rules: anything that crosses the wire or lives in a file another
Margo tool might read is spec-exact; everything else is ours. This
document turns that rule of thumb into normative requirements
(Section 3) and then spends the rest of its length in the "ours"
column: capabilities layered over Margo in ways vanilla tooling can
ignore entirely.

Recurring context, cited throughout as "Law N" (CLAUDE.md, The Five
Laws): Law 3 — crash-only, startup IS recovery; Law 4 — state lives
in engines with someone else's test suite (SQLite, including the
content-addressed revision store); Law 5 — the agent assumes it is
offline more than online, polling, outbound-only, NAT-native.

State/delivery taxonomy (DECISIONS.md D13): the per-device State
Manifest is the ONE mutable coordinating document; everything it
points to is immutable (code: OCI images/packages; config: the
render-bundle digest) or authenticated-on-demand (secrets: the
resolve endpoint, Section 12). Settings, when decided, are expected
to become a third versioned pointer (`settings_version`) with its
own endpoint.

## 2. Conventions and Terminology

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in
BCP 14 [RFC2119] [RFC8174] when, and only when, they appear in all
capitals, as shown here.

- **Extension**: a capability defined by a REV-numbered section of
  this document, beyond the pinned Margo specification.
- **Vanilla Margo tooling**: any WFM, device client, or other
  implementation that conforms to `spec/margo/` and knows nothing of
  this document.
- **Additive field**: a field added to a Margo-defined payload that
  is OPTIONAL, syntactically ignorable by a receiver that does not
  know it, and whose absence or removal does not change the meaning
  of any Margo-defined field.
- **Margo surface**: any endpoint, payload, or file format defined
  in `spec/margo/` (the onboarding API, `DeviceCapabilitiesManifest`,
  `DeploymentStatusManifest`, application description YAML, ...).
- **reeve surface**: any endpoint or format defined only here. reeve
  surfaces live under the URL prefix `/api/reeve/`, with one named
  carve-out whose prefix is fixed by external convention: `/v2/`
  (the OCI distribution routes — native read-only pull of reeve's
  own artifacts, plus the OPTIONAL image proxy to zot; Section 10.2,
  DECISIONS.md D7/D8). All are invisible to vanilla Margo tooling
  and none shadows a Margo path.
- **Revision store**: the content-addressed history of the overlay
  tree — sha256 blobs + append-only revisions — in reeve-server's
  SQLite (DECISIONS.md D13). Git does not exist in the runtime.
- **State Manifest / render bundle**: the per-device desired-state
  pointer document (manifestVersion, bundle digest, per-app
  secrets_version) and the OCI artifact it references (the rendered
  D2 layout), Section 10.2.
- **Channel / sub-channel / nudge / presence**: see Section 4.
- **Journal / backfill / original timestamp**: see Section 7.
- **Tier / root / gateway (tier) / layer**: see Section 8.
- **Snapshot / target / RPO / verify-restore**: see Section 9.
- **Rollout / cohort / wave / gate**: see Section 11.
- **Secret reference / secrets_version / resolve**: see Section 12.

## 3. Extension Framework & Conformance (REV-000)

This section governs everything after it. If any later section ever
appears to conflict with this one, this one wins, and the conflict
is a spec bug to fix — not a judgment call to make in code.

### 3.1 The additivity rule

Extensions are ADDITIVE:

1. A conforming Margo WFM or device client that knows nothing of
   this document MUST be able to interoperate unmodified with every
   Margo surface reeve serves or emits. (reeve does not serve every
   Margo surface: Section 3.8 enumerates the surfaces reeve replaces
   rather than serves — a closed list; anything not listed there is
   served or emitted spec-exact.)
2. An extension MUST NOT redefine, rename, remove, or change the
   semantics of any field, endpoint, or status code defined in
   `spec/margo/`.
3. An extension MAY add fields to Margo-defined payloads only as
   additive fields, and all reeve additive fields MUST be nested
   under a single top-level key named `reeve`, so a receiver can
   ignore the entire extension surface by ignoring one key.
4. An extension MAY define new endpoints, but they MUST live on
   reeve surfaces, MUST NOT shadow any path defined in
   `spec/margo/`, and MUST NOT be required for Margo-defined flows
   to complete.

### 3.2 Degradation

Absence of a capability MUST degrade to feature-unavailable, never
to error:

- reeve-server facing a device that does not advertise an extension
  MUST treat the feature as unavailable for that device (no terminal
  button, presence from polling recency only) and MUST NOT fail
  enrollment, status ingest, or deployment because of the absence.
- reeve-agent facing a server that offers no extensions (a vanilla
  Margo WFM) MUST fall back to pure Margo behavior. Convergence MUST
  NOT depend on any extension (Law 5; Section 4.6).
- A payload carrying unknown `reeve` sub-fields MUST NOT be rejected
  for that reason by either side.

### 3.3 Capability discovery

**Agent side.** reeve-agent advertises supported extensions as one
additive object on the Margo `DeviceCapabilitiesManifest`
(`spec/margo/system-design/specification/margo-management-interface/
device-capabilities.md`), inside `properties`:

```json
{
  "properties": {
    "...": "margo-defined fields unchanged",
    "reeve": {
      "agentVersion": "0.4.2",
      "extensions": ["rev-001/1", "rev-002/1", "rev-004/1"]
    }
  }
}
```

Each entry is `"rev-NNN/V"`: REV number, protocol version
(Section 3.4). A vanilla WFM sees one unknown optional object and
ignores it. A manifest with no `reeve` key means: no extensions.

**Server side.** reeve-server advertises its extension list plus
server version at `GET /api/reeve/v1/capabilities`. The agent SHOULD
probe once per enrollment and on version change; 404 or any error
means "vanilla Margo server" and the agent MUST proceed with pure
Margo behavior.

**Negotiation.** There is none. A feature is usable between a given
agent and server iff both advertise a common protocol version for
it; anything else is feature-unavailable (Section 3.2).

### 3.4 Versioning

- Each extension that touches the wire carries an integer
  **protocol version**, starting at 1, advertised as `"rev-NNN/V"`.
- Additive changes (new optional fields, new event types, new frame
  types peers may ignore) do NOT bump the protocol version; this
  document is amended in place.
- Breaking changes bump the protocol version. Versions are distinct
  capabilities: an implementation MAY advertise several
  (`"rev-001/1"`, `"rev-001/2"`); peers use the highest common one.
- REV numbers are never reused, including numbers of withdrawn
  extensions.

### 3.5 Extension index

| REV | Extension | Section | Protocol | Depends on |
|-----|-----------|---------|----------|------------|
| REV-001 | Persistent Agent Channel | [4](#4-persistent-agent-channel-rev-001) | rev-001/1 | — |
| REV-002 | Remote Terminal | [5](#5-remote-terminal-rev-002) | rev-002/1 | REV-001 |
| REV-003 | Live Status Stream | [6](#6-live-status-stream-rev-003) | rev-003/1 | REV-001 |
| REV-004 | Device Health & Status Journal | [7](#7-device-health--status-journal-rev-004) | rev-004/1 | REV-001 |
| REV-005 | Federation & Gateway | [8](#8-federation--gateway-rev-005) | rev-005/1 | REV-004 |
| REV-006 | Durability & Restore Verification | [9](#9-durability--restore-verification-rev-006) | — (server-internal) | REV-004, REV-005 |
| REV-007 | Packaging & Self-Hosting | [10](#10-packaging--self-hosting-rev-007) | — (server/agent-local) | REV-006 |
| REV-008 | Staged Rollouts | [11](#11-staged-rollouts-rev-008) | — (WFM-internal) | REV-001, REV-003, REV-004, REV-005 |
| REV-009 | Secrets | [12](#12-secrets-rev-009) | rev-009/1 | REV-001, REV-004, REV-005 |

Extensions with protocol `—` define no agent↔server wire protocol;
they are internal behavior and need no capability advertisement.
They still obey Section 3.1 wherever they touch a Margo surface.

### 3.6 Conformance

**Baseline test.** The Milestone 1 end-to-end loop (agent polls a
State Manifest, pulls the render bundle by digest, converges,
reports Margo-shaped status) MUST pass with ALL extensions compiled
out or disabled. This is the standing proof of Section 3.1: if any
extension becomes load-bearing for the base loop, the extension is
in violation and gets fixed, not the test.

**Enforcement.** The baseline test is a REQUIRED CI job — an
extensions-disabled build plus the e2e loop — that fails the
pipeline on regression. Conformance is a gate, not a convention.

**Wire-exactness.** Nothing here relaxes the WIRE-EXACT rule:
`reeve-types` and `margo-package` MUST parse real Margo artifacts
unmodified. reeve additive fields MUST round-trip through those
crates without disturbing Margo-defined fields (serde:
unknown-field-tolerant, extension fields optional).

**Surface audit.** Section 3.7 enumerates every touch on a Margo
surface. An extension that cannot be added to that table without
modifying a Margo-defined structure MUST NOT be implemented; the
conflict is escalated and resolved in spec first.

### 3.7 Margo surfaces touched — complete audit

The complete list of places any extension touches a Margo-defined
surface. Everything else in this document lives on reeve surfaces
vanilla tooling never sees.

| Extension | Surface | Touch | Why additive |
|-----------|---------|-------|--------------|
| framework (§3.3) | `DeviceCapabilitiesManifest.properties` | optional `reeve` object (advertisement) | one unknown optional key; no Margo field altered |
| REV-004 (§7.3) | `DeploymentStatusManifest` body | optional `reeve` object (`observedAt`, `seq`, `health`) | same envelope key; all Margo-required fields present, unchanged |
| REV-003 (§6) | Margo status ingest | read-only producer of `deployment-status` events | reads only; endpoint unmodified |
| REV-007 (§10.5) | `ApplicationDescription` format | consumer: example self-management package | a wire-exact artifact, not a format change |
| render bundle (§3.8 item 3, DECISIONS.md D2) | `ApplicationDescription` + `ApplicationDeployment` formats | consumer: wire-exact files emitted per app dir | artifacts emitted verbatim, not format changes |
| REV-008 (§11.3) | `DeploymentStatusManifest` state enum | read-only gate input | reads only |
| REV-009 (§12, DECISIONS.md D15) | `ApplicationDeployment` parameter values | value convention: a secret-typed parameter's `value` is the reference string `${secret:<name>}`, substituted agent-side at apply | syntactically valid per the pinned schema (parameter values are plain strings — DesiredState-001.yaml); no field added, renamed, or retyped; vanilla tooling sees a well-formed manifest |

Margo's device-gateway concepts (opaque/see-thru,
`device-capabilities.md` "Gateways considerations") are untouched by
all extensions and orthogonal to Section 8's management-plane tiers.
(reeve's v1 model does not yet implement hierarchical deviceIds or
gateway-relayed placement — recorded as known-unmodeled in
DECISIONS.md, not foreclosed.)

### 3.8 Margo surfaces replaced (not extended)

Additivity (3.1–3.7) governs everything reeve serves and emits.
Three Margo device-facing surfaces are REPLACED outright — reeve
neither serves nor extends them, and a vanilla Margo device client
therefore cannot enroll against reeve-server:

1. **Onboarding & Certificate API**
   (`POST /api/v1/onboarding`, `GET /onboarding/certificate`,
   `device-client-onboarding.md`): replaced by reeve enrollment —
   `POST /api/reeve/v1/enroll`, join token → device credential
   (DECISIONS.md D4).
2. **Device-API authentication** (X.509 client certificates + HTTP
   Message Signatures [RFC 9421],
   `api-requirements-and-security.md`): replaced by the
   enrollment-issued device credential (bearer token in v1; the
   Identity seam in DECISIONS.md D1 admits certificate/
   message-signature auth later). Wherever this document says
   "device credentials", it means the enrollment-issued credential,
   not Margo's certificate identity.
3. **Desired-state delivery** (`GET /api/v1/clients/{clientId}/
   deployments` returning `UnsignedAppStateManifest` +
   content-addressed ApplicationDeployment fetch,
   `workload-management-api-1.0.0.yaml`): replaced by reeve's State
   Manifest poll + OCI render-bundle pull (Section 10.2).
   REASSESSMENT (DECISIONS.md D13): this item is now
   PATTERN-FAITHFUL, no longer merely replaced-with-something-else —
   reeve deliberately adopts Margo's own Desired State API model:
   conditional GET with ETag as the manifest digest (RFC 9110 strong
   validator, `sha256:<hex>` grammar), strict `manifestVersion`
   monotonicity (Margo's anti-rollback check, adopted — regression
   is rejected and logged as a security event), and content-
   addressed immutable fetch of what the manifest references. It
   remains on this list only because the endpoint path, the payload
   envelope (State-Manifest-shaped but reeve-defined, carrying
   bundle digest + per-app `secrets_version`), and the
   authentication (item 2) are reeve's. The bundle still carries
   wire-exact Margo artifacts (ApplicationDescription and
   ApplicationDeployment files, DECISIONS.md D2).

What reeve still emits or ingests on Margo formats stays governed by
3.1–3.7 and audited in the 3.7 table: `DeploymentStatusManifest`
ingest keeps Margo's path and payload shape (Section 7.3) — its
authentication is the replaced credential of item 2. This list is
closed: replacing any further Margo surface requires amending this
section first (3.6 escalation rule applies).

## 4. Persistent Agent Channel (REV-001)

An OPTIONAL, outbound, persistent websocket from reeve-agent to
reeve-server providing: liveness as fact (open socket = reachable
*now*; drop = event), server→agent nudges that shorten convergence
latency without being required for correctness, and multiplexed
sub-channels for other extensions (the terminal, Section 5, is the
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
  credential — Section 3.8 item 2). Unknown or
  unauthenticated clients MUST be rejected before upgrade. One
  channel per device: a new authenticated channel atomically
  replaces the old one (old socket closed; crash-only, no draining).
- The agent MUST NOT attempt the channel unless the server
  advertises `rev-001/1` (Section 3.3). Upgrade failure is
  feature-unavailable: log once, keep polling, retry on the
  reconnect schedule (Section 4.5).

### 4.2 Message framing

All messages are either **control frames** — websocket text, one
JSON object with a `type` field; unknown `type` values MUST be
ignored (Section 3.4) — or **data frames** — websocket binary,
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
  `reject`, never by tearing down the channel (Section 3.2).
- Data frames for an id not accepted-and-open MUST be discarded
  silently (frames race `close`; crash-only tolerance).
- Sub-channels carry bytes; content, flow control, and lifecycle
  beyond open/close belong to the registering extension. Registered
  purposes: `rev-002/terminal` (Section 5).
- Channel teardown implicitly closes all sub-channels; extensions
  MUST treat sub-channel loss as a normal event, not corruption.
- Any extension needing bidirectional agent↔server bytes MUST use a
  sub-channel rather than a new listener or second websocket.
  (One-way server→UI events use SSE instead: Section 6.)

### 4.3 Liveness as fact

- An open channel means: this device was reachable at last
  ping/pong. The server MUST maintain per-device presence (`online`
  + since / `offline` + last-seen) from channel state alone, and
  MUST NOT infer "device dead" from a closed channel — only "link
  down" (Section 7.4 makes the device- vs link-degraded
  distinction).
- Either side SHOULD `ping` when idle past its keepalive interval
  (RECOMMENDED 30 s) and MUST treat a missing `pong` within a
  timeout (RECOMMENDED 10 s) as a dead channel: close the socket,
  emit the presence event.
- Presence transitions are published to the UI as
  `device-presence` events (Section 6.3).

### 4.4 Nudges — optimization, never replacement

- When desired state changes for a device (its manifestVersion
  advances — new render bundle or secrets_version,
  including via Section 11 wave advancement), the server SHOULD
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
- Features layered on the channel degrade per Section 3.2: no
  presence (UI falls back to last-report recency), no nudges
  (latency = poll interval), no terminal.
- The agent MUST NOT block startup, enrollment, or its first
  converge on channel establishment.

### 4.7 Security

- The channel reuses device-API authentication; it grants no
  authority beyond the device API EXCEPT as a carrier for
  sub-channels, whose purposes carry their own authorization rules
  (the terminal's, Section 5, are strict).
- Nudges disclose "something changed" timing, which devices already
  learn by polling; `hint` MUST NOT carry secret material.
- A malicious agent with valid credentials can hold a socket open;
  resource limits SHOULD be enforced (max frame size RECOMMENDED
  1 MiB; per-device sub-channel cap RECOMMENDED 16).

## 5. Remote Terminal (REV-002)

On-demand terminal sessions from the reeve UI to a device, carried
over a Section 4 sub-channel (purpose `rev-002/terminal`). Disabled
by default, enabled only through desired state, short-lived, fully
audited, relayed by reeve-server as opaque bytes. The guardrails in
Sections 5.2–5.5 are MUST-level and are summarized in CLAUDE.md
("Remote terminal (guardrails)"); this section is the authoritative
text. Because a remote shell is the highest-privilege capability in
the system, this section is written guardrails-first: enablement,
lifecycle, and audit are the spec; the byte plumbing is deliberately
trivial.

Terms: **session** — one interactive terminal, initiation to close,
identified by a server-assigned `sessionId`; **bridge** — the
reeve-server component splicing the UI leg to the agent leg;
**enablement** — the per-device desired-state configuration that
permits the agent to accept terminal sub-channels at all.

### 5.1 Transport

- Agent leg: a Section 4 sub-channel, purpose `rev-002/terminal`,
  opened by the server (even id) when a session is authorized.
  Framing and open/accept/reject/close semantics are Section 4.2's,
  not restated here.
- UI leg: a websocket `GET /api/reeve/v1/terminal/{sessionId}` —
  the one genuinely bidirectional UI surface, hence a websocket and
  not SSE (Section 6).
- Sub-channel `open.meta` carries only session bootstrap:
  `sessionId`, requested PTY size, TERM string. Resize and control
  ride in-band in the sub-channel payload; the format is
  agent-owned and the server does not parse it (Section 5.5).
- No channel, no terminal: if the device's channel is down, session
  initiation MUST fail immediately with "device offline". There is
  no queueing of session requests.

### 5.2 Enablement — desired state only

- Terminal access is DISABLED by default. A freshly enrolled device
  MUST refuse `rev-002/terminal` opens.
- Enablement is expressed ONLY in desired state: a configuration
  item in the device's render bundle (rendered through the overlay
  tree like any other config), so enabling the terminal is a tree
  revision — with an author and a diff (DECISIONS.md D13: revisions
  carry author/message/parent; diff is a query) — subject to the
  same review,
  history, and federation ownership rules (Section 8.4) as any
  other change.
- There MUST NOT be any runtime toggle: no API call, UI switch,
  environment variable, or channel message enables the terminal
  without a desired-state revision. reeve-server MUST additionally
  refuse to initiate sessions to devices whose rendered desired
  state does not enable the terminal (defense in depth: both sides
  check).
- The agent evaluates enablement from its last converged desired
  state — including while offline from the server (Law 5: last known
  state governs).
- Disablement is the same mechanism in reverse; on converging to a
  disabling commit the agent MUST terminate any live session.

### 5.3 Session lifecycle

- Sessions are explicitly initiated by an authenticated, authorized
  UI user. There are no standing or background sessions: nothing
  MAY auto-open, re-open after disconnect, or hold a session
  waiting for a device.
- Sessions are short-lived; both sides enforce limits. RECOMMENDED
  defaults: idle timeout 5 min, hard cap 60 min. Expiry closes the
  sub-channel and the UI websocket.
- Any leg failure (UI websocket drop, sub-channel close, channel
  loss, agent restart) closes the whole session. Reconnection is a
  new session with a new `sessionId` and a new audit record.
  `kill -9` of agent or server mid-session MUST leave nothing
  resumable — the PTY dies with its process; the audit record's
  accounting completes on next startup (Section 5.4; Law 3).
- The agent runs the PTY under the workload-execution identity
  configured in enablement, never as an unconstrained root shell by
  default.

### 5.4 Audit

- Every session MUST be recorded in reeve-server's SQLite DB:
  `sessionId`, initiating user, target device, requested/opened/
  closed at, close reason, and the enablement commit id in effect.
  Denied initiations (authorization failure, not enabled, device
  offline) MUST be recorded too.
- The audit record is written at initiation, BEFORE any bytes flow,
  and finalized at close. A server crash mid-session is finalized
  at next startup as `close reason = server-restart`.
- Audit records are irreplaceable data (Section 9.5).
- Lifecycle transitions are published as `terminal-session` events
  (Section 6.3) — metadata only, never content.

### 5.5 Bridge conduct

The bridge relays bytes only:

- It MUST NOT interpret, parse, transform, or filter session
  content in either direction (the resize/control encoding is
  agent-owned; the bridge sees opaque payload).
- It MUST NOT log session content, in plaintext or otherwise; only
  Section 5.4 metadata is recorded. Secrets typed into a session
  never touch server storage.
- It MUST NOT execute anything server-side on behalf of a session:
  no command helpers, no server-side shell, no recording/replay in
  rev-002/1.
- It MAY count bytes and enforce rate/size limits — accounting, not
  interpretation.

### 5.6 Security

- Authorization to initiate MUST be a distinct, auditable
  privilege, not implied by general UI login.
- The threat model assumes a compromised server should gain as
  little as possible: with no runtime toggle, a hostile server
  operator still needs a desired-state revision (visible, attributed,
  federated per Section 8.4) to open a terminal path to a device
  that has not enabled it.
- Timeouts (Section 5.3) bound the blast radius of a stolen UI
  session.

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
| `durability-lag` (PROPOSAL) | §9.3 | `generation`, `lastSeq`, `lagSeconds` | changeset upload lag crosses/clears a threshold — ops dashboard signal |
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

- **Link-degraded**: no presence (Section 4.3) and no fresh reports
  — but subsequent backfill shows healthy samples during the
  window. The device was fine; the path was not.
- **Device-degraded**: samples (live or backfilled) breaching
  health thresholds, or a journal gap mark, or a device that is
  present yet reporting unhealthy samples.
- **Unknown**: offline window not yet backfilled — MUST be surfaced
  as unknown, never silently assumed healthy or dead.

Transitions are published as `health-state` events (Section 6.3)
with `kind` = `device` | `link`. Reclassification after backfill
(unknown resolving to healthy or degraded) is normal and MUST
update history retroactively — exactly what original-timestamp
ingest makes possible. Rollout health gates consume this
classification (Section 11.4).

### 7.5 Federation

Under Section 8, a gateway tier journals what its local agents
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

## 8. Federation & Gateway (REV-005)

Multi-tier operation: a reeve-server MAY take an optional
`upstream`, in which case it syncs tree revisions from its parent,
renders and serves its local agents, and forwards journaled status
upstream. With no upstream it is the root — which is also the
air-gapped mode. Same binary, same UI, at every tier. Agents are
federation-blind. The load-bearing invariant: single writer per
overlay layer — federation replicates, it never merges.

Margo models gateways as devices fronting child devices
(`spec/margo/…/device-capabilities.md`, "Gateways considerations");
those roles are unchanged and orthogonal. reeve additionally needs
*management-plane* tiering: a site-local server that keeps a
factory converging when the WAN is down (Law 5 one tier up), and
air-gapped estates where "WAN down" is policy. Margo is silent on
WFM-to-WFM topology; the agent-facing surface at every tier remains
exactly the Margo device API plus advertised reeve extensions.

Terms: **tier** — one reeve-server in the hierarchy; **root** — a
server with no `upstream`; **gateway (tier)** — a server with one;
**layer** — one level of the desired-state overlay tree (fleet,
class, region, site, device, ...) as rendered by the
`desired-state` crate; **revision sync** — replication of the
parent's append-only revision stream such that the copy only ever
appends what the parent published, verbatim.

### 8.1 Topology

- Configuration is one optional value: `upstream` (URL +
  credentials for the parent tier). Present: sync tree revisions
  from the parent (§8.2), render and serve agents locally, journal
  status locally and forward it upstream (§8.3). Absent: root;
  air-gapped deployment is exactly this mode plus archive transfer
  (§8.5), not a special build.
- Same binary, same UI at every tier; a site operator at a gateway
  tier sees their slice of the fleet with full functionality.
- Depth is unbounded; each tier speaks rev-005/1 only to its
  immediate parent and children.

### 8.2 Revision sync (replaces git mirroring)

- A gateway tier MUST sync from its parent the tree revisions
  relevant to its scope and render locally — render is pure (D3),
  so renders are byte-identical at any tier.
- Protocol: the identical shape as device delivery (Section 10.2),
  one tier up — conditional GET on the parent's revision head
  (ETag), then content-addressed blob fetch for missing digests.
  Pull-based, resumable, idempotent by digest: a sync killed
  mid-transfer resumes by fetching what is still missing; a
  revision becomes visible locally only when its full closure is
  present (atomic; Law 3).
- The synced stream is append-only and IMMUTABLE at the gateway: a
  synced revision that disagrees with an already-held revision id
  (id/digest mismatch) MUST be surfaced as an error (UI, logs,
  Section 6 consumers) and MUST NOT be auto-resolved — it means the
  single-writer rule was violated somewhere, and hiding it would
  convert a spec violation into silent divergence. (This is the
  append-only successor of the old fast-forward-or-error rule.)
- Transport is authenticated with the gateway's tier credentials.

**PROPOSAL (per-tier revision model — confirm before implementation
relies on it):** each tier holds exactly ONE revision store
containing TWO streams: (a) the *upstream stream*, a verbatim
read-only copy of the parent's published revisions (hub-owned
layers: fleet/class/region), and (b) the *local stream*, the
gateway's own revisions for the layers it owns (its site + its
locally-enrolled device layers). Render input = (latest synced
upstream revision, latest local revision, device context); the
render-bundle manifest.yaml records BOTH revision ids (D2). Nothing
is ever written upward — status/journal is the only up-flow (§8.3).
Ownership (§8.4) becomes structural: the API refuses writes to
layer paths outside the tier's ownership set, and the upstream
stream is not writable at all. Divergence is impossible by
construction rather than detected after the fact; §8.2's error case
remains only for storage corruption or a misbehaving parent.

### 8.3 Status flow upstream

A gateway ingests local agents' status and journal records
(Section 7), then backfills its parent with the same protocol,
recursively: original timestamps, `(deviceId, seq)` idempotency,
late-ingest semantics unchanged at every hop. Upstream outage
buffers status at the gateway (bounded, gap-marked like any
journal); reconnection backfills; history at the root converges to
the same forensic record, later.

### 8.4 Ownership — single writer per layer

The MUST-level core of federation:

- Each overlay layer is authored by exactly ONE tier. Examples:
  cloud/root authors fleet and region layers; a gateway authors its
  own site layer only; device layers are authored by the tier the
  device enrolls against.
- A tier MUST NOT author a revision touching a layer it does not
  own — including the root: the root does not edit site layers
  owned by gateways. (Under the §8.2 PROPOSAL this is enforced
  structurally, not by convention.)
- Federation therefore only replicates, never merges. There is no
  conflict-resolution machinery in reeve, by design; every sync
  appends verbatim or errors (§8.2).
- Ownership applies to every desired-state change without exception
  — including terminal enablement (Section 5.2) and rollouts
  (Section 11.7), each authored at exactly one tier, propagating
  downward only.

### 8.5 Air-gap transfer

Where no network path exists between tiers:

- **Export**: revisions, packages, and images all export as signed
  OCI layout archives (oras/skopeo-compatible, inspectable with
  stock tooling) — ONE archive format for everything on the media
  (DECISIONS.md D13; git bundles are gone). Signing covers the
  archive index and content digests.
- **Import**: verifies signature and integrity, then appends
  revisions under §8.2 rules — verbatim append or error. A tampered
  or truncated archive MUST be rejected whole.
- **Return trip**: status exports journal records (Section 7.3
  form) for import at the parent, preserving original timestamps —
  sneakernet backfill.
- **Secrets** ride the same media encrypted to the destination
  gateway's public key (Section 12.5) — never plaintext on media.
- Export and import MUST be idempotent: importing the same archive
  twice is a no-op (Law 3 applied to sneakernet).

### 8.6 Agents are federation-blind

An agent enrolls against one server URL (manifest poll, artifact
pull, device API — all one origin). It MUST NOT need to know its
server's tier, and no rev-005 field appears in any agent-facing
payload. Moving a device between tiers is
re-enrollment, not reconfiguration of federation knowledge on the
device. Failure semantics are Law 5 unchanged: a gateway offline
from its upstream changes nothing for the gateway's local agents.

### 8.7 Security

- Tier credentials are scoped: a gateway can sync only the
  revisions/blobs in its scope and backfill only its subtree's
  devices; the parent MUST enforce that scope server-side. Scoped
  secret sync is further constrained per Section 12.5.
- Archive signing keys are tier identities; key distribution for
  air-gapped import is out of band by definition and MUST be
  documented per deployment (gateway keypairs minted at init,
  fingerprints verified at commissioning — Section 12.5).
- A compromised gateway can fabricate status for its subtree and
  author its own layers — but cannot alter layers it does not own,
  because upstream accepts no desired-state writes from below at
  all (data flows down; status flows up).
- Single-writer plus append-only revisions makes desired-state
  history at every tier an attributable chain — the property
  Section 5.6 leans on.

## 9. Durability & Restore Verification (REV-006)

How reeve-server's state survives the loss of the machine it runs
on. ALL server state — tree history (revision store), enrollment,
settings, audit, journal ingest, rollout state — is ONE SQLite
database (DECISIONS.md D13), made durable ENTIRELY IN-BINARY
(DECISIONS.md D16): a snapshot tier (minutes RPO, the generation
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
  enrollment, settings, secrets ciphertext (§12), audit and
  terminal session records (§5.4), status journal ingest (§7),
  rollout state (§11). The former parallel git-mirror/bundle
  durability path is DELETED — revision sync (§8.2) still gives
  every downstream tier a warm copy of the layers in its scope as a
  side effect, but it is no longer a durability mechanism this
  section depends on or maintains.
- Render bundles and other derived artifacts need no backup: they
  are reproducible from revisions (render is pure, D3) and
  re-materialize on demand.
- The secrets master key lives in a FILE OUTSIDE the DB
  (REEVE_DATA/secret.key — §12.2): snapshots ship ciphertext only,
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

Replaces the former litestream sidecar option (DECISIONS.md D16
records why: post-D15, WAL-frame replication was the sole remaining
plaintext escape past the keyfile, the only foreign process on the
durability path, and a second restore procedure verify-restore
didn't govern).

- The single writer connection (DECISIONS.md D6 — exactly what
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
  migrate → if migrated, snapshot → resume streaming (DECISIONS.md
  D6). A changeset sequence never spans a schema version.
- The server MAY publish upload lag (age of the last uploaded
  sequence) as a `durability-lag` event (§6.3 — marked PROPOSAL).

### 9.4 verify-restore (MUST)

- reeve-server MUST provide `reeve-server verify-restore` as a
  subcommand AND as a scheduled internal task
  (`durability.verify.interval`, RECOMMENDED default 24 h).
- A run MUST prove the WHOLE chain: download the latest
  generation's snapshot, decrypt, apply ALL its changesets in
  sequence order, open the result as SQLite (integrity check),
  assert the schema version is known to this binary, assert recency
  (last applied sequence age ≤ 2× the relevant interval, config),
  and record the result (when, which generation, last sequence,
  outcome, failure detail) in the live DB. One restore procedure
  for everything — there is no second path to rot.
- The result MUST be surfaced in the API and UI as "last verified
  restore: <when>", and published as a `verify-restore` event
  (§6.3). An unverified or stale-verified target is an
  operator-visible warning state.
- A deployment whose verify-restore has never succeeded MUST be
  treated (in UI/API status) as having NO durability tier, whatever
  the bucket contains.

### 9.5 Crash-only bootstrap, DR, and data value

- reeve-server starting with NO local database and a configured
  snapshot target MUST offer restore-from-latest as the startup
  path: fetch the latest generation, decrypt, replay changesets
  (§9.3), place the result as the local DB, run migrations
  idempotently (§10.1), continue as a normal start.
  Whether restore is automatic or requires a confirmation flag
  (`--restore-from-target`) is an implementation choice, but the
  path MUST exist and MUST be the documented DR procedure. Disaster
  recovery is therefore normal startup with one precondition
  removed — no runbook of special cases, no restore mode that rots
  untested. Tree history restores WITH the snapshot (it is in the
  same DB); render bundles re-materialize on demand.
- Secrets restore requires the keyfile too (§9.1, §12.2) — the DR
  procedure MUST state both artifacts.
- Data loss on restore is bounded by the tier's RPO — snapshot
  interval (minutes) on the snapshot tier, changeset interval
  (seconds) with the changeset tier enabled; agents' journals
  re-backfill everything journaled since the restore point (§7.3) —
  agent-side store-and-forward is itself part of the durability
  story.
- **CONFLICT FLAG + PROPOSAL (restore vs anti-rollback,
  unresolved):** restore-from-snapshot can resurrect a
  manifestVersion older than what devices have already seen inside
  the RPO window, and §10.2's strict monotonicity then makes every
  affected device reject the server's manifests as a rollback
  attack. PROPOSAL: manifestVersion is the pair
  `(epoch, counter)`, compared lexicographically; a tiny epoch
  marker lives AT THE SNAPSHOT TARGET (not in the DB) and every
  restore-from-snapshot increments it, so a restored server's
  manifests always compare strictly greater. Devices treat an epoch
  bump as legitimate; a counter regression within an epoch remains
  a security event. Confirm before implementing either side.

Data-value analysis — what the DB holds, split by fate on loss:

| Data | On loss | Why |
|------|---------|-----|
| Status journal ingest (§7) | **Reconstructible** | agents re-backfill from device journals with original timestamps; history converges again (bounded by agent retention) |
| Device capabilities cache | **Reconstructible** | devices MUST re-send on change per Margo; next report repopulates |
| Tree history (revision store, D13) | **IRREPLACEABLE** | the config source of truth and its full attributable history; now snapshot-covered like everything else |
| Render bundles / derived artifacts | **Reconstructible** | re-rendered from revisions (pure render, D3) |
| Presence / derived health | **Reconstructible** | recomputed from journal + channel state |
| Enrollment (device identity ↔ credential, §3.8) | **IRREPLACEABLE** | losing it orphans every device; re-enrollment is manual, fleet-wide toil |
| Settings | **IRREPLACEABLE** | operator intent, recorded nowhere else (files hold shape, DB holds values) |
| Secrets (ciphertext, §12) | **IRREPLACEABLE** (with keyfile) | operator-entered values; ciphertext useless without REEVE_DATA/secret.key — back both up |
| Audit + terminal session records (§5.4) | **IRREPLACEABLE** | forensic record; by definition cannot be regenerated |
| Rollout state/history (§11) | **IRREPLACEABLE in flight** | a lost in-flight rollout's position must not be guessed; history is audit-like |

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
  construction: the master key lives outside the DB (§12.2), so a
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
- Terminal audit records are metadata only (§5.5 keeps content out
  of the DB), so snapshots cannot leak session content by
  construction.
- Pruning is destructive; credentials that can prune MUST NOT be
  able to rewrite existing objects (versioned or write-once-keyed
  layout RECOMMENDED) so a compromised server cannot silently
  corrupt history it already shipped.

## 10. Packaging & Self-Hosting (REV-007)

How reeve ships and installs itself: one static binary per role
embedding everything it needs; desired state delivered via a State
Manifest poll + native read-only OCI pull, in-process, on one
listening socket; idempotent self-install for both roles; an
optional `/install` bootstrap endpoint serving version-coherent
agent binaries; agent self-management through the normal
desired-state tree with A/B binary safety.

reeve targets boxes and sites where "install the platform first" is
the failure mode being replaced. The goal: one file, one port, one
command — and the agent's update path is the same desired-state
mechanism the agent exists to apply. Distribution is outside
Margo's scope, with one wire-adjacent constraint: the /v2 routes
MUST speak standard OCI distribution pull so stock clients (oras,
skopeo, crane) remain debugging tools, not casualties.

### 10.1 Single static binary

- Each role (`reeve-server`, `reeve-agent`) ships as ONE static
  binary (musl) for `x86_64` and `aarch64` Linux. No shared-library
  dependencies; a binary copied to a bare box runs.
- The reeve-server binary embeds:
  - UI dist (rust-embed), served by path with index.html fallback
    for SPA deep links (CLAUDE.md "ui/").
  - SQL migrations, run idempotently at EVERY startup — startup IS
    recovery (Law 3); no separate migrate step, and a half-migrated
    database from a mid-migration `kill -9` MUST be resumable
    (transactional, versioned migrations).
  - `openapi.json` (the document the UI client is generated from),
    served at a stable path.
  - Default configuration (files hold shape; values live in the DB
    per Law 4 — the embedded default is the shape).
  - Shell completions (`--completions <shell>`).
- Both binaries MUST support `--spec`, printing this specification
  (embedded at build) — the deployed artifact carries its own
  contract.
- Version output MUST include the workspace git revision (§10.4
  depends on it).

### 10.2 Desired-state delivery: State Manifest poll + native OCI pull

(v2 of this section — v1 was git over smart HTTP; git is removed
from the runtime by DECISIONS.md D13.)

- **Manifest poll**: `GET /api/reeve/v1/manifest` (device-scoped by
  its credential) returns the device's State-Manifest-shaped JSON:
  `manifestVersion`, render-bundle digest + pull URL, per-app
  `secrets_version` (§12). Conditional GET: the ETag is the
  manifest digest, an RFC 9110 strong validator with the digest
  grammar `sha256:<hex>`; `If-None-Match` match returns 304. This
  models Margo's Desired State API (§3.8 item 3 reassessment).
- **Anti-rollback (adopted from Margo)**: the agent MUST enforce
  strict `manifestVersion` monotonicity — a regression is rejected
  and logged as a security event, and the agent continues from last
  known state (Law 5). See §9.5's flagged restore interaction.
- **Artifact pull**: the render bundle, vendored app packages, and
  agent binaries are OCI artifacts served natively, read-only
  (GET manifest / GET blob by digest, standard OCI distribution
  routes under `/v2/`; DECISIONS.md D7) through the same axum
  stack. No push routes exist — desired state is written only by
  the rendering pipeline (§8.4). Pull is content-addressed and
  immutable: verify digest, unpack to temp, atomic dir swap,
  converge (D5).
- Same port as everything else: UI, REST API, SSE (§6), websockets
  (§4, §5), native artifact /v2 routes, and — when the registry
  sidecar is deployed — the image proxy on the same /v2 space
  (DECISIONS.md D8). ONE listening socket total per server. No
  path shadows Margo's `/api/v1/…`.
- Wire behavior on /v2 MUST be standard OCI distribution pull:
  stock clients (oras, skopeo, crane) MUST fetch reeve's artifacts
  successfully — tested against stock tooling, not just our own
  client.
- Auth: the same device credential as the device API authorizes a
  device to poll exactly its own manifest and pull exactly the
  artifacts its manifest references; tier credentials (§8)
  authorize a gateway's revision sync scope. Anonymous pull MUST
  NOT be enabled by default.

### 10.3 Self-install

- `reeve-agent install` MUST: create its system user, write its
  systemd unit (via the same unit-emitting machinery as the
  systemd-unit Provider), write config shape, enable and start
  itself. `reeve-agent uninstall` reverses it.
- `reeve-server init` MUST emit deployment artifacts for the
  operator's chosen substrate: a compose file, or systemd unit
  files, and the zot configuration when the registry profile is
  selected (D9). Durability needs no sidecar and therefore no
  emitted config — both tiers are in-binary, env/config-selected
  (§9). init also creates the secrets master keyfile
  (REEVE_DATA/secret.key, 0600) and MUST warn that the keyfile
  needs separate backup (§12.2).
- Both commands MUST be idempotent — Law 3 applies to installers:
  re-running `install` on a half-installed box (killed mid-install)
  converges to installed; it never errors on "already exists" and
  never duplicates units or users.
- Installers write config and units; they MUST NOT bake secrets
  into world-readable files (units reference credential files with
  appropriate modes).

### 10.4 `/install` bootstrap endpoint

- Behind the cargo feature `embedded-agents`, reeve-server embeds
  the reeve-agent binaries for BOTH architectures and serves them
  as OCI artifacts on the native /v2 routes ("the agent is an
  artifact", DECISIONS.md D7), plus `GET /install` (a shell
  installer script).
- The script detects the machine architecture, pulls the matching
  agent artifact by digest FROM THE SERVER IT WILL ENROLL AGAINST
  (digests baked into the script at build), and runs `reeve-agent
  install` pointed at that same server. One origin for trust,
  transport, and enrollment:
  `curl -fsSL https://reeve.site.example/install | sh` is the whole
  bootstrap.
- Version coherence: served agent binaries MUST be built from the
  same workspace revision as the serving server (enforced at build;
  the feature does not admit mixing).
- The endpoint requires an enrollment credential/token by default;
  a deployment MAY open it on trusted networks (config). Without
  the feature the route is absent (404) — invisible, per §3.1
  rule 4.

### 10.5 Self-management — the agent as a workload

- The repository ships an example Margo-shaped application package
  (an `ApplicationDescription` valid per `margo-package`) for
  reeve-agent itself — the agent binary it references is the OCI
  artifact of §10.4 — so agent updates flow through the normal
  desired-state tree: authored at one tier, staged via Section 11,
  converged like any other workload. No side-band updater exists.
- Update mechanics: rename-and-exec or service restart, A/B on the
  binary path — install the new binary beside the old, atomically
  swap a symlink (or equivalent rename), restart the unit.
- A failed self-update MUST leave the previous binary running: the
  swap is the last step; a new binary failing its first health
  window is rolled back to the retained previous binary by the
  supervising unit's failure handling. `kill -9` at any point
  leaves either old-running or new-running — never neither (Law 3).
- The agent reports its version in status (§7.2, §3.3), which is
  how a staged agent rollout's health gates observe success.

### 10.6 deploy/

`deploy/` holds ONLY what the binaries cannot emit about
themselves: Kubernetes manifests wrapping the same artifacts for
operators who insist, and CI examples (musl build matrix,
embedding, release). Anything `reeve-server init` or `reeve-agent
install` can emit MUST NOT be duplicated as a checked-in file that
can drift — with ONE named exception: `deploy/compose.yml`, the
canonical tier-agnostic compose file (DECISIONS.md D9). `init`
emits a copy/variant of it and CI MUST keep the two in sync.

### 10.7 Security

- `curl | sh` bootstrap is trust-on-first-use of the server's TLS
  identity; the digest check binds binary to script but both come
  from the same origin. Deployments needing stronger provenance pin
  the server certificate or distribute the agent out of band — both
  remain supported since `/install` is optional.
- The native artifact routes expose desired state, which includes
  config: per-device authorization (a device pulls only what its
  own manifest references) is the confidentiality boundary and MUST
  be enforced on every /v2 route, including manifest GETs.
- Pull-only transport (no push routes exist) removes an entire
  class of write-path attacks against desired state.
- Self-update executes a binary the server delivered; the
  desired-state commit that triggered it is attributable (§8.4),
  and A/B retention bounds the damage of a bad (not malicious)
  binary. A malicious server is already the agent's root of trust —
  established at enrollment (Section 3.8 item 1), the same
  trust-the-server-you-enrolled-with posture as Margo's onboarding
  model (`spec/margo/…/device-client-onboarding.md`).

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

## 12. Secrets (REV-009)

Secrets are desired state BY REFERENCE, never by value
(DECISIONS.md D15, the deciding document — this section is the
normative wire/behavior summary). Tree content and rendered
artifacts carry `${secret:<name>}` references; values NEVER enter
the config plane: no plaintext in revisions, renders, bundles,
snapshots, revision sync, or air-gap media — by construction, since
those artifacts only ever contain references.

### 12.1 Scoping and rendering

- Secrets are defined at layers and resolve down the same chain as
  config (fleet -> class -> region -> site -> device, deeper wins).
- Resolution is SERVER-SIDE AT REQUEST TIME — never at render time.
  Render stays pure (D3) and bundles stay secret-free. A
  secret-typed parameter inside the wire-exact ApplicationDeployment
  carries the reference string as its value (audited in §3.7).

### 12.2 Storage

- Secrets live in a table in reeve-server's SQLite, AEAD-encrypted
  under a master key in a FILE OUTSIDE the DB (REEVE_DATA/
  secret.key, 0600, created at init). Snapshots therefore ship
  ciphertext only; restore = snapshot + keyfile (§9.1, §9.5).
  `reeve-server init` MUST warn that the keyfile needs separate
  backup (§10.3).
- The same store holds the server's own operational secrets (zot
  upstream credentials, S3 keys, tier tokens).
- UI: secrets are write-only after entry — set, rotate, view
  metadata (name, scope, version, last-rotated); never read back.

### 12.3 Delivery (rev-009/1)

- At apply time the agent calls
  `POST /api/reeve/v1/secrets/resolve` with its device credential
  (D1 provision-once): a device can only ask as itself and receives
  only its own resolution. Plaintext exists in exactly three
  places, ever: server RAM during resolve, TLS in flight, and the
  device's env files at rest (0600, temp+rename, agent-local,
  OUTSIDE the hashed bundle dir — the honest v1 trade for Law 5
  reboot-while-offline; FDE recommended in deployment docs).
- Service-level scoping rides Margo's own primitive: parameter
  `targets` declare `components: []`, and the agent materializes
  env PER SERVICE (`apps/<name>/env/<service>.env`, only the values
  targeted at that component); rendered compose references them via
  `env_file`. Compose recreates only services whose resolved config
  changed, so a rotation bounces exactly the consuming services
  (Law 4: restart semantics delegated to compose's own diff).
- Offline devices apply from last materialized env files (Law 5);
  the resolve endpoint being unreachable never blocks convergence
  of already-resolved apps.

### 12.4 Rotation and propagation

- Rotating a secret bumps its version => affected devices' per-app
  `secrets_version` (hash of resolved secret names+versions, never
  values) changes in the State Manifest => manifestVersion bumps =>
  REV-001 nudge says "poll now" (§4.4).
- Agent diff: bundle digest unchanged + secrets_version changed =>
  re-resolve, rewrite only env files whose content differs,
  `up -d` affected apps. No bundle re-pull. Offline devices catch
  the same rotation on next poll (nudge = optimization, never
  correctness).
- Rotation state is published as `secret-rotation` events (§6.3).
- Coordinated rotation across apps/devices is explicitly NOT
  decided (DECISIONS.md NOT-decided list) — implementations MUST
  NOT improvise it.

### 12.5 Federation and air-gap

- The hub syncs DOWN to each gateway only the secrets resolvable
  within that gateway's subtree, over the tier channel,
  RE-ENCRYPTED under the gateway's own local master key (per-tier
  keys: a stolen snapshot from one tier + another tier's key yields
  nothing). Gateways serve cached scoped secrets through WAN
  outages; rotations queue and land on reconnect (§8.3 pattern).
- Air-gap: secret sets export encrypted TO THE DESTINATION
  GATEWAY'S PUBLIC KEY (each gateway mints a keypair at init;
  fingerprint verified out-of-band at commissioning). Never
  plaintext on media (§8.5).

### 12.6 Security

- The resolve endpoint is the single plaintext egress; it MUST be
  scoped to the requesting device's own resolution, rate-limited,
  and audit-countable (who resolved what version when — metadata,
  not values).
- A compromised device learns exactly the secrets targeted at its
  own apps' components — the minimum a running workload must know
  anyway. A compromised gateway learns its subtree's scoped set,
  never the fleet's.
- Env files at rest on devices are the accepted v1 residue; their
  scope is bounded per service by §12.3.

## 13. References

- [RFC2119] Bradner, S., "Key words for use in RFCs to Indicate
  Requirement Levels", BCP 14, RFC 2119.
- [RFC8174] Leiba, B., "Ambiguity of Uppercase vs Lowercase in RFC
  2119 Key Words", BCP 14, RFC 8174.
- [RFC6455] Fette, I. and A. Melnikov, "The WebSocket Protocol".
- [RFC9110] Fielding, R., et al., "HTTP Semantics" — ETag /
  conditional requests (§10.2).
- [SSE] WHATWG HTML Living Standard, "Server-sent events".
- OCI Distribution Specification — pull routes (§10.2, DECISIONS.md
  D7/D13); OCI Image Layout — air-gap archives (§8.5).
- `spec/margo/system-design/specification/margo-management-interface/`
  — device-client-onboarding.md, device-capabilities.md,
  deployment-status.md, workload-management-api-1.0.0.yaml (the
  Margo surfaces guarded by Section 3; the Desired State API
  modeled by §10.2).
- SQLite documentation — `VACUUM INTO`, WAL mode, and the session
  extension (changeset capture/apply, §9.3).
- CLAUDE.md — The Five Laws; "Spec fidelity — where the line is";
  "Remote terminal (guardrails)" (summary of Section 5); "ui/".
- DECISIONS.md — D7/D8 (artifact serving, registry), D12
  (labels/class/pins), D13 (revision store + OCI delivery; the git
  removal rationale, incl. Margo WG decision tracker issue #22),
  D14 (authoring API), D15 (secrets), D16 (in-binary changeset
  streaming; the litestream removal rationale).
