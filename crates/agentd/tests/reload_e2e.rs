//! E2E test of SIGHUP hot reload (RFC 0017 §5) — observe-to-validate.
//!
//! A real reactive agentd with a `--config` file must, on SIGHUP, re-read the
//! FILE, re-validate, and apply a reloadable change at the quiesce boundary,
//! emitting a `config.reloaded` event — then still drain to exit 0 on SIGTERM.
//! A reload that touches a restart-only field (or is invalid) is a clean no-op
//! that emits `config.reload_rejected` and leaves the daemon healthy.
//!
//! Validated by observing the JSON-lines telemetry + the exit code. No live LLM:
//! each reaction's subagent fails fast on an unreachable intelligence endpoint,
//! but the reload *behaviour* is fully visible in the event stream. Gated on the
//! `hot-reload` feature (the SIGHUP handler is feature-gated) + unix.
#![cfg(all(feature = "hot-reload", unix))]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

fn sighup(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGHUP);
    }
}

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// Spawn a reactive agentd reading `config_path`, run it, SIGHUP after rewriting
/// the file to `new_file`, then SIGTERM and return (exit_code, stderr).
fn run_reload(initial_file: &str, new_file: &str) -> (Option<i32>, String) {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mcp = format!("mock={exe} --internal-mock-mcp file:///in.json --no-emit");

    // Write the initial config file (a reloadable-only file: model + log_level).
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("agentd.json");
    std::fs::write(&cfg_path, initial_file).expect("write initial config");

    let mut child = Command::new(exe)
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            "unix:/nonexistent-reload.sock",
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
        .expect("spawn agentd reactive");

    let mut stderr = child.stderr.take().expect("stderr piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    // Let it subscribe + reach readiness, rewrite the file, then SIGHUP.
    std::thread::sleep(Duration::from_millis(700));
    {
        let mut f = std::fs::File::create(&cfg_path).expect("rewrite config");
        f.write_all(new_file.as_bytes()).expect("write new config");
        f.flush().ok();
    }
    sighup(child.id());

    // Give the reactor a few ticks to run the reload routine, then drain.
    std::thread::sleep(Duration::from_millis(800));
    sigterm(child.id());
    let status = child.wait().expect("wait for agentd");
    let out = reader.join().unwrap_or_default();
    (status.code(), out)
}

#[test]
fn sighup_applies_a_reloadable_change_then_drains_to_exit_0() {
    // The reload changes a RELOADABLE field (model), so it is applied — the
    // daemon emits config.reloaded and keeps running, then drains to exit 0.
    let initial = r#"{ "model": "model-a", "log_level": "info" }"#;
    let changed = r#"{ "model": "model-b", "log_level": "info" }"#;
    let (code, out) = run_reload(initial, changed);

    assert!(
        out.contains(r#""event":"proc.ready""#),
        "reactive never became ready:\n{out}"
    );
    assert!(
        out.contains(r#""event":"config.reload_requested""#),
        "SIGHUP did not trigger a reload request:\n{out}"
    );
    assert!(
        out.contains(r#""event":"config.reloaded""#),
        "a reloadable change was not applied:\n{out}"
    );
    assert!(
        out.contains(r#""model""#) && out.contains("model"),
        "the changed field was not surfaced:\n{out}"
    );
    // A successful reload never trips the daemon — it still drains to exit 0.
    assert_eq!(code, Some(0), "expected graceful exit 0; stderr:\n{out}");
}

/// Like [`run_reload`] but the intelligence endpoint list is set in the FILE
/// (not a flag), so a reload can repoint it — the RFC 0018 §5 hot-swap path. No
/// `--intelligence` flag is passed (a flag would override the file + pin it).
fn run_reload_intel_in_file(initial_file: &str, new_file: &str) -> (Option<i32>, String) {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let mcp = format!("mock={exe} --internal-mock-mcp file:///in.json --no-emit");
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("agentd.json");
    std::fs::write(&cfg_path, initial_file).expect("write initial config");

    let mut child = Command::new(exe)
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "--mode",
            "reactive",
            "--instruction",
            "react",
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
        .expect("spawn agentd reactive");

    let mut stderr = child.stderr.take().expect("stderr piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });

    std::thread::sleep(Duration::from_millis(700));
    {
        let mut f = std::fs::File::create(&cfg_path).expect("rewrite config");
        f.write_all(new_file.as_bytes()).expect("write new config");
        f.flush().ok();
    }
    sighup(child.id());
    std::thread::sleep(Duration::from_millis(800));
    sigterm(child.id());
    let status = child.wait().expect("wait for agentd");
    let out = reader.join().unwrap_or_default();
    (status.code(), out)
}

#[test]
fn sighup_applies_an_intelligence_repoint_as_a_hot_swap() {
    // RFC 0018 §5.1: changing the intelligence endpoint LIST in the file is now a
    // RELOADABLE hot-swap — applied (config.reloaded + intel.swap), NOT rejected.
    // The endpoints are unreachable (no live LLM), but the reload BEHAVIOUR — the
    // swap is accepted, the daemon stays up, drains to 0 — is fully observable.
    let initial =
        r#"{ "intelligence": "unix:/nonexistent-a.sock", "model": "m", "log_level": "info" }"#;
    let changed = r#"{ "intelligence": "unix:/nonexistent-b.sock,unix:/nonexistent-c.sock", "model": "m", "model_swap": "restart-turn", "log_level": "info" }"#;
    let (code, out) = run_reload_intel_in_file(initial, changed);

    assert!(
        out.contains(r#""event":"proc.ready""#),
        "reactive never became ready:\n{out}"
    );
    assert!(
        out.contains(r#""event":"config.reloaded""#),
        "an intelligence repoint must be APPLIED, not rejected:\n{out}"
    );
    assert!(
        out.contains(r#""event":"intel.swap""#),
        "the swap event was not emitted:\n{out}"
    );
    assert!(
        out.contains("restart-turn"),
        "the new swap policy should surface in the swap event:\n{out}"
    );
    // RFC 0012 §3.7: the endpoint URL must NEVER appear in the swap event/logs —
    // only transport+index. The full `unix:/...` socket path must not leak.
    // (The mock URI in `--subscribe` is file:///in.json, distinct from the intel
    // sockets, so finding the intel path would be a real leak.)
    assert!(
        !out.contains("nonexistent-b.sock"),
        "the endpoint URL leaked into the telemetry:\n{out}"
    );
    assert_eq!(code, Some(0), "expected graceful exit 0; stderr:\n{out}");
}

#[test]
fn sighup_rejects_a_restart_only_change_as_a_no_op() {
    // Changing a RESTART-ONLY field (mcp_servers — scoped restart-only in this
    // build) is rejected: the reload is a clean no-op (config.reload_rejected),
    // the daemon stays healthy and drains to exit 0. The file declares a working
    // mock server (so startup reaches the loop); the reload edits its argv, an
    // mcp_servers diff → restart_required.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let initial = format!(
        r#"{{ "model": "m", "mcp_servers": [
            {{ "name": "filemock", "command": "{exe}",
               "argv": ["--internal-mock-mcp", "file:///other-a.json", "--no-emit"] }}
        ] }}"#
    );
    let changed = format!(
        r#"{{ "model": "m", "mcp_servers": [
            {{ "name": "filemock", "command": "{exe}",
               "argv": ["--internal-mock-mcp", "file:///other-b.json", "--no-emit"] }}
        ] }}"#
    );
    let (code, out) = run_reload(&initial, &changed);

    assert!(
        out.contains(r#""event":"config.reload_rejected""#),
        "a restart-only change should be rejected:\n{out}"
    );
    assert!(
        out.contains("restart_required"),
        "the rejection reason should be restart_required:\n{out}"
    );
    // A rejected reload is a no-op — the daemon is unharmed and drains to exit 0.
    assert_eq!(code, Some(0), "expected graceful exit 0; stderr:\n{out}");
}
