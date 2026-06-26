//! OTLP export E2E (M6/M7, deferred): a real agentd run under `--features otel`
//! must POST an `invoke_agent` span (GenAI semconv) to the configured collector.
//! Runs only under `cargo test --features otel`.
#![cfg(all(unix, feature = "otel"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_agentd")
}

fn start_mock_llm(socket: &std::path::Path) -> Child {
    let c = Command::new(exe())
        .args(["--internal-mock-llm", socket.to_str().unwrap(), "final"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "mock-llm never bound");
        std::thread::sleep(Duration::from_millis(20));
    }
    c
}

#[test]
fn a_completed_run_exports_an_invoke_agent_span() {
    // A one-shot OTLP/HTTP collector: accept one POST, read it, reply 200.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let collector = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match listener.accept() {
                Ok((mut s, _)) => {
                    s.set_nonblocking(false).ok();
                    s.set_read_timeout(Some(Duration::from_secs(2))).ok();
                    let mut buf = vec![0u8; 65536];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    break String::from_utf8_lossy(&buf[..n]).into_owned();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        break String::new();
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break String::new(),
            }
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_mock_llm(&sock);

    let intel = format!("unix:{}", sock.display());
    let status = Command::new(exe())
        .args(["--mode", "once", "--instruction", "do it", "--intelligence", &intel, "--model", "my-model", "--log-level", "error"])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{addr}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run agentd");

    let captured = collector.join().unwrap_or_default();
    let _ = llm.kill();
    let _ = llm.wait();

    assert!(status.success(), "run should complete (exit 0)");
    assert!(captured.contains("POST /v1/traces"), "no OTLP POST to /v1/traces:\n{captured}");
    assert!(captured.contains("resourceSpans"), "not an OTLP body:\n{captured}");
    assert!(captured.contains("invoke_agent"), "no invoke_agent span:\n{captured}");
    assert!(captured.contains("gen_ai.operation.name"), "no GenAI semconv attributes:\n{captured}");
    assert!(captured.contains("my-model"), "model not recorded on the span:\n{captured}");
}
