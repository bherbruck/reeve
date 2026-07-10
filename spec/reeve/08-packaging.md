## 10. Packaging & Self-Hosting (REV-007)

How reeve ships and installs itself: one static binary per role
embedding everything it needs; desired state delivered via a State
Manifest poll + native read-only OCI pull, in-process, on one
listening socket; idempotent self-install for both roles; an
optional `/install` bootstrap endpoint serving version-coherent
agent binaries; agent self-management through the normal
desired-state tree with A/B binary safety.

reeve targets boxes and sites where "install the platform first" is
the failure mode being replaced. The goal: one file, one port, one
command — and the agent's update path is the same desired-state
mechanism the agent exists to apply. Distribution is outside
Margo's scope, with one wire-adjacent constraint: the /v2 routes
MUST speak standard OCI distribution pull so stock clients (oras,
skopeo, crane) remain debugging tools, not casualties.

### 10.1 Single static binary

- Each role (`reeve-server`, `reeve-agent`) ships as ONE static
  binary (musl) for `x86_64` and `aarch64` Linux. No shared-library
  dependencies; a binary copied to a bare box runs.
- The reeve-server binary embeds:
  - UI dist (rust-embed), served by path with index.html fallback
    for SPA deep links (CLAUDE.md "ui/").
  - SQL migrations, run idempotently at EVERY startup — startup IS
    recovery (Law 3); no separate migrate step, and a half-migrated
    database from a mid-migration `kill -9` MUST be resumable
    (transactional, versioned migrations).
  - `openapi.json` (the document the UI client is generated from),
    served at a stable path.
  - Default configuration (files hold shape; values live in the DB
    per Law 4 — the embedded default is the shape).
  - Shell completions (`--completions <shell>`).
- Both binaries MUST support `--spec`, printing this specification
  (embedded at build) — the deployed artifact carries its own
  contract.
- Version output MUST include the workspace git revision (§10.4
  depends on it).

### 10.2 Desired-state delivery: State Manifest poll + native OCI pull

(v2 of this section — v1 was git over smart HTTP; git is removed
from the runtime by DECISIONS.md D13.)

- **Manifest poll**: `GET /api/reeve/v1/manifest` (device-scoped by
  its credential) returns the device's State-Manifest-shaped JSON:
  `manifestVersion`, render-bundle digest + pull URL, per-app
  `secrets_version` (§12). Conditional GET: the ETag is the
  manifest digest, an RFC 9110 strong validator with the digest
  grammar `sha256:<hex>`; `If-None-Match` match returns 304. This
  models Margo's Desired State API (§3.8 item 3 reassessment).
- **Anti-rollback (adopted from Margo)**: `manifestVersion` is
  logically the pair `(epoch, counter)` compared lexicographically,
  encoded on the wire as ONE monotonically increasing unsigned
  64-bit integer — epoch in the high 16 bits, counter in the low 48
  — so plain integer comparison IS the pair comparison and the
  value stays exactly Margo's modeled shape (`ManifestVersion`:
  monotonic u64, `workload-management-api-1.0.0.yaml`). The agent
  MUST enforce strict monotonicity: a non-increasing value is
  rejected and logged as a SECURITY event, and the agent continues
  from last known state (Law 5). An increase that bumps the epoch
  bits is accepted and logged as a NOTABLE event (a restore
  happened, §9.5 — the server's restore fencing guarantees a
  restored server always compares strictly greater).
- **Artifact pull**: the render bundle, vendored app packages, and
  agent binaries are OCI artifacts served natively, read-only
  (GET manifest / GET blob by digest, standard OCI distribution
  routes under `/v2/`; DECISIONS.md D7) through the same axum
  stack. No push routes exist — desired state is written only by
  the rendering pipeline (§8.4). Pull is content-addressed and
  immutable: verify digest, unpack to temp, atomic dir swap,
  converge (D5).
- Same port as everything else: UI, REST API, SSE (§6), websockets
  (§4, §5), native artifact /v2 routes, and — when the registry
  sidecar is deployed — the image proxy on the same /v2 space
  (DECISIONS.md D8). ONE listening socket total per server. No
  path shadows Margo's `/api/v1/…`.
- Wire behavior on /v2 MUST be standard OCI distribution pull:
  stock clients (oras, skopeo, crane) MUST fetch reeve's artifacts
  successfully — tested against stock tooling, not just our own
  client.
- Auth: the same device credential as the device API authorizes a
  device to poll exactly its own manifest and pull exactly the
  artifacts its manifest references; tier credentials (§8)
  authorize a gateway's revision sync scope. Anonymous pull MUST
  NOT be enabled by default.

