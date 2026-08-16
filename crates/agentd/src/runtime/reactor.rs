// SPDX-License-Identifier: AGPL-3.0-only
//! The **runtime state + event loop** (RFC 0026 §3, §8): one single-threaded
//! reactor over child frames, reaped children, executor results, timers, the
//! durable inbox and signals; state mutation happens only here (single
//! writer); every mutation is followed by a checkpoint decision (RFC 0025 §5).
//! The other `runtime::*` modules add `impl Runtime` blocks for turns, tools,
//! steps and subagents; this file owns construction, the loop, lifecycle and
//! the status view.

use super::artifacts::Artifacts;
use super::children::{ChildKind, Children};
use super::events::{Event, kinds};
use super::timers::Timers;
use crate::config::v2::{RunUntil, Settings};
use crate::context::memory::Memory;
use crate::context::{Contexts, skills, tokens};
use crate::engine::{RunState, RunStatus, Workflow};
use crate::governor::Governor;
use crate::mcp::client::McpClient;
use crate::obs::log::Logger;
use crate::registry::Registry;
use crate::state::{Durable, InboxEvent, Kind, now_ms};
use crate::subagent::protocol::AgentMsg;
use crate::supervisor::reap::Reaped;
use crate::supervisor::tree::NodeId;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// The reactor tick.
pub const TICK: Duration = Duration::from_millis(200);
/// Extra grace after the drain deadline before children are abandoned.
pub const ABANDON_GRACE: Duration = Duration::from_secs(3);

/// Who receives a deferred tool's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A child's `ToolRequest` (answered with `ToolResult`).
    Child(NodeId, u64),
    /// A workflow step (answered as the step's outcome).
    Step(String, String),
}

/// A deferred internal-tool request (answered when its wait resolves).
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub target: Target,
    pub name: String,
    pub kind: PendingKind,
    pub started_ms: u64,
}

#[derive(Debug, Clone)]
pub enum PendingKind {
    /// A durable timer (`sleep`).
    Timer { id: String },
    /// A subagent result (`subagent.run` sync / `subagent.await`).
    Subagent { handle: String },
    /// A think child (`think` tool / `context.compact`).
    Think { child: NodeId },
    /// A run's terminal state (`workflow.run wait` / `workflow.wait`).
    Run { run: String, deadline_ms: u64 },
    /// A CEL condition polled each tick (`await`).
    Await { condition: String, deadline_ms: u64 },
    /// A human's answer (`ask_human` / the `human` node — RFC 0032 §16): the
    /// A2A task `task` sits in `input-required`; a `SendMessage` carrying its
    /// `taskId` resolves this with the reply text. With no interface to answer
    /// on, `task` is a synthetic ask id (no A2A task exists).
    Human {
        task: String,
        question: String,
        deadline_ms: u64,
        /// The task exists ONLY for this ask (no A2A caller/run owns it) —
        /// complete it when the answer lands.
        standalone: bool,
        /// The `auto` fallback judge is running (or already ran) for this ask.
        auto_fired: bool,
    },
}

/// A queued root/conversation turn (RFC 0026 §3.2), waiting for a slot and
/// for its context to be free.
#[derive(Debug, Clone)]
pub struct TurnJob {
    pub ctx: String,
    /// The triggering inbox event (marked done when the turn completes).
    pub event: Option<String>,
    pub principal: Option<String>,
    /// The message appended to the context before the turn (already appended
    /// when `None`).
    pub message: Option<crate::context::Msg>,
    /// Skill references to preload.
    pub skills: Vec<String>,
    /// The user text (for preflight / knowledge retrieval).
    pub text: String,
    /// Preflight ran (or was not needed).
    pub preflight_done: bool,
    /// Knowledge auto-context ran (or was not needed).
    pub knowledge_done: bool,
    /// The retrieved knowledge block (system message) for this turn.
    pub knowledge: Option<String>,
}

impl TurnJob {
    pub fn new(
        ctx: String,
        event: Option<String>,
        principal: Option<String>,
        message: Option<crate::context::Msg>,
        skills: Vec<String>,
        text: String,
    ) -> TurnJob {
        TurnJob {
            ctx,
            event,
            principal,
            message,
            skills,
            text,
            preflight_done: false,
            knowledge_done: false,
            knowledge: None,
        }
    }
}

