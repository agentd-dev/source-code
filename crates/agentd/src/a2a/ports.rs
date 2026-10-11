// SPDX-License-Identifier: AGPL-3.0-only
//! **agentd behind the A2A specification's request handler.**
//!
//! The A2A project's official server ([`a2a_server`]) models an A2A server as
//! one transport-agnostic trait, [`RequestHandler`] — a method per operation of
//! the specification — and owns everything around it: the JSON-RPC envelope,
//! the canonical ProtoJSON of every request and response, SSE framing, the
//! error codes. This module is agentd's implementation of that trait: what a
//! send does, what a task is and who may see it, and the streams a caller
//! watches.
//!
//! The one structural fact to keep in mind is that agentd's runtime is a single
//! blocking reactor, and the trait is `async`. Every method therefore hands its
//! work to [`A2aBridge`] — post an [`Event::A2a`] to the loop, wait for the
//! reply — on a blocking thread, and awaits that. The reactor stays
//! single-threaded and knows nothing about tokio; the protocol layer stays async
//! and knows nothing about the reactor.
//!
//! Streams are agentd's too ([`Fanout`]): the reactor publishes every task
//! transition into a per-task channel, and a stream opens with the task as it
//! is (the specification's MUST for both streaming methods) followed by the
//! transitions after it, closing at a terminal state. The reactor stamps each
//! transition with the channel's sequence and every snapshot with the sequence
//! it was read at, so a stream never replays a transition its snapshot already
//! shows — order is the reactor's, which is the only writer.
//!
//! [`Event::A2a`]: crate::runtime::events::Event::A2a

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a_server::{RequestHandler, ServiceParams};
use a2a_types::{
    A2AError, AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResponse, StreamResponse,
    SubscribeToTaskRequest, Task as WireTask, TaskPushNotificationConfig, TaskState, TypedDetail,
};
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::a2a::Principal;
use crate::a2a::principals::Via;
use crate::a2a::wire::{from_wire, message_from_wire, message_to_wire, to_wire};
use crate::runtime::a2a_server::A2aBridge;
use crate::runtime::surface::Active;

/// How long a blocking send waits for its task to settle before answering with
/// the task as it stands. The specification's "MUST wait" has no escape
/// clause, and an agent that never finishes would otherwise hold the
/// connection for as long as the client tolerates; the task it gets back is
/// true — `WORKING` says the agent has not finished — and carries the id to
/// follow it by.
const SEND_WAIT: Duration = Duration::from_secs(25);

/// The transitions a slow stream may fall behind by before it is handed the
/// task as it now is instead of the ones it missed.
const STREAM_BUFFER: usize = 256;

/// agentd's answers to the specification's operations, in one value.
pub struct RuntimePorts {
    bridge: Arc<A2aBridge>,
    /// Where the reactor publishes transitions, and every stream reads them.
    streams: Arc<Fanout>,
}

impl RuntimePorts {
    pub fn new(bridge: Arc<A2aBridge>, streams: Arc<Fanout>) -> RuntimePorts {
        RuntimePorts { bridge, streams }
    }

    /// Run one reactor round trip without blocking the async runtime.
    ///
    /// The reply is either a result value or agentd's JSON-RPC error object;
    /// the latter is turned back into the spec's error type so the protocol
    /// layer maps it to the right code, rather than being passed off as a
    /// successful result that happens to contain an error — and it is
    /// recorded on the request's [`RequestScope`], because the SDK renders an
    /// error in its own words (see [`record_error`]).
    ///
    /// An error is recognised under either spelling — the reactor's `_error`
    /// marker, or a plain JSON-RPC `error` member. Fail closed: no reply the
    /// runtime answers carries an `error` member as data, so one that does is a
    /// refusal, and reading it as a result would hand a caller whatever else
    /// happened to be in the object.
    async fn call(&self, method: &str, params: Value, who: &Principal) -> Result<Value, A2AError> {
        let active = serving().map_or(Active::NONE, |s| s.active);
        let v = round_trip(&self.bridge, method, params, who, active).await?;
        match error_of(&v) {
            Some(e) => {
                record_error(e);
                Err(from_error_object(e))
            }
            None => Ok(v),
        }
    }

    /// The task `id`, read as `who` may see it, with the stream position it was
    /// read at. A caller who may not see it is told it does not exist.
    async fn read_task(&self, id: &str, who: &Principal) -> Result<(WireTask, u64), A2AError> {
        let v = self
            .call("GetTask", json!({"id": id, "_attach": true}), who)
            .await?;
        attached(v, id)
    }

