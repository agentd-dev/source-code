// SPDX-License-Identifier: AGPL-3.0-only
//! OpenAI-compatible `/chat/completions` adapter with native tool-calling.
//!
//! This is the default dialect because it covers vLLM, Ollama, LM Studio and
//! most hosted gateways, and because it gives the model first-class `tools` and
//! `tool_calls` instead of prompt-parsed actions. The module is pure
//! translation — neutral [`Request`] → OpenAI JSON, OpenAI JSON → neutral
//! [`Response`] — and performs no I/O; `intel/client.rs` owns the socket.

use crate::wire::intel::{Message, Request, Response, StopReason, ToolCall, Usage};
use serde_json::{Map, Value, json};

/// The default endpoint path when the intelligence URL carries no explicit
/// path (`https://host[:port]` with path `/`).
pub const DEFAULT_PATH: &str = "/v1/chat/completions";

/// The output-token-limit parameter name for `model`. OpenAI's reasoning models
/// (`gpt-5*`, the `o1/o3/o4` series) require `max_completion_tokens` and REJECT
/// the older `max_tokens`; everything else (gpt-4o/4.1, and self-hosted
/// OpenAI-compatible servers that only implement the classic param) takes
/// `max_tokens`. Choosing per model keeps both working.
fn max_tokens_key(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m.starts_with("gpt-5")
        || m.starts_with("chatgpt-5")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
    {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

/// Build the request body (JSON bytes) and the HTTP headers for a chat
/// completion. `token`, if present, becomes `Authorization: Bearer …`.
pub fn build_request(req: &Request, token: Option<&str>) -> (Vec<u8>, Vec<(String, String)>) {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    match &req.reasoning {
        // A reasoning model takes `reasoning_effort` and no temperature. Its
        // thinking counts against `max_completion_tokens` — the parameter
        // every reasoning model takes, whatever its name — so the cap leaves
        // the effort's room for it above the answer.
        Some(r) => {
            body.insert(
                "max_completion_tokens".into(),
                json!(req.max_tokens as u64 + r.budget_tokens()),
            );
            body.insert("reasoning_effort".into(), json!(r.effort.as_str()));
        }
        None => {
            body.insert(max_tokens_key(&req.model).into(), json!(req.max_tokens));
            if let Some(t) = req.temperature {
                body.insert("temperature".into(), json!(t));
            }
        }
    }
    body.insert("messages".into(), json!(messages_to_openai(&req.messages)));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();
        body.insert("tools".into(), json!(tools));
        body.insert("tool_choice".into(), json!("auto"));
    }
    // The answer's schema, asked natively: strict where OpenAI's strict mode
    // takes it, else as guidance. It coexists with `tools` — the model may
    // still call one, and its final answer takes the format.
    if let Some((schema, strict)) = req
        .output_schema
        .as_ref()
        .and_then(super::structured::openai)
    {
        body.insert(
            "response_format".into(),
            json!({"type": "json_schema", "json_schema": {"name": "answer", "schema": schema, "strict": strict}}),
        );
    }

    let bytes = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(tok) = token {
        headers.push(("authorization".to_string(), format!("Bearer {tok}")));
    }
    (bytes, headers)
}

fn messages_to_openai(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| match m {
            Message::System(s) => json!({"role": "system", "content": s}),
            Message::User(s) => json!({"role": "user", "content": s}),
            Message::Assistant {
                text, tool_calls, ..
            } => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("assistant"));
                obj.insert("content".into(), json!(text));
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    // OpenAI requires arguments as a JSON *string*.
                                    "arguments": serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".into()),
                                }
                            })
                        })
                        .collect();
                    obj.insert("tool_calls".into(), json!(calls));
                }
                Value::Object(obj)
            }
            Message::ToolResult { id, content, is_error } => {
                // OpenAI has no error flag on tool messages; prefix so the
                // model still sees that this observation was an error.
                let body = if *is_error { format!("ERROR: {content}") } else { content.clone() };
                json!({"role": "tool", "tool_call_id": id, "content": body})
            }
        })
        .collect()
}

/// Parse an OpenAI `/chat/completions` response body into the neutral
/// [`Response`]. Tolerant: missing usage → zero; unknown finish reason →
/// [`StopReason::Other`]; tool-call arguments that aren't valid JSON are
/// wrapped as `{"_raw": "…"}` rather than dropped.
pub fn parse_response(body: &[u8]) -> Result<Response, String> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| format!("intel: bad JSON response: {e}"))?;

    // Surface an OpenAI-style error object clearly.
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(format!("intel: provider error: {msg}"));
    }

    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| "intel: response has no choices".to_string())?;
    let message = choice.get("message").unwrap_or(&Value::Null);

    let text = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for c in calls {
            let id = c
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let func = c.get("function").unwrap_or(&Value::Null);
            let name = func
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let raw_args = func
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let arguments =
                serde_json::from_str(raw_args).unwrap_or_else(|_| json!({ "_raw": raw_args }));
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
    }

    let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };

    let usage = v.get("usage").map(|u| Usage {
        input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: u
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    });

    Ok(Response {
        text,
        tool_calls,
        stop_reason,
        usage: usage.unwrap_or_default(),
        fallback: None,
        decision: None,
        reasoning: None,
    })
}

