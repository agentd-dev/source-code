// SPDX-License-Identifier: Apache-2.0
//! The reusable **MCP server** base: transport, framing, connection handling, the
//! lifecycle/version machinery, and the resource-subscription registry — agentd's
//! served self-MCP (and any other embedder's server) builds its domain surface on
//! top by implementing [`Handler`].
//!
//! The split mirrors the client: this module owns the *protocol* (how bytes become
//! requests, how `initialize` / `server/discover` / `ping` are answered across
//! both eras, how a subscriber is pushed a `notifications/resources/updated`),
//! while the embedder owns the *domain* (which tools exist, which resources are
//! readable, who may subscribe to what). One [`Handler`] trait is the seam.
//!
//! Transport is deliberately minimal and dependency-light (RFC 0015 §3.6): a
//! blocking listener, one thread per connection, speaking the same NDJSON JSON-RPC
//! codec ([`crate::rpc::frame`]) as the client. No async, no mio. [`ServeStream`]
//! type-erases unix vs. vsock so the framing, threading, and dispatch are entirely
//! transport-agnostic ("the unix server with the socket type swapped").

use crate::rpc::{Incoming, Notification, Request, Response, frame};
use crate::wire::method;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Which transport a connection arrived on, and therefore its trust domain (RFC
/// 0015 §3.3-§3.4). A generic two-domain model the framework only carries and
/// hands to the [`Handler`]; the embedder assigns meaning:
///   * [`Stdio`](PeerOrigin::Stdio) — an in-process / same-trust caller (agentd's
///     own driving harness over the process stdio).
///   * [`Management`](PeerOrigin::Management) — a peer that dialed a listener (unix
///     socket / vsock), i.e. the management trust domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOrigin {
    /// The process's own stdio / an in-process caller (the driving harness).
    Stdio,
    /// A peer on a listener (unix / vsock) — the management trust domain.
    Management,
}

impl PeerOrigin {
    /// Stable lowercase label for logs/metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            PeerOrigin::Stdio => "stdio",
            PeerOrigin::Management => "management",
        }
    }
}

/// The served-MCP transport, type-erased to one concrete enum so the connection
/// registry ([`SharedWriter`], [`Subscriber`]) stays monomorphic across transports
/// while the *same* connection code serves each. Both variants are `Read + Write`
/// with a [`try_clone`](ServeStream::try_clone) (the connection's write half is
/// shared with the threads that push notifications), so the NDJSON framing,
/// threading, and dispatch are entirely transport-agnostic (RFC 0015 §3.2).
pub enum ServeStream {
    /// A unix-domain-socket peer.
    Unix(UnixStream),
    /// An AF_VSOCK peer (host↔guest management transport).
    #[cfg(feature = "vsock")]
    Vsock(vsock::VsockStream),
}

impl ServeStream {
    /// Clone the handle (a second fd onto the same connection) for the shared write
    /// half. Mirrors `UnixStream::try_clone`.
    pub fn try_clone(&self) -> io::Result<ServeStream> {
        match self {
            ServeStream::Unix(s) => s.try_clone().map(ServeStream::Unix),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.try_clone().map(ServeStream::Vsock),
        }
    }

    /// Bound a stalled-but-alive peer so it can't pin the writer Mutex forever.
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match self {
            ServeStream::Unix(s) => s.set_write_timeout(dur),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.set_write_timeout(dur),
        }
    }
}

impl Read for ServeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ServeStream::Unix(s) => s.read(buf),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.read(buf),
        }
    }
}

impl Write for ServeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ServeStream::Unix(s) => s.write(buf),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            ServeStream::Unix(s) => s.flush(),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.flush(),
        }
    }
}

/// A connection's shared write half — both replies and pushed notifications go
/// through it, serialized by the Mutex (a reply and a notification can't interleave
/// bytes). The [`ServeStream`] enum keeps this one type across unix + vsock peers.
pub type SharedWriter = Arc<Mutex<ServeStream>>;

