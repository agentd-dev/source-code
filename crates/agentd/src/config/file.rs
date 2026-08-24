// SPDX-License-Identifier: AGPL-3.0-only
//! The declarative config **file** + its JSON Schema.
//!
//! One document, two syntaxes: **YAML** (`.yaml`/`.yml`, read by the
//! hand-rolled [`super::yaml`] subset reader — no `serde_yaml`, the minimalism
//! moat) or **JSON** with comments (`.json`/`.jsonc`); an unknown extension is
//! sniffed (`{`/`[` ⇒ JSON, else YAML). Both parse to the same
//! `serde_json::Value` document ([`read_document`]) and then to the typed
//! [`ConfigFile`] ([`ConfigFile::from_document`]) — so validation, the schema,
//! the env/flag path bindings ([`super::paths`]) and hot reload are all
//! format-agnostic.
//!
//! The file carries **only verbose structural config**: the MCP-server
//! inventory, declared subscriptions, A2A peers, limits, and the model/log
//! knobs. It **never** carries secrets or per-environment scalars (those stay
//! env/flag).
//!
//! Precedence: `built-in default < FILE < env < flag`. The file is loaded
//! first, then `Config::load` applies env and flags over it; a flag/env for the
//! same key wins. List-valued keys (`mcp_servers`, `subscribe`, `a2a_peers`)
//! *seed* the list — repeatable `--mcp`/`--subscribe`/`--a2a-peer` flags **add
//! to** the file's list rather than replacing it, matching the repeatable-flag
//! semantics operators already expect.
//!
//! `deny_unknown_fields` makes a typo'd key (`max_token` vs `max_tokens`) a hard
//! config error (exit 2) instead of a silently-ignored value — the single most
//! common config footgun, closed at parse time.
//!
//! The schema is **hand-written** (no `schemars` — a forbidden dependency) and
//! kept faithful to this struct by a unit test asserting the schema's top-level
//! properties match the struct's fields, so the two cannot diverge unnoticed.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// The two config-file syntaxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// JSON, with `//` and `/* */` comments tolerated (jsonc).
    Json,
    /// The YAML subset [`super::yaml`] reads.
    Yaml,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Yaml => "yaml",
        }
    }

    /// Decide the format of a config document: the file extension when it is a
    /// known one (`.yaml`/`.yml` ⇒ YAML; `.json`/`.jsonc` ⇒ JSON), else by
    /// sniffing the text — a document whose first significant character (after
    /// whitespace and `//`/`/* */` comments) is `{` or `[` is JSON, anything
    /// else is YAML.
    pub fn detect(path: Option<&Path>, text: &str) -> Format {
        if let Some(ext) = path.and_then(|p| p.extension()).and_then(|e| e.to_str()) {
            match ext.to_ascii_lowercase().as_str() {
                "yaml" | "yml" => return Format::Yaml,
                "json" | "jsonc" => return Format::Json,
                _ => {}
            }
        }
        Format::sniff(text)
    }

    fn sniff(text: &str) -> Format {
        let t = text.strip_prefix('\u{feff}').unwrap_or(text);
        let bytes = t.as_bytes();
        let mut i = 0;
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            break;
        }
        match bytes.get(i) {
            Some(b'{') | Some(b'[') => Format::Json,
            _ => Format::Yaml,
        }
    }
}

/// Parse config text of the given format into its document (a JSON value). A
/// syntax error names the line/column; the document must be a mapping (object)
/// at the top level.
pub fn parse_document(text: &str, format: Format) -> Result<Value, String> {
    let doc = match format {
        Format::Json => {
            let stripped = strip_jsonc(text);
            serde_json::from_str::<Value>(&stripped)
                .map_err(|e| format!("config file parse error (json): {e}"))?
        }
        Format::Yaml => {
            super::yaml::parse(text).map_err(|e| format!("config file parse error (yaml): {e}"))?
        }
    };
    match doc {
        Value::Object(_) => Ok(doc),
        Value::Null if format == Format::Yaml => Ok(Value::Object(serde_json::Map::new())),
        other => Err(format!(
            "config file must be a mapping (an object) at the top level, got {}",
            kind_name(&other)
        )),
    }
}

