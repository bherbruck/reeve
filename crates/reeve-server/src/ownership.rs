//! Layer ownership — the structural single-writer seam.
//!
//! spec/reeve/06-federation.md §8.4: each overlay layer is authored by
//! exactly ONE tier; §8.2 (per-tier revision model, normative): the API
//! MUST refuse writes to layer paths outside the tier's ownership set,
//! and the upstream stream MUST NOT be writable at all. Divergence is
//! impossible by construction, not detected after the fact.
//!
//! v1 is single-tier: the root owns every authorable path, and there is
//! no upstream stream content — but the refusal is still structural
//! ([`Ownership::check_write`] rejects [`Stream::Upstream`]
//! unconditionally, for every tier including the root). Federation (C10)
//! populates [`Ownership::Gateway`] from tier configuration; nothing in
//! the authoring API changes when it does.

use revision_store::Stream;

/// Which tree paths this tier may author (docs/decisions/authoring.md
/// D14; spec/reeve/06-federation.md §8.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Root tier (no `upstream` configured): owns every authorable path
    /// in the local stream. v1 single-tier operation is exactly this.
    Root,
    /// Gateway tier (C10): owns only the listed tree-path prefixes —
    /// its own site layer(s), its locally-enrolled device layers, and
    /// (if granted) local package vendoring.
    ///
    /// Prefix matching: an entry matches a candidate path when they are
    /// equal, when the entry ends with `.` or `/` and the candidate
    /// starts with it (open families like `layers/30-device.`), or when
    /// the candidate continues past the entry at a `/` boundary.
    /// `layers/20-site.plant-a` therefore matches itself and
    /// `layers/20-site.plant-a/...` but NOT `layers/20-site.plant-a2`.
    Gateway { owned_prefixes: Vec<String> },
}

/// Why a write was refused (spec/reeve/06-federation.md §8.2/§8.4).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteRefusal {
    /// The upstream stream is a verbatim read-only copy of the parent
    /// tier's revisions — never writable, at any tier (§8.2).
    #[error("the upstream stream is read-only at every tier (federation §8.2)")]
    UpstreamImmutable,
    /// The path belongs to a layer this tier does not own (§8.4).
    #[error("this tier does not own `{path}` (single writer per layer, federation §8.4)")]
    NotOwned { path: String },
}

impl Ownership {
    /// Gate every authoring write: MUST be called with the stream the
    /// write targets and the tree path (layer dir or package dir) it
    /// touches. The authoring API only ever targets [`Stream::Local`];
    /// the [`Stream::Upstream`] arm exists so the refusal is enforced
    /// here, structurally, rather than by call-site convention.
    pub fn check_write(&self, stream: Stream, tree_path: &str) -> Result<(), WriteRefusal> {
        if stream == Stream::Upstream {
            return Err(WriteRefusal::UpstreamImmutable);
        }
        match self {
            Ownership::Root => Ok(()),
            Ownership::Gateway { owned_prefixes } => {
                if owned_prefixes.iter().any(|p| prefix_matches(p, tree_path)) {
                    Ok(())
                } else {
                    Err(WriteRefusal::NotOwned {
                        path: tree_path.to_string(),
                    })
                }
            }
        }
    }
}

/// See [`Ownership::Gateway`] for the matching rule. Public because the
/// federation scope filter (ext/federation.rs — tier-token
/// `sync_prefixes`, spec/reeve/06-federation.md §8.7) and the core
/// delegated-layer write gate (tree.rs, §8.4) apply the SAME rule: one
/// matcher, no drift.
pub fn prefix_matches(prefix: &str, path: &str) -> bool {
    if path == prefix {
        return true;
    }
    if (prefix.ends_with('.') || prefix.ends_with('/')) && path.starts_with(prefix) {
        return true;
    }
    match path.strip_prefix(prefix) {
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway() -> Ownership {
        Ownership::Gateway {
            owned_prefixes: vec![
                "layers/20-site.plant-a".into(),
                "layers/30-device.".into(),
            ],
        }
    }

    #[test]
    fn upstream_stream_is_never_writable_at_any_tier() {
        // §8.2: not even the root writes the upstream stream.
        assert_eq!(
            Ownership::Root.check_write(Stream::Upstream, "layers/00-fleet"),
            Err(WriteRefusal::UpstreamImmutable)
        );
        assert_eq!(
            gateway().check_write(Stream::Upstream, "layers/20-site.plant-a"),
            Err(WriteRefusal::UpstreamImmutable)
        );
    }

    #[test]
    fn root_owns_every_local_path() {
        for path in [
            "layers/00-fleet",
            "layers/30-device.dev-0011223344556677",
            "packages/nginx/1.0.0",
        ] {
            assert_eq!(Ownership::Root.check_write(Stream::Local, path), Ok(()));
        }
    }

    #[test]
    fn gateway_owns_only_its_prefixes() {
        let g = gateway();
        assert_eq!(g.check_write(Stream::Local, "layers/20-site.plant-a"), Ok(()));
        assert_eq!(
            g.check_write(Stream::Local, "layers/30-device.dev-0011223344556677"),
            Ok(())
        );
        // hub-owned layers refused (§8.4: the root authors fleet/region)
        assert!(matches!(
            g.check_write(Stream::Local, "layers/00-fleet"),
            Err(WriteRefusal::NotOwned { .. })
        ));
        assert!(matches!(
            g.check_write(Stream::Local, "packages/nginx/1.0.0"),
            Err(WriteRefusal::NotOwned { .. })
        ));
        // sibling-label near-miss must NOT match
        assert!(matches!(
            g.check_write(Stream::Local, "layers/20-site.plant-a2"),
            Err(WriteRefusal::NotOwned { .. })
        ));
    }
}
