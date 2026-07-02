// SPDX-License-Identifier: Apache-2.0
//! MCP wire types — the Model Context Protocol message surface. RFC 0004 (client),
//! RFC 0005 (server).
//!
//! Method/notification names are constants (typos become compile errors).
//! Result/param structs use `camelCase` to match the spec. `content[]` and
//! resource `contents[]` are kept as `Vec<Value>` with text-extraction helpers
//! rather than a brittle tagged enum, so an unknown content type from a newer
//! server is preserved, not a parse error (forward-compat).
//!
//! The protocol version + era model lives in [`crate::version`]; it is re-exported
//! here so `mcp::wire::{PROTOCOL_VERSION, negotiate_version, …}` resolves.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::version::*;

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
    pub const RESOURCES_TEMPLATES_LIST: &str = "resources/templates/list";
    pub const PROMPTS_LIST: &str = "prompts/list";
    pub const PROMPTS_GET: &str = "prompts/get";
    pub const COMPLETION_COMPLETE: &str = "completion/complete";
    pub const LOGGING_SET_LEVEL: &str = "logging/setLevel";

    // Tasks extension (io.modelcontextprotocol/tasks): async long-running requests.
    pub const TASKS_GET: &str = "tasks/get";
    pub const TASKS_UPDATE: &str = "tasks/update";
    pub const TASKS_CANCEL: &str = "tasks/cancel";
    pub const NOTIFY_TASKS: &str = "notifications/tasks";

    // Modern (2026-07-28+, stateless) methods.
    /// Query a server's supported versions + capabilities + identity in one call
    /// (the stateless replacement for the `initialize` capability exchange).
    pub const SERVER_DISCOVER: &str = "server/discover";
    /// Open the long-lived notification stream (its SSE response carries the
    /// change notifications the client opted in to — the stateless replacement
    /// for the removed GET SSE stream).
    pub const SUBSCRIPTIONS_LISTEN: &str = "subscriptions/listen";

    // Notifications (no id, no response).
    pub const NOTIFY_RESOURCES_UPDATED: &str = "notifications/resources/updated";
    pub const NOTIFY_RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
    pub const NOTIFY_TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    pub const NOTIFY_SUBSCRIPTIONS_ACK: &str = "notifications/subscriptions/acknowledged";
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

/// Result of `server/discover` (modern era): the server's supported protocol
/// versions, capabilities, and identity in a single call — the stateless
/// replacement for the legacy `initialize` capability exchange. `resultType` and
/// the caching fields (`ttlMs`/`cacheScope`) are carried for forward-compat.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_type: Option<String>,
    #[serde(default)]
    pub supported_versions: Vec<String>,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    #[serde(default)]
    pub server_info: Option<Implementation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResourceTemplatesResult {
    pub resource_templates: Vec<ResourceTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
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

// ---- tasks extension (io.modelcontextprotocol/tasks) ----

/// The tasks extension identifier — advertised in `capabilities.extensions` to
/// opt into task-augmented (async long-running) requests.
pub const TASKS_EXTENSION: &str = "io.modelcontextprotocol/tasks";

/// A durable async-task handle (the tasks extension). A supported request (e.g.
/// `tools/call`) may return one (`resultType: "task"`) instead of blocking; the
/// client polls [`method::TASKS_GET`] until a terminal `status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub task_id: String,
    /// `working` | `input_required` | `completed` | `failed` | `cancelled`.
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
    /// On `completed`: what the original request would have returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// On `failed`: the JSON-RPC error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    /// On `input_required`: the server's outstanding input requests (MRTR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_requests: Option<Value>,
}

impl Task {
    /// A terminal status (`completed`/`failed`/`cancelled`) — polling stops.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
    pub fn needs_input(&self) -> bool {
        self.status == "input_required"
    }
}

/// If a result value is a task handle (`resultType: "task"`), parse it — the
/// polymorphic shape a task-augmented request returns instead of its normal result.
pub fn as_task_result(result: &Value) -> Option<Task> {
    if result.get("resultType").and_then(Value::as_str) == Some("task") {
        serde_json::from_value(result.clone()).ok()
    } else {
        None
    }
}

// ---- prompts ----

/// A prompt template a server offers (RFC 0004 §prompts). `arguments` describe
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPromptsResult {
    pub prompts: Vec<Prompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `prompts/get` params — the template name + its argument fills (all strings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
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

/// `completion/complete` params: what to complete (a `ref` to a prompt or
/// resource template) and the argument being typed. Kept as `Value` — the `ref`
/// shape varies by target and revision (forward-compat).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteParams {
    #[serde(rename = "ref")]
    pub reference: Value,
    pub argument: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

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
    use serde_json::json;

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

    #[test]
    fn discover_result_parses() {
        let json = r#"{
            "resultType": "complete",
            "supportedVersions": ["2026-07-28", "2025-11-25"],
            "capabilities": {"tools": {}, "resources": {"subscribe": true}, "prompts": {}},
            "serverInfo": {"name": "s", "version": "1"},
            "ttlMs": 3600000, "cacheScope": "public"
        }"#;
        let d: DiscoverResult = serde_json::from_str(json).unwrap();
        assert_eq!(d.supported_versions, ["2026-07-28", "2025-11-25"]);
        assert!(d.capabilities.supports_tools());
        assert!(d.capabilities.supports_subscribe());
        assert!(d.capabilities.supports_prompts());
        assert_eq!(d.ttl_ms, Some(3_600_000));
    }

    #[test]
    fn task_result_detected_and_lifecycle() {
        // A tools/call result that is actually a task handle.
        let create = json!({"resultType": "task", "taskId": "t-1", "status": "working",
            "pollIntervalMs": 250, "ttlMs": 60000});
        let t = as_task_result(&create).expect("is a task result");
        assert_eq!(t.task_id, "t-1");
        assert_eq!(t.poll_interval_ms, Some(250));
        assert!(!t.is_terminal());
        // A normal (non-task) result is not a task.
        assert!(as_task_result(&json!({"content": []})).is_none());
        // Terminal / input states.
        let done: Task = serde_json::from_value(
            json!({"taskId": "t-1", "status": "completed", "result": {"content": []}}),
        )
        .unwrap();
        assert!(done.is_terminal() && !done.needs_input());
        let ask: Task = serde_json::from_value(
            json!({"taskId": "t-1", "status": "input_required", "inputRequests": {}}),
        )
        .unwrap();
        assert!(ask.needs_input() && !ask.is_terminal());
    }

    #[test]
    fn prompts_and_completion_parse() {
        let list: ListPromptsResult = serde_json::from_str(
            r#"{"prompts": [{"name": "greet", "arguments": [{"name": "who", "required": true}]}]}"#,
        )
        .unwrap();
        assert_eq!(list.prompts[0].name, "greet");
        assert_eq!(list.prompts[0].arguments[0].name, "who");
        assert_eq!(list.prompts[0].arguments[0].required, Some(true));

        let got: GetPromptResult = serde_json::from_str(
            r#"{"description": "d", "messages": [{"role": "user", "content": {"type": "text", "text": "hi"}}]}"#,
        )
        .unwrap();
        assert_eq!(got.messages.len(), 1);

        let comp: CompleteResult =
            serde_json::from_str(r#"{"completion": {"values": ["alice", "bob"], "hasMore": false}}"#)
                .unwrap();
        assert_eq!(comp.completion.values, ["alice", "bob"]);
        assert_eq!(comp.completion.has_more, Some(false));
    }
}
