//! Tree authoring + inspection API (docs/decisions/authoring.md D14).
//!
//! The revision store's single writer is this API. It is
//! automation-first: an IaC tool holds the layer content in its own git
//! repo and applies a directory of files with one idempotent PUT —
//! identical content produces NO new revision (the D3
//! no-change-no-commit rule; the store's commit is idempotent against
//! its head).
//!
//! Write surface (operator+):
//!   PUT /api/tree/layers/{layer}            — replace one layer's whole
//!       subtree (declarative: files absent from the body are removed
//!       from the layer). Layer dir grammar per tree-render.md D11.
//!   PUT /api/tree/packages/{name}/{version} — vendor a margo package
//!       dir into `packages/<name>/<version>/` (D11), validated by the
//!       margo-package crate before anything is committed.
//!
//! Every write targets [`Stream::Local`] only and passes the
//! [`Ownership`](crate::ownership::Ownership) gate first —
//! spec/reeve/06-federation.md §8.2/§8.4: refuse writes outside this
//! tier's ownership set; the upstream stream is never writable.
//!
//! Read surface (viewer+): revision history, tree at a revision, diff,
//! blame, file content — all straight revision-store queries (D13:
//! diff/undo/blame are computed on read).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use device_api::{Identity, Role};
use revision_store::{Revision, RevisionId, RevisionStore, Stream};
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::join_tokens::require_at_least;
use crate::state::AppState;

// ---------------------------------------------------------------------
// Path grammar (tree-render.md D11)
// ---------------------------------------------------------------------

/// Validate a layer directory name: `NN-<label>` where NN are two ASCII
/// digits (only the numeric prefix orders the merge — D11/D12) and
/// `<label>` is 1..=128 chars of `[A-Za-z0-9._-]`, starting with an
/// alphanumeric and not ending with `.`. The taxonomy
/// (`fleet`/`class.<n>`/`region.<n>`/`site.<n>`/`device.<id>`) is
/// convention, not engine knowledge (D12) — the grammar is the contract.
pub fn validate_layer_dir(layer: &str) -> Result<(), String> {
    let bytes = layer.as_bytes();
    if bytes.len() < 4 || bytes.len() > 131 {
        return Err(format!(
            "layer `{layer}` must be `NN-<label>` (2 digits, dash, 1..=128 char label)"
        ));
    }
    if !(bytes[0].is_ascii_digit() && bytes[1].is_ascii_digit() && bytes[2] == b'-') {
        return Err(format!("layer `{layer}` must start with two digits and a dash (D11)"));
    }
    let label = &layer[3..];
    if !label.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(format!("layer `{layer}`: label must start with an alphanumeric"));
    }
    if label.ends_with('.') {
        return Err(format!("layer `{layer}`: label must not end with `.`"));
    }
    if let Some(bad) = label
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!("layer `{layer}`: illegal character `{bad}` in label"));
    }
    Ok(())
}

/// Validate one path segment for `packages/<name>/<version>` (D11):
/// 1..=100 chars of `[A-Za-z0-9._+-]`, starting alphanumeric (so `.` and
/// `..` are impossible).
pub fn validate_package_segment(kind: &str, seg: &str) -> Result<(), String> {
    if seg.is_empty() || seg.len() > 100 {
        return Err(format!("package {kind} must be 1..=100 characters"));
    }
    if !seg.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(format!("package {kind} `{seg}` must start with an alphanumeric"));
    }
    if let Some(bad) = seg
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')))
    {
        return Err(format!("package {kind} `{seg}`: illegal character `{bad}`"));
    }
    Ok(())
}

