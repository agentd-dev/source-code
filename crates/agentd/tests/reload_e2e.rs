// SPDX-License-Identifier: Apache-2.0
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
    let exe = env!("CARGO_BIN_EXE_agent");
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
    let exe = env!("CARGO_BIN_EXE_agent");
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
fn sighup_rejects_an_invalid_config_as_a_no_op() {
    // An INVALID new config (a typo'd key — `deny_unknown_fields` rejects it) is a
    // clean no-op: the reload emits config.reload_rejected{reason:"invalid"}, the
    // daemon stays healthy and drains to exit 0. (mcp_servers is now RELOADABLE, so
    // the restart-required-reject is exercised at the unit level — config.rs's
    // coherence_rejects_a_differing_restart_only_field — not here.)
    let initial = r#"{ "model": "model-a", "log_level": "info" }"#;
    let bad = r#"{ "model": "model-a", "max_token": 5, "log_level": "info" }"#; // typo: max_token
    let (code, out) = run_reload(initial, bad);

    assert!(
        out.contains(r#""event":"config.reload_rejected""#),
        "an invalid reload should be rejected:\n{out}"
    );
    assert!(
        out.contains(r#""reason":"invalid""#),
        "the rejection reason should be invalid:\n{out}"
    );
    // A rejected reload is a no-op — the daemon is unharmed and drains to exit 0.
    assert_eq!(code, Some(0), "expected graceful exit 0; stderr:\n{out}");
}

/// Spawn a reactive agentd whose MCP servers + subscriptions are declared in the
/// config FILE (so a reload can add/remove one), run it, SIGHUP after rewriting
/// the file, then SIGTERM. Returns (exit_code, stderr). No `--mcp`/`--subscribe`
/// flags — those would compose with the file and pin the flag-declared server.
fn run_reload_mcp_in_file(initial_file: &str, new_file: &str) -> (Option<i32>, String) {
    let exe = env!("CARGO_BIN_EXE_agent");
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
    std::thread::sleep(Duration::from_millis(900));
    sigterm(child.id());
    let status = child.wait().expect("wait for agentd");
    let out = reader.join().unwrap_or_default();
    (status.code(), out)
}

#[test]
fn sighup_re_handshakes_mcp_servers_on_reload_add_and_remove() {
    // RFC 0017 §5.1/§5.3 step 4: `mcp_servers` is now RELOADABLE — a live
    // re-handshake at the quiesce boundary. The reload here REMOVES `mockA` (its
    // McpClient Drop runs the stdio shutdown ladder) and ADDS `mockB` + a new
    // subscription on its URI. We observe the re-handshake in the event stream:
    // config.reloaded names mcp_servers, a reload-kind mcp.connect fires for the
    // added server, a reload-kind subscribe + read-after-subscribe arms the new
    // URI — and the daemon stays up and drains to exit 0.
    let exe = env!("CARGO_BIN_EXE_agent");
    let initial = format!(
        r#"{{ "model": "m", "log_level": "info",
            "mcp_servers": [
              {{ "name": "mockA", "command": "{exe}",
                 "argv": ["--internal-mock-mcp", "file:///a.json", "--no-emit"] }}
            ],
            "subscribe": ["file:///a.json"] }}"#
    );
    let changed = format!(
        r#"{{ "model": "m", "log_level": "info",
            "mcp_servers": [
              {{ "name": "mockB", "command": "{exe}",
                 "argv": ["--internal-mock-mcp", "file:///b.json", "--no-emit"] }}
            ],
            "subscribe": ["file:///b.json"] }}"#
    );
    let (code, out) = run_reload_mcp_in_file(&initial, &changed);

    assert!(
        out.contains(r#""event":"proc.ready""#),
        "reactive never became ready:\n{out}"
    );
    assert!(
        out.contains(r#""event":"config.reloaded""#),
        "an mcp_servers re-handshake must be APPLIED, not rejected:\n{out}"
    );
    assert!(
        out.contains(r#""mcp_servers""#),
        "the changed list should name mcp_servers:\n{out}"
    );
    // The ADDED server is connected with reload provenance.
    assert!(
        out.contains(r#""event":"mcp.connect""#) && out.contains(r#""kind":"reload""#),
        "the added MCP server should re-handshake on reload:\n{out}"
    );
    // The REMOVED server's URI is unsubscribed and the NEW URI armed + read.
    assert!(
        out.contains(r#""event":"subscribe""#) && out.contains("file:///b.json"),
        "the added server's subscription should arm on reload:\n{out}"
    );
    // The tool set changed → never trips the daemon; it drains to exit 0.
    assert_eq!(code, Some(0), "expected graceful exit 0; stderr:\n{out}");
}

#[test]
fn sighup_contained_failure_when_an_added_mcp_server_cannot_spawn() {
    // RFC 0017 §5.3 contained-failure: a reload that ADDS a server which cannot
    // spawn (a nonexistent command) is CONTAINED — the failed add logs
    // mcp.connect.fail (the server is simply absent, a tool-domain absence, RFC
    // 0007), the reload still APPLIES (config.reloaded), and the daemon stays up +
    // drains to exit 0. It is NOT a rollback and NOT a daemon abort.
    let exe = env!("CARGO_BIN_EXE_agent");
    let initial = format!(
        r#"{{ "model": "m", "log_level": "info",
            "mcp_servers": [
              {{ "name": "mockA", "command": "{exe}",
                 "argv": ["--internal-mock-mcp", "file:///a.json", "--no-emit"] }}
            ],
            "subscribe": ["file:///a.json"] }}"#
    );
    // Keep mockA, ADD a server whose command does not exist → contained add-fail.
    let changed = format!(
        r#"{{ "model": "m", "log_level": "info",
            "mcp_servers": [
              {{ "name": "mockA", "command": "{exe}",
                 "argv": ["--internal-mock-mcp", "file:///a.json", "--no-emit"] }},
              {{ "name": "broken", "command": "/nonexistent/agentd-mcp-xyz", "argv": [] }}
            ],
            "subscribe": ["file:///a.json"] }}"#
    );
    let (code, out) = run_reload_mcp_in_file(&initial, &changed);

    assert!(
        out.contains(r#""event":"config.reloaded""#),
        "a reload with a contained add-failure must still APPLY:\n{out}"
    );
    assert!(
        out.contains(r#""event":"mcp.connect.fail""#) && out.contains("broken"),
        "the failed add should log mcp.connect.fail for the broken server:\n{out}"
    );
    // Contained: the daemon is unharmed and drains to exit 0 (no abort, no rollback).
    assert_eq!(
        code,
        Some(0),
        "a failed add must NOT abort the daemon; stderr:\n{out}"
    );
}
