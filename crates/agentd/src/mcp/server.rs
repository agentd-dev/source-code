//! agentd serving its own MCP over a unix socket — composability. RFC 0005. [feature: serve-mcp]
//!
//! A peer (another agentd, an MCP client, or a driving harness) `initialize`s
//! against `--serve-mcp unix:PATH` and calls agentd's tools. Transport per RFC
//! §3.6: a **blocking `UnixListener`, thread-per-connection** (no async, no
//! mio) speaking the same NDJSON JSON-RPC codec as the MCP *client* (`json/`,
//! RFC 0004). It exposes a read-only `status` tool and the action tools
//! `subagent.spawn` (sync / async / warm), `subagent.send`, `subagent.status`,
//! and `subagent.cancel` (RFC §3.2), plus the agentd:// resource scheme with
//! resources/subscribe push.

use crate::config::Config;
use crate::json::{self, Id, Incoming, Notification, Request, Response, frame};
use crate::obs::log::Logger;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload};
use crate::supervisor::cgroup;
use crate::supervisor::reactor::{SuperviseResult, supervise_cancellable, supervise_once};
use crate::supervisor::spawn::{Subagent, spawn};
use crate::supervisor::tree::NodeId;
use crate::wire::mcp::{PROTOCOL_VERSION, method};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Which transport a connection arrived on, and therefore its trust domain
/// (RFC 0015 §3.3-§3.4). `Stdio` is the agent's own driving harness (the
/// process's stdio); `Management` is a peer that connected to a `--serve-mcp`
/// listener (unix socket / vsock). This chunk only *plumbs* it — it is logged on
/// connect and carried in the per-connection context; the operator-tool gate that
/// reads it lands in the next chunk (RFC 0015 §3.4). Stored so it isn't dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOrigin {
    /// The process's own stdio (the driving harness).
    Stdio,
    /// A peer on a `--serve-mcp` listener (unix / vsock) — the management domain.
    Management,
}

impl PeerOrigin {
    fn as_str(self) -> &'static str {
        match self {
            PeerOrigin::Stdio => "stdio",
            PeerOrigin::Management => "management",
        }
    }
}

/// The served-MCP transport, type-erased to one concrete enum so the connection
/// registry (`SharedWriter`, `Subscriber`) stays monomorphic across transports
/// while the *same* [`handle_conn`] code serves each. Both variants are
/// `Read + Write` with a `try_clone` (the connection's write half is shared with
/// the run threads that push notifications), so the NDJSON framing, threading,
/// and dispatch are entirely transport-agnostic (RFC 0015 §3.2 — "the unix server
/// with the socket type swapped").
pub enum ServeStream {
    Unix(UnixStream),
    #[cfg(feature = "vsock")]
    Vsock(vsock::VsockStream),
}

impl ServeStream {
    /// Clone the handle (a second fd onto the same connection) for the shared
    /// write half. Mirrors `UnixStream::try_clone`.
    fn try_clone(&self) -> io::Result<ServeStream> {
        match self {
            ServeStream::Unix(s) => s.try_clone().map(ServeStream::Unix),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.try_clone().map(ServeStream::Vsock),
        }
    }

    /// Bound a stalled-but-alive peer so it can't pin the writer Mutex forever.
    fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
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

/// Cap on concurrent peer-driven `subagent.spawn` runs in flight (bounds a peer
/// spamming the socket; each run is also bounded by the base payload's limits).
const MAX_INFLIGHT_SPAWNS: usize = 4;
/// Cap on the result text returned to the peer (~chars).
const RESULT_CAP: usize = 4096;
/// Cap on tracked served async sessions (running + finished). When exceeded, the
/// oldest *finished* session is evicted so a long-lived daemon can't grow without
/// bound; live sessions are never evicted (they are bounded by the permit cap).
const MAX_SESSIONS: usize = 64;
/// Cap on concurrent served **warm** sessions (each is a live subagent process the
/// peer drives with `subagent.send`). Bounded independently of the async permit so
/// long-lived warm sessions can't starve one-shot `subagent.spawn` runs; a peer
/// that hits the cap must `subagent.cancel` an idle one.
const MAX_WARM_SESSIONS: usize = 8;

/// A served **warm** session: a live subagent the peer drives turn-by-turn with
/// `subagent.send`. Held by the server (not reactor-managed); drained lazily on
/// send/status — each `AgentMsg::Turn` is one reply, death is the channel
/// disconnecting. `Subagent`'s Drop kills + reaps it when the session is removed.
struct WarmServed {
    sub: Subagent,
    rx: Receiver<(NodeId, AgentMsg)>,
    /// The most recent completed turn's distilled `(result, is_error)`.
    last: Option<(String, bool)>,
    /// Turns observed-complete (drained `AgentMsg::Turn`s).
    turns: u32,
    /// Turns queued-or-running but not yet observed complete: the instruction's
    /// first turn (1 at spawn) plus one per `subagent.send`, minus one per drained
    /// `Turn`. `> 0` means a turn is in flight — the peer's `busy` signal.
    pending: u32,
    done: bool,
    started: Instant,
}

/// Handle → live warm session.
type WarmRegistry = Arc<Mutex<HashMap<String, WarmServed>>>;

/// Drain a warm session's channel: record completed turns + detect end. Returns
/// `true` if the session's observable state advanced (a turn completed or it
/// ended) — the caller pushes a `agentd://session/<handle>` update on `true`.
/// Idempotent: a no-op drain on an already-`done` session returns `false`.
fn drain_warm(w: &mut WarmServed) -> bool {
    if w.done {
        return false;
    }
    let mut changed = false;
    loop {
        match w.rx.try_recv() {
            Ok((_, AgentMsg::Turn { outcome })) => {
                w.turns += 1;
                w.pending = w.pending.saturating_sub(1); // a queued turn completed
                w.last = Some((
                    distill(&outcome.result),
                    !matches!(
                        outcome.status,
                        crate::agentloop::stop::TerminalStatus::Completed
                    ),
                ));
                changed = true; // a turn boundary — the session resource changed
            }
            Ok((_, AgentMsg::Result { .. })) | Ok((_, AgentMsg::Failed { .. })) => {
                w.done = true;
                return true;
            }
            Ok(_) => {} // Ready / Pong / Event / Usage
            Err(TryRecvError::Empty) => return changed,
            Err(TryRecvError::Disconnected) => {
                w.done = true;
                return true;
            }
        }
    }
}

/// Drain a warm session and, if its state advanced, push a
/// `notifications/resources/updated` for `agentd://session/<handle>` to its
/// subscribers (the keep-variant — a warm session fires on *every* turn boundary,
/// so the subscription must survive each emission). Used on every path that drains
/// a still-tracked session by handle.
fn drain_warm_notify(handle: &str, w: &mut WarmServed, subs: &SubRegistry) {
    if drain_warm(w) {
        notify_resource_updated_keep(subs, &crate::agentd_uri::session_uri(handle));
    }
}

/// The structured state body for a warm session (the `subagent.status` reply).
fn warm_body(handle: &str, w: &WarmServed) -> Value {
    let (result, is_error) = match &w.last {
        Some((r, e)) => (json!(r), *e),
        None => (Value::Null, false),
    };
    json!({
        "handle": handle, "warm": true, "alive": !w.done, "done": w.done,
        // `busy` lets a peer distinguish "a turn is running" from "idle, last_result
        // is fresh" without remembering the pre-send turn count: poll until !busy.
        "busy": !w.done && w.pending > 0,
        "turns": w.turns, "last_result": result, "last_is_error": is_error,
        "age_ms": w.started.elapsed().as_millis() as u64,
    })
}

/// The lifecycle of a served **async** run, tracked by handle so a peer can poll
/// `subagent.status` / read `agentd://subagent/<handle>` / `subagent.cancel` it.
enum ServedStatus {
    Running,
    Done {
        status: String,
        partial: bool,
        result: Value,
    },
    Failed(String),
    Cancelled,
}

impl ServedStatus {
    fn is_terminal(&self) -> bool {
        !matches!(self, ServedStatus::Running)
    }
}

/// One tracked served async run: its state, its per-run cancel flag, and when it
/// started (for age reporting + oldest-first eviction).
struct ServedSession {
    status: ServedStatus,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

/// Handle → tracked session. Shared (Arc<Mutex>) across the accept/connection
/// threads and each async run's background thread.
type Registry = Arc<Mutex<HashMap<String, ServedSession>>>;

/// A connection's shared write half — both replies and pushed notifications go
/// through it, serialized by the Mutex (a reply and a notification can't interleave
/// bytes). The [`ServeStream`] enum keeps this one type across unix + vsock peers.
type SharedWriter = Arc<Mutex<ServeStream>>;

/// A peer subscribed to an `agentd://` resource: which connection, and the writer
/// to push a `notifications/resources/updated` to.
struct Subscriber {
    conn: u64,
    writer: SharedWriter,
}

/// uri → its subscribers. Pushed when a served session's resource changes (a run
/// reaches a terminal status). Arc-shared with each async run's background thread.
type SubRegistry = Arc<Mutex<HashMap<String, Vec<Subscriber>>>>;

/// The structured state body for one session (shared by the `subagent.status`
/// tool and the `agentd://subagent/<handle>` resource).
fn session_body(handle: &str, s: &ServedSession) -> Value {
    match &s.status {
        ServedStatus::Running => {
            json!({"handle": handle, "status": "running", "done": false, "age_ms": s.started.elapsed().as_millis() as u64})
        }
        ServedStatus::Done {
            status,
            partial,
            result,
        } => {
            json!({"handle": handle, "status": status, "done": true, "partial": partial, "result": result})
        }
        ServedStatus::Failed(e) => {
            json!({"handle": handle, "status": "failed", "done": true, "error": e})
        }
        ServedStatus::Cancelled => json!({"handle": handle, "status": "cancelled", "done": true}),
    }
}

/// Context for the served tools. `run_id`/`mode`/`started` back `status`; the
/// rest back `subagent.spawn` (a peer delegating work). Shared across
/// connections; the atomics enforce the concurrency cap + mint handles.
pub struct ServeCtx {
    run_id: String,
    mode: String,
    started: Instant,
    exe: PathBuf,
    base: SpawnPayload,
    drain_timeout: Duration,
    inflight: Arc<AtomicUsize>,
    counter: Arc<AtomicU64>,
    /// Tracked served async runs, by handle.
    sessions: Registry,
    /// Live warm sessions (driven by `subagent.send`), by handle.
    warm: WarmRegistry,
    /// Resource subscriptions, by uri → subscribers (for push notifications).
    subscriptions: SubRegistry,
    /// Monotonic per-connection id (to scope + clean up subscriptions).
    conn_counter: Arc<AtomicU64>,
    /// The resolved daemon config — the source for the `agentd://capabilities`
    /// self-description manifest (RFC 0015 §3.4). Arc-shared so the per-read
    /// build borrows it without cloning the struct.
    config: Arc<Config>,
}

impl ServeCtx {
    pub fn new(
        run_id: String,
        mode: String,
        exe: PathBuf,
        base: SpawnPayload,
        drain_timeout: Duration,
        config: Arc<Config>,
    ) -> ServeCtx {
        ServeCtx {
            run_id,
            mode,
            started: Instant::now(),
            exe,
            base,
            drain_timeout,
            inflight: Arc::new(AtomicUsize::new(0)),
            counter: Arc::new(AtomicU64::new(0)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            warm: Arc::new(Mutex::new(HashMap::new())),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            conn_counter: Arc::new(AtomicU64::new(0)),
            config,
        }
    }

    /// The live `agentd://capabilities` manifest — this daemon's self-description
    /// plus cheap liveness counters lifted off the same atomics the `status`
    /// surface reads (RFC 0015 §3.4). Built `live=true` from the running daemon.
    fn capabilities_body(&self) -> Value {
        // ONE builder for the one-shot `agentd --capabilities` and this live
        // resource, so they never drift (RFC 0015 §5.2). Identity is env-only and
        // cheap, so build it per read rather than threading it through ServeCtx.
        let identity = crate::identity::Identity::from_env(&self.run_id);
        crate::capabilities::manifest(&self.config, &identity, true)
    }

    /// This agentd's own run/health state — the single source of truth for both
    /// the `status` tool and the `agentd://status` resource.
    fn status_body(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "mode": self.mode,
            "version": crate::VERSION,
            "pid": std::process::id(),
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "inflight_spawns": self.inflight.load(Ordering::Relaxed),
            "total_spawns": self.counter.load(Ordering::Relaxed),
        })
    }

