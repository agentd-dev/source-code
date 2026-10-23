// SPDX-License-Identifier: AGPL-3.0-only
//! Subagent templates end to end: flat-tier instantiation with schema-checked
//! params, the freeform switch, and the instance tier (a template whose
//! instruction defines machinery spawning a FULL child daemon: typed A2A
//! commands answered by the child's own workflow over its unix socket, ttl
//! retirement, singleton refusal, a durable child's spend charged to its
//! parent) — plus the boot-time refusal of template machinery that tries to
//! define listeners.
#![cfg(unix)]

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
    let out = run_cfg_in(&format!("{dir}/state"), cfg_text);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// [`run_cfg`] with its state under `state`, which is left for the caller to
/// read (the children's composed configurations are there).
fn run_cfg_in(state: &str, cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("tpl", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env("AGENTD_STATE_DIR", state)
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
        "agent: { name: t }\nstore: { kind: memory }\n\
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
        "agent: { name: t }\nstore: { kind: memory }\n\
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
        "agent: { name: t }\nstore: { kind: memory }\n\
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
    // The full instance-tier arc in one daemon: a template whose machinery is a
    // typed-command workflow spawns a CHILD DAEMON; the parent delegates to it
    // by handle over the auto-wired unix socket; the ttl retires it through
    // the child's own graceful drain.
    let (code, log) = run_cfg(
        "agent: { name: parent }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 1500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   room:\n\
        \x20     instruction: |\n\
        \x20       You are the room for {{params.id}}.\n\
        \x20       :::!workflow{name=on-ping}\n\
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

/// The parent delivers an instance template's instruction once, at boot, and
/// writes that prose, params folded in, as the child's instruction; the child
/// uses it as written. Delivered a second time, the `plain` label would turn
/// bold and the rule fence quoted in the example would become a live rule —
/// so what the child's model receives (echoed back by the mock intelligence)
/// is the template's delivered text, unchanged.
#[cfg(feature = "a2a")]
#[test]
fn an_instance_childs_instruction_is_delivered_once_by_the_parent() {
    let (code, log) = run_cfg(
        "agent: { name: parent }\nstore: { kind: memory }\n\
         intelligence: { endpoints: \"mock:echo-system\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 1500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   scribe:\n\
        \x20     instruction: |\n\
        \x20       ---\n\
        \x20       spec: \"1\"\n\
        \x20       delivery: {labels: plain}\n\
        \x20       ---\n\
        \x20       MUST: answer for {{params.id}}.\n\
        \n\
        \x20       ::::example\n\
        \x20       MUST: is how a rule reads, and so is\n\
        \n\
        \x20       :::must\n\
        \x20       Never promise a refund.\n\
        \x20       :::\n\
        \x20       ::::\n\
        \n\
        \x20       :::!workflow{name=on-ask}\n\
        \x20       steps:\n\
        \x20         cmd: { kind: a2a, command: scribe.ask, roles: [agent, operator] }\n\
        \x20         a:   { kind: agent, depends_on: [cmd], instruction: go }\n\
        \x20         f:   { kind: finish, depends_on: [a], status: completed, output: \"{{steps.a.output}}\" }\n\
        \x20       :::\n\
        \x20     params: { id: { type: string, required: true } }\n\
        \x20     ttl: 4s\n\
         workflows:\n  - name: caller\n    steps:\n\
        \x20     s:     { kind: once }\n\
        \x20     spawn: { kind: subagent, template: scribe, params: { id: inc-7 }, depends_on: [s] }\n\
        \x20     ask:   { kind: a2a.delegate, depends_on: [spawn], peer: \"{{steps.spawn.output.peer}}\", command: scribe.ask, args: {}, timeout: 30s, retry: { max: 6, backoff: 1s } }\n\
        \x20     nap:   { kind: sleep, depends_on: [ask], duration: 5s }\n\
        \x20     f:     { kind: finish, depends_on: [nap], status: completed, output: \"{{steps.ask.output}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    let caller: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "caller").collect();
    assert_eq!(caller.len(), 1, "{log}");
    assert_eq!(caller[0]["status"], "completed", "{log}");
    // The child's model's system prompt, echoed.
    let prompt = caller[0]["output"].as_str().unwrap_or("");
    assert!(
        prompt.contains("MUST: answer for inc-7.\n"),
        "the plain label reached the child's model as delivered:\n{prompt}"
    );
    assert!(
        prompt.contains("EXAMPLE:\nMUST: is how a rule reads, and so is\n\n:::must\nNever promise a refund.\n:::"),
        "the example's quoted rule stayed quoted:\n{prompt}"
    );
    assert!(
        !prompt.contains("**MUST:**"),
        "nothing was delivered a second time:\n{prompt}"
    );
}

/// Delivered text does not always read a second time: a verbatim block
/// quoting `MUST[y]` twice is a duplicate to a second parse, and an example
/// quoting `[[must/x]]` dangles once delivery dropped the rule's name. Such a
/// template compiles at boot, so its spawns go through — the param guard and
/// the parent's check of the composed config judge what the PARAMS added,
/// and deliver nothing a second time. A `${HOME}` it quotes reaches the
/// child's model as written: the child's loader substitutes every string,
/// and an expanded copy would no longer be the text the parent delivered.
#[cfg(feature = "a2a")]
#[test]
fn an_instance_template_whose_delivered_text_does_not_reread_spawns() {
    let (code, log) = run_cfg(
        "agent: { name: parent }\nstore: { kind: memory }\n\
         intelligence: { endpoints: \"mock:echo-system\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 1500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   scribe:\n\
        \x20     instruction: |\n\
        \x20       MUST[x]: answer for {{params.id}} from $${HOME}.\n\
        \n\
        \x20       :::example\n\
        \x20       See [[must/x]].\n\
        \x20       :::\n\
        \n\
        \x20       :::verbatim\n\
        \x20       MUST[y]: a\n\
        \x20       MUST[y]: b\n\
        \x20       :::\n\
        \n\
        \x20       :::!workflow{name=on-ask}\n\
        \x20       steps:\n\
        \x20         cmd: { kind: a2a, command: scribe.ask, roles: [agent, operator] }\n\
        \x20         a:   { kind: agent, depends_on: [cmd], instruction: go }\n\
        \x20         f:   { kind: finish, depends_on: [a], status: completed, output: \"{{steps.a.output}}\" }\n\
        \x20       :::\n\
        \x20     params: { id: { type: string, required: true } }\n\
        \x20     ttl: 4s\n\
         workflows:\n  - name: caller\n    steps:\n\
        \x20     s:     { kind: once }\n\
        \x20     spawn: { kind: subagent, template: scribe, params: { id: inc-7 }, depends_on: [s] }\n\
        \x20     ask:   { kind: a2a.delegate, depends_on: [spawn], peer: \"{{steps.spawn.output.peer}}\", command: scribe.ask, args: {}, timeout: 30s, retry: { max: 6, backoff: 1s } }\n\
        \x20     nap:   { kind: sleep, depends_on: [ask], duration: 5s }\n\
        \x20     f:     { kind: finish, depends_on: [nap], status: completed, output: \"{{steps.ask.output}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    let caller: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "caller").collect();
    assert_eq!(caller.len(), 1, "{log}");
    assert_eq!(caller[0]["status"], "completed", "{log}");
    let prompt = caller[0]["output"].as_str().unwrap_or("");
    for quoted in [
        "**MUST:** answer for inc-7 from ${HOME}.\n",
        "See [[must/x]].",
        "MUST[y]: a\nMUST[y]: b",
    ] {
        assert!(
            prompt.contains(quoted),
            "{quoted:?} did not reach the child's model as delivered:\n{prompt}\n{log}"
        );
    }
}

/// A template whose machinery is configuration alone, with no workflow,
/// spawns a child daemon like any other instance, and a daemon's
/// instruction is its policy: the child runs nothing when it starts, and a
/// message it is sent is answered under its prose.
#[cfg(feature = "a2a")]
#[test]
fn a_config_only_instance_child_runs_nothing_until_it_is_asked() {
    let state = common::unique_path("tpl-policy", "state");
    let (code, log) = run_cfg_in(
        &state,
        "agent: { name: parent }\nstore: { kind: file }\n\
         intelligence: { endpoints: \"mock:echo-system\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 1500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   scout:\n\
        \x20     instruction: |\n\
        \x20       You scout {{params.topic}} for the desk.\n\
        \x20       :::!config\n\
        \x20       agent: { preflight: never }\n\
        \x20       :::\n\
        \x20     params: { topic: { type: string, required: true } }\n\
        \x20     ttl: 4s\n\
         workflows:\n  - name: caller\n    steps:\n\
        \x20     s:     { kind: once }\n\
        \x20     spawn: { kind: subagent, template: scout, params: { topic: tides }, depends_on: [s] }\n\
        \x20     nap:   { kind: sleep, depends_on: [spawn], duration: 1500ms }\n\
        \x20     ask:   { kind: a2a.delegate, depends_on: [nap], peer: \"{{steps.spawn.output.peer}}\", objective: \"What do you watch?\", timeout: 30s, retry: { max: 6, backoff: 1s } }\n\
        \x20     f:     { kind: finish, depends_on: [ask], status: completed, output: \"{{steps.ask.output}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let spawned = events(&log, "instance.spawn");
    assert_eq!(spawned.len(), 1, "{log}");
    // A durable parent keeps its child's directory, log and all.
    let child = std::fs::read_to_string(format!(
        "{state}/subagents/file/agentd/parent/{}/log",
        spawned[0]["handle"].as_str().unwrap()
    ))
    .unwrap_or_default();
    assert!(!events(&child, "proc.ready").is_empty(), "{child}");
    assert!(
        events(&child, "run.start").is_empty(),
        "the child ran its prose as a task:\n{child}"
    );
    let done: Vec<Value> = events(&log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "caller")
        .collect();
    assert_eq!(done.len(), 1, "{log}");
    // The child's model's system prompt, echoed: the prose, params folded.
    assert!(
        done[0]["output"]
            .as_str()
            .unwrap_or("")
            .contains("You scout tides for the desk."),
        "{log}\n{child}"
    );
    let _ = std::fs::remove_dir_all(&state);
}

#[cfg(feature = "a2a")]
#[test]
fn a_singleton_instance_refuses_a_second_live_spawn() {
    let (code, log) = run_cfg(
        "agent: { name: parent }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 1200ms }\n\
         observability: { log_level: info, log_content: true }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   board:\n\
        \x20     instruction: |\n\
        \x20       The one board.\n\
        \x20       :::!workflow{name=on-ask}\n\
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

/// A template's instruction is an instruction document, so its `human` step
/// addressed to its own `::!human` loads as the agent's own document's does:
/// answered by the operators. Before, the parent accepted the template and
/// every spawn failed — the child was handed the folded channel as its `to`
/// and refused it. The channel travels to the child in its composed
/// `agent.gate_channels`, so the child announces it.
#[cfg(feature = "a2a")]
#[test]
fn a_templates_document_gate_spawns_and_its_channel_reaches_the_child() {
    let state = common::unique_path("tpl-gate", "state");
    let (code, log) = run_cfg_in(
        &state,
        "agent: { name: parent }\nstore: { kind: file }\n\
         lifecycle: { run_until: idle, idle_grace: 2500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         intelligence: { models: { fast: { model: claude-haiku-4-5 } }, roles: { turn: fast } }\n\
         subagents:\n\
        \x20 defaults: { model: gpt-5.1 }\n\
        \x20 templates:\n\
        \x20   approver:\n\
        \x20     params: { team: { type: string } }\n\
        \x20     instruction: |\n\
        \x20       Approve deploys.\n\
        \x20       :::!channel{name=ops}\n\
        \x20       :::\n\
        \x20       :::!human{name=oncall channel=@channel/ops}\n\
        \x20       :::\n\
        \x20       :::!workflow{name=\"approve_{{params.team}}\"}\n\
        \x20       steps:\n\
        \x20         cmd: { kind: a2a, command: deploy.approve, roles: [agent, operator] }\n\
        \x20         ask: { kind: human, question: \"ship it?\", to: \"@human/oncall\", depends_on: [cmd] }\n\
        \x20         f:   { kind: finish, depends_on: [ask], status: completed }\n\
        \x20       :::\n\
        \x20     ttl: 2s\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:     { kind: once }\n\
        \x20     spawn: { kind: subagent, template: approver, params: { team: blue }, depends_on: [s], on_error: continue }\n\
        \x20     f:     { kind: finish, depends_on: [spawn], status: completed, output: \"{{steps.spawn.error | ok}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done: Vec<Value> = events(&log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "w")
        .collect();
    assert_eq!(done.len(), 1, "{log}");
    // `steps.spawn.error` is unset when the spawn succeeded; a refused child
    // puts its reason there.
    assert!(
        done[0]["output"].is_null(),
        "the child accepted the gate its template addressed to its own human:\n{log}"
    );
    assert!(
        !events(&log, "instance.spawn").is_empty(),
        "the instance child spawned:\n{log}"
    );
    // The child's composed configuration carries the channel its parent
    // read off the template's `::!human`. A durable parent keeps it.
    let children = std::fs::read_dir(format!("{state}/subagents/file/agentd/parent")).unwrap();
    let configs: Vec<Value> = children
        .filter_map(|e| std::fs::read_to_string(e.ok()?.path().join("config.json")).ok())
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect();
    assert_eq!(configs.len(), 1, "{log}");
    // Keyed by the workflow's name as the child has it, its param folded.
    assert_eq!(
        configs[0]["agent"]["gate_channels"]["approve_blue"]["ask"], "@channel/ops",
        "{}",
        configs[0]
    );
    // The child runs the model its variants were folded for — the spawn
    // default here — whatever the parent's `roles.turn` says, said once.
    assert_eq!(
        configs[0]["intelligence"]["model"], "gpt-5.1",
        "{}",
        configs[0]
    );
    assert!(
        configs[0]["intelligence"]["roles"].get("turn").is_none(),
        "{}",
        configs[0]
    );
    assert!(
        !log.contains("does not announce the channel"),
        "the parent no longer warns:\n{log}"
    );
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn template_machinery_may_not_define_listeners_and_fails_the_parents_boot() {
    let (code, log) = run_cfg(
        "agent: { name: parent }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 500ms }\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   sneaky:\n\
        \x20     instruction: |\n\
        \x20       Hi.\n\
        \x20       :::!config\n\
        \x20       webhooks: { listen: \"http://127.0.0.1:1\" }\n\
        \x20       :::\n\
        \x20       :::!workflow{name=w}\n\
        \x20       steps: { s: { kind: once }, f: { kind: finish, depends_on: [s], status: completed } }\n\
        \x20       :::\n",
    );
    assert_eq!(
        code,
        Some(2),
        "a listener-defining template refuses BOOT:\n{log}"
    );
}

/// A durable instance child outlives its parent's restart: the restored
/// record respawns it on its composed config, and the respawned child is
/// told, as the first one was, that the parent delivered its instruction —
/// from that config, the one copy of the prose the child reads. Delivered
/// a second time, the `plain` label below would turn bold.
#[cfg(feature = "a2a")]
#[test]
fn a_respawned_instance_child_is_delivered_once_too() {
    use std::time::{Duration, Instant};
    let dir = common::unique_path("tpl-respawn", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let template = "subagents:\n\
        \x20 templates:\n\
        \x20   scribe:\n\
        \x20     instruction: |\n\
        \x20       ---\n\
        \x20       spec: \"1\"\n\
        \x20       delivery: {labels: plain}\n\
        \x20       ---\n\
        \x20       MUST: answer for {{params.id}}.\n\
        \n\
        \x20       :::!workflow{name=on-ask}\n\
        \x20       steps:\n\
        \x20         cmd: { kind: a2a, command: scribe.ask, roles: [agent, operator] }\n\
        \x20         a:   { kind: agent, depends_on: [cmd], instruction: go }\n\
        \x20         f:   { kind: finish, depends_on: [a], status: completed, output: \"{{steps.a.output}}\" }\n\
        \x20       :::\n\
        \x20     params: { id: { type: string, required: true } }\n\
        \x20     ttl: 12s\n";
    let head = format!(
        "agent: {{ name: parent }}\nstore: {{ kind: file, file: {{ path: {dir}/state }} }}\n\
         intelligence: {{ endpoints: \"mock:echo-system\", model: mock }}\n\
         observability: {{ log_level: info, log_content: true }}\n{template}"
    );
    // The first life spawns the child and is killed with it running.
    let first = format!(
        "{head}lifecycle: {{ run_until: drained }}\n\
         workflows:\n  - name: caller\n    steps:\n\
        \x20     s:     {{ kind: once }}\n\
        \x20     spawn: {{ kind: subagent, template: scribe, params: {{ id: inc-7 }}, depends_on: [s] }}\n\
        \x20     nap:   {{ kind: sleep, depends_on: [spawn], duration: 6s }}\n\
        \x20     f:     {{ kind: finish, depends_on: [nap], status: completed }}\n"
    );
    std::fs::write(format!("{dir}/first.yaml"), first).unwrap();
    let log1 = format!("{dir}/first.log");
    let mut p1 = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &format!("{dir}/first.yaml")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log1).unwrap())
        .spawn()
        .unwrap();
    // Killed once the spawn step is done and its record persisted, with
    // the run still napping.
    let deadline = Instant::now() + Duration::from_secs(20);
    let child_pid = loop {
        let log = std::fs::read_to_string(&log1).unwrap_or_default();
        let spawned = events(&log, "step.done")
            .iter()
            .any(|e| e["step"] == "spawn" && e["status"] == "done");
        if let Some(pid) = events(&log, "instance.spawn")
            .first()
            .and_then(|e| e["pid"].as_i64())
            .filter(|_| spawned)
        {
            break pid as i32;
        }
        assert!(Instant::now() < deadline, "no spawn:\n{log}");
        std::thread::sleep(Duration::from_millis(50));
    };
    std::thread::sleep(Duration::from_millis(500));
    unsafe { libc::kill(p1.id() as i32, libc::SIGKILL) };
    let _ = p1.wait();
    // PDEATHSIG takes the child down with its parent.
    let deadline = Instant::now() + Duration::from_secs(10);
    while unsafe { libc::kill(child_pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "the child outlived its parent");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The second life respawns it and asks it by its handle.
    let second = format!(
        "{head}lifecycle: {{ run_until: idle, idle_grace: 1500ms }}\n\
         workflows:\n  - name: asker\n    steps:\n\
        \x20     s:   {{ kind: once }}\n\
        \x20     ask: {{ kind: a2a.delegate, depends_on: [s], peer: inst-1, command: scribe.ask, args: {{}}, timeout: 30s, retry: {{ max: 6, backoff: 1s }} }}\n\
        \x20     f:   {{ kind: finish, depends_on: [ask], status: completed, output: \"{{{{steps.ask.output}}}}\" }}\n"
    );
    std::fs::write(format!("{dir}/second.yaml"), second).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &format!("{dir}/second.yaml")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert!(
        !events(&log, "instance.respawn").is_empty(),
        "the child was not respawned:\n{log}"
    );
    let done = events(&log, "run.done");
    let asker: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "asker").collect();
    assert_eq!(asker.len(), 1, "{log}");
    assert_eq!(asker[0]["status"], "completed", "{log}");
    let prompt = asker[0]["output"].as_str().unwrap_or("");
    assert!(
        prompt.contains("MUST: answer for inc-7.\n") && !prompt.contains("**MUST:**"),
        "the respawned child delivered its instruction a second time:\n{prompt}"
    );
}

/// A param is data, and names no credential: one that spells a secret
/// reference — text from outside, here the trigger's input — refuses the
/// instance spawn, so no child resolves it or is started with the variable
/// it names.
#[cfg(feature = "a2a")]
#[test]
fn an_instance_param_that_names_a_secret_is_refused() {
    let dir = common::unique_path("tpl-secret", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        "agent: { name: parent }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 700ms }\n\
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
         workflows:\n  - name: caller\n    steps:\n\
        \x20     s:     { kind: once, inputs: { id: \"{{secret:TEMPLATE_PARAM_SECRET}}\" } }\n\
        \x20     spawn: { kind: subagent, template: room, params: { id: \"{{ inputs.id }}\" }, depends_on: [s], on_error: continue }\n\
        \x20     f:     { kind: finish, depends_on: [spawn], status: completed, output: \"{{steps.spawn.error | ok}}\" }\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env("AGENTD_STATE_DIR", format!("{dir}/state"))
        .env("TEMPLATE_PARAM_SECRET", "s3cr3t-value")
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{log}");
    let spawn: Vec<Value> = events(&log, "step.done")
        .into_iter()
        .filter(|e| e["step"] == "spawn")
        .collect();
    assert_eq!(spawn.len(), 1, "{log}");
    let err = spawn[0]["err"].as_str().unwrap_or("");
    assert!(
        spawn[0]["status"] == "failed" && err.contains("names a secret"),
        "the spawn is refused for the param: {err}\n{log}"
    );
    assert!(
        events(&log, "instance.spawn").is_empty(),
        "no child was started:\n{log}"
    );
    assert!(!log.contains("s3cr3t-value"), "{log}");
}

/// A `desk` whose `desk.work` command runs one model step, so each command
/// spends what the mock model reports for one call.
#[cfg(feature = "a2a")]
const DESK: &str = "subagents:\n\
    \x20 templates:\n\
    \x20   desk:\n\
    \x20     instruction: |\n\
    \x20       You keep the books for {{params.id}}.\n\
    \x20       :::!workflow{name=on-work}\n\
    \x20       steps:\n\
    \x20         cmd: { kind: a2a, command: desk.work, roles: [agent, operator] }\n\
    \x20         a:   { kind: agent, depends_on: [cmd], instruction: go }\n\
    \x20         f:   { kind: finish, depends_on: [a], status: completed, output: \"{{steps.a.output}}\" }\n\
    \x20       :::\n\
    \x20     params: { id: { type: string, required: true } }\n\
    \x20     durable: true\n";

/// The lifetime spend the manifest at `path` records, `0` when there is none.
#[cfg(feature = "a2a")]
fn lifetime_used(path: &str) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|m| m.pointer("/state/budget/instance/lifetime_used")?.as_u64())
        .unwrap_or(0)
}

/// `agentd --config <dir>/<name>.yaml` with `cfg`, to its exit: the exit and
/// the log.
#[cfg(feature = "a2a")]
fn run_named(dir: &str, name: &str, cfg: &str) -> (Option<i32>, String) {
    let path = format!("{dir}/{name}.yaml");
    std::fs::write(&path, cfg).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A durable instance child's spend is its parent's: what the child's model
/// calls cost reaches the parent's budget while the child runs, and a
/// restarted parent charges only what the child spent since. What it charged
/// before is on the child's record, so the child's whole lifetime is not
/// charged a second time. The child's name is composed (`parent/desk`), so
/// its store keeps its manifest a level deeper than a name of one segment
/// would.
#[cfg(feature = "a2a")]
#[test]
fn a_durable_instance_childs_spend_is_charged_to_its_parent_once() {
    let dir = common::unique_path("tpl-meter", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let head = format!(
        "agent: {{ name: parent }}\nstore: {{ kind: file, file: {{ path: {dir}/state }} }}\n\
         intelligence: {{ endpoints: \"mock:final\", model: mock, budget: {{ lifetime_tokens: 1000000 }} }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
         observability: {{ log_level: info, log_content: true }}\n{DESK}"
    );
    let parent = format!("{dir}/state/agentd/parent/manifest/agent.json");
    let child = format!(
        "{dir}/state/subagents/file/agentd/parent/inst-1/state/agentd/parent/desk/manifest/agent.json"
    );
    // The first life spawns the desk, has it work once and outwaits a poll.
    let (code, log) = run_named(
        &dir,
        "first",
        &format!(
            "{head}workflows:\n  - name: caller\n    steps:\n\
            \x20     s:     {{ kind: once }}\n\
            \x20     spawn: {{ kind: subagent, template: desk, params: {{ id: north }}, depends_on: [s] }}\n\
            \x20     work:  {{ kind: a2a.delegate, depends_on: [spawn], peer: \"{{{{steps.spawn.output.peer}}}}\", command: desk.work, args: {{}}, timeout: 30s, retry: {{ max: 6, backoff: 1s }} }}\n\
            \x20     nap:   {{ kind: sleep, depends_on: [work], duration: 7s }}\n\
            \x20     f:     {{ kind: finish, depends_on: [nap], status: completed }}\n"
        ),
    );
    assert_eq!(code, Some(0), "{log}");
    let spent = lifetime_used(&child);
    assert!(spent > 0, "the desk recorded no spend at {child}:\n{log}");
    assert!(
        events(&log, "instance.metered")
            .iter()
            .any(|e| e["handle"] == "inst-1" && e["total_tokens"] == spent),
        "the parent never metered the desk's {spent} tokens:\n{log}"
    );
    assert_eq!(
        lifetime_used(&parent),
        spent,
        "the parent's budget is not what its desk spent:\n{log}"
    );
    // The second life respawns the desk and has it work again.
    let (code, log) = run_named(
        &dir,
        "second",
        &format!(
            "{head}workflows:\n  - name: again\n    steps:\n\
            \x20     s:    {{ kind: once }}\n\
            \x20     work: {{ kind: a2a.delegate, depends_on: [s], peer: inst-1, command: desk.work, args: {{}}, timeout: 30s, retry: {{ max: 6, backoff: 1s }} }}\n\
            \x20     nap:  {{ kind: sleep, depends_on: [work], duration: 7s }}\n\
            \x20     f:    {{ kind: finish, depends_on: [nap], status: completed }}\n"
        ),
    );
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !events(&log, "instance.respawn").is_empty(),
        "the desk was not respawned:\n{log}"
    );
    let total = lifetime_used(&child);
    let charged = lifetime_used(&parent);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(total > spent, "the respawned desk spent nothing:\n{log}");
    assert_eq!(
        charged, total,
        "the parent charged {charged} for a desk that spent {total} over two lives:\n{log}"
    );
}

/// A child that ends between two polls is charged for what it spent since
/// the last one: the parent reads its total once more as it reaps it. Here
/// the desk works and its ttl retires it inside the five seconds before the
/// parent's next poll, after which it is closed and polled no more.
#[cfg(feature = "a2a")]
#[test]
fn a_durable_instance_childs_last_spend_is_charged_when_it_exits() {
    let dir = common::unique_path("tpl-meter-exit", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let (code, log) = run_named(
        &dir,
        "parent",
        &format!(
            "agent: {{ name: parent }}\nstore: {{ kind: file, file: {{ path: {dir}/state }} }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
             observability: {{ log_level: info, log_content: true }}\n\
             {DESK}\x20     ttl: 3s\n\
             workflows:\n  - name: caller\n    steps:\n\
            \x20     s:     {{ kind: once }}\n\
            \x20     spawn: {{ kind: subagent, template: desk, params: {{ id: south }}, depends_on: [s] }}\n\
            \x20     work:  {{ kind: a2a.delegate, depends_on: [spawn], peer: \"{{{{steps.spawn.output.peer}}}}\", command: desk.work, args: {{}}, timeout: 30s, retry: {{ max: 6, backoff: 1s }} }}\n\
            \x20     nap:   {{ kind: sleep, depends_on: [work], duration: 6s }}\n\
            \x20     f:     {{ kind: finish, depends_on: [nap], status: completed }}\n"
        ),
    );
    let spent = lifetime_used(&format!(
        "{dir}/state/subagents/file/agentd/parent/inst-1/state/agentd/parent/desk/manifest/agent.json"
    ));
    let charged = lifetime_used(&format!("{dir}/state/agentd/parent/manifest/agent.json"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        events(&log, "instance.exited")
            .iter()
            .any(|e| e["status"] == "retired"),
        "the desk was not retired:\n{log}"
    );
    assert!(spent > 0, "the desk recorded no spend:\n{log}");
    assert_eq!(
        charged, spent,
        "the parent charged {charged} for a desk that spent {spent}:\n{log}"
    );
}

/// A child its restarted parent cannot bring back is still charged for what
/// it spent after the previous life's last poll: the parent reads its total
/// as it finds its record again, before the respawn fails and closes it.
/// Here the first life ends inside the five seconds after the desk's work,
/// and the desk's composed config is gone by the second.
#[cfg(feature = "a2a")]
#[test]
fn a_durable_instance_child_that_does_not_come_back_is_charged_for_its_last_spend() {
    let dir = common::unique_path("tpl-meter-lost", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let head = format!(
        "agent: {{ name: parent }}\nstore: {{ kind: file, file: {{ path: {dir}/state }} }}\n\
         intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
         observability: {{ log_level: info, log_content: true }}\n{DESK}"
    );
    let (code, log) = run_named(
        &dir,
        "first",
        &format!(
            "{head}workflows:\n  - name: caller\n    steps:\n\
            \x20     s:     {{ kind: once }}\n\
            \x20     spawn: {{ kind: subagent, template: desk, params: {{ id: east }}, depends_on: [s] }}\n\
            \x20     work:  {{ kind: a2a.delegate, depends_on: [spawn], peer: \"{{{{steps.spawn.output.peer}}}}\", command: desk.work, args: {{}}, timeout: 30s, retry: {{ max: 6, backoff: 1s }} }}\n\
            \x20     f:     {{ kind: finish, depends_on: [work], status: completed }}\n"
        ),
    );
    assert_eq!(code, Some(0), "{log}");
    // The desk drains as its parent dies, and has written its last before
    // the next life reads it.
    let pid = events(&log, "instance.spawn")
        .first()
        .and_then(|e| e["pid"].as_i64())
        .expect("a spawn") as i32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the desk outlived its parent"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let desk = format!("{dir}/state/subagents/file/agentd/parent/inst-1");
    std::fs::remove_file(format!("{desk}/config.json")).unwrap();
    let (code, log) = run_named(
        &dir,
        "second",
        &format!(
            "{head}workflows:\n  - name: idle\n    steps:\n\
            \x20     s: {{ kind: once }}\n\
            \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
        ),
    );
    let spent = lifetime_used(&format!(
        "{desk}/state/agentd/parent/desk/manifest/agent.json"
    ));
    let charged = lifetime_used(&format!("{dir}/state/agentd/parent/manifest/agent.json"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        events(&log, "instance.respawn").is_empty(),
        "the desk came back:\n{log}"
    );
    assert!(spent > 0, "the desk recorded no spend:\n{log}");
    assert_eq!(
        charged, spent,
        "the parent charged {charged} for a desk that spent {spent}:\n{log}"
    );
}
