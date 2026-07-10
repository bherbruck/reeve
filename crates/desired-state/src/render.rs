//! The render pipeline: (tree contents at a revision, device context)
//! -> rendered per-device file set (docs/decisions/tree-render.md D2,
//! D3, D11, D12). Pure functions, zero I/O: the tree is an in-memory
//! map of path -> bytes, the output is the same shape.

use std::collections::{BTreeMap, BTreeSet};

use reeve_types::margo::application::{ApplicationDescription, DeploymentProfile, Parameter};
use reeve_types::margo::deployment::{
    APPLICATION_DEPLOYMENT_API_VERSION, APPLICATION_DEPLOYMENT_KIND, ApplicationDeployment,
    DeploymentMetadata, DeploymentProfileSpec, DeploymentSpec,
};
use serde::Serialize;
use serde_yaml_ng::Value;
use uuid::Uuid;

use crate::emit::to_canonical_yaml;
use crate::error::RenderError;
use crate::merge::merge_value;

/// An in-memory file set: relative path -> bytes. Used both for the
/// render INPUT (revision content, D11 layout: `layers/**`,
/// `packages/**`) and the render OUTPUT (the D2 bundle layout:
/// `manifest.yaml`, `apps/<name>/**`). `BTreeMap` so iteration order —
/// and therefore everything derived from it — is deterministic.
pub type FileSet = BTreeMap<String, Vec<u8>>;

/// reeve's UUIDv5 namespace for deterministic ids
/// (docs/decisions/tree-render.md D2). Defined as
/// `UUIDv5(NAMESPACE_DNS, "reeve.dev")` =
/// `06c32e1b-5365-5c68-80a2-6cccfa182cf8` so any independent
/// implementation can re-derive it.
pub const REEVE_UUID_NAMESPACE: Uuid = Uuid::from_u128(0x06c32e1b_5365_5c68_80a2_6cccfa182cf8);

/// Deterministic deploymentId:
/// `UUIDv5(REEVE_UUID_NAMESPACE, "<device_id>/<app-name>")`
/// (docs/decisions/tree-render.md D2). A pure function of render
/// inputs — no DB coordination, stable across re-renders, survives
/// device wipe + re-enroll to the same identity.
pub fn deployment_id(device_id: &str, app_name: &str) -> Uuid {
    Uuid::new_v5(
        &REEVE_UUID_NAMESPACE,
        format!("{device_id}/{app_name}").as_bytes(),
    )
}

/// The declared render-input set (docs/decisions/tree-render.md D3):
/// everything that varies between renders enters HERE and is recorded
/// in `manifest.yaml` (D2). No clock, no environment reads, no
/// network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderContext {
    /// The device's identity (device row).
    pub device_id: String,
    /// The device's layer chain: layer dir names under `layers/`
    /// (e.g. `00-fleet`, `05-class.gpu`, `20-site.a`,
    /// `30-device.dev-1`). Membership comes from the device row, not
    /// from tree content (D11). The engine treats these as ordered
    /// opaque names — ONLY the numeric prefix (lexical sort) orders
    /// the merge; the fleet/class/region/site/device taxonomy is
    /// convention, not engine knowledge (D12).
    pub layers: Vec<String>,
    /// Tier registry endpoint (docs/decisions/delivery.md D8):
    /// replaces the literal `${REEVE_REGISTRY}` in rendered artifacts.
    pub registry_endpoint: String,
    /// Render generation counter (D2 manifest provenance).
    pub generation: u64,
    /// Tree revision id this render was produced from
    /// (docs/decisions/delivery.md D13 revision store).
    pub local_revision: u64,
    /// Upstream (hub) revision id when federated
    /// (spec/reeve/06-federation.md); `None` on a single tier.
    pub hub_revision: Option<u64>,
}

