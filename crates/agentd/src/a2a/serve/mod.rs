// SPDX-License-Identifier: AGPL-3.0-only
//! **The A2A listener.**
//!
//! What arrives here is HTTP; what leaves is a decision about *who is calling*.
//! Everything after that — parsing the JSON-RPC envelope, typing the params,
//! dispatching the method, framing SSE, mapping errors to the spec's codes — is
//! the A2A project's official server's ([`a2a_server`]), reached by handing it
//! the request with the authenticated caller in scope. agentd's job is the two
//! things a protocol crate cannot know: which caller a connection represents,
//! and which of them may do what — and, behind the SDK's handler trait, what
//! each operation does ([`crate::a2a::ports`]).
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
//! `/.well-known/agent-card.json` — and any other name, an earlier
//! specification's included, is `-32601`.
//!
//! Most of the table is the SDK's to answer. A few calls are answered here:
//! the command ops that reply with a Message (a read has no task to track),
//! and the observation feed a display client reads, which the SDK correctly
//! does not know. Operator admin is not a method family: `admin.*` rides in as
//! a command DataPart on `SendMessage` and is handled in
//! [`crate::runtime::a2a_server`].
//!
//! What the SDK answers passes back through one filter on the way out, so a
//! refusal the runtime made reaches the caller as the runtime made it rather
//! than as the SDK reworded it (see [`ports::RequestScope::error`]).
//!
//! ## Signing in
//!
//! With `a2a.device_grant.enabled`, the listener origin is also an OAuth 2.0
//! authorization server ([`crate::a2a::oauth`]): the device grant's endpoints
//! under `/oauth2/`, and its RFC 8414 metadata at the root. They sit behind
//! the same origin gate as `POST /`, and every answer is `no-store`. Without
//! the grant they are not routes at all — except that a launcher's slot
//! (`agentd tui` / `agentd ui`) mounts the token and revocation endpoints for
//! the launch grant, and, for a launched web UI, the endpoint its tab asks
//! the launcher's terminal to sign it in at.
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

use card::{card, card_preflight};
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
    /// Public JSON documents served at their paths with a `GET`: the client
    /// metadata document and key set a Company fetches (RFC 0047 §5.5.2),
    /// when `security.companies.client.serve` says so, and agentd's Client ID
    /// Metadata Document and key set an authorization server fetches (RFC
    /// 0052), when `security.client.serve` does. Mounted only then.
    pub documents: Vec<(String, Vec<u8>)>,
    /// The path of the Direct Sign-In callback (RFC 0047 §5.7), when a
    /// Company may be signed in to directly and the identity is served
    /// here. Mounted only then.
    pub callback: Option<String>,
}

/// How long a Company or an authorization server may cache a served document.
const DOCUMENT_MAX_AGE: &str = "max-age=300";

/// A running listener. Dropping it stops serving.
pub struct Listener {
    /// The authority actually bound (a `:0` request resolves to a real port).
    pub bound: String,
    /// Where the reactor publishes task transitions for subscribers.
    pub sink: Arc<ports::StreamSink>,
    /// Where an instance child's connections are served (`a2a::channel`).
    pub door: Door,
    /// Kept alive because dropping the runtime stops the accept loop.
    _runtime: tokio::runtime::Runtime,
}

/// The listener's door for instance children: a connection a child opened
/// through the channel its parent handed it at spawn is served here, by the
/// same router as every other, as the child's principal `instance:<handle>`.
#[derive(Clone)]
pub struct Door {
    handle: tokio::runtime::Handle,
    router: Router,
    log: Logger,
}

impl Door {
    /// Serve `stream`, a connection instance child `child` opened through
    /// its channel.
    pub fn serve_instance(&self, stream: std::os::unix::net::UnixStream, child: &str) {
        let (router, log, child) = (
            self.router.clone(),
            self.log.clone(),
            Arc::<str>::from(child),
        );
        self.handle.spawn(async move {
            if stream.set_nonblocking(true).is_err() {
                return;
            }
            let Ok(s) = tokio::net::UnixStream::from_std(stream) else {
                return;
            };
            serve_conn(s, router, PeerId::default(), Peer::Instance(child), log).await;
        });
    }
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
    /// The SDK's JSON-RPC surface over agentd's handler, delegated to for
    /// every spec method.
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
    /// The browser CORS allowlist, shared so a reload can revise it: an
    /// operator who removes an origin to revoke a web client's access gets a
    /// listener that stops honouring it, not a reload that reported success
    /// and changed nothing.
    cors_origins: OriginList,
    /// How long a call handed to the SDK may take to produce its first byte:
    /// the fidelity filter waits this long for a stream's first event.
    request_timeout: Duration,
    stream_deadline: Duration,
    /// Whether a caller's session is still alive, consulted while its
    /// requests are in flight. `None` on a listener with no sessions.
    liveness: Option<Liveness>,
    documents: Vec<(String, Vec<u8>)>,
    callback: Option<String>,
    log: Logger,
}

