// SPDX-License-Identifier: AGPL-3.0-only
//! **The A2A listener.**
//!
//! What arrives here is HTTP; what leaves is a decision about *who is calling*.
//! Everything after that — parsing the JSON-RPC envelope, typing the params,
//! dispatching the method, framing SSE, mapping errors to the spec's codes — is
//! [`a2a_rs`]'s, reached by handing it the request with an authenticated
//! principal attached. agentd's job is the two things a protocol crate cannot
//! know: which caller a connection represents, and which of them may do what.
//!
//! ## Identity
//!
//! Four kinds of evidence, in the order they are trusted:
//!
//! 1. a **pairing session token** (RFC 0032 §13) — the code-for-token exchange
//!    that logs a display client in;
//! 2. a **verified client certificate** — subject CN and SANs, so a `san`/`sub`
//!    principal rule matches a cert directly (a SPIFFE X.509-SVID's
//!    `spiffe://…` arrives as a URI SAN);
//! 3. the configured **server bearer**;
//! 4. **loopback with nothing configured**, which is the single-operator dev
//!    posture and the only case where absent credentials mean trust.
//!
//! ## Two vocabularies on one endpoint
//!
//! Most methods are the specification's. A few are agentd's own — the
//! observation feed the display clients read (`SubscribeToEvents`), the pairing
//! exchange (`Pair`), and the operator admin family (`a2a.*`) — and those are
//! answered here rather than passed down, because a2a-rs correctly does not know
//! them. Anything else goes to the protocol layer, including the methods it
//! implements that agentd does not, so an unimplemented method is refused with
//! the code the spec assigns rather than a generic failure.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::a2a::Principal;
use crate::a2a::ports::{self, RuntimePorts};
use crate::obs::log::Logger;
use crate::runtime::a2a_server::{A2aBridge, PairingState, SharedFeed};

/// How the listener decides whether a caller is credentialed.
pub struct Auth {
    /// The listener requires a credential (a client CA or a server bearer).
    pub require_auth: bool,
    /// The resolved server bearer; presenting it is the operator.
    pub server_bearer: Option<String>,
    /// Pairing-code login (RFC 0032 §13), when the interface arms it.
    pub pairing: Option<Arc<PairingState>>,
}

/// Everything the listener needs that is not the bridge.
pub struct Opts {
    pub auth: Auth,
    /// Origins a browser UI may be served from, beyond loopback (RFC 0032 §7).
    pub extra_origins: Vec<String>,
    /// TLS, when the listen URL is `https://`.
    pub tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    /// How long a unary request may wait on the runtime.
    pub request_timeout: Duration,
    /// How long an observation-feed stream is held open before it hands the
    /// client a cursor and asks it to come back.
    pub stream_deadline: Duration,
}

/// A running listener. Dropping it stops serving.
pub struct Listener {
    /// The authority actually bound (a `:0` request resolves to a real port).
    pub bound: String,
    /// Where the reactor publishes task transitions for subscribers.
    pub sink: Arc<ports::StreamSink>,
    /// Kept alive because dropping the runtime stops the accept loop.
    _runtime: tokio::runtime::Runtime,
}

struct App {
    /// a2a-rs's JSON-RPC surface, delegated to for every spec method.
    protocol: Router,
    bridge: Arc<A2aBridge>,
    auth: Auth,
    extra_origins: Vec<String>,
    stream_deadline: Duration,
    log: Logger,
}

