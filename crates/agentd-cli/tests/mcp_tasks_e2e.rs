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
//! - A step that detaches (RFC 0054) finishes with its task's handle while
//!   the task runs, and a `wait {on: mcp_task}` collects how it ended —
//!   completed, cancelled with `mcp.task.cancel`, expired — however long
//!   after; the wait's timeout leaves the task to the run, whose end cancels
//!   one still open. A `foreach` detaches and collects many; a server that
//!   answers at once answers the step; a restart polls a detached task again
//!   and resolves the wait on it. `detach` on a tool no MCP server serves
//!   refuses the start.
//! - A step with `input` puts its task's question to a person through the
//!   gate a `human` node opens — the operator's stand-in in a build without
//!   a listener, a person over A2A with one — and the answer reaches the
//!   server; without `input` the question is declined. Over A2A each
//!   question opens on a task of its own, so two questions and a `human`
//!   step's gate are open at once; a run that ends closes its question and
//!   keeps its own task completed, and a restart ends a question's task and
//!   asks again on a new one. A step's own task's question ends with the
//!   step's timeout.
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

/// The mock tagged `trusted`, for a test whose gate the operator's stand-in
/// answers: an untagged server's question is outside text, which the judge
/// never answers for the operator.
const TRUSTED: &str = "      tags: {\"*\": [trusted]}\n";

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
    let (code, log) = life("tasks-ask", &config(&mock.uri(), TRUSTED, steps, &extra));
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

/// The position of the first log line that is the event `name` for which
/// `pred` holds.
fn line_of(log: &str, name: &str, pred: impl Fn(&Value) -> bool) -> usize {
    log.lines()
        .position(|l| {
            serde_json::from_str::<Value>(l).is_ok_and(|v| v["event"] == name && pred(&v))
        })
        .unwrap_or_else(|| panic!("no {name} line:\n{log}"))
}

/// A step that detaches finishes once its call becomes a task, with the
/// task's handle, while the task runs; the run goes on, and a wait collects
/// the task once it completed. So for an `mcp.tool` step and a `tool` step
/// on the server's route alike.
#[test]
fn a_detached_task_is_collected_by_a_wait_once_it_completes() {
    let mock = mock();
    let steps = "      build: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, detach: true, args: {script: done, after_ms: 1500, poll_ms: 200}}\n\
                 \x20     reg: {kind: tool, depends_on: [s], name: mock.job, detach: true, args: {script: done, after_ms: 1500, poll_ms: 200}}\n\
                 \x20     meanwhile: {kind: assign, depends_on: [build, reg], value: \"{{ steps.build.output.task.status }}\"}\n\
                 \x20     wb: {kind: wait, depends_on: [meanwhile], on: mcp_task, task: \"{{ steps.build.output.task.id }}\", timeout: 30s}\n\
                 \x20     wr: {kind: wait, depends_on: [meanwhile], on: mcp_task, task: \"{{ steps.reg.output.task.id }}\", timeout: 30s}\n\
                 \x20     f: {kind: finish, depends_on: [wb, wr], status: completed, output: {build: \"{{ steps.wb.output }}\", reg: \"{{ steps.wr.output }}\", meanwhile: \"{{ steps.meanwhile.output }}\"}}\n";
    let (code, log) = life("tasks-detach", &config(&mock.uri(), "", steps, MEMORY));
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let out = &events(&log, "run.done")[0]["output"];
    assert_eq!(out["meanwhile"], "working", "{out}");
    let detached = events(&log, "mcp.task.detached");
    assert_eq!(detached.len(), 2, "{log}");
    for step in ["build", "reg"] {
        let got = &out[step];
        assert_eq!(got["status"], "completed", "{got}");
        assert_eq!(got["ok"], true, "{got}");
        assert_eq!(got["result"], json!({"done": true}), "{got}");
        assert_eq!(got["server"], "mock");
        assert_eq!(got["tool"], "mock.job");
        let h = got["task"].as_str().unwrap();
        assert!(h.starts_with("mt_"), "{got}");
        assert!(
            detached.iter().any(|d| d["task"] == h && d["step"] == step),
            "{log}"
        );
        // The step was done while its task still ran.
        let done = line_of(&log, "step.done", |e| e["step"] == step);
        let completed = line_of(&log, "mcp.task.completed", |e| e["task"] == h);
        assert!(done < completed, "{step} waited for its task:\n{log}");
    }
    assert_eq!(marked(&mock, "MOCK_TASK_CREATED").len(), 2);
    assert!(
        marked(&mock, "MOCK_TASK_CANCEL").is_empty(),
        "{}",
        mock.log()
    );
    assert!(!log.contains("mock-task-"), "the task id is never logged");
}

