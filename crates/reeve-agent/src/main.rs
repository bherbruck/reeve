//! reeve-agent binary — thin shell over the library: load config,
//! open agent.db (startup IS recovery, Law 3), report what we're
//! continuing from, then poll forever (Law 5: offline is a logged
//! no-op, never an exit).

use std::path::Path;
use std::time::Duration;

use reeve_agent::{
    AgentConfig, AgentDb, AgentUpdater, BinDir, BinaryFetcher, BundleSource, BundleStore,
    CommandComposeProvider, ExitRestarter, ManifestSource, PollOutcome, Provider, PullError,
    Severity, StatusSink, converge_full, poll_once, record_reports, resolve_desired,
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
    /// ext-channel (REV-001): the persistent-channel task's nudge
    /// signal + rate limiter. `None` for `dir://` sources and
    /// unenrolled agents — the channel changes NOTHING about
    /// convergence either way (spec/reeve/02-channel.md §4.6).
    #[cfg(feature = "ext-channel")]
    channel: Option<reeve_agent::ext::channel::ChannelRuntime>,
    /// ext-terminal (REV-002): enablement gate + live-session
    /// registry. The gate exists whenever the feature is compiled
    /// in; ENABLEMENT comes only from desired state, re-evaluated
    /// after every converge pass (spec/reeve/03-terminal.md §5.2).
    #[cfg(feature = "ext-terminal")]
    terminal: Option<std::sync::Arc<reeve_agent::ext::terminal::TerminalGate>>,
    /// ext-health (REV-004): background health sampler + journal
    /// backfill sender (spec/reeve/05-health-journal.md §7). The
    /// sampler runs even for `dir://` sources — journaling is
    /// local-first (§7.1); only the backfill sender needs a server.
    #[cfg(feature = "ext-health")]
    health: Option<reeve_agent::ext::health::HealthRuntime>,
}

/// Wait between cycles: the poll interval tick, or — with the
/// channel up — a rate-limited nudge (spec/reeve/02-channel.md §4.4;
/// polling stays the correctness path, Law 5).
#[cfg(feature = "ext-channel")]
async fn wait_next_cycle(interval: Duration, hooks: &mut ExtHooks) {
    use reeve_agent::ext::channel::{CycleTrigger, next_cycle};
    match hooks.channel.as_mut() {
        Some(ch) => {
            if next_cycle(interval, ch).await == CycleTrigger::Nudge {
                info!("nudge: fetch-and-converge now (spec/reeve/02-channel.md §4.4)");
            }
        }
        None => tokio::time::sleep(interval).await,
    }
}

