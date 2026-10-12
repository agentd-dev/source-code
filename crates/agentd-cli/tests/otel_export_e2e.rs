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
#![cfg(all(unix, feature = "otel", feature = "tls"))]

mod common;

use agentd::net::tls::{ServerIdentity, TlsAcceptor};
use serde_json::Value;
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
    for signal in ["traces", "logs"] {
        let lines: Vec<&Value> = failed.iter().filter(|e| e["signal"] == signal).collect();
        assert_eq!(lines.len(), 1, "{signal}:\n{log}");
        assert_eq!(lines[0]["level"], "warn");
        assert_eq!(lines[0]["endpoint"], endpoint.as_str());
        assert_eq!(lines[0]["failures"], 1);
        assert!(
            lines[0]["err"]
                .as_str()
                .is_some_and(|e| e.contains(&format!("127.0.0.1:{port}/v1/{signal}"))),
            "{log}"
        );
    }
    assert_eq!(failed.len(), 2, "{log}");
}
