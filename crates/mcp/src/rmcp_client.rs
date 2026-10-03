// SPDX-License-Identifier: AGPL-3.0-only
//! The **official SDK**, wrapped so the rest of agentd stays blocking.
//!
//! [`rmcp`] — the Rust SDK maintained alongside the protocol — owns the
//! handshake and every operation (tools, resources, prompts, completion,
//! subscriptions), so spec-tracking is inherited rather than hand-maintained.
//! [`crate::client::McpClient`] is the builder agentd holds; this is what it
//! connects.
//!
//! **Blocking on the outside.** agentd's runtime is blocking: the supervisor is
//! a single-threaded reactor and the turn worker is a straight-line state
//! machine. rmcp is async. Rather than colour the entire codebase, this facade
//! owns a private runtime (multi-threaded, for the reason given at
//! [`RmcpBuilder::connect`]) and blocks on it, exposing synchronous methods.
//! The runtime lives as long as the client and dies with it.
//!
//! **agentd chooses the revision it offers in `initialize`.** rmcp 3.5's
//! `ProtocolVersion::default()` is its `LATEST`, 2026-07-28 — a revision with
//! no `initialize` at all (its lifecycle moved into per-request `_meta`). A
//! handshake that offered it would ask the server for something the handshake
//! itself cannot be. So `initialize` offers `INITIALIZE_OFFER`, rmcp's
//! `LATEST_WITH_INITIALIZE`, named in exactly one place. Everything
//! version-dependent here (notably [`RmcpClient::subscribe`]) still branches on
//! the *negotiated* version the server answered with, compared against rmcp's
//! own constants.
//!
//! **No response cache.** SEP-2549 lets a client reuse a `resources/read` or
//! list result for the server's `ttlMs`, and serve an expired one when a
//! re-fetch fails; rmcp does both by default. agentd switches that off on every
//! connection. It reads because it needs the server's *current* answer — the
//! §7.7 freshness watch, notify-then-read, signing-key resolution — and a copy
//! from memory reported as a read is the "reports success, did nothing" defect:
//! a registry that was down, or that answered "not found" to a withdrawn
//! instruction, went on confirming it. So every read and list is one request on
//! the wire, and a failure is an `Err`, never an earlier answer. There is no knob
//! to turn it back on; `refresh`/`freshness` is the lever for registry load.

use crate::client::McpError;
use crate::inbound;
use crate::rpc;
use crate::wire::{Implementation, Prompt, ReadResourceResult, Resource, ServerCapabilities, Tool};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult,
    ElicitationAction, ElicitationCapability, Implementation as RmcpImpl, ProtocolVersion,
    ReadResourceRequestParams, RequestMetaObject, SubscriptionFilter,
};
use rmcp::service::{RoleClient, RunningService, Subscription, SubscriptionEnd};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ServiceExt};

/// The revision agentd offers in `initialize`, and the only place it is named.
///
/// The newest one that still has an `initialize`: from 2026-07-28 on the
/// lifecycle is per-request `_meta`, so rmcp's `LATEST` (and its `default()`)
/// is a revision no `initialize` can ask for.
const INITIALIZE_OFFER: ProtocolVersion = ProtocolVersion::LATEST_WITH_INITIALIZE;

/// What the client tells a server about itself in `initialize`, at `version`.
/// The revision is a parameter, never the SDK's default, for the reason at
/// [`INITIALIZE_OFFER`].
fn client_config(
    caps: ClientCapabilities,
    implementation: RmcpImpl,
    version: ProtocolVersion,
) -> ClientConfig {
    ClientConfig::new(caps, implementation).with_protocol_version(version)
}

/// Bridges rmcp's `ClientHandler` onto agentd: a server's elicitation reaches
/// the host that can answer it, and a server's notifications reach the queue the
/// reactor drains.
///
/// The notification half matters more than it looks. agentd is a *reactive*
/// daemon: `notifications/resources/updated` is what wakes a subscribed
/// workflow. A handler that accepted those and dropped them would leave the
/// agent idle forever, with nothing in any log to say why.
#[derive(Clone)]
struct Handler {
    info: ClientConfig,
    /// The host's elicitation answerer; `None` when the capability is not
    /// declared, which is also what the handshake told the server.
    elicitation: Option<Arc<dyn inbound::Handler>>,
    /// Where a server's notifications land until the reactor drains them.
    queue: Arc<Mutex<Vec<rpc::Notification>>>,
}

impl Handler {
    fn queue(&self, method: &str, params: Value) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(rpc::Notification::new(method, Some(params)));
    }
}