/// Validate a file path relative to a layer/package root: non-empty,
/// `/`-separated, every segment non-empty and none of `.`/`..`, no
/// backslash or control characters. Rejects anything that could escape
/// or alias the subtree being written.
pub fn validate_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 512 {
        return Err("file path must be 1..=512 characters".to_string());
    }
    if path.chars().any(|c| c == '\\' || c.is_control()) {
        return Err(format!("file path `{path}` contains a forbidden character"));
    }
    for seg in path.split('/') {
        if seg.is_empty() {
            return Err(format!(
                "file path `{path}` has an empty segment (no leading/trailing/double `/`)"
            ));
        }
        if seg == "." || seg == ".." {
            return Err(format!("file path `{path}` contains `.`/`..` segments"));
        }
        if seg.len() > 255 {
            return Err(format!("file path `{path}`: segment over 255 characters"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------

/// Body of both PUT endpoints: the complete content of one layer (or
/// package) as relative path -> base64(content). Base64 keeps the wire
/// JSON binary-safe (compose files are text, but package resources —
/// icons — are not).
#[derive(Debug, Deserialize)]
pub struct PutFilesRequest {
    /// Commit message; a default is derived when absent.
    pub message: Option<String>,
    /// Relative path within the layer/package -> standard-base64 bytes.
    pub files: BTreeMap<String, String>,
}

/// Revision metadata as served to clients.
#[derive(Debug, Serialize)]
pub struct RevisionInfo {
    pub id: RevisionId,
    pub stream: &'static str,
    pub parent: Option<RevisionId>,
    pub author: String,
    pub message: String,
    pub created_at: String,
}

impl From<Revision> for RevisionInfo {
    fn from(r: Revision) -> Self {
        RevisionInfo {
            id: r.id,
            stream: match r.stream {
                Stream::Upstream => "upstream",
                Stream::Local => "local",
            },
            parent: r.parent,
            author: r.author,
            message: r.message,
            created_at: r.created_at,
        }
    }
}

// ---------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "tree route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn unprocessable(msg: impl std::fmt::Display) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({ "error": msg.to_string() })),
    )
        .into_response()
}

fn store_err(e: revision_store::Error) -> Response {
    match e {
        revision_store::Error::UnknownRevision(id) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown revision {id}") })),
        )
            .into_response(),
        other => internal(other),
    }
}

/// The OTHER half of §8.4's single-writer rule, enforced at the PARENT
/// (spec/reeve/06-federation.md §8.4: "the root does not edit site
/// layers owned by gateways"): a tree path is refused when it falls
/// under a layer delegated to a child tier — the `20-site.<site>`
/// family of any active tier token, or the device layer of a device
/// that reached us via forwarded ingest (`devices.tier_origin`,
/// §8.3). CORE, not ext-federation: the V9 tables exist regardless
/// (db.rs), so a --no-default-features binary keeps refusing writes it
/// does not own. Returns the owning child's name.
fn delegated_to(conn: &rusqlite::Connection, tree_path: &str) -> rusqlite::Result<Option<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, site FROM tier_tokens
         WHERE revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > ?1)",
    )?;
    let rows = stmt.query_map(rusqlite::params![crate::db::now_secs()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (name, site) = row?;
        // Site layers are `NN-site.<label>` by D11 convention; match
        // any numeric prefix so renumbering does not open a hole.
        if let Some(rest) = tree_path.strip_prefix("layers/")
            && let Some(dash) = rest.find('-')
            && rest[..dash].bytes().all(|b| b.is_ascii_digit())
            && crate::ownership::prefix_matches(
                &format!("site.{site}"),
                rest[dash + 1..].split('/').next().unwrap_or(""),
            )
        {
            return Ok(Some(name));
        }
    }
    // Device layers of forwarded devices (enrolled at a child tier).
    if let Some(rest) = tree_path.strip_prefix("layers/")
        && let Some(dev) = rest.split('/').next().and_then(|l| {
            l.split_once('-')
                .and_then(|(n, label)| {
                    (n.bytes().all(|b| b.is_ascii_digit())).then_some(label)
                })
                .and_then(|label| label.strip_prefix("device."))
        })
    {
        let origin: Option<Option<String>> = conn
            .query_row(
                "SELECT tier_origin FROM devices WHERE device_id = ?1",
                rusqlite::params![dev],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(Some(origin)) = origin {
            return Ok(Some(origin));
        }
    }
    Ok(None)
}

/// 403 when `tree_path` belongs to a child tier (see [`delegated_to`]).
fn check_not_delegated(state: &AppState, tree_path: &str) -> Result<(), Box<Response>> {
    let conn = state.db.lock().expect("db mutex poisoned");
    match delegated_to(&conn, tree_path) {
        Ok(None) => Ok(()),
        Ok(Some(child)) => Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": format!(
                        "`{tree_path}` is owned by child tier `{child}` \
                         (single writer per layer, federation §8.4)"
                    )
                })),
            )
                .into_response(),
        )),
        Err(e) => Err(Box::new(internal(e))),
    }
}

