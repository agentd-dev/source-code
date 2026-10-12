// SPDX-License-Identifier: AGPL-3.0-only
//! A unit of work's trace crosses agentd's boundaries in both directions:
//!
//! * an A2A request's `traceparent` is continued by the task and the run it
//!   starts — its span the parent, its flags and `tracestate` kept — and a
//!   malformed one is ignored; the message's `metadata` stands in for a
//!   caller that cannot set a header; the response names the task's span;
//! * two agentd daemons over A2A are one trace: the caller's run, the span its
//!   delegation logged, the callee's task, run and MCP calls;
//! * work that only resumes a unit records its cause as a link and leaves the
//!   unit in its trace: a peer's reply that wakes an `a2a.wait`, a message
//!   that continues a task;
//! * a webhook that starts a run continues the sender's trace, and a callback
//!   to a waiting run is recorded as a link while the run keeps its own;
//! * a stream event carries the span it was emitted in, a `stream` start's run
//!   continues it, and a `batch` run starts a trace of its own linked to every
//!   member;
//! * the `http` step and an A2A push notification send the unit's span, and
//!   an `http` step whose headers set the trace context sends only its own;
//! * an MCP Events occurrence's `_meta.traceparent` starts its run in that
//!   trace.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// An upstream caller's span, sampled.
const UP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
/// Another, unsampled.
const UP_UNSAMPLED: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00";
const STATE: &str = "congo=t61rcWkgMzE,rojo=00f067aa0ba902b7";

/// The trace id a `traceparent` names.
fn trace_of(tp: &str) -> &str {
    assert_eq!(tp.len(), 55, "not a W3C traceparent: {tp}");
    &tp[3..35]
}

/// The span id a `traceparent` names.
fn span_of(tp: &str) -> &str {
    &tp[36..52]
}

/// A daemon whose stderr the test reads as it goes; stopped on drop.
struct Daemon {
    child: Child,
    err_path: String,
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    /// Its JSON lines named `event`.
    fn events(&self, event: &str) -> Vec<Value> {
        self.log()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == event)
            .collect()
    }

    /// The first line named `event` that `pred` accepts, waited for.
    fn wait_line(&self, event: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(v) = self.events(event).into_iter().find(&pred) {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "no matching {event} line:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.err_path);
    }
}

fn spawn(tag: &str, config: &str) -> Daemon {
    let cfg = common::unique_path(tag, "yaml");
    std::fs::write(&cfg, config).expect("write config");
    let err_path = common::unique_path(tag, "err");
    let err = std::fs::File::create(&err_path).expect("stderr file");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn agentd");
    Daemon { child, err_path }
}

/// A daemon on `config_for(port)`, once its `listener` (`a2a.listen`,
/// `webhooks.listen`) has bound: the daemon and the authority it bound.
fn spawn_on(tag: &str, listener: &str, config_for: impl Fn(u16) -> String) -> (Daemon, String) {
    common::spawn_listener_bound(listener, |port| {
        let d = spawn(tag, &config_for(port));
        let log = d.err_path.clone();
        (d, log)
    })
}

/// One request a [`Capture`] server received: its headers (names lowercased)
/// and its body.
type Captured = (Vec<(String, String)>, Value);

/// A loopback HTTP server that records every request and answers `200 {}`.
struct Capture {
    addr: String,
    seen: Arc<Mutex<Vec<Captured>>>,
}

