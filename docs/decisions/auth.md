# reeve decisions — Auth (D1)

Part of docs/decisions/; start at [00-INDEX.md](00-INDEX.md).

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
  against reeve-server in v1; spec/reeve/01-framework.md §3.8 scopes the interop claim
  accordingly and lists all replaced surfaces. The Identity
  extractor seam is where cert/message-signature auth lands later
  with zero handler changes (see NOT-decided list).
- Terminal (REV-002) enables only under password/proxy modes; every
  session row records the authenticated username.