#[cfg(not(feature = "ext-channel"))]
async fn wait_next_cycle(interval: Duration, _hooks: &mut ExtHooks) {
    tokio::time::sleep(interval).await;
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
/// B8 self-update context (spec/reeve/08-packaging.md §10.5): the
/// A/B updater and the binary prefetcher, carried together through
/// the loop.
struct UpdateCtx {
    updater: AgentUpdater,
    fetcher: Option<BinaryFetcher>,
}

async fn converge_and_report(
    db: &mut AgentDb,
    data_dir: &Path,
    store: &BundleStore,
    provider: &dyn Provider,
    update: &UpdateCtx,
    sink: Option<&StatusSink>,
    hooks: &ExtHooks,
) {
    // B8 self-update prefetch (spec/reeve/08-packaging.md §10.5):
    // stage any agent-update binary the bundle names BEFORE converge,
    // so converge itself consumes only local state (Law 5 — same
    // split as the bundle pull; offline is a journaled no-op).
    reeve_agent::update::prefetch(
        db,
        store,
        update.updater.bin_dir(),
        update.fetcher.as_ref(),
        env!("CARGO_PKG_VERSION"),
    )
    .await;
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
    let reports = converge_full(db, data_dir, provider, Some(&update.updater), &desired);
    if !reports.is_empty() {
        info!(acted_on = reports.len(), "converge pass acted");
        record_reports(db, &reports);
    }
    // ext-terminal (REV-002): re-evaluate enablement from the state
    // just converged — the agent's terminal gate follows its LAST
    // CONVERGED desired state, online or offline, and converging to
    // a disabling commit terminates live sessions
    // (spec/reeve/03-terminal.md §5.2).
    #[cfg(feature = "ext-terminal")]
    if let Some(gate) = &hooks.terminal {
        reeve_agent::ext::terminal::sync_enablement(gate, store.current_path());
    }
    // Flush unsent status rows every cycle (store-and-forward,
    // spec/reeve/05-health-journal.md §7.3) — also drains the
    // backlog accumulated while offline.
    if let Some(sink) = sink {
        sink.send_unsent(db).await;
    }
    // ext-health (REV-004): drain unacknowledged journal records
    // (health samples, lifecycle marks, statuses, gap marks) to the
    // reeve journal surface. Running every cycle IS both the
    // reconnect backfill and the periodic sweep (§7.3); offline it
    // returns on the first send error and accumulates (Law 5).
    #[cfg(feature = "ext-health")]
    if let Some(runtime) = &hooks.health
        && let Some(sender) = runtime.sender()
    {
        reeve_agent::ext::health::backfill(db, sender).await;
    }
}

/// `reeve-agent install [--server <URL> --token <JOIN_TOKEN>]
/// [--root <PATH>]` — the §10.3 self-install (docs/decisions/
/// agent.md D4: with `--server`/`--token` it enrolls first, so one
/// command takes a bare box to a running, enrolled agent). `--root`
/// stages the whole layout under a prefix (testing/harness hook);
/// root privileges are required either way.
async fn install_cmd(
    args: std::iter::Peekable<impl Iterator<Item = String>>,
) -> anyhow::Result<()> {
    let usage = "usage: reeve-agent install [--server <URL> --token <JOIN_TOKEN>] [--root <PATH>]";
    let mut server = None;
    let mut token = None;
    let mut root = None;
    let mut it = args;
    while let Some(flag) = it.next() {
        let mut value =
            |name: &str| it.next().ok_or_else(|| anyhow::anyhow!("{name} requires a value\n{usage}"));
        match flag.as_str() {
            "--server" => server = Some(value("--server")?),
            "--token" => token = Some(value("--token")?),
            "--root" => root = Some(std::path::PathBuf::from(value("--root")?)),
            other => anyhow::bail!("unknown argument {other:?}\n{usage}"),
        }
    }
    let layout = root
        .as_deref()
        .map(reeve_agent::InstallLayout::under)
        .unwrap_or_else(reeve_agent::InstallLayout::system);

    // Enroll first when credentials were given (D4): the config this
    // writes is what step 6 of install() checks before starting the
    // unit.
    match (server, token) {
        (Some(server), Some(join_token)) => {
            let cfg = reeve_agent::enroll(&reeve_agent::EnrollOpts {
                server,
                join_token,
                config_path: layout.config_path(),
                data_dir: Some(layout.data_dir()),
            })
            .await?;
            info!(device_id = cfg.device_id.as_deref().unwrap_or(""), "enrolled");
        }
        (None, None) => {}
        _ => anyhow::bail!("--server and --token must be given together\n{usage}"),
    }

    let opts = reeve_agent::InstallOpts {
        source_binary: std::env::current_exe()?,
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let actions = reeve_agent::install(&reeve_agent::RealSys, &layout, &opts)
        .map_err(|e| anyhow::anyhow!(e))?;
    for a in &actions {
        println!("install: {a}");
    }
    Ok(())
}

/// `reeve-agent uninstall [--purge] [--root <PATH>]` — reverses
/// install (§10.3). Without `--purge` the device identity
/// (agent.toml) and state (data dir) survive.
fn uninstall_cmd(args: std::iter::Peekable<impl Iterator<Item = String>>) -> anyhow::Result<()> {
    let usage = "usage: reeve-agent uninstall [--purge] [--root <PATH>]";
    let mut purge = false;
    let mut root = None;
    let mut it = args;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--purge" => purge = true,
            "--root" => {
                root = Some(std::path::PathBuf::from(
                    it.next().ok_or_else(|| anyhow::anyhow!("--root requires a value\n{usage}"))?,
                ));
            }
            other => anyhow::bail!("unknown argument {other:?}\n{usage}"),
        }
    }
    let layout = root
        .as_deref()
        .map(reeve_agent::InstallLayout::under)
        .unwrap_or_else(reeve_agent::InstallLayout::system);
    let actions = reeve_agent::uninstall(&reeve_agent::RealSys, &layout, purge)
        .map_err(|e| anyhow::anyhow!(e))?;
    for a in &actions {
        println!("uninstall: {a}");
    }
    Ok(())
}