fn declined() -> ElicitResult {
    ElicitResult::new(ElicitationAction::Decline)
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }

    async fn create_elicitation(
        &self,
        params: ElicitRequestParams,
        _ctx: rmcp::service::RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        // Only the form flavour asks for structured input; a URL elicitation
        // has nothing for `ask_human` to answer, so it is declined honestly.
        let (message, requested_schema) = match &params {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => (
                message.clone(),
                serde_json::to_value(requested_schema).unwrap_or_else(|_| json!({})),
            ),
            _ => return Ok(declined()),
        };
        let Some(handler) = &self.elicitation else {
            return Ok(declined());
        };
        // The host answers synchronously and may wait minutes for a human.
        // Run on the runtime, that wait would hold its one worker — and with
        // it the timer of a bounded call, which could then fire only after the
        // human had answered and the answer was on its way to the server. On a
        // plain thread it holds nothing: a bound fires on time, and an answer
        // for a call abandoned meanwhile lands on a closed channel. Not the
        // blocking pool either — the runtime waits for that pool when it is
        // dropped, so an unanswered question would hold the client's drop.
        let handler = Arc::clone(handler);
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(handler.handle(inbound::Inbound::Elicit {
                message,
                requested_schema,
            }));
        });
        let answer = rx.await.ok().flatten();
        Ok(match answer {
            Some(inbound::Answer::Accept(content)) => {
                ElicitResult::new(ElicitationAction::Accept).with_content(content)
            }
            Some(inbound::Answer::Decline) => declined(),
            // Nothing could ask: cancel is the honest outcome, and it is not an
            // error.
            _ => ElicitResult::new(ElicitationAction::Cancel),
        })
    }

    // ---- notifications: the reactive wake path ----

    async fn on_resource_updated(
        &self,
        params: rmcp::model::ResourceUpdatedNotificationParam,
        _ctx: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.queue(
            "notifications/resources/updated",
            serde_json::to_value(&params).unwrap_or_else(|_| json!({})),
        );
    }

    async fn on_resource_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue("notifications/resources/list_changed", json!({}));
    }

    async fn on_tool_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue("notifications/tools/list_changed", json!({}));
    }

    async fn on_prompt_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue("notifications/prompts/list_changed", json!({}));
    }

    // Upstream marks logging notifications deprecated, but a server that still
    // sends them is better heard than silently ignored.
    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _ctx: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.queue(
            "notifications/message",
            serde_json::to_value(&params).unwrap_or_else(|_| json!({})),
        );
    }

    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _ctx: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.queue(
            "notifications/progress",
            serde_json::to_value(&params).unwrap_or_else(|_| json!({})),
        );
    }
}

/// What happened to a connection's `subscriptions/listen` stream, for the host
/// to log. The stream is the connection's only source of `resources/updated`
/// at a stateless revision, so its end is never silent: the pump says it ended
/// and why, and that it is open again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenEvent {
    /// The stream ended (or a re-listen failed); the pump listens again in
    /// `retry_ms`.
    Ended { reason: String, retry_ms: u64 },
    /// A re-listen succeeded: the subscription covers every URI again.
    Resumed,
    /// The server acknowledged a listen without these URIs, which it held
    /// before: it no longer notifies for them, and the connection no longer
    /// counts them as subscribed. The host asks for them again, where a
    /// server that keeps refusing is said as the refusal it is.
    Narrowed { dropped: Vec<String> },
}

/// The first wait before re-listening after a listen stream ended. Doubled on
/// each consecutive end, up to [`RELISTEN_MAX`].
pub const RELISTEN_MIN: Duration = Duration::from_millis(250);
/// The longest wait between re-listens — and how long a stream must have
/// stayed open for its end to start the backoff over. A server that closes
/// every listen at once is re-asked at most this often, not in a tight loop.
pub const RELISTEN_MAX: Duration = Duration::from_secs(30);

/// A blocking MCP client backed by the official SDK.
pub struct RmcpClient {
    name: String,
    rt: tokio::runtime::Runtime,
    service: RunningService<RoleClient, Handler>,
    /// The socket the connection runs on, read for whether the server has
    /// forgotten the session ([`crate::http::HttpTransport::session_lost`]).
    http: Arc<crate::http::HttpTransport>,
    caps: ServerCapabilities,
    /// The revision the handshake settled on, kept as rmcp's own type so the
    /// version-dependent branches compare it the way rmcp does.
    protocol_version: Option<ProtocolVersion>,
    notifications: Arc<Mutex<Vec<rpc::Notification>>>,
    /// Every URI the server ACCEPTED a subscription for — inserted only once
    /// the server call succeeded, and at a stateless revision only if the
    /// listen's acknowledgment names it, so a URI the server did not take is
    /// asked for again on the next try rather than reported as already
    /// covered. At a stateless revision one `listen` subscription covers them
    /// all. Shared with the listen pump, which drops what a re-listen's
    /// acknowledgment leaves out; locked only for a read or a write, never
    /// across a server call, because the pump runs on the runtime's one
    /// worker and a host thread holding this lock inside `block_on` would
    /// stall the very task that call waits on.
    uris: Arc<Mutex<std::collections::BTreeSet<String>>>,
    /// Held across a whole subscribe or unsubscribe, so two of them cannot
    /// each widen the filter from the same old set. Never taken by the pump.
    subscribing: Mutex<()>,
    /// The task pumping that subscription into `notifications`, re-listening
    /// when it ends. Aborted, not merely dropped, when the filter changes: a
    /// dropped tokio handle detaches its task, which would go on listening
    /// with the old filter beside the new one.
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// What the pump reports about the listen stream, until the host drains it.
    listen_events: Arc<Mutex<Vec<ListenEvent>>>,
}

