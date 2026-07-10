//! reeve State Manifest — the device's desired-state pointer, polled
//! via conditional GET.
//!
//! Spec sources:
//! - `spec/reeve/08-packaging.md` §10.2 (endpoint
//!   `GET /api/reeve/v1/manifest`, ETag = manifest digest with
//!   grammar `sha256:<hex>`, anti-rollback rules).
//! - `docs/decisions/delivery.md` D13 (bundle digest + per-app
//!   `secrets_version`; models Margo's Desired State API).
//! - `spec/margo/system-design/specification/margo-management-interface/workload-management-api-1.0.0.yaml`
//!   (`UnsignedAppStateManifest`, `ManifestVersion`,
//!   `DeploymentBundleRef` — the Margo shapes this manifest is
//!   deliberately shaped after; spec/reeve/01-framework.md §3.8
//!   item 3 reassessment).

use serde::{Deserialize, Serialize};

/// Number of low bits holding the counter in a packed
/// [`ManifestVersion`] (spec/reeve/08-packaging.md §10.2: epoch in
/// the high 16 bits, counter in the low 48).
pub const COUNTER_BITS: u32 = 48;
/// Maximum counter value representable in 48 bits.
pub const COUNTER_MAX: u64 = (1 << COUNTER_BITS) - 1;

/// Error packing a `(epoch, counter)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManifestVersionError {
    #[error("manifest counter {0} exceeds 48-bit maximum {COUNTER_MAX}")]
    CounterOverflow(u64),
}

/// `manifestVersion` — logically the pair `(epoch, counter)` compared
/// lexicographically, encoded on the wire as ONE monotonically
/// increasing unsigned 64-bit integer: epoch in the high 16 bits,
/// counter in the low 48 (spec/reeve/08-packaging.md §10.2). Plain
/// integer comparison IS the pair comparison, and the wire value
/// stays exactly Margo's modeled shape (`ManifestVersion`: monotonic
/// u64 in `[1, 2^64-1]`, `workload-management-api-1.0.0.yaml` —
/// "The first manifest MUST use 1").
///
/// Agent rules (§10.2): a non-increasing value is rejected and logged
/// as a SECURITY event; an increase that bumps the epoch bits is
/// accepted and logged as a NOTABLE event (a restore happened,
/// spec/reeve/07-durability.md §9.5 restore fencing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManifestVersion(pub u64);

impl ManifestVersion {
    /// Pack `(epoch, counter)` into the wire u64. Fails if `counter`
    /// does not fit in 48 bits.
    pub fn pack(epoch: u16, counter: u64) -> Result<Self, ManifestVersionError> {
        if counter > COUNTER_MAX {
            return Err(ManifestVersionError::CounterOverflow(counter));
        }
        Ok(ManifestVersion(((epoch as u64) << COUNTER_BITS) | counter))
    }

    /// Unpack the wire u64 into `(epoch, counter)`.
    pub fn unpack(self) -> (u16, u64) {
        ((self.0 >> COUNTER_BITS) as u16, self.0 & COUNTER_MAX)
    }

    /// Epoch — high 16 bits; bumped by restore fencing
    /// (spec/reeve/07-durability.md §9.5).
    pub fn epoch(self) -> u16 {
        (self.0 >> COUNTER_BITS) as u16
    }

    /// Counter — low 48 bits; monotonic within an epoch.
    pub fn counter(self) -> u64 {
        self.0 & COUNTER_MAX
    }

    /// Strict monotonicity check the agent MUST enforce
    /// (spec/reeve/08-packaging.md §10.2): `other` is acceptable as a
    /// successor of `self` iff it is strictly greater.
    pub fn accepts_successor(self, other: ManifestVersion) -> bool {
        other > self
    }

    /// True if moving from `self` to `next` bumps the epoch bits —
    /// loggable NOTABLE event (a restore happened;
    /// spec/reeve/07-durability.md §9.5).
    pub fn is_epoch_bump(self, next: ManifestVersion) -> bool {
        next.epoch() > self.epoch()
    }
}

