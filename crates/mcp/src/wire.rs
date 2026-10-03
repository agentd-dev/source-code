// SPDX-License-Identifier: AGPL-3.0-only
//! MCP wire types — agentd's own view of the Model Context Protocol results the
//! client hands its host. The SDK's typed results are converted into these
//! through JSON (see [`crate::rmcp_client`]), so agentd's call sites never name
//! an `rmcp` type.
//!
//! Method/notification names are constants (typos become compile errors).
//! Result structs use `camelCase` to match the spec. `content[]` and
//! resource `contents[]` are kept as `Vec<Value>` with text-extraction helpers
//! rather than a brittle tagged enum, so an unknown content type from a newer
//! server is preserved, not a parse error (forward-compat).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The MCP revision agentd offers in `initialize`, and the one agentd's
/// 2025-11-25 mock server answers. It is rmcp's
/// `ProtocolVersion::LATEST_WITH_INITIALIZE` — the newest revision that still
/// has an `initialize` — not rmcp's `LATEST`, which is 2026-07-28 and has none.
/// The server's answer decides the revision a connection actually speaks; the
/// pin to rmcp's constant is tested in `rmcp_client` and `tests/rmcp_backend.rs`.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Method + notification names. Constants, so a typo is a compile error rather
/// than a `-32601` at runtime.
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
    pub const RESOURCES_TEMPLATES_LIST: &str = "resources/templates/list";
    pub const PROMPTS_LIST: &str = "prompts/list";
    pub const PROMPTS_GET: &str = "prompts/get";
    pub const COMPLETION_COMPLETE: &str = "completion/complete";
    pub const LOGGING_SET_LEVEL: &str = "logging/setLevel";

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

/// What a server says it can do. The calls whose silence would be mistaken for
/// success are gated on these, and the gate is fail-closed — an absent
/// capability is a refusal, not a maybe: no subscription (`resources/subscribe`,
/// or a URI in a `subscriptions/listen` filter) unless
/// `resources.subscribe == Some(true)`, so a server that would never notify is
/// an error at subscribe time rather than a wait that parks forever; no
/// `prompts/get` without `prompts`; no `completion/complete` without
/// `completions`; and `resources/templates/list` without `resources` is empty.
/// `tools/list` and `tools/call` are not gated: a server without tools answers
/// them with an error of its own.
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
    pub fn supports_prompts(&self) -> bool {
        self.prompts.is_some()
    }
    pub fn supports_completions(&self) -> bool {
        self.completions.is_some()
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

/// Result of `tools/call`. `is_error: true` is a **tool-domain** failure (fed
/// to the model as an observation so it can adapt), distinct from a JSON-RPC
/// transport error, which fails the call outright.
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

/// A resource **template** (a parameterized `uriTemplate`, RFC 6570) a server
/// offers via `resources/templates/list` — distinct from a concrete [`Resource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    pub uri_template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

// ---- prompts ----

/// A prompt template a server offers via `prompts/list`. `arguments` describe
/// the template's fill-ins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// `prompts/get` result — the rendered messages. `messages[]` is kept as
/// `Vec<Value>` (each `{role, content}`) for forward-compat with content types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetPromptResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<Value>,
}

// ---- completion ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompleteResult {
    #[serde(default)]
    pub completion: Completion,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
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
    fn server_capabilities_parse_from_the_handshake_shape() {
        let json = r#"{"tools": {"listChanged": true}, "resources": {"subscribe": true}}"#;
        let caps: ServerCapabilities = serde_json::from_str(json).unwrap();
        assert!(caps.supports_tools());
        assert!(caps.supports_subscribe());
        assert_eq!(caps.tools.unwrap().list_changed, Some(true));
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
    fn prompts_and_completion_parse() {
        let prompt: Prompt = serde_json::from_str(
            r#"{"name": "greet", "arguments": [{"name": "who", "required": true}]}"#,
        )
        .unwrap();
        assert_eq!(prompt.name, "greet");
        assert_eq!(prompt.arguments[0].name, "who");
        assert_eq!(prompt.arguments[0].required, Some(true));

        let got: GetPromptResult = serde_json::from_str(
            r#"{"description": "d", "messages": [{"role": "user", "content": {"type": "text", "text": "hi"}}]}"#,
        )
        .unwrap();
        assert_eq!(got.messages.len(), 1);

        let comp: CompleteResult = serde_json::from_str(
            r#"{"completion": {"values": ["alice", "bob"], "hasMore": false}}"#,
        )
        .unwrap();
        assert_eq!(comp.completion.values, ["alice", "bob"]);
        assert_eq!(comp.completion.has_more, Some(false));
    }
}
