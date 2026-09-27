// SPDX-License-Identifier: AGPL-3.0-only
//! Durable tasks: the task methods, the push-notification family, and the
//! lifecycle every other path creates and advances tasks through.

use super::{FeedVis, TASK_NOT_FOUND, err_obj};
use crate::a2a::Principal;
use crate::a2a::tasks::{Link, State, Task};
use crate::runtime::reactor::{PendingKind, Runtime};
use serde_json::{Value, json};

/// A fresh durable task id.
///
/// A ULID, not the reactor's `seq` counter: `seq` starts at 0 in every life
/// while tasks are RESTORED from the store, so a counter-minted id names a task
/// from a PREVIOUS life. That collision is not benign — an id that already
/// exists makes `a2a_send` read the message as a continuation, so the caller is
/// handed someone else's task (and its history) while an unrelated message
/// advances that task's state. ULID is what every other durable id in the store
/// is minted from (runs, inbox events, artifacts) for exactly this reason, and
/// it keeps ids time-sortable.
pub(super) fn new_task_id() -> String {
    format!("task-{}", crate::state::ulid::new())
}

impl Runtime {
    pub(super) fn a2a_get_task(&self, principal: &Principal, params: &Value) -> Value {
        let id = params
            .get("id")
            .or_else(|| params.get("taskId"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match self.tasks.get(id) {
            Some(t)
                if principal.is_operator()
                    || t.principal.as_deref() == Some(principal.id.as_str()) =>
            {
                t.to_a2a()
            }
            // Don't disclose existence to a non-owner.
            _ => err_obj(TASK_NOT_FOUND, "task not found"),
        }
    }

    pub(super) fn a2a_list_tasks(&self, principal: &Principal) -> Value {
        let tasks: Vec<Value> = self
            .tasks
            .values()
            .filter(|t| {
                principal.is_operator() || t.principal.as_deref() == Some(principal.id.as_str())
            })
            .map(|t| t.summary())
            .collect();
        // `ListTasksResult` is a fixed shape: a peer's generated type has
        // `totalSize`/`pageSize`/`nextPageToken` as non-optional. We return the
        // whole set in one page, so the token is empty and the sizes agree.
        let n = tasks.len();
        json!({"tasks": tasks, "totalSize": n, "pageSize": n, "nextPageToken": ""})
    }

    // ---- push notifications (the `*TaskPushNotificationConfig` family) -----

    /// The task this request names, if the caller may touch it.
    ///
    /// "Not yours" and "does not exist" answer identically on purpose: a caller
    /// must not be able to probe for other principals' task ids.
    fn owned_task(&self, principal: &Principal, params: &Value) -> Result<String, Value> {
        let id = params
            .get("taskId")
            .or_else(|| params.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match self.tasks.get(id) {
            Some(t)
                if principal.is_operator()
                    || t.principal.as_deref() == Some(principal.id.as_str()) =>
            {
                Ok(id.to_string())
            }
            _ => Err(err_obj(TASK_NOT_FOUND, "task not found")),
        }
    }

    fn push_enabled(&self) -> Result<(), Value> {
        if self.settings.a2a.push.enabled {
            Ok(())
        } else {
            Err(err_obj(
                -32003,
                "push notifications are not enabled (set a2a.push.enabled: true)",
            ))
        }
    }

    /// Register (or replace) a webhook for a task.
    ///
    /// The target is checked here, while the caller is present to be told why —
    /// a refused URL is a `-32602` with a reason, not a delivery that silently
    /// never happens.
    pub(super) fn a2a_push_set(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let cfg = params
            .get("pushNotificationConfig")
            .or_else(|| params.get("config"))
            .cloned()
            .unwrap_or_else(|| params.clone());
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let id = cfg
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("push"));
        let target = match crate::a2a::push::from_wire(&cfg, id.clone()) {
            Ok(t) => t,
            Err(e) => return err_obj(::mcp::rpc::INVALID_PARAMS, &e),
        };
        let allow_private = self.settings.a2a.push.allow_private;
        if let Err(e) = crate::a2a::push::check_url(&target.url, allow_private) {
            return err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!("push url refused: {e}"),
            );
        }
        // In `closed` mode a caller-chosen push target must clear the service
        // catalog as well as the SSRF check above. The two guards answer
        // different questions — "is this address reachable from here?" and "is
        // this endpoint one we declared?" — and a push URL comes from the
        // caller, so both have to hold.
        if let Err(e) = crate::config::v2::egress_allows(
            &self.settings.services,
            self.settings.security.egress,
            crate::config::v2::ServiceKind::Http,
            &target.url,
        ) {
            return err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!("push url refused: {e}"),
            );
        }
        let wire = crate::a2a::push::to_wire(&task_id, &target);
        if let Some(t) = self.tasks.get_mut(&task_id) {
            t.push.retain(|p| p.id != id);
            t.push.push(target);
            t.dirty = true;
        }
        self.task_persist(&task_id);
        self.log.info(
            "a2a.push.registered",
            json!({"task": task_id, "config": id, "principal": principal.id}),
        );
        wire
    }

