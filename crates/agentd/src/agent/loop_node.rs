//! `agent_loop` — a bounded agentic step inside the graph
//! (RFC 0006 §2, Mode 2).
//!
//! The model receives instructions, the running transcript, and a
//! catalogue of *only the tools the node declared*. Each turn it
//! must answer with exactly one JSON object:
//!
//! ```json
//! {"action": "tool", "tool": "read_file", "args": {"path": "..."}}
//! {"action": "final", "result": <any json>}
//! ```
//!
//! Containment, in order of authority:
//!
//! 1. **Tool subset** — proposals outside `tools` are refused (the
//!    refusal is fed back; the model may adapt).
//! 2. **Policy** — every execution goes through the same
//!    [`Policy`]/budget gates the declared node kinds use. A loop
//!    cannot reach anything `[policy]` would deny a regular node.
//! 3. **Step cap** — `max_steps` (required, hard ceiling 64). Cap
//!    exhaustion ends the node on the `"exhausted"` branch.
//! 4. **Token budget** — `max_tokens` accumulated across turns.
//! 5. **Run deadline** — checked every turn.
//!
//! Malformed model output and failed tool calls are *recoverable*:
//! the error text goes back into the conversation (consuming a
//! step). Every turn emits `agentd::audit` events.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::budget::BudgetRef;
use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::intelligence::backends::BackendMap;
use crate::intelligence::client::IntelligenceClient;
use crate::intelligence::protocol::{Message, Request};
use crate::mcp::client::McpClient as _;
use crate::observability::Metrics;
use crate::tools::policy::{Decision, PolicyRef};
use crate::workflow::{Node, NodeKind};
use std::sync::Arc;

/// Hard ceiling on `max_steps` regardless of what the node declares.
pub const MAX_STEPS_CEILING: u32 = 64;

/// Tool names a loop may declare. Feature-gated members surface a
/// CapabilityUnavailable error at execution if the family isn't
/// compiled in — same contract as registry dispatch.
pub const LOOP_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "read_env",
    "http_request",
    "shell_run",
    "call_mcp_tool",
];

#[allow(clippy::too_many_arguments)]
pub fn register(
    registry: &mut HandlerRegistry,
    backends: BackendMap,
    policy: PolicyRef,
    budget: BudgetRef,
    mcp: Option<crate::mcp::McpRegistryRef>,
    system: Option<String>,
    metrics: Arc<Metrics>,
) {
    registry.register(
        "agent_loop",
        Box::new(AgentLoopHandler {
            backends,
            broker: ToolBroker {
                policy,
                budget: budget.clone(),
                mcp,
            },
            run_budget: budget,
            metrics,
            system,
        }),
    );
}

pub struct AgentLoopHandler {
    backends: BackendMap,
    broker: ToolBroker,
    /// Run-wide budget — the loop's per-turn token usage counts
    /// against the same cumulative cap as llm_infer (RFC 0006 §5).
    run_budget: BudgetRef,
    metrics: Arc<Metrics>,
    /// Standing system prompt from `--instructions`, if any.
    system: Option<String>,
}

