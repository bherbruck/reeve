# The reeve Specification

```
Status:       Stable
Scope:        All reeve extensions to the pinned Margo specification
Margo pin:    spec/margo/ (submodule commit is authoritative)
Extensions:   REV-001 .. REV-011 (index in 01-framework Section 3.5)
```

## Abstract

reeve implements the Margo specification pinned at `spec/margo/` and
extends it where Margo is silent (offline behavior, desired-state
derivation, federation) or where reeve needs capabilities Margo does
not define (remote terminal, health journal, staged rollouts). This
document is the complete reeve extension specification. 01-framework Section 3 is
its load-bearing clause: every extension is additive, and vanilla
Margo tooling that knows nothing of this document MUST interoperate
unmodified with every Margo surface reeve serves or emits. reeve
also REPLACES three Margo device-facing surfaces outright
(onboarding, device-API auth, desired-state delivery); 01-framework Section 3.8
enumerates them — replacement is declared there, never implied
elsewhere. Sections 4–12 define the extensions themselves,
identified REV-001 through REV-009; REV-010 (Section 11) adds the
operator-facing fleet model over the same storage engine, and REV-011
(05-health-journal §7.7) adds per-deployment compose-log capture.

## 1. Introduction

The founding constraint, restated from CLAUDE.md's "Spec fidelity"
rules: anything that crosses the wire or lives in a file another
Margo tool might read is spec-exact; everything else is ours. This
document turns that rule of thumb into normative requirements
(01-framework Section 3) and then spends the rest of its length in the "ours"
column: capabilities layered over Margo in ways vanilla tooling can
ignore entirely.

Recurring context, cited throughout as "Law N" (CLAUDE.md, The Five
Laws): Law 3 — crash-only, startup IS recovery; Law 4 — state lives
in engines with someone else's test suite (SQLite, including the
content-addressed revision store); Law 5 — the agent assumes it is
offline more than online, polling, outbound-only, NAT-native.

State/delivery taxonomy (docs/decisions/delivery.md D13): the per-device State
Manifest is the ONE mutable coordinating document; everything it
points to is immutable (code: OCI images/packages; config: the
render-bundle digest) or authenticated-on-demand (secrets: the
resolve endpoint, 10-secrets Section 12). Settings, when decided, are expected
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
  own artifacts, plus the OPTIONAL image proxy to zot; 08-packaging Section 10.2,
  docs/decisions/delivery.md D7/D8). All are invisible to vanilla Margo tooling
  and none shadows a Margo path.
- **Revision store**: the content-addressed history of the overlay
  tree — sha256 blobs + append-only revisions — in reeve-server's
  SQLite (docs/decisions/delivery.md D13). Git does not exist in the runtime.
- **State Manifest / render bundle**: the per-device desired-state
  pointer document (manifestVersion, bundle digest, per-app
  secrets_version) and the OCI artifact it references (the rendered
  D2 layout), 08-packaging Section 10.2.
- **Channel / sub-channel / nudge / presence**: see 02-channel Section 4.
- **Journal / backfill / original timestamp**: see 05-health-journal Section 7.
- **Tier / root / gateway (tier) / layer**: see 06-federation Section 8.
- **Snapshot / target / RPO / verify-restore**: see 07-durability Section 9.
- **Rollout / cohort / wave / gate**: see 09-rollouts Section 11.
- **Secret reference / secrets_version / resolve**: see 10-secrets Section 12.

## File Map & Reading Order

The specification is split per concern; section numbers (§1–§13),
REV identifiers, and D-numbers are stable anchors that survive file
moves. Read [01-framework.md](01-framework.md) first — it governs
everything else. The authoritative per-extension index (status,
protocol versions, dependencies) is its §3.5.

| File | Contents | Sections |
|------|----------|----------|
| [00-INDEX.md](00-INDEX.md) | abstract, conventions, terminology, references | §1, §2, §13 |
| [01-framework.md](01-framework.md) | additivity, discovery, versioning, conformance, surface audit, replaced surfaces | §3 (REV-000) |
| [02-channel.md](02-channel.md) | persistent agent channel | §4 (REV-001) |
| [03-terminal.md](03-terminal.md) | remote terminal | §5 (REV-002) |
| [04-status-stream.md](04-status-stream.md) | SSE live status stream | §6 (REV-003) |
| [05-health-journal.md](05-health-journal.md) | device health & status journal; deploy-log capture | §7 (REV-004), §7.7 (REV-011) |
| [06-federation.md](06-federation.md) | federation & gateway, per-tier revision model | §8 (REV-005) |
| [07-durability.md](07-durability.md) | durability, epoch fencing, verify-restore | §9 (REV-006) |
| [08-packaging.md](08-packaging.md) | packaging & self-hosting, delivery endpoints | §10 (REV-007) |
| [09-rollouts.md](09-rollouts.md) | staged rollouts, convergence target | §11 (REV-008) |
| [10-secrets.md](10-secrets.md) | secrets | §12 (REV-009) |
| [11-fleet-model.md](11-fleet-model.md) | operator fleet model: hierarchy, device management, deploy-to-scope, History/Undo, server tier | §11 (REV-010) |

House style for every file in this directory: RFC/IETF register,
BCP 14 (RFC 2119/8174) requirement keywords, under 1000 lines per
file, original section numbering preserved, cross-references
explicit ("06-federation §8.2" or a relative link).

Register boundary: `spec/reeve/` holds only what an independent
implementation would need to conform — wire shapes, protocol
semantics, normative behavior. Build choices (crates, sidecars,
tool picks, trade-offs) live in `docs/decisions/`; spec text may
cross-reference a D-number but MUST NOT normatively depend on a
crate or tool name, and wire-behavior MUSTs MUST NOT live only in a
decision file.

## 13. References

- [RFC2119] Bradner, S., "Key words for use in RFCs to Indicate
  Requirement Levels", BCP 14, RFC 2119.
- [RFC8174] Leiba, B., "Ambiguity of Uppercase vs Lowercase in RFC
  2119 Key Words", BCP 14, RFC 8174.
- [RFC6455] Fette, I. and A. Melnikov, "The WebSocket Protocol".
- [RFC9110] Fielding, R., et al., "HTTP Semantics" — ETag /
  conditional requests (08-packaging §10.2).
- [SSE] WHATWG HTML Living Standard, "Server-sent events".
- OCI Distribution Specification — pull routes (08-packaging §10.2, docs/decisions/delivery.md
  D7/D13); OCI Image Layout — air-gap archives (06-federation §8.5).
- `spec/margo/system-design/specification/margo-management-interface/`
  — device-client-onboarding.md, device-capabilities.md,
  deployment-status.md, workload-management-api-1.0.0.yaml (the
  Margo surfaces guarded by 01-framework Section 3; the Desired State API
  modeled by 08-packaging §10.2).
- SQLite documentation — `VACUUM INTO`, WAL mode, and the session
  extension (changeset capture/apply, 07-durability §9.3).
- CLAUDE.md — The Five Laws; "Spec fidelity — where the line is";
  "Remote terminal (guardrails)" (summary of 03-terminal Section 5); "ui/".
- docs/decisions/ — D7/D8 (delivery.md: artifact serving, registry), D12
  (labels/class/pins), D13 (revision store + OCI delivery; the git
  removal rationale, incl. Margo WG decision tracker issue #22),
  D14 (authoring API), D15 (secrets), D16 (in-binary changeset
  streaming; the litestream removal rationale).