    /// The aggregate state of this served run — what `agentd://run/<run_id>`
    /// reads + pushes on each change. The `root` handle convention names the run's
    /// own node (depth 0); `status` is "running" for the daemon's life. Spawn
    /// counts come straight from the same atomics `status_body` reads (no token
    /// aggregation exists yet — RFC 0005 §3.3 reports counts, not totals).
    fn run_body(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "mode": self.mode,
            "root": "0", // the run's own node id (depth-0 root) by convention
            "status": "running",
            "inflight_spawns": self.inflight.load(Ordering::Relaxed),
            "total_spawns": self.counter.load(Ordering::Relaxed),
            "uptime_ms": self.started.elapsed().as_millis() as u64,
        })
    }
}

/// RAII concurrency permit for a served spawn; releases the slot on drop.
struct SpawnGuard(Arc<AtomicUsize>);

impl SpawnGuard {
    fn acquire(slots: &Arc<AtomicUsize>, max: usize) -> Option<SpawnGuard> {
        if slots.fetch_add(1, Ordering::Relaxed) >= max {
            slots.fetch_sub(1, Ordering::Relaxed);
            None
        } else {
            Some(SpawnGuard(Arc::clone(slots)))
        }
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A shutdown handle for the served self-MCP: lets the daemon drain in-flight
/// served runs before it exits (their reactors also self-drain on the process
/// SIGTERM; this makes the daemon *wait* for them rather than guillotine their
/// subtrees at `process::exit`).
pub struct ServeHandle {
    sessions: Registry,
    warm: WarmRegistry,
    inflight: Arc<AtomicUsize>,
}

impl ServeHandle {
    /// Ask every in-flight served run to cancel, then wait (bounded by `timeout`)
    /// for them to finish so their subtrees drain gracefully. Warm sessions are
    /// cancelled + dropped (their `Subagent` Drop kills + reaps the subtree).
    pub fn drain(&self, timeout: Duration) {
        if let Ok(mut warm) = self.warm.lock() {
            for w in warm.values_mut() {
                let _ = w.sub.send(&ControlMsg::Cancel {
                    reason: "drain".into(),
                });
            }
            // Take the sessions out and drop them after releasing the lock, so the N
            // Subagent Drops (SIGKILL + waitpid each) don't run holding the registry.
            let drained = std::mem::take(&mut *warm);
            drop(warm);
            drop(drained);
        }
        if let Ok(reg) = self.sessions.lock() {
            for s in reg.values() {
                s.cancel.store(true, Ordering::Relaxed);
            }
        }
        let deadline = Instant::now() + timeout;
        while self.inflight.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Bind `path` and serve the self-MCP on a background accept thread (one thread
/// per connection). Returns a [`ServeHandle`] (for shutdown drain), or the bind
/// error so the caller decides if it's fatal.
pub fn serve(path: &str, ctx: ServeCtx, log: Logger) -> std::io::Result<ServeHandle> {
    // A stale socket from a crashed prior run would block the bind; clear it.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    log.info(
        "mcp.serving",
        json!({"transport": "unix", "path": path, "tools": ["status", "subagent.spawn", "subagent.send", "subagent.status", "subagent.cancel"], "resources": ["agentd://status", "agentd://capabilities", crate::agentd_uri::run_uri(&ctx.run_id)]}),
    );
    let handle = ServeHandle {
        sessions: Arc::clone(&ctx.sessions),
        warm: Arc::clone(&ctx.warm),
        inflight: Arc::clone(&ctx.inflight),
    };
    let ctx = Arc::new(ctx);
    thread::Builder::new()
        .name("serve-mcp".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let ctx = Arc::clone(&ctx);
                let log = log.clone();
                // One blocking thread per peer connection (RFC §3.6). A unix
                // `--serve-mcp` peer is in the management trust domain (§3.3).
                thread::Builder::new()
                    .name("serve-mcp-conn".into())
                    .spawn(move || {
                        handle_conn(
                            ServeStream::Unix(stream),
                            PeerOrigin::Management,
                            &ctx,
                            &log,
                        )
                    })
                    .ok();
            }
        })?;
    Ok(handle)
}

/// Bind a vsock `(cid, port)` and serve the self-MCP — the **management
/// transport** (RFC 0015 §3.2). Byte-for-byte the unix server with the socket
/// type swapped: a blocking `VsockListener`, thread-per-connection, the same
/// generic [`handle_conn`]; no async, no new framing. Peers arrive in
/// [`PeerOrigin::Management`]. Returns a [`ServeHandle`] for shutdown drain (the
/// session/warm/inflight registries are shared with `ctx`), or the bind error.
#[cfg(feature = "vsock")]
pub fn serve_vsock(
    cid: u32,
    port: u32,
    ctx: ServeCtx,
    log: Logger,
) -> std::io::Result<ServeHandle> {
    let listener = vsock::VsockListener::bind_with_cid_port(cid, port)?;
    log.info(
        "mcp.serving",
        json!({"transport": "vsock", "cid": cid, "port": port, "tools": ["status", "subagent.spawn", "subagent.send", "subagent.status", "subagent.cancel"], "resources": ["agentd://status", "agentd://capabilities", crate::agentd_uri::run_uri(&ctx.run_id)]}),
    );
    let handle = ServeHandle {
        sessions: Arc::clone(&ctx.sessions),
        warm: Arc::clone(&ctx.warm),
        inflight: Arc::clone(&ctx.inflight),
    };
    let ctx = Arc::new(ctx);
    thread::Builder::new()
        .name("serve-mcp-vsock".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let ctx = Arc::clone(&ctx);
                let log = log.clone();
                thread::Builder::new()
                    .name("serve-mcp-conn".into())
                    .spawn(move || {
                        handle_conn(
                            ServeStream::Vsock(stream),
                            PeerOrigin::Management,
                            &ctx,
                            &log,
                        )
                    })
                    .ok();
            }
        })?;
    Ok(handle)
}