/// Start the listener on its own runtime and thread.
///
/// The runtime is separate from everything else agentd does: the reactor is a
/// blocking single-threaded loop and must stay that way, so the async world is
/// confined to this listener and reaches the runtime only through [`A2aBridge`].
pub fn spawn(
    bind: &str,
    opts: Opts,
    bridge: Arc<A2aBridge>,
    feed: Option<Arc<SharedFeed>>,
    log: Logger,
) -> Result<Listener, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("agentd-a2a")
        .build()
        .map_err(|e| format!("a2a runtime: {e}"))?;

    let updates = Arc::new(a2a_rs::adapter::InMemoryStreamingHandler::new());
    let sink = Arc::new(ports::StreamSink::new(
        Arc::clone(&updates),
        runtime.handle().clone(),
        log.clone(),
    ));
    let ports = RuntimePorts::new(Arc::clone(&bridge), Arc::clone(&updates));
    let adapter = Arc::new(
        a2a_rs::adapter::JsonRpcAdapter::with_handler(ports, CardFromRuntime(Arc::clone(&bridge)))
            .with_streaming_handler(ports::SharedStreaming(updates)),
    );

    let app = Arc::new(App {
        protocol: a2a_rs::adapter::jsonrpc_router(adapter),
        bridge: Arc::clone(&bridge),
        auth: opts.auth,
        extra_origins: opts.extra_origins,
        stream_deadline: opts.stream_deadline,
        log: log.clone(),
    });
    let _ = feed;

    let router = Router::new()
        .route("/", post(rpc).options(preflight))
        // Discovery: the card is public, by both of its conventional paths.
        .route("/.well-known/agent-card.json", get(card))
        .route("/.well-known/agent.json", get(card))
        .with_state(Arc::clone(&app));

    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(bind))
        .map_err(|e| format!("a2a bind {bind}: {e}"))?;
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| bind.to_string());

    let tls = opts.tls;
    runtime.spawn(accept_loop(listener, router, tls, log));

    Ok(Listener {
        bound,
        sink,
        _runtime: runtime,
    })
}

/// Accept connections forever, terminating TLS when configured, and serve each
/// with the router. The verified peer identity is attached to every request on
/// that connection, which is how a `san`/`sub` principal rule sees a client cert.
async fn accept_loop(
    listener: tokio::net::TcpListener,
    router: Router,
    tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    log: Logger,
) {
    loop {
        let Ok((sock, peer)) = listener.accept().await else {
            continue;
        };
        let router = router.clone();
        let tls = tls.clone();
        let log = log.clone();
        tokio::spawn(async move {
            match tls {
                Some(cfg) => {
                    let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
                    match acceptor.accept(sock).await {
                        Ok(stream) => {
                            let peer_id = peer_identity(stream.get_ref().1);
                            serve_conn(stream, router, peer_id, peer, log).await;
                        }
                        Err(e) => log.debug("a2a.tls", json!({"err": e.to_string()})),
                    }
                }
                None => serve_conn(sock, router, PeerId::default(), peer, log).await,
            }
        });
    }
}

/// The verified identity of the client certificate, when one was presented.
#[derive(Clone, Default, Debug)]
pub struct PeerId {
    pub presented: bool,
    pub subject: Option<String>,
    pub sans: Vec<String>,
}

fn peer_identity(conn: &tokio_rustls::rustls::ServerConnection) -> PeerId {
    let Some(chain) = conn.peer_certificates() else {
        return PeerId::default();
    };
    let Some(leaf) = chain.first() else {
        return PeerId {
            presented: true,
            ..Default::default()
        };
    };
    let id = crate::net::x509::parse(leaf.as_ref());
    PeerId {
        presented: true,
        subject: id.subject_cn,
        sans: id.sans,
    }
}

async fn serve_conn<S>(stream: S, router: Router, peer_id: PeerId, peer: SocketAddr, log: Logger)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Every request on this connection carries the connection's evidence.
    let router = router
        .layer(axum::Extension(peer_id))
        .layer(axum::Extension(Peer(peer)));
    let svc = hyper_util::service::TowerToHyperService::new(
        router.into_service::<hyper::body::Incoming>(),
    );
    let io = hyper_util::rt::TokioIo::new(stream);
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, svc)
        .with_upgrades()
        .await
    {
        log.debug("a2a.conn", json!({"err": e.to_string()}));
    }
}

/// The remote address, for the loopback determination.
#[derive(Clone, Copy)]
struct Peer(SocketAddr);

// ---- the card ---------------------------------------------------------------

