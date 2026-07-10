//! Table tests — these ARE the spec for this crate
//! (docs/decisions/tree-render.md D3: "these ARE the desired-state
//! tests"; D11: "revision content fixture + device context in ->
//! rendered file set out, byte-exact").
//!
//! Every test builds an in-memory tree (path -> bytes, the revision
//! content shape of D11), renders it for a device context (D3), and
//! asserts on the rendered file set (D2 layout).

use desired_state::{
    FileSet, REEVE_UUID_NAMESPACE, RenderContext, RenderError, deployment_id, render,
};
use uuid::Uuid;

// ---------------------------------------------------------------- helpers

/// Build a tree (revision content) from `(path, contents)` pairs.
fn tree(entries: &[(&str, &str)]) -> FileSet {
    entries
        .iter()
        .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
        .collect()
}

/// Device context with the standard test fixture values.
fn ctx(device_id: &str, layers: &[&str]) -> RenderContext {
    RenderContext {
        device_id: device_id.to_string(),
        layers: layers.iter().map(|l| l.to_string()).collect(),
        registry_endpoint: "registry.example:5000".to_string(),
        generation: 1,
        local_revision: 42,
        hub_revision: None,
    }
}

/// A valid vendored compose package (passes margo-package validation:
/// id pattern, metadata.version/catalog/organization, component name,
/// parameter targets naming a profile component — all per
/// spec/margo application-description.linkml.yaml).
const PKG_MANIFEST: &str = "\
apiVersion: margo.org/v1-alpha1
kind: ApplicationDescription
metadata:
  id: web
  name: Web
  version: 1.0.0
  catalog:
    organization:
      - name: Reeve Tests
        site: https://example.com
deploymentProfiles:
  - type: compose
    id: web-compose
    components:
      - name: web-stack
        properties:
          packageLocation: ./compose.yml
parameters:
  greeting:
    value: hello
    targets:
      - pointer: ENV.GREETING
        components: [\"web-stack\"]
";

const PKG_COMPOSE: &str = "\
services:
  web:
    image: ${REEVE_REGISTRY}/nginx:1.25
";

/// The vendored package under `packages/<name>/<version>/` (D11).
fn package_entries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("packages/web/1.0.0/margo.yaml", PKG_MANIFEST),
        ("packages/web/1.0.0/compose.yml", PKG_COMPOSE),
    ]
}

/// Fleet layer defining the app: package ref only (enabled defaults
/// to true — an app defined by any layer is desired unless switched
/// off, D11).
const FLEET_APP: &str = "\
package:
  name: web
  version: 1.0.0
";

fn base_tree() -> FileSet {
    let mut entries = package_entries();
    entries.push(("layers/00-fleet/apps/web/app.yaml", FLEET_APP));
    tree(&entries)
}

fn utf8(fs: &FileSet, path: &str) -> String {
    String::from_utf8(
        fs.get(path)
            .unwrap_or_else(|| panic!("missing rendered file {path}; have: {:?}", fs.keys()))
            .clone(),
    )
    .unwrap()
}

// ---------------------------------------------------------------- fixtures

/// Empty tree -> just manifest.yaml, exact bytes (canonical emitter:
/// keys sorted, block style, LF, trailing newline — D3).
#[test]
fn empty_tree_renders_manifest_only() {
    let out = render(
        &FileSet::new(),
        &ctx("dev-1", &["00-fleet", "30-device.dev-1"]),
    )
    .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(
        utf8(&out, "manifest.yaml"),
        "\
deviceId: dev-1
generation: 1
registryEndpoint: registry.example:5000
revisions:
  local: 42
"
    );
}

/// Single device, single fleet app: the full D2 layout, byte-exact.
#[test]
fn single_device_full_layout() {
    let out = render(&base_tree(), &ctx("dev-1", &["00-fleet", "30-device.dev-1"])).unwrap();

    let expected_id = deployment_id("dev-1", "web");
    // deployment.yaml: wire-exact ApplicationDeployment (D2), canonical
    // key order, deterministic deploymentId.
    assert_eq!(
        utf8(&out, "apps/web/deployment.yaml"),
        format!(
            "\
apiVersion: application.margo.org/v1alpha1
id: {expected_id}
kind: ApplicationDeployment
metadata:
  deviceId: dev-1
  name: web
spec:
  applicationId: web
  deploymentProfile:
    components:
    - name: web-stack
      properties:
        packageLocation: ./compose.yml
    type: compose
  parameters:
    greeting:
      targets:
      - components:
        - web-stack
        pointer: ENV.GREETING
      value: hello
"
        )
    );

    // application.yaml: the vendored package margo.yaml VERBATIM
    // (wire-exact by construction).
    assert_eq!(utf8(&out, "apps/web/application.yaml"), PKG_MANIFEST);

    // compose.yml: ${REEVE_REGISTRY} resolved from device context (D8),
    // env_file per service injected because the app has env-targeted
    // parameters (spec/reeve/10-secrets.md §12.3: "rendered compose
    // references them via env_file").
    assert_eq!(
        utf8(&out, "apps/web/compose.yml"),
        "\
services:
  web:
    env_file:
    - env/web.env
    image: registry.example:5000/nginx:1.25
"
    );

    assert_eq!(
        utf8(&out, "manifest.yaml"),
        "\
deviceId: dev-1
generation: 1
registryEndpoint: registry.example:5000
revisions:
  local: 42
"
    );

    assert_eq!(out.len(), 4, "no extra files: {:?}", out.keys());
}

