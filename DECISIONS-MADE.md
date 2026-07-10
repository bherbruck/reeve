# DECISIONS-MADE — judgment calls made during the autonomous build

One line each: what, where, which doc informed it.

- rusqlite gains the `session` feature at workspace level (Cargo.toml) — required by D16 changeset tier (docs/decisions/storage.md).
- crates/repo-store renamed to crates/revision-store, gix removed from workspace — direct execution of D13 (docs/decisions/delivery.md).

## A1 reeve-types
- ApplicationDescription accepts id both top-level (spec examples) and under metadata (reference sandbox artifacts); effective_id() reads either — src/margo/application.rs, informed by ApplicationDescription-001.yaml vs reference nextcloud margo.yaml
- deploymentProfiles[].type, roles[], peripheral/interface/architecture kept as String with named constants, not enums: pinned fixtures contradict the OpenAPI enums (helm.v3, 'standalone cluster' case, 'GPU', x86_64) — workload-management-api-1.0.0.yaml vs device-capabilities.md examples
- Component.properties and Parameter.value are generic serde_yaml_ng::Value: property sets are profile-type-specific and fixtures disagree on scalar types (wait: true vs "true") — wire-exact preservation
- resources.cpu modeled as untagged CpuSpec {One|Many}: OpenAPI says object, every device-capabilities.md example is an array — original shape preserved on re-serialize
- DeploymentStatusManifest.status read as an object (not []status): the attribute table's []status is contradicted by both the example manifest and the OpenAPI schema — noted as spec typo in doc comment
- Overall-state precedence 'failed > removing > installing > pending > removing > installed' has duplicate 'removing'; second occurrence read as 'removed' (only member otherwise absent) — DeploymentState::severity, deployment-status.md
- StateManifest shaped after Margo UnsignedAppStateManifest: manifestVersion + bundle{mediaType,digest,sizeBytes,url} + apps[{appId,deploymentId,secrets_version}]; bundle always serialized (null when empty) per DeploymentBundleRef's MUST-NOT-omit rule — delivery.md D13 + 08-packaging §10.2
- secrets_version spelled snake_case on the wire (exact token used normatively throughout spec/reeve/); other reeve fields camelCase matching Margo convention
- Render-bundle media type const application/vnd.reeve.render-bundle.v1+tar+gzip coined after Margo's application/vnd.margo.bundle.v1+tar+gzip — D13 names no media type
- ServerCapabilities field named serverVersion (01-framework §3.3 says 'extension list plus server version' without pinning a name)
- HealthSample/DiskSample/MemorySample inner field names (usedBytes/freeBytes/totalBytes) reeve-chosen: §7.2 pins semantics not sub-shape; #[serde(flatten)] extra map preserves extensible fields per §7.2 MUST-ignore-unknown
- Journal backfill wire types (JournalBatch/JournalRecord kinds status|health|lifecycle|gap, JournalAck.ackedSeq) defined from 05-health-journal §7.1/§7.3 prose — no example payload exists in spec
- SSE payload field types: durability-lag generation is String (generation keys are rfc3339-shaped per 07-durability §9.2), lagSeconds/lastSeq u64, rollout wave u32, secret-rotation version u64
- custom-otel-helm-app margo.yaml is parse-only in tests: it ships unsubstituted {{HELM_REPOSITORY}} mustache placeholders which YAML parses as complex mapping keys that the YAML emitter cannot re-serialize; full round-trip covered by the other three ApplicationDescription fixtures
- Spec-markdown JSON example blocks extracted at test time as fixtures (deployment-status.md, device-capabilities.md, 01-framework §3.3, 05-health-journal §7.3); ellipsis placeholder keys ("...") stripped before parsing as they are elision notation, not wire fields

