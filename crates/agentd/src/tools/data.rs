//! Data and utility tool family (RFC §10.2).
//!
//! Pure handlers — no side effects, no policy gate.
//!
//! - `parse_json`      — parse a context string as JSON
//! - `json_select`     — dotted-path walk into a context value
//! - `template_render` — `{{key}}` substitution from a context value
//! - `diff_compute`    — structural JSON diff between two context values

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::tools::{resolve_string, resolve_value};
use crate::workflow::{Node, NodeKind};

pub(crate) fn register(registry: &mut HandlerRegistry) {
    registry.register("parse_json", Box::new(ParseJsonHandler));
    registry.register("json_select", Box::new(JsonSelectHandler));
    registry.register("template_render", Box::new(TemplateRenderHandler));
    registry.register("diff_compute", Box::new(DiffComputeHandler));
}

// ---------------------------------------------------------------------------
// parse_json
// ---------------------------------------------------------------------------

pub struct ParseJsonHandler;

impl NodeHandler for ParseJsonHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::ParseJson { input_from } = &node.kind else {
            return Err(mismatch(node, "parse_json"));
        };
        let raw = resolve_string("parse_json", ctx, input_from)?;
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| Error::Tool {
            tool: "parse_json".into(),
            reason: format!("invalid JSON at `{input_from}`: {e}"),
        })?;
        Ok(NodeOutcome::Continue {
            value: json!({ "parsed": parsed }),
            branch: None,
        })
    }
}

// ---------------------------------------------------------------------------
// json_select
// ---------------------------------------------------------------------------

pub struct JsonSelectHandler;

impl NodeHandler for JsonSelectHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::JsonSelect { input_from, path } = &node.kind else {
            return Err(mismatch(node, "json_select"));
        };
        let input = resolve_value("json_select", ctx, input_from)?;
        let selected = walk_path(&input, path).cloned().unwrap_or(Value::Null);
        Ok(NodeOutcome::Continue {
            value: json!({ "value": selected, "found": !selected.is_null() || is_null_literal(&input, path) }),
            branch: None,
        })
    }
}

/// Walk a dotted path into a JSON value. Unlike
/// `ExecutionContext::resolve_path`, the head is NOT a node id — the
/// whole path is relative to `root`. Arrays accept numeric segments
/// (`items.0.name`), mirroring `resolve_path`.
fn walk_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
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

/// Distinguish "path missing" from "path resolves to `null`".
fn is_null_literal(root: &Value, path: &str) -> bool {
    walk_path(root, path).is_some_and(|v| v.is_null())
}

// ---------------------------------------------------------------------------
// template_render
// ---------------------------------------------------------------------------

pub struct TemplateRenderHandler;

impl NodeHandler for TemplateRenderHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::TemplateRender {
            template,
            input_from,
        } = &node.kind
        else {
            return Err(mismatch(node, "template_render"));
        };
        let data = match input_from {
            Some(path) => resolve_value("template_render", ctx, path)?,
            None => Value::Null,
        };
        let rendered = render_template(template, &data)?;
        Ok(NodeOutcome::Continue {
            value: json!({ "rendered": rendered }),
            branch: None,
        })
    }
}

/// Minimal `{{key}}` substitution. `key` is a dotted path into `data`.
/// Unknown keys render as the literal `{{key}}` marker so the author
/// sees what went missing rather than a silent empty string.
fn render_template(template: &str, data: &Value) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // find matching `}}`
            let rest = &template[i + 2..];
            if let Some(end_rel) = rest.find("}}") {
                let key = rest[..end_rel].trim();
                match walk_path(data, key) {
                    Some(Value::String(s)) => out.push_str(s),
                    Some(v) => out.push_str(&serde_json::to_string(v).map_err(Error::Json)?),
                    None => {
                        out.push_str("{{");
                        out.push_str(key);
                        out.push_str("}}");
                    }
                }
                i += 2 + end_rel + 2; // skip over `{{key}}`
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// diff_compute
// ---------------------------------------------------------------------------

/// Structural diff between two JSON values. Emits:
///
/// ```json
/// {
///   "added":    { "path.to.field": <new value>, … },
///   "removed":  { "path.to.field": <old value>, … },
///   "changed":  { "path.to.field": { "from": …, "to": … }, … },
///   "unchanged": <bool>
/// }
/// ```
///
/// Paths use dot notation; arrays use bracket notation
/// (`items[2].name`). Uses `Value == Value` for leaf equality — no
/// type coercion, so `1` ≠ `"1"`. Consumers build their own
/// semantic interpretation on top (e.g., "only `changed` allowed"
/// for idempotency checks).
pub struct DiffComputeHandler;

impl NodeHandler for DiffComputeHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::DiffCompute {
            left_from,
            right_from,
        } = &node.kind
        else {
            return Err(mismatch(node, "diff_compute"));
        };
        let left = resolve_value("diff_compute", ctx, left_from)?;
        let right = resolve_value("diff_compute", ctx, right_from)?;

        let mut added = serde_json::Map::new();
        let mut removed = serde_json::Map::new();
        let mut changed = serde_json::Map::new();
        walk_diff("", &left, &right, &mut added, &mut removed, &mut changed);

        let unchanged = added.is_empty() && removed.is_empty() && changed.is_empty();
        Ok(NodeOutcome::Continue {
            value: json!({
                "added": Value::Object(added),
                "removed": Value::Object(removed),
                "changed": Value::Object(changed),
                "unchanged": unchanged,
            }),
            branch: None,
        })
    }
}

