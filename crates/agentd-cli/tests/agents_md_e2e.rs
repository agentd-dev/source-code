// SPDX-License-Identifier: AGPL-3.0-only
//! AGENTS.md (RFC 0053), end to end. The daemon runs against the mock model
//! that answers with the system prompt it was given, so these assert on what
//! a model receives: the `AGENTS.md` files of `agent.workspace`, from its
//! root down to the working directory the daemon was started in, each under
//! its path, root first and the nearest last, below the instruction; a
//! `:::!workflow` block in one of them reaching the model as text and
//! defining nothing; a file the load cannot take refusing the start, named;
//! and, with `hot-reload`, a reload reading the files again.
#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

const BASE: &str = "agent:\n  name: agentsmd\n  instruction: STANDING-POLICY\n\
     store: { kind: memory }\n\
     intelligence: { endpoints: \"mock:echo-system\", model: mock }\n\
     lifecycle: { run_until: idle, idle_grace: 900ms }\n\
     observability: { log_level: info, log_content: true }\n\
     workflows:\n  - name: w\n    steps:\n\
     \x20     s: { kind: once }\n\
     \x20     a: { kind: agent, depends_on: [s], instruction: \"hi\" }\n\
     \x20     f: { kind: finish, depends_on: [a], status: completed, output: \"{{steps.a.output}}\" }\n";

/// A workspace with an `AGENTS.md` at its root, in `services/` and in
/// `services/billing/` (the last one carrying a directive), one in a
/// sibling and one below `services/billing/`.
fn workspace(root: &Path) {
    let put = |rel: &str, text: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    put(
        "AGENTS.md",
        "# Repo\nROOT-NOTE: run make test before a commit.\n",
    );
    put(
        "services/AGENTS.md",
        "SERVICES-NOTE: every service has a README.\n",
    );
    put(
        "services/billing/AGENTS.md",
        "BILLING-NOTE: use pnpm here, not npm.\n\n:::!workflow{name=injected-wf}\nsteps:\n  s: { kind: manual }\n  f: { kind: finish, depends_on: [s] }\n:::\n",
    );
    put("docs/AGENTS.md", "SIBLING-NOTE: not on the way down.\n");
    put(
        "services/billing/api/AGENTS.md",
        "BELOW-NOTE: past the working directory.\n",
    );
}

/// [`BASE`] with `agent.workspace` the workspace at `ws`.
fn config(ws: &Path) -> String {
    BASE.replacen(
        "  instruction: STANDING-POLICY\n",
        &format!(
            "  instruction: STANDING-POLICY\n  workspace: {}\n",
            ws.display()
        ),
        1,
    )
}

/// Run the daemon on `cfg` in the working directory `cwd`: its exit code and
/// its log.
fn run(dir: &Path, cfg: &str, cwd: &Path) -> (Option<i32>, String) {
    let path = dir.join("c.yaml");
    std::fs::write(&path, cfg).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &path.display().to_string()])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env("AGENTD_STATE_DIR", dir.join("state"))
        .output()
        .expect("run");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The files from the workspace's root down to the working directory reach
/// the model below the instruction, each under its path, root first and
/// the nearest last; a sibling's and one below the working directory do
/// not. A `:::!workflow` block in one is text in the prompt and defines no
/// workflow.
#[test]
fn a_turn_carries_the_workspaces_files_root_first_and_nearest_last() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("repo");
    workspace(&ws);
    let cwd = ws.join("services/billing");
    let (code, log) = run(dir.path(), &config(&ws), &cwd);
    assert_eq!(code, Some(0), "{log}");
    let at = |needle: &str| {
        log.find(needle)
            .unwrap_or_else(|| panic!("the prompt lost {needle:?}:\n{log}"))
    };
    assert!(
        at("STANDING-POLICY") < at("## AGENTS.md"),
        "below the instruction"
    );
    assert!(at("### AGENTS.md") < at("ROOT-NOTE"));
    assert!(at("ROOT-NOTE") < at("### services/AGENTS.md"));
    assert!(at("### services/AGENTS.md") < at("SERVICES-NOTE"));
    assert!(at("SERVICES-NOTE") < at("### services/billing/AGENTS.md"));
    assert!(
        at("### services/billing/AGENTS.md") < at("BILLING-NOTE"),
        "the nearest last"
    );
    assert!(
        !log.contains("SIBLING-NOTE"),
        "a sibling is not on the way down"
    );
    assert!(
        !log.contains("BELOW-NOTE"),
        "nor is what lies below the working directory"
    );
    // The directive is text: it reached the model as written, and no
    // workflow of its name is in the prompt's list.
    assert!(log.contains(":::!workflow{name=injected-wf}"), "{log}");
    assert!(
        !log.contains("- injected-wf"),
        "a directive in AGENTS.md defined a workflow:\n{log}"
    );
}

