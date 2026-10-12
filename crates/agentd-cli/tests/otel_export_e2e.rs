// SPDX-License-Identifier: AGPL-3.0-only
//! OTLP export to an `https://` collector, and what a collector that fails
//! costs the log.
//!
//! - A loopback TLS collector with the test PKI (its CA given as
//!   `security.tls_ca`) receives the turn's trace on `/v1/traces` and the
//!   supervisor's log lines on `/v1/logs`. It used to receive nothing: every
//!   export refused an `https://` endpoint, and the refusal was discarded.
//! - A collector that drops every connection is named once per signal in a
//!   short window — `otel.export.failed` with the signal, the endpoint and the
//!   error — not once a batch.
//! - Under an upstream `traceparent` that is not sampled (flags `00`), no
//!   span is exported: the upstream's decision stands. The log lines still
//!   go.
//! - A job's spans are one tree: the process, its run, each step, the agent
//!   step's worker with its model and tool calls, and the CLIENT span of each
//!   request — whose ids are the span ids each request carried. The log
//!   records name the span their line is in.
//! - A `batch` run's span links the spans of the events it consumed, at any
//!   log level.
//! - A command the `exec` tool runs is the span its `TRACEPARENT` named; a
//!   stream forward's span names where it went, not its URL.
//! - A run an `a2a` start fires is under its message's SERVER span.
#![cfg(all(unix, feature = "otel", feature = "tls"))]

mod common;

use agentd::net::tls::{ServerIdentity, TlsAcceptor};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn fixtures() -> String {
    format!("{}/../net/tests/fixtures", env!("CARGO_MANIFEST_DIR"))
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// One request the collector took: its path and its JSON body.
struct Post {
    path: String,
    body: Value,
}

/// A loopback OTLP collector over TLS: answers every request `200 {}` and
/// hands it to the test.
fn spawn_collector() -> (u16, mpsc::Receiver<Post>) {
    let fx = fixtures();
    let identity = ServerIdentity::from_pem(
        &std::fs::read(format!("{fx}/server.pem")).unwrap(),
        &std::fs::read(format!("{fx}/server.key")).unwrap(),
    )
    .unwrap();
    let acceptor = Arc::new(TlsAcceptor::new(identity, None).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let (acceptor, tx) = (acceptor.clone(), tx.clone());
            std::thread::spawn(move || {
                tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
                let Ok(tls) = acceptor.accept(tcp) else {
                    return;
                };
                let mut r = BufReader::new(tls);
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    return;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 {
                        return;
                    }
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                if r.read_exact(&mut body).is_err() {
                    return;
                }
                let mut tls = r.into_inner();
                let _ = tls.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                );
                let _ = tls.flush();
                let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let _ = tx.send(Post { path, body });
            });
        }
    });
    (port, rx)
}

/// A collector that takes each connection and drops it: every export to it
/// fails. Returns its port and the count of connections it has taken.
fn spawn_dropping_collector() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let taken = Arc::new(AtomicUsize::new(0));
    let count = taken.clone();
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            drop(tcp);
        }
    });
    (port, taken)
}

/// agentd answering `--prompt` with the mock model, then staying up (a
/// daemon, so the 5 s log flush runs) until it is stopped.
struct Daemon {
    child: Child,
    cfg_path: String,
    err_path: String,
}

impl Daemon {
    fn start(tag: &str, endpoint: &str) -> Daemon {
        Daemon::start_in(tag, endpoint, &[])
    }

