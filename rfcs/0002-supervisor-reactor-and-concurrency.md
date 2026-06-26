# RFC 0002: Supervisor Reactor & Concurrency Model

**Status:** Accepted (shipped v1)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## Problem / Context

The supervisor is a single process that must concurrently service a handful of
heterogeneous I/O sources while owning a state machine, enforcing deadlines, and
staying *idle at near-zero cost*. It has no LLM dependency; the agentic ReAct
loop lives only in subagent child processes (RFC 0001, RFC 0009). The supervisor's
job is to multiplex:

| Source | Count | Shape | Liveness concern |
|---|---|---|---|
| MCP server connections (stdio) | ~1–8 | child stdin/stdout pipes, NDJSON JSON-RPC | server hangs mid-call; emits async `notifications/resources/updated` |
| Subagent control channels | 1–50 (bounded tree) | child stdin/stdout pipes, length-framed JSON-RPC | runaway loop, deadlock, crash |
| Intelligence connection | 1 | unix / https(tls) / vsock, request + optional SSE | slow/streaming, can stall mid-token |
| Timers | a few | interval / deadline / loop tick | — |
| OS signals | SIGTERM/SIGINT/SIGCHLD/SIGPIPE | async-signal context | must drain + reap tree |
| Served self-MCP (optional) | 0–N peer clients | unix/(vsock) listener | only with `--serve-mcp` |

Two structural facts dominate the design (per the concurrency review):

1. **Scale is small and bounded.** One agent unit and its subagent tree per
   process tree; scale by running more instances (RFC 0001). The C10k problem
   does not exist here.
2. **The hard part is liveness, not throughput.** The concurrency model is
   chosen for *robust supervision of a few things*, not *efficient juggling of
   many*. RFC 0001's hard requirement is "detect dead/stuck subprocesses,
   recover state, stay stable."

A third fact is easy to miss and load-bearing: **MCP `notifications` make every
subscribed MCP server pipe full-duplex and server-initiated.** A reader must be
*always* listening on every subscribed pipe, independent of whether a `tools/call`
is outstanding. This is what rules out the retired "one Mutex, one blocking
round-trip" MCP client and forces a real concurrency model.

The assessment (§1.2 item 2) found the supervisor was specified as a slogan
("idle at near-zero cost") with no named I/O primitive. This RFC specifies the
primitive, the reactor, the signal plumbing, the write path, and the invariant
that makes it safe against stuck pipes.

This RFC owns the **mechanism of multiplexing and the per-child supervision
record's reactor-facing fields**. It does *not* own the dead/stuck *policy*
(three-detector model, kill ladder, reaping, restart governor) — those are
RFC 0003. It does not own the MCP codec (RFC 0004), the control protocol
(RFC 0005), or routing (RFC 0008). It provides the loop those RFCs plug into.

---

## Decision

Per assessment §2.1 (binding):

> **thread-per-fd with blocking I/O + `std::sync::mpsc`. No async runtime.
> `mio`/`libc::poll` held in reserve behind a `serve-mcp` feature for the one
> high-fan-in case (many idle peer connections on the served self-MCP).**

Concretely:

- **One reader thread per long-lived readable stream.** Each MCP-server stdout,
  each subagent control-channel stdout, the intelligence connection. Each reader
  parses frames and forwards **tagged** events onto **one merged `mpsc`** into
  the single supervisor thread.
- **The supervisor owns the state machine and `recv_timeout`s the merged
  channel.** That timeout *is* the timer tick — deadlines, intervals, backoff,
  ping cadence all ride the same loop. There is no separate timer thread and no
  busy-poll.
- **Writes** go from the owning context behind a **per-pipe `Mutex<ChildStdin>`**;
  child stdin is set `O_NONBLOCK` and fronted by a **bounded outbound queue**.
  A full queue is itself a stuck signal.
- **Signal handlers flip `AtomicBool`s *and* write one byte to a self-pipe** so
  the reactor wakes promptly. `SA_RESTART` is deliberately **off** so blocked
  syscalls return `EINTR`.
