// SPDX-License-Identifier: Apache-2.0
//! Observe-to-validate E2E tests of reactivity (M3 / the M7 observe-suite seed).
//!
//! Runs a real reactive agentd against the built-in mock MCP server and
//! validates the behaviour **by observing agentd's JSON-lines telemetry**. No
//! live LLM needed — each reaction's subagent fails on an unreachable
//! intelligence endpoint, but the reactive *behaviour* is fully visible and
//! auditable in the event stream.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Run reactive agentd for `run_ms` against the mock MCP server (extra mock
/// args appended via `mock_args`), then return the captured stderr telemetry.
fn run_reactive_capture(mock_args: &str, run_ms: u64) -> String {
    let exe = env!("CARGO_BIN_EXE_agent");
    // The mock MCP server is the agentd binary itself in its hidden mock mode.
    let mcp = format!("mock={exe} --internal-mock-mcp file:///in.json{mock_args}");

    let mut child = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react to the changed resource",
            "--intelligence",
            "unix:/nonexistent/agentd-react.sock",
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mcp,
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
    // Mock emits a resources/updated ~200ms after subscribe.
    let out = run_reactive_capture("", 1800);

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
    let exe = env!("CARGO_BIN_EXE_agent");
    let mcp = format!("mock={exe} --internal-mock-mcp file:///in.json");
    let trace_id = "1234567890abcdef1234567890abcdef";
    let traceparent = format!("00-{trace_id}-1111111111111111-01");

    let mut child = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            "unix:/nonexistent.sock",
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mcp,
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

#[test]
fn read_after_subscribe_reacts_without_an_emitted_update() {
    // `--no-emit`: the mock pushes NO update. The agent must still react to the
    // resource's current state on startup (read-after-subscribe, §2.8).
    let out = run_reactive_capture(" --no-emit", 1500);

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
