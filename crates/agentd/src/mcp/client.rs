// SPDX-License-Identifier: Apache-2.0
//! MCP client over the **Streamable HTTP** transport. RFC 0004; RFC 0012 (no local
//! process spawn).
//!
//! One client connects one remote server (`https`/`http`/`unix`/`vsock`) and
//! implements the client subset from RFC 0004: initialize + capability store,
//! tools (list+call), resources (list+read), subscribe/unsubscribe, ping. We
//! declare **no** client capabilities.
//!
//! Each request is one POST of a JSON-RPC frame over a fresh connection (the
//! per-request socket timeout is the per-call bound); the response is
//! `application/json` or an SSE stream. Server→client notifications ride a
//! long-lived `GET` SSE stream, opened lazily on the first subscribe — a
//! background thread pumps them into a queue [`Self::drain_notifications`] serves.

use crate::json::{self, RpcError};
use crate::mcp::http::{EventStream, HttpError, HttpTransport, McpEndpoint};
use crate::wire::mcp::{
    CallToolResult, ClientCapabilities, DiscoverResult, Era, Implementation, InitializeParams,
    InitializeResult, LATEST_MODERN_VERSION, ListResourcesResult, ListToolsResult, PROTOCOL_VERSION,
    ReadResourceParams, ReadResourceResult, Resource, SUPPORTED_PROTOCOL_VERSIONS,
    ServerCapabilities, SubscribeParams, Tool, UnsupportedProtocolVersion,
    UNSUPPORTED_PROTOCOL_VERSION_CODE, best_mutual_version, is_modern_error_code, method,
    negotiate_version,
};
// The modern (stateless) request builders live alongside `wire` in the mcp crate.
use ::mcp::modern;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug)]
pub enum McpError {
    Transport(String),
    /// A JSON-RPC error object from the server (protocol failure, distinct
    /// from a `tools/call` result with `isError: true`).
    Rpc(RpcError),
    /// No response within the per-request timeout.
    Timeout(String),
    /// The server doesn't advertise the capability the call needs.
    Capability(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Transport(m) => write!(f, "mcp: transport: {m}"),
            McpError::Rpc(e) => write!(f, "mcp: rpc error {}: {}", e.code, e.message),
            McpError::Timeout(m) => write!(f, "mcp: timeout: {m}"),
            McpError::Capability(m) => write!(f, "mcp: capability: {m}"),
        }
    }
}
impl std::error::Error for McpError {}

type NotifQueue = Arc<Mutex<VecDeque<json::Notification>>>;

/// A connected (and, after [`McpClient::initialize`], handshaken) remote MCP
/// server over Streamable HTTP.
pub struct McpClient {
    name: String,
    http: Arc<HttpTransport>,
    /// Notifications queued from two sources: those captured off a POST's SSE
    /// response, and the long-lived server→client `GET` SSE stream (`events`).
    notifications: NotifQueue,
    /// The background notification-stream thread, started lazily on first
    /// subscribe (the reactive push channel — a `GET` stream on legacy, a
    /// `subscriptions/listen` POST stream on modern).
    events: Mutex<Option<EventStreamHandle>>,
    /// The resource URIs subscribed to. On modern this is the filter the
    /// `subscriptions/listen` stream is (re)opened with; legacy subscribes
    /// per-URI over `resources/subscribe` and doesn't need it.
    subscribed_uris: Mutex<BTreeSet<String>>,
    next_id: AtomicI64,
    caps: ServerCapabilities,
    /// The protocol version negotiated at `initialize`/discovery; `None` until then.
    protocol_version: Option<String>,
    /// The protocol era established on connect: legacy (`initialize` handshake) or
    /// modern (stateless per-request `_meta`). Governs how every request is built.
    era: Era,
    timeout: Duration,
    /// Stamped into every `tools/call` request's `params._meta` (e.g.
    /// `{"agent/run_id": …}`) so backing services can dedupe retries
    /// (RFC 0011 §idempotency).
    tool_meta: Option<Value>,
}

