# DECISIONS.md — reeve implementation decisions (pre-Milestone 1)

Principles applied throughout: minimal sidecars, maximum cohesion,
migrations built in, atomic-or-absent writes, idempotent everything,
simplicity, explainability (every choice survives the layman test).
These are DECIDED. Agents: do not relitigate; propose changes to a
human, don't improvise them.

## D1. Auth — one Identity seam, three human modes, one device credential
- All auth is tower middleware + axum extractors. Handlers receive
  `Identity` (Device(id) | Human(user, role) | Anonymous) from an
  extractor and NEVER parse credentials themselves. Swapping or
  adding auth = one module.
- HUMAN auth, selected by REEVE_AUTH (password | proxy | none):
  - password (default): local users table (argon2id), SQLite-backed
    session cookies (sliding expiry), login page. First boot: zero
    users => log a one-time setup token, serve /setup to create the
    admin. Idempotent; all writes tx or temp+rename.
  - proxy: trust REEVE_PROXY_USER_HEADER from a fronting auth proxy
    (Authelia/authentik/oauth2-proxy/Tailscale). MUST refuse to
    start unless REEVE_PROXY_TRUSTED_CIDR is set and the peer
    matches — never trust the header from the world.
  - none: Anonymous is admin; loud startup warning. Bench and
    air-gapped dev only.
  - Roles: admin | operator | viewer. OIDC is never built in;
    proxy mode is the SSO story.
- DEVICE auth — provision once, use everywhere: enrollment (D4)
  issues ONE device token, and that single credential authenticates
  every device-facing surface: the device API, the desired-state
  manifest poll (D13), /v2 pulls (render bundles, packages, agent
  binaries served natively; container images proxied to zot — D7,
  D8), the persistent websocket (REV-001), and the secrets resolve
  endpoint (D15). For image pulls the proxy authenticates the device
  itself and injects backend credentials to zot — device tokens
  never reach the sidecar, and the sidecar trusts only the proxy.
  One enrollment = full site capability; one revocation (kill the
  token, tombstone its desired state) = full site cutoff, including
  images.
- DIVERGENCE FROM MARGO (deliberate, recorded here per CLAUDE.md):
  Margo mandates X.509 client certs + HTTP Message Signatures
  (RFC 9421) on the device API, established via its certificate
  onboarding flow (POST /api/v1/onboarding + Certificate API). reeve
  v1 REPLACES both with join-token enrollment (D4) + bearer device
  token. Consequence: a vanilla Margo device client cannot enroll
  against reeve-server in v1; SPEC.md §3.8 scopes the interop claim
  accordingly and lists all replaced surfaces. The Identity
  extractor seam is where cert/message-signature auth lands later
  with zero handler changes (see NOT-decided list).
- Terminal (REV-002) enables only under password/proxy modes; every
  session row records the authenticated username.


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

## D4. Enrollment ceremony
- Operator creates a join token in the UI/API: TTL + max-uses
  (default: 24h, 1 use). Token is random, stored hashed.
