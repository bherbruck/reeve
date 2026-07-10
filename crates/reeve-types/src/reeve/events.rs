//! Live Status Stream event types — REV-003
//! (spec/reeve/04-status-stream.md §6.3, rev-003/1 event table).
//!
//! Events ride SSE (`GET /api/reeve/v1/events`): the event NAME
//! travels in the SSE `event:` field, the payload as JSON in `data:`.
//! Every payload includes `ts` (RFC 3339 server time). Events are
//! cache-invalidation hints, droppable and at-most-once (§6.2);
//! payloads identify entities and MUST NOT be treated as the new
//! entity state (§6.3). Unknown event types MUST be ignored by
//! clients — [`SseEvent::from_wire`] returns `Ok(None)` for them.

use serde::{Deserialize, Serialize};

use crate::margo::status::DeploymentState;
use crate::reeve::health::{HealthKind, HealthState};

/// `reset` (§6.2): replay from `Last-Event-ID` not possible; the
/// client MUST treat all cached state as stale and refetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetEvent {
    pub ts: String,
}

/// Device presence state (`device-presence` payload, §6.3;
/// spec/reeve/02-channel.md §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceState {
    Online,
    Offline,
}

/// `device-presence` (§6.3): channel opens/drops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevicePresenceEvent {
    pub ts: String,
    pub device_id: String,
    pub state: PresenceState,
    /// RFC 3339 — since when the device has been in `state`.
    pub since: String,
}

/// `deployment-status` (§6.3): an ingested manifest changed a
/// deployment's overall state. `state` is the Margo enum
/// (`margo::status::DeploymentState`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStatusEvent {
    pub ts: String,
    pub device_id: String,
    pub deployment_id: String,
    pub state: DeploymentState,
}

/// Terminal session lifecycle phase (`terminal-session` payload,
/// §6.3; spec/reeve/03-terminal.md §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalPhase {
    Requested,
    Opened,
    Closed,
    Denied,
}

/// `terminal-session` (§6.3): session lifecycle transition. Exposes
/// session METADATA only (who, which device, when) — intentional
/// audit visibility; MUST NOT ever carry session content (§6.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSessionEvent {
    pub ts: String,
    pub session_id: String,
    pub device_id: String,
    pub phase: TerminalPhase,
    pub user: String,
}

/// `health-state` (§6.3): health classification changed
/// (spec/reeve/05-health-journal.md §7.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthStateEvent {
    pub ts: String,
    pub device_id: String,
    pub state: HealthState,
    pub kind: HealthKind,
}

/// Outcome of a verify-restore run (`verify-restore` payload, §6.3;
/// spec/reeve/07-durability.md §9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyRestoreOutcome {
    Ok,
    Failed,
}

/// `verify-restore` (§6.3): a verify-restore run completed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRestoreEvent {
    pub ts: String,
    pub outcome: VerifyRestoreOutcome,
    /// RFC 3339 timestamp of the snapshot that was verified.
    pub snapshot_ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `durability-lag` (§6.3): changeset upload lag crossed/cleared a
/// threshold — ops dashboard signal (spec/reeve/07-durability.md
/// §9.3: changesets are keyed by generation id + monotonic seq).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurabilityLagEvent {
    pub ts: String,
    /// Snapshot generation id (spec/reeve/07-durability.md §9.2 —
    /// generation keys are string-shaped, e.g. `<rfc3339>-<schema>`).
    pub generation: String,
    /// Last uploaded changeset sequence within the generation.
    pub last_seq: u64,
    pub lag_seconds: u64,
}

/// Rollout/wave phase (`rollout` payload, §6.3;
/// spec/reeve/09-rollouts.md §11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RolloutPhase {
    Started,
    Gated,
    Paused,
    Completed,
    Failed,
}

/// `rollout` (§6.3): rollout/wave transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RolloutEvent {
    pub ts: String,
    pub rollout_id: String,
    /// Wave index within the rollout's ordered wave list
    /// (spec/reeve/09-rollouts.md).
    pub wave: u32,
    pub phase: RolloutPhase,
}

/// Secret rotation propagation state (`secret-rotation` payload,
/// §6.3; spec/reeve/10-secrets.md §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretRotationState {
    Propagating,
    Converged,
}

/// `secret-rotation` (§6.3): a secret version changed / all affected
/// devices report converged. Carries metadata only, never values
/// (§6.4; spec/reeve/10-secrets.md).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretRotationEvent {
    pub ts: String,
    pub secret_name: String,
    pub scope: String,
    /// Secret version (bumped on rotation, spec/reeve/10-secrets.md).
    pub version: u64,
    pub state: SecretRotationState,
}

