// SPDX-License-Identifier: Apache-2.0
//! A turn worker that dies **without a terminal frame** must still fail its
//! unit (RFC 0026 §3.2). The supervisor learns of the death only through the
//! reaper, and by then `Children::on_reaped` has already removed the child from
//! the table — so "did this worker report a `TurnDone`?" cannot be answered by
//! asking whether the child is still there: that is false for a settled worker
//! and an orphaned one alike. A `settled` marker on the child record, left by
//! `on_turn_done` / `on_turn_failed` and carried past the removal, answers it.
//!
//! The regression this exists for: a SIGKILLed **root-turn** worker fell
//! through that broken guard entirely. Its budget reservation was never
//! released — the tokens stayed reserved for the life of the window, so the
//! next turn could not be admitted — and its inbox event stayed pending until a
//! restart replayed it. The `StepTurn` half of the same defect is decided from
//! the step's own state and was fixed first; this covers the root turn.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long any single wait below may take before the test calls the daemon
/// wedged. Generous next to the ~1 s each phase needs, because the point is to
/// distinguish "slow" from "never".
const WAIT: Duration = Duration::from_secs(30);

/// The first message's size, in characters. It dominates BOTH reservation
/// estimates (`chars / 4` + the 4096-token completion allowance), which is
/// what makes the budget window below a reliable probe: one turn fits, two
/// concurrent reservations do not, and the exact prompt overhead cannot drift
/// far enough to change either answer.
const BIG_MESSAGE_CHARS: usize = 40_000;

/// A window that admits one turn but not two at once: ~14k tokens are reserved
/// per turn (10k of message + the 4096 allowance), so a reservation the
/// orphaned worker LEAKED leaves no room for the second turn. `on_exhausted`
/// stays at its default (`wait`), so a leak parks the second turn instead of
/// dropping it — the daemon then never spawns it, which is the assertion.
const WINDOW_TOKENS: u64 = 20_000;

struct MockLlm {
    child: std::process::Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}

/// The mock LLM, scripted to HANG on any request it is given: the turn worker
/// must be alive and unsettled when the test kills it.
fn spawn_hanging_llm() -> MockLlm {
    let pb = common::unique_path("playbook", "json");
    std::fs::write(
        &pb,
        serde_json::json!({"turns": [{"content": "never delivered", "delay_ms": 600_000}]})
            .to_string(),
    )
    .unwrap();
    let addr_file = common::unique_path("mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

fn write_file(tag: &str, ext: &str, body: &str) -> String {
    let path = common::unique_path(tag, ext);
    std::fs::File::create(&path)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    path
}

/// The telemetry lines named `name`, in emission order.
fn events(stderr: &str, name: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The last lines of a log, for a failure message: a leaked reservation makes
/// the daemon re-log `budget.wait` every tick, and dumping thousands of
/// identical lines buries the assertion that failed.
fn tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

/// Block until the daemon's stderr satisfies `want`, or fail with the log.
fn wait_for(err_path: &str, what: &str, want: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let log = std::fs::read_to_string(err_path).unwrap_or_default();
        if want(&log) {
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "waited {WAIT:?} for {what}; daemon stderr (tail):\n{}",
            tail(&log)
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The daemon's live children, from `pgrep -P` — the turn worker is a DIRECT
/// child of the supervisor (the flat child tree, RFC 0026 §2).
fn children_of(pid: i32) -> Vec<i32> {
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .collect()
}

#[test]
fn a_sigkilled_root_turn_worker_fails_its_turn_and_releases_its_reservation() {
    let llm = spawn_hanging_llm();
    // No `instruction` sugar workflow: a `manual` workflow keeps the daemon up
    // as a conversation host, the way the other v2 conversation suites do.
    let cfg = write_file(
        "agentd-orphan",
        "yaml",
        &format!(
            "config_version: \"2\"\nagent:\n  name: orphan\n  instruction: You answer questions.\n  preflight: never\n\
intelligence:\n  endpoints: {}\n  model: mock\n  budget:\n    windows: [{{ per: hour, tokens: {WINDOW_TOKENS} }}]\n\
store:\n  kind: memory\n\
workflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\n\
lifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n",
            llm.uri
        ),
    );
    // Two messages in ONE conversation, so the second is serialized behind the
    // first and is dispatched only once the first turn has settled.
    let big = "the deployment policy paragraph. ".repeat(BIG_MESSAGE_CHARS / 33);
    let inbox = write_file(
        "inbox-orphan",
        "json",
        &serde_json::json!([
            {"kind": "a2a_message", "principal": "user:alice", "payload": {"context_id": "c1", "text": big}},
            {"kind": "a2a_message", "principal": "user:alice", "payload": {"context_id": "c1", "text": "and now the short follow-up"}}
        ])
        .to_string(),
    );
    let err_path = common::unique_path("agentd-orphan", "err");
    let err = std::fs::File::create(&err_path).expect("create stderr file");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_TEST_INBOX_FILE", &inbox)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn agentd");
    let daemon_pid = daemon.id() as i32;

    // 1. The first turn's worker is up.
    wait_for(&err_path, "the first turn to spawn", |log| {
        !events(log, "turn.spawn").is_empty()
    });
    let deadline = Instant::now() + WAIT;
    let worker = loop {
        match children_of(daemon_pid).first() {
            Some(pid) => break *pid,
            None if Instant::now() >= deadline => {
                let _ = daemon.kill();
                panic!("the turn worker never appeared as a child of {daemon_pid}");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };

    // 2. Kill it where it stands — no `TurnDone`, no `Failed`, just a corpse
    //    for the reaper. This is the orphan the guard has to catch.
    assert_eq!(
        unsafe { libc::kill(worker, libc::SIGKILL) },
        0,
        "SIGKILL the turn worker {worker}"
    );

    // 3. The turn must FAIL (it hung forever before the fix), and the second
    //    turn must then be admitted — which it can only be if the dead
    //    worker's reservation went back to the window.
    let log = wait_for(&err_path, "the orphaned turn to fail", |log| {
        events(log, "turn.failed")
            .iter()
            .any(|e| e["kind"] == "turn:c1")
    });
    let failed = events(&log, "turn.failed");
    assert!(
        failed[0]["err"]
            .as_str()
            .unwrap_or_default()
            .contains("worker exited without a result"),
        "the failure names the orphan: {failed:?}"
    );
    let log = wait_for(&err_path, "the second turn to be dispatched", |log| {
        events(log, "turn.spawn").len() >= 2
    });
    let waits = events(&log, "budget.wait")
        .into_iter()
        .filter(|e| e["ctx"] == "c1")
        .count();
    assert_eq!(
        waits, 0,
        "the second turn was admitted without ever waiting on the budget, so nothing stayed reserved"
    );

    // 4. The daemon is still a healthy single writer: it drains on SIGTERM.
    //    (The second worker is parked in the model call the mock never answers,
    //    so the kill ladder is what ends it.)
    unsafe { libc::kill(daemon_pid, libc::SIGTERM) };
    let deadline = Instant::now() + WAIT;
    loop {
        match daemon.try_wait().expect("wait for agentd") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = daemon.kill();
                let _ = daemon.wait();
                let log = std::fs::read_to_string(&err_path).unwrap_or_default();
                panic!(
                    "the daemon never drained after SIGTERM; stderr (tail):\n{}",
                    tail(&log)
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = std::fs::remove_file(&err_path);
}
