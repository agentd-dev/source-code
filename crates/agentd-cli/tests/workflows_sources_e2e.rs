// SPDX-License-Identifier: AGPL-3.0-only
//! A workflow NAME is one definition, whichever sources it arrives from: an
//! inline entry, a `file:`, a `dir:` folder, the instruction document's
//! `:::!workflow`. Two that resolve to one name are refused — at startup with
//! exit 2, at reload with nothing applied — and the refusal names BOTH
//! sources. Before, the later one silently replaced the earlier in the loaded
//! map: the operator's workflow changed without a word, and a folder's file
//! was invisible to the entry-level check because its name lives inside it.
//!
//! And a configured name is changed where it is configured: the workflow
//! tools refuse to update, delete or create it, naming its source, while a
//! workflow created at runtime stays theirs to change.
#![cfg(unix)]

mod common;

#[cfg(feature = "hot-reload")]
use std::process::Child;
use std::process::{Command, Stdio};
#[cfg(feature = "hot-reload")]
use std::time::{Duration, Instant};

use serde_json::Value;

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A one-shot workflow file whose run prints `output`.
fn workflow_file(name: &str, output: &str) -> String {
    format!(
        "name: {name}\nsteps:\n\
         \x20 s: {{kind: once}}\n\
         \x20 v: {{kind: assign, depends_on: [s], value: \"{output}\"}}\n\
         \x20 f: {{kind: finish, depends_on: [v], status: completed, output: \"{{{{steps.v.output}}}}\"}}\n"
    )
}

/// An instruction document carrying one `:::!workflow` block.
fn document(name: &str, output: &str) -> String {
    format!(
        "Keep things tidy.\n\n\
         :::!workflow{{name={name}}}\n\
         steps:\n\
         \x20 s: {{kind: once}}\n\
         \x20 v: {{kind: assign, depends_on: [s], value: \"{output}\"}}\n\
         \x20 f: {{kind: finish, depends_on: [v], status: completed, output: \"{{{{steps.v.output}}}}\"}}\n\
         :::\n"
    )
}

/// A job-shaped config: the folder's workflows plus the document's.
fn config(dir: &std::path::Path, instruction: &std::path::Path) -> String {
    format!(
        "agent:\n  name: sources\n  instruction: {}\n\
         workflows:\n  - dir: {}\n\
         store:\n  kind: memory\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 300ms\n\
         observability:\n  log_level: info\n",
        instruction.display(),
        dir.display()
    )
}

fn run(cfg: &std::path::Path) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg.to_string_lossy()])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The `config.invalid` errors a refused start logged.
/// The refusals of a start: `config.invalid` lines from the loader, and the
/// plain `agentd: …` line of a refusal while the configuration loads, which
/// is where a local file's or folder's definition is judged.
fn invalid(stderr: &str) -> Vec<String> {
    events(stderr, "config.invalid")
        .iter()
        .filter_map(|e| e["error"].as_str().map(str::to_string))
        .chain(
            stderr
                .lines()
                .filter_map(|l| l.strip_prefix("agentd: ").map(str::to_string)),
        )
        .collect()
}

#[test]
fn a_folder_file_and_a_document_block_sharing_a_name_are_refused_naming_both() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    let file = wf.join("dup.yaml");
    std::fs::write(&file, workflow_file("dup", "from-the-file")).unwrap();
    let doc = t.path().join("agent.md");
    std::fs::write(&doc, document("dup", "from-the-document")).unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, config(&wf, &doc)).unwrap();

    let (code, stdout, stderr) = run(&cfg);
    assert_eq!(code, Some(2), "stderr:\n{stderr}");
    let errs = invalid(&stderr);
    let refusal = errs
        .iter()
        .find(|e| e.contains("\"dup\" is defined twice"))
        .unwrap_or_else(|| panic!("no refusal naming the workflow:\n{stderr}"));
    assert!(
        refusal.contains(&format!("file {}", file.display())),
        "names the folder's file: {refusal}"
    );
    assert!(
        refusal.contains(&format!("the instruction document {}", doc.display())),
        "names the document: {refusal}"
    );
    // Neither definition ran: the refusal is the whole outcome. Nor is
    // either reported loaded — a `workflow.loaded` for the first one, ahead
    // of the refusal, read as if it had gone live.
    assert!(
        !stdout.contains("from-the-file") && !stdout.contains("from-the-document"),
        "stdout: {stdout}"
    );
    assert!(events(&stderr, "workflow.loaded").is_empty(), "{stderr}");
}

