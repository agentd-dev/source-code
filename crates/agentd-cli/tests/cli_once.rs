// SPDX-License-Identifier: Apache-2.0
//! Black-box CLI test of supervised once-mode (M2): the binary spawns a root
//! subagent, supervises it, and maps the outcome to an exit code. With an
//! unreachable intelligence endpoint the run fails fast with exit 4 (intel
//! unavailable) and must not hang or leak.

use std::process::Command;

#[test]
fn validate_config_rejects_retired_intelligence_transports() {
    // HTTPS-only intelligence (target-vision pivot, Phase 1): the retired unix:/
    // vsock: schemes and non-loopback plaintext http:// are exit 2 at the
    // validation gate; https:// (and loopback http://) pass.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let run = |intel: &str| {
        Command::new(exe)
            .args([
                "--validate-config",
                "--instruction",
                "x",
                "--intelligence",
                intel,
            ])
            .output()
            .expect("run agentd --validate-config")
    };
    for bad in [
        "unix:/run/intel.sock",
        "vsock:3:8080",
        "http://intel.example:8080",
    ] {
        let out = run(bad);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{bad} must be exit 2; stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for good in ["https://intel.example", "http://127.0.0.1:9"] {
        let out = run(good);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{good} must validate; stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn supervised_once_exits_4_on_unreachable_intel() {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let out = Command::new(exe)
        .args([
            "--instruction",
            "do a thing",
            "--intelligence",
            "http://127.0.0.1:9",
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
            "http://127.0.0.1:9",
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
            "http://127.0.0.1:9",
            "--subscribe",
            "file:///in.json",
            "--mcp",
            "bad=http://127.0.0.1:9",
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
