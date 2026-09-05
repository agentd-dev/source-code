// SPDX-License-Identifier: AGPL-3.0-only
//! The **runtime state + event loop**: one single-threaded reactor over child
//! frames, reaped children, executor results, timers, the durable inbox and
//! signals.
//!
//! State mutation happens only here. Being the single writer is what makes the
//! rest of the runtime reasonable about: no lock ordering, no torn reads, and
//! an answer computed for one caller is computed against one consistent view.
//! Every mutation is followed by a checkpoint decision, so durable state never
//! trails the in-memory state by more than one loop turn.
//!
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
    /// A human's answer (`ask_human` / the `human` node): the
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
        /// The answer's declared shape (`human.schema` / `ask_human.schema`).
        ///
        /// Carried on the pending ask so the reply can be validated against it
        /// when it lands. Forwarding the schema to clients only makes them
        /// render the right form; a gate that declares it wants
        /// `{decision: "file"|"hold"}` must also refuse "maybe later", or the
        /// run proceeds on an answer it never asked for.
        schema: Option<Value>,
        /// Who must answer (`to:`). `None` ⇒ whoever holds the task, which is
        /// the ordinary case. Enforced when the answer lands, for the same
        /// reason the schema is: a gate that names a decider and then accepts
        /// anyone records something that did not happen.
        addressee: Option<crate::a2a::principals::Addressee>,
    },
}

/// A queued root/conversation turn, waiting for a worker slot and for its
/// context to be free. One context runs at most one turn at a time, so turns
/// for the same conversation queue behind each other rather than interleaving
/// into the same history.
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
    /// The message-hop depth this turn inherits (see `RunState::msg_depth`).
    /// A message from a person is depth 0; one a `message` step delivered
    /// carries that step's depth, and anything this turn starts inherits it.
    pub msg_depth: u32,
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
            msg_depth: 0,
        }
    }
    /// The same job, carrying a delivered message's hop depth.
    pub fn at_depth(mut self, depth: u32) -> TurnJob {
        self.msg_depth = depth;
        self
    }
}

/// A subagent registry record, persisted as `subagent/<handle>` so a child's
/// identity and result outlive both the child and this process.
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
    /// The template this child was instantiated from, and its tier
    /// (`flat` | `instance`). A freeform spawn carries neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Instance tier: the child daemon's pid, config path, A2A socket and
    /// (epoch-ms) retire-at deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retire_at: Option<u64>,
    /// Instance tier: set when retirement began (SIGTERM sent); the tick
    /// escalates to SIGKILL after the drain window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retiring_since: Option<u64>,
    /// Durability class (default true). `false` ⇒ the record is memory-only:
    /// never persisted, never restore-respawned — the fast path for throwaway
    /// workers. Restored records (all persisted by construction) default true.
    #[serde(default = "record_durable_default")]
    pub durable: bool,
    #[serde(skip)]
    pub node: Option<NodeId>,
    #[serde(skip)]
    pub dirty: bool,
}

fn record_durable_default() -> bool {
    true
}

