// SPDX-License-Identifier: AGPL-3.0-only
//! The command ops, as ONE table.
//!
//! Every property of an op — whether it answers with a Message or a Task, who
//! may call it, what switch serves it and which handler runs it — is a column
//! of [`OPS`], and everything else is derived from the table: the reserved
//! names a workflow may not claim, the ops the card and `params.ops` list, the
//! authorization floor, the audit mirror and the dispatch. One table because
//! an op classified in one list and missing from another is an op the
//! authorization floor and the dispatch disagree about. A new op is one row
//! plus its handler.

use super::events::FeedKind;
use crate::a2a::errors::{self, reason};
use crate::config::settings::{DeviceScope, Role, Settings};
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
    /// A TCP listener is configured (`a2a.listen`, not `unix://`): the
    /// sessions a TCP listener holds are what these ops list and end, and a
    /// unix socket issues none.
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
            // The bare prefix too: `_instance.` names no op, but a workflow
            // that claimed it would take the declared-command path, where the
            // operator floor this row carries does not run.
            op.starts_with(self.name)
        } else {
            op == self.name
        }
    }
}

const USER: &[Role] = &[Role::User];
const USER_AGENT: &[Role] = &[Role::User, Role::Agent];

/// A row. Every column is named at the row, the schemas included: an op
/// without a published argument schema is an op a caller cannot be told how
/// to call, and the schema is also what [`check_command`] holds its arguments
/// to — so there is no default to forget.
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
    args_schema: fn() -> Value,
    result_schema: fn() -> Value,
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
        args_schema,
        result_schema,
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
        "Liveness and a snapshot of this instance, as the caller may see it: its runs, conversations and activity; an operator sees everything",
        no_args,
        status_result,
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
        no_args,
        config_result,
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
        workflow_run_args,
        workflow_run_result,
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
        workflow_status_args,
        workflow_status_result,
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
        run_args,
        no_result,
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
        workflow_signal_args,
        workflow_signal_result,
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
        subagent_send_args,
        subagent_send_result,
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
        subagent_kill_args,
        subagent_kill_result,
    ),
    op(
        "subagent.status",
        Message,
        Granted,
        USER,
        Scope::Owner,
        Gate::Always,
        Handler::Subagent,
        "The status of one subagent, and its result once it has one",
        subagent_status_args,
        subagent_status_result,
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
        plan_get_args,
        plan_get_result,
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
        conversation_get_args,
        conversation_get_result,
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
        run_args,
        run_get_result,
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
        handle_args,
        subagent_get_result,
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
        debug_events_args,
        debug_events_result,
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
        admin_drain_args,
        admin_drain_result,
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
        admin_pause_args,
        admin_pause_result,
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
        admin_resume_args,
        admin_resume_result,
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
        admin_cancel_args,
        admin_cancel_result,
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
        admin_set_args,
        admin_set_result,
    ),
    op(
        "auth.device.pending",
        Message,
        Operator,
        &[],
        Scope::Instance,
        Gate::DeviceGrant,
        Handler::Auth,
        "Device sign-ins waiting for an operator's decision",
        no_args,
        device_pending_result,
    ),
    op(
        "auth.device.approve",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::DeviceGrant,
        Handler::Auth,
        "Approve a device sign-in {user_code, as, scope?}; every session approved as one name is one principal",
        device_approve_args,
        device_approve_result,
    ),
    op(
        "auth.device.deny",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::DeviceGrant,
        Handler::Auth,
        "Refuse a pending device sign-in {user_code}, or all of them {all: true}",
        device_deny_args,
        device_deny_result,
    ),
    op(
        "auth.sessions",
        Message,
        Operator,
        &[],
        Scope::Instance,
        Gate::Listener,
        Handler::Auth,
        "The signed-in sessions: kind, name, principal and expiry",
        no_args,
        sessions_result,
    ),
    op(
        "auth.sessions.revoke",
        Task,
        Operator,
        &[],
        Scope::Instance,
        Gate::Listener,
        Handler::Auth,
        "End one session {sid}, every session of a name {name}, or all {all: true}",
        sessions_revoke_args,
        sessions_revoke_result,
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
        instance_family_args,
        instance_family_result,
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
        never,
        never,
    ),
];

/// The names `auth.device.approve` refuses as `as`: each is already how the
/// audit trail and the labels spell someone else — the operator, a caller
/// nobody named, the launcher, the runtime acting on its own — so a device
/// approved as one would read as that party in every line it caused. The one
/// list the approval handler and the op's published argument schema read.
pub const RESERVED_APPROVAL_NAMES: &[&str] = &[
    "operator",
    "anonymous",
    "unknown",
    "launcher",
    "runtime",
    "system",
];

/// The shape of an approval name, as a pattern: lowercase, so two spellings
/// never name two principals, and without `=` or `:`, so it can never spell a
/// certificate-derived id or another role's. [`approval_name_ok`] is this
/// pattern, checked without a regex engine; a client that checks a name before
/// sending it holds its copy to this string.
pub const APPROVAL_NAME_PATTERN: &str = "^[a-z0-9][a-z0-9._-]{0,63}$";

/// Whether `name` matches [`APPROVAL_NAME_PATTERN`] — its shape alone; the
/// reserved names are a separate refusal with its own reason.
pub fn approval_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    let first = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=64).contains(&b.len())
        && first(b[0])
        && b[1..]
            .iter()
            .all(|&c| first(c) || matches!(c, b'.' | b'_' | b'-'))
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

/// Does `op` answer to the operator role alone, whatever the grants say?
pub fn is_operator_floor(op: &str) -> bool {
    op_spec(op).is_some_and(|s| s.floor == Floor::Operator)
}

/// Is `op` a read — answered with a Message, creating no task?
pub fn is_read_op(op: &str) -> bool {
    op_spec(op).is_some_and(|s| s.reply == Reply::Message)
}

