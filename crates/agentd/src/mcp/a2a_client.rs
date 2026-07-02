// SPDX-License-Identifier: Apache-2.0
//! The A2A (Agent2Agent) **client** — agentd-as-A2A-client, the remote-A2A-agent
//! delegation backend. RFC 0020 §3. [feature: a2a]
//!
//! This is the third A2A layer (RFC 0020 §3): a coordinator can delegate an
//! objective to a LOCAL supervised subagent (`subagent.spawn`, unchanged) OR to a
//! REMOTE A2A agent. Same abstraction (objective → distilled result); this module
//! is the remote backend. It connects to a declared peer (an [`A2aEndpoint`] —
//! `https://host[:port]`, or loopback `http://` for dev) and drives the A2A unary
//! surface as a client with one `POST` per call (the [`HttpConn`] caller), through
//! the streaming consumer / recovery loop:
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
//! Trust: agentd dials the peer over HTTP(S). (Presenting a client credential TO an
//! authenticated peer — mTLS/bearer — is a follow-up; today it dials peers it
//! already trusts, RFC 0012 §3.8.)

use crate::config::A2aEndpoint;
use crate::json::{Id, Request};
use crate::mcp::a2a::{self, TaskState};
use serde_json::{Value, json};
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
    auth: PeerAuth,
    objective: &str,
    output_contract: Option<&str>,
    deadline: Instant,
) -> DelegateOutcome {
    // The sole transport is HTTP(S), presenting the peer client-auth material
    // (bearer headers and/or an mTLS identity) on every request. STREAMING
    // FIRST: one `a2a.SendStreamingMessage` SSE round trip carries the whole
    // lifecycle (working → artifact → final) with no polling; an older peer
    // that degrades it to a unary final frame is recovered via `a2a.GetTask`
    // (the run happened either way — never re-sent).
    match endpoint {
        A2aEndpoint::Https(url) => match HttpEp::parse(url) {
            Ok(ep) => {
                let mut conn = HttpConn::new(ep, auth);
                match conn.call_streaming(objective, output_contract, deadline) {
                    Err(e) => DelegateOutcome::Error(e),
                    Ok(StreamOutcome::Done(outcome)) => outcome,
                    Ok(StreamOutcome::Recover(task_id)) => {
                        poll_task(&mut conn, &task_id, deadline)
                    }
                }
            }
            Err(e) => DelegateOutcome::Error(e),
        },
    }
}

/// How a streaming attempt resolved: a terminal outcome, or a task id whose
/// terminal state must be RECOVERED over unary `a2a.GetTask` (an older peer's
/// unary-final degradation, or a stream that broke after the run started —
/// the run exists server-side either way, so it is polled, never re-sent).
enum StreamOutcome {
    Done(DelegateOutcome),
    Recover(String),
}

