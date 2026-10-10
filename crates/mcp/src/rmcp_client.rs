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
//! **Two ways in, one revision offered on each.** A 2026-07-28 server is
//! reached with `server/discover`, which offers 2026-07-28 and nothing else
//! (`discover_lifecycle`); an older one with the `initialize` handshake, which
//! offers `INITIALIZE_OFFER` — rmcp's `LATEST_WITH_INITIALIZE`, the newest
//! revision that still has an `initialize` — or the newest older one a server
//! lists. [`FirstRequest`] says which goes first, and `classify` decides
//! when a refusal means the other era. Both pins are named in exactly one
//! place each, and a test holds them to rmcp's constants. Everything
//! version-dependent here (notably [`RmcpClient::subscribe`]) branches on the
//! *negotiated* version, compared against rmcp's own constants.
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
use crate::wire::method;
use crate::wire::{Implementation, Prompt, ReadResourceResult, Resource, ServerCapabilities, Tool};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::rmcp_transport::BootstrapFailure;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult,
    ElicitationAction, ElicitationCapability, ErrorCode, Implementation as RmcpImpl,
    ProtocolVersion, ReadResourceRequestParams, RequestMetaObject, SubscriptionFilter,
};
use rmcp::service::{
    ClientInitializeError, RoleClient, RunningService, Subscription, SubscriptionEnd,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt};

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

/// The server notifications [`Handler`] queues for the host. One the transport
/// takes off a stream itself (ahead of a `tools/list` answer, see
/// [`crate::rmcp_transport::AgentdHttp::with_notifications`]) is queued only if
/// it is one of these, as the handler would have queued it.
pub(crate) const QUEUED_NOTIFICATIONS: [&str; 6] = [
    method::NOTIFY_RESOURCES_UPDATED,
    method::NOTIFY_RESOURCES_LIST_CHANGED,
    method::NOTIFY_TOOLS_LIST_CHANGED,
    method::NOTIFY_PROMPTS_LIST_CHANGED,
    method::NOTIFY_MESSAGE,
    method::NOTIFY_PROGRESS,
];

