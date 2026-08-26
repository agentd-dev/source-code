// SPDX-License-Identifier: AGPL-3.0-only
//! The `message` node and `message.send` tool: a run delivering into one of
//! this instance's own conversations, which is the only way anything inside
//! the process can cause a turn.
//!
//! The cases that matter are the delivery itself, the hop cap that keeps
//! message → turn → run → message from re-arming forever, and the two
//! refusals that make the cap unavoidable (a turn messaging itself, and a
//! chain routed through a workflow).
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

/// Run one config to completion and return (exit code, stderr log).
fn run(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("msg", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text.replace("__STATE__", &format!("{dir}/state"))).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

/// Run a config in the DAEMON shape (`run_until: drained` never exits on its
/// own), let it settle, then terminate it and return its log. Some behaviour
/// only exists outside the job shape — the workflow-finished notification is
/// deliberately suppressed for one-shot jobs — so those cases cannot be
/// observed with the run-to-completion helper above.
fn run_daemon(cfg_text: &str, settle: std::time::Duration) -> String {
    let dir = common::unique_path("msgd", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    let errf = format!("{dir}/err.log");
    std::fs::write(&cfg, cfg_text.replace("__STATE__", &format!("{dir}/state"))).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&errf).unwrap()))
        .spawn()
        .expect("spawn");
    std::thread::sleep(settle);
    let _ = child.kill();
    let _ = child.wait();
    let log = std::fs::read_to_string(&errf).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    log
}

const BASE: &str = "config_version: \"1\"\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info, log_content: true }\n";

/// The plain case: a workflow step delivers, and the delivery becomes a real
/// turn — not a note appended for someone else to find later.
#[test]
fn a_message_step_delivers_into_a_conversation_and_starts_a_turn() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: m }}\n\
         intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 2s }}\n\
         workflows:\n\
        \x20 - name: greet\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: message, to: root, text: \"look at this\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"kind\":\"a2a_message\""),
        "the step should have produced a message event\n{log}"
    );
    // The delivery has to reach the agent loop, not just the inbox.
    assert!(
        log.contains("\"event\":\"turn.done\""),
        "the delivered message should have caused a turn\n{log}"
    );
}

/// `to: new` opens a fresh conversation rather than borrowing the operator's,
/// so a run can think somewhere without polluting the root transcript.
#[test]
fn to_new_opens_its_own_conversation() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: m }}\n\
         intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 2s }}\n\
         workflows:\n\
        \x20 - name: aside\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: message, to: new, text: \"a private aside\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"event\":\"turn.done\"") && !log.contains("\"ctx\":\"root\""),
        "the turn should have run in a generated context, not root\n{log}"
    );
}

