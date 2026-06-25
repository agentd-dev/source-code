//! End-to-end test of the supervisor↔subagent process plumbing (M2).
//!
//! Launches a *real* `agentd` subagent process (re-exec of the built binary),
//! delivers a `SpawnPayload`, and asserts the round trip over the merged
//! event channel: the child emits `Ready`, then — because its intelligence
//! endpoint is unreachable — a terminal `Failed`. Exercises re-exec + payload
//! delivery + `main` dispatch + `subagent::control` without a live LLM.

use agentd::subagent::protocol::{AgentMsg, IntelConfig, Limits, SpawnPayload, Telemetry};
use agentd::supervisor::spawn::spawn;
use agentd::supervisor::tree::{Caps, NodeId, Tree};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn bogus_payload() -> SpawnPayload {
    SpawnPayload {
        instruction: "summarize the situation".into(),
        output_contract: None,
        context_seed: Vec::new(),
        intelligence: IntelConfig {
            uri: "unix:/nonexistent/agentd-subagent-test.sock".into(),
            token: None,
            model: Some("test-model".into()),
        },
        mcp_servers: Vec::new(),
        limits: Limits { max_steps: 3, max_tokens: 10_000, deadline_ms: 10_000, max_depth: 4 },
        telemetry: Telemetry {
            run_id: "itest".into(),
            agent_id: "0".into(),
            agent_path: "0".into(),
            trace_id: None,
            log_level: "error".into(),
        },
        depth: 0,
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

#[test]
fn subagent_round_trips_ready_then_failed() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mut tree = Tree::new(Caps::default());
    let node = tree.mint_root().unwrap();

    let (tx, rx) = mpsc::channel();
    let _sub = spawn(Path::new(exe), &bogus_payload(), node, tx).expect("spawn subagent");

    let msgs = drain_to_terminal(&rx, Instant::now() + Duration::from_secs(20));

    assert!(matches!(msgs.first(), Some(AgentMsg::Ready)), "expected Ready first, got {msgs:?}");
    match msgs.last() {
        Some(AgentMsg::Failed { error }) => {
            assert!(error.contains("intel"), "expected an intel failure, got: {error}")
        }
        other => panic!("expected terminal Failed, got {other:?}"),
    }
    // Dropping `_sub` kills + reaps the process group (no leak).
}