/// For a caller, the check that says whether its session is still alive — or
/// `None` when the caller holds no session that could end.
///
/// A revoked session must not keep what it already opened: a stream the SDK
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

    // The per-task channels the reactor publishes into (through the sink)
    // and every stream the handler serves reads from.
    let streams = Arc::new(ports::Fanout::default());
    let sink = Arc::new(ports::StreamSink::new(
        Arc::clone(&streams),
        runtime.handle().clone(),
        log.clone(),
    ));
    let tls = opts.tls.clone();
    let app = Arc::new(App::new(bridge, opts, streams, log.clone()));
    let _ = feed;
    let router = router(app);
    let door = Door {
        handle: runtime.handle().clone(),
        router: router.clone(),
        log: log.clone(),
    };

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
            let stage = crate::a2a::socket::stage_dir(sock, std::process::id());
            let _ = std::fs::remove_dir_all(&stage);
            std::fs::create_dir(&stage)
                .map_err(|e| format!("a2a stage {}: {e}", stage.display()))?;
            std::fs::set_permissions(&stage, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .map_err(|e| format!("a2a stage perms: {e}"))?;
            let staged = crate::a2a::socket::staged(sock, std::process::id());
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
        door,
        _runtime: runtime,
    })
}

impl App {
    /// The listener's state, with the SDK's router over agentd's handler.
    fn new(bridge: Arc<A2aBridge>, opts: Opts, streams: Arc<ports::Fanout>, log: Logger) -> App {
        let handler = Arc::new(RuntimePorts::new(Arc::clone(&bridge), streams));
        // The liveness hook comes from the sessions: a caller holding one is
        // served only while that session — its sid — lives.
        let liveness = opts
            .auth
            .sessions
            .as_ref()
            .map(|s| crate::a2a::oauth::Sessions::liveness(Arc::clone(s)));
        App {
            protocol: a2a_server::jsonrpc::jsonrpc_router(handler),
            bridge,
            auth: opts.auth,
            failures: limits::SourceLimiter::auth_failures(),
            denials: limits::DenialLog::listener(),
            rates: limits::PrincipalRates::default(),
            cors_origins: opts.cors_origins,
            request_timeout: opts.request_timeout,
            stream_deadline: opts.stream_deadline,
            liveness,
            documents: opts.documents,
            callback: opts.callback,
            log,
        }
    }
}

/// The listener's routes. The card has one path, the one A2A 1.0 names; any
/// other is 404 like every path the listener does not serve.
///
/// The authorization server's routes exist only while something issues
/// through them — a listener that issues nothing does not answer as if it
/// might: the token and revocation endpoints while the device grant or a
/// launcher's slot does, the device grant's own endpoints and the RFC 8414
/// metadata only with the device grant, and the launch request only while the
/// slot names a web UI to accept it from.
fn router(app: Arc<App>) -> Router {
    use crate::a2a::oauth;
    let mut r = Router::new()
        .route("/", post(rpc).options(preflight))
        .route(
            "/.well-known/agent-card.json",
            get(card).options(card_preflight),
        );
    if let Some(authority) = &app.auth.authority {
        r = r
            .route(oauth::TOKEN_PATH, post(oauth_token).options(preflight))
            .route(oauth::REVOKE_PATH, post(oauth_revoke).options(preflight));
        if authority.device().is_some() {
            r = r
                .route(
                    oauth::DEVICE_AUTHORIZATION_PATH,
                    post(oauth_device_authorization).options(preflight),
                )
                .route(
                    oauth::VERIFICATION_PATH,
                    get(oauth_verification).options(preflight),
                );
        }
        if authority.device().is_some() || authority.login() {
            r = r.route(oauth::METADATA_PATH, get(oauth_metadata).options(preflight));
        }
        if authority.login() {
            r = r.route(
                crate::a2a::login::LOGIN_AUTHORIZATION_PATH,
                post(oauth_login_authorization).options(preflight),
            );
        }
        // The browser's two stops of an IdP sign-in: plain navigations,
        // answering nothing a page's script could read.
        if authority.signins().is_some() {
            r = r
                .route(
                    &format!("{}/{{id}}", crate::a2a::login::START_PATH),
                    get(signin_start),
                )
                .route(crate::a2a::login::CALLBACK_PATH, get(signin_callback));
        }
        if authority.launch().is_some_and(|l| l.origin().is_some()) {
            r = r.route(
                oauth::LAUNCH_AUTHORIZATION_PATH,
                post(oauth_launch_authorization).options(preflight),
            );
        }
    }
    // The public documents, at their paths: each the same bytes to every
    // caller, with no credential asked for and none read.
    for (path, body) in &app.documents {
        let body = body.clone();
        r = r.route(
            path,
            get(move || {
                let body = body.clone();
                async move { document(body) }
            }),
        );
    }
    // Where Direct Sign-In returns the User's browser: its query goes to the
    // loop as it came, and the browser is told only that it may close the
    // page — the `state` it carries is what says whose sign-in it is.
    if let Some(path) = &app.callback {
        r = r.route(path, get(company_callback));
    }
    r.with_state(app)
}

