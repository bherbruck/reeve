//! ext-federation (REV-005, C10) — multi-tier operation.
//!
//! Normative sources:
//! - spec/reeve/06-federation.md §8 (THE spec for this module):
//!   §8.1 topology (REEVE_UPSTREAM presence selects the tier — same
//!   binary, deploy.md D9), §8.2 revision sync (conditional GET on the
//!   parent head + content-addressed blob fetch; verbatim read-only
//!   upstream stream; divergence is an ERROR, never auto-resolved),
//!   §8.3 status forwarding (same journal protocol one tier up,
//!   original timestamps, `(deviceId, seq)` idempotency), §8.4 single
//!   writer per layer, §8.5 air-gap transfer (signed OCI layout
//!   archives, one format for everything), §8.6 federation-blind
//!   agents (NOTHING in this module touches an agent-facing payload),
//!   §8.7 scoped tier credentials enforced server-side.
//! - spec/reeve/10-secrets.md §12.5 / docs/decisions/secrets.md D15:
//!   scoped secret sync — the child PULLS (recorded decision: the same
//!   pull-based pattern as revision sync, one loop, one credential;
//!   the spec's "hub syncs DOWN" describes data flow, not who dials)
//!   only the secrets resolvable in its subtree, transported plaintext
//!   over the TLS tier channel and re-encrypted at rest under the
//!   child's OWN keyfile (per-tier keys). Air-gapped secret sets are
//!   sealed to the destination gateway's X25519 public key — never
//!   plaintext on media.
//!
//! Crash-only (Law 3): every append is transactional in revision-store
//! (closure-complete or invisible); blob inserts are idempotent by
//! digest so an interrupted transfer resumes by fetching only what is
//! still missing; forwarding cursors advance only after the parent
//! persisted the batch; import re-runs are no-ops.
//!
//! Recorded decisions (OURS ENTIRELY — the Margo spec is silent on
//! WFM-to-WFM topology):
//! - Sync revision identity: the parent serves each local-stream
//!   revision with a `digest` = SHA-256 over (origin id, origin
//!   parent, author, message, created_at, scope-filtered file
//!   manifest). The child recomputes it on receipt and revision-store
//!   enforces the §8.2 id/digest rule on append.
//! - Scope filtering is per tier token (`sync_prefixes`), applied to
//!   revision manifests AND blob fetches with the same prefix matcher
//!   as ownership (one rule, no drift).
//! - Sealed box for air-gap secrets: X25519 ECIES — ephemeral
//!   keypair, DH with the recipient key, key = SHA-256(domain-sep ||
//!   shared || eph_pub || recipient_pub), payload = eph_pub ||
//!   XChaCha20-Poly1305 envelope (durability::aead). Pure Rust
//!   (x25519-dalek + the existing chacha20poly1305), NaCl-shaped.
//! - Archive signing: ed25519 (ed25519-dalek, pinned) over the OCI
//!   `index.json` bytes; integrity of everything else follows the
//!   content-addressed chain index -> manifest -> payload -> blobs.
//!   Import pins the signer key trust-on-first-use in settings
//!   (`federation_trusted_signer`), overridable with
//!   `--expect-signer`; fingerprints are verified out of band at
//!   commissioning (§8.7).

use std::collections::BTreeMap;
use std::path::Path as FsPath;

use axum::Json;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header, request::Parts};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use device_api::status::StatusIngest as _;
use device_api::{Identity, Role, token_hash};
use reeve_types::reeve::events::{FederationSyncEvent, SseEvent};
use reeve_types::reeve::health::{JournalAck, JournalBatch, JournalRecord, JournalRecordKind};
use revision_store::{Stream, VerbatimOutcome, VerbatimRevision, digest_of};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info, warn};

use crate::config::FederationConfig;
use crate::db::now_secs;
use crate::durability::aead;
use crate::events::EventHub;
use crate::join_tokens::require_at_least;
use crate::keyfile;
use crate::ownership::prefix_matches;
use crate::state::AppState;

/// Prefix on every tier token — distinct from join (`rvj_`), device
/// (`rvd_`) and session (`rvh_`) tokens.
pub const TIER_TOKEN_PREFIX: &str = "rvt_";

/// Default tree-path scope of a fresh tier token: the hub-owned layer
/// families (fleet/class/region, §8.2) plus vendored packages. The
/// numeric prefixes are the D11 taxonomy convention (render.rs
/// layer_chain); admins can override per token.
pub const DEFAULT_SYNC_PREFIXES: &[&str] = &[
    "layers/00-fleet",
    "layers/05-class.",
    "layers/10-region.",
    "packages/",
];

/// Tier signing identity (ed25519 seed), minted at first use (§8.7).
pub const SIGNING_KEY_FILE: &str = "tier_ed25519.key";
/// Tier sealed-box identity (X25519 secret), minted at first use
/// (10-secrets §12.5: each gateway mints a keypair at init).
pub const X25519_KEY_FILE: &str = "tier_x25519.key";

// =====================================================================
// Wire shapes (tier-to-tier only — agents never see any of this, §8.6)
// =====================================================================

/// `GET /api/reeve/v1/sync/head` response.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncHead {
    /// Parent-tier revision id of its local-stream head; 0 when the
    /// parent has no revisions yet.
    pub head: i64,
}

/// One parent revision as served to a child (`GET
/// /api/reeve/v1/sync/revisions`) — verbatim metadata plus the
/// scope-filtered file manifest (path -> blob digest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRevision {
    /// The revision id AT THE PARENT (the child's origin id).
    pub id: i64,
    pub parent: Option<i64>,
    pub author: String,
    pub message: String,
    pub created_at: String,
    /// Identity digest (see module docs) the child MUST verify.
    pub digest: String,
    pub files: BTreeMap<String, String>,
}

/// One scoped secret in flight on the tier channel (10-secrets §12.5:
/// plaintext exists only in RAM and TLS; the child re-seals under its
/// own keyfile before the row touches its DB).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSecret {
    pub name: String,
    pub scope: String,
    pub version: u64,
    pub value: String,
}

/// The §8.2 revision identity digest: SHA-256 over length-prefixed
/// (origin id, origin parent, author, message, created_at, sorted
/// (path, digest) pairs). Both sides compute it independently.
pub fn revision_digest(
    id: i64,
    parent: Option<i64>,
    author: &str,
    message: &str,
    created_at: &str,
    files: &BTreeMap<String, String>,
) -> String {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    let put = |h: &mut Sha256, bytes: &[u8]| {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    h.update(id.to_le_bytes());
    h.update(parent.unwrap_or(-1).to_le_bytes());
    put(&mut h, author.as_bytes());
    put(&mut h, message.as_bytes());
    put(&mut h, created_at.as_bytes());
    for (path, digest) in files {
        put(&mut h, path.as_bytes());
        put(&mut h, digest.as_bytes());
    }
    format!("sha256:{:x}", h.finalize())
}

impl SyncRevision {
    fn computed_digest(&self) -> String {
        revision_digest(
            self.id,
            self.parent,
            &self.author,
            &self.message,
            &self.created_at,
            &self.files,
        )
    }
}

/// Does `path` fall inside the token's sync scope? Same matcher as
/// ownership (§8.4/§8.7 — one rule).
fn in_scope(prefixes: &[String], path: &str) -> bool {
    prefixes.iter().any(|p| prefix_matches(p, path))
}

// =====================================================================
// Tier tokens (§8.7) — issued like join tokens, by an admin
// =====================================================================

/// Generate a fresh tier token: `rvt_` + 64 hex chars (256 bits).
pub fn generate_tier_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    format!("{TIER_TOKEN_PREFIX}{}", hex::encode(buf))
}

/// Issue a tier token; returns the raw token — the only time it exists
/// here (only the hash is stored). `ttl_secs: None` => non-expiring
/// (a tier credential outlives sessions; revocation is the lever).
pub fn issue_tier_token(
    conn: &Connection,
    name: &str,
    site: &str,
    sync_prefixes: &[String],
    created_by: &str,
    ttl_secs: Option<i64>,
) -> anyhow::Result<String> {
    let token = generate_tier_token();
    let now = now_secs();
    conn.execute(
        "INSERT INTO tier_tokens
             (token_hash, name, site, sync_prefixes, created_by, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            token_hash(&token),
            name,
            site,
            serde_json::to_string(sync_prefixes)?,
            created_by,
            now,
            ttl_secs.map(|t| now + t),
        ],
    )?;
    Ok(token)
}

