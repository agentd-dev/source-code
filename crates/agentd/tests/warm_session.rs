//! Warm continue-session E2E (M3): a `Disposition::Continue` route delivers
//! every event into ONE live warm subagent — the first event spawns the session,
//! each later event injects into the SAME live process (no re-spawn), and the
//! agent runs one turn per event over a persistent transcript. Drives the real
//! `WarmRegistry` against the built-in mock LLM.
#![cfg(unix)]

use agentd::obs::log::{Comp, Level, LogCtx, Logger};
use agentd::subagent::protocol::{IntelConfig, Limits, SpawnPayload, Telemetry};
use agentd::triggers::warm::WarmRegistry;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_agentd")
}

fn start_mock_llm(socket: &Path) -> Child {
    let child = Command::new(exe())
        .args(["--internal-mock-llm", socket.to_str().unwrap(), "final"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "mock-llm never bound");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

fn logger() -> Logger {
    Logger::new(
        LogCtx {
            run_id: "warm".into(),
            agent_id: "0".into(),
            agent_path: "0".into(),
            comp: Comp::Supervisor,
            pid: 0,
            trace_id: None,
        },
        Level::Error,
    )
}

fn payload(sock: &Path) -> SpawnPayload {
    SpawnPayload {
        instruction: "react to the event".into(),
        output_contract: None,
        context_seed: Vec::new(),
        intelligence: IntelConfig {
            uri: format!("unix:{}", sock.display()),
            token: None,
            model: Some("m".into()),
        },
        mcp_servers: Vec::new(),
        limits: Limits { max_steps: 4, max_tokens: 10_000, deadline_ms: 10_000, max_depth: 4 },
        telemetry: Telemetry {
            run_id: "warm".into(),
            agent_id: "0".into(),
            agent_path: "0".into(),
            trace_id: None,
            log_level: "error".into(),
            log_content: false,
        },
        depth: 0,
        enable_exec: false,
        warm: false, // the registry forces warm = true
    }
}

/// Drain turns until `target` total have been seen, or the deadline.
fn drain_until(reg: &mut WarmRegistry, log: &Logger, have: usize, target: usize, deadline: Instant) -> usize {
    let mut turns = have;
    while turns < target && Instant::now() < deadline {
        turns += reg.drain(log).len();
        std::thread::sleep(Duration::from_millis(20));
    }
    turns
}

#[test]
fn continue_route_spawns_once_then_injects_into_the_same_session() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_mock_llm(&sock);
    let log = logger();
    let mut reg = WarmRegistry::default();
    let deadline = Instant::now() + Duration::from_secs(20);

    // First event → spawns the warm session.
    let spawned = reg.deliver(Path::new(exe()), "s1", payload(&sock), "first event", &log).expect("deliver 1");
    assert!(spawned, "the first delivery should spawn a warm session");
    assert_eq!(reg.len(), 1);

    let turns = drain_until(&mut reg, &log, 0, 1, deadline);
    assert!(turns >= 1, "the warm session should complete turn 1");

    // Second event on the same route → injected into the SAME live session.
    let spawned2 = reg.deliver(Path::new(exe()), "s1", payload(&sock), "second event", &log).expect("deliver 2");
    assert!(!spawned2, "the second delivery should inject, not spawn a new process");
    assert_eq!(reg.len(), 1, "still exactly one warm session");

    let turns = drain_until(&mut reg, &log, turns, 2, deadline);
    assert!(turns >= 2, "the SAME warm session should run a second turn from the injected event");

    // Graceful teardown: cancel → let it emit a terminal Result + exit.
    reg.cancel_all(&log);
    let end = Instant::now() + Duration::from_secs(10);
    while !reg.is_empty() && Instant::now() < end {
        let _ = reg.drain(&log);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(reg.is_empty(), "the warm session should wind down on cancel");

    reg.clear();
    let _ = llm.kill();
    let _ = llm.wait();
}
