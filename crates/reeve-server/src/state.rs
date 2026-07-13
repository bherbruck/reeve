//! Shared application state: config + the single SQLite DB (Law 4).

use std::sync::{Arc, Mutex};

use device_api::{Identity, Role};
use rusqlite::Connection;

use crate::config::{AuthMode, Config};
use crate::ownership::Ownership;

/// Cloneable handle threaded through every route.
///
/// Locking: `db` is THE single writer connection (D6/D16 — server
/// tables AND revision-store tables; the durability changeset session
/// rides it, spec/reeve/07-durability.md §9.3). `revisions` wraps the
/// SAME connection via `RevisionStore::from_shared` and locks `db`
/// internally per call. Lock order: `revisions` may be held while a
/// store method briefly takes `db`; code holding `db` MUST NOT call
/// into `revisions` (one-direction rule — no cycles). Locks are short
/// and never held across `.await`.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Arc<Mutex<Connection>>,
    pub revisions: Arc<Mutex<revision_store::RevisionStore>>,
    /// The C6 durability engine (spec/reeve/07-durability.md §9.1 —
    /// ONE trait seam; tier selected by config).
    pub durability: Arc<dyn crate::durability::Durability>,
    /// True when this boot applied schema migrations — D16: a schema
    /// migration must cut a new snapshot generation (durability::startup
    /// consumes this).
    pub migrated_at_boot: bool,
    /// sha256 hex of the one-time first-boot setup token (password mode,
    /// zero users). In memory only: a crash mints a fresh one on restart
    /// (crash-only — nothing to persist, startup regenerates).
    pub setup_token_hash: Arc<Mutex<Option<String>>>,
    /// Which tree paths this tier may author (federation §8.4 single
    /// writer per layer). v1 single-tier: [`Ownership::Root`]; C10
    /// populates [`Ownership::Gateway`] from tier configuration.
    pub ownership: Arc<Ownership>,
    /// C8 event hub (spec/reeve/04-status-stream.md §6): producers in
    /// core and extensions emit typed rev-003/1 events here; the SSE
    /// endpoint (ext/sse.rs, ext-sse) subscribes. Droppable,
    /// at-most-once, RAM only (events.rs).
    pub events: crate::events::EventHub,
    /// C8 per-device channel registry (spec/reeve/02-channel.md §4):
    /// populated by the websocket endpoint (ext/channel.rs,
    /// ext-channel); consulted by presence (§4.3 presence-as-fact)
    /// and the render pipeline's nudge hook (§4.4). Always empty in a
    /// core build — presence degrades to recency, nudges are no-ops.
    pub channels: crate::channels::Channels,
    /// C11 zot image proxy (docs/decisions/delivery.md D8): `Some`
    /// iff `REEVE_ZOT_URL` is configured. `None` => non-native /v2
    /// repos 404 (proxy absent, zot_proxy.rs).
    pub zot: Option<crate::zot_proxy::ZotProxy>,
    /// REV-011 deploy-log store (ext-logs): THE seam — `SqliteLogStore`
    /// by default, a future `LokiLogStore` without touching any caller
    /// (ext/logs.rs). Absent in a core build (the routes are gated too).
    #[cfg(feature = "ext-logs")]
    pub logs: std::sync::Arc<dyn crate::ext::logs::LogStore>,
}

impl AppState {
    /// Mode-aware authorization (docs/decisions/auth.md D1): the role this
    /// identity acts with. `Anonymous` is admin ONLY under REEVE_AUTH=none;
    /// devices carry no human role.
    pub fn effective_role(&self, identity: &Identity) -> Option<Role> {
        match identity {
            Identity::Human { role, .. } => Some(*role),
            Identity::Anonymous => match self.cfg.auth {
                AuthMode::None => Some(Role::Admin),
                _ => None,
            },
            Identity::Device { .. } => None,
        }
    }
}