/// `manifest.yaml` — render provenance (docs/decisions/tree-render.md
/// D2): ONLY revision ids, device id, generation counter, registry
/// endpoint. NO timestamps — renders must be byte-identical when
/// inputs are identical.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RenderManifest<'a> {
    device_id: &'a str,
    generation: u64,
    registry_endpoint: &'a str,
    revisions: Revisions,
}

#[derive(Serialize)]
struct Revisions {
    #[serde(skip_serializing_if = "Option::is_none")]
    hub: Option<u64>,
    local: u64,
}

/// Render one device's desired state. Pure: same `(tree, ctx)` =>
/// byte-identical `FileSet` (D3 determinism MUST — the byte-identical
/// re-render table test holds the line).
pub fn render(tree: &FileSet, ctx: &RenderContext) -> Result<FileSet, RenderError> {
    // D3/D11: the numeric prefix makes the layer order lexically
    // sortable; caller order is irrelevant.
    let mut chain = ctx.layers.clone();
    chain.sort();
    chain.dedup();

    let mut out = FileSet::new();

    for app in discover_apps(tree, &chain)? {
        render_app(tree, ctx, &chain, &app, &mut out)?;
    }

    let manifest = RenderManifest {
        device_id: &ctx.device_id,
        generation: ctx.generation,
        registry_endpoint: &ctx.registry_endpoint,
        revisions: Revisions {
            hub: ctx.hub_revision,
            local: ctx.local_revision,
        },
    };
    out.insert(
        "manifest.yaml".to_string(),
        to_canonical_yaml(&manifest, "manifest.yaml")?,
    );

    Ok(out)
}

/// All tree entries starting with `prefix`.
fn with_prefix<'t>(
    tree: &'t FileSet,
    prefix: &str,
) -> impl Iterator<Item = (&'t String, &'t Vec<u8>)> {
    tree.range(prefix.to_string()..)
        .take_while(move |(k, _)| k.starts_with(prefix))
}

/// Every app defined by any layer in the chain (D11: "an app is
/// desired iff any layer in the chain defines it..."), and strict
/// layout validation of each app dir (only app.yaml / params.yaml /
/// files/**).
fn discover_apps(tree: &FileSet, chain: &[String]) -> Result<BTreeSet<String>, RenderError> {
    let mut apps = BTreeSet::new();
    for layer in chain {
        let prefix = format!("layers/{layer}/apps/");
        for (path, _) in with_prefix(tree, &prefix) {
            let rest = &path[prefix.len()..];
            let Some((app, entry)) = rest.split_once('/') else {
                // A file directly under apps/ — not an app dir.
                return Err(RenderError::UnexpectedTreePath { path: path.clone() });
            };
            if entry != "app.yaml" && entry != "params.yaml" && !entry.starts_with("files/") {
                return Err(RenderError::UnexpectedTreePath { path: path.clone() });
            }
            apps.insert(app.to_string());
        }
    }
    Ok(apps)
}

/// Parse a tree YAML file; empty / whole-document-null files count as
/// "no contribution".
fn parse_layer_file(path: &str, bytes: &[u8]) -> Result<Option<Value>, RenderError> {
    let value: Value =
        serde_yaml_ng::from_slice(bytes).map_err(|source| RenderError::Yaml {
            path: path.to_string(),
            source,
        })?;
    match value {
        Value::Null => Ok(None),
        Value::Mapping(_) => Ok(Some(value)),
        _ => Err(RenderError::Invalid {
            path: path.to_string(),
            message: "expected a YAML mapping".to_string(),
        }),
    }
}

/// Merge one named file (`app.yaml` or `params.yaml`) across the
/// chain per D3.
fn merge_chain_file(
    tree: &FileSet,
    chain: &[String],
    app: &str,
    file: &str,
) -> Result<Value, RenderError> {
    let mut merged = Value::Mapping(serde_yaml_ng::Mapping::new());
    for layer in chain {
        let path = format!("layers/{layer}/apps/{app}/{file}");
        if let Some(bytes) = tree.get(&path)
            && let Some(value) = parse_layer_file(&path, bytes)?
        {
            merge_value(&mut merged, value);
        }
    }
    Ok(merged)
}

