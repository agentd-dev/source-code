// SPDX-License-Identifier: AGPL-3.0-only
//! `subagent.run`'s `tools:` narrowing is a GRANT the child ENFORCES (scope
//! narrows monotonically down the subagent tree). A grant names tools as the
//! agent's catalogue does: the mock server's `knowledge.search` shares its
//! name with an agentd contract, so it is `mock.knowledge.search`.
//!
//! The failure this pins: the argument must not be silently accepted, written
//! into the durable record as `allowed_tools`, and never read back — a parent
//! bounding an untrusted sub-task to one tool must not end up with a child
//! holding the whole catalogue. Here a real reactor delegates to a real subagent
//! PROCESS with `tools: ["mock.knowledge.search"]` while the granted MCP server
//! publishes a dozen, and we assert both halves of enforcement: the child's
//! catalogue carries only the granted tool, and a model that names an excluded
//! one anyway is refused rather than served.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

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

fn spawn_mock_llm(playbook: &serde_json::Value) -> MockLlm {
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

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-narrow", "yaml");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    path
}

/// The NDJSON telemetry lines (supervisor AND child — the subagent inherits
/// stderr) named `name`.
fn events(stderr: &str, name: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// Only the lines a SUBAGENT wrote (`agent_path` = `sub/<handle>`).
fn subagent_events(stderr: &str, name: &str) -> Vec<serde_json::Value> {
    events(stderr, name)
        .into_iter()
        .filter(|v| {
            v["agent_path"]
                .as_str()
                .is_some_and(|p| p.starts_with("sub/"))
        })
        .collect()
}

#[test]
fn a_subagent_granted_one_tool_gets_only_that_tool_and_is_refused_the_rest() {
    // The mock MCP server publishes a dozen tools, `knowledge.search` and
    // `search.query` among them, plus a resource (so `resource.read` would join
    // an ungranted catalogue too).
    let mock = common::spawn_mock_mcp("mock://noop", false);
    // Root turn 0: delegate with a one-tool grant. Child turns are picked by the
    // `match` rules — the child is identified by its own system prompt, which
    // never appears in the root's transcript. Rule order matters: the refusal
    // rules come first, because by the time the child is refused its transcript
    // still contains the earlier rounds' markers.
    let llm = spawn_mock_llm(&serde_json::json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {
                "instruction": "search the corpus for the deployment policy",
                "mode": "sync",
                "tools": ["mock.knowledge.search"]
            }}]},
            {"content": "delegated and done"}
        ],
        "match": [
            // The child called an excluded tool and was refused → it says so.
            {"when_contains": "'mock.search.query' is not in this subagent", "content": "REFUSED_SEARCH"},
            // The grant must not swallow the GRANTED tool (over-narrowing).
            {"when_contains": "'mock.knowledge.search' is not in this subagent", "content": "OVER_NARROWED"},
            // The granted tool answered (hits carry `kb://` uris) → now name an
            // excluded one anyway, the way a model that ignores its catalogue would.
            {"when_contains": "kb://", "tool_calls": [{"name": "mock.search.query", "arguments": {"query": "policy"}}]},
            // The child's first round: the subagent system prompt.
            {"when_contains": "You are agentd, an autonomous agent.", "tool_calls": [{"name": "mock.knowledge.search", "arguments": {}}]}
        ]
    }));
    let cfg = write_config(&format!(
        "agent:\n  instruction: delegate the search\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nobservability:\n  log_level: info\n  log_content: true\n",
        llm.uri,
        mock.uri()
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");

    // One child was spawned with the grant.
    assert_eq!(events(&stderr, "subagent.spawn").len(), 1, "{stderr}");

    // (1) THE CATALOGUE. The child's `loop.start` reports the tool count it
    // offered the model: exactly the one granted tool. Ungranted it would be the
    // mock's whole tools/list plus `resource.read` (13).
    let start = subagent_events(&stderr, "loop.start");
    assert!(
        start.iter().any(|e| e["tools"] == 1),
        "the child's catalogue is the granted tool alone, got {:?}: {stderr}",
        start.iter().map(|e| e["tools"].clone()).collect::<Vec<_>>()
    );

    // (2) THE DISPATCH. The granted tool ran; the excluded one was REFUSED, not
    // served — an error observation, so a model that names it anyway gains nothing.
    let results = subagent_events(&stderr, "tool.result");
    assert!(
        results
            .iter()
            .any(|e| e["tool"] == "mock.knowledge.search" && e["is_error"] == false),
        "the granted tool still works (the grant narrows, it does not blind): {stderr}"
    );
    let excluded = results
        .iter()
        .find(|e| e["tool"] == "mock.search.query")
        .unwrap_or_else(|| panic!("the child called the excluded tool: {stderr}"));
    assert_eq!(
        excluded["is_error"], true,
        "an excluded tool must be refused, never served: {stderr}"
    );

    // (3) What the MODEL saw for the excluded call is a refusal naming the tool
    // (`log_content: true` captures the observation), and the child ran to a
    // normal completion on it — a refused tool is an observation to adapt to,
    // not a crashed child.
    assert!(
        excluded["content"]
            .as_str()
            .is_some_and(|c| c.contains("is not in this subagent's allowed tools")),
        "the refusal is the observation the model reads: {excluded}"
    );
    let res = events(&stderr, "subagent.result");
    assert_eq!(res.len(), 1, "{stderr}");
    assert_eq!(res[0]["status"], "completed", "{stderr}");
    assert!(
        !stderr.contains("OVER_NARROWED"),
        "the grant must not filter out the tool it granted: {stderr}"
    );

    // (4) And the confinement traces back to the ARGUMENT the parent passed —
    // this is `subagent.run`'s `tools:` being honoured, not some other narrowing.
    assert!(
        events(&stderr, "tool.request")
            .iter()
            .any(|e| e["tool"] == "subagent.run"
                && e["args"]["tools"] == serde_json::json!(["mock.knowledge.search"])),
        "{stderr}"
    );
}

/// A subagent's catalogue is the one the supervisor's registry builds: the
/// server's `exclude` removes a tool from the child too, and its `ns` names
/// the child's tools as it names the parent's. The spawn payload used to
/// carry neither, so a child listed the server's raw tools and could call
/// one the operator excluded.
#[test]
fn a_subagent_lists_a_server_as_the_operator_scoped_it() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&serde_json::json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {
                "instruction": "search the corpus for the deployment policy",
                "mode": "sync"
            }}]},
            {"content": "delegated and done"}
        ],
        // Latest stage first: each round's transcript holds the earlier ones.
        "match": [
            {"when_contains": "no such tool 'knowledge.search'", "content": "SCOPED"},
            {"when_contains": "no such tool 'kb.search.query'",
             "tool_calls": [{"name": "knowledge.search", "arguments": {}}]},
            {"when_contains": "kb://",
             "tool_calls": [{"name": "kb.search.query", "arguments": {"query": "policy"}}]},
            {"when_contains": "You are agentd, an autonomous agent.",
             "tool_calls": [{"name": "kb.knowledge.search", "arguments": {}}]}
        ]
    }));
    let run = |scope: &str| {
        let cfg = write_config(&format!(
            "agent:\n  instruction: delegate the search\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\n      ns: kb\n{scope}observability:\n  log_level: info\n  log_content: true\n",
            llm.uri,
            mock.uri()
        ));
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg])
            .stdin(Stdio::null())
            .output()
            .expect("run agentd");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let tools = |stderr: &str| {
        subagent_events(stderr, "loop.start")
            .iter()
            .find_map(|e| e["tools"].as_u64())
            .unwrap_or_else(|| panic!("the child's catalogue size: {stderr}"))
    };
    let (code, stderr) = run("      exclude: [search.query]\n");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let results = subagent_events(&stderr, "tool.result");
    let result = |tool: &str| {
        results
            .iter()
            .find(|e| e["tool"] == tool)
            .unwrap_or_else(|| panic!("the child never called {tool}: {stderr}"))
    };
    assert_eq!(result("kb.knowledge.search")["is_error"], false, "{stderr}");
    for refused in ["kb.search.query", "knowledge.search"] {
        assert_eq!(
            result(refused)["is_error"],
            true,
            "{refused} was served: {stderr}"
        );
    }
    // The excluded tool is not offered either: one tool fewer than the same
    // server unscoped (two steps are enough to see its catalogue).
    let (_, unscoped) = run("limits: {run: {steps: 2}}\n");
    assert_eq!(tools(&unscoped) - tools(&stderr), 1, "{stderr}\n{unscoped}");
    let res = events(&stderr, "subagent.result");
    assert_eq!(res.len(), 1, "{stderr}");
    assert_eq!(res[0]["status"], "completed", "{stderr}");
}