/// The Direct Sign-In callback: the query handed to the loop, and a page that
/// says only that it can be closed, never cached and never framed.
async fn company_callback(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    app.bridge
        .company_callback(uri.query().unwrap_or_default().to_string());
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
        ],
        "<!doctype html><title>agentd</title><p>You can close this page.</p>",
    )
        .into_response()
}

/// A served public document.
fn document(body: Vec<u8>) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, DOCUMENT_MAX_AGE),
        ],
        body,
    )
        .into_response()
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
        if let Err(refused) = cors::gate(headers, &app.origins()) {
            break 'resp refused.into_response();
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
                // The sid and the bind, never the code or the token.
                Notice::Launched { session, bind, via } => {
                    app.log.info(
                        "auth.launch.exchanged",
                        json!({"sid": session.sid, "client_id": session.client_id, "bind": bind, "via": via}),
                    );
                    push_auth_event(app, session.launched_event());
                }
                Notice::LaunchRequested(client_id) => {
                    app.log
                        .info("auth.launch.requested", json!({"client_id": client_id}));
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
    // The launch grant is bound to the request's Origin — whether it has one
    // at all, and which — so the endpoint is told it.
    let origin = cors::origin_of(&headers).map(str::to_string);
    oauth_call(&app, &headers, Some(body), |a, ct, b| {
        a.token(ct, b, ip, origin.as_deref())
    })
    .await
}

async fn oauth_launch_authorization(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    axum::Extension(peer): axum::Extension<Peer>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let ip = peer_ip(&peer);
    let origin = cors::origin_of(&headers).map(str::to_string);
    oauth_call(&app, &headers, Some(body), |a, ct, b| {
        a.launch_authorization(ct, b, ip, origin.as_deref())
    })
    .await
}

async fn oauth_revoke(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    oauth_call(&app, &headers, Some(body), |a, ct, b| a.revoke(ct, b)).await
}

async fn oauth_login_authorization(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    axum::Extension(peer): axum::Extension<Peer>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let ip = peer_ip(&peer);
    oauth_call(&app, &headers, Some(body), |a, ct, b| {
        a.login_authorization(ct, b, ip)
    })
    .await
}

/// A sign-in page's answer: fixed text, never cached, never framed, sent
/// to no other origin as a referrer.
fn signin_page(
    status: axum::http::StatusCode,
    text: String,
    extra: Vec<(axum::http::HeaderName, String)>,
) -> axum::response::Response {
    use axum::http::{HeaderValue, header};
    use axum::response::IntoResponse;
    let mut resp = (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        text,
    )
        .into_response();
    for (k, v) in extra {
        if let Ok(v) = HeaderValue::from_str(&v) {
            resp.headers_mut().append(k, v);
        }
    }
    resp
}

/// `GET /oauth2/start/<id>`: a start link, once — its cookie, and the
/// redirect to the IdP, whose URL the federation builds off this thread.
async fn signin_start(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use crate::a2a::login::Start;
    use axum::http::{StatusCode, header};
    let Some(a) = app.auth.authority.clone() else {
        return signin_page(StatusCode::NOT_FOUND, String::new(), Vec::new());
    };
    let (Some(signins), Some(issuer)) = (a.signins().cloned(), a.issuer().map(str::to_string))
    else {
        return signin_page(StatusCode::NOT_FOUND, String::new(), Vec::new());
    };
    let Start::Go(ask, cookie) = signins.start(&id, &issuer) else {
        return signin_page(
            StatusCode::GONE,
            "This sign-in link was used or has expired.".into(),
            Vec::new(),
        );
    };
    let url = tokio::task::spawn_blocking(move || signins.authorize_url(&ask)).await;
    match url {
        Ok(Ok(url)) => signin_page(
            StatusCode::FOUND,
            String::new(),
            vec![(header::LOCATION, url), (header::SET_COOKIE, cookie)],
        ),
        Ok(Err(e)) => {
            app.log.warn("auth.signin.unavailable", json!({"err": e}));
            signin_page(
                StatusCode::BAD_GATEWAY,
                "The identity provider could not be reached; try again.".into(),
                Vec::new(),
            )
        }
        Err(_) => signin_page(StatusCode::INTERNAL_SERVER_ERROR, String::new(), Vec::new()),
    }
}

/// `GET /oauth2/callback`: the browser back from the IdP. Taken only with
/// the cookie its start link set; the federation completes it off this
/// thread, and a login's session is then issued.
async fn signin_callback(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    use crate::a2a::login::Callback;
    use axum::http::{StatusCode, header};
    let Some(a) = app.auth.authority.clone() else {
        return signin_page(StatusCode::NOT_FOUND, String::new(), Vec::new());
    };
    let (Some(signins), Some(issuer), Some(sessions)) = (
        a.signins().cloned(),
        a.issuer().map(str::to_string),
        app.auth.sessions.clone(),
    ) else {
        return signin_page(StatusCode::NOT_FOUND, String::new(), Vec::new());
    };
    let cookies = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join("; ");
    let query = uri.query().unwrap_or_default().to_string();
    let found = signins.callback(
        &query,
        Some(cookies.as_str()).filter(|c| !c.is_empty()),
        &issuer,
    );
    let (id, answer) = match found {
        Callback::Complete(id, answer) => (id, answer),
        Callback::Refused(status, text) => {
            app.log.warn(
                "auth.signin.refused",
                json!({"status": status, "why": text}),
            );
            return signin_page(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                text,
                Vec::new(),
            );
        }
    };
    let s = Arc::clone(&signins);
    let done = tokio::task::spawn_blocking(move || {
        let result = s.complete(*answer);
        s.finished(&id, result, &sessions)
    })
    .await;
    match done {
        Ok((said, session)) => {
            if let Some(session) = session {
                app.log.info(
                    "auth.login.signed_in",
                    json!({"sid": session.sid, "principal": session.principal,
                           "client_id": session.client_id, "idp": session.proof.as_ref().map(|p| p.idp.clone())}),
                );
                push_auth_event(&app, session.signed_in_event());
            }
            signin_page(StatusCode::OK, said, Vec::new())
        }
        Err(_) => signin_page(StatusCode::INTERNAL_SERVER_ERROR, String::new(), Vec::new()),
    }
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
            serve_conn(sock, router, PeerId::default(), Peer::Unix(uid), log).await;
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
        listener_at("127.0.0.1:5000", answer, auth, origins, liveness)
    }

    /// [`listener_with`], every request arriving from `peer` — the address
    /// the accept loop would have recorded, with no second interface needed
    /// to be anywhere else.
    fn listener_at(
        peer: &str,
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
                span_id: None,
                parent_span_id: None,
                trace_flags: None,
            },
            crate::obs::log::Level::Error,
        );
        let opts = Opts {
            auth,
            cors_origins: Arc::new(std::sync::RwLock::new(origins)),
            tls: None,
            request_timeout: Duration::from_secs(5),
            stream_deadline: Duration::from_secs(5),
            documents: Vec::new(),
            callback: None,
        };
        let mut app = App::new(
            A2aBridge::new(tx, resolver),
            opts,
            Arc::new(ports::Fanout::default()),
            log,
        );
        if liveness.is_some() {
            app.liveness = liveness;
        }
        let router = router(Arc::new(app))
            .layer(axum::Extension(PeerId::default()))
            .layer(axum::Extension(Peer::Tcp(peer.parse().unwrap())));
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
    /// What reaches the SDK is the envelope the pipeline read: a request with
    /// no `params` — which JSON-RPC allows, and the specification's own
    /// `GetExtendedAgentCard` example sends — is answered, and a caller's
    /// integer past ±2^53 reaches the runtime as the double the protocol
    /// carries it as rather than refusing the request.
    #[tokio::test]
    async fn the_sdk_is_handed_the_envelope_the_pipeline_read() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let (router, _) = listener(
            move |req| {
                log.lock()
                    .unwrap()
                    .push((req.method.clone(), req.params.clone()));
                match req.method.as_str() {
                    "ListTasks" => {
                        json!({"tasks": [], "nextPageToken": "", "pageSize": 50, "totalSize": 0})
                    }
                    _ => json!({"task": {"id": req.params["taskId"], "contextId": "c",
                        "status": {"state": "TASK_STATE_COMPLETED"}}, "_seq": 0}),
                }
            },
            None,
        );
        let v = json_of(
            post(
                &router,
                r#"{"jsonrpc":"2.0","id":6,"method":"ListTasks"}"#,
                &[],
                &[],
            )
            .await,
        )
        .await;
        assert_eq!(v["result"]["totalSize"], 0, "{v}");

        let send = json!({"jsonrpc": "2.0", "id": 7, "method": "SendMessage", "params": {
            "message": {"role": "ROLE_USER", "messageId": "m-big",
                "parts": [{"data": {"cursor": 9_007_199_254_740_993u64}}]},
            "configuration": {"returnImmediately": true}}});
        let v = json_of(post(&router, &send.to_string(), &[], &[]).await).await;
        assert!(v.get("error").is_none(), "{v}");
        let seen = seen.lock().unwrap().clone();
        let (_, sent) = seen.iter().find(|(m, _)| m == "SendMessage").expect("sent");
        assert_eq!(
            sent["message"]["parts"][0]["data"]["cursor"].as_f64(),
            Some(9_007_199_254_740_992.0),
            "{sent}"
        );
    }

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

    /// An error envelope as the SDK writes it.
    fn sdk_error(code: i64, message: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": code, "message": message,
            "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "INVALID_PARAMS", "domain": "a2a-protocol.org"}]}})
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
            page: None,
            empty_result: false,
        }
    }

    async fn filtered(resp: Response, f: dispatch::Fidelity) -> Response {
        dispatch::faithful(resp, f, Duration::from_secs(5), None).await
    }

    /// What the SDK answers reaches the caller in the runtime's words: the
    /// runtime's error object replaces the SDK's rendering of it byte for
    /// byte, with the status its code travels with; an error the SDK raised
    /// on its own keeps the SDK's answer; and a stream that opens with an
    /// error is answered as JSON.
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

        // The SDK's own error is left as it wrote it.
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
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32603,\"message\":\"Internal error\"}}\nid: 4\n\n",
            )),
            fidelity(Some(draining.clone()), false),
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
        assert_eq!(frame["error"], draining);

        // The last page of a listing says so, as the spec's response does.
        let page = json!({"jsonrpc": "2.0", "id": 1, "result": {"pageSize": 50}});
        let resp = dispatch::faithful(
            unary_response(&page),
            dispatch::Fidelity {
                kept: None,
                bearer_used: false,
                page: Some(dispatch::Page::Tasks),
                empty_result: false,
            },
            Duration::from_secs(5),
            None,
        )
        .await;
        let v = json_of(resp).await;
        assert_eq!(v["result"]["nextPageToken"], "", "{v}");
        assert_eq!(v["result"]["tasks"], json!([]), "{v}");
        assert_eq!(
            v["result"]["totalSize"], 0,
            "an empty listing counts nothing: {v}"
        );
        assert_eq!(v["result"]["pageSize"], 50, "a field present is kept: {v}");
        let v = json_of(filtered(unary_response(&page), fidelity(None, false)).await).await;
        assert_eq!(v, page, "only a listing is completed");
    }

    /// A subscription reads its task first, as the caller: a task the runtime
    /// will not show this caller is refused as JSON, and no stream opens.
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
                task_not_found()
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
        assert_eq!(reads.len(), 1, "one read: {reads:?}");
        let (verb, params, who) = &reads[0];
        assert_eq!(verb, "GetTask");
        assert_eq!(params, &json!({"id": "task-1", "_attach": true}));
        assert_eq!(who, "operator", "read as the caller");
    }

    /// A caller whose session dies loses what it already opened: a stream
    /// the SDK is serving ends within a tick, and a unary call still waiting
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
        let cfg: crate::config::settings::DeviceGrant = serde_json::from_value(cfg).unwrap();
        let sessions = Arc::new(Sessions::new(oauth::system_clock()));
        let a = Arc::new(Authority::new(
            Some(DeviceGrant::new(
                &cfg,
                oauth::system_clock(),
                oauth::os_mint(),
            )),
            None,
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
            role: crate::config::settings::Role::User,
            principal: format!("user:{name}"),
            client_id: "cli".into(),
            created_ms: crate::state::now_ms(),
            expires_ms: None,
            approved_by: "operator".into(),
            approved_rule: None,
            rate: None,
            typed: crate::a2a::principals::typed::PrincipalRef::local(
                crate::a2a::principals::typed::Source::Device,
                name,
            ),
            proof: None,
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
        let a2a: crate::config::settings::A2a = serde_json::from_value(json!({
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
            crate::runtime::surface::Active::NONE,
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

    // ---- the launch grant ------------------------------------------------

    use crate::a2a::oauth::{LaunchBind, LaunchSlot};
    use crate::runtime::surface::launch::LAUNCH_GRANT_TYPE;

    const UI: &str = "http://127.0.0.1:4555";

    /// The authority a listener builds with `slot` installed — and the
    /// device grant too, when `device` — as `spawn_a2a_listener` builds it.
    fn launch_auth(slot: &Arc<LaunchSlot>, device: bool) -> Auth {
        let cfg: crate::config::settings::DeviceGrant =
            serde_json::from_value(json!({"enabled": true})).unwrap();
        let sessions = Arc::new(Sessions::new(oauth::system_clock()));
        let a = Arc::new(Authority::new(
            device.then(|| DeviceGrant::new(&cfg, oauth::system_clock(), oauth::os_mint())),
            Some(Arc::clone(slot)),
            Arc::clone(&sessions),
        ));
        a.set_issuer("http://127.0.0.1:8420");
        Auth {
            sessions: Some(sessions),
            authority: Some(a),
        }
    }

    fn launch_form(code: &str, client: &str) -> String {
        format!(
            "grant_type={}&code={code}&client_id={client}",
            LAUNCH_GRANT_TYPE.replace(':', "%3A").replace('/', "%2F")
        )
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn launch_routes_follow_the_slot() {
        // Neither the device grant nor a slot: no authorization server.
        let (router, _) = listener(|_| task_not_found(), None);
        for path in [oauth::TOKEN_PATH, oauth::LAUNCH_AUTHORIZATION_PATH] {
            let r = send(&router, form_post(path, "client_id=agentd-ui", None)).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
        }

        // A terminal client's slot alone: the token and revocation endpoints,
        // the launch grant only, and nothing a browser tab could ask at.
        let tui = Arc::new(LaunchSlot::new(None).unwrap());
        let (router, _) = listener_with(
            |_| task_not_found(),
            launch_auth(&tui, false),
            Vec::new(),
            None,
        );
        let r = send(
            &router,
            form_post(
                oauth::LAUNCH_AUTHORIZATION_PATH,
                "client_id=agentd-ui",
                None,
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let code = tui.issue(LaunchBind::NoOrigin, "agentd-tui").unwrap();
        let r = send(
            &router,
            form_post(oauth::TOKEN_PATH, &launch_form(&code, "agentd-tui"), None),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let token = json_of(r).await["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        let device = format!(
            "grant_type={}&device_code=x&client_id=cli",
            oauth::DEVICE_CODE_GRANT.replace(':', "%3A")
        );
        let r = send(&router, form_post(oauth::TOKEN_PATH, &device, None)).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_of(r).await["error"], "unsupported_grant_type");
        let r = send(
            &router,
            form_post(oauth::REVOKE_PATH, &format!("token={token}"), None),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        for req in [
            form_post(oauth::DEVICE_AUTHORIZATION_PATH, "client_id=cli", None),
            get(oauth::VERIFICATION_PATH),
            get(oauth::METADATA_PATH),
        ] {
            let path = req.uri().to_string();
            assert_eq!(
                send(&router, req).await.status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }

        // A web UI's slot: its tab may ask the terminal.
        let ui = Arc::new(LaunchSlot::new(Some(UI)).unwrap());
        let (router, _) = listener_with(
            |_| task_not_found(),
            launch_auth(&ui, false),
            oauth::admitted_origins(&[], Some(&ui)),
            None,
        );
        let r = send(
            &router,
            form_post(
                oauth::LAUNCH_AUTHORIZATION_PATH,
                "client_id=agentd-ui",
                Some(UI),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let v = json_of(r).await;
        assert!(
            v["request_code"]
                .as_str()
                .unwrap()
                .starts_with(oauth::LAUNCH_REQUEST_PREFIX),
            "{v}"
        );

        // Both: the metadata names both grants.
        let (router, _) = listener_with(
            |_| task_not_found(),
            launch_auth(&ui, true),
            Vec::new(),
            None,
        );
        let r = send(&router, get(oauth::METADATA_PATH)).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            json_of(r).await["grant_types_supported"],
            json!([oauth::DEVICE_CODE_GRANT, LAUNCH_GRANT_TYPE])
        );
    }

    /// The UI a launcher started is admitted by CORS with no configured
    /// origin at all — and admitted is all it is: without a session it is
    /// asked to sign in like any browser, and the origin next door is still
    /// refused.
    #[tokio::test]
    async fn the_launched_origin_is_allowed_and_nothing_else() {
        let slot = Arc::new(LaunchSlot::new(Some(UI)).unwrap());
        let (router, reached) = listener_with(
            |_| task_not_found(),
            launch_auth(&slot, false),
            oauth::admitted_origins(&[], Some(&slot)),
            None,
        );
        let preflight = |origin: &str| {
            Request::builder()
                .method("OPTIONS")
                .uri("/")
                .header("origin", origin)
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap()
        };
        let r = send(&router, preflight(UI)).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], UI);
        let r = send(&router, preflight("http://127.0.0.1:4556")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"t"}}"#;
        let r = post(&router, body, &[], &[("origin", UI)]).await;
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "admitted is not trusted"
        );
        assert_eq!(r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], UI);
        let r = post(&router, body, &[], &[("origin", "http://127.0.0.1:4556")]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(reached.load(Ordering::SeqCst), 0);

        // Signed in with the launch code, the tab is the operator.
        let code = slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        let r = send(
            &router,
            form_post(
                oauth::TOKEN_PATH,
                &launch_form(&code, "agentd-ui"),
                Some(UI),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let token = json_of(r).await["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        let auth = format!("Bearer {token}");
        let r = post(
            &router,
            body,
            &[],
            &[("origin", UI), ("authorization", &auth)],
        )
        .await;
        assert_eq!(json_of(r).await["error"]["code"], -32001);
        assert_eq!(reached.load(Ordering::SeqCst), 1);
    }

    /// The loopback rule as the router applies it, with the peer the accept
    /// loop records standing in for a second interface — so it is proven on
    /// a host that has none, where [`launch_grants_need_a_loopback_peer`]
    /// can only skip. A code presented from off the host is refused and
    /// burned; an approved request polled from there is refused and kept for
    /// its tab.
    #[tokio::test]
    async fn the_router_redeems_launch_grants_from_loopback_only() {
        let slot = Arc::new(LaunchSlot::new(Some(UI)).unwrap());
        let listener_from = |peer| {
            listener_at(
                peer,
                |_| task_not_found(),
                launch_auth(&slot, false),
                oauth::admitted_origins(&[], Some(&slot)),
                None,
            )
            .0
        };
        let (remote, local) = (listener_from("10.0.0.1:5000"), listener_from("[::1]:5000"));
        let token = |router: &Router, form: String| {
            let router = router.clone();
            async move {
                let r = send(&router, form_post(oauth::TOKEN_PATH, &form, Some(UI))).await;
                (r.status(), json_of(r).await)
            }
        };

        let code = slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        let (status, v) = token(&remote, launch_form(&code, "agentd-ui")).await;
        assert_eq!(
            (status, &v["error"]),
            (StatusCode::BAD_REQUEST, &json!("invalid_grant"))
        );
        let (status, v) = token(&local, launch_form(&code, "agentd-ui")).await;
        assert_eq!(
            (status, &v["error"]),
            (StatusCode::BAD_REQUEST, &json!("invalid_grant")),
            "burned: {v}"
        );

        let r = send(
            &remote,
            form_post(
                oauth::LAUNCH_AUTHORIZATION_PATH,
                "client_id=agentd-ui",
                Some(UI),
            ),
        )
        .await;
        assert_eq!(json_of(r).await["error"], "invalid_request");
        let r = send(
            &local,
            form_post(
                oauth::LAUNCH_AUTHORIZATION_PATH,
                "client_id=agentd-ui",
                Some(UI),
            ),
        )
        .await;
        let v = json_of(r).await;
        let request = v["request_code"].as_str().unwrap().to_string();
        assert!(slot.approve_user_code(v["user_code"].as_str().unwrap()));
        let poll = format!(
            "grant_type={}&request_code={request}&client_id=agentd-ui",
            LAUNCH_GRANT_TYPE.replace(':', "%3A").replace('/', "%2F")
        );
        let (status, v) = token(&remote, poll.clone()).await;
        assert_eq!(
            (status, &v["error"]),
            (StatusCode::BAD_REQUEST, &json!("invalid_grant"))
        );
        let (status, v) = token(&local, poll).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["scope"], "operator");
    }

    /// A launch code presented from an origin the listener does not admit —
    /// `Origin: null` or a page next door — never reaches the grant: the
    /// CORS gate every `/oauth2/*` route runs first answers 403, so the code
    /// is neither burned nor reported consumed, and its own client still
    /// redeems it. (A page could not read the answer anyway.) Only an
    /// ADMITTED origin that is not the code's bind reaches the grant, and
    /// burns it.
    #[tokio::test]
    async fn a_foreign_origin_is_refused_before_the_launch_grant() {
        use std::sync::atomic::AtomicU64;
        let slot = Arc::new(LaunchSlot::new(Some(UI)).unwrap());
        let consumed = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&consumed);
        slot.on_consume(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        const LISTED: &str = "https://console.example";
        let (router, _) = listener_with(
            |_| task_not_found(),
            launch_auth(&slot, false),
            oauth::admitted_origins(&[LISTED.to_string()], Some(&slot)),
            None,
        );
        let present = |code: &str, client: &str, origin: Option<&str>| {
            let req = form_post(oauth::TOKEN_PATH, &launch_form(code, client), origin);
            let router = router.clone();
            async move {
                let r = send(&router, req).await;
                let status = r.status();
                let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (
                    status,
                    serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
                )
            }
        };

        // A terminal client's code, then a browser's.
        let tui = slot.issue(LaunchBind::NoOrigin, "agentd-tui").unwrap();
        let ui = slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        for origin in ["null", "http://127.0.0.1:4556"] {
            for (code, client) in [(&tui, "agentd-tui"), (&ui, "agentd-ui")] {
                let (status, _) = present(code, client, Some(origin)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{origin} {client}");
            }
        }
        assert_eq!(consumed.load(Ordering::SeqCst), 0, "nothing was consumed");
        let (status, v) = present(&tui, "agentd-tui", None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let (status, v) = present(&ui, "agentd-ui", Some(UI)).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(consumed.load(Ordering::SeqCst), 2);

        // An admitted origin reaches the grant, which burns a code bound to
        // another.
        let ui = slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        let (status, v) = present(&ui, "agentd-ui", Some(LISTED)).await;
        assert_eq!(
            (status, &v["error"]),
            (StatusCode::BAD_REQUEST, &json!("invalid_grant"))
        );
        assert_eq!(consumed.load(Ordering::SeqCst), 3, "burned");
        let (status, _) = present(&ui, "agentd-ui", Some(UI)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// An address of this host that is not loopback, if it has one: the
    /// source address of a route out (no packet is sent).
    fn interface_ip() -> Option<std::net::IpAddr> {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("192.0.2.1:9").ok()?;
        let ip = s.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }

    /// A form POST over a real connection to `addr`, with `origin`: the
    /// status and the body.
    fn wire_post(addr: &str, path: &str, body: &str, origin: Option<&str>) -> (u16, Value) {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let origin = origin
            .map(|o| format!("Origin: {o}\r\n"))
            .unwrap_or_default();
        write!(
            s,
            "POST {path} HTTP/1.1\r\nHost: x\r\n{origin}Content-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).ok();
        let status = raw
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = raw
            .split_once("\r\n\r\n")
            .and_then(|(_, b)| serde_json::from_str(b.trim()).ok())
            .unwrap_or(Value::Null);
        (status, body)
    }

    /// A live code presented from off the host — a wildcard-bound listener
    /// dialled on this host's own interface address, so the peer is not
    /// loopback — is refused and burned; a tab's request from there is
    /// refused too.
    #[test]
    fn launch_grants_need_a_loopback_peer() {
        let Some(ip) = interface_ip() else {
            eprintln!("skipped: this host has no non-loopback address to dial");
            return;
        };
        let slot = Arc::new(LaunchSlot::new(Some(UI)).unwrap());
        let resolver = crate::a2a::Resolver::build(
            &serde_json::from_value(
                json!({"listen": "http://0.0.0.0:0", "url": "http://127.0.0.1:1"}),
            )
            .unwrap(),
            &|_| None,
        )
        .unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let log = Logger::new(
            crate::obs::log::LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: crate::obs::log::Comp::Supervisor,
                pid: std::process::id(),
                trace_id: None,
                span_id: None,
                parent_span_id: None,
                trace_flags: None,
            },
            crate::obs::log::Level::Error,
        );
        let listener = spawn(
            Bind::Tcp("0.0.0.0:0".into()),
            Opts {
                auth: launch_auth(&slot, false),
                cors_origins: Arc::new(std::sync::RwLock::new(oauth::admitted_origins(
                    &[],
                    Some(&slot),
                ))),
                tls: None,
                request_timeout: Duration::from_secs(5),
                stream_deadline: Duration::from_secs(5),
                documents: Vec::new(),
                callback: None,
            },
            A2aBridge::new(tx, resolver),
            None,
            log,
        )
        .unwrap();
        let port = listener.bound.rsplit_once(':').unwrap().1.to_string();
        let remote = format!(
            "{}:{port}",
            crate::runtime::surface::auth::bracket(&ip.to_string())
        );
        let local = format!("127.0.0.1:{port}");

        let code = slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        let form = launch_form(&code, "agentd-ui");
        let (status, v) = wire_post(&remote, oauth::TOKEN_PATH, &form, Some(UI));
        assert_eq!((status, &v["error"]), (400, &json!("invalid_grant")), "{v}");
        let (status, v) = wire_post(&local, oauth::TOKEN_PATH, &form, Some(UI));
        assert_eq!(
            (status, &v["error"]),
            (400, &json!("invalid_grant")),
            "burned: {v}"
        );
        let (status, v) = wire_post(
            &remote,
            oauth::LAUNCH_AUTHORIZATION_PATH,
            "client_id=agentd-ui",
            Some(UI),
        );
        assert_eq!(
            (status, &v["error"]),
            (400, &json!("invalid_request")),
            "{v}"
        );
        // From loopback, both work.
        let code = slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        let (status, v) = wire_post(
            &local,
            oauth::TOKEN_PATH,
            &launch_form(&code, "agentd-ui"),
            Some(UI),
        );
        assert_eq!(status, 200, "{v}");
        let (status, v) = wire_post(
            &local,
            oauth::LAUNCH_AUTHORIZATION_PATH,
            "client_id=agentd-ui",
            Some(UI),
        );
        assert_eq!(status, 200, "{v}");
    }
}
