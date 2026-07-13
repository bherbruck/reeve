//! reeve extension wire types — everything defined by `spec/reeve/`
//! rather than the pinned Margo spec.
//!
//! All of these live on reeve surfaces or ride as the single additive
//! `reeve` key on Margo payloads (spec/reeve/01-framework.md §3.1
//! rule 3, §3.7 surface audit). None are required for Margo-defined
//! flows to complete (§3.2 degradation rule).

pub mod capabilities;
pub mod channel;
pub mod enroll;
pub mod events;
pub mod health;
pub mod logs;
pub mod manifest;
pub mod secrets;
pub mod terminal;