/// The background notification-stream thread + its stop flag (RFC 0004 §GET SSE).
struct EventStreamHandle {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// Each read on the notification `GET` stream is bounded so the loop can poll its
/// stop flag between events (clean shutdown) — the reactive push cadence is far
/// below this, so a live update is never delayed by it.
const EVENT_READ_TIMEOUT: Duration = Duration::from_secs(1);

impl McpClient {
    /// Connect to a remote MCP server over Streamable HTTP (RFC 0004). `endpoint`
    /// is `https://…` / `http://…` / `unix:/path` / `vsock:cid:port`. `headers`
    /// are caller-owned request headers (auth/framing — resolved secret values,
    /// never templates or logs). No process is spawned (RFC 0012). Call
    /// [`Self::initialize`] before any tool/resource call.
    pub fn connect(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<McpClient, McpError> {
        let ep = McpEndpoint::parse(endpoint)
            .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?;
        Ok(McpClient {
            name: name.to_string(),
            http: Arc::new(HttpTransport::new(ep, headers)),
            notifications: Arc::new(Mutex::new(VecDeque::new())),
            events: Mutex::new(None),
            subscribed_uris: Mutex::new(BTreeSet::new()),
            next_id: AtomicI64::new(1),
            caps: ServerCapabilities::default(),
            protocol_version: None,
            // Established on connect; legacy is the safe default until then.
            era: Era::Legacy,
            timeout,
            tool_meta: None,
        })
    }

    /// Build a client from a declared [`McpServerSpec`]: connect to its remote
    /// `endpoint` over Streamable HTTP, resolving its secret-free auth header
    /// templates at this moment. Call [`Self::initialize`] before any call.
    pub fn from_spec(
        spec: &crate::config::McpServerSpec,
        timeout: Duration,
    ) -> Result<McpClient, McpError> {
        if spec.endpoint.trim().is_empty() {
            return Err(McpError::Transport(format!(
                "mcp server '{}' has no endpoint",
                spec.name
            )));
        }
        let headers =
            crate::mcp::auth::resolve_headers(&spec.headers).map_err(McpError::Transport)?;
        McpClient::connect(&spec.name, &spec.endpoint, headers, timeout)
    }

    /// Attach a mutual-TLS client identity (a mounted cert chain + key) for a
    /// `https://` endpoint. A no-op on non-TLS endpoints (the identity is only
    /// presented during the TLS handshake). RFC 0012 §3.7: the key never leaves
    /// the process (see [`crate::net::tls`]).
    #[cfg(feature = "tls")]
    pub fn with_identity(mut self, identity: crate::net::tls::ClientIdentity) -> Self {
        // The Arc is unshared here (called right after connect, before the event
        // thread), so get_mut succeeds; a no-op if it were somehow already shared.
        if let Some(h) = Arc::get_mut(&mut self.http) {
            h.set_identity(Some(identity));
        }
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.caps
    }

    /// Set the `_meta` stamped onto every `tools/call` (e.g. the run id, for
    /// retry dedup). Call after `initialize`. RFC 0011 §idempotency.
    pub fn set_tool_meta(&mut self, meta: Value) {
        self.tool_meta = Some(meta);
    }

    /// MCP lifecycle handshake: `initialize` → store capabilities →
    /// `notifications/initialized`. Uses the default per-request timeout.
    pub fn initialize(&mut self) -> Result<(), McpError> {
        self.initialize_within(self.timeout)
    }

    /// [`Self::initialize`] with a caller-supplied timeout for the `initialize`
    /// round-trip (the SHORT management bound, RFC 0016 §10). Used by the
    /// hot-reload re-handshake, which adds a server ON the reactor thread mid-loop:
    /// a slow-but-alive added server must not block the reactor (and starve the
    /// liveness heartbeat) for the full ~60s — a timeout is a contained
    /// `mcp.connect.fail` (the server is simply absent, RFC 0007 / RFC 0017 §5.3).
    pub fn initialize_within(&mut self, timeout: Duration) -> Result<(), McpError> {
        // Detect the server's ERA (versioning §backward-compatibility): attempt a
        // MODERN `server/discover`; a modern JSON-RPC error body identifies a
        // modern server (retry with a mutual version), otherwise fall back to the
        // legacy `initialize` handshake.
        match self.probe_modern(timeout)? {
            Probe::Modern(discover) => self.establish_modern(*discover),
            Probe::ModernRetry(supported) => {
                let v = best_mutual_version(&supported).ok_or_else(|| {
                    McpError::Transport(format!(
                        "server '{}' shares no MCP protocol version with agentd \
                         (server offered {supported:?}, agentd speaks {SUPPORTED_PROTOCOL_VERSIONS:?})",
                        self.name
                    ))
                })?;
                self.http.set_protocol_version(v.clone());
                self.protocol_version = Some(v);
                match self.probe_modern(timeout)? {
                    Probe::Modern(discover) => self.establish_modern(*discover),
                    _ => Err(McpError::Transport(format!(
                        "server '{}' rejected the negotiated MCP protocol version",
                        self.name
                    ))),
                }
            }
            Probe::Legacy => self.legacy_initialize(timeout),
        }
    }

    /// Probe the server with a MODERN `server/discover` and classify the era. The
    /// discover request carries the per-request `_meta` + routing headers; the raw
    /// transport outcome (a discover result, a modern JSON-RPC error, or anything
    /// else) tells us whether the server is modern or legacy.
    fn probe_modern(&mut self, timeout: Duration) -> Result<Probe, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let version = self
            .protocol_version
            .clone()
            .unwrap_or_else(|| LATEST_MODERN_VERSION.to_string());
        let mut params = json!({});
        modern::inject_client_meta(&mut params, &version, &client_info());
        self.http.set_protocol_version(version);
        let routing = modern::routing_headers(method::SERVER_DISCOVER, &params);
        let refs: Vec<(&str, &str)> = routing.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let req = json::Request::new(id, method::SERVER_DISCOVER, Some(params));
        let body = serde_json::to_vec(&req)
            .map_err(|e| McpError::Transport(format!("encode server/discover: {e}")))?;
        let notifications = &self.notifications;
        let outcome = self.http.send(Some(id), &body, timeout, &refs, |n| {
            queue_notification(notifications, n)
        });

        match outcome {
            // HTTP 2xx: a discover result, or a JSON-RPC error (a legacy server that
            // doesn't know server/discover answers with a generic error).
            Ok(Some(msg)) => {
                let resp: json::Response = serde_json::from_value(msg).map_err(|e| {
                    McpError::Transport(format!("bad server/discover reply on '{}': {e}", self.name))
                })?;
                if let Some(err) = resp.error {
                    return Ok(if is_modern_error_code(err.code) {
                        classify_modern_error(&err)
                    } else {
                        Probe::Legacy
                    });
                }
                let discover: DiscoverResult =
                    serde_json::from_value(resp.result.unwrap_or(Value::Null)).map_err(|e| {
                        McpError::Transport(format!(
                            "bad server/discover result on '{}': {e}",
                            self.name
                        ))
                    })?;
                Ok(Probe::Modern(Box::new(discover)))
            }
            Ok(None) => Ok(Probe::Legacy),
            // A non-2xx: a recognized MODERN error body ⇒ modern; else legacy.
            Err(HttpError::Status(_, body)) => Ok(rpc_error_from_body(&body)
                .filter(|e| is_modern_error_code(e.code))
                .map(|e| classify_modern_error(&e))
                .unwrap_or(Probe::Legacy)),
            // A connect/transport failure is not an era signal — propagate it.
            Err(e) => Err(http_err(&self.name, method::SERVER_DISCOVER, e)),
        }
    }

