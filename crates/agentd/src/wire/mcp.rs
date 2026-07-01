// SPDX-License-Identifier: Apache-2.0
//! MCP wire types — the Model Context Protocol surface agentd's client and
//! self-server speak. Target **2025-11-25** (interop down to 2024-11-05).
//! RFC 0004 (client), RFC 0005 (server).
//!
//! Method/notification names are constants (typos become compile errors).
//! Result/param structs use `camelCase` to match the spec. `content[]` and
//! resource `contents[]` are kept as `Vec<Value>` with text-extraction
//! helpers rather than a brittle tagged enum, so an unknown content type from
//! a newer server is preserved, not a parse error (forward-compat).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The MCP protocol revisions agentd's client understands, **newest first**
/// (MCP versions the spec's `YYYY-MM-DD` date form, and dates sort
/// chronologically). The head is the LATEST — advertised in `initialize`
/// (lifecycle §version-negotiation: the client "SHOULD send the *latest* version
/// it supports"). To support a newly-released revision, add its date at the FRONT
/// — agentd's narrow client surface (initialize / ping / tools/{list,call} /
/// resources/{list,read,subscribe,unsubscribe}) is stable across every revision
/// to date, so a bump is a one-line change here.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25", // current (modelcontextprotocol.io/specification/versioning)
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// The latest protocol version agentd advertises in `initialize` (the head of
/// [`SUPPORTED_PROTOCOL_VERSIONS`]).
pub const PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// The version a spec-compliant Streamable HTTP server assumes when a request
/// carries no `MCP-Protocol-Version` header (RFC transports §protocol-version-
/// header). agentd always sends the header post-initialize, so this is only the
/// documented fallback contract.
pub const DEFAULT_NEGOTIATED_VERSION: &str = "2025-03-26";

/// Is `v` a revision this client explicitly understands?
pub fn is_supported_version(v: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&v)
}

/// Does `s` have the MCP `YYYY-MM-DD` version shape? (Cheap structural check, not
/// a calendar validation — enough to tell a date revision from a bogus string.)
pub fn is_date_version(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, &c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Negotiate the session protocol version from the server's `initialize` response
/// (lifecycle §version-negotiation). The server echoes our advertised version if
/// it supports it, else returns another version it supports.
///
/// * A version we **know** → adopt it.
/// * An **unknown but newer** well-formed date → adopt it optimistically
///   (forward-compat: a future revision keeps our stable method subset, so a
///   brand-new server is reachable *before* we add its date above) — the caller
///   should log that it is speaking an unrecognized revision.
/// * Anything else (an older-unknown or malformed version) → `None`: the client
///   cannot agree on a version and SHOULD disconnect.
pub fn negotiate_version(server_version: &str) -> Option<String> {
    if is_supported_version(server_version) {
        return Some(server_version.to_string());
    }
    if is_date_version(server_version) && server_version > PROTOCOL_VERSION {
        return Some(server_version.to_string());
    }
    None
}

/// Method + notification names (RFC 0004 §wire).
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "notifications/initialized";
    pub const PING: &str = "ping";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const RESOURCES_LIST: &str = "resources/list";
    pub const RESOURCES_READ: &str = "resources/read";
    pub const RESOURCES_SUBSCRIBE: &str = "resources/subscribe";
    pub const RESOURCES_UNSUBSCRIBE: &str = "resources/unsubscribe";

    // Notifications (no id, no response).
    pub const NOTIFY_RESOURCES_UPDATED: &str = "notifications/resources/updated";
    pub const NOTIFY_RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
    pub const NOTIFY_TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    pub const NOTIFY_CANCELLED: &str = "notifications/cancelled";
    pub const NOTIFY_PROGRESS: &str = "notifications/progress";
    pub const NOTIFY_MESSAGE: &str = "notifications/message";
}

// ---- lifecycle ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Capabilities a client declares. agentd declares **none** in v1 (no roots /
/// sampling / elicitation / tasks) — RFC 0004 §declare-no-client-caps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: Implementation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    #[serde(default)]
    pub server_info: Option<Implementation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// What a server says it can do. We gate every call on these (RFC 0004
/// §capability-gating): no `tools/call` unless `tools` is present; no
/// `resources/subscribe` unless `resources.subscribe == Some(true)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completions: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

impl ServerCapabilities {
    pub fn supports_tools(&self) -> bool {
        self.tools.is_some()
    }
    pub fn supports_resources(&self) -> bool {
        self.resources.is_some()
    }
    pub fn supports_subscribe(&self) -> bool {
        self.resources
            .as_ref()
            .and_then(|r| r.subscribe)
            .unwrap_or(false)
    }
}

// ---- tools ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListToolsResult {
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

/// Result of `tools/call`. `is_error: true` is a **tool-domain** failure (fed
/// to the model as an observation), distinct from a JSON-RPC transport error
/// (RFC 0004 §isError).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl CallToolResult {
    pub fn is_error(&self) -> bool {
        self.is_error.unwrap_or(false)
    }
    /// Concatenate the `text` parts of `content[]` — what the loop feeds back
    /// to the model. Non-text parts (image/audio/resource) are summarized by
    /// type so the model knows they were returned.
    pub fn text(&self) -> String {
        content_text(&self.content)
    }
}

