// SPDX-License-Identifier: AGPL-3.0-only
//! A unit of work keeps its trace across what happens to it, and hands it on
//! to the work it causes:
//!
//! * a run resumed after the process died — its interrupted step replayed —
//!   carries on in the trace it started in, not the new life's, and a step
//!   the restore skips or fails instead is said to be so in that trace;
//! * a run that another run starts (a child workflow, a signal) continues the
//!   starter's trace, one span below it;
//! * an operator's `observability.traceparent` is continued as given: its
//!   trace id, its unsampled flag on every hop below;
//! * the work a process was started to do is a span of the process trace in
//!   a job, and a trace of its own in a daemon;
//! * a subagent a run kills is killed in the run's trace;
//! * an instance child — a whole daemon of its own — continues the trace of
//!   the unit that spawned it, and its retirement and exit are said in it.
#![cfg(unix)]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long a bounded life may take before the test calls it wedged.
const HARD_TIMEOUT: Duration = Duration::from_secs(30);

/// agentd's JSON log lines.
fn lines(stderr: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn events(stderr: &str, name: &str) -> Vec<Value> {
    lines(stderr)
        .into_iter()
        .filter(|v| v["event"] == name)
        .collect()
}

/// The trace id a `traceparent` names.
fn trace_of(tp: &Value) -> &str {
    let tp = tp
        .as_str()
        .unwrap_or_else(|| panic!("no traceparent: {tp}"));
    assert_eq!(tp.len(), 55, "not a W3C traceparent: {tp}");
    &tp[3..35]
}

/// The mock LLM over HTTP with a `file:` playbook; killed on drop.
struct MockLlm {
    child: Child,
    addr_file: String,
    log_path: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
        let _ = std::fs::remove_file(&self.log_path);
    }
}
impl MockLlm {
    /// The W3C trace headers of every model call that reached the mock.
    fn traces(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("MOCK_LLM_TRACE "))
            .filter_map(|t| serde_json::from_str(t).ok())
            .collect()
    }
}

fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("trace-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("trace-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let log_path = common::unique_path("trace-llm", "log");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log_path).expect("mock llm log"))
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        log_path,
        uri: format!("http://{addr}"),
    }
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("trace", "yaml");
    std::fs::write(&path, yaml).expect("write config");
    path
}

/// Spawn a daemon with its stderr on disk, so the test can read it while the
/// process is alive.
fn spawn_daemon(config: &str) -> (Child, String) {
    let err_path = common::unique_path("trace", "err");
    let err = std::fs::File::create(&err_path).expect("create stderr file");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn agentd");
    (child, err_path)
}

