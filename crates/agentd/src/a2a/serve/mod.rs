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
//! ## Two vocabularies on one endpoint
//!
//! Most methods are the specification's. A few are answered here rather than
//! passed down: the observation feed the display clients read
//! (`SubscribeToEvents`) is agentd's own, so a2a-rs correctly does not know
//! it; the public card read (`GetAgentCard`)
//! is agentd's convenience over a document the spec publishes only at
//! `.well-known`, and both must answer with the *same* card; and the spec's own
//! `GetExtendedAgentCard` is served locally too — it is that card plus the
//! skills this caller may actually run, and a round trip through the SDK's
//! typed `AgentCard` drops any field it has no place for. Operator admin is not
//! a method family: `admin.drain`, `admin.pause` and their siblings ride in as
//! command DataParts on `SendMessage` and are handled in
//! [`crate::runtime::a2a_server`]. Anything else goes to the protocol layer,
//! including the methods it implements that agentd does not, so an
//! unimplemented method is refused with the code the spec assigns rather than a
//! generic failure.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use serde_json::json;

use crate::a2a::ports::{self, RuntimePorts};
use crate::obs::log::Logger;
use crate::runtime::a2a_server::{A2aBridge, SharedFeed};

mod card;
mod cors;
mod dispatch;
mod feed;
mod identity;
/// Per-source failure limits and per-principal admission.
mod limits;

use card::{CardFromRuntime, card};
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
    stream_deadline: Duration,
    log: Logger,
}

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
    let ports = RuntimePorts::new(Arc::clone(&bridge), Arc::clone(&updates));
    let adapter = Arc::new(
        a2a_rs::adapter::JsonRpcAdapter::with_handler(ports, CardFromRuntime(Arc::clone(&bridge)))
            .with_streaming_handler(ports::SharedStreaming(updates)),
    );

    let app = Arc::new(App {
        protocol: a2a_rs::adapter::jsonrpc_router(adapter),
        bridge: Arc::clone(&bridge),
        auth: opts.auth,
        failures: limits::SourceLimiter::auth_failures(),
        denials: limits::DenialLog::listener(),
        rates: limits::PrincipalRates::default(),
        cors_origins: opts.cors_origins,
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
            let tls = opts.tls;
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
