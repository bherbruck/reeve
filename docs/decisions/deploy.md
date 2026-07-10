# reeve decisions — Deployment (D9)

Part of docs/decisions/; start at [00-INDEX.md](00-INDEX.md).

## D9. The canonical compose file — one file, every tier
- deploy/compose.yml is THE deployment artifact for reeve-server at
  any tier. Same services; tier is chosen by env vars, optional
  sidecars by compose profiles. It is the ONE checked-in compose
  file — the explicit exception to spec/reeve/08-packaging.md §10.6's no-emittable-files
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

