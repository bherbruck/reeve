//! REV-010 operator fleet model, end-to-end (server + REAL agent +
//! FakeProvider): every operator action from spec/reeve/11-fleet-model.md
//! driven through the HTTP API the UI uses, each asserting a real
//! observable outcome — the layer chain a device renders, the workloads
//! the agent's provider brings up/down, and the deployment states the
//! server lists back.
//!
//! Headline regression (test [`move_device_clears_removed_deployment`]):
//! a device moved out of a site removes the app and it DISAPPEARS from the
//! device's current deployments — before the ingest fix the
//! `deployment_status_current` row lingered at "removed" forever, because
//! `upsert_current` only ever INSERT..ON CONFLICT DO UPDATE'd and never
//! DELETEd a terminal-removed report.

use e2e::{Author, FakeProvider, TestAgent, boot, enroll_device, vendor_named, vendor_web};
use reeve_agent::PollOutcome;
use reqwest::StatusCode;
use serde_json::{Value, json};

/// The minimal stack ref for a vendored package (defaults `name` to the
/// package, §11.4).
fn stack(pkg: &str) -> Value {
    json!({ "package": pkg, "version": "1.0.0" })
}

/// A `{kind:"site"}` deploy/rollout scope (§11.4).
fn site_scope(name: &str) -> Value {
    json!({ "kind": "site", "name": name })
}

/// Create the `north` fleet with the given sites under it; return the
/// fleet's group id. Panics on any non-201 (fixture setup).
async fn fleet_with_sites(author: &Author, fleet: &str, sites: &[&str]) -> i64 {
    let (st, body) = author.create_group("fleet", fleet, None).await;
    assert_eq!(st, StatusCode::CREATED, "create fleet {fleet}: {body}");
    let fid = body["id"].as_i64().unwrap();
    for s in sites {
        let (st, body) = author.create_group("site", s, Some(fid)).await;
        assert_eq!(st, StatusCode::CREATED, "create site {s}: {body}");
    }
    fid
}

// ---------------------------------------------------------------------
// 1. Enroll + assign fleet/site; the render reflects the assignment.
// ---------------------------------------------------------------------

/// Assigning a device to a fleet+site (a) round-trips on the device row
/// and (b) pulls the site's layer into its render chain — the site param
/// lands in what the agent actually converges.
#[tokio::test]
async fn assign_fleet_site_updates_row_and_render_chain() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    fleet_with_sites(&author, "north", &["plant-a"]).await;
    let token = enroll_device(&srv.state, "dev-1", None);
    // web deployed to every device (00-all).
    e2e::author_web_app(&author).await;

    let (st, detail) = author.patch_device("dev-1", json!({ "fleet": "north", "site": "plant-a" })).await;
    assert_eq!(st, StatusCode::OK, "assign: {detail}");
    assert_eq!(detail["fleet"], "north");
    assert_eq!(detail["site"], "plant-a");

    // A site-scoped param proves the chain now includes 20-site.plant-a.
    author
        .put_layer("20-site.plant-a", &[("apps/web/params.yaml", "greeting: from-plant-a\n")])
        .await;

    let provider = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();
    agent.tick(&provider).await;

    let deployment = std::fs::read_to_string(
        agent.store.current_path().join("apps/web/deployment.yaml"),
    )
    .unwrap();
    assert!(deployment.contains("from-plant-a"), "site layer merged into render: {deployment}");
    assert_eq!(author.deployment_state("dev-1").await.as_deref(), Some("installed"));
}

// ---------------------------------------------------------------------
// 2. Containment: fleet -> site is a tree; mixed pairs are rejected.
// ---------------------------------------------------------------------