impl NodeHandler for AgentLoopHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::AgentLoop {
            backend,
            instructions,
            instructions_from,
            tools,
            max_steps,
            max_tokens,
        } = &node.kind
        else {
            return Err(Error::Tool {
                tool: "agent_loop".into(),
                reason: format!(
                    "handler for `agent_loop` received node `{}` of kind `{}`",
                    node.id,
                    node.kind.name()
                ),
            });
        };

        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({ "result": null, "steps": 0, "dry_run": true }),
                branch: None,
            });
        }

        let client = self.backends.get(backend.as_str()).ok_or_else(|| {
            Error::Intelligence(format!(
                "agent_loop node `{}`: backend `{backend}` is not configured",
                node.id
            ))
        })?;

        // Resolve instructions: inline wins, else a context path.
        let task = match (instructions, instructions_from) {
            (Some(s), _) => s.clone(),
            (None, Some(path)) => match ctx.resolve_path(path) {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => {
                    return Err(Error::Tool {
                        tool: "agent_loop".into(),
                        reason: format!(
                            "instructions_from `{path}` is not set in the execution context"
                        ),
                    });
                }
            },
            (None, None) => unreachable!("validator enforces instructions presence"),
        };

        let cap = (*max_steps).clamp(1, MAX_STEPS_CEILING);
        let mut messages = vec![Message {
            role: "system".into(),
            content: system_prompt(self.system.as_deref(), tools),
        }];
        messages.push(Message {
            role: "user".into(),
            content: task,
        });

        let mut transcript: Vec<Value> = Vec::new();
        let mut tokens_spent: u64 = 0;

        tracing::info!(
            target: "agentd::audit",
            event = "loop.started",
            node_id = %node.id,
            backend = %backend,
            max_steps = cap,
        );

        for step in 1..=cap {
            if std::time::Instant::now() >= ctx.deadline {
                return Err(Error::Timeout(std::time::Duration::ZERO));
            }
            if let Some(budget_cap) = max_tokens {
                if tokens_spent >= u64::from(*budget_cap) {
                    tracing::warn!(
                        target: "agentd::audit",
                        event = "loop.token_budget_exhausted",
                        node_id = %node.id,
                        tokens_spent,
                    );
                    return Ok(exhausted(transcript, step - 1, "token budget exhausted"));
                }
            }

            if let Err(reason) = self.run_budget.check_llm_budget() {
                tracing::warn!(
                    target: "agentd::audit",
                    event = "loop.run_budget_exhausted",
                    node_id = %node.id,
                    reason = %reason,
                );
                return Ok(exhausted(
                    transcript,
                    step - 1,
                    "run token budget exhausted",
                ));
            }
            let request = Request {
                model: "fast".into(),
                messages: messages.clone(),
                max_tokens: None,
                temperature: None,
            };
            let response = client.complete(&request)?;
            let turn_tokens =
                u64::from(response.usage.prompt_tokens + response.usage.completion_tokens);
            tokens_spent += turn_tokens;
            self.run_budget.add_llm_tokens(turn_tokens);
            self.metrics.add_llm(turn_tokens);
            messages.push(Message {
                role: "assistant".into(),
                content: response.content.clone(),
            });

            match parse_action(&response.content) {
                Ok(Action::Final(result)) => {
                    tracing::info!(
                        target: "agentd::audit",
                        event = "loop.final",
                        node_id = %node.id,
                        steps = step,
                        tokens = tokens_spent,
                    );
                    transcript.push(json!({ "step": step, "action": "final" }));
                    return Ok(NodeOutcome::Continue {
                        value: json!({
                            "result": result,
                            "steps": step,
                            "tokens": tokens_spent,
                            "transcript": transcript,
                        }),
                        branch: None,
                    });
                }
                Ok(Action::Tool { tool, args }) => {
                    let outcome = if !tools.iter().any(|t| t == &tool) {
                        json!({ "error": format!(
                            "tool `{tool}` is not in this loop's allowed set: {}",
                            tools.join(", ")
                        )})
                    } else {
                        match self.broker.execute(&tool, &args, ctx) {
                            Ok(v) => v,
                            Err(e) => json!({ "error": format!("{e}") }),
                        }
                    };
                    tracing::info!(
                        target: "agentd::audit",
                        event = "loop.tool_call",
                        node_id = %node.id,
                        step,
                        tool = %tool,
                        ok = outcome.get("error").is_none(),
                    );
                    transcript.push(json!({
                        "step": step,
                        "action": "tool",
                        "tool": tool,
                        "args": args,
                        "ok": outcome.get("error").is_none(),
                    }));
                    messages.push(Message {
                        role: "user".into(),
                        content: json!({ "tool_result": outcome }).to_string(),
                    });
                }
                Err(parse_err) => {
                    transcript.push(json!({
                        "step": step,
                        "action": "malformed",
                    }));
                    messages.push(Message {
                        role: "user".into(),
                        content: json!({
                            "error": format!(
                                "your reply was not a valid action object: {parse_err}. \
                                 Reply with exactly one JSON object."
                            )
                        })
                        .to_string(),
                    });
                }
            }
        }

        tracing::warn!(
            target: "agentd::audit",
            event = "loop.steps_exhausted",
            node_id = %node.id,
            max_steps = cap,
        );
        Ok(exhausted(transcript, cap, "step cap exhausted"))
    }
}

fn exhausted(transcript: Vec<Value>, steps: u32, reason: &str) -> NodeOutcome {
    NodeOutcome::Continue {
        value: json!({
            "result": null,
            "reason": reason,
            "steps": steps,
            "transcript": transcript,
        }),
        branch: Some("exhausted".into()),
    }
}

// ---------------------------------------------------------------------------
// Action protocol
// ---------------------------------------------------------------------------

enum Action {
    Tool { tool: String, args: Value },
    Final(Value),
}

