// SPDX-License-Identifier: AGPL-3.0-only
//! The **mapping language** the store adapters and the tool overrides share
//! (RFC 0025 §4, RFC 0028 §4): render an argument object / URL / body from a
//! template over named inputs, and extract a value from a result.
//!
//! - **JSON templates** — a JSON text with `{name}` placeholders: a placeholder
//!   inside quotes (`"{key}"`) is substituted as JSON-escaped string content; a
//!   bare placeholder (`"seq": {seq}`, `"state": {envelope}`) is substituted as
//!   the JSON serialization of the value; the result must parse as JSON. A
//!   template that is exactly one bare placeholder yields the value itself.
//! - **Text templates** — `{name}` substituted textually (URLs, header values).
//! - **`CEL:` expressions** — evaluated over the same inputs (feature `cel`;
//!   an error without the feature).
//! - **Extraction** — a JSON pointer (`/result/structuredContent/state`), a
//!   dotted path (`result.structuredContent.state`, numeric segments index
//!   arrays), or `CEL:` over the context.
//!
//! No dependency: placeholders are `{ident}` or `{{ident}}` where ident is
//! `[A-Za-z_][A-Za-z0-9_.]*` (dotted paths reach into object inputs) — JSON
//! braces are followed by `"` / `}` / whitespace, never an identifier, so the
//! two never collide.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// A rendering/extraction failure (a template that does not parse, an unknown
/// placeholder, a CEL error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingError(pub String);

impl std::fmt::Display for MappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MappingError {}

/// The named inputs a template renders over.
pub type Vars = BTreeMap<String, Value>;

/// Render a JSON template (or a `CEL:` expression) into a value.
pub fn render_json(template: &str, vars: &Vars) -> Result<Value, MappingError> {
    let t = template.trim();
    if let Some(expr) = t.strip_prefix("CEL:") {
        return crate::cel::eval_value(expr.trim(), &crate::cel::vars_of(vars))
            .map_err(|e| MappingError(format!("CEL: {e}")));
    }
    // A lone placeholder yields the value itself (any type).
    if let Some(name) = lone_placeholder(t) {
        return vars
            .get(name)
            .cloned()
            .ok_or_else(|| MappingError(format!("unknown placeholder {{{name}}}")));
    }
    let text = substitute(t, vars, Mode::Json)?;
    serde_json::from_str(&text).map_err(|e| {
        MappingError(format!(
            "template does not render to valid JSON: {e} (rendered: {})",
            text.chars().take(200).collect::<String>()
        ))
    })
}

/// Render a text template (`{name}` substituted textually) or a `CEL:`
/// expression (its result stringified).
pub fn render_text(template: &str, vars: &Vars) -> Result<String, MappingError> {
    let t = template.trim();
    if let Some(expr) = t.strip_prefix("CEL:") {
        let v = crate::cel::eval_value(expr.trim(), &crate::cel::vars_of(vars))
            .map_err(|e| MappingError(format!("CEL: {e}")))?;
        return Ok(match v {
            Value::String(s) => s,
            other => other.to_string(),
        });
    }
    substitute(t, vars, Mode::Text)
}

