// SPDX-License-Identifier: AGPL-3.0-only
//! The whole agent from ONE markdown file (RFC 0034 + the config-defining
//! directives): `agentd --instruction-file agent.md`, no `--config` at all.
//! The document declares its runtime (`:::config`), an event stream
//! (`:::stream`), and two workflows (`:::workflow`) that talk to each other
//! over that stream — and the process runs them and exits clean. Also: the
//! precedence rule, end to end — an explicit flag beats the document's
//! fragment.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

use serde_json::Value;

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

const AGENT_MD: &str = r#"You are the order desk. Every paid order is fulfilled.

:::config
store: { kind: memory }
lifecycle: { run_until: idle, idle_grace: 900ms }
observability: { log_level: info, log_content: true }
limits: { max_runs: 20 }
:::

:::stream{name=orders}
retention: { max_events: 100 }
:::

:::workflow
name: producer
steps:
  s:   { kind: once, policy: always }
  pub: { kind: emit, depends_on: [s], stream: orders, subject: order.paid, data: { n: 7 } }
  f:   { kind: finish, depends_on: [pub], status: completed }
:::

:::workflow
name: fulfil
steps:
  take: { kind: stream, stream: orders, subject: "order.*", from: earliest }
  f:    { kind: finish, depends_on: [take], status: completed, output: "shipped #{{steps.take.output.data.n}}" }
:::
"#;

#[test]
fn one_markdown_file_defines_and_runs_the_whole_agent() {
    let dir = common::unique_path("instr-only", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let md = format!("{dir}/agent.md");
    std::fs::write(&md, AGENT_MD).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--instruction-file", &md])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert_eq!(events(&log, "stream.emit").len(), 1, "{log}");
    let shipped: Vec<String> = events(&log, "run.done")
        .iter()
        .filter(|e| e["workflow"] == "fulfil" && e["status"] == "completed")
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .collect();
    assert_eq!(shipped, vec!["shipped #7"], "{log}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_explicit_flag_beats_the_documents_fragment() {
    let dir = common::unique_path("instr-only-prec", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let md = format!("{dir}/agent.md");
    std::fs::write(&md, AGENT_MD).unwrap();

    // The document's fragment says `observability.log_level: info`; the flag
    // says warn. If the flag wins, the info-level run telemetry is absent —
    // while the agent still runs (exit 0 proves the rest of the fragment,
    // store and workflows included, was honoured).
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--instruction-file",
            &md,
            "--observability.log_level",
            "warn",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert!(
        events(&log, "run.done").is_empty() && events(&log, "stream.emit").is_empty(),
        "the flag's warn level must beat the document's info:\n{log}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