/// Is the switch that serves `spec` on? The dispatch asks this of every row
/// before its handler runs, so a row behind a closed switch is refused in one
/// place rather than by each handler remembering to check.
pub fn gate_open(spec: &OpSpec, s: &Settings) -> bool {
    match spec.gate {
        Gate::Always => true,
        Gate::Introspection => s.a2a.introspection.enabled,
        Gate::DeviceGrant => s.a2a.device_grant.enabled,
        Gate::Listener => s
            .a2a
            .listen
            .as_deref()
            .is_some_and(|l| crate::config::unix_socket_path(l).is_none()),
    }
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

// ── The reserved `_instance.` family ─────────────────────────────────────────

/// One member of a prefix family: an op the family's row answers for, with
/// the schemas of its own arguments and result.
#[derive(Debug)]
pub struct Member {
    pub name: &'static str,
    pub description: &'static str,
    pub args_schema: fn() -> Value,
    pub result_schema: fn() -> Value,
}

/// The reports a child instance sends its parent — the members of the
/// `_instance.` row, and the only ones it serves.
///
/// Reserved, not listed: they are the child-to-parent protocol, operator
/// only, and no peer is meant to call them, so they are never in
/// `params.ops`. They are PUBLISHED all the same, under the bundle's
/// `$defs.reserved`, because the listener accepts them on the public
/// endpoint, and an op the endpoint answers without saying what it takes is
/// an op nobody can audit.
pub const INSTANCE_OPS: &[Member] = &[
    Member {
        name: "_instance.result",
        description: "reserved: a `mode: sync` child's first result, recorded on the parent's handle; operator only",
        args_schema: instance_result_args,
        result_schema: no_result,
    },
    Member {
        name: "_instance.emit",
        description: "reserved: one event of a child's `mirror_streams` stream, appended to the parent's stream of the same name; operator only",
        args_schema: instance_emit_args,
        result_schema: no_result,
    },
];

// ── Argument and result schemas ──────────────────────────────────────────────
//
// Written from the handler code (commands.rs, admin.rs, introspection.rs,
// auth_ops.rs, instances.rs), not from what a client happens to send. An
// argument schema is closed: a name the handler does not read is refused
// rather than ignored, because an ignored argument is one the caller believes
// was applied. A result is the document a Message op answers with, or the
// data of the result artifact a Task op completes with — `false` where the op
// leaves none and its task's status message says what was done.

/// An object with exactly `props`, of which `required` must be present.
fn closed(required: &[&str], props: Value) -> Value {
    json!({
        "type": "object",
        "required": required,
        "properties": props,
        "additionalProperties": false,
    })
}

fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn named(description: &str) -> Value {
    json!({"type": "string", "minLength": 1, "description": description})
}

fn string_or_null() -> Value {
    json!({"type": ["string", "null"]})
}

fn count() -> Value {
    json!({"type": "integer", "minimum": 0})
}

fn count_or_null() -> Value {
    json!({"type": ["integer", "null"], "minimum": 0})
}

fn list_of(items: Value) -> Value {
    json!({"type": "array", "items": items})
}

/// An op that takes no arguments.
fn no_args() -> Value {
    closed(&[], json!({}))
}

/// An op that leaves no result artifact.
fn no_result() -> Value {
    json!(false)
}

/// A row nothing serves: nothing is accepted and nothing answered.
fn never() -> Value {
    json!(false)
}

/// `{run}`: one run, which the caller must own.
fn run_args() -> Value {
    closed(&["run"], json!({"run": named("The run id")}))
}

/// `{handle}`: one subagent, which the caller must own.
fn handle_args() -> Value {
    closed(&["handle"], json!({"handle": named("The subagent handle")}))
}

/// The internal tool the `subagent.*` ops run verbatim: its own input and
/// output schemas are the op's, so the two cannot drift.
fn tool(name: &str) -> crate::registry::internal::Contract {
    crate::registry::internal::contracts()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no internal contract {name}"))
}

fn subagent_send_args() -> Value {
    tool("subagent.send").input
}
fn subagent_send_result() -> Value {
    tool("subagent.send").output
}
fn subagent_kill_args() -> Value {
    tool("subagent.kill").input
}
fn subagent_kill_result() -> Value {
    tool("subagent.kill").output
}
fn subagent_status_args() -> Value {
    tool("subagent.status").input
}
fn subagent_status_result() -> Value {
    tool("subagent.status").output
}

/// The `status` document every caller is told: the instance's facts, the
/// workflows it may run, and its own runs, conversations and activity — each
/// item in the shape the feed pushes it, which is the same list. An
/// operator's document carries the instance's internals beside these, so it
/// is not closed.
fn status_result() -> Value {
    json!({
        "type": "object",
        "required": [
            "instance", "uptime_ms", "draining", "paused", "model", "version",
            "skills", "skill_prefix", "values", "workflows", "runs",
            "conversations", "activity",
        ],
        "properties": {
            "instance": {"type": "string"},
            "uptime_ms": count(),
            "draining": {"type": "boolean"},
            "paused": {"type": "boolean"},
            "model": {"type": "string"},
            "version": {"type": "string"},
            "skills": list_of(json!({"type": "string"})),
            "skill_prefix": {"type": "string"},
            "values": {"type": "object"},
            "workflows": list_of(json!({
                "type": "object",
                "required": ["name"],
                "properties": {"name": {"type": "string"}},
            })),
            "runs": list_of(FeedKind::Run.data_schema()),
            "conversations": list_of(FeedKind::Conversation.data_schema()),
            "activity": list_of(FeedKind::Activity.data_schema()),
        },
    })
}

fn config_result() -> Value {
    closed(
        &["config"],
        json!({"config": {"type": "object", "description": "credentials redacted to their {{secret:…}} references"}}),
    )
}

fn workflow_run_args() -> Value {
    closed(
        &["workflow"],
        json!({
            "workflow": named("The workflow to start"),
            "inputs": {"type": "object", "description": "The run's inputs"},
        }),
    )
}

fn workflow_run_result() -> Value {
    json!({"description": "the run's output, once the run finishes"})
}

fn workflow_status_args() -> Value {
    closed(
        &[],
        json!({"run": named("One run; without it, every run the caller may see")}),
    )
}

fn workflow_status_result() -> Value {
    closed(
        &["runs"],
        json!({"runs": list_of(closed(
            &["run", "workflow", "status", "output", "error"],
            json!({
                "run": {"type": "string"},
                "workflow": {"type": "string"},
                "status": {"type": "string"},
                "output": true,
                "error": string_or_null(),
            }),
        ))}),
    )
}

fn workflow_signal_args() -> Value {
    closed(
        &["name"],
        json!({
            "name": named("The signal a `wait: {on: signal}` step waits for"),
            "payload": {"description": "What the waiting step receives"},
            "run": named("Deliver to this run only; without it, to every waiter the caller owns"),
        }),
    )
}

fn workflow_signal_result() -> Value {
    closed(
        &["signal", "delivered"],
        json!({"signal": {"type": "string"}, "delivered": count()}),
    )
}

fn plan_get_args() -> Value {
    closed(
        &[],
        json!({"id": named("The conversation, as the caller names it; without it, the root")}),
    )
}

fn plan_get_result() -> Value {
    closed(
        &["conversation", "plan", "progress"],
        json!({
            "conversation": {"type": "string"},
            "plan": {"type": ["object", "null"]},
            "progress": string_or_null(),
        }),
    )
}

fn conversation_get_args() -> Value {
    closed(
        &["id"],
        json!({
            "id": named("The conversation, as the caller names it"),
            "limit": {"type": "integer", "minimum": 0, "description": "The newest messages to return (default 200, at most 1000)"},
        }),
    )
}

fn conversation_get_result() -> Value {
    closed(
        &["conversation"],
        json!({"conversation": closed(
            &[
                "id", "kind", "version", "turns", "est_tokens", "principal",
                "task", "skills", "plan", "summary", "total_messages",
                "messages", "updated",
            ],
            json!({
                "id": {"type": "string"},
                "kind": true,
                "version": count(),
                "turns": count(),
                "est_tokens": count(),
                "principal": string_or_null(),
                "task": string_or_null(),
                "skills": list_of(json!({"type": "string"})),
                "plan": true,
                "summary": true,
                "total_messages": count(),
                "messages": list_of(json!({"description": "strings truncated at 4096 bytes"})),
                "updated": count(),
            }),
        )}),
    )
}

/// `run.get`: the run's summary, with per-step detail in place of the step
/// histogram and its variables beside it.
fn run_get_result() -> Value {
    let mut run = FeedKind::Run.data_schema();
    run["properties"]["steps"] = json!({
        "type": "object",
        "additionalProperties": closed(
            &["status", "attempt", "started", "finished", "error", "wait", "output"],
            json!({
                "status": true,
                "attempt": count(),
                "started": count_or_null(),
                "finished": count_or_null(),
                "error": string_or_null(),
                "wait": true,
                "output": {"description": "strings truncated at 2048 bytes"},
            }),
        ),
    });
    run["properties"]["vars"] = json!({"type": "object"});
    if let Some(required) = run["required"].as_array_mut() {
        required.push(json!("vars"));
    }
    closed(&["run"], json!({"run": run}))
}

fn subagent_get_result() -> Value {
    closed(
        &["subagent"],
        json!({"subagent": closed(
            &[
                "handle", "mode", "status", "attempt", "tokens", "instruction",
                "result", "error", "requested_by", "created", "updated", "node",
            ],
            json!({
                "handle": {"type": "string"},
                "mode": true,
                "status": true,
                "attempt": count(),
                "tokens": count(),
                "instruction": true,
                "result": true,
                "error": string_or_null(),
                "requested_by": string_or_null(),
                "created": count(),
                "updated": count(),
                "node": count_or_null(),
            }),
        )}),
    )
}

fn debug_events_args() -> Value {
    closed(
        &[],
        json!({
            "after": {"type": "integer", "minimum": 0, "description": "Lines after this sequence number"},
            "limit": {"type": "integer", "minimum": 0, "description": "At most this many (default 200, at most 500)"},
            "level": string("Only lines of this level"),
            "prefix": string("Only events whose name starts with this"),
        }),
    )
}

fn debug_events_result() -> Value {
    closed(
        &["events", "newest_seq", "oldest_seq", "dropped"],
        json!({
            "events": list_of(json!({"type": "object"})),
            "newest_seq": count(),
            "oldest_seq": count(),
            "dropped": count(),
        }),
    )
}

fn reason_arg() -> Value {
    string("Why, for the log and the feed")
}

fn admin_drain_args() -> Value {
    closed(&[], json!({"reason": reason_arg()}))
}

fn admin_drain_result() -> Value {
    closed(
        &["ok", "state", "reason"],
        json!({"ok": {"const": true}, "state": {"const": "draining"}, "reason": {"type": "string"}}),
    )
}

fn admin_pause_args() -> Value {
    closed(
        &[],
        json!({"run": named("Pause this run; without it, the whole instance"), "reason": reason_arg()}),
    )
}

fn admin_pause_result() -> Value {
    json!({"oneOf": [
        closed(&["ok", "paused"], json!({"ok": {"const": true}, "paused": {"type": "string"}})),
        closed(
            &["ok", "state", "reason"],
            json!({"ok": {"const": true}, "state": {"const": "paused"}, "reason": {"type": "string"}}),
        ),
    ]})
}

fn admin_resume_args() -> Value {
    closed(
        &[],
        json!({"run": named("Resume this run; without it, the whole instance")}),
    )
}

fn admin_resume_result() -> Value {
    json!({"oneOf": [
        closed(&["ok", "resumed"], json!({"ok": {"const": true}, "resumed": {"type": "string"}})),
        closed(&["ok", "state"], json!({"ok": {"const": true}, "state": {"const": "running"}})),
    ]})
}

fn admin_cancel_args() -> Value {
    closed(
        &["run"],
        json!({"run": named("The run id"), "reason": reason_arg()}),
    )
}

fn admin_cancel_result() -> Value {
    closed(
        &["ok", "cancelled"],
        json!({"ok": {"const": true}, "cancelled": {"type": "string"}}),
    )
}

/// `admin.set {path, value}`. The value is read with the configuration's own
/// type for the path, so it accepts exactly what the config file does there;
/// spelling that type out again here would be a second list to drift.
fn admin_set_args() -> Value {
    closed(
        &["path", "value"],
        json!({
            "path": {"enum": RUNTIME_SETTABLE},
            "value": {"description": "What the config file accepts at `path`"},
        }),
    )
}

fn admin_set_result() -> Value {
    closed(
        &["path", "value"],
        json!({"path": {"enum": RUNTIME_SETTABLE}, "value": {"description": "as parsed"}}),
    )
}

fn scopes() -> Value {
    json!({"enum": DeviceScope::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>()})
}

fn device_pending_result() -> Value {
    closed(
        &["pending"],
        json!({"pending": list_of(closed(
            &["user_code", "client_id", "scope", "peer", "requested_at", "expires_at"],
            json!({
                "user_code": {"type": "string"},
                "client_id": {"type": "string"},
                "scope": scopes(),
                "peer": string_or_null(),
                "requested_at": count(),
                "expires_at": count(),
            }),
        ))}),
    )
}

/// `auth.device.approve {user_code, as, scope?}`: `as` is the name the
/// device signs in as, held to [`APPROVAL_NAME_PATTERN`] and kept off
/// [`RESERVED_APPROVAL_NAMES`] — the handler's own rule and list, so the
/// published schema and the refusal cannot disagree.
fn device_approve_args() -> Value {
    closed(
        &["user_code", "as"],
        json!({
            "user_code": named("The code the device shows"),
            "as": {
                "type": "string",
                "pattern": APPROVAL_NAME_PATTERN,
                "not": {"enum": RESERVED_APPROVAL_NAMES},
                "description": "The name the device signs in as: every session approved as one name is one principal",
            },
            "scope": scopes(),
        }),
    )
}

fn device_approve_result() -> Value {
    closed(
        &["approved"],
        json!({"approved": closed(
            &["user_code", "client_id", "scope", "peer", "principal", "existing"],
            json!({
                "user_code": {"type": "string"},
                "client_id": {"type": "string"},
                "scope": scopes(),
                "peer": string_or_null(),
                "principal": {"type": "string"},
                "existing": {"type": "boolean"},
            }),
        )}),
    )
}

/// `{user_code}` or `{all: true}`: exactly one.
fn device_deny_args() -> Value {
    json!({
        "type": "object",
        "properties": {
            "user_code": named("The code the device shows"),
            "all": {"type": "boolean"},
        },
        "additionalProperties": false,
        "oneOf": [
            {"required": ["user_code"]},
            {"required": ["all"], "properties": {"all": {"const": true}}},
        ],
    })
}

fn device_deny_result() -> Value {
    closed(&["denied"], json!({"denied": count()}))
}

fn sessions_result() -> Value {
    closed(
        &["sessions"],
        json!({"sessions": list_of(closed(
            &["sid", "kind", "principal", "role", "client_id", "created_at", "expires_at", "approved_by"],
            json!({
                "sid": {"type": "string"},
                "kind": {"type": "string"},
                "principal": {"type": "string"},
                "role": {"type": "string"},
                "client_id": {"type": "string"},
                "created_at": count(),
                "expires_at": count_or_null(),
                "approved_by": {"type": "string"},
                "name": {"type": "string"},
                "approved_rule": {"type": "string"},
            }),
        ))}),
    )
}

/// `{sid}`, `{name}` or `{all: true}`: exactly one.
fn sessions_revoke_args() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sid": named("One session"),
            "name": named("Every session approved as this name"),
            "all": {"type": "boolean"},
        },
        "additionalProperties": false,
        "oneOf": [
            {"required": ["sid"]},
            {"required": ["name"]},
            {"required": ["all"], "properties": {"all": {"const": true}}},
        ],
    })
}

