// SPDX-License-Identifier: Apache-2.0
//! The **modern** (stateless, `2026-07-28`+) protocol dialect: how a request is
//! constructed when there is no `initialize` handshake and no session.
//! modelcontextprotocol.io/specification/draft/basic/transports/streamable-http.
//!
//! Every request carries its protocol version, client identity, and client
//! capabilities in `params._meta.io.modelcontextprotocol/*`; the Streamable HTTP
//! binding mirrors selected fields into headers so intermediaries can route
//! without parsing the body:
//!
//! * `MCP-Protocol-Version: <version>` — MUST, and MUST match the `_meta` version.
//! * `Mcp-Method: <method>` — MUST, on every request.
//! * `Mcp-Name: <params.name | params.uri>` — MUST, for `tools/call`,
//!   `resources/read`, `prompts/get`. Value-encoded (`=?base64?…?=`) when not
//!   header-safe.
//!
//! Header/body mismatch or a missing required header is a `-32020`
//! ([`crate::version::HEADER_MISMATCH_CODE`]) `400`.

use crate::version::META_NS;
use crate::wire::Implementation;
use serde_json::{Value, json};

/// Add the per-request `io.modelcontextprotocol/*` metadata to a request's params
/// (modern era): protocol version, client identity, client capabilities. Creates
/// `params._meta` if absent; a non-object `params` is left untouched (all MCP
/// method params are objects, so this is a no-op guard).
pub fn inject_client_meta(params: &mut Value, protocol_version: &str, client: &Implementation) {
    let Some(obj) = params.as_object_mut() else {
        return;
    };
    let meta = obj
        .entry("_meta")
        .or_insert_with(|| Value::Object(Default::default()));
    let Some(m) = meta.as_object_mut() else {
        return;
    };
    m.insert(
        format!("{META_NS}protocolVersion"),
        Value::String(protocol_version.to_string()),
    );
    m.insert(
        format!("{META_NS}clientInfo"),
        json!({"name": client.name, "version": client.version}),
    );
    m.insert(
        format!("{META_NS}clientCapabilities"),
        Value::Object(Default::default()),
    );
}

/// The `Mcp-Name` header source for a method: `params.name` (`tools/call`,
/// `prompts/get`) or `params.uri` (`resources/read`). `None` for methods that
/// carry no name/uri (no `Mcp-Name` header is sent for those).
pub fn mcp_name(method: &str, params: &Value) -> Option<String> {
    match method {
        "tools/call" | "prompts/get" => params.get("name").and_then(Value::as_str).map(String::from),
        "resources/read" => params.get("uri").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

/// The modern-era Streamable HTTP routing headers for a request: `Mcp-Method`
/// always, and `Mcp-Name` (value-encoded) for name/uri-bearing methods. The
/// caller adds `MCP-Protocol-Version` (shared with the legacy path) separately.
pub fn routing_headers(method: &str, params: &Value) -> Vec<(&'static str, String)> {
    let mut headers = vec![("Mcp-Method", method.to_string())];
    if let Some(name) = mcp_name(method, params) {
        headers.push(("Mcp-Name", header_value(&name)));
    }
    headers
}

/// Encode a value for an `Mcp-Name` / `Mcp-Param-*` HTTP header (transports
/// §value-encoding). A plain, header-safe value passes through; anything else —
/// non-visible-ASCII, whitespace, or a string that itself looks like the base64
/// sentinel — is carried as `=?base64?<standard-base64>?=` so it survives on the
/// wire unambiguously and cannot be used for header injection.
pub fn header_value(raw: &str) -> String {
    if is_header_safe(raw) {
        raw.to_string()
    } else {
        format!("=?base64?{}?=", base64_encode(raw.as_bytes()))
    }
}

/// A value is header-safe when it is non-empty, made only of visible ASCII
/// (`0x21..=0x7E` — no spaces or control chars), and does not itself match the
/// `=?base64?…?=` sentinel (which must be encoded to avoid ambiguity).
fn is_header_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
        && !(s.starts_with("=?base64?") && s.ends_with("?="))
}

/// Extract the `Mcp-Param-*` headers for a `tools/call` from a tool's
/// `input_schema` + the call `arguments` (transports §custom-headers-from-tool-
/// parameters). Walks the schema's `properties` chains (the only statically-
/// reachable path); for each property annotated with `x-mcp-header`, reads the
/// value at that path from `arguments` (omitting when absent), stringifies the
/// primitive, and value-encodes it. Assumes the schema passed
/// [`validate_x_mcp_headers`] (a client rejects tools that don't).
pub fn param_headers(input_schema: &Value, arguments: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    collect_param_headers(input_schema, arguments, &mut out);
    out
}

fn collect_param_headers(schema: &Value, instance: &Value, out: &mut Vec<(String, String)>) {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (key, sub) in props {
        let value = instance.get(key);
        if let Some(header_name) = sub.get("x-mcp-header").and_then(Value::as_str)
            && let Some(v) = value
            && let Some(s) = primitive_to_string(v)
        {
            out.push((format!("Mcp-Param-{header_name}"), header_value(&s)));
        }
        // Recurse into nested object properties (still statically reachable).
        if let Some(v) = value
            && sub.get("properties").is_some()
        {
            collect_param_headers(sub, v, out);
        }
    }
}

/// Stringify a primitive JSON value for a header (transports §value-encoding):
/// string as-is, integer as decimal, boolean lowercase. A `number` (float),
/// null, array, or object yields `None` — `number` is not header-permitted.
fn primitive_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "true".into() } else { "false".into() }),
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        _ => None,
    }
}

