// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP client agentd's runtime holds: one remote server over **Streamable
//! HTTP**, reached over a socket or not at all — no local process is spawned.
//!
//! [`McpClient`] is a connection *builder* until [`McpClient::initialize`]: it
//! collects the endpoint, the caller's headers, a request signer, an mTLS
//! identity and the elicitation handler, then hands them to the official SDK
//! ([`crate::rmcp_client`]), which starts the connection and answers every
//! operation after it over agentd's own socket ([`crate::http`]).
//!
//! **Starting a connection.** A 2026-07-28 server is reached with
//! `server/discover` alone — no handshake, no session; an older one with the
//! 2025-11-25 `initialize` handshake. [`McpClient::start_with`] says which to
//! send first, and either way a server of the other era is still reached: a
//! refusal that shows it speaks the other one leads to the other request, once.
//! [`McpClient::initialize`] keeps its name for whichever it turns out to be.

pub use crate::rmcp_client::FirstRequest;

use crate::http::{HttpTransport, McpEndpoint};
use crate::inbound;
use crate::rpc;
use crate::wire::{
    CallToolResult, CompleteResult, GetPromptResult, Implementation, Prompt, ReadResourceResult,
    Resource, ResourceTemplate, ServerCapabilities, Tool,
};
use serde_json::Value;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum McpError {
    Transport(String),
    /// The server doesn't advertise the capability the call needs.
    Capability(String),
    /// The server no longer knows this connection's session (Streamable
    /// HTTP's `404`): every call on it fails, and every subscription it held
    /// is gone. [`McpClient::redial_within`] gets a working connection back.
    SessionExpired(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Transport(m) => write!(f, "mcp: transport: {m}"),
            McpError::Capability(m) => write!(f, "mcp: capability: {m}"),
            McpError::SessionExpired(m) => write!(f, "mcp: session expired: {m}"),
        }
    }
}

/// The first wait before re-dialing again after a failed re-dial; doubled on
/// each failure up to [`REDIAL_MAX`]. A server that lost the session because
/// it is restarting may refuse a few handshakes before it is back.
pub const REDIAL_MIN: Duration = Duration::from_secs(1);
/// The longest wait between re-dial attempts.
pub const REDIAL_MAX: Duration = Duration::from_secs(30);

/// When a lost connection may be re-dialed next: after how many failed
/// attempts, and not before when.
#[derive(Default)]
struct Redial {
    failures: u32,
    /// How many re-dials in a row replaced a session that had lived less
    /// than [`REDIAL_MAX`]. A handshake that succeeds proves only that the
    /// server answers `initialize`: one that loses each new session at once
    /// (crash-looping after the handshake, or answering the notification
    /// stream's `GET` with `404` where the spec says `405`) would otherwise
    /// be re-dialed about once a second for ever, each time re-subscribing
    /// everything and leaving a session behind on the server.
    flaps: u32,
    /// When the live connection's session was handshaken.
    dialed: Option<Instant>,
    not_before: Option<Instant>,
}

/// The wait before the next attempt after `n` reasons to slow down:
/// [`REDIAL_MIN`] doubled per reason, up to [`REDIAL_MAX`]. Public so a host
/// retrying what a re-dial restores keeps the same schedule.
pub fn redial_backoff(n: u32) -> Duration {
    REDIAL_MIN.saturating_mul(1u32 << n.min(5)).min(REDIAL_MAX)
}

impl std::error::Error for McpError {}

/// How far a failed `tools/call` got: whether the server may have acted on
/// it, and whether anything in its error is the server's. The transport says
/// it — beside the error, never inside text the server could have written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// It never left: no connection was made, this build cannot speak the
    /// endpoint's scheme, or the server's own wait (`Retry-After`) was still
    /// running. Nothing in the error is the server's.
    Unsent,
    /// It never left: the request signer produced no credential. The error
    /// carries the signer's cause.
    Unsigned,
    /// It was sent and no answer came — silence past the timeout, a dropped
    /// connection, a stream that ended first: the server may have acted on it.
    Unanswered,
    /// The server answered with an error — a JSON-RPC error, or an HTTP
    /// status without one — and this HTTP status.
    Answered { status: u16 },
    /// The server answered with a result this client could not read: it
    /// acted on the request.
    Unreadable,
}

/// A `tools/call` that failed, and how far it got.
#[derive(Debug)]
pub struct CallFailed {
    pub error: McpError,
    pub reach: Reach,
}

/// A connected (and, after [`McpClient::initialize`], handshaken) remote MCP
/// server over Streamable HTTP.
pub struct McpClient {
    name: String,
    /// The socket the first handshake runs on, and the one a re-dial copies
    /// its endpoint, headers, signer and identity from.
    http: Arc<HttpTransport>,
    timeout: Duration,
    /// The official SDK, which answers every operation. `None` before
    /// `initialize`; this type is a connection *builder* until then, and every
    /// operation on an unconnected client is a transport error rather than a
    /// panic. Swapped whole by a re-dial, behind a lock held only to clone the
    /// handle — so everyone holding this client reaches the new connection,
    /// and a call in flight finishes on the one it started on. The negotiated
    /// capabilities and revision are the connection's, read through it: a
    /// server that restarted may answer a re-dial with different ones.
    rmcp: Mutex<Option<Arc<crate::rmcp_client::RmcpClient>>>,
    /// The re-dial schedule after a lost session.
    redial: Mutex<Redial>,
    /// What the SDK needs to build its side of the connection. The socket
    /// itself is `http` above — that is how a request signer and an mTLS
    /// identity survive the SDK owning the protocol, and it is also the ONE
    /// place the caller's headers live: handing them to the SDK as well put
    /// each of them, `Authorization` included, on every request twice.
    endpoint: String,
    /// The host callback that answers a server's `elicitation/create`. Present
    /// iff the `elicitation` client capability is declared, so a server only
    /// asks what the host can actually deliver to a human.
    elicitation: Option<Arc<dyn inbound::Handler>>,
    /// What an extension adds to the connection's listen; see
    /// [`Self::with_listen_extension`]. Kept, so a re-dial adds it too.
    listen_extension: Option<crate::rmcp_client::ListenExtensionFn>,
    /// Stamped into every `tools/call` request's `params._meta` (e.g.
    /// `{"agent/run_id": …}`) so a backing service can recognize a retried call
    /// as the same logical operation and dedupe it rather than repeating a side
    /// effect. The one copy of it: every call path merges from here, so no path
    /// can send a call without it.
    tool_meta: Option<Value>,
    /// The client identity sent to the server. Defaults to this crate's
    /// identity; the host overrides it via [`Self::with_client_info`] (agentd
    /// sets its own name + version).
    client_info: Implementation,
    /// What the first connection sends first ([`Self::start_with`]). A
    /// re-dial starts the way the connection it replaces was reached.
    first: FirstRequest,
    /// How long a listen stream may stay silent before it is cut and opened
    /// again; see [`crate::rmcp_client::LISTEN_IDLE`].
    listen_idle: Duration,
    /// Declare the MCP Tasks extension where the revision defines it
    /// ([`Self::with_tasks`]). Kept, so a re-dial declares it too.
    tasks: bool,
    /// Extensions declared on every request ([`Self::with_extension`]).
    /// Kept, so a re-dial declares them too.
    extensions: Vec<String>,
}