    /// [`Daemon::start`] with `args` added to the command line.
    fn start_in(tag: &str, endpoint: &str, args: &[&str]) -> Daemon {
        let cfg_path = common::unique_path(tag, "yaml");
        std::fs::write(
            &cfg_path,
            format!(
                "store: {{kind: memory}}\nlifecycle: {{run_until: drained}}\nsecurity: {{tls_ca: {}/ca.pem}}\nobservability:\n  otel: {{endpoint: \"{endpoint}\", logs: true}}\n",
                fixtures()
            ),
        )
        .unwrap();
        let err_path = common::unique_path(tag, "err");
        let err = std::fs::File::create(&err_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args([
                "--config",
                &cfg_path,
                "--intelligence",
                "mock:final",
                "--model",
                "m",
                "--prompt",
                "say hello",
            ])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn agentd");
        Daemon {
            child,
            cfg_path,
            err_path,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.cfg_path);
        let _ = std::fs::remove_file(&self.err_path);
    }
}

#[test]
fn an_https_collector_receives_the_turn_trace_and_the_log_lines() {
    let (port, posts) = spawn_collector();
    let daemon = Daemon::start("otel-https", &format!("https://127.0.0.1:{port}"));
    let (mut spans, mut records) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(30);
    while spans.is_empty() || records.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(post) = posts.recv_timeout(left) else {
            panic!(
                "the collector got spans {spans:?} and log records {records:?} in 30 s:\n{}",
                daemon.log()
            );
        };
        match post.path.as_str() {
            "/v1/traces" => spans.extend(
                post.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            ),
            "/v1/logs" => records.extend(
                post.body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            ),
            other => panic!("a POST to {other}"),
        }
    }
    let names: Vec<&str> = spans.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"invoke_agent"), "{names:?}");
    assert!(names.contains(&"chat"), "{names:?}");
    let bodies: Vec<&str> = records
        .iter()
        .filter_map(|r| r["body"]["stringValue"].as_str())
        .collect();
    assert!(bodies.contains(&"otel.logs.armed"), "{bodies:?}");
    let log = daemon.log();
    assert!(events(&log, "otel.export.failed").is_empty(), "{log}");
}

#[test]
fn a_failing_collector_is_named_once_not_once_a_batch() {
    let (port, taken) = spawn_dropping_collector();
    let endpoint = format!("https://127.0.0.1:{port}");
    let daemon = Daemon::start("otel-down", &endpoint);
    // The turn's trace, then two log flushes 5 s apart — the second carries
    // the line the first failure wrote.
    let deadline = Instant::now() + Duration::from_secs(30);
    while taken.load(Ordering::SeqCst) < 3 {
        assert!(
            Instant::now() < deadline,
            "{} exports in 30 s:\n{}",
            taken.load(Ordering::SeqCst),
            daemon.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Give a line the last failure might still be writing time to land.
    std::thread::sleep(Duration::from_millis(500));
    let log = daemon.log();
    let failed = events(&log, "otel.export.failed");
    // Each process that exports names it once: the turn worker its trace,
    // the supervisor its own spans and its log lines.
    for signal in ["traces", "logs"] {
        let lines: Vec<&Value> = failed.iter().filter(|e| e["signal"] == signal).collect();
        assert!(!lines.is_empty(), "{signal}:\n{log}");
        let mut pids: Vec<u64> = lines.iter().filter_map(|l| l["pid"].as_u64()).collect();
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(
            pids.len(),
            lines.len(),
            "once per process: {signal}:\n{log}"
        );
        for line in &lines {
            assert_eq!(line["level"], "warn");
            assert_eq!(line["endpoint"], endpoint.as_str());
            assert_eq!(line["failures"], 1);
            assert!(
                line["err"]
                    .as_str()
                    .is_some_and(|e| e.contains(&format!("127.0.0.1:{port}/v1/{signal}"))),
                "{log}"
            );
        }
    }
    let logs = failed.iter().filter(|e| e["signal"] == "logs").count();
    assert_eq!(logs, 1, "only the supervisor exports log lines:\n{log}");
}

/// An upstream that did not sample is not overruled: the turn under its
/// `traceparent` (flags `00`) exports no span, while the log lines, its own
/// included, reach the collector.
#[test]
fn an_unsampled_upstream_trace_exports_no_spans() {
    let (port, posts) = spawn_collector();
    let daemon = Daemon::start_in(
        "otel-unsampled",
        &format!("https://127.0.0.1:{port}"),
        &[
            "--traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
        ],
    );
    // A turn exports its spans before it exits, so by the time a log batch
    // carries the supervisor's `child.exit` for it, they would have arrived.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut exited = false;
    while !exited {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(post) = posts.recv_timeout(left) else {
            panic!(
                "no log batch with the turn's exit in 30 s:\n{}",
                daemon.log()
            );
        };
        match post.path.as_str() {
            "/v1/traces" => panic!(
                "an unsampled trace exported spans: {}\n{}",
                post.body,
                daemon.log()
            ),
            "/v1/logs" => {
                exited = post.body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|r| r["body"]["stringValue"] == "child.exit"))
            }
            other => panic!("a POST to {other}"),
        }
    }
    while let Ok(post) = posts.recv_timeout(Duration::from_millis(500)) {
        assert_ne!(post.path, "/v1/traces", "{}", post.body);
    }
    // The turn ran, in the upstream's trace.
    let log = daemon.log();
    let done = events(&log, "turn.done");
    assert!(!done.is_empty(), "{log}");
    for e in done {
        assert_eq!(e["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736", "{log}");
    }
}

/// The mock LLM with a `file:` playbook; killed on drop.
struct MockLlm {
    child: Child,
    addr_file: String,
    log_path: String,
    uri: String,
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
        let _ = std::fs::remove_file(&self.log_path);
    }
}

