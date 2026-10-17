// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end test of the supervisor↔subagent process plumbing.
//!
//! Launches a *real* `agentd` subagent process (re-exec of the built binary),
//! delivers a `SpawnPayload`, and asserts the round trip over the merged
//! event channel: the child emits `Ready`, then — because its intelligence
//! endpoint is unreachable — a terminal `Failed`. Exercises re-exec + payload
//! delivery + `main` dispatch + `subagent::control` without a live LLM.

use agentd::subagent::protocol::{
    AgentMsg, ControlMsg, IntelConfig, Limits, SpawnPayload, Telemetry,
};
use agentd::supervisor::spawn::spawn;
use agentd::supervisor::tree::{Caps, NodeId, Tree};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn bogus_payload() -> SpawnPayload {
    SpawnPayload {
        instruction: "summarize the situation".into(),
        output_contract: None,
        context_seed: Vec::new(),
        gated_tools: Vec::new(),
        mcp_routes: None,
        intelligence: IntelConfig {
            uri: "http://127.0.0.1:9".into(),
            token: None,
            model: Some("test-model".into()),
            ..Default::default()
        },
        mcp_servers: Vec::new(),
        a2a_peers: Vec::new(),
        tls_ca: None,
        aauth: None,
        limits: Limits {
            max_steps: 3,
            max_tokens: 10_000,
            deadline_ms: 10_000,
            max_depth: 4,
            memory_bytes: None,
            cpu_seconds: None,
            nice: None,
        },
        telemetry: Telemetry {
            run_id: "itest".into(),
            instance: "itest".into(),
            agent_id: "0".into(),
            agent_path: "0".into(),
            traceparent: None,
            tracestate: None,
            log_level: "error".into(),
            log_content: false,
            otel_endpoint: None,
        },
        depth: 0,
        warm: false,
        role: agentd::subagent::protocol::Role::Agent,
        turn: None,
    }
}

