// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0036 end to end: subagent templates — flat-tier instantiation with
//! schema-checked params, the freeform switch, and the instance tier (a
//! template whose instruction defines machinery spawning a FULL child daemon:
//! typed A2A commands answered by the child's own workflow over its unix
//! socket, ttl retirement, singleton refusal) — plus the boot-time refusal of
//! template machinery that tries to define listeners.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

use serde_json::Value;

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

fn run_cfg(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("tpl", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env("AGENTD_STATE_DIR", format!("{dir}/state"))
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

#[test]
fn a_flat_template_spawns_with_folded_params_and_the_template_grant() {
    // The worker's whole brief comes from the template — the call site names
    // it and fills the declared hole; `instruction` at the call site would be
    // refused (mutual exclusion is validated at workflow load).
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: t }\nstore: { kind: memory }\n\
         intelligence: { endpoints: \"mock:final\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 900ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   researcher:\n\
        \x20     instruction: \"Research {{params.topic}} and reply with one line.\"\n\
        \x20     params: { topic: { type: string, required: true } }\n\
        \x20     mode: sync\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:   { kind: once }\n\
        \x20     sub: { kind: subagent, depends_on: [s], template: researcher, params: { topic: rust } }\n\
        \x20     f:   { kind: finish, depends_on: [sub], status: completed, output: \"{{steps.sub.output.status}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["status"], "completed", "{log}");
    assert_eq!(
        done[0]["output"], "completed",
        "the flat template child ran to completion:\n{log}"
    );
    assert!(
        !events(&log, "subagent.spawn").is_empty(),
        "a flat spawn went through the normal chokepoint:\n{log}"
    );
}

#[test]
fn freeform_spawns_are_refused_when_the_operator_disables_them() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: t }\nstore: { kind: memory }\n\
         intelligence: { endpoints: \"mock:final\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 700ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents: { allow_freeform: false }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:   { kind: once }\n\
        \x20     sub: { kind: subagent, depends_on: [s], instruction: \"do a thing\", on_error: continue }\n\
        \x20     f:   { kind: finish, depends_on: [sub], status: completed, output: \"{{steps.sub.error | ok}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert!(
        done[0]["output"]
            .as_str()
            .unwrap_or("")
            .contains("allow_freeform"),
        "the refusal names the switch: {done:?}\n{log}"
    );
}

#[test]
fn template_params_are_schema_checked_at_the_chokepoint() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: t }\nstore: { kind: memory }\n\
         intelligence: { endpoints: \"mock:final\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 700ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   triage:\n\
        \x20     instruction: \"Triage at severity {{params.sev}}.\"\n\
        \x20     params: { sev: { type: string, enum: [low, high] } }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:   { kind: once }\n\
        \x20     bad: { kind: subagent, depends_on: [s], template: triage, params: { sev: mid }, on_error: continue }\n\
        \x20     f:   { kind: finish, depends_on: [bad], status: completed, output: \"{{steps.bad.error | ok}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    let msg = done[0]["output"].as_str().unwrap_or("");
    assert!(
        msg.contains("one of") && msg.contains("sev"),
        "the mismatch names the param and the allowed set: {msg}\n{log}"
    );
}

#[cfg(feature = "a2a")]
#[test]
fn an_instance_template_boots_answers_typed_commands_and_retires_on_ttl() {
    // The full RFC 0036 §6 arc in one daemon: a template whose machinery is a
    // typed-command workflow spawns a CHILD DAEMON; the parent delegates to it
    // by handle over the auto-wired unix socket; the ttl retires it through
    // the child's own graceful drain.
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: parent }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 1500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   room:\n\
        \x20     instruction: |\n\
        \x20       You are the room for {{params.id}}.\n\
        \x20       :::workflow\n\
        \x20       name: on-ping\n\
        \x20       version: 3\n\
        \x20       steps:\n\
        \x20         cmd: { kind: a2a, command: room.ping, roles: [agent, operator] }\n\
        \x20         f:   { kind: finish, depends_on: [cmd], status: completed, output: \"pong {{params.id}}/{{steps.cmd.output.args.x}}\" }\n\
        \x20       :::\n\
        \x20     params: { id: { type: string, required: true } }\n\
        \x20     ttl: 4s\n\
         workflows:\n  - name: caller\n    steps:\n\
        \x20     s:     { kind: once }\n\
        \x20     spawn: { kind: subagent, template: room, params: { id: inc-7 }, depends_on: [s] }\n\
        \x20     ask:   { kind: a2a.delegate, depends_on: [spawn], peer: \"{{steps.spawn.output.peer}}\", command: room.ping, args: { x: hello }, timeout: 30s, retry: { max: 6, backoff: 1s } }\n\
        \x20     nap:   { kind: sleep, depends_on: [ask], duration: 5s }\n\
        \x20     f:     { kind: finish, depends_on: [nap], status: completed, output: \"{{steps.ask.output}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !events(&log, "instance.spawn").is_empty(),
        "the instance child spawned:\n{log}"
    );
    let done = events(&log, "run.done");
    let caller: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "caller").collect();
    assert_eq!(caller.len(), 1, "{log}");
    assert_eq!(caller[0]["status"], "completed", "{log}");
    let out = caller[0]["output"].as_str().unwrap_or("");
    assert!(
        out.contains("pong inc-7/hello"),
        "the child's OWN workflow answered the typed command (params folded into its machinery): {out}\n{log}"
    );
    assert!(
        !events(&log, "instance.retire").is_empty(),
        "the ttl began graceful retirement:\n{log}"
    );
    let exited = events(&log, "instance.exited");
    assert!(
        exited.iter().any(|e| e["status"] == "retired"),
        "the child drained and exited as retired:\n{log}"
    );
}

#[cfg(feature = "a2a")]
#[test]
fn a_singleton_instance_refuses_a_second_live_spawn() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: parent }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 1200ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   board:\n\
        \x20     instruction: |\n\
        \x20       The one board.\n\
        \x20       :::workflow\n\
        \x20       name: on-ask\n\
        \x20       version: 3\n\
        \x20       steps:\n\
        \x20         cmd: { kind: a2a, command: board.ask, roles: [agent, operator] }\n\
        \x20         f:   { kind: finish, depends_on: [cmd], status: completed, output: ok }\n\
        \x20       :::\n\
        \x20     singleton: true\n\
        \x20     ttl: 30s\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:  { kind: once }\n\
        \x20     a:  { kind: subagent, template: board, depends_on: [s] }\n\
        \x20     b:  { kind: subagent, template: board, depends_on: [a], on_error: continue }\n\
        \x20     f:  { kind: finish, depends_on: [b], status: completed, output: \"{{steps.b.error | ok}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert!(
        done[0]["output"]
            .as_str()
            .unwrap_or("")
            .contains("singleton"),
        "the second spawn was refused as a singleton violation: {done:?}\n{log}"
    );
}

#[test]
fn template_machinery_may_not_define_listeners_and_fails_the_parents_boot() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: parent }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 500ms }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   sneaky:\n\
        \x20     instruction: |\n\
        \x20       Hi.\n\
        \x20       :::config\n\
        \x20       webhooks: { listen: \"http://127.0.0.1:1\" }\n\
        \x20       :::\n\
        \x20       :::workflow\n\
        \x20       name: w\n\
        \x20       version: 3\n\
        \x20       steps: { s: { kind: once }, f: { kind: finish, depends_on: [s], status: completed } }\n\
        \x20       :::\n",
    );
    assert_eq!(
        code,
        Some(2),
        "a listener-defining template refuses BOOT:\n{log}"
    );
}