    /// Adopt the MODERN (stateless) era from a `server/discover` result: no
    /// session, no `notifications/initialized`; every subsequent request carries
    /// `_meta` + routing headers (see [`Self::request_with_timeout`]).
    fn establish_modern(&mut self, discover: DiscoverResult) -> Result<(), McpError> {
        self.era = Era::Modern;
        // The version we probed with was accepted (we got a result); prefer the
        // newest we share with the server's advertised list, else keep it.
        let version = best_mutual_version(&discover.supported_versions)
            .or_else(|| self.protocol_version.clone())
            .unwrap_or_else(|| LATEST_MODERN_VERSION.to_string());
        self.http.set_protocol_version(version.clone());
        self.protocol_version = Some(version);
        self.caps = discover.capabilities;
        Ok(())
    }

    /// The LEGACY `initialize` handshake (lifecycle §initialization): advertise our
    /// latest legacy version, adopt the server's negotiated version, store caps,
    /// send `notifications/initialized`.
    fn legacy_initialize(&mut self, timeout: Duration) -> Result<(), McpError> {
        self.era = Era::Legacy;
        // The modern probe set a version header; clear it so the initialize request
        // carries none (nothing negotiated yet — legacy sends the header only after).
        self.http.clear_protocol_version();
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: client_info(),
        };
        let result =
            self.request_with_timeout(method::INITIALIZE, Some(to_value(&params)), timeout)?;
        let init: InitializeResult = serde_json::from_value(result)
            .map_err(|e| McpError::Transport(format!("bad initialize result: {e}")))?;

        // Version negotiation (lifecycle §version-negotiation): the server echoes
        // our version if it supports it, else returns one it does. Adopt its choice
        // iff we can speak it; otherwise we cannot agree and must disconnect.
        let negotiated = negotiate_version(&init.protocol_version).ok_or_else(|| {
            McpError::Transport(format!(
                "server '{}' offered unsupported MCP protocol version '{}' \
                 (agentd speaks {:?})",
                self.name, init.protocol_version, SUPPORTED_PROTOCOL_VERSIONS
            ))
        })?;
        // Echo it on every subsequent request as MCP-Protocol-Version (a Streamable
        // HTTP MUST) — set BEFORE `notifications/initialized`, which is the first
        // request that must carry the header.
        self.http.set_protocol_version(negotiated.clone());
        self.protocol_version = Some(negotiated);

        self.caps = init.capabilities;
        self.notify(method::INITIALIZED, None)?;
        Ok(())
    }

    /// The protocol era established on connect (legacy handshake vs modern
    /// stateless). Governs how each request is built.
    pub fn era(&self) -> Era {
        self.era
    }