/// Validates the digest grammar `sha256:<hex>` used as the manifest
/// ETag (RFC 9110 strong validator) and for all content-addressed
/// pulls (spec/reeve/08-packaging.md §10.2; docs/decisions/delivery.md
/// D13).
pub fn is_sha256_digest(s: &str) -> bool {
    match s.strip_prefix("sha256:") {
        Some(hex) => hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

/// Media type of the reeve render bundle artifact referenced by the
/// State Manifest — the D2 layout packaged as an OCI artifact
/// (docs/decisions/delivery.md D13; docs/decisions/tree-render.md D2).
/// Named after Margo's bundle media-type convention
/// (`application/vnd.margo.bundle.v1+tar+gzip`,
/// `workload-management-api-1.0.0.yaml` `DeploymentBundleRef`).
pub const RENDER_BUNDLE_MEDIA_TYPE: &str = "application/vnd.reeve.render-bundle.v1+tar+gzip";

/// The reeve State Manifest body returned by
/// `GET /api/reeve/v1/manifest` (spec/reeve/08-packaging.md §10.2) —
/// State-Manifest-shaped after Margo's `UnsignedAppStateManifest`
/// (`workload-management-api-1.0.0.yaml`): `manifestVersion`,
/// render-bundle digest + pull URL, per-app `secrets_version`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateManifest {
    pub manifest_version: ManifestVersion,
    /// Render-bundle reference. Follows Margo's `DeploymentBundleRef`
    /// null rule: with zero apps the property MUST be present with
    /// the value `null`, never omitted — so it is always serialized.
    pub bundle: Option<BundleRef>,
    /// Per-app entries carrying `secrets_version`
    /// (docs/decisions/delivery.md D13; spec/reeve/10-secrets.md §12).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<AppManifestEntry>,
}

/// Render-bundle reference — shaped after Margo's
/// `DeploymentBundleRef` (`workload-management-api-1.0.0.yaml`):
/// mediaType, digest, advisory sizeBytes, content-addressable url.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// `sha256:<hex>`; MUST be verified after pull — `sizeBytes` is
    /// advisory only, never integrity.
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Content-addressed pull URL (native OCI /v2 routes,
    /// docs/decisions/delivery.md D7).
    pub url: String,
}

/// Per-app State Manifest entry. The agent diffs `secrets_version`
/// separately from the bundle digest: bundle unchanged +
/// `secrets_version` changed ⇒ re-resolve secrets and minimally
/// re-up, no bundle re-pull (spec/reeve/10-secrets.md §12;
/// docs/decisions/agent.md).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppManifestEntry {
    /// Application id (matches the rendered app dir /
    /// `ApplicationDeployment.spec.applicationId`).
    pub app_id: String,
    /// Deterministic deployment id of the rendered
    /// `ApplicationDeployment` (docs/decisions/tree-render.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    /// Hash of resolved secret names+versions, never values
    /// (spec/reeve/10-secrets.md §12; docs/decisions/delivery.md D13).
    /// Spelled `secrets_version` on the wire — the exact token used
    /// normatively throughout spec/reeve/.
    #[serde(rename = "secrets_version", default, skip_serializing_if = "Option::is_none")]
    pub secrets_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let v = ManifestVersion::pack(3, 12345).unwrap();
        assert_eq!(v.unpack(), (3, 12345));
        assert_eq!(v.epoch(), 3);
        assert_eq!(v.counter(), 12345);
    }

    #[test]
    fn first_manifest_is_one() {
        // Margo: "The first manifest MUST use 1" — epoch 0, counter 1.
        assert_eq!(ManifestVersion::pack(0, 1).unwrap(), ManifestVersion(1));
    }

    #[test]
    fn epoch_dominates_counter() {
        // Lexicographic pair comparison == plain integer comparison.
        let old = ManifestVersion::pack(0, COUNTER_MAX).unwrap();
        let new = ManifestVersion::pack(1, 0).unwrap();
        assert!(old.accepts_successor(new));
        assert!(old.is_epoch_bump(new));
        assert!(!new.accepts_successor(old));
        assert!(!new.accepts_successor(new)); // strict: equal rejected
    }

    #[test]
    fn counter_overflow_rejected() {
        assert_eq!(
            ManifestVersion::pack(0, COUNTER_MAX + 1),
            Err(ManifestVersionError::CounterOverflow(COUNTER_MAX + 1))
        );
    }

    #[test]
    fn manifest_version_serializes_as_plain_u64() {
        let v = ManifestVersion::pack(1, 7).unwrap();
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, ((1u64 << 48) | 7).to_string());
        assert_eq!(serde_json::from_str::<ManifestVersion>(&json).unwrap(), v);
    }

    #[test]
    fn digest_grammar() {
        assert!(is_sha256_digest(&format!("sha256:{}", "a".repeat(64))));
        assert!(!is_sha256_digest(&format!("sha256:{}", "a".repeat(63))));
        assert!(!is_sha256_digest(&format!("sha512:{}", "a".repeat(64))));
        assert!(!is_sha256_digest(&format!("sha256:{}", "g".repeat(64))));
    }
}
