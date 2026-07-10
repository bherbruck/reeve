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