/// Validate every `x-mcp-header` annotation in a tool `input_schema` (transports
/// §schema-extension). `Err(reason)` if any is invalid — the client then excludes
/// the tool from `tools/list`. Enforces: non-empty; HTTP token syntax (no CR/LF/
/// controls); case-insensitively unique; a primitive type (string/integer/boolean,
/// NOT number); and statically reachable (a chain of `properties` keys only — an
/// annotation under items/composition/conditional/`$ref` is invalid).
pub fn validate_x_mcp_headers(input_schema: &Value) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    validate_schema_node(input_schema, true, &mut seen)
}

fn validate_schema_node(
    node: &Value,
    reachable: bool,
    seen: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    let Some(obj) = node.as_object() else {
        return Ok(());
    };
    if let Some(h) = obj.get("x-mcp-header") {
        let name = h.as_str().ok_or("x-mcp-header must be a string")?;
        if !reachable {
            return Err(format!("x-mcp-header '{name}' is not statically reachable"));
        }
        validate_header_name(name)?;
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(format!("duplicate x-mcp-header '{name}'"));
        }
        match obj.get("type").and_then(Value::as_str) {
            Some("string") | Some("integer") | Some("boolean") => {}
            Some("number") => return Err(format!("x-mcp-header '{name}' on a number type")),
            _ => return Err(format!("x-mcp-header '{name}' on a non-primitive type")),
        }
    }
    // `properties` children stay reachable; every other composite/conditional
    // keyword breaks the static-reachability chain.
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for sub in props.values() {
            validate_schema_node(sub, reachable, seen)?;
        }
    }
    for key in ["items", "additionalProperties", "not", "if", "then", "else"] {
        if let Some(sub) = obj.get(key) {
            validate_schema_node(sub, false, seen)?;
        }
    }
    for key in ["oneOf", "anyOf", "allOf", "prefixItems"] {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            for sub in arr {
                validate_schema_node(sub, false, seen)?;
            }
        }
    }
    Ok(())
}

/// An HTTP field-name token (RFC 9110 §5.1 `1*tchar`) — non-empty, no CR/LF or
/// controls. `x-mcp-header` values must satisfy this to form `Mcp-Param-{name}`.
fn validate_header_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty x-mcp-header".into());
    }
    let is_tchar =
        |c: u8| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c);
    if !name.bytes().all(is_tchar) {
        return Err(format!("x-mcp-header '{name}' is not a valid HTTP token"));
    }
    Ok(())
}

