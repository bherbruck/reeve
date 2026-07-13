//! reeve — the server. Compiles the layered deployment tree into
//! per-device desired state, serves the device API and (later) the
//! embedded UI. ALL server state is one SQLite file (Law 4): revision
//! store tables (revision-store crate) + server tables (embedded
//! migrations, docs/decisions/storage.md D6). Crash-only (Law 3):
//! startup IS recovery — migrate, purge, serve; kill -9 anywhere leaves
//! resumable state because every write is transactional.
//!
//! Library + thin `main.rs` so integration tests and later build items
//! (C2..C12) compose the same router and state.

pub mod assets;
pub mod auth;
pub mod channels;
pub mod completions;
pub mod config;
pub mod db;
pub mod delivery;
pub mod deploy;
pub mod devices;
pub mod events;
pub mod device_tokens;
pub mod groups;
pub mod durability;
pub mod enroll;
pub mod ext;
pub mod history;
pub mod ingest;
pub mod init;
pub mod join_tokens;
pub mod keyfile;
pub mod openapi;
pub mod ownership;
pub mod presence;
pub mod render;
pub mod router;
pub mod scope;
pub mod specdocs;
pub mod state;
pub mod tree;
pub mod zot_proxy;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use tracing::{info, warn};

use crate::config::Config;
use crate::state::AppState;

/// Build the full application state from config: open/migrate the single
/// SQLite DB, open the revision store on the same file. Idempotent — safe
/// on every startup (Law 3). Callers run [`auth::bootstrap`] next (as
/// [`run`] does) for mode-specific startup work.
pub fn bootstrap(cfg: Config) -> anyhow::Result<AppState> {
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating data dir {}", cfg.data_dir.display()))?;
    let db_path = cfg.data_dir.join("reeve.db");

    let mut conn = db::open(&db_path)?;
    let migrated = db::migrate(&mut conn)?;
    if migrated {
        // D16 schema law: a schema migration must cut a new snapshot
        // generation — durability::startup consumes migrated_at_boot.
        info!("schema migrated; durability tier will cut a new snapshot generation (D16)");
    }

    // Writer unification (D6/D16, spec/reeve/07-durability.md §9.3):
    // ONE writer connection carries server tables AND revision-store
    // tables, so the changeset session captures every write. The store
    // locks the shared handle per call (Law 2 — the crate still stands
    // alone via its owned-connection constructors).
    let db = Arc::new(Mutex::new(conn));
    let revisions = revision_store::RevisionStore::from_shared(db.clone())
        .map_err(|e| anyhow::anyhow!("opening revision store: {e}"))?;

    // The C6 durability engine (tier from config; NoneDurability when
    // disabled). Session capture (changeset tier) attaches to THE
    // writer at the first generation cut.
    let durability = durability::from_config(&cfg, db.clone())?;

    // C11 zot image proxy (docs/decisions/delivery.md D8): built iff
    // REEVE_ZOT_URL is configured; otherwise the /v2 catch-all 404s.
    let zot = cfg.zot.as_ref().map(zot_proxy::ZotProxy::from_config);

    // Tier ownership (C10, spec/reeve/06-federation.md §8.4): computed
    // before `cfg` moves into the Arc below.
    let ownership = match &cfg.federation {
        Some(fed) => ownership::Ownership::Gateway {
            owned_prefixes: vec![
                format!("layers/20-site.{}", fed.site),
                "layers/40-device.".to_string(),
            ],
        },
        None => ownership::Ownership::Root,
    };

    // REV-011 deploy-log store (ext-logs): bound BEFORE the struct
    // literal because `db` and `cfg` are moved into it below. The
    // SqliteLogStore over THE writer connection; a config-selected
    // LokiLogStore would be constructed here instead, no route changes.
    #[cfg(feature = "ext-logs")]
    let logs: Arc<dyn ext::logs::LogStore> = Arc::new(ext::logs::SqliteLogStore::new(
        db.clone(),
        cfg.logs_retain_per_deployment,
    ));

    let state = AppState {
        cfg: Arc::new(cfg),
        db,
        revisions: Arc::new(Mutex::new(revisions)),
        durability,
        migrated_at_boot: migrated,
        setup_token_hash: Arc::new(Mutex::new(None)),
        // Tier ownership (C10, spec/reeve/06-federation.md §8.4):
        // REEVE_UPSTREAM present => gateway, owning ONLY its own site
        // layer family and its locally-enrolled device layers (derived
        // per §8.4; the `20-site.` numbering is the D11 taxonomy
        // convention shared with render.rs layer_chain). Absent =>
        // root, unchanged. The upstream stream is refused structurally
        // at every tier regardless (§8.2).
        ownership: Arc::new(ownership),
        // C8: fresh per boot — event ids restart (clients get `reset`,
        // spec/reeve/04-status-stream.md §6.2) and no channel survives
        // its process (Law 3: nothing durable to recover here).
        events: events::EventHub::new(),
        channels: channels::Channels::new(),
        zot,
        #[cfg(feature = "ext-logs")]
        logs,
    };

    // Terminal audit finalization (spec/reeve/03-terminal.md §5.4,
    // Law 3): a server crash mid-session left rows with NULL ended_at
    // — the PTYs died with their sub-channels; startup completes the
    // accounting as close_reason = server-restart. Unconditional
    // (the V7 table exists regardless of ext-terminal, and a core
    // binary must still finalize a full binary's dangling rows).
    {
        let conn = state.db.lock().expect("db mutex poisoned");
        let dangling = conn.execute(
            "UPDATE terminal_sessions
             SET ended_at = ?1, close_reason = 'server-restart'
             WHERE ended_at IS NULL",
            rusqlite::params![db::now_secs()],
        )?;
        if dangling > 0 {
            warn!(dangling, "finalized terminal sessions dangling from a crash (§5.4)");
        }
    }

    // Render-on-startup reconcile (Law 3: startup IS recovery): a
    // revision committed but un-rendered at kill time gets rendered now;
    // unreferenced bundle blobs are purged (render.rs).
    render::reconcile(&state)
        .map_err(|e| anyhow::anyhow!("startup render reconcile: {e}"))?;

    Ok(state)
}

