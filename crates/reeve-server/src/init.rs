//! `reeve-server init` — §10.3 self-install for the server role
//! (spec/reeve/08-packaging.md): emit deployment artifacts for the
//! operator's chosen substrate (a compose file, or systemd unit
//! files, plus the zot configuration when the registry profile is
//! selected — docs/decisions/deploy.md D9), create the secrets master
//! keyfile (REEVE_DATA/secret.key, 0600), and WARN that the keyfile
//! needs separate backup (10-secrets §12.2).
//!
//! Idempotent (Law 3 applies to installers): every file write is
//! temp+rename to a deterministic content, the keyfile is
//! load_or_create — re-running on a half-initialized dir converges
//! and never errors on "already exists".
//!
//! The emitted compose file IS the canonical deploy/compose.yml (D9:
//! "init emits a copy/variant of it, and CI keeps the two in sync") —
//! embedded verbatim at compile time, so the two cannot drift within
//! one build; tests/packaging_flow.rs pins the sync against the
//! checked-in file.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// The one checked-in compose file (docs/decisions/deploy.md D9),
/// embedded verbatim.
pub const CANONICAL_COMPOSE: &str = include_str!("../../../deploy/compose.yml");

/// The §12.2 separate-backup warning — MUST reach the operator
/// (§10.3); returned as an action line AND logged by the caller.
pub const KEYFILE_BACKUP_WARNING: &str = "WARNING: secret.key is the master key for \
    everything shipped off-box (encrypted snapshots, changesets, the secrets vault). \
    It is NEVER included in snapshots — back it up separately, or every durability \
    generation is unreadable after a rebuild (spec/reeve/10-secrets.md §12.2, \
    07-durability §9.5).";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitFormat {
    Compose,
    Systemd,
}

#[derive(Debug, Clone)]
pub struct InitOptions {
    /// Where deployment artifacts land (`--out`).
    pub out_dir: PathBuf,
    /// `--format compose|systemd` (default compose).
    pub format: InitFormat,
    /// `--registry`: the D9 zot profile is selected — emit the zot
    /// config the compose profile mounts.
    pub registry: bool,
    /// REEVE_DATA_DIR (default ./data): where secret.key lives.
    pub data_dir: PathBuf,
    /// REEVE_UPSTREAM when set (D9 tier selection): the emitted zot
    /// config then syncs on demand from the hub's registry (D8).
    pub upstream: Option<String>,
}

/// Atomic file emission (Law 3 / D6 file-write rule): temp + rename,
/// same content every run — idempotent by construction.
fn emit(path: &Path, content: &str) -> anyhow::Result<()> {
    let dir = path.parent().context("emit path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("emit"),
        std::process::id()
    ));
    let mut f = std::fs::File::create(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming into place at {}", path.display()))?;
    Ok(())
}

/// The zot configuration the registry profile mounts (D9: emitted by
/// init, never checked in). Hub tiers cache/hold images; a tier with
/// an upstream syncs on demand from the hub's /v2 space (D8).
pub fn zot_config(upstream: Option<&str>) -> String {
    let mut cfg = serde_json::json!({
        "distSpecVersion": "1.1.1",
        "storage": { "rootDirectory": "/var/lib/registry" },
        "http": {
            // Reached only via reeve-server's /v2 proxy (D8): no
            // published ports in the compose file; zot listens for
            // the co-located server only.
            "address": "0.0.0.0",
            "port": "5000"
        },
        "log": { "level": "info" }
    });
    if let Some(upstream) = upstream {
        // Spoke tier (REEVE_UPSTREAM set, D9): mirror the hub's image
        // /v2 space on demand — the same route space reeve-server
        // proxies (D8).
        cfg["extensions"] = serde_json::json!({
            "sync": {
                "enable": true,
                "registries": [{
                    "urls": [upstream],
                    "onDemand": true,
                    "tlsVerify": true,
                    "content": [{ "prefix": "**" }]
                }]
            }
        });
    }
    let mut out = serde_json::to_string_pretty(&cfg).expect("static json");
    out.push('\n');
    out
}

/// The systemd unit for the bare-binary substrate (§10.3 "or systemd
/// unit files"). No secrets baked in (§10.3): values live in the
/// referenced EnvironmentFile, emitted as commented SHAPE only
/// (.env rule — never values).
fn server_unit() -> String {
    "\
[Unit]
Description=reeve-server — fleet desired-state manager
Documentation=https://github.com/bherbruck/reeve
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/reeve-server
# Values live here, not in this unit (spec/reeve/08-packaging.md
# §10.3: installers MUST NOT bake secrets into world-readable files).
EnvironmentFile=-/etc/reeve/reeve-server.env
Environment=REEVE_DATA_DIR=/var/lib/reeve
User=reeve
Group=reeve
StateDirectory=reeve
# Crash-only (Law 3): startup IS recovery; always restart.
Restart=always
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/reeve

[Install]
WantedBy=multi-user.target
"
    .to_string()
}

