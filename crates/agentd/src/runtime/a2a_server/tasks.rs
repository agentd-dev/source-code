// SPDX-License-Identifier: AGPL-3.0-only
//! Durable tasks: the task methods, the push-notification family, and the
//! lifecycle every other path creates and advances tasks through.

use super::{FeedVis, TASK_NOT_FOUND, err_obj};
use crate::a2a::Principal;
use crate::a2a::errors::{INVALID_PARAMS, TASK_NOT_CANCELABLE};
use crate::a2a::tasks::{Link, PushTarget, State, Task};
use crate::a2a::wire::{Annotations, DEFAULT_PAGE_SIZE, ListView, MAX_PAGE_SIZE};
use crate::runtime::reactor::{PendingKind, Runtime};
use crate::runtime::surface::{Active, Ext};
use serde_json::{Value, json};

/// A fresh durable task id, for a task the runtime opens on its own — a
/// gate's. A task a send creates takes the id a2a-rs generated for it instead,
/// and this is the same generator, so every task id has one shape.
///
/// Never the reactor's `seq` counter: `seq` starts at 0 in every life while
/// tasks are RESTORED from the store, so a counter-minted id names a task from
/// a PREVIOUS life — and a message naming it would be read as a continuation,
/// handing the caller someone else's task and history. A random UUIDv4 cannot
/// collide with an id already in the store.
pub(super) fn new_task_id() -> String {
    a2a_rs::domain::TaskId::generate().into_string()
}

/// A fresh push-config id, for a registration that named none.
///
/// A ULID for the same reason [`new_task_id`] is random: configs are durable
/// with their task, and a counter that restarts with the process would mint an id a
/// restored task already holds — and registering under it REPLACES that
/// config, silently dropping a webhook somebody else was promised.
fn new_push_id() -> String {
    format!("push-{}", crate::state::ulid::new())
}

/// `-32602` with a reason: a request the caller can fix.
fn invalid(msg: &str) -> Value {
    err_obj(INVALID_PARAMS, msg)
}

/// A listing's `pageSize`: unset is the default, anything outside the spec's
/// range is refused rather than clamped — a caller that asked for 500 and got
/// 100 would read a full page as the end of the list.
fn page_size_of(asked: Option<i64>) -> Result<usize, Value> {
    match asked {
        None => Ok(DEFAULT_PAGE_SIZE as usize),
        Some(n) if (1..=MAX_PAGE_SIZE as i64).contains(&n) => Ok(n as usize),
        Some(n) => Err(invalid(&format!(
            "pageSize must be between 1 and {MAX_PAGE_SIZE}, not {n}"
        ))),
    }
}