    /// The protocol version negotiated with the server (`None` before connect).
    /// Sent as `MCP-Protocol-Version` on every subsequent request.
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// `tools/list`, following cursor pagination to completion. Empty when the
    /// server doesn't advertise `tools`. Uses the default per-request timeout.
    pub fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        self.list_tools_within(self.timeout)
    }

    /// `tools/list` with a caller-supplied per-request timeout (the SHORT
    /// management bound, RFC 0016 §10) instead of the default ~60s. Used by the
    /// reactor-thread management path (hot-reload re-handshake, claim coordination
    /// re-validation) so a slow-but-alive coordination server cannot outrun the
    /// liveness heartbeat. A timeout surfaces as the usual [`McpError::Timeout`],
    /// which the callers already treat as a best-effort failure. The timeout is
    /// applied to EACH page (each pagination round-trip is bounded), matching the
    /// per-request contract of [`Self::request_with_timeout`].
    pub fn list_tools_within(&self, timeout: Duration) -> Result<Vec<Tool>, McpError> {
        if !self.caps.supports_tools() {
            return Ok(Vec::new());
        }
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let page: ListToolsResult =
                self.request_as_within(method::TOOLS_LIST, params, timeout)?;
            tools.extend(page.tools);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(tools)
    }

    /// `tools/call`. The returned [`CallToolResult`] carries `isError` (a
    /// tool-domain failure observation) — distinct from an `Err` here, which
    /// is a transport/protocol failure (RFC 0004 §isError).
    pub fn call_tool(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<CallToolResult, McpError> {
        if !self.caps.supports_tools() {
            return Err(McpError::Capability(format!(
                "server '{}' has no tools",
                self.name
            )));
        }
        let params = build_call_params(name, arguments, self.tool_meta.as_ref());
        self.request_as(method::TOOLS_CALL, Some(params))
    }

    /// `tools/call` with **per-call** `_meta` merged on top of the persistent
    /// [`Self::set_tool_meta`] for this one call only — without mutating the
    /// stored meta. Used by the work-claim client (RFC 0019 §3 / RFC 0015 §5.6),
    /// where `agent/claim_key` is per-item and must ride the individual call,
    /// never the persistent stamp. `extra_meta` (an object) wins key-by-key over
    /// the persistent meta; a non-object `extra_meta` replaces it. The persistent
    /// meta is left untouched.
    pub fn call_tool_with_meta(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Value,
    ) -> Result<CallToolResult, McpError> {
        self.call_tool_with_meta_within(name, arguments, extra_meta, self.timeout)
    }

    /// `tools/call` with per-call `_meta` AND a caller-supplied per-request
    /// timeout (the SHORT management bound, RFC 0016 §10) instead of the default
    /// ~60s. Used by the reactor-thread lease management path (claim
    /// renew/ack/release) — a slow coordination server must not block the reactor
    /// past the liveness staleness window. Behaviour is otherwise identical to
    /// [`Self::call_tool_with_meta`]; a timeout surfaces as [`McpError::Timeout`],
    /// which the lease callers already treat as a best-effort failure. The data
    /// path (subagent tool calls) never uses this — it keeps the default timeout.
    pub fn call_tool_with_meta_within(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        if !self.caps.supports_tools() {
            return Err(McpError::Capability(format!(
                "server '{}' has no tools",
                self.name
            )));
        }
        let merged = merge_meta(self.tool_meta.as_ref(), extra_meta);
        let params = build_call_params(name, arguments, Some(&merged));
        self.request_as_within(method::TOOLS_CALL, Some(params), timeout)
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, McpError> {
        if !self.caps.supports_resources() {
            return Ok(Vec::new());
        }
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let page: ListResourcesResult = self.request_as(method::RESOURCES_LIST, params)?;
            resources.extend(page.resources);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(resources)
    }

    pub fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        self.read_resource_within(uri, self.timeout)
    }

    /// `resources/read` with a caller-supplied per-request timeout (the SHORT
    /// management bound, RFC 0016 §10) instead of the default ~60s. The reactor
    /// thread's notify-then-read (`read_current`) blocks on this; a slow-but-alive
    /// resource server must not outrun the liveness heartbeat. A timeout surfaces
    /// as [`McpError::Timeout`]; the level-triggered reactor treats a timed-out
    /// read exactly like any read failure (act on empty / skip), so a transient
    /// slow read is recovered on the next `updated` notification or re-read.
    pub fn read_resource_within(
        &self,
        uri: &str,
        timeout: Duration,
    ) -> Result<ReadResourceResult, McpError> {
        let params = ReadResourceParams {
            uri: uri.to_string(),
        };
        self.request_as_within(method::RESOURCES_READ, Some(to_value(&params)), timeout)
    }

    /// `resources/subscribe` — gated on the server advertising it (RFC 0004).
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        self.subscribe_within(uri, self.timeout)
    }

    /// [`Self::subscribe`] with a caller-supplied timeout (the SHORT management
    /// bound, RFC 0016 §10) — for the reactor-thread reload re-handshake, where a
    /// slow-but-alive server arming a subscription must not block the reactor.
    pub fn subscribe_within(&self, uri: &str, timeout: Duration) -> Result<(), McpError> {
        if !self.caps.supports_subscribe() {
            return Err(McpError::Capability(format!(
                "server '{}' does not support resource subscriptions",
                self.name
            )));
        }
        if self.era == Era::Modern {
            // Modern: `resources/subscribe` is replaced by `subscriptions/listen` —
            // record the URI and (re)open the listen stream with the full filter.
            self.subscribed_uris
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(uri.to_string());
            self.restart_modern_listen();
            return Ok(());
        }
        // Legacy: register per-URI over `resources/subscribe`, then open the GET
        // notification stream (lazily; idempotent).
        self.request_with_timeout(
            method::RESOURCES_SUBSCRIBE,
            Some(to_value(&SubscribeParams { uri: uri.into() })),
            timeout,
        )?;
        self.ensure_event_stream();
        Ok(())
    }

    /// Start the background LEGACY notification `GET` SSE thread if it isn't
    /// running. Idempotent. Notifications land in the shared queue drained by
    /// [`Self::drain_notifications`]. If the server has no push channel the thread
    /// exits quietly and the client runs pull-only.
    fn ensure_event_stream(&self) {
        let mut guard = self.events.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let http = Arc::clone(&self.http);
        let queue = Arc::clone(&self.notifications);
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name(format!("mcp-events:{}", self.name))
            .spawn(move || event_loop(http, queue, stop_thread))
            .ok();
        if let Some(handle) = handle {
            *guard = Some(EventStreamHandle { stop, handle });
        }
    }

    /// (Re)open the MODERN `subscriptions/listen` stream with the current URI set.
    /// The filter is carried in one request, so adding/removing a URI restarts the
    /// stream. Stops any prior stream first; a no-op when nothing is subscribed.
    fn restart_modern_listen(&self) {
        self.stop_event_stream();
        let uris: Vec<String> = self
            .subscribed_uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        if uris.is_empty() {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let version = self
            .protocol_version
            .clone()
            .unwrap_or_else(|| LATEST_MODERN_VERSION.to_string());
        let mut params = json!({ "notifications": { "resourceSubscriptions": uris } });
        modern::inject_client_meta(&mut params, &version, &client_info());
        let routing: Vec<(String, String)> = modern::routing_headers(method::SUBSCRIPTIONS_LISTEN, &params)
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let req = json::Request::new(id, method::SUBSCRIPTIONS_LISTEN, Some(params));
        let body = match serde_json::to_vec(&req) {
            Ok(b) => b,
            Err(_) => return,
        };

        let stop = Arc::new(AtomicBool::new(false));
        let http = Arc::clone(&self.http);
        let queue = Arc::clone(&self.notifications);
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name(format!("mcp-listen:{}", self.name))
            .spawn(move || modern_listen_loop(http, queue, stop_thread, body, routing))
            .ok();
        if let Some(handle) = handle {
            *self.events.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(EventStreamHandle { stop, handle });
        }
    }

    /// Stop the background notification thread (either era) if one is running.
    fn stop_event_stream(&self) {
        if let Some(ev) = self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            ev.stop.store(true, Ordering::SeqCst);
            let _ = ev.handle.join();
        }
    }

    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        self.unsubscribe_within(uri, self.timeout)
    }

    /// [`Self::unsubscribe`] with a caller-supplied timeout (the SHORT management
    /// bound, RFC 0016 §10) — for the reactor-thread reload reconcile + the drain
    /// unsubscribe, both best-effort: a slow server here must not block the reactor
    /// or the drain past the liveness window / drain budget.
    pub fn unsubscribe_within(&self, uri: &str, timeout: Duration) -> Result<(), McpError> {
        if self.era == Era::Modern {
            // Modern: drop the URI and re-open `subscriptions/listen` with the rest
            // (or stop the stream if none remain).
            self.subscribed_uris
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(uri);
            self.restart_modern_listen();
            return Ok(());
        }
        self.request_with_timeout(
            method::RESOURCES_UNSUBSCRIBE,
            Some(to_value(&SubscribeParams { uri: uri.into() })),
            timeout,
        )?;
        Ok(())
    }

    /// Drain any notifications queued since the last drain (e.g.
    /// `notifications/resources/updated`). The reactive router
    /// (`triggers/mode.rs`) drains these between runs to drive re-reactions.
    pub fn drain_notifications(&self) -> Vec<json::Notification> {
        let mut q = self.notifications.lock().unwrap_or_else(|e| e.into_inner());
        q.drain(..).collect()
    }

    // ---- internals ----

    fn request_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<T, McpError> {
        self.request_as_within(method, params, self.timeout)
    }

    fn request_as_within<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<T, McpError> {
        let v = self.request_with_timeout(method, params, timeout)?;
        serde_json::from_value(v)
            .map_err(|e| McpError::Transport(format!("bad {method} result: {e}")))
    }

    /// Send one JSON-RPC request over a fresh HTTP connection and return the
    /// matching response (`timeout` is the socket connect+read bound). The
    /// default-timeout callers delegate here with `self.timeout`; the reactor-
    /// thread management path passes the SHORT bound (RFC 0016 §10) so a slow-but-
    /// alive server cannot block the reactor past the liveness window.
    fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // In the MODERN era, every request carries per-request `_meta` and the
        // Mcp-Method / Mcp-Name routing headers; legacy sends plain params.
        let (params, routing) = if self.era == Era::Modern {
            let mut p = params.unwrap_or_else(|| json!({}));
            let version = self.protocol_version.as_deref().unwrap_or(LATEST_MODERN_VERSION);
            modern::inject_client_meta(&mut p, version, &client_info());
            let routing = modern::routing_headers(method, &p);
            (Some(p), routing)
        } else {
            (params, Vec::new())
        };
        let refs: Vec<(&str, &str)> = routing.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let req = json::Request::new(id, method, params);
        let body = serde_json::to_vec(&req)
            .map_err(|e| McpError::Transport(format!("encode {method}: {e}")))?;
        let notifications = &self.notifications;
        let msg = self
            .http
            .send(Some(id), &body, timeout, &refs, |n| {
                queue_notification(notifications, n)
            })
            .map_err(|e| http_err(&self.name, method, e))?
            .ok_or_else(|| {
                McpError::Transport(format!("no response to {method} on '{}'", self.name))
            })?;
        let resp: json::Response = serde_json::from_value(msg).map_err(|e| {
            McpError::Transport(format!("bad {method} response on '{}': {e}", self.name))
        })?;
        match resp.error {
            Some(err) => Err(McpError::Rpc(err)),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }

    fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let note = json::Notification::new(method, params);
        let body = serde_json::to_vec(&note)
            .map_err(|e| McpError::Transport(format!("encode {method}: {e}")))?;
        let notifications = &self.notifications;
        self.http
            .send(None, &body, self.timeout, &[], |n| {
                queue_notification(notifications, n)
            })
            .map_err(|e| http_err(&self.name, method, e))?;
        Ok(())
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Stop the notification thread: set its stop flag; it wakes within
        // EVENT_READ_TIMEOUT (its read bound) and exits. The per-request
        // connections open+close themselves, so there is nothing else to reap.
        if let Some(ev) = self
            .events
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            ev.stop.store(true, Ordering::SeqCst);
            let _ = ev.handle.join();
        }
    }
}