/// The instruction in force. `version` increments on every change, so a
/// consumer can tell a re-read from a genuinely new instruction.
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
    /// Resource pressure (disk headroom, cgroup memory): consulted at every
    /// ADMISSION gate — start-node firing, webhook accept, `workflow.run`,
    /// turn dispatch, subagent spawn — never on work already in flight.
    pub(crate) pressure: std::sync::Arc<super::pressure::Pressure>,
    /// The last level the tick reported, so transitions log exactly once.
    pub(crate) pressure_seen: super::pressure::Level,
    /// A step reached a terminal state since the last scheduling pass — its
    /// dependents may be ready NOW (the same-iteration re-schedule fixpoint).
    pub(crate) resched: bool,
    /// Reaps already deferred once for frame ordering (by pid) — see
    /// [`Runtime::on_reaped`].
    pub(crate) reap_deferred: std::collections::HashSet<i32>,
    /// Outbound token buckets for steps that declare `rate:`, keyed like the
    /// breaker (`workflow/unscoped-step`). In-memory on purpose: a rate is a
    /// statement about LIVE traffic, and a restart briefly refilling the burst
    /// is harmless where a durable bucket would be bookkeeping for its own
    /// sake. The paired f64 is the window seconds, for computing the wait.
    pub(crate) step_rates:
        std::collections::HashMap<String, (crate::supervisor::tree::TokenBucket, f64, u32)>,
    pub(crate) settings: Settings,
    /// The merged document the settings came from (restart-only diff base).
    pub(crate) settings_doc: Value,
    /// The invocation (for reload).
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    /// Workflow definitions pinned by live runs after a reload (hash → definition).
    pub(crate) pinned: BTreeMap<String, std::sync::Arc<Workflow>>,
    /// Retired definitions still owning live runs (`runtime::retire`), by hash.
    pub(crate) retiring: BTreeMap<String, super::retire::Retiring>,
    /// Definition hashes whose durable pin was written this life (one write
    /// per version; see `retire::ensure_pin`).
    pub(crate) pin_written: std::collections::HashSet<String>,
    /// The last payload per signal name (for `await`/`wait condition` views).
    pub(crate) recent_signals: BTreeMap<String, Value>,
    /// Memoized `memory.<key>` references per definition content hash: the
    /// scan walks the whole definition and `run_data` runs per step.
    pub(crate) memory_keys: std::collections::HashMap<String, Vec<String>>,
    /// An `emit` appended since the last stream poll (same-iteration wake).
    pub(crate) stream_dirty: bool,
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
    /// Per-principal budgets and rate quotas, indexed by principal id when one
    /// is first seen. `a2a.principals[].quotas` parsed and validated for a
    /// long time without anything reading it; these are its readers.
    pub(crate) principal_budgets: BTreeMap<String, crate::config::v2::Budget>,
    /// Only the A2A listener admits callers, so a build without it has
    /// nowhere to spend an arrival quota.
    #[cfg_attr(not(feature = "a2a"), allow(dead_code))]
    pub(crate) principal_rates: BTreeMap<String, crate::supervisor::tree::TokenBucket>,
    /// Labels an id acts under, for `_meta` and audit.
    pub(crate) principal_labels: BTreeMap<String, BTreeMap<String, String>>,
    pub(crate) workflows: BTreeMap<String, std::sync::Arc<Workflow>>,
    pub(crate) runs: BTreeMap<String, RunState>,
    pub(crate) children: Children,
    pub(crate) timers: Timers,
    pub(crate) events_rx: Receiver<Event>,
    pub(crate) events_tx: Sender<Event>,
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
    /// Resolved `intelligence.headers`, pushed on every LLM dial and threaded
    /// to subagents via the spawn payload so a child dials identically.
    pub(crate) intel_headers: Vec<(String, String)>,
    /// An optional intelligence credential provider: a closure returning the
    /// current bearer, refreshed from the device-login cache. Its resolved
    /// bearer overrides `intel_token`, and is threaded to subagents fresh at
    /// each spawn so no child carries a stale one. `None` when no
    /// `intelligence.auth` oauth2 block is configured.
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
    /// The §7.7 freshness deadline (unix-ms): a signed instruction source's
    /// authorization must be re-read before this, or the runtime refuses NEW
    /// work. `None` = no freshness watch armed.
    pub(crate) freshness_deadline_ms: Option<u64>,
    /// True when a signed instruction source has gone STALE past its freshness
    /// deadline (§7.7 rule 2): new autonomous work is refused; live work drains.
    /// A successful re-read clears it.
    pub(crate) freshness_frozen: bool,
    /// Durable A2A tasks, keyed by task id.
    #[cfg(feature = "a2a")]
    pub(crate) tasks: BTreeMap<String, crate::a2a::Task>,
    /// Inbox-event id → the A2A task it answers (a conversation turn).
    #[cfg(feature = "a2a")]
    pub(crate) event_to_task: BTreeMap<String, String>,
    /// The task snapshot the A2A listener threads read (None ⇒ not serving).
    #[cfg(feature = "a2a")]
    /// The interface event feed. `None` means the interface is disabled.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_feed: Option<std::sync::Arc<super::a2a_server::SharedFeed>>,
    /// Pairing-code login state. `None` means pairing is disabled.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_pairing: Option<std::sync::Arc<super::a2a_server::PairingState>>,
    /// The id the listener reserved for the task the request being served will
    /// create. Taken by the first `task_create` of that request, and cleared
    /// after it — an id belongs to one request only.
    #[cfg(feature = "a2a")]
    pub(crate) reserved_task_id: Option<String>,
    /// Where a task transition is published so A2A subscribers see it.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_sink: Option<std::sync::Arc<crate::a2a::ports::StreamSink>>,
    /// The live listener. Held, not used: dropping it stops serving.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_listener: Option<crate::a2a::serve::Listener>,
    /// The listener's bridge, so a reload can swap rebuilt principal rules in.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_bridge: Option<std::sync::Arc<super::a2a_server::A2aBridge>>,
    /// The webhook listener's handler, so a reload can swap rebuilt routes in.
    #[cfg(feature = "a2a")]
    pub(crate) webhook_handler: Option<std::sync::Arc<super::webhooks::WebhookHandler>>,
    /// The listener's live CORS allowlist, so a reload can revise it.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_origins: Option<crate::a2a::serve::OriginList>,
    /// Live per-unit activity, keyed by child node id.
    pub(crate) activity: BTreeMap<u64, super::activity::Activity>,
    /// The newest root-context reply, so a `--prompt` job can print its answer
    /// (a prompt runs as a turn, not as a `once` run with an output).
    pub(crate) last_root_reply: Option<String>,
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
    /// The deployment's default durability class for work (runs + subagent
    /// records): `store.durability.work: ephemeral` ⇒ false.
    pub(crate) fn work_durable_default(&self) -> bool {
        !matches!(
            self.settings.store.durability.work,
            Some(crate::config::v2::WorkDurability::Ephemeral)
        )
    }

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
            // Pressure transitions are logged HERE, once per change, so the
            // per-request gates can refuse silently instead of each writing its
            // own line per refusal — under real pressure that would be a log
            // flood on top of a disk that is already full.
            {
                let level = self.pressure.level();
                if level != self.pressure_seen {
                    let free = self
                        .pressure
                        .disk_free
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let detail = json!({
                        "level": level.as_str(),
                        "cause": self.pressure.cause(),
                        "disk_free_bytes": if free == u64::MAX { Value::Null } else { json!(free) },
                    });
                    match level {
                        super::pressure::Level::Ok => self.log.info("pressure.cleared", detail),
                        super::pressure::Level::Warn => self.log.warn("pressure.warn", detail),
                        super::pressure::Level::Shed => self.log.warn("pressure.shed", detail),
                    }
                    self.pressure_seen = level;
                }
            }
            // 1. Child frames.
            // (child frames arrive as Event::Child on the main channel — they
            // wake the parked loop instead of waiting for the tick)
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
            // 3.5. Retiring workflows whose drain deadline passed.
            self.retire_tick();
            // 4. Timers.
            let now = now_ms();
            for t in self.timers.fire(&self.durable, now) {
                self.on_timer(t);
            }
            // 4.9. The daemon's own events, queued by the tap since the last
            // tick, become appends — so a tripped breaker or a shed admission
            // can start a run. Done BEFORE the inbox and the start poll so
            // this tick's consumers see this tick's telemetry.
            self.drain_runtime_events();
            // 5. The inbox.
            self.process_inbox();
            // 6. Start nodes + runs (+ suspended waits).
            self.poll_starts();
            self.poll_stream_starts();
            // Joins consume the same stream, so they advance in the same pass —
            // and their window sweep runs every tick, which is what makes an
            // `on_timeout: fire_partial` escalation fire on time rather than on
            // the next event to arrive.
            self.poll_correlate_starts();
            // Runs parked on the log resolve in the same pass that advances
            // consumers, so a produce→wait hop costs a tick, not a timeout.
            self.poll_event_waits();
            self.poll_waits();
            self.schedule_runs();
            // Inline steps (assign/map/template/switch…) complete synchronously
            // inside that pass, which makes their dependents ready NOW — without
            // this fixpoint a pure data pipeline advanced ONE step per 200 ms
            // tick (measured: 200 chained assigns = 42 s; with it, milliseconds).
            // Bounded for the loop's honesty: effectful steps complete via
            // events, so only inline chains re-enter here, and `limits.run.steps`
            // already caps how long one can be.
            let mut passes = 0;
            while std::mem::take(&mut self.resched) && passes < 1024 {
                self.schedule_runs();
                passes += 1;
            }
            // 6.6. Streams appended in this iteration fire their consumers
            // NOW: a same-process produce->consume pipeline advances at
            // engine speed instead of paying the tick park per hop. Bounded
            // like the fixpoint; an emit inside a fired consumer re-enters
            // here, and `limits.run.steps` caps how deep that can go.
            let mut stream_rounds = 0;
            while std::mem::take(&mut self.stream_dirty) && stream_rounds < 64 {
                self.poll_stream_starts();
                self.poll_correlate_starts();
                // A run parked on the log is a consumer too: without this, a
                // saga whose awaited event was emitted by a step in this very
                // iteration would park until the next tick.
                self.poll_event_waits();
                self.schedule_runs();
                let mut passes = 0;
                while std::mem::take(&mut self.resched) && passes < 1024 {
                    self.schedule_runs();
                    passes += 1;
                }
                stream_rounds += 1;
            }
            // 7. Turns.
            self.dispatch_turns();
            // 8. Pending waits + MCP notifications.
            self.poll_pending();
            self.poll_mcp_notifications();
            // 9. Children maintenance.
            for (node, health) in self.children.tick() {
                self.on_unhealthy_child(node, health);
            }
            // 9b. Instance-tier children: ttl retirement, plus the
            // SIGTERM→SIGKILL escalation for children that ignored the drain.
            self.instances_tick();
            // 10. Checkpoints + the point-in-time observability gauges.
            self.checkpoint(false);
            crate::obs::metrics::set_inbox_pending(self.inbox_queue.len() as u64);
            crate::obs::metrics::set_context_tokens(self.contexts.max_est_tokens());
            {
                let free = self
                    .pressure
                    .disk_free
                    .load(std::sync::atomic::Ordering::Relaxed);
                crate::obs::metrics::set_pressure(
                    self.pressure_seen as u64,
                    (free != u64::MAX).then_some(free),
                );
                crate::obs::metrics::set_work_backlog(
                    self.runs
                        .values()
                        .filter(|r| !r.status.is_terminal())
                        .count() as u64,
                    self.turn_queue.len() as u64,
                );
            }
            // 10.5. The interface feed's section diff: publish
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
            Event::SubscribeRead {
                server,
                uri,
                content,
            } => self.on_subscribe_read(&server, &uri, content),
            Event::Background { .. } | Event::Tick => {}
        }
    }

    // ---- inbox -------------------------------------------------------------

    /// Accept a durable event: write it to the store first, then queue it for
    /// the loop. Write-ahead is the whole point — once acceptance is
    /// acknowledged to the outside world, a crash before the event is acted on
    /// must replay it rather than drop it.
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
        // Drain a SNAPSHOT, never the live deque: a start event that overflows
        // its workflow's concurrency cap re-queues itself (`on_overflow: queue`,
        // the default), and the cap can only be relieved by `schedule_runs` — a
        // LATER step of this tick. Popping from the same deque the requeue
        // pushes onto re-offers the event immediately and the single-writer
        // reactor spins at 100% CPU forever: no timers, no checkpoint, no
        // SIGTERM. Requeued (and newly accepted) events land in the fresh
        // `self.inbox_queue` and are retried on the next tick instead.
        let mut batch = std::mem::take(&mut self.inbox_queue);
        while let Some(ev) = batch.pop_front() {
            if self.draining {
                // Keep it durable for the next life; stop intake — with one
                // exception: the start event of a `lifecycle.shutdown` deinit
                // workflow exists to run DURING the drain, and the drain gate
                // is waiting for it. Everything else waits for the next life.
                let deinit = ev.kind == kinds::START_FIRED
                    && ev.payload["workflow"]
                        .as_str()
                        .and_then(|n| self.workflows.get(n))
                        .is_some_and(|w| {
                            w.start_steps().iter().any(|s| {
                                s.kind == "event" && s.field_str("on") == Some("lifecycle.shutdown")
                            })
                        });
                if !deinit {
                    self.inbox_queue.push_back(ev);
                    continue;
                }
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
                    // Handled the same whether it arrived live or was replayed
                    // from the inbox after a restart.
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
        // Whatever the drain did not consume keeps its place ahead of the
        // events requeued (or accepted) while the batch was processing.
        batch.append(&mut self.inbox_queue);
        self.inbox_queue = batch;
    }

    pub(crate) fn inbox_done(&mut self, id: &str) {
        if let Err(e) = self.durable.inbox_done(id) {
            self.log.warn(
                "inbox.done.fail",
                json!({"inbox_event": id, "err": e.to_string()}),
            );
        }
    }

    /// An A2A message event, routed to whichever reader owns it. Control-plane
    /// ops are consumed first, then a waiting step, then a start node, and only
    /// what is left becomes a conversation turn.
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
        // `_instance.*` ops are a child reporting home. The runtime consumes
        // them BEFORE any reader, so they can never be mistaken for a wait's
        // answer, a start's request, or a conversational turn — control-plane
        // traffic must not reach a model.
        #[cfg(feature = "a2a")]
        if self.handle_instance_op(ev) {
            return;
        }
        // An inbound message has three possible readers, in this order. Only one
        // takes it: a message that woke a waiting step is an ANSWER, and a
        // message that fired a workflow is a REQUEST — neither should also
        // become a conversational turn, or the agent replies to itself.
        //
        // 1. A step suspended on this conversation (`a2a.wait` / `wait {on:
        //    message}`) — the reply half of an asynchronous exchange.
        let msg = json!({"parts": ev.payload.get("parts").cloned().unwrap_or(Value::Null),
                         "text": text, "message_id": ev.payload.get("message_id").cloned()});
        if self.deliver_a2a_message(&ctx, &msg, principal.as_deref()) > 0 {
            self.log.info(
                "a2a.message.delivered",
                json!({"inbox_event": ev.id, "conversation": ctx}),
            );
            return;
        }
        // 2. An `a2a` START node whose command and roles match — a peer or an
        //    operator asking for a workflow rather than a conversation.
        if self.fire_a2a_start(ev, &ctx) {
            return;
        }
        // 3. Otherwise it is what it looks like: something to answer.
        #[allow(unused)]
        let skills = self.skills.references(&text);
        let depth = ev.payload["msg_depth"].as_u64().unwrap_or(0) as u32;
        self.turn_queue.push_back(
            TurnJob::new(
                ctx,
                Some(ev.id.clone()),
                principal.clone(),
                Some(crate::context::Msg::user(text.clone(), principal)),
                skills,
                text,
            )
            .at_depth(depth),
        );
    }

    /// Without the `a2a` feature there is no listener to deliver a message, so a
    /// replayed event simply degrades to a turn.
    #[cfg(not(feature = "a2a"))]
    fn fire_a2a_start(&mut self, _ev: &InboxEvent, _ctx: &str) -> bool {
        false
    }

    /// Match an inbound A2A message against every `a2a` start node and fire the
    /// first that accepts it. Returns whether a run was started.
    ///
    /// `command` selects on the command DataPart's `op` — absent means "any
    /// message", which is how a workflow takes plain conversation as its
    /// trigger. `roles` restricts which principals may fire it, and defaults to
    /// no restriction beyond the authorization the listener already applied:
    /// the start node narrows, it never widens.
    #[cfg(feature = "a2a")]
    fn fire_a2a_start(&mut self, ev: &InboxEvent, ctx: &str) -> bool {
        let op = ev.payload.get("parts").and_then(|parts| {
            crate::runtime::a2a_server::command_op(&json!({"parts": parts.clone()}))
        });
        // The typed command payload, `op` removed: a workflow reads
        // `{{ steps.cmd.output.args.<field> }}` instead of parsing parts.
        let args = ev.payload.get("parts").and_then(|parts| {
            crate::runtime::a2a_server::command_data(&json!({"parts": parts.clone()})).map(
                |mut d| {
                    if let Some(o) = d.as_object_mut() {
                        o.remove("op");
                    }
                    d
                },
            )
        });
        let role = ev.payload["role"].as_str().unwrap_or("");
        let specs: Vec<(String, String, serde_json::Map<String, Value>)> = self
            .workflows
            .values()
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .filter(|s| s.kind == "a2a")
                    .map(|s| (w.name.clone(), s.id.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (workflow, node, spec) in specs {
            if let Some(want) = spec.get("command").and_then(Value::as_str)
                && Some(want) != op.as_deref()
            {
                continue;
            }
            if let Some(roles) = spec.get("roles").and_then(Value::as_array)
                && !roles.is_empty()
                && !roles.iter().any(|r| r.as_str() == Some(role))
            {
                continue;
            }
            let payload = json!({
                "conversation": ctx,
                "principal": ev.principal,
                "role": role,
                "command": op,
                "args": args.clone().unwrap_or(Value::Null),
                // The A2A task tracking this message: carried onto the run so
                // its terminal status completes the task — which is what lets
                // a peer's `a2a.delegate {command}` BLOCK on the answer.
                "task": ev.payload.get("task").cloned().unwrap_or(Value::Null),
                "parts": ev.payload.get("parts").cloned().unwrap_or(Value::Null),
                "text": ev.payload.get("text").cloned().unwrap_or(Value::Null),
                "message_id": ev.payload.get("message_id").cloned().unwrap_or(Value::Null),
                // The message-hop depth rides through this reader too. Without
                // it a chain routed through an `a2a` start would reset to zero
                // on every hop, and the cap would never bite — the run this
                // fires can `message` again, and that is the same loop.
                "msg_depth": ev.payload.get("msg_depth").cloned().unwrap_or(json!(0)),
            });
            // `into: {stream, subject}` — APPEND the message instead of
            // firing a run (RFC 0035 §5), so a fleet peer can feed a stream
            // over mTLS (or the co-located unix-socket lane) and get the same
            // replay-after-downtime a webhook `into` gives. Authorization has
            // already happened: the principal was resolved and its `roles`
            // filter applied above, so this is the last step, not a bypass.
            if let Some(into) = spec.get("into") {
                let stream = into.get("stream").and_then(Value::as_str).unwrap_or("");
                let subject = into.get("subject").and_then(Value::as_str).unwrap_or("");
                let id = ev
                    .payload
                    .get("message_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::state::ulid::new().to_string());
                match self.append_event(stream, subject, Some(ctx), payload, &id, &workflow) {
                    Ok(seq) => self.log.info(
                        "start.a2a.into",
                        json!({"workflow": workflow, "node": node, "conversation": ctx,
                               "stream": stream, "subject": subject, "seq": seq}),
                    ),
                    Err(e) => self.log.warn(
                        "start.a2a.into.refused",
                        json!({"workflow": workflow, "node": node, "stream": stream,
                               "err": e}),
                    ),
                }
                return true;
            }
            self.log.info(
                "start.a2a.fired",
                json!({"workflow": workflow, "node": node, "conversation": ctx,
                       "command": op, "role": role}),
            );
            self.fire_start(&workflow, &node, &spec, payload, "a2a");
            return true;
        }
        false
    }

    // ---- children ----------------------------------------------------------

    fn on_child_frame(&mut self, node: NodeId, msg: AgentMsg) {
        if !self.children.on_frame(node, &msg) {
            return; // a late frame from a reaped child
        }
        match msg {
            AgentMsg::Ready
            | AgentMsg::Pong { .. }
            | AgentMsg::Gate { .. }
            | AgentMsg::GateClosed { .. } => {}
            // Coarse progress from the child: what this unit is doing right
            // now, for the display clients' working row.
            AgentMsg::Event { event, fields } => self.on_child_progress(node, &event, &fields),
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
        // Frames-before-reap. A child's terminal frame rides the same event
        // queue as everything else (that is what makes its arrival WAKE the
        // loop), so a reap racing ahead of it would read as "worker exited
        // without a result". Restore the invariant by construction: join the
        // child's reader thread — bounded, its pipe has already EOF'd — so
        // every frame it ever wrote is IN the queue, then requeue the reap
        // BEHIND them. FIFO does the rest; one deferral suffices.
        if !self.reap_deferred.remove(&r.pid) && self.children.has_pid(r.pid) {
            self.children.join_reader_of(r.pid);
            self.reap_deferred.insert(r.pid);
            let _ = self.events_tx.send(Event::Reaped(r));
            return;
        }
        // An instance-tier daemon child has no control channel and no node in
        // the child table, so its exit closes the subagent record directly.
        if !self.children.has_pid(r.pid) && self.on_instance_reaped(&r) {
            return;
        }
        let Some((node, child)) = self.children.on_reaped(&r) else {
            return;
        };
        self.activity_end(node);
        self.log.info("child.exit", json!({"node": node.0, "pid": r.pid, "kind": super::children::kind_label(&child.kind), "outcome": format!("{:?}", r.outcome)}));
        // A child that died without its terminal frame: fail its unit.
        match child.kind {
            // Ask the STEP, not the child table, whether this worker died
            // owing a result. The child table cannot answer it here: a
            // `TurnDone` settles the step but leaves the child in the table
            // until it is reaped, and `Children::on_reaped` above has already
            // removed the entry — so "is the child in the table?" reads the
            // same for a settled worker and an orphaned one. The step is
            // unambiguous: it is Running and still owned by THIS worker only
            // when no terminal frame ever landed.
            ChildKind::StepTurn {
                ref run,
                ref step,
                reservation,
            } => {
                let node_owned = node.0.to_string();
                let orphaned = self
                    .runs
                    .get(run)
                    .and_then(|st| st.step(step))
                    .is_some_and(|s| {
                        s.status == crate::engine::StepStatus::Running
                            && s.worker.as_deref() == Some(node_owned.as_str())
                    });
                if orphaned {
                    // `on_turn_failed` would route this, but it re-reads the
                    // child table too and returns early on the reaped node; the
                    // reservation it would have released is released here.
                    if let Some(res) = reservation {
                        self.governor.release(res);
                    }
                    self.log.warn(
                        "turn.failed",
                        json!({"node": node.0, "kind": super::children::kind_label(&child.kind), "err": "worker exited without a result"}),
                    );
                    self.on_step_turn_done(
                        run,
                        step,
                        crate::subagent::protocol::TurnResult {
                            status: "failed".into(),
                            error: Some(format!(
                                "worker exited without a result ({:?})",
                                r.outcome
                            )),
                            ..Default::default()
                        },
                    );
                }
            }
            // A root turn and a think expose no equivalent state to test
            // here, so they ask `pending_turn_exists`, which answers from the
            // settled marker `on_turn_done` / `on_turn_failed` leave on the
            // child record rather than from the child's presence in the table.
            // Presence cannot answer it: `on_reaped` has already removed the
            // child by the time this runs, and a normally-settled worker also
            // stays in the table until it is reaped, so presence reads the same
            // for settled and orphaned workers alike.
            ChildKind::RootTurn { .. } | ChildKind::Think { .. } => {
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
        // Tell every attached display client, so a client can stop offering
        // actions the daemon will now refuse.
        #[cfg(feature = "a2a")]
        self.feed_push(
            "lifecycle",
            super::a2a_server::FeedVis::All,
            json!({"draining": true, "reason": reason}),
        );
        self.children.begin_drain(reason);
        // Deinitialization workflows: `event {on: lifecycle.shutdown}` starts
        // fire NOW — releasing a claimed webhook route, deregistering from a
        // service, flushing a summary — and the drain below WAITS for exactly
        // those runs (bounded by drain_timeout like everything else). The
        // mirror of `once {policy: always}`, which is the init workflow.
        self.fire_event_starts("lifecycle.shutdown", &json!({"reason": reason}));
    }

    /// Non-terminal runs of workflows that declare a `lifecycle.shutdown`
    /// start — the runs drain must wait for. (Any of the workflow's runs
    /// counts: an in-flight ordinary run of a deinit-capable workflow is not
    /// distinguishable from the deinit run by the time both must finish.)
    fn shutdown_runs_live(&self) -> usize {
        let capable = |name: &str, hash: &str| {
            self.definition_for_run_ref(name, hash).is_some_and(|w| {
                w.start_steps()
                    .iter()
                    .any(|s| s.kind == "event" && s.field_str("on") == Some("lifecycle.shutdown"))
            })
        };
        let live = self
            .runs
            .values()
            .filter(|r| !r.status.is_terminal())
            .filter(|r| capable(&r.workflow, &r.workflow_hash))
            .count();
        // A fired-but-not-yet-created run is still in the inbox for a tick —
        // the gate must not slip through that window.
        let queued = self
            .inbox_queue
            .iter()
            .filter(|e| e.kind == super::events::kinds::START_FIRED)
            .filter(|e| {
                e.payload["workflow"]
                    .as_str()
                    .and_then(|n| self.workflows.get(n))
                    .is_some_and(|w| {
                        w.start_steps().iter().any(|s| {
                            s.kind == "event" && s.field_str("on") == Some("lifecycle.shutdown")
                        })
                    })
            })
            .count();
        live + queued
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
            let done =
                self.children.drive_drain(force) && (force || self.shutdown_runs_live() == 0);
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
        // `auto` re-reads the LIVE workflow set, not just the configured one:
        // a long-lived workflow the agent defined at runtime (`workflow.create`
        // — the self-setup shape, where a `--prompt` tells it to build its own
        // loop/schedule/subscribe) turns the one-shot job into a daemon exactly
        // as a configured one would have. Without this the instance idle-exits
        // out from under the thing it was just asked to set up.
        let job_now = self.job_shape && !self.workflows.values().any(|w| w.is_long_lived());
        let idle_policy = match run_until {
            RunUntil::Idle => true,
            RunUntil::Drained => false,
            RunUntil::Auto => job_now,
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
        if since.elapsed() >= self.settings.lifecycle.idle_grace() || job_now {
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

    /// The exit code of a job-shaped instance, mapped from the `once`-started
    /// workflow's finish status. With several such runs the worst outcome
    /// wins, so a partial success is never reported as a clean exit. A daemon
    /// is not job-shaped and drains to 0.
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
            // A `--prompt` job has no `once` run to carry an output: its answer
            // is the root turn's reply.
            .or_else(|| self.last_root_reply.clone().map(Value::String))
    }

    // ---- checkpoints ---------------------------------------------------------

    /// Persist dirty runs/contexts/subagents; flush the manifest (debounced,
    /// forced at drain). A halting store error triggers an exit.
    pub(crate) fn checkpoint(&mut self, force: bool) {
        let mut failed: Option<String> = None;
        for run in self.runs.values_mut() {
            if run.dirty {
                // A non-durable run (workflow `durable: false`, or the
                // `store.durability.work: ephemeral` default) is memory-only:
                // no serialization, no write, gone after a restart.
                if !run.durable {
                    run.dirty = false;
                    continue;
                }
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
                if !s.durable {
                    s.dirty = false;
                    continue;
                }
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
            "subagents": self.subagents.values().map(|s| json!({"handle": s.handle, "mode": s.mode, "status": s.status, "tokens": s.tokens, "template": s.template, "tier": s.tier, "pid": s.pid, "retire_at": s.retire_at})).collect::<Vec<_>>(),
            "children": self.children.status(),
            "timers": self.timers.status(),
            "inbox_pending": self.inbox_queue.len(),
            "budget": self.governor.status(now_ms()),
            "tools": self.registry.len(),
            "skills": self.skills.names(),
            "counters": {"turns": self.counters.turns, "tool_calls": self.counters.tool_calls, "runs_started": self.counters.runs_started, "runs_finished": self.counters.runs_finished, "tokens_in": self.counters.tokens_in, "tokens_out": self.counters.tokens_out},
            "instruction": {"source": self.instruction.source, "uri": self.instruction.uri, "version": self.instruction.version, "bytes": self.instruction.text.len()},
            "model": self.model,
            "activity": self.activity_value(),
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
    /// The model window (compaction threshold base).
    ///
    /// `context.model_window` wins, then the active tier's declared `window`,
    /// and only then the guess from the model NAME — a substring match that is
    /// simply wrong for any provider whose naming does not happen to match.
    /// A tier that declares its window replaces the guess with a fact.
    pub(crate) fn model_window(&self) -> u64 {
        if let Some(w) = self.settings.context.model_window {
            return w;
        }
        if let Some(w) = self
            .settings
            .intelligence
            .default_reference()
            .and_then(|r| self.settings.intelligence.tier(&r).and_then(|t| t.window))
        {
            return w;
        }
        tokens::window_for_model(&self.model)
    }
}

pub(crate) fn is_terminal_status(s: &str) -> bool {
    matches!(
        s,
        "completed" | "failed" | "cancelled" | "refused" | "killed" | "crashed" | "retired"
    )
}

/// Map a finished run's status onto a process exit code, so a caller can tell
/// *how* a job ended without parsing its output: refusal, budget exhaustion,
/// a missed deadline and an unreachable model each get their own code, and
/// anything still unfinished reports as partial.
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
