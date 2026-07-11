//! Embedded reeve specification (spec/reeve/08-packaging.md §10.1):
//! "Both binaries MUST support `--spec`" — the deployed artifact
//! carries its own contract. Same contract as reeve-server's
//! specdocs.rs; duplicated because every crate stands alone (Law 2).
//!
//! Index order is the numeric file order the 00-INDEX.md file map
//! declares (00-INDEX, 01-framework, … 10-secrets); the moved-notice
//! stubs (SPEC.md, DECISIONS.md, README.md) are not specification
//! text and are excluded from the embed.

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
        .filter(|f| {
            f.len() > 3
                && f.as_bytes()[0].is_ascii_digit()
                && f.as_bytes()[1].is_ascii_digit()
                && f.as_bytes()[2] == b'-'
        })
        .collect();
    files.sort();
    files
}

/// The whole spec (all sections, index order) or one section by name
/// (`07-durability`, `07-durability.md`, or `durability`).
pub fn render(name: Option<&str>) -> Result<String, String> {
    let files = section_files();
    if files.is_empty() {
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
        assert_eq!(files.first().map(String::as_str), Some("00-INDEX.md"));
        assert_eq!(files.last().map(String::as_str), Some("10-secrets.md"));
        assert_eq!(files.len(), 11);
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
    }

    #[test]
    fn single_section_selects() {
        let s = render(Some("08-packaging")).unwrap();
        assert!(s.contains("## 10. Packaging & Self-Hosting (REV-007)"));
        assert!(render(Some("99-nope")).is_err());
    }
}