/// Poll `a2a.GetTask` until the task is terminal or the deadline passes — the
/// shared tail of the unary path and stream recovery.
fn poll_task<C: Caller>(conn: &mut C, task_id: &str, deadline: Instant) -> DelegateOutcome {
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

/// The client credential presented TO a peer (RFC 0020 §auth): resolved bearer/
/// framing headers (secrets ALREADY materialized — never logged; this struct has
/// no Debug) and/or an mTLS client identity. Both empty = anonymous dial (a
/// loopback dev peer).
#[derive(Default)]
pub struct PeerAuth {
    /// Resolved header (name, value) pairs sent on every request.
    pub headers: Vec<(String, String)>,
    /// The mutual-TLS client identity presented during the handshake.
    #[cfg(feature = "tls")]
    pub identity: Option<crate::net::tls::ClientIdentity>,
}

/// A one-in-flight-request-at-a-time A2A caller: `call(method, params)` → the
/// `result` Task value or an error string. Implemented by [`HttpConn`] (one HTTP
/// POST per call); the trait keeps [`poll_task`] testable against a fixture.
trait Caller {
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, String>;
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
    auth: PeerAuth,
    next_id: i64,
}

impl HttpConn {
    fn new(ep: HttpEp, auth: PeerAuth) -> HttpConn {
        HttpConn { ep, auth, next_id: 1 }
    }

    fn connect(&self, timeout: Duration) -> Result<Box<dyn crate::net::http::Stream>, String> {
        let tcp = crate::net::http::connect_tcp(&self.ep.host, self.ep.port, timeout)
            .map_err(|e| format!("a2a: cannot reach peer {}: {e}", self.ep.host))?;
        if self.ep.tls {
            #[cfg(feature = "tls")]
            {
                let tls =
                    crate::net::tls::connect(tcp, &self.ep.host, self.auth.identity.as_ref())
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

impl HttpConn {
    /// One `a2a.SendStreamingMessage` round trip: POST with
    /// `Accept: text/event-stream`, then consume the SSE frames — working →
    /// (artifact) → final — to a terminal outcome. Returns `Recover(task_id)`
    /// when the terminal state must be fetched over unary GetTask instead: an
    /// older peer answered `application/json` (its unary-final degradation —
    /// the final frame names the task, the artifact rode a discarded frame), or
    /// the stream broke after the run started. `Err` only when nothing was
    /// started (safe for the caller to surface — the run is NOT duplicated).
    fn call_streaming(
        &mut self,
        objective: &str,
        output_contract: Option<&str>,
        deadline: Instant,
    ) -> Result<StreamOutcome, String> {
        let id = self.next_id;
        self.next_id += 1;
        let message_id = mint_message_id();
        let params = a2a::send_message_params(objective, output_contract, &message_id);
        let req = Request::new(Id::Num(id), "a2a.SendStreamingMessage", Some(params));
        let body =
            serde_json::to_vec(&req).map_err(|e| format!("a2a: encode streaming send: {e}"))?;
        // The read timeout must span the QUIET stretches of a long run; the
        // server writes a keep-alive comment every ~15s, so 45s means three
        // missed beats before the stream is declared dead.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = remaining
            .min(Duration::from_secs(45))
            .max(Duration::from_millis(1));
        let stream = self.connect(timeout)?;
        let mut headers: Vec<(&str, &str)> = vec![
            ("Content-Type", "application/json"),
            ("Accept", "text/event-stream"),
        ];
        for (name, value) in &self.auth.headers {
            headers.push((name.as_str(), value.as_str()));
        }
        let resp = crate::net::http::send_streaming(
            stream,
            &self.ep.host_header,
            "POST",
            &self.ep.path,
            &headers,
            &body,
        )
        .map_err(|e| format!("a2a: streaming send: {e}"))?;
        if resp.status != 200 {
            return Err(format!("a2a: SendStreamingMessage HTTP {}", resp.status));
        }
        let sse = resp
            .header("content-type")
            .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));
        if !sse {
            // An older peer's unary-final degradation: the whole body is ONE
            // JSON-RPC response whose result is the final status frame. The run
            // already happened — recover its artifacts via GetTask.
            use std::io::Read as _;
            let mut text = String::new();
            let _ = resp
                .into_reader()
                .take(1 << 20)
                .read_to_string(&mut text);
            let frame: crate::json::Response = serde_json::from_str(text.trim())
                .map_err(|e| format!("a2a: bad unary streaming reply: {e}"))?;
            if let Some(err) = frame.error {
                return Err(format!("a2a: streaming rpc error {}: {}", err.code, err.message));
            }
            let result = frame.result.unwrap_or(Value::Null);
            // A Task-shaped reply that is already TERMINAL carries its artifacts —
            // resolve it directly (no recovery round trip). Anything else that
            // names a task (a working Task, a final statusUpdate frame whose
            // artifact rode a discarded stream frame) is recovered via GetTask.
            if let Some(outcome) = terminal_outcome(&result) {
                return Ok(StreamOutcome::Done(outcome));
            }
            let task_id = result
                .pointer("/statusUpdate/taskId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(a2a::task_id_of(&result)).filter(|s| !s.is_empty()));
            return match task_id {
                Some(tid) => Ok(StreamOutcome::Recover(tid)),
                None => Err("a2a: unary streaming reply named no task".into()),
            };
        }

        // Consume the stream: statusUpdate frames carry the lifecycle (the one
        // with final:true ends it), a completed run's artifactUpdate carries the
        // distillate just before.
        let mut events = resp.sse();
        let mut task_id: Option<String> = None;
        let mut distillate: Option<String> = None;
        loop {
            if Instant::now() >= deadline {
                return match task_id {
                    Some(tid) => Ok(StreamOutcome::Recover(tid)),
                    None => Err("a2a: deadline while streaming".into()),
                };
            }
            let ev = match events.next_event() {
                Ok(Some(ev)) => ev,
                // EOF / a broken stream: the run may well be alive server-side —
                // recover over GetTask when we know which task it is.
                Ok(None) => {
                    return match task_id {
                        Some(tid) => Ok(StreamOutcome::Recover(tid)),
                        None => Err("a2a: stream ended before any frame".into()),
                    };
                }
                Err(e) => {
                    return match task_id {
                        Some(tid) => Ok(StreamOutcome::Recover(tid)),
                        None => Err(format!("a2a: stream read: {e}")),
                    };
                }
            };
            if ev.data.trim().is_empty() {
                continue;
            }
            let Ok(frame) = serde_json::from_str::<crate::json::Response>(ev.data.trim()) else {
                continue; // an unparseable frame is skipped, not fatal
            };
            if let Some(err) = frame.error {
                return Ok(StreamOutcome::Done(DelegateOutcome::Error(format!(
                    "a2a: streaming rpc error {}: {}",
                    err.code, err.message
                ))));
            }
            let result = frame.result.unwrap_or(Value::Null);
            if let Some(update) = result.get("statusUpdate") {
                if let Some(tid) = update.get("taskId").and_then(Value::as_str) {
                    task_id = Some(tid.to_string());
                }
                let state = update
                    .pointer("/status/state")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let is_final = update.get("final").and_then(Value::as_bool).unwrap_or(false);
                if is_final {
                    let outcome = match state {
                        "TASK_STATE_COMPLETED" => match distillate {
                            Some(text) => DelegateOutcome::Distillate(text),
                            None => DelegateOutcome::Error(
                                "a2a: remote completed without a distillate artifact".into(),
                            ),
                        },
                        "TASK_STATE_REJECTED" => DelegateOutcome::Error(
                            "a2a: remote agent rejected the objective".into(),
                        ),
                        "TASK_STATE_CANCELLED" | "TASK_STATE_CANCELED" => {
                            DelegateOutcome::Error("a2a: remote task was canceled".into())
                        }
                        other => DelegateOutcome::Error(format!(
                            "a2a: remote task ended {other}"
                        )),
                    };
                    return Ok(StreamOutcome::Done(outcome));
                }
            } else if let Some(update) = result.get("artifactUpdate") {
                if let Some(text) = update
                    .pointer("/artifact/parts/0/text")
                    .and_then(Value::as_str)
                {
                    distillate = Some(text.to_string());
                }
            }
            // A full-Task frame (some peers stream the initial Task) is benign:
            // capture its id and keep reading.
            else if !a2a::task_id_of(&result).is_empty() {
                task_id = Some(a2a::task_id_of(&result));
            }
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
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        for (name, value) in &self.auth.headers {
            headers.push((name.as_str(), value.as_str()));
        }
        let resp = crate::net::http::send(
            &mut *stream,
            &self.ep.host_header,
            "POST",
            &self.ep.path,
            &headers,
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
    use std::io::{BufRead, BufReader, Write};
    use std::thread;

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

    /// A tiny loopback-TCP **HTTP** A2A server fixture: answers each `POST` with
    /// one JSON-RPC reply drawn from the canned queue (SendMessage first, then the
    /// GetTask replies). Exercises the `HttpConn` client path end-to-end.
    fn serve_http_fixture(replies: Vec<Value>) -> String {
        serve_http_impl(replies, false)
    }

    /// Like [`serve_http_fixture`] but answers the FIRST call with a JSON-RPC
    /// error object (to exercise the peer-error path).
    fn serve_http_error_fixture() -> String {
        serve_http_impl(vec![json!({"__rpc_error__": true})], true)
    }

    fn serve_http_impl(replies: Vec<Value>, error: bool) -> String {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut idx = 0usize;
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
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
                if replies.is_empty() {
                    break;
                }
                // Clamp-repeat the LAST reply so a never-terminal task keeps the
                // client polling until its deadline (the timeout path).
                let payload = if error {
                    json!({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32602, "message": "bad params"}})
                } else {
                    let result = replies[idx.min(replies.len() - 1)].clone();
                    json!({"jsonrpc": "2.0", "id": req_id, "result": result})
                };
                idx += 1;
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

    /// An SSE A2A fixture: answers the FIRST connection with a `text/event-stream`
    /// of the given frames (each a StreamResponse result; wrapped in JSON-RPC
    /// envelopes here), then — for recovery tests — answers every LATER
    /// connection with unary JSON replies from `unary` (clamp-repeating the last).
    fn serve_sse_fixture(frames: Vec<Value>, unary: Vec<Value>) -> String {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut first = true;
            let mut uidx = 0usize;
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
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
                if first {
                    first = false;
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    );
                    // A comment first — the client must skip keep-alives.
                    let _ = stream.write_all(b": keep-alive\n\n");
                    for f in &frames {
                        let env = json!({"jsonrpc": "2.0", "id": req_id, "result": f});
                        let _ = stream.write_all(format!("data: {env}\n\n").as_bytes());
                    }
                    let _ = stream.flush();
                    continue; // close (drop) the stream after the frames
                }
                if unary.is_empty() {
                    break;
                }
                let result = unary[uidx.min(unary.len() - 1)].clone();
                uidx += 1;
                let payload =
                    json!({"jsonrpc": "2.0", "id": req_id, "result": result}).to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    fn status_frame(id: &str, state: &str, is_final: bool) -> Value {
        json!({"statusUpdate": {"taskId": id, "contextId": "ctx", "status": {"state": state}, "final": is_final}})
    }

    #[test]
    fn delegate_consumes_an_sse_stream_to_the_distillate_without_polling() {
        let url = serve_sse_fixture(
            vec![
                status_frame("s-1", "TASK_STATE_WORKING", false),
                json!({"artifactUpdate": {"taskId": "s-1", "contextId": "ctx", "artifact": {"artifactId": "s-1.distillate", "parts": [{"text": "streamed answer"}]}, "lastChunk": true}}),
                status_frame("s-1", "TASK_STATE_COMPLETED", true),
            ],
            Vec::new(), // NO unary replies — any poll would hang up and fail
        );
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(&ep, PeerAuth::default(), "obj", None, deadline) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "streamed answer"),
            DelegateOutcome::Error(e) => panic!("expected streamed distillate: {e}"),
        }
    }

    #[test]
    fn a_broken_stream_recovers_over_get_task() {
        // The stream dies after WORKING (no final frame); the client recovers the
        // terminal Task over unary GetTask instead of erroring or re-sending.
        let url = serve_sse_fixture(
            vec![status_frame("s-2", "TASK_STATE_WORKING", false)],
            vec![task("s-2", TaskState::Completed, Some("recovered answer"))],
        );
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(&ep, PeerAuth::default(), "obj", None, deadline) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "recovered answer"),
            DelegateOutcome::Error(e) => panic!("expected recovery: {e}"),
        }
    }

    #[test]
    fn a_final_failed_stream_frame_is_a_terminal_error() {
        let url = serve_sse_fixture(
            vec![
                status_frame("s-3", "TASK_STATE_WORKING", false),
                status_frame("s-3", "TASK_STATE_FAILED", true),
            ],
            Vec::new(),
        );
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(&ep, PeerAuth::default(), "obj", None, deadline) {
            DelegateOutcome::Error(e) => assert!(e.contains("FAILED"), "{e}"),
            DelegateOutcome::Distillate(s) => panic!("expected error, got: {s}"),
        }
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
        match delegate(&ep, PeerAuth::default(), "do the work", Some("one line"), deadline) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "http distilled answer"),
            DelegateOutcome::Error(e) => panic!("expected distillate, got error: {e}"),
        }
    }

    #[test]
    fn delegate_presents_the_peer_auth_headers() {
        // The fixture captures the request head of the FIRST call; the resolved
        // bearer header must be on the wire.
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<String>> = Arc::default();
        let cap = Arc::clone(&captured);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Some(Ok(mut stream)) = listener.incoming().next() {
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                *cap.lock().unwrap() = head;
                // Reply terminal immediately so the client stops after one call.
                let result = task("h-a", TaskState::Completed, Some("authed"));
                let payload =
                    json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        });
        let ep = A2aEndpoint::parse(&format!("http://{addr}")).unwrap();
        let auth = PeerAuth {
            headers: vec![("authorization".into(), "Bearer sekrit-token".into())],
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(&ep, auth, "obj", None, deadline) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "authed"),
            DelegateOutcome::Error(e) => panic!("unexpected error: {e}"),
        }
        let head = captured.lock().unwrap().clone();
        assert!(
            head.to_lowercase().contains("authorization: bearer sekrit-token"),
            "the bearer header was presented to the peer:\n{head}"
        );
    }

    #[test]
    fn delegate_over_http_send_message_already_terminal_skips_polling() {
        // A blocking peer returns COMPLETED straight from SendMessage.
        let url = serve_http_fixture(vec![task("h-t", TaskState::Completed, Some("immediate"))]);
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        match delegate(&ep, PeerAuth::default(), "obj", None, deadline) {
            DelegateOutcome::Distillate(s) => assert_eq!(s, "immediate"),
            DelegateOutcome::Error(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn delegate_over_http_surfaces_a_failed_task() {
        let url = serve_http_fixture(vec![task("h-2", TaskState::Failed, None)]);
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(matches!(
            delegate(&ep, PeerAuth::default(), "obj", None, deadline),
            DelegateOutcome::Error(_)
        ));
    }

    #[test]
    fn delegate_over_http_deadline_while_polling_is_a_timeout() {
        // The task never terminates (WORKING repeats) → give up on the deadline.
        let url = serve_http_fixture(vec![task("h-w", TaskState::Working, None)]);
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        match delegate(&ep, PeerAuth::default(), "obj", None, deadline) {
            DelegateOutcome::Error(e) => assert!(e.contains("timed out"), "got: {e}"),
            DelegateOutcome::Distillate(s) => panic!("expected timeout, got: {s}"),
        }
    }

    #[test]
    fn delegate_over_http_surfaces_a_peer_rpc_error() {
        let url = serve_http_error_fixture();
        let ep = A2aEndpoint::parse(&url).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        match delegate(&ep, PeerAuth::default(), "obj", None, deadline) {
            DelegateOutcome::Error(e) => assert!(e.contains("rpc error"), "got: {e}"),
            DelegateOutcome::Distillate(s) => panic!("expected error, got: {s}"),
        }
    }
}
