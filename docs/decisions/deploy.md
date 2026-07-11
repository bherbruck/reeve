# reeve decisions — Deployment (D9)

Part of docs/decisions/; start at [00-INDEX.md](00-INDEX.md).

## D9. The canonical compose file — one file, every tier
- The root `compose.yml` is THE deployment artifact for reeve-server
  at any tier. Same services; tier is chosen by env vars, optional
  sidecars by compose profiles. It is the ONE checked-in compose
  file — the explicit exception to spec/reeve/08-packaging.md §10.6's no-emittable-files
  rule (recorded there); `reeve-server init` emits a copy/variant of
  it, and CI keeps the two from drifting.
- REVISED (post-build, operator ergonomics over spec tidiness): the
  file lives at the REPO ROOT, not `deploy/`, alongside `docker/`
  (server + agent Dockerfiles) and `.env.example`. Rationale: the
  thing you run constantly (`docker compose up`) should need zero
  `-f deploy/...` flags and sit where every convention says to look;
  `deploy/` keeps only what genuinely can't run from root (k8s
  manifests, the CI build matrix, the agent package). The anti-drift
  concern that motivated hiding it is covered by the sync test, not
  by burying the file.
- Container images build via `docker/Dockerfile.server` (multi-stage
  UI+rust -> distroless) and `docker/Dockerfile.agent` (dev/test;
  docker:cli base, needs the host socket — the agent's real path is
  still the static binary + systemd unit, §10.3). The static-musl
  bare-box binaries remain the deploy/ci matrix's job.
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

