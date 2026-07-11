//! The render pipeline (C4): tree revision + device rows -> per-device
//! State Manifests + OCI render-bundle artifacts.
//!
//! Spec sources:
//! - spec/reeve/08-packaging.md §10.2 — State Manifest poll + native
//!   OCI pull, manifestVersion = packed (epoch, counter).
//! - docs/decisions/delivery.md D7 (native read-only OCI serving) and
//!   D13 (revision store in, OCI artifact out, per-app secrets_version).
//! - docs/decisions/tree-render.md D2 (bundle layout, byte-identical
//!   renders) and D3 (no-change re-render => no new bundle, no bump).
//!
//! Change detection: the digest of the rendered `apps/**` file set
//! EXCLUDING `manifest.yaml`. `manifest.yaml` carries provenance
//! (revision ids, generation) that moves with every commit even when
//! this device's desired state does not; hashing it would defeat the
//! D3 no-change rule. When content is unchanged the PREVIOUS bundle —
//! with its previous provenance — stays current: the recorded declared
//! inputs are those of the last MATERIAL render.
//!
//! Crash-only (Law 3): every device update is one SQLite transaction;
//! `reconcile` at startup detects a head committed but not fully
//! rendered (`settings.last_rendered_local`) and re-renders; blob
//! inserts are `INSERT OR IGNORE` (content-addressed, idempotent);
//! unreferenced blobs are purged at startup, never mid-flight.
//!
//! Per-device render targets (C9, spec/reeve/09-rollouts.md §11.2): a
//! `device_render_targets` row (V8) pins a device's render to a
//! specific revision instead of the local head — staged rollouts are
//! nothing but these rows moving (ext/rollouts.rs is the only writer;
//! the mechanism is core so a --no-default-features binary keeps a
//! paused rollout's position stable instead of jumping every device to
//! head). No row = head-tracking, exactly the pre-C9 behavior.

use std::collections::{BTreeMap, BTreeSet};

use desired_state::{FileSet, RenderContext, deployment_id};
use reeve_types::reeve::manifest::{
    AppManifestEntry, BundleRef, ManifestVersion, RENDER_BUNDLE_MEDIA_TYPE, StateManifest,
};
use revision_store::{RevisionId, RevisionStore, Stream, digest_of};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde_json::json;
use tracing::warn;

use crate::db::now_secs;
use crate::state::AppState;

/// OCI image manifest media type (image-spec v1) — what
/// `/v2/…/manifests/<digest>` serves (§10.2: standard OCI distribution
/// pull; stock clients must work, so we speak stock shapes).
pub const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
/// OCI empty descriptor media type (image-spec v1 §guidance for
/// artifacts): the config slot of a render-bundle artifact.
pub const OCI_EMPTY_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";
/// The empty config blob (`{}`) every render-bundle artifact's config
/// descriptor points at (OCI image-spec "empty descriptor" guidance).
pub const EMPTY_CONFIG_BLOB: &[u8] = b"{}";

/// Repo naming DECISION: device render bundles live at
/// `reeve/bundles/<device_id>` under the native /v2 space
/// (docs/decisions/delivery.md D7). `StateManifest.bundle.url` is the
/// server-relative repo base, per the agent's B2 contract.
pub fn repo_url_for_device(device_id: &str) -> String {
    format!("/v2/reeve/bundles/{device_id}")
}

/// Digest of the shared empty config blob.
pub fn empty_config_digest() -> String {
    digest_of(EMPTY_CONFIG_BLOB)
}

