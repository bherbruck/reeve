//! Device presence (C5) — online|offline + since.
//!
//! The presence SEAM: spec/reeve/02-channel.md §4.3 defines presence
//! from persistent-channel state ("an open channel means: this device
//! was reachable at last ping/pong"). Until the channel lands (C8),
//! presence degrades to polling recency exactly as the framework
//! prescribes for a device without the extension
//! (spec/reeve/01-framework.md §3.2: "presence from polling recency
//! only") — `devices.last_seen_at` (touched by every manifest poll and
//! status/journal ingest) against a freshness threshold.
//!
//! C8 slots channel state in ABOVE the recency fallback inside
//! [`device_presence`]; callers never change. §4.3's asymmetry is
//! preserved by construction: offline means "link down", never "device
//! dead" — device- vs link-degraded classification is 05-health-journal
//! §7.4 and consumes this signal, not the reverse.

use rusqlite::OptionalExtension as _;

use crate::db;
use crate::state::AppState;

/// Freshness threshold for recency-based presence: a device whose last
/// contact is older than this is offline. Chosen as 3x the agent's
/// default 30 s poll interval so one dropped poll never flaps presence.
pub const DEFAULT_ONLINE_THRESHOLD_SECS: i64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceState {
    Online,
    Offline,
}

/// Presence answer (02-channel §4.3 vocabulary): `online` + since /
/// `offline` + last-seen. Under recency-based presence both carry
/// `last_seen_at` — the honest fact we hold; "online since" begins to
/// mean channel-open time when C8 lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Presence {
    pub state: PresenceState,
    /// Unix seconds: last contact (`None` = never seen).
    pub since: Option<i64>,
}

/// Pure recency classification (unit-testable, no I/O).
pub fn from_recency(last_seen_at: Option<i64>, now: i64, threshold_secs: i64) -> Presence {
    match last_seen_at {
        Some(seen) if now - seen <= threshold_secs => Presence {
            state: PresenceState::Online,
            since: Some(seen),
        },
        seen => Presence {
            state: PresenceState::Offline,
            since: seen,
        },
    }
}

/// Presence of one device; `None` = unknown device. Today: recency of
/// `devices.last_seen_at`; C8 adds "an open channel wins" above the
/// fallback.
pub fn device_presence(state: &AppState, device_id: &str) -> anyhow::Result<Option<Presence>> {
    let conn = state.db.lock().expect("db mutex poisoned");
    let row: Option<Option<i64>> = conn
        .query_row(
            "SELECT last_seen_at FROM devices WHERE device_id = ?1",
            rusqlite::params![device_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row.map(|last_seen| from_recency(last_seen, db::now_secs(), DEFAULT_ONLINE_THRESHOLD_SECS)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_contact_is_online_since_last_seen() {
        let p = from_recency(Some(1000), 1010, 90);
        assert_eq!(p.state, PresenceState::Online);
        assert_eq!(p.since, Some(1000));
    }

    #[test]
    fn boundary_is_still_online() {
        assert_eq!(from_recency(Some(910), 1000, 90).state, PresenceState::Online);
        assert_eq!(from_recency(Some(909), 1000, 90).state, PresenceState::Offline);
    }

    #[test]
    fn stale_contact_is_offline_with_last_seen() {
        let p = from_recency(Some(100), 1000, 90);
        assert_eq!(p.state, PresenceState::Offline);
        assert_eq!(p.since, Some(100), "offline still reports last-seen (§4.3)");
    }

    #[test]
    fn never_seen_is_offline_with_no_since() {
        let p = from_recency(None, 1000, 90);
        assert_eq!(p.state, PresenceState::Offline);
        assert_eq!(p.since, None);
    }
}
