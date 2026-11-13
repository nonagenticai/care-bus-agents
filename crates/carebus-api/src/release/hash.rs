//! Canonical JSON and the release hashes.
//!
//! `canonical_json` is RFC 8785 (JCS) restricted to the release bundle schema: object keys sorted by UTF-16 code
//! units, no whitespace, strings escaped exactly as `serde_json` escapes them, integers only. It never relies on
//! `serde_json::Value`'s map order: feature unification turns on serde_json's `preserve_order` in this workspace,
//! so `serde_json::to_vec(&value)` would keep insertion order and two equal bundles could hash differently.

use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum HashError {
    /// A float (or an integer outside i64/u64) anywhere in the document; the bundle schema has integers only.
    #[error("{path}: numbers must be integers")]
    NonInteger { path: String },
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl HashError {
    /// JSON pointer of the offending value, relative to the hashed document (`""` for a serialisation error).
    pub fn path(&self) -> &str {
        match self {
            HashError::NonInteger { path } => path.as_str(),
            HashError::Json(_) => "",
        }
    }
}

/// Sorted keys, compact, rejects non-integer numbers.
pub fn canonical_json(v: &Value) -> Result<Vec<u8>, HashError> {
    let mut out = Vec::new();
    let mut path = String::new();
    write_canonical(v, &mut out, &mut path)?;
    Ok(out)
}

/// `"sha256:" + lowercase hex(SHA-256(canonical_json(bundle)))`.
pub fn bundle_hash(bundle: &Value) -> Result<String, HashError> {
    let bytes = canonical_json(bundle)?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

/// A row's `content_hash`: the hash of its bundle item with the nested child arrays named in `strip` removed
/// (`attributes` for an entity, `field_mappings` for a connector). A procedure keeps its `steps`.
pub fn row_hash(item: &Value, strip: &[&str]) -> Result<String, HashError> {
    match item {
        Value::Object(m) => {
            let kept: serde_json::Map<String, Value> = m
                .iter()
                .filter(|(k, _)| !strip.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            bundle_hash(&Value::Object(kept))
        }
        other => bundle_hash(other),
    }
}

/// One JSON-pointer reference token (RFC 6901): `~` → `~0`, `/` → `~1`.
pub(crate) fn pointer_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn write_canonical(v: &Value, out: &mut Vec<u8>, path: &mut String) -> Result<(), HashError> {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.extend_from_slice(i.to_string().as_bytes());
            } else if let Some(u) = n.as_u64() {
                out.extend_from_slice(u.to_string().as_bytes());
            } else {
                return Err(HashError::NonInteger { path: path.clone() });
            }
        }
        Value::String(s) => out.extend_from_slice(&serde_json::to_vec(s)?),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                let len = path.len();
                path.push('/');
                path.push_str(&i.to_string());
                write_canonical(item, out, path)?;
                path.truncate(len);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push(b'{');
            for (i, (k, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&serde_json::to_vec(k)?);
                out.push(b':');
                let len = path.len();
                path.push('/');
                path.push_str(&pointer_token(k));
                write_canonical(item, out, path)?;
                path.truncate(len);
            }
            out.push(b'}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_keys_and_strips_whitespace() {
        let v: Value =
            serde_json::from_str(r#"{ "b": [1, {"d": "x", "c": true}], "a": null }"#).unwrap();
        assert_eq!(
            canonical_json(&v).unwrap(),
            br#"{"a":null,"b":[1,{"c":true,"d":"x"}]}"#.to_vec()
        );
    }

    #[test]
    fn canonical_json_ignores_insertion_order() {
        let a = json!({"x": 1, "y": 2});
        let mut m = serde_json::Map::new();
        m.insert("y".into(), json!(2));
        m.insert("x".into(), json!(1));
        assert_eq!(
            bundle_hash(&a).unwrap(),
            bundle_hash(&Value::Object(m)).unwrap()
        );
    }

    #[test]
    fn floats_are_rejected() {
        let err = canonical_json(&json!({"t": [0, 1.5]})).unwrap_err();
        assert_eq!(err.path(), "/t/1");
    }

    #[test]
    fn strings_escape_as_serde_json_does() {
        let v = json!({"s": "a\"b\\c\n\u{1}/é", "n": -3});
        assert_eq!(
            canonical_json(&v).unwrap(),
            "{\"n\":-3,\"s\":\"a\\\"b\\\\c\\n\\u0001/é\"}"
                .as_bytes()
                .to_vec()
        );
    }

    #[test]
    fn row_hash_strips_named_children_only() {
        let item = json!({"name": "Order", "attributes": [{"name": "order_id"}]});
        assert_eq!(
            row_hash(&item, &["attributes"]).unwrap(),
            bundle_hash(&json!({"name": "Order"})).unwrap()
        );
        assert_eq!(row_hash(&item, &[]).unwrap(), bundle_hash(&item).unwrap());
    }

    #[test]
    fn hash_format_is_prefixed_lowercase_hex() {
        let h = bundle_hash(&json!({})).unwrap();
        // SHA-256 of "{}".
        assert_eq!(
            h,
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }
}