/// The agent card, read from the runtime so its skills reflect the workflows
/// that are actually loaded rather than a snapshot taken at boot.
struct CardFromRuntime(Arc<A2aBridge>);

#[async_trait::async_trait]
impl a2a_rs::services::AgentInfoProvider for CardFromRuntime {
    async fn get_agent_card(&self) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        self.card("GetAgentCard", Principal::anonymous()).await
    }

    /// The authenticated card, scoped to whoever is asking. The caller travels
    /// on the request's task-local, because the port takes none.
    async fn get_authenticated_extended_card(
        &self,
    ) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        self.card("GetExtendedAgentCard", ports::caller()).await
    }
}

impl CardFromRuntime {
    async fn card(
        &self,
        method: &'static str,
        who: Principal,
    ) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        let bridge = Arc::clone(&self.0);
        let v = tokio::task::spawn_blocking(move || bridge.call(method, json!({}), who))
            .await
            .map_err(|e| a2a_rs::domain::A2AError::Internal(e.to_string()))?;
        if let Some(e) = v.get("_error") {
            return Err(a2a_rs::domain::A2AError::UnsupportedOperation(
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no extended card")
                    .to_string(),
            ));
        }
        serde_json::from_value(v).map_err(a2a_rs::domain::A2AError::JsonParse)
    }
}

/// A CORS preflight. A browser UI served from a configured origin (RFC 0032 §7)
/// has to be told it may POST here; every other origin is refused, which is the
/// same DNS-rebind answer the POST itself gives.
async fn preflight(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !origin_allowed(origin, &app.extra_origins) {
        return (StatusCode::FORBIDDEN, "").into_response();
    }
    (
        StatusCode::NO_CONTENT,
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.to_string()),
            (
                header::ACCESS_CONTROL_ALLOW_METHODS,
                "POST, GET, OPTIONS".to_string(),
            ),
            (
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "content-type, authorization, last-event-id".to_string(),
            ),
            (header::ACCESS_CONTROL_MAX_AGE, "600".to_string()),
        ],
    )
        .into_response()
}

/// Grant the caller's origin on a real response, so the browser hands the body
/// to the page that asked for it.
fn allow_origin(mut resp: Response, origin: Option<&str>) -> Response {
    if let Some(o) = origin
        && let Ok(v) = axum::http::HeaderValue::from_str(o)
    {
        resp.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    resp
}

/// GET on either well-known path: discovery is public, by design.
async fn card(State(app): State<Arc<App>>) -> Response {
    let bridge = Arc::clone(&app.bridge);
    let v = tokio::task::spawn_blocking(move || {
        bridge.call("GetAgentCard", json!({}), Principal::anonymous())
    })
    .await
    .unwrap_or(Value::Null);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&v).unwrap_or_default(),
    )
        .into_response()
}

// ---- the JSON-RPC endpoint --------------------------------------------------

async fn rpc(
    State(app): State<Arc<App>>,
    axum::Extension(peer_id): axum::Extension<PeerId>,
    axum::Extension(Peer(peer)): axum::Extension<Peer>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let allowed = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    allow_origin(
        dispatch(app, peer_id, peer, headers, body).await,
        allowed.as_deref(),
    )
}