/// Render a single app into `out` (the D2 per-app layout).
fn render_app(
    tree: &FileSet,
    ctx: &RenderContext,
    chain: &[String],
    app: &str,
    out: &mut FileSet,
) -> Result<(), RenderError> {
    let app_yaml = merge_chain_file(tree, chain, app, "app.yaml")?;
    let app_yaml_path = format!("merged app.yaml for app `{app}`");

    // App presence (D11): desired iff defined and merged `enabled` is
    // true; absent key defaults to enabled (a defined app is desired
    // unless switched off).
    match app_yaml.get("enabled") {
        None | Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => return Ok(()),
        Some(_) => {
            return Err(RenderError::Invalid {
                path: app_yaml_path,
                message: "`enabled` must be a boolean".to_string(),
            });
        }
    }

    // Strict app.yaml schema: the tree has a single writer
    // (reeve-server's API, D11/D14); unknown keys are authoring
    // errors, not extension points.
    if let Value::Mapping(map) = &app_yaml {
        for key in map.keys() {
            match key.as_str() {
                Some("enabled") | Some("package") | Some("profile") => {}
                _ => {
                    return Err(RenderError::Invalid {
                        path: app_yaml_path,
                        message: format!(
                            "unknown app.yaml key {key:?} (allowed: enabled, package, profile)"
                        ),
                    });
                }
            }
        }
    }

    // Package ref -> vendored package content (D11: packages are
    // revision content under packages/<name>/<version>/).
    let package = app_yaml
        .get("package")
        .ok_or_else(|| RenderError::MissingPackageRef {
            app: app.to_string(),
        })?;
    let (name, version) = match (
        package.get("name").and_then(Value::as_str),
        package.get("version").and_then(Value::as_str),
    ) {
        (Some(n), Some(v)) => (n, v),
        _ => {
            return Err(RenderError::Invalid {
                path: app_yaml_path,
                message: "`package.name` and `package.version` must be strings \
                          (quote numeric-looking versions)"
                    .to_string(),
            });
        }
    };
    let package_prefix = format!("packages/{name}/{version}/");
    let manifest_path = format!("{package_prefix}margo.yaml");
    let manifest_bytes =
        tree.get(&manifest_path)
            .ok_or_else(|| RenderError::PackageNotFound {
                app: app.to_string(),
                path: manifest_path.clone(),
            })?;
    let manifest_text =
        std::str::from_utf8(manifest_bytes).map_err(|_| RenderError::Invalid {
            path: manifest_path.clone(),
            message: "margo.yaml is not valid UTF-8".to_string(),
        })?;
    let description =
        margo_package::parse_description(manifest_text).map_err(|source| RenderError::Package {
            app: app.to_string(),
            source,
        })?;
    let issues = margo_package::validate_description(&description);
    if margo_package::has_errors(&issues) {
        return Err(RenderError::Package {
            app: app.to_string(),
            source: margo_package::PackageError::Invalid { issues },
        });
    }

    let profile = select_profile(app, &app_yaml, &description)?;
    // v1 provider is compose (CLAUDE.md substrate rules); refuse
    // rather than emit a bundle the agent cannot converge.
    if profile.profile_type != reeve_types::margo::application::profile_type::COMPOSE {
        return Err(RenderError::UnsupportedProfileType {
            app: app.to_string(),
            profile_type: profile.profile_type.clone(),
        });
    }

    let parameters = resolve_parameters(tree, ctx, chain, app, &description)?;

    // deployment.yaml — wire-exact Margo ApplicationDeployment (D2):
    // the STATUS contract; status reports use its deploymentId and one
    // component entry per its components[] (Margo deployment-status.md).
    let application_id =
        description
            .effective_id()
            .ok_or_else(|| RenderError::Invalid {
                path: manifest_path.clone(),
                message: "application description has no id".to_string(),
            })?;
    let deployment = ApplicationDeployment {
        api_version: APPLICATION_DEPLOYMENT_API_VERSION.to_string(),
        kind: APPLICATION_DEPLOYMENT_KIND.to_string(),
        id: Some(deployment_id(&ctx.device_id, app).to_string()),
        metadata: DeploymentMetadata {
            name: app.to_string(),
            namespace: None,
            device_id: Some(ctx.device_id.clone()),
            annotations: None,
            labels: None,
        },
        spec: DeploymentSpec {
            application_id: application_id.to_string(),
            deployment_profile: DeploymentProfileSpec {
                profile_type: profile.profile_type.clone(),
                components: profile.components.clone(),
            },
            parameters: parameters.clone(),
        },
    };
    out.insert(
        format!("apps/{app}/deployment.yaml"),
        to_canonical_yaml(&deployment, &format!("apps/{app}/deployment.yaml"))?,
    );

    // application.yaml — the vendored package margo.yaml VERBATIM:
    // wire-exact by construction (D2; CLAUDE.md spec-fidelity rules).
    out.insert(
        format!("apps/{app}/application.yaml"),
        manifest_bytes.clone(),
    );

    // compose.yml — the rendered deployment artifact the agent
    // converges from (D2).
    let compose = render_compose(tree, ctx, app, &package_prefix, profile, &parameters)?;
    out.insert(format!("apps/{app}/compose.yml"), compose);

    // files/ — whole-file replace across the chain (D11: a file is a
    // scalar, not a mergeable map).
    for layer in chain {
        let prefix = format!("layers/{layer}/apps/{app}/files/");
        for (path, bytes) in with_prefix(tree, &prefix) {
            let rel = &path[prefix.len()..];
            let content = match std::str::from_utf8(bytes) {
                Ok(text) => substitute_registry(text, ctx).into_bytes(),
                Err(_) => bytes.clone(), // binary config: verbatim
            };
            out.insert(format!("apps/{app}/files/{rel}"), content);
        }
    }

    Ok(())
}