/// Read + parse a config file from a local path into its document, deciding the
/// format from the extension (else by sniffing the text). Errors name the path.
pub fn read_document(path: &str) -> Result<(Value, Format), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config file {path}: {e}"))?;
    let format = Format::detect(Some(Path::new(path)), &text);
    let doc = parse_document(&text, format).map_err(|e| format!("{path}: {e}"))?;
    Ok((doc, format))
}

/// Read several config files, in order, into ONE effective document: each later
/// file is merged over the previous ones with **JSON Merge Patch** semantics
/// (RFC 7396) — objects merge recursively, scalars and lists are REPLACED by the
/// later file, and a `null` value UNSETS the key. Every file is type-checked on
/// its own first (so an unknown key is reported against the file that carries
/// it), then the merged document is returned with the `(path, format)` list.
pub fn read_documents(paths: &[String]) -> Result<(Value, Vec<(String, Format)>), String> {
    read_documents_checked(paths, &|doc, source| {
        ConfigFile::from_document(doc.clone(), source).map(|_| ())
    })
}

/// [`read_documents`] with a caller-supplied per-file check (the v2 settings
/// typing, or none) — `check(doc, "config file <path>")` runs before the merge
/// so an unknown key is attributed to its file.
pub fn read_documents_checked(
    paths: &[String],
    check: &dyn Fn(&Value, &str) -> Result<(), String>,
) -> Result<(Value, Vec<(String, Format)>), String> {
    let mut merged = Value::Object(serde_json::Map::new());
    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let (doc, format) = read_document(path)?;
        check(&doc, &format!("config file {path}"))?;
        merge_into(&mut merged, doc);
        loaded.push((path.clone(), format));
    }
    Ok((merged, loaded))
}

/// JSON Merge Patch (RFC 7396): `overlay` onto `base`. Objects merge key by key
/// (recursively); any other value — a scalar or a list — replaces what was
/// there; an explicit `null` removes the key. A non-object overlay replaces the
/// base wholesale.
pub fn merge_into(base: &mut Value, overlay: Value) {
    match overlay {
        Value::Object(over) => {
            if !base.is_object() {
                *base = Value::Object(serde_json::Map::new());
            }
            let map = base.as_object_mut().expect("just ensured an object");
            for (k, v) in over {
                match v {
                    Value::Null => {
                        map.remove(&k);
                    }
                    Value::Object(_) => {
                        let slot = map
                            .entry(k)
                            .or_insert(Value::Object(serde_json::Map::new()));
                        merge_into(slot, v);
                    }
                    other => {
                        map.insert(k, other);
                    }
                }
            }
        }
        other => *base = other,
    }
}

fn kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// The `x-agentd-contract-version` the schema carries. It is the same value as
/// the capabilities manifest's `contract_version` — a tool that validated a
/// document against this schema knows exactly which runtime contract it targets
/// — and `tests::schema_contract_version_matches_manifest` holds the two equal.
pub const SCHEMA_CONTRACT_VERSION: &str = "1.0";