/// The authenticated child tier on a sync call — resolved from the
/// bearer tier token; carries the scope the parent MUST enforce
/// server-side (§8.7).
#[derive(Debug, Clone)]
pub struct TierIdentity {
    pub name: String,
    pub site: String,
    pub sync_prefixes: Vec<String>,
}

impl FromRequestParts<AppState> for TierIdentity {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if !bearer.starts_with(TIER_TOKEN_PREFIX) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let hash = token_hash(bearer);
        let conn = state.db.lock().expect("db mutex poisoned");
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT name, site, sync_prefixes FROM tier_tokens
                 WHERE token_hash = ?1 AND revoked_at IS NULL
                   AND (expires_at IS NULL OR expires_at > ?2)",
                params![hash, now_secs()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let Some((name, site, prefixes)) = row else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        let sync_prefixes: Vec<String> =
            serde_json::from_str(&prefixes).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(TierIdentity { name, site, sync_prefixes })
    }
}

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "federation route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// Body of POST /api/tier-tokens.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTierTokenRequest {
    /// Unique child-tier name (e.g. the gateway hostname).
    pub name: String,
    /// The child's site label — the layer family it owns (§8.4).
    pub site: String,
    /// Tree-path prefixes the child may sync; default
    /// [`DEFAULT_SYNC_PREFIXES`].
    #[serde(default)]
    pub sync_prefixes: Option<Vec<String>>,
    /// Optional TTL; absent => non-expiring (revocation is the lever).
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

/// POST /api/tier-tokens (admin — §8.7 "issued like join tokens by
/// admin"). Returns the raw token ONCE.
pub async fn create_token_route(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<CreateTierTokenRequest>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Admin) {
        return status.into_response();
    }
    if body.name.is_empty() || body.name.len() > 128 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "name must be 1..=128 chars" })),
        )
            .into_response();
    }
    if let Err(msg) = crate::tree::validate_layer_dir(&format!("20-site.{}", body.site)) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": format!("invalid site label: {msg}") })),
        )
            .into_response();
    }
    let prefixes: Vec<String> = body
        .sync_prefixes
        .unwrap_or_else(|| DEFAULT_SYNC_PREFIXES.iter().map(|s| s.to_string()).collect());
    let created_by = match &identity {
        Identity::Human { user, .. } => user.clone(),
        _ => "anonymous".to_string(),
    };
    let conn = state.db.lock().expect("db mutex poisoned");
    match issue_tier_token(&conn, &body.name, &body.site, &prefixes, &created_by, body.ttl_secs) {
        Ok(token) => Json(json!({
            "token": token,
            "name": body.name,
            "site": body.site,
            "syncPrefixes": prefixes,
        }))
        .into_response(),
        Err(e) if e.to_string().contains("UNIQUE") => (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("tier token name `{}` already exists", body.name) })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

/// GET /api/tier-tokens (viewer+): metadata, never raw tokens.
pub async fn list_tokens_route(State(state): State<AppState>, identity: Identity) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    let rows: Result<Vec<serde_json::Value>, rusqlite::Error> = (|| {
        let mut stmt = conn.prepare(
            "SELECT name, site, sync_prefixes, created_by, created_at, expires_at, revoked_at
             FROM tier_tokens ORDER BY created_at DESC, name",
        )?;
        let rows = stmt.query_map([], |r| {
            let prefixes: String = r.get(2)?;
            Ok(json!({
                "name": r.get::<_, String>(0)?,
                "site": r.get::<_, String>(1)?,
                "syncPrefixes": serde_json::from_str::<Vec<String>>(&prefixes)
                    .unwrap_or_default(),
                "createdBy": r.get::<_, String>(3)?,
                "createdAt": r.get::<_, i64>(4)?,
                "expiresAt": r.get::<_, Option<i64>>(5)?,
                "revokedAt": r.get::<_, Option<i64>>(6)?,
            }))
        })?;
        rows.collect()
    })();
    match rows {
        Ok(list) => Json(json!({ "tierTokens": list })).into_response(),
        Err(e) => internal(e),
    }
}

/// DELETE /api/tier-tokens/{name} (admin): revoke. Idempotent.
pub async fn revoke_token_route(
    State(state): State<AppState>,
    identity: Identity,
    Path(name): Path<String>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Admin) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    match conn.execute(
        "UPDATE tier_tokens SET revoked_at = ?1 WHERE name = ?2 AND revoked_at IS NULL",
        params![now_secs(), name],
    ) {
        Ok(n) => Json(json!({ "revoked": n > 0 })).into_response(),
        Err(e) => internal(e),
    }
}

// =====================================================================
// Parent side: sync serving (§8.2 protocol, §8.7 scope enforcement)
// =====================================================================

fn head_etag(head: i64) -> String {
    format!("head-{head}")
}

/// GET /api/reeve/v1/sync/head — conditional GET on the parent's
/// local-stream revision head (§8.2). ETag is a strong validator over
/// the head id; a child whose upstream stream is current gets 304.
pub async fn sync_head_route(
    State(state): State<AppState>,
    _tier: TierIdentity,
    headers: HeaderMap,
) -> Response {
    let head = {
        let store = state.revisions.lock().expect("revisions mutex poisoned");
        match store.head(Stream::Local) {
            Ok(h) => h.unwrap_or(0),
            Err(e) => return internal(e),
        }
    };
    let etag = head_etag(head);
    if crate::delivery::if_none_match_matches(&headers, &etag) {
        return (
            [(header::ETAG, format!("\"{etag}\""))],
            StatusCode::NOT_MODIFIED,
        )
            .into_response();
    }
    (
        [(header::ETAG, format!("\"{etag}\""))],
        Json(SyncHead { head }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SyncRevisionsQuery {
    /// Serve revisions with id > after (ascending). Default 0.
    pub after: Option<i64>,
    /// Page size; default 100, max 500.
    pub limit: Option<usize>,
}

/// GET /api/reeve/v1/sync/revisions?after=N&limit=M — the parent's
/// local-stream chain, ascending, file manifests filtered to the tier
/// token's scope (§8.7). Ascending contiguity is what lets the child
/// append verbatim page by page.
pub async fn sync_revisions_route(
    State(state): State<AppState>,
    tier: TierIdentity,
    Query(q): Query<SyncRevisionsQuery>,
) -> Response {
    let after = q.after.unwrap_or(0);
    let limit = q.limit.unwrap_or(100).min(500);
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    let result: Result<Vec<SyncRevision>, revision_store::Error> = (|| {
        // Walk the chain head -> after (parent pointers ARE the
        // stream), then reverse to ascending.
        let mut ids = Vec::new();
        let mut cursor = store.head(Stream::Local)?;
        while let Some(id) = cursor {
            if id <= after {
                break;
            }
            let rev = store.revision(id)?;
            cursor = rev.parent;
            ids.push(rev);
        }
        ids.reverse();
        ids.truncate(limit);
        let mut out = Vec::with_capacity(ids.len());
        for rev in ids {
            let mut files = store.tree_at(rev.id)?;
            files.retain(|path, _| in_scope(&tier.sync_prefixes, path));
            let digest =
                revision_digest(rev.id, rev.parent, &rev.author, &rev.message, &rev.created_at, &files);
            out.push(SyncRevision {
                id: rev.id,
                parent: rev.parent,
                author: rev.author,
                message: rev.message,
                created_at: rev.created_at,
                digest,
                files,
            });
        }
        Ok(out)
    })();
    match result {
        Ok(revs) => Json(revs).into_response(),
        Err(e) => internal(e),
    }
}

/// GET /api/reeve/v1/sync/blobs/{digest} — content-addressed blob
/// fetch, served ONLY when the digest is referenced by an in-scope
/// path of some local-stream revision (§8.7: a gateway can sync only
/// the blobs in its scope). Out-of-scope digests are 404 — the
/// confidentiality boundary does not confirm existence.
pub async fn sync_blob_route(
    State(state): State<AppState>,
    tier: TierIdentity,
    Path(digest): Path<String>,
) -> Response {
    // Scope check against the paths referencing this digest.
    let allowed = {
        let conn = state.db.lock().expect("db mutex poisoned");
        let paths: Result<Vec<String>, rusqlite::Error> = (|| {
            let mut stmt = conn.prepare_cached(
                "SELECT DISTINCT rf.path
                 FROM revision_files rf JOIN revisions r ON r.id = rf.revision_id
                 WHERE rf.digest = ?1 AND r.stream = 'local'",
            )?;
            let rows = stmt.query_map(params![digest], |r| r.get(0))?;
            rows.collect()
        })();
        match paths {
            Ok(paths) => paths.iter().any(|p| in_scope(&tier.sync_prefixes, p)),
            Err(e) => return internal(e),
        }
    };
    if !allowed {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "unknown blob" }))).into_response();
    }
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    match store.blob(&digest) {
        Ok(Some(bytes)) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "unknown blob" }))).into_response(),
        Err(e) => internal(e),
    }
}

