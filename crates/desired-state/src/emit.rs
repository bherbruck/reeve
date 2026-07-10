//! Canonical YAML emitter (docs/decisions/tree-render.md D3,
//! normative MUST): keys sorted lexically, block style, LF endings,
//! trailing newline, no anchors/aliases. Same inputs => byte-identical
//! output => same bundle digest.
//!
//! serde_yaml_ng already emits block style with LF endings, a trailing
//! newline, and never emits anchors/aliases; the one property it does
//! not guarantee is key order, so every value is recursively key-sorted
//! before serialization.

use serde::Serialize;
use serde_yaml_ng::Value;

use crate::error::RenderError;

/// Serialize any value to canonical YAML bytes (via
/// [`canonicalize`]).
pub fn to_canonical_yaml<T: Serialize>(value: &T, context: &str) -> Result<Vec<u8>, RenderError> {
    let value = serde_yaml_ng::to_value(value).map_err(|source| RenderError::Yaml {
        path: context.to_string(),
        source,
    })?;
    let text =
        serde_yaml_ng::to_string(&canonicalize(value)).map_err(|source| RenderError::Yaml {
            path: context.to_string(),
            source,
        })?;
    Ok(text.into_bytes())
}

/// Recursively sort every mapping's keys lexically. Non-string keys
/// (legal YAML) sort by their own serialized form, after all string
/// keys — deterministic even for exotic documents.
pub fn canonicalize(value: Value) -> Value {
    match value {
        Value::Mapping(map) => {
            let mut entries: Vec<(Value, Value)> = map
                .into_iter()
                .map(|(k, v)| (k, canonicalize(v)))
                .collect();
            entries.sort_by_key(|(key, _)| sort_key(key));
            Value::Mapping(entries.into_iter().collect())
        }
        Value::Sequence(seq) => Value::Sequence(seq.into_iter().map(canonicalize).collect()),
        Value::Tagged(tagged) => {
            let mut tagged = tagged;
            tagged.value = canonicalize(tagged.value);
            Value::Tagged(tagged)
        }
        scalar => scalar,
    }
}

/// Sort key: string keys first (by value), then everything else by
/// serialized form.
fn sort_key(key: &Value) -> (u8, String) {
    match key {
        Value::String(s) => (0, s.clone()),
        other => (
            1,
            serde_yaml_ng::to_string(other)
                .unwrap_or_default()
                .trim_end()
                .to_string(),
        ),
    }
}
