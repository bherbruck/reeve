//! Server configuration from environment (CLAUDE.md: config via
//! env/files; never commit values, only shape).
//!
//! Shape (.env rule):
//! - REEVE_LISTEN                default 0.0.0.0:8420
//! - REEVE_DATA_DIR              default ./data  (single SQLite DB lives here)
//! - REEVE_AUTH                  password (default) | proxy | none  (D1)
//! - REEVE_PROXY_USER_HEADER     proxy mode: REQUIRED header carrying the user
//! - REEVE_PROXY_ROLE_HEADER     proxy mode: optional header carrying the role
//! - REEVE_PROXY_TRUSTED_CIDR    proxy mode: REQUIRED comma-separated CIDRs
//! - REEVE_SESSION_TTL_SECS      password mode: sliding idle TTL, default 604800 (7d)
//! - REEVE_REGISTRY              tier registry endpoint substituted for
//!   ${REEVE_REGISTRY} at render time (docs/decisions/delivery.md D8;
//!   declared render input, tree-render.md D3); default
//!   localhost:<listen port>
//!
//! Zot image-registry proxy (C11, docs/decisions/delivery.md D8 —
//! /v2 image repos reverse-proxy to the zot sidecar; unset => the
//! proxy is absent and non-native /v2 repos 404):
//! - REEVE_ZOT_URL               backend base url, http:// only (the
//!   sidecar is co-located; zot accepts connections only from
//!   reeve-server, D8). Unset => no proxy.
//! - REEVE_ZOT_USERNAME          optional Basic credential the proxy
//! - REEVE_ZOT_PASSWORD          injects toward zot (set together).
//!   Unset => vault internal-scope `zot.upstream.username`/`.password`
//!   (spec/reeve/10-secrets.md §12.2 typed getter) when ext-secrets is
//!   compiled in; otherwise anonymous toward zot.
//!
//! Durability (C6, spec/reeve/07-durability.md — tiers are config, not
//! surgery, §9.1):
//! - REEVE_DURABILITY            none (default) | snapshot | changeset
//! - REEVE_DURABILITY_TARGET     object-store url: s3://bucket/prefix,
//!   file:///abs/path, or a plain filesystem path (test/air-gap tier);
//!   REQUIRED for any tier other than none
//! - REEVE_DURABILITY_INSTANCE   key namespace `reeve/<instance>/…`
//!   at the target; default "default"
//! - REEVE_DURABILITY_SNAPSHOT_INTERVAL_SECS   default 900 (§9.2)
//! - REEVE_DURABILITY_RETAIN_DAYS              default 7 (§9.2)
//! - REEVE_DURABILITY_RETAIN_MIN_GENERATIONS   default 8 (§9.2)
//! - REEVE_DURABILITY_CHANGESET_INTERVAL_SECS  default 5 (§9.3)
//! - REEVE_DURABILITY_CHANGESET_COMMITS        default 100 (§9.3)
//! - REEVE_DURABILITY_VERIFY_INTERVAL_SECS     default 86400 (§9.4)
//!
//! Federation (C10, spec/reeve/06-federation.md §8.1; docs/decisions/
//! deploy.md D9: tier selection is REEVE_UPSTREAM presence — same
//! binary, same image, no mode flag):
//! Install bootstrap (C12, spec/reeve/08-packaging.md §10.4; only
//! consulted by builds with the `embedded-agents` feature):
//! - REEVE_INSTALL_OPEN          true|1 => GET /install and the agent
//!   artifact pulls skip the enrollment-token requirement (trusted
//!   networks only). Default: closed.
//!
//! Deploy logs (REV-011, server `ext-logs`):
//! - REEVE_LOGS_RETAIN_PER_DEPLOYMENT  recent log runs kept per
//!   (device, deployment); older pruned on insert. Default 10.
//!
//! - REEVE_UPSTREAM               parent tier base URL; present =>
//!   this instance is a gateway tier, absent => root (unchanged)
//! - REEVE_UPSTREAM_TOKEN         tier credential presented upstream
//!   (§8.7 scoped tier tokens); REQUIRED when REEVE_UPSTREAM is set
//! - REEVE_SITE                   site label this gateway owns (its
//!   `20-site.<label>` overlay layer, §8.4 single writer); REQUIRED
//!   when REEVE_UPSTREAM is set
//! - REEVE_SYNC_INTERVAL_SECS     sync loop period, default 60
//!
//! Server tier declaration (spec/reeve/11-fleet-model.md §11.6):
//! - REEVE_TIER                   root (default) | site. A `site` tier
//!   is a gateway and REQUIRES REEVE_UPSTREAM + REEVE_SITE (error
//!   otherwise); `root` ignores them.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context as _, bail};

