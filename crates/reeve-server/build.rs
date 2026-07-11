//! Build-time embedding (spec/reeve/08-packaging.md §10.1/§10.4):
//!
//! - `GIT_HASH` env for `--version` (workspace git revision; §10.1
//!   "version output MUST include the workspace git revision").
//!   Fallback `unknown` when git or the repo is unavailable (source
//!   tarball builds).
//! - `ui/dist` is embedded by rust-embed (src/assets.rs). Tolerated
//!   missing until Track D wires the UI build: we create the empty
//!   dir so the embed macro compiles, and warn.
//! - `ui/openapi.json` (Track D generates via `just gen-api`) is
//!   embedded IF PRESENT — OUT_DIR/openapi_embed.rs carries an
//!   `Option<&str>`.
//! - Agent binaries for /install (§10.4, cargo feature
//!   `embedded-agents`): embedded IF PRESENT from the dir named by
//!   `REEVE_AGENT_BINARIES` (files `reeve-agent-x86_64`,
//!   `reeve-agent-aarch64`), falling back to this workspace's own
//!   musl release outputs under target/. Version coherence is
//!   asserted HERE (§10.4 "enforced at build; the feature does not
//!   admit mixing"): a REEVE_AGENT_BINARIES dir MUST carry a
//!   GIT_HASH file matching the workspace revision.

use std::path::{Path, PathBuf};

fn git_hash(manifest_dir: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(manifest_dir)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// The agent binary for `arch`, if present: REEVE_AGENT_BINARIES dir
/// first (cross-built, CI), then this workspace's own musl release
/// output (same revision by construction).
fn agent_binary(manifest_dir: &Path, arch: &str, workspace_git: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("REEVE_AGENT_BINARIES") {
        let dir = PathBuf::from(dir);
        let candidate = dir.join(format!("reeve-agent-{arch}"));
        if candidate.exists() {
            // §10.4 version coherence: enforced at build when binaries
            // are provided from outside this cargo invocation.
            let hash_file = dir.join("GIT_HASH");
            let provided = std::fs::read_to_string(&hash_file).unwrap_or_else(|e| {
                panic!(
                    "REEVE_AGENT_BINARIES={} provides agent binaries but no readable \
                     GIT_HASH file ({e}); §10.4 version coherence cannot be asserted — \
                     write `git rev-parse HEAD` (or --short=12) into {}",
                    dir.display(),
                    hash_file.display()
                )
            });
            let provided = provided.trim();
            if workspace_git == "unknown" {
                println!(
                    "cargo:warning=workspace git revision unknown; accepting \
                     REEVE_AGENT_BINARIES GIT_HASH {provided} unverified"
                );
            } else if !(provided.starts_with(workspace_git) || workspace_git.starts_with(provided))
            {
                panic!(
                    "§10.4 version coherence: embedded agent binaries were built from \
                     {provided} but this server build is {workspace_git} — the \
                     embedded-agents feature does not admit mixing revisions"
                );
            }
            return Some(candidate);
        }
        return None;
    }
    // Same-workspace fallback: our own musl release build of the agent
    // (same revision as this server build by construction).
    let target_root = manifest_dir.join("../../target");
    let candidate = target_root.join(format!("{arch}-unknown-linux-musl/release/reeve-agent"));
    candidate.exists().then_some(candidate)
}

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    // --- GIT_HASH for --version (§10.1) ------------------------------
    let hash = git_hash(&manifest_dir);
    println!("cargo:rustc-env=GIT_HASH={hash}");
    // Re-run when HEAD moves (best effort; harmless if the path is
    // absent in a tarball build).
    let git_head = manifest_dir.join("../../.git/HEAD");
    println!("cargo:rerun-if-changed={}", git_head.display());

    // --- ui/dist (rust-embed, tolerates missing until Track D) -------
    let ui_dist = manifest_dir.join("../../ui/dist");
    println!("cargo:rerun-if-changed={}", ui_dist.display());
    if !ui_dist.exists() {
        // rust-embed needs the folder to exist; empty embeds nothing
        // and the SPA fallback 404s (assets.rs). Tighten to a hard
        // failure once `just build` runs in CI (Track D).
        std::fs::create_dir_all(&ui_dist).expect("creating empty ui/dist for the embed");
        println!(
            "cargo:warning=ui/dist not found at {} — created empty; run `just build` \
             (or `cd ui && npm run build`) before shipping",
            ui_dist.display()
        );
    }

    // --- openapi.json embed-if-present (§10.1) ------------------------
    let openapi = manifest_dir.join("../../ui/openapi.json");
    println!("cargo:rerun-if-changed={}", openapi.display());
    let openapi_src = if openapi.exists() {
        format!(
            "pub(crate) const OPENAPI_JSON: Option<&str> = Some(include_str!({:?}));\n",
            openapi.canonicalize().expect("canonicalize openapi.json")
        )
    } else {
        "pub(crate) const OPENAPI_JSON: Option<&str> = None;\n".to_string()
    };
    std::fs::write(out_dir.join("openapi_embed.rs"), openapi_src).expect("write openapi_embed.rs");

    // --- embedded agent binaries (§10.4, feature embedded-agents) ----
    println!("cargo:rerun-if-env-changed=REEVE_AGENT_BINARIES");
    let mut agents_src = String::new();
    for (arch, konst) in [("x86_64", "AGENT_X86_64"), ("aarch64", "AGENT_AARCH64")] {
        match agent_binary(&manifest_dir, arch, &hash) {
            Some(path) => {
                println!("cargo:rerun-if-changed={}", path.display());
                agents_src.push_str(&format!(
                    "#[allow(dead_code)]\npub(crate) const {konst}: Option<&[u8]> = \
                     Some(include_bytes!({:?}));\n",
                    path.canonicalize().expect("canonicalize agent binary")
                ));
            }
            None => {
                if std::env::var("CARGO_FEATURE_EMBEDDED_AGENTS").is_ok() {
                    println!(
                        "cargo:warning=embedded-agents: no reeve-agent binary for {arch} \
                         (REEVE_AGENT_BINARIES or target/{arch}-unknown-linux-musl/release) — \
                         /install will refuse that architecture"
                    );
                }
                agents_src.push_str(&format!(
                    "#[allow(dead_code)]\npub(crate) const {konst}: Option<&[u8]> = None;\n"
                ));
            }
        }
    }
    std::fs::write(out_dir.join("agent_binaries.rs"), agents_src).expect("write agent_binaries.rs");
}
