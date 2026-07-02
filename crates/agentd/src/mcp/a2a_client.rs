// SPDX-License-Identifier: Apache-2.0
//! The A2A (Agent2Agent) **client** — agentd-as-A2A-client, the remote-A2A-agent
//! delegation backend. RFC 0020 §3. [feature: a2a]
//!
//! This is the third A2A layer (RFC 0020 §3): a coordinator can delegate an
//! objective to a LOCAL supervised subagent (`subagent.spawn`, unchanged) OR to a
//! REMOTE A2A agent. Same abstraction (objective → distilled result); this module
//! is the remote backend. It connects to a declared peer (an [`A2aEndpoint`] —
//! `https://host[:port]` over HTTP(S), the target-vision transport; or the legacy
//! `unix:/path` / `vsock:CID:PORT` sockets) and drives the A2A unary surface as a
//! client. HTTP peers use one `POST` per call (the [`HttpConn`] caller); socket
//! peers speak the RFC 0004 JSON-RPC NDJSON codec ([`crate::json::frame`]). Both
//! flow through the same transport-agnostic [`drive`] loop:
//!
//!   1. `a2a.SendMessage` with the objective as one text `Part` (role `ROLE_USER`,
//!      a minted `messageId`) → a Task whose `id` comes back,
//!   2. **poll `a2a.GetTask`** (~[`POLL_INTERVAL`] between polls) until the Task
//!      reaches a terminal `TASK_STATE_*` OR a per-delegation deadline elapses
//!      (so it never hangs),
//!   3. return the result: on COMPLETED, the concatenated text of the Task's
//!      terminal artifact parts (the **distillate**); on FAILED/REJECTED/CANCELED
//!      or a transport error, an error string.
//!
//! An A2A client does **not** send MCP `initialize` — A2A is its own surface, so
//! the client just calls `a2a.*`. The wire (de)serialization is shared with the
//! served side ([`crate::mcp::a2a`]) — one A2A vocabulary, no duplication.
//!
//! Trust: agentd dials the peer over a transport it already trusts (the gateway is
//! the PEP / the vsock peer is in-domain, RFC 0012 §3.8) — no network auth here.

use crate::config::A2aEndpoint;
use crate::json::{Id, Incoming, Request, frame};
use crate::mcp::a2a::{self, TaskState};
use serde_json::{Value, json};
use std::io::{BufReader, Read, Write};
use std::time::{Duration, Instant};

/// Poll cadence between `a2a.GetTask` reads while a remote Task is in flight
/// (RFC 0020 §3: "sleep ~100ms between polls"). Bounded above by the
/// per-delegation deadline the caller passes in.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Per-request read/write timeout on the peer socket — bounds a single
/// SendMessage/GetTask round-trip so a wedged peer can't hang the connect/read.
/// The overall delegation is separately bounded by the caller's deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The outcome of a remote A2A delegation: either the distillate (a COMPLETED
/// Task's terminal artifact text) or an error observation (a non-completed
/// terminal state, a transport failure, or the deadline). Maps straight onto the
/// `(observation, is_error)` tool-result shape the orchestrator returns.
pub enum DelegateOutcome {
    /// COMPLETED: the concatenated terminal-artifact text (may be empty if the
    /// remote completed with no artifact — still a success).
    Distillate(String),
    /// A non-success terminal state, a transport error, or the deadline — an
    /// observation the model sees as `isError`, never a crash.
    Error(String),
}

/// Delegate `objective` to the remote A2A agent at `endpoint`, bounded by
/// `deadline`. Connects, `SendMessage`s, then polls `GetTask` to a terminal
/// state. Every failure path (connect, write, read, RPC error, non-completed
/// terminal, deadline) returns [`DelegateOutcome::Error`] — never panics, never
/// hangs (the deadline is the hard backstop). RFC 0020 §3.
pub fn delegate(
    endpoint: &A2aEndpoint,
    objective: &str,
    output_contract: Option<&str>,
    deadline: Instant,
) -> DelegateOutcome {
    // The per-request socket timeout is the min of REQUEST_TIMEOUT and the time
    // left until the delegation deadline (so a single read never outlives it).
    let timeout = request_timeout(deadline);
    match endpoint {
        // The target-vision transport: dial the peer's A2A method surface over
        // HTTP(S), one POST per unary call (SendMessage + GetTask polls).
        A2aEndpoint::Https(url) => match HttpEp::parse(url) {
            Ok(ep) => drive(HttpConn::new(ep), objective, output_contract, deadline),
            Err(e) => DelegateOutcome::Error(e),
        },
        A2aEndpoint::Unix(path) => {
            let path = path.to_string_lossy().into_owned();
            match crate::net::unixsock::connect(&path, timeout) {
                Ok(stream) => drive(Conn::new(stream), objective, output_contract, deadline),
                Err(e) => DelegateOutcome::Error(format!("a2a: cannot reach peer {path}: {e}")),
            }
        }
        #[cfg(feature = "vsock")]
        A2aEndpoint::Vsock { cid, port } => {
            match crate::net::vsock::connect(*cid, *port, timeout) {
                Ok(stream) => drive(Conn::new(stream), objective, output_contract, deadline),
                Err(e) => DelegateOutcome::Error(format!(
                    "a2a: cannot reach peer vsock:{cid}:{port}: {e}"
                )),
            }
        }
        // A vsock endpoint cannot exist on a non-vsock build (the config validator
        // rejects `vsock:` without the feature), so this arm is unreachable — but
        // keep it total rather than relying on that invariant at a distance.
        #[cfg(not(feature = "vsock"))]
        A2aEndpoint::Vsock { .. } => {
            DelegateOutcome::Error("a2a: vsock peer requires the 'vsock' build feature".into())
        }
    }
}

