// SPDX-License-Identifier: Apache-2.0
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

/// The two classes of tool the agentic loop offers the model — the boundary that
/// keeps agentd honest to target principle 1 (tools come ONLY from registered MCP
/// servers) and principle 2 (no local code/command execution). EVERY tool in the
/// loop's catalogue is exactly one of these; there is no third "general capability
/// library".
///   * [`Mcp`](ToolClass::Mcp) — a tool discovered from a connected MCP server
///     (`tools/list`). Dispatched by routing the call BACK to its owning server
///     ([`dispatch_tool`](crate::agentloop::runner)); agentd never runs it locally.
///   * [`SelfControl`](ToolClass::SelfControl) — agentd's OWN orchestration
///     primitives (see [`SELF_CONTROL_TOOLS`]): delegation (`subagent.*`,
///     `a2a.delegate`), reactivity (root-only `schedule`/`subscribe`/`unsubscribe`),
///     and resource attention (`resource.read`). These are handled in-process by a
///     [`SelfHandler`] / the runner — NONE shells out. This is the named
///     "self/control" class: the agent's own control surface, structurally distinct
///     from the MCP task-tool catalogue (a different code path assembles each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// A tool from a connected MCP server; dispatched back to that server.
    Mcp,
    /// One of agentd's own self/control orchestration primitives; handled in-process.
    SelfControl,
}

/// The authoritative membership of the [`ToolClass::SelfControl`] class: every
/// self/control primitive name agentd may offer the model. The
/// [`SelfHandler`] advertises a depth-/feature-conditioned SUBSET of this set
/// (`a2a.delegate` only with peers; `schedule`/`subscribe`/`unsubscribe` only at
/// the root; the `subagent.*` delegation tools only within the depth budget), and
/// the runner adds `resource.read` when any resource is readable. A drift-guard
/// test asserts everything a handler can advertise is listed here — so a new
/// self-tool cannot silently escape the class boundary (and, by construction, this
/// set contains NO local-exec primitive: principle 2).
pub const SELF_CONTROL_TOOLS: &[&str] = &[
    "subagent.spawn",
    "subagent.status",
    "subagent.await",
    "schedule",
    "subscribe",
    "unsubscribe",
    "await_resource",
    "workflow.define",
    "workflow.patch",
    "workflow.run",
    "a2a.delegate",
    "resource.read",
];

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

    /// Read an `agentd://` self-resource (e.g. `agentd://subagent/<handle>` — an
    /// async child's completion). A `resource.read` for an `agentd://` URI routes
    /// here instead of to MCP. `Some((content, is_error))` if this handler serves
    /// the URI; `None` (the default) means it does not. RFC 0009 §async.
    fn read_resource(&mut self, _uri: &str) -> Option<(String, bool)> {
        None
    }

    /// Whether this handler exposes any `agentd://` self-resources — so the loop
    /// offers the `resource.read` tool even when no MCP resources exist. Default
    /// `false`.
    fn serves_self_resources(&self) -> bool {
        false
    }

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