/// Errors from the pipeline that are NOT per-device render errors
/// (those degrade to keep-last-good; these are infrastructure faults).
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("revision store: {0}")]
    Store(#[from] revision_store::Error),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("bundle packing: {0}")]
    Pack(#[from] std::io::Error),
    #[error("manifest encoding: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("manifest version: {0}")]
    Version(#[from] reeve_types::reeve::manifest::ManifestVersionError),
    #[error("unknown device {0}")]
    UnknownDevice(String),
}

/// Outcome of rendering one device.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Content digest unchanged: no new bundle, no manifestVersion bump
    /// (D3). `rendered_revision` was advanced so polls stay cheap.
    Unchanged,
    /// New content: bundle stored, manifest row updated, version bumped.
    Updated(ManifestVersion),
    /// desired-state refused this device's tree (authoring error).
    /// The device's previous manifest stays current (keep-last-good);
    /// the error is surfaced in the render report and logs.
    Failed(String),
}

/// Result of a full render pass.
#[derive(Debug, Default)]
pub struct RenderReport {
    pub rendered: usize,
    pub unchanged: usize,
    /// (device_id, error) for devices whose render failed.
    pub failed: Vec<(String, String)>,
}

/// One device row's render-relevant fields (tree-render.md D11: layer
/// chain membership comes from the device row, not tree content).
struct DeviceRow {
    device_id: String,
    class: Option<String>,
    region: Option<String>,
    site: Option<String>,
}

impl DeviceRow {
    /// fleet -> class? -> region? -> site? -> device (D11/D12). The
    /// numeric prefixes make D3's merge order lexically sortable.
    fn layer_chain(&self) -> Vec<String> {
        let mut chain = vec!["00-fleet".to_string()];
        if let Some(c) = &self.class {
            chain.push(format!("05-class.{c}"));
        }
        if let Some(r) = &self.region {
            chain.push(format!("10-region.{r}"));
        }
        if let Some(s) = &self.site {
            chain.push(format!("20-site.{s}"));
        }
        chain.push(format!("30-device.{}", self.device_id));
        chain
    }
}

/// Settings key marking that an OUT-OF-BAND render input changed (a
/// secrets write, C7 — spec/reeve/10-secrets.md §12.4) and the
/// propagating render pass may not have completed. Writers set it in
/// the SAME transaction as the input write; [`render_all`] clears it
/// when a full pass completes under the same db lock (Law 3: a kill -9
/// between the write and the pass leaves the flag, and startup
/// [`reconcile`] runs the pass).
pub const RENDER_DIRTY_KEY: &str = "render_dirty";

/// Server epoch — high 16 bits of every manifestVersion. Stored in
/// `settings.server_epoch`; absent means 0. Durability restore fencing
/// (C6, spec/reeve/07-durability.md §9.5) owns bumping it.
pub fn server_epoch(conn: &Connection) -> Result<u16, rusqlite::Error> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'server_epoch'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
}