/// A one-in-flight-request-at-a-time A2A caller: `call(method, params)` → the
/// `result` Task value or an error string. Implemented by [`Conn`] (NDJSON over a
/// socket) and [`HttpConn`] (HTTP POST). Lets [`drive`] be transport-agnostic.
trait Caller {
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, String>;
}

/// Drive the A2A unary exchange over any [`Caller`]: SendMessage → poll GetTask to
/// terminal. Split out from [`delegate`] so it is transport-agnostic (unix / vsock
/// NDJSON and HTTP alike) and directly unit-testable against a fixture.
fn drive<C: Caller>(
    mut conn: C,
    objective: &str,
    output_contract: Option<&str>,
    deadline: Instant,
) -> DelegateOutcome {
    // 1) a2a.SendMessage → a Task; capture its id.
    let message_id = mint_message_id();
    let params = a2a::send_message_params(objective, output_contract, &message_id);
    let task = match conn.call("a2a.SendMessage", params, deadline) {
        Ok(t) => t,
        Err(e) => return DelegateOutcome::Error(e),
    };
    let task_id = a2a::task_id_of(&task);
    if task_id.is_empty() {
        return DelegateOutcome::Error("a2a: peer SendMessage returned no task id".to_string());
    }
    // A SendMessage that already came back terminal (e.g. a blocking peer) needs
    // no polling.
    if let Some(outcome) = terminal_outcome(&task) {
        return outcome;
    }

    // 2) poll a2a.GetTask until terminal or the deadline.
    let get_params = json!({ "id": task_id });
    loop {
        if Instant::now() >= deadline {
            return DelegateOutcome::Error(format!(
                "a2a: delegation to peer timed out (task {task_id} still running)"
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
        let task = match conn.call("a2a.GetTask", get_params.clone(), deadline) {
            Ok(t) => t,
            Err(e) => return DelegateOutcome::Error(e),
        };
        if let Some(outcome) = terminal_outcome(&task) {
            return outcome;
        }
    }
}

/// Map a `Task` value to a terminal [`DelegateOutcome`], or `None` if it is still
/// in flight (the client keeps polling). COMPLETED → the distillate; the other
/// terminal states → a descriptive error observation.
fn terminal_outcome(task: &Value) -> Option<DelegateOutcome> {
    let state = a2a::task_state_of(task);
    if !state.is_terminal() {
        return None;
    }
    Some(match state {
        TaskState::Completed => DelegateOutcome::Distillate(a2a::artifact_text_of(task)),
        TaskState::Rejected => {
            DelegateOutcome::Error("a2a: remote agent rejected the objective".into())
        }
        TaskState::Canceled => DelegateOutcome::Error("a2a: remote task was canceled".into()),
        // Failed (and any other terminal mapped here) — the remote run did not
        // reach a clean conclusion.
        _ => DelegateOutcome::Error("a2a: remote task failed".into()),
    })
}

/// A single A2A client connection: an NDJSON JSON-RPC channel over a peer stream,
/// with a monotonically increasing request id. One in-flight request at a time
/// (the client is strictly request→reply), so a reply is correlated by simply
/// reading the next frame and checking its id.
struct Conn<S: Read + Write> {
    writer: S,
    reader: BufReader<S>,
    next_id: i64,
}

impl<S: Read + Write> Conn<S> {
    fn new(stream: S) -> Conn<S>
    where
        S: CloneStream,
    {
        let reader = BufReader::new(stream.clone_stream());
        Conn {
            writer: stream,
            reader,
            next_id: 1,
        }
    }
}

impl<S: Read + Write> Caller for Conn<S> {
    /// Send one `a2a.<Method>` request and read its reply, returning the `result`
    /// value (a `Task`) or an error string (a transport failure or a JSON-RPC
    /// error object from the peer). Skips any interleaved notification/mismatched
    /// frame and reads until the matching id or the deadline.
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(Id::Num(id), method, Some(params));
        frame::write_line(&mut self.writer, &req)
            .map_err(|e| format!("a2a: write {method}: {e}"))?;

        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "a2a: {method} timed out waiting for the peer reply"
                ));
            }
            let bytes = match frame::read_line(&mut self.reader) {
                Ok(Some(b)) => b,
                Ok(None) => return Err(format!("a2a: peer closed the connection during {method}")),
                Err(e) => return Err(format!("a2a: read {method}: {e}")),
            };
            // Correlate by id; ignore notifications and stray frames.
            match serde_json::from_slice::<Incoming>(&bytes) {
                Ok(Incoming::Response(resp)) => {
                    if resp.id != Id::Num(id) {
                        continue; // not our reply (a multi-frame stream shares an id; we never stream)
                    }
                    if let Some(err) = resp.error {
                        return Err(format!(
                            "a2a: {method} rpc error {}: {}",
                            err.code, err.message
                        ));
                    }
                    return Ok(resp.result.unwrap_or(Value::Null));
                }
                // A request/notification from the peer is not expected on the A2A
                // client path; skip it and keep reading for our reply.
                Ok(_) => continue,
                Err(_) => continue, // unparseable frame — skip, keep the stream moving
            }
        }
    }
}

