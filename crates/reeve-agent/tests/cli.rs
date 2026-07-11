//! C12 packaging CLI surfaces of the agent binary
//! (spec/reeve/08-packaging.md §10.1: --version includes the
//! workspace git revision; both binaries support --spec; embedded
//! shell completions).

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reeve-agent"))
}

#[test]
fn version_includes_workspace_git_revision() {
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        format!("reeve-agent {} (git {})", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"))
    );
    if std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.git")).exists() {
        assert_ne!(env!("GIT_HASH"), "unknown");
    }
}

#[test]
fn spec_flag_prints_index_and_single_section() {
    let all = bin().arg("--spec").output().unwrap();
    assert!(all.status.success());
    let text = String::from_utf8(all.stdout).unwrap();
    assert!(text.starts_with("# The reeve Specification"), "00-INDEX.md first");
    assert!(text.contains("## 10. Packaging & Self-Hosting (REV-007)"));

    let one = bin().args(["--spec", "05-health-journal"]).output().unwrap();
    assert!(one.status.success());
    let text = String::from_utf8(one.stdout).unwrap();
    assert!(text.contains("(REV-004)"));
    assert!(!text.contains("(REV-007)"), "single section only");

    assert!(!bin().args(["--spec", "99-nope"]).output().unwrap().status.success());
}

#[test]
fn completions_flag_prints_shell_scripts() {
    for (shell, needle) in [
        ("bash", "complete -F _reeve_agent reeve-agent"),
        ("zsh", "#compdef reeve-agent"),
        ("fish", "complete -c reeve-agent"),
    ] {
        let out = bin().args(["--completions", shell]).output().unwrap();
        assert!(out.status.success(), "{shell}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains(needle), "{shell}: {stdout}");
        assert!(stdout.contains("rollback"), "{shell} completes subcommands");
    }
    assert!(!bin().args(["--completions", "powershell"]).output().unwrap().status.success());
}
