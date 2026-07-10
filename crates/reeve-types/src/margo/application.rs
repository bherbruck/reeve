//! `ApplicationDescription` — the Margo application package manifest
//! (`margo.yaml`).
//!
//! Spec sources (pinned):
//! - `spec/margo/src/specification/applications/resources/examples/valid/ApplicationDescription-001.yaml`
//! - `spec/margo/src/specification/applications/resources/examples/valid/ApplicationDescription-002.yaml`
//! - `spec/margo/src/specification/applications/application-description.linkml.yaml`
//! - `spec/margo/system-design/specification/margo-management-interface/workload-management-api-1.0.0.yaml`
//!   (`appDeploymentProfile`, `appDeploymentParams` schemas — the
//!   parameter/target/component shapes are shared with
//!   `ApplicationDeployment`).
//!
//! Real-world fixtures also parsed by the round-trip tests:
//! `reference/poc/tests/artefacts/*/margo-package/margo.yaml`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `apiVersion` used by the pinned spec examples for
/// `ApplicationDescription` (`ApplicationDescription-001.yaml`).
pub const APPLICATION_DESCRIPTION_API_VERSION: &str = "margo.org/v1-alpha1";
/// `kind` for an application description document.
pub const APPLICATION_DESCRIPTION_KIND: &str = "ApplicationDescription";

/// Margo `ApplicationDescription` (root of `margo.yaml`).
///
/// WIRE-EXACT NOTE on `id`: the pinned spec examples
/// (`ApplicationDescription-001.yaml`) carry a top-level `id`;
/// artifacts in `reference/` (`nextcloud-compose/margo-package/
/// margo.yaml`) carry `metadata.id` instead. Both are accepted; use
/// [`ApplicationDescription::effective_id`] to read whichever is
/// present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationDescription {
    pub api_version: String,
    pub kind: String,
    /// Top-level application id (spec examples form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub metadata: ApplicationMetadata,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployment_profiles: Vec<DeploymentProfile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, Parameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Configuration>,
}

impl ApplicationDescription {
    /// The application id, whether spelled top-level (spec examples)
    /// or under `metadata` (reference sandbox artifacts).
    pub fn effective_id(&self) -> Option<&str> {
        self.id.as_deref().or(self.metadata.id.as_deref())
    }
}

/// `metadata` of an `ApplicationDescription`
/// (`ApplicationDescription-001.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationMetadata {
    /// Application id in `metadata` position (reference-sandbox form;
    /// see [`ApplicationDescription::effective_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<Catalog>,
}

/// `metadata.catalog` (`ApplicationDescription-001.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<CatalogApplication>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub author: Vec<CatalogContact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub organization: Vec<CatalogContact>,
}

/// `metadata.catalog.application` (`ApplicationDescription-001.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogApplication {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// `metadata.catalog.author[]` / `metadata.catalog.organization[]`
/// entries (`ApplicationDescription-001.yaml`): authors carry
/// `name`+`email`, organizations `name`+`site`; one tolerant shape
/// covers both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogContact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

/// `deploymentProfiles[]` (`ApplicationDescription-002.yaml`;
/// `appDeploymentProfile` in `workload-management-api-1.0.0.yaml`).
///
/// `type` is a `String`, not an enum: the OpenAPI enum says
/// `helm`/`compose` but the pinned reference sandbox ships
/// `helm.v3` (`custom-otel-helm-app/margo-package/margo.yaml`,
/// `AppDeploymentProfileType` in `reference/standard/generatedCode/
/// wfm/sbi/models.go`). Constants below cover the known values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentProfile {
    #[serde(rename = "type")]
    pub profile_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_resources: Option<RequiredResources>,
}

/// Known `deploymentProfiles[].type` values in the pinned spec and
/// reference artifacts.
pub mod profile_type {
    pub const HELM: &str = "helm";
    pub const HELM_V3: &str = "helm.v3";
    pub const COMPOSE: &str = "compose";
}

/// A deployment profile component
/// (`helmApplicationDeploymentProfileComponent` /
/// `composeApplicationDeploymentProfileComponent` in
/// `workload-management-api-1.0.0.yaml`).
///
/// `properties` stays a generic YAML value: the property set is
/// profile-type-specific (helm: `repository`, `revision`, `wait`,
/// `timeout`; compose: `packageLocation`, `keyLocation`, ...) and
/// fixtures disagree on scalar types (`wait: true` in
/// `ApplicationDescription-001.yaml`, `wait: "true"` in
/// `DesiredState-001.yaml`) — preserving the value verbatim is the
/// wire-exact behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_yaml_ng::Value>,
}

/// `deploymentProfiles[].requiredResources`
/// (`ApplicationDescription-002.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<RequiredCpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peripherals: Option<Vec<super::capabilities::Peripheral>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interfaces: Option<Vec<super::capabilities::CommunicationInterface>>,
}

/// `requiredResources.cpu` (`ApplicationDescription-002.yaml`:
/// fractional cores allowed, e.g. `1.5`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredCpu {
    pub cores: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architectures: Option<Vec<String>>,
}

/// A named parameter (`parameters.<name>` in
/// `ApplicationDescription-001.yaml`; `appParameterValue` in
/// `workload-management-api-1.0.0.yaml`). Shared by
/// `ApplicationDescription` and `ApplicationDeployment`.
///
/// `value` is a generic value: fixtures carry strings, integers and
/// doubles (`pollFrequency: 30`, `cpuLimit: 1`, `value: Hello`).
/// reeve's secret convention (spec/reeve/01-framework.md §3.7,
/// REV-009) rides in-band: a secret-typed parameter's `value` is the
/// plain string `${secret:<name>}` — no field added or retyped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_yaml_ng::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<ParameterTarget>,
}

/// `parameters.<name>.targets[]` (`appParameterTarget` in
/// `workload-management-api-1.0.0.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParameterTarget {
    pub pointer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
}

/// `configuration` (`ApplicationDescription-002.yaml`) — the
/// operator-facing configuration sections and validation schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Configuration {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ConfigurationSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schema: Vec<ConfigurationSchemaRule>,
}

/// `configuration.sections[]` (`ApplicationDescription-002.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationSection {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<ConfigurationSetting>,
}

/// `configuration.sections[].settings[]`
/// (`ApplicationDescription-002.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationSetting {
    pub parameter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub immutable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

/// `configuration.schema[]` (`ApplicationDescription-002.yaml`) —
/// named validation rules referenced by settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationSchemaRule {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_precision: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_empty: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex_match: Option<String>,
}
