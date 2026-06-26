//! The self-orchestration handler: the `subagent.spawn` self-tool. RFC 0005
//! §self-tools, RFC 0009 §nesting, RFC 0001 §self-orchestration.
//!
//! This is where the model splits its instruction into delegated child agents.
//! When the loop sees a `subagent.spawn` call it routes here; the orchestrator
//! mints a child [`SpawnPayload`] (depth + 1, a narrowed scope, an inherited
//! intelligence config), enforces the caps (depth / breadth) — **refusing as a
//! tool result, never crashing** — and supervises the child synchronously by
//! reusing [`supervise_once`]. The child is a real OS process parented to this
//! one, so `PDEATHSIG` + the subreaper keep the nested tree in the reaping
//! domain (RFC 0003).
//!
//! v1 is **synchronous**: the parent's agentic loop blocks while the child
//! runs (its control thread keeps answering pings, so the grandparent sees it
//! as busy, not stuck). Async delegation lands in M3.
//!
//! Known v1 limitation: the tree-wide token ceiling is enforced per *local*
//! supervisor, not globally across the nested tree (each subagent bounds its
//! own children); per-child token/step/deadline limits still apply.

use crate::agentloop::action::SelfHandler;
use crate::agentloop::stop::{ScheduleRequest, SubscriptionAction, SubscriptionRequest};
use crate::config::McpServerSpec;
use crate::obs::log::Logger;
use crate::subagent::protocol::{AgentMsg, IntelConfig, Limits, SeedMessage, SpawnPayload, Telemetry};
use crate::supervisor::reactor::{supervise_once, SuperviseResult};
use crate::supervisor::spawn::{spawn, Subagent};
use crate::supervisor::tree::NodeId;
use crate::wire::intel::ToolDef;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Conservative per-node breadth cap (depth comes from the payload's
/// `max_depth`). RFC 0009.
const MAX_CHILDREN: u32 = 8;
/// Cap on the distilled result handed back to the parent model (~chars).
const DISTILL_CAP: usize = 8_000;
/// Cap on self-scheduled wake-ups per run (bounds a runaway self-scheduler).
const MAX_SCHEDULED: usize = 8;
/// Bounds on a self-scheduled delay (≥ 1s; ≤ 30 days so `now + delay` is safe).
const MIN_SCHEDULE_SECS: u64 = 1;
const MAX_SCHEDULE_SECS: u64 = 30 * 24 * 3600;
/// Cap on resource (un)subscriptions an agent may request per run.
const MAX_SUBSCRIPTIONS: usize = 16;
/// Poll granularity while awaiting an async child.
const AWAIT_POLL: Duration = Duration::from_millis(50);
/// Bounded `subagent.await` window: a single call blocks at most this long, then
/// hands control back ("still running") so the loop stays responsive to cancel.
const AWAIT_MAX: Duration = Duration::from_secs(30);

/// A backgrounded child from `subagent.spawn{async|detach}`: its handle's
/// process + control channel, and its distilled outcome once it reports a
/// terminal frame. The parent supervises it lazily — draining on status/await —
/// and `Subagent`'s Drop kills + reaps it when the orchestrator is dropped, so
/// no async child outlives the parent's tree (RFC 0009 §async).
struct AsyncChild {
    sub: Subagent,
    rx: Receiver<(NodeId, AgentMsg)>,
    /// `Some((distilled, is_error))` once a terminal `Result`/`Failed` (or the
    /// channel disconnecting) was seen.
    outcome: Option<(String, bool)>,
    detached: bool,
}

pub struct Orchestrator {
    exe: PathBuf,
    parent_depth: u32,
    parent_path: String,
    max_depth: u32,
    child_count: u32,
    intelligence: IntelConfig,
    mcp_servers: Vec<McpServerSpec>,
    run_id: String,
    trace_id: Option<String>,
    log_level: String,
    child_limits: Limits,
    enable_exec: bool,
    drain_timeout: Duration,
    /// Future wake-ups the root agent requested via `schedule` this run; drained
    /// into the run's `Outcome` for a daemon supervisor to arm (RFC 0008).
    scheduled: Vec<ScheduleRequest>,
    /// Resource (un)subscriptions the root agent requested this run (RFC 0008).
    subscriptions: Vec<SubscriptionRequest>,
    /// Backgrounded children from `subagent.spawn{async|detach}`, keyed by handle
    /// (= the child's agent_path). Drained on status/await; reaped on Drop.
    async_children: HashMap<String, AsyncChild>,
    log: Logger,
}

impl Orchestrator {
    /// Build from the running subagent's own payload. Children inherit its
    /// intelligence + (narrowable) MCP scope and carry depth + 1.
    pub fn from_payload(exe: PathBuf, payload: &SpawnPayload, drain_timeout: Duration, log: Logger) -> Orchestrator {
        Orchestrator {
            exe,
            parent_depth: payload.depth,
            parent_path: payload.telemetry.agent_path.clone(),
            max_depth: payload.limits.max_depth,
            child_count: 0,
            intelligence: payload.intelligence.clone(),
            mcp_servers: payload.mcp_servers.clone(),
            run_id: payload.telemetry.run_id.clone(),
            trace_id: payload.telemetry.trace_id.clone(),
            log_level: payload.telemetry.log_level.clone(),
            // Children inherit the parent's per-run bounds (v1).
            child_limits: payload.limits.clone(),
            // exec is inherited by children (scope only narrows; the Rule-of-Two
            // tag check is a later refinement).
            enable_exec: payload.enable_exec,
            drain_timeout,
            scheduled: Vec::new(),
            subscriptions: Vec::new(),
            async_children: HashMap::new(),
            log,
        }
    }

    /// Request a resource (un)subscription for the daemon to apply after this
    /// run (RFC 0008). Bounded; refused-as-tool-result, never crashes.
    fn subscription(&mut self, action: SubscriptionAction, args: &Value) -> (String, bool) {
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            return refused("maximum self-subscription changes reached for this run");
        }
        let uri = args.get("uri").and_then(Value::as_str).unwrap_or("").trim();
        if uri.is_empty() {
            return ("error: subscribe/unsubscribe requires a non-empty 'uri'".into(), true);
        }
        let verb = match action {
            SubscriptionAction::Subscribe => "subscribe",
            SubscriptionAction::Unsubscribe => "unsubscribe",
        };
        self.subscriptions.push(SubscriptionRequest { uri: uri.to_string(), action });
        self.log.info("self.subscribe", json!({"action": verb, "uri": uri}));
        (format!("requested: the daemon will {verb} {uri} after this run"), false)
    }

    /// Queue a future self-wake-up (RFC 0008 §self-scheduling). Bounded; refused
    /// as a tool result (never crashes). Effective only under a daemon — the
    /// requests ride out on the run's `Outcome`.
    fn schedule(&mut self, args: &Value) -> (String, bool) {
        if self.scheduled.len() >= MAX_SCHEDULED {
            return refused("maximum self-scheduled wake-ups reached for this run");
        }
        let after = match args.get("after_seconds").and_then(Value::as_u64) {
            Some(s) if (MIN_SCHEDULE_SECS..=MAX_SCHEDULE_SECS).contains(&s) => s,
            _ => {
                return (
                    format!("error: schedule needs 'after_seconds' in {MIN_SCHEDULE_SECS}..={MAX_SCHEDULE_SECS}"),
                    true,
                );
            }
        };
        let instruction = args.get("instruction").and_then(Value::as_str).unwrap_or("").trim();
        if instruction.is_empty() {
            return ("error: schedule requires a non-empty 'instruction'".into(), true);
        }
        self.scheduled
            .push(ScheduleRequest { after_ms: after.saturating_mul(1000), instruction: instruction.to_string() });
        self.log.info("self.schedule", json!({"after_s": after, "queued": self.scheduled.len()}));
        (format!("scheduled: a wake-up in {after}s will run the given instruction"), false)
    }

    fn can_nest(&self) -> bool {
        // A child would be at `parent_depth + 1`, which must be ≤ max_depth.
        self.parent_depth < self.max_depth
    }

    fn spawn(&mut self, args: &Value) -> (String, bool) {
        // Caps — refused as a tool result so the model adapts (RFC 0009).
        if !self.can_nest() {
            return refused("maximum subagent depth reached; do this step yourself");
        }
        if self.child_count >= MAX_CHILDREN {
            return refused("maximum number of child subagents reached for this agent");
        }
        let instruction = args.get("instruction").and_then(Value::as_str).unwrap_or("").trim();
        if instruction.is_empty() {
            return ("error: subagent.spawn requires a non-empty 'instruction'".into(), true);
        }

        let output_contract = str_arg(args, "output_contract");
        let context_seed = str_arg(args, "context")
            .map(|c| vec![SeedMessage { role: "user".into(), content: c }])
            .unwrap_or_default();
        let mcp_servers = self.narrow_servers(args);

        let idx = self.child_count;
        let child_path = format!("{}.{}", self.parent_path, idx);
        let payload = SpawnPayload {
            instruction: instruction.to_string(),
            output_contract,
            context_seed,
            intelligence: self.intelligence.clone(),
            mcp_servers,
            limits: self.child_limits.clone(),
            telemetry: Telemetry {
                run_id: self.run_id.clone(),
                agent_id: child_path.clone(),
                agent_path: child_path.clone(),
                trace_id: self.trace_id.clone(),
                log_level: self.log_level.clone(),
                // Content-capture policy flows down the tree (RFC 0010 §2.9).
                log_content: self.log.content_capture(),
            },
            depth: self.parent_depth + 1,
            enable_exec: self.enable_exec,
            warm: false, // a delegated subagent is a one-shot distilled subtask
        };
        let is_async = args.get("async").and_then(Value::as_bool).unwrap_or(false);
        let detach = args.get("detach").and_then(Value::as_bool).unwrap_or(false);
        self.child_count += 1;
        self.log.info(
            "subagent.delegate",
            json!({"child": child_path, "depth": payload.depth, "servers": payload.mcp_servers.len(), "async": is_async || detach}),
        );

        // Async / detach: spawn in the background and return a handle now (RFC
        // 0009 §async). Default is synchronous: block on the nested supervisor.
        if is_async || detach {
            return self.spawn_async(payload, child_path, detach);
        }

        match supervise_once(self.exe.clone(), &payload, self.drain_timeout, self.log.clone()) {
            Ok(SuperviseResult::Completed(outcome)) => (distill(&outcome.result), false),
            Ok(SuperviseResult::Failed(e)) => (format!("subagent failed: {e}"), true),
            Ok(SuperviseResult::Killed(r)) => (format!("subagent terminated ({r:?})"), true),
            Err(e) => (format!("subagent could not start: {e}"), true),
        }
    }

    /// Spawn a backgrounded child and return its handle immediately — the parent
    /// keeps working while it runs. The result is collected later via
    /// `subagent.await` / `subagent.status`; if never collected it is reaped when
    /// the orchestrator drops (no async child outlives the parent's tree).
    fn spawn_async(&mut self, payload: SpawnPayload, handle: String, detach: bool) -> (String, bool) {
        let (tx, rx) = mpsc::channel();
        let node = NodeId(u64::from(self.child_count));
        match spawn(&self.exe, &payload, node, tx) {
            Ok(sub) => {
                self.async_children.insert(handle.clone(), AsyncChild { sub, rx, outcome: None, detached: detach });
                self.log.info("subagent.spawn_async", json!({"handle": handle, "detach": detach}));
                if detach {
                    (format!("spawned detached subagent (handle={handle}); fire-and-forget — it runs independently and is reaped on completion"), false)
                } else {
                    let uri = crate::agentd_uri::subagent_uri(&handle);
                    (format!("spawned async subagent (handle={handle}); keep working, then get its result with subagent.await (waits for it) — or peek anytime with subagent.status / resource.read {uri} (all idempotent)"), false)
                }
            }
            Err(e) => {
                self.async_children.remove(&handle);
                (format!("subagent could not start: {e}"), true)
            }
        }
    }

    /// Pull any pending frames for one async child, recording its terminal
    /// outcome (idempotent once terminal).
    fn drain_child(child: &mut AsyncChild) {
        if child.outcome.is_some() {
            return;
        }
        loop {
            match child.rx.try_recv() {
                Ok((_, AgentMsg::Result { outcome })) => {
                    child.outcome = Some((distill(&outcome.result), false));
                    return;
                }
                Ok((_, AgentMsg::Failed { error })) => {
                    child.outcome = Some((format!("subagent failed: {error}"), true));
                    return;
                }
                Ok(_) => {} // Ready / Turn / Pong / Event / Usage — progress
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    child.outcome = Some(("subagent exited without a result".into(), true));
                    return;
                }
            }
        }
    }

    /// Idempotent peek at an async child: drain its channel and report the
    /// current state — "still running", the distilled result, or (for a detached
    /// child) a not-collectable notice. `None` if no such handle. Does **not**
    /// consume the handle, so status / await / `agentd://` reads of one child are
    /// all consistent and repeatable; the child is reaped at [`Drop`] (or via the
    /// breadth cap). Shared by `status`, `await_child`, and `read_resource`.
    fn peek_child(&mut self, handle: &str) -> Option<(String, bool)> {
        let child = self.async_children.get_mut(handle)?;
        if child.detached {
            return Some((
                format!("subagent {handle} was spawned detached (fire-and-forget); its result is not collectable"),
                false,
            ));
        }
        Self::drain_child(child);
        Some(match &child.outcome {
            None => (format!("subagent {handle} is still running"), false),
            Some((result, is_err)) => (result.clone(), *is_err),
        })
    }

    /// `subagent.status` — non-blocking, idempotent peek (see [`Self::peek_child`]).
    fn status(&mut self, args: &Value) -> (String, bool) {
        let handle = args.get("handle").and_then(Value::as_str).unwrap_or("").trim().to_string();
        self.peek_child(&handle).unwrap_or_else(|| (format!("error: no async subagent with handle '{handle}'"), true))
    }

    /// `subagent.await` — block (bounded by [`AWAIT_MAX`]) until the child
    /// finishes, then hand back its distilled result (idempotent — the handle is
    /// not consumed). On timeout returns "still running" so the loop regains
    /// control (await again).
    fn await_child(&mut self, args: &Value) -> (String, bool) {
        let handle = args.get("handle").and_then(Value::as_str).unwrap_or("").trim().to_string();
        match self.async_children.get(&handle) {
            None => return (format!("error: no async subagent with handle '{handle}'"), true),
            // A detached child is not awaitable (fire-and-forget).
            Some(c) if c.detached => return self.peek_child(&handle).unwrap(),
            Some(_) => {}
        }
        // (the immutable borrow above ends here; the loop below re-borrows mutably)
        let deadline = Instant::now() + AWAIT_MAX;
        loop {
            if let Some(child) = self.async_children.get_mut(&handle) {
                Self::drain_child(child);
                if let Some((result, is_err)) = &child.outcome {
                    return (result.clone(), *is_err);
                }
            }
            if Instant::now() >= deadline {
                return (format!("subagent {handle} is still running (awaited {}s); await again or check status", AWAIT_MAX.as_secs()), false);
            }
            std::thread::sleep(AWAIT_POLL);
        }
    }

    /// Narrow the child's MCP servers to the requested subset (if any). Names
    /// the parent doesn't have are dropped — scope only ever shrinks.
    fn narrow_servers(&self, args: &Value) -> Vec<McpServerSpec> {
        match args.get("servers").and_then(Value::as_array) {
            Some(names) => {
                let wanted: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                self.mcp_servers.iter().filter(|s| wanted.contains(&s.name.as_str())).cloned().collect()
            }
            None => self.mcp_servers.clone(),
        }
    }
}

