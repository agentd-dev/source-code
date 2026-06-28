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

/// Reserve a free localhost TCP port, then release it so the daemon can bind it.
/// (A momentary `:0` bind that we drop — the standard "grab a free port" trick;
/// the sub-ms reuse window is benign for a single-process test.)
#[cfg(feature = "metrics")]
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// GET `/metrics` over a fresh TCP connection; return the response body (or "" if
/// the surface is not up yet).
#[cfg(feature = "metrics")]
fn scrape_metrics(port: u16) -> String {
    use std::io::Write;
    use std::net::TcpStream;
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return String::new();
    };
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    if s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return String::new();
    }
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok();
    buf
}

/// Extract the integer value of a `agentd_tokens_total{type="..."} N` sample.
#[cfg(feature = "metrics")]
fn token_total(body: &str, ty: &str) -> u64 {
    let needle = format!("agentd_tokens_total{{type=\"{ty}\"}} ");
    for line in body.lines() {
        if let Some(v) = line.strip_prefix(&needle) {
            return v.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// The producer→consumer→counter chain (metrics-honesty): a real reactive run
/// drives a real subagent against the mock LLM; the child now PRODUCES
/// `AgentMsg::Usage`, the supervisor reactor CONSUMES it via `record_tokens`, and
/// the frozen `agentd_tokens_total{type}` counter is non-zero on the `/metrics`
/// scrape. Before this fix the child never emitted `Usage`, so the counter was
/// silently 0 despite the wired consumer. [feature: metrics]
#[cfg(feature = "metrics")]
#[test]
fn reactive_run_rolls_token_usage_up_to_agentd_tokens_total() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("llm.sock");
    let mut llm = start_mock_llm(&sock, "final");

    let intel = format!("unix:{}", sock.display());
    // `--internal-mock-mcp` (no `--no-emit`) pushes one resources/updated after the
    // subscribe, firing exactly one reaction — one real subagent run.
    let mcp = format!("mock={} --internal-mock-mcp file:///in.json", exe());
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let mut child = Command::new(exe())
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react to the changed resource",
            "--intelligence",
            &intel,
            "--subscribe",
            "file:///in.json",
            "--mcp",
            &mcp,
            "--metrics-addr",
            &addr,
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

    // Poll /metrics while the daemon is LIVE: the reaction fires ~200ms after
    // subscribe; once the child rolls its Usage up, the supervisor's
    // `agentd_tokens_total` goes non-zero. The mock `final` answer reports
    // {prompt_tokens:11, completion_tokens:5}, so `out` reaches ≥ 5.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut out_tokens = 0u64;
    let mut in_tokens = 0u64;
    while Instant::now() < deadline {
        let body = scrape_metrics(port);
        out_tokens = token_total(&body, "out");
        in_tokens = token_total(&body, "in");
        if out_tokens > 0 && in_tokens > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let _ = child.kill();
    let _ = child.wait();
    sigterm(llm.id());
    let _ = llm.wait();
    let out = reader.join().unwrap_or_default();

    // Sanity: the reaction actually ran a real subagent against the mock LLM —
    // there WAS a run to produce Usage.
    assert!(
        out.contains(r#""event":"trigger.fired""#),
        "no reaction fired (no real run to produce Usage):\n{out}"
    );
    // The chain: child PRODUCES AgentMsg::Usage → supervisor reactor CONSUMES it via
    // record_tokens → the frozen agentd_tokens_total counter is non-zero. Before
    // this fix the producer was missing, so this would read 0.
    assert!(
        in_tokens > 0 && out_tokens > 0,
        "agentd_tokens_total stayed zero — the child→supervisor→counter token \
         roll-up is broken (in={in_tokens}, out={out_tokens})\n{out}"
    );
}