fn sessions_revoke_result() -> Value {
    closed(&["revoked"], json!({"revoked": count()}))
}

fn instance_result_args() -> Value {
    closed(
        &["handle"],
        json!({
            "handle": named("The parent's handle for the child"),
            "status": {"description": "The child's run status"},
            "output": {"description": "The child's run output"},
        }),
    )
}

fn instance_emit_args() -> Value {
    closed(
        &["handle", "stream", "event"],
        json!({
            "handle": named("The parent's handle for the child"),
            "stream": named("The stream, by the name both sides declare"),
            "event": {
                "type": "object",
                "description": "The child's stream event: subject, correlation, data and id",
            },
        }),
    )
}

/// The `_instance.` row's own schemas: one of its members'.
fn instance_family_args() -> Value {
    json!({"oneOf": INSTANCE_OPS.iter().map(|m| (m.args_schema)()).collect::<Vec<_>>()})
}

fn instance_family_result() -> Value {
    no_result()
}

// ── The command envelope ─────────────────────────────────────────────────────

/// The row and, for a prefix family, the member that serve `op` — or `None`
/// for an op nothing serves: no row, a row only held in reserve, or a name
/// under a prefix that is no member of it.
fn served(op: &str) -> Option<(&'static OpSpec, Option<&'static Member>)> {
    let spec = op_spec(op).filter(|s| s.handler != Handler::Reserved)?;
    if !spec.is_prefix() {
        return Some((spec, None));
    }
    let member = INSTANCE_OPS.iter().find(|m| m.name == op)?;
    Some((spec, Some(member)))
}