impl SelfHandler for Orchestrator {
    fn tools(&self) -> Vec<ToolDef> {
        let mut t = Vec::new();
        // Advertise delegation only when there is depth budget to use it. The
        // async-collection tools ride alongside (status/await on returned handles).
        if self.can_nest() {
            t.push(spawn_tool_def());
            t.push(status_tool_def());
            t.push(await_tool_def());
        }
        // Self-scheduling + self-subscription are root-agent capabilities: a
        // nested child's request would be lost to its parent, which only
        // distills the child's result.
        if self.parent_depth == 0 {
            t.push(schedule_tool_def());
            t.push(subscribe_tool_def());
            t.push(unsubscribe_tool_def());
        }
        // The gated exec tool — only when --enable-exec was set (RFC 0012).
        if self.enable_exec {
            t.push(crate::sec::exec::tool_def());
        }
        t
    }

    fn handle(&mut self, name: &str, args: &Value) -> Option<(String, bool)> {
        match name {
            "subagent.spawn" => Some(self.spawn(args)),
            // Collection tools accept a handle regardless of depth budget — an
            // agent at its breadth cap still needs to await its in-flight children.
            "subagent.status" => Some(self.status(args)),
            "subagent.await" => Some(self.await_child(args)),
            "schedule" if self.parent_depth == 0 => Some(self.schedule(args)),
            "subscribe" if self.parent_depth == 0 => {
                Some(self.subscription(SubscriptionAction::Subscribe, args))
            }
            "unsubscribe" if self.parent_depth == 0 => {
                Some(self.subscription(SubscriptionAction::Unsubscribe, args))
            }
            "exec" if self.enable_exec => {
                Some(crate::sec::exec::handle_call(args, crate::sec::exec::DEFAULT_TIMEOUT))
            }
            _ => None,
        }
    }

