//! reeve extensions — the compiled extension boundary
//! (docs/build-charter.md CODE BOUNDARY; spec/reeve/01-framework.md
//! §3.2 degradation rule).
//!
//! Each extension is a whole module gated by its own `ext-*` cargo
//! feature (default ON). Core code — poll, pull, converge, compose
//! provider, status — MUST NOT depend on anything in here; the
//! `--no-default-features` conformance build enforces that with the
//! compiler. Extensions integrate by being CALLED from the binary
//! shell (main.rs) behind the same feature gate, operating on core's
//! public seams (e.g. mutating [`crate::converge::Desired`] before a
//! converge pass).

#[cfg(feature = "ext-secrets")]
pub mod secrets;