/// Wait for `child` to exit, killing (and failing) it if it overruns.
fn wait_bounded(mut child: Child, err_path: &str, what: &str) -> (Option<i32>, String) {
    let deadline = Instant::now() + HARD_TIMEOUT;
    loop {
        match child.try_wait().expect("wait for agentd") {
            Some(status) => {
                let log = std::fs::read_to_string(err_path).unwrap_or_default();
                let _ = std::fs::remove_file(err_path);
                return (status.code(), log);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let log = std::fs::read_to_string(err_path).unwrap_or_default();
                panic!("{what}: agentd never exited (waited {HARD_TIMEOUT:?}).\nstderr:\n{log}");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn run_config(config: &str) -> (Option<i32>, String) {
    let (child, err_path) = spawn_daemon(config);
    wait_bounded(child, &err_path, "the daemon")
}

/// A run whose process is killed mid-step resumes in the next life in the
/// trace it started in: the replay of the interrupted step, the steps after
/// it, every line about the run and every MCP call it makes carry the first
/// life's trace id — a new life's process trace is not the run's.
/// A file-store daemon whose `once` run calls `mock.slow` (its step's
/// `on_replay` set to `on_replay`, when given) and then `state.list`: its
/// config and its state directory.
fn resume_config(mock: &common::MockMcp, on_replay: Option<&str>) -> (String, String) {
    let state = common::unique_path("trace-resume", "state");
    let _ = std::fs::remove_dir_all(&state);
    let mut slow = json!({"kind": "mcp.tool", "depends_on": ["start"], "server": "mock",
                          "tool": "mock.slow", "args": {"ms": 1500}});
    if let Some(p) = on_replay {
        slow["on_replay"] = json!(p);
    }
    let cfg = write_config(&format!(
        "agent:\n  name: resume\nstore:\n  kind: file\n  file:\n    path: {state}\n  checkpoint:\n    debounce_ms: 0\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nworkflows:\n  - name: resume\n    steps: {}\nlifecycle:\n  run_until: idle\n  idle_grace: 500ms\nobservability:\n  log_level: info\n",
        mock.uri(),
        json!({
            "start": {"kind": "once"},
            "slow": slow,
            "after": {"kind": "mcp.tool", "depends_on": ["slow"], "server": "mock",
                      "tool": "state.list", "args": {"key": "resume"}},
            "done": {"kind": "finish", "depends_on": ["after"], "status": "completed"}
        })
    ));
    (cfg, state)
}

/// Life 1 of [`resume_config`]: killed while its slow call is in flight.
/// Its log.
fn killed_mid_call(cfg: &str, mock: &common::MockMcp) -> String {
    let (child, err_path) = spawn_daemon(cfg);
    let deadline = Instant::now() + HARD_TIMEOUT;
    while !mock
        .metas()
        .iter()
        .any(|m| m["method"] == "tools/call" && m["_meta"]["agent/run"].is_string())
    {
        assert!(
            Instant::now() < deadline,
            "the slow call never reached the server"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let mut child = child;
    let _ = child.wait();
    let first = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    first
}

#[test]
fn a_run_resumed_after_a_restart_keeps_its_trace() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let (cfg, state) = resume_config(&mock, None);
    // Life 1: die while the slow call is in flight.
    let first = killed_mid_call(&cfg, &mock);
    let start = events(&first, "run.start");
    assert_eq!(start.len(), 1, "life 1 started the run:\n{first}");
    let run = start[0]["run"].as_str().unwrap().to_string();
    let trace = start[0]["trace_id"].as_str().unwrap().to_string();

    // Life 2: the run is restored, its interrupted step replayed.
    let (code, second) = run_config(&cfg);
    assert_eq!(code, Some(0), "life 2 finished the run:\n{second}");
    let process = events(&second, "proc.start")[0]["trace_id"].clone();
    assert_ne!(process, trace.as_str(), "a new life has a trace of its own");
    let replayed = events(&second, "restore.step.replay");
    assert_eq!(replayed.len(), 1, "{second}");
    assert_eq!(replayed[0]["trace_id"], trace.as_str(), "{second}");
    let about_run: Vec<Value> = lines(&second)
        .into_iter()
        .filter(|l| l["run"] == run.as_str())
        .collect();
    for event in ["step.start", "step.done", "run.done"] {
        assert!(
            about_run.iter().any(|l| l["event"] == event),
            "no {event} in life 2:\n{second}"
        );
    }
    for l in &about_run {
        assert_eq!(l["trace_id"], trace.as_str(), "{l}");
    }
    // Both attempts of the slow call, and the step after them, are the run's.
    let calls: Vec<Value> = mock
        .metas()
        .into_iter()
        .filter(|m| m["method"] == "tools/call" && m["_meta"]["agent/run"] == run.as_str())
        .collect();
    assert_eq!(
        calls.len(),
        3,
        "the call, its replay, the next step: {calls:?}"
    );
    for c in &calls {
        assert_eq!(trace_of(&c["_meta"]["traceparent"]), trace, "{c}");
    }
    assert_eq!(
        calls[0]["_meta"]["agent/idempotency_key"], calls[1]["_meta"]["agent/idempotency_key"],
        "the replay is the same operation: {calls:?}"
    );
    assert_ne!(
        calls[0]["_meta"]["traceparent"], calls[1]["_meta"]["traceparent"],
        "each attempt is a span of its own: {calls:?}"
    );
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_file(&cfg);
}

/// A step the restore does not replay — its `on_replay` is `skip` or `fail`
/// — is said to be skipped or failed in its run's trace, not the new life's.
#[test]
fn a_step_the_restore_skips_or_fails_is_said_in_its_runs_trace() {
    for (policy, event) in [
        ("skip", "restore.step.skipped"),
        ("fail", "restore.step.failed"),
    ] {
        let mock = common::spawn_mock_mcp("mock://noop", false);
        let (cfg, state) = resume_config(&mock, Some(policy));
        let first = killed_mid_call(&cfg, &mock);
        let trace = events(&first, "run.start")[0]["trace_id"].clone();
        let (_, second) = run_config(&cfg);
        assert_ne!(
            events(&second, "proc.start")[0]["trace_id"],
            trace,
            "{policy}: a new life has a trace of its own"
        );
        let said = events(&second, event);
        assert_eq!(said.len(), 1, "{policy}:\n{second}");
        assert_eq!(said[0]["trace_id"], trace, "{policy}: {}", said[0]);
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_file(&cfg);
    }
}

/// A subagent a run kills is killed in the trace it was spawned in: the
/// run's, not the process's. The child's call outlasts the kill step, so the
/// child is running when it is killed; one already inside that call is ended
/// by the kill's ladder at its grace, long before the call would return, and
/// the signal is said in the run's trace too.
#[test]
fn a_subagent_a_run_kills_is_killed_in_the_runs_trace() {
    let play = common::unique_path("trace-kill", "json");
    std::fs::write(
        &play,
        json!({"match": [{"when_contains": "SUB-TASK", "delay_ms": 60000, "content": "late"}]})
            .to_string(),
    )
    .unwrap();
    let cfg = write_config(&format!(
        "agent:\n  name: killer\nstore:\n  kind: memory\nintelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\nworkflows:\n  - name: kill\n    steps: {}\nlifecycle:\n  run_until: idle\n  idle_grace: 500ms\nobservability:\n  log_level: info\n",
        json!({
            "start": {"kind": "loop", "max_iterations": 1},
            "sub": {"kind": "subagent", "depends_on": ["start"], "mode": "async",
                    "instruction": "SUB-TASK"},
            "kill": {"kind": "tool", "depends_on": ["sub"], "name": "subagent.kill",
                     "args": {"handle": "{{steps.sub.output.handle}}"}},
            "done": {"kind": "finish", "depends_on": ["kill"], "status": "completed"}
        })
    ));
    let (code, log) = run_config(&cfg);
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
    assert_eq!(code, Some(0), "{log}");
    let process = events(&log, "proc.start")[0]["trace_id"].clone();
    let run = events(&log, "run.start")[0]["trace_id"].clone();
    assert_ne!(run, process, "{log}");
    let spawned = events(&log, "subagent.spawn");
    assert_eq!(spawned.len(), 1, "{log}");
    assert_eq!(spawned[0]["trace_id"], run, "{log}");
    let killed = events(&log, "subagent.kill");
    assert_eq!(killed.len(), 1, "{log}");
    assert_eq!(killed[0]["trace_id"], run, "{}", killed[0]);
    for escalated in events(&log, "subagent.kill.escalated") {
        assert_eq!(escalated["trace_id"], run, "{escalated}");
    }
}

/// A run another run starts — as a child workflow, or by a signal it sends —
/// is a span of the starter's trace, whose parent is the span of the step
/// that started it; and the starter's own trace is not the process's.
#[test]
fn a_run_another_run_starts_continues_its_trace() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let cfg = write_config(&format!(
        "agent:\n  name: chain\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nworkflows:\n  - name: parent\n    steps: {}\n  - name: child\n    steps: {}\n  - name: signalled\n    steps: {}\nlifecycle:\n  run_until: idle\n  idle_grace: 800ms\nobservability:\n  log_level: info\n",
        mock.uri(),
        json!({
            "start": {"kind": "loop", "max_iterations": 1},
            "call": {"kind": "workflow", "depends_on": ["start"], "name": "child", "mode": "sync"},
            "tell": {"kind": "workflow.signal", "depends_on": ["call"], "name": "go"},
            "done": {"kind": "finish", "depends_on": ["tell"], "status": "completed"}
        }),
        json!({
            "start": {"kind": "manual"},
            "probe": {"kind": "mcp.tool", "depends_on": ["start"], "server": "mock",
                      "tool": "state.list", "args": {"key": "child"}},
            "done": {"kind": "finish", "depends_on": ["probe"], "status": "completed"}
        }),
        json!({
            "start": {"kind": "signal", "name": "go"},
            "done": {"kind": "finish", "depends_on": ["start"], "status": "completed"}
        })
    ));
    let (code, log) = run_config(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let process = events(&log, "proc.start")[0]["trace_id"].clone();
    let started = |workflow: &str| -> Value {
        events(&log, "run.start")
            .into_iter()
            .find(|l| l["workflow"] == workflow)
            .unwrap_or_else(|| panic!("no {workflow} run:\n{log}"))
    };
    let parent = started("parent");
    assert_ne!(
        parent["trace_id"], process,
        "a loop firing is a trace of its own"
    );
    for workflow in ["child", "signalled"] {
        let run = started(workflow);
        assert_eq!(run["trace_id"], parent["trace_id"], "{workflow}: {run}");
        assert_ne!(run["span_id"], parent["span_id"], "{workflow}: {run}");
    }
    // The child workflow was asked for by the parent run's `call` step: its
    // span's parent is the step's, a child of the parent run's.
    let call_step = events(&log, "step.start")
        .into_iter()
        .find(|l| l["run"] == parent["run"] && l["step"] == "call")
        .unwrap_or_else(|| panic!("no call step:\n{log}"));
    assert_eq!(
        call_step["parent_span_id"], parent["span_id"],
        "{call_step}"
    );
    assert_eq!(
        started("child")["parent_span_id"],
        call_step["span_id"],
        "{log}"
    );
    // The child run's call is the parent's trace too.
    let child = started("child");
    let call = mock
        .metas()
        .into_iter()
        .find(|m| m["method"] == "tools/call" && m["_meta"]["agent/run"] == child["run"])
        .unwrap_or_else(|| panic!("no call from the child run:\n{}", mock.log()));
    assert_eq!(
        trace_of(&call["_meta"]["traceparent"]),
        parent["trace_id"].as_str().unwrap()
    );
    let _ = std::fs::remove_file(&cfg);
}

/// An operator's `observability.traceparent` is the process's caller: the
/// work the process was started to do continues its trace, and its sampling
/// decision — here `00`, not sampled — holds on every hop below, every MCP
/// request and every model call included.
#[test]
fn an_unsampled_upstream_trace_stays_unsampled_on_every_hop() {
    let upstream_trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let upstream = format!("00-{upstream_trace}-00f067aa0ba902b7-00");
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "thought"}]}));
    let cfg = write_config(&format!(
        "agent:\n  name: unsampled\nstore:\n  kind: memory\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nworkflows:\n  - name: job\n    steps: {}\nlifecycle:\n  run_until: idle\n  idle_grace: 500ms\nobservability:\n  log_level: info\n  traceparent: \"{upstream}\"\n",
        llm.uri,
        mock.uri(),
        json!({
            "start": {"kind": "once"},
            "probe": {"kind": "mcp.tool", "depends_on": ["start"], "server": "mock",
                      "tool": "state.list", "args": {"key": "unsampled"}},
            "read": {"kind": "mcp.resource", "depends_on": ["probe"], "server": "mock",
                     "op": "read", "uri": "mock://noop"},
            "think": {"kind": "agent", "depends_on": ["read"], "instruction": "think it over"},
            "done": {"kind": "finish", "depends_on": ["think"], "status": "completed"}
        })
    ));
    let (code, log) = run_config(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let proc = &events(&log, "proc.start")[0];
    assert_eq!(proc["trace_id"], upstream_trace, "{proc}");
    assert_eq!(proc["parent_span_id"], "00f067aa0ba902b7", "{proc}");
    let run = &events(&log, "run.start")[0];
    assert_eq!(
        run["trace_id"], upstream_trace,
        "the job is the caller's: {run}"
    );
    let metas = mock.metas();
    assert!(
        metas.iter().any(|m| m["method"] == "resources/read"),
        "{metas:?}"
    );
    for m in &metas {
        let tp = m["_meta"]["traceparent"].as_str().unwrap_or_default();
        assert_eq!(trace_of(&m["_meta"]["traceparent"]), upstream_trace, "{m}");
        assert!(tp.ends_with("-00"), "an unsampled trace was sampled: {m}");
    }
    let model_calls = llm.traces();
    assert!(!model_calls.is_empty(), "no model call reached the mock");
    for t in &model_calls {
        let tp = t["traceparent"].as_str().unwrap_or_default();
        assert_eq!(trace_of(&t["traceparent"]), upstream_trace, "{t}");
        assert!(tp.ends_with("-00"), "an unsampled trace was sampled: {t}");
    }
    let _ = std::fs::remove_file(&cfg);
}

/// What a process was started to do — a `once` start's run — is a span of
/// the process trace in a job, so the job is one trace; in a daemon it is a
/// trace of its own, as any unit is, since the process trace goes on
/// carrying the daemon's background requests for as long as it runs.
#[test]
fn a_boot_run_is_the_jobs_trace_and_a_daemons_own() {
    let boot = json!({
        "start": {"kind": "once"},
        "done": {"kind": "finish", "depends_on": ["start"], "status": "completed"}
    });
    let traces = |extra: &str| -> (Value, Value) {
        let cfg = write_config(&format!(
            "agent:\n  name: boot\nstore:\n  kind: memory\nworkflows:\n  - name: boot\n    steps: {boot}\n{extra}lifecycle:\n  run_until: idle\n  idle_grace: 500ms\nobservability:\n  log_level: info\n"
        ));
        let (code, log) = run_config(&cfg);
        let _ = std::fs::remove_file(&cfg);
        assert_eq!(code, Some(0), "{log}");
        let process = events(&log, "proc.start")[0]["trace_id"].clone();
        let run = events(&log, "run.start")
            .into_iter()
            .find(|l| l["workflow"] == "boot")
            .unwrap_or_else(|| panic!("no boot run:\n{log}"));
        (process, run["trace_id"].clone())
    };
    let (process, run) = traces("");
    assert_eq!(run, process, "a job is one trace");
    // A loop makes the process a daemon.
    let ticker = json!({
        "start": {"kind": "loop", "max_iterations": 1},
        "done": {"kind": "finish", "depends_on": ["start"], "status": "completed"}
    });
    let (process, run) = traces(&format!("  - name: ticker\n    steps: {ticker}\n"));
    assert_ne!(run, process, "a daemon's boot run is a trace of its own");
}

/// An instance child is a daemon of its own, started with a scrubbed
/// environment: what carries the spawning unit's trace across is its composed
/// `observability.traceparent`, so the child's own lines are in that trace.
#[cfg(feature = "a2a")]
#[test]
fn an_instance_child_continues_the_spawning_units_trace() {
    let state = common::unique_path("trace-instance", "state");
    let _ = std::fs::remove_dir_all(&state);
    let cfg = write_config(
        "agent: { name: parent }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 1500ms }\n\
         observability: { log_level: info }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   room:\n\
        \x20     instruction: |\n\
        \x20       You are the room for {{params.id}}.\n\
        \x20       :::!workflow{name=on-ping}\n\
        \x20       steps:\n\
        \x20         cmd: { kind: a2a, command: room.ping, roles: [agent, operator] }\n\
        \x20         f:   { kind: finish, depends_on: [cmd], status: completed }\n\
        \x20       :::\n\
        \x20     params: { id: { type: string, required: true } }\n\
        \x20     ttl: 2s\n\
         workflows:\n  - name: caller\n    steps:\n\
        \x20     s:     { kind: loop, max_iterations: 1 }\n\
        \x20     spawn: { kind: subagent, template: room, params: { id: inc-7 }, depends_on: [s] }\n\
        \x20     nap:   { kind: sleep, depends_on: [spawn], duration: 3s }\n\
        \x20     f:     { kind: finish, depends_on: [nap], status: completed }\n",
    );
    let err_path = common::unique_path("trace-instance", "err");
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_STATE_DIR", &state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .output()
        .expect("run");
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    assert_eq!(out.status.code(), Some(0), "{log}");
    let caller = events(&log, "run.start")
        .into_iter()
        .find(|l| l["workflow"] == "caller")
        .unwrap_or_else(|| panic!("no caller run:\n{log}"));
    let spawned = events(&log, "instance.spawn");
    assert_eq!(spawned.len(), 1, "{log}");
    assert_eq!(spawned[0]["trace_id"], caller["trace_id"], "{log}");
    // The child's own log, under the parent's state root.
    let child_logs: Vec<String> = std::fs::read_dir(format!("{state}/subagents"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("log"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    assert_eq!(child_logs.len(), 1, "one instance child, one log");
    let child = &child_logs[0];
    let proc = events(child, "proc.start");
    assert_eq!(proc.len(), 1, "{child}");
    assert_eq!(
        proc[0]["trace_id"], caller["trace_id"],
        "the child daemon is the spawning run's trace:\n{child}"
    );
    assert_eq!(proc[0]["instance"], "parent/room", "{child}");
    assert_ne!(proc[0]["run_id"], caller["run_id"], "a daemon of its own");
    // Its `ttl` retires it: what the parent says about that, and about its
    // exit, is in the spawning run's trace too.
    for event in ["instance.retire", "instance.exited"] {
        let said = events(&log, event);
        assert_eq!(said.len(), 1, "{event}:\n{log}");
        assert_eq!(said[0]["trace_id"], caller["trace_id"], "{}", said[0]);
    }
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_file(&cfg);
}
