//! Tests for reeve extension types (spec/reeve/). Where the reeve
//! spec embeds example payloads, those are extracted and used as
//! fixtures; hand-written fixtures appear only for reeve-only types
//! with no upstream example (permitted by CLAUDE.md "Verification"
//! for reeve-only types).

use std::fs;
use std::path::{Path, PathBuf};

use reeve_types::margo::status::{DeploymentState, DeploymentStatusManifest};
use reeve_types::reeve::capabilities::{ReeveCapabilities, ServerCapabilities, parse_extension};
use reeve_types::reeve::events::{self, SseEvent};
use reeve_types::reeve::health::{JournalAck, JournalBatch, JournalRecordKind, ReeveStatusExtension};
use reeve_types::reeve::manifest::{ManifestVersion, StateManifest, is_sha256_digest};

fn repo_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

fn first_json_block(rel: &str) -> String {
    let md = fs::read_to_string(repo_path(rel))
        .unwrap_or_else(|e| panic!("cannot read {rel}: {e}"));
    let mut block = String::new();
    let mut inside = false;
    for line in md.lines() {
        if inside {
            if line.trim_start().starts_with("```") {
                return block;
            }
            block.push_str(line);
            block.push('\n');
        } else if line.trim_start().starts_with("```json") {
            inside = true;
        }
    }
    panic!("no ```json block in {rel}");
}

// --- capability advertisement (01-framework §3.3) ---------------------------

#[test]
fn capability_advertisement_spec_example() {
    // The §3.3 example payload is the fixture; the `reeve` object is
    // extracted from it (the surrounding block elides Margo fields
    // with a "..." placeholder key, which is exactly the tolerance
    // this crate must have).
    let block = first_json_block("spec/reeve/01-framework.md");
    let v: serde_json::Value = serde_json::from_str(&block).unwrap();
    let caps: ReeveCapabilities =
        serde_json::from_value(v["properties"]["reeve"].clone()).unwrap();
    assert_eq!(caps.agent_version, "0.4.2");
    assert_eq!(caps.extensions, vec!["rev-001/1", "rev-002/1", "rev-004/1"]);
    assert_eq!(
        caps.extensions.iter().map(|e| parse_extension(e).unwrap()).collect::<Vec<_>>(),
        vec![(1, 1), (2, 1), (4, 1)]
    );
    assert!(caps.supports(1, 1));
    assert!(!caps.supports(3, 1));

    // Round-trip.
    let json = serde_json::to_string(&caps).unwrap();
    assert_eq!(serde_json::from_str::<ReeveCapabilities>(&json).unwrap(), caps);
    // Wire spelling is camelCase per the spec example.
    assert!(json.contains("\"agentVersion\""));
}

#[test]
fn server_capabilities_roundtrip() {
    let caps = ServerCapabilities {
        server_version: "0.1.0".into(),
        extensions: vec!["rev-001/1".into(), "rev-003/1".into()],
    };
    let json = serde_json::to_string(&caps).unwrap();
    assert!(json.contains("\"serverVersion\""));
    assert_eq!(serde_json::from_str::<ServerCapabilities>(&json).unwrap(), caps);
    assert_eq!(caps.highest_version(3), Some(1));
}

// --- health payload (05-health-journal §7.3) --------------------------------

/// Remove the spec's ellipsis placeholders (`"...": ...` entries) —
/// elision notation in example blocks, not wire fields.
fn strip_ellipsis(v: &mut serde_json::Value) {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("...");
        for child in obj.values_mut() {
            strip_ellipsis(child);
        }
    }
}

#[test]
fn reeve_status_extension_spec_example() {
    // The §7.3 example is the fixture for the additive `reeve` object.
    let block = first_json_block("spec/reeve/05-health-journal.md");
    let mut v: serde_json::Value = serde_json::from_str(&block).unwrap();
    strip_ellipsis(&mut v);
    let ext: ReeveStatusExtension = serde_json::from_value(v["reeve"].clone()).unwrap();
    assert_eq!(ext.observed_at, "2026-07-10T06:12:03Z");
    assert_eq!(ext.seq, 48211);
    let health = ext.health.as_ref().unwrap();
    assert_eq!(health.agent_version.as_deref(), Some("0.4.2"));
    assert_eq!(health.clock_skew_ms, Some(-120));
    assert_eq!(health.load.as_deref(), Some(&[][..]));

    // Round-trip, including extensible fields captured via flatten.
    let json = serde_json::to_string(&ext).unwrap();
    assert_eq!(serde_json::from_str::<ReeveStatusExtension>(&json).unwrap(), ext);
}

