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