/// Upsert the device row a forwarded ingest names (§8.3: "forwarded
/// devices appear at parent — mark tier-origin"). Refuses devices this
/// token does not own: a locally-enrolled device (tier_origin NULL) or
/// another child's device is 404 — a child can backfill ONLY its own
/// subtree (§8.7), so it cannot fabricate status for anyone else's.
fn ensure_forwarded_device(
    conn: &Connection,
    device_id: &str,
    origin: &str,
) -> Result<bool, rusqlite::Error> {
    let existing: Option<Option<String>> = conn
        .query_row(
            "SELECT tier_origin FROM devices WHERE device_id = ?1",
            params![device_id],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(Some(o)) if o == origin => Ok(true),
        Some(_) => Ok(false), // local device or another child's — refuse
        None => {
            conn.execute(
                "INSERT INTO devices
                     (device_id, hostname, arch, agent_version, enrolled_at, tier_origin)
                 VALUES (?1, '', '', '', ?2, ?3)",
                params![device_id, now_secs(), origin],
            )?;
            Ok(true)
        }
    }
}

/// POST /api/reeve/v1/sync/journal/{device_id} — journal backfill one
/// tier up (§8.3): the SAME JournalBatch/JournalAck wire shape and
/// `(deviceId, seq)` idempotency as the device path
/// (05-health-journal §7.3), authenticated by the tier credential.
pub async fn sync_journal_route(
    State(state): State<AppState>,
    tier: TierIdentity,
    Path(device_id): Path<String>,
    Json(batch): Json<JournalBatch>,
) -> Response {
    {
        let conn = state.db.lock().expect("db mutex poisoned");
        match ensure_forwarded_device(&conn, &device_id, &tier.name) {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "unknown device" })),
                )
                    .into_response();
            }
            Err(e) => return internal(e),
        }
    }
    let ingest = crate::ingest::SqliteStatusIngest::new(state.db.clone(), state.events.clone());
    match ingest.ingest_journal(&device_id, &batch) {
        Ok(ack) => Json(ack).into_response(),
        Err(device_api::status::StatusIngestError::Invalid(m)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": m })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

/// (name, scope, version, ciphertext) as stored in the vault.
#[cfg(feature = "ext-secrets")]
type SecretRow = (String, String, i64, Vec<u8>);

/// The scope of secrets a child may pull (10-secrets §12.5: "only the
/// secrets resolvable within that gateway's subtree"): `fleet`, every
/// `class.*`/`region.*` (any device in the subtree may sit in any
/// class/region — the parent does not track the child's device rows),
/// `site.<child site>`, and `device.<id>` rows for devices this token
/// forwarded. The reserved internal scope is NEVER served.
#[cfg(feature = "ext-secrets")]
fn scoped_secret_rows(
    conn: &Connection,
    site: &str,
    origin: &str,
) -> rusqlite::Result<Vec<SecretRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.name, s.scope, s.version, s.ciphertext FROM secrets s
         WHERE s.scope = 'fleet'
            OR s.scope LIKE 'class.%'
            OR s.scope LIKE 'region.%'
            OR s.scope = 'site.' || ?1
            OR (s.scope LIKE 'device.%' AND EXISTS (
                    SELECT 1 FROM devices d
                    WHERE 'device.' || d.device_id = s.scope AND d.tier_origin = ?2))
         ORDER BY s.name, s.scope",
    )?;
    let rows = stmt.query_map(params![site, origin], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })?;
    rows.collect()
}

/// GET /api/reeve/v1/sync/secrets (tier auth; ext-secrets) — the
/// child's scoped secret set, decrypted here (server RAM only) and
/// re-encrypted by the child under ITS keyfile (per-tier keys, D15).
#[cfg(feature = "ext-secrets")]
pub async fn sync_secrets_route(State(state): State<AppState>, tier: TierIdentity) -> Response {
    let key = match crate::ext::secrets::vault_key(&state.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return internal(e),
    };
    let conn = state.db.lock().expect("db mutex poisoned");
    let rows = match scoped_secret_rows(&conn, &tier.site, &tier.name) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut out = Vec::with_capacity(rows.len());
    for (name, scope, version, ciphertext) in rows {
        let plaintext = match aead::open(&key, &ciphertext) {
            Ok(p) => p,
            Err(e) => return internal(format!("secret {name:?} scope {scope:?}: {e}")),
        };
        let value = match String::from_utf8(plaintext) {
            Ok(v) => v,
            Err(_) => return internal(format!("secret {name:?}: not UTF-8")),
        };
        out.push(SyncSecret { name, scope, version: version as u64, value });
    }
    info!(child = %tier.name, count = out.len(), "scoped secrets served to child tier (§12.5)");
    Json(out).into_response()
}

// =====================================================================
// Child side: the upstream sync client (§8.2 pull-based, resumable)
// =====================================================================

fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn get_setting(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |r| r.get(0),
    )
    .optional()
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?)
}

async fn check_status(resp: reqwest::Response, what: &str) -> anyhow::Result<reqwest::Response> {
    if !resp.status().is_success() {
        anyhow::bail!("{what}: upstream returned {}", resp.status());
    }
    Ok(resp)
}

/// Outcome of one revision-sync pass.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// Revisions appended to the upstream stream this pass.
    pub appended: usize,
    /// Blobs fetched (missing-by-digest only — resume metric).
    pub blobs_fetched: usize,
    /// Parent head after the pass (origin ids).
    pub origin_head: Option<i64>,
}

/// One §8.2 revision sync: conditional GET on the parent head, then
/// page revisions ascending, fetch missing blobs by digest, append
/// verbatim. Resumable at every step (blob inserts are idempotent and
/// individually durable; a revision appears only complete). An
/// id/digest mismatch is surfaced as an ERROR (log + `federation-sync`
/// event via the caller) and stops the pass — never auto-resolved.
pub async fn sync_revisions_once(
    state: &AppState,
    fed: &FederationConfig,
    client: &reqwest::Client,
) -> anyhow::Result<SyncReport> {
    let mut report = SyncReport::default();
    let known_origin: Option<i64> = {
        let store = state.revisions.lock().expect("revisions mutex poisoned");
        store.origin_head(Stream::Upstream)?.map(|(_, origin)| origin)
    };
    report.origin_head = known_origin;

    // Conditional GET on the parent's revision head (§8.2).
    let mut req = client
        .get(format!("{}/api/reeve/v1/sync/head", fed.upstream))
        .bearer_auth(&fed.token);
    if let Some(origin) = known_origin {
        req = req.header(header::IF_NONE_MATCH, format!("\"{}\"", head_etag(origin)));
    }
    let resp = req.send().await?;
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(report); // current — nothing to transfer
    }
    let head: SyncHead = check_status(resp, "sync/head").await?.json().await?;
    let mut after = known_origin.unwrap_or(0);
    if head.head <= after {
        report.origin_head = Some(after);
        return Ok(report);
    }

    'pages: loop {
        let page: Vec<SyncRevision> = check_status(
            client
                .get(format!(
                    "{}/api/reeve/v1/sync/revisions?after={after}&limit=100",
                    fed.upstream
                ))
                .bearer_auth(&fed.token)
                .send()
                .await?,
            "sync/revisions",
        )
        .await?
        .json()
        .await?;
        if page.is_empty() {
            break 'pages;
        }
        for rev in &page {
            // Transport integrity: recompute the identity digest.
            if rev.computed_digest() != rev.digest {
                anyhow::bail!(
                    "sync integrity: revision {} digest mismatch (claimed {}, computed {})",
                    rev.id,
                    rev.digest,
                    rev.computed_digest()
                );
            }
            // Content-addressed blob fetch for what is still missing
            // (§8.2 resumable: already-held digests are skipped, and
            // each fetched blob is durably inserted before the next).
            let missing: Vec<String> = {
                let store = state.revisions.lock().expect("revisions mutex poisoned");
                let mut missing = Vec::new();
                for digest in rev.files.values() {
                    if !store.has_blob(digest)? {
                        missing.push(digest.clone());
                    }
                }
                missing.sort();
                missing.dedup();
                missing
            };
            for digest in missing {
                let bytes = check_status(
                    client
                        .get(format!("{}/api/reeve/v1/sync/blobs/{digest}", fed.upstream))
                        .bearer_auth(&fed.token)
                        .send()
                        .await?,
                    "sync/blobs",
                )
                .await?
                .bytes()
                .await?;
                let mut store = state.revisions.lock().expect("revisions mutex poisoned");
                // put_blob verifies bytes against the claimed digest.
                store.put_blob(&digest, &bytes)?;
                report.blobs_fetched += 1;
            }
            // Verbatim append — visible only with its full closure.
            let outcome = {
                let mut store = state.revisions.lock().expect("revisions mutex poisoned");
                store.append_verbatim(
                    Stream::Upstream,
                    &VerbatimRevision {
                        origin_id: rev.id,
                        origin_parent: rev.parent,
                        author: rev.author.clone(),
                        message: rev.message.clone(),
                        created_at: rev.created_at.clone(),
                        files: rev.files.clone(),
                    },
                )
            };
            match outcome {
                Ok(VerbatimOutcome::Appended(_)) => report.appended += 1,
                Ok(VerbatimOutcome::AlreadyPresent(_)) => {}
                Err(e @ revision_store::Error::VerbatimConflict { .. }) => {
                    // §8.2: single-writer was violated somewhere (or
                    // storage corruption / misbehaving parent). Loud,
                    // never hidden.
                    error!(error = %e, "UPSTREAM SYNC DIVERGENCE (federation §8.2) — refusing to auto-resolve");
                    return Err(e.into());
                }
                Err(e) => return Err(e.into()),
            }
            after = rev.id;
            report.origin_head = Some(after);
        }
        if after >= head.head {
            break 'pages;
        }
    }

    if report.appended > 0 {
        // Two-stream render (§8.2): upstream layers under local ones.
        crate::render::render_all_logged(state);
    }
    Ok(report)
}