/// Depth-first walk over two JSON values. `prefix` is the dotted
/// path accumulated so far (empty at the top level). Leaves get
/// recorded in one of the three bins; nested objects/arrays recurse.
fn walk_diff(
    prefix: &str,
    left: &Value,
    right: &Value,
    added: &mut serde_json::Map<String, Value>,
    removed: &mut serde_json::Map<String, Value>,
    changed: &mut serde_json::Map<String, Value>,
) {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            for (k, lv) in l {
                let p = extend_path(prefix, k);
                match r.get(k) {
                    Some(rv) => walk_diff(&p, lv, rv, added, removed, changed),
                    None => {
                        removed.insert(p, lv.clone());
                    }
                }
            }
            for (k, rv) in r {
                if !l.contains_key(k) {
                    added.insert(extend_path(prefix, k), rv.clone());
                }
            }
        }
        (Value::Array(l), Value::Array(r)) => {
            // Array handling: match by index. Longer-side indices
            // become added / removed. For content-addressable array
            // diffs operators should pre-transform into keyed
            // objects before calling diff_compute.
            let max = l.len().max(r.len());
            for i in 0..max {
                let p = format!("{prefix}[{i}]");
                match (l.get(i), r.get(i)) {
                    (Some(lv), Some(rv)) => walk_diff(&p, lv, rv, added, removed, changed),
                    (Some(lv), None) => {
                        removed.insert(p, lv.clone());
                    }
                    (None, Some(rv)) => {
                        added.insert(p, rv.clone());
                    }
                    (None, None) => {}
                }
            }
        }
        (l, r) if l == r => {}
        (l, r) => {
            changed.insert(
                if prefix.is_empty() {
                    "$".into()
                } else {
                    prefix.to_string()
                },
                json!({ "from": l, "to": r }),
            );
        }
    }
}

fn extend_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn time_now_value() -> Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    json!({ "unix_ms": now })
}

