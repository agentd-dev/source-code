// SPDX-License-Identifier: Apache-2.0
//! **`--prompt` and self-setup**: a one-shot prompt is a complete job, and a
//! prompt that tells the agent to build its own recurring work leaves a live
//! instance behind instead of exiting out from under it.
//!
//! The second case is the interesting one. `lifecycle.run_until: auto` decides
//! "job or daemon" — and it used to decide it once, at startup, from the
//! CONFIGURED workflows. An agent that answered a prompt by calling
//! `workflow.create` with a `loop` start node therefore armed a workflow and
//! was then idle-exited a moment later. `auto` now re-reads the live set.
#![cfg(unix)]

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Run the mock LLM with a playbook, returning its base URI and a guard.
struct MockLlm {
    child: std::process::Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}

fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("prompt-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("prompt-mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

#[test]
fn a_prompt_runs_once_and_prints_the_answer() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "42 open issues, 3 stale."}]}));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--prompt",
            "How many open issues?",
            "--intelligence",
            &llm.uri,
        ])
        .output()
        .expect("run agentd");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("42 open issues"),
        "the answer rides stdout: {stdout}"
    );
}

/// The self-setup shape: the prompt asks for recurring work, the agent defines
/// it with `workflow.create`, and the instance must STAY UP to run it.
#[test]
fn a_prompt_that_sets_up_recurring_work_leaves_a_live_daemon() {
    // Turn 1: define a loop workflow. Turn 2: report what it did.
    let playbook = json!({"turns": [
        {"tool_calls": [{"name": "workflow.create", "arguments": {"definition": {
            "name": "watcher",
            "version": 3,
            "steps": {
                "tick": {"kind": "loop", "interval": "30s"},
                "note": {"kind": "finish", "depends_on": ["tick"], "status": "completed", "output": "checked"}
            }
        }}}]},
        {"content": "Set up 'watcher' to run every 30s."}
    ]});
    let llm = spawn_mock_llm(&playbook);

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--prompt",
            "Check the queue every 30 seconds from now on.",
            "--intelligence",
            &llm.uri,
            // Defining a workflow persists its definition, so the instance
            // needs a store; `memory` is enough to prove the lifecycle.
            "--store-kind",
            "memory",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentd");

    // A one-shot would be gone in well under this; a daemon is still here.
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut exited_early = None;
    while Instant::now() < deadline {
        if let Ok(Some(st)) = child.try_wait() {
            exited_early = Some(st);
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let alive = exited_early.is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        alive,
        "the instance exited ({exited_early:?}) despite arming a loop workflow — \
         `auto` must re-read the live workflow set, not just the configured one"
    );
}
