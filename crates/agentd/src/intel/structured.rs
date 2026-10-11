// SPDX-License-Identifier: AGPL-3.0-only
//! Structured generation (RFC 0046 §5.8): the copy of an answer's JSON Schema
//! a provider is sent, so it constrains what the model writes rather than the
//! prompt asking for JSON and agentd hoping.
//!
//! A provider that constrains generation takes a subset of JSON Schema. Each
//! object must be closed, and the constraints a decoder cannot enforce token
//! by token — bounds, lengths, patterns, item counts, most `format`s — are
//! refused. The wire copy is therefore the answer's schema made sendable:
//! every object that lists `properties` is closed (an answer without the
//! extra keys still validates), and the refused constraints are left out.
//! Nothing is lost by either: the caller validates the answer against the
//! FULL schema afterwards and re-asks on a miss, exactly as it does when no
//! format is sent at all.
//!
//! Some schemas have no constrained form — a free-form object, `oneOf`,
//! `not`, a conditional, a recursive definition. Anthropic takes none of
//! them, so such a schema is asked in the prompt there; OpenAI takes any
//! schema with `strict: false`, as guidance, and enforces it (`strict: true`)
//! only when every object is closed and requires all its properties.
//!
//! Pure functions, no I/O: each dialect calls the one it needs.

use serde_json::{Map, Value};

/// The keywords a constraining provider refuses; the wire copy leaves them
/// out, and the caller's validation checks them on the answer.
const REFUSED: &[&str] = &[
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "contains",
    "minContains",
    "maxContains",
];

/// The string `format`s a constraining provider takes; any other is left out.
const FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

/// Keywords whose value is a map of names to subschemas.
const SCHEMA_MAPS: &[&str] = &["properties", "$defs", "definitions", "patternProperties"];

/// Keywords whose value is one subschema (or, for `items`, a list of them).
const SCHEMA_ONE: &[&str] = &["items", "additionalProperties", "not", "if", "then", "else"];

/// Keywords whose value is a list of subschemas.
const SCHEMA_LISTS: &[&str] = &["prefixItems", "anyOf", "allOf", "oneOf"];

/// The schema as a constraining provider takes it — Anthropic, and a Claude
/// model on Bedrock — or `None` when it has no constrained form: its root is
/// not an object, or some part of it is a free-form object, `oneOf`, `not`,
/// a conditional, a `$ref` outside `$defs`, or a recursive definition. A
/// schema with no constrained form is asked in the prompt.
pub fn constrained(schema: &Value) -> Option<Value> {
    if !is_object_root(schema) || open_part(schema) || recursive(schema) {
        return None;
    }
    Some(sendable(schema))
}

/// OpenAI's `json_schema` format for `schema`: the schema it is sent and
/// whether it is `strict`. Strict — the provider guarantees the shape — only
/// when the constrained copy exists and every object in it requires all of
/// its properties (and nothing is an `allOf`, which strict mode does not
/// take); otherwise the schema as written, unstripped, with `strict: false`,
/// which OpenAI takes as guidance whatever it holds. `None` when the root is
/// not an object: the format is an object's.
pub fn openai(schema: &Value) -> Option<(Value, bool)> {
    if !is_object_root(schema) {
        return None;
    }
    Some(match constrained(schema) {
        Some(wire) if strict_ready(&wire) => (wire, true),
        _ => (schema.clone(), false),
    })
}

/// `schema` with every object that lists `properties` closed and the refused
/// constraints left out, through every subschema. Property NAMES are kept
/// whatever they are — a property called `pattern` is not a constraint.
pub fn sendable(schema: &Value) -> Value {
    let Value::Object(m) = schema else {
        return schema.clone();
    };
    let mut out = Map::new();
    for (k, v) in m {
        let k = k.as_str();
        if REFUSED.contains(&k) {
            continue;
        }
        let wire = if k == "format" {
            match v.as_str() {
                Some(f) if FORMATS.contains(&f) => v.clone(),
                _ => continue,
            }
        } else if SCHEMA_MAPS.contains(&k) {
            match v {
                Value::Object(named) => Value::Object(
                    named
                        .iter()
                        .map(|(name, s)| (name.clone(), sendable(s)))
                        .collect(),
                ),
                other => other.clone(),
            }
        } else if SCHEMA_ONE.contains(&k) || SCHEMA_LISTS.contains(&k) {
            match v {
                Value::Array(list) => Value::Array(list.iter().map(sendable).collect()),
                other => sendable(other),
            }
        } else {
            v.clone()
        };
        out.insert(k.to_string(), wire);
    }
    if out.contains_key("properties") {
        out.insert("additionalProperties".into(), Value::Bool(false));
    }
    Value::Object(out)
}