    fn read_resource(&mut self, uri: &str) -> Option<(String, bool)> {
        // `agentd://subagent/<handle>` reads an async child's completion as a
        // resource (completion-as-self-resource, RFC 0009) — the same idempotent
        // peek as subagent.status (a detached child is not collectable).
        match crate::agentd_uri::AgentdResource::parse(uri) {
            Some(crate::agentd_uri::AgentdResource::Subagent(handle)) => {
                Some(self.peek_child(&handle).unwrap_or_else(|| (format!("no async subagent with handle '{handle}'"), true)))
            }
            // agentd://status is a served-only resource; not served from a subagent.
            _ => None,
        }
    }

    fn serves_self_resources(&self) -> bool {
        // Async children (and thus agentd://subagent/<handle> resources) are
        // possible exactly when delegation is.
        self.can_nest()
    }

    fn take_scheduled(&mut self) -> Vec<ScheduleRequest> {
        std::mem::take(&mut self.scheduled)
    }

    fn take_subscriptions(&mut self) -> Vec<SubscriptionRequest> {
        std::mem::take(&mut self.subscriptions)
    }
}

impl Drop for Orchestrator {
    fn drop(&mut self) {
        // Async children that were never collected die with the parent — no
        // orphan outlives the tree (each `Subagent` reaps on its own Drop). Just
        // record what was force-reaped (and how many were detached).
        if !self.async_children.is_empty() {
            let detached = self.async_children.values().filter(|c| c.detached).count();
            self.log.info(
                "subagent.async_reaped",
                json!({"uncollected": self.async_children.len(), "detached": detached}),
            );
        }
    }
}

