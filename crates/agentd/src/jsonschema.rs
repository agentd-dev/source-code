// SPDX-License-Identifier: AGPL-3.0-only
//! A dependency-free **JSON Schema subset validator** (draft 2020-12 vocabulary,
//! the parts tool contracts and workflow schemas actually use): `type` (single
//! or list, `integer` distinct from `number`), `properties`, `required`,
//! `additionalProperties` (bool or schema), `patternProperties` (literal-prefix
//! and `^…$` anchored-literal patterns only — no regex engine), `enum`, `const`,
//! `items` (schema), `prefixItems`, `minItems`/`maxItems`, `uniqueItems`,
//! `minimum`/`maximum`/`exclusiveMinimum`/`exclusiveMaximum`, `multipleOf`,
//! `minLength`/`maxLength`, `minProperties`/`maxProperties`, `allOf`/`anyOf`/
//! `oneOf`/`not`, `if`/`then`/`else`, `$ref` to `#/$defs/<name>` /
//! `#/definitions/<name>` / `#` (root), boolean schemas, `nullable` (OpenAPI
//! sugar), `default` (ignored), `format`/`pattern`/`description`/`title`/
//! `examples`/`$schema`/`$id`/`$comment` (accepted, not enforced — `pattern`
//! is checked only for the same literal shapes as `patternProperties`).
//!
//! Errors are collected (not fail-fast) and name the JSON pointer of the
//! offending value, so a model or an author sees every miss at once.

use serde_json::{Map, Value};

/// Validate `value` against `schema`. `Ok(())` or every violation, each as
/// `"<pointer>: <message>"` (`<pointer>` is `/a/0/b`, or `/` for the root).
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut errs = Vec::new();
    let mut depth = 0u32;
    walk(schema, schema, value, "", &mut errs, &mut depth);
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

/// One-line rendering of a validation failure (for tool errors / logs).
pub fn explain(errs: &[String]) -> String {
    errs.join("; ")
}

