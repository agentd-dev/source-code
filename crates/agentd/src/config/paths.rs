// SPDX-License-Identifier: AGPL-3.0-only
//! Schema-derived **path bindings**: every path in the config-file schema is
//! also settable as an env var and as a generic `--<path>` flag, with names
//! derived mechanically from the path — so a re-defined parameter set needs no
//! per-field plumbing here.
//!
//! For a config path `limits.run.steps`:
//!
//! | source | name                                                    |
//! |--------|---------------------------------------------------------|
//! | file   | `limits: { run: { steps: 5 } }` (YAML or JSON)          |
//! | env    | `AGENTD_LIMITS_RUN_STEPS`                               |
//! | flag   | `--limits.run.steps 5` / `--limits.run-steps 5` / `--limits-run-steps 5` |
//!
//! The env name is `AGENTD_` and the upper-cased path with `.` → `_` — one
//! spelling, so a variable either configures agentd or is not agentd's.
//! A flag is the path with `.`/`_` → `-` (any of the three spellings above
//! canonicalizes to the same flag). Values are typed by the schema's declared
//! type ([`Kind`]): integers/numbers/booleans parse, enums are checked against
//! their allowed set, arrays take a `[a, b]` literal or a comma-separated list,
//! objects take a `{k: v}` / JSON literal — everything else is the verbatim
//! string. The typed [`super::settings::Settings`] then re-validates the merged
//! document exactly as it does the file (unknown keys, ranges).
//!
//! A dotted flag may also reach INTO a free-form map (a schema object with
//! `additionalProperties`): `--intelligence.headers.x-team ops` sets ONE key of
//! that map (the key keeps its exact spelling — no canonicalization past the
//! schema path), typed by the map's value type. Array elements are not
//! addressable by path (set the whole list, or use the named repeatable flag).
//!
//! The single source of truth is [`super::settings::schema::schema`] — the same JSON
//! Schema `--config-schema` prints — walked once at startup by [`bindings_of`].

use super::yaml;
use serde_json::{Map, Value};
use std::collections::HashMap;

/// The prefix of every environment variable the config loader reads. The only
/// one: a second spelling is a variable some other program may already set.
pub const ENV_PREFIX: &str = "AGENTD_";

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
    /// Dotted path from the document root, e.g. `limits.run.steps`.
    pub path: String,
    pub kind: Kind,
    pub description: Option<String>,
    /// For a free-form map leaf ([`Kind::Object`] with `additionalProperties`):
    /// the type of each entry, so a `--<path>.<key> <value>` flag can type the
    /// single entry it sets. `None` for every other kind.
    pub entry_kind: Option<Kind>,
}

impl Binding {
    /// The env-var name that sets this path.
    pub fn env_name(&self) -> String {
        let base = self.path.to_ascii_uppercase().replace('.', "_");
        format!("{ENV_PREFIX}{base}")
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

/// `limits.run.steps` / `limits-run-steps` / `limits.run-steps` → `limits-run-steps`.
fn canonical_flag_body(s: &str) -> String {
    s.replace(['.', '_'], "-")
}

/// Every path of a JSON Schema document, in schema order (nested objects are
/// walked; arrays and free-form maps are leaves).
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
        // A `oneOf` whose branches are "a scalar shorthand" and "the full
        // object" is walked through the OBJECT branch, so the dotted flags of
        // the long form resolve while the short form stays a plain value.
        // `agent.instruction` is the case this exists for: `--instruction
        // oci://…` and `--instruction.refresh 60s` are the same setting.
        if let Some(branches) = prop.get("oneOf").and_then(Value::as_array)
            && let Some(obj) = branches.iter().find(|b| {
                b.get("type").and_then(Value::as_str) == Some("object")
                    && b.get("properties").is_some()
            })
        {
            walk_object(obj, defs, &path, out);
            // The scalar branch keeps the bare flag usable, with exactly the
            // kind it had before this branch existed — `oneOf` without a
            // `type` resolves to `Any`, and a caller that samples by kind
            // (the schema-path test) depends on that not shifting.
            out.push(Binding {
                path: path.clone(),
                kind: kind_of(&prop, defs),
                entry_kind: None,
                description: description.clone(),
            });
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

/// The env layer as a config DOCUMENT: for every schema path whose variable is
/// set, the value is coerced and set at its path. Returns the document (an
/// empty object when nothing is set) plus the `(env name, path)` pairs that
/// were applied. An untypeable value is an error naming the variable.
pub fn env_document(
    bindings: &[Binding],
    env: &HashMap<&str, &str>,
) -> Result<(Value, Vec<(String, String)>), String> {
    let mut doc = Value::Object(Map::new());
    let mut applied = Vec::new();
    for b in bindings {
        let name = b.env_name();
        if let Some(raw) = env.get(name.as_str()) {
            let v = b.coerce(raw).map_err(|e| format!("invalid {name}: {e}"))?;
            set_path(&mut doc, &b.path, v);
            applied.push((name, b.path.clone()));
        }
    }
    Ok((doc, applied))
}

/// A resolved `--<path>[.<key>]` flag: the schema binding it addresses and, when
/// the flag reaches into a free-form map, the entry key (exact spelling).
#[derive(Debug, Clone, PartialEq)]
pub struct FlagTarget {
    pub binding: Binding,
    /// `Some(key)` for `--intelligence.headers.x-team` (key = `x-team`); `None`
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

/// The part of a command-line argument that names it: everything before the
/// first `=`. A refusal echoes this and never the whole argument, because an
/// argument typed as `--name=value` carries its value — often a credential —
/// and refusals land in stderr, the journal and pod logs.
pub fn flag_name(arg: &str) -> &str {
    arg.split_once('=').map_or(arg, |(name, _)| name)
}

/// Resolve a `--flag` (with or without the leading dashes) to the schema path it
/// addresses — canonicalizing `.`/`_`/`-` — or, for a dotted flag whose longest
/// schema-path prefix is a free-form map, to that map plus the remaining
/// segments as ONE entry key with its exact spelling (`--intelligence.headers.x-team`
/// ⇒ path `intelligence.headers`, key `x-team`). `Ok(None)` when it is not a
/// config path at all (the caller reports an unknown argument); `Err` when it
/// names a config path but reaches into something that is not a map (an array
/// element, a scalar).
pub fn resolve_flag(all: &[Binding], arg: &str) -> Result<Option<FlagTarget>, String> {
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
                "{}: array elements cannot be addressed by path (set the whole list `--{} '[…]'`, or use the named repeatable flag)",
                flag_name(arg),
                canonical_flag_body(&binding.path)
            )),
            _ => Err(format!(
                "{}: `{}` is a {} value, not an object — nothing to set at `.{}`",
                flag_name(arg),
                binding.path,
                binding.kind.hint().trim_matches(['<', '>']),
                flag_name(&rest)
            )),
        };
    }
    Ok(None)
}

