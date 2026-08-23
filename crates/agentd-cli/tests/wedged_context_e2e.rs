// SPDX-License-Identifier: AGPL-3.0-only
//! The durable context stays **self-consistent** when a turn ends inside the
//! tool-call loop (RFC 0026 §3.2).
//!
//! Every provider dialect requires one tool result per `tool_calls` id on the
//! preceding assistant message. The turn worker pushes the assistant message
//! first and the results one by one, so an exit taken between them (loop
//! detection here; cancellation is the other) used to persist an assistant
//! message with unanswered call ids. That context is DURABLE: it is replayed by
//! every later turn and by every restart, and the provider rejects it with a
//! fatal 400 forever — surfaced as a retryable `intel:` failure, so an external
//! scheduler retries a request agentd itself malformed.
//!
//! This drives a real daemon (mock LLM + mock MCP checkpointer as the store),
//! makes the model emit four identical calls in ONE assistant message so loop
//! detection fires on the fourth, then reads the context back OUT OF THE STORE
//! over the wire and checks the pairing invariant on what was actually written.

mod common;

use agentd::mcp::client::McpClient;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

struct MockLlm {
    child: std::process::Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}

fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

fn write_file(tag: &str, ext: &str, body: &str) -> String {
    let path = common::unique_path(tag, ext);
    std::fs::File::create(&path)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    path
}

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// Every context record the instance checkpointed, read back through the mock
/// checkpointer's own `state.list` / `state.get` — the bytes a restart would
/// restore from, not the runtime's in-memory copy.
fn stored_contexts(client: &McpClient) -> Vec<(String, Value)> {
    let call = |tool: &str, args: Value| -> Value {
        let res = client
            .call_tool_with_meta_within(tool, Some(args), json!({}), Duration::from_secs(5))
            .expect("checkpointer call");
        serde_json::from_str(&res.text()).unwrap_or(Value::Null)
    };
    let listed = call("state.list", json!({"prefix": "agentd/"}));
    listed["keys"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|k| k["key"].as_str().map(str::to_string))
        .filter(|k| k.contains("/context/"))
        .map(|k| {
            let env = call("state.get", json!({"key": k}));
            (k, env["state"]["state"].clone())
        })
        .collect()
}

#[test]
fn a_turn_that_ends_inside_the_tool_loop_leaves_no_unanswered_tool_call_in_the_store() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    // One assistant message, four IDENTICAL calls: loop detection trips on the
    // fourth (`LOOP_REPEATS`), i.e. inside the tool loop with call 4 never run.
    let call = json!({"name": "memory.set", "arguments": {"key": "k", "value": 1}});
    let llm = spawn_mock_llm(&json!({"turns": [
        {"tool_calls": [call, call, call, call]},
        {"content": "unreachable"}
    ]}));
    // A `manual` workflow so no run starts on its own; the conversation turn
    // comes from the injected inbox event, and `run_until: idle` ends the life
    // once it has been folded into the context and checkpointed.
    let cfg = write_file(
        "agentd-wedge",
        "yaml",
        &format!(
            "config_version: \"1\"\nagent:\n  name: wedge\n  instruction: You help the team.\n  preflight: never\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nstore:\n  kind: mcp\n  mcp:\n    server: mock\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n",
            llm.uri,
            mock.uri()
        ),
    );
    let inbox = write_file(
        "inbox",
        "json",
        &json!([{"kind": "a2a_message", "principal": "user:alice", "payload": {"context_id": "c1", "text": "set the key"}}])
            .to_string(),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_TEST_INBOX_FILE", &inbox)
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The premise: the turn really did end inside the tool loop. Without this
    // the pairing assertion below would hold vacuously.
    let done = events(&stderr, "turn.done");
    assert!(
        done.iter().any(|e| e["status"] == "loop_detected"),
        "the turn must end inside the tool loop: {stderr}"
    );

    let mut client =
        McpClient::connect("ckpt", &mock.uri(), vec![], Duration::from_secs(5)).expect("connect");
    client.initialize().expect("initialize");
    let contexts = stored_contexts(&client);
    assert!(
        !contexts.is_empty(),
        "the context was checkpointed: {stderr}"
    );
    let mut saw_calls = false;
    for (key, ctx) in &contexts {
        let msgs = ctx["messages"].as_array().cloned().unwrap_or_default();
        let answered: BTreeSet<&str> = msgs
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["id"].as_str())
            .collect();
        for m in msgs.iter().filter(|m| m["role"] == "assistant") {
            for tc in m["tool_calls"].as_array().cloned().unwrap_or_default() {
                saw_calls = true;
                let id = tc["id"].as_str().unwrap_or_default();
                assert!(
                    answered.contains(id),
                    "{key}: tool_call {id} ({}) has no tool result — the stored context is wedged: {}",
                    tc["name"],
                    serde_json::to_string_pretty(&msgs).unwrap()
                );
            }
        }
    }
    assert!(
        saw_calls,
        "the stored context must carry the assistant message with tool calls: {contexts:?}"
    );
}