impl Capture {
    fn start() -> Capture {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let out = Arc::clone(&seen);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let out = Arc::clone(&out);
                std::thread::spawn(move || serve(conn, &out));
            }
        });
        Capture { addr, seen }
    }

    /// The first request that arrived, waited for.
    fn first(&self) -> Captured {
        self.find(|_| true)
    }

    /// The first request `pred` accepts, waited for.
    fn find(&self, pred: impl Fn(&Captured) -> bool) -> Captured {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(c) = self.seen.lock().unwrap().iter().find(|c| pred(c)).cloned() {
                return c;
            }
            assert!(
                Instant::now() < deadline,
                "nothing matching reached {}",
                self.addr
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn serve(conn: TcpStream, out: &Arc<Mutex<Vec<Captured>>>) {
    conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut w = conn.try_clone().unwrap();
    let mut r = BufReader::new(conn);
    let mut start = String::new();
    if r.read_line(&mut start).unwrap_or(0) == 0 {
        return;
    }
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        let mut l = String::new();
        if r.read_line(&mut l).unwrap_or(0) == 0 {
            return;
        }
        if l.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            if k == "content-length" {
                len = v.trim().parse().unwrap_or(0);
            }
            headers.push((k, v.trim().to_string()));
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok();
    out.lock().unwrap().push((
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    ));
    let _ = w.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// The `traceparent` every `tools/call` that reached `mock` for run `run`
/// carried in `params._meta`, with its `tracestate`.
fn tool_call_traces(mock: &common::MockMcp, run: &str) -> Vec<(String, Option<String>)> {
    let calls: Vec<(String, Option<String>)> = mock
        .metas()
        .into_iter()
        .filter(|m| m["method"] == "tools/call" && m["_meta"]["agent/run"] == run)
        .map(|m| {
            (
                m["_meta"]["traceparent"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                m["_meta"]["tracestate"].as_str().map(str::to_string),
            )
        })
        .collect();
    assert!(!calls.is_empty(), "no tools/call for {run}: {}", mock.log());
    calls
}

/// A callee: an A2A listener, a workflow `work` that calls the mock MCP
/// server once, push notifications to loopback, and the audit on the log.
fn callee_config(port: u16, mcp: &str) -> String {
    format!(
        "\
         agent:\n  name: callee\n  instruction: test\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
         store:\n  kind: memory\n\
         mcp:\n  servers:\n    - name: mock\n      endpoint: {mcp}\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n  push:\n    enabled: true\n    allow_private: true\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  audit:\n    sink: [log]\n\
         workflows:\n\
         \x20 - name: work\n\
         \x20   steps:\n\
         \x20     s: {{kind: manual}}\n\
         \x20     call: {{kind: mcp.tool, depends_on: [s], server: mock, tool: state.put,\n\
         \x20            args: {{key: k, seq: 1, state: {{n: 1}}}}}}\n\
         \x20     f: {{kind: finish, depends_on: [call], status: completed, output: done}}\n"
    )
}

/// `workflow.run {workflow: work}` as a plain A2A client sends it.
fn run_work() -> common::SendMessage {
    common::SendMessage::command("workflow.run", json!({"workflow": "work"})).return_immediately()
}

/// POST `send` with `params` (its own, unless given) and the headers it
/// carries; the reply.
fn post(addr: &str, send: &common::SendMessage, params: Option<Value>) -> common::HttpReply {
    let headers = send.headers();
    let extra: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let body = common::rpc_body(1, send.method(), params.unwrap_or_else(|| send.params()));
    common::a2a_post(addr, &body, &extra)
}

/// An A2A request's `traceparent` is the parent of the work it starts: the
/// task and its run continue the caller's trace, one span below the caller's
/// span, unsampled as the caller said and with its `tracestate` — through to
/// the run's MCP call and the push about the task — and the response names
/// the task's span. A malformed `traceparent` starts a trace of agentd's own,
/// and a caller that cannot set a header may name its span in the message's
/// `metadata`.
#[test]
fn an_a2a_request_s_trace_is_continued_by_its_task_and_run() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let hook = Capture::start();
    let (b, addr) = spawn_on("trace-a2a", "a2a.listen", |port| {
        callee_config(port, &mock.uri())
    });

    // 1. A header: unsampled, with a vendor state, and a push to hear about it.
    let send = run_work()
        .header("traceparent", UP_UNSAMPLED)
        .header("tracestate", STATE);
    let mut params = send.params();
    params["configuration"]["taskPushNotificationConfig"] =
        json!({"url": format!("http://{}/hook", hook.addr)});
    let reply = post(&addr, &send, Some(params));
    assert_eq!(reply.status, 200, "{reply:?}");
    let task = reply.json()["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a task: {}", reply.body))
        .to_string();
    let up_trace = trace_of(UP_UNSAMPLED);
    let run = b.wait_line("run.start", |v| v["workflow"] == "work");
    let run_id = run["run"].as_str().unwrap().to_string();
    assert_eq!(run["trace_id"], up_trace, "{run}");
    assert_eq!(
        run["parent_span_id"],
        span_of(UP_UNSAMPLED),
        "the caller's span is the run's parent: {run}"
    );
    // The response names the span the task's work is done in: the run's.
    let named = reply
        .header("traceparent")
        .unwrap_or_else(|| panic!("no traceparent response header: {reply:?}"));
    assert_eq!(
        named,
        format!("00-{up_trace}-{}-00", run["span_id"].as_str().unwrap()),
        "{reply:?}"
    );
    // So does a read of the task.
    let read = common::a2a_post(
        &addr,
        &common::rpc_body(2, "GetTask", json!({"id": task})),
        &[],
    );
    assert_eq!(read.header("traceparent"), Some(named), "{read:?}");
    b.wait_line("run.done", |v| v["run"] == run_id.as_str());
    // The run's MCP call: the same trace, still unsampled, the state kept.
    for (tp, ts) in tool_call_traces(&mock, &run_id) {
        assert_eq!(trace_of(&tp), up_trace, "{tp}");
        assert!(tp.ends_with("-00"), "an unsampled caller stays so: {tp}");
        assert_eq!(ts.as_deref(), Some(STATE));
    }
    // The audit record of the call is in the task's trace.
    let audit = b.wait_line("audit", |v| v["target"]["task"] == task.as_str());
    assert_eq!(audit["trace"], up_trace, "{audit}");
    // The push about the task is a span of its own in the task's trace.
    let (headers, body) = hook.first();
    assert_eq!(body["task"]["id"], task.as_str(), "{body}");
    let pushed = header(&headers, "traceparent").expect("the push names its span");
    assert_eq!(trace_of(pushed), up_trace, "{headers:?}");
    assert!(pushed.ends_with("-00"), "{pushed}");
    assert_eq!(header(&headers, "tracestate"), Some(STATE), "{headers:?}");

    // 2. A malformed header is ignored: a trace of agentd's own.
    let reply = post(
        &addr,
        &run_work().header("traceparent", "00-not-a-trace-01"),
        None,
    );
    assert_eq!(reply.status, 200, "{reply:?}");
    let run = b.wait_line("run.start", |v| {
        v["workflow"] == "work" && v["run"] != run_id.as_str()
    });
    let fresh = run["trace_id"].as_str().unwrap();
    assert_eq!(fresh.len(), 32, "{run}");
    assert_ne!(fresh, up_trace, "{run}");
    assert!(run.get("parent_span_id").is_none(), "a new trace: {run}");
    let second = run["run"].as_str().unwrap().to_string();

    // 3. No header, the span in the message's metadata.
    let reply = post(
        &addr,
        &run_work().metadata(json!({"traceparent": UP, "tracestate": "k=v"})),
        None,
    );
    assert_eq!(reply.status, 200, "{reply:?}");
    let run = b.wait_line("run.start", |v| {
        v["workflow"] == "work" && v["run"] != run_id.as_str() && v["run"] != second.as_str()
    });
    assert_eq!(run["trace_id"], trace_of(UP), "{run}");
    assert_eq!(run["parent_span_id"], span_of(UP), "{run}");
}

/// Two agentd daemons over A2A are one trace: the caller's run delegates in a
/// span of its own, logged on its `a2a.delegate` line and sent on every
/// request; the callee's task and run continue that span — the run its child —
/// and the callee's MCP call carries the caller's trace.
#[test]
fn two_daemons_over_a2a_are_one_trace() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let (b, b_addr) = spawn_on("trace-callee", "a2a.listen", |port| {
        callee_config(port, &mock.uri())
    });
    let a = spawn(
        "trace-caller",
        &format!(
            "\
             agent:\n  name: caller\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  peers:\n    - name: b\n      endpoint: http://{b_addr}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n\
             workflows:\n\
             \x20 - name: ask\n\
             \x20   steps:\n\
             \x20     s: {{kind: once, policy: always}}\n\
             \x20     d: {{kind: a2a.delegate, depends_on: [s], peer: b, command: workflow.run,\n\
             \x20         args: {{workflow: work}}, timeout: 30s}}\n\
             \x20     f: {{kind: finish, depends_on: [d], status: completed}}\n"
        ),
    );
    let done = a.wait_line("run.done", |v| v["workflow"] == "ask");
    assert_eq!(done["status"], "completed", "{done}\n{}", b.log());
    let started = a.wait_line("run.start", |v| v["workflow"] == "ask");
    let trace = started["trace_id"].as_str().unwrap().to_string();
    // The hop: a span of the run's, on its own line.
    let hop = a.wait_line("a2a.delegate", |v| v["step"] == "d");
    assert_eq!(hop["trace_id"], trace.as_str(), "{hop}");
    assert_eq!(hop["parent_span_id"], started["span_id"], "{hop}");
    let hop_span = hop["span_id"].as_str().unwrap();
    assert_ne!(hop_span, started["span_id"].as_str().unwrap());

    // The callee's run: the caller's trace, a child of the caller's hop.
    let run = b.wait_line("run.start", |v| v["workflow"] == "work");
    assert_eq!(run["trace_id"], trace.as_str(), "{run}");
    assert_eq!(run["parent_span_id"], hop_span, "{run}");
    // Its task, as the audit of the request records it.
    let audit = b.wait_line("audit", |v| {
        v["action"] == "a2a.SendMessage:workflow.run" && v["target"]["task"].is_string()
    });
    assert_eq!(audit["trace"], trace.as_str(), "{audit}");
    // And its MCP call.
    let run_id = run["run"].as_str().unwrap();
    for (tp, _) in tool_call_traces(&mock, run_id) {
        assert_eq!(trace_of(&tp), trace, "{tp}");
    }
}

/// A peer's reply that wakes a waiting run resumes it; it did not cause it.
/// The run keeps its own trace, and its `wait.resolved` line names as `link`
/// the span the reply was accepted in — which is in the sender's trace, a
/// child of the span the sender logged on its `a2a.send` line, on the
/// `a2a.message.delivered` line that names the run.
#[test]
fn a_reply_that_wakes_an_a2a_wait_is_a_link() {
    let (b, b_addr) = spawn_on("trace-awaiter", "a2a.listen", |port| {
        format!(
            "\
             agent:\n  name: awaiter\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n\
             workflows:\n\
             \x20 - name: awaiter\n\
             \x20   steps:\n\
             \x20     go:    {{kind: once}}\n\
             \x20     reply: {{kind: a2a.wait, depends_on: [go], conversation: \"conv-t\", from: operator, timeout: 10m}}\n\
             \x20     fin:   {{kind: finish, depends_on: [reply], status: completed}}\n"
        )
    });
    // Parked: the step started, and it suspends on the loop that would take
    // the reply.
    b.wait_line("step.start", |v| v["step"] == "reply");
    let waiting = b.wait_line("run.start", |v| v["workflow"] == "awaiter");
    let own = waiting["trace_id"].as_str().unwrap().to_string();
    let run_id = waiting["run"].as_str().unwrap().to_string();

    let a = spawn(
        "trace-replier",
        &format!(
            "\
             agent:\n  name: replier\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  peers:\n    - name: b\n      endpoint: http://{b_addr}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n\
             workflows:\n\
             \x20 - name: teller\n\
             \x20   steps:\n\
             \x20     s:    {{kind: once, policy: always}}\n\
             \x20     tell: {{kind: a2a.send, depends_on: [s], to: b, parts: \"done\", context: \"conv-t\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [tell], status: completed}}\n"
        ),
    );
    let hop = a.wait_line("a2a.send", |v| v["step"] == "tell");
    let sender = hop["trace_id"].as_str().unwrap().to_string();
    assert_ne!(sender, own);

    // The message, accepted in the sender's trace under its hop, woke the run.
    let delivered = b.wait_line("a2a.message.delivered", |_| true);
    assert_eq!(delivered["trace_id"], sender.as_str(), "{delivered}");
    assert_eq!(delivered["parent_span_id"], hop["span_id"], "{delivered}");
    assert_eq!(delivered["runs"], json!([run_id]), "{delivered}");
    // The run: its own trace, the message's span as the link.
    let resolved = b.wait_line("wait.resolved", |v| v["step"] == "reply");
    assert_eq!(resolved["kind"], "message", "{resolved}");
    assert_eq!(resolved["trace_id"], own.as_str(), "{resolved}");
    assert_eq!(
        resolved["link"],
        format!("00-{sender}-{}-01", delivered["span_id"].as_str().unwrap()),
        "{resolved}"
    );
    let done = b.wait_line("run.done", |v| v["workflow"] == "awaiter");
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(
        done["trace_id"],
        own.as_str(),
        "the run kept its trace: {done}"
    );
}

/// A message that continues a task carries on in the task's trace; the
/// caller's span, from another trace, is a link — on the `a2a.task.continued`
/// line and on the audit record of the call — rather than dropped.
#[test]
fn a_message_continuing_a_task_links_its_caller() {
    let (b, addr) = spawn_on("trace-continue", "a2a.listen", |port| {
        format!(
            "\
             agent:\n  name: holder\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n  audit:\n    sink: [log]\n\
             workflows:\n\
             \x20 - name: hold\n\
             \x20   steps:\n\
             \x20     s: {{kind: manual}}\n\
             \x20     w: {{kind: wait, on: signal, signal: never, depends_on: [s], timeout: 10m}}\n\
             \x20     f: {{kind: finish, depends_on: [w], status: completed}}\n"
        )
    });
    // A task in the caller's trace, still working.
    let open = common::SendMessage::command("workflow.run", json!({"workflow": "hold"}))
        .return_immediately()
        .header("traceparent", UP);
    let reply = post(&addr, &open, None);
    assert_eq!(reply.status, 200, "{reply:?}");
    let task = reply.json()["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a task: {}", reply.body))
        .to_string();
    b.wait_line("step.start", |v| v["step"] == "w");

    // Another caller, in another trace, continues it.
    let more = common::SendMessage::text("and another thing")
        .task(&task)
        .return_immediately()
        .header("traceparent", UP_UNSAMPLED);
    let reply = post(&addr, &more, None);
    assert_eq!(reply.status, 200, "{reply:?}");
    let continued = b.wait_line("a2a.task.continued", |v| v["task"] == task.as_str());
    assert_eq!(continued["trace_id"], trace_of(UP), "{continued}");
    assert_eq!(continued["link"], UP_UNSAMPLED, "{continued}");
    let id = more.params()["message"]["messageId"].clone();
    let audit = b.wait_line("audit", |v| v["request_id"] == id);
    assert_eq!(audit["trace"], trace_of(UP), "{audit}");
    assert_eq!(audit["link"], UP_UNSAMPLED, "{audit}");
    // The call that opened the task was in the caller's trace: nothing to link.
    let id = open.params()["message"]["messageId"].clone();
    let audit = b.wait_line("audit", |v| v["request_id"] == id);
    assert_eq!(audit["trace"], trace_of(UP), "{audit}");
    assert!(audit.get("link").is_none(), "{audit}");
}

