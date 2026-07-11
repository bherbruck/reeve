//! GIT_HASH for `--version` (spec/reeve/08-packaging.md §10.1:
//! "version output MUST include the workspace git revision"; §10.5
//! reads the agent version back through status). Fallback `unknown`
//! when git or the repo is unavailable (source tarball builds).

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(&manifest_dir)
        .output();
    let hash = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=GIT_HASH={hash}");
    let git_head = manifest_dir.join("../../.git/HEAD");
    println!("cargo:rerun-if-changed={}", git_head.display());
}
