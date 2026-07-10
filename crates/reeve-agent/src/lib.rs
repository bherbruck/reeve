//! reeve-agent — the per-device agent: fetch -> diff -> apply ->
//! report. Manifest poll loop (B1), bundle pull (B2), and the
//! compose-provider converge loop + status reporting (B3).
//!
//! Design laws in force (CLAUDE.md):
//! - **Offline-first (Law 5)**: the agent assumes it is offline more
//!   than online. Every network call in this crate has a "couldn't
//!   reach — continue from last known state" path, and first
//!   converge never blocks on the network.
//! - **Crash-only (Law 3)**: no shutdown ceremony. State is
//!   agent.db (SQLite WAL, [`state`]); startup IS recovery.
//! - **Spec-grounded (Law 1)**: poll semantics from
//!   spec/reeve/08-packaging.md §10.2 and docs/decisions/delivery.md
//!   D13; capability discovery from spec/reeve/01-framework.md
//!   §3.2/§3.3; enrollment/config shape from docs/decisions/agent.md.

pub mod bundle;
pub mod config;
pub mod converge;
pub mod enroll;
pub mod ext;
pub mod poll;
pub mod provider;
pub mod report;
pub mod source;
pub mod state;

pub use bundle::{BundleSource, BundleStore, PullError};
pub use config::AgentConfig;
pub use converge::{AppReport, Desired, converge, resolve_desired};
pub use enroll::{EnrollCmdError, EnrollOpts, enroll};
pub use poll::{PollOutcome, VersionDecision, evaluate_version, poll_once};
pub use provider::{AppStatus, CommandComposeProvider, Provider, ProviderError};
pub use report::{StatusSink, record_reports};
pub use source::{ManifestSource, PollResponse, SourceError};
pub use state::{AgentDb, Severity};
