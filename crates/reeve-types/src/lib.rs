//! reeve-types — Margo-shaped wire types plus reeve extension types.
//!
//! Mirrors the pinned Margo spec in `spec/margo/` (WIRE-EXACT: these
//! types MUST parse real Margo artifacts unmodified) and the reeve
//! spec in `spec/reeve/` (extension types). Every type cites the spec
//! file/section it mirrors in its doc comment.
//!
//! Rules encoded here (spec/reeve/01-framework.md §3.1, §3.6):
//! - serde is unknown-field tolerant: no `deny_unknown_fields`
//!   anywhere; a payload carrying unknown fields is never rejected.
//! - All reeve additive fields on Margo payloads nest under a single
//!   optional top-level `reeve` key so a vanilla receiver can ignore
//!   the whole extension surface by ignoring one key.
//! - serde only — no I/O in this crate.

pub mod margo;
pub mod reeve;

pub use margo::application::ApplicationDescription;
pub use margo::capabilities::DeviceCapabilitiesManifest;
pub use margo::deployment::ApplicationDeployment;
pub use margo::status::{DeploymentState, DeploymentStatusManifest};
pub use reeve::manifest::{ManifestVersion, StateManifest};
