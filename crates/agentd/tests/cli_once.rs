// SPDX-License-Identifier: Apache-2.0
//! Black-box CLI test of supervised once-mode (M2): the binary spawns a root
//! subagent, supervises it, and maps the outcome to an exit code. With an
//! unreachable intelligence endpoint the run fails fast with exit 4 (intel
//! unavailable) and must not hang or leak.

use std::process::Command;

#[test]
fn supervised_once_exits_4_on_unreachable_intel() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let out = Command::new(exe)
        .args([
            "--instruction",
            "do a thing",
            "--intelligence",
            "unix:/nonexistent/agentd-cli-test.sock",
            "--model",
            "m",
            "--log-level",
            "error",
        ])
        .output()
        .expect("run agentd");

    assert_eq!(
        out.status.code(),
        Some(4),
        "expected exit 4 (intel unavailable); stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn bad_flag_exits_2() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let out = Command::new(exe)
        .arg("--no-such-flag")
        .output()
        .expect("run agentd");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn reactive_requires_a_subscription() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let out = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "hi",
            "--intelligence",
            "unix:/x",
        ])
        .output()
        .expect("run agentd");
    assert_eq!(out.status.code(), Some(2)); // validation: needs --subscribe
}

#[test]
fn reactive_exits_6_when_required_mcp_server_is_down() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let out = Command::new(exe)
        .args([
            "--mode",
            "reactive",
            "--instruction",
            "react",
            "--intelligence",
            "unix:/x",
            "--subscribe",
            "file:///in.json",
            "--mcp",
            "bad=/nonexistent/mcp-server",
            "--log-level",
            "error",
        ])
        .output()
        .expect("run agentd");
    assert_eq!(
        out.status.code(),
        Some(6),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
