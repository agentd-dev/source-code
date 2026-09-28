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
use crate::runtime::surface::{self, Floor};
use serde_json::Value;

mod addressee;
mod resolve;

pub use addressee::Addressee;
pub use resolve::{
    CertId, Evidence, Resolution, Resolver, SESSION_TOKEN_PREFIX, SessionCheck, SessionVerifier,
    Via,
};

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
    /// `op` is the command op of a command DataPart (else `None`).
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
                    Some(op) => self.may_command(op),
                },
                _ => false,
            },
        }
    }

    /// May this principal run command op `op`?
    ///
    /// The op's FLOOR is consulted before any grant. An operator-floor op
    /// answers to the role alone, because `grants: ["*"]` — or any prefix
    /// pattern that happens to cover it — on a `user` must not hand out the
    /// power to drain the instance, relax its approval policy or read every
    /// principal's log lines; grants used to be checked first, and that is how
    /// an ordinary wildcard grant reached the operator controls. An op that is
    /// no row of the table is a workflow-declared command, which the grants
    /// decide and the start node's `roles:` then filter.
    pub fn may_command(&self, op: &str) -> bool {
        if self.is_anonymous() {
            return false;
        }
        let Some(spec) = surface::op_spec(op) else {
            return self.is_operator() || self.granted(op);
        };
        match spec.floor {
            Floor::Operator => self.is_operator(),
            Floor::AnyNamed => true,
            Floor::Granted => {
                self.is_operator()
                    || spec.defaults.contains(&self.role)
                    || self.granted(op)
                    // A scoped `workflow.run:<pattern>` grant is a grant of
                    // the op, narrowed to the workflows it names.
                    || (op == "workflow.run" && self.scoped_run_grants().next().is_some())
            }
        }
    }

    /// May this principal start the workflow `name`?
    ///
    /// Scoped `workflow.run:<pattern>` grants REPLACE the role default rather
    /// than adding to it: an operator who writes `workflow.run:deploy-*` for a
    /// user means "these and no others", which would be meaningless if the
    /// user role's default `workflow.run` still admitted every workflow.
    pub fn may_run_workflow(&self, name: &str) -> bool {
        if self.is_anonymous() {
            return false;
        }
        if self.is_operator() {
            return true;
        }
        let target = format!("workflow.run:{name}");
        let mut scoped = self.scoped_run_grants().peekable();
        if scoped.peek().is_some() {
            return scoped.any(|p| crate::registry::pattern_matches(p, &target));
        }
        self.may_command("workflow.run")
    }

    /// Authorize the command `op` with its arguments `data`: the op itself,
    /// then — for `workflow.run` — the workflow it names. `Err` carries the
    /// refusal's text.
    pub fn authorize_command(&self, op: &str, data: &Value) -> Result<(), String> {
        if !self.may_command(op) {
            return Err(format!("command {op:?} is not granted to {}", self.id));
        }
        if op == "workflow.run"
            && let Some(name) = workflow_name_of(data)
            && !self.may_run_workflow(name)
        {
            return Err(format!("workflow {name:?} is not granted to {}", self.id));
        }
        Ok(())
    }

    /// Does an explicit grant cover `op`?
    fn granted(&self, op: &str) -> bool {
        self.grants
            .iter()
            .any(|p| crate::registry::pattern_matches(p, op))
    }

    /// The grants scoped to named workflows.
    fn scoped_run_grants(&self) -> impl Iterator<Item = &str> {
        self.grants
            .iter()
            .map(String::as_str)
            .filter(|g| g.starts_with("workflow.run:"))
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

/// The workflow a `workflow.run` names — `workflow`, the one spelling.
pub fn workflow_name_of(data: &Value) -> Option<&str> {
    data.get("workflow").and_then(Value::as_str)
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

impl crate::runtime::reactor::Runtime {
    /// Whether `principal` may start `wf`: the grants allow the workflow, and
    /// its default start node's `roles:` admit the caller. The one predicate
    /// every way of starting a workflow on someone's behalf asks, so the card
    /// can never list a workflow the op then refuses. Here rather than beside
    /// the A2A dispatch because the model's tools and signal starts ask it
    /// too, and they are compiled without the listener — until they do, only
    /// the listener's builds use it.
    #[cfg(feature = "a2a")]
    pub(crate) fn may_run(principal: &Principal, wf: &crate::engine::model::Workflow) -> bool {
        let role = serde_json::to_value(principal.role).unwrap_or(Value::Null);
        principal.may_run_workflow(&wf.name)
            && role.as_str().is_some_and(|r| wf.default_start_admits(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::surface::OPS;

    fn with(role: Role, grants: &[&str]) -> Principal {
        Principal {
            role,
            grants: grants.iter().map(|g| (*g).to_string()).collect(),
            ..Principal::anonymous()
        }
    }

    /// An operator-floor op answers to the ROLE, never to a grant — every
    /// row of the table, and the `_instance.*` reports a child sends home.
    /// Grants used to be consulted first, so `grants: ["*"]` on a user
    /// reached `config.set` (and through it `agent.approval`), `config` and
    /// the cross-principal log ring.
    #[test]
    fn operator_floor_ops_answer_to_the_role_whatever_the_grants_say() {
        let floor: Vec<&str> = OPS
            .iter()
            .filter(|s| s.floor == Floor::Operator)
            .map(|s| s.name)
            .filter(|n| !n.ends_with('.'))
            .chain(["_instance.result", "_instance.emit"])
            .collect();
        for must in [
            "config",
            "debug.events",
            "admin.set",
            "admin.drain",
            "_instance.result",
        ] {
            assert!(floor.contains(&must), "{must} is on the operator floor");
        }
        for op in floor {
            assert!(
                with(Role::Operator, &[]).may_command(op),
                "an operator may {op} with no grants at all"
            );
            // Every prefix of the op, as a pattern: `a*`, `ad*`, … `admin.set*`.
            let prefixes: Vec<String> = (1..=op.len())
                .filter(|i| op.is_char_boundary(*i))
                .map(|i| format!("{}*", &op[..i]))
                .collect();
            for role in [Role::User, Role::Agent, Role::Anonymous] {
                assert!(
                    !with(role, &["*"]).may_command(op),
                    "{role:?} ['*'] must not {op}"
                );
                assert!(
                    !with(role, &[op]).may_command(op),
                    "{role:?} [{op}] must not {op}"
                );
                for p in &prefixes {
                    assert!(
                        !with(role, &[p.as_str()]).may_command(op),
                        "{role:?} [{p}] must not {op}"
                    );
                }
                assert!(
                    with(role, &["*"])
                        .authorize_command(op, &Value::Null)
                        .is_err(),
                    "the belt check agrees for {op}"
                );
            }
        }
        // …while an ordinary op still answers to grants and role defaults.
        assert!(with(Role::Agent, &["subagent.kill"]).may_command("subagent.kill"));
        assert!(!with(Role::Agent, &[]).may_command("subagent.kill"));
        assert!(with(Role::User, &[]).may_command("subagent.send"));
        // `status` needs no grant at all; anonymous still gets nothing.
        assert!(with(Role::Agent, &[]).may_command("status"));
        assert!(!with(Role::Anonymous, &["*"]).may_command("status"));
        // Nothing grants a removed op back.
        for (op, _) in crate::runtime::surface::REMOVED_OPS {
            assert!(!with(Role::User, &[]).may_command(op), "{op}");
        }
    }

    /// A scoped `workflow.run:<pattern>` grant REPLACES the role default:
    /// the user it is written for may run those workflows and no others,
    /// though the user role alone could run any.
    #[test]
    fn a_scoped_workflow_grant_replaces_the_role_default() {
        let plain = with(Role::User, &[]);
        assert!(plain.may_run_workflow("deploy-web") && plain.may_run_workflow("wipe"));

        let scoped = with(Role::User, &["workflow.run:deploy-*"]);
        assert!(scoped.may_command("workflow.run"));
        assert!(scoped.may_run_workflow("deploy-web"));
        assert!(
            !scoped.may_run_workflow("wipe"),
            "the scope replaces the default"
        );
        assert!(
            scoped
                .authorize_command("workflow.run", &serde_json::json!({"workflow": "wipe"}))
                .is_err()
        );
        assert!(
            scoped
                .authorize_command(
                    "workflow.run",
                    &serde_json::json!({"workflow": "deploy-api"})
                )
                .is_ok()
        );
        // A role with no default gets the op from the scoped grant alone.
        let agent = with(Role::Agent, &["workflow.run:report"]);
        assert!(agent.may_command("workflow.run") && agent.may_run_workflow("report"));
        assert!(!agent.may_run_workflow("deploy-web"));
        // A wildcard is not a scope, so it does not narrow anything.
        assert!(with(Role::Agent, &["*"]).may_run_workflow("anything"));
        assert!(with(Role::Operator, &["workflow.run:x"]).may_run_workflow("y"));
        assert!(!Principal::anonymous().may_run_workflow("x"));
        // The one spelling of the workflow's name.
        assert_eq!(
            workflow_name_of(&serde_json::json!({"workflow": "a", "name": "b"})),
            Some("a")
        );
        assert_eq!(workflow_name_of(&serde_json::json!({"name": "b"})), None);
    }

    /// `may_run` is both halves: the grants allow the workflow, AND its
    /// default start's `roles:` admit the caller. A user holding every grant
    /// still may not run a workflow whose only start is an operator's.
    #[cfg(feature = "a2a")]
    #[test]
    fn may_run_asks_the_grants_and_the_default_starts_roles() {
        use crate::runtime::reactor::Runtime;
        let wf = |start: Value| {
            crate::engine::model::parse_workflow(&serde_json::json!({
                "name": "deploy-web", "steps": {
                    "s": start,
                    "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}}}))
            .unwrap()
        };
        let gated = wf(serde_json::json!({"kind": "a2a", "command": "x", "roles": ["operator"]}));
        let open = wf(serde_json::json!({"kind": "a2a", "command": "x"}));
        assert!(!Runtime::may_run(&with(Role::User, &["*"]), &gated));
        assert!(!Runtime::may_run(&with(Role::Agent, &["*"]), &gated));
        assert!(Runtime::may_run(&with(Role::Operator, &[]), &gated));
        assert!(Runtime::may_run(&with(Role::User, &[]), &open));
        // …and the grants half: a scope that excludes the name refuses it
        // whatever the start admits.
        assert!(!Runtime::may_run(
            &with(Role::User, &["workflow.run:wipe"]),
            &open
        ));
        assert!(Runtime::may_run(
            &with(Role::User, &["workflow.run:deploy-*"]),
            &open
        ));
    }
}