/// `mcp.task.cancel` from a step reaches the run's detached task: it sends
/// `tasks/cancel` and follows the task on, so the wait resolves with what
/// the server made of it, and `mcp.task.get` still reads it after.
#[test]
fn a_detached_task_cancelled_from_a_step_resolves_its_wait_cancelled() {
    let mock = mock();
    let steps = "      build: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, detach: true, args: {script: stuck, poll_ms: 200}}\n\
                 \x20     stop: {kind: tool, depends_on: [build], name: mcp.task.cancel, args: {task: \"{{ steps.build.output.task.id }}\"}}\n\
                 \x20     w: {kind: wait, depends_on: [stop], on: mcp_task, task: \"{{ steps.build.output.task.id }}\", timeout: 30s}\n\
                 \x20     read: {kind: tool, depends_on: [w], name: mcp.task.get, args: {task: \"{{ steps.build.output.task.id }}\"}}\n\
                 \x20     f: {kind: finish, depends_on: [read], status: completed, output: {w: \"{{ steps.w.output }}\", stop: \"{{ steps.stop.output }}\", read: \"{{ steps.read.output }}\"}}\n";
    let (code, log) = life(
        "tasks-detach-cancel",
        &config(&mock.uri(), "", steps, MEMORY),
    );
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let out = &events(&log, "run.done")[0]["output"];
    assert_eq!(out["stop"]["cancelled"], true, "{out}");
    assert_eq!(out["w"]["status"], "cancelled", "{out}");
    assert_eq!(out["w"]["ok"], false, "{out}");
    assert!(
        out["w"]["error"].as_str().unwrap().contains("cancelled"),
        "{out}"
    );
    assert_eq!(
        out["read"]["status"], "cancelled",
        "kept for the run: {out}"
    );
    assert_eq!(
        marked(&mock, "MOCK_TASK_CANCEL"),
        ["mock-task-0"],
        "{}",
        mock.log()
    );
    let cancelled = &events(&log, "mcp.task.cancelled")[0];
    assert_eq!(cancelled["reason"], "mcp.task.cancel", "{cancelled}");
}

/// The wait's timeout ends the wait, not the task: the task stays the run's,
/// and a later wait collects it.
#[test]
fn a_waits_timeout_leaves_the_detached_task_to_the_run() {
    let mock = mock();
    let steps = "      build: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, detach: true, args: {script: done, after_ms: 2500, poll_ms: 200}}\n\
                 \x20     first: {kind: wait, depends_on: [build], on: mcp_task, task: \"{{ steps.build.output.task.id }}\", timeout: 1s, on_timeout: again}\n\
                 \x20     again: {kind: wait, on: mcp_task, task: \"{{ steps.build.output.task.id }}\", timeout: 30s}\n\
                 \x20     f: {kind: finish, depends_on: [again], status: completed, output: \"{{ steps.again.output }}\"}\n";
    let (code, log) = life(
        "tasks-detach-slices",
        &config(&mock.uri(), "", steps, MEMORY),
    );
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    assert_eq!(step_done(&log, "first")["status"], "timeout", "{log}");
    let out = &events(&log, "run.done")[0]["output"];
    assert_eq!(out["status"], "completed", "{out}");
    assert_eq!(out["result"], json!({"done": true}), "{out}");
    assert!(
        marked(&mock, "MOCK_TASK_CANCEL").is_empty(),
        "{}",
        mock.log()
    );
    assert_eq!(marked(&mock, "MOCK_TASK_CREATED").len(), 1);
}

