//! margo-package — parse and validate Margo application packages.
//!
//! A Margo application package is a directory holding a `margo.yaml`
//! ([`reeve_types::ApplicationDescription`], shape pinned by
//! `spec/margo/src/specification/applications/application-description.linkml.yaml`)
//! plus its linked `resources/` files — the layout of every package
//! in `reference/poc/tests/artefacts/*/margo-package/` and of the
//! OCI push flow in `reference/docs/upload-package.md`.
//!
//! This crate is WIRE-EXACT (spec/reeve/01-framework.md §5): the YAML
//! in `spec/margo/` and `reference/` are the test fixtures and MUST
//! parse unmodified. It is pure parse/validate — the only I/O is
//! reading a caller-supplied directory ([`Package::load_dir`]).
//!
//! Sources: local directories are the v1 path (packages are vendored,
//! docs/decisions/tree-render.md D11); `oci://` references parse into
//! a typed [`OciRef`] but loading one returns
//! [`PackageError::UnsupportedSource`].

mod error;
mod package;
mod source;
mod validate;

pub use error::PackageError;
pub use package::{MANIFEST_FILE_NAME, Package, parse_description};
pub use source::{OciRef, PackageSource};
pub use validate::{
    CPU_ARCHITECTURES, DATA_TYPES, INTERFACE_TYPES, PERIPHERAL_TYPES, PROFILE_TYPES, Severity,
    ValidationIssue, has_errors, validate_description,
};
