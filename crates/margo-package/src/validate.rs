//! Semantic validation of a parsed [`ApplicationDescription`].
//!
//! The rules mirror the pinned Margo desired-state model:
//! `spec/margo/src/specification/applications/application-description.linkml.yaml`
//! (required fields, `equals_string`/`pattern` constraints, enums) —
//! cited per check below. The invalid examples under
//! `spec/margo/src/specification/applications/resources/examples/invalid/`
//! are the negative fixtures.
//!
//! Severity policy: constraints the linkml schema states for the
//! document itself are [`Severity::Error`]; findings where the pinned
//! *reference sandbox* artifacts legitimately diverge from the linkml
//! text (spec/margo wins on shape, but WIRE-EXACT says real artifacts
//! must not be rejected) are [`Severity::Warning`]. Concretely:
//! architecture values outside the linkml enum, and parameter targets
//! that name a deployment-profile id instead of a component name
//! (both shipped by `reference/poc/tests/artefacts/
//! custom-otel-helm-app/margo-package/margo.yaml`).

use std::collections::{BTreeMap, BTreeSet};

use reeve_types::margo::application::{
    APPLICATION_DESCRIPTION_API_VERSION, APPLICATION_DESCRIPTION_KIND, ApplicationDescription,
    Configuration, DeploymentProfile, profile_type,
};
use serde::Serialize;
use serde_yaml_ng::Value;

/// How bad a validation finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The document violates a spec-required constraint; the package
    /// must not be used.
    Error,
    /// Suspicious but tolerated (see module docs on severity policy).
    Warning,
}

/// One validation finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationIssue {
    pub severity: Severity,
    /// Dotted path to the offending element, e.g.
    /// `deploymentProfiles[0].components[1].name`.
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{sev}: {}: {}", self.path, self.message)
    }
}

/// Accepted `configuration.schema[].dataType` values
/// (`application-description.linkml.yaml`, `Schema.dataType`).
pub const DATA_TYPES: &[&str] = &[
    "string",
    "integer",
    "double",
    "boolean",
    "array[string]",
    "array[integer]",
    "array[double]",
    "array[boolean]",
];

/// `CpuArchitectureType` permissible values
/// (`application-description.linkml.yaml`, enums).
pub const CPU_ARCHITECTURES: &[&str] = &["amd64", "arm64", "arm", "riscv64", "other"];

/// `PeripheralType` permissible values
/// (`application-description.linkml.yaml`, enums).
pub const PERIPHERAL_TYPES: &[&str] =
    &["gpu", "display", "camera", "microphone", "speaker", "other"];

/// `CommunicationInterfaceType` permissible values
/// (`application-description.linkml.yaml`, enums).
pub const INTERFACE_TYPES: &[&str] = &[
    "ethernet",
    "wifi",
    "cellular",
    "bluetooth",
    "usb",
    "canbus",
    "rs232",
    "other",
];

/// Deployment profile types accepted by this crate:
/// `helm` and `compose` per the linkml `type` slot pattern
/// `^(helm|compose)$`, plus `helm.v3`, which the pinned reference
/// sandbox ships as a live artifact
/// (`custom-otel-helm-app/margo-package/margo.yaml`;
/// `reeve_types::margo::application::profile_type`).
pub const PROFILE_TYPES: &[&str] = &[
    profile_type::HELM,
    profile_type::HELM_V3,
    profile_type::COMPOSE,
];

/// Validate a parsed description against the pinned desired-state
/// model. Returns every finding; the document is usable iff no
/// finding has [`Severity::Error`].
pub fn validate_description(desc: &ApplicationDescription) -> Vec<ValidationIssue> {
    let mut v = Validator::default();

    v.check_header(desc);
    v.check_metadata(desc);
    let (component_names, profile_ids) = v.check_profiles(&desc.deployment_profiles);
    v.check_parameters(desc, &component_names, &profile_ids);
    if let Some(cfg) = &desc.configuration {
        v.check_configuration(desc, cfg);
    }

    v.issues
}

/// True iff `issues` contains at least one [`Severity::Error`].
pub fn has_errors(issues: &[ValidationIssue]) -> bool {
    issues.iter().any(|i| i.severity == Severity::Error)
}

#[derive(Default)]
struct Validator {
    issues: Vec<ValidationIssue>,
}