- `reeve-agent install --server https://reeve.example --token <JT>`:
  1. POST /api/reeve/v1/enroll { join_token, hostname, arch,
     agent_version } — a reeve surface (SPEC §3.1 rule 4; never under
     Margo's /api/v1/). Margo's POST /api/v1/onboarding is NOT
     served; that replacement is recorded in D1 and SPEC §3.8.
  2. Server: validates token, creates device row + initial desired
     state, issues device_id + device token. Atomic (one SQLite tx —
     the revision store lives in the same DB, D13). This ONE token
     is the device's credential for API, manifest poll, /v2 pulls,
     websocket, and secrets resolve (D1).
  3. Agent writes /etc/reeve-agent/agent.toml (0600, temp+rename),
     installs its systemd unit, starts.
- Idempotent re-run: same join token before expiry + same hostname
  returns the SAME device (no duplicate rows from a retried install).
- Re-enrollment after a wipe: operator issues a re-enroll token bound
  to the existing device_id; a fresh box resumes the old identity and
  desired state. Plain join token on a wiped box = new device, old
  one flagged stale in UI.

## D5. Compose provider semantics
- v1 shells out to `docker compose` v2. Boring, correct, debuggable
  by anyone ("we run exactly what you'd type"). Docker API later
  only if a named need appears.
- Project name = app dir name. Apply = `docker compose -f
  apps/<name>/compose.yml -p <name> up -d --remove-orphans`.
- Diff = content hash per app dir (recorded in agent.db) vs unpacked
  bundle. Hash unchanged => skip (no-op convergence is silent).
  Secrets are diffed separately by secrets_version (D15): bundle
  digest unchanged + secrets_version changed => re-resolve and
  rewrite env files only, `up -d` affected apps, no bundle re-pull.
- Removal: app dir gone from bundle => `docker compose -p <name> down`
  using the RETAINED last-applied copy in /var/lib/reeve-agent/
  applied/ (you can't down a stack whose file you deleted first).
  Down succeeds => applied copy removed. Order: down before delete.
- Post-apply status: `docker compose ps --format json` mapped to
  Margo deployment states; recorded in journal, reported per REV-004.
- Crash-only: convergence is resumable from any kill point — the
  journal records intent before action (app, hash, phase), and
  startup re-runs any incomplete phase. `up -d` and `down` are
  idempotent, which is why shelling out is safe.
- Journal phases (the crash-safety contract, per app): planned(app,
  hash) -> applying -> applied | failed; removal: removing ->
  removed. Terminal phases: applied, removed, failed. Startup
  re-runs any row not in a terminal phase; re-running any phase is
  a no-op when its postcondition already holds.

## D6. Migrations & storage plumbing (mostly restating law as picks)
- refinery with embedded migrations; run at every startup, both
  binaries. Schema version asserted by verify-restore (REV-006).
- SQLite open mode everywhere: WAL, foreign_keys ON, busy_timeout
  5s, single writer connection + read pool.
- Every file write anywhere in the codebase: temp file, fsync,
  rename. No exceptions. Uploads: temp key, finalize (REV-006).
- SCHEMA LAW (D16): every tracked table MUST have an explicit
  PRIMARY KEY — universal, no rowid-only tables (session-extension
  requirement made a house rule; enforce in review).
- SCHEMA LAW (D16): a schema migration MUST immediately cut a new
  snapshot generation — changesets do not capture schema changes.
  Bootstrap sequence: migrate -> if migrated, snapshot -> resume
  changeset streaming. A changeset sequence never spans a schema
  version.

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

## D7. Artifact serving — native read-only OCI, never a sidecar
(v2 of this decision — v1 was embedded git serving; git is removed
from the runtime entirely by D13.)
- reeve-server natively serves READ-ONLY OCI pull (GET manifest, GET
  blob by digest, standard /v2 distribution routes) for its OWN
  artifacts: render bundles, vendored app packages, agent binaries
  (REV-007 "embedded-agents" becomes "the agent is an artifact").
  In-process, same port as UI/API/SSE/websocket. No push routes,
  ever — artifacts are produced only by the server itself.
- Container IMAGES are not served natively: /v2 requests for image
  repos reverse-proxy to the zot sidecar (D8). One /v2 route space,
  two backends, invisible to clients.
- Operator escape hatch replacing "clone it and look":
  `reeve-server export --revision N --dir ./out` dumps any
  historical tree revision as plain files; `sqlite3` direct
  inspection of the revision store is the other stock-tool path.
- Published artifacts SHOULD be cosign-signed in v1 (MUST later) —
  content-addressing gives integrity; signing adds provenance.

## D8. Container registry — zot sidecar, proxied through reeve
- Problem: rendered compose files reference OCI images; NAT'd,
  flaky-WAN, and air-gapped tiers need images available locally.
- Pick: zot (CNCF, single static binary, pure OCI dist-spec) as a
  sidecar, one per reeve-server tier. Not embedded in reeve-server —
  a registry is a whole product; this is the one earned sidecar.
- Scope split with D7: reeve-server's OWN artifacts (render bundles,
  packages, agent binaries) are served natively; zot serves/caches
  CONTAINER IMAGES only. That is the whole sidecar contract.
- Single-endpoint preserved: reeve-server reverse-proxies image
  /v2/* routes to the zot sidecar. Devices and operators only ever
  talk to https://<reeve-host> — one host, one port, one firewall
  rule, for API + UI + artifacts + images.
- Image flow follows the federation tree (single-writer analog):
  images are pushed/pinned at the HUB only. Spoke zot uses the sync
  extension to pull-through/cache from the hub's registry on demand;
  hub zot caches from public registries or holds privately pushed
  images. Air-gap: images travel as OCI layout archives (oras/skopeo
  copy) on the same media as revision + package exports (D13 — one
  archive format for everything); imported into the spoke zot.
- Render rule: compose files are rendered with image refs pointing
  at the tier-local endpoint — `${REEVE_REGISTRY}/...` resolved at
  render time from the device-context input (D3: declared input,
  recorded in manifest.yaml — render stays pure and byte-
  reproducible). No docker daemon mirror config, no
  containerd rewriting magic on devices: the image ref in the file
  IS the truth. Explainable: read the compose file, that's where the
  image comes from.
- Agent devices need NO registry sidecar — they pull from their
  tier's reeve endpoint like any registry, authenticating with the
  same device token as everything else (D1: provision once). The
  proxy terminates device auth and speaks its own credential to zot;
  zot accepts connections only from reeve-server.

## D9. The canonical compose file — one file, every tier
- deploy/compose.yml is THE deployment artifact for reeve-server at
  any tier. Same services; tier is chosen by env vars, optional
  sidecars by compose profiles. It is the ONE checked-in compose
  file — the explicit exception to SPEC §10.6's no-emittable-files
  rule (recorded there); `reeve-server init` emits a copy/variant of
  it, and CI keeps the two from drifting.
- Tier selection: REEVE_UPSTREAM unset => this instance is a ROOT
  (hub, or air-gapped standalone). REEVE_UPSTREAM set => this
  instance is a SPOKE/gateway mirroring that upstream (REV-005).
  Same image, same service, no mode flag beyond that.
- Profiles: `registry` (zot sidecar — on for any tier serving
  devices images). That is the ONLY compose profile: durability is
  entirely in-binary (D16 — snapshot + changeset tiers, env/config
  selected), so ZERO durability sidecars exist. The full sidecar
  roster of the architecture: zot (images, optional) and the user's
  own auth proxy (D1 proxy mode, optional). `reeve-server init`
  emits the zot config the profile mounts — never checked in.
- reeve-server remains fully runnable as a bare binary with zero
  sidecars (native artifact serving, snapshot durability, no image
  registry) — the compose file adds capability, never rescues
  necessity.

## D10. API types — utoipa -> openapi.json -> orval (TanStack Query)
- Server side: every axum route is annotated with utoipa; the
  resulting openapi.json is emitted at build, embedded in the binary
  (REV-007), and served at a stable path. The Rust types ARE the
  source of truth.
- UI side: orval generates ui/src/api/ from openapi.json — typed
  client functions plus TanStack Query artifacts (useQuery/
  useMutation hooks and query-key factories). The generated
  directory is never hand-edited; no hand-written API types in TS,
  no exceptions (CLAUDE.md rule, restated as pipeline).
- `just gen-api` = run server openapi dump -> orval. CI regenerates
  and fails on drift (`git diff --exit-code ui/src/api`).
- SSE payloads (SPEC §6.3) are typed through the same pipeline:
  event payload schemas are registered as OpenAPI components, so the
  UI's invalidation handlers consume generated types too.
- Query-key discipline: routes' generated key factories are the only
  query keys the UI uses — SSE invalidation (SPEC §6) invalidates by
  those factories, never by hand-built keys.

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

## D13. No more git — SQLite history + OCI artifacts + HTTP delivery
DECIDED: git is removed from the runtime architecture entirely.
- The overlay tree is a REVISION STORE in reeve-server's SQLite:
  content-addressed blobs (sha256 -> bytes) + append-only revisions
  (id monotonic, parent, author, message, root manifest of path ->
  blob digest). Single writer (the API/UI, D14) unchanged.
  Diff/undo/blame/bisect become queries: diff computed on read,
  undo = new revision with prior content, blame = SELECT, bisect =
  binary search over revision ids. Atomicity = one SQLite tx.
- Rendered desired state is delivered to devices as an OCI ARTIFACT
  (a bundle: the D2 layout as manifest + blobs), pulled by digest.
  The device-facing flow models Margo's actual Desired State API
  (workload-management-api-1.0.0.yaml /deployments): agent polls a
  small State-Manifest-shaped JSON via conditional GET (ETag =
  RFC 9110 strong validator, digest grammar "sha256:<hex>"),
  enforces manifestVersion strict monotonicity (reject + log
  security event on regression — Margo's anti-rollback check,
  adopted), then pulls the referenced render artifact from /v2 by
  digest, verifies, unpacks to temp, atomic dir swap, converges (D5
  unchanged). Devices never speak git.
- The manifest carries, per app, a `secrets_version` (hash of
  resolved secret names+versions, never values — D15) alongside the
  bundle digest, so secret rotation propagates without bundle
  re-pull.
- reeve-server natively serves read-only OCI pull for its own
  artifacts (D7); zot remains the one earned sidecar, images only
  (D8). Bare-binary zero-sidecar mode stays fully functional.
- Federation (REV-005) re-plumbs onto the same primitives one tier
  up: a gateway syncs tree revisions from upstream via conditional
  GET + content-addressed blob fetch (identical protocol shape to
  device delivery), renders locally (render is pure, D3 — renders
  are byte-identical at any tier). Air-gap: revisions + packages +
  images all export as OCI layout archives on the same media; git
  bundles are gone.
- Durability unifies (REV-006): the ENTIRE server state including
  tree history is one SQLite file -> one snapshot pipeline, one
  verify-restore. The parallel git-mirror/bundle durability path is
  deleted.
- gix leaves the workspace. crates/repo-store becomes
  crates/revision-store (rusqlite; content-addressed blob + revision
  tables; no VCS anywhere).
- Operator escape hatch + signing: see D7.
- RATIONALE: the tree is authored only by reeve-server (D11, D14) —
  no human runs git; the git feature set in actual use was ~15%
  (append/read/diff/revert/attribute/atomic), all of which SQLite
  provides more simply and OCI distributes more uniformly. Removes a
  major dependency, a second durability system, and a device-side
  protocol. CONVERGES with the pinned Margo spec: the Margo WG voted
  git OUT in favor of REST retrieval (decision tracker issue #22,
  Feb 2025); their Desired State API is the conditional-GET + digest
  + monotonic-version model adopted above. Margo also models app
  packages as registry-hosted, so packages-as-OCI is more
  Margo-native than tree-vendoring (v1 vendors anyway; see D11).

## D14. Tree authoring is an API — automation-friendly by design
- The revision store's single writer is reeve-server's API. That API
  MUST be first-class for automation: token-authed, idempotent
  "put this layer's content" semantics (same content => no new
  revision, per the D3 no-change-no-commit rule), so IaC workflows
  (operators keeping tree content in their own git repo, reviewing
  via PRs, applying from CI — e.g. `reeve-tree apply ./layers`) are
  a supported front door, not a UI scrape. Git may exist UPSTREAM of
  reeve as a human review ritual; it never exists inside it.

## D15. Secrets — referenced in config, valued out-of-band
- Secrets are desired state BY REFERENCE, never by value. Tree/
  params carry `${secret:<name>}` (shape); values NEVER enter the
  config plane: no plaintext in revisions, renders, bundles,
  snapshots, mirrors, or air-gap media — by construction, since
  those artifacts only ever contain references.
- Storage: secrets table in reeve-server SQLite, AEAD-encrypted
  under a master key in a FILE OUTSIDE the DB (REEVE_DATA/
  secret.key, 0600, created at init). Consequence: REV-006
  snapshots ship ciphertext only; restore = snapshot + keyfile, two
  artifacts from two places. `reeve-server init` MUST warn that the
  keyfile needs separate backup. The same store holds the server's
  own operational secrets (zot upstream creds, S3 keys, tier
  tokens).
- Scoping: secrets are defined at layers and resolve down the same
  chain as config (fleet -> class -> region -> site -> device,
  deeper wins). Resolution is SERVER-SIDE AT REQUEST TIME — never at
  render time; render stays pure and bundles stay secret-free.
- UI: secrets are write-only after entry — set, rotate, view
  metadata (name, scope, version, last-rotated); never read back.
- Delivery: at apply time the agent calls a resolve endpoint over
  its existing device token (D1 provision-once; a device can only
  ask as itself => receives only its own resolution). Plaintext
  exists in exactly three places, ever: server RAM during resolve,
  TLS in flight, and the device's env files at rest (0600,
  temp+rename, agent-local, OUTSIDE the hashed bundle dir — honest
  v1 trade for Law 5 reboot-while-offline, documented with an FDE
  recommendation).
- Service-level scoping (balena-style, but via Margo's own
  primitive): ApplicationDeployment parameter `targets` already
  declare `components: []`. The agent materializes env PER SERVICE
  (apps/<name>/env/<service>.env, only the values targeted at that
  component); rendered compose references them via env_file. Since
  compose recreates only services whose resolved config changed, a
  rotation bounces exactly the consuming services and nothing else
  — restart semantics delegated to compose's own diff (Law 4).
- Rotation & propagation: rotating a secret bumps its version =>
  affected devices' per-app `secrets_version` in the manifest
  changes => manifestVersion bumps => REV-001 nudge says "poll now."
  Agent diffs: bundle digest unchanged + secrets_version changed =>
  re-resolve, rewrite only the env files whose content differs,
  `up -d` affected apps. No bundle re-pull. Offline devices catch
  the same rotation on next poll (nudge = optimization, never
  correctness).
- Federation: hub syncs DOWN to each gateway only the secrets
  resolvable within that gateway's subtree, over the tier channel,
  RE-ENCRYPTED under the gateway's own local master key (per-tier
  keys: a stolen snapshot from one tier + another tier's key yields
  nothing). Gateways serve cached scoped secrets through WAN
  outages; rotations queue and land on reconnect.
- Air-gap: secret sets export encrypted TO THE DESTINATION GATEWAY'S
  PUBLIC KEY (each gateway mints a keypair at init; fingerprint
  verified out-of-band at commissioning). Never plaintext on media.
- Wire-exactness: a secret-typed parameter inside the wire-exact
  ApplicationDeployment carries the `${secret:<name>}` reference as
  its `value` string — syntactically valid per the pinned schema
  (parameter values are plain strings; verified against
  DesiredState-001.yaml), substituted agent-side at apply. Recorded
  in SPEC §3.7's audit table as a value convention, not a field
  change.

## D16. In-binary changeset streaming — litestream removed
DECIDED: litestream is REMOVED from the design entirely. Seconds-RPO
is provided in-binary via SQLite's session extension (trunk SQLite,
rusqlite `session` feature).
- Two durability tiers, both in-binary, one pipeline (SPEC §9):
  1. SNAPSHOT tier (the generation anchor, ships first): VACUUM INTO
     on interval, AEAD-encrypted under the D15 external keyfile,
     atomic upload (temp key + finalize), retention window.
  2. CHANGESET tier (seconds-RPO, fast-follow): the single writer
     connection (D6 — exactly what session capture requires) carries
     an attached session. Every N seconds or M commits (config, e.g.
     5s/100): extract changeset, compress, AEAD-encrypt under the
     same keyfile, upload with a strictly sequenced key chained to
     the current snapshot generation (generation id + monotonic
     seq). Atomic-or-absent; empty changeset => no upload.
- Restore = snapshot generation + changesets applied in sequence via
  changeset_apply. Conflicts are structurally impossible (own
  lineage onto own snapshot) — any reported conflict is CORRUPTION:
  abort loudly, never auto-resolve. Point-in-time = replay to seq K
  (`--to-seq` / `--to-time`; upload timestamps are the coarse
  index).
- verify-restore proves the WHOLE chain (snapshot + all changesets +
  schema + recency of last applied seq). One restore procedure for
  everything (SPEC §9.4).
- Crash-only: an unflushed in-memory session lost to kill -9 costs
  at most the configured interval (that IS the RPO); startup resumes
  from the last uploaded sequence. No session state outside the DB
  and the object store.
- Schema laws this adds live in D6 (universal explicit PRIMARY KEY;
  migration cuts a new generation).
- RATIONALE: the session extension is SQLite's own change-log
  primitive — capture is trunk code (Law 4: someone else's test
  suite); reeve writes only the shell (extract, encrypt, upload,
  replay). Litestream, post-D15, was the sole remaining plaintext
  escape (WAL frames to the object store outside our keyfile), the
  only foreign process on the durability path, and a second restore
  procedure verify-restore didn't govern. Changesets also replay
  TRANSACTIONS (logical row changes), so point-in-time restore lands
  on a transaction boundary of our own schema — coherent state, not
  whatever pages had flushed. The Durability trait seam (SPEC §9.1)
  is what makes this reversible if the changeset tier disappoints on
  the bench, and where a future engine-native CDC could slot.