/// Standard Base64 (RFC 4648, with `=` padding). Hand-rolled — no base64 crate
/// (the minimalism moat); only used for the header sentinel above.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Implementation {
        Implementation {
            name: "agentd".into(),
            version: "1.0.0".into(),
            title: None,
        }
    }

    #[test]
    fn injects_the_three_meta_fields() {
        let mut params = json!({"name": "echo", "arguments": {"x": 1}});
        inject_client_meta(&mut params, "2026-07-28", &client());
        let meta = &params["_meta"];
        assert_eq!(meta["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
        assert_eq!(meta["io.modelcontextprotocol/clientInfo"]["name"], "agentd");
        assert_eq!(
            meta["io.modelcontextprotocol/clientInfo"]["version"],
            "1.0.0"
        );
        assert!(meta["io.modelcontextprotocol/clientCapabilities"].is_object());
        // The original params are preserved.
        assert_eq!(params["name"], "echo");
        assert_eq!(params["arguments"]["x"], 1);
    }

    #[test]
    fn routing_headers_carry_method_and_name() {
        let p = json!({"name": "get_weather", "arguments": {}});
        let h = routing_headers("tools/call", &p);
        assert_eq!(h[0], ("Mcp-Method", "tools/call".to_string()));
        assert_eq!(h[1], ("Mcp-Name", "get_weather".to_string()));

        // resources/read uses the uri as the name.
        let p = json!({"uri": "file:///a.json"});
        let h = routing_headers("resources/read", &p);
        assert_eq!(h[1], ("Mcp-Name", "file:///a.json".to_string()));

        // A method with no name gets only Mcp-Method.
        let h = routing_headers("tools/list", &json!({}));
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "Mcp-Method");
    }

    #[test]
    fn header_value_encodes_only_when_unsafe() {
        assert_eq!(header_value("get_weather"), "get_weather");
        assert_eq!(header_value("file:///a.json"), "file:///a.json");
        // Non-ASCII → base64 sentinel.
        assert_eq!(
            header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        // A space forces encoding.
        assert_eq!(header_value("a b"), format!("=?base64?{}?=", base64_encode(b"a b")));
        // A value that looks like the sentinel is itself encoded.
        assert!(header_value("=?base64?x?=").starts_with("=?base64?"));
    }

    #[test]
    fn param_headers_extracts_annotated_values() {
        let schema = json!({
            "type": "object",
            "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"},
                "limit": {"type": "integer", "x-mcp-header": "Limit"},
                "query": {"type": "string"}
            }
        });
        let args = json!({"region": "us-west1", "limit": 42, "query": "SELECT 1"});
        let mut h = param_headers(&schema, &args);
        h.sort();
        assert_eq!(
            h,
            vec![
                ("Mcp-Param-Limit".to_string(), "42".to_string()),
                ("Mcp-Param-Region".to_string(), "us-west1".to_string()),
            ]
        );
        // A missing annotated value omits its header.
        let h = param_headers(&schema, &json!({"query": "x"}));
        assert!(h.is_empty());
    }

    #[test]
    fn validate_accepts_valid_and_rejects_invalid() {
        // Valid: primitive, reachable, unique.
        assert!(validate_x_mcp_headers(&json!({
            "type": "object",
            "properties": {"r": {"type": "string", "x-mcp-header": "Region"}}
        }))
        .is_ok());
        // number type is not permitted.
        assert!(validate_x_mcp_headers(&json!({
            "properties": {"n": {"type": "number", "x-mcp-header": "N"}}
        }))
        .is_err());
        // Duplicate (case-insensitive) names.
        assert!(validate_x_mcp_headers(&json!({
            "properties": {
                "a": {"type": "string", "x-mcp-header": "Dup"},
                "b": {"type": "string", "x-mcp-header": "dup"}
            }
        }))
        .is_err());
        // Not statically reachable (under `items`).
        assert!(validate_x_mcp_headers(&json!({
            "properties": {"list": {"type": "array",
                "items": {"type": "object", "properties": {
                    "x": {"type": "string", "x-mcp-header": "X"}}}}}
        }))
        .is_err());
        // Invalid HTTP token character.
        assert!(validate_x_mcp_headers(&json!({
            "properties": {"a": {"type": "string", "x-mcp-header": "bad name"}}
        }))
        .is_err());
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
