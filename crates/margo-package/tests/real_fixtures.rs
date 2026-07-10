//! Round-trip and validation tests against the REAL pinned fixtures
//! (CLAUDE.md "Spec fidelity": the YAML in `spec/margo/` and
//! `reference/` are the test fixtures — never approximations).

use std::path::{Path, PathBuf};

use margo_package::{
    Package, PackageError, PackageSource, Severity, has_errors, parse_description,
    validate_description,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn spec_examples() -> PathBuf {
    let dir = repo_root().join("spec/margo/src/specification/applications/resources/examples");
    assert!(
        dir.is_dir(),
        "spec/margo submodule missing — run `git submodule update --init --recursive`"
    );
    dir
}

fn reference_package(name: &str) -> PathBuf {
    let dir = repo_root().join(format!("reference/poc/tests/artefacts/{name}/margo-package"));
    assert!(
        dir.is_dir(),
        "reference submodule missing — run `git submodule update --init --recursive`"
    );
    dir
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn valid_spec_examples_validate_clean() {
    for name in ["ApplicationDescription-001.yaml", "ApplicationDescription-002.yaml"] {
        let path = spec_examples().join("valid").join(name);
        let desc = parse_description(&read(&path)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let issues = validate_description(&desc);
        assert!(
            !has_errors(&issues),
            "{name} should have no validation errors, got: {issues:#?}"
        );
        assert!(desc.effective_id().is_some(), "{name}: missing id");
    }
}

#[test]
fn valid_spec_examples_round_trip() {
    for name in ["ApplicationDescription-001.yaml", "ApplicationDescription-002.yaml"] {
        let path = spec_examples().join("valid").join(name);
        let first = parse_description(&read(&path)).unwrap();
        let reserialized = serde_yaml_ng::to_string(&first).unwrap();
        let second = parse_description(&reserialized).unwrap();
        assert_eq!(first, second, "{name}: round-trip changed the document");
    }
}

/// `invalid/ApplicationDescription-001.yaml`: "Demonstrates
/// validation of kind field" (`kind: SomethingErroneous`).
#[test]
fn invalid_kind_is_an_error() {
    let path = spec_examples().join("invalid/ApplicationDescription-001.yaml");
    // Structurally it still parses (WIRE-EXACT tolerant shape);
    // invalidity is semantic.
    let desc = parse_description(&read(&path)).unwrap();
    let issues = validate_description(&desc);
    assert!(
        issues
            .iter()
            .any(|i| i.severity == Severity::Error && i.path == "kind"),
        "expected a kind error, got: {issues:#?}"
    );
}

/// `invalid/ApplicationDescription-002.yaml`: "Demonstrates
/// validation of id field" (special characters in `id`).
#[test]
fn invalid_id_is_an_error() {
    let path = spec_examples().join("invalid/ApplicationDescription-002.yaml");
    let desc = parse_description(&read(&path)).unwrap();
    let issues = validate_description(&desc);
    assert!(
        issues
            .iter()
            .any(|i| i.severity == Severity::Error && i.path == "id"),
        "expected an id error, got: {issues:#?}"
    );
    // The kind is correct in this fixture, so no kind error.
    assert!(
        !issues
            .iter()
            .any(|i| i.severity == Severity::Error && i.path == "kind"),
        "unexpected kind error: {issues:#?}"
    );
}

#[test]
fn nextcloud_reference_package_loads_clean() {
    let pkg = Package::load_dir(reference_package("nextcloud-compose")).unwrap();
    assert_eq!(pkg.description.effective_id(), Some("nextcloud-stack"));
    assert_eq!(pkg.description.metadata.name, "Nextcloud Stack");
    assert_eq!(pkg.description.deployment_profiles.len(), 1);
    assert_eq!(pkg.description.deployment_profiles[0].profile_type, "compose");
    assert!(
        pkg.warnings.is_empty(),
        "nextcloud package should be warning-free, got: {:#?}",
        pkg.warnings
    );
}

/// The otel reference package is a live wire artifact that diverges
/// from the linkml text in known ways; it must LOAD (no errors) but
/// surface the divergences as warnings.
#[test]
fn otel_reference_package_loads_with_expected_warnings() {
    let pkg = Package::load_dir(reference_package("custom-otel-helm-app")).unwrap();
    assert_eq!(pkg.description.effective_id(), Some("com-go-otel-service"));
    assert_eq!(pkg.description.deployment_profiles[0].profile_type, "helm.v3");

    let warn_paths: Vec<&str> = pkg.warnings.iter().map(|w| w.path.as_str()).collect();
    // targets name the profile id `otel-demo`, not component `otel-app`
    assert!(
        warn_paths
            .iter()
            .any(|p| p.starts_with("parameters.otlpEndpoint.targets")),
        "expected profile-id target warning, got: {:#?}",
        pkg.warnings
    );
    // architectures include `x86_64`, not a CpuArchitectureType
    assert!(
        warn_paths.iter().any(|p| p.contains("cpu.architectures")),
        "expected architecture warning, got: {:#?}",
        pkg.warnings
    );
    // links `./resources/license.pdf`; the dir ships `license.txt`
    assert!(
        warn_paths.contains(&"metadata.catalog.application.licenseFile"),
        "expected dangling licenseFile warning, got: {:#?}",
        pkg.warnings
    );
}

/// Discovery sweep: every ApplicationDescription YAML in the pinned
/// submodules parses. Finds files by the fixture naming conventions
/// (`margo.yaml` package manifests, `ApplicationDescription-*.yaml`
/// spec examples).
#[test]
fn every_pinned_application_description_parses() {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name == "margo.yaml"
                    || (name.starts_with("ApplicationDescription-") && name.ends_with(".yaml")))
            {
                found.push(path);
            }
        }
    }

    let mut found = Vec::new();
    walk(&repo_root().join("spec/margo/src"), &mut found);
    walk(&repo_root().join("reference/poc"), &mut found);
    assert!(
        found.len() >= 6,
        "expected at least the 4 spec examples + 2 reference packages, found: {found:#?}"
    );
    for path in &found {
        parse_description(&read(path))
            .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));
    }
}

