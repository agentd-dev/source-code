// SPDX-License-Identifier: AGPL-3.0-only
//! Schema-derived **path bindings**: every path in the config-file schema is
//! also settable as an env var and as a generic `--<path>` flag, with names
//! derived mechanically from the path — so a re-defined parameter set needs no
//! per-field plumbing here.
//!
//! For a config path `limits.max_steps`:
//!
//! | source | name                                                    |
//! |--------|---------------------------------------------------------|
//! | file   | `limits: { max_steps: 5 }` (YAML or JSON)               |
//! | env    | `AGENTD_LIMITS_MAX_STEPS` > `AGENT_LIMITS_MAX_STEPS` > `LIMITS_MAX_STEPS` |
//! | flag   | `--limits.max_steps 5` / `--limits.max-steps 5` / `--limits-max-steps 5` |
//!
//! The env candidates are the branded, the neutral (ACC de-branding), and the
//! bare spelling of the upper-cased path with `.` → `_`; the first present wins.
//! A flag is the path with `.`/`_` → `-` (any of the three spellings above
//! canonicalizes to the same flag). Values are typed by the schema's declared
//! type ([`Kind`]): integers/numbers/booleans parse, enums are checked against
//! their allowed set, arrays take a `[a, b]` literal or a comma-separated list,
//! objects take a `{k: v}` / JSON literal — everything else is the verbatim
//! string. The typed [`super::file::ConfigFile`] then re-validates the merged
//! document exactly as it does the file (unknown keys, ranges).
//!
//! A dotted flag may also reach INTO a free-form map (a schema object with
//! `additionalProperties`): `--intelligence_headers.x-team ops` sets ONE key of
//! that map (the key keeps its exact spelling — no canonicalization past the
//! schema path), typed by the map's value type. Array elements are not
//! addressable by path (set the whole list, or use the named repeatable flag).
//!
//! The single source of truth is [`super::file::config_schema`] — the same
//! JSON Schema `--config-schema` prints — walked once at startup.

use super::file::config_schema;
use super::yaml;
use serde_json::{Map, Value};
use std::collections::HashMap;

/// The env-name prefixes tried for every path, most-specific first. Branded
/// (`AGENTD_`), neutral (`AGENT_`, ACC de-branding), then the bare path.
pub const ENV_PREFIXES: [&str; 3] = ["AGENTD_", "AGENT_", ""];

/// The value type a config path takes, per its JSON Schema.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    String,
    Integer,
    Number,
    Boolean,
    /// A closed string set — the allowed values, checked at coercion time.
    Enum(Vec<String>),
    /// A list; the item kind types each comma-separated / literal element.
    Array(Box<Kind>),
    /// A free-form object (a map with `additionalProperties`, or an array item
    /// object): set from a `{…}` literal.
    Object,
    /// Untyped — parsed as an inline YAML/JSON value.
    Any,
}

impl Kind {
    /// The `<TYPE>` hint shown in `--help`.
    pub fn hint(&self) -> String {
        match self {
            Kind::String => "<string>".into(),
            Kind::Integer => "<int>".into(),
            Kind::Number => "<number>".into(),
            Kind::Boolean => "<bool>".into(),
            Kind::Enum(vs) => format!("<{}>", vs.join("|")),
            Kind::Array(k) => format!("<list of {}>", k.hint().trim_matches(['<', '>'])),
            Kind::Object => "<object literal>".into(),
            Kind::Any => "<value>".into(),
        }
    }
}

/// One config-file path with its schema type and (optional) description.
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    /// Dotted path from the document root, e.g. `limits.max_steps`.
    pub path: String,
    pub kind: Kind,
    pub description: Option<String>,
    /// For a free-form map leaf ([`Kind::Object`] with `additionalProperties`):
    /// the type of each entry, so a `--<path>.<key> <value>` flag can type the
    /// single entry it sets. `None` for every other kind.
    pub entry_kind: Option<Kind>,
}

impl Binding {
    /// The env-var names that set this path, most-specific first.
    pub fn env_names(&self) -> Vec<String> {
        let base = self.path.to_ascii_uppercase().replace('.', "_");
        ENV_PREFIXES.iter().map(|p| format!("{p}{base}")).collect()
    }

    /// The canonical generic flag: `--<path>` with `.`/`_` → `-`.
    pub fn flag(&self) -> String {
        format!("--{}", canonical_flag_body(&self.path))
    }