- **The abandon-don't-interrupt invariant (assessment §2.1, §10 of the review):**
  *the supervisor never blocks on an untrusted source. It reaches every pipe only
  via an `mpsc` it `recv_timeout`s, and it unblocks a parked reader only by
  closing/killing the producer, never by interrupting the read.* Pipes have no
  `set_read_timeout`; this invariant is what makes thread-per-fd safe without one.

**Scale check.** Worst case ≈ (8 MCP readers + 50 subagent readers + 1
intelligence + 1 signal/self-pipe reader) ≈ **60–65 threads, ~130 fds** — three
orders of magnitude inside default Linux limits (`ulimit -n` 1024; a thread with
a small set stack costs ~tens of KiB resident; the default 8 MiB stack is
virtual, not resident). At 50 subagents the dominant cost is the 50 *child
processes*, not the 50 reader threads.

**tokio rejected** (assessment §2.1, review §2(C)): it is a stated non-goal,
pulls scores of crates, reintroduces "one stuck thing starves everything" via the
shared work-stealing pool, and **does not solve the cancel problem** — the only
reliable cancel of model-driven work is `SIGKILL` of a process group, which
async future-drop cannot do. The retired code already ships proven thread-per-fd
prior art.

---

## Mechanisms

### M1. The merged event channel

All asynchronous inputs to the supervisor are normalized into one tagged enum
delivered over a single `std::sync::mpsc::Receiver<Event>`. The tag carries the
source identity so the supervisor can route the payload to the right child
record without a second lookup channel.

```rust
/// Stable identity of a managed source. Reused as the key in the child table
/// (RFC 0003 owns the full SupervisionRecord; this is the reactor-facing key).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
enum SourceId {
    Mcp(McpServerId),     // u32 index into the MCP registry
    Subagent(AgentId),    // u32, the supervisor-minted handle
    Intelligence,         // the single intelligence connection (rare here:
                          //   intelligence I/O usually lives in subagents)
    SelfPipe,             // signal wakeups
    Listener,             // served self-MCP accept (serve-mcp feature)
}

enum Event {
    /// A fully-parsed frame arrived on a reader thread. Bytes already framed
    /// and JSON-validated upstream; the supervisor never parses on its thread.
    Frame { src: SourceId, frame: Frame },
    /// The reader hit EOF (pipe closed / peer exited). The reader thread is
    /// about to terminate. Drives the EOF leg of the EOF×pong classifier (RFC 0003).
    Eof { src: SourceId },
    /// The reader hit a fatal decode/IO error it could not recover from.
    ReaderError { src: SourceId, err: ReaderError },
    /// One or more signal flags were set; the supervisor sweeps the AtomicBools.
    /// Coalesced: many bytes on the self-pipe collapse to (at least) one Event.
    Signal,
    /// A new peer connected to the served self-MCP (serve-mcp feature only).
    Accepted { stream: PeerStream },
}
```

`Frame` is the shared codec type (RFC 0004 / RFC 0005): NDJSON for MCP stdio,
length-prefixed for the control channel. The reactor is codec-agnostic — a reader
thread owns the framing for its source and hands up already-decoded `Frame`s.

**Channel choice.** `std::sync::mpsc` (unbounded) for the merged channel. The
merged channel must never block a reader thread (a blocked reader cannot drain
its pipe → backpressure into a *trusted* internal queue → deadlock risk). Bounded
backpressure is applied per-source at the *outbound* write path (M5) and per-route
at the router (RFC 0008), not on the merged inbound channel. Memory growth on the
inbound channel is bounded in practice because each reader can only produce as
fast as its single upstream pipe delivers, and stuck sources stop producing
entirely.

> Note: `std::sync::mpsc` was reimplemented on `crossbeam` internals in std; we
> rely only on its public API (`Sender`/`Receiver`/`recv_timeout`/`try_recv`).
> No external crate.

### M2. Reader threads

One thread per long-lived readable stream. The body is a straight-line framing
loop — exactly the shape already shipping in the retired `mcp/client.rs`,
`intelligence/client.rs`, and `triggers/http.rs`.