/// Deterministic digest of the rendered file set EXCLUDING
/// `manifest.yaml` (see module docs): length-prefixed (path, bytes)
/// pairs in BTreeMap order, so identical content => identical digest,
/// independent of tar/gzip encoding details.
pub fn content_digest(files: &FileSet) -> String {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    for (path, bytes) in files {
        if path == "manifest.yaml" {
            continue;
        }
        h.update((path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    format!("sha256:{:x}", h.finalize())
}

/// Fold a device's per-app secrets_version map into one change
/// detector value (spec/reeve/10-secrets.md §12.4 — a rotation must
/// bump manifestVersion even though no rendered byte changed). `None`
/// when no rendered app references a secret (and always in core
/// builds), so existing rows see no spurious bump.
fn secrets_digest(secret_versions: &BTreeMap<String, String>) -> Option<String> {
    if secret_versions.is_empty() {
        return None;
    }
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    for (app, sv) in secret_versions {
        h.update((app.len() as u64).to_le_bytes());
        h.update(app.as_bytes());
        h.update((sv.len() as u64).to_le_bytes());
        h.update(sv.as_bytes());
    }
    Some(format!("sha256:{:x}", h.finalize()))
}

/// Pack a rendered file set as a canonical/deterministic tar.gz
/// (docs/decisions/tree-render.md D2: byte-identical bundles for
/// identical file sets): entries in sorted (BTreeMap) order, zeroed
/// mtime/uid/gid, fixed 0644 mode, gzip mtime 0.
pub fn pack_bundle(files: &FileSet) -> std::io::Result<Vec<u8>> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    for (path, content) in files {
        let mut header = tar::Header::new_ustar();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut header, path, content.as_slice())?;
    }
    tar.into_inner()?.finish()
}

/// Load the whole tree at `head` into memory (render input shape,
/// desired-state `FileSet`). `None` head => empty tree.
fn load_tree(store: &RevisionStore, head: Option<RevisionId>) -> Result<FileSet, PipelineError> {
    let mut tree = FileSet::new();
    if let Some(id) = head {
        for (path, digest) in store.tree_at(id)? {
            let bytes = store.blob(&digest)?.ok_or_else(|| {
                revision_store::Error::Corrupt(format!("missing blob {digest} for {path}"))
            })?;
            tree.insert(path, bytes);
        }
    }
    Ok(tree)
}

/// App names present in a rendered file set (`apps/<name>/…`, D2: one
/// app dir = one unit of convergence).
fn app_names(files: &FileSet) -> BTreeSet<String> {
    files
        .keys()
        .filter_map(|k| k.strip_prefix("apps/"))
        .filter_map(|rest| rest.split('/').next())
        .map(str::to_string)
        .collect()
}

/// The State Manifest `apps` list for one device. `secrets_version` is
/// present iff the app's rendered deployment.yaml references secrets
/// (spec/reeve/10-secrets.md §12.4, ext-secrets/C7); always absent in
/// core builds.
fn app_entries(
    apps: &BTreeSet<String>,
    dev: &DeviceRow,
    secret_versions: &BTreeMap<String, String>,
) -> Vec<AppManifestEntry> {
    apps.iter()
        .map(|a| AppManifestEntry {
            app_id: a.clone(),
            deployment_id: Some(deployment_id(&dev.device_id, a).to_string()),
            secrets_version: secret_versions.get(a).cloned(),
        })
        .collect()
}

/// The upstream-stream head a render pass merges under the local tree
/// (spec/reeve/06-federation.md §8.2 two-stream render input):
/// `(local row id, parent-tier origin id)`. `None` on a root with no
/// synced revisions.
pub type UpstreamHead = Option<(RevisionId, RevisionId)>;

/// Render one device against an already-loaded MERGED tree (upstream
/// layers under local ones, §8.2), inside the given connection. One
/// transaction per updated device (Law 3).
fn render_one(
    conn: &mut Connection,
    tree: &FileSet,
    head: RevisionId,
    upstream: UpstreamHead,
    epoch: u16,
    registry_endpoint: &str,
    dev: &DeviceRow,
) -> Result<Outcome, PipelineError> {
    let stored: Option<(i64, i64, String, Option<String>, String)> = conn
        .query_row(
            "SELECT counter, generation, content_digest, secrets_digest, manifest_json
             FROM device_manifests WHERE device_id = ?1",
            params![dev.device_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (counter, generation, prev_content, prev_secrets) = match &stored {
        Some((c, g, d, s, _)) => (*c, *g, Some(d.as_str()), s.clone()),
        None => (0, 0, None, None),
    };
    let upstream_row = upstream.map(|(row, _)| row);

    let ctx = RenderContext {
        device_id: dev.device_id.clone(),
        layers: dev.layer_chain(),
        registry_endpoint: registry_endpoint.to_string(),
        generation: (generation + 1) as u64,
        local_revision: head.max(0) as u64,
        // manifest.yaml records BOTH revision ids (D2/§8.2); the hub id
        // is the PARENT tier's own revision id (origin), so provenance
        // reads the same at every tier.
        hub_revision: upstream.map(|(_, origin)| origin.max(0) as u64),
    };
    let out = match desired_state::render(tree, &ctx) {
        Ok(o) => o,
        // Keep-last-good: an authoring error for this device leaves its
        // previous manifest current; rendered_revision is NOT advanced,
        // so the next poll/pass retries (self-healing once fixed).
        Err(e) => return Ok(Outcome::Failed(e.to_string())),
    };

    // ext-secrets render hook (spec/reeve/10-secrets.md §12.4): per-app
    // secrets_version = hash of the (name, version) pairs this app's
    // rendered `${secret:<name>}` references resolve to down THIS
    // device's chain. It joins the change-detection digest below, so a
    // rotation bumps manifestVersion for exactly the referencing
    // devices — with the bundle digest unchanged (no re-pull, agent
    // does a minimal re-up per B4). Core builds (--no-default-features)
    // compute nothing and leave secretsVersion absent.
    #[cfg(feature = "ext-secrets")]
    let secret_versions: BTreeMap<String, String> = crate::ext::secrets::app_secrets_versions(
        conn,
        &out,
        &crate::ext::secrets::device_chain(
            &dev.device_id,
            dev.class.as_deref(),
            dev.region.as_deref(),
            dev.site.as_deref(),
        ),
    )?;
    #[cfg(not(feature = "ext-secrets"))]
    let secret_versions: BTreeMap<String, String> = BTreeMap::new();

    let cdig = content_digest(&out);
    let sdig = secrets_digest(&secret_versions);
    if prev_content == Some(cdig.as_str()) && prev_secrets == sdig {
        // D3: no-change re-render => no new bundle, no bump. Advance
        // only the revision bookkeeping so this pass isn't repeated.
        conn.execute(
            "UPDATE device_manifests
             SET rendered_revision = ?2, rendered_upstream = ?3, updated_at = ?4
             WHERE device_id = ?1",
            params![dev.device_id, head, upstream_row, now_secs()],
        )?;
        return Ok(Outcome::Unchanged);
    }

    // Change of SOME kind: allocate the next manifestVersion (per-device
    // counter, monotonic; epoch from settings — §10.2 anti-rollback).
    let next_counter = counter + 1;
    let version = ManifestVersion::pack(epoch, next_counter as u64)?;
    let apps = app_names(&out);

    if prev_content == Some(cdig.as_str()) {
        // Secrets-only change (spec/reeve/10-secrets.md §12.4): no
        // rendered byte moved, only resolved secret versions did. Keep
        // the PREVIOUS bundle — digest, layer, generation, provenance —
        // verbatim, and rewrite just the State Manifest: new
        // manifestVersion + per-app secretsVersion. The agent sees
        // "bundle digest unchanged + secrets_version changed" and does a
        // re-resolve + minimal re-up, no re-pull (B4).
        let (_, _, _, _, prev_json) = stored.as_ref().expect("prev_content implies a stored row");
        let prev_manifest: StateManifest = serde_json::from_str(prev_json)?;
        let manifest = StateManifest {
            manifest_version: version,
            bundle: prev_manifest.bundle,
            apps: app_entries(&apps, dev, &secret_versions),
        };
        let manifest_json = serde_json::to_vec(&manifest)?;
        let etag = digest_of(&manifest_json);
        conn.execute(
            "UPDATE device_manifests
             SET manifest_version = ?2, counter = ?3, secrets_digest = ?4,
                 manifest_json = ?5, etag = ?6, rendered_revision = ?7,
                 rendered_upstream = ?8, updated_at = ?9
             WHERE device_id = ?1",
            params![
                dev.device_id,
                version.0 as i64,
                next_counter,
                sdig,
                String::from_utf8(manifest_json).expect("serde_json emits UTF-8"),
                etag,
                head,
                upstream_row,
                now_secs(),
            ],
        )?;
        return Ok(Outcome::Updated(version));
    }
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    // The bundle carries apps AND bundle-level config (config/**, e.g.
    // the remote-terminal enable file). Build it whenever the render has
    // any content beyond manifest.yaml — a config-only tree (terminal
    // enabled, zero apps) still needs its config delivered. Only a truly
    // empty render (manifest.yaml alone) yields the Margo null bundle.
    let bundle_empty = out.keys().all(|k| k == "manifest.yaml");
    let (bundle_ref, bundle_digest, layer_digest) = if bundle_empty {
        // Margo DeploymentBundleRef null rule: nothing to deliver =>
        // bundle is present with the value null (reeve-types StateManifest doc).
        (None, None, None)
    } else {
        let tarball = pack_bundle(&out)?;
        let layer_digest = digest_of(&tarball);
        let config_digest = empty_config_digest();
        // Stock OCI image manifest (image-spec v1) with artifactType +
        // empty config: pullable by oras/skopeo/crane (§10.2).
        // serde_json maps are sorted => deterministic bytes.
        let oci = json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST_MEDIA_TYPE,
            "artifactType": RENDER_BUNDLE_MEDIA_TYPE,
            "config": {
                "mediaType": OCI_EMPTY_MEDIA_TYPE,
                "digest": config_digest,
                "size": EMPTY_CONFIG_BLOB.len(),
            },
            "layers": [{
                "mediaType": RENDER_BUNDLE_MEDIA_TYPE,
                "digest": layer_digest,
                "size": tarball.len(),
            }],
        });
        let oci_bytes = serde_json::to_vec(&oci)?;
        let manifest_digest = digest_of(&oci_bytes);
        let bundle = BundleRef {
            media_type: Some(RENDER_BUNDLE_MEDIA_TYPE.to_string()),
            digest: manifest_digest.clone(),
            size_bytes: Some(oci_bytes.len() as u64),
            url: repo_url_for_device(&dev.device_id),
        };
        blobs.push((layer_digest.clone(), tarball));
        blobs.push((manifest_digest.clone(), oci_bytes));
        blobs.push((config_digest, EMPTY_CONFIG_BLOB.to_vec()));
        (Some(bundle), Some(manifest_digest), Some(layer_digest))
    };

    let manifest = StateManifest {
        manifest_version: version,
        bundle: bundle_ref,
        apps: app_entries(&apps, dev, &secret_versions),
    };
    let manifest_json = serde_json::to_vec(&manifest)?;
    let etag = digest_of(&manifest_json);

    // One transaction: blobs + manifest row (Law 3 — kill -9 leaves
    // either the previous manifest or the complete next one).
    let tx = conn.transaction()?;
    let now = now_secs();
    for (digest, content) in &blobs {
        tx.execute(
            "INSERT OR IGNORE INTO bundle_blobs (digest, content, created_at)
             VALUES (?1, ?2, ?3)",
            params![digest, content, now],
        )?;
    }
    tx.execute(
        "INSERT INTO device_manifests
             (device_id, manifest_version, counter, generation, content_digest,
              secrets_digest, bundle_digest, layer_digest, manifest_json, etag,
              rendered_revision, rendered_upstream, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(device_id) DO UPDATE SET
             manifest_version = excluded.manifest_version,
             counter = excluded.counter,
             generation = excluded.generation,
             content_digest = excluded.content_digest,
             secrets_digest = excluded.secrets_digest,
             bundle_digest = excluded.bundle_digest,
             layer_digest = excluded.layer_digest,
             manifest_json = excluded.manifest_json,
             etag = excluded.etag,
             rendered_revision = excluded.rendered_revision,
             rendered_upstream = excluded.rendered_upstream,
             updated_at = excluded.updated_at",
        params![
            dev.device_id,
            version.0 as i64,
            next_counter,
            generation + 1,
            cdig,
            sdig,
            bundle_digest,
            layer_digest,
            String::from_utf8(manifest_json).expect("serde_json emits UTF-8"),
            etag,
            head,
            upstream_row,
            now,
        ],
    )?;
    tx.commit()?;
    Ok(Outcome::Updated(version))
}

/// Render-eligible device row: locally-enrolled only. Devices that
/// appeared via forwarded status ingest (`tier_origin` set — federation
/// §8.3) converge against THEIR OWN tier and are never rendered or
/// served desired state here (§8.6: an agent has exactly one server).
fn device_row(conn: &Connection, device_id: &str) -> Result<Option<DeviceRow>, rusqlite::Error> {
    conn.query_row(
        "SELECT device_id, class, region, site FROM devices
         WHERE device_id = ?1 AND tier_origin IS NULL",
        params![device_id],
        |r| {
            Ok(DeviceRow {
                device_id: r.get(0)?,
                class: r.get(1)?,
                region: r.get(2)?,
                site: r.get(3)?,
            })
        },
    )
    .optional()
}

/// The pinned render revision for one device
/// (spec/reeve/09-rollouts.md §11.2): `Some` while a rollout holds or
/// advances this device, `None` = head-tracking.
pub fn device_target(
    conn: &Connection,
    device_id: &str,
) -> Result<Option<RevisionId>, rusqlite::Error> {
    conn.query_row(
        "SELECT revision FROM device_render_targets WHERE device_id = ?1",
        params![device_id],
        |r| r.get(0),
    )
    .optional()
}

/// All per-device render targets (one snapshot for a full pass).
fn all_targets(conn: &Connection) -> Result<BTreeMap<String, RevisionId>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT device_id, revision FROM device_render_targets")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Load one revision's full tree under the revisions lock. Revision 0
/// (or a store with no head) is the empty tree.
fn tree_at_revision(
    store: &RevisionStore,
    revision: RevisionId,
) -> Result<FileSet, PipelineError> {
    load_tree(store, (revision > 0).then_some(revision))
}

/// Merge the upstream tree under a local tree
/// (spec/reeve/06-federation.md §8.2 render input): layers are
/// path-ordered, so the union IS the single merged tree view — the
/// upstream stream provides hub-owned layers (fleet/class/region),
/// the local stream this tier's own (site/device). Overlapping paths
/// are impossible by ownership (§8.4); if one appears anyway (storage
/// corruption, misbehaving parent) the LOCAL side wins and we warn —
/// visible, never silent.
fn merge_streams(upstream: &FileSet, local: FileSet) -> FileSet {
    let mut merged = upstream.clone();
    for (path, bytes) in local {
        if merged.contains_key(&path) {
            warn!(%path, "path present in BOTH streams — ownership violation (federation §8.4); local wins");
        }
        merged.insert(path, bytes);
    }
    merged
}

/// Read both stream heads + the MERGED trees a pass needs (local head
/// plus every distinct pinned revision, each merged over the upstream
/// head — §8.2) under the revisions lock, releasing it before any DB
/// work (locks are short, never held together longer than needed).
fn snapshot_trees(
    state: &AppState,
    targets: &BTreeMap<String, RevisionId>,
) -> Result<(RevisionId, UpstreamHead, BTreeMap<RevisionId, FileSet>), PipelineError> {
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    let head = store.head(Stream::Local)?.unwrap_or(0);
    let upstream = store.origin_head(Stream::Upstream)?;
    let upstream_tree = match upstream {
        Some((row, _)) => tree_at_revision(&store, row)?,
        None => FileSet::new(),
    };
    let mut trees = BTreeMap::new();
    trees.insert(head, merge_streams(&upstream_tree, tree_at_revision(&store, head)?));
    for revision in targets.values() {
        if !trees.contains_key(revision) {
            trees.insert(
                *revision,
                merge_streams(&upstream_tree, tree_at_revision(&store, *revision)?),
            );
        }
    }
    Ok((head, upstream, trees))
}

/// Render every enrolled device against the current local head. Called
/// after every changed authoring commit, from POST /api/render, and
/// from startup reconcile. Records `settings.last_rendered_local` when
/// the pass completes (per-device Failed outcomes do not block it —
/// they are authoring errors that only a new commit can fix, and
/// per-device `rendered_revision` staying behind retries them on poll).
pub fn render_all(state: &AppState) -> Result<RenderReport, PipelineError> {
    // Target snapshot first (db), trees second (revisions) — the
    // one-direction lock rule (state.rs). A target row written between
    // the snapshot and the loop renders once at the stale revision; the
    // writer (ext/rollouts.rs) follows every target move with its own
    // ensure_current, and the device's next poll self-corrects anyway.
    let targets = {
        let conn = state.db.lock().expect("db mutex poisoned");
        all_targets(&conn)?
    };
    let (head, upstream, trees) = snapshot_trees(state, &targets)?;

    let mut conn = state.db.lock().expect("db mutex poisoned");
    let epoch = server_epoch(&conn)?;
    let devices: Vec<DeviceRow> = {
        // tier_origin IS NULL: forwarded devices are status-only here
        // (federation §8.3/§8.6) — see device_row.
        let mut stmt =
            conn.prepare("SELECT device_id, class, region, site FROM devices
                          WHERE tier_origin IS NULL ORDER BY device_id")?;
        let rows = stmt.query_map([], |r| {
            Ok(DeviceRow {
                device_id: r.get(0)?,
                class: r.get(1)?,
                region: r.get(2)?,
                site: r.get(3)?,
            })
        })?;
        rows.collect::<Result<_, _>>()?
    };

    let mut report = RenderReport::default();
    let mut updated: Vec<String> = Vec::new();
    for dev in &devices {
        // §11.2: a held/advancing device renders at ITS revision, not
        // head — the rollout engine times manifest movement, nothing
        // else changes.
        let effective = targets.get(&dev.device_id).copied().unwrap_or(head);
        let tree = trees
            .get(&effective)
            .expect("snapshot_trees loaded every effective revision");
        match render_one(&mut conn, tree, effective, upstream, epoch, &state.cfg.registry_endpoint, dev)? {
            Outcome::Unchanged => report.unchanged += 1,
            Outcome::Updated(_) => {
                report.rendered += 1;
                updated.push(dev.device_id.clone());
            }
            Outcome::Failed(e) => {
                warn!(device = %dev.device_id, error = %e, "render failed; keeping last good manifest");
                report.failed.push((dev.device_id.clone(), e));
            }
        }
    }

    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('last_rendered_local', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![head.to_string()],
    )?;
    // Two-stream bookkeeping (§8.2): reconcile compares this against
    // the upstream head, so a sync appended-then-killed before its
    // render pass is healed at startup exactly like a local commit.
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('last_rendered_upstream', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![upstream.map(|(row, _)| row).unwrap_or(0).to_string()],
    )?;
    // Out-of-band inputs written before this pass took the db lock were
    // visible to every render above; later writers re-set the flag and
    // kick their own pass (writes are serialized on the db mutex).
    conn.execute(
        "DELETE FROM settings WHERE key = ?1",
        params![RENDER_DIRTY_KEY],
    )?;
    drop(conn);

    // Nudge hook (spec/reeve/02-channel.md §4.4): every device whose
    // manifestVersion just advanced — tree commit or secrets rotation,
    // both flow through this pass — gets a best-effort `nudge` scope
    // `desired-state` on its channel. Non-blocking try_send; no retry,
    // no queue, no ack (a lost nudge costs one poll interval). No-op
    // for offline devices and in core builds (empty registry).
    // `ensure_current` deliberately does NOT nudge: it runs inside the
    // device's own poll, which is already the cycle a nudge would ask
    // for.
    for device_id in &updated {
        state.channels.nudge_desired_state(device_id);
    }
    Ok(report)
}

/// Fire-and-log wrapper for authoring hooks: the PUT already committed;
/// a render fault must not fail it (startup reconcile and per-poll
/// ensure_current retry).
pub fn render_all_logged(state: &AppState) {
    match render_all(state) {
        Ok(report) if report.failed.is_empty() => {}
        Ok(report) => warn!(failed = report.failed.len(), "render pass had per-device failures"),
        Err(e) => warn!(error = %e, "render pass failed; startup reconcile will retry"),
    }
}

/// Make one device's manifest row current with the local head, rendering
/// on demand. Serving path for GET /api/reeve/v1/manifest: covers
/// devices enrolled after the last pass (no row yet) and revisions whose
/// pass this device missed (crash, earlier per-device failure).
pub fn ensure_current(state: &AppState, device_id: &str) -> Result<Outcome, PipelineError> {
    // Effective revision = the device's pinned target (§11.2 rollout
    // hold) or the local head; upstream head joins the change check
    // (§8.2: a synced upstream revision must re-render too). Cheap
    // check first: row already there? Lock order everywhere in this
    // module: revisions BEFORE db, never held together.
    let (head, upstream) = {
        let store = state.revisions.lock().expect("revisions mutex poisoned");
        (
            store.head(Stream::Local)?.unwrap_or(0),
            store.origin_head(Stream::Upstream)?,
        )
    };
    let upstream_row = upstream.map(|(row, _)| row);
    let effective = {
        let conn = state.db.lock().expect("db mutex poisoned");
        let effective = device_target(&conn, device_id)?.unwrap_or(head);
        let at: Option<(i64, Option<i64>)> = conn
            .query_row(
                "SELECT rendered_revision, rendered_upstream
                 FROM device_manifests WHERE device_id = ?1",
                params![device_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if at == Some((effective, upstream_row)) {
            return Ok(Outcome::Unchanged);
        }
        effective
    };

    let tree = {
        let store = state.revisions.lock().expect("revisions mutex poisoned");
        let upstream_tree = match upstream {
            Some((row, _)) => tree_at_revision(&store, row)?,
            None => FileSet::new(),
        };
        merge_streams(&upstream_tree, tree_at_revision(&store, effective)?)
    };
    let mut conn = state.db.lock().expect("db mutex poisoned");
    let epoch = server_epoch(&conn)?;
    let dev = device_row(&conn, device_id)?
        .ok_or_else(|| PipelineError::UnknownDevice(device_id.to_string()))?;
    render_one(&mut conn, &tree, effective, upstream, epoch, &state.cfg.registry_endpoint, &dev)
}

/// Render `device_id` at `revision` PURELY (no writes, no manifest
/// bump) and return the content digest of the resulting file set —
/// `None` when desired-state refuses the tree for this device. The
/// rollout engine (ext/rollouts.rs) compares this against the device's
/// stored `content_digest` to detect "pinned/unaffected" cohort members
/// (docs/decisions/tree-render.md D12; spec/reeve/09-rollouts.md §11.1)
/// crash-safely: the probe is a pure recomputation, nothing to lose.
/// `generation`/revision provenance live only in `manifest.yaml`, which
/// [`content_digest`] excludes, so placeholder values do not perturb
/// the digest.
pub fn probe_content_digest(
    state: &AppState,
    device_id: &str,
    revision: RevisionId,
) -> Result<Option<String>, PipelineError> {
    let (tree, upstream) = {
        let store = state.revisions.lock().expect("revisions mutex poisoned");
        let upstream = store.origin_head(Stream::Upstream)?;
        let upstream_tree = match upstream {
            Some((row, _)) => tree_at_revision(&store, row)?,
            None => FileSet::new(),
        };
        (
            merge_streams(&upstream_tree, tree_at_revision(&store, revision)?),
            upstream,
        )
    };
    let dev = {
        let conn = state.db.lock().expect("db mutex poisoned");
        device_row(&conn, device_id)?
    }
    .ok_or_else(|| PipelineError::UnknownDevice(device_id.to_string()))?;
    let ctx = RenderContext {
        device_id: dev.device_id.clone(),
        layers: dev.layer_chain(),
        registry_endpoint: state.cfg.registry_endpoint.clone(),
        generation: 0,
        local_revision: revision.max(0) as u64,
        hub_revision: upstream.map(|(_, origin)| origin.max(0) as u64),
    };
    Ok(desired_state::render(&tree, &ctx)
        .ok()
        .map(|out| content_digest(&out)))
}

/// Startup reconcile (Law 3: startup IS recovery):
/// 1. If the local head moved past `settings.last_rendered_local`
///    (a revision was committed but the render pass was killed), OR an
///    out-of-band render input is flagged dirty ([`RENDER_DIRTY_KEY`] —
///    a secrets write whose propagating pass was killed, §12.4), run a
///    full pass now.
/// 2. Purge bundle blobs no manifest row references (failed/superseded
///    renders leave orphans only until the next startup).
pub fn reconcile(state: &AppState) -> Result<(), PipelineError> {
    let needs_pass = {
        let (head, upstream_row) = {
            let store = state.revisions.lock().expect("revisions mutex poisoned");
            (
                store.head(Stream::Local)?.unwrap_or(0),
                store
                    .origin_head(Stream::Upstream)?
                    .map(|(row, _)| row)
                    .unwrap_or(0),
            )
        };
        let conn = state.db.lock().expect("db mutex poisoned");
        let setting = |key: &str| -> Result<Option<String>, rusqlite::Error> {
            conn.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()
        };
        let last = setting("last_rendered_local")?;
        // §8.2: an upstream revision synced but not yet rendered (kill
        // -9 between append and pass) is recovered here, like a local
        // commit. Absent key (pre-federation DB) reads as 0.
        let last_upstream = setting("last_rendered_upstream")?;
        let dirty = setting(RENDER_DIRTY_KEY)?;
        dirty.is_some()
            || last.and_then(|s| s.parse::<i64>().ok()) != Some(head)
            || last_upstream.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0) != upstream_row
    };
    if needs_pass {
        let report = render_all(state)?;
        if !report.failed.is_empty() {
            warn!(failed = report.failed.len(), "startup render pass had per-device failures");
        }
    }

    let conn = state.db.lock().expect("db mutex poisoned");
    conn.execute(
        "DELETE FROM bundle_blobs WHERE digest NOT IN (
             SELECT bundle_digest FROM device_manifests WHERE bundle_digest IS NOT NULL
             UNION
             SELECT layer_digest FROM device_manifests WHERE layer_digest IS NOT NULL
         ) AND digest <> ?1",
        params![empty_config_digest()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(entries: &[(&str, &str)]) -> FileSet {
        entries
            .iter()
            .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
            .collect()
    }

    /// D2 determinism MUST: identical file sets pack to byte-identical
    /// tar.gz (sorted entries, zeroed metadata).
    #[test]
    fn pack_is_deterministic() {
        let fs = files(&[
            ("manifest.yaml", "deviceId: d\n"),
            ("apps/web/compose.yml", "services: {}\n"),
            ("apps/web/files/a.conf", "x=1\n"),
        ]);
        let a = pack_bundle(&fs).unwrap();
        let b = pack_bundle(&fs).unwrap();
        assert_eq!(a, b, "same file set must pack byte-identically");
        assert_eq!(digest_of(&a), digest_of(&b));

        let other = files(&[("apps/web/compose.yml", "services: {}\n")]);
        assert_ne!(pack_bundle(&other).unwrap(), a);
    }

    /// Packed bundles unpack to exactly the input file set.
    #[test]
    fn pack_roundtrips_through_tar() {
        let fs = files(&[
            ("apps/web/compose.yml", "services: {}\n"),
            ("manifest.yaml", "deviceId: d\n"),
        ]);
        let packed = pack_bundle(&fs).unwrap();
        let gz = flate2::read::GzDecoder::new(packed.as_slice());
        let mut ar = tar::Archive::new(gz);
        let mut got = FileSet::new();
        for entry in ar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf).unwrap();
            assert_eq!(entry.header().mtime().unwrap(), 0, "zeroed mtime");
            assert_eq!(entry.header().uid().unwrap(), 0, "zeroed uid");
            assert_eq!(entry.header().gid().unwrap(), 0, "zeroed gid");
            got.insert(path, buf);
        }
        assert_eq!(got, fs);
    }

    /// content_digest ignores manifest.yaml (provenance) but tracks
    /// every apps/** byte — the D3 no-change detector.
    #[test]
    fn content_digest_excludes_manifest_yaml() {
        let a = files(&[
            ("manifest.yaml", "generation: 1\n"),
            ("apps/web/compose.yml", "services: {}\n"),
        ]);
        let b = files(&[
            ("manifest.yaml", "generation: 2\n"),
            ("apps/web/compose.yml", "services: {}\n"),
        ]);
        assert_eq!(content_digest(&a), content_digest(&b));

        let c = files(&[
            ("manifest.yaml", "generation: 2\n"),
            ("apps/web/compose.yml", "services: {web: {}}\n"),
        ]);
        assert_ne!(content_digest(&a), content_digest(&c));
    }

    /// Path/content boundaries are unambiguous (length-prefixed): moving
    /// a byte between path and content changes the digest.
    #[test]
    fn content_digest_is_boundary_safe() {
        let a = files(&[("apps/ab", "c")]);
        let b = files(&[("apps/a", "bc")]);
        assert_ne!(content_digest(&a), content_digest(&b));
    }

    #[test]
    fn empty_config_digest_is_the_oci_constant() {
        // sha256("{}") — the OCI image-spec empty descriptor digest.
        assert_eq!(
            empty_config_digest(),
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }
}