/// §11.1/§11.3 containment: a site belongs to exactly one fleet. A device
/// in fleet B cannot take a site that belongs to fleet A (422); a valid
/// pair works; a site name is unique within its fleet (409) but may recur
/// under a different fleet (201).
#[tokio::test]
async fn containment_rejects_mixed_fleet_site_pairs() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let north = fleet_with_sites(&author, "north", &["plant-a"]).await;
    let south = fleet_with_sites(&author, "south", &[]).await;
    let token = enroll_device(&srv.state, "dev-1", None);
    let _ = token;

    // Put the device in fleet south (fleet-only, south exists → OK).
    let (st, _) = author.patch_device("dev-1", json!({ "fleet": "south" })).await;
    assert_eq!(st, StatusCode::OK);

    // plant-a belongs to north, not south → mixed pair, 422.
    let (st, body) = author.patch_device("dev-1", json!({ "site": "plant-a" })).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "mixed pair must be 422: {body}");

    // A valid pair (a site under south) works.
    let (st, body) = author.create_group("site", "plant-s", Some(south)).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let (st, _) = author.patch_device("dev-1", json!({ "site": "plant-s" })).await;
    assert_eq!(st, StatusCode::OK, "valid (south, plant-s) accepted");

    // Duplicate site name under the SAME fleet → 409.
    let (st, _) = author.create_group("site", "plant-a", Some(north)).await;
    assert_eq!(st, StatusCode::CONFLICT, "duplicate site under its fleet is 409");

    // Same site name under a DIFFERENT fleet → 201 (distinct site).
    let (st, _) = author.create_group("site", "plant-a", Some(south)).await;
    assert_eq!(st, StatusCode::CREATED, "same name under another fleet is allowed");
}

// ---------------------------------------------------------------------
// 3. Deploy to a Site: only that site's devices get the stack.
// ---------------------------------------------------------------------

/// §11.4: deploying a stack to Site plant-a reaches a device in plant-a
/// (its provider brings the app up) and NOT a device in plant-b (nothing
/// converges).
#[tokio::test]
async fn deploy_to_site_reaches_only_that_site() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    fleet_with_sites(&author, "north", &["plant-a", "plant-b"]).await;
    vendor_web(&author).await;
    let ta = enroll_device(&srv.state, "dev-a", None);
    let tb = enroll_device(&srv.state, "dev-b", None);
    author.patch_device("dev-a", json!({ "fleet": "north", "site": "plant-a" })).await;
    author.patch_device("dev-b", json!({ "fleet": "north", "site": "plant-b" })).await;

    let (st, body) = author.deploy(stack("web"), site_scope("plant-a")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["scope"], "Site plant-a");

    let pa = FakeProvider::new();
    let mut agent_a = TestAgent::http(&srv.base(), "dev-a", &ta);
    agent_a.recover();
    agent_a.tick(&pa).await;

    let pb = FakeProvider::new();
    let mut agent_b = TestAgent::http(&srv.base(), "dev-b", &tb);
    agent_b.recover();
    agent_b.tick(&pb).await;

    assert_eq!(pa.up_count("web"), 1, "plant-a device got the stack");
    assert_eq!(author.deployment_state("dev-a").await.as_deref(), Some("installed"));
    assert_eq!(pb.up_count("web"), 0, "plant-b device did not");
    assert!(author.device_deployments("dev-b").await.is_empty(), "no deployment on the other site");
}

// ---------------------------------------------------------------------
// 4. MOVE — the bug: a moved device removes the app and it DISAPPEARS.
// ---------------------------------------------------------------------

