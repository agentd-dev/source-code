// SPDX-License-Identifier: AGPL-3.0-only
//! A worker cannot open what only the supervisor may (RFC 0044 P2a,
//! invariant 1, T4): where the configuration holds a supervisor-only key
//! the turn worker is started in a Landlock domain that denies the file
//! store — where the grant vault is — the key files and the audit chain;
//! with only the audit chain set, the chain. The worker's own process tries
//! each path (the `AGENTD_TEST_WORKER_OPEN` hook of a debug build) and says
//! what it could open; a path beside them stays open. Skipped on a kernel
//! without Landlock, where the RFC's P1 residual holds.
#![cfg(unix)]

mod common;

use std::process::{Command, Stdio};

use serde_json::Value;

const WORKFLOW: &str = "workflows:\n  - name: w\n    steps:\n\
                        \x20     s: { kind: once }\n\
                        \x20     t: { kind: agent, depends_on: [s], instruction: \"say done\" }\n\
                        \x20     f: { kind: finish, depends_on: [t], status: completed }\n";

/// Run the daemon on `cfg`, its workers asked to open `paths`: what each
/// opened, by path, and the daemon's log.
fn opened(cfg: &str, paths: &[String]) -> (Vec<(String, bool)>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .env("AGENTD_TEST_WORKER_OPEN", paths.join(":"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(0), "{log}");
    let seen = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["event"] == "test.worker_open")
        .map(|e| {
            (
                e["path"].as_str().unwrap_or_default().to_string(),
                e["opened"] == true,
            )
        })
        .collect();
    (seen, log)
}

/// Assert that every worker opened `open` and none of `denied`.
fn held(seen: &[(String, bool)], denied: &[String], open: &str, log: &str) {
    assert!(!seen.is_empty(), "no worker said what it opened:\n{log}");
    for (p, ok) in seen {
        if p == open {
            assert!(
                ok,
                "the worker could not open {p}, which is no one's secret:\n{log}"
            );
        } else if denied.contains(p) {
            assert!(!ok, "the worker opened {p}:\n{log}");
        }
    }
    for d in denied {
        assert!(
            seen.iter().any(|(p, _)| p == d),
            "{d} was not tried:\n{log}"
        );
    }
}