impl MockLlm {
    fn start(playbook: &Value) -> MockLlm {
        let pb = common::unique_path("otel-playbook", "json");
        std::fs::write(&pb, playbook.to_string()).unwrap();
        let addr_file = common::unique_path("otel-llm", "addr");
        let log_path = common::unique_path("otel-llm", "log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log_path).expect("mock llm log"))
            .spawn()
            .expect("spawn mock llm");
        let addr = common::read_addr_file(&addr_file);
        MockLlm {
            child,
            addr_file,
            log_path,
            uri: format!("http://{addr}"),
        }
    }

    /// The `traceparent` of every model call that reached the mock.
    fn traceparents(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("MOCK_LLM_TRACE "))
            .filter_map(|t| serde_json::from_str::<Value>(t).ok())
            .filter_map(|t| t["traceparent"].as_str().map(str::to_string))
            .collect()
    }
}

/// A loopback HTTP server that answers `200 {}` and keeps each request's
/// `traceparent`.
fn spawn_api() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let out = Arc::clone(&seen);
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut w = tcp.try_clone().unwrap();
            let mut r = BufReader::new(tcp);
            let mut len = 0usize;
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    let k = k.trim().to_ascii_lowercase();
                    if k == "content-length" {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if k == "traceparent" {
                        out.lock().unwrap().push(v.trim().to_string());
                    }
                }
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            let _ = w.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            );
        }
    });
    (addr, seen)
}

/// Every span and every log record the collector took until `posts` has been
/// quiet for `quiet`.
fn drain(posts: &mpsc::Receiver<Post>, quiet: Duration) -> (Vec<Value>, Vec<Value>) {
    let (mut spans, mut records) = (Vec::new(), Vec::new());
    while let Ok(post) = posts.recv_timeout(quiet) {
        match post.path.as_str() {
            "/v1/traces" => spans.extend(
                post.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            ),
            "/v1/logs" => records.extend(
                post.body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            ),
            other => panic!("a POST to {other}"),
        }
    }
    (spans, records)
}

/// The span id a `traceparent` names.
fn span_of(tp: &str) -> &str {
    &tp[36..52]
}

/// The one exported span named `name`.
fn named<'a>(spans: &'a [Value], name: &str) -> &'a Value {
    let found: Vec<&Value> = spans.iter().filter(|s| s["name"] == name).collect();
    assert_eq!(found.len(), 1, "{name}: {spans:#?}");
    found[0]
}

