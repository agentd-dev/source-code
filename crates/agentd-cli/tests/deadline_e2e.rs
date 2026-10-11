// SPDX-License-Identifier: AGPL-3.0-only
//! A run's deadline bounds a model call in flight. The deadline used to be
//! checked between rounds only, so one model call that never answered held
//! the run for the call's own timeout (two minutes by default). The call's
//! timeout is now cut to what is left of the deadline, and the run ends on it
//! with the deadline's exit code, 124.
#![cfg(unix)]

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_deadline_cuts_off_a_model_call_in_flight() {
    // A model endpoint that accepts the connection and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let _held = conn;
                std::thread::sleep(Duration::from_secs(60));
            });
        }
    });
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--intelligence",
            &format!("http://127.0.0.1:{port}/v1/chat/completions"),
            "--model",
            "m-1",
            "--instruction",
            "say hello",
            "--deadline",
            "2s",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    let took = started.elapsed();
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(124), "{log}");
    assert!(
        took < Duration::from_secs(15),
        "the deadline ended the call, not the call's own timeout: {took:?}\n{log}"
    );
}
