//! `--completions <shell>` (spec/reeve/08-packaging.md §10.1: the
//! binary embeds its own shell completions). The CLI is a hand-rolled
//! dispatcher (main.rs) — no clap in the tree — so the completions
//! are generated the boring way from the same subcommand/flag tables
//! the dispatcher matches on. Update BOTH together (the packaging
//! test asserts every subcommand appears in every script).

/// Subcommands of `reeve-server` (main.rs dispatch). Federation
/// subcommands are listed unconditionally: completions are inert
/// text, and a --no-default-features binary answering "unknown
/// subcommand" is clearer than divergent scripts per build.
pub const SUBCOMMANDS: &[(&str, &str)] = &[
    ("init", "emit deployment artifacts (compose/systemd, zot config, keyfile)"),
    ("healthz", "probe a running server's /healthz (compose healthcheck)"),
    ("verify-restore", "run one durability verify-restore pass"),
    ("export", "air-gap: signed OCI layout archive of this tier's revisions"),
    ("export-status", "air-gap: journal records for sneakernet backfill"),
    ("import", "air-gap: verify + append an exported archive"),
    ("tier-identity", "print this tier's public keys (commissioning)"),
];

/// Top-level flags (main.rs dispatch).
pub const FLAGS: &[&str] = &["--version", "--spec", "--completions", "--restore-from-target"];

const SHELLS: &str = "bash zsh fish";

/// The completion script for `shell`, or an error naming the
/// supported shells.
pub fn script(shell: &str) -> Result<String, String> {
    match shell {
        "bash" => Ok(bash()),
        "zsh" => Ok(zsh()),
        "fish" => Ok(fish()),
        other => Err(format!("unsupported shell {other:?} (supported: {SHELLS})")),
    }
}

fn words() -> String {
    SUBCOMMANDS
        .iter()
        .map(|(name, _)| *name)
        .chain(FLAGS.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

fn spec_sections() -> String {
    crate::specdocs::section_files()
        .iter()
        .map(|f| f.trim_end_matches(".md").to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn bash() -> String {
    format!(
        r#"# bash completion for reeve-server (reeve-server --completions bash)
_reeve_server() {{
    local cur prev
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    case "$prev" in
        --completions)
            COMPREPLY=($(compgen -W "{SHELLS}" -- "$cur")); return ;;
        --spec)
            COMPREPLY=($(compgen -W "{sections}" -- "$cur")); return ;;
        --out|--format)
            return ;;
    esac
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=($(compgen -W "{words}" -- "$cur"))
    fi
}}
complete -F _reeve_server reeve-server
"#,
        sections = spec_sections(),
        words = words(),
    )
}

fn zsh() -> String {
    let subcommands = SUBCOMMANDS
        .iter()
        .map(|(name, desc)| format!("        '{name}:{desc}'"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"#compdef reeve-server
# zsh completion for reeve-server (reeve-server --completions zsh)
local -a subcommands
subcommands=(
{subcommands}
)
_arguments \
    '--version[print version and workspace git revision]' \
    '--spec[print the embedded reeve spec]: :({sections})' \
    '--completions[print shell completions]: :({SHELLS})' \
    '--restore-from-target[DR: restore latest generation before startup]' \
    '1: :{{_describe subcommand subcommands}}'
"#,
        sections = spec_sections(),
    )
}

fn fish() -> String {
    let mut out = String::from(
        "# fish completion for reeve-server (reeve-server --completions fish)\n\
         complete -c reeve-server -f\n",
    );
    for (name, desc) in SUBCOMMANDS {
        out.push_str(&format!(
            "complete -c reeve-server -n __fish_use_subcommand -a {name} -d \"{desc}\"\n"
        ));
    }
    for flag in FLAGS {
        let flag = flag.trim_start_matches("--");
        out.push_str(&format!("complete -c reeve-server -l {flag}\n"));
    }
    out.push_str(&format!(
        "complete -c reeve-server -l spec -x -a \"{}\"\n",
        spec_sections()
    ));
    out.push_str(&format!("complete -c reeve-server -l completions -x -a \"{SHELLS}\"\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_covers_every_subcommand_and_flag() {
        for shell in ["bash", "zsh", "fish"] {
            let s = script(shell).unwrap();
            for (name, _) in SUBCOMMANDS {
                assert!(s.contains(name), "{shell} missing subcommand {name}");
            }
            for flag in FLAGS {
                let bare = flag.trim_start_matches("--");
                assert!(s.contains(bare), "{shell} missing flag {flag}");
            }
            assert!(s.contains("08-packaging"), "{shell} completes --spec sections");
        }
        assert!(script("powershell").is_err());
    }
}