/// Whether the root is an object schema — the only root a format takes.
fn is_object_root(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("object")
}

/// Whether some part of `schema` has no constrained form.
fn open_part(schema: &Value) -> bool {
    let Value::Object(m) = schema else {
        return false;
    };
    if ["oneOf", "not", "if", "then", "else", "patternProperties"]
        .iter()
        .any(|k| m.contains_key(*k))
    {
        return true;
    }
    // A free-form object: one that names no properties, or admits keys of
    // its own shape beyond them.
    let is_object = match m.get("type") {
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "object"),
        _ => false,
    };
    if (is_object && !m.contains_key("properties"))
        || m.get("additionalProperties").is_some_and(Value::is_object)
    {
        return true;
    }
    if let Some(r) = m.get("$ref")
        && def_name(r).is_none()
    {
        return true;
    }
    subschemas(m).any(open_part)
}

/// Every direct subschema of a schema object.
fn subschemas(m: &Map<String, Value>) -> impl Iterator<Item = &Value> {
    m.iter().flat_map(|(k, v)| {
        let k = k.as_str();
        let found: Vec<&Value> = if SCHEMA_MAPS.contains(&k) {
            v.as_object()
                .map(|named| named.values().collect())
                .unwrap_or_default()
        } else if SCHEMA_ONE.contains(&k) || SCHEMA_LISTS.contains(&k) {
            match v {
                Value::Array(list) => list.iter().collect(),
                Value::Object(_) => vec![v],
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        found
    })
}

/// The definition a `$ref` names, when it names one under `$defs` (or the
/// older `definitions`).
fn def_name(r: &Value) -> Option<&str> {
    let r = r.as_str()?;
    r.strip_prefix("#/$defs/")
        .or_else(|| r.strip_prefix("#/definitions/"))
        .filter(|n| !n.is_empty() && !n.contains('/'))
}

/// Whether a definition reaches itself through `$ref`s — a recursive schema,
/// which a constraining provider does not take.
fn recursive(schema: &Value) -> bool {
    let defs: Map<String, Value> = ["$defs", "definitions"]
        .iter()
        .filter_map(|k| schema.get(*k).and_then(Value::as_object))
        .flat_map(|m| m.clone())
        .collect();
    let refs_of = |name: &str| -> Vec<String> {
        let mut out = Vec::new();
        if let Some(d) = defs.get(name) {
            collect_refs(d, &mut out);
        }
        out
    };
    defs.keys().any(|start| {
        let mut seen: Vec<String> = Vec::new();
        let mut todo = refs_of(start);
        while let Some(next) = todo.pop() {
            if next == *start {
                return true;
            }
            if !seen.contains(&next) {
                todo.extend(refs_of(&next));
                seen.push(next);
            }
        }
        false
    })
}

/// The definitions every `$ref` inside `v` names.
fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(name) = m.get("$ref").and_then(|r| def_name(r)) {
                out.push(name.to_string());
            }
            m.values().for_each(|c| collect_refs(c, out));
        }
        Value::Array(list) => list.iter().for_each(|c| collect_refs(c, out)),
        _ => {}
    }
}