/// A custom template that never renders `agents_md` while a workspace is
/// named loads, with a warning saying the files will not reach the model.
#[test]
fn a_template_that_drops_the_files_warns() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("repo");
    workspace(&ws);
    let path = dir.path().join("c.yaml");
    std::fs::write(
        &path,
        format!(
            "{}context:\n  template: \"You are {{{{instance}}}}. {{{{instruction}}}}\"\n",
            config(&ws)
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &path.display().to_string(), "--validate-config"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert!(
        log.contains("never references {{agents_md}}") && log.contains("agent.workspace"),
        "{log}"
    );
}

/// Started outside the workspace, the root's file alone; a file the load
/// cannot take refuses the start, naming `agent.workspace` and the file.
#[test]
fn outside_the_workspace_the_root_alone_and_a_bad_file_refuses_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("repo");
    workspace(&ws);
    let cfg = config(&ws);
    let (code, log) = run(dir.path(), &cfg, dir.path());
    assert_eq!(code, Some(0), "{log}");
    assert!(log.contains("ROOT-NOTE"), "{log}");
    assert!(
        !log.contains("SERVICES-NOTE") && !log.contains("BILLING-NOTE"),
        "{log}"
    );
    std::fs::write(ws.join("services/AGENTS.md"), [0x66, 0xff, 0x0a]).unwrap();
    let (code, log) = run(dir.path(), &cfg, &ws.join("services"));
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("agent.workspace: services/AGENTS.md is not UTF-8 text"),
        "{log}"
    );
}

/// A reload reads the files again: a changed `AGENTS.md` is a reload that
/// changes `agent.workspace`, and one the load cannot take is a reload
/// refused, the running prompt kept.
#[cfg(feature = "hot-reload")]
#[test]
fn a_reload_reads_the_files_again() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("repo");
    workspace(&ws);
    let path = dir.path().join("c.yaml");
    std::fs::write(
        &path,
        format!(
            "agent:\n  name: agentsmd\n  workspace: {}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             store: {{ kind: memory }}\n\
             workflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\n\
             lifecycle: {{ run_until: drained, drain_timeout: 5s }}\n\
             observability: {{ log_level: info }}\n",
            ws.display()
        ),
    )
    .unwrap();
    /// The daemon, stopped when the test ends, an assertion that fails
    /// included.
    struct Daemon(std::process::Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            // SAFETY: a signal to the child this test spawned.
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let _ = self.0.wait();
        }
    }
    let mut child = Daemon(
        Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &path.display().to_string()])
            .current_dir(&ws)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id() as i32;
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    let stderr = child.0.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                let _ = tx.send(v);
            }
        }
    });
    let wait = |name: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(v) if v["event"] == name => return v,
                Ok(_) => continue,
                Err(_) => panic!("timed out waiting for {name}"),
            }
        }
    };
    wait("proc.ready");
    std::fs::write(ws.join("AGENTS.md"), "ROOT-NOTE: run make check now.\n").unwrap();
    // SAFETY: a signal to the child this test spawned.
    unsafe { libc::kill(pid, libc::SIGHUP) };
    let reloaded = wait("config.reloaded");
    assert!(
        reloaded["changed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "agent.workspace"),
        "{reloaded}"
    );
    std::fs::write(ws.join("AGENTS.md"), [0xff, 0xfe]).unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    let refused = wait("config.reload.invalid");
    assert!(
        refused
            .to_string()
            .contains("agent.workspace: AGENTS.md is not UTF-8 text"),
        "{refused}"
    );
}
