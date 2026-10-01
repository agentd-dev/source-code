// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP client agentd's runtime holds: one remote server over **Streamable
//! HTTP**, reached over a socket or not at all — no local process is spawned.
//!
//! [`McpClient`] is a connection *builder* until [`McpClient::initialize`]: it
//! collects the endpoint, the caller's headers, a request signer, an mTLS
//! identity and the elicitation handler, then hands them to the official SDK
//! ([`crate::rmcp_client`]), which runs the handshake and answers every
//! operation after it over agentd's own socket ([`crate::http`]).

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
    /// Stamped into every `tools/call` request's `params._meta` (e.g.
    /// `{"agent/run_id": …}`) so a backing service can recognize a retried call
    /// as the same logical operation and dedupe it rather than repeating a side
    /// effect. The one copy of it: every call path merges from here, so no path
    /// can send a call without it.
    tool_meta: Option<Value>,
    /// The client identity sent in `initialize`. Defaults to this crate's
    /// identity; the host overrides it via [`Self::with_client_info`] (agentd
    /// sets its own name + version).
    client_info: Implementation,
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

    /// [`Self::connect`] with an optional per-request AAuth signer — every
    /// outbound request to this server is then signed, including the long-lived
    /// notification stream. `None` = unsigned (the `connect` default).
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
            tool_meta: None,
            client_info: Implementation {
                name: "agentd".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
            },
        })
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

    /// Set the `_meta` stamped onto every `tools/call` (e.g. the run id), so a
    /// server can recognize a retried call and dedupe its side effect. Call
    /// after `initialize`.
    pub fn set_tool_meta(&mut self, meta: Value) {
        self.tool_meta = Some(meta);
    }

    /// MCP lifecycle handshake: `initialize` → store capabilities →
    /// `notifications/initialized`. Uses the default per-request timeout.
    pub fn initialize(&mut self) -> Result<(), McpError> {
        self.initialize_within(self.timeout)
    }

    /// [`Self::initialize`] with a caller-supplied timeout for the `initialize`
    /// round-trip — the SHORT management bound. Used by the hot-reload
    /// re-handshake, which adds a server ON the reactor thread mid-loop: a
    /// slow-but-alive added server must not block the reactor (and starve the
    /// liveness heartbeat) for the full default bound. A timeout is contained,
    /// reported as `mcp.connect.fail`, and the server is simply treated as
    /// absent rather than failing the reload.
    pub fn initialize_within(&mut self, timeout: Duration) -> Result<(), McpError> {
        let c = self.dial(Arc::clone(&self.http), timeout, None)?;
        *self.rmcp.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(c));
        self.redial
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .dialed = Some(Instant::now());
        Ok(())
    }

    /// One handshake on `http`.
    fn dial(
        &self,
        http: Arc<HttpTransport>,
        timeout: Duration,
        handshake_bound: Option<Duration>,
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
                .with_client_info(self.client_info.clone());
        if let Some(h) = &self.elicitation {
            b = b.with_elicitation(Arc::clone(h));
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
        let fresh = match self.dial(Arc::new(self.http.redial_copy()), self.timeout, Some(bound)) {
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
    /// [`Self::call_tool_with_meta_within`].
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
    pub fn call_tool_with_meta_within(
        &self,
        name: &str,
        arguments: Option<Value>,
        extra_meta: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        self.call(name, arguments, Some(extra_meta), Some(timeout))
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
        let meta = merge_meta(self.tool_meta.as_ref(), extra_meta.as_ref());
        let raw = self.sdk()?.call_tool(name, arguments, meta, bound)?;
        serde_json::from_value(raw).map_err(|e| {
            McpError::Transport(format!("bad tools/call result on '{}': {e}", self.name))
        })
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, McpError> {
        self.sdk()?.list_resources()
    }

    /// `prompts/list`, following cursor pagination to completion.
    pub fn list_prompts(&self) -> Result<Vec<Prompt>, McpError> {
        self.sdk()?.list_prompts()
    }

    /// `prompts/get` — render the named prompt template with `arguments` (a flat
    /// string map). Gated on the server advertising `prompts`.
    pub fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<GetPromptResult, McpError> {
        if !self.capabilities().supports_prompts() {
            return Err(McpError::Capability(format!(
                "server '{}' has no prompts",
                self.name
            )));
        }
        self.sdk()?.get_prompt(name, arguments)
    }

    /// `completion/complete` — argument autocompletion for a prompt / resource-
    /// template `reference`. Gated on the server advertising `completions`.
    pub fn complete(&self, reference: Value, argument: Value) -> Result<CompleteResult, McpError> {
        if !self.capabilities().supports_completions() {
            return Err(McpError::Capability(format!(
                "server '{}' has no completions",
                self.name
            )));
        }
        self.sdk()?.complete(reference, argument)
    }

    /// `resources/templates/list`, paginated. Empty when the server doesn't
    /// advertise `resources`.
    pub fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        if !self.capabilities().supports_resources() {
            return Ok(Vec::new());
        }
        // The SDK walks the cursor; pagination is its problem, not ours.
        self.sdk()?.list_resource_templates()
    }

    /// `resources/read`. Always one request to the server: the SDK's response
    /// cache is disabled at connect, so an unreachable server or a JSON-RPC error
    /// is an `Err`, never an earlier answer.
    pub fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        self.sdk()?.read_resource(uri)
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
