//! Deploy-log wire types — a REEVE EXTENSION (REV-011, `ext-logs`).
//!
//! The one-line failure reason already rides in the Margo-native
//! `DeploymentStatus.error` (margo/status.rs, spec/margo) and is left
//! exactly there. These types carry the FULL captured output of a
//! device's `docker compose up`/`down` for one deployment so an
//! operator can see WHY it failed beyond that one line.
//!
//! Additivity (spec/reeve/01-framework.md §3.1 rule 4): this rides only
//! on new reeve endpoints (`POST /api/reeve/v1/devices/{id}/logs` for
//! the agent's own upload; `GET /api/devices/{id}/logs...` for the
//! operator viewer). It NEVER appears in a Margo status body and never
//! shadows a Margo path. A vanilla WFM/agent knows nothing of it and is
//! unaffected (§3.2 degradation).

use serde::{Deserialize, Serialize};

/// What the converge attempt this log covers resulted in — mirrors the
/// agent's per-deployment outcome, independent of the Margo state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum DeployLogOutcome {
    /// The stack was brought up / updated successfully.
    Applied,
    /// The compose invocation failed (the interesting case).
    Failed,
    /// The stack was torn down (`compose down`).
    Removed,
}

/// Which compose phase produced this output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum DeployLogPhase {
    /// `docker compose up`.
    Up,
    /// `docker compose down`.
    Down,
}

/// The agent's upload body: one captured compose run for one
/// deployment. `text` is the combined stdout+stderr the agent
/// captured; `truncated` is true when the agent already clipped it to
/// its own cap before sending (the server additionally rejects bodies
/// over its accept cap).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "camelCase")]
pub struct DeployLogUpload {
    /// The Margo deployment id this run converged (same value the
    /// device reports in `DeploymentStatusManifest.deploymentId`).
    pub deployment_id: String,
    /// The application id (Margo `applicationId`) for display grouping.
    pub app_id: String,
    pub outcome: DeployLogOutcome,
    pub phase: DeployLogPhase,
    /// Process exit code when known (`None` if the agent never got one
    /// — e.g. the binary could not be spawned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// True if the agent clipped `text` before upload.
    pub truncated: bool,
    /// When the agent captured this run (RFC 3339, device clock —
    /// preserved verbatim like the journal's `observedAt`).
    pub captured_at: String,
    /// Combined captured output (stdout+stderr). Plain text.
    pub text: String,
}

/// Listing/reference metadata for one stored log — everything the UI
/// needs for a list row, minus the (potentially large) text body,
/// which is fetched on demand by `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "camelCase")]
pub struct DeployLogMeta {
    /// Opaque server-assigned log id (the handle for the GET-one route).
    pub id: String,
    pub deployment_id: String,
    pub app_id: String,
    pub outcome: DeployLogOutcome,
    pub phase: DeployLogPhase,
    /// Byte length of the stored text.
    pub size_bytes: u64,
    pub truncated: bool,
    /// The device-captured RFC 3339 timestamp, preserved verbatim.
    pub captured_at: String,
}

/// GET-one response: metadata + the full text. (The route MAY also
/// serve raw `text/plain`; this is the JSON shape the UI consumes.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "camelCase")]
pub struct DeployLogContent {
    pub meta: DeployLogMeta,
    pub text: String,
}

/// GET-list response: metas newest-first for one (device, deployment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "camelCase")]
pub struct DeployLogList {
    pub logs: Vec<DeployLogMeta>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_round_trips_camel_case() {
        let up = DeployLogUpload {
            deployment_id: "web-deploy".into(),
            app_id: "web".into(),
            outcome: DeployLogOutcome::Failed,
            phase: DeployLogPhase::Up,
            exit_code: Some(1),
            truncated: false,
            captured_at: "2026-07-13T10:00:00Z".into(),
            text: "Error: pull access denied\n".into(),
        };
        let json = serde_json::to_value(&up).unwrap();
        assert_eq!(json["deploymentId"], "web-deploy");
        assert_eq!(json["appId"], "web");
        assert_eq!(json["outcome"], "failed");
        assert_eq!(json["phase"], "up");
        assert_eq!(json["exitCode"], 1);
        let back: DeployLogUpload = serde_json::from_value(json).unwrap();
        assert_eq!(back, up);
    }

    #[test]
    fn exit_code_omitted_when_absent() {
        let up = DeployLogUpload {
            deployment_id: "d".into(),
            app_id: "a".into(),
            outcome: DeployLogOutcome::Removed,
            phase: DeployLogPhase::Down,
            exit_code: None,
            truncated: true,
            captured_at: "2026-07-13T10:00:00Z".into(),
            text: String::new(),
        };
        let json = serde_json::to_value(&up).unwrap();
        assert!(json.get("exitCode").is_none(), "absent exitCode must be omitted");
        assert_eq!(json["outcome"], "removed");
        assert_eq!(json["phase"], "down");
    }

    #[test]
    fn meta_shape() {
        let meta = DeployLogMeta {
            id: "abc123".into(),
            deployment_id: "web-deploy".into(),
            app_id: "web".into(),
            outcome: DeployLogOutcome::Applied,
            phase: DeployLogPhase::Up,
            size_bytes: 42,
            truncated: false,
            captured_at: "2026-07-13T10:00:00Z".into(),
        };
        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["id"], "abc123");
        assert_eq!(json["sizeBytes"], 42);
        assert_eq!(json["outcome"], "applied");
        let back: DeployLogMeta = serde_json::from_value(json).unwrap();
        assert_eq!(back, meta);
    }
}
