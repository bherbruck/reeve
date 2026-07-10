//! `DeviceCapabilitiesManifest` — the device→WFM capability report.
//!
//! Spec sources (pinned):
//! - `spec/margo/system-design/specification/margo-management-interface/device-capabilities.md`
//!   (attribute tables + example payloads — the JSON examples in
//!   that file are round-trip test fixtures).
//! - `spec/margo/system-design/specification/margo-management-interface/workload-management-api-1.0.0.yaml`
//!   (`DeviceCapabilitiesManifest`, `DevicePeripheral`,
//!   `DeviceCommunicationInterface`, `DeviceId` schemas).
//!
//! Real-world fixtures also parsed by the round-trip tests:
//! `reference/docker-compose/config/capabilities.json`,
//! `reference/poc/device/agent/config/capabilities.json`.

use serde::{Deserialize, Serialize};

use crate::reeve::capabilities::ReeveCapabilities;

/// `apiVersion` used by the pinned spec example
/// (`device-capabilities.md`).
pub const DEVICE_CAPABILITIES_API_VERSION: &str = "device.margo.org/v1alpha1";
/// `kind` — MUST be `DeviceCapabilitiesManifest`
/// (`device-capabilities.md`).
pub const DEVICE_CAPABILITIES_KIND: &str = "DeviceCapabilitiesManifest";

/// Device roles (`device-capabilities.md` "Properties Attributes").
/// Kept as constants, not an enum: the pinned fixtures disagree on
/// case (`"Standalone Cluster"` in the OpenAPI enum and gateway
/// examples vs `"standalone cluster"` in the main example payload),
/// so the wire value is preserved verbatim as a `String`.
pub mod role {
    pub const STANDALONE_CLUSTER: &str = "Standalone Cluster";
    pub const CLUSTER_LEADER: &str = "Cluster Leader";
    pub const STANDALONE_DEVICE: &str = "Standalone Device";
    pub const GATEWAY: &str = "Gateway";
}

/// `DeviceCapabilitiesManifest` request body
/// (`device-capabilities.md`; posted to
/// `POST /api/v1/clients/{clientId}/capabilities/{deviceId}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCapabilitiesManifest {
    pub api_version: String,
    pub kind: String,
    pub properties: DeviceProperties,
}

/// `properties` (`device-capabilities.md` "Properties Attributes").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceProperties {
    /// Device id; hierarchical path form
    /// `{gatewayId}/[{intermediateDeviceId}/...]{deviceId}` for
    /// devices behind a see-thru gateway (`DeviceId` grammar in
    /// `workload-management-api-1.0.0.yaml`).
    pub id: String,
    pub vendor: String,
    pub model_number: String,
    pub serial_number: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Required for hosting roles; absent for a pure see-thru
    /// gateway (`device-capabilities.md` gateway examples).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<DeviceResources>,
    /// reeve capability advertisement — one additive optional object
    /// (spec/reeve/01-framework.md §3.3). A manifest with no `reeve`
    /// key means: no extensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reeve: Option<ReeveCapabilities>,
}

/// `properties.resources` (`device-capabilities.md` "Resources
/// Attributes").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceResources {
    pub cpu: CpuSpec,
    pub memory: String,
    pub storage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peripherals: Option<Vec<Peripheral>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interfaces: Option<Vec<CommunicationInterface>>,
}

/// `resources.cpu` — WIRE-EXACT shape preserved: the OpenAPI schema
/// (`workload-management-api-1.0.0.yaml`) models `cpu` as an object,
/// but every example payload in `device-capabilities.md` serializes
/// it as an ARRAY of CPU objects (multi-socket / aggregated
/// gateways), and `reference/docker-compose/config/capabilities.json`
/// uses the object form. Both parse; the original shape is kept so
/// re-serialization is faithful.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CpuSpec {
    Many(Vec<Cpu>),
    One(Cpu),
}

impl CpuSpec {
    /// Uniform view over both wire shapes.
    pub fn iter(&self) -> impl Iterator<Item = &Cpu> {
        match self {
            CpuSpec::Many(v) => v.as_slice().iter(),
            CpuSpec::One(c) => std::slice::from_ref(c).iter(),
        }
    }

    /// Total cores across all CPU entries.
    pub fn total_cores(&self) -> f64 {
        self.iter().map(|c| c.cores).sum()
    }
}

/// A CPU entry (`device-capabilities.md` "CPU Attributes").
/// `architecture` stays a `String`: the enum says
/// `amd64`/`arm64`/`arm` but the pinned example payload reports
/// `x86_64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cpu {
    /// Decimal units of CPU cores (e.g. `0.5` is half a core).
    pub cores: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
}

/// Known `architecture` values (`device-capabilities.md`
/// "CpuArchitectureType"; `x86_64` appears in the example payloads).
pub mod cpu_architecture {
    pub const AMD64: &str = "amd64";
    pub const ARM64: &str = "arm64";
    pub const ARM: &str = "arm";
    pub const X86_64: &str = "x86_64";
}

/// A peripheral (`device-capabilities.md` "Peripheral Attributes";
/// `DevicePeripheral` in `workload-management-api-1.0.0.yaml`).
/// `type` stays a `String`: the enum is lowercase (`gpu`) but the
/// pinned example payload reports `"GPU"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Peripheral {
    #[serde(rename = "type")]
    pub peripheral_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Known peripheral types (`device-capabilities.md`
/// "PeripheralType").
pub mod peripheral_type {
    pub const GPU: &str = "gpu";
    pub const DISPLAY: &str = "display";
    pub const CAMERA: &str = "camera";
    pub const MICROPHONE: &str = "microphone";
    pub const SPEAKER: &str = "speaker";
}

/// A communication interface (`device-capabilities.md`
/// "CommunicationInterface Attributes"; `DeviceCommunicationInterface`
/// in `workload-management-api-1.0.0.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunicationInterface {
    #[serde(rename = "type")]
    pub interface_type: String,
}

/// Known interface types (`device-capabilities.md`
/// "CommunicationInterfaceType").
pub mod interface_type {
    pub const ETHERNET: &str = "ethernet";
    pub const WIFI: &str = "wifi";
    pub const CELLULAR: &str = "cellular";
    pub const BLUETOOTH: &str = "bluetooth";
    pub const USB: &str = "usb";
    pub const CANBUS: &str = "canbus";
    pub const RS232: &str = "rs232";
}
