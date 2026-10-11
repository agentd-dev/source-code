// SPDX-License-Identifier: AGPL-3.0-only
//! Amazon Bedrock **Converse** API adapter. Pure translation, no I/O; the SigV4
//! signing that authenticates the dial is a separate axis
//! ([`crate::auth::aws`], applied by the transport in [`super::endpoints`]).
//!
//! Converse is Bedrock's provider-neutral chat surface, so agentd speaks ONE
//! dialect to every Bedrock model (Anthropic, Llama, Titan, …). It differs from
//! both in-binary dialects in ways the loop never sees:
//!   * the model id rides the **URL path** (`/model/{modelId}/converse`), not the
//!     body — so there is no `model` field here (see [`converse_path`]);
//!   * content is a list of blocks keyed by *shape* (`{"text":…}`,
//!     `{"toolUse":…}`, `{"toolResult":…}`) — no `type` tag;
//!   * `system` is a block list, inference knobs live under `inferenceConfig`,
//!     and tools under `toolConfig.tools[].toolSpec` with the JSON Schema nested
//!     one level as `inputSchema.json`;
//!   * Bedrock **validates strict user/assistant alternation**, so consecutive
//!     same-role turns (notably the N tool results of one assistant turn) are
//!     merged into a single message with N content blocks.
//!
//! Auth is SigV4 only (no bearer/api-key), so `token` is ignored here.

use crate::wire::intel::{Message, ReasoningParts, Request, Response, StopReason, ToolCall, Usage};
use serde_json::{Map, Value, json};

/// A placeholder default path. Bedrock's real path is computed per-request from
/// the model id ([`converse_path`]); this is only the host-only resolve-time
/// fallback and is always overridden before a dial.
pub const DEFAULT_PATH: &str = "/";
/// The dialect's name in a reasoning part's origin.
pub const NAME: &str = "bedrock";

/// The Converse request path for `model`: `/model/{modelId}/converse`. The model
/// id is a single opaque path parameter, so it is fully percent-encoded
/// (unreserved `A-Za-z0-9-._~` pass; everything else — notably the `:` of a
/// versioned id like `…-v2:0`, and the `:`/`/` of an inference-profile ARN —
/// becomes `%XX`). This exact string is BOTH sent on the wire and fed to the
/// SigV4 signer, so the canonical URI the signature covers matches the
/// request-target byte-for-byte (the signer does not re-encode).
pub fn converse_path(model: &str) -> String {
    format!("/model/{}/converse", encode_segment(model))
}

/// Percent-encode one path segment per RFC 3986, the way SigV4 canonicalisation
/// requires:
/// the unreserved set `A-Za-z0-9-._~` passes through, and every other byte —
/// including `/` and `:` — becomes an uppercase `%XX` escape. Uppercase hex is
/// mandatory: the signer compares the canonical URI byte-for-byte, so lowercase
/// escapes would produce a signature mismatch.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the Converse request body (JSON bytes) + headers. `token` is ignored —
/// Bedrock authenticates by SigV4 (added by the transport), never a bearer.
pub fn build_request(req: &Request, _token: Option<&str>) -> (Vec<u8>, Vec<(String, String)>) {
    // System turns are hoisted into the top-level `system` block list.
    let system: Vec<Value> = req
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::System(s) if !s.is_empty() => Some(json!({"text": s})),
            _ => None,
        })
        .collect();

    let mut inference = Map::new();
    // A reasoning model takes its family's own fields — a Claude model
    // adaptive thinking at an effort (or the budget form, when the model
    // names a `budget`), an OpenAI open-weight model `reasoning_effort` —
    // with `maxTokens` leaving room for the thinking, and no temperature.
    // Validation admits a reasoning model on Bedrock only from a family it
    // can tell ([`model_family`]).
    let mut additional = Map::new();
    match &req.reasoning {
        Some(r) => {
            match model_family(&req.model) {
                Some(Family::Anthropic) => match r.budget {
                    Some(budget) => {
                        additional.insert(
                            "thinking".into(),
                            json!({"type": "enabled", "budget_tokens": budget}),
                        );
                    }
                    None => {
                        additional.insert("thinking".into(), json!({"type": "adaptive"}));
                        additional
                            .insert("output_config".into(), json!({"effort": r.effort.as_str()}));
                    }
                },
                Some(Family::OpenAi) => {
                    additional.insert("reasoning_effort".into(), json!(r.effort.as_str()));
                }
                None => {}
            }
            inference.insert(
                "maxTokens".into(),
                json!(req.max_tokens as u64 + r.budget_tokens()),
            );
        }
        // A Claude model takes no temperature either: current ones refuse
        // it, as on Anthropic's own API.
        None => {
            inference.insert("maxTokens".into(), json!(req.max_tokens));
            if let Some(t) = req.temperature
                && model_family(&req.model) != Some(Family::Anthropic)
            {
                inference.insert("temperature".into(), json!(t));
            }
        }
    }

    let mut body = Map::new();
    if !additional.is_empty() {
        body.insert(
            "additionalModelRequestFields".into(),
            Value::Object(additional),
        );
    }
    body.insert(
        "messages".into(),
        json!(messages_to_bedrock(&req.messages, &req.model)),
    );
    if !system.is_empty() {
        body.insert("system".into(), json!(system));
    }
    body.insert("inferenceConfig".into(), Value::Object(inference));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({"toolSpec": {
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": {"json": t.input_schema},
                }})
            })
            .collect();
        body.insert("toolConfig".into(), json!({"tools": tools}));
    }

    let bytes = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
    // content-type/accept are unsigned (SigV4 covers host;x-amz-date only), so
    // they ride as ordinary headers; the signature is added by the transport.
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    (bytes, headers)
}

