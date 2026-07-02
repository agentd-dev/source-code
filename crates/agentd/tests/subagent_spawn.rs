// SPDX-License-Identifier: Apache-2.0
//! End-to-end test of the supervisor↔subagent process plumbing (M2).
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
        intelligence: IntelConfig {
            uri: "http://127.0.0.1:9".into(),
            token: None,
            model: Some("test-model".into()),
        },
        mcp_servers: Vec::new(),
        a2a_peers: Vec::new(),
        tls_ca: None,
        limits: Limits {
            max_steps: 3,
            max_tokens: 10_000,
            deadline_ms: 10_000,
            max_depth: 4,
        },
        telemetry: Telemetry {
            run_id: "itest".into(),
            agent_id: "0".into(),
            agent_path: "0".into(),
            trace_id: None,
            log_level: "error".into(),
            log_content: false,
        },
        depth: 0,
        warm: false,
        #[cfg(feature = "workflow")]
        workflow: None,
        #[cfg(feature = "workflow")]
        workflow_reactive: false,
        #[cfg(feature = "workflow")]
        workflow_resume: None,
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
    let mut sub = spawn(Path::new(exe), &payload, node, tx).expect("spawn warm subagent");

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

#[test]
fn subagent_round_trips_ready_then_failed() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mut tree = Tree::new(Caps::default());
    let node = tree.mint_root().unwrap();

    let (tx, rx) = mpsc::channel();
    let _sub = spawn(Path::new(exe), &bogus_payload(), node, tx).expect("spawn subagent");

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
