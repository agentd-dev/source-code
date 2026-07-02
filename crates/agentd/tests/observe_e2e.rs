// SPDX-License-Identifier: Apache-2.0
//! The observe-to-validate E2E suite (M7, the operator ask): drive *real* agentd
//! runs against the built-in mock LLM (+ mock MCP) and assert on the **observed**
//! JSON-lines telemetry + outcome. This is the first end-to-end exercise of the
//! actual agentic loop — every other test stubs the intelligence endpoint.
#![cfg(unix)]

mod common;

use common::spawn_mock_mcp;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_agentd")
}

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// Start the mock LLM with `script`, announcing its loopback address through
/// `addr_file`. Returns the child and the `http://<addr>` intelligence URL.
fn start_mock_llm(addr_file: &Path, script: &str) -> (Child, String) {
    let child = Command::new(exe())
        .args(["--internal-mock-llm", addr_file.to_str().unwrap(), script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !addr_file.exists() {
        if Instant::now() >= deadline {
            panic!("mock-llm never announced its address");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let addr = std::fs::read_to_string(addr_file).expect("read mock-llm addr-file");
    (child, format!("http://{}", addr.trim()))
}

/// Run `agentd <args>` to completion; return (exit_code, stdout, stderr).
fn run_once(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(exe()).args(args).output().expect("run agentd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn once_mode_runs_the_real_loop_to_a_completed_answer() {
    let dir = tempfile::tempdir().unwrap();
    let addr_file = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(&addr_file, "final");

    let (code, stdout, stderr) = run_once(&[
        "--mode",
        "once",
        "--instruction",
        "do the thing",
        "--intelligence",
        &intel,
        "--log-level",
        "info",
    ]);

    sigterm(llm.id());
    let _ = llm.wait();

    assert_eq!(code, 0, "expected exit 0; stderr:\n{stderr}");
    assert!(
        stdout.contains("mock-llm done"),
        "model answer not on stdout: {stdout:?}"
    );
    assert!(
        stderr.contains(r#""event":"loop.final""#),
        "no loop.final:\n{stderr}"
    );
    assert!(
        stderr.contains(r#""status":"completed""#),
        "loop did not complete:\n{stderr}"
    );
}

#[test]
fn once_mode_runs_a_tool_call_react_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let addr_file = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(&addr_file, "read");

    let mock = spawn_mock_mcp("file:///in.json", false);
    let (code, stdout, stderr) = run_once(&[
        "--mode",
        "once",
        "--instruction",
        "read the resource",
        "--intelligence",
        &intel,
        "--mcp",
        &mock.mcp_arg("mock"),
        "--log-level",
        "info",
    ]);

    sigterm(llm.id());
    let _ = llm.wait();

    assert_eq!(code, 0, "expected exit 0; stderr:\n{stderr}");
    assert!(
        stdout.contains("read complete"),
        "final answer not on stdout: {stdout:?}"
    );
    // The model called the resource.read tool, and the loop ran it then finished.
    assert!(
        stderr.contains(r#""event":"tool.call""#) && stderr.contains("resource.read"),
        "no resource.read tool.call:\n{stderr}"
    );
    assert!(
        stderr.contains(r#""event":"tool.result""#),
        "no tool.result:\n{stderr}"
    );
    assert!(
        stderr.contains(r#""status":"completed""#),
        "loop did not complete:\n{stderr}"
    );
}

#[cfg(feature = "workflow")]
#[test]
fn workflow_mode_drives_a_pinned_workflow_to_completion() {
    // A pinned workflow (agent → halt) driven by `--mode workflow` against the mock LLM:
    // the agent node runs a REAL ReAct turn ("final" answers "mock-llm done"), its
    // result flows to the halt, and the run exits 0 (pivot Phase 7 · P6). No
    // --instruction needed — the workflow carries it.
    let dir = tempfile::tempdir().unwrap();
    let addr_file = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(&addr_file, "final");

    let graph_path = dir.path().join("g.json");
    std::fs::write(
        &graph_path,
        r#"{
            "start": "a",
            "nodes": {
                "a": {"kind": "agent", "instruction": "do the thing", "writes": "out", "edges": {"ok": "h"}},
                "h": {"kind": "halt", "status": "completed", "result_from": "out"}
            }
        }"#,
    )
    .unwrap();

    let (code, stdout, stderr) = run_once(&[
        "--mode",
        "workflow",
        "--workflow",
        graph_path.to_str().unwrap(),
        "--intelligence",
        &intel,
        "--log-level",
        "info",
    ]);

    sigterm(llm.id());
    let _ = llm.wait();

    assert_eq!(code, 0, "pinned workflow completes 0; stderr:\n{stderr}");
    assert!(
        stdout.contains("mock-llm done"),
        "the agent node's result reached the halt: {stdout:?}"
    );
    assert!(
        stderr.contains(r#""workflow_status":"Completed""#),
        "workflow completed status logged:\n{stderr}"
    );
}

#[cfg(feature = "workflow")]
#[test]
fn workflow_mode_requires_a_workflow_file() {
    // `--mode workflow` without `--workflow` is a clear usage error (exit 2).
    let (code, _out, stderr) =
        run_once(&["--mode", "workflow", "--intelligence", "http://127.0.0.1:9"]);
    assert_eq!(code, 2, "usage error; stderr:\n{stderr}");
    assert!(
        stderr.contains("--mode workflow requires --workflow"),
        "{stderr}"
    );
}

#[cfg(feature = "workflow")]
#[test]
fn a_reactive_workflow_daemon_suspends_and_resumes_across_children() {
    // The reactive-daemon workflow (pivot Phase 7 follow-up): the FIRST child
    // drives to the Wait and SUSPENDS (exits, serializing its slice); the DAEMON
    // arms the subscription; the mock MCP pushes an update; a SECOND child
    // resumes on the `updated` edge and completes — and the daemon's lifetime
    // ends with the workflow's, exit 0. No process blocks on the wait.
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_mcp("file:///in.json", true); // emit=true → pushes an update
    let wf_path = dir.path().join("reactive-wait.json");
    std::fs::write(
        &wf_path,
        r#"{
            "start": "w",
            "nodes": {
                "w": {"kind": "wait", "on_uri": "file:///in.json", "writes": "evt", "timeout_ms": 15000, "edges": {"updated": "done", "timeout": "expired"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "evt"},
                "expired": {"kind": "halt", "status": "deadline"}
            }
        }"#,
    )
    .unwrap();

    let (code, _stdout, stderr) = run_once(&[
        "--mode",
        "reactive",
        "--workflow",
        wf_path.to_str().unwrap(),
        "--intelligence",
        "http://127.0.0.1:9",
        "--mcp",
        &mock.mcp_arg("mock"),
        "--log-level",
        "info",
    ]);

    assert_eq!(
        code, 0,
        "the resumed workflow completed; stderr:
{stderr}"
    );
    assert!(
        stderr.contains(r#""event":"workflow.suspended""#),
        "the first child suspended on the wait:
{stderr}"
    );
    assert!(
        stderr.contains(r#""event":"workflow.reactive.exit""#)
            && stderr.contains(r#""status":"completed""#),
        "the daemon ended with the workflow's terminal:
{stderr}"
    );
}

#[cfg(feature = "workflow")]
#[test]
fn an_async_subgraph_spawns_a_real_child_and_join_collects_it() {
    // The full spawn/join chain through REAL processes: the supervised workflow
    // child spawns a GRANDCHILD subagent to drive the async subgraph (an agent
    // node against the mock LLM), then a join node collects its distillate.
    let dir = tempfile::tempdir().unwrap();
    let addr_file = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(&addr_file, "final");

    let wf_path = dir.path().join("spawnjoin.json");
    std::fs::write(
        &wf_path,
        r#"{
            "start": "fan",
            "nodes": {
                "fan": {"kind": "subgraph", "async": true,
                        "graph": {"start": "work", "nodes": {
                            "work": {"kind": "agent", "instruction": "do the parallel piece", "writes": "out", "edges": {"ok": "h", "error": "f"}},
                            "h": {"kind": "halt", "status": "completed", "result_from": "out"},
                            "f": {"kind": "halt", "status": "crashed"}
                        }},
                        "writes": "h1", "edges": {"ok": "join", "error": "fail"}},
                "join": {"kind": "join", "handles": {"$from": "h1"}, "timeout_ms": 30000, "writes": "results",
                         "edges": {"ok": "done", "error": "fail", "timeout": "fail"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "results"},
                "fail": {"kind": "halt", "status": "crashed", "result_from": "results"}
            }
        }"#,
    )
    .unwrap();

    let (code, stdout, stderr) = run_once(&[
        "--mode",
        "workflow",
        "--workflow",
        wf_path.to_str().unwrap(),
        "--intelligence",
        &intel,
        "--log-level",
        "info",
    ]);

    sigterm(llm.id());
    let _ = llm.wait();

    assert_eq!(
        code, 0,
        "spawn/join workflow completes 0; stderr:
{stderr}"
    );
    assert!(
        stdout.contains("mock-llm done"),
        "the grandchild's distillate flowed through the join: {stdout:?}"
    );
    assert!(
        stderr.contains(r#""event":"subagent.spawn_async""#),
        "a real async child was spawned:
{stderr}"
    );
}

#[cfg(feature = "workflow")]
#[test]
fn workflow_mode_resolves_a_wait_node_in_process() {
    // A pinned graph whose Wait node blocks on an MCP resource: the mock MCP pushes an
    // update after subscribe, so the wait resolves IN-PROCESS (no daemon) and the graph
    // completes 0 (pivot Phase 7 · P6). No Agent node → no LLM needed.
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_mcp("file:///in.json", true); // emit=true → pushes an update
    let graph_path = dir.path().join("wait.json");
    std::fs::write(
        &graph_path,
        r#"{
            "start": "w",
            "nodes": {
                "w": {"kind": "wait", "on_uri": "file:///in.json", "writes": "evt", "timeout_ms": 8000, "edges": {"updated": "done", "timeout": "expired"}},
                "done": {"kind": "halt", "status": "completed", "result_from": "evt"},
                "expired": {"kind": "halt", "status": "deadline"}
            }
        }"#,
    )
    .unwrap();

    let (code, _stdout, stderr) = run_once(&[
        "--mode",
        "workflow",
        "--workflow",
        graph_path.to_str().unwrap(),
        "--intelligence",
        "http://127.0.0.1:9",
        "--mcp",
        &mock.mcp_arg("mock"),
        "--log-level",
        "info",
    ]);

    assert_eq!(
        code, 0,
        "the wait resolved + the graph completed; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(r#""event":"workflow.wait""#),
        "the wait was logged:\n{stderr}"
    );
    assert!(
        stderr.contains(r#""workflow_status":"Completed""#),
        "the graph completed after the update:\n{stderr}"
    );
}

#[test]
fn once_mode_documents_dropped_deferred_effects() {
    // A one-shot run whose model calls the `schedule` deferred-effect self-tool has
    // no reactor to arm the wake — so the effect is DROPPED, but LOUDLY (pivot Phase
    // 5.2 acceptance): the run still completes 0, and both the telemetry and stderr
    // say the deferred effect needs a daemon mode. This is the "document the drop"
    // contract for schedule/subscribe/await_resource under `--mode once`.
    let dir = tempfile::tempdir().unwrap();
    let addr_file = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(&addr_file, "schedule");

    let (code, _stdout, stderr) = run_once(&[
        "--mode",
        "once",
        "--instruction",
        "schedule a follow-up",
        "--intelligence",
        &intel,
        "--log-level",
        "info",
    ]);

    sigterm(llm.id());
    let _ = llm.wait();

    assert_eq!(code, 0, "one-shot still completes 0; stderr:\n{stderr}");
    // The documented drop: a telemetry event + a human-readable stderr notice.
    assert!(
        stderr.contains(r#""event":"once.deferred_effects_dropped""#),
        "no dropped-effects telemetry event:\n{stderr}"
    );
    assert!(
        stderr.contains("daemon mode"),
        "no human-readable drop notice on stderr:\n{stderr}"
    );
}

#[test]
fn reactive_self_scheduling_fires_a_wake() {
    // A reaction's model calls the `schedule` self-tool; the daemon arms the wake
    // and fires it ~1s later — a self-sustaining agent, observed end to end.
    let dir = tempfile::tempdir().unwrap();
    let addr_file = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(&addr_file, "schedule");
    let mock = spawn_mock_mcp("file:///in.json", false);
    let mut child = Command::new(exe())
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            &intel,
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mock.mcp_arg("mock"),
            "--log-level",
            "info",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn reactive agentd");

    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // read-after-subscribe → reaction → schedule(after 1s) → wake fires ~1s later.
    std::thread::sleep(Duration::from_millis(2800));
    let _ = child.kill();
    let _ = child.wait();
    sigterm(llm.id());
    let _ = llm.wait();
    let out = reader.join().unwrap_or_default();

    assert!(
        out.contains(r#""event":"self.schedule""#),
        "model never called schedule:\n{out}"
    );
    assert!(
        out.contains(r#""kind":"self_schedule""#),
        "no self-scheduled wake armed/fired:\n{out}"
    );
    assert!(
        out.contains(r#""event":"trigger.fired""#),
        "no trigger fired:\n{out}"
    );
}

#[test]
fn reactive_self_subscribe_arms_a_warm_continue_route() {
    // A reaction's model calls the `subscribe` self-tool for a NEW uri; the daemon
    // must arm it as a WARM continue route (RFC 0008 §self-subscribe = continue),
    // not a fresh-spawn route — so future events re-enter one live session.
    let dir = tempfile::tempdir().unwrap();
    let addr_file = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(&addr_file, "subscribe");
    let mock = spawn_mock_mcp("file:///in.json", false);
    let mut child = Command::new(exe())
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            &intel,
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mock.mcp_arg("mock"),
            "--log-level",
            "info",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn reactive agentd");

    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // read-after-subscribe → reaction → the model self-subscribes to file:///watch.json.
    std::thread::sleep(Duration::from_millis(2000));
    let _ = child.kill();
    let _ = child.wait();
    sigterm(llm.id());
    let _ = llm.wait();
    let out = reader.join().unwrap_or_default();

    assert!(
        out.contains(r#""event":"self.subscribe""#),
        "model never called subscribe:\n{out}"
    );
    assert!(
        out.contains(r#""kind":"self_subscribe""#),
        "no self-subscription armed:\n{out}"
    );
    // The new route is a WARM continue, not a Spawn (the signature capability).
    assert!(
        out.contains(r#""disposition":"continue""#),
        "self-subscribe must arm a continue (warm) route:\n{out}"
    );
}

/// Reserve a free localhost TCP port, then release it so the daemon can bind it.
/// (A momentary `:0` bind that we drop — the standard "grab a free port" trick;
/// the sub-ms reuse window is benign for a single-process test.)
#[cfg(feature = "metrics")]
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// GET `/metrics` over a fresh TCP connection; return the response body (or "" if
/// the surface is not up yet).
#[cfg(feature = "metrics")]
fn scrape_metrics(port: u16) -> String {
    use std::io::Write;
    use std::net::TcpStream;
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return String::new();
    };
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    if s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return String::new();
    }
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok();
    buf
}

/// Extract the integer value of a `agent_tokens_total{type="..."} N` sample.
#[cfg(feature = "metrics")]
fn token_total(body: &str, ty: &str) -> u64 {
    let needle = format!("agent_tokens_total{{type=\"{ty}\"}} ");
    for line in body.lines() {
        if let Some(v) = line.strip_prefix(&needle) {
            return v.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// The producer→consumer→counter chain (metrics-honesty): a real reactive run
/// drives a real subagent against the mock LLM; the child now PRODUCES
/// `AgentMsg::Usage`, the supervisor reactor CONSUMES it via `record_tokens`, and
/// the frozen `agent_tokens_total{type}` counter is non-zero on the `/metrics`
/// scrape. Before this fix the child never emitted `Usage`, so the counter was
/// silently 0 despite the wired consumer. [feature: metrics]
#[cfg(feature = "metrics")]
#[test]
fn reactive_run_rolls_token_usage_up_to_agent_tokens_total() {
    let dir = tempfile::tempdir().unwrap();
    let addr_file = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(&addr_file, "final");
    // The HTTP mock (emit=true) pushes one resources/updated on the GET SSE stream
    // after the subscribe, firing exactly one reaction — one real subagent run.
    let mock = spawn_mock_mcp("file:///in.json", true);
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let mut child = Command::new(exe())
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react to the changed resource",
            "--intelligence",
            &intel,
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mock.mcp_arg("mock"),
            "--metrics-addr",
            &addr,
            "--log-level",
            "info",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn reactive agentd");

    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // Poll /metrics while the daemon is LIVE: the reaction fires ~200ms after
    // subscribe; once the child rolls its Usage up, the supervisor's
    // `agent_tokens_total` goes non-zero. The mock `final` answer reports
    // {prompt_tokens:11, completion_tokens:5}, so `out` reaches ≥ 5.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut out_tokens = 0u64;
    let mut in_tokens = 0u64;
    while Instant::now() < deadline {
        let body = scrape_metrics(port);
        out_tokens = token_total(&body, "out");
        in_tokens = token_total(&body, "in");
        if out_tokens > 0 && in_tokens > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let _ = child.kill();
    let _ = child.wait();
    sigterm(llm.id());
    let _ = llm.wait();
    let out = reader.join().unwrap_or_default();

    // Sanity: the reaction actually ran a real subagent against the mock LLM —
    // there WAS a run to produce Usage.
    assert!(
        out.contains(r#""event":"trigger.fired""#),
        "no reaction fired (no real run to produce Usage):\n{out}"
    );
    // The chain: child PRODUCES AgentMsg::Usage → supervisor reactor CONSUMES it via
    // record_tokens → the frozen agent_tokens_total counter is non-zero. Before
    // this fix the producer was missing, so this would read 0.
    assert!(
        in_tokens > 0 && out_tokens > 0,
        "agent_tokens_total stayed zero — the child→supervisor→counter token \
         roll-up is broken (in={in_tokens}, out={out_tokens})\n{out}"
    );
}
