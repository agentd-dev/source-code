// SPDX-License-Identifier: AGPL-3.0-only
//! agentd as a client of the MCP Tasks extension (SEP-2663,
//! `io.modelcontextprotocol/tasks`), read from what it logs and from what
//! reached the server: a workflow step whose `tools/call` the server answers
//! with a `CreateTaskResult` follows the task to its result, its timeout
//! sends `tasks/cancel`, and `tasks: off` declares nothing — so a server that
//! requires the extension is refused, with an error that names it. The
//! server is the built-in mock at MCP 2026-07-28, whose `mock.job` becomes a
//! task only for a call that declares the extension, whose `tasks/*` refuse a
//! request without `Mcp-Name` or the extension, and which writes one line per
//! request it answers.

use crate::checks::util::write_file;
use crate::{Category, Check, Harness, Outcome};
use serde_json::{Value, json};

const EXTENSION: &str = "io.modelcontextprotocol/tasks";

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "mcp-tasks/a-step-follows-its-task-to-the-result",
            category: Category::McpTasks,
            desc: "a workflow step whose tools/call becomes a task finishes with the task's result",
            run: a_step_follows_its_task,
        },
        Check {
            id: "mcp-tasks/the-step-timeout-cancels-the-task",
            category: Category::McpTasks,
            desc: "a step's timeout ends its wait for a task and sends tasks/cancel",
            run: the_timeout_cancels,
        },
        Check {
            id: "mcp-tasks/off-refuses-a-server-that-requires-tasks",
            category: Category::McpTasks,
            desc: "with tasks: off, a server that requires the extension fails the call with an error naming it",
            run: off_refuses,
        },
    ]
}

/// A job: one `once` run of `step` against the server `mock`, its output the
/// run's.
fn config(endpoint: &str, server_extra: &str, step: &str) -> String {
    format!(
        "agent:\n  name: tasks\nstore:\n  kind: memory\n\
         mcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\n{server_extra}\
         workflows:\n  - name: job\n    steps:\n      s: {{kind: once}}\n      call: {step}\n\
         \x20     f: {{kind: finish, depends_on: [call], status: completed, output: \"{{{{ steps.call.output }}}}\"}}\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 200ms\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

/// What one run shows: agentd's exit code, its events and its log, and the
/// mock's record of what reached it (`MOCK_TASK_CREATED <id> …`,
/// `MOCK_TASK_GET <id> …`, `MOCK_TASK_CANCEL <id>`, `MOCK_REFUSED …`).
struct Seen {
    code: Option<i32>,
    events: Vec<Value>,
    stderr: String,
    server: String,
}

impl Seen {
    fn named(&self, name: &str) -> Vec<&Value> {
        self.events.iter().filter(|e| e["event"] == name).collect()
    }

    /// The `step.done` of the step `call`.
    fn call(&self) -> Option<Value> {
        self.named("step.done")
            .into_iter()
            .find(|e| e["step"] == "call")
            .cloned()
    }

    /// The server's lines that begin with `marker`.
    fn served(&self, marker: &str) -> Vec<&str> {
        self.server
            .lines()
            .filter(|l| l.starts_with(marker))
            .collect()
    }
}

/// Run `step` once against a 2026-07-28 mock.
fn run(h: &Harness, server_extra: &str, step: &str) -> Seen {
    let tmp = h.tempdir();
    let log = tmp.path().join("mcp.log");
    let mock = h.spawn_mock_mcp_at(
        &tmp.path().join("mcp.addr"),
        "mock://res",
        "2026-07-28",
        &log,
    );
    let cfg = write_file(
        &tmp,
        "agentd.yaml",
        &config(&mock.endpoint(), server_extra, step),
    );
    let r = h.run(&["--config", &cfg]);
    drop(mock);
    Seen {
        code: r.code,
        events: r.events(),
        stderr: r.stderr,
        server: std::fs::read_to_string(&log).unwrap_or_default(),
    }
}

fn a_step_follows_its_task(h: &Harness) -> Outcome {
    let s = run(
        h,
        "",
        "{kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: done, after_ms: 600, poll_ms: 200}}",
    );
    let done = s.named("run.done");
    Outcome::require(
        s.code == Some(0),
        format!("exit {:?}; stderr:\n{}", s.code, s.stderr),
    )
    .and(|| {
        Outcome::require(
            s.served("MOCK_TASK_CREATED").len() == 1,
            format!("the call did not become one task; server:\n{}", s.server),
        )
    })
    .and(|| {
        Outcome::require(
            !s.served("MOCK_TASK_GET").is_empty() && s.served("MOCK_REFUSED").is_empty(),
            format!(
                "the task was not polled with tasks/get as the extension requires; server:\n{}",
                s.server
            ),
        )
    })
    .and(|| {
        Outcome::require(
            done.first()
                .is_some_and(|d| d["output"] == json!({"done": true})),
            format!("the run's output is not the task's result: {done:?}"),
        )
    })
}

fn the_timeout_cancels(h: &Harness) -> Outcome {
    let s = run(
        h,
        "",
        "{kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {script: stuck, poll_ms: 200}, timeout: 1s}",
    );
    let step = s.call();
    Outcome::require(
        step.as_ref().is_some_and(|c| c["status"] == "timeout"),
        format!("the step did not time out: {step:?}; stderr:\n{}", s.stderr),
    )
    .and(|| {
        Outcome::require(
            s.served("MOCK_TASK_CANCEL") == ["MOCK_TASK_CANCEL mock-task-0"]
                && s.served("MOCK_REFUSED").is_empty(),
            format!(
                "the server was not sent tasks/cancel for the task; server:\n{}",
                s.server
            ),
        )
    })
}

fn off_refuses(h: &Harness) -> Outcome {
    let s = run(
        h,
        "      tasks: off\n",
        "{kind: mcp.tool, depends_on: [s], server: mock, tool: mock.job, args: {require: true}}",
    );
    let step = s.call();
    let err = step
        .as_ref()
        .and_then(|c| c["err"].as_str())
        .unwrap_or_default()
        .to_string();
    Outcome::require(
        step.as_ref().is_some_and(|c| c["status"] == "failed"),
        format!("the call was not refused: {step:?}; stderr:\n{}", s.stderr),
    )
    .and(|| {
        Outcome::require(
            err.contains(EXTENSION) && err.contains("-32021"),
            format!("the error does not name the extension: {err}"),
        )
    })
    .and(|| {
        Outcome::require(
            s.served("MOCK_TASK_CREATED").is_empty(),
            "a task was made with tasks: off",
        )
    })
}