#[cfg(test)]
mod tests {

    /// A reasoning model is asked with `reasoning_effort`, its completion in
    /// `max_completion_tokens` with room for the thinking, and no
    /// temperature (RFC 0046 §5.7).
    #[test]
    fn a_reasoning_request_carries_its_effort_and_no_temperature() {
        let req = Request {
            model: "o-mini".into(),
            messages: vec![Message::user("hi")],
            max_tokens: 500,
            temperature: Some(0.0),
            reasoning: Some(crate::wire::intel::ReasoningAsk {
                effort: crate::wire::intel::Effort::High,
                budget: None,
            }),
            ..Default::default()
        };
        let (body, _) = build_request(&req, None);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["reasoning_effort"], "high");
        assert_eq!(v["max_completion_tokens"], 500 + 24576);
        assert!(
            v.get("temperature").is_none() && v.get("max_tokens").is_none(),
            "{v}"
        );
    }
    use super::*;
    use crate::wire::intel::ToolDef;

    fn req() -> Request {
        Request {
            model: "gpt-x".into(),
            messages: vec![Message::system("be terse"), Message::user("hi")],
            tools: vec![ToolDef {
                name: "read_file".into(),
                description: "read a file".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 256,
            temperature: Some(0.0),
            decision: None,
            reasoning: None,
            output_schema: None,
        }
    }

    #[test]
    fn reasoning_models_switch_the_token_limit_param() {
        // A reasoning model (gpt-5*/o-series) must use max_completion_tokens.
        let mut r = req();
        r.model = "gpt-5.1".into();
        let v: Value = serde_json::from_slice(&build_request(&r, None).0).unwrap();
        assert_eq!(v["max_completion_tokens"], 256);
        assert!(v.get("max_tokens").is_none(), "{v}");
        // An older / self-hosted model keeps the classic max_tokens (gpt-x here).
        let v: Value = serde_json::from_slice(&build_request(&req(), None).0).unwrap();
        assert_eq!(v["max_tokens"], 256);
        assert!(v.get("max_completion_tokens").is_none(), "{v}");
    }

    /// A schema-bearing request asks for the answer natively — strict when
    /// every property is required, beside the tools; a schema whose root is
    /// not an object, and a request with none, send no format.
    #[test]
    fn an_output_schema_is_asked_as_a_response_format() {
        let body =
            |r: &Request| -> Value { serde_json::from_slice(&build_request(r, None).0).unwrap() };
        let mut r = req();
        r.output_schema = Some(json!({"type": "object", "properties": {
            "achieved": {"type": "boolean"}, "reason": {"type": "string", "maxLength": 80}
        }, "required": ["achieved", "reason"]}));
        let v = body(&r);
        let f = &v["response_format"];
        assert_eq!(f["type"], "json_schema");
        assert_eq!(f["json_schema"]["name"], "answer");
        assert_eq!(f["json_schema"]["strict"], true);
        assert_eq!(
            f["json_schema"]["schema"]["additionalProperties"], false,
            "{v}"
        );
        assert!(
            f["json_schema"]["schema"]["properties"]["reason"]
                .get("maxLength")
                .is_none()
        );
        assert_eq!(v["tools"][0]["function"]["name"], "read_file");

        r.output_schema = Some(json!({"type": "object", "properties": {"a": {"type": "string"}}}));
        let v = body(&r);
        assert_eq!(v["response_format"]["json_schema"]["strict"], false);
        assert!(
            v["response_format"]["json_schema"]["schema"]
                .get("additionalProperties")
                .is_none(),
            "guidance is the schema as written: {v}"
        );

        r.output_schema = Some(json!({"type": "array"}));
        assert!(body(&r).get("response_format").is_none());
        assert!(body(&req()).get("response_format").is_none());
    }

    #[test]
    fn build_includes_tools_and_auth() {
        let (body, headers) = build_request(&req(), Some("sk-test"));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["model"], "gpt-x");
        assert_eq!(v["tools"][0]["function"]["name"], "read_file");
        assert_eq!(v["tool_choice"], "auto");
        assert!(
            headers
                .iter()
                .any(|(k, val)| k == "authorization" && val == "Bearer sk-test")
        );
    }

    #[test]
    fn parse_final_text() {
        let body = br#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.text.as_deref(), Some("done"));
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert_eq!(r.usage.total(), 12);
        assert!(!r.wants_tools());
    }

    #[test]
    fn parse_tool_call() {
        let body = br#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"/x\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let r = parse_response(body).unwrap();
        assert!(r.wants_tools());
        assert_eq!(r.tool_calls[0].name, "read_file");
        assert_eq!(r.tool_calls[0].arguments["path"], "/x");
        assert_eq!(r.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn parse_provider_error() {
        let body = br#"{"error":{"message":"invalid api key"}}"#;
        assert!(
            parse_response(body)
                .unwrap_err()
                .contains("invalid api key")
        );
    }
}