#[test]
fn two_files_in_one_folder_sharing_a_name_are_refused_naming_both() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    let (a, b) = (wf.join("a.yaml"), wf.join("b.yaml"));
    std::fs::write(&a, workflow_file("dup", "from-a")).unwrap();
    std::fs::write(&b, workflow_file("dup", "from-b")).unwrap();
    let doc = t.path().join("agent.md");
    std::fs::write(&doc, "Keep things tidy.\n").unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, config(&wf, &doc)).unwrap();

    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(2), "stderr:\n{stderr}");
    let errs = invalid(&stderr);
    let refusal = errs
        .iter()
        .find(|e| e.contains("\"dup\" is defined twice"))
        .unwrap_or_else(|| panic!("no refusal naming the workflow:\n{stderr}"));
    // Folder order is name order, so `a` is the first definition and `b` the
    // second — the same message on every machine.
    let (at_a, at_b) = (
        refusal.find(&format!("file {}", a.display())),
        refusal.find(&format!("file {}", b.display())),
    );
    assert!(
        at_a.is_some() && at_b.is_some() && at_a < at_b,
        "names both files, first then second: {refusal}"
    );
}

#[test]
fn distinct_names_from_every_source_all_load() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    std::fs::write(wf.join("a.yaml"), workflow_file("one", "ran-one")).unwrap();
    std::fs::write(wf.join("b.yaml"), workflow_file("two", "ran-two")).unwrap();
    let doc = t.path().join("agent.md");
    std::fs::write(&doc, document("three", "ran-three")).unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, config(&wf, &doc)).unwrap();

    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let loaded: Vec<Value> = events(&stderr, "workflow.loaded")
        .iter()
        .map(|e| e["name"].clone())
        .collect();
    let completed: Vec<Value> = events(&stderr, "run.done")
        .iter()
        .filter(|e| e["status"] == "completed")
        .map(|e| e["workflow"].clone())
        .collect();
    for name in ["one", "two", "three"] {
        assert!(
            loaded.contains(&Value::from(name)),
            "{name} loaded: {stderr}"
        );
        assert!(
            completed.contains(&Value::from(name)),
            "{name} ran: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Runtime edits: a configured name is changed in its configuration.
//
// `workflow.update` of a configured workflow used to be stored and reported as
// success, and the next start or workflow reload quietly put the configured
// definition back; a `workflow.delete` came back the same way. Now the tools
// refuse the name, saying where it is defined, and a workflow the runtime
// created stays the runtime's to change.

/// A definition the model may write: a manual start, so it never runs.
fn definition(name: &str) -> serde_json::Value {
    serde_json::json!({"name": name, "steps": {
        "s": {"kind": "manual"},
        "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}}})
}

/// A playbook that makes each call in its own turn, then answers with the
/// last call's result. The mock picks a turn by how many tool results the
/// transcript holds, so one call per turn keeps the order the list gives —
/// and a conversation a file store kept from an earlier run starts `past`
/// results in, so that many turns are never reached.
fn one_call_per_turn(past: usize, calls: &[serde_json::Value]) -> String {
    let mut turns = vec![serde_json::json!({"content": "never reached"}); past];
    turns.extend(calls.iter().map(|c| serde_json::json!({"tool_calls": [c]})));
    turns.push(serde_json::json!({"echo_tool_result": true}));
    serde_json::json!({ "turns": turns }).to_string()
}

/// What the model answered, in order: the playbooks end by echoing the last
/// tool result, so this is what that tool handed the model.
fn replies(stderr: &str) -> String {
    events(stderr, "turn.reply")
        .iter()
        .filter_map(|e| e["text"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n")
}

fn call(tool: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"name": tool, "arguments": args})
}

/// A job whose model plays `play`: the folder's workflows, the document's,
/// and a file store, so what one run stores the next one loads.
fn model_config(
    t: &std::path::Path,
    dir: &std::path::Path,
    instruction: &std::path::Path,
) -> String {
    format!(
        "agent: {{ name: edits, prompt: go, instruction: {} }}\n\
         workflows:\n  - dir: {}\n\
         store: {{ kind: file, file: {{ path: {} }} }}\n\
         intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 300ms }}\n\
         observability: {{ log_level: info, log_content: true }}\n",
        instruction.display(),
        dir.display(),
        t.join("state").display(),
        t.join("play.json").display()
    )
}

/// The `(tool, name, configured source)` of every refused edit.
fn refused(stderr: &str) -> Vec<(String, String, String)> {
    events(stderr, "workflow.refused")
        .iter()
        .map(|e| {
            let s = |k: &str| e[k].as_str().unwrap_or_default().to_string();
            (s("tool"), s("name"), s("configured"))
        })
        .collect()
}

/// `(op, name)` of every definition the tools wrote.
fn defined(stderr: &str) -> Vec<(String, String)> {
    events(stderr, "workflow.defined")
        .iter()
        .map(|e| {
            let s = |k: &str| e[k].as_str().unwrap_or_default().to_string();
            (s("op"), s("name"))
        })
        .collect()
}

#[test]
fn a_configured_workflow_is_refused_to_update_create_and_delete_naming_its_source() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    let file = wf.join("tick.yaml");
    std::fs::write(&file, workflow_file("tick", "from-the-file")).unwrap();
    let doc = t.path().join("agent.md");
    std::fs::write(&doc, document("doc", "from-the-document")).unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, model_config(t.path(), &wf, &doc)).unwrap();
    std::fs::write(
        t.path().join("play.json"),
        one_call_per_turn(
            0,
            &[
                call(
                    "workflow.update",
                    serde_json::json!({"name": "tick", "definition": definition("tick")}),
                ),
                call(
                    "workflow.create",
                    serde_json::json!({"definition": definition("tick")}),
                ),
                call(
                    "workflow.update",
                    serde_json::json!({"name": "doc", "definition": definition("doc")}),
                ),
                call("workflow.delete", serde_json::json!({"name": "doc"})),
                // The name the definition is stored under is the one parsing
                // settles: padded, it is still `tick`.
                call(
                    "workflow.update",
                    serde_json::json!({"name": "other", "definition": definition(" tick ")}),
                ),
                // An update aimed at `tick` is refused as that, whatever the
                // definition it carries is called.
                call(
                    "workflow.update",
                    serde_json::json!({"name": "tick", "definition": definition("mine")}),
                ),
                call("workflow.delete", serde_json::json!({"name": "tick"})),
            ],
        ),
    )
    .unwrap();

    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let from_file = format!("file {}", file.display());
    let from_doc = format!("the instruction document {}", doc.display());
    let got = refused(&stderr);
    for (tool, name, source) in [
        ("workflow.update", "tick", &from_file),
        ("workflow.create", "tick", &from_file),
        ("workflow.update", "doc", &from_doc),
        ("workflow.delete", "doc", &from_doc),
        ("workflow.delete", "tick", &from_file),
    ] {
        assert!(
            got.iter()
                .any(|(t, n, s)| t == tool && n == name && s.contains(source.as_str())),
            "{tool} of {name} refused naming {source}: {got:?}\n{stderr}"
        );
    }
    let updates_of_tick = got
        .iter()
        .filter(|(t, n, s)| t == "workflow.update" && n == "tick" && s.contains(&from_file))
        .count();
    assert_eq!(updates_of_tick, 3, "{got:?}\n{stderr}");
    assert_eq!(got.len(), 7, "every edit was refused: {got:?}\n{stderr}");
    // A refusal writes nothing and removes nothing.
    assert!(defined(&stderr).is_empty(), "{stderr}");
    assert!(events(&stderr, "workflow.deleted").is_empty(), "{stderr}");
    // What the model reads says where to change it: the last refusal, echoed.
    let answer = replies(&stderr);
    assert!(
        answer.contains("workflow.delete: workflow \\\"tick\\\" is defined in the configuration")
            && answer.contains(&from_file)
            && answer.contains("change it there"),
        "the model's answer: {answer}"
    );
}

#[test]
fn a_workflow_created_at_runtime_is_updated_and_deleted() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    std::fs::write(wf.join("tick.yaml"), workflow_file("tick", "from-the-file")).unwrap();
    let doc = t.path().join("agent.md");
    std::fs::write(&doc, document("doc", "from-the-document")).unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, model_config(t.path(), &wf, &doc)).unwrap();
    std::fs::write(
        t.path().join("play.json"),
        one_call_per_turn(
            0,
            &[
                call(
                    "workflow.create",
                    serde_json::json!({"definition": definition("mine")}),
                ),
                call(
                    "workflow.update",
                    serde_json::json!({"name": "mine", "definition": definition("mine")}),
                ),
                call("workflow.delete", serde_json::json!({"name": "mine"})),
            ],
        ),
    )
    .unwrap();

    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(
        defined(&stderr),
        [
            ("workflow.create".to_string(), "mine".to_string()),
            ("workflow.update".to_string(), "mine".to_string())
        ],
        "{stderr}"
    );
    assert!(
        events(&stderr, "workflow.deleted")
            .iter()
            .any(|e| e["name"] == "mine"),
        "{stderr}"
    );
    assert!(refused(&stderr).is_empty(), "{stderr}");
    assert!(replies(&stderr).contains("\"ok\":true"), "{stderr}");
}

/// A name the runtime created that the configuration later defines is the
/// configuration's from then on: its definition loads, the stored one is
/// reported discarded, and an edit is refused. A name the configuration
/// stops defining is free again — nothing stored comes back in its place,
/// and it can be created. Across restarts here; the
/// reload test below takes the same steps through SIGHUP.
#[test]
fn a_name_moves_between_the_runtime_and_the_configuration_across_restarts() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    std::fs::write(wf.join("keep.yaml"), workflow_file("keep", "kept")).unwrap();
    let doc = t.path().join("agent.md");
    std::fs::write(&doc, "Keep things tidy.\n").unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, model_config(t.path(), &wf, &doc)).unwrap();
    let play = t.path().join("play.json");
    let tick = wf.join("tick.yaml");
    let gone = wf.join("gone.yaml");

    // The runtime creates `tick`.
    std::fs::write(
        &play,
        one_call_per_turn(
            0,
            &[call(
                "workflow.create",
                serde_json::json!({"definition": definition("tick")}),
            )],
        ),
    )
    .unwrap();
    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(
        defined(&stderr),
        [("workflow.create".to_string(), "tick".to_string())],
        "{stderr}"
    );

    // The configuration now defines `tick` (and `gone`): the file's runs, the
    // stored one is reported shadowed, and the model's update is refused.
    std::fs::write(&tick, workflow_file("tick", "from-the-file")).unwrap();
    std::fs::write(&gone, workflow_file("gone", "configured")).unwrap();
    std::fs::write(
        &play,
        one_call_per_turn(
            1,
            &[call(
                "workflow.update",
                serde_json::json!({"name": "tick", "definition": definition("tick")}),
            )],
        ),
    )
    .unwrap();
    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let from_file = format!("file {}", tick.display());
    assert!(
        events(&stderr, "workflow.stored.shadowed")
            .iter()
            .any(|e| e["name"] == "tick"
                && e["configured"]
                    .as_str()
                    .is_some_and(|c| c.contains(&from_file))
                && e["hash"].as_str().is_some_and(|h| h.len() == 12)),
        "the stored definition is reported discarded, naming the configured source and its own hash: {stderr}"
    );
    assert!(
        events(&stderr, "workflow.loaded")
            .iter()
            .all(|e| e["source"] != "store"),
        "{stderr}"
    );
    assert!(
        events(&stderr, "run.done")
            .iter()
            .any(|e| e["workflow"] == "tick" && e["output"] == "from-the-file"),
        "the file's tick ran: {stderr}"
    );
    assert!(
        refused(&stderr)
            .iter()
            .any(|(t, n, s)| t == "workflow.update" && n == "tick" && s.contains(&from_file)),
        "{stderr}"
    );
    assert!(defined(&stderr).is_empty(), "{stderr}");

    // Both files go, and both names are free: the stored `tick` was
    // discarded when the configuration took the name, so nothing loads in
    // its place — the old runtime definition is not armed again — and each
    // name can be created.
    std::fs::remove_file(&tick).unwrap();
    std::fs::remove_file(&gone).unwrap();
    std::fs::write(
        &play,
        one_call_per_turn(
            2,
            &[
                call(
                    "workflow.create",
                    serde_json::json!({"definition": definition("tick")}),
                ),
                call(
                    "workflow.create",
                    serde_json::json!({"definition": definition("gone")}),
                ),
            ],
        ),
    )
    .unwrap();
    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert!(
        events(&stderr, "workflow.loaded")
            .iter()
            .all(|e| e["source"] != "store"),
        "the discarded definition came back: {stderr}"
    );
    assert_eq!(
        defined(&stderr),
        [
            ("workflow.create".to_string(), "tick".to_string()),
            ("workflow.create".to_string(), "gone".to_string())
        ],
        "{stderr}"
    );
    assert!(refused(&stderr).is_empty(), "{stderr}");
}

