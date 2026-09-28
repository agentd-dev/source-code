// SPDX-License-Identifier: AGPL-3.0-only
//! The command ops, as ONE table.
//!
//! Every property of an op — whether it answers with a Message or a Task, who
//! may call it, what switch serves it and which handler runs it — is a column
//! of [`OPS`], and everything else is derived from the table: the reserved
//! names a workflow may not claim, the ops the card and `params.ops` list, the
//! authorization floor, the audit mirror and the dispatch. The properties used
//! to live in five hand lists (the reserved set, the served set, the display
//! subset, the admin family and the role defaults), and they disagreed: an op
//! classified in one and missing from another is how `config.set` ended up
//! reachable by a `grants: ["*"]` user. A new op is one row plus its handler.

use crate::config::v2::{Role, Settings};
use serde_json::{Value, json};

/// How an op answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// A read: a Message carrying the document, and no Task — a display
    /// client polls these, and a durable task per poll filled the task list
    /// every client of the same principal enumerates.
    Message,
    /// Work: a Task tracks it.
    Task,
}

/// Who may call an op, before any grant is consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Floor {
    /// The operator role alone. A grant never reaches these: a principal
    /// that may drain the instance, relax its approval policy or read every
    /// principal's log lines is not a peer, it is an operator.
    Operator,
    /// Any named (non-anonymous) caller, with no grant.
    AnyNamed,
    /// The role's defaults ([`OpSpec::defaults`]) or an explicit grant.
    Granted,
}

/// Whose objects an op touches (ownership is enforced at the object).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The instance as a whole.
    Instance,
    /// The caller's own view of the instance.
    Caller,
    /// One object, which the caller must own.
    Owner,
    /// A workflow the caller may run.
    Workflow,
}

/// The setting that serves an op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Always,
    /// `a2a.introspection.enabled`.
    Introspection,
    /// `a2a.device_grant.enabled`.
    DeviceGrant,
    /// A listener is configured (`a2a.listen`).
    Listener,
}

/// The module that runs an op. The command dispatch is one `match` on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handler {
    Read,
    Workflow,
    Subagent,
    Admin,
    Introspection,
    Auth,
    Instance,
    /// Held so no workflow can claim the name, and served by nothing.
    Reserved,
}

/// One command op.
#[derive(Debug)]
pub struct OpSpec {
    /// The op, or — ending in `.` — a prefix every op under it shares.
    pub name: &'static str,
    pub reply: Reply,
    pub floor: Floor,
    /// The roles a [`Floor::Granted`] op is open to without a grant.
    pub defaults: &'static [Role],
    pub scope: Scope,
    pub gate: Gate,
    pub handler: Handler,
    /// One line, for the card's skill and the extension's schema. Never
    /// empty: the SDK drops an empty description, and the proto requires one.
    pub description: &'static str,
    pub args_schema: fn() -> Value,
    pub result_schema: fn() -> Value,
}

impl OpSpec {
    /// A prefix row: `_instance.` answers for every `_instance.<x>`.
    fn is_prefix(&self) -> bool {
        self.name.ends_with('.')
    }

    fn matches(&self, op: &str) -> bool {
        if self.is_prefix() {
            op.len() > self.name.len() && op.starts_with(self.name)
        } else {
            op == self.name
        }
    }
}

/// No published schema yet.
fn any_object() -> Value {
    json!({})
}

const USER: &[Role] = &[Role::User];
const USER_AGENT: &[Role] = &[Role::User, Role::Agent];

/// A row with the placeholder schemas.
#[allow(clippy::too_many_arguments)] // one per column, named at the row
const fn op(
    name: &'static str,
    reply: Reply,
    floor: Floor,
    defaults: &'static [Role],
    scope: Scope,
    gate: Gate,
    handler: Handler,
    description: &'static str,
) -> OpSpec {
    OpSpec {
        name,
        reply,
        floor,
        defaults,
        scope,
        gate,
        handler,
        description,
        args_schema: any_object,
        result_schema: any_object,
    }
}

use Floor::{AnyNamed, Granted, Operator};
use Reply::{Message, Task};