### 10.3 Self-install

- `reeve-agent install` MUST: create its system user, write its
  systemd unit (via the same unit-emitting machinery as the
  systemd-unit Provider), write config shape, enable and start
  itself. `reeve-agent uninstall` reverses it.
- `reeve-server init` MUST emit deployment artifacts for the
  operator's chosen substrate: a compose file, or systemd unit
  files, and the zot configuration when the registry profile is
  selected (D9). Durability needs no sidecar and therefore no
  emitted config — both tiers are in-binary, env/config-selected
  (§9). init also creates the secrets master keyfile
  (REEVE_DATA/secret.key, 0600) and MUST warn that the keyfile
  needs separate backup (§12.2).
- Both commands MUST be idempotent — Law 3 applies to installers:
  re-running `install` on a half-installed box (killed mid-install)
  converges to installed; it never errors on "already exists" and
  never duplicates units or users.
- Installers write config and units; they MUST NOT bake secrets
  into world-readable files (units reference credential files with
  appropriate modes).

### 10.4 `/install` bootstrap endpoint

- Behind the cargo feature `embedded-agents`, reeve-server embeds
  the reeve-agent binaries for BOTH architectures and serves them
  as OCI artifacts on the native /v2 routes ("the agent is an
  artifact", DECISIONS.md D7), plus `GET /install` (a shell
  installer script).
- The script detects the machine architecture, pulls the matching
  agent artifact by digest FROM THE SERVER IT WILL ENROLL AGAINST
  (digests baked into the script at build), and runs `reeve-agent
  install` pointed at that same server. One origin for trust,
  transport, and enrollment:
  `curl -fsSL https://reeve.site.example/install | sh` is the whole
  bootstrap.
- Version coherence: served agent binaries MUST be built from the
  same workspace revision as the serving server (enforced at build;
  the feature does not admit mixing).
- The endpoint requires an enrollment credential/token by default;
  a deployment MAY open it on trusted networks (config). Without
  the feature the route is absent (404) — invisible, per §3.1
  rule 4.

### 10.5 Self-management — the agent as a workload

- The repository ships an example Margo-shaped application package
  (an `ApplicationDescription` valid per `margo-package`) for
  reeve-agent itself — the agent binary it references is the OCI
  artifact of §10.4 — so agent updates flow through the normal
  desired-state tree: authored at one tier, staged via Section 11,
  converged like any other workload. No side-band updater exists.
- Update mechanics: rename-and-exec or service restart, A/B on the
  binary path — install the new binary beside the old, atomically
  swap a symlink (or equivalent rename), restart the unit.
- A failed self-update MUST leave the previous binary running: the
  swap is the last step; a new binary failing its first health
  window is rolled back to the retained previous binary by the
  supervising unit's failure handling. `kill -9` at any point
  leaves either old-running or new-running — never neither (Law 3).
- The agent reports its version in status (§7.2, §3.3), which is
  how a staged agent rollout's health gates observe success.

### 10.6 deploy/

`deploy/` holds ONLY what the binaries cannot emit about
themselves: Kubernetes manifests wrapping the same artifacts for
operators who insist, and CI examples (musl build matrix,
embedding, release). Anything `reeve-server init` or `reeve-agent
install` can emit MUST NOT be duplicated as a checked-in file that
can drift — with ONE named exception: `deploy/compose.yml`, the
canonical tier-agnostic compose file (DECISIONS.md D9). `init`
emits a copy/variant of it and CI MUST keep the two in sync.

### 10.7 Security

- `curl | sh` bootstrap is trust-on-first-use of the server's TLS
  identity; the digest check binds binary to script but both come
  from the same origin. Deployments needing stronger provenance pin
  the server certificate or distribute the agent out of band — both
  remain supported since `/install` is optional.
- The native artifact routes expose desired state, which includes
  config: per-device authorization (a device pulls only what its
  own manifest references) is the confidentiality boundary and MUST
  be enforced on every /v2 route, including manifest GETs.
- Pull-only transport (no push routes exist) removes an entire
  class of write-path attacks against desired state.
- Self-update executes a binary the server delivered; the
  desired-state commit that triggered it is attributable (§8.4),
  and A/B retention bounds the damage of a bad (not malicious)
  binary. A malicious server is already the agent's root of trust —
  established at enrollment (Section 3.8 item 1), the same
  trust-the-server-you-enrolled-with posture as Margo's onboarding
  model (`spec/margo/…/device-client-onboarding.md`).

