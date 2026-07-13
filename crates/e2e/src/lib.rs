//! Track E end-to-end harness (docs/build-charter.md E1): boot the
//! REAL reeve-server on a localhost listener and drive the REAL
//! reeve-agent (library entry points, exactly as `reeve-agent`'s main
//! loop calls them) against it. No mocks of either side — the server
//! is `reeve_server::bootstrap` + `router::build`; the agent is
//! `poll_once` -> `BundleStore::sync` -> `resolve_desired` ->
//! `converge` -> `record_reports` -> `StatusSink::send_unsent`, the
//! same sequence as `reeve-agent/src/main.rs`.
//!
//! The only substituted piece is the workload [`Provider`]: CI has no
//! docker, so [`FakeProvider`] stands in and RECORDS which apps got
//! `up -d` / `down` — the observable that "rotation bounces only
//! consuming services" and "converge is a silent no-op when
//! converged" assert against.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rusqlite::params;
use serde_json::{Value, json};

use reeve_agent::provider::{AppStatus, CapturedRun, Provider, ProviderError};
use reeve_agent::{
    AgentDb, BundleSource, BundleStore, Desired, ManifestSource, PollOutcome, StatusSink, converge,
    poll_once, record_reports, resolve_desired,
};
use reeve_server::config::{AuthMode, Config, DurabilityConfig, DurabilityTier};
use reeve_server::state::AppState;
use reeve_server::{auth, device_tokens, router};
use reeve_types::margo::status::DeploymentState;

// --------------------------------------------------------------- server

/// A live reeve-server: its bound address, its [`AppState`] (so tests
/// can enroll devices and snapshot durability directly), and the temp
/// dirs kept alive for the process lifetime.
pub struct TestServer {
    pub addr: SocketAddr,
    pub state: AppState,
    pub data_dir: tempfile::TempDir,
    pub target_dir: Option<tempfile::TempDir>,
}

impl TestServer {
    /// `http://127.0.0.1:PORT` — the agent's `server` URL.
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Server config with durability disabled — the default for the plain
/// core-loop tests (AuthMode::None => anonymous acts as admin, D1).
pub fn config_disabled(data_dir: &Path) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth: AuthMode::None,
        session_ttl_secs: 3600,
        tier: reeve_server::config::ServerTier::Root,
        registry_endpoint: "registry.example:5000".to_string(),
        durability: DurabilityConfig::disabled(),
        zot: None,
        federation: None,
        install_open: false,
        admin_seed: None,
        logs_retain_per_deployment: 10,
    }
}

/// Server config with the snapshot durability tier pointed at a local
/// filesystem target (the test + air-gap tier, §9.2). Snapshot is CORE
/// (only the changeset CAPTURE tier is `ext-durability-changeset`), so
/// the epoch-restore e2e runs in the conformance build too.
pub fn config_snapshot(data_dir: &Path, target_dir: &Path) -> Config {
    let mut cfg = config_disabled(data_dir);
    cfg.durability = DurabilityConfig {
        tier: DurabilityTier::Snapshot,
        target: Some(target_dir.to_string_lossy().into_owned()),
        instance: "default".into(),
        snapshot_interval_secs: 900,
        retain_days: 7,
        retain_min_generations: 8,
        changeset_interval_secs: 5,
        changeset_commits: 100,
        verify_interval_secs: 86_400,
    };
    cfg
}

