// SPDX-License-Identifier: AGPL-3.0-only
//! MCP Tasks (RFC 0050, SEP-2663), end to end, against the built-in mock's
//! 2026-07-28 edge: `mock.job` becomes a task when the call declares the
//! extension, runs on the mock's clock, and is followed with `tasks/get`,
//! `tasks/update` and `tasks/cancel`, each held to the revision's rules —
//! `Mcp-Name` equal to the `taskId`, the extension declared — and recorded on
//! the mock's side.
//!
//! - A workflow step's task is a durable wait: followed at the server's
//!   `pollIntervalMs` to its result, and polled again after a SIGKILL and a
//!   restart, the call never sent twice. A seed that already ended is
//!   fetched for its result.
//! - How a task ends is how its step ends: failed, a tool error, unknown,
//!   expired; an input request on the supervisor's connection is declined.
//!   A decline the server will not take waits as a failed poll does, and a
//!   task given up while the server may still run it is cancelled.
//! - The step's timeout cancels its task with `tasks/cancel`.
//! - A fan-out past the per-server limit sends the calls past it without
//!   the extension.
//! - `tasks: off` declares nothing, and a server that requires the extension
//!   is refused with an error that names it.
//! - A turn's task asks a person through the elicitation gate, and the
//!   answer reaches the server; one still running past the call is handed
//!   over and read with `mcp.task.get`, and cancelled when its run ends
//!   unread.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TASKS: &str = "io.modelcontextprotocol/tasks";

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

fn mock() -> common::MockMcp {
    common::spawn_mock_mcp_at("2026-07-28", "mock://res", false)
}

/// The mock's `<marker> …` lines, the marker stripped.
fn marked(mock: &common::MockMcp, marker: &str) -> Vec<String> {
    let with = format!("{marker} ");
    mock.log()
        .lines()
        .filter_map(|l| l.strip_prefix(&with).map(str::to_string))
        .collect()
}

/// A config: one server `mock` (with `server_extra` under it), the workflow
/// `job` with `steps` after its `once` start, and `extra` at the top.
fn config(endpoint: &str, server_extra: &str, steps: &str, extra: &str) -> String {
    format!(
        "agent:\n  name: tasks\n{extra}mcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\n{server_extra}\
         workflows:\n  - name: job\n    steps:\n      s: {{kind: once}}\n{steps}\
         lifecycle:\n  run_until: idle\n  idle_grace: 300ms\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

const MEMORY: &str = "store:\n  kind: memory\n";

/// Run a config to its exit; (exit code, log).
fn life(tag: &str, cfg: &str) -> (Option<i32>, String) {
    let path = common::unique_path(tag, "yaml");
    std::fs::write(&path, cfg).unwrap();
    let out = life_at(tag, &path);
    let _ = std::fs::remove_file(&path);
    out
}

/// Run the config at `path` to its exit, within a minute; (exit code, log).
fn life_at(tag: &str, path: &str) -> (Option<i32>, String) {
    let err_path = common::unique_path(tag, "err");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");
    let deadline = Instant::now() + Duration::from_secs(60);
    let code = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    (code, log)
}

/// The `step.done` line of `step`.
fn step_done(log: &str, step: &str) -> Value {
    events(log, "step.done")
        .into_iter()
        .rev()
        .find(|e| e["step"] == step)
        .unwrap_or_else(|| panic!("no step.done for {step}:\n{log}"))
}

/// The mock's tasks of `script`, by `MOCK_TASK_CREATED`.
fn created_as(mock: &common::MockMcp, script: &str) -> Vec<String> {
    marked(mock, "MOCK_TASK_CREATED")
        .iter()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, s)| *s == script)
        .map(|(id, _)| id.to_string())
        .collect()
}

/// How many times the mock was polled for `id`.
fn polls_of(mock: &common::MockMcp, id: &str) -> usize {
    marked(mock, "MOCK_TASK_GET")
        .iter()
        .filter(|l| l.split(' ').next() == Some(id))
        .count()
}

