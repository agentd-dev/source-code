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
use crate::config::settings::{RunUntil, Settings};
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
        /// Set when this gate is a `security.policies` `action: ask` verdict:
        /// the call it is holding. The answer is then a DECISION about that
        /// call, not a result for it — an approval runs it and the asker gets
        /// its real result, a refusal is an error. Without this the approver's
        /// words came back as the tool's result, and a tool that never ran
        /// read as one that had.
        policy: Option<PolicyCall>,
    },
}

/// The tool call a `security.policies` `action: ask` gate holds (see
/// [`PendingKind::Human`]).
#[derive(Debug, Clone)]
pub struct PolicyCall {
    pub tool: String,
    pub args: Value,
    /// The index of the rule that asked, for every message about it.
    pub rule: usize,
    /// What the rule said an unanswered gate becomes.
    pub on_timeout: crate::config::settings::PolicyAction,
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
    /// A caller asked for this turn — over the A2A listener, or through the
    /// `message.send` of a turn it drives — so it may run only in a
    /// conversation that caller owns (an operator's in any). Work the
    /// runtime itself delivers — a `message` step, the prompt, a subagent or
    /// a timer acting for nobody — is not held to that: it is the instance
    /// talking to itself.
    pub owner_checked: bool,
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
            owner_checked: false,
        }
    }
    /// The same job, carrying a delivered message's hop depth.
    pub fn at_depth(mut self, depth: u32) -> TurnJob {
        self.msg_depth = depth;
        self
    }
    /// The same job, marked as asked for by a caller who must own its
    /// conversation (or not).
    pub fn owner_checked(mut self, checked: bool) -> TurnJob {
        self.owner_checked = checked;
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
    /// The principal this subagent works for: who may steer, read or kill it.
    /// `requested_by` names WHAT spawned it — a conversation, a run, another
    /// subagent — and any of those can be gone by the time someone asks, so
    /// the owner is recorded at spawn ([`inherited_principal`]); `None` is
    /// operator-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
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
    /// The registry version this text came from (`md.instruction/versionId`,
    /// RFC-0028 §3.3) — `None` for sources that are not a versioned registry.
    pub version_id: Option<String>,
    /// The delivered-content digest the registry attested
    /// (`md.instruction/deliveredDigest`).
    pub delivered_digest: Option<String>,
}

impl Instruction {
    /// Where to re-read THIS instruction from: the server that served it
    /// (`mcp://<server>/<uri>`), never whichever connected server happens to
    /// answer the same uri. A bare uri fans out to every server, so once the
    /// serving registry died an unrelated server — or a mirror holding an older
    /// signed copy — would confirm freshness for it. `oci://` and a uri with no
    /// serving server recorded stay as they are; static text has none.
    ///
    /// The one derivation the freshness poll, the notification re-read and the
    /// `instruction.subscribe` tool all use.
    pub(crate) fn source_ref(&self) -> Option<String> {
        let uri = self.uri.as_deref()?;
        Some(match &self.server {
            Some(s) => format!("mcp://{s}/{uri}"),
            None => uri.to_string(),
        })
    }