/// Whether `schema` is a well-formed schema for this validator: an object or
/// a boolean, with known-typed keywords. Unknown keywords are allowed (JSON
/// Schema is open); wrong-typed known keywords are reported.
pub fn check_schema(schema: &Value) -> Result<(), Vec<String>> {
    let mut errs = Vec::new();
    check_schema_at(schema, "", &mut errs, 0);
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

const MAX_DEPTH: u32 = 64;

fn ptr(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

fn walk(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    errs: &mut Vec<String>,
    depth: &mut u32,
) {
    if *depth > MAX_DEPTH {
        errs.push(format!("{}: schema nesting exceeds {MAX_DEPTH}", ptr(path)));
        return;
    }
    *depth += 1;
    walk_inner(root, schema, value, path, errs, depth);
    *depth -= 1;
}

fn walk_inner(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    errs: &mut Vec<String>,
    depth: &mut u32,
) {
    let s = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errs.push(format!("{}: no value is allowed here", ptr(path)));
            return;
        }
        Value::Object(s) => s,
        _ => {
            errs.push(format!(
                "{}: invalid schema (not an object or boolean)",
                ptr(path)
            ));
            return;
        }
    };
    // $ref (draft 2020-12: siblings apply too).
    if let Some(Value::String(r)) = s.get("$ref") {
        match resolve_ref(root, r) {
            Some(target) => walk(root, target, value, path, errs, depth),
            None => errs.push(format!("{}: unresolvable $ref {r:?}", ptr(path))),
        }
    }
    // nullable sugar.
    if value.is_null() && s.get("nullable") == Some(&Value::Bool(true)) {
        return;
    }
    // type
    if let Some(t) = s.get("type") {
        let ok = match t {
            Value::String(t) => type_matches(t, value),
            Value::Array(ts) => ts
                .iter()
                .any(|t| t.as_str().is_some_and(|t| type_matches(t, value))),
            _ => true,
        };
        if !ok {
            errs.push(format!(
                "{}: expected type {}, got {}",
                ptr(path),
                render_type(t),
                type_name(value)
            ));
            // A type miss makes most other keywords moot; keep going only for
            // combinators so `anyOf` failures still explain themselves.
        }
    }
    // enum / const
    if let Some(Value::Array(e)) = s.get("enum")
        && !e.iter().any(|x| x == value)
    {
        errs.push(format!(
            "{}: value is not one of the allowed values ({})",
            ptr(path),
            short(&Value::Array(e.clone()))
        ));
    }
    if let Some(c) = s.get("const")
        && c != value
    {
        errs.push(format!("{}: value must equal {}", ptr(path), short(c)));
    }
    // Per-type keywords.
    match value {
        Value::Object(obj) => object_keywords(root, s, obj, path, errs, depth),
        Value::Array(arr) => array_keywords(root, s, arr, path, errs, depth),
        Value::String(st) => string_keywords(s, st, path, errs),
        Value::Number(n) => number_keywords(s, n, path, errs),
        _ => {}
    }
    // Combinators.
    if let Some(Value::Array(all)) = s.get("allOf") {
        for sub in all {
            walk(root, sub, value, path, errs, depth);
        }
    }
    if let Some(Value::Array(any)) = s.get("anyOf") {
        let mut best: Option<Vec<String>> = None;
        let mut matched = false;
        for sub in any {
            let mut e = Vec::new();
            walk(root, sub, value, path, &mut e, depth);
            if e.is_empty() {
                matched = true;
                break;
            }
            if best.as_ref().is_none_or(|b| e.len() < b.len()) {
                best = Some(e);
            }
        }
        if !matched {
            errs.push(format!(
                "{}: matches none of anyOf ({})",
                ptr(path),
                best.map(|b| b.join("; ")).unwrap_or_default()
            ));
        }
    }
    if let Some(Value::Array(one)) = s.get("oneOf") {
        let mut n = 0;
        let mut best: Option<Vec<String>> = None;
        for sub in one {
            let mut e = Vec::new();
            walk(root, sub, value, path, &mut e, depth);
            if e.is_empty() {
                n += 1;
            } else if best.as_ref().is_none_or(|b| e.len() < b.len()) {
                best = Some(e);
            }
        }
        if n != 1 {
            errs.push(format!(
                "{}: must match exactly one of oneOf (matched {n}{})",
                ptr(path),
                if n == 0 {
                    format!("; {}", best.map(|b| b.join("; ")).unwrap_or_default())
                } else {
                    String::new()
                }
            ));
        }
    }
    if let Some(not) = s.get("not") {
        let mut e = Vec::new();
        walk(root, not, value, path, &mut e, depth);
        if e.is_empty() {
            errs.push(format!("{}: must not match the `not` schema", ptr(path)));
        }
    }
    if let Some(cond) = s.get("if") {
        let mut e = Vec::new();
        walk(root, cond, value, path, &mut e, depth);
        let branch = if e.is_empty() {
            s.get("then")
        } else {
            s.get("else")
        };
        if let Some(b) = branch {
            walk(root, b, value, path, errs, depth);
        }
    }
}

fn object_keywords(
    root: &Value,
    s: &Map<String, Value>,
    obj: &Map<String, Value>,
    path: &str,
    errs: &mut Vec<String>,
    depth: &mut u32,
) {
    if let Some(Value::Array(req)) = s.get("required") {
        for r in req {
            if let Some(r) = r.as_str()
                && !obj.contains_key(r)
            {
                errs.push(format!("{}: missing required property {r:?}", ptr(path)));
            }
        }
    }
    let props = s.get("properties").and_then(Value::as_object);
    let pattern_props = s.get("patternProperties").and_then(Value::as_object);
    let additional = s.get("additionalProperties");
    for (k, v) in obj {
        let child = format!("{path}/{}", escape(k));
        let mut covered = false;
        if let Some(sub) = props.and_then(|p| p.get(k)) {
            covered = true;
            walk(root, sub, v, &child, errs, depth);
        }
        if let Some(pp) = pattern_props {
            for (pat, sub) in pp {
                if literal_pattern_matches(pat, k) {
                    covered = true;
                    walk(root, sub, v, &child, errs, depth);
                }
            }
        }
        if !covered {
            match additional {
                Some(Value::Bool(false)) => {
                    errs.push(format!("{}: unknown property {k:?}", ptr(path)))
                }
                Some(sub @ Value::Object(_)) => walk(root, sub, v, &child, errs, depth),
                _ => {}
            }
        }
    }
    if let Some(n) = s.get("minProperties").and_then(Value::as_u64)
        && (obj.len() as u64) < n
    {
        errs.push(format!("{}: at least {n} properties required", ptr(path)));
    }
    if let Some(n) = s.get("maxProperties").and_then(Value::as_u64)
        && (obj.len() as u64) > n
    {
        errs.push(format!("{}: at most {n} properties allowed", ptr(path)));
    }
    if let Some(Value::Object(deps)) = s.get("dependentRequired") {
        for (k, needs) in deps {
            if obj.contains_key(k)
                && let Some(needs) = needs.as_array()
            {
                for n in needs.iter().filter_map(Value::as_str) {
                    if !obj.contains_key(n) {
                        errs.push(format!("{}: property {k:?} requires {n:?}", ptr(path)));
                    }
                }
            }
        }
    }
}