#[test]
fn status_manifest_with_reeve_extension_roundtrips() {
    // Margo-required fields present and unchanged; reeve rides as the
    // single additive key (01-framework §3.1 rule 3, §3.7 audit).
    let raw = r#"{
        "apiVersion": "deployment.margo.org/v1alpha1",
        "kind": "DeploymentStatusManifest",
        "deploymentId": "a3e2f5dc-912e-494f-8395-52cf3769bc06",
        "status": { "state": "installed" },
        "components": [ { "name": "web", "state": "installed" } ],
        "reeve": {
            "observedAt": "2026-07-10T06:12:03Z",
            "seq": 7,
            "health": {
                "disk": { "/var": { "usedBytes": 100, "freeBytes": 900 } },
                "memory": { "usedBytes": 1024, "totalBytes": 4096 },
                "load": [0.5, 0.4, 0.3],
                "restarts": { "web": 2 },
                "agentVersion": "0.4.2",
                "clockSkewMs": -120,
                "someFutureField": { "nested": true }
            },
            "unknownReeveSubField": 42
        }
    }"#;
    let m: DeploymentStatusManifest = serde_json::from_str(raw).unwrap();
    assert_eq!(m.status.state, DeploymentState::Installed);
    let ext = m.reeve.as_ref().unwrap();
    assert_eq!(ext.seq, 7);
    let health = ext.health.as_ref().unwrap();
    assert_eq!(health.disk.as_ref().unwrap()["/var"].used_bytes, Some(100));
    assert_eq!(health.restarts.as_ref().unwrap()["web"], 2);
    // §7.2: extensible sample fields are preserved, not dropped.
    assert!(health.extra.contains_key("someFutureField"));

    let json = serde_json::to_string(&m).unwrap();
    let reparsed: DeploymentStatusManifest = serde_json::from_str(&json).unwrap();
    assert_eq!(m, reparsed);
    // Unknown `reeve` sub-fields tolerated (§3.2) — parse succeeded —
    // though unmodeled sub-fields outside the sample are not re-emitted.
}

#[test]
fn journal_batch_roundtrip() {
    let raw = r#"{
        "records": [
            { "seq": 1, "observedAt": "2026-07-10T06:00:00Z", "kind": "lifecycle",
              "payload": { "event": "start" } },
            { "seq": 2, "observedAt": "2026-07-10T06:01:00Z", "kind": "health",
              "payload": { "load": [0.1, 0.1, 0.0] } },
            { "seq": 3, "observedAt": "2026-07-10T06:02:00Z", "kind": "gap" }
        ]
    }"#;
    let batch: JournalBatch = serde_json::from_str(raw).unwrap();
    assert_eq!(batch.records.len(), 3);
    assert_eq!(batch.records[2].kind, JournalRecordKind::Gap);
    assert!(batch.records[2].payload.is_none());
    let json = serde_json::to_string(&batch).unwrap();
    assert_eq!(serde_json::from_str::<JournalBatch>(&json).unwrap(), batch);

    let ack: JournalAck = serde_json::from_str(r#"{"ackedSeq": 3}"#).unwrap();
    assert_eq!(ack.acked_seq, 3);
}

// --- State Manifest (delivery.md D13, 08-packaging §10.2) -------------------

#[test]
fn state_manifest_roundtrip() {
    let digest = format!("sha256:{}", "ab".repeat(32));
    let raw = format!(
        r#"{{
            "manifestVersion": {},
            "bundle": {{
                "mediaType": "application/vnd.reeve.render-bundle.v1+tar+gzip",
                "digest": "{digest}",
                "sizeBytes": 4096,
                "url": "/v2/reeve/render-bundles/blobs/{digest}"
            }},
            "apps": [
                {{ "appId": "nextcloud-stack",
                   "deploymentId": "2f9e5cbe-0000-4000-8000-000000000001",
                   "secrets_version": "sv-1a2b3c" }},
                {{ "appId": "hello-world" }}
            ]
        }}"#,
        ManifestVersion::pack(1, 42).unwrap().0
    );
    let m: StateManifest = serde_json::from_str(&raw).unwrap();
    assert_eq!(m.manifest_version.unpack(), (1, 42));
    let bundle = m.bundle.as_ref().unwrap();
    assert!(is_sha256_digest(&bundle.digest));
    assert_eq!(m.apps[0].secrets_version.as_deref(), Some("sv-1a2b3c"));
    assert_eq!(m.apps[1].secrets_version, None);

    let json = serde_json::to_string(&m).unwrap();
    assert_eq!(serde_json::from_str::<StateManifest>(&json).unwrap(), m);
    // Wire spelling: `secrets_version` stays snake_case (the exact
    // token the reeve spec uses normatively); refs are camelCase.
    assert!(json.contains("\"secrets_version\""));
    assert!(json.contains("\"manifestVersion\""));
}

