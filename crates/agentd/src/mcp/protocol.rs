//! MCP JSON-RPC 2.0 wire types.
//!
//! Narrow surface — only the methods the Phase 5 handlers call:
//!
//! - `initialize` (once, at session start)
//! - `tools/call`
//! - `resources/read`
//!
//! Matches the wire shapes of the 2024-11-05 MCP spec revision, so
//! any compliant stdio server (the reference servers, mcp-fs-style
//! file servers, custom ones) can sit on the other end.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// JSON-RPC envelopes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct RpcRequest<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: P,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RpcResponse<R> {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// Correlation id. Kept on the type so future multiplexed
    /// transports can assert pairing; Phase 5's one-in-flight
    /// stdio client doesn't branch on it.
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub result: Option<R>,
    pub error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RpcError {
    pub code: i32,
    pub message: String,
}

// A JSON-RPC notification (no `id`).
#[derive(Debug, Serialize)]
pub(crate) struct RpcNotification<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: P,
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'static str,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
    pub capabilities: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClientInfo {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InitializeResult {
    #[serde(rename = "protocolVersion", default)]
    #[allow(dead_code)]
    pub protocol_version: String,
    // Server info / capabilities are accepted but unused for now;
    // `serde(default)` keeps the parse tolerant.
}

// ---------------------------------------------------------------------------
// tools/call
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct ToolsCallParams<'a> {
    pub name: &'a str,
    pub arguments: Value,
}

/// Result of a `tools/call` invocation — expose the full shape so
/// workflow authors can branch on it. `structured_content` /
/// `content` / `is_error` all surface here; unknown fields get
/// collected so downstream nodes can still read them.
#[derive(Debug, Deserialize, Default)]
pub struct ToolsCallResult {
    #[serde(default, rename = "content")]
    pub content: Vec<Value>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
    #[serde(default, rename = "structuredContent")]
    pub structured_content: Option<Value>,
}

// ---------------------------------------------------------------------------
// resources/read
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct ResourcesReadParams<'a> {
    pub uri: &'a str,
}

#[derive(Debug, Deserialize, Default)]
pub struct ResourcesReadResult {
    #[serde(default)]
    pub contents: Vec<Value>,
}
