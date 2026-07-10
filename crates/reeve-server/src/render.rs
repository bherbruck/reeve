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

use std::collections::BTreeSet;

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

/// Render one device against an already-loaded tree, inside the given
/// connection. One transaction per updated device (Law 3).
fn render_one(
    conn: &mut Connection,
    tree: &FileSet,
    head: RevisionId,
    epoch: u16,
    registry_endpoint: &str,
    dev: &DeviceRow,
) -> Result<Outcome, PipelineError> {
    let stored: Option<(i64, i64, String)> = conn
        .query_row(
            "SELECT counter, generation, content_digest
             FROM device_manifests WHERE device_id = ?1",
            params![dev.device_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (counter, generation, prev_content) = match &stored {
        Some((c, g, d)) => (*c, *g, Some(d.as_str())),
        None => (0, 0, None),
    };

    let ctx = RenderContext {
        device_id: dev.device_id.clone(),
        layers: dev.layer_chain(),
        registry_endpoint: registry_endpoint.to_string(),
        generation: (generation + 1) as u64,
        local_revision: head.max(0) as u64,
        hub_revision: None,
    };
    let out = match desired_state::render(tree, &ctx) {
        Ok(o) => o,
        // Keep-last-good: an authoring error for this device leaves its
        // previous manifest current; rendered_revision is NOT advanced,
        // so the next poll/pass retries (self-healing once fixed).
        Err(e) => return Ok(Outcome::Failed(e.to_string())),
    };

    let cdig = content_digest(&out);
    if prev_content == Some(cdig.as_str()) {
        // D3: no-change re-render => no new bundle, no bump. Advance
        // only the revision bookkeeping so this pass isn't repeated.
        conn.execute(
            "UPDATE device_manifests
             SET rendered_revision = ?2, updated_at = ?3
             WHERE device_id = ?1",
            params![dev.device_id, head, now_secs()],
        )?;
        return Ok(Outcome::Unchanged);
    }

    // Material change: allocate the next manifestVersion (per-device
    // counter, monotonic; epoch from settings — §10.2 anti-rollback).
    let next_counter = counter + 1;
    let version = ManifestVersion::pack(epoch, next_counter as u64)?;

    let apps = app_names(&out);
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    let (bundle_ref, bundle_digest, layer_digest) = if apps.is_empty() {
        // Margo DeploymentBundleRef null rule: zero apps => bundle is
        // present with the value null (reeve-types StateManifest doc).
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
        apps: apps
            .iter()
            .map(|a| AppManifestEntry {
                app_id: a.clone(),
                deployment_id: Some(deployment_id(&dev.device_id, a).to_string()),
                // 10-secrets §12 lands with the secrets task; absent
                // until then. A future secrets_version change MUST bump
                // the version even with an unchanged bundle — it will
                // enter content_digest's input set when implemented.
                secrets_version: None,
            })
            .collect(),
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
              bundle_digest, layer_digest, manifest_json, etag,
              rendered_revision, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(device_id) DO UPDATE SET
             manifest_version = excluded.manifest_version,
             counter = excluded.counter,
             generation = excluded.generation,
             content_digest = excluded.content_digest,
             bundle_digest = excluded.bundle_digest,
             layer_digest = excluded.layer_digest,
             manifest_json = excluded.manifest_json,
             etag = excluded.etag,
             rendered_revision = excluded.rendered_revision,
             updated_at = excluded.updated_at",
        params![
            dev.device_id,
            version.0 as i64,
            next_counter,
            generation + 1,
            cdig,
            bundle_digest,
            layer_digest,
            String::from_utf8(manifest_json).expect("serde_json emits UTF-8"),
            etag,
            head,
            now,
        ],
    )?;
    tx.commit()?;
    Ok(Outcome::Updated(version))
}

fn device_row(conn: &Connection, device_id: &str) -> Result<Option<DeviceRow>, rusqlite::Error> {
    conn.query_row(
        "SELECT device_id, class, region, site FROM devices WHERE device_id = ?1",
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

/// Read the local head + full tree under the revisions lock, releasing
/// it before any DB work (locks are short, never held together longer
/// than needed).
fn snapshot_tree(state: &AppState) -> Result<(RevisionId, FileSet), PipelineError> {
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    let head = store.head(Stream::Local)?;
    let tree = load_tree(&store, head)?;
    Ok((head.unwrap_or(0), tree))
}

/// Render every enrolled device against the current local head. Called
/// after every changed authoring commit, from POST /api/render, and
/// from startup reconcile. Records `settings.last_rendered_local` when
/// the pass completes (per-device Failed outcomes do not block it —
/// they are authoring errors that only a new commit can fix, and
/// per-device `rendered_revision` staying behind retries them on poll).
pub fn render_all(state: &AppState) -> Result<RenderReport, PipelineError> {
    let (head, tree) = snapshot_tree(state)?;

    let mut conn = state.db.lock().expect("db mutex poisoned");
    let epoch = server_epoch(&conn)?;
    let devices: Vec<DeviceRow> = {
        let mut stmt =
            conn.prepare("SELECT device_id, class, region, site FROM devices ORDER BY device_id")?;
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
    for dev in &devices {
        match render_one(&mut conn, &tree, head, epoch, &state.cfg.registry_endpoint, dev)? {
            Outcome::Unchanged => report.unchanged += 1,
            Outcome::Updated(_) => report.rendered += 1,
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
    // Cheap check first: row already at head? Lock order everywhere in
    // this module: revisions BEFORE db, never held together.
    {
        let head = {
            let store = state.revisions.lock().expect("revisions mutex poisoned");
            store.head(Stream::Local)?.unwrap_or(0)
        };
        let conn = state.db.lock().expect("db mutex poisoned");
        let at: Option<i64> = conn
            .query_row(
                "SELECT rendered_revision FROM device_manifests WHERE device_id = ?1",
                params![device_id],
                |r| r.get(0),
            )
            .optional()?;
        if at == Some(head) {
            return Ok(Outcome::Unchanged);
        }
    }

    let (head, tree) = snapshot_tree(state)?;
    let mut conn = state.db.lock().expect("db mutex poisoned");
    let epoch = server_epoch(&conn)?;
    let dev = device_row(&conn, device_id)?
        .ok_or_else(|| PipelineError::UnknownDevice(device_id.to_string()))?;
    render_one(&mut conn, &tree, head, epoch, &state.cfg.registry_endpoint, &dev)
}

/// Startup reconcile (Law 3: startup IS recovery):
/// 1. If the local head moved past `settings.last_rendered_local`
///    (a revision was committed but the render pass was killed), run a
///    full pass now.
/// 2. Purge bundle blobs no manifest row references (failed/superseded
///    renders leave orphans only until the next startup).
pub fn reconcile(state: &AppState) -> Result<(), PipelineError> {
    let needs_pass = {
        let head = {
            let store = state.revisions.lock().expect("revisions mutex poisoned");
            store.head(Stream::Local)?.unwrap_or(0)
        };
        let conn = state.db.lock().expect("db mutex poisoned");
        let last: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'last_rendered_local'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        last.and_then(|s| s.parse::<i64>().ok()) != Some(head)
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
