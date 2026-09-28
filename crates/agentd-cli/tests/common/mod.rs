// SPDX-License-Identifier: AGPL-3.0-only
//! Shared E2E harness: launch the built-in **HTTP** mock MCP server as a
//! subprocess and hand agentd its loopback-TCP endpoint. The mock binds
//! `127.0.0.1:0` and announces the bound address through an **addr-file**
//! (`agentd::announce_addr`); the harness waits for the file, reads the
//! address, and dials `http://<addr>`.
//!
//! It is also the A2A client every listener test speaks through — see the
//! section below.
#![allow(dead_code)] // each test file uses a different subset of these helpers.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A running mock HTTP MCP server. Killed (and its addr-file and log removed)
/// on drop.
pub struct MockMcp {
    child: Child,
    addr_file: String,
    addr: String,
    /// The mock's stderr: its `MOCK_*` lines are the server side's own record
    /// of what reached it.
    log_path: String,
}

impl MockMcp {
    /// The bare `http://<addr>` endpoint agentd dials.
    pub fn uri(&self) -> String {
        format!("http://{}", self.addr)
    }
    /// The `--mcp` argument value: `name=http://<addr>`.
    pub fn mcp_arg(&self, name: &str) -> String {
        format!("{name}=http://{}", self.addr)
    }
    /// Everything the mock wrote to stderr so far.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
    /// How many `resources/read` calls for exactly `uri` reached the mock —
    /// counted by the server, so a read the client answered from anywhere else
    /// does not count.
    pub fn reads(&self, uri: &str) -> usize {
        let want = format!("MOCK_READ {uri}");
        self.log().lines().filter(|l| *l == want).count()
    }
    /// Stop the server mid-test: afterwards its port refuses connections.
    /// Idempotent — a second call (or the drop) is a no-op on a reaped child.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for MockMcp {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.addr_file);
        let _ = std::fs::remove_file(&self.log_path);
    }
}

/// Launch the mock HTTP MCP server serving one resource at `uri`. `emit` controls
/// the post-subscribe `resources/updated` push on the GET SSE stream. Blocks
/// until the mock has bound and announced its address (so agentd can connect
/// immediately).
pub fn spawn_mock_mcp(uri: &str, emit: bool) -> MockMcp {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let addr_file = unique_path("mock-mcp", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let mut args = vec![
        "--internal-mock-mcp-http".to_string(),
        addr_file.clone(),
        uri.to_string(),
    ];
    if !emit {
        args.push("--no-emit".to_string());
    }
    let log_path = unique_path("mock-mcp", "log");
    let log = std::fs::File::create(&log_path).expect("mock log");
    let child = Command::new(exe)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("spawn mock http mcp");
    let addr = read_addr_file(&addr_file);
    MockMcp {
        child,
        addr_file,
        addr,
        log_path,
    }
}

/// A free loopback TCP port (bind :0, read it back, release).
#[allow(dead_code)]
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("addr")
        .port()
}