/// Does the `_meta` of a request declare the Tasks extension?
fn declares(meta: &Value) -> bool {
    meta["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
        .get(TASKS)
        .is_some()
}

#[test]
fn a_steps_task_is_followed_at_the_servers_interval_to_its_result() {
    let mock = mock();
    let steps = "      call: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: done, after_ms: 900, poll_ms: 250}}\n\
                 \x20     f: {kind: finish, depends_on: [call], status: completed, output: \"{{ steps.call.output }}\"}\n";
    let (code, log) = life("tasks-done", &config(&mock.uri(), "", steps, MEMORY));
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let done = events(&log, "run.done");
    assert_eq!(done[0]["status"], "completed", "{log}");
    assert_eq!(done[0]["output"], json!({"done": true}), "{log}");
    // The call declared the extension; the rest of the connection did not.
    let metas = mock.metas();
    let call = metas
        .iter()
        .find(|m| m["method"] == "tools/call")
        .expect("the call");
    assert!(declares(call), "{call}");
    assert!(
        metas
            .iter()
            .filter(|m| m["method"] == "tools/list")
            .all(|m| !declares(m))
    );
    assert_eq!(marked(&mock, "MOCK_TASK_CREATED").len(), 1);
    // Every `tasks/get` carried `Mcp-Name` and the extension — the mock
    // refuses one that does not — and came no sooner than the server asked.
    assert_eq!(mock.refused(), 0, "{}", mock.log());
    let at: Vec<u64> = marked(&mock, "MOCK_TASK_GET")
        .iter()
        .map(|l| l.split(' ').nth(1).unwrap().parse().unwrap())
        .collect();
    assert!(at.len() >= 2, "polled until it completed: {at:?}");
    for w in at.windows(2) {
        assert!(
            w[1] - w[0] >= 240,
            "polled faster than pollIntervalMs: {at:?}"
        );
    }
    let created = &events(&log, "mcp.task.created")[0];
    assert_eq!(created["server"], "mock");
    let handle = created["task"].as_str().unwrap();
    assert!(handle.starts_with("mt_"), "{created}");
    assert!(!log.contains("mock-task-0"), "the task id is never logged");
    assert_eq!(events(&log, "mcp.task.completed")[0]["task"], handle);
}

/// A `CreateTaskResult` is the base task only: a job that finished at once
/// says `completed` and nothing of its result, which one `tasks/get` fetches
/// at once — not at the 5 s the server asks for, and never read as a
/// completed call without a result. So for an `mcp.tool` step and for a
/// `tool` step on the server's route alike.
#[test]
fn a_seed_that_already_ended_is_fetched_for_its_result() {
    let mock = mock();
    let steps = "      call: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: done, after_ms: 0, seed: completed, poll_ms: 5000}}\n\
                 \x20     reg: {kind: tool, depends_on: [call], name: mock.job, args: {script: done, after_ms: 0, seed: completed, poll_ms: 5000}}\n\
                 \x20     f: {kind: finish, depends_on: [reg], status: completed, output: \"{{ steps.reg.output }}\"}\n";
    let started = Instant::now();
    let (code, log) = life("tasks-seed", &config(&mock.uri(), "", steps, MEMORY));
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    assert_eq!(events(&log, "run.done")[0]["output"], json!({"done": true}));
    assert_eq!(step_done(&log, "call")["status"], "done", "{log}");
    let created = events(&log, "mcp.task.created");
    assert_eq!(created.len(), 2, "both calls became tasks: {log}");
    assert!(created.iter().all(|c| c["status"] == "completed"), "{log}");
    assert_eq!(marked(&mock, "MOCK_TASK_GET").len(), 2, "{}", mock.log());
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "waited the 5 s pollIntervalMs"
    );
    assert!(marked(&mock, "MOCK_TASK_CANCEL").is_empty());
}