/// Commented config SHAPE for the unit's EnvironmentFile (.env rule:
/// shape checked in / emitted, values never).
fn server_env_shape() -> String {
    "\
# /etc/reeve/reeve-server.env — reeve-server configuration SHAPE.
# Uncomment and fill; this file may hold credentials — keep it 0600.
# Full key reference: `reeve-server --spec 08-packaging` and
# crates/reeve-server/src/config.rs.
#REEVE_LISTEN=0.0.0.0:8420
#REEVE_DATA_DIR=/var/lib/reeve
#REEVE_AUTH=password
# --- tier (docs/decisions/deploy.md D9): unset => root/hub ---------
#REEVE_UPSTREAM=
#REEVE_UPSTREAM_TOKEN=
#REEVE_SITE=
# --- durability (spec/reeve/07-durability.md §9) -------------------
#REEVE_DURABILITY=snapshot
#REEVE_DURABILITY_TARGET=s3://bucket/prefix
# --- image registry proxy (docs/decisions/delivery.md D8) ----------
#REEVE_ZOT_URL=http://127.0.0.1:5000
"
    .to_string()
}

/// Run init: emit artifacts into `out_dir`, ensure the keyfile.
/// Returns human-readable action lines (the keyfile backup warning is
/// always among them). Idempotent — a second run reports the same
/// actions and changes nothing.
pub fn run(opts: &InitOptions) -> anyhow::Result<Vec<String>> {
    let mut actions = Vec::new();
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("creating out dir {}", opts.out_dir.display()))?;

    match opts.format {
        InitFormat::Compose => {
            let path = opts.out_dir.join("compose.yml");
            emit(&path, CANONICAL_COMPOSE)?;
            actions.push(format!("wrote {} (docs/decisions/deploy.md D9)", path.display()));
        }
        InitFormat::Systemd => {
            let unit = opts.out_dir.join("reeve-server.service");
            emit(&unit, &server_unit())?;
            actions.push(format!("wrote {}", unit.display()));
            let env = opts.out_dir.join("reeve-server.env");
            emit(&env, &server_env_shape())?;
            actions.push(format!(
                "wrote {} (shape only — fill values, keep 0600)",
                env.display()
            ));
        }
    }

    if opts.registry {
        let path = opts.out_dir.join("zot-config.json");
        emit(&path, &zot_config(opts.upstream.as_deref()))?;
        actions.push(format!(
            "wrote {} ({})",
            path.display(),
            match &opts.upstream {
                Some(u) => format!("sync-on-demand from {u} — D8 spoke"),
                None => "hub: cache/hold images — D8".to_string(),
            }
        ));
    }

    // Keyfile (§10.3): REEVE_DATA/secret.key, 0600, created if
    // missing — idempotent via load_or_create (never re-mints).
    let key_path = opts.data_dir.join(crate::keyfile::KEY_FILE_NAME);
    let existed = key_path.exists();
    crate::keyfile::load_or_create(&key_path)
        .with_context(|| format!("ensuring keyfile {}", key_path.display()))?;
    actions.push(format!(
        "{} {}",
        if existed { "kept existing keyfile" } else { "created keyfile (0600)" },
        key_path.display()
    ));
    actions.push(KEYFILE_BACKUP_WARNING.to_string());
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &Path, format: InitFormat, registry: bool) -> InitOptions {
        InitOptions {
            out_dir: dir.join("out"),
            format,
            registry,
            data_dir: dir.join("data"),
            upstream: None,
        }
    }

    #[test]
    fn compose_init_is_idempotent_and_warns_about_the_keyfile() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path(), InitFormat::Compose, true);

        let first = run(&o).unwrap();
        assert!(first.iter().any(|a| a == KEYFILE_BACKUP_WARNING));
        let compose = std::fs::read_to_string(o.out_dir.join("compose.yml")).unwrap();
        assert_eq!(compose, CANONICAL_COMPOSE, "init emits the canonical file (D9)");
        assert!(o.out_dir.join("zot-config.json").exists());
        let key = std::fs::read(o.data_dir.join("secret.key")).unwrap();
        assert_eq!(key.len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(o.data_dir.join("secret.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Idempotent: same actionable outcome, keyfile NOT re-minted.
        let second = run(&o).unwrap();
        assert!(second.iter().any(|a| a == KEYFILE_BACKUP_WARNING));
        assert!(second.iter().any(|a| a.starts_with("kept existing keyfile")));
        assert_eq!(std::fs::read(o.data_dir.join("secret.key")).unwrap(), key);
    }

    #[test]
    fn systemd_init_bakes_no_values() {
        let dir = tempfile::tempdir().unwrap();
        let o = opts(dir.path(), InitFormat::Systemd, false);
        run(&o).unwrap();
        let unit = std::fs::read_to_string(o.out_dir.join("reeve-server.service")).unwrap();
        assert!(unit.contains("EnvironmentFile=-/etc/reeve/reeve-server.env"));
        assert!(unit.contains("Restart=always"), "crash-only: always restart");
        let env = std::fs::read_to_string(o.out_dir.join("reeve-server.env")).unwrap();
        for line in env.lines() {
            assert!(
                line.is_empty() || line.starts_with('#'),
                "env shape must carry no live values (.env rule): {line:?}"
            );
        }
        assert!(!o.out_dir.join("zot-config.json").exists(), "no registry profile selected");
    }

    #[test]
    fn zot_config_hub_vs_spoke() {
        let hub: serde_json::Value = serde_json::from_str(&zot_config(None)).unwrap();
        assert!(hub.get("extensions").is_none(), "hub holds images, no sync");
        let spoke: serde_json::Value =
            serde_json::from_str(&zot_config(Some("https://hub.example:8420"))).unwrap();
        assert_eq!(
            spoke["extensions"]["sync"]["registries"][0]["urls"][0],
            "https://hub.example:8420"
        );
        assert_eq!(spoke["extensions"]["sync"]["registries"][0]["onDemand"], true);
    }
}
