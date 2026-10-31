// SPDX-License-Identifier: AGPL-3.0-only
//! OpenAI Responses API adapter (`POST /v1/responses`).
//!
//! OpenAI's newest models take function tools only here: on
//! `/v1/chat/completions` they refuse a request that carries `tools`. The
//! module is pure translation — neutral [`Request`] → Responses JSON,
//! Responses JSON → neutral [`Response`] — and performs no I/O; the socket,
//! the retries and the failover belong to `intel/endpoints.rs`, exactly as
//! for the chat dialect.
//!
//! Differences from Chat Completions, absorbed here:
//! - system messages become the top-level `instructions`;
//! - an assistant's tool calls are `function_call` input items and a tool's
//!   result a `function_call_output` item, joined by `call_id`;
//! - tools are flat `{type: function, name, description, parameters}`;
//! - a reasoning model's effort is `reasoning.effort`, its cap
//!   `max_output_tokens`, and the answer's schema `text.format`;
//! - the answer is the `output[]` item list: `message` (`output_text`
//!   parts), `function_call` and `reasoning` items.
//!
//! Every request is sent with `store: false`, so nothing is kept at OpenAI
//! between calls. A reasoning model's `reasoning` items are therefore asked
//! for with their `encrypted_content` and, while the same turn is still
//! calling tools, the whole output of that call goes back to the same model
//! verbatim — as Anthropic's thinking blocks do (RFC 0046 §5.7).

use crate::wire::intel::{Message, ReasoningParts, Request, Response, StopReason, ToolCall, Usage};
use serde_json::{Map, Value, json};

/// The default path when the endpoint URL carries none.
pub const DEFAULT_PATH: &str = "/v1/responses";
/// The dialect's name — in `intelligence.dialect` and in a reasoning part's
/// origin.
pub const NAME: &str = "openai-responses";

/// Whether the default `openai` dialect asks `model`, on the endpoint whose
/// `Host` is `host`, on the Responses API — the one rule it routes by
/// ([`super::client::Provider`]). Two tiers:
///
/// - **anywhere**, a model that cannot take function tools on Chat
///   Completions ([`required_by`]): chat would refuse it, so Responses is
///   the only call that can work;
/// - **on OpenAI's own API** (`api.openai.com`), every `gpt-5` model and
///   later as well — Responses is OpenAI's current API for them, and keeps
///   their reasoning across a turn's tool rounds.
///
/// Any other host is taken for an OpenAI-compatible server — vLLM, Ollama,
/// a gateway, Azure — which mostly speaks Chat Completions alone, so a model
/// chat still serves stays on chat there.
pub fn routes_to(model: &str, host: &str) -> bool {
    required_by(model)
        || (is_openai_api(host)
            && gpt_generation(&model.to_ascii_lowercase()).is_some_and(|(major, _)| major >= 5))
}

/// Whether `host` (a `Host` header, port and all) is OpenAI's own API.
fn is_openai_api(host: &str) -> bool {
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
    name.eq_ignore_ascii_case("api.openai.com")
}

/// Whether `model` must be asked on the Responses API: OpenAI families that
/// refuse function tools on Chat Completions. `gpt-5.6` and later
/// (`gpt-5.N`, N ≥ 6, and `gpt-6`… — every later family by default), the
/// `-pro` models of the `o1`, `o3` and `gpt-5` families, and the `codex`
/// models.
pub fn required_by(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains("codex") {
        return true;
    }
    if (m.starts_with("o1-pro") || m.starts_with("o3-pro"))
        || (m.starts_with("gpt-5") && m.contains("-pro"))
    {
        return true;
    }
    gpt_generation(&m).is_some_and(|(major, minor)| major > 5 || (major == 5 && minor >= 6))
}

/// `(major, minor)` of a `gpt-<major>[.<minor>]…` name; minor 0 when absent.
pub(super) fn gpt_generation(lower: &str) -> Option<(u32, u32)> {
    let rest = lower.strip_prefix("gpt-")?;
    let digits = |s: &str| -> (Option<u32>, usize) {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        (s[..n].parse().ok(), n)
    };
    let (major, n) = digits(rest);
    let major = major?;
    let minor = rest[n..]
        .strip_prefix('.')
        .and_then(|r| digits(r).0)
        .unwrap_or(0);
    Some((major, minor))
}

