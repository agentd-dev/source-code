//! Minimal HTTP/1.1 server (RFC §13).
//!
//! Hand-rolled, zero external HTTP crate. The request surface is
//! tiny (parse a request line, read a Content-Length body, write a
//! structured response) and keeping it in-tree beats pulling in
//! hyper/axum and an async runtime.
//!
//! Threading: one accept loop, one thread per accepted connection.
//! Max body 1 MiB, max headers 16 KiB — hardened against
//! head-of-line attacks without a full framework.
//!
//! Routing: the `http_routes` block of the workflow becomes a
//! `(METHOD, PATH)` table. A request matches exactly one entry;
//! everything else returns 404 / 405.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::engine::{Engine, ExecutionOutcome, RunOptions, TriggerMeta};
use crate::error::{Error, Result};
use crate::workflow::WorkflowDoc;

/// Identity extracted from a client cert after a successful mTLS
/// handshake. Always-defined so the non-TLS path can
/// thread `Option<PeerIdentity>` through uniformly; populated only
/// when the `server-tls` feature is on AND the peer presented a
/// client cert.
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    /// `sha256:<64-hex>` of the leaf cert's DER bytes. Stable
    /// identifier operators pin.
    pub fingerprint: String,
    /// Common Name from the subject DN, if present. Many modern
    /// PKIs leave CN empty and rely entirely on SANs — don't
    /// default to CN for identity.
    pub cn: Option<String>,
    /// DNS SAN entries, lowercase, in order of appearance.
    pub sans: Vec<String>,
}

/// Max body size accepted on an HTTP request. Declines larger
/// requests with 413 Payload Too Large.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Max size of the request-line + headers block before the server
/// gives up and returns 431.
const MAX_HEADERS_BYTES: usize = 16 * 1024;

/// One configured HTTP listener.
pub struct HttpServer {
    bind: SocketAddr,
    workflow: Arc<WorkflowDoc>,
    engine: Arc<Engine>,
    options: RunOptions,
    drain_timeout: Duration,
}

impl HttpServer {
    pub fn new(
        bind: SocketAddr,
        workflow: Arc<WorkflowDoc>,
        engine: Arc<Engine>,
        options: RunOptions,
    ) -> Self {
        Self {
            bind,
            workflow,
            engine,
            options,
            drain_timeout: Duration::from_secs(30),
        }
    }

    /// Override the graceful-drain budget (default 30 s). After the
    /// shutdown flag flips, the server stops accepting and waits up
    /// to this long for in-flight requests to complete.
    pub fn with_drain_timeout(mut self, d: Duration) -> Self {
        self.drain_timeout = d;
        self
    }