/// Queue `v` — a message taken off a stream outside the SDK — for the host,
/// as [`Handler`] would have: a notification of a kind it queues goes onto
/// `queue`, any other notification is passed over as the handler would pass
/// it over. `false`, and nothing queued, for a message that is not a
/// notification (a request needs the handler to answer it).
pub(crate) fn queue_as_handler(queue: &Mutex<Vec<rpc::Notification>>, v: &Value) -> bool {
    let Some(method) = v.get("method").and_then(Value::as_str) else {
        return false;
    };
    if v.get("id").is_some() {
        return false;
    }
    if QUEUED_NOTIFICATIONS.contains(&method) {
        let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
        queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(rpc::Notification::new(method, Some(params)));
    }
    true
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
            method::NOTIFY_RESOURCES_UPDATED,
            serde_json::to_value(&params).unwrap_or_else(|_| json!({})),
        );
    }

    async fn on_resource_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue(method::NOTIFY_RESOURCES_LIST_CHANGED, json!({}));
    }

    async fn on_tool_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue(method::NOTIFY_TOOLS_LIST_CHANGED, json!({}));
    }

    async fn on_prompt_list_changed(&self, _ctx: rmcp::service::NotificationContext<RoleClient>) {
        self.queue(method::NOTIFY_PROMPTS_LIST_CHANGED, json!({}));
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
            method::NOTIFY_MESSAGE,
            serde_json::to_value(&params).unwrap_or_else(|_| json!({})),
        );
    }

    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _ctx: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.queue(
            method::NOTIFY_PROGRESS,
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
    /// `retry_ms`. `idle`: the stream was quiet past its idle bound and cut
    /// for that alone — routine, since a listen is quiet whenever nothing
    /// changes, and followed by a resync of what it held.
    Ended {
        reason: String,
        retry_ms: u64,
        idle: bool,
    },
    /// The standing listen (the `list_changed` notifications, opened at
    /// connect) was refused, failed or got no answer. Said once; it is asked
    /// for again on the usual backoff without a word, and its acknowledgment
    /// is said as [`Self::Resumed`]. The connection works without it.
    Unavailable { reason: String, retry_ms: u64 },
    /// The server refused a listen that asked for its `list_changed`
    /// notifications beside URIs, and took the URIs alone: the connection
    /// stops asking for them, so its URI subscriptions keep working.
    FlagsRefused { reason: String },
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

/// How long a listen stream may stay silent before it is cut and opened again
/// (never less than the connection's timeout). A listen is quiet whenever
/// nothing changes; the connection's timeout, meant for a server that stopped
/// answering, would cut every quiet one, and listens cannot be resumed.
pub const LISTEN_IDLE: Duration = Duration::from_secs(600);

/// A blocking MCP client backed by the official SDK.
pub struct RmcpClient {
    name: String,
    rt: tokio::runtime::Runtime,
    service: RunningService<RoleClient, Handler>,
    /// The socket the connection runs on, read for whether the server has
    /// forgotten the session ([`crate::http::HttpTransport::session_lost`]).
    http: Arc<crate::http::HttpTransport>,
    caps: ServerCapabilities,
    /// The server's `capabilities` object exactly as it sent it, for members
    /// `caps` does not model ([`Self::capability`]).
    raw_caps: Option<Value>,
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
    /// Listen streams the socket cut for idleness, counted by the transport.
    idle_cuts: Arc<std::sync::atomic::AtomicU64>,
    /// Set once the server refused a listen that asked for its
    /// `list_changed` notifications ([`ListenEvent::FlagsRefused`]).
    flags_refused: std::sync::atomic::AtomicBool,
    /// The tool names the server's `tools/list` answers named during the
    /// current list (recorded by the transport, taken by the list).
    tools_seen: ToolNames,
    /// Tools dropped by the last list and already named, so each is named
    /// once while it stays dropped.
    tools_reported: Mutex<std::collections::BTreeSet<String>>,
    /// Dropped tools not yet drained by the host.
    tools_dropped: Mutex<Vec<String>>,
    /// What the SDK says about this client on every 2026-07-28 request —
    /// `clientInfo` and `clientCapabilities` — for [`Self::call_raw`], which
    /// says the same.
    own_meta: Value,
    /// The next id [`Self::call_raw`] uses: a range the SDK's own counter,
    /// which starts at 0, never reaches.
    raw_ids: std::sync::atomic::AtomicI64,
}

/// Where [`RmcpClient::call_raw`]'s request ids start.
const RAW_ID_BASE: i64 = 1 << 40;

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
    /// What the connection sends first; see [`FirstRequest`].
    first: FirstRequest,
    /// How long a listen stream may stay silent; see [`LISTEN_IDLE`].
    listen_idle: Duration,
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
            first: FirstRequest::Discover,
            listen_idle: LISTEN_IDLE,
        }
    }

    /// Let a listen stream stay silent for `idle` (never less than the
    /// connection's timeout) before it is cut; see [`LISTEN_IDLE`].
    pub fn with_listen_idle(mut self, idle: Duration) -> Self {
        self.listen_idle = idle;
        self
    }

    /// Send `first` first; see [`FirstRequest`].
    pub fn start_with(mut self, first: FirstRequest) -> Self {
        self.first = first;
        self
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

    /// Connect: reach the server through `server/discover` or `initialize`, as
    /// [`Self::start_with`] says, falling back to the other when the server's
    /// answer shows it speaks the other era.
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

        let own_meta = json!({
            "clientInfo": serde_json::to_value(&implementation).unwrap_or_else(|_| json!({})),
            "clientCapabilities": serde_json::to_value(&caps).unwrap_or_else(|_| json!({})),
        });
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
        let start = Start {
            endpoint: self.endpoint.clone(),
            socket: Arc::clone(&socket),
            timeout: self.timeout,
            listen_idle: self.listen_idle,
            handler: Handler {
                queue: Arc::clone(&notifications),
                // Replaced per attempt: `initialize` carries the revision it
                // offers here, and `server/discover` its own in `_meta`.
                info: client_config(caps, implementation, INITIALIZE_OFFER),
                elicitation: self.elicitation.clone(),
            },
        };
        let first = self.first;
        let bound = self.handshake_bound;
        let started = std::time::Instant::now();
        let (service, raw_caps, idle_cuts, tools_seen) = rt
            .block_on(async move {
                match bound {
                    // The timer is built inside the runtime: it needs its clock.
                    Some(b) => tokio::time::timeout(b, start.run(first))
                        .await
                        .map_err(|_| format!("handshake timed out after {} ms", b.as_millis()))?,
                    None => start.run(first).await,
                }
            })
            .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?;
        // Before any request can be issued (neither startup goes through the
        // cache): the cache is per peer and the peer only exists once the
        // startup returns. With it on, a read inside the server's `ttlMs`
        // never reaches the server, and a failed read past it is answered with
        // the expired copy — so a dead or refusing registry would keep
        // "confirming" freshness.
        rt.block_on(
            service
                .peer()
                .set_response_cache_config(rmcp::ClientCacheConfig::disabled()),
        );

        let info = service.peer_info();
        let protocol_version = info.as_ref().map(|i| i.protocol_version.clone());
        // The server's own capabilities object, exactly as sent — members
        // rmcp does not model (`events`) included. rmcp's typed view is the
        // fallback for a startup whose answer was not captured whole.
        let raw_caps = raw_caps.or_else(|| {
            info.as_ref()
                .and_then(|i| serde_json::to_value(i.as_ref()).ok())
                .and_then(|v| v.get("capabilities").cloned())
        });
        let caps = raw_caps
            .as_ref()
            .and_then(|c| serde_json::from_value(c.clone()).ok())
            .unwrap_or_default();

        let client = RmcpClient {
            name: self.name,
            rt,
            service,
            http: socket,
            caps,
            raw_caps,
            protocol_version,
            notifications,
            uris: Arc::default(),
            subscribing: Mutex::new(()),
            pump: Mutex::new(None),
            listen_events: Arc::default(),
            idle_cuts,
            flags_refused: std::sync::atomic::AtomicBool::new(false),
            tools_seen,
            tools_reported: Mutex::default(),
            tools_dropped: Mutex::default(),
            own_meta,
            raw_ids: std::sync::atomic::AtomicI64::new(RAW_ID_BASE),
        };
        // rmcp sends a tool's `Mcp-Param-*` headers only for tools whose
        // schema it has cached on this connection from a `tools/list` — and a
        // connection that never lists (a turn worker's, handed its tools by
        // the supervisor) would send a call its server refuses for the
        // missing header. So a 2026-07-28 connection lists once before it is
        // used — after the response cache is off, so the list is a real one —
        // and a connection that cannot list cannot send those headers, and
        // fails. One POST per dial, as the probe is for an older server.
        if client.listens() && client.caps.tools.is_some() {
            // Inside the handshake bound: what is left of it, not a new one.
            client.prime(bound.map(|b| b.saturating_sub(started.elapsed())))?;
        }
        client.stand();
        Ok(client)
    }
}

/// What a connection sends first, named by mechanism rather than by revision.
///
/// Either way, a server of the other era is still reached: a refusal of the
/// first request that shows the server speaks the other one leads to the
/// other request, once. The host remembers what reached a server
/// ([`RmcpClient::first_request`]) and starts its next connection to it the
/// same way — the spec lets a client cache the era per origin and re-probe
/// when the assumption fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstRequest {
    /// `server/discover`, the 2026-07-28 lifecycle: no handshake and no
    /// session, everything per request. A server that predates it is reached
    /// through `initialize`.
    Discover,
    /// `initialize`, the 2025-11-25 handshake and its session. A server that
    /// refuses it is re-probed with `server/discover`.
    Initialize,
}

/// The 2026-07-28 lifecycle, and the only revision `server/discover` offers.
///
/// Not rmcp's `ClientLifecycleMode::Auto`, which probes with this and falls
/// back by itself: its probe has a fixed 10 s timer, and over agentd's socket
/// an answer that arrives after it fails the connection instead of falling
/// back; and it does not fall back when a server answers that it speaks only
/// older revisions. agentd falls back itself ([`classify`]), and the probe is
/// bounded like every other request, by the connection's timeout (or the
/// handshake bound when one is set).
fn discover_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    }
}

/// What to do after `server/discover` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Next {
    /// Nothing `initialize` could fix: say the discover failure.
    Refuse,
    /// The server speaks an older era: send `initialize` offering this.
    Initialize(ProtocolVersion),
}