/// Whether OpenAI's strict mode takes `wire` (a [`constrained`] copy): every
/// object requires all of its properties, and nothing is an `allOf`.
fn strict_ready(wire: &Value) -> bool {
    let Value::Object(m) = wire else {
        return true;
    };
    if m.contains_key("allOf") {
        return false;
    }
    if let Some(props) = m.get("properties").and_then(Value::as_object) {
        let required: Vec<&str> = m
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !props.keys().all(|p| required.contains(&p.as_str())) {
            return false;
        }
    }
    subschemas(m).all(strict_ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Each object that lists properties is closed — even one written open —
    /// and the refused constraints are left out at every depth, while a
    /// property NAMED like a keyword, an allowed `format`, `enum` and the
    /// descriptions are kept.
    #[test]
    fn the_wire_copy_is_closed_and_stripped_at_every_depth() {
        let schema = json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "pattern": {"type": "string", "minLength": 2, "pattern": "^a", "description": "a field named pattern"},
                "when": {"type": "string", "format": "date-time"},
                "colour": {"type": "string", "format": "hex-colour"},
                "score": {"type": "number", "minimum": 0, "maximum": 1},
                "tags": {"type": "array", "minItems": 1, "uniqueItems": true, "items": {
                    "type": "object", "properties": {"k": {"enum": ["a", "b"]}}, "maxProperties": 3
                }},
                "either": {"anyOf": [{"type": "integer", "multipleOf": 2}, {"type": "null"}]},
                "ref": {"$ref": "#/$defs/item"}
            },
            "required": ["pattern"],
            "$defs": {"item": {"type": "object", "properties": {"n": {"type": "integer", "exclusiveMinimum": 0}}}}
        });
        let wire = constrained(&schema).expect("a constrained form");
        assert_eq!(
            wire["additionalProperties"], false,
            "closed though written open"
        );
        let p = &wire["properties"];
        assert_eq!(
            p["pattern"],
            json!({"type": "string", "description": "a field named pattern"})
        );
        assert_eq!(p["when"]["format"], "date-time");
        assert!(p["colour"].get("format").is_none(), "{wire}");
        assert_eq!(p["score"], json!({"type": "number"}));
        assert_eq!(
            p["tags"],
            json!({"type": "array", "items": {"type": "object", "properties": {"k": {"enum": ["a", "b"]}}, "additionalProperties": false}})
        );
        assert_eq!(p["either"]["anyOf"][0], json!({"type": "integer"}));
        assert_eq!(
            wire["$defs"]["item"],
            json!({"type": "object", "properties": {"n": {"type": "integer"}}, "additionalProperties": false})
        );
        assert_eq!(wire["required"], json!(["pattern"]));
    }

    /// A schema with a part no constrained form takes has none: it is asked
    /// in the prompt on Anthropic.
    #[test]
    fn a_schema_with_an_open_part_has_no_constrained_form() {
        let with = |prop: Value| json!({"type": "object", "properties": {"x": prop}});
        for open in [
            // A free-form object, written three ways.
            with(json!({"type": "object"})),
            with(
                json!({"type": "object", "properties": {}, "additionalProperties": {"type": "string"}}),
            ),
            with(json!({"type": "object", "patternProperties": {"^a": {"type": "string"}}})),
            with(json!({"oneOf": [{"type": "string"}, {"type": "integer"}]})),
            with(json!({"not": {"type": "string"}})),
            with(json!({"if": {"type": "string"}, "then": {"minLength": 1}})),
            with(json!({"$ref": "https://example.com/schema.json"})),
            // The root itself free-form, or not an object at all.
            json!({"type": "object"}),
            json!({"type": "array", "items": {"type": "string"}}),
            json!({"properties": {"a": {"type": "string"}}}),
        ] {
            assert_eq!(constrained(&open), None, "{open}");
        }
        // A definition that reaches itself, directly or through another.
        let recursive = json!({"type": "object", "properties": {"tree": {"$ref": "#/$defs/node"}},
        "$defs": {
            "node": {"type": "object", "properties": {"kids": {"type": "array", "items": {"$ref": "#/$defs/kids"}}}},
            "kids": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}
        }});
        assert_eq!(constrained(&recursive), None);
        // An unrelated nesting of definitions is not recursion.
        let nested = json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/a"}},
            "$defs": {"a": {"type": "object", "properties": {"b": {"$ref": "#/$defs/b"}}},
                      "b": {"type": "object", "properties": {"c": {"type": "string"}}}}});
        assert!(constrained(&nested).is_some());
    }

    /// OpenAI is sent every object schema: strict when the constrained copy
    /// requires every property everywhere, else the schema as written, as
    /// guidance. A root that is not an object is no format.
    #[test]
    fn openai_is_strict_only_when_every_property_is_required() {
        let full = json!({"type": "object", "properties": {
            "a": {"type": "string", "maxLength": 5},
            "b": {"type": "array", "items": {"type": "object", "properties": {"c": {"type": "boolean"}}, "required": ["c"]}}
        }, "required": ["a", "b"]});
        let (wire, strict) = openai(&full).unwrap();
        assert!(strict);
        assert_eq!(wire, constrained(&full).unwrap());
        assert!(wire["properties"]["a"].get("maxLength").is_none());

        // An optional property, deep or shallow: guidance, unstripped.
        let mut optional = full.clone();
        optional["properties"]["b"]["items"]["required"] = json!([]);
        let (wire, strict) = openai(&optional).unwrap();
        assert!(!strict);
        assert_eq!(wire, optional, "the schema as written");

        // An open part, or an allOf: guidance too.
        let open =
            json!({"type": "object", "properties": {"x": {"type": "object"}}, "required": ["x"]});
        assert_eq!(openai(&open), Some((open.clone(), false)));
        let all_of = json!({"type": "object", "properties": {"x": {"allOf": [{"type": "string"}]}}, "required": ["x"]});
        assert_eq!(openai(&all_of).map(|(_, s)| s), Some(false));
        // The think tool's bare object is sendable, as guidance.
        assert_eq!(
            openai(&json!({"type": "object"})),
            Some((json!({"type": "object"}), false))
        );
        assert_eq!(openai(&json!({"type": "string"})), None);
    }
}
