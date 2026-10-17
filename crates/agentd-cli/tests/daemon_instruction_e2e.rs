// SPDX-License-Identifier: AGPL-3.0-only
//! **A daemon's instruction is its policy, not a task it runs at startup.**
//!
//! In a job the instruction is the task: with no workflow and no prompt,
//! agentd makes it the workflow `once → agent → finish`, runs it and exits
//! with that run's code. A daemon — an A2A listener, or `run_until: drained` —
//! takes work as it arrives, and its instruction is the policy that work is
//! done under. Run as a task at startup it was a model turn nobody asked
//! for, on every start: tokens, tool calls, a run left in status. A daemon
//! with work to do at boot says so, with `agent.prompt`; one that can be
//! given no work at all is refused at load; and a reload cannot turn one
//! into the other.
#![cfg(unix)]

mod common;

use std::process::{Child, Command, Stdio};
#[cfg(feature = "a2a")]
use std::time::{Duration, Instant};

#[cfg(feature = "a2a")]
use common::SendMessage;
use serde_json::{Value, json};

/// In the instruction, so a model request that carries it can be told apart.
const POLICY: &str = "You are the triage desk; policy mark 7f3a.";

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The mock model over HTTP with a `file:` playbook. Its stderr holds one
/// `MOCK_LLM_TRACE` line per request it answered.
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
    /// How many model requests reached the mock.
    fn requests(&self) -> usize {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("MOCK_LLM_TRACE "))
            .count()
    }
}