/// Bootstrap + serve a router on a real ephemeral port. Mirrors the
/// server's own startup: `bootstrap` (migrate + reconcile) then
/// `auth::bootstrap`. Returns the bound address and the live state.
/// Durability background loops are NOT spawned — tests that exercise
/// durability call `state.durability.snapshot_now` explicitly, so
/// nothing races the assertions.
pub async fn serve_router(cfg: Config) -> (SocketAddr, AppState) {
    let state = reeve_server::bootstrap(cfg).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    let app = router::build(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (addr, state)
}

/// Boot a durability-disabled server, owning its data dir.
pub async fn boot() -> TestServer {
    let data = tempfile::tempdir().unwrap();
    let cfg = config_disabled(data.path());
    let (addr, state) = serve_router(cfg).await;
    TestServer { addr, state, data_dir: data, target_dir: None }
}

/// Boot a durability-disabled server with a custom deploy-log
/// retention (ext-logs, REV-011) — the server keeps the most-recent
/// `retain` logs per (device, deployment).
pub async fn boot_with_log_retain(retain: u64) -> TestServer {
    let data = tempfile::tempdir().unwrap();
    let mut cfg = config_disabled(data.path());
    cfg.logs_retain_per_deployment = retain;
    let (addr, state) = serve_router(cfg).await;
    TestServer { addr, state, data_dir: data, target_dir: None }
}

/// Boot a snapshot-tier server, owning its data dir AND its target dir.
pub async fn boot_snapshot() -> TestServer {
    let data = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let cfg = config_snapshot(data.path(), target.path());
    let (addr, state) = serve_router(cfg).await;
    TestServer { addr, state, data_dir: data, target_dir: Some(target) }
}

/// Insert a device row and issue its bearer token (enrollment proper is
/// covered by enroll_flow.rs; here the row + token are the fixture).
pub fn enroll_device(state: &AppState, id: &str, site: Option<&str>) -> String {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at, site)
         VALUES (?1, 'box', 'x86_64', '0.1.0', 0, ?2)",
        params![id, site],
    )
    .unwrap();
    device_tokens::issue(&conn, id).unwrap()
}

// -------------------------------------------------------------- authoring

/// A thin authoring client over the served HTTP surface — the same
/// routes the UI/CLI drive (`PUT /api/tree/...`, `POST /api/render`,
/// `GET /api/devices`). Anonymous acts as admin under AuthMode::None.
pub struct Author {
    base: String,
    client: reqwest::Client,
}

impl Author {
    pub fn new(base: &str) -> Self {
        Author {
            base: base.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    fn files_body(files: &[(&str, &str)]) -> Value {
        let map: serde_json::Map<String, Value> = files
            .iter()
            .map(|(p, c)| ((*p).to_string(), Value::String(B64.encode(c))))
            .collect();
        json!({ "files": map })
    }

    /// PUT a package under /api/tree/packages/{name}/{version}.
    pub async fn put_package(&self, name: &str, version: &str, files: &[(&str, &str)]) {
        let url = format!("{}/api/tree/packages/{name}/{version}", self.base);
        let resp = self
            .client
            .put(&url)
            .json(&Self::files_body(files))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "put_package {name}: {}", resp.status());
    }

    /// PUT a layer under /api/tree/layers/{layer}. Returns whether the
    /// commit actually changed anything (D14 idempotence).
    pub async fn put_layer(&self, layer: &str, files: &[(&str, &str)]) -> bool {
        let url = format!("{}/api/tree/layers/{layer}", self.base);
        let resp = self
            .client
            .put(&url)
            .json(&Self::files_body(files))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "put_layer {layer}: {}", resp.status());
        let body: Value = resp.json().await.unwrap();
        body.get("changed").and_then(Value::as_bool).unwrap_or(true)
    }

