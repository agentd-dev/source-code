//! Minimal `{{key}}` template substitution + the dotted-path walk it
//! uses. Lives in the engine (always compiled) because both the
//! `tools-data` family (`template_render`, `json_select`) and the core
//! `respond` node render templates — a control node can't depend on an
//! optional tool family.

use crate::error::{Error, Result};
use serde_json::Value;

/// Walk a dotted path into a JSON value. The whole path is relative to
/// `root` (the head is NOT a node id — that's
/// `ExecutionContext::resolve_path`). Objects resolve by key; arrays
/// accept numeric segments (`items.0.name`); anything else is a miss.
pub(crate) fn walk_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cursor = root;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        cursor = match cursor {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cursor)
}

/// Minimal `{{key}}` substitution. `key` is a dotted path into `data`.
/// Unknown keys render as the literal `{{key}}` marker so the author
/// sees what went missing rather than a silent empty string.
///
/// Literal text is copied as string slices (never byte-by-byte), so
/// multi-byte UTF-8 in templates survives intact.
pub(crate) fn render_template(template: &str, data: &Value) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let (literal, after_open) = rest.split_at(open);
        out.push_str(literal);
        match after_open[2..].find("}}") {
            Some(end_rel) => {
                let key = after_open[2..2 + end_rel].trim();
                match walk_path(data, key) {
                    Some(Value::String(s)) => out.push_str(s),
                    Some(v) => out.push_str(&serde_json::to_string(v).map_err(Error::Json)?),
                    None => {
                        out.push_str("{{");
                        out.push_str(key);
                        out.push_str("}}");
                    }
                }
                rest = &after_open[2 + end_rel + 2..];
            }
            None => {
                // Unclosed marker — emit the rest verbatim.
                out.push_str(after_open);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitutes_paths_and_keeps_unknown_markers() {
        let data = json!({ "user": { "name": "Ada" }, "n": 7 });
        let out = render_template("hi {{user.name}}, n={{n}}, {{missing}}!", &data).unwrap();
        assert_eq!(out, "hi Ada, n=7, {{missing}}!");
    }

    #[test]
    fn multibyte_utf8_literals_survive() {
        let out = render_template("Hello, {{who}} — ça va? ✓", &json!({ "who": "wörld" })).unwrap();
        assert_eq!(out, "Hello, wörld — ça va? ✓");
    }

    #[test]
    fn array_segments_resolve_in_templates() {
        let data = json!({ "results": [{ "result": "de" }, { "result": "fr" }] });
        let out = render_template("first={{results.0.result}}", &data).unwrap();
        assert_eq!(out, "first=de");
    }

    #[test]
    fn unclosed_marker_is_verbatim() {
        let out = render_template("a {{open and done", &json!({})).unwrap();
        assert_eq!(out, "a {{open and done");
    }
}