/// Parse the model's reply. Tolerates surrounding prose / code
/// fences by extracting the first top-level `{...}` object.
fn parse_action(raw: &str) -> std::result::Result<Action, String> {
    let candidate = extract_json_object(raw).ok_or("no JSON object found")?;
    let v: Value = serde_json::from_str(candidate).map_err(|e| format!("invalid JSON: {e}"))?;
    match v["action"].as_str() {
        Some("tool") => {
            let tool = v["tool"]
                .as_str()
                .ok_or("`tool` must be a string")?
                .to_string();
            Ok(Action::Tool {
                tool,
                args: v.get("args").cloned().unwrap_or(json!({})),
            })
        }
        Some("final") => Ok(Action::Final(
            v.get("result").cloned().unwrap_or(Value::Null),
        )),
        _ => Err("`action` must be \"tool\" or \"final\"".into()),
    }
}

/// First balanced top-level JSON object in `raw`.
fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn system_prompt(standing: Option<&str>, tools: &[String]) -> String {
    let mut out = String::new();
    if let Some(s) = standing {
        out.push_str(s);
        out.push_str("\n\n");
    }
    out.push_str(
        "You operate a bounded tool loop. Each turn, reply with exactly ONE \
         JSON object and nothing else:\n\
         {\"action\":\"tool\",\"tool\":\"<name>\",\"args\":{...}} — to use a tool\n\
         {\"action\":\"final\",\"result\":<json>} — when the task is done\n\n\
         Available tools:\n",
    );
    for t in tools {
        out.push_str(match t.as_str() {
            "read_file" => "- read_file {path}: read a UTF-8 file\n",
            "write_file" => "- write_file {path, content}: write a file (parents created)\n",
            "read_env" => "- read_env {key}: read an environment variable\n",
            "http_request" => "- http_request {method, url, body?}: plain-HTTP request\n",
            "shell_run" => "- shell_run {command, args?}: run an allowlisted binary (absolute path, argv array)\n",
            "call_mcp_tool" => "- call_mcp_tool {tool, args?, server?}: invoke an MCP tool\n",
            other => {
                out.push_str(&format!("- {other}\n"));
                continue;
            }
        });
    }
    out.push_str(
        "\nTool results arrive as {\"tool_result\": ...}; errors as \
         {\"error\": ...} — adapt or finish. You have a limited number \
         of steps; be economical.",
    );
    out
}

// ---------------------------------------------------------------------------
// Tool broker — the same gates the declared node kinds use
// ---------------------------------------------------------------------------

struct ToolBroker {
    policy: PolicyRef,
    budget: BudgetRef,
    mcp: Option<crate::mcp::McpRegistryRef>,
}

