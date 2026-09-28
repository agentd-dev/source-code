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
//! Every request is resolved from its headers and its connection, before a
//! byte of its body is read, by the resolver the bridge holds — one snapshot
//! per request, which supplies both the rules and the listener's posture:
//!
//! 1. a **unix-socket peer** of the daemon's own uid is the operator;
//! 2. a presented **bearer** — the server bearer, a `bearer_ref` rule, or a
//!    session token — decides, and one that matches nothing is a 401;
//! 3. a **verified client certificate** matches the `san`/`sub` rules (a
//!    SPIFFE X.509-SVID's `spiffe://…` arrives as a URI SAN), and is the
//!    operator only while no rule exists;
//! 4. an **`any` rule** names whoever is left;
//! 5. a local, non-browser caller of a **loopback listener with nothing
//!    configured** is the operator — the single-operator dev posture, and the
//!    only case where absent credentials mean trust.
//!
//! A request that fails to authenticate after presenting something counts
//! against its source ([`limits`]); one that presented nothing never does.
//! Past the limit, the source's bearers are refused 429 without being
//! checked, so a guesser's rate is the limiter's refill rate — while the
//! implicit operator and an `any` rule, which present nothing, are never
//! refused by it. Refusals of callers nobody vouched for are logged once per
//! source per window rather than once per request ([`limits::DenialLog`]).
//!
//! ## One pipeline, one vocabulary
//!
//! Every POST goes through the same steps in the same order, and each refusal
//! is final and plain JSON — never an SSE frame: the origin, the content type,
//! the caller, the JSON-RPC envelope (an `id` is required: a notification
//! would be executed with nobody to answer), the `A2A-Version`, admission,
//! the method, the method's authorization, and the command and task checks
//! that need the params. `dispatch.rs` carries the order and why each step sits
//! where it does.
//!
//! The method names are the route table's ([`crate::runtime::surface::route_of`]):
//! the specification's eleven and the extension methods agentd declares,
//! matched exactly. The public card is not a method — the spec publishes it at
//! `/.well-known/agent-card.json` — and the old spellings (an `a2a.` prefix,
//! the 0.3 names, the card read as a call) are `-32601` like any other unknown
//! name.
//!
//! Most of the table is a2a-rs's to answer. A few calls are answered here:
//! the command ops that reply with a Message (a read has no task for the
//! protocol layer to frame), the push-config listing (a2a-rs 0.10 drops its
//! paging), and the observation feed a display client reads, which a2a-rs
//! correctly does not know. Operator admin is not a method family: `admin.*`
//! rides in as a command DataPart on `SendMessage` and is handled in
//! [`crate::runtime::a2a_server`].
//!
//! What a2a-rs answers passes back through one filter on the way out, so a
//! refusal the runtime made reaches the caller as the runtime made it rather
//! than as a2a-rs reworded it (see [`ports::RequestScope::error`]).
//!
//! ## Signing in
//!
//! With `a2a.device_grant.enabled`, the listener origin is also an OAuth 2.0
//! authorization server ([`crate::a2a::oauth`]): the device grant's endpoints
//! under `/oauth2/`, and its RFC 8414 metadata at the root. They sit behind
//! the same origin gate as `POST /`, and every answer is `no-store`. Without
//! the grant they are not routes at all.
//!
//! The session a sign-in issues is checked on every request, and while a
//! caller's requests are in flight: a revoked session loses the streams it
//! already opened — by its own sid, so a sibling session approved under the
//! same name keeps its own.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use serde_json::json;

use crate::a2a::Principal;
use crate::a2a::ports::{self, RuntimePorts};
use crate::obs::log::Logger;
use crate::runtime::a2a_server::{A2aBridge, SharedFeed};

mod card;
mod cors;
mod dispatch;
mod feed;
mod identity;
/// Per-source failure limits and per-principal admission.
pub(crate) mod limits;

use card::{CardFromRuntime, card, card_preflight};
use cors::preflight;
use dispatch::rpc;
pub use identity::{Auth, PeerId};
use identity::{Peer, peer_identity};

