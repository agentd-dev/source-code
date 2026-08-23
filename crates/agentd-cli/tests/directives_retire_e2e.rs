// SPDX-License-Identifier: AGPL-3.0-only
//! Colon-fence directives in the instruction (`:::workflow` / `:::skill`) and
//! graceful workflow retirement, end to end: an instruction that CARRIES its
//! workflow runs it; editing the instruction hot-swaps the definition; and a
//! definition that leaves the config lets its live runs finish (or cancels
//! them, when its `unload:` policy says so) instead of stranding them —
//! which is exactly what the old reload/delete paths did not guarantee.
#![cfg(all(unix, feature = "workflow"))]

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

// The daemon harness serves only the hot-reload tests; without that feature
// it is dead code, and the per-feature clippy row rightly says so.
#[cfg(feature = "hot-reload")]
struct Daemon {
    child: Child,
    err_path: String,
}
#[cfg(feature = "hot-reload")]
impl Daemon {
    fn spawn(cfg: &str) -> Daemon {
        let err_path = common::unique_path("dir-ret-daemon", "log");
        let errf = std::fs::File::create(&err_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", cfg])
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

// ---------------------------------------------------------------------------

#[test]
fn an_instruction_carries_its_workflow_and_its_skill() {
    let cfg = common::unique_path("dir-embed", "yaml");
    // The instruction is prose + machinery, in one document. NOTE the YAML
    // block scalar (`|`): the fences reach agentd verbatim.
    std::fs::write(
        &cfg,
        r#"config_version: "2"
agent:
  name: carried
  instruction: |
    You watch the queue and keep things tidy.

    :::workflow
    name: embedded
    steps:
      start: {kind: once}
      make:  {kind: assign, depends_on: [start], value: "made-by-embedded"}
      done:  {kind: finish, depends_on: [make], status: completed, output: "{{steps.make.output}}"}
    :::

    :::skill{name=tidy description="how to tidy"}
    Always sweep before you mop.
    :::

    :::context{title="ops notes"}
    The queue drains overnight.
    :::
lifecycle:
  run_until: idle
  idle_grace: 500ms
observability:
  log_level: info
"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    // The embedded workflow really ran, with the run's output on stdout.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("made-by-embedded"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        events(&stderr, "workflow.loaded")
            .iter()
            .any(|e| e["name"] == "embedded"),
        "{stderr}"
    );
    // The inline skill joined the catalogue, attributed to the instruction.
    assert!(
        events(&stderr, "skills.discovered")
            .iter()
            .any(|e| e["server"] == "instruction"
                && e["skills"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|s| s == "tidy"))),
        "{stderr}"
    );
    let _ = std::fs::remove_file(&cfg);
}

#[test]
fn an_unknown_directive_is_refused_at_startup_naming_the_known_set() {
    let cfg = common::unique_path("dir-bad", "yaml");
    std::fs::write(
        &cfg,
        "config_version: \"2\"\nagent:\n  name: x\n  instruction: |\n    :::workfow\n    name: typo\n    :::\nstore:\n  kind: none\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown directive")
            && stderr.contains("workflow, skill, context, example"),
        "{stderr}"
    );
    let _ = std::fs::remove_file(&cfg);
}

#[cfg(feature = "hot-reload")]
fn scheduled_cfg(version: &str) -> String {
    format!(
        r#"config_version: "2"
agent:
  name: swapper
  instruction: |
    Keep ticking.

    :::workflow
    name: tick
    steps:
      s: {{kind: schedule, every: 400ms}}
      v: {{kind: assign, depends_on: [s], value: "{version}"}}
      f: {{kind: finish, depends_on: [v], status: completed, output: "{{{{steps.v.output}}}}"}}
    :::
store:
  kind: memory
lifecycle:
  run_until: drained
observability:
  log_level: info
  log_content: true
"#
    )
}

#[cfg(feature = "hot-reload")]
#[test]
fn editing_the_instruction_hot_swaps_the_embedded_workflow() {
    let cfg = common::unique_path("dir-swap", "yaml");
    std::fs::write(&cfg, scheduled_cfg("v1")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |l| events(l, "run.done").iter().any(|e| e["output"] == "v1"),
        "a v1 run",
        10,
    );
    std::fs::write(&cfg, scheduled_cfg("v2")).unwrap();
    d.sighup();
    let log = d.wait_for(
        |l| events(l, "run.done").iter().any(|e| e["output"] == "v2"),
        "a v2 run after SIGHUP",
        10,
    );
    // The old definition left through the retirement path, named as replaced.
    assert!(
        events(&log, "workflow.retiring")
            .iter()
            .chain(events(&log, "workflow.unloaded").iter())
            .any(|e| e["workflow"] == "tick"),
        "{log}"
    );
    let _ = std::fs::remove_file(&cfg);
}

#[cfg(feature = "hot-reload")]
fn slow_cfg(unload: &str, with_wf: bool) -> String {
    // A `manual` workflow keeps the config valid (and the daemon shaped the
    // same) after `slowjob` is removed.
    let slow = if with_wf {
        format!(
            "  - name: slowjob\n    {unload}steps:\n\
             \x20     s: {{kind: once}}\n\
             \x20     nap: {{kind: sleep, depends_on: [s], duration: 2500ms}}\n\
             \x20     f: {{kind: finish, depends_on: [nap], status: completed}}\n"
        )
    } else {
        String::new()
    };
    format!(
        "config_version: \"2\"\nagent:\n  name: griefer\n\
         store:\n  kind: memory\n\
         workflows:\n  - name: idle\n    steps:\n\
         \x20     s: {{kind: manual}}\n\
         \x20     f: {{kind: finish, depends_on: [s]}}\n{slow}\
         lifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
    )
}

#[cfg(feature = "hot-reload")]
#[test]
fn a_removed_workflow_drains_its_live_run_instead_of_stranding_it() {
    let cfg = common::unique_path("dir-drain", "yaml");
    std::fs::write(&cfg, slow_cfg("", true)).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |l| !events(l, "run.start").is_empty(),
        "the run to start",
        10,
    );
    // Mid-sleep, the workflow leaves the config.
    std::fs::write(&cfg, slow_cfg("", false)).unwrap();
    d.sighup();
    let log = d.wait_for(
        |l| {
            events(l, "run.done")
                .iter()
                .any(|e| e["workflow"] == "slowjob")
        },
        "the orphaned run to finish",
        10,
    );
    assert!(
        events(&log, "run.done")
            .iter()
            .any(|e| e["workflow"] == "slowjob" && e["status"] == "completed"),
        "a drained run COMPLETES (it used to lose its definition):\n{log}"
    );
    assert!(
        events(&log, "workflow.retiring")
            .iter()
            .any(|e| e["workflow"] == "slowjob" && e["policy"] == "drain"),
        "{log}"
    );
    // …and once it finished, the pin was released.
    let log = d.wait_for(
        |l| {
            events(l, "workflow.unloaded")
                .iter()
                .any(|e| e["workflow"] == "slowjob")
        },
        "the pin to be garbage-collected",
        10,
    );
    drop(log);
    let _ = std::fs::remove_file(&cfg);
}

#[cfg(feature = "hot-reload")]
#[test]
fn unload_cancel_cancels_live_runs_on_retirement() {
    let cfg = common::unique_path("dir-cancel", "yaml");
    std::fs::write(&cfg, slow_cfg("unload: {policy: cancel}\n    ", true)).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |l| !events(l, "run.start").is_empty(),
        "the run to start",
        10,
    );
    std::fs::write(&cfg, slow_cfg("", false)).unwrap();
    d.sighup();
    let log = d.wait_for(
        |l| {
            events(l, "run.done")
                .iter()
                .any(|e| e["workflow"] == "slowjob")
        },
        "the run to be cancelled",
        10,
    );
    assert!(
        events(&log, "run.done")
            .iter()
            .any(|e| e["workflow"] == "slowjob" && e["status"] == "cancelled"),
        "unload: cancel cancels rather than drains:\n{log}"
    );
    let _ = std::fs::remove_file(&cfg);
}