    pub(super) fn a2a_push_get(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let want = params
            .get("pushNotificationConfigId")
            .or_else(|| params.get("configId"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match self
            .tasks
            .get(&task_id)
            .and_then(|t| t.push.iter().find(|p| want.is_empty() || p.id == want))
        {
            Some(p) => crate::a2a::push::to_wire(&task_id, p),
            None => err_obj(TASK_NOT_FOUND, "no such push notification config"),
        }
    }

    pub(super) fn a2a_push_list(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let configs: Vec<Value> = self
            .tasks
            .get(&task_id)
            .map(|t| {
                t.push
                    .iter()
                    .map(|p| crate::a2a::push::to_wire(&task_id, p))
                    .collect()
            })
            .unwrap_or_default();
        json!({ "configs": configs })
    }

    pub(super) fn a2a_push_delete(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let want = params
            .get("pushNotificationConfigId")
            .or_else(|| params.get("configId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(t) = self.tasks.get_mut(&task_id) {
            t.push.retain(|p| !want.is_empty() && p.id != want);
            t.dirty = true;
        }
        self.task_persist(&task_id);
        json!({})
    }

    pub(super) fn a2a_cancel_task(&mut self, principal: &Principal, params: &Value) -> Value {
        let id = params
            .get("id")
            .or_else(|| params.get("taskId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let owned = match self.tasks.get(&id) {
            Some(t) => {
                principal.is_operator() || t.principal.as_deref() == Some(principal.id.as_str())
            }
            None => false,
        };
        if !owned {
            return err_obj(TASK_NOT_FOUND, "task not found");
        }
        if self.tasks.get(&id).is_some_and(|t| t.state.is_terminal()) {
            return self.tasks.get(&id).map(Task::to_a2a).unwrap_or(Value::Null);
        }
        // A live human gate on this task: unblock the asker with an error so
        // the turn or step resolves instead of dangling until its ask timeout.
        if let Some(i) = self
            .pending
            .iter()
            .position(|p| matches!(&p.kind, PendingKind::Human { task, .. } if task == &id))
        {
            self.human_fail(i, "ask_human: the gate task was cancelled");
        }
        match self.tasks.get(&id).map(|t| t.link.clone()) {
            Some(Link::Run { id: run }) if self.runs.contains_key(&run) => {
                self.cancel_run(&run, "task cancelled over A2A")
            }
            Some(Link::Subagent { handle }) => {
                if let Some(node) = self.subagents.get(&handle).and_then(|s| s.node) {
                    self.children.cancel(node, "task cancelled over A2A");
                }
            }
            _ => {}
        }
        if let Some(t) = self.tasks.get_mut(&id) {
            t.transition(State::Canceled, Some("cancelled".into()));
        }
        self.task_persist(&id);
        self.task_sync(&id);
        self.tasks.get(&id).map(Task::to_a2a).unwrap_or(Value::Null)
    }

    // ---- task lifecycle ----------------------------------------------------

    /// Create + persist a fresh task; publish it to the shared view.
    /// Create a task, taking the listener's reserved id when the request that
    /// is being served brought one (see [`Runtime::reserved_task_id`]).
    pub(crate) fn task_create(&mut self, ctx: &str, principal: &Principal, link: Link) -> String {
        let id = match self.reserved_task_id.take() {
            Some(id) => id,
            None => new_task_id(),
        };
        let task = Task::new(&id, ctx, Some(&principal.id), link);
        self.tasks.insert(id.clone(), task);
        self.task_persist(&id);
        self.task_sync(&id);
        id
    }

    /// A command that finishes at once: create the task already terminal.
    pub(super) fn task_complete_now(
        &mut self,
        ctx: &str,
        principal: &Principal,
        link: Link,
        state: State,
        text: Option<String>,
        result: Option<Value>,
    ) -> Value {
        let id = self.task_create(ctx, principal, link);
        if let Some(t) = self.tasks.get_mut(&id) {
            if let Some(r) = result {
                t.set_result(r);
            }
            t.transition(state, text);
        }
        self.task_persist(&id);
        self.task_sync(&id);
        json!({"task": self.tasks.get(&id).map(Task::to_a2a).unwrap_or(Value::Null)})
    }

    /// Publish a task transition: to A2A subscribers, and onto the interface
    /// feed so every attached display client converges without polling.
    ///
    /// The A2A half is also what *settles a blocking send* — the protocol layer
    /// waits on this stream rather than polling a snapshot — so a task that
    /// never publishes is a task that never finishes as far as a caller can
    /// tell.
    pub(crate) fn task_sync(&self, id: &str) {
        let Some(sink) = &self.a2a_sink else {
            return;
        };
        let allow_private = self.settings.a2a.push.allow_private;
        match self.tasks.get(id) {
            Some(t) => {
                // Send the artifact BEFORE the terminal status frame. A
                // conformant streaming client stops reading as soon as it sees
                // a terminal state, so a result frame sent after it is a result
                // nobody reads.
                if t.state.is_terminal()
                    && let Some(a) = crate::a2a::wire::result_artifact(t)
                {
                    sink.artifact(&t.id, &t.context_id, a.clone());
                }
                sink.status(
                    &t.id,
                    &t.context_id,
                    t.state.to_wire(),
                    t.message.as_deref(),
                    t.updated,
                );
                // A caller that asked to be told rather than to watch. Fired
                // from here because this is the one place every transition
                // passes through, whatever caused it.
                if !t.push.is_empty() {
                    sink.push(t, allow_private);
                }
                self.feed_push(
                    "task",
                    FeedVis::Owner(t.principal.clone()),
                    json!({"task": t.to_a2a(), "link": t.link, "principal": t.principal}),
                );
            }
            None => {
                self.feed_push("task.removed", FeedVis::Operator, json!({"id": id}));
            }
        }
    }

    /// Persist a task if dirty (durable across restarts — `GetTask` survives).
    pub(crate) fn task_persist(&mut self, id: &str) {
        if !self.tasks.get(id).is_some_and(|t| t.dirty) {
            return;
        }
        let encoded = self.tasks.get(id).map(serde_json::to_value);
        match encoded {
            Some(Ok(v)) => {
                if let Err(e) = self.durable.put(crate::state::Kind::Task, id, v, None) {
                    self.log.warn(
                        "a2a.task.persist.fail",
                        json!({"task": id, "err": e.to_string()}),
                    );
                } else if let Some(t) = self.tasks.get_mut(id) {
                    t.dirty = false;
                }
            }
            Some(Err(e)) => self.log.warn(
                "a2a.task.encode.fail",
                json!({"task": id, "err": e.to_string()}),
            ),
            None => {}
        }
    }

    /// Resolve the task bound to a completed conversation turn (via its inbox
    /// event) and drive it to a terminal / input-required state.
    pub(crate) fn a2a_task_for_event(
        &mut self,
        event: Option<&str>,
        state: State,
        text: Option<String>,
        result: Option<Value>,
    ) {
        let Some(ev) = event else { return };
        let Some(task_id) = self.event_to_task.remove(ev) else {
            return;
        };
        if let Some(t) = self.tasks.get_mut(&task_id) {
            if let Some(r) = result {
                t.set_result(r);
            }
            t.transition(state, text);
        }
        self.task_persist(&task_id);
        self.task_sync(&task_id);
    }

    /// Drive the task bound to a finished run to match the run's outcome.
    pub(crate) fn a2a_task_for_run(
        &mut self,
        task_id: &str,
        status: &str,
        output: Option<&Value>,
        error: Option<&str>,
    ) {
        if !self.tasks.contains_key(task_id) {
            return;
        }
        let state = State::from_run(status);
        if let Some(t) = self.tasks.get_mut(task_id) {
            if let Some(o) = output {
                t.set_result(o.clone());
            }
            t.transition(state, error.map(str::to_string));
        }
        self.task_persist(task_id);
        self.task_sync(task_id);
    }

    pub(super) fn a2a_task_fail(&mut self, id: &str, err: &str) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.transition(State::Failed, Some(err.to_string()));
        }
        self.task_persist(id);
        self.task_sync(id);
    }

    /// Restore durable tasks at startup: seed the shared view the transport
    /// threads read, and re-arm run-linked human gates so a question asked
    /// before the restart is still answerable after it.
    pub(crate) fn restore_a2a_tasks(&mut self, envs: &[crate::store::Envelope]) {
        for env in envs {
            match serde_json::from_value::<Task>(env.state.clone()) {
                Ok(t) => {
                    let id = t.id.clone();
                    self.tasks.insert(id.clone(), t);
                    self.task_sync(&id);
                }
                Err(e) => self.log.warn(
                    "restore.task.corrupt",
                    json!({"id": env.id, "err": e.to_string()}),
                ),
            }
        }
        self.rebuild_human_asks();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_are_ulids_so_two_lives_cannot_mint_the_same_one() {
        // The whole point of the ULID: `seq` restarts at 0 with the process,
        // and the ids of a previous life are still in the store.
        let a = new_task_id();
        let b = new_task_id();
        assert_ne!(a, b);
        assert!(a.starts_with("task-"), "the id keeps its prefix: {a}");
        assert_eq!(a.len(), "task-".len() + 26, "a 26-char ULID: {a}");
        assert!(a < b, "still time-sortable: {a} < {b}");
    }
}