/// A peer subscribed to a resource: which connection, and the writer to push a
/// `notifications/resources/updated` to. Opaque — fields are private; construct +
/// mutate a registry through [`register_subscriber`] / [`drop_subscription`] /
/// [`remove_conn_subscriptions`] and fire pushes through the `notify_*` helpers.
pub struct Subscriber {
    conn: u64,
    writer: SharedWriter,
}

/// `uri` → its subscribers. Pushed when a resource changes. `Arc`-shared with the
/// background threads that mutate resource state (a run reaching a terminal status,
/// a reload landing, an event-ring growth).
pub type SubRegistry = Arc<Mutex<HashMap<String, Vec<Subscriber>>>>;

/// Register `conn` (with its `writer`) as a subscriber of `uri`, idempotently — a
/// second subscribe from the same connection is a no-op rather than a duplicate
/// push target. The embedder does its own gating (which URIs are subscribable, who
/// may subscribe) *before* calling this.
pub fn register_subscriber(subs: &SubRegistry, uri: &str, conn: u64, writer: &SharedWriter) {
    let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
    let list = g.entry(uri.to_string()).or_default();
    if !list.iter().any(|s| s.conn == conn) {
        list.push(Subscriber {
            conn,
            writer: Arc::clone(writer),
        });
    }
}

/// Drop `conn`'s subscription to a single `uri` (the `resources/unsubscribe` path).
/// Prunes the uri entry entirely once its last subscriber leaves.
pub fn drop_subscription(subs: &SubRegistry, uri: &str, conn: u64) {
    let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(list) = g.get_mut(uri) {
        list.retain(|s| s.conn != conn);
        if list.is_empty() {
            g.remove(uri);
        }
    }
}

/// Drop every subscription held by a (now-closed) connection — called when a
/// connection's reader loop ends so pushes never target a dead socket.
pub fn remove_conn_subscriptions(subs: &SubRegistry, conn: u64) {
    let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|_uri, list| {
        list.retain(|s| s.conn != conn);
        !list.is_empty()
    });
}

/// Push `notifications/resources/updated{uri}` to every current subscriber of
/// `uri`, **consuming** the subscription list (the resource changes exactly once —
/// e.g. a subagent run reaching its terminal status — so no entry should linger
/// after its one event). Best-effort: a write to a dead peer fails and is cleaned
/// up when that connection's reader loop ends. The lock is released before writing,
/// so a slow/blocked peer can't stall other notifications.
pub fn notify_resource_updated(subs: &SubRegistry, uri: &str) {
    let writers: Vec<SharedWriter> = {
        let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
        match g.remove(uri) {
            Some(list) => list.into_iter().map(|s| s.writer).collect(),
            None => return,
        }
    };
    push_updated(&writers, uri);
}

/// Like [`notify_resource_updated`] but **keeps** the subscriber list — for
/// resources that change REPEATEDLY (a run aggregate on each spawn, a warm session
/// on each turn boundary, `config/effective` on each reload, an event ring on each
/// batch). Cloning the writers under the lock (then releasing it before writing)
/// keeps the entry intact for the next emission. Dead peers are pruned when their
/// reader loop ends ([`remove_conn_subscriptions`]).
pub fn notify_resource_updated_keep(subs: &SubRegistry, uri: &str) {
    let writers: Vec<SharedWriter> = {
        let g = subs.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(uri) {
            Some(list) => list.iter().map(|s| Arc::clone(&s.writer)).collect(),
            None => return,
        }
    };
    push_updated(&writers, uri);
}

fn push_updated(writers: &[SharedWriter], uri: &str) {
    let note = Notification::new(method::NOTIFY_RESOURCES_UPDATED, Some(json!({ "uri": uri })));
    for w in writers {
        if let Ok(mut wl) = w.lock() {
            let _ = frame::write_line(&mut *wl, &note);
        }
    }
}