async fn dispatch(
    app: Arc<App>,
    peer_id: PeerId,
    peer: SocketAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // A browser page on an unexpected origin must not be able to drive this
    // endpoint through a victim's browser (DNS rebinding, RFC 0032 §7).
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(o) = &origin
        && !origin_allowed(o, &app.extra_origins)
    {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }

    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        return err(Value::Null, -32700, "invalid JSON");
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(str::to_string);

    let Some(principal) = resolve(&app, &peer_id, peer, bearer.as_deref()) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "",
        )
            .into_response();
    };

    // agentd's own vocabulary, which the protocol layer does not know.
    let bare = method.strip_prefix("a2a.").unwrap_or(&method).to_string();
    match bare.as_str() {
        // Discovery. The spec's JSON-RPC binding has no method for the *public*
        // card — it is fetched from `.well-known` — but every agentd client asks
        // for it here, so both ways work and both are unauthenticated.
        "GetAgentCard" | "agent/card" => {
            return unary(&app, id, "GetAgentCard", json!({}), principal).await;
        }
        // Served here rather than passed down for the same reason as the public
        // card: both must be the *same document*, and a round trip through the
        // SDK's `AgentCard` drops any field it has no place for.
        "GetExtendedAgentCard" | "agent/getAuthenticatedExtendedCard" => {
            if principal.is_anonymous() {
                return err(
                    id,
                    -32007,
                    "the extended card requires an authenticated caller",
                );
            }
            return unary(&app, id, "GetExtendedAgentCard", json!({}), principal).await;
        }
        // The one method an anonymous caller may use: exchanging the rotating
        // code for a session token IS the login (RFC 0032 §13).
        "Pair" | "interface.pair" => {
            return unary(&app, id, "Pair", params, principal).await;
        }
        "SubscribeToEvents" => {
            return match &app.bridge.feed() {
                Some(feed) => {
                    if !principal.may("SubscribeToEvents", None) {
                        return err(id, -32003, "not authorized");
                    }
                    feed_stream(Arc::clone(feed), id, params, principal, app.stream_deadline)
                }
                None => err(
                    id,
                    -32004,
                    "the interface surface is disabled (set interface.enabled: true)",
                ),
            };
        }
        _ => {}
    }
    if crate::a2a::principals::is_admin(&method) {
        if !principal.is_operator() {
            return err(id, -32003, "operator role required");
        }
        return unary(&app, id, &method, params, principal).await;
    }

    // Authorization for the spec's methods: natural language is open to any
    // non-anonymous role; a command DataPart is checked against the role's
    // command grants.
    let op = params
        .get("message")
        .and_then(crate::runtime::a2a_server::command_op);
    if !principal.may(&bare, op.as_deref()) {
        app.log.warn(
            "a2a.denied",
            json!({"principal": principal.id, "method": bare, "op": op}),
        );
        return err(id, -32003, "not authorized");
    }

    // A command DataPart is agentd's own vocabulary riding the spec's data
    // part, not an A2A concept: some of them answer without creating a task at
    // all (RFC 0032's taskless reads). Forcing those through a port that must
    // return a `Task` would mean inventing one. So they go straight to the
    // runtime, and its answer — task or not — is returned as it stands.
    if matches!(bare.as_str(), "SendMessage" | "SendStreamingMessage")
        && crate::runtime::a2a_server::command_op(&params["message"]).is_some()
    {
        let streaming = bare == "SendStreamingMessage";
        return unary_maybe_streamed(&app, id, "SendMessage", params, principal, streaming).await;
    }

    // A send with no task id yet gets one now. The protocol layer subscribes to
    // a task's updates *before* it processes the message — so that a fast
    // transition cannot be missed — and it can only do that if the id exists
    // first. Without this, a blocking send would never see the task settle and
    // a streaming send would be refused outright for want of an id.
    let body = match bare.as_str() {
        "SendMessage" | "SendStreamingMessage" => {
            match normalize_send(&app.bridge, &req, &params).await {
                Some(rewritten) => Bytes::from(rewritten),
                None => body,
            }
        }
        _ => body,
    };

    // Everything else is the specification's, and a2a-rs answers it — including
    // the methods it implements and agentd does not, which is why an
    // unsupported one comes back with the spec's code rather than ours.
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/")
        .body(Body::from(body))
        .expect("build request");
    *request.headers_mut() = headers;
    request
        .extensions_mut()
        .insert(a2a_rs::port::AuthPrincipal::new(
            principal.id.clone(),
            "agentd".to_string(),
        ));
    let protocol = app.protocol.clone();
    ports::with_caller(principal, async move {
        protocol
            .oneshot(request)
            .await
            .unwrap_or_else(|_| err(Value::Null, -32603, "dispatch failed"))
    })
    .await
}