/// The published argument schema of `op`, if anything serves it.
pub fn args_schema_of(op: &str) -> Option<Value> {
    served(op).map(|(spec, member)| match member {
        Some(m) => (m.args_schema)(),
        None => (spec.args_schema)(),
    })
}

/// The published result schema of `op`, if anything serves it.
pub fn result_schema_of(op: &str) -> Option<Value> {
    served(op).map(|(spec, member)| match member {
        Some(m) => (m.result_schema)(),
        None => (spec.result_schema)(),
    })
}

/// The schema of `op`'s whole envelope — the object under `data.agentd` —
/// which is its argument schema with `op` itself beside the arguments. This is
/// what [`check_command`] validates against and what the bundle publishes,
/// built once, here, so the two are the same value.
pub fn command_envelope(op: &str) -> Option<Value> {
    let mut schema = args_schema_of(op)?;
    let obj = schema.as_object_mut()?;
    obj.entry("properties")
        .or_insert_with(|| json!({}))
        .as_object_mut()?
        .insert("op".into(), json!({"const": op}));
    let required = obj.entry("required").or_insert_with(|| json!([]));
    if let Some(r) = required.as_array_mut() {
        r.insert(0, json!("op"));
    }
    Some(schema)
}

/// Every envelope agentd serves: the listable ops, then the reserved
/// `_instance.` members.
pub fn command_envelopes() -> Vec<(&'static str, Value)> {
    static_vocabulary()
        .into_iter()
        .chain(INSTANCE_OPS.iter().map(|m| m.name))
        .filter_map(|op| command_envelope(op).map(|e| (op, e)))
        .collect()
}

/// The patterns the published schemas use that [`crate::jsonschema`] does not
/// enforce — it has no regex engine, and checks only literal patterns — each
/// with the function that is the pattern. [`check_command`] applies them, so
/// a pattern a caller reads in a schema is a pattern its call is held to.
const REGEX_PATTERNS: &[(&str, Matches)] = &[(APPROVAL_NAME_PATTERN, approval_name_ok)];

/// Whether a string matches one of the [`REGEX_PATTERNS`].
type Matches = fn(&str) -> bool;

/// The violations of the [`REGEX_PATTERNS`] among `envelope`'s top-level
/// arguments, in the validator's own `<pointer>: <message>` form.
fn regex_violations(schema: &Value, envelope: &Value) -> Vec<String> {
    let Some(props) = schema["properties"].as_object() else {
        return Vec::new();
    };
    props
        .iter()
        .filter_map(|(name, prop)| {
            let pattern = prop["pattern"].as_str()?;
            let (_, matches) = REGEX_PATTERNS.iter().find(|(p, _)| *p == pattern)?;
            let value = envelope.get(name)?.as_str()?;
            (!matches(value)).then(|| format!("/{name}: does not match pattern {pattern:?}"))
        })
        .collect()
}

/// A command envelope that passed [`check_command`].
#[derive(Debug, Clone)]
pub struct Command {
    pub op: String,
    /// The part carrying it, for the field a later refusal names.
    pub index: usize,
    /// The whole object under `data.agentd`, `op` included.
    pub envelope: Value,
    /// Its row, for a built-in op; `None` for an op only a loaded workflow
    /// can declare, which the runtime alone can judge.
    pub spec: Option<&'static OpSpec>,
}

/// A command refusal: `code`, with the field it concerns and agentd's reason.
pub fn command_refusal(
    code: i64,
    field: &str,
    violation: &str,
    why: &str,
    message: &str,
    meta: &[(&str, &str)],
) -> Value {
    json!({
        "code": code,
        "message": message,
        "data": [
            errors::bad_request(&[(field, violation)]),
            errors::error_info(errors::domain_of(why), why, meta),
        ],
    })
}

/// An op nothing serves.
pub fn unknown_op_error(op: &str, field: &str) -> Value {
    let message = format!("unknown command {op:?}");
    command_refusal(
        errors::INVALID_PARAMS,
        field,
        "not an op this agent serves",
        reason::UNKNOWN_OP,
        &message,
        &[("op", op)],
    )
}

/// Arguments that miss their schema: one field violation per miss, each
/// named from the envelope's own part.
pub fn invalid_args(op: &str, index: usize, errs: &[String]) -> Value {
    let base = format!("message.parts[{index}].data.agentd");
    let violations: Vec<(String, String)> = errs
        .iter()
        .map(|e| {
            let (pointer, what) = e.split_once(": ").unwrap_or(("/", e.as_str()));
            let path = pointer.trim_start_matches('/').replace('/', ".");
            let field = if path.is_empty() {
                base.clone()
            } else {
                format!("{base}.{path}")
            };
            (field, what.to_string())
        })
        .collect();
    let pairs: Vec<(&str, &str)> = violations
        .iter()
        .map(|(f, w)| (f.as_str(), w.as_str()))
        .collect();
    json!({
        "code": errors::INVALID_PARAMS,
        "message": format!("command {op:?} arguments do not match its schema: {}", errs.join("; ")),
        "data": [
            errors::bad_request(&pairs),
            errors::error_info(errors::AGENTD_DOMAIN, reason::INVALID_COMMAND_ARGS, &[("op", op)]),
        ],
    })
}

