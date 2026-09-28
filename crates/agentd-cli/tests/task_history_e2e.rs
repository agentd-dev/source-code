// SPDX-License-Identifier: AGPL-3.0-only
//! **`Task.history` over the wire**: the conversation a task carries, read
//! back with nothing but the core methods.
//!
//! A stock A2A client once saw the agent's answer and never the turn that
//! produced it — the prompt reached other clients only through a private feed
//! event, and the transcript only through a debug read. The assertions here
//! are what a spec client relies on instead: the caller's message under the id
//! it sent, the agent's question under the id it had as the status, the answer
//! after it, `historyLength` keeping the newest, and a command's task carrying
//! the command that opened it.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentd::runtime::surface::TASK_ANNOTATIONS_EXTENSION;
use serde_json::{Value, json};

use common::{SendMessage, rpc_result as rpc};

struct MockLlm {
    child: Child,
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
    let pb = common::unique_path("history-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("history-mock-llm", "addr");
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

struct Daemon {
    child: Child,
    stderr_path: String,
    cfg_path: String,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
        let _ = std::fs::remove_file(&self.cfg_path);
    }
}

/// A loopback daemon with no credential (so the test is the operator) and a
/// one-step workflow to run as a command. The probe→bind gap is a real race
/// under parallel CI, so a lost bind is retried.
fn boot(llm: &str) -> (Daemon, String) {
    for _ in 0..5 {
        let cfg_path = common::unique_path("history-e2e", "yaml");
        std::fs::write(
            &cfg_path,
            format!(
                "config_version: \"1\"\n\
                 agent:\n  name: history-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
                 intelligence:\n  endpoints: {llm}\n  model: mock\n\
                 store:\n  kind: memory\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n\
                 lifecycle:\n  run_until: drained\n\
                 observability:\n  log_level: info\n\
                 workflows:\n  - name: greet\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s], output: {{greeting: hello}}}}\n",
                port = common::free_port()
            ),
        )
        .unwrap();
        let stderr_path = common::unique_path("history-e2e", "log");
        let errf = std::fs::File::create(&stderr_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf))
            .spawn()
            .expect("spawn agentd");
        let daemon = Daemon {
            child,
            stderr_path,
            cfg_path,
        };
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(15)) {
            return (daemon, addr);
        }
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

/// Poll `GetTask` until `pred` holds.
fn wait_task(addr: &str, id: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let t = rpc(addr, 900, "GetTask", json!({"id": id}));
        if pred(&t) {
            return t;
        }
        assert!(Instant::now() < deadline, "timeout: {what}; last: {t}");
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// The `messageId`s of a task's history, oldest first.
fn history_ids(task: &Value) -> Vec<String> {
    task["history"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| m["messageId"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn history_carries_the_turn() {
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Proceed?"}}]},
            {"content": "Proceeded."}
        ]
    }));
    let (_daemon, addr) = boot(&llm.uri);

    // The prompt, under the id the caller gave it.
    let prompt = SendMessage::text("Do the thing").return_immediately();
    let prompt_id = prompt.params()["message"]["messageId"]
        .as_str()
        .unwrap()
        .to_string();
    let sent = prompt.result(&addr);
    let task_id = sent["task"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        history_ids(&sent["task"]),
        [prompt_id.as_str()],
        "the reply to the send already carries the prompt: {sent}"
    );

    // Gated: the question is the status; history is the prompt alone, as the
    // caller sent it, addressed to the task it opened.
    let gated = wait_task(&addr, &task_id, "the gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    assert_eq!(history_ids(&gated), [prompt_id.as_str()], "{gated}");
    let first = &gated["history"][0];
    assert_eq!(first["role"], "ROLE_USER");
    assert_eq!(first["taskId"], task_id.as_str());
    assert_eq!(first["contextId"], gated["contextId"]);
    assert_eq!(first["parts"][0]["text"], "Do the thing");
    let question_id = gated["status"]["message"]["messageId"]
        .as_str()
        .expect("the question is a message with an id")
        .to_string();

    // The answer resumes the turn; afterwards history reads prompt, question,
    // answer — the question under the id it had while it was the status.
    let answer = SendMessage::text("yes").task(&task_id).return_immediately();
    let answer_id = answer.params()["message"]["messageId"]
        .as_str()
        .unwrap()
        .to_string();
    answer.result(&addr);
    let done = wait_task(&addr, &task_id, "completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    let ids = history_ids(&done);
    assert_eq!(
        ids[..3],
        [prompt_id.as_str(), question_id.as_str(), answer_id.as_str()],
        "{done}"
    );
    assert_eq!(done["history"][1]["role"], "ROLE_AGENT");
    assert_eq!(done["history"][1]["parts"][0]["text"], "Proceed?");
    assert_eq!(done["history"][2]["role"], "ROLE_USER");
    assert_eq!(done["history"][2]["parts"][0]["text"], "yes");
    // The reply is the artifact, not a history entry.
    assert_eq!(done["artifacts"][0]["parts"][0]["text"], "Proceeded.");
    assert!(
        !done["history"].to_string().contains("Proceeded."),
        "{done}"
    );

    // `historyLength` keeps the newest; 0 keeps none.
    let one = rpc(
        &addr,
        5,
        "GetTask",
        json!({"id": task_id, "historyLength": 1}),
    );
    assert_eq!(history_ids(&one), [ids.last().unwrap().clone()], "{one}");
    let none = rpc(
        &addr,
        6,
        "GetTask",
        json!({"id": task_id, "historyLength": 0}),
    );
    assert!(none.get("history").is_none(), "{none}");
    // A listing carries history only when asked, and then the newest.
    let listed = rpc(&addr, 7, "ListTasks", json!({"historyLength": 2}));
    let row = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == task_id.as_str())
        .expect("the task is listed")
        .clone();
    assert_eq!(history_ids(&row), ids[ids.len() - 2..], "{row}");
    let bare = rpc(&addr, 8, "ListTasks", json!({}));
    assert!(
        bare["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t.get("history").is_none()),
        "{bare}"
    );

    // A command's task carries the command message that opened it, and says
    // which op it runs; its structured result is data, not text.
    let run = SendMessage::command("workflow.run", json!({"workflow": "greet"}));
    let run_id = run.params()["message"]["messageId"]
        .as_str()
        .unwrap()
        .to_string();
    let started = run.result(&addr);
    let run_task = started["task"]["id"].as_str().unwrap().to_string();
    let finished = wait_task(&addr, &run_task, "the run", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert_eq!(history_ids(&finished)[0], run_id, "{finished}");
    assert_eq!(
        finished["history"][0]["parts"][0]["data"]["agentd"]["op"],
        "workflow.run"
    );
    assert_eq!(
        finished["metadata"][TASK_ANNOTATIONS_EXTENSION]["command"], "workflow.run",
        "{finished}"
    );
    let part = &finished["artifacts"][0]["parts"][0];
    assert_eq!(part["data"]["greeting"], "hello", "{finished}");
    assert_eq!(part["mediaType"], "application/json");
    assert_eq!(
        finished["artifacts"][0]["extensions"],
        json!([agentd::runtime::surface::COMMAND_EXTENSION])
    );
}