fn handle_conn(stream: ServeStream, origin: PeerOrigin, ctx: &ServeCtx, log: &Logger) {
    // The write half is shared (Arc<Mutex>) so a run thread can push a
    // notification on it concurrently with this thread writing a reply. A write
    // timeout bounds a stalled-but-alive peer so it can't pin the writer Mutex
    // (and a run thread's notification) forever — matching the rest of the crate's
    // sockets.
    let writer: SharedWriter = match stream.try_clone() {
        Ok(w) => {
            let _ = w.set_write_timeout(Some(ctx.drain_timeout));
            Arc::new(Mutex::new(w))
        }
        Err(_) => return,
    };
    let conn = ctx.conn_counter.fetch_add(1, Ordering::Relaxed);
    log.info(
        "mcp.connect",
        json!({"origin": origin.as_str(), "conn": conn}),
    );
    let mut reader = BufReader::new(stream);
    while let Ok(Some(bytes)) = frame::read_line(&mut reader) {
        // Requests get a reply; notifications (initialized, …) do not.
        if let Ok(Incoming::Request(req)) = serde_json::from_slice::<Incoming>(&bytes) {
            let resp = dispatch(req, ctx, origin, &writer, conn, log);
            let wrote = writer
                .lock()
                .is_ok_and(|mut w| frame::write_line(&mut *w, &resp).is_ok());
            if !wrote {
                break; // peer hung up mid-reply
            }
        }
    }
    remove_conn_subscriptions(ctx, conn); // don't push to a dead socket
    log.debug(
        "mcp.disconnect",
        json!({"origin": origin.as_str(), "conn": conn}),
    );
}

/// Route one JSON-RPC request to a response. `writer`/`conn` identify the calling
/// connection so `resources/subscribe` can register a push target. `origin` is the
/// caller's trust domain (RFC 0015 §3.4) — carried through so the next chunk's
/// operator-tool gate can refuse `Stdio`-origin management calls; this chunk's
/// surface (incl. `agentd://capabilities`) is readable on every origin.
fn dispatch(
    req: Request,
    ctx: &ServeCtx,
    origin: PeerOrigin,
    writer: &SharedWriter,
    conn: u64,
    log: &Logger,
) -> Response {
    let _ = origin; // chunk 3 (RFC 0015 §3.4) reads this to gate operator tools.
    match req.method.as_str() {
        "initialize" => Response::ok(
            req.id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                // `resources.subscribe` advertises that a peer can subscribe to an
                // agentd:// resource and be pushed updates (e.g. a run completing).
                "capabilities": {"tools": {}, "resources": {"subscribe": true}},
                "serverInfo": {"name": "agentd", "version": crate::VERSION}
            }),
        ),
        "ping" => Response::ok(req.id, json!({})),
        "tools/list" => Response::ok(
            req.id,
            json!({"tools": [status_tool_def(), spawn_tool_def(), send_tool_def(), session_status_tool_def(), session_cancel_tool_def()]}),
        ),
        "tools/call" => tools_call(req, ctx, log),
        "resources/list" => Response::ok(req.id, json!({"resources": resource_list(ctx)})),
        "resources/read" => resources_read(req, ctx),
        "resources/subscribe" => subscribe_resource(req, ctx, writer, conn),
        "resources/unsubscribe" => unsubscribe_resource(req, ctx, conn),
        other => Response::err(
            req.id,
            json::METHOD_NOT_FOUND,
            format!("unsupported method: {other}"),
        ),
    }
}

/// `resources/subscribe`: register this connection to be pushed a
/// `notifications/resources/updated` when `uri`'s state changes. Three resources
/// are subscribable:
///   * `agentd://subagent/<handle>` — a **running** async run; fires exactly once
///     (on completion), so its subscription is *consumed* when it fires.
///   * `agentd://run/<run_id>` (this run) — fires REPEATEDLY (each spawn / each
///     terminal-run change); the subscription is *kept*.
///   * `agentd://session/<handle>` — a **live** (non-done) warm session; fires
///     REPEATEDLY (each warm-turn boundary); the subscription is *kept*.
///
/// An unknown / already-finished handle (or the read-only `agentd://status`) is
/// rejected so the peer `resources/read`s it instead; this also avoids storing a
/// subscription that would never fire.
fn subscribe_resource(req: Request, ctx: &ServeCtx, writer: &SharedWriter, conn: u64) -> Response {
    let uri = req
        .params
        .as_ref()
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match crate::agentd_uri::AgentdResource::parse(uri) {
        Some(crate::agentd_uri::AgentdResource::Subagent(handle)) => {
            let reg = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match reg.get(&handle) {
                None => {
                    return Response::err(
                        req.id,
                        json::RESOURCE_NOT_FOUND,
                        format!("no such run: {uri}"),
                    );
                }
                Some(s) if s.status.is_terminal() => {
                    return Response::err(
                        req.id,
                        json::RESOURCE_NOT_FOUND,
                        format!("run already finished; resources/read {uri}"),
                    );
                }
                Some(_) => {} // running → subscribable
            }
        }
        // The run aggregate is subscribable only for *this* daemon's run id — a
        // peer asking for some other run's URI is rejected (it never fires here).
        Some(crate::agentd_uri::AgentdResource::Run(id)) if id == ctx.run_id => {}
        // A warm session is subscribable while it's LIVE (not yet done); an unknown
        // or finished handle is rejected so the peer reads it instead.
        Some(crate::agentd_uri::AgentdResource::Session(handle)) => {
            let mut warm = ctx.warm.lock().unwrap_or_else(|e| e.into_inner());
            match warm.get_mut(&handle) {
                Some(w) => {
                    drain_warm(w); // refresh `done` before deciding subscribability
                    if w.done {
                        return Response::err(
                            req.id,
                            json::RESOURCE_NOT_FOUND,
                            format!("warm session already ended; resources/read {uri}"),
                        );
                    }
                }
                None => {
                    return Response::err(
                        req.id,
                        json::RESOURCE_NOT_FOUND,
                        format!("no such warm session: {uri}"),
                    );
                }
            }
        }
        _ => {
            return Response::err(
                req.id,
                json::RESOURCE_NOT_FOUND,
                format!("not a subscribable resource: {uri}"),
            );
        }
    } // release the sessions / warm lock before taking the subscriptions lock
    let mut subs = ctx.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
    let list = subs.entry(uri.to_string()).or_default();
    if !list.iter().any(|s| s.conn == conn) {
        list.push(Subscriber {
            conn,
            writer: Arc::clone(writer),
        });
    }
    Response::ok(req.id, json!({}))
}

/// `resources/unsubscribe`: drop this connection's subscription to `uri`.
fn unsubscribe_resource(req: Request, ctx: &ServeCtx, conn: u64) -> Response {
    let uri = req
        .params
        .as_ref()
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut subs = ctx.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(list) = subs.get_mut(uri) {
        list.retain(|s| s.conn != conn);
        if list.is_empty() {
            subs.remove(uri);
        }
    }
    Response::ok(req.id, json!({}))
}

/// Drop every subscription held by a (now-closed) connection.
fn remove_conn_subscriptions(ctx: &ServeCtx, conn: u64) {
    let mut subs = ctx.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
    subs.retain(|_uri, list| {
        list.retain(|s| s.conn != conn);
        !list.is_empty()
    });
}

/// Push `notifications/resources/updated{uri}` to every current subscriber of
/// `uri`. Best-effort: a write to a dead peer fails and is cleaned up when that
/// connection's reader loop ends. The subscriptions lock is released before
/// writing, so a slow/blocked peer can't stall other notifications.
fn notify_resource_updated(subs: &SubRegistry, uri: &str) {
    let writers: Vec<SharedWriter> = {
        let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
        // A subagent resource changes exactly once (terminal), so CONSUME the
        // subscriptions as we fire them — no entry lingers after its one event.
        match g.remove(uri) {
            Some(list) => list.into_iter().map(|s| s.writer).collect(),
            None => return,
        }
    };
    let note = Notification::new(method::NOTIFY_RESOURCES_UPDATED, Some(json!({"uri": uri})));
    for w in writers {
        if let Ok(mut wl) = w.lock() {
            let _ = frame::write_line(&mut *wl, &note);
        }
    }
}

/// Like [`notify_resource_updated`] but **keeps** the subscriber list — the
/// `agentd://run/<run_id>` and `agentd://session/<handle>` resources change
/// REPEATEDLY (each spawn / each warm-turn boundary), so a single subscribe must
/// keep receiving updates rather than fire once and be consumed. Cloning the
/// writers under the lock (then releasing it before writing) keeps the entry
/// intact for the next emission. Dead peers are pruned when their reader loop ends
/// (`remove_conn_subscriptions`).
fn notify_resource_updated_keep(subs: &SubRegistry, uri: &str) {
    let writers: Vec<SharedWriter> = {
        let g = subs.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(uri) {
            Some(list) => list.iter().map(|s| Arc::clone(&s.writer)).collect(),
            None => return,
        }
    };
    let note = Notification::new(method::NOTIFY_RESOURCES_UPDATED, Some(json!({"uri": uri})));
    for w in writers {
        if let Ok(mut wl) = w.lock() {
            let _ = frame::write_line(&mut *wl, &note);
        }
    }
}

