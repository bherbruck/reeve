//! C12 packaging (spec/reeve/08-packaging.md REV-007) end to end:
//! the CLI surfaces of the shipped binary (--version with git
//! revision, --spec in index order, --completions), `init` emission
//! (compose canonical-file sync per docs/decisions/deploy.md D9,
//! idempotency, keyfile + separate-backup warning), and the §10.4
//! /install bootstrap: ABSENT without the embedded-agents feature
//! (404 — invisible, 01-framework §3.1 rule 4); with the feature,
//! script + OCI artifact serving over injected dummy binaries.

use std::path::Path as FsPath;
use std::process::Command;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use reeve_server::config::{AuthMode, Config};
use reeve_server::state::AppState;
use reeve_server::{auth, router};

// ------------------------------------------------------------- harness

fn config(data_dir: &FsPath, install_open: bool) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth: AuthMode::None, // anonymous acts as admin (D1)
        session_ttl_secs: 3600,
        registry_endpoint: "registry.example:5000".to_string(),
        durability: reeve_server::config::DurabilityConfig::disabled(),
        zot: None,
        federation: None,
        install_open,
    }
}

fn app(dir: &FsPath, install_open: bool) -> (Router, AppState) {
    let state = reeve_server::bootstrap(config(dir, install_open)).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(Method::GET).uri(uri).header(header::HOST, "reeve.example:8420");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reeve-server"))
}

// ------------------------------------------------ §10.1 CLI surfaces

#[test]
fn version_includes_workspace_git_revision() {
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    // build.rs GIT_HASH is set for every target of this package —
    // tests and binary see the same value.
    assert!(!env!("GIT_HASH").is_empty());
    assert_eq!(
        stdout.trim(),
        format!("reeve-server {} (git {})", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"))
    );
    // In a git checkout the revision must be real, not the fallback.
    if FsPath::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.git")).exists() {
        assert_ne!(env!("GIT_HASH"), "unknown");
    }
}

#[test]
fn spec_flag_prints_whole_spec_in_index_order() {
    let out = bin().arg("--spec").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("# The reeve Specification"), "00-INDEX.md first");
    // Index order: every numbered section present, in order.
    let mut last = 0usize;
    for marker in [
        "## 3. Extension Framework & Conformance", // 01-framework
        "## 4. Persistent Agent Channel",          // 02-channel
        "## 10. Packaging & Self-Hosting",          // 08-packaging
        "## 12. Secrets (REV-009)",                 // 10-secrets
    ] {
        let pos = stdout.find(marker).unwrap_or_else(|| panic!("missing {marker:?}"));
        assert!(pos > last, "{marker:?} out of index order");
        last = pos;
    }
}