/// The fail-closed gate. `wf-once` starts one workflow per turn and
/// that workflow messages back, so the chain would re-arm forever. Each hop
/// inherits the last one's depth, so the cap bites and the run FAILS loudly
/// rather than the daemon quietly spinning.
#[test]
fn a_message_loop_is_refused_once_it_runs_out_of_depth() {
    let (code, log) = run(&format!(
        "{BASE}intelligence: {{ endpoints: \"mock:wf-once\", model: mock }}\n\
         agent: {{ name: m, prompt: \"start the loop\" }}\n\
         limits: {{ max_message_depth: 1, run: {{ steps: 4 }} }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 3s }}\n\
         workflows:\n\
        \x20 - name: loop\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: message, to: root, text: \"again\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert!(
        log.contains("\"event\":\"message.too_deep\""),
        "the hop cap should have refused the chain\n{log}"
    );
    // And it refuses rather than degrading: the step fails, so the operator
    // sees a broken workflow instead of a daemon that looks healthy.
    assert!(
        log.contains("max_message_depth"),
        "the refusal should name the limit that stopped it\n{log}"
    );
    // The refusal fails the step, which fails the run — so the daemon exits 1
    // (a run failed), not 0. That is the point: a loop that was silently
    // spinning is now a visibly broken workflow.
    assert_eq!(code, Some(1), "{log}");
    assert!(
        !log.contains("panicked"),
        "the refusal must be an ordinary step failure\n{log}"
    );
}

/// The chain climbs one hop per cycle, which is what makes the cap a bound on
/// recursion rather than on volume: run → message(1) → turn(1) → run(1) →
/// message(2). The refusal names the hop it stopped at.
#[test]
fn each_hop_inherits_the_last_ones_depth() {
    let (_code, log) = run(&format!(
        "{BASE}intelligence: {{ endpoints: \"mock:wf-once\", model: mock }}\n\
         agent: {{ name: m, prompt: \"start the loop\" }}\n\
         limits: {{ max_message_depth: 1, run: {{ steps: 4 }} }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 3s }}\n\
         workflows:\n\
        \x20 - name: loop\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: message, to: root, text: \"again\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    // Depth 2 refused against a cap of 1: the second hop knew about the first.
    assert!(
        log.contains("\"depth\":2") && log.contains("\"max\":1"),
        "the second hop should have inherited the first hop's depth\n{log}"
    );
}

/// The hop cap has to survive every READER a delivered message can take, not
/// just the turn. A message that matches an `a2a` start fires a run, and that
/// run can message again — so if the depth reset at that hop the chain would
/// be unbounded through a path the cap never sees.
#[test]
fn the_depth_survives_a_delivery_that_fires_a_workflow() {
    let (_code, log) = run(&format!(
        "{BASE}agent: {{ name: m }}\n\
         intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         limits: {{ max_message_depth: 2 }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 4s }}\n\
         a2a: {{ }}\n\
         workflows:\n\
        \x20 - name: relay\n    steps:\n\
        \x20     s: {{ kind: a2a }}\n\
        \x20     m: {{ kind: message, to: relayed, text: \"round\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n\
        \x20 - name: kick\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: message, to: relayed, text: \"start\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    // The chain really does route through the `a2a` reader...
    assert!(
        log.contains("\"event\":\"start.a2a.fired\""),
        "the delivery should have fired the a2a start\n{log}"
    );
    // ...and the depth kept counting across it. Depth 3 against a cap of 2 is
    // the proof: a reset at that hop would have looped at depth 1 forever.
    assert!(
        log.contains("\"event\":\"message.too_deep\"") && log.contains("\"depth\":3"),
        "the hop depth must survive the a2a reader\n{log}"
    );
}

/// A message with neither text nor parts is a configuration mistake, and the
/// step says so instead of delivering an empty turn.
#[test]
fn an_empty_message_is_refused() {
    let (_code, log) = run(&format!(
        "{BASE}agent: {{ name: m }}\n\
         intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
         workflows:\n\
        \x20 - name: empty\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: message, to: root, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert!(
        log.contains("one of text or parts is required"),
        "an empty message should be refused by the step\n{log}"
    );
}

/// `on_workflow_finished: think` used to be byte-identical to `note` — both
/// appended a line and waited for a human to happen by. `think` now delivers,
/// which is the whole difference between leaving a message and making the call.
#[test]
fn on_workflow_finished_think_starts_a_turn_where_note_only_appends() {
    let wf = "workflows:\n\
        \x20 - name: job\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     f: { kind: finish, depends_on: [s], status: completed, output: done }\n";
    // The daemon shape, not the job shape: a one-shot job deliberately skips
    // the whole notification path, so `idle` would prove nothing either way.
    let settle = std::time::Duration::from_secs(3);
    let note_log = run_daemon(
        &format!(
            "{BASE}intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         agent: {{ name: m, on_workflow_finished: note, wake_on: [workflow_finished] }}\n\
         lifecycle: {{ run_until: drained }}\n{wf}"
        ),
        settle,
    );
    let think_log = run_daemon(
        &format!(
            "{BASE}intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         agent: {{ name: m, on_workflow_finished: think, wake_on: [workflow_finished] }}\n\
         lifecycle: {{ run_until: drained }}\n{wf}"
        ),
        settle,
    );
    assert!(
        !note_log.contains("\"event\":\"turn.done\""),
        "note should append without waking the agent\n{note_log}"
    );
    assert!(
        think_log.contains("\"event\":\"turn.done\""),
        "think should actually cause a turn\n{think_log}"
    );
}