// ---------------------------------------------------------------------------
// Reload: the same refusal, and the running definition stays.

#[cfg(feature = "hot-reload")]
struct Daemon {
    child: Child,
    err_path: String,
}
#[cfg(feature = "hot-reload")]
impl Daemon {
    fn spawn(cfg: &std::path::Path) -> Daemon {
        let err_path = common::unique_path("wf-sources-daemon", "log");
        let errf = std::fs::File::create(&err_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg.to_string_lossy()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf))
            .spawn()
            .expect("spawn daemon");
        Daemon { child, err_path }
    }
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }
    fn wait_for(&self, pred: impl Fn(&str) -> bool, what: &str, secs: u64) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let log = self.stderr();
            if pred(&log) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    fn sighup(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
    }
}
#[cfg(feature = "hot-reload")]
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.err_path);
    }
}

/// A long-lived config: the folder's scheduled `tick`, an instruction that
/// may or may not carry a block of its own, and any further sections.
#[cfg(feature = "hot-reload")]
fn daemon_config(dir: &std::path::Path, instruction: &str, extra: &str) -> String {
    let indented: String = instruction.lines().map(|l| format!("    {l}\n")).collect();
    format!(
        "agent:\n  name: sources\n  instruction: |\n{indented}\
         workflows:\n  - dir: {}\n\
         store:\n  kind: memory\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n{extra}",
        dir.display()
    )
}

