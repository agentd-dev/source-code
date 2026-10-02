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

/// A child's instruction: [`OPENING`] is what `subagent.list` hands back of
/// it (the first 80 characters), and the word the mock answers a child on
/// comes after, so a root handed the opening is not taken for a child.
const OPENING: &str = "CHILD-INSTRUCTION";
fn instruction(asked: &str) -> String {
    format!("{OPENING} {} {asked}", ".".repeat(80))
}

/// `hold`, started from `m` (the default start, so its sync tool `hold.now`
/// runs from it too): it spawns a child that answers [`CHILD_SECRET`] after
/// a moment, and finishes with [`RUN_SECRET`]. With `peer`, an A2A peer can
/// start it, so its runs carry outside input. `hold_slow` is the same with a
/// child that takes long enough for a later call to bind it before it ends.
fn hold(peer: bool) -> String {
    let def = |name: &str, tool: &str, instruction: &str| {
        format!(
            "  - name: {name}\n{tool}    steps:\n{}      \
             m: {{kind: manual}}\n      \
             c: {{kind: subagent, depends_on: [m], mode: async, instruction: \"{instruction}\", servers: []}}\n      \
             f: {{kind: finish, depends_on: [c], status: completed, output: \"{RUN_SECRET}\"}}\n",
            if peer {
                "      peer: {kind: a2a, command: poke}\n"
            } else {
                ""
            }
        )
    };
    def(
        "hold",
        "    tool: {name: hold.now, mode: sync}\n",
        &instruction("CHILD-ASKED"),
    ) + &def("hold_slow", "", &instruction("SLOW-ASKED"))
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

/// One daemon's configuration, beyond the root's `vault`.
#[derive(Default)]
struct Cfg<'a> {
    /// What `vault`, the server the root holds, is tagged.
    vault_tags: &'a str,
    /// More lines under `agent:`.
    agent: &'a str,
    /// More top-level sections.
    top: &'a str,
    workflows: &'a str,
    /// The root's turns (a mock playbook).
    turns: Value,
    /// An event to wait for after the root's turn answers, for what lands
    /// after it.
    until: Option<&'a str>,
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
    run_cfg(
        dir,
        Cfg {
            vault_tags,
            agent,
            workflows,
            turns,
            ..Default::default()
        },
        settle,
    )
}

/// The child's answer comes after a moment — long enough, for `SLOW-ASKED`,
/// that a call after the run that spawned it is made before it lands.
fn play(turns: Value) -> Value {
    json!({
        "match": [{"when_contains": "SLOW-ASKED", "content": CHILD_SECRET, "delay_ms": 3000},
                  {"when_contains": "CHILD-ASKED", "content": CHILD_SECRET, "delay_ms": 300}],
        "turns": turns
    })
}