use crate::auth::cidr::Cidr;

/// Human auth mode (docs/decisions/auth.md D1).
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// Local users table (argon2id) + SQLite-backed session cookies.
    Password,
    /// Trust a user header from a fronting auth proxy — only from
    /// trusted peers. D1: MUST refuse startup unless the trusted CIDR
    /// list is set.
    Proxy(ProxyConfig),
    /// Anonymous is admin. Bench and air-gapped dev only.
    None,
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Header carrying the authenticated username (e.g. Remote-User).
    pub user_header: String,
    /// Optional header carrying the role (admin|operator|viewer).
    /// Absent header => admin (the proxy gates who reaches us at all);
    /// unknown value => viewer (least privilege on misconfig).
    pub role_header: Option<String>,
    /// Peers allowed to assert the user header. Never trust the world.
    pub trusted: Vec<Cidr>,
}

/// Server tier declaration (spec/reeve/11-fleet-model.md §11.6): a
/// convenience/clarity statement of where this instance sits in the
/// topology. The operative federation behavior is still driven by
/// `REEVE_UPSTREAM` presence (deploy.md D9) — `Site` merely names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerTier {
    /// The cloud/hub (default). Serves every level; no upstream.
    Root,
    /// An on-prem site gateway: MUST also set `REEVE_UPSTREAM` and
    /// `REEVE_SITE` (federation §8.1). Belongs to exactly one Site.
    Site,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub auth: AuthMode,
    pub session_ttl_secs: i64,
    /// Declared topology tier (§11.6). `Site` requires federation to be
    /// configured (`REEVE_UPSTREAM` + `REEVE_SITE`); `Root` is the
    /// default and ignores them.
    pub tier: ServerTier,
    /// Tier registry endpoint (docs/decisions/delivery.md D8): the value
    /// `${REEVE_REGISTRY}` resolves to at render time. A DECLARED render
    /// input (tree-render.md D3) — it enters render via `RenderContext`,
    /// never via environment reads in the render path.
    pub registry_endpoint: String,
    /// Durability tier configuration (C6, spec/reeve/07-durability.md).
    pub durability: DurabilityConfig,
    /// Zot sidecar reverse-proxy (C11, docs/decisions/delivery.md D8):
    /// non-`reeve/*` /v2 repos proxy here. `None` => proxy absent —
    /// proxied repos fall through to the native 404.
    pub zot: Option<ZotConfig>,
    /// Federation tier (C10, spec/reeve/06-federation.md §8.1;
    /// deploy.md D9): `Some` => gateway syncing from a parent tier,
    /// `None` => root. Presence also selects
    /// [`crate::ownership::Ownership::Gateway`] at bootstrap (§8.4).
    pub federation: Option<FederationConfig>,
    /// C12 §10.4: open the /install bootstrap (no enrollment token)
    /// on trusted networks. Parsed unconditionally, consumed only by
    /// `embedded-agents` builds. Default false — closed.
    pub install_open: bool,
    /// REV-011 deploy logs (server `ext-logs`): how many recent log
    /// runs to retain per (device, deployment) — older ones are pruned
    /// on insert (REEVE_LOGS_RETAIN_PER_DEPLOYMENT, default 10). Parsed
    /// unconditionally, consumed only by the `ext-logs` LogStore.
    pub logs_retain_per_deployment: u64,
    /// Optional first-boot admin seed (REEVE_ADMIN_USER +
    /// REEVE_ADMIN_PASSWORD). When both are set and the users table is
    /// empty, password-mode bootstrap creates this admin instead of
    /// minting a one-time setup token — no token-copying dance. Ignored
    /// once any user exists (idempotent). Convenience for dev/automated
    /// bring-up; a real deployment uses the setup flow or proxy SSO.
    pub admin_seed: Option<(String, String)>,
}