/// THE regression. Deploy to Site plant-a; a device there converges the
/// app. Move it to Site plant-b — its chain drops the app, the agent
/// downs it (FakeProvider records the down) and reports terminal
/// `removed`. The server must then NOT list that deployment for the
/// device.
///
/// Before the ingest fix (`upsert_current` INSERT..ON CONFLICT only), the
/// `deployment_status_current` row survived at state "removed" and the
/// device stayed stuck showing "removing"/"removed". The fix DELETEs the
/// row on a terminal-removed report, so the deployment disappears while
/// the status_journal keeps the removal on record.
#[tokio::test]
async fn move_device_clears_removed_deployment() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    fleet_with_sites(&author, "north", &["plant-a", "plant-b"]).await;
    vendor_web(&author).await;
    let token = enroll_device(&srv.state, "dev-1", None);
    author.patch_device("dev-1", json!({ "fleet": "north", "site": "plant-a" })).await;
    author.deploy(stack("web"), site_scope("plant-a")).await;

    let provider = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();
    agent.tick(&provider).await;
    assert_eq!(provider.up_count("web"), 1, "converged the deployed app");
    assert_eq!(author.deployment_state("dev-1").await.as_deref(), Some("installed"));
    let deps_before = author.device_deployments("dev-1").await;
    assert_eq!(deps_before.len(), 1, "one current deployment before the move");

    // Move to plant-b: web is deployed only to plant-a, so the chain drops
    // it and the render bundle empties.
    let (st, _) = author.patch_device("dev-1", json!({ "site": "plant-b" })).await;
    assert_eq!(st, StatusCode::OK);

    let out = agent.tick(&provider).await;
    assert!(out.acted.iter().any(|a| a == "web"), "converge acted on the removed app: {:?}", out.acted);
    assert_eq!(provider.downs(), ["web"], "provider brought the app down");

    // The fix: the removed deployment is gone from the device's current
    // deployments (not lingering at "removed").
    let deps_after = author.device_deployments("dev-1").await;
    assert!(
        deps_after.is_empty(),
        "removed deployment must disappear from current deployments, got {deps_after:?}"
    );
    assert_eq!(author.deployment_state("dev-1").await, None);

    // History is intact: the removal is still journaled forensically.
    let (st, journal) = author.get_json("/api/devices/dev-1/journal").await;
    assert_eq!(st, StatusCode::OK);
    let records = journal["records"].as_array().cloned().unwrap_or_default();
    let removed_journaled = records.iter().any(|r| {
        r["kind"] == "status"
            && r["payload"]["status"]["state"] == "removed"
    });
    assert!(removed_journaled, "the removal stays in the status journal: {records:?}");
}

// ---------------------------------------------------------------------
// 5. UNDEPLOY: removing a deployment from a scope removes it from devices.
// ---------------------------------------------------------------------

/// §11.4: undeploy is the same call removing the app from the scope. The
/// device converges the removal and the deployment disappears from its
/// current deployments.
#[tokio::test]
async fn undeploy_removes_app_from_scope_devices() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    fleet_with_sites(&author, "north", &["plant-a"]).await;
    vendor_web(&author).await;
    let token = enroll_device(&srv.state, "dev-1", None);
    author.patch_device("dev-1", json!({ "fleet": "north", "site": "plant-a" })).await;
    author.deploy(stack("web"), site_scope("plant-a")).await;

    let provider = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();
    agent.tick(&provider).await;
    assert_eq!(provider.up_count("web"), 1);

    let (st, body) = author.undeploy(stack("web"), site_scope("plant-a")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true);

    agent.tick(&provider).await;
    assert_eq!(provider.downs(), ["web"], "undeploy downed the app");
    assert!(author.device_deployments("dev-1").await.is_empty(), "deployment cleared after undeploy");
}

// ---------------------------------------------------------------------
// 7. PIN: a pinned device is not moved by a new deploy.
// ---------------------------------------------------------------------

