// SPDX-License-Identifier: AGPL-3.0-only
//! `security.policies`: an operator's verdict on the tool call itself.
//!
//! Grants are name patterns, so an argument has never been judgeable; this is
//! the layer that can. The cases that matter are the argument guard, the
//! shadow verdict refusing rather than fabricating, and — above all — that a
//! SUBAGENT is covered. A policy table that held for root turns but not for
//! subagent turns would be worse than none, because the operator would believe
//! they were covered.
#![cfg(all(unix, feature = "workflow", feature = "cel"))]

mod common;

use std::process::{Command, Stdio};

fn run(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("pol", "d");
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

const BASE: &str = "config_version: \"1\"\nagent: { name: p }\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info, log_content: true }\n\
     lifecycle: { run_until: idle, idle_grace: 2s }\n";

/// A workflow step calling a denied tool is refused, and the refusal names the
/// rule so an operator can act on it.
#[test]
fn a_denied_tool_is_refused_and_the_rule_is_named() {
    let (_code, log) = run(&format!(
        "{BASE}security:\n  policies:\n\
        \x20   - match: {{ tool: \"memory.set\" }}\n      action: deny\n\
         workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert!(
        log.contains("denied by security.policies[0]"),
        "the call should have been denied, naming the rule\n{log}"
    );
    assert!(
        log.contains("\"event\":\"tool.policy.refused\""),
        "the refusal should be visible as an event\n{log}"
    );
}

/// The reason the layer exists. A grant is a name pattern, so it cannot say
/// "this key but not that one". An argument guard can.
#[test]
fn an_argument_guard_judges_the_arguments_a_grant_cannot_see() {
    let cfg = |key: &str| {
        format!(
            "{BASE}security:\n  policies:\n\
        \x20   - match: {{ tool: \"memory.set\", args: \"CEL: args.key.startsWith('secret_')\" }}\n      action: deny\n\
         workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: memory.set, key: {key}, value: v, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
        )
    };
    let (_c1, blocked) = run(&cfg("secret_token"));
    assert!(
        blocked.contains("denied by security.policies[0]"),
        "the guarded key should be refused\n{blocked}"
    );
    let (c2, allowed) = run(&cfg("ordinary"));
    assert_eq!(c2, Some(0), "{allowed}");
    assert!(
        !allowed.contains("denied by security.policies"),
        "the same tool with a different argument should pass\n{allowed}"
    );
}

/// `shadow` must never fabricate a result. A schema-conformant fake is
/// reasoned over as real, and every later decision is then built on an
/// observation that never happened.
#[test]
fn shadow_says_the_call_was_held_and_returns_no_result() {
    let (_code, log) = run(&format!(
        "{BASE}security:\n  policies:\n\
        \x20   - match: {{ tool: \"memory.set\" }}\n      action: shadow\n\
         workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert!(
        log.contains("was NOT executed") && log.contains("do not assume an outcome"),
        "shadow should say plainly that nothing ran\n{log}"
    );
    assert!(
        log.contains("\"action\":\"shadow\""),
        "the held call should be auditable as shadow\n{log}"
    );
}

/// First match wins, so a narrow allow can precede a broad deny — otherwise
/// every exception would need to be encoded as a negation.
#[test]
fn an_explicit_allow_can_precede_a_broad_deny() {
    let (code, log) = run(&format!(
        "{BASE}security:\n  policies:\n\
        \x20   - match: {{ tool: \"memory.set\" }}\n      action: allow\n\
        \x20   - match: {{ tool: \"memory.*\" }}\n      action: deny\n\
         workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !log.contains("denied by security.policies"),
        "the earlier allow should win\n{log}"
    );
}

/// `caller` narrows. A rule scoped to subagents must not touch the workflow
/// steps running beside them.
#[test]
fn a_subagent_scoped_rule_leaves_workflow_calls_alone() {
    let (code, log) = run(&format!(
        "{BASE}security:\n  policies:\n\
        \x20   - match: {{ tool: \"memory.*\", caller: [subagent] }}\n      action: deny\n\
         workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !log.contains("denied by security.policies"),
        "a subagent-scoped rule should not apply to a workflow step\n{log}"
    );
}

/// A security control must fail loudly when it cannot do what it says. An
/// argument guard on a build without CEL would silently evaluate to no-match,
/// turning a deny into an allow at the moment it was meant to bite.
#[test]
fn an_uncompilable_argument_guard_is_refused_at_startup() {
    let (code, log) = run(&format!(
        "{BASE}security:\n  policies:\n\
        \x20   - match: {{ tool: \"*\", args: \"CEL: ((( not an expression\" }}\n      action: deny\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("match.args"),
        "the refusal should point at the guard\n{log}"
    );
}

/// `ask` with no A2A listener cannot be answered — nobody can be asked — and
/// an unanswered gate has not been approved, so it denies rather than quietly
/// running the call.
#[test]
fn an_unanswerable_gate_denies_rather_than_passing() {
    let (_code, log) = run(&format!(
        "{BASE}security:\n  policies:\n\
        \x20   - match: {{ tool: \"memory.set\" }}\n      action: ask\n      question: \"allow {{{{tool}}}}?\"\n\
         workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
    ));
    assert!(
        log.contains("\"event\":\"tool.policy.unanswerable\""),
        "an unanswerable gate should say so\n{log}"
    );
    assert!(
        log.contains("a person had to approve this call"),
        "and it should deny, not pass\n{log}"
    );
}

/// Nonsense in the rule shape is a startup error, not a rule that quietly
/// does nothing.
#[test]
fn a_gate_that_would_ask_forever_is_refused() {
    let (code, log) = run(&format!(
        "{BASE}security:\n  policies:\n\
        \x20   - match: {{ tool: \"*\" }}\n      action: ask\n      on_timeout: ask\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("ask again forever"), "{log}");
}

/// A daemon whose model calls `memory.set` (gated by an `ask` rule that names
/// nobody), then reads the key back with `memory.get` and says what it found:
/// `PRESENT` or `MISSING`. The final answer is how a test sees whether the
/// gated call actually RAN — a gate that returned the approver's words in its
/// place would leave the key missing while the turn read as a success.
#[cfg(feature = "a2a")]
struct GatedDaemon {
    _llm: std::process::Child,
    daemon: std::process::Child,
    addr: String,
    log_path: String,
    dir: String,
    playbook: String,
}

#[cfg(feature = "a2a")]
impl Drop for GatedDaemon {
    fn drop(&mut self) {
        for c in [&mut self._llm, &mut self.daemon] {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_file(&self.playbook);
    }
}

#[cfg(feature = "a2a")]
const POLICY_OPERATOR: &str = "policy-e2e-operator-bearer";
#[cfg(feature = "a2a")]
const POLICY_ALICE: &str = "policy-e2e-alice-bearer";

#[cfg(feature = "a2a")]
impl GatedDaemon {
    fn start() -> GatedDaemon {
        GatedDaemon::start_with("")
    }

    /// [`GatedDaemon::start`], with `extra` YAML appended to the config.
    fn start_with(extra: &str) -> GatedDaemon {
        use serde_json::json;
        use std::time::Duration;

        let playbook = common::unique_path("pol-playbook", "json");
        std::fs::write(
            &playbook,
            json!({
                "turns": [
                    {"tool_calls": [{"name": "memory.set", "arguments": {"key": "k", "value": 1}}]},
                    {"tool_calls": [{"name": "memory.get", "arguments": {"key": "k"}}]},
                    {"content": "unreachable"}
                ],
                "match": [
                    {"when_contains": "found\\\":true", "content": "PRESENT"},
                    {"when_contains": "found\\\":false", "content": "MISSING"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let llm_addr = common::unique_path("pol-mock-llm", "addr");
        let llm_child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args([
                "--internal-mock-llm",
                &llm_addr,
                &format!("file:{playbook}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mock llm");
        let llm = format!("http://{}", common::read_addr_file(&llm_addr));

        let dir = common::unique_path("pol-gate", "d");
        std::fs::create_dir_all(&dir).unwrap();
        let mut llm_child = Some(llm_child);
        for _ in 0..5 {
            let cfg = format!("{dir}/c.yaml");
            std::fs::write(
                &cfg,
                format!(
                    "config_version: \"1\"\n\
                     agent: {{ name: p, instruction: You save things., preflight: never }}\n\
                     intelligence: {{ endpoints: {llm}, model: mock }}\n\
                     store: {{ kind: memory }}\n\
                     observability: {{ log_level: info }}\n\
                     lifecycle: {{ run_until: drained }}\n\
                     a2a:\n  listen: http://127.0.0.1:{port}\n\
                     \x20 bearer: \"{{{{secret:AGENTD_POLICY_OPERATOR}}}}\"\n\
                     \x20 principals:\n\
                     \x20   - id: alice\n\
                     \x20     match: {{ bearer_ref: \"{{{{secret:AGENTD_POLICY_ALICE}}}}\" }}\n\
                     \x20     role: user\n\
                     security:\n  policies:\n\
                     \x20   - match: {{ tool: \"memory.set\" }}\n      action: ask\n      question: \"may {{{{caller}}}} write memory?\"\n\
                     {extra}",
                    port = common::free_port()
                ),
            )
            .unwrap();
            let log_path = common::unique_path("pol-gate", "log");
            let mut daemon = Command::new(env!("CARGO_BIN_EXE_agentd"))
                .args(["--config", &cfg])
                .env("AGENTD_POLICY_OPERATOR", POLICY_OPERATOR)
                .env("AGENTD_POLICY_ALICE", POLICY_ALICE)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log_path).unwrap())
                .spawn()
                .expect("spawn agentd");
            match common::try_a2a_bound(&log_path, Duration::from_secs(15)) {
                Some(addr) => {
                    return GatedDaemon {
                        _llm: llm_child.take().unwrap(),
                        daemon,
                        addr,
                        log_path,
                        dir,
                        playbook,
                    };
                }
                None => {
                    let _ = daemon.kill();
                    let _ = daemon.wait();
                }
            }
        }
        panic!("the daemon never bound an A2A listener (5 attempts)");
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    fn task(&self, id: &str) -> serde_json::Value {
        common::rpc_as(
            &self.addr,
            POLICY_ALICE,
            9,
            "GetTask",
            serde_json::json!({"id": id}),
        )["result"]
            .clone()
    }

    fn wait_state(&self, id: &str, want: &str) -> serde_json::Value {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let t = self.task(id);
            if t["status"]["state"] == want {
                return t;
            }
            assert!(
                Instant::now() < deadline,
                "never reached {want}: {t}\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(80));
        }
    }

    /// Alice's message makes the model call the gated tool: her task gates.
    fn gate(&self) -> String {
        let sent = common::SendMessage::text("save k")
            .bearer(POLICY_ALICE)
            .return_immediately()
            .result(&self.addr);
        let task = sent["task"]["id"].as_str().expect("task id").to_string();
        self.wait_state(&task, "TASK_STATE_INPUT_REQUIRED");
        task
    }

    fn answer(&self, task: &str, bearer: &str, text: &str) -> serde_json::Value {
        common::SendMessage::text(text)
            .bearer(bearer)
            .task(task)
            .return_immediately()
            .post(&self.addr)
    }
}

/// The artifact text a finished task carries.
#[cfg(feature = "a2a")]
fn said(task: &serde_json::Value) -> String {
    task["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// An `ask` rule that names nobody is addressed to the operator. The gate
/// lands on the task of the very caller whose call is being judged, and
/// whoever holds a task may answer an unaddressed gate on it — so without the
/// default the requesting peer would approve its own gated call. It is told
/// why and the gate stays open; the operator's answer is what closes it.
///
/// And the answer is a DECISION about the call, in a declared shape: "yes"
/// is asked again rather than guessed at, and `approve` runs the call — the
/// model reads back the key the gated `memory.set` wrote.
#[cfg(feature = "a2a")]
#[test]
fn a_policy_gate_is_answered_by_an_operator_not_by_the_caller() {
    let d = GatedDaemon::start();
    let task = d.gate();

    // Alice holds the task, and still cannot approve her own gated call.
    let own = d.answer(&task, POLICY_ALICE, "approve");
    assert_eq!(
        own["error"]["code"],
        -32602,
        "the caller being judged must not approve its own call: {own}\n{}",
        d.log()
    );
    assert!(
        own["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("role operator"),
        "and is told whose decision it is: {own}"
    );
    let open = d.task(&task);
    assert_eq!(
        open["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "the gate stays open"
    );
    assert!(
        open["status"]["message"].to_string().contains("approve"),
        "the question says how to answer: {open}"
    );

    // An answer that is not a decision is asked again, not read as one.
    let vague = d.answer(&task, POLICY_OPERATOR, "yes");
    assert!(vague.get("error").is_none(), "{vague}");
    let reasked = d.task(&task);
    assert_eq!(
        reasked["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{reasked}"
    );
    assert!(
        reasked["status"]["message"]
            .to_string()
            .contains("previous answer rejected"),
        "{reasked}"
    );

    // The operator's approval closes it, as the addressee — not as an
    // override — and the call RUNS: the model finds the key it wrote.
    let op = d.answer(&task, POLICY_OPERATOR, "approve");
    assert!(op.get("error").is_none(), "the operator answers: {op}");
    let done = d.wait_state(&task, "TASK_STATE_COMPLETED");
    assert_eq!(
        said(&done),
        "PRESENT",
        "the approved call never ran: {done}\n{}",
        d.log()
    );
    let log = d.log();
    assert!(
        log.contains("\"event\":\"human.answer.not_addressed\""),
        "{log}"
    );
    assert!(log.contains("\"event\":\"tool.policy.decided\""), "{log}");
    assert!(!log.contains("operator_override"), "{log}");
}

/// A workflow step's gated call runs on approval too — as the step's own
/// work, so the steps after it see what it did. The step is suspended on the
/// gate, and it is the approval, not the approver's words, that finishes it.
#[cfg(feature = "a2a")]
#[test]
fn an_approved_policy_gate_runs_a_workflow_steps_call() {
    use serde_json::json;
    let d = GatedDaemon::start_with(
        "workflows:\n\
         \x20 - name: w\n    steps:\n\
         \x20     s: { kind: manual }\n\
         \x20     m: { kind: memory.set, key: k, value: v, depends_on: [s] }\n\
         \x20     g: { kind: memory.get, key: k, depends_on: [m] }\n\
         \x20     f: { kind: finish, depends_on: [g], output: \"found={{ steps.g.output.found }}\" }\n",
    );
    let started = common::SendMessage::command("workflow.run", json!({"workflow": "w"}))
        .bearer(POLICY_OPERATOR)
        .result(&d.addr);
    let task = started["task"]["id"].as_str().expect("task id").to_string();
    let wait = |want: &str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let t = common::rpc_as(&d.addr, POLICY_OPERATOR, 9, "GetTask", json!({"id": task}))
                ["result"]
                .clone();
            if t["status"]["state"] == want {
                return t;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never reached {want}: {t}\n{}",
                d.log()
            );
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
    };
    wait("TASK_STATE_INPUT_REQUIRED");
    let op = d.answer(&task, POLICY_OPERATOR, "approve");
    assert!(op.get("error").is_none(), "the operator answers: {op}");
    let done = wait("TASK_STATE_COMPLETED");
    assert!(
        done.to_string().contains("found=true"),
        "the approved step's call never ran: {done}\n{}",
        d.log()
    );
}

/// A refusal is an error for the call, and the call does not run: the model
/// is told it was denied and finds nothing written.
#[cfg(feature = "a2a")]
#[test]
fn a_denied_policy_gate_refuses_the_call_it_holds() {
    let d = GatedDaemon::start();
    let task = d.gate();
    let op = d.answer(&task, POLICY_OPERATOR, "deny");
    assert!(op.get("error").is_none(), "the operator answers: {op}");
    let done = d.wait_state(&task, "TASK_STATE_COMPLETED");
    assert_eq!(
        said(&done),
        "MISSING",
        "the denied call ran: {done}\n{}",
        d.log()
    );
    let log = d.log();
    assert!(
        log.lines()
            .any(|l| l.contains("\"event\":\"tool.policy.decided\"")
                && l.contains("\"approved\":false")),
        "{log}"
    );
}

/// A policy gate nobody answers in time is NOT handed to the `auto` judge,
/// whatever `ask_human_fallback` says: it is addressed to the operator, and a
/// model judge approving it would be the agent approving the operator's own
/// security gate by waiting it out. It times out and the call is refused.
#[cfg(feature = "a2a")]
#[test]
fn an_unanswered_policy_gate_is_never_approved_by_the_auto_judge() {
    use common::{SendMessage, rpc_result};
    use serde_json::json;
    use std::time::{Duration, Instant};

    struct Proc(std::process::Child);
    impl Drop for Proc {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let pb = common::unique_path("pol-judge-playbook", "json");
    std::fs::write(
        &pb,
        json!({
            "turns": [
                {"tool_calls": [{"name": "memory.set", "arguments": {"key": "k", "value": 1}}]},
                {"content": "Done."}
            ],
            "match": [
                {"when_contains": "answering ON BEHALF OF the unavailable human operator", "content": "yes"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let llm_addr = common::unique_path("pol-judge-llm", "addr");
    let _llm = Proc(
        Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--internal-mock-llm", &llm_addr, &format!("file:{pb}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mock llm"),
    );
    let llm = format!("http://{}", common::read_addr_file(&llm_addr));

    let dir = common::unique_path("pol-judge", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let (daemon, addr, log_path) = (0..5)
        .find_map(|_| {
            let cfg = format!("{dir}/c.yaml");
            std::fs::write(
                &cfg,
                format!(
                    "config_version: \"1\"\n\
                     agent: {{ name: p, instruction: You save things., preflight: never, ask_human_fallback: auto }}\n\
                     intelligence: {{ endpoints: {llm}, model: mock }}\n\
                     store: {{ kind: memory }}\n\
                     observability: {{ log_level: info }}\n\
                     lifecycle: {{ run_until: drained }}\n\
                     a2a: {{ listen: \"http://127.0.0.1:{port}\" }}\n\
                     security:\n  policies:\n\
                     \x20   - match: {{ tool: \"memory.set\" }}\n      action: ask\n      timeout: 1s\n",
                    port = common::free_port()
                ),
            )
            .unwrap();
            let log_path = common::unique_path("pol-judge", "log");
            let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
                .args(["--config", &cfg])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log_path).unwrap())
                .spawn()
                .expect("spawn agentd");
            let daemon = Proc(child);
            common::try_a2a_bound(&log_path, Duration::from_secs(15))
                .map(|addr| (daemon, addr, log_path))
        })
        .expect("the daemon never bound an A2A listener (5 attempts)");

    let sent = SendMessage::text("save k")
        .return_immediately()
        .result(&addr);
    let task = sent["task"]["id"].as_str().expect("task id").to_string();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let t = rpc_result(&addr, 9, "GetTask", json!({"id": task}));
        if t["status"]["state"] == "TASK_STATE_COMPLETED" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the turn never finished: {t}\n{}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(80));
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("\"event\":\"tool.policy.ask\""),
        "the gate opened: {log}"
    );
    assert!(
        !log.contains("human.judge.start"),
        "the auto judge must not answer an operator-addressed gate\n{log}"
    );
    assert!(
        log.contains("no answer within the timeout"),
        "the gate timed out and the call was refused\n{log}"
    );
    drop(daemon);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&pb);
}