/// Scalar override precedence: later layer wins (D3), across
/// fleet -> site -> device.
#[test]
fn override_precedence_scalar() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/params.yaml".into(),
        b"greeting: from-fleet\n".to_vec(),
    );
    t.insert(
        "layers/20-site.a/apps/web/params.yaml".into(),
        b"greeting: from-site\n".to_vec(),
    );
    t.insert(
        "layers/30-device.dev-1/apps/web/params.yaml".into(),
        b"greeting: from-device\n".to_vec(),
    );
    let out = render(
        &t,
        &ctx("dev-1", &["00-fleet", "20-site.a", "30-device.dev-1"]),
    )
    .unwrap();
    assert!(utf8(&out, "apps/web/deployment.yaml").contains("value: from-device\n"));
}

/// Layer order comes from the numeric prefix, not caller order: a
/// shuffled chain renders identically (D11: "ONLY the numeric prefix
/// orders the merge").
#[test]
fn layer_order_is_numeric_prefix_not_caller_order() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/params.yaml".into(),
        b"greeting: from-fleet\n".to_vec(),
    );
    t.insert(
        "layers/30-device.dev-1/apps/web/params.yaml".into(),
        b"greeting: from-device\n".to_vec(),
    );
    let sorted = render(&t, &ctx("dev-1", &["00-fleet", "30-device.dev-1"])).unwrap();
    let shuffled = render(&t, &ctx("dev-1", &["30-device.dev-1", "00-fleet"])).unwrap();
    assert_eq!(sorted, shuffled);
    assert!(utf8(&sorted, "apps/web/deployment.yaml").contains("value: from-device\n"));
}

/// Lists REPLACE, never append or merge-by-index (D3).
#[test]
fn list_replace() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/params.yaml".into(),
        b"greeting:\n- a\n- b\n- c\n".to_vec(),
    );
    t.insert(
        "layers/30-device.dev-1/apps/web/params.yaml".into(),
        b"greeting:\n- z\n".to_vec(),
    );
    let out = render(&t, &ctx("dev-1", &["00-fleet", "30-device.dev-1"])).unwrap();
    let dep = utf8(&out, "apps/web/deployment.yaml");
    assert!(dep.contains("- z\n"), "list not replaced:\n{dep}");
    assert!(!dep.contains("- a"), "base list leaked into merge:\n{dep}");
}

/// Explicit `null` deletes the key (D3): a device null on a param set
/// by fleet reverts it to the package default.
#[test]
fn null_delete_reverts_to_package_default() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/params.yaml".into(),
        b"greeting: from-fleet\n".to_vec(),
    );
    t.insert(
        "layers/30-device.dev-1/apps/web/params.yaml".into(),
        b"greeting: null\n".to_vec(),
    );
    let out = render(&t, &ctx("dev-1", &["00-fleet", "30-device.dev-1"])).unwrap();
    // Package default from margo.yaml `parameters.greeting.value: hello`.
    assert!(utf8(&out, "apps/web/deployment.yaml").contains("value: hello\n"));
}

/// Maps deep-merge key by key (D3): a device app.yaml overriding only
/// `package.version` keeps `package.name` from fleet.
#[test]
fn maps_deep_merge() {
    let mut t = base_tree();
    t.insert(
        "layers/30-device.dev-1/apps/web/app.yaml".into(),
        b"package:\n  version: 1.0.0\n".to_vec(),
    );
    let out = render(&t, &ctx("dev-1", &["00-fleet", "30-device.dev-1"])).unwrap();
    // Renders at all => package.name survived the partial override.
    assert!(out.contains_key("apps/web/deployment.yaml"));
}

