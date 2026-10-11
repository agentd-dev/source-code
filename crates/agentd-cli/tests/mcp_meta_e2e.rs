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
//!
//! At both revisions agentd reaches. At 2026-07-28 the run's keys sit beside
//! the revision's own `_meta`, and the turn worker — handed its tools, never
//! listing them itself — still sends the `Mcp-Param-*` header an annotated
//! tool declares. At 2025-11-25 only the supervisor probes with
//! `server/discover`: every process it starts dials the way it reached the
//! server.

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
    let mock = probe("2025-11-25");
    // Worker seeding: the supervisor alone probes with `server/discover`
    // (which this server refuses); the turn worker and the subagent it
    // delegates to start with `initialize`, as the supervisor reached it.
    assert_eq!(mock.discovers(), 1, "{}", mock.log());
    assert_eq!(mock.initializes(), 3, "{}", mock.log());
}

#[test]
fn tool_calls_from_the_reactor_a_turn_and_a_subagent_carry_the_run_meta_in_params_meta_at_2026_07_28()
 {
    let mock = probe("2026-07-28");
    // Every call's `_meta` holds the revision's own keys beside the run's.
    for params in calls(&mock) {
        assert_eq!(
            params["_meta"]["io.modelcontextprotocol/protocolVersion"], "2026-07-28",
            "{params}"
        );
    }
    // The turn worker called the annotated tool, with the header its
    // argument travels in, and the server took it.
    let regional = calls(&mock)
        .into_iter()
        .find(|p| p["name"] == "mock.regional")
        .unwrap_or_else(|| panic!("no mock.regional call: {}", mock.log()));
    assert_eq!(regional["headers"]["mcp-param-region"], "eu", "{regional}");
    assert_eq!(regional["arguments"], json!({"region": "eu"}), "{regional}");
    assert_eq!(mock.initializes(), 0, "{}", mock.log());
    assert_eq!(mock.refused(), 0, "{}", mock.log());
}

/// Run the probe workflow against a mock speaking `protocol_version`, assert
/// what every process's calls carried, and hand back the mock.
fn probe(protocol_version: &str) -> common::MockMcp {
    let mock = common::spawn_mock_mcp_at(protocol_version, "mock://noop", false);
    let modern = protocol_version == "2026-07-28";
    // The workflow's `probe` step calls `state.list` from the reactor. Its
    // `work` step is a turn: the model calls `mock.ops` (and, at 2026-07-28,
    // `mock.regional`, whose argument travels as a header), then delegates;
    // the child — identified by its own system prompt, which the turn's
    // transcript never contains — calls the server's `search.query`, which it
    // names `mock.search.query` as the registry does (an agentd contract holds
    // the bare name). The rule for the
    // child's second round comes first: by then its transcript still carries
    // the first round's marker too.
    let mut first = vec![json!({"name": "mock.ops", "arguments": {}})];
    if modern {
        first.push(json!({"name": "mock.regional", "arguments": {"region": "eu"}}));
    }
    // A turn is picked by how many tool results the transcript holds, so the
    // delegation is the turn after every call of the first one.
    let mut turns = vec![json!({"tool_calls": first})];
    if modern {
        turns.push(json!({"content": "unreached"}));
    }
    turns.push(
        json!({"tool_calls": [{"name": "subagent.run", "arguments": {
            "instruction": "look something up",
            "mode": "sync"
        }}]}),
    );
    turns.push(json!({"content": "delegated and done"}));
    let llm = spawn_mock_llm(&json!({
        "turns": turns,
        "match": [
            {"when_contains": "Result for zebra-child", "content": "child done"},
            {"when_contains": "You are agentd, an autonomous agent.",
             "tool_calls": [{"name": "mock.search.query", "arguments": {"query": "zebra-child"}}]}
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
    mock
}

/// A turn's `agent/idempotency_key` does not repeat across the lives of an
/// instance. Turn ids begin again in every process, so two runs of the same
/// prompt sent the same key, and a server that dedupes on it would have
/// answered the second call with the first one's result.
#[test]
fn a_turns_idempotency_key_does_not_repeat_across_restarts() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&json!({"turns": [
        {"tool_calls": [{"name": "mock.ops", "arguments": {}}]},
        {"content": "counted"}
    ]}));
    // The same run id in both lives, as an operator who pins it to the unit
    // of work has: the key is still each life's own.
    let life = || {
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args([
                "--prompt",
                "count the operations",
                "--intelligence",
                &llm.uri,
                "--model",
                "mock",
                "--mcp",
                &format!("mock={}", mock.uri()),
                "--run-id",
                "pinned-run",
            ])
            .stdin(Stdio::null())
            .output()
            .expect("run agentd");
        let log = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(out.status.code(), Some(0), "{log}");
    };
    life();
    life();
    let keys: Vec<String> = calls(&mock)
        .into_iter()
        .filter(|p| p["name"] == "mock.ops")
        .filter_map(|p| {
            p["_meta"]["agent/idempotency_key"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(keys.len(), 2, "{}", mock.log());
    assert_ne!(keys[0], keys[1], "the second life repeated the first's key");
}

/// A call the supervisor makes for a turn — here a `knowledge.search` the
/// profile server implements — carries a key of this process and this
/// child: the child's request ids begin at 1 in every turn, so the key
/// repeated across turns and across lives.
#[test]
fn a_supervisor_call_for_a_turn_has_a_key_of_its_own() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&json!({"turns": [
        {"tool_calls": [{"name": "knowledge.search", "arguments": {"query": "q"}}]},
        {"content": "found"}
    ]}));
    let life = || {
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args([
                "--prompt",
                "look it up",
                "--intelligence",
                &llm.uri,
                "--model",
                "mock",
                "--mcp",
                &format!("mock={}", mock.uri()),
                "--knowledge.server",
                "mock",
            ])
            .stdin(Stdio::null())
            .output()
            .expect("run agentd");
        let log = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(out.status.code(), Some(0), "{log}");
    };
    life();
    life();
    let keys: Vec<String> = calls(&mock)
        .into_iter()
        .filter(|p| p["name"] == "knowledge.search")
        .filter_map(|p| {
            p["_meta"]["agent/idempotency_key"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(keys.len(), 2, "{}", mock.log());
    assert_ne!(keys[0], keys[1], "the second life repeated the first's key");
}
