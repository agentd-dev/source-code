// SPDX-License-Identifier: AGPL-3.0-only
//! The circuit breaker (`breaker:` on remote-effect steps), end to end and —
//! the part that distinguishes it from `retry` — ACROSS process lives: five
//! runs of the same workflow in five separate daemon processes sharing one
//! file store. Two real failures open the circuit; the third run fails fast
//! without the mock seeing a request (and a restart did not amnesty the
//! remote); after the cooldown one probe goes through, finds the remote
//! recovered, and closes the circuit for the runs that follow.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde_json::Value;

/// Counts requests; answers 500 until `healthy` flips, then 200.
fn spawn_flaky(healthy: Arc<AtomicBool>, hits: Arc<AtomicU32>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
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
            hits.fetch_add(1, Ordering::SeqCst);
            let mut s = reader.into_inner();
            let resp: &[u8] = if healthy.load(Ordering::SeqCst) {
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}"
            } else {
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndown"
            };
            let _ = s.write_all(resp);
            let _ = s.flush();
        }
    });
    port
}

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// One life: run the workflow once, return this life's stderr.
fn life(cfg: &str, state: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .env("AGENTD_STATE_DIR", state)
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_breaker_opens_survives_restarts_probes_and_closes() {
    let healthy = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicU32::new(0));
    let port = spawn_flaky(Arc::clone(&healthy), Arc::clone(&hits));

    let dir = common::unique_path("agentd-breaker", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/config.yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"2\"\n\
             agent:\n  name: breakerbox\n\
             store:\n  kind: file\n  file:\n    path: {dir}/state\n\
             workflows:\n  - name: pay\n    steps:\n\
             \x20     start: {{kind: once, policy: always}}\n\
             \x20     charge: {{kind: http, depends_on: [start], method: POST, url: \"http://127.0.0.1:{port}/charge\", allow_private: true, breaker: {{failures: 2, cooldown: \"2s\"}}}}\n\
             \x20     done: {{kind: finish, depends_on: [charge], status: completed}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 300ms\n\
             observability:\n  log_level: info\n"
        ),
    )
    .unwrap();
    let state = format!("{dir}/state");

    // Lives 1–2: real attempts, real failures — the second opens the circuit.
    let l1 = life(&cfg, &state);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "life 1 dialled:\n{l1}");
    assert!(
        events(&l1, "breaker.open").is_empty(),
        "1 failure < 2:\n{l1}"
    );
    let l2 = life(&cfg, &state);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "life 2 dialled:\n{l2}");
    assert_eq!(events(&l2, "breaker.open").len(), 1, "opens at 2:\n{l2}");

    // Life 3: a NEW process, same store — the open circuit survived the
    // restart, and the mock sees no request at all.
    let l3 = life(&cfg, &state);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "life 3 failed fast without dialling:\n{l3}"
    );
    let done = events(&l3, "step.done");
    assert!(
        done.iter().any(|e| e["err"]
            .as_str()
            .is_some_and(|m| m.starts_with("breaker open"))),
        "the fast-fail names itself:\n{l3}"
    );
    assert!(
        events(&l3, "breaker.open").is_empty(),
        "no re-open logging on a fast-fail:\n{l3}"
    );

    // The remote recovers; the cooldown passes.
    healthy.store(true, Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(2_300));

    // Life 4: the one probe goes through, succeeds, closes the circuit.
    let l4 = life(&cfg, &state);
    assert_eq!(hits.load(Ordering::SeqCst), 3, "the probe dialled:\n{l4}");
    assert_eq!(events(&l4, "breaker.probe").len(), 1, "{l4}");
    assert_eq!(events(&l4, "breaker.closed").len(), 1, "{l4}");
    assert!(
        events(&l4, "run.done")
            .iter()
            .any(|e| e["status"] == "completed"),
        "{l4}"
    );

    // Life 5: closed means ordinary — a dial, a success, no breaker events.
    let l5 = life(&cfg, &state);
    assert_eq!(hits.load(Ordering::SeqCst), 4, "{l5}");
    assert!(events(&l5, "breaker.probe").is_empty(), "{l5}");
    assert!(
        events(&l5, "run.done")
            .iter()
            .any(|e| e["status"] == "completed"),
        "{l5}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rated_step_paces_a_fanout_instead_of_bursting() {
    // 1 token, 1s window: of three sequential calls, the first dials at once
    // and each of the others parks on a durable timer for its token first.
    let healthy = Arc::new(AtomicBool::new(true));
    let hits = Arc::new(AtomicU32::new(0));
    let port = spawn_flaky(Arc::clone(&healthy), Arc::clone(&hits));

    let dir = common::unique_path("agentd-steprate", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/config.yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"2\"\n\
             agent:\n  name: pacer\n\
             store:\n  kind: memory\n\
             workflows:\n  - name: sweep\n    steps:\n\
             \x20     start: {{kind: once}}\n\
             \x20     each: {{kind: foreach, depends_on: [start], over: [1, 2, 3], batch: {{size: 1, parallel: 1}},\n\
             \x20            body: {{steps: {{call: {{kind: http, method: POST, url: \"http://127.0.0.1:{port}/ping\", allow_private: true, rate: \"1/1s\"}}}}}}}}\n\
             \x20     done: {{kind: finish, depends_on: [each], status: completed}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 500ms\n\
             observability:\n  log_level: info\n"
        ),
    )
    .unwrap();
    let stderr = life(&cfg, &format!("{dir}/state"));
    assert!(
        events(&stderr, "run.done")
            .iter()
            .any(|e| e["status"] == "completed"),
        "{stderr}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "all three dialled:\n{stderr}"
    );
    let waits = events(&stderr, "step.rate_wait");
    assert!(
        waits.len() >= 2,
        "calls 2 and 3 each waited for a token (got {} waits):\n{stderr}",
        waits.len()
    );
    // The park never consumes an attempt: every dial reports attempt 1.
    assert!(
        events(&stderr, "step.start")
            .iter()
            .filter(|e| e["kind"] == "http")
            .all(|e| e["attempt"] == 1),
        "a throttle wait is not an attempt:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
