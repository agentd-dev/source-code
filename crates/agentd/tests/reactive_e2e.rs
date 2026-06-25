//! Observe-to-validate E2E test of reactivity (M3 / the M7 observe-suite seed).
//!
//! Runs a real reactive agentd against the built-in mock MCP server, then
//! validates the behaviour **by observing agentd's JSON-lines telemetry**:
//! it must subscribe to the resource, receive the mock's `resources/updated`,
//! read the current state, and fire a reaction. No live LLM needed — the
//! reaction's subagent fails on an unreachable intelligence endpoint, but the
//! reactive *behaviour* is fully visible and auditable in the event stream.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn reactive_observably_reacts_to_a_resource_update() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    // The mock MCP server is the agentd binary itself in its hidden mock mode.
    let mcp = format!("mock={exe} --internal-mock-mcp file:///in.json");

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

    // Give it time to: connect MCP → subscribe → receive the update (mock emits
    // ~200ms after subscribe) → debounce (250ms) → read → fire a reaction.
    std::thread::sleep(Duration::from_millis(1800));
    let _ = child.kill();
    let _ = child.wait();
    let out = reader.join().unwrap_or_default();

    // Validate the reactive behaviour purely by what is observable in the logs.
    assert!(out.contains(r#""event":"subscribe""#), "no subscribe event:\n{out}");
    assert!(out.contains(r#""event":"resource.updated""#), "no resource.updated event:\n{out}");
    assert!(out.contains(r#""event":"trigger.fired""#), "no trigger.fired event:\n{out}");
    // The reaction ran a real subagent (its own agent_path under the run).
    assert!(out.contains(r#""event":"subagent.spawn""#), "no reaction subagent.spawn:\n{out}");
}