/// Prepare a send for the protocol layer, returning a rewritten request body —
/// or `None` when nothing needed changing.
///
/// Two adjustments, both about meeting the specification where agentd used to
/// differ:
///
/// * **The task id.** A send that names no task gets one now, because the
///   protocol layer subscribes to a task's updates *before* it processes the
///   message — so a fast transition cannot be missed — and it can only do that
///   if the id exists first.
/// * **`blocking` → `returnImmediately`.** agentd's own clients ask not to wait
///   with `configuration.blocking: false`; the spec spells the same thing
///   `returnImmediately: true`. Translating here keeps those clients working
///   against a server that now speaks only the specification's field.
///
/// Both rewrites write *into* `params`, which is whatever a remote caller put on
/// the wire. Neither is attempted unless the params carry the shape the spec
/// requires — an object with an object `message` — because the only way to write
/// into a `Value` is through a path of objects, and serde_json's `IndexMut`
/// *panics* rather than declining when the value under the path is a string, a
/// number or an array (`params: []`, `params: {"message": "hi"}`). The release
/// profile is `panic = "abort"`, so one malformed request would take the whole
/// daemon down. A shape that cannot be rewritten is passed through untouched
/// instead, and a2a-rs refuses it with the spec's -32602.
async fn normalize_send(bridge: &Arc<A2aBridge>, req: &Value, params: &Value) -> Option<Vec<u8>> {
    if !params.is_object() || !params.get("message").is_some_and(Value::is_object) {
        return None;
    }

    let mut req = req.clone();
    let mut changed = false;

    if params["message"]["taskId"]
        .as_str()
        .unwrap_or("")
        .is_empty()
    {
        let bridge = Arc::clone(bridge);
        if let Ok(v) = tokio::task::spawn_blocking(move || {
            bridge.call("NewTaskId", json!({}), Principal::anonymous())
        })
        .await
            && let Some(id) = v.get("id").and_then(Value::as_str)
            && let Some(message) = param_object(&mut req, "message")
        {
            message.insert("taskId".to_string(), json!(id));
            changed = true;
        }
    }

    if let Some(blocking) = params["configuration"]["blocking"].as_bool()
        && params["configuration"]["returnImmediately"].is_null()
        && let Some(config) = param_object(&mut req, "configuration")
    {
        config.insert("returnImmediately".to_string(), json!(!blocking));
        changed = true;
    }

    changed.then(|| serde_json::to_vec(&req).ok()).flatten()
}

/// `req.params.<field>` as a map to write into, or `None` when anything along
/// that path is not an object. Every rewrite goes through here rather than
/// through `IndexMut`, whose failure mode on a caller-controlled shape is a
/// panic in the listener rather than a request that gets refused.
fn param_object<'a>(
    req: &'a mut Value,
    field: &str,
) -> Option<&'a mut serde_json::Map<String, Value>> {
    req.as_object_mut()?
        .get_mut("params")?
        .as_object_mut()?
        .get_mut(field)?
        .as_object_mut()
}

/// One reactor round trip, answered as a JSON-RPC envelope.
async fn unary(
    app: &Arc<App>,
    id: Value,
    method: &str,
    params: Value,
    principal: Principal,
) -> Response {
    unary_maybe_streamed(app, id, method, params, principal, false).await
}

/// [`unary`], but able to answer as a one-frame SSE stream.
///
/// `SendStreamingMessage` promises a stream, and that promise does not depend on
/// what the message turned out to contain. A command DataPart is answered by the
/// runtime in one step, so there is exactly one frame to send — but a caller
/// that asked for a stream and received a JSON body would fail to parse it,
/// which is a worse answer than a short stream.
async fn unary_maybe_streamed(
    app: &Arc<App>,
    id: Value,
    method: &str,
    params: Value,
    principal: Principal,
    streamed: bool,
) -> Response {
    let bridge = Arc::clone(&app.bridge);
    let method = method.to_string();
    let v = tokio::task::spawn_blocking(move || bridge.call(&method, params, principal))
        .await
        .unwrap_or_else(|e| json!({"_error": {"code": -32603, "message": e.to_string()}}));
    let envelope = match v.get("_error") {
        Some(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        None => json!({"jsonrpc": "2.0", "id": id, "result": v}),
    };
    if !streamed {
        return json_response(envelope);
    }
    let frame = axum::response::sse::Event::default()
        .id("1")
        .data(serde_json::to_string(&envelope).unwrap_or_default());
    axum::response::Sse::new(futures_util::stream::once(async move {
        Ok::<_, std::convert::Infallible>(frame)
    }))
    .into_response()
}

fn json_response(v: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&v).unwrap_or_default(),
    )
        .into_response()
}