#[cfg(feature = "hot-reload")]
fn done_with(log: &str, output: &str) -> usize {
    events(log, "run.done")
        .iter()
        .filter(|e| e["output"] == output)
        .count()
}

#[cfg(feature = "hot-reload")]
#[test]
fn a_reload_that_would_define_a_name_twice_is_refused_and_the_running_one_stays() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    let file = wf.join("tick.yaml");
    let tick = |output: &str| {
        format!(
            "name: tick\nsteps:\n\
             \x20 s: {{kind: schedule, every: 300ms}}\n\
             \x20 v: {{kind: assign, depends_on: [s], value: \"{output}\"}}\n\
             \x20 f: {{kind: finish, depends_on: [v], status: completed, output: \"{{{{steps.v.output}}}}\"}}\n"
        )
    };
    std::fs::write(&file, tick("from-the-file")).unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, daemon_config(&wf, "Keep ticking.", "")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |l| done_with(l, "from-the-file") >= 1,
        "a run of the file's tick",
        10,
    );

    // The document now declares `tick` too — and the file changed in the
    // same edit, so a refusal that kept whatever had loaded before the
    // collision (the file's NEW definition) would show as a new output. The
    // same edit adds an MCP server: a refused reload must not dial it.
    std::fs::write(&file, tick("from-the-file-v2")).unwrap();
    std::fs::write(
        &cfg,
        daemon_config(
            &wf,
            &document("tick", "from-the-document"),
            "mcp:\n  servers:\n    - name: probe\n      endpoint: http://127.0.0.1:9/mcp\n",
        ),
    )
    .unwrap();
    d.sighup();
    let log = d.wait_for(
        |l| !events(l, "config.reload.invalid").is_empty(),
        "the reload refusal",
        10,
    );
    let refusal = events(&log, "config.reload.invalid")
        .iter()
        // The load's refusal, quoted inside the reload's error.
        .filter_map(|e| e["error"].as_str().map(|m| m.replace("\\\"", "\"")))
        .find(|e| e.contains("\"tick\" is defined twice"))
        .unwrap_or_else(|| panic!("no refusal naming the workflow:\n{log}"));
    assert!(
        refusal.contains(&format!("file {}", file.display()))
            && refusal.contains("the instruction document"),
        "names both sources: {refusal}"
    );

    // The running definition is untouched: it keeps firing, neither new
    // one runs, and nothing was retired on the refused reload's account.
    let before = done_with(&log, "from-the-file");
    let log = d.wait_for(
        |l| done_with(l, "from-the-file") >= before + 2,
        "the file's tick firing on after the refusal",
        10,
    );
    assert_eq!(done_with(&log, "from-the-document"), 0, "{log}");
    assert_eq!(done_with(&log, "from-the-file-v2"), 0, "{log}");
    assert!(
        events(&log, "workflow.retiring")
            .iter()
            .chain(events(&log, "workflow.unloaded").iter())
            .all(|e| e["workflow"] != "tick"),
        "{log}"
    );
    // Nothing else of the refused edit applied either: the server it added
    // was never dialed, and the file's new definition was never reported
    // as loaded.
    assert!(
        events(&log, "mcp.connect")
            .iter()
            .chain(events(&log, "mcp.connect.fail").iter())
            .all(|e| e["server"] != "probe"),
        "the refused reload dialed its new MCP server:\n{log}"
    );
    assert_eq!(
        events(&log, "workflow.loaded").len(),
        1,
        "only the startup load of tick is logged:\n{log}"
    );

    // …and the running config is the one it started with: putting the
    // original files back and reloading changes nothing. Had the refused
    // reload left its instruction and servers live, this one would report
    // moving them back.
    std::fs::write(&file, tick("from-the-file")).unwrap();
    std::fs::write(&cfg, daemon_config(&wf, "Keep ticking.", "")).unwrap();
    d.sighup();
    let log = d.wait_for(
        |l| !events(l, "config.reloaded").is_empty(),
        "the restoring reload",
        10,
    );
    let changed = events(&log, "config.reloaded")[0]["changed"].clone();
    for section in ["agent.instruction", "mcp", "workflows"] {
        assert!(
            !changed
                .as_array()
                .is_some_and(|c| c.iter().any(|v| v == section)),
            "the refused reload had applied {section}: {changed}\n{log}"
        );
    }
}

