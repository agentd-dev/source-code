// SPDX-License-Identifier: Apache-2.0
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
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload, SwapIntel};
use crate::supervisor::cgroup;
use crate::supervisor::reactor::{SuperviseResult, supervise_once, supervise_swappable};
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

/// What one [`drain_warm`] pass observed: whether the session's resource state
/// advanced (pushes a `agentd://session/<handle>` update), and whether the latched
/// intel all-down state TRANSITIONED on this pass (pushes an `agentd://intelligence`
/// update — RFC 0018 §6). The two are independent surfaces.
#[derive(Default)]
struct WarmDrainOutcome {
    advanced: bool,
    intel_transition: bool,
}

/// Drain a warm session's channel: record completed turns + detect end, and latch
/// any `AgentMsg::IntelHealth` reachability report into the process-global truth
/// (RFC 0018 §6). `advanced` is `true` if the session's observable state advanced
/// (a turn completed or it ended) — the caller pushes a `agentd://session/<handle>`
/// update then. Idempotent: a no-op drain on an already-`done` session is empty.
fn drain_warm(w: &mut WarmServed) -> WarmDrainOutcome {
    let mut out = WarmDrainOutcome::default();
    if w.done {
        return out;
    }
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
                out.advanced = true; // a turn boundary — the session resource changed
            }
            Ok((_, AgentMsg::Result { .. })) | Ok((_, AgentMsg::Failed { .. })) => {
                w.done = true;
                out.advanced = true;
                return out;
            }
            // RFC 0018 §6: latch the served warm session's intel reachability into
            // the ONE eventually-consistent truth `/readyz`, `agentd_intel_all_down`,
            // and `agentd://intelligence`/`capacity` read. The caller fires the
            // notify-then-read on a transition (it holds the subs registry).
            Ok((_, AgentMsg::IntelHealth { all_down, .. })) => {
                if crate::signals::set_intel_all_down(all_down) {
                    crate::obs::metrics::set_intel_all_down(all_down);
                    out.intel_transition = true;
                }
            }
            Ok(_) => {} // Ready / Pong / Event / Usage
            Err(TryRecvError::Empty) => return out,
            Err(TryRecvError::Disconnected) => {
                w.done = true;
                out.advanced = true;
                return out;
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
    let out = drain_warm(w);
    if out.advanced {
        notify_resource_updated_keep(subs, &crate::agentd_uri::session_uri(handle));
    }
    // RFC 0018 §6: an all-down ENTER/EXIT transition is a breaker change — fire the
    // `agentd://intelligence` notify-then-read so a subscriber re-reads the live
    // `all_down` (making the §4.4 "fires on … all-down transitions" contract true).
    if out.intel_transition {
        notify_resource_updated_keep(subs, crate::agentd_uri::INTELLIGENCE_URI);
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

/// One tracked served async run: its state, its per-run cancel flag, its per-run
/// pause flag, and when it started (for age reporting + oldest-first eviction).
struct ServedSession {
    status: ServedStatus,
    cancel: Arc<AtomicBool>,
    /// Per-run pause flag (RFC 0015 §4.3): the instance-wide `pause`/`resume`
    /// tools toggle it; the run's supervisor reactor forwards `ctrl/pause`/
    /// `ctrl/resume` to its live children on each edge. The async parallel of
    /// `cancel` — a shared atomic the reactor reads, never the child directly.
    paused: Arc<AtomicBool>,
    /// Per-run intelligence hot-swap channel (RFC 0018 §5.2): a hot reload that
    /// touches `intelligence`/`model` publishes the new config here; the run's
    /// supervisor reactor fans `ctrl/swap_intel` to its live children at the next
    /// tick (each applies it at a turn boundary). The async parallel of `paused`:
    /// the reload fan-out (via `LiveConfig::fan_swap_intel`) publishes; the reactor
    /// reads + forwards. Cloned into the reactor at launch.
    swap: crate::supervisor::swap::SwapChannel,
    started: Instant,
}

/// Handle → tracked session. Shared (Arc<Mutex>) across the accept/connection
/// threads and each async run's background thread.
type Registry = Arc<Mutex<HashMap<String, ServedSession>>>;

/// A connection's shared write half — both replies and pushed notifications go
/// through it, serialized by the Mutex (a reply and a notification can't interleave
/// bytes). The [`ServeStream`] enum keeps this one type across unix + vsock peers.
/// `pub(crate)` so the A2A streaming handlers ([`crate::mcp::a2a`]) can write their
/// intermediate `StreamResponse` frames directly to the calling connection.
pub(crate) type SharedWriter = Arc<Mutex<ServeStream>>;

/// A peer subscribed to an `agentd://` resource: which connection, and the writer
/// to push a `notifications/resources/updated` to.
struct Subscriber {
    conn: u64,
    writer: SharedWriter,
}

/// uri → its subscribers. Pushed when a served session's resource changes (a run
/// reaches a terminal status). Arc-shared with each async run's background thread.
type SubRegistry = Arc<Mutex<HashMap<String, Vec<Subscriber>>>>;

/// The live, hot-reloadable config plus the served subscription registry, shared
/// between the served self-MCP and the reactive supervisor's reload path (RFC 0017
/// §4.2 / §5.6). The served `agentd://config/effective` read clones the current
/// `Arc<Config>` (lock held only for the cheap clone, never across request
/// handling); on an APPLIED hot reload the supervisor [`swap`](LiveConfig::swap)s
/// in the new config (so the served view goes live) and fires
/// `resources/updated{config/effective}` to subscribers via [`subs`](LiveConfig::subs).
/// The SAME registry backs `ServeCtx.subscriptions` — there is exactly one.
pub struct LiveConfig {
    cfg: Mutex<Arc<Config>>,
    subs: SubRegistry,
    /// The served async-run registry — the SAME one [`ServeCtx`] holds. A hot
    /// reload that touches `intelligence`/`model` (RFC 0018 §5.2) publishes the
    /// new config into each live run's [`SwapChannel`](crate::supervisor::swap::SwapChannel)
    /// via [`fan_swap_intel`](LiveConfig::fan_swap_intel) — the supervisor reach
    /// into served runs, mirroring how `fan_pause` flips each run's `paused`.
    sessions: Registry,
    /// The served warm-session registry — the SAME one [`ServeCtx`] holds. A hot
    /// reload fans `ctrl/swap_intel` straight down each live warm session's control
    /// channel (`w.sub.send`), the parallel of `fan_pause`'s warm branch.
    warm: WarmRegistry,
}

impl LiveConfig {
    /// Build a live-config handle from the startup config snapshot and the served
    /// registries (the same ones [`ServeCtx`] uses for push + run tracking).
    fn new(
        cfg: Arc<Config>,
        subs: SubRegistry,
        sessions: Registry,
        warm: WarmRegistry,
    ) -> Arc<LiveConfig> {
        Arc::new(LiveConfig {
            cfg: Mutex::new(cfg),
            subs,
            sessions,
            warm,
        })
    }

    /// The current (post-any-reload) config. Locks ONLY to clone the cheap
    /// `Arc<Config>` — never held across request handling.
    pub fn current(&self) -> Arc<Config> {
        self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Adopt `cfg` as the live config (called by the reactive supervisor after an
    /// applied hot reload, so the served `config/effective` view reflects it).
    pub fn swap(&self, cfg: Arc<Config>) {
        *self.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    /// Fire `notifications/resources/updated{uri: agentd://config/effective}` to
    /// every current subscriber — the push half of the applied-reload path (RFC
    /// 0017 §5.6). Subscriptions are KEPT (the resource fires on every reload).
    pub fn notify_config_effective_updated(&self) {
        notify_resource_updated_keep(&self.subs, crate::agentd_uri::CONFIG_EFFECTIVE_URI);
    }

    /// Fire `notifications/resources/updated{uri: agentd://intelligence}` to every
    /// subscriber after a hot-swap is applied (RFC 0018 §4.4 / §5.3) — so a
    /// subscribed agentctl re-reads the new endpoint topology / model / swap
    /// policy. Notify-then-read: NO payload in the notification (the body is read
    /// on demand, carrying transport+index only — never the URL/creds). KEPT
    /// (fires on every swap), exactly like the config/effective notify.
    ///
    /// The SAME notify is also fired on an all-down breaker ENTER/EXIT transition
    /// (RFC 0018 §6) — not from here but from the served warm drain
    /// (`drain_warm_notify`), which latches a child's `AgentMsg::IntelHealth` and
    /// re-pushes `agentd://intelligence` on a transition so a subscriber re-reads
    /// the live `all_down`.
    pub fn notify_intelligence_updated(&self) {
        notify_resource_updated_keep(&self.subs, crate::agentd_uri::INTELLIGENCE_URI);
    }

    /// Fire `notifications/tools/list_changed` after an `mcp_servers` hot reload
    /// changed the available tool set (RFC 0005 §3.1 / RFC 0017 §5.3 step 5). The
    /// served self-MCP has no global connection registry, so this broadcasts to
    /// every DISTINCT writer currently in the subscription registry — a connected
    /// agentctl that subscribed to any agentd:// resource (e.g.
    /// `agentd://config/effective`, the config watcher) is exactly the peer that
    /// must re-read the tool catalogue. Best-effort; a dead writer is pruned by its
    /// own reader loop. No payload (the peer re-lists on receipt).
    pub fn notify_tools_list_changed(&self) {
        let writers: Vec<SharedWriter> = {
            let g = self.subs.lock().unwrap_or_else(|e| e.into_inner());
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
        let note = Notification::new(method::NOTIFY_TOOLS_LIST_CHANGED, None);
        for w in writers {
            if let Ok(mut wl) = w.lock() {
                let _ = frame::write_line(&mut *wl, &note);
            }
        }
    }

    /// Fan an intelligence hot-swap (RFC 0018 §5.2) into every live SERVED run —
    /// the supervisor-side reach of a reload that repoints the endpoint list /
    /// changes the model. Mirrors `fan_pause`'s dual fan-out EXACTLY:
    /// - warm served sessions (`warm`): the server holds the `Subagent`, so send
    ///   `ctrl/swap_intel` straight down its control channel (the parallel of
    ///   `w.sub.send(ControlMsg::Pause)`).
    /// - async runs (`sessions`): the server holds only a shared `SwapChannel` the
    ///   run's reactor reads, so PUBLISH into it — the reactor fans the frame to
    ///   its live children (the parallel of flipping `s.paused`). No second control
    ///   path is invented.
    ///
    /// Returns the count of live subtrees the swap reached. Only live (non-terminal)
    /// subtrees take it — a finished run can neither swap nor be reached. The
    /// `token` rides the frame (like the spawn payload) and is NEVER logged.
    pub fn fan_swap_intel(&self, swap: SwapIntel) -> u64 {
        let mut reached = 0u64;
        {
            let msg = ControlMsg::SwapIntel(Box::new(swap.clone()));
            let mut warm = self.warm.lock().unwrap_or_else(|e| e.into_inner());
            for w in warm.values_mut() {
                if w.done {
                    continue;
                }
                if w.sub.send(&msg).is_ok() {
                    reached += 1;
                }
            }
        }
        {
            let reg = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            for s in reg.values() {
                if s.status.is_terminal() {
                    continue;
                }
                s.swap.publish(swap.clone());
                reached += 1;
            }
        }
        reached
    }
}

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
    ///
    /// This is the STARTUP SNAPSHOT: `capabilities` exposes compile-time /
    /// restart-only fields and `capacity`'s config-derived fields are restart-only,
    /// so a snapshot is correct for them. Only `agentd://config/effective` is about
    /// RELOADABLE fields, and it reads [`live_config`](ServeCtx::live_config)
    /// instead (the post-reload view).
    config: Arc<Config>,
    /// The live, hot-reloadable config + the shared subscription registry (RFC
    /// 0017 §4.2 / §5.6). Backs `agentd://config/effective` (the current,
    /// post-reload redacted view) and is handed to the reactive supervisor so an
    /// applied reload swaps it + pushes `resources/updated`. Its `subs` IS
    /// `subscriptions` above (one registry, not two).
    live_config: Arc<LiveConfig>,
    /// The terminal run-outcome report (RFC 0016 §6.2), once the run reaches a
    /// terminal status — `None` while running. Folded into `agentd://run/<run_id>`
    /// so a still-connected reader (vsock mgmt profile) learns the outcome without
    /// the file (§6.3). The driver publishes it through [`ServeHandle::publish_report`]
    /// at the terminal transition. Arc-shared so the read borrows it cheaply.
    report: Arc<Mutex<Option<Value>>>,
    /// RFC 0018 §5.4 lazy + cached model discovery. `None` until the served
    /// `agentd://intelligence` / live `agentd://capabilities` surface is FIRST
    /// read; then the probe runs, the result is cached with a read `Instant`, and
    /// is reused for [`DISCOVERY_TTL`]. NEVER probed at startup before validation
    /// (RFC 0011 §3.3 — it is a network call); the probe runs only on a served
    /// read, off the hot path, and degrades silently (§5.4). Arc-shared so the
    /// per-read borrow is cheap; the `Instant` keys staleness for the refresh.
    discovery_cache: Arc<Mutex<Option<(Instant, crate::intel::discovery::DiscoveryResult)>>>,
}

/// RFC 0018 §5.4 discovery-cache TTL: a served `agentd://intelligence` /
/// `agentd://capabilities` read older than this re-probes; within it, the cached
/// model set is reused (the reads are operator-driven + infrequent, so a small
/// TTL keeps the additive field fresh without re-probing on every read).
const DISCOVERY_TTL: Duration = Duration::from_secs(60);

impl ServeCtx {
    pub fn new(
        run_id: String,
        mode: String,
        exe: PathBuf,
        base: SpawnPayload,
        drain_timeout: Duration,
        config: Arc<Config>,
    ) -> ServeCtx {
        // ONE subscription registry, shared by the served push helpers AND the
        // live-config handle (the reload notify fires on the SAME registry). The
        // session + warm registries are likewise shared into `LiveConfig` so the
        // reload-driven swap fan-out reaches the SAME live runs (RFC 0018 §5.2).
        let subscriptions: SubRegistry = Arc::new(Mutex::new(HashMap::new()));
        let sessions: Registry = Arc::new(Mutex::new(HashMap::new()));
        let warm: WarmRegistry = Arc::new(Mutex::new(HashMap::new()));
        let live_config = LiveConfig::new(
            Arc::clone(&config),
            Arc::clone(&subscriptions),
            Arc::clone(&sessions),
            Arc::clone(&warm),
        );
        ServeCtx {
            run_id,
            mode,
            started: Instant::now(),
            exe,
            base,
            drain_timeout,
            inflight: Arc::new(AtomicUsize::new(0)),
            counter: Arc::new(AtomicU64::new(0)),
            sessions,
            warm,
            subscriptions,
            conn_counter: Arc::new(AtomicU64::new(0)),
            config,
            live_config,
            report: Arc::new(Mutex::new(None)),
            discovery_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// The live-config + shared-subscription handle (RFC 0017 §4.2 / §5.6). The
    /// reactive supervisor adopts this so an APPLIED hot reload swaps the served
    /// `agentd://config/effective` view and pushes `resources/updated` to
    /// subscribers — the SAME registry the rest of the served surface uses.
    pub fn live_config(&self) -> Arc<LiveConfig> {
        Arc::clone(&self.live_config)
    }

    /// The `agentd://config/effective` body (RFC 0017 §4.2): the CURRENT
    /// (post-any-reload) redacted reloadable-config view. Carries NO secret / URL /
    /// `{{secret:…}}` value — `Config::effective_view` guarantees the redaction
    /// (header NAMES only, structural server names only). Reads the live config,
    /// so a read after a reload reflects it.
    fn config_effective_body(&self) -> Value {
        self.live_config.current().effective_view()
    }

    /// The live `agentd://capabilities` manifest — this daemon's self-description
    /// plus cheap liveness counters lifted off the same atomics the `status`
    /// surface reads (RFC 0015 §3.4). Built `live=true` from the running daemon.
    fn capabilities_body(&self) -> Value {
        // ONE builder for the one-shot `agentd --capabilities` and this live
        // resource, so they never drift (RFC 0015 §5.2). Identity is env-only and
        // cheap, so build it per read rather than threading it through ServeCtx.
        let identity = crate::identity::Identity::from_env(&self.run_id);
        let mut manifest = crate::capabilities::manifest(&self.config, &identity, true);
        // RFC 0018 §5.4: overlay the LIVE discovery probe onto the manifest's
        // `intelligence.models`. The one-shot `--capabilities` (live=false) is
        // network-free and skips this (RFC 0015 §5.2 side-effect-free admission);
        // only this served read probes — lazily + cached, off the hot path. Read
        // the LIVE (post-reload) intelligence config so a hot-swap is reflected.
        let cfg = self.live_config.current();
        let uri = cfg.intelligence.as_deref().unwrap_or_default();
        if let Ok(list) =
            crate::intel::endpoints::EndpointList::parse(uri, cfg.intelligence_token.clone())
        {
            let disc = self.discovery(&list, cfg.model.as_deref());
            crate::capabilities::intelligence_discovery_overlay(
                &mut manifest,
                disc.discovery,
                &disc.models,
            );
        }
        manifest
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

    /// The `agentd://inventory` projection (RFC 0015 §5.3) — the instance-local
    /// view of the served subagent tree: the lifecycle flags, totals, and a
    /// `nodes[]` array projected from the served-run registries (warm sessions +
    /// async runs). It is a pure read of supervisor-held state, so it costs only
    /// serialization. `draining` reads the global drain latch (the same one the
    /// `drain` tool/SIGTERM set); `ready` reflects the lame-duck override; `paused`
    /// reflects the instance-wide pause flag (the `pause`/`resume` operator tools,
    /// RFC 0015 §4.3). Pause is tree-wide, so the instance flag is the source of
    /// truth and every live node mirrors it (per-node pause is not tracked
    /// separately — pause suspends the whole tree). `status` reuses the
    /// terminal-status vocabulary (RFC 0007 §3.4) plus `running` for a live node —
    /// this surface introduces no new status.
    fn inventory_body(&self) -> Value {
        let mut nodes: Vec<Value> = Vec::new();
        let mut active = 0u64;
        // Pause is instance-wide (RFC 0015 §4.3); a live node mirrors it.
        let paused = crate::signals::paused();
        // Warm sessions: live (driven by subagent.send) until done. Drain is not
        // done here (a &self read must not mutate the channel); `status` reflects
        // the last observed turn state.
        if let Ok(warm) = self.warm.lock() {
            for (handle, w) in warm.iter() {
                let status = if w.done {
                    "completed"
                } else if w.pending > 0 {
                    "working"
                } else {
                    "idle"
                };
                if !w.done {
                    active += 1;
                }
                nodes.push(json!({
                    "handle": handle,
                    "depth": 0,                 // served runs are fresh roots (RFC 0005 §3.2)
                    "kind": "warm",
                    "status": status,
                    "paused": paused && !w.done, // a live node mirrors the tree pause
                    "usage": { "turns": w.turns },
                    "last_event_ms": w.started.elapsed().as_millis() as u64,
                }));
            }
        }
        // Async runs, by handle.
        if let Ok(reg) = self.sessions.lock() {
            for (handle, s) in reg.iter() {
                let status = match &s.status {
                    ServedStatus::Running => "running",
                    ServedStatus::Done { status, .. } => status.as_str(),
                    ServedStatus::Failed(_) => "failed",
                    ServedStatus::Cancelled => "cancelled",
                };
                if !s.status.is_terminal() {
                    active += 1;
                }
                nodes.push(json!({
                    "handle": handle,
                    "depth": 0,
                    "kind": "async",
                    "status": status,
                    "paused": paused && !s.status.is_terminal(), // live nodes mirror the tree pause
                    "usage": {},
                    "last_event_ms": s.started.elapsed().as_millis() as u64,
                }));
            }
        }
        json!({
            "run_id": self.run_id,
            "mode": self.mode,
            // Instance-level lifecycle flags (RFC 0015 §4 / §5.3).
            "draining": crate::signals::draining(),
            "paused": paused, // instance-wide pause (RFC 0015 §4.3); NOT readiness
            "ready": !crate::signals::lame_duck() && !crate::signals::draining(),
            "totals": {
                "active": active,
                "total_spawned": self.counter.load(Ordering::Relaxed),
                "depth": 0,
            },
            "nodes": nodes,
        })
    }

    /// The `agentd://intelligence` body (RFC 0018 §4.4 / §6): the endpoint list
    /// (transport + index, NEVER the URL/creds), which is active, the swap policy,
    /// the discovery surface, and the LIVE all-down reachability. Built from the
    /// daemon config's endpoint topology — a pure structural read, no secret, no URL
    /// (RFC 0012 §3.7).
    ///
    /// HONESTY (RFC 0018 §6): in served mode the model loop runs in a CHILD subagent
    /// process that owns the live breaker/failover state — the supervisor has no LLM
    /// and cannot re-derive per-endpoint breaker state. So:
    /// - the top-level `all_down` is the LATCHED, eventually-consistent truth a child
    ///   reports up via `AgentMsg::IntelHealth` (NOT a fresh-parse fiction that was
    ///   structurally always-false), the same flag that flips `/readyz`; and
    /// - the per-endpoint `state`/`error_rate`/`latency` that genuinely is NOT bridged
    ///   is reported as `"unknown"` / `aggregated:false` rather than fabricating a
    ///   freshly-parsed `closed`/0. The endpoint TOPOLOGY (index/transport/addr) is
    ///   real; only the per-endpoint live health is not aggregated supervisor-side.
    fn intelligence_body(&self) -> Value {
        // Read the LIVE (post-reload) config: `intelligence`/`model` are reloadable
        // via RFC 0018 §5, so a hot-swap must be reflected here. A swap (and an
        // all-down breaker transition, §6) fans `notifications/resources/updated{
        // agentd://intelligence}` so a subscriber re-reads this.
        let cfg = self.live_config.current();
        let uri = cfg.intelligence.as_deref().unwrap_or_default();
        match crate::intel::endpoints::EndpointList::parse(uri, cfg.intelligence_token.clone()) {
            Ok(list) => {
                let mut body = list.body(cfg.model.as_deref());
                // The active swap policy (§4.4 / §5) — non-secret, read on demand.
                if let Value::Object(m) = &mut body {
                    // RFC 0018 §6: replace the fresh-parse `all_down` (structurally
                    // always-false here — the supervisor's parse has fresh breakers)
                    // with the LATCHED last-child-experience truth. This is the same
                    // flag /readyz + `agentd_intel_all_down` read.
                    let all_down = crate::signals::intel_all_down();
                    m.insert("all_down".into(), json!(all_down));
                    // Be honest that the per-endpoint breaker detail is NOT bridged
                    // from the child loop to this supervisor-side view: the topology
                    // is real, but the live per-endpoint `state`/`error_rate`/latency
                    // are not aggregated here. A reader keys off `all_down` (latched)
                    // + `agentd_intel_*` metrics for live health, not these fields.
                    m.insert("health_aggregated".into(), json!(false));
                    m.insert("all_down_source".into(), json!("latched_child_report"));
                    if let Some(Value::Array(eps)) = m.get_mut("endpoints") {
                        for ep in eps.iter_mut() {
                            if let Value::Object(epm) = ep {
                                // The fresh-parse health fields are not real — null
                                // them out and mark the breaker state unknown rather
                                // than fabricating closed/0 (RFC 0018 §6 honesty).
                                epm.insert("state".into(), json!("unknown"));
                                epm.remove("ewma_latency_ms");
                                epm.remove("error_rate");
                                epm.remove("consec_fail");
                            }
                        }
                    }
                    m.insert("swap_policy".into(), json!(cfg.model_swap.as_str()));
                    // RFC 0018 §5.4: the discovery surface — `discovery` (did any
                    // endpoint answer /v1/models) + `models` (union of discovered +
                    // configured). Lazy + cached (probed only on THIS read, off the
                    // hot path), silent-degrade. `[]`/false if none discovered.
                    let disc = self.discovery(&list, cfg.model.as_deref());
                    m.insert("discovery".into(), json!(disc.discovery));
                    m.insert("models".into(), json!(disc.models));
                }
                body
            }
            // A misconfigured list (should not reach a running daemon — startup
            // validated it) degrades to an empty, honest body rather than erroring.
            Err(_) => json!({ "active": 0, "all_down": true, "endpoints": [] }),
        }
    }

    /// RFC 0018 §5.4 lazy + cached model discovery for the served surfaces. On a
    /// fresh (`None`) or stale (older than [`DISCOVERY_TTL`]) cache, run the
    /// best-effort probe over the endpoint `list` (each OpenAI-compatible endpoint
    /// gets one `GET /v1/models` over the existing transport; anthropic → none),
    /// cache it with the read `Instant`, and return it; otherwise reuse the cache.
    ///
    /// This is the ONLY caller of the probe. It runs supervisor-side, exclusively
    /// on a served `agentd://intelligence` / live `agentd://capabilities` read —
    /// NEVER at startup before validation (RFC 0011 §3.3), NEVER on the hot path.
    /// The probe degrades silently (§5.4): a 404 / connection / non-JSON yields no
    /// models + `discovery:false`, never fatal, never a failover-class error.
    fn discovery(
        &self,
        list: &crate::intel::endpoints::EndpointList,
        model: Option<&str>,
    ) -> crate::intel::discovery::DiscoveryResult {
        let mut cache = self
            .discovery_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((at, result)) = cache.as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return result.clone();
        }
        let result = crate::intel::discovery::discover(
            list,
            model,
            crate::intel::discovery::DEFAULT_TIMEOUT,
        );
        *cache = Some((Instant::now(), result.clone()));
        result
    }

    /// The `agentd://capacity` body (RFC 0019 §7.2/§9): this instance's placement
    /// view — identity, shard `K/N`, free slots, active subagents, intelligence
    /// warmth/health, and saturation — read by agentctl to place work. Built from
    /// the supervisor-reachable state (config + the served-run atomics + the
    /// configured intelligence topology); no secret, no URL (RFC 0012 §3.7).
    ///
    /// `active_subagents` is the served-run in-flight spawn count (the load-bearing
    /// saturation numerator the supervisor can see); `free_slots` = `max_total -
    /// active`; `saturation` = `active / max_total` (the tree cap, RFC 0009 — the
    /// per-route product is not reachable from the served ctx, so the cap alone is
    /// used per RFC 0019 §5.1's `min(…, max_total_subagents)`). `standby` reflects
    /// `--standby` (RFC 0019 §7). Intelligence `warm`/`healthy` derive
    /// from the configured endpoint list's all-down flag (the supervisor-side view
    /// is fresh per RFC 0018 §4.4 — the model loop runs in a child process).
    #[cfg(feature = "cluster")]
    fn capacity_body(&self) -> Value {
        let identity = crate::identity::Identity::from_env(&self.run_id);
        let max_total = u64::from(crate::supervisor::tree::Caps::default().max_total);
        let active = self.inflight.load(Ordering::Relaxed) as u64;
        let free = max_total.saturating_sub(active);
        let saturation = if max_total == 0 {
            0.0
        } else {
            (active as f64 / max_total as f64).min(1.0)
        };
        // Intelligence warmth/health (RFC 0018 §6): the supervisor has no LLM and
        // cannot probe the endpoints itself, so warmth/health derive from the
        // LATCHED, eventually-consistent all-down truth a child reports up via
        // `AgentMsg::IntelHealth` (the same flag /readyz + `agentd_intel_all_down`
        // read) — NOT a fresh config-parse whose breakers are structurally always
        // closed (which made `healthy` always true regardless of a down endpoint).
        // A misconfigured list (should not reach a running daemon — startup
        // validated it) reads not-warm. `warm` == `healthy` here: both mean "the
        // fleet should route model work to this pod", and that is exactly what the
        // latched all-down flag answers.
        let uri = self.config.intelligence.as_deref().unwrap_or_default();
        let configured_ok = crate::intel::endpoints::EndpointList::parse(
            uri,
            self.config.intelligence_token.clone(),
        )
        .is_ok();
        let reachable = configured_ok && !crate::signals::intel_all_down();
        let (warm, healthy) = (reachable, reachable);
        json!({
            "instance": identity.instance,
            // The shard identity string "K/N", or null when unsharded (N==1).
            "shard": self.config.shard.label(),
            // Standby (RFC 0019 §7) reflects `--standby`: agentctl reads this to
            // place a directed assignment only on warm standby members.
            "standby": self.config.standby,
            "free_slots": free,
            "active_subagents": active,
            "intelligence": { "warm": warm, "healthy": healthy },
            "max_total_subagents": max_total,
            "saturation": saturation,
        })
    }

    /// The aggregate state of this served run — what `agentd://run/<run_id>`
    /// reads + pushes on each change. The `root` handle convention names the run's
    /// own node (depth 0); `status` is "running" for the daemon's life. Spawn
    /// counts come straight from the same atomics `status_body` reads (no token
    /// aggregation exists yet — RFC 0005 §3.3 reports counts, not totals).
    ///
    /// Once the run reaches a terminal status, the frozen run-outcome report
    /// (RFC 0016 §6.2) is folded in under `"report"` and `status` flips to the
    /// report's terminal-status string — so a still-connected reader learns the
    /// outcome over the resource without the `--report-file` (§6.3.2). The driver
    /// publishes the report via [`ServeHandle::publish_report`] at the transition.
    fn run_body(&self) -> Value {
        let report = self
            .report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // The terminal status string (if any) from the report — the run aggregate's
        // `status` flips to it; otherwise the daemon is "running".
        let status = report
            .as_ref()
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("running")
            .to_string();
        let mut body = json!({
            "run_id": self.run_id,
            "mode": self.mode,
            "root": "0", // the run's own node id (depth-0 root) by convention
            "status": status,
            "inflight_spawns": self.inflight.load(Ordering::Relaxed),
            "total_spawns": self.counter.load(Ordering::Relaxed),
            "uptime_ms": self.started.elapsed().as_millis() as u64,
        });
        if let (Value::Object(m), Some(report)) = (&mut body, report) {
            m.insert("report".into(), report);
        }
        body
    }

    /// The `agentd://events` read body (RFC 0016 §7.2): the §7.2 envelope
    /// (`events_schema`/`oldest_seq`/`newest_seq`/`dropped`/`events`) drained from
    /// the bounded in-memory ring with the `?after=<seq>` cursor + the optional
    /// §7.3 level/event-prefix filters. `None` when no ring is installed (the
    /// resource 404s — the build/serve did not arm it). Pure read of the ring; it
    /// never blocks the supervisor (lossy-by-design, §8.4).
    #[cfg(feature = "events")]
    fn events_body(&self, q: &crate::agentd_uri::EventsQuery) -> Option<Value> {
        // A bounded read window so one read can't return an unbounded ring; the
        // subscriber advances `after` and reads again (the standard MCP cursor).
        const READ_LIMIT: usize = 512;
        let prefixes: Vec<&str> = q.event_prefixes.iter().map(String::as_str).collect();
        let w =
            crate::obs::log::read_event_window(q.after, READ_LIMIT, q.level.as_deref(), &prefixes)?;
        Some(json!({
            "events_schema": crate::obs::log::EVENTS_SCHEMA,
            "oldest_seq": w.oldest_seq,
            "newest_seq": w.newest_seq,
            "dropped": w.dropped,
            "events": w.events,
        }))
    }
}

/// The A2A binding's access to the served-run machinery (RFC 0020 §5). These are
/// the thin reuse seams the `a2a` module ([`crate::mcp::a2a`]) translates to/from
/// A2A `Task` objects — they do NOT duplicate the spawn/cancel/registry code, they
/// drive the SAME `sessions` registry + `launch_async_run`/`supervise_once` path
/// the `subagent.*` tools use. A `Task` IS a served run. [feature: a2a]
#[cfg(feature = "a2a")]
impl ServeCtx {
    /// Start a served run for an A2A `SendMessage` and return its handle (the
    /// Task `id`). Mirrors `handle_spawn`'s async path: mint a `served.N` handle,
    /// build the run payload from the daemon template + instruction, acquire a
    /// concurrency permit, and launch it on the same `launch_async_run` thread the
    /// `subagent.spawn{async}` tool uses. Pushes the run/inventory notifications a
    /// new spawn always does. `Err` is the concurrency-cap or thread-spawn refusal.
    pub(crate) fn a2a_spawn_async(
        &self,
        instruction: &str,
        log: &Logger,
    ) -> Result<String, String> {
        // Backpressure mirrors the `subagent.spawn` chokepoint: refuse under
        // memory.high rather than push the cgroup toward OOM (best-effort).
        if cgroup::under_memory_pressure() {
            return Err(
                "spawn refused: memory pressure (cgroup at memory.high); retry shortly".to_string(),
            );
        }
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        notify_resource_updated_keep(
            &self.subscriptions,
            &crate::agentd_uri::run_uri(&self.run_id),
        );
        notify_resource_updated_keep(&self.subscriptions, crate::agentd_uri::INVENTORY_URI);
        let handle = format!("served.{n}");
        let args = json!({ "instruction": instruction });
        let payload = build_served_payload(&self.base, &args, &handle);
        let permit = SpawnGuard::acquire(&self.inflight, MAX_INFLIGHT_SPAWNS).ok_or_else(|| {
            format!("spawn refused: {MAX_INFLIGHT_SPAWNS} concurrent served spawns in flight")
        })?;
        log.info(
            "a2a.send_message",
            json!({"handle": handle, "async": true, "servers": payload.mcp_servers.len()}),
        );
        launch_async_run(self, log, &handle, payload, permit)
            .map(|()| handle)
            .map_err(|()| "subagent could not start: thread spawn failed".to_string())
    }

    /// Start a BLOCKING served run for an A2A `SendMessage{returnImmediately:false}`
    /// and return `(handle, terminal_status, result?)`. Mirrors `handle_spawn`'s
    /// sync path (`supervise_once`); a cap/start failure is surfaced as a terminal
    /// `crashed`/`refused`-shaped status so the caller always gets a Task, never a
    /// crash. The handle is registered terminal so a later `GetTask` can read it.
    pub(crate) fn a2a_spawn_sync(
        &self,
        instruction: &str,
        log: &Logger,
    ) -> (String, String, Option<Value>) {
        self.a2a_spawn_sync_with(instruction, log, |_| {})
    }

    /// `a2a_spawn_sync` for the STREAMING path (`a2a.SendStreamingMessage`):
    /// identical blocking served-spawn, but `on_handle` fires with the minted handle
    /// the instant the run is registered + launched — BEFORE the blocking wait — so
    /// the streaming handler can emit its `statusUpdate{WORKING}` frame while the run
    /// is genuinely in flight. Returns the same `(handle, terminal_status, result?)`.
    pub(crate) fn a2a_spawn_stream_sync(
        &self,
        instruction: &str,
        log: &Logger,
        on_handle: impl FnOnce(&str),
    ) -> (String, String, Option<Value>) {
        self.a2a_spawn_sync_with(instruction, log, on_handle)
    }

    /// Shared core of the A2A sync served-spawn (unary `SendMessage{!immediate}` +
    /// streaming `SendStreamingMessage`): mint the handle, fire `on_handle` once it's
    /// known (the stream's WORKING-frame hook; a no-op for the unary path), block on
    /// `supervise_once`, and record the terminal `ServedSession` so a later
    /// `GetTask`/`SubscribeToTask` resolves it. A cap/pressure refusal is a terminal
    /// `crashed`-shaped status (the caller always gets a Task). `on_handle` runs even
    /// on a refusal, so the stream still opens with a WORKING frame before the
    /// immediately-terminal close.
    fn a2a_spawn_sync_with(
        &self,
        instruction: &str,
        log: &Logger,
        on_handle: impl FnOnce(&str),
    ) -> (String, String, Option<Value>) {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let handle = format!("served.{n}");
        notify_resource_updated_keep(
            &self.subscriptions,
            &crate::agentd_uri::run_uri(&self.run_id),
        );
        notify_resource_updated_keep(&self.subscriptions, crate::agentd_uri::INVENTORY_URI);
        on_handle(&handle);
        if cgroup::under_memory_pressure() {
            self.record_terminal_session(&handle, ServedStatus::Failed("memory pressure".into()));
            return (handle, "crashed".to_string(), None);
        }
        let permit = match SpawnGuard::acquire(&self.inflight, MAX_INFLIGHT_SPAWNS) {
            Some(g) => g,
            None => {
                self.record_terminal_session(
                    &handle,
                    ServedStatus::Failed("spawn cap reached".into()),
                );
                return (handle, "crashed".to_string(), None);
            }
        };
        log.info(
            "a2a.send_message",
            json!({"handle": handle, "async": false}),
        );
        let args = json!({ "instruction": instruction });
        let payload = build_served_payload(&self.base, &args, &handle);
        let result = supervise_once(self.exe.clone(), &payload, self.drain_timeout, log.clone());
        drop(permit);
        let status = run_to_status(result, false);
        // Record the terminal session so a later GetTask/ListTasks can read it.
        let (status_str, result_val) = a2a_status_and_result(&status);
        self.record_terminal_session(&handle, status);
        (handle, status_str, result_val)
    }

    /// Insert a terminal `ServedSession` under `handle` (evicting an old finished one
    /// if the registry is full) so a later `GetTask`/`ListTasks`/`SubscribeToTask`
    /// resolves it.
    fn record_terminal_session(&self, handle: &str, status: ServedStatus) {
        let mut reg = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        evict_if_full(&mut reg);
        reg.insert(
            handle.to_string(),
            ServedSession {
                status,
                cancel: Arc::new(AtomicBool::new(false)),
                paused: Arc::new(AtomicBool::new(false)),
                // A terminal session never runs again, so its swap channel is inert.
                swap: crate::supervisor::swap::SwapChannel::new(),
                started: Instant::now(),
            },
        );
    }

    /// Poll the sessions registry for a still-running served run until it reaches a
    /// terminal status OR a bounded deadline elapses (the drain timeout — the same
    /// cap `ServeHandle::drain` waits on). Used by `a2a.SubscribeToTask` to follow a
    /// live run to completion. Returns `(status_string, result?)`: the real terminal
    /// status on completion, or — if the deadline elapses while still running — the
    /// synthetic `"running"` so the caller closes the stream honestly (a bounded
    /// subscribe never hangs a connection thread forever). An evicted/unknown handle
    /// mid-poll resolves as `"running"` too (it can't be re-read; the stream closes).
    pub(crate) fn a2a_poll_until_terminal(&self, handle: &str) -> (String, Option<Value>) {
        const POLL_INTERVAL: Duration = Duration::from_millis(50);
        let deadline = Instant::now() + self.drain_timeout;
        loop {
            match self.a2a_task_snapshot(handle) {
                // Terminal (or vanished) → done.
                Some((status, result)) if status != "running" => return (status, result),
                None => return ("running".to_string(), None),
                Some(_) => {} // still running
            }
            if Instant::now() >= deadline {
                return ("running".to_string(), None);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Test-only: seed a terminal `Done` served session under `handle` with the
    /// given terminal-status string + distillate, so the A2A binding's tests
    /// ([`crate::mcp::a2a`]) can drive `GetTask`/`SubscribeToTask`/`ListTasks`
    /// against a pre-existing run without spawning a real subagent.
    #[cfg(test)]
    pub(crate) fn a2a_seed_done(&self, handle: &str, status: &str, result: Value) {
        self.record_terminal_session(
            handle,
            ServedStatus::Done {
                status: status.to_string(),
                partial: false,
                result,
            },
        );
    }

    /// Read a served run by handle for an A2A `GetTask` → `(status_string,
    /// result?)`, or `None` if the handle is unknown (→ A2A `TaskNotFound`). A
    /// still-running run reports the synthetic `"running"` status the binding maps
    /// to `WORKING`; a terminal run reports its real terminal status + (for a
    /// completed run) its distillate result.
    pub(crate) fn a2a_task_snapshot(&self, handle: &str) -> Option<(String, Option<Value>)> {
        let reg = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        reg.get(handle).map(|s| match &s.status {
            ServedStatus::Running => ("running".to_string(), None),
            other => a2a_status_and_result(other),
        })
    }

    /// Cancel a served run by handle for an A2A `CancelTask`. `Some(true)` = a live
    /// run's cancel was requested (it drains via the kill ladder); `Some(false)` =
    /// the run is already terminal (cancel-of-finished is a read); `None` = unknown
    /// handle (→ A2A `TaskNotFound`). Wraps the same per-run cancel flag the
    /// `subagent.cancel`/`cancel` tools set, and pushes the inventory notification.
    pub(crate) fn a2a_cancel(&self, handle: &str) -> Option<bool> {
        let requested = {
            let mut reg = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match reg.get_mut(handle) {
                None => return None,
                Some(s) if s.status.is_terminal() => false,
                Some(s) => {
                    s.cancel.store(true, Ordering::Relaxed);
                    true
                }
            }
        };
        if requested {
            notify_resource_updated_keep(&self.subscriptions, crate::agentd_uri::INVENTORY_URI);
        }
        Some(requested)
    }

    /// List the live served-run registry for an A2A `ListTasks` → one
    /// `(handle, status_string, result?)` per tracked run. The ephemeral
    /// instance-local view only (durable history is gateway-held, RFC 0020 §7).
    pub(crate) fn a2a_list(&self) -> Vec<(String, String, Option<Value>)> {
        let reg = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        reg.iter()
            .map(|(handle, s)| {
                let (status, result) = match &s.status {
                    ServedStatus::Running => ("running".to_string(), None),
                    other => a2a_status_and_result(other),
                };
                (handle.clone(), status, result)
            })
            .collect()
    }
}

/// Project a terminal [`ServedStatus`] to `(status_string, result?)` for the A2A
/// binding: the run's terminal-status vocabulary (RFC 0007 §3.4) plus the
/// distillate result for a `Done`/completed run. A `Failed`/`Cancelled` run
/// carries no result (the distillate-only invariant — RFC 0009 §8). `Running` is
/// never passed here (the callers special-case it to `"running"`).
#[cfg(feature = "a2a")]
fn a2a_status_and_result(status: &ServedStatus) -> (String, Option<Value>) {
    match status {
        ServedStatus::Done { status, result, .. } => (status.clone(), Some(result.clone())),
        ServedStatus::Failed(_) => ("crashed".to_string(), None),
        ServedStatus::Cancelled => ("cancelled".to_string(), None),
        // Unreachable in practice (callers special-case Running), but total.
        ServedStatus::Running => ("running".to_string(), None),
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
    /// The run aggregate's terminal-report slot + its subscribers + uri, so the
    /// driver can publish the run-outcome report (RFC 0016 §6.3.2) through the
    /// handle at the terminal transition (folded into `agentd://run/<run_id>`,
    /// with a final `notifications/resources/updated`).
    report: Arc<Mutex<Option<Value>>>,
    subscriptions: SubRegistry,
    run_uri: String,
}

impl ServeHandle {
    /// Publish the frozen run-outcome report (RFC 0016 §6.2) onto the served
    /// `agentd://run/<run_id>` resource and fire a final
    /// `notifications/resources/updated` so a still-connected reader (vsock mgmt
    /// profile) learns the outcome without the `--report-file` (§6.3.2). Telemetry
    /// never crashes the run (§8.4): a poisoned lock is recovered, never fatal.
    /// `report` is the §6.2 JSON object (built by [`crate::report::RunReport`]).
    pub fn publish_report(&self, report: Value) {
        {
            let mut slot = self.report.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(report);
        }
        // The run resource fires REPEATEDLY (each spawn / terminal change), so keep
        // the subscriber list — this is its final, terminal emission.
        notify_resource_updated_keep(&self.subscriptions, &self.run_uri);
    }

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
        json!({"transport": "unix", "path": path, "tools": ["status", "subagent.spawn", "subagent.send", "subagent.status", "subagent.cancel"], "resources": ["agent://status", "agent://capabilities", crate::agentd_uri::run_uri(&ctx.run_id)]}),
    );
    let handle = ServeHandle {
        sessions: Arc::clone(&ctx.sessions),
        warm: Arc::clone(&ctx.warm),
        inflight: Arc::clone(&ctx.inflight),
        report: Arc::clone(&ctx.report),
        subscriptions: Arc::clone(&ctx.subscriptions),
        run_uri: crate::agentd_uri::run_uri(&ctx.run_id),
    };
    // Coalesce ring-growth into `agentd://events` notifications (RFC 0016 §7.2).
    #[cfg(feature = "events")]
    spawn_events_notifier(Arc::clone(&ctx.subscriptions));
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
        json!({"transport": "vsock", "cid": cid, "port": port, "tools": ["status", "subagent.spawn", "subagent.send", "subagent.status", "subagent.cancel"], "resources": ["agent://status", "agent://capabilities", crate::agentd_uri::run_uri(&ctx.run_id)]}),
    );
    let handle = ServeHandle {
        sessions: Arc::clone(&ctx.sessions),
        warm: Arc::clone(&ctx.warm),
        inflight: Arc::clone(&ctx.inflight),
        report: Arc::clone(&ctx.report),
        subscriptions: Arc::clone(&ctx.subscriptions),
        run_uri: crate::agentd_uri::run_uri(&ctx.run_id),
    };
    // Coalesce ring-growth into `agentd://events` notifications (RFC 0016 §7.2).
    #[cfg(feature = "events")]
    spawn_events_notifier(Arc::clone(&ctx.subscriptions));
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
/// Test-only `dispatch` seam: routes one request at the given origin with a
/// throwaway connection writer (its peer end is dropped). Lets sibling-module tests
/// ([`crate::mcp::a2a`]) exercise the origin gate (e.g. a `Stdio`-origin `a2a.*`
/// falling through to `-32601`) without re-plumbing a full connection. Only the
/// `a2a` tests use this seam, so it's gated on that feature to stay dead-code-free
/// in test builds that don't (e.g. `--features serve-mcp,hot-reload`).
#[cfg(all(test, feature = "a2a"))]
pub(crate) fn dispatch_for_test(
    req: Request,
    ctx: &ServeCtx,
    origin: PeerOrigin,
    log: &Logger,
) -> Response {
    let (a, _b) = UnixStream::pair().expect("socketpair");
    let writer: SharedWriter = Arc::new(Mutex::new(ServeStream::Unix(a)));
    dispatch(req, ctx, origin, &writer, 0, log)
}

fn dispatch(
    req: Request,
    ctx: &ServeCtx,
    origin: PeerOrigin,
    writer: &SharedWriter,
    conn: u64,
    log: &Logger,
) -> Response {
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
        // The work tools are listed to every peer; the operator tools
        // (drain/lame-duck/cancel) only to a `Management` peer (RFC 0015 §3.4) —
        // a stdio-spawned subagent must never see, much less call, them.
        "tools/list" => {
            let mut tools = vec![
                status_tool_def(),
                spawn_tool_def(),
                send_tool_def(),
                session_status_tool_def(),
                session_cancel_tool_def(),
            ];
            if origin == PeerOrigin::Management {
                tools.extend(operator_tool_defs());
            }
            Response::ok(req.id, json!({ "tools": tools }))
        }
        "tools/call" => tools_call(req, ctx, origin, log),
        "resources/list" => Response::ok(req.id, json!({"resources": resource_list(ctx, origin)})),
        "resources/read" => resources_read(req, ctx, origin),
        "resources/subscribe" => subscribe_resource(req, ctx, origin, writer, conn),
        "resources/unsubscribe" => unsubscribe_resource(req, ctx, conn),
        // The A2A external-agent surface (RFC 0020). Served only over the trusted
        // management transport — the gateway is the PEP that already authenticated
        // the client; a `Stdio` peer (a spawned subagent) must never reach it, so
        // its `a2a.*` call falls through to the `-32601` catch-all below. Gated on
        // the `a2a` feature: without it, `a2a.*` is just an unknown method.
        #[cfg(feature = "a2a")]
        m if m.starts_with("a2a.") && origin == PeerOrigin::Management => {
            let method = m.to_string(); // own it so `req` can move into the handler
            // `writer` is threaded in so the streaming handlers
            // (`a2a.SendStreamingMessage`/`a2a.SubscribeToTask`) can write their
            // intermediate `StreamResponse` frames directly to this connection.
            crate::mcp::a2a::dispatch_a2a(&method, req, ctx, writer, log)
        }
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
fn subscribe_resource(
    req: Request,
    ctx: &ServeCtx,
    origin: PeerOrigin,
    writer: &SharedWriter,
    conn: u64,
) -> Response {
    let uri = req
        .params
        .as_ref()
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match crate::agentd_uri::AgentdResource::parse(uri) {
        // agentd://inventory fires REPEATEDLY (each spawn/exit/status change) and
        // is Management-only (RFC 0015 §5.3) — reject a non-Management subscribe
        // as not-found, matching the read gate.
        Some(crate::agentd_uri::AgentdResource::Inventory) => {
            if origin != PeerOrigin::Management {
                // ACC SPEC L7 / management-profile.json `gating`: a non-Management
                // caller of an operator resource gets METHOD_NOT_FOUND (-32601),
                // uniform with the read gate — it can't even confirm it exists.
                return Response::err(
                    req.id,
                    json::METHOD_NOT_FOUND,
                    format!("method not found: {uri}"),
                );
            }
        }
        // agentd://intelligence fires its `resources/updated` on a hot-swap
        // (RFC 0018 §5, via `notify_intelligence_updated`) AND on an all-down breaker
        // ENTER/EXIT transition (RFC 0018 §6 — the served warm drain fires the notify
        // when a child's `AgentMsg::IntelHealth` flips the latched `all_down`). It is
        // Management-only — reject a non-Management subscribe as not-found, matching
        // the read gate. (Per-endpoint breaker/active transitions are NOT bridged to
        // this supervisor-side view — see `intelligence_body`'s honesty note.)
        Some(crate::agentd_uri::AgentdResource::Intelligence) => {
            if origin != PeerOrigin::Management {
                // ACC SPEC L7: non-Management → METHOD_NOT_FOUND, uniform with read.
                return Response::err(
                    req.id,
                    json::METHOD_NOT_FOUND,
                    format!("method not found: {uri}"),
                );
            }
        }
        // agentd://config/effective fires on each applied hot reload (RFC 0017
        // §5.6) and is Management-only — reject a non-Management subscribe as
        // not-found, matching the read gate. The subscription is *kept* (it fires
        // on every reload, never consumed).
        Some(crate::agentd_uri::AgentdResource::ConfigEffective) => {
            if origin != PeerOrigin::Management {
                // ACC SPEC L7: non-Management → METHOD_NOT_FOUND, uniform with read.
                return Response::err(
                    req.id,
                    json::METHOD_NOT_FOUND,
                    format!("method not found: {uri}"),
                );
            }
        }
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
        // agentd://events fires REPEATEDLY (each new event — notify-then-read, RFC
        // 0016 §7.2) and is Management-only (the operator live-tail) and only when
        // this build serves it (`events` feature). A non-mgmt origin or an
        // events-less build is rejected as not-found, matching the read gate. The
        // subscription is *kept* (the keep-variant fires it on every new event).
        Some(crate::agentd_uri::AgentdResource::Events(_)) => {
            if origin != PeerOrigin::Management {
                // ACC SPEC L7: a non-Management caller of the operator events
                // resource gets METHOD_NOT_FOUND (-32601), uniform with the read
                // gate. (An events-less build is a separate honest-absence case.)
                return Response::err(
                    req.id,
                    json::METHOD_NOT_FOUND,
                    format!("method not found: {uri}"),
                );
            }
            if !cfg!(feature = "events") {
                return Response::err(
                    req.id,
                    json::RESOURCE_NOT_FOUND,
                    format!("not a subscribable resource: {uri}"),
                );
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

/// Spawn the `agentd://events` coalescing notifier (RFC 0016 §7.2). The event
/// ring is fed from the logging layer ([`crate::obs::log`]), which knows nothing
/// of the self-MCP server; rather than wire a callback across that layer, this
/// background thread polls the "ring dirty" flag on a short tick and, when new
/// lines landed, fires ONE `notifications/resources/updated{uri:agentd://events}`
/// to subscribers — the "small coalescing batch" the RFC allows. Non-blocking and
/// best-effort: a slow/dead subscriber is pruned by its own reader loop; the ring
/// stays lossy + bounded (§8.4). Idempotent per-subscriber notify-then-read — the
/// peer drains with `?after=<seq>`.
#[cfg(feature = "events")]
fn spawn_events_notifier(subscriptions: SubRegistry) {
    thread::Builder::new()
        .name("serve-mcp-events".into())
        .spawn(move || {
            loop {
                // ~100ms coalescing window: many lines collapse to one notify.
                thread::sleep(Duration::from_millis(100));
                if crate::obs::log::take_events_dirty() {
                    notify_resource_updated_keep(&subscriptions, crate::agentd_uri::EVENTS_URI);
                }
            }
        })
        .ok();
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
fn resource_list(ctx: &ServeCtx, origin: PeerOrigin) -> Value {
    let mut list = vec![
        json!({
            "uri": "agent://status",
            "name": "status",
            "description": "This agentd's run id, mode, version, pid, uptime, and spawn counts.",
            "mimeType": "application/json"
        }),
        json!({
            // Stable + listable (RFC 0015 §3.4): a self-description manifest, readable
            // on every origin. The run id is fixed at startup, so the uri never 404s.
            "uri": "agent://capabilities",
            "name": "capabilities",
            "description": "This agentd's self-description: identity, the declared capability surface (intelligence transport, MCP servers, exec, limits, isolation), and live daemon counters.",
            "mimeType": "application/json"
        }),
        json!({
            "uri": crate::agentd_uri::run_uri(&ctx.run_id),
            "name": "run",
            "description": "This served run's aggregate: mode, root handle, status, spawn counts, and uptime. Subscribable — pushed on each spawn / terminal-run change.",
            "mimeType": "application/json"
        }),
    ];
    // agentd://inventory is operator-facing — listed (and readable) only to a
    // `Management` peer (RFC 0015 §3.4 / §5.3). A stdio-spawned subagent never
    // sees it; a stdio read of it 404s like any unknown uri (resources_read).
    if origin == PeerOrigin::Management {
        list.push(json!({
            "uri": crate::agentd_uri::INVENTORY_URI,
            "name": "inventory",
            "description": "The live subagent-tree projection: lifecycle flags (draining/paused/ready), totals, and per-node status/usage. Subscribable — pushed on each spawn / exit / status change.",
            "mimeType": "application/json"
        }));
        // agentd://intelligence — operator-facing intelligence-endpoint health
        // (RFC 0018 §4.4), Management-only. The endpoint list (transport + index),
        // which is active, and per-endpoint breaker/latency — never the URL/creds.
        list.push(json!({
            "uri": crate::agentd_uri::INTELLIGENCE_URI,
            "name": "intelligence",
            "description": "The intelligence-endpoint health view: the ordered endpoint list (transport + index, never the URL/creds), which is active, each one's breaker state / EWMA latency / error rate, and the all-down flag. Subscribable — pushed on breaker / active / all-down transitions.",
            "mimeType": "application/json"
        }));
        // agentd://config/effective — the live, redacted reloadable-config view
        // (RFC 0017 §4.2 / §5.6), Management-only. model / limits / log level /
        // subscribe set / structural server names / intelligence header NAMES —
        // never a token/URL/secret. Subscribable: pushed on each applied hot reload.
        list.push(json!({
            "uri": crate::agentd_uri::CONFIG_EFFECTIVE_URI,
            "name": "config_effective",
            "description": "The live, redacted view of the running daemon's reloadable config: model, max_tokens, limits (max_steps/max_depth/deadline), log level, the subscribe set, structural MCP-server names+tags, and intelligence header NAMES — never a token, URL, or secret value. Subscribable — pushed on each applied hot reload.",
            "mimeType": "application/json"
        }));
        // agentd://capacity — the placement view (RFC 0019 §7.2/§9), Management-only
        // and present only in `cluster` builds. Identity, shard K/N, free slots,
        // active subagents, intelligence warmth, and saturation — what agentctl
        // reads to place work. Not subscribable in this chunk (a static-ish read).
        #[cfg(feature = "cluster")]
        list.push(json!({
            "uri": crate::agentd_uri::CAPACITY_URI,
            "name": "capacity",
            "description": "The placement/capacity view: instance identity, shard K/N (null if unsharded), standby flag, free slots, active subagents, intelligence warmth/health, max_total_subagents, and saturation [0,1]. Read by agentctl to place work (RFC 0019).",
            "mimeType": "application/json"
        }));
        // agentd://events — the bounded live-event ring (RFC 0016 §7), operator-
        // facing and only when this build serves it (`events` feature). Its URI is
        // daemon-stable (the bare base), so it's safe to list; the cursor/filters
        // ride the read query, not the listed uri.
        #[cfg(feature = "events")]
        list.push(json!({
            "uri": crate::agentd_uri::EVENTS_URI,
            "name": "events",
            "description": "The live event stream: a bounded ring of the JSON log lines (RFC 0010 schema). Read agentd://events?after=<seq> to drain new lines (with the dropped count + window bounds); ?level=/?event=<prefixes> filter. Subscribable — pushed on each new event.",
            "mimeType": "application/json"
        }));
    }
    Value::Array(list)
}

/// `resources/read` over the agentd:// scheme. A known URI returns a `contents`
/// body; an unknown/missing URI is a JSON-RPC INVALID_PARAMS error.
fn resources_read(req: Request, ctx: &ServeCtx, origin: PeerOrigin) -> Response {
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
        // agentd://inventory — operator-facing, Management-only (RFC 0015 §5.3).
        // A non-Management origin is refused as resource-not-found (the same shape
        // as an unknown uri) so a stdio peer can't even confirm it exists.
        Some(crate::agentd_uri::AgentdResource::Inventory) => {
            if origin != PeerOrigin::Management {
                // ACC SPEC L7 / management-profile.json `gating`: a non-Management
                // caller of an operator resource gets METHOD_NOT_FOUND (-32601) —
                // it can't even confirm the resource exists. (Branded message kept
                // non-disclosing.)
                return Response::err(
                    req.id,
                    json::METHOD_NOT_FOUND,
                    format!("resource not found: {uri}"),
                );
            }
            Response::ok(
                req.id,
                json!({
                    "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.inventory_body().to_string()}]
                }),
            )
        }
        // agentd://intelligence — operator-facing intelligence-endpoint health
        // (RFC 0018 §4.4). Management-only (the same gate as inventory): a
        // non-Management origin is refused as resource-not-found so a stdio peer
        // can't even confirm it exists. The body is transport+index+health only —
        // never the URL or any credential (RFC 0012 §3.7).
        Some(crate::agentd_uri::AgentdResource::Intelligence) => {
            if origin != PeerOrigin::Management {
                // ACC SPEC L7 / management-profile.json `gating`: a non-Management
                // caller of an operator resource gets METHOD_NOT_FOUND (-32601) —
                // it can't even confirm the resource exists. (Branded message kept
                // non-disclosing.)
                return Response::err(
                    req.id,
                    json::METHOD_NOT_FOUND,
                    format!("resource not found: {uri}"),
                );
            }
            Response::ok(
                req.id,
                json!({
                    "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.intelligence_body().to_string()}]
                }),
            )
        }
        // agentd://capacity — the placement view (RFC 0019 §7.2/§9). Management-only
        // (the same gate as inventory/intelligence): a non-Management origin is
        // refused as resource-not-found. Present only in `cluster` builds; without
        // the feature it 404s like any unknown uri (capability-absence-not-error,
        // RFC 0015 §2.5). No secret, no URL.
        #[cfg(feature = "cluster")]
        Some(crate::agentd_uri::AgentdResource::Capacity) => {
            if origin != PeerOrigin::Management {
                // ACC SPEC L7 / management-profile.json `gating`: a non-Management
                // caller of an operator resource gets METHOD_NOT_FOUND (-32601) —
                // it can't even confirm the resource exists. (Branded message kept
                // non-disclosing.)
                return Response::err(
                    req.id,
                    json::METHOD_NOT_FOUND,
                    format!("resource not found: {uri}"),
                );
            }
            Response::ok(
                req.id,
                json!({
                    "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.capacity_body().to_string()}]
                }),
            )
        }
        // agentd://config/effective — the live, redacted reloadable-config view
        // (RFC 0017 §4.2). Management-only (the same gate as inventory/
        // intelligence): a non-Management origin is refused as resource-not-found
        // so a stdio peer can't even confirm it exists. The body is the CURRENT
        // (post-any-reload) view, with NO secret / URL (`effective_view` redacts).
        Some(crate::agentd_uri::AgentdResource::ConfigEffective) => {
            if origin != PeerOrigin::Management {
                // ACC SPEC L7 / management-profile.json `gating`: a non-Management
                // caller of an operator resource gets METHOD_NOT_FOUND (-32601) —
                // it can't even confirm the resource exists. (Branded message kept
                // non-disclosing.)
                return Response::err(
                    req.id,
                    json::METHOD_NOT_FOUND,
                    format!("resource not found: {uri}"),
                );
            }
            Response::ok(
                req.id,
                json!({
                    "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.config_effective_body().to_string()}]
                }),
            )
        }
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
        // agentd://run/<run_id> — the served run aggregate (RFC 0005 §3.3), frozen
        // to the RFC 0016 §6.2 schema once terminal (the report is folded into
        // `run_body` under `report`).
        Some(crate::agentd_uri::AgentdResource::Run(_)) => Response::ok(
            req.id,
            json!({
                "contents": [{"uri": uri, "mimeType": "application/json", "text": ctx.run_body().to_string()}]
            }),
        ),
        // agentd://events[?after=…] — the bounded live-event ring (RFC 0016 §7).
        // Management-only (the live-tail tool is an operator surface): a non-mgmt
        // origin is refused as not-found, matching the inventory gate. The cursor +
        // filters ride the query; the read returns the §7.2 envelope. Without the
        // `events` feature the resource is absent — it 404s like any unknown uri.
        Some(crate::agentd_uri::AgentdResource::Events(query)) => {
            events_read(req, &query, origin, ctx)
        }
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
        // Without the `cluster` feature the capacity resource is absent: parse may
        // still yield it (the enum variant is unconditional), so 404 it like any
        // unknown uri — capability-absence-not-error (RFC 0015 §2.5).
        #[cfg(not(feature = "cluster"))]
        Some(crate::agentd_uri::AgentdResource::Capacity) => Response::err(
            req.id,
            json::RESOURCE_NOT_FOUND,
            format!("resource not found: {uri}"),
        ),
        None => Response::err(
            req.id,
            json::RESOURCE_NOT_FOUND,
            format!("resource not found: {uri}"),
        ),
    }
}

/// `resources/read` for `agentd://events` (RFC 0016 §7). Management-only (the
/// live-tail is an operator surface): a non-mgmt origin gets METHOD_NOT_FOUND
/// (-32601) per ACC SPEC L7 — a stdio peer can't even confirm it exists, uniform
/// with the inventory/intelligence read gate. With the `events` feature it serves
/// the §7.2 envelope from the bounded ring; an installed-but-empty window is still
/// a valid read. Without the ring (no `events` feature, or never installed) it
/// 404s — capability-absence-not-error (RFC 0015 §2.5).
fn events_read(
    req: Request,
    query: &crate::agentd_uri::EventsQuery,
    origin: PeerOrigin,
    ctx: &ServeCtx,
) -> Response {
    if origin != PeerOrigin::Management {
        // ACC SPEC L7: a non-Management caller of the operator events resource gets
        // METHOD_NOT_FOUND (-32601), uniform with the other operator-resource gates.
        return Response::err(
            req.id,
            json::METHOD_NOT_FOUND,
            "method not found: agentd://events".to_string(),
        );
    }
    #[cfg(feature = "events")]
    {
        match ctx.events_body(query) {
            Some(body) => Response::ok(
                req.id,
                json!({
                    "contents": [{"uri": crate::agentd_uri::EVENTS_URI, "mimeType": "application/json", "text": body.to_string()}]
                }),
            ),
            // The feature is built but no ring is installed (the daemon did not arm
            // it) — honest absence.
            None => Response::err(
                req.id,
                json::RESOURCE_NOT_FOUND,
                "agent://events not served (no event ring installed)".to_string(),
            ),
        }
    }
    #[cfg(not(feature = "events"))]
    {
        let _ = (query, ctx);
        Response::err(
            req.id,
            json::RESOURCE_NOT_FOUND,
            "resource not found: agentd://events (built without --features events)".to_string(),
        )
    }
}

fn tools_call(req: Request, ctx: &ServeCtx, origin: PeerOrigin, log: &Logger) -> Response {
    let name = req
        .params
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // The operator tools are gated by transport origin, not an in-band flag
    // (RFC 0015 §3.4). A `Stdio` peer never reaches the operator arms — they
    // fall through to the `-32601`-style unknown-tool error below, so a spawned
    // subagent can neither see (tools/list) nor invoke (tools/call) them. Bare
    // `match` arms with a `Management` guard keep the gate structural.
    if origin == PeerOrigin::Management {
        let args = || {
            req.params
                .as_ref()
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(json!({}))
        };
        match name {
            "drain" => return handle_drain(req.id, ctx, &args(), log),
            "lame-duck" => return handle_lame_duck(req.id, ctx, &args(), log),
            "pause" => return handle_pause(req.id, ctx, log),
            "resume" => return handle_resume(req.id, ctx, log),
            "cancel" => return handle_cancel(req.id, ctx, &args(), log),
            _ => {}
        }
    }
    // A non-Management peer naming an operator tool: the tool is invisible on this
    // transport (RFC 0015 §3.4 / ACC management-profile.json `gating`, SPEC L7).
    // Reaching here with an operator-tool name implies a non-Management origin (a
    // Management peer already returned above), so refuse with METHOD_NOT_FOUND
    // (-32601) — NOT INVALID_PARAMS — so a stdio peer can't even confirm the
    // operator surface exists. A genuinely-unknown tool name still falls through to
    // the INVALID_PARAMS arm below (unchanged — scoped to the origin gate).
    if crate::capabilities::OPERATOR_TOOLS.contains(&name) {
        return Response::err(
            req.id,
            json::METHOD_NOT_FOUND,
            format!("method not found: {name}"),
        );
    }
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

/// The operator tool defs listed to a `Management` peer (RFC 0015 §4). The names
/// MIRROR `capabilities::OPERATOR_TOOLS` — the manifest's `surfaces.operator_tools`
/// and this `tools/list` set are the same const, so they cannot drift (§5.2).
fn operator_tool_defs() -> Vec<Value> {
    vec![
        drain_tool_def(),
        lame_duck_tool_def(),
        pause_tool_def(),
        resume_tool_def(),
        cancel_tool_def(),
    ]
}

/// `drain` (RFC 0015 §4.1) — trigger the SAME graceful-drain choreography
/// SIGTERM does, via the one-way `DRAINING` latch (`signals::request_drain`), and
/// return immediately with a snapshot. Idempotent/monotonic: a second `drain`
/// (or a SIGTERM after this) re-reports; it never escalates to the second-signal
/// FORCE path. `deadline_ms` is accepted and clamped to the configured drain
/// timeout (never above it, so a tool call can't push drain past the pod grace,
/// RFC 0015 §8) — the latch itself carries no deadline, so the clamp only shapes
/// the reported `eta_ms`.
fn handle_drain(id: Id, ctx: &ServeCtx, args: &Value, log: &Logger) -> Response {
    crate::signals::request_drain();
    let drain_timeout_ms = ctx.drain_timeout.as_millis() as u64;
    // An optional deadline override is clamped to the configured bound.
    let eta_ms = args
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .map_or(drain_timeout_ms, |d| d.min(drain_timeout_ms));
    let in_flight = ctx.inflight.load(Ordering::Relaxed);
    let started_at = crate::obs::log::rfc3339_millis(std::time::SystemTime::now());
    log.info(
        "mcp.drain",
        json!({"in_flight": in_flight, "eta_ms": eta_ms}),
    );
    let body = json!({
        "draining": true,
        "in_flight": in_flight,
        "eta_ms": eta_ms,
        "drain_timeout_ms": drain_timeout_ms,
        "started_at": started_at,
    });
    Response::ok(
        id,
        json!({
            "content": [{"type": "text", "text": format!("draining: {in_flight} in flight, eta {eta_ms}ms")}],
            "structuredContent": body,
            "isError": false
        }),
    )
}

/// `lame-duck` (RFC 0015 §4.2) — flip the readiness override toward NotReady
/// (`ready:false`, the default) or clear it (`ready:true`), WITHOUT draining or
/// exiting. The override only ever pushes *toward* NotReady: clearing it restores
/// the genuine computed readiness (it can't assert Ready over a not-ready
/// supervisor — here, a drain in progress still holds `/readyz` down). Reversible.
fn handle_lame_duck(id: Id, ctx: &ServeCtx, args: &Value, log: &Logger) -> Response {
    // Default false: the unqualified call lame-ducks the instance (§4.2).
    let want_ready = args.get("ready").and_then(Value::as_bool).unwrap_or(false);
    // A drain already holds readiness down and cannot be undone (one-way latch) —
    // `ready:true` then can't assert Ready, so report it honestly as a refusal
    // rather than silently flip a flag with no effect.
    if want_ready && crate::signals::draining() {
        return tool_error(
            id,
            "cannot clear lame-duck: a drain is in progress (readiness stays NotReady)".to_string(),
        );
    }
    crate::signals::set_lame_duck(!want_ready);
    let in_flight = ctx.inflight.load(Ordering::Relaxed);
    let since = crate::obs::log::rfc3339_millis(std::time::SystemTime::now());
    log.info("mcp.lame_duck", json!({"ready": want_ready}));
    let body = json!({ "ready": want_ready, "since": since, "in_flight": in_flight });
    Response::ok(
        id,
        json!({
            "content": [{"type": "text", "text": if want_ready { "readiness override cleared" } else { "lame-duck: advertising NotReady" }}],
            "structuredContent": body,
            "isError": false
        }),
    )
}

/// `pause` (RFC 0015 §4.3) — tree-wide turn-boundary suspension. Fans
/// `ctrl/pause` to every in-flight ROOT subagent (warm sessions directly; async
/// runs via their per-run pause channel, which the reactor forwards as
/// `ctrl/pause`), so each loop suspends at its next turn boundary. NOT a drain
/// and NOT a lame-duck: the tree freezes but stays intact, readiness is unchanged
/// (the supervisor reactor + liveness heartbeat keep running). Sets the
/// instance-wide pause flag (so `agentd://inventory.paused` + the `agentd_paused`
/// gauge reflect it, and runs launched while paused start paused). Returns
/// `{paused:true, affected:N}` — N = the count of subtrees that took the message.
fn handle_pause(id: Id, ctx: &ServeCtx, log: &Logger) -> Response {
    crate::signals::set_paused(true);
    crate::obs::metrics::set_paused(true);
    let affected = fan_pause(ctx, true);
    log.info("mcp.pause", json!({"affected": affected}));
    // State changed → the inventory projection's instance-level `paused` flag
    // flipped (RFC 0015 §5.3): notify a subscribed agentctl to re-read.
    notify_resource_updated_keep(&ctx.subscriptions, crate::agentd_uri::INVENTORY_URI);
    let body = json!({ "paused": true, "affected": affected });
    Response::ok(
        id,
        json!({
            "content": [{"type": "text", "text": format!("paused: {affected} subtree(s) suspending at their next turn boundary")}],
            "structuredContent": body,
            "isError": false
        }),
    )
}

/// `resume` (RFC 0015 §4.3) — clear a prior `pause`: fan `ctrl/resume` to every
/// in-flight root subagent so each loop continues at its next turn. Clears the
/// instance-wide pause flag. Returns `{paused:false, affected:N}`.
fn handle_resume(id: Id, ctx: &ServeCtx, log: &Logger) -> Response {
    crate::signals::set_paused(false);
    crate::obs::metrics::set_paused(false);
    let affected = fan_pause(ctx, false);
    log.info("mcp.resume", json!({"affected": affected}));
    notify_resource_updated_keep(&ctx.subscriptions, crate::agentd_uri::INVENTORY_URI);
    let body = json!({ "paused": false, "affected": affected });
    Response::ok(
        id,
        json!({
            "content": [{"type": "text", "text": format!("resumed: {affected} subtree(s) continuing")}],
            "structuredContent": body,
            "isError": false
        }),
    )
}

/// Fan a pause (`want=true`) or resume (`want=false`) to every in-flight root
/// subagent, mirroring `handle_cancel`'s dual fan-out (RFC 0015 §4.3), and return
/// the count of subtrees that took the message:
/// - warm sessions (`ctx.warm`): the server holds the `Subagent`, so send
///   `ctrl/pause`/`ctrl/resume` straight down its control channel (the parallel
///   of `w.sub.send(ControlMsg::Cancel)`).
/// - async runs (`ctx.sessions`): the server holds only a shared atomic the run's
///   supervisor reactor reads, so flip `s.paused` — the reactor translates the
///   edge into `ctrl/pause`/`ctrl/resume` to its live children (the parallel of
///   how `s.cancel` becomes a `ctrl/cancel`). Match the existing mechanism
///   EXACTLY: no second control path is invented.
///
/// Only live (non-terminal) subtrees are counted — a finished run can neither
/// pause nor resume, so it does not contribute to `affected`.
fn fan_pause(ctx: &ServeCtx, want: bool) -> u64 {
    let mut affected = 0u64;
    let msg = if want {
        ControlMsg::Pause
    } else {
        ControlMsg::Resume
    };
    {
        let mut warm = ctx.warm.lock().unwrap_or_else(|e| e.into_inner());
        for w in warm.values_mut() {
            if w.done {
                continue;
            }
            if w.sub.send(&msg).is_ok() {
                affected += 1;
            }
        }
    }
    {
        let reg = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for s in reg.values() {
            if s.status.is_terminal() {
                continue;
            }
            s.paused.store(want, Ordering::Relaxed);
            affected += 1;
        }
    }
    affected
}

/// Cancel every in-flight root subtree — the whole run (the ACC management-profile
/// `cancel{handle:"0"}`/omitted sentinel), mirroring [`fan_pause`]'s dual fan-out:
/// - warm sessions (`ctx.warm`): send `ControlMsg::Cancel` down the held control
///   channel, then remove the `Subagent` (its `Drop` runs the SIGKILL+reap kill
///   ladder) outside the lock — the parallel of the single-handle warm path.
/// - async runs (`ctx.sessions`): flip the shared `cancel` atomic the run's
///   supervisor reactor reads (it turns the edge into `ctrl/cancel` to live
///   children) — the parallel of the single-handle async path. No new path is
///   invented.
///
/// Only live (non-terminal) subtrees are counted into the returned `subtree_size`.
fn fan_cancel(ctx: &ServeCtx, reason: &str) -> u64 {
    let mut affected = 0u64;
    {
        let mut warm = ctx.warm.lock().unwrap_or_else(|e| e.into_inner());
        let live: Vec<String> = warm
            .iter()
            .filter(|(_, w)| !w.done)
            .map(|(h, _)| h.clone())
            .collect();
        let mut removed = Vec::new();
        for h in live {
            if let Some(w) = warm.get_mut(&h) {
                let _ = w.sub.send(&ControlMsg::Cancel {
                    reason: reason.to_string(),
                });
            }
            if let Some(r) = warm.remove(&h) {
                removed.push(r);
                affected += 1;
            }
        }
        // Release the registry lock before the Subagents' Drop (SIGKILL+reap).
        drop(warm);
        drop(removed);
    }
    {
        let reg = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for s in reg.values() {
            if s.status.is_terminal() {
                continue;
            }
            s.cancel.store(true, Ordering::Relaxed);
            affected += 1;
        }
    }
    affected
}

/// `cancel` (RFC 0015 §4.4) — the instance-scoped wrapper over the served
/// cancellation by handle (the `subagent.cancel` path). Cancels a tracked warm
/// session or async run by handle; an unknown handle is `isError:true` inside a
/// successful result (a racing reap may have already removed it — RFC 0015 §8),
/// not a protocol error. `cancel{handle:"0"}`/`"served.0"` targets the run, never
/// the supervisor — distinct from `drain`, which also exits.
fn handle_cancel(id: Id, ctx: &ServeCtx, args: &Value, log: &Logger) -> Response {
    let handle = args
        .get("handle")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // ACC management-profile.json `cancel`: `handle:"0"` OR an omitted/empty handle
    // is the sentinel-in-string for the ROOT subtree — i.e. the whole run. Cancel
    // every live served subtree (the kill ladder), mirroring pause/resume's
    // tree-wide fan-out. A real handle below still targets just that one subtree
    // (back-compat).
    if handle.is_empty() || handle == "0" {
        let affected = fan_cancel(ctx, &cancel_reason(args));
        log.info("mcp.cancel", json!({"handle": "0", "affected": affected}));
        notify_resource_updated_keep(&ctx.subscriptions, crate::agentd_uri::INVENTORY_URI);
        let body = json!({ "handle": "0", "cancelled": true, "subtree_size": affected });
        return Response::ok(
            id,
            json!({"content": [{"type": "text", "text": format!("cancelling the whole run: {affected} live subtree(s)")}], "structuredContent": body, "isError": false}),
        );
    }
    // Warm session?
    {
        let mut warm = ctx.warm.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = warm.get_mut(&handle) {
            let _ = w.sub.send(&ControlMsg::Cancel {
                reason: cancel_reason(args),
            });
            // Drop the Subagent outside the lock (its Drop SIGKILL+reaps).
            let removed = warm.remove(&handle);
            drop(warm);
            drop(removed);
            notify_resource_updated_keep(&ctx.subscriptions, crate::agentd_uri::INVENTORY_URI);
            let body = json!({ "handle": handle, "cancelled": true });
            return Response::ok(
                id,
                json!({"content": [{"type": "text", "text": format!("cancelled {handle}")}], "structuredContent": body, "isError": false}),
            );
        }
    }
    // Async run?
    let mut reg = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
    match reg.get_mut(&handle) {
        Some(s) if !s.status.is_terminal() => {
            s.cancel.store(true, Ordering::Relaxed);
            drop(reg);
            notify_resource_updated_keep(&ctx.subscriptions, crate::agentd_uri::INVENTORY_URI);
            let body = json!({ "handle": handle, "cancelled": true });
            Response::ok(
                id,
                json!({"content": [{"type": "text", "text": format!("cancel requested for {handle}; it is draining")}], "structuredContent": body, "isError": false}),
            )
        }
        Some(_) => {
            // Already finished — nothing to cancel, but not an error condition.
            let body =
                json!({ "handle": handle, "cancelled": false, "reason": "already finished" });
            Response::ok(
                id,
                json!({"content": [{"type": "text", "text": format!("{handle} already finished; nothing to cancel")}], "structuredContent": body, "isError": false}),
            )
        }
        // Unknown handle → isError result (a racing reap, §8), not a protocol error.
        None => tool_error(id, format!("no such handle: {handle}")),
    }
}

/// The cancel reason surfaced into `ctrl/cancel` + logs (RFC 0015 §4.4).
fn cancel_reason(args: &Value) -> String {
    args.get("reason")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "operator cancel".to_string())
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
    // Hot-reload quiesce guard (RFC 0017 §5.3 step 3): while a validated reload is
    // mid-apply, transiently refuse NEW spawns so they pick up the new config —
    // mirrors the `draining` guard, but the flag is cleared the instant the apply
    // finishes (step 6), so the peer's retry succeeds within milliseconds. Gated
    // on `hot-reload` (the flag is only ever set by that build's reactive loop).
    #[cfg(feature = "hot-reload")]
    if crate::signals::reloading() {
        log.warn("mcp.spawn_refused", json!({"reason": "reload_in_progress"}));
        return tool_error(
            id,
            "spawn refused: a config reload is in progress; retry shortly".to_string(),
        );
    }
    let n = ctx.counter.fetch_add(1, Ordering::Relaxed);
    // A new spawn changed the run aggregate (total_spawns / inflight) → push to any
    // `agentd://run/<run_id>` subscribers (the keep-variant: the run resource fires
    // repeatedly over the daemon's life). The tree gained a node, so the
    // `agentd://inventory` projection changed too (RFC 0015 §5.3 — emit on spawn).
    notify_resource_updated_keep(&ctx.subscriptions, &crate::agentd_uri::run_uri(&ctx.run_id));
    notify_resource_updated_keep(&ctx.subscriptions, crate::agentd_uri::INVENTORY_URI);
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
    if launch_async_run(ctx, log, &handle, payload, permit).is_err() {
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

/// Register an async run in the `sessions` registry and launch its supervising
/// background thread (holding `permit` + the per-run cancel flag), pushing the
/// usual resource notifications on completion. The shared core of `spawn_async`
/// (the `subagent.spawn{async}` tool) and the A2A `a2a.SendMessage` path, so the
/// served-run lifecycle is written once. Returns `Err(())` if the supervising
/// thread could not be spawned (the half-registered handle is rolled back first).
fn launch_async_run(
    ctx: &ServeCtx,
    log: &Logger,
    handle: &str,
    payload: SpawnPayload,
    permit: SpawnGuard,
) -> Result<(), ()> {
    let cancel = Arc::new(AtomicBool::new(false));
    // Seed the per-run pause flag from the instance-wide pause state (RFC 0015
    // §4.3): a run launched while the instance is paused starts paused, so the
    // reactor forwards `ctrl/pause` to its root as soon as it is live.
    let paused = Arc::new(AtomicBool::new(crate::signals::paused()));
    // The per-run intelligence hot-swap channel (RFC 0018 §5.2): the reload
    // fan-out publishes into it; the run's reactor reads + fans to its children.
    let swap = crate::supervisor::swap::SwapChannel::new();
    {
        let mut reg = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
        evict_if_full(&mut reg);
        reg.insert(
            handle.to_string(),
            ServedSession {
                status: ServedStatus::Running,
                cancel: Arc::clone(&cancel),
                paused: Arc::clone(&paused),
                swap: swap.clone(),
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
        handle.to_string(),
        crate::agentd_uri::run_uri(&ctx.run_id),
    );
    let spawned = thread::Builder::new()
        .name(format!("served-run:{handle}"))
        .spawn(move || {
            let _permit = permit; // held for the run's lifetime → bounds live runs
            let result = supervise_swappable(
                exe,
                &payload,
                drain,
                log2,
                Some(Arc::clone(&cancel)),
                Some(Arc::clone(&paused)),
                Some(swap),
            );
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
            // The node reached a terminal status → the inventory projection changed
            // (RFC 0015 §5.3 — emit on exit/status-change; keep — fires repeatedly).
            notify_resource_updated(&subs, &crate::agentd_uri::subagent_uri(&h));
            notify_resource_updated_keep(&subs, &run_uri);
            notify_resource_updated_keep(&subs, crate::agentd_uri::INVENTORY_URI);
        });
    if spawned.is_err() {
        ctx.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(handle);
        return Err(());
    }
    Ok(())
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

fn drain_tool_def() -> Value {
    json!({
        "name": "drain",
        "description": "Begin a graceful drain of this instance — identical to a SIGTERM: flip \
            readiness to NotReady, stop accepting new work, wind down in-flight subagents at turn \
            boundaries, then exit 0. Returns IMMEDIATELY with a snapshot (it does not block until \
            exit). Idempotent: a second call re-reports. Management transport only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "deadline_ms": {"type": "integer", "description": "optional drain budget; clamped to the configured drain timeout"}
            },
            "additionalProperties": false
        }
    })
}

fn lame_duck_tool_def() -> Value {
    json!({
        "name": "lame-duck",
        "description": "Flip /readyz to NotReady WITHOUT draining or exiting (the rolling-update \
            primitive): the instance keeps running and serving in-flight work but advertises \
            'don't send me new work'. Reversible: ready=true clears the override (readiness then \
            reflects the genuine computed state). Management transport only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "ready": {"type": "boolean", "default": false, "description": "false ⇒ NotReady (default); true ⇒ clear the override"}
            },
            "additionalProperties": false
        }
    })
}

fn pause_tool_def() -> Value {
    json!({
        "name": "pause",
        "description": "Suspend the whole agentic tree at turn boundaries (RFC 0015 §4.3): every \
            in-flight root subagent finishes its current turn, then waits. NOT a drain and NOT a \
            lame-duck — the tree freezes but stays intact, readiness is unchanged, the instance \
            keeps answering ping / serving management / bumping liveness. Reversible with resume. \
            Useful for live debugging or holding a tree while the model service is swapped. \
            Management transport only.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false
        }
    })
}

fn resume_tool_def() -> Value {
    json!({
        "name": "resume",
        "description": "Clear a prior pause (RFC 0015 §4.3): every paused root subagent continues \
            at its next turn. Management transport only.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false
        }
    })
}

fn cancel_tool_def() -> Value {
    json!({
        "name": "cancel",
        "description": "Cancel a run or subtree in THIS instance by handle (the management-transport, \
            instance-scoped wrapper over subagent.cancel) — kills the work, keeps the pod (unlike \
            drain, which also exits). handle \"0\" or OMITTED targets the root subtree (the whole \
            run): every live subtree is cancelled. An unknown handle is reported as an error result, \
            not a failure. Management transport only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "handle": {"type": "string", "description": "the run/subtree handle to cancel; \"0\" or omitted = the whole run (root subtree)"},
                "reason": {"type": "string", "description": "surfaced in logs + ctrl/cancel"}
            },
            "additionalProperties": false
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
                    ..Default::default()
                },
                McpServerSpec {
                    name: "db".into(),
                    command: vec!["b".into()],
                    tags: Vec::new(),
                    ..Default::default()
                },
            ],
            a2a_peers: Vec::new(),
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
                paused: Arc::new(AtomicBool::new(false)),
                swap: crate::supervisor::swap::SwapChannel::new(),
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
        let _g = crate::signals::test_guard();
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
            uris.contains(&"agent://status"),
            "agent://status listed: {uris:?}"
        );
        // the run aggregate is stable (daemon-lifetime uri) → safe to list.
        assert!(
            uris.contains(&"agent://run/r1"),
            "agent://run/r1 listed: {uris:?}"
        );
        // capabilities is a stable, listable self-description (RFC 0015 §3.4).
        assert!(
            uris.contains(&"agent://capabilities"),
            "agent://capabilities listed: {uris:?}"
        );
    }

    #[test]
    fn resources_read_capabilities_returns_the_manifest() {
        let r = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agent://capabilities"})),
            ),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        let entry = &v["contents"][0];
        assert_eq!(entry["uri"], "agent://capabilities");
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
                Some(json!({"uri": "agent://capabilities"})),
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
            req("resources/read", Some(json!({"uri": "agent://status"}))),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        let entry = &v["contents"][0];
        assert_eq!(entry["uri"], "agent://status");
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
    fn run_body_folds_in_the_terminal_report_and_flips_status() {
        // RFC 0016 §6.3.2: once a terminal report is published, the run aggregate
        // carries it under `report` and `status` flips to the report's terminal
        // status (vs the daemon's "running").
        let ctx = ctx();
        // Before terminal: running, no report.
        let b = ctx.run_body();
        assert_eq!(b["status"], "running");
        assert!(b.get("report").is_none());
        // Publish a frozen §6.2 report (the same shape RunReport produces).
        *ctx.report.lock().unwrap() = Some(json!({
            "report_schema": "1.0",
            "status": "completed",
            "exit_code": 0,
        }));
        let b = ctx.run_body();
        assert_eq!(b["status"], "completed");
        assert_eq!(b["report"]["report_schema"], "1.0");
        assert_eq!(b["report"]["exit_code"], 0);
    }

    #[test]
    fn events_resource_is_management_only() {
        // A Stdio peer (a spawned subagent) must not even confirm the events
        // resource exists — ACC SPEC L7: a non-Management caller of an operator
        // resource gets METHOD_NOT_FOUND (-32601), uniform with the inventory/
        // intelligence read gate. This holds with or without the `events` feature.
        let r = dispatch(
            req("resources/read", Some(json!({"uri": "agentd://events"}))),
            &ctx(),
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        assert_eq!(r.error.expect("err").code, json::METHOD_NOT_FOUND);
    }

    #[cfg(feature = "events")]
    #[test]
    fn events_read_returns_the_envelope_after_ring_install() {
        // With a ring installed, a Management read of agentd://events returns the
        // §7.2 envelope (events_schema / oldest_seq / newest_seq / dropped /
        // events). The ring is process-global, so this test owns it.
        crate::obs::log::install_event_ring(64);
        let l = log();
        l.warn("limit.exceeded", json!({"limit": "steps"}));
        let r = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://events?after=0"})),
            ),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        let body: Value = serde_json::from_str(v["contents"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["events_schema"], "1.0");
        assert!(body["events"].is_array());
        assert!(body["dropped"].is_u64());
        assert!(body["newest_seq"].is_u64());
    }

    #[cfg(feature = "events")]
    #[test]
    fn events_subscribe_is_management_only_and_kept() {
        // Management may subscribe to the live events resource; a Stdio peer is
        // rejected (ACC SPEC L7: a non-Management caller of an operator resource
        // gets METHOD_NOT_FOUND (-32601), uniform with the read gate).
        let ctx = ctx();
        crate::obs::log::install_event_ring(8);
        let ok = subscribe_resource(
            req("sub", Some(json!({"uri": "agentd://events"}))),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
        );
        assert!(ok.error.is_none(), "management subscribe accepted");
        let denied = subscribe_resource(
            req("sub", Some(json!({"uri": "agentd://events"}))),
            &ctx,
            PeerOrigin::Stdio,
            &writer(),
            1,
        );
        assert_eq!(denied.error.expect("err").code, json::METHOD_NOT_FOUND);
    }

    #[test]
    fn run_resource_is_subscribable_only_for_this_run_id() {
        let ctx = ctx();
        // the daemon's own run id is subscribable…
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://run/r1"}))),
                &ctx,
                PeerOrigin::Management,
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
                PeerOrigin::Management,
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
            PeerOrigin::Management,
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
            subscribe_resource(
                req("sub", Some(json!({"uri": uri}))),
                &ctx,
                PeerOrigin::Management,
                &w,
                7
            )
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
            paused: Arc::new(AtomicBool::new(false)),
            swap: crate::supervisor::swap::SwapChannel::new(),
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
            paused: Arc::new(AtomicBool::new(false)),
            swap: crate::supervisor::swap::SwapChannel::new(),
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
                    paused: Arc::new(AtomicBool::new(false)),
                    swap: crate::supervisor::swap::SwapChannel::new(),
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
                    paused: Arc::new(AtomicBool::new(false)),
                    swap: crate::supervisor::swap::SwapChannel::new(),
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
            subscribe_resource(
                req("sub", Some(json!({"uri": uri}))),
                &ctx,
                PeerOrigin::Management,
                &w,
                7
            )
            .error
            .is_none()
        );
        // dedup: a second subscribe from the same conn doesn't double-register.
        subscribe_resource(
            req("sub", Some(json!({"uri": uri}))),
            &ctx,
            PeerOrigin::Management,
            &w,
            7,
        );
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
        subscribe_resource(
            req("sub", Some(json!({"uri": uri}))),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            3,
        );
        subscribe_resource(
            req("sub", Some(json!({"uri": uri}))),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            4,
        );
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
            let r = subscribe_resource(
                req("sub", Some(json!({"uri": uri}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
            );
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
                paused: Arc::new(AtomicBool::new(false)),
                swap: crate::supervisor::swap::SwapChannel::new(),
                started: Instant::now(),
            },
        );
        let r = subscribe_resource(
            req("sub", Some(json!({"uri": "agentd://subagent/served.5"}))),
            &ctx,
            PeerOrigin::Management,
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

    // ── RFC 0015 chunk 3: the operator surface (drain / lame-duck / cancel,
    //    agentd://inventory, and the PeerOrigin gate). ──────────────────────────

    fn tool_names(r: &Response) -> Vec<String> {
        r.result
            .as_ref()
            .and_then(|v| v["tools"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn management_peer_sees_operator_tools_stdio_does_not() {
        let mgmt = dispatch(
            req("tools/list", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let names = tool_names(&mgmt);
        for t in ["drain", "lame-duck", "pause", "resume", "cancel"] {
            assert!(
                names.contains(&t.to_string()),
                "management sees {t}: {names:?}"
            );
        }
        // The operator-tool set matches the manifest's authoritative list (§5.2).
        for t in crate::capabilities::OPERATOR_TOOLS {
            assert!(names.contains(&t.to_string()), "manifest tool {t} listed");
        }
        // pause/resume are PRESENT now (RFC 0015 §4.3 — shipped).
        assert!(names.contains(&"pause".to_string()), "pause is listed");
        assert!(names.contains(&"resume".to_string()), "resume is listed");

        let stdio = dispatch(
            req("tools/list", None),
            &ctx(),
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        let names = tool_names(&stdio);
        // No operator tool — INCLUDING pause/resume — is visible to a stdio peer
        // (a spawned subagent must not pause/drain its supervisor, RFC 0015 §3.4).
        for t in ["drain", "lame-duck", "pause", "resume", "cancel"] {
            assert!(
                !names.contains(&t.to_string()),
                "stdio must NOT see {t}: {names:?}"
            );
        }
        // …but the work tools are still there for a stdio peer.
        assert!(names.contains(&"status".to_string()));
        assert!(names.contains(&"subagent.spawn".to_string()));
    }

    #[test]
    fn stdio_call_of_an_operator_tool_is_refused() {
        // A stdio peer can't even call drain — it falls through to unknown-tool
        // (JSON-RPC error), so it can neither see nor invoke the operator surface.
        // (DRAINING is a one-way process-global latch another test may have set;
        // assert the refused call doesn't CHANGE it rather than asserting absolute.)
        let before = crate::signals::draining();
        let r = dispatch(
            req("tools/call", Some(json!({"name": "drain"}))),
            &ctx(),
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        // ACC SPEC L7 / management-profile.json gating: a non-Management caller of
        // an operator tool gets METHOD_NOT_FOUND (-32601), not INVALID_PARAMS.
        assert_eq!(
            r.error.as_ref().expect("stdio drain → JSON-RPC error").code,
            json::METHOD_NOT_FOUND,
            "stdio drain → -32601 METHOD_NOT_FOUND"
        );
        assert_eq!(
            crate::signals::draining(),
            before,
            "a refused stdio drain must not change the latch"
        );
        // lame-duck and cancel are equally invisible to stdio — same -32601 gate.
        for tool in ["lame-duck", "cancel"] {
            let r = dispatch(
                req(
                    "tools/call",
                    Some(json!({"name": tool, "arguments": {"handle": "0"}})),
                ),
                &ctx(),
                PeerOrigin::Stdio,
                &writer(),
                0,
                &log(),
            );
            assert_eq!(
                r.error.as_ref().expect("stdio op tool → error").code,
                json::METHOD_NOT_FOUND,
                "stdio {tool} → -32601 METHOD_NOT_FOUND"
            );
        }
    }

    #[test]
    fn drain_latches_draining_and_returns_a_snapshot_idempotently() {
        let _g = crate::signals::test_guard();
        let ctx = ctx();
        let call = || {
            dispatch(
                req("tools/call", Some(json!({"name": "drain"}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            )
        };
        let v = call().result.expect("drain ok");
        assert_eq!(v["isError"], false);
        let body = &v["structuredContent"];
        assert_eq!(body["draining"], true);
        assert!(body["in_flight"].is_u64());
        assert!(body["eta_ms"].is_u64());
        assert!(body["drain_timeout_ms"].is_u64());
        assert!(body["started_at"].is_string());
        // The SAME one-way latch SIGTERM sets is now on.
        assert!(
            crate::signals::draining(),
            "drain set the global DRAINING latch"
        );
        // Idempotent/monotonic: a second drain re-reports, never escalates to FORCE.
        let v2 = call().result.expect("second drain ok");
        assert_eq!(v2["structuredContent"]["draining"], true);
        assert!(
            !crate::signals::force(),
            "drain never maps to the FORCE path"
        );

        // deadline_ms is clamped to the configured drain timeout (5s here).
        let clamped = dispatch(
            req(
                "tools/call",
                Some(json!({"name": "drain", "arguments": {"deadline_ms": 999_999}})),
            ),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        )
        .result
        .expect("ok");
        assert_eq!(clamped["structuredContent"]["eta_ms"], json!(5000));
    }

    #[test]
    fn lame_duck_flips_the_readiness_override_and_clears() {
        let _g = crate::signals::test_guard();
        let ctx = ctx();
        crate::signals::set_lame_duck(false); // clean baseline (process-global)
        let on = dispatch(
            req("tools/call", Some(json!({"name": "lame-duck"}))),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        )
        .result
        .expect("ok");
        assert_eq!(on["isError"], false);
        assert_eq!(on["structuredContent"]["ready"], false);
        assert!(crate::signals::lame_duck(), "lame-duck override set");
        // ready:true clears the override — but only when no drain holds readiness
        // down. (DRAINING is a one-way process-global latch another test may have
        // set; guard so this stays order-independent.)
        let off = dispatch(
            req(
                "tools/call",
                Some(json!({"name": "lame-duck", "arguments": {"ready": true}})),
            ),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        )
        .result
        .expect("ok");
        if crate::signals::draining() {
            // §4.2: can't assert Ready over a draining supervisor → isError refusal.
            assert_eq!(off["isError"], true);
        } else {
            assert_eq!(off["structuredContent"]["ready"], true);
            assert!(!crate::signals::lame_duck(), "override cleared");
        }
    }

    #[test]
    fn pause_resume_fans_to_async_sessions_and_sets_the_flag() {
        let _g = crate::signals::test_guard();
        let ctx = ctx();
        crate::signals::set_paused(false); // clean baseline (process-global)
        insert_running(&ctx, "served.0");
        insert_running(&ctx, "served.1");
        // Grab one session's pause Arc to prove the fan-out actually flips it.
        let one = Arc::clone(&ctx.sessions.lock().unwrap().get("served.0").unwrap().paused);

        let on = dispatch(
            req("tools/call", Some(json!({"name": "pause"}))),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        )
        .result
        .expect("ok");
        assert_eq!(on["isError"], false);
        assert_eq!(on["structuredContent"]["paused"], true);
        // Two live async subtrees took the message.
        assert_eq!(on["structuredContent"]["affected"], 2);
        assert!(crate::signals::paused(), "instance-wide pause flag set");
        assert!(one.load(Ordering::Relaxed), "session pause channel flipped");

        // Inventory now reflects the pause (instance flag + per live node).
        let inv = ctx.inventory_body();
        assert_eq!(inv["paused"], true);
        assert!(
            inv["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|n| n["paused"] == true),
            "every live node mirrors the tree pause: {inv}"
        );

        let off = dispatch(
            req("tools/call", Some(json!({"name": "resume"}))),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        )
        .result
        .expect("ok");
        assert_eq!(off["structuredContent"]["paused"], false);
        assert_eq!(off["structuredContent"]["affected"], 2);
        assert!(!crate::signals::paused(), "instance-wide pause cleared");
        assert!(
            !one.load(Ordering::Relaxed),
            "session pause channel cleared"
        );
    }

    #[test]
    fn fan_swap_intel_publishes_into_each_live_async_run() {
        // RFC 0018 §5.2: the reload-driven swap fan-out reaches every LIVE served
        // async run by PUBLISHING into its per-run `SwapChannel` (the parallel of
        // flipping `paused`). The run's reactor reads + fans `ctrl/swap_intel` to
        // its children. A terminal run is skipped (it can't swap).
        let ctx = ctx();
        insert_running(&ctx, "served.0");
        insert_running(&ctx, "served.1");
        // A terminal run that must NOT be reached.
        ctx.sessions.lock().unwrap().insert(
            "served.done".into(),
            ServedSession {
                status: ServedStatus::Cancelled,
                cancel: Arc::new(AtomicBool::new(false)),
                paused: Arc::new(AtomicBool::new(false)),
                swap: crate::supervisor::swap::SwapChannel::new(),
                started: Instant::now(),
            },
        );
        // Grab one live run's channel to prove the swap actually lands in it.
        let ch = ctx
            .sessions
            .lock()
            .unwrap()
            .get("served.0")
            .unwrap()
            .swap
            .clone();

        let swap = SwapIntel {
            uri: "vsock:7:9000".into(),
            token: Some("rotated".into()),
            model: Some("claude-haiku-4".into()),
            policy: crate::config::SwapPolicy::FinishOnOld,
        };
        let reached = ctx.live_config().fan_swap_intel(swap);
        // Two live async subtrees took it; the terminal one did not.
        assert_eq!(reached, 2);
        let (got, _gen) = ch.take_newer(0).expect("the swap landed in the live run");
        assert_eq!(got.model.as_deref(), Some("claude-haiku-4"));
        assert_eq!(got.uri, "vsock:7:9000");
    }

    #[test]
    fn pause_is_not_drain_or_lame_duck_readiness_unchanged() {
        let _g = crate::signals::test_guard();
        // §4.3: a paused instance is NOT lame-duck/draining — readiness reflects
        // only those, never pause.
        let ctx = ctx();
        crate::signals::set_paused(false);
        crate::signals::set_lame_duck(false);
        let was_draining = crate::signals::draining();
        let before = ctx.inventory_body();
        let ready_before = before["ready"].as_bool().unwrap();
        dispatch(
            req("tools/call", Some(json!({"name": "pause"}))),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        )
        .result
        .expect("ok");
        let after = ctx.inventory_body();
        assert_eq!(after["paused"], true);
        // ready is computed from lame_duck/draining ONLY — pause must not move it.
        assert_eq!(
            after["ready"].as_bool().unwrap(),
            ready_before,
            "pause must not change readiness"
        );
        // And readiness still equals the genuine drain/lame-duck computation.
        assert_eq!(
            after["ready"].as_bool().unwrap(),
            !crate::signals::lame_duck() && !was_draining && !crate::signals::draining()
        );
        crate::signals::set_paused(false); // restore
    }

    #[test]
    fn cancel_unknown_handle_is_an_iserror_result_not_a_protocol_error() {
        let r = dispatch(
            req(
                "tools/call",
                Some(json!({"name": "cancel", "arguments": {"handle": "0.2.9"}})),
            ),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("cancel returns a result, not an error");
        assert_eq!(v["isError"], true);
        assert!(
            v["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no such handle"),
            "{v}"
        );
    }

    #[test]
    fn cancel_of_a_running_async_run_requests_cancel() {
        let ctx = ctx();
        insert_running(&ctx, "served.7");
        let r = dispatch(
            req(
                "tools/call",
                Some(json!({"name": "cancel", "arguments": {"handle": "served.7"}})),
            ),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["cancelled"], true);
        // The run's cancel flag is now set.
        assert!(
            ctx.sessions.lock().unwrap()["served.7"]
                .cancel
                .load(Ordering::Relaxed)
        );
    }

    #[test]
    fn cancel_handle_zero_or_omitted_cancels_the_whole_run() {
        // ACC management-profile.json: cancel{handle:"0"} (or an omitted handle) is
        // the root-subtree sentinel — it cancels EVERY live served subtree, not one
        // handle. Distinct from drain (which also exits the pod).
        let ctx_a = ctx();
        insert_running(&ctx_a, "served.1");
        insert_running(&ctx_a, "served.2");
        let r = dispatch(
            req(
                "tools/call",
                Some(json!({"name": "cancel", "arguments": {"handle": "0"}})),
            ),
            &ctx_a,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok");
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["handle"], "0");
        assert_eq!(v["structuredContent"]["subtree_size"], 2);
        for h in ["served.1", "served.2"] {
            assert!(
                ctx_a.sessions.lock().unwrap()[h]
                    .cancel
                    .load(Ordering::Relaxed),
                "{h} cancel flag set by the whole-run cancel"
            );
        }

        // An OMITTED handle is the same sentinel (defaults to the whole run).
        let ctx2 = ctx();
        insert_running(&ctx2, "served.3");
        let r = dispatch(
            req(
                "tools/call",
                Some(json!({"name": "cancel", "arguments": {}})),
            ),
            &ctx2,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("ok (omitted handle == whole run)");
        assert_eq!(v["isError"], false);
        assert_eq!(v["structuredContent"]["subtree_size"], 1);
        assert!(
            ctx2.sessions.lock().unwrap()["served.3"]
                .cancel
                .load(Ordering::Relaxed)
        );
    }

    #[test]
    fn inventory_read_is_management_gated_and_projects_the_lifecycle_flags() {
        let _g = crate::signals::test_guard();
        let ctx = ctx();
        crate::signals::set_lame_duck(false);
        insert_running(&ctx, "served.0");
        // Management read → the projection with lifecycle flags + the node.
        let r = dispatch(
            req("resources/read", Some(json!({"uri": "agentd://inventory"}))),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("inventory readable for management");
        let body: Value = serde_json::from_str(v["contents"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["run_id"], "r1");
        assert_eq!(body["mode"], "reactive");
        // The instance-level lifecycle flags (§5.3). `paused` reflects the
        // instance-wide pause flag (RFC 0015 §4.3) — a process-global another test
        // may have toggled, so assert the shape, not an absolute (like
        // draining/ready). `ready` reflects the lame-duck override.
        assert!(body["paused"].is_boolean());
        assert!(body["draining"].is_boolean());
        assert!(body["ready"].is_boolean());
        assert!(body["totals"]["total_spawned"].is_u64());
        // The running async node is projected with a non-new status.
        let nodes = body["nodes"].as_array().unwrap();
        assert!(
            nodes
                .iter()
                .any(|n| n["handle"] == "served.0" && n["status"] == "running"),
            "running node projected: {nodes:?}"
        );

        // A stdio peer must NOT be able to read it — 404, as if it didn't exist.
        let denied = dispatch(
            req("resources/read", Some(json!({"uri": "agentd://inventory"}))),
            &ctx,
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        // ACC SPEC L7: a non-Management caller of an operator resource gets
        // METHOD_NOT_FOUND (-32601) — refused as if the resource did not exist.
        assert_eq!(
            denied
                .error
                .as_ref()
                .expect("stdio inventory read is refused")
                .code,
            json::METHOD_NOT_FOUND,
            "stdio inventory read → -32601 METHOD_NOT_FOUND"
        );
    }

    #[test]
    fn inventory_is_listed_only_for_management() {
        let mgmt = dispatch(
            req("resources/list", None),
            &ctx(),
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let mgmt_uris: Vec<String> = mgmt.result.unwrap()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["uri"].as_str().map(str::to_string))
            .collect();
        assert!(mgmt_uris.contains(&"agent://inventory".to_string()));

        let stdio = dispatch(
            req("resources/list", None),
            &ctx(),
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        let stdio_uris: Vec<String> = stdio.result.unwrap()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["uri"].as_str().map(str::to_string))
            .collect();
        assert!(
            !stdio_uris.contains(&"agent://inventory".to_string()),
            "inventory not listed to stdio: {stdio_uris:?}"
        );
        // capabilities stays visible on stdio (it's harmless self-description).
        assert!(stdio_uris.contains(&"agent://capabilities".to_string()));
    }

    #[test]
    fn inventory_is_subscribable_for_management_only() {
        let ctx = ctx();
        // Management can subscribe (it fires repeatedly — keep-variant).
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://inventory"}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
            )
            .error
            .is_none()
        );
        // A stdio peer is refused.
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://inventory"}))),
                &ctx,
                PeerOrigin::Stdio,
                &writer(),
                1,
            )
            .error
            .is_some()
        );
    }

    /// A ServeCtx whose config carries a multi-endpoint intelligence list, so
    /// the `agentd://intelligence` body has >1 endpoint to project.
    fn ctx_multi_endpoint() -> ServeCtx {
        let cfg = crate::config::Config {
            run_id: "r1".into(),
            mode: crate::config::Mode::Reactive,
            intelligence: Some("vsock:3:8080,unix:/run/intel.sock".into()),
            intelligence_token: Some("super-secret-tok".into()),
            model: Some("claude-opus-4".into()),
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

    #[test]
    fn intelligence_read_is_management_gated_and_carries_no_url_or_token() {
        let _g = crate::signals::test_guard(); // `all_down` reads the process-global latch
        let ctx = ctx_multi_endpoint();
        // Management read → endpoints + health, schema per RFC 0018 §4.4.
        let r = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://intelligence"})),
            ),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("intelligence readable for management");
        let text = v["contents"][0]["text"].as_str().unwrap();
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["active"], 0);
        // RFC 0018 §6: `all_down` is the LATCHED truth (false by default here); the
        // per-endpoint live health is NOT bridged supervisor-side → honest markers.
        assert_eq!(body["all_down"], false);
        assert_eq!(body["health_aggregated"], false);
        assert_eq!(body["all_down_source"], "latched_child_report");
        assert_eq!(body["model"], "claude-opus-4");
        let eps = body["endpoints"].as_array().unwrap();
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0]["index"], 0);
        assert_eq!(eps[0]["transport"], "vsock");
        assert_eq!(eps[0]["addr"], "3:8080");
        // The per-endpoint breaker state is reported `unknown` (not a fabricated
        // `closed`/0) because it genuinely isn't aggregated here (RFC 0018 §6).
        assert_eq!(eps[0]["state"], "unknown");
        assert!(eps[0].get("ewma_latency_ms").is_none());
        assert!(eps[0].get("error_rate").is_none());
        assert_eq!(eps[1]["transport"], "unix");
        // RFC 0012 §3.7: NEVER the token, NEVER a full URL with its scheme.
        assert!(!text.contains("super-secret-tok"), "token leaked: {text}");
        assert!(!text.contains("vsock:3:8080"), "full URI leaked: {text}");
        assert!(!text.contains("unix:/run"), "full URI leaked: {text}");

        // A stdio peer must NOT read it — 404, as if it didn't exist.
        let denied = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://intelligence"})),
            ),
            &ctx,
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        assert!(denied.error.is_some(), "stdio intelligence read is refused");
    }

    // --- RFC 0018 §5.4 model discovery on the served surfaces ----------------

    #[test]
    fn intelligence_body_carries_discovery_surface_and_degrades_silently() {
        // The endpoints (vsock unsupported in this build / a dead unix path) are
        // unreachable, so the lazy probe degrades silently: `discovery:false`, and
        // `models` holds only the configured model (always usable, §5.4). Never an
        // error, never fatal — the body is otherwise the normal §4.4 view.
        let ctx = ctx_multi_endpoint();
        let body = ctx.intelligence_body();
        assert_eq!(body["discovery"], json!(false), "no endpoint answered");
        assert_eq!(
            body["models"],
            json!(["claude-opus-4"]),
            "configured model is the union when none discovered"
        );
        // The rest of the §4.4 body is intact alongside the additive fields.
        assert_eq!(body["active"], 0);
        assert_eq!(body["model"], "claude-opus-4");
    }

    #[test]
    fn intelligence_body_reflects_the_latched_all_down_not_a_fresh_parse_fiction() {
        // RFC 0018 §6: the body's `all_down` must track the latched last-child
        // report, NOT a fresh `EndpointList::parse` (whose breakers are always
        // CLOSED, so `all_down` would be structurally always-false). Latch true →
        // the body reports true; clear → false.
        let _g = crate::signals::test_guard();
        let ctx = ctx_multi_endpoint();
        assert_eq!(
            ctx.intelligence_body()["all_down"],
            json!(false),
            "default latch is not-all-down"
        );
        crate::signals::set_intel_all_down(true);
        assert_eq!(
            ctx.intelligence_body()["all_down"],
            json!(true),
            "a latched child all-down report surfaces (not the fresh-parse false)"
        );
        crate::signals::set_intel_all_down(false);
        assert_eq!(ctx.intelligence_body()["all_down"], json!(false));
    }

    #[cfg(feature = "cluster")]
    #[test]
    fn capacity_body_intelligence_warmth_reflects_the_latched_all_down() {
        // RFC 0018 §6: `intelligence.warm`/`healthy` derive from the latched truth,
        // not a fresh parse (which made `healthy` always true regardless of a down
        // endpoint). A valid config + not-all-down ⇒ warm/healthy; all-down ⇒ cold.
        let _g = crate::signals::test_guard();
        let ctx = ctx_multi_endpoint();
        let body = ctx.capacity_body();
        assert_eq!(body["intelligence"]["warm"], json!(true));
        assert_eq!(body["intelligence"]["healthy"], json!(true));
        crate::signals::set_intel_all_down(true);
        let body = ctx.capacity_body();
        assert_eq!(
            body["intelligence"]["warm"],
            json!(false),
            "a latched all-down makes the pod cold (don't route model work here)"
        );
        assert_eq!(body["intelligence"]["healthy"], json!(false));
    }

    #[test]
    fn discovery_probe_is_cached_within_ttl() {
        // Two reads in quick succession must reuse the cached probe (one probe,
        // not two) — the cache is populated on the FIRST read, reused under the
        // TTL. We assert identity of the result rather than timing.
        let ctx = ctx_multi_endpoint();
        let first = ctx.intelligence_body();
        let second = ctx.intelligence_body();
        assert_eq!(first["discovery"], second["discovery"]);
        assert_eq!(first["models"], second["models"]);
        // The cache is populated (the first read primed it).
        let cache = ctx.discovery_cache.lock().unwrap();
        assert!(
            cache.is_some(),
            "first served read primed the discovery cache"
        );
    }

    #[test]
    fn capabilities_intelligence_carries_discovery_on_the_served_read() {
        // The LIVE served `agentd://capabilities` overlays the (degraded) probe
        // onto `intelligence.models` — additive, RFC 0018 §5.4.
        let ctx = ctx_multi_endpoint();
        let body = ctx.capabilities_body();
        assert_eq!(body["intelligence"]["discovery"], json!(false));
        assert_eq!(body["intelligence"]["models"], json!(["claude-opus-4"]));
        // The structural fields the manifest already carried are untouched.
        assert_eq!(body["intelligence"]["endpoints"], json!(2));
        assert_eq!(body["intelligence"]["transport"], json!("vsock"));
    }

    #[test]
    fn one_shot_capabilities_is_network_free_and_does_not_probe() {
        // RFC 0015 §5.2: the one-shot `--capabilities` manifest (`live=false`) is
        // side-effect-free admission — it must NOT probe the network. The
        // discovery field is the network-free baseline: `discovery:false`, `models`
        // is the configured model only, and the cache stays EMPTY (no probe ran).
        let ctx = ctx_multi_endpoint();
        let identity = crate::identity::Identity::from_env("r1");
        let manifest = crate::capabilities::manifest(&ctx.config, &identity, false);
        assert_eq!(manifest["intelligence"]["discovery"], json!(false));
        assert_eq!(manifest["intelligence"]["models"], json!(["claude-opus-4"]));
        // The one-shot path never touched the supervisor discovery cache.
        let cache = ctx.discovery_cache.lock().unwrap();
        assert!(
            cache.is_none(),
            "the one-shot --capabilities manifest must not probe (cache empty)"
        );
    }

    #[test]
    fn intelligence_is_listed_and_subscribable_for_management_only() {
        let ctx = ctx_multi_endpoint();
        // listed to management, not to stdio
        let mgmt = dispatch(
            req("resources/list", None),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let uris: Vec<String> = mgmt.result.unwrap()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["uri"].as_str().map(str::to_string))
            .collect();
        assert!(uris.contains(&"agent://intelligence".to_string()));

        let stdio = dispatch(
            req("resources/list", None),
            &ctx,
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        let stdio_uris: Vec<String> = stdio.result.unwrap()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["uri"].as_str().map(str::to_string))
            .collect();
        assert!(!stdio_uris.contains(&"agent://intelligence".to_string()));

        // subscribable for management, refused for stdio
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://intelligence"}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
            )
            .error
            .is_none()
        );
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://intelligence"}))),
                &ctx,
                PeerOrigin::Stdio,
                &writer(),
                1,
            )
            .error
            .is_some()
        );
    }

    #[test]
    fn config_effective_read_is_management_gated_and_reflects_a_reload() {
        // A ctx whose live config has a redaction-worthy header + a model, so we can
        // assert both the redaction and that a read reflects a swapped (reloaded)
        // config (RFC 0017 §4.2 / §5.6).
        let mut cfg = crate::config::Config {
            run_id: "r1".into(),
            mode: crate::config::Mode::Reactive,
            intelligence: Some("unix:/x".into()),
            model: Some("claude-opus-4".into()),
            ..crate::config::Config::default()
        };
        cfg.intelligence_headers
            .insert("x-api-key".into(), "{{secret:SOME_NAME}}".into());
        let ctx = ServeCtx::new(
            "r1".into(),
            "reactive".into(),
            "agentd".into(),
            base(),
            Duration::from_secs(5),
            Arc::new(cfg),
        );

        // Management read → the redacted reloadable view (RFC 0017 §4.2).
        let r = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://config/effective"})),
            ),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let v = r.result.expect("config/effective readable for management");
        let text = v["contents"][0]["text"].as_str().unwrap();
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["model"], "claude-opus-4");
        // Header NAMES only — the {{secret:…}} ref value is NEVER exposed.
        assert_eq!(body["intelligence_headers"], json!(["x-api-key"]));
        assert!(
            !text.contains("SOME_NAME"),
            "header ref value leaked: {text}"
        );

        // After a hot reload swaps the live config, a fresh read reflects it.
        let reloaded = crate::config::Config {
            run_id: "r1".into(),
            mode: crate::config::Mode::Reactive,
            intelligence: Some("unix:/x".into()),
            model: Some("claude-sonnet-9".into()),
            ..crate::config::Config::default()
        };
        ctx.live_config().swap(Arc::new(reloaded));
        let r2 = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://config/effective"})),
            ),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let text2 = r2.result.unwrap()["contents"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let body2: Value = serde_json::from_str(&text2).unwrap();
        assert_eq!(
            body2["model"], "claude-sonnet-9",
            "read reflects the post-reload config"
        );

        // A stdio peer must NOT read it — 404, as if it didn't exist.
        let denied = dispatch(
            req(
                "resources/read",
                Some(json!({"uri": "agentd://config/effective"})),
            ),
            &ctx,
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        assert!(
            denied.error.is_some(),
            "stdio config/effective read is refused"
        );
    }

    #[test]
    fn config_effective_is_listed_and_subscribable_for_management_only() {
        let ctx = ctx();
        // listed to management, not to stdio
        let mgmt = dispatch(
            req("resources/list", None),
            &ctx,
            PeerOrigin::Management,
            &writer(),
            0,
            &log(),
        );
        let uris: Vec<String> = mgmt.result.unwrap()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["uri"].as_str().map(str::to_string))
            .collect();
        assert!(uris.contains(&"agent://config/effective".to_string()));

        let stdio = dispatch(
            req("resources/list", None),
            &ctx,
            PeerOrigin::Stdio,
            &writer(),
            0,
            &log(),
        );
        let stdio_uris: Vec<String> = stdio.result.unwrap()["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["uri"].as_str().map(str::to_string))
            .collect();
        assert!(!stdio_uris.contains(&"agent://config/effective".to_string()));

        // subscribable for management, refused for stdio
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://config/effective"}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
            )
            .error
            .is_none()
        );
        assert!(
            subscribe_resource(
                req("sub", Some(json!({"uri": "agentd://config/effective"}))),
                &ctx,
                PeerOrigin::Stdio,
                &writer(),
                1,
            )
            .error
            .is_some()
        );
    }

    // ── RFC 0020: the A2A external-agent surface over the management transport. ──
    // The TerminalStatus→TaskState mapping + the A2A Task schema are unit-tested in
    // mcp::a2a; these drive the surface end-to-end through `dispatch` (routing,
    // Management gating, the served-run reuse seams). [feature: a2a]
    #[cfg(feature = "a2a")]
    mod a2a_surface {
        use super::*;

        /// Seed a terminal `Done` session so `a2a.GetTask`/`ListTasks` can read it
        /// without forking a real subagent.
        fn insert_done(ctx: &ServeCtx, handle: &str, status: &str, result: Value) {
            ctx.sessions.lock().unwrap().insert(
                handle.to_string(),
                ServedSession {
                    status: ServedStatus::Done {
                        status: status.into(),
                        partial: false,
                        result,
                    },
                    cancel: Arc::new(AtomicBool::new(false)),
                    paused: Arc::new(AtomicBool::new(false)),
                    swap: crate::supervisor::swap::SwapChannel::new(),
                    started: Instant::now(),
                },
            );
        }

        #[test]
        fn send_message_with_a_text_part_returns_a_working_task() {
            // returnImmediately defaults true → an async Task with an id + a
            // non-terminal (WORKING) state. (This DOES launch a background run; the
            // mock-llm/mock-mcp child fails fast under the test config, but the
            // SendMessage reply is synchronous + independent of the run outcome.)
            let r = dispatch(
                req(
                    "a2a.SendMessage",
                    Some(json!({
                        "message": {"messageId": "m1", "role": "ROLE_USER",
                                    "parts": [{"text": "summarize the doc"}]}
                    })),
                ),
                &ctx(),
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            let task = r.result.expect("SendMessage → a Task");
            assert!(
                task["id"].as_str().unwrap().starts_with("served."),
                "task id is the served handle: {task}"
            );
            assert_eq!(task["status"]["state"], "TASK_STATE_WORKING");
            assert!(task["contextId"].as_str().is_some(), "contextId minted");
        }

        #[test]
        fn send_message_without_text_parts_is_invalid_params() {
            // No spawn happens — a malformed message is a JSON-RPC error.
            let r = dispatch(
                req(
                    "a2a.SendMessage",
                    Some(json!({"message": {"messageId": "m", "role": "ROLE_USER", "parts": []}})),
                ),
                &ctx(),
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            assert_eq!(r.error.expect("error").code, json::INVALID_PARAMS);
        }

        #[test]
        fn get_task_on_a_finished_run_maps_state_and_attaches_the_distillate() {
            let ctx = ctx();
            insert_done(
                &ctx,
                "served.42",
                "completed",
                json!("the distilled answer"),
            );
            let r = dispatch(
                req("a2a.GetTask", Some(json!({"id": "served.42"}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            let task = r.result.expect("GetTask → a Task");
            assert_eq!(task["id"], "served.42");
            assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
            // The distillate is the single terminal artifact (distillate-only).
            assert_eq!(
                task["artifacts"][0]["parts"][0]["text"],
                "the distilled answer"
            );

            // A budget-exhausted terminal run maps to FAILED with NO artifact.
            insert_done(&ctx, "served.43", "exhausted_steps", json!("partial junk"));
            let r = dispatch(
                req("a2a.GetTask", Some(json!({"id": "served.43"}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            let task = r.result.expect("ok");
            assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
            assert!(
                task.get("artifacts").is_none(),
                "no partial-artifact leakage on a failed task"
            );
        }

        #[test]
        fn get_task_unknown_id_is_task_not_found() {
            let r = dispatch(
                req("a2a.GetTask", Some(json!({"id": "served.404"}))),
                &ctx(),
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            assert_eq!(
                r.error.expect("error").code,
                crate::mcp::a2a::TASK_NOT_FOUND
            );
        }

        #[test]
        fn cancel_task_of_a_running_run_returns_canceled_and_sets_the_flag() {
            let ctx = ctx();
            insert_running(&ctx, "served.7");
            let r = dispatch(
                req("a2a.CancelTask", Some(json!({"id": "served.7"}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            let task = r.result.expect("CancelTask → a Task");
            assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED");
            assert!(
                ctx.sessions.lock().unwrap()["served.7"]
                    .cancel
                    .load(Ordering::Relaxed),
                "the run's cancel flag is now set"
            );
        }

        #[test]
        fn cancel_task_unknown_id_is_task_not_found() {
            let r = dispatch(
                req("a2a.CancelTask", Some(json!({"id": "served.404"}))),
                &ctx(),
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            assert_eq!(
                r.error.expect("error").code,
                crate::mcp::a2a::TASK_NOT_FOUND
            );
        }

        #[test]
        fn list_tasks_returns_the_tasks_array() {
            let ctx = ctx();
            insert_running(&ctx, "served.0");
            insert_done(&ctx, "served.1", "completed", json!("done"));
            let r = dispatch(
                req("a2a.ListTasks", Some(json!({}))),
                &ctx,
                PeerOrigin::Management,
                &writer(),
                0,
                &log(),
            );
            let tasks = r.result.expect("ok")["tasks"].clone();
            let arr = tasks.as_array().expect("tasks is an array");
            assert_eq!(arr.len(), 2);
            let ids: Vec<&str> = arr.iter().filter_map(|t| t["id"].as_str()).collect();
            assert!(
                ids.contains(&"served.0") && ids.contains(&"served.1"),
                "{ids:?}"
            );
            // The running one is WORKING; the completed one carries its distillate.
            for t in arr {
                match t["id"].as_str().unwrap() {
                    "served.0" => assert_eq!(t["status"]["state"], "TASK_STATE_WORKING"),
                    "served.1" => {
                        assert_eq!(t["status"]["state"], "TASK_STATE_COMPLETED");
                        assert_eq!(t["artifacts"][0]["parts"][0]["text"], "done");
                    }
                    other => panic!("unexpected task {other}"),
                }
            }
        }

        #[test]
        fn send_streaming_message_dispatches_a_stream_then_final_frame() {
            use std::io::{BufRead, BufReader};
            // Through the full `dispatch` path at a Management origin: the streaming
            // handler writes the WORKING frame to the connection writer and RETURNS
            // the terminal frame (the run fails fast under the test config → FAILED).
            let ctx = ctx();
            let (a, b) = UnixStream::pair().unwrap();
            let w: SharedWriter = Arc::new(Mutex::new(ServeStream::Unix(a)));
            let r = dispatch(
                req(
                    "a2a.SendStreamingMessage",
                    Some(json!({"message": {"parts": [{"text": "go"}]}})),
                ),
                &ctx,
                PeerOrigin::Management,
                &w,
                0,
                &log(),
            );
            // The returned frame is the FINAL statusUpdate (terminal, final:true).
            let final_sr = r.result.expect("final frame");
            assert_eq!(final_sr["statusUpdate"]["final"], true);
            assert!(
                final_sr["statusUpdate"]["status"]["state"]
                    .as_str()
                    .unwrap()
                    .starts_with("TASK_STATE_")
            );
            // The first WRITTEN frame is the WORKING status (read off the peer end).
            drop(w); // close the writer so the reader EOFs after the buffered frame
            let mut reader = BufReader::new(b);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let f1: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                f1["result"]["statusUpdate"]["status"]["state"],
                "TASK_STATE_WORKING"
            );
            assert_eq!(f1["result"]["statusUpdate"]["final"], false);
        }

        #[test]
        fn a2a_is_management_gated_stdio_gets_method_not_found() {
            // A Stdio peer's a2a.* call falls through to the catch-all → -32601,
            // never reaching the A2A handlers (the external-agent surface is for the
            // trusted management transport only — the gateway is the PEP).
            for m in [
                "a2a.SendMessage",
                "a2a.GetTask",
                "a2a.CancelTask",
                "a2a.ListTasks",
            ] {
                let r = dispatch(
                    req(m, Some(json!({"id": "served.0"}))),
                    &ctx(),
                    PeerOrigin::Stdio,
                    &writer(),
                    0,
                    &log(),
                );
                assert_eq!(
                    r.error.expect("error").code,
                    json::METHOD_NOT_FOUND,
                    "{m} from stdio → -32601"
                );
            }
        }
    }
}