/// Read messages until a terminal one, or the deadline.
fn drain_to_terminal(rx: &mpsc::Receiver<(NodeId, AgentMsg)>, deadline: Instant) -> Vec<AgentMsg> {
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok((_node, msg)) => {
                let terminal = matches!(msg, AgentMsg::Result { .. } | AgentMsg::Failed { .. });
                seen.push(msg);
                if terminal {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    seen
}

/// Start the built-in mock LLM (`final` script) on a unix socket; wait until it
/// binds. The subagent's intel calls then succeed without a live model.
fn start_mock_llm(exe: &str, socket: &Path) -> (Child, String) {
    let child = Command::new(exe)
        .args(["--internal-mock-llm", socket.to_str().unwrap(), "final"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "mock-llm never announced");
        std::thread::sleep(Duration::from_millis(20));
    }
    let addr = std::fs::read_to_string(socket).expect("read mock-llm addr-file");
    (child, format!("http://{}", addr.trim()))
}

/// Scan upward frames (ignoring Ready/Pong/Event/Usage) until one of `kind`
/// (`turn`|`result`|`failed`) arrives, or the deadline. Returns the kind seen.
fn recv_kind(rx: &mpsc::Receiver<(NodeId, AgentMsg)>, kind: &str, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok((_, msg)) => {
                let m = match &msg {
                    AgentMsg::Ready => "ready",
                    AgentMsg::Pong { .. } => "pong",
                    AgentMsg::Event { .. } => "event",
                    AgentMsg::Usage(_) => "usage",
                    AgentMsg::Turn { .. } => "turn",
                    AgentMsg::Result { .. } => "result",
                    AgentMsg::Failed { .. } => "failed",
                    AgentMsg::IntelHealth { .. } => "intel_health",
                    AgentMsg::Gate { .. } => "gate",
                    AgentMsg::GateClosed { .. } => "gate_closed",
                    AgentMsg::ToolRequest { .. } => "tool_request",
                    AgentMsg::BudgetRequest { .. } => "budget_request",
                    AgentMsg::TurnDone { .. } => "turn_done",
                    AgentMsg::Metrics(_) => "metrics",
                    AgentMsg::Credential { .. } => "credential",
                    AgentMsg::AAuthToken { .. } => "aauth_token",
                };
                if m == kind {
                    return true;
                }
                if m == "failed" {
                    return false; // an unexpected terminal failure
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
    false
}

#[test]
fn warm_session_runs_a_turn_per_injected_event_then_ends_on_cancel() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.addr");
    let (mut llm, intel) = start_mock_llm(exe, &sock);

    let mut payload = bogus_payload();
    payload.intelligence.uri = intel;
    payload.warm = true;

    let mut tree = Tree::new(Caps::default());
    let node = tree.mint_root().unwrap();
    let (tx, rx) = mpsc::channel();
    let sink: agentd::supervisor::spawn::FrameSink = {
        let tx = tx.clone();
        std::sync::Arc::new(move |n, m| tx.send((n, m)).is_ok())
    };
    let mut sub = spawn(Path::new(exe), &payload, node, sink).expect("spawn warm subagent");

    // Turn 1 reacts to the payload's instruction, completes, and the session
    // stays warm (no terminal yet).
    assert!(
        recv_kind(&rx, "turn", Instant::now() + Duration::from_secs(20)),
        "expected a Turn after the first (payload) event"
    );

    // Deliver a second event → a second turn over the SAME live session.
    sub.send(&ControlMsg::Inject {
        message: "now the second thing".into(),
    })
    .expect("inject");
    assert!(
        recv_kind(&rx, "turn", Instant::now() + Duration::from_secs(20)),
        "expected a second Turn after Inject (warm re-entry)"
    );

    // Cancel → the warm session winds down with a terminal Result.
    sub.send(&ControlMsg::Cancel {
        reason: "test done".into(),
    })
    .expect("cancel");
    assert!(
        recv_kind(&rx, "result", Instant::now() + Duration::from_secs(20)),
        "expected a terminal Result after Cancel"
    );

    let _ = llm.kill();
    let _ = llm.wait();
    // Dropping `sub` kills + reaps the (already-exiting) subagent group.
}

/// A child gives up at start only when the process that spawned it is gone:
/// its parent is compared with the pid the spawner named, not with 1, so a
/// supervisor that is PID 1 — a container's entrypoint, a pod's — keeps its
/// children; every turn there exited 1 before it began. Here the test is the
/// parent. An empty stdin is not a spawn payload, so a child that stayed says
/// so and exits 2; one that finds another parent than it was told exits 1 at
/// once. (PID 1 itself is the unit test in `supervisor::reap`.)
#[test]
fn a_child_stays_while_the_pid_its_spawner_named_is_its_parent() {
    let run = |parent: String| {
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .env(agentd::subagent::protocol::SUBAGENT_ENV, "1")
            .env(agentd::supervisor::reap::PARENT_PID_ENV, parent)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("run the child");
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        (out.status.code(), err)
    };
    let (code, err) = run(std::process::id().to_string());
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("bad spawn payload"), "{err}");
    // Told a parent it does not have (above any pid Linux hands out): it was
    // reparented, so it exits.
    let (code, err) = run(i32::MAX.to_string());
    assert_eq!(code, Some(1), "{err}");
    assert!(!err.contains("bad spawn payload"), "{err}");
}

#[test]
fn subagent_round_trips_ready_then_failed() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mut tree = Tree::new(Caps::default());
    let node = tree.mint_root().unwrap();

    let (tx, rx) = mpsc::channel();
    let sink: agentd::supervisor::spawn::FrameSink = {
        let tx = tx.clone();
        std::sync::Arc::new(move |n, m| tx.send((n, m)).is_ok())
    };
    let _sub = spawn(Path::new(exe), &bogus_payload(), node, sink).expect("spawn subagent");

    let msgs = drain_to_terminal(&rx, Instant::now() + Duration::from_secs(20));

    assert!(
        matches!(msgs.first(), Some(AgentMsg::Ready)),
        "expected Ready first, got {msgs:?}"
    );
    match msgs.last() {
        Some(AgentMsg::Failed { error }) => {
            assert!(
                error.contains("intel"),
                "expected an intel failure, got: {error}"
            )
        }
        other => panic!("expected terminal Failed, got {other:?}"),
    }
    // Dropping `_sub` kills + reaps the process group (no leak).
}