/// Whether a media type is JSON: `application/json`, with any parameters,
/// in any case.
fn is_json(media_type: &str) -> bool {
    media_type
        .split(';')
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
}

/// The command a `SendMessage`'s `params` carries, held to the command extension — or
/// the JSON-RPC error object refusing it. `Ok(None)` is a message that
/// carries no command at all.
///
/// `task` is the task the message names (the listener reads it off the
/// message; the runtime from what the protocol layer passed it), and
/// `activated` whether the request activated [`super::COMMAND_EXTENSION`].
///
/// The listener asks this before authorization, and the runtime asks it
/// again for whatever reaches a send another way, so the two refuse the same
/// requests the same way. In order:
///
/// 1. a command needs the command extension activated by the `A2A-Extensions` header
///    (`EXTENSION_NOT_ACTIVATED`) — a DataPart under `agentd` sent without it
///    is not a command a client can have meant, and it is not run as one;
/// 2. and the message marked with it in `extensions` (`EXTENSION_NOT_MARKED`),
///    which is how a 1.0 peer tells a command from a message carrying data;
/// 3. exactly one envelope (`COMMAND_ENVELOPE_AMBIGUOUS`);
/// 4. no task (`COMMAND_TASK_ID`): a command starts its own;
/// 5. JSON among the output modes the caller accepts, if it names any
///    (`-32005`): every command answers with data;
/// 6. an op something serves (`UNKNOWN_OP`) — an op no row holds is passed
///    on, since only the runtime knows the commands its workflows declare;
/// 7. arguments that match the op's published schema (`INVALID_COMMAND_ARGS`).
pub fn check_command(
    params: &Value,
    task: Option<&str>,
    activated: bool,
) -> Result<Option<Command>, Value> {
    let message = &params["message"];
    let envelopes: Vec<(usize, &Value)> = message["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.get("data").and_then(|d| d.get("agentd")).map(|a| (i, a)))
                .collect()
        })
        .unwrap_or_default();
    let Some(&(index, envelope)) = envelopes.first() else {
        return Ok(None);
    };
    let uri = super::COMMAND_EXTENSION;
    let at = format!("message.parts[{index}].data.agentd");
    if !activated {
        return Err(command_refusal(
            errors::INVALID_PARAMS,
            &at,
            "a command needs its extension activated",
            reason::EXTENSION_NOT_ACTIVATED,
            &format!("activate {uri} with the A2A-Extensions header to send a command"),
            &[("extension", uri)],
        ));
    }
    let marked = message["extensions"]
        .as_array()
        .is_some_and(|e| e.iter().any(|x| x == uri));
    if !marked {
        return Err(command_refusal(
            errors::INVALID_PARAMS,
            "message.extensions",
            "a command message lists its extension",
            reason::EXTENSION_NOT_MARKED,
            &format!("a command message lists {uri} in message.extensions"),
            &[("extension", uri)],
        ));
    }
    if envelopes.len() > 1 {
        return Err(command_refusal(
            errors::INVALID_PARAMS,
            "message.parts",
            "a command message carries exactly one command part",
            reason::COMMAND_ENVELOPE_AMBIGUOUS,
            &format!(
                "a message carries one command; this one carries {}",
                envelopes.len()
            ),
            &[],
        ));
    }
    let op = envelope["op"].as_str().unwrap_or_default();
    if let Some(task) = task {
        return Err(command_refusal(
            errors::INVALID_PARAMS,
            "message.taskId",
            "a command message carries no taskId",
            reason::COMMAND_TASK_ID,
            &format!("command {op:?} starts its own task; it cannot name one (taskId {task:?})"),
            &[("op", op)],
        ));
    }
    let accepted = params["configuration"]["acceptedOutputModes"]
        .as_array()
        .filter(|modes| !modes.is_empty());
    if let Some(modes) = accepted
        && !modes.iter().filter_map(Value::as_str).any(is_json)
    {
        return Err(command_refusal(
            errors::CONTENT_TYPE_NOT_SUPPORTED,
            "configuration.acceptedOutputModes",
            "a command answers with application/json",
            reason::CONTENT_TYPE_NOT_SUPPORTED,
            &format!(
                "command {op:?} answers with application/json, which the request does not accept"
            ),
            &[("op", op)],
        ));
    }
    let op_field = format!("{at}.op");
    if op.is_empty() {
        return Err(command_refusal(
            errors::INVALID_PARAMS,
            &op_field,
            "a command names its op",
            reason::UNKNOWN_OP,
            "a command names its op: {\"agentd\": {\"op\": \"…\"}}",
            &[],
        ));
    }
    let spec = if is_builtin_op(op) {
        let Some(schema) = command_envelope(op) else {
            return Err(unknown_op_error(op, &op_field));
        };
        let mut errs = crate::jsonschema::validate(&schema, envelope)
            .err()
            .unwrap_or_default();
        errs.extend(regex_violations(&schema, envelope));
        if !errs.is_empty() {
            return Err(invalid_args(op, index, &errs));
        }
        op_spec(op)
    } else {
        None
    };
    Ok(Some(Command {
        op: op.to_string(),
        index,
        envelope: envelope.clone(),
        spec,
    }))
}