/// The Responses path beside a Chat Completions path — `…/chat/completions`
/// → `…/responses` — so a gateway mount carries over; `None` when the path
/// is not a Chat Completions path, and nothing can be inferred from it.
pub fn path_beside(chat_path: &str) -> Option<String> {
    chat_path
        .strip_suffix("/chat/completions")
        .map(|base| format!("{base}/responses"))
}

/// Build the request body (JSON bytes) and the HTTP headers. `token`, if
/// present, becomes `Authorization: Bearer …`.
pub fn build_request(req: &Request, token: Option<&str>) -> (Vec<u8>, Vec<(String, String)>) {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("store".into(), json!(false));

    let instructions: String = req
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::System(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions));
    }
    body.insert(
        "input".into(),
        json!(input_items(&req.messages, &req.model)),
    );

    match &req.reasoning {
        // A reasoning model takes its effort and no temperature; its
        // thinking counts against `max_output_tokens`, so the cap leaves the
        // effort's room above the answer. Its reasoning items come back
        // encrypted, to be returned to it while the turn calls tools.
        Some(r) => {
            body.insert(
                "max_output_tokens".into(),
                json!(req.max_tokens as u64 + r.budget_tokens()),
            );
            body.insert("reasoning".into(), json!({"effort": r.effort.as_str()}));
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
        None => {
            body.insert("max_output_tokens".into(), json!(req.max_tokens));
            if let Some(t) = super::openai::temperature_for(&req.model, req.temperature) {
                body.insert("temperature".into(), json!(t));
            }
            // A model of a reasoning family thinks whether asked or not;
            // what it thought is kept across the turn's tool rounds all
            // the same.
            if super::openai::reasoning_family(&req.model) {
                body.insert("include".into(), json!(["reasoning.encrypted_content"]));
            }
        }
    }

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                // `strict: false`: an MCP input schema is guidance, not a
                // closed schema the strict decoder accepts.
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                    "strict": false,
                })
            })
            .collect();
        body.insert("tools".into(), json!(tools));
        body.insert("tool_choice".into(), json!("auto"));
    }
    // The answer's schema, as the chat dialect asks it: strict where the
    // schema allows, else as guidance; beside the tools.
    if let Some((schema, strict)) = req
        .output_schema
        .as_ref()
        .and_then(super::structured::openai)
    {
        body.insert(
            "text".into(),
            json!({"format": {"type": "json_schema", "name": "answer", "schema": schema, "strict": strict}}),
        );
    }

    let bytes = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(tok) = token {
        headers.push(("authorization".to_string(), format!("Bearer {tok}")));
    }
    (bytes, headers)
}

/// The conversation as Responses input items. System messages are the
/// `instructions` and are left out here.
fn input_items(messages: &[Message], model: &str) -> Vec<Value> {
    let mut items = Vec::new();
    for m in messages {
        match m {
            Message::System(_) => {}
            Message::User(s) => items.push(json!({"role": "user", "content": s})),
            Message::Assistant {
                text,
                tool_calls,
                reasoning,
            } => {
                // This model's own output goes back as it came — reasoning
                // items with the call they led to, in order.
                if let Some(output) = ReasoningParts::for_model(reasoning, NAME, model) {
                    items.extend(output.iter().cloned());
                    continue;
                }
                if let Some(t) = text.as_deref().filter(|t| !t.is_empty()) {
                    items.push(json!({"role": "assistant", "content": t}));
                }
                for tc in tool_calls {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.name,
                        // Arguments ride as a JSON *string*, as on chat.
                        "arguments": serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".into()),
                    }));
                }
            }
            Message::ToolResult {
                id,
                content,
                is_error,
            } => {
                // No error flag on a function output: prefixed, as on chat.
                let output = if *is_error {
                    format!("ERROR: {content}")
                } else {
                    content.clone()
                };
                items
                    .push(json!({"type": "function_call_output", "call_id": id, "output": output}));
            }
        }
    }
    items
}