/// Everything the listener needs that is not the bridge.
pub struct Opts {
    pub auth: Auth,
    /// `a2a.cors.origins`: the origins a browser UI may be served from. Any
    /// other `Origin` is refused outright, which is what stops a page the
    /// operator never authorised from driving this endpoint through their
    /// browser.
    ///
    /// Shared rather than owned so a reload can revise the list in place; see
    /// [`OriginList`].
    pub cors_origins: OriginList,
    /// TLS, when the listen URL is `https://` — a PROVIDER consulted per
    /// connection, not a snapshot taken at spawn.
    ///
    /// The `TlsAcceptor` behind it re-stats the mounted identity on a throttle,
    /// so a rotated certificate is picked up by the next handshake. Resolving
    /// it once here defeated that entirely: the accept loop captured the first
    /// `Arc<ServerConfig>` and every later connection reused it, so a
    /// cert-manager renewal needed a restart even though the machinery to
    /// avoid one already existed and was being called — once.
    pub tls: Option<TlsConfigProvider>,
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

impl App {
    /// The origin allowlist in force for this request.
    fn origins(&self) -> Vec<String> {
        self.cors_origins
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

struct App {
    /// a2a-rs's JSON-RPC surface, delegated to for every spec method.
    protocol: Router,
    bridge: Arc<A2aBridge>,
    auth: Auth,
    /// Failed authentications per source. Once a source is over, its bearers
    /// are refused unchecked; a request presenting nothing never is.
    failures: limits::SourceLimiter,
    /// The refusal lines written, bounded per source and overall.
    denials: limits::DenialLog,
    /// Per-principal admission, from each rule's declared rate.
    rates: limits::PrincipalRates,
    /// The browser CORS allowlist, shared so a reload can revise it.
    ///
    /// Frozen at spawn until v1.4.0: an operator who REMOVED an origin to
    /// revoke a web client's access got a successful reload and a listener
    /// that kept honouring the old list — the same shape as the webhook
    /// secret that would not rotate.
    cors_origins: OriginList,
    /// How long a call handed to a2a-rs may take to produce its first byte:
    /// the fidelity filter waits this long for a stream's first event.
    request_timeout: Duration,
    stream_deadline: Duration,
    /// Whether a caller's session is still alive, consulted while its
    /// requests are in flight. `None` on a listener with no sessions.
    liveness: Option<Liveness>,
    log: Logger,
}

/// For a caller, the check that says whether its session is still alive — or
/// `None` when the caller holds no session that could end.
///
/// A revoked session must not keep what it already opened: a stream a2a-rs
/// is serving outlives the request that authenticated it, and a unary call
/// can wait on a turn for minutes. While the check answers `false`, an SSE
/// body ends within one tick and a unary wait is answered with the 401 a
/// revoked token gets.
pub type Liveness = Arc<dyn Fn(&Principal) -> Option<LivenessCheck> + Send + Sync>;

/// One session's liveness: `true` while it may still be served.
pub type LivenessCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Where the listener binds: a TCP authority, or a unix socket path (the
/// co-located-peer transport — same protocol, kernel-authenticated).
pub enum Bind {
    Tcp(String),
    Unix(String),
}

/// Start the listener on its own runtime and thread.
///
/// The runtime is separate from everything else agentd does: the reactor is a
/// blocking single-threaded loop and must stay that way, so the async world is
/// confined to this listener and reaches the runtime only through [`A2aBridge`].
pub fn spawn(
    bind: Bind,
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
    let tls = opts.tls.clone();
    let app = Arc::new(App::new(bridge, opts, updates, log.clone()));
    let _ = feed;
    let router = router(app);

    let bound;
    match &bind {
        Bind::Tcp(authority) => {
            let listener = runtime
                .block_on(tokio::net::TcpListener::bind(authority))
                .map_err(|e| format!("a2a bind {authority}: {e}"))?;
            bound = listener
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| authority.clone());
            runtime.spawn(accept_loop(listener, router, tls, log));
        }
        Bind::Unix(path) => {
            // A stale socket file from a previous life refuses the bind;
            // unlink it first — the flock on the state dir already guarantees
            // there is no OTHER live instance of this agent.
            let _ = std::fs::remove_file(path);
            // Bind inside a 0700 staging directory, then rename into place.
            //
            // `bind` creates the socket with 0777 & ~umask — 0755 under the
            // usual 022 — and narrowing it afterwards leaves a window in which
            // any local user can connect to the DOCUMENTED path. The window is
            // brief and `SO_PEERCRED` still refuses a different uid, so this
            // was defence-in-depth only; but a security control that holds
            // "almost always" is one its own test can lose a race to, and ours
            // did. Staging removes the window instead of shortening it: the
            // socket is unreachable while its directory is 0700, the chmod
            // lands, and the rename publishes an already-correct inode
            // atomically.
            let sock = std::path::Path::new(path);
            let stage = sock
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join(format!(".agentd-sock-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&stage);
            std::fs::create_dir(&stage)
                .map_err(|e| format!("a2a stage {}: {e}", stage.display()))?;
            std::fs::set_permissions(&stage, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .map_err(|e| format!("a2a stage perms: {e}"))?;
            let staged = stage.join("s");
            let bound_listener = {
                let _guard = runtime.enter();
                tokio::net::UnixListener::bind(&staged)
            };
            // Clean up the staging directory on the FAILURE path too. A bind
            // can fail for ordinary reasons — the commonest being a path over
            // SUN_LEN (108 bytes, socket addresses are not PATH_MAX) — and
            // leaving a 0700 directory behind on every failed start would
            // litter the mount the operator chose for the socket.
            let listener = match bound_listener {
                Ok(l) => l,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&stage);
                    return Err(format!("a2a bind {path}: {e}"));
                }
            };
            // 0600: the filesystem is the outer gate (peer-cred is the inner).
            // Applied while the socket is still behind the 0700 directory, so
            // it is never world-connectable at any path.
            let perms = std::fs::set_permissions(
                &staged,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            );
            let published = perms.and_then(|()| std::fs::rename(&staged, sock));
            let _ = std::fs::remove_dir_all(&stage);
            published.map_err(|e| format!("a2a publish {path}: {e}"))?;
            bound = format!("unix:{path}");
            runtime.spawn(accept_loop_unix(listener, router, log));
        }
    }

    Ok(Listener {
        bound,
        sink,
        _runtime: runtime,
    })
}

impl App {
    /// The listener's state, with a2a-rs wired to the runtime's ports.
    fn new(
        bridge: Arc<A2aBridge>,
        opts: Opts,
        updates: Arc<a2a_rs::adapter::InMemoryStreamingHandler>,
        log: Logger,
    ) -> App {
        let ports = RuntimePorts::new(Arc::clone(&bridge), Arc::clone(&updates));
        let adapter = Arc::new(
            a2a_rs::adapter::JsonRpcAdapter::with_handler(
                ports,
                CardFromRuntime(Arc::clone(&bridge)),
            )
            .with_streaming_handler(ports::SharedStreaming(updates)),
        );
        // The liveness hook comes from the sessions: a caller holding one is
        // served only while that session — its sid — lives.
        let liveness = opts
            .auth
            .sessions
            .as_ref()
            .map(|s| crate::a2a::oauth::Sessions::liveness(Arc::clone(s)));
        App {
            protocol: a2a_rs::adapter::jsonrpc_router(adapter),
            bridge,
            auth: opts.auth,
            failures: limits::SourceLimiter::auth_failures(),
            denials: limits::DenialLog::listener(),
            rates: limits::PrincipalRates::default(),
            cors_origins: opts.cors_origins,
            request_timeout: opts.request_timeout,
            stream_deadline: opts.stream_deadline,
            liveness,
            log,
        }
    }
}

/// The listener's routes. The card has one path: `/.well-known/agent.json`
/// was the pre-1.0 name, and a client still asking there is told 404 rather
/// than handed a document it would read with the wrong expectations.
///
/// The authorization server's routes exist only while the device grant does:
/// a listener that issues nothing does not answer as if it might.
fn router(app: Arc<App>) -> Router {
    use crate::a2a::oauth;
    let mut r = Router::new()
        .route("/", post(rpc).options(preflight))
        .route(
            "/.well-known/agent-card.json",
            get(card).options(card_preflight),
        );
    if app.auth.authority.is_some() {
        r = r
            .route(
                oauth::DEVICE_AUTHORIZATION_PATH,
                post(oauth_device_authorization).options(preflight),
            )
            .route(oauth::TOKEN_PATH, post(oauth_token).options(preflight))
            .route(oauth::REVOKE_PATH, post(oauth_revoke).options(preflight))
            .route(
                oauth::VERIFICATION_PATH,
                get(oauth_verification).options(preflight),
            )
            .route(oauth::METADATA_PATH, get(oauth_metadata).options(preflight));
    }
    r.with_state(app)
}

// ---- the authorization server's routes --------------------------------------

/// One OAuth endpoint, as HTTP: the origin gate `POST /` has, a body read to
/// at most [`crate::a2a::oauth::FORM_MAX`] bytes, the endpoint's answer, and
/// `Cache-Control: no-store` on every response — a device code, a token or a
/// refusal of either is never something a cache may keep. What the operator
/// should hear about goes to the feed, and a revocation to the log.
async fn oauth_call(
    app: &App,
    headers: &axum::http::HeaderMap,
    body: Option<axum::body::Body>,
    endpoint: impl FnOnce(
        &crate::a2a::oauth::Authority,
        Option<&str>,
        &[u8],
    ) -> crate::a2a::oauth::Reply,
) -> axum::response::Response {
    use crate::a2a::oauth::{Notice, OAuthError, Reply, ReplyBody};
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;
    let origin = cors::origin_of(headers).map(str::to_string);
    let resp = 'resp: {
        if let Some(o) = &origin
            && !cors::origin_allowed(o, &app.origins())
        {
            break 'resp (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
        let Some(authority) = app.auth.authority.as_deref() else {
            break 'resp StatusCode::NOT_FOUND.into_response();
        };
        let bytes = match body {
            Some(b) => match axum::body::to_bytes(b, crate::a2a::oauth::FORM_MAX).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    let e = OAuthError::new(
                        "invalid_request",
                        format!("the body is over {} bytes", crate::a2a::oauth::FORM_MAX),
                    );
                    break 'resp identity::json_with(
                        StatusCode::BAD_REQUEST,
                        &json!({"error": e.error, "error_description": e.description}),
                    );
                }
            },
            None => Default::default(),
        };
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        let Reply {
            status,
            body,
            retry_after,
            notices,
        } = endpoint(authority, content_type, &bytes);
        for notice in notices {
            match notice {
                Notice::Pending(event) => push_auth_event(app, event),
                Notice::Revoked(s) => {
                    app.log
                        .info("auth.session.revoked", s.revoked_line("oauth2_revoke"));
                    push_auth_event(app, s.revoked_event());
                }
            }
        }
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = match body {
            ReplyBody::Json(v) => identity::json_with(status, &v),
            ReplyBody::Empty => status.into_response(),
            ReplyBody::Text(t) => (
                status,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                t,
            )
                .into_response(),
        };
        if let Some(secs) = retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        resp
    };
    let mut resp = cors::allow_origin(resp, origin.as_deref());
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    resp
}

/// An `auth` event on the observation feed, for operators only: sign-ins
/// waiting on them, and sessions that ended.
fn push_auth_event(app: &App, event: serde_json::Value) {
    if let Some(feed) = app.bridge.feed() {
        feed.push("auth", crate::runtime::a2a_server::FeedVis::Operator, event);
    }
}

/// The address a request is limited by: a TCP peer's.
fn peer_ip(peer: &Peer) -> Option<std::net::IpAddr> {
    peer.source()
}

async fn oauth_device_authorization(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    axum::Extension(peer): axum::Extension<Peer>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let ip = peer_ip(&peer);
    oauth_call(&app, &headers, Some(body), |a, ct, b| {
        a.device_authorization(ct, b, ip)
    })
    .await
}

async fn oauth_token(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    axum::Extension(peer): axum::Extension<Peer>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let ip = peer_ip(&peer);
    oauth_call(&app, &headers, Some(body), |a, ct, b| a.token(ct, b, ip)).await
}

async fn oauth_revoke(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    oauth_call(&app, &headers, Some(body), |a, ct, b| a.revoke(ct, b)).await
}

async fn oauth_metadata(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    oauth_call(&app, &headers, None, |a, _, _| a.metadata()).await
}

async fn oauth_verification(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    oauth_call(&app, &headers, None, |a, _, _| a.verification()).await
}

/// The live browser-origin allowlist, revisable by a reload.
///
/// A plain `Vec` behind a lock rather than a provider closure: the list has no
/// source to re-consult, it is simply replaced.
pub type OriginList = Arc<std::sync::RwLock<Vec<String>>>;

/// Supplies the current rustls configuration for each inbound connection.
///
/// A closure rather than the acceptor type so this module stays independent of
/// how the identity is sourced (mounted files today, something else later).
pub type TlsConfigProvider = Arc<dyn Fn() -> Arc<tokio_rustls::rustls::ServerConfig> + Send + Sync>;

/// Accept connections forever, terminating TLS when configured, and serve each
/// with the router. The verified peer identity is attached to every request on
/// that connection, which is how a `san`/`sub` principal rule sees a client cert.
async fn accept_loop(
    listener: tokio::net::TcpListener,
    router: Router,
    tls: Option<TlsConfigProvider>,
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
                Some(provider) => {
                    // Per connection: a rotated identity takes effect on the
                    // next handshake rather than the next restart.
                    let acceptor = tokio_rustls::TlsAcceptor::from(provider());
                    match acceptor.accept(sock).await {
                        Ok(stream) => {
                            let peer_id = peer_identity(stream.get_ref().1);
                            serve_conn(stream, router, peer_id, Peer::Tcp(peer), log).await;
                        }
                        Err(e) => log.debug("a2a.tls", json!({"err": e.to_string()})),
                    }
                }
                None => serve_conn(sock, router, PeerId::default(), Peer::Tcp(peer), log).await,
            }
        });
    }
}

