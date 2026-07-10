//! Loading a package from a directory.
//!
//! Package directory layout (every package under
//! `reference/poc/tests/artefacts/*/margo-package/` and the push flow
//! in `reference/docs/upload-package.md`):
//!
//! ```text
//! <root>/
//!   margo.yaml       # the ApplicationDescription
//!   resources/       # files linked from metadata.catalog.application
//! ```

use std::path::{Component, Path, PathBuf};

use reeve_types::margo::application::ApplicationDescription;

use crate::error::PackageError;
use crate::source::PackageSource;
use crate::validate::{Severity, ValidationIssue, has_errors, validate_description};

/// File name of the application description inside a package
/// directory (`reference/docs/upload-package.md`: "package directory
/// where margo.yaml and /resources present").
pub const MANIFEST_FILE_NAME: &str = "margo.yaml";

/// A loaded, validated Margo application package.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    /// The package directory this was loaded from.
    pub root: PathBuf,
    /// The parsed `margo.yaml`.
    pub description: ApplicationDescription,
    /// Non-fatal validation findings (all [`Severity::Warning`];
    /// error-severity findings fail the load instead).
    pub warnings: Vec<ValidationIssue>,
}

impl Package {
    /// Load a package from a source. Only [`PackageSource::Dir`] is
    /// implemented in v1 — packages are vendored directories
    /// (docs/decisions/tree-render.md D11); [`PackageSource::Oci`]
    /// returns [`PackageError::UnsupportedSource`].
    pub fn load(source: &PackageSource) -> Result<Self, PackageError> {
        match source {
            PackageSource::Dir(path) => Self::load_dir(path),
            PackageSource::Oci(oci_ref) => Err(PackageError::UnsupportedSource(format!(
                "{oci_ref} (OCI package sources are not implemented in v1; \
                 packages are vendored directories — docs/decisions/tree-render.md D11)"
            ))),
        }
    }

    /// Load and validate a package from a directory containing
    /// `margo.yaml`. Fails if the manifest is missing, unparseable,
    /// or has error-severity validation findings; warnings are
    /// retained on the returned [`Package`].
    pub fn load_dir(root: impl AsRef<Path>) -> Result<Self, PackageError> {
        let root = root.as_ref();
        let manifest_path = root.join(MANIFEST_FILE_NAME);
        if !manifest_path.is_file() {
            return Err(PackageError::ManifestNotFound(root.to_path_buf()));
        }

        let yaml = std::fs::read_to_string(&manifest_path).map_err(|source| PackageError::Io {
            path: manifest_path.clone(),
            source,
        })?;
        let description = parse_description_named(&yaml, &manifest_path.display().to_string())?;

        let mut issues = validate_description(&description);
        check_resource_links(root, &description, &mut issues);

        if has_errors(&issues) {
            return Err(PackageError::Invalid { issues });
        }

        Ok(Self {
            root: root.to_path_buf(),
            description,
            warnings: issues,
        })
    }

    /// Resolve a catalog resource link (e.g.
    /// `./resources/description.md`) against the package root.
    /// Returns `None` for non-local links (URLs) and for links that
    /// lexically escape the package root.
    pub fn resource_path(&self, link: &str) -> Option<PathBuf> {
        local_resource_path(&self.root, link)
    }
}

/// Parse YAML text as an [`ApplicationDescription`] (shape:
/// `spec/margo/src/specification/applications/application-description.linkml.yaml`).
/// Parsing is shape-only; run
/// [`validate_description`](crate::validate_description) for the
/// semantic rules.
pub fn parse_description(yaml: &str) -> Result<ApplicationDescription, PackageError> {
    parse_description_named(yaml, "application description")
}

fn parse_description_named(
    yaml: &str,
    context: &str,
) -> Result<ApplicationDescription, PackageError> {
    serde_yaml_ng::from_str(yaml).map_err(|source| PackageError::Parse {
        context: context.to_string(),
        source,
    })
}

/// Local-looking catalog links (`./resources/...` per
/// `ApplicationDescription-001.yaml`) must resolve inside the package
/// directory. Missing files are warnings, not errors: the pinned
/// reference sandbox ships dangling links
/// (`custom-otel-helm-app/margo.yaml` links `license.pdf`; the
/// directory has `license.txt`).
fn check_resource_links(
    root: &Path,
    description: &ApplicationDescription,
    issues: &mut Vec<ValidationIssue>,
) {
    let Some(app) = description
        .metadata
        .catalog
        .as_ref()
        .and_then(|c| c.application.as_ref())
    else {
        return;
    };

    let links = [
        ("icon", &app.icon),
        ("descriptionFile", &app.description_file),
        ("releaseNotes", &app.release_notes),
        ("licenseFile", &app.license_file),
    ];
    for (field, link) in links {
        let Some(link) = link else { continue };
        if !is_local_link(link) {
            continue;
        }
        let path = format!("metadata.catalog.application.{field}");
        match local_resource_path(root, link) {
            None => issues.push(ValidationIssue {
                severity: Severity::Warning,
                path,
                message: format!("resource link `{link}` escapes the package directory"),
            }),
            Some(resolved) if !resolved.is_file() => issues.push(ValidationIssue {
                severity: Severity::Warning,
                path,
                message: format!("resource link `{link}` does not exist in the package"),
            }),
            Some(_) => {}
        }
    }
}

/// A link is local iff it has no URL scheme and is not absolute.
fn is_local_link(link: &str) -> bool {
    !link.contains("://") && !Path::new(link).is_absolute()
}

fn local_resource_path(root: &Path, link: &str) -> Option<PathBuf> {
    if !is_local_link(link) {
        return None;
    }
    // Lexical containment check: `..` must never escape the root.
    let mut depth: i32 = 0;
    for component in Path::new(link).components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(root.join(link))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_links_resolve_inside_root() {
        let root = Path::new("/pkg");
        assert_eq!(
            local_resource_path(root, "./resources/a.md"),
            Some(PathBuf::from("/pkg/./resources/a.md"))
        );
        assert_eq!(local_resource_path(root, "../escape.md"), None);
        assert_eq!(local_resource_path(root, "a/../../escape.md"), None);
        assert_eq!(local_resource_path(root, "http://example.com/x"), None);
        assert_eq!(local_resource_path(root, "/abs/path.md"), None);
        assert!(local_resource_path(root, "a/../b.md").is_some());
    }
}
