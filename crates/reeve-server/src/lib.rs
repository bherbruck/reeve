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

pub mod auth;
pub mod config;
pub mod db;
pub mod delivery;
pub mod device_tokens;
pub mod enroll;
pub mod ingest;
pub mod join_tokens;
pub mod ownership;
pub mod presence;
pub mod render;
pub mod router;
pub mod state;
pub mod tree;

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
        // generation. The durability tier (C6) consumes this signal.
        info!("schema migrated; durability tier must cut a new snapshot generation (D16)");
    }

    // Same file, second connection: revision-store self-initializes its
    // own tables idempotently (Law 2 — the crate stands alone). WAL +
    // busy_timeout arbitrate the two writers; C6 unifies them if the
    // session-capture writer requires it.
    let revisions = revision_store::RevisionStore::open(&db_path)
        .map_err(|e| anyhow::anyhow!("opening revision store: {e}"))?;

    let state = AppState {
        cfg: Arc::new(cfg),
        db: Arc::new(Mutex::new(conn)),
        revisions: Arc::new(Mutex::new(revisions)),
        setup_token_hash: Arc::new(Mutex::new(None)),
        // v1 single-tier: root owns every authorable path; the upstream
        // stream is refused structurally regardless (federation §8.2).
        // C10 swaps in Ownership::Gateway when `upstream` is configured.
        ownership: Arc::new(ownership::Ownership::Root),
    };

    // Render-on-startup reconcile (Law 3: startup IS recovery): a
    // revision committed but un-rendered at kill time gets rendered now;
    // unreferenced bundle blobs are purged (render.rs).
    render::reconcile(&state)
        .map_err(|e| anyhow::anyhow!("startup render reconcile: {e}"))?;

    Ok(state)
}

/// Run the server until killed. No shutdown ceremony (Law 3): SIGTERM and
/// ctrl-c log and exit; startup is the recovery path.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let listen = cfg.listen;
    let state = bootstrap(cfg)?;

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