// =====================================================================
// Child side: scoped secret pull (10-secrets §12.5, via ext-secrets)
// =====================================================================

/// Pull the scoped secret set and mirror it into the local vault,
/// RE-ENCRYPTED under this tier's own keyfile (per-tier keys, D15).
/// Versions are preserved verbatim (a rotation at the hub arrives as
/// the same version number here — §12.4 propagation stays coherent
/// across tiers); rows previously synced but no longer served are
/// pruned (`origin = 'upstream'` marks them; local rows untouched).
/// Returns whether anything changed (the caller kicks a render pass).
#[cfg(feature = "ext-secrets")]
pub async fn sync_secrets_once(
    state: &AppState,
    fed: &FederationConfig,
    client: &reqwest::Client,
) -> anyhow::Result<bool> {
    let secrets: Vec<SyncSecret> = check_status(
        client
            .get(format!("{}/api/reeve/v1/sync/secrets", fed.upstream))
            .bearer_auth(&fed.token)
            .send()
            .await?,
        "sync/secrets",
    )
    .await?
    .json()
    .await?;

    let key = crate::ext::secrets::vault_key(&state.cfg.data_dir)?;
    let mut conn = state.db.lock().expect("db mutex poisoned");
    let tx = conn.transaction()?;
    let mut changed = false;
    let mut served: std::collections::BTreeSet<(String, String)> = Default::default();
    for s in &secrets {
        served.insert((s.name.clone(), s.scope.clone()));
        let existing: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT version, origin FROM secrets WHERE name = ?1 AND scope = ?2",
                params![s.name, s.scope],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // Unchanged synced row => skip (idempotent tick). A LOCAL row
        // shadowing an upstream-served (name, scope) is a single-writer
        // smell: warn, do not overwrite silently.
        match existing {
            Some((v, Some(ref o))) if o == "upstream" && v as u64 == s.version => continue,
            Some((_, None)) => {
                warn!(name = %s.name, scope = %s.scope,
                      "locally-authored secret shadows an upstream-served one (§8.4 smell); keeping local");
                continue;
            }
            _ => {}
        }
        let ciphertext = aead::seal(&key, s.value.as_bytes())?;
        tx.execute(
            "INSERT INTO secrets (name, scope, version, ciphertext, created_at, origin)
             VALUES (?1, ?2, ?3, ?4, ?5, 'upstream')
             ON CONFLICT(name, scope) DO UPDATE SET
                 version = excluded.version,
                 ciphertext = excluded.ciphertext,
                 rotated_at = excluded.created_at,
                 origin = 'upstream'",
            params![s.name, s.scope, s.version as i64, ciphertext, now_secs()],
        )?;
        changed = true;
    }
    // Prune synced rows the parent no longer serves (deletion
    // propagates like rotation — §12.4/§12.5).
    let stale: Vec<(String, String)> = {
        let mut stmt =
            tx.prepare("SELECT name, scope FROM secrets WHERE origin = 'upstream'")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (name, scope) in stale {
        if !served.contains(&(name.clone(), scope.clone())) {
            tx.execute(
                "DELETE FROM secrets WHERE name = ?1 AND scope = ?2 AND origin = 'upstream'",
                params![name, scope],
            )?;
            changed = true;
        }
    }
    if changed {
        // Same Law-3 shape as a local vault write (ext/secrets.rs):
        // dirty flag in the SAME transaction; a kill -9 before the
        // render pass is healed by startup reconcile.
        set_setting(&tx, crate::render::RENDER_DIRTY_KEY, "1")?;
    }
    tx.commit()?;
    drop(conn);
    if changed {
        crate::render::render_all_logged(state);
    }
    Ok(changed)
}

// =====================================================================
// Child side: status forwarding (§8.3)
// =====================================================================

fn kind_from_storage(s: &str) -> Option<JournalRecordKind> {
    Some(match s {
        "status" => JournalRecordKind::Status,
        "health" => JournalRecordKind::Health,
        "lifecycle" => JournalRecordKind::Lifecycle,
        "gap" => JournalRecordKind::Gap,
        _ => return None,
    })
}

/// Forward journaled status upstream (§8.3): every local device's
/// journal records past its forwarding cursor, as JournalBatch pages
/// to the parent's sync journal endpoint — original timestamps
/// preserved, `(deviceId, seq)` idempotency at every hop. Forwarded
/// devices' records recurse the same way (a grandchild's history
/// reaches the root). The journal itself is the outage buffer; the
/// cursor advances only after the parent persisted a batch.
pub async fn forward_status_once(
    state: &AppState,
    fed: &FederationConfig,
    client: &reqwest::Client,
) -> anyhow::Result<usize> {
    let devices: Vec<(String, i64)> = {
        let conn = state.db.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT d.device_id, COALESCE(f.forwarded_seq, 0)
             FROM devices d LEFT JOIN federation_forward f ON f.device_id = d.device_id
             ORDER BY d.device_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    let mut forwarded_total = 0usize;
    for (device_id, mut cursor) in devices {
        loop {
            let rows: Vec<(i64, String, String, Option<String>)> = {
                let conn = state.db.lock().expect("db mutex poisoned");
                let mut stmt = conn.prepare_cached(
                    "SELECT seq, observed_at, kind, payload FROM status_journal
                     WHERE device_id = ?1 AND seq > ?2 ORDER BY seq LIMIT 200",
                )?;
                let rows = stmt.query_map(params![device_id, cursor], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            if rows.is_empty() {
                break;
            }
            let last_seq = rows.last().expect("non-empty").0;
            let records: Vec<JournalRecord> = rows
                .into_iter()
                .filter_map(|(seq, observed_at, kind, payload)| {
                    let kind = kind_from_storage(&kind)?;
                    Some(JournalRecord {
                        seq: seq.max(0) as u64,
                        observed_at,
                        kind,
                        // Journal payloads are stored as JSON text; a
                        // non-JSON payload rides as a JSON string so
                        // nothing is dropped in transit.
                        payload: payload.map(|p| {
                            serde_json::from_str(&p)
                                .unwrap_or(serde_json::Value::String(p))
                        }),
                    })
                })
                .collect();
            let batch = JournalBatch { records };
            let ack: JournalAck = check_status(
                client
                    .post(format!(
                        "{}/api/reeve/v1/sync/journal/{device_id}",
                        fed.upstream
                    ))
                    .bearer_auth(&fed.token)
                    .json(&batch)
                    .send()
                    .await?,
                "sync/journal",
            )
            .await?
            .json()
            .await?;
            // The parent persisted the WHOLE batch transactionally
            // (ingest.rs), so the cursor moves to the last seq SENT —
            // the contiguous ack may lag behind pre-existing holes
            // (gap-marked eviction) without stalling forwarding.
            forwarded_total += batch.records.len();
            cursor = last_seq;
            let conn = state.db.lock().expect("db mutex poisoned");
            conn.execute(
                "INSERT INTO federation_forward (device_id, forwarded_seq) VALUES (?1, ?2)
                 ON CONFLICT(device_id) DO UPDATE SET forwarded_seq = excluded.forwarded_seq",
                params![device_id, cursor],
            )?;
            let _ = ack; // §7.3 ack semantics live at the parent
        }
    }
    Ok(forwarded_total)
}

// =====================================================================
// Child side: the sync loop + status surface
// =====================================================================

/// One full gateway tick: revisions down, secrets down, status up.
/// Each leg is independent — a failure in one is recorded and the
/// others still run (Law 5 one tier up: partial connectivity degrades,
/// never wedges).
pub async fn sync_tick(state: &AppState) -> anyhow::Result<SyncReport> {
    let fed = state
        .cfg
        .federation
        .clone()
        .ok_or_else(|| anyhow::anyhow!("sync_tick on a root tier"))?;
    let client = http_client()?;

    let mut first_err: Option<anyhow::Error> = None;
    let report = match sync_revisions_once(state, &fed, &client).await {
        Ok(r) => r,
        Err(e) => {
            first_err = Some(e);
            SyncReport::default()
        }
    };
    #[cfg(feature = "ext-secrets")]
    if let Err(e) = sync_secrets_once(state, &fed, &client).await {
        warn!(error = %e, "scoped secret sync failed; retrying next tick (§12.5 queue-on-outage)");
        first_err = first_err.or(Some(e));
    }
    if let Err(e) = forward_status_once(state, &fed, &client).await {
        warn!(error = %e, "status forwarding failed; journal keeps buffering (§8.3)");
        first_err = first_err.or(Some(e));
    }

    // Record + event the tick (§8.2: sync status queryable AND evented).
    let (ok, error_text) = match &first_err {
        None => (true, None),
        Some(e) => (false, Some(e.to_string())),
    };
    {
        let conn = state.db.lock().expect("db mutex poisoned");
        set_setting(&conn, "federation_last_sync_at", &now_secs().to_string())?;
        match &error_text {
            Some(t) => set_setting(&conn, "federation_last_sync_error", t)?,
            None => {
                conn.execute(
                    "DELETE FROM settings WHERE key = 'federation_last_sync_error'",
                    [],
                )?;
            }
        }
    }
    state.events.emit(SseEvent::FederationSync(FederationSyncEvent {
        ts: EventHub::now_ts(),
        upstream: fed.upstream.clone(),
        ok,
        origin_head: report.origin_head.map(|h| h.max(0) as u64),
        appended: report.appended as u64,
        error: error_text,
    }));

    match first_err {
        None => Ok(report),
        Some(e) => Err(e),
    }
}

/// Spawn the periodic gateway loop (REEVE_SYNC_INTERVAL_SECS, §8.2).
/// First tick fires immediately: startup IS recovery (Law 3) — a
/// gateway that crashed mid-sync resumes by digest right away.
pub fn spawn_sync(state: AppState) {
    let interval = state
        .cfg
        .federation
        .as_ref()
        .map(|f| f.sync_interval_secs)
        .unwrap_or(60);
    tokio::spawn(async move {
        loop {
            if let Err(e) = sync_tick(&state).await {
                warn!(error = %e, "federation sync tick failed; continuing from last known state (Law 5)");
            }
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        }
    });
}

/// GET /api/federation/status (viewer+): this tier's federation state
/// (§8.2 queryable). Root tiers report mode "root" and their delegated
/// children; gateways report upstream, sync bookkeeping and cursors.
pub async fn status_route(State(state): State<AppState>, identity: Identity) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let (origin_head, upstream_rows) = {
        let store = state.revisions.lock().expect("revisions mutex poisoned");
        match store.origin_head(Stream::Upstream) {
            Ok(h) => (h.map(|(_, o)| o), h.map(|(row, _)| row)),
            Err(e) => return internal(e),
        }
    };
    let conn = state.db.lock().expect("db mutex poisoned");
    let result: Result<serde_json::Value, rusqlite::Error> = (|| {
        let last_sync_at = get_setting(&conn, "federation_last_sync_at")?
            .and_then(|s| s.parse::<i64>().ok());
        let last_error = get_setting(&conn, "federation_last_sync_error")?;
        let forwarded: i64 = conn.query_row(
            "SELECT COALESCE(SUM(forwarded_seq), 0) FROM federation_forward",
            [],
            |r| r.get(0),
        )?;
        let children: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tier_tokens WHERE revoked_at IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(match &state.cfg.federation {
            Some(fed) => json!({
                "mode": "gateway",
                "upstream": fed.upstream,
                "site": fed.site,
                "syncIntervalSecs": fed.sync_interval_secs,
                "upstreamOriginHead": origin_head,
                "upstreamLocalRow": upstream_rows,
                "lastSyncAt": last_sync_at,
                "lastSyncError": last_error,
                "forwardedSeqTotal": forwarded,
                "childTiers": children,
            }),
            None => json!({
                "mode": "root",
                "childTiers": children,
                "upstreamOriginHead": origin_head,
            }),
        })
    })();
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

// =====================================================================
// Tier identity keys (§8.7; 10-secrets §12.5) — minted at first use
// =====================================================================

/// This tier's ed25519 archive-signing key (§8.5 "signing keys are
/// tier identities"), minted at first use via the keyfile discipline
/// (temp+fsync+rename, 0600).
pub fn signing_key(data_dir: &FsPath) -> anyhow::Result<ed25519_dalek::SigningKey> {
    let seed = keyfile::load_or_create(&data_dir.join(SIGNING_KEY_FILE))?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
}

/// This tier's X25519 sealed-box secret (10-secrets §12.5), minted at
/// first use. Its PUBLIC half is what a parent seals air-gap secrets
/// to; the fingerprint is verified out of band at commissioning.
pub fn x25519_secret(data_dir: &FsPath) -> anyhow::Result<x25519_dalek::StaticSecret> {
    let bytes = keyfile::load_or_create(&data_dir.join(X25519_KEY_FILE))?;
    Ok(x25519_dalek::StaticSecret::from(bytes))
}

/// Print-friendly tier identity: public keys + fingerprints (the out-
/// of-band commissioning artifact, §8.7).
pub fn tier_identity_json(data_dir: &FsPath) -> anyhow::Result<serde_json::Value> {
    use sha2::Digest as _;
    let signing = signing_key(data_dir)?;
    let sealing = x25519_secret(data_dir)?;
    let verifying = signing.verifying_key();
    let sealing_pub = x25519_dalek::PublicKey::from(&sealing);
    let fp = |bytes: &[u8]| hex::encode(sha2::Sha256::digest(bytes))[..16].to_string();
    Ok(json!({
        "ed25519PublicKey": B64.encode(verifying.as_bytes()),
        "ed25519Fingerprint": fp(verifying.as_bytes()),
        "x25519PublicKey": B64.encode(sealing_pub.as_bytes()),
        "x25519Fingerprint": fp(sealing_pub.as_bytes()),
    }))
}

const SEALED_BOX_DOMAIN: &[u8] = b"reeve-sealed-box-v1";

fn sealed_box_key(shared: &[u8; 32], eph_pub: &[u8; 32], recipient_pub: &[u8; 32]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    h.update(SEALED_BOX_DOMAIN);
    h.update(shared);
    h.update(eph_pub);
    h.update(recipient_pub);
    h.finalize().into()
}

/// Seal `plaintext` to a recipient's X25519 public key (10-secrets
/// §12.5 air-gap: "encrypted TO THE DESTINATION GATEWAY'S PUBLIC KEY
/// … never plaintext on media"). See module docs for the recorded
/// construction. Output: `eph_pub(32) || aead envelope`.
pub fn seal_to(recipient_pub: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut eph_bytes = [0u8; 32];
    getrandom::fill(&mut eph_bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let eph = x25519_dalek::StaticSecret::from(eph_bytes);
    let eph_pub = x25519_dalek::PublicKey::from(&eph);
    let shared = eph.diffie_hellman(&x25519_dalek::PublicKey::from(*recipient_pub));
    let key = sealed_box_key(shared.as_bytes(), eph_pub.as_bytes(), recipient_pub);
    let sealed = aead::seal(&key, plaintext)?;
    let mut out = Vec::with_capacity(32 + sealed.len());
    out.extend_from_slice(eph_pub.as_bytes());
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Open a [`seal_to`] blob with this tier's X25519 secret. Any tamper
/// or wrong recipient fails AEAD authentication — loud, never partial.
pub fn open_sealed(own: &x25519_dalek::StaticSecret, blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    if blob.len() < 32 {
        anyhow::bail!("sealed box too short ({} bytes)", blob.len());
    }
    let mut eph_pub = [0u8; 32];
    eph_pub.copy_from_slice(&blob[..32]);
    let own_pub = x25519_dalek::PublicKey::from(own);
    let shared = own.diffie_hellman(&x25519_dalek::PublicKey::from(eph_pub));
    let key = sealed_box_key(shared.as_bytes(), &eph_pub, own_pub.as_bytes());
    aead::open(&key, &blob[32..])
}

// =====================================================================
// Air-gap transfer (§8.5) — signed OCI layout archives
// =====================================================================

/// artifactType of a federation export manifest.
pub const EXPORT_ARTIFACT_TYPE: &str = "application/vnd.reeve.federation-export.v1+json";
/// mediaType of the payload layer.
pub const PAYLOAD_MEDIA_TYPE: &str = "application/vnd.reeve.federation-payload.v1+json";
/// OCI empty config media type (shared with render.rs).
const OCI_EMPTY: &str = "application/vnd.oci.empty.v1+json";
/// The signature side-file: ed25519 over the exact `index.json` bytes.
pub const SIGNATURE_FILE: &str = "reeve.sig";

/// Payload kinds (one archive format for everything on the media —
/// §8.5; `kind` dispatches import).
pub const TREE_PAYLOAD_KIND: &str = "reeve-tree-sync/1";
pub const STATUS_PAYLOAD_KIND: &str = "reeve-status-export/1";

/// Sealed secret set riding a tree export (10-secrets §12.5).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealedSecrets {
    /// b64 X25519 public key of the intended recipient (integrity aid
    /// + operator sanity check; the seal itself binds the key).
    pub recipient: String,
    /// b64 [`seal_to`] blob over the JSON `Vec<SyncSecret>`.
    pub sealed: String,
}

/// The tree-export payload — the §8.2 sync stream, serialized.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreePayload {
    pub kind: String,
    /// Ascending, chain-contiguous parent revisions.
    pub revisions: Vec<SyncRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SealedSecrets>,
}

/// The status-export payload (§8.5 return trip): journal records per
/// device, original timestamps preserved — sneakernet backfill in the
/// 05-health-journal §7.3 form.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusPayload {
    pub kind: String,
    pub devices: Vec<DeviceRecords>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecords {
    pub device_id: String,
    pub records: Vec<JournalRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SignatureFile {
    algo: String,
    /// b64 ed25519 verifying key.
    public_key: String,
    /// b64 signature over the exact index.json bytes.
    signature: String,
}

fn blob_rel_path(digest: &str) -> anyhow::Result<String> {
    let hex_part = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("unsupported digest {digest:?}"))?;
    if hex_part.len() != 64 || !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("malformed digest {digest:?}");
    }
    Ok(format!("blobs/sha256/{hex_part}"))
}

/// Write a signed OCI layout archive. `out` is a directory (created)
/// or a `.tar` file (§8.5: oras/skopeo-compatible layout, inspectable
/// with stock tooling). Atomic enough for media: the signature is
/// written LAST, so a torn write is an unverifiable (rejected) archive.
fn write_archive(
    out: &FsPath,
    payload: &[u8],
    content_blobs: &BTreeMap<String, Vec<u8>>,
    signing: &ed25519_dalek::SigningKey,
) -> anyhow::Result<()> {
    use ed25519_dalek::Signer as _;

    let tar_mode = out.extension().is_some_and(|e| e == "tar");
    let tmp = tempfile::tempdir()?;
    let root = if tar_mode { tmp.path().to_path_buf() } else { out.to_path_buf() };
    std::fs::create_dir_all(root.join("blobs/sha256"))?;

    std::fs::write(root.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;

    let payload_digest = digest_of(payload);
    std::fs::write(root.join(blob_rel_path(&payload_digest)?), payload)?;
    for (digest, bytes) in content_blobs {
        std::fs::write(root.join(blob_rel_path(digest)?), bytes)?;
    }
    let config: &[u8] = b"{}";
    let config_digest = digest_of(config);
    std::fs::write(root.join(blob_rel_path(&config_digest)?), config)?;

    // serde_json object order is insertion order here; bytes are what
    // the digests cover, so no canonicalization concerns.
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": EXPORT_ARTIFACT_TYPE,
        "config": { "mediaType": OCI_EMPTY, "digest": config_digest, "size": config.len() },
        "layers": [{
            "mediaType": PAYLOAD_MEDIA_TYPE,
            "digest": payload_digest,
            "size": payload.len(),
        }],
    }))?;
    let manifest_digest = digest_of(&manifest);
    std::fs::write(root.join(blob_rel_path(&manifest_digest)?), &manifest)?;

    let index = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": EXPORT_ARTIFACT_TYPE,
            "digest": manifest_digest,
            "size": manifest.len(),
        }],
    }))?;
    std::fs::write(root.join("index.json"), &index)?;

    // Sign the archive index (§8.5: "signing covers the archive index
    // and content digests" — the digest chain covers the rest).
    let signature = signing.sign(&index);
    let sig = serde_json::to_vec_pretty(&SignatureFile {
        algo: "ed25519".into(),
        public_key: B64.encode(signing.verifying_key().as_bytes()),
        signature: B64.encode(signature.to_bytes()),
    })?;
    std::fs::write(root.join(SIGNATURE_FILE), sig)?;

    if tar_mode {
        let file = std::fs::File::create(out)?;
        let mut tar = tar::Builder::new(file);
        tar.append_dir_all(".", &root)?;
        tar.into_inner()?.sync_all()?;
    }
    Ok(())
}