/// Gateway-tier configuration (spec/reeve/06-federation.md §8.1:
/// "configuration is one optional value: `upstream` (URL + credentials
/// for the parent tier)" — plus the site layer this tier owns, §8.4).
#[derive(Debug, Clone)]
pub struct FederationConfig {
    /// Parent tier base URL, normalized without a trailing slash.
    pub upstream: String,
    /// Tier credential (`rvt_…`) presented on every sync/backfill call
    /// (§8.7: the parent enforces its scope server-side).
    pub token: String,
    /// The site label whose `20-site.<site>` layer THIS tier authors
    /// (§8.4: a gateway authors its own site layer only, plus its
    /// locally-enrolled device layers).
    pub site: String,
    /// Sync loop period (revisions + secrets + status forwarding).
    pub sync_interval_secs: u64,
}

/// Backend for the /v2 image reverse proxy (docs/decisions/delivery.md
/// D8: "reeve-server reverse-proxies image /v2/* routes to the zot
/// sidecar"; spec/reeve/08-packaging.md §10.2: one /v2 space, one
/// listening socket).
#[derive(Debug, Clone)]
pub struct ZotConfig {
    /// Base URL, e.g. `http://127.0.0.1:5000` — normalized without a
    /// trailing slash. http only: the sidecar is co-located and
    /// firewalled to reeve-server (D8), so the boring hyper-util
    /// client needs no TLS stack.
    pub url: String,
    /// Basic credential injected toward zot (D8: "the proxy terminates
    /// device auth and speaks its own credential to zot"). Env pair
    /// wins; absent => vault internal scope, else anonymous.
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Durability tier selection (spec/reeve/07-durability.md §9.1: tiers
/// `none` | `snapshot` | `snapshot+changeset` are config, not surgery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityTier {
    None,
    Snapshot,
    /// Snapshot + seconds-RPO changeset streaming (§9.3). Requires the
    /// `ext-durability-changeset` cargo feature; a core-only binary
    /// refuses startup with this tier configured.
    Changeset,
}

#[derive(Debug, Clone)]
pub struct DurabilityConfig {
    pub tier: DurabilityTier,
    /// Object-store target url (§9.2): `s3://…`, `file://…`, or a plain
    /// filesystem path. `Some` iff tier != None (validated at parse).
    pub target: Option<String>,
    /// Key namespace at the target: `reeve/<instance>/…`.
    pub instance: String,
    pub snapshot_interval_secs: u64,
    pub retain_days: u64,
    pub retain_min_generations: u64,
    pub changeset_interval_secs: u64,
    pub changeset_commits: u64,
    pub verify_interval_secs: u64,
}

impl DurabilityConfig {
    /// The disabled tier — what absent env yields, and what tests that
    /// don't exercise durability use.
    pub fn disabled() -> Self {
        DurabilityConfig {
            tier: DurabilityTier::None,
            target: None,
            instance: "default".into(),
            snapshot_interval_secs: 900,
            retain_days: 7,
            retain_min_generations: 8,
            changeset_interval_secs: 5,
            changeset_commits: 100,
            verify_interval_secs: 86_400,
        }
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        // An empty (or whitespace-only) env var reads as UNSET. Docker
        // Compose materializes `${VAR:-}` as `VAR=""`, so without this a
        // blank REEVE_UPSTREAM/REEVE_ZOT_URL/etc. would be parsed as a
        // present-but-invalid value and refuse to boot. Empty == absent
        // is the 12-factor convention and what an operator means.
        Self::from_lookup(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    /// Testable core: build config from any key lookup. Proxy mode
    /// without REEVE_PROXY_TRUSTED_CIDR (or without a user header) is a
    /// startup refusal (D1), not a lenient default.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let listen: SocketAddr = get("REEVE_LISTEN")
            .unwrap_or_else(|| "0.0.0.0:8420".into())
            .parse()
            .context("REEVE_LISTEN must be host:port")?;

        let data_dir = PathBuf::from(get("REEVE_DATA_DIR").unwrap_or_else(|| "./data".into()));

        let session_ttl_secs: i64 = match get("REEVE_SESSION_TTL_SECS") {
            Some(v) => v.parse().context("REEVE_SESSION_TTL_SECS must be an integer")?,
            None => 7 * 24 * 3600,
        };
        if session_ttl_secs <= 0 {
            bail!("REEVE_SESSION_TTL_SECS must be positive");
        }

        let auth = match get("REEVE_AUTH").as_deref().unwrap_or("password") {
            "password" => AuthMode::Password,
            "none" => AuthMode::None,
            "proxy" => {
                let Some(user_header) = get("REEVE_PROXY_USER_HEADER") else {
                    bail!(
                        "REEVE_AUTH=proxy requires REEVE_PROXY_USER_HEADER \
                         (the header the fronting auth proxy sets)"
                    );
                };
                let Some(cidrs) = get("REEVE_PROXY_TRUSTED_CIDR") else {
                    // D1: MUST refuse to start — never trust the header
                    // from the world.
                    bail!(
                        "REEVE_AUTH=proxy requires REEVE_PROXY_TRUSTED_CIDR \
                         (comma-separated CIDRs of the trusted proxy)"
                    );
                };
                let trusted = cidrs
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse::<Cidr>())
                    .collect::<Result<Vec<_>, _>>()
                    .context("REEVE_PROXY_TRUSTED_CIDR parse")?;
                if trusted.is_empty() {
                    bail!("REEVE_PROXY_TRUSTED_CIDR must contain at least one CIDR");
                }
                AuthMode::Proxy(ProxyConfig {
                    user_header,
                    role_header: get("REEVE_PROXY_ROLE_HEADER"),
                    trusted,
                })
            }
            other => bail!("REEVE_AUTH must be password|proxy|none, got {other:?}"),
        };

        let registry_endpoint =
            get("REEVE_REGISTRY").unwrap_or_else(|| format!("localhost:{}", listen.port()));

        let durability = Self::durability_from_lookup(&get)?;
        let zot = Self::zot_from_lookup(&get)?;
        let federation = Self::federation_from_lookup(&get)?;

        // Server tier declaration (§11.6). A `site` tier is a gateway and
        // therefore MUST have federation configured (REEVE_UPSTREAM +
        // REEVE_SITE, which federation_from_lookup already validates
        // together); a `site` without an upstream is a config error. A
        // `root` tier ignores REEVE_SITE/REEVE_UPSTREAM (they may be
        // absent). The operative behavior remains upstream-presence
        // driven (D9); this is the clarity declaration.
        let tier = match get("REEVE_TIER").as_deref().unwrap_or("root") {
            "root" => ServerTier::Root,
            "site" => {
                if federation.is_none() {
                    bail!(
                        "REEVE_TIER=site requires REEVE_UPSTREAM and REEVE_SITE \
                         (a site gateway syncs from its parent tier, federation §8.1)"
                    );
                }
                ServerTier::Site
            }
            other => bail!("REEVE_TIER must be root|site, got {other:?}"),
        };

        let install_open = match get("REEVE_INSTALL_OPEN").as_deref() {
            None | Some("") | Some("false") | Some("0") => false,
            Some("true") | Some("1") => true,
            Some(other) => bail!("REEVE_INSTALL_OPEN must be true|false, got {other:?}"),
        };

        let logs_retain_per_deployment: u64 = match get("REEVE_LOGS_RETAIN_PER_DEPLOYMENT") {
            Some(v) => v
                .parse()
                .context("REEVE_LOGS_RETAIN_PER_DEPLOYMENT must be an integer")?,
            None => 10,
        };
        if logs_retain_per_deployment == 0 {
            bail!("REEVE_LOGS_RETAIN_PER_DEPLOYMENT must be positive");
        }

        let admin_seed = match (get("REEVE_ADMIN_USER"), get("REEVE_ADMIN_PASSWORD")) {
            (Some(u), Some(p)) => Some((u, p)),
            (Some(_), None) | (None, Some(_)) => {
                bail!("REEVE_ADMIN_USER and REEVE_ADMIN_PASSWORD must be set together");
            }
            (None, None) => None,
        };

        Ok(Config {
            listen,
            data_dir,
            auth,
            session_ttl_secs,
            tier,
            registry_endpoint,
            durability,
            zot,
            federation,
            install_open,
            logs_retain_per_deployment,
            admin_seed,
        })
    }

