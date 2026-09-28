// SPDX-License-Identifier: AGPL-3.0-only
//! **Human-in-the-loop** end to end: the `ask_human` tool and the workflow
//! `human` node gate as `input-required` A2A tasks; a `SendMessage` carrying
//! the `taskId` resolves the suspended asker with the reply text.
//!
//! Ownership decides who can be asked, not any display switch. An ask whose
//! turn or run a caller OWNS gates on core A2A with the observation feed and
//! introspection both off — the caller's blocking send returns at the gate.
//! An ask nobody owns (here: the configured `agent.prompt`, which no caller
//! sent, and a subagent's policy gate) gates only with
//! `agent.ask_human_unowned: gate`; otherwise the configured fallback applies
//! — `fail` errors the ask immediately (the model carries on), `wait` parks it
//! until its timeout, `auto` has an LLM judge answer on the operator's behalf
//! (marked as auto). A cancelled gate unblocks its asker with an error.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{SendMessage, rpc_result as rpc};

fn command(addr: &str, op: &str, args: Value) -> Value {
    SendMessage::command(op, args).result(addr)
}

/// A read op's document: its reply is a Message carrying it, and no task.
fn read(addr: &str, op: &str, args: Value) -> Value {
    let r = command(addr, op, args);
    assert!(r.get("task").is_none(), "{op} created a task: {r}");
    r["message"]["parts"][0]["data"].clone()
}

/// Poll GetTask until `pred` holds (returns the task).
fn wait_task<F: Fn(&Value) -> bool>(addr: &str, id: &str, secs: u64, what: &str, pred: F) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let t = rpc(addr, 900, "GetTask", json!({"id": id}));
        if pred(&t) {
            return t;
        }
        assert!(Instant::now() < deadline, "timeout: {what}; last: {t}");
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Poll the daemon's log until `needle` appears (returns the log).
fn wait_log(daemon: &Daemon, needle: &str, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let log = daemon.stderr();
        if log.contains(needle) {
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "timeout waiting for {needle}; log:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Every task in `input-required`, by id.
fn gates(addr: &str) -> Vec<String> {
    let tasks = rpc(addr, 901, "ListTasks", json!({}));
    // An empty list is omitted from the wire (proto3 JSON), not sent as `[]`.
    tasks["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED")
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect()
}

/// The agent section of [`base_config`] with an `agent.prompt` — a turn the
/// instance starts for itself, so no A2A caller owns it — plus `extra` agent
/// keys.
fn unowned(cfg: String, prompt: &str, extra: &str) -> String {
    cfg.replace(
        "  preflight: never\n",
        &format!("  preflight: never\n  prompt: {prompt}\n{extra}"),
    )
}

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
    let pb = common::unique_path("hitl-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("hitl-mock-llm", "addr");
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
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
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
    }
}
fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("hitl-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd daemon");
    Daemon { child, stderr_path }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-hitl", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

/// Spawn the daemon on a probed free port and return the authority IT actually
/// bound. The probe→bind gap is a real race under parallel CI (another process
/// can take the port), so a daemon whose bind lost is retried on a fresh port
/// rather than leaving the test talking to a stranger's listener.
fn spawn_bound(cfg_for: impl Fn(u16) -> String) -> (Daemon, String, String) {
    spawn_bound_with(cfg_for, spawn_daemon)
}

fn spawn_bound_with(
    cfg_for: impl Fn(u16) -> String,
    spawn: impl Fn(&str) -> Daemon,
) -> (Daemon, String, String) {
    for _ in 0..5 {
        let cfg = write_config(&cfg_for(free_port()));
        let daemon = spawn(&cfg);
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(15)) {
            return (daemon, addr, cfg);
        }
        std::fs::remove_file(&cfg).ok();
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

fn base_config(llm: &str, port: u16, feed: bool, extra: &str) -> String {
    let feed = if feed {
        "  events:\n    enabled: true\n  introspection:\n    enabled: true\n"
    } else {
        ""
    };
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: hitl-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{feed}\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n{extra}"
    )
}

#[test]
fn a_turn_ask_gates_as_input_required_and_the_reply_resumes_the_turn() {
    // Turn 1: the model asks the human; turn 2 (after the tool result): final.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Which color should the rollout badge be?"}}]},
            {"content": "Done — the badge is set."}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, true, ""));

    // Send the prompt WITHOUT blocking; the task must reach input-required
    // with the QUESTION as its status message.
    let sent = SendMessage::text("Set up the badge")
        .return_immediately()
        .result(&addr);
    let task_id = sent["task"]["id"].as_str().unwrap().to_string();
    let gated = wait_task(&addr, &task_id, 10, "gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    assert!(
        gated["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Which color"),
        "{gated}"
    );

    // The answer (carrying the taskId) resolves the gate; the turn finishes.
    let answered = SendMessage::text("blue")
        .task(&task_id)
        .return_immediately()
        .result(&addr);
    assert_eq!(
        answered["task"]["status"]["state"], "TASK_STATE_WORKING",
        "back to working after the answer: {answered}"
    );
    let done = wait_task(&addr, &task_id, 15, "turn completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    let text = done["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("badge is set"), "{done}");
    // The audit trail marks the human answer.
    let logs = daemon.stderr();
    assert!(logs.contains("human.ask"), "asked: {logs}");
    assert!(logs.contains("human.answered"), "answered: {logs}");
    std::fs::remove_file(&cfg).ok();
}