    /// Spawn the listener on its own thread. Returns a
    /// [`ServerHandle`] for orderly shutdown.
    pub fn spawn(self) -> Result<ServerHandle> {
        // Startup validation #1 — auth refs resolve to configured
        // bindings + OIDC JWKS parses cleanly. Expensive to debug
        // at first-request time.
        #[cfg(feature = "auth")]
        let prepared_auth = {
            let mut refs = Vec::with_capacity(self.workflow.http_routes.len());
            for route in &self.workflow.http_routes {
                refs.push(crate::auth::AuthRef::parse(route.auth.as_deref())?);
            }
            let empty = crate::auth::AuthConfig::default();
            let cfg = self.workflow.auth.as_ref().unwrap_or(&empty);
            cfg.validate(&refs)?;
            // Parsing the JWKS happens here, at spawn. A bad file
            // / malformed JWK / disallowed algorithm surfaces as
            // a bind error instead of an opaque per-request 401.
            crate::auth::PreparedAuth::from_config(cfg)?
        };
        #[cfg(not(feature = "auth"))]
        let prepared_auth: () = ();

        // Startup validation #2 — rate-limit numbers are sensible,
        // build one bucket per configured route up-front, and fold
        // both into the hot-reloadable shape the accept loop reads.
        let reloadable = HttpReloadable::build(&self.workflow.http_routes, &self.workflow.name)?;

        // Startup validation #3 — if `[server.tls]` is declared,
        // load the cert/key + (optional) client-auth CA so a
        // misconfigured cert path fails the bind rather than the
        // first request.
        let tls_config = self.build_tls_arc()?;

        let listener = TcpListener::bind(self.bind).map_err(|e| Error::Workflow {
            workflow: self.workflow.name.clone(),
            reason: format!("bind {}: {e}", self.bind),
        })?;
        let local_addr = listener.local_addr().unwrap_or(self.bind);

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Reloadable state — TLS config, prepared auth, and the
        // route+bucket map. Wrapped in `ArcSwap` so `SIGHUP` can
        // replace them atomically without tearing the accept loop.
        // In-flight requests keep the old snapshot (via the `Arc`
        // clone they hold); new connections see the replacement.
        // See §4.3 of RFC 0001 "Hot reload".
        let tls_swap: Arc<arc_swap::ArcSwap<TlsInner>> =
            Arc::new(arc_swap::ArcSwap::from_pointee(tls_config));
        let auth_swap: Arc<arc_swap::ArcSwap<PreparedAuthInner>> =
            Arc::new(arc_swap::ArcSwap::from_pointee(prepared_auth));
        let reloadable_swap: Arc<arc_swap::ArcSwap<HttpReloadable>> =
            Arc::new(arc_swap::ArcSwap::from_pointee(reloadable));

        let shutdown_flag = shutdown.clone();
        let in_flight_accept = in_flight.clone();
        let workflow = self.workflow.clone();
        let engine = self.engine.clone();
        let options = self.options.clone();
        let tls_accept = tls_swap.clone();
        let auth_accept = auth_swap.clone();
        let reloadable_accept = reloadable_swap.clone();

        let handle = thread::spawn(move || {
            listener.set_nonblocking(true).ok();
            while !shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        // BSD-family kernels (macOS) hand accepted
                        // sockets the listener's O_NONBLOCK flag;
                        // Linux does not. Force blocking explicitly
                        // so reads honour the timeouts below instead
                        // of returning WouldBlock between requests.
                        let _ = stream.set_nonblocking(false);
                        // Apply I/O timeouts on the raw TCP side —
                        // TLS-wrapped reads inherit them.
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

                        let wf = workflow.clone();
                        let eng = engine.clone();
                        let opts = options.clone();
                        // Snapshot reloadable state per accept —
                        // `load_full()` returns an `Arc` that stays
                        // valid even if the main state is swapped
                        // mid-request. The deref clone looks
                        // redundant when `TlsInner` is the
                        // feature-off stub (`Option<()>` is `Copy`),
                        // but clippy's copy-lint is the cost of
                        // keeping the shape uniform across features.
                        #[allow(clippy::clone_on_copy)]
                        let tls_snapshot: TlsArc = (**tls_accept.load()).clone();
                        let auth_snapshot: PreparedAuthArc = auth_accept.load_full();
                        let reloadable_snapshot: Arc<HttpReloadable> =
                            reloadable_accept.load_full();
                        let guard = InFlightGuard::acquire(in_flight_accept.clone());
                        thread::spawn(move || {
                            let _g = guard; // drop decrements counter
                            dispatch_accepted(
                                stream,
                                tls_snapshot,
                                &wf,
                                &eng,
                                &opts,
                                &reloadable_snapshot,
                                &auth_snapshot,
                            );
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });

        Ok(ServerHandle {
            local_addr,
            shutdown,
            in_flight,
            drain_timeout: self.drain_timeout,
            thread: Some(handle),
            tls_swap,
            auth_swap,
            reloadable_swap,
            workflow_name: self.workflow.name.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Hot-reloadable HTTP state (routes + rate-limit buckets)
// ---------------------------------------------------------------------------

/// Slice of the HTTP server that can be swapped on SIGHUP without
/// tearing. Routes and rate-limit buckets live together because a
/// route's `rate_limit` config drives its bucket's capacity — an
/// observer mid-reload must not see routes from the new doc mapped
/// to buckets from the old.
///
/// Rebuild semantics: on reload, all buckets are re-created from
/// scratch. Token counters are lost — this matches operator
/// expectations (reloading policy shouldn't let a flooding client
/// retain their allowance) and keeps the implementation
/// stateless.
pub(crate) struct HttpReloadable {
    /// Snapshot of `[[http_routes]]`. Route lookup reads this.
    pub(crate) routes: Vec<crate::workflow::HttpRoute>,
    /// Token buckets keyed by `(METHOD, PATH)`. Only routes with
    /// `rate_limit` declared get an entry.
    pub(crate) buckets:
        std::collections::HashMap<(String, String), std::sync::Arc<crate::ratelimit::TokenBucket>>,
}

impl HttpReloadable {
    /// Build from a workflow's route list, validating every
    /// rate-limit config along the way. Called both at spawn time
    /// and during reload.
    pub(crate) fn build(
        routes: &[crate::workflow::HttpRoute],
        workflow_name: &str,
    ) -> Result<Self> {
        let mut buckets: std::collections::HashMap<
            (String, String),
            std::sync::Arc<crate::ratelimit::TokenBucket>,
        > = std::collections::HashMap::new();
        for r in routes {
            if let Some(cfg) = &r.rate_limit {
                cfg.validate().map_err(|reason| Error::Workflow {
                    workflow: workflow_name.to_string(),
                    reason,
                })?;
                buckets.insert(
                    (r.method.to_ascii_uppercase(), r.path.clone()),
                    std::sync::Arc::new(crate::ratelimit::TokenBucket::new(cfg)),
                );
            }
        }
        Ok(Self {
            routes: routes.to_vec(),
            buckets,
        })
    }
}

/// The inner type `ArcSwap` wraps — `Option<Arc<ServerConfig>>` on
/// feature, `Option<()>` otherwise. ArcSwap holds these behind its
/// own Arc; the accept loop deref-clones once per accept.
#[cfg(feature = "server-tls")]
type TlsInner = Option<std::sync::Arc<rustls::ServerConfig>>;
#[cfg(not(feature = "server-tls"))]
type TlsInner = Option<()>;

/// `Arc<PreparedAuth>` kept behind `ArcSwap`. Identical shape
/// across features — the off-feature stub is `()` wrapped once.
#[cfg(feature = "auth")]
type PreparedAuthInner = crate::auth::PreparedAuth;
#[cfg(not(feature = "auth"))]
type PreparedAuthInner = ();

// ---------------------------------------------------------------------------
// TLS wiring
// ---------------------------------------------------------------------------

/// Opaque handle for the TLS config — `Some` iff `[server.tls]` is
/// set AND `server-tls` is compiled. Without the feature the type
/// is a `()` marker so the rest of the module stays uniform.
#[cfg(feature = "server-tls")]
type TlsArc = Option<std::sync::Arc<rustls::ServerConfig>>;
#[cfg(not(feature = "server-tls"))]
type TlsArc = Option<()>;

impl HttpServer {
    /// Build the optional TLS config. Fails the bind early if the
    /// workflow asks for TLS but the build doesn't carry rustls.
    fn build_tls_arc(&self) -> Result<TlsArc> {
        let Some(server) = &self.workflow.server else {
            return Ok(None);
        };
        let Some(tls) = &server.tls else {
            return Ok(None);
        };

        #[cfg(feature = "server-tls")]
        {
            let cfg = crate::triggers::http_tls::build_server_config(tls)?;
            Ok(Some(cfg))
        }

        #[cfg(not(feature = "server-tls"))]
        {
            let _ = tls;
            Err(Error::Workflow {
                workflow: self.workflow.name.clone(),
                reason: "workflow declares [server.tls] but this build lacks \
                         the `server-tls` Cargo feature; rebuild with \
                         --features server-tls"
                    .into(),
            })
        }
    }
}

/// Per-connection dispatch. Decides between plain TCP and TLS-wrapped
/// streams and hands each through the same [`handle_connection`]
/// pipeline.
fn dispatch_accepted(
    stream: TcpStream,
    tls: TlsArc,
    workflow: &WorkflowDoc,
    engine: &Engine,
    options: &RunOptions,
    reloadable: &HttpReloadable,
    prepared_auth: &PreparedAuthArc,
) {
    match tls {
        None => {
            let _ = handle_connection(
                stream,
                workflow,
                engine,
                options,
                reloadable,
                None,
                prepared_auth,
            );
        }
        #[cfg(feature = "server-tls")]
        Some(cfg) => {
            match crate::triggers::http_tls::accept_tls(stream, cfg) {
                Ok((tls_stream, identity)) => {
                    let _ = handle_connection(
                        tls_stream,
                        workflow,
                        engine,
                        options,
                        reloadable,
                        identity,
                        prepared_auth,
                    );
                }
                Err(e) => {
                    // TLS handshake failed — no way to reply at the
                    // HTTP layer; emit an audit event and drop.
                    tracing::warn!(
                        target: "agentd::audit",
                        event = "tls.handshake_failed",
                        reason = %e,
                    );
                }
            }
        }
        #[cfg(not(feature = "server-tls"))]
        Some(_) => unreachable!("build_tls_arc errors when TLS feature is off"),
    }
}

/// Auth container shared across request-handling threads. Uniform
/// type regardless of the `auth` feature so the dispatch signature
/// stays identical on both sides.
#[cfg(feature = "auth")]
type PreparedAuthArc = std::sync::Arc<crate::auth::PreparedAuth>;
#[cfg(not(feature = "auth"))]
type PreparedAuthArc = std::sync::Arc<()>;

/// RAII counter decrement for in-flight request tracking.
struct InFlightGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl InFlightGuard {
    fn acquire(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Handle on a running [`HttpServer`]. Dropping it triggers shutdown
/// and joins the accept thread.
pub struct ServerHandle {
    local_addr: SocketAddr,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    drain_timeout: Duration,
    thread: Option<thread::JoinHandle<()>>,
    /// `ArcSwap`-backed TLS config for SIGHUP hot-reload. `None`
    /// inside when TLS is not configured; the value is replaced on
    /// reload with a freshly-built `rustls::ServerConfig` from the
    /// updated `[server.tls]` block.
    tls_swap: Arc<arc_swap::ArcSwap<TlsInner>>,
    /// `ArcSwap`-backed prepared auth (including re-parsed OIDC
    /// JWKS) for SIGHUP hot-reload.
    auth_swap: Arc<arc_swap::ArcSwap<PreparedAuthInner>>,
    /// `ArcSwap`-backed route table + rate-limit bucket map
    ///. Swapping replaces both atomically;
    /// in-flight connections keep their per-accept snapshot.
    reloadable_swap: Arc<arc_swap::ArcSwap<HttpReloadable>>,
    /// Workflow name — used in audit events on reload.
    workflow_name: String,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Number of connections still being handled.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Request shutdown and return; does not wait. The accept loop
    /// sees the flag on its next poll and exits.
    pub fn request_shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Re-read the TLS cert/key from disk and atomically swap the
    /// active `rustls::ServerConfig`. In-flight TLS connections
    /// keep the old config via their per-accept snapshot; new
    /// handshakes use the new config.
    ///
    /// Passing `None` removes TLS from the active config (new
    /// connections fall back to plain HTTP) — useful for
    /// disable-on-issue runbooks. Enabling TLS on a server that
    /// bound without TLS is supported too; the same `build_tls_arc`
    /// path runs.
    pub fn reload_tls(&self, new_tls: Option<&crate::server_config::TlsConfig>) -> Result<()> {
        let next: TlsInner = match new_tls {
            None => None,
            #[cfg(feature = "server-tls")]
            Some(cfg) => Some(crate::triggers::http_tls::build_server_config(cfg)?),
            #[cfg(not(feature = "server-tls"))]
            Some(_) => {
                return Err(Error::Workflow {
                    workflow: self.workflow_name.clone(),
                    reason: "reload_tls: this build lacks the `server-tls` \
                             Cargo feature; rebuild with --features server-tls"
                        .into(),
                });
            }
        };
        self.tls_swap.store(Arc::new(next));
        tracing::info!(
            target: "agentd::audit",
            event = "reload.tls",
            workflow = %self.workflow_name,
            tls_active = new_tls.is_some(),
        );
        Ok(())
    }

    /// Rebuild the route table + rate-limit bucket map from a fresh
    /// `[[http_routes]]` list and atomically swap. New connections see the new routes and buckets;
    /// in-flight connections keep their per-accept snapshot. Token
    /// counters are not carried over — a new bucket starts at full
    /// capacity on every swap.
    pub fn reload_http_state(&self, new_routes: &[crate::workflow::HttpRoute]) -> Result<()> {
        let next = HttpReloadable::build(new_routes, &self.workflow_name)?;
        let bucket_count = next.buckets.len();
        self.reloadable_swap.store(Arc::new(next));
        tracing::info!(
            target: "agentd::audit",
            event = "reload.routes",
            workflow = %self.workflow_name,
            route_count = new_routes.len(),
            rate_limit_buckets = bucket_count,
        );
        Ok(())
    }

    /// Rebuild [`crate::auth::PreparedAuth`] from a fresh
    /// [`crate::auth::AuthConfig`] (re-parses JWKS, re-validates
    /// algorithm allowlists) and atomically swap. Only compiled when
    /// the `auth` feature is on — the caller (`run_reload`) is
    /// likewise gated, so the method genuinely doesn't exist on
    /// no-auth builds.
    #[cfg(feature = "auth")]
    pub fn reload_auth(&self, cfg: &crate::auth::AuthConfig) -> Result<()> {
        let prepared = crate::auth::PreparedAuth::from_config(cfg)?;
        self.auth_swap.store(Arc::new(prepared));
        tracing::info!(
            target: "agentd::audit",
            event = "reload.auth",
            workflow = %self.workflow_name,
        );
        Ok(())
    }

    /// Request shutdown, then block for up to `drain_timeout` for
    /// in-flight connections to complete. Returns `true` if the
    /// drain finished cleanly, `false` on deadline.
    pub fn shutdown_and_drain(mut self) -> bool {
        self.request_shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let deadline = std::time::Instant::now() + self.drain_timeout;
        while self.in_flight() > 0 {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    target: "agentd::audit",
                    event = "http.drain_deadline_exceeded",
                    in_flight = self.in_flight(),
                );
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
        true
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// Soft cap on requests per keep-alive connection. Prevents a long-
/// lived client from pinning a thread indefinitely. Most HTTP
/// clients cycle connections well below this ceiling.
const MAX_REQUESTS_PER_CONN: usize = 100;

/// Drive an accepted connection through the keep-alive request/
/// response loop. The single BufReader spans every request on the
/// connection; `read_timeout` (set on the TCP socket at accept
/// time) doubles as the idle-timeout — a client that stops sending
/// within 30s gets a timeout on the next `parse_request` and the
/// loop exits cleanly.
#[allow(clippy::too_many_arguments)]
fn handle_connection<S: std::io::Read + Write>(
    stream: S,
    workflow: &WorkflowDoc,
    engine: &Engine,
    options: &RunOptions,
    reloadable: &HttpReloadable,
    peer_identity: Option<PeerIdentity>,
    prepared_auth: &PreparedAuthArc,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    for _ in 0..MAX_REQUESTS_PER_CONN {
        let keep_alive = handle_one_request(
            &mut reader,
            workflow,
            engine,
            options,
            reloadable,
            peer_identity.as_ref(),
            prepared_auth,
        )?;
        if !keep_alive {
            break;
        }
    }
    Ok(())
}

/// Handle exactly one HTTP request on the connection. Returns
/// `Ok(true)` when the client and server both want to keep the
/// connection open for another request; `Ok(false)` ends the
/// loop (errors, explicit `Connection: close`, or an EOF while
/// waiting for the next request).
#[allow(clippy::too_many_arguments)]
fn handle_one_request<S: std::io::Read + Write>(
    reader: &mut BufReader<S>,
    workflow: &WorkflowDoc,
    engine: &Engine,
    options: &RunOptions,
    reloadable: &HttpReloadable,
    peer_identity: Option<&PeerIdentity>,
    prepared_auth: &PreparedAuthArc,
) -> std::io::Result<bool> {
    let parse_result = parse_request(reader);
    let stream = reader.get_mut();

    let mut request = match parse_result {
        Ok(r) => r,
        Err(e) if e.silent_close => {
            // Idle-timeout / clean EOF between requests on a
            // keep-alive connection. No bytes to reply to — just
            // close quietly.
            return Ok(false);
        }
        Err(e) => {
            write_response(stream, e.status, &e.body)?;
            return Ok(false);
        }
    };
    request.peer_identity = peer_identity.cloned();
    let keep_alive = client_wants_keep_alive(&request);

    // Health / metrics endpoints are always live so operators can
    // monitor the listener without touching a workflow.
    if request.method == "GET" && request.path == "/healthz" {
        write_response_with_header(
            stream,
            Status::new(200, "OK"),
            &json!({ "status": "ok", "workflow": workflow.name }),
            &connection_headers(keep_alive),
        )?;
        return Ok(keep_alive);
    }
    if request.method == "GET" && request.path == "/metrics" {
        let body = engine.metrics().snapshot().to_prometheus(&workflow.name);
        write_text_response_with_header(
            stream,
            Status::new(200, "OK"),
            "text/plain; version=0.0.4; charset=utf-8",
            body.as_bytes(),
            &connection_headers(keep_alive),
        )?;
        return Ok(keep_alive);
    }

    // Route. Dispatched against the ArcSwap snapshot the accept
    // loop captured — not `workflow.http_routes` — so SIGHUP route
    // edits take effect on the next accepted connection without
    // tearing any in-flight request.
    let route = reloadable
        .routes
        .iter()
        .find(|r| r.method.eq_ignore_ascii_case(&request.method) && r.path == request.path);
    let Some(route) = route else {
        // Distinguish "wrong method on a known path" (405) from
        // "unknown path entirely" (404) so clients see the right hint.
        let path_known = reloadable.routes.iter().any(|r| r.path == request.path);
        let status = if path_known {
            Status::new(405, "Method Not Allowed")
        } else {
            Status::new(404, "Not Found")
        };
        write_response_with_header(
            stream,
            status,
            &json!({ "error": status.reason, "path": request.path }),
            &connection_headers(keep_alive),
        )?;
        return Ok(keep_alive);
    };

    // Rate limit check — per-route token bucket. Cheapest
    // per-request gate, runs before auth so a flood of invalid
    // tokens also gets 429'd.
    let rate_key = (request.method.to_ascii_uppercase(), route.path.clone());
    if let Some(bucket) = reloadable.buckets.get(&rate_key) {
        if let Err(retry_after) = bucket.try_take() {
            tracing::warn!(
                target: "agentd::audit",
                event = "http.rate_limited",
                method = %request.method,
                path = %request.path,
                retry_after_ms = retry_after.as_millis() as u64,
            );
            let retry_secs = format!("{}", retry_after.as_secs().max(1));
            let mut headers = connection_headers(keep_alive);
            headers.push(("Retry-After".into(), retry_secs));
            write_response_with_header(
                stream,
                Status::new(429, "Too Many Requests"),
                &json!({
                    "error": "rate limited",
                    "retry_after_ms": retry_after.as_millis() as u64,
                }),
                &headers,
            )?;
            return Ok(keep_alive);
        }
    }

    // Auth check happens before we parse the body as JSON so we
    // don't burn cycles on unauthenticated requests.
    #[cfg(feature = "auth")]
    let principal = {
        let auth_ref = match crate::auth::AuthRef::parse(route.auth.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                // spawn() validated this; a mid-flight parse error
                // is an internal bug, surface as 500.
                write_response_with_header(
                    stream,
                    Status::new(500, "Internal Server Error"),
                    &json!({ "error": format!("auth config error: {e}") }),
                    &connection_headers(false),
                )?;
                return Ok(false);
            }
        };
        let auth_req = crate::auth::AuthRequest {
            headers: &request.headers,
            body: &request.body,
            peer_cert_fingerprint: request
                .peer_identity
                .as_ref()
                .map(|p| p.fingerprint.as_str()),
        };
        match crate::auth::evaluate(&auth_ref, prepared_auth, &auth_req) {
            crate::auth::AuthDecision::Allow { principal } => principal,
            crate::auth::AuthDecision::Deny { reason } => {
                tracing::warn!(
                    target: "agentd::audit",
                    event = "http.auth_denied",
                    method = %request.method,
                    path = %request.path,
                    reason = %reason,
                );
                write_response_with_header(
                    stream,
                    Status::new(401, "Unauthorized"),
                    &json!({ "error": "unauthorized", "detail": reason }),
                    &connection_headers(keep_alive),
                )?;
                return Ok(keep_alive);
            }
        }
    };

    // Parse body as JSON (or accept an empty body as `null`).
    let input = if request.body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&request.body) {
            Ok(v) => v,
            Err(e) => {
                write_response_with_header(
                    stream,
                    Status::new(400, "Bad Request"),
                    &json!({ "error": "invalid JSON body", "detail": e.to_string() }),
                    &connection_headers(keep_alive),
                )?;
                return Ok(keep_alive);
            }
        }
    };

    // When auth is compiled in, attach the principal to the trigger
    // payload so `trigger.principal.kind` / `trigger.principal.name`
    // are reachable from condition / switch nodes. For mTLS also
    // attach `cn` and `sans` when x509 parsing extracted them
    // so workflows can branch on logical service names
    // rather than opaque fingerprints.
    #[cfg(feature = "auth")]
    let input = {
        let mut input = input;
        let mut principal_json = json!({
            "kind": principal.kind,
            "name": principal.name,
        });
        if let Some(identity) = &request.peer_identity {
            if let Value::Object(pobj) = &mut principal_json {
                if let Some(cn) = &identity.cn {
                    pobj.insert("cn".into(), Value::String(cn.clone()));
                }
                if !identity.sans.is_empty() {
                    pobj.insert(
                        "sans".into(),
                        Value::Array(identity.sans.iter().cloned().map(Value::String).collect()),
                    );
                }
            }
        }
        if let Value::Object(obj) = &mut input {
            obj.insert("principal".to_string(), principal_json);
        } else if input.is_null() {
            input = json!({ "principal": principal_json });
        }
        input
    };

    // Run. Propagate W3C trace-context if the caller supplied a
    // `traceparent` header — fields land on a span that parents the
    // engine's `workflow.run`, so JSON-format log consumers see
    // trace_id / parent_id on every nested event without needing a
    // full OTLP exporter in-process.
    let trace_ctx = request
        .headers
        .get("traceparent")
        .and_then(|raw| crate::observability::parse_traceparent(raw));
    // Attach the parsed context to the trigger so the engine's
    // ExecutionContext carries it and outbound `http_request`
    // calls can continue the trace (follow-up).
    let trigger = match &trace_ctx {
        Some(tp) => TriggerMeta::http_with_trace(input, tp.clone()),
        None => TriggerMeta::http(input),
    };
    let request_span = tracing::info_span!(
        "http.request",
        method = %request.method,
        path = %request.path,
        trace_id = trace_ctx.as_ref().map(|t| t.trace_id.as_str()).unwrap_or(""),
        parent_id = trace_ctx.as_ref().map(|t| t.parent_id.as_str()).unwrap_or(""),
        trace_flags = trace_ctx.as_ref().map(|t| t.trace_flags.as_str()).unwrap_or(""),
        sampled = trace_ctx.as_ref().map(|t| t.sampled()).unwrap_or(false),
    );
    let _span_guard = request_span.enter();
    match engine.run(workflow, &route.start_node, trigger, options.clone()) {
        Ok(outcome) => {
            let status = match &outcome {
                ExecutionOutcome::Completed { .. } => Status::new(200, "OK"),
                ExecutionOutcome::Failed { .. } => Status::new(422, "Unprocessable Entity"),
                ExecutionOutcome::TimedOut { .. } => Status::new(504, "Gateway Timeout"),
            };
            write_response_with_header(stream, status, &outcome, &connection_headers(keep_alive))?;
            Ok(keep_alive)
        }
        Err(e) => {
            // Engine-level errors close the connection — the server
            // has observably misbehaved and keeping the socket open
            // for more traffic would just amplify the problem.
            write_response_with_header(
                stream,
                Status::new(500, "Internal Server Error"),
                &json!({ "error": format!("{e}") }),
                &connection_headers(false),
            )?;
            Ok(false)
        }
    }
}

/// Decide whether the client wants to keep the connection open
/// after this request. HTTP/1.1 default is keep-alive; the client
/// opts out with `Connection: close`. We treat HTTP/1.0 the same
/// way for simplicity — 1.0 clients in the wild that want
/// keep-alive send `Connection: keep-alive` explicitly, and
/// everything else is fine closing.
fn client_wants_keep_alive(request: &Request) -> bool {
    match request.headers.get("connection") {
        Some(v) => !v.to_ascii_lowercase().contains("close"),
        None => true,
    }
}

/// Build the `Connection` header for the response, reflecting the
/// server's decision back to the client. Allocates owned strings
/// because these headers flow through `write_response_with_header`
/// which expects `(String, String)` pairs.
fn connection_headers(keep_alive: bool) -> Vec<(String, String)> {
    let value = if keep_alive { "keep-alive" } else { "close" };
    vec![("Connection".into(), value.into())]
}

// ---------------------------------------------------------------------------
// HTTP parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
    /// Populated when the connection arrived over mTLS and a client
    /// cert was presented. Format: `sha256:<64-hex>`. `None` on
    /// plain HTTP and on HTTPS without client auth.
    peer_identity: Option<PeerIdentity>,
}

#[derive(Debug, Clone, Copy)]
struct Status {
    code: u16,
    reason: &'static str,
}

impl Status {
    const fn new(code: u16, reason: &'static str) -> Self {
        Self { code, reason }
    }
}

struct ParseError {
    status: Status,
    body: Value,
    /// When true, the caller should close the connection without
    /// writing a response. Used for idle-timeout / clean EOF on a
    /// keep-alive connection waiting for the next request — there's
    /// nothing to reply to.
    silent_close: bool,
}

fn parse_request<R: BufRead>(reader: &mut R) -> std::result::Result<Request, ParseError> {
    // Request line.
    let mut line = String::new();
    let read = reader.read_line(&mut line);
    let n = match read {
        Ok(n) => n,
        Err(e) => {
            // WouldBlock / TimedOut while waiting for the next
            // request on a keep-alive connection is our idle-timeout
            // signal — close quietly.
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                return Err(silent_close());
            }
            return Err(bad(400, "request line read failed"));
        }
    };
    if n == 0 {
        return Err(silent_close());
    }
    let mut parts = line.trim_end().split(' ');
    let method = parts
        .next()
        .ok_or_else(|| bad(400, "missing method"))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| bad(400, "missing path"))?
        .to_string();
    // Strip query string if present — Phase 6 routes on path only.
    let path = path.split('?').next().unwrap_or(&path).to_string();
    let _ = parts.next(); // ignore HTTP version

    // Headers.
    let mut headers_bytes = n;
    let mut content_length = 0usize;
    let mut headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        let mut header_line = String::new();
        let read = reader
            .read_line(&mut header_line)
            .map_err(|_| bad(400, "header read failed"))?;
        if read == 0 {
            return Err(bad(400, "unexpected EOF in headers"));
        }
        headers_bytes += read;
        if headers_bytes > MAX_HEADERS_BYTES {
            return Err(ParseError {
                status: Status::new(431, "Request Header Fields Too Large"),
                body: json!({ "error": "headers too large" }),
                silent_close: false,
            });
        }
        let trimmed = header_line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if key == "content-length" {
                content_length = value
                    .parse::<usize>()
                    .map_err(|_| bad(400, "invalid Content-Length"))?;
            }
            headers.insert(key, value);
        }
    }

    if content_length > MAX_BODY_BYTES {
        return Err(ParseError {
            status: Status::new(413, "Payload Too Large"),
            body: json!({ "error": "body exceeds server cap", "cap_bytes": MAX_BODY_BYTES }),
            silent_close: false,
        });
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|_| bad(400, "truncated body"))?;
    }
    Ok(Request {
        method,
        path,
        headers,
        body,
        peer_identity: None,
    })
}

fn bad(code: u16, msg: &'static str) -> ParseError {
    ParseError {
        status: Status::new(code, msg),
        body: json!({ "error": msg }),
        silent_close: false,
    }
}

/// Signal that the peer went away cleanly between requests on a
/// keep-alive connection (idle timeout / clean EOF). The caller
/// should close without writing a response.
fn silent_close() -> ParseError {
    ParseError {
        status: Status::new(0, ""),
        body: Value::Null,
        silent_close: true,
    }
}

// ---------------------------------------------------------------------------
// Response writing
// ---------------------------------------------------------------------------

fn write_response<S: Write, B: serde::Serialize>(
    stream: &mut S,
    status: Status,
    body: &B,
) -> std::io::Result<()> {
    // Single-shot convenience: mirrors the pre-keep-alive behaviour
    // for the rare sites that haven't made a keep-alive decision
    // yet (e.g. the parse-error fast path in handle_one_request).
    write_response_with_header(stream, status, body, &connection_headers(false))
}

fn write_response_with_header<S: Write, B: serde::Serialize>(
    stream: &mut S,
    status: Status,
    body: &B,
    extra_headers: &[(String, String)],
) -> std::io::Result<()> {
    let body = serde_json::to_vec(body)?;
    let mut header = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n",
        status.code,
        status.reason,
        body.len()
    );
    for (k, v) in extra_headers {
        header.push_str(&format!("{k}: {v}\r\n"));
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

/// Write a raw-body response (non-JSON). Used for the Prometheus
/// text-exposition endpoint, which cannot share the JSON helper's
/// `Content-Type`. All callers today go through
/// [`write_text_response_with_header`]; this convenience variant is
/// kept around for parity with [`write_response`] in case a future
/// caller wants the default (close) shape.
#[allow(dead_code)]
fn write_text_response<S: Write>(
    stream: &mut S,
    status: Status,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write_text_response_with_header(
        stream,
        status,
        content_type,
        body,
        &connection_headers(false),
    )
}

fn write_text_response_with_header<S: Write>(
    stream: &mut S,
    status: Status,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(String, String)],
) -> std::io::Result<()> {
    let mut header = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n",
        status.code,
        status.reason,
        content_type,
        body.len()
    );
    for (k, v) in extra_headers {
        header.push_str(&format!("{k}: {v}\r\n"));
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{HandlerRegistry, StubHandler};
    use crate::tools::{policy::allow_all, register_default_tools};
    use crate::workflow::model::{Edge, HttpRoute, Node, NodeKind, StartNode, StartSource};
    use std::io::{BufReader, Read, Write};
    use std::net::TcpStream;

    fn minimal_wf() -> WorkflowDoc {
        WorkflowDoc {
            name: "t".into(),
            start_nodes: vec![StartNode {
                name: "on_http".into(),
                source: StartSource::Http,
                entry_node: Some("a".into()),
            }],
            http_routes: vec![HttpRoute {
                method: "POST".into(),
                path: "/run".into(),
                start_node: "on_http".into(),
                input_schema: None,
                auth: None,
                rate_limit: None,
            }],
            nodes: vec![
                Node {
                    id: "a".into(),
                    retry: None,
                    kind: NodeKind::Merge,
                },
                Node {
                    id: "b".into(),
                    retry: None,
                    kind: NodeKind::Terminate,
                },
            ],
            edges: vec![Edge {
                from: "a".into(),
                to: "b".into(),
                when: None,
            }],
            ..Default::default()
        }
    }

    fn start_server(wf: WorkflowDoc) -> ServerHandle {
        let mut registry = HandlerRegistry::with_builtin_controls();
        register_default_tools(&mut registry, allow_all(), crate::budget::unbounded());
        registry.set_fallback(Box::new(StubHandler));
        let engine = Arc::new(Engine::new(registry));
        let server = HttpServer::new(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(wf),
            engine,
            RunOptions::default(),
        );
        server.spawn().expect("spawn http server")
    }

    fn send(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, String) {
        send_with_headers(addr, method, path, &std::collections::HashMap::new(), body)
    }

    fn send_with_headers(
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &std::collections::HashMap<String, String>,
        body: &[u8],
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();

        let mut buf = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_to_string(&mut buf).unwrap();
        let (status_line, rest) = buf.split_once("\r\n").unwrap_or((&buf, ""));
        let code = status_line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        // Body starts after the empty line separating headers + body.
        let body = rest.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        (code, body.to_string())
    }

    /// Send two requests back-to-back on the same TCP connection,
    /// asserting both responses parse cleanly and each signals
    /// `Connection: keep-alive` except the last (which we mark
    /// `close` so the loop ends).
    fn send_two_keepalive(addr: SocketAddr, method: &str, path: &str) -> (u16, u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        // Request 1: explicit keep-alive.
        let r1 = format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Length: 2\r\n\
             Connection: keep-alive\r\n\r\n{{}}"
        );
        stream.write_all(r1.as_bytes()).unwrap();
        stream.flush().unwrap();

        // Read response 1 up to the expected Content-Length. Since we
        // don't want to block on Connection: close, parse headers
        // manually.
        let (status1, body1_tail) = read_one_response(&mut stream);

        // Request 2: ask to close after this one.
        let r2 = format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\r\n{{}}"
        );
        stream.write_all(r2.as_bytes()).unwrap();
        stream.flush().unwrap();