impl ToolBroker {
    fn execute(&self, tool: &str, args: &Value, ctx: &mut ExecutionContext) -> Result<Value> {
        match tool {
            "read_file" => {
                let path = req_str(args, "path")?;
                let path = PathBuf::from(path);
                deny_to_err("read_file", self.policy.check_fs_read(&path))?;
                let content = std::fs::read_to_string(&path).map_err(|e| Error::Tool {
                    tool: "read_file".into(),
                    reason: format!("read {}: {e}", path.display()),
                })?;
                Ok(json!({ "path": path.display().to_string(), "content": content }))
            }
            "write_file" => {
                let path = PathBuf::from(req_str(args, "path")?);
                let content = req_str(args, "content")?;
                deny_to_err("write_file", self.policy.check_fs_write(&path))?;
                if let Err(reason) = self.budget.check_fs_write(content.len() as u64) {
                    return Err(Error::Tool {
                        tool: "write_file".into(),
                        reason,
                    });
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Error::Tool {
                        tool: "write_file".into(),
                        reason: format!("mkdir_p {}: {e}", parent.display()),
                    })?;
                }
                std::fs::write(&path, content.as_bytes()).map_err(|e| Error::Tool {
                    tool: "write_file".into(),
                    reason: format!("write {}: {e}", path.display()),
                })?;
                Ok(json!({ "path": path.display().to_string(), "bytes": content.len() }))
            }
            "read_env" => {
                let key = req_str(args, "key")?;
                deny_to_err("read_env", self.policy.check_env_read(key))?;
                match std::env::var(key) {
                    Ok(v) => Ok(json!({ "key": key, "value": v })),
                    Err(_) => Ok(json!({ "key": key, "value": null, "missing": true })),
                }
            }
            "http_request" => {
                #[cfg(feature = "tools-http")]
                {
                    let method = req_str(args, "method")?.to_ascii_uppercase();
                    let url = req_str(args, "url")?;
                    deny_to_err("http_request", self.policy.check_http_request(&method, url))?;
                    let body = args.get("body").and_then(|b| match b {
                        Value::String(s) => Some(s.clone().into_bytes()),
                        Value::Null => None,
                        other => Some(other.to_string().into_bytes()),
                    });
                    crate::tools::http::perform_for_loop(
                        &method,
                        url,
                        body.as_deref(),
                        ctx.outbound_traceparent().as_deref(),
                    )
                }
                #[cfg(not(feature = "tools-http"))]
                {
                    let _ = ctx;
                    Err(Error::CapabilityUnavailable(
                        "http_request requires the `tools-http` Cargo feature".into(),
                    ))
                }
            }
            "shell_run" => {
                #[cfg(feature = "tools-shell")]
                {
                    let command = PathBuf::from(req_str(args, "command")?);
                    if !command.is_absolute() {
                        return Err(Error::Tool {
                            tool: "shell_run".into(),
                            reason: "shell_run requires an absolute path".into(),
                        });
                    }
                    let canonical = std::fs::canonicalize(&command).map_err(|e| Error::Tool {
                        tool: "shell_run".into(),
                        reason: format!("resolve {}: {e}", command.display()),
                    })?;
                    deny_to_err("shell_run", self.policy.check_shell_run(&canonical))?;
                    let shell_args: Vec<String> = args
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    crate::tools::shell::run_for_loop(&canonical, &shell_args)
                }
                #[cfg(not(feature = "tools-shell"))]
                Err(Error::CapabilityUnavailable(
                    "shell_run requires the `tools-shell` Cargo feature".into(),
                ))
            }
            "call_mcp_tool" => {
                let Some(registry) = &self.mcp else {
                    return Err(Error::Mcp("no MCP servers are configured".into()));
                };
                let name = req_str(args, "tool")?;
                let server = args.get("server").and_then(Value::as_str);
                let handle = registry.resolve(server)?;
                if !handle.allowlist.tool_allowed(name) {
                    return Err(Error::Policy(format!(
                        "mcp tool `{name}` is not allowlisted on server `{}`",
                        handle.name
                    )));
                }
                let call_args = args.get("args").cloned().unwrap_or(json!({}));
                let result = handle.client.call_tool(name, call_args)?;
                Ok(json!({
                    "tool": name,
                    "content": result.content,
                    "structured": result.structured_content,
                    "is_error": result.is_error,
                }))
            }
            other => Err(Error::CapabilityUnavailable(format!(
                "`{other}` is not a loop tool (known: {})",
                LOOP_TOOLS.join(", ")
            ))),
        }
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Tool {
            tool: "agent_loop".into(),
            reason: format!("tool args missing string field `{key}`"),
        })
}

