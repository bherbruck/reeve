## 12. Secrets (REV-009)

Secrets are desired state BY REFERENCE, never by value
(DECISIONS.md D15, the deciding document — this section is the
normative wire/behavior summary). Tree content and rendered
artifacts carry `${secret:<name>}` references; values NEVER enter
the config plane: no plaintext in revisions, renders, bundles,
snapshots, revision sync, or air-gap media — by construction, since
those artifacts only ever contain references.

### 12.1 Scoping and rendering

- Secrets are defined at layers and resolve down the same chain as
  config (fleet -> class -> region -> site -> device, deeper wins).
- Resolution is SERVER-SIDE AT REQUEST TIME — never at render time.
  Render stays pure (D3) and bundles stay secret-free. A
  secret-typed parameter inside the wire-exact ApplicationDeployment
  carries the reference string as its value (audited in §3.7).

### 12.2 Storage

- Secrets live in a table in reeve-server's SQLite, AEAD-encrypted
  under a master key in a FILE OUTSIDE the DB (REEVE_DATA/
  secret.key, 0600, created at init). Snapshots therefore ship
  ciphertext only; restore = snapshot + keyfile (§9.1, §9.5).
  `reeve-server init` MUST warn that the keyfile needs separate
  backup (§10.3).
- The same store holds the server's own operational secrets (zot
  upstream credentials, S3 keys, tier tokens).
- UI: secrets are write-only after entry — set, rotate, view
  metadata (name, scope, version, last-rotated); never read back.

### 12.3 Delivery (rev-009/1)

- At apply time the agent calls
  `POST /api/reeve/v1/secrets/resolve` with its device credential
  (D1 provision-once): a device can only ask as itself and receives
  only its own resolution. Plaintext exists in exactly three
  places, ever: server RAM during resolve, TLS in flight, and the
  device's env files at rest (0600, temp+rename, agent-local,
  OUTSIDE the hashed bundle dir — the honest v1 trade for Law 5
  reboot-while-offline; FDE recommended in deployment docs).
- Service-level scoping rides Margo's own primitive: parameter
  `targets` declare `components: []`, and the agent materializes
  env PER SERVICE (`apps/<name>/env/<service>.env`, only the values
  targeted at that component); rendered compose references them via
  `env_file`. Compose recreates only services whose resolved config
  changed, so a rotation bounces exactly the consuming services
  (Law 4: restart semantics delegated to compose's own diff).
- Offline devices apply from last materialized env files (Law 5);
  the resolve endpoint being unreachable never blocks convergence
  of already-resolved apps.

### 12.4 Rotation and propagation

- Rotating a secret bumps its version => affected devices' per-app
  `secrets_version` (hash of resolved secret names+versions, never
  values) changes in the State Manifest => manifestVersion bumps =>
  REV-001 nudge says "poll now" (§4.4).
- Agent diff: bundle digest unchanged + secrets_version changed =>
  re-resolve, rewrite only env files whose content differs,
  `up -d` affected apps. No bundle re-pull. Offline devices catch
  the same rotation on next poll (nudge = optimization, never
  correctness).
- Rotation state is published as `secret-rotation` events (§6.3).
- Coordinated rotation across apps/devices is explicitly NOT
  decided (DECISIONS.md NOT-decided list) — implementations MUST
  NOT improvise it.

### 12.5 Federation and air-gap

- The hub syncs DOWN to each gateway only the secrets resolvable
  within that gateway's subtree, over the tier channel,
  RE-ENCRYPTED under the gateway's own local master key (per-tier
  keys: a stolen snapshot from one tier + another tier's key yields
  nothing). Gateways serve cached scoped secrets through WAN
  outages; rotations queue and land on reconnect (§8.3 pattern).
- Air-gap: secret sets export encrypted TO THE DESTINATION
  GATEWAY'S PUBLIC KEY (each gateway mints a keypair at init;
  fingerprint verified out-of-band at commissioning). Never
  plaintext on media (§8.5).

### 12.6 Security

- The resolve endpoint is the single plaintext egress; it MUST be
  scoped to the requesting device's own resolution, rate-limited,
  and audit-countable (who resolved what version when — metadata,
  not values).
- A compromised device learns exactly the secrets targeted at its
  own apps' components — the minimum a running workload must know
  anyway. A compromised gateway learns its subtree's scoped set,
  never the fleet's.
- Env files at rest on devices are the accepted v1 residue; their
  scope is bounded per service by §12.3.