/// The agentd:// resources this server *lists*: the stable `agentd://status` and
/// `agentd://run/<run_id>` resources. The run resource has a daemon-lifetime URI
/// (the run id is fixed at startup), so it's safe to list — unlike the per-handle
/// `agentd://subagent/<handle>` and `agentd://session/<handle>` resources, which
/// are **read**able / **subscribe**able (the peer learns a handle from its
/// `subagent.spawn` reply) but deliberately NOT listed: they appear and vanish
/// (eviction / session end) and this reply-only transport has no
/// `resources/list_changed` to announce that, so a listed handle could 404 on
/// read. Listing only the stable resources avoids advertising vanishing ones.
fn resource_list(ctx: &ServeCtx) -> Value {
    json!([
        {
            "uri": "agentd://status",
            "name": "status",
            "description": "This agentd's run id, mode, version, pid, uptime, and spawn counts.",
            "mimeType": "application/json"
        },
        {
            // Stable + listable (RFC 0015 §3.4): a self-description manifest, readable
            // on every origin. The run id is fixed at startup, so the uri never 404s.
            "uri": "agentd://capabilities",
            "name": "capabilities",
            "description": "This agentd's self-description: identity, the declared capability surface (intelligence transport, MCP servers, exec, limits, isolation), and live daemon counters.",
            "mimeType": "application/json"
        },
        {
            "uri": crate::agentd_uri::run_uri(&ctx.run_id),
            "name": "run",
            "description": "This served run's aggregate: mode, root handle, status, spawn counts, and uptime. Subscribable — pushed on each spawn / terminal-run change.",
            "mimeType": "application/json"
        }
    ])
}

/// `resources/read` over the agentd:// scheme. A known URI returns a `contents`
/// body; an unknown/missing URI is a JSON-RPC INVALID_PARAMS error.
fn resources_read(req: Request, ctx: &ServeCtx) -> Response {
    let uri = req
        .params
        .as_ref()
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match crate::agentd_uri::AgentdResource::parse(uri) {
        Some(crate::agentd_uri::AgentdResource::Status) => Response::ok(
            req.id,
            json!({
                "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.status_body().to_string()}]
            }),
        ),
        // agentd://capabilities — the live self-description manifest (RFC 0015
        // §3.4). Readable on every origin (it discloses no secret, confers no
        // authority).
        Some(crate::agentd_uri::AgentdResource::Capabilities) => Response::ok(
            req.id,
            json!({
                "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.capabilities_body().to_string()}]
            }),
        ),
        // agentd://subagent/<handle> — a served async run's status / result.
        Some(crate::agentd_uri::AgentdResource::Subagent(handle)) => {
            let reg = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match reg.get(&handle) {
                Some(s) => Response::ok(
                    req.id,
                    json!({
                        "contents": [{"uri": uri, "mimeType": "application/json", "text": session_body(&handle, s).to_string()}]
                    }),
                ),
                None => Response::err(
                    req.id,
                    json::RESOURCE_NOT_FOUND,
                    format!("resource not found: {uri}"),
                ),
            }
        }
        // agentd://run/<run_id> — the served run aggregate (RFC 0005 §3.3).
        Some(crate::agentd_uri::AgentdResource::Run(_)) => Response::ok(
            req.id,
            json!({
                "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.run_body().to_string()}]
            }),
        ),
        // agentd://session/<handle> — a served warm session's turn state. Drain
        // first so the body reflects any turns that completed since the last poll;
        // an unknown handle is RESOURCE_NOT_FOUND (mirrors the subagent arm).
        Some(crate::agentd_uri::AgentdResource::Session(handle)) => {
            let mut warm = ctx.warm.lock().unwrap_or_else(|e| e.into_inner());
            match warm.get_mut(&handle) {
                Some(w) => {
                    drain_warm_notify(&handle, w, &ctx.subscriptions);
                    Response::ok(
                        req.id,
                        json!({
                            "contents": [{"uri": uri, "mimeType": "application/json", "text": warm_body(&handle, w).to_string()}]
                        }),
                    )
                }
                None => Response::err(
                    req.id,
                    json::RESOURCE_NOT_FOUND,
                    format!("resource not found: {uri}"),
                ),
            }
        }
        None => Response::err(
            req.id,
            json::RESOURCE_NOT_FOUND,
            format!("resource not found: {uri}"),
        ),
    }
}

fn tools_call(req: Request, ctx: &ServeCtx, log: &Logger) -> Response {
    let name = req
        .params
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match name {
        "status" => {
            let body = ctx.status_body();
            // CallToolResult: human text + a structured payload (MCP 2025-11-25).
            Response::ok(
                req.id,
                json!({
                    "content": [{"type": "text", "text": body.to_string()}],
                    "structuredContent": body,
                    "isError": false
                }),
            )
        }
        "subagent.spawn" => handle_spawn(req, ctx, log),
        "subagent.send" => {
            let args = req
                .params
                .as_ref()
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(json!({}));
            handle_send(req.id, ctx, &args)
        }
        "subagent.status" | "subagent.cancel" => {
            let args = req
                .params
                .as_ref()
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(json!({}));
            handle_session_tool(req.id, ctx, name, &args)
        }
        other => Response::err(
            req.id,
            json::INVALID_PARAMS,
            format!("unknown tool: {other}"),
        ),
    }
}

/// A peer delegates a task to agentd (RFC 0005 §3.2). Build a fresh root run from
/// the daemon's payload template + the request and supervise it. **Sync** (default)
/// blocks and returns the distilled outcome; **`async: true`** returns a handle
/// immediately and runs in the background — the peer then polls `subagent.status`,
/// reads `agentd://subagent/<handle>`, or `subagent.cancel`s it. Bad params → a
/// JSON-RPC error; a cap/scope refusal or a run failure → `isError:true` inside a
/// successful result (so the caller's model adapts), never a crash.
///
/// **Trust boundary:** anyone able to connect to the `--serve-mcp` socket can run
/// instructions with this agentd's intelligence + tool scope. The operator gates
/// access via the socket's filesystem permissions. **`warm: true`** keeps the run
/// alive as a session the peer drives with `subagent.send`.
fn handle_spawn(req: Request, ctx: &ServeCtx, log: &Logger) -> Response {
    let id = req.id.clone();
    let args = req
        .params
        .as_ref()
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or(json!({}));
    let instruction = args
        .get("instruction")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if instruction.is_empty() {
        // Malformed call (missing required param) → JSON-RPC error (RFC §3.2).
        return Response::err(
            id,
            json::INVALID_PARAMS,
            "subagent.spawn requires a non-empty 'instruction'",
        );
    }
    // Memory backpressure: when the unit sits at its `memory.high` soft limit,
    // refuse new subagents (warm/async/sync alike) rather than push the cgroup
    // into reclaim/OOM — the peer retries once pressure clears. Best-effort:
    // never fires off-cgroup or when `memory.high` is unset.
    if cgroup::under_memory_pressure() {
        log.warn(
            "cgroup.backpressure",
            json!({"reason": "memory.high", "tool": "subagent.spawn"}),
        );
        return tool_error(
            id,
            "spawn refused: memory pressure (cgroup at memory.high); retry shortly".to_string(),
        );
    }
    let n = ctx.counter.fetch_add(1, Ordering::Relaxed);
    // A new spawn changed the run aggregate (total_spawns / inflight) → push to any
    // `agentd://run/<run_id>` subscribers (the keep-variant: the run resource fires
    // repeatedly over the daemon's life).
    notify_resource_updated_keep(&ctx.subscriptions, &crate::agentd_uri::run_uri(&ctx.run_id));
    let handle = format!("served.{n}");
    let payload = build_served_payload(&ctx.base, &args, &handle);
    let is_warm = args.get("warm").and_then(Value::as_bool).unwrap_or(false);
    let is_async = args.get("async").and_then(Value::as_bool).unwrap_or(false);
    log.info("mcp.spawn", json!({"handle": handle, "servers": payload.mcp_servers.len(), "warm": is_warm, "async": is_async}));

    // Warm: a live session driven by subagent.send — bounded by its own cap, not
    // the async permit (so long-lived warm sessions can't starve one-shot runs).
    if is_warm {
        return spawn_warm(id, ctx, log, handle, payload);
    }

    // Concurrency cap → refused as a tool result, never a crash. For async the
    // permit is held by the background run thread (so it bounds live runs).
    let permit = match SpawnGuard::acquire(&ctx.inflight, MAX_INFLIGHT_SPAWNS) {
        Some(g) => g,
        None => {
            return tool_error(
                id,
                format!("spawn refused: {MAX_INFLIGHT_SPAWNS} concurrent served spawns in flight"),
            );
        }
    };
    if is_async {
        return spawn_async(id, ctx, log, handle, payload, permit);
    }

    // Sync: block this connection thread on the run.
    let result = supervise_once(ctx.exe.clone(), &payload, ctx.drain_timeout, log.clone());
    spawn_result_response(id, &handle, result)
}