fn refused(why: &str) -> (String, bool) {
    (format!("subagent.spawn refused: {why}"), true)
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn distill(result: &Value) -> String {
    let s = match result {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    if s.len() > DISTILL_CAP {
        format!("{}… [truncated]", &s[..DISTILL_CAP])
    } else {
        s
    }
}

fn spawn_tool_def() -> ToolDef {
    ToolDef {
        name: "subagent.spawn".into(),
        description: "Delegate a focused subtask to a fresh child agent. By DEFAULT the call BLOCKS \
            until the child finishes and returns its distilled result. Pass async=true to instead \
            get a handle back immediately and keep working — then collect the result later with \
            subagent.await (blocks) or subagent.status (peek). Pass detach=true for fire-and-forget \
            (you will not collect a result). Give a clear 'instruction' and (strongly recommended) \
            an 'output_contract' stating exactly what the child should return. Optionally pass \
            'context' (only the facts the child needs — it does not see your conversation) and \
            'servers' (a subset of tool-server names to grant it). Use async to run independent \
            subtasks in parallel."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "instruction": {"type": "string", "description": "the subtask for the child agent"},
                "output_contract": {"type": "string", "description": "exactly what the child should return"},
                "context": {"type": "string", "description": "only the facts the child needs"},
                "servers": {"type": "array", "items": {"type": "string"}, "description": "subset of MCP server names to grant"},
                "async": {"type": "boolean", "description": "return a handle immediately instead of blocking (collect later)"},
                "detach": {"type": "boolean", "description": "fire-and-forget; do not collect a result"}
            },
            "required": ["instruction"]
        }),
    }
}

