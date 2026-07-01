// SPDX-License-Identifier: Apache-2.0
//! The declarative config **file** (RFC 0017 §3) + its JSON Schema (§4.2).
//!
//! A single **JSON** document (NOT YAML — `serde_yaml` is a dependency the
//! minimalism moat forbids; an operator/agentctl renders a `ConfigMap` as JSON)
//! that carries **only verbose structural config**: the MCP-server inventory,
//! declared subscriptions, A2A peers, limits, and the model/log knobs. It
//! **never** carries secrets or per-environment scalars (those stay env/flag).
//!
//! Precedence (RFC 0011 §2.1 / RFC 0017 §3.2): `built-in default < FILE < env <
//! flag`. The file is loaded first, then `Config::load` applies env
//! and flags over it; a flag/env for the same key wins. List-valued keys
//! (`mcp_servers`, `subscribe`, `a2a_peers`) *seed* the list — repeatable
//! `--mcp`/`--subscribe`/`--a2a-peer` flags **add to** the file's list (the
//! repeatable-flag semantics operators already expect, §3.2).
//!
//! `deny_unknown_fields` makes a typo'd key (`max_token` vs `max_tokens`) a hard
//! config error (exit 2) instead of a silently-ignored value — the single most
//! common config footgun, closed at parse time.
//!
//! The schema is **hand-written** (no `schemars` — a forbidden dependency) and
//! kept faithful to this struct by a unit test asserting the schema's top-level
//! properties match the struct's fields (so they can't silently drift, §4.2).

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The `x-agentd-contract-version` the schema carries (ties to the capabilities
/// manifest's `contract_version`, RFC 0014 §5 / RFC 0017 §4.2). Kept equal to the
/// manifest's contract version by `tests::schema_contract_version_matches_manifest`.
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
    /// endpoint *list* URI (RFC 0018 §3.1). File-settable + **reloadable** so a
    /// ConfigMap update can repoint the endpoint list as a hot-swap (RFC 0018 §5):
    /// the reload fans `ctrl/swap_intel` to in-flight work and re-points new
    /// spawns. The transport SCHEME is data, not a secret; the per-endpoint
    /// credential is NEVER inline here (env/`_FILE` only, RFC 0012 §3.7).
    pub intelligence: Option<String>,
    /// `--model-swap` / `AGENTD_MODEL_SWAP` (RFC 0018 §5.3): the model hot-swap
    /// policy (`finish-on-old` | `restart-turn`). Reloadable. Validated against
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
    /// Declared intelligence HTTP headers (RFC 0006 §3). Values MAY interpolate
    /// `{{secret:NAME}}` / `{{secret-file:PATH}}` (§6); the resolved secret never
    /// lands here or in a log. An inline secret-shaped value is rejected (§3.1).
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
}

/// One MCP server. As of v2.0.0 the transport is a remote `endpoint`
/// (`https://`/`http://`/`unix:`/`vsock:`, RFC 0004 Streamable HTTP) with optional
/// secret-free auth `headers`; the legacy `command`/`argv` (stdio) is retained for
/// the test mock only. Exactly one of `endpoint` / `command` is set. `tags` is the
/// RFC 0012 §3.1 glob→tags wire (the loader flattens a `{"*": ["sensitive"]}` map
/// to the server's tag set).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpServerFile {
    pub name: String,
    /// Legacy stdio argv[0] (mutually exclusive with `endpoint`).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub argv: Vec<String>,
    /// Remote MCP endpoint (the v2.0.0 transport).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Auth/framing header templates for an `endpoint` server — values MAY
    /// interpolate `{{secret:NAME}}` / `{{secret-file:PATH}}`, never inline
    /// secrets (RFC 0012 §3.7).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// `stdio` (default) — the only transport the client speaks today.
    pub transport: Option<String>,
    /// Names only — values come from the process env, never inline (no secrets).
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// Glob→trifecta-tags (RFC 0012 §3.1). An untagged server ⇒ `untrusted_input`.
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
}

/// One A2A peer — maps to `--a2a-peer name=endpoint` (RFC 0020 §3).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct A2aPeerFile {
    pub name: String,
    pub endpoint: String,
}

