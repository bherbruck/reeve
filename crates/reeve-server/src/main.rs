//! Thin binary entrypoint; all logic lives in the reeve-server library so
//! integration tests exercise the same code paths.
//!
//! Invocations:
//! - `reeve-server`                        — run (normal startup)
//! - `reeve-server --restore-from-target`  — DR: with NO local DB and a
//!   configured durability target, restore the latest generation first
//!   (spec/reeve/07-durability.md §9.5; needs the keyfile in place too)
//! - `reeve-server verify-restore`         — one §9.4 verify-restore
//!   pass; prints the outcome as JSON, exit 0 iff the chain verified
//!
//! Packaging & self-hosting (C12, spec/reeve/08-packaging.md):
//! - `reeve-server --version`              — version + workspace git
//!   revision (§10.1)
//! - `reeve-server --spec [name]`          — print the embedded reeve
//!   spec, whole (index order) or one section (§10.1)
//! - `reeve-server --completions <shell>`  — bash|zsh|fish (§10.1)
//! - `reeve-server init --out <dir> [--format compose|systemd]
//!    [--registry]` — §10.3 self-install: emit deployment artifacts,
//!   ensure REEVE_DATA/secret.key (0600), WARN about separate keyfile
//!   backup. Idempotent.
//! - `reeve-server healthz`                — probe a running server's
//!   /healthz at REEVE_LISTEN (the compose healthcheck, D9)
//!
//! Federation air-gap (ext-federation, spec/reeve/06-federation.md
//! §8.5 — same binary at every tier, docs/decisions/deploy.md D9):
//! - `reeve-server export --out <dir-or-.tar> [--prefix <p>]...
//!    [--site <label>] [--recipient <b64 x25519 pubkey>]`
//!   — signed OCI layout archive of this tier's revision stream
//!   (+ scoped secrets sealed to the destination gateway)
//! - `reeve-server export-status --out <dir-or-.tar>`
//!   — journal records for sneakernet backfill at the parent
//! - `reeve-server import <archive> [--expect-signer <b64 key>]`
//!   — verify + verbatim append / journal ingest; idempotent
//! - `reeve-server tier-identity`
//!   — print this tier's public keys/fingerprints (commissioning)

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Structured logs to stdout (operational contract, CLAUDE.md).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // C12 packaging surfaces (spec/reeve/08-packaging.md §10.1/§10.3)
    // that must not require a valid runtime Config: --version, --spec,
    // --completions, init, healthz. `init` in particular runs BEFORE
    // the deployment env is complete (a lone REEVE_UPSTREAM would fail
    // Config validation), so it reads only the env keys it needs.
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            // §10.1: version output MUST include the workspace git
            // revision (build.rs GIT_HASH; "unknown" without a repo).
            println!(
                "reeve-server {} (git {})",
                env!("CARGO_PKG_VERSION"),
                env!("GIT_HASH")
            );
            return Ok(());
        }
        Some("--spec") => {
            match reeve_server::specdocs::render(args.get(1).map(String::as_str)) {
                Ok(text) => {
                    print!("{text}");
                    return Ok(());
                }
                Err(e) => anyhow::bail!(e),
            }
        }
        Some("--completions") => {
            let Some(shell) = args.get(1) else {
                anyhow::bail!("usage: reeve-server --completions <bash|zsh|fish>");
            };
            match reeve_server::completions::script(shell) {
                Ok(s) => {
                    print!("{s}");
                    return Ok(());
                }
                Err(e) => anyhow::bail!(e),
            }
        }
        Some("init") => return init_cmd(&args[1..]),
        Some("healthz") => return healthz_cmd(),
        _ => {}
    }

    let cfg = reeve_server::config::Config::from_env()?;

    match args.first().map(String::as_str) {
        Some("verify-restore") => {
            let outcome = reeve_server::durability::verify_restore_cli(cfg).await?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            if !outcome.ok {
                std::process::exit(1);
            }
            Ok(())
        }
        #[cfg(feature = "ext-federation")]
        Some("export") => {
            let mut out: Option<String> = None;
            let mut opts = reeve_server::ext::federation::ExportOptions::default();
            let mut it = args[1..].iter();
            while let Some(arg) = it.next() {
                match arg.as_str() {
                    "--out" => out = it.next().cloned(),
                    "--prefix" => opts.prefixes.extend(it.next().cloned()),
                    "--site" => opts.site = it.next().cloned(),
                    "--recipient" => opts.recipient_x25519 = it.next().cloned(),
                    other => anyhow::bail!("export: unknown argument {other:?}"),
                }
            }
            let out = out.ok_or_else(|| anyhow::anyhow!("export requires --out <dir-or-.tar>"))?;
            let state = reeve_server::bootstrap(cfg)?;
            reeve_server::ext::federation::export_tree(
                &state,
                std::path::Path::new(&out),
                &opts,
            )
        }
        #[cfg(feature = "ext-federation")]
        Some("export-status") => {
            let mut out: Option<String> = None;
            let mut it = args[1..].iter();
            while let Some(arg) = it.next() {
                match arg.as_str() {
                    "--out" => out = it.next().cloned(),
                    other => anyhow::bail!("export-status: unknown argument {other:?}"),
                }
            }
            let out =
                out.ok_or_else(|| anyhow::anyhow!("export-status requires --out <dir-or-.tar>"))?;
            let state = reeve_server::bootstrap(cfg)?;
            reeve_server::ext::federation::export_status(&state, std::path::Path::new(&out))
        }
        #[cfg(feature = "ext-federation")]
        Some("import") => {
            let mut archive: Option<String> = None;
            let mut expect_signer: Option<String> = None;
            let mut it = args[1..].iter();
            while let Some(arg) = it.next() {
                match arg.as_str() {
                    "--expect-signer" => expect_signer = it.next().cloned(),
                    other if archive.is_none() => archive = Some(other.to_string()),
                    other => anyhow::bail!("import: unknown argument {other:?}"),
                }
            }
            let archive =
                archive.ok_or_else(|| anyhow::anyhow!("import requires an archive path"))?;
            let state = reeve_server::bootstrap(cfg)?;
            let report = reeve_server::ext::federation::import_archive(
                &state,
                std::path::Path::new(&archive),
                expect_signer.as_deref(),
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        #[cfg(feature = "ext-federation")]
        Some("tier-identity") => {
            // Keys live beside the DB; mint at first use (§8.7).
            std::fs::create_dir_all(&cfg.data_dir)?;
            let identity = reeve_server::ext::federation::tier_identity_json(&cfg.data_dir)?;
            println!("{}", serde_json::to_string_pretty(&identity)?);
            Ok(())
        }
        _ => {
            let mut opts = reeve_server::RunOptions::default();
            for arg in &args {
                match arg.as_str() {
                    "--restore-from-target" => opts.restore_from_target = true,
                    other => anyhow::bail!(
                        "unknown argument {other:?} (subcommands: init, healthz, \
                         verify-restore, export, export-status, import, tier-identity; \
                         flags: --version, --spec [name], --completions <shell>, \
                         --restore-from-target)"
                    ),
                }
            }
            reeve_server::run_with_options(cfg, opts).await
        }
    }
}

/// `reeve-server init --out <dir> [--format compose|systemd]
/// [--registry]` (spec/reeve/08-packaging.md §10.3; docs/decisions/
/// deploy.md D9). Reads only REEVE_DATA_DIR / REEVE_UPSTREAM from the
/// env — init runs before a complete runtime configuration exists.
fn init_cmd(args: &[String]) -> anyhow::Result<()> {
    let usage = "usage: reeve-server init --out <dir> [--format compose|systemd] [--registry]";
    let mut out: Option<String> = None;
    let mut format = reeve_server::init::InitFormat::Compose;
    let mut registry = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => out = it.next().cloned(),
            "--format" => {
                format = match it.next().map(String::as_str) {
                    Some("compose") => reeve_server::init::InitFormat::Compose,
                    Some("systemd") => reeve_server::init::InitFormat::Systemd,
                    other => anyhow::bail!("--format must be compose|systemd, got {other:?}\n{usage}"),
                }
            }
            "--registry" => registry = true,
            other => anyhow::bail!("init: unknown argument {other:?}\n{usage}"),
        }
    }
    let Some(out) = out else {
        anyhow::bail!("init requires --out <dir>\n{usage}");
    };
    let opts = reeve_server::init::InitOptions {
        out_dir: std::path::PathBuf::from(out),
        format,
        registry,
        data_dir: std::path::PathBuf::from(
            std::env::var("REEVE_DATA_DIR").unwrap_or_else(|_| "./data".into()),
        ),
        upstream: std::env::var("REEVE_UPSTREAM").ok().filter(|s| !s.is_empty()),
    };
    for action in reeve_server::init::run(&opts)? {
        // The keyfile backup warning MUST reach the operator (§10.3);
        // stdout carries all action lines, warnings included.
        println!("init: {action}");
    }
    Ok(())
}