/// Spawn a **warm** session: a live subagent (warm mode) the peer drives with
/// `subagent.send`. Held by the server and drained lazily (no supervision thread);
/// `subagent.cancel` ends it. Bounded by `MAX_WARM_SESSIONS`.
fn spawn_warm(
    id: Id,
    ctx: &ServeCtx,
    log: &Logger,
    handle: String,
    mut payload: SpawnPayload,
) -> Response {
    payload.warm = true;
    let (tx, rx) = std::sync::mpsc::channel();
    // Hold the registry lock across sweep + cap-check + spawn + insert so the cap is
    // enforced atomically (no check-then-insert TOCTOU that could overshoot it). The
    // hold is bounded by child startup (fork + a small framed payload write + the
    // reader-thread spawn); warm spawns are rare, so briefly blocking concurrent
    // status/send is acceptable.
    let mut warm = ctx.warm.lock().unwrap_or_else(|e| e.into_inner());
    // Reclaim finished-but-unpolled sessions first: drain_warm marks done ones, and a
    // peer that spawns-and-forgets would otherwise pin their slots forever (the only
    // other removal is on a per-handle send/status). Their children are already dead,
    // so the retain's Drops reap instantly (ECHILD).
    for w in warm.values_mut() {
        drain_warm(w);
    }
    warm.retain(|_, w| !w.done);
    if warm.len() >= MAX_WARM_SESSIONS {
        return tool_error(
            id,
            format!("warm refused: {MAX_WARM_SESSIONS} warm sessions live; cancel one"),
        );
    }
    match spawn(&ctx.exe, &payload, NodeId(0), tx) {
        Ok(sub) => {
            warm.insert(
                handle.clone(),
                // pending: 1 — the instruction's first turn is already in flight.
                WarmServed {
                    sub,
                    rx,
                    last: None,
                    turns: 0,
                    pending: 1,
                    done: false,
                    started: Instant::now(),
                },
            );
            drop(warm); // release before logging + building the reply
            log.info("mcp.spawn_warm", json!({"handle": handle}));
            let body = json!({"handle": handle, "warm": true, "alive": true});
            Response::ok(
                id,
                json!({
                    "content": [{"type": "text", "text": format!("started warm session (handle={handle}); drive it with subagent.send, read it with subagent.status, end it with subagent.cancel")}],
                    "structuredContent": body,
                    "isError": false
                }),
            )
        }
        Err(e) => tool_error(id, format!("subagent could not start: {e}")),
    }
}

/// `subagent.send{handle, message}`: inject the next user message into a live warm
/// session (it runs another turn over the same conversation). Drains any completed
/// turns first; reports its current state.
fn handle_send(id: Id, ctx: &ServeCtx, args: &Value) -> Response {
    let handle = args
        .get("handle")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        return Response::err(
            id,
            json::INVALID_PARAMS,
            "subagent.send requires a non-empty 'message'",
        );
    }
    let mut warm = ctx.warm.lock().unwrap_or_else(|e| e.into_inner());
    let Some(w) = warm.get_mut(&handle) else {
        return tool_error(id, format!("no such warm session '{handle}'"));
    };
    drain_warm_notify(&handle, w, &ctx.subscriptions);
    if w.done {
        warm.remove(&handle); // ended — reap it
        return tool_error(id, format!("warm session '{handle}' has ended"));
    }
    match w.sub.send(&ControlMsg::Inject {
        message: message.to_string(),
    }) {
        Ok(()) => {
            w.pending += 1; // a new turn is now queued
            // The turn index this send (or the latest still-queued send) will produce.
            // `delivered` means "queued to the child", not "ran" — the peer confirms by
            // polling subagent.status until `turns` reaches `awaiting_turn`.
            let awaiting_turn = w.turns + w.pending;
            let body = json!({"handle": handle, "delivered": true, "turns": w.turns, "awaiting_turn": awaiting_turn});
            Response::ok(
                id,
                json!({"content": [{"type": "text", "text": format!("delivered to {handle}; poll subagent.status until turns reaches {awaiting_turn}")}], "structuredContent": body, "isError": false}),
            )
        }
        Err(e) => {
            warm.remove(&handle);
            tool_error(id, format!("warm session '{handle}' send failed: {e}"))
        }
    }
}

/// Map a finished sync run to the tool result the peer gets inline.
fn spawn_result_response(
    id: Id,
    handle: &str,
    result: std::io::Result<SuperviseResult>,
) -> Response {
    match result {
        Ok(SuperviseResult::Completed(o)) => Response::ok(
            id,
            json!({
                "content": [{"type": "text", "text": distill(&o.result)}],
                // `done:true` keeps the structuredContent shape unified with the
                // async ack ({handle,status,done,…}) so a peer parses one schema.
                "structuredContent": {
                    "handle": handle, "status": o.status.as_str(), "done": true, "partial": o.partial, "result": o.result
                },
                "isError": false
            }),
        ),
        Ok(SuperviseResult::Failed(e)) => tool_error(id, format!("subagent failed: {e}")),
        Ok(SuperviseResult::Killed(r)) => tool_error(id, format!("subagent terminated: {r:?}")),
        Err(e) => tool_error(id, format!("subagent could not start: {e}")),
    }
}

/// Register an async run, launch it on a background thread (holding the permit +
/// a per-run cancel flag), and return the handle to the peer immediately.
///
/// **Concurrency:** "async" is non-blocking to the calling peer, and the run now
/// executes **truly concurrently** with the daemon's own reactions and other
/// served runs — the process-global [`reaper`](crate::supervisor::reaper)
/// dispatches each child's exit by pid, so supervisors no longer serialize on a
/// lock (bounded only by `MAX_INFLIGHT_SPAWNS`). A run is bounded by its payload
/// deadline; `subagent.cancel` drains it early. On daemon shutdown
/// `ServeHandle::drain` (wired in `main.rs`) asks in-flight served runs to cancel
/// and waits, bounded by the drain timeout, for their subtrees to drain
/// gracefully; `PR_SET_PDEATHSIG` is the backstop against orphan leak if the
/// drain window elapses. Handles are shared
/// across all peers on the socket (one trust domain — socket perms gate access)
/// and confer no ownership.
fn spawn_async(
    id: Id,
    ctx: &ServeCtx,
    log: &Logger,
    handle: String,
    payload: SpawnPayload,
    permit: SpawnGuard,
) -> Response {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut reg = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
        evict_if_full(&mut reg);
        reg.insert(
            handle.clone(),
            ServedSession {
                status: ServedStatus::Running,
                cancel: Arc::clone(&cancel),
                started: Instant::now(),
            },
        );
    }
    let (exe, drain, sessions, subs, log2, h, run_uri) = (
        ctx.exe.clone(),
        ctx.drain_timeout,
        Arc::clone(&ctx.sessions),
        Arc::clone(&ctx.subscriptions),
        log.clone(),
        handle.clone(),
        crate::agentd_uri::run_uri(&ctx.run_id),
    );
    let spawned = thread::Builder::new()
        .name(format!("served-run:{handle}"))
        .spawn(move || {
            let _permit = permit; // held for the run's lifetime → bounds live runs
            let result =
                supervise_cancellable(exe, &payload, drain, log2, Some(Arc::clone(&cancel)));
            // Write the terminal status and re-read the cancel flag UNDER the same
            // lock the cancel tool uses, so a cancel that lands while we finish is
            // never lost: either its store happens-before this load (→ a killed run
            // reads Cancelled) or after this write (→ the tool sees terminal + no-ops).
            {
                let mut reg = sessions.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(s) = reg.get_mut(&h) {
                    s.status = run_to_status(result, cancel.load(Ordering::Relaxed));
                }
            }
            // The run finished → its `agentd://subagent/<handle>` resource changed
            // (consume — it fires exactly once), and the run aggregate's inflight
            // count dropped → push the run resource too (keep — it fires repeatedly).
            notify_resource_updated(&subs, &crate::agentd_uri::subagent_uri(&h));
            notify_resource_updated_keep(&subs, &run_uri);
        });
    if spawned.is_err() {
        ctx.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&handle);
        return tool_error(
            id,
            "subagent could not start: thread spawn failed".to_string(),
        );
    }
    let body = json!({"handle": handle, "status": "running", "done": false});
    Response::ok(
        id,
        json!({
            "content": [{"type": "text", "text": format!("spawned async (handle={handle}); poll subagent.status or read agentd://subagent/{handle}")}],
            "structuredContent": body,
            "isError": false
        }),
    )
}

