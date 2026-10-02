// SPDX-License-Identifier: AGPL-3.0-only
//! **A context holding `sensitive` and `egress` never receives a tainted
//! result** (RFC 0045 §5.11.3), end to end.
//!
//! `hold` is a workflow an A2A peer can start (`peer`), so every run of it
//! carries outside input, and the child it spawns carries what the run
//! handed it. The root conversation holds `vault`, tagged with both other
//! legs. Every way the root reads that result back — `workflow.run` with
//! `wait`, `workflow.status`, a sync workflow tool, `subagent.await`, a plan
//! item bound to the child — answers with the status and a marker in place
//! of the text, and logs `readback.withheld`. The same read by a root that
//! holds a leg less, or of a run nothing outside reaches, is handed whole.
//! The notes a finished run and a child's result leave in the root
//! transcript are opt-in, and withheld when they are opted into.
#![cfg(unix)]

#[cfg(feature = "a2a")]
mod common;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What the run handed back and what the child answered: neither may reach
/// a root holding both legs.
const RUN_SECRET: &str = "RUN-SECRET";
const CHILD_SECRET: &str = "CHILD-SECRET";

/// `hold`, started from `m` (the default start, so its sync tool `hold.now`
/// runs from it too): it spawns a child that answers [`CHILD_SECRET`] after
/// a moment, and finishes with [`RUN_SECRET`]. With `peer`, an A2A peer can
/// start it, so its runs carry outside input.
fn hold(peer: bool) -> String {
    format!(
        "  - name: hold\n    tool: {{name: hold.now, mode: sync}}\n    steps:\n{}      \
         m: {{kind: manual}}\n      \
         c: {{kind: subagent, depends_on: [m], mode: async, instruction: \"CHILD-INSTRUCTION\", servers: []}}\n      \
         f: {{kind: finish, depends_on: [c], status: completed, output: \"{RUN_SECRET}\"}}\n",
        if peer {
            "      peer: {kind: a2a, command: poke}\n"
        } else {
            ""
        }
    )
}

/// One daemon's run, read after it stopped.
struct Ran {
    log: String,
    state: PathBuf,
}