/// Parse a Responses body into the neutral [`Response`]. Tolerant as the
/// chat dialect is: missing usage → zero, arguments that are not JSON →
/// `{"_raw": …}`, an unknown status → [`StopReason::Other`].
pub fn parse_response(body: &[u8]) -> Result<Response, String> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| format!("intel: bad JSON response: {e}"))?;

    // A success carries `"error": null`; only an error object is one.
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(format!("intel: provider error: {msg}"));
    }
    if v.get("status").and_then(Value::as_str) == Some("failed") {
        return Err("intel: provider error: the response failed".into());
    }
    let output = v
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| "intel: response has no output".to_string())?;

    let mut texts: Vec<&str> = Vec::new();
    let mut tool_calls = Vec::new();
    let mut reasoned = false;
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                texts.push(t);
                            }
                        }
                        // A refusal is the model's answer: shown, not lost.
                        Some("refusal") => {
                            if let Some(t) = part.get("refusal").and_then(Value::as_str) {
                                texts.push(t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("");
                let raw_args = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                tool_calls.push(ToolCall {
                    id: s("call_id").to_string(),
                    name: s("name").to_string(),
                    arguments: serde_json::from_str(raw_args)
                        .unwrap_or_else(|_| json!({ "_raw": raw_args })),
                });
            }
            Some("reasoning") => reasoned = true,
            _ => {}
        }
    }
    let text = Some(texts.concat()).filter(|t| !t.is_empty());

    let incomplete = v
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str);
    let stop_reason = match v.get("status").and_then(Value::as_str) {
        _ if !tool_calls.is_empty() => StopReason::ToolUse,
        Some("completed") => StopReason::EndTurn,
        Some("incomplete") if incomplete == Some("max_output_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };

    // `output_tokens` counts the reasoning tokens too
    // (`output_tokens_details.reasoning_tokens` is a part of it).
    let usage = v.get("usage").map(|u| Usage {
        input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
    });

    // A call that reasoned keeps its whole output — the reasoning items and
    // the items they led to — to send back to the same model while the turn
    // calls tools; the endpoint stamps its origin.
    let reasoning = reasoned.then(|| ReasoningParts {
        origin: String::new(),
        content: output.iter().map(input_form).collect(),
    });

    Ok(Response {
        text,
        tool_calls,
        stop_reason,
        usage: usage.unwrap_or_default(),
        fallback: None,
        decision: None,
        reasoning,
    })
}

/// An output item as it is sent back as input: as returned, without its
/// `status` on a reasoning item (output-only there).
fn input_form(item: &Value) -> Value {
    let mut item = item.clone();
    if item.get("type").and_then(Value::as_str) == Some("reasoning")
        && let Some(o) = item.as_object_mut()
    {
        o.remove("status");
    }
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::intel::{Effort, ReasoningAsk, ToolDef};

    fn req() -> Request {
        Request {
            model: "gpt-5.6-sol".into(),
            messages: vec![Message::system("be terse"), Message::user("hi")],
            tools: vec![ToolDef {
                name: "read_file".into(),
                description: "read a file".into(),
                input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            max_tokens: 256,
            ..Default::default()
        }
    }

    fn body(r: &Request) -> Value {
        serde_json::from_slice(&build_request(r, Some("sk-test")).0).unwrap()
    }

    #[test]
    fn the_models_that_need_responses_are_named_by_family() {
        for m in [
            "gpt-5.6-sol",
            "gpt-5.6",
            "GPT-5.7-mini",
            "gpt-5.10",
            "gpt-6",
            "gpt-6.1-nano",
            "o1-pro",
            "o3-pro-2025-06-10",
            "gpt-5-pro",
            "gpt-5.5-pro",
            "gpt-5-codex",
            "codex-mini-latest",
        ] {
            assert!(required_by(m), "{m} is asked on Responses");
        }
        for m in [
            "gpt-5.5",
            "gpt-5",
            "gpt-5.1",
            "gpt-5-mini",
            "gpt-4o",
            "gpt-4.1",
            "o3",
            "o4-mini",
            "llama-3.1-70b",
            "mock",
            "gpt-",
        ] {
            assert!(!required_by(m), "{m} stays on chat");
        }
    }

    /// The routing rule's two tiers: a model chat refuses tools for goes to
    /// Responses on any host; on OpenAI's own API every gpt-5 model and later
    /// does too, and an older one stays on chat.
    #[test]
    fn the_default_dialect_routes_by_model_and_host() {
        const OPENAI: &str = "api.openai.com";
        for m in [
            "gpt-6.1-sol",
            "gpt-6.1-terra",
            "gpt-6-sol",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-7",
        ] {
            assert!(routes_to(m, OPENAI), "{m} on OpenAI");
            assert!(routes_to(m, "127.0.0.1:8000"), "{m} anywhere");
            assert!(routes_to(m, "gw.example"), "{m} anywhere");
        }
        for m in ["gpt-5", "gpt-5.5", "gpt-5-mini", "GPT-5.1"] {
            assert!(routes_to(m, OPENAI), "{m} on OpenAI");
            assert!(routes_to(m, "API.OPENAI.COM:443"), "{m} on OpenAI");
            assert!(!routes_to(m, "vllm.internal:8000"), "{m} elsewhere");
            assert!(!routes_to(m, "api.openai.com.evil.example"), "{m}");
        }
        for m in ["gpt-4o", "gpt-4.1", "o3", "o4-mini", "llama-3.1-70b"] {
            assert!(!routes_to(m, OPENAI), "{m} stays on chat");
        }
    }

    #[test]
    fn the_responses_path_sits_beside_the_chat_path() {
        assert_eq!(
            path_beside("/v1/chat/completions").as_deref(),
            Some("/v1/responses")
        );
        assert_eq!(
            path_beside("/gw/openai/v1/chat/completions").as_deref(),
            Some("/gw/openai/v1/responses")
        );
        assert_eq!(path_beside("/v1/generate"), None);
    }

    #[test]
    fn a_request_is_instructions_input_tools_and_no_store() {
        let v = body(&req());
        assert_eq!(v["model"], "gpt-5.6-sol");
        assert_eq!(v["store"], false);
        assert_eq!(v["instructions"], "be terse");
        assert_eq!(v["input"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(
            v["tools"][0],
            json!({"type": "function", "name": "read_file", "description": "read a file",
                   "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                   "strict": false})
        );
        assert_eq!(v["tool_choice"], "auto");
        assert_eq!(v["max_output_tokens"], 256);
        assert!(v.get("messages").is_none() && v.get("max_tokens").is_none());
        // A reasoning family keeps its reasoning across tool rounds.
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
        let (_, headers) = build_request(&req(), Some("sk-test"));
        assert!(
            headers
                .iter()
                .any(|(k, val)| k == "authorization" && val == "Bearer sk-test")
        );
    }

    /// Prior tool calls and their results go back as `function_call` and
    /// `function_call_output` items, joined by `call_id`.
    #[test]
    fn a_tool_round_goes_back_as_function_call_items() {
        let mut r = req();
        r.messages.push(Message::assistant(
            Some("looking".into()),
            vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "/x"}),
            }],
        ));
        r.messages
            .push(Message::tool_result("call_1", "contents", false));
        r.messages
            .push(Message::tool_result("call_2", "nope", true));
        let v = body(&r);
        let input = v["input"].as_array().unwrap();
        assert_eq!(input[1], json!({"role": "assistant", "content": "looking"}));
        assert_eq!(
            input[2],
            json!({"type": "function_call", "call_id": "call_1", "name": "read_file",
                   "arguments": "{\"path\":\"/x\"}"})
        );
        assert_eq!(
            input[3],
            json!({"type": "function_call_output", "call_id": "call_1", "output": "contents"})
        );
        assert_eq!(input[4]["output"], "ERROR: nope");
    }

    #[test]
    fn a_reasoning_request_carries_its_effort_and_no_temperature() {
        let mut r = req();
        r.model = "o-mini".into();
        r.temperature = Some(0.0);
        r.reasoning = Some(ReasoningAsk {
            effort: Effort::High,
            budget: None,
        });
        let v = body(&r);
        assert_eq!(v["reasoning"], json!({"effort": "high"}));
        assert_eq!(v["max_output_tokens"], 256 + 24576);
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
        assert!(v.get("temperature").is_none(), "{v}");
    }

    /// A temperature is sent only when one is set, and never to a model of
    /// an OpenAI reasoning family.
    #[test]
    fn temperature_only_when_set_and_never_to_a_reasoning_family() {
        let mut r = req();
        r.model = "gpt-4.1".into();
        assert!(body(&r).get("temperature").is_none());
        r.temperature = Some(0.2);
        let t = body(&r)["temperature"].as_f64().unwrap();
        assert!((t - 0.2).abs() < 1e-6);
        assert!(body(&r).get("include").is_none());
        for m in ["gpt-5.5", "gpt-5.6-sol", "o3", "o4-mini"] {
            r.model = m.into();
            assert!(body(&r).get("temperature").is_none(), "{m}");
        }
    }

    #[test]
    fn an_output_schema_is_asked_as_a_text_format() {
        let mut r = req();
        r.output_schema = Some(json!({"type": "object", "properties": {
            "achieved": {"type": "boolean"}, "reason": {"type": "string", "maxLength": 80}
        }, "required": ["achieved", "reason"]}));
        let v = body(&r);
        let f = &v["text"]["format"];
        assert_eq!(f["type"], "json_schema");
        assert_eq!(f["name"], "answer");
        assert_eq!(f["strict"], true);
        assert_eq!(f["schema"]["additionalProperties"], false);
        assert!(
            f["schema"]["properties"]["reason"]
                .get("maxLength")
                .is_none()
        );
        assert_eq!(v["tools"][0]["name"], "read_file");

        r.output_schema = Some(json!({"type": "object", "properties": {"a": {"type": "string"}}}));
        assert_eq!(body(&r)["text"]["format"]["strict"], false);
        r.output_schema = Some(json!({"type": "array"}));
        assert!(body(&r).get("text").is_none());
        assert!(body(&req()).get("text").is_none());
    }

    #[test]
    fn parse_final_text_and_usage() {
        let b = br#"{"id":"resp_1","object":"response","status":"completed","error":null,
            "incomplete_details":null,
            "output":[{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"enc"},
                      {"type":"message","id":"msg_1","role":"assistant","status":"completed",
                       "content":[{"type":"output_text","text":"do","annotations":[]},
                                  {"type":"output_text","text":"ne","annotations":[]}]}],
            "usage":{"input_tokens":10,"output_tokens":7,
                     "output_tokens_details":{"reasoning_tokens":5}}}"#;
        let r = parse_response(b).unwrap();
        assert_eq!(r.text.as_deref(), Some("done"));
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert_eq!(r.usage.input_tokens, 10);
        assert_eq!(r.usage.output_tokens, 7, "reasoning tokens are output");
        assert!(!r.wants_tools());
        let parts = r.reasoning.expect("the reasoning is kept");
        assert_eq!(parts.content.len(), 2);
        assert_eq!(parts.content[0]["encrypted_content"], "enc");
    }

    #[test]
    fn parse_function_call() {
        let b = br#"{"status":"completed","error":null,"output":[
            {"type":"function_call","id":"fc_1","call_id":"call_9","name":"read_file",
             "arguments":"{\"path\":\"/x\"}","status":"completed"},
            {"type":"function_call","id":"fc_2","call_id":"call_10","name":"bad","arguments":"not json"}]}"#;
        let r = parse_response(b).unwrap();
        assert!(r.wants_tools());
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.tool_calls[0].id, "call_9");
        assert_eq!(r.tool_calls[0].name, "read_file");
        assert_eq!(r.tool_calls[0].arguments["path"], "/x");
        assert_eq!(r.tool_calls[1].arguments["_raw"], "not json");
        assert!(r.reasoning.is_none(), "nothing reasoned, nothing kept");
        assert_eq!(r.usage, Usage::default());
    }

    #[test]
    fn parse_incomplete_and_errors() {
        let b = br#"{"status":"incomplete","error":null,"incomplete_details":{"reason":"max_output_tokens"},"output":[]}"#;
        assert_eq!(
            parse_response(b).unwrap().stop_reason,
            StopReason::MaxTokens
        );
        let b = br#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#;
        assert!(parse_response(b).unwrap_err().contains("invalid api key"));
        let b = br#"{"status":"failed","error":null,"output":[]}"#;
        assert!(parse_response(b).is_err());
        assert!(parse_response(b"not json").is_err());
    }

    /// The model's own output goes back verbatim — its reasoning items with
    /// the call they led to — and to no other model.
    #[test]
    fn a_reasoning_models_output_goes_back_to_it_alone() {
        let output = vec![
            json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"}),
            json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read_file", "arguments": "{}"}),
        ];
        let mut r = req();
        r.messages.push(Message::Assistant {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({}),
            }],
            reasoning: Some(ReasoningParts {
                origin: ReasoningParts::origin(NAME, "gpt-5.6-sol"),
                content: output.clone(),
            }),
        });
        r.messages.push(Message::tool_result("call_1", "ok", false));
        let input = body(&r)["input"].as_array().unwrap().clone();
        assert_eq!(input[1..3], output[..]);
        assert_eq!(input[3]["type"], "function_call_output");

        r.model = "gpt-5.7".into();
        let input = body(&r)["input"].as_array().unwrap().clone();
        assert_eq!(input[1]["type"], "function_call");
        assert!(input[1].get("id").is_none());
        assert!(input.iter().all(|i| i["type"] != "reasoning"));
    }
}

