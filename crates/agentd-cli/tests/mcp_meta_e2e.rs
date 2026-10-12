// SPDX-License-Identifier: AGPL-3.0-only
//! Every `tools/call` agentd makes carries the run's `_meta` — `agent/run_id`,
//! `agent/instance` and a W3C `traceparent` — in `params._meta`, where MCP
//! reserves it, and the tool's `arguments` carry the tool's arguments alone.
//!
//! The trace is the run's, end to end: the supervisor's lines about the run,
//! the turn worker's and the subagent's lines, every MCP request any of them
//! makes — `tools/call`, `resources/read`, `prompts/get`, `resources/list`
//! alike, each a span of its own — and every model call carry the run's trace
//! id, which is not the process's; and two runs are two traces.
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

use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// The mock LLM with a `file:` playbook; killed on drop.
struct MockLlm {
    child: std::process::Child,
    addr_file: String,
    log_path: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
        let _ = std::fs::remove_file(&self.log_path);
    }
}
impl MockLlm {
    /// The W3C trace headers of every model call that reached the mock.
    fn traces(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("MOCK_LLM_TRACE "))
            .filter_map(|t| serde_json::from_str(t).ok())
            .collect()
    }
}

fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let log_path = common::unique_path("mock-llm", "log");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log_path).expect("mock llm log"))
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        log_path,
        uri: format!("http://{addr}"),
    }
}