#[test]
fn a_worker_cannot_open_the_audit_chain() {
    if agentd::sec::landlock::abi().is_none() {
        eprintln!("skipped: this kernel has no Landlock");
        return;
    }
    let dir = common::unique_path("confine-chain", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let public = format!("{dir}/public.txt");
    std::fs::write(&public, "public").unwrap();
    let chain = format!("{dir}/audit/audit.log");
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{ name: confine, preflight: never }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             store: {{ kind: file, file: {{ path: \"{dir}/state\" }} }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 500ms }}\n\
             observability: {{ log_level: info, audit: {{ chain: {{ path: \"{chain}\" }} }} }}\n{WORKFLOW}"
        ),
    )
    .unwrap();
    let (seen, log) = opened(&cfg, &[chain.clone(), public.clone()]);
    assert!(log.contains("\"event\":\"proc.landlock\""), "{log}");
    held(&seen, &[chain], &public, &log);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An instance-tier child's directory is under the parent's store root,
/// which the parent's children are denied: the child keeps its own — its
/// config, socket, log and store — and answers its parent.
#[cfg(all(feature = "xaa", feature = "a2a"))]
#[test]
fn an_instance_keeps_its_own_directory_inside_the_denied_store() {
    if agentd::sec::landlock::abi().is_none() {
        eprintln!("skipped: this kernel has no Landlock");
        return;
    }
    let dir = common::unique_path("confine-instance", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let key = format!("{dir}/idp.pem");
    std::fs::write(
        &key,
        "-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    let seal = format!("{dir}/seal");
    std::fs::write(&seal, "ab".repeat(32)).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{ name: parent, preflight: never }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             store: {{ kind: file, file: {{ path: \"{dir}/state\" }} }}\n\
             identity:\n  providers:\n    okta:\n      issuer: \"https://acme.okta.example\"\n\
             \x20     client_id: agentd-idp\n\
             \x20     client_auth: {{ method: private_key_jwt, key: \"{{{{secret-file:{key}}}}}\" }}\n\
             \x20 delegation: {{ seal_key: \"{{{{secret-file:{seal}}}}}\" }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 500ms }}\n\
             observability: {{ log_level: info }}\n\
             subagents:\n  templates:\n    room:\n      instruction: |\n\
             \x20       You are the room.\n\
             \x20       :::!workflow{{name=on-ping}}\n\
             \x20       steps:\n\
             \x20         cmd: {{ kind: a2a, command: room.ping, roles: [agent, operator] }}\n\
             \x20         f:   {{ kind: finish, depends_on: [cmd], status: completed, output: pong }}\n\
             \x20       :::\n\
             \x20     ttl: 15s\n\
             workflows:\n  - name: caller\n    steps:\n\
             \x20     s:     {{ kind: once }}\n\
             \x20     spawn: {{ kind: subagent, template: room, depends_on: [s] }}\n\
             \x20     ping:  {{ kind: a2a.delegate, depends_on: [spawn], peer: \"{{{{steps.spawn.output.peer}}}}\", command: room.ping, args: {{}}, timeout: 10s, retry: {{ max: 6, backoff: 1s }} }}\n\
             \x20     f:     {{ kind: finish, depends_on: [ping], status: completed }}\n"
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let done: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["event"] == "run.done" && e["workflow"] == "caller")
        .collect();
    assert_eq!(done.len(), 1, "{log}");
    // The run completes only once the child, started from its own
    // directory, answered the ping.
    assert_eq!(done[0]["status"], "completed", "{log}");
    let spawn = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["event"] == "instance.spawn")
        .expect("an instance was spawned");
    let socket = spawn["socket"].as_str().unwrap_or_default();
    assert!(
        socket.starts_with(&format!("{dir}/state")),
        "the child's directory is inside the parent's store root: {socket}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "xaa")]
#[test]
fn a_worker_cannot_open_the_vault_or_a_key_file() {
    if agentd::sec::landlock::abi().is_none() {
        eprintln!("skipped: this kernel has no Landlock");
        return;
    }
    let dir = common::unique_path("confine-vault", "d");
    std::fs::create_dir_all(format!("{dir}/state")).unwrap();
    let in_store = format!("{dir}/state/probe");
    std::fs::write(&in_store, "x").unwrap();
    let key = format!("{dir}/idp.pem");
    std::fs::write(
        &key,
        "-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    let seal = format!("{dir}/seal");
    std::fs::write(&seal, "ab".repeat(32)).unwrap();
    let public = format!("{dir}/public.txt");
    std::fs::write(&public, "public").unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{ name: confine, preflight: never }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             store: {{ kind: file, file: {{ path: \"{dir}/state\" }} }}\n\
             identity:\n  providers:\n    okta:\n      issuer: \"https://acme.okta.example\"\n\
             \x20     client_id: agentd-idp\n\
             \x20     client_auth: {{ method: private_key_jwt, key: \"{{{{secret-file:{key}}}}}\" }}\n\
             \x20 delegation: {{ seal_key: \"{{{{secret-file:{seal}}}}}\" }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 500ms }}\n\
             observability: {{ log_level: info }}\n{WORKFLOW}"
        ),
    )
    .unwrap();
    let (seen, log) = opened(
        &cfg,
        &[in_store.clone(), key.clone(), seal.clone(), public.clone()],
    );
    held(&seen, &[in_store, key, seal], &public, &log);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The documented layout: the seal key and the audit chain inside the file
/// store's root. The rest of the root — the vault, the chain's head — is
/// still denied, and not granted again on the way to them.
#[cfg(feature = "xaa")]
#[test]
fn a_key_and_the_chain_inside_the_store_leave_the_whole_store_denied() {
    if agentd::sec::landlock::abi().is_none() {
        eprintln!("skipped: this kernel has no Landlock");
        return;
    }
    let dir = common::unique_path("confine-nested", "d");
    std::fs::create_dir_all(format!("{dir}/state")).unwrap();
    let in_store = format!("{dir}/state/probe");
    std::fs::write(&in_store, "x").unwrap();
    let seal = format!("{dir}/state/seal");
    std::fs::write(&seal, "ab".repeat(32)).unwrap();
    let key = format!("{dir}/idp.pem");
    std::fs::write(
        &key,
        "-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    let chain = format!("{dir}/state/audit/audit.log");
    let public = format!("{dir}/public.txt");
    std::fs::write(&public, "public").unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{ name: confine, preflight: never }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             store: {{ kind: file, file: {{ path: \"{dir}/state\" }} }}\n\
             identity:\n  providers:\n    okta:\n      issuer: \"https://acme.okta.example\"\n\
             \x20     client_id: agentd-idp\n\
             \x20     client_auth: {{ method: private_key_jwt, key: \"{{{{secret-file:{key}}}}}\" }}\n\
             \x20 delegation: {{ seal_key: \"{{{{secret-file:{seal}}}}}\" }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 500ms }}\n\
             observability: {{ log_level: info, audit: {{ chain: {{ path: \"{chain}\" }} }} }}\n{WORKFLOW}"
        ),
    )
    .unwrap();
    let (seen, log) = opened(
        &cfg,
        &[
            in_store.clone(),
            seal.clone(),
            chain.clone(),
            key.clone(),
            public.clone(),
        ],
    );
    held(&seen, &[in_store, seal, chain, key], &public, &log);
    let _ = std::fs::remove_dir_all(&dir);
}
