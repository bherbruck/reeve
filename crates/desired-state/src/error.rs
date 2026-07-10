//! Error type for `desired-state`.
//!
//! Every variant names the offending tree path or app so a render
//! failure points straight at the authoring mistake — the tree is
//! authored only through reeve-server's API (docs/decisions/
//! tree-render.md D11), so these surface as API validation errors.

/// Everything that can go wrong rendering a device's desired state
/// from a tree revision.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// A tree file failed to parse as YAML.
    #[error("tree file `{path}` is not valid YAML")]
    Yaml {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    /// A tree file parsed but has the wrong shape (e.g. app.yaml not
    /// a mapping, `enabled` not a bool, `package.version` not a
    /// string).
    #[error("`{path}`: {message}")]
    Invalid { path: String, message: String },

    /// A path under `layers/<layer>/apps/` that is not `app.yaml`,
    /// `params.yaml`, or `files/**` (docs/decisions/tree-render.md
    /// D11 defines exactly those three per-app entries). Rejected,
    /// not ignored: a typo'd `params.yml` silently ignored would be
    /// a config change that never lands.
    #[error(
        "unexpected tree path `{path}` (an app dir may contain only \
         app.yaml, params.yaml and files/** — docs/decisions/tree-render.md D11)"
    )]
    UnexpectedTreePath { path: String },

    /// The merged app.yaml chain never produced a `package` ref.
    #[error(
        "app `{app}`: no package reference; the merged app.yaml must \
         carry `package.name` and `package.version` (docs/decisions/tree-render.md D11)"
    )]
    MissingPackageRef { app: String },

    /// The referenced package is not vendored in this revision — and
    /// there is NO fetch-at-render, ever (docs/decisions/tree-render.md
    /// D11: packages are revision content).
    #[error(
        "app `{app}`: package not vendored in this revision (expected \
         `{path}`; no fetch-at-render — docs/decisions/tree-render.md D11)"
    )]
    PackageNotFound { app: String, path: String },

    /// The vendored package's `margo.yaml` failed margo-package
    /// parsing or validation.
    #[error("app `{app}`: invalid vendored package")]
    Package {
        app: String,
        #[source]
        source: margo_package::PackageError,
    },

    /// app.yaml selected a deployment profile id the application
    /// description does not define.
    #[error("app `{app}`: no deployment profile with id `{profile}`")]
    UnknownProfile { app: String, profile: String },

    /// No `profile` selection and no unambiguous default (single
    /// profile, or single compose-typed profile).
    #[error(
        "app `{app}`: cannot pick a deployment profile unambiguously; \
         set `profile: <id>` in app.yaml"
    )]
    AmbiguousProfile { app: String },

    /// v1 renders compose profiles only (CLAUDE.md substrate rules:
    /// compose first; helm later/never).
    #[error(
        "app `{app}`: unsupported deployment profile type \
         `{profile_type}` (v1 renders `compose` profiles only)"
    )]
    UnsupportedProfileType { app: String, profile_type: String },

    /// params.yaml set a parameter the application description does
    /// not declare — an authoring error, not silently dropped.
    #[error(
        "app `{app}`: parameter `{parameter}` is not declared by the \
         application description"
    )]
    UnknownParameter { app: String, parameter: String },

    /// The compose deployment artifact could not be resolved from the
    /// vendored package (missing/remote `packageLocation`, non-UTF-8
    /// or non-mapping compose file, ...).
    #[error("app `{app}`: compose source: {message}")]
    ComposeSource { app: String, message: String },
}
