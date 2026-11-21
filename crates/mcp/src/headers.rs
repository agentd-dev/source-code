//! The SEP-2243 request headers agentd builds itself, for the requests it
//! sends outside the SDK's typed helpers (the MCP Events methods).
//!
//! The SDK builds these for `tools/call`, `prompts/get`, `resources/*` and
//! `tasks/*`, but keeps its builder private; this is the same rule set, so a
//! value reaches a server encoded byte for byte as the SDK would encode it
//! (rmcp `transport/common/mcp_headers.rs`): a value that is not plain
//! printable ASCII, or that has leading or trailing whitespace, travels as
//! `=?base64?<standard base64>?=`.

use serde_json::Value;

/// `Mcp-Name`: the request's target, mirrored from its params.
pub const NAME: &str = "Mcp-Name";
/// `Mcp-Param-{Name}`: one annotated argument.
pub const PARAM_PREFIX: &str = "Mcp-Param-";
const B64_PREFIX: &str = "=?base64?";
const B64_SUFFIX: &str = "?=";

/// A header value as SEP-2243 encodes it.
pub fn encode_value(value: &str) -> String {
    if needs_base64(value) {
        format!("{B64_PREFIX}{}{B64_SUFFIX}", base64(value.as_bytes()))
    } else {
        value.to_string()
    }
}

fn needs_base64(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let b = value.as_bytes();
    matches!(b.first(), Some(b' ' | b'\t'))
        || matches!(b.last(), Some(b' ' | b'\t'))
        || value
            .chars()
            .any(|c| (c as u32) < 0x20 || (c as u32) > 0x7E)
        || (value.starts_with(B64_PREFIX) && value.ends_with(B64_SUFFIX))
}

/// Standard base64 (RFC 4648 §4), padded.
fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '!' | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '.'
                | '^'
                | '_'
                | '`'
                | '|'
                | '~'
        )
}

/// Every `x-mcp-header` annotation on a `properties` chain from the root:
/// `(path, its schema, the annotation)`.
fn annotations(schema: &Value) -> Vec<(Vec<&str>, &Value, &Value)> {
    fn walk<'a>(
        props: Option<&'a Value>,
        path: &mut Vec<&'a str>,
        out: &mut Vec<(Vec<&'a str>, &'a Value, &'a Value)>,
    ) {
        let Some(Value::Object(props)) = props else {
            return;
        };
        for (name, s) in props {
            path.push(name);
            if let Some(h) = s.get("x-mcp-header") {
                out.push((path.clone(), s, h));
            }
            walk(s.get("properties"), path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(schema.get("properties"), &mut Vec::new(), &mut out);
    out
}

/// Whether an `x-mcp-header` sits anywhere but on a `properties` chain.
fn off_chain(schema: &Value, on_chain: bool) -> bool {
    let sub = |v: &Value, on: bool| match v {
        Value::Object(_) => off_chain(v, on),
        Value::Array(items) => items.iter().any(|i| off_chain(i, false)),
        _ => false,
    };
    let map = |v: &Value, on: bool| {
        v.as_object()
            .is_some_and(|m| m.values().any(|s| sub(s, on)))
    };
    let Some(o) = schema.as_object() else {
        return false;
    };
    o.iter().any(|(k, v)| match k.as_str() {
        "x-mcp-header" => !on_chain,
        "properties" => map(v, on_chain),
        "$defs" | "definitions" | "dependencies" | "dependentSchemas" | "patternProperties" => {
            map(v, false)
        }
        "default" | "const" | "enum" | "examples" | "dependentRequired" => false,
        _ => sub(v, false),
    })
}

/// Whether a schema's `x-mcp-header` annotations are valid (SEP-2243): each
/// on a property reached by `properties` only, a non-empty HTTP token, unique
/// ignoring case, on a string, integer or boolean. `Err` says why not.
pub fn validate_annotations(schema: &Value) -> Result<(), String> {
    if schema.get("x-mcp-header").is_some() {
        return Err("x-mcp-header must annotate a property, not the schema root".into());
    }
    if off_chain(schema, true) {
        return Err("x-mcp-header must sit on a `properties`-only path from the root".into());
    }
    let mut seen = std::collections::HashSet::new();
    for (path, s, h) in annotations(schema) {
        let prop = path.join(".");
        let Value::String(h) = h else {
            return Err(format!("property `{prop}`: x-mcp-header must be a string"));
        };
        if h.is_empty() {
            return Err(format!("property `{prop}`: x-mcp-header must not be empty"));
        }
        if !h.chars().all(is_tchar) {
            return Err(format!(
                "property `{prop}`: x-mcp-header `{h}` is not a valid HTTP token"
            ));
        }
        if !seen.insert(h.to_ascii_lowercase()) {
            return Err(format!(
                "property `{prop}`: duplicate x-mcp-header `{h}` (case-insensitive)"
            ));
        }
        if !matches!(
            s.get("type").and_then(Value::as_str),
            Some("string" | "integer" | "boolean")
        ) {
            return Err(format!(
                "property `{prop}`: x-mcp-header requires a primitive type (string/integer/boolean)"
            ));
        }
    }
    Ok(())
}

/// The `Mcp-Param-{Name}` headers for `arguments` under `schema`: one per
/// annotated argument present with a primitive value, encoded.
pub fn param_headers(schema: &Value, arguments: &Value) -> Vec<(String, String)> {
    annotations(schema)
        .into_iter()
        .filter_map(|(path, _, h)| {
            let h = h.as_str().filter(|h| !h.is_empty())?;
            let v = path.iter().try_fold(arguments, |v, k| v.get(*k))?;
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            Some((format!("{PARAM_PREFIX}{h}"), encode_value(&s)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_encode_as_the_sdk_encodes_them() {
        assert_eq!(encode_value("email.received"), "email.received");
        assert_eq!(encode_value(""), "");
        // Not printable ASCII, edge whitespace, or a lookalike sentinel.
        assert_eq!(encode_value("café"), "=?base64?Y2Fmw6k=?=");
        assert_eq!(encode_value(" x"), "=?base64?IHg=?=");
        assert_eq!(encode_value("=?base64?x?="), "=?base64?PT9iYXNlNjQ/eD89?=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }

    #[test]
    fn annotations_are_checked_like_a_tools() {
        let ok = json!({"type": "object", "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "outer": {"type": "object", "properties": {
                "n": {"type": "integer", "x-mcp-header": "N"}}}}});
        assert_eq!(validate_annotations(&ok), Ok(()));
        for bad in [
            json!({"x-mcp-header": "Root"}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "Bad Name"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": ""}}}),
            json!({"properties": {"a": {"type": "object", "x-mcp-header": "A"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "A"},
                                  "b": {"type": "string", "x-mcp-header": "a"}}}),
            json!({"properties": {"a": {"type": "array", "items":
                {"type": "string", "x-mcp-header": "I"}}}}),
        ] {
            assert!(validate_annotations(&bad).is_err(), "{bad}");
        }
        assert_eq!(
            param_headers(&ok, &json!({"region": "eu", "outer": {"n": 3}, "x": 1})),
            vec![
                ("Mcp-Param-N".to_string(), "3".to_string()),
                ("Mcp-Param-Region".to_string(), "eu".to_string())
            ]
        );
        assert!(param_headers(&ok, &json!({})).is_empty());
    }
}