/// `reeve-agent rollback [--install-dir <PATH>]` — flip `current`
/// back to the retained previous binary and restart the unit
/// (spec/reeve/08-packaging.md §10.5). Invoked by the OnFailure
/// companion unit THROUGH the previous binary (see
/// crate::systemd::rollback_unit); safe to run by hand.
fn rollback_cmd(args: std::iter::Peekable<impl Iterator<Item = String>>) -> anyhow::Result<()> {
    let usage = "usage: reeve-agent rollback [--install-dir <PATH>]";
    let mut install_dir = std::path::PathBuf::from(reeve_agent::config::DEFAULT_INSTALL_DIR);
    let mut it = args;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--install-dir" => {
                install_dir = std::path::PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--install-dir requires a value\n{usage}"))?,
                );
            }
            other => anyhow::bail!("unknown argument {other:?}\n{usage}"),
        }
    }
    let bin = BinDir::new(&install_dir);
    let restored = bin.rollback().map_err(|e| anyhow::anyhow!(e))?;
    println!("rollback: current -> {restored}");
    // Best-effort restart: when invoked from the OnFailure unit we
    // run as root and systemd is there; by hand it may not be.
    use reeve_agent::UnitRestarter as _;
    let restarter = reeve_agent::update::SystemctlRestarter {
        unit: reeve_agent::systemd::AGENT_UNIT.to_string(),
    };
    match restarter.restart() {
        Ok(()) => println!("rollback: restart of {} requested", reeve_agent::systemd::AGENT_UNIT),
        Err(e) => warn!(error = %e, "rollback: could not restart unit; restart it manually"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Operational contract (CLAUDE.md Substrate rules): structured
    // logs to stdout, config via env/files.
    tracing_subscriber::fmt().with_writer(std::io::stdout).init();

    // Subcommand dispatch (minimal, no CLI framework): `enroll` runs
    // the D4 ceremony and exits; no subcommand runs the poll loop.
    let mut args = std::env::args().skip(1).peekable();
    // C12 packaging surfaces (spec/reeve/08-packaging.md §10.1).
    match args.peek().map(String::as_str) {
        Some("--version" | "-V") => {
            // §10.1: version output MUST include the workspace git
            // revision (build.rs GIT_HASH; "unknown" without a repo).
            println!(
                "reeve-agent {} (git {})",
                env!("CARGO_PKG_VERSION"),
                env!("GIT_HASH")
            );
            return Ok(());
        }
        Some("--spec") => {
            args.next();
            match reeve_agent::specdocs::render(args.next().as_deref()) {
                Ok(text) => {
                    print!("{text}");
                    return Ok(());
                }
                Err(e) => anyhow::bail!(e),
            }
        }
        Some("--completions") => {
            args.next();
            let Some(shell) = args.next() else {
                anyhow::bail!("usage: reeve-agent --completions <bash|zsh|fish>");
            };
            match reeve_agent::completions::script(&shell) {
                Ok(s) => {
                    print!("{s}");
                    return Ok(());
                }
                Err(e) => anyhow::bail!(e),
            }
        }
        _ => {}
    }
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
    // B8 self-install / uninstall / A-B rollback
    // (spec/reeve/08-packaging.md §10.3, §10.5).
    match args.peek().map(String::as_str) {
        Some("install") => {
            args.next();
            return install_cmd(args).await;
        }
        Some("uninstall") => {
            args.next();
            return uninstall_cmd(args);
        }
        Some("rollback") => {
            args.next();
            return rollback_cmd(args);
        }
        _ => {}
    }
    if let Some(other) = args.peek() {
        anyhow::bail!(
            "unknown subcommand {other:?}\nusage: reeve-agent [enroll --server <URL> --token <JOIN_TOKEN> | install [--server <URL> --token <JOIN_TOKEN>] | uninstall [--purge] | rollback | --version | --spec [name] | --completions <shell>]"
        );
    }

    let config = AgentConfig::load().map_err(|e| {
        error!(error = %e, "cannot load agent config");
        anyhow::anyhow!(e)
    })?;
    info!(server = %config.server, data_dir = %config.data_dir.display(), "reeve-agent starting");

    // Startup IS recovery: opening the DB is the whole ceremony.
    let mut db = AgentDb::open(&config.db_path())?;

    // Lifecycle mark (spec/reeve/05-health-journal.md §7.1: the
    // journal records agent start); mirrored to the wire journal by
    // trigger.
    if let Err(e) = db.journal(Severity::Info, "agent-start", env!("CARGO_PKG_VERSION")) {
        warn!(error = %e, "could not journal agent start");
    }

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
    // locally, spec/reeve/05-health-journal.md §7.3). Arc'd so the
    // ext-health sampler can query restart counts (§7.2) without
    // borrowing from the loop.
    let provider = std::sync::Arc::new(CommandComposeProvider::new(&config.data_dir));
    // B8 self-update (spec/reeve/08-packaging.md §10.5): the A/B
    // updater over config.install_dir with the exit-to-re-exec
    // restarter, and the binary prefetcher over the same source the
    // bundles come from.
    let update = UpdateCtx {
        updater: AgentUpdater::new(
            BinDir::new(&config.install_dir),
            Box::new(ExitRestarter),
            env!("CARGO_PKG_VERSION"),
        ),
        fetcher: BinaryFetcher::from_config(&config.server, config.device_token.clone()),
    };
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
    #[cfg(feature = "ext-health")]
    {
        // REV-004: sampler task + backfill sender. The sampler
        // journals locally regardless of connectivity (§7.1) and
        // feeds the live-status health slot (§7.3); a sampler
        // failure never touches convergence (Law 5).
        hooks.health = Some(reeve_agent::ext::health::spawn(
            &config,
            provider.clone(),
            sink.as_ref().map(|s| s.health_slot()),
        ));
    }
    #[cfg(feature = "ext-channel")]
    {
        // Sub-channel consumers register here, BEFORE spawn — the
        // hello frame advertises the registry's purposes.
        #[cfg_attr(not(feature = "ext-terminal"), allow(unused_mut))]
        let mut registry = reeve_agent::ext::channel::SubChannelRegistry::new();
        // ext-terminal (REV-002): rev-002/terminal handler. The gate
        // starts DISABLED and follows desired state only
        // (spec/reeve/03-terminal.md §5.2) — installing the handler
        // grants nothing by itself.
        #[cfg(feature = "ext-terminal")]
        {
            hooks.terminal = Some(reeve_agent::ext::terminal::TerminalGate::install(
                &mut registry,
            ));
        }
        // The task probes for rev-001/1 itself, per attempt, on the
        // §4.5 backoff schedule — never attempts the upgrade against
        // a server that doesn't advertise it (§4.1), and never blocks
        // startup or the first converge (§4.6).
        hooks.channel = reeve_agent::ext::channel::spawn(
            &config.server,
            config.device_token.clone(),
            registry,
        );
        if hooks.channel.is_none() {
            info!("no channel (dir:// source or not enrolled); presence/nudges unavailable (spec/reeve/02-channel.md §4.6)");
        }
    }

    // First converge BEFORE the first poll: startup IS recovery
    // (Law 3 — any non-terminal phase re-runs) and must work from
    // last known state with the server unreachable (Law 5).
    converge_and_report(
        &mut db,
        &config.data_dir,
        &store,
        provider.as_ref(),
        &update,
        sink.as_ref(),
        &hooks,
    )
    .await;

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
        converge_and_report(
            &mut db,
            &config.data_dir,
            &store,
            provider.as_ref(),
            &update,
            sink.as_ref(),
            &hooks,
        )
        .await;
        // Interval tick, or a rate-limited channel nudge (§4.4) —
        // either way the next iteration is a full poll+converge, so
        // the gap between polls never exceeds the interval.
        wait_next_cycle(interval, &mut hooks).await;
    }
}
