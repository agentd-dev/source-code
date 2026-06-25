//! Observe-to-validate E2E test of the long-lived `loop`/`schedule` modes (M4).
//!
//! A real agentd in `loop` mode must fire independent runs on its interval and,
//! on SIGTERM, drain gracefully and exit 0 (the cloud-native contract for
//! daemon modes, RFC 0011). Validated by observing the telemetry + exit code.
//! No live LLM — each run's subagent fails fast on an unreachable intelligence
//! endpoint, but the *scheduling* behaviour is fully visible.

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
    assert_eq!(status.code(), Some(0), "expected graceful exit 0; stderr:\n{out}");
    assert!(out.contains(r#""event":"schedule.fired""#), "no schedule.fired:\n{out}");
    assert!(out.contains(r#""event":"subagent.spawn""#), "no run subagent.spawn:\n{out}");
    assert!(out.contains(r#""reason":"drain""#), "no graceful drain logged:\n{out}");
}