        let (status2, _) = read_one_response(&mut stream);
        (status1, status2, body1_tail)
    }

    /// Read one HTTP/1.1 response from the stream using Content-Length.
    fn read_one_response(stream: &mut TcpStream) -> (u16, String) {
        use std::io::Read as _;
        let mut acc: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        // Read until we've seen `\r\n\r\n` and N body bytes.
        let mut content_length = 0usize;
        let mut headers_end = None;
        let mut status: u16 = 0;
        loop {
            let read = stream.read(&mut tmp).unwrap();
            if read == 0 {
                break;
            }
            acc.extend_from_slice(&tmp[..read]);
            if headers_end.is_none() {
                if let Some(pos) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
                    headers_end = Some(pos + 4);
                    let header_str = std::str::from_utf8(&acc[..pos]).unwrap();
                    let mut lines = header_str.split("\r\n");
                    if let Some(status_line) = lines.next() {
                        status = status_line
                            .split(' ')
                            .nth(1)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                    }
                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            if k.trim().eq_ignore_ascii_case("content-length") {
                                content_length = v.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                }
            }
            if let Some(end) = headers_end {
                if acc.len() >= end + content_length {
                    break;
                }
            }
        }
        let body = headers_end
            .map(|e| String::from_utf8_lossy(&acc[e..e + content_length]).into_owned())
            .unwrap_or_default();
        (status, body)
    }

    #[test]
    fn keepalive_serves_two_requests_on_one_connection() {
        let handle = start_server(minimal_wf());
        let (s1, s2, b1) = send_two_keepalive(handle.local_addr(), "POST", "/run");
        assert_eq!(s1, 200, "first response must succeed");
        assert_eq!(s2, 200, "second response must succeed on same conn");
        let json: serde_json::Value = serde_json::from_str(&b1).unwrap();
        assert_eq!(json["status"], "completed");
        handle.shutdown_and_drain();
    }

    #[test]
    fn keepalive_closes_when_client_requests_close() {
        // Explicit Connection: close should keep the semantics
        // identical to the pre-keep-alive world: one request, one
        // response, then close. The existing `send` helper already
        // does this — we just assert the contract still holds.
        let handle = start_server(minimal_wf());
        let (code, body) = send(handle.local_addr(), "POST", "/run", b"{}");
        assert_eq!(code, 200);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "completed");
        handle.shutdown_and_drain();
    }

    #[test]
    fn routes_to_declared_path() {
        let handle = start_server(minimal_wf());
        let (code, body) = send(handle.local_addr(), "POST", "/run", b"{}");
        assert_eq!(code, 200);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "completed");
        handle.shutdown_and_drain();
    }

    #[test]
    fn unknown_path_returns_404() {
        let handle = start_server(minimal_wf());
        let (code, _body) = send(handle.local_addr(), "POST", "/nope", b"{}");
        assert_eq!(code, 404);
        handle.shutdown_and_drain();
    }

    #[test]
    fn wrong_method_on_known_path_returns_405() {
        let handle = start_server(minimal_wf());
        let (code, _body) = send(handle.local_addr(), "GET", "/run", b"");
        assert_eq!(code, 405);
        handle.shutdown_and_drain();
    }

    #[test]
    fn invalid_json_returns_400() {
        let handle = start_server(minimal_wf());
        let (code, body) = send(handle.local_addr(), "POST", "/run", b"not json");
        assert_eq!(code, 400);
        assert!(body.contains("invalid JSON"));
        handle.shutdown_and_drain();
    }

    #[test]
    fn healthz_is_always_live() {
        let handle = start_server(minimal_wf());
        let (code, body) = send(handle.local_addr(), "GET", "/healthz", b"");
        assert_eq!(code, 200);
        assert!(body.contains("\"status\":\"ok\""));
        handle.shutdown_and_drain();
    }

    #[test]
    fn metrics_endpoint_returns_prometheus_text() {
        let handle = start_server(minimal_wf());
        // Hit the workflow once so counters advance past zero.
        let (_code, _body) = send(handle.local_addr(), "POST", "/run", b"{}");
        let (code, body) = send(handle.local_addr(), "GET", "/metrics", b"");
        assert_eq!(code, 200);
        assert!(
            body.contains("# TYPE agentd_workflow_starts_total counter"),
            "body was: {body}"
        );
        assert!(body.contains("agentd_workflow_starts_total{workflow=\"t\"} "));
        assert!(body.contains("agentd_build_info{"));
        handle.shutdown_and_drain();
    }

    #[test]
    fn reload_auth_swaps_prepared_state() {
        // Spawn with an empty auth config (no bindings), then
        // reload with a bearer binding. A request carrying the new
        // binding's token should be accepted afterwards.
        #[cfg(feature = "auth")]
        {
            use std::collections::HashMap;
            let mut wf = minimal_wf();
            wf.http_routes[0].auth = Some("bearer:ops".into());
            // Intentionally don't set wf.auth — the server won't
            // validate the ref. Work around by supplying a dummy
            // config with the binding present but empty tokens.
            let mut cfg = crate::auth::AuthConfig::default();
            cfg.bearer
                .insert("ops".into(), crate::auth::config::BearerDef::default());
            wf.auth = Some(cfg);

            let handle = start_server(wf);

            // Before reload: empty token set → 401.
            let mut h = HashMap::new();
            h.insert("Authorization".into(), "Bearer s3cret".into());
            let (code, _) = send_with_headers(handle.local_addr(), "POST", "/run", &h, b"{}");
            assert_eq!(code, 401);

            // Reload with a real token.
            let mut new_cfg = crate::auth::AuthConfig::default();
            let def = crate::auth::config::BearerDef {
                tokens: vec!["s3cret".into()],
                ..Default::default()
            };
            new_cfg.bearer.insert("ops".into(), def);
            handle.reload_auth(&new_cfg).unwrap();

            // After reload: same request succeeds.
            let (code, _) = send_with_headers(handle.local_addr(), "POST", "/run", &h, b"{}");
            assert_eq!(code, 200);
            handle.shutdown_and_drain();
        }
    }

    #[test]
    fn reload_tls_swaps_server_config() {
        // Reload accepts `None` even when not currently configured;
        // a no-op swap should succeed and emit an audit event.
        let handle = start_server(minimal_wf());
        handle.reload_tls(None).expect("reload with None");
        handle.shutdown_and_drain();
    }

    #[test]
    fn reload_http_state_adds_new_route() {
        // Hot-reload a new POST route that wasn't in the original
        // workflow. The new route must dispatch on the next request
        // without a restart.
        let handle = start_server(minimal_wf());

        // Original workflow only exposes POST /run. /added is 404.
        let (code, _) = send(handle.local_addr(), "POST", "/added", b"");
        assert_eq!(code, 404);

        let new_routes = vec![
            HttpRoute {
                method: "POST".into(),
                path: "/run".into(),
                start_node: "on_http".into(),
                input_schema: None,
                auth: None,
                rate_limit: None,
            },
            HttpRoute {
                method: "POST".into(),
                path: "/added".into(),
                start_node: "on_http".into(),
                input_schema: None,
                auth: None,
                rate_limit: None,
            },
        ];
        handle
            .reload_http_state(&new_routes)
            .expect("reload routes");

        let (code, _) = send(handle.local_addr(), "POST", "/added", b"");
        assert_eq!(code, 200, "new route should dispatch after reload");

        handle.shutdown_and_drain();
    }

    #[test]
    fn reload_http_state_removes_route_and_returns_404() {
        // Start with two routes; reload down to one. The removed
        // route must now return 404.
        let mut wf = minimal_wf();
        wf.http_routes.push(HttpRoute {
            method: "POST".into(),
            path: "/gone".into(),
            start_node: "on_http".into(),
            input_schema: None,
            auth: None,
            rate_limit: None,
        });
        let handle = start_server(wf);

        let (code, _) = send(handle.local_addr(), "POST", "/gone", b"");
        assert_eq!(code, 200);

        let shrunk = vec![HttpRoute {
            method: "POST".into(),
            path: "/run".into(),
            start_node: "on_http".into(),
            input_schema: None,
            auth: None,
            rate_limit: None,
        }];
        handle.reload_http_state(&shrunk).expect("shrink routes");

        let (code, _) = send(handle.local_addr(), "POST", "/gone", b"");
        assert_eq!(code, 404, "removed route must 404 after reload");

        handle.shutdown_and_drain();
    }

    #[test]
    fn reload_http_state_rebuilds_rate_limit_buckets() {
        // Add a rate-limit config via reload; the new bucket must be
        // enforced on subsequent requests.
        let handle = start_server(minimal_wf());
        let routes_with_limit = vec![HttpRoute {
            method: "POST".into(),
            path: "/run".into(),
            start_node: "on_http".into(),
            input_schema: None,
            auth: None,
            rate_limit: Some(crate::ratelimit::RateLimitConfig {
                capacity: 1,
                per_second: 0.001,
            }),
        }];
        handle
            .reload_http_state(&routes_with_limit)
            .expect("reload with rate limit");

        // First request — bucket has 1 token.
        let (code1, _) = send(handle.local_addr(), "POST", "/run", b"");
        assert_eq!(code1, 200);
        // Second immediate request — bucket empty, expect 429.
        let (code2, _) = send(handle.local_addr(), "POST", "/run", b"");
        assert_eq!(code2, 429, "rate limit should kick in after first token");

        handle.shutdown_and_drain();
    }

    #[test]
    fn metrics_endpoint_requires_get() {
        // POST /metrics is an unknown route (not a 405) because the
        // always-live handler only matches GET — we want declared
        // workflow routes to be independently addressable.
        let handle = start_server(minimal_wf());
        let (code, _body) = send(handle.local_addr(), "POST", "/metrics", b"");
        assert_eq!(code, 404);
        handle.shutdown_and_drain();
    }

    #[test]
    fn empty_body_treated_as_null_input() {
        // A workflow that reads trigger and terminates — verifies the
        // empty body doesn't break the pipeline.
        let mut wf = minimal_wf();
        wf.nodes[0] = Node {
            id: "a".into(),
            retry: None,
            kind: NodeKind::Condition {
                expr: "trigger.kind".into(),
            },
        };
        wf.edges = vec![
            Edge {
                from: "a".into(),
                to: "b".into(),
                when: Some("true".into()),
            },
            Edge {
                from: "a".into(),
                to: "b".into(),
                when: Some("false".into()),
            },
        ];
        let handle = start_server(wf);
        let (code, _body) = send(handle.local_addr(), "POST", "/run", b"");
        assert_eq!(code, 200);
        handle.shutdown_and_drain();
    }

    #[test]
    fn failed_workflow_maps_to_422() {
        let mut wf = minimal_wf();
        wf.nodes[0] = Node {
            id: "a".into(),
            retry: None,
            kind: NodeKind::Fail {
                reason: Some("boom".into()),
            },
        };
        wf.edges.clear();
        let handle = start_server(wf);
        let (code, body) = send(handle.local_addr(), "POST", "/run", b"{}");
        assert_eq!(code, 422);
        assert!(body.contains("\"status\":\"failed\""));
        assert!(body.contains("boom"));
        handle.shutdown_and_drain();
    }

    #[test]
    fn oversized_body_returns_413() {
        // Claim 32 MiB without actually writing it; the server should
        // 413 on the Content-Length check before reading the body.
        let handle = start_server(minimal_wf());
        let addr = handle.local_addr();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(
                b"POST /run HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Content-Length: 33554432\r\n\
                 Connection: close\r\n\
                 \r\n",
            )
            .unwrap();
        stream.flush().unwrap();

        let mut buf = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_to_string(&mut buf).unwrap();
        assert!(buf.contains("413"));
        handle.shutdown_and_drain();
    }
}