    /// PUT /api/secrets — set or ROTATE a secret (ext-secrets). Same
    /// (name, scope) bumps the version (§12.4). Returns the new version.
    pub async fn put_secret(&self, name: &str, scope: &str, value: &str) -> i64 {
        let url = format!("{}/api/secrets", self.base);
        let resp = self
            .client
            .put(&url)
            .json(&json!({ "name": name, "scope": scope, "value": value }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "put_secret {name}: {}", resp.status());
        let body: Value = resp.json().await.unwrap();
        body["version"].as_i64().expect("secret version")
    }

    /// POST /api/render — the manual render kick; returns the report.
    pub async fn render(&self) -> Value {
        let url = format!("{}/api/render", self.base);
        let resp = self.client.post(&url).send().await.unwrap();
        assert!(resp.status().is_success(), "render: {}", resp.status());
        resp.json().await.unwrap()
    }

    /// GET /api/devices — the human fleet view; returns the array.
    pub async fn devices(&self) -> Vec<Value> {
        let url = format!("{}/api/devices", self.base);
        let resp = self.client.get(&url).send().await.unwrap();
        assert!(resp.status().is_success(), "devices: {}", resp.status());
        resp.json().await.unwrap()
    }

    /// The current deployment state string the server holds for a
    /// device's app (from GET /api/devices deployments[]), if any.
    pub async fn deployment_state(&self, device_id: &str) -> Option<String> {
        let devices = self.devices().await;
        let dev = devices.iter().find(|d| d["deviceId"] == device_id)?;
        let deployments = dev.get("deployments")?.as_array()?;
        let first = deployments.first()?;
        first.get("state").and_then(Value::as_str).map(str::to_string)
    }

    /// Every current deployment the server lists for a device (GET
    /// /api/devices deployments[]) — the observable the removal fix
    /// clears: after a terminal `removed`, the deployment DISAPPEARS here.
    pub async fn device_deployments(&self, device_id: &str) -> Vec<Value> {
        let devices = self.devices().await;
        devices
            .iter()
            .find(|d| d["deviceId"] == device_id)
            .and_then(|d| d.get("deployments"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    // ---------------------------------------------------------- raw HTTP

    /// POST a JSON body; returns the status and parsed body (`Null` if
    /// the response has no JSON body).
    pub async fn post_json(&self, path: &str, body: &Value) -> (reqwest::StatusCode, Value) {
        let resp = self
            .client
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let v = resp.json::<Value>().await.unwrap_or(Value::Null);
        (status, v)
    }

    /// PATCH a JSON body; returns the status and parsed body.
    pub async fn patch_json(&self, path: &str, body: &Value) -> (reqwest::StatusCode, Value) {
        let resp = self
            .client
            .patch(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let v = resp.json::<Value>().await.unwrap_or(Value::Null);
        (status, v)
    }

    /// GET; returns the status and parsed body.
    pub async fn get_json(&self, path: &str) -> (reqwest::StatusCode, Value) {
        let resp = self.client.get(format!("{}{path}", self.base)).send().await.unwrap();
        let status = resp.status();
        let v = resp.json::<Value>().await.unwrap_or(Value::Null);
        (status, v)
    }

    /// DELETE; returns the status only.
    pub async fn delete_status(&self, path: &str) -> reqwest::StatusCode {
        self.client
            .delete(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
            .status()
    }

    // ------------------------------------------------- operator actions

    /// POST /api/groups — create a fleet (parent None) or a site (parent =
    /// a fleet id). Returns the status and body (the [`GroupNode`]).
    pub async fn create_group(
        &self,
        kind: &str,
        name: &str,
        parent_id: Option<i64>,
    ) -> (reqwest::StatusCode, Value) {
        self.post_json("/api/groups", &json!({ "kind": kind, "name": name, "parentId": parent_id }))
            .await
    }

    /// GET /api/groups — the fleet->site containment tree.
    pub async fn groups(&self) -> Value {
        self.get_json("/api/groups").await.1
    }

    /// PATCH /api/groups/{id} — rename. Status + body.
    pub async fn rename_group(&self, id: i64, name: &str) -> (reqwest::StatusCode, Value) {
        self.patch_json(&format!("/api/groups/{id}"), &json!({ "name": name })).await
    }

    /// PATCH /api/devices/{id} — partial device update. Status + detail.
    pub async fn patch_device(&self, id: &str, body: Value) -> (reqwest::StatusCode, Value) {
        self.patch_json(&format!("/api/devices/{id}"), &body).await
    }

    /// POST /api/deploy — deploy a stack to a scope. Status + body.
    pub async fn deploy(&self, stack: Value, scope: Value) -> (reqwest::StatusCode, Value) {
        self.post_json("/api/deploy", &json!({ "stack": stack, "scope": scope })).await
    }

    /// POST /api/undeploy — remove a stack from a scope. Status + body.
    pub async fn undeploy(&self, stack: Value, scope: Value) -> (reqwest::StatusCode, Value) {
        self.post_json("/api/undeploy", &json!({ "stack": stack, "scope": scope })).await
    }

    /// POST /api/devices/{id}/decommission — revoke + tombstone.
    pub async fn decommission(&self, id: &str) -> reqwest::StatusCode {
        self.post_json(&format!("/api/devices/{id}/decommission"), &json!({})).await.0
    }

    /// GET /api/history — the human change log, newest first.
    pub async fn history(&self) -> Vec<Value> {
        self.get_json("/api/history").await.1.as_array().cloned().unwrap_or_default()
    }

    /// POST /api/history/{id}/undo. Status + body.
    pub async fn undo(&self, id: i64) -> (reqwest::StatusCode, Value) {
        self.post_json(&format!("/api/history/{id}/undo"), &json!({})).await
    }

    /// Device-auth manifest poll with a bearer token — the surface a
    /// decommissioned credential is cut off from (401). Returns the
    /// raw status.
    pub async fn manifest_poll_status(&self, token: &str) -> reqwest::StatusCode {
        self.client
            .get(format!("{}/api/reeve/v1/manifest", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status()
    }
}

/// The canonical single-app compose package manifest (the shape
/// desired-state's table tests pin). `greeting` flows into the rendered
/// compose env. `{id}` is templated so a second, distinctly-named stack
/// can be vendored for multi-app tests.
pub const WEB_MANIFEST: &str = "\
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

pub const WEB_COMPOSE: &str = "\
services:
  web:
    image: ${REEVE_REGISTRY}/nginx:1.25
";

/// Vendor just the `web` package (no layer): the stack a deploy-to-scope
/// authors. Use with [`Author::deploy`].
pub async fn vendor_web(author: &Author) {
    author
        .put_package("web", "1.0.0", &[("margo.yaml", WEB_MANIFEST), ("compose.yml", WEB_COMPOSE)])
        .await;
}

/// Vendor a second, distinctly-named single-service package (`id`) so a
/// test can deploy TWO stacks and tell them apart in FakeProvider up/down
/// records (the dir name is `id`).
pub async fn vendor_named(author: &Author, id: &str) {
    let manifest = WEB_MANIFEST.replace("id: web", &format!("id: {id}"));
    let compose = format!("services:\n  {id}:\n    image: ${{REEVE_REGISTRY}}/nginx:1.25\n");
    author
        .put_package(id, "1.0.0", &[("margo.yaml", &manifest), ("compose.yml", &compose)])
        .await;
}

/// Author the canonical single-app compose package + base (`00-all`)
/// layer used by the core-loop tests. `greeting` flows into the rendered
/// compose env.
pub async fn author_web_app(author: &Author) {
    vendor_web(author).await;
    author
        .put_layer(
            "00-all",
            &[("apps/web/app.yaml", "package:\n  name: web\n  version: 1.0.0\n")],
        )
        .await;
}

// ------------------------------------------------------------ fake provider

/// A workload [`Provider`] that performs NO real docker work but
/// records every `up -d` (apply) and `down` (remove) by app-dir name,
/// in call order — the observable for convergence assertions. Apps
/// named in `fail` report a failed status (for gate/health tests).
#[derive(Default)]
pub struct FakeProvider {
    ups: Mutex<Vec<String>>,
    downs: Mutex<Vec<String>>,
    fail: Mutex<HashSet<String>>,
    /// Apps whose `up` command itself FAILS (returns `ProviderError`) —
    /// the `docker compose up` non-zero-exit case (e.g. pull denied).
    /// Unlike [`fail`](Self::fail) (up ran, container unhealthy), an
    /// errored app is never marked `applied`, so converge re-applies it
    /// every pass — and each attempt's capture is `success == false`
    /// (a `failed` deploy log).
    err: Mutex<HashSet<String>>,
    /// Per-app canned combined `compose up`/`down` output the provider
    /// "captured" (ext-logs, REV-011). Mirrors what the real
    /// `CommandComposeProvider` records into its interior slot; drained
    /// by [`Provider::take_capture`] exactly once per apply/remove so
    /// converge can harvest it onto the [`AppReport`].
    output: Mutex<HashMap<String, String>>,
    /// The most-recent capture, set on every apply/remove, drained by
    /// `take_capture` (latest-run-wins, like the compose provider).
    capture: Mutex<Option<CapturedRun>>,
}

impl FakeProvider {
    pub fn new() -> Self {
        Self::default()
    }
    /// Mark an app so its next apply reports `error` (a failed gate).
    pub fn fail_app(&self, name: &str) {
        self.fail.lock().unwrap().insert(name.to_string());
    }
    /// Mark an app so its `up` command FAILS (`apply` returns
    /// `ProviderError`) — the compose-up-failed case (pull denied, bad
    /// image). The one-line reason rides in the returned `ProviderError`
    /// (-> Margo `status.error`), the full captured output in the log.
    pub fn error_app(&self, name: &str) {
        self.err.lock().unwrap().insert(name.to_string());
    }
    /// Clear prior [`fail_app`](Self::fail_app)/[`error_app`](Self::error_app)
    /// marks so the app's next apply converges (a recovered deploy).
    pub fn heal_app(&self, name: &str) {
        self.fail.lock().unwrap().remove(name);
        self.err.lock().unwrap().remove(name);
    }
    /// Set the combined output this provider "captures" for an app's
    /// next apply/remove — the failure text an operator later reads
    /// back through the ext-logs GET routes. Absent = a generated
    /// default line.
    pub fn set_output(&self, name: &str, text: &str) {
        self.output.lock().unwrap().insert(name.to_string(), text.to_string());
    }
    /// Build + stash the capture for one lifecycle run, mirroring
    /// `CommandComposeProvider::run_lifecycle`.
    fn record_capture(&self, name: &str, phase_up: bool, success: bool) {
        let combined = self.output.lock().unwrap().get(name).cloned().unwrap_or_else(|| {
            let verb = if phase_up { "up" } else { "down" };
            if success {
                format!("compose {verb} {name}\nContainer {name} Started\n")
            } else {
                format!("compose {verb} {name}\nError: compose {verb} failed for {name}\n")
            }
        });
        *self.capture.lock().unwrap() = Some(CapturedRun {
            phase_up,
            combined,
            exit_code: Some(if success { 0 } else { 1 }),
            truncated: false,
            success,
        });
    }
    /// Apps that were `up -d`'d, in order (duplicates = re-ups).
    pub fn ups(&self) -> Vec<String> {
        self.ups.lock().unwrap().clone()
    }
    /// Apps that were `down`'d, in order.
    pub fn downs(&self) -> Vec<String> {
        self.downs.lock().unwrap().clone()
    }
    /// How many times a given app was `up -d`'d.
    pub fn up_count(&self, name: &str) -> usize {
        self.ups().iter().filter(|n| *n == name).count()
    }
    fn dir_name(dir: &Path) -> String {
        dir.file_name().unwrap().to_string_lossy().into_owned()
    }
}

impl Provider for FakeProvider {
    fn apply(&self, app_dir: &Path) -> Result<AppStatus, ProviderError> {
        let name = Self::dir_name(app_dir);
        self.ups.lock().unwrap().push(name.clone());
        // `up` command failed outright (Err) — captured as success=false,
        // and converge never marks it applied, so it re-applies next pass.
        if self.err.lock().unwrap().contains(&name) {
            self.record_capture(&name, true, false);
            return Err(ProviderError(format!("`docker compose up` failed for {name}")));
        }
        // `up` ran (capture success=true); state may still be Failed when
        // the app is marked unhealthy via `fail`.
        self.record_capture(&name, true, true);
        let state = if self.fail.lock().unwrap().contains(&name) {
            DeploymentState::Failed
        } else {
            DeploymentState::Installed
        };
        Ok(AppStatus { state, detail: None })
    }
    fn remove(&self, retained_dir: &Path) -> Result<(), ProviderError> {
        let name = Self::dir_name(retained_dir);
        self.downs.lock().unwrap().push(name.clone());
        self.record_capture(&name, false, true);
        Ok(())
    }
    fn status(&self, app_dir: &Path) -> Result<AppStatus, ProviderError> {
        let name = Self::dir_name(app_dir);
        let state = if self.fail.lock().unwrap().contains(&name) {
            DeploymentState::Failed
        } else {
            DeploymentState::Installed
        };
        Ok(AppStatus { state, detail: None })
    }
    fn take_capture(&self) -> Option<CapturedRun> {
        self.capture.lock().unwrap().take()
    }
}

// ------------------------------------------------------------------- agent

/// A real reeve-agent driven one loop-iteration at a time. Holds the
/// same state the binary does: agent.db, the bundle store, the manifest
/// + bundle sources, and (for enrolled HTTP agents) a StatusSink.
pub struct TestAgent {
    pub data_dir: tempfile::TempDir,
    pub db: AgentDb,
    pub store: BundleStore,
    pub source: ManifestSource,
    pub bundle_source: BundleSource,
    pub sink: Option<StatusSink>,
    pub device_id: String,
    /// ext-logs (REV-011): the device-token uploader for captured
    /// compose output, exactly as `reeve-agent/src/main.rs` builds it.
    /// `tick` runs `record_logs` over each pass's reports through this,
    /// mirroring the binary's post-converge hook. Gated on the e2e
    /// `ext` feature so the core-only conformance build (ext-logs
    /// compiled out) still compiles this harness.
    #[cfg(feature = "ext")]
    pub logs: Option<reeve_agent::ext::logs::LogUploader>,
}

/// What one [`TestAgent::tick`] observed — enough to assert epoch bumps,
/// offline no-ops, and which apps converge acted on.
#[derive(Debug)]
pub struct TickOutcome {
    pub poll: PollOutcome,
    pub acted: Vec<String>,
}

impl TestAgent {
    /// Construct an enrolled HTTP agent pointed at `server_base`.
    pub fn http(server_base: &str, device_id: &str, token: &str) -> Self {
        let data_dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(&data_dir.path().join("agent.db")).unwrap();
        let store = BundleStore::open(data_dir.path()).unwrap();
        let source = ManifestSource::parse(server_base, Some(token.to_string())).unwrap();
        let bundle_source = BundleSource::parse(server_base, Some(token.to_string())).unwrap();
        let sink =
            StatusSink::from_config(server_base, Some(token.to_string()), Some(device_id.to_string()));
        #[cfg(feature = "ext")]
        let logs = reeve_agent::ext::logs::LogUploader::from_config(
            server_base,
            Some(token.to_string()),
            Some(device_id.to_string()),
        );
        TestAgent {
            data_dir,
            db,
            store,
            source,
            bundle_source,
            sink,
            device_id: device_id.to_string(),
            #[cfg(feature = "ext")]
            logs,
        }
    }

    /// The agent's data dir path (where the converged app tree lands).
    pub fn path(&self) -> PathBuf {
        self.data_dir.path().to_path_buf()
    }

    /// Startup recovery (Law 3): the same `BundleStore::recover` the
    /// binary runs before its first converge. Idempotent.
    pub fn recover(&mut self) {
        self.store.recover(&mut self.db).expect("store recovery");
    }

    /// One full loop iteration, mirroring reeve-agent/src/main.rs:
    /// poll -> (on accept/304) sync bundle -> converge -> record ->
    /// send. Returns the poll outcome and the apps converge acted on.
    pub async fn tick(&mut self, provider: &dyn Provider) -> TickOutcome {
        let poll = poll_once(&mut self.db, &self.source).await;
        match poll {
            PollOutcome::Accepted { .. } | PollOutcome::NotModified => {
                // 304 does not imply the bundle is in place (an accept
                // whose pull crashed retries here); Accepted pulls the
                // new bundle. sync short-circuits when already swapped.
                if let Err(e) = self.store.sync(&mut self.db, &self.bundle_source).await {
                    // Offline/pull failure is journaled inside apply;
                    // converge still runs from last known state (Law 5).
                    eprintln!("bundle sync: {e}");
                }
            }
            PollOutcome::SourceUnavailable | PollOutcome::Rejected { .. } => {}
        }
        let desired = resolve_desired(&self.db, &self.store);
        let reports = converge(&mut self.db, self.data_dir.path(), provider, &desired);
        let acted: Vec<String> = reports.iter().map(|r| r.app_id.clone()).collect();
        record_reports(&self.db, &reports);
        // ext-logs (REV-011): persist + best-effort upload the captured
        // compose output for each acted-on app, on the same cadence and
        // in the same place `reeve-agent/src/main.rs` runs it (right
        // after record_reports). Feature-gated so the conformance build
        // (ext-logs out) never references it.
        #[cfg(feature = "ext")]
        reeve_agent::ext::logs::record_logs(
            &self.db,
            self.data_dir.path(),
            self.logs.as_ref(),
            &reports,
        )
        .await;
        if let Some(sink) = &self.sink {
            sink.send_unsent(&self.db).await;
        }
        TickOutcome { poll, acted }
    }

    /// Resolve the agent's current desired state (local reads only).
    pub fn desired(&self) -> Desired {
        resolve_desired(&self.db, &self.store)
    }

    /// The rendered compose file for an app in the swapped bundle, if
    /// present — lets tests read what actually landed on the box.
    pub fn app_compose(&self, app: &str) -> Option<String> {
        let path = self.store.current_path().join("apps").join(app).join("compose.yml");
        std::fs::read_to_string(path).ok()
    }

    /// Env-file materialization the converge pass wrote for a service
    /// (ext-secrets / plain env targets), if present.
    pub fn app_env(&self, app: &str, service: &str) -> Option<String> {
        let path = self
            .data_dir
            .path()
            .join("apps")
            .join(app)
            .join("env")
            .join(format!("{service}.env"));
        std::fs::read_to_string(path).ok()
    }
}

/// Open the agent's on-disk state at `data` against `base` — the exact
/// set of handles a restarted agent process reconstructs (Law 3:
/// startup IS recovery). Lets a test "reopen" an agent on the same data
/// dir after a simulated `kill -9`, or point the same agent.db at a
/// different server (post-restore epoch fencing).
pub fn open_agent(
    data: &Path,
    base: &str,
    token: &str,
) -> (AgentDb, BundleStore, ManifestSource, BundleSource) {
    let db = AgentDb::open(&data.join("agent.db")).unwrap();
    let store = BundleStore::open(data).unwrap();
    let source = ManifestSource::parse(base, Some(token.to_string())).unwrap();
    let bundle = BundleSource::parse(base, Some(token.to_string())).unwrap();
    (db, store, source, bundle)
}

/// Assert the agent converged app `name` to Installed at least once and
/// the server holds a non-failed state for it — a convenience for the
/// happy-path core loop. Returns the map of per-app up counts.
pub async fn assert_converged(
    author: &Author,
    device_id: &str,
    provider: &FakeProvider,
) -> HashMap<String, usize> {
    let state = author.deployment_state(device_id).await;
    assert!(
        matches!(state.as_deref(), Some("installed") | Some("Installed")),
        "server must show installed for {device_id}, got {state:?}"
    );
    let mut counts = HashMap::new();
    for app in provider.ups() {
        *counts.entry(app).or_insert(0) += 1;
    }
    counts
}
