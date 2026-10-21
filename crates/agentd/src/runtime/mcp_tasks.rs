// SPDX-License-Identifier: AGPL-3.0-only
//! **MCP Tasks** (RFC 0050): the tasks MCP servers answered agentd's tool
//! calls with, followed by the supervisor to their end.
//!
//! A server that cannot finish a `tools/call` in one answer may hand back a
//! task (the Tasks extension, SEP-2663). Every call path follows one: most
//! inline, inside the call ([`::mcp::client::McpClient::follow_task`]); two
//! leave it here, because their task can outlive the call that made it:
//!
//! - **A workflow step's.** A `tool` step on an MCP route or an `mcp.tool`
//!   step whose call becomes a task suspends on a durable wait
//!   (`{kind: mcp_task, task: <handle>}`) until the task ends, and its
//!   outcome finishes the step. The step's timeout bounds the wait (the wait
//!   record's `deadline_ms`, resolved by `poll_waits`); a step that ends any
//!   other way — its timeout, its run cancelled, its scope torn down — no
//!   longer waits on the record, and [`Runtime::mcp_tasks_tick`] cancels the
//!   task with `tasks/cancel`.
//! - **A turn's, past the call's bound.** The worker follows its task within
//!   the call and hands one still open over ([`HAND_OVER`]); the model then
//!   follows it with `mcp.task.get` and `mcp.task.cancel`, which answer only
//!   the conversation and principal it was made for. One a workflow's
//!   `agent` step handed over is its run's: when the run ends, an open one is
//!   cancelled and an unread outcome dropped.
//! - **One a step detached** (`detach: true`, RFC 0054). The step finished
//!   at once with the task's handle, and the task is its run's: a
//!   `wait {on: mcp_task}` of the run collects its outcome, which is kept
//!   once it ended for as many waits as read it, `mcp.task.get` and
//!   `mcp.task.cancel` reach it from the run's `tool` steps, and the run's end
//!   cancels it while it is open.
//!
//! Each task is a record in the store (`_mcptask/<handle>`, listed in
//! `_mcptask/index`), so a restart reads it back and polls again. Its handle
//! names it everywhere agentd speaks of it: a server may use task ids as
//! bearer tokens, so the id is kept in the record and sent only to its
//! server.
//!
//! Polls run on threads of their own, never the loop: at most [`POLLERS`] at
//! once, one per task, at the task's own interval
//! ([`::mcp::tasks::Tracker::interval`], never below its floor) — the ceiling
//! for a task the server's listen acknowledged it will push. A failed poll
//! waits 1 s, doubling to 5 minutes. The supervisor's connections declare no
//! `elicitation`, and a turn that handed its task over has moved on, so an
//! input request here is declined, as the same request sent directly would
//! be; one for something agentd never declares cancels the task. A poll's
//! declines go back in one `tasks/update` from the poll's thread, before the
//! loop records them, so a restart in between declines again rather than
//! leaving a key unanswered; an update that fails is a failed poll, and
//! waits as one does.
//!
//! **A question put to a person** (RFC 0054). A step's own or detached task
//! whose step sets `input` has its form elicitations asked instead, through
//! the gate a `human` node opens ([`Runtime::ask_human`]), the gate's answer
//! coming back here as a [`Target::McpTask`] reply. Over A2A the gate opens
//! on a task made for the question, in the run's conversation, never on the
//! run's own task: a task holds one gate, and a question the task asks at
//! any moment must neither take the run's from its `human` steps nor be
//! refused while one of theirs is open. A round's keys are asked one at a
//! time, and their answers, a URL elicitation's `decline` among them, go to
//! the server together in one `tasks/update` from the next poll's thread.
//! An answer is kept in memory only, and a key is stored as answered once
//! the server acknowledged it, so a restart ends the question's task and
//! asks again on a new one rather than losing the key.
//!
//! A task given up while the server may still run it — past its TTL, or a
//! request about it failed for good — is cancelled with `tasks/cancel`; one
//! the server does not know is not.

use super::events::Event;
use super::reactor::{PendingKind, Runtime, Target};
use super::tools::ToolOutcome;
use crate::engine::run::{RunStatus, StepStatus};
use crate::obs::trace::TraceContext;
use crate::state::{Kind, now_ms};
use ::mcp::rpc::RpcError;
use ::mcp::tasks::{Ask, Tracker};
use ::mcp::wire::{Task, TaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

/// The most tasks the supervisor keeps open on one server, its declaring
/// calls still in flight counted among them. At that many, its own calls to
/// the server go out without the extension until one ends.
pub(crate) const PER_SERVER: usize = 32;
/// How many polls are in flight at once.
const POLLERS: usize = 8;
/// The record listing every handle.
const INDEX: &str = "_mcptask/index";
/// The request a turn worker hands an unfinished task over with: a name no
/// registry tool has, so a model cannot send it.
pub(crate) const HAND_OVER: &str = "mcp.task.hand_over";
/// How long an ended turn's task waits for `mcp.task.get` to read it.
const UNREAD_FOR_MS: u64 = 24 * 3600 * 1000;
/// The longest pause after failed polls.
const RETRY_MAX_MS: u64 = 300_000;
/// The bound on a listen that asks for task ids. It holds the connection's
/// subscribe lock, which the loop's own subscribes wait on, so it is the
/// loop's management bound; a listen that misses it leaves the server's
/// tasks on polling.
const LISTEN_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

/// What the model is told when its call became a task still running.
pub(crate) const STILL_RUNNING: &str = "The call became an MCP task that is still running. Read its status, and its result once it ends, with mcp.task.get; cancel it with mcp.task.cancel.";

/// The handle a task is known by in agentd: `mt_` and 16 hex digits of
/// SHA-256 over the server and the task id.
pub fn handle(server: &str, task_id: &str) -> String {
    let hex = crate::sha::sha256_hex(format!("{server}\0{task_id}").as_bytes());
    format!("mt_{}", &hex[..16])
}

fn record_key(handle: &str) -> String {
    format!("_mcptask/{handle}")
}

/// Who waits for a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Owner {
    /// A workflow step, suspended on it. `registry` when its call went
    /// through the tool registry (a `tool` step), whose error is the result
    /// as JSON; an `mcp.tool` step's is the result's text.
    Step {
        run: String,
        step: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        registry: bool,
    },
    /// A turn's call handed it over: the conversation (or run) and the
    /// principal it was made for follow it with the `mcp.task.*` tools.
    Turn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ctx: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<String>,
    },
    /// A workflow step detached it (RFC 0054): the step finished with its
    /// handle, and the task is the run's until the run ends.
    Detached { run: String, step: String },
}

impl Owner {
    /// The step whose `input` a task's question is asked under: the one
    /// waiting on it, or the one that detached it.
    fn step(&self) -> Option<(&str, &str)> {
        match self {
            Owner::Step { run, step, .. } | Owner::Detached { run, step } => {
                Some((run.as_str(), step.as_str()))
            }
            Owner::Turn { .. } => None,
        }
    }
}

/// One round of a task's questions being put to a person (RFC 0054), kept
/// in memory only: a restart asks again what the server did not take.
#[derive(Debug, Default)]
pub(crate) struct Round {
    /// The asks not put yet, in key order.
    queue: VecDeque<Ask>,
    /// The key a gate is open for, with the schema its answer is shaped to.
    asking: Option<(String, Value)>,
    /// The answers given, by key: sent together once none is left to ask.
    given: BTreeMap<String, Value>,
}

impl Round {
    /// Whether `key` is in this round already.
    fn holds(&self, key: &str) -> bool {
        self.given.contains_key(key)
            || self.asking.as_ref().is_some_and(|(k, _)| k == key)
            || self.queue.iter().any(|a| a.key() == key)
    }
}

/// Who answers a task's question, and for how long: a step's `input`.
#[derive(Debug, Clone)]
struct InputRoute {
    to: Option<Value>,
    timeout: Option<Value>,
}

/// One task the supervisor follows, as kept in the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub handle: String,
    pub server: String,
    pub tool: String,
    pub tracker: Tracker,
    pub owner: Owner,
    /// The span of the call that made it: its polls are spans under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceContext>,
    /// When a turn's task ended (Unix ms), waiting for `mcp.task.get`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
    /// Why it ended short of the server's word: the server does not know
    /// it, it outlived its TTL, it asked for something agentd never
    /// declares, or a request about it failed for good.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost: Option<String>,
}

impl Record {
    pub(crate) fn open(&self) -> bool {
        self.ended_ms.is_none() && self.lost.is_none() && !self.tracker.ended()
    }
}

/// The supervisor's calls in flight that declared the extension, by server:
/// each may come back a task, so each holds a place under [`PER_SERVER`]
/// until it answers or its task is recorded.
#[derive(Debug, Default, Clone)]
pub(crate) struct Calls(Arc<std::sync::Mutex<BTreeMap<String, usize>>>);

