//! The observe-to-validate E2E suite (M7, the operator ask): drive *real* agentd
//! runs against the built-in mock LLM (+ mock MCP) and assert on the **observed**
//! JSON-lines telemetry + outcome. This is the first end-to-end exercise of the
//! actual agentic loop — every other test stubs the intelligence endpoint.
#![cfg(unix)]

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_agentd")
}

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// Start the mock LLM on `socket` with `script`, waiting until it binds.
fn start_mock_llm(socket: &Path, script: &str) -> Child {
    let child = Command::new(exe())
        .args(["--internal-mock-llm", socket.to_str().unwrap(), script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock-llm");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !socket.exists() {
        if Instant::now() >= deadline {
            panic!("mock-llm never bound its socket");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

/// Run `agentd <args>` to completion; return (exit_code, stdout, stderr).
fn run_once(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(exe()).args(args).output().expect("run agentd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn once_mode_runs_the_real_loop_to_a_completed_answer() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_mock_llm(&sock, "final");

    let intel = format!("unix:{}", sock.display());
    let (code, stdout, stderr) = run_once(&[
        "--mode",
        "once",
        "--instruction",
        "do the thing",
        "--intelligence",
        &intel,
        "--log-level",
        "info",
    ]);

    sigterm(llm.id());
    let _ = llm.wait();

    assert_eq!(code, 0, "expected exit 0; stderr:\n{stderr}");
    assert!(
        stdout.contains("mock-llm done"),
        "model answer not on stdout: {stdout:?}"
    );
    assert!(
        stderr.contains(r#""event":"loop.final""#),
        "no loop.final:\n{stderr}"
    );
    assert!(
        stderr.contains(r#""status":"completed""#),
        "loop did not complete:\n{stderr}"
    );
}

#[test]
fn once_mode_runs_a_tool_call_react_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_mock_llm(&sock, "read");

    let intel = format!("unix:{}", sock.display());
    let mcp = format!(
        "mock={} --internal-mock-mcp file:///in.json --no-emit",
        exe()
    );
    let (code, stdout, stderr) = run_once(&[
        "--mode",
        "once",
        "--instruction",
        "read the resource",
        "--intelligence",
        &intel,
        "--mcp",
        &mcp,
        "--log-level",
        "info",
    ]);

    sigterm(llm.id());
    let _ = llm.wait();

    assert_eq!(code, 0, "expected exit 0; stderr:\n{stderr}");
    assert!(
        stdout.contains("read complete"),
        "final answer not on stdout: {stdout:?}"
    );
    // The model called the resource.read tool, and the loop ran it then finished.
    assert!(
        stderr.contains(r#""event":"tool.call""#) && stderr.contains("resource.read"),
        "no resource.read tool.call:\n{stderr}"
    );
    assert!(
        stderr.contains(r#""event":"tool.result""#),
        "no tool.result:\n{stderr}"
    );
    assert!(
        stderr.contains(r#""status":"completed""#),
        "loop did not complete:\n{stderr}"
    );
}

#[test]
fn reactive_self_scheduling_fires_a_wake() {
    // A reaction's model calls the `schedule` self-tool; the daemon arms the wake
    // and fires it ~1s later — a self-sustaining agent, observed end to end.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_mock_llm(&sock, "schedule");

    let intel = format!("unix:{}", sock.display());
    let mcp = format!(
        "mock={} --internal-mock-mcp file:///in.json --no-emit",
        exe()
    );
    let mut child = Command::new(exe())
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            &intel,
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mcp,
            "--log-level",
            "info",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn reactive agentd");

    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // read-after-subscribe → reaction → schedule(after 1s) → wake fires ~1s later.
    std::thread::sleep(Duration::from_millis(2800));
    let _ = child.kill();
    let _ = child.wait();
    sigterm(llm.id());
    let _ = llm.wait();
    let out = reader.join().unwrap_or_default();

    assert!(
        out.contains(r#""event":"self.schedule""#),
        "model never called schedule:\n{out}"
    );
    assert!(
        out.contains(r#""kind":"self_schedule""#),
        "no self-scheduled wake armed/fired:\n{out}"
    );
    assert!(
        out.contains(r#""event":"trigger.fired""#),
        "no trigger fired:\n{out}"
    );
}

#[test]
fn reactive_self_subscribe_arms_a_warm_continue_route() {
    // A reaction's model calls the `subscribe` self-tool for a NEW uri; the daemon
    // must arm it as a WARM continue route (RFC 0008 §self-subscribe = continue),
    // not a fresh-spawn route — so future events re-enter one live session.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_mock_llm(&sock, "subscribe");

    let intel = format!("unix:{}", sock.display());
    let mcp = format!(
        "mock={} --internal-mock-mcp file:///in.json --no-emit",
        exe()
    );
    let mut child = Command::new(exe())
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            &intel,
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mcp,
            "--log-level",
            "info",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn reactive agentd");

    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // read-after-subscribe → reaction → the model self-subscribes to file:///watch.json.
    std::thread::sleep(Duration::from_millis(2000));
    let _ = child.kill();
    let _ = child.wait();
    sigterm(llm.id());
    let _ = llm.wait();
    let out = reader.join().unwrap_or_default();

    assert!(
        out.contains(r#""event":"self.subscribe""#),
        "model never called subscribe:\n{out}"
    );
    assert!(
        out.contains(r#""kind":"self_subscribe""#),
        "no self-subscription armed:\n{out}"
    );
    // The new route is a WARM continue, not a Spawn (the signature capability).
    assert!(
        out.contains(r#""disposition":"continue""#),
        "self-subscribe must arm a continue (warm) route:\n{out}"
    );
}
