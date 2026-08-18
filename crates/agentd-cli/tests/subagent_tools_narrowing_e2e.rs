// SPDX-License-Identifier: Apache-2.0
//! `subagent.run`'s `tools:` narrowing is a GRANT the child ENFORCES (RFC 0009:
//! scope narrows monotonically down the subagent tree).
//!
//! The regression this pins: the argument used to be accepted, written into the
//! durable record as `allowed_tools`, and never read back — a parent bounding an
//! untrusted sub-task to one tool silently got a child holding the whole
//! catalogue. Here a real reactor delegates to a real subagent PROCESS with
//! `tools: ["knowledge.search"]` while the granted MCP server publishes a dozen,
//! and we assert both halves of enforcement: the child's catalogue carries only
//! the granted tool, and a model that names an excluded one anyway is refused
//! rather than served.

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

/// Only the lines a SUBAGENT wrote (`agent_path` = `sub/<handle>`, RFC 0010).
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
                "tools": ["knowledge.search"]
            }}]},
            {"content": "delegated and done"}
        ],
        "match": [
            // The child called an excluded tool and was refused → it says so.
            {"when_contains": "'search.query' is not in this subagent", "content": "REFUSED_SEARCH"},
            // The grant must not swallow the GRANTED tool (over-narrowing).
            {"when_contains": "'knowledge.search' is not in this subagent", "content": "OVER_NARROWED"},
            // The granted tool answered (hits carry `kb://` uris) → now name an
            // excluded one anyway, the way a model that ignores its catalogue would.
            {"when_contains": "kb://", "tool_calls": [{"name": "search.query", "arguments": {"query": "policy"}}]},
            // The child's first round: the subagent system prompt.
            {"when_contains": "You are agentd, an autonomous agent.", "tool_calls": [{"name": "knowledge.search", "arguments": {}}]}
        ]
    }));
    let cfg = write_config(&format!(
        "config_version: \"2\"\nagent:\n  instruction: delegate the search\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nobservability:\n  log_level: info\n  log_content: true\n",
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
    // mock's whole tools/list plus `resource.read` (13) — the pre-fix behaviour.
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
            .any(|e| e["tool"] == "knowledge.search" && e["is_error"] == false),
        "the granted tool still works (the grant narrows, it does not blind): {stderr}"
    );
    let excluded = results
        .iter()
        .find(|e| e["tool"] == "search.query")
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
                && e["args"]["tools"] == serde_json::json!(["knowledge.search"])),
        "{stderr}"
    );
}