/// A subagent registry record (RFC 0026 §6; durable `subagent/<handle>`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentRecord {
    pub handle: String,
    pub instruction: String,
    pub mode: String,
    pub status: String,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<Value>,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    /// The payload (secret-free) for restore re-spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(skip)]
    pub node: Option<NodeId>,
    #[serde(skip)]
    pub dirty: bool,
}

/// The current instruction (RFC 0028 §3 `instruction.*`).
#[derive(Debug, Clone)]
pub struct Instruction {
    pub text: String,
    pub source: &'static str,
    pub uri: Option<String>,
    pub server: Option<String>,
    pub version: u64,
}

/// Counters for status/reports.
#[derive(Debug, Default, Clone)]
pub struct Counters {
    pub turns: u64,
    pub tool_calls: u64,
    pub runs_started: u64,
    pub runs_finished: u64,
    pub inbox_processed: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

pub struct Runtime {
    pub(crate) settings: Settings,
    /// The merged document the settings came from (restart-only diff base).
    pub(crate) settings_doc: Value,
    /// The invocation (for reload).
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    /// Workflow definitions pinned by live runs after a reload (hash → definition).
    pub(crate) pinned: BTreeMap<String, Workflow>,
    /// The last payload per signal name (for `await`/`wait condition` views).
    pub(crate) recent_signals: BTreeMap<String, Value>,
    pub(crate) log: Logger,
    pub(crate) instance: String,
    pub(crate) run_id: String,
    pub(crate) durable: Durable,
    pub(crate) mcp: BTreeMap<String, Arc<McpClient>>,
    pub(crate) mcp_specs: BTreeMap<String, crate::config::McpServerSpec>,
    pub(crate) registry: Registry,
    pub(crate) contexts: Contexts,
    pub(crate) memory: Memory,
    pub(crate) artifacts: Artifacts,
    pub(crate) skills: skills::Catalogue,
    pub(crate) governor: Governor,
    pub(crate) workflows: BTreeMap<String, Workflow>,
    pub(crate) runs: BTreeMap<String, RunState>,
    pub(crate) children: Children,
    pub(crate) timers: Timers,
    pub(crate) events_rx: Receiver<Event>,
    pub(crate) events_tx: Sender<Event>,
    pub(crate) child_rx: Receiver<(NodeId, AgentMsg)>,
    pub(crate) reap_rx: Receiver<Reaped>,
    pub(crate) pending: Vec<PendingTool>,
    pub(crate) turn_queue: VecDeque<TurnJob>,
    /// Turn jobs parked while their preflight think / knowledge retrieval runs.
    pub(crate) staged_turns: BTreeMap<u64, TurnJob>,
    pub(crate) inbox_queue: VecDeque<InboxEvent>,
    pub(crate) subagents: BTreeMap<String, SubagentRecord>,
    pub(crate) instruction: Instruction,
    pub(crate) job_shape: bool,
    pub(crate) exit: Option<i32>,
    pub(crate) draining: bool,
    /// Operator-held (a2a.pause): intake continues; no new turns dispatch and
    /// no steps schedule until a2a.resume. Reversible, unlike drain.
    pub(crate) paused: bool,
    pub(crate) drain_started: Option<Instant>,
    pub(crate) drain_reason: String,
    pub(crate) idle_since: Option<Instant>,
    pub(crate) intel_uri: String,
    pub(crate) intel_token: Option<String>,
    /// Resolved `intelligence.headers` (RFC 0031), pushed on every LLM dial and
    /// threaded to subagents via the spawn payload.
    pub(crate) intel_headers: Vec<(String, String)>,
    /// An optional intelligence OAuth credential provider (RFC 0031): a closure
    /// returning the current bearer (refreshing from the device-login cache). The
    /// resolved bearer overrides `intel_token` and is threaded to subagents fresh
    /// at each spawn. `None` when no `intelligence.auth` oauth2 block is set.
    pub(crate) intel_bearer: Option<std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    pub(crate) model: String,
    pub(crate) trace_id: Option<String>,
    pub(crate) started: Instant,
    pub(crate) seq: u64,
    pub(crate) counters: Counters,
    /// The `once`-started run(s) whose finish decides a job's exit code.
    pub(crate) job_runs: Vec<String>,
    /// Steps executing on executor threads (`run/step` → started).
    pub(crate) executing: BTreeMap<String, Instant>,
    pub(crate) last_manifest_flush: Instant,
    /// Unix-ms a goal LLM judge was dispatched (so overlapping checks don't spawn
    /// duplicate judges); `None` = none in flight.
    pub(crate) goal_judge_at: Option<u64>,
    /// Durable A2A tasks (RFC 0029 §4), keyed by task id.
    #[cfg(feature = "a2a")]
    pub(crate) tasks: BTreeMap<String, crate::a2a::Task>,
    /// Inbox-event id → the A2A task it answers (a conversation turn).
    #[cfg(feature = "a2a")]
    pub(crate) event_to_task: BTreeMap<String, String>,
    /// The task snapshot the A2A listener threads read (None ⇒ not serving).
    #[cfg(feature = "a2a")]
    pub(crate) a2a_shared: Option<std::sync::Arc<super::a2a_server::SharedTasks>>,
    /// The interface event feed (RFC 0032; None ⇒ interface disabled).
    #[cfg(feature = "a2a")]
    pub(crate) a2a_feed: Option<std::sync::Arc<super::a2a_server::SharedFeed>>,
    /// Pairing-code login (RFC 0032 §13; None ⇒ pairing disabled).
    #[cfg(feature = "a2a")]
    pub(crate) a2a_pairing: Option<std::sync::Arc<super::a2a_server::PairingState>>,
    /// Per-item fingerprints behind the feed's section diffing (`feed_tick`).
    #[cfg(feature = "a2a")]
    pub(crate) feed_marks: BTreeMap<String, u64>,
    /// The last section-diff pass (rate-limits `feed_tick`).
    #[cfg(feature = "a2a")]
    pub(crate) feed_last: Instant,
    /// The `wait: {on: webhook}` await-callback registry, shared with the webhook
    /// listener threads.
    #[cfg(feature = "a2a")]
    pub(crate) webhook_callbacks: super::webhooks::SharedCallbacks,
    /// Pending `respond: sync` webhook replies, keyed by the run id they await.
    #[cfg(feature = "a2a")]
    pub(crate) webhook_sync: std::collections::HashMap<
        String,
        std::sync::mpsc::SyncSender<super::webhooks::WebhookReply>,
    >,
}

impl Runtime {
    /// A fresh id (turn ids, handles).
    pub(crate) fn next_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}-{}", self.seq)
    }

    // ---- the loop ----------------------------------------------------------

    /// Run until exit. Returns the process exit code.
    pub fn run_loop(&mut self) -> i32 {
        self.log.info("proc.ready", json!({"instance": self.instance, "job_shape": self.job_shape, "workflows": self.workflows.len(), "runs": self.runs.len(), "inbox_pending": self.inbox_queue.len()}));
        loop {
            crate::obs::health::tick();
            // 1. Child frames.
            while let Ok((node, msg)) = self.child_rx.try_recv() {
                self.on_child_frame(node, msg);
            }
            // 2. Reaped children.
            let _ = crate::signals::take_child_exit();
            crate::supervisor::reaper::reap_and_dispatch();
            while let Ok(r) = self.reap_rx.try_recv() {
                self.on_reaped(r);
            }
            // 3. Executor / internal events.
            while let Ok(ev) = self.events_rx.try_recv() {
                self.on_event(ev);
            }
            // 4. Timers.
            let now = now_ms();
            for t in self.timers.fire(&self.durable, now) {
                self.on_timer(t);
            }
            // 5. The inbox.
            self.process_inbox();
            // 6. Start nodes + runs (+ suspended waits).
            self.poll_starts();
            self.poll_waits();
            self.schedule_runs();
            // 7. Turns.
            self.dispatch_turns();
            // 8. Pending waits + MCP notifications.
            self.poll_pending();
            self.poll_mcp_notifications();
            // 9. Children maintenance.
            for (node, health) in self.children.tick() {
                self.on_unhealthy_child(node, health);
            }
            // 10. Checkpoints + the point-in-time observability gauges (§3.11).
            self.checkpoint(false);
            crate::obs::metrics::set_inbox_pending(self.inbox_queue.len() as u64);
            crate::obs::metrics::set_context_tokens(self.contexts.max_est_tokens());
            // 10.5. The interface feed's section diff (RFC 0032 §4): publish
            // run/conversation/subagent/child/status deltas to attached display
            // clients. A no-op unless `interface.enabled`; rate-limited inside.
            #[cfg(feature = "a2a")]
            self.feed_tick();
            // 11. Signals + lifecycle.
            self.check_signals();
            if let Some(code) = self.lifecycle_step() {
                self.shutdown(code);
                return code;
            }
            // 12. Wait for the next event, bounded by the tick or the nearest
            // imminent deadline (a timer, a schedule/loop start, a pending wait)
            // so time-based work fires promptly rather than at tick granularity.
            crate::signals::drain_wakeup();
            let wait = self.next_wake().min(TICK);
            match self.events_rx.recv_timeout(wait) {
                Ok(ev) => self.on_event(ev),
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Child(node, msg) => self.on_child_frame(node, msg),
            Event::Reaped(r) => self.on_reaped(r),
            Event::StepDone {
                run,
                step,
                output,
                is_error,
                error,
                tokens,
            } => self.on_step_done(&run, &step, output, is_error, error, tokens),
            Event::ToolDone {
                node,
                req,
                result,
                is_error,
            } => self.on_tool_done(node, req, result, is_error),
            Event::KnowledgeDone { job, block } => self.on_knowledge_done(job, block),
            Event::TimerFired { id, owner, payload } => self.on_timer(crate::state::TimerRecord {
                id,
                deadline_ms: now_ms(),
                owner,
                payload,
            }),
            Event::Inbox(ev) => self.inbox_queue.push_back(ev),
            #[cfg(feature = "a2a")]
            Event::A2a(req) => self.on_a2a_request(*req),
            #[cfg(feature = "a2a")]
            Event::Webhook(req) => self.on_webhook_request(*req),
            Event::Background { id, result } if id == "goal.judge" => self.on_goal_judge(&result),
            Event::Background { id, result } if id.starts_with("human.judge:") => {
                let ask = id.trim_start_matches("human.judge:").to_string();
                self.on_human_judge(&ask, &result);
            }
            Event::Background { .. } | Event::Tick => {}
        }
    }

    // ---- inbox -------------------------------------------------------------

    /// Accept a durable event: write-ahead, then queue (RFC 0025 §5).
    pub(crate) fn accept_event(
        &mut self,
        kind: &str,
        principal: Option<String>,
        payload: Value,
    ) -> Result<String, String> {
        let ev = InboxEvent::new(kind, principal, payload);
        self.durable
            .inbox_put(&ev)
            .map_err(|e| format!("inbox: {e}"))?;
        let id = ev.id.clone();
        self.log
            .info("inbox.accepted", json!({"inbox_event": id, "kind": kind}));
        self.inbox_queue.push_back(ev);
        Ok(id)
    }

    fn process_inbox(&mut self) {
        while let Some(ev) = self.inbox_queue.pop_front() {
            if self.draining {
                // Keep it durable for the next life; stop intake.
                self.inbox_queue.push_front(ev);
                return;
            }
            self.counters.inbox_processed += 1;
            match ev.kind.as_str() {
                kinds::START_FIRED | kinds::WORKFLOW_RUN => {
                    let done = self.on_start_event(&ev);
                    if done {
                        self.inbox_done(&ev.id);
                    }
                }
                kinds::A2A_MESSAGE => {
                    // P5 wires the A2A server; a replayed message still becomes a turn.
                    self.on_a2a_message_event(&ev);
                }
                kinds::SIGNAL => {
                    let name = ev.payload["name"].as_str().unwrap_or("").to_string();
                    let payload = ev.payload.get("payload").cloned().unwrap_or(Value::Null);
                    let target = ev
                        .payload
                        .get("run")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let from = ev
                        .payload
                        .get("from")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let delivered =
                        self.deliver_signal(&name, payload, target.as_deref(), from.as_deref());
                    self.log.info(
                        "signal.received",
                        json!({"inbox_event": ev.id, "name": name, "delivered": delivered}),
                    );
                    self.inbox_done(&ev.id);
                }
                other => {
                    self.log.warn(
                        "inbox.unknown_kind",
                        json!({"inbox_event": ev.id, "kind": other}),
                    );
                    self.inbox_done(&ev.id);
                }
            }
        }
    }

    pub(crate) fn inbox_done(&mut self, id: &str) {
        if let Err(e) = self.durable.inbox_done(id) {
            self.log.warn(
                "inbox.done.fail",
                json!({"inbox_event": id, "err": e.to_string()}),
            );
        }
    }

    /// An A2A message event → a conversation turn (RFC 0026 §3.2). P5 adds
    /// commands/authorization; here every message is natural language.
    fn on_a2a_message_event(&mut self, ev: &InboxEvent) {
        let ctx = ev.payload["context_id"]
            .as_str()
            .unwrap_or("default")
            .to_string();
        let text = ev.payload["text"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| ev.payload["parts"].to_string());
        let principal = ev.principal.clone();
        // Re-link a replayed message to its durable task (crash recovery).
        #[cfg(feature = "a2a")]
        if let Some(task_id) = ev.payload["task"].as_str() {
            self.event_to_task
                .insert(ev.id.clone(), task_id.to_string());
        }
        let skills = self.skills.references(&text);
        self.turn_queue.push_back(TurnJob::new(
            ctx,
            Some(ev.id.clone()),
            principal.clone(),
            Some(crate::context::Msg::user(text.clone(), principal)),
            skills,
            text,
        ));
    }

    // ---- children ----------------------------------------------------------

    fn on_child_frame(&mut self, node: NodeId, msg: AgentMsg) {
        if !self.children.on_frame(node, &msg) {
            return; // a late frame from a reaped child
        }
        match msg {
            AgentMsg::Ready
            | AgentMsg::Pong { .. }
            | AgentMsg::Event { .. }
            | AgentMsg::Gate { .. }
            | AgentMsg::GateClosed { .. } => {}
            AgentMsg::Usage(u) => {
                self.counters.tokens_in += u.input_tokens;
                self.counters.tokens_out += u.output_tokens;
                crate::obs::metrics::record_tokens(u.input_tokens, u.output_tokens);
                // A subagent's usage is charged as it reports; turn usage is
                // settled on TurnDone against its reservation.
                if let Some(ChildKind::Subagent { .. }) = self.children.get(node).map(|c| &c.kind) {
                    self.governor.charge(u, &[]);
                }
            }
            AgentMsg::IntelHealth { all_down, .. } => {
                if crate::signals::set_intel_all_down(all_down) {
                    self.log.warn("intel.health", json!({"all_down": all_down}));
                }
            }
            AgentMsg::ToolRequest { id, name, args } => self.on_tool_request(node, id, &name, args),
            AgentMsg::BudgetRequest { id, estimate } => self.on_budget_request(node, id, estimate),
            AgentMsg::TurnDone { turn } => self.on_turn_done(node, *turn),
            AgentMsg::Turn { outcome } => self.on_subagent_turn(node, outcome),
            AgentMsg::Result { outcome } => self.on_subagent_result(node, Ok(outcome)),
            AgentMsg::Failed { error } => {
                let kind = self.children.get(node).map(|c| c.kind.clone());
                match kind {
                    Some(ChildKind::Subagent { .. }) => self.on_subagent_result(node, Err(error)),
                    Some(_) => self.on_turn_failed(node, error),
                    None => {}
                }
            }
        }
    }

    fn on_reaped(&mut self, r: Reaped) {
        let Some((node, child)) = self.children.on_reaped(&r) else {
            return;
        };
        self.log.info("child.exit", json!({"node": node.0, "pid": r.pid, "kind": super::children::kind_label(&child.kind), "outcome": format!("{:?}", r.outcome)}));
        // A child that died without its terminal frame: fail its unit.
        match child.kind {
            ChildKind::RootTurn { .. } | ChildKind::StepTurn { .. } | ChildKind::Think { .. } => {
                if self.pending_turn_exists(node) {
                    self.on_turn_failed(
                        node,
                        format!("worker exited without a result ({:?})", r.outcome),
                    );
                }
            }
            ChildKind::Subagent { ref handle } => {
                if self
                    .subagents
                    .get(handle)
                    .is_some_and(|s| !is_terminal_status(&s.status))
                {
                    self.on_subagent_result(
                        node,
                        Err(format!(
                            "subagent exited without a result ({:?})",
                            r.outcome
                        )),
                    );
                }
            }
        }
        // Answer any tool request that was waiting on this child (a think).
        let waiting: Vec<PendingTool> = self
            .pending
            .iter()
            .filter(|p| matches!(&p.kind, PendingKind::Think { child } if *child == node))
            .cloned()
            .collect();
        for p in waiting {
            self.pending.retain(|q| q.target != p.target);
            self.reply(
                &p.target,
                Value::String("think worker exited without a result".into()),
                true,
            );
        }
    }

    fn on_unhealthy_child(&mut self, node: NodeId, health: crate::supervisor::liveness::Health) {
        self.log.warn(
            "child.unhealthy",
            json!({"node": node.0, "health": format!("{health:?}")}),
        );
        self.children.cancel(node, &format!("{health:?}"));
        // Escalate: give it a moment, then kill.
        let started = self
            .children
            .get(node)
            .map(|c| c.started)
            .unwrap_or_else(Instant::now);
        if started.elapsed() > Duration::from_secs(1) {
            self.children.kill(node);
        }
    }

    // ---- lifecycle ---------------------------------------------------------

    fn check_signals(&mut self) {
        if crate::signals::draining() && !self.draining {
            self.begin_drain("signal");
        }
        if crate::signals::reload_requested() {
            crate::signals::clear_reload();
            self.on_reload_requested();
        }
    }

    pub(crate) fn begin_drain(&mut self, reason: &str) {
        if self.draining {
            return;
        }
        self.draining = true;
        self.drain_started = Some(Instant::now());
        self.drain_reason = reason.to_string();
        crate::signals::set_lame_duck(true);
        self.log.info("drain.start", json!({"reason": reason, "children": self.children.len(), "runs": self.runs.values().filter(|r| !r.status.is_terminal()).count()}));
        crate::obs::metrics::record_drain("started");
        // Tell every attached display client (RFC 0032 §4).
        #[cfg(feature = "a2a")]
        self.feed_push(
            "lifecycle",
            super::a2a_server::FeedVis::All,
            json!({"draining": true, "reason": reason}),
        );
        self.children.begin_drain(reason);
    }

    /// Decide whether to exit now. Returns the exit code when done.
    fn lifecycle_step(&mut self) -> Option<i32> {
        if let Some(code) = self.exit {
            // A `finish {exit: true}` or a fatal store failure asked to exit:
            // drain first.
            if !self.draining {
                self.begin_drain("exit");
            }
            if self.children.is_empty() {
                return Some(code);
            }
        }
        if self.draining {
            let timeout = self.settings.lifecycle.drain_timeout();
            let started = self.drain_started.unwrap_or_else(Instant::now);
            let force = crate::signals::force() || started.elapsed() >= timeout;
            let done = self.children.drive_drain(force);
            if done || started.elapsed() >= timeout + ABANDON_GRACE {
                if !done {
                    self.log
                        .warn("drain.abandon", json!({"children": self.children.len()}));
                    self.children.abandon();
                }
                crate::obs::metrics::record_drain("completed");
                self.checkpoint(true);
                self.log
                    .info("drain.done", json!({"reason": self.drain_reason}));
                return Some(self.exit.unwrap_or(crate::exit::SUCCESS));
            }
            return None;
        }
        // Job shape / idle policy.
        let run_until = self.settings.lifecycle.run_until;
        let idle_policy = match run_until {
            RunUntil::Idle => true,
            RunUntil::Drained => false,
            RunUntil::Auto => self.job_shape,
        };
        if !idle_policy {
            return None;
        }
        let busy = self.paused // a paused instance never idle-exits underneath the operator
            || !self.children.is_empty()
            || !self.turn_queue.is_empty()
            || !self.staged_turns.is_empty()
            || !self.inbox_queue.is_empty()
            || !self.pending.is_empty()
            || !self.executing.is_empty()
            || self.runs.values().any(|r| !r.status.is_terminal())
            || !self.timers.is_empty();
        if busy {
            self.idle_since = None;
            return None;
        }
        let since = *self.idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= self.settings.lifecycle.idle_grace() || self.job_shape {
            let code = self.job_exit_code();
            self.log.info(
                "lifecycle.idle_exit",
                json!({"code": code, "job_shape": self.job_shape}),
            );
            self.checkpoint(true);
            return Some(code);
        }
        None
    }

    /// The exit code of a job-shaped instance: the once-started workflow's
    /// finish status (RFC 0011 §5 mapping); a daemon drains to 0.
    fn job_exit_code(&self) -> i32 {
        let mut code = crate::exit::SUCCESS;
        for id in &self.job_runs {
            if let Some(r) = self.runs.get(id) {
                let c = run_exit_code(r);
                if c != crate::exit::SUCCESS {
                    code = c;
                }
            }
        }
        if self.job_runs.is_empty() && self.job_shape {
            // Nothing ever ran (no workflow fired) — a configuration edge; report success.
            return crate::exit::SUCCESS;
        }
        crate::exit::apply_budget_remap(
            code,
            self.settings
                .lifecycle
                .exit_code_map
                .get(&code.to_string())
                .copied(),
        )
    }

    fn shutdown(&mut self, code: i32) {
        self.children.abandon();
        let _ = self.durable.flush(true);
        self.log.info("proc.exit", json!({"code": code, "uptime_ms": self.started.elapsed().as_millis() as u64, "turns": self.counters.turns, "tool_calls": self.counters.tool_calls, "runs": self.counters.runs_finished, "tokens_in": self.counters.tokens_in, "tokens_out": self.counters.tokens_out}));
    }

    /// The job's result (the once-started run's output), for stdout.
    pub fn job_output(&self) -> Option<Value> {
        self.job_runs
            .iter()
            .rev()
            .filter_map(|id| self.runs.get(id))
            .find_map(|r| r.output.clone())
    }

    // ---- checkpoints ---------------------------------------------------------

    /// Persist dirty runs/contexts/subagents; flush the manifest (debounced,
    /// forced at drain). A halting store error triggers an exit.
    pub(crate) fn checkpoint(&mut self, force: bool) {
        let mut failed: Option<String> = None;
        for run in self.runs.values_mut() {
            if run.dirty {
                crate::state::kill_point("step.before_done");
                match self.durable.put(
                    Kind::Run,
                    &run.id,
                    serde_json::to_value(&*run).unwrap_or(Value::Null),
                    Some(run.workflow_hash.clone()),
                ) {
                    Ok(_) => run.dirty = false,
                    Err(e) => failed = Some(format!("run {}: {e}", run.id)),
                }
            }
        }
        if let Err(e) = self.contexts.checkpoint(&self.durable) {
            failed = Some(format!("context: {e}"));
        }
        for s in self.subagents.values_mut() {
            if s.dirty {
                match self.durable.put(
                    Kind::Subagent,
                    &s.handle,
                    serde_json::to_value(&*s).unwrap_or(Value::Null),
                    None,
                ) {
                    Ok(_) => s.dirty = false,
                    Err(e) => failed = Some(format!("subagent {}: {e}", s.handle)),
                }
            }
        }
        // Manifest: budget counters + lifecycle, debounced.
        let budget = self.governor.to_value();
        self.durable.manifest_update(|m| {
            m.budget = budget;
        });
        match self.durable.flush(force) {
            Ok(_) => {}
            Err(e) => failed = Some(format!("manifest: {e}")),
        }
        if let Some(e) = failed {
            self.log.error("store.checkpoint.fail", json!({"err": e}));
            if !self.durable.is_degraded() {
                // Halt policy: refuse new intake, drain.
                self.exit = Some(crate::exit::GENERIC);
            }
        }
    }

    // ---- status ------------------------------------------------------------

    /// `status` tool / `agent://status`.
    pub(crate) fn status_value(&self) -> Value {
        json!({
            "instance": self.instance,
            "run_id": self.run_id,
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "job_shape": self.job_shape,
            "draining": self.draining,
            "paused": self.paused,
            "store": {"kind": self.durable.store_kind(), "degraded": self.durable.is_degraded(), "generation": self.durable.manifest().generation},
            "workflows": self.workflows.values().map(|w| json!({"name": w.name, "hash": w.hash, "armed": w.armed, "starts": w.start_steps().iter().map(|s| s.kind.clone()).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "runs": self.runs.values().map(RunState::summary).collect::<Vec<_>>(),
            "conversations": self.contexts.status(),
            "subagents": self.subagents.values().map(|s| json!({"handle": s.handle, "mode": s.mode, "status": s.status, "tokens": s.tokens})).collect::<Vec<_>>(),
            "children": self.children.status(),
            "timers": self.timers.status(),
            "inbox_pending": self.inbox_queue.len(),
            "budget": self.governor.status(now_ms()),
            "tools": self.registry.len(),
            "skills": self.skills.names(),
            "counters": {"turns": self.counters.turns, "tool_calls": self.counters.tool_calls, "runs_started": self.counters.runs_started, "runs_finished": self.counters.runs_finished, "tokens_in": self.counters.tokens_in, "tokens_out": self.counters.tokens_out},
            "instruction": {"source": self.instruction.source, "uri": self.instruction.uri, "version": self.instruction.version, "bytes": self.instruction.text.len()},
            "model": self.model,
        })
    }

    /// The shortest time until the next time-based wake (a timer, an armed
    /// schedule/loop start, a suspended wait deadline, a budget wait). Bounded
    /// below at 5 ms so a due deadline is serviced on the next pass without a
    /// busy spin.
    fn next_wake(&self) -> Duration {
        let now = now_ms();
        let mut soonest = now + 200;
        if let Some(t) = self.timers.next_deadline() {
            soonest = soonest.min(t);
        }
        for st in self.durable.manifest().starts.values() {
            for k in ["next_ms", "debounce_until"] {
                if let Some(n) = st[k].as_u64() {
                    soonest = soonest.min(n);
                }
            }
        }
        for run in self.runs.values() {
            if run.status.is_terminal() {
                continue;
            }
            for step in run.steps.values() {
                if let Some(w) = &step.wait
                    && let Some(d) = w["deadline_ms"].as_u64()
                {
                    soonest = soonest.min(d);
                }
            }
        }
        if !self.pending.is_empty() || !self.turn_queue.is_empty() {
            soonest = soonest.min(now + 50);
        }
        Duration::from_millis(soonest.saturating_sub(now).max(5))
    }

    /// The model window (compaction threshold base): `context.model_window`
    /// when set, else inferred from the model name.
    pub(crate) fn model_window(&self) -> u64 {
        self.settings
            .context
            .model_window
            .unwrap_or_else(|| tokens::window_for_model(&self.model))
    }
}

pub(crate) fn is_terminal_status(s: &str) -> bool {
    matches!(
        s,
        "completed" | "failed" | "cancelled" | "refused" | "killed" | "crashed"
    )
}

/// RFC 0011 §5 exit mapping for a finished run.
pub fn run_exit_code(r: &RunState) -> i32 {
    match r.status {
        RunStatus::Completed => crate::exit::SUCCESS,
        RunStatus::Refused => crate::exit::REFUSED,
        RunStatus::Stalled => crate::exit::PARTIAL,
        RunStatus::Failed => {
            let e = r.error.as_deref().unwrap_or("");
            if e.contains("exhausted") || e.contains("budget") {
                crate::exit::BUDGET
            } else if e.contains("deadline") {
                crate::exit::DEADLINE
            } else if e.contains("intel") {
                crate::exit::INTEL_UNAVAILABLE
            } else {
                crate::exit::GENERIC
            }
        }
        RunStatus::Cancelled => crate::exit::GENERIC,
        _ => crate::exit::PARTIAL,
    }
}