/// Profile selection (tree app.yaml `profile:` key, D11): by profile
/// id when given; otherwise the sole profile, or the sole
/// compose-typed profile.
fn select_profile<'d>(
    app: &str,
    app_yaml: &Value,
    description: &'d ApplicationDescription,
) -> Result<&'d DeploymentProfile, RenderError> {
    match app_yaml.get("profile") {
        Some(Value::String(wanted)) => description
            .deployment_profiles
            .iter()
            .find(|p| p.id.as_deref() == Some(wanted))
            .ok_or_else(|| RenderError::UnknownProfile {
                app: app.to_string(),
                profile: wanted.clone(),
            }),
        Some(_) => Err(RenderError::Invalid {
            path: format!("merged app.yaml for app `{app}`"),
            message: "`profile` must be a string (a deployment profile id)".to_string(),
        }),
        None => {
            if description.deployment_profiles.len() == 1 {
                return Ok(&description.deployment_profiles[0]);
            }
            let compose_type = reeve_types::margo::application::profile_type::COMPOSE;
            let mut compose_profiles = description
                .deployment_profiles
                .iter()
                .filter(|p| p.profile_type == compose_type);
            match (compose_profiles.next(), compose_profiles.next()) {
                (Some(single), None) => Ok(single),
                _ => Err(RenderError::AmbiguousProfile {
                    app: app.to_string(),
                }),
            }
        }
    }
}

