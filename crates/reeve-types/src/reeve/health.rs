//! Device Health & Status Journal wire types — REV-004
//! (spec/reeve/05-health-journal.md).
//!
//! Live path: one additive `reeve` object on the Margo
//! `DeploymentStatusManifest` (§7.3). Backfill path: journal batches
//! to `POST /api/reeve/v1/journal/{deviceId}` on a reeve surface.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The additive `reeve` object on a `DeploymentStatusManifest`
/// (spec/reeve/05-health-journal.md §7.3, rev-001 example):
///
/// ```json
/// "reeve": { "observedAt": "2026-07-10T06:12:03Z", "seq": 48211,
///            "health": { "...": "..." } }
/// ```
///
/// The server uses `observedAt`/`seq` to place the report in history
/// and detect records it already holds. A vanilla WFM ignores the
/// whole object (spec/reeve/01-framework.md §3.7 audit row REV-004).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReeveStatusExtension {
    /// Original timestamp (RFC 3339) — assigned when journaled on
    /// the device, never rewritten (§7 "original timestamp").
    pub observed_at: String,
    /// Monotonic per-device sequence number (§7.1).
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthSample>,
}

/// A point-in-time health sample (spec/reeve/05-health-journal.md
/// §7.2): disk usage/free per relevant filesystem, memory usage,
/// load averages, per-workload container restart counts, agent
/// version, clock skew vs the server. "Fields are extensible;
/// receivers MUST ignore unknown sample fields" — unknown fields are
/// captured (and re-emitted) via `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HealthSample {
    /// Per-filesystem usage, keyed by mount point / identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<BTreeMap<String, DiskSample>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySample>,
    /// Load averages (1/5/15 min).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<Vec<f64>>,
    /// Per-workload container restart counts, from the active
    /// Provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    /// Clock skew versus the server in milliseconds, measured
    /// opportunistically when connected (§7.2 — skew matters because
    /// original timestamps are device-assigned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_skew_ms: Option<i64>,
    /// Extensible sample fields (§7.2): preserved verbatim so the
    /// payload round-trips without loss.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Disk usage/free for one filesystem (spec/reeve/05-health-journal.md
/// §7.2). Inner field names are reeve-chosen (the spec pins the
/// sample's semantics, not its sub-shape); extensible like the rest
/// of the sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiskSample {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Memory usage (spec/reeve/05-health-journal.md §7.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySample {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Kind of a journal record (spec/reeve/05-health-journal.md §7.1):
/// status reports, health samples, agent lifecycle marks (start,
/// converge begin/end, provider errors), and gap marks (forced
/// eviction of unacknowledged records — so the server can
/// distinguish "evicted" from "never happened").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalRecordKind {
    Status,
    Health,
    Lifecycle,
    Gap,
}

/// One journal record in a backfill batch
/// (spec/reeve/05-health-journal.md §7.3 backfill path). Idempotency
/// key is `(deviceId, seq)` — deviceId travels in the endpoint path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalRecord {
    /// Monotonic per-device sequence number (§7.1).
    pub seq: u64,
    /// Original timestamp (RFC 3339), assigned when journaled.
    pub observed_at: String,
    pub kind: JournalRecordKind,
    /// The journaled payload: a full `DeploymentStatusManifest` for
    /// `status`, a [`HealthSample`] for `health`, a free-form object
    /// for `lifecycle`, absent for `gap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

/// Request body of `POST /api/reeve/v1/journal/{deviceId}`
/// (spec/reeve/05-health-journal.md §7.3): unacknowledged records in
/// batches ordered by sequence number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalBatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<JournalRecord>,
}

/// Response: "the server replies with the highest contiguously
/// ingested sequence number; that acknowledgement is what permits
/// journal eviction" (spec/reeve/05-health-journal.md §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalAck {
    pub acked_seq: u64,
}

/// Server-side health classification
/// (spec/reeve/05-health-journal.md §7.4). `Unknown` (offline window
/// not yet backfilled) MUST be surfaced as unknown, never silently
/// assumed healthy or dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    Healthy,
    Degraded,
    Unknown,
}

/// What is degraded (spec/reeve/05-health-journal.md §7.4):
/// `Device` — the device itself breaches thresholds; `Link` — the
/// path was down but backfill shows the device was fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthKind {
    Device,
    Link,
}