/// The list of `ConfigFile` field names, in declaration order — the single
/// source the schema generator and the drift test both read, so the schema's
/// `properties` can never silently diverge from the struct (§4.2).
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
    /// Parse a config file's bytes (JSON). A malformed document is an `Err` with
    /// a message the caller maps to exit 2 — before any side effect. JSON-with-
    /// comments is tolerated by stripping `//` and `/* */` first (matching the
    /// jsonc shown throughout the RFC set).
    pub fn parse(bytes: &str) -> Result<ConfigFile, String> {
        let stripped = strip_jsonc(bytes);
        serde_json::from_str(&stripped).map_err(|e| format!("config file parse error: {e}"))
    }

    /// Load + parse a config file from a local path (`read_local` — no network).
    pub fn load(path: &str) -> Result<ConfigFile, String> {
        let bytes = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config file {path}: {e}"))?;
        Self::parse(&bytes)
    }
}

/// Strip line (`//`) and block (`/* */`) comments from JSON-with-comments,
/// preserving string literals (a `//` inside a `"…"` is data, not a comment).
/// Byte-oriented and minimal — the moat forbids a jsonc *crate*.
fn strip_jsonc(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                // Keep the escaped char verbatim (e.g. \" must not end the string).
                out.push(bytes[i + 1] as char);
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
            out.push('"');
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // line comment → skip to end of line (keep the newline for line counts).
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            // block comment → skip to the closing */.
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        // UTF-8 safety: push the raw byte's char only for ASCII; for multibyte
        // sequences copy them through unchanged.
        if b < 0x80 {
            out.push(b as char);
            i += 1;
        } else {
            // Copy the full multibyte char.
            let ch_len = utf8_len(b);
            let end = (i + ch_len).min(bytes.len());
            out.push_str(&src[i..end]);
            i = end;
        }
    }
    out
}

/// UTF-8 leading-byte → sequence length (1–4). Used only to copy a multibyte
/// char through the comment stripper unchanged.
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

/// Emit the hand-written **JSON Schema (Draft 2020-12)** of the config file
/// (RFC 0017 §4.2). No `schemars` — a schema *library* is binary weight the moat
/// forbids. Kept faithful to [`ConfigFile`] by `tests::schema_properties_match_struct_fields`.
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
                    "deadline_secs": { "type": "integer", "minimum": 0 }
                }
            },
            "McpServer": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" },
                    "command": { "type": "string" },
                    "argv": { "type": "array", "items": { "type": "string" } },
                    "endpoint": { "type": "string" },
                    "headers": {
                        "type": "object",
                        "additionalProperties": { "type": "string" }
                    },
                    "transport": { "enum": ["stdio", "unix"] },
                    "env_passthrough": { "type": "array", "items": { "type": "string" } },
                    "tags": {
                        "type": "object",
                        "additionalProperties": {
                            "type": "array",
                            "items": { "enum": ["untrusted_input", "sensitive", "egress"] }
                        }
                    }
                }
            },
            "A2aPeer": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "endpoint"],
                "properties": {
                    "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" },
                    "endpoint": { "type": "string" }
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
                { "name": "web", "command": "mcp-fetch", "argv": ["--timeout", "30"],
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
        assert_eq!(cf.mcp_servers[0].command, "mcp-fetch");
        assert_eq!(cf.subscribe, vec!["fs:file:///watch/inbox"]);
        assert_eq!(cf.a2a_peers[0].name, "mesh");
        assert_eq!(cf.log_level.as_deref(), Some("info"));
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields: a typo'd key is a hard error, not silently ignored.
        let e = ConfigFile::parse(r#"{ "max_token": 5 }"#).unwrap_err();
        assert!(e.contains("parse error"), "got: {e}");
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
    fn schema_contract_version_matches_manifest() {
        // The schema's x-agentd-contract-version must equal the capabilities
        // manifest's contract_version (RFC 0014 §5) — they are one frozen public
        // contract and must not drift. We read the manifest's value through its
        // public surface so the coupling is enforced without exposing the const.
        let env: Vec<(String, String)> = vec![
            ("INSTRUCTION".into(), "x".into()),
            ("AGENTD_INTELLIGENCE".into(), "unix:/x".into()),
        ];
        let cfg = crate::config::Config::load(&[], &env).unwrap();
        let id = crate::identity::Identity::from_env(&cfg.run_id);
        let manifest = crate::capabilities::manifest(&cfg, &id, false);
        assert_eq!(
            manifest["contract_version"],
            json!(SCHEMA_CONTRACT_VERSION),
            "schema contract version drifted from the manifest"
        );
        assert_eq!(
            config_schema()["x-agentd-contract-version"],
            manifest["contract_version"]
        );
    }

    #[test]
    fn schema_properties_match_struct_fields() {
        // The hand-written schema cannot silently drift from the struct: its
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
                "mcp_servers" => json!([{ "name": "a", "command": "c" }]),
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
