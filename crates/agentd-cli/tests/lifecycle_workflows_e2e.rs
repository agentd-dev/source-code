// SPDX-License-Identifier: AGPL-3.0-only
//! Init and deinit workflows, end to end on the exact motivating scenario: a
//! daemon that REGISTERS its webhook URL with a third-party service when it
//! starts (`once {policy: always}` — the init idiom that always existed) and
//! DEREGISTERS it during shutdown (`event {on: lifecycle.shutdown}` — new),
//! with the drain WAITING for the deregistration to land before exit 0.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// One recorded call: (method, path, body).
type Calls = Arc<Mutex<Vec<(String, String, String)>>>;

/// The "third-party service": records every call it serves.
fn spawn_service() -> (u16, Calls) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut reader = BufReader::new(stream);
            let mut start = String::new();
            if reader.read_line(&mut start).is_err() || start.is_empty() {
                continue;
            }
            let mut parts = start.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let mut len = 0usize;
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).is_err() || l.trim().is_empty() {
                    break;
                }
                if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).ok();
            seen.lock()
                .unwrap()
                .push((method, path, String::from_utf8_lossy(&body).into_owned()));
            let mut s = reader.into_inner();
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            );
            let _ = s.flush();
        }
    });
    (port, calls)
}

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

#[test]
fn a_daemon_registers_on_start_and_deregisters_during_drain() {
    let (port, calls) = spawn_service();
    let cfg = common::unique_path("lifecycle-wf", "yaml");
    std::fs::write(
        &cfg,
        format!(
            r#"config_version: "2"
agent:
  name: hooked
vars:
  service: "http://127.0.0.1:{port}"
  my_hook: "https://this-agent.example/hooks/inbound"
store: {{ kind: memory }}
workflows:
  - name: init
    steps:
      boot:     {{ kind: once, policy: always }}
      register: {{ kind: http, depends_on: [boot], method: POST, url: "{{{{config.service}}}}/webhooks",
                   allow_private: true, json: {{ url: "{{{{config.my_hook}}}}" }} }}
      f:        {{ kind: finish, depends_on: [register], status: completed }}
  - name: deinit
    steps:
      bye:        {{ kind: event, on: lifecycle.shutdown }}
      deregister: {{ kind: http, depends_on: [bye], method: DELETE,
                     url: "{{{{config.service}}}}/webhooks?url={{{{config.my_hook}}}}",
                     allow_private: true }}
      f:          {{ kind: finish, depends_on: [deregister], status: completed }}
lifecycle: {{ run_until: drained, drain_timeout: 5s }}
observability: {{ log_level: info }}
"#
        ),
    )
    .unwrap();
    let err_path = common::unique_path("lifecycle-wf", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn daemon");

    // 1. The init workflow registered the hook at boot.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, p, b)| m == "POST" && p == "/webhooks" && b.contains("this-agent.example"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "registration never arrived:\n{}",
            std::fs::read_to_string(&err_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        calls.lock().unwrap().iter().all(|(m, _, _)| m != "DELETE"),
        "no deregistration before shutdown"
    );

    // 2. SIGTERM: the deinit workflow fires DURING drain, and exit waits for it.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().expect("exit");
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    assert_eq!(status.code(), Some(0), "clean drain:\n{stderr}");
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, p, _)| m == "DELETE" && p.starts_with("/webhooks")),
        "the hook was deregistered before exit:\n{stderr}"
    );
    // The deinit run completed inside this life, not a later one.
    assert!(
        events(&stderr, "run.done")
            .iter()
            .any(|e| e["workflow"] == "deinit" && e["status"] == "completed"),
        "{stderr}"
    );
    assert!(
        events(&stderr, "start.fired")
            .iter()
            .any(|e| e["kind"] == "event" && e["workflow"] == "deinit"),
        "{stderr}"
    );
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&err_path);
}