/// files/ entries replace whole-file — a file is a scalar, not a
/// mergeable map (D11).
#[test]
fn whole_file_replace() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/files/app.conf".into(),
        b"a: 1\nb: 2\n".to_vec(),
    );
    t.insert(
        "layers/30-device.dev-1/apps/web/files/app.conf".into(),
        b"c: 3\n".to_vec(),
    );
    let out = render(&t, &ctx("dev-1", &["00-fleet", "30-device.dev-1"])).unwrap();
    // Whole-file: no trace of the fleet file's keys.
    assert_eq!(utf8(&out, "apps/web/files/app.conf"), "c: 3\n");
}

/// Three-layer inheritance: keys contributed by fleet, site and device
/// all present; deeper wins on conflict (D3).
#[test]
fn three_layer_inheritance() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/files/fleet.txt".into(),
        b"fleet\n".to_vec(),
    );
    t.insert(
        "layers/20-site.a/apps/web/files/site.txt".into(),
        b"site\n".to_vec(),
    );
    t.insert(
        "layers/30-device.dev-1/apps/web/files/device.txt".into(),
        b"device\n".to_vec(),
    );
    let out = render(
        &t,
        &ctx("dev-1", &["00-fleet", "20-site.a", "30-device.dev-1"]),
    )
    .unwrap();
    assert_eq!(utf8(&out, "apps/web/files/fleet.txt"), "fleet\n");
    assert_eq!(utf8(&out, "apps/web/files/site.txt"), "site\n");
    assert_eq!(utf8(&out, "apps/web/files/device.txt"), "device\n");
}

/// Class layer (05-class.<name>, D12): sits between fleet and region;
/// class overrides fleet, region overrides class.
#[test]
fn class_layer_orders_between_fleet_and_region() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/params.yaml".into(),
        b"greeting: from-fleet\n".to_vec(),
    );
    t.insert(
        "layers/05-class.gpu/apps/web/params.yaml".into(),
        b"greeting: from-class\n".to_vec(),
    );
    let chain = &[
        "00-fleet",
        "05-class.gpu",
        "10-region.eu",
        "30-device.dev-1",
    ];
    let out = render(&t, &ctx("dev-1", chain)).unwrap();
    assert!(utf8(&out, "apps/web/deployment.yaml").contains("value: from-class\n"));

    // Region overrides class.
    t.insert(
        "layers/10-region.eu/apps/web/params.yaml".into(),
        b"greeting: from-region\n".to_vec(),
    );
    let out = render(&t, &ctx("dev-1", chain)).unwrap();
    assert!(utf8(&out, "apps/web/deployment.yaml").contains("value: from-region\n"));
}

/// Merged `enabled: false` removes the app from the render — absent
/// dir = remove (D2); a site switches off a fleet app with one line
/// (D11).
#[test]
fn enabled_false_removes_app() {
    let mut t = base_tree();
    t.insert(
        "layers/20-site.a/apps/web/app.yaml".into(),
        b"enabled: false\n".to_vec(),
    );
    let out = render(
        &t,
        &ctx("dev-1", &["00-fleet", "20-site.a", "30-device.dev-1"]),
    )
    .unwrap();
    assert!(
        !out.keys().any(|k| k.starts_with("apps/web/")),
        "disabled app leaked into render: {:?}",
        out.keys()
    );
    // A deeper layer can switch it back on (scalar override, D3).
    t.insert(
        "layers/30-device.dev-1/apps/web/app.yaml".into(),
        b"enabled: true\n".to_vec(),
    );
    let out = render(
        &t,
        &ctx("dev-1", &["00-fleet", "20-site.a", "30-device.dev-1"]),
    )
    .unwrap();
    assert!(out.contains_key("apps/web/deployment.yaml"));
}