/// Builder state, so the host can declare capabilities before connecting (the
/// handshake carries them, so they cannot be added afterwards).
pub struct RmcpBuilder {
    name: String,
    endpoint: String,
    /// Request headers for the socket this builder dials when none is handed to
    /// it ([`Self::with_http`]); a supplied socket carries its own.
    headers: Vec<(String, String)>,
    timeout: Duration,
    client_info: Implementation,
    elicitation: Option<Arc<dyn inbound::Handler>>,
    /// agentd's authenticated socket. Present whenever the connection carries a
    /// credential the SDK's own client could not (a request signer, an mTLS
    /// identity); absent only in tests that dial a bare loopback server.
    http: Option<Arc<crate::http::HttpTransport>>,
    /// A bound on the handshake alone, below the connection's own `timeout`
    /// (which stays the per-silence bound of every later call). A host that
    /// re-dials from a thread that must not stall — the reactor — passes its
    /// short management bound here.
    handshake_bound: Option<Duration>,
}

impl RmcpBuilder {
    pub fn new(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Self {
        RmcpBuilder {
            name: name.to_string(),
            endpoint: endpoint.to_string(),
            headers,
            timeout,
            client_info: Implementation {
                name: "agentd".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
            },
            elicitation: None,
            http: None,
            handshake_bound: None,
        }
    }

    /// Bound the handshake as a whole; see `handshake_bound`.
    pub fn with_handshake_bound(mut self, bound: Duration) -> Self {
        self.handshake_bound = Some(bound);
        self
    }

    /// Use agentd's socket for this connection — the one carrying its request
    /// signer and mTLS identity.
    pub fn with_http(mut self, http: Arc<crate::http::HttpTransport>) -> Self {
        self.http = Some(http);
        self
    }

    pub fn with_client_info(mut self, info: Implementation) -> Self {
        self.client_info = info;
        self
    }

    /// Declare `elicitation` and route it to `handler`, the host seam that
    /// reaches `ask_human`.
    pub fn with_elicitation(mut self, handler: Arc<dyn inbound::Handler>) -> Self {
        self.elicitation = Some(handler);
        self
    }

    /// Connect and run the `initialize` handshake.
    pub fn connect(self) -> Result<RmcpClient, McpError> {
        // A *multi-threaded* runtime, deliberately. The SDK runs the transport
        // and the service dispatch as background tasks, and a server pushing a
        // notification (`resources/updated` — agentd's reactive wake) has to be
        // heard between our calls, not only during one. On a current-thread
        // runtime those tasks advance only inside `block_on`, so a daemon that
        // was idling — exactly when a wake matters — would never receive it.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("agentd-mcp")
            .build()
            .map_err(|e| {
                McpError::Transport(format!("mcp server '{}': runtime: {e}", self.name))
            })?;

        // The caller's headers are the socket's, never the SDK's too: the SDK
        // hands its custom headers to the socket on every request, so a copy
        // here put each one — `Authorization` included — on the wire twice.
        //
        // No re-initialization behind our back. When a server forgets the
        // session, the SDK would by default handshake again inside the
        // transport and replay the request — and the subscriptions the old
        // session held would be gone with nothing to say so, while every call
        // succeeded. The host is told instead (`McpError::SessionExpired`),
        // and re-dials and re-subscribes what the lost session carried.
        let config =
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                self.endpoint.clone(),
            )
            .reinit_on_expired_session(false);

        let mut caps = ClientCapabilities::default();
        if self.elicitation.is_some() {
            caps.elicitation = Some(ElicitationCapability::new());
        }

        // Notifications arrive on the `listen` subscription, not through the
        // handler — rmcp routes one or the other, never both.
        let notifications: Arc<Mutex<Vec<rpc::Notification>>> = Arc::default();
        let mut implementation = RmcpImpl::new(
            self.client_info.name.clone(),
            self.client_info.version.clone(),
        );
        implementation.title = self.client_info.title.clone();

        let handler = Handler {
            queue: Arc::clone(&notifications),
            // Never `ClientConfig::new`'s own default: that is rmcp's `LATEST`,
            // 2026-07-28, which has no `initialize`, so offering it here would
            // ask for a revision this handshake cannot be.
            info: client_config(caps, implementation, INITIALIZE_OFFER),
            elicitation: self.elicitation.clone(),
        };

        let name = self.name.clone();
        // The SDK speaks the protocol; agentd supplies the socket, so a
        // connection keeps its signer and its mTLS identity.
        let socket = match &self.http {
            Some(h) => Arc::clone(h),
            None => Arc::new(crate::http::HttpTransport::new(
                crate::http::McpEndpoint::parse(&self.endpoint)
                    .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?,
                self.headers.clone(),
            )),
        };
        let client = crate::rmcp_transport::AgentdHttp::new(Arc::clone(&socket), self.timeout);
        let bound = self.handshake_bound;
        let service = rt
            .block_on(async move {
                let transport = StreamableHttpClientTransport::with_client(client, config);
                match bound {
                    // The timer is built inside the runtime: it needs its clock.
                    Some(b) => tokio::time::timeout(b, handler.serve(transport))
                        .await
                        .map_err(|_| format!("handshake timed out after {} ms", b.as_millis()))?
                        .map_err(|e| e.to_string()),
                    None => handler.serve(transport).await.map_err(|e| e.to_string()),
                }
            })
            .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?;
        // Before any request can be issued (`initialize` itself is not cached):
        // the cache is per peer and the peer only exists once `serve` returns.
        // With it on, a read inside the server's `ttlMs` never reaches the
        // server, and a failed read past it is answered with the expired copy —
        // so a dead or refusing registry would keep "confirming" freshness.
        rt.block_on(
            service
                .peer()
                .set_response_cache_config(rmcp::ClientCacheConfig::disabled()),
        );

        let info = service.peer_info();
        let protocol_version = info.as_ref().map(|i| i.protocol_version.clone());
        let info_json = info
            .as_ref()
            .and_then(|i| serde_json::to_value(i.as_ref()).ok());
        let caps = server_capabilities(info_json.as_ref());

        Ok(RmcpClient {
            name: self.name,
            rt,
            service,
            http: socket,
            caps,
            protocol_version,
            notifications,
            uris: Arc::default(),
            subscribing: Mutex::new(()),
            pump: Mutex::new(None),
            listen_events: Arc::default(),
        })
    }
}