/// A child names its tools as the registry does, whatever order its servers
/// reach it in, so a policy gate written against the registry's name holds
/// in the child. Two servers advertise `mock.ops`; `zeta`, first in the
/// configuration, keeps the name and `alpha`'s is `alpha.mock.ops`. A child
/// that named them by its own (alphabetical) server order called `zeta`'s
/// tool as `zeta.mock.ops`, which no gate named, and reached it directly.
#[test]
fn a_policy_gate_holds_in_a_child_whatever_order_its_servers_come_in() {
    let zeta = common::spawn_mock_mcp("mock://zeta", false);
    let alpha = common::spawn_mock_mcp("mock://alpha", false);
    let llm = spawn_mock_llm(&serde_json::json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {
                "instruction": "count the operations", "mode": "sync"
            }}]},
            {"content": "delegated and done"}
        ],
        // Latest stage first: each round's transcript holds the earlier ones.
        "match": [
            {"when_contains": "\\\"ops\\\"", "content": "CHILD_DONE"},
            {"when_contains": "denied by security.policies",
             "tool_calls": [{"name": "alpha.mock.ops", "arguments": {}}]},
            {"when_contains": "You are agentd, an autonomous agent.",
             "tool_calls": [{"name": "mock.ops", "arguments": {}}]}
        ]
    }));
    let cfg = write_config(&format!(
        "agent:\n  instruction: delegate the count\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: zeta\n      endpoint: {}\n    - name: alpha\n      endpoint: {}\nsecurity:\n  policies:\n    - match: {{ tool: \"mock.ops\", caller: [subagent] }}\n      action: deny\nobservability:\n  log_level: info\n  log_content: true\n",
        llm.uri,
        zeta.uri(),
        alpha.uri()
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let results = subagent_events(&stderr, "tool.result");
    let result = |tool: &str| {
        results
            .iter()
            .find(|e| e["tool"] == tool)
            .unwrap_or_else(|| panic!("the child never called {tool}: {stderr}"))
    };
    let gated = result("mock.ops");
    assert_eq!(gated["is_error"], true, "{stderr}");
    assert!(
        gated["content"]
            .as_str()
            .is_some_and(|c| c.contains("denied by security.policies")),
        "the gate answered the call: {gated}"
    );
    assert_eq!(result("alpha.mock.ops")["is_error"], false, "{stderr}");
    let res = events(&stderr, "subagent.result");
    assert_eq!(res.len(), 1, "{stderr}");
    assert_eq!(res[0]["status"], "completed", "{stderr}");
}

/// A tool the operator disabled does not exist for a child either: the
/// child listed its server's tools itself and offered the disabled one.
#[test]
fn a_disabled_tool_is_not_a_childs() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&serde_json::json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {
                "instruction": "count the operations", "mode": "sync"
            }}]},
            {"content": "delegated and done"}
        ],
        "match": [
            {"when_contains": "no such tool 'mock.ops'", "content": "CHILD_DONE"},
            {"when_contains": "You are agentd, an autonomous agent.",
             "tool_calls": [{"name": "mock.ops", "arguments": {}}]}
        ]
    }));
    let cfg = write_config(&format!(
        "agent:\n  instruction: delegate it\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\ntools:\n  disabled: [mock.ops]\nobservability:\n  log_level: info\n  log_content: true\n",
        llm.uri,
        mock.uri()
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let refused = subagent_events(&stderr, "tool.result")
        .into_iter()
        .find(|e| e["tool"] == "mock.ops")
        .unwrap_or_else(|| panic!("the child never called it: {stderr}"));
    assert_eq!(
        refused["is_error"], true,
        "a disabled tool was served: {stderr}"
    );
}