```rust
fn spawn_reader<R: Read + Send + 'static>(
    src: SourceId,
    reader: R,
    framing: Framing,            // Ndjson | LengthPrefixed (RFC 0004/0005)
    tx: mpsc::Sender<Event>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("rd-{src:?}"))
        .stack_size(256 * 1024)  // small stack: readers hold no deep state
        .spawn(move || {
            let mut buf = BufReader::new(reader);
            loop {
                match read_frame(&mut buf, framing) {
                    Ok(Some(frame)) => {
                        // A send error means the supervisor is gone → exit.
                        if tx.send(Event::Frame { src, frame }).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {                       // clean EOF
                        let _ = tx.send(Event::Eof { src });
                        return;
                    }
                    Err(e) if e.is_eintr() => continue, // SA_RESTART is off
                    Err(e) => {
                        let _ = tx.send(Event::ReaderError { src, err: e.into() });
                        return;
                    }
                }
            }
        })
        .expect("spawn reader thread")
}
```

Key properties:

- **A misbehaving source is isolated by construction:** it is one thread blocked
  in one `read`. The supervisor never blocks on it because it only ever reaches
  it via the `mpsc` it `recv_timeout`s.
- **Recovery is "make the producer go away."** Closing the child's stdout (drop
  the handle) or `SIGKILL`-ing the child unblocks the parked `read` into the EOF
  path, where the reader emits `Eof` and exits cleanly. **No `read` is ever
  interrupted in place.** This is the abandon-don't-interrupt invariant made
  concrete.
- **`EINTR` is handled, not fatal.** With `SA_RESTART` off, a signal delivered to
  a reader thread can return `EINTR`; the loop retries the read. (Signal delivery
  targets the dedicated signal thread by mask where possible — see M4 — but the
  retry is correct regardless.)

`read_frame` returns `Ok(None)` on clean EOF, `Ok(Some(frame))` on a complete
frame, and `Err` on a decode/IO error. It is lifted from the retired
`intelligence/protocol.rs` `read_frame`/`write_frame` (assessment §2.3) for the
length-prefixed control channel and from the MCP NDJSON line codec for stdio.

The stuck-pipe walk-through (review §2.5, made concrete): an MCP server hangs
after we sent `tools/call`. Its reader is parked in `read_frame`. The supervisor,
waiting on the merged `mpsc` with the call's deadline armed (M3), times out, marks
the call failed per policy (RFC 0003/0004), and tears down or respawns that MCP
child — closing its stdout, which unblocks the reader into `Eof` and clean exit.
No async machinery; we changed the world the `read` was waiting on.

### M3. The reactor loop

The single supervisor thread. One blocking wait that wakes on any source; the
wait's timeout is the timer.

```rust
fn run_reactor(rx: mpsc::Receiver<Event>, st: &mut Supervisor) -> ExitCode {
    loop {
        let now = Instant::now();

        // 1. Compute the next wake: nearest of all armed timers.
        //    Deadlines, interval/cron fires, ping cadence, backoff retries,
        //    debounce flushes (RFC 0008). O(log n) via a binary min-heap.
        let timeout = st.timers.next_deadline(now)        // Option<Instant>
            .map(|t| t.saturating_duration_since(now))
            .unwrap_or(IDLE_TICK);                        // cap so heartbeat bumps

        // 2. One blocking wait. Idle cost = one parked thread on a futex.
        match rx.recv_timeout(timeout) {
            Ok(ev)                              => st.dispatch(ev),
            Err(RecvTimeoutError::Timeout)      => { /* fall through to timers */ }
            Err(RecvTimeoutError::Disconnected) => return st.fatal_no_sources(),
        }

        // 3. Drain anything else already queued (batch wakeups cheaply).
        while let Ok(ev) = rx.try_recv() { st.dispatch(ev); }

        // 4. Fire due timers: deadlines → stuck/kill verdict (RFC 0003);
        //    intervals/cron → router event (RFC 0008); ping cadence → emit
        //    ping on each subagent control channel; backoff → restart (RFC 0003).
        st.timers.fire_due(Instant::now(), st);

        // 5. Bump the liveness heartbeat EVERY wake, including idle timeouts.
        //    Idle is healthy; a stuck subagent must NOT flip pod liveness
        //    (assessment §2.9 — this RFC owns the bump, RFC 0010 the surface).
        st.last_loop_tick = Instant::now();

        // 6. Honor the one-way draining flag set by the signal sweep (M4).
        if st.draining && st.tree_is_empty() {
            return st.drain_exit_code();   // RFC 0011 exit-code table
        }
    }
}
```