/// A stream that can be split into a read half and a write half by cloning the
/// underlying file descriptor — both `UnixStream` and `VsockStream` support this.
/// Lets [`Conn`] own a buffered reader and a writer over the same connection.
trait CloneStream: Sized {
    fn clone_stream(&self) -> Self;
}

#[cfg(unix)]
impl CloneStream for std::os::unix::net::UnixStream {
    fn clone_stream(&self) -> Self {
        // A failed clone is exceptional; fall back to a duplicate connect-less
        // panic-free path by re-using the same handle is impossible, so we expect
        // the clone (sockets created here are always cloneable).
        self.try_clone().expect("clone unix peer stream")
    }
}

#[cfg(feature = "vsock")]
impl CloneStream for vsock::VsockStream {
    fn clone_stream(&self) -> Self {
        self.try_clone().expect("clone vsock peer stream")
    }
}

/// A resolved HTTP(S) A2A peer endpoint: the dial coordinates + framing.
struct HttpEp {
    host: String,
    port: u16,
    path: String,
    host_header: String,
    tls: bool,
}

impl HttpEp {
    fn parse(url: &str) -> Result<HttpEp, String> {
        let u = crate::net::http::Url::parse(url).map_err(|e| format!("a2a: bad peer url {url}: {e}"))?;
        let path = if u.path.is_empty() || u.path == "/" {
            "/".to_string()
        } else {
            u.path.clone()
        };
        Ok(HttpEp {
            host_header: u.host_header(),
            tls: u.is_tls(),
            host: u.host,
            port: u.port,
            path,
        })
    }
}

/// An HTTP(S) A2A caller: each unary call is one `POST` (Connection: close),
/// mirroring the MCP client's dialer — server-auth TLS for `https://`, plaintext
/// for a loopback `http://` peer. (Presenting a client credential TO the peer —
/// mTLS/bearer — is a follow-up; today agentd dials peers it already trusts.)
struct HttpConn {
    ep: HttpEp,
    next_id: i64,
}

impl HttpConn {
    fn new(ep: HttpEp) -> HttpConn {
        HttpConn { ep, next_id: 1 }
    }

    fn connect(&self, timeout: Duration) -> Result<Box<dyn crate::net::http::Stream>, String> {
        let tcp = crate::net::http::connect_tcp(&self.ep.host, self.ep.port, timeout)
            .map_err(|e| format!("a2a: cannot reach peer {}: {e}", self.ep.host))?;
        if self.ep.tls {
            #[cfg(feature = "tls")]
            {
                let tls = crate::net::tls::connect(tcp, &self.ep.host, None)
                    .map_err(|e| format!("a2a: tls to peer {}: {e}", self.ep.host))?;
                Ok(Box::new(tls))
            }
            #[cfg(not(feature = "tls"))]
            {
                Err("a2a: https peer requires the 'tls' build feature".to_string())
            }
        } else {
            Ok(Box::new(tcp))
        }
    }
}

