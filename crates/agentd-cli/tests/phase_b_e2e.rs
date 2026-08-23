// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0036/0037 Phase B end to end: a sync-mode instance child resolving the
//! spawn with its declared workflow's first result; child streams mirrored
//! into the parent's; the catalog's per-entry breaker default opening on an
//! unreachable service; and the `http` step's method ceiling.
#![cfg(all(unix, feature = "workflow", feature = "a2a"))]

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
    let dir = common::unique_path("pb", "d");
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
fn a_sync_instance_resolves_the_spawn_with_its_workflows_first_result() {
    // caller1 parks on the sync spawn; caller2 pokes the singleton child by
    // its template alias; the child's on-ping completes; the composed
    // reporter dials `_instance.result` home; the spawn resolves with the
    // run's output while the child keeps running under its ttl.
    let port = common::free_port();
    let (code, log) = run_cfg(&format!(
        "config_version: \"1\"\nagent: {{ name: parent }}\nstore: {{ kind: memory }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1500ms }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         a2a: {{ listen: \"http://127.0.0.1:{port}\" }}\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   room:\n\
        \x20     instruction: |\n\
        \x20       The room.\n\
        \x20       :::workflow\n\
        \x20       name: on-ping\n\
        \x20       version: 3\n\
        \x20       steps:\n\
        \x20         cmd: {{ kind: a2a, command: room.ping, roles: [agent, operator] }}\n\
        \x20         f:   {{ kind: finish, depends_on: [cmd], status: completed, output: \"pong {{{{steps.cmd.output.args.x}}}}\" }}\n\
        \x20       :::\n\
        \x20     mode: sync\n\
        \x20     result: {{ workflow: on-ping }}\n\
        \x20     singleton: true\n\
        \x20     ttl: 20s\n\
         workflows:\n\
        \x20 - name: waiter\n    steps:\n\
        \x20     s:     {{ kind: once }}\n\
        \x20     spawn: {{ kind: subagent, template: room, depends_on: [s], timeout: 25s }}\n\
        \x20     f:     {{ kind: finish, depends_on: [spawn], status: completed, output: \"{{{{steps.spawn.output.result.output}}}}\" }}\n\
        \x20 - name: poker\n    steps:\n\
        \x20     s:    {{ kind: once }}\n\
        \x20     nap:  {{ kind: sleep, depends_on: [s], duration: 1500ms }}\n\
        \x20     poke: {{ kind: a2a.delegate, depends_on: [nap], peer: room, command: room.ping, args: {{ x: hello }}, timeout: 30s, retry: {{ max: 8, backoff: 1s }} }}\n\
        \x20     f:    {{ kind: finish, depends_on: [poke], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    let waiter: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "waiter").collect();
    assert_eq!(waiter.len(), 1, "{log}");
    assert_eq!(waiter[0]["status"], "completed", "{log}");
    assert_eq!(
        waiter[0]["output"], "pong hello",
        "the sync spawn resolved with the child workflow's first output:\n{log}"
    );
    assert!(
        !events(&log, "instance.result").is_empty(),
        "the reporter dialed home:\n{log}"
    );
}

#[test]
fn a_mirrored_child_stream_lands_in_the_parents_stream() {
    let port = common::free_port();
    let (code, log) = run_cfg(&format!(
        "config_version: \"1\"\nagent: {{ name: parent }}\nstore: {{ kind: memory }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 2000ms }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         a2a: {{ listen: \"http://127.0.0.1:{port}\" }}\n\
         streams: {{ orders: {{ retention: {{ max_events: 100 }} }} }}\n\
         subagents:\n\
        \x20 templates:\n\
        \x20   desk:\n\
        \x20     instruction: |\n\
        \x20       The desk.\n\
        \x20       :::stream{{name=orders}}\n\
        \x20       retention: {{ max_events: 100 }}\n\
        \x20       :::\n\
        \x20       :::workflow\n\
        \x20       name: on-add\n\
        \x20       version: 3\n\
        \x20       steps:\n\
        \x20         cmd: {{ kind: a2a, command: desk.add, roles: [agent, operator] }}\n\
        \x20         put: {{ kind: emit, depends_on: [cmd], stream: orders, subject: \"order.new\", data: {{ sku: \"{{{{steps.cmd.output.args.sku}}}}\" }} }}\n\
        \x20         f:   {{ kind: finish, depends_on: [put], status: completed }}\n\
        \x20       :::\n\
        \x20     mirror_streams: [orders]\n\
        \x20     singleton: true\n\
        \x20     ttl: 20s\n\
         workflows:\n\
        \x20 - name: spawner\n    steps:\n\
        \x20     s:     {{ kind: once }}\n\
        \x20     spawn: {{ kind: subagent, template: desk, depends_on: [s] }}\n\
        \x20     nap:   {{ kind: sleep, depends_on: [spawn], duration: 1500ms }}\n\
        \x20     poke:  {{ kind: a2a.delegate, depends_on: [nap], peer: desk, command: desk.add, args: {{ sku: \"A-1\" }}, timeout: 30s, retry: {{ max: 8, backoff: 1s }} }}\n\
        \x20     f:     {{ kind: finish, depends_on: [poke], status: completed }}\n\
        \x20 - name: watcher\n    steps:\n\
        \x20     ev: {{ kind: stream, stream: orders, from: new }}\n\
        \x20     f:  {{ kind: finish, depends_on: [ev], status: completed, output: \"saw {{{{steps.ev.output.data.sku}}}} from {{{{steps.ev.output.source}}}}\" }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !events(&log, "instance.mirror").is_empty(),
        "the child's event crossed the socket into the parent's stream:\n{log}"
    );
    let done = events(&log, "run.done");
    let watcher: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "watcher").collect();
    assert_eq!(watcher.len(), 1, "the parent's consumer fired once:\n{log}");
    let out = watcher[0]["output"].as_str().unwrap_or("");
    assert!(
        out.starts_with("saw A-1 from instance:"),
        "the mirrored event carried the data and its instance source: {out}\n{log}"
    );
}

#[test]
fn a_catalog_breaker_default_opens_for_a_failing_service() {
    // The entry declares the breaker; the referencing server's steps inherit
    // it with no per-step `breaker:` — first failure opens, second fast-fails.
    // The mock MCP connects fine; calling a tool it does not serve fails.
    let mock = common::spawn_mock_mcp("mock://r", false);
    let (code, log) = run_cfg(&format!(
        "config_version: \"1\"\nagent: {{ name: b }}\nstore: {{ kind: memory }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 700ms }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         services:\n\
        \x20 flaky:\n\
        \x20   endpoint: \"{}\"\n\
        \x20   breaker: {{ failures: 1, cooldown: 60s }}\n\
         mcp:\n  servers:\n    - {{ name: fl, service: flaky }}\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:  {{ kind: once }}\n\
        \x20     c1: {{ kind: mcp.tool, depends_on: [s], server: fl, tool: no_such_tool, retry: {{ max: 1, backoff: 100ms }}, on_error: continue }}\n\
        \x20     f:  {{ kind: finish, depends_on: [c1], status: completed, output: \"{{{{steps.c1.error | ok}}}}\" }}\n",
        mock.uri()
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !events(&log, "breaker.open").is_empty(),
        "the first failure opened the entry's default breaker:\n{log}"
    );
    let done = events(&log, "run.done");
    assert!(
        done[0]["output"]
            .as_str()
            .unwrap_or("")
            .contains("breaker open"),
        "the retry attempt failed fast on the inherited breaker (per-step state, entry-supplied policy): {done:?}\n{log}"
    );
    drop(mock);
}

#[test]
fn the_http_steps_method_ceiling_holds() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: h }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         services:\n\
        \x20 hooks:\n\
        \x20   kind: http\n\
        \x20   endpoint: \"http://127.0.0.1:1/hook\"\n\
        \x20   methods: [GET]\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     p: { kind: http, depends_on: [s], method: POST, url: \"http://127.0.0.1:1/hook\", on_error: continue }\n\
        \x20     f: { kind: finish, depends_on: [p], status: completed, output: \"{{steps.p.error | ok}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert!(
        done[0]["output"]
            .as_str()
            .unwrap_or("")
            .contains("method ceiling"),
        "POST was refused by the entry's methods ceiling before any dial: {done:?}\n{log}"
    );
}
