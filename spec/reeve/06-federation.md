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

**Per-tier revision model (normative):** each tier holds exactly
ONE revision store containing TWO streams: (a) the *upstream
stream*, a verbatim read-only copy of the parent's published
revisions (hub-owned layers: fleet/class/region), and (b) the
*local stream*, the gateway's own revisions for the layers it owns
(its site + its locally-enrolled device layers). Render input =
(latest synced upstream revision, latest local revision, device
context); the render-bundle manifest.yaml records BOTH revision ids
(D2). Nothing is ever written upward — status/journal is the only
up-flow (§8.3). Ownership (§8.4) is structural: the API MUST refuse
writes to layer paths outside the tier's ownership set, and the
upstream stream MUST NOT be writable at all. Divergence is
impossible by construction rather than detected after the fact;
§8.2's error case remains only for storage corruption or a
misbehaving parent.

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
  owned by gateways. Enforced structurally per §8.2's two-stream
  model, not by convention.
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