/// Resolved parameters: the application description's declared
/// parameters (defaults + targets) with values overridden by the
/// merged params.yaml chain (D11). Secret values never appear — a
/// secret-typed value is the reference string `${secret:<name>}`
/// (spec/reeve/10-secrets.md §12.1), passed through untouched.
fn resolve_parameters(
    tree: &FileSet,
    ctx: &RenderContext,
    chain: &[String],
    app: &str,
    description: &ApplicationDescription,
) -> Result<BTreeMap<String, Parameter>, RenderError> {
    let mut parameters = description.parameters.clone();
    let merged = merge_chain_file(tree, chain, app, "params.yaml")?;
    if let Value::Mapping(map) = merged {
        for (key, value) in map {
            let Some(key) = key.as_str() else {
                return Err(RenderError::Invalid {
                    path: format!("merged params.yaml for app `{app}`"),
                    message: format!("parameter name {key:?} is not a string"),
                });
            };
            let Some(parameter) = parameters.get_mut(key) else {
                return Err(RenderError::UnknownParameter {
                    app: app.to_string(),
                    parameter: key.to_string(),
                });
            };
            parameter.value = Some(value);
        }
    }
    // ${REEVE_REGISTRY} resolves from the device-context input, never
    // from env (D3) — in parameter values too.
    for parameter in parameters.values_mut() {
        if let Some(value) = parameter.value.take() {
            parameter.value = Some(substitute_registry_value(value, ctx));
        }
    }
    Ok(parameters)
}

/// Replace the literal `${REEVE_REGISTRY}` with the tier registry
/// endpoint from the declared device context (D3, D8).
fn substitute_registry(text: &str, ctx: &RenderContext) -> String {
    text.replace("${REEVE_REGISTRY}", &ctx.registry_endpoint)
}

fn substitute_registry_value(value: Value, ctx: &RenderContext) -> Value {
    match value {
        Value::String(s) => Value::String(substitute_registry(&s, ctx)),
        Value::Sequence(seq) => Value::Sequence(
            seq.into_iter()
                .map(|v| substitute_registry_value(v, ctx))
                .collect(),
        ),
        Value::Mapping(map) => Value::Mapping(
            map.into_iter()
                .map(|(k, v)| (k, substitute_registry_value(v, ctx)))
                .collect(),
        ),
        other => other,
    }
}

/// True if any parameter target pointer is env-shaped (`ENV.X` /
/// `env.x` — both spellings appear in the pinned fixtures:
/// `nextcloud-compose` uses `ENV.MYSQL_DATABASE`,
/// `custom-otel-helm-app` uses `env.OTEL_EXPORTER_OTLP_ENDPOINT`).
fn has_env_parameters(parameters: &BTreeMap<String, Parameter>) -> bool {
    parameters.values().any(|p| {
        p.targets
            .iter()
            .any(|t| t.pointer.to_ascii_lowercase().starts_with("env."))
    })
}