    /// What the `instruction.subscribe` tool follows: the `uri` its caller
    /// named, else [`Self::source_ref`] — the serving server, not a bare uri
    /// any connected server would answer.
    pub(crate) fn subscribe_target(&self, args: &serde_json::Value) -> Option<String> {
        args.get("uri")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| self.source_ref())
    }
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
    /// Per-principal budgets, indexed by principal id when one is first seen.
    /// `a2a.principals[].quotas` parsed and validated for a long time without
    /// anything reading it; this is the budget's reader. (The rate is
    /// admission, and the listener applies it.)
    pub(crate) principal_budgets: BTreeMap<String, crate::config::settings::Budget>,
    /// Labels an id acts under, for `_meta` and audit.
    pub(crate) principal_labels: BTreeMap<String, BTreeMap<String, String>>,
    /// Every resolved caller as last seen, by principal id. A turn or a run
    /// carries only the id of whoever it works for, so this is how the
    /// model's tools recover that caller's role and grants. Keyed on the id
    /// alone — never on a credential or session — so one principal's several
    /// sessions are one owner, and what it owns outlives a re-login.
    pub(crate) principal_index: BTreeMap<String, crate::a2a::Principal>,
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
    /// The lifetime token ceiling has tripped and its policy has been applied.
    /// Latched so a ceiling every subsequent admission also trips does not
    /// re-log or re-drain.
    pub(crate) lifetime_spent: bool,
    /// Operator-held (admin.pause): intake continues; no new turns dispatch and
    /// no steps schedule until admin.resume. Reversible, unlike drain.
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
    /// When the tick last applied `store.retention.tasks` (see
    /// `Runtime::sweep_terminal_tasks`): the sweep is rate-limited, because the
    /// tick that calls it can run many scheduling passes a second.
    #[cfg(feature = "a2a")]
    pub(crate) tasks_swept: Instant,
    /// Each non-operator's `contextId`s, bound to the conversations they name
    /// (see `runtime::conversations`). Rebuilt from the tasks and contexts at
    /// restore.
    #[cfg(feature = "a2a")]
    pub(crate) conv_index: super::conversations::ConversationIndex,
    /// The observation feed. `None` means the feed is off.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_feed: Option<std::sync::Arc<super::a2a_server::SharedFeed>>,
    /// The id the listener reserved for the task the request being served will
    /// create. Taken by the first `task_create` of that request, and cleared
    /// after it — an id belongs to one request only.
    #[cfg(feature = "a2a")]
    pub(crate) reserved_task_id: Option<String>,
    /// The extensions the request being served activated; `NONE` between
    /// requests. A task's annotations are projected only while it holds
    /// task-annotations.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_active: super::surface::Active,
    /// Where a task transition is published so A2A subscribers see it.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_sink: Option<std::sync::Arc<crate::a2a::ports::StreamSink>>,
    /// The live listener, its bridge (so a reload can swap rebuilt principal
    /// rules — and the posture with them — in), its CORS allowlist and the URL
    /// it is published at. Held while serving: dropping it stops the listener.
    #[cfg(feature = "a2a")]
    pub(crate) a2a_serving: Option<super::a2a_server::A2aServing>,
    /// The webhook listener's handler, so a reload can swap rebuilt routes in.
    #[cfg(feature = "a2a")]
    pub(crate) webhook_handler: Option<std::sync::Arc<super::webhooks::WebhookHandler>>,
    /// Live per-unit activity, keyed by child node id.
    pub(crate) activity: BTreeMap<u64, super::activity::Activity>,
    /// The newest root-context reply, so a `--prompt` job can print its answer
    /// (a prompt runs as a turn, not as a `once` run with an output).
    pub(crate) last_root_reply: Option<String>,
    /// Per-item marks behind the feed's section diffing (`feed_tick`): the
    /// item's fingerprint, its kind and who may see it — the last two so its
    /// departure reaches exactly the principals its updates did.
    #[cfg(feature = "a2a")]
    pub(crate) feed_marks: BTreeMap<String, (u64, &'static str, super::a2a_server::FeedVis)>,
    /// The `observability.status_values` last resolved: when, for which keys,
    /// and to what. `status` and the feed's status item both publish them, so
    /// without it a client polling `status` would be a store read per key per
    /// poll. Behind a lock only because the status view is built from `&self`.
    pub(crate) status_values_cache: std::sync::Mutex<Option<(Instant, Vec<String>, Value)>>,
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
            Some(crate::config::settings::WorkDurability::Ephemeral)
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
            // 10.5. The observation feed's section diff: publish
            // run/conversation/subagent/child/status deltas to attached display
            // clients. A no-op unless `a2a.events.enabled`; rate-limited inside.
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
                    // Recorded with the principal the sending tool call
                    // acted for, so a replay after a restart is scoped as the
                    // live delivery would have been.
                    let sender = self.signal_sender(ev.principal.as_deref());
                    let delivered = self.deliver_signal(
                        &name,
                        payload,
                        target.as_deref(),
                        from.as_deref(),
                        &sender,
                    );
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
        // The name the sender used for the conversation — the key itself for
        // an operator and a `message` step.
        let wire = ev.payload["wire_id"].as_str().unwrap_or(&ctx).to_string();
        // Only the listener writes a message ahead with the task it answers;
        // a caller's own `message.send` says it is one (`owner_checked`).
        // Either way the turn is the caller's, and held to its conversations.
        let owner_checked =
            ev.payload["task"].is_string() || ev.payload["owner_checked"] == Value::Bool(true);
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
        if self.deliver_a2a_message(&ctx, &wire, &msg, principal.as_deref()) > 0 {
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
            .at_depth(depth)
            .owner_checked(owner_checked),
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
        let fired =
            crate::runtime::a2a_server::a2a_start_node(&self.workflows, op.as_deref(), role)
                .map(|(w, s)| (w.name.clone(), s.id.clone(), s.spec.clone()));
        if let Some((workflow, node, spec)) = fired {
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

    /// The lifetime token ceiling is spent — decide what the INSTANCE does.
    ///
    /// Called from every admission that trips it, so it must be idempotent: a
    /// drain already under way is left alone, and the log line is emitted once.
    /// The unit that tripped it still fails; this only answers the larger
    /// question of whether the process should carry on refusing.
    pub(crate) fn apply_lifetime_exhausted(&mut self, reason: &str) {
        use crate::config::settings::LifetimeExhausted as P;
        let policy = self.settings.intelligence.budget.lifetime_exhausted;
        if self.lifetime_spent {
            return;
        }
        self.lifetime_spent = true;
        self.log.warn(
            "budget.lifetime_exhausted",
            json!({"reason": reason, "policy": format!("{policy:?}").to_lowercase()}),
        );
        match policy {
            // Deliberately nothing beyond the line above: the operator asked to
            // keep the instance up, and every admission from here fails.
            P::Refuse => {}
            P::Drain => {
                self.note_root(
                    "budget.lifetime_exhausted: the lifetime token budget is spent; \
                     finishing live work and then exiting."
                        .into(),
                );
                self.begin_drain("lifetime token budget exhausted");
            }
            P::Exit => {
                self.log.error(
                    "proc.exit",
                    json!({"code": crate::exit::BUDGET, "err": reason}),
                );
                self.exit = Some(crate::exit::BUDGET);
            }
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

    /// The `status` tool and op: the whole document, as an operator
    /// sees it. A caller who is not the operator is answered from
    /// [`Runtime::status_value_for`].
    pub(crate) fn status_value(&self) -> Value {
        let mut doc = self.status_facts();
        let full = json!({
            "run_id": self.run_id,
            "job_shape": self.job_shape,
            "store": {"kind": self.durable.store_kind(), "degraded": self.durable.is_degraded(), "generation": self.durable.manifest().generation},
            "workflows": self.workflows.values().map(|w| json!({"name": w.name, "hash": w.hash, "armed": w.armed, "starts": w.start_steps().iter().map(|s| s.kind.clone()).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "runs": self.runs.values().map(RunState::summary).collect::<Vec<_>>(),
            "conversations": self.conversation_views(),
            "subagents": self.subagents.values().map(|s| json!({"handle": s.handle, "mode": s.mode, "status": s.status, "tokens": s.tokens, "template": s.template, "tier": s.tier, "pid": s.pid, "retire_at": s.retire_at})).collect::<Vec<_>>(),
            "children": self.children.status(),
            "timers": self.timers.status(),
            "inbox_pending": self.inbox_queue.len(),
            "budget": self.governor.status(now_ms()),
            "tools": self.registry.len(),
            "counters": self.counters_value(),
            "instruction": {"source": self.instruction.source, "uri": self.instruction.uri, "version": self.instruction.version, "version_id": self.instruction.version_id, "delivered_digest": self.instruction.delivered_digest, "bytes": self.instruction.text.len()},
            "activity": self.activity_value(),
        });
        if let (Value::Object(doc), Value::Object(full)) = (&mut doc, full) {
            doc.extend(full);
        }
        // How many `contextId`s callers have bound: memory the listener holds
        // on their behalf, so its growth is visible to the one who must bound
        // it.
        #[cfg(feature = "a2a")]
        {
            doc["conversation_bindings"] = json!(self.conv_index.len());
        }
        doc
    }

    /// The facts about the instance itself that every caller of `status` is
    /// told: that it is up and how, and what a client needs to talk to it —
    /// the skills, the prefix that preloads one, and the values the operator
    /// chose to publish. Nothing here is anyone's work.
    fn status_facts(&self) -> Value {
        json!({
            "instance": self.instance,
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "draining": self.draining,
            "paused": self.paused,
            "model": self.model,
            "version": crate::VERSION,
            "skills": self.skills.names(),
            "skill_prefix": self.skills.prefix,
            "values": self.status_values(),
        })
    }

    fn counters_value(&self) -> Value {
        json!({"turns": self.counters.turns, "tool_calls": self.counters.tool_calls, "runs_started": self.counters.runs_started, "runs_finished": self.counters.runs_finished, "tokens_in": self.counters.tokens_in, "tokens_out": self.counters.tokens_out})
    }

    /// The conversations as `status` and the feed show them, each with the
    /// `contextId` its owner addresses it by beside the `id` it is kept under
    /// (the same, unless the owner is not an operator).
    fn conversation_views(&self) -> Vec<Value> {
        self.contexts
            .status()
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// `observability.status_values`: the current value of each listed memory
    /// key, for a client's `memory:<key>` chrome item. A key never written, or
    /// whose TTL has run out, is left out — an empty slot reads as broken,
    /// and an expired value still showing reads as current when the workflow
    /// that kept it fresh has stopped.
    ///
    /// Resolved at most once a second: `status` is polled, and the feed asks
    /// four times a second. Straight to the store rather than through
    /// `Memory`, which caches behind `&mut` — a read for display must not be
    /// able to disturb anything.
    pub(crate) fn status_values(&self) -> Value {
        const FRESH_FOR: Duration = Duration::from_secs(1);
        let keys = &self.settings.observability.status_values;
        if keys.is_empty() {
            return json!({});
        }
        let mut cache = self
            .status_values_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The keys are part of the cache's identity: a reload that changes
        // the list is answered for the new list at once.
        if let Some((at, cached_keys, v)) = &*cache
            && at.elapsed() < FRESH_FOR
            && cached_keys == keys
        {
            return v.clone();
        }
        let now = now_ms();
        let mut out = serde_json::Map::new();
        for key in keys {
            if let Ok(Some(env)) = self.durable.get(Kind::Memory, key)
                && let Ok(rec) = serde_json::from_value::<crate::context::memory::Record>(env.state)
                && !rec.expired(now)
            {
                out.insert(key.clone(), rec.value);
            }
        }
        let v = Value::Object(out);
        *cache = Some((Instant::now(), keys.clone(), v.clone()));
        v
    }

    /// The status document for whoever a model's tool call or a turn acts
    /// for: the principal's own view ([`Runtime::status_value_for`]) when
    /// someone asked, the whole document when the runtime itself did.
    pub(crate) fn status_for(&self, principal: Option<&str>) -> Value {
        #[cfg(feature = "a2a")]
        if let Some(p) = self.acting_principal(principal) {
            return self.status_value_for(&p);
        }
        #[cfg(not(feature = "a2a"))]
        let _ = principal;
        self.status_value()
    }

    /// The status document `principal` may read.
    ///
    /// The operator reads it all. Anyone else gets the instance's facts, the
    /// workflows it may run, and — from [`Runtime::status_items`], through
    /// the one visibility rule the feed applies — its own runs,
    /// conversations and activity. Every other principal's work, and the
    /// instance's internals (subagents, children, timers, budget, counters,
    /// the store, the instruction), stay out: `status` is granted to every
    /// named caller, so what it returns is exactly what may be shown to any
    /// of them.
    #[cfg(feature = "a2a")]
    pub(crate) fn status_value_for(&self, principal: &crate::a2a::Principal) -> Value {
        if principal.is_operator() {
            return self.status_value();
        }
        let items = self.status_items();
        let section = |kind: &str| -> Vec<Value> {
            items
                .iter()
                .filter(|i| i.kind == kind && i.vis.admits(&principal.id, false))
                .map(|i| i.data.clone())
                .collect()
        };
        let mut doc = self.status_facts();
        doc["workflows"] = json!(
            self.workflows
                .values()
                .filter(|w| Runtime::may_run(principal, w))
                .map(|w| json!({"name": w.name}))
                .collect::<Vec<_>>()
        );
        doc["runs"] = json!(section("run"));
        doc["conversations"] = json!(section("conversation"));
        doc["activity"] = json!(section("activity"));
        doc
    }

    /// Every item of the status document that belongs to somebody, tagged
    /// with who may see it: runs and conversations (their owner's), subagents
    /// and OS children (the operator's), the activity of each unit at work
    /// (its task owner's) and the slim status (the operator's).
    ///
    /// The feed's section diff and the `status` op both read these, so what
    /// a principal can watch and what it can poll come from one list with
    /// one tag per item.
    #[cfg(feature = "a2a")]
    pub(crate) fn status_items(&self) -> Vec<StatusItem> {
        use super::a2a_server::FeedVis;
        let mut items = Vec::new();
        for (id, r) in &self.runs {
            items.push(StatusItem {
                key: format!("run:{id}"),
                kind: "run",
                vis: FeedVis::Owner(r.principal.clone()),
                data: r.summary(),
            });
        }
        for c in self.conversation_views() {
            let id = c["id"].as_str().unwrap_or("").to_string();
            let owner = c["principal"].as_str().map(str::to_string);
            items.push(StatusItem {
                key: format!("conv:{id}"),
                kind: "conversation",
                vis: FeedVis::Owner(owner),
                data: c,
            });
        }
        for (h, s) in &self.subagents {
            items.push(StatusItem {
                key: format!("sub:{h}"),
                kind: "subagent",
                vis: FeedVis::Operator,
                data: json!({"handle": s.handle, "mode": s.mode, "status": s.status, "tokens": s.tokens, "error": s.error, "updated": s.updated}),
            });
        }
        for c in self.children.status().as_array().into_iter().flatten() {
            let node = c["node"].as_u64().unwrap_or(0);
            items.push(StatusItem {
                key: format!("child:{node}"),
                kind: "child",
                vis: FeedVis::Operator,
                data: c.clone(),
            });
        }
        for a in self.activity_value().as_array().into_iter().flatten() {
            let id = a["id"].as_str().unwrap_or("").to_string();
            items.push(StatusItem {
                key: format!("activity:{id}"),
                kind: "activity",
                vis: self.activity_vis(a["task"].as_str()),
                data: a.clone(),
            });
        }
        items.push(StatusItem {
            key: "status".into(),
            kind: "status",
            vis: FeedVis::Operator,
            data: json!({
                "instance": self.instance,
                "model": self.model,
                "version": crate::VERSION,
                "draining": self.draining,
                "inbox_pending": self.inbox_queue.len(),
                "counters": self.counters_value(),
                "budget": self.governor.status(now_ms()),
                "store": {"kind": self.durable.store_kind(), "degraded": self.durable.is_degraded()},
                "values": self.status_values(),
                "skill_prefix": self.skills.prefix,
            }),
        });
        items
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

/// One item of the status document, tagged with who may see it: the unit
/// the feed diffs and the `status` op filters.
#[cfg(feature = "a2a")]
pub(crate) struct StatusItem {
    /// Stable across ticks (`run:<id>`, `conv:<id>`, …): the feed's mark key,
    /// whose part after the `:` a departure names.
    pub(crate) key: String,
    /// The feed event kind (`run`, `conversation`, …).
    pub(crate) kind: &'static str,
    pub(crate) vis: super::a2a_server::FeedVis,
    pub(crate) data: Value,
}

/// A one-line summary of a status document, counting only what the document
/// holds — so a caller shown its own runs is not told how many there are in
/// all.
pub(crate) fn status_summary(status: &Value) -> String {
    let mut parts = Vec::new();
    for (section, noun) in [
        ("runs", "runs"),
        ("subagents", "subagents"),
        ("conversations", "conversations"),
    ] {
        if let Some(a) = status[section].as_array() {
            parts.push(format!("{} {noun}", a.len()));
        }
    }
    if let Some(active) = status["budget"].get("active") {
        parts.push(format!("budget active: {active}"));
    }
    format!("Status: {}", parts.join(", "))
}

// ---- ownership: who may act on a run or a subagent -------------------------

/// May `principal` act on an object owned by `owner`?
///
/// An operator always may. Anyone else only on what their principal ID owns —
/// the ID and nothing else, never the credential or session that presented it,
/// so a principal keeps its runs and subagents across a re-login and a token's
/// expiry, and two sessions of one principal are one owner. An object nobody
/// owns is the operator's alone.
pub(crate) fn may_act_on(principal: &crate::a2a::Principal, owner: Option<&str>) -> bool {
    principal.is_operator() || owner == Some(principal.id.as_str())
}

/// The principal a subagent record works for: its own, else the owner of the
/// run that spawned it, else the owner of the conversation, else its parent
/// subagent's — the order in which `requested_by` narrows who asked. `None`
/// when the chain ends without anyone, which leaves the subagent to the
/// operator.
pub(crate) fn inherited_principal(
    subagents: &BTreeMap<String, SubagentRecord>,
    record: &SubagentRecord,
    run_owner: impl Fn(&str) -> Option<String>,
    ctx_owner: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let mut cur = record;
    // Each hop is a distinct record, so the chain ends within as many hops
    // as there are records — the bound only guards a cycle.
    for _ in 0..=subagents.len() {
        if let Some(p) = &cur.principal {
            return Some(p.clone());
        }
        let by = cur.requested_by.as_ref()?;
        if let Some(p) = by["run"].as_str().and_then(&run_owner) {
            return Some(p);
        }
        if let Some(p) = by["ctx"].as_str().and_then(&ctx_owner) {
            return Some(p);
        }
        cur = subagents.get(by["subagent"].as_str()?)?;
    }
    None
}

impl Runtime {
    /// The run `id`, if `principal` may act on it. Unknown and not-yours are
    /// one answer, so a caller cannot probe for other principals' run ids.
    pub(crate) fn owned_run(
        &self,
        principal: &crate::a2a::Principal,
        id: &str,
    ) -> Option<&RunState> {
        self.runs
            .get(id)
            .filter(|r| may_act_on(principal, r.principal.as_deref()))
    }

    /// The subagent `handle`, if `principal` may act on it — asked before
    /// anything that could tell "not running" or "not warm" from "not yours".
    pub(crate) fn owned_subagent(
        &self,
        principal: &crate::a2a::Principal,
        handle: &str,
    ) -> Option<&SubagentRecord> {
        self.subagents
            .get(handle)
            .filter(|s| may_act_on(principal, s.principal.as_deref()))
    }

    /// Who a model's tool call acts for, when it acts for somebody.
    ///
    /// `None` is the runtime itself: work no caller asked for (a schedule, a
    /// webhook, `identity.autonomous_as`), on whose behalf the tools check
    /// nothing, as they never did. A caller the index knows — the operator
    /// and every declared rule id from the rules in force, and any other
    /// caller seen since they last changed — is itself, with its role and
    /// grants. An id the index does NOT know — one named by its evidence
    /// alone, whose work was restored, or whose rule a reload removed —
    /// fails closed: it acts as nobody but that id, with no role, so it keeps
    /// what it owns and can reach nothing else, and is never mistaken for the
    /// operator.
    pub(crate) fn acting_principal(&self, id: Option<&str>) -> Option<crate::a2a::Principal> {
        let id = id?;
        if let Some(p) = self.principal_index.get(id) {
            return Some(p.clone());
        }
        if id == self.settings.identity.autonomous_id() {
            return None;
        }
        Some(crate::a2a::Principal {
            id: id.to_string(),
            ..crate::a2a::Principal::anonymous()
        })
    }

    /// Whether the tool call `caller` may act on subagent `handle`.
    pub(crate) fn tool_owns_subagent(
        &self,
        caller: &super::tools::ToolCaller,
        handle: &str,
    ) -> bool {
        match self.acting_principal(caller.principal.as_deref()) {
            None => true,
            Some(p) => self.owned_subagent(&p, handle).is_some(),
        }
    }

    /// The principal a new subagent `record` works for: the spawning caller's
    /// own when it has one, else whoever owns what the caller is part of.
    pub(crate) fn spawn_principal(&self, record: &SubagentRecord) -> Option<String> {
        inherited_principal(
            &self.subagents,
            record,
            |r| self.runs.get(r).and_then(|r| r.principal.clone()),
            |c| self.contexts.get(c).and_then(|c| c.principal.clone()),
        )
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

#[cfg(test)]
mod tests {
    use super::{Instruction, may_act_on, status_summary};
    use crate::a2a::Principal;
    use crate::config::settings::Role;
    use serde_json::json;

    fn principal(id: &str, role: Role, grants: &[&str], rate: Option<&str>) -> Principal {
        Principal {
            id: id.into(),
            role,
            grants: grants.iter().map(|g| (*g).to_string()).collect(),
            rate: rate.map(str::to_string),
            ..Principal::anonymous()
        }
    }

    /// Ownership is the principal ID and nothing else. The same person can
    /// arrive as two `Principal` values — a second session, a re-login after
    /// a reload changed their grants or rate — and both must own what either
    /// started; a different ID owns nothing of theirs, whatever it holds.
    /// `owned_run` and `owned_subagent` are this predicate over the record's
    /// owner, and the A2A ops answer its `false` with -32001.
    #[test]
    fn ownership_keys_on_the_principal_id() {
        let first = principal("user:alice", Role::User, &[], None);
        let again = Principal {
            labels: [("team".to_string(), "ops".to_string())].into(),
            ..principal("user:alice", Role::User, &["*"], Some("5/1s"))
        };
        for owner in [&first, &again] {
            for asker in [&first, &again] {
                assert!(
                    may_act_on(asker, Some(owner.id.as_str())),
                    "{asker:?} owns what {owner:?} started"
                );
            }
        }
        let bob = principal("user:bob", Role::User, &["*"], None);
        assert!(!may_act_on(&bob, Some("user:alice")), "not bob's");
        // Holding the owner's grants, or a role that is not operator, is no
        // ownership; the operator is the one role that acts on anything.
        let agent_alice = principal("agent:alice", Role::Agent, &["*"], None);
        assert!(!may_act_on(&agent_alice, Some("user:alice")));
        let operator = principal("operator", Role::Operator, &[], None);
        assert!(may_act_on(&operator, Some("user:alice")));
        // An object nobody owns is the operator's alone.
        assert!(may_act_on(&operator, None));
        assert!(!may_act_on(&first, None));
    }

    fn instruction(uri: Option<&str>, server: Option<&str>) -> Instruction {
        Instruction {
            text: "x".into(),
            source: "resource",
            uri: uri.map(str::to_string),
            server: server.map(str::to_string),
            version: 1,
            version_id: None,
            delivered_digest: None,
        }
    }

    #[test]
    fn a_served_instruction_re_reads_from_the_server_that_served_it() {
        assert_eq!(
            instruction(Some("instruction://ins_x@stable"), Some("registry")).source_ref(),
            Some("mcp://registry/instruction://ins_x@stable".to_string())
        );
        assert_eq!(
            instruction(Some("oci://h/r:t"), None).source_ref(),
            Some("oci://h/r:t".to_string())
        );
        assert_eq!(instruction(None, None).source_ref(), None);
    }

    /// `instruction.subscribe` with no uri follows the instruction where it
    /// was served; a uri the caller names is followed as named.
    #[test]
    fn subscribe_without_a_uri_follows_the_serving_server() {
        let served = instruction(Some("instruction://ins_x@stable"), Some("a"));
        assert_eq!(
            served.subscribe_target(&serde_json::json!({})),
            Some("mcp://a/instruction://ins_x@stable".to_string())
        );
        assert_eq!(
            served.subscribe_target(&serde_json::json!({"uri": "file://other"})),
            Some("file://other".to_string())
        );
        assert_eq!(
            instruction(None, None).subscribe_target(&serde_json::json!({})),
            None
        );
    }

    /// The one-line status a turn answers with counts what the asker's
    /// document holds and nothing else: a section the caller may not read is
    /// not counted as zero, it is not mentioned at all.
    #[test]
    fn a_status_summary_counts_only_the_returned_arrays() {
        let scoped = json!({"runs": [{"id": "r1"}], "conversations": [{"id": "c1"}, {"id": "c2"}]});
        assert_eq!(status_summary(&scoped), "Status: 1 runs, 2 conversations");
        let full = json!({
            "runs": [], "subagents": [{"handle": "h"}], "conversations": [],
            "budget": {"active": true},
        });
        assert_eq!(
            status_summary(&full),
            "Status: 0 runs, 1 subagents, 0 conversations, budget active: true"
        );
    }
}
