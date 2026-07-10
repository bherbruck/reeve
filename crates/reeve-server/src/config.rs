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

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub auth: AuthMode,
    pub session_ttl_secs: i64,
    /// Tier registry endpoint (docs/decisions/delivery.md D8): the value
    /// `${REEVE_REGISTRY}` resolves to at render time. A DECLARED render
    /// input (tree-render.md D3) — it enters render via `RenderContext`,
    /// never via environment reads in the render path.
    pub registry_endpoint: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
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

        Ok(Config {
            listen,
            data_dir,
            auth,
            session_ttl_secs,
            registry_endpoint,
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
}
