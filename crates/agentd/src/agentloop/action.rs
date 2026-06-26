//! Self-tool dispatch. RFC 0005 §self-tools, RFC 0007.
//!
//! The agentic loop's tools come from connected MCP servers *plus* agentd's own
//! self-tools (`subagent.spawn`, …). A [`SelfHandler`] supplies those tool
//! definitions and handles their calls in-process — distinct from the MCP
//! dispatch path. This is the seam through which the model **self-orchestrates**:
//! it calls `subagent.spawn` to split its instruction into delegated child
//! agents (the supervisor enforces the caps + scope). RFC 0001 §self-orchestration.

use crate::wire::intel::ToolDef;
use serde_json::Value;

/// Provides agentd's in-process self-tools to the loop. The loop tries the
/// self-handler first; a `None` result means "not a self-tool — fall through to
/// MCP".
pub trait SelfHandler {
    /// The self-tool definitions to advertise to the model (added to the MCP
    /// catalogue).
    fn tools(&self) -> Vec<ToolDef>;

    /// Handle a tool call. Returns `Some((observation, is_error))` if `name` is
    /// one of this handler's self-tools; `None` to fall through to MCP.
    fn handle(&mut self, name: &str, args: &Value) -> Option<(String, bool)>;

    /// Drain any future wake-ups the agent scheduled for itself this run
    /// (RFC 0008 §self-scheduling). Default: none. The loop attaches these to
    /// the run's [`Outcome`](crate::agentloop::stop::Outcome) so a daemon
    /// supervisor can arm them.
    fn take_scheduled(&mut self) -> Vec<crate::agentloop::stop::ScheduleRequest> {
        Vec::new()
    }

    /// Drain any resource (un)subscriptions the agent requested for itself this
    /// run (RFC 0008). Default: none. Attached to the run's `Outcome`.
    fn take_subscriptions(&mut self) -> Vec<crate::agentloop::stop::SubscriptionRequest> {
        Vec::new()
    }
}

/// The default: no self-tools (used by the in-process once-mode loop, which
/// does not delegate). Subagents use a real `Orchestrator`.
pub struct NoopSelfHandler;

impl SelfHandler for NoopSelfHandler {
    fn tools(&self) -> Vec<ToolDef> {
        Vec::new()
    }
    fn handle(&mut self, _name: &str, _args: &Value) -> Option<(String, bool)> {
        None
    }
}
