# reeve decisions — Delivery & Artifacts (D7, D8, D13)

Part of docs/decisions/; start at [00-INDEX.md](00-INDEX.md).

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
  adopted; manifestVersion is (epoch, counter) packed into one u64,
  epoch bumped by restore fencing — spec/reeve/07-durability.md §9.5/§10.2), then pulls
  the referenced render artifact from /v2 by
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

