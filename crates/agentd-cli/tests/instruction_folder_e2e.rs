// SPDX-License-Identifier: AGPL-3.0-only
//! A FOLDER of documents as one instruction, through the real binary.
//!
//! The property that matters is not "the files were concatenated" — it is that
//! the result is ONE instruction document: machinery declared in the second
//! file folds into the configuration exactly as it would had someone pasted
//! the folder into a single markdown file, and the front matter of a later
//! document does not end up read as prose in the middle of the text.
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

/// The first document: front matter (kept), the policy prose, and the runtime.
const POLICY: &str = r#"---
id: ins_folder_1
---
You are the order desk.

:::!config
store: { kind: memory }
lifecycle: { run_until: idle, idle_grace: 900ms }
observability: { log_level: info }
limits: { max_runs: 20 }
:::
"#;

/// The second: machinery only. It must fold, which is the whole test.
const WORKFLOW: &str = r#"---
id: ins_folder_2
---
Every paid order is fulfilled.

:::!workflow{name=fulfil}
steps:
  s: { kind: once, policy: always }
  f: { kind: finish, depends_on: [s], status: completed, output: "shipped" }
:::
"#;

/// Not a document by the default glob — it must not be swept in.
const NOTES: &str = "internal: true\n";

fn write_folder(tag: &str) -> String {
    let dir = common::unique_path(tag, "d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/10-policy.md"), POLICY).unwrap();
    std::fs::write(format!("{dir}/20-workflow.md"), WORKFLOW).unwrap();
    std::fs::write(format!("{dir}/30-notes.yaml"), NOTES).unwrap();
    dir
}

#[test]
fn a_folder_of_documents_is_one_instruction() {
    let dir = write_folder("instr-folder");

    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--instruction", &dir])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{log}");

    // The machinery from the SECOND document folded into the configuration.
    let done = events(&log, "run.done")
        .iter()
        .filter(|e| e["workflow"] == "fulfil" && e["status"] == "completed")
        .count();
    assert_eq!(done, 1, "the second document's workflow ran: {log}");

    // …and the load said which documents it combined, in the order it used.
    let loaded = events(&log, "instruction.loaded");
    let combined = loaded
        .iter()
        .find(|e| e["files"].is_array())
        .unwrap_or_else(|| panic!("no instruction.loaded naming its files: {log}"));
    let files: Vec<String> = combined["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| {
            f.as_str()
                .map(|s| s.rsplit('/').next().unwrap().to_string())
        })
        .collect();
    assert_eq!(
        files,
        vec!["10-policy.md", "20-workflow.md"],
        "in name order, and `30-notes.yaml` is not a document: {log}"
    );
    assert_eq!(combined["order"], "name");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The delivered instruction is one document: the first file's front matter is
/// kept, the second's is dropped — with the drop reported, not silent.
#[test]
fn a_later_documents_front_matter_is_dropped_out_loud() {
    let dir = write_folder("instr-folder-fm");

    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--instruction", &dir, "--validate-config"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{log}");
    let warned = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == "config.warning")
        .any(|v| {
            v["warning"]
                .as_str()
                .or_else(|| v["msg"].as_str())
                .is_some_and(|w| w.contains("front matter") && w.contains("20-workflow.md"))
        });
    assert!(warned, "the dropped front matter is reported: {log}");

    let _ = std::fs::remove_dir_all(&dir);
}