Defaults:

- `IDLE_TICK` = **1 s**. The cap on `recv_timeout` so the heartbeat bumps even
  when no timer is armed (reactive mode at full idle). It does *not* cause
  busy-poll: at idle the thread is parked on a futex 99.9%+ of the time.
- The timer structure is a `BinaryHeap<Reverse<(Instant, TimerId)>>` plus a
  side map `TimerId → Timer{kind, payload}`. `next_deadline` peeks the top;
  `fire_due` pops while `top.0 <= now`. Cancelled timers are tombstoned and
  skipped on pop (lazy deletion) — no extra crate, O(log n) amortized.

The reactor never parses JSON, never does I/O on a child pipe directly, and never
calls a blocking syscall on an untrusted fd. Its only blocking call is
`recv_timeout` on a channel it fully controls.

### M4. Self-pipe signal handling

Signals are delivered into the reactor without races and without `signalfd` (kept
to raw `libc`, no `nix` in the default build — assessment §2.2). The classic
self-pipe trick: an async-signal-safe handler writes one byte to the write end of
a non-blocking pipe whose read end is owned by a dedicated reader thread that
emits `Event::Signal`.

Setup at startup (raw `libc`, in `signals.rs`):

```rust
static SIG_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);   // set once at init
static GOT_TERM:  AtomicBool = AtomicBool::new(false);
static GOT_INT:   AtomicBool = AtomicBool::new(false);
static GOT_CHLD:  AtomicBool = AtomicBool::new(false);

extern "C" fn handler(signum: c_int) {
    match signum {
        libc::SIGTERM => GOT_TERM.store(true, Ordering::SeqCst),
        libc::SIGINT  => GOT_INT.store(true, Ordering::SeqCst),
        libc::SIGCHLD => GOT_CHLD.store(true, Ordering::SeqCst),
        _ => {}
    }
    // Wake the reactor. write() is async-signal-safe. One byte; EAGAIN on a
    // full non-blocking pipe is fine — a pending wake already exists.
    let fd = SIG_PIPE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        let b: u8 = 0;
        unsafe { libc::write(fd, &b as *const u8 as *const c_void, 1); }
    }
}

unsafe fn install(signum: c_int) {
    let mut sa: libc::sigaction = mem::zeroed();
    sa.sa_sigaction = handler as usize;
    libc::sigemptyset(&mut sa.sa_mask);
    sa.sa_flags = 0;            // SA_RESTART deliberately OFF → blocked syscalls EINTR
    libc::sigaction(signum, &sa, ptr::null_mut());
}
```

Installed signals:

| Signal | Action |
|---|---|
| `SIGTERM` | set `GOT_TERM`, wake → reactor sets one-way `draining` flag → bounded drain (RFC 0003 kill ladder, RFC 0011 choreography). Second `SIGTERM` → `force` (immediate `SIGKILL` of all groups). |
| `SIGINT` | identical to `SIGTERM`. Second → force. |
| `SIGCHLD` | set `GOT_CHLD`, wake → reactor runs `waitpid(-1, WNOHANG)` **in a loop** (SIGCHLD does not queue — RFC 0003 owns the reap loop and exit classification). |
| `SIGPIPE` | `signal(SIGPIPE, SIG_IGN)` at startup — one line; prevents the supervisor dying when it writes to a just-dead child (assessment §2.8). Not a handler; ignored outright. |

The signal reader thread:

```rust
fn spawn_signal_reader(read_fd: OwnedFd, tx: mpsc::Sender<Event>) {
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        loop {
            match read(read_fd.as_raw_fd(), &mut buf) {
                Ok(0)                          => return,        // pipe closed
                Ok(_)                          => { let _ = tx.send(Event::Signal); }
                Err(e) if e.is_eintr()         => continue,
                Err(_)                         => return,
            }
        }
    });
}
```