impl McpClient {
    /// Connect to a remote MCP server over Streamable HTTP. `endpoint` is
    /// `https://…` or `http://…`. `headers` are caller-owned request headers
    /// (auth/framing — already-resolved secret values, never templates, and
    /// never logged). No process is spawned. Call [`Self::initialize`] before
    /// any tool/resource call.
    pub fn connect(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<McpClient, McpError> {
        Self::connect_signed(name, endpoint, headers, timeout, None)
    }

    /// [`Self::connect`] with an optional per-request signer — every outbound
    /// request to this server is then signed, including the long-lived
    /// notification stream, and a request whose credential the signer cannot
    /// produce is not sent. `None` = unsigned (the `connect` default).
    pub fn connect_signed(
        name: &str,
        endpoint: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
        signer: Option<Arc<dyn crate::http::RequestSigner>>,
    ) -> Result<McpClient, McpError> {
        let ep = McpEndpoint::parse(endpoint)
            .map_err(|e| McpError::Transport(format!("mcp server '{name}': {e}")))?;
        Ok(McpClient {
            name: name.to_string(),
            http: Arc::new(HttpTransport::new(ep, headers).with_signer(signer)),
            timeout,
            rmcp: Mutex::new(None),
            redial: Mutex::default(),
            endpoint: endpoint.to_string(),
            elicitation: None,
            listen_extension: None,
            tool_meta: None,
            client_info: Implementation {
                name: "agentd".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
            },
            first: FirstRequest::Discover,
            listen_idle: crate::rmcp_client::LISTEN_IDLE,
            tasks: false,
            extensions: Vec::new(),
        })
    }

    /// Declare the extension `id` with no settings — in `initialize`'s
    /// `capabilities.extensions` at 2025-11-25, and in every request's
    /// `_meta` client capabilities from 2026-07-28 — whatever the server
    /// says it supports. Enterprise-Managed Authorization is declared this
    /// way (`io.modelcontextprotocol/enterprise-managed-authorization`).
    pub fn with_extension(mut self, id: &str) -> Self {
        if !self.extensions.iter().any(|e| e == id) {
            self.extensions.push(id.to_string());
        }
        self
    }

    /// Declare the MCP Tasks extension (SEP-2663) on the requests that may
    /// create or follow a task, at a revision that defines it (2026-07-28
    /// on). A server may then answer a bounded `tools/call` with a task:
    /// [`Self::call_tool_with_meta_within`] follows it to its end inside the
    /// call's bound and cancels it there, and [`Self::call_tool_called`]
    /// hands it back for a host that follows it itself. A call with no bound
    /// never declares it: nothing could end its follow. Off (the default),
    /// no request declares it.
    pub fn with_tasks(mut self, on: bool) -> Self {
        self.tasks = on;
        self
    }

    /// Let a listen stream stay silent for `idle` (never less than the
    /// connection's timeout) before it is cut and opened again.
    pub fn with_listen_idle(mut self, idle: Duration) -> Self {
        self.listen_idle = idle;
        self
    }

    /// Start the connection with `first` — `server/discover` (MCP 2026-07-28)
    /// or `initialize` (2025-11-25). A server of the other era is reached
    /// either way; this only decides which request it sees first.
    pub fn start_with(mut self, first: FirstRequest) -> Self {
        self.first = first;
        self
    }

    /// Override the client identity sent to servers (name + version). agentd sets
    /// its own; other hosts of the `mcp` crate set theirs.
    pub fn with_client_info(mut self, info: Implementation) -> Self {
        self.client_info = info;
        self
    }

    /// Answer server→client **elicitation** requests through `handler`: a server
    /// may ask the operator a question mid-call and get a typed answer back.
    /// Declares the `elicitation` client capability, so a server only asks when
    /// we can actually deliver the question to a human.
    pub fn with_elicitation(mut self, handler: Arc<dyn inbound::Handler>) -> Self {
        self.elicitation = Some(handler);
        self
    }

    /// Let an extension add to the connection's `subscriptions/listen`; see
    /// [`crate::rmcp_client::RmcpBuilder::with_listen_extension`].
    pub fn with_listen_extension(mut self, decide: crate::rmcp_client::ListenExtensionFn) -> Self {
        self.listen_extension = Some(decide);
        self
    }

    /// Stamp `meta`'s keys into the `params._meta` of every request this
    /// connection sends — every kind, the SDK's own and a re-dial's included —
    /// unless the request carries one of them already
    /// ([`crate::http::RequestMeta`]). Call right after connect, as
    /// [`Self::with_identity`].
    pub fn with_request_meta(mut self, meta: crate::http::RequestMeta) -> Self {
        if let Some(h) = Arc::get_mut(&mut self.http) {
            h.set_request_meta(Some(meta));
        }
        self
    }

    /// Dial this server through the SSRF guard: resolved once, every address
    /// classified, only a vetted one connected — loopback, private and
    /// link-local refused unless `allow_private`. For a server whose URL
    /// someone other than the operator chose. Call right after connect, as
    /// [`Self::with_identity`].
    pub fn with_vetted_dial(mut self, allow_private: bool) -> Self {
        if let Some(h) = Arc::get_mut(&mut self.http) {
            h.set_vetted(allow_private);
        }
        self
    }

    /// Attach a mutual-TLS client identity (a mounted cert chain + key) for a
    /// `https://` endpoint. A no-op on non-TLS endpoints (the identity is only
    /// presented during the TLS handshake). The private key is read from a
    /// mounted file and never leaves the process — it is not logged, rendered
    /// into an error, or copied onto the wire (see [`net::tls`]).
    #[cfg(feature = "tls")]
    pub fn with_identity(mut self, identity: net::tls::ClientIdentity) -> Self {
        // The Arc is unshared here (called right after connect, before the SDK
        // holds the socket), so get_mut succeeds; a no-op if it were somehow
        // already shared.
        if let Some(h) = Arc::get_mut(&mut self.http) {
            h.set_identity(Some(identity));
        }
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    /// What the server advertised on the live connection; nothing before
    /// `initialize`.
    pub fn capabilities(&self) -> ServerCapabilities {
        self.sdk()
            .map(|c| c.capabilities().clone())
            .unwrap_or_default()
    }

    /// The member `name` of the capabilities the server advertised on the live
    /// connection, exactly as it sent it — including capabilities this crate
    /// does not model (an extension's, for one). `None` before `initialize`,
    /// and when the server did not advertise it.
    pub fn capability(&self, name: &str) -> Option<Value> {
        self.sdk().ok().and_then(|c| c.capability(name))
    }

    /// Tools the server listed that the SDK dropped (an `x-mcp-header`
    /// annotation it rejects, at 2026-07-28), each named once per connection,
    /// for the host to log: otherwise a tool disappears without a word.
    pub fn drain_dropped_tools(&self) -> Vec<String> {
        self.sdk()
            .map(|c| c.drain_dropped_tools())
            .unwrap_or_default()
    }

    /// What reached the server on the live connection:
    /// [`FirstRequest::Discover`] for a 2026-07-28 connection,
    /// [`FirstRequest::Initialize`] for one reached through the handshake,
    /// first or by fallback. `None` before `initialize`.
    pub fn first_request(&self) -> Option<FirstRequest> {
        self.sdk().ok().map(|c| c.first_request())
    }

    /// Set the `_meta` stamped onto every `tools/call` (e.g. the run id), so a
    /// server can recognize a retried call and dedupe its side effect. Call
    /// after `initialize`.
    pub fn set_tool_meta(&mut self, meta: Value) {
        self.tool_meta = Some(meta);
    }

    /// Start the connection: `server/discover` or the `initialize` handshake,
    /// as [`Self::start_with`] says, then the server's capabilities are
    /// stored. Uses the default per-request timeout.
    pub fn initialize(&mut self) -> Result<(), McpError> {
        self.initialize_within(self.timeout)
    }

    /// [`Self::initialize`] with a caller-supplied per-request timeout for
    /// the startup — for a host that must not wait the full default bound on
    /// a slow-but-alive server. A timeout is the caller's to contain.
    pub fn initialize_within(&mut self, timeout: Duration) -> Result<(), McpError> {
        let c = self.dial(Arc::clone(&self.http), timeout, None, self.first)?;
        *self.rmcp.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(c));
        self.redial
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .dialed = Some(Instant::now());
        Ok(())
    }

    /// One connection start on `http`, beginning with `first`.
    fn dial(
        &self,
        http: Arc<HttpTransport>,
        timeout: Duration,
        handshake_bound: Option<Duration>,
        first: FirstRequest,
    ) -> Result<crate::rmcp_client::RmcpClient, McpError> {
        // The SDK owns the handshake and every operation after it — over *this*
        // connection's transport, so a request signer (AAuth's challenge loop,
        // AWS SigV4) and an mTLS client identity still apply. Adopting the SDK
        // cost neither of them.
        // No headers for the SDK: the socket already carries the caller's, and
        // the SDK's own come on top of them per request.
        let mut b =
            crate::rmcp_client::RmcpBuilder::new(&self.name, &self.endpoint, Vec::new(), timeout)
                .with_http(http)
                .with_client_info(self.client_info.clone())
                .start_with(first)
                .with_listen_idle(self.listen_idle)
                .with_tasks(self.tasks)
                .with_extensions(&self.extensions);
        if let Some(h) = &self.elicitation {
            b = b.with_elicitation(Arc::clone(h));
        }
        if let Some(decide) = &self.listen_extension {
            b = b.with_listen_extension(Arc::clone(decide));
        }
        if let Some(bound) = handshake_bound {
            b = b.with_handshake_bound(bound);
        }
        b.connect()
    }

    /// Has the server forgotten the live connection's session? Then every call
    /// on it fails with [`McpError::SessionExpired`], its subscriptions are
    /// gone, and the host re-dials ([`Self::redial_within`]).
    pub fn session_lost(&self) -> bool {
        self.sdk().is_ok_and(|c| c.session_lost())
    }

    /// Is a re-dial of a lost session due — not inside the wait a failed one
    /// set? The host polls this; the schedule lives here so every host gets
    /// the same backoff.
    pub fn redial_due(&self) -> bool {
        self.session_lost()
            && self
                .redial
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .not_before
                .is_none_or(|t| Instant::now() >= t)
    }

    /// How many re-dials of the current loss have failed so far — 0 when the
    /// next attempt is the first, which is when a host reports the loss.
    pub fn redial_failures(&self) -> u32 {
        self.redial
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
    }

    /// Replace a connection whose session the server forgot with a fresh
    /// handshake on a fresh socket, and return the URIs the lost session was
    /// subscribed to. None of them is subscribed on the new connection: the
    /// host subscribes each again, so the new set holds exactly what the
    /// server accepted this time.
    ///
    /// `bound` caps the handshake alone (the host's management bound when it
    /// re-dials from a thread that must not stall); later calls keep the
    /// connection's own timeout. On failure the lost connection stays in place
    /// — its URIs with it, for the next attempt — and the next attempt is due
    /// after a wait that doubles per failure, [`REDIAL_MIN`] to [`REDIAL_MAX`].
    /// A success that replaced a session younger than [`REDIAL_MAX`] keeps
    /// doubling the wait before the next one, so a server that loses every
    /// new session at once settles to a re-dial every [`REDIAL_MAX`].
    ///
    /// What the lost connection received and the host had not drained yet
    /// moves to the new one, so the drain after the re-dial still sees it.
    pub fn redial_within(&self, bound: Duration) -> Result<Vec<String>, McpError> {
        let lost = self.sdk()?;
        // Started the way the lost connection was reached. Only a handshake
        // connection has a session to lose, so this is `initialize`: two
        // POSTs, inside the bound, as before discover existed — and a server
        // that now refuses it is re-probed with `server/discover`.
        let fresh = match self.dial(
            Arc::new(self.http.redial_copy()),
            self.timeout,
            Some(bound),
            lost.first_request(),
        ) {
            Ok(c) => c,
            Err(e) => {
                let mut r = self.redial.lock().unwrap_or_else(|p| p.into_inner());
                let wait = redial_backoff(r.failures + r.flaps);
                r.failures += 1;
                r.not_before = Some(Instant::now() + wait);
                return Err(e);
            }
        };
        {
            let mut r = self.redial.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let short_lived = r.dialed.is_some_and(|t| now.duration_since(t) < REDIAL_MAX);
            r.flaps = if short_lived { r.flaps + 1 } else { 0 };
            r.failures = 0;
            r.dialed = Some(now);
            r.not_before = (r.flaps > 0).then(|| now + redial_backoff(r.flaps - 1));
        }
        let fresh = Arc::new(fresh);
        *self.rmcp.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&fresh));
        // After the swap, so nothing the lost connection queued up to it is
        // left behind on a client no one drains again.
        fresh.inherit_queues(&lost);
        Ok(lost.subscribed())
    }

    /// The protocol version negotiated on the live connection (`None` before
    /// connect).
    pub fn protocol_version(&self) -> Option<String> {
        self.sdk()
            .ok()
            .and_then(|c| c.protocol_version().map(str::to_string))
    }

    /// The SDK connection, or the error every operation on an unconnected
    /// client reports.
    fn sdk(&self) -> Result<Arc<crate::rmcp_client::RmcpClient>, McpError> {
        self.rmcp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| McpError::Transport("the MCP connection is not established".into()))
    }

    /// `tools/list`, following cursor pagination to completion.
    pub fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        self.sdk()?.list_tools()
    }

    /// `tools/call`. The returned [`CallToolResult`] carries `isError` (a
    /// tool-domain failure the model sees as an observation) — distinct from an
    /// `Err` here, which is a transport/protocol failure and fails the call.
    /// Carries the persistent [`Self::set_tool_meta`].
    ///
    /// No bound on the call as a whole: the connection's timeout caps each
    /// silence on the socket, so a dead server still fails, but a call that is
    /// alive is left to finish. A tool may run long while it streams progress,
    /// and a server may stop mid-call to ask the operator a question
    /// (elicitation), whose answer takes a human's time, not a transport's.
    /// A caller with a deadline of its own says so through
    /// [`Self::call_tool_with_meta_within`]. Unbounded, it never declares the
    /// MCP Tasks extension, so the server answers inline.
    pub fn call_tool(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<CallToolResult, McpError> {
        self.call(name, arguments, None, None)
    }

    /// `tools/call` with **per-call** `_meta` merged on top of the persistent
    /// [`Self::set_tool_meta`] for this one call only — without mutating the
    /// stored meta. Used by the work-claim client, where `agent/claim_key`
    /// identifies one work item and must ride only that call — stamping it
    /// persistently would attach one item's key to every later call.
    /// The keys of `extra_meta` (an object) win key-by-key over the persistent
    /// meta; `_meta` is an object on the wire, so a non-object `extra_meta`
    /// adds nothing. The persistent meta is left untouched. Unbounded as a
    /// whole, exactly like [`Self::call_tool`].
    pub fn call_tool_with_meta(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Value,
    ) -> Result<CallToolResult, McpError> {
        self.call(name, arguments, Some(extra_meta), None)
    }

    /// `tools/call` with per-call `_meta` AND a caller-supplied bound on the
    /// whole call — the step timeout of a workflow step, the SHORT management
    /// bound of the reactor's lease path. A tool that has not answered inside
    /// `timeout` is a transport error then, not when the connection's own
    /// default runs out, so a silent server cannot hold the caller past the
    /// deadline it derived the bound from. The connection's timeout still caps
    /// each silence on the socket, so a bound longer than it does not extend
    /// that.
    ///
    /// Only a bound derived from a deadline belongs here. Everything under it
    /// counts against it — an operator answering an elicitation included —
    /// so a fixed default passed as a bound cuts off live calls: a slow tool,
    /// and a server whose own `timeout` was set longer than the default.
    ///
    /// On a connection that declares the MCP Tasks extension, the call does:
    /// a task it becomes is followed here to its end, and cancelled with
    /// `tasks/cancel` if it is still open at the bound.
    pub fn call_tool_with_meta_within(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        self.call(name, arguments, Some(extra_meta), Some(timeout))
    }

    /// [`Self::call_tool_with_meta`], saying of a failure how far the call
    /// got ([`Reach`]): for a caller that must know whether the server may
    /// have acted on it, and whether its error holds the server's words.
    pub fn call_tool_reaching(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Value,
    ) -> Result<CallToolResult, CallFailed> {
        self.call_reaching(name, arguments, Some(extra_meta), None)
    }

    /// Every `tools/call` path lands here, so the persistent meta rides each
    /// one — the subagent loop's plain `call_tool` included.
    fn call(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Option<Value>,
        bound: Option<Duration>,
    ) -> Result<CallToolResult, McpError> {
        self.call_reaching(name, arguments, extra_meta, bound)
            .map_err(|f| f.error)
    }

    /// Only a bounded call declares the MCP Tasks extension: a task it
    /// becomes is followed here, to its end or the call's bound
    /// ([`Self::follow_task`]). Its outcome is the call's, and a task still
    /// open at the bound is cancelled and reported as a call that may have
    /// acted.
    fn call_reaching(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Option<Value>,
        bound: Option<Duration>,
    ) -> Result<CallToolResult, CallFailed> {
        let started = Instant::now();
        let called =
            self.call_tool_called(name, arguments, extra_meta.clone(), bound, bound.is_some())?;
        let (task, bound) = match (called, bound) {
            (crate::wire::Called::Result(r), _) => return Ok(r),
            (crate::wire::Called::Task(t), Some(b)) => (t, b),
            // Not declared, so never handed back: the SDK refuses a task
            // here itself.
            (crate::wire::Called::Task(_), None) => {
                return Err(CallFailed {
                    error: McpError::Transport(format!(
                        "mcp server '{}': tools/call {name}: a task this call did not declare",
                        self.name
                    )),
                    reach: Reach::Unreadable,
                });
            }
        };
        let trace = trace_meta(extra_meta.as_ref());
        let tracker = crate::tasks::Tracker::new(task, unix_ms());
        let op = format!("tools/call {name}");
        match self.follow_task(tracker, Some(started + bound), trace.clone(), &|| false) {
            Ok(crate::tasks::Followed::Ended(t)) => task_outcome(&self.name, &op, &t.task),
            Ok(crate::tasks::Followed::Open(t)) => {
                let _ = self.cancel_task(&t.task.task_id, trace, crate::tasks::REQUEST_BOUND);
                Err(CallFailed {
                    error: McpError::Transport(format!(
                        "mcp server '{}': {op}: the task was still {} at the call's bound ({} ms); tasks/cancel sent",
                        self.name,
                        t.task.status.as_str(),
                        bound.as_millis()
                    )),
                    reach: Reach::Unanswered,
                })
            }
            Err(lost) => Err(CallFailed {
                error: McpError::Transport(format!("mcp server '{}': {op}: {lost}", self.name)),
                reach: Reach::Unanswered,
            }),
        }
    }

    /// `tools/call`, handed back as the server answered it: the tool's
    /// result, or — when `tasks` and the connection declares the extension —
    /// the task it became, for a host that follows it itself. `extra_meta`
    /// and `bound` as for [`Self::call_tool_with_meta_within`].
    pub fn call_tool_called(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Option<Value>,
        bound: Option<Duration>,
        tasks: bool,
    ) -> Result<crate::wire::Called, CallFailed> {
        let meta = merge_meta(self.tool_meta.as_ref(), extra_meta.as_ref());
        let sdk = self.sdk().map_err(|error| CallFailed {
            error,
            reach: Reach::Unsent,
        })?;
        sdk.call_tool_called(name, arguments, meta, bound, tasks)
            .map_err(|(error, reach)| CallFailed { error, reach })
    }

    /// Does the live connection declare the MCP Tasks extension? Only when
    /// [`Self::with_tasks`] asked for it and the server was reached at a
    /// revision that defines it.
    pub fn declares_tasks(&self) -> bool {
        self.sdk().is_ok_and(|c| c.declares_tasks())
    }

    /// `tasks/get`; see [`crate::rmcp_client::RmcpClient::get_task`].
    pub fn get_task(
        &self,
        task_id: &str,
        meta: Option<serde_json::Map<String, Value>>,
        bound: Duration,
    ) -> Result<Result<crate::wire::Task, rpc::RpcError>, McpError> {
        self.sdk()?.get_task(task_id, meta, bound)
    }

    /// `tasks/update`; see [`crate::rmcp_client::RmcpClient::update_task`].
    pub fn update_task(
        &self,
        task_id: &str,
        responses: serde_json::Map<String, Value>,
        meta: Option<serde_json::Map<String, Value>>,
        bound: Duration,
    ) -> Result<Result<(), rpc::RpcError>, McpError> {
        self.sdk()?.update_task(task_id, responses, meta, bound)
    }

    /// `tasks/cancel`; see [`crate::rmcp_client::RmcpClient::cancel_task`].
    pub fn cancel_task(
        &self,
        task_id: &str,
        meta: Option<serde_json::Map<String, Value>>,
        bound: Duration,
    ) -> Result<Result<(), rpc::RpcError>, McpError> {
        self.sdk()?.cancel_task(task_id, meta, bound)
    }

    /// Ask the live connection's listen for pushes about `ids`; see
    /// [`crate::rmcp_client::RmcpClient::listen_tasks`].
    pub fn listen_tasks(
        &self,
        ids: std::collections::BTreeSet<String>,
        bound: Duration,
    ) -> Result<std::collections::BTreeSet<String>, McpError> {
        self.sdk()?.listen_tasks(ids, bound)
    }

    /// Follow a task on this connection until it ends, `until` passes, or
    /// `stop` says the caller no longer waits (checked at least every
    /// [`STOP_CHECK`]): `tasks/get` at the task's own interval
    /// ([`crate::tasks::Tracker::interval`]), each input request answered as
    /// the same request sent directly would be ([`crate::tasks`]) and sent
    /// back with `tasks/update`, and a failed poll tried again after a pause
    /// that doubles from 1 s to 5 min. `meta` rides every `tasks/*` request
    /// (a trace context).
    ///
    /// A form elicitation goes to this connection's elicitation handler —
    /// the host's person — and waits for them no longer than `until`; a key
    /// still being asked when it passes is left unanswered, for whoever
    /// follows the task next. No poll goes out while a person decides.
    /// Without a handler, or for a URL elicitation, the answer is `decline`.
    /// A request this client never declares cancels the task.
    ///
    /// A task handed back open has every answer it was given sent, or its
    /// key released ([`crate::tasks::Tracker::release_unsent`]). A task
    /// given up for its TTL or a failed request is cancelled with
    /// `tasks/cancel` first, as the server may still be running it; one the
    /// server does not know is not.
    pub fn follow_task(
        &self,
        tracker: crate::tasks::Tracker,
        until: Option<Instant>,
        meta: Option<serde_json::Map<String, Value>>,
        stop: &dyn Fn() -> bool,
    ) -> Result<crate::tasks::Followed, crate::tasks::Lost> {
        use crate::tasks::{Followed, Lost};
        let id = tracker.task.task_id.clone();
        match self.follow(tracker, until, &meta, stop) {
            Ok(Followed::Open(mut t)) => {
                // One last try for what the server has not acknowledged.
                let _ = self.send_answers(&mut t, &meta);
                t.release_unsent();
                Ok(Followed::Open(t))
            }
            Err(lost @ (Lost::Expired | Lost::Failed(_))) => {
                let _ = self.cancel_task(&id, meta, crate::tasks::REQUEST_BOUND);
                Err(lost)
            }
            other => other,
        }
    }

    fn follow(
        &self,
        mut tracker: crate::tasks::Tracker,
        until: Option<Instant>,
        meta: &Option<serde_json::Map<String, Value>>,
        stop: &dyn Fn() -> bool,
    ) -> Result<crate::tasks::Followed, crate::tasks::Lost> {
        use crate::tasks::{Followed, Lost};
        let id = tracker.task.task_id.clone();
        let mut failures: u32 = 0;
        // The seed may already be asking.
        let mut asks = tracker.observe(tracker.task.clone()).asks;
        loop {
            self.answer_asks(&mut tracker, std::mem::take(&mut asks), until, meta, stop)?;
            if tracker.ended() {
                return Ok(Followed::Ended(tracker));
            }
            if tracker.expired(unix_ms()) {
                return Err(Lost::Expired);
            }
            let wait = match failures {
                0 => tracker.interval(),
                n => Duration::from_secs(1u64 << (n - 1).min(8)).min(Duration::from_secs(300)),
            };
            let bound = match until {
                Some(u) => {
                    let left = u.saturating_duration_since(Instant::now());
                    if left <= wait {
                        pause(left, stop);
                        return Ok(Followed::Open(tracker));
                    }
                    left - wait
                }
                None => crate::tasks::REQUEST_BOUND,
            };
            if !pause(wait, stop) {
                return Ok(Followed::Open(tracker));
            }
            match self.get_task(&id, meta.clone(), bound.min(crate::tasks::REQUEST_BOUND)) {
                Ok(Ok(task)) => {
                    failures = 0;
                    asks = tracker.observe(task).asks;
                }
                Ok(Err(e)) if e.code == rpc::INVALID_PARAMS => {
                    return Err(Lost::Unknown(e.message));
                }
                // An internal error is the server's or the transport's, and
                // may pass.
                Ok(Err(e)) if e.code == rpc::INTERNAL_ERROR => failures += 1,
                Ok(Err(e)) => {
                    return Err(Lost::Failed(format!(
                        "tasks/get: {} ({})",
                        e.message, e.code
                    )));
                }
                Err(_) => failures += 1,
            }
        }
    }

    /// Answer `asks`, then send every answer not yet acknowledged in one
    /// `tasks/update`. Asking ends early when `until` passes or `stop` says
    /// so: the keys not reached are left unanswered.
    fn answer_asks(
        &self,
        tracker: &mut crate::tasks::Tracker,
        asks: Vec<crate::tasks::Ask>,
        until: Option<Instant>,
        meta: &Option<serde_json::Map<String, Value>>,
        stop: &dyn Fn() -> bool,
    ) -> Result<(), crate::tasks::Lost> {
        use crate::tasks::{Ask, Lost};
        let id = tracker.task.task_id.clone();
        for ask in asks {
            let response = match &ask {
                Ask::Unsupported { method, .. } => {
                    let _ = self.cancel_task(&id, meta.clone(), crate::tasks::REQUEST_BOUND);
                    return Err(Lost::Unsupported(method.clone()));
                }
                Ask::Decline { .. } => serde_json::json!({"action": "decline"}),
                Ask::Elicit {
                    message,
                    requested_schema,
                    ..
                } => match &self.elicitation {
                    None => serde_json::json!({"action": "decline"}),
                    Some(handler) => {
                        // On a plain thread, for the reason the SDK's own
                        // elicitation runs on one: a person's wait holds
                        // nothing, and one past `until` is abandoned.
                        let (tx, rx) = std::sync::mpsc::channel();
                        let handler = Arc::clone(handler);
                        let req = inbound::Inbound::Elicit {
                            message: message.clone(),
                            requested_schema: requested_schema.clone(),
                        };
                        std::thread::spawn(move || {
                            let _ = tx.send(handler.handle(req));
                        });
                        match wait_for(&rx, until, stop) {
                            Some(answer) => crate::tasks::elicit_result(answer.as_ref()),
                            None => break,
                        }
                    }
                },
            };
            tracker.answer(ask.key(), response);
        }
        self.send_answers(tracker, meta)
    }

    /// Send what [`crate::tasks::Tracker::unsent`] holds, in one
    /// `tasks/update`: a server may refuse a partial set. What the server
    /// did not acknowledge stays, for the next try: the person is never
    /// asked again for it.
    fn send_answers(
        &self,
        tracker: &mut crate::tasks::Tracker,
        meta: &Option<serde_json::Map<String, Value>>,
    ) -> Result<(), crate::tasks::Lost> {
        if tracker.unsent.is_empty() {
            return Ok(());
        }
        let responses: serde_json::Map<String, Value> = tracker
            .unsent
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let id = tracker.task.task_id.clone();
        match self.update_task(&id, responses, meta.clone(), crate::tasks::REQUEST_BOUND) {
            Ok(Ok(())) => tracker.unsent.clear(),
            Ok(Err(e)) if e.code == rpc::INVALID_PARAMS => {
                return Err(crate::tasks::Lost::Unknown(e.message));
            }
            Ok(Err(_)) | Err(_) => {}
        }
        Ok(())
    }

    /// `resources/list`, following cursor pagination to completion. `meta`
    /// mints each page's `params._meta`, so a list made for one unit of work
    /// carries its keys; `None` leaves the connection's own
    /// ([`Self::with_request_meta`]).
    pub fn list_resources(
        &self,
        meta: Option<&crate::http::RequestMeta>,
    ) -> Result<Vec<Resource>, McpError> {
        self.sdk()?.list_resources(meta)
    }

    /// `prompts/list`, following cursor pagination to completion.
    pub fn list_prompts(&self) -> Result<Vec<Prompt>, McpError> {
        self.sdk()?.list_prompts()
    }

    /// `prompts/get` — render the named prompt template with `arguments` (a flat
    /// string map). Gated on the server advertising `prompts`. `meta` (an
    /// object) is this request's `params._meta`.
    pub fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
        meta: Option<&Value>,
    ) -> Result<GetPromptResult, McpError> {
        if !self.capabilities().supports_prompts() {
            return Err(McpError::Capability(format!(
                "server '{}' has no prompts",
                self.name
            )));
        }
        self.sdk()?.get_prompt(name, arguments, meta_object(meta))
    }

    /// `completion/complete` — argument autocompletion for a prompt / resource-
    /// template `reference`. Gated on the server advertising `completions`.
    /// `meta` (an object) is this request's `params._meta`.
    pub fn complete(
        &self,
        reference: Value,
        argument: Value,
        meta: Option<&Value>,
    ) -> Result<CompleteResult, McpError> {
        if !self.capabilities().supports_completions() {
            return Err(McpError::Capability(format!(
                "server '{}' has no completions",
                self.name
            )));
        }
        self.sdk()?.complete(reference, argument, meta_object(meta))
    }

    /// `resources/templates/list`, paginated. Empty when the server doesn't
    /// advertise `resources`. `meta` as for [`Self::list_resources`].
    pub fn list_resource_templates(
        &self,
        meta: Option<&crate::http::RequestMeta>,
    ) -> Result<Vec<ResourceTemplate>, McpError> {
        if !self.capabilities().supports_resources() {
            return Ok(Vec::new());
        }
        self.sdk()?.list_resource_templates(meta)
    }

    /// `resources/read`. Always one request to the server: the SDK's response
    /// cache is disabled at connect, so an unreachable server or a JSON-RPC error
    /// is an `Err`, never an earlier answer. `meta` (an object) is this
    /// request's `params._meta`.
    pub fn read_resource(
        &self,
        uri: &str,
        meta: Option<&Value>,
    ) -> Result<ReadResourceResult, McpError> {
        self.sdk()?.read_resource(uri, meta_object(meta))
    }

    /// Subscribe to `uri`'s updates, by whichever mechanism the negotiated
    /// revision defines (see [`crate::rmcp_client::RmcpClient::subscribe`]).
    /// A server that does not advertise `resources.subscribe` is a
    /// [`McpError::Capability`], never a subscription that silently never
    /// fires.
    pub fn subscribe(&self, uri: &str) -> Result<(), McpError> {
        self.sdk()?.subscribe(uri)
    }

    /// [`Self::subscribe`], abandoned as a transport error after `bound` — the
    /// host's management bound, for a subscribe made on a thread that must not
    /// stall. Without it a server that accepts the connection and answers
    /// slowly holds the caller for the connection's whole timeout per URI.
    pub fn subscribe_within(&self, uri: &str, bound: Duration) -> Result<(), McpError> {
        self.sdk()?.subscribe_within(uri, bound)
    }

    pub fn unsubscribe(&self, uri: &str) -> Result<(), McpError> {
        self.sdk()?.unsubscribe(uri)
    }

    /// [`Self::unsubscribe`], abandoned as a transport error after `bound`.
    pub fn unsubscribe_within(&self, uri: &str, bound: Duration) -> Result<(), McpError> {
        self.sdk()?.unsubscribe_within(uri, bound)
    }

    /// One JSON-RPC request the SDK does not model, on this connection; see
    /// [`crate::rmcp_client::RmcpClient::call_raw`].
    pub fn call_raw(
        &self,
        method: &str,
        params: serde_json::Map<String, Value>,
        extensions: Value,
        bound: Duration,
    ) -> Result<Result<Value, rpc::RpcError>, McpError> {
        self.sdk()?.call_raw(method, params, extensions, bound)
    }

    /// The URIs this connection holds a subscription for.
    pub fn subscribed(&self) -> Vec<String> {
        self.sdk().map(|c| c.subscribed()).unwrap_or_default()
    }

    /// Drain any notifications queued since the last drain (e.g.
    /// `notifications/resources/updated`). The reactive router
    /// (`triggers/mode.rs`) drains these between runs to drive re-reactions.
    pub fn drain_notifications(&self) -> Vec<rpc::Notification> {
        self.sdk()
            .map(|c| c.drain_notifications())
            .unwrap_or_default()
    }

    /// Drain what the live connection's listen pump reported (a stream that
    /// ended, one opened again) since the last drain, for the host to log.
    pub fn drain_listen_events(&self) -> Vec<crate::rmcp_client::ListenEvent> {
        self.sdk()
            .map(|c| c.drain_listen_events())
            .unwrap_or_default()
    }
}