    /// Start or continue the work `req` asks for, as `who`: the task it made
    /// or continued (or the message it answered with), and the stream
    /// position the answer was read at.
    async fn send(
        &self,
        req: &SendMessageRequest,
        task_id: &str,
        new: bool,
        who: &Principal,
    ) -> Result<(SendMessageResponse, u64), A2AError> {
        let mut params = json!({
            "message": message_to_wire(&req.message),
            "taskId": task_id,
            "newTask": new,
            "_attach": true,
        });
        // A push config the send carried inline crosses with the message: the
        // runtime checks it and attaches it to the task the message creates or
        // continues, so a refused send leaves no webhook behind.
        if let Some(push) = req
            .configuration
            .as_ref()
            .and_then(|c| c.task_push_notification_config.as_ref())
        {
            params["push"] = to_wire(push);
        }
        let mut v = self.call("SendMessage", params, who).await?;
        let seq = take_seq(&mut v);
        if let Some(m) = v.get("message") {
            let m = message_from_wire(m).map_err(A2AError::internal)?;
            return Ok((SendMessageResponse::Message(m), seq));
        }
        // The task that answers must be the task the stream was opened on: a
        // send answered with any other would settle — or stream — someone
        // else's.
        let task = same_task(task_id, task_from(v)?)?;
        Ok((SendMessageResponse::Task(task), seq))
    }
}

/// The task a send names — continue it — or `None` for a new one. A blank id
/// names nothing.
fn named_task(req: &SendMessageRequest) -> Option<String> {
    req.message
        .task_id
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
}

/// Whether a blocking send stops waiting at `state`: a terminal state, or one
/// that waits on the caller (A2A 1.0 §3.1.1).
fn settles(state: &TaskState) -> bool {
    state.is_terminal() || matches!(state, TaskState::InputRequired | TaskState::AuthRequired)
}

#[async_trait::async_trait]
impl RequestHandler for RuntimePorts {
    /// A message becomes runtime work: a conversation turn, or — when it carries
    /// agentd's command DataPart — a registry action. Which one it is, and the
    /// durable task that results, is the reactor's decision; this carries the
    /// message across with what the caller meant by it — continue the task it
    /// names, or start one under an id minted here — and, unless the caller
    /// asked not to wait, waits for that task to settle.
    async fn send_message(
        &self,
        _params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let who = caller();
        let named = named_task(&req);
        let task_id = named.clone().unwrap_or_else(a2a_types::new_task_id);
        let config = req.configuration.as_ref();
        let immediately = config.and_then(|c| c.return_immediately).unwrap_or(false);
        let history = config.and_then(|c| c.history_length);
        // Watching starts before the work does: a task that settles while the
        // message is still being processed would otherwise be waited on for a
        // transition that has already happened.
        let mut updates = self.streams.subscribe(&task_id);
        let (answer, seq) = self.send(&req, &task_id, named.is_none(), &who).await?;
        let SendMessageResponse::Task(task) = answer else {
            return Ok(answer);
        };
        if immediately || settles(&task.status.state) {
            return Ok(SendMessageResponse::Task(with_history(task, history)));
        }
        let _ = tokio::time::timeout(SEND_WAIT, async {
            loop {
                match updates.recv().await {
                    Ok(p) if p.seq <= seq => {}
                    Ok(p) if p.settles => return,
                    Ok(_) => {}
                    // Behind or gone: what the task now is is read below.
                    Err(_) => return,
                }
            }
        })
        .await;
        // Read back rather than assembled from the event: an event carries a
        // status, not the artifacts and history the caller is owed.
        let (task, _) = self.read_task(&task_id, &who).await?;
        Ok(SendMessageResponse::Task(with_history(task, history)))
    }

