//! End-to-end test of the supervisor↔subagent process plumbing (M2).
//!
//! Launches a *real* `agentd` subagent process (re-exec of the built binary),
//! delivers a `SpawnPayload` over the control channel, and asserts the round
//! trip: the child emits `Ready`, then — because its intelligence endpoint is
//! unreachable — a terminal `Failed`. This exercises re-exec + payload
//! delivery + `main` dispatch + `subagent::control` without needing a live LLM.

use agentd::subagent::protocol::{AgentMsg, IntelConfig, Limits, SpawnPayload, Telemetry};
use agentd::supervisor::spawn::{spawn, Terminal};
use agentd::supervisor::tree::{Caps, Tree};
use std::path::Path;
use std::time::{Duration, Instant};

fn bogus_payload() -> SpawnPayload {
    SpawnPayload {
        instruction: "summarize the situation".into(),
        output_contract: None,
        context_seed: Vec::new(),
        intelligence: IntelConfig {
            // A unix socket that does not exist → the first model call fails.
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

#[test]
fn subagent_round_trips_ready_then_failed() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mut tree = Tree::new(Caps::default());
    let node = tree.mint_root().unwrap();

    let sub = spawn(Path::new(exe), &bogus_payload(), node).expect("spawn subagent");

    // First frame up should be Ready (sent before the loop starts).
    let first = sub.recv_timeout(Duration::from_secs(10)).expect("a Ready message");
    assert!(matches!(first, AgentMsg::Ready), "expected Ready, got {first:?}");

    // Then a terminal Failed, because intelligence is unreachable.
    match sub.wait_terminal(Instant::now() + Duration::from_secs(15)) {
        Terminal::Failed(e) => assert!(e.contains("intel"), "expected an intel failure, got: {e}"),
        other => panic!("expected Terminal::Failed, got {other:?}"),
    }
    // Dropping `sub` kills + reaps the process group (no leak).
}