    /// Type a raw string (an env value / a flag value) per this path's kind.
    pub fn coerce(&self, raw: &str) -> Result<Value, String> {
        coerce(&self.kind, raw)
    }
}

/// `limits.max_steps` / `limits-max-steps` / `limits.max-steps` → `limits-max-steps`.
fn canonical_flag_body(s: &str) -> String {
    s.replace(['.', '_'], "-")
}

/// Every path in the (v1) config-file schema, in schema order (nested objects
/// are walked; arrays and free-form maps are leaves).
pub fn bindings() -> Vec<Binding> {
    bindings_of(&config_schema())
}

/// Every path of an arbitrary JSON Schema document (the same walk, for the v2
/// settings schema or any future one).
pub fn bindings_of(schema: &Value) -> Vec<Binding> {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    let mut out = Vec::new();
    walk_object(schema, &defs, "", &mut out);
    out
}

fn walk_object(obj_schema: &Value, defs: &Value, prefix: &str, out: &mut Vec<Binding>) {
    let Some(props) = obj_schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, prop) in props {
        let prop = resolve_ref(prop, defs);
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let description = prop
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        let ty = prop.get("type").and_then(Value::as_str);
        // A nested object WITH declared properties is walked into paths; one
        // without (a free-form map) is a leaf.
        if ty == Some("object") && prop.get("properties").is_some() {
            walk_object(&prop, defs, &path, out);
            continue;
        }
        let kind = kind_of(&prop, defs);
        let entry_kind = match kind {
            Kind::Object => Some(
                prop.get("additionalProperties")
                    .filter(|ap| ap.is_object())
                    .map(|ap| kind_of(&resolve_ref(ap, defs), defs))
                    .unwrap_or(Kind::Any),
            ),
            _ => None,
        };
        out.push(Binding {
            path,
            kind,
            description,
            entry_kind,
        });
    }
}

/// Follow a local `$ref: "#/$defs/Name"`; anything else is returned as-is.
fn resolve_ref(prop: &Value, defs: &Value) -> Value {
    if let Some(r) = prop.get("$ref").and_then(Value::as_str)
        && let Some(name) = r.strip_prefix("#/$defs/")
        && let Some(def) = defs.get(name)
    {
        return def.clone();
    }
    prop.clone()
}

fn kind_of(prop: &Value, defs: &Value) -> Kind {
    if let Some(vals) = prop.get("enum").and_then(Value::as_array) {
        return Kind::Enum(
            vals.iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        );
    }
    match prop.get("type").and_then(Value::as_str) {
        Some("string") => Kind::String,
        Some("integer") => Kind::Integer,
        Some("number") => Kind::Number,
        Some("boolean") => Kind::Boolean,
        Some("object") => Kind::Object,
        Some("array") => {
            let item = prop
                .get("items")
                .map(|i| kind_of(&resolve_ref(i, defs), defs))
                .unwrap_or(Kind::Any);
            Kind::Array(Box::new(item))
        }
        _ => Kind::Any,
    }
}

/// Type a raw string per `kind` (see the module docs for the rules).
pub fn coerce(kind: &Kind, raw: &str) -> Result<Value, String> {
    match kind {
        Kind::String => Ok(Value::String(raw.to_string())),
        Kind::Enum(allowed) => {
            let t = raw.trim();
            if allowed.iter().any(|a| a == t) {
                Ok(Value::String(t.to_string()))
            } else {
                Err(format!("{t:?} is not one of {}", allowed.join("|")))
            }
        }
        Kind::Integer => {
            let t = raw.trim();
            if let Ok(i) = t.parse::<i64>() {
                return Ok(Value::from(i));
            }
            if let Ok(u) = t.parse::<u64>() {
                return Ok(Value::from(u));
            }
            Err(format!("expected an integer, got {t:?}"))
        }
        Kind::Number => {
            let t = raw.trim();
            match t.parse::<f64>() {
                Ok(f) if f.is_finite() => serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .ok_or_else(|| format!("expected a number, got {t:?}")),
                _ => Err(format!("expected a number, got {t:?}")),
            }
        }
        Kind::Boolean => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Value::Bool(true)),
            "0" | "false" | "no" | "off" => Ok(Value::Bool(false)),
            other => Err(format!("expected a boolean (true|false), got {other:?}")),
        },
        Kind::Array(item) => {
            let t = raw.trim();
            if t.is_empty() {
                return Ok(Value::Array(Vec::new()));
            }
            if t.starts_with('[') {
                return match yaml::parse_inline(t) {
                    Ok(Value::Array(a)) => Ok(Value::Array(a)),
                    Ok(_) => Err("expected a list literal".into()),
                    Err(e) => Err(format!("bad list literal: {e}")),
                };
            }
            // Comma-separated items, each typed by the item kind. Object items
            // must use the literal form.
            if matches!(**item, Kind::Object) {
                return Err("expected a `[{...}, ...]` list literal".into());
            }
            t.split(',')
                .map(|s| coerce(item, s.trim()))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        Kind::Object => {
            let t = raw.trim();
            if !t.starts_with('{') {
                return Err("expected a `{key: value, ...}` object literal".into());
            }
            match yaml::parse_inline(t) {
                Ok(Value::Object(o)) => Ok(Value::Object(o)),
                Ok(_) => Err("expected an object literal".into()),
                Err(e) => Err(format!("bad object literal: {e}")),
            }
        }
        Kind::Any => yaml::parse_inline(raw).map_err(|e| format!("bad value: {e}")),
    }
}

