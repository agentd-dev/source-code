//! Intelligence wire types — the **provider-neutral** representation the
//! agentic loop reasons over. RFC 0006.
//!
//! The loop builds a [`Request`] and consumes a [`Response`] without knowing
//! which provider answered. The `intel/openai.rs` and `intel/anthropic.rs`
//! adapters translate to/from the on-the-wire JSON dialects; a model lacking
//! native tool-calling falls back to the JSON-action shape parsed in
//! `agentloop/action.rs`. Keeping the neutral model here (not a provider
//! struct) is what holds the two-adapters-and-no-more line (RFC 0006 §wire).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One conversation message. `Assistant` may carry tool calls; `ToolResult`
/// feeds a tool's output back as the next observation (RFC 0007 §loop).
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    /// A tool/exec result fed back into the loop. `is_error` carries the MCP
    /// `isError: true` signal (a tool-domain failure observation, distinct
    /// from a transport error — RFC 0004 §isError).
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
}

impl Message {
    pub fn system(s: impl Into<String>) -> Message {
        Message::System(s.into())
    }
    pub fn user(s: impl Into<String>) -> Message {
        Message::User(s.into())
    }
    pub fn tool_result(
        id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Message {
        Message::ToolResult {
            id: id.into(),
            content: content.into(),
            is_error,
        }
    }
}

/// A model-requested tool invocation. `arguments` is already-parsed JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A tool advertised to the model in the request `tools` field. Sourced from
/// the scoped MCP `tools/list` (RFC 0004) plus agentd's self-tools (RFC 0005).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// JSON Schema of the tool's input (the MCP `inputSchema`).
    pub input_schema: Value,
}

/// A request to the intelligence endpoint.
#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

/// Why the model stopped — drives the loop's branch (tool-use vs final) and
/// the `exhausted_tokens` terminal status (RFC 0007 §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model produced a final answer.
    EndTurn,
    /// The model requested one or more tools.
    ToolUse,
    /// The model hit the response `max_tokens` cap.
    MaxTokens,
    /// Anything else a provider reports (mapped, not dropped).
    Other,
}

/// Token accounting from one model call. Summed into the run budget by the
/// supervisor (RFC 0003 §hierarchical-accounting).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// A response from the intelligence endpoint, normalized across providers.
#[derive(Debug, Clone)]
pub struct Response {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl Response {
    /// The model wants tools run before it continues — the loop must execute
    /// them and feed results back rather than treating `text` as final.
    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_totals() {
        let u = Usage {
            input_tokens: 100,
            output_tokens: 25,
        };
        assert_eq!(u.total(), 125);
    }

    #[test]
    fn tool_call_roundtrips() {
        let tc = ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/etc/hosts"}),
        };
        let s = serde_json::to_string(&tc).unwrap();
        let back: ToolCall = serde_json::from_str(&s).unwrap();
        assert_eq!(back, tc);
    }

    #[test]
    fn response_branch() {
        let r = Response {
            text: None,
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "x".into(),
                arguments: Value::Null,
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };
        assert!(r.wants_tools());
    }

    #[test]
    fn stop_reason_snake_case() {
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"tool_use\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
    }
}