/// §11.3 pin: a pinned device holds its current desired config and is
/// excluded from new deploys — a stack deployed after the pin reaches an
/// unpinned peer but not the pinned box.
#[tokio::test]
async fn pinned_device_holds_through_new_deploy() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    fleet_with_sites(&author, "north", &["plant-a"]).await;
    vendor_web(&author).await;
    vendor_named(&author, "extra").await;
    let ta = enroll_device(&srv.state, "dev-a", None);
    let tb = enroll_device(&srv.state, "dev-b", None);
    author.patch_device("dev-a", json!({ "fleet": "north", "site": "plant-a" })).await;
    author.patch_device("dev-b", json!({ "fleet": "north", "site": "plant-a" })).await;
    author.deploy(stack("web"), site_scope("plant-a")).await;

    let pa = FakeProvider::new();
    let mut agent_a = TestAgent::http(&srv.base(), "dev-a", &ta);
    agent_a.recover();
    agent_a.tick(&pa).await;
    let pb = FakeProvider::new();
    let mut agent_b = TestAgent::http(&srv.base(), "dev-b", &tb);
    agent_b.recover();
    agent_b.tick(&pb).await;
    assert_eq!(pa.up_count("web"), 1);
    assert_eq!(pb.up_count("web"), 1);

    // Pin dev-a, then deploy a NEW stack to the same site.
    let (st, _) = author.patch_device("dev-a", json!({ "pinned": true })).await;
    assert_eq!(st, StatusCode::OK);
    author.deploy(stack("extra"), site_scope("plant-a")).await;

    let out_a = agent_a.tick(&pa).await;
    agent_b.tick(&pb).await;

    assert!(matches!(out_a.poll, PollOutcome::NotModified), "pinned device holds: {:?}", out_a.poll);
    assert_eq!(pa.up_count("extra"), 0, "pinned device did not pick up the new deploy");
    assert_eq!(pb.up_count("extra"), 1, "unpinned peer did");
}

// ---------------------------------------------------------------------
// 8. RENAME + TAGS round-trip.
// ---------------------------------------------------------------------

/// §11.2/§11.3: displayName and free-form tags round-trip on GET
/// /api/devices (tags mirrored under the `labels` alias).
#[tokio::test]
async fn rename_and_tags_round_trip() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let _ = enroll_device(&srv.state, "dev-1", None);

    let (st, _) = author
        .patch_device(
            "dev-1",
            json!({ "displayName": "Front Desk", "tags": { "env": "prod", "role": "kiosk" } }),
        )
        .await;
    assert_eq!(st, StatusCode::OK);

    let devices = author.devices().await;
    let dev = devices.iter().find(|d| d["deviceId"] == "dev-1").unwrap();
    assert_eq!(dev["displayName"], "Front Desk");
    assert_eq!(dev["tags"]["env"], "prod");
    assert_eq!(dev["tags"]["role"], "kiosk");
    assert_eq!(dev["labels"]["env"], "prod", "tags mirror the labels alias");
}

// ---------------------------------------------------------------------
// 9. DECOMMISSION: revoke credential + drop the served manifest.
// ---------------------------------------------------------------------