/// The command extension's schema bundle, published next to the extension's URI.
///
/// The root is the object a command DataPart carries (`{"agentd": {…}}`);
/// `$defs.envelopes.<op>` is each op's whole envelope, the schema
/// [`check_command`] holds it to; `$defs.ops.<op>` and
/// `$defs.reserved.<op>` give each op's reply kind, description, arguments
/// and result. An op no row serves — one a loaded workflow declares — is
/// not described here: the root lets it through, to the schema its workflow
/// declares.
pub fn schema_bundle() -> Value {
    let entry = |op: &str, description: &str| {
        let reply = match op_spec(op).map(|s| s.reply) {
            Some(Reply::Message) => "message",
            _ => "task",
        };
        json!({
            "description": description,
            "reply": reply,
            "args": args_schema_of(op),
            "result": result_schema_of(op),
        })
    };
    let ops: serde_json::Map<String, Value> = static_vocabulary()
        .into_iter()
        .filter_map(|op| op_spec(op).map(|s| (op.to_string(), entry(op, s.description))))
        .collect();
    let reserved: serde_json::Map<String, Value> = INSTANCE_OPS
        .iter()
        .map(|m| (m.name.to_string(), entry(m.name, m.description)))
        .collect();
    let envelopes = command_envelopes();
    let per_op: Vec<Value> = envelopes
        .iter()
        .map(|(op, _)| {
            json!({
                "if": {"required": ["op"], "properties": {"op": {"const": op}}},
                "then": {"$ref": format!("#/$defs/envelopes/$defs/{op}")},
            })
        })
        .collect();
    let envelopes: serde_json::Map<String, Value> = envelopes
        .into_iter()
        .map(|(op, e)| (op.to_string(), e))
        .collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": super::schema_of(super::COMMAND_EXTENSION),
        "title": "agentd command",
        "description": "The data of a command DataPart. $defs/envelopes/$defs/<op> is each op's \
                        envelope; $defs/ops/<op> and $defs/reserved/<op> give its reply kind \
                        (message or task), arguments and result — for a message op the document \
                        its reply carries, for a task op the data of the result artifact it \
                        completes with (false: none). Reserved ops are operator only and never \
                        listed in params.ops.",
        "type": "object",
        "required": ["agentd"],
        "properties": {"agentd": {"$ref": "#/$defs/envelope"}},
        "$defs": {
            "envelope": {
                "type": "object",
                "required": ["op"],
                "properties": {"op": {"type": "string", "minLength": 1}},
                "allOf": per_op,
            },
            "envelopes": {"$defs": envelopes},
            "ops": ops,
            "reserved": reserved,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::Principal;

    /// Every `auth.*` row answers to the operator role alone: `grants: ["*"]`
    /// on a user or an agent — or a grant naming the op — reaches none of
    /// them. They approve sign-ins and end sessions; a grant that reached them
    /// would let a signed-in user approve themselves an operator.
    #[test]
    fn auth_ops_are_operator_only_even_with_star_grants() {
        let auth: Vec<&OpSpec> = OPS.iter().filter(|s| s.handler == Handler::Auth).collect();
        assert_eq!(auth.len(), 5, "the five auth ops");
        let who = |role, grants: &[&str]| Principal {
            role,
            grants: grants.iter().map(|g| (*g).to_string()).collect(),
            ..Principal::anonymous()
        };
        for spec in auth {
            assert_eq!(spec.floor, Floor::Operator, "{}", spec.name);
            assert!(who(Role::Operator, &[]).may_command(spec.name));
            for role in [Role::User, Role::Agent] {
                for grants in [&["*"][..], &[spec.name], &["auth.*"]] {
                    assert!(
                        !who(role, grants).may_command(spec.name),
                        "{role:?} {grants:?} must not {}",
                        spec.name
                    );
                }
            }
        }
    }

    /// The device ops are served with the grant; the session ops on any TCP
    /// listener, and on no unix socket, which issues no session.
    #[test]
    fn auth_ops_are_gated_by_the_grant_and_a_tcp_listener() {
        let served = |listen: Option<&str>, device: bool| {
            let mut s = Settings::default();
            s.a2a.listen = listen.map(str::to_string);
            s.a2a.device_grant.enabled = device;
            command_ops_of(&s)
                .into_iter()
                .filter(|o| o.starts_with("auth."))
                .collect::<Vec<_>>()
        };
        assert_eq!(served(None, false), Vec::<&str>::new());
        // Every spelling the listener parses as a socket.
        for socket in ["unix:///run/a.sock", "unix:/run/a.sock"] {
            assert_eq!(served(Some(socket), false), Vec::<&str>::new(), "{socket}");
        }
        assert_eq!(
            served(Some("http://127.0.0.1:0"), false),
            ["auth.sessions", "auth.sessions.revoke"]
        );
        assert_eq!(served(Some("https://0.0.0.0:8443"), true).len(), 5);
    }

    #[test]
    fn approval_names_are_the_pattern() {
        for ok in ["alice", "a", "0", "ci-bot", "a.b_c-d", &"a".repeat(64)] {
            assert!(approval_name_ok(ok), "{ok:?}");
        }
        for bad in [
            "",
            "Alice",
            "-a",
            ".a",
            "_a",
            "a b",
            "a:b",
            "a=b",
            "a/b",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!approval_name_ok(bad), "{bad:?}");
        }
        // Every reserved name has the shape, so it needs its own refusal.
        for r in RESERVED_APPROVAL_NAMES {
            assert!(approval_name_ok(r), "{r}");
        }
    }

    /// A command send's params, as the listener reads them: `parts`, marked
    /// with the command extension unless `marked` is false.
    fn send(parts: Value, marked: bool) -> Value {
        let mut message = json!({"role": "ROLE_USER", "messageId": "m-1", "parts": parts});
        if marked {
            message["extensions"] = json!([super::super::COMMAND_EXTENSION]);
        }
        json!({"message": message})
    }

    fn command(envelope: Value) -> Value {
        send(
            json!([{"data": {"agentd": envelope}, "mediaType": "application/json"}]),
            true,
        )
    }

    /// The code and agentd reason of a refusal, and its first field.
    fn refused(e: &Value) -> (i64, &str, &str) {
        (
            e["code"].as_i64().unwrap_or_default(),
            e["data"][1]["reason"].as_str().unwrap_or_default(),
            e["data"][0]["fieldViolations"][0]["field"]
                .as_str()
                .unwrap_or_default(),
        )
    }

    /// A valid envelope of every op agentd serves. A new op must add one, so
    /// the agreement below covers it.
    fn sample(op: &str) -> Value {
        let args = match op {
            "workflow.run" => json!({"workflow": "greet", "inputs": {"who": "ada"}}),
            "workflow.status" => json!({"run": "greet-1"}),
            "workflow.cancel" | "run.get" | "admin.cancel" => json!({"run": "greet-1"}),
            "workflow.signal" => json!({"name": "go", "payload": {"n": 1}, "run": "greet-1"}),
            "subagent.send" => json!({"handle": "sub-1", "message": "left"}),
            "subagent.kill" => json!({"handle": "sub-1", "reason": "done"}),
            "subagent.status" | "subagent.get" => json!({"handle": "sub-1"}),
            "plan.get" => json!({"id": "chat"}),
            "conversation.get" => json!({"id": "chat", "limit": 20}),
            "debug.events" => json!({"after": 3, "limit": 10, "level": "warn", "prefix": "a2a."}),
            "admin.drain" => json!({"reason": "deploy"}),
            "admin.pause" => json!({"run": "greet-1", "reason": "look"}),
            "admin.resume" => json!({"run": "greet-1"}),
            "admin.set" => json!({"path": "agent.approval", "value": "auto"}),
            "auth.device.approve" => {
                json!({"user_code": "WDJB-MJHT", "as": "alice", "scope": "user"})
            }
            "auth.device.deny" => json!({"user_code": "WDJB-MJHT"}),
            "auth.sessions.revoke" => json!({"sid": "ds_0123456789abcdef"}),
            "_instance.result" => json!({"handle": "c1", "status": "completed", "output": "ok"}),
            "_instance.emit" => {
                json!({"handle": "c1", "stream": "orders", "event": {"subject": "s"}})
            }
            "status" | "config" | "auth.device.pending" | "auth.sessions" => json!({}),
            other => panic!("no sample for {other}: add one"),
        };
        let mut envelope = json!({"op": op});
        envelope
            .as_object_mut()
            .unwrap()
            .extend(args.as_object().unwrap().clone());
        envelope
    }

    /// Every refusal the command extension names, each in its shape — code, reason and
    /// the field it concerns — and nothing refused that should run.
    #[test]
    fn command_check_refusals() {
        let check = |params: &Value| check_command(params, None, true);
        let status = json!({"op": "status"});

        // Not a command at all: no envelope, whatever else the message holds.
        assert!(
            check(&send(json!([{"text": "hi"}]), false))
                .unwrap()
                .is_none()
        );
        assert!(
            check(&send(json!([{"data": {"x": 1}}]), false))
                .unwrap()
                .is_none()
        );

        // Not activated: the header is how a command is asked for, and a
        // DataPart under `agentd` without it is not run as one.
        let e = check_command(&command(status.clone()), None, false).unwrap_err();
        assert_eq!(
            refused(&e),
            (
                errors::INVALID_PARAMS,
                reason::EXTENSION_NOT_ACTIVATED,
                "message.parts[0].data.agentd"
            )
        );
        assert_eq!(
            e["data"][1]["metadata"]["extension"],
            super::super::COMMAND_EXTENSION
        );
        // Not marked on the message.
        let unmarked = send(
            json!([{"text": "please"}, {"data": {"agentd": status}}]),
            false,
        );
        let e = check(&unmarked).unwrap_err();
        assert_eq!(
            refused(&e),
            (
                errors::INVALID_PARAMS,
                reason::EXTENSION_NOT_MARKED,
                "message.extensions"
            )
        );
        // Two envelopes.
        let two = send(
            json!([{"data": {"agentd": status}}, {"data": {"agentd": status}}]),
            true,
        );
        let e = check(&two).unwrap_err();
        assert_eq!(
            refused(&e),
            (
                errors::INVALID_PARAMS,
                reason::COMMAND_ENVELOPE_AMBIGUOUS,
                "message.parts"
            )
        );
        // A task of its own choosing.
        let e = check_command(&command(status.clone()), Some("t-1"), true).unwrap_err();
        assert_eq!(
            refused(&e),
            (
                errors::INVALID_PARAMS,
                reason::COMMAND_TASK_ID,
                "message.taskId"
            )
        );
        // An answer the caller does not accept — and one it does, with
        // parameters and in any case.
        let mut picky = command(status.clone());
        picky["configuration"] = json!({"acceptedOutputModes": ["text/plain"]});
        let e = check(&picky).unwrap_err();
        assert_eq!(
            refused(&e),
            (
                errors::CONTENT_TYPE_NOT_SUPPORTED,
                reason::CONTENT_TYPE_NOT_SUPPORTED,
                "configuration.acceptedOutputModes"
            )
        );
        picky["configuration"]["acceptedOutputModes"] =
            json!(["text/plain", "Application/JSON; charset=utf-8"]);
        assert!(check(&picky).is_ok());
        picky["configuration"]["acceptedOutputModes"] = json!([]);
        assert!(check(&picky).is_ok(), "an empty list names no preference");

        // Ops nothing serves: held in reserve, a name under a prefix that is
        // no member.
        for op in ["ask_human", "_instance.nope", "_instance."] {
            let e = check(&command(json!({"op": op}))).unwrap_err();
            assert_eq!(
                refused(&e),
                (
                    errors::INVALID_PARAMS,
                    reason::UNKNOWN_OP,
                    "message.parts[0].data.agentd.op"
                ),
                "{op}"
            );
            assert_eq!(e["message"], format!("unknown command {op:?}"));
        }
        for missing in [json!({}), json!({"op": ""}), json!({"op": 7})] {
            let e = check(&command(missing.clone())).unwrap_err();
            assert_eq!(refused(&e).1, reason::UNKNOWN_OP, "{missing}");
        }
        // An op no row holds is the runtime's to judge: it may be a command
        // a loaded workflow declares, whose schema only the runtime has.
        let declared = check(&command(json!({"op": "review.start", "any": [1]})))
            .unwrap()
            .unwrap();
        assert_eq!(
            (declared.op.as_str(), declared.spec.is_none()),
            ("review.start", true)
        );

        // Arguments off the schema, each miss named as the field it is.
        for (envelope, field) in [
            (
                json!({"op": "status", "verbose": true}),
                "message.parts[1].data.agentd",
            ),
            (
                json!({"op": "workflow.run", "name": "greet"}),
                "message.parts[1].data.agentd",
            ),
            (
                json!({"op": "workflow.run", "workflow": 3}),
                "message.parts[1].data.agentd.workflow",
            ),
            (
                json!({"op": "admin.set", "path": "agent.model", "value": "x"}),
                "message.parts[1].data.agentd.path",
            ),
            (
                json!({"op": "auth.device.deny", "user_code": "A", "all": true}),
                "message.parts[1].data.agentd",
            ),
            (
                json!({"op": "auth.sessions.revoke", "sid": "s", "name": "n"}),
                "message.parts[1].data.agentd",
            ),
            (
                json!({"op": "auth.device.approve", "user_code": "A", "as": "Alice"}),
                "message.parts[1].data.agentd.as",
            ),
            (
                json!({"op": "auth.device.approve", "user_code": "A", "as": "operator"}),
                "message.parts[1].data.agentd.as",
            ),
            (
                json!({"op": "auth.device.approve", "user_code": "A"}),
                "message.parts[1].data.agentd",
            ),
            (
                json!({"op": "_instance.emit", "handle": "c1", "stream": "s"}),
                "message.parts[1].data.agentd",
            ),
        ] {
            let params = send(
                json!([{"text": "please"}, {"data": {"agentd": envelope}}]),
                true,
            );
            let e = check(&params).unwrap_err();
            assert_eq!(
                refused(&e),
                (errors::INVALID_PARAMS, reason::INVALID_COMMAND_ARGS, field),
                "{envelope}: {e}"
            );
            assert_eq!(e["data"][1]["metadata"]["op"], envelope["op"], "{e}");
        }

        // Admitted: the op, its part and its row. A number a DataPart
        // carried as a double is still the integer it was.
        let ok = check(&command(
            json!({"op": "conversation.get", "id": "c", "limit": 20.0}),
        ))
        .unwrap()
        .unwrap();
        assert_eq!((ok.op.as_str(), ok.index), ("conversation.get", 0));
        assert_eq!(ok.spec.map(|s| s.name), Some("conversation.get"));
        assert_eq!(ok.envelope["id"], "c");
    }

    /// The schema a caller reads is the schema its call is held to: each
    /// envelope the bundle publishes IS the one `check_command` validates
    /// against, and on every op's sample — as sent, with a field it does not
    /// take, and without each field it requires — the bundle and the check
    /// agree. The only thing the bundle's validator cannot hold a call to is
    /// a regex pattern, and each one the schemas use is one `check_command`
    /// enforces itself, with the function that is that pattern.
    #[test]
    fn published_args_schema_is_the_enforced_one() {
        let bundle = schema_bundle();
        assert_eq!(
            bundle["$id"],
            format!("{}/schema.json", super::super::COMMAND_EXTENSION)
        );
        // Well-formed, but for the regex patterns only check_command applies.
        if let Err(errs) = crate::jsonschema::check_schema(&bundle) {
            for e in &errs {
                assert!(
                    REGEX_PATTERNS
                        .iter()
                        .any(|(p, _)| e.contains(&format!("{p:?}"))),
                    "{e}"
                );
            }
        }
        let envelopes = command_envelopes();
        assert_eq!(
            envelopes.len(),
            static_vocabulary().len() + INSTANCE_OPS.len(),
            "every served op, every member"
        );
        for (op, envelope) in &envelopes {
            assert_eq!(&bundle["$defs"]["envelopes"]["$defs"][op], envelope, "{op}");
            let entry = if op.starts_with("_instance.") {
                &bundle["$defs"]["reserved"][op]
            } else {
                &bundle["$defs"]["ops"][op]
            };
            assert_eq!(entry["args"], args_schema_of(op).unwrap(), "{op}");
            assert_eq!(entry["result"], result_schema_of(op).unwrap(), "{op}");
            let reply = if op_spec(op).unwrap().reply == Reply::Message {
                "message"
            } else {
                "task"
            };
            assert_eq!(entry["reply"], reply, "{op}");
            assert!(
                !entry["description"].as_str().unwrap_or("").is_empty(),
                "{op}"
            );
            // Closed: a name the handler does not read is refused, not ignored.
            assert_eq!(envelope["additionalProperties"], false, "{op}");
            // Every pattern is one something enforces: the validator (a
            // literal one) or check_command (a known regex, on a top-level
            // argument, which is where check_command looks).
            walk_patterns(envelope, &mut |at_top, p| {
                let literal = crate::jsonschema::check_schema(&json!({"pattern": p})).is_ok();
                let known = REGEX_PATTERNS.iter().any(|(k, _)| *k == p);
                assert!(
                    literal || (known && at_top),
                    "{op}: pattern {p:?} is not enforced"
                );
            });

            // The agreement, sample by sample.
            let good = sample(op);
            let mut variants = vec![good.clone()];
            let mut extra = good.clone();
            extra["bogus"] = json!(true);
            variants.push(extra);
            for req in envelope["required"].as_array().unwrap() {
                let req = req.as_str().unwrap();
                if req != "op" {
                    let mut without = good.clone();
                    without.as_object_mut().unwrap().remove(req);
                    variants.push(without);
                }
            }
            for (i, v) in variants.iter().enumerate() {
                let published = crate::jsonschema::validate(&bundle, &json!({"agentd": v})).is_ok()
                    && regex_violations(envelope, v).is_empty();
                let enforced = check_command(&command(v.clone()), None, true).is_ok();
                assert_eq!(published, enforced, "{op} {v}");
                assert_eq!(enforced, i == 0, "{op} {v}: only the sample is valid");
            }
        }
        // A regex the validator skips is still refused, and the published
        // schema of the approval carries the handler's own rule and list.
        let approve = args_schema_of("auth.device.approve").unwrap();
        assert_eq!(
            approve["properties"]["as"]["pattern"],
            APPROVAL_NAME_PATTERN
        );
        assert_eq!(
            approve["properties"]["as"]["not"]["enum"],
            json!(RESERVED_APPROVAL_NAMES)
        );
        assert_eq!(approve["required"], json!(["user_code", "as"]));
        for name in ["Alice", "a:b", "-x", ""] {
            let v = json!({"op": "auth.device.approve", "user_code": "A", "as": name});
            assert!(check_command(&command(v), None, true).is_err(), "{name:?}");
        }
        for name in RESERVED_APPROVAL_NAMES {
            let v = json!({"op": "auth.device.approve", "user_code": "A", "as": name});
            assert!(
                crate::jsonschema::validate(&bundle, &json!({"agentd": v})).is_err(),
                "{name}"
            );
        }
        // The root lets an op no row holds through, to its workflow's schema.
        assert!(
            crate::jsonschema::validate(
                &bundle,
                &json!({"agentd": {"op": "review.start", "x": 1}})
            )
            .is_ok()
        );
    }

    /// Every `pattern` under `schema`, with whether it sits on a top-level
    /// property.
    fn walk_patterns(schema: &Value, each: &mut dyn FnMut(bool, &str)) {
        fn walk(v: &Value, depth: usize, each: &mut dyn FnMut(bool, &str)) {
            match v {
                Value::Object(o) => {
                    if let Some(p) = o.get("pattern").and_then(Value::as_str) {
                        each(depth == 2, p);
                    }
                    for x in o.values() {
                        walk(x, depth + 1, each);
                    }
                }
                Value::Array(a) => a.iter().for_each(|x| walk(x, depth + 1, each)),
                _ => {}
            }
        }
        // `properties` (1) → the property's schema (2).
        walk(schema, 0, each);
    }

    /// The `_instance.` reports are published — under `$defs.reserved`, each
    /// with its arguments and result — and never listed: no vocabulary, no
    /// instance's `params.ops`, and not among the bundle's listed ops.
    #[test]
    fn reserved_instance_ops_are_published_not_listed() {
        let bundle = schema_bundle();
        let reserved = bundle["$defs"]["reserved"].as_object().unwrap();
        let mut names: Vec<&str> = reserved.keys().map(String::as_str).collect();
        let mut members: Vec<&str> = INSTANCE_OPS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        members.sort_unstable();
        assert_eq!(names, members);
        let result = &bundle["$defs"]["reserved"]["_instance.result"];
        assert_eq!(result["reply"], "task");
        assert_eq!(result["args"]["required"], json!(["handle"]));
        assert_eq!(result["result"], json!(false));
        assert!(bundle["$defs"]["reserved"]["_instance.emit"]["args"].is_object());
        for m in INSTANCE_OPS {
            assert!(m.name.starts_with("_instance."), "{}", m.name);
            assert_eq!(op_spec(m.name).map(|s| s.floor), Some(Floor::Operator));
            assert!(bundle["$defs"]["ops"].get(m.name).is_none(), "{}", m.name);
        }
        assert!(
            !static_vocabulary()
                .iter()
                .any(|o| o.starts_with("_instance."))
        );
        for events in [false, true] {
            for introspection in [false, true] {
                for device in [false, true] {
                    let mut s = Settings::default();
                    s.a2a.listen = Some("http://127.0.0.1:0".into());
                    s.a2a.events.enabled = events;
                    s.a2a.introspection.enabled = introspection;
                    s.a2a.device_grant.enabled = device;
                    let ops = command_ops_of(&s);
                    assert!(!ops.iter().any(|o| o.starts_with("_instance.")), "{ops:?}");
                }
            }
        }
        // And the ask_human row, held but never served, is in neither.
        assert!(bundle["$defs"]["ops"].get("ask_human").is_none());
        assert!(bundle["$defs"]["reserved"].get("ask_human").is_none());
    }
}