/// Broadcast a payload-free `note` to every DISTINCT writer currently in the
/// registry — for connection-scoped notifications that aren't tied to a single uri
/// (e.g. `notifications/tools/list_changed` after a hot reload changed the tool
/// set). A connection subscribed to several resources is written to once. Dead
/// writers are pruned by their own reader loop.
pub fn broadcast_distinct(subs: &SubRegistry, note: &Notification) {
    let writers: Vec<SharedWriter> = {
        let g = subs.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen: Vec<*const Mutex<ServeStream>> = Vec::new();
        let mut out: Vec<SharedWriter> = Vec::new();
        for list in g.values() {
            for s in list {
                let ptr = Arc::as_ptr(&s.writer);
                if !seen.contains(&ptr) {
                    seen.push(ptr);
                    out.push(Arc::clone(&s.writer));
                }
            }
        }
        out
    };
    for w in writers {
        if let Ok(mut wl) = w.lock() {
            let _ = frame::write_line(&mut *wl, note);
        }
    }
}

// ---------------------------------------------------------------------------
// The connection framework: the lifecycle/version machinery, the `Handler` seam,
// and the blocking thread-per-connection listeners. An embedder implements
// [`Handler`] for its domain surface and calls [`serve_unix`] / [`serve_vsock`];
// everything below is transport- and domain-agnostic.
// ---------------------------------------------------------------------------

/// The embedder's domain seam. The framework owns the transport, the framing, the
/// connection lifecycle, and the subscription registry; the `Handler` supplies the
/// *meaning* — which tools/resources exist, who may call/read/subscribe to what.
///
/// Lifecycle (`initialize` / `server/discover` / `ping`) is NOT routed here: a
/// handler answers it once, version-aware, by calling [`lifecycle_response`] at the
/// top of its [`dispatch`](Handler::dispatch) (so the multi-version negotiation
/// lives in one place). Everything else — `tools/*`, `resources/*`, and any custom
/// method — flows through `dispatch`.
pub trait Handler: Send + Sync + 'static {
    /// Route one request to a response. `origin` is the caller's trust domain;
    /// `writer`/`conn` identify the connection so a `resources/subscribe` can
    /// register a push target via [`register_subscriber`]. Called from the
    /// connection's own thread — implementations do their own locking.
    fn dispatch(&self, req: Request, origin: PeerOrigin, writer: &SharedWriter, conn: u64)
    -> Response;

    /// Called once when a connection is accepted (before its first request), for
    /// logging/metrics. Default: nothing.
    fn on_connect(&self, _origin: PeerOrigin, _conn: u64) {}

    /// Called once when a connection's reader loop ends. The framework has already
    /// dropped the connection's subscriptions; this is for logging/metrics or any
    /// embedder-side per-connection cleanup. Default: nothing.
    fn on_disconnect(&self, _origin: PeerOrigin, _conn: u64) {}
}

/// Answer the three lifecycle methods every MCP server must handle, in ONE place,
/// version-aware across both eras — the server-side mirror of the client's version
/// negotiation. Returns `Some(response)` for `initialize` / `server/discover` /
/// `ping`, or `None` if `req.method` is a domain method the [`Handler`] must route.
///
///   * `initialize` (legacy handshake): negotiate the protocol version — echo the
///     peer's requested version when it's [supported](crate::version::is_supported_version),
///     else fall back to our latest legacy [`PROTOCOL_VERSION`](crate::version::PROTOCOL_VERSION).
///   * `server/discover` (modern, stateless): advertise the full
///     [`SUPPORTED_PROTOCOL_VERSIONS`](crate::version::SUPPORTED_PROTOCOL_VERSIONS)
///     list + capabilities in one call, so a modern client needn't fall back to the
///     legacy handshake. This is what makes the embedder a *dual-era server*.
///   * `ping`: an empty result.
///
/// `server_info` is the `{name, version}` object and `capabilities` the advertised
/// capability object; both are echoed verbatim into the two lifecycle replies. When
/// the crate gains support for a new protocol version, both replies pick it up here
/// without the embedder changing anything.
pub fn lifecycle_response(
    req: &Request,
    server_info: &Value,
    capabilities: &Value,
) -> Option<Response> {
    match req.method.as_str() {
        "initialize" => {
            let requested = req
                .params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str);
            let version = match requested {
                Some(v) if crate::version::is_supported_version(v) => v,
                _ => crate::version::PROTOCOL_VERSION,
            };
            Some(Response::ok(
                req.id.clone(),
                json!({
                    "protocolVersion": version,
                    "capabilities": capabilities,
                    "serverInfo": server_info,
                }),
            ))
        }
        method::SERVER_DISCOVER => Some(Response::ok(
            req.id.clone(),
            json!({
                "resultType": "complete",
                "supportedVersions": crate::version::SUPPORTED_PROTOCOL_VERSIONS,
                "capabilities": capabilities,
                "serverInfo": server_info,
            }),
        )),
        "ping" => Some(Response::ok(req.id.clone(), json!({}))),
        _ => None,
    }
}