/// Accept unix-socket connections forever. No TLS layer: the KERNEL is the
/// authenticator — `SO_PEERCRED` names the calling process's uid, and only the
/// daemon's own user (or root) gets past this gate. That is strictly stronger
/// than loopback TCP (which every local user can dial), so every connection
/// that gets through is the operator, whatever else is configured.
async fn accept_loop_unix(listener: tokio::net::UnixListener, router: Router, log: Logger) {
    let me = unsafe { libc::geteuid() };
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        let uid = match sock.peer_cred() {
            Ok(cred) => cred.uid(),
            Err(e) => {
                log.warn("a2a.unix.denied", json!({"err": e.to_string()}));
                continue;
            }
        };
        if uid != me && uid != 0 {
            log.warn("a2a.unix.denied", json!({"uid": uid, "reason": "peer uid"}));
            continue;
        }
        let router = router.clone();
        let log = log.clone();
        tokio::spawn(async move {
            serve_conn(sock, router, PeerId::default(), Peer::Unix, log).await;
        });
    }
}

async fn serve_conn<S>(stream: S, router: Router, peer_id: PeerId, peer: Peer, log: Logger)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Every request on this connection carries the connection's evidence.
    let router = router
        .layer(axum::Extension(peer_id))
        .layer(axum::Extension(peer));
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

