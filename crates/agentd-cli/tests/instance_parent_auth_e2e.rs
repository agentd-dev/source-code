// SPDX-License-Identifier: AGPL-3.0-only
//! **An instance child must be able to name itself to its parent.**
//!
//! A sync-mode instance child reports home with `_instance.result`, an op only
//! the operator may send. Over a unix listener the kernel names the child;
//! over TCP the child is the operator only by presenting the parent's bearer,
//! and the composed config may carry that bearer only as a `{{secret:…}}`
//! reference — an inline one is never written to the child's config file.
//!
//! So the same parent, differing only in how it holds its bearer, either
//! spawns a child whose report arrives, or refuses the spawn up front with
//! `instance.spawn.refused`. The refusal is the point: without it the child
//! ran its work and then had every report refused, and the spawn sat parked
//! until its timeout with nothing in the log saying why.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Command, Stdio};

use serde_json::Value;

/// The committed test PKI: a server identity for `127.0.0.1` and its CA.
fn fixture(name: &str) -> String {
    format!(
        "{}/../net/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// The parent's bearer. The variable is deliberately not `AGENTD_`-prefixed:
/// a child's environment is scrubbed of the config aliases, and this one has
/// to reach the child for its `{{secret:…}}` reference to resolve.
const TOKEN_VAR: &str = "INSTANCE_PARENT_A2A_TOKEN";
const TOKEN: &str = "instance-parent-token";

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// Pings the child once it is up, which is what completes its result
/// workflow and so sends its report home.
const POKER: &str = "\x20 - name: poker\n    steps:\n\
    \x20     s:    { kind: once }\n\
    \x20     nap:  { kind: sleep, depends_on: [s], duration: 1500ms }\n\
    \x20     poke: { kind: a2a.delegate, depends_on: [nap], peer: room, command: room.ping, args: { x: hello }, timeout: 30s, retry: { max: 8, backoff: 1s } }\n\
    \x20     f:    { kind: finish, depends_on: [poke], status: completed }\n";

/// A parent on `https://127.0.0.1` whose `waiter` spawns a sync instance
/// child and finishes with the child's first result, which `poker`, when
/// `poke` is set, provokes. `bearer` is the parent's `a2a` bearer line, if it
/// keeps one in the file.
fn parent_config(port: u16, bearer: &str, poke: bool) -> String {
    format!(
        "agent: {{ name: parent }}\nstore: {{ kind: memory }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1500ms }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         security: {{ tls_ca: \"{ca}\" }}\n\
         a2a:\n  listen: \"https://127.0.0.1:{port}\"\n  tls: {{ cert: \"{cert}\", key: \"{key}\" }}\n{bearer}\
         subagents:\n\
        \x20 templates:\n\
        \x20   room:\n\
        \x20     instruction: |\n\
        \x20       The room.\n\
        \x20       :::!workflow{{name=on-ping}}\n\
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
        {poker}",
        poker = if poke { POKER } else { "" },
        ca = fixture("ca.pem"),
        cert = fixture("server.pem"),
        key = fixture("server.key"),
    )
}

/// Run the parent to completion with `extra` arguments; its exit code and log.
fn run_parent(cfg_text: &str, extra: &[&str]) -> (Option<i32>, String) {
    let dir = common::unique_path("instance-parent", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .args(extra)
        .env(TOKEN_VAR, TOKEN)
        .env("AGENTD_STATE_DIR", format!("{dir}/state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run the parent");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

/// The bearer as a reference: the child's `parent` peer carries it, the
/// child's report is admitted as the operator's, and the spawn resolves with
/// the child's result.
#[test]
fn a_tcp_parent_with_a_referenced_bearer_hears_its_child() {
    let bearer = format!("  bearer: \"{{{{secret:{TOKEN_VAR}}}}}\"\n");
    let (code, log) = run_parent(&parent_config(common::free_port(), &bearer, true), &[]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        events(&log, "instance.spawn.refused").is_empty(),
        "a child that can authenticate is not refused:\n{log}"
    );
    assert!(
        !events(&log, "instance.result").is_empty(),
        "the child's `_instance.result` reached the parent:\n{log}"
    );
    let done = events(&log, "run.done");
    let waiter: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "waiter").collect();
    assert_eq!(waiter.len(), 1, "{log}");
    assert_eq!(waiter[0]["output"], "pong hello", "{log}");
}

/// The same parent holding its bearer inline — given on the command line,
/// since a config file may not carry one — cannot hand it to a child, so the
/// spawn is refused before a child exists, and says why.
#[test]
fn a_tcp_parent_with_an_inline_bearer_refuses_the_spawn() {
    let (_code, log) = run_parent(
        // Nothing pokes the child: there is none to poke.
        &parent_config(common::free_port(), "", false),
        &["--a2a.bearer", TOKEN],
    );
    let refused = events(&log, "instance.spawn.refused");
    assert_eq!(refused.len(), 1, "the spawn is refused once:\n{log}");
    assert_eq!(refused[0]["template"], "room", "{log}");
    assert!(
        refused[0]["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("the child cannot authenticate to its parent")),
        "{log}"
    );
    assert!(
        events(&log, "instance.spawn").is_empty(),
        "no child was started:\n{log}"
    );
}
