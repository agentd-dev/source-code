//! End-to-end test of self-orchestration (M2): the `subagent.spawn` self-tool.
//!
//! Drives the `Orchestrator` directly (standing in for the model calling the
//! tool). It must spawn a *real* child agent process, supervise it via
//! `supervise_once`, and — because the child's intelligence is unreachable —
//! return a tool result flagged as an error mentioning the failure. This
//! exercises the whole nested path: Orchestrator → supervise_once → spawn child
//! process → child `control::run` → child fails → distilled result back.

use agentd::agentloop::action::SelfHandler;
use agentd::obs::log::{Comp, Level, LogCtx, Logger};
use agentd::subagent::orchestrator::Orchestrator;
use agentd::subagent::protocol::{IntelConfig, Limits, SpawnPayload, Telemetry};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

fn logger() -> Logger {
    Logger::new(
        LogCtx {
            run_id: "itest".into(),
            agent_id: "0".into(),
            agent_path: "0".into(),
            comp: Comp::Agent,
            pid: std::process::id(),
            trace_id: None,
        },
        Level::Error,
    )
}

fn parent_payload() -> SpawnPayload {
    SpawnPayload {
        instruction: "parent task".into(),
        output_contract: None,
        context_seed: Vec::new(),
        intelligence: IntelConfig {
            uri: "unix:/nonexistent/agentd-orch-test.sock".into(),
            token: None,
            model: Some("m".into()),
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
fn subagent_spawn_runs_a_real_child() {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_agentd"));
    let mut orch = Orchestrator::from_payload(exe, &parent_payload(), Duration::from_secs(15), logger());

    // The parent (depth 0, max_depth 4) can delegate, so the tool is advertised.
    assert_eq!(orch.tools().len(), 1, "subagent.spawn should be advertised");

    let (content, is_error) = orch
        .handle("subagent.spawn", &json!({"instruction": "do a focused subtask"}))
        .expect("subagent.spawn is a self-tool");

    assert!(is_error, "child should report failure (unreachable intel)");
    assert!(
        content.contains("subagent failed") && content.contains("intel"),
        "expected a child intel failure, got: {content}"
    );
}