/// Serve one accepted connection to completion: the blocking NDJSON read loop.
/// Requests get a reply (through the shared writer, which a background thread may
/// also push notifications on — the Mutex serializes them); notifications
/// (`initialized`, …) are read and dropped. On EOF/hangup the connection's
/// subscriptions are dropped so no push ever targets a dead socket. A write timeout
/// bounds a stalled-but-alive peer so it can't pin the writer Mutex (and a pushing
/// thread) forever.
pub fn handle_conn(
    stream: ServeStream,
    origin: PeerOrigin,
    handler: &Arc<dyn Handler>,
    subs: &SubRegistry,
    conn_counter: &AtomicU64,
    write_timeout: Duration,
) {
    let writer: SharedWriter = match stream.try_clone() {
        Ok(w) => {
            let _ = w.set_write_timeout(Some(write_timeout));
            Arc::new(Mutex::new(w))
        }
        Err(_) => return,
    };
    let conn = conn_counter.fetch_add(1, Ordering::Relaxed);
    handler.on_connect(origin, conn);
    let mut reader = BufReader::new(stream);
    while let Ok(Some(bytes)) = frame::read_line(&mut reader) {
        if let Ok(Incoming::Request(req)) = serde_json::from_slice::<Incoming>(&bytes) {
            let resp = handler.dispatch(req, origin, &writer, conn);
            let wrote = writer
                .lock()
                .is_ok_and(|mut w| frame::write_line(&mut *w, &resp).is_ok());
            if !wrote {
                break; // peer hung up mid-reply
            }
        }
    }
    remove_conn_subscriptions(subs, conn); // don't push to a dead socket
    handler.on_disconnect(origin, conn);
}

/// Bind a unix socket for serving, clearing any stale socket file first. Returned
/// separately from the accept loop so the caller can log/act on a successful bind
/// (or propagate the bind error) before the accept thread starts.
pub fn bind_unix(path: &str) -> io::Result<UnixListener> {
    // A stale socket from a crashed prior run would block the bind; clear it.
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path)
}

/// Spawn the background accept thread for `listener`: one blocking thread per
/// connection, each running [`handle_conn`] against `handler`. Peers arrive in the
/// [`PeerOrigin::Management`] trust domain (they dialed a listener). Returns once
/// the accept thread is spawned; a thread-spawn failure is surfaced as the error.
pub fn spawn_accept_unix(
    listener: UnixListener,
    handler: Arc<dyn Handler>,
    subs: SubRegistry,
    conn_counter: Arc<AtomicU64>,
    write_timeout: Duration,
) -> io::Result<()> {
    thread::Builder::new()
        .name("serve-mcp".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = Arc::clone(&handler);
                let subs = Arc::clone(&subs);
                let conn_counter = Arc::clone(&conn_counter);
                thread::Builder::new()
                    .name("serve-mcp-conn".into())
                    .spawn(move || {
                        handle_conn(
                            ServeStream::Unix(stream),
                            PeerOrigin::Management,
                            &handler,
                            &subs,
                            &conn_counter,
                            write_timeout,
                        )
                    })
                    .ok();
            }
        })
        .map(|_| ())
}