/// Extract a value from `ctx` by JSON pointer, dotted path, or `CEL:`.
/// `None` when the path does not resolve (a CEL error is `Err`).
pub fn extract(expr: &str, ctx: &Value) -> Result<Option<Value>, MappingError> {
    let e = expr.trim();
    if let Some(cel) = e.strip_prefix("CEL:") {
        let vars: Vars = match ctx {
            Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            other => {
                let mut m = Vars::new();
                m.insert("value".into(), other.clone());
                m
            }
        };
        return crate::cel::eval_value(cel.trim(), &crate::cel::vars_of(&vars))
            .map(Some)
            .map_err(|e| MappingError(format!("CEL: {e}")));
    }
    if e.is_empty() {
        return Ok(Some(ctx.clone()));
    }
    if e.starts_with('/') {
        return Ok(ctx.pointer(e).cloned());
    }
    // Dotted path with numeric array indexes.
    let mut cur = ctx;
    for seg in e.split('.') {
        cur = match cur {
            Value::Object(m) => match m.get(seg) {
                Some(v) => v,
                None => return Ok(None),
            },
            Value::Array(a) => match seg.parse::<usize>().ok().and_then(|i| a.get(i)) {
                Some(v) => v,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(cur.clone()))
}

/// Truthiness for `ok`/`conflict` predicates: `true`, a non-zero number, a
/// non-empty string/array/object.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Build the standard `Vars` for a store operation.
pub fn store_vars(
    key: &str,
    seq: Option<u64>,
    prefix: &str,
    instance: &str,
    envelope: Option<&Value>,
    kind: &str,
    id: &str,
) -> Vars {
    let mut v = Vars::new();
    v.insert("key".into(), Value::String(key.to_string()));
    v.insert("seq".into(), seq.map(Value::from).unwrap_or(Value::Null));
    v.insert("prefix".into(), Value::String(prefix.to_string()));
    v.insert("instance".into(), Value::String(instance.to_string()));
    v.insert("envelope".into(), envelope.cloned().unwrap_or(Value::Null));
    v.insert("kind".into(), Value::String(kind.to_string()));
    v.insert("id".into(), Value::String(id.to_string()));
    v
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Json,
    Text,
}

fn lone_placeholder(t: &str) -> Option<&str> {
    let inner = t.strip_prefix('{')?.strip_suffix('}')?;
    let inner = match (inner.strip_prefix('{'), inner.strip_suffix('}')) {
        (Some(a), Some(_)) => &a[..a.len() - 1],
        _ => inner,
    };
    is_ident(inner).then_some(inner)
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Look up a (possibly dotted) placeholder in the vars.
fn lookup<'a>(vars: &'a Vars, name: &str) -> Option<&'a Value> {
    if let Some(v) = vars.get(name) {
        return Some(v);
    }
    let (head, rest) = name.split_once('.')?;
    let mut cur = vars.get(head)?;
    for seg in rest.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn substitute(t: &str, vars: &Vars, mode: Mode) -> Result<String, MappingError> {
    let bytes = t.as_bytes();
    let mut out = String::with_capacity(t.len() + 32);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // `{name}` or `{{name}}` (both accepted; the double form is the
            // RFC 0028 override spelling). Find the identifier + closing braces.
            let open = if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                2
            } else {
                1
            };
            let start = i + open;
            let mut j = start;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'.')
            {
                j += 1;
            }
            let closes = j + open <= bytes.len() && bytes[j..j + open].iter().all(|b| *b == b'}');
            if j > start && closes && is_ident(&t[start..j]) {
                let name = &t[start..j];
                let end = j + open; // index just past the closing braces
                let value = lookup(vars, name)
                    .ok_or_else(|| MappingError(format!("unknown placeholder {{{name}}}")))?;
                match mode {
                    Mode::Text => out.push_str(&match value {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    }),
                    Mode::Json => {
                        let quoted = i > 0
                            && bytes[i - 1] == b'"'
                            && end < bytes.len()
                            && bytes[end] == b'"';
                        match value {
                            Value::String(s) if quoted => {
                                // Inside quotes: escaped string CONTENT (the
                                // template supplies the quotes).
                                let js = serde_json::to_string(s).unwrap_or_default();
                                out.push_str(&js[1..js.len() - 1]);
                            }
                            other => out.push_str(&other.to_string()),
                        }
                    }
                }
                i = end;
                continue;
            }
        }
        // Copy one UTF-8 char verbatim.
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&t[i..(i + ch_len).min(bytes.len())]);
        i += ch_len;
    }
    Ok(out)
}

fn utf8_len(lead: u8) -> usize {
    if lead >= 0xF0 {
        4
    } else if lead >= 0xE0 {
        3
    } else if lead >= 0xC0 {
        2
    } else {
        1
    }
}