/// One job's exported spans are one tree, and each span's id is the one that
/// span has everywhere else. The process span is the root; the run is under
/// it, each step attempt under the run; the agent step's worker exports its
/// `invoke_agent` under the step's span, with its model call (`chat`) and its
/// MCP tool call (`execute_tool`) under it — CLIENT spans whose ids are the
/// span ids the model and the MCP server received. The `mcp.tool` step's
/// `tools/call` and the `http` step's request are CLIENT spans under their
/// steps, with the ids the server and the API received. The log records name
/// the span their line is in.
#[test]
fn a_job_s_spans_are_one_tree_whose_ids_are_the_wire_s() {
    let (port, posts) = spawn_collector();
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let (api, sent) = spawn_api();
    let llm = MockLlm::start(&json!({"turns": [
        {"tool_calls": [{"name": "mock.ops", "arguments": {}}]},
        {"content": "done"}
    ]}));
    let cfg = common::unique_path("otel-tree", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "\
             agent:\n  name: tree\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: {llm}\n  model: mock\n\
             mcp:\n  servers:\n    - name: mock\n      endpoint: {mcp}\n\
             store:\n  kind: memory\n\
             security:\n  tls_ca: {fx}/ca.pem\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 500ms\n\
             observability:\n  log_level: info\n  otel: {{endpoint: \"https://127.0.0.1:{port}\", logs: true}}\n\
             workflows:\n\
             \x20 - name: tree\n\
             \x20   steps:\n\
             \x20     s: {{kind: once}}\n\
             \x20     probe: {{kind: mcp.tool, depends_on: [s], server: mock, tool: state.list, args: {{key: k}}}}\n\
             \x20     work: {{kind: agent, depends_on: [probe], instruction: call the tool}}\n\
             \x20     h: {{kind: http, depends_on: [work], method: POST, url: \"http://{api}/x\", json: {{a: 1}}, allow_private: true}}\n\
             \x20     f: {{kind: finish, depends_on: [h], status: completed}}\n",
            llm = llm.uri,
            mcp = mock.uri(),
            fx = fixtures(),
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let _ = std::fs::remove_file(&cfg);
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "{log}");
    // The process exports what it holds before it exits.
    let (spans, records) = drain(&posts, Duration::from_secs(2));

    // One trace, one root: every other span's parent was exported too.
    let ids: Vec<&str> = spans.iter().filter_map(|s| s["spanId"].as_str()).collect();
    let roots: Vec<&Value> = spans
        .iter()
        .filter(|s| s["parentSpanId"].as_str().is_none_or(|p| !ids.contains(&p)))
        .collect();
    assert_eq!(roots.len(), 1, "one root: {roots:#?}\n{log}");
    let root = roots[0];
    assert_eq!(root["name"], "agentd", "the process is the root: {root}");
    for s in &spans {
        assert_eq!(s["traceId"], root["traceId"], "one trace: {s}");
    }
    let id = |s: &Value| s["spanId"].as_str().unwrap().to_string();
    let parent = |s: &Value| s["parentSpanId"].as_str().unwrap_or_default().to_string();

    let run = named(&spans, "run tree");
    assert_eq!(parent(run), id(root), "{run}");
    for step in ["probe", "work", "h"] {
        let s = named(&spans, &format!("step {step}"));
        assert_eq!(parent(s), id(run), "{s}");
        assert_eq!(s["kind"], 1, "INTERNAL: {s}");
    }
    let work = named(&spans, "step work");
    let invoke = named(&spans, "invoke_agent");
    assert_eq!(
        parent(invoke),
        id(work),
        "the worker is under its step: {invoke}"
    );

    // The model call: its span is the one the model received.
    let chats: Vec<&Value> = spans.iter().filter(|s| s["name"] == "chat").collect();
    let received: Vec<String> = llm
        .traceparents()
        .iter()
        .map(|tp| span_of(tp).to_string())
        .collect();
    assert_eq!(chats.len(), received.len(), "{chats:#?}\n{received:?}");
    for c in &chats {
        assert_eq!(parent(c), id(invoke), "{c}");
        assert_eq!(c["kind"], 3, "CLIENT: {c}");
        assert!(received.contains(&id(c)), "{c} not in {received:?}");
    }

    // The MCP calls: the turn's tool call and the step's, each the span the
    // server received.
    let calls: Vec<Value> = mock
        .log()
        .lines()
        .filter_map(|l| l.strip_prefix("MOCK_CALL "))
        .filter_map(|p| serde_json::from_str(p).ok())
        .collect();
    let wire = |tool: &str| -> String {
        let call = calls
            .iter()
            .find(|c| c["name"] == tool)
            .unwrap_or_else(|| panic!("no {tool} call: {calls:?}"));
        span_of(call["_meta"]["traceparent"].as_str().unwrap()).to_string()
    };
    let tool = named(&spans, "execute_tool");
    assert_eq!(parent(tool), id(invoke), "{tool}");
    assert_eq!(tool["kind"], 3, "CLIENT: {tool}");
    assert_eq!(id(tool), wire("mock.ops"), "{tool}");
    let probe = named(&spans, "tools/call state.list");
    assert_eq!(parent(probe), id(named(&spans, "step probe")), "{probe}");
    assert_eq!(probe["kind"], 3, "CLIENT: {probe}");
    assert_eq!(id(probe), wire("state.list"), "{probe}");
    // Every request the server was sent names an exported span: a
    // connection's own — its handshake, its lists — the span of the process
    // that made it.
    for m in mock.metas() {
        if let Some(tp) = m["_meta"]["traceparent"].as_str() {
            assert!(ids.contains(&span_of(tp)), "under no exported span: {m}");
        }
    }

    // The http step's request: the span the API received.
    let request = named(&spans, "POST");
    assert_eq!(parent(request), id(named(&spans, "step h")), "{request}");
    assert_eq!(request["kind"], 3, "CLIENT: {request}");
    let got = sent.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(id(request), span_of(&got[0]), "{request}");

    // The log records name the span their line is in.
    let started = records
        .iter()
        .find(|r| r["body"]["stringValue"] == "run.start")
        .unwrap_or_else(|| panic!("no run.start record: {records:#?}"));
    assert_eq!(started["traceId"], run["traceId"], "{started}");
    assert_eq!(started["spanId"], run["spanId"], "{started}");
}

