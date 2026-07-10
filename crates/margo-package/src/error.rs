//! Error type for `margo-package`.

use std::path::PathBuf;

use crate::validate::{Severity, ValidationIssue};

/// Everything that can go wrong loading or validating a Margo
/// application package.
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    /// The package directory exists but contains no `margo.yaml`
    /// (the manifest file name used by every package in
    /// `reference/poc/tests/artefacts/*/margo-package/` and by
    /// `reference/docs/upload-package.md`).
    #[error("package manifest not found under {0} (expected `margo.yaml`)")]
    ManifestNotFound(PathBuf),

    /// Filesystem error reading the package directory.
    #[error("i/o error reading {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The manifest is not parseable as a Margo
    /// `ApplicationDescription` (shape defined by
    /// `spec/margo/src/specification/applications/application-description.linkml.yaml`).
    #[error("failed to parse {context} as ApplicationDescription")]
    Parse {
        /// What was being parsed (a file path or a caller-supplied label).
        context: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    /// The manifest parsed but failed semantic validation. `issues`
    /// holds every finding (warnings included); at least one has
    /// [`Severity::Error`].
    #[error("application description is invalid ({} error(s))", error_count(issues))]
    Invalid { issues: Vec<ValidationIssue> },

    /// A syntactically invalid `oci://` reference.
    #[error("invalid OCI reference `{reference}`: {reason}")]
    InvalidOciRef { reference: String, reason: String },

    /// The package source is recognized but not implemented. OCI
    /// package refs are a typed stub in v1: packages are vendored
    /// directories (docs/decisions/tree-render.md D11/D13 — OCI refs
    /// later happen via a pre-fetch step that vendors, never via
    /// fetch-at-render).
    #[error("unsupported package source: {0}")]
    UnsupportedSource(String),
}

fn error_count(issues: &[ValidationIssue]) -> usize {
    issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .count()
}