#[test]
fn dir_source_string_loads_a_package() {
    let dir = reference_package("nextcloud-compose");
    let source = PackageSource::parse(&format!("dir://{}", dir.display())).unwrap();
    let pkg = Package::load(&source).unwrap();
    assert_eq!(pkg.description.effective_id(), Some("nextcloud-stack"));
}

#[test]
fn oci_source_is_a_typed_unsupported_stub() {
    let source = PackageSource::parse("oci://northstarida.azurecr.io/charts/hello-world:1.0.1")
        .unwrap();
    match Package::load(&source) {
        Err(PackageError::UnsupportedSource(msg)) => {
            assert!(msg.contains("oci://northstarida.azurecr.io/charts/hello-world:1.0.1"));
        }
        other => panic!("expected UnsupportedSource, got {other:?}"),
    }
}

#[test]
fn missing_manifest_dir_is_a_typed_error() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("empty-package");
    std::fs::create_dir_all(&dir).unwrap();
    match Package::load_dir(&dir) {
        Err(PackageError::ManifestNotFound(p)) => assert_eq!(p, dir),
        other => panic!("expected ManifestNotFound, got {other:?}"),
    }
}

#[test]
fn error_severity_findings_fail_the_load() {
    // The invalid spec example (bad kind) staged as a package dir.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("invalid-kind-package");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        spec_examples().join("invalid/ApplicationDescription-001.yaml"),
        dir.join("margo.yaml"),
    )
    .unwrap();
    match Package::load_dir(&dir) {
        Err(PackageError::Invalid { issues }) => {
            assert!(issues.iter().any(|i| i.severity == Severity::Error && i.path == "kind"));
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}
