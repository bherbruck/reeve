# reeve spec — Extension Framework & Conformance (REV-000)

Part of the reeve specification; start at [00-INDEX.md](00-INDEX.md).

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
  NOT depend on any extension (Law 5; 02-channel Section 4.6).
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
| REV-004 (05-health-journal §7.3) | `DeploymentStatusManifest` body | optional `reeve` object (`observedAt`, `seq`, `health`) | same envelope key; all Margo-required fields present, unchanged |
| REV-003 (04-status-stream §6) | Margo status ingest | read-only producer of `deployment-status` events | reads only; endpoint unmodified |
| REV-007 (08-packaging §10.5) | `ApplicationDescription` format | consumer: example self-management package | a wire-exact artifact, not a format change |
| render bundle (§3.8 item 3, docs/decisions/tree-render.md D2) | `ApplicationDescription` + `ApplicationDeployment` formats | consumer: wire-exact files emitted per app dir | artifacts emitted verbatim, not format changes |
| REV-008 (09-rollouts §11.3) | `DeploymentStatusManifest` state enum | read-only gate input | reads only |
| REV-009 (10-secrets §12, docs/decisions/secrets.md D15) | `ApplicationDeployment` parameter values | value convention: a secret-typed parameter's `value` is the reference string `${secret:<name>}`, substituted agent-side at apply | syntactically valid per the pinned schema (parameter values are plain strings — DesiredState-001.yaml); no field added, renamed, or retyped; vanilla tooling sees a well-formed manifest |

Margo's device-gateway concepts (opaque/see-thru,
`device-capabilities.md` "Gateways considerations") are untouched by
all extensions and orthogonal to 06-federation Section 8's management-plane tiers.
(reeve's v1 model does not yet implement hierarchical deviceIds or
gateway-relayed placement — recorded as known-unmodeled in
docs/decisions/00-INDEX.md, not foreclosed.)

### 3.8 Margo surfaces replaced (not extended)

Additivity (3.1–3.7) governs everything reeve serves and emits.
Three Margo device-facing surfaces are REPLACED outright — reeve
neither serves nor extends them, and a vanilla Margo device client
therefore cannot enroll against reeve-server:

1. **Onboarding & Certificate API**
   (`POST /api/v1/onboarding`, `GET /onboarding/certificate`,
   `device-client-onboarding.md`): replaced by reeve enrollment —
   `POST /api/reeve/v1/enroll`, join token → device credential
   (docs/decisions/agent.md D4).
2. **Device-API authentication** (X.509 client certificates + HTTP
   Message Signatures [RFC 9421],
   `api-requirements-and-security.md`): replaced by the
   enrollment-issued device credential (bearer token in v1; the
   Identity seam in docs/decisions/auth.md D1 admits certificate/
   message-signature auth later). Wherever this document says
   "device credentials", it means the enrollment-issued credential,
   not Margo's certificate identity.
3. **Desired-state delivery** (`GET /api/v1/clients/{clientId}/
   deployments` returning `UnsignedAppStateManifest` +
   content-addressed ApplicationDeployment fetch,
   `workload-management-api-1.0.0.yaml`): replaced by reeve's State
   Manifest poll + OCI render-bundle pull (08-packaging Section 10.2).
   REASSESSMENT (docs/decisions/delivery.md D13): this item is now
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
   ApplicationDeployment files, docs/decisions/tree-render.md D2).

What reeve still emits or ingests on Margo formats stays governed by
3.1–3.7 and audited in the 3.7 table: `DeploymentStatusManifest`
ingest keeps Margo's path and payload shape (05-health-journal Section 7.3) — its
authentication is the replaced credential of item 2. This list is
closed: replacing any further Margo surface requires amending this
section first (3.6 escalation rule applies).