/// The deserialized config-file shape — one source of truth for the loader, the
/// validator, and the `--config-schema` generator. `serde` only.
///
/// `deny_unknown_fields` rejects a typo'd key at parse time (exit 2). A flattened
/// catch-all is INTENTIONALLY ABSENT — `deny_unknown_fields` is the guard.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Optional; pins the file to a schema major agentctl validated against.
    pub config_version: Option<String>,
    /// `--intelligence` / `AGENTD_INTELLIGENCE` — the ordered intelligence
    /// endpoint *list* URI. File-settable and **reloadable** so a ConfigMap
    /// update can repoint the endpoint list as a hot swap: the reload fans
    /// `ctrl/swap_intel` to in-flight work and re-points new spawns. The
    /// transport SCHEME is data, not a secret; the per-endpoint credential is
    /// NEVER inline here — it comes from env or a `_FILE` path, so a config
    /// document can be committed and mounted without carrying a credential.
    pub intelligence: Option<String>,
    /// `--model-swap` / `AGENTD_MODEL_SWAP` — the model hot-swap policy
    /// (`finish-on-old` | `restart-turn`), deciding what an in-flight turn does
    /// when the model changes under it. Reloadable. Validated against
    /// [`crate::config::SwapPolicy`].
    pub model_swap: Option<String>,
    /// `--model` / `AGENTD_MODEL` (reloadable param, never the transport).
    pub model: Option<String>,
    /// `--max-tokens` / `AGENTD_MAX_TOKENS`.
    pub max_tokens: Option<u64>,
    /// Bounds on the model loop (`--max-steps` / `--max-depth` / `--deadline`).
    pub limits: Option<LimitsFile>,
    /// The MCP server inventory — one object per `--mcp name=cmd … --mcp-tags …`.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerFile>,
    /// Declared subscriptions (reactive mode) — each string == one `--subscribe URI`.
    #[serde(default)]
    pub subscribe: Vec<String>,
    /// Declared remote-A2A delegation peers — each == one `--a2a-peer name=endpoint`.
    #[serde(default)]
    pub a2a_peers: Vec<A2aPeerFile>,
    /// `--log-level` / `AGENTD_LOG_LEVEL` (a string; validated against `Level`).
    pub log_level: Option<String>,
    /// Declared intelligence HTTP headers. Values MAY interpolate
    /// `{{secret:NAME}}` / `{{secret-file:PATH}}`; the resolved secret never
    /// lands in this struct or in a log — only the reference does. A value that
    /// looks like an inline secret is rejected outright, so a credential cannot
    /// be committed to a config file by accident.
    #[serde(default)]
    pub intelligence_headers: BTreeMap<String, String>,
}

/// The `limits` sub-object — maps to the per-run limit flags.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LimitsFile {
    /// `--max-steps`.
    pub max_steps: Option<u32>,
    /// `--max-depth`.
    pub max_depth: Option<u32>,
    /// `--deadline` in whole seconds.
    pub deadline_secs: Option<u64>,
    /// `--budget-tokens-lifetime` — the per-instance cumulative token cap
    /// across all runs and reactions, not per run (the CRD's
    /// `limits.lifetimeTokens`). `0` or absent = unbounded.
    pub lifetime_tokens: Option<u64>,
}

/// One MCP server, reached over the Streamable HTTP transport: a remote
/// `endpoint` (`https://host[:port][/path]`, loopback `http://` for dev) with
/// optional secret-free auth `headers`. There is no local process spawn — every
/// server is a network peer, so config can never turn into command execution.
/// `tags` is the glob→tags wire (the loader flattens a `{"*": ["sensitive"]}`
/// map to the server's tag set).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpServerFile {
    pub name: String,
    /// Remote MCP endpoint.
    pub endpoint: Option<String>,
    /// Auth/framing header templates — values MAY interpolate `{{secret:NAME}}` /
    /// `{{secret-file:PATH}}`, never inline secrets.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Glob→trifecta-tags. A server with no tags is treated as
    /// `untrusted_input`, so forgetting to tag one narrows the trust budget
    /// rather than widening it.
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
    /// Sign requests to this server with the AAuth agent identity.
    /// `None` inherits the global default (sign all when an identity is
    /// configured); `false` opts out; `true` opts in. Needs `--features aauth`.
    #[serde(default)]
    pub aauth: Option<bool>,
}

/// One A2A peer — maps to `--a2a-peer name=endpoint`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct A2aPeerFile {
    pub name: String,
    pub endpoint: String,
    /// Secret-free auth header templates presented TO the peer (bearer leg),
    /// e.g. `"authorization": "Bearer {{secret:PEER_TOKEN}}"`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Client-certificate PEM file paths for mutual TLS to the peer (both or
    /// neither).
    #[serde(default)]
    pub client_cert: Option<String>,
    #[serde(default)]
    pub client_key: Option<String>,
}