## A4 margo-package
- Accepted helm.v3 as a valid deploymentProfiles[].type alongside the linkml pattern's helm|compose, since the pinned reference sandbox ships it (custom-otel-helm-app/margo.yaml; matches reeve-types profile_type consts) — anything else is an Error.
- Two-tier severity: linkml-required violations are Errors that fail Package::load_dir; divergences the pinned reference artifacts legitimately ship (parameter target naming a profile id instead of a component name, cpu architecture x86_64 outside the enum, dangling catalog resource links like license.pdf) are Warnings retained on Package.warnings — WIRE-EXACT rule says real artifacts must not be rejected, spec/margo wins on the rule itself.
- Parameter default values are checked against their linked schema's dataType only (Warning on mismatch, int widens to double); range/regex rules apply to user-provided values at configuration time — enforcing them on defaults would reject the nextcloud reference package (default port 80 vs portRange min 1024).
- Catalog resource links resolve with a lexical containment check (.. escaping the package root and absolute/URL links are never resolved to disk); missing files are Warnings.
- Skipped regex-based linkml checks (author email, url schema regexMatch) rather than adding a regex dependency; id/memory/storage patterns are hand-rolled charset checks.
- OCI refs parse oci://registry/repo[:tag][@algo:hex] with port-aware tag splitting (colon in last path segment only); PackageSource::parse also accepts dir:// per the Milestone-1 agent fetcher convention in CLAUDE.md.
- metadata.catalog + at least one organization with a name enforced as Errors per linkml required flags — all six pinned fixtures satisfy this.