/// The trace keys of a call's own `_meta`, which the requests that follow
/// its task carry too: they are spans of the same call.
fn trace_meta(meta: Option<&Value>) -> Option<serde_json::Map<String, Value>> {
    let m: serde_json::Map<String, Value> = meta
        .and_then(Value::as_object)?
        .iter()
        .filter(|(k, _)| *k == "traceparent" || *k == "tracestate")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!m.is_empty()).then_some(m)
}

/// Milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How often a follower that sleeps, or waits for a person, looks at its
/// caller's stop.
pub const STOP_CHECK: Duration = Duration::from_millis(100);

/// Sleep `d`, cut short when `stop` says so: whether it slept all of it.
fn pause(d: Duration, stop: &dyn Fn() -> bool) -> bool {
    let end = Instant::now() + d;
    loop {
        if stop() {
            return false;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(STOP_CHECK));
    }
}

/// A person's answer from `rx`: `None` when `until` passed or `stop` said so
/// first, `Some(None)` when nobody answered.
fn wait_for(
    rx: &std::sync::mpsc::Receiver<Option<inbound::Answer>>,
    until: Option<Instant>,
    stop: &dyn Fn() -> bool,
) -> Option<Option<inbound::Answer>> {
    use std::sync::mpsc::RecvTimeoutError;
    loop {
        if stop() {
            return None;
        }
        let slice = match until {
            Some(u) => {
                let left = u.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                left.min(STOP_CHECK)
            }
            None => STOP_CHECK,
        };
        match rx.recv_timeout(slice) {
            Ok(a) => return Some(a),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Some(None),
        }
    }
}