fn err(id: Value, code: i64, message: &str) -> Response {
    json_response(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
}

// ---- identity ---------------------------------------------------------------

/// Resolve the caller, or `None` for "present a credential".
///
/// The order is the order of trust: a pairing session names its own role; a
/// verified certificate and the server bearer are the operator; any other bearer
/// may still match a configured principal rule; and an uncredentialed request is
/// refused unless the listener has no credentials to require, or pairing is
/// armed — in which case it arrives as anonymous, able to call exactly `Pair`
/// and read the public card, which is how a code holder logs in.
fn resolve(
    app: &Arc<App>,
    peer_id: &PeerId,
    peer: SocketAddr,
    bearer: Option<&str>,
) -> Option<Principal> {
    let a = &app.auth;
    if let (Some(p), Some(b)) = (&a.pairing, bearer)
        && let Some(role) = p.check_bearer(b)
    {
        return Some(crate::runtime::a2a_server::paired_principal(role));
    }
    let loopback = peer.ip().is_loopback();
    let mgmt = (!a.require_auth && loopback) || peer_id.presented || is_server_bearer(a, bearer);
    if !a.require_auth {
        return Some(app.bridge.principal_of(
            true,
            bearer,
            peer_id.subject.clone(),
            peer_id.sans.clone(),
        ));
    }
    if !mgmt && bearer.is_none() && a.pairing.is_none() {
        return None;
    }
    Some(
        app.bridge
            .principal_of(mgmt, bearer, peer_id.subject.clone(), peer_id.sans.clone()),
    )
}

fn is_server_bearer(a: &Auth, bearer: Option<&str>) -> bool {
    match (&a.server_bearer, bearer) {
        (Some(server), Some(got)) => crate::sha::ct_eq(server.as_bytes(), got.as_bytes()),
        _ => false,
    }
}

/// Loopback origins are always allowed; anything else must be configured.
fn origin_allowed(origin: &str, extra: &[String]) -> bool {
    if extra.iter().any(|o| o == origin || o == "*") {
        return true;
    }
    let host = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .split(':')
        .next()
        .unwrap_or("");
    crate::net::http::is_loopback_host(host)
}

// ---- the observation feed ----------------------------------------------------

/// `SubscribeToEvents` (RFC 0032 §4): agentd's own stream, not the spec's.
///
/// A `hello` frame states the cursor the client resumed from and whether that
/// cursor still exists — a cursor evicted from the replay window comes back
/// `resync`, meaning re-bootstrap. Then the events the principal may see, and
/// finally a `goodbye` carrying the cursor to resume from, so a reconnect is a
/// continuation rather than a restart.
fn feed_stream(
    feed: Arc<SharedFeed>,
    id: Value,
    params: Value,
    principal: Principal,
    deadline: Duration,
) -> Response {
    let after = params
        .get("fromSeq")
        .or_else(|| params.get("after"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let (newest, oldest, dropped) = feed.bounds();
    // The cursor predates the replay window: events were evicted past it, so
    // replay from the window start and tell the client to re-bootstrap.
    let evicted = after > 0 && dropped > 0 && after < oldest.saturating_sub(1);
    // The cursor is *ahead* of the feed, which is what every attached client
    // holds across a daemon restart: the feed is in-memory and its seq begins
    // again at 0. Honouring such a cursor silently kills the subscription —
    // `since` only ever yields `seq > cursor`, so the client would sit through a
    // whole restart's worth of events seeing nothing and never learn why.
    let ahead = after > newest;
    let resync = evicted || ahead;
    let start = if resync { 0 } else { after };

    let (tx, rx) = tokio::sync::mpsc::channel::<axum::response::sse::Event>(64);
    let is_op = principal.is_operator();
    let who = principal.id.clone();
    tokio::spawn(async move {
        let hello = json!({"hello": {
            "seq": newest,
            "resume": after,
            "resync": resync,
            "debug": feed.debug(),
            "version": crate::VERSION,
        }});
        if tx.send(frame(&id, hello)).await.is_err() {
            return;
        }
        let mut cursor = start;
        let end = Instant::now() + deadline;
        loop {
            let (events, next) = feed.since(cursor, &who, is_op, 256);
            cursor = next;
            for ev in events {
                if tx.send(frame(&id, json!({"event": ev}))).await.is_err() {
                    return; // the client went away
                }
            }
            if Instant::now() >= end {
                let bye = json!({"goodbye": {"seq": cursor, "reason": "deadline"}});
                let _ = tx.send(frame(&id, bye)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let stream =
        tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);
    axum::response::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

fn frame(id: &Value, payload: Value) -> axum::response::sse::Event {
    axum::response::sse::Event::default().data(
        serde_json::to_string(&json!({"jsonrpc": "2.0", "id": id, "result": payload}))
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bridge with a stand-in for the reactor: it answers `NewTaskId` with an
    /// id, because a bridge whose loop is missing fails fast and would leave the
    /// rewrite — the code that used to panic — unreached, making these tests pass
    /// against the bug they exist to catch.
    fn stub_bridge() -> Arc<A2aBridge> {
        let resolver =
            crate::a2a::Resolver::build(&crate::config::v2::A2a::default(), &|_| None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(crate::runtime::events::Event::A2a(req)) = rx.recv() {
                let _ = req.reply.send(json!({"id": "task-stub"}));
            }
        });
        A2aBridge::new(tx, resolver)
    }

    /// Params are remote input, and a send whose params are not the shape the
    /// spec requires used to reach serde_json's `IndexMut` and panic the
    /// listener — which under the release profile's `panic = "abort"` is a dead
    /// daemon from one curl. Every one of these must come back "nothing to
    /// rewrite" so the body travels on and a2a-rs answers it with -32602.
    #[tokio::test]
    async fn malformed_send_params_are_left_alone_rather_than_panicking() {
        let bridge = stub_bridge();
        for params in [
            json!([]),
            json!({"message": "hi"}),
            json!({"message": 3}),
            json!({"message": []}),
            Value::Null,
            json!("send"),
            json!({}),
        ] {
            let req =
                json!({"jsonrpc": "2.0", "id": 1, "method": "message/send", "params": params});
            // Exactly how `dispatch` derives the params it passes in.
            let p = req.get("params").cloned().unwrap_or_else(|| json!({}));
            assert_eq!(
                normalize_send(&bridge, &req, &p).await,
                None,
                "params {p} must not be rewritten"
            );
        }
    }

    /// The other half of the guard: a well-formed send must still be normalised
    /// — both rewrites — because refusing every shape would "fix" the panic by
    /// breaking the send path the protocol layer depends on.
    #[tokio::test]
    async fn a_well_formed_send_is_still_normalised() {
        let bridge = stub_bridge();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "message/send", "params": {
            "message": {"messageId": "m1", "role": "user", "parts": [{"kind": "text", "text": "hi"}]},
            "configuration": {"blocking": false},
        }});
        let params = req["params"].clone();
        let out = normalize_send(&bridge, &req, &params)
            .await
            .expect("a well-formed send is rewritten");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["params"]["message"]["taskId"], json!("task-stub"));
        assert_eq!(
            v["params"]["configuration"]["returnImmediately"],
            json!(true)
        );
    }
}