/// agentd's JSON log lines.
fn lines(stderr: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// The trace id a `traceparent` names, after checking it is one.
fn trace_of(tp: &Value) -> &str {
    let tp = tp
        .as_str()
        .unwrap_or_else(|| panic!("no traceparent: {tp}"));
    assert!(
        tp.len() == 55 && tp.starts_with("00-") && tp.ends_with("-01"),
        "not a sampled W3C traceparent: {tp}"
    );
    &tp[3..35]
}

/// The span id a `traceparent` names.
fn span_of(tp: &Value) -> &str {
    &tp.as_str().unwrap_or_default()[36..52]
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
    // delegates to start with `initialize`, as the supervisor reached it —
    // theirs in the run's trace, since all either process does is the run's.
    assert_eq!(mock.discovers(), 1, "{}", mock.log());
    assert_eq!(mock.initializes(), 3, "{}", mock.log());
    let traces: Vec<String> = mock
        .metas()
        .iter()
        .filter(|m| m["method"] == "initialize")
        .map(|m| trace_of(&m["_meta"]["traceparent"]).to_string())
        .collect();
    let distinct: BTreeSet<&String> = traces.iter().collect();
    assert_eq!(
        distinct.len(),
        2,
        "the supervisor's in the process trace, the workers' in the run's: {traces:?}"
    );
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
                "agent:\n  name: meta-probe\nstore:\n  kind: memory\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nworkflows:\n  - name: probe\n    steps: {}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n",
                llm.uri,
                mock.uri(),
                json!({
                    // A loop's run is a trace of its own, apart from the
                    // process's, so a request that fell back to the process
                    // trace shows.
                    "start": {"kind": "loop", "max_iterations": 1},
                    "probe": {"kind": "mcp.tool", "depends_on": ["start"], "server": "mock",
                              "tool": "state.list", "args": {"key": "meta-probe"}},
                    "read": {"kind": "mcp.resource", "depends_on": ["probe"], "server": "mock",
                             "op": "read", "uri": "mock://noop"},
                    "list": {"kind": "mcp.resource", "depends_on": ["read"], "server": "mock",
                             "op": "list"},
                    "prompt": {"kind": "mcp.resource", "depends_on": ["list"], "server": "mock",
                               "op": "prompt", "name": "review-pr"},
                    "work": {"kind": "agent", "depends_on": ["prompt"], "instruction": "list and delegate"},
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
    assert_one_trace(&stderr, &mock, &llm);
    let _ = std::fs::remove_file(&cfg);
    mock
}

/// The probe run is one trace, whichever process does the work and whatever
/// it asks: the supervisor's lines about the run, the turn worker's and the
/// subagent's, every request made for it — each a span of its own — and every
/// model call carry the run's trace id. The process's trace is another, and
/// only the supervisor's own requests carry it.
fn assert_one_trace(stderr: &str, mock: &common::MockMcp, llm: &MockLlm) {
    let log = lines(stderr);
    let start = log
        .iter()
        .find(|l| l["event"] == "run.start" && l["workflow"] == "probe")
        .unwrap_or_else(|| panic!("no run.start:\n{stderr}"));
    let run = start["run"].as_str().unwrap();
    let trace = start["trace_id"]
        .as_str()
        .expect("run.start names its trace");
    let process = log
        .iter()
        .find(|l| l["event"] == "proc.start" && l["comp"] == "supervisor")
        .and_then(|l| l["trace_id"].as_str())
        .expect("proc.start names the process trace");
    assert_ne!(trace, process, "the run is a trace of its own");
    // The supervisor's lines about the run…
    let about_run: Vec<&Value> = log.iter().filter(|l| l["run"] == run).collect();
    for event in [
        "run.start",
        "step.start",
        "step.done",
        "step.turn.spawn",
        "run.done",
    ] {
        assert!(
            about_run.iter().any(|l| l["event"] == event),
            "no {event} line about the run:\n{stderr}"
        );
    }
    for l in &about_run {
        assert_eq!(l["trace_id"], trace, "a line about the run: {l}");
        assert!(l["span_id"].is_string(), "a line about the run: {l}");
    }
    // …the turn worker's, parented on the step's span the supervisor spawned
    // it under…
    let spawned = about_run
        .iter()
        .find(|l| l["event"] == "step.turn.spawn" && l["step"] == "work")
        .expect("the work step spawned a turn");
    // The supervisor's lines about that worker name its span as the spawn
    // did, parent and all: one span, one parent.
    let about_worker: Vec<&Value> = log
        .iter()
        .filter(|l| l["comp"] == "supervisor" && l["span_id"] == spawned["span_id"])
        .collect();
    assert!(
        about_worker.iter().any(|l| l["event"] == "child.exit"),
        "no child.exit in the worker's span:\n{stderr}"
    );
    for l in &about_worker {
        assert_eq!(l["trace_id"], trace, "{l}");
        assert_eq!(l["parent_span_id"], spawned["parent_span_id"], "{l}");
    }
    let worker: Vec<&Value> = log
        .iter()
        .filter(|l| {
            l["comp"] == "agent"
                && l["agent_path"]
                    .as_str()
                    .is_some_and(|p| p == format!("run/{run}/work"))
        })
        .collect();
    assert!(!worker.is_empty(), "no turn worker lines:\n{stderr}");
    for l in &worker {
        assert_eq!(l["trace_id"], trace, "a turn worker line: {l}");
        assert_eq!(
            l["parent_span_id"], spawned["span_id"],
            "a turn worker line: {l}"
        );
    }
    // …and the subagent's, delegated from that turn.
    let sub: Vec<&Value> = log
        .iter()
        .filter(|l| {
            l["agent_path"]
                .as_str()
                .is_some_and(|p| p.starts_with("sub/"))
        })
        .collect();
    assert!(!sub.is_empty(), "no subagent lines:\n{stderr}");
    for l in &sub {
        assert_eq!(l["trace_id"], trace, "a subagent line: {l}");
    }
    // Every request made for the run, whoever made it and whatever it was,
    // carried the run's trace, each under a span of its own. Only the
    // supervisor's connection-level requests — its handshake and the lists it
    // keeps its catalogue with — are in the process trace.
    let metas = mock.metas();
    for kind in [
        "tools/call",
        "resources/read",
        "prompts/get",
        "resources/list",
    ] {
        let made: Vec<&Value> = metas.iter().filter(|m| m["method"] == kind).collect();
        assert!(!made.is_empty(), "no {kind} reached the server: {metas:?}");
        for m in made {
            let tp = &m["_meta"]["traceparent"];
            // The supervisor lists resources for its own catalogue too.
            if kind != "resources/list" || trace_of(tp) != process {
                assert_eq!(trace_of(tp), trace, "{m}");
            }
        }
    }
    assert!(
        metas
            .iter()
            .any(|m| m["method"] == "resources/list"
                && trace_of(&m["_meta"]["traceparent"]) == trace),
        "the list step's request is the run's: {metas:?}"
    );
    let mut spans = BTreeSet::new();
    let mut of_process = 0;
    for m in &metas {
        let tp = &m["_meta"]["traceparent"];
        if trace_of(tp) == process {
            of_process += 1;
            assert!(
                [
                    "initialize",
                    "server/discover",
                    "tools/list",
                    "prompts/list",
                    "resources/list",
                    "resources/templates/list"
                ]
                .contains(&m["method"].as_str().unwrap_or("")),
                "a request made for the run went in the process trace: {m}"
            );
        } else {
            assert_eq!(trace_of(tp), trace, "{m}");
        }
        assert!(
            spans.insert(span_of(tp).to_string()),
            "a span id repeated: {m}"
        );
    }
    assert!(of_process > 0, "the supervisor's handshake: {metas:?}");
    let calls = metas.iter().filter(|m| m["method"] == "tools/call").count();
    assert!(calls >= 3, "{metas:?}");
    // And so did every model call — the turn's and the subagent's.
    let model_calls = llm.traces();
    assert!(model_calls.len() >= 3, "{model_calls:?}");
    for t in &model_calls {
        assert_eq!(trace_of(&t["traceparent"]), trace, "{t}");
        assert!(spans.insert(span_of(&t["traceparent"]).to_string()), "{t}");
    }
}

/// Two runs are two traces: each run's lines and each run's MCP requests — a
/// `tools/call`, a `resources/read` and a `resources/list` — carry its own
/// trace id, and neither is the process's.
#[test]
fn each_run_is_a_trace_of_its_own() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let cfg = common::unique_path("agentd-two-runs", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: twice\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nworkflows:\n  - name: twice\n    steps: {}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n",
            mock.uri(),
            json!({
                "start": {"kind": "loop", "max_iterations": 2},
                "probe": {"kind": "mcp.tool", "depends_on": ["start"], "server": "mock",
                          "tool": "state.list", "args": {"key": "twice"}},
                "read": {"kind": "mcp.resource", "depends_on": ["probe"], "server": "mock",
                         "op": "read", "uri": "mock://noop"},
                "list": {"kind": "mcp.resource", "depends_on": ["read"], "server": "mock",
                         "op": "list"},
                "done": {"kind": "finish", "depends_on": ["list"], "status": "completed"}
            })
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let log = lines(&stderr);
    let process = log
        .iter()
        .find(|l| l["event"] == "proc.start")
        .and_then(|l| l["trace_id"].as_str())
        .expect("proc.start names the process trace")
        .to_string();
    let runs: Vec<(String, String)> = log
        .iter()
        .filter(|l| l["event"] == "run.start")
        .map(|l| {
            (
                l["run"].as_str().unwrap().to_string(),
                l["trace_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(runs.len(), 2, "{stderr}");
    assert_ne!(runs[0].1, runs[1].1, "two runs shared a trace:\n{stderr}");
    let calls: Vec<Value> = mock
        .metas()
        .into_iter()
        .filter(|m| m["method"] == "tools/call")
        .collect();
    for (run, trace) in &runs {
        assert_ne!(*trace, process, "the run is not the process's trace");
        for l in log.iter().filter(|l| l["run"] == run.as_str()) {
            assert_eq!(l["trace_id"], trace.as_str(), "{l}");
        }
        // The run's own call, by the run id it names.
        let call = calls
            .iter()
            .find(|m| m["_meta"]["agent/run"] == run.as_str())
            .unwrap_or_else(|| panic!("no call from {run}: {calls:?}"));
        assert_eq!(trace_of(&call["_meta"]["traceparent"]), trace, "{call}");
    }
    // A read names no run, so the two reads are the two runs' traces.
    let reads: BTreeSet<String> = mock
        .metas()
        .iter()
        .filter(|m| m["method"] == "resources/read")
        .map(|m| trace_of(&m["_meta"]["traceparent"]).to_string())
        .collect();
    let traces: BTreeSet<String> = runs.iter().map(|(_, t)| t.clone()).collect();
    assert_eq!(reads, traces, "each run's read is its own trace");
    // Nor does a list; with no worker here to list for itself, a list in
    // a trace other than the process's is a run's list step.
    let lists: BTreeSet<String> = mock
        .metas()
        .iter()
        .filter(|m| m["method"] == "resources/list")
        .map(|m| trace_of(&m["_meta"]["traceparent"]).to_string())
        .filter(|t| *t != process)
        .collect();
    assert_eq!(lists, traces, "each run's list is its own trace");
    let _ = std::fs::remove_file(&cfg);
}

/// A loop-started `human` gate nobody answers in time, which the `approve`
/// judge answers with `verdict`: the log, the gated run's trace id, and the
/// mock the judge called.
fn judged(verdict: &str) -> (String, Vec<Value>, String, MockLlm) {
    let llm = spawn_mock_llm(&json!({
        "turns": [{"content": "unreached"}],
        "match": [
            {"when_contains": "answering ON BEHALF OF the unavailable human operator", "content": verdict}
        ]
    }));
    let cfg = common::unique_path("agentd-judge-trace", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: judged\n  ask_human_fallback: auto\nstore:\n  kind: memory\nintelligence:\n  endpoints: {}\n  model: mock\nworkflows:\n  - name: gated\n    steps: {}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n",
            llm.uri,
            json!({
                "start": {"kind": "loop", "max_iterations": 1},
                "gate": {"kind": "human", "depends_on": ["start"],
                         "question": "Deploy the release?", "timeout": "1s"},
                "done": {"kind": "finish", "depends_on": ["gate"], "status": "completed"}
            })
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let _ = std::fs::remove_file(&cfg);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let log = lines(&stderr);
    let start = log
        .iter()
        .find(|l| l["event"] == "run.start")
        .unwrap_or_else(|| panic!("no run:\n{stderr}"));
    let trace = start["trace_id"].as_str().unwrap().to_string();
    (stderr, log, trace, llm)
}

/// The supervisor's own model call — the `approve` judge answering a gate
/// nobody answered in time — is a span of the gated run's trace, as a
/// worker's model call is of its unit's.
#[test]
fn the_approve_judge_calls_in_the_gated_runs_trace() {
    let (stderr, log, trace, llm) = judged("go");
    let judged = log
        .iter()
        .find(|l| l["event"] == "human.judge.start")
        .unwrap_or_else(|| panic!("the judge never ran:\n{stderr}"));
    assert_eq!(judged["trace_id"], trace, "{judged}");
    let calls = llm.traces();
    assert!(!calls.is_empty(), "the judge's model call reached the mock");
    for t in &calls {
        assert_eq!(trace_of(&t["traceparent"]), trace, "{t}");
    }
}

/// A gate the judge cannot decide fails, and says so in the gated run's
/// trace.
#[test]
fn a_gate_the_judge_cannot_decide_fails_in_the_gated_runs_trace() {
    let (stderr, log, trace, _llm) = judged("UNDECIDED");
    let failed: Vec<&Value> = log
        .iter()
        .filter(|l| l["event"] == "human.ask.failed")
        .collect();
    assert!(!failed.is_empty(), "the gate did not fail:\n{stderr}");
    for l in failed {
        assert_eq!(l["trace_id"], trace.as_str(), "{l}");
    }
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