/// The list of `ConfigFile` field names, in declaration order — the single
/// source both the schema generator and its unit test read, so the schema's
/// `properties` can never silently diverge from the struct.
pub const CONFIG_FILE_FIELDS: &[&str] = &[
    "config_version",
    "intelligence",
    "model_swap",
    "model",
    "max_tokens",
    "limits",
    "mcp_servers",
    "subscribe",
    "a2a_peers",
    "log_level",
    "intelligence_headers",
];

impl ConfigFile {
    /// Parse config text (YAML or JSON — sniffed, since there is no path). A
    /// malformed document is an `Err` with a message the caller maps to exit 2
    /// — before any side effect. JSON-with-comments is tolerated (`//` and
    /// `/* */` are stripped first, matching the jsonc shown in the RFC set).
    pub fn parse(text: &str) -> Result<ConfigFile, String> {
        let doc = parse_document(text, Format::detect(None, text))?;
        Self::from_document(doc, "config file")
    }

    /// Type a config DOCUMENT (from a file, or the env/flag path layers —
    /// `source` names it in errors). Unknown keys are rejected
    /// (`deny_unknown_fields`); the error names the offending key.
    pub fn from_document(doc: Value, source: &str) -> Result<ConfigFile, String> {
        serde_json::from_value(doc).map_err(|e| format!("{source} parse error: {e}"))
    }

    /// Load + parse a config file from a local path (no network) — YAML or JSON
    /// by extension, sniffed otherwise.
    pub fn load(path: &str) -> Result<ConfigFile, String> {
        let (doc, _format) = read_document(path)?;
        Self::from_document(doc, "config file")
    }
}

/// Strip line (`//`) and block (`/* */`) comments from JSON-with-comments,
/// preserving string literals (a `//` inside a `"…"` is data, not a comment).
/// Byte-oriented and minimal — the moat forbids a jsonc *crate*.
///
/// **Everything kept is copied as a SLICE of `src`, never as `byte as char`.**
/// That distinction is the whole UTF-8 story: `0xE2 as char` is U+00E2 ('â'), so
/// a byte-wise copy silently mojibake's an em-dash, an accented name or any CJK
/// text into its Latin-1 shadow — and the result is still valid JSON, so nothing
/// ever reports an error and the agent runs on a subtly wrong instruction.
/// Slicing carries the whole multibyte sequence through untouched.
///
/// Scanning stays byte-wise, which is safe because every byte this function
/// *matches on* (`"`, `\`, `/`, `*`, `\n`) is ASCII, and an ASCII byte can never
/// occur inside a multibyte UTF-8 sequence (continuation bytes are all ≥ 0x80).
/// So a comment boundary is always a char boundary and the slices below can
/// never split a character.
fn strip_jsonc(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    // Start of the run of bytes not yet copied out. A run is broken only by a
    // comment; everything else is emitted verbatim by slicing `src[run..i]`.
    let mut run = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                // Skip the escape AND the escaped byte without inspecting it, so
                // a \" cannot end the string. Both stay in the current run.
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // line comment → skip to end of line (keep the newline for line counts).
            out.push_str(&src[run..i]);
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            run = i;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            // block comment → skip to the closing */.
            out.push_str(&src[run..i]);
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            // Step past the `*/`. On an UNTERMINATED comment `i` is left mid-text,
            // so clamp to the end — `bytes.len()` is always a char boundary, and
            // the malformed document is serde_json's error to report, not ours.
            i = (i + 2).min(bytes.len());
            run = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&src[run..]);
    out
}