/// The era-detection outcome of a modern `server/discover` probe.
enum Probe {
    /// The server is modern and returned its capabilities (boxed — much larger
    /// than the other variants).
    Modern(Box<DiscoverResult>),
    /// The server is modern but did not support our version — the versions it does
    /// support (from a `-32022` error); retry with a mutual one.
    ModernRetry(Vec<String>),
    /// The server is not modern — fall back to the legacy `initialize` handshake.
    Legacy,
}

/// agentd's client identity — sent in every request's `_meta` (modern) or in the
/// `initialize` handshake (legacy).
fn client_info() -> Implementation {
    Implementation {
        name: "agentd".into(),
        version: crate::VERSION.into(),
        title: None,
    }
}

/// Parse a JSON-RPC error object out of a raw HTTP error-response body (a modern
/// server returns one for an unsupported version / header mismatch).
fn rpc_error_from_body(body: &[u8]) -> Option<RpcError> {
    serde_json::from_slice::<json::Response>(body)
        .ok()
        .and_then(|r| r.error)
}

/// Classify a modern JSON-RPC error into a probe outcome: a `-32022`
/// unsupported-version error yields the server's `supported` list to retry with;
/// any other modern error (e.g. header mismatch) can't be proceeded with.
fn classify_modern_error(err: &RpcError) -> Probe {
    if err.code == UNSUPPORTED_PROTOCOL_VERSION_CODE {
        let supported = err
            .data
            .as_ref()
            .and_then(|d| serde_json::from_value::<UnsupportedProtocolVersion>(d.clone()).ok())
            .map(|u| u.supported)
            .unwrap_or_default();
        Probe::ModernRetry(supported)
    } else {
        // A header mismatch (-32020) is our own request bug against a modern server;
        // ModernRetry with no list ⇒ best_mutual returns None ⇒ a clear error.
        Probe::ModernRetry(Vec::new())
    }
}

