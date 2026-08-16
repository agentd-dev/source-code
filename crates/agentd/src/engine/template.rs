// SPDX-License-Identifier: Apache-2.0
//! Workflow **templates** (RFC 0027 §3): `{{path}}` interpolation with dotted /
//! JSON-pointer paths and `{{path | default}}` over the run data (`inputs`,
//! `run`, `steps.<id>.output`, `vars`, `memory.<key>`, `item`, `index`,
//! `batch`, `env`), plus `CEL:` expressions over the same names (feature
//! `cel`; a non-CEL build refuses them at validation). Dependency-free.
//!
//! Rules: a string that is exactly one `{{path}}` yields the value itself
//! (typed); any other string interpolates (strings raw, other values as
//! JSON); objects and arrays render recursively; a missing path with no
//! default is an error (the step takes its error edge rather than running
//! with a silently-wrong shape).

use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The named inputs of a render (RFC 0027 §3 data model).
pub type Data = BTreeMap<String, Value>;

/// Render `template` over `data`.
pub fn render(template: &Value, data: &Data) -> Result<Value, String> {
    match template {
        Value::String(s) => render_str(s, data),
        Value::Array(a) => a
            .iter()
            .map(|v| render(v, data))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, v) in o {
                out.insert(k.clone(), render(v, data)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// Render a string template: `CEL:` expression, a lone `{{path}}` (typed), or
/// interpolation.
pub fn render_str(s: &str, data: &Data) -> Result<Value, String> {
    let t = s.trim_start();
    if let Some(expr) = t.strip_prefix("CEL:") {
        let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
        return crate::cel::eval_value(expr.trim(), &vars).map_err(|e| format!("CEL: {e}"));
    }
    if !s.contains("{{") {
        return Ok(Value::String(s.to_string()));
    }
    // A lone placeholder yields the typed value.
    let trimmed = s.trim();
    if trimmed.starts_with("{{") && trimmed.ends_with("}}") && trimmed.matches("{{").count() == 1 {
        let inner = &trimmed[2..trimmed.len() - 2];
        return resolve_placeholder(inner, data);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(format!("unterminated placeholder in {s:?}"));
        };
        let inner = &after[..end];
        let v = resolve_placeholder(inner, data)?;
        out.push_str(&match v {
            Value::String(x) => x,
            Value::Null => String::new(),
            other => other.to_string(),
        });
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(Value::String(out))
}

/// `path` or `path | default` (default parsed as JSON, else a literal string).
fn resolve_placeholder(inner: &str, data: &Data) -> Result<Value, String> {
    // `{{secret:NAME}}` / `{{secret-file:PATH}}` are config-secret references,
    // not workflow data. Leave them verbatim so the consuming node resolves
    // them through the redacting secret resolver — a credential must never be
    // expanded into rendered step data (and thence into logs/outputs).
    let t = inner.trim();
    if t.starts_with("secret:") || t.starts_with("secret-file:") {
        return Ok(Value::String(format!("{{{{{t}}}}}")));
    }
    let (path, default) = match inner.split_once('|') {
        Some((p, d)) => (p.trim(), Some(d.trim())),
        None => (inner.trim(), None),
    };
    match lookup(path, data) {
        Some(v) => Ok(v),
        None => match default {
            Some(d) => Ok(serde_json::from_str::<Value>(d)
                .unwrap_or_else(|_| Value::String(d.trim_matches(['"', '\'']).to_string()))),
            None => Err(format!(
                "template path {path:?} is not set (no default given)"
            )),
        },
    }
}

/// Look up a dotted or JSON-pointer path in the data.
pub fn lookup(path: &str, data: &Data) -> Option<Value> {
    if path.is_empty() {
        return None;
    }
    if let Some(p) = path.strip_prefix('/') {
        let (head, rest) = match p.split_once('/') {
            Some((h, r)) => (h, Some(r)),
            None => (p, None),
        };
        let root = data.get(head)?;
        return match rest {
            None => Some(root.clone()),
            Some(r) => root.pointer(&format!("/{r}")).cloned(),
        };
    }
    let mut segs = path.split('.');
    let head = segs.next()?;
    let mut cur = data.get(head)?;
    for seg in segs {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur.clone())
}

/// Every `{{path}}` root referenced by a template (`steps`, `vars`, …) — for
/// validation / dependency hints.
pub fn referenced_roots(template: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                let mut rest = s.as_str();
                while let Some(start) = rest.find("{{") {
                    let after = &rest[start + 2..];
                    let Some(end) = after.find("}}") else { break };
                    let inner = after[..end].split('|').next().unwrap_or("").trim();
                    let root = inner
                        .trim_start_matches('/')
                        .split(['.', '/'])
                        .next()
                        .unwrap_or("")
                        .to_string();
                    if !root.is_empty() && !out.contains(&root) {
                        out.push(root);
                    }
                    rest = &after[end + 2..];
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(template, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data() -> Data {
        let mut d = Data::new();
        d.insert("inputs".into(), json!({"instruction": "do it", "n": 3}));
        d.insert(
            "steps".into(),
            json!({"fetch": {"status": "done", "output": {"items": [1, 2, 3], "name": "x"}}}),
        );
        d.insert("vars".into(), json!({"count": 2}));
        d.insert(
            "env".into(),
            json!({"instance": "i", "instruction": "brief"}),
        );
        d
    }

    #[test]
    fn typed_lone_placeholders_interpolation_defaults_and_pointers() {
        let d = data();
        assert_eq!(
            render(&json!("{{steps.fetch.output.items}}"), &d).unwrap(),
            json!([1, 2, 3])
        );
        assert_eq!(render(&json!("  {{inputs.n}} "), &d).unwrap(), json!(3));
        assert_eq!(render(&json!("count={{vars.count}}, first={{steps.fetch.output.items.0}}, name={{steps.fetch.output.name}}"), &d).unwrap(), json!("count=2, first=1, name=x"));
        assert_eq!(
            render(&json!("{{/steps/fetch/output/items/1}}"), &d).unwrap(),
            json!(2)
        );
        assert_eq!(
            render(&json!("{{vars.missing | 7}}"), &d).unwrap(),
            json!(7)
        );
        assert_eq!(
            render(&json!("{{vars.missing | \"dflt\"}}"), &d).unwrap(),
            json!("dflt")
        );
        assert_eq!(
            render(&json!("x{{vars.missing | y}}z"), &d).unwrap(),
            json!("xyz")
        );
        assert!(
            render(&json!("{{vars.missing}}"), &d)
                .unwrap_err()
                .contains("not set")
        );
        assert!(
            render(&json!("{{oops"), &d)
                .unwrap_err()
                .contains("unterminated")
        );
        // Recursion into objects/arrays; non-strings pass through.
        let v = render(
            &json!({"a": ["{{inputs.n}}", {"b": "{{env.instruction}}"}], "c": 5, "d": null}),
            &d,
        )
        .unwrap();
        assert_eq!(v, json!({"a": [3, {"b": "brief"}], "c": 5, "d": null}));
        assert_eq!(
            referenced_roots(
                &json!({"a": "{{steps.x.output}} {{vars.y | 1}}", "b": ["{{/inputs/z}}"]})
            ),
            vec!["steps", "vars", "inputs"]
        );
        assert_eq!(render(&json!("plain"), &d).unwrap(), json!("plain"));
    }

    #[cfg(feature = "cel")]
    #[test]
    fn cel_values_evaluate_over_the_data() {
        let d = data();
        assert_eq!(render(&json!("CEL: inputs.n * 2"), &d).unwrap(), json!(6));
        assert_eq!(
            render(&json!("CEL: steps.fetch.output.items.size()"), &d).unwrap(),
            json!(3)
        );
        assert!(render(&json!("CEL: nope.x"), &d).is_err());
    }
}
