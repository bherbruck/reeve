//! `--completions <shell>` for reeve-agent (spec/reeve/08-packaging.md
//! §10.1). Hand-rolled like the CLI itself (no clap in the tree);
//! generated from the same subcommand/flag tables main.rs matches on
//! — update both together.

/// Subcommands of `reeve-agent` (main.rs dispatch).
pub const SUBCOMMANDS: &[(&str, &str)] = &[
    ("enroll", "enroll against a server with a join token"),
    ("install", "self-install: user, unit, config; optionally enroll first"),
    ("uninstall", "reverse install (identity/state survive without --purge)"),
    ("rollback", "A/B: flip the current symlink back to the previous binary"),
];

/// Top-level flags (main.rs dispatch).
pub const FLAGS: &[&str] = &["--version", "--spec", "--completions"];

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
        r#"# bash completion for reeve-agent (reeve-agent --completions bash)
_reeve_agent() {{
    local cur prev
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    case "$prev" in
        --completions)
            COMPREPLY=($(compgen -W "{SHELLS}" -- "$cur")); return ;;
        --spec)
            COMPREPLY=($(compgen -W "{sections}" -- "$cur")); return ;;
        --server|--token|--root|--install-dir)
            return ;;
    esac
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=($(compgen -W "{words}" -- "$cur"))
    fi
}}
complete -F _reeve_agent reeve-agent
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
        r#"#compdef reeve-agent
# zsh completion for reeve-agent (reeve-agent --completions zsh)
local -a subcommands
subcommands=(
{subcommands}
)
_arguments \
    '--version[print version and workspace git revision]' \
    '--spec[print the embedded reeve spec]: :({sections})' \
    '--completions[print shell completions]: :({SHELLS})' \
    '1: :{{_describe subcommand subcommands}}'
"#,
        sections = spec_sections(),
    )
}

fn fish() -> String {
    let mut out = String::from(
        "# fish completion for reeve-agent (reeve-agent --completions fish)\n\
         complete -c reeve-agent -f\n",
    );
    for (name, desc) in SUBCOMMANDS {
        out.push_str(&format!(
            "complete -c reeve-agent -n __fish_use_subcommand -a {name} -d \"{desc}\"\n"
        ));
    }
    for flag in FLAGS {
        let flag = flag.trim_start_matches("--");
        out.push_str(&format!("complete -c reeve-agent -l {flag}\n"));
    }
    out.push_str(&format!(
        "complete -c reeve-agent -l spec -x -a \"{}\"\n",
        spec_sections()
    ));
    out.push_str(&format!("complete -c reeve-agent -l completions -x -a \"{SHELLS}\"\n"));
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
        }
        assert!(script("powershell").is_err());
    }
}