#[test]
fn spec_flag_prints_a_single_section() {
    let out = bin().args(["--spec", "08-packaging"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("## 10. Packaging & Self-Hosting (REV-007)"));
    assert!(!stdout.contains("## 12. Secrets"), "single section only");

    let bad = bin().args(["--spec", "99-nope"]).output().unwrap();
    assert!(!bad.status.success(), "unknown section is an error");
}

#[test]
fn completions_flag_prints_shell_scripts() {
    for (shell, needle) in [
        ("bash", "complete -F _reeve_server reeve-server"),
        ("zsh", "#compdef reeve-server"),
        ("fish", "complete -c reeve-server"),
    ] {
        let out = bin().args(["--completions", shell]).output().unwrap();
        assert!(out.status.success(), "{shell}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains(needle), "{shell}: {stdout}");
        assert!(stdout.contains("init"), "{shell} completes subcommands");
    }
    let bad = bin().args(["--completions", "powershell"]).output().unwrap();
    assert!(!bad.status.success());
}

// ---------------------------------------------------- §10.3 init

#[test]
fn init_emits_compose_and_zot_idempotently_and_warns_about_keyfile() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("deploy");
    let data_dir = dir.path().join("data");
    let run = || {
        bin()
            .args(["init", "--out", out_dir.to_str().unwrap(), "--registry"])
            .env("REEVE_DATA_DIR", &data_dir)
            .output()
            .unwrap()
    };

    let first = run();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let stdout = String::from_utf8(first.stdout).unwrap();
    // §10.3: MUST warn that the keyfile needs separate backup.
    assert!(stdout.contains("back it up separately"), "{stdout}");
    assert!(out_dir.join("compose.yml").exists());
    assert!(out_dir.join("zot-config.json").exists());
    let key = std::fs::read(data_dir.join("secret.key")).unwrap();
    assert_eq!(key.len(), 32);

    // Idempotent (§10.3): re-run converges, never errors, never
    // re-mints the keyfile.
    let second = run();
    assert!(second.status.success());
    assert!(String::from_utf8(second.stdout).unwrap().contains("kept existing keyfile"));
    assert_eq!(std::fs::read(data_dir.join("secret.key")).unwrap(), key);
}

/// D9 / §10.6: the root `compose.yml` is the ONE checked-in emittable
/// file; `init` emits a copy of it and CI keeps the two in sync —
/// this test IS that check (byte-identical AND same service set, so
/// a divergence names the services involved).
#[test]
fn init_compose_matches_canonical_file() {
    let canonical_path = FsPath::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../compose.yml"));
    let canonical = std::fs::read_to_string(canonical_path).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let opts = reeve_server::init::InitOptions {
        out_dir: dir.path().join("out"),
        format: reeve_server::init::InitFormat::Compose,
        registry: false,
        data_dir: dir.path().join("data"),
        upstream: None,
    };
    reeve_server::init::run(&opts).unwrap();
    let emitted = std::fs::read_to_string(dir.path().join("out/compose.yml")).unwrap();

    let services = |yaml: &str| -> Vec<String> {
        let v: serde_json::Value =
            serde_yaml_ng::from_str::<serde_json::Value>(yaml).expect("compose parses");
        let mut names: Vec<String> =
            v["services"].as_object().expect("services map").keys().cloned().collect();
        names.sort();
        names
    };
    assert_eq!(services(&canonical), vec!["reeve-server", "registry"]);
    assert_eq!(services(&emitted), services(&canonical), "init drifted from compose.yml (D9)");
    assert_eq!(emitted, canonical, "init must emit the canonical compose file verbatim");
}

/// Guard against the compose file referencing env vars the binary
/// never reads (the class of drift that once shipped REEVE_PORT /
/// REEVE_SNAPSHOT_TARGET / REEVE_REGISTRY_BACKEND — names nothing in
/// config.rs consumes). Every `REEVE_*` key in the compose service
/// environment MUST be a name the server actually honours.
#[test]
fn compose_env_keys_are_all_read_by_the_binary() {
    // The env surface config.rs documents/reads (crates/reeve-server/
    // src/config.rs). REEVE_IMAGE / REEVE_PORT are compose-level knobs
    // (image ref, host port map), not server env — allowed as interp
    // defaults but never set INTO the container environment.
    const KNOWN: &[&str] = &[
        "REEVE_LISTEN",
        "REEVE_DATA_DIR",
        "REEVE_AUTH",
        "REEVE_PROXY_USER_HEADER",
        "REEVE_PROXY_ROLE_HEADER",
        "REEVE_PROXY_TRUSTED_CIDR",
        "REEVE_SESSION_TTL_SECS",
        "REEVE_REGISTRY",
        "REEVE_UPSTREAM",
        "REEVE_UPSTREAM_TOKEN",
        "REEVE_SITE",
        "REEVE_SYNC_INTERVAL_SECS",
        "REEVE_DURABILITY",
        "REEVE_DURABILITY_TARGET",
        "REEVE_DURABILITY_INSTANCE",
        "REEVE_DURABILITY_SNAPSHOT_INTERVAL_SECS",
        "REEVE_DURABILITY_RETAIN_DAYS",
        "REEVE_DURABILITY_RETAIN_MIN_GENERATIONS",
        "REEVE_DURABILITY_CHANGESET_INTERVAL_SECS",
        "REEVE_DURABILITY_CHANGESET_COMMITS",
        "REEVE_DURABILITY_VERIFY_INTERVAL_SECS",
        "REEVE_ZOT_URL",
        "REEVE_ZOT_USERNAME",
        "REEVE_ZOT_PASSWORD",
        "REEVE_INSTALL_OPEN",
    ];
    let canonical_path = FsPath::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../compose.yml"));
    let canonical = std::fs::read_to_string(canonical_path).unwrap();
    let v: serde_json::Value =
        serde_yaml_ng::from_str::<serde_json::Value>(&canonical).expect("compose parses");
    // Compose list form: `- KEY=value`; take the key before the first '='.
    let entries = v["services"]["reeve-server"]["environment"]
        .as_array()
        .expect("reeve-server.environment list");
    let keys: Vec<String> = entries
        .iter()
        .map(|e| e.as_str().expect("env entry is a string"))
        .map(|e| e.split_once('=').map(|(k, _)| k).unwrap_or(e).to_string())
        .collect();
    for key in keys.iter().filter(|k| k.starts_with("REEVE_")) {
        assert!(
            KNOWN.contains(&key.as_str()),
            "compose sets {key} into the server environment, but config.rs reads no such var — \
             drift (add it to config.rs, or fix the compose key)"
        );
    }
    // AWS_* target creds are read by object_store, not config.rs —
    // present in the compose but intentionally outside the REEVE_ set.
    assert!(keys.iter().any(|k| k == "AWS_ACCESS_KEY_ID"), "S3 target creds must be wired");
}

#[test]
fn init_systemd_emits_units_without_values() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("units");
    let out = bin()
        .args(["init", "--out", out_dir.to_str().unwrap(), "--format", "systemd"])
        .env("REEVE_DATA_DIR", dir.path().join("data"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out_dir.join("reeve-server.service").exists());
    assert!(out_dir.join("reeve-server.env").exists());
}

// ------------------------------------- §10.4 /install: feature OFF

/// Without the embedded-agents feature the route is ABSENT — 404,
/// invisible (01-framework §3.1 rule 4). The SPA fallback must not
/// mask this while no UI is embedded.
#[cfg(not(feature = "embedded-agents"))]
#[tokio::test]
async fn install_route_absent_without_the_feature() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), false);
    let (status, _) = send(&app, get("/install", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The agent-artifact repos do not exist either: even an
    // authenticated device sees the native namespace's 404 (the /v2
    // catch-all itself is device-auth'd — an anonymous probe gets 401
    // there, which is the pre-existing /v2 posture, not this feature).
    let device_token = {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at)
             VALUES ('dev-1', 'box', 'x86_64', '0.1.0', 0)",
            [],
        )
        .unwrap();
        reeve_server::device_tokens::issue(&conn, "dev-1").unwrap()
    };
    let (status, _) = send(
        &app,
        get(
            "/v2/reeve/agent/x86_64/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000",
            Some(&device_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// -------------------------------------- §10.4 /install: feature ON

#[cfg(feature = "embedded-agents")]
mod embedded {
    use super::*;
    use reeve_server::ext::install::{ArchArtifact, router as install_router};
    use reeve_server::join_tokens;
    use std::borrow::Cow;

    const DUMMY: &[u8] = b"\x7fELF-dummy-agent-binary";

    fn dummy_artifacts() -> Vec<ArchArtifact> {
        vec![ArchArtifact::new("x86_64", Cow::Borrowed(DUMMY))]
    }

    fn app_with_install(dir: &FsPath, open: bool) -> (Router, AppState) {
        let (app, state) = app(dir, open);
        let app = app.merge(install_router(&state, dummy_artifacts()));
        (app, state)
    }

    fn join_token(state: &AppState) -> String {
        join_tokens::issue(&state.db.lock().unwrap(), "op", 3600, 1, None).unwrap()
    }

    #[tokio::test]
    async fn install_requires_an_enrollment_token_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let (app, state) = app_with_install(dir.path(), false);

        let (status, _) = send(&app, get("/install", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Garbage and revoked tokens stay out.
        let (status, _) = send(&app, get("/install", Some("rvj_nope"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let revoked = join_token(&state);
        join_tokens::revoke(&state.db.lock().unwrap(), &device_api::token_hash(&revoked)).unwrap();
        let (status, _) = send(&app, get("/install", Some(&revoked))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn install_script_bakes_digest_arch_and_token() {
        let dir = tempfile::tempdir().unwrap();
        let (app, state) = app_with_install(dir.path(), false);
        let token = join_token(&state);

        let expected_digest = revision_store::digest_of(DUMMY);
        // Bearer header form.
        let (status, body) = send(&app, get("/install", Some(&token))).await;
        assert_eq!(status, StatusCode::OK);
        let script = String::from_utf8(body).unwrap();
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains(&format!("DIGEST_X86_64=\"{expected_digest}\"")));
        assert!(script.contains("DIGEST_AARCH64=\"\""), "absent arch => empty digest");
        assert!(script.contains(&format!("TOKEN=\"{token}\"")));
        assert!(script.contains("SERVER=\"http://reeve.example:8420\""), "origin from Host");
        assert!(script.contains("install --server \"$SERVER\" --token \"$TOKEN\""));

        // ?token= query form (the curl|sh flow) is equivalent, and
        // the pull it authorizes must NOT consume the token's use.
        let (status, _) = send(&app, get(&format!("/install?token={token}"), None)).await;
        assert_eq!(status, StatusCode::OK);
        let uses: i64 = state
            .db
            .lock()
            .unwrap()
            .query_row("SELECT uses FROM join_tokens", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uses, 0, "/install must not burn the enrollment token");
    }

    #[tokio::test]
    async fn agent_artifact_pulls_by_digest() {
        let dir = tempfile::tempdir().unwrap();
        let (app, state) = app_with_install(dir.path(), false);
        let token = join_token(&state);
        let artifacts = dummy_artifacts();
        let a = &artifacts[0];

        // Manifest by tag and by digest; stock OCI shape.
        for reference in ["latest", a.manifest_digest.as_str()] {
            let (status, body) = send(
                &app,
                get(&format!("/v2/reeve/agent/x86_64/manifests/{reference}"), Some(&token)),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{reference}");
            let m: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(m["schemaVersion"], 2);
            assert_eq!(m["layers"][0]["digest"], a.blob_digest);
        }

        // Blob: exact bytes back, digest-addressed.
        let (status, body) =
            send(&app, get(&format!("/v2/reeve/agent/x86_64/blobs/{}", a.blob_digest), Some(&token))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, DUMMY);

        // Unknown digest / arch: 404. Unauthenticated: 401.
        let (status, _) = send(
            &app,
            get(&format!("/v2/reeve/agent/x86_64/blobs/sha256:{}", "0".repeat(64)), Some(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) =
            send(&app, get(&format!("/v2/reeve/agent/aarch64/blobs/{}", a.blob_digest), Some(&token))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) =
            send(&app, get(&format!("/v2/reeve/agent/x86_64/blobs/{}", a.blob_digest), None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn device_credential_pulls_agent_artifacts_for_self_update() {
        // §10.5: the self-update prefetch pulls the same artifacts
        // with the device token.
        let dir = tempfile::tempdir().unwrap();
        let (app, state) = app_with_install(dir.path(), false);
        let device_token = {
            let conn = state.db.lock().unwrap();
            conn.execute(
                "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at)
                 VALUES ('dev-1', 'box', 'x86_64', '0.1.0', 0)",
                [],
            )
            .unwrap();
            reeve_server::device_tokens::issue(&conn, "dev-1").unwrap()
        };
        let digest = revision_store::digest_of(DUMMY);
        let (status, body) =
            send(&app, get(&format!("/v2/reeve/agent/x86_64/blobs/{digest}"), Some(&device_token))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, DUMMY);
    }

    #[tokio::test]
    async fn install_open_admits_anonymous() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _state) = app_with_install(dir.path(), true);
        let (status, body) = send(&app, get("/install", None)).await;
        assert_eq!(status, StatusCode::OK);
        let script = String::from_utf8(body).unwrap();
        assert!(script.contains("TOKEN=\"\""), "no token baked when opened anonymously");
        assert!(script.contains("exec \"$TMP/reeve-agent\" install\n"), "enroll-less install");
    }
}
