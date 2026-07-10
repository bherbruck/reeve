//! `ApplicationDeployment` — the per-device desired-state document.
//!
//! Spec sources (pinned):
//! - `spec/margo/src/specification/margo-management-interface/resources/examples/valid/DesiredState-001.yaml`
//! - `spec/margo/src/specification/margo-management-interface/resources/examples/valid/DesiredState-002.yaml`
//! - `spec/margo/system-design/specification/margo-management-interface/workload-management-api-1.0.0.yaml`
//!   (`appDeploymentManifest`, `appDeploymentMetadata`,
//!   `appDeploymentSpec` schemas).
//!
//! reeve emits these files wire-exact inside the render bundle
//! (spec/reeve/01-framework.md §3.7 "render bundle" row;
//! docs/decisions/tree-render.md D2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::application::{Component, Parameter};

/// `apiVersion` used by the pinned spec examples for
/// `ApplicationDeployment` (`DesiredState-001.yaml`).
pub const APPLICATION_DEPLOYMENT_API_VERSION: &str = "application.margo.org/v1alpha1";
/// `kind` for an application deployment document.
pub const APPLICATION_DEPLOYMENT_KIND: &str = "ApplicationDeployment";

/// Margo `ApplicationDeployment` (`DesiredState-001.yaml`;
/// `appDeploymentManifest` in `workload-management-api-1.0.0.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationDeployment {
    pub api_version: String,
    pub kind: String,
    /// Deployment UUID, assigned by the WFM (`DesiredState-001.yaml`
    /// carries it top-level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub metadata: DeploymentMetadata,
    pub spec: DeploymentSpec,
}

/// `metadata` (`appDeploymentMetadata` in
/// `workload-management-api-1.0.0.yaml`; example in
/// `DesiredState-001.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentMetadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Target device id; `DeviceId_with_asterisk` grammar
    /// (`workload-management-api-1.0.0.yaml`):
    /// `{id}[/{id}...][/*]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
}

/// `spec` (`appDeploymentSpec` in
/// `workload-management-api-1.0.0.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentSpec {
    /// MUST match the associated application description's id
    /// (`^[-a-z0-9]{1,200}$` per the OpenAPI schema).
    pub application_id: String,
    pub deployment_profile: DeploymentProfileSpec,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, Parameter>,
}

/// `spec.deploymentProfile` — the single profile selected for this
/// device (`appDeploymentProfile` in
/// `workload-management-api-1.0.0.yaml`). Unlike
/// `ApplicationDescription.deploymentProfiles[]` it carries no
/// profile `id` in the pinned examples; `type` follows the same
/// string values (see `margo::application::profile_type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentProfileSpec {
    #[serde(rename = "type")]
    pub profile_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
}
