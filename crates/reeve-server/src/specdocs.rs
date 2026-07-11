//! Embedded reeve specification (spec/reeve/08-packaging.md §10.1):
//! "Both binaries MUST support `--spec`: the binary embeds the
//! `spec/reeve/` directory (the split files are the canonical form)
//! and prints the files in index order at runtime; `--spec <name>`
//! MAY print a single section." The deployed artifact carries its own
//! contract.
//!
//! Index order is the numeric file order the 00-INDEX.md file map
//! declares (00-INDEX, 01-framework, … 10-secrets) — exactly the
//! `NN-name.md` files; the moved-notice stubs (SPEC.md, DECISIONS.md,
//! README.md) are not part of the specification text and are excluded
//! from the embed.

#[derive(rust_embed::RustEmbed)]
#[folder = "../../spec/reeve"]
#[include = "*.md"]
#[exclude = "SPEC.md"]
#[exclude = "DECISIONS.md"]
#[exclude = "README.md"]
struct SpecDir;

/// Embedded section file names in index order (`00-INDEX.md` first).
pub fn section_files() -> Vec<String> {
    let mut files: Vec<String> = SpecDir::iter()
        .map(|f| f.to_string())
        // Only the numbered section files are the spec (defensive:
        // the excludes above already drop the stubs).
        .filter(|f| f.len() > 3 && f.as_bytes()[0].is_ascii_digit() && f.as_bytes()[1].is_ascii_digit() && f.as_bytes()[2] == b'-')
        .collect();
    files.sort();
    files
}

/// The whole spec (all sections, index order) or one section by name.
/// `name` matches a file stem loosely: `07-durability`,
/// `07-durability.md`, or just `durability`.
pub fn render(name: Option<&str>) -> Result<String, String> {
    let files = section_files();
    if files.is_empty() {
        // Embeds are compile-time; an empty embed means the build was
        // made from a tree without spec/reeve/ — say so honestly.
        return Err("this binary was built without spec/reeve/ embedded".into());
    }
    let selected: Vec<&String> = match name {
        None => files.iter().collect(),
        Some(n) => {
            let n = n.trim().trim_end_matches(".md");
            let matched: Vec<&String> = files
                .iter()
                .filter(|f| {
                    let stem = f.trim_end_matches(".md");
                    stem == n || stem.split_once('-').map(|(_, rest)| rest) == Some(n)
                })
                .collect();
            if matched.is_empty() {
                return Err(format!(
                    "unknown spec section {n:?}; sections: {}",
                    files
                        .iter()
                        .map(|f| f.trim_end_matches(".md"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            matched
        }
    };
    let mut out = String::new();
    for (i, file) in selected.iter().enumerate() {
        let data = SpecDir::get(file).ok_or_else(|| format!("embedded file {file} missing"))?;
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&String::from_utf8_lossy(&data.data));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_are_complete_and_index_ordered() {
        let files = section_files();
        assert_eq!(
            files,
            vec![
                "00-INDEX.md",
                "01-framework.md",
                "02-channel.md",
                "03-terminal.md",
                "04-status-stream.md",
                "05-health-journal.md",
                "06-federation.md",
                "07-durability.md",
                "08-packaging.md",
                "09-rollouts.md",
                "10-secrets.md",
            ]
        );
    }

    #[test]
    fn whole_spec_starts_at_the_index() {
        let all = render(None).unwrap();
        assert!(all.starts_with("# The reeve Specification"), "00-INDEX.md leads");
        assert!(all.contains("REV-007"), "packaging section present");
    }

    #[test]
    fn single_section_by_stem_and_by_suffix() {
        let by_stem = render(Some("08-packaging")).unwrap();
        assert!(by_stem.contains("## 10. Packaging & Self-Hosting (REV-007)"));
        assert_eq!(render(Some("packaging")).unwrap(), by_stem);
        assert_eq!(render(Some("08-packaging.md")).unwrap(), by_stem);
    }

    #[test]
    fn unknown_section_lists_the_valid_ones() {
        let err = render(Some("99-nope")).unwrap_err();
        assert!(err.contains("07-durability"), "{err}");
    }
}