impl Calls {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, usize>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One declaring call's place among [`Calls`], given up when it is dropped:
/// by the call's thread once it answered, or by the loop once the task it
/// became is recorded.
#[derive(Debug)]
pub struct Calling {
    server: String,
    calls: Calls,
}

impl Drop for Calling {
    fn drop(&mut self) {
        let mut m = self.calls.lock();
        if let Some(n) = m.get_mut(&self.server) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.server);
            }
        }
    }
}

/// The tasks the supervisor follows, and the polls and listens in flight.
#[derive(Default)]
pub(crate) struct Tasks {
    pub(crate) records: BTreeMap<String, Record>,
    /// Handles with a poll in flight.
    polling: BTreeSet<String>,
    /// When each handle is due its next poll (Unix ms). Not kept: a restart
    /// polls each at once.
    due: BTreeMap<String, u64>,
    /// Failed polls in a row, by handle.
    failures: BTreeMap<String, u32>,
    /// Handles the server's listen acknowledged it will push.
    pushed: BTreeSet<String>,
    /// Per server, the task ids its listen last asked for.
    listened: BTreeMap<String, BTreeSet<String>>,
    /// Servers with a listen in flight.
    listening: BTreeSet<String>,
    /// Declaring calls in flight.
    calls: Calls,
    /// By handle, the round of questions a person is being asked.
    rounds: BTreeMap<String, Round>,
    /// `tasks/cancel` requests still on the wire: an idle exit waits for
    /// them, as it waits for a `forward:` push, so a run that ends with its
    /// task cancelled does not exit before the server is told.
    pub(crate) cancels: Arc<std::sync::atomic::AtomicUsize>,
}

impl Tasks {
    /// The run whose step made or detached task `h`.
    pub(crate) fn task_run(&self, h: &str) -> Option<String> {
        self.records
            .get(h)
            .and_then(|r| r.owner.step())
            .map(|(run, _)| run.to_string())
    }

    /// How many tasks are open on `server`.
    pub(crate) fn open_on(&self, server: &str) -> usize {
        self.records
            .values()
            .filter(|r| r.server == server && r.open())
            .count()
    }

    /// When the next poll is due (Unix ms), for the loop's wake-up: a task
    /// is polled at its server's interval, not at the next tick.
    pub(crate) fn next_due(&self) -> Option<u64> {
        // With every poller busy, the next answer wakes the loop anyway.
        if self.polling.len() >= POLLERS {
            return None;
        }
        self.due
            .iter()
            .filter(|(h, _)| !self.polling.contains(*h))
            .map(|(_, d)| *d)
            .min()
    }
}

/// What a thread reports about a task.
#[derive(Debug)]
pub enum Note {
    /// A step's call became a task. `calling` is the call's place under
    /// [`PER_SERVER`], held until the task is recorded.
    Created {
        run: String,
        step: String,
        server: String,
        tool: String,
        task: Task,
        trace: Option<TraceContext>,
        registry: bool,
        calling: Option<Calling>,
    },
    /// A poll answered. `declined` are the input keys its thread answered
    /// `decline` (with the request's method); `unsupported`, the method of a
    /// request that made it cancel the task; `undeclined`, why the
    /// `tasks/update` that declined — or sent a person's answers — failed:
    /// the server's error, or the transport's; `sent`, the keys of the
    /// answers the server took.
    Polled {
        handle: String,
        answer: Result<Result<Task, RpcError>, String>,
        declined: Vec<(String, String)>,
        unsupported: Option<String>,
        undeclined: Option<Result<RpcError, String>>,
        sent: Vec<String>,
    },
    /// A listen asked for `asked` and the server took `acked`.
    Listened {
        server: String,
        asked: BTreeSet<String>,
        acked: Result<BTreeSet<String>, String>,
    },
}

/// A server's text, for a log line: no control characters, at most 200
/// bytes, so it cannot drive an operator's terminal.
fn tame(s: &str) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).collect();
    if out.len() > 200 {
        let mut cut = 200;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// The step outcome a task's end makes: `(output, is_error, error)`, shaped
/// as the call's own answer would have been.
fn step_outcome(rec: &Record, registry: bool) -> (Value, bool, Option<String>) {
    if let Some(why) = &rec.lost {
        return (
            Value::Null,
            true,
            Some(format!(
                "mcp server '{}': tools/call {}: {why}",
                rec.server, rec.tool
            )),
        );
    }
    let op = format!("tools/call {}", rec.tool);
    match ::mcp::client::task_outcome(&rec.server, &op, &rec.tracker.task) {
        Ok(res) => {
            let v = super::worker::tool_result_value(&res);
            match (res.is_error(), registry) {
                (false, _) => (v, false, None),
                (true, true) => {
                    let e = v.to_string();
                    (v, true, Some(e))
                }
                (true, false) => (v, true, Some(res.text())),
            }
        }
        Err(f) => (
            Value::Null,
            true,
            Some(format!("transport error: {}", f.error)),
        ),
    }
}

/// What a `wait {on: mcp_task}` resolves with once its task ended (RFC
/// 0054): `{task, server, tool, status, ok, result?, error?}`. `status` is
/// the server's final word, or `failed` when agentd stopped following the
/// task short of it; `ok` holds only for a completed task whose result is no
/// error.
fn waited_outcome(rec: &Record) -> Value {
    let mut out = json!({"task": rec.handle, "server": rec.server, "tool": rec.tool});
    if let Some(why) = &rec.lost {
        out["status"] = json!("failed");
        out["ok"] = json!(false);
        out["error"] = json!(why);
        return out;
    }
    out["status"] = json!(rec.tracker.task.status.as_str());
    let op = format!("tools/call {}", rec.tool);
    match ::mcp::client::task_outcome(&rec.server, &op, &rec.tracker.task) {
        Ok(res) => {
            out["ok"] = json!(!res.is_error());
            out["result"] = super::worker::tool_result_value(&res);
            if res.is_error() {
                out["error"] = json!(res.text());
            }
        }
        Err(f) => {
            out["ok"] = json!(false);
            out["error"] = json!(f.error.to_string());
        }
    }
    out
}

/// How long until a task the server just described is polled again: what no
/// poll's thread has answered yet — a pushed question — at the floor, a task
/// the server's listen pushes at the ceiling, any other at its own interval.
/// Never below the floor.
fn next_poll_ms(tracker: &Tracker, asking: bool, pushed: bool) -> u64 {
    let next = if asking {
        ::mcp::tasks::POLL_FLOOR
    } else if pushed {
        ::mcp::tasks::POLL_CEILING
    } else {
        tracker.interval()
    };
    next.as_millis() as u64
}

/// How a task ended, for the counter: `completed`, `failed`, `cancelled` or
/// `expired`.
fn outcome_label(rec: &Record) -> &'static str {
    match (&rec.lost, rec.tracker.task.status) {
        (Some(_), _) => "expired",
        (None, TaskStatus::Completed) => "completed",
        (None, TaskStatus::Cancelled) => "cancelled",
        _ => "failed",
    }
}

/// Why following a task stopped short, as a record says it: the reason's
/// word, and the sentence, the server's own text in it tamed.
fn lost_said(lost: &::mcp::tasks::Lost) -> (&'static str, String) {
    use ::mcp::tasks::Lost;
    let tamed = match lost {
        Lost::Unknown(m) => Lost::Unknown(tame(m)),
        Lost::Unsupported(m) => Lost::Unsupported(tame(m)),
        Lost::Failed(m) => Lost::Failed(tame(m)),
        Lost::Expired => Lost::Expired,
    };
    let word = match lost {
        Lost::Unknown(_) => "unknown",
        Lost::Expired => "ttl",
        Lost::Unsupported(_) => "unsupported",
        Lost::Failed(_) => "failed",
    };
    (word, tamed.to_string())
}