impl Validator {
    fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            severity: Severity::Error,
            path: path.into(),
            message: message.into(),
        });
    }

    fn warning(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            severity: Severity::Warning,
            path: path.into(),
            message: message.into(),
        });
    }

    /// `apiVersion` (required), `kind` (`equals_string:
    /// "ApplicationDescription"`), `id` (required, pattern
    /// `^[a-z0-9-]{1,200}$`) — linkml `ApplicationDescription`
    /// attributes. Negative fixtures: `invalid/
    /// ApplicationDescription-001.yaml` (kind), `-002.yaml` (id).
    fn check_header(&mut self, desc: &ApplicationDescription) {
        if desc.api_version.is_empty() {
            self.error("apiVersion", "apiVersion is required");
        } else if desc.api_version != APPLICATION_DESCRIPTION_API_VERSION {
            self.warning(
                "apiVersion",
                format!(
                    "unexpected apiVersion `{}` (pinned spec examples use `{}`)",
                    desc.api_version, APPLICATION_DESCRIPTION_API_VERSION
                ),
            );
        }

        if desc.kind != APPLICATION_DESCRIPTION_KIND {
            self.error(
                "kind",
                format!(
                    "kind must be `{APPLICATION_DESCRIPTION_KIND}`, got `{}`",
                    desc.kind
                ),
            );
        }

        match desc.effective_id() {
            None => self.error("id", "id is required (top-level `id` or `metadata.id`)"),
            Some(id) => {
                if !is_valid_id(id) {
                    self.error(
                        "id",
                        format!(
                            "id `{id}` must match ^[a-z0-9-]{{1,200}}$ (lower case letters, \
                             numbers and dashes only, at most 200 characters)"
                        ),
                    );
                }
                if let (Some(top), Some(meta)) = (&desc.id, &desc.metadata.id)
                    && top != meta
                {
                    self.warning(
                        "metadata.id",
                        format!("`id` (`{top}`) and `metadata.id` (`{meta}`) disagree"),
                    );
                }
            }
        }
    }

    /// `Metadata`: `name`, `version`, `catalog` required;
    /// `Catalog.organization` required; `Organization.name` required
    /// — linkml `Metadata`/`Catalog`/`Organization` classes.
    fn check_metadata(&mut self, desc: &ApplicationDescription) {
        if desc.metadata.name.is_empty() {
            self.error("metadata.name", "metadata.name is required");
        }
        if desc.metadata.version.as_deref().unwrap_or("").is_empty() {
            self.error("metadata.version", "metadata.version is required");
        }
        match &desc.metadata.catalog {
            None => self.error("metadata.catalog", "metadata.catalog is required"),
            Some(catalog) => {
                if catalog.organization.is_empty() {
                    self.error(
                        "metadata.catalog.organization",
                        "at least one organization entry is required",
                    );
                }
                for (i, org) in catalog.organization.iter().enumerate() {
                    if org.name.as_deref().unwrap_or("").is_empty() {
                        self.error(
                            format!("metadata.catalog.organization[{i}].name"),
                            "organization name is required",
                        );
                    }
                }
            }
        }
    }

    /// `deploymentProfiles` required and non-empty; per profile:
    /// `id` required (unique within the description), `type` in
    /// [`PROFILE_TYPES`], `components` required and non-empty
    /// (linkml `DeploymentProfile` + `type`/`components` slots).
    /// Returns (component names, profile ids) for cross-reference
    /// checks.
    fn check_profiles(
        &mut self,
        profiles: &[DeploymentProfile],
    ) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut component_names = BTreeSet::new();
        let mut profile_ids = BTreeSet::new();

        if profiles.is_empty() {
            self.error(
                "deploymentProfiles",
                "at least one deployment profile is required",
            );
        }

        for (pi, profile) in profiles.iter().enumerate() {
            let base = format!("deploymentProfiles[{pi}]");

            match profile.id.as_deref() {
                None | Some("") => self.error(format!("{base}.id"), "profile id is required"),
                Some(id) => {
                    if !profile_ids.insert(id.to_string()) {
                        self.error(format!("{base}.id"), format!("duplicate profile id `{id}`"));
                    }
                }
            }

            if !PROFILE_TYPES.contains(&profile.profile_type.as_str()) {
                self.error(
                    format!("{base}.type"),
                    format!(
                        "unknown deployment profile type `{}` (expected one of {})",
                        profile.profile_type,
                        PROFILE_TYPES.join(", ")
                    ),
                );
            }

            if profile.components.is_empty() {
                self.error(
                    format!("{base}.components"),
                    "at least one component is required",
                );
            }

            let mut names_in_profile = BTreeSet::new();
            for (ci, component) in profile.components.iter().enumerate() {
                let cbase = format!("{base}.components[{ci}]");

                // linkml `Component.name`: "must be lower case letters
                // and numbers and MAY contain dashes".
                if component.name.is_empty() || !component.name.chars().all(is_id_char) {
                    self.error(
                        format!("{cbase}.name"),
                        format!(
                            "component name `{}` must be lower case letters, numbers and dashes",
                            component.name
                        ),
                    );
                }
                if !names_in_profile.insert(component.name.clone()) {
                    self.error(
                        format!("{cbase}.name"),
                        format!("duplicate component name `{}` within profile", component.name),
                    );
                }
                component_names.insert(component.name.clone());

                // linkml `Component.properties`: required, a dictionary.
                match &component.properties {
                    None => self.error(
                        format!("{cbase}.properties"),
                        "component properties are required",
                    ),
                    Some(props) if !props.is_mapping() => self.error(
                        format!("{cbase}.properties"),
                        "component properties must be a mapping",
                    ),
                    Some(_) => {}
                }
            }

            self.check_required_resources(profile, &base);
        }

        (component_names, profile_ids)
    }

    /// `RequiredResources`: `cpu.cores` positive double; `memory`
    /// pattern `^[0-9]+(Mi|Gi|Ki)$`; `storage` pattern
    /// `^[0-9]+(Mi|Gi|Ki|Ti|Pi|Ei)$`; peripheral/interface/
    /// architecture enums (linkml `RequiredResources`/`CPU`/
    /// `Peripheral`/`CommunicationInterface` + enums).
    fn check_required_resources(&mut self, profile: &DeploymentProfile, base: &str) {
        let Some(rr) = &profile.required_resources else {
            return;
        };
        let rbase = format!("{base}.requiredResources");

        if let Some(cpu) = &rr.cpu {
            if !cpu.cores.is_finite() || cpu.cores <= 0.0 {
                self.error(
                    format!("{rbase}.cpu.cores"),
                    format!("cpu.cores must be a positive number, got {}", cpu.cores),
                );
            }
            for (ai, arch) in cpu.architectures.iter().flatten().enumerate() {
                if !CPU_ARCHITECTURES.contains(&arch.as_str()) {
                    // Warning, not error: the reference sandbox ships
                    // `x86_64` (custom-otel-helm-app/margo.yaml).
                    self.warning(
                        format!("{rbase}.cpu.architectures[{ai}]"),
                        format!(
                            "`{arch}` is not a CpuArchitectureType ({})",
                            CPU_ARCHITECTURES.join(", ")
                        ),
                    );
                }
            }
        }

        if let Some(memory) = &rr.memory
            && !is_binary_quantity(memory, &["Mi", "Gi", "Ki"])
        {
            self.error(
                format!("{rbase}.memory"),
                format!("memory `{memory}` must match ^[0-9]+(Mi|Gi|Ki)$"),
            );
        }
        if let Some(storage) = &rr.storage
            && !is_binary_quantity(storage, &["Mi", "Gi", "Ki", "Ti", "Pi", "Ei"])
        {
            self.error(
                format!("{rbase}.storage"),
                format!("storage `{storage}` must match ^[0-9]+(Mi|Gi|Ki|Ti|Pi|Ei)$"),
            );
        }

        for (i, p) in rr.peripherals.iter().flatten().enumerate() {
            if !PERIPHERAL_TYPES.contains(&p.peripheral_type.as_str()) {
                self.warning(
                    format!("{rbase}.peripherals[{i}].type"),
                    format!(
                        "`{}` is not a PeripheralType ({})",
                        p.peripheral_type,
                        PERIPHERAL_TYPES.join(", ")
                    ),
                );
            }
        }
        for (i, iface) in rr.interfaces.iter().flatten().enumerate() {
            if !INTERFACE_TYPES.contains(&iface.interface_type.as_str()) {
                self.warning(
                    format!("{rbase}.interfaces[{i}].type"),
                    format!(
                        "`{}` is not a CommunicationInterfaceType ({})",
                        iface.interface_type,
                        INTERFACE_TYPES.join(", ")
                    ),
                );
            }
        }
    }

    /// `Parameter.targets` required and non-empty; `Target.pointer`
    /// and `Target.components` required; each target component "MUST
    /// match a component name in the deployment profiles section"
    /// (linkml `Parameter`/`Target`). A target naming a *profile id*
    /// is downgraded to a warning — the reference sandbox does this
    /// (custom-otel-helm-app targets `otel-demo`, its profile id).
    fn check_parameters(
        &mut self,
        desc: &ApplicationDescription,
        component_names: &BTreeSet<String>,
        profile_ids: &BTreeSet<String>,
    ) {
        for (name, param) in &desc.parameters {
            let base = format!("parameters.{name}");
            if param.targets.is_empty() {
                self.error(format!("{base}.targets"), "at least one target is required");
            }
            for (ti, target) in param.targets.iter().enumerate() {
                let tbase = format!("{base}.targets[{ti}]");
                if target.pointer.is_empty() {
                    self.error(format!("{tbase}.pointer"), "target pointer is required");
                }
                if target.components.is_empty() {
                    self.error(
                        format!("{tbase}.components"),
                        "at least one target component is required",
                    );
                }
                for comp in &target.components {
                    if component_names.contains(comp) {
                        continue;
                    }
                    if profile_ids.contains(comp) {
                        self.warning(
                            format!("{tbase}.components"),
                            format!(
                                "`{comp}` names a deployment profile id, not a component name \
                                 (the spec requires a component name)"
                            ),
                        );
                    } else {
                        self.error(
                            format!("{tbase}.components"),
                            format!("`{comp}` does not match any deployment profile component"),
                        );
                    }
                }
            }
        }
    }

    /// `Configuration.sections`/`Configuration.schema` required and
    /// non-empty; `Setting.parameter`/`Setting.schema` required and
    /// must resolve; `Schema.name` unique; `Schema.dataType` required
    /// and one of [`DATA_TYPES`] (linkml `Configuration`/`Section`/
    /// `Setting`/`Schema`). Parameter default values are checked
    /// against their schema's dataType only (warning on mismatch);
    /// range/regex rules apply to user-provided values at
    /// configuration time, not to defaults.
    fn check_configuration(&mut self, desc: &ApplicationDescription, cfg: &Configuration) {
        if cfg.sections.is_empty() {
            self.error(
                "configuration.sections",
                "at least one section is required",
            );
        }
        if cfg.schema.is_empty() {
            self.error(
                "configuration.schema",
                "at least one schema rule is required",
            );
        }

        let mut rules: BTreeMap<&str, Option<&str>> = BTreeMap::new();
        for (ri, rule) in cfg.schema.iter().enumerate() {
            let rbase = format!("configuration.schema[{ri}]");
            if rule.name.is_empty() {
                self.error(format!("{rbase}.name"), "schema rule name is required");
            } else if rules
                .insert(rule.name.as_str(), rule.data_type.as_deref())
                .is_some()
            {
                self.error(
                    format!("{rbase}.name"),
                    format!("duplicate schema rule name `{}`", rule.name),
                );
            }
            match rule.data_type.as_deref() {
                None | Some("") => self.error(
                    format!("{rbase}.dataType"),
                    "schema rule dataType is required",
                ),
                Some(dt) if !DATA_TYPES.contains(&dt) => self.error(
                    format!("{rbase}.dataType"),
                    format!("unknown dataType `{dt}` (expected one of {})", DATA_TYPES.join(", ")),
                ),
                Some(_) => {}
            }
        }

        for (si, section) in cfg.sections.iter().enumerate() {
            let sbase = format!("configuration.sections[{si}]");
            if section.name.is_empty() {
                self.error(format!("{sbase}.name"), "section name is required");
            }
            if section.settings.is_empty() {
                self.error(
                    format!("{sbase}.settings"),
                    "at least one setting is required",
                );
            }
            for (wi, setting) in section.settings.iter().enumerate() {
                let wbase = format!("{sbase}.settings[{wi}]");
                if setting.parameter.is_empty() {
                    self.error(
                        format!("{wbase}.parameter"),
                        "setting parameter is required",
                    );
                } else if !desc.parameters.contains_key(&setting.parameter) {
                    self.error(
                        format!("{wbase}.parameter"),
                        format!("setting references undefined parameter `{}`", setting.parameter),
                    );
                }
                match setting.schema.as_deref() {
                    None | Some("") => {
                        self.error(format!("{wbase}.schema"), "setting schema is required");
                    }
                    Some(schema_name) => match rules.get(schema_name) {
                        None => self.error(
                            format!("{wbase}.schema"),
                            format!("setting references undefined schema rule `{schema_name}`"),
                        ),
                        Some(Some(data_type)) => {
                            if let Some(param) = desc.parameters.get(&setting.parameter)
                                && let Some(value) = &param.value
                                && !value_matches_data_type(value, data_type)
                            {
                                self.warning(
                                    format!("parameters.{}.value", setting.parameter),
                                    format!(
                                        "default value does not match schema `{schema_name}` \
                                         dataType `{data_type}`"
                                    ),
                                );
                            }
                        }
                        Some(None) => {} // dataType error already reported
                    },
                }
            }
        }
    }
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