fn webhook_config(port: u16) -> String {
    format!(
        "\
         agent:\n  name: hooks\n  instruction: test\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
         store:\n  kind: memory\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n\
         workflows:\n\
         \x20 - name: hooked\n\
         \x20   steps:\n\
         \x20     h: {{kind: webhook, path: /hooks/in, methods: [POST], auth: {{none: true}}}}\n\
         \x20     f: {{kind: finish, depends_on: [h], status: completed}}\n\
         \x20 - name: waiter\n\
         \x20   steps:\n\
         \x20     s: {{kind: once}}\n\
         \x20     w: {{kind: wait, on: webhook, webhook: {{path: /hooks/cb}}, depends_on: [s], timeout: 30s}}\n\
         \x20     f: {{kind: finish, depends_on: [w], status: completed}}\n"
    )
}

/// POST `{}` to `path` with `headers`; the status.
fn webhook_post(addr: &str, path: &str, headers: &[(&str, &str)]) -> u16 {
    let mut s = TcpStream::connect(addr).expect("connect webhook listener");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n"
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n{}");
    s.write_all(head.as_bytes()).unwrap();
    let mut status = String::new();
    BufReader::new(s).read_line(&mut status).unwrap();
    status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

/// A webhook that starts a run is the run's parent: the run continues the
/// sender's trace. A callback to a run that waits does not move the run: it
/// keeps its own trace and records the sender's span as a link.
#[test]
fn a_webhook_starts_a_run_in_its_trace_and_a_callback_is_a_link() {
    let (d, addr) = spawn_on("trace-hooks", "webhooks.listen", webhook_config);

    assert_eq!(
        webhook_post(
            &addr,
            "/hooks/in",
            &[("traceparent", UP), ("tracestate", "k=v")]
        ),
        202
    );
    let run = d.wait_line("run.start", |v| v["workflow"] == "hooked");
    assert_eq!(run["trace_id"], trace_of(UP), "{run}");
    assert_eq!(run["parent_span_id"], span_of(UP), "{run}");

    // The waiting run, in a trace of its own.
    d.wait_line("webhook.await.armed", |v| v["path"] == "/hooks/cb");
    let waiting = d.wait_line("run.start", |v| v["workflow"] == "waiter");
    let own = waiting["trace_id"].as_str().unwrap().to_string();
    assert_ne!(own, trace_of(UP_UNSAMPLED));
    assert_eq!(
        webhook_post(&addr, "/hooks/cb", &[("traceparent", UP_UNSAMPLED)]),
        200
    );
    let delivered = d.wait_line("signal.delivered", |v| v["step"] == "w");
    assert_eq!(delivered["trace_id"], own.as_str(), "{delivered}");
    assert_eq!(delivered["link"], UP_UNSAMPLED, "{delivered}");
    let callback = d.wait_line("webhook.callback", |_| true);
    assert_eq!(callback["link"], UP_UNSAMPLED, "{callback}");
    let done = d.wait_line("run.done", |v| v["workflow"] == "waiter");
    assert_eq!(
        done["trace_id"],
        own.as_str(),
        "the run kept its trace: {done}"
    );
}

/// An emitted event carries the span it was emitted in: a `stream` start's
/// run continues it, a child of the producer's `stream.emit` span. A `batch`
/// run has several causes and no one parent: it starts a trace of its own and
/// links to every member's span.
#[test]
fn a_stream_consumer_continues_its_event_and_a_batch_links_its_members() {
    let d = spawn(
        "trace-streams",
        "\
         agent:\n  name: streams\n  instruction: test\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
         store:\n  kind: memory\n\
         streams:\n  orders: {}\n  bulk: {}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n\
         workflows:\n\
         \x20 - name: producer\n\
         \x20   steps:\n\
         \x20     s: {kind: once, policy: always}\n\
         \x20     e: {kind: emit, depends_on: [s], stream: orders, subject: order.placed, data: {n: 1}}\n\
         \x20     f: {kind: finish, depends_on: [e], status: completed}\n\
         \x20 - name: consumer\n\
         \x20   steps:\n\
         \x20     t: {kind: stream, stream: orders, subject: \"order.*\", from: earliest}\n\
         \x20     f: {kind: finish, depends_on: [t], status: completed}\n\
         \x20 - name: bulk-a\n\
         \x20   steps:\n\
         \x20     s: {kind: once, policy: always}\n\
         \x20     e: {kind: emit, depends_on: [s], stream: bulk, subject: item, data: {n: 1}}\n\
         \x20     f: {kind: finish, depends_on: [e], status: completed}\n\
         \x20 - name: bulk-b\n\
         \x20   steps:\n\
         \x20     s: {kind: once, policy: always}\n\
         \x20     e: {kind: emit, depends_on: [s], stream: bulk, subject: item, data: {n: 2}}\n\
         \x20     f: {kind: finish, depends_on: [e], status: completed}\n\
         \x20 - name: batcher\n\
         \x20   steps:\n\
         \x20     t: {kind: stream, stream: bulk, from: earliest, batch: {size: 2, window: 30s}}\n\
         \x20     f: {kind: finish, depends_on: [t], status: completed}\n",
    );
    let emit = |stream: &str| {
        d.wait_line("stream.emit", |v| {
            v["stream"] == stream && v["seq"].is_u64()
        })
    };
    let produced = emit("orders");
    let consumed = d.wait_line("run.start", |v| v["workflow"] == "consumer");
    assert_eq!(consumed["trace_id"], produced["trace_id"], "{consumed}");
    assert_eq!(
        consumed["parent_span_id"], produced["span_id"],
        "the run is a child of the emit: {consumed}\n{produced}"
    );

    // The batch: both members, each in its producer's trace.
    let fired = d.wait_line("start.fired", |v| v["workflow"] == "batcher");
    let members: Vec<String> = d
        .events("stream.emit")
        .into_iter()
        .filter(|v| v["stream"] == "bulk")
        .map(|v| {
            format!(
                "00-{}-{}-01",
                v["trace_id"].as_str().unwrap(),
                v["span_id"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(members.len(), 2, "{}", d.log());
    let mut links: Vec<String> = fired["links"]
        .as_array()
        .unwrap_or_else(|| panic!("no links: {fired}"))
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect();
    let mut want = members.clone();
    links.sort();
    want.sort();
    assert_eq!(links, want, "{fired}");
    for m in &members {
        assert_ne!(
            fired["trace_id"],
            trace_of(m),
            "a trace of its own: {fired}"
        );
    }
    assert!(fired.get("parent_span_id").is_none(), "{fired}");
}

/// The `http` step sends the step's span, a child of the run's, as
/// `traceparent`: the API it reaches joins the run's trace. A step whose
/// headers set the trace context owns it: agentd adds nothing — no second
/// `tracestate` — and logs the request in the run's span, since none of its
/// own went out.
#[test]
fn the_http_step_sends_its_span() {
    let api = Capture::start();
    let d = spawn(
        "trace-http",
        &format!(
            "\
             agent:\n  name: http\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n\
             workflows:\n\
             \x20 - name: caller\n\
             \x20   steps:\n\
             \x20     s: {{kind: once, policy: always}}\n\
             \x20     h: {{kind: http, depends_on: [s], method: POST, url: \"http://{0}/x\",\n\
             \x20         json: {{a: 1}}, allow_private: true}}\n\
             \x20     o: {{kind: http, depends_on: [h], method: POST, url: \"http://{0}/x\",\n\
             \x20         headers: {{TraceState: \"k=v\"}}, json: {{a: 2}}, allow_private: true}}\n\
             \x20     f: {{kind: finish, depends_on: [o], status: completed}}\n",
            api.addr
        ),
    );
    let (headers, _) = api.find(|(_, body)| body["a"] == 1);
    let sent = header(&headers, "traceparent")
        .unwrap_or_else(|| panic!("no traceparent: {headers:?}"))
        .to_string();
    let line = d.wait_line("http.request", |v| v["step"] == "h");
    let run = d.wait_line("run.start", |v| v["workflow"] == "caller");
    assert_eq!(trace_of(&sent), run["trace_id"].as_str().unwrap(), "{sent}");
    assert_eq!(span_of(&sent), line["span_id"].as_str().unwrap(), "{line}");
    assert_eq!(line["parent_span_id"], run["span_id"], "{line}");

    let (headers, _) = api.find(|(_, body)| body["a"] == 2);
    assert_eq!(header(&headers, "traceparent"), None, "{headers:?}");
    let states: Vec<&str> = headers
        .iter()
        .filter(|(k, _)| k == "tracestate")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(states, ["k=v"], "{headers:?}");
    let line = d.wait_line("http.request", |v| v["step"] == "o");
    assert_eq!(line["span_id"], run["span_id"], "{line}");
}

/// An MCP Events occurrence names the span the server produced it in, in its
/// `_meta`: the event keeps it, and the run a `stream` start fires for it is
/// a child of that span.
#[cfg(feature = "mcp-events")]
#[test]
fn an_mcp_events_occurrence_starts_its_run_in_its_trace() {
    let mock = common::spawn_mock_mcp_at("2026-07-28", "mock://res", false);
    let d = spawn(
        "trace-events",
        &format!(
            "\
             agent:\n  name: events\n\
             store:\n  kind: memory\n\
             streams:\n  ticks: {{}}\n\
             mcp:\n  servers:\n    - name: mock\n      endpoint: {}/mcp\n      tags: {{\"*\": [sensitive]}}\n      events:\n\
             \x20       - name: mock.tick\n\
             \x20         arguments: {{topic: t1}}\n\
             \x20         delivery: [poll]\n\
             \x20         into: {{stream: ticks, subject: tick}}\n\
             workflows:\n\
             \x20 - name: on_tick\n\
             \x20   steps:\n\
             \x20     s: {{kind: stream, stream: ticks, subject: tick}}\n\
             \x20     f: {{kind: finish, depends_on: [s], status: completed}}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n",
            mock.uri()
        ),
    );
    d.wait_line("mcp.events.armed", |v| v["event_type"] == "mock.tick");
    // The first poll asks from now: an occurrence before it is never read.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !mock.log().contains("MOCK_EVENTS_POLL ") {
        assert!(Instant::now() < deadline, "no poll: {}", mock.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    mock.control(
        "mock.event",
        json!({"topic": "t1", "count": 1, "traceparent": UP, "tracestate": "k=v"}),
    );
    let run = d.wait_line("run.start", |v| v["workflow"] == "on_tick");
    assert_eq!(run["trace_id"], trace_of(UP), "{run}");
    assert_eq!(run["parent_span_id"], span_of(UP), "{run}");
}