/// base64url (RFC 4648 §5, unpadded): the page tokens' outer form, so a token
/// is opaque to the caller and safe in any transport. Decoded with
/// [`crate::config::envelope::b64url_decode`].
fn b64url(bytes: &[u8]) -> String {
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(URL[(n >> 18 & 63) as usize] as char);
        out.push(URL[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(URL[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(URL[(n & 63) as usize] as char);
        }
    }
    out
}

/// A page token's payload, or `None` for anything this server did not issue.
fn token_payload(token: &str) -> Option<String> {
    String::from_utf8(crate::config::envelope::b64url_decode(token)?).ok()
}

/// A task's place in the listing order: newest status first, ties broken by id
/// (descending) so the order is total and a cursor names exactly one position.
fn list_key(t: &Task) -> (u64, &str) {
    (t.updated, t.id.as_str())
}

/// The `ListTasks` cursor: where the previous page stopped, as
/// `<updated_ms>:<id>`. A position in the order, not an index into it — a
/// task that moves or disappears between pages cannot shift the next one onto
/// something already seen.
fn list_cursor(t: &Task) -> String {
    b64url(format!("{}:{}", t.updated, t.id).as_bytes())
}

fn parse_list_cursor(token: &str) -> Option<(u64, String)> {
    let payload = token_payload(token)?;
    let (ms, id) = payload.split_once(':')?;
    let ms = ms.parse().ok()?;
    (!id.is_empty()).then(|| (ms, id.to_string()))
}

/// An RFC 3339 instant as epoch milliseconds, rounded UP: a task's status time
/// is whole milliseconds, so `updated >= ceil(t)` is exactly `updated >= t` —
/// rounding down would admit a task that moved a fraction of a millisecond
/// before the instant asked for. Parsed by the spec's own `Timestamp`, so every
/// offset form the proto JSON mapping accepts is accepted here.
fn instant_ms(s: &str) -> Option<u64> {
    let t: buffa_types::google::protobuf::Timestamp =
        serde_json::from_value(Value::String(s.to_string())).ok()?;
    let ms =
        t.seconds.saturating_mul(1000) + (u32::try_from(t.nanos).ok()?).div_ceil(1_000_000) as i64;
    Some(ms.max(0) as u64)
}

/// One page of `ListTasks` over `tasks`, as `principal` may see them.
///
/// `params` is the serde form of the spec's `ListTasksParams`, whole — every
/// filter the caller named is applied, and one this cannot honour is refused
/// with `-32602` rather than ignored:
///
/// * visibility first: a non-operator sees its own tasks and nothing else, so
///   `totalSize` counts only what the caller may know exists;
/// * `contextId`, `status`, and `statusTimestampAfter` (inclusive) narrow it —
///   `contextId` as [`Task::matches_context`] reads it, so an operator finds a
///   conversation by the runtime's key or by any owner's name for it, and
///   everyone else by its own name only;
/// * the order is newest status first, then id descending, and `pageToken`
///   resumes after the task the previous page ended on;
/// * `historyLength` and `includeArtifacts` shape each task (see [`ListView`]).
fn list_tasks<'a>(
    tasks: impl Iterator<Item = &'a Task>,
    principal: &Principal,
    params: &Value,
    ann: Annotations,
) -> Result<Value, Value> {
    let p: a2a_rs::domain::ListTasksParams = serde_json::from_value(params.clone())
        .map_err(|e| invalid(&format!("ListTasks params: {e}")))?;
    let page_size = page_size_of(p.page_size.map(i64::from))?;
    let history_length = match p.history_length {
        None => None,
        Some(n) => Some(
            u32::try_from(n)
                .map_err(|_| invalid(&format!("historyLength must be >= 0, not {n}")))?,
        ),
    };
    let view = ListView {
        history_length,
        include_artifacts: p.include_artifacts == Some(true),
    };
    let status = match p.status {
        Some(a2a_rs::domain::TaskState::TASK_STATE_UNSPECIFIED) => {
            return Err(invalid("status is not a task state"));
        }
        other => other,
    };
    let after = match p.status_timestamp_after.as_deref() {
        None => None,
        Some(s) => Some(instant_ms(s).ok_or_else(|| {
            invalid(&format!(
                "statusTimestampAfter is not an RFC 3339 instant: {s:?}"
            ))
        })?),
    };
    let cursor = match p.page_token.as_deref() {
        None | Some("") => None,
        Some(tok) => Some(
            parse_list_cursor(tok)
                .ok_or_else(|| invalid("pageToken is not one this server issued"))?,
        ),
    };

    let operator = principal.is_operator();
    let mut hits: Vec<&Task> = tasks
        .filter(|t| t.is_visible_to(principal))
        .filter(|t| {
            p.context_id
                .as_deref()
                .is_none_or(|c| t.matches_context(c, operator))
        })
        .filter(|t| status.is_none_or(|s| t.state.to_wire() == s))
        .filter(|t| after.is_none_or(|ms| t.updated >= ms))
        .collect();
    hits.sort_by(|a, b| list_key(b).cmp(&list_key(a)));
    let total = hits.len();
    let start = match &cursor {
        None => 0,
        Some((ms, id)) => hits
            .iter()
            .position(|t| list_key(t) < (*ms, id.as_str()))
            .unwrap_or(total),
    };
    let page = &hits[start..(start + page_size).min(total)];
    let next = match page.last() {
        Some(last) if start + page.len() < total => list_cursor(last),
        _ => String::new(),
    };
    Ok(json!({
        "tasks": page.iter().map(|t| t.summary(view, ann)).collect::<Vec<_>>(),
        // Always present, empty on the last page: the spec's response has it
        // REQUIRED, and "absent" must never be readable as "there is more".
        "nextPageToken": next,
        "pageSize": page_size,
        "totalSize": total,
    }))
}

/// How often the tick applies `store.retention.tasks`. A terminal transition
/// applies it at once (see [`Runtime::task_sync`]); the tick only has to catch
/// what no transition does — a `ttl` passing, a reload lowering the bound — and
/// a second late is nothing to either.
const TASK_SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a settled task nobody has read back yet is kept past the bound.
///
/// A blocking `SendMessage` is answered in two trips through the reactor: the
/// terminal event ends a2a-rs's wait, and a2a-rs then fetches the task with a
/// separate `GetTask` to build the reply. Between the two, any other task
/// finishing — or the tick — applies the bound, and with `keep_last: 0`, or
/// `keep_last: N` under N concurrent callers, it would drop the task before
/// that `GetTask` lands and answer its caller "not found" for a task that
/// COMPLETED. The read clears the mark (see [`Runtime::a2a_get_task`]), so in
/// the ordinary case a finished task goes the moment the bound says; the
/// grace only caps how long one nobody reads (a streamed or
/// `returnImmediately` send) outlives it. Far past any reactor round trip.
const SETTLE_GRACE_MS: u64 = 30_000;

/// The terminal tasks `policy` drops at `now`, by id.
///
/// Only a TERMINAL task is a candidate. A working task is someone's answer in
/// progress, and an input-required one is a question somebody is waiting to
/// answer; whatever the policy says, neither is dropped. The terminal ones are
/// ranked newest status first (the listing's order), so `keep_last` keeps a
/// prefix, and `ttl` counts from the status that finished the task.
///
/// A task still being handed back — settled within [`SETTLE_GRACE_MS`] and
/// not read since — is never returned: the reply its caller is waiting for is
/// still to be built from it. It goes on a later pass, once read or once the
/// grace is over.
fn tasks_to_evict<'a>(
    tasks: impl Iterator<Item = &'a Task>,
    policy: &crate::config::v2::TerminalRetention,
    now: u64,
) -> Vec<String> {
    let ttl_ms = policy.ttl.as_ref().map(|d| d.0.as_millis() as u64);
    let over_age = |t: &Task| ttl_ms.is_some_and(|ttl| now.saturating_sub(t.updated) > ttl);
    let settling = |t: &Task| t.unread && now.saturating_sub(t.updated) < SETTLE_GRACE_MS;
    let terminal = tasks.filter(|t| t.state.is_terminal());
    let Some(keep) = policy.keep_last else {
        // A ttl alone is a test of each task's own age: nothing to rank, so
        // nothing is sorted on the single-writer loop.
        return terminal
            .filter(|t| over_age(t) && !settling(t))
            .map(|t| t.id.clone())
            .collect();
    };
    let mut terminal: Vec<&Task> = terminal.collect();
    terminal.sort_by(|a, b| list_key(b).cmp(&list_key(a)));
    terminal
        .iter()
        .enumerate()
        .filter(|(i, t)| (*i >= keep as usize || over_age(t)) && !settling(t))
        .map(|(_, t)| t.id.clone())
        .collect()
}

