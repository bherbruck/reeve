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