/// Set `value` at the dotted `path` inside `root`, creating intermediate
/// objects (a non-object in the way is replaced).
pub fn set_path(root: &mut Value, path: &str, value: Value) {
    let mut cur = root;
    let segs: Vec<&str> = path.split('.').collect();
    for (i, seg) in segs.iter().enumerate() {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let map = cur.as_object_mut().expect("just ensured an object");
        if i + 1 == segs.len() {
            map.insert((*seg).to_string(), value);
            return;
        }
        cur = map
            .entry((*seg).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

/// The env layer as a config DOCUMENT: for every schema path, the first present
/// env candidate (`AGENTD_…` > `AGENT_…` > bare) is coerced and set at its
/// path. Returns the document (an empty object when nothing is set) plus the
/// `(env name, path)` pairs that were applied. An untypeable value is an
/// error naming the variable.
pub fn env_document(env: &HashMap<&str, &str>) -> Result<(Value, Vec<(String, String)>), String> {
    env_document_in(&bindings(), env)
}

/// [`env_document`] over a given binding set (a schema other than v1's).
pub fn env_document_in(
    bindings: &[Binding],
    env: &HashMap<&str, &str>,
) -> Result<(Value, Vec<(String, String)>), String> {
    let mut doc = Value::Object(Map::new());
    let mut applied = Vec::new();
    for b in bindings {
        for name in b.env_names() {
            if let Some(raw) = env.get(name.as_str()) {
                let v = b.coerce(raw).map_err(|e| format!("invalid {name}: {e}"))?;
                set_path(&mut doc, &b.path, v);
                applied.push((name, b.path.clone()));
                break;
            }
        }
    }
    Ok((doc, applied))
}

/// A resolved `--<path>[.<key>]` flag: the schema binding it addresses and, when
/// the flag reaches into a free-form map, the entry key (exact spelling).
#[derive(Debug, Clone, PartialEq)]
pub struct FlagTarget {
    pub binding: Binding,
    /// `Some(key)` for `--intelligence_headers.x-team` (key = `x-team`); `None`
    /// when the flag names the schema path itself.
    pub entry: Option<String>,
}

impl FlagTarget {
    /// The kind the flag's VALUE is typed by: the map's entry type when an
    /// entry is addressed, else the path's own type.
    pub fn value_kind(&self) -> &Kind {
        match (&self.entry, &self.binding.entry_kind) {
            (Some(_), Some(k)) => k,
            _ => &self.binding.kind,
        }
    }

    /// The document `{…: value}` this flag sets: the value at the schema path,
    /// or — for a map entry — `{path: {key: value}}` (the key is one map key,
    /// dots and all).
    pub fn document(&self, value: Value) -> Value {
        let mut doc = Value::Object(Map::new());
        match &self.entry {
            Some(key) => {
                let mut entry = Map::new();
                entry.insert(key.clone(), value);
                set_path(&mut doc, &self.binding.path, Value::Object(entry));
            }
            None => set_path(&mut doc, &self.binding.path, value),
        }
        doc
    }
}

/// Resolve a `--flag` (with or without the leading dashes) to the schema path it
/// addresses — canonicalizing `.`/`_`/`-` — or, for a dotted flag whose longest
/// schema-path prefix is a free-form map, to that map plus the remaining
/// segments as ONE entry key with its exact spelling (`--intelligence_headers.x-team`
/// ⇒ path `intelligence_headers`, key `x-team`). `Ok(None)` when it is not a
/// config path at all (the caller reports an unknown argument); `Err` when it
/// names a config path but reaches into something that is not a map (an array
/// element, a scalar).
pub fn resolve_flag(arg: &str) -> Result<Option<FlagTarget>, String> {
    resolve_flag_in(&bindings(), arg)
}

/// [`resolve_flag`] over a given binding set.
pub fn resolve_flag_in(all: &[Binding], arg: &str) -> Result<Option<FlagTarget>, String> {
    let body = arg.strip_prefix("--").unwrap_or(arg);
    if body.is_empty() {
        return Ok(None);
    }
    let segments: Vec<&str> = body.split('.').collect();
    // Longest schema-path prefix first (whole flag, then one segment fewer…).
    for k in (1..=segments.len()).rev() {
        let prefix = segments[..k].join(".");
        let want = canonical_flag_body(&prefix);
        let Some(binding) = all.iter().find(|b| canonical_flag_body(&b.path) == want) else {
            continue;
        };
        if k == segments.len() {
            return Ok(Some(FlagTarget {
                binding: binding.clone(),
                entry: None,
            }));
        }
        let rest = segments[k..].join(".");
        return match binding.kind {
            Kind::Object => Ok(Some(FlagTarget {
                binding: binding.clone(),
                entry: Some(rest),
            })),
            Kind::Array(_) => Err(format!(
                "{arg}: array elements cannot be addressed by path (set the whole list `--{} '[…]'`, or use the named repeatable flag)",
                canonical_flag_body(&binding.path)
            )),
            _ => Err(format!(
                "{arg}: `{}` is a {} value, not an object — nothing to set at `.{rest}`",
                binding.path,
                binding.kind.hint().trim_matches(['<', '>'])
            )),
        };
    }
    Ok(None)
}

/// The `--help` section listing every config path with its flag and env name.
pub fn help_section() -> String {
    help_section_in(&bindings())
}

/// [`help_section`] over a given binding set.
pub fn help_section_in(bindings: &[Binding]) -> String {
    let mut out = String::from(
        "CONFIG PATHS (every config-file path is also a flag and an env var; \
         env: AGENTD_<PATH> > AGENT_<PATH> > <PATH>; a named flag above with the \
         same spelling keeps its own semantics):\n",
    );
    for b in bindings {
        let flag = format!("{} {}", b.flag(), b.kind.hint());
        out.push_str(&format!(
            "  {:<26} {:<44} {}\n",
            b.path,
            flag,
            b.env_names()[0]
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths() -> Vec<String> {
        bindings().into_iter().map(|b| b.path).collect()
    }

    #[test]
    fn bindings_walk_the_schema_into_dotted_paths() {
        let p = paths();
        // Top-level scalars, a nested object (walked), lists + maps (leaves).
        for want in [
            "config_version",
            "intelligence",
            "model_swap",
            "model",
            "max_tokens",
            "limits.max_steps",
            "limits.max_depth",
            "limits.deadline_secs",
            "limits.lifetime_tokens",
            "mcp_servers",
            "subscribe",
            "a2a_peers",
            "log_level",
            "intelligence_headers",
        ] {
            assert!(p.contains(&want.to_string()), "missing path {want}: {p:?}");
        }
        assert!(
            !p.contains(&"limits".to_string()),
            "walked objects are not leaves"
        );
        // Kinds follow the schema.
        let by: HashMap<String, Kind> = bindings().into_iter().map(|b| (b.path, b.kind)).collect();
        assert_eq!(by["model"], Kind::String);
        assert_eq!(by["max_tokens"], Kind::Integer);
        assert_eq!(by["limits.max_steps"], Kind::Integer);
        assert_eq!(by["subscribe"], Kind::Array(Box::new(Kind::String)));
        assert_eq!(by["mcp_servers"], Kind::Array(Box::new(Kind::Object)));
        assert_eq!(by["intelligence_headers"], Kind::Object);
        assert!(matches!(&by["log_level"], Kind::Enum(v) if v.contains(&"info".to_string())));
        assert!(matches!(&by["model_swap"], Kind::Enum(v) if v.len() == 2));
    }

    #[test]
    fn env_and_flag_names_derive_from_the_path() {
        let b = bindings()
            .into_iter()
            .find(|b| b.path == "limits.max_steps")
            .unwrap();
        assert_eq!(
            b.env_names(),
            vec![
                "AGENTD_LIMITS_MAX_STEPS".to_string(),
                "AGENT_LIMITS_MAX_STEPS".to_string(),
                "LIMITS_MAX_STEPS".to_string()
            ]
        );
        assert_eq!(b.flag(), "--limits-max-steps");
        // Every spelling resolves to the same binding.
        for spelling in [
            "--limits.max_steps",
            "--limits.max-steps",
            "--limits-max-steps",
            "--limits_max_steps",
            "limits.max_steps",
        ] {
            let t = resolve_flag(spelling).unwrap().expect(spelling);
            assert_eq!(t.binding.path, "limits.max_steps", "{spelling}");
            assert!(t.entry.is_none());
        }
        assert!(resolve_flag("--no-such-path").unwrap().is_none());
        assert!(resolve_flag("--").unwrap().is_none());
        // A nested object is not itself addressable (only its leaves are).
        assert!(resolve_flag("--limits").unwrap().is_none());
    }

    #[test]
    fn dotted_flags_reach_into_free_form_maps_with_exact_keys() {
        // `intelligence_headers` is a map: a dotted flag past it names ONE entry,
        // spelling preserved (dashes/underscores/dots inside the key are data).
        let t = resolve_flag("--intelligence_headers.x-team")
            .unwrap()
            .unwrap();
        assert_eq!(t.binding.path, "intelligence_headers");
        assert_eq!(t.entry.as_deref(), Some("x-team"));
        assert_eq!(
            *t.value_kind(),
            Kind::String,
            "typed by additionalProperties"
        );
        assert_eq!(
            t.document(json!("ops")),
            json!({"intelligence_headers": {"x-team": "ops"}})
        );
        // The schema-path part still canonicalizes; the key never does.
        let t = resolve_flag("--intelligence-headers.Anthropic_Version.v2")
            .unwrap()
            .unwrap();
        assert_eq!(t.entry.as_deref(), Some("Anthropic_Version.v2"));
        // The whole-map form has no entry.
        let t = resolve_flag("--intelligence-headers").unwrap().unwrap();
        assert!(t.entry.is_none());
        assert_eq!(*t.value_kind(), Kind::Object);
        // Reaching into a list or a scalar is a clear error, not a guess.
        let e = resolve_flag("--mcp-servers.0.aauth").unwrap_err();
        assert!(e.contains("array elements"), "{e}");
        let e = resolve_flag("--model.sub").unwrap_err();
        assert!(e.contains("not an object"), "{e}");
    }

    #[test]
    fn derived_names_are_unique_across_the_schema() {
        // Two paths canonicalizing to the same flag/env would be ambiguous —
        // guard the schema against it.
        let bs = bindings();
        let mut flags = std::collections::HashSet::new();
        let mut envs = std::collections::HashSet::new();
        for b in &bs {
            assert!(flags.insert(b.flag()), "duplicate flag {}", b.flag());
            assert!(
                envs.insert(b.env_names()[0].clone()),
                "duplicate env {}",
                b.env_names()[0]
            );
        }
    }

    #[test]
    fn coercion_types_by_kind() {
        assert_eq!(coerce(&Kind::String, " x ").unwrap(), json!(" x "));
        assert_eq!(coerce(&Kind::Integer, "42").unwrap(), json!(42));
        assert_eq!(coerce(&Kind::Integer, "-1").unwrap(), json!(-1));
        assert!(coerce(&Kind::Integer, "4.2").is_err());
        assert!(coerce(&Kind::Integer, "abc").is_err());
        assert_eq!(coerce(&Kind::Number, "1.5").unwrap(), json!(1.5));
        assert!(coerce(&Kind::Number, "nan").is_err());
        assert_eq!(coerce(&Kind::Boolean, "on").unwrap(), json!(true));
        assert_eq!(coerce(&Kind::Boolean, "False").unwrap(), json!(false));
        assert!(coerce(&Kind::Boolean, "maybe").is_err());
        let en = Kind::Enum(vec!["a".into(), "b".into()]);
        assert_eq!(coerce(&en, "b").unwrap(), json!("b"));
        let e = coerce(&en, "c").unwrap_err();
        assert!(e.contains("a|b"), "{e}");
        let strs = Kind::Array(Box::new(Kind::String));
        assert_eq!(coerce(&strs, "a, b ,c").unwrap(), json!(["a", "b", "c"]));
        assert_eq!(coerce(&strs, "[x, \"y z\"]").unwrap(), json!(["x", "y z"]));
        assert_eq!(coerce(&strs, "").unwrap(), json!([]));
        let ints = Kind::Array(Box::new(Kind::Integer));
        assert_eq!(coerce(&ints, "1,2").unwrap(), json!([1, 2]));
        assert!(coerce(&ints, "1,x").is_err());
        let objs = Kind::Array(Box::new(Kind::Object));
        assert_eq!(
            coerce(&objs, r#"[{name: a, endpoint: "https://x"}]"#).unwrap(),
            json!([{"name": "a", "endpoint": "https://x"}])
        );
        assert!(coerce(&objs, "a,b").is_err());
        assert_eq!(
            coerce(&Kind::Object, "{k: v, n: 1}").unwrap(),
            json!({"k": "v", "n": 1})
        );
        assert!(coerce(&Kind::Object, "not-an-object").is_err());
        assert_eq!(coerce(&Kind::Any, "[1, two]").unwrap(), json!([1, "two"]));
    }

    #[test]
    fn set_path_builds_nested_objects() {
        let mut doc = Value::Object(Map::new());
        set_path(&mut doc, "limits.max_steps", json!(5));
        set_path(&mut doc, "limits.max_depth", json!(2));
        set_path(&mut doc, "model", json!("m"));
        assert_eq!(
            doc,
            json!({"limits": {"max_steps": 5, "max_depth": 2}, "model": "m"})
        );
        // A scalar in the way of a nested path is replaced.
        set_path(&mut doc, "model.sub", json!(1));
        assert_eq!(doc["model"], json!({"sub": 1}));
    }

    #[test]
    fn env_document_prefers_branded_then_neutral_then_bare() {
        let mut env: HashMap<&str, &str> = HashMap::new();
        env.insert("LIMITS_MAX_STEPS", "1");
        env.insert("AGENT_LIMITS_MAX_STEPS", "2");
        env.insert("AGENTD_LIMITS_MAX_STEPS", "3");
        env.insert("MODEL", "bare-model");
        env.insert("AGENTD_SUBSCRIBE", "a,b");
        env.insert("UNRELATED", "x");
        let (doc, applied) = env_document(&env).unwrap();
        assert_eq!(doc["limits"]["max_steps"], json!(3));
        assert_eq!(doc["model"], json!("bare-model"));
        assert_eq!(doc["subscribe"], json!(["a", "b"]));
        assert!(
            applied
                .iter()
                .any(|(n, p)| n == "AGENTD_LIMITS_MAX_STEPS" && p == "limits.max_steps")
        );
        assert!(applied.iter().any(|(n, _)| n == "MODEL"));
        assert!(!applied.iter().any(|(n, _)| n == "UNRELATED"));
        // A bad value names the variable.
        env.insert("AGENTD_MAX_TOKENS", "lots");
        let e = env_document(&env).unwrap_err();
        assert!(e.contains("AGENTD_MAX_TOKENS"), "{e}");
    }

    #[test]
    fn every_binding_deserializes_into_the_typed_config_file() {
        // The schema (bindings) and the typed struct must agree at every path:
        // a sample value per kind, set at the path, must deserialize.
        for b in bindings() {
            let sample = match &b.kind {
                Kind::String => json!("x"),
                Kind::Integer => json!(1),
                Kind::Number => json!(1.5),
                Kind::Boolean => json!(true),
                Kind::Enum(vs) => json!(vs[0]),
                Kind::Array(item) => match **item {
                    Kind::Object if b.path == "mcp_servers" => {
                        json!([{"name": "a", "endpoint": "https://a.example/mcp"}])
                    }
                    Kind::Object if b.path == "a2a_peers" => {
                        json!([{"name": "p", "endpoint": "https://p.example"}])
                    }
                    Kind::Object => json!([{}]),
                    _ => json!(["s"]),
                },
                Kind::Object => json!({"k": "v"}),
                Kind::Any => json!(null),
            };
            let mut doc = Value::Object(Map::new());
            set_path(&mut doc, &b.path, sample);
            super::super::file::ConfigFile::from_document(doc, "test")
                .unwrap_or_else(|e| panic!("path {} does not deserialize: {e}", b.path));
        }
    }

    #[test]
    fn help_section_lists_every_path() {
        let h = help_section();
        for b in bindings() {
            assert!(h.contains(&b.path), "help lacks {}", b.path);
            assert!(h.contains(&b.flag()), "help lacks {}", b.flag());
            assert!(
                h.contains(&b.env_names()[0]),
                "help lacks {}",
                b.env_names()[0]
            );
        }
    }
}