/// Translate rmcp's negotiated server capabilities into ours.
///
/// Deliberately via JSON rather than field-by-field: both sides are the same
/// wire shape, so a round trip is exact today and does not break the day rmcp
/// adds a capability we have not heard of.
fn server_capabilities(info: Option<&serde_json::Value>) -> ServerCapabilities {
    info.and_then(|v| v.get("capabilities"))
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default()
}

fn rpc_err(name: &str, op: &str, e: impl std::fmt::Display) -> McpError {
    McpError::Transport(format!("mcp server '{name}': {op}: {e}"))
}

impl RmcpClient {
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.caps
    }

    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_ref().map(ProtocolVersion::as_str)
    }

    /// Has the server forgotten this connection's session? Once it has, every
    /// call fails with [`McpError::SessionExpired`] and the subscriptions it
    /// held are gone; only a re-dial gets them back.
    pub fn session_lost(&self) -> bool {
        self.http.session_lost()
    }

    /// The error one failed server call becomes. A call that failed because
    /// the server no longer knows the session is said as exactly that — the
    /// SDK surfaces it as a transport failure like any other (the request's
    /// stream was already handed over when the `404` arrived), so the socket's
    /// record of the `404` is what tells them apart.
    fn fail(&self, op: &str, e: impl std::fmt::Display) -> McpError {
        if self.http.session_lost() {
            return McpError::SessionExpired(format!("mcp server '{}': {op}: {e}", self.name));
        }
        rpc_err(&self.name, op, e)
    }

    /// The URIs the server accepted a subscription for on this connection —
    /// what a re-dial must subscribe again, since a subscription lives and
    /// dies with its session.
    pub fn subscribed(&self) -> Vec<String> {
        self.uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Convert an rmcp value into our wire type. Both sides are the same JSON
    /// shape, so this is exact — and it does not need updating when rmcp adds a
    /// field we do not model.
    fn convert<T: serde::de::DeserializeOwned>(
        &self,
        v: &impl serde::Serialize,
        what: &str,
    ) -> Result<T, McpError> {
        let json = serde_json::to_value(v).map_err(|e| rpc_err(&self.name, what, e))?;
        serde_json::from_value(json).map_err(|e| rpc_err(&self.name, what, e))
    }

    pub fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        let res = self
            .rt
            .block_on(self.service.list_all_tools())
            .map_err(|e| self.fail("tools/list", e))?;
        self.convert(&res, "tools/list")
    }

    /// `tools/call`, bounded as a whole by `bound` when there is one. `meta`
    /// (run id, idempotency key, traceparent) is the request's `params._meta`
    /// — the field MCP reserves for it — and the tool's `arguments` carry the
    /// tool's arguments alone: a server validating them against a strict
    /// schema would refuse a stray `_meta` there.
    ///
    /// A bound covers every round of the call, not one HTTP exchange — the
    /// operator's answer to an elicitation included; on expiry the call is
    /// abandoned and the caller gets a timeout error. The abandoned exchange
    /// runs on until the socket's own timeout without holding up the calls
    /// behind it (see [`crate::rmcp_transport`]). No `notifications/cancelled`
    /// is sent for it: the SDK's call helper, which drives the MRTR rounds,
    /// does not expose the request id, so the server learns of the abandonment
    /// only when the exchange closes. Without a bound, only the socket's
    /// timeout on each silence can end the call.
    pub fn call_tool(
        &self,
        name: &str,
        args: Option<Value>,
        meta: Option<serde_json::Map<String, Value>>,
        bound: Option<Duration>,
    ) -> Result<Value, McpError> {
        let arguments = match args {
            Some(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        let mut param = CallToolRequestParams::new(name.to_string()).with_arguments(arguments);
        param.meta = meta.map(RequestMetaObject::from);
        let op = format!("tools/call {name}");
        let call = self.service.call_tool(param);
        let res = match bound {
            Some(bound) => self
                .rt
                // The timer is built inside the runtime: it needs its clock.
                .block_on(async { tokio::time::timeout(bound, call).await })
                .map_err(|_| {
                    rpc_err(
                        &self.name,
                        &op,
                        format!("timed out after {} ms", bound.as_millis()),
                    )
                })?,
            None => self.rt.block_on(call),
        }
        .map_err(|e| self.fail(&op, e))?;
        serde_json::to_value(&res).map_err(|e| rpc_err(&self.name, "tools/call", e))
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, McpError> {
        let res = self
            .rt
            .block_on(self.service.list_all_resources())
            .map_err(|e| self.fail("resources/list", e))?;
        self.convert(&res, "resources/list")
    }

    pub fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let res = self
            .rt
            .block_on(
                self.service
                    .read_resource(ReadResourceRequestParams::new(uri.to_string())),
            )
            .map_err(|e| self.fail(&format!("resources/read {uri}"), e))?;
        self.convert(&res, "resources/read")
    }

    pub fn list_prompts(&self) -> Result<Vec<Prompt>, McpError> {
        let res = self
            .rt
            .block_on(self.service.list_all_prompts())
            .map_err(|e| self.fail("prompts/list", e))?;
        self.convert(&res, "prompts/list")
    }

    pub fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<crate::wire::GetPromptResult, McpError> {
        let mut params = rmcp::model::GetPromptRequestParams::new(name);
        params.arguments = arguments.and_then(|a| a.as_object().cloned());
        let res = self
            .rt
            .block_on(self.service.get_prompt(params))
            .map_err(|e| self.fail(&format!("prompts/get {name}"), e))?;
        self.convert(&res, "prompts/get")
    }

    pub fn complete(
        &self,
        reference: Value,
        argument: Value,
    ) -> Result<crate::wire::CompleteResult, McpError> {
        // Both halves are protocol-typed by the SDK: `ref` is a tagged union
        // over prompt/resource references, and a shape it does not recognise is
        // refused here rather than on the wire.
        let r#ref = serde_json::from_value(reference)
            .map_err(|e| rpc_err(&self.name, "completion/complete ref", e))?;
        let argument = serde_json::from_value(argument)
            .map_err(|e| rpc_err(&self.name, "completion/complete argument", e))?;
        let res = self
            .rt
            .block_on(
                self.service
                    .complete(rmcp::model::CompleteRequestParams::new(r#ref, argument)),
            )
            .map_err(|e| self.fail("completion/complete", e))?;
        self.convert(&res, "completion/complete")
    }

    pub fn list_resource_templates(&self) -> Result<Vec<crate::wire::ResourceTemplate>, McpError> {
        let res = self
            .rt
            .block_on(self.service.list_all_resource_templates())
            .map_err(|e| self.fail("resources/templates/list", e))?;
        self.convert(&res, "resources/templates/list")
    }

    /// Subscribe to a resource, by whichever mechanism the negotiated revision
    /// actually defines.
    ///
    /// The revisions disagree: up to 2025-11-25 a client calls
    /// `resources/subscribe`, and from 2026-07-28 on `subscriptions/listen`
    /// replaces it (rmcp marks the former deprecated *for those versions
    /// only*). Because the server's answer decides the revision a connection
    /// speaks, the choice is read off the negotiated version rather than
    /// hard-coded — calling the wrong one leaves the host with a
    /// subscription the server never honours.
    ///
    /// Either way the server must have advertised `resources.subscribe`: a
    /// server that did not would accept the call, or ignore the URI in a
    /// listen filter, and never notify — a wait that parks forever with
    /// nothing in any log. That is a [`McpError::Capability`] here instead,
    /// and so is a listen whose acknowledgment leaves the URI out: rmcp lets a
    /// server accept a narrower filter than the one asked for, and then drops
    /// every update for what it left out.
    ///
    /// The URI is recorded only once the server accepted it, so a failed
    /// subscribe is a real retry next time rather than "already covered". At
    /// 2025-11-25 only the new URI is sent; the ones already held stay held.
    /// From 2026-07-28 on, one subscription covers every tracked URI: adding a
    /// URI reopens it with the widened filter, and its notifications pump into
    /// the queue the host drains.
    ///
    /// Only the socket's timeout on each silence ends the server call; a
    /// caller on a thread that must not stall bounds it with
    /// [`Self::subscribe_within`].
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        self.subscribe_bounded(uri, None)
    }

    /// [`Self::subscribe`], abandoned as a transport error after `bound` (the
    /// host's management bound). A server that accepts the connection and
    /// answers slowly would otherwise hold the caller for the connection's
    /// whole timeout, per URI.
    pub fn subscribe_within(&self, uri: &str, bound: Duration) -> Result<(), McpError> {
        self.subscribe_bounded(uri, Some(bound))
    }

    #[allow(deprecated)]
    fn subscribe_bounded(&self, uri: &str, bound: Option<Duration>) -> Result<(), McpError> {
        if !self.caps.supports_subscribe() {
            return Err(McpError::Capability(format!(
                "mcp server '{}' does not offer resources.subscribe; {uri} cannot be watched",
                self.name
            )));
        }
        let _serial = self.subscribing.lock().unwrap_or_else(|e| e.into_inner());
        let held = self.subscribed_set();
        if held.contains(uri) {
            return Ok(()); // already covered by the live subscription
        }
        let op = format!("resources/subscribe {uri}");
        if self.listens() {
            let mut widened = held;
            widened.insert(uri.to_string());
            let acked = self.listen_on(&widened, bound, Some(uri))?;
            if !acked.contains(uri) {
                return Err(McpError::Capability(format!(
                    "mcp server '{}' acknowledged the listen without {uri}; it will not notify for it",
                    self.name
                )));
            }
            return Ok(());
        }
        self.bounded(
            bound,
            &op,
            self.service
                .subscribe(rmcp::model::SubscribeRequestParams::new(uri.to_string())),
        )?
        .map_err(|e| self.fail(&op, e))?;
        self.uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uri.to_string());
        Ok(())
    }

    #[allow(deprecated)]
    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        let _serial = self.subscribing.lock().unwrap_or_else(|e| e.into_inner());
        let mut narrowed = self.subscribed_set();
        if !narrowed.remove(uri) {
            return Ok(());
        }
        // Before 2026-07-28 the server holds a per-URI subscription, so it
        // needs an explicit `resources/unsubscribe` — narrowing a filter would
        // not reach it. From then on, reopening the listen with the narrowed
        // filter is the cancellation.
        if !self.listens() {
            self.uris
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(uri);
            return self
                .rt
                .block_on(
                    self.service
                        .unsubscribe(rmcp::model::UnsubscribeRequestParams::new(uri.to_string())),
                )
                .map_err(|e| self.fail(&format!("resources/unsubscribe {uri}"), e));
        }
        self.listen_on(&narrowed, None, None).map(|_| ())
    }

    /// A copy of the accepted set, taken under its lock and released at once.
    fn subscribed_set(&self) -> std::collections::BTreeSet<String> {
        self.uris.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Run `f` on the runtime, abandoned as a timeout after `bound` if there
    /// is one.
    fn bounded<F: std::future::Future>(
        &self,
        bound: Option<Duration>,
        op: &str,
        f: F,
    ) -> Result<F::Output, McpError> {
        match bound {
            // The timer is built inside the runtime: it needs its clock.
            Some(b) => self
                .rt
                .block_on(async { tokio::time::timeout(b, f).await })
                .map_err(|_| {
                    rpc_err(
                        &self.name,
                        op,
                        format!("timed out after {} ms", b.as_millis()),
                    )
                }),
            None => Ok(self.rt.block_on(f)),
        }
    }

    /// (Re)open the single subscription covering `uris`, and pump its
    /// notifications into the drain queue on a background task that listens
    /// again whenever the stream ends. The previous pump is stopped only once
    /// the new listen is acknowledged, so a refused widening leaves the URIs
    /// already held still watched.
    ///
    /// Returns the URIs the acknowledgment names — the accepted set from now
    /// on, since the new listen replaces the old one. A held URI it leaves
    /// out (anything but `asking`, which the caller reports) is said as
    /// [`ListenEvent::Narrowed`].
    fn listen_on(
        &self,
        uris: &std::collections::BTreeSet<String>,
        bound: Option<Duration>,
        asking: Option<&str>,
    ) -> Result<std::collections::BTreeSet<String>, McpError> {
        if uris.is_empty() {
            self.stop_pump();
            self.uris.lock().unwrap_or_else(|e| e.into_inner()).clear();
            return Ok(Default::default());
        }
        let mut filter = SubscriptionFilter::builder().resources_list_changed();
        for u in uris {
            filter = filter.resource_subscription(u.clone());
        }
        let filter = filter.build();

        let peer = self.service.peer().clone();
        let subscription = self
            .bounded(bound, "subscriptions/listen", peer.listen(filter.clone()))?
            .map_err(|e| self.fail("subscriptions/listen", e))?;
        let acked = acknowledged(subscription.acknowledged());
        let dropped: Vec<String> = uris
            .iter()
            .filter(|u| !acked.contains(*u) && Some(u.as_str()) != asking)
            .cloned()
            .collect();
        self.stop_pump();
        *self.uris.lock().unwrap_or_else(|e| e.into_inner()) = acked.clone();
        if !dropped.is_empty() {
            self.listen_events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(ListenEvent::Narrowed { dropped });
        }
        // Re-listen with what the server took, not with what was asked: the
        // URIs it left out are the host's to ask for again.
        let filter = subscription.acknowledged().clone();
        let handle = self.rt.spawn(pump(
            subscription,
            peer,
            filter,
            Arc::clone(&self.notifications),
            Arc::clone(&self.listen_events),
            Arc::clone(&self.uris),
        ));
        *self.pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(acked)
    }

    fn stop_pump(&self) {
        if let Some(old) = self.pump.lock().unwrap_or_else(|e| e.into_inner()).take() {
            old.abort();
        }
    }

    /// Does the negotiated revision define `subscriptions/listen`? Every
    /// revision without an `initialize` does — rmcp's own predicate,
    /// `has_initialize`, the one its client uses to choose the same lifecycle
    /// (`service/client.rs`), so a later revision rmcp negotiates keeps the
    /// listen path rather than falling back to a method that revision removed.
    fn listens(&self) -> bool {
        self.protocol_version
            .as_ref()
            .is_some_and(|v| !v.has_initialize())
    }

    /// What the listen pump reported since the last drain.
    pub fn drain_listen_events(&self) -> Vec<ListenEvent> {
        std::mem::take(&mut *self.listen_events.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Drain notifications the handler queued: take what has arrived, leave
    /// the queue empty.
    pub fn drain_notifications(&self) -> Vec<rpc::Notification> {
        std::mem::take(&mut *self.notifications.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Take over what `lost` received and the host has not drained yet, ahead
    /// of anything this connection has queued since. A re-dial replaces the
    /// connection whole, and the queues are the connection's: an update that
    /// reached the old one just before the server restarted is often the last
    /// wake before it, and would be dropped with it.
    pub(crate) fn inherit_queues(&self, lost: &RmcpClient) {
        let mut notes = lost.drain_notifications();
        let mut q = self.notifications.lock().unwrap_or_else(|e| e.into_inner());
        notes.append(&mut q);
        *q = notes;
        let mut events = lost.drain_listen_events();
        let mut q = self.listen_events.lock().unwrap_or_else(|e| e.into_inner());
        events.append(&mut q);
        *q = events;
    }
}

/// The URIs a listen acknowledgment names.
fn acknowledged(filter: &SubscriptionFilter) -> std::collections::BTreeSet<String> {
    filter
        .resource_subscriptions
        .iter()
        .flatten()
        .cloned()
        .collect()
}

/// Pump one listen subscription into `queue`, and when it ends — the server
/// closed it, completed it, cancelled it, or the client lagged — say so on
/// `events` and listen again with the same filter. A listen stream that ended
/// and was not reopened left every subscribed URI without a wake, with nothing
/// in any log; the wait before each retry doubles from [`RELISTEN_MIN`] to
/// [`RELISTEN_MAX`], so a server that refuses to hold one is not hammered.
///
/// A re-listen the server acknowledges with fewer URIs than it held leaves
/// those out of `held` and says so ([`ListenEvent::Narrowed`]), and the pump
/// listens on with what was acknowledged: rmcp drops every update outside the
/// acknowledgment, so a URI kept in `held` would read as watched and never
/// wake.
async fn pump(
    mut subscription: Subscription,
    peer: rmcp::service::Peer<RoleClient>,
    mut filter: SubscriptionFilter,
    queue: Arc<Mutex<Vec<rpc::Notification>>>,
    events: Arc<Mutex<Vec<ListenEvent>>>,
    held: Arc<Mutex<std::collections::BTreeSet<String>>>,
) {
    let report = |e: ListenEvent| events.lock().unwrap_or_else(|p| p.into_inner()).push(e);
    let mut wait = RELISTEN_MIN;
    loop {
        let opened = tokio::time::Instant::now();
        let mut reason = loop {
            match subscription.next().await {
                // The SDK's notification serializes as `{method, params}` with
                // no `jsonrpc` member, so it is rebuilt from those two rather
                // than parsed as a whole frame — which failed on every one of
                // them and dropped each wake without a word.
                Ok(Some(note)) => {
                    if let Ok(v) = serde_json::to_value(&note)
                        && let Some(method) = v.get("method").and_then(Value::as_str)
                    {
                        queue
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(rpc::Notification::new(method, v.get("params").cloned()));
                    }
                }
                Ok(None) => break ended(subscription.end()),
                Err(e) => break format!("subscriptions/listen: {e}"),
            }
        };
        // A stream that stayed open a good while ended for a reason of its
        // own, and starts the backoff over; one that ended at once did not.
        if opened.elapsed() >= RELISTEN_MAX {
            wait = RELISTEN_MIN;
        }
        loop {
            report(ListenEvent::Ended {
                reason,
                retry_ms: wait.as_millis() as u64,
            });
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RELISTEN_MAX);
            match peer.listen(filter.clone()).await {
                Ok(s) => {
                    let acked = acknowledged(s.acknowledged());
                    let dropped: Vec<String> = acknowledged(&filter)
                        .into_iter()
                        .filter(|u| !acked.contains(u))
                        .collect();
                    subscription = s;
                    report(ListenEvent::Resumed);
                    if !dropped.is_empty() {
                        held.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .retain(|u| acked.contains(u));
                        filter = subscription.acknowledged().clone();
                        report(ListenEvent::Narrowed { dropped });
                    }
                    break;
                }
                Err(e) => reason = format!("subscriptions/listen: {e}"),
            }
        }
    }
}

/// Why a listen stream ended, in words for the log line.
fn ended(end: Option<&SubscriptionEnd>) -> String {
    match end {
        Some(SubscriptionEnd::Graceful(_)) => "the server completed the subscription".into(),
        Some(SubscriptionEnd::Cancelled) => "the subscription was cancelled".into(),
        Some(SubscriptionEnd::Lagged { capacity }) => {
            format!("notifications outran the {capacity}-slot buffer")
        }
        _ => "the stream closed".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_offers_the_newest_revision_that_has_one() {
        // `initialize` offers 2025-11-25 because it is the newest revision
        // with an `initialize`, not because it is the SDK's default (that is
        // 2026-07-28 now). agentd's mock answers `wire::PROTOCOL_VERSION`, so
        // pinning it to the offer keeps the mock and the handshake on one
        // revision.
        assert_eq!(INITIALIZE_OFFER, ProtocolVersion::LATEST_WITH_INITIALIZE);
        assert_eq!(INITIALIZE_OFFER, ProtocolVersion::V_2025_11_25);
        assert_eq!(crate::wire::PROTOCOL_VERSION, INITIALIZE_OFFER.as_str());
    }

    /// The SDK's own messages — a notification it sends, the answer to a
    /// server's elicitation — meet a forgotten session as rmcp's
    /// `SessionExpired`, on which rmcp by default handshakes again and
    /// replays, behind the host's back: the subscriptions the old session
    /// held would be gone while every call succeeded. The connection is built
    /// so it does not; the host re-dials and restores them instead.
    #[test]
    fn a_forgotten_session_never_makes_the_sdk_handshake_again_by_itself() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::atomic::{AtomicU64, Ordering};

        // A server that issues `s-<generation>` and forgets it on a restart.
        let generation = Arc::new(AtomicU64::new(0));
        let initializes = Arc::new(AtomicU64::new(0));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ep = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (g, inits) = (Arc::clone(&generation), Arc::clone(&initializes));
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let (mut len, mut session) = (0usize, None);
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap_or_default();
                    match k.trim().to_ascii_lowercase().as_str() {
                        "content-length" => len = v.trim().parse().unwrap_or(0),
                        "mcp-session-id" => session = Some(v.trim().to_string()),
                        _ => {}
                    }
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let live = format!("s-{}", g.load(Ordering::SeqCst));
                let reply: &[u8] = if line.starts_with("GET") {
                    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n"
                } else if msg["method"] == "initialize" {
                    inits.fetch_add(1, Ordering::SeqCst);
                    let b = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {
                        "protocolVersion": msg["params"]["protocolVersion"],
                        "capabilities": {}, "serverInfo": {"name": "s", "version": "0"}}})
                    .to_string();
                    let _ = write!(
                        w,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: {live}\r\nContent-Length: {}\r\n\r\n{b}",
                        b.len()
                    );
                    continue;
                } else if session.is_some_and(|s| s != live) {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
                } else {
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n"
                };
                let _ = w.write_all(reply);
            }
        });

        let client = RmcpBuilder::new("s", &ep, vec![], Duration::from_secs(5))
            .connect()
            .expect("connect");
        assert_eq!(initializes.load(Ordering::SeqCst), 1);
        generation.fetch_add(1, Ordering::SeqCst);
        let peer = client.service.peer().clone();
        let _ = client.rt.block_on(peer.notify_roots_list_changed());
        std::thread::sleep(Duration::from_millis(300));
        assert!(client.session_lost(), "the 404 was seen");
        assert_eq!(
            initializes.load(Ordering::SeqCst),
            1,
            "no handshake the host did not ask for"
        );
    }
}
