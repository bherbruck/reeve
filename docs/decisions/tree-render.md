# reeve decisions — Tree & Render (D2, D3, D11, D12)

Part of docs/decisions/; start at [00-INDEX.md](00-INDEX.md).

## D2. Rendered bundle layout (the agent's wire contract)
The render bundle is an OCI artifact (D13), pulled by digest and
unpacked to a temp dir + atomic dir swap. Layout inside the bundle:

    manifest.yaml                 # render provenance (see D3 rules)
    apps/
      <app-name>/
        deployment.yaml           # Margo ApplicationDeployment (wire-exact):
                                  #   deploymentId, profile components,
                                  #   resolved parameters
        application.yaml          # Margo ApplicationDescription (wire-exact)
        compose.yml              # rendered deployment artifact for this device
        files/                    # config files the workload mounts

- Margo kinds, pinned: application.yaml is kind:
  ApplicationDescription; deployment.yaml is kind:
  ApplicationDeployment (both wire-exact, per spec/margo desired-
  state model). The agent CONVERGES from compose.yml; deployment.
  yaml is the STATUS contract — status reports use its deploymentId
  and carry one component entry per its components[] (Margo
  deployment-status.md requirement).
- deploymentId is deterministic: UUIDv5(REEVE_UUID_NAMESPACE,
  "<device_id>/<app-name>"). Pure function of render inputs — no DB
  coordination, stable across re-renders (byte-identical rule
  holds), survives device wipe + re-enroll to the same identity.
- One app dir = one unit of convergence. Present dir = desired,
  absent dir = remove. No other channels of intent.
- manifest.yaml contains ONLY: source tree revision ids (D13; hub +
  local revision when federated), device id, render generation
  counter, tier registry endpoint (the full declared render-input
  set, D3). NO timestamps — renders must be byte-identical when
  inputs are identical.
- Agent-local state (never in the bundle): /var/lib/reeve-agent/
  { agent.db (journal, WAL), applied/ (copy of last-applied bundle),
  work/, apps/<name>/env/<service>.env (materialized secrets, D15 —
  agent-local, 0600, OUTSIDE the hashed bundle dir, never part of
  any digest) }. applied bundle digest recorded in agent.db, not a
  loose file.

## D3. Overlay merge semantics (these ARE the desired-state tests)
- Layer order: fleet -> class -> region -> site -> device (class is
  the optional per-device hardware/type layer, D12; at most one).
  Later layer wins.
- Maps: deep merge, key by key.
- Lists: REPLACE, always. Never append, never merge-by-index.
  (Append semantics are where YAML merge systems go to die.)
- Explicit `null` at a key = deletion of that key from the merged
  result. Absence = inherit. No other tombstone mechanism.
- Scalars: override.
- Determinism (MUST): canonical emitter — keys sorted lexically,
  block style, LF endings, trailing newline, no anchors/aliases in
  output. Same inputs => byte-identical render => same bundle
  digest. A re-render with no changes MUST produce no new bundle
  and no manifestVersion bump; likewise authoring identical layer
  content MUST produce no new revision (D13, D14).
- Render is a pure function: (tree contents at a revision, device
  context) -> file set, where device context = { device_id, layer
  chain (class/region/site assignment from the device row), tier
  registry endpoint (D8) }. Everything that varies is a DECLARED
  input, recorded in manifest.yaml (D2). No clock, no environment
  reads, no network, NO SECRET VALUES in the render path (D15:
  secrets render as references) — ${REEVE_REGISTRY} is resolved from
  the device-context input, never from env.

## D11. The overlay tree (the render INPUT — review hardest)
- The tree lives in the REVISION STORE (D13): content-addressed
  blobs + append-only revisions in reeve-server's SQLite, authored
  only by reeve-server's API (D14; single writer per layer,
  REV-005). Devices never see the tree — they see renders (D2).
- Layers are PATHS within a revision's manifest; the numeric prefix
  makes D3's layer order lexically sortable:

    layers/
      00-fleet/
      05-class.<name>/            # optional, at most one per device (D12)
      10-region.<name>/
      20-site.<name>/
      30-device.<device_id>/
    packages/
      <app-name>/<version>/       # vendored margo-package trees (v1),
                                  #   stored as blobs like everything else

- The engine treats layer dirs as ordered opaque names
  (NN-<label>.<n>); ONLY the numeric prefix orders the merge. The
  canonical taxonomy above is convention, not engine knowledge
  (D12).
- A device's layer chain = fleet -> its class (if any) -> its region
  -> its site -> its device dir; membership comes from the device
  row and enters render as device context (D3), not from tree
  content.
- Each layer path may contain, per app:
    apps/<app-name>/app.yaml      # app source: package name+version
                                  #   (packages/ ref), profile
                                  #   selection, enabled: true|false
    apps/<app-name>/params.yaml   # parameter values (secret values
                                  #   never — references only, D15)
    apps/<app-name>/files/<path>  # config files
  Merge per D3 across the chain: app.yaml/params.yaml deep-merge
  key-by-key (lists replace, null deletes); files/ entries replace
  whole-file (a file is a scalar, not a mergeable map).
- App presence: an app is desired iff any layer in the chain defines
  it and merged `enabled` is true (scalar override per D3 — a site
  can switch off a fleet app with one line). No path-deletion
  semantics in the tree; that's a rendered-bundle concept (D2).
- Packages are vendored into packages/ in v1 (air-gap-friendly,
  keeps render pure — package bytes are revision content, no network
  in the render path). OCI package refs later happen via a PRE-FETCH
  step that vendors into a new revision, never via fetch-at-render.
  (D13 rationale notes registry-hosted packages are the more
  Margo-native end state; vendoring is the v1 simplicity trade.)
- Render (desired-state crate): merged app.yaml + params.yaml +
  package (via margo-package crate) + device context -> rendered
  apps/<name>/{deployment.yaml, application.yaml, compose.yml,
  files/} per D2. Margo parameter targets (env for compose) and
  ${REEVE_REGISTRY} resolve here from declared inputs.
- The desired-state table tests are therefore: revision content
  fixture (layer paths + vendored package) + device context in ->
  rendered file set out, byte-exact. Required fixtures include a
  class-layer case and a pinned-device-under-rollout case (D12).

## D12. Grouping, labels, pins × rollouts
- Labels group, layers configure. Devices carry free-form labels
  (device row). Labels are legal cohort selectors for rollouts
  (REV-008) and UI filtering ONLY — they MUST NOT select or inject
  configuration. Config derivation remains the linear layer chain.
- One added chain dimension: optional class layer, 05-class.<name>,
  at most ONE per device, assigned in the device row (like
  region/site). Chain: fleet -> class -> region -> site -> device.
  For hardware/device-type config variance. Still a straight line.
- Engine treats layer dirs as ordered opaque names (NN-<label>.<n>);
  only the numeric prefix orders the merge. The canonical taxonomy
  is convention, not engine knowledge.
- Rollout convergence target: a device's target is ITS OWN RENDER of
  the rollout's tree revision. Pinned devices converge to renders
  still carrying the pin and count as CONVERGED in gate math.
  Rollout status API/UI MUST surface cohort members whose render is
  materially unchanged ("pinned/unaffected: N").
- Still NOT decided: selector syntax/UX for cohorts, operator-
  defined taxonomies beyond naming, multi-class devices (REFUSED —
  one class max keeps the chain linear; two classes means computed
  layer ordering, which is a design session, not a merge tweak).