fn author_of(identity: &Identity) -> String {
    match identity {
        Identity::Human { user, .. } => user.clone(),
        // REEVE_AUTH=none: anonymous acts as admin (D1).
        _ => "anonymous".to_string(),
    }
}

/// Decode and validate a request's files into full tree paths under
/// `prefix` (which must end with `/`).
fn decode_files(
    prefix: &str,
    files: &BTreeMap<String, String>,
) -> Result<Vec<(String, Vec<u8>)>, Box<Response>> {
    let mut out = Vec::with_capacity(files.len());
    for (rel, b64) in files {
        validate_rel_path(rel).map_err(|m| Box::new(unprocessable(m)))?;
        let content = B64
            .decode(b64)
            .map_err(|e| Box::new(unprocessable(format!("file `{rel}`: invalid base64 ({e})"))))?;
        out.push((format!("{prefix}{rel}"), content));
    }
    Ok(out)
}

/// Replace the subtree under `prefix` (trailing `/`) with `files`
/// (already full tree paths), carrying every other path of the local
/// head forward. One store commit — atomic (Law 3), idempotent against
/// head (D14): returns `(revision, changed)`.
fn commit_subtree(
    store: &mut RevisionStore,
    prefix: &str,
    files: Vec<(String, Vec<u8>)>,
    author: &str,
    message: &str,
) -> Result<(RevisionId, bool), revision_store::Error> {
    debug_assert!(prefix.ends_with('/'));
    let head = store.head(Stream::Local)?;
    let mut manifest: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(head_id) = head {
        for (path, digest) in store.tree_at(head_id)? {
            if path.starts_with(prefix) {
                continue; // replaced wholesale by this PUT
            }
            let content = store.blob(&digest)?.ok_or_else(|| {
                revision_store::Error::Corrupt(format!("missing blob {digest} for {path}"))
            })?;
            manifest.push((path, content));
        }
    }
    manifest.extend(files);
    let id = store.commit(manifest, author, message, Stream::Local)?;
    Ok((id, head != Some(id)))
}

// ---------------------------------------------------------------------
// Write handlers (operator+)
// ---------------------------------------------------------------------

/// PUT /api/tree/layers/{layer} — apply one layer's complete content
/// (D14 batch semantics: the body IS the layer; a file absent from the
/// body is removed). Identical content => no new revision.
pub async fn put_layer(
    State(state): State<AppState>,
    identity: Identity,
    Path(layer): Path<String>,
    Json(body): Json<PutFilesRequest>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    if let Err(msg) = validate_layer_dir(&layer) {
        return unprocessable(msg);
    }
    let tree_path = format!("layers/{layer}");
    // §8.2/§8.4: structural ownership gate; authoring only ever targets
    // the local stream.
    if let Err(refusal) = state.ownership.check_write(Stream::Local, &tree_path) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": refusal.to_string() })),
        )
            .into_response();
    }
    // §8.4 the other direction: this tier must not edit layers it
    // delegated to a child tier.
    if let Err(resp) = check_not_delegated(&state, &tree_path) {
        return *resp;
    }

    let prefix = format!("{tree_path}/");
    let files = match decode_files(&prefix, &body.files) {
        Ok(f) => f,
        Err(resp) => return *resp,
    };
    let author = author_of(&identity);
    let message = body
        .message
        .unwrap_or_else(|| format!("apply layer {tree_path}"));

    let committed = {
        let mut store = state.revisions.lock().expect("revisions mutex poisoned");
        commit_subtree(&mut store, &prefix, files, &author, &message)
    };
    match committed {
        Ok((revision, changed)) => {
            if changed {
                // Render hook (C4): a new revision re-renders affected
                // devices. Fire-and-log — the commit already succeeded;
                // startup reconcile / per-poll ensure_current retry.
                crate::render::render_all_logged(&state);
            }
            Json(json!({
                "revision": revision,
                "changed": changed,
                "stream": "local",
            }))
            .into_response()
        }
        Err(e) => internal(e),
    }
}