fn mismatch(node: &Node, expected: &str) -> Error {
    Error::Tool {
        tool: expected.into(),
        reason: format!(
            "handler for `{expected}` received node `{}` of kind `{}`",
            node.id,
            node.kind.name()
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};

    fn ctx(input: Value) -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(input),
            &RunOptions::default(),
        )
    }

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.into(),
            retry: None,
            kind,
        }
    }

    #[test]
    fn parse_json_handles_well_formed_input() {
        let mut c = ctx(json!({ "raw": r#"{"n": 7}"# }));
        let out = ParseJsonHandler
            .handle(
                &node(
                    "p",
                    NodeKind::ParseJson {
                        input_from: "trigger.raw".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["parsed"]["n"], 7);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_json_rejects_invalid_input() {
        let mut c = ctx(json!({ "raw": "not json" }));
        let err = ParseJsonHandler
            .handle(
                &node(
                    "p",
                    NodeKind::ParseJson {
                        input_from: "trigger.raw".into(),
                    },
                ),
                &mut c,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("invalid JSON"));
    }

    #[test]
    fn json_select_walks_into_object() {
        let mut c = ctx(json!({ "body": { "user": { "name": "Ada" } } }));
        let out = JsonSelectHandler
            .handle(
                &node(
                    "s",
                    NodeKind::JsonSelect {
                        input_from: "trigger.body".into(),
                        path: "user.name".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["value"], "Ada");
                assert_eq!(value["found"], true);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn json_select_indexes_arrays_numerically() {
        let mut c = ctx(json!({ "body": { "items": [{ "id": "a" }, { "id": "b" }] } }));
        let out = JsonSelectHandler
            .handle(
                &node(
                    "s",
                    NodeKind::JsonSelect {
                        input_from: "trigger.body".into(),
                        path: "items.1.id".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["value"], "b");
                assert_eq!(value["found"], true);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn json_select_missing_path_returns_null_not_found() {
        let mut c = ctx(json!({ "body": { "user": {} } }));
        let out = JsonSelectHandler
            .handle(
                &node(
                    "s",
                    NodeKind::JsonSelect {
                        input_from: "trigger.body".into(),
                        path: "user.name".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["value"], Value::Null);
                assert_eq!(value["found"], false);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn template_render_substitutes_string_keys() {
        let mut c = ctx(json!({ "user": { "name": "Ada", "age": 36 } }));
        let out = TemplateRenderHandler
            .handle(
                &node(
                    "t",
                    NodeKind::TemplateRender {
                        template: "Hi {{user.name}}, you are {{user.age}}.".into(),
                        input_from: Some("trigger".into()),
                    },
                ),
                &mut c,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["rendered"], "Hi Ada, you are 36.");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn template_render_unknown_key_is_visible() {
        let mut c = ctx(json!({}));
        let out = TemplateRenderHandler
            .handle(
                &node(
                    "t",
                    NodeKind::TemplateRender {
                        template: "X={{nope}} Y={{also.missing}}".into(),
                        input_from: Some("trigger".into()),
                    },
                ),
                &mut c,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["rendered"], "X={{nope}} Y={{also.missing}}");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn template_render_no_input_renders_literals_through() {
        let mut c = ctx(json!({}));
        let out = TemplateRenderHandler
            .handle(
                &node(
                    "t",
                    NodeKind::TemplateRender {
                        template: "static text".into(),
                        input_from: None,
                    },
                ),
                &mut c,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["rendered"], "static text");
            }
            _ => panic!(),
        }
    }

    // -----------------------------------------------------------------
    // diff_compute
    // -----------------------------------------------------------------

    /// Run DiffCompute against two values staged in ctx under
    /// synthetic node ids `l` and `r`. Returns the handler output.
    fn diff(left: Value, right: Value) -> Value {
        let mut c = ctx(json!({}));
        c.node_outputs.insert("l".into(), left);
        c.node_outputs.insert("r".into(), right);
        let out = DiffComputeHandler
            .handle(
                &node(
                    "d",
                    NodeKind::DiffCompute {
                        left_from: "l".into(),
                        right_from: "r".into(),
                    },
                ),
                &mut c,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => value,
            _ => panic!("expected continue"),
        }
    }

    #[test]
    fn diff_detects_added_field() {
        let out = diff(json!({"a": 1}), json!({"a": 1, "b": 2}));
        assert_eq!(out["added"]["b"], json!(2));
        assert!(out["removed"].as_object().unwrap().is_empty());
        assert!(out["changed"].as_object().unwrap().is_empty());
        assert_eq!(out["unchanged"], json!(false));
    }

    #[test]
    fn diff_detects_removed_field() {
        let out = diff(json!({"a": 1, "b": 2}), json!({"a": 1}));
        assert_eq!(out["removed"]["b"], json!(2));
    }

    #[test]
    fn diff_detects_changed_leaf() {
        let out = diff(json!({"a": 1}), json!({"a": 2}));
        assert_eq!(out["changed"]["a"], json!({"from": 1, "to": 2}));
    }

    #[test]
    fn diff_recurses_into_nested_objects() {
        let out = diff(
            json!({"config": {"timeout": 30, "retries": 3}}),
            json!({"config": {"timeout": 60, "retries": 3, "new": true}}),
        );
        assert_eq!(
            out["changed"]["config.timeout"],
            json!({"from": 30, "to": 60})
        );
        assert_eq!(out["added"]["config.new"], json!(true));
    }

    #[test]
    fn diff_arrays_by_index() {
        let out = diff(json!({"items": [1, 2, 3]}), json!({"items": [1, 4, 3, 5]}));
        assert_eq!(out["changed"]["items[1]"], json!({"from": 2, "to": 4}));
        assert_eq!(out["added"]["items[3]"], json!(5));
    }

    #[test]
    fn diff_type_mismatch_is_change_not_crash() {
        let out = diff(json!({"a": 1}), json!({"a": "1"}));
        assert_eq!(out["changed"]["a"], json!({"from": 1, "to": "1"}));
    }

    #[test]
    fn diff_identical_values_report_unchanged() {
        let left = json!({"config": {"timeout": 30}, "items": [1, 2]});
        let right = left.clone();
        let out = diff(left, right);
        assert_eq!(out["unchanged"], json!(true));
    }

    #[test]
    fn diff_whole_value_changed_at_root() {
        let out = diff(json!(1), json!("hello"));
        assert_eq!(out["changed"]["$"], json!({"from": 1, "to": "hello"}));
    }
}