/// Map a finished async run + whether its cancel was requested to a
/// [`ServedStatus`]. A run that produced a result **finished before any cancel
/// took effect**, so its real outcome is surfaced (not discarded as "cancelled");
/// `Cancelled` is reported only when the run was actually torn down.
fn run_to_status(result: std::io::Result<SuperviseResult>, cancel_requested: bool) -> ServedStatus {
    match result {
        Ok(SuperviseResult::Completed(o)) => ServedStatus::Done {
            status: o.status.as_str().to_string(),
            partial: o.partial,
            result: o.result,
        },
        Ok(SuperviseResult::Killed(_)) if cancel_requested => ServedStatus::Cancelled,
        Ok(SuperviseResult::Killed(r)) => ServedStatus::Failed(format!("terminated: {r:?}")),
        Ok(SuperviseResult::Failed(e)) => ServedStatus::Failed(e),
        Err(e) => ServedStatus::Failed(format!("could not start: {e}")),
    }
}

/// Evict the oldest *finished* session when the registry is at capacity. Running
/// sessions are never evicted (bounded by the permit cap).
fn evict_if_full(reg: &mut HashMap<String, ServedSession>) {
    if reg.len() < MAX_SESSIONS {
        return;
    }
    if let Some(oldest) = reg
        .iter()
        .filter(|(_, s)| s.status.is_terminal())
        .min_by_key(|(_, s)| s.started)
        .map(|(k, _)| k.clone())
    {
        reg.remove(&oldest);
    }
}

/// `subagent.status{handle}` / `subagent.cancel{handle}` — works on a **warm**
/// session (checked first) or an **async** run.
fn handle_session_tool(id: Id, ctx: &ServeCtx, name: &str, args: &Value) -> Response {
    let handle = args
        .get("handle")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // Warm session?
    {
        let mut warm = ctx.warm.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = warm.get_mut(&handle) {
            drain_warm_notify(&handle, w, &ctx.subscriptions);
            return match name {
                "subagent.cancel" => {
                    let _ = w.sub.send(&ControlMsg::Cancel {
                        reason: "cancel".into(),
                    });
                    // Take it out, then release the registry lock *before* dropping it:
                    // Subagent::Drop does SIGKILL + waitpid on a still-live child (a brief
                    // stall), and no other warm op should block on that.
                    let removed = warm.remove(&handle);
                    drop(warm);
                    drop(removed); // kill + reap, now unlocked
                    let body = json!({"handle": handle, "cancelled": true, "warm": true});
                    Response::ok(
                        id,
                        json!({"content": [{"type": "text", "text": format!("ended warm session {handle}")}], "structuredContent": body, "isError": false}),
                    )
                }
                _ => {
                    let body = warm_body(&handle, w);
                    let done = w.done;
                    if done {
                        warm.remove(&handle);
                    }
                    Response::ok(
                        id,
                        json!({"content": [{"type": "text", "text": body.to_string()}], "structuredContent": body, "isError": false}),
                    )
                }
            };
        }
    }
    let mut reg = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let Some(session) = reg.get_mut(&handle) else {
        return tool_error(id, format!("no async subagent with handle '{handle}'"));
    };
    match name {
        "subagent.cancel" => {
            let (text, body) = if session.status.is_terminal() {
                (
                    format!("subagent {handle} already finished; nothing to cancel"),
                    json!({"handle": handle, "cancelled": false, "reason": "already finished"}),
                )
            } else {
                session.cancel.store(true, Ordering::Relaxed);
                (
                    format!(
                        "cancel requested for {handle}; it is draining — poll subagent.status for the outcome"
                    ),
                    json!({"handle": handle, "cancelled": true}),
                )
            };
            Response::ok(
                id,
                json!({"content": [{"type": "text", "text": text}], "structuredContent": body, "isError": false}),
            )
        }
        // "subagent.status"
        _ => {
            let body = session_body(&handle, session);
            Response::ok(
                id,
                json!({"content": [{"type": "text", "text": body.to_string()}], "structuredContent": body, "isError": false}),
            )
        }
    }
}

/// Build a served run's payload from the daemon's template + the request. The
/// child's depth is minted here (a fresh root, not read from the request); the
/// `tool_scope` only ever narrows the daemon's server set (RFC 0005 §3.2). Pure.
fn build_served_payload(base: &SpawnPayload, args: &Value, handle: &str) -> SpawnPayload {
    let mut p = base.clone();
    p.instruction = args
        .get("instruction")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    p.output_contract = args.get("output_contract").map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    if let Some(names) = args.get("tool_scope").and_then(Value::as_array) {
        let wanted: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
        p.mcp_servers.retain(|s| wanted.contains(&s.name.as_str()));
    }
    p.telemetry.agent_id = handle.to_string();
    p.telemetry.agent_path = handle.to_string();
    p.depth = 0; // a fresh root run triggered by the peer
    p
}

/// A tool-domain failure: `isError:true` inside a *successful* JSON-RPC result.
fn tool_error(id: Id, msg: String) -> Response {
    Response::ok(
        id,
        json!({"content": [{"type": "text", "text": msg}], "isError": true}),
    )
}

/// Cap a result value to text for the `content` block (the full value is also in
/// `structuredContent`).
fn distill(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() <= RESULT_CAP {
        s
    } else {
        s.chars().take(RESULT_CAP).collect::<String>() + "…[truncated]"
    }
}

fn status_tool_def() -> Value {
    json!({
        "name": "status",
        "description": "Report this agentd's run id, mode, version, pid and uptime (ms).",
        "inputSchema": {"type": "object", "properties": {}}
    })
}

fn spawn_tool_def() -> Value {
    json!({
        "name": "subagent.spawn",
        "description": "Delegate a task to this agentd: it runs a fresh agent (its own intelligence \
            + tool scope) and returns the distilled result. Give an 'instruction' and optionally an \
            'output_contract' and a 'tool_scope' (a subset of this agentd's MCP server names). By \
            default the call blocks until the run finishes; pass async=true to get a 'handle' back \
            immediately and then poll subagent.status / read agentd://subagent/<handle> / \
            subagent.cancel. Pass warm=true to keep the agent ALIVE as a session you drive with \
            subagent.send (a multi-turn conversation); end it with subagent.cancel.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "instruction": {"type": "string", "description": "the task for the spawned agent"},
                "output_contract": {"type": "string", "description": "exactly what the agent should return"},
                "tool_scope": {"type": "array", "items": {"type": "string"}, "description": "subset of MCP server names to grant"},
                "async": {"type": "boolean", "description": "return a handle immediately and run in the background"},
                "warm": {"type": "boolean", "description": "keep the agent alive as a session you drive with subagent.send"}
            },
            "required": ["instruction"]
        }
    })
}

fn send_tool_def() -> Value {
    json!({
        "name": "subagent.send",
        "description": "Send another message into a warm session you started with subagent.spawn \
            warm=true (by 'handle'). The agent runs another turn over the SAME conversation. Returns \
            'awaiting_turn' (the turn index this message will produce); poll subagent.status until its \
            'turns' reaches that value (and 'busy' is false) to read the result. 'delivered' means \
            queued to the agent, not that the turn ran — if a later status shows done=true before \
            'turns' advances, the message was lost (the session ended).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "handle": {"type": "string", "description": "the handle from subagent.spawn warm=true"},
                "message": {"type": "string", "description": "the next message for the warm agent"}
            },
            "required": ["handle", "message"]
        }
    })
}

fn session_status_tool_def() -> Value {
    json!({
        "name": "subagent.status",
        "description": "Check on a run you started with subagent.spawn, by 'handle'. For an async run \
            (async=true): returns 'done' (bool) and a 'status' string — 'running' while in flight, else \
            the terminal status (completed / failed / cancelled) plus its result. For a warm session \
            (warm=true): returns 'turns' (completed turn count), 'busy' (a turn is in flight), \
            'last_result' / 'last_is_error', and 'done'/'alive'. Non-blocking.",
        "inputSchema": {
            "type": "object",
            "properties": {"handle": {"type": "string", "description": "the handle from subagent.spawn async=true"}},
            "required": ["handle"]
        }
    })
}