    /// The same work, watched: the task as the send left it, then each
    /// transition after it, until a terminal state closes the stream (A2A 1.0
    /// §3.1.2). A send answered with a message is a stream of that one message.
    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let who = caller();
        let named = named_task(&req);
        let task_id = named.clone().unwrap_or_else(a2a_types::new_task_id);
        let history = req.configuration.as_ref().and_then(|c| c.history_length);
        let updates = self.streams.subscribe(&task_id);
        let (answer, seq) = self.send(&req, &task_id, named.is_none(), &who).await?;
        Ok(match answer {
            SendMessageResponse::Message(m) => {
                Box::pin(stream::once(async move { Ok(StreamResponse::Message(m)) }))
            }
            SendMessageResponse::Task(task) => {
                self.watch(with_history(task, history), seq, updates, who)
            }
        })
    }

    async fn get_task(
        &self,
        _params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<WireTask, A2AError> {
        let who = caller();
        let v = self.call("GetTask", json!({"id": req.id}), &who).await?;
        let task = same_task(&req.id, task_from(v)?)?;
        Ok(with_history(task, req.history_length))
    }

    /// One page of the tasks the caller may see. The request crosses whole —
    /// every filter, the page and the projection are the reactor's to apply,
    /// because only the reactor knows who owns what, and a parameter dropped
    /// here would be a filter silently not applied.
    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let who = caller();
        let v = self.call("ListTasks", to_wire(&req), &who).await?;
        from_wire(v).map_err(A2AError::internal)
    }

    async fn cancel_task(
        &self,
        _params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<WireTask, A2AError> {
        let who = caller();
        let v = self.call("CancelTask", json!({"id": req.id}), &who).await?;
        same_task(&req.id, task_from(v)?)
    }

    /// The task as it is, then its transitions until it ends (A2A 1.0
    /// §3.1.6). Read first, as the caller: a task the caller may not see is
    /// "not found" like any read — a non-owner must not learn from the
    /// difference that it exists — and one that has already ended is
    /// `UnsupportedOperation`, as plain JSON, never a stream that opens and
    /// closes with nothing in it.
    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let who = caller();
        let updates = self.streams.subscribe(&req.id);
        let (task, seq) = self.read_task(&req.id, &who).await?;
        if task.status.state.is_terminal() {
            return Err(A2AError::unsupported_operation(format!(
                "task {} has already ended; read it with GetTask",
                req.id
            )));
        }
        Ok(self.watch(task, seq, updates, who))
    }

    /// Push notifications: a caller registers a webhook and is told about its
    /// task instead of watching it. The register/read/list/delete half is
    /// here; the *delivery* half is [`StreamSink::push`], fired from the
    /// reactor where every transition passes. Both are refused unless
    /// `a2a.push.enabled` — the URL comes from a peer, so making the request
    /// at all is the operator's decision (see [`crate::a2a::push`]).
    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let who = caller();
        let v = self.call("PushConfigSet", to_wire(&req), &who).await?;
        from_wire(v).map_err(A2AError::internal)
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let who = caller();
        let v = self
            .call(
                "PushConfigGet",
                json!({"taskId": req.task_id, "id": req.id}),
                &who,
            )
            .await?;
        from_wire(v).map_err(A2AError::internal)
    }

    /// One page of a task's configs. The request crosses whole, paging
    /// included, and the runtime refuses what it cannot honour.
    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let who = caller();
        let v = self.call("PushConfigList", to_wire(&req), &who).await?;
        from_wire(v).map_err(A2AError::internal)
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let who = caller();
        self.call(
            "PushConfigDelete",
            json!({"taskId": req.task_id, "id": req.id}),
            &who,
        )
        .await?;
        Ok(())
    }

    /// The authenticated card, scoped to whoever is asking.
    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let who = caller();
        let v = self.call("GetExtendedAgentCard", json!({}), &who).await?;
        from_wire(v).map_err(A2AError::internal)
    }
}