/// What a task that ended means for the call it answers: a completed one's
/// result is the call's (an `isError` result included), a failed one's
/// JSON-RPC error is the call's error, and a cancelled one is an error the
/// server chose.
pub fn task_outcome(
    server: &str,
    op: &str,
    task: &crate::wire::Task,
) -> Result<CallToolResult, CallFailed> {
    use crate::wire::TaskStatus;
    let failed = |error: McpError| CallFailed {
        error,
        reach: Reach::Answered { status: 200 },
    };
    match task.status {
        TaskStatus::Completed => match task.tool_result() {
            Some(Ok(r)) => Ok(r),
            Some(Err(e)) => Err(CallFailed {
                error: McpError::Transport(format!("mcp server '{server}': {op}: {e}")),
                reach: Reach::Unreadable,
            }),
            None => Err(CallFailed {
                error: McpError::Transport(format!(
                    "mcp server '{server}': {op}: a completed task without its result"
                )),
                reach: Reach::Unreadable,
            }),
        },
        TaskStatus::Failed => {
            let e = task.error.clone().unwrap_or(Value::Null);
            Err(failed(McpError::Transport(format!(
                "mcp server '{server}': {op}: the task failed: {} ({})",
                e["message"].as_str().unwrap_or("no message"),
                e["code"]
            ))))
        }
        TaskStatus::Cancelled => Err(failed(McpError::Transport(format!(
            "mcp server '{server}': {op}: the task was cancelled{}",
            task.status_message
                .as_deref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        )))),
        TaskStatus::Working | TaskStatus::InputRequired => Err(CallFailed {
            error: McpError::Transport(format!(
                "mcp server '{server}': {op}: the task has not ended"
            )),
            reach: Reach::Unanswered,
        }),
    }
}