fn session_cancel_tool_def() -> Value {
    json!({
        "name": "subagent.cancel",
        "description": "Cancel a still-running async run (by 'handle'): agentd drains its subtree \
            gracefully. A run that already finished is left as-is.",
        "inputSchema": {
            "type": "object",
            "properties": {"handle": {"type": "string", "description": "the handle from subagent.spawn async=true"}},
            "required": ["handle"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerSpec;
    use crate::obs::log::{Comp, Level, LogCtx};
    use crate::subagent::protocol::{IntelConfig, Limits, Telemetry};

    fn base() -> SpawnPayload {
        SpawnPayload {
            instruction: "standing".into(),
            output_contract: None,
            context_seed: Vec::new(),
            intelligence: IntelConfig {
                uri: "unix:/x".into(),
                token: None,
                model: None,
            },
            mcp_servers: vec![
                McpServerSpec {
                    name: "fs".into(),
                    command: vec!["a".into()],
                    tags: Vec::new(),
                },
                McpServerSpec {
                    name: "db".into(),
                    command: vec!["b".into()],
                    tags: Vec::new(),
                },
            ],
            limits: Limits {
                max_steps: 10,
                max_tokens: 1000,
                deadline_ms: 1000,
                max_depth: 4,
            },
            telemetry: Telemetry {
                run_id: "r1".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                trace_id: None,
                log_level: "error".into(),
                log_content: false,
            },
            depth: 0,
            enable_exec: false,
            warm: false,
        }
    }

    fn ctx() -> ServeCtx {
        let cfg = crate::config::Config {
            run_id: "r1".into(),
            mode: crate::config::Mode::Reactive,
            intelligence: Some("unix:/x".into()),
            ..crate::config::Config::default()
        };
        ServeCtx::new(
            "r1".into(),
            "reactive".into(),
            "agentd".into(),
            base(),
            Duration::from_secs(5),
            Arc::new(cfg),
        )
    }

    fn log() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "r1".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }

    fn req(method: &str, params: Option<Value>) -> Request {
        Request::new(Id::Num(1), method, params)
    }

    /// A throwaway connection writer for unit-dispatching (its peer end is
    /// dropped; the unit tests never push to it, so no write occurs).
    fn writer() -> SharedWriter {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        Arc::new(Mutex::new(ServeStream::Unix(a)))
    }

    /// Insert a Running session so `subscribe_resource` accepts its handle.
    fn insert_running(ctx: &ServeCtx, handle: &str) {
        ctx.sessions.lock().unwrap().insert(
            handle.to_string(),
            ServedSession {
                status: ServedStatus::Running,
                cancel: Arc::new(AtomicBool::new(false)),
                started: Instant::now(),
            },
        );
    }

    #[test]
    fn initialize_declares_tools_capability() {
        let r = dispatch(
            req("initialize", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSION);
        assert!(v["capabilities"]["tools"].is_object());
        assert_eq!(v["serverInfo"]["name"], "agentd");
    }

    #[test]
    fn tools_list_advertises_status_and_spawn() {
        let r = dispatch(
            req("tools/list", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let tools = r.result.expect("ok")["tools"].clone();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"status"));
        assert!(names.contains(&"subagent.spawn"));
    }

    #[test]
    fn status_call_returns_structured_state() {
        let r = dispatch(
            req("tools/call", Some(json!({"name": "status"}))),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["run_id"], "r1");
        assert_eq!(v["structuredContent"]["mode"], "reactive");
    }

    #[test]
    fn initialize_declares_resources_capability() {
        let r = dispatch(
            req("initialize", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        assert!(
            v["capabilities"]["resources"].is_object(),
            "resources capability advertised"
        );
    }

    #[test]
    fn resources_list_advertises_status_and_run() {
        let r = dispatch(
            req("resources/list", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let resources = r.result.expect("ok")["resources"].clone();
        let uris: Vec<&str> = resources
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["uri"].as_str())
            .collect();
        assert!(
            uris.contains(&"agentd://status"),
            "agentd://status listed: {uris:?}"
        );
        // the run aggregate is stable (daemon-lifetime uri) → safe to list.
        assert!(
            uris.contains(&"agentd://run/r1"),
            "agentd://run/r1 listed: {uris:?}"
        );
        // capabilities is a stable, listable self-description (RFC 0015 §3.4).
        assert!(
            uris.contains(&"agentd://capabilities"),
            "agentd://capabilities listed: {uris:?}"
        );
    }

    #[test]
    fn resources_read_capabilities_returns_the_manifest() {
        let r = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://capabilities"})),
            ),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        let entry = &v["contents"][0];
        assert_eq!(entry["uri"], "agentd://capabilities");
        assert_eq!(entry["mimeType"], "application/json");
        let body: Value = serde_json::from_str(entry["text"].as_str().unwrap()).unwrap();
        // The served manifest is the canonical RFC 0015 §5.2 schema (the same
        // builder as the `--capabilities` one-shot): contract_version + top-level
        // mode + identity.run_id from the downward-API/run id.
        assert_eq!(body["contract_version"], "1.0");
        assert_eq!(body["identity"]["run_id"], "r1");
        assert_eq!(body["mode"], "reactive");
    }

    #[test]
    fn capabilities_is_readable_on_stdio_origin_too() {
        // §3.4: the manifest is harmless self-description — readable on every origin.
        let r = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://capabilities"})),
            ),
            &ctx(),
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        assert!(r.result.is_some(), "capabilities readable on stdio origin");
    }

    #[test]
    fn resources_read_status_returns_a_contents_body() {
        let r = dispatch(
            req("resources/read", Some(json!({"uri": "agentd://status"}))),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        let entry = &v["contents"][0];
        assert_eq!(entry["uri"], "agentd://status");
        assert_eq!(entry["mimeType"], "application/json");
        // the text is the JSON status body
        let body: Value = serde_json::from_str(entry["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["run_id"], "r1");
        assert_eq!(body["mode"], "reactive");
    }

    #[test]
    fn resources_read_unknown_uri_is_an_error() {
        let r = dispatch(
            req("resources/read", Some(json!({"uri": "agentd://ghost"}))),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        assert!(r.error.is_some(), "unknown agentd:// uri → JSON-RPC error");
        let bad = dispatch(
            req("resources/read", Some(json!({"uri": "file:///x"}))),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        assert!(bad.error.is_some(), "non-agentd uri → JSON-RPC error");
    }

    #[test]
    fn tools_list_advertises_send_and_spawn_schema_has_warm() {
        let r = dispatch(
            req("tools/list", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let tools = r.result.expect("ok")["tools"].clone();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"subagent.send"), "{names:?}");
        assert!(
            spawn_tool_def()["inputSchema"]["properties"]
                .get("warm")
                .is_some(),
            "spawn offers warm"
        );
    }

    #[test]
    fn send_validates_message_and_handle() {
        let ctx = ctx();
        // missing/empty message → JSON-RPC error
        assert!(
            handle_send(Id::Num(1), &ctx, &json!({"handle": "served.0"}))
                .error
                .is_some()
        );
        assert!(
            handle_send(
                Id::Num(1),
                &ctx,
                &json!({"handle": "served.0", "message": "  "})
            )
            .error
            .is_some()
        );
        // unknown warm handle → tool error (isError result)
        let r = handle_send(
            Id::Num(1),
            &ctx,
            &json!({"handle": "served.9", "message": "hi"}),
        );
        assert_eq!(r.result.unwrap()["isError"], true);
    }

    #[test]
    fn tools_list_advertises_async_session_tools() {
        let r = dispatch(
            req("tools/list", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let tools = r.result.expect("ok")["tools"].clone();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"subagent.status"), "{names:?}");
        assert!(names.contains(&"subagent.cancel"), "{names:?}");
    }

    #[test]
    fn spawn_schema_advertises_async() {
        let v = spawn_tool_def();
        assert!(
            v["inputSchema"]["properties"].get("async").is_some(),
            "spawn schema offers async"
        );
    }

    #[test]
    fn status_and_cancel_of_unknown_handle_are_tool_errors() {
        for tool in ["subagent.status", "subagent.cancel"] {
            let r = dispatch(
                req(
                    "tools/call",
                    Some(json!({"name": tool, "arguments": {"handle": "served.9"}})),
                ),
                &ctx(),
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            let v = r.result.expect("ok");
            assert_eq!(v["isError"], true, "{tool} unknown handle → isError");
            assert!(
                v["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("no async subagent")
            );
        }
    }

    #[test]
    fn resources_read_unknown_session_is_an_error() {
        let r = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://subagent/served.404"})),
            ),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        assert!(r.error.is_some(), "unknown session uri → JSON-RPC error");
    }

    #[test]
    fn run_body_reports_run_id_mode_and_counts() {
        let ctx = ctx();
        let b = ctx.run_body();
        assert_eq!(b["run_id"], "r1");
        assert_eq!(b["mode"], "reactive");
        assert_eq!(b["root"], "0");
        assert_eq!(b["status"], "running");
        // counts come straight from the atomics; fresh ctx → zero.
        assert_eq!(b["inflight_spawns"], 0);
        assert_eq!(b["total_spawns"], 0);
        assert!(b["uptime_ms"].is_u64());
    }

    #[test]
    fn resources_read_run_returns_a_contents_body() {
        let r = dispatch(
            req("resources/read", Some(json!({"uri": "agentd://run/r1"}))),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        let entry = &v["contents"][0];
        assert_eq!(entry["uri"], "agentd://run/r1");
        assert_eq!(entry["mimeType"], "application/json");
        let body: Value = serde_json::from_str(entry["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["run_id"], "r1");
        assert_eq!(body["mode"], "reactive");
    }

    #[test]
    fn run_resource_is_subscribable_only_for_this_run_id() {
        let ctx = ctx();
        // the daemon's own run id is subscribable…
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://run/r1"}))),
                &ctx,
                &writer(),
                0,
            )
            .error
            .is_none()
        );
        // …but some other run's uri is rejected (it would never fire here).
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://run/other"}))),
                &ctx,
                &writer(),
                0,
            )
            .error
            .is_some()
        );
    }

    #[test]
    fn subscribe_rejects_unknown_session() {
        let ctx = ctx();
        let r = subscribe_resource(
            req("sub", Some(json!({"uri": "agentd://session/served.404"}))),
            &ctx,
            &writer(),
            0,
        );
        assert!(
            r.error.is_some(),
            "unknown warm session is not subscribable"
        );
    }

    #[test]
    fn keep_notify_fires_repeatedly_without_consuming() {
        use std::io::{BufRead, BufReader};
        let ctx = ctx();
        let uri = "agentd://run/r1";
        let (a, b) = UnixStream::pair().unwrap();
        let w: SharedWriter = Arc::new(Mutex::new(ServeStream::Unix(a)));
        assert!(
            subscribe_resource(req("sub", Some(json!({"uri": uri}))), &ctx, &w, 7)
                .error
                .is_none()
        );
        // The keep-variant must NOT consume the subscription: it fires every time.
        notify_resource_updated_keep(&ctx.subscriptions, uri);
        notify_resource_updated_keep(&ctx.subscriptions, uri);
        assert_eq!(
            ctx.subscriptions.lock().unwrap().get(uri).unwrap().len(),
            1,
            "keep-variant leaves the subscriber registered"
        );
        let mut reader = BufReader::new(b);
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["method"], "notifications/resources/updated");
            assert_eq!(v["params"]["uri"], uri);
        }
    }

    #[test]
    fn run_to_status_maps_outcome_and_cancel() {
        use crate::agentloop::stop::{Outcome, TerminalStatus};
        use crate::supervisor::reactor::KillReason;
        let outcome = || Outcome {
            status: TerminalStatus::Completed,
            partial: false,
            result: json!("r"),
            scheduled: Vec::new(),
            subscriptions: Vec::new(),
        };
        assert!(matches!(
            run_to_status(Ok(SuperviseResult::Completed(outcome())), false),
            ServedStatus::Done { .. }
        ));
        // a run that COMPLETED keeps its real result even if a cancel raced in
        // late — it finished before the cancel could tear it down.
        assert!(matches!(
            run_to_status(Ok(SuperviseResult::Completed(outcome())), true),
            ServedStatus::Done { .. }
        ));
        // a run that was actually torn down + cancel requested → Cancelled.
        assert!(matches!(
            run_to_status(Ok(SuperviseResult::Killed(KillReason::Drain)), true),
            ServedStatus::Cancelled
        ));
        // killed without a cancel request (e.g. SIGTERM drain) → failed/terminated.
        assert!(matches!(
            run_to_status(Ok(SuperviseResult::Killed(KillReason::Drain)), false),
            ServedStatus::Failed(_)
        ));
        assert!(matches!(
            run_to_status(Ok(SuperviseResult::Failed("e".into())), false),
            ServedStatus::Failed(_)
        ));
    }

    #[test]
    fn session_body_reports_running_then_done() {
        let s = ServedSession {
            status: ServedStatus::Running,
            cancel: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
        };
        let b = session_body("served.0", &s);
        assert_eq!(b["status"], "running");
        assert_eq!(b["done"], false);

        let s = ServedSession {
            status: ServedStatus::Done {
                status: "completed".into(),
                partial: false,
                result: json!("done"),
            },
            cancel: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
        };
        let b = session_body("served.0", &s);
        assert_eq!(b["status"], "completed");
        assert_eq!(b["done"], true);
        assert_eq!(b["result"], "done");
    }

    #[test]
    fn evict_drops_oldest_terminal_never_running() {
        let mut reg: HashMap<String, ServedSession> = HashMap::new();
        for i in 0..MAX_SESSIONS {
            let status = if i == 0 {
                ServedStatus::Done {
                    status: "completed".into(),
                    partial: false,
                    result: json!(i),
                }
            } else {
                ServedStatus::Running
            };
            reg.insert(
                format!("served.{i}"),
                ServedSession {
                    status,
                    cancel: Arc::new(AtomicBool::new(false)),
                    started: Instant::now(),
                },
            );
        }
        evict_if_full(&mut reg);
        assert_eq!(
            reg.len(),
            MAX_SESSIONS - 1,
            "one terminal session evicted at cap"
        );
        assert!(
            !reg.contains_key("served.0"),
            "the (only) terminal session was evicted, not a running one"
        );

        // all-running at cap → nothing evicted (live runs are never dropped)
        let mut all_running: HashMap<String, ServedSession> = HashMap::new();
        for i in 0..MAX_SESSIONS {
            all_running.insert(
                format!("r.{i}"),
                ServedSession {
                    status: ServedStatus::Running,
                    cancel: Arc::new(AtomicBool::new(false)),
                    started: Instant::now(),
                },
            );
        }
        evict_if_full(&mut all_running);
        assert_eq!(
            all_running.len(),
            MAX_SESSIONS,
            "running sessions are never evicted"
        );
    }

    #[test]
    fn initialize_declares_subscribe_capability() {
        let r = dispatch(
            req("initialize", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        assert_eq!(
            r.result.unwrap()["capabilities"]["resources"]["subscribe"],
            true
        );
    }

    #[test]
    fn subscribe_registers_and_notify_pushes_to_the_peer() {
        use std::io::{BufRead, BufReader};
        let ctx = ctx();
        insert_running(&ctx, "served.0");
        // A connected pair: `a` is the peer's write target (what subscribe stores);
        // `b` is the peer's read end, where the pushed notification lands.
        let (a, b) = UnixStream::pair().unwrap();
        let w: SharedWriter = Arc::new(Mutex::new(ServeStream::Unix(a)));
        let uri = "agentd://subagent/served.0";
        assert!(
            subscribe_resource(req("sub", Some(json!({"uri": uri}))), &ctx, &w, 7)
                .error
                .is_none()
        );
        // dedup: a second subscribe from the same conn doesn't double-register.
        subscribe_resource(req("sub", Some(json!({"uri": uri}))), &ctx, &w, 7);
        assert_eq!(ctx.subscriptions.lock().unwrap().get(uri).unwrap().len(), 1);

        notify_resource_updated(&ctx.subscriptions, uri);
        let mut reader = BufReader::new(b);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["method"], "notifications/resources/updated");
        assert_eq!(v["params"]["uri"], uri);
    }

    #[test]
    fn unsubscribe_and_conn_cleanup_drop_subscriptions() {
        let ctx = ctx();
        let uri = "agentd://subagent/served.1";
        insert_running(&ctx, "served.1");
        subscribe_resource(req("sub", Some(json!({"uri": uri}))), &ctx, &writer(), 3);
        subscribe_resource(req("sub", Some(json!({"uri": uri}))), &ctx, &writer(), 4);
        assert_eq!(ctx.subscriptions.lock().unwrap().get(uri).unwrap().len(), 2);
        unsubscribe_resource(req("unsub", Some(json!({"uri": uri}))), &ctx, 3);
        assert_eq!(ctx.subscriptions.lock().unwrap().get(uri).unwrap().len(), 1);
        remove_conn_subscriptions(&ctx, 4); // conn 4 disconnects
        assert!(
            ctx.subscriptions.lock().unwrap().get(uri).is_none(),
            "uri pruned when empty"
        );
    }

    #[test]
    fn subscribe_rejects_non_subscribable_uris() {
        let ctx = ctx();
        // non-agentd, agentd://status (read-only), and an unknown handle all reject.
        for uri in [
            "file:///x",
            "agentd://status",
            "agentd://subagent/served.999",
        ] {
            let r = subscribe_resource(req("sub", Some(json!({"uri": uri}))), &ctx, &writer(), 0);
            assert!(r.error.is_some(), "{uri} must not be subscribable");
        }
        // an already-finished run is not subscribable either (read it instead).
        ctx.sessions.lock().unwrap().insert(
            "served.5".into(),
            ServedSession {
                status: ServedStatus::Done {
                    status: "completed".into(),
                    partial: false,
                    result: json!("ok"),
                },
                cancel: Arc::new(AtomicBool::new(false)),
                started: Instant::now(),
            },
        );
        let r = subscribe_resource(
            req("sub", Some(json!({"uri": "agentd://subagent/served.5"}))),
            &ctx,
            &writer(),
            0,
        );
        assert!(r.error.is_some(), "a terminal run is not subscribable");
    }

    #[test]
    fn unknown_tool_and_method_are_errors() {
        let bad_tool = dispatch(
            req("tools/call", Some(json!({"name": "ghost"}))),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        assert!(bad_tool.error.is_some());
        let bad_method = dispatch(
            req("frobnicate", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        assert!(bad_method.error.is_some());
    }

    #[test]
    fn build_served_payload_sets_instruction_and_narrows_scope() {
        let p = build_served_payload(
            &base(),
            &json!({"instruction": "do x", "tool_scope": ["fs"]}),
            "served.0",
        );
        assert_eq!(p.instruction, "do x");
        assert_eq!(p.mcp_servers.len(), 1); // narrowed to the requested subset
        assert_eq!(p.mcp_servers[0].name, "fs");
        assert_eq!(p.telemetry.agent_path, "served.0"); // handle minted here
        assert_eq!(p.depth, 0);
    }

    #[test]
    fn spawn_missing_instruction_is_a_jsonrpc_error() {
        // Malformed params → JSON-RPC error (not an isError result); never reaches a real spawn.
        let r = dispatch(
            req(
                "tools/call",
                Some(json!({"name": "subagent.spawn", "arguments": {}})),
            ),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        assert!(r.error.is_some(), "missing instruction → JSON-RPC error");
    }
}