fn array_keywords(
    root: &Value,
    s: &Map<String, Value>,
    arr: &[Value],
    path: &str,
    errs: &mut Vec<String>,
    depth: &mut u32,
) {
    let prefix = s.get("prefixItems").and_then(Value::as_array);
    let items = s.get("items");
    for (i, v) in arr.iter().enumerate() {
        let child = format!("{path}/{i}");
        if let Some(p) = prefix.and_then(|p| p.get(i)) {
            walk(root, p, v, &child, errs, depth);
            continue;
        }
        match items {
            Some(Value::Bool(false)) if prefix.is_some() => {
                errs.push(format!("{}: no additional items allowed", ptr(&child)));
            }
            Some(sub @ (Value::Object(_) | Value::Bool(_))) => {
                walk(root, sub, v, &child, errs, depth)
            }
            _ => {}
        }
    }
    if let Some(n) = s.get("minItems").and_then(Value::as_u64)
        && (arr.len() as u64) < n
    {
        errs.push(format!("{}: at least {n} items required", ptr(path)));
    }
    if let Some(n) = s.get("maxItems").and_then(Value::as_u64)
        && (arr.len() as u64) > n
    {
        errs.push(format!("{}: at most {n} items allowed", ptr(path)));
    }
    if s.get("uniqueItems") == Some(&Value::Bool(true)) {
        for i in 0..arr.len() {
            if arr[i + 1..].contains(&arr[i]) {
                errs.push(format!(
                    "{}: items must be unique (duplicate at {i})",
                    ptr(path)
                ));
                break;
            }
        }
    }
    if let Some(c) = s.get("contains") {
        let any = arr.iter().any(|v| {
            let mut e = Vec::new();
            walk(root, c, v, path, &mut e, depth);
            e.is_empty()
        });
        if !any {
            errs.push(format!("{}: no item matches `contains`", ptr(path)));
        }
    }
}

fn string_keywords(s: &Map<String, Value>, st: &str, path: &str, errs: &mut Vec<String>) {
    let len = st.chars().count() as u64;
    if let Some(n) = s.get("minLength").and_then(Value::as_u64)
        && len < n
    {
        errs.push(format!("{}: at least {n} characters required", ptr(path)));
    }
    if let Some(n) = s.get("maxLength").and_then(Value::as_u64)
        && len > n
    {
        errs.push(format!("{}: at most {n} characters allowed", ptr(path)));
    }
    if let Some(Value::String(p)) = s.get("pattern")
        && is_literal_pattern(p)
        && !literal_pattern_matches(p, st)
    {
        errs.push(format!("{}: does not match pattern {p:?}", ptr(path)));
    }
}