/// A unique path under the temp dir (per-process + per-call), for addr-files
/// and other per-test artifacts.
pub fn unique_path(tag: &str, ext: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir();
    dir.join(format!("agentd-{tag}-{}-{n}.{ext}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// Block until `path` exists (the mock has bound + announced), then return the
/// `host:port` address it carries.
pub fn read_addr_file(path: &str) -> String {
    wait_for_file(path);
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read mock addr-file {path}: {e}"))
        .trim()
        .to_string()
}

/// Block until `path` exists (bounded).
pub fn wait_for_file(path: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new(path).exists() {
        assert!(
            Instant::now() < deadline,
            "mock addr-file never appeared: {path}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The A2A authority a daemon ACTUALLY bound, read from its telemetry.
///
/// Tests configure `a2a.listen: http://127.0.0.1:0` and learn the port here,
/// instead of pre-picking one with a bind-and-drop probe: under parallel load
/// another process can take that port in the gap, and the test then talks to a
/// stranger's listener (or to nothing). The daemon logs the bound authority on
/// its `a2a.listen` line, which is the only race-free source.
pub fn wait_a2a_bound(stderr_path: &str) -> String {
    try_a2a_bound(stderr_path, Duration::from_secs(20)).unwrap_or_else(|| {
        let log = std::fs::read_to_string(stderr_path).unwrap_or_default();
        panic!("no a2a.listen line; daemon stderr:\n{log}")
    })
}

/// [`wait_a2a_bound`] that gives up instead of panicking — a caller retrying a
/// stolen port needs to distinguish "not yet" from "this daemon is dead".
pub fn try_a2a_bound(stderr_path: &str, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(log) = std::fs::read_to_string(stderr_path) {
            for line in log.lines().rev() {
                if !line.contains("\"a2a.listen\"") {
                    continue;
                }
                if let Some(rest) = line.split("\"bound\":\"").nth(1)
                    && let Some(addr) = rest.split('"').next()
                {
                    return Some(addr.to_string());
                }
            }
            // A daemon whose bind lost the race exits at once — do not wait out
            // the whole timeout for a process that is already gone.
            if log.contains("a2a listen") || log.contains("a2a.listen:") {
                return None;
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ── The A2A client harness ──────────────────────────────────────────────────
//
// Every e2e test that talks to the listener goes through here, so the request
// shape the specification asks for is written ONCE: the `A2A-Version` header,
// `ROLE_USER`, an explicit `returnImmediately`, a command marked as the
// extension it belongs to. The tests used to carry a dozen copies of a
// hand-rolled POST, and every one of them spelled the request its own way —
// which is how a suite ends up green against a server that accepts shapes no
// real client sends. `harness_guard.rs` holds the line: it proves the helpers
// send the spec's shape, and refuses the legacy spellings anywhere else.

/// The `A2A-Version` every request carries: the protocol version the listener
/// answers. Spelled as the spec's literal, not read from the daemon, so a test
/// notices when the server's idea of the version drifts from the one clients
/// actually send.
pub const A2A_VERSION: &str = "1.0";

/// How long a plain request may take. A blocking `SendMessage` holds the
/// connection until its task settles, which is a model turn on a loaded runner.
const REQUEST_BUDGET: Duration = Duration::from_secs(130);

/// One HTTP response, read until the server closed the connection.
#[derive(Debug, Clone)]
pub struct HttpReply {
    pub status: u16,
    /// Header names lowercased, values as sent, in the order they arrived.
    pub headers: Vec<(String, String)>,
    /// The raw body. An SSE body is kept as the wire has it, so a test can ask
    /// whether any `data:` frame arrived at all.
    pub body: String,
}

impl HttpReply {
    /// The value of header `name` (any case), if the response carried it.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body as JSON, panicking with the whole response when it is not.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("non-JSON reply ({e}): {} {:?}", self.status, self.body))
    }
}

/// The request line and headers of one POST to `/`. The ONE place a harness
/// request head is written, so no helper can leave out the version or the
/// content type.
fn post_head(body_len: usize, extra: &[(&str, &str)]) -> String {
    let mut head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nA2A-Version: {A2A_VERSION}\r\nContent-Length: {body_len}\r\nConnection: close\r\n"
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    head
}

fn parse_reply(raw: &[u8]) -> HttpReply {
    let text = String::from_utf8_lossy(raw).into_owned();
    let (head, body) = match text.find("\r\n\r\n") {
        Some(i) => (&text[..i], text[i + 4..].to_string()),
        None => (text.as_str(), String::new()),
    };
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    HttpReply {
        status,
        headers,
        body,
    }
}

/// POST `body` and read until the connection closes, `budget` runs out, or
/// `done` says the bytes so far already answer the question. A transport
/// failure is an `Err`, because for some suites "the listener hung up" IS the
/// finding.
fn exchange(
    addr: &str,
    body: &str,
    extra: &[(&str, &str)],
    budget: Duration,
    done: &dyn Fn(&[u8]) -> bool,
) -> Result<HttpReply, String> {
    let mut s = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    // A short poll rather than one long timeout, so `budget` bounds the whole
    // read and a stream that never ends still returns what it sent.
    s.set_read_timeout(Some(Duration::from_millis(200))).ok();
    s.write_all(post_head(body.len(), extra).as_bytes())
        .and_then(|()| s.write_all(body.as_bytes()))
        .and_then(|()| s.flush())
        .map_err(|e| format!("write request: {e}"))?;
    let deadline = Instant::now() + budget;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if done(&raw) {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(format!("read reply: {e}")),
        }
    }
    if raw.is_empty() {
        return Err("the listener closed the connection without a response".into());
    }
    Ok(parse_reply(&raw))
}

/// POST a JSON-RPC body to the listener with the spec's headers plus `extra`.
pub fn a2a_post(addr: &str, body: &str, extra: &[(&str, &str)]) -> HttpReply {
    try_a2a_post(addr, body, extra).unwrap_or_else(|e| panic!("POST to {addr}: {e}"))
}

/// [`a2a_post`] that reports a transport failure instead of panicking.
pub fn try_a2a_post(addr: &str, body: &str, extra: &[(&str, &str)]) -> Result<HttpReply, String> {
    exchange(addr, body, extra, REQUEST_BUDGET, &|_| false)
}

/// [`a2a_post`] bounded by `budget`, returning as soon as one SSE frame has
/// arrived. For a stream that may never end: the question is whether anything
/// was delivered at all, and the first frame answers it.
pub fn a2a_post_within(
    addr: &str,
    body: &str,
    extra: &[(&str, &str)],
    budget: Duration,
) -> HttpReply {
    exchange(addr, body, extra, budget, &|raw| {
        raw.windows(6).any(|w| w == b"\ndata:")
    })
    .unwrap_or_else(|e| panic!("POST to {addr}: {e}"))
}

/// POST and hand back the open connection, for a caller that reads an SSE
/// stream line by line as it arrives.
pub fn a2a_open(
    addr: &str,
    body: &str,
    extra: &[(&str, &str)],
    read_timeout: Duration,
) -> BufReader<TcpStream> {
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    s.set_read_timeout(Some(read_timeout)).ok();
    s.write_all(post_head(body.len(), extra).as_bytes())
        .unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
    BufReader::new(s)
}

/// A JSON-RPC request envelope.
pub fn rpc_body(id: i64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// A JSON-RPC call; returns the whole envelope — result or error.
pub fn rpc(addr: &str, id: i64, method: &str, params: Value) -> Value {
    a2a_post(addr, &rpc_body(id, method, params), &[]).json()
}

/// [`rpc`] presenting `bearer` as the caller's credential.
pub fn rpc_as(addr: &str, bearer: &str, id: i64, method: &str, params: Value) -> Value {
    let auth = format!("Bearer {bearer}");
    a2a_post(
        addr,
        &rpc_body(id, method, params),
        &[("Authorization", &auth)],
    )
    .json()
}

/// A JSON-RPC call that must succeed; returns its `result`.
pub fn rpc_result(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let v = rpc(addr, id, method, params);
    assert!(v.get("error").is_none(), "A2A rpc error for {method}: {v}");
    v["result"].clone()
}

/// A JSON-RPC call that must fail; returns `(code, message)`.
pub fn rpc_error(addr: &str, id: i64, method: &str, params: Value) -> (i64, String) {
    error_of(&rpc(addr, id, method, params))
}

/// `(code, message)` of an envelope that must be an error.
pub fn error_of(v: &Value) -> (i64, String) {
    let e = v
        .get("error")
        .unwrap_or_else(|| panic!("expected an error: {v}"));
    (
        e["code"].as_i64().unwrap_or(0),
        e["message"].as_str().unwrap_or("").to_string(),
    )
}

/// The public agent card, read the way the spec publishes it: an
/// unauthenticated GET of the well-known path, not a JSON-RPC method.
pub fn get_card(addr: &str) -> Value {
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(
        b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: x\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok();
    let reply = parse_reply(&raw);
    assert_eq!(reply.status, 200, "GET agent-card.json: {reply:?}");
    reply.json()
}

/// The JSON-RPC method of the observation feed. Named in one place because the
/// feed is an extension method, and its name is the extension's to decide.
pub fn feed_method() -> &'static str {
    "SubscribeToEvents"
}

/// Open the observation feed from `from_seq`; the caller reads SSE lines.
pub fn subscribe_feed(addr: &str, from_seq: u64, read_timeout: Duration) -> BufReader<TcpStream> {
    let body = rpc_body(77, feed_method(), json!({"fromSeq": from_seq}));
    a2a_open(addr, &body, &[], read_timeout)
}

/// Every SSE `data:` payload on `reader` that parses as JSON, until the stream
/// ends or its read timeout fires, each handed to `each`; stops when `each`
/// returns `false`.
pub fn read_frames(reader: &mut BufReader<TcpStream>, mut each: impl FnMut(Value) -> bool) {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(v) = serde_json::from_str::<Value>(data.trim())
            && !each(v)
        {
            return;
        }
    }
}

/// A fresh `messageId`: a message id names one message, so no two sends share
/// one, even across the daemons a test restarts.
fn message_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "m-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// A `SendMessage` in the shape the specification gives it.
///
/// The role is `ROLE_USER` and `returnImmediately` is always stated, never
/// left to the server's default. A command is a DataPart under `agentd`, and
/// it is MARKED: the message lists the command extension in `extensions`, and
/// the request activates it with the `A2A-Extensions` header — which is what a
/// client that read the card would send.
#[derive(Debug, Clone)]
pub struct SendMessage {
    message: Value,
    command: bool,
    return_immediately: bool,
    streaming: bool,
    bearer: Option<String>,
}

impl SendMessage {
    /// A natural-language message.
    pub fn text(text: &str) -> Self {
        Self::of(json!([{"text": text}]), false)
    }

    /// A command: `op` with `args` merged beside it.
    pub fn command(op: &str, args: Value) -> Self {
        let mut data = json!({"op": op});
        if let (Value::Object(d), Value::Object(a)) = (&mut data, args) {
            d.extend(a);
        }
        let mut s = Self::of(json!([{"data": {"agentd": data}}]), true);
        s.message["extensions"] = json!([agentd::runtime::surface::COMMAND_EXTENSION]);
        s
    }

    fn of(parts: Value, command: bool) -> Self {
        Self {
            message: json!({"role": "ROLE_USER", "messageId": message_id(), "parts": parts}),
            command,
            return_immediately: false,
            streaming: false,
            bearer: None,
        }
    }

    /// Continue the task `id` (a reply to a gate, say).
    pub fn task(mut self, id: &str) -> Self {
        self.message["taskId"] = json!(id);
        self
    }

    /// Send within the conversation `id`.
    pub fn context(mut self, id: &str) -> Self {
        self.message["contextId"] = json!(id);
        self
    }

    /// Answer as soon as the task exists rather than when it settles.
    pub fn return_immediately(mut self) -> Self {
        self.return_immediately = true;
        self
    }

    /// Send as `SendStreamingMessage`.
    pub fn streaming(mut self) -> Self {
        self.streaming = true;
        self
    }

    /// Present `bearer` as the caller's credential.
    pub fn bearer(mut self, bearer: &str) -> Self {
        self.bearer = Some(bearer.to_string());
        self
    }

    pub fn method(&self) -> &'static str {
        if self.streaming {
            "SendStreamingMessage"
        } else {
            "SendMessage"
        }
    }

    /// The `params` of the request.
    pub fn params(&self) -> Value {
        json!({
            "message": self.message,
            "configuration": {"returnImmediately": self.return_immediately},
        })
    }

    /// The headers the request carries beyond the ones every request does.
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut h = Vec::new();
        if self.command {
            h.push((
                "A2A-Extensions".to_string(),
                agentd::runtime::surface::COMMAND_EXTENSION.to_string(),
            ));
        }
        if let Some(b) = &self.bearer {
            h.push(("Authorization".to_string(), format!("Bearer {b}")));
        }
        h
    }

    /// The whole JSON-RPC request body.
    pub fn body(&self, id: i64) -> String {
        rpc_body(id, self.method(), self.params())
    }

    /// Send, returning the raw response.
    pub fn post_raw(&self, addr: &str) -> HttpReply {
        self.try_post_raw(addr)
            .unwrap_or_else(|e| panic!("{} to {addr}: {e}", self.method()))
    }

    /// [`Self::post_raw`] that reports a transport failure instead of panicking.
    pub fn try_post_raw(&self, addr: &str) -> Result<HttpReply, String> {
        let headers = self.headers();
        let extra: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        try_a2a_post(addr, &self.body(1), &extra)
    }

    /// Send, returning the JSON-RPC envelope — result or error.
    pub fn post(&self, addr: &str) -> Value {
        self.post_raw(addr).json()
    }

    /// Send a request that must succeed; returns its `result`.
    pub fn result(&self, addr: &str) -> Value {
        let v = self.post(addr);
        assert!(v.get("error").is_none(), "{} error: {v}", self.method());
        v["result"].clone()
    }
}

/// Send `text` and wait for the task to settle; returns the envelope.
pub fn send_text(addr: &str, text: &str) -> Value {
    SendMessage::text(text).post(addr)
}

/// Run the command `op` with `args`; returns the envelope.
pub fn send_command(addr: &str, op: &str, args: Value) -> Value {
    SendMessage::command(op, args).post(addr)
}