/// The `--help` section listing every config path with its flag and env name.
pub fn help_section(bindings: &[Binding]) -> String {
    let mut out = String::from(
        "CONFIG PATHS (every config-file path is also a flag and an env var; \
         env: AGENTD_<PATH>; a named flag above with the \
         same spelling keeps its own semantics):\n",
    );
    for b in bindings {
        let flag = format!("{} {}", b.flag(), b.kind.hint());
        out.push_str(&format!("  {:<26} {:<44} {}\n", b.path, flag, b.env_name()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A small schema with one of every shape the walk distinguishes, so these
    /// tests pin the mechanics rather than whatever the settings schema holds
    /// today (the settings tests walk that one).
    fn fixture() -> Value {
        json!({
            "type": "object",
            "properties": {
                "model": {"type": "string"},
                "max_tokens": {"type": "integer"},
                "log_level": {"enum": ["error", "warn", "info", "debug"]},
                "swap": {"enum": ["next-turn", "restart-turn"]},
                "limits": {"type": "object", "properties": {
                    "max_steps": {"type": "integer"},
                    "max_depth": {"type": "integer"}
                }},
                "subscribe": {"type": "array", "items": {"type": "string"}},
                "servers": {"type": "array", "items": {"$ref": "#/$defs/Server"}},
                "headers": {"type": "object", "additionalProperties": {"type": "string"}}
            },
            "$defs": {"Server": {"type": "object", "properties": {"name": {"type": "string"}}}}
        })
    }

    fn bindings() -> Vec<Binding> {
        bindings_of(&fixture())
    }

    fn flag(arg: &str) -> Result<Option<FlagTarget>, String> {
        resolve_flag(&bindings(), arg)
    }

    fn paths() -> Vec<String> {
        bindings().into_iter().map(|b| b.path).collect()
    }

    #[test]
    fn bindings_walk_the_schema_into_dotted_paths() {
        let p = paths();
        // Top-level scalars, a nested object (walked), lists + maps (leaves).
        for want in [
            "model",
            "max_tokens",
            "log_level",
            "swap",
            "limits.max_steps",
            "limits.max_depth",
            "subscribe",
            "servers",
            "headers",
        ] {
            assert!(p.contains(&want.to_string()), "missing path {want}: {p:?}");
        }
        assert!(
            !p.contains(&"limits".to_string()),
            "walked objects are not leaves"
        );
        // Kinds follow the schema, through a `$ref`.
        let by: HashMap<String, Kind> = bindings().into_iter().map(|b| (b.path, b.kind)).collect();
        assert_eq!(by["model"], Kind::String);
        assert_eq!(by["max_tokens"], Kind::Integer);
        assert_eq!(by["limits.max_steps"], Kind::Integer);
        assert_eq!(by["subscribe"], Kind::Array(Box::new(Kind::String)));
        assert_eq!(by["servers"], Kind::Array(Box::new(Kind::Object)));
        assert_eq!(by["headers"], Kind::Object);
        assert!(matches!(&by["log_level"], Kind::Enum(v) if v.contains(&"info".to_string())));
        assert!(matches!(&by["swap"], Kind::Enum(v) if v.len() == 2));
    }

    #[test]
    fn env_and_flag_names_derive_from_the_path() {
        let b = bindings()
            .into_iter()
            .find(|b| b.path == "limits.max_steps")
            .unwrap();
        assert_eq!(b.env_name(), "AGENTD_LIMITS_MAX_STEPS");
        assert_eq!(b.flag(), "--limits-max-steps");
        // Every spelling resolves to the same binding.
        for spelling in [
            "--limits.max_steps",
            "--limits.max-steps",
            "--limits-max-steps",
            "--limits_max_steps",
            "limits.max_steps",
        ] {
            let t = flag(spelling).unwrap().expect(spelling);
            assert_eq!(t.binding.path, "limits.max_steps", "{spelling}");
            assert!(t.entry.is_none());
        }
        assert!(flag("--no-such-path").unwrap().is_none());
        assert!(flag("--").unwrap().is_none());
        // A nested object is not itself addressable (only its leaves are).
        assert!(flag("--limits").unwrap().is_none());
    }

    #[test]
    fn dotted_flags_reach_into_free_form_maps_with_exact_keys() {
        // `headers` is a map: a dotted flag past it names ONE entry, spelling
        // preserved (dashes/underscores/dots inside the key are data).
        let t = flag("--headers.x-team").unwrap().unwrap();
        assert_eq!(t.binding.path, "headers");
        assert_eq!(t.entry.as_deref(), Some("x-team"));
        assert_eq!(
            *t.value_kind(),
            Kind::String,
            "typed by additionalProperties"
        );
        assert_eq!(
            t.document(json!("ops")),
            json!({"headers": {"x-team": "ops"}})
        );
        // The key never canonicalizes.
        let t = flag("--headers.Anthropic_Version.next").unwrap().unwrap();
        assert_eq!(t.entry.as_deref(), Some("Anthropic_Version.next"));
        // The whole-map form has no entry.
        let t = flag("--headers").unwrap().unwrap();
        assert!(t.entry.is_none());
        assert_eq!(*t.value_kind(), Kind::Object);
        // Reaching into a list or a scalar is a clear error, not a guess.
        let e = flag("--servers.0.name").unwrap_err();
        assert!(e.contains("array elements"), "{e}");
        let e = flag("--model.sub").unwrap_err();
        assert!(e.contains("not an object"), "{e}");
    }

    #[test]
    fn derived_names_are_unique_across_the_schema() {
        // Two paths canonicalizing to the same flag/env would be ambiguous —
        // guard the schema against it.
        let mut flags = std::collections::HashSet::new();
        let mut envs = std::collections::HashSet::new();
        for b in bindings() {
            assert!(flags.insert(b.flag()), "duplicate flag {}", b.flag());
            assert!(envs.insert(b.env_name()), "duplicate env {}", b.env_name());
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

    /// Only the `AGENTD_` spelling is read: the same path under `AGENT_` or
    /// bare is some other program's variable, and binds nothing.
    #[test]
    fn env_document_reads_only_the_agentd_prefix() {
        let mut env: HashMap<&str, &str> = HashMap::new();
        env.insert("LIMITS_MAX_STEPS", "1");
        env.insert("AGENT_LIMITS_MAX_STEPS", "2");
        env.insert("AGENTD_LIMITS_MAX_STEPS", "3");
        env.insert("MODEL", "bare-model");
        env.insert("AGENT_MAX_TOKENS", "5");
        env.insert("AGENTD_SUBSCRIBE", "a,b");
        env.insert("UNRELATED", "x");
        let (doc, applied) = env_document(&bindings(), &env).unwrap();
        assert_eq!(doc["limits"]["max_steps"], json!(3));
        assert_eq!(doc["subscribe"], json!(["a", "b"]));
        assert!(doc.get("model").is_none(), "a bare name binds nothing");
        assert!(doc.get("max_tokens").is_none(), "AGENT_ binds nothing");
        assert_eq!(
            applied,
            vec![
                (
                    "AGENTD_LIMITS_MAX_STEPS".to_string(),
                    "limits.max_steps".to_string()
                ),
                ("AGENTD_SUBSCRIBE".to_string(), "subscribe".to_string()),
            ]
        );
        // A bad value names the variable.
        env.insert("AGENTD_MAX_TOKENS", "lots");
        let e = env_document(&bindings(), &env).unwrap_err();
        assert!(e.contains("AGENTD_MAX_TOKENS"), "{e}");
    }

    #[test]
    fn help_section_lists_every_path() {
        let h = help_section(&bindings());
        for b in bindings() {
            assert!(h.contains(&b.path), "help lacks {}", b.path);
            assert!(h.contains(&b.flag()), "help lacks {}", b.flag());
            assert!(h.contains(&b.env_name()), "help lacks {}", b.env_name());
        }
    }
}