/// A verified, in-memory view of an archive: payload bytes + access to
/// content blobs. EVERYTHING is verified before anything is returned —
/// a tampered or truncated archive is rejected whole (§8.5).
struct VerifiedArchive {
    payload: Vec<u8>,
    root: std::path::PathBuf,
    /// Keeps an unpacked .tar alive for the import's duration.
    _tmp: Option<tempfile::TempDir>,
}

impl VerifiedArchive {
    /// Read a content blob, verifying its digest.
    fn blob(&self, digest: &str) -> anyhow::Result<Vec<u8>> {
        let bytes = std::fs::read(self.root.join(blob_rel_path(digest)?))
            .map_err(|e| anyhow::anyhow!("archive blob {digest} unreadable: {e} — truncated media?"))?;
        if digest_of(&bytes) != digest {
            anyhow::bail!("archive blob {digest} content mismatch — tampered media");
        }
        Ok(bytes)
    }
}

/// Open + verify an archive (dir or .tar): signature over index.json,
/// signer pinning (TOFU into settings `federation_trusted_signer`, or
/// `expect_signer` — the b64 ed25519 key), then the content-addressed
/// chain down to the payload.
fn open_archive(
    conn: &Connection,
    path: &FsPath,
    expect_signer: Option<&str>,
) -> anyhow::Result<VerifiedArchive> {
    use ed25519_dalek::Verifier as _;

    let (root, tmp) = if path.is_file() {
        let tmp = tempfile::tempdir()?;
        let mut ar = tar::Archive::new(std::fs::File::open(path)?);
        ar.unpack(tmp.path())?;
        (tmp.path().to_path_buf(), Some(tmp))
    } else {
        (path.to_path_buf(), None)
    };

    let index = std::fs::read(root.join("index.json")).map_err(|e| {
        anyhow::anyhow!("not a reeve archive (no index.json): {e}")
    })?;
    let sig_file: SignatureFile =
        serde_json::from_slice(&std::fs::read(root.join(SIGNATURE_FILE)).map_err(|e| {
            anyhow::anyhow!("unsigned archive rejected (no {SIGNATURE_FILE}): {e}")
        })?)?;
    if sig_file.algo != "ed25519" {
        anyhow::bail!("unsupported signature algo {:?}", sig_file.algo);
    }
    let key_bytes: [u8; 32] = B64
        .decode(&sig_file.public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("malformed signer key"))?;
    let verifying = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes)?;
    let sig_bytes: [u8; 64] = B64
        .decode(&sig_file.signature)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("malformed signature"))?;
    verifying
        .verify(&index, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
        .map_err(|_| anyhow::anyhow!("archive signature INVALID — rejected whole (§8.5)"))?;

    // Signer trust: explicit expectation wins; else pin on first use
    // (key distribution is out of band by definition, §8.7).
    match expect_signer {
        Some(expected) => {
            if expected != sig_file.public_key {
                anyhow::bail!(
                    "archive signed by {} but --expect-signer {} — rejected",
                    sig_file.public_key,
                    expected
                );
            }
        }
        None => match get_setting(conn, "federation_trusted_signer")? {
            Some(pinned) if pinned != sig_file.public_key => {
                anyhow::bail!(
                    "archive signed by {} but this tier trusts {} \
                     (settings federation_trusted_signer) — rejected",
                    sig_file.public_key,
                    pinned
                );
            }
            Some(_) => {}
            None => {
                info!(signer = %sig_file.public_key,
                      "pinning archive signer key (trust-on-first-use; verify the fingerprint out of band, §8.7)");
                set_setting(conn, "federation_trusted_signer", &sig_file.public_key)?;
            }
        },
    }

    // Walk the digest chain: index -> manifest -> payload.
    let index_json: serde_json::Value = serde_json::from_slice(&index)?;
    let manifest_digest = index_json["manifests"][0]["digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("index.json carries no manifest"))?
        .to_string();
    let scratch = VerifiedArchive { payload: Vec::new(), root, _tmp: tmp };
    let manifest_bytes = scratch.blob(&manifest_digest)?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    if manifest["artifactType"].as_str() != Some(EXPORT_ARTIFACT_TYPE) {
        anyhow::bail!(
            "unexpected artifactType {:?}",
            manifest["artifactType"].as_str()
        );
    }
    let payload_digest = manifest["layers"][0]["digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("manifest carries no payload layer"))?
        .to_string();
    let payload = scratch.blob(&payload_digest)?;
    Ok(VerifiedArchive { payload, ..scratch })
}

// =====================================================================
// Air-gap export / import (§8.5) — CLI surface (main.rs subcommands)
// =====================================================================

/// Options for [`export_tree`].
#[derive(Debug, Default)]
pub struct ExportOptions {
    /// Tree-path scope; empty => [`DEFAULT_SYNC_PREFIXES`]. MUST match
    /// what the destination gateway's tier token would grant, so the
    /// media result equals the network sync result (§8.5).
    pub prefixes: Vec<String>,
    /// Destination site — scopes the secret set (10-secrets §12.5).
    pub site: Option<String>,
    /// Destination gateway's X25519 public key (b64); `Some` =>
    /// scoped secrets ride the media sealed to it (requires
    /// ext-secrets to gather them).
    pub recipient_x25519: Option<String>,
}

/// Export this tier's local revision stream (+ optionally scoped,
/// sealed secrets) as a signed OCI layout archive (§8.5). Idempotent:
/// exporting twice produces equivalent archives (timestamps live only
/// inside revision metadata, which is verbatim).
pub fn export_tree(state: &AppState, out: &FsPath, opts: &ExportOptions) -> anyhow::Result<()> {
    let prefixes: Vec<String> = if opts.prefixes.is_empty() {
        DEFAULT_SYNC_PREFIXES.iter().map(|s| s.to_string()).collect()
    } else {
        opts.prefixes.clone()
    };

    // The §8.2 stream, ascending, scope-filtered — identical shape to
    // the network sync response.
    let (revisions, blobs) = {
        let store = state.revisions.lock().expect("revisions mutex poisoned");
        let mut chain = Vec::new();
        let mut cursor = store.head(Stream::Local)?;
        while let Some(id) = cursor {
            let rev = store.revision(id)?;
            cursor = rev.parent;
            chain.push(rev);
        }
        chain.reverse();
        let mut revisions = Vec::with_capacity(chain.len());
        let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for rev in chain {
            let mut files = store.tree_at(rev.id)?;
            files.retain(|path, _| in_scope(&prefixes, path));
            for digest in files.values() {
                if !blobs.contains_key(digest) {
                    let bytes = store.blob(digest)?.ok_or_else(|| {
                        anyhow::anyhow!("store corrupt: missing blob {digest}")
                    })?;
                    blobs.insert(digest.clone(), bytes);
                }
            }
            let digest = revision_digest(
                rev.id, rev.parent, &rev.author, &rev.message, &rev.created_at, &files,
            );
            revisions.push(SyncRevision {
                id: rev.id,
                parent: rev.parent,
                author: rev.author,
                message: rev.message,
                created_at: rev.created_at,
                digest,
                files,
            });
        }
        (revisions, blobs)
    };

    // Scoped secrets, sealed to the destination gateway (§12.5) —
    // never plaintext on media.
    #[allow(unused_mut)]
    let mut secrets: Option<SealedSecrets> = None;
    #[cfg(feature = "ext-secrets")]
    if let Some(recipient_b64) = &opts.recipient_x25519 {
        let recipient: [u8; 32] = B64
            .decode(recipient_b64)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("--recipient must be a b64 32-byte X25519 key"))?;
        let key = crate::ext::secrets::vault_key(&state.cfg.data_dir)?;
        let site = opts.site.as_deref().unwrap_or("");
        let conn = state.db.lock().expect("db mutex poisoned");
        let rows = scoped_secret_rows(&conn, site, "\u{0}never-a-token")?;
        drop(conn);
        let mut set = Vec::with_capacity(rows.len());
        for (name, scope, version, ciphertext) in rows {
            let value = String::from_utf8(aead::open(&key, &ciphertext)?)
                .map_err(|_| anyhow::anyhow!("secret {name:?}: not UTF-8"))?;
            set.push(SyncSecret { name, scope, version: version as u64, value });
        }
        let sealed = seal_to(&recipient, &serde_json::to_vec(&set)?)?;
        secrets = Some(SealedSecrets {
            recipient: recipient_b64.clone(),
            sealed: B64.encode(sealed),
        });
    }
    #[cfg(not(feature = "ext-secrets"))]
    if opts.recipient_x25519.is_some() {
        anyhow::bail!("this binary was built without ext-secrets; cannot export secrets");
    }

    let payload = serde_json::to_vec(&TreePayload {
        kind: TREE_PAYLOAD_KIND.to_string(),
        revisions,
        secrets,
    })?;
    let signing = signing_key(&state.cfg.data_dir)?;
    write_archive(out, &payload, &blobs, &signing)?;
    info!(out = %out.display(), "tree export written (federation §8.5)");
    Ok(())
}

/// Import outcome (both payload kinds).
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub kind: String,
    pub revisions_appended: usize,
    pub revisions_already_present: usize,
    pub secrets_imported: usize,
    pub journal_records: usize,
}