/// Every command op any build can serve, and the names held back from
/// workflows. Order is the order the card and `params.ops` list them in.
pub const OPS: &[OpSpec] = &[
    op(
        "status",
        Message,
        AnyNamed,
        &[],
        Scope::Caller,
        Gate::Always,
        Handler::Read,
        "Liveness and a snapshot of this instance: runs, subagents, conversations and budget",
    ),
    op(
        "config",
        Message,
        Operator,
        &[],
        Scope::Instance,
        Gate::Always,
        Handler::Read,
        "The effective configuration, credentials redacted",
    ),
    op(
        "workflow.run",
        Task,
        Granted,
        USER_AGENT,
        Scope::Workflow,
        Gate::Always,
        Handler::Workflow,
        "Start a workflow; the reply is the task it runs under",
    ),
    op(
        "workflow.status",
        Message,
        Granted,
        USER_AGENT,
        Scope::Owner,
        Gate::Always,
        Handler::Workflow,
        "The status of one run, or of every run the caller started",
    ),
    op(
        "workflow.cancel",
        Task,
        Granted,
        USER,
        Scope::Owner,
        Gate::Always,
        Handler::Workflow,
        "Cancel one run by id",
    ),
    op(
        "workflow.signal",
        Task,
        Granted,
        &[],
        Scope::Owner,
        Gate::Always,
        Handler::Workflow,
        "Deliver a signal a workflow is waiting on",
    ),
    op(
        "subagent.send",
        Task,
        Granted,
        USER,
        Scope::Owner,
        Gate::Always,
        Handler::Subagent,
        "Send a message to a warm subagent",
    ),
    op(
        "subagent.kill",
        Task,
        Granted,
        &[],
        Scope::Owner,
        Gate::Always,
        Handler::Subagent,
        "Stop a subagent",
    ),
    op(
        "subagent.status",
        Message,
        Granted,
        USER,
        Scope::Owner,
        Gate::Always,
        Handler::Subagent,
        "The status of one subagent, or of all of them",
    ),
    op(
        "plan.get",
        Message,
        Granted,
        USER,
        Scope::Owner,
        Gate::Always,
        Handler::Read,
        "One conversation's current plan and its progress",
    ),
    op(
        "conversation.get",
        Message,
        Granted,
        USER,
        Scope::Owner,
        Gate::Introspection,
        Handler::Introspection,
        "One conversation's transcript, message bodies included",
    ),
    op(
        "run.get",
        Message,
        Granted,
        USER,
        Scope::Owner,
        Gate::Introspection,
        Handler::Introspection,
        "One run with per-step status, timings, errors and output",
    ),
    op(
        "subagent.get",
        Message,
        Granted,
        USER,
        Scope::Owner,
        Gate::Introspection,
        Handler::Introspection,
        "One subagent's instruction, attempts, result and error",
    ),
    op(
        "debug.events",
        Message,
        Operator,
        &[],
        Scope::Instance,
        Gate::Introspection,
        Handler::Introspection,
        "A cursor read of the live log ring, across every principal",
    ),
    op(
        "admin.drain",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::Always,
        Handler::Admin,
        "Begin a graceful drain, then exit 0",
    ),
    op(
        "admin.pause",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::Always,
        Handler::Admin,
        "Hold the instance, or one run, at a safe boundary",
    ),
    op(
        "admin.resume",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::Always,
        Handler::Admin,
        "Clear a prior pause",
    ),
    op(
        "admin.cancel",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::Always,
        Handler::Admin,
        "Cancel one run by id, whoever started it",
    ),
    op(
        "admin.set",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::Always,
        Handler::Admin,
        "Set a runtime-settable path (agent.approval, a2a.introspection.enabled) until the next reload",
    ),
    op(
        "_instance.",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::Always,
        Handler::Instance,
        "reserved: a child instance reports to its parent; operator only; not listed in params.ops",
    ),
    op(
        "ask_human",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::Always,
        Handler::Reserved,
        "reserved: the model's tool for asking a person; not served over A2A",
    ),
];

/// Command ops that no longer exist, each with what replaces it. Refused BY
/// NAME wherever one can still be written — a principal's grant, a workflow's
/// `a2a` start command, a command sent at runtime — so an operator upgrading
/// reads the replacement rather than "unknown".
pub const REMOVED_OPS: &[(&str, &str)] = &[
    ("interface.info", "read the agent card and the `status` op"),
    ("config.set", "use admin.set {path, value}"),
    (
        "pairing.code",
        "pairing was replaced by the OAuth device authorization grant (a2a.device_grant); operators approve with auth.device.approve {user_code, as}",
    ),
    ("admin.lameduck", "use admin.drain"),
];

