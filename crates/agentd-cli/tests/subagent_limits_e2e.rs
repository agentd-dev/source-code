// SPDX-License-Identifier: AGPL-3.0-only
//! `subagent.run`'s OS resource allocation, proven on the real child process:
//! `limits: {memory, cpu}` become `RLIMIT_AS`/`RLIMIT_CPU` between fork and
//! exec, and `priority: low` becomes a niceness of +10 — read back from
//! `/proc/<pid>/limits` and `/proc/<pid>/stat` while the child is alive, not
//! inferred from our own bookkeeping.
#![cfg(all(target_os = "linux", feature = "workflow"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct MockLlm {
    child: Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}
fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("sublim-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("sublim-mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

#[test]
fn subagent_limits_and_priority_reach_the_child_process() {
    // A WARM child stays alive after spawning, so /proc stays readable.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {
                "instruction": "hold this context warm",
                "mode": "warm",
                "limits": {"memory": "512MB", "cpu": "5m"},
                "priority": "low"
            }}]},
            {"content": "spawned and done"}
        ]
    }));
    let cfg = common::unique_path("agentd-sublim", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
             agent:\n  instruction: delegate with caps\n\
             intelligence:\n  endpoints: {}\n  model: mock\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 6s\n\
             observability:\n  log_level: info\n",
            llm.uri
        ),
    )
    .unwrap();
    let err_path = common::unique_path("agentd-sublim", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");

    // Find the child's pid from the spawn event, then read its /proc.
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid = loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if let Some(p) = events(&log, "subagent.spawn")
            .first()
            .and_then(|e| e["pid"].as_i64())
        {
            break p;
        }
        assert!(
            Instant::now() < deadline,
            "no subagent.spawn with a pid:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    // /proc/<pid>/limits — the kernel's own view of the caps we set.
    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits"))
        .expect("the warm child is alive and readable");
    let line = |name: &str| {
        limits
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name:?} in:\n{limits}"))
            .to_string()
    };
    let addr = line("Max address space");
    assert!(
        addr.contains("536870912"),
        "RLIMIT_AS is the declared 512MB: {addr}"
    );
    let cpu = line("Max cpu time");
    let fields: Vec<&str> = cpu.split_whitespace().collect();
    // "Max cpu time <soft> <hard> seconds" — soft = 300s, hard = +5 grace.
    assert!(
        cpu.contains("300") && cpu.contains("305"),
        "RLIMIT_CPU soft 300 / hard 305: {fields:?}"
    );

    // /proc/<pid>/stat field 19 is the nice value: `priority: low` → +10. The
    // comm is parenthesised and may contain spaces, so split after the LAST ')'
    // — the numeric fields start with `state` (overall field 3) there.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    let after = &stat[stat.rfind(')').map(|i| i + 1).unwrap_or(0)..];
    let nice: i64 = after
        .split_whitespace()
        .nth(16) // state is field 3 → nice (field 19) is 16 past it
        .and_then(|f| f.parse().ok())
        .unwrap_or(i64::MIN);
    assert_eq!(nice, 10, "priority: low is nice +10; stat: {stat}");

    unsafe {
        libc::kill(daemon.id() as i32, libc::SIGTERM);
    }
    let _ = daemon.wait();
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&err_path);
}
