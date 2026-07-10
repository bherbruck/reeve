//! Round-trip tests against the ACTUAL pinned fixtures in
//! `spec/margo/` and `reference/` (CLAUDE.md "Verification": wire
//! types are tested against real spec/reference files, never
//! hand-written approximations).
//!
//! Property asserted per fixture: parse → re-serialize → re-parse
//! yields a value equal to the first parse (semantic equality of the
//! typed view; unknown-field tolerance means fields this crate does
//! not model are dropped on re-serialize, which is the documented
//! serde posture — spec/reeve/01-framework.md §3.6 wire-exactness).

use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use reeve_types::margo::application::ApplicationDescription;
use reeve_types::margo::capabilities::{CpuSpec, DeviceCapabilitiesManifest};
use reeve_types::margo::deployment::ApplicationDeployment;
use reeve_types::margo::status::{DeploymentState, DeploymentStatusManifest};

fn repo_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

fn read_fixture(rel: &str) -> String {
    let path = repo_path(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read fixture {} ({e}). If spec/margo/ or reference/ is empty, \
             run `git submodule update --init --recursive` first (CLAUDE.md).",
            path.display()
        )
    })
}

fn yaml_roundtrip<T>(rel: &str) -> T
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let raw = read_fixture(rel);
    let parsed: T = serde_yaml_ng::from_str(&raw)
        .unwrap_or_else(|e| panic!("failed to parse {rel}: {e}"));
    let reserialized = serde_yaml_ng::to_string(&parsed)
        .unwrap_or_else(|e| panic!("failed to re-serialize {rel}: {e}"));
    let reparsed: T = serde_yaml_ng::from_str(&reserialized)
        .unwrap_or_else(|e| panic!("failed to re-parse {rel}: {e}\n---\n{reserialized}"));
    assert_eq!(parsed, reparsed, "round-trip changed {rel}");
    parsed
}

fn json_roundtrip_str<T>(raw: &str, what: &str) -> T
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let parsed: T = serde_json::from_str(raw)
        .unwrap_or_else(|e| panic!("failed to parse {what}: {e}"));
    let reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    let reparsed: T = serde_json::from_str(&reserialized)
        .unwrap_or_else(|e| panic!("failed to re-parse {what}: {e}\n---\n{reserialized}"));
    assert_eq!(parsed, reparsed, "round-trip changed {what}");
    parsed
}

fn json_roundtrip<T>(rel: &str) -> T
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    json_roundtrip_str(&read_fixture(rel), rel)
}

/// Extract every fenced ```json block from a pinned markdown spec
/// file — the spec's embedded example payloads ARE fixtures.
fn json_blocks(rel: &str) -> Vec<String> {
    let md = read_fixture(rel);
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in md.lines() {
        match current.as_mut() {
            Some(block) => {
                if line.trim_start().starts_with("```") {
                    blocks.push(current.take().unwrap());
                } else {
                    block.push_str(line);
                    block.push('\n');
                }
            }
            None => {
                if line.trim_start().starts_with("```json") {
                    current = Some(String::new());
                }
            }
        }
    }
    assert!(!blocks.is_empty(), "no ```json blocks found in {rel}");
    blocks
}

// --- ApplicationDescription -------------------------------------------------

#[test]
fn application_description_spec_examples() {
    let d: ApplicationDescription = yaml_roundtrip(
        "spec/margo/src/specification/applications/resources/examples/valid/ApplicationDescription-001.yaml",
    );
    assert_eq!(d.effective_id(), Some("com-northstartida-hello-world"));
    assert_eq!(d.kind, "ApplicationDescription");
    assert_eq!(d.api_version, "margo.org/v1-alpha1");
    assert_eq!(d.deployment_profiles.len(), 1);
    assert_eq!(d.deployment_profiles[0].profile_type, "helm");
    assert_eq!(d.parameters.len(), 2);
    assert!(d.configuration.is_some());

    let d: ApplicationDescription = yaml_roundtrip(
        "spec/margo/src/specification/applications/resources/examples/valid/ApplicationDescription-002.yaml",
    );
    assert_eq!(d.effective_id(), Some("com-northstartida-digitron-orchestrator"));
    assert_eq!(d.deployment_profiles.len(), 2);
    let helm = &d.deployment_profiles[0];
    let rr = helm.required_resources.as_ref().expect("requiredResources");
    assert_eq!(rr.cpu.as_ref().unwrap().cores, 1.5);
    assert_eq!(rr.peripherals.as_ref().unwrap().len(), 2);
    let cfg = d.configuration.as_ref().unwrap();
    assert_eq!(cfg.sections.len(), 4);
    assert_eq!(cfg.schema.len(), 7);
}

#[test]
fn application_description_reference_packages() {
    // Reference sandbox artifacts: id lives under metadata here.
    let d: ApplicationDescription = yaml_roundtrip(
        "reference/poc/tests/artefacts/nextcloud-compose/margo-package/margo.yaml",
    );
    assert_eq!(d.id, None);
    assert_eq!(d.effective_id(), Some("nextcloud-stack"));
    assert_eq!(d.deployment_profiles[0].profile_type, "compose");
    assert_eq!(d.parameters.len(), 6);

    // This sandbox file is a TEMPLATE: it ships unsubstituted
    // `{{HELM_REPOSITORY}}` placeholders, which YAML parses as
    // complex (mapping) keys — parseable, but not emittable by the
    // YAML serializer. Parse fidelity is asserted; full round-trip is
    // covered by the real artifacts above.
    let raw = read_fixture(
        "reference/poc/tests/artefacts/custom-otel-helm-app/margo-package/margo.yaml",
    );
    let d: ApplicationDescription = serde_yaml_ng::from_str(&raw).unwrap();
    assert_eq!(d.effective_id(), Some("com-go-otel-service"));
    // Non-enum profile type shipped by the reference sandbox.
    assert_eq!(d.deployment_profiles[0].profile_type, "helm.v3");
    assert!(d.deployment_profiles[0].components[0].properties.is_some());
}

