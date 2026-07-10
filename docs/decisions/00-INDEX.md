# DECISIONS.md — reeve implementation decisions (pre-Milestone 1)

Principles applied throughout: minimal sidecars, maximum cohesion,
migrations built in, atomic-or-absent writes, idempotent everything,
simplicity, explainability (every choice survives the layman test).
These are DECIDED. Agents: do not relitigate; propose changes to a
human, don't improvise them.

## Explicitly NOT decided yet (do not improvise)
- RBAC beyond admin/operator/viewer; mTLS device certs / RFC 9421
  message signatures replacing bearer tokens (the D1 extractor seam
  holds the door open; the v1 divergence is recorded in D1 and
  SPEC §3.8).
- Rollout gate thresholds and cohort selection UX (REV-008 gives
  semantics; numbers come from using it).
- SSE event types and payload fields BEYOND SPEC §6.3's table —
  §6.3 is decided and governs; only additions are deferred.
- Settings envelopes (formerly envelope/settings.schema.yaml in the
  D2 layout): purpose, format, producer, and consumer all TBD. Not
  part of the rendered-repo contract until decided.
- Margo see-thru gateway support (hierarchical deviceIds,
  gateway/* placement): known-unmodeled. reeve's flat
  device=bundle=box model doesn't foreclose it, but supporting it
  means growing the deviceId/desired-state model — a future
  decision, not an implied capability.
- Cohort selector syntax/UX, operator taxonomies, multi-class
  devices: see D12 (multi-class REFUSED, not merely deferred).
- Coordinated secret rotation across apps/devices (e.g. MQTT broker
  + its clients that cannot flip in the same instant —
  overlap-validity windows or dual-valid versions until dependents
  report converged via REV-004). Do NOT improvise this; no "restart
  everything simultaneously" is acceptable as rotation semantics
  (D15).

