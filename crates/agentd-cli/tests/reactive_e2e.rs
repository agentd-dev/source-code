// SPDX-License-Identifier: Apache-2.0
//! Observe-to-validate E2E tests of reactivity (M3 / the M7 observe-suite seed).
//!
//! Runs a real reactive agentd against the built-in mock MCP server and
//! validates the behaviour **by observing agentd's JSON-lines telemetry**. No
//! live LLM needed — each reaction's subagent fails on an unreachable
//! intelligence endpoint, but the reactive *behaviour* is fully visible and
//! auditable in the event stream.

mod common;

use common::spawn_mock_mcp;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Run reactive agentd for `run_ms` against the HTTP mock MCP server (`emit`
/// controls the post-subscribe push), then return the captured stderr telemetry.
fn run_reactive_capture(emit: bool, run_ms: u64) -> String {
    let exe = env!("CARGO_BIN_EXE_agentd");
    // The mock MCP server runs as a separate process; agentd connects over its
    // unix-socket HTTP endpoint (v2.0.0 — no stdio spawn).
    let mock = spawn_mock_mcp("file:///in.json", emit);

    let mut child = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react to the changed resource",
            "--intelligence",
            "http://127.0.0.1:9",
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
        .expect("spawn agentd reactive");

    // Drain telemetry on a thread so the read never blocks the test.
    let mut stderr = child.stderr.take().expect("stderr piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    std::thread::sleep(Duration::from_millis(run_ms));
    let _ = child.kill();
    let _ = child.wait();
    reader.join().unwrap_or_default()
}

#[test]
fn reactive_observably_reacts_to_a_resource_update() {
    // Mock pushes a resources/updated on the GET SSE stream after subscribe.
    let out = run_reactive_capture(true, 1800);

    assert!(
        out.contains(r#""event":"subscribe""#),
        "no subscribe event:\n{out}"
    );
    assert!(
        out.contains(r#""event":"resource.updated""#),
        "no resource.updated event:\n{out}"
    );
    assert!(
        out.contains(r#""event":"trigger.fired""#),
        "no trigger.fired event:\n{out}"
    );
    // The reaction ran a real subagent (its own agent_path under the run).
    assert!(
        out.contains(r#""event":"subagent.spawn""#),
        "no reaction subagent.spawn:\n{out}"
    );
}

#[test]
fn trace_context_propagates_across_the_agent_tree() {
    // Run with an upstream traceparent; its trace id must appear on BOTH the
    // supervisor's and the spawned subagent's log lines — one auditable trace
    // for the whole run (RFC 0010 §context-propagation).
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mock = spawn_mock_mcp("file:///in.json", true);
    let trace_id = "1234567890abcdef1234567890abcdef";
    let traceparent = format!("00-{trace_id}-1111111111111111-01");

    let mut child = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            "http://127.0.0.1:9",
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mock.mcp_arg("mock"),
            "--traceparent",
            &traceparent,
            "--log-level",
            "info",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd reactive");

    let mut stderr = child.stderr.take().expect("stderr");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });
    std::thread::sleep(Duration::from_millis(1500));
    let _ = child.kill();
    let _ = child.wait();
    let out = reader.join().unwrap_or_default();

    let tid_field = format!(r#""trace_id":"{trace_id}""#);
    // present on supervisor-emitted lines
    let on_supervisor = out
        .lines()
        .any(|l| l.contains(r#""comp":"supervisor""#) && l.contains(&tid_field));
    // present on subagent-emitted lines
    let on_agent = out
        .lines()
        .any(|l| l.contains(r#""comp":"agent""#) && l.contains(&tid_field));
    assert!(
        on_supervisor,
        "upstream trace id not on supervisor lines:\n{out}"
    );
    assert!(
        on_agent,
        "upstream trace id not propagated to subagent lines:\n{out}"
    );
}

/// Start the built-in mock LLM (`final` script → one ReAct turn reporting 16
/// tokens), announcing its loopback address through `addr_file`. Returns the
/// child and the `http://<addr>` intelligence URL.
fn start_mock_llm(addr_file: &Path) -> (Child, String) {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let child = Command::new(exe)
        .args(["--internal-mock-llm", addr_file.to_str().unwrap(), "final"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !addr_file.exists() {
        assert!(
            Instant::now() < deadline,
            "mock-llm never announced address"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let addr = std::fs::read_to_string(addr_file).expect("read mock-llm addr");
    (child, format!("http://{}", addr.trim()))
}

#[test]
fn reactive_daemon_drains_when_the_lifetime_budget_is_exhausted() {
    // RFC 0025: a per-INSTANCE lifetime token budget bounds a reactive daemon
    // across reactions. With a 1-token cap and a working mock LLM (each turn
    // reports 16 tokens), the first reaction exhausts the budget; the daemon then
    // stops accepting new reactions and drains cleanly (exit 0) on its own — no
    // external kill.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().unwrap();
    let (mut llm, intel) = start_mock_llm(&dir.path().join("llm.addr"));
    let mock = spawn_mock_mcp("file:///in.json", true);

    let mut child = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react to the changed resource",
            "--intelligence",
            &intel,
            "--budget-tokens-lifetime",
            "1",
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
        .expect("spawn agentd reactive");

    let mut stderr = child.stderr.take().expect("stderr piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // The daemon should exit ON ITS OWN (a budget drain) well within this window.
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break Some(st);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let self_exited = status.is_some();
    if !self_exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    let out = reader.join().unwrap_or_default();
    let _ = llm.kill();

    assert!(self_exited, "daemon did not drain on its own:\n{out}");
    assert_eq!(
        status.unwrap().code(),
        Some(0),
        "a default budget drain exits 0 (clean):\n{out}"
    );
    // The budget armed, the reactor charged past the cap, and the loop drained.
    assert!(
        out.contains(r#""event":"budget.armed""#),
        "no budget.armed:\n{out}"
    );
    assert!(
        out.contains(r#""event":"budget.exhausted""#)
            && out.contains(r#""limit":"tokens_lifetime""#),
        "no budget.exhausted(tokens_lifetime):\n{out}"
    );
    assert!(
        out.contains(r#""reason":"budget""#),
        "drain did not report the budget disposition:\n{out}"
    );
}

#[test]
fn read_after_subscribe_reacts_without_an_emitted_update() {
    // `emit=false`: the mock pushes NO update. The agent must still react to the
    // resource's current state on startup (read-after-subscribe, §2.8).
    let out = run_reactive_capture(false, 1500);

    assert!(
        out.contains(r#""event":"subscribe""#),
        "no subscribe event:\n{out}"
    );
    assert!(
        out.contains(r#""event":"reactive.initial_read""#),
        "no read-after-subscribe:\n{out}"
    );
    assert!(
        out.contains(r#""event":"trigger.fired""#),
        "no trigger.fired event:\n{out}"
    );
    // No real notification arrived, so there must be no resource.updated event.
    assert!(
        !out.contains(r#""event":"resource.updated""#),
        "unexpected resource.updated:\n{out}"
    );
}