/// `reeve-server healthz` — probe /healthz on the local listener
/// (REEVE_LISTEN, default 0.0.0.0:8420). The deploy/compose.yml D9
/// healthcheck runs this INSIDE the container where no curl exists.
/// Plain std TCP; exit 0 iff HTTP 200.
fn healthz_cmd() -> anyhow::Result<()> {
    use std::io::{Read as _, Write as _};
    let listen = std::env::var("REEVE_LISTEN").unwrap_or_else(|_| "0.0.0.0:8420".into());
    let addr: std::net::SocketAddr = listen.parse()
        .map_err(|e| anyhow::anyhow!("REEVE_LISTEN {listen:?}: {e}"))?;
    // An unspecified bind address is probed via loopback.
    let probe = if addr.ip().is_unspecified() {
        std::net::SocketAddr::new("127.0.0.1".parse().unwrap(), addr.port())
    } else {
        addr
    };
    let mut stream = std::net::TcpStream::connect_timeout(&probe, std::time::Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("connect {probe}: {e}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.write_all(
        format!("GET /healthz HTTP/1.1\r\nHost: {probe}\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let ok = response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200");
    if ok {
        println!("ok");
        Ok(())
    } else {
        anyhow::bail!(
            "healthz probe failed at {probe}: {:?}",
            response.lines().next().unwrap_or("")
        );
    }
}