impl Runtime {
    /// Read back the tasks a previous life followed, and poll each at once.
    pub(crate) fn restore_mcp_tasks(&mut self) {
        let Ok(Some(index)) = self.durable.get(Kind::Memory, INDEX) else {
            return;
        };
        let handles: Vec<String> = index.state["handles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let now = now_ms();
        for h in handles {
            let rec = match self.durable.get(Kind::Memory, &record_key(&h)) {
                Ok(Some(env)) => serde_json::from_value::<Record>(env.state),
                _ => continue,
            };
            let Ok(rec) = rec else {
                self.log.warn("mcp.task.corrupt", json!({"task": h}));
                continue;
            };
            self.log.in_trace(rec.trace.as_ref()).info(
                "mcp.task.resumed",
                json!({"server": rec.server, "task": rec.handle,
                       "status": rec.tracker.task.status.as_str()}),
            );
            // An ended turn's task waits for `mcp.task.get`, not for a poll.
            if rec.open() {
                self.mcp_tasks.due.insert(h.clone(), now);
            }
            self.mcp_tasks.records.insert(h, rec);
        }
        self.mcp_task_gauge();
    }

    /// Keep `handle`'s record, and the index, in the store.
    fn persist_task(&mut self, handle: &str) -> Result<(), String> {
        let Some(rec) = self.mcp_tasks.records.get(handle) else {
            return Ok(());
        };
        // A key whose answer the server has not taken is not kept as
        // answered: the answer itself is a person's words, never stored, so
        // a restart asks for it again.
        let mut rec = rec.clone();
        rec.tracker.release_unsent();
        let state = serde_json::to_value(&rec).map_err(|e| e.to_string())?;
        self.durable
            .put(Kind::Memory, &record_key(handle), state, None)
            .map_err(|e| e.to_string())?;
        self.persist_task_index()
    }

    fn persist_task_index(&mut self) -> Result<(), String> {
        let handles: Vec<&String> = self.mcp_tasks.records.keys().collect();
        self.durable
            .put(Kind::Memory, INDEX, json!({"handles": handles}), None)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Forget `handle`: its record, its place in the index, its schedule.
    fn drop_task(&mut self, handle: &str) -> Option<Record> {
        let rec = self.mcp_tasks.records.remove(handle)?;
        self.mcp_tasks.due.remove(handle);
        self.mcp_tasks.failures.remove(handle);
        self.mcp_tasks.pushed.remove(handle);
        let _ = self.durable.delete(Kind::Memory, &record_key(handle));
        let _ = self.persist_task_index();
        self.mcp_task_gauge();
        self.close_task_questions(handle);
        Some(rec)
    }

    /// Close the gate a task's question is open on, its answer no longer
    /// wanted: the task ended, or nothing follows it any more.
    fn close_task_questions(&mut self, h: &str) {
        self.mcp_tasks.rounds.remove(h);
        let mut closed = Vec::new();
        self.pending.retain(|p| match (&p.target, &p.kind) {
            (
                Target::McpTask(t, _),
                PendingKind::Human {
                    task, standalone, ..
                },
            ) if t == h => {
                closed.push((task.clone(), *standalone));
                false
            }
            _ => true,
        });
        for (task, standalone) in closed {
            self.human_task_settle(&task, standalone, "the MCP task no longer asks");
        }
    }

    /// The task an MCP task's question opens on over A2A (RFC 0054): one
    /// made for the question, in the conversation of the run whose step set
    /// `input` (`run-<id>` for a run started in none), held by whoever the
    /// run runs for or else the operator, as a gate task made for a run is.
    /// Never the run's own task: that holds one gate at a time — its `human`
    /// steps' and policy gates' — and cancelling it cancels the run.
    #[cfg(feature = "a2a")]
    pub(crate) fn mcp_question_task(
        &mut self,
        caller: &super::tools::ToolCaller,
        handle: &str,
    ) -> String {
        use crate::a2a::tasks::Link;
        let run = caller.run.clone().unwrap_or_default();
        let ctx = self
            .runs
            .get(&run)
            .and_then(|r| r.conversation.clone())
            .unwrap_or_else(|| format!("run-{run}"));
        let principal = crate::a2a::Principal {
            id: caller
                .principal
                .clone()
                .unwrap_or_else(|| "operator".to_string()),
            role: crate::config::settings::Role::Operator,
            ..crate::a2a::Principal::anonymous()
        };
        // A freshly minted id, never the listener's reservation: that belongs
        // to the task the request being served creates.
        let reserved = self.reserved_task_id.take();
        let task = self.task_create(&ctx, &principal, Link::Turn { ctx: ctx.clone() }, None);
        self.reserved_task_id = reserved;
        if let Some(t) = self.tasks.get_mut(&task) {
            t.mcp_question = Some(handle.to_string());
        }
        task
    }

    /// After a restore: a question a previous life put on a task of its own
    /// ends, since the answer it waited for could reach nobody now. The task
    /// is polled again, and a question the server still asks is put on a new
    /// task.
    #[cfg(feature = "a2a")]
    pub(crate) fn interrupt_mcp_task_questions(&mut self) {
        use crate::a2a::tasks::State;
        let open: Vec<(String, String)> = self
            .tasks
            .values()
            .filter(|t| !t.state.is_terminal())
            .filter_map(|t| Some((t.id.clone(), t.mcp_question.clone()?)))
            .collect();
        for (task, h) in open {
            if let Some(t) = self.tasks.get_mut(&task) {
                t.transition(
                    State::Failed,
                    Some("The question was interrupted: agentd restarted before it was answered. If the MCP server still asks, it is put again on a new task.".into()),
                );
            }
            self.task_persist(&task);
            self.task_sync(&task);
            self.log.info(
                "mcp.task.input.interrupted",
                json!({"task": h, "gate": task}),
            );
        }
    }

    fn mcp_task_gauge(&self) {
        let mut by: BTreeMap<String, u64> = BTreeMap::new();
        for r in self.mcp_tasks.records.values().filter(|r| r.open()) {
            *by.entry(r.server.clone()).or_default() += 1;
        }
        crate::obs::metrics::set_mcp_tasks_open(by.into_iter().collect());
    }

    /// A place for a new call to `server` from the supervisor that declares
    /// the extension: `None` when its open tasks and its declaring calls in
    /// flight already reach [`PER_SERVER`], and the call goes out without it.
    pub(crate) fn mcp_task_slot(&self, server: &str) -> Option<Calling> {
        let open = self.mcp_tasks.open_on(server);
        let mut calls = self.mcp_tasks.calls.lock();
        let flying = calls.entry(server.to_string()).or_default();
        if open + *flying >= PER_SERVER {
            if *flying == 0 {
                calls.remove(server);
            }
            return None;
        }
        *flying += 1;
        Some(Calling {
            server: server.to_string(),
            calls: self.mcp_tasks.calls.clone(),
        })
    }

    /// A thread's word about a task.
    pub(crate) fn on_mcp_task(&mut self, note: Note) {
        match note {
            Note::Created {
                run,
                step,
                server,
                tool,
                task,
                trace,
                registry,
                // The call's place is given up once its task is recorded.
                calling: _held,
            } => self.mcp_task_created(&run, &step, &server, &tool, task, trace, registry),
            Note::Polled {
                handle,
                answer,
                declined,
                unsupported,
                undeclined,
                sent,
            } => {
                self.mcp_task_sent(&handle, &sent);
                self.mcp_task_polled(&handle, answer, declined, unsupported, undeclined)
            }
            Note::Listened {
                server,
                asked,
                acked,
            } => {
                self.mcp_tasks.listening.remove(&server);
                let took = match acked {
                    Ok(took) => took,
                    Err(e) => {
                        self.log.info(
                            "mcp.task.listen.unavailable",
                            json!({"server": server, "err": e}),
                        );
                        BTreeSet::new()
                    }
                };
                for r in self.mcp_tasks.records.values() {
                    if r.server == server {
                        if took.contains(&r.tracker.task.task_id) {
                            self.mcp_tasks.pushed.insert(r.handle.clone());
                        } else {
                            self.mcp_tasks.pushed.remove(&r.handle);
                        }
                    }
                }
                self.mcp_tasks.listened.insert(server, asked);
            }
        }
    }

    /// A step's call became a task: record it, and suspend the step on it.
    #[allow(clippy::too_many_arguments)]
    fn mcp_task_created(
        &mut self,
        run: &str,
        step: &str,
        server: &str,
        tool: &str,
        task: Task,
        trace: Option<TraceContext>,
        registry: bool,
    ) {
        self.executing.remove(&format!("{run}/{step}"));
        let h = handle(server, &task.task_id);
        let started = self
            .runs
            .get(run)
            .filter(|r| !r.status.is_terminal())
            .and_then(|r| r.steps.get(step))
            .filter(|s| s.status == StepStatus::Running)
            .and_then(|s| s.started);
        let Some(started) = started else {
            // Nothing waits for it any more: the step ended while its call
            // was out.
            self.cancel_task_on(server, &task.task_id, trace.as_ref());
            self.log.in_trace(trace.as_ref()).info(
                "mcp.task.cancelled",
                json!({"server": server, "task": h,
                       "reason": "the step ended before its task was recorded"}),
            );
            return;
        };
        let seed_status = task.status;
        // A step that detaches finishes now, and the task is its run's (RFC
        // 0054); any other waits on it.
        let detach = self
            .step_field(run, step, "detach")
            .and_then(|d| d.as_bool())
            == Some(true);
        let owner = if detach {
            Owner::Detached {
                run: run.to_string(),
                step: step.to_string(),
            }
        } else {
            Owner::Step {
                run: run.to_string(),
                step: step.to_string(),
                registry,
            }
        };
        let rec = Record {
            handle: h.clone(),
            server: server.to_string(),
            tool: tool.to_string(),
            tracker: Tracker::new(task, now_ms()),
            owner,
            trace,
            ended_ms: None,
            lost: None,
        };
        self.log.in_trace(rec.trace.as_ref()).info(
            "mcp.task.created",
            json!({"server": server, "tool": tool, "task": h, "status": seed_status.as_str(),
                   "ttl_ms": rec.tracker.task.ttl_ms, "poll_interval_ms": rec.tracker.task.poll_interval_ms,
                   "run": run, "step": step}),
        );
        let interval = rec.tracker.interval();
        let task_id = rec.tracker.task.task_id.clone();
        let call = rec.trace.clone();
        // A seed in a final status carries no outcome: only a `cancelled`
        // one has ended, and the rest are fetched at once.
        let ended = rec.tracker.ended();
        self.mcp_tasks.records.insert(h.clone(), rec);
        if let Err(e) = self.persist_task(&h) {
            // A task agentd cannot keep is one it cannot promise to follow.
            self.mcp_tasks.records.remove(&h);
            self.cancel_task_on(server, &task_id, call.as_ref());
            self.finish_step_pub(
                run,
                step,
                StepStatus::Failed,
                None,
                Some(format!(
                    "mcp server '{server}': tools/call {tool}: the task could not be kept ({e}); tasks/cancel sent"
                )),
                0,
            );
            return;
        }
        self.mcp_tasks
            .due
            .insert(h.clone(), now_ms() + interval.as_millis() as u64);
        self.mcp_task_gauge();
        if detach {
            self.log.in_trace(call.as_ref()).info(
                "mcp.task.detached",
                json!({"server": server, "task": h, "run": run, "step": step}),
            );
            self.finish_step_pub(
                run,
                step,
                StepStatus::Done,
                Some(json!({"task": {"server": server, "id": h, "status": seed_status.as_str()}})),
                None,
                0,
            );
            if ended {
                self.mcp_task_ended(&h);
            }
            return;
        }
        // The step's own timeout bounds the wait, from the step's start.
        let timeout = self.step_task_timeout(run, step);
        let left = timeout.map(|t| (started + t).saturating_sub(now_ms()));
        self.suspend_wait(
            run,
            step,
            super::waits::wait_record("mcp_task", json!({"task": h, "server": server}), left),
        );
        if ended {
            self.mcp_task_ended(&h);
        }
    }

    /// A field of step `step` of run `run`, as its definition writes it.
    fn step_field(&self, run: &str, step: &str, field: &str) -> Option<Value> {
        let wf = self.definition_for_run(run)?;
        let r = self.runs.get(run)?;
        let (s, _) = self.resolve_step(&wf, r, step)?;
        s.field(field).cloned()
    }

    /// The bound of a step's task wait: its `timeout`, and for an `mcp.tool`
    /// step the default every such call gets.
    fn step_task_timeout(&self, run: &str, step: &str) -> Option<u64> {
        let wf = self.definition_for_run(run)?;
        let r = self.runs.get(run)?;
        let (s, _) = self.resolve_step(&wf, r, step)?;
        s.timeout_ms.or_else(|| {
            (s.kind == "mcp.tool").then(|| {
                self.settings
                    .limits
                    .step_timeout
                    .map(|d| d.0.as_millis() as u64)
                    .unwrap_or(600_000)
            })
        })
    }

    /// A turn worker handed over a task its call made that was still open at
    /// the call's bound. The task is kept for the conversation and principal
    /// it was made for, and the worker is told its handle.
    pub(crate) fn mcp_task_hand_over(
        &mut self,
        caller: &super::tools::ToolCaller,
        args: Value,
    ) -> ToolOutcome {
        let refuse = |e: String| ToolOutcome::Ready(Value::String(e), true);
        let server = args["server"].as_str().unwrap_or_default().to_string();
        let tool = args["tool"].as_str().unwrap_or_default().to_string();
        let tracker: Tracker = match serde_json::from_value(args["tracker"].clone()) {
            Ok(t) => t,
            Err(e) => return refuse(format!("{HAND_OVER}: {e}")),
        };
        if !self.mcp.contains_key(&server) {
            return refuse(format!(
                "{HAND_OVER}: mcp server {server:?} is not connected"
            ));
        }
        let trace = serde_json::from_value::<TraceContext>(args["trace"].clone()).ok();
        let h = handle(&server, &tracker.task.task_id);
        let rec = Record {
            handle: h.clone(),
            server: server.clone(),
            tool,
            tracker,
            owner: Owner::Turn {
                ctx: caller.ctx.clone(),
                run: caller.run.clone(),
                principal: caller.principal.clone(),
            },
            trace,
            ended_ms: None,
            lost: None,
        };
        let interval = rec.tracker.interval();
        self.mcp_tasks.records.insert(h.clone(), rec);
        if let Err(e) = self.persist_task(&h) {
            self.mcp_tasks.records.remove(&h);
            return refuse(format!("{HAND_OVER}: the task could not be kept: {e}"));
        }
        self.mcp_tasks
            .due
            .insert(h.clone(), now_ms() + interval.as_millis() as u64);
        self.mcp_task_gauge();
        let rec = &self.mcp_tasks.records[&h];
        self.log.in_trace(rec.trace.as_ref()).info(
            "mcp.task.handed_over",
            json!({"server": server, "task": h, "ctx": caller.ctx, "run": caller.run}),
        );
        ToolOutcome::Ready(json!({"task": h}), false)
    }

    /// `mcp.task.get` / `mcp.task.cancel`: a task a turn of the caller's
    /// conversation handed over, for its own principal only, or one a step
    /// of the caller's run detached (RFC 0054). A detached task stays the
    /// run's: reading its outcome leaves it for the run's waits, and
    /// cancelling it sends `tasks/cancel` and follows it on, so a wait
    /// resolves with what the server made of the cancel.
    pub(crate) fn mcp_task_tool(
        &mut self,
        caller: &super::tools::ToolCaller,
        name: &str,
        args: Value,
    ) -> ToolOutcome {
        let h = args["task"].as_str().unwrap_or_default().to_string();
        let turns = Owner::Turn {
            ctx: caller.ctx.clone(),
            run: caller.run.clone(),
            principal: caller.principal.clone(),
        };
        let (mine, detached) = match self.mcp_tasks.records.get(&h).map(|r| &r.owner) {
            Some(o) if *o == turns => (true, false),
            Some(Owner::Detached { run, .. })
                if caller.subagent.is_none() && caller.run.as_deref() == Some(run.as_str()) =>
            {
                (true, true)
            }
            _ => (false, false),
        };
        if !mine {
            return ToolOutcome::Ready(
                Value::String(format!(
                    "{name}: no task {h:?} was handed over in this conversation or detached in this run (or its result was already read)"
                )),
                true,
            );
        }
        let rec = self.mcp_tasks.records[&h].clone();
        let t = &rec.tracker.task;
        let mut out = json!({"task": h, "server": rec.server, "tool": rec.tool,
                             "status": t.status.as_str(), "created_at": t.created_at,
                             "last_updated_at": t.last_updated_at});
        if let Some(m) = &t.status_message {
            out["status_message"] = json!(m);
        }
        match name {
            "mcp.task.cancel" => {
                if rec.open() {
                    self.cancel_task_on(&rec.server, &t.task_id, rec.trace.as_ref());
                    self.log.in_trace(rec.trace.as_ref()).info(
                        "mcp.task.cancelled",
                        json!({"server": rec.server, "task": h, "reason": "mcp.task.cancel"}),
                    );
                    out["cancelled"] = json!(true);
                    // A detached task is followed to the server's word on
                    // it, which its end counts.
                    if detached {
                        return ToolOutcome::Ready(out, false);
                    }
                    crate::obs::metrics::record_mcp_task(&rec.server, "cancelled");
                } else {
                    out["cancelled"] = json!(false);
                }
                if !detached {
                    self.drop_task(&h);
                }
                ToolOutcome::Ready(out, false)
            }
            _ => {
                if rec.open() {
                    return ToolOutcome::Ready(out, false);
                }
                // Ended: a turn's outcome is handed over once; a detached
                // one is the run's until it ends.
                let (value, is_error, error) = step_outcome(&rec, false);
                match error {
                    Some(e) if value.is_null() => {
                        out["error"] = json!(e);
                    }
                    _ => {
                        out["tool_result"] = value;
                        out["is_error"] = json!(is_error);
                    }
                }
                if !detached {
                    self.drop_task(&h);
                }
                ToolOutcome::Ready(out, false)
            }
        }
    }

    /// The per-tick pass: cancel what nothing waits for, give up what
    /// outlived its TTL, drop an unread turn's outcome after a day, poll
    /// what is due, and ask each server's listen for its open tasks.
    pub(crate) fn mcp_tasks_tick(&mut self) {
        if self.mcp_tasks.records.is_empty() && self.mcp_tasks.listened.is_empty() {
            return;
        }
        let now = now_ms();
        let mut abandoned: Vec<(String, &'static str)> = Vec::new();
        let mut expired: Vec<String> = Vec::new();
        let mut unread: Vec<String> = Vec::new();
        for (h, r) in &self.mcp_tasks.records {
            match &r.owner {
                Owner::Step { run, step, .. } => {
                    let run_state = self.runs.get(run);
                    let waits = run_state
                        .filter(|r| !r.status.is_terminal())
                        .and_then(|r| r.steps.get(step))
                        .is_some_and(|s| {
                            s.status == StepStatus::Suspended
                                && s.wait.as_ref().is_some_and(|w| {
                                    w["kind"] == "mcp_task" && w["task"].as_str() == Some(h)
                                })
                        });
                    if !waits {
                        let reason = match (
                            run_state.map(|r| r.status),
                            run_state.and_then(|r| r.steps.get(step)).map(|s| s.status),
                        ) {
                            (_, Some(StepStatus::Timeout)) => "the step timed out",
                            (Some(RunStatus::Cancelled), _) => "the run was cancelled",
                            _ => "the step no longer waits for it",
                        };
                        abandoned.push((h.clone(), reason));
                        continue;
                    }
                }
                Owner::Turn { run, .. } => {
                    // A workflow's `agent` step handed it over: it is the
                    // run's, and goes when the run does.
                    if let Some(run) = run
                        && self.runs.get(run).is_none_or(|r| r.status.is_terminal())
                    {
                        abandoned.push((h.clone(), "the run ended"));
                        continue;
                    }
                    if r.ended_ms.is_some_and(|e| now > e + UNREAD_FOR_MS) {
                        unread.push(h.clone());
                        continue;
                    }
                }
                // A detached task is its run's, kept until the run ends —
                // whatever its waits did — and cancelled then if still open.
                Owner::Detached { run, .. } => match self.runs.get(run).map(|r| r.status) {
                    Some(s) if !s.is_terminal() => {}
                    Some(RunStatus::Cancelled) => {
                        abandoned.push((h.clone(), "the run was cancelled"));
                        continue;
                    }
                    _ => {
                        abandoned.push((h.clone(), "the run ended"));
                        continue;
                    }
                },
            }
            if r.open() && r.tracker.expired(now) {
                expired.push(h.clone());
            }
        }
        for (h, reason) in abandoned {
            let Some(rec) = self.drop_task(&h) else {
                continue;
            };
            let log = self.log.in_trace(rec.trace.as_ref());
            if rec.open() {
                self.cancel_task_on(&rec.server, &rec.tracker.task.task_id, rec.trace.as_ref());
                log.info(
                    "mcp.task.cancelled",
                    json!({"server": rec.server, "task": h, "reason": reason}),
                );
                crate::obs::metrics::record_mcp_task(&rec.server, "cancelled");
            } else if !matches!(rec.owner, Owner::Detached { .. }) {
                // Ended, its outcome never read.
                log.info(
                    "mcp.task.expired",
                    json!({"server": rec.server, "task": h, "reason": "unread",
                           "detail": "its run ended before mcp.task.get read it"}),
                );
            }
        }
        for h in unread {
            if let Some(rec) = self.drop_task(&h) {
                self.log.in_trace(rec.trace.as_ref()).info(
                    "mcp.task.expired",
                    json!({"server": rec.server, "task": h, "reason": "unread",
                           "detail": "its outcome was not read with mcp.task.get within a day"}),
                );
            }
        }
        for h in expired {
            self.mcp_task_lost(&h, &::mcp::tasks::Lost::Expired);
        }
        self.mcp_tasks_poll(now);
        self.mcp_tasks_listen();
    }

    /// Start the polls that are due.
    fn mcp_tasks_poll(&mut self, now: u64) {
        let due: Vec<String> = self
            .mcp_tasks
            .records
            .values()
            .filter(|r| r.open())
            .filter(|r| !self.mcp_tasks.polling.contains(&r.handle))
            .filter(|r| self.mcp_tasks.due.get(&r.handle).is_none_or(|d| *d <= now))
            .map(|r| r.handle.clone())
            .collect();
        for h in due {
            if self.mcp_tasks.polling.len() >= POLLERS {
                return;
            }
            let rec = &self.mcp_tasks.records[&h];
            // A server not connected now is asked again once it is, looked
            // for once a second.
            let Some(client) = self.mcp.get(&rec.server).cloned() else {
                self.mcp_tasks.due.insert(h, now + 1000);
                continue;
            };
            let (server, task_id) = (rec.server.clone(), rec.tracker.task.task_id.clone());
            let answered = rec.tracker.answered.clone();
            // A person answers this task's questions: its thread declines
            // none of them, and sends the answers given first.
            let asks = Asks {
                routed: self.task_input(rec).is_some(),
                answers: rec
                    .tracker
                    .unsent
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            };
            let call = rec.trace.as_ref().map(TraceContext::child);
            let tx = self.events_tx.clone();
            self.mcp_tasks.polling.insert(h.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("mcp.task:{server}"))
                .spawn(move || {
                    let note = poll(
                        &client,
                        &server,
                        &h,
                        &task_id,
                        &answered,
                        asks,
                        call.as_ref(),
                    );
                    let _ = tx.send(Event::McpTask(Box::new(note)));
                });
            if spawned.is_err() {
                self.mcp_tasks.polling.clear();
                return;
            }
        }
    }

    /// Ask each server's listen for the tasks open on it, when that set
    /// changed: a server that pushes them is polled at the ceiling only.
    fn mcp_tasks_listen(&mut self) {
        let mut want: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for r in self.mcp_tasks.records.values().filter(|r| r.open()) {
            want.entry(r.server.clone())
                .or_default()
                .insert(r.tracker.task.task_id.clone());
        }
        let servers: BTreeSet<String> = want
            .keys()
            .chain(self.mcp_tasks.listened.keys())
            .cloned()
            .collect();
        for server in servers {
            let ids = want.remove(&server).unwrap_or_default();
            if self.mcp_tasks.listening.contains(&server)
                || self.mcp_tasks.listened.get(&server) == Some(&ids)
            {
                continue;
            }
            let Some(client) = self.mcp.get(&server).cloned() else {
                continue;
            };
            if !client.declares_tasks() {
                continue;
            }
            if ids.is_empty() && !self.mcp_tasks.listened.contains_key(&server) {
                continue;
            }
            let tx = self.events_tx.clone();
            self.mcp_tasks.listening.insert(server.clone());
            let name = server.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("mcp.task.listen:{server}"))
                .spawn(move || {
                    let acked = client
                        .listen_tasks(ids.clone(), LISTEN_BOUND)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Event::McpTask(Box::new(Note::Listened {
                        server: name,
                        asked: ids,
                        acked,
                    })));
                });
            if spawned.is_err() {
                self.mcp_tasks.listening.remove(&server);
            }
        }
    }

    /// A poll answered.
    fn mcp_task_polled(
        &mut self,
        h: &str,
        answer: Result<Result<Task, RpcError>, String>,
        declined: Vec<(String, String)>,
        unsupported: Option<String>,
        undeclined: Option<Result<RpcError, String>>,
    ) {
        self.mcp_tasks.polling.remove(h);
        if !self.mcp_tasks.records.contains_key(h) {
            // Delivered, cancelled or dropped while the poll was out.
            return;
        }
        match answer {
            Ok(Ok(task)) => {
                if !declined.is_empty() {
                    let rec = self.mcp_tasks.records.get_mut(h).expect("present");
                    for (key, _) in &declined {
                        rec.tracker.answering(key);
                    }
                    let rec = &self.mcp_tasks.records[h];
                    let reason = match rec.owner {
                        Owner::Step { .. } | Owner::Detached { .. } => {
                            "the supervisor's connections declare no elicitation, and the step sets no `input`"
                        }
                        Owner::Turn { .. } => "the turn that made the call has moved on",
                    };
                    let log = self.log.in_trace(rec.trace.as_ref());
                    for (key, method) in &declined {
                        log.info(
                            "mcp.task.input.requested",
                            json!({"server": rec.server, "task": h, "key": tame(key), "method": method}),
                        );
                        log.warn(
                            "mcp.task.input.declined",
                            json!({"server": rec.server, "task": h, "key": tame(key),
                                   "method": method, "reason": reason}),
                        );
                    }
                }
                if let Some(method) = unsupported {
                    self.mcp_task_lost(h, &::mcp::tasks::Lost::Unsupported(method));
                    return;
                }
                // A decline that did not go is a failed poll: the task is
                // asked again after the pause one gets, not at once.
                let retry = match undeclined {
                    None => None,
                    Some(Ok(e)) if e.code == ::mcp::rpc::INVALID_PARAMS => {
                        self.mcp_task_lost(h, &::mcp::tasks::Lost::Unknown(e.message));
                        return;
                    }
                    Some(Ok(e)) if e.code == ::mcp::rpc::INTERNAL_ERROR => {
                        Some(format!("tasks/update: {} ({})", tame(&e.message), e.code))
                    }
                    Some(Ok(e)) => {
                        self.mcp_task_lost(
                            h,
                            &::mcp::tasks::Lost::Failed(format!(
                                "tasks/update: {} ({})",
                                tame(&e.message),
                                e.code
                            )),
                        );
                        return;
                    }
                    Some(Err(e)) => Some(format!("tasks/update: {}", tame(&e))),
                };
                if retry.is_none() {
                    self.mcp_tasks.failures.remove(h);
                }
                self.mcp_task_observe(h, task, !declined.is_empty());
                if let Some(err) = retry
                    && self.mcp_tasks.records.get(h).is_some_and(Record::open)
                {
                    self.mcp_task_retry(h, &err);
                }
            }
            Ok(Err(e)) if e.code == ::mcp::rpc::INVALID_PARAMS => {
                self.mcp_task_lost(h, &::mcp::tasks::Lost::Unknown(e.message));
            }
            // An internal error is the server's or the transport's, and may
            // pass; so may a request that never got an answer.
            Ok(Err(e)) if e.code == ::mcp::rpc::INTERNAL_ERROR => {
                self.mcp_task_retry(h, &format!("{} ({})", tame(&e.message), e.code));
            }
            Ok(Err(e)) => {
                self.mcp_task_lost(
                    h,
                    &::mcp::tasks::Lost::Failed(format!(
                        "tasks/get: {} ({})",
                        tame(&e.message),
                        e.code
                    )),
                );
            }
            Err(e) => self.mcp_task_retry(h, &e),
        }
    }

    /// A poll failed in a way that may pass: try again after a pause that
    /// doubles from 1 s to 5 minutes.
    fn mcp_task_retry(&mut self, h: &str, err: &str) {
        let n = {
            let f = self.mcp_tasks.failures.entry(h.to_string()).or_default();
            *f += 1;
            *f
        };
        let wait = 1000u64
            .saturating_mul(1 << (n - 1).min(16))
            .min(RETRY_MAX_MS);
        self.mcp_tasks.due.insert(h.to_string(), now_ms() + wait);
        let rec = &self.mcp_tasks.records[h];
        self.log.in_trace(rec.trace.as_ref()).warn(
            "mcp.task.poll.fail",
            json!({"server": rec.server, "task": h, "err": tame(err), "retry_ms": wait}),
        );
    }

    /// The server's newest word on a task: a poll's answer or a push.
    fn mcp_task_observe(&mut self, h: &str, task: Task, answered: bool) {
        let Some(rec) = self.mcp_tasks.records.get_mut(h) else {
            return;
        };
        let seen = rec.tracker.observe(task);
        let pushed = self.mcp_tasks.pushed.contains(h);
        let rec = &self.mcp_tasks.records[h];
        if let Some(from) = seen.moved_from {
            let mut fields = json!({"server": rec.server, "task": h, "from": from.as_str(),
                                    "to": rec.tracker.task.status.as_str()});
            if let Some(m) = &rec.tracker.task.status_message {
                fields["status_message"] = json!(tame(m));
            }
            self.log
                .in_trace(rec.trace.as_ref())
                .info("mcp.task.status", fields);
        }
        if rec.tracker.ended() {
            self.mcp_task_ended(h);
            return;
        }
        // A question a person answers is not the next poll's to answer, so
        // it does not bring that poll forward; their answer does.
        let routed = self.task_input(rec).is_some();
        let next = next_poll_ms(&rec.tracker, !routed && !seen.asks.is_empty(), pushed);
        self.mcp_tasks.due.insert(h.to_string(), now_ms() + next);
        if seen.moved_from.is_some() || answered {
            let _ = self.persist_task(h);
        }
        if routed && !seen.asks.is_empty() {
            self.mcp_task_route_asks(h, seen.asks);
        }
    }

    /// Who a task's question is put to: the `input` of the step that waits
    /// on the task or detached it (RFC 0054). `None` declines it.
    fn task_input(&self, rec: &Record) -> Option<InputRoute> {
        let (run, step) = rec.owner.step()?;
        let input = self.step_field(run, step, "input")?;
        let m = input.as_object()?;
        Some(InputRoute {
            to: m.get("to").cloned().filter(|v| !v.is_null()),
            timeout: m.get("timeout").cloned().filter(|v| !v.is_null()),
        })
    }

    /// Queue a task's new questions for a person, and ask the next.
    fn mcp_task_route_asks(&mut self, h: &str, asks: Vec<Ask>) {
        let Some(rec) = self.mcp_tasks.records.get(h) else {
            return;
        };
        let log = self.log.in_trace(rec.trace.as_ref());
        let server = rec.server.clone();
        let round = self.mcp_tasks.rounds.entry(h.to_string()).or_default();
        for ask in asks {
            if round.holds(ask.key()) {
                continue;
            }
            log.info(
                "mcp.task.input.requested",
                json!({"server": server, "task": h, "key": tame(ask.key()), "method": ask.method()}),
            );
            round.queue.push_back(ask);
        }
        self.mcp_task_ask_next(h);
    }

    /// Ask the round's next question, or — every key answered — hand the
    /// answers to the next poll, which sends them in one `tasks/update`.
    fn mcp_task_ask_next(&mut self, h: &str) {
        loop {
            let Some(round) = self.mcp_tasks.rounds.get_mut(h) else {
                return;
            };
            if round.asking.is_some() {
                return;
            }
            match round.queue.pop_front() {
                // Nothing a person could type: declined, as directly.
                Some(Ask::Decline { key, method }) => {
                    round
                        .given
                        .insert(key.clone(), json!({"action": "decline"}));
                    if let Some(rec) = self.mcp_tasks.records.get(h) {
                        self.log.in_trace(rec.trace.as_ref()).warn(
                            "mcp.task.input.declined",
                            json!({"server": rec.server, "task": h, "key": tame(&key),
                                   "method": method, "reason": "a URL elicitation has nothing a person could type"}),
                        );
                    }
                }
                Some(Ask::Unsupported { method, .. }) => {
                    if let Some(rec) = self.mcp_tasks.records.get(h) {
                        let (server, id, trace) = (
                            rec.server.clone(),
                            rec.tracker.task.task_id.clone(),
                            rec.trace.clone(),
                        );
                        self.cancel_task_on(&server, &id, trace.as_ref());
                    }
                    self.mcp_task_lost(h, &::mcp::tasks::Lost::Unsupported(method));
                    return;
                }
                Some(Ask::Elicit {
                    key,
                    message,
                    requested_schema,
                }) => {
                    round.asking = Some((key.clone(), requested_schema.clone()));
                    self.mcp_task_put(h, &key, &message, requested_schema);
                    return;
                }
                None => {
                    let given = std::mem::take(&mut round.given);
                    self.mcp_tasks.rounds.remove(h);
                    if given.is_empty() {
                        return;
                    }
                    if let Some(rec) = self.mcp_tasks.records.get_mut(h) {
                        for (key, response) in given {
                            rec.tracker.answer(&key, response);
                        }
                    }
                    let soon = now_ms() + ::mcp::tasks::POLL_FLOOR.as_millis() as u64;
                    self.mcp_tasks.due.insert(h.to_string(), soon);
                    return;
                }
            }
        }
    }

    /// Put one question to a person, through the gate a `human` node opens:
    /// the same channel, ownership and fallback, `to` and `timeout` from the
    /// step's `input`, and the request's schema as the answer's. Over A2A it
    /// opens on a task of its own ([`super::human::GateOn::McpQuestion`]).
    fn mcp_task_put(&mut self, h: &str, key: &str, message: &str, schema: Value) {
        let Some(rec) = self.mcp_tasks.records.get(h).cloned() else {
            return;
        };
        let (Some(route), Some((run, step))) = (self.task_input(&rec), rec.owner.step()) else {
            return self.mcp_task_answered(h, key, Value::Null, true);
        };
        let (run, step) = (run.to_string(), step.to_string());
        let mut args = json!({
            "question": format!(
                "The MCP server {:?} asks, for its tool {:?} (task {h}): {message}",
                rec.server, rec.tool
            ),
            "schema": schema,
        });
        if let Some(to) = route.to {
            args["to"] = to;
        }
        let asked = route
            .timeout
            .as_ref()
            .and_then(crate::engine::model::duration_ms_opt);
        // A step that waits on its own task ends it at the step's bound —
        // its `timeout`, an `mcp.tool` step's `limits.step_timeout` by
        // default — and the question with it: the gate shows that deadline
        // when it comes first, never one the step will not wait for.
        let left = match &rec.owner {
            Owner::Step { .. } => self
                .runs
                .get(&run)
                .and_then(|r| r.steps.get(&step))
                .and_then(|s| s.wait.as_ref())
                .and_then(|w| w.get("deadline_ms"))
                .and_then(Value::as_u64)
                .map(|d| d.saturating_sub(now_ms()).max(1)),
            _ => None,
        };
        let ms = match left {
            Some(left) => Some(asked.unwrap_or(super::human::ASK_TIMEOUT_MS).min(left)),
            None => asked,
        };
        // The gate reads a duration's text; the load took any form of one.
        if let Some(ms) = ms {
            args["timeout"] = json!(format!("{ms}ms"));
        }
        let r = self.runs.get(&run);
        let caller = super::tools::ToolCaller {
            principal: r.and_then(|r| r.principal.clone()),
            ctx: r.and_then(|r| r.conversation.clone()),
            run: Some(run.clone()),
            step: Some(step.clone()),
            session: r.and_then(|r| r.session.clone()),
            ..Default::default()
        };
        self.log.in_trace(rec.trace.as_ref()).info(
            "mcp.task.input.asked",
            json!({"server": rec.server, "task": h, "key": tame(key), "run": run, "step": step}),
        );
        match self.ask_human(&caller, args, Some(h)) {
            ToolOutcome::Deferred(kind) => self.push_pending(super::reactor::PendingTool {
                target: Target::McpTask(h.to_string(), key.to_string()),
                name: "mcp.task.input".into(),
                kind,
                started_ms: now_ms(),
            }),
            // An answer the approval policy gave at once.
            ToolOutcome::Ready(v, false) => {
                let reply = v.get("reply").cloned().unwrap_or(Value::Null);
                self.mcp_task_answered(h, key, reply, false)
            }
            // Nobody could be asked.
            ToolOutcome::Ready(why, true) => self.mcp_task_answered(h, key, why, true),
            ToolOutcome::Executing => self.mcp_task_answered(h, key, Value::Null, true),
        }
    }

    /// A person's answer to a task's question — or its gate's failure, which
    /// answers `cancel` — joins the round, and the next key is asked.
    pub(crate) fn mcp_task_answered(&mut self, h: &str, key: &str, result: Value, is_error: bool) {
        let Some(round) = self.mcp_tasks.rounds.get_mut(h) else {
            return;
        };
        let schema = match round.asking.take() {
            Some((asked, schema)) if asked == key => schema,
            other => {
                round.asking = other;
                return;
            }
        };
        let answer = (!is_error).then(|| crate::mcp::elicit::shape_reply(&result, &schema));
        let response = ::mcp::tasks::elicit_result(answer.as_ref());
        let action = response["action"].as_str().unwrap_or("cancel").to_string();
        round.given.insert(key.to_string(), response);
        if let Some(rec) = self.mcp_tasks.records.get(h) {
            let mut fields =
                json!({"server": rec.server, "task": h, "key": tame(key), "action": action});
            if is_error && let Some(why) = result.as_str() {
                fields["reason"] = json!(tame(why));
            }
            self.log
                .in_trace(rec.trace.as_ref())
                .info("mcp.task.input.answered", fields);
        }
        self.mcp_task_ask_next(h);
    }

    /// The server took the answers to `sent`: their keys are answered for
    /// good, and kept as such.
    fn mcp_task_sent(&mut self, h: &str, sent: &[String]) {
        let Some(rec) = self.mcp_tasks.records.get_mut(h) else {
            return;
        };
        if sent.is_empty() {
            return;
        }
        for key in sent {
            rec.tracker.unsent.remove(key);
        }
        let _ = self.persist_task(h);
    }

    /// A pushed `notifications/tasks`: applied as the poll it equals.
    pub(crate) fn mcp_task_pushed(&mut self, server: &str, params: &Value) {
        let Ok(task) = Task::read(params) else {
            return;
        };
        let h = handle(server, &task.task_id);
        if self.mcp_tasks.records.contains_key(&h) {
            self.mcp_task_observe(&h, task, false);
        }
    }

    /// A task reached a final status: hand its outcome to whoever waits.
    fn mcp_task_ended(&mut self, h: &str) {
        let Some(rec) = self.mcp_tasks.records.get(h).cloned() else {
            return;
        };
        let status = rec.tracker.task.status;
        let log = self.log.in_trace(rec.trace.as_ref());
        match status {
            TaskStatus::Completed => {
                let is_error = rec
                    .tracker
                    .task
                    .tool_result()
                    .and_then(Result::ok)
                    .is_some_and(|r| r.is_error());
                log.info(
                    "mcp.task.completed",
                    json!({"server": rec.server, "task": h, "is_error": is_error}),
                );
            }
            TaskStatus::Failed => log.warn(
                "mcp.task.failed",
                json!({"server": rec.server, "task": h,
                       "code": rec.tracker.task.error.as_ref().map(|e| e["code"].clone())}),
            ),
            _ => log.info(
                "mcp.task.cancelled",
                json!({"server": rec.server, "task": h, "reason": "the server cancelled it"}),
            ),
        }
        crate::obs::metrics::record_mcp_task(&rec.server, outcome_label(&rec));
        self.mcp_task_deliver(h);
    }

    /// Following a task stopped short of its end: say why, and hand that to
    /// whoever waits.
    fn mcp_task_lost(&mut self, h: &str, lost: &::mcp::tasks::Lost) {
        let Some(rec) = self.mcp_tasks.records.get_mut(h) else {
            return;
        };
        let (reason, said) = lost_said(lost);
        rec.lost = Some(said.clone());
        let rec = rec.clone();
        // Given up while the server may still run it: it is told so. One it
        // does not know is not, and one that asked for what agentd never
        // declares was cancelled when it asked.
        if matches!(
            lost,
            ::mcp::tasks::Lost::Expired | ::mcp::tasks::Lost::Failed(_)
        ) {
            self.cancel_task_on(&rec.server, &rec.tracker.task.task_id, rec.trace.as_ref());
        }
        let log = self.log.in_trace(rec.trace.as_ref());
        match lost {
            ::mcp::tasks::Lost::Unknown(_) | ::mcp::tasks::Lost::Expired => log.warn(
                "mcp.task.expired",
                json!({"server": rec.server, "task": h, "reason": reason}),
            ),
            _ => log.warn(
                "mcp.task.failed",
                json!({"server": rec.server, "task": h, "reason": reason, "detail": said}),
            ),
        }
        crate::obs::metrics::record_mcp_task(
            &rec.server,
            match lost {
                ::mcp::tasks::Lost::Unknown(_) | ::mcp::tasks::Lost::Expired => "expired",
                _ => "failed",
            },
        );
        self.mcp_task_deliver(h);
    }

    /// Hand an ended task's outcome over, once: a step is finished with it
    /// and the record goes; a turn's is kept for `mcp.task.get`, and a
    /// detached one for its run's waits, which the next pass of
    /// `poll_waits` resolves.
    fn mcp_task_deliver(&mut self, h: &str) {
        let Some(rec) = self.mcp_tasks.records.get(h).cloned() else {
            return;
        };
        match &rec.owner {
            Owner::Step {
                run,
                step,
                registry,
            } => {
                self.drop_task(h);
                let waits = self
                    .runs
                    .get(run)
                    .and_then(|r| r.steps.get(step))
                    .is_some_and(|s| {
                        s.status == StepStatus::Suspended
                            && s.wait
                                .as_ref()
                                .is_some_and(|w| w["task"].as_str() == Some(h))
                    });
                if waits {
                    let (output, is_error, error) = step_outcome(&rec, *registry);
                    self.finish_step_pub(
                        run,
                        step,
                        if is_error {
                            StepStatus::Failed
                        } else {
                            StepStatus::Done
                        },
                        Some(output),
                        error,
                        0,
                    );
                }
            }
            Owner::Turn { .. } | Owner::Detached { .. } => {
                if let Some(r) = self.mcp_tasks.records.get_mut(h) {
                    r.ended_ms = Some(now_ms());
                }
                self.mcp_tasks.due.remove(h);
                let _ = self.persist_task(h);
                self.mcp_task_gauge();
                self.close_task_questions(h);
            }
        }
    }

    /// What a `wait {on: mcp_task, task}` of run `run` finds (RFC 0054):
    /// `Ok(Some(outcome))` once the task ended, `Ok(None)` while it is open,
    /// or why the wait cannot wait on it — no task of that handle was
    /// detached in this run.
    pub(crate) fn detached_outcome(&self, run: &str, h: &str) -> Result<Option<Value>, String> {
        match self.mcp_tasks.records.get(h) {
            Some(rec) if matches!(&rec.owner, Owner::Detached { run: r, .. } if r == run) => {
                Ok((!rec.open()).then(|| waited_outcome(rec)))
            }
            _ if h.is_empty() => Err(
                "no task to wait on: `task` is empty (a detaching step the server answered at once made none; its output is the answer)".into(),
            ),
            _ => Err(format!("no task {h:?} was detached in this run")),
        }
    }

    /// Send `tasks/cancel` for `task_id` on `server`, off the loop. Its
    /// answer is not waited for: cancelling is cooperative, and the record is
    /// already gone.
    fn cancel_task_on(&self, server: &str, task_id: &str, trace: Option<&TraceContext>) {
        let Some(client) = self.mcp.get(server).cloned() else {
            return;
        };
        let (task_id, call) = (task_id.to_string(), trace.map(TraceContext::child));
        let (server, h) = (server.to_string(), handle(server, task_id.as_str()));
        let cancels = Arc::clone(&self.mcp_tasks.cancels);
        cancels.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name(format!("mcp.task.cancel:{server}"))
            .spawn({
                let cancels = Arc::clone(&cancels);
                move || {
                    let start = crate::obs::otel::now_unix_nanos();
                    let meta = call.as_ref().map(TraceContext::meta);
                    let sent = client.cancel_task(&task_id, meta, ::mcp::tasks::REQUEST_BOUND);
                    crate::obs::otel::mcp_span(
                        call.as_ref(),
                        ::mcp::wire::method::TASKS_CANCEL,
                        &h,
                        &server,
                        matches!(sent, Ok(Ok(()))),
                        start,
                    );
                    cancels.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        if spawned.is_err() {
            cancels.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// How a poll's thread treats a task's questions (RFC 0054).
struct Asks {
    /// A person answers them: the thread declines none.
    routed: bool,
    /// The answers given and not yet taken by the server, sent first.
    answers: Map<String, Value>,
}

/// One poll, on its own thread: the answers a person gave sent in one
/// `tasks/update`, then `tasks/get`; and when the task asks for input that
/// nobody is to answer, every key not answered yet answered `decline` in one
/// `tasks/update` — a server may refuse a partial set — or, for a request
/// agentd never declares, the task cancelled. Each request is a span under
/// the call's.
fn poll(
    client: &Arc<::mcp::client::McpClient>,
    server: &str,
    h: &str,
    task_id: &str,
    answered: &BTreeSet<String>,
    asks: Asks,
    call: Option<&TraceContext>,
) -> Note {
    use ::mcp::wire::method;
    let meta = call.map(TraceContext::meta);
    let span = |m: &str, ok: bool, start: u128| {
        crate::obs::otel::mcp_span(call, m, h, server, ok, start);
    };
    let mut sent = Vec::new();
    let mut undeclined = None;
    if !asks.answers.is_empty() {
        let keys: Vec<String> = asks.answers.keys().cloned().collect();
        let start = crate::obs::otel::now_unix_nanos();
        let r = client.update_task(
            task_id,
            asks.answers,
            meta.clone(),
            ::mcp::tasks::REQUEST_BOUND,
        );
        span(method::TASKS_UPDATE, matches!(r, Ok(Ok(()))), start);
        match r {
            Ok(Ok(())) => sent = keys,
            Ok(Err(e)) => undeclined = Some(Ok(e)),
            Err(e) => undeclined = Some(Err(e.to_string())),
        }
    }
    let start = crate::obs::otel::now_unix_nanos();
    let got = client.get_task(task_id, meta.clone(), ::mcp::tasks::REQUEST_BOUND);
    span(method::TASKS_GET, matches!(got, Ok(Ok(_))), start);
    let mut declined = Vec::new();
    let mut unsupported = None;
    if let Ok(Ok(task)) = &got
        && task.status == TaskStatus::InputRequired
        && let Some(requests) = &task.input_requests
    {
        let mut decline = serde_json::Map::new();
        let mut asked = Vec::new();
        for (key, request) in requests {
            if answered.contains(key) {
                continue;
            }
            match ::mcp::tasks::ask_of(key, request) {
                Ask::Unsupported { method, .. } => {
                    let start = crate::obs::otel::now_unix_nanos();
                    let sent =
                        client.cancel_task(task_id, meta.clone(), ::mcp::tasks::REQUEST_BOUND);
                    span(method::TASKS_CANCEL, matches!(sent, Ok(Ok(()))), start);
                    unsupported = Some(method);
                    break;
                }
                // The loop puts it to a person.
                _ if asks.routed => {}
                ask => {
                    decline.insert(key.clone(), json!({"action": "decline"}));
                    asked.push((key.clone(), ask.method().to_string()));
                }
            }
        }
        if unsupported.is_none() && !decline.is_empty() {
            let start = crate::obs::otel::now_unix_nanos();
            let sent =
                client.update_task(task_id, decline, meta.clone(), ::mcp::tasks::REQUEST_BOUND);
            span(method::TASKS_UPDATE, matches!(sent, Ok(Ok(()))), start);
            match sent {
                Ok(Ok(())) => declined = asked,
                Ok(Err(e)) => undeclined = Some(Ok(e)),
                Err(e) => undeclined = Some(Err(e.to_string())),
            }
        }
    }
    Note::Polled {
        handle: h.to_string(),
        answer: got.map_err(|e| e.to_string()),
        declined,
        unsupported,
        undeclined,
        sent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_names_the_task_without_its_id() {
        let h = handle("builds", "786512e2-9e0d-44bd-8f29-789f320fe840");
        assert!(h.starts_with("mt_") && h.len() == 19, "{h}");
        assert!(!h.contains("786512e2"));
        // The same task on another server is another task.
        assert_ne!(h, handle("deploys", "786512e2-9e0d-44bd-8f29-789f320fe840"));
        assert_eq!(h, handle("builds", "786512e2-9e0d-44bd-8f29-789f320fe840"));
    }

    fn rec(status: &str, extra: Value) -> Record {
        let mut t = json!({"taskId": "t", "status": status, "createdAt": "", "lastUpdatedAt": "", "ttlMs": null});
        if let (Some(o), Value::Object(e)) = (t.as_object_mut(), extra) {
            o.extend(e);
        }
        Record {
            handle: "mt_x".into(),
            server: "s".into(),
            tool: "build".into(),
            tracker: Tracker::new(Task::read(&t).unwrap(), 0),
            owner: Owner::Step {
                run: "r".into(),
                step: "a".into(),
                registry: false,
            },
            trace: None,
            ended_ms: None,
            lost: None,
        }
    }

    #[test]
    fn a_steps_outcome_is_the_calls_answer() {
        let ok = rec(
            "completed",
            json!({"result": {"content": [{"type": "text", "text": "{\"n\": 3}"}]}}),
        );
        assert_eq!(step_outcome(&ok, false), (json!({"n": 3}), false, None));
        let tool_error = rec(
            "completed",
            json!({"result": {"content": [{"type": "text", "text": "no"}], "isError": true}}),
        );
        assert_eq!(
            step_outcome(&tool_error, false),
            (json!("no"), true, Some("no".into()))
        );
        // A `tool` step's error is the result as the registry path gives it.
        assert_eq!(
            step_outcome(&tool_error, true),
            (json!("no"), true, Some("\"no\"".into()))
        );
        let failed = rec(
            "failed",
            json!({"error": {"code": -32603, "message": "rate limited"}}),
        );
        let (v, e, msg) = step_outcome(&failed, false);
        assert!(v.is_null() && e);
        assert!(msg.unwrap().contains("rate limited"));
        let mut lost = rec("working", json!({}));
        lost.lost = Some("the task was still open past its ttlMs".into());
        let (_, e, msg) = step_outcome(&lost, false);
        assert!(e && msg.unwrap().contains("past its ttlMs"));
        assert_eq!(outcome_label(&lost), "expired");
        assert_eq!(outcome_label(&ok), "completed");
        assert_eq!(outcome_label(&failed), "failed");
    }

    #[test]
    fn a_task_is_never_polled_below_the_floor() {
        let floor = ::mcp::tasks::POLL_FLOOR.as_millis() as u64;
        let mut working = rec("working", json!({"pollIntervalMs": 1}));
        assert_eq!(next_poll_ms(&working.tracker, false, false), floor);
        working.tracker.task.poll_interval_ms = Some(4000);
        assert_eq!(next_poll_ms(&working.tracker, false, false), 4000);
        // A question no poll answered yet is answered on the next one, at
        // the floor: never at once, which would spin.
        assert_eq!(next_poll_ms(&working.tracker, true, false), floor);
        assert_eq!(next_poll_ms(&working.tracker, true, true), floor);
        assert_eq!(
            next_poll_ms(&working.tracker, false, true),
            ::mcp::tasks::POLL_CEILING.as_millis() as u64
        );
    }

    #[test]
    fn a_servers_text_is_tamed_for_a_log_line() {
        assert_eq!(tame("ok\u{1b}[2J\n"), "ok[2J");
        assert_eq!(tame(&"é".repeat(200)).len(), 200);
    }
}