/// The model families agentd tells apart on Bedrock, for the fields each
/// takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `anthropic.claude-…` (or a cross-region profile `us.anthropic.…`).
    Anthropic,
    /// `openai.gpt-oss-…`.
    OpenAi,
}

/// The family a Bedrock model id names, when it names one agentd knows: the
/// provider segment, after an optional cross-region prefix (`us.`, `eu.`,
/// `global.`, …), of the id or of the ARN's resource. An application
/// inference profile names none — the model behind it cannot be told — and
/// is refused for a reasoning model at load rather than sent a field its
/// family rejects.
pub fn model_family(model: &str) -> Option<Family> {
    let m = model.to_ascii_lowercase();
    let id = m.rsplit('/').next().unwrap_or(&m);
    if id.starts_with("arn:") {
        return None;
    }
    id.split('.').take(2).find_map(|part| match part {
        "anthropic" => Some(Family::Anthropic),
        "openai" => Some(Family::OpenAi),
        _ => None,
    })
}

/// Translate neutral messages into Converse turns, **merging consecutive
/// same-role turns** (Bedrock validates strict user/assistant alternation): the
/// N tool results of one assistant turn collapse into a single user message with
/// N `toolResult` blocks, and an assistant's text + tool-use blocks share one
/// message.
fn messages_to_bedrock(messages: &[Message], model: &str) -> Vec<Value> {
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, block: Value| match turns.last_mut() {
        Some((r, blocks)) if *r == role => blocks.push(block),
        _ => turns.push((role, vec![block])),
    };
    for m in messages {
        match m {
            Message::System(_) => {} // hoisted into `system`
            Message::User(s) => push("user", json!({"text": s})),
            Message::Assistant {
                text,
                tool_calls,
                reasoning,
            } => {
                // This model's own message goes back as it wrote it — its
                // `reasoningContent` blocks among the rest, in order.
                if let Some(content) = ReasoningParts::for_model(reasoning, NAME, model) {
                    for block in content {
                        push("assistant", block.clone());
                    }
                    continue;
                }
                if let Some(t) = text.as_deref().filter(|t| !t.is_empty()) {
                    push("assistant", json!({"text": t}));
                }
                for tc in tool_calls {
                    push(
                        "assistant",
                        json!({"toolUse": {
                            "toolUseId": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        }}),
                    );
                }
            }
            Message::ToolResult {
                id,
                content,
                is_error,
            } => push(
                "user",
                json!({"toolResult": {
                    "toolUseId": id,
                    "content": [{"text": content}],
                    "status": if *is_error { "error" } else { "success" },
                }}),
            ),
        }
    }
    turns
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect()
}