/// Classify a failed `server/discover`. The fallback is broad on purpose:
/// every server that answers `initialize` today must keep being reached, so
/// anything that does not PROVE the server modern-but-unwilling, or unreachable,
/// falls back. `failure` is what the socket recorded when the probe got no
/// JSON-RPC answer at all.
fn classify(e: &ClientInitializeError, failure: Option<BootstrapFailure>) -> Next {
    use ClientInitializeError as E;
    match e {
        // A modern server refusing agentd's own request (a header it holds
        // wrong, a capability it requires): `initialize` would fare no better.
        E::JsonRpcError(d)
            if d.code == ErrorCode::HEADER_MISMATCH
                || d.code == ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY =>
        {
            Next::Refuse
        }
        // The server listed what it speaks and 2026-07-28 is not there (a
        // `-32022`, or a DiscoverResult): reach it at the newest
        // `initialize`-era revision it lists that rmcp knows, else nothing.
        // A version refusal that lists nothing proves nothing about the
        // server: reach it as any other refusal.
        E::NoCompatibleProtocolVersion {
            server_supported, ..
        } if server_supported.is_empty() => Next::Initialize(INITIALIZE_OFFER),
        E::NoCompatibleProtocolVersion {
            server_supported, ..
        } => server_supported
            .iter()
            .filter(|v| v.has_initialize() && ProtocolVersion::KNOWN_VERSIONS.contains(v))
            .max_by(|a, b| a.as_str().cmp(b.as_str()))
            .map_or(Next::Refuse, |v| Next::Initialize(v.clone())),
        _ => match failure {
            // The challenge an operator logs in with: never hidden behind a
            // second request. (403 falls back: a proxy that allow-lists
            // methods refuses one it does not know and still passes
            // `initialize`.)
            Some(BootstrapFailure::Status(401)) => Next::Refuse,
            // The server never saw the request; there is no era to learn.
            Some(BootstrapFailure::Unreached) => Next::Refuse,
            _ => Next::Initialize(INITIALIZE_OFFER),
        },
    }
}

/// Should a refused `initialize` be followed by `server/discover`? Only when
/// the server answered — a JSON-RPC refusal, or a 4xx other than 401 — which
/// is how a 2026-07-28-only server refuses a handshake it does not have. A 403
/// re-probes too, for the reason a 403 to `server/discover` falls back: a
/// proxy that allow-lists methods refuses the one it does not know. A 5xx,
/// silence or a server never reached says nothing about the era.
fn re_probes(e: &ClientInitializeError, failure: Option<BootstrapFailure>) -> bool {
    matches!(e, ClientInitializeError::JsonRpcError(_))
        || matches!(failure, Some(BootstrapFailure::Status(s)) if (400..500).contains(&s) && s != 401)
}

/// Everything one startup attempt needs, built once per connection.
struct Start {
    endpoint: String,
    socket: Arc<crate::http::HttpTransport>,
    timeout: Duration,
    listen_idle: Duration,
    handler: Handler,
}

/// A started connection: the service, the server's capabilities as sent, the
/// transport's count of listen streams cut for idleness, and the tool names
/// its `tools/list` answers named.
type Started = (
    RunningService<RoleClient, Handler>,
    Option<Value>,
    Arc<std::sync::atomic::AtomicU64>,
    ToolNames,
);

type ToolNames = Arc<Mutex<std::collections::BTreeSet<String>>>;

/// A failed startup attempt: rmcp's error, and what the socket recorded when
/// the request got no JSON-RPC answer at all. Boxed: rmcp's error is large.
type Failed = Box<(ClientInitializeError, Option<BootstrapFailure>)>;

impl Start {
    /// One attempt in `mode`: a fresh transport over the same socket (which
    /// holds no session until an `initialize` answer issues one), the startup,
    /// and what the socket recorded about it.
    async fn attempt(
        &self,
        mode: ClientLifecycleMode,
        offer: ProtocolVersion,
    ) -> Result<Started, Failed> {
        let client = crate::rmcp_transport::AgentdHttp::new(Arc::clone(&self.socket), self.timeout)
            .with_listen_idle(self.listen_idle)
            .with_notifications(Arc::clone(&self.handler.queue));
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
        let transport = StreamableHttpClientTransport::with_client(client.clone(), config);
        let mut handler = self.handler.clone();
        handler.info.protocol_version = offer;
        match handler.serve_with_lifecycle(transport, mode).await {
            Ok(service) => Ok((
                service,
                client.raw_capabilities(),
                client.idle_cuts(),
                client.tools_seen(),
            )),
            Err(e) => Err(Box::new((e, client.bootstrap_failure()))),
        }
    }

    /// The whole startup, starting with `first`. Each request goes out at most
    /// once: a fallback that is refused too is said with both refusals, never
    /// answered with a third request.
    async fn run(&self, first: FirstRequest) -> Result<Started, String> {
        let initialize = |v: ProtocolVersion| self.attempt(ClientLifecycleMode::Initialize, v);
        match first {
            FirstRequest::Discover => {
                let (d, failure) = match self.attempt(discover_lifecycle(), INITIALIZE_OFFER).await
                {
                    Ok(s) => return Ok(s),
                    Err(e) => *e,
                };
                match classify(&d, failure) {
                    Next::Refuse => Err(format!("server/discover: {}", said(&d))),
                    Next::Initialize(v) => initialize(v).await.map_err(|f| {
                        format!("server/discover: {}; initialize: {}", said(&d), said(&f.0))
                    }),
                }
            }
            FirstRequest::Initialize => {
                let (f, failure) = match initialize(INITIALIZE_OFFER).await {
                    Ok(s) => return Ok(s),
                    Err(e) => *e,
                };
                if !re_probes(&f, failure) {
                    return Err(format!("initialize: {}", said(&f)));
                }
                self.socket.forget_session();
                // The re-probe never leads back to a second `initialize`.
                self.attempt(discover_lifecycle(), INITIALIZE_OFFER)
                    .await
                    .map_err(|d| {
                        format!("initialize: {}; server/discover: {}", said(&f), said(&d.0))
                    })
            }
        }
    }
}

/// A startup error as an operator reads it: rmcp's text, with a JSON-RPC
/// refusal's code and the server's own message.
fn said(e: &ClientInitializeError) -> String {
    match e {
        ClientInitializeError::JsonRpcError(d) => format!("{} ({})", d.message, d.code.0),
        other => other.to_string(),
    }
}

fn rpc_err(name: &str, op: &str, e: impl std::fmt::Display) -> McpError {
    McpError::Transport(format!("mcp server '{name}': {op}: {e}"))
}