/// Pinned device under rollout (D12, spec/reeve/09-rollouts.md §11.1):
/// the device's target is its own render of the new revision; a device
/// pin makes the apps/ content materially unchanged — only manifest
/// provenance moves.
#[test]
fn pinned_device_under_rollout_is_materially_unchanged() {
    // Old revision: fleet at 1.0.0; device pins version 1.0.0.
    let mut old = base_tree();
    old.insert(
        "layers/30-device.dev-1/apps/web/app.yaml".into(),
        b"package:\n  version: 1.0.0\n".to_vec(),
    );

    // Rollout revision: fleet moves to 2.0.0 (new vendored package);
    // the pin still holds the device at 1.0.0.
    let mut new = old.clone();
    new.insert(
        "layers/00-fleet/apps/web/app.yaml".into(),
        b"package:\n  name: web\n  version: 2.0.0\n".to_vec(),
    );
    new.insert(
        "packages/web/2.0.0/margo.yaml".into(),
        PKG_MANIFEST
            .replace("version: 1.0.0", "version: 2.0.0")
            .into_bytes(),
    );
    new.insert(
        "packages/web/2.0.0/compose.yml".into(),
        PKG_COMPOSE.replace("nginx:1.25", "nginx:2.0").into_bytes(),
    );

    let ctx_old = ctx("dev-1", &["00-fleet", "30-device.dev-1"]);
    let mut ctx_new = ctx("dev-1", &["00-fleet", "30-device.dev-1"]);
    ctx_new.local_revision = 43;
    ctx_new.generation = 2;

    let render_old = render(&old, &ctx_old).unwrap();
    let render_new = render(&new, &ctx_new).unwrap();

    // apps/ byte-identical: the pinned device converges to a render
    // still carrying the pin ("pinned/unaffected" in gate math).
    let apps = |fs: &FileSet| -> FileSet {
        fs.iter()
            .filter(|(k, _)| k.starts_with("apps/"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    assert_eq!(apps(&render_old), apps(&render_new));
    assert!(utf8(&render_new, "apps/web/compose.yml").contains("nginx:1.25"));

    // An UNPINNED device on the same revisions does move.
    let ctx_unpinned = ctx("dev-2", &["00-fleet", "30-device.dev-2"]);
    let moved = render(&new, &ctx_unpinned).unwrap();
    assert!(utf8(&moved, "apps/web/compose.yml").contains("nginx:2.0"));

    // manifest provenance reflects the new declared inputs (D2: NO
    // timestamps — provenance is revision ids + generation only).
    assert_eq!(
        utf8(&render_new, "manifest.yaml"),
        "\
deviceId: dev-1
generation: 2
registryEndpoint: registry.example:5000
revisions:
  local: 43
"
    );
}

/// Determinism (D3 MUST): same inputs => byte-identical file set.
#[test]
fn byte_identical_rerender() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/params.yaml".into(),
        b"greeting: hi\n".to_vec(),
    );
    t.insert(
        "layers/00-fleet/apps/web/files/a.conf".into(),
        b"x = 1\n".to_vec(),
    );
    let c = ctx("dev-1", &["00-fleet", "30-device.dev-1"]);
    let a = render(&t, &c).unwrap();
    let b = render(&t, &c).unwrap();
    assert_eq!(a, b);
}

/// deploymentId is UUIDv5(REEVE_UUID_NAMESPACE, "<device_id>/<app-name>")
/// (D2): stable across re-renders, distinct per device and per app.
#[test]
fn deterministic_deployment_id() {
    // The namespace itself is pinned: UUIDv5(DNS, "reeve.dev").
    assert_eq!(
        REEVE_UUID_NAMESPACE,
        Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"reeve.dev")
    );
    assert_eq!(
        REEVE_UUID_NAMESPACE.to_string(),
        "06c32e1b-5365-5c68-80a2-6cccfa182cf8"
    );

    let id = deployment_id("dev-1", "web");
    assert_eq!(
        id,
        Uuid::new_v5(&REEVE_UUID_NAMESPACE, b"dev-1/web"),
        "name string must be <device_id>/<app-name>"
    );
    assert_eq!(id, deployment_id("dev-1", "web"));
    assert_ne!(id, deployment_id("dev-2", "web"));
    assert_ne!(id, deployment_id("dev-1", "other"));

    // And it is what lands in deployment.yaml.
    let out = render(&base_tree(), &ctx("dev-1", &["00-fleet", "30-device.dev-1"])).unwrap();
    assert!(utf8(&out, "apps/web/deployment.yaml").contains(&format!("id: {id}\n")));
}