/// The core A2A flow, with nothing but the listener: no observation feed, no
/// introspection. The caller who sent the message owns the task, so their
/// BLOCKING send returns at `input-required` with the question, and a send
/// carrying the `taskId` answers it and the task completes. Deciding
/// availability from a display switch refused exactly this caller.
#[test]
fn an_owned_ask_gates_on_core_a2a_with_no_events_or_introspection() {
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Which region first?"}}]},
            {"content": "Rolled out to the chosen region."}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, false, ""));

    let gated = SendMessage::text("Roll it out").result(&addr);
    assert_eq!(
        gated["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "a blocking send returns at the gate: {gated}"
    );
    assert!(
        gated["task"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Which region first?"),
        "{gated}"
    );
    let task_id = gated["task"]["id"].as_str().unwrap().to_string();

    SendMessage::text("eu-west").task(&task_id).result(&addr);
    let done = wait_task(&addr, &task_id, 15, "turn completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(
        done["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("chosen region"),
        "{done}"
    );
    let logs = daemon.stderr();
    assert!(logs.contains("\"event\":\"human.answered\""), "{logs}");
    std::fs::remove_file(&cfg).ok();
}

/// An ask no caller owns has nobody waiting on it, so whether the operator is
/// asked is the deployment's call: by default it takes the fallback (`fail`
/// here — the ask errors and the model carries on, and no gate exists), and
/// with `agent.ask_human_unowned: gate` it opens a gate the operator answers.
#[test]
fn an_unowned_ask_takes_the_fallback_unless_ask_human_unowned_is_gate() {
    // The tool result reaches the model and nowhere else, so the mock reads
    // it: the reply says whether the error named the setting that chose it.
    let playbook = json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Anyone there?"}}]},
            {"content": "Carried on."}
        ],
        "match": [
            {"when_contains": "no A2A caller owns this ask and agent.ask_human_unowned is fallback",
             "content": "Carried on; the fallback named its setting."}
        ]
    });

    // Default (`fallback`, with `ask_human_fallback: fail`).
    let llm = spawn_mock_llm(&playbook);
    let (daemon, addr, cfg) =
        spawn_bound(|port| unowned(base_config(&llm.uri, port, false, ""), "Try asking", ""));
    let logs = wait_log(&daemon, "\"event\":\"turn.reply\"", 15);
    assert!(
        logs.contains("the fallback named its setting"),
        "the ask errored, naming the setting that chose the fallback\n{logs}"
    );
    assert!(gates(&addr).is_empty(), "no gate was opened");
    drop(daemon);
    std::fs::remove_file(&cfg).ok();

    // `gate`: the operator is asked, and answering completes the gate task.
    let llm = spawn_mock_llm(&playbook);
    let (daemon, addr, cfg) = spawn_bound(|port| {
        unowned(
            base_config(&llm.uri, port, false, ""),
            "Try asking",
            "  ask_human_unowned: gate\n",
        )
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let gate = loop {
        if let Some(g) = gates(&addr).pop() {
            break g;
        }
        assert!(
            Instant::now() < deadline,
            "no gate opened for the unowned ask\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(80));
    };
    SendMessage::text("yes, here")
        .task(&gate)
        .return_immediately()
        .result(&addr);
    wait_task(&addr, &gate, 10, "the gate completes", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    let logs = wait_log(&daemon, "\"event\":\"turn.reply\"", 15);
    assert!(
        logs.contains("\"text\":\"Carried on.\""),
        "the answer went back into the asking turn\n{logs}"
    );
    std::fs::remove_file(&cfg).ok();
}

/// A SUBAGENT's gate is unowned, even when the turn that spawned it is a
/// caller's: the subagent is its own unit, working for whichever unit spawned
/// it — possibly long after that unit's caller got its answer — so a policy
/// gate on its call opens only with `ask_human_unowned: gate`. By default it
/// takes the fallback (the rule's deny), and the caller's task never stops at
/// a gate it did not ask for.
#[test]
fn a_subagents_gate_is_unowned_even_under_an_owned_turn() {
    // Rule order matters: the child's later rounds still carry its system
    // prompt, so the fallback's words are matched first. The child is told
    // apart by its own system prompt, which the root's transcript never holds.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {"instruction": "note it down", "mode": "sync"}}]},
            {"content": "unreachable"}
        ],
        "match": [
            {"when_contains": "no A2A caller owns this ask and agent.ask_human_unowned is fallback",
             "content": "SUB_FELL_BACK"},
            {"when_contains": "SUB_FELL_BACK", "content": "the subagent's gate took the fallback"},
            {"when_contains": "You are agentd, an autonomous agent.",
             "tool_calls": [{"name": "memory.set", "arguments": {"key": "k", "value": 1}}]}
        ]
    }));
    let policy = "security:\n  policies:\n\
                  \x20   - match: { tool: \"memory.set\", caller: [subagent] }\n      action: ask\n";
    let (daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, false, policy));

    // A blocking send: had the subagent's gate opened on this task, it would
    // return at `input-required`.
    let done = SendMessage::text("Delegate it").result(&addr);
    assert_eq!(
        done["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "the caller's task stopped at the subagent's gate: {done}"
    );
    assert_eq!(
        done["task"]["artifacts"][0]["parts"][0]["text"],
        "the subagent's gate took the fallback",
        "{done}\n{}",
        daemon.stderr()
    );
    std::fs::remove_file(&cfg).ok();
}