/// The dialect over a real socket: a local mock Responses server, dialed by
/// the intelligence client exactly as an OpenAI endpoint would be.
#[cfg(test)]
mod over_http {
    use super::*;
    use crate::intel::client::{IntelClient, IntelError};
    use crate::wire::intel::{Effort, ReasoningAsk, ToolDef};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// What the mock saw: each request's path and JSON body.
    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    /// Serve `answers` — `(status, body)` — one per connection, in order,
    /// recording every request. Returns the `http://127.0.0.1:<port>` base.
    fn serve(answers: Vec<(u16, Value)>) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Seen = Arc::default();
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for (status, answer) in answers {
                let Ok((s, _)) = listener.accept() else {
                    break;
                };
                let mut r = BufReader::new(s);
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).unwrap();
                record
                    .lock()
                    .unwrap()
                    .push((path, serde_json::from_slice(&body).unwrap_or(Value::Null)));
                let text = answer.to_string();
                let mut s = r.into_inner();
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = s.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    fn ask(model: &str) -> Request {
        Request {
            model: model.into(),
            messages: vec![Message::system("be terse"), Message::user("read /x")],
            tools: vec![ToolDef {
                name: "resource.read".into(),
                description: "read a resource".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 512,
            ..Default::default()
        }
    }

    fn completed(output: Value, usage: Value) -> Value {
        json!({"id": "resp_1", "object": "response", "status": "completed", "error": null,
               "incomplete_details": null, "output": output, "usage": usage})
    }

    fn says(text: &str) -> Value {
        completed(
            json!([{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": text, "annotations": []}]}]),
            json!({"input_tokens": 1, "output_tokens": 1}),
        )
    }

    /// gpt-5.6-sol on the default dialect is asked on `/v1/responses`: it
    /// calls a tool, the result goes back with its reasoning beside the
    /// call, and it answers — the usage of each call counted.
    #[test]
    fn a_tool_call_round_trip_on_the_default_dialect() {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [],
                               "encrypted_content": "enc-1", "status": "completed"});
        let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1",
                          "name": "resource_read", "arguments": "{\"uri\":\"/x\"}", "status": "completed"});
        let (base, seen) = serve(vec![
            (
                200,
                completed(
                    json!([reasoning, call]),
                    json!({"input_tokens": 40, "output_tokens": 30,
                           "output_tokens_details": {"reasoning_tokens": 22}}),
                ),
            ),
            (
                200,
                completed(
                    json!([{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                            "content": [{"type": "output_text", "text": "it says hi", "annotations": []}]}]),
                    json!({"input_tokens": 90, "output_tokens": 5}),
                ),
            ),
        ]);
        let client = IntelClient::from_parts(&base, Some("sk-mock".into())).unwrap();
        let mut req = ask("gpt-5.6-sol");
        let first = client.complete(&req).unwrap();
        assert_eq!(first.stop_reason, StopReason::ToolUse);
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(
            first.tool_calls[0].name, "resource.read",
            "routed by the original name"
        );
        assert_eq!(first.tool_calls[0].id, "call_1");
        assert_eq!(first.tool_calls[0].arguments["uri"], "/x");
        assert_eq!(first.usage.input_tokens, 40);
        assert_eq!(first.usage.output_tokens, 30);
        let parts = first.reasoning.clone().expect("its reasoning is kept");
        assert_eq!(parts.origin, ReasoningParts::origin(NAME, "gpt-5.6-sol"));

        req.messages.push(Message::Assistant {
            text: None,
            tool_calls: first.tool_calls.clone(),
            reasoning: first.reasoning.clone(),
        });
        req.messages
            .push(Message::tool_result("call_1", "hi", false));
        let second = client.complete(&req).unwrap();
        assert_eq!(second.text.as_deref(), Some("it says hi"));
        assert_eq!(second.stop_reason, StopReason::EndTurn);
        assert_eq!(second.usage.total(), 95);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let (path, body) = &seen[0];
        assert_eq!(path, "/v1/responses");
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["tools"][0]["name"], "resource_read");
        assert!(body.get("temperature").is_none(), "{body}");
        let input = seen[1].1["input"].as_array().unwrap();
        assert_eq!(input[0], json!({"role": "user", "content": "read /x"}));
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], "enc-1");
        assert!(input[1].get("status").is_none());
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(
            input[3],
            json!({"type": "function_call_output", "call_id": "call_1", "output": "hi"})
        );
    }

    /// A reasoning model's effort, and a structured answer, reach the wire.
    #[test]
    fn reasoning_effort_and_a_structured_answer() {
        let (base, seen) = serve(vec![(200, says("{\"ok\":true}"))]);
        let client = IntelClient::from_parts(&base, None)
            .unwrap()
            .with_dialect(Some(NAME))
            .with_reasoning(Some(ReasoningAsk {
                effort: Effort::Low,
                budget: None,
            }));
        let mut req = ask("gpt-5.6-sol");
        req.tools.clear();
        req.output_schema = Some(
            json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]}),
        );
        let resp = client.complete(&req).unwrap();
        assert_eq!(resp.text.as_deref(), Some("{\"ok\":true}"));
        let seen = seen.lock().unwrap();
        let body = &seen[0].1;
        assert_eq!(body["reasoning"], json!({"effort": "low"}));
        assert_eq!(body["max_output_tokens"], 512 + 2048);
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["strict"], true);
        assert!(body.get("tools").is_none());
    }

    /// Errors map as on the chat dialect: a 400 is returned at once, with
    /// the provider's message; a 429 is retried in place; a 401 is auth.
    #[test]
    fn errors_map_like_the_chat_dialect() {
        let refusal = json!({"error": {"message": "Unsupported parameter: 'x'", "type": "invalid_request_error"}});
        let (base, seen) = serve(vec![(400, refusal)]);
        let client = IntelClient::from_parts(&base, None).unwrap();
        match client.complete(&ask("gpt-5.6-sol")) {
            Err(IntelError::Http(400, body)) => {
                assert!(body.contains("Unsupported parameter"), "{body}")
            }
            other => panic!("want a 400, got {other:?}"),
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "a 400 is not retried");

        let (base, seen) = serve(vec![
            (429, json!({"error": {"message": "slow down"}})),
            (200, says("ok")),
        ]);
        let client = IntelClient::from_parts(&base, None).unwrap();
        assert_eq!(
            client
                .complete(&ask("gpt-5.6-sol"))
                .unwrap()
                .text
                .as_deref(),
            Some("ok")
        );
        assert_eq!(seen.lock().unwrap().len(), 2, "a 429 is retried");

        let (base, _) = serve(vec![(401, json!({"error": {"message": "bad key"}}))]);
        let client = IntelClient::from_parts(&base, None).unwrap();
        let e = client.complete(&ask("gpt-5.6-sol")).unwrap_err();
        assert!(crate::intel::failover::is_auth(&e), "{e:?}");

        // A 200 that carries an error object is a failure, not an answer.
        let (base, _) = serve(vec![(
            200,
            json!({"error": {"message": "server had an error"}, "output": []}),
        )]);
        let client = IntelClient::from_parts(&base, None).unwrap();
        assert!(matches!(
            client.complete(&ask("gpt-5.6-sol")),
            Err(IntelError::Parse(m)) if m.contains("server had an error")
        ));
    }

    /// The routing rule: the default dialect sends a model that needs
    /// Responses there, beside the configured chat path, and every other
    /// model to chat; `openai-chat` and `openai-responses` are fixed.
    #[test]
    fn the_dialect_and_path_each_request_is_sent_on() {
        let chat = json!({"choices": [{"message": {"content": "c"}, "finish_reason": "stop"}]});
        let resp = says("r");
        let case = |dialect: Option<&str>, path: &str, model: &str| -> (String, String) {
            let answer = if path.ends_with("route") || dialect == Some("openai-chat") {
                chat.clone()
            } else if dialect == Some(NAME) || required_by(model) {
                resp.clone()
            } else {
                chat.clone()
            };
            let (base, seen) = serve(vec![(200, answer)]);
            let client = IntelClient::from_parts(&format!("{base}{path}"), None)
                .unwrap()
                .with_dialect(dialect);
            let text = client
                .complete(&ask(model))
                .unwrap()
                .text
                .unwrap_or_default();
            let path = seen.lock().unwrap()[0].0.clone();
            (path, text)
        };
        let r = |p: &str| (p.to_string(), "r".to_string());
        let c = |p: &str| (p.to_string(), "c".to_string());
        assert_eq!(case(None, "", "gpt-5.6-sol"), r("/v1/responses"));
        assert_eq!(
            case(Some("openai"), "", "gpt-4o"),
            c("/v1/chat/completions")
        );
        assert_eq!(case(None, "", "gpt-5.5"), c("/v1/chat/completions"));
        assert_eq!(
            case(None, "/gw/openai/v1/chat/completions", "gpt-5.6-sol"),
            r("/gw/openai/v1/responses")
        );
        assert_eq!(
            case(None, "/custom/route", "gpt-5.6-sol"),
            c("/custom/route")
        );
        assert_eq!(
            case(Some("openai-chat"), "", "gpt-5.6-sol"),
            c("/v1/chat/completions")
        );
        assert_eq!(case(Some(NAME), "", "gpt-4o"), r("/v1/responses"));
        assert_eq!(case(Some(NAME), "/v1/proxy", "gpt-4o"), r("/v1/proxy"));
    }

    /// A model's configured temperature is sent where the model takes one,
    /// and no temperature is sent to one that refuses it.
    #[test]
    fn a_configured_temperature_reaches_only_the_models_that_take_one() {
        let (base, seen) = serve(vec![(200, says("ok")), (200, says("ok"))]);
        let client = IntelClient::from_parts(&base, None)
            .unwrap()
            .with_dialect(Some(NAME))
            .with_temperature(Some(0.3));
        client.complete(&ask("gpt-4.1")).unwrap();
        client.complete(&ask("gpt-5.6-sol")).unwrap();
        let seen = seen.lock().unwrap();
        let t = seen[0].1["temperature"].as_f64().unwrap();
        assert!((t - 0.3).abs() < 1e-6, "{}", seen[0].1);
        assert!(seen[1].1.get("temperature").is_none(), "{}", seen[1].1);
    }
}