/// linkml `ApplicationDescription.id` pattern `^[a-z0-9-]{1,200}$`.
fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 200 && id.chars().all(is_id_char)
}

/// linkml `RequiredResources.memory`/`storage` patterns:
/// one or more digits followed by a binary unit suffix.
fn is_binary_quantity(s: &str, units: &[&str]) -> bool {
    units.iter().any(|unit| {
        s.strip_suffix(unit)
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Minimal dataType check for parameter *default* values (linkml
/// `Schema.dataType`: "At a minimum, the provided parameter value
/// MUST match the schema's data type"). Integers are accepted where
/// a double is expected (YAML `1` is a valid double).
fn value_matches_data_type(value: &Value, data_type: &str) -> bool {
    match data_type {
        "string" => value.is_string(),
        "boolean" => matches!(value, Value::Bool(_)),
        "integer" => matches!(value, Value::Number(n) if n.is_i64() || n.is_u64()),
        "double" => matches!(value, Value::Number(_)),
        "array[string]" => is_seq_of(value, |v| v.is_string()),
        "array[boolean]" => is_seq_of(value, |v| matches!(v, Value::Bool(_))),
        "array[integer]" => is_seq_of(
            value,
            |v| matches!(v, Value::Number(n) if n.is_i64() || n.is_u64()),
        ),
        "array[double]" => is_seq_of(value, |v| matches!(v, Value::Number(_))),
        // Unknown dataType is reported separately; don't double-report.
        _ => true,
    }
}

fn is_seq_of(value: &Value, pred: impl Fn(&Value) -> bool) -> bool {
    matches!(value, Value::Sequence(items) if items.iter().all(pred))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_pattern() {
        assert!(is_valid_id("com-northstartida-hello-world"));
        assert!(is_valid_id("a"));
        assert!(is_valid_id(&"a".repeat(200)));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id(&"a".repeat(201)));
        assert!(!is_valid_id("an-incorrect-id-with-special-chars**&%"));
        assert!(!is_valid_id("Uppercase"));
        assert!(!is_valid_id("under_score"));
        assert!(!is_valid_id("dotted.id"));
    }

    #[test]
    fn binary_quantities() {
        assert!(is_binary_quantity("1024Mi", &["Mi", "Gi", "Ki"]));
        assert!(is_binary_quantity("10Gi", &["Mi", "Gi", "Ki"]));
        assert!(!is_binary_quantity("10G", &["Mi", "Gi", "Ki"]));
        assert!(!is_binary_quantity("Mi", &["Mi", "Gi", "Ki"]));
        assert!(!is_binary_quantity("1.5Gi", &["Mi", "Gi", "Ki"]));
        assert!(is_binary_quantity("10Ti", &["Mi", "Gi", "Ki", "Ti", "Pi", "Ei"]));
    }

    #[test]
    fn value_data_types() {
        let s = Value::String("x".into());
        let i = Value::Number(30.into());
        let d = serde_yaml_ng::from_str::<Value>("1.5").unwrap();
        let b = Value::Bool(true);
        assert!(value_matches_data_type(&s, "string"));
        assert!(!value_matches_data_type(&i, "string"));
        assert!(value_matches_data_type(&i, "integer"));
        assert!(value_matches_data_type(&i, "double")); // int widens
        assert!(value_matches_data_type(&d, "double"));
        assert!(!value_matches_data_type(&d, "integer"));
        assert!(value_matches_data_type(&b, "boolean"));
        let seq = serde_yaml_ng::from_str::<Value>("[1, 2]").unwrap();
        assert!(value_matches_data_type(&seq, "array[integer]"));
        assert!(!value_matches_data_type(&seq, "array[string]"));
    }
}