/// Every turn that carries the instruction is answered `under policy`, and a
/// turn on the prompt `prompt answered`; anything else `no policy`.
fn spawn_mock_llm() -> MockLlm {
    let playbook = json!({
        "turns": [{"content": "no policy"}],
        "match": [
            {"when_contains": "the oldest open ticket", "content": "prompt answered"},
            {"when_contains": "policy mark 7f3a", "content": "under policy"}
        ]
    });
    let pb = common::unique_path("daemon-instr-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("daemon-instr-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let log_path = common::unique_path("daemon-instr-llm", "log");
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

/// A daemon killed when the test lets go of it.
#[cfg(feature = "a2a")]
struct Daemon {
    child: Child,
    log: String,
    cfg: String,
}
#[cfg(feature = "a2a")]
impl Daemon {
    fn spawn(cfg: &str) -> Daemon {
        let log = common::unique_path("daemon-instr", "log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", cfg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("spawn agentd");
        Daemon {
            child,
            log,
            cfg: cfg.to_string(),
        }
    }
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn wait_log(&self, what: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let log = self.log();
            if ok(&log) {
                return log;
            }
            assert!(Instant::now() < deadline, "{what}; daemon log:\n{log}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    /// SIGTERM, then the exit code of the drain it begins.
    fn drain(&mut self) -> Option<i32> {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Some(st)) = self.child.try_wait() {
                return st.code();
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not drain; log:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
#[cfg(feature = "a2a")]
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
        let _ = std::fs::remove_file(&self.cfg);
    }
}

fn config(llm: &str, extra: &str) -> String {
    format!(
        "\
         agent:\n  name: desk\n  instruction: \"{POLICY}\"\n  preflight: never\n{extra}\
         intelligence:\n  endpoints: {llm}\n  model: m-1\n\
         store:\n  kind: memory\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

/// What `--effective-config` reports a config to be.
#[cfg(feature = "a2a")]
fn effective(cfg: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--effective-config", "--config", cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run --effective-config");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("the JSON report")
}

/// A listener makes a daemon under the default `run_until: auto`. Its
/// instruction generates no workflow, nothing runs when it starts, and no
/// model is called until a message arrives — which is then answered under
/// the instruction, as the policy it is.
#[cfg(feature = "a2a")]
#[test]
fn a_listening_daemon_runs_nothing_until_a_message_arrives() {
    let llm = spawn_mock_llm();
    let cfg = common::unique_path("daemon-instr", "yaml");
    let probe = |port: u16| {
        config(
            &llm.uri,
            &format!("a2a:\n  listen: http://127.0.0.1:{port}\n"),
        )
    };

    std::fs::write(&cfg, probe(common::free_port())).unwrap();
    let report = effective(&cfg);
    assert!(
        report["config"].get("workflows").is_none()
            && report["provenance"].get("workflows").is_none(),
        "a daemon's instruction generates no workflow: {report}"
    );

    let (mut daemon, addr) = common::spawn_bound(|port| {
        std::fs::write(&cfg, probe(port)).unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.log.clone();
        (d, log)
    });

    // A message is answered, under the instruction.
    let result = SendMessage::text("Anything new?").result(&addr);
    let task = &result["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
    assert!(
        task.to_string().contains("under policy"),
        "the turn was not given the instruction: {task}"
    );
    assert_eq!(
        llm.requests(),
        1,
        "one model call, the message's: nothing else ran"
    );
    // …and no run is left in status.
    let status = SendMessage::command("status", json!({})).result(&addr);
    assert_eq!(
        status["message"]["parts"][0]["data"]["runs"],
        json!([]),
        "{status}"
    );

    assert_eq!(daemon.drain(), Some(0), "{}", daemon.log());
    let log = daemon.log();
    assert!(
        events(&log, "run.start").is_empty(),
        "the instruction ran as a task:\n{log}"
    );
    assert!(events(&log, "workflow.loaded").is_empty(), "{log}");
}

/// A daemon that has work for its first moments says so: `agent.prompt` is
/// delivered into its conversation when it starts and answered there, under
/// the instruction, with no workflow generated for either.
#[cfg(feature = "a2a")]
#[test]
fn a_daemons_prompt_is_answered_at_startup() {
    let llm = spawn_mock_llm();
    let cfg = common::unique_path("daemon-instr", "yaml");
    let (mut daemon, _) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            config(
                &llm.uri,
                &format!(
                    "  prompt: Start with the oldest open ticket.\n\
                     a2a:\n  listen: http://127.0.0.1:{port}\n"
                ),
            ),
        )
        .unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.log.clone();
        (d, log)
    });
    let log = daemon.wait_log("the prompt was never answered", |l| {
        events(l, "turn.reply")
            .iter()
            .any(|e| e["ctx"] == "root" && e["text"] == "prompt answered")
    });
    assert!(events(&log, "run.start").is_empty(), "{log}");
    assert_eq!(llm.requests(), 1, "the prompt's turn, and nothing else");
    assert_eq!(daemon.drain(), Some(0), "{}", daemon.log());
}

/// A job is unchanged: its instruction is the task, run once as the
/// generated workflow, its output on stdout and its outcome the exit code.
#[test]
fn a_jobs_instruction_still_runs_once_and_exits_with_its_run() {
    let llm = spawn_mock_llm();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--instruction",
            POLICY,
            "--intelligence",
            &llm.uri,
            "--model",
            "m-1",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run the job");
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "under policy");
    let runs = events(&log, "run.start");
    assert!(
        runs.len() == 1 && runs[0]["workflow"] == "main",
        "the one-shot workflow ran once:\n{log}"
    );

    // Its run's outcome is the exit code: a model nobody answers is 4.
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--instruction",
            POLICY,
            "--intelligence",
            "http://127.0.0.1:9",
            "--model",
            "m-1",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run the job");
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Told to stay up with nothing that can bring it work — no A2A listener, no
/// workflow, no prompt — an instance would load and wait for ever. A webhook
/// listener or a goal does not change that: each only starts workflows, and
/// there are none. It is refused at load, naming the ways to give it work.
#[test]
fn a_daemon_nothing_can_reach_is_refused_at_load() {
    let llm = spawn_mock_llm();
    let cfg = common::unique_path("daemon-instr", "yaml");
    let mut alone = vec!["", "goal:\n  statement: the queue is empty\n"];
    if cfg!(feature = "a2a") {
        alone.push("webhooks:\n  listen: http://127.0.0.1:1\n");
    }
    for extra in alone {
        std::fs::write(
            &cfg,
            config(
                &llm.uri,
                &format!("{extra}lifecycle:\n  run_until: drained\n"),
            ),
        )
        .unwrap();
        for ask in [&["--validate-config"][..], &[]] {
            let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
                .args(ask)
                .args(["--config", &cfg])
                .stdin(Stdio::null())
                .output()
                .expect("run agentd");
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(2), "{extra:?} {ask:?}: {err}");
            assert!(
                err.contains("nothing can bring this instance work")
                    && err.contains("agent.prompt"),
                "{extra:?} {ask:?}: {err}"
            );
        }
    }
    assert_eq!(llm.requests(), 0);
    let _ = std::fs::remove_file(&cfg);
}

/// The job shape is settled when the process starts, and so is what its
/// instruction is. An instruction document whose own `:::!config` changes
/// `lifecycle.run_until` is refused on reload as the operator's change would
/// be, rather than reloading a daemon into a job: that would generate the
/// one-shot workflow and run the instruction, unprompted, in a live daemon.
#[cfg(all(feature = "a2a", feature = "hot-reload"))]
#[test]
fn an_instruction_cannot_reload_a_daemon_into_a_job() {
    let llm = spawn_mock_llm();
    let md = common::unique_path("daemon-instr", "md");
    std::fs::write(&md, format!("{POLICY}\n")).unwrap();
    let cfg = common::unique_path("daemon-instr", "yaml");
    let (mut daemon, _) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            format!(
                "agent:\n  name: desk\n  instruction: {{file: {md}}}\n  preflight: never\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n\
                 intelligence:\n  endpoints: {}\n  model: m-1\n\
                 store:\n  kind: memory\n\
                 observability:\n  log_level: info\n",
                llm.uri
            ),
        )
        .unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.log.clone();
        (d, log)
    });
    daemon.wait_log("the daemon never came up", |l| {
        !events(l, "proc.ready").is_empty()
    });
    std::fs::write(
        &md,
        format!("{POLICY}\n\n:::!config\nlifecycle: {{ run_until: idle }}\n:::\n"),
    )
    .unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    let log = daemon.wait_log("the reload was not refused", |l| {
        !events(l, "config.reload.restart_required").is_empty()
    });
    let refused = &events(&log, "config.reload.restart_required")[0];
    assert_eq!(refused["paths"], json!(["lifecycle.run_until"]), "{log}");
    // Long enough for a generated run to have started and dialed the model.
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(llm.requests(), 0, "a model call nobody asked for");
    assert!(
        daemon.child.try_wait().unwrap().is_none(),
        "the daemon is still up:\n{}",
        daemon.log()
    );
    assert_eq!(daemon.drain(), Some(0), "{}", daemon.log());
    let log = daemon.log();
    assert!(
        events(&log, "run.start").is_empty(),
        "the instruction ran as a task:\n{log}"
    );
    let _ = std::fs::remove_file(&md);
}
