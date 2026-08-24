// SPDX-License-Identifier: AGPL-3.0-only
//! The guarded `exec` tool, end to end: a workflow `tool` node runs an
//! allow-listed local command and observes `{stdout, exit_code, …}`; a command
//! that is NOT allow-listed is refused. Runs only with `--features exec`
//! AND `security.exec.enabled` — agentd's default is no local execution.
#![cfg(all(unix, feature = "exec"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
    fn events(&self, name: &str) -> Vec<Value> {
        self.stderr()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == name)
            .collect()
    }
    fn wait_done(&self, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if !self.events("run.done").is_empty() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

fn run(cfg: &str) -> Daemon {
    let cfg_path = common::unique_path("exec-cfg", "yaml");
    std::fs::write(&cfg_path, cfg).unwrap();
    let stderr_path = common::unique_path("exec-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn exec daemon");
    Daemon { child, stderr_path }
}

fn config(workdir: &str, cmd_step: &str) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: exec\n  instruction: x\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1\n  model: m\n\
         store:\n  kind: memory\n\
         security:\n  exec:\n    enabled: true\n    allow: [echo]\n    workdir: {workdir}\n    timeout: 10s\n\
         workflows:\n  - name: run\n    steps:\n\
         \x20     s:    {{kind: once}}\n\
         {cmd_step}\n\
         \x20     done: {{kind: finish, depends_on: [run], output: \"{{{{steps.run.output}}}}\"}}\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 1s\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

#[test]
fn an_allow_listed_command_runs_and_a_denied_one_is_refused() {
    let workdir = common::unique_path("exec-wd", "d");
    std::fs::create_dir_all(&workdir).unwrap();

    // 1. `echo` is allow-listed → it runs and its output is observable.
    let allowed = "\x20     run:  {kind: tool, depends_on: [s], name: exec, args: {cmd: echo, args: [\"hi-from-exec\"]}}";
    let d = run(&config(&workdir, allowed));
    assert!(
        d.wait_done(15),
        "the exec workflow finished:\n{}",
        d.stderr()
    );
    let done = d.events("run.done");
    let out = &done[0]["output"];
    assert_eq!(out["exit_code"], 0, "echo exits 0: {out}");
    assert_eq!(out["stdout"], "hi-from-exec\n", "stdout is captured: {out}");
    assert_eq!(out["timed_out"], false);
    drop(d);

    // 2. `cat` is NOT in the allow-list → the tool call is refused (the step fails).
    let denied = "\x20     run:  {kind: tool, depends_on: [s], name: exec, args: {cmd: cat, args: [\"/etc/passwd\"]}}";
    let d = run(&config(&workdir, denied));
    assert!(
        d.wait_done(15),
        "the run reached a terminal state:\n{}",
        d.stderr()
    );
    let done = d.events("run.done");
    assert_eq!(
        done[0]["status"], "failed",
        "a non-allow-listed command fails the run: {}",
        done[0]
    );
    assert!(
        d.stderr().contains("not in security.exec.allow"),
        "the refusal names the allow-list:\n{}",
        d.stderr()
    );

    std::fs::remove_dir_all(&workdir).ok();
}