/// A run that ends with its detached task still open cancels it.
#[test]
fn a_detached_task_still_open_is_cancelled_when_its_run_ends() {
    let mock = mock();
    let steps = "      build: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, detach: true, args: {script: stuck, poll_ms: 200, ttl_ms: null}}\n\
                 \x20     f: {kind: finish, depends_on: [build], status: completed, output: \"{{ steps.build.output.task.id }}\"}\n";
    let (code, log) = life(
        "tasks-detach-orphan",
        &config(&mock.uri(), "", steps, MEMORY),
    );
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let h = events(&log, "run.done")[0]["output"]
        .as_str()
        .unwrap()
        .to_string();
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

/// A detached task agentd stops following — here one the server purged at
/// its TTL — resolves its wait `failed`, saying why; a wait on a handle no
/// step of the run detached fails its step, naming it.
#[test]
fn a_detached_task_that_expires_resolves_its_wait_failed() {
    let mock = mock();
    let steps = "      build: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, detach: true, args: {script: expire, poll_ms: 200, ttl_ms: 500}}\n\
                 \x20     w: {kind: wait, depends_on: [build], on: mcp_task, task: \"{{ steps.build.output.task.id }}\", timeout: 30s}\n\
                 \x20     stray: {kind: wait, depends_on: [s], on: mcp_task, task: mt_0000000000000000, timeout: 30s, on_error: continue}\n\
                 \x20     f: {kind: finish, depends_on: [w, stray], status: completed, output: \"{{ steps.w.output }}\"}\n";
    let (code, log) = life(
        "tasks-detach-expire",
        &config(&mock.uri(), "", steps, MEMORY),
    );
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let out = &events(&log, "run.done")[0]["output"];
    assert_eq!(out["status"], "failed", "{out}");
    assert_eq!(out["ok"], false);
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("unknown or expired"),
        "{out}"
    );
    let stray = step_done(&log, "stray");
    assert_eq!(stray["status"], "failed", "{stray}");
    assert!(
        stray["err"]
            .as_str()
            .unwrap()
            .contains("no task \"mt_0000000000000000\" was detached in this run"),
        "{stray}"
    );
}

/// A `foreach` element that detaches holds its lane only for the call: the
/// fan-out finishes while every task runs, and a second `foreach` collects
/// them all.
#[test]
fn a_foreach_detaches_many_tasks_and_another_collects_them() {
    let mock = mock();
    let steps = "      fan: {kind: foreach, depends_on: [s], over: [1, 2, 3], body: {steps: {b: {kind: mcp.tool, server: mock, tool: mock.job, detach: true, args: {script: done, after_ms: 1200, poll_ms: 200}}}}}\n\
                 \x20     collect: {kind: foreach, depends_on: [fan], over: \"{{ steps.fan.output }}\", body: {steps: {w: {kind: wait, on: mcp_task, task: \"{{ item.task.id }}\", timeout: 30s}}}}\n\
                 \x20     f: {kind: finish, depends_on: [collect], status: completed, output: \"{{ steps.collect.output }}\"}\n";
    let (code, log) = life("tasks-detach-fan", &config(&mock.uri(), "", steps, MEMORY));
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let out = events(&log, "run.done")[0]["output"].clone();
    let all = out.as_array().unwrap_or_else(|| panic!("{out}"));
    assert_eq!(all.len(), 3, "{out}");
    assert!(
        all.iter()
            .all(|w| w["status"] == "completed" && w["result"] == json!({"done": true})),
        "{out}"
    );
    assert_eq!(marked(&mock, "MOCK_TASK_CREATED").len(), 3);
    let fanned = line_of(&log, "step.done", |e| e["step"] == "fan");
    let first = line_of(&log, "mcp.task.completed", |_| true);
    assert!(fanned < first, "the fan-out waited for its tasks:\n{log}");
}

/// A server that answers at once — here one agentd declares no tasks to —
/// answers the detaching step as it would any step.
#[test]
fn a_detaching_step_the_server_answers_at_once_outputs_the_answer() {
    let mock = mock();
    let steps = "      build: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, detach: true, args: {script: done}}\n\
                 \x20     f: {kind: finish, depends_on: [build], status: completed, output: \"{{ steps.build.output }}\"}\n";
    let (code, log) = life(
        "tasks-detach-inline",
        &config(&mock.uri(), "      tasks: off\n", steps, MEMORY),
    );
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(
        events(&log, "run.done")[0]["output"],
        json!({"inline": true}),
        "{log}"
    );
    assert!(events(&log, "mcp.task.detached").is_empty(), "{log}");
}

