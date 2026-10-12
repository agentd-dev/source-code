// SPDX-License-Identifier: AGPL-3.0-only
//! Black-box CLI test of the agentd supervisor: the binary loads a
//! configuration, validates it, and — for a bare `once` job — spawns a root
//! turn worker and maps the outcome to an exit code. With an unreachable
//! intelligence endpoint the run fails fast with exit 4 (intel unavailable) and
//! must not hang or leak. The validation gate (exit 2) fires before any side
//! effect.

use std::process::Command;

#[test]
fn validate_config_rejects_non_https_intelligence() {
    // HTTPS-only intelligence: socket schemes and non-loopback plaintext
    // http:// are exit 2 at the validation gate; https:// (and loopback
    // http://) pass. `--instruction`/`--intelligence` alias onto the settings.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let run = |intel: &str| {
        Command::new(exe)
            .args([
                "--validate-config",
                "--instruction",
                "x",
                "--intelligence",
                intel,
                "--model",
                "m-1",
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
    // Black-box: a YAML config file, a path-derived env var, and a generic
    // `--<path>` flag all reach `--validate-config` through the real binary.
    let exe = env!("CARGO_BIN_EXE_agentd");
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("agentd.yaml");
    std::fs::write(&cfg, "# yaml config\nlimits:\n  max_runs: 4\n").unwrap();
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
            "--model",
            "m-1",
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
    std::fs::write(&cfg, "max_token: 5\n").unwrap();
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

/// The built-in mock model runs where there is no temporary directory: the
/// image is built `FROM scratch` and has none. It announced its address
/// through a file there, so in the image every turn on it failed with exit 4.
#[test]
fn a_mock_model_needs_no_temporary_directory() {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--instruction",
            "say hi",
            "--intelligence",
            "mock:final",
            "--agent.preflight",
            "never",
            "--log-level",
            "error",
        ])
        .env("TMPDIR", "/nonexistent/agentd-no-tmp")
        .output()
        .expect("run agentd");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("mock-llm done"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A `--prompt` one-shot is a turn, not a `once` run, and its turn's outcome
/// is the job's exit code: an unreachable model is exit 4 here too, where it
/// used to end the job 0 with nothing on stdout.
#[test]
fn a_prompt_exits_4_on_unreachable_intel() {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--prompt",
            "say hello",
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

/// What a workflow names that the configuration does not declare — an
/// `mcp.tool` step's server, a `stream` step's stream — is refused by
/// `--validate-config`, as startup refuses it; it passed validation and
/// failed the first start with exit 2.
#[test]
fn validate_refuses_an_undeclared_server_or_stream() {
    for (step, says) in [
        (
            "{kind: mcp.tool, depends_on: [s], server: ghost, tool: t}",
            "mcp server \"ghost\" is not declared under mcp.servers",
        ),
        (
            "{kind: emit, depends_on: [s], stream: ghost, subject: x, data: {}}",
            "stream \"ghost\" is not declared under `streams:`",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("c.yaml");
        std::fs::write(
            &cfg,
            format!(
                "agent: {{name: c}}\nstore: {{kind: memory}}\nintelligence: {{endpoints: \"mock:final\", model: m}}\nworkflows:\n  - name: w\n    steps:\n      s: {{kind: once}}\n      x: {step}\n      f: {{kind: finish, depends_on: [x]}}\n"
            ),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--validate-config", "--config"])
            .arg(&cfg)
            .output()
            .expect("run agentd");
        // The refusal is a JSON log line: its quotes arrive escaped.
        let stderr = String::from_utf8_lossy(&out.stderr).replace("\\\"", "\"");
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(says), "{stderr}");
    }
}

/// A templated `emit` target is the run's to render — any declared stream —
/// and loads, in a body as at the top level.
#[test]
fn validate_accepts_a_templated_emit_target_in_a_body() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("c.yaml");
    std::fs::write(
        &cfg,
        "agent: {name: c}\nstore: {kind: memory}\nintelligence: {endpoints: \"mock:final\", model: m}\nstreams: {orders: {}}\nworkflows:\n  - name: w\n    steps:\n      s: {kind: once}\n      g: {kind: subgraph, depends_on: [s], body: {steps: {e: {kind: emit, stream: \"{{ inputs.target }}\", subject: x, data: {}}}}}\n      f: {kind: finish, depends_on: [g]}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--validate-config", "--config"])
        .arg(&cfg)
        .output()
        .expect("run agentd");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A declared server that is down at boot does not stop the start: it joins
/// when it comes up, and a step calling it before then fails when it runs.
#[test]
fn a_step_on_a_declared_server_down_at_boot_fails_when_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("c.yaml");
    std::fs::write(
        &cfg,
        "agent: {name: c}\nstore: {kind: memory}\nintelligence: {endpoints: \"mock:final\", model: m}\nmcp:\n  servers:\n    - {name: down, endpoint: \"http://127.0.0.1:9/mcp\"}\nworkflows:\n  - name: w\n    steps:\n      s: {kind: once}\n      x: {kind: mcp.tool, depends_on: [s], server: down, tool: t}\n      f: {kind: finish, depends_on: [x]}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config"])
        .arg(&cfg)
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("mcp server \\\"down\\\" is not connected"),
        "the step failed on the server it called:\n{stderr}"
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