// --- ApplicationDeployment --------------------------------------------------

#[test]
fn application_deployment_spec_examples() {
    let d: ApplicationDeployment = yaml_roundtrip(
        "spec/margo/src/specification/margo-management-interface/resources/examples/valid/DesiredState-001.yaml",
    );
    assert_eq!(d.kind, "ApplicationDeployment");
    assert_eq!(d.api_version, "application.margo.org/v1alpha1");
    assert_eq!(d.id.as_deref(), Some("a3e2f5dc-912e-494f-8395-52cf3769bc06"));
    assert_eq!(d.metadata.device_id.as_deref(), Some("edge-01"));
    assert_eq!(d.spec.application_id, "com-northstartida-digitron-orchestrator");
    assert_eq!(d.spec.deployment_profile.profile_type, "helm");
    assert_eq!(d.spec.deployment_profile.components.len(), 2);
    assert_eq!(d.spec.parameters.len(), 10);
    let target = &d.spec.parameters["pollFrequency"].targets[0];
    assert_eq!(target.pointer, "settings.pollFrequency");
    assert_eq!(target.components.len(), 2);

    let d: ApplicationDeployment = yaml_roundtrip(
        "spec/margo/src/specification/margo-management-interface/resources/examples/valid/DesiredState-002.yaml",
    );
    assert_eq!(d.spec.deployment_profile.profile_type, "compose");
    assert_eq!(d.spec.deployment_profile.components[0].name, "digitron-orchestrator-docker");
}

// --- DeploymentStatusManifest ----------------------------------------------

#[test]
fn deployment_status_manifest_spec_example() {
    // The example manifest embedded in the pinned spec page is the
    // fixture (deployment-status.md has no standalone JSON file).
    let blocks = json_blocks(
        "spec/margo/system-design/specification/margo-management-interface/deployment-status.md",
    );
    let m: DeploymentStatusManifest =
        json_roundtrip_str(&blocks[0], "deployment-status.md example manifest");
    assert_eq!(m.kind, "DeploymentStatusManifest");
    assert_eq!(m.api_version, "deployment.margo.org/v1alpha1");
    assert_eq!(m.deployment_id, "a3e2f5dc-912e-494f-8395-52cf3769bc06");
    assert_eq!(m.device_id.as_deref(), Some("plant-alfa-zone1-edge01"));
    assert_eq!(m.status.state, DeploymentState::Pending);
    assert_eq!(m.components.len(), 2);
    assert_eq!(m.components[0].name, "digitron-orchestrator");
    assert!(m.reeve.is_none()); // vanilla Margo manifest: no reeve key
}

// --- DeviceCapabilitiesManifest ---------------------------------------------

#[test]
fn device_capabilities_spec_examples() {
    // Every JSON example payload on the pinned spec page, including
    // the see-thru gateway variants (some without `resources`).
    let blocks = json_blocks(
        "spec/margo/system-design/specification/margo-management-interface/device-capabilities.md",
    );
    assert!(blocks.len() >= 5, "expected ≥5 example payloads, got {}", blocks.len());
    for (i, block) in blocks.iter().enumerate() {
        let m: DeviceCapabilitiesManifest =
            json_roundtrip_str(block, &format!("device-capabilities.md example #{i}"));
        assert_eq!(m.kind, "DeviceCapabilitiesManifest");
        assert!(!m.properties.id.is_empty());
        assert!(!m.properties.roles.is_empty());
    }
    // Main example: cpu serialized as an ARRAY.
    let main: DeviceCapabilitiesManifest = serde_json::from_str(&blocks[0]).unwrap();
    let res = main.properties.resources.as_ref().unwrap();
    assert!(matches!(res.cpu, CpuSpec::Many(_)));
    assert_eq!(res.cpu.total_cores(), 24.0);
    assert_eq!(res.cpu.iter().next().unwrap().architecture.as_deref(), Some("x86_64"));
}

#[test]
fn device_capabilities_reference_fixtures() {
    // Reference agent config: cpu is an OBJECT here (the OpenAPI
    // schema's shape) — both forms must parse and round-trip.
    let m: DeviceCapabilitiesManifest =
        json_roundtrip("reference/poc/device/agent/config/capabilities.json");
    assert_eq!(m.api_version, "device.margo.org/v1alpha1");
    let res = m.properties.resources.as_ref().unwrap();
    assert!(matches!(res.cpu, CpuSpec::One(_)));
    assert_eq!(res.cpu.total_cores(), 24.0);

    // Older sandbox config (kind "DeviceCapabilities", no
    // peripherals/interfaces): still parses — unknown-field/looser
    // tolerance, kind is data not an enum.
    let m: DeviceCapabilitiesManifest =
        json_roundtrip("reference/docker-compose/config/capabilities.json");
    assert_eq!(m.kind, "DeviceCapabilities");
    assert!(m.properties.resources.as_ref().unwrap().peripherals.is_none());
}
