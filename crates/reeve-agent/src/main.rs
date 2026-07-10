//! reeve-agent binary — thin shell over the library: load config,
//! open agent.db (startup IS recovery, Law 3), report what we're
//! continuing from, then poll forever (Law 5: offline is a logged
//! no-op, never an exit).

use std::path::Path;
use std::time::Duration;

use reeve_agent::{
    AgentConfig, AgentDb, BundleSource, BundleStore, CommandComposeProvider, ManifestSource,
    PollOutcome, Provider, PullError, StatusSink, converge, poll_once, record_reports,
    resolve_desired,
};
use tracing::{error, info, warn};

/// Compiled-in extension hooks (docs/build-charter.md CODE BOUNDARY):
/// each field exists only when its `ext-*` feature is on; core code
/// never reaches into this struct — extensions are invoked from the
/// loop shell between core steps.
#[derive(Default)]
struct ExtHooks {
    /// ext-secrets (REV-009): resolve-endpoint client. `None` for
    /// `dir://` sources and unenrolled agents — apps with secret
    /// references are then deferred/held by `sync_env`.
    #[cfg(feature = "ext-secrets")]
    secrets: Option<reeve_agent::ext::secrets::SecretResolver>,
}

/// Ensure the last-accepted manifest's bundle is pulled + swapped
/// (B2). Infallible at the loop level: every failure is a logged
/// continue-from-last-swapped-bundle (Law 5); journaling happens
/// inside [`BundleStore::apply`].
async fn sync_bundle(store: &BundleStore, db: &mut AgentDb, source: &BundleSource) {
    match store.sync(db, source).await {
        Ok(Some(path)) => info!(bundle = %path.display(), "render bundle in place"),
        Ok(None) => {}
        Err(PullError::Unreachable(reason)) => {
            info!(%reason, "bundle source unreachable; continuing from last swapped bundle");
        }
        Err(e) => {
            warn!(error = %e, "bundle pull failed; continuing from last swapped bundle");
        }
    }
}

