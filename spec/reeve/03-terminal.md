# reeve spec — Remote Terminal (REV-002)

Part of the reeve specification; start at [00-INDEX.md](00-INDEX.md).

## 5. Remote Terminal (REV-002)

On-demand terminal sessions from the reeve UI to a device, carried
over a 02-channel Section 4 sub-channel (purpose `rev-002/terminal`). Disabled
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

- Agent leg: a 02-channel Section 4 sub-channel, purpose `rev-002/terminal`,
  opened by the server (even id) when a session is authorized.
  Framing and open/accept/reject/close semantics are 02-channel Section 4.2's,
  not restated here.
- UI leg: a websocket `GET /api/reeve/v1/terminal/{sessionId}` —
  the one genuinely bidirectional UI surface, hence a websocket and
  not SSE (04-status-stream Section 6).
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
  revision — with an author and a diff (docs/decisions/delivery.md D13: revisions
  carry author/message/parent; diff is a query) — subject to the
  same review,
  history, and federation ownership rules (06-federation Section 8.4) as any
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
- Audit records are irreplaceable data (07-durability Section 9.5).
- Lifecycle transitions are published as `terminal-session` events
  (04-status-stream Section 6.3) — metadata only, never content.

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
  federated per 06-federation Section 8.4) to open a terminal path to a device
  that has not enabled it.
- Timeouts (Section 5.3) bound the blast radius of a stolen UI
  session.

