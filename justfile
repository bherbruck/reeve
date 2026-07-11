test:
    cargo test --workspace

# Prove every crate stands alone (Law 2)
standalone:
    for c in reeve-server reeve-agent reeve-types margo-package desired-state revision-store device-api; do \
        cargo build -p $c || exit 1; \
    done

ui-dev:
    cd ui && npm run dev

# utoipa openapi.json -> orval-generated TS client + React Query hooks (D1)
gen-api:
    cargo run -p reeve-server -- openapi > ui/openapi.json
    cd ui && npm run gen-api

# CI drift gate (D10): regenerate, then fail if the committed
# openapi.json or generated client differs — the Rust annotations are
# the source of truth, ui/src/api/ is never hand-edited.
check-api-drift: gen-api
    git diff --exit-code ui/openapi.json ui/src/api

# Conformance: core loop with all extensions compiled out (E2). The
# e2e crate's `--no-default-features` build compiles a core-only
# server+agent and runs the ungated end-to-end suite (core_loop / chaos
# / epoch_restore) against them — proving no extension is load-bearing
# for the base loop (docs/build-charter.md CODE BOUNDARY).
conformance:
    cargo build -p reeve-agent --no-default-features
    cargo build -p reeve-server --no-default-features
    cargo test -p reeve-server --no-default-features
    cargo test -p e2e --no-default-features

# vite build before cargo so build.rs embeds a fresh ui/dist
build:
    cd ui && npm run build
    cargo build --release -p reeve-server