/// PUT /api/tree/packages/{name}/{version} — vendor a margo package
/// directory into the tree (D11: packages are vendored in v1, keeping
/// render pure). The upload is validated by the margo-package crate
/// BEFORE anything is committed: invalid packages produce 422 and no
/// revision.
pub async fn put_package(
    State(state): State<AppState>,
    identity: Identity,
    Path((name, version)): Path<(String, String)>,
    Json(body): Json<PutFilesRequest>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    if let Err(msg) =
        validate_package_segment("name", &name).and_then(|()| validate_package_segment("version", &version))
    {
        return unprocessable(msg);
    }
    let tree_path = format!("packages/{name}/{version}");
    if let Err(refusal) = state.ownership.check_write(Stream::Local, &tree_path) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": refusal.to_string() })),
        )
            .into_response();
    }
    if let Err(resp) = check_not_delegated(&state, &tree_path) {
        return *resp;
    }

    let prefix = format!("{tree_path}/");
    let files = match decode_files(&prefix, &body.files) {
        Ok(f) => f,
        Err(resp) => return *resp,
    };
    if !body.files.contains_key(margo_package::MANIFEST_FILE_NAME) {
        return unprocessable(format!(
            "package upload must contain `{}` at its root",
            margo_package::MANIFEST_FILE_NAME
        ));
    }

    // Validate via margo-package against a materialized temp dir — the
    // same loader the render path uses, so what vendors is what renders.
    let package = match materialize_and_load(&prefix, &files) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let warnings: Vec<String> = package
        .warnings
        .iter()
        .map(|w| format!("{}: {}", w.path, w.message))
        .collect();

    let author = author_of(&identity);
    let message = body
        .message
        .unwrap_or_else(|| format!("vendor package {name}/{version}"));

    let committed = {
        let mut store = state.revisions.lock().expect("revisions mutex poisoned");
        commit_subtree(&mut store, &prefix, files, &author, &message)
    };
    match committed {
        Ok((revision, changed)) => {
            if changed {
                // Render hook (C4) — see put_layer.
                crate::render::render_all_logged(&state);
            }
            Json(json!({
                "revision": revision,
                "changed": changed,
                "stream": "local",
                "warnings": warnings,
            }))
            .into_response()
        }
        Err(e) => internal(e),
    }
}

/// Write the uploaded files (full tree paths under `prefix`) into a
/// temp dir and load them with [`margo_package::Package::load_dir`].
/// Paths were already validated by [`validate_rel_path`], so the temp
/// dir cannot be escaped.
fn materialize_and_load(
    prefix: &str,
    files: &[(String, Vec<u8>)],
) -> Result<margo_package::Package, Box<Response>> {
    let dir = tempfile::tempdir().map_err(|e| Box::new(internal(e)))?;
    for (path, content) in files {
        let rel = path.strip_prefix(prefix).expect("paths built from prefix");
        let dest = dir.path().join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Box::new(internal(e)))?;
        }
        std::fs::write(&dest, content).map_err(|e| Box::new(internal(e)))?;
    }
    margo_package::Package::load_dir(dir.path())
        .map_err(|e| Box::new(unprocessable(format!("package validation failed: {e}"))))
}

// ---------------------------------------------------------------------
// Read handlers (viewer+) — straight revision-store queries (D13)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<usize>,
}

/// GET /api/tree/revisions[?limit=N] — revision history, both streams,
/// newest first.
pub async fn list_revisions(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let limit = q.limit.unwrap_or(100).min(1000);
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    match history(&store, limit) {
        Ok(revs) => Json(revs.into_iter().map(RevisionInfo::from).collect::<Vec<_>>())
            .into_response(),
        Err(e) => store_err(e),
    }
}

/// Both streams' chains, newest first. Chain-walk from each head — the
/// parent pointers ARE the stream (append-only, D13).
fn history(store: &RevisionStore, limit: usize) -> Result<Vec<Revision>, revision_store::Error> {
    let mut out = Vec::new();
    for stream in [Stream::Local, Stream::Upstream] {
        let mut cursor = store.head(stream)?;
        while let Some(id) = cursor {
            let rev = store.revision(id)?;
            cursor = rev.parent;
            out.push(rev);
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.id));
    out.truncate(limit);
    Ok(out)
}

