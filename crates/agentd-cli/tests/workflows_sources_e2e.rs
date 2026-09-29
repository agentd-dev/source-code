// SPDX-License-Identifier: AGPL-3.0-only
//! A workflow NAME is one definition, whichever sources it arrives from: an
//! inline entry, a `file:`, a `dir:` folder, the instruction document's
//! `:::!workflow`. Two that resolve to one name are refused — at startup with
//! exit 2, at reload with nothing applied — and the refusal names BOTH
//! sources. Before, the later one silently replaced the earlier in the loaded
//! map: the operator's workflow changed without a word, and a folder's file
//! was invisible to the entry-level check because its name lives inside it.
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
fn invalid(stderr: &str) -> Vec<String> {
    events(stderr, "config.invalid")
        .iter()
        .filter_map(|e| e["error"].as_str().map(str::to_string))
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

/// A runtime-stored definition under a configured name does not load — the
/// configuration is where that name lives — and the daemon SAYS so. A
/// `workflow.update` of a configured workflow is stored and reported as
/// success, and without the line the next start dropped it without a word.
#[test]
fn a_stored_definition_under_a_configured_name_is_reported_not_loaded() {
    let t = tempfile::tempdir().unwrap();
    let wf = t.path().join("workflows");
    std::fs::create_dir(&wf).unwrap();
    let file = wf.join("tick.yaml");
    std::fs::write(&file, workflow_file("tick", "from-the-file")).unwrap();
    let update = serde_json::json!({"turns": [
        {"tool_calls": [{"name": "workflow.update", "arguments": {"name": "tick", "definition": {
            "name": "tick", "steps": {
                "s": {"kind": "manual"},
                "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}}}}}]},
        {"echo_tool_result": true}]});
    let play = t.path().join("play.json");
    let cfg = t.path().join("agent.yaml");
    let write_cfg = |prompt: &str| {
        std::fs::write(
            &cfg,
            format!(
                "agent: {{ name: shadow, prompt: {prompt} }}\n\
                 workflows:\n  - dir: {}\n\
                 store: {{ kind: file, file: {{ path: {} }} }}\n\
                 intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
                 lifecycle: {{ run_until: idle, idle_grace: 300ms }}\n\
                 observability: {{ log_level: info, log_content: true }}\n",
                wf.display(),
                t.path().join("state").display(),
                play.display()
            ),
        )
        .unwrap();
    };

    std::fs::write(&play, update.to_string()).unwrap();
    write_cfg("update");
    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert!(
        events(&stderr, "workflow.defined")
            .iter()
            .any(|e| e["name"] == "tick" && e["op"] == "workflow.update"),
        "the update is stored and reported: {stderr}"
    );

    std::fs::write(&play, r#"{"turns": [{"text": "done"}]}"#).unwrap();
    write_cfg("again");
    let (code, _, stderr) = run(&cfg);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let shadowed = events(&stderr, "workflow.stored.shadowed");
    assert!(
        shadowed.iter().any(|e| e["name"] == "tick"
            && e["configured"]
                .as_str()
                .is_some_and(|c| c.contains(&format!("file {}", file.display())))),
        "the stored definition is reported not loaded, naming the configured source: {stderr}"
    );
    assert!(
        events(&stderr, "workflow.loaded")
            .iter()
            .all(|e| e["source"] != "store"),
        "{stderr}"
    );
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
        .filter_map(|e| e["error"].as_str().map(str::to_string))
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
