//! desired-state — THE crate: compile a layered deployment tree into
//! the rendered per-device file set.
//!
//! Render is a pure function (docs/decisions/tree-render.md D3):
//! `(tree contents at a revision, device context) -> file set`. Zero
//! I/O: the tree is an in-memory map of path -> bytes ([`FileSet`],
//! the D11 revision-content layout `layers/**` + `packages/**`), the
//! output is the same shape in the D2 bundle layout
//! (`manifest.yaml`, `apps/<name>/{deployment.yaml, application.yaml,
//! compose.yml, files/**}`). No clock, no environment reads, no
//! network, no secret values (spec/reeve/10-secrets.md §12.1 —
//! secrets render as `${secret:<name>}` references).
//!
//! Merge semantics (D3, normative — the table tests in
//! `tests/render.rs` ARE the spec for this crate):
//! - Layer order: lexical by the numeric dir-name prefix
//!   (fleet -> class -> region -> site -> device by convention, D12);
//!   later layer wins.
//! - Maps deep-merge key by key; lists REPLACE; explicit `null`
//!   deletes a key; scalars override; `files/` replace whole-file.
//! - Canonical emitter: keys sorted lexically, block style, LF,
//!   trailing newline, no anchors — same inputs => byte-identical
//!   render => same bundle digest.
//!
//! Wire fidelity: `deployment.yaml` / `application.yaml` are
//! wire-exact Margo documents (`reeve-types`); the overlay tree and
//! this render pipeline are OURS ENTIRELY (the Margo spec is silent
//! on how desired state is derived — CLAUDE.md spec-fidelity rules).

mod emit;
mod error;
mod merge;
mod render;

pub use emit::{canonicalize, to_canonical_yaml};
pub use error::RenderError;
pub use merge::merge_value;
pub use render::{FileSet, REEVE_UUID_NAMESPACE, RenderContext, deployment_id, render};