fn deny_to_err(tool: &str, d: Decision) -> Result<()> {
    match d {
        Decision::Allow => Ok(()),
        Decision::Deny(reason) => Err(Error::Policy(format!("{tool} denied: {reason}"))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};
    use crate::intelligence::backends::single_backend;
    use crate::intelligence::client::MockClient;
    use std::sync::Arc;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(json!({})),
            &RunOptions::default(),
        )
    }

    fn handler(mock: Arc<MockClient>) -> AgentLoopHandler {
        AgentLoopHandler {
            backends: single_backend(mock),
            broker: ToolBroker {
                policy: crate::tools::policy::allow_all(),
                budget: crate::budget::unbounded(),
                mcp: None,
            },
            run_budget: crate::budget::unbounded(),
            metrics: crate::observability::Metrics::new(),
            system: Some("standing orders".into()),
        }
    }

    fn loop_node(tools: &[&str], max_steps: u32) -> Node {
        Node {
            id: "loop".into(),
            retry: None,
            kind: NodeKind::AgentLoop {
                backend: "default".into(),
                instructions: Some("do the thing".into()),
                instructions_from: None,
                tools: tools.iter().map(|s| s.to_string()).collect(),
                max_steps,
                max_tokens: None,
            },
        }
    }

    #[test]
    fn tool_then_final_round_trip() {
        let key = "AGENTD_LOOP_TEST_VALUE";
        unsafe { std::env::set_var(key, "42") };
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text(format!(
            r#"{{"action":"tool","tool":"read_env","args":{{"key":"{key}"}}}}"#
        ));
        mock.enqueue_text(r#"{"action":"final","result":{"answer":42}}"#);

        let h = handler(mock.clone());
        let mut c = ctx();
        let out = h.handle(&loop_node(&["read_env"], 4), &mut c).unwrap();
        unsafe { std::env::remove_var(key) };

        match out {
            NodeOutcome::Continue { value, branch } => {
                assert!(branch.is_none());
                assert_eq!(value["result"]["answer"], 42);
                assert_eq!(value["steps"], 2);
                assert_eq!(value["transcript"].as_array().unwrap().len(), 2);
            }
            other => panic!("{other:?}"),
        }

        // The model saw the tool result on its second turn.
        let received = mock.received();
        assert_eq!(received.len(), 2);
        let second = &received[1];
        let last = &second.messages[second.messages.len() - 1];
        assert!(last.content.contains("tool_result"), "{}", last.content);
        assert!(last.content.contains("42"));
        // System prompt carries standing orders + the tool catalogue.
        assert!(second.messages[0].content.contains("standing orders"));
        assert!(second.messages[0].content.contains("read_env"));
    }

    #[test]
    fn undeclared_tool_is_refused_and_recoverable() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text(
            r#"{"action":"tool","tool":"write_file","args":{"path":"/x","content":"y"}}"#,
        );
        mock.enqueue_text(r#"{"action":"final","result":"gave up"}"#);

        let h = handler(mock.clone());
        let mut c = ctx();
        let out = h.handle(&loop_node(&["read_env"], 4), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, branch } => {
                assert!(branch.is_none());
                assert_eq!(value["result"], "gave up");
            }
            _ => panic!(),
        }
        // The refusal reached the model.
        let received = mock.received();
        let last = &received[1].messages[received[1].messages.len() - 1];
        assert!(last.content.contains("not in this loop's allowed set"));
    }

    #[test]
    fn policy_denial_feeds_back_not_aborts() {
        struct NoEnv;
        impl crate::tools::policy::Policy for NoEnv {
            fn check_env_read(&self, _: &str) -> Decision {
                Decision::Deny("sealed".into())
            }
        }
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text(r#"{"action":"tool","tool":"read_env","args":{"key":"HOME"}}"#);
        mock.enqueue_text(r#"{"action":"final","result":"ok"}"#);

        let mut h = handler(mock.clone());
        h.broker.policy = Arc::new(NoEnv);
        let mut c = ctx();
        let out = h.handle(&loop_node(&["read_env"], 4), &mut c).unwrap();
        assert!(matches!(out, NodeOutcome::Continue { branch: None, .. }));
        let received = mock.received();
        let last = &received[1].messages[received[1].messages.len() - 1];
        assert!(last.content.contains("denied"), "{}", last.content);
    }

    #[test]
    fn step_cap_exhausts_on_branch() {
        let mock = Arc::new(MockClient::new());
        for _ in 0..3 {
            mock.enqueue_text(r#"{"action":"tool","tool":"read_env","args":{"key":"PATH"}}"#);
        }
        let h = handler(mock);
        let mut c = ctx();
        let out = h.handle(&loop_node(&["read_env"], 3), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, branch } => {
                assert_eq!(branch.as_deref(), Some("exhausted"));
                assert_eq!(value["steps"], 3);
                assert_eq!(value["reason"], "step cap exhausted");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn malformed_reply_is_recoverable() {
        let mock = Arc::new(MockClient::new());
        mock.enqueue_text("I think I should probably read a file?");
        mock.enqueue_text(r#"{"action":"final","result":"recovered"}"#);
        let h = handler(mock.clone());
        let mut c = ctx();
        let out = h.handle(&loop_node(&["read_env"], 4), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => assert_eq!(value["result"], "recovered"),
            _ => panic!(),
        }
        let received = mock.received();
        let last = &received[1].messages[received[1].messages.len() - 1];
        assert!(last.content.contains("not a valid action object"));
    }

    #[test]
    fn dry_run_makes_no_calls() {
        let mock = Arc::new(MockClient::new());
        let h = handler(mock.clone());
        let mut c = ctx();
        c.dry_run = true;
        let out = h.handle(&loop_node(&["read_env"], 4), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => assert_eq!(value["dry_run"], true),
            _ => panic!(),
        }
        assert!(mock.received().is_empty());
    }

    #[test]
    fn extract_json_handles_fences_and_prose() {
        assert_eq!(
            extract_json_object("```json\n{\"a\":1}\n```").unwrap(),
            "{\"a\":1}"
        );
        assert_eq!(
            extract_json_object("sure! {\"a\":{\"b\":\"}\"}} trailing").unwrap(),
            "{\"a\":{\"b\":\"}\"}}"
        );
        assert!(extract_json_object("no json here").is_none());
    }
}