fn status_tool_def() -> ToolDef {
    ToolDef {
        name: "subagent.status".into(),
        description: "Check on an async child you spawned (by 'handle'). Returns 'still running', or \
            — once it has finished — its distilled result (after which the handle is consumed). \
            Non-blocking."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"handle": {"type": "string", "description": "the handle returned by subagent.spawn async=true"}},
            "required": ["handle"]
        }),
    }
}

fn await_tool_def() -> ToolDef {
    ToolDef {
        name: "subagent.await".into(),
        description: "Wait for an async child you spawned (by 'handle') to finish and return its \
            distilled result. Blocks until it completes; if it is taking a while it returns 'still \
            running' so you can do other work and await again."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"handle": {"type": "string", "description": "the handle returned by subagent.spawn async=true"}},
            "required": ["handle"]
        }),
    }
}

fn schedule_tool_def() -> ToolDef {
    ToolDef {
        name: "schedule".into(),
        description: "Schedule a future wake-up of yourself. After 'after_seconds' have elapsed, \
            agentd re-invokes you with 'instruction' as a fresh run. Use it to defer work, poll a \
            slow resource later, or set your own next tick instead of blocking. Effective only when \
            agentd runs as a long-lived daemon (reactive/loop/schedule); ignored in a one-shot run."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "after_seconds": {"type": "integer", "description": "delay in seconds before the wake-up fires"},
                "instruction": {"type": "string", "description": "what the woken agent should do"}
            },
            "required": ["after_seconds", "instruction"]
        }),
    }
}

