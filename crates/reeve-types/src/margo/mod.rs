//! Margo wire types — WIRE-EXACT mirrors of the pinned spec in
//! `spec/margo/`. Field names, structure, and semantics follow the
//! spec exactly; the YAML/JSON in `spec/margo/` and `reference/` are
//! the test fixtures (see `tests/roundtrip.rs`).
//!
//! reeve extensions are additive only and always live under a single
//! optional `reeve` key (spec/reeve/01-framework.md §3.1 rule 3); the
//! complete audit of touched Margo surfaces is 01-framework §3.7.

pub mod application;
pub mod capabilities;
pub mod deployment;
pub mod status;