/// A restart reads a detached task back, polls it again and resolves the
/// wait on it, the call never sent again.
#[test]
fn a_restart_while_waiting_on_a_detached_task_resolves_the_wait() {
    let mock = mock();
    let dir = common::unique_path("tasks-detach-restart", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let store = format!(
        "store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n"
    );
    let steps = "      build: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, detach: true, args: {script: done, after_ms: 3500, poll_ms: 200}}\n\
                 \x20     w: {kind: wait, depends_on: [build], on: mcp_task, task: \"{{ steps.build.output.task.id }}\", timeout: 1m}\n\
                 \x20     f: {kind: finish, depends_on: [w], status: completed, output: \"{{ steps.w.output }}\"}\n";
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, config(&mock.uri(), "", steps, &store)).unwrap();
    let err_path = common::unique_path("tasks-detach-restart", "err");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .unwrap();
    // Killed once the wait is armed and the task polled.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let first = std::fs::read_to_string(&err_path).unwrap_or_default();
        if !marked(&mock, "MOCK_TASK_GET").is_empty()
            && events(&first, "step.done")
                .iter()
                .any(|e| e["step"] == "build")
        {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("never waited:\n{first}\n{}", mock.log());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // A tick for the wait to arm and be kept.
    std::thread::sleep(Duration::from_millis(400));
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
    let first = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    assert!(
        events(&first, "run.done").is_empty(),
        "killed mid-wait:\n{first}"
    );
    let (code, second) = life_at("tasks-detach-restart", &cfg);
    assert_eq!(code, Some(0), "{second}");
    assert_eq!(events(&second, "mcp.task.resumed").len(), 1, "{second}");
    let done = &events(&second, "run.done")[0];
    assert_eq!(done["status"], "completed", "{second}");
    assert_eq!(done["output"]["status"], "completed", "{done}");
    assert_eq!(done["output"]["result"], json!({"done": true}), "{done}");
    assert_eq!(
        marked(&mock, "MOCK_TASK_CREATED").len(),
        1,
        "the call was not sent again: {}",
        mock.log()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A step with `input` puts its task's question to the gate a `human` node
/// opens; here the operator's stand-in answers it (`approval: auto`, the
/// playbook's judge), so no listener is needed. The answer reaches the
/// server as the elicitation's `accept`, for a step's own task and for one
/// it detached.
#[test]
fn a_tasks_question_goes_to_the_gate_and_its_answer_reaches_the_server() {
    let mock = mock();
    let play = playbook(
        "tasks-input",
        &json!({"turns": [{"content": "unused"}],
                "match": [{"when_contains": "Your name?", "content": "Luca"}]}),
    );
    let extra = format!(
        "  approval: auto\n{MEMORY}intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n"
    );
    let steps = "      own: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, input: {timeout: 5m}, args: {script: ask, poll_ms: 200}}\n\
                 \x20     bg: {kind: tool, depends_on: [s], name: mock.job, detach: true, input: {}, args: {script: ask, poll_ms: 200}}\n\
                 \x20     w: {kind: wait, depends_on: [bg], on: mcp_task, task: \"{{ steps.bg.output.task.id }}\", timeout: 30s}\n\
                 \x20     f: {kind: finish, depends_on: [own, w], status: completed, output: {own: \"{{ steps.own.output }}\", bg: \"{{ steps.w.output }}\"}}\n";
    let (code, log) = life("tasks-input", &config(&mock.uri(), TRUSTED, steps, &extra));
    let _ = std::fs::remove_file(&play);
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let out = &events(&log, "run.done")[0]["output"];
    assert_eq!(out["own"], "Hello, Luca!", "{out}");
    assert_eq!(out["bg"]["result"], "Hello, Luca!", "{out}");
    assert_eq!(out["bg"]["ok"], true, "{out}");
    let updates = marked(&mock, "MOCK_TASK_UPDATE");
    assert_eq!(updates.len(), 2, "asked once each: {updates:?}");
    assert!(
        updates
            .iter()
            .all(|u| u.ends_with(r#"{"name":{"action":"accept","content":{"name":"Luca"}}}"#)),
        "{updates:?}"
    );
    assert_eq!(events(&log, "mcp.task.input.asked").len(), 2, "{log}");
    let answered = events(&log, "mcp.task.input.answered");
    assert_eq!(answered.len(), 2, "{log}");
    assert!(
        answered
            .iter()
            .all(|a| a["action"] == "accept" && a["key"] == "name"),
        "{log}"
    );
    assert!(events(&log, "mcp.task.input.declined").is_empty(), "{log}");
    // The question the gate put names the server and the tool.
    let judged = events(&log, "human.judge.start");
    assert_eq!(judged.len(), 2, "{log}");
    assert!(
        judged.iter().all(|j| j["question"]
            .as_str()
            .unwrap()
            .contains("The MCP server \"mock\" asks, for its tool \"mock.job\"")),
        "{log}"
    );
}

/// Without `input`, a detached task's question is declined, as a step's own
/// is, and the wait reads the answer the server gave to that.
#[test]
fn a_detached_tasks_question_without_input_is_declined() {
    let mock = mock();
    let steps = "      bg: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, detach: true, args: {script: ask, poll_ms: 200}}\n\
                 \x20     w: {kind: wait, depends_on: [bg], on: mcp_task, task: \"{{ steps.bg.output.task.id }}\", timeout: 30s}\n\
                 \x20     f: {kind: finish, depends_on: [w], status: completed, output: \"{{ steps.w.output }}\"}\n";
    let (code, log) = life(
        "tasks-detach-declined",
        &config(&mock.uri(), "", steps, MEMORY),
    );
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let out = &events(&log, "run.done")[0]["output"];
    assert_eq!(out["status"], "completed", "{out}");
    assert_eq!(out["ok"], false, "{out}");
    assert_eq!(out["error"], "no name was given", "{out}");
    let updates = marked(&mock, "MOCK_TASK_UPDATE");
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert!(
        updates[0].ends_with(r#"{"name":{"action":"decline"}}"#),
        "{updates:?}"
    );
    assert!(events(&log, "mcp.task.input.asked").is_empty(), "{log}");
}

/// A step that waits on its own task ends it at the step's `timeout`, and
/// the question with it, so the gate's deadline is the step's when that
/// comes first, never the question's 24 hours. Here nobody can be asked, and
/// the fallback parks the gate, which says its deadline.
#[test]
fn a_steps_own_tasks_question_ends_with_the_steps_timeout() {
    let mock = mock();
    let steps = "      own: {kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, timeout: 2s, on_error: continue, input: {}, args: {script: ask, poll_ms: 200}}\n\
                 \x20     f: {kind: finish, depends_on: [own], status: completed}\n";
    let extra = format!("  ask_human_fallback: wait\n{MEMORY}");
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let (code, log) = life("tasks-input-bound", &config(&mock.uri(), "", steps, &extra));
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    let parked = events(&log, "human.ask.parked");
    assert_eq!(parked.len(), 1, "{log}");
    let deadline = parked[0]["deadline_ms"].as_u64().unwrap();
    assert!(
        deadline <= started + 30_000,
        "the gate outlives its step: {deadline} - {started}\n{log}"
    );
    assert_eq!(step_done(&log, "own")["status"], "timeout", "{log}");
    assert_eq!(marked(&mock, "MOCK_TASK_CANCEL").len(), 1, "{}", mock.log());
    assert!(
        marked(&mock, "MOCK_TASK_UPDATE").is_empty(),
        "{}",
        mock.log()
    );
}

/// A daemon a test talks to, stopped and its files removed however the test
/// ends, a failed assertion included.
#[cfg(feature = "a2a")]
struct Daemon {
    child: Child,
    cfg_path: String,
    err_path: String,
}

#[cfg(feature = "a2a")]
impl Daemon {
    /// Stop it, and return its log.
    fn stop(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }
}

#[cfg(feature = "a2a")]
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.cfg_path);
        let _ = std::fs::remove_file(&self.err_path);
    }
}

/// A config listening for A2A on `port`: one server `mock`, the workflow
/// `ask` with `steps` after its manual start `go`, and `store`.
#[cfg(feature = "a2a")]
fn a2a_config(port: u16, endpoint: &str, steps: &str, store: &str) -> String {
    format!(
        "agent:\n  name: tasks-a2a\n  preflight: never\n{store}\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         mcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\n\
         workflows:\n  - name: ask\n    steps:\n      go: {{kind: manual}}\n{steps}\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

/// The daemon of `cfg` once its listener bound, with the address it bound;
/// `None`, the daemon stopped, when the bind lost a race for its port.
#[cfg(feature = "a2a")]
fn bound(tag: &str, cfg: impl Fn(u16) -> String) -> Option<(Daemon, String)> {
    let cfg_path = common::unique_path(tag, "yaml");
    std::fs::write(&cfg_path, cfg(common::free_port())).unwrap();
    let err_path = common::unique_path(tag, "err");
    let errf = std::fs::File::create(&err_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");
    let daemon = Daemon {
        child,
        cfg_path,
        err_path,
    };
    let addr = common::try_a2a_bound(&daemon.err_path, Duration::from_secs(15))?;
    Some((daemon, addr))
}

/// A daemon of [`a2a_config`], on a port it could bind.
#[cfg(feature = "a2a")]
fn listening(tag: &str, endpoint: &str, steps: &str, store: &str) -> (Daemon, String) {
    (0..5)
        .find_map(|_| bound(tag, |port| a2a_config(port, endpoint, steps, store)))
        .expect("the daemon never bound")
}

/// GetTask `id` until `pred` holds, within 20 s.
#[cfg(feature = "a2a")]
fn task_until(addr: &str, id: &str, what: &str, pred: &dyn Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let t = common::rpc_result(addr, 900, "GetTask", json!({"id": id}));
        if pred(&t) {
            return t;
        }
        assert!(Instant::now() < deadline, "timeout: {what}; last: {t}");
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// The tasks of conversation `ctx` that ask a question, once `n` do at
/// once, within 20 s.
#[cfg(feature = "a2a")]
fn asking(addr: &str, ctx: &str, n: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let v = common::rpc_result(
            addr,
            901,
            "ListTasks",
            json!({"contextId": ctx, "status": "TASK_STATE_INPUT_REQUIRED"}),
        );
        // An empty list is omitted from the wire, not sent as `[]`.
        let open: Vec<Value> = v["tasks"].as_array().cloned().unwrap_or_default();
        if open.len() >= n {
            return open;
        }
        assert!(
            Instant::now() < deadline,
            "never {n} questions open at once in {ctx}; last: {v}"
        );
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// A task's status message.
#[cfg(feature = "a2a")]
fn said(t: &Value) -> String {
    t["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// Start the workflow `ask` over A2A: its run's task and conversation. The
/// send returns at once: a question on a task of its own does not interrupt
/// the run's.
#[cfg(feature = "a2a")]
fn start_ask(addr: &str) -> (String, String) {
    let started = common::SendMessage::command("workflow.run", json!({"workflow": "ask"}))
        .return_immediately()
        .result(addr);
    let task = &started["task"];
    (
        task["id"].as_str().unwrap().to_string(),
        task["contextId"].as_str().unwrap().to_string(),
    )
}

/// Answer the gate on `task` with `text`.
#[cfg(feature = "a2a")]
fn answer(addr: &str, task: &str, text: &str) {
    common::SendMessage::text(text)
        .task(task)
        .return_immediately()
        .result(addr);
}

/// Over the A2A listener, a step task's question opens on a task of its own
/// in the run's conversation, as a `human` gate: the person's reply answers
/// it, and the answer reaches the server. The run's task never asks.
#[cfg(feature = "a2a")]
#[test]
fn a_person_answers_a_tasks_question_over_a2a() {
    let mock = mock();
    let steps = "      own: {kind: mcp.tool, depends_on: [go], server: mock, tool: mock.job, input: {to: operator}, args: {script: ask, poll_ms: 200}}\n\
                 \x20     f: {kind: finish, depends_on: [own], status: completed, output: \"{{ steps.own.output }}\"}\n";
    let (mut daemon, addr) = listening("tasks-a2a", &mock.uri(), steps, MEMORY);
    let (run_task, ctx) = start_ask(&addr);
    let open = asking(&addr, &ctx, 1);
    assert_eq!(open.len(), 1, "{open:?}");
    let question = open[0]["id"].as_str().unwrap().to_string();
    assert_ne!(question, run_task, "the question took the run's task");
    let asked = said(&open[0]);
    assert!(
        asked.contains("Your name?") && asked.contains("\"mock\""),
        "{asked}"
    );
    answer(&addr, &question, "Luca");
    let settled = task_until(&addr, &question, "the question settled", &|t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert_eq!(said(&settled), "answered", "{settled}");
    let done = task_until(&addr, &run_task, "the run", &|t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(
        done["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Hello, Luca!"),
        "{done}"
    );
    let updates = marked(&mock, "MOCK_TASK_UPDATE");
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert!(
        updates[0].ends_with(r#"{"name":{"action":"accept","content":{"name":"Luca"}}}"#),
        "{updates:?}"
    );
    let log = daemon.stop();
    assert!(
        events(&log, "mcp.task.input.answered")
            .iter()
            .any(|a| a["action"] == "accept"),
        "{log}"
    );
    assert!(
        events(&log, "human.ask")
            .iter()
            .all(|a| a["task"] != run_task.as_str()),
        "{log}"
    );
}

/// A task holds one gate, and a question comes when the server asks it: two
/// detached tasks' questions and a `human` step that opens after them are
/// open at once, each question on a task of its own and the `human` step on
/// the run's, and every answer reaches whoever asked.
#[cfg(feature = "a2a")]
#[test]
fn two_tasks_questions_and_a_human_gate_are_open_at_once_over_a2a() {
    let mock = mock();
    let steps = "      a: {kind: mcp.tool, depends_on: [go], server: mock, tool: mock.job, detach: true, input: {}, args: {script: ask, poll_ms: 200}}\n\
                 \x20     b: {kind: tool, depends_on: [go], name: mock.job, detach: true, input: {}, args: {script: ask, poll_ms: 200}}\n\
                 \x20     pause: {kind: wait, depends_on: [a, b], on: deadline, timeout: 1s}\n\
                 \x20     approve: {kind: human, depends_on: [pause], question: \"Ship it?\"}\n\
                 \x20     wa: {kind: wait, depends_on: [a], on: mcp_task, task: \"{{ steps.a.output.task.id }}\", timeout: 1m}\n\
                 \x20     wb: {kind: wait, depends_on: [b], on: mcp_task, task: \"{{ steps.b.output.task.id }}\", timeout: 1m}\n\
                 \x20     f: {kind: finish, depends_on: [approve, wa, wb], status: completed, output: {approve: \"{{ steps.approve.output }}\", a: \"{{ steps.wa.output.result }}\", b: \"{{ steps.wb.output.result }}\"}}\n";
    let (mut daemon, addr) = listening("tasks-a2a-three", &mock.uri(), steps, MEMORY);
    let (run_task, ctx) = start_ask(&addr);
    let open = asking(&addr, &ctx, 3);
    assert_eq!(open.len(), 3, "{open:?}");
    let mut names = ["Ada", "Bo"].into_iter();
    for t in &open {
        let id = t["id"].as_str().unwrap();
        if id == run_task {
            assert_eq!(said(t), "Ship it?", "{t}");
            answer(&addr, id, "yes");
        } else {
            assert!(said(t).contains("Your name?"), "{t}");
            answer(&addr, id, names.next().expect("two questions"));
        }
    }
    let done = task_until(&addr, &run_task, "the run", &|t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    let log = daemon.stop();
    let out = &events(&log, "run.done")[0]["output"];
    assert_eq!(out["approve"], "yes", "{done}\n{log}");
    let mut hellos = [
        out["a"].as_str().unwrap_or(""),
        out["b"].as_str().unwrap_or(""),
    ];
    hellos.sort_unstable();
    assert_eq!(hellos, ["Hello, Ada!", "Hello, Bo!"], "{out}");
    assert!(!log.contains("already pending"), "{log}");
    let updates = marked(&mock, "MOCK_TASK_UPDATE");
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(
        updates.iter().all(|u| u.contains(r#""action":"accept""#)),
        "{updates:?}"
    );
}

/// A run that ends while a detached task's question is open cancels the
/// task, and the question's task closes; the run's task stays completed.
#[cfg(feature = "a2a")]
#[test]
fn a_run_that_ends_with_a_question_open_stays_completed() {
    let mock = mock();
    let steps = "      build: {kind: mcp.tool, depends_on: [go], server: mock, tool: mock.job, detach: true, input: {}, args: {script: ask, poll_ms: 200}}\n\
                 \x20     pause: {kind: wait, depends_on: [build], on: deadline, timeout: 1s}\n\
                 \x20     f: {kind: finish, depends_on: [pause], status: completed, output: done}\n";
    let (mut daemon, addr) = listening("tasks-a2a-ends", &mock.uri(), steps, MEMORY);
    let (run_task, ctx) = start_ask(&addr);
    let question = asking(&addr, &ctx, 1)[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    task_until(&addr, &run_task, "the run", &|t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    let closed = task_until(&addr, &question, "the question closed", &|t| {
        t["status"]["state"] != "TASK_STATE_INPUT_REQUIRED"
    });
    assert_eq!(
        closed["status"]["state"], "TASK_STATE_COMPLETED",
        "{closed}"
    );
    assert_eq!(said(&closed), "the MCP task no longer asks", "{closed}");
    // A few ticks on, the run's task is still as its run left it.
    std::thread::sleep(Duration::from_millis(600));
    let after = common::rpc_result(&addr, 902, "GetTask", json!({"id": run_task}));
    assert_eq!(after["status"]["state"], "TASK_STATE_COMPLETED", "{after}");
    let log = daemon.stop();
    assert!(
        events(&log, "mcp.task.cancelled")
            .iter()
            .any(|e| e["reason"] == "the run ended"),
        "{log}"
    );
    assert_eq!(marked(&mock, "MOCK_TASK_CANCEL").len(), 1, "{}", mock.log());
    assert!(
        marked(&mock, "MOCK_TASK_UPDATE").is_empty(),
        "{}",
        mock.log()
    );
}

/// An answer is never kept, so a restart ends the task a question was open
/// on, and the question is asked again on a new one, whose answer reaches
/// the server: the call made once, one update.
#[cfg(feature = "a2a")]
#[test]
fn a_restart_ends_a_questions_task_and_asks_again_on_a_new_one() {
    let mock = mock();
    let dir = common::unique_path("tasks-a2a-restart", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let store = format!(
        "store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n"
    );
    let steps = "      build: {kind: mcp.tool, depends_on: [go], server: mock, tool: mock.job, detach: true, input: {}, args: {script: ask, poll_ms: 200}}\n\
                 \x20     w: {kind: wait, depends_on: [build], on: mcp_task, task: \"{{ steps.build.output.task.id }}\", timeout: 1m}\n\
                 \x20     f: {kind: finish, depends_on: [w], status: completed, output: \"{{ steps.w.output.result }}\"}\n";
    let (mut first, addr) = listening("tasks-a2a-restart", &mock.uri(), steps, &store);
    let (run_task, ctx) = start_ask(&addr);
    let before = asking(&addr, &ctx, 1)[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A tick for the wait to arm and be kept.
    std::thread::sleep(Duration::from_millis(400));
    unsafe { libc::kill(first.child.id() as i32, libc::SIGKILL) };
    let first_log = first.stop();
    assert!(events(&first_log, "run.done").is_empty(), "{first_log}");
    let (mut second, addr) = listening("tasks-a2a-restart", &mock.uri(), steps, &store);
    let ended = task_until(&addr, &before, "the old question ended", &|t| {
        t["status"]["state"] == "TASK_STATE_FAILED"
    });
    assert!(said(&ended).contains("agentd restarted"), "{ended}");
    let open = asking(&addr, &ctx, 1);
    assert_eq!(open.len(), 1, "{open:?}");
    let again = open[0]["id"].as_str().unwrap().to_string();
    assert_ne!(again, before);
    assert_ne!(again, run_task);
    answer(&addr, &again, "Luca");
    let done = task_until(&addr, &run_task, "the run", &|t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(
        done["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Hello, Luca!"),
        "{done}"
    );
    let log = second.stop();
    assert!(
        events(&log, "mcp.task.input.interrupted")
            .iter()
            .any(|e| e["gate"] == before.as_str()),
        "{log}"
    );
    assert_eq!(
        marked(&mock, "MOCK_TASK_CREATED").len(),
        1,
        "{}",
        mock.log()
    );
    let updates = marked(&mock, "MOCK_TASK_UPDATE");
    assert_eq!(updates.len(), 1, "{updates:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `detach` and `input` are about the task an MCP server makes of a call: on
/// a `tool` step whose tool is no MCP server's, they refuse the start, which
/// exits 2 naming the step and the tool; on one that is, they load.
#[test]
fn detach_on_a_tool_no_mcp_server_serves_refuses_the_start() {
    let mock = mock();
    let steps = "      own: {kind: tool, depends_on: [s], name: memory.get, detach: true, args: {key: k}}\n\
                 \x20     f: {kind: finish, depends_on: [own], status: completed}\n";
    let (code, log) = life(
        "tasks-detach-internal",
        &config(&mock.uri(), "", steps, MEMORY),
    );
    assert_eq!(code, Some(2), "{log}");
    assert!(
        events(&log, "config.invalid").iter().any(|e| e["error"]
            .as_str()
            .is_some_and(|m| m.contains("step \"own\"")
                && m.contains("tool \"memory.get\" is not on an MCP server's route"))),
        "{log}"
    );
    assert!(marked(&mock, "MOCK_TASK_CREATED").is_empty());
}