impl RmcpClient {
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.caps
    }

    /// The member `name` of the server's capabilities, exactly as sent.
    pub fn capability(&self, name: &str) -> Option<Value> {
        self.raw_caps.as_ref().and_then(|c| c.get(name)).cloned()
    }

    /// What reached the server: `server/discover` for a connection at a
    /// revision without `initialize`, the handshake otherwise.
    pub fn first_request(&self) -> FirstRequest {
        if self.listens() {
            FirstRequest::Discover
        } else {
            FirstRequest::Initialize
        }
    }

    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_ref().map(ProtocolVersion::as_str)
    }

    /// One JSON-RPC request the SDK does not model — an extension's method —
    /// on this connection's own socket, signed like every other request.
    ///
    /// It carries what the SDK puts on each request it sends: the negotiated
    /// `MCP-Protocol-Version`, and at a revision without `initialize`
    /// `Mcp-Method` and the per-request `_meta` — the revision, the client's
    /// info, and its capabilities with `extensions` declared in them (a
    /// method's own `_meta` keys are kept). `Mcp-Name` is not set: the
    /// revision defines it for tools, resources and prompts only. The id comes
    /// from a range the SDK's counter never reaches, and a notification sent
    /// on the answer's stream reaches the host's queue as the handler would
    /// queue it.
    ///
    /// `bound` is the whole exchange's: an answer still streaming at it —
    /// keepalives, notifications — is hung up on, so no server holds the
    /// caller past it.
    ///
    /// The outer error is the connection's (a transport failure, a lost
    /// session, the bound); the inner one is the server's JSON-RPC refusal,
    /// with its `code` and `data`, for the caller to branch on. A result
    /// whose `resultType` is anything but `"complete"` is a protocol error:
    /// this call has no way to follow an interim result.
    pub fn call_raw(
        &self,
        method: &str,
        mut params: serde_json::Map<String, Value>,
        extensions: Value,
        bound: Duration,
    ) -> Result<Result<Value, rpc::RpcError>, McpError> {
        let id = self
            .raw_ids
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let version = self.protocol_version().unwrap_or_default().to_string();
        let mut headers: Vec<(&str, &str)> = vec![("MCP-Protocol-Version", &version)];
        if self.listens() {
            let mut caps = self.own_meta["clientCapabilities"].clone();
            if let Some(ext) = extensions.as_object().filter(|e| !e.is_empty())
                && let Some(c) = caps.as_object_mut()
                && let Value::Object(have) = c.entry("extensions").or_insert_with(|| json!({}))
            {
                have.extend(ext.clone());
            }
            let meta = params.entry("_meta").or_insert_with(|| json!({}));
            if !meta.is_object() {
                // The revision's keys need an object; what the caller put
                // there is kept beside them rather than dropped.
                *meta = json!({"value": meta.take()});
            }
            meta["io.modelcontextprotocol/protocolVersion"] = json!(version);
            meta["io.modelcontextprotocol/clientInfo"] = self.own_meta["clientInfo"].clone();
            meta["io.modelcontextprotocol/clientCapabilities"] = caps;
            headers.push(("Mcp-Method", method));
        }
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let bytes = serde_json::to_vec(&body).map_err(|e| {
            McpError::Transport(format!("mcp server '{}': {method}: {e}", self.name))
        })?;
        let queue = Arc::clone(&self.notifications);
        // The socket's timeout bounds each read; the exchange as a whole is
        // bounded here, by hanging it up.
        let hangup = Arc::new(crate::http::Hangup::default());
        let (finished, deadline) = std::sync::mpsc::channel::<()>();
        let timer = {
            let hangup = Arc::clone(&hangup);
            std::thread::spawn(move || {
                if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    deadline.recv_timeout(bound)
                {
                    hangup.hang_up();
                }
            })
        };
        let answered = self.http.send_until_hangup(
            Some(id),
            &bytes,
            bound,
            &headers,
            |n| {
                queue_as_handler(&queue, &n);
            },
            &hangup,
        );
        drop(finished);
        let _ = timer.join();
        let answer = match answered {
            Ok(Some(v)) => v,
            Ok(None) => {
                return Err(McpError::Transport(format!(
                    "mcp server '{}': {method}: no answer",
                    self.name
                )));
            }
            Err(e) if self.http.session_lost() => {
                return Err(McpError::SessionExpired(format!(
                    "mcp server '{}': {method}: {e}",
                    self.name
                )));
            }
            Err(e) => {
                return Err(McpError::Transport(format!(
                    "mcp server '{}': {method}: {e}",
                    self.name
                )));
            }
        };
        if let Some(error) = answer.get("error") {
            return match serde_json::from_value::<rpc::RpcError>(error.clone()) {
                Ok(e) => Ok(Err(e)),
                Err(e) => Err(McpError::Transport(format!(
                    "mcp server '{}': {method}: an error answer that is not JSON-RPC's: {e}",
                    self.name
                ))),
            };
        }
        let result = answer.get("result").cloned().unwrap_or(Value::Null);
        match result.get("resultType").and_then(Value::as_str) {
            None | Some("complete") => Ok(Ok(result)),
            Some(other) => Err(McpError::Transport(format!(
                "mcp server '{}': {method}: resultType {other:?} is not one this request can follow",
                self.name
            ))),
        }
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
        self.forget_seen();
        let res = self
            .rt
            .block_on(self.service.list_all_tools())
            .map_err(|e| self.fail("tools/list", e));
        let res = self.listed(res)?;
        self.convert(&res, "tools/list")
    }

    /// The priming list (see `connect`), bounded like the startup.
    fn prime(&self, bound: Option<Duration>) -> Result<(), McpError> {
        self.forget_seen();
        let res = self
            .bounded(bound, "tools/list", self.service.list_all_tools())
            .and_then(|r| r.map_err(|e| self.fail("tools/list", e)));
        self.listed(res).map(|_| ())
    }

    fn forget_seen(&self) {
        self.tools_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Compare what this list's answers named with what rmcp kept, and queue
    /// each newly dropped tool once. A failed list compares nothing; a tool
    /// kept again can be named again if it is dropped later.
    fn listed(
        &self,
        res: Result<Vec<rmcp::model::Tool>, McpError>,
    ) -> Result<Vec<rmcp::model::Tool>, McpError> {
        let seen = std::mem::take(&mut *self.tools_seen.lock().unwrap_or_else(|e| e.into_inner()));
        let tools = res?;
        let kept: std::collections::BTreeSet<&str> =
            tools.iter().map(|t| t.name.as_ref()).collect();
        let dropped: std::collections::BTreeSet<String> = seen
            .into_iter()
            .filter(|t| !kept.contains(t.as_str()))
            .collect();
        let mut reported = self
            .tools_reported
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reported.retain(|t| dropped.contains(t));
        let fresh: Vec<String> = dropped
            .into_iter()
            .filter(|t| !reported.contains(t))
            .collect();
        reported.extend(fresh.iter().cloned());
        self.tools_dropped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(fresh);
        Ok(tools)
    }

    /// Tools the server listed that rmcp dropped, each named once per
    /// connection. At 2026-07-28 rmcp leaves out of a `tools/list` every tool
    /// whose schema carries an `x-mcp-header` annotation it rejects, and says
    /// so only to its own tracing — so the agent would lose a tool with
    /// nothing in any log. This crate has no logger; the host says it.
    pub fn drain_dropped_tools(&self) -> Vec<String> {
        std::mem::take(&mut *self.tools_dropped.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// `tools/call`, bounded as a whole by `bound` when there is one. `meta`
    /// (run id, idempotency key, traceparent) is the request's `params._meta`
    /// — the field MCP reserves for it — and the tool's `arguments` carry the
    /// tool's arguments alone: a server validating them against a strict
    /// schema would refuse a stray `_meta` there.
    ///
    /// A bound covers every round of the call, not one HTTP exchange — the
    /// operator's answer to an elicitation included; on expiry the call is
    /// abandoned and the caller gets a timeout error.
    ///
    /// **An abandoned call is not cancelled, and its exchange is not hung
    /// up.** Dropping the SDK's call future drops a request handle that does
    /// nothing when dropped — only a subscription cancels that way — so the
    /// request stays pending in the SDK's worker, which keeps its response
    /// stream open. The exchange therefore runs on until the server answers,
    /// the socket's idle bound (this connection's timeout) fires, or the
    /// client is closed (the SDK then aborts its streams, which hangs them
    /// up), without holding up the calls behind it (see
    /// [`crate::rmcp_transport`]). The
    /// server is told nothing meanwhile: no `notifications/cancelled` (the
    /// SDK's call helper, which drives the MRTR rounds, does not expose the
    /// request id), and at 2026-07-28 no closed stream either. Without a
    /// bound, only the socket's timeout on each silence can end the call.
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

    /// Subscribe to a resource, by whichever mechanism the connection's
    /// revision defines.
    ///
    /// The revisions disagree: up to 2025-11-25 a client calls
    /// `resources/subscribe` per URI, and the subscription lives on the
    /// session; from 2026-07-28 on `subscriptions/listen` replaces it (rmcp
    /// marks the former deprecated *for those versions only*), and the
    /// subscription lives as long as the listen's response stream. Which
    /// revision a connection speaks is decided by what reached the server —
    /// `initialize` or `server/discover` — so the choice is read off the
    /// negotiated version rather than hard-coded: calling the wrong one
    /// leaves the host with a subscription the server never honours.
    ///
    /// Either way the server must have advertised `resources.subscribe`, in
    /// whichever answer started the connection: a server that did not would
    /// accept the call, or ignore the URI in a listen filter, and never
    /// notify — a wait that parks forever with nothing in any log. That is a
    /// [`McpError::Capability`] here instead, with nothing sent, and so is a
    /// listen whose acknowledgment leaves the URI out: rmcp lets a server
    /// accept a narrower filter than the one asked for, and then drops every
    /// update for what it left out.
    ///
    /// The URI is recorded only once the server accepted it, so a failed
    /// subscribe is a real retry next time rather than "already covered". At
    /// 2025-11-25 only the new URI is sent; the ones already held stay held.
    /// From 2026-07-28 on, one listen covers every tracked URI: adding a URI
    /// opens a listen with the widened filter, and the one it replaces is
    /// hung up once the server has acknowledged the new one, so the server
    /// never holds the narrower listen beside it. Its notifications pump into
    /// the queue the host drains, and a listen stream that ends — as it does
    /// when the server restarts, there being no session to forget — is said
    /// ([`ListenEvent`]) and opened again.
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

    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        self.unsubscribe_bounded(uri, None)
    }

    /// [`Self::unsubscribe`], abandoned as a transport error after `bound`.
    /// The URI is let go of on this side either way: nothing will read its
    /// updates.
    pub fn unsubscribe_within(&self, uri: &str, bound: Duration) -> Result<(), McpError> {
        self.unsubscribe_bounded(uri, Some(bound))
    }

    #[allow(deprecated)]
    fn unsubscribe_bounded(&self, uri: &str, bound: Option<Duration>) -> Result<(), McpError> {
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
            let op = format!("resources/unsubscribe {uri}");
            return self
                .bounded(
                    bound,
                    &op,
                    self.service
                        .unsubscribe(rmcp::model::UnsubscribeRequestParams::new(uri.to_string())),
                )?
                .map_err(|e| self.fail(&op, e));
        }
        self.listen_on(&narrowed, bound, None).map(|_| ())
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

    /// The `list_changed` notifications this connection's listens ask for:
    /// those the server advertised, unless it refused them in a filter
    /// ([`ListenEvent::FlagsRefused`]).
    fn list_flags(&self) -> (bool, bool, bool) {
        if self.flags_refused.load(std::sync::atomic::Ordering::SeqCst) {
            return (false, false, false);
        }
        let tools = self
            .caps
            .tools
            .as_ref()
            .and_then(|t| t.list_changed)
            .unwrap_or(false);
        let prompts = self
            .caps
            .prompts
            .as_ref()
            .and_then(|p| p.get("listChanged"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resources = self
            .caps
            .resources
            .as_ref()
            .and_then(|r| r.list_changed)
            .unwrap_or(false);
        (tools, prompts, resources)
    }

    /// Does any listen on this connection ask for a `list_changed`? Then one
    /// is held even with no URI subscribed: at 2026-07-28 a listen is the only
    /// way a `list_changed` arrives.
    fn wants_list_changed(&self) -> bool {
        let (t, p, r) = self.list_flags();
        t || p || r
    }

    /// The filter of a listen on `uris`, with the `list_changed` flags when
    /// `flags`.
    fn filter_for(
        &self,
        uris: &std::collections::BTreeSet<String>,
        flags: bool,
    ) -> SubscriptionFilter {
        let mut b = SubscriptionFilter::builder();
        if flags {
            let (tools, prompts, resources) = self.list_flags();
            if tools {
                b = b.tools_list_changed();
            }
            if prompts {
                b = b.prompts_list_changed();
            }
            if resources {
                b = b.resources_list_changed();
            }
        }
        for u in uris {
            b = b.resource_subscription(u.clone());
        }
        b.build()
    }

    /// What a pump on this connection shares with it.
    fn pump_ctx(&self) -> PumpCtx {
        PumpCtx {
            peer: self.service.peer().clone(),
            queue: Arc::clone(&self.notifications),
            events: Arc::clone(&self.listen_events),
            held: Arc::clone(&self.uris),
            idle_cuts: Arc::clone(&self.idle_cuts),
        }
    }

    /// Open the standing listen in the background: the `list_changed`
    /// notifications the server advertised, no URI. Never awaited — a server
    /// that refuses it, fails it or is slow to acknowledge it must not fail or
    /// delay a connection that works without it ([`establish`]).
    fn stand(&self) {
        if !self.listens() || !self.wants_list_changed() {
            return;
        }
        let filter = self.filter_for(&Default::default(), true);
        let handle = self.rt.spawn(pump(None, self.pump_ctx(), filter));
        *self.pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// (Re)open the single subscription covering `uris` (and the
    /// `list_changed` notifications the server advertised), and pump its
    /// notifications into the drain queue on a background task that listens
    /// again whenever the stream ends. The previous pump is stopped only once
    /// the new listen is acknowledged, so a refused widening leaves the URIs
    /// already held still watched.
    ///
    /// Returns the URIs the acknowledgment names — the accepted set from now
    /// on, since the new listen replaces the old one. A held URI it leaves
    /// out (anything but `asking`, which the caller reports) is said as
    /// [`ListenEvent::Narrowed`].
    ///
    /// A server that advertises a `list_changed` but refuses that flag in a
    /// filter keeps its URI subscriptions: the listen is asked for once more
    /// without the flags, and the connection stops asking for them
    /// ([`ListenEvent::FlagsRefused`]).
    fn listen_on(
        &self,
        uris: &std::collections::BTreeSet<String>,
        bound: Option<Duration>,
        asking: Option<&str>,
    ) -> Result<std::collections::BTreeSet<String>, McpError> {
        if uris.is_empty() && !self.wants_list_changed() {
            self.stop_pump();
            self.uris.lock().unwrap_or_else(|e| e.into_inner()).clear();
            return Ok(Default::default());
        }
        let peer = self.service.peer().clone();
        let flagged = self.filter_for(uris, true);
        let subscription =
            match self.bounded(bound, "subscriptions/listen", peer.listen(flagged.clone()))? {
                Ok(s) => s,
                // Only a refusal: a transport failure says nothing about the flags,
                // and turning them off for it would cost the connection its
                // `list_changed` notifications for good.
                Err(e) if listen_refused(&e) && !uris.is_empty() && self.wants_list_changed() => {
                    let plain = self.filter_for(uris, false);
                    match self.bounded(bound, "subscriptions/listen", peer.listen(plain))? {
                        Ok(s) => {
                            self.flags_refused
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            self.listen_events
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .push(ListenEvent::FlagsRefused {
                                    reason: format!("subscriptions/listen: {e}"),
                                });
                            s
                        }
                        Err(_) => return Err(self.fail("subscriptions/listen", e)),
                    }
                }
                Err(e) => return Err(self.fail("subscriptions/listen", e)),
            };
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
        let handle = self
            .rt
            .spawn(pump(Some(subscription), self.pump_ctx(), filter));
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

/// The most listen events a connection keeps until its host drains them. A
/// connection nothing drains (a turn worker's) keeps only the latest, so a
/// server that stays down does not grow its queue without end.
const LISTEN_EVENTS_KEPT: usize = 256;

/// What a listen pump shares with its connection.
#[derive(Clone)]
struct PumpCtx {
    peer: rmcp::service::Peer<RoleClient>,
    queue: Arc<Mutex<Vec<rpc::Notification>>>,
    events: Arc<Mutex<Vec<ListenEvent>>>,
    held: Arc<Mutex<std::collections::BTreeSet<String>>>,
    /// Listen streams the socket cut for idleness, counted by the transport.
    idle_cuts: Arc<std::sync::atomic::AtomicU64>,
}

impl PumpCtx {
    fn report(&self, e: ListenEvent) {
        let mut q = self.events.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() >= LISTEN_EVENTS_KEPT {
            q.remove(0);
        }
        q.push(e);
    }

    fn queue_update(&self, uri: &str) {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(rpc::Notification::new(
                crate::wire::method::NOTIFY_RESOURCES_UPDATED,
                Some(json!({ "uri": uri })),
            ));
    }

    /// The resource's content as one canonical string, or `None` when the
    /// read failed. Through the connection's own peer, so it is a read on the
    /// wire (the response cache is off).
    async fn read(&self, uri: &str) -> Option<String> {
        let r = self
            .peer
            .read_resource(ReadResourceRequestParams::new(uri.to_string()))
            .await
            .ok()?;
        serde_json::to_value(&r.contents)
            .ok()
            .map(|v| v.to_string())
    }
}

/// Did the server refuse a listen in JSON-RPC as one it will not take — a
/// method it does not have, a filter it does not accept? Asking again gets the
/// same answer. A transport failure is not that: agentd's socket answers one
/// as an internal error.
fn listen_refused(e: &rmcp::ServiceError) -> bool {
    matches!(
        e,
        rmcp::ServiceError::McpError(d)
            if d.code == ErrorCode::INVALID_PARAMS || d.code == ErrorCode::METHOD_NOT_FOUND
    )
}

/// Work for the [`resync`] reader.
enum Job {
    /// A listen was acknowledged: take what these URIs hold now.
    Baseline(Vec<String>),
    /// A live update arrived for this URI: read it again.
    Reread(String),
    /// The stream was seen to end at this instant.
    Ended(std::time::Instant),
    /// A re-listen was acknowledged: wake each of these URIs that changed.
    Resync(Vec<String>),
}

/// The pump's reads, on a task of their own so the pump drains its stream at
/// memory speed: rmcp ends a subscription whose 64-slot buffer fills, and a
/// pump that read after every update would fill it in any burst.
///
/// Listens cannot be resumed — a re-opened one starts from now — so an update
/// sent while one was being re-opened is gone. What agentd guarantees instead
/// is that state converges: what each held URI holds is read when a listen is
/// acknowledged and after each live update; after every re-listen it is read
/// again, and a URI whose content differs, or cannot be compared, gets ONE
/// wake (`resources/updated`), which the waits, starts and instruction on it
/// read as they would any update. Changes inside one gap are coalesced; a
/// change reverted inside the gap is not delivered. A read that finished after
/// the stream was seen to end may have run in the gap, so it is doubted, and
/// its URI woken. Only that one: a wake is not free — a one-shot `wait` it
/// reaches resolves — so a URI is never woken on suspicion alone. `pending`
/// coalesces re-reads: a burst of updates for one URI costs one read.
async fn resync(
    ctx: PumpCtx,
    mut jobs: tokio::sync::mpsc::UnboundedReceiver<Job>,
    pending: Arc<Mutex<std::collections::BTreeSet<String>>>,
) {
    // What each URI held, and when that read finished.
    let mut seen: std::collections::BTreeMap<String, (Option<String>, std::time::Instant)> =
        Default::default();
    while let Some(job) = jobs.recv().await {
        match job {
            Job::Baseline(uris) => {
                for uri in uris {
                    let now = ctx.read(&uri).await;
                    seen.insert(uri, (now, std::time::Instant::now()));
                }
            }
            Job::Reread(uri) => {
                pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&uri);
                let now = ctx.read(&uri).await;
                seen.insert(uri, (now, std::time::Instant::now()));
            }
            Job::Ended(at) => {
                for (content, read_at) in seen.values_mut() {
                    if *read_at > at {
                        *content = None;
                    }
                }
            }
            Job::Resync(uris) => {
                for uri in uris {
                    let now = ctx.read(&uri).await;
                    let before = seen.get(&uri).and_then(|(c, _)| c.clone());
                    if now.is_none() || before.is_none() || before != now {
                        ctx.queue_update(&uri);
                    }
                    seen.insert(uri, (now, std::time::Instant::now()));
                }
            }
        }
    }
}

/// Aborts a task when dropped: the pump's reader lives exactly as long as the
/// pump, which is aborted whenever its listen is replaced.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Open a listen on `filter`, retrying until the server takes it. Used for a
/// listen nobody is waiting on — the standing one that carries `list_changed`,
/// opened in the background at connect — so it must never fail anything: a
/// failure is said ONCE ([`ListenEvent::Unavailable`]), the listen is asked for
/// again on the usual backoff without a word, and its acknowledgment is said
/// as [`ListenEvent::Resumed`]. A server that refuses it in JSON-RPC will
/// refuse it again: said, and not asked for again (`None`).
async fn establish(ctx: &PumpCtx, filter: &SubscriptionFilter) -> Option<Subscription> {
    let mut wait = RELISTEN_MIN;
    let mut said = false;
    loop {
        match ctx.peer.listen(filter.clone()).await {
            Ok(s) => {
                if said {
                    ctx.report(ListenEvent::Resumed);
                }
                return Some(s);
            }
            Err(e) => {
                let refused = listen_refused(&e);
                if !said {
                    ctx.report(ListenEvent::Unavailable {
                        reason: format!("subscriptions/listen: {e}"),
                        retry_ms: if refused { 0 } else { wait.as_millis() as u64 },
                    });
                    said = true;
                }
                if refused {
                    return None;
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(RELISTEN_MAX);
            }
        }
    }
}

/// Pump one listen subscription into the drain queue, and when it ends — the
/// server closed it, completed it, cancelled it, the socket cut it for
/// idleness, or the client lagged — say so and listen again with the same
/// filter. A listen stream that ended and was not reopened left every
/// subscribed URI without a wake, with nothing in any log; the wait before
/// each retry doubles from [`RELISTEN_MIN`] to [`RELISTEN_MAX`], so a server
/// that refuses to hold one is not hammered.
///
/// `first` is the acknowledged listen to pump, or `None` for a standing one
/// still to be opened ([`establish`]).
///
/// A re-listen the server acknowledges with fewer URIs than it held leaves
/// those out of `held` and says so ([`ListenEvent::Narrowed`]), and the pump
/// listens on with what was acknowledged: rmcp drops every update outside the
/// acknowledgment, so a URI kept in `held` would read as watched and never
/// wake. Every re-listen is followed by a resync ([`resync`]).
async fn pump(first: Option<Subscription>, ctx: PumpCtx, mut filter: SubscriptionFilter) {
    let mut subscription = match first {
        Some(s) => s,
        None => match establish(&ctx, &filter).await {
            Some(s) => s,
            None => return,
        },
    };
    let (jobs, rx) = tokio::sync::mpsc::unbounded_channel();
    let pending: Arc<Mutex<std::collections::BTreeSet<String>>> = Arc::default();
    let _reader = AbortOnDrop(tokio::spawn(resync(ctx.clone(), rx, Arc::clone(&pending))));
    let mut acked = acknowledged(subscription.acknowledged());
    let _ = jobs.send(Job::Baseline(acked.iter().cloned().collect()));
    let mut wait = RELISTEN_MIN;
    loop {
        let opened = tokio::time::Instant::now();
        let cuts = ctx.idle_cuts.load(std::sync::atomic::Ordering::SeqCst);
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
                        let params = v.get("params").cloned();
                        if method == crate::wire::method::NOTIFY_RESOURCES_UPDATED
                            && let Some(uri) = params
                                .as_ref()
                                .and_then(|p| p.get("uri"))
                                .and_then(Value::as_str)
                            && acked.contains(uri)
                            && pending
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert(uri.to_string())
                        {
                            // The read that moves the baseline, so the next
                            // resync compares against what the host was just
                            // woken for.
                            let _ = jobs.send(Job::Reread(uri.to_string()));
                        }
                        ctx.queue
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(rpc::Notification::new(method, params));
                    }
                }
                Ok(None) => break ended(subscription.end()),
                Err(e) => break format!("subscriptions/listen: {e}"),
            }
        };
        let _ = jobs.send(Job::Ended(std::time::Instant::now()));
        let idle = ctx.idle_cuts.load(std::sync::atomic::Ordering::SeqCst) > cuts;
        // A stream that stayed open a good while ended for a reason of its
        // own, and starts the backoff over; one that ended at once did not.
        // An idle cut is the former by definition.
        if idle || opened.elapsed() >= RELISTEN_MAX {
            wait = RELISTEN_MIN;
        }
        loop {
            ctx.report(ListenEvent::Ended {
                reason,
                retry_ms: wait.as_millis() as u64,
                idle,
            });
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RELISTEN_MAX);
            match ctx.peer.listen(filter.clone()).await {
                Ok(s) => {
                    let now = acknowledged(s.acknowledged());
                    let dropped: Vec<String> = acked
                        .iter()
                        .filter(|u| !now.contains(*u))
                        .cloned()
                        .collect();
                    subscription = s;
                    acked = now;
                    ctx.report(ListenEvent::Resumed);
                    if !dropped.is_empty() {
                        ctx.held
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .retain(|u| acked.contains(u));
                        filter = subscription.acknowledged().clone();
                        ctx.report(ListenEvent::Narrowed { dropped });
                    }
                    let _ = jobs.send(Job::Resync(acked.iter().cloned().collect()));
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
    fn the_revisions_agentd_offers_are_pinned() {
        // `server/discover` offers 2026-07-28 alone; `initialize` offers the
        // newest revision that still has one. Neither follows rmcp's default:
        // when rmcp promotes a newer revision, this fails and asks for a
        // decision instead of moving agentd silently.
        assert_eq!(
            discover_lifecycle(),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28]
            }
        );
        let offered = client_config(
            ClientCapabilities::default(),
            RmcpImpl::new("a", "0"),
            INITIALIZE_OFFER,
        )
        .protocol_version;
        assert_eq!(offered, ProtocolVersion::LATEST_WITH_INITIALIZE);
        assert_eq!(offered, ProtocolVersion::V_2025_11_25);
        assert_eq!(
            ProtocolVersion::LATEST,
            ProtocolVersion::V_2026_07_28,
            "rmcp promoted a newer revision: decide whether agentd offers it"
        );
        assert_eq!(
            ProtocolVersion::NO_INITIALIZE,
            ProtocolVersion::V_2026_07_28
        );
        assert_eq!(crate::wire::PROTOCOL_VERSION, "2025-11-25");
        assert_eq!(crate::wire::DISCOVER_PROTOCOL_VERSION, "2026-07-28");
        // A connection starts with `server/discover` unless told otherwise.
        let b = RmcpBuilder::new(
            "a",
            "http://127.0.0.1:1/mcp",
            vec![],
            Duration::from_secs(1),
        );
        assert_eq!(b.first, FirstRequest::Discover);
    }

    #[test]
    fn classify_refuses_what_a_fallback_cannot_fix_and_falls_back_on_the_rest() {
        use BootstrapFailure::*;
        use ClientInitializeError as E;
        let rpc = |code: i32| E::JsonRpcError(rmcp::ErrorData::new(ErrorCode(code), "x", None));
        let listed = |vs: &[&str]| E::NoCompatibleProtocolVersion {
            client_supported: vec![ProtocolVersion::V_2026_07_28],
            server_supported: vs
                .iter()
                .map(|v| serde_json::from_value(json!(v)).unwrap())
                .collect(),
        };
        let closed = || E::ConnectionClosed("gone".into());
        // How a socket failure reaches the classifier: rmcp's transport
        // error, carrying no text of ours.
        let transport = || {
            E::transport::<StreamableHttpClientTransport<crate::rmcp_transport::AgentdHttp>>(
                rmcp::transport::streamable_http_client::StreamableHttpError::UnexpectedEndOfStream,
                "send discover request",
            )
        };
        let at = |v: ProtocolVersion| Next::Initialize(v);
        let rows: Vec<(E, Option<BootstrapFailure>, Next)> = vec![
            (rpc(-32020), None, Next::Refuse),
            (rpc(-32021), None, Next::Refuse),
            (closed(), Some(Status(401)), Next::Refuse),
            (closed(), Some(Unreached), Next::Refuse),
            (listed(&["2027-01-01"]), None, Next::Refuse),
            (
                listed(&["2025-06-18"]),
                None,
                at(ProtocolVersion::V_2025_06_18),
            ),
            (
                listed(&["2024-11-05", "2025-11-25"]),
                None,
                at(ProtocolVersion::V_2025_11_25),
            ),
            (rpc(-32601), None, at(INITIALIZE_OFFER)),
            (closed(), Some(Status(403)), at(INITIALIZE_OFFER)),
            (closed(), Some(Status(404)), at(INITIALIZE_OFFER)),
            (closed(), Some(Status(500)), at(INITIALIZE_OFFER)),
            (closed(), Some(NoAnswer), at(INITIALIZE_OFFER)),
            (E::ExpectedInitResult(None), None, at(INITIALIZE_OFFER)),
            (
                E::ConflictInitResponseId(
                    rmcp::model::NumberOrString::Number(0),
                    rmcp::model::NumberOrString::Number(1),
                ),
                None,
                at(INITIALIZE_OFFER),
            ),
            (closed(), None, at(INITIALIZE_OFFER)),
            (listed(&[]), None, at(INITIALIZE_OFFER)),
            (
                E::UncorrelatedErrorResponse {
                    expected: rmcp::model::NumberOrString::Number(0),
                    received: rmcp::model::NumberOrString::String("server-error".into()),
                },
                None,
                at(INITIALIZE_OFFER),
            ),
            (transport(), None, at(INITIALIZE_OFFER)),
            (transport(), Some(NoAnswer), at(INITIALIZE_OFFER)),
            (transport(), Some(Status(401)), Next::Refuse),
            (transport(), Some(Unreached), Next::Refuse),
        ];
        for (e, failure, want) in rows {
            assert_eq!(classify(&e, failure), want, "{e} / {failure:?}");
        }
    }

    #[test]
    fn a_refused_handshake_re_probes_only_when_the_server_answered() {
        use BootstrapFailure::*;
        let rpc = ClientInitializeError::JsonRpcError(rmcp::ErrorData::new(
            ErrorCode(-32022),
            "Unsupported protocol version",
            None,
        ));
        let closed = || ClientInitializeError::ConnectionClosed("gone".into());
        assert!(re_probes(&rpc, None), "a JSON-RPC refusal");
        assert!(
            re_probes(&closed(), Some(Status(400))),
            "a 4xx without a body"
        );
        assert!(re_probes(&closed(), Some(Status(404))));
        assert!(
            re_probes(&closed(), Some(Status(403))),
            "an allow-listing proxy"
        );
        assert!(!re_probes(&closed(), Some(Status(401))), "the challenge");
        assert!(!re_probes(&closed(), Some(Status(500))), "a server error");
        assert!(!re_probes(&closed(), Some(NoAnswer)), "silence");
        assert!(!re_probes(&closed(), Some(Unreached)), "never reached");
        assert!(!re_probes(&closed(), None));
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