/// Options for [`run_with_options`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RunOptions {
    /// `--restore-from-target`: with NO local DB and a configured
    /// durability target, restore the latest generation before normal
    /// startup — THE DR procedure (spec/reeve/07-durability.md §9.5).
    pub restore_from_target: bool,
}

/// Run the server until killed. No shutdown ceremony (Law 3): SIGTERM and
/// ctrl-c log and exit; startup is the recovery path.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    run_with_options(cfg, RunOptions::default()).await
}

/// [`run`] with explicit startup options (restore-at-bootstrap, §9.5:
/// disaster recovery is normal startup with one precondition removed).
pub async fn run_with_options(cfg: Config, opts: RunOptions) -> anyhow::Result<()> {
    let listen = cfg.listen;

    // §9.5 restore-at-bootstrap: runs BEFORE bootstrap so the restored
    // file becomes the local DB that normal startup then migrates.
    durability::maybe_restore_at_bootstrap(&cfg, opts.restore_from_target).await?;

    let state = bootstrap(cfg)?;

    // D6/D16 startup sequencing: migrate (done in bootstrap) -> snapshot
    // -> resume streaming; scheduled loops for snapshot/ship/verify.
    durability::startup(&state.durability, state.migrated_at_boot).await;
    durability::spawn_tasks(state.durability.clone(), &state.cfg.durability);

    // C8 durability sampling (spec/reeve/04-status-stream.md §6.3):
    // poll durability.status() and emit `durability-lag` /
    // `verify-restore` events on transitions (ext/sse.rs).
    #[cfg(feature = "ext-sse")]
    ext::sse::spawn_durability_sampler(
        state.durability.clone(),
        state.events.clone(),
        std::time::Duration::from_secs(10),
    );

    // C9 rollout engine (spec/reeve/09-rollouts.md §11.2, Law 3): the
    // periodic tick's FIRST fire is the startup resume — rollout state
    // is all in SQLite, so a server killed mid-wave re-reads manifest/
    // advancement state and continues exactly where it stopped. The
    // second leg reacts to `failed` status ingests immediately (§11.4
    // auto-pause "at any time").
    #[cfg(feature = "ext-rollouts")]
    ext::rollouts::spawn_engine(state.clone());

    // C10 federation sync loop (spec/reeve/06-federation.md §8.2/§8.3):
    // gateway tiers pull revisions + scoped secrets down and forward
    // journaled status up, every REEVE_SYNC_INTERVAL_SECS. Offline-
    // tolerant by construction: every tick that cannot reach the parent
    // records the error and tries again — local agents are unaffected
    // (§8.6, Law 5 one tier up).
    #[cfg(feature = "ext-federation")]
    if state.cfg.federation.is_some() {
        ext::federation::spawn_sync(state.clone());
    }
    #[cfg(not(feature = "ext-federation"))]
    if state.cfg.federation.is_some() {
        // Ownership is still enforced (core), but nothing syncs: say so
        // loudly rather than impersonating a healthy gateway.
        warn!(
            "REEVE_UPSTREAM is configured but this binary was built without \
             ext-federation — ownership is enforced, upstream sync is NOT running"
        );
    }

    let report = auth::bootstrap(&state)?;
    for notice in &report.notices {
        warn!("{notice}");
    }
    if let Some(token) = &report.setup_token {
        // One-time setup token (D1): logged, never stored durably — a
        // restart mints a fresh one while zero users exist (crash-only).
        warn!(
            "FIRST BOOT: no users exist. Create the admin via \
             POST /api/auth/setup with setup token: {token}"
        );
    }

    // C12 §10.4 /install bootstrap: routes exist only in
    // embedded-agents builds (absent => 404, invisible — 01-framework
    // §3.1 rule 4). Wired here rather than router::build so tests can
    // compose install::router with injected artifacts.
    #[cfg(feature = "embedded-agents")]
    let app = router::build(state.clone()).merge(ext::install::router_from_embedded(&state));
    #[cfg(not(feature = "embedded-agents"))]
    let app = router::build(state);

    tokio::spawn(async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("installing SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        info!("signal received; exiting (crash-only: startup is recovery)");
        std::process::exit(0);
    });

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    info!(%listen, "reeve-server listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("server error")
}