fn run_cfg(dir: &Path, c: Cfg, settle: Duration) -> Ran {
    let play_path = dir.join("play.json");
    std::fs::write(&play_path, play(c.turns).to_string()).unwrap();
    let state = dir.join("state");
    let _ = std::fs::remove_dir_all(&state);
    let cfg = dir.join("agent.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: hold\n  prompt: go\n  instruction: Look after it.\n{}\
             intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
             store: {{kind: file, file: {{path: {}}}}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: info\n  log_content: true\n{}\
             mcp:\n  servers:\n\
             \x20   - {{name: vault, endpoint: \"https://vault.invalid/mcp\", tags: {{\"*\": [{}]}}}}\n\
             workflows:\n{}",
            c.agent,
            play_path.display(),
            state.display(),
            c.top,
            c.vault_tags,
            c.workflows,
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
    let mut wait = |event: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        let line = format!("\"event\":\"{event}\"");
        while !read().contains(&line) {
            if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("no {event}:\n{}", read());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    };
    wait("turn.reply");
    if let Some(event) = c.until {
        wait(event);
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

/// A call naming what the last tool result returned: `from` maps argument
/// names to JSON pointers into it (a run id, which no script can know).
fn call_with(name: &str, args: Value, from: Value) -> Value {
    json!({"tool_calls": [{"name": name, "arguments": args, "arguments_from_tool_result": from}]})
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
        // The plan note a bound child settles, read back with `plan.get`. The
        // child of `hold_slow` is still at work when the item is bound.
        (
            "plan.update",
            vec![
                call(
                    "plan.create",
                    json!({"goal": "watch", "items": ["the child"]}),
                ),
                call("workflow.run", json!({"name": "hold_slow", "wait": true})),
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
        // A finished run, answered at once.
        (
            "workflow.wait",
            vec![
                run_hold(),
                call_with("workflow.wait", json!({}), json!({"run": "/run"})),
                echo(),
            ],
            RUN_SECRET,
        ),
        // A run still going, answered when it lands.
        (
            "workflow.wait",
            vec![
                call("workflow.run", json!({"name": "hold"})),
                call_with("workflow.wait", json!({}), json!({"run": "/run"})),
                echo(),
            ],
            RUN_SECRET,
        ),
        // A child already finished, answered at once.
        (
            "subagent.await",
            vec![
                run_hold(),
                call("subagent.await", json!({"handle": "sub-2"})),
                call("subagent.await", json!({"handle": "sub-2"})),
                echo(),
            ],
            CHILD_SECRET,
        ),
        (
            "subagent.status",
            vec![
                run_hold(),
                call("subagent.await", json!({"handle": "sub-2"})),
                call("subagent.status", json!({"handle": "sub-2"})),
                echo(),
            ],
            CHILD_SECRET,
        ),
        // The opening of each child's instruction, which is its spawner's
        // text.
        (
            "subagent.list",
            vec![run_hold(), call("subagent.list", json!({})), echo()],
            OPENING,
        ),
        // The instance status lists every run with its output.
        (
            "status",
            vec![run_hold(), call("status", json!({})), echo()],
            RUN_SECRET,
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
        // A child listed may still be at work; a plan item carries no
        // status of the child's.
        if via != "plan.update" && via != "subagent.list" {
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
    let warm = format!(
        "  - name: hold\n    steps:\n      peer: {{kind: a2a, command: poke}}\n      \
         m: {{kind: manual}}\n      \
         c: {{kind: subagent, depends_on: [m], mode: warm, instruction: \"{}\", servers: []}}\n      \
         f: {{kind: finish, depends_on: [c], status: completed}}\n",
        instruction("CHILD-ASKED")
    );
    let turns = json!([
        call("workflow.run", json!({"name": "hold", "wait": true})),
        echo()
    ]);
    // The daemon is stopped once the child has finished a turn, whenever
    // that lands, not after a guess at how long it takes.
    let ran_with = |agent: &str| {
        run_cfg(
            t.path(),
            Cfg {
                vault_tags: "sensitive, egress",
                agent,
                workflows: &warm,
                turns: turns.clone(),
                until: Some("subagent.turn"),
                ..Default::default()
            },
            Duration::from_millis(100),
        )
    };
    let turned = |ran: &Ran| {
        ran.root_notes()
            .into_iter()
            .filter(|n| n.contains("finished a turn"))
            .collect::<Vec<_>>()
    };
    let ran = ran_with("");
    assert_eq!(turned(&ran), Vec::<String>::new());

    let ran = ran_with("  wake_on: [subagent_result]\n");
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

/// An `emit` with a `note:` and no stream writes the rendered note into the
/// root transcript: the run's text, read back like the note a finished run
/// leaves — withheld from a root holding both legs.
#[test]
fn an_emit_note_of_a_tainted_run_is_withheld() {
    let t = tempfile::tempdir().unwrap();
    let workflows = "  - name: noter\n    steps:\n      peer: {kind: a2a, command: poke}\n      \
                     m: {kind: manual}\n      \
                     e: {kind: emit, depends_on: [m], note: \"EMIT-SECRET\"}\n      \
                     f: {kind: finish, depends_on: [e], status: completed}\n";
    let turns = json!([
        call("workflow.run", json!({"name": "noter", "wait": true})),
        echo()
    ]);
    let ran = run(
        t.path(),
        "sensitive, egress",
        "",
        workflows,
        turns,
        Duration::ZERO,
    );
    let notes = ran.root_notes();
    assert!(
        notes.iter().any(|n| n.starts_with("run noter-")
            && n.contains("output withheld: this run carries outside input")),
        "{notes:?}\n{}",
        ran.log
    );
    assert!(
        !notes.iter().any(|n| n.contains("EMIT-SECRET")),
        "{notes:?}"
    );
    assert!(
        ran.withheld().iter().any(|v| v == "emit note"),
        "{}",
        ran.log
    );
}

/// A child carries what every run that steered it carried. The root spawns
/// a warm child, so it carries nothing of the root's; a run that carries
/// outside input steers it with `subagent.send`, and from then on the
/// root, holding both legs, reads it withheld. A run that carries nothing
/// steers it without changing that.
#[test]
fn a_child_steered_by_a_tainted_run_carries_its_text() {
    let t = tempfile::tempdir().unwrap();
    for (peer, withheld) in [(true, true), (false, false)] {
        let workflows = format!(
            "  - name: steer\n    steps:\n{}      m: {{kind: manual}}\n      \
             s: {{kind: tool, depends_on: [m], name: subagent.send, args: {{handle: sub-2, message: \"Again.\"}}}}\n      \
             f: {{kind: finish, depends_on: [s], status: completed}}\n",
            if peer {
                "      peer: {kind: a2a, command: poke}\n"
            } else {
                ""
            }
        );
        // From a template, so the root's own call does not carry the word
        // the mock answers a child on.
        let top = format!(
            "subagents:\n  templates:\n    warmer:\n      instruction: \"{}\"\n      servers: []\n",
            instruction("CHILD-ASKED")
        );
        let turns = json!([
            call(
                "subagent.run",
                json!({"template": "warmer", "mode": "warm"})
            ),
            call("workflow.run", json!({"name": "steer", "wait": true})),
            call("subagent.list", json!({})),
            echo()
        ]);
        let ran = run_cfg(
            t.path(),
            Cfg {
                vault_tags: "sensitive, egress",
                top: &top,
                workflows: &workflows,
                turns,
                ..Default::default()
            },
            Duration::ZERO,
        );
        assert!(
            ran.events("subagent.spawn")
                .first()
                .is_some_and(|e| e["handle"] == "sub-2")
                && ran
                    .events("run.done")
                    .iter()
                    .any(|e| e["workflow"] == "steer" && e["status"] == "completed"),
            "the run steers the root's child:\n{}",
            ran.log
        );
        let answer = ran.answer();
        assert_eq!(
            answer.contains("output withheld: this subagent carries outside input"),
            withheld,
            "{answer}\n{}",
            ran.log
        );
        assert_eq!(answer.contains(OPENING), !withheld, "{answer}");
    }
}

/// What a run carries is on its own record, so its result stays withheld
/// when the definition it ran under goes: replaced by one nothing outside
/// reaches, deleted, or deleted while the run is still going and a wait on
/// it is answered after the definition is released.
#[test]
fn a_runs_result_stays_withheld_after_its_definition_goes() {
    let t = tempfile::tempdir().unwrap();
    let tainted = json!({"definition": {"name": "w", "steps": {
        "peer": {"kind": "a2a", "command": "poke"},
        "m": {"kind": "manual"},
        "s": {"kind": "sleep", "depends_on": ["m"], "duration": "300ms"},
        "f": {"kind": "finish", "depends_on": ["s"], "status": "completed", "output": RUN_SECRET}}}});
    let plain = json!({"name": "w", "definition": {"name": "w", "steps": {
        "m": {"kind": "manual"},
        "f": {"kind": "finish", "depends_on": ["m"], "status": "completed", "output": "PLAIN"}}}});
    for (case, turns) in [
        (
            "replaced",
            json!([
                call("workflow.create", tainted.clone()),
                call("workflow.run", json!({"name": "w", "wait": true})),
                call("workflow.update", plain.clone()),
                call("status", json!({})),
                echo()
            ]),
        ),
        (
            "deleted",
            json!([
                call("workflow.create", tainted.clone()),
                call("workflow.run", json!({"name": "w", "wait": true})),
                call("workflow.delete", json!({"name": "w"})),
                call("status", json!({})),
                echo()
            ]),
        ),
        // `workflow.delete` answers `{ok}`, so the wait is asked in the same
        // turn, from the result of the start before it.
        (
            "deleted while it runs",
            json!([
                call("workflow.create", tainted.clone()),
                call("workflow.run", json!({"name": "w"})),
                {"tool_calls": [
                    {"name": "workflow.delete", "arguments": {"name": "w"}},
                    {"name": "workflow.wait", "arguments": {},
                     "arguments_from_tool_result": {"run": "/run"}}]},
                echo()
            ]),
        ),
    ] {
        let ran = run(
            t.path(),
            "sensitive, egress",
            "",
            "  - name: idle\n    steps:\n      m: {kind: manual}\n      f: {kind: finish, depends_on: [m], status: completed}\n",
            turns,
            Duration::ZERO,
        );
        assert!(
            ran.events("workflow.defined")
                .iter()
                .any(|e| e["name"] == "w"),
            "{case}: the stored definition loads:\n{}",
            ran.log
        );
        let answer = ran.answer();
        assert!(
            answer.contains("output withheld: this run carries outside input")
                && !answer.contains(RUN_SECRET),
            "{case}: {answer}\n{}",
            ran.log
        );
    }
}

#[cfg(feature = "a2a")]
const OPERATOR: &str = "operator-token-0123456789";
#[cfg(feature = "a2a")]
const PEER: &str = "peer-token-0123456789";

/// A daemon an operator and an A2A peer (`agent:peer`, an `agent`
/// principal) talk to, whose root holds `vault` with both other legs.
#[cfg(feature = "a2a")]
struct Peered {
    child: Stopped,
    addr: String,
    log: PathBuf,
    state: PathBuf,
}

#[cfg(feature = "a2a")]
impl Peered {
    /// The configuration for `workflows` (and more top-level `top`), with
    /// the root's turns `turns`, on `port`.
    fn write(dir: &Path, port: u16, top: &str, workflows: &str, turns: &Value) {
        std::fs::write(dir.join("play.json"), play(turns.clone()).to_string()).unwrap();
        std::fs::write(
            dir.join("agent.yaml"),
            format!(
                "agent:\n  name: hold\n  instruction: Look after it.\n  preflight: never\n\
                 intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
                 store: {{kind: file, file: {{path: {}}}}}\n\
                 lifecycle: {{run_until: drained}}\n\
                 observability:\n  log_level: info\n  log_content: true\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n  principals:\n\
                 \x20   - {{id: ops, match: {{bearer_ref: \"{{{{secret:WITHHOLD_OP_TOKEN}}}}\"}}, role: operator}}\n\
                 \x20   - {{id: peer, match: {{bearer_ref: \"{{{{secret:WITHHOLD_PEER_TOKEN}}}}\"}}, role: agent, grants: [workflow.signal]}}\n\
                 {top}\
                 mcp:\n  servers:\n\
                 \x20   - {{name: vault, endpoint: \"https://vault.invalid/mcp\", tags: {{\"*\": [sensitive, egress]}}}}\n\
                 workflows:\n{workflows}",
                dir.join("play.json").display(),
                dir.join("state").display()
            ),
        )
        .unwrap();
    }

    /// Start it over `dir`'s store — a fresh one unless `keep`.
    fn start(dir: &Path, top: &str, workflows: &str, turns: Value, keep: bool) -> Peered {
        let state = dir.join("state");
        if !keep {
            let _ = std::fs::remove_dir_all(&state);
        }
        let log = dir.join("daemon.log");
        let cfg = dir.join("agent.yaml");
        let (child, addr) = common::spawn_bound(|port| {
            Peered::write(dir, port, top, workflows, &turns);
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
        Peered {
            child,
            addr,
            log,
            state,
        }
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Wait until `event` has been logged `n` times.
    fn wait(&self, event: &str, n: usize) {
        let line = format!("\"event\":\"{event}\"");
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.read().matches(&line).count() < n {
            assert!(Instant::now() < deadline, "{n} × {event}:\n{}", self.read());
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// Wait until the log says `what`.
    fn wait_for(&self, what: &str) {
        self.wait_until(|log| log.contains(what));
    }

    fn wait_until(&self, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(&self.read()) {
            assert!(Instant::now() < deadline, "{}", self.read());
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// `op` as `bearer`, without waiting for the work it starts: the test
    /// waits for what it needs in the log.
    fn command(&self, bearer: &str, op: &str, args: Value) {
        let r = common::SendMessage::command(op, args)
            .bearer(bearer)
            .return_immediately()
            .post(&self.addr);
        assert!(r.get("error").is_none(), "{op}: {r}");
    }

    /// The operator asks the root, which plays its turns; stop once it
    /// answers.
    fn ask(mut self) -> Ran {
        let r = common::SendMessage::text("go")
            .bearer(OPERATOR)
            .return_immediately()
            .post(&self.addr);
        assert!(r.get("error").is_none(), "{r}");
        self.wait("turn.reply", 1);
        self.child.stop();
        Ran {
            log: self.read(),
            state: self.state.clone(),
        }
    }
}

/// The marker a root holding both legs gets for a run the peer's request
/// started, or a run that carries it.
#[cfg(feature = "a2a")]
fn peer_marker(answer: &str) -> bool {
    answer.contains("output withheld: this run carries outside input")
        && answer.contains("A2A peer agent:peer")
        && !answer.contains(RUN_SECRET)
}

/// What a run's definition can be handed is put on the run's record when
/// it starts and again when it ends, so it holds whatever the table says
/// later: taint present at the start and gone by the end (the stream's
/// producer deleted while the run is going), and taint that arrived during
/// the run (a producer created) with the definition deleted after. Each
/// time the root reads the run back after, holding both legs.
#[test]
fn a_runs_record_keeps_the_taint_its_definition_had_while_it_ran() {
    let t = tempfile::tempdir().unwrap();
    let producer = json!({"definition": {"name": "p", "steps": {
        "s": {"kind": "a2a", "command": "note", "into": {"stream": "inbox", "subject": "msg"}}}}});
    let consumer = json!({"definition": {"name": "w", "steps": {
        "st": {"kind": "stream", "stream": "inbox"},
        "m": {"kind": "manual"},
        "s": {"kind": "sleep", "depends_on": ["m"], "duration": "500ms"},
        "f": {"kind": "finish", "depends_on": ["s"], "status": "completed", "output": RUN_SECRET}}}});
    let start = call("workflow.run", json!({"name": "w", "start": "m"}));
    let wait = json!({"name": "workflow.wait", "arguments": {},
                      "arguments_from_tool_result": {"run": "/run"}});
    for (case, turns) in [
        (
            "the producer deleted while it runs",
            json!([
                call("workflow.create", producer.clone()),
                call("workflow.create", consumer.clone()),
                start.clone(),
                {"tool_calls": [{"name": "workflow.delete", "arguments": {"name": "p"}}, wait.clone()]},
                echo()
            ]),
        ),
        (
            "a producer created while it runs, the definition deleted after",
            json!([
                call("workflow.create", consumer.clone()),
                start.clone(),
                {"tool_calls": [{"name": "workflow.create", "arguments": producer.clone()}, wait.clone()]},
                echo(),
                call("workflow.delete", json!({"name": "w"})),
                call("status", json!({})),
                echo()
            ]),
        ),
    ] {
        let ran = run_cfg(
            t.path(),
            Cfg {
                vault_tags: "sensitive, egress",
                top: "streams:\n  inbox: {}\n",
                workflows: "  - name: idle\n    steps:\n      m: {kind: manual}\n      f: {kind: finish, depends_on: [m], status: completed}\n",
                turns,
                ..Default::default()
            },
            Duration::ZERO,
        );
        let answer = ran.answer();
        assert!(
            answer.contains("output withheld: this run carries outside input")
                && !answer.contains(RUN_SECRET),
            "{case}: {answer}\n{}",
            ran.log
        );
    }
}

/// A run an A2A peer — an `agent` principal — asked for directly carries the
/// peer's text, though its definition opens no route: the root holding both
/// legs reads it back withheld. The same run started by the operator is read
/// whole.
#[cfg(feature = "a2a")]
#[test]
fn a_run_a_peer_started_carries_its_text() {
    let t = tempfile::tempdir().unwrap();
    let job = format!(
        "  - name: job\n    steps:\n      m: {{kind: manual}}\n      \
         f: {{kind: finish, depends_on: [m], status: completed, output: \"{RUN_SECRET}\"}}\n"
    );
    for (bearer, withheld) in [(PEER, true), (OPERATOR, false)] {
        let turns = json!([call("workflow.status", json!({"name": "job"})), echo()]);
        let d = Peered::start(t.path(), "", &job, turns, false);
        d.command(bearer, "workflow.run", json!({"workflow": "job"}));
        d.wait("run.done", 1);
        let ran = d.ask();
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

/// What a peer-started run starts carries the peer's text too: through the
/// `workflow.run` tool (which a workflow tool is, spelled as its own name)
/// and through a `workflow` step. Nothing in the definitions opens a route,
/// so only what each start recorded on the run says so.
#[cfg(feature = "a2a")]
#[test]
fn a_run_a_peer_started_run_starts_carries_its_text() {
    let t = tempfile::tempdir().unwrap();
    for start in [
        "{kind: tool, depends_on: [m], name: workflow.run, args: {name: inner, wait: true}}",
        "{kind: workflow, depends_on: [m], name: inner}",
    ] {
        let workflows = format!(
            "  - name: job\n    steps:\n      m: {{kind: manual}}\n      t: {start}\n      \
             f: {{kind: finish, depends_on: [t], status: completed}}\n  \
             - name: inner\n    steps:\n      \
             m: {{kind: manual}}\n      \
             f: {{kind: finish, depends_on: [m], status: completed, output: \"{RUN_SECRET}\"}}\n"
        );
        let turns = json!([call("workflow.status", json!({"name": "inner"})), echo()]);
        let d = Peered::start(t.path(), "", &workflows, turns, false);
        d.command(PEER, "workflow.run", json!({"workflow": "job"}));
        d.wait("run.done", 2);
        let ran = d.ask();
        let answer = ran.answer();
        assert!(peer_marker(&answer), "{start}: {answer}\n{}", ran.log);
    }
}

/// A reader holding a leg less is handed a peer-started run whole — and
/// carries it on: the root, holding both, reads that reader's result
/// withheld. The static check cannot see this (nothing in the definitions
/// opens a route); the reader's record does.
#[cfg(feature = "a2a")]
#[test]
fn a_reader_handed_a_peer_started_run_whole_carries_it_on() {
    let t = tempfile::tempdir().unwrap();
    let workflows = format!(
        "  - name: job\n    steps:\n      m: {{kind: manual}}\n      \
         f: {{kind: finish, depends_on: [m], status: completed, output: \"{RUN_SECRET}\"}}\n  \
         - name: reader\n    steps:\n      s: {{kind: manual}}\n      \
         r: {{kind: tool, depends_on: [s], name: workflow.status, args: {{name: job}}}}\n      \
         f: {{kind: finish, depends_on: [r], status: completed, output: \"{{{{steps.r.output}}}}\"}}\n"
    );
    let turns = json!([
        call("workflow.run", json!({"name": "reader", "wait": true})),
        echo()
    ]);
    let d = Peered::start(t.path(), "", &workflows, turns, false);
    d.command(PEER, "workflow.run", json!({"workflow": "job"}));
    d.wait("run.done", 1);
    let ran = d.ask();
    assert!(
        ran.events("run.done")
            .iter()
            .any(|e| e["workflow"] == "reader" && e["output"].to_string().contains(RUN_SECRET)),
        "the reader holds a leg less and is handed the run whole:\n{}",
        ran.log
    );
    assert!(peer_marker(&ran.answer()), "{}\n{}", ran.answer(), ran.log);
}

/// What a run carries is on its durable record: after a restart — when the
/// peer has not been heard from again — a peer-started run's result is still
/// withheld, and so is the result of the reader that was handed it whole
/// before the restart.
#[cfg(feature = "a2a")]
#[test]
fn a_peer_started_runs_result_is_withheld_after_a_restart() {
    let t = tempfile::tempdir().unwrap();
    let workflows = format!(
        "  - name: job\n    steps:\n      m: {{kind: manual}}\n      \
         f: {{kind: finish, depends_on: [m], status: completed, output: \"{RUN_SECRET}\"}}\n  \
         - name: reader\n    steps:\n      s: {{kind: manual}}\n      \
         r: {{kind: tool, depends_on: [s], name: workflow.status, args: {{name: job}}}}\n      \
         f: {{kind: finish, depends_on: [r], status: completed, output: \"{{{{steps.r.output}}}}\"}}\n"
    );
    let d = Peered::start(
        t.path(),
        "",
        &workflows,
        json!([
            call("workflow.run", json!({"name": "reader", "wait": true})),
            echo()
        ]),
        false,
    );
    d.command(PEER, "workflow.run", json!({"workflow": "job"}));
    d.wait("run.done", 1);
    let ran = d.ask();
    assert!(
        ran.events("run.done")
            .iter()
            .any(|e| e["workflow"] == "reader" && e["output"].to_string().contains(RUN_SECRET)),
        "{}",
        ran.log
    );
    let ran = Peered::start(
        t.path(),
        "",
        &workflows,
        json!([call("status", json!({})), echo()]),
        true,
    )
    .ask();
    let answer = ran.answer();
    for w in ["job", "reader"] {
        assert!(
            answer.contains(&format!("\"workflow\":\"{w}\"")),
            "{w} comes back from the store: {answer}\n{}",
            ran.log
        );
    }
    assert!(peer_marker(&answer), "{answer}\n{}", ran.log);
}

/// A signal's payload, and an A2A message a wait takes, are the sender's
/// text: the run they reach carries what the sender carried. A run the peer
/// started signals with a `workflow.signal` step (waking a wait) and with
/// the `workflow.signal` tool (firing a `signal` start); the peer signals
/// itself over A2A; the peer's message wakes a `wait {on: message}`. No
/// definition opens a route, so only what each delivery recorded says so.
#[cfg(feature = "a2a")]
#[test]
fn a_run_a_peer_signals_or_messages_carries_its_text() {
    let t = tempfile::tempdir().unwrap();
    let waits = "  - name: waiter\n    steps:\n      m: {kind: manual}\n      \
                 w: {kind: wait, depends_on: [m], on: signal, signal: go, timeout: 10m}\n      \
                 f: {kind: finish, depends_on: [w], status: completed, output: \"{{steps.w.output}}\"}\n  \
                 - name: started\n    steps:\n      k: {kind: signal, name: go}\n      \
                 f: {kind: finish, depends_on: [k], status: completed, output: \"{{steps.k.output}}\"}\n  \
                 - name: listener\n    steps:\n      m: {kind: manual}\n      \
                 w: {kind: wait, depends_on: [m], on: message, conversation: shared, from: \"agent:peer\", timeout: 10m}\n      \
                 f: {kind: finish, depends_on: [w], status: completed, output: \"{{steps.w.output}}\"}\n";
    let signaller = |send: &str| {
        format!(
            "  - name: signaller\n    steps:\n      m: {{kind: manual}}\n      s: {send}\n      \
             f: {{kind: finish, depends_on: [s], status: completed}}\n"
        )
    };
    let step =
        signaller("{kind: workflow.signal, depends_on: [m], name: go, payload: \"PEER-TEXT\"}");
    let tool = signaller(
        "{kind: tool, depends_on: [m], name: workflow.signal, args: {name: go, payload: \"PEER-TEXT\"}}",
    );
    // The reached run, and how the text reaches it once the operator has
    // started `waiter` or `listener` (whose run waits for it).
    type Deliver = fn(&Peered);
    let cases: [(&str, &str, &str, Deliver); 4] = [
        ("a signal step waking a wait", &step, "waiter", |d| {
            d.command(PEER, "workflow.run", json!({"workflow": "signaller"}));
        }),
        ("the signal tool firing a start", &tool, "started", |d| {
            d.command(PEER, "workflow.run", json!({"workflow": "signaller"}));
        }),
        ("the peer's own signal", "", "started", |d| {
            d.command(
                PEER,
                "workflow.signal",
                json!({"name": "go", "payload": "PEER-TEXT"}),
            );
        }),
        ("the peer's message", "", "listener", |d| {
            let r = common::SendMessage::text("PEER-TEXT")
                .context("shared")
                .bearer(PEER)
                .return_immediately()
                .post(&d.addr);
            assert!(r.get("error").is_none(), "{r}");
        }),
    ];
    for (case, extra, reached, deliver) in cases {
        let turns = json!([call("workflow.status", json!({"name": reached})), echo()]);
        let d = Peered::start(t.path(), "", &format!("{waits}{extra}"), turns, false);
        if reached != "started" {
            d.command(OPERATOR, "workflow.run", json!({"workflow": reached}));
            d.wait_for("\"step\":\"w\"");
        }
        deliver(&d);
        d.wait_until(|log| {
            log.lines().any(|l| {
                l.contains("\"event\":\"run.done\"")
                    && l.contains(&format!("\"workflow\":\"{reached}\""))
            })
        });
        let ran = d.ask();
        let answer = ran.answer();
        assert!(
            answer.contains("output withheld: this run carries outside input")
                && answer.contains("A2A peer agent:peer")
                && !answer.contains("PEER-TEXT"),
            "{case}: {answer}\n{}",
            ran.log
        );
    }
}

/// The root is judged by what it can hand its text on to, as a run is. With
/// its own `subagent.run` and `subagent.send` disabled and only `sensitive`
/// in its own reach, a workflow's `subagent` step can still spawn the
/// instance template whose service holds `egress` — so the root holds both,
/// and is withheld from.
#[cfg(feature = "a2a")]
#[test]
fn the_root_is_judged_by_what_it_can_hand_on() {
    let t = tempfile::tempdir().unwrap();
    let workflows = format!(
        "{}  - name: desk_run\n    steps:\n      m: {{kind: manual}}\n      \
         c: {{kind: subagent, depends_on: [m], template: desk}}\n      \
         f: {{kind: finish, depends_on: [c], status: completed}}\n",
        hold(true)
    );
    let top = "services:\n  web: {kind: mcp, endpoint: \"https://web.invalid/mcp\", tags: {\"*\": [egress]}}\n\
               subagents:\n  templates:\n    desk:\n      instruction: |\n        The desk.\n        \
               :::!mcp{name=web}\n        service: web\n        :::\n\
               tools:\n  disabled: [subagent.run, subagent.send]\n";
    let turns = json!([
        call("workflow.run", json!({"name": "hold", "wait": true})),
        echo()
    ]);
    for (tags, withheld) in [("sensitive", true), ("", false)] {
        let ran = run_cfg(
            t.path(),
            Cfg {
                vault_tags: tags,
                top,
                workflows: &workflows,
                turns: turns.clone(),
                ..Default::default()
            },
            Duration::ZERO,
        );
        let answer = ran.answer();
        assert_eq!(
            answer.contains("this conversation holds sensitive and egress tools"),
            withheld,
            "{tags}: {answer}\n{}",
            ran.log
        );
        assert_eq!(answer.contains(RUN_SECRET), !withheld, "{tags}: {answer}");
    }
}

/// `b` reads `a` back whole while it holds a leg less. Its `t`, after its
/// `finish` and never run, may start any workflow: once a workflow holding
/// `vault` exists, `b` reaches both legs through it and is withheld from.
fn reader_b() -> String {
    "  - name: b\n    steps:\n      s: {kind: manual}\n      \
     r: {kind: workflow, depends_on: [s], name: a}\n      \
     f: {kind: finish, depends_on: [r], status: completed, output: \"{{steps.r.output}}\"}\n      \
     t: {kind: workflow, depends_on: [f], name: \"{{inputs.w}}\", mode: detached}\n"
        .to_string()
}

/// `a` carries outside input (an A2A peer can start it).
fn tainted_a() -> String {
    format!(
        "  - name: a\n    steps:\n      peer: {{kind: a2a, command: poke}}\n      \
         m: {{kind: manual}}\n      \
         f: {{kind: finish, depends_on: [m], status: completed, output: \"{RUN_SECRET}\"}}\n"
    )
}

/// What `b` hands back once it holds both legs: `a`'s output withheld from
/// it, so the root reads `b`'s whole — a marker that names `b`.
fn withheld_from_b(answer: &str) -> bool {
    answer.contains("workflow \\\"b\\\" holds sensitive and egress tools")
        && !answer.contains(RUN_SECRET)
}

/// A reload that only adds a workflow — `c`, holding `vault` — retires
/// nothing and moves no tool's tags (`a`'s runs are tainted before and
/// after), and still changes what is withheld: `b`, which can start `c`,
/// now reaches both legs, and reading `a` back is withheld from it.
#[cfg(all(feature = "a2a", feature = "hot-reload"))]
#[test]
fn a_reload_that_moves_only_who_holds_both_legs_changes_what_is_withheld() {
    let t = tempfile::tempdir().unwrap();
    let c = "  - name: c\n    steps:\n      m: {kind: manual}\n      \
             x: {kind: agent, depends_on: [m], instruction: \"Act.\", servers: [vault], tools: [\"vault.*\"]}\n      \
             f: {kind: finish, depends_on: [x], status: completed}\n";
    let before = format!("{}{}", tainted_a(), reader_b());
    let turns = json!([
        call("workflow.run", json!({"name": "b", "wait": true})),
        echo()
    ]);
    let d = Peered::start(t.path(), "", &before, turns.clone(), false);
    let port: u16 = d.addr.rsplit(':').next().unwrap().parse().unwrap();
    Peered::write(t.path(), port, "", &format!("{before}{c}"), &turns);
    unsafe { libc::kill(d.child.0.id() as i32, libc::SIGHUP) };
    d.wait("config.reloaded", 1);
    let ran = d.ask();
    assert!(
        ran.events("registry.workflow_tools").len() <= 1,
        "the reload moved no tool's tags:\n{}",
        ran.log
    );
    assert!(
        withheld_from_b(&ran.answer()),
        "{}\n{}",
        ran.answer(),
        ran.log
    );
}

/// The same through `workflow.create`: the definition it adds moves no tag,
/// and `b` is withheld from after it.
#[test]
fn a_definition_created_that_moves_only_who_holds_both_legs_changes_what_is_withheld() {
    let t = tempfile::tempdir().unwrap();
    let c = json!({"definition": {"name": "c", "steps": {
        "m": {"kind": "manual"},
        "x": {"kind": "agent", "depends_on": ["m"], "instruction": "Act.",
              "servers": ["vault"], "tools": ["vault.*"]},
        "f": {"kind": "finish", "depends_on": ["x"], "status": "completed"}}}});
    let turns = json!([
        call("workflow.create", c),
        call("workflow.run", json!({"name": "b", "wait": true})),
        echo()
    ]);
    let ran = run(
        t.path(),
        "sensitive, egress",
        "",
        &format!("{}{}", tainted_a(), reader_b()),
        turns,
        Duration::ZERO,
    );
    assert!(
        withheld_from_b(&ran.answer()),
        "{}\n{}",
        ran.answer(),
        ran.log
    );
}

/// What a reader still at work was handed whole goes on its durable record
/// at the next checkpoint: across a restart, its result is withheld once it
/// finishes.
#[cfg(feature = "a2a")]
#[test]
fn a_live_readers_record_keeps_what_it_was_handed_across_a_restart() {
    let t = tempfile::tempdir().unwrap();
    let workflows = format!(
        "  - name: job\n    steps:\n      m: {{kind: manual}}\n      \
         f: {{kind: finish, depends_on: [m], status: completed, output: \"{RUN_SECRET}\"}}\n  \
         - name: reader\n    steps:\n      s: {{kind: manual}}\n      \
         r: {{kind: tool, depends_on: [s], name: workflow.status, args: {{name: job}}}}\n      \
         w: {{kind: wait, depends_on: [r], on: signal, signal: go, timeout: 10m}}\n      \
         f: {{kind: finish, depends_on: [w], status: completed, output: \"{{{{steps.r.output}}}}\"}}\n"
    );
    let turns = json!([call("workflow.status", json!({"name": "reader"})), echo()]);
    let mut d = Peered::start(t.path(), "", &workflows, turns.clone(), false);
    d.command(PEER, "workflow.run", json!({"workflow": "job"}));
    d.wait("run.done", 1);
    d.command(OPERATOR, "workflow.run", json!({"workflow": "reader"}));
    d.wait_for("\"step\":\"w\"");
    d.child.stop();
    let d = Peered::start(t.path(), "", &workflows, turns, true);
    d.command(OPERATOR, "workflow.signal", json!({"name": "go"}));
    d.wait_until(|log| {
        log.lines()
            .any(|l| l.contains("\"event\":\"run.done\"") && l.contains("\"workflow\":\"reader\""))
    });
    let ran = d.ask();
    assert!(
        ran.events("run.done")
            .iter()
            .any(|e| e["workflow"] == "reader" && e["output"].to_string().contains(RUN_SECRET)),
        "the reader finishes after the restart with what it read:\n{}",
        ran.log
    );
    assert!(peer_marker(&ran.answer()), "{}\n{}", ran.answer(), ran.log);
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