/// The same moves through SIGHUP: a reload that newly defines a name the
/// runtime created makes it the configuration's (and discards the stored
/// one), and a reload that drops a configured name frees it. The set of configured names follows every
/// reload, not only the start.
#[cfg(all(feature = "hot-reload", feature = "a2a"))]
#[test]
fn a_reload_moves_a_name_between_the_runtime_and_the_configuration() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    std::fs::write(wf.join("keep.yaml"), workflow_file("keep", "kept")).unwrap();
    let play = t.path().join("play.json");
    let cfg = t.path().join("agent.yaml");
    let (d, addr) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            daemon_config(
                &wf,
                "Keep things tidy.",
                &format!(
                    "intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
                     a2a: {{ listen: \"http://127.0.0.1:{port}\" }}\n",
                    play.display()
                ),
            ),
        )
        .unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.err_path.clone();
        (d, log)
    });
    // One conversation per step: each starts its playbook from the top.
    let converse = |calls: &[serde_json::Value]| {
        std::fs::write(&play, one_call_per_turn(0, calls)).unwrap();
        let sent = common::SendMessage::text("go").post(&addr);
        assert_eq!(
            sent["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
            "{sent}"
        );
    };
    let reload = |n: usize| {
        d.sighup();
        d.wait_for(
            |l| events(l, "config.reloaded").len() >= n,
            "the reload",
            10,
        )
    };

    converse(&[call(
        "workflow.create",
        serde_json::json!({"definition": definition("tick")}),
    )]);
    assert_eq!(
        defined(&d.stderr()),
        [("workflow.create".to_string(), "tick".to_string())],
        "{}",
        d.stderr()
    );

    let tick = wf.join("tick.yaml");
    let gone = wf.join("gone.yaml");
    std::fs::write(&tick, workflow_file("tick", "from-the-file")).unwrap();
    std::fs::write(&gone, workflow_file("gone", "configured")).unwrap();
    let log = reload(1);
    let from_file = format!("file {}", tick.display());
    assert!(
        events(&log, "workflow.stored.shadowed")
            .iter()
            .any(|e| e["name"] == "tick"),
        "{log}"
    );
    converse(&[
        call(
            "workflow.update",
            serde_json::json!({"name": "tick", "definition": definition("tick")}),
        ),
        call("workflow.delete", serde_json::json!({"name": "gone"})),
    ]);
    let log = d.stderr();
    let got = refused(&log);
    assert!(
        got.iter()
            .any(|(t, n, s)| t == "workflow.update" && n == "tick" && s.contains(&from_file)),
        "the reload made tick the configuration's: {got:?}\n{log}"
    );
    assert!(
        got.iter()
            .any(|(t, n, _)| t == "workflow.delete" && n == "gone"),
        "{got:?}\n{log}"
    );
    assert_eq!(defined(&log).len(), 1, "{log}");

    std::fs::remove_file(&tick).unwrap();
    std::fs::remove_file(&gone).unwrap();
    reload(2);
    converse(&[
        call(
            "workflow.create",
            serde_json::json!({"definition": definition("tick")}),
        ),
        call(
            "workflow.create",
            serde_json::json!({"definition": definition("gone")}),
        ),
    ]);
    let log = d.stderr();
    assert_eq!(
        defined(&log)[1..],
        [
            ("workflow.create".to_string(), "tick".to_string()),
            ("workflow.create".to_string(), "gone".to_string())
        ],
        "the reload that dropped them freed both names:\n{log}"
    );
    assert_eq!(refused(&log).len(), 2, "{log}");
}