/// A request's own `params._meta`: `meta` when it is an object holding a key.
fn meta_object(meta: Option<&Value>) -> Option<serde_json::Map<String, Value>> {
    meta.and_then(Value::as_object)
        .filter(|m| !m.is_empty())
        .cloned()
}

/// The `params._meta` of one call: the connection's persistent meta with the
/// call's own keys on top. `None` when neither holds a key, so a call with no
/// meta sends no `_meta` at all.
fn merge_meta(
    base: Option<&Value>,
    extra: Option<&Value>,
) -> Option<serde_json::Map<String, Value>> {
    let mut m = base.and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(eo) = extra.and_then(Value::as_object) {
        for (k, v) in eo {
            m.insert(k.clone(), v.clone());
        }
    }
    (!m.is_empty()).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn meta_overlay_wins_without_mutating_the_base() {
        let base = json!({"agent/run_id": "r1", "traceparent": "tp"});
        let merged = merge_meta(Some(&base), Some(&json!({"traceparent": "tp2", "k": 1}))).unwrap();
        assert_eq!(merged["agent/run_id"], "r1");
        assert_eq!(merged["traceparent"], "tp2");
        assert_eq!(merged["k"], 1);
        assert_eq!(base["traceparent"], "tp");
        assert!(merge_meta(None, None).is_none());
        assert!(merge_meta(None, Some(&json!({}))).is_none());
    }

    #[test]
    fn error_display() {
        let e = McpError::Capability("resources on 'fs'".into());
        assert!(e.to_string().contains("capability"));
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

    #[test]
    fn an_operation_before_initialize_is_an_error_not_a_panic() {
        let c = McpClient::connect(
            "early",
            "http://127.0.0.1:1/mcp",
            Vec::new(),
            Duration::from_secs(1),
        )
        .expect("connect only parses");
        assert!(matches!(c.list_tools(), Err(McpError::Transport(_))));
        assert!(c.drain_notifications().is_empty());
    }

    /// A loopback TCP listener that ACCEPTS a connection but never replies — an
    /// alive-but-silent server, to prove the per-request timeout governs (not a
    /// hang).
    fn spawn_silent_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind silent server");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
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
        format!("http://{addr}/mcp")
    }

    #[test]
    fn management_timeout_bounds_a_handshake_on_a_silent_server() {
        // The server accepts but never replies; a handshake with the SHORT
        // management bound must fail fast — the per-call timeout, not a hang
        // (the default here is a minute, so only the short bound can end it).
        let endpoint = spawn_silent_server();
        let mut client =
            McpClient::connect("silent", &endpoint, Vec::new(), Duration::from_secs(60))
                .expect("connect");

        let short = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let r = client.initialize_within(short);
        let elapsed = started.elapsed();
        assert!(r.is_err(), "a silent server cannot complete a handshake");
        assert!(
            elapsed < Duration::from_secs(5),
            "the short per-call timeout must govern (took {elapsed:?})"
        );
    }
}
