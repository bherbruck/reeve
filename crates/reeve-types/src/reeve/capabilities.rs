//! Capability advertisement — how agent and server discover each
//! other's reeve extensions (spec/reeve/01-framework.md §3.3).
//!
//! There is no negotiation: a feature is usable between a given agent
//! and server iff both advertise a common protocol version for it;
//! anything else is feature-unavailable (§3.2), never an error.

use serde::{Deserialize, Serialize};

/// The agent-side advertisement — one additive object on the Margo
/// `DeviceCapabilitiesManifest`, inside `properties.reeve`
/// (spec/reeve/01-framework.md §3.3):
///
/// ```json
/// { "agentVersion": "0.4.2",
///   "extensions": ["rev-001/1", "rev-002/1", "rev-004/1"] }
/// ```
///
/// A vanilla WFM sees one unknown optional object and ignores it. A
/// manifest with no `reeve` key means: no extensions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReeveCapabilities {
    pub agent_version: String,
    /// Entries are `"rev-NNN/V"`: REV number, protocol version
    /// (§3.4). An implementation MAY advertise several versions of
    /// one extension; peers use the highest common one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
}

impl ReeveCapabilities {
    /// Highest advertised protocol version for extension `rev`,
    /// `None` if the extension is not advertised at all.
    pub fn highest_version(&self, rev: u16) -> Option<u32> {
        self.extensions
            .iter()
            .filter_map(|e| parse_extension(e))
            .filter(|(r, _)| *r == rev)
            .map(|(_, v)| v)
            .max()
    }

    /// True iff `rev` is advertised at exactly protocol version
    /// `version` (§3.4: versions are distinct capabilities).
    pub fn supports(&self, rev: u16, version: u32) -> bool {
        self.extensions
            .iter()
            .filter_map(|e| parse_extension(e))
            .any(|(r, v)| r == rev && v == version)
    }
}

/// The server-side advertisement at
/// `GET /api/reeve/v1/capabilities` — "its extension list plus
/// server version" (spec/reeve/01-framework.md §3.3). 404 or any
/// error means "vanilla Margo server"; the agent proceeds with pure
/// Margo behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub server_version: String,
    /// Same `"rev-NNN/V"` grammar as the agent side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
}

impl ServerCapabilities {
    /// Highest advertised protocol version for extension `rev`.
    pub fn highest_version(&self, rev: u16) -> Option<u32> {
        self.extensions
            .iter()
            .filter_map(|e| parse_extension(e))
            .filter(|(r, _)| *r == rev)
            .map(|(_, v)| v)
            .max()
    }
}

/// Parse one advertisement entry `"rev-NNN/V"` into
/// `(rev_number, protocol_version)` (spec/reeve/01-framework.md
/// §3.3/§3.4). Malformed entries yield `None` — a receiver ignores
/// what it cannot parse rather than rejecting the payload (§3.2).
pub fn parse_extension(entry: &str) -> Option<(u16, u32)> {
    let rest = entry.strip_prefix("rev-")?;
    let (num, ver) = rest.split_once('/')?;
    Some((num.parse().ok()?, ver.parse().ok()?))
}

/// Format an advertisement entry `"rev-NNN/V"` (REV numbers are
/// conventionally zero-padded to three digits: `rev-001/1`).
pub fn format_extension(rev: u16, version: u32) -> String {
    format!("rev-{rev:03}/{version}")
}

/// REV numbers of the extension index
/// (spec/reeve/01-framework.md §3.5). Extensions with no wire
/// protocol (REV-006/007/008) need no advertisement and are absent.
pub mod rev {
    /// REV-001 Persistent Agent Channel (spec/reeve/02-channel.md).
    pub const CHANNEL: u16 = 1;
    /// REV-002 Remote Terminal (spec/reeve/03-terminal.md).
    pub const TERMINAL: u16 = 2;
    /// REV-003 Live Status Stream (spec/reeve/04-status-stream.md).
    pub const STATUS_STREAM: u16 = 3;
    /// REV-004 Device Health & Status Journal
    /// (spec/reeve/05-health-journal.md).
    pub const HEALTH_JOURNAL: u16 = 4;
    /// REV-005 Federation & Gateway (spec/reeve/06-federation.md).
    pub const FEDERATION: u16 = 5;
    /// REV-009 Secrets (spec/reeve/10-secrets.md).
    pub const SECRETS: u16 = 9;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format() {
        assert_eq!(parse_extension("rev-001/1"), Some((1, 1)));
        assert_eq!(parse_extension("rev-009/2"), Some((9, 2)));
        assert_eq!(parse_extension("rev-1/1"), Some((1, 1))); // tolerant of padding
        assert_eq!(parse_extension("rev-abc/1"), None);
        assert_eq!(parse_extension("REV-001/1"), None);
        assert_eq!(parse_extension("rev-001"), None);
        assert_eq!(format_extension(1, 1), "rev-001/1");
        assert_eq!(format_extension(rev::SECRETS, 1), "rev-009/1");
    }

    #[test]
    fn highest_common_version() {
        let caps = ReeveCapabilities {
            agent_version: "0.4.2".into(),
            extensions: vec!["rev-001/1".into(), "rev-001/2".into(), "junk".into()],
        };
        assert_eq!(caps.highest_version(1), Some(2));
        assert!(caps.supports(1, 1));
        assert!(caps.supports(1, 2));
        assert!(!caps.supports(1, 3));
        assert_eq!(caps.highest_version(2), None);
    }
}
