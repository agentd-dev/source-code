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

/// `ask` with no interface attached cannot be answered, and an unanswered gate
/// has not been approved — so it denies rather than quietly running the call.
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