/// A gate opened WHILE a request is being served never takes the task id
/// that request reserved for its own task.
///
/// The path: a `workflow.signal` releases a wait inside a `foreach` body, and
/// the body's next step — a call an `ask` policy gates, in a run nobody owns
/// — opens its gate before the signal command's own task is created. Had the
/// gate taken the reservation, the caller would be answered with a task it
/// never asked for, and handed a subscription to someone else's gate.
#[test]
fn a_gate_opened_mid_request_never_takes_the_requests_task_id() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let extra = "workflows:\n  - name: probe\n    steps:\n\
                 \x20     s: {kind: once}\n\
                 \x20     each: {kind: foreach, depends_on: [s], over: [1],\n\
                 \x20            body: {steps: {w: {kind: wait, on: signal, signal: go},\n\
                 \x20                           m: {kind: memory.set, key: k, value: v, depends_on: [w]}}}}\n\
                 \x20     f: {kind: finish, depends_on: [each]}\n\
                 security:\n  policies:\n\
                 \x20   - match: { tool: \"memory.set\" }\n      action: ask\n";
    let (daemon, addr, cfg) = spawn_bound(|port| {
        unowned(
            base_config(&llm.uri, port, true, extra),
            "hello",
            "  ask_human_unowned: gate\n",
        )
        .replace("  prompt: hello\n", "")
    });
    // The run is parked on the signal before it is sent.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ws = read(&addr, "workflow.status", json!({}));
        let run = ws["runs"][0]["run"]
            .as_str()
            .map(|id| read(&addr, "run.get", json!({"run": id})));
        if run
            .as_ref()
            .is_some_and(|r| r["run"]["steps"]["each[0].w"]["status"] == "suspended")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run never parked: {run:?}\n{}",
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(80));
    }

    let reserved = "task-signal-probe";
    let sent = SendMessage::command("workflow.signal", json!({"name": "go"}))
        .task(reserved)
        .result(&addr);
    assert_eq!(
        sent["task"]["id"],
        reserved,
        "the signal's task lost its id to the gate it opened: {sent}\n{}",
        daemon.stderr()
    );
    let open = gates(&addr);
    assert_eq!(
        open.len(),
        1,
        "the body's gate opened: {open:?}\n{}",
        daemon.stderr()
    );
    assert_ne!(open[0], reserved, "the gate took the reservation");
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_workflow_human_node_gates_the_run_task_and_the_reply_is_the_step_output() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let extra = "workflows:\n  - name: approve\n    steps:\n      s: {kind: manual}\n      gate: {kind: human, question: \"Approve the deploy?\", depends_on: [s]}\n      f: {kind: finish, depends_on: [gate], output: \"shipped\"}\n";
    let (_daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, true, extra));

    // Start the workflow; ITS task (linking the run) becomes the gate.
    let started = command(&addr, "workflow.run", json!({"workflow": "approve"}));
    let task_id = started["task"]["id"].as_str().unwrap().to_string();
    let gated = wait_task(&addr, &task_id, 10, "run gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    assert!(
        gated["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Approve the deploy?"),
        "{gated}"
    );

    // Answer → the human step completes with the reply as output; the run
    // finishes and drives the task terminal.
    SendMessage::text("yes, ship it")
        .task(&task_id)
        .return_immediately()
        .result(&addr);
    let done = wait_task(&addr, &task_id, 10, "run completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(
        done["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("shipped"),
        "{done}"
    );
    // Per-step detail (debug read): the gate step is Done with the reply.
    let ws = read(&addr, "workflow.status", json!({}));
    let run_id = ws["runs"][0]["run"]
        .as_str()
        .map(str::to_string)
        .expect("run id");
    let run = read(&addr, "run.get", json!({"run": run_id}));
    assert_eq!(run["run"]["steps"]["gate"]["status"], "done", "{run}");
    assert_eq!(
        run["run"]["steps"]["gate"]["output"], "yes, ship it",
        "{run}"
    );
    std::fs::remove_file(&cfg).ok();
}