/// The config a push `Get`/`Delete` names. The spec makes `id` REQUIRED, and it
/// is refused rather than defaulted: an empty id once matched *any* config on a
/// read and *every* config on a delete.
fn config_id(params: &Value) -> Result<&str, Value> {
    params
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("id is required"))
}

/// The push-config cursor: the id the previous page ended on.
fn push_cursor(id: &str) -> String {
    b64url(format!("push:{id}").as_bytes())
}

fn parse_push_cursor(token: &str) -> Option<String> {
    token_payload(token)?
        .strip_prefix("push:")
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// One page of a task's push configs. Ordered by id, and the token resumes
/// after the id the previous page ended on, so a config deleted between pages
/// cannot shift the next page onto one already seen. `pageSize` is the proto's
/// plain `int32`, so `0` is "unset" here, not a refusal.
fn push_page(targets: &[PushTarget], task_id: &str, params: &Value) -> Result<Value, Value> {
    let asked = params
        .get("pageSize")
        .and_then(Value::as_i64)
        .filter(|n| *n != 0);
    let page_size = page_size_of(asked)?;
    let after = match params.get("pageToken").and_then(Value::as_str) {
        None | Some("") => None,
        Some(tok) => Some(
            parse_push_cursor(tok)
                .ok_or_else(|| invalid("pageToken is not one this server issued"))?,
        ),
    };
    let mut sorted: Vec<&PushTarget> = targets
        .iter()
        .filter(|p| after.as_deref().is_none_or(|a| p.id.as_str() > a))
        .collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let more = sorted.len() > page_size;
    sorted.truncate(page_size);
    let next = match sorted.last() {
        Some(last) if more => push_cursor(&last.id),
        _ => String::new(),
    };
    Ok(json!({
        "configs": sorted
            .iter()
            .map(|p| crate::a2a::push::to_wire(task_id, p))
            .collect::<Vec<_>>(),
        "nextPageToken": next,
    }))
}

/// Whether a task projected for a request that activated `active` carries
/// its annotations: only when the request activated task-annotations/v1. An
/// extension a client did not ask for is not the client's to parse.
fn annotations_for(active: Active) -> Annotations {
    if active.contains(Ext::TaskAnnotations) {
        Annotations::Include
    } else {
        Annotations::Omit
    }
}

impl Runtime {
    /// How the request being served sees a task (see [`annotations_for`]).
    /// Between requests nothing is activated, so nothing is annotated.
    fn annotations(&self) -> Annotations {
        annotations_for(self.a2a_active)
    }

    /// `GetTask`: the task, to a caller who may see it.
    ///
    /// A read hands a settled task back, which is what retention was waiting
    /// for (see [`tasks_to_evict`]): from here the bound may take it.
    pub(super) fn a2a_get_task(&mut self, principal: &Principal, params: &Value) -> Value {
        let id = params.get("id").and_then(Value::as_str).unwrap_or("");
        let annotations = self.annotations();
        match self.tasks.get_mut(id) {
            Some(t) if t.is_visible_to(principal) => {
                t.unread = false;
                t.to_a2a(annotations)
            }
            // Don't disclose existence to a non-owner.
            _ => err_obj(TASK_NOT_FOUND, "task not found"),
        }
    }

    /// `ListTasks`: see [`list_tasks`].
    pub(super) fn a2a_list_tasks(&self, principal: &Principal, params: &Value) -> Value {
        list_tasks(self.tasks.values(), principal, params, self.annotations()).unwrap_or_else(|e| e)
    }

    // ---- push notifications (the `*TaskPushNotificationConfig` family) -----

    /// The task this request names, if the caller may touch it.
    ///
    /// "Not yours" and "does not exist" answer identically on purpose: a caller
    /// must not be able to probe for other principals' task ids.
    fn owned_task(&self, principal: &Principal, params: &Value) -> Result<String, Value> {
        let id = params.get("taskId").and_then(Value::as_str).unwrap_or("");
        match self.tasks.get(id) {
            Some(t) if t.is_visible_to(principal) => Ok(id.to_string()),
            _ => Err(err_obj(TASK_NOT_FOUND, "task not found")),
        }
    }

    pub(super) fn push_enabled(&self) -> Result<(), Value> {
        if self.settings.a2a.push.enabled {
            Ok(())
        } else {
            Err(err_obj(
                -32003,
                "push notifications are not enabled (set a2a.push.enabled: true)",
            ))
        }
    }

    /// Register (or replace) a webhook for a task. `params` is the spec's
    /// `TaskPushNotificationConfig` itself: `{taskId, id?, url, token?,
    /// authentication?}`.
    ///
    /// The target is checked here, while the caller is present to be told why —
    /// a refused URL or an authentication agentd could not send as given is a
    /// `-32602` with a reason, not a delivery that silently never happens or
    /// silently goes out without it.
    pub(super) fn a2a_push_set(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let target = match self.push_target(params) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let wire = crate::a2a::push::to_wire(&task_id, &target);
        self.attach_push(&task_id, target, principal);
        wire
    }

    /// The webhook a registration describes — `{id?, url, token?,
    /// authentication?}` — checked. Shared by the registration method and a
    /// send's inline config, which must refuse exactly the same targets.
    pub(super) fn push_target(&self, params: &Value) -> Result<PushTarget, Value> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(new_push_id);
        let target = crate::a2a::push::from_wire(params, id)
            .map_err(|e| err_obj(::mcp::rpc::INVALID_PARAMS, &e))?;
        let allow_private = self.settings.a2a.push.allow_private;
        if let Err(e) = crate::a2a::push::check_url(&target.url, allow_private) {
            return Err(err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!("push url refused: {e}"),
            ));
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
            return Err(err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!("push url refused: {e}"),
            ));
        }
        Ok(target)
    }

    /// Register `target` on the task, replacing a config of the same id.
    pub(super) fn attach_push(&mut self, task_id: &str, target: PushTarget, principal: &Principal) {
        let id = target.id.clone();
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.push.retain(|p| p.id != id);
            t.push.push(target);
            t.dirty = true;
        }
        self.task_persist(task_id);
        self.log.info(
            "a2a.push.registered",
            json!({"task": task_id, "config": id, "principal": principal.id}),
        );
    }

    /// Tell a task's webhooks the state it is in now, without a transition —
    /// for a config attached after the task had already moved.
    pub(super) fn push_now(&self, task_id: &str) {
        if let (Some(sink), Some(t)) = (&self.a2a_sink, self.tasks.get(task_id)) {
            sink.push(t, self.settings.a2a.push.allow_private);
        }
    }

    /// `{taskId, id}` → that config. Both are required; a config the task
    /// does not have is `-32001`, the same "not found" as a task.
    pub(super) fn a2a_push_get(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let want = match config_id(params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        match self
            .tasks
            .get(&task_id)
            .and_then(|t| t.push.iter().find(|p| p.id == want))
        {
            Some(p) => crate::a2a::push::to_wire(&task_id, p),
            None => err_obj(TASK_NOT_FOUND, "no such push notification config"),
        }
    }

    /// `{taskId, pageSize?, pageToken?}` → one page (see [`push_page`]).
    pub(super) fn a2a_push_list(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let targets = self
            .tasks
            .get(&task_id)
            .map(|t| t.push.as_slice())
            .unwrap_or_default();
        push_page(targets, &task_id, params).unwrap_or_else(|e| e)
    }

    /// `{taskId, id}` → `{}`. Deletes exactly the config named; one the task
    /// does not have is `-32001`, not a quiet success.
    pub(super) fn a2a_push_delete(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let want = match config_id(params) {
            Ok(id) => id.to_string(),
            Err(e) => return e,
        };
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let Some(t) = self.tasks.get_mut(&task_id) else {
            return err_obj(TASK_NOT_FOUND, "task not found");
        };
        let before = t.push.len();
        t.push.retain(|p| p.id != want);
        if t.push.len() == before {
            return err_obj(TASK_NOT_FOUND, "no such push notification config");
        }
        t.dirty = true;
        self.task_persist(&task_id);
        json!({})
    }

    pub(super) fn a2a_cancel_task(&mut self, principal: &Principal, params: &Value) -> Value {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let state = match self.tasks.get(&id) {
            Some(t) if t.is_visible_to(principal) => t.state,
            _ => return err_obj(TASK_NOT_FOUND, "task not found"),
        };
        // A settled task stays settled: the spec's answer is "not cancelable",
        // and the task is left exactly as it was — rewriting a COMPLETED task
        // to CANCELED would erase the result its caller is owed.
        if state.is_terminal() {
            return err_obj(
                TASK_NOT_CANCELABLE,
                &format!("task is already {}", state.wire()),
            );
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
        self.task_value(&id)
    }

    // ---- task lifecycle ----------------------------------------------------

    /// A task as a reply carries it, `Null` when there is no such task.
    fn task_value(&self, id: &str) -> Value {
        self.tasks
            .get(id)
            .map(|t| t.to_a2a(self.annotations()))
            .unwrap_or(Value::Null)
    }

    /// The `{task}` answer to a send.
    pub(crate) fn task_reply(&self, id: &str) -> Value {
        json!({"task": self.task_value(id)})
    }

    /// Create + persist a fresh task; publish it to the shared view.
    /// Create a task, taking the listener's reserved id when the request that
    /// is being served brought one (see [`Runtime::reserved_task_id`]).
    ///
    /// `inbound` is the caller's message that opened it, when one did: it is
    /// the first entry of the task's history, and when it carries a command
    /// the task records which op it runs. A gate's task passes `None` — its
    /// first message is the agent's question.
    pub(crate) fn task_create(
        &mut self,
        ctx: &str,
        principal: &Principal,
        link: Link,
        inbound: Option<&Value>,
    ) -> String {
        let id = match self.reserved_task_id.take() {
            Some(id) => id,
            None => new_task_id(),
        };
        let mut task = Task::new(&id, ctx, Some(&principal.id), link);
        // `ctx` is the conversation's key; the wire says it the way the task's
        // own caller addressed it — the owner's name, or the key an operator
        // joined by.
        task.set_conversation(ctx, &self.conversation_wire(&principal.id, ctx));
        if let Some(m) = inbound {
            task.command = super::send::command_op(m);
            task.record_inbound(m);
        }
        self.tasks.insert(id.clone(), task);
        self.task_persist(&id);
        self.task_sync(&id);
        id
    }

    /// A command that finishes at once: create its task already completed,
    /// opened by the command message `inbound`.
    pub(super) fn task_complete_now(
        &mut self,
        ctx: &str,
        principal: &Principal,
        link: Link,
        inbound: &Value,
        text: Option<String>,
        result: Option<Value>,
    ) -> Value {
        let id = self.task_create(ctx, principal, link, Some(inbound));
        if let Some(t) = self.tasks.get_mut(&id) {
            if let Some(r) = result {
                t.set_result(r);
            }
            t.transition(State::Completed, text);
        }
        self.task_persist(&id);
        self.task_sync(&id);
        self.task_reply(&id)
    }

    /// Publish a task transition, then apply `store.retention.tasks`.
    ///
    /// A task settling is the only moment the terminal set grows, so that is
    /// where the bound is kept; any other transition leaves it to the sweep,
    /// and costs no ranking. The settled task is marked unread, which spares
    /// it until its caller has its answer (see [`tasks_to_evict`]).
    pub(crate) fn task_sync(&mut self, id: &str) {
        let settled = match self.tasks.get_mut(id) {
            Some(t) if t.state.is_terminal() => {
                t.unread = true;
                true
            }
            _ => false,
        };
        self.task_publish(id);
        if settled {
            self.evict_terminal_tasks();
        }
    }

    /// The tick's share of `store.retention.tasks`: at most once per
    /// [`TASK_SWEEP_EVERY`], drop what a `ttl` has aged out or a reloaded
    /// bound no longer keeps. Nothing transitions for either, so without this a
    /// finished task on a quiet listener would outlive its `ttl` indefinitely.
    pub(crate) fn sweep_terminal_tasks(&mut self) {
        if self.tasks_swept.elapsed() < TASK_SWEEP_EVERY {
            return;
        }
        self.tasks_swept = std::time::Instant::now();
        self.evict_terminal_tasks();
    }

    /// Drop the terminal tasks `store.retention.tasks` no longer keeps (see
    /// [`tasks_to_evict`]).
    ///
    /// Each goes everywhere at once: from the task map, from the inbox-event
    /// links that would otherwise name it, from the stream fan-out — whose
    /// replay ring holds its every status and its result, and would otherwise
    /// outlive it for the daemon's whole life — from the store, so a restart
    /// does not bring it back, and from every display client, which is told
    /// with `task.removed` by the same visibility its `task` events had, so
    /// the owner's view drops it too.
    pub(crate) fn evict_terminal_tasks(&mut self) {
        let drop = tasks_to_evict(
            self.tasks.values(),
            &self.settings.store.retention.tasks,
            crate::state::now_ms(),
        );
        for id in drop {
            let Some(task) = self.tasks.remove(&id) else {
                continue;
            };
            self.event_to_task.retain(|_, t| *t != id);
            if let Some(sink) = &self.a2a_sink {
                sink.forget(&id);
            }
            match self.durable.delete(crate::state::Kind::Task, &id) {
                Ok(()) => self.log.info("a2a.task.evicted", json!({"task": id})),
                // Gone from memory regardless: the next life restores it and
                // the first sweep drops it again, which is the most a store
                // that refused the delete allows.
                Err(e) => self.log.warn(
                    "a2a.task.evict.fail",
                    json!({"task": id, "err": e.to_string()}),
                ),
            }
            self.feed_push(
                "task.removed",
                FeedVis::Owner(task.principal.clone()),
                json!({"id": id}),
            );
        }
    }

    /// Publish a task transition: to A2A subscribers, and onto the interface
    /// feed so every attached display client converges without polling.
    ///
    /// The A2A half is also what *settles a blocking send* — the protocol layer
    /// waits on this stream rather than polling a snapshot — so a task that
    /// never publishes is a task that never finishes as far as a caller can
    /// tell.
    fn task_publish(&self, id: &str) {
        let Some(sink) = &self.a2a_sink else {
            return;
        };
        let allow_private = self.settings.a2a.push.allow_private;
        // Retention is the only way a task leaves the map, and it has already
        // told the owner's display clients. A turn or run that ends after
        // that has nothing left to publish — and a second `task.removed`
        // would reach a different audience than the first.
        let Some(t) = self.tasks.get(id) else {
            return;
        };
        // Send the artifact BEFORE the terminal status frame. A conformant
        // streaming client stops reading as soon as it sees a terminal state,
        // so a result frame sent after it is a result nobody reads.
        if t.state.is_terminal()
            && let Some(a) = crate::a2a::wire::result_artifact(t)
        {
            sink.artifact(&t.id, &t.context_id, a.clone());
        }
        sink.status(t);
        // A caller that asked to be told rather than to watch. Fired from
        // here because this is the one place every transition passes
        // through, whatever caused it.
        if !t.push.is_empty() {
            sink.push(t, allow_private);
        }
        self.feed_task(t);
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
    fn task_ids_are_random_so_two_lives_cannot_mint_the_same_one() {
        // `seq` restarts at 0 with the process, and the ids of a previous life
        // are still in the store; a gate's id is a2a-rs's UUIDv4, the shape
        // of every task id a send creates.
        let a = new_task_id();
        let b = new_task_id();
        assert_ne!(a, b);
        let v4 = |id: &str| {
            id.len() == 36
                && id.as_bytes()[14] == b'4'
                && id
                    .chars()
                    .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        };
        assert!(v4(&a) && v4(&b), "{a} / {b}");
    }

    fn who(id: &str, role: crate::config::v2::Role) -> Principal {
        Principal {
            id: id.into(),
            role,
            ..Principal::anonymous()
        }
    }

    /// A task owned by `owner` in `ctx`, last moved at `updated` ms.
    fn task(id: &str, ctx: &str, owner: &str, state: State, updated: u64) -> Task {
        let mut t = Task::new(id, ctx, Some(owner), Link::Turn { ctx: ctx.into() });
        t.set_result(json!(format!("result of {id}")));
        t.transition(state, None);
        t.updated = updated;
        t
    }

    fn ids(page: &Value) -> Vec<String> {
        page["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// Every `ListTasks` field is honoured — the filters, the order, the page,
    /// the projection — and one that cannot be is refused, never ignored.
    #[test]
    fn list_tasks_semantics() {
        use crate::config::v2::Role;
        let tasks = [
            task("t-a", "c1", "user:a", State::Completed, 1_000),
            task("t-b", "c2", "user:a", State::Working, 3_000),
            task("t-c", "c2", "user:a", State::Completed, 2_000),
            task("t-d", "c2", "user:a", State::Completed, 2_000),
            task("t-z", "c9", "user:z", State::Completed, 9_000),
        ];
        let op = who("operator", Role::Operator);
        let alice = who("user:a", Role::User);
        let list = |p: &Principal, params: Value| {
            list_tasks(tasks.iter(), p, &params, Annotations::Include)
        };

        // Newest status first; a tie broken by id, descending. Somebody else's
        // task is not merely hidden — it is not counted either.
        let all = list(&alice, json!({})).unwrap();
        assert_eq!(ids(&all), ["t-b", "t-d", "t-c", "t-a"]);
        assert_eq!(all["totalSize"], 4);
        assert_eq!(all["pageSize"], DEFAULT_PAGE_SIZE);
        assert_eq!(all["nextPageToken"], "", "the last page says so: {all}");
        assert_eq!(ids(&list(&op, json!({})).unwrap())[0], "t-z");

        // The filters, each one alone and together.
        let c2 = list(&alice, json!({"contextId": "c2"})).unwrap();
        assert_eq!(ids(&c2), ["t-b", "t-d", "t-c"]);
        let done = json!({"contextId": "c2", "status": "TASK_STATE_COMPLETED"});
        assert_eq!(ids(&list(&alice, done).unwrap()), ["t-d", "t-c"]);
        let since = crate::a2a::wire::timestamp_string(2_000);
        let recent = list(&alice, json!({"statusTimestampAfter": since})).unwrap();
        assert_eq!(
            ids(&recent),
            ["t-b", "t-d", "t-c"],
            "the bound is inclusive"
        );
        // A sub-millisecond bound past a task's instant excludes it.
        let past = json!({"statusTimestampAfter": "1970-01-01T00:00:02.000500Z"});
        assert_eq!(ids(&list(&alice, past).unwrap()), ["t-b"]);

        // Paging walks the same order to the end, each task exactly once.
        let mut seen = Vec::new();
        let mut token = String::new();
        loop {
            assert!(seen.len() < 4, "the walk must end: {seen:?}");
            let page = list(&alice, json!({"pageSize": 1, "pageToken": token})).unwrap();
            assert_eq!(page["totalSize"], 4);
            seen.extend(ids(&page));
            token = page["nextPageToken"].as_str().unwrap().to_string();
            if token.is_empty() {
                break;
            }
        }
        assert_eq!(seen, ["t-b", "t-d", "t-c", "t-a"]);

        // The projection: artifacts only when asked for.
        let bare = list(&alice, json!({"contextId": "c1"})).unwrap();
        assert!(bare["tasks"][0].get("artifacts").is_none(), "{bare}");
        let full = list(&alice, json!({"contextId": "c1", "includeArtifacts": true})).unwrap();
        assert_eq!(
            full["tasks"][0]["artifacts"][0]["parts"][0]["text"],
            "result of t-a"
        );

        // What cannot be honoured is refused with a reason.
        for bad in [
            json!({"pageSize": 0}),
            json!({"pageSize": 101}),
            json!({"pageSize": -1}),
            json!({"historyLength": -1}),
            json!({"pageToken": "!!not-a-token"}),
            json!({"pageToken": b64url(b"no-colon-here")}),
            json!({"statusTimestampAfter": "yesterday"}),
            json!({"status": "TASK_STATE_SLEEPING"}),
        ] {
            let e = list(&alice, bad.clone()).expect_err(&bad.to_string());
            assert_eq!(e["_error"]["code"], INVALID_PARAMS, "{bad}: {e}");
        }
        assert!(list(&alice, json!({"pageSize": 100, "historyLength": 0})).is_ok());
    }

    /// Two owners who both call their conversation `chat1`: the operator
    /// finds both by that name and either by its key; an owner finds its own
    /// by its name and nothing by a key the runtime chose.
    #[test]
    fn the_context_filter_reads_keys_for_operators_and_names_for_owners() {
        use crate::config::v2::Role;
        let mut a = task("t-a", "chat1", "user:a", State::Completed, 1_000);
        a.set_conversation("ctx-aaaa", "chat1");
        let mut b = task("t-b", "chat1", "user:b", State::Completed, 2_000);
        b.set_conversation("ctx-bbbb", "chat1");
        let tasks = [a, b];
        let list = |p: &Principal, ctx: &str| {
            ids(&list_tasks(
                tasks.iter(),
                p,
                &json!({"contextId": ctx}),
                Annotations::Omit,
            )
            .unwrap())
        };
        let op = who("operator", Role::Operator);
        let alice = who("user:a", Role::User);
        assert_eq!(list(&op, "chat1"), ["t-b", "t-a"]);
        assert_eq!(list(&op, "ctx-aaaa"), ["t-a"]);
        assert_eq!(list(&alice, "chat1"), ["t-a"]);
        assert!(list(&alice, "ctx-aaaa").is_empty());
    }

    fn retention(
        keep_last: Option<u32>,
        ttl_ms: Option<u64>,
    ) -> crate::config::v2::TerminalRetention {
        crate::config::v2::TerminalRetention {
            keep_last,
            ttl: ttl_ms.map(|ms| crate::config::v2::Dur(std::time::Duration::from_millis(ms))),
        }
    }

    /// Retention takes terminal tasks only, newest kept first, and never one
    /// still being handed back to its caller.
    #[test]
    fn retention_drops_only_terminal_tasks_past_the_bound() {
        let tasks = [
            task("t-old", "c", "u", State::Completed, 1_000),
            task("t-mid", "c", "u", State::Failed, 2_000),
            task("t-new", "c", "u", State::Canceled, 3_000),
            // Older than anything, and never a candidate.
            task("t-work", "c", "u", State::Working, 10),
            task("t-ask", "c", "u", State::InputRequired, 20),
        ];
        // In id order: a ttl alone ranks nothing, so it answers in map order.
        let evict = |tasks: &[Task], policy, now| {
            let mut ids = tasks_to_evict(tasks.iter(), &policy, now);
            ids.sort();
            ids
        };

        assert!(
            evict(&tasks, retention(None, None), 99_000).is_empty(),
            "unset keeps"
        );
        assert_eq!(
            evict(&tasks, retention(Some(1), None), 3_000),
            ["t-mid", "t-old"]
        );
        assert_eq!(
            evict(&tasks, retention(Some(0), None), 3_000),
            ["t-mid", "t-new", "t-old"]
        );
        // The age is strictly past the ttl, counted from the finishing status.
        assert_eq!(
            evict(&tasks, retention(None, Some(1_500)), 3_500),
            ["t-old"]
        );
        assert_eq!(
            evict(&tasks, retention(None, Some(0)), 99_000),
            ["t-mid", "t-new", "t-old"]
        );
        // Either bound drops.
        assert_eq!(
            evict(&tasks, retention(Some(2), Some(1_500)), 3_500),
            ["t-old"]
        );
    }

    /// A settled task its caller has not read back is spared by every bound
    /// — `keep_last: 0` and `ttl: 0s` included — until it is read or the
    /// grace is over. A blocking send is answered by a `GetTask` a2a-rs makes
    /// after the terminal event, and a task dropped in between answers the
    /// caller whose task COMPLETED with "not found".
    #[test]
    fn retention_spares_a_task_until_its_caller_has_read_it() {
        let mut tasks = [
            task("t-read", "c", "u", State::Completed, 1_000),
            task("t-unread", "c", "u", State::Completed, 2_000),
        ];
        tasks[1].unread = true;
        let evict = |tasks: &[Task], policy, now| tasks_to_evict(tasks.iter(), &policy, now);
        for policy in [
            || retention(Some(0), None),
            || retention(None, Some(0)),
            || retention(Some(0), Some(0)),
        ] {
            assert_eq!(evict(&tasks, policy(), 2_001), ["t-read"]);
            assert_eq!(
                evict(&tasks, policy(), 2_000 + SETTLE_GRACE_MS - 1),
                ["t-read"]
            );
            // Nobody came for it: the grace does not outlast itself.
            let mut late = evict(&tasks, policy(), 2_000 + SETTLE_GRACE_MS);
            late.sort();
            assert_eq!(late, ["t-read", "t-unread"]);
        }
        // Read back, it goes the moment the bound says.
        tasks[1].unread = false;
        let mut read = evict(&tasks, retention(Some(0), None), 2_001);
        read.sort();
        assert_eq!(read, ["t-read", "t-unread"]);
        // A spared task still holds its place in the ranking, so it pushes an
        // older one out rather than letting it stay.
        tasks[1].unread = true;
        assert_eq!(evict(&tasks, retention(Some(1), None), 2_001), ["t-read"]);
    }

    /// An evicted task takes its stream state with it. What `forget` drops
    /// is pinned in `ports`; that eviction asks for it cannot be shown from a
    /// request — no read of an evicted task gets past "not found" to its
    /// replay ring — and a runtime cannot be built in a unit test, so this
    /// pins the call where every eviction passes.
    #[test]
    fn an_evicted_task_is_forgotten_by_the_stream_side() {
        let src = include_str!("tasks.rs");
        let body = src
            .split("pub(crate) fn evict_terminal_tasks(&mut self)")
            .nth(1)
            .and_then(|rest| rest.split("\n    }\n").next())
            .expect("evict_terminal_tasks");
        assert!(
            body.contains("sink.forget(&id)"),
            "eviction must drop the task's replay ring and channel"
        );
        // …and eviction is the one place a removal is announced: a turn or
        // run ending after it must not tell display clients a second time,
        // to a different audience.
        let code = src.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            code.matches("\"task.removed\"").count(),
            1,
            "task.removed is pushed by eviction alone"
        );
        assert!(body.contains("\"task.removed\""));
    }

    fn target(id: &str) -> PushTarget {
        PushTarget {
            id: id.into(),
            url: "https://hooks.example/x".into(),
            token: String::new(),
            auth: None,
        }
    }

    /// Pages walk to the end with nothing twice; the last page's token is
    /// empty, not missing; a token this server never issued is refused.
    #[test]
    fn push_config_listing_pages() {
        let targets: Vec<PushTarget> = ["p-e", "p-a", "p-c", "p-b", "p-d"]
            .into_iter()
            .map(target)
            .collect();
        let mut seen = Vec::new();
        let mut token = String::new();
        let mut pages = 0;
        loop {
            assert!(pages < 3, "the walk must end: {seen:?}");
            let page = push_page(
                &targets,
                "task-1",
                &json!({"taskId": "task-1", "pageSize": 2, "pageToken": token}),
            )
            .unwrap();
            pages += 1;
            let configs = page["configs"].as_array().unwrap();
            assert!(configs.len() <= 2, "{page}");
            seen.extend(
                configs
                    .iter()
                    .map(|c| c["id"].as_str().unwrap().to_string()),
            );
            token = page["nextPageToken"]
                .as_str()
                .expect("nextPageToken is always present")
                .to_string();
            if token.is_empty() {
                break;
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(seen, ["p-a", "p-b", "p-c", "p-d", "p-e"]);

        // The default page holds them all, and still says it is the last.
        let all = push_page(&targets, "task-1", &json!({"taskId": "task-1"})).unwrap();
        assert_eq!(all["configs"].as_array().unwrap().len(), 5);
        assert_eq!(all["nextPageToken"], "");

        for bad in [
            json!({"pageToken": "garbage!"}),
            // A ListTasks cursor is not a push-config cursor.
            json!({"pageToken": b64url(b"2000:t-a")}),
            json!({"pageSize": 101}),
        ] {
            let e = push_page(&targets, "task-1", &bad).expect_err(&bad.to_string());
            assert_eq!(e["_error"]["code"], INVALID_PARAMS, "{bad}: {e}");
        }
    }

    /// Get and Delete name one config, always: an empty id once read as "any"
    /// on a get and "all" on a delete.
    #[test]
    fn a_push_config_id_is_required() {
        for p in [json!({"taskId": "t"}), json!({"taskId": "t", "id": ""})] {
            let e = config_id(&p).unwrap_err();
            assert_eq!(e["_error"]["code"], INVALID_PARAMS);
            assert_eq!(e["_error"]["message"], "id is required");
        }
        assert_eq!(config_id(&json!({"taskId": "t", "id": "p-1"})), Ok("p-1"));
    }
}