    /// Federation tier selection (C10, deploy.md D9: REEVE_UPSTREAM
    /// presence IS the tier). Fail closed at startup: an upstream
    /// without a credential or a site is a misconfiguration — a
    /// gateway that cannot sync or does not know which layer it owns
    /// must not boot as a silent root.
    fn federation_from_lookup(
        get: &impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Option<FederationConfig>> {
        let Some(raw) = get("REEVE_UPSTREAM") else {
            if get("REEVE_UPSTREAM_TOKEN").is_some() || get("REEVE_SITE").is_some() {
                bail!("REEVE_UPSTREAM_TOKEN/REEVE_SITE require REEVE_UPSTREAM");
            }
            return Ok(None);
        };
        let parsed = url::Url::parse(&raw)
            .context("REEVE_UPSTREAM must be an absolute URL, e.g. https://hub.example:8420")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("REEVE_UPSTREAM must be http(s)://, got scheme {:?}", parsed.scheme());
        }
        let Some(token) = get("REEVE_UPSTREAM_TOKEN") else {
            bail!("REEVE_UPSTREAM requires REEVE_UPSTREAM_TOKEN (the tier credential, §8.7)");
        };
        let Some(site) = get("REEVE_SITE") else {
            bail!(
                "REEVE_UPSTREAM requires REEVE_SITE (the site layer this gateway owns, \
                 federation §8.4)"
            );
        };
        // Same label grammar as a `20-site.<label>` layer dir (D11):
        // the value is spliced into ownership prefixes and layer paths.
        if site.is_empty()
            || site.len() > 128
            || !site.as_bytes()[0].is_ascii_alphanumeric()
            || site.ends_with('.')
            || site
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        {
            bail!("REEVE_SITE must be a valid site label ([A-Za-z0-9._-], alphanumeric start)");
        }
        let sync_interval_secs: u64 = match get("REEVE_SYNC_INTERVAL_SECS") {
            Some(v) => v.parse().context("REEVE_SYNC_INTERVAL_SECS must be an integer")?,
            None => 60,
        };
        if sync_interval_secs == 0 {
            bail!("REEVE_SYNC_INTERVAL_SECS must be positive");
        }
        Ok(Some(FederationConfig {
            upstream: raw.trim_end_matches('/').to_string(),
            token,
            site,
            sync_interval_secs,
        }))
    }