/// One SSE event: the pair of wire event name (`event:` field) and
/// typed JSON payload (`data:` field) — the complete rev-003/1 table
/// (spec/reeve/04-status-stream.md §6.3).
#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    Reset(ResetEvent),
    DevicePresence(DevicePresenceEvent),
    DeploymentStatus(DeploymentStatusEvent),
    TerminalSession(TerminalSessionEvent),
    HealthState(HealthStateEvent),
    VerifyRestore(VerifyRestoreEvent),
    DurabilityLag(DurabilityLagEvent),
    Rollout(RolloutEvent),
    SecretRotation(SecretRotationEvent),
}

/// Wire event names (spec/reeve/04-status-stream.md §6.3 table).
pub mod event_type {
    pub const RESET: &str = "reset";
    pub const DEVICE_PRESENCE: &str = "device-presence";
    pub const DEPLOYMENT_STATUS: &str = "deployment-status";
    pub const TERMINAL_SESSION: &str = "terminal-session";
    pub const HEALTH_STATE: &str = "health-state";
    pub const VERIFY_RESTORE: &str = "verify-restore";
    pub const DURABILITY_LAG: &str = "durability-lag";
    pub const ROLLOUT: &str = "rollout";
    pub const SECRET_ROTATION: &str = "secret-rotation";
}

impl SseEvent {
    /// The SSE `event:` field value.
    pub fn event_type(&self) -> &'static str {
        match self {
            SseEvent::Reset(_) => event_type::RESET,
            SseEvent::DevicePresence(_) => event_type::DEVICE_PRESENCE,
            SseEvent::DeploymentStatus(_) => event_type::DEPLOYMENT_STATUS,
            SseEvent::TerminalSession(_) => event_type::TERMINAL_SESSION,
            SseEvent::HealthState(_) => event_type::HEALTH_STATE,
            SseEvent::VerifyRestore(_) => event_type::VERIFY_RESTORE,
            SseEvent::DurabilityLag(_) => event_type::DURABILITY_LAG,
            SseEvent::Rollout(_) => event_type::ROLLOUT,
            SseEvent::SecretRotation(_) => event_type::SECRET_ROTATION,
        }
    }

    /// The SSE `data:` field value (JSON payload).
    pub fn data_json(&self) -> serde_json::Result<String> {
        match self {
            SseEvent::Reset(p) => serde_json::to_string(p),
            SseEvent::DevicePresence(p) => serde_json::to_string(p),
            SseEvent::DeploymentStatus(p) => serde_json::to_string(p),
            SseEvent::TerminalSession(p) => serde_json::to_string(p),
            SseEvent::HealthState(p) => serde_json::to_string(p),
            SseEvent::VerifyRestore(p) => serde_json::to_string(p),
            SseEvent::DurabilityLag(p) => serde_json::to_string(p),
            SseEvent::Rollout(p) => serde_json::to_string(p),
            SseEvent::SecretRotation(p) => serde_json::to_string(p),
        }
    }

    /// Parse the (`event:`, `data:`) pair. `Ok(None)` for an unknown
    /// event name — "unknown event types MUST be ignored by clients"
    /// (§6.3); `Err` only for a malformed payload of a KNOWN type.
    pub fn from_wire(event: &str, data: &str) -> serde_json::Result<Option<SseEvent>> {
        Ok(Some(match event {
            event_type::RESET => SseEvent::Reset(serde_json::from_str(data)?),
            event_type::DEVICE_PRESENCE => SseEvent::DevicePresence(serde_json::from_str(data)?),
            event_type::DEPLOYMENT_STATUS => SseEvent::DeploymentStatus(serde_json::from_str(data)?),
            event_type::TERMINAL_SESSION => SseEvent::TerminalSession(serde_json::from_str(data)?),
            event_type::HEALTH_STATE => SseEvent::HealthState(serde_json::from_str(data)?),
            event_type::VERIFY_RESTORE => SseEvent::VerifyRestore(serde_json::from_str(data)?),
            event_type::DURABILITY_LAG => SseEvent::DurabilityLag(serde_json::from_str(data)?),
            event_type::ROLLOUT => SseEvent::Rollout(serde_json::from_str(data)?),
            event_type::SECRET_ROTATION => SseEvent::SecretRotation(serde_json::from_str(data)?),
            _ => return Ok(None),
        }))
    }
}