/// Map a [`HttpError`] onto the client's error domain, folding socket timeouts
/// into [`McpError::Timeout`] so the management-timeout callers (which treat a
/// timeout as a best-effort failure) behave identically across the request path.
fn http_err(name: &str, method: &str, e: HttpError) -> McpError {
    use std::io::ErrorKind;
    match e {
        HttpError::Connect(io) | HttpError::Http(io) => match io.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => {
                McpError::Timeout(format!("{method} on '{name}'"))
            }
            _ => McpError::Transport(format!("{method} on '{name}': {io}")),
        },
        HttpError::Status(code, _) => {
            McpError::Transport(format!("{method} on '{name}': server returned HTTP {code}"))
        }
        HttpError::Unsupported(m) => McpError::Transport(m),
        HttpError::NoResponse => {
            McpError::Transport(format!("{method} on '{name}': no JSON-RPC response"))
        }
    }
}

/// Queue a raw notification Value captured off an HTTP response or the GET SSE
/// stream (a JSON-RPC message with no matching request id). Non-notification
/// frames (e.g. a server→client request) that don't deserialize are dropped — v1
/// declares no client capabilities, so there is nothing to answer.
fn queue_notification(queue: &Mutex<VecDeque<json::Notification>>, n: Value) {
    if let Ok(note) = serde_json::from_value::<json::Notification>(n) {
        queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(note);
    }
}

