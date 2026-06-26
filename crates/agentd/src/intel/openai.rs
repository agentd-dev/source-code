//! OpenAI-compatible `/chat/completions` adapter with native tool-calling.
//! RFC 0006 §canonical-wire.
//!
//! Canonical because it covers vLLM / Ollama / LM Studio / most hosted
//! gateways and gives the model first-class `tools` + `tool_calls`. This
//! module is pure translation: neutral [`Request`] → OpenAI JSON, and OpenAI
//! JSON → neutral [`Response`]. No I/O (that's `intel/client.rs`).

use crate::wire::intel::{Message, Request, Response, StopReason, ToolCall, Usage};
use serde_json::{Map, Value, json};

/// The default endpoint path when the intelligence URI is a bare socket
/// (`unix:`/`vsock:`) rather than a full `https://…` URL.
pub const DEFAULT_PATH: &str = "/v1/chat/completions";

/// Build the request body (JSON bytes) and the HTTP headers for a chat
/// completion. `token`, if present, becomes `Authorization: Bearer …`.
pub fn build_request(req: &Request, token: Option<&str>) -> (Vec<u8>, Vec<(String, String)>) {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("max_tokens".into(), json!(req.max_tokens));
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
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
            Message::Assistant { text, tool_calls } => {
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
    })
}

#[cfg(test)]
mod tests {
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
        }
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