/// A server that takes a listen's `taskIds` pushes the task's end: agentd
/// stops polling it at the server's interval once the listen is
/// acknowledged, and the push alone ends the step.
#[test]
fn a_task_the_server_pushes_ends_its_step_on_the_push() {
    let mock = common::spawn_mock_mcp_with("2026-07-28", "mock://res", false, &["--tasks-push"]);
    let steps = "      call: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: done, after_ms: 2500, poll_ms: 200}}\n\
                 \x20     f: {kind: finish, depends_on: [call], status: completed, output: \"{{ steps.call.output }}\"}\n";
    let (code, log) = life("tasks-push", &config(&mock.uri(), "", steps, MEMORY));
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    assert_eq!(events(&log, "run.done")[0]["output"], json!({"done": true}));
    assert!(
        marked(&mock, "MOCK_LISTEN")
            .iter()
            .any(|f| f.contains(r#""taskIds":["mock-task-0"]"#)),
        "a listen asked for the task: {}",
        mock.log()
    );
    assert_eq!(marked(&mock, "MOCK_TASK_PUSH"), ["mock-task-0"]);
    // Polled at the server's interval only until the push was promised: a
    // dozen polls would have found it otherwise.
    let polls = marked(&mock, "MOCK_TASK_GET").len();
    assert!(polls <= 5, "{polls} polls: {}", mock.log());
}

#[test]
fn how_a_task_ends_is_how_its_step_ends() {
    let mock = mock();
    let steps = "      fail: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: fail, poll_ms: 200}, on_error: continue}\n\
                 \x20     err: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: tool_error, poll_ms: 200}, on_error: continue}\n\
                 \x20     gone: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: vanish, poll_ms: 200}, on_error: continue}\n\
                 \x20     old: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: expire, poll_ms: 200, ttl_ms: 500}, on_error: continue}\n\
                 \x20     ask: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: ask, poll_ms: 200}, on_error: continue}\n\
                 \x20     f: {kind: finish, depends_on: [fail, err, gone, old, ask], status: completed}\n";
    let (_, log) = life("tasks-ends", &config(&mock.uri(), "", steps, MEMORY));
    let fail = step_done(&log, "fail");
    assert_eq!(fail["status"], "failed", "{fail}");
    assert!(
        fail["err"]
            .as_str()
            .unwrap()
            .contains("the job ran out of quota"),
        "{fail}"
    );
    let err = step_done(&log, "err");
    assert_eq!(err["status"], "failed");
    assert_eq!(
        err["err"], "the job failed: no such city",
        "a tool error is the result's text"
    );
    for step in ["gone", "old"] {
        let d = step_done(&log, step);
        assert_eq!(d["status"], "failed", "{d}");
        assert!(
            d["err"].as_str().unwrap().contains("unknown or expired"),
            "{d}"
        );
    }
    assert_eq!(events(&log, "mcp.task.expired").len(), 2, "{log}");
    // The supervisor's connection declares no elicitation: the question is
    // declined, once, and the server's answer to that ends the step.
    let ask = step_done(&log, "ask");
    assert_eq!(ask["err"], "no name was given", "{ask}");
    let updates = marked(&mock, "MOCK_TASK_UPDATE");
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert!(
        updates[0].ends_with(r#"{"name":{"action":"decline"}}"#),
        "{updates:?}"
    );
    let declined = &events(&log, "mcp.task.input.declined")[0];
    assert_eq!(declined["method"], "elicitation/create");
    assert_eq!(mock.refused(), 0, "{}", mock.log());
}

/// A decline the server will not take (`-32603`) is a failed poll: the task
/// is asked again after the pause one gets, never at once. One it answers
/// `-32602` is a task it no longer knows, and ends the step so. A task given
/// up while the server may still run it — a `tasks/get` refused for good —
/// is cancelled; the one it does not know is not.
#[test]
fn a_refused_decline_waits_and_a_task_given_up_is_cancelled() {
    let mock = mock();
    let steps = "      busy: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: ask, poll_ms: 200, update_error: -32603}, timeout: 3s, on_error: continue}\n\
                 \x20     gone: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: ask, poll_ms: 200, update_error: -32602}, on_error: continue}\n\
                 \x20     refused: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: stuck, poll_ms: 200, get_error: -32001}, on_error: continue}\n\
                 \x20     f: {kind: finish, depends_on: [busy, gone, refused], status: completed}\n";
    let (_, log) = life("tasks-refused", &config(&mock.uri(), "", steps, MEMORY));
    let gone = step_done(&log, "gone");
    assert_eq!(gone["status"], "failed", "{gone}");
    assert!(
        gone["err"].as_str().unwrap().contains("unknown or expired"),
        "{gone}"
    );
    let refused = step_done(&log, "refused");
    assert_eq!(refused["status"], "failed", "{refused}");
    assert!(
        refused["err"].as_str().unwrap().contains("tasks/get"),
        "{refused}"
    );
    assert_eq!(step_done(&log, "busy")["status"], "timeout", "{log}");
    // Which `ask` task was which: the one it no longer knew was polled once.
    let asks = created_as(&mock, "ask");
    let (gone_id, busy_id) = match polls_of(&mock, &asks[0]) {
        1 => (&asks[0], &asks[1]),
        _ => (&asks[1], &asks[0]),
    };
    assert_eq!(polls_of(&mock, gone_id), 1, "{}", mock.log());
    let busy_polls = polls_of(&mock, busy_id);
    assert!(
        (1..=6).contains(&busy_polls),
        "{busy_polls} polls in 3 s: {}",
        mock.log()
    );
    let fails: Vec<Value> = events(&log, "mcp.task.poll.fail")
        .into_iter()
        .filter(|e| e["err"].as_str().unwrap_or("").starts_with("tasks/update"))
        .collect();
    assert!(fails.len() >= 2, "{log}");
    assert_eq!(fails[0]["retry_ms"], 1000, "{}", fails[0]);
    assert_eq!(
        fails[1]["retry_ms"], 2000,
        "the pause doubles: {}",
        fails[1]
    );
    let mut cancels = marked(&mock, "MOCK_TASK_CANCEL");
    cancels.sort();
    let mut want = vec![busy_id.clone(), created_as(&mock, "stuck")[0].clone()];
    want.sort();
    assert_eq!(cancels, want, "{}", mock.log());
    assert_eq!(mock.refused(), 0, "{}", mock.log());
}

/// The per-server limit counts the declaring calls still in flight: a
/// fan-out of 40 to one server makes 32 tasks, and the 8 calls past the
/// limit go out without the extension and are answered inline. Once they
/// have ended, a call declares it again.
#[test]
fn a_fan_out_past_the_limit_sends_the_rest_without_the_extension() {
    let mock = mock();
    let mut steps = String::new();
    let mut names = Vec::new();
    for i in 0..40 {
        steps.push_str(&format!(
            "      j{i}: {{kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {{script: done, after_ms: 1500, poll_ms: 300}}}}\n"
        ));
        names.push(format!("j{i}"));
    }
    steps.push_str(&format!(
        "      after: {{kind: mcp.tool, depends_on: [{}], server: mock, tool: mock.job, args: {{script: done, after_ms: 0}}}}\n\
         \x20     f: {{kind: finish, depends_on: [after], status: completed}}\n",
        names.join(", ")
    ));
    let (code, log) = life("tasks-fan", &config(&mock.uri(), "", &steps, MEMORY));
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    assert_eq!(
        marked(&mock, "MOCK_TASK_CREATED").len(),
        33,
        "{}",
        mock.log()
    );
    let calls: Vec<Value> = mock
        .metas()
        .into_iter()
        .filter(|m| m["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 41);
    assert_eq!(calls[..40].iter().filter(|m| !declares(m)).count(), 8);
    assert!(declares(&calls[40]), "the places were given back");
    assert_eq!(events(&log, "mcp.task.completed").len(), 33, "{log}");
}

#[test]
fn the_steps_timeout_cancels_its_task() {
    let mock = mock();
    let steps = "      call: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: stuck, poll_ms: 200}, timeout: 1500ms}\n\
                 \x20     f: {kind: finish, depends_on: [call], status: completed}\n";
    let started = Instant::now();
    let (_, log) = life("tasks-timeout", &config(&mock.uri(), "", steps, MEMORY));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(step_done(&log, "call")["status"], "timeout", "{log}");
    let cancelled = &events(&log, "mcp.task.cancelled")[0];
    assert_eq!(cancelled["reason"], "the step timed out", "{cancelled}");
    let cancels = marked(&mock, "MOCK_TASK_CANCEL");
    assert_eq!(cancels, ["mock-task-0"], "{}", mock.log());
    assert_eq!(
        mock.refused(),
        0,
        "tasks/cancel carried Mcp-Name: {}",
        mock.log()
    );
    assert!(
        !mock.log().contains("notifications/cancelled"),
        "never notifications/cancelled"
    );
}

#[test]
fn a_restart_polls_the_task_again_and_finishes_the_step() {
    let mock = mock();
    let dir = common::unique_path("tasks-restart", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let store = format!(
        "store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n"
    );
    let steps = "      call: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: done, after_ms: 3500, poll_ms: 200}}\n\
                 \x20     f: {kind: finish, depends_on: [call], status: completed, output: \"{{ steps.call.output }}\"}\n";
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, config(&mock.uri(), "", steps, &store)).unwrap();
    // First life: killed once the task is being polled.
    let err_path = common::unique_path("tasks-restart", "err");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while marked(&mock, "MOCK_TASK_GET").is_empty() {
        assert!(Instant::now() < deadline, "never polled:\n{}", mock.log());
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
    let first = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    assert!(
        events(&first, "run.done").is_empty(),
        "killed mid-task:\n{first}"
    );
    // Second life: the record is read back and polled to the end.
    let (code, second) = life_at("tasks-restart", &cfg);
    assert_eq!(code, Some(0), "{second}");
    let resumed = &events(&second, "mcp.task.resumed")[0];
    assert_eq!(resumed["server"], "mock", "{resumed}");
    let done = &events(&second, "run.done")[0];
    assert_eq!(done["status"], "completed", "{second}");
    assert_eq!(done["output"], json!({"done": true}));
    assert_eq!(
        marked(&mock, "MOCK_TASK_CREATED").len(),
        1,
        "the call was not sent again: {}",
        mock.log()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tasks_off_declares_nothing_and_a_server_that_requires_them_is_refused_by_name() {
    let mock = mock();
    let steps = "      need: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {require: true}, on_error: continue}\n\
                 \x20     plain: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: done}}\n\
                 \x20     f: {kind: finish, depends_on: [need, plain], status: completed, output: \"{{ steps.plain.output }}\"}\n";
    let (code, log) = life(
        "tasks-off",
        &config(&mock.uri(), "      tasks: off\n", steps, MEMORY),
    );
    assert_eq!(code, Some(0), "{log}");
    let need = step_done(&log, "need");
    assert_eq!(need["status"], "failed");
    let err = need["err"].as_str().unwrap();
    assert!(err.contains(TASKS) && err.contains("-32021"), "{err}");
    assert_eq!(
        events(&log, "run.done")[0]["output"],
        json!({"inline": true})
    );
    assert!(
        mock.metas()
            .iter()
            .filter(|m| m["method"] == "tools/call")
            .all(|m| !declares(m)),
        "tasks: off declares nothing"
    );
    assert!(marked(&mock, "MOCK_TASK_CREATED").is_empty());
}

/// A playbook file the in-process mock model reads.
fn playbook(tag: &str, v: &Value) -> String {
    let p = common::unique_path(tag, "json");
    std::fs::write(&p, v.to_string()).unwrap();
    p
}

/// The gate's answer comes from the operator's stand-in (`approval: auto`),
/// the model judge — answered here by the playbook — so no listener is
/// needed: what is under test is that the task's question reaches the gate a
/// direct elicitation opens, once, and its answer reaches the server.
#[test]
fn a_turns_task_puts_its_question_to_the_gate_and_the_answer_reaches_the_server() {
    let mock = mock();
    let play = playbook(
        "tasks-ask",
        &json!({"turns": [
            {"tool_calls": [{"name": "mock.job", "arguments": {"script": "ask", "poll_ms": 200}}]},
            {"echo_tool_result": true}
        ], "match": [{"when_contains": "Your name?", "content": "Luca"}]}),
    );
    let extra = format!(
        "  approval: auto\n{MEMORY}intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n"
    );
    let steps = "      a: {kind: agent, depends_on: [s], instruction: \"run the job\"}\n\
                 \x20     f: {kind: finish, depends_on: [a], status: completed, output: \"{{ steps.a.output }}\"}\n";
    let (code, log) = life("tasks-ask", &config(&mock.uri(), "", steps, &extra));
    let _ = std::fs::remove_file(&play);
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    // The worker's connection declares elicitation: the question went to the
    // gate a direct one opens, and its answer to the server.
    let updates = marked(&mock, "MOCK_TASK_UPDATE");
    assert_eq!(updates.len(), 1, "asked once: {updates:?}");
    assert!(
        updates[0].ends_with(r#"{"name":{"action":"accept","content":{"name":"Luca"}}}"#),
        "{updates:?}\n{log}"
    );
    let out = events(&log, "run.done")[0]["output"].to_string();
    assert!(out.contains("Hello, Luca!"), "{out}");
}

#[test]
fn a_turns_task_past_the_call_is_handed_over_and_read_with_mcp_task_get() {
    let mock = mock();
    let play = playbook(
        "tasks-hand",
        &json!({"turns": [
            {"tool_calls": [{"name": "mock.job", "arguments": {"script": "done", "after_ms": 4500, "poll_ms": 200}}]},
            {"echo_tool_result": true}
        ]}),
    );
    let extra =
        format!("{MEMORY}intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n");
    // The mock's first task, on the server `mock`.
    let h = agentd::runtime::mcp_tasks::handle("mock", "mock-task-0");
    let steps = format!(
        "      a: {{kind: agent, depends_on: [s], instruction: \"start the job\", timeout: 4s}}\n\
         \x20     nap: {{kind: sleep, depends_on: [a], duration: 4s}}\n\
         \x20     read: {{kind: tool, depends_on: [nap], name: mcp.task.get, args: {{task: \"{h}\"}}}}\n\
         \x20     again: {{kind: tool, depends_on: [read], name: mcp.task.get, args: {{task: \"{h}\"}}, on_error: continue}}\n\
         \x20     f: {{kind: finish, depends_on: [again], status: completed, output: \"{{{{ steps.read.output }}}}\"}}\n"
    );
    let (code, log) = life("tasks-hand", &config(&mock.uri(), "", &steps, &extra));
    let _ = std::fs::remove_file(&play);
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let handed = &events(&log, "mcp.task.handed_over")[0];
    assert_eq!(handed["task"], h.as_str(), "{handed}");
    // The model was told the handle, not the server's id.
    let a = events(&log, "run.done")[0].clone();
    let read = &a["output"];
    assert_eq!(read["task"], h.as_str(), "{a}");
    assert_eq!(read["status"], "completed", "{a}");
    assert_eq!(read["tool_result"], json!({"done": true}), "{a}");
    // Handed over once: a second read finds nothing.
    let again = step_done(&log, "again");
    assert_eq!(again["status"], "failed", "{again}");
    assert!(
        again["err"].as_str().unwrap().contains("already read"),
        "{again}"
    );
}

/// A task a workflow's `agent` step handed over is its run's: when the run
/// ends without reading it, the task is cancelled, not polled for ever.
#[test]
fn a_handed_over_task_is_cancelled_when_its_run_ends() {
    let mock = mock();
    let play = playbook(
        "tasks-orphan",
        &json!({"turns": [
            {"tool_calls": [{"name": "mock.job", "arguments": {"script": "stuck", "poll_ms": 200, "ttl_ms": null}}]},
            {"echo_tool_result": true}
        ]}),
    );
    let extra =
        format!("{MEMORY}intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n");
    let steps = "      a: {kind: agent, depends_on: [s], instruction: \"start the job\", timeout: 3s}\n\
                 \x20     f: {kind: finish, depends_on: [a], status: completed}\n";
    let (code, log) = life("tasks-orphan", &config(&mock.uri(), "", steps, &extra));
    let _ = std::fs::remove_file(&play);
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let h = agentd::runtime::mcp_tasks::handle("mock", "mock-task-0");
    assert_eq!(events(&log, "mcp.task.handed_over")[0]["task"], h.as_str());
    let cancelled = &events(&log, "mcp.task.cancelled")[0];
    assert_eq!(cancelled["reason"], "the run ended", "{cancelled}");
    assert_eq!(cancelled["task"], h.as_str());
    assert_eq!(
        marked(&mock, "MOCK_TASK_CANCEL"),
        ["mock-task-0"],
        "{}",
        mock.log()
    );
}
