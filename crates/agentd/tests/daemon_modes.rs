// SPDX-License-Identifier: Apache-2.0
//! Observe-to-validate E2E test of the long-lived `loop`/`schedule` modes (M4).
//!
//! A real agentd in `loop` mode must fire independent runs on its interval and,
//! on SIGTERM, drain gracefully and exit 0 (the cloud-native contract for
//! daemon modes, RFC 0011). Validated by observing the telemetry + exit code.
//! No live LLM — each run's subagent fails fast on an unreachable intelligence
//! endpoint, but the *scheduling* behaviour is fully visible.

mod common;

use common::spawn_mock_mcp;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(unix)]
fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

#[test]
#[cfg(unix)]
fn loop_mode_fires_runs_then_drains_to_exit_0_on_sigterm() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mut child = Command::new(exe)
        .args([
            "--mode",
            "loop",
            "--interval",
            "500ms",
            "--instruction",
            "do the recurring thing",
            "--intelligence",
            "unix:/nonexistent/agentd-loop.sock",
            "--model",
            "m",
            "--log-level",
            "info",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd loop");

    let mut stderr = child.stderr.take().expect("stderr piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // Let it fire a couple of runs, then request a graceful shutdown.
    std::thread::sleep(Duration::from_millis(1300));
    sigterm(child.id());
    let status = child.wait().expect("wait for agentd");
    let out = reader.join().unwrap_or_default();

    // Graceful drain → exit 0 (not 143), and the daemon fired supervised runs.
    assert_eq!(
        status.code(),
        Some(0),
        "expected graceful exit 0; stderr:\n{out}"
    );
    assert!(
        out.contains(r#""event":"schedule.fired""#),
        "no schedule.fired:\n{out}"
    );
    assert!(
        out.contains(r#""event":"subagent.spawn""#),
        "no run subagent.spawn:\n{out}"
    );
    assert!(
        out.contains(r#""reason":"drain""#),
        "no graceful drain logged:\n{out}"
    );
}

#[test]
#[cfg(unix)]
fn reactive_mode_drains_to_exit_0_on_sigterm() {
    // The reactive daemon must, on SIGTERM, stop accepting work, unsubscribe,
    // and exit 0 — the same cloud-native drain contract as loop/schedule.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mock = spawn_mock_mcp("file:///in.json", false);
    let mut child = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            "unix:/nonexistent.sock",
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mock.mcp_arg("mock"),
            "--log-level",
            "info",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd reactive");

    let mut stderr = child.stderr.take().expect("stderr piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // Let it subscribe + reach readiness, then request a graceful shutdown.
    std::thread::sleep(Duration::from_millis(900));
    sigterm(child.id());
    let status = child.wait().expect("wait for agentd");
    let out = reader.join().unwrap_or_default();

    assert_eq!(
        status.code(),
        Some(0),
        "expected graceful exit 0; stderr:\n{out}"
    );
    assert!(
        out.contains(r#""event":"proc.ready""#),
        "reactive never became ready:\n{out}"
    );
    assert!(
        out.contains(r#""reason":"drain""#) && out.contains(r#""mode":"reactive""#),
        "no reactive graceful drain logged:\n{out}"
    );
}

#[test]
#[cfg(unix)]
fn daemon_writes_a_live_health_file() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().expect("tempdir");
    let hf = dir.path().join("health.json");

    let mut child = Command::new(exe)
        .args([
            "--mode",
            "loop",
            "--interval",
            "2s",
            "--instruction",
            "x",
            "--intelligence",
            "unix:/nonexistent.sock",
            "--model",
            "m",
            "--health-file",
            hf.to_str().unwrap(),
            "--log-level",
            "warn",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentd loop");

    // Let the writer thread emit at least one heartbeat, then snapshot it.
    std::thread::sleep(Duration::from_millis(1400));
    let body = std::fs::read_to_string(&hf).unwrap_or_default();
    sigterm(child.id());
    let _ = child.wait();

    // A live supervisor: alive=true, the right mode, and a fresh tick age
    // (string-asserted to avoid a serde_json dev-dep).
    // alive=true already means the tick was fresh (age < stale window) at write.
    assert!(
        body.contains(r#""alive":true"#),
        "health file not alive:\n{body}"
    );
    assert!(
        body.contains(r#""mode":"loop""#),
        "wrong mode in health file:\n{body}"
    );
    assert!(
        body.contains(r#""supervisor_tick_age_ms""#),
        "no tick age:\n{body}"
    );
}