/// Render the compose deployment artifact: vendored compose file +
/// `${REEVE_REGISTRY}` substitution + per-service `env_file`
/// references (spec/reeve/10-secrets.md §12.3: "the agent
/// materializes env PER SERVICE (apps/<name>/env/<service>.env ...);
/// rendered compose references them via env_file") + canonical emit.
fn render_compose(
    tree: &FileSet,
    ctx: &RenderContext,
    app: &str,
    package_prefix: &str,
    profile: &DeploymentProfile,
    parameters: &BTreeMap<String, Parameter>,
) -> Result<Vec<u8>, RenderError> {
    let compose_err = |message: String| RenderError::ComposeSource {
        app: app.to_string(),
        message,
    };

    // One app dir = one unit of convergence (D2): exactly one compose
    // component per profile in v1.
    let component = match profile.components.as_slice() {
        [single] => single,
        _ => {
            return Err(compose_err(format!(
                "expected exactly one component in the compose profile, found {}",
                profile.components.len()
            )));
        }
    };

    // The component's packageLocation must resolve INSIDE the vendored
    // package (v1: no fetch-at-render — D11); default to a
    // conventional compose.yml/compose.yaml at the package root.
    let location = component
        .properties
        .as_ref()
        .and_then(|p| p.get("packageLocation"))
        .and_then(Value::as_str);
    let compose_key = match location {
        Some(location) => {
            let rel = package_local_path(location).ok_or_else(|| {
                compose_err(format!(
                    "packageLocation `{location}` is not a package-local path \
                     (v1 packages are vendored — docs/decisions/tree-render.md D11)"
                ))
            })?;
            format!("{package_prefix}{rel}")
        }
        None => ["compose.yml", "compose.yaml"]
            .iter()
            .map(|f| format!("{package_prefix}{f}"))
            .find(|k| tree.contains_key(k))
            .ok_or_else(|| {
                compose_err("no packageLocation and no compose.yml in the package".to_string())
            })?,
    };
    let bytes = tree
        .get(&compose_key)
        .ok_or_else(|| compose_err(format!("compose file `{compose_key}` not in the revision")))?;
    let text = std::str::from_utf8(bytes)
        .map_err(|_| compose_err(format!("compose file `{compose_key}` is not valid UTF-8")))?;
    let text = substitute_registry(text, ctx);

    let mut compose: Value =
        serde_yaml_ng::from_str(&text).map_err(|source| RenderError::Yaml {
            path: compose_key.clone(),
            source,
        })?;

    if has_env_parameters(parameters) {
        inject_env_files(&mut compose).map_err(|m| compose_err(format!("{compose_key}: {m}")))?;
    }

    to_canonical_yaml(&compose, &format!("apps/{app}/compose.yml"))
}

/// Reference the agent-materialized per-service env file from every
/// service: `env_file: [env/<service>.env]`, path relative to the
/// app dir (`apps/<name>/env/<service>.env` is agent-local state
/// OUTSIDE the hashed bundle — D2, spec/reeve/10-secrets.md §12.3).
fn inject_env_files(compose: &mut Value) -> Result<(), String> {
    let Value::Mapping(root) = compose else {
        return Err("compose file is not a mapping".to_string());
    };
    let Some(services) = root.get_mut("services") else {
        return Err("compose file has no `services` mapping".to_string());
    };
    let Value::Mapping(services) = services else {
        return Err("compose `services` is not a mapping".to_string());
    };
    for (name, service) in services.iter_mut() {
        let Some(name) = name.as_str() else {
            return Err(format!("service name {name:?} is not a string"));
        };
        let Value::Mapping(service) = service else {
            return Err(format!("service `{name}` is not a mapping"));
        };
        let entry = Value::String(format!("env/{name}.env"));
        match service.get_mut("env_file") {
            None => {
                service.insert(
                    Value::String("env_file".to_string()),
                    Value::Sequence(vec![entry]),
                );
            }
            Some(Value::Sequence(seq)) => {
                if !seq.contains(&entry) {
                    seq.push(entry);
                }
            }
            Some(existing @ Value::String(_)) => {
                let prior = existing.clone();
                *existing = Value::Sequence(vec![prior, entry]);
            }
            Some(_) => {
                return Err(format!("service `{name}` has a non-list, non-string env_file"));
            }
        }
    }
    Ok(())
}

/// Normalize a package-relative location; `None` for URLs, absolute
/// paths, and anything that lexically escapes the package dir.
fn package_local_path(location: &str) -> Option<String> {
    if location.contains("://") || location.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for component in location.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            normal => parts.push(normal),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_local_paths() {
        assert_eq!(package_local_path("./compose.yml"), Some("compose.yml".into()));
        assert_eq!(
            package_local_path("resources/compose.yaml"),
            Some("resources/compose.yaml".into())
        );
        assert_eq!(package_local_path("a/../b"), Some("b".into()));
        assert_eq!(package_local_path("../escape"), None);
        assert_eq!(package_local_path("a/../../escape"), None);
        assert_eq!(package_local_path("/abs"), None);
        assert_eq!(package_local_path("https://example.com/c.yml"), None);
        assert_eq!(package_local_path("."), None);
    }
}