/// Parse a Converse response body into the neutral [`Response`]. Tolerant:
/// missing usage → zero; an unknown stop reason → [`StopReason::Other`].
pub fn parse_response(body: &[u8]) -> Result<Response, String> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| format!("intel: bad JSON response: {e}"))?;

    // A 2xx Converse reply always carries `output.message`; a Bedrock error is a
    // non-2xx `{"message": …}` surfaced upstream as `IntelError::Http` before we
    // parse. Guard anyway so a stray error body reads clearly.
    let message = v.pointer("/output/message");
    if message.is_none()
        && let Some(msg) = v.get("message").and_then(Value::as_str)
    {
        return Err(format!("intel: provider error: {msg}"));
    }

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls = Vec::new();
    let mut thought = false;
    if let Some(blocks) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        for b in blocks {
            if b.get("reasoningContent").is_some() {
                thought = true;
            } else if let Some(t) = b.get("text").and_then(Value::as_str) {
                text_parts.push(t.to_string());
            } else if let Some(tu) = b.get("toolUse") {
                tool_calls.push(ToolCall {
                    id: tu
                        .get("toolUseId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    name: tu
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    arguments: tu.get("input").cloned().unwrap_or(Value::Null),
                });
            }
        }
    }

    let stop_reason = match v.get("stopReason").and_then(Value::as_str) {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };

    let usage = v.get("usage").map(|u| Usage {
        input_tokens: u.get("inputTokens").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: u.get("outputTokens").and_then(Value::as_u64).unwrap_or(0),
    });

    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    Ok(Response {
        text,
        tool_calls,
        stop_reason,
        usage: usage.unwrap_or_default(),
        fallback: None,
        decision: None,
        // A message that thought is kept whole, in order; the endpoint stamps
        // its origin.
        reasoning: thought.then(|| ReasoningParts {
            origin: String::new(),
            content: message
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::intel::ToolDef;

    /// On Bedrock a reasoning model takes its family's own fields: a Claude
    /// model adaptive thinking at an effort (or the budget form), an
    /// OpenAI open-weight model `reasoning_effort` — with `maxTokens` above
    /// the thinking and no temperature; a message that thought goes back
    /// whole to its own model alone.
    #[test]
    fn reasoning_goes_in_the_familys_own_field() {
        use crate::wire::intel::{Effort, ReasoningAsk};
        let ask = |model: &str, budget: Option<u64>| Request {
            model: model.into(),
            messages: vec![Message::user("hi")],
            max_tokens: 1000,
            temperature: Some(0.0),
            reasoning: Some(ReasoningAsk {
                effort: Effort::Medium,
                budget,
            }),
            ..Default::default()
        };
        let body =
            |r: &Request| -> Value { serde_json::from_slice(&build_request(r, None).0).unwrap() };
        let claude = body(&ask("us.anthropic.claude-sonnet-5-v1:0", None));
        assert_eq!(
            claude["additionalModelRequestFields"]["thinking"],
            json!({"type": "adaptive"})
        );
        assert_eq!(
            claude["additionalModelRequestFields"]["output_config"]["effort"],
            "medium"
        );
        assert_eq!(claude["inferenceConfig"]["maxTokens"], 9192);
        assert!(claude["inferenceConfig"].get("temperature").is_none());
        let budgeted = body(&ask("anthropic.claude-haiku-4-5-v1:0", Some(2048)));
        assert_eq!(
            budgeted["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            2048
        );
        let oss = body(&ask("openai.gpt-oss-120b-1:0", None));
        assert_eq!(
            oss["additionalModelRequestFields"]["reasoning_effort"],
            "medium"
        );
        assert_eq!(oss["inferenceConfig"]["maxTokens"], 9192);
        assert_eq!(
            model_family("arn:aws:bedrock:us-east-1:1:application-inference-profile/x"),
            None
        );
        assert_eq!(model_family("meta.llama3-70b-instruct-v1:0"), None);
        assert_eq!(
            model_family(
                "arn:aws:bedrock:us-gov-west-1:1:inference-profile/us-gov.anthropic.claude-x"
            ),
            Some(Family::Anthropic)
        );
        assert_eq!(
            model_family("apac.openai.gpt-oss-20b-1:0"),
            Some(Family::OpenAi)
        );

        let answer = json!({"output": {"message": {"role": "assistant", "content": [
            {"reasoningContent": {"reasoningText": {"text": "step one", "signature": "s"}}},
            {"toolUse": {"toolUseId": "t1", "name": "a", "input": {}}}
        ]}}, "stopReason": "tool_use"});
        let mut r = parse_response(answer.to_string().as_bytes()).unwrap();
        r.reasoning.as_mut().unwrap().origin = ReasoningParts::origin(NAME, "anthropic.claude-x");
        let msg = Message::Assistant {
            text: None,
            tool_calls: r.tool_calls.clone(),
            reasoning: r.reasoning.clone(),
        };
        let same = messages_to_bedrock(std::slice::from_ref(&msg), "anthropic.claude-x");
        assert!(same[0]["content"][0].get("reasoningContent").is_some());
        assert!(same[0]["content"][1].get("toolUse").is_some());
        let other = messages_to_bedrock(&[msg], "openai.gpt-oss-120b-1:0");
        assert!(other[0]["content"][0].get("toolUse").is_some(), "{other:?}");
    }

    #[test]
    fn converse_path_encodes_the_model_id() {
        // A versioned model id: the `:` becomes %3A (both wire + signature).
        assert_eq!(
            converse_path("anthropic.claude-3-5-sonnet-20241022-v2:0"),
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse"
        );
        // An inference-profile ARN: `:` and `/` both encode (single opaque param).
        assert_eq!(
            converse_path("arn:aws:bedrock:us-east-1::foundation-model/x"),
            "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A%3Afoundation-model%2Fx/converse"
        );
        // A plain id (no reserved chars) is unchanged.
        assert_eq!(
            converse_path("amazon.titan-text-express-v1"),
            "/model/amazon.titan-text-express-v1/converse"
        );
    }

    fn req() -> Request {
        Request {
            model: "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
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
        }
    }

    #[test]
    fn build_hoists_system_and_wraps_inference_and_tools() {
        let (body, headers) = build_request(&req(), Some("ignored"));
        let v: Value = serde_json::from_slice(&body).unwrap();
        // No `model` in the body — it rides the URL path.
        assert!(
            v.get("model").is_none(),
            "model must not be in the body: {v}"
        );
        assert_eq!(v["system"][0]["text"], "be terse");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(v["inferenceConfig"]["maxTokens"], 256);
        // A Claude model is sent no temperature; another family keeps it.
        assert!(v["inferenceConfig"].get("temperature").is_none(), "{v}");
        let mut llama = req();
        llama.model = "meta.llama3-70b-instruct-v1:0".into();
        let v: Value = serde_json::from_slice(&build_request(&llama, None).0).unwrap();
        assert_eq!(v["inferenceConfig"]["temperature"], 0.0);
        assert_eq!(v["toolConfig"]["tools"][0]["toolSpec"]["name"], "read_file");
        assert_eq!(
            v["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
            "object"
        );
        // No bearer/api-key header — Bedrock authenticates by SigV4 only.
        assert!(
            !headers
                .iter()
                .any(|(k, _)| k == "authorization" || k == "x-api-key"),
            "no bearer header for Bedrock: {headers:?}"
        );
    }

    #[test]
    fn build_omits_toolconfig_when_no_tools() {
        let mut r = req();
        r.tools.clear();
        let v: Value = serde_json::from_slice(&build_request(&r, None).0).unwrap();
        assert!(
            v.get("toolConfig").is_none(),
            "empty tools ⇒ no toolConfig: {v}"
        );
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        // Two tool calls in one assistant turn → two ToolResults. Bedrock needs
        // strict alternation: they must collapse into a SINGLE user message with
        // two toolResult blocks (not two consecutive user messages).
        let r = Request {
            model: "m".into(),
            messages: vec![
                Message::user("go"),
                Message::Assistant {
                    text: Some("working".into()),
                    tool_calls: vec![
                        ToolCall {
                            id: "t1".into(),
                            name: "a".into(),
                            arguments: json!({}),
                        },
                        ToolCall {
                            id: "t2".into(),
                            name: "b".into(),
                            arguments: json!({}),
                        },
                    ],
                    reasoning: None,
                },
                Message::tool_result("t1", "r1", false),
                Message::tool_result("t2", "r2", true),
            ],
            tools: vec![],
            max_tokens: 8,
            temperature: None,
            decision: None,
            reasoning: None,
        };
        let v: Value = serde_json::from_slice(&build_request(&r, None).0).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        // user("go") | assistant(text+2 toolUse) | user(2 toolResult) == 3 turns.
        assert_eq!(msgs.len(), 3, "roles must alternate: {v}");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["text"], "working");
        assert_eq!(msgs[1]["content"][1]["toolUse"]["toolUseId"], "t1");
        assert_eq!(msgs[1]["content"][2]["toolUse"]["name"], "b");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["toolResult"]["toolUseId"], "t1");
        assert_eq!(msgs[2]["content"][0]["toolResult"]["status"], "success");
        assert_eq!(msgs[2]["content"][1]["toolResult"]["status"], "error");
    }

    #[test]
    fn parse_text_and_tool_use() {
        let body = br#"{"output":{"message":{"role":"assistant","content":[
            {"text":"hi"},
            {"toolUse":{"toolUseId":"tu_1","name":"read","input":{"p":1}}}
        ]}},"stopReason":"tool_use","usage":{"inputTokens":5,"outputTokens":7,"totalTokens":12}}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.text.as_deref(), Some("hi"));
        assert_eq!(r.tool_calls[0].id, "tu_1");
        assert_eq!(r.tool_calls[0].name, "read");
        assert_eq!(r.tool_calls[0].arguments["p"], 1);
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.usage.total(), 12);
    }

    #[test]
    fn parse_final_text_and_stop_reasons() {
        let body = br#"{"output":{"message":{"content":[{"text":"done"}]}},"stopReason":"end_turn","usage":{"inputTokens":10,"outputTokens":2}}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.text.as_deref(), Some("done"));
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert!(!r.wants_tools());
        // max_tokens maps through.
        let body =
            br#"{"output":{"message":{"content":[{"text":"x"}]}},"stopReason":"max_tokens"}"#;
        assert_eq!(
            parse_response(body).unwrap().stop_reason,
            StopReason::MaxTokens
        );
    }
}