#[test]
fn state_manifest_empty_bundle_is_null_never_omitted() {
    // Margo DeploymentBundleRef rule adopted: with zero apps the
    // bundle property MUST be present with the value null.
    let m = StateManifest {
        manifest_version: ManifestVersion::pack(0, 1).unwrap(),
        bundle: None,
        apps: vec![],
    };
    let json = serde_json::to_string(&m).unwrap();
    assert!(json.contains("\"bundle\":null"), "bundle must be emitted as null: {json}");
    assert_eq!(serde_json::from_str::<StateManifest>(&json).unwrap(), m);
}

// --- SSE events (04-status-stream §6.3) -------------------------------------

#[test]
fn sse_events_roundtrip_all_types() {
    use reeve_types::margo::status::DeploymentState;
    use reeve_types::reeve::events::*;
    use reeve_types::reeve::health::{HealthKind, HealthState};

    let ts = "2026-07-10T06:12:03Z".to_string();
    let all = vec![
        SseEvent::Reset(ResetEvent { ts: ts.clone() }),
        SseEvent::DevicePresence(DevicePresenceEvent {
            ts: ts.clone(),
            device_id: "edge-01".into(),
            state: PresenceState::Online,
            since: ts.clone(),
        }),
        SseEvent::DeploymentStatus(DeploymentStatusEvent {
            ts: ts.clone(),
            device_id: "edge-01".into(),
            deployment_id: "a3e2f5dc-912e-494f-8395-52cf3769bc06".into(),
            state: DeploymentState::Installing,
        }),
        SseEvent::TerminalSession(TerminalSessionEvent {
            ts: ts.clone(),
            session_id: "s-1".into(),
            device_id: "edge-01".into(),
            phase: TerminalPhase::Opened,
            user: "operator@example.com".into(),
        }),
        SseEvent::HealthState(HealthStateEvent {
            ts: ts.clone(),
            device_id: "edge-01".into(),
            state: HealthState::Degraded,
            kind: HealthKind::Link,
        }),
        SseEvent::VerifyRestore(VerifyRestoreEvent {
            ts: ts.clone(),
            outcome: VerifyRestoreOutcome::Ok,
            snapshot_ts: ts.clone(),
            detail: None,
        }),
        SseEvent::DurabilityLag(DurabilityLagEvent {
            ts: ts.clone(),
            generation: "2026-07-10T06:00:00Z-3".into(),
            last_seq: 118,
            lag_seconds: 45,
        }),
        SseEvent::Rollout(RolloutEvent {
            ts: ts.clone(),
            rollout_id: "r-9".into(),
            wave: 2,
            phase: RolloutPhase::Gated,
        }),
        SseEvent::SecretRotation(SecretRotationEvent {
            ts: ts.clone(),
            secret_name: "db-password".into(),
            scope: "plant-alfa".into(),
            version: 4,
            state: SecretRotationState::Propagating,
        }),
    ];

    // Every rev-003/1 table row is covered.
    let expected_names = [
        "reset",
        "device-presence",
        "deployment-status",
        "terminal-session",
        "health-state",
        "verify-restore",
        "durability-lag",
        "rollout",
        "secret-rotation",
    ];
    assert_eq!(all.len(), expected_names.len());
    for (event, expected) in all.iter().zip(expected_names) {
        assert_eq!(event.event_type(), expected);
        let data = event.data_json().unwrap();
        // Every payload includes `ts` (§6.3).
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert!(v["ts"].is_string(), "{expected} payload lacks ts: {data}");
        let back = SseEvent::from_wire(event.event_type(), &data).unwrap().unwrap();
        assert_eq!(&back, event);
    }
}

#[test]
fn sse_unknown_event_type_is_ignored() {
    // §6.3: unknown event types MUST be ignored by clients.
    let out = SseEvent::from_wire("shiny-new-thing", r#"{"ts":"2026-07-10T00:00:00Z"}"#).unwrap();
    assert_eq!(out, None);
}

#[test]
fn durability_lag_event_wire_field_names() {
    let raw = r#"{"ts":"2026-07-10T06:12:03Z","generation":"g1","lastSeq":9,"lagSeconds":61}"#;
    let e = SseEvent::from_wire(events::event_type::DURABILITY_LAG, raw).unwrap().unwrap();
    match &e {
        SseEvent::DurabilityLag(p) => {
            assert_eq!(p.last_seq, 9);
            assert_eq!(p.lag_seconds, 61);
        }
        other => panic!("wrong variant: {other:?}"),
    }
    let data = e.data_json().unwrap();
    assert!(data.contains("\"lastSeq\"") && data.contains("\"lagSeconds\""));
}