impl Ran {
    fn events(&self, name: &str) -> Vec<Value> {
        self.log
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == name)
            .collect()
    }
    /// What the root's turn answered: an `echo_tool_result` turn answers
    /// with the last tool result it was handed.
    fn answer(&self) -> String {
        self.events("turn.reply")
            .last()
            .and_then(|e| e["text"].as_str().map(str::to_string))
            .unwrap_or_default()
    }
    /// The `via` of every `readback.withheld` line.
    fn withheld(&self) -> Vec<String> {
        self.events("readback.withheld")
            .iter()
            .map(|e| e["via"].as_str().unwrap_or_default().to_string())
            .collect()
    }
    /// The notes in the root transcript, as the store kept it.
    fn root_notes(&self) -> Vec<String> {
        let path = self.state.join("agentd/hold/context/root.json");
        let ctx: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default())
            .unwrap_or_else(|_| panic!("no root context at {}:\n{}", path.display(), self.log));
        ctx["state"]["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == "note")
            .map(|m| m["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }
    /// The first child `hold` spawned, which the plays address by handle.
    fn assert_child(&self, handle: &str) {
        let spawned = self.events("subagent.spawn");
        assert!(
            spawned.first().is_some_and(|e| e["handle"] == handle),
            "the plays address the first child as {handle}:\n{}",
            self.log
        );
    }
}

/// Run a daemon whose root holds `vault` tagged `vault_tags`, beside
/// `workflows`, through `turns` (a mock playbook), until its turn answers —
/// and `settle` more, for what lands after it — then stop it, which
/// checkpoints the transcript.
fn run(
    dir: &Path,
    vault_tags: &str,
    agent: &str,
    workflows: &str,
    turns: Value,
    settle: Duration,
) -> Ran {
    let play = dir.join("play.json");
    std::fs::write(
        &play,
        json!({
            "match": [{"when_contains": "CHILD-INSTRUCTION", "content": CHILD_SECRET,
                       "delay_ms": 300}],
            "turns": turns
        })
        .to_string(),
    )
    .unwrap();
    let state = dir.join("state");
    let _ = std::fs::remove_dir_all(&state);
    let cfg = dir.join("agent.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: hold\n  prompt: go\n  instruction: Look after it.\n{agent}\
             intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
             store: {{kind: file, file: {{path: {}}}}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: info\n  log_content: true\n\
             mcp:\n  servers:\n\
             \x20   - {{name: vault, endpoint: \"https://vault.invalid/mcp\", tags: {{\"*\": [{vault_tags}]}}}}\n\
             workflows:\n{workflows}",
            play.display(),
            state.display()
        ),
    )
    .unwrap();
    let log = dir.join("daemon.log");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    for (k, _) in std::env::vars() {
        if k.starts_with("AGENTD_") || k.starts_with("AGENT_") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd
        .args(["--config", &cfg.to_string_lossy()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("spawn agentd");
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !read().contains("\"event\":\"turn.reply\"") {
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the root's turn did not answer:\n{}", read());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    std::thread::sleep(settle);
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.wait();
    Ran { log: read(), state }
}

fn call(name: &str, args: Value) -> Value {
    json!({"tool_calls": [{"name": name, "arguments": args}]})
}

fn echo() -> Value {
    json!({"echo_tool_result": true})
}

/// Start `hold` and wait for it: the run's output, and its child spawned.
fn run_hold() -> Value {
    call("workflow.run", json!({"name": "hold", "wait": true}))
}

/// The reads, each with the `via` its withholding logs and what its reply
/// would otherwise carry.
fn reads() -> Vec<(&'static str, Vec<Value>, &'static str)> {
    vec![
        ("workflow.run", vec![run_hold(), echo()], RUN_SECRET),
        (
            "workflow.status",
            vec![
                run_hold(),
                call("workflow.status", json!({"name": "hold"})),
                echo(),
            ],
            RUN_SECRET,
        ),
        // A sync workflow tool is `workflow.run` with `wait`, and answered as
        // one.
        (
            "workflow.run",
            vec![call("hold.now", json!({})), echo()],
            RUN_SECRET,
        ),
        (
            "subagent.await",
            vec![
                run_hold(),
                call("subagent.await", json!({"handle": "sub-2"})),
                echo(),
            ],
            CHILD_SECRET,
        ),
        // The plan note a bound child settles, read back with `plan.get`.
        (
            "plan.update",
            vec![
                call(
                    "plan.create",
                    json!({"goal": "watch", "items": ["the child"]}),
                ),
                run_hold(),
                call(
                    "plan.update",
                    json!({"item": 1, "bind": {"subagent": "sub-2"}}),
                ),
                call("subagent.await", json!({"handle": "sub-2"})),
                call("plan.get", json!({})),
                echo(),
            ],
            CHILD_SECRET,
        ),
    ]
}

/// A root holding both legs reads a tainted run or child back through each
/// path: the status comes back, the text does not, and `readback.withheld`
/// names the path.
#[test]
fn a_root_holding_both_legs_is_withheld_the_tainted_text_on_every_path() {
    let t = tempfile::tempdir().unwrap();
    for (via, turns, secret) in reads() {
        let ran = run(
            t.path(),
            "sensitive, egress",
            "",
            &hold(true),
            json!(turns),
            Duration::ZERO,
        );
        ran.assert_child("sub-2");
        let answer = ran.answer();
        assert!(
            answer.contains("output withheld: this ")
                && answer.contains("carries outside input")
                && answer.contains("this conversation holds sensitive and egress tools"),
            "{via}: {answer}\n{}",
            ran.log
        );
        assert!(!answer.contains(secret), "{via}: {answer}");
        if via != "plan.update" {
            assert!(
                answer.contains("\"status\":\"completed\""),
                "{via}: {answer}"
            );
        }
        assert!(
            ran.withheld().iter().any(|v| v == via),
            "{via}: {:?}\n{}",
            ran.withheld(),
            ran.log
        );
    }
}

/// The same reads by a root that holds a leg less are handed the text whole,
/// and nothing is withheld.
#[test]
fn a_root_holding_a_leg_less_reads_the_text_whole() {
    let t = tempfile::tempdir().unwrap();
    for (via, turns, secret) in reads() {
        let ran = run(
            t.path(),
            "sensitive",
            "",
            &hold(true),
            json!(turns),
            Duration::ZERO,
        );
        assert!(
            ran.answer().contains(secret),
            "{via}: {}\n{}",
            ran.answer(),
            ran.log
        );
        assert!(ran.withheld().is_empty(), "{via}: {:?}", ran.withheld());
    }
}

/// A run nothing outside can reach carries nothing to withhold, from a root
/// holding both legs as from any.
#[test]
fn an_untainted_result_is_never_withheld() {
    let t = tempfile::tempdir().unwrap();
    for (via, turns, secret) in reads() {
        let ran = run(
            t.path(),
            "sensitive, egress",
            "",
            &hold(false),
            json!(turns),
            Duration::ZERO,
        );
        assert!(
            ran.answer().contains(secret),
            "{via}: {}\n{}",
            ran.answer(),
            ran.log
        );
        assert!(ran.withheld().is_empty(), "{via}: {:?}", ran.withheld());
    }
}

/// A workflow is a reader too. `caller` holds `vault` (its `a` step never
/// runs, but it is the step's grant that counts) and reads a run of `hold`
/// back through each step that can: what it gets, and so what it returns,
/// is the marker. One that holds a leg less is handed the run whole — and
/// then carries it: the root, holding both, has `caller`'s result withheld
/// in turn.
#[test]
fn a_workflow_holding_both_legs_is_withheld_from_and_one_a_leg_less_carries_it_on() {
    let t = tempfile::tempdir().unwrap();
    let caller = |read: &str, act: bool| {
        format!(
            "  - name: caller\n    steps:\n      s: {{kind: manual}}\n      \
             r0: {{kind: workflow, depends_on: [s], name: hold, mode: async}}\n      \
             r: {{depends_on: [r0], {read}}}\n      \
             f: {{kind: finish, depends_on: [r], status: completed, output: \"{{{{steps.r.output}}}}\"}}\n{}",
            if act {
                "      a: {kind: agent, depends_on: [f], instruction: \"Act.\", servers: [vault], tools: [\"vault.*\"]}\n"
            } else {
                ""
            }
        )
    };
    let handle = "\"{{steps.r0.output.run}}\"";
    for (read, via, secret) in [
        (
            format!("kind: join, handles: [{handle}]"),
            "join",
            RUN_SECRET,
        ),
        (
            format!("kind: wait, on: run, run: {handle}"),
            "wait on: run",
            RUN_SECRET,
        ),
        (
            format!("kind: workflow.wait, run: {handle}"),
            "wait on: run",
            RUN_SECRET,
        ),
        (
            "kind: workflow, name: hold".to_string(),
            "wait on: run",
            RUN_SECRET,
        ),
        // The child the run of `hold` spawned.
        (
            "kind: wait, on: subagent, subagent: sub-2".to_string(),
            "wait on: subagent",
            CHILD_SECRET,
        ),
    ] {
        let turns = json!([
            call("workflow.run", json!({"name": "caller", "wait": true})),
            echo()
        ]);
        let workflows = format!("{}{}", hold(true), caller(&read, true));
        let ran = run(
            t.path(),
            "sensitive, egress",
            "",
            &workflows,
            turns.clone(),
            Duration::ZERO,
        );
        let answer = ran.answer();
        assert!(
            answer.contains("workflow \\\"caller\\\" holds sensitive and egress tools")
                && !answer.contains(secret),
            "{via}: {answer}\n{}",
            ran.log
        );
        assert!(
            ran.events("readback.withheld")
                .iter()
                .any(|e| e["via"] == via
                    && e["reader"]
                        .as_str()
                        .is_some_and(|r| r.starts_with("run:caller-"))),
            "{via}: {}",
            ran.log
        );

        // A leg less: handed whole, so its own result carries the taint, and
        // the root reading it back is withheld from.
        let workflows = format!("{}{}", hold(true), caller(&read, false));
        let ran = run(
            t.path(),
            "sensitive, egress",
            "",
            &workflows,
            turns,
            Duration::ZERO,
        );
        assert!(
            !ran.events("readback.withheld").iter().any(|e| e["reader"]
                .as_str()
                .is_some_and(|r| r.starts_with("run:caller-"))),
            "{via}: {}",
            ran.log
        );
        let answer = ran.answer();
        assert!(
            answer.contains("reads back workflow \\\"hold\\\"") && !answer.contains(secret),
            "{via}: {answer}\n{}",
            ran.log
        );
    }
}

/// With the defaults no note about a finished or failed run, or a child's
/// result, reaches the root transcript: implicit notes are opt-in. Opted
/// into (`on_workflow_finished: note`, `wake_on: [subagent_result,
/// workflow_finished, workflow_failed]`), the notes arrive — withheld, since
/// the root holds both legs.
#[test]
fn implicit_notes_are_opt_in_and_withheld_when_opted_into() {
    let t = tempfile::tempdir().unwrap();
    let workflows = format!(
        "{}  - name: flop\n    steps:\n      peer: {{kind: a2a, command: poke}}\n      \
         m: {{kind: manual}}\n      \
         f: {{kind: finish, depends_on: [m], status: failed, output: \"{RUN_SECRET}\"}}\n",
        hold(true)
    );
    let turns = json!([
        run_hold(),
        call("workflow.run", json!({"name": "flop", "wait": true})),
        call("subagent.await", json!({"handle": "sub-2"})),
        echo()
    ]);
    let settle = Duration::from_millis(500);
    let ran = run(
        t.path(),
        "sensitive, egress",
        "",
        &workflows,
        turns.clone(),
        settle,
    );
    ran.assert_child("sub-2");
    let notes = ran.root_notes();
    assert!(
        !notes
            .iter()
            .any(|n| n.starts_with("workflow ") || n.starts_with("subagent ")),
        "{notes:?}"
    );

    let opted = "  on_workflow_finished: note\n  \
                 wake_on: [subagent_result, workflow_finished, workflow_failed]\n";
    let ran = run(
        t.path(),
        "sensitive, egress",
        opted,
        &workflows,
        turns,
        settle,
    );
    let notes = ran.root_notes();
    for about in [
        "workflow hold run ",
        "workflow flop run ",
        "subagent sub-2 completed: ",
    ] {
        assert!(
            notes.iter().any(|n| n.starts_with(about)
                && n.contains("output withheld: this ")
                && !n.contains(RUN_SECRET)
                && !n.contains(CHILD_SECRET)),
            "{about}: {notes:?}\n{}",
            ran.log
        );
    }
    for via in ["agent.on_workflow_finished", "agent.wake_on"] {
        assert!(
            ran.withheld().iter().any(|v| v == via),
            "{via}: {}",
            ran.log
        );
    }
}

/// A warm child's per-turn note reaches the root only under
/// `subagent_result`, as a finished child's does — where before it was
/// written whatever the wake policy said — and withheld there.
#[test]
fn a_warm_childs_turn_is_noted_only_when_opted_into() {
    let t = tempfile::tempdir().unwrap();
    let warm = "  - name: hold\n    steps:\n      peer: {kind: a2a, command: poke}\n      \
                m: {kind: manual}\n      \
                c: {kind: subagent, depends_on: [m], mode: warm, instruction: \"CHILD-INSTRUCTION\", servers: []}\n      \
                f: {kind: finish, depends_on: [c], status: completed}\n";
    let turns = json!([
        call("workflow.run", json!({"name": "hold", "wait": true})),
        call("sleep", json!({"duration": "1s"})),
        echo()
    ]);
    let settle = Duration::from_millis(300);
    let turned = |ran: &Ran| {
        assert!(
            !ran.events("subagent.turn").is_empty(),
            "the warm child never finished a turn:\n{}",
            ran.log
        );
        ran.root_notes()
            .into_iter()
            .filter(|n| n.contains("finished a turn"))
            .collect::<Vec<_>>()
    };
    let ran = run(
        t.path(),
        "sensitive, egress",
        "",
        warm,
        turns.clone(),
        settle,
    );
    assert_eq!(turned(&ran), Vec::<String>::new());

    let opted = "  wake_on: [subagent_result]\n";
    let ran = run(t.path(), "sensitive, egress", opted, warm, turns, settle);
    let notes = turned(&ran);
    assert!(
        !notes.is_empty()
            && notes
                .iter()
                .all(|n| n.contains("output withheld: this subagent") && !n.contains(CHILD_SECRET)),
        "{notes:?}\n{}",
        ran.log
    );
}

/// A result is withheld only for text its reader was not handed already. A
/// route's own run holds `vault`, and it is what handed its child the
/// route's text: reading the child back, it reads nothing new and gets it
/// whole. The root, handed none of it, does not.
#[test]
fn a_routes_own_run_reads_its_childs_result_whole() {
    let t = tempfile::tempdir().unwrap();
    let workflows = format!(
        "  - name: route\n    steps:\n      peer: {{kind: a2a, command: poke}}\n      \
         m: {{kind: manual}}\n      \
         c: {{kind: workflow, depends_on: [m], name: child}}\n      \
         f: {{kind: finish, depends_on: [c], status: completed, output: \"{{{{steps.c.output}}}}\"}}\n      \
         a: {{kind: agent, depends_on: [f], instruction: \"Act.\", servers: [vault], tools: [\"vault.*\"]}}\n  \
         - name: child\n    steps:\n      m: {{kind: manual}}\n      \
         f: {{kind: finish, depends_on: [m], status: completed, output: \"{RUN_SECRET}\"}}\n"
    );
    let turns = json!([
        call(
            "workflow.run",
            json!({"name": "route", "start": "m", "wait": true})
        ),
        echo()
    ]);
    let ran = run(
        t.path(),
        "sensitive, egress",
        "",
        &workflows,
        turns,
        Duration::ZERO,
    );
    let routes_own = |e: &Value| {
        e["reader"]
            .as_str()
            .is_some_and(|r| r.starts_with("run:route-"))
    };
    assert!(
        !ran.events("readback.withheld").iter().any(routes_own),
        "{}",
        ran.log
    );
    assert!(
        ran.events("run.done")
            .iter()
            .any(|e| e["workflow"] == "route" && e["output"].to_string().contains(RUN_SECRET)),
        "the route's run read its child whole:\n{}",
        ran.log
    );
    assert!(
        ran.withheld().iter().any(|v| v == "workflow.run") && !ran.answer().contains(RUN_SECRET),
        "{}",
        ran.answer()
    );
}

/// A `workflow` step whose child run is queued behind the workflow's
/// concurrency cap is resolved when the child lands, by another path than a
/// run that starts at once — withheld the same way.
#[test]
fn a_queued_child_runs_result_is_withheld_too() {
    let t = tempfile::tempdir().unwrap();
    let workflows = format!(
        "  - name: slow\n    concurrency: {{max_runs: 1, on_overflow: queue}}\n    steps:\n      \
         peer: {{kind: a2a, command: poke}}\n      \
         m: {{kind: manual}}\n      \
         s: {{kind: sleep, depends_on: [m], duration: 300ms}}\n      \
         f: {{kind: finish, depends_on: [s], status: completed, output: \"{RUN_SECRET}\"}}\n  \
         - name: caller\n    steps:\n      s: {{kind: manual}}\n      \
         r0: {{kind: workflow, depends_on: [s], name: slow, mode: detached}}\n      \
         r: {{kind: workflow, depends_on: [r0], name: slow}}\n      \
         f: {{kind: finish, depends_on: [r], status: completed, output: \"{{{{steps.r.output}}}}\"}}\n      \
         a: {{kind: agent, depends_on: [f], instruction: \"Act.\", servers: [vault], tools: [\"vault.*\"]}}\n"
    );
    let turns = json!([
        call("workflow.run", json!({"name": "caller", "wait": true})),
        echo()
    ]);
    let ran = run(
        t.path(),
        "sensitive, egress",
        "",
        &workflows,
        turns,
        Duration::ZERO,
    );
    assert!(
        ran.events("readback.withheld")
            .iter()
            .any(|e| e["via"] == "workflow step"
                && e["reader"]
                    .as_str()
                    .is_some_and(|r| r.starts_with("run:caller-"))),
        "{}",
        ran.log
    );
    let answer = ran.answer();
    assert!(
        answer.contains("workflow \\\"caller\\\" holds sensitive and egress tools")
            && !answer.contains(RUN_SECRET),
        "{answer}"
    );
}

/// A run an A2A peer — an `agent` principal — asked for directly carries the
/// peer's text, though its definition opens no route: the root holding both
/// legs reads it back withheld. The same run started by the operator is read
/// whole.
#[cfg(feature = "a2a")]
#[test]
fn a_run_a_peer_started_carries_its_text() {
    const OPERATOR: &str = "operator-token-0123456789";
    const PEER: &str = "peer-token-0123456789";
    let t = tempfile::tempdir().unwrap();
    for (bearer, withheld) in [(PEER, true), (OPERATOR, false)] {
        let play = t.path().join("play.json");
        std::fs::write(
            &play,
            json!({"turns": [call("workflow.status", json!({"name": "job"})), echo()]}).to_string(),
        )
        .unwrap();
        let state = t.path().join("state");
        let _ = std::fs::remove_dir_all(&state);
        let cfg = t.path().join("agent.yaml");
        let log = t.path().join("daemon.log");
        let (mut child, addr) = common::spawn_bound(|port| {
            std::fs::write(
                &cfg,
                format!(
                    "agent:\n  name: hold\n  instruction: Look after it.\n  preflight: never\n\
                     intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
                     store: {{kind: file, file: {{path: {}}}}}\n\
                     lifecycle: {{run_until: drained}}\n\
                     observability:\n  log_level: info\n  log_content: true\n\
                     a2a:\n  listen: http://127.0.0.1:{port}\n  principals:\n\
                     \x20   - {{id: ops, match: {{bearer_ref: \"{{{{secret:WITHHOLD_OP_TOKEN}}}}\"}}, role: operator}}\n\
                     \x20   - {{id: peer, match: {{bearer_ref: \"{{{{secret:WITHHOLD_PEER_TOKEN}}}}\"}}, role: agent}}\n\
                     mcp:\n  servers:\n\
                     \x20   - {{name: vault, endpoint: \"https://vault.invalid/mcp\", tags: {{\"*\": [sensitive, egress]}}}}\n\
                     workflows:\n  - name: job\n    steps:\n      m: {{kind: manual}}\n      \
                     f: {{kind: finish, depends_on: [m], status: completed, output: \"{RUN_SECRET}\"}}\n",
                    play.display(),
                    state.display()
                ),
            )
            .unwrap();
            let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
                .args(["--config", &cfg.to_string_lossy()])
                .env("WITHHOLD_OP_TOKEN", OPERATOR)
                .env("WITHHOLD_PEER_TOKEN", PEER)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .expect("spawn agentd");
            (Stopped(child), log.to_string_lossy().into_owned())
        });
        let read = || std::fs::read_to_string(&log).unwrap_or_default();
        let started = common::SendMessage::command("workflow.run", json!({"workflow": "job"}))
            .bearer(bearer)
            .post(&addr);
        assert!(started.get("error").is_none(), "{started}");
        let wait = |what: &str, done: &dyn Fn(&str) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !done(&read()) {
                assert!(Instant::now() < deadline, "{what}:\n{}", read());
                std::thread::sleep(Duration::from_millis(30));
            }
        };
        wait("the run", &|l| l.contains("\"event\":\"run.done\""));
        let asked = common::SendMessage::text("go")
            .bearer(OPERATOR)
            .return_immediately()
            .post(&addr);
        assert!(asked.get("error").is_none(), "{asked}");
        wait("the root's turn", &|l| {
            l.contains("\"event\":\"turn.reply\"")
        });
        child.stop();
        let ran = Ran { log: read(), state };
        let answer = ran.answer();
        assert_eq!(
            answer.contains("output withheld: this run carries outside input (it was started by the A2A peer agent:peer)"),
            withheld,
            "{bearer}: {answer}\n{}",
            ran.log
        );
        assert_eq!(answer.contains(RUN_SECRET), !withheld, "{bearer}: {answer}");
    }
}

/// A daemon stopped on drop.
#[cfg(feature = "a2a")]
struct Stopped(std::process::Child);

#[cfg(feature = "a2a")]
impl Stopped {
    fn stop(&mut self) {
        unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(feature = "a2a")]
impl Drop for Stopped {
    fn drop(&mut self) {
        self.stop();
    }
}