impl Caller for HttpConn {
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let timeout = request_timeout(deadline);
        let req = Request::new(Id::Num(id), method, Some(params));
        let body = serde_json::to_vec(&req).map_err(|e| format!("a2a: encode {method}: {e}"))?;
        let mut stream = self.connect(timeout)?;
        let resp = crate::net::http::send(
            &mut *stream,
            &self.ep.host_header,
            "POST",
            &self.ep.path,
            &[("Content-Type", "application/json")],
            &body,
        )
        .map_err(|e| format!("a2a: {method}: {e}"))?;
        if !resp.is_success() {
            return Err(format!("a2a: {method} HTTP {}", resp.status));
        }
        let response: crate::json::Response = serde_json::from_slice(&resp.body)
            .map_err(|e| format!("a2a: {method} bad reply: {e}"))?;
        if let Some(err) = response.error {
            return Err(format!(
                "a2a: {method} rpc error {}: {}",
                err.code, err.message
            ));
        }
        Ok(response.result.unwrap_or(Value::Null))
    }
}

/// Mint a per-delegation `messageId` (the A2A `Message.messageId`). agentd has no
/// ULID dependency; time-plus-counter is unique enough for one client's run.
fn mint_message_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("a2a-msg-{millis:x}-{n:x}")
}

/// The per-request socket timeout: capped by [`REQUEST_TIMEOUT`] but never longer
/// than the time left to the delegation deadline (and never zero — a tiny floor
/// so the connect/read can at least attempt).
fn request_timeout(deadline: Instant) -> Duration {
    let remaining = deadline.saturating_duration_since(Instant::now());
    remaining.min(REQUEST_TIMEOUT).max(Duration::from_millis(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Response;
    use std::io::BufRead;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// A tiny in-process A2A **server fixture**: it reads NDJSON JSON-RPC requests
    /// off one end of a `UnixStream` pair and answers `a2a.SendMessage` /
    /// `a2a.GetTask` with the canned `Task` JSON the test supplies, exercising the
    /// client's full SendMessage → poll-GetTask → distillate path in-process (no
    /// child agentd needed — the e2e composition is covered by the serve_mcp
    /// loopback test).
    fn serve_fixture(
        server: UnixStream,
        send_reply: Value,
        get_replies: Arc<Mutex<Vec<Value>>>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let writer = server.try_clone().expect("clone");
            let mut reader = BufReader::new(server);
            let mut w = writer;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // client hung up
                    Ok(_) => {}
                    Err(_) => break,
                }
                let req: Request = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let result = match req.method.as_str() {
                    "a2a.SendMessage" => send_reply.clone(),
                    "a2a.GetTask" => {
                        let mut q = get_replies.lock().unwrap();
                        if q.len() > 1 {
                            q.remove(0)
                        } else {
                            // last reply repeats (terminal) so a slow client still resolves
                            q[0].clone()
                        }
                    }
                    _ => json!({}),
                };
                let resp = Response::ok(req.id, result);
                if frame::write_line(&mut w, &resp).is_err() {
                    break;
                }
            }
        })
    }

    fn task(id: &str, state: TaskState, artifact: Option<&str>) -> Value {
        let mut t = json!({
            "id": id,
            "contextId": format!("ctx-{id}"),
            "status": { "state": state.as_str(), "timestamp": "1970-01-01T00:00:00.000Z" },
        });
        if let Some(text) = artifact {
            t["artifacts"] =
                json!([{ "artifactId": format!("{id}.distillate"), "parts": [{ "text": text }] }]);
        }
        t
    }

    #[test]
    fn send_then_poll_to_completed_returns_the_distillate() {
        let (client, server) = UnixStream::pair().unwrap();
        // SendMessage → WORKING; GetTask → WORKING, then COMPLETED with the answer.
        let send = task("t-1", TaskState::Working, None);
        let gets = Arc::new(Mutex::new(vec![
            task("t-1", TaskState::Working, None),
            task("t-1", TaskState::Completed, Some("the distilled answer")),
        ]));
        let h = serve_fixture(server, send, gets);

        let deadline = Instant::now() + Duration::from_secs(5);
        let out = drive(Conn::new(client), "do the work", Some("one line"), deadline);
        match out {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "the distilled answer"),
            DelegateOutcome::Error(e) => panic!("expected distillate, got error: {e}"),
        }
        let _ = h.join();
    }

    #[test]
    fn send_message_already_terminal_skips_polling() {
        let (client, server) = UnixStream::pair().unwrap();
        // A blocking peer returns COMPLETED straight from SendMessage.
        let send = task("t-2", TaskState::Completed, Some("immediate"));
        let gets = Arc::new(Mutex::new(vec![task("t-2", TaskState::Working, None)]));
        let h = serve_fixture(server, send, gets);
        let deadline = Instant::now() + Duration::from_secs(5);
        match drive(Conn::new(client), "obj", None, deadline) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "immediate"),
            DelegateOutcome::Error(e) => panic!("unexpected error: {e}"),
        }
        let _ = h.join();
    }

    #[test]
    fn failed_remote_task_is_an_error_outcome() {
        let (client, server) = UnixStream::pair().unwrap();
        let send = task("t-3", TaskState::Working, None);
        let gets = Arc::new(Mutex::new(vec![task("t-3", TaskState::Failed, None)]));
        let h = serve_fixture(server, send, gets);
        let deadline = Instant::now() + Duration::from_secs(5);
        match drive(Conn::new(client), "obj", None, deadline) {
            DelegateOutcome::Error(e) => assert!(e.contains("failed"), "got: {e}"),
            DelegateOutcome::Distillate(s) => panic!("expected error, got distillate: {s}"),
        }
        let _ = h.join();
    }

    #[test]
    fn deadline_while_polling_is_a_timeout_error() {
        let (client, server) = UnixStream::pair().unwrap();
        // The task never terminates → the client must give up on the deadline.
        let send = task("t-4", TaskState::Working, None);
        let gets = Arc::new(Mutex::new(vec![task("t-4", TaskState::Working, None)]));
        let h = serve_fixture(server, send, gets);
        // A short deadline so the test is fast.
        let deadline = Instant::now() + Duration::from_millis(300);
        match drive(Conn::new(client), "obj", None, deadline) {
            DelegateOutcome::Error(e) => assert!(e.contains("timed out"), "got: {e}"),
            DelegateOutcome::Distillate(s) => panic!("expected timeout, got: {s}"),
        }
        // Dropping the client end unblocks the fixture's read loop.
        let _ = h.join();
    }

    #[test]
    fn peer_rpc_error_is_surfaced() {
        let (client, server) = UnixStream::pair().unwrap();
        // The fixture answers SendMessage with a JSON-RPC error instead of a Task.
        let h = thread::spawn(move || {
            let w = server.try_clone().unwrap();
            let mut reader = BufReader::new(server);
            let mut w = w;
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let req: Request = serde_json::from_str(&line).unwrap();
                let resp = Response::err(req.id, -32602, "bad params");
                let _ = frame::write_line(&mut w, &resp);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        match drive(Conn::new(client), "obj", None, deadline) {
            DelegateOutcome::Error(e) => assert!(e.contains("rpc error"), "got: {e}"),
            DelegateOutcome::Distillate(s) => panic!("expected error, got: {s}"),
        }
        let _ = h.join();
    }

    /// A tiny loopback-TCP **HTTP** A2A server fixture: answers each `POST` with
    /// one JSON-RPC reply drawn from the canned queue (SendMessage first, then the
    /// GetTask replies). Exercises the `HttpConn` client path end-to-end.
    fn serve_http_fixture(replies: Vec<Value>) -> String {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut queue = replies.into_iter();
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .ok();
                // Read the request head + Content-Length body.
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                let mut req_id = json!(1);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.trim().eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                if reader.read_exact(&mut body).is_ok()
                    && let Ok(rpc) = serde_json::from_slice::<Value>(&body)
                {
                    req_id = rpc["id"].clone();
                }
                let Some(result) = queue.next() else { break };
                let payload = json!({"jsonrpc": "2.0", "id": req_id, "result": result});
                let text = serde_json::to_vec(&payload).unwrap();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    text.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&text);
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn delegate_over_http_send_then_poll_returns_the_distillate() {
        // SendMessage → WORKING; GetTask → WORKING then COMPLETED — over HTTP.
        let url = serve_http_fixture(vec![
            task("h-1", TaskState::Working, None),
            task("h-1", TaskState::Working, None),
            task("h-1", TaskState::Completed, Some("http distilled answer")),
        ]);
        let ep = A2aEndpoint::parse(&url).expect("parse https endpoint");
        assert!(matches!(ep, A2aEndpoint::Https(_)));
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(&ep, "do the work", Some("one line"), deadline) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "http distilled answer"),
            DelegateOutcome::Error(e) => panic!("expected distillate, got error: {e}"),
        }
    }

    #[test]
    fn delegate_over_http_surfaces_a_failed_task() {
        let url = serve_http_fixture(vec![task("h-2", TaskState::Failed, None)]);
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(matches!(
            delegate(&ep, "obj", None, deadline),
            DelegateOutcome::Error(_)
        ));
    }
}
