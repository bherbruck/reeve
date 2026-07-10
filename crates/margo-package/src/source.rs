//! Package sources: local directories (the v1 path — packages are
//! vendored, docs/decisions/tree-render.md D11) and typed-but-stubbed
//! OCI references (`reference/docs/upload-package.md` pushes packages
//! as OCI artifacts with media type
//! `application/vnd.margo.app.description.v1+yaml`; fetching them is
//! a later pre-fetch step, never part of parse/render).

use std::path::PathBuf;
use std::str::FromStr;

use crate::error::PackageError;

/// Where a package comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// A package directory on local disk (`margo.yaml` +
    /// `resources/`, the layout of
    /// `reference/poc/tests/artefacts/*/margo-package/`). Written
    /// `dir://<path>` or as a bare path.
    Dir(PathBuf),
    /// An `oci://` reference (the form used by `repository:` fields
    /// in `ApplicationDescription-001.yaml`). Parsed, not fetched:
    /// [`crate::Package::load`] returns
    /// [`PackageError::UnsupportedSource`] for this variant in v1.
    Oci(OciRef),
}

impl PackageSource {
    /// Parse a source string. `oci://...` is an OCI reference,
    /// `dir://<path>` or any other string is a directory path.
    pub fn parse(s: &str) -> Result<Self, PackageError> {
        if s.starts_with("oci://") {
            return Ok(Self::Oci(OciRef::parse(s)?));
        }
        let path = s.strip_prefix("dir://").unwrap_or(s);
        Ok(Self::Dir(PathBuf::from(path)))
    }
}

impl FromStr for PackageSource {
    type Err = PackageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// A parsed `oci://registry/repository[:tag][@algo:digest]`
/// reference (typed stub — see [`PackageSource::Oci`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciRef {
    /// Registry host (may include a port), e.g.
    /// `northstarida.azurecr.io`.
    pub registry: String,
    /// Repository path within the registry, e.g. `charts/hello-world`.
    pub repository: String,
    /// Optional tag, e.g. `1.0.1`.
    pub tag: Option<String>,
    /// Optional digest in `algo:hex` form, e.g. `sha256:abc...`.
    pub digest: Option<String>,
}

impl OciRef {
    /// Parse an `oci://` reference string.
    pub fn parse(reference: &str) -> Result<Self, PackageError> {
        let err = |reason: &str| PackageError::InvalidOciRef {
            reference: reference.to_string(),
            reason: reason.to_string(),
        };

        let rest = reference
            .strip_prefix("oci://")
            .ok_or_else(|| err("missing `oci://` scheme"))?;

        let (rest, digest) = match rest.split_once('@') {
            Some((head, digest)) => {
                let valid = digest
                    .split_once(':')
                    .is_some_and(|(algo, hex)| !algo.is_empty() && !hex.is_empty());
                if !valid {
                    return Err(err("digest must be `algo:hex`"));
                }
                (head, Some(digest.to_string()))
            }
            None => (rest, None),
        };

        let (registry, path) = rest
            .split_once('/')
            .ok_or_else(|| err("missing repository path after registry host"))?;
        if registry.is_empty() {
            return Err(err("empty registry host"));
        }

        // A `:` in the last path segment separates the tag; a `:`
        // in the registry host is a port and must not be mistaken
        // for a tag separator.
        let (repository, tag) = match path.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => {
                if tag.is_empty() {
                    return Err(err("empty tag"));
                }
                (repo, Some(tag.to_string()))
            }
            _ => (path, None),
        };
        if repository.is_empty() {
            return Err(err("empty repository path"));
        }

        Ok(Self {
            registry: registry.to_string(),
            repository: repository.to_string(),
            tag,
            digest,
        })
    }
}

impl std::fmt::Display for OciRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "oci://{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repository_from_spec_fixture() {
        // `ApplicationDescription-001.yaml` component property:
        // repository: oci://northstarida.azurecr.io/charts/hello-world
        let r = OciRef::parse("oci://northstarida.azurecr.io/charts/hello-world").unwrap();
        assert_eq!(r.registry, "northstarida.azurecr.io");
        assert_eq!(r.repository, "charts/hello-world");
        assert_eq!(r.tag, None);
        assert_eq!(r.digest, None);
    }

    #[test]
    fn parses_tag_port_and_digest() {
        let r = OciRef::parse("oci://harbor.local:5000/library/app:0.1.0@sha256:deadbeef").unwrap();
        assert_eq!(r.registry, "harbor.local:5000");
        assert_eq!(r.repository, "library/app");
        assert_eq!(r.tag.as_deref(), Some("0.1.0"));
        assert_eq!(r.digest.as_deref(), Some("sha256:deadbeef"));
        assert_eq!(
            r.to_string(),
            "oci://harbor.local:5000/library/app:0.1.0@sha256:deadbeef"
        );
    }

    #[test]
    fn port_without_tag_is_not_a_tag() {
        // rsplit_once(':') hits the port; the `/` in the remainder
        // proves it is not a tag.
        let r = OciRef::parse("oci://harbor.local:5000/library/app").unwrap();
        assert_eq!(r.registry, "harbor.local:5000");
        assert_eq!(r.repository, "library/app");
        assert_eq!(r.tag, None);
    }

    #[test]
    fn rejects_malformed_refs() {
        assert!(OciRef::parse("https://example.com/x").is_err());
        assert!(OciRef::parse("oci://hostonly").is_err());
        assert!(OciRef::parse("oci:///no-registry").is_err());
        assert!(OciRef::parse("oci://reg/repo@notadigest").is_err());
        assert!(OciRef::parse("oci://reg/repo:").is_err());
    }

    #[test]
    fn source_parse_forms() {
        assert_eq!(
            PackageSource::parse("dir:///opt/pkg").unwrap(),
            PackageSource::Dir(PathBuf::from("/opt/pkg"))
        );
        assert_eq!(
            PackageSource::parse("relative/pkg").unwrap(),
            PackageSource::Dir(PathBuf::from("relative/pkg"))
        );
        assert!(matches!(
            PackageSource::parse("oci://reg/repo:1").unwrap(),
            PackageSource::Oci(_)
        ));
    }
}