/// A gate that names a decider is only worth declaring if someone else cannot
/// satisfy it. An OPERATOR can — refusing them would be theatre, since they
/// can already rewrite the config, the store or the definition — but the
/// override is marked and audited, so the record still names who really
/// decided rather than implying the addressee did.
#[test]
fn an_operator_answering_someone_elses_gate_is_recorded_as_an_override() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let extra = "workflows:\n  - name: approve\n    steps:\n      s: {kind: manual}\n      gate: {kind: human, question: \"Approve the refund?\", to: \"*@finance.example\", depends_on: [s]}\n      f: {kind: finish, depends_on: [gate], output: \"refunded\"}\n";
    let (daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, true, extra));

    let started = command(&addr, "workflow.run", json!({"workflow": "approve"}));
    let task_id = started["task"]["id"].as_str().unwrap().to_string();
    wait_task(&addr, &task_id, 10, "run gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });

    // The loopback caller is an operator, and is NOT the addressee.
    SendMessage::text("approved")
        .task(&task_id)
        .return_immediately()
        .result(&addr);
    wait_task(&addr, &task_id, 10, "run completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    let log = daemon.stderr();
    assert!(
        log.contains("\"event\":\"human.answer.override\""),
        "an operator answering another's gate must be marked as an override\n{log}"
    );
    assert!(
        log.contains("operator_override"),
        "and the answer must be recorded as one, not as the addressee deciding\n{log}"
    );
    std::fs::remove_file(&cfg).ok();
}

/// The gate's enforcement lives in the DURABLE wait record, not only in the
/// in-memory pending ask — a restart rebuilds the pending from that record, so
/// anything missing from it is silently dropped on restart. That mattered
/// before this was fixed: a gate demanding `{decision: …}` would accept
/// anything after a restart, and one naming a decider would accept anyone.
#[test]
fn a_gates_addressee_and_schema_are_durable() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let extra = "workflows:\n  - name: approve\n    steps:\n      s: {kind: manual}\n      gate: {kind: human, question: \"Approve?\", to: \"*@finance.example\", schema: {type: object, properties: {ok: {type: boolean}}}, depends_on: [s]}\n      f: {kind: finish, depends_on: [gate], output: \"done\"}\n";
    let (_daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, true, extra));

    let started = command(&addr, "workflow.run", json!({"workflow": "approve"}));
    let task_id = started["task"]["id"].as_str().unwrap().to_string();
    wait_task(&addr, &task_id, 10, "run gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });

    // Read the run back: the suspended step's wait record must carry both, or
    // a restart would rebuild a weaker gate than the one that was declared.
    let ws = read(&addr, "workflow.status", json!({}));
    let run_id = ws["runs"][0]["run"]
        .as_str()
        .map(str::to_string)
        .expect("run id");
    let run = read(&addr, "run.get", json!({"run": run_id}));
    let wait = &run["run"]["steps"]["gate"]["wait"];
    assert_eq!(wait["kind"], "human", "{run}");
    assert_eq!(
        wait["to"], "*@finance.example",
        "the addressee must survive a restart\n{run}"
    );
    assert!(
        wait["schema"]["properties"]["ok"].is_object(),
        "the answer schema must survive a restart\n{run}"
    );
    std::fs::remove_file(&cfg).ok();
}