/// A reload refused while it installs its workflows keeps the running set —
/// and with it the names that set's configuration owns. The refused edit
/// drops `tick` from the configuration; were the refused set's names taken
/// anyway, the still-running configured `tick` could be edited, and the next
/// load would undo it.
#[cfg(all(feature = "hot-reload", feature = "a2a"))]
#[test]
fn a_refused_reload_keeps_the_names_the_running_configuration_owns() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    let tick = wf.join("tick.yaml");
    std::fs::write(&tick, workflow_file("tick", "from-the-file")).unwrap();
    let play = t.path().join("play.json");
    let cfg = t.path().join("agent.yaml");
    let (d, addr) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            daemon_config(
                &wf,
                "Keep things tidy.",
                &format!(
                    "intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
                     a2a: {{ listen: \"http://127.0.0.1:{port}\" }}\n",
                    play.display()
                ),
            ),
        )
        .unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.err_path.clone();
        (d, log)
    });

    // The edit: `tick` goes, and a workflow that can only be refused at
    // install (its tool is known to no registry) arrives.
    std::fs::remove_file(&tick).unwrap();
    std::fs::write(
        wf.join("bad.yaml"),
        "name: bad\nsteps:\n\
         \x20 s: {kind: manual}\n\
         \x20 x: {kind: tool, depends_on: [s], name: no_such_tool}\n\
         \x20 f: {kind: finish, depends_on: [x], status: completed}\n",
    )
    .unwrap();
    d.sighup();
    let log = d.wait_for(
        |l| !events(l, "config.reload.invalid").is_empty(),
        "the reload refusal",
        10,
    );
    assert!(
        events(&log, "config.reload.invalid")
            .iter()
            .any(|e| e["error"]
                .as_str()
                .is_some_and(|m| m.contains("no_such_tool"))),
        "refused for the unknown tool:\n{log}"
    );

    std::fs::write(
        &play,
        one_call_per_turn(
            0,
            &[
                call(
                    "workflow.update",
                    serde_json::json!({"name": "tick", "definition": definition("tick")}),
                ),
                call("workflow.delete", serde_json::json!({"name": "tick"})),
            ],
        ),
    )
    .unwrap();
    let sent = common::SendMessage::text("go").post(&addr);
    assert_eq!(
        sent["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );
    let log = d.stderr();
    let from_file = format!("file {}", tick.display());
    let got = refused(&log);
    for tool in ["workflow.update", "workflow.delete"] {
        assert!(
            got.iter()
                .any(|(t, n, s)| t == tool && n == "tick" && s.contains(&from_file)),
            "{tool} of the running configured tick is refused: {got:?}\n{log}"
        );
    }
    assert!(defined(&log).is_empty(), "{log}");
    assert!(events(&log, "workflow.deleted").is_empty(), "{log}");
}
