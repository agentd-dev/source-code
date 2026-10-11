// SPDX-License-Identifier: AGPL-3.0-only
//! Intelligence wire types — the **provider-neutral** representation the
//! agentic loop reasons over.
//!
//! The loop builds a [`Request`] and consumes a [`Response`] without knowing
//! which provider answered. The `intel/openai.rs` and `intel/anthropic.rs`
//! adapters translate to/from the on-the-wire JSON dialects; a model lacking
//! native tool-calling falls back to the JSON-action shape parsed in
//! `agentloop/action.rs`. Because the neutral model lives here rather than in
//! one provider's struct, supporting another provider costs one adapter and no
//! change to the loop — that is what keeps the adapter count from growing into
//! the rest of the runtime.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One conversation message. `Assistant` may carry tool calls; `ToolResult`
/// feeds a tool's output back into the conversation as the next observation.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
        /// The model's reasoning, as its dialect returned it (RFC 0046 §5.7):
        /// sent back unchanged — to the model that produced it, and no other —
        /// while the same turn is still calling tools; every other path
        /// ignores it, and it never outlives the turn.
        reasoning: Option<ReasoningParts>,
    },
    /// A tool/exec result fed back into the loop. `is_error` carries the MCP
    /// `isError: true` signal: the tool ran and reported a domain failure the
    /// model should see and react to. A transport error never reaches here —
    /// it fails the call instead of becoming an observation.
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
    /// An assistant message with no reasoning parts.
    pub fn assistant(text: Option<String>, tool_calls: Vec<ToolCall>) -> Message {
        Message::Assistant {
            text,
            tool_calls,
            reasoning: None,
        }
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
/// the scoped MCP `tools/list` plus agentd's own self-tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// JSON Schema of the tool's input (the MCP `inputSchema`).
    pub input_schema: Value,
}

/// A request to the intelligence endpoint.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    /// A decision to answer instead of a conversation to continue (RFC 0046
    /// §5.5): a `decisions` endpoint answers it natively, a chat endpoint
    /// emulates it. `messages` and `tools` are unused when it is set.
    pub decision: Option<super::decision::Decision>,
    /// How hard a reasoning model thinks (RFC 0046 §5.7); set by the client
    /// from the model it dials. `None` for a chat model.
    pub reasoning: Option<ReasoningAsk>,
}

/// How much a reasoning model thinks before it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Effort> {
        match s {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            _ => None,
        }
    }

    /// The thinking budget a dialect that takes one in tokens is given for
    /// an effort, when the model names no `budget`.
    pub fn default_budget(self) -> u64 {
        match self {
            Effort::Low => 2_048,
            Effort::Medium => 8_192,
            Effort::High => 24_576,
        }
    }
}

/// An assistant message's content as a reasoning model returned it — every
/// block, in order (thinking interleaves with tool use), with the dialect
/// and model it came from. A thinking block's signature binds it to the
/// model and the conversation that produced it, so the parts go back to
/// that model alone: a fallback, a degrade or a swap to another model is
/// sent the message without them.
#[derive(Debug, Clone, PartialEq)]
pub struct ReasoningParts {
    /// `<dialect>:<model>` — [`ReasoningParts::origin`].
    pub origin: String,
    /// The content blocks, verbatim and in their order.
    pub content: Vec<Value>,
}

impl ReasoningParts {
    /// The origin a dialect and a model give their parts.
    pub fn origin(dialect: &str, model: &str) -> String {
        format!("{dialect}:{model}")
    }

    /// `parts`, when they came from `dialect` and `model`.
    pub fn for_model<'a>(
        parts: &'a Option<ReasoningParts>,
        dialect: &str,
        model: &str,
    ) -> Option<&'a [Value]> {
        parts
            .as_ref()
            .filter(|p| p.origin == Self::origin(dialect, model))
            .map(|p| p.content.as_slice())
    }
}

/// A reasoning model's controls for one call: the effort, and a token budget
/// for the dialects that take one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningAsk {
    pub effort: Effort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u64>,
}

impl ReasoningAsk {
    /// The budget in tokens: the model's, else the effort's default.
    pub fn budget_tokens(&self) -> u64 {
        self.budget.unwrap_or_else(|| self.effort.default_budget())
    }
}

/// Why the model stopped — drives the loop's branch (tool-use vs final) and
/// the `exhausted_tokens` terminal status.
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

/// Token accounting from one model call. The supervisor sums these into the
/// run's budget, and a child's usage also counts against its parent's.
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
    /// The fallback model that answered, when the model asked was
    /// unavailable; `None` when it answered itself.
    pub fallback: Option<String>,
    /// The answers, when the request was a decision.
    pub decision: Option<super::decision::Answers>,
    /// The model's reasoning ([`Message::Assistant`]); its origin is
    /// stamped by the endpoint that dialed it.
    pub reasoning: Option<ReasoningParts>,
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
            fallback: None,
            decision: None,
            reasoning: None,
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