/// An unowned ask with `ask_human_fallback: auto`: nobody is asked, and the
/// judge answers on the operator's behalf — marked as auto.
#[test]
fn an_unowned_ask_with_fallback_auto_lets_the_judge_answer_on_the_operators_behalf() {
    // The judge dial hits the same mock: route it by its system prompt.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Blue or green?"}}]},
            {"content": "Went ahead with the chosen color."}
        ],
        "match": [
            {"when_contains": "answering ON BEHALF OF the unavailable human operator", "content": "blue"}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(|port| {
        unowned(
            base_config(&llm.uri, port, false, ""),
            "Pick a color and proceed",
            "  ask_human_fallback: auto\n",
        )
    });
    let logs = wait_log(&daemon, "\"event\":\"turn.reply\"", 20);
    assert!(logs.contains("Went ahead"), "{logs}");
    assert!(logs.contains("human.judge.start"), "{logs}");
    assert!(
        logs.contains("\"via\":\"auto\"") || logs.contains("\"outcome\":\"auto\""),
        "the auto answer is marked: {logs}"
    );
    assert!(gates(&addr).is_empty(), "nobody was asked");
    std::fs::remove_file(&cfg).ok();
}

/// An unowned ask with `ask_human_fallback: wait`: it parks, times out (1s
/// here), errors, and the model carries on.
#[test]
fn an_unowned_ask_with_fallback_wait_parks_until_its_timeout() {
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Waiting?", "timeout": "1s"}}]},
            {"content": "Timed out; proceeding."}
        ]
    }));
    let (daemon, _addr, cfg) = spawn_bound(|port| {
        unowned(
            base_config(&llm.uri, port, false, ""),
            "Ask and wait",
            "  ask_human_fallback: wait\n",
        )
    });
    let logs = wait_log(&daemon, "\"event\":\"turn.reply\"", 20);
    assert!(logs.contains("Timed out; proceeding"), "{logs}");
    assert!(logs.contains("human.ask.parked"), "{logs}");
    assert!(
        logs.contains("no A2A caller owns this ask"),
        "the parked note names why nobody was asked\n{logs}"
    );
    assert!(logs.contains("no answer within the timeout"), "{logs}");
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn auto_fires_as_the_safety_net_when_an_owned_gate_times_out_unanswered() {
    // An owned gate + `auto`, with no feed at all: the gate opens for the
    // caller, nobody answers within the (1s) timeout, and the judge answers on
    // the operator's behalf.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Green or blue?", "timeout": "1s"}}]},
            {"content": "Color applied."}
        ],
        "match": [
            {"when_contains": "answering ON BEHALF OF the unavailable human operator", "content": "green"}
        ]
    }));
    let (daemon, addr, cfg) = spawn_bound(|port| {
        base_config(&llm.uri, port, false, "").replace(
            "  preflight: never\n",
            "  preflight: never\n  ask_human_fallback: auto\n",
        )
    });
    let sent = SendMessage::text("Choose")
        .return_immediately()
        .result(&addr);
    let task_id = sent["task"]["id"].as_str().unwrap().to_string();
    // The gate appears first (a human COULD answer)…
    wait_task(&addr, &task_id, 10, "gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    // …then the judge takes over and the turn completes.
    let done = wait_task(&addr, &task_id, 20, "auto answer + completion", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(
        done["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Color applied"),
        "{done}"
    );
    let logs = daemon.stderr();
    assert!(logs.contains("human.judge.start"), "{logs}");
    assert!(logs.contains("auto"), "marked auto: {logs}");
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn cancelling_a_gate_unblocks_the_asker_with_an_error() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let extra = "workflows:\n  - name: gated\n    steps:\n      s: {kind: manual}\n      gate: {kind: human, question: \"Proceed?\", depends_on: [s]}\n      f: {kind: finish, depends_on: [gate], output: \"done\"}\n";
    let (_daemon, addr, cfg) = spawn_bound(|port| base_config(&llm.uri, port, true, extra));
    let started = command(&addr, "workflow.run", json!({"workflow": "gated"}));
    let task_id = started["task"]["id"].as_str().unwrap().to_string();
    wait_task(&addr, &task_id, 10, "gate", |t| {
        t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED"
    });
    let cancelled = rpc(&addr, 2, "CancelTask", json!({"id": task_id}));
    assert_eq!(
        cancelled["status"]["state"], "TASK_STATE_CANCELED",
        "{cancelled}"
    );
    // The run resolved (the gate step failed / the run was cancelled) — it is
    // terminal, not stuck.
    let ws = read(&addr, "workflow.status", json!({}));
    let status = ws["runs"][0]["status"]
        .as_str()
        .map(str::to_string)
        .expect("run status");
    assert!(
        status == "cancelled" || status == "failed",
        "the gated run is terminal: {status}"
    );
    std::fs::remove_file(&cfg).ok();
}