/// §11.3 decommission: the device credential is revoked (device auth
/// 401) and the served manifest is dropped (render provenance null).
/// Idempotent.
#[tokio::test]
async fn decommission_revokes_credential_and_manifest() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", None);
    // Give the device a manifest to drop.
    e2e::author_web_app(&author).await;
    let (_, before) = author.get_json("/api/devices/dev-1").await;
    assert!(!before["render"].is_null(), "device has a served manifest before decommission");
    // Credential works before.
    assert_ne!(author.manifest_poll_status(&token).await, StatusCode::UNAUTHORIZED);

    assert_eq!(author.decommission("dev-1").await, StatusCode::NO_CONTENT);

    // Device auth is cut off; the served manifest is gone.
    assert_eq!(author.manifest_poll_status(&token).await, StatusCode::UNAUTHORIZED);
    let (_, after) = author.get_json("/api/devices/dev-1").await;
    assert!(after["render"].is_null(), "served manifest dropped on decommission");

    // Idempotent.
    assert_eq!(author.decommission("dev-1").await, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------
// 10. GROUP management: create / rename / delete + in-use refusal.
// ---------------------------------------------------------------------

/// §11.3 group API: the tree reads back nested; rename/delete of a group
/// a device references is refused (409); an unreferenced group renames
/// and deletes cleanly.
#[tokio::test]
async fn group_management_and_in_use_refusal() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let north = fleet_with_sites(&author, "north", &["plant-a"]).await;
    let west = fleet_with_sites(&author, "west", &[]).await;
    let plant_a = {
        let tree = author.groups().await;
        tree["fleets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "north")
            .and_then(|f| f["sites"].as_array())
            .and_then(|s| s.iter().find(|s| s["name"] == "plant-a"))
            .unwrap()["id"]
            .as_i64()
            .unwrap()
    };

    // The tree reads back nested: north contains plant-a.
    let tree = author.groups().await;
    let north_node = tree["fleets"].as_array().unwrap().iter().find(|f| f["name"] == "north").unwrap();
    let sites: Vec<&str> = north_node["sites"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(sites, ["plant-a"]);

    // A device references north/plant-a.
    let _ = enroll_device(&srv.state, "dev-1", None);
    author.patch_device("dev-1", json!({ "fleet": "north", "site": "plant-a" })).await;

    // Rename/delete of an in-use group is refused (409).
    assert_eq!(author.rename_group(north, "north2").await.0, StatusCode::CONFLICT, "in-use fleet rename refused");
    assert_eq!(author.delete_status(&format!("/api/groups/{plant_a}")).await, StatusCode::CONFLICT, "in-use site delete refused");
    assert_eq!(author.delete_status(&format!("/api/groups/{north}")).await, StatusCode::CONFLICT, "fleet with child/device refused");

    // An unreferenced fleet renames cleanly.
    assert_eq!(author.rename_group(west, "west2").await.0, StatusCode::OK);

    // Reassign the device away, then the groups delete cleanly.
    author.patch_device("dev-1", json!({ "fleet": null, "site": null })).await;
    assert_eq!(author.delete_status(&format!("/api/groups/{plant_a}")).await, StatusCode::NO_CONTENT);
    assert_eq!(author.delete_status(&format!("/api/groups/{north}")).await, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------
// 11. HISTORY + UNDO: human summary; undo restores prior config.
// ---------------------------------------------------------------------

/// §11.5: a deploy shows a human summary in History; Undo authors a new
/// change restoring prior content — the device converges the reversal and
/// the app is removed.
#[tokio::test]
async fn history_summary_and_undo_restore() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    fleet_with_sites(&author, "north", &["plant-a"]).await;
    vendor_web(&author).await;
    let token = enroll_device(&srv.state, "dev-1", None);
    author.patch_device("dev-1", json!({ "fleet": "north", "site": "plant-a" })).await;
    let (_, dep) = author.deploy(stack("web"), site_scope("plant-a")).await;
    let deploy_rev = dep["revision"].as_i64().unwrap();

    let hist = author.history().await;
    assert_eq!(hist[0]["summary"], "deployed web to Site plant-a");
    assert_eq!(hist[0]["who"], "anonymous");

    let provider = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();
    agent.tick(&provider).await;
    assert_eq!(provider.up_count("web"), 1);

    // Undo restores the config to before the deploy.
    let (st, undo) = author.undo(deploy_rev).await;
    assert_eq!(st, StatusCode::OK, "{undo}");
    assert_eq!(undo["changed"], true);
    let new_rev = undo["revision"].as_i64().unwrap();
    assert!(new_rev > deploy_rev, "undo is a new change on top");

    agent.tick(&provider).await;
    assert_eq!(provider.downs(), ["web"], "undo reversal converged: app downed");
    assert!(author.device_deployments("dev-1").await.is_empty(), "undo cleared the deployment");

    // The undo itself is the newest change.
    let hist = author.history().await;
    assert_eq!(hist[0]["id"].as_i64().unwrap(), new_rev);
}

// ---------------------------------------------------------------------
// 6. ROLLOUT by scope (ext-rollouts): wave advance on healthy, auto-pause
// on failed, rollback as a reverse rollout.
// ---------------------------------------------------------------------

/// §11.5 rollout: a scope-targeted rollout advances wave 0, a device
/// reports healthy, and its gate passes so the wave advances to wave 1.
/// The engine `tick` is driven explicitly (no background loop under
/// tests) so every transition is deterministic.
#[cfg(feature = "ext")]
#[tokio::test]
async fn rollout_by_scope_advances_on_healthy() {
    use reeve_server::ext::rollouts;

    let srv = boot().await;
    let author = Author::new(&srv.base());
    fleet_with_sites(&author, "north", &["plant-a"]).await;
    vendor_web(&author).await;
    let ta = enroll_device(&srv.state, "dev-1", Some("plant-a"));
    let _tb = enroll_device(&srv.state, "dev-2", Some("plant-a"));
    author.deploy(stack("web"), site_scope("plant-a")).await;

    // Two waves so we can watch wave 0 gate and wave 1 begin.
    let (st, body) = author
        .post_json(
            "/api/rollouts",
            &json!({
                "scope": { "kind": "site", "name": "plant-a" },
                "waves": [["dev-1"], ["dev-2"]],
                "gate": { "soakSecs": 0, "gateTimeoutSecs": 0, "undeterminedAllowance": 0 },
            }),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "create rollout: {body}");
    let rollout_id = body["rolloutId"].as_str().unwrap().to_string();

    // dev-1 (wave 0) was advanced on create; converge it and report healthy.
    let pa = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &ta);
    agent.recover();
    agent.tick(&pa).await;
    assert_eq!(pa.up_count("web"), 1, "wave-0 device converged the rollout config");

    // Gate wave 0 (soak 0 → passes), then start + advance wave 1.
    rollouts::tick(&srv.state).unwrap();
    rollouts::tick(&srv.state).unwrap();

    let (st, status) = author.get_json(&format!("/api/rollouts/{rollout_id}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(status["currentWave"], 1, "healthy wave 0 advanced the rollout");
    assert_eq!(status["waves"][0]["state"], "passed", "wave 0 gate passed");
    assert_eq!(status["waves"][0]["devices"][0]["status"], "converged");
}

/// §11.4/§11.5 rollout: a device reporting `failed` auto-pauses the
/// rollout at the failure threshold; rolling it back starts a NEW rollout
/// that reverses to the pre-rollout config.
#[cfg(feature = "ext")]
#[tokio::test]
async fn rollout_auto_pauses_on_failed_then_rolls_back() {
    use reeve_server::ext::rollouts;

    let srv = boot().await;
    let author = Author::new(&srv.base());
    fleet_with_sites(&author, "north", &["plant-a"]).await;
    vendor_web(&author).await;
    let token = enroll_device(&srv.state, "dev-1", Some("plant-a"));
    author.deploy(stack("web"), site_scope("plant-a")).await;

    let (st, body) = author
        .post_json(
            "/api/rollouts",
            &json!({
                "scope": { "kind": "site", "name": "plant-a" },
                "gate": { "soakSecs": 0, "gateTimeoutSecs": 0, "undeterminedAllowance": 0 },
                "failureThreshold": 1,
            }),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "create rollout: {body}");
    let rollout_id = body["rolloutId"].as_str().unwrap().to_string();
    let rollout_revision = body["revision"].as_i64().unwrap();

    // dev-1 was advanced on create; make its convergence FAIL and report it.
    let provider = FakeProvider::new();
    provider.fail_app("web");
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();
    agent.tick(&provider).await;

    // The engine sees a failed device and auto-pauses at the threshold.
    rollouts::tick(&srv.state).unwrap();
    let (st, status) = author.get_json(&format!("/api/rollouts/{rollout_id}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(status["state"], "paused", "failed device auto-paused the rollout");
    assert!(
        status["pauseReason"].as_str().unwrap_or("").contains("failure threshold"),
        "auto-pause reason names the threshold: {}",
        status["pauseReason"]
    );

    // Rollback: a NEW rollout that reverses to the pre-rollout config.
    let (st, rb) = author.post_json(&format!("/api/rollouts/{rollout_id}/rollback"), &json!({})).await;
    assert_eq!(st, StatusCode::CREATED, "rollback: {rb}");
    assert_eq!(rb["rollbackOf"], rollout_id.as_str(), "rollback references the original");
    assert_eq!(rb["state"], "active");
    assert!(
        rb["revision"].as_i64().unwrap() < rollout_revision,
        "rollback targets an earlier (pre-rollout) config"
    );
    assert_ne!(rb["rolloutId"], Value::String(rollout_id), "rollback is a distinct rollout");
}