/// Emit the hand-written **JSON Schema (Draft 2020-12)** of the config file.
/// No `schemars` — a schema *library* is binary weight the moat forbids. Kept
/// faithful to [`ConfigFile`] by `tests::schema_properties_match_struct_fields`.
///
/// `additionalProperties:false` mirrors `deny_unknown_fields`; `$id` pins the
/// major; `x-agentd-contract-version` ties it to the manifest. agentctl
/// validates a CR against this before applying it to a pod.
pub fn config_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("https://agentd.dev/schema/config/{SCHEMA_CONTRACT_VERSION}"),
        "x-agentd-contract-version": SCHEMA_CONTRACT_VERSION,
        "title": "agentd config file",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "config_version": { "type": "string" },
            "intelligence": { "type": "string" },
            "model_swap": { "enum": ["finish-on-old", "restart-turn"] },
            "model": { "type": "string" },
            "max_tokens": { "type": "integer", "minimum": 1 },
            "limits": { "$ref": "#/$defs/Limits" },
            "mcp_servers": { "type": "array", "items": { "$ref": "#/$defs/McpServer" } },
            "subscribe": { "type": "array", "items": { "type": "string" } },
            "a2a_peers": { "type": "array", "items": { "$ref": "#/$defs/A2aPeer" } },
            "log_level": { "enum": ["trace", "debug", "info", "warn", "error"] },
            "intelligence_headers": {
                "type": "object",
                "additionalProperties": { "type": "string" }
            }
        },
        "$defs": {
            "Limits": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "max_steps": { "type": "integer", "minimum": 1 },
                    "max_depth": { "type": "integer", "minimum": 0 },
                    "deadline_secs": { "type": "integer", "minimum": 0 },
                    "lifetime_tokens": { "type": "integer", "minimum": 0 }
                }
            },
            "McpServer": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "endpoint"],
                "properties": {
                    "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" },
                    "endpoint": { "type": "string" },
                    "headers": {
                        "type": "object",
                        "additionalProperties": { "type": "string" }
                    },
                    "tags": {
                        "type": "object",
                        "additionalProperties": {
                            "type": "array",
                            "items": { "enum": ["untrusted_input", "sensitive", "egress"] }
                        }
                    },
                    "aauth": {
                        "type": "boolean",
                        "description": "sign requests to this server with the AAuth agent identity; omit to inherit the global default"
                    }
                }
            },
            "A2aPeer": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "endpoint"],
                "properties": {
                    "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" },
                    "endpoint": { "type": "string" },
                    "headers": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "secret-free auth header templates presented to the peer ({{secret:NAME}} references)"
                    },
                    "client_cert": { "type": "string", "description": "client certificate PEM file path (mutual TLS to the peer; requires client_key)" },
                    "client_key": { "type": "string", "description": "client private-key PEM file path" }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_file() {
        let src = r#"{
            "config_version": "1.0",
            "model": "claude-opus-4",
            "max_tokens": 2000000,
            "limits": { "max_steps": 200, "max_depth": 4, "deadline_secs": 600 },
            "mcp_servers": [
                { "name": "web", "endpoint": "https://web.example.com/mcp",
                  "headers": { "Authorization": "Bearer {{secret:WEB_TOKEN}}" },
                  "tags": { "*": ["untrusted_input"] } }
            ],
            "subscribe": ["fs:file:///watch/inbox"],
            "a2a_peers": [{ "name": "mesh", "endpoint": "unix:/run/peer.sock" }],
            "log_level": "info",
            "intelligence_headers": { "anthropic-version": "2023-06-01" }
        }"#;
        let cf = ConfigFile::parse(src).unwrap();
        assert_eq!(cf.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(cf.max_tokens, Some(2_000_000));
        assert_eq!(cf.limits.unwrap().max_steps, Some(200));
        assert_eq!(cf.mcp_servers.len(), 1);
        assert_eq!(
            cf.mcp_servers[0].endpoint.as_deref(),
            Some("https://web.example.com/mcp")
        );
        assert_eq!(cf.subscribe, vec!["fs:file:///watch/inbox"]);
        assert_eq!(cf.a2a_peers[0].name, "mesh");
        assert_eq!(cf.log_level.as_deref(), Some("info"));
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields: a typo'd key is a hard error, not silently ignored.
        let e = ConfigFile::parse(r#"{ "max_token": 5 }"#).unwrap_err();
        assert!(e.contains("parse error"), "got: {e}");
        assert!(e.contains("max_token"), "names the key: {e}");
        // Same for YAML — the typo is named, whatever the syntax.
        let e = ConfigFile::parse("max_token: 5\n").unwrap_err();
        assert!(
            e.contains("parse error") && e.contains("max_token"),
            "got: {e}"
        );
    }

    #[test]
    fn yaml_and_json_documents_type_identically() {
        let yaml = r#"
# the same document as parses_a_full_file, in YAML
config_version: "1.0"
model: claude-opus-4
max_tokens: 2000000
limits:
  max_steps: 200
  max_depth: 4
  deadline_secs: 600
mcp_servers:
  - name: web
    endpoint: https://web.example.com/mcp
    headers:
      Authorization: "Bearer {{secret:WEB_TOKEN}}"
    tags:
      "*": [untrusted_input]
subscribe: [fs:file:///watch/inbox]
a2a_peers:
  - name: mesh
    endpoint: unix:/run/peer.sock
log_level: info
intelligence_headers:
  anthropic-version: "2023-06-01"
"#;
        let json = r#"{
            "config_version": "1.0",
            "model": "claude-opus-4",
            "max_tokens": 2000000,
            "limits": { "max_steps": 200, "max_depth": 4, "deadline_secs": 600 },
            "mcp_servers": [
                { "name": "web", "endpoint": "https://web.example.com/mcp",
                  "headers": { "Authorization": "Bearer {{secret:WEB_TOKEN}}" },
                  "tags": { "*": ["untrusted_input"] } }
            ],
            "subscribe": ["fs:file:///watch/inbox"],
            "a2a_peers": [{ "name": "mesh", "endpoint": "unix:/run/peer.sock" }],
            "log_level": "info",
            "intelligence_headers": { "anthropic-version": "2023-06-01" }
        }"#;
        let from_yaml = ConfigFile::parse(yaml).expect("yaml parses");
        let from_json = ConfigFile::parse(json).expect("json parses");
        assert_eq!(from_yaml, from_json, "one document model, two syntaxes");
        assert_eq!(from_yaml.limits.as_ref().unwrap().max_steps, Some(200));
        assert_eq!(from_yaml.mcp_servers[0].tags["*"], vec!["untrusted_input"]);
    }

    #[test]
    fn format_detection_by_extension_then_sniff() {
        assert_eq!(
            Format::detect(Some(Path::new("/etc/agentd/config.yaml")), "{}"),
            Format::Yaml
        );
        assert_eq!(Format::detect(Some(Path::new("c.YML")), "{}"), Format::Yaml);
        assert_eq!(
            Format::detect(Some(Path::new("c.json")), "model: x"),
            Format::Json
        );
        assert_eq!(
            Format::detect(Some(Path::new("c.jsonc")), "model: x"),
            Format::Json
        );
        // Unknown extension / no path: sniff the first significant character.
        assert_eq!(
            Format::detect(Some(Path::new("agentd.conf")), "  { \"a\": 1 }"),
            Format::Json
        );
        assert_eq!(Format::detect(None, "// jsonc\n{ \"a\": 1 }"), Format::Json);
        assert_eq!(Format::detect(None, "/* c */ [1]"), Format::Json);
        assert_eq!(Format::detect(None, "# yaml\nmodel: x\n"), Format::Yaml);
        assert_eq!(Format::detect(None, "model: x\n"), Format::Yaml);
        assert_eq!(Format::detect(None, ""), Format::Yaml);
    }

    #[test]
    fn merge_follows_json_merge_patch() {
        let mut base = json!({
            "model": "base",
            "limits": {"max_steps": 1, "max_depth": 2},
            "subscribe": ["a", "b"],
            "intelligence_headers": {"h1": "v1"},
            "log_level": "info"
        });
        merge_into(
            &mut base,
            json!({
                "model": "over",                    // scalar: replaced
                "limits": {"max_steps": 9},         // object: merged (max_depth kept)
                "subscribe": ["c"],                 // list: REPLACED, not appended
                "intelligence_headers": {"h2": "v2"}, // map: merged
                "log_level": null                   // null: unset
            }),
        );
        assert_eq!(
            base,
            json!({
                "model": "over",
                "limits": {"max_steps": 9, "max_depth": 2},
                "subscribe": ["c"],
                "intelligence_headers": {"h1": "v1", "h2": "v2"}
            })
        );
        // A scalar in the way of an object overlay is replaced by the object.
        let mut base = json!({"limits": 5});
        merge_into(&mut base, json!({"limits": {"max_steps": 1}}));
        assert_eq!(base, json!({"limits": {"max_steps": 1}}));
    }

    #[test]
    fn multiple_files_merge_in_order_later_wins() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.yaml");
        let prod = dir.path().join("prod.yaml");
        let extra = dir.path().join("extra.json");
        std::fs::write(
            &base,
            "model: base\nlimits:\n  max_steps: 1\n  max_depth: 2\nsubscribe: [a, b]\n",
        )
        .unwrap();
        std::fs::write(
            &prod,
            "model: prod\nlimits:\n  max_steps: 9\nsubscribe: [c]\n",
        )
        .unwrap();
        std::fs::write(
            &extra,
            r#"{ "log_level": "warn", "limits": { "max_depth": null } }"#,
        )
        .unwrap();
        let paths: Vec<String> = [&base, &prod, &extra]
            .iter()
            .map(|p| p.to_str().unwrap().to_string())
            .collect();
        let (doc, loaded) = read_documents(&paths).unwrap();
        assert_eq!(
            doc,
            json!({
                "model": "prod",
                "limits": {"max_steps": 9},
                "subscribe": ["c"],
                "log_level": "warn"
            })
        );
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].1, Format::Yaml);
        assert_eq!(loaded[2].1, Format::Json);
        // An unknown key is attributed to the file that carries it.
        std::fs::write(&prod, "modle: typo\n").unwrap();
        let e = read_documents(&paths).unwrap_err();
        assert!(e.contains("prod.yaml") && e.contains("modle"), "{e}");
        // A missing file is an error naming it.
        let e = read_documents(&["/no/such/agentd.yaml".to_string()]).unwrap_err();
        assert!(e.contains("/no/such/agentd.yaml"), "{e}");
    }

    #[test]
    fn a_non_mapping_document_is_rejected() {
        let e = parse_document("- a\n- b\n", Format::Yaml).unwrap_err();
        assert!(e.contains("mapping"), "{e}");
        let e = parse_document("[1, 2]", Format::Json).unwrap_err();
        assert!(e.contains("mapping"), "{e}");
        // An empty YAML file is an empty config (nothing set) — not an error.
        assert_eq!(
            parse_document("# nothing yet\n", Format::Yaml).unwrap(),
            json!({})
        );
        // A YAML syntax error names the line.
        let e = parse_document("a: 1\n\tb: 2\n", Format::Yaml).unwrap_err();
        assert!(e.contains("(yaml)") && e.contains("line 2"), "{e}");
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(ConfigFile::parse("{ not json").is_err());
    }

    #[test]
    fn jsonc_comments_are_stripped() {
        let src = r#"{
            // a line comment
            "model": "m", /* block */ "max_tokens": 10,
            "subscribe": ["http://x//path"]  // a // inside a string is data
        }"#;
        let cf = ConfigFile::parse(src).unwrap();
        assert_eq!(cf.model.as_deref(), Some("m"));
        assert_eq!(cf.max_tokens, Some(10));
        // The `//` inside the string literal survived (not treated as a comment).
        assert_eq!(cf.subscribe, vec!["http://x//path"]);
    }

    #[test]
    fn non_ascii_round_trips_through_the_jsonc_stripper() {
        // Silent corruption is the worst failure mode: mojibake'd text is still
        // valid JSON, so a byte-wise stripper would hand the agent a subtly wrong
        // instruction with nothing reporting an error. Every string here must come
        // back byte-identical, INCLUDING the ones pressed up against a comment —
        // that adjacency is exactly where a byte-wise stripper splits a sequence.
        let model = "Ünïcøde — 日本語 μοντέλο";
        let src = format!(
            "{{\n  /* 日本語 block */\"model\": \"{model}\",/*é*/\n  \"subscribe\": [\"fs:file:///wätch/收件箱\"] // — trailing 日本語\n}}"
        );
        let cf = ConfigFile::parse(&src).unwrap();
        assert_eq!(cf.model.as_deref(), Some(model), "mojibake in the value");
        assert_eq!(cf.subscribe, vec!["fs:file:///wätch/收件箱"]);
        // The stripper itself must be the identity on a comment-free document.
        let plain = format!("{{ \"model\": \"{model}\" }}");
        assert_eq!(strip_jsonc(&plain), plain);
        // A \-escape adjacent to multibyte text must not eat the following byte.
        let cf = ConfigFile::parse("{ \"model\": \"a\\\"—\\\\é\" }").unwrap();
        assert_eq!(cf.model.as_deref(), Some("a\"—\\é"));
    }

    #[test]
    fn schema_is_parseable_draft_2020_12() {
        let s = config_schema();
        assert_eq!(
            s["$schema"],
            json!("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(s["additionalProperties"], json!(false));
        assert_eq!(
            s["x-agentd-contract-version"],
            json!(SCHEMA_CONTRACT_VERSION)
        );
        // It round-trips through serde_json as a valid document.
        let text = serde_json::to_string(&s).unwrap();
        let _: Value = serde_json::from_str(&text).unwrap();
    }

    #[test]
    fn schema_properties_match_struct_fields() {
        // The hand-written schema cannot silently diverge from the struct: its
        // top-level `properties` keys must be EXACTLY the struct's fields.
        let s = config_schema();
        let props = s["properties"].as_object().unwrap();
        let schema_keys: std::collections::BTreeSet<&str> =
            props.keys().map(String::as_str).collect();
        let struct_keys: std::collections::BTreeSet<&str> =
            CONFIG_FILE_FIELDS.iter().copied().collect();
        assert_eq!(
            schema_keys, struct_keys,
            "schema properties drifted from ConfigFile fields"
        );
    }

    #[test]
    fn config_file_fields_const_matches_a_full_deser() {
        // Guard the CONFIG_FILE_FIELDS const itself: a fully-populated JSON object
        // keyed by every const entry must deserialize (so a renamed/added struct
        // field forces the const + schema to be updated together).
        let mut obj = serde_json::Map::new();
        for k in CONFIG_FILE_FIELDS {
            let v = match *k {
                "config_version" | "model" | "log_level" | "intelligence" => json!("x"),
                "model_swap" => json!("finish-on-old"),
                "max_tokens" => json!(1),
                "limits" => json!({}),
                "mcp_servers" => json!([{ "name": "a", "endpoint": "unix:/a.sock" }]),
                "subscribe" => json!(["u"]),
                "a2a_peers" => json!([{ "name": "p", "endpoint": "unix:/x" }]),
                "intelligence_headers" => json!({ "h": "v" }),
                other => panic!("CONFIG_FILE_FIELDS has an unmapped key {other}"),
            };
            obj.insert((*k).to_string(), v);
        }
        let text = serde_json::to_string(&Value::Object(obj)).unwrap();
        ConfigFile::parse(&text).expect("every CONFIG_FILE_FIELDS key must deserialize");
    }
}