/// GET /api/tree/revisions/{id} — metadata + full manifest
/// (path -> blob digest) at that revision.
pub async fn get_revision(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<RevisionId>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    let rev = match store.revision(id) {
        Ok(r) => r,
        Err(e) => return store_err(e),
    };
    match store.tree_at(id) {
        Ok(tree) => Json(json!({
            "revision": RevisionInfo::from(rev),
            "files": tree,
        }))
        .into_response(),
        Err(e) => store_err(e),
    }
}

/// GET /api/tree/revisions/{id}/files/{*path} — raw file content at a
/// revision.
pub async fn file_at(
    State(state): State<AppState>,
    identity: Identity,
    Path((id, path)): Path<(RevisionId, String)>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    match store.read_at(id, &path) {
        Ok(Some(bytes)) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no `{path}` at revision {id}") })),
        )
            .into_response(),
        Err(e) => store_err(e),
    }
}

/// GET /api/tree/diff/{a}/{b} — manifest diff between two revisions,
/// computed on read (D13).
pub async fn diff(
    State(state): State<AppState>,
    identity: Identity,
    Path((a, b)): Path<(RevisionId, RevisionId)>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    match store.diff(a, b) {
        Ok(entries) => {
            let body: Vec<_> = entries
                .into_iter()
                .map(|e| match e.change {
                    revision_store::Change::Added { digest } => {
                        json!({ "path": e.path, "change": "added", "new": digest })
                    }
                    revision_store::Change::Removed { digest } => {
                        json!({ "path": e.path, "change": "removed", "old": digest })
                    }
                    revision_store::Change::Modified { old, new } => {
                        json!({ "path": e.path, "change": "modified", "old": old, "new": new })
                    }
                })
                .collect();
            Json(body).into_response()
        }
        Err(e) => store_err(e),
    }
}

/// GET /api/tree/blame/{*path} — every revision at which the path
/// changed, ascending (blame = SELECT, D13). `digest: null` marks a
/// removal.
pub async fn blame(
    State(state): State<AppState>,
    identity: Identity,
    Path(path): Path<String>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    match store.blame(&path) {
        Ok(entries) => {
            let body: Vec<_> = entries
                .into_iter()
                .map(|e| {
                    json!({
                        "revision": RevisionInfo::from(e.revision),
                        "digest": e.digest,
                    })
                })
                .collect();
            Json(body).into_response()
        }
        Err(e) => store_err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_grammar_accepts_d11_taxonomy() {
        for ok in [
            "00-fleet",
            "05-class.gpu",
            "10-region.emea",
            "20-site.plant-a",
            "30-device.dev-0011223344556677",
        ] {
            assert!(validate_layer_dir(ok).is_ok(), "{ok} should be valid");
        }
    }

    #[test]
    fn layer_grammar_rejects_malformed_names() {
        for bad in [
            "",
            "fleet",            // no numeric prefix
            "0-fleet",          // one digit
            "000-fleet",        // three digits
            "00_fleet",         // no dash
            "00-",              // empty label
            "00-.fleet",        // label starts with dot
            "00-fleet.",        // label ends with dot
            "00-fle/et",        // path separator
            "00-fle et",        // space
            "00-../evil",       // traversal
        ] {
            assert!(validate_layer_dir(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rel_path_rejects_escapes() {
        assert!(validate_rel_path("apps/nginx/app.yaml").is_ok());
        assert!(validate_rel_path(".keep").is_ok());
        for bad in [
            "",
            "/abs/path",
            "a//b",
            "a/",
            "../up",
            "a/../b",
            "a/./b",
            "a\\b",
            "a\nb",
        ] {
            assert!(validate_rel_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn package_segments_are_safe_path_segments() {
        assert!(validate_package_segment("name", "nextcloud").is_ok());
        assert!(validate_package_segment("version", "1.0.0+build-7").is_ok());
        for bad in ["", ".", "..", ".hidden", "a/b", "a b", "-lead"] {
            assert!(validate_package_segment("name", bad).is_err(), "{bad:?}");
        }
    }
}
