// SPDX-License-Identifier: AGPL-3.0-only
//! Self-tool dispatch.
//!
//! The agentic loop's tools come from connected MCP servers *plus* agentd's own
//! self-tools (`subagent.spawn`, …). A [`SelfHandler`] supplies those tool
//! definitions and handles their calls in-process — distinct from the MCP
//! dispatch path. This is the seam through which the model **self-orchestrates**:
//! it calls `subagent.spawn` to split its instruction into delegated child
//! agents. The model only *asks*; the supervisor is what enforces the depth and
//! concurrency caps and narrows the child's scope, so a compromised model
//! cannot widen its own budget through this seam.

use crate::wire::intel::ToolDef;
use serde_json::Value;

/// The classes of tool the agentic loop offers the model. This boundary is what
/// keeps two invariants true: a task tool reaches the model ONLY by being
/// exported from a registered MCP server or registered in code by the embedder,
/// and nothing in the catalogue shells out to a local command. EVERY tool the
/// loop advertises is exactly one of these classes; there is no third "general
/// capability library" that could smuggle in an unaudited capability.
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
    /// A CODE-REGISTERED tool: native Rust the embedder registered via
    /// [`crate::tools::register`] — first-party by definition, dispatched
    /// in-process, and it WINS a name collision with a remote MCP tool, so a
    /// server cannot steal a registered tool's calls by claiming its name.
    Code,
}

/// The authoritative membership of the [`ToolClass::SelfControl`] class: every
/// self/control primitive name agentd may offer the model. The
/// [`SelfHandler`] advertises a depth-/feature-conditioned SUBSET of this set
/// (`a2a.delegate` only with peers; `schedule`/`subscribe`/`unsubscribe` only at
/// the root; the `subagent.*` delegation tools only within the depth budget), and
/// the runner adds `resource.read` when any resource is readable. A test asserts
/// that everything a handler can advertise appears in this list, so a new
/// self-tool cannot silently escape the class boundary. By construction the set
/// contains no local-execution primitive.
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
    /// here instead of to MCP. Returns `Some((content, is_error))` if this handler
    /// serves the URI; `None` (the default) means it does not, and the read falls
    /// through to MCP.
    fn read_resource(&mut self, _uri: &str) -> Option<(String, bool)> {
        None
    }

    /// Whether this handler exposes any `agentd://` self-resources — so the loop
    /// offers the `resource.read` tool even when no MCP resources exist. Default
    /// `false`.
    fn serves_self_resources(&self) -> bool {
        false
    }

    /// Drain any future wake-ups the agent scheduled for itself this run.
    /// Default: none. The loop attaches these to the run's
    /// [`Outcome`](crate::agentloop::stop::Outcome) so a daemon supervisor can
    /// arm them.
    fn take_scheduled(&mut self) -> Vec<crate::agentloop::stop::ScheduleRequest> {
        Vec::new()
    }

    /// Drain any resource (un)subscriptions the agent requested for itself this
    /// run. Default: none. Attached to the run's `Outcome`.
    fn take_subscriptions(&mut self) -> Vec<crate::agentloop::stop::SubscriptionRequest> {
        Vec::new()
    }
}
