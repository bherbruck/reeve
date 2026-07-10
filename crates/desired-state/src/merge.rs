//! Overlay merge semantics (docs/decisions/tree-render.md D3 — these
//! rules ARE this crate's table-test spec):
//!
//! - Maps: deep merge, key by key.
//! - Lists: REPLACE, always. Never append, never merge-by-index.
//! - Explicit `null` at a key = deletion of that key from the merged
//!   result. Absence = inherit. No other tombstone mechanism.
//! - Scalars: override.

use serde_yaml_ng::Value;

/// Merge `overlay` into `base` per D3. Later (deeper) layers call this
/// with their content as `overlay`, so "later layer wins".
pub fn merge_value(base: &mut Value, overlay: Value) {
    match overlay {
        Value::Mapping(overlay_map) => {
            if let Value::Mapping(base_map) = base {
                for (key, value) in overlay_map {
                    if value.is_null() {
                        // Explicit null deletes the key (D3).
                        base_map.remove(&key);
                    } else if let Some(existing) = base_map.get_mut(&key) {
                        merge_value(existing, value);
                    } else {
                        base_map.insert(key, value);
                    }
                }
            } else {
                *base = Value::Mapping(overlay_map);
            }
        }
        // Lists replace; scalars override (D3). A whole-document null
        // never reaches here (callers skip empty/null layer files).
        other => *base = other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(yaml: &str) -> Value {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn scalars_override() {
        let mut base = v("a: 1");
        merge_value(&mut base, v("a: 2"));
        assert_eq!(base, v("a: 2"));
    }

    #[test]
    fn maps_deep_merge() {
        let mut base = v("m:\n  a: 1\n  b: 2");
        merge_value(&mut base, v("m:\n  b: 3\n  c: 4"));
        assert_eq!(base, v("m:\n  a: 1\n  b: 3\n  c: 4"));
    }

    #[test]
    fn lists_replace() {
        let mut base = v("l: [1, 2, 3]");
        merge_value(&mut base, v("l: [9]"));
        assert_eq!(base, v("l: [9]"));
    }

    #[test]
    fn null_deletes() {
        let mut base = v("a: 1\nb: 2");
        merge_value(&mut base, v("a: null"));
        assert_eq!(base, v("b: 2"));
    }

    #[test]
    fn null_on_absent_key_is_noop() {
        let mut base = v("b: 2");
        merge_value(&mut base, v("a: null"));
        assert_eq!(base, v("b: 2"));
    }

    #[test]
    fn map_replaces_scalar_and_vice_versa() {
        let mut base = v("a: 1");
        merge_value(&mut base, v("a:\n  x: 1"));
        assert_eq!(base, v("a:\n  x: 1"));
        merge_value(&mut base, v("a: done"));
        assert_eq!(base, v("a: done"));
    }
}