On `Event::Signal`, the reactor sweeps the `AtomicBool`s (`swap(false)`), so the
expensive work (drain choreography, reap loop) runs on the reactor thread, never
in the handler. The byte-coalescing is intentional: many signals → at least one
wake → one sweep that observes all set flags.

**Why no `signalfd`:** raw `libc::sigaction` + self-pipe is ~40 lines of audited
`unsafe` already in the retired `src/signals.rs`, needs no `nix`, and unifies
cleanly with `recv_timeout` (the self-pipe reader is just another reader thread
on the merged channel). `signalfd` would only matter if the supervisor were built
on `poll` — which it is not (M7).

### M5. The write path: `Mutex<ChildStdin>`, `O_NONBLOCK`, bounded queue

Reads are isolated per thread; writes are the inverse problem — many contexts may
need to write to one child's stdin (the reactor sending control frames, a route
delivering an event, a ping). Each writable pipe is guarded by a per-pipe mutex,
set non-blocking, and fronted by a bounded queue so a child that stops draining
its stdin cannot block the writer.

```rust
struct OutPipe {
    stdin: Mutex<ChildStdin>,    // O_NONBLOCK set on the raw fd at spawn
    queue: Mutex<VecDeque<Vec<u8>>>,  // bounded; framed bytes ready to write
    cap: usize,                  // OUTBOUND_QUEUE_CAP
}

enum WriteOutcome { Sent, Queued, Backpressure }

impl OutPipe {
    /// Try to write a framed message. Returns Backpressure (a stuck signal)
    /// if the queue is full — the caller surfaces this per RFC 0003.
    fn enqueue(&self, bytes: Vec<u8>) -> WriteOutcome {
        let mut q = self.queue.lock().unwrap();
        if q.len() >= self.cap {
            return WriteOutcome::Backpressure;
        }
        q.push_back(bytes);
        drop(q);
        self.flush()
    }

    /// Drain the queue with non-blocking writes. EAGAIN/EWOULDBLOCK means the
    /// pipe buffer is full → leave the remainder queued, retry on the next tick.
    fn flush(&self) -> WriteOutcome {
        let mut stdin = self.stdin.lock().unwrap();
        let mut q = self.queue.lock().unwrap();
        while let Some(front) = q.front_mut() {
            match stdin.write(front) {
                Ok(n) if n == front.len() => { q.pop_front(); }
                Ok(n)                     => { front.drain(..n); return WriteOutcome::Queued; }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return WriteOutcome::Queued,
                Err(e) if e.is_eintr()    => continue,
                Err(_)                    => return WriteOutcome::Backpressure, // EPIPE etc.
            }
        }
        WriteOutcome::Sent
    }
}
```

Mechanics:

- **`O_NONBLOCK` on the child stdin fd** is set in `pre_exec`/post-spawn via
  `fcntl(fd, F_SETFL, O_NONBLOCK)` (raw `libc`). Control writes are tiny (well
  under the 64 KiB pipe buffer), so `WouldBlock` is rare — but when it happens we
  must not block the reactor.
- **The bounded queue is the stuck detector.** `OUTBOUND_QUEUE_CAP` default = **64
  frames**. A queue that hits the cap means the child is not draining its stdin —
  itself a stuck signal handed to RFC 0003's detection (a wedged child that stops
  reading). The supervisor does **not** spin waiting; it records the backpressure
  and lets the deadline/no-progress/ping detectors converge on a kill verdict.
- **Queued remainders are flushed on the reactor tick.** `flush()` is also called
  opportunistically when the timer wakes; no writer thread is needed for the
  small control-write volume. (Should a future high-throughput need appear, a
  per-pipe writer thread drops in behind the same `OutPipe` API — but it is not in
  v1.)
- **`EPIPE`/write error → `Backpressure`** and the pipe is marked dead; the
  matching `Eof` from the reader side confirms it and RFC 0003 reaps.

`SIGPIPE` being ignored (M4) is what converts a write-to-dead-child from a process
kill into an `EPIPE` error we handle here.

### M6. Reader/writer lifecycle and the abandon-don't-interrupt invariant

The invariant (assessment §2.1, restated as a recorded design law):

