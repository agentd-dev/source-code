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
    config_with(workdir, cmd_step, "")
}

/// [`config`], with further workflow entries after `run`.
fn config_with(workdir: &str, cmd_step: &str, more_workflows: &str) -> String {
    format!(
        "\
         agent:\n  name: exec\n  instruction: x\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1\n  model: m\n\
         store:\n  kind: memory\n\
         security:\n  exec:\n    enabled: true\n    allow: [echo, sh]\n    workdir: {workdir}\n    timeout: 10s\n\
         workflows:\n  - name: run\n    steps:\n\
         \x20     s:    {{kind: once}}\n\
         {cmd_step}\n\
         \x20     done: {{kind: finish, depends_on: [run], output: \"{{{{steps.run.output}}}}\"}}\n\
         {more_workflows}\
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

/// The daemon's reactor reaps every exited child in the process while a
/// workflow's `exec` step waits on its own: run the step many times, several
/// at once, and every one still reports its command's exit.
///
/// The reactor reaps on each pass of its loop, and it passes on events and
/// at its tick — rarely enough, left alone, that an exec child's exit would
/// seldom fall between one pass and the runner's next look. So each command
/// outlives the runner's first look (a short sleep), and a second workflow
/// firing every few milliseconds keeps the reactor passing: an exit the
/// runner did not own the route of would be taken by the reactor nearly
/// every time.
#[test]
fn every_exec_step_reports_its_exit_while_the_reactor_reaps() {
    const RUNS: usize = 30;
    let workdir = common::unique_path("exec-wd", "d");
    std::fs::create_dir_all(&workdir).unwrap();
    let over = (0..RUNS)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let fan = format!(
        "\x20     run:  {{kind: foreach, depends_on: [s], over: [{over}], batch: {{size: 1, parallel: 4}},\n\
         \x20            body: {{steps: {{x: {{kind: tool, name: exec, args: {{cmd: sh, args: [\"-c\", \"/bin/sleep 0.02; echo n-{{{{item}}}}\"]}}}}}}}}}}"
    );
    let ticker = "\x20 - name: ticker\n    steps:\n\
                  \x20     s: {kind: schedule, every: 3ms}\n\
                  \x20     f: {kind: finish, depends_on: [s], status: completed}\n";
    let d = run(&config_with(&workdir, &fan, ticker));
    let deadline = Instant::now() + Duration::from_secs(60);
    let done = loop {
        let done: Vec<Value> = d
            .events("run.done")
            .into_iter()
            .filter(|e| e["workflow"] == "run")
            .collect();
        if !done.is_empty() {
            break done;
        }
        assert!(
            Instant::now() < deadline,
            "the exec workflow finished:\n{}",
            d.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    // How often the ticker got in depends on how busy the machine is; that
    // it got in while the commands ran is what makes the reactor pass.
    assert!(
        d.events("run.done")
            .iter()
            .any(|e| e["workflow"] == "ticker"),
        "the ticker kept the reactor passing:\n{}",
        d.stderr()
    );
    let outs = done[0]["output"]
        .as_array()
        .unwrap_or_else(|| panic!("one output per run: {}", done[0]))
        .clone();
    assert_eq!(outs.len(), RUNS, "{}", done[0]);
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(out["exit_code"], 0, "run {i} exits 0: {out}");
        assert_eq!(out["stdout"], format!("n-{i}\n"), "run {i}: {out}");
        assert_eq!(out["timed_out"], false, "run {i}: {out}");
    }
    drop(d);
    std::fs::remove_dir_all(&workdir).ok();
}

/// A turn's `exec` call refused before anything ran says why in the log —
/// the code in the worker's `tool.result` (`refused`) and in the
/// supervisor's `tool.refused` (`why`), the text only with content capture —
/// and a call that gives its optional arguments as `null`, as a model asked
/// for tools non-strictly does, runs. On the Responses API (`gpt-6.1-sol`),
/// as demo (d) is asked live: a refusal that was invisible in the log cost
/// that take.
#[test]
fn a_refused_exec_call_says_why_in_the_log_and_null_optionals_run() {
    let workdir = common::unique_path("exec-wd", "d");
    std::fs::create_dir_all(&workdir).unwrap();
    let play = common::unique_path("exec-play", "json");
    std::fs::write(
        &play,
        r#"{"turns": [
          {"tool_calls": [
            {"name": "exec", "arguments": {"cmd": "echo", "args": ["nulls-ok"], "cwd": null, "stdin": null, "timeout": null}},
            {"name": "exec", "arguments": {"cmd": "cat", "args": ["/etc/hostname"]}},
            {"name": "exec", "arguments": {"cmd": "echo", "workdir": "."}}
          ]},
          {"content": "done"}, {"content": "done"}, {"content": "done"}, {"content": "done"}
        ]}"#,
    )
    .unwrap();
    let cfg = format!(
        "agent:\n  name: exec\n  instruction: x\n  preflight: never\n\
         store:\n  kind: memory\n\
         security:\n  exec:\n    enabled: true\n    allow: [echo]\n    workdir: {workdir}\n    timeout: 10s\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 1s\n\
         observability:\n  log_level: info\n"
    );
    let cfg_path = common::unique_path("exec-cfg", "yaml");
    std::fs::write(&cfg_path, cfg).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .args(["--intelligence", &format!("mock:file:{play}")])
        .args(["--model", "gpt-6.1-sol", "--prompt", "run it"])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    let events = |name: &str| -> Vec<Value> {
        log.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == name && v["tool"] == "exec")
            .collect()
    };
    let results = events("tool.result");
    assert_eq!(results.len(), 3, "three exec results:\n{log}");
    assert_eq!(
        results[0]["is_error"], false,
        "null optionals run: {}",
        results[0]
    );
    assert!(results[0].get("refused").is_none(), "{}", results[0]);
    assert!(
        log.lines()
            .any(|l| l.contains("\"event\":\"exec.run\"") && l.contains("\"cmd\":\"echo\"")),
        "echo ran:\n{log}"
    );
    assert_eq!(results[1]["is_error"], true);
    assert_eq!(results[1]["refused"], "exec_not_allowed", "{}", results[1]);
    assert_eq!(results[2]["is_error"], true);
    assert_eq!(results[2]["refused"], "invalid_arguments", "{}", results[2]);
    for r in &results {
        assert!(r.get("error").is_none(), "no content without capture: {r}");
    }
    let why: Vec<Value> = events("tool.refused")
        .iter()
        .map(|e| e["why"].clone())
        .collect();
    assert_eq!(why, ["exec_not_allowed", "invalid_arguments"], "{log}");
    assert!(
        !log.contains("/etc/hostname"),
        "the arguments stay out of the log:\n{log}"
    );
    std::fs::remove_file(&play).ok();
    std::fs::remove_file(&cfg_path).ok();
    std::fs::remove_dir_all(&workdir).ok();
}