/// One converge + report pass (B3): diff the swapped bundle against
/// applied state, act through the provider, journal status rows
/// locally FIRST, then flush anything unsent (backlog included) to
/// the server if reachable. Consumes only local state until the
/// final send — the first pass after restart works with the server
/// unreachable (Law 5).
async fn converge_and_report(
    db: &mut AgentDb,
    data_dir: &Path,
    store: &BundleStore,
    provider: &dyn Provider,
    sink: Option<&StatusSink>,
    hooks: &ExtHooks,
) {
    #[cfg_attr(not(feature = "ext-secrets"), allow(unused_mut))]
    let mut desired = resolve_desired(db, store);
    // ext-secrets (REV-009): resolve `${secret:<name>}` references
    // and materialize per-service env files BEFORE converging, so
    // `up -d` runs against current secrets; failures mutate `desired`
    // (hold/defer) and never block convergence of already-resolved
    // apps (spec/reeve/10-secrets.md §12.3, Law 5).
    #[cfg(feature = "ext-secrets")]
    reeve_agent::ext::secrets::sync_env(db, data_dir, hooks.secrets.as_ref(), &mut desired).await;
    #[cfg(not(feature = "ext-secrets"))]
    let _ = hooks;
    let reports = converge(db, data_dir, provider, &desired);
    if !reports.is_empty() {
        info!(acted_on = reports.len(), "converge pass acted");
        record_reports(db, &reports);
    }
    // Flush unsent status rows every cycle (store-and-forward,
    // spec/reeve/05-health-journal.md §7.3) — also drains the
    // backlog accumulated while offline.
    if let Some(sink) = sink {
        sink.send_unsent(db).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Operational contract (CLAUDE.md Substrate rules): structured
    // logs to stdout, config via env/files.
    tracing_subscriber::fmt().with_writer(std::io::stdout).init();

    // Subcommand dispatch (minimal, no CLI framework): `enroll` runs
    // the D4 ceremony and exits; no subcommand runs the poll loop.
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) == Some("enroll") {
        args.next();
        let opts = reeve_agent::enroll::parse_enroll_args(args)
            .map_err(|e| anyhow::anyhow!(e))?;
        let cfg = reeve_agent::enroll(&opts).await?;
        info!(
            device_id = cfg.device_id.as_deref().unwrap_or(""),
            config = %opts.config_path.display(),
            "enrolled; start the agent to begin converging"
        );
        return Ok(());
    }
    if let Some(other) = args.peek() {
        anyhow::bail!(
            "unknown subcommand {other:?}\nusage: reeve-agent [enroll --server <URL> --token <JOIN_TOKEN>]"
        );
    }

    let config = AgentConfig::load().map_err(|e| {
        error!(error = %e, "cannot load agent config");
        anyhow::anyhow!(e)
    })?;
    info!(server = %config.server, data_dir = %config.data_dir.display(), "reeve-agent starting");

    // Startup IS recovery: opening the DB is the whole ceremony.
    let mut db = AgentDb::open(&config.db_path())?;

    // First converge must not block on network (Law 5): say what we
    // already hold before the first poll.
    match db.last_accepted() {
        Ok(Some(a)) => info!(
            manifest_version = a.version.0,
            etag = %a.etag,
            "continuing from last accepted manifest"
        ),
        Ok(None) => info!("no previously accepted manifest; awaiting first"),
        Err(e) => warn!(error = %e, "could not read last accepted manifest"),
    }
    if let Ok(apps) = db.applied_apps() {
        info!(applied_apps = apps.len(), "continuing from applied state");
    }

    let source = ManifestSource::parse(&config.server, config.device_token.clone())
        .map_err(|e| anyhow::anyhow!(e))?;
    let bundle_source = BundleSource::parse(&config.server, config.device_token.clone())
        .map_err(|e| anyhow::anyhow!(e))?;

    // Bundle store recovery (Law 3): wipe crashed work/ entries,
    // roll forward an interrupted swap-then-record, GC unreferenced
    // bundles — then resume any interrupted pull WITHOUT waiting for
    // the first poll (offline-first: the source may be gone; that's
    // a logged no-op).
    let store = BundleStore::open(&config.data_dir)?;
    match store.recover(&mut db) {
        Ok(Some(digest)) => info!(%digest, "continuing from swapped render bundle"),
        Ok(None) => info!("no render bundle in place yet"),
        Err(e) => warn!(error = %e, "bundle store recovery incomplete"),
    }
    sync_bundle(&store, &mut db, &bundle_source).await;

    // The compose provider (docs/decisions/agent.md D5) and the
    // status sink (Margo deployment-status path; None for dir://
    // sources and unenrolled agents — reports then accumulate
    // locally, spec/reeve/05-health-journal.md §7.3).
    let provider = CommandComposeProvider::new(&config.data_dir);
    let sink = StatusSink::from_config(
        &config.server,
        config.device_token.clone(),
        config.device_id.clone(),
    );
    if sink.is_none() {
        info!("no status sink (dir:// source or not enrolled); status reports journal locally");
    }

    // Extension hooks compiled in behind their ext-* features.
    #[cfg_attr(not(feature = "ext-secrets"), allow(unused_mut))]
    let mut hooks = ExtHooks::default();
    #[cfg(feature = "ext-secrets")]
    {
        hooks.secrets = reeve_agent::ext::secrets::SecretResolver::from_config(
            &config.server,
            config.device_token.clone(),
        );
        if hooks.secrets.is_none() {
            info!("no secrets resolver (dir:// source or not enrolled); apps with secret references defer");
        }
    }

    // First converge BEFORE the first poll: startup IS recovery
    // (Law 3 — any non-terminal phase re-runs) and must work from
    // last known state with the server unreachable (Law 5).
    converge_and_report(&mut db, &config.data_dir, &store, &provider, sink.as_ref(), &hooks).await;

    // Capability probe once per startup (spec/reeve/01-framework.md
    // §3.3: probe per enrollment and on version change; a restart
    // covers "on version change" — ours may have changed). 404 or
    // any error => vanilla Margo server => pure Margo behavior.
    match source.probe_capabilities().await {
        Some(caps) => info!(
            server_version = %caps.server_version,
            extensions = ?caps.extensions,
            "reeve server capabilities"
        ),
        None => info!("no reeve capabilities advertised; proceeding with pure Margo behavior"),
    }

    // The loop: poll -> sync bundle -> converge -> report. Converge
    // runs on EVERY cycle — it is a silent no-op when converged (D5)
    // and doubles as the retry path for failed applies, interrupted
    // phases, and unsent status backlog.
    let interval = Duration::from_secs(config.poll_interval_secs.max(1));
    loop {
        match poll_once(&mut db, &source).await {
            PollOutcome::NotModified => {
                info!("manifest unchanged (304)");
                // 304 does NOT mean the bundle is in place: an
                // accept whose pull failed/crashed retries here
                // (sync short-circuits when already swapped).
                sync_bundle(&store, &mut db, &bundle_source).await;
            }
            PollOutcome::SourceUnavailable => {
                // Already journaled + logged inside poll_once.
                // Converge still runs from last known state (Law 5).
            }
            PollOutcome::Accepted { manifest, etag, epoch_bump } => {
                info!(
                    manifest_version = manifest.manifest_version.0,
                    etag = %etag,
                    epoch_bump,
                    apps = manifest.apps.len(),
                    "new desired state accepted; pulling render bundle"
                );
                sync_bundle(&store, &mut db, &bundle_source).await;
            }
            PollOutcome::Rejected { received } => {
                warn!(received = received.0, "manifest rejected; holding last known state");
            }
        }
        converge_and_report(&mut db, &config.data_dir, &store, &provider, sink.as_ref(), &hooks)
            .await;
        tokio::time::sleep(interval).await;
    }
}
