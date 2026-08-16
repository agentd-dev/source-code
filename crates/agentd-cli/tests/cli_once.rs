// SPDX-License-Identifier: AGPL-3.0-only
//! Black-box CLI test of the agentd 2.0 supervisor (RFC 0026): the binary loads
//! a v2 configuration, validates it, and — for a bare `once` job — spawns a root
//! turn worker and maps the outcome to an exit code. With an unreachable
//! intelligence endpoint the run fails fast with exit 4 (intel unavailable) and
//! must not hang or leak. The validation gate (exit 2) fires before any side
//! effect. A 1.x (`--mode`) invocation is rejected with a migration hint.

use std::process::Command;

#[test]
fn validate_config_rejects_retired_intelligence_transports() {
    // HTTPS-only intelligence: the retired unix:/vsock: schemes and non-loopback
    // plaintext http:// are exit 2 at the validation gate; https:// (and loopback
    // http://) pass. `--instruction`/`--intelligence` alias onto the v2 schema.
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
fn yaml_config_path_env_and_generic_flags_pass_the_validation_gate() {
    // Black-box: a v2 YAML config file, a path-derived env var, and a generic
    // `--<path>` flag all reach `--validate-config` through the real binary.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("agentd.yaml");
    std::fs::write(
        &cfg,
        "# yaml config\nconfig_version: \"2\"\nlimits:\n  max_runs: 4\n",
    )
    .unwrap();
    let base = |extra: &[&str], env: &[(&str, &str)]| {
        let mut cmd = Command::new(exe);
        cmd.args([
            "--validate-config",
            "--config",
            cfg.to_str().unwrap(),
            "--instruction",
            "x",
            "--intelligence",
            "https://intel.example",
        ]);
        cmd.args(extra);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("run agentd --validate-config")
    };
    let ok = base(
        &["--limits.max-runs", "9"],
        &[("AGENTD_LIMITS_INLINE_MAX_BYTES", "2048")],
    );
    assert_eq!(
        ok.status.code(),
        Some(0),
        "yaml + env path + generic flag validate; stderr:\n{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    // A mistyped path env var is refused at the gate, naming the variable.
    let bad = base(&[], &[("AGENTD_LIMITS_MAX_RUNS", "lots")]);
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert_eq!(bad.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(stderr.contains("AGENTD_LIMITS_MAX_RUNS"), "{stderr}");
    // So is a YAML typo (unknown key), before any side effect.
    std::fs::write(&cfg, "config_version: \"2\"\nmax_token: 5\n").unwrap();
    let typo = base(&[], &[]);
    let stderr = String::from_utf8_lossy(&typo.stderr);
    assert_eq!(typo.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(stderr.contains("max_token"), "{stderr}");
}

#[test]
fn a_once_job_exits_4_on_unreachable_intel() {
    // The `--instruction` sugar workflow (`once → agent → finish`) runs a root
    // turn; an unreachable endpoint fails fast with exit 4 and must not hang.
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
fn a_v1_mode_invocation_is_rejected_with_a_migration_hint() {
    // agentd 2.0 removed the mode drivers; `--mode` is a retired flag (exit 2).
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
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--mode"),
        "names the retired flag"
    );
}