#[cfg(test)]
mod tests {
    use super::identity::{Peer, PeerId};
    use super::*;
    use crate::runtime::a2a_server::A2aRequest;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::response::Response;
    use futures_util::StreamExt;
    use serde_json::Value;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;
    use tower::ServiceExt;

    /// A listener over a stand-in reactor that answers each request with
    /// `answer`, each on its own thread so a slow answer holds up no other.
    /// Loopback-bound with nothing configured, so the test's local caller is
    /// the implicit operator. Also returns how many requests reached the
    /// runtime, which is how a refusal proves it cost the runtime nothing.
    fn listener(
        answer: impl Fn(&A2aRequest) -> Value + Send + Sync + 'static,
        liveness: Option<Liveness>,
    ) -> (Router, Arc<AtomicUsize>) {
        listener_with(answer, Auth::default(), Vec::new(), liveness)
    }

    /// [`listener`] with the sessions and authorization server `auth` holds,
    /// and a browser origin allowlist. The liveness hook is the one the
    /// sessions install, unless `liveness` overrides it.
    fn listener_with(
        answer: impl Fn(&A2aRequest) -> Value + Send + Sync + 'static,
        auth: Auth,
        origins: Vec<String>,
        liveness: Option<Liveness>,
    ) -> (Router, Arc<AtomicUsize>) {
        let resolver = crate::a2a::Resolver::build(
            &serde_json::from_value(json!({"listen": "http://127.0.0.1:0"})).unwrap(),
            &|_| None,
        )
        .unwrap();
        let reached = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reached);
        let answer = Arc::new(answer);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(crate::runtime::events::Event::A2a(req)) = rx.recv() {
                counted.fetch_add(1, Ordering::SeqCst);
                let answer = Arc::clone(&answer);
                std::thread::spawn(move || {
                    let _ = req.reply.send(answer(&req));
                });
            }
        });
        let log = Logger::new(
            crate::obs::log::LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: crate::obs::log::Comp::Supervisor,
                pid: std::process::id(),
                trace_id: None,
            },
            crate::obs::log::Level::Error,
        );
        let opts = Opts {
            auth,
            cors_origins: Arc::new(std::sync::RwLock::new(origins)),
            tls: None,
            request_timeout: Duration::from_secs(5),
            stream_deadline: Duration::from_secs(5),
        };
        let updates = Arc::new(a2a_rs::adapter::InMemoryStreamingHandler::new());
        let mut app = App::new(A2aBridge::new(tx, resolver), opts, updates, log);
        if liveness.is_some() {
            app.liveness = liveness;
        }
        let router = router(Arc::new(app))
            .layer(axum::Extension(PeerId::default()))
            .layer(axum::Extension(Peer::Tcp(
                "127.0.0.1:5000".parse().unwrap(),
            )));
        (router, reached)
    }

    /// A POST of `body` with the spec's headers, minus any named in `drop`,
    /// plus `extra`.
    async fn post(router: &Router, body: &str, drop: &[&str], extra: &[(&str, &str)]) -> Response {
        let mut req = Request::builder().method("POST").uri("/");
        for (k, v) in [("content-type", "application/json"), ("a2a-version", "1.0")] {
            if !drop.contains(&k) {
                req = req.header(k, v);
            }
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        router
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn json_of(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&bytes)))
    }

    fn task_not_found() -> Value {
        json!({"_error": {"code": -32001, "message": "task not found"}})
    }

    /// Every POST states the protocol it speaks. A missing header, an empty
    /// one and any version but 1.0 are -32009 at HTTP 200, with the id echoed
    /// and the version this listener does speak — and nothing reaches the
    /// runtime. A 1.0 client with any patch is let through.
    #[tokio::test]
    async fn version_gate() {
        let (router, reached) = listener(|_| task_not_found(), None);
        let body = r#"{"jsonrpc":"2.0","id":"v1","method":"GetTask","params":{"id":"task-1"}}"#;
        for header in [
            None,
            Some(""),
            Some("0.3"),
            Some("1.1"),
            Some("2.0"),
            Some("1"),
        ] {
            let extra: Vec<(&str, &str)> = header.map(|h| ("a2a-version", h)).into_iter().collect();
            let resp = post(&router, body, &["a2a-version"], &extra).await;
            assert_eq!(resp.status(), StatusCode::OK, "{header:?}");
            let v = json_of(resp).await;
            assert_eq!(v["id"], "v1", "{header:?}: {v}");
            assert_eq!(v["error"]["code"], -32009, "{header:?}: {v}");
            let info = &v["error"]["data"][0];
            assert_eq!(info["reason"], "VERSION_NOT_SUPPORTED", "{v}");
            assert_eq!(info["domain"], "a2a-protocol.org", "{v}");
            assert_eq!(info["metadata"]["supportedVersions"], "1.0", "{v}");
        }
        // Before the route table: a 0.3 client asking for a 0.3 method is
        // told about the version, which is what it can act on.
        let v = json_of(
            post(
                &router,
                r#"{"jsonrpc":"2.0","id":2,"method":"tasks/get"}"#,
                &["a2a-version"],
                &[],
            )
            .await,
        )
        .await;
        assert_eq!(v["error"]["code"], -32009, "{v}");
        assert_eq!(
            reached.load(Ordering::SeqCst),
            0,
            "a refusal reached the runtime"
        );

        for ok in ["1.0", "1.0.3", " 1.0 "] {
            let v =
                json_of(post(&router, body, &["a2a-version"], &[("a2a-version", ok)]).await).await;
            assert_eq!(v["error"]["code"], -32001, "{ok:?} is let through: {v}");
        }
        assert!(reached.load(Ordering::SeqCst) > 0);
    }

    /// Every malformed envelope is refused before it can be anything else,
    /// and none reaches the runtime. A request with no `id` — a JSON-RPC
    /// notification — and one with `id: null` are refused rather than run: a
    /// send nobody can be answered about is a task nobody can find.
    #[tokio::test]
    async fn envelope_validation() {
        let (router, reached) = listener(|_| task_not_found(), None);
        let cases: &[(&str, i64, Value)] = &[
            ("not json", -32700, Value::Null),
            ("[]", -32600, Value::Null),
            (
                r#"[{"jsonrpc":"2.0","id":1,"method":"GetTask"}]"#,
                -32600,
                Value::Null,
            ),
            (r#""GetTask""#, -32600, Value::Null),
            ("7", -32600, Value::Null),
            (r#"{"id":1,"method":"GetTask"}"#, -32600, json!(1)),
            (
                r#"{"jsonrpc":"1.0","id":1,"method":"GetTask"}"#,
                -32600,
                json!(1),
            ),
            (
                r#"{"jsonrpc":2.0,"id":1,"method":"GetTask"}"#,
                -32600,
                json!(1),
            ),
            (r#"{"jsonrpc":"2.0","id":1}"#, -32600, json!(1)),
            (r#"{"jsonrpc":"2.0","id":1,"method":""}"#, -32600, json!(1)),
            (r#"{"jsonrpc":"2.0","id":1,"method":7}"#, -32600, json!(1)),
            // No id, a null id, and ids JSON-RPC's integer-or-string rule
            // does not admit.
            (
                r#"{"jsonrpc":"2.0","method":"GetTask","params":{"id":"t"}}"#,
                -32600,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","id":null,"method":"GetTask"}"#,
                -32600,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1.5,"method":"GetTask"}"#,
                -32600,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","id":{},"method":"GetTask"}"#,
                -32600,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","id":[1],"method":"GetTask"}"#,
                -32600,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","id":true,"method":"GetTask"}"#,
                -32600,
                Value::Null,
            ),
            // Params, when present, are an object.
            (
                r#"{"jsonrpc":"2.0","id":"p","method":"GetTask","params":[]}"#,
                -32602,
                json!("p"),
            ),
            (
                r#"{"jsonrpc":"2.0","id":"p","method":"GetTask","params":"t"}"#,
                -32602,
                json!("p"),
            ),
            (
                r#"{"jsonrpc":"2.0","id":"p","method":"GetTask","params":null}"#,
                -32602,
                json!("p"),
            ),
            (
                r#"{"jsonrpc":"2.0","id":3,"method":"SendMessage","params":7}"#,
                -32602,
                json!(3),
            ),
        ];
        for (body, code, id) in cases {
            let resp = post(&router, body, &[], &[]).await;
            assert_eq!(resp.status(), StatusCode::OK, "{body}");
            let v = json_of(resp).await;
            assert_eq!(v["error"]["code"], *code, "{body}: {v}");
            assert_eq!(v["id"], *id, "{body}: {v}");
        }
        let v =
            json_of(post(&router, r#"{"jsonrpc":"2.0","method":"GetTask"}"#, &[], &[]).await).await;
        assert_eq!(
            v["error"]["message"], "A2A requests must carry an id",
            "{v}"
        );
        let v = json_of(post(&router, "[]", &[], &[]).await).await;
        assert_eq!(
            v["error"]["message"], "batch requests are not supported",
            "{v}"
        );

        // The content type comes first, and its refusal has no body at all.
        for ct in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let extra: Vec<(&str, &str)> = ct.map(|c| ("content-type", c)).into_iter().collect();
            let resp = post(
                &router,
                r#"{"jsonrpc":"2.0","id":1,"method":"GetTask"}"#,
                &["content-type"],
                &extra,
            )
            .await;
            assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "{ct:?}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(body.is_empty(), "{ct:?}");
        }
        assert_eq!(
            reached.load(Ordering::SeqCst),
            0,
            "a refusal reached the runtime"
        );

        // With parameters and in any case, JSON is JSON.
        let v = json_of(
            post(
                &router,
                r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"t"}}"#,
                &["content-type"],
                &[("content-type", "Application/JSON; charset=utf-8")],
            )
            .await,
        )
        .await;
        assert_eq!(v["error"]["code"], -32001, "{v}");
    }

    /// An error envelope as a2a-rs writes it.
    fn sdk_error(code: i64, message: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": code, "message": message,
            "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "JSON_RPC_ERROR", "domain": "a2a-rs"}]}})
    }

    fn unary_response(v: &Value) -> Response {
        axum::response::IntoResponse::into_response(axum::Json(v.clone()))
    }

    fn sse_response(body: &'static str) -> Response {
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    fn fidelity(kept: Option<Value>, bearer_used: bool) -> dispatch::Fidelity {
        dispatch::Fidelity {
            kept,
            bearer_used,
            list_tasks: false,
        }
    }

    async fn filtered(resp: Response, f: dispatch::Fidelity) -> Response {
        dispatch::faithful(resp, f, Duration::from_secs(5), None).await
    }

    /// What a2a-rs answers reaches the caller in the runtime's words: the
    /// runtime's error object replaces a2a-rs's rendering of it byte for
    /// byte, with the status its code travels with; an error a2a-rs raised
    /// on its own keeps a2a-rs's answer, minus the codes it invented; and a
    /// stream that opens with an error is answered as JSON.
    #[tokio::test]
    async fn error_fidelity_filter() {
        let draining = json!({"code": -32603, "message": "the agent is draining",
            "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "DRAINING", "domain": "agentd.dev"}]});
        let resp = filtered(
            unary_response(&sdk_error(-32603, "JSON-RPC error: -32603 - x")),
            fidelity(Some(draining.clone()), false),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_of(resp).await;
        assert_eq!(v["error"], draining, "the runtime's object, whole: {v}");
        assert_eq!(v["id"], 1);

        // A kept refusal of the caller carries its status and its challenge.
        let denied = json!({"code": -31403, "message": "admin.drain is not permitted for user:a"});
        let resp = filtered(
            unary_response(&sdk_error(-31403, "JSON-RPC error: -31403 - admin.drain")),
            fidelity(Some(denied.clone()), true),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            resp.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("error=\"insufficient_scope\"")
        );
        assert_eq!(json_of(resp).await["error"], denied);

        // a2a-rs's own error: its band of invented codes is folded, and the
        // rest is left as a2a-rs wrote it.
        let v = json_of(
            filtered(
                unary_response(&sdk_error(-32102, "Context belongs to another principal")),
                fidelity(None, false),
            )
            .await,
        )
        .await;
        assert_eq!(v["error"]["code"], -32603, "{v}");
        let own = sdk_error(-32602, "Invalid params: x");
        let v = json_of(filtered(unary_response(&own), fidelity(None, false)).await).await;
        assert_eq!(v, own);
        // A kept refusal is not borrowed by an error with another code.
        let v = json_of(
            filtered(
                unary_response(&own),
                fidelity(Some(draining.clone()), false),
            )
            .await,
        )
        .await;
        assert_eq!(v, own);

        // A stream whose first event is an error is a JSON answer instead.
        let resp = filtered(
            sse_response("data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32603,\"message\":\"JSON-RPC error: -32603 - x\"}}\n\n"),
            fidelity(Some(draining.clone()), false),
        )
        .await;
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(json_of(resp).await["error"], draining);

        // A normal first event goes out unchanged, `id:` and all; a later
        // error frame is restored in place and keeps its own `id:`.
        let resp = filtered(
            sse_response(concat!(
                ":keep-alive\n\n",
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"task\":{\"id\":\"t\"}}}\nid: 3\n\n",
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32102,\"message\":\"Context belongs to another principal\"}}\nid: 4\n\n",
            )),
            fidelity(None, false),
        )
        .await;
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.starts_with(concat!(
                ":keep-alive\n\n",
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"task\":{\"id\":\"t\"}}}\nid: 3\n\n",
            )),
            "{body}"
        );
        let last = body.split("\n\n").nth(2).unwrap();
        assert!(last.ends_with("\nid: 4"), "{last}");
        let frame: Value =
            serde_json::from_str(last.lines().next().unwrap().strip_prefix("data: ").unwrap())
                .unwrap();
        assert_eq!(
            frame["error"],
            json!({"code": -32603, "message": "internal error"})
        );

        // The last page of a listing says so, as the spec's response does.
        let page = json!({"jsonrpc": "2.0", "id": 1, "result": {"pageSize": 50}});
        let resp = dispatch::faithful(
            unary_response(&page),
            dispatch::Fidelity {
                kept: None,
                bearer_used: false,
                list_tasks: true,
            },
            Duration::from_secs(5),
            None,
        )
        .await;
        let v = json_of(resp).await;
        assert_eq!(v["result"]["nextPageToken"], "", "{v}");
        assert_eq!(v["result"]["tasks"], json!([]), "{v}");
        let v = json_of(filtered(unary_response(&page), fidelity(None, false)).await).await;
        assert_eq!(v, page, "only a listing is completed");
    }

    /// A subscription is read first, by the listener, as the caller: a task
    /// the runtime will not show this caller is refused as JSON, and a2a-rs —
    /// which in 0.10 opens a stream on a task it cannot find — is never asked.
    /// The stand-in shows the pre-check alone the task as missing, so only
    /// the listener's own read can be what refuses it.
    #[tokio::test]
    async fn a_subscription_is_read_first_as_the_caller() {
        let reads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&reads);
        let (router, _) = listener(
            move |req| {
                seen.lock().unwrap().push((
                    req.method.clone(),
                    req.params.clone(),
                    req.principal.id.clone(),
                ));
                if req.params.get("historyLength").is_some() {
                    return task_not_found();
                }
                json!({"id": req.params["id"], "contextId": "ctx-1",
                    "status": {"state": "TASK_STATE_WORKING"}})
            },
            None,
        );
        let resp = post(
            &router,
            r#"{"jsonrpc":"2.0","id":9,"method":"SubscribeToTask","params":{"id":"task-1"}}"#,
            &[],
            &[],
        )
        .await;
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/json");
        let v = json_of(resp).await;
        assert_eq!(v["error"]["code"], -32001, "{v}");
        assert_eq!(v["id"], 9, "{v}");
        let reads = reads.lock().unwrap().clone();
        assert_eq!(reads.len(), 1, "only the listener's read: {reads:?}");
        let (verb, params, who) = &reads[0];
        assert_eq!(verb, "GetTask");
        assert_eq!(params, &json!({"id": "task-1", "historyLength": 0}));
        assert_eq!(who, "operator", "read as the caller");
    }

    /// A caller whose session dies loses what it already opened: a stream
    /// a2a-rs is serving ends within a tick, and a unary call still waiting
    /// on the runtime is answered with the 401 a revoked token gets.
    #[tokio::test]
    async fn liveness_hook_ends_streams() {
        let alive = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&alive);
        let liveness: Liveness = Arc::new(move |_| {
            let flag = Arc::clone(&flag);
            Some(Arc::new(move || flag.load(Ordering::SeqCst)) as LivenessCheck)
        });
        let (router, _) = listener(
            |req| {
                if req.method == "GetTask" && req.params["id"] == "task-slow" {
                    std::thread::sleep(Duration::from_secs(3));
                }
                json!({"id": req.params["id"], "contextId": "ctx-1",
                    "status": {"state": "TASK_STATE_WORKING"}})
            },
            Some(liveness),
        );

        // A subscription to a running task: the snapshot, then silence.
        let resp = post(
            &router,
            r#"{"jsonrpc":"2.0","id":1,"method":"SubscribeToTask","params":{"id":"task-1"}}"#,
            &[],
            &[],
        )
        .await;
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut body = resp.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("the snapshot")
            .expect("a frame")
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("task-1"));
        // Still open while the session lives.
        assert!(
            tokio::time::timeout(Duration::from_millis(250), body.next())
                .await
                .is_err(),
            "the stream ended with its session alive"
        );
        alive.store(false, Ordering::SeqCst);
        let revoked_at = Instant::now();
        let end = tokio::time::timeout(Duration::from_secs(2), async {
            while body.next().await.is_some() {}
        })
        .await;
        assert!(end.is_ok(), "the stream outlived its session");
        assert!(
            revoked_at.elapsed() <= Duration::from_millis(200),
            "ended {:?} after the revocation",
            revoked_at.elapsed()
        );

        // A unary call waiting on the runtime.
        alive.store(true, Ordering::SeqCst);
        let flip = Arc::clone(&alive);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            flip.store(false, Ordering::SeqCst);
        });
        let started = Instant::now();
        let resp = post(
            &router,
            r#"{"jsonrpc":"2.0","id":2,"method":"GetTask","params":{"id":"task-slow"}}"#,
            &[],
            &[],
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "it waited out the runtime"
        );
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(www.contains("error=\"invalid_token\""), "{www}");
        assert!(
            www.contains("the session token is unknown, expired or revoked"),
            "{www}"
        );
    }

    // ---- the authorization server and its sessions ----------------------

    use crate::a2a::oauth::{self, Authority, DeviceGrant, Session, SessionKind, Sessions};

    /// An authority over fresh sessions, issuing at `issuer`.
    fn authority(cfg: Value, issuer: &str) -> Arc<Authority> {
        let cfg: crate::config::v2::DeviceGrant = serde_json::from_value(cfg).unwrap();
        let sessions = Arc::new(Sessions::new(oauth::system_clock()));
        let a = Arc::new(Authority::new(
            DeviceGrant::new(&cfg, oauth::system_clock(), oauth::os_mint()),
            sessions,
        ));
        a.set_issuer(issuer);
        a
    }

    fn device_session(sid: &str, name: &str) -> Session {
        Session {
            sid: sid.into(),
            kind: SessionKind::Device,
            name: Some(name.into()),
            role: crate::config::v2::Role::User,
            principal: format!("user:{name}"),
            client_id: "cli".into(),
            created_ms: crate::state::now_ms(),
            expires_ms: None,
            approved_by: "operator".into(),
            approved_rule: None,
            rate: None,
        }
    }

    async fn send(router: &Router, req: Request<Body>) -> Response {
        router.clone().oneshot(req).await.unwrap()
    }

    fn form_post(path: &str, body: &str, origin: Option<&str>) -> Request<Body> {
        let mut req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded");
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    /// A session token the sessions no longer hold is the session variant of
    /// the 401 — `invalid_token`, "unknown, expired or revoked" — and nothing
    /// reaches the runtime. A live one is its session's principal, with its
    /// sid, and stops being one the moment it is revoked.
    #[tokio::test]
    async fn dead_session_token_is_401_invalid_token() {
        let sessions = Arc::new(Sessions::new(oauth::system_clock()));
        sessions.insert(
            "agentd_at_live",
            device_session("ds_0123456789abcdef", "alice"),
        );
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let who = Arc::clone(&seen);
        let (router, reached) = listener_with(
            move |req| {
                who.lock()
                    .unwrap()
                    .push((req.principal.id.clone(), req.principal.session.clone()));
                task_not_found()
            },
            Auth {
                sessions: Some(Arc::clone(&sessions)),
                authority: None,
            },
            Vec::new(),
            None,
        );
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"t"}}"#;
        let v = json_of(
            post(
                &router,
                body,
                &[],
                &[("authorization", "Bearer agentd_at_live")],
            )
            .await,
        )
        .await;
        assert_eq!(
            v["error"]["code"], -32001,
            "the live session is let in: {v}"
        );
        assert_eq!(
            seen.lock().unwrap()[0],
            (
                "user:alice".to_string(),
                Some("ds_0123456789abcdef".to_string())
            )
        );
        sessions.revoke(&oauth::Revoke::Sid("ds_0123456789abcdef".into()));
        for token in ["agentd_at_live", "agentd_at_never"] {
            let auth = format!("Bearer {token}");
            let resp = post(&router, body, &[], &[("authorization", &auth)]).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{token}");
            let www = resp.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .to_string();
            assert!(www.contains("error=\"invalid_token\""), "{www}");
            assert!(www.contains("unknown, expired or revoked"), "{www}");
        }
        assert_eq!(
            reached.load(Ordering::SeqCst),
            1,
            "only the live call reached the runtime"
        );
    }

    /// The authorization server's routes sit behind the origin gate `POST /`
    /// has — an unlisted origin is refused with no grant and no body, a
    /// listed one gets its grant — and every answer, a refusal included, is
    /// `no-store`. Without the grant they are not routes.
    #[tokio::test]
    async fn oauth_routes_share_the_origin_gate_and_are_no_store() {
        let a = authority(json!({"enabled": true}), "https://agent.example");
        let (router, reached) = listener_with(
            |_| task_not_found(),
            Auth {
                sessions: Some(Arc::clone(a.sessions())),
                authority: Some(Arc::clone(&a)),
            },
            vec!["https://ui.example".into()],
            None,
        );
        let no_store = |r: &Response| {
            assert_eq!(
                r.headers()
                    .get(header::CACHE_CONTROL)
                    .map(|v| v.to_str().unwrap()),
                Some("no-store"),
                "{:?}",
                r.headers()
            );
        };
        let posts = [
            oauth::DEVICE_AUTHORIZATION_PATH,
            oauth::TOKEN_PATH,
            oauth::REVOKE_PATH,
        ];
        let gets = [oauth::METADATA_PATH, oauth::VERIFICATION_PATH];
        let req = |path: &str, origin: Option<&str>| {
            if gets.contains(&path) {
                let mut r = Request::builder().uri(path);
                if let Some(o) = origin {
                    r = r.header("origin", o);
                }
                r.body(Body::empty()).unwrap()
            } else {
                form_post(path, "client_id=cli", origin)
            }
        };
        for path in posts.iter().chain(gets.iter()) {
            // An origin nobody listed: 403, no grant, no body.
            let r = send(&router, req(path, Some("https://evil.example"))).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{path}");
            assert!(
                r.headers()
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .is_none(),
                "{path}"
            );
            no_store(&r);
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(body.is_empty(), "{path}");
            // A listed one: answered, granted, not cached.
            let r = send(&router, req(path, Some("https://ui.example"))).await;
            assert_ne!(r.status(), StatusCode::FORBIDDEN, "{path}");
            assert_eq!(
                r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                "https://ui.example",
                "{path}"
            );
            no_store(&r);
            // No origin at all: a terminal client.
            let r = send(&router, req(path, None)).await;
            assert_ne!(r.status(), StatusCode::NOT_FOUND, "{path}");
            no_store(&r);
            // The preflight follows the same list.
            let pre = |o: &str| {
                Request::builder()
                    .method("OPTIONS")
                    .uri(*path)
                    .header("origin", o)
                    .header("access-control-request-method", "POST")
                    .body(Body::empty())
                    .unwrap()
            };
            assert_eq!(
                send(&router, pre("https://ui.example")).await.status(),
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                send(&router, pre("https://evil.example")).await.status(),
                StatusCode::FORBIDDEN
            );
        }
        // A device authorization is a JSON answer; the page is plain text.
        let r = send(
            &router,
            form_post(oauth::DEVICE_AUTHORIZATION_PATH, "client_id=cli", None),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let v = json_of(r).await;
        assert_eq!(
            v["verification_uri"], "https://agent.example/oauth2/device",
            "{v}"
        );
        let r = send(&router, req(oauth::VERIFICATION_PATH, None)).await;
        assert!(
            r.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        // A body over the cap is refused as the form rules say.
        let big = format!("client_id=cli&pad={}", "x".repeat(oauth::FORM_MAX));
        let r = send(
            &router,
            form_post(oauth::DEVICE_AUTHORIZATION_PATH, &big, None),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        no_store(&r);
        assert_eq!(json_of(r).await["error"], "invalid_request");
        assert_eq!(
            reached.load(Ordering::SeqCst),
            0,
            "sign-in never reaches the runtime"
        );

        // Without the grant: not routes.
        let (plain, _) = listener(|_| task_not_found(), None);
        for path in posts.iter().chain(gets.iter()) {
            let r = send(&plain, req(path, None)).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    /// RFC 8414 round trip from the card: the `oauth2MetadataUrl` an https
    /// card publishes is the issuer's well-known path, the document there
    /// names that issuer — the listener origin — and every endpoint in it is
    /// the issuer joined with its path, the same URLs the card's flow names.
    #[tokio::test]
    async fn metadata_round_trips_rfc8414() {
        let a2a: crate::config::v2::A2a = serde_json::from_value(json!({
            "listen": "https://0.0.0.0:8443", "url": "https://agent.example:8443",
            "tls": {"cert": "c", "key": "k"}, "bearer": "{{secret:B}}",
            "device_grant": {"enabled": true, "scopes": ["user", "operator"]}}))
        .unwrap();
        let origin = crate::runtime::surface::auth::origin_of(a2a.url.as_deref().unwrap()).unwrap();
        let posture = crate::runtime::surface::auth::listener_auth_of(&a2a);
        let security = crate::runtime::surface::auth::security_of(
            &posture,
            Some(&origin),
            &a2a.device_grant.scopes,
        )
        .expect("a declared scheme");
        let scheme = serde_json::to_value(&security.schemes["device_code"]).unwrap();
        fn find<'v>(v: &'v Value, key: &str) -> Option<&'v Value> {
            match v {
                Value::Object(m) => m.get(key).or_else(|| m.values().find_map(|x| find(x, key))),
                Value::Array(a) => a.iter().find_map(|x| find(x, key)),
                _ => None,
            }
        }
        let metadata_url = find(&scheme, "oauth2MetadataUrl")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no metadata URL on an https card: {scheme}"))
            .to_string();

        let a = authority(
            json!({"enabled": true, "scopes": ["user", "operator"]}),
            &origin,
        );
        let (router, _) = listener_with(
            |_| task_not_found(),
            Auth {
                sessions: Some(Arc::clone(a.sessions())),
                authority: Some(a),
            },
            Vec::new(),
            None,
        );
        let path = metadata_url
            .strip_prefix(&origin)
            .expect("the card's URL is on the origin");
        let r = send(
            &router,
            Request::builder().uri(path).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let m = json_of(r).await;
        let issuer = m["issuer"].as_str().unwrap();
        assert_eq!(issuer, origin);
        assert_eq!(
            crate::runtime::surface::auth::join(issuer, oauth::METADATA_PATH),
            metadata_url
        );
        for (field, path) in [
            (
                "device_authorization_endpoint",
                oauth::DEVICE_AUTHORIZATION_PATH,
            ),
            ("token_endpoint", oauth::TOKEN_PATH),
            ("revocation_endpoint", oauth::REVOKE_PATH),
        ] {
            assert_eq!(
                m[field].as_str(),
                Some(crate::runtime::surface::auth::join(issuer, path).as_str()),
                "{field}: {m}"
            );
        }
        assert_eq!(
            find(&scheme, "deviceAuthorizationUrl"),
            Some(&m["device_authorization_endpoint"]),
            "{scheme}"
        );
        assert_eq!(
            find(&scheme, "tokenUrl"),
            Some(&m["token_endpoint"]),
            "{scheme}"
        );
    }

    /// The feed ends a revoked caller's stream with `goodbye{reason:
    /// "revoked"}` within a tick — through the hook the sessions install.
    #[tokio::test]
    async fn a_revoked_session_is_said_goodbye_on_the_feed() {
        let feed = Arc::new(crate::runtime::a2a_server::SharedFeed::new(false));
        let sessions = Arc::new(Sessions::new(oauth::system_clock()));
        sessions.insert(
            "agentd_at_feed",
            device_session("ds_feedfeedfeedfeed", "bob"),
        );
        let alive = Sessions::liveness(Arc::clone(&sessions));
        let principal =
            match crate::a2a::principals::SessionVerifier::verify(&*sessions, "agentd_at_feed") {
                crate::a2a::principals::SessionCheck::Valid(p) => p,
                _ => unreachable!(),
            };
        let check = alive(&principal);
        let resp = feed::feed_stream(
            feed,
            json!(7),
            json!({}),
            principal,
            Duration::from_secs(30),
            check,
        );
        let mut body = resp.into_body().into_data_stream();
        let hello = body.next().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&hello).contains("hello"));
        sessions.revoke(&oauth::Revoke::Name("bob".into()));
        let at = Instant::now();
        let bye = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let chunk = body
                    .next()
                    .await
                    .expect("a goodbye before the end")
                    .unwrap();
                let text = String::from_utf8_lossy(&chunk).to_string();
                if text.contains("goodbye") {
                    return text;
                }
            }
        })
        .await
        .expect("the stream outlived its session");
        assert!(bye.contains("\"reason\":\"revoked\""), "{bye}");
        assert!(
            at.elapsed() <= Duration::from_millis(200),
            "{:?}",
            at.elapsed()
        );
        assert!(body.next().await.is_none(), "nothing after the goodbye");
    }
}