    /// Zot proxy backend (C11, docs/decisions/delivery.md D8). Fail
    /// closed on misconfiguration at startup: creds without a URL,
    /// half a credential pair, or a non-http scheme all refuse boot.
    fn zot_from_lookup(get: &impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<ZotConfig>> {
        let Some(raw) = get("REEVE_ZOT_URL") else {
            if get("REEVE_ZOT_USERNAME").is_some() || get("REEVE_ZOT_PASSWORD").is_some() {
                bail!("REEVE_ZOT_USERNAME/REEVE_ZOT_PASSWORD require REEVE_ZOT_URL");
            }
            return Ok(None);
        };
        let parsed = url::Url::parse(&raw)
            .context("REEVE_ZOT_URL must be an absolute URL, e.g. http://127.0.0.1:5000")?;
        if parsed.scheme() != "http" {
            // The sidecar is co-located and accepts connections only
            // from reeve-server (D8) — plain http keeps the proxy
            // client boring (no TLS stack). https is a misconfig, not
            // a degraded mode.
            bail!(
                "REEVE_ZOT_URL must be http:// (co-located sidecar, D8), got scheme {:?}",
                parsed.scheme()
            );
        }
        if parsed.host_str().is_none() {
            bail!("REEVE_ZOT_URL must carry a host");
        }
        let username = get("REEVE_ZOT_USERNAME");
        let password = get("REEVE_ZOT_PASSWORD");
        if username.is_some() != password.is_some() {
            bail!("REEVE_ZOT_USERNAME and REEVE_ZOT_PASSWORD must be set together");
        }
        Ok(Some(ZotConfig {
            url: raw.trim_end_matches('/').to_string(),
            username,
            password,
        }))
    }

    fn durability_from_lookup(
        get: &impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<DurabilityConfig> {
        let tier = match get("REEVE_DURABILITY").as_deref().unwrap_or("none") {
            "none" => DurabilityTier::None,
            "snapshot" => DurabilityTier::Snapshot,
            "changeset" => DurabilityTier::Changeset,
            other => bail!("REEVE_DURABILITY must be none|snapshot|changeset, got {other:?}"),
        };
        let target = get("REEVE_DURABILITY_TARGET");
        if tier != DurabilityTier::None && target.is_none() {
            // Fail closed at startup, not at first snapshot: a tier
            // without a target is a misconfiguration, not a degraded
            // state (spec/reeve/07-durability.md §9.2).
            bail!("REEVE_DURABILITY={tier:?} requires REEVE_DURABILITY_TARGET");
        }
        let parse_u64 = |key: &str, default: u64| -> anyhow::Result<u64> {
            match get(key) {
                Some(v) => v.parse().with_context(|| format!("{key} must be an integer")),
                None => Ok(default),
            }
        };
        Ok(DurabilityConfig {
            tier,
            target,
            instance: get("REEVE_DURABILITY_INSTANCE").unwrap_or_else(|| "default".into()),
            snapshot_interval_secs: parse_u64("REEVE_DURABILITY_SNAPSHOT_INTERVAL_SECS", 900)?,
            retain_days: parse_u64("REEVE_DURABILITY_RETAIN_DAYS", 7)?,
            retain_min_generations: parse_u64("REEVE_DURABILITY_RETAIN_MIN_GENERATIONS", 8)?,
            changeset_interval_secs: parse_u64("REEVE_DURABILITY_CHANGESET_INTERVAL_SECS", 5)?,
            changeset_commits: parse_u64("REEVE_DURABILITY_CHANGESET_COMMITS", 100)?,
            verify_interval_secs: parse_u64("REEVE_DURABILITY_VERIFY_INTERVAL_SECS", 86_400)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_to_password_mode() {
        let c = cfg(&[]).unwrap();
        assert!(matches!(c.auth, AuthMode::Password));
        assert_eq!(c.session_ttl_secs, 604800);
    }

    #[test]
    fn proxy_without_trusted_cidr_refuses_startup() {
        let err = cfg(&[
            ("REEVE_AUTH", "proxy"),
            ("REEVE_PROXY_USER_HEADER", "Remote-User"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("REEVE_PROXY_TRUSTED_CIDR"));
    }

    #[test]
    fn proxy_without_user_header_refuses_startup() {
        let err = cfg(&[
            ("REEVE_AUTH", "proxy"),
            ("REEVE_PROXY_TRUSTED_CIDR", "10.0.0.0/8"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("REEVE_PROXY_USER_HEADER"));
    }

    #[test]
    fn proxy_with_both_parses() {
        let c = cfg(&[
            ("REEVE_AUTH", "proxy"),
            ("REEVE_PROXY_USER_HEADER", "Remote-User"),
            ("REEVE_PROXY_TRUSTED_CIDR", "10.0.0.0/8, 127.0.0.1"),
        ])
        .unwrap();
        match c.auth {
            AuthMode::Proxy(p) => {
                assert_eq!(p.user_header, "Remote-User");
                assert_eq!(p.trusted.len(), 2);
            }
            other => panic!("expected proxy mode, got {other:?}"),
        }
    }

    #[test]
    fn bad_mode_is_an_error() {
        assert!(cfg(&[("REEVE_AUTH", "oidc")]).is_err());
    }

    #[test]
    fn durability_defaults_to_none() {
        let c = cfg(&[]).unwrap();
        assert_eq!(c.durability.tier, DurabilityTier::None);
        assert_eq!(c.durability.snapshot_interval_secs, 900);
        assert_eq!(c.durability.retain_days, 7);
        assert_eq!(c.durability.retain_min_generations, 8);
        assert_eq!(c.durability.changeset_interval_secs, 5);
        assert_eq!(c.durability.changeset_commits, 100);
        assert_eq!(c.durability.verify_interval_secs, 86_400);
    }

    #[test]
    fn durability_tier_without_target_refuses_startup() {
        let err = cfg(&[("REEVE_DURABILITY", "snapshot")]).unwrap_err();
        assert!(err.to_string().contains("REEVE_DURABILITY_TARGET"));
        let err = cfg(&[("REEVE_DURABILITY", "changeset")]).unwrap_err();
        assert!(err.to_string().contains("REEVE_DURABILITY_TARGET"));
    }

    #[test]
    fn durability_tier_with_target_parses() {
        let c = cfg(&[
            ("REEVE_DURABILITY", "changeset"),
            ("REEVE_DURABILITY_TARGET", "s3://bucket/prefix"),
            ("REEVE_DURABILITY_INSTANCE", "edge-1"),
            ("REEVE_DURABILITY_SNAPSHOT_INTERVAL_SECS", "60"),
        ])
        .unwrap();
        assert_eq!(c.durability.tier, DurabilityTier::Changeset);
        assert_eq!(c.durability.target.as_deref(), Some("s3://bucket/prefix"));
        assert_eq!(c.durability.instance, "edge-1");
        assert_eq!(c.durability.snapshot_interval_secs, 60);
    }

    #[test]
    fn bad_durability_tier_is_an_error() {
        assert!(cfg(&[("REEVE_DURABILITY", "litestream")]).is_err());
    }

    #[test]
    fn zot_defaults_to_absent() {
        assert!(cfg(&[]).unwrap().zot.is_none());
    }

    #[test]
    fn zot_url_parses_and_normalizes() {
        let c = cfg(&[("REEVE_ZOT_URL", "http://127.0.0.1:5000/")]).unwrap();
        let zot = c.zot.expect("zot configured");
        assert_eq!(zot.url, "http://127.0.0.1:5000");
        assert!(zot.username.is_none() && zot.password.is_none());
    }

    #[test]
    fn zot_env_credentials_parse() {
        let c = cfg(&[
            ("REEVE_ZOT_URL", "http://zot:5000"),
            ("REEVE_ZOT_USERNAME", "reeve"),
            ("REEVE_ZOT_PASSWORD", "hunter2"),
        ])
        .unwrap();
        let zot = c.zot.unwrap();
        assert_eq!(zot.username.as_deref(), Some("reeve"));
        assert_eq!(zot.password.as_deref(), Some("hunter2"));
    }

    #[test]
    fn federation_defaults_to_root() {
        assert!(cfg(&[]).unwrap().federation.is_none());
    }

    #[test]
    fn tier_defaults_to_root() {
        assert_eq!(cfg(&[]).unwrap().tier, ServerTier::Root);
        // Explicit root ignores absent upstream/site.
        assert_eq!(cfg(&[("REEVE_TIER", "root")]).unwrap().tier, ServerTier::Root);
    }

    #[test]
    fn site_tier_requires_upstream_and_site() {
        // §11.6: a site tier without an upstream is a config error.
        let err = cfg(&[("REEVE_TIER", "site")]).unwrap_err();
        assert!(err.to_string().contains("REEVE_UPSTREAM"));
        // A full gateway config with REEVE_TIER=site parses as Site.
        let c = cfg(&[
            ("REEVE_TIER", "site"),
            ("REEVE_UPSTREAM", "https://hub.example:8420"),
            ("REEVE_UPSTREAM_TOKEN", "rvt_deadbeef"),
            ("REEVE_SITE", "plant-a"),
        ])
        .unwrap();
        assert_eq!(c.tier, ServerTier::Site);
    }

    #[test]
    fn bad_tier_is_an_error() {
        assert!(cfg(&[("REEVE_TIER", "edge")]).is_err());
    }

    #[test]
    fn admin_seed_requires_both_or_neither() {
        assert!(cfg(&[]).unwrap().admin_seed.is_none());
        let c = cfg(&[("REEVE_ADMIN_USER", "admin"), ("REEVE_ADMIN_PASSWORD", "pw")]).unwrap();
        assert_eq!(c.admin_seed, Some(("admin".into(), "pw".into())));
        assert!(cfg(&[("REEVE_ADMIN_USER", "admin")]).is_err());
        assert!(cfg(&[("REEVE_ADMIN_PASSWORD", "pw")]).is_err());
    }

    /// Regression: Docker Compose materializes `${VAR:-}` as `VAR=""`,
    /// so blank optional vars must read as UNSET, not as a
    /// present-but-invalid value. Uses the same empty-filter closure as
    /// `from_env`. Before the fix, blank REEVE_UPSTREAM tripped
    /// "must be an absolute URL" and the server refused to boot.
    #[test]
    fn blank_env_vars_read_as_unset_like_from_env() {
        let map: HashMap<String, String> = [
            ("REEVE_UPSTREAM", ""),
            ("REEVE_UPSTREAM_TOKEN", ""),
            ("REEVE_SITE", ""),
            ("REEVE_ZOT_URL", ""),
            ("REEVE_REGISTRY", ""),
            ("REEVE_DURABILITY_TARGET", ""),
            ("REEVE_PROXY_USER_HEADER", "   "),
            ("REEVE_INSTALL_OPEN", ""),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        // Mirror from_env's filter: empty/whitespace == absent.
        let c = Config::from_lookup(|k| map.get(k).cloned().filter(|v| !v.trim().is_empty()))
            .expect("blank vars must boot as a root/hub, not refuse");
        assert!(c.federation.is_none(), "blank REEVE_UPSTREAM => root");
        assert!(c.zot.is_none(), "blank REEVE_ZOT_URL => no proxy");
        assert!(matches!(c.auth, AuthMode::Password));
        assert!(!c.install_open);
    }

    #[test]
    fn federation_gateway_parses() {
        let c = cfg(&[
            ("REEVE_UPSTREAM", "https://hub.example:8420/"),
            ("REEVE_UPSTREAM_TOKEN", "rvt_deadbeef"),
            ("REEVE_SITE", "plant-a"),
            ("REEVE_SYNC_INTERVAL_SECS", "5"),
        ])
        .unwrap();
        let f = c.federation.expect("gateway tier");
        assert_eq!(f.upstream, "https://hub.example:8420");
        assert_eq!(f.token, "rvt_deadbeef");
        assert_eq!(f.site, "plant-a");
        assert_eq!(f.sync_interval_secs, 5);
    }

    #[test]
    fn federation_misconfigurations_refuse_startup() {
        // Upstream without credential / site (§8.7 / §8.4).
        assert!(cfg(&[("REEVE_UPSTREAM", "http://hub:8420")]).is_err());
        assert!(
            cfg(&[("REEVE_UPSTREAM", "http://hub:8420"), ("REEVE_UPSTREAM_TOKEN", "t")]).is_err()
        );
        // Credential without an upstream.
        assert!(cfg(&[("REEVE_UPSTREAM_TOKEN", "t")]).is_err());
        // Bad site label (spliced into layer paths).
        assert!(
            cfg(&[
                ("REEVE_UPSTREAM", "http://hub:8420"),
                ("REEVE_UPSTREAM_TOKEN", "t"),
                ("REEVE_SITE", "../evil"),
            ])
            .is_err()
        );
        // Not a URL.
        assert!(
            cfg(&[
                ("REEVE_UPSTREAM", "hub"),
                ("REEVE_UPSTREAM_TOKEN", "t"),
                ("REEVE_SITE", "plant-a"),
            ])
            .is_err()
        );
    }

    #[test]
    fn logs_retain_defaults_to_ten() {
        assert_eq!(cfg(&[]).unwrap().logs_retain_per_deployment, 10);
        assert_eq!(
            cfg(&[("REEVE_LOGS_RETAIN_PER_DEPLOYMENT", "3")])
                .unwrap()
                .logs_retain_per_deployment,
            3
        );
        assert!(cfg(&[("REEVE_LOGS_RETAIN_PER_DEPLOYMENT", "0")]).is_err());
        assert!(cfg(&[("REEVE_LOGS_RETAIN_PER_DEPLOYMENT", "nope")]).is_err());
    }

    #[test]
    fn install_open_defaults_closed() {
        assert!(!cfg(&[]).unwrap().install_open);
        assert!(cfg(&[("REEVE_INSTALL_OPEN", "true")]).unwrap().install_open);
        assert!(!cfg(&[("REEVE_INSTALL_OPEN", "false")]).unwrap().install_open);
        assert!(cfg(&[("REEVE_INSTALL_OPEN", "yes")]).is_err());
    }

    #[test]
    fn zot_misconfigurations_refuse_startup() {
        // Half a credential pair.
        assert!(
            cfg(&[("REEVE_ZOT_URL", "http://zot:5000"), ("REEVE_ZOT_USERNAME", "u")]).is_err()
        );
        // Creds without a backend.
        assert!(cfg(&[("REEVE_ZOT_USERNAME", "u"), ("REEVE_ZOT_PASSWORD", "p")]).is_err());
        // Non-http scheme (D8: co-located sidecar, boring client).
        assert!(cfg(&[("REEVE_ZOT_URL", "https://zot:5000")]).is_err());
        // Not a URL at all.
        assert!(cfg(&[("REEVE_ZOT_URL", "zot:5000/nope nope")]).is_err());
    }
}