## A2 desired-state
- REEVE_UUID_NAMESPACE defined as UUIDv5(NAMESPACE_DNS, "reeve.dev") = 06c32e1b-5365-5c68-80a2-6cccfa182cf8 (src/render.rs) so independent implementations can re-derive it — D2 asked for a defined namespace constant
- merged `enabled` defaults to true when unset: an app defined by any layer is desired unless explicitly switched off (D11's `enabled: true|false` read as explicit-off switch; tests/render.rs enabled_false_removes_app)
- application.yaml is the vendored package margo.yaml bytes VERBATIM, not re-emitted — wire-exact by construction (D2 + CLAUDE.md WIRE-EXACT rule)
- canonical emitter (D3) applies to ALL rendered YAML including wire-exact deployment.yaml — keys sorted lexically; semantics unchanged, determinism guaranteed
- env-targeted parameters (pointer matching ^env. case-insensitive — both ENV.X and env.x appear in pinned fixtures) trigger per-service `env_file: [env/<service>.env]` injection into every compose service; values ride in deployment.yaml parameters and the agent materializes the env files (spec/reeve/10-secrets.md §12.3 'rendered compose references them via env_file')
- v1 renders compose profiles only; helm/helm.v3 profiles are RenderError::UnsupportedProfileType (CLAUDE.md substrate rules: compose first, helm later/never)
- profile selection: app.yaml `profile:` matches DeploymentProfile.id; when absent, the sole profile or the sole compose-typed profile is used, else AmbiguousProfile error
- compose packageLocation must be package-local; URLs/absolute/escaping paths error (D11 no fetch-at-render); when absent, compose.yml then compose.yaml at package root; exactly one component per compose profile in v1
- strict tree authoring: unknown app.yaml keys, stray paths in an app dir (e.g. typo'd params.yml), and params.yaml names not declared by the ApplicationDescription are errors, never silently ignored (single-writer tree, D11/D14)
- ${REEVE_REGISTRY} substituted from device context in compose.yml, files/** (UTF-8 files only; binary files pass through verbatim), and resolved parameter values — never in the verbatim application.yaml (D3/D8)
- package.name/package.version must be YAML strings (quote numeric-looking versions) since version keys the vendored packages/<name>/<version>/ dir
- manifest.yaml field spelling: camelCase deviceId/generation/registryEndpoint/revisions.{hub optional, local} (u64 revision ids), matching reeve-types' camelCase reeve-extension convention
- uuid dep (v5 feature) pinned in the crate's own Cargo.toml, not workspace root, to avoid cross-agent Cargo.toml conflicts

## A3 revision-store
- Two streams as a CHECK-constrained TEXT column on one revisions table (not parallel tables) — single blob table shared, one monotonic id space, simpler queries; §8.2 only requires the streams be independent chains.
- Revision ids are globally monotonic across both streams (INTEGER PRIMARY KEY AUTOINCREMENT); parent chains are per-stream (parent = stream head at commit time).
- Idempotency compared against the STREAM HEAD only: re-committing content identical to an older (non-head) revision creates a new revision — matches D13's 'undo = new revision with prior content'.
- commit() takes the full tree manifest each time (root manifest model per D13), not a delta; empty manifests are allowed.
- read_at returns Ok(None) for a missing path but Err(UnknownRevision) for a missing revision id — distinguishes 'file absent' from caller bugs.
- blame(path) spans both streams, ascending id, comparing each revision's digest for the path against its own parent's; removal shows as digest=None.
- Plain rowid tables with explicit PKs (no WITHOUT ROWID) so the D16 session-extension change capture at server level can track them.
- sha2 0.10 pinned in the crate's own Cargo.toml rather than workspace.dependencies, per build-rule preference to avoid root Cargo.toml conflicts.
- Kept AUTOINCREMENT so rolled-back/killed transactions can never reuse a revision id (append-only invariant survives crashes).

## B1 manifest poll
- Default poll interval 30s (spec pins no value; 02-channel notes latency = poll interval without the channel)
- HTTP client is reqwest 0.12 pinned in-crate with default-features off + rustls-tls (no openssl); 30s request timeout so a poll can never hang
- ManifestVersion persisted as u64 bit-cast to i64 in SQLite; compared only in Rust (documented in schema) — test covers epoch 0xFFFF past bit 63
- manifestVersion 0 rejected as invalid (Margo range [1, 2^64-1], first MUST be 1) and journaled as security; first-ever manifest otherwise accepted at any valid value
- 200 response missing an ETag header falls back to sha256 digest of the body so conditional GET still works
- Error classification: unreachable (network/missing dir) journaled 'info' severity — expected offline operation; non-200/304 status or unparseable body journaled 'error'; both continue from last known state
- Bundle digest violating sha256:<hex> grammar rejects the manifest before version evaluation (it could never verify after pull)
- Acceptance is atomic: manifest_state upsert + journal row in ONE transaction; persist failure means not accepted (floor unchanged, retried next cycle)
- applied_state table created now with D5 phase CHECK (planned/applying/applied/failed/removing/removed) and record_applied/applied_apps accessors so B3 has its contract; B1 only reads
- dir:// sources never advertise capabilities (no server) => pure Margo behavior
- Capability probe runs once per startup (restart covers 'on version change'); result is informational only, convergence never depends on it (§3.2)
- Used axum (already a workspace dep) as the mock test server instead of adding httpmock

## C1 identity/auth
- Placement per task suggestion: Identity/Role/extractors + device-token machinery in device-api; human auth modes + role policy in reeve-server/src/auth/ (D1 seam shared, Law 2 kept)
- refinery is UNLINKABLE here: refinery-core 0.9.2 caps rusqlite at <=0.39 while the workspace pins rusqlite 0.40 (session feature, D16) and libsqlite3-sys `links=sqlite3` forbids two copies — shipped a minimal embedded runner keeping refinery's refinery_schema_history table shape (drop-in swap later), sha256 checksums with drift detection, one tx per migration; documented in db.rs module docs
- db::migrate() returns bool 'applied anything' so C6 can honor D16's migration-cuts-snapshot-generation law
- revision-store keeps self-initializing its own DDL on the shared single DB file (Law 2); server migrations own only server tables; two writer connections arbitrated by WAL+busy_timeout for now
- Device tokens: 'rvd_' + 64 hex (256-bit CSPRNG), stored as plain hex sha256 — sufficient preimage resistance for high-entropy random tokens; argon2 reserved for human passwords
- Identity::Anonymous carries no privilege in the type; REEVE_AUTH=none elevation to admin happens only in mode-aware AppState::effective_role (password-mode anonymous stays role-less)
- Proxy mode refuses startup unless BOTH REEVE_PROXY_USER_HEADER and REEVE_PROXY_TRUSTED_CIDR are set; missing peer address or untrusted peer => 401 fail-closed; optional REEVE_PROXY_ROLE_HEADER: absent => admin (proxy gates access), unparseable => viewer (least privilege)
- First-boot setup token is in-memory only (sha256 in AppState), logged at WARN, single-use, burned on success; crash-only: a restart while zero users exist mints a fresh one — nothing persisted
- Sessions: cookie 'reeve_session' holds raw 'rvh_' token, DB stores sha256; sliding expiry (REEVE_SESSION_TTL_SECS, default 7d) with 60s write granularity to avoid per-request writes; expired sessions purged at startup, no background reaper
- Hand-rolled ~60-line CIDR matcher (IPv4/IPv6, v4-mapped canonicalization) and manual cookie parse/set — avoided ipnet and axum-extra deps
- Cookies are HttpOnly+SameSite=Lax without Secure attribute (TLS termination is deployment-specific); noted for packaging docs
- reeve-server restructured to lib (src/lib.rs) + thin main.rs so integration tests and C2..C12 compose the same bootstrap/router
- V1 migration creates minimal devices + device_tokens tables (auth needs the FK target); C2 enrollment extends devices via a V2 migration, never recreates
- Defaults: REEVE_LISTEN 0.0.0.0:8420, REEVE_DATA_DIR ./data (DB at <data_dir>/reeve.db), REEVE_AUTH password
- login/setup return 404 outside password mode (surface does not exist); logout/me exist in all modes
- argon2id via argon2-0.5 defaults with 128-bit getrandom salt through SaltString::encode_b64 (avoids password-hash rand_core feature); dummy-hash verification on unknown usernames against timing enumeration
- No root Cargo.toml edits — all new deps version-pinned in the two crates (sha2 0.10, hex 0.4, getrandom 0.3, argon2 0.5; dev: tower 0.5, http-body-util 0.1, tempfile 3)

## B2 bundle pull
- Atomic dir swap implemented as content-addressed dirs + symlink flip: bundles unpack to work/, validate, fsync, rename to data_dir/bundles/<hex> (presence there ALWAYS means complete+verified), then one rename(2) of a pre-made relative symlink data_dir/bundle -> bundles/<hex>; kill -9 leaves either old or new target, never neither (rename over a non-empty dir is not atomic on Linux; symlink rename is)
- Recovery direction is roll-FORWARD: the swap is the commitment point; if kill -9 lands between swap and DB record, startup recovery records the disk digest (journal event bundle-rolled-forward); if the recorded bundle vanished from disk (external interference), the record is cleared (notable journal event bundle-state-cleared)
- bundle.url interpretation: for http(s) sources it is the OCI repository base (absolute URL, or server-relative /v2/<name> joined to the configured server origin; a full .../manifests/<digest> URL is trimmed to its repo base); for dir:// sources it is an OCI layout directory path resolved relative to the manifest source dir (blobs/sha256/<hex> read directly) — oras/skopeo layout output is directly consumable
- bundle.digest names the OCI image manifest (not the layer); the manifest bytes are verified against it, then exactly ONE layer with mediaType application/vnd.reeve.render-bundle.v1+tar+gzip is required and its blob verified against the layer digest; zero or multiple render layers fail closed
- Unpack is fail-closed: only Regular and Directory tar entries accepted (symlinks/devices/hardlinks rejected, not skipped); any path component that is absolute or .. rejects the whole bundle; file bytes fsynced at write, dirs fsynced bottom-up before the publishing rename
- Bundle bytes are buffered in memory during fetch (render bundles are config-scale; digest verification needs the whole payload anyway); HTTP client timeout 120s
- manifest bundle:null (zero apps) is a no-op for B2 — the current bundle link is left in place; removal convergence belongs to B3 per D5
- sizeBytes never enforced (advisory per reeve-types BundleRef doc; digest is the sole integrity check)
- main.rs runs BundleStore::sync on NotModified (304) as well as Accepted, because 304 does not imply the bundle is in place — an accept whose pull failed or crashed must retry; sync short-circuits (no fetch, no journal) when the recorded+linked digest already matches
- Unreachable pull failures journal at info severity (Law 5: offline is expected operation), all other pull failures at error; GC of old bundles/ dirs is best-effort (failure costs disk, never correctness)

## C2 enrollment
- Enroll wire types live in reeve_types::reeve::enroll (additive) so device-api (serves) and reeve-agent (calls) share one shape; field names snake_case exactly as written in D4 step 1, response {device_id, device_token, resumed}
- Route placement: POST /api/reeve/v1/enroll in crates/device-api behind an EnrollmentService trait (Law 2: no SQLite in device-api); join-token MANAGEMENT (POST/GET /api/join-tokens, DELETE /api/join-tokens/{token_hash}) in reeve-server behind human auth with role >= operator enforced in-handler
- Idempotent re-run (same unexpired token + same hostname) returns the SAME device with a FRESH token — returning the same token is impossible since only its hash is stored; all prior device tokens are revoked in the same tx (D1: one live credential per device); the re-run consumes NO additional use (matched via devices.enrolled_with = join token hash)
- Atomicity vs D4's 'one SQLite tx': all server-table writes (token validate + use count, device row, token revoke+issue) are ONE IMMEDIATE tx; the revision-store device-layer commit is sequenced AFTER on the store's own connection to the same DB file (Law 2 forbids reaching into its tables). Crash between the two leaves an enrolled device with an absent layer dir — semantically identical to an empty layer (D3: absence = inherit) and repaired by the idempotent retry
- Initial desired state = empty device layer marker layers/30-device.<device_id>/.keep committed to the LOCAL stream as a whole-tree snapshot carrying the head forward; author 'system:enroll'; idempotent (present at head => no new revision)
- Stale flagging (D4 wiped-box): plain-token enrollment sets stale=1 on every other device with the same hostname; idempotent re-run and re-enroll clear stale=0
- device_id = 'dev-' + 16 lowercase hex (64 bits CSPRNG); PK collision fails the insert loudly rather than merging identities
- Join tokens: 'rvj_' + 64 hex, sha256-hashed at rest, defaults 24h TTL / 1 use; DELETE revokes (sets revoked_at) rather than deleting rows (audit trail); enroll 401 is deliberately indistinguishable across unknown/expired/exhausted/revoked
- Re-enroll token creation validates the target device exists (404 otherwise); binding enforced by FK with ON DELETE CASCADE
- Agent: hostname detected from /proc/sys/kernel/hostname, /etc/hostname, then $HOSTNAME (no new dependency); reqwest 'json' feature added to reeve-agent; enroll subcommand parsed by hand (two required flags do not justify a CLI framework); server URL trailing slash trimmed before persisting

## C3 authoring API (first run)
- Layer writes are one batch endpoint — PUT /api/tree/layers/{layer} with body {message?, files: {relpath: base64}} — declarative whole-layer replace (file absent from body is removed from the layer); matches the IaC 'apply this directory' flow better than per-file puts.
- File content on the wire is standard (padded) base64 — binary-safe for package resources such as icons.
- Ownership seam is an enum on AppState: Ownership::Root (v1, owns every authorable path) | Ownership::Gateway{owned_prefixes} (C10 populates from tier config); checked via Ownership::check_write(stream, tree_path) which refuses Stream::Upstream unconditionally at EVERY tier including root; authoring handlers additionally hardcode Stream::Local, so upstream is unwritable structurally, not by convention.
- Gateway prefix matching: exact match, or prefix ending in '.'/'/' matches by starts_with (open families like layers/30-device.), or continuation at a '/' boundary — layers/20-site.plant-a does NOT match layers/20-site.plant-a2.
- Package vendoring (PUT /api/tree/packages/{name}/{version}) materializes the upload to a temp dir and validates with margo_package::Package::load_dir — the same loader the render path uses — BEFORE committing; invalid package => 422 and no revision; warnings returned in the response.
- revision-store commits are whole-tree snapshots, so commit_subtree carries every head path outside the target prefix forward and delegates idempotency to the store's head comparison; response reports {revision, changed} where changed = head moved.
- Revision history is a parent-chain walk from both stream heads merged newest-first (default limit 100, max 1000) — no new revision-store API needed; O(chain) per request accepted for v1.
- Path grammar enforced at the API: layer dir ^[0-9]{2}-<label> with label 1..=128 of [A-Za-z0-9._-] starting alphanumeric and not ending '.'; package name/version segments 1..=100 of [A-Za-z0-9._+-] starting alphanumeric; relative file paths reject empty/'.'/'..' segments, backslash, and control chars.
- Writes require Role::Operator, reads Role::Viewer, both via the existing join_tokens::require_at_least gate; author recorded in the revision is the authenticated username ('anonymous' under REEVE_AUTH=none).

## C3 authoring API
- Chose batch 'apply this layer content' semantics over per-file puts: PUT /api/tree/layers/{layer} body is the COMPLETE layer content (rel-path -> base64 bytes); files absent from the body are removed — matches the IaC 'apply a directory' front door (D14) and makes idempotence trivial (one commit, store idempotent vs head).
- File bytes cross the wire as standard base64 in JSON — binary-safe for package resources (icons) without multipart complexity.
- Layer grammar enforced as NN-<label>: exactly 2 ASCII digits + dash + 1..=128-char label of [A-Za-z0-9._-], starting alphanumeric, not ending in '.'; taxonomy names are convention not engine knowledge (D11/D12).
- Package segments (name, version) restricted to [A-Za-z0-9._+-] starting alphanumeric, so '.'/'..'/path separators are impossible; all relative file paths reject empty/./..' segments, backslash, and control chars.
- Ownership gate is a standalone enum (Ownership::Root | Gateway{owned_prefixes}) on AppState; check_write refuses Stream::Upstream unconditionally at EVERY tier including root (section 8.2), so upstream immutability is structural, not call-site convention. Gateway prefix matching is boundary-aware (layers/20-site.plant-a does NOT match layers/20-site.plant-a2). C10 populates Gateway from tier config; v1 defaults to Root.
- Package vendoring validates by materializing the upload into a tempdir and running margo_package::Package::load_dir — the exact loader the render path uses, so what vendors is what renders; validation warnings are returned in the response body.
- Revision history endpoint chain-walks both streams from their heads and merges newest-first — parent pointers are the stream (D13 append-only), no extra index needed.
- Write role floor Operator, read role floor Viewer, enforced in-handler via join_tokens::require_at_least; commit author is the authenticated username ('anonymous' under REEVE_AUTH=none per D1).

## B3 compose provider
- Removal argv includes -f <retained>/compose.yml alongside -p <name> down (D5 writes just `-p <name> down` but also mandates using the retained copy; explicit -f is the only way to guarantee that and is what an operator would type)
- deploymentId precedence in status reports: State-Manifest entry deployment_id, then deployment.yaml id, then app name — both sources are the same deterministic UUIDv5 per D2, so this only matters when one is absent
- 4xx on status ingest marks the row sent after journaling at error severity — a server-rejected body must not wedge the store-and-forward queue behind a poison row; 5xx and network errors hold everything for retry
- ps observation failure after a successful `up -d` degrades to installed-with-detail rather than failing the apply: the convergence action completed, observation is best-effort
- compose ps mapping: dead/unhealthy/nonzero-exit => failed; created/restarting/paused/removing/health=starting => installing; zero containers => vacuously installed
- retain_applied failure leaves phase at applying (non-terminal) because the retained copy is part of the apply postcondition removal depends on; next pass re-runs (up -d idempotent)
- clippy fix only code change this pass: scoped a test MutexGuard so it drops before the second send_unsent await (report.rs)

## B4 secrets fetch
- Env delivery mechanism: no symlink or project-dir override needed — B3 already stages bundle apps to mutable data_dir/apps/<name>/ (provider cwd = data_dir, -f apps/<name>/compose.yml) and preserves env/ across restages, so render's relative env_file: [env/<service>.env] resolves to agent-local files while bundles/<hex> stays immutable; sync_env writes there (creating apps/<name>/env/ even before first staging).
- Resolve wire shape defined in reeve-types/src/reeve/secrets.rs (additive shared module, one line added to reeve/mod.rs) so C7 shares it: request {"secrets": [names]}, response {"secrets": {name: {value, version}}}; a name the device may not read or that does not exist is simply ABSENT (no existence oracle, §12.6); ResolvedSecret has a manual Debug redacting value.
- Target matching (fixtures show components name PROFILE components, e.g. nextcloud-stack, not compose services): a components entry matching the deployment-profile component name applies to all services of its compose file (Margo-exact, v1 = one compose component per app); an entry matching a compose service name narrows to that service (reeve's additive per-service scoping per D15); empty components = all services.
- Failure semantics expressed by mutating Desired, keeping core converge secrets-ignorant (compiler-enforced ext boundary): resolve failure with prior materialized state pins entry secrets_version to the last-applied value (bundle-content changes still apply from last materialized env, §12.3-endorsed; new sv never recorded as satisfied), while a never-materialized app is dropped from the pass (never started with wrong/empty config) and retried next cycle.
- Network thrift: resolve is skipped when the app is fully converged including secrets (applied phase + bundle-dir hash + secrets_version match + all env files present), so steady-state polling makes zero resolve calls; rotation or bundle change breaks the check.
- Missing secret names in a 200 response are an error (secrets-missing, app held/deferred), not a partial write; non-scalar values, newline-bearing values, and invalid env var names from pointers are materialization errors (env_file format is line-oriented).
- Plain (non-secret) env-targeted parameters are materialized by the same ext module with no resolver (works for dir:// sources); resolver construction requires an enrolled HTTP(S) agent, else None.
- Journal events: secrets-resolved (info, names@versions audit metadata per §12.6), secrets-env-updated (info, file names only), secrets-resolve-unreachable/-unavailable (notable, Law 5 expected), secrets-resolve-failed/-missing/-env-invalid/-env-write-failed (error); secret values never appear in journal, logs, or Debug output (test-asserted).
- Both ENV. and env. pointer spellings accepted (both appear in pinned fixtures); resolved values are inserted literally and never re-scanned for references; unterminated ${secret: passes through as literal text.

## C4 render pipeline
- Bundle change detection = sha256 over rendered file set EXCLUDING manifest.yaml: provenance (revision ids, generation) moves with every commit even when a device's apps do not, so hashing it would defeat D3 no-change-no-bump; when content is unchanged the previous bundle (with its previous declared-input provenance) stays current. Side benefit: gzip-encoding nondeterminism across library upgrades can never cause a spurious bump.
- Artifact storage: dedicated bundle_blobs table (digest PK, content, created_at), NOT reused revision-store blobs — render artifacts are derived state with their own lifecycle (replaced per render, purged when unreferenced at startup); revision blobs are authored append-only history.
- Repo naming: device render bundles at /v2/reeve/bundles/<device_id>; StateManifest.bundle.url is that server-relative repo base (matches agent B2 repo_base resolution).
- Per-device pull authorization: only the digests the device's CURRENT manifest references (OCI manifest digest, layer digest, shared empty-config digest) are pullable; superseded digests 404 immediately — an agent racing a re-render simply re-polls. Cross-device and unknown-digest requests are 404, never 403 (no existence oracle, §10.7).
- If-None-Match uses RFC 9110 STRONG comparison (W/"…" never matches) per the §10.2/B1 contract wording ('strong If-None-Match'), noting RFC 9110 itself prescribes weak comparison for If-None-Match; our ETags are always strong so this only affects deliberately-weak client tags. Unquoted tags tolerated.
- Zero-apps device: bundle served as JSON null (present, never omitted — Margo DeploymentBundleRef rule), no OCI artifact stored; first manifest is version 1 (epoch 0, counter 1).
- Per-device render errors (authoring mistakes) degrade to keep-last-good: previous manifest stays current, rendered_revision is not advanced so every poll retries, error surfaced in render report + logs; they do not block the pass or the last_rendered_local marker (only a new commit can fix them).
- Device enrolled after the last pass / missed by a crashed pass: GET manifest runs ensure_current (render-on-demand) — no hook into enrollment needed. Device layer-chain edits (no API yet) are picked up by POST /api/render.
- REEVE_REGISTRY env (Config.registry_endpoint, default localhost:<listen port>) is the D8 tier registry endpoint fed to RenderContext — a declared render input, never read from env inside the render path.
- Capabilities advertisement returns an empty extensions list: no server-side wire extension is implemented yet, and we only advertise what is compiled in; the cfg!(feature="ext-<name>") append pattern is documented at the single insertion point (server_capabilities in delivery.rs) for future ext modules.
- Render passes run synchronously in the request path (authoring PUT, manifest GET on-demand, POST /api/render); locks are std::sync and never held across await, lock order fixed as revisions-before-db module-wide.
- generation is a declared render input recorded in manifest.yaml, so byte-identical bundles require identical generation too; determinism is asserted for identical input sets (unit pack test + e2e first-render-twice test), while content_digest keeps generation drift from ever causing a version bump.

## C5 status ingest
- Live-path journal stores the VERBATIM request body (handler passes raw_body alongside the parsed manifest) so unknown extensible fields survive forensically instead of a lossy struct re-serialization (05-health-journal §7.2).
- JournalAck contiguity = end of the run starting at the LOWEST journaled seq (first hole stops the ack); empty journal acks 0; an empty batch is a valid ack query.
- Vanilla Margo reports (no reeve key) are accepted per §3.2: they update last_seen and current state but are NOT journaled (no (deviceId,seq) identity exists); an un-seq'd report never overwrites a seq'd current row, and un-seq'd rows update in arrival order (Margo's own connected assumption).
- Equal seq re-applies to deployment_status_current (>= guard, crash-resend friendly) while the journal row is never overwritten (INSERT OR IGNORE) — 'MUST NOT overwrite' binds the journal, current is derived.
- Wrong-device on both POST surfaces is 403 (Margo's untrusted-client analog), unlike delivery's 404 policy — POST paths name the caller's own id, no existence confidentiality at stake; missing/invalid token stays 401 via device_auth.
- Body deviceId mismatching the caller is 422: child-device (see-thru gateway) reporting is known-unmodeled in v1 (01-framework §3.7 note), so the claim is refused, not silently accepted.
- Manifest poll now touches devices.last_seen_at (delivery.rs): the poll IS the liveness signal pre-C8 — an idle agent 304-ing forever must read online. journal/status ingest also touch it.
- Presence threshold is a module constant (DEFAULT_ONLINE_THRESHOLD_SECS=90, 3x default poll interval), not a Config field — avoids touching the shared Config constructor contract mid-swarm; presence::device_presence returns None for unknown devices (unknown, never guessed).
- Journal seq is stored as i64; a wire seq > i64::MAX is a 422 semantic error (StatusIngestError::Invalid), never a silent truncation.
- Backfilled status-kind records whose payload fails to parse as a DeploymentStatusManifest are journaled as-is but skipped for current-state materialization (journal is forensic, current is derived); parseable ones feed the same max-seq upsert.
- Health classification (§7.4 device- vs link-degraded) and health-state events deliberately deferred to the status-stream task; presence.rs documents the seam and preserves the offline='link down, never device dead' asymmetry.