/// The notification `GET` SSE thread: (re)open the server→client stream and pump
/// its JSON-RPC notifications into the shared queue until `stop`. Reconnects on a
/// transient drop; gives up if the server has no push channel (a non-2xx / non-SSE
/// response), leaving the client pull-only.
fn event_loop(http: Arc<HttpTransport>, queue: NotifQueue, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let mut sse = match http.open_events(EVENT_READ_TIMEOUT) {
            Ok(s) => s,
            // No usable push channel — stop trying (don't spin).
            Err(HttpError::Status(_, _)) | Err(HttpError::Unsupported(_)) => return,
            // Transient (connect/HTTP) — back off, then retry unless stopping.
            Err(_) => {
                for _ in 0..20 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                continue;
            }
        };
        pump_events(&mut sse, &queue, &stop);
    }
}

/// The MODERN notification thread: (re)open the `subscriptions/listen` POST stream
/// with the pre-built request `body` + routing headers, and pump its notifications
/// (the acknowledgment + the opted-in change notifications) into the queue until
/// `stop`. Reconnects on a transient drop; gives up if the server rejects the
/// listen (non-2xx / non-SSE), leaving the client pull-only.
fn modern_listen_loop(
    http: Arc<HttpTransport>,
    queue: NotifQueue,
    stop: Arc<AtomicBool>,
    body: Vec<u8>,
    routing: Vec<(String, String)>,
) {
    while !stop.load(Ordering::Relaxed) {
        let refs: Vec<(&str, &str)> = routing.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut sse = match http.open_listen(EVENT_READ_TIMEOUT, &body, &refs) {
            Ok(s) => s,
            Err(HttpError::Status(_, _)) | Err(HttpError::Unsupported(_)) => return,
            Err(_) => {
                for _ in 0..20 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                continue;
            }
        };
        pump_events(&mut sse, &queue, &stop);
    }
}

/// Read events off one SSE stream into `queue` until EOF/error or `stop`.
fn pump_events(sse: &mut EventStream, queue: &NotifQueue, stop: &AtomicBool) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match sse.next_event() {
            Ok(Some(ev)) => {
                if let Ok(note) = serde_json::from_str::<json::Notification>(&ev.data) {
                    queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back(note);
                }
            }
            Ok(None) => return, // clean EOF — reconnect
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Idle read timeout — loop to re-check the stop flag.
                continue;
            }
            Err(_) => return, // stream error — reconnect
        }
    }
}

/// Build `tools/call` params — `{name, arguments?, _meta?}` — injecting the
/// client's `_meta` (run id, etc.) for retry dedup. Pure. RFC 0011 §idempotency.
fn build_call_params(name: &str, arguments: Option<Value>, meta: Option<&Value>) -> Value {
    let mut p = serde_json::Map::new();
    p.insert("name".into(), Value::String(name.to_string()));
    if let Some(args) = arguments {
        p.insert("arguments".into(), args);
    }
    if let Some(m) = meta {
        p.insert("_meta".into(), m.clone());
    }
    Value::Object(p)
}

fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Merge `extra` over the persistent `base` meta for a single call, without
/// mutating either. When both are objects, `extra` wins key-by-key (a shallow
/// merge — the claim contract's keys are flat); a non-object `extra` replaces
/// `base` wholesale; a `None`/non-object `base` yields `extra`. Pure.
fn merge_meta(base: Option<&Value>, extra: Value) -> Value {
    match (base, &extra) {
        (Some(Value::Object(b)), Value::Object(e)) => {
            let mut m = b.clone();
            for (k, v) in e {
                m.insert(k.clone(), v.clone());
            }
            Value::Object(m)
        }
        _ => extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn call_params_inject_meta_for_idempotency() {
        let meta = json!({"agent/run_id": "r1"});
        let p = build_call_params("send_email", Some(json!({"to": "x"})), Some(&meta));
        assert_eq!(p["name"], "send_email");
        assert_eq!(p["arguments"]["to"], "x");
        assert_eq!(p["_meta"]["agent/run_id"], "r1");

        // no meta / no args → those keys are absent
        let p2 = build_call_params("noop", None, None);
        assert_eq!(p2["name"], "noop");
        assert!(p2.get("_meta").is_none());
        assert!(p2.get("arguments").is_none());
    }

    #[test]
    fn merge_meta_overlays_extra_without_mutating_base() {
        // Per-call claim_key rides on top of the persistent run_id stamp.
        let base = json!({"agent/run_id": "r1", "traceparent": "tp"});
        let merged = merge_meta(
            Some(&base),
            json!({"agent/claim_key": "ck", "traceparent": "tp2"}),
        );
        assert_eq!(merged["agent/run_id"], "r1"); // persistent key preserved
        assert_eq!(merged["agent/claim_key"], "ck"); // per-call key added
        assert_eq!(merged["traceparent"], "tp2"); // extra wins on conflict
        // The base is untouched.
        assert_eq!(base["traceparent"], "tp");
        // No persistent base → the extra is the meta.
        let only = merge_meta(None, json!({"agent/claim_key": "ck"}));
        assert_eq!(only["agent/claim_key"], "ck");
    }

    #[test]
    fn error_display() {
        let e = McpError::Timeout("tools/call on 'fs'".into());
        assert!(e.to_string().contains("timeout"));
    }

    #[test]
    fn http_err_folds_socket_timeout_into_timeout_variant() {
        use std::io::{Error, ErrorKind};
        let e = http_err(
            "fs",
            "tools/call",
            HttpError::Http(Error::new(ErrorKind::WouldBlock, "read timed out")),
        );
        assert!(matches!(e, McpError::Timeout(_)), "got {e:?}");
        // A non-2xx HTTP status is a transport error, not a timeout.
        let e = http_err("fs", "initialize", HttpError::Status(503, Vec::new()));
        assert!(matches!(e, McpError::Transport(_)), "got {e:?}");
    }

    #[test]
    fn queue_notification_enqueues_notifications_and_drops_others() {
        let q = Mutex::new(VecDeque::new());
        // A real notification is queued.
        queue_notification(
            &q,
            json!({"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"x"}}),
        );
        // A response frame (has id, no method) is not a notification → dropped.
        queue_notification(&q, json!({"jsonrpc":"2.0","id":1,"result":{}}));
        let drained: Vec<_> = q.lock().unwrap().drain(..).collect();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].method, "notifications/resources/updated");
    }

    #[test]
    fn connect_rejects_a_bad_endpoint() {
        // McpClient isn't Debug, so match the Result rather than unwrap_err().
        match McpClient::connect("bad", "ftp://nope/", Vec::new(), Duration::from_secs(1)) {
            Err(McpError::Transport(_)) => {}
            Err(other) => panic!("expected a Transport error, got {other:?}"),
            Ok(_) => panic!("expected connect to reject an unsupported scheme"),
        }
    }

    /// A unix listener that ACCEPTS a connection but never replies — an alive-but-
    /// silent server, to prove the per-request timeout governs (not a hang).
    fn spawn_silent_server() -> (String, std::thread::JoinHandle<()>) {
        let path = std::env::temp_dir().join(format!(
            "agentd-mcp-silent-{}-{}.sock",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind silent server");
        let handle = std::thread::spawn(move || {
            // Accept connections and hold them open, reading forever (never reply).
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 256];
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                    }
                });
            }
        });
        (format!("unix:{}", path.display()), handle)
    }

    #[test]
    fn management_timeout_bounds_a_call_on_a_silent_server() {
        // The server accepts but never replies; a request with the SHORT management
        // bound must return a Timeout fast — the per-call timeout, not a hang.
        let (endpoint, _srv) = spawn_silent_server();
        let client = McpClient::connect("silent", &endpoint, Vec::new(), Duration::from_secs(60))
            .expect("connect");

        let short = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let r = client.request_with_timeout("ping", None, short);
        let elapsed = started.elapsed();
        assert!(
            matches!(r, Err(McpError::Timeout(_))),
            "expected a Timeout within the short bound, got {r:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the short per-call timeout must govern (took {elapsed:?})"
        );
    }

    #[test]
    fn write_read_smoke_for_unix_stream() {
        // Guard that the test transport helpers are wired (a trivial round-trip),
        // so a future refactor of spawn_silent_server fails loudly here.
        let path = std::env::temp_dir().join(format!("agentd-smoke-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let p2 = path.clone();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.write_all(b"hi");
        });
        let mut c = UnixStream::connect(&p2).unwrap();
        let mut buf = [0u8; 2];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
        h.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn management_timeout_const_is_under_the_liveness_window() {
        // The pinned invariant (RFC 0016 §10): the reactor-thread management-call
        // timeout MUST stay under the `/healthz` + health-file liveness staleness
        // window, or a single management call could itself age the heartbeat past
        // the threshold — the starvation the short bound exists to prevent. The
        // authority is the compile-time `const _: () = assert!(...)` in `obs::health`
        // (it fails the BUILD if the invariant is broken); this test documents the
        // relationship where a reader of the client sees it, via the runtime
        // `Duration` accessor (not a const-foldable comparison).
        let mgmt = crate::obs::health::management_timeout();
        let window = Duration::from_millis(crate::obs::health::LIVENESS_STALE_AFTER_MS);
        assert!(
            mgmt < window,
            "management timeout {mgmt:?} must be under the liveness window {window:?}"
        );
        assert_eq!(
            mgmt,
            Duration::from_millis(crate::obs::health::MANAGEMENT_TIMEOUT_MS),
        );
    }
}