/// ${REEVE_REGISTRY} resolves from the device-context input, never
/// from the environment (D3, D8) — in compose.yml, files/ and
/// parameter values.
#[test]
fn registry_resolves_from_device_context() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/files/registry.conf".into(),
        b"pull-from = ${REEVE_REGISTRY}/library\n".to_vec(),
    );
    t.insert(
        "layers/00-fleet/apps/web/params.yaml".into(),
        b"greeting: ${REEVE_REGISTRY}/greeting\n".to_vec(),
    );

    // A hostile env var must be invisible to the render (pure
    // function: no environment reads — D3).
    // SAFETY: single-threaded at this point in the test.
    unsafe { std::env::set_var("REEVE_REGISTRY", "evil.example") };

    let mut c = ctx("dev-1", &["00-fleet", "30-device.dev-1"]);
    let out_a = render(&t, &c).unwrap();
    assert!(
        utf8(&out_a, "apps/web/compose.yml").contains("image: registry.example:5000/nginx:1.25")
    );
    assert_eq!(
        utf8(&out_a, "apps/web/files/registry.conf"),
        "pull-from = registry.example:5000/library\n"
    );
    assert!(
        utf8(&out_a, "apps/web/deployment.yaml").contains("value: registry.example:5000/greeting")
    );

    // A different context endpoint changes the bytes — the endpoint is
    // a DECLARED input recorded in manifest.yaml.
    c.registry_endpoint = "other.example".to_string();
    let out_b = render(&t, &c).unwrap();
    assert!(utf8(&out_b, "apps/web/compose.yml").contains("image: other.example/nginx:1.25"));
    assert!(utf8(&out_b, "manifest.yaml").contains("registryEndpoint: other.example\n"));
    for fs in [&out_a, &out_b] {
        for (path, bytes) in fs.iter() {
            assert!(
                !String::from_utf8_lossy(bytes).contains("evil.example"),
                "environment leaked into {path}"
            );
        }
    }
}

/// Federated provenance: hub revision id recorded in manifest.yaml
/// when present (D2: "hub + local revision when federated").
#[test]
fn manifest_carries_hub_revision_when_federated() {
    let mut c = ctx("dev-1", &["00-fleet"]);
    c.hub_revision = Some(7);
    let out = render(&FileSet::new(), &c).unwrap();
    assert_eq!(
        utf8(&out, "manifest.yaml"),
        "\
deviceId: dev-1
generation: 1
registryEndpoint: registry.example:5000
revisions:
  hub: 7
  local: 42
"
    );
}

// ---------------------------------------------------------------- errors

/// An app defined with no package ref anywhere in the chain cannot
/// render.
#[test]
fn missing_package_ref_is_an_error() {
    let t = tree(&[("layers/00-fleet/apps/web/app.yaml", "enabled: true\n")]);
    let err = render(&t, &ctx("dev-1", &["00-fleet"])).unwrap_err();
    assert!(matches!(err, RenderError::MissingPackageRef { .. }), "{err}");
}

/// A package ref that is not vendored in the revision fails — no
/// fetch-at-render, ever (D11).
#[test]
fn unvendored_package_is_an_error() {
    let t = tree(&[(
        "layers/00-fleet/apps/web/app.yaml",
        "package:\n  name: web\n  version: 9.9.9\n",
    )]);
    let err = render(&t, &ctx("dev-1", &["00-fleet"])).unwrap_err();
    assert!(matches!(err, RenderError::PackageNotFound { .. }), "{err}");
}

/// Setting a parameter the application does not declare is a tree
/// authoring error, not silently dropped.
#[test]
fn unknown_parameter_is_an_error() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/params.yaml".into(),
        b"typo_param: x\n".to_vec(),
    );
    let err = render(&t, &ctx("dev-1", &["00-fleet"])).unwrap_err();
    assert!(matches!(err, RenderError::UnknownParameter { .. }), "{err}");
}

/// A remote packageLocation cannot be rendered in v1 — packages are
/// vendored directories; no network in the render path (D11).
#[test]
fn remote_package_location_is_an_error() {
    let mut t = base_tree();
    t.insert(
        "packages/web/1.0.0/margo.yaml".into(),
        PKG_MANIFEST
            .replace("./compose.yml", "https://example.com/compose.yml")
            .into_bytes(),
    );
    let err = render(&t, &ctx("dev-1", &["00-fleet"])).unwrap_err();
    assert!(matches!(err, RenderError::ComposeSource { .. }), "{err}");
}

/// Selecting a profile id the package does not define fails.
#[test]
fn unknown_profile_is_an_error() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/app.yaml".into(),
        b"package:\n  name: web\n  version: 1.0.0\nprofile: nope\n".to_vec(),
    );
    let err = render(&t, &ctx("dev-1", &["00-fleet"])).unwrap_err();
    assert!(matches!(err, RenderError::UnknownProfile { .. }), "{err}");
}

/// Stray files inside an app's tree dir (e.g. a typo'd params.yml)
/// are rejected, not silently ignored.
#[test]
fn unexpected_app_tree_path_is_an_error() {
    let mut t = base_tree();
    t.insert(
        "layers/00-fleet/apps/web/params.yml".into(),
        b"greeting: lost\n".to_vec(),
    );
    let err = render(&t, &ctx("dev-1", &["00-fleet"])).unwrap_err();
    assert!(
        matches!(err, RenderError::UnexpectedTreePath { .. }),
        "{err}"
    );
}
