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
use crate::config::McpServerSpec;
use crate::obs::log::Logger;
use crate::subagent::protocol::{IntelConfig, Limits, SeedMessage, SpawnPayload, Telemetry};
use crate::supervisor::reactor::{supervise_once, SuperviseResult};
use crate::wire::intel::ToolDef;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

/// Conservative per-node breadth cap (depth comes from the payload's
/// `max_depth`). RFC 0009.
const MAX_CHILDREN: u32 = 8;
/// Cap on the distilled result handed back to the parent model (~chars).
const DISTILL_CAP: usize = 8_000;

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
            log,
        }
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
            },
            depth: self.parent_depth + 1,
            enable_exec: self.enable_exec,
        };
        self.child_count += 1;
        self.log.info(
            "subagent.delegate",
            json!({"child": child_path, "depth": payload.depth, "servers": payload.mcp_servers.len()}),
        );

        match supervise_once(self.exe.clone(), &payload, self.drain_timeout, self.log.clone()) {
            Ok(SuperviseResult::Completed(outcome)) => (distill(&outcome.result), false),
            Ok(SuperviseResult::Failed(e)) => (format!("subagent failed: {e}"), true),
            Ok(SuperviseResult::Killed(r)) => (format!("subagent terminated ({r:?})"), true),
            Err(e) => (format!("subagent could not start: {e}"), true),
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
        // Advertise delegation only when there is depth budget to use it.
        if self.can_nest() {
            t.push(spawn_tool_def());
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
            "exec" if self.enable_exec => {
                Some(crate::sec::exec::handle_call(args, crate::sec::exec::DEFAULT_TIMEOUT))
            }
            _ => None,
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
        description: "Delegate a focused subtask to a fresh child agent that runs independently and \
            returns its result. Give a clear 'instruction' and (strongly recommended) an \
            'output_contract' stating exactly what the child should return. Optionally pass \
            'context' (only the facts the child needs — it does not see your conversation) and \
            'servers' (a subset of tool-server names to grant it). Use this to split a large task \
            into smaller independent pieces. The call blocks until the child finishes and returns \
            its distilled result."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "instruction": {"type": "string", "description": "the subtask for the child agent"},
                "output_contract": {"type": "string", "description": "exactly what the child should return"},
                "context": {"type": "string", "description": "only the facts the child needs"},
                "servers": {"type": "array", "items": {"type": "string"}, "description": "subset of MCP server names to grant"}
            },
            "required": ["instruction"]
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
                McpServerSpec { name: "fs".into(), command: vec!["a".into()] },
                McpServerSpec { name: "db".into(), command: vec!["b".into()] },
            ],
            limits: Limits { max_steps: 10, max_tokens: 1000, deadline_ms: 1000, max_depth },
            telemetry: Telemetry {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                trace_id: None,
                log_level: "error".into(),
            },
            depth,
            enable_exec: false,
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
        let o = Orchestrator::from_payload("agentd".into(), &payload(0, 4), Duration::from_secs(5), logger());
        let tools = o.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "subagent.spawn");
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