impl RuntimePorts {
    /// A stream over `task`: the task first, then every transition published
    /// after position `seq`, until a terminal state ends it.
    ///
    /// A stream that falls more than [`STREAM_BUFFER`] transitions behind is
    /// handed the task as it now is instead of the transitions it missed —
    /// read as its caller, so the reader learns no more than a `GetTask`
    /// would tell it. A task retention drops ends its streams.
    fn watch(
        &self,
        task: WireTask,
        seq: u64,
        updates: broadcast::Receiver<Published>,
        who: Principal,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let state = Watch {
            id: task.id.clone(),
            first: Some(task),
            seq,
            updates,
            bridge: Arc::clone(&self.bridge),
            who,
            done: false,
        };
        Box::pin(stream::unfold(state, |mut w| async move {
            if w.done {
                return None;
            }
            if let Some(task) = w.first.take() {
                w.done = task.status.state.is_terminal();
                return Some((Ok(StreamResponse::Task(task)), w));
            }
            loop {
                match w.updates.recv().await {
                    Ok(p) if p.seq <= w.seq => {}
                    Ok(p) => {
                        w.seq = p.seq;
                        w.done = p.terminal;
                        return Some((Ok(p.response), w));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let read = round_trip(
                            &w.bridge,
                            "GetTask",
                            json!({"id": w.id, "_attach": true}),
                            &w.who,
                            Active::NONE,
                        )
                        .await
                        .and_then(|v| match error_of(&v) {
                            Some(e) => Err(from_error_object(e)),
                            None => attached(v, &w.id),
                        });
                        return match read {
                            Ok((task, seq)) => {
                                w.seq = seq;
                                w.done = task.status.state.is_terminal();
                                Some((Ok(StreamResponse::Task(task)), w))
                            }
                            Err(e) => {
                                w.done = true;
                                Some((Err(e), w))
                            }
                        };
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }))
    }
}

/// One stream's position.
struct Watch {
    id: String,
    /// The task the stream opens with, until it is sent.
    first: Option<WireTask>,
    /// The last position the reader has seen, by snapshot or by transition.
    seq: u64,
    updates: broadcast::Receiver<Published>,
    bridge: Arc<A2aBridge>,
    who: Principal,
    done: bool,
}

/// One reactor round trip, on a blocking thread.
async fn round_trip(
    bridge: &Arc<A2aBridge>,
    method: &str,
    params: Value,
    who: &Principal,
    active: Active,
) -> Result<Value, A2AError> {
    let bridge = Arc::clone(bridge);
    let method = method.to_string();
    let who = who.clone();
    tokio::task::spawn_blocking(move || bridge.call(&method, params, who, active))
        .await
        .map_err(|e| A2AError::internal(format!("the runtime call did not complete: {e}")))
}

/// The stream position a reply was read at, taken out of it (`0` when it
/// carries none).
fn take_seq(v: &mut Value) -> u64 {
    v.as_object_mut()
        .and_then(|o| o.remove("_seq"))
        .and_then(|s| s.as_u64())
        .unwrap_or(0)
}

/// A task read with its stream position, which must be the task asked for.
fn attached(mut v: Value, id: &str) -> Result<(WireTask, u64), A2AError> {
    let seq = take_seq(&mut v);
    Ok((same_task(id, task_from(v)?)?, seq))
}

/// `historyLength`: the newest `n` of the task's history kept.
fn with_history(mut task: WireTask, history_length: Option<i32>) -> WireTask {
    if let Some(n) = history_length {
        crate::a2a::wire::limit_history(&mut task, u32::try_from(n).unwrap_or(0));
    }
    task
}

/// The request being served, as the handler sees it.
///
/// The specification's handler takes no caller — it was drawn for a server
/// whose store is not per-principal. agentd's is: a task belongs to whoever
/// started it, and a non-operator may only see its own. So the request
/// travels out-of-band, scoped to its tokio task rather than passed down
/// through the method signatures — set by agentd's own pipeline after it
/// authenticated the caller, and readable by nothing a client sends.
#[derive(Clone)]
pub struct RequestScope {
    /// Who is calling.
    pub caller: Principal,
    /// Which evidence named them.
    pub via: Via,
    /// The first error object the runtime answered this request with, kept
    /// whole. The handler returns a typed [`A2AError`], which the SDK renders
    /// in its own words — its `ErrorInfo` under the protocol's domain — so a
    /// refusal agentd's runtime made (a draining agent, an op nobody may run)
    /// would reach the caller with the runtime's reason gone. The listener
    /// reads this back and puts the runtime's object on the wire instead
    /// (`serve::dispatch`'s fidelity filter), so a refusal is the same code,
    /// message and data whichever path answered it.
    pub error: Arc<Mutex<Option<Value>>>,
    /// The extensions the request activated (pipeline step 9). Every runtime
    /// call carries it, which projects a task's annotations only while
    /// task-annotations is in it.
    pub active: Active,
}

impl RequestScope {
    /// A scope for `caller`, named by `via`, that activated `active`, with
    /// nothing recorded yet.
    pub fn new(caller: Principal, via: Via, active: Active) -> RequestScope {
        RequestScope {
            caller,
            via,
            error: Arc::default(),
            active,
        }
    }
}

tokio::task_local! {
    /// The request currently being served.
    ///
    /// Set once by the transport ([`crate::a2a::serve`]) around the whole
    /// dispatch. Unset means nobody is being served, which reads as anonymous —
    /// the role the authorization matrix refuses everything.
    static SCOPE: RequestScope;
}

/// Run `f` as one request's handling, in `scope`.
pub async fn with_request<F, T>(scope: RequestScope, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    SCOPE.scope(scope, f).await
}

/// The caller of the request being served.
pub fn caller() -> Principal {
    SCOPE
        .try_with(|s| s.caller.clone())
        .unwrap_or_else(|_| Principal::anonymous())
}

/// Keep `error` — a runtime error object, `{code, message, data?}` — as the
/// answer to the request being served, unless one is kept already.
///
/// The first write wins because the first refusal is the one that ended the
/// request: a method that fails returns at once. Outside a request it is
/// dropped; there is nobody to answer.
pub(crate) fn record_error(error: &Value) {
    let _ = SCOPE.try_with(|s| {
        if let Ok(mut kept) = s.error.lock()
            && kept.is_none()
        {
            *kept = Some(error.clone());
        }
    });
}

/// The request being served, when one is.
fn serving() -> Option<RequestScope> {
    SCOPE.try_with(RequestScope::clone).ok()
}

/// A reactor reply's error object, under either spelling (see
/// [`RuntimePorts::call`]).
pub(crate) fn error_of(v: &Value) -> Option<&Value> {
    v.get("_error").or_else(|| v.get("error"))
}

/// The reactor's error, as the spec's error type: its code — the spec's, or
/// agentd's own outside the reserved range — and message as they are, and its
/// `data` as the typed details the SDK carries. The reactor marks a failed
/// answer with an `_error` member rather than returning a `Result`, because
/// its reply channel carries one JSON value; going through the typed error is
/// what makes the protocol layer emit an error instead of passing it off as a
/// successful result that happens to contain one.
pub(crate) fn from_error_object(e: &Value) -> A2AError {
    use crate::a2a::errors as code;
    let n = e
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or(code::INTERNAL_ERROR);
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("internal error");
    let error = A2AError::new(i32::try_from(n).unwrap_or(code::INTERNAL_ERROR as i32), msg);
    match e
        .get("data")
        .cloned()
        .and_then(|d| serde_json::from_value::<Vec<TypedDetail>>(d).ok())
    {
        Some(details) if !details.is_empty() => error.with_details(details),
        _ => error,
    }
}

/// Read a `Task` out of a reactor reply, which may be the task itself or the
/// `{task}` envelope a send answers with.
///
/// A task with no id is not a task. The spec's message defaults every field,
/// so `null` — or any object that is not a task at all — would read as an
/// empty one; accepting it would answer a caller with a blank `Task` and call
/// that success. Refused instead, as the internal error it is.
fn task_from(v: Value) -> Result<WireTask, A2AError> {
    let body = match v.get("task") {
        Some(t) => t.clone(),
        None => v,
    };
    if !body
        .get("id")
        .is_some_and(|id| id.as_str().is_some_and(|s| !s.is_empty()))
    {
        return Err(A2AError::internal("the runtime answered without a task"));
    }
    from_wire(body).map_err(A2AError::internal)
}

/// The task a reply is about must be the task that was asked about. Fail
/// closed: an answer for some other id is an internal error, never a result.
fn same_task(want: &str, t: WireTask) -> Result<WireTask, A2AError> {
    if t.id == want {
        Ok(t)
    } else {
        Err(A2AError::internal(format!(
            "the runtime answered for task {:?}, not {want:?}",
            t.id
        )))
    }
}

/// One transition, as a stream reads it.
#[derive(Clone)]
pub struct Published {
    /// Its position in its task's channel.
    seq: u64,
    response: StreamResponse,
    /// It ends a stream: the task reached a terminal state.
    terminal: bool,
    /// It ends a blocking send's wait: terminal, or waiting on the caller.
    settles: bool,
}

/// The per-task channels the reactor publishes into and streams read from.
///
/// A channel exists while somebody watches its task: one is made when a stream
/// or a blocking send starts watching, and goes when nobody is left to read it
/// (seen at the next publish) or retention drops the task. A transition with
/// nobody watching is published nowhere — there is nobody to tell.
#[derive(Default)]
pub struct Fanout {
    channels: Mutex<HashMap<String, Channel>>,
}

struct Channel {
    tx: broadcast::Sender<Published>,
    /// The position of the last transition published.
    seq: u64,
}

impl Fanout {
    /// Watch `task`, from now.
    pub fn subscribe(&self, task: &str) -> broadcast::Receiver<Published> {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        channels
            .entry(task.to_string())
            .or_insert_with(|| Channel {
                tx: broadcast::channel(STREAM_BUFFER).0,
                seq: 0,
            })
            .tx
            .subscribe()
    }

    /// The position of `task`'s last transition (`0` when nobody watches it).
    pub fn seq(&self, task: &str) -> u64 {
        let channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        channels.get(task).map_or(0, |c| c.seq)
    }

    /// Publish one transition of `task` to whoever watches it.
    fn publish(&self, task: &str, response: StreamResponse, state: Option<&TaskState>) {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ch) = channels.get_mut(task) else {
            return;
        };
        if ch.tx.receiver_count() == 0 {
            channels.remove(task);
            return;
        }
        ch.seq += 1;
        let _ = ch.tx.send(Published {
            seq: ch.seq,
            response,
            terminal: state.is_some_and(TaskState::is_terminal),
            settles: state.is_some_and(settles),
        });
    }

    /// Drop `task`'s channel: its streams end.
    fn forget(&self, task: &str) {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        channels.remove(task);
    }
}

/// Where the reactor publishes a task transition so subscribers see it, and
/// fires its webhooks.
///
/// Publishing is synchronous and never waits (a full channel drops a slow
/// reader's oldest transitions, and the reader is told). Webhook deliveries
/// are blocking POSTs, so they run on the listener's runtime, off the reactor.
pub struct StreamSink {
    streams: Arc<Fanout>,
    handle: tokio::runtime::Handle,
    log: crate::obs::log::Logger,
}

impl StreamSink {
    pub fn new(
        streams: Arc<Fanout>,
        handle: tokio::runtime::Handle,
        log: crate::obs::log::Logger,
    ) -> StreamSink {
        StreamSink {
            streams,
            handle,
            log,
        }
    }

    /// The position of `task`'s last transition — what a snapshot read now
    /// already shows (see [`Fanout`]).
    pub fn seq(&self, task: &str) -> u64 {
        self.streams.seq(task)
    }

    /// Drop everything the stream side holds for a task retention dropped:
    /// its channel, and with it its streams.
    pub fn forget(&self, task_id: &str) {
        self.streams.forget(task_id);
    }

    /// Publish a task's status as it now is.
    pub fn status(&self, task: &crate::a2a::tasks::Task) {
        let ev = crate::a2a::wire::status_event(task);
        let state = ev.status.state.clone();
        self.streams
            .publish(&task.id, StreamResponse::StatusUpdate(ev), Some(&state));
    }

    /// Publish a delivered artifact.
    pub fn artifact(&self, task_id: &str, context_id: &str, artifact: a2a_types::Artifact) {
        let ev = crate::a2a::wire::artifact_event(task_id, context_id, artifact, true);
        self.streams
            .publish(task_id, StreamResponse::ArtifactUpdate(ev), None);
    }

    /// Deliver this task's state to every webhook registered on it, as the
    /// `StreamResponse` the spec says a webhook receives (see
    /// [`crate::a2a::wire::push_body`]).
    ///
    /// Best-effort and off the reactor: a webhook that is down, slow, or now
    /// pointing somewhere it should not must not affect the task it is
    /// reporting on. Each delivery is one blocking POST on the blocking pool.
    pub fn push(&self, task: &crate::a2a::tasks::Task, allow_private: bool) {
        let event = crate::a2a::wire::push_body(task);
        for target in &task.push {
            let target = target.clone();
            let event = event.clone();
            let task_id = task.id.clone();
            let log = self.log.clone();
            self.handle.spawn(async move {
                let outcome = tokio::task::spawn_blocking(move || {
                    crate::a2a::push::deliver(&target, &event, allow_private)
                })
                .await;
                if let Ok(Err(e)) = outcome {
                    log.warn(
                        "a2a.push.failed",
                        serde_json::json!({"task": task_id, "err": e}),
                    );
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// Ports over a stand-in reactor that answers each call with `answer`.
    fn ports_with(
        answer: impl Fn(&crate::runtime::a2a_server::A2aRequest) -> Value + Send + 'static,
    ) -> (RuntimePorts, Arc<Fanout>) {
        let resolver =
            crate::a2a::Resolver::build(&crate::config::settings::A2a::default(), &|_| None)
                .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(crate::runtime::events::Event::A2a(req)) = rx.recv() {
                let _ = req.reply.send(answer(&req));
            }
        });
        let streams = Arc::new(Fanout::default());
        (
            RuntimePorts::new(A2aBridge::new(tx, resolver), Arc::clone(&streams)),
            streams,
        )
    }

    /// Ports over a stand-in reactor that answers every call with `reply`.
    fn ports_answering(reply: Value) -> RuntimePorts {
        ports_with(move |_| reply.clone()).0
    }

    /// An anonymous request's scope.
    fn scope() -> RequestScope {
        RequestScope::new(Principal::anonymous(), Via::Implicit, Active::NONE)
    }

    fn wire_task(id: &str, state: &str) -> Value {
        json!({"id": id, "contextId": "c", "status": {"state": state}})
    }

    fn send_request(task: Option<&str>, immediately: bool) -> SendMessageRequest {
        let mut message =
            a2a_types::Message::new(a2a_types::Role::User, vec![a2a_types::Part::text("hello")]);
        message.message_id = "m-1".into();
        message.task_id = task.map(str::to_string);
        SendMessageRequest {
            message,
            configuration: Some(a2a_types::SendMessageConfiguration {
                accepted_output_modes: None,
                task_push_notification_config: None,
                history_length: None,
                return_immediately: Some(immediately),
            }),
            metadata: None,
            tenant: None,
        }
    }

    /// The runtime's error object is kept exactly as the runtime answered it —
    /// code, message and data — because the SDK renders the typed error in its
    /// own words. The first one wins: it is the refusal that ended the request.
    #[tokio::test]
    async fn a_runtime_refusal_is_kept_whole_and_the_first_wins() {
        let refusal = json!({"code": -32603, "message": "the agent is draining",
            "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                      "reason": "DRAINING", "domain": "agentd.dev"}]});
        let ports = ports_answering(json!({"_error": refusal}));
        let s = scope();
        let kept = Arc::clone(&s.error);
        with_request(s, async {
            let req = CancelTaskRequest {
                id: "task-1".into(),
                metadata: None,
                tenant: None,
            };
            let e = ports
                .cancel_task(&ServiceParams::new(), req)
                .await
                .unwrap_err();
            assert_eq!(e.code, -32603);
            assert_eq!(e.details.map(|d| d.len()), Some(1));
            // A second refusal in the same request does not replace it.
            record_error(&json!({"code": -32001, "message": "later"}));
        })
        .await;
        assert_eq!(kept.lock().unwrap().clone(), Some(refusal));

        // Outside a request there is nobody to answer, and nothing is kept.
        record_error(&json!({"code": -32001, "message": "nobody"}));
    }

    /// What the caller meant crosses with the message: a new task under an id
    /// minted here, or the task the caller named — and the push config the
    /// send carried, which the runtime attaches to the task.
    #[tokio::test]
    async fn a_send_crosses_with_its_intent_and_its_inline_push() {
        let calls: Arc<Mutex<Vec<Value>>> = Arc::default();
        let seen = Arc::clone(&calls);
        let (ports, _) = ports_with(move |req| {
            seen.lock().unwrap().push(req.params.clone());
            let id = req.params["taskId"].as_str().unwrap_or("task-x");
            json!({"task": wire_task(id, "TASK_STATE_COMPLETED"), "_seq": 0})
        });
        let mut new = send_request(None, true);
        new.configuration
            .as_mut()
            .unwrap()
            .task_push_notification_config = Some(TaskPushNotificationConfig {
            url: "https://hooks.example/x".into(),
            id: None,
            task_id: String::new(),
            token: None,
            authentication: None,
            tenant: None,
        });
        let p = ServiceParams::new();
        with_request(scope(), async {
            ports.send_message(&p, new).await.expect("the new task");
            ports
                .send_message(&p, send_request(Some("task-named"), true))
                .await
                .expect("the named task");
        })
        .await;
        let calls = calls.lock().unwrap().clone();
        let (new, named) = (&calls[0], &calls[1]);
        assert_eq!(new["newTask"], true);
        assert!(
            new["taskId"].as_str().is_some_and(|t| !t.is_empty()),
            "{new}"
        );
        assert_eq!(new["push"]["url"], "https://hooks.example/x", "{new}");
        assert_eq!(new["message"]["role"], "ROLE_USER", "{new}");
        assert_eq!(named["taskId"], "task-named");
        assert_eq!(named["newTask"], false);
        assert!(named.get("push").is_none(), "{named}");
    }

    /// The task a send is answered with is the task it watches, or the send
    /// fails: any other would settle — or stream — somebody else's.
    #[tokio::test]
    async fn a_send_answered_with_another_task_fails() {
        let ports = ports_answering(json!({"task": wire_task("task-other", "TASK_STATE_WORKING")}));
        let got = with_request(scope(), async {
            ports
                .send_message(&ServiceParams::new(), send_request(Some("task-mine"), true))
                .await
        })
        .await;
        assert!(got.is_err());
    }

    /// A blocking send waits for its task to settle — on the transitions the
    /// reactor publishes, never on one its snapshot already shows — and then
    /// answers with the task read back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocking_send_waits_for_its_task_to_settle() {
        let reads = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&reads);
        let (ports, streams) = ports_with(move |req| match req.method.as_str() {
            "SendMessage" => {
                let id = req.params["taskId"].as_str().unwrap().to_string();
                json!({"task": wire_task(&id, "TASK_STATE_WORKING"), "_seq": 0})
            }
            _ => {
                *counted.lock().unwrap() += 1;
                let id = req.params["id"].as_str().unwrap().to_string();
                json!({"id": id, "contextId": "c", "status": {"state": "TASK_STATE_COMPLETED"}, "_seq": 2})
            }
        });
        let publisher = Arc::clone(&streams);
        tokio::spawn(async move {
            // Wait for the send to start watching, then settle its task.
            loop {
                let id = publisher.channels.lock().unwrap().keys().next().cloned();
                if let Some(id) = id {
                    let mut t = crate::a2a::tasks::Task::new(
                        &id,
                        "c",
                        None,
                        crate::a2a::tasks::Link::Turn { ctx: "c".into() },
                    );
                    t.transition(crate::a2a::tasks::State::Completed, None);
                    let ev = crate::a2a::wire::status_event(&t);
                    let state = ev.status.state.clone();
                    publisher.publish(&id, StreamResponse::StatusUpdate(ev), Some(&state));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let answer = with_request(scope(), async {
            ports
                .send_message(&ServiceParams::new(), send_request(None, false))
                .await
        })
        .await
        .expect("an answer");
        let SendMessageResponse::Task(task) = answer else {
            panic!("a task");
        };
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(*reads.lock().unwrap(), 1, "read back once it settled");
    }

    /// A subscription opens with the task as it is, skips every transition the
    /// snapshot already shows, and closes at a terminal state; one on a task
    /// that has already ended is refused, never an empty stream.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_subscription_opens_with_the_task_and_closes_at_its_end() {
        let (ports, streams) = ports_with(|req| {
            let id = req.params["id"].as_str().unwrap().to_string();
            let state = if id == "t-done" {
                "TASK_STATE_COMPLETED"
            } else {
                "TASK_STATE_WORKING"
            };
            json!({"id": id, "contextId": "c", "status": {"state": state}, "_seq": 1})
        });
        let p = ServiceParams::new();
        let refused = with_request(scope(), async {
            ports
                .subscribe_to_task(
                    &p,
                    SubscribeToTaskRequest {
                        id: "t-done".into(),
                        tenant: None,
                    },
                )
                .await
                .err()
        })
        .await;
        assert_eq!(refused.map(|e| e.code), Some(-32004));

        let mut s = with_request(scope(), async {
            ports
                .subscribe_to_task(
                    &p,
                    SubscribeToTaskRequest {
                        id: "t-1".into(),
                        tenant: None,
                    },
                )
                .await
                .expect("a stream")
        })
        .await;
        let status = |state: crate::a2a::tasks::State| {
            let mut t = crate::a2a::tasks::Task::new(
                "t-1",
                "c",
                None,
                crate::a2a::tasks::Link::Turn { ctx: "c".into() },
            );
            t.transition(state, None);
            let ev = crate::a2a::wire::status_event(&t);
            let st = ev.status.state.clone();
            (StreamResponse::StatusUpdate(ev), st)
        };
        // Position 1 is already in the snapshot: it is not sent again.
        for state in [
            crate::a2a::tasks::State::Working,
            crate::a2a::tasks::State::InputRequired,
            crate::a2a::tasks::State::Completed,
        ] {
            let (ev, st) = status(state);
            streams.publish("t-1", ev, Some(&st));
        }
        let mut got = Vec::new();
        while let Some(item) = s.next().await {
            got.push(item.expect("an event"));
        }
        assert!(matches!(got[0], StreamResponse::Task(_)), "{got:?}");
        let states: Vec<TaskState> = got[1..]
            .iter()
            .map(|e| match e {
                StreamResponse::StatusUpdate(u) => u.status.state.clone(),
                other => panic!("a status, got {other:?}"),
            })
            .collect();
        assert_eq!(states, [TaskState::InputRequired, TaskState::Completed]);
    }

    /// Every way a reply can fail to be the answer it claims is an error, and
    /// every code crosses as the code it is.
    #[tokio::test]
    async fn fail_closed_and_codes() {
        // A task with no id is not a task — `null` included, which the spec's
        // all-defaults message would otherwise read as an empty success.
        for v in [
            Value::Null,
            json!({}),
            json!({"task": null}),
            json!({"x": 1}),
        ] {
            assert!(task_from(v.clone()).is_err(), "{v}");
        }
        let t = task_from(json!({"task": wire_task("task-1", "TASK_STATE_WORKING")})).unwrap();
        assert_eq!(t.id, "task-1");
        assert!(same_task("task-2", t).is_err());

        // Every code keeps its number, and its data travels as typed details.
        let e = from_error_object(&json!({"code": -31403, "message": "no",
            "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "PERMISSION_DENIED"}]}));
        assert_eq!(e.code, -31403);
        assert_eq!(e.message, "no");
        assert_eq!(e.details.map(|d| d.len()), Some(1));
        assert_eq!(
            from_error_object(&json!({"code": -32001, "message": "gone"})).code,
            -32001
        );
    }

    /// A task retention forgets leaves nothing on the stream side: its channel
    /// goes, its streams end, and every other task keeps its own; a channel
    /// nobody reads is dropped at its next publish.
    #[test]
    fn a_forgotten_task_leaves_no_stream_state() {
        let streams = Fanout::default();
        let mut one = streams.subscribe("t-1");
        let _two = streams.subscribe("t-2");
        streams.forget("t-1");
        assert!(matches!(
            one.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
        assert_eq!(
            streams.channels.lock().unwrap().len(),
            1,
            "t-2 keeps its own"
        );

        let reader = streams.subscribe("t-3");
        drop(reader);
        let ev = StreamResponse::Message(a2a_types::Message::new(a2a_types::Role::Agent, vec![]));
        streams.publish("t-3", ev, None);
        assert!(!streams.channels.lock().unwrap().contains_key("t-3"));
    }
}
