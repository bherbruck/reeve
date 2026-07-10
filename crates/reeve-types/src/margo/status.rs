//! `DeploymentStatusManifest` — the device→WFM deployment status
//! report.
//!
//! Spec sources (pinned):
//! - `spec/margo/system-design/specification/margo-management-interface/deployment-status.md`
//!   (request body attributes + example manifest — the example JSON
//!   in that file is a round-trip test fixture).
//! - `spec/margo/system-design/specification/margo-management-interface/workload-management-api-1.0.0.yaml`
//!   (`DeploymentStatusManifest`, `ComponentStatus` schemas).
//!
//! reeve ingests this manifest on Margo's path and payload shape
//! (spec/reeve/01-framework.md §3.8); rev-004/1 adds one additive
//! `reeve` object (spec/reeve/05-health-journal.md §7.3).

use serde::{Deserialize, Serialize};

use crate::reeve::health::ReeveStatusExtension;

/// `apiVersion` used by the pinned spec example
/// (`deployment-status.md`).
pub const DEPLOYMENT_STATUS_API_VERSION: &str = "deployment.margo.org/v1alpha1";
/// `kind` — MUST be `DeploymentStatusManifest` (`deployment-status.md`).
pub const DEPLOYMENT_STATUS_KIND: &str = "DeploymentStatusManifest";

/// Deployment/component state
/// (`workload-management-api-1.0.0.yaml` `DeploymentStatusManifest.
/// status.state` enum; `deployment-status.md` "Status Attributes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentState {
    Pending,
    Installing,
    Installed,
    Removing,
    Removed,
    Failed,
}

impl DeploymentState {
    /// Severity for computing the overall deployment state.
    ///
    /// `deployment-status.md`: "The overall deployment status MUST
    /// reflect the most severe of the components states, following
    /// this precedence: failed > removing > installing > pending >
    /// removing > installed." The spec text lists `removing` twice
    /// and omits `removed`; the second `removing` is read as
    /// `removed` (the only enum member otherwise absent), giving:
    /// failed > removing > installing > pending > removed >
    /// installed.
    pub fn severity(self) -> u8 {
        match self {
            DeploymentState::Failed => 5,
            DeploymentState::Removing => 4,
            DeploymentState::Installing => 3,
            DeploymentState::Pending => 2,
            DeploymentState::Removed => 1,
            DeploymentState::Installed => 0,
        }
    }

    /// The most severe of a set of component states, per the
    /// precedence rule above; `None` for an empty set.
    pub fn most_severe(states: impl IntoIterator<Item = DeploymentState>) -> Option<DeploymentState> {
        states.into_iter().max_by_key(|s| s.severity())
    }
}

/// `DeploymentStatusManifest` request body
/// (`deployment-status.md` "Request Body Attributes"; posted to
/// `POST /api/v1/clients/{clientId}/deployments/{deploymentId}/status`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStatusManifest {
    pub api_version: String,
    pub kind: String,
    /// UUID of the deployment specification, assigned by the WFM.
    pub deployment_id: String,
    /// Required only when reporting on behalf of a child device
    /// (`deployment-status.md`); full hierarchy path when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub status: DeploymentStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<ComponentStatus>,
    /// reeve additive extension object, rev-004/1
    /// (spec/reeve/05-health-journal.md §7.3). A vanilla WFM ignores
    /// it; all Margo-defined fields above are present and unchanged
    /// (spec/reeve/01-framework.md §3.7 audit row REV-004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reeve: Option<ReeveStatusExtension>,
}

/// Overall deployment `status` (`deployment-status.md` "Status
/// Attributes"; an object, per the example manifest and the OpenAPI
/// schema — the attribute table's `[]status` type is contradicted by
/// both and is read as a spec typo).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStatus {
    pub state: DeploymentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<StatusError>,
}

/// `components[]` entry (`deployment-status.md` "Component
/// Attributes"; `ComponentStatus` in
/// `workload-management-api-1.0.0.yaml`). MUST contain one entry per
/// component of the referenced `ApplicationDeployment`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentStatus {
    pub name: String,
    pub state: DeploymentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<StatusError>,
}

/// `error` element (`deployment-status.md` "Error Attributes").
/// Reserved gateway-generated codes: 101 unknown child device, 102
/// child device unreachable, 103 autonomous placement not supported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Device id (full hierarchy) or component/deployment name that
    /// generated the error (`deployment-status.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_severe_follows_precedence() {
        use DeploymentState::*;
        assert_eq!(DeploymentState::most_severe([Installed, Pending, Failed]), Some(Failed));
        assert_eq!(DeploymentState::most_severe([Installed, Removed]), Some(Removed));
        assert_eq!(DeploymentState::most_severe([Installing, Removing]), Some(Removing));
        assert_eq!(DeploymentState::most_severe([]), None);
    }

    #[test]
    fn state_wire_names_are_lowercase() {
        assert_eq!(serde_json::to_string(&DeploymentState::Pending).unwrap(), "\"pending\"");
        assert_eq!(
            serde_json::from_str::<DeploymentState>("\"failed\"").unwrap(),
            DeploymentState::Failed
        );
    }
}
