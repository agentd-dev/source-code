// SPDX-License-Identifier: AGPL-3.0-only
//! A `wait {on: resource}` across a restart.
//!
//! The wait is durable — its record is in the store, and the next life
//! restores the step suspended — but the subscription that wakes it lived on
//! the previous life's connection. Boot subscribes again, through the path a
//! reload's re-dial takes, so the server's next update still resolves the
//! wait. Without it the restored step parked until its timeout with nothing
//! in any log.
//!
//! A restored wait that can never be woken is a loud failure instead: its
//! server is gone from the config, or no longer offers subscriptions.
#![cfg(unix)]

mod common;

use serde_json::Value;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const WAITED: &str = "mock://waited";

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A long-lived config on a file store at `<dir>/state`, written through on
/// every checkpoint. `server` is the endpoint of `a`; `None` drops both the
/// server and the waiting workflow, so only the restored run still names it.
fn config(dir: &str, server: Option<&str>) -> String {
    let mut out = format!(
        "agent:\n  name: waiter\n\
         store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 700ms\n\
         observability:\n  log_level: info\n  log_content: true\n\
         workflows:\n  - name: idle\n    steps:\n\
         \x20     s: {{kind: manual}}\n\
         \x20     f: {{kind: finish, depends_on: [s]}}\n"
    );
    if let Some(endpoint) = server {
        out.push_str(&format!(
            "  - name: waits\n    steps:\n\
             \x20     s: {{kind: once}}\n\
             \x20     w: {{kind: wait, depends_on: [s], on: resource, server: a, uri: \"{WAITED}\", timeout: 120s}}\n\
             \x20     f: {{kind: finish, depends_on: [w], status: completed, output: woken}}\n\
             mcp:\n  servers:\n    - {{name: a, endpoint: \"{endpoint}\", ns: a}}\n"
        ));
    }
    out
}

/// Whether the store holds the suspended wait's record: the run, written
/// with the wait it parked on (`since_ms` is the wait record's own field).
fn wait_is_durable(dir: &str) -> bool {
    fn walk(p: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir(p) else {
            return false;
        };
        entries.flatten().any(|e| {
            let path = e.path();
            if path.is_dir() {
                return walk(&path);
            }
            std::fs::read_to_string(&path).is_ok_and(|s| {
                s.contains("\"since_ms\"")
                    && s.contains("\"kind\":\"resource\"")
                    && s.contains(WAITED)
            })
        })
    }
    walk(&std::path::Path::new(dir).join("state"))
}

/// The first life: the run reaches its wait, the wait is in the store, and
/// the process is SIGKILLed — no drain, no goodbye checkpoint.
fn park_and_kill(cfg: &str, dir: &str) -> String {
    let err_path = common::unique_path("wait-resource", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !wait_is_durable(dir) {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let log = std::fs::read_to_string(&err_path).unwrap_or_default();
            panic!("the wait never reached the store:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    log
}

/// The next life, to its idle exit — bounded: a restored wait nothing
/// wakes keeps the run, and so the daemon, alive, and that is the failure
/// this file exists to catch.
fn life(cfg: &str) -> (ExitStatus, String) {
    let err_path = common::unique_path("wait-resource", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let log = std::fs::read_to_string(&err_path).unwrap_or_default();
            panic!("the restored wait was never resolved, and the daemon never went idle:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    (status, log)
}

/// The first life parks on a server that never pushes, so the wait is still
/// suspended when the process dies.
fn first_life(tag: &str) -> (String, String) {
    let dir = common::unique_path(tag, "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    let quiet = common::spawn_mock_mcp(WAITED, false);
    std::fs::write(&cfg, config(&dir, Some(&format!("{}/mcp", quiet.uri())))).unwrap();
    let l1 = park_and_kill(&cfg, &dir);
    assert_eq!(quiet.subscribes(WAITED), 1, "{}", quiet.log());
    assert!(events(&l1, "run.done").is_empty(), "killed mid-wait:\n{l1}");
    (dir, cfg)
}

#[test]
fn a_resource_wait_survives_a_restart_and_resolves_on_the_next_update() {
    let (dir, cfg) = first_life("wait-resource-boot");

    // The next life, on a server that pushes an update once subscribed: the
    // only subscription that can reach it is the one boot makes for the
    // restored wait (the `once` start does not fire again beside a live run).
    let pushing = common::spawn_mock_mcp(WAITED, true);
    std::fs::write(&cfg, config(&dir, Some(&format!("{}/mcp", pushing.uri())))).unwrap();
    let (status, l2) = life(&cfg);
    assert_eq!(status.code(), Some(0), "{l2}");
    assert!(
        events(&l2, "wait.resubscribed")
            .iter()
            .any(|e| e["server"] == "a" && e["uri"] == WAITED && e["reason"] == "boot"),
        "boot subscribed the restored wait again:\n{l2}"
    );
    // Counted where it lands: the new server saw the subscription.
    assert_eq!(pushing.subscribes(WAITED), 1, "{}", pushing.log());
    let done = events(&l2, "run.done");
    assert!(
        done.iter().any(|e| e["workflow"] == "waits"
            && e["status"] == "completed"
            && e["output"] == "woken"),
        "the restored run was woken by the update and finished:\n{l2}"
    );
    assert_eq!(
        done.len(),
        1,
        "no second run beside the restored one:\n{l2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_restored_wait_whose_server_is_gone_fails_at_boot() {
    let (dir, cfg) = first_life("wait-resource-gone");

    // The server, and the workflow that named it, are gone from the config;
    // the run still holds its pinned definition and its wait on `a`.
    std::fs::write(&cfg, config(&dir, None)).unwrap();
    let (_, l2) = life(&cfg);
    let refused: Vec<Value> = events(&l2, "wait.resubscribe.fail");
    assert!(
        refused.iter().any(|e| e["level"] == "error"
            && e["server"] == "a"
            && e["reason"] == "boot"
            && e["err"]
                .as_str()
                .is_some_and(|s| s.contains("not connected"))),
        "the wait that can never be woken is said at error level:\n{l2}"
    );
    assert!(
        events(&l2, "run.done")
            .iter()
            .any(|e| e["workflow"] == "waits" && e["status"] == "failed"),
        "the restored run failed instead of parking:\n{l2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_restored_wait_on_a_server_without_subscriptions_fails_at_boot() {
    let (dir, cfg) = first_life("wait-resource-nosub");

    // The server came back without `resources.subscribe`.
    let addr_file = common::unique_path("mock-nosub", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let thread_file = addr_file.clone();
    std::thread::spawn(move || {
        agentd::mcp::mock_http::run_offering(&thread_file, WAITED, true, false);
    });
    let endpoint = format!("http://{}/mcp", common::read_addr_file(&addr_file));
    std::fs::write(&cfg, config(&dir, Some(&endpoint))).unwrap();
    let (_, l2) = life(&cfg);
    assert!(
        events(&l2, "wait.resubscribe.fail")
            .iter()
            .any(|e| e["level"] == "error"
                && e["uri"] == WAITED
                && e["reason"] == "boot"
                && e["err"]
                    .as_str()
                    .is_some_and(|s| s.contains("resources.subscribe"))),
        "the missing capability is named at error level:\n{l2}"
    );
    assert!(
        events(&l2, "run.done")
            .iter()
            .any(|e| e["workflow"] == "waits" && e["status"] == "failed"),
        "the restored run failed instead of parking:\n{l2}"
    );
    assert!(events(&l2, "wait.resubscribed").is_empty(), "{l2}");
    let _ = std::fs::remove_file(&addr_file);
    let _ = std::fs::remove_dir_all(&dir);
}