// ---- resources ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResourcesResult {
    pub resources: Vec<Resource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceParams {
    pub uri: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadResourceResult {
    /// Each entry is a `{uri, mimeType, text}` or `{uri, mimeType, blob}`
    /// object; kept as `Value` for forward-compat. Use [`Self::text`].
    #[serde(default)]
    pub contents: Vec<Value>,
}

impl ReadResourceResult {
    pub fn text(&self) -> String {
        content_text(&self.contents)
    }
}

/// `resources/subscribe` / `resources/unsubscribe` params (per-URI only —
/// templates are NOT subscribable, RFC 0004 §item-vs-list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeParams {
    pub uri: String,
}

/// Payload of `notifications/resources/updated` — **URI only** (no diff). The
/// reactive core re-reads on wake: notify-then-read (RFC 0004 §1.3, RFC 0008).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceUpdatedParams {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Extract human-readable text from an MCP `content[]` / `contents[]` array.
/// Text parts are concatenated; other known parts are noted by type.
fn content_text(items: &[Value]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            // Tool text parts and resource `contents[]` (which omit `type` but
            // carry `text`) both land here.
            Some("text") | None => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            Some(other) => parts.push(format!("[{other} content]")),
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_version_is_the_head_of_the_supported_list() {
        assert_eq!(PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS[0]);
        assert_eq!(PROTOCOL_VERSION, "2025-11-25");
        // The list is ordered newest-first (dates sort chronologically).
        let mut sorted = SUPPORTED_PROTOCOL_VERSIONS.to_vec();
        sorted.sort_unstable();
        sorted.reverse();
        assert_eq!(sorted.as_slice(), SUPPORTED_PROTOCOL_VERSIONS);
    }

    #[test]
    fn is_date_version_recognizes_the_shape() {
        assert!(is_date_version("2025-11-25"));
        assert!(is_date_version("2026-07-01"));
        assert!(!is_date_version("2025-11-5")); // wrong length
        assert!(!is_date_version("2025/11/25")); // wrong separators
        assert!(!is_date_version("1.0.0"));
        assert!(!is_date_version("twenty-fifth"));
    }

    #[test]
    fn negotiate_adopts_known_versions() {
        // Our latest, and any older known revision the server may downgrade to.
        for v in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(negotiate_version(v).as_deref(), Some(*v));
        }
    }

    #[test]
    fn negotiate_accepts_a_newer_unknown_revision_forward_compat() {
        // A future dated revision (this month's upcoming one, before we add it):
        // adopted optimistically so a brand-new server is still reachable.
        let future = "2099-01-01";
        assert!(!is_supported_version(future));
        assert_eq!(negotiate_version(future).as_deref(), Some(future));
    }

    #[test]
    fn negotiate_refuses_unknown_old_or_malformed_versions() {
        // An OLDER unknown date we can't speak → disconnect.
        assert_eq!(negotiate_version("2020-01-01"), None);
        // A malformed / non-date version string → disconnect.
        assert_eq!(negotiate_version("1.0.0"), None);
        assert_eq!(negotiate_version(""), None);
    }

    #[test]
    fn initialize_result_parses_capabilities() {
        let json = r#"{
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {"listChanged": true}, "resources": {"subscribe": true}},
            "serverInfo": {"name": "fs", "version": "1.0"}
        }"#;
        let r: InitializeResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.protocol_version, "2025-11-25");
        assert!(r.capabilities.supports_tools());
        assert!(r.capabilities.supports_subscribe());
    }

    #[test]
    fn capability_gating_defaults_closed() {
        let caps = ServerCapabilities::default();
        assert!(!caps.supports_tools());
        assert!(!caps.supports_subscribe());
        // tools present but subscribe absent -> subscribe denied
        let json = r#"{"tools": {}, "resources": {"listChanged": true}}"#;
        let caps: ServerCapabilities = serde_json::from_str(json).unwrap();
        assert!(caps.supports_tools());
        assert!(!caps.supports_subscribe());
    }

    #[test]
    fn call_tool_result_text_and_error() {
        let json = r#"{"content": [{"type": "text", "text": "hello"}, {"type": "image", "data": "..."}], "isError": false}"#;
        let r: CallToolResult = serde_json::from_str(json).unwrap();
        assert!(!r.is_error());
        assert!(r.text().contains("hello"));
        assert!(r.text().contains("[image content]"));
    }

    #[test]
    fn updated_notification_is_uri_only() {
        let json = r#"{"uri": "file:///data/in.json"}"#;
        let p: ResourceUpdatedParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.uri, "file:///data/in.json");
        assert!(p.title.is_none());
    }

    #[test]
    fn tool_list_pagination_cursor() {
        let json = r#"{"tools": [{"name": "read_file", "inputSchema": {"type": "object"}}], "nextCursor": "abc"}"#;
        let r: ListToolsResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.next_cursor.as_deref(), Some("abc"));
    }
}
