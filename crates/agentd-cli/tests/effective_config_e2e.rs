// SPDX-License-Identifier: AGPL-3.0-only
//! `--effective-config`: what am I running, and who said so.
//!
//! Through the real binary from a temp directory, because the whole feature is
//! about layering — the discovery chain, the environment, the flags, the
//! conventional folders and an instruction's own `:::!config` — and a unit
//! test over the loader would assert the parts and miss the arrangement.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

fn run_in(cwd: &Path, home: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(args)
        .current_dir(cwd)
        .env_remove("AGENT_CONFIG")
        .env_remove("AGENTD_CONFIG")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn project(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = PathBuf::from(common::unique_path(tag, "d"));
    let (home, work) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(home.join(".config/agentd")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    (root, home, work)
}

/// Every layer is attributed to the thing that set it — including the FILE,
/// not just "a file", because the three discovery rungs exist precisely so a
/// person's defaults, a checkout's settings and one machine's overrides can
/// disagree.
#[test]
fn the_report_names_the_layer_that_set_each_value() {
    let (root, home, work) = project("eff-layers");
    std::fs::write(
        home.join(".config/agentd/config.yml"),
        "config_version: \"1\"\nagent: { name: from-user, instruction: user-policy, preflight: never }\n\
         intelligence: { endpoints: [\"https://intel.example/v1\"], model: from-user }\n\
         store: { kind: memory }\n",
    )
    .unwrap();
    std::fs::write(
        work.join("agentd.yml"),
        "agent: { name: from-project }\nintelligence: { model: from-project }\n",
    )
    .unwrap();
    std::fs::create_dir_all(work.join("workflows")).unwrap();
    std::fs::write(
        work.join("workflows/triage.yaml"),
        "name: triage\nsteps:\n  s: { kind: manual }\n  f: { kind: finish, depends_on: [s] }\n",
    )
    .unwrap();

    let (code, stdout, _) = run_in(
        &work,
        &home,
        &["--effective-config", "--model", "from-flag"],
    );
    assert_eq!(code, Some(0), "{stdout}");
    let v: Value = serde_json::from_str(&stdout).expect("stdout is the JSON report");
    let p = &v["provenance"];

    assert_eq!(v["config"]["agent"]["name"], "from-project");
    assert!(
        p["agent.name"].as_str().unwrap().ends_with("agentd.yml"),
        "the project rung won and is named: {p}"
    );
    assert!(
        p["agent.instruction"]
            .as_str()
            .unwrap()
            .contains("config.yml"),
        "a value only the user rung set is credited to it: {p}"
    );
    assert_eq!(v["config"]["intelligence"]["model"], "from-flag");
    assert_eq!(
        p["intelligence.model"], "flag",
        "the flag wins over both files"
    );
    assert_eq!(
        p["workflows"], "convention (folder beside the config)",
        "the `workflows/` folder is a layer of its own: {p}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The report runs on a BROKEN config — that is when it is most wanted — and
/// says so rather than reading as a clean bill of health.
#[test]
fn an_invalid_config_still_reports_and_names_its_errors() {
    let (root, home, work) = project("eff-invalid");
    std::fs::write(
        work.join("agentd.yml"),
        "config_version: \"1\"\nagent: { name: broken, instruction: policy, preflight: never }\n\
         intelligence: { endpoints: [\"https://intel.example/v1\"], model: m }\n\
         store: { kind: memory }\n\
         mcp:\n  servers:\n    - name: gw\n      endpoint: https://gw.example/mcp\n      \
         headers: { authorization: \"Bearer LIVE-VALUE\" }\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_in(&work, &home, &["--effective-config"]);
    assert_eq!(code, Some(0), "a report is not a verdict: {stderr}");
    let v: Value = serde_json::from_str(&stdout).expect("the report still prints");
    assert_eq!(v["config"]["agent"]["name"], "broken");
    assert!(
        stderr.contains("config.invalid") && stderr.contains("looks like a credential"),
        "the errors are reported beside the document: {stderr}"
    );
    // …and the live credential is not in the report.
    assert!(
        !stdout.contains("LIVE-VALUE"),
        "a credential-shaped value must be redacted:\n{stdout}"
    );
    assert_eq!(
        v["config"]["mcp"]["servers"][0]["headers"]["authorization"],
        "<redacted>"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Two things a config file alone cannot tell you: what an instruction's own
/// `:::!config` put into effect, and the workflow a bare `--instruction` run
/// generates. Both are the report's job.
#[test]
fn the_report_shows_what_the_document_declared_and_what_was_generated() {
    let (root, home, work) = project("eff-document");
    std::fs::write(
        work.join("agent.md"),
        "You are the desk.\n\n:::!config\nlimits: { max_runs: 7 }\n:::\n",
    )
    .unwrap();
    std::fs::write(
        work.join("agentd.yml"),
        "config_version: \"1\"\nagent: { name: doc, instruction: ./agent.md, preflight: never }\n\
         intelligence: { endpoints: [\"https://intel.example/v1\"], model: m }\n\
         store: { kind: memory }\n",
    )
    .unwrap();

    let (code, stdout, _) = run_in(&work, &home, &["--effective-config"]);
    assert_eq!(code, Some(0), "{stdout}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        v["document_config"]["limits"]["max_runs"], 7,
        "a setting only the instruction declared is otherwise invisible: {stdout}"
    );
    assert_eq!(
        v["provenance"]["workflows"], "generated (the one-shot sugar workflow)",
        "the report describes what a RUN would do, so the synthesized workflow is in it"
    );
    assert_eq!(v["config"]["workflows"][0]["name"], "main");

    let _ = std::fs::remove_dir_all(&root);
}