fn subscribe_tool_def() -> ToolDef {
    ToolDef {
        name: "subscribe".into(),
        description: "Subscribe yourself to an MCP resource by uri. When that resource changes, \
            agentd wakes you with its current content — so you can watch something and react to it \
            later instead of polling. Effective only when agentd runs as a long-lived daemon."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"uri": {"type": "string", "description": "the resource uri to watch"}},
            "required": ["uri"]
        }),
    }
}

fn unsubscribe_tool_def() -> ToolDef {
    ToolDef {
        name: "unsubscribe".into(),
        description: "Stop watching an MCP resource you previously subscribed to (by uri). Use this \
            when you no longer need to react to its changes."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {"uri": {"type": "string", "description": "the resource uri to stop watching"}},
            "required": ["uri"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::log::{Comp, Level, LogCtx};

    fn logger() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Agent,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }

    fn payload(depth: u32, max_depth: u32) -> SpawnPayload {
        SpawnPayload {
            instruction: "parent".into(),
            output_contract: None,
            context_seed: Vec::new(),
            intelligence: IntelConfig { uri: "unix:/x".into(), token: None, model: None },
            mcp_servers: vec![
                McpServerSpec { name: "fs".into(), command: vec!["a".into()], tags: Vec::new() },
                McpServerSpec { name: "db".into(), command: vec!["b".into()], tags: Vec::new() },
            ],
            limits: Limits { max_steps: 10, max_tokens: 1000, deadline_ms: 1000, max_depth },
            telemetry: Telemetry {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                trace_id: None,
                log_level: "error".into(),
                log_content: false,
            },
            depth,
            enable_exec: false,
            warm: false,
        }
    }

    #[test]
    fn exec_tool_is_gated_by_enable_exec() {
        let mut p = payload(0, 4);
        p.enable_exec = false;
        let mut o = Orchestrator::from_payload("agentd".into(), &p, Duration::from_secs(5), logger());
        assert!(!o.tools().iter().any(|t| t.name == "exec"), "exec must be off by default");
        assert!(o.handle("exec", &json!({"argv": ["/bin/true"]})).is_none(), "exec must not run when disabled");

        p.enable_exec = true;
        let o = Orchestrator::from_payload("agentd".into(), &p, Duration::from_secs(5), logger());
        assert!(o.tools().iter().any(|t| t.name == "exec"), "exec advertised when enabled");
    }

    #[test]
    fn refuses_when_at_max_depth() {
        // depth 4, max_depth 4 → can't nest (child would be depth 5).
        let mut o = Orchestrator::from_payload("agentd".into(), &payload(4, 4), Duration::from_secs(5), logger());
        assert!(o.tools().is_empty());
        let (msg, is_err) = o.spawn(&json!({"instruction": "x"}));
        assert!(is_err);
        assert!(msg.contains("depth"));
    }

    #[test]
    fn advertises_tool_with_depth_budget() {
        // The root (depth 0) advertises delegation + self-scheduling + self-subscribe.
        let o = Orchestrator::from_payload("agentd".into(), &payload(0, 4), Duration::from_secs(5), logger());
        let tools = o.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"subagent.spawn"));
        assert!(names.contains(&"schedule"));
        assert!(names.contains(&"subscribe"));
        assert!(names.contains(&"unsubscribe"));
    }

    #[test]
    fn subscribe_is_root_only_and_accumulates() {
        // A nested child does not get the self-subscription tools.
        let mut child = Orchestrator::from_payload("agentd".into(), &payload(1, 4), Duration::from_secs(5), logger());
        assert!(!child.tools().iter().any(|t| t.name == "subscribe"));
        assert!(child.handle("subscribe", &json!({"uri": "file:///x"})).is_none());

        // The root accumulates subscribe/unsubscribe requests, drained by take.
        let mut root = Orchestrator::from_payload("agentd".into(), &payload(0, 4), Duration::from_secs(5), logger());
        assert!(!root.handle("subscribe", &json!({"uri": "file:///watch"})).unwrap().1);
        assert!(!root.handle("unsubscribe", &json!({"uri": "file:///old"})).unwrap().1);
        assert!(root.handle("subscribe", &json!({"uri": "  "})).unwrap().1, "empty uri → error");
        let drained = root.take_subscriptions();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].uri, "file:///watch");
        assert_eq!(drained[0].action, SubscriptionAction::Subscribe);
        assert_eq!(drained[1].action, SubscriptionAction::Unsubscribe);
        assert!(root.take_subscriptions().is_empty(), "take drains");
    }

    #[test]
    fn schedule_is_root_only_and_accumulates() {
        // A nested child (depth 1) does NOT get `schedule` (its request would be
        // lost to the parent), and handle() declines it.
        let mut child = Orchestrator::from_payload("agentd".into(), &payload(1, 4), Duration::from_secs(5), logger());
        assert!(!child.tools().iter().any(|t| t.name == "schedule"));
        assert!(child.handle("schedule", &json!({"after_seconds": 5, "instruction": "x"})).is_none());

        // The root accumulates valid requests, drained by take_scheduled.
        let mut root = Orchestrator::from_payload("agentd".into(), &payload(0, 4), Duration::from_secs(5), logger());
        let (_m, err) = root.handle("schedule", &json!({"after_seconds": 30, "instruction": "poll again"})).unwrap();
        assert!(!err);
        let drained = root.take_scheduled();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].after_ms, 30_000);
        assert_eq!(drained[0].instruction, "poll again");
        assert!(root.take_scheduled().is_empty(), "take_scheduled drains");
    }

    #[test]
    fn schedule_validates_delay_and_instruction() {
        let mut o = Orchestrator::from_payload("agentd".into(), &payload(0, 4), Duration::from_secs(5), logger());
        // out-of-range / missing delay → error observation, nothing queued
        assert!(o.schedule(&json!({"after_seconds": 0, "instruction": "x"})).1);
        assert!(o.schedule(&json!({"instruction": "x"})).1);
        // empty instruction → error
        assert!(o.schedule(&json!({"after_seconds": 5, "instruction": "  "})).1);
        assert!(o.take_scheduled().is_empty());
        // cap is enforced
        for _ in 0..MAX_SCHEDULED {
            assert!(!o.schedule(&json!({"after_seconds": 5, "instruction": "x"})).1);
        }
        assert!(o.schedule(&json!({"after_seconds": 5, "instruction": "x"})).1, "over cap → refused");
        assert_eq!(o.take_scheduled().len(), MAX_SCHEDULED);
    }

    #[test]
    fn status_and_await_reject_unknown_handles() {
        let mut o = Orchestrator::from_payload("agentd".into(), &payload(0, 4), Duration::from_secs(5), logger());
        let (msg, err) = o.status(&json!({"handle": "0.7"}));
        assert!(err && msg.contains("no async subagent"), "status on an unknown handle errors: {msg}");
        let (msg, err) = o.await_child(&json!({"handle": "0.7"}));
        assert!(err && msg.contains("no async subagent"), "await on an unknown handle errors: {msg}");
    }

    #[test]
    fn spawn_schema_advertises_async_and_detach() {
        let def = spawn_tool_def();
        let props = &def.input_schema["properties"];
        assert!(props.get("async").is_some(), "spawn schema must offer async");
        assert!(props.get("detach").is_some(), "spawn schema must offer detach");
    }

    #[test]
    fn empty_instruction_is_rejected() {
        let mut o = Orchestrator::from_payload("agentd".into(), &payload(0, 4), Duration::from_secs(5), logger());
        let (_msg, is_err) = o.spawn(&json!({"instruction": "   "}));
        assert!(is_err);
    }

    #[test]
    fn narrow_servers_filters_to_subset() {
        let o = Orchestrator::from_payload("agentd".into(), &payload(0, 4), Duration::from_secs(5), logger());
        let narrowed = o.narrow_servers(&json!({"servers": ["fs", "ghost"]}));
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].name, "fs");
        // no subset → inherit all
        assert_eq!(o.narrow_servers(&json!({})).len(), 2);
    }

    #[test]
    fn distill_truncates() {
        let big = Value::String("x".repeat(DISTILL_CAP + 100));
        assert!(distill(&big).ends_with("[truncated]"));
        assert_eq!(distill(&Value::String("short".into())), "short");
    }
}