fn number_keywords(
    s: &Map<String, Value>,
    n: &serde_json::Number,
    path: &str,
    errs: &mut Vec<String>,
) {
    let Some(x) = n.as_f64() else { return };
    let num = |k: &str| s.get(k).and_then(Value::as_f64);
    if let Some(m) = num("minimum")
        && x < m
    {
        errs.push(format!("{}: must be >= {m}", ptr(path)));
    }
    if let Some(m) = num("maximum")
        && x > m
    {
        errs.push(format!("{}: must be <= {m}", ptr(path)));
    }
    if let Some(m) = num("exclusiveMinimum")
        && x <= m
    {
        errs.push(format!("{}: must be > {m}", ptr(path)));
    }
    if let Some(m) = num("exclusiveMaximum")
        && x >= m
    {
        errs.push(format!("{}: must be < {m}", ptr(path)));
    }
    if let Some(m) = num("multipleOf")
        && m > 0.0
    {
        let q = x / m;
        if (q - q.round()).abs() > 1e-9 {
            errs.push(format!("{}: must be a multiple of {m}", ptr(path)));
        }
    }
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => {
            v.as_i64().is_some()
                || v.as_u64().is_some()
                || v.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => true, // unknown type names are not enforced
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn render_type(t: &Value) -> String {
    match t {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|"),
        _ => "?".into(),
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 80 {
        let mut cut = 77;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    } else {
        s
    }
}

fn escape(k: &str) -> String {
    k.replace('~', "~0").replace('/', "~1")
}

/// Resolve `#`, `#/$defs/x`, `#/definitions/x`, or any `#/a/b` pointer.
fn resolve_ref<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    let rest = r.strip_prefix('#')?;
    if rest.is_empty() {
        return Some(root);
    }
    let mut cur = root;
    for seg in rest.trim_start_matches('/').split('/') {
        let seg = seg.replace("~1", "/").replace("~0", "~");
        cur = match cur {
            Value::Object(m) => m.get(&seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The pattern shapes we can honour without a regex engine: an anchored
/// literal (`^foo$`), an anchored literal prefix (`^foo`), an anchored literal
/// suffix (`foo$`), a bare literal (substring), and `^.*$`/`.*` (anything). A
/// pattern with any other metacharacter is not enforced (`is_literal_pattern`
/// says so; `check_schema` warns).
fn is_literal_pattern(p: &str) -> bool {
    let core = p.trim_start_matches('^').trim_end_matches('$');
    core == ".*" || !core.chars().any(|c| ".*+?()[]{}|\\".contains(c))
}

fn literal_pattern_matches(p: &str, s: &str) -> bool {
    if !is_literal_pattern(p) {
        return true; // not enforceable ⇒ permissive
    }
    let anchored_start = p.starts_with('^');
    let anchored_end = p.ends_with('$');
    let core = p.trim_start_matches('^').trim_end_matches('$');
    if core == ".*" {
        return true;
    }
    match (anchored_start, anchored_end) {
        (true, true) => s == core,
        (true, false) => s.starts_with(core),
        (false, true) => s.ends_with(core),
        (false, false) => s.contains(core),
    }
}

fn check_schema_at(schema: &Value, path: &str, errs: &mut Vec<String>, depth: u32) {
    if depth > MAX_DEPTH {
        errs.push(format!("{}: schema nesting exceeds {MAX_DEPTH}", ptr(path)));
        return;
    }
    let s = match schema {
        Value::Bool(_) => return,
        Value::Object(s) => s,
        _ => {
            errs.push(format!(
                "{}: a schema must be an object or a boolean",
                ptr(path)
            ));
            return;
        }
    };
    let sub = |k: &str, v: &Value, errs: &mut Vec<String>| {
        check_schema_at(v, &format!("{path}/{k}"), errs, depth + 1)
    };
    for (k, v) in s {
        match k.as_str() {
            "type" => {
                let ok = match v {
                    Value::String(t) => KNOWN_TYPES.contains(&t.as_str()),
                    Value::Array(a) => a
                        .iter()
                        .all(|t| t.as_str().is_some_and(|t| KNOWN_TYPES.contains(&t))),
                    _ => false,
                };
                if !ok {
                    errs.push(format!(
                        "{}/type: must be a known type name or a list of them",
                        ptr(path)
                    ));
                }
            }
            "properties" | "patternProperties" | "$defs" | "definitions" => match v {
                Value::Object(m) => {
                    for (name, sv) in m {
                        check_schema_at(
                            sv,
                            &format!("{path}/{k}/{}", escape(name)),
                            errs,
                            depth + 1,
                        );
                    }
                }
                _ => errs.push(format!("{}/{k}: must be an object of schemas", ptr(path))),
            },
            "items" | "additionalProperties" | "not" | "if" | "then" | "else" | "contains" => {
                sub(k, v, errs)
            }
            "prefixItems" | "allOf" | "anyOf" | "oneOf" => match v {
                Value::Array(a) => {
                    for (i, sv) in a.iter().enumerate() {
                        check_schema_at(sv, &format!("{path}/{k}/{i}"), errs, depth + 1);
                    }
                }
                _ => errs.push(format!("{}/{k}: must be an array of schemas", ptr(path))),
            },
            "required" => {
                if !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
                    errs.push(format!(
                        "{}/required: must be an array of property names",
                        ptr(path)
                    ));
                }
            }
            "enum" => {
                if !v.is_array() {
                    errs.push(format!("{}/enum: must be an array", ptr(path)));
                }
            }
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" | "multipleOf" => {
                if !v.is_number() {
                    errs.push(format!("{}/{k}: must be a number", ptr(path)));
                }
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
            | "maxProperties" => {
                if v.as_u64().is_none() {
                    errs.push(format!("{}/{k}: must be a non-negative integer", ptr(path)));
                }
            }
            "pattern" => {
                if let Some(p) = v.as_str() {
                    if !is_literal_pattern(p) {
                        errs.push(format!(
                            "{}/pattern: {p:?} uses regex features this validator does not enforce (literal, ^prefix, suffix$, ^exact$ only)",
                            ptr(path)
                        ));
                    }
                } else {
                    errs.push(format!("{}/pattern: must be a string", ptr(path)));
                }
            }
            "$ref" if !v.as_str().is_some_and(|r| r.starts_with('#')) => {
                errs.push(format!(
                    "{}/$ref: only local references (`#/...`) are supported",
                    ptr(path)
                ));
            }
            _ => {}
        }
    }
}

const KNOWN_TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "string", "number", "integer",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(s: &Value, v: &Value) {
        if let Err(e) = validate(s, v) {
            panic!("expected valid, got {e:?}");
        }
    }
    fn bad(s: &Value, v: &Value) -> Vec<String> {
        validate(s, v).expect_err("expected invalid")
    }

    #[test]
    fn types_required_and_additional_properties() {
        let s = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 1},
                "n": {"type": "integer", "minimum": 0},
                "tags": {"type": "array", "items": {"type": "string"}, "uniqueItems": true},
                "mode": {"enum": ["a", "b"]},
                "x": {"type": ["number", "null"]}
            },
            "required": ["name"],
            "additionalProperties": false
        });
        ok(
            &s,
            &json!({"name": "k", "n": 3, "tags": ["a", "b"], "mode": "a", "x": null}),
        );
        ok(&s, &json!({"name": "k", "n": 3.0}));
        let e = bad(
            &s,
            &json!({"n": -1.5, "tags": ["a", "a"], "mode": "z", "extra": 1, "x": "s"}),
        );
        let joined = e.join("\n");
        assert!(
            joined.contains("/: missing required property \"name\""),
            "{joined}"
        );
        assert!(joined.contains("/n: expected type integer"), "{joined}");
        assert!(joined.contains("/n: must be >= 0"), "{joined}");
        assert!(joined.contains("/tags: items must be unique"), "{joined}");
        assert!(
            joined.contains("/mode: value is not one of the allowed values"),
            "{joined}"
        );
        assert!(joined.contains("/: unknown property \"extra\""), "{joined}");
        assert!(
            joined.contains("/x: expected type number|null, got string"),
            "{joined}"
        );
        // Boolean schemas.
        ok(&json!(true), &json!(42));
        assert!(!bad(&json!(false), &json!(42)).is_empty());
        // Non-object with properties keyword is fine (keyword applies to objects only).
        ok(
            &json!({"properties": {"a": {"type": "string"}}}),
            &json!("str"),
        );
    }

    #[test]
    fn combinators_refs_and_conditionals() {
        let s = json!({
            "$defs": {"pos": {"type": "integer", "exclusiveMinimum": 0}},
            "type": "object",
            "properties": {
                "id": {"$ref": "#/$defs/pos"},
                "kind": {"type": "string"},
                "v": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
                "w": {"anyOf": [{"type": "string", "maxLength": 2}, {"type": "boolean"}]},
                "z": {"not": {"const": 0}},
                "self": {"$ref": "#"}
            },
            "if": {"properties": {"kind": {"const": "a"}}, "required": ["kind"]},
            "then": {"required": ["v"]},
            "else": {"required": ["w"]}
        });
        ok(&s, &json!({"id": 1, "kind": "a", "v": "x", "z": 1}));
        ok(
            &s,
            &json!({"id": 2, "kind": "b", "w": true, "self": {"id": 3, "w": "ab"}}),
        );
        let e = bad(
            &s,
            &json!({"id": 0, "kind": "a", "w": "abc", "z": 0, "self": {"id": -1, "kind": "a"}}),
        );
        let joined = e.join("\n");
        assert!(joined.contains("/id: must be > 0"), "{joined}");
        assert!(
            joined.contains("/: missing required property \"v\""),
            "{joined}"
        );
        assert!(joined.contains("/z: must not match"), "{joined}");
        assert!(joined.contains("/self/id: must be > 0"), "{joined}");
        assert!(
            joined.contains("/self: missing required property \"v\""),
            "{joined}"
        );
        let e = bad(&s, &json!({"id": 1, "kind": "b", "w": "abc"}));
        assert!(e.join("\n").contains("/w: matches none of anyOf"), "{e:?}");
        let e = bad(&s, &json!({"id": 1, "kind": "b", "w": "a", "v": 1.5}));
        assert!(
            e.join("\n")
                .contains("/v: must match exactly one of oneOf (matched 0"),
            "{e:?}"
        );
        assert!(bad(&json!({"$ref": "#/$defs/nope"}), &json!(1))[0].contains("unresolvable $ref"));
    }

    #[test]
    fn arrays_strings_numbers_and_patterns() {
        let s = json!({
            "type": "array",
            "prefixItems": [{"type": "string"}, {"type": "number"}],
            "items": {"type": "boolean"},
            "minItems": 2, "maxItems": 4,
            "contains": {"const": true}
        });
        ok(&s, &json!(["a", 1, true]));
        let e = bad(&s, &json!(["a", "b", 1, false, true]));
        let joined = e.join("\n");
        assert!(joined.contains("/1: expected type number"), "{joined}");
        assert!(joined.contains("/2: expected type boolean"), "{joined}");
        assert!(joined.contains("/: at most 4 items"), "{joined}");
        assert!(
            bad(&s, &json!(["a", 1, false]))
                .join("")
                .contains("contains")
        );
        let s = json!({"type": "string", "pattern": "^agentd/", "maxLength": 12});
        ok(&s, &json!("agentd/x"));
        assert!(bad(&s, &json!("other/x"))[0].contains("pattern"));
        assert!(bad(&s, &json!("agentd/toolongvalue"))[0].contains("at most 12"));
        // Complex regex patterns are not enforced (permissive) but flagged by check_schema.
        ok(&json!({"pattern": "^[a-z]+$"}), &json!("123"));
        assert!(
            check_schema(&json!({"pattern": "^[a-z]+$"})).unwrap_err()[0]
                .contains("regex features")
        );
        let s = json!({"type": "number", "multipleOf": 0.5, "maximum": 10, "exclusiveMaximum": 10});
        ok(&s, &json!(9.5));
        let e = bad(&s, &json!(10));
        assert!(e.iter().any(|m| m.contains("must be < 10")), "{e:?}");
        assert!(bad(&s, &json!(9.3))[0].contains("multiple of 0.5"));
        // patternProperties with a literal prefix.
        let s = json!({"type": "object", "patternProperties": {"^x-": {"type": "string"}}, "additionalProperties": false});
        ok(&s, &json!({"x-team": "ops"}));
        assert!(bad(&s, &json!({"x-team": 1}))[0].contains("/x-team: expected type string"));
        assert!(bad(&s, &json!({"team": "ops"}))[0].contains("unknown property"));
        // dependentRequired
        let s = json!({"type": "object", "dependentRequired": {"a": ["b"]}});
        ok(&s, &json!({"a": 1, "b": 2}));
        assert!(bad(&s, &json!({"a": 1}))[0].contains("requires \"b\""));
        // integer accepts 3.0
        ok(&json!({"type": "integer"}), &json!(3.0));
        assert!(!bad(&json!({"type": "integer"}), &json!(3.5)).is_empty());
    }

    #[test]
    fn schema_well_formedness() {
        assert!(check_schema(&json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]})).is_ok());
        let e = check_schema(&json!({"type": "strng", "properties": [], "required": "a", "items": 3, "minLength": -1, "$ref": "http://x"}))
            .unwrap_err();
        let joined = e.join("\n");
        assert!(joined.contains("/type: must be a known type"), "{joined}");
        assert!(
            joined.contains("/properties: must be an object"),
            "{joined}"
        );
        assert!(joined.contains("/required: must be an array"), "{joined}");
        assert!(
            joined.contains("/items: a schema must be an object or a boolean"),
            "{joined}"
        );
        assert!(
            joined.contains("/minLength: must be a non-negative integer"),
            "{joined}"
        );
        assert!(joined.contains("/$ref: only local references"), "{joined}");
        assert!(check_schema(&json!(true)).is_ok());
    }
}
