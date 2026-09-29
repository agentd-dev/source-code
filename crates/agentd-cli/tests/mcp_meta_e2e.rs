// SPDX-License-Identifier: AGPL-3.0-only
//! Every `tools/call` agentd makes carries the run's `_meta` — `agent/run_id`,
//! `agent/instance` and a W3C `traceparent` — in `params._meta`, where MCP
//! reserves it, and the tool's `arguments` carry the tool's arguments alone.
//!
//! Every process that dials MCP is held to it: the reactor, which runs a
//! workflow's `mcp.tool` step on its own connection; the turn worker, which
//! runs an agent step's model calls on its; and a subagent PROCESS delegated
//! from that turn, on its own again. The mock MCP server is the arbiter: it
//! logs each call's params as they arrived, and refuses a `_meta` inside
//! `arguments` the way a strict (`additionalProperties: false`) tool schema
//! would.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// The mock LLM with a `file:` playbook; killed on drop.
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

/// The params of every `tools/call` that reached the mock, in arrival order.
fn calls(mock: &common::MockMcp) -> Vec<Value> {
    mock.log()
        .lines()
        .filter_map(|l| l.strip_prefix("MOCK_CALL "))
        .filter_map(|p| serde_json::from_str(p).ok())
        .collect()
}

#[test]
fn tool_calls_from_the_reactor_a_turn_and_a_subagent_carry_the_run_meta_in_params_meta() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    // The workflow's `probe` step calls `state.list` from the reactor. Its
    // `work` step is a turn: the model calls `mock.ops`, then delegates; the
    // child — identified by its own system prompt, which the turn's transcript
    // never contains — calls `search.query`. The rule for the child's second
    // round comes first: by then its transcript still carries the first
    // round's marker too.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "mock.ops", "arguments": {}}]},
            {"tool_calls": [{"name": "subagent.run", "arguments": {
                "instruction": "look something up",
                "mode": "sync"
            }}]},
            {"content": "delegated and done"}
        ],
        "match": [
            {"when_contains": "Result for zebra-child", "content": "child done"},
            {"when_contains": "You are agentd, an autonomous agent.",
             "tool_calls": [{"name": "search.query", "arguments": {"query": "zebra-child"}}]}
        ]
    }));
    let cfg = common::unique_path("agentd-mcp-meta", "yaml");
    std::fs::File::create(&cfg)
        .unwrap()
        .write_all(
            format!(
                "agent:\n  name: meta-probe\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nworkflows:\n  - name: probe\n    steps: {}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n",
                llm.uri,
                mock.uri(),
                json!({
                    "start": {"kind": "once"},
                    "probe": {"kind": "mcp.tool", "depends_on": ["start"], "server": "mock",
                              "tool": "state.list", "args": {"key": "meta-probe"}},
                    "work": {"kind": "agent", "depends_on": ["probe"], "instruction": "list and delegate"},
                    "done": {"kind": "finish", "depends_on": ["work"], "status": "completed"}
                })
            )
            .as_bytes(),
        )
        .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");

    let calls = calls(&mock);
    let by_tool = |tool: &str| -> Value {
        calls
            .iter()
            .find(|p| p["name"] == tool)
            .cloned()
            .unwrap_or_else(|| panic!("no {tool} call reached the mock: {calls:?}\n{stderr}"))
    };
    let from_reactor = by_tool("state.list");
    let from_turn = by_tool("mock.ops");
    let from_subagent = by_tool("search.query");

    let all = [
        ("reactor", &from_reactor),
        ("turn", &from_turn),
        ("subagent", &from_subagent),
    ];
    for (who, params) in all {
        let meta = &params["_meta"];
        assert!(
            meta["agent/run_id"].as_str().is_some_and(|s| !s.is_empty()),
            "{who}: no agent/run_id in params._meta: {params}"
        );
        assert!(
            meta["agent/instance"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "{who}: no agent/instance in params._meta: {params}"
        );
        let tp = meta["traceparent"].as_str().unwrap_or("");
        assert!(
            tp.starts_with("00-") && tp.len() == 55,
            "{who}: no W3C traceparent in params._meta: {params}"
        );
        assert!(
            params["arguments"].get("_meta").is_none(),
            "{who}: `_meta` rode inside the tool's arguments: {params}"
        );
    }
    // One run, one instance, one trace — whichever process made the call.
    let trace_of = |p: &Value| p["_meta"]["traceparent"].as_str().unwrap()[3..35].to_string();
    for (who, params) in &all[1..] {
        for key in ["agent/run_id", "agent/instance"] {
            assert_eq!(
                params["_meta"][key], from_reactor["_meta"][key],
                "{who}: {key} differs from the reactor's: {calls:?}"
            );
        }
        assert_eq!(
            trace_of(params),
            trace_of(&from_reactor),
            "{who}: {calls:?}"
        );
    }
    // The subagent's call arrived with exactly the arguments the model gave it.
    assert_eq!(from_subagent["arguments"], json!({"query": "zebra-child"}));
    let _ = std::fs::remove_file(&cfg);
}
