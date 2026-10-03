// SPDX-License-Identifier: AGPL-3.0-only
//! A FOLDER of documents as one instruction, through the real binary.
//!
//! The property that matters is not "the files were concatenated" — it is that
//! the result is ONE instruction document: machinery declared in the second
//! file folds into the configuration exactly as it would had someone pasted
//! the folder into a single markdown file, and the front matter of a later
//! document does not end up read as prose in the middle of the text.
#![cfg(unix)]

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

/// A file's end matter (S27) is its record, never delivered — and in a
/// folder, every file's but the last would otherwise sit mid-text where
/// nothing reads it as end matter. Each file's is dropped, without a
/// warning: dropping it is what delivery does anyway.
#[test]
fn every_files_end_matter_is_dropped_from_the_folder() {
    let dir = common::unique_path("instr-folder-em", "d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        format!("{dir}/10-a.md"),
        "First file.\n\n---\nowner: team-one\n---\n",
    )
    .unwrap();
    std::fs::write(
        format!("{dir}/20-b.md"),
        "Second file.\n\n---\nowner: team-two\n---\n",
    )
    .unwrap();
    let (loaded, _) = agentd::config::settings::load(
        &[
            "--instruction".to_string(),
            dir.clone(),
            "--model".to_string(),
            "mock".to_string(),
        ],
        &[],
    )
    .unwrap_or_else(|e| panic!("the folder did not load: {e:?}"));
    let instruction = loaded.settings.agent.instruction.expect("an instruction");
    assert!(
        instruction.contains("First file.") && instruction.contains("Second file."),
        "{instruction}"
    );
    for gone in ["owner:", "team-one", "team-two"] {
        assert!(
            !instruction.contains(gone),
            "{gone:?} delivered:\n{instruction}"
        );
    }
    assert!(
        !loaded.warnings.iter().any(|w| w.contains("end matter")),
        "end matter is dropped without a word: {:?}",
        loaded.warnings
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed end matter is a defect in ONE file, and the refusal says which.
#[test]
fn malformed_end_matter_refuses_the_folder_naming_the_file() {
    let dir = common::unique_path("instr-folder-em-bad", "d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/10-a.md"), "First file.\n").unwrap();
    std::fs::write(
        format!("{dir}/20-b.md"),
        "Second file.\n\n---\nowner: [unclosed\n---\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--instruction", &dir, "--validate-config"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{log}");
    assert!(
        log.contains("20-b.md: end matter is not valid YAML"),
        "the refusal names the file: {log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder reads the way the same files read alone. A `---` block that is
/// body in the last file — its real end matter follows it — stays body in
/// the folder too: the combined document ends in that file's own end matter,
/// so only that is dropped, where splitting it off first made the body block
/// the folder's last and dropped it as end matter.
#[test]
fn a_body_block_before_the_last_files_end_matter_stays_text() {
    const LAST: &str = "Second file, with a sample record:\n\n---\nkind: sample\n---\n\n---\nowner: team-two\n---\n";
    let dir = common::unique_path("instr-folder-framed", "d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/10-a.md"), "First file.\n").unwrap();
    std::fs::write(format!("{dir}/20-b.md"), LAST).unwrap();
    let file = common::unique_path("instr-file-framed", "md");
    std::fs::write(&file, LAST).unwrap();
    let instruction_of = |source: &str| {
        let (loaded, _) = agentd::config::settings::load(
            &[
                "--instruction".to_string(),
                source.to_string(),
                "--model".to_string(),
                "mock".to_string(),
            ],
            &[],
        )
        .unwrap_or_else(|e| panic!("{source} did not load: {e:?}"));
        loaded.settings.agent.instruction.expect("an instruction")
    };
    for (what, instruction) in [
        ("folder", instruction_of(&dir)),
        ("file", instruction_of(&file)),
    ] {
        assert!(
            instruction.contains("---\nkind: sample\n---"),
            "the {what} dropped the body block:\n{instruction}"
        );
        assert!(
            !instruction.contains("team-two"),
            "the {what} delivered its end matter:\n{instruction}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&file);
}
