//! Secrets resolve wire types — `POST /api/reeve/v1/secrets/resolve`
//! (spec/reeve/10-secrets.md §12.3, REV-009; docs/decisions/secrets.md
//! D15).
//!
//! The device asks with its enrollment-issued bearer token and can
//! only ask as itself (§12.3 / §12.6: the response is scoped to the
//! requesting device's own resolution). Values in the response are
//! plaintext — this body exists only in server RAM, TLS in flight,
//! and the agent's RAM before env-file materialization (§12.3). It
//! MUST never be journaled, logged, or persisted as-is;
//! [`ResolvedSecret`] redacts its value from `Debug` output as a
//! guard rail.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Resolve endpoint path on a reeve server
/// (spec/reeve/10-secrets.md §12.3).
pub const SECRETS_RESOLVE_PATH: &str = "/api/reeve/v1/secrets/resolve";

/// Request body: the secret names referenced (`${secret:<name>}`)
/// by the requesting device's rendered apps. Names only — never
/// values; names are metadata (spec/reeve/10-secrets.md §12.6
/// "who resolved what version when — metadata, not values").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsResolveRequest {
    pub secrets: Vec<String>,
}

/// One resolved secret: plaintext value + its version (the rotation
/// counter that feeds the manifest-level `secrets_version` hash,
/// spec/reeve/10-secrets.md §12.4).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSecret {
    /// Plaintext. RAM/TLS/env-file only (§12.3); never log.
    pub value: String,
    /// Version at resolution time — audit metadata, safe to log.
    pub version: u64,
}

impl std::fmt::Debug for ResolvedSecret {
    /// Redacts `value`: a stray `{:?}` in agent or server logs must
    /// not become the fourth place plaintext exists (§12.3).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSecret")
            .field("value", &"<redacted>")
            .field("version", &self.version)
            .finish()
    }
}

/// Response body: resolutions keyed by requested name. A name the
/// device may not read — or that does not exist — is simply ABSENT
/// (indistinguishable by design: the resolve endpoint must not be a
/// secret-existence oracle beyond the device's own scope, §12.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsResolveResponse {
    pub secrets: BTreeMap<String, ResolvedSecret>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_shape() {
        let resp = SecretsResolveResponse {
            secrets: BTreeMap::from([(
                "db-password".to_string(),
                ResolvedSecret {
                    value: "hunter2".to_string(),
                    version: 3,
                },
            )]),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["secrets"]["db-password"]["value"], "hunter2");
        assert_eq!(json["secrets"]["db-password"]["version"], 3);
        let back: SecretsResolveResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, resp);

        let req = SecretsResolveRequest {
            secrets: vec!["db-password".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["secrets"][0], "db-password");
    }

    #[test]
    fn debug_never_prints_the_value() {
        let s = ResolvedSecret {
            value: "hunter2".to_string(),
            version: 1,
        };
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("hunter2"), "plaintext leaked into Debug: {dbg}");
        assert!(dbg.contains("<redacted>"));
        assert!(dbg.contains('1'));
    }
}