/// Bind an AF_VSOCK `(cid, port)` for serving — the management transport (RFC 0015
/// §3.2). The vsock counterpart of [`bind_unix`].
#[cfg(feature = "vsock")]
pub fn bind_vsock(cid: u32, port: u32) -> io::Result<vsock::VsockListener> {
    vsock::VsockListener::bind_with_cid_port(cid, port)
}

/// Spawn the background accept thread for a vsock `listener` — byte-for-byte
/// [`spawn_accept_unix`] with the socket type swapped (the same [`handle_conn`], no
/// new framing). Peers arrive in [`PeerOrigin::Management`].
#[cfg(feature = "vsock")]
pub fn spawn_accept_vsock(
    listener: vsock::VsockListener,
    handler: Arc<dyn Handler>,
    subs: SubRegistry,
    conn_counter: Arc<AtomicU64>,
    write_timeout: Duration,
) -> io::Result<()> {
    thread::Builder::new()
        .name("serve-mcp-vsock".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = Arc::clone(&handler);
                let subs = Arc::clone(&subs);
                let conn_counter = Arc::clone(&conn_counter);
                thread::Builder::new()
                    .name("serve-mcp-conn".into())
                    .spawn(move || {
                        handle_conn(
                            ServeStream::Vsock(stream),
                            PeerOrigin::Management,
                            &handler,
                            &subs,
                            &conn_counter,
                            write_timeout,
                        )
                    })
                    .ok();
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Request;
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;

    // --- lifecycle / version negotiation ---------------------------------

    fn info() -> Value {
        json!({"name": "test-server", "version": "9.9.9"})
    }
    fn caps() -> Value {
        json!({"tools": {}, "resources": {"subscribe": true}})
    }

    #[test]
    fn initialize_echoes_a_supported_requested_version() {
        // A legacy client requesting a version we support gets it echoed back.
        let want = crate::version::SUPPORTED_PROTOCOL_VERSIONS[1]; // a non-latest supported one
        let req = Request::new(1, "initialize", Some(json!({"protocolVersion": want})));
        let resp = lifecycle_response(&req, &info(), &caps()).expect("lifecycle handled");
        let r = resp.result.expect("ok");
        assert_eq!(r["protocolVersion"], want);
        assert_eq!(r["serverInfo"]["name"], "test-server");
        assert!(r["capabilities"]["resources"]["subscribe"].as_bool().unwrap());
    }

    #[test]
    fn initialize_falls_back_to_latest_legacy_for_an_unsupported_version() {
        // An unknown/too-old version isn't echoed — we answer with our own latest.
        let req = Request::new(1, "initialize", Some(json!({"protocolVersion": "1999-01-01"})));
        let resp = lifecycle_response(&req, &info(), &caps()).expect("handled");
        let r = resp.result.expect("ok");
        assert_eq!(r["protocolVersion"], crate::version::PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_defaults_when_no_version_is_requested() {
        let req = Request::new(1, "initialize", Some(json!({})));
        let resp = lifecycle_response(&req, &info(), &caps()).expect("handled");
        assert_eq!(
            resp.result.expect("ok")["protocolVersion"],
            crate::version::PROTOCOL_VERSION
        );
    }

    #[test]
    fn server_discover_advertises_every_supported_version() {
        // The modern stateless probe learns our full version list + caps in one call.
        let req = Request::new(7, method::SERVER_DISCOVER, None);
        let resp = lifecycle_response(&req, &info(), &caps()).expect("handled");
        let r = resp.result.expect("ok");
        assert_eq!(r["resultType"], "complete");
        let listed = r["supportedVersions"].as_array().expect("array");
        assert_eq!(listed.len(), crate::version::SUPPORTED_PROTOCOL_VERSIONS.len());
        assert!(listed.iter().any(|v| v == crate::version::FIRST_MODERN_VERSION));
        assert!(listed.iter().any(|v| v == crate::version::PROTOCOL_VERSION));
        assert_eq!(r["serverInfo"]["name"], "test-server");
    }

    #[test]
    fn ping_is_an_empty_ok_and_domain_methods_fall_through() {
        let ping = Request::new(1, "ping", None);
        assert_eq!(
            lifecycle_response(&ping, &info(), &caps())
                .expect("handled")
                .result,
            Some(json!({}))
        );
        // A non-lifecycle method is the handler's job — the helper declines it.
        let dom = Request::new(2, "tools/call", None);
        assert!(lifecycle_response(&dom, &info(), &caps()).is_none());
    }

    // --- subscription registry ------------------------------------------

    /// A writer whose pushes can be read back off its peer end.
    fn wired() -> (SharedWriter, BufReader<UnixStream>) {
        let (tx, rx) = UnixStream::pair().unwrap();
        // Bound the read so a "should push nothing" assertion can't hang.
        rx.set_read_timeout(Some(Duration::from_millis(250))).unwrap();
        (Arc::new(Mutex::new(ServeStream::Unix(tx))), BufReader::new(rx))
    }

    /// Read one pushed notification and return `(method, uri-or-empty)`.
    fn read_note(rx: &mut BufReader<UnixStream>) -> (String, String) {
        let bytes = frame::read_line(rx).expect("read").expect("a frame");
        let v: Value = serde_json::from_slice(&bytes).expect("json");
        let method = v["method"].as_str().unwrap_or_default().to_string();
        let uri = v["params"]["uri"].as_str().unwrap_or_default().to_string();
        (method, uri)
    }

    /// Assert nothing more was pushed (the bounded read times out / hits EOF).
    fn assert_silent(rx: &mut BufReader<UnixStream>) {
        assert!(
            !matches!(frame::read_line(rx), Ok(Some(_))),
            "expected no further push"
        );
    }

    #[test]
    fn register_is_idempotent_per_connection() {
        let subs: SubRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (w, _rx) = wired();
        register_subscriber(&subs, "res://a", 1, &w);
        register_subscriber(&subs, "res://a", 1, &w); // same conn again — no dup
        let g = subs.lock().unwrap();
        assert_eq!(g.get("res://a").unwrap().len(), 1);
    }

    #[test]
    fn notify_updated_consumes_but_keep_retains() {
        let subs: SubRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (w, mut rx) = wired();
        register_subscriber(&subs, "res://run", 1, &w);

        // keep-variant fires and leaves the subscription in place …
        notify_resource_updated_keep(&subs, "res://run");
        let (m, uri) = read_note(&mut rx);
        assert_eq!(m, method::NOTIFY_RESOURCES_UPDATED);
        assert_eq!(uri, "res://run");
        assert!(subs.lock().unwrap().contains_key("res://run"));

        // … the consume-variant fires once and drops the entry.
        notify_resource_updated(&subs, "res://run");
        let (_m, uri2) = read_note(&mut rx);
        assert_eq!(uri2, "res://run");
        assert!(!subs.lock().unwrap().contains_key("res://run"));
    }

    #[test]
    fn drop_and_conn_cleanup_remove_subscriptions() {
        let subs: SubRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (w, _rx) = wired();
        register_subscriber(&subs, "res://a", 1, &w);
        register_subscriber(&subs, "res://b", 1, &w);

        drop_subscription(&subs, "res://a", 1);
        assert!(!subs.lock().unwrap().contains_key("res://a"));
        assert!(subs.lock().unwrap().contains_key("res://b"));

        remove_conn_subscriptions(&subs, 1);
        assert!(subs.lock().unwrap().is_empty());
    }

    #[test]
    fn broadcast_distinct_writes_once_per_connection() {
        let subs: SubRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (w, mut rx) = wired();
        // Same connection subscribed to two resources → one distinct writer.
        register_subscriber(&subs, "res://a", 1, &w);
        register_subscriber(&subs, "res://b", 1, &w);

        let note = Notification::new(method::NOTIFY_TOOLS_LIST_CHANGED, None);
        broadcast_distinct(&subs, &note);

        let (m, _) = read_note(&mut rx);
        assert_eq!(m, method::NOTIFY_TOOLS_LIST_CHANGED);
        assert_silent(&mut rx); // not written twice
    }
}