> The supervisor never blocks on an untrusted source. It reaches every pipe only
> via an `mpsc` it `recv_timeout`s, and it unblocks a parked reader only by
> closing/killing the producer, never by interrupting the read.

Consequences for lifecycle:

- **Pipes have no read timeout.** `std` exposes `set_read_timeout` on
  `TcpStream`/`UnixStream`, **not** on `ChildStdout` pipes. We never need it: the
  deadline lives at the reactor's `recv_timeout`, and unblocking is done by
  closing the producer. This is the precise reason thread-per-fd is safe here
  without async.
- **Teardown order:** to retire a source, the supervisor (1) drops/closes the
  child's stdin and stdout handles or `SIGKILL`s the child (RFC 0003 ladder),
  which (2) unblocks the parked reader into `Eof`, after which (3) the reader
  thread emits `Event::Eof` and returns. The supervisor `join()`s reader handles
  only during final shutdown; a stuck reader (its producer is a `D`-state process
  that won't close the fd) is **left parked harmlessly** until process exit — it
  is never join-blocked on the hot path. RFC 0003's classifier reports the
  stuck-leak; the reactor stays live.
- **No reader is ever asked to stop cooperatively.** There is no "please exit"
  channel into a reader. The only way a reader exits is EOF or fatal read error —
  both produced by the producer going away.

### M7. `serve-mcp` fan-in fallback (feature-gated, default OFF)

The single exception to thread-per-fd. When the served self-MCP (RFC 0005)
accepts *many idle* peer connections, one parked reader thread per peer is
wasteful. Behind the `serve-mcp` feature, the accept+read side of the listener
runs on **one `mio::Poll` or raw `libc::poll` loop** (assessment §2.2 table) in a
dedicated thread, dispatching readiness to per-peer frame reassembly and emitting
`Event::Frame { src: SelfMcpPeer(id), .. }` onto the same merged channel.

This is contained to the listener; the core supervision path (MCP-server readers,
subagent readers, intelligence) stays thread-per-fd regardless. For a small peer
count even `--serve-mcp` can stay thread-per-connection and skip `poll` entirely.
The reactor is unchanged — it still only sees `Event`s on the merged channel.

Crate cost: `mio` (deps = `log` + `libc`) **or** raw `libc::poll` (zero new
crates). The choice is deferred to the `serve-mcp` implementation (RFC 0005); the
assessment permits either.

### M8. Per-child reactor-facing fields

This RFC owns only the slice of the supervision record the *reactor* touches;
RFC 0003 owns the full record (parent edges, depth, budgets, detectors, restart
state). The reactor-facing fields:

```rust
struct ReactorChild {
    id: AgentId,
    src: SourceId,              // its merged-channel tag
    reader: Option<JoinHandle<()>>,   // joined only at final shutdown
    out: OutPipe,              // M5 write path
    deadline: Instant,         // armed in the timer heap; mandatory & finite
    next_ping_at: Instant,     // ping cadence; pong resets liveness (RFC 0003)
    last_event_at: Instant,    // stamped on every Event::Frame for this src
}
```

`deadline` is **mandatory and finite** (assessment §2.8 Detector A; default never
infinity). The reactor arms it in the timer heap; on fire it produces a stuck/kill
verdict that RFC 0003 acts on. `last_event_at` is stamped here so RFC 0003's
no-progress watchdog (Detector B) and the EOF×pong classifier read consistent
state. The reactor is the single writer of these fields (single-threaded), so no
locking is needed beyond `OutPipe`'s.

### M9. Defaults summary

| Constant | Default | Meaning |
|---|---|---|
| `IDLE_TICK` | 1 s | cap on `recv_timeout`; heartbeat bump cadence at idle |
| `OUTBOUND_QUEUE_CAP` | 64 frames | per-pipe outbound bound; full = stuck signal |
| reader stack size | 256 KiB | small; readers hold no deep state |
| `SA_RESTART` | off | blocked syscalls return `EINTR` |
| `SIGPIPE` | `SIG_IGN` | write-to-dead-child → `EPIPE`, not process death |
| merged channel | `std::sync::mpsc`, unbounded | inbound never blocks a reader |

---

## Interactions with other RFCs

- **RFC 0001 (core architecture):** this RFC realizes the "supervisor idle at
  near-zero cost" claim and the supervisor/subagent split. The reactor is the
  no-LLM supervisor loop; the agentic loop runs only in subagents.
- **RFC 0003 (supervision, dead/stuck, recovery):** *the primary consumer.* This
  RFC delivers `Event::{Frame,Eof,ReaderError,Signal}`, stamps `last_event_at`,
  arms `deadline`/`next_ping_at` in the timer heap, and surfaces `Backpressure`.
  RFC 0003 owns the three-detector model, the EOF×pong classifier, the
  `waitpid(-1, WNOHANG)` reap loop, `PR_SET_CHILD_SUBREAPER`/`PR_SET_PDEATHSIG`,
  the bounded depth-first kill ladder, the restart governor, and rebuild+reconcile.
  The clean split: this RFC = *how events arrive and writes leave*; RFC 0003 =
  *what to conclude and do about a child*.
- **RFC 0004 (MCP client & codec):** provides the NDJSON `Frame` codec and
  `read_frame`/`write_frame` for MCP-server reader threads; the reactor dispatches
  decoded MCP frames (responses → pending-request map; notifications → router).
- **RFC 0005 (self-MCP server & control protocol):** provides the length-framed
  (4-byte LE + payload, cap 16 MiB) control-channel codec for subagent readers,
  and owns the `serve-mcp` listener whose fan-in fallback (M7) plugs into the same
  merged channel.
- **RFC 0008 (modes, triggers, routing):** the reactor's timer heap is the single
  scheduling subsystem — interval/cron fires (M3 step 4) and debounce flushes are
  timers on this heap. "A clock is just another event source" is realized by
  feeding timer fires into the router exactly as MCP notifications are fed.
- **RFC 0009 (subagent process model):** spawn (re-exec, `setpgid`, `O_NONBLOCK`
  stdin, reader-thread attach) is the producer side; this RFC defines the
  consumer side. The spawn chokepoint registers the new `SourceId` and `OutPipe`.
- **RFC 0010 (observability/health):** this RFC bumps `last_loop_tick` every wake;
  RFC 0010 turns it into the heartbeat liveness surface (idle is healthy; a stuck
  subagent must not flip pod liveness).
- **RFC 0011 (cloud-native contract):** the one-way `draining` flag and second-
  signal `force` set here drive RFC 0011's drain choreography and exit-code
  mapping.

---

## Non-goals / Deferred

- **Async runtime of any kind.** tokio/async-std/smol rejected as core
  (assessment §2.2 "Explicitly OUT"). Not revisited.
- **`mio`/`poll` in the default build.** Reserved strictly for the `serve-mcp`
  high-fan-in listener (M7); the core path is thread-per-fd unconditionally.
- **`signalfd` / `signal-hook` / `nix` for signals.** Raw `libc` self-pipe only
  (assessment §2.2). `nix` may appear *only* transitively via the `vsock` feature,
  never for signals in the default build.
- **Per-pipe writer threads.** Not in v1; the `OutPipe` API leaves room for them
  if a high-throughput need is later proven, but control-write volume does not
  justify them.
- **Read timeouts on pipes.** Structurally unnecessary given the
  abandon-don't-interrupt invariant; never added.
- **The dead/stuck *policy*, reaping, kill ladder, restart governor, hierarchical
  token accounting, cgroup-awareness** — all RFC 0003, not here.
- **Bounded backpressure on the inbound merged channel** — deliberately not
  applied (would risk deadlocking a trusted reader); backpressure lives at the
  outbound write path and the router instead.

## Open items

None that block implementation. Two implementation-time confirmations (carried
from the research note, not architectural):

1. **Reader thread stack size (256 KiB default).** Confirm under the real framing
   code that no reader path needs more; trim further if the minimalism audit
   (M7 of the build plan) wants lower resident memory at 50+ subagents.
2. **`mio` vs raw `libc::poll` for the `serve-mcp` listener.** Decided at RFC 0005
   implementation; both are sanctioned by the assessment and neither affects the
   core reactor.