/// A `batch` run's span links every member event's: the PRODUCER spans of
/// the emits that put them on the stream. At `log_level: warn`, so the links
/// do not hang on the `start.fired` line that names them being written.
#[test]
fn a_batch_run_s_span_links_its_members() {
    let (port, posts) = spawn_collector();
    let cfg = common::unique_path("otel-batch", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "\
             agent:\n  name: batch\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             streams:\n  bulk: {{}}\n\
             security:\n  tls_ca: {fx}/ca.pem\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: warn\n  otel: {{endpoint: \"https://127.0.0.1:{port}\"}}\n\
             workflows:\n\
             \x20 - name: bulk-a\n\
             \x20   steps:\n\
             \x20     s: {{kind: once, policy: always}}\n\
             \x20     e: {{kind: emit, depends_on: [s], stream: bulk, subject: item, data: {{n: 1}}}}\n\
             \x20     f: {{kind: finish, depends_on: [e], status: completed}}\n\
             \x20 - name: bulk-b\n\
             \x20   steps:\n\
             \x20     s: {{kind: once, policy: always}}\n\
             \x20     e: {{kind: emit, depends_on: [s], stream: bulk, subject: item, data: {{n: 2}}}}\n\
             \x20     f: {{kind: finish, depends_on: [e], status: completed}}\n\
             \x20 - name: batcher\n\
             \x20   steps:\n\
             \x20     t: {{kind: stream, stream: bulk, from: earliest, batch: {{size: 2, window: 30s}}}}\n\
             \x20     f: {{kind: finish, depends_on: [t], status: completed}}\n",
            fx = fixtures(),
        ),
    )
    .unwrap();
    let err_path = common::unique_path("otel-batch", "err");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn agentd");
    // The supervisor exports a run's span when the run ends, every few
    // seconds.
    let mut spans: Vec<Value> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !spans.iter().any(|s| s["name"] == "run batcher") {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(post) = posts.recv_timeout(left) else {
            break;
        };
        spans.extend(
            post.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&err_path);
    let batcher = spans
        .iter()
        .find(|s| s["name"] == "run batcher")
        .unwrap_or_else(|| panic!("no batcher span: {spans:#?}\n{log}"));
    assert_eq!(batcher["kind"], 5, "CONSUMER: {batcher}");
    let emits: Vec<(String, String)> = spans
        .iter()
        .filter(|s| s["name"] == "emit bulk")
        .map(|s| {
            assert_eq!(s["kind"], 4, "PRODUCER: {s}");
            (
                s["traceId"].as_str().unwrap().to_string(),
                s["spanId"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(emits.len(), 2, "{spans:#?}");
    let mut links: Vec<(String, String)> = batcher["links"]
        .as_array()
        .unwrap_or_else(|| panic!("no links: {batcher}"))
        .iter()
        .map(|l| {
            (
                l["traceId"].as_str().unwrap().to_string(),
                l["spanId"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let mut want = emits;
    links.sort();
    want.sort();
    assert_eq!(links, want, "{batcher}");
}

/// A command the `exec` tool runs is exported as the span its `TRACEPARENT`
/// named, under its step's. A `forward: {webhook}` names where it went —
/// scheme, address, port — and not its URL, whose path and query may be a
/// credential.
#[cfg(feature = "exec")]
#[test]
fn an_exec_command_is_its_span_and_a_forward_names_no_url() {
    let (port, posts) = spawn_collector();
    let (api, sent) = spawn_api();
    let workdir = common::unique_path("otel-exec", "d");
    std::fs::create_dir_all(&workdir).unwrap();
    let cfg = common::unique_path("otel-exec", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "\
             agent:\n  name: exec\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             streams:\n  out: {{}}\n\
             security:\n  tls_ca: {fx}/ca.pem\n  exec:\n    enabled: true\n    allow: [sh]\n    workdir: {workdir}\n    timeout: 10s\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 500ms\n\
             observability:\n  log_level: info\n  log_content: true\n  otel: {{endpoint: \"https://127.0.0.1:{port}\"}}\n\
             workflows:\n\
             \x20 - name: job\n\
             \x20   steps:\n\
             \x20     s: {{kind: once}}\n\
             \x20     e: {{kind: emit, depends_on: [s], stream: out, subject: x, data: {{n: 1}}, forward: {{webhook: \"http://{api}/hooks/T0/s3cr3t?token=t0k3n\", allow_private: true}}}}\n\
             \x20     x: {{kind: tool, depends_on: [e], name: exec, args: {{cmd: sh, args: [\"-c\", 'echo $TRACEPARENT']}}}}\n\
             \x20     f: {{kind: finish, depends_on: [x], output: \"{{{{steps.x.output}}}}\"}}\n",
            fx = fixtures(),
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_dir_all(&workdir);
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "{log}");
    let (spans, _) = drain(&posts, Duration::from_secs(2));
    let id = |s: &Value| s["spanId"].as_str().unwrap().to_string();

    let done = &events(&log, "run.done")[0];
    let seen = done["output"]["stdout"]
        .as_str()
        .unwrap_or_else(|| panic!("no stdout: {done}"))
        .trim()
        .to_string();
    let exec = named(&spans, "exec sh");
    assert_eq!(id(exec), span_of(&seen), "{exec}\n{seen}");
    assert_eq!(exec["parentSpanId"], id(named(&spans, "step x")), "{exec}");
    assert_eq!(exec["kind"], 1, "INTERNAL: {exec}");

    let forward = named(&spans, "POST");
    assert_eq!(forward["kind"], 3, "CLIENT: {forward}");
    let got = sent.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(id(forward), span_of(&got[0]), "{forward}");
    let attr = |k: &str| {
        forward["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == k)
            .map(|a| a["value"].clone())
    };
    assert_eq!(
        attr("server.address"),
        Some(json!({"stringValue": "127.0.0.1"})),
        "{forward}"
    );
    assert_eq!(
        attr("url.scheme"),
        Some(json!({"stringValue": "http"})),
        "{forward}"
    );
    assert!(attr("server.port").is_some(), "{forward}");
    assert_eq!(attr("url.full"), None, "{forward}");
    let text = forward.to_string();
    assert!(
        !text.contains("s3cr3t") && !text.contains("t0k3n"),
        "the URL's credential went out: {forward}"
    );
}

/// A run an `a2a` start fires is INTERNAL, under the SERVER span of the
/// message it was fired for, which is under the caller's span: one request,
/// one SERVER span. A push about the task is a CLIENT span that names where
/// it went, not its URL.
#[cfg(feature = "a2a")]
#[test]
fn an_a2a_start_s_run_is_under_its_message_s_server_span() {
    const UP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let (port, posts) = spawn_collector();
    let (hook, pushed) = spawn_api();
    let (_d, addr) = common::spawn_listener_bound("a2a.listen", |a2a| {
        let cfg_path = common::unique_path("otel-a2a", "yaml");
        std::fs::write(
            &cfg_path,
            format!(
                "\
                 agent:\n  name: reviews\n  instruction: test\n  preflight: never\n\
                 intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
                 store:\n  kind: memory\n\
                 security:\n  tls_ca: {fx}/ca.pem\n\
                 a2a:\n  listen: http://127.0.0.1:{a2a}\n  push:\n    enabled: true\n    allow_private: true\n\
                 lifecycle:\n  run_until: drained\n\
                 observability:\n  log_level: info\n  otel: {{endpoint: \"https://127.0.0.1:{port}\"}}\n\
                 workflows:\n\
                 \x20 - name: reviewer\n\
                 \x20   steps:\n\
                 \x20     trigger: {{kind: a2a, command: \"review.start\"}}\n\
                 \x20     fin: {{kind: finish, depends_on: [trigger], status: completed}}\n",
                fx = fixtures(),
            ),
        )
        .unwrap();
        let err_path = common::unique_path("otel-a2a", "err");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
            .spawn()
            .expect("spawn agentd");
        let log = err_path.clone();
        (
            Daemon {
                child,
                cfg_path,
                err_path,
            },
            log,
        )
    });
    let send = common::SendMessage::command("review.start", json!({}))
        .header("traceparent", UP)
        .return_immediately();
    let mut params = send.params();
    params["configuration"]["taskPushNotificationConfig"] =
        json!({"url": format!("http://{hook}/hooks/T0/s3cr3t?token=t0k3n")});
    let headers = send.headers();
    let extra: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let reply = common::a2a_post(&addr, &common::rpc_body(1, send.method(), params), &extra);
    assert_eq!(reply.status, 200, "{reply:?}");
    // The supervisor exports a run's span when the run ends, every few
    // seconds, and a push's when it is delivered.
    let mut spans: Vec<Value> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !(spans.iter().any(|s| s["name"] == "run reviewer")
        && spans.iter().any(|s| s["name"] == "a2a.push"))
    {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(post) = posts.recv_timeout(left) else {
            break;
        };
        spans.extend(
            post.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
    let message = named(&spans, "SendMessage");
    assert_eq!(message["kind"], 2, "SERVER: {message}");
    assert_eq!(message["parentSpanId"], span_of(UP), "{message}");
    let run = named(&spans, "run reviewer");
    assert_eq!(run["kind"], 1, "INTERNAL: {run}");
    assert_eq!(run["parentSpanId"], message["spanId"], "{run}");
    let servers: Vec<&Value> = spans.iter().filter(|s| s["kind"] == 2).collect();
    assert_eq!(servers.len(), 1, "one SERVER span: {servers:#?}");

    let sent: Vec<String> = pushed
        .lock()
        .unwrap()
        .iter()
        .map(|tp| span_of(tp).to_string())
        .collect();
    let pushes: Vec<&Value> = spans.iter().filter(|s| s["name"] == "a2a.push").collect();
    assert!(!pushes.is_empty(), "{spans:#?}");
    for p in pushes {
        assert_eq!(p["kind"], 3, "CLIENT: {p}");
        assert!(
            sent.contains(&p["spanId"].as_str().unwrap().to_string()),
            "{p}\n{sent:?}"
        );
        let keys: Vec<&str> = p["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["key"].as_str())
            .collect();
        assert!(
            keys.contains(&"server.address") && !keys.contains(&"url.full"),
            "{p}"
        );
        let text = p.to_string();
        assert!(
            !text.contains("s3cr3t") && !text.contains("t0k3n"),
            "the URL's credential went out: {p}"
        );
    }
}