/// Import an archive (dir or .tar): verify signature + integrity
/// FIRST (tampered/truncated => rejected whole, nothing written), then
/// append verbatim under §8.2 rules. Idempotent: re-importing the same
/// archive is a no-op (Law 3 applied to sneakernet).
pub fn import_archive(
    state: &AppState,
    path: &FsPath,
    expect_signer: Option<&str>,
) -> anyhow::Result<ImportReport> {
    let archive = {
        let conn = state.db.lock().expect("db mutex poisoned");
        open_archive(&conn, path, expect_signer)?
    };
    let kind: serde_json::Value = serde_json::from_slice(&archive.payload)?;
    match kind["kind"].as_str() {
        Some(TREE_PAYLOAD_KIND) => import_tree(state, &archive),
        Some(STATUS_PAYLOAD_KIND) => import_status(state, &archive),
        other => anyhow::bail!("unknown payload kind {other:?}"),
    }
}

fn import_tree(state: &AppState, archive: &VerifiedArchive) -> anyhow::Result<ImportReport> {
    let payload: TreePayload = serde_json::from_slice(&archive.payload)?;
    let mut report = ImportReport {
        kind: TREE_PAYLOAD_KIND.to_string(),
        ..ImportReport::default()
    };

    // Phase 1 — verify EVERYTHING before writing anything (§8.5:
    // reject whole): revision identity digests + every referenced
    // blob's presence and content digest.
    let mut blob_bytes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for rev in &payload.revisions {
        if rev.computed_digest() != rev.digest {
            anyhow::bail!(
                "archive revision {} digest mismatch — tampered media, rejected whole",
                rev.id
            );
        }
        for digest in rev.files.values() {
            if !blob_bytes.contains_key(digest) {
                blob_bytes.insert(digest.clone(), archive.blob(digest)?);
            }
        }
    }

    // Phase 2 — verbatim append (idempotent; divergence errors, §8.2).
    {
        let mut store = state.revisions.lock().expect("revisions mutex poisoned");
        for (digest, bytes) in &blob_bytes {
            store.put_blob(digest, bytes)?;
        }
        for rev in &payload.revisions {
            let outcome = store.append_verbatim(
                Stream::Upstream,
                &VerbatimRevision {
                    origin_id: rev.id,
                    origin_parent: rev.parent,
                    author: rev.author.clone(),
                    message: rev.message.clone(),
                    created_at: rev.created_at.clone(),
                    files: rev.files.clone(),
                },
            );
            match outcome {
                Ok(VerbatimOutcome::Appended(_)) => report.revisions_appended += 1,
                Ok(VerbatimOutcome::AlreadyPresent(_)) => {
                    report.revisions_already_present += 1;
                }
                Err(e @ revision_store::Error::VerbatimConflict { .. }) => {
                    error!(error = %e, "IMPORT DIVERGENCE (federation §8.2) — refusing to auto-resolve");
                    return Err(e.into());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    // Sealed secrets (10-secrets §12.5): unseal with THIS tier's key,
    // re-encrypt under the local keyfile — versions verbatim.
    #[cfg(feature = "ext-secrets")]
    if let Some(sealed) = &payload.secrets {
        let own = x25519_secret(&state.cfg.data_dir)?;
        let plaintext = open_sealed(&own, &B64.decode(&sealed.sealed)?)
            .map_err(|e| anyhow::anyhow!("sealed secrets: {e} — wrong destination gateway?"))?;
        let set: Vec<SyncSecret> = serde_json::from_slice(&plaintext)?;
        let key = crate::ext::secrets::vault_key(&state.cfg.data_dir)?;
        let conn = state.db.lock().expect("db mutex poisoned");
        for s in &set {
            let existing: Option<(i64, Option<String>)> = conn
                .query_row(
                    "SELECT version, origin FROM secrets WHERE name = ?1 AND scope = ?2",
                    params![s.name, s.scope],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match existing {
                Some((v, Some(ref o))) if o == "upstream" && v as u64 == s.version => continue,
                Some((_, None)) => continue, // local row wins; §8.4 smell logged on sync path
                _ => {}
            }
            let ciphertext = aead::seal(&key, s.value.as_bytes())?;
            conn.execute(
                "INSERT INTO secrets (name, scope, version, ciphertext, created_at, origin)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'upstream')
                 ON CONFLICT(name, scope) DO UPDATE SET
                     version = excluded.version,
                     ciphertext = excluded.ciphertext,
                     rotated_at = excluded.created_at,
                     origin = 'upstream'",
                params![s.name, s.scope, s.version as i64, ciphertext, now_secs()],
            )?;
            report.secrets_imported += 1;
        }
    }

    if report.revisions_appended > 0 || report.secrets_imported > 0 {
        crate::render::render_all_logged(state);
    }
    Ok(report)
}

/// Export journaled status for sneakernet return (§8.5 "return trip"):
/// ALL journal records per device, original timestamps preserved —
/// `(deviceId, seq)` idempotency at the parent makes re-imports no-ops.
pub fn export_status(state: &AppState, out: &FsPath) -> anyhow::Result<()> {
    let devices = {
        let conn = state.db.lock().expect("db mutex poisoned");
        let ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT device_id FROM status_journal ORDER BY device_id",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut devices = Vec::with_capacity(ids.len());
        for device_id in ids {
            let mut stmt = conn.prepare_cached(
                "SELECT seq, observed_at, kind, payload FROM status_journal
                 WHERE device_id = ?1 ORDER BY seq",
            )?;
            let rows = stmt.query_map(params![device_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?;
            let mut records = Vec::new();
            for row in rows {
                let (seq, observed_at, kind, payload) = row?;
                let Some(kind) = kind_from_storage(&kind) else { continue };
                records.push(JournalRecord {
                    seq: seq.max(0) as u64,
                    observed_at,
                    kind,
                    payload: payload
                        .map(|p| serde_json::from_str(&p).unwrap_or(serde_json::Value::String(p))),
                });
            }
            devices.push(DeviceRecords { device_id, records });
        }
        devices
    };
    let payload = serde_json::to_vec(&StatusPayload {
        kind: STATUS_PAYLOAD_KIND.to_string(),
        devices,
    })?;
    let signing = signing_key(&state.cfg.data_dir)?;
    write_archive(out, &payload, &BTreeMap::new(), &signing)?;
    info!(out = %out.display(), "status export written (federation §8.5 return trip)");
    Ok(())
}

fn import_status(state: &AppState, archive: &VerifiedArchive) -> anyhow::Result<ImportReport> {
    let payload: StatusPayload = serde_json::from_slice(&archive.payload)?;
    let mut report = ImportReport {
        kind: STATUS_PAYLOAD_KIND.to_string(),
        ..ImportReport::default()
    };
    let ingest = crate::ingest::SqliteStatusIngest::new(state.db.clone(), state.events.clone());
    for dev in &payload.devices {
        {
            let conn = state.db.lock().expect("db mutex poisoned");
            if !ensure_forwarded_device(&conn, &dev.device_id, "airgap")? {
                warn!(device = %dev.device_id,
                      "status import names a device this tier owns or another child forwarded — skipped (§8.7)");
                continue;
            }
        }
        let batch = JournalBatch { records: dev.records.clone() };
        ingest
            .ingest_journal(&dev.device_id, &batch)
            .map_err(|e| anyhow::anyhow!("journal import for {}: {e}", dev.device_id))?;
        report.journal_records += dev.records.len();
    }
    Ok(report)
}

// =====================================================================
// Unit tests (the two-server e2e lives in tests/federation_flow.rs)
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_digest_is_boundary_safe_and_field_sensitive() {
        let files: BTreeMap<String, String> =
            [("a".to_string(), "sha256:aa".to_string())].into();
        let base = revision_digest(1, None, "op", "msg", "T0", &files);
        assert_eq!(base, revision_digest(1, None, "op", "msg", "T0", &files));
        assert_ne!(base, revision_digest(2, None, "op", "msg", "T0", &files));
        assert_ne!(base, revision_digest(1, Some(0), "op", "msg", "T0", &files));
        assert_ne!(base, revision_digest(1, None, "op2", "msg", "T0", &files));
        assert_ne!(base, revision_digest(1, None, "op", "msg2", "T0", &files));
        assert_ne!(base, revision_digest(1, None, "op", "msg", "T1", &files));
        // author/message boundary shift
        assert_ne!(
            revision_digest(1, None, "ab", "c", "T0", &files),
            revision_digest(1, None, "a", "bc", "T0", &files)
        );
        let other: BTreeMap<String, String> =
            [("a".to_string(), "sha256:bb".to_string())].into();
        assert_ne!(base, revision_digest(1, None, "op", "msg", "T0", &other));
    }

    #[test]
    fn scope_filter_matches_ownership_rules() {
        let prefixes: Vec<String> =
            DEFAULT_SYNC_PREFIXES.iter().map(|s| s.to_string()).collect();
        for path in [
            "layers/00-fleet/apps/web/app.yaml",
            "layers/05-class.gpu/apps/x/app.yaml",
            "layers/10-region.emea/params.yaml",
            "packages/web/1.0.0/margo.yaml",
        ] {
            assert!(in_scope(&prefixes, path), "{path} should be in scope");
        }
        for path in [
            "layers/20-site.plant-a/apps/web/app.yaml",
            "layers/30-device.dev-1/apps/web/app.yaml",
            "layers/00-fleet-other/x", // no boundary match
        ] {
            assert!(!in_scope(&prefixes, path), "{path} should be OUT of scope");
        }
    }

    #[test]
    fn sealed_box_roundtrip_wrong_key_and_tamper_fail() {
        let mut a = [0u8; 32];
        getrandom::fill(&mut a).unwrap();
        let recipient = x25519_dalek::StaticSecret::from(a);
        let recipient_pub = x25519_dalek::PublicKey::from(&recipient);

        let sealed = seal_to(recipient_pub.as_bytes(), b"scoped secrets").unwrap();
        assert_eq!(open_sealed(&recipient, &sealed).unwrap(), b"scoped secrets");

        // Wrong recipient key.
        let mut b = [1u8; 32];
        getrandom::fill(&mut b).unwrap();
        let other = x25519_dalek::StaticSecret::from(b);
        assert!(open_sealed(&other, &sealed).is_err());

        // Tamper.
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open_sealed(&recipient, &tampered).is_err());
        assert!(open_sealed(&recipient, &sealed[..20]).is_err());
    }

    #[test]
    fn blob_rel_path_rejects_traversal() {
        assert!(blob_rel_path(&digest_of(b"x")).is_ok());
        assert!(blob_rel_path("sha256:../../etc/passwd").is_err());
        assert!(blob_rel_path("md5:abc").is_err());
        assert!(blob_rel_path("sha256:short").is_err());
    }
}