/// The release that removed the [`REMOVED_OPS`], as every refusal names it —
/// the same release as the removed configuration keys.
pub const OPS_REMOVED_IN: &str = crate::config::v2::KEYS_REMOVED_IN;

/// The replacement hint for a removed op.
pub fn removed_op(op: &str) -> Option<&'static str> {
    REMOVED_OPS.iter().find(|(n, _)| *n == op).map(|(_, h)| *h)
}

/// The paths `admin.set` may change. Everything else is the config file plus
/// a reload, so the operator's documents stay the one source of truth.
pub const RUNTIME_SETTABLE: &[&str] = &["agent.approval", "a2a.introspection.enabled"];

/// The row for `op`, exact names before prefixes.
pub fn op_spec(op: &str) -> Option<&'static OpSpec> {
    OPS.iter()
        .find(|s| !s.is_prefix() && s.matches(op))
        .or_else(|| OPS.iter().find(|s| s.is_prefix() && s.matches(op)))
}

/// Is `op` one of the built-in command ops — a row of [`OPS`], served or
/// merely reserved, the `_instance.` family included?
///
/// The built-in surface is RESERVED: a workflow's `a2a` start node may not
/// declare a command that collides with one, and the listener dispatches a
/// built-in to its own handler even if one somehow does. Both halves matter —
/// the ops carry their own authorization (`admin.*` is operator-only), and a
/// declared command takes the inbox path where that check does not run. A
/// workflow able to claim `admin.drain` could quietly shadow an operator's
/// drain control, and where the model may create workflows, that author is
/// the model. Every row counts, not just this instance's enabled subset:
/// reserving a name only while a switch is on would make the collision appear
/// the day it flipped.
pub fn is_builtin_op(op: &str) -> bool {
    op_spec(op).is_some()
}

/// Is `op` in the operator admin family (the `Admin` handler)?
pub fn is_admin_op(op: &str) -> bool {
    op_spec(op).is_some_and(|s| s.handler == Handler::Admin)
}

/// Does `op` answer to the operator role alone, whatever the grants say?
pub fn is_operator_floor(op: &str) -> bool {
    op_spec(op).is_some_and(|s| s.floor == Floor::Operator)
}

/// Is `op` a read — answered with a Message, creating no task?
pub fn is_read_op(op: &str) -> bool {
    op_spec(op).is_some_and(|s| s.reply == Reply::Message)
}

/// Is the switch that serves `spec` on?
fn gate_open(spec: &OpSpec, s: &Settings) -> bool {
    match spec.gate {
        Gate::Always => true,
        Gate::Introspection => s.a2a.introspection.enabled,
        Gate::DeviceGrant => s.a2a.device_grant.enabled,
        Gate::Listener => s.a2a.listen.is_some(),
    }
}

/// Does this instance answer `op` right now?
pub fn served(op: &str, s: &Settings) -> bool {
    op_spec(op).is_some_and(|spec| spec.handler != Handler::Reserved && gate_open(spec, s))
}

/// The rows that name an op a caller can ask for: not a prefix family, and
/// not merely reserved.
fn listable() -> impl Iterator<Item = &'static OpSpec> {
    OPS.iter()
        .filter(|s| !s.is_prefix() && s.handler != Handler::Reserved)
}

/// The command ops an instance serves, in ONE place: the agent card renders
/// them as skills, the command extension declares them, and `--capabilities`
/// reports them. Three views, one list — they cannot disagree (they did: the
/// manifest listed ops the card never mentioned).
pub fn command_ops_of(s: &Settings) -> Vec<&'static str> {
    listable()
        .filter(|spec| gate_open(spec, s))
        .map(|spec| spec.name)
        .collect()
}

/// Every op any instance can serve, whatever its switches — the vocabulary a
/// client may learn without authenticating, since it says what agentd CAN
/// answer and nothing about what this instance does.
pub fn static_vocabulary() -> Vec<&'static str> {
    listable().map(|spec| spec.name).collect()
}
