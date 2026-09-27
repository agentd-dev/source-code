// SPDX-License-Identifier: AGPL-3.0-only
//! **Principals, roles and authorization**: every A2A caller is resolved to a
//! principal (identity from mTLS SAN / bearer subject / AAuth agent id) with a
//! role (`operator | user | agent | anonymous`), a set of granted tool
//! patterns, and optional per-principal quotas. The authorization matrix —
//! which methods and commands a role may call — is decided here; the served
//! surface calls [`Resolver::resolve`] then [`Principal::may`]. Anything that
//! does not match a rule lands on the anonymous principal, which may call
//! nothing, so a caller agentd cannot identify gets no surface at all.

use crate::config::v2::{self, Role};

mod addressee;
mod resolve;

pub use addressee::Addressee;
pub use resolve::{CallerIdentity, Resolver};

/// A resolved caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    /// A stable id for logs/audit/context (`operator`, `user:<sub>`, `agent:<id>`).
    pub id: String,
    pub role: Role,
    /// Explicit tool-name grants (patterns) beyond the role defaults.
    pub grants: Vec<String>,
    /// The rate quota (`"<burst>/<per>s"`) and budget scope key, if any.
    pub rate: Option<String>,
    pub budget: Option<v2::Budget>,
    /// Operator-declared attributes that travel with everything this
    /// principal causes (the run, the MCP `_meta`, the audit line).
    pub labels: std::collections::BTreeMap<String, String>,
}

impl Principal {
    pub fn anonymous() -> Principal {
        Principal {
            id: "anonymous".into(),
            role: Role::Anonymous,
            grants: Vec::new(),
            rate: None,
            budget: None,
            labels: Default::default(),
        }
    }

    pub fn is_operator(&self) -> bool {
        self.role == Role::Operator
    }
    pub fn is_anonymous(&self) -> bool {
        self.role == Role::Anonymous
    }

    /// May this principal invoke A2A method `method`?
    /// `op` is the command tool name for a command DataPart (else `None`).
    ///
    /// The matrix is deny-by-default: anonymous callers get nothing, operators
    /// get everything, and every other role is limited to the named read/task
    /// methods below — an unrecognised method falls through to `false`.
    pub fn may(&self, method: &str, op: Option<&str>) -> bool {
        match self.role {
            Role::Anonymous => false,
            Role::Operator => true,
            _ => match method {
                // The read/task methods every non-anonymous role may use on its
                // own conversations/tasks (ownership is enforced at the object).
                // `SubscribeToEvents` is principal-scoped at the feed itself,
                // so any non-anonymous role may attach and will only see the
                // frames belonging to it.
                "SendMessage"
                | "SendStreamingMessage"
                | "GetTask"
                | "CancelTask"
                | "ListTasks"
                | "SubscribeToTask"
                | "SubscribeToEvents"
                // The push-notification family is scoped to the caller's own
                // tasks the same way `GetTask` is — ownership is enforced at
                // the task, so any named caller may manage webhooks on what it
                // started.
                | "CreateTaskPushNotificationConfig"
                | "GetTaskPushNotificationConfig"
                | "ListTaskPushNotificationConfigs"
                | "DeleteTaskPushNotificationConfig"
                // The extended card is the *authenticated* card: any named
                // caller may read it, and it is scoped to what they may run.
                | "GetExtendedAgentCard" => match op {
                    None => true, // natural language / streaming
                    Some(tool) => self.may_command(tool),
                },
                _ => false,
            },
        }
    }

    /// May this principal run command tool `tool`?
    pub fn may_command(&self, tool: &str) -> bool {
        if self.role == Role::Anonymous {
            return false;
        }
        // The admin family answers to the ROLE alone — checked before grants,
        // because `grants: ["*"]` on a `user` must not hand out the power to
        // drain the instance. The JSON-RPC spelling was hard-denied for
        // non-operators regardless of grants; moving these onto the command
        // surface must not quietly relax that.
        if is_admin_op(tool) {
            return self.role == Role::Operator;
        }
        if tool == "status" || tool == "interface.info" {
            // Liveness and capability discovery: a named caller must be able to
            // learn whether agentd is up and what interface it offers before it
            // can ask for anything else, so these need no grant. Neither leaks
            // work product, and the interface gate still applies at the op.
            return true;
        }
        if self
            .grants
            .iter()
            .any(|p| crate::registry::pattern_matches(p, tool))
        {
            return true;
        }
        // Role defaults for command ops. The debug reads
        // (`conversation.get`/`run.get`) are owner-scoped at the object; the
        // log ring (`debug.events`) stays operator-only, because it spans every
        // principal's activity and cannot be scoped to the caller.
        match self.role {
            Role::Operator => true,
            Role::User => matches!(
                tool,
                "workflow.run"
                    | "workflow.status"
                    | "workflow.cancel"
                    | "subagent.send"
                    | "subagent.status"
                    | "plan.get"
                    | "ask_human"
                    | "conversation.get"
                    | "run.get"
            ),
            Role::Agent => matches!(tool, "workflow.run" | "workflow.status"),
            Role::Anonymous => false,
        }
    }

    /// The governor scope this principal's spend is charged to.
    pub fn scope_key(&self) -> String {
        scope_key_for(&self.id)
    }
}

/// The governor scope key for a principal id. One source for the format,
/// because the runtime carries only the id once work is under way while the
/// A2A layer still holds the whole `Principal`.
pub fn scope_key_for(id: &str) -> String {
    format!("principal:{id}")
}

/// The admin COMMAND ops — the A2A-compliant spelling, invoked through
/// `SendMessage` with a command DataPart like every other op.
///
/// Operator-only unconditionally: unlike an ordinary command, an explicit
/// `grants:` entry does NOT reach these. A principal that could drain the
/// instance it is talking to is not a peer, it is an operator, and the two
/// are different roles on purpose.
pub fn is_admin_op(op: &str) -> bool {
    matches!(
        op,
        "admin.drain" | "admin.lameduck" | "admin.pause" | "admin.resume" | "admin.cancel"
    )
}

/// A `*`-glob match (`*` = any run of chars; else literal).
fn glob(pattern: &str, s: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(pos) = pattern.find('*') {
        let (pre, post) = (&pattern[..pos], &pattern[pos + 1..]);
        return s.starts_with(pre) && s.ends_with(post) && s.len() >= pre.len() + post.len();
    }
    pattern == s
}