/// Convenience: an object → `Vars`.
pub fn vars_from(map: &Map<String, Value>) -> Vars {
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars() -> Vars {
        let mut v = Vars::new();
        v.insert("key".into(), json!("agentd/x/run/1"));
        v.insert("seq".into(), json!(7));
        v.insert(
            "envelope".into(),
            json!({"v": 2, "state": {"a": "q\"uote"}}),
        );
        v.insert("prefix".into(), json!("agentd"));
        v.insert(
            "nested".into(),
            json!({"deep": {"n": 3}, "arr": ["x", "y"]}),
        );
        v
    }

    #[test]
    fn json_templates_substitute_typed_values() {
        let v = render_json(
            r#"{"key": "{key}", "seq": {seq}, "state": {envelope}}"#,
            &vars(),
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"key": "agentd/x/run/1", "seq": 7, "state": {"v": 2, "state": {"a": "q\"uote"}}})
        );
        // A lone placeholder yields the value itself.
        assert_eq!(
            render_json("{envelope}", &vars()).unwrap(),
            vars()["envelope"]
        );
        // Dotted placeholders reach into objects/arrays.
        assert_eq!(
            render_json(r#"{"n": {nested.deep.n}, "y": "{nested.arr.1}"}"#, &vars()).unwrap(),
            json!({"n": 3, "y": "y"})
        );
        // A string placeholder used bare becomes a JSON string.
        assert_eq!(
            render_json(r#"{"k": {key}}"#, &vars()).unwrap(),
            json!({"k": "agentd/x/run/1"})
        );
        // Double-brace spelling (RFC 0028 overrides) is the same placeholder.
        assert_eq!(
            render_json(r#"{"k": "{{key}}", "n": {{nested.deep.n}}}"#, &vars()).unwrap(),
            json!({"k": "agentd/x/run/1", "n": 3})
        );
        assert_eq!(
            render_json("{{envelope}}", &vars()).unwrap(),
            vars()["envelope"]
        );
        assert_eq!(
            render_text("x={{key}}", &vars()).unwrap(),
            "x=agentd/x/run/1"
        );
        // Unknown placeholder / non-JSON result are errors.
        assert!(render_json(r#"{"k": {nope}}"#, &vars()).is_err());
        assert!(render_json(r#"{"k": "{key}"#, &vars()).is_err());
        // Escaping: a value with quotes inside a quoted placeholder stays valid.
        let mut v2 = vars();
        v2.insert("key".into(), json!("a\"b"));
        assert_eq!(
            render_json(r#"{"k": "{key}"}"#, &v2).unwrap(),
            json!({"k": "a\"b"})
        );
    }

    #[test]
    fn text_templates_and_extraction() {
        assert_eq!(
            render_text("{prefix}/kv/{key}?seq={seq}", &vars()).unwrap(),
            "agentd/kv/agentd/x/run/1?seq=7"
        );
        assert!(render_text("{missing}", &vars()).is_err());
        let ctx = json!({"result": {"structuredContent": {"state": {"x": 1}, "keys": ["a"]}, "isError": false}, "status": 200});
        assert_eq!(
            extract("result.structuredContent.state", &ctx).unwrap(),
            Some(json!({"x": 1}))
        );
        assert_eq!(
            extract("/result/structuredContent/keys/0", &ctx).unwrap(),
            Some(json!("a"))
        );
        assert_eq!(
            extract("result.structuredContent.keys.0", &ctx).unwrap(),
            Some(json!("a"))
        );
        assert_eq!(extract("result.nope.deeper", &ctx).unwrap(), None);
        assert_eq!(extract("", &ctx).unwrap(), Some(ctx.clone()));
        assert!(
            truthy(&json!(true))
                && truthy(&json!(1))
                && truthy(&json!("x"))
                && !truthy(&json!(null))
                && !truthy(&json!(0))
        );
    }

    #[cfg(feature = "cel")]
    #[test]
    fn cel_templates_render_and_extract() {
        let v = render_json(r#"CEL: {"k": key, "next": seq + 1}"#, &vars()).unwrap();
        assert_eq!(v, json!({"k": "agentd/x/run/1", "next": 8}));
        assert_eq!(
            render_text("CEL: prefix + '/' + key", &vars()).unwrap(),
            "agentd/agentd/x/run/1"
        );
        let ctx = json!({"result": {"structuredContent": {"ok": true, "latest": 9}}});
        assert_eq!(
            extract("CEL: result.structuredContent.latest * 2", &ctx).unwrap(),
            Some(json!(18))
        );
    }
}
