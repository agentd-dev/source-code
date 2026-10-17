// SPDX-License-Identifier: AGPL-3.0-only
//! Where an instance-tier child's state lives. A child's directory — its
//! composed config, socket, log and store — belongs to the parent that spawned
//! it. A parent whose store does not outlive it (memory, none) keeps its
//! children in a directory of its own process, which goes when it exits; it
//! never puts them in the state directory every such parent of the user would
//! share, so a new parent's `inst-1` is never an earlier parent's `inst-1`.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A child that answers `room.ping` at once and holds `room.hold` for a
/// minute.
const ROOM: &str = "subagents:\n\
    \x20 templates:\n\
    \x20   room:\n\
    \x20     instruction: |\n\
    \x20       You are the room for {{params.id}}.\n\
    \x20       :::!workflow{name=on-ping}\n\
    \x20       steps:\n\
    \x20         cmd: { kind: a2a, command: room.ping, roles: [agent, operator] }\n\
    \x20         f:   { kind: finish, depends_on: [cmd], status: completed, output: \"pong {{params.id}}\" }\n\
    \x20       :::\n\
    \x20       :::!workflow{name=on-hold}\n\
    \x20       steps:\n\
    \x20         cmd: { kind: a2a, command: room.hold, roles: [agent, operator] }\n\
    \x20         nap: { kind: sleep, depends_on: [cmd], duration: 60s }\n\
    \x20         f:   { kind: finish, depends_on: [nap], status: completed }\n\
    \x20       :::\n\
    \x20     params: { id: { type: string, required: true } }\n\
    \x20     ttl: 15s\n";

/// A caller that spawns a `room` and pings it.
const PING: &str = "\x20     s:     { kind: once }\n\
    \x20     spawn: { kind: subagent, template: room, params: { id: solo }, depends_on: [s] }\n\
    \x20     ping:  { kind: a2a.delegate, depends_on: [spawn], peer: \"{{steps.spawn.output.peer}}\", command: room.ping, args: {}, timeout: 10s, retry: { max: 6, backoff: 1s } }\n\
    \x20     f:     { kind: finish, depends_on: [ping], status: completed, output: \"{{steps.ping.output}}\" }\n";

/// A caller that spawns a `room` and gives it a few seconds.
const SPAWN: &str = "\x20     s:     { kind: once }\n\
    \x20     spawn: { kind: subagent, template: room, params: { id: solo }, depends_on: [s] }\n\
    \x20     nap:   { kind: sleep, depends_on: [spawn], duration: 5s }\n\
    \x20     f:     { kind: finish, depends_on: [nap], status: completed }\n";

/// A memory-store parent named `parent` with the `room` template, the given
/// lifecycle and one workflow, `caller`, of `steps`. Every parent here pins
/// the same run id: a process's private directory must not be the run's.
fn parent(lifecycle: &str, steps: &str) -> String {
    format!(
        "agent: {{ name: parent }}\nstore: {{ kind: memory }}\n\
         lifecycle: {{ {lifecycle}, run_id: pinned }}\n\
         observability: {{ log_level: info, log_content: true }}\n{ROOM}\
         workflows:\n  - name: caller\n    steps:\n{steps}"
    )
}

/// A fresh `AGENTD_STATE_DIR` and `TMPDIR` for the parents of one test,
/// removed with it.
struct Dirs {
    root: String,
}

impl Dirs {
    fn new(tag: &str) -> Dirs {
        let root = common::unique_path(tag, "d");
        std::fs::create_dir_all(format!("{root}/state")).unwrap();
        std::fs::create_dir_all(format!("{root}/tmp")).unwrap();
        Dirs { root }
    }
    fn state(&self) -> String {
        format!("{}/state", self.root)
    }
    fn tmp(&self) -> String {
        format!("{}/tmp", self.root)
    }
    /// `agentd --config <cfg>` under these directories, stderr to `err`.
    fn agentd(&self, name: &str, cfg: &str, err: &str) -> Command {
        let path = format!("{}/{name}.yaml", self.root);
        std::fs::write(&path, cfg).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_agentd"));
        c.args(["--config", &path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(err).unwrap())
            .env("AGENTD_STATE_DIR", self.state())
            .env("TMPDIR", self.tmp())
            .env_remove("XDG_RUNTIME_DIR");
        c
    }
    /// [`Dirs::agentd`] run to its exit: the exit and the log.
    fn run(&self, name: &str, cfg: &str) -> (Option<i32>, String) {
        self.run_with(name, cfg, &[])
    }
    /// [`Dirs::run`] with `env` set over these directories'.
    fn run_with(&self, name: &str, cfg: &str, env: &[(&str, &str)]) -> (Option<i32>, String) {
        let err = format!("{}/{name}.log", self.root);
        let mut c = self.agentd(name, cfg, &err);
        c.envs(env.iter().copied());
        let out: Output = c.output().expect("run");
        (
            out.status.code(),
            std::fs::read_to_string(&err).unwrap_or_default(),
        )
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A parent killed with SIGKILL when the test lets go of it, however it does.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The value of `run.done`'s `output` for the run of `caller`.
fn caller_output(log: &str) -> Value {
    let done: Vec<Value> = events(log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "caller")
        .collect();
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["status"], "completed", "{log}");
    done[0]["output"].clone()
}

/// A rendered value that may have come back as its JSON text.
fn value(v: &Value) -> Value {
    v.as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| v.clone())
}

/// The directory the child `instance.spawn` names lives in: its socket's.
fn child_dir(spawn: &Value) -> PathBuf {
    Path::new(spawn["socket"].as_str().expect("socket"))
        .parent()
        .expect("a socket in a directory")
        .to_path_buf()
}

fn wait_for(what: &str, mut ready: impl FnMut() -> Option<Value>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = ready() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A memory-store parent's `inst-1` is its own: a second parent of the same
/// name, under the same state directory and run id, spawns a child that
/// holds nothing the first parent's `inst-1` was doing when it died — no run
/// and no command it was sent — and answers for the second parent.
#[test]
fn a_memory_parent_does_not_adopt_an_earlier_parents_child() {
    let dirs = Dirs::new("inst-stale");
    // The first parent sends its child a command that holds a run open, and
    // dies with it there.
    let first = parent(
        "run_until: drained",
        "\x20     s:     { kind: once }\n\
         \x20     spawn: { kind: subagent, template: room, params: { id: first }, depends_on: [s] }\n\
         \x20     hold:  { kind: a2a.delegate, depends_on: [spawn], peer: \"{{steps.spawn.output.peer}}\", command: room.hold, args: {}, timeout: 60s, retry: { max: 6, backoff: 1s } }\n\
         \x20     f:     { kind: finish, depends_on: [hold], status: completed }\n",
    );
    let err1 = format!("{}/first.log", dirs.root);
    let p1 = Killed(dirs.agentd("first", &first, &err1).spawn().unwrap());
    let spawn = wait_for("the first child", || {
        events(
            &std::fs::read_to_string(&err1).unwrap_or_default(),
            "instance.spawn",
        )
        .into_iter()
        .next()
    });
    let child_log = child_dir(&spawn).join("log");
    wait_for("the first child's held run", || {
        let log = std::fs::read_to_string(&child_log).unwrap_or_default();
        events(&log, "run.start")
            .into_iter()
            .find(|e| e["workflow"] == "on-hold")
    });
    std::thread::sleep(Duration::from_millis(1000));
    drop(p1);
    let pid = spawn["pid"].as_i64().unwrap() as i32;
    wait_for("the first child to die with its parent", || {
        (unsafe { libc::kill(pid, 0) } != 0).then_some(Value::Null)
    });

    // The second parent's `inst-1` answers it, and is a daemon with nothing
    // in it the second parent did not send.
    let second = parent(
        "run_until: idle, idle_grace: 1500ms",
        "\x20     s:     { kind: once }\n\
         \x20     spawn: { kind: subagent, template: room, params: { id: second }, depends_on: [s] }\n\
         \x20     ping:  { kind: a2a.delegate, depends_on: [spawn], peer: \"{{steps.spawn.output.peer}}\", command: room.ping, args: {}, timeout: 10s, retry: { max: 6, backoff: 1s } }\n\
         \x20     st:    { kind: a2a.delegate, depends_on: [ping], peer: \"{{steps.spawn.output.peer}}\", command: status, args: {}, timeout: 10s }\n\
         \x20     f:     { kind: finish, depends_on: [st], status: completed, output: { pong: \"{{steps.ping.output}}\", runs: \"{{steps.st.output.runs}}\", pending: \"{{steps.st.output.inbox_pending}}\" } }\n",
    );
    let (code, log) = dirs.run("second", &second);
    assert_eq!(code, Some(0), "{log}");
    let spawned = events(&log, "instance.spawn");
    assert_eq!(spawned.len(), 1, "{log}");
    assert_eq!(spawned[0]["handle"], "inst-1", "{log}");
    let out = caller_output(&log);
    assert_eq!(out["pong"], "pong second", "{log}");
    let runs = value(&out["runs"]);
    let runs = runs.as_array().unwrap_or_else(|| panic!("{out}\n{log}"));
    assert!(
        runs.iter().all(|r| r["workflow"] != "on-hold"),
        "the child resumed the first parent's command: {runs:?}"
    );
    assert_eq!(value(&out["pending"]), 0, "{out}");
}

/// Two memory-store parents running at once, under one state directory and
/// one temp directory, each spawn an `inst-1` and each is answered by its own.
#[test]
fn concurrent_memory_parents_each_reach_their_own_child() {
    let dirs = Dirs::new("inst-concurrent");
    let cfg = |id: &str| {
        parent(
            "run_until: idle, idle_grace: 1500ms",
            &format!(
                "\x20     s:     {{ kind: once }}\n\
                 \x20     spawn: {{ kind: subagent, template: room, params: {{ id: {id} }}, depends_on: [s] }}\n\
                 \x20     nap:   {{ kind: sleep, depends_on: [spawn], duration: 2500ms }}\n\
                 \x20     ping:  {{ kind: a2a.delegate, depends_on: [nap], peer: \"{{{{steps.spawn.output.peer}}}}\", command: room.ping, args: {{}}, timeout: 10s, retry: {{ max: 6, backoff: 1s }} }}\n\
                 \x20     f:     {{ kind: finish, depends_on: [ping], status: completed, output: \"{{{{steps.ping.output}}}}\" }}\n"
            ),
        )
    };
    let (a_err, b_err) = (
        format!("{}/alpha.log", dirs.root),
        format!("{}/bravo.log", dirs.root),
    );
    let a = dirs.agentd("alpha", &cfg("alpha"), &a_err).spawn().unwrap();
    let b = dirs.agentd("bravo", &cfg("bravo"), &b_err).spawn().unwrap();
    let (a, b) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    let (a_log, b_log) = (
        std::fs::read_to_string(&a_err).unwrap_or_default(),
        std::fs::read_to_string(&b_err).unwrap_or_default(),
    );
    assert_eq!(a.status.code(), Some(0), "{a_log}");
    assert_eq!(b.status.code(), Some(0), "{b_log}");
    assert_eq!(caller_output(&a_log), "pong alpha", "{a_log}");
    assert_eq!(caller_output(&b_log), "pong bravo", "{b_log}");
    let socket = |log: &str| events(log, "instance.spawn")[0]["socket"].clone();
    assert_ne!(socket(&a_log), socket(&b_log));
}

/// A memory-store parent's children live in a directory of its own process
/// under the temp directory, never under the state directory: a child's own
/// directory goes when it is retired, and the parent's when it exits.
#[test]
fn a_memory_parents_children_go_with_it() {
    let dirs = Dirs::new("inst-private");
    let cfg = parent(
        "run_until: idle, idle_grace: 1500ms",
        "\x20     s:      { kind: once }\n\
         \x20     spawn:  { kind: subagent, template: room, params: { id: solo }, depends_on: [s] }\n\
         \x20     ping:   { kind: a2a.delegate, depends_on: [spawn], peer: \"{{steps.spawn.output.peer}}\", command: room.ping, args: {}, timeout: 10s, retry: { max: 6, backoff: 1s } }\n\
         \x20     retire: { kind: tool, depends_on: [ping], name: subagent.retire, args: { handle: \"{{steps.spawn.output.handle}}\" } }\n\
         \x20     nap:    { kind: sleep, depends_on: [retire], duration: 4s }\n\
         \x20     f:      { kind: finish, depends_on: [nap], status: completed, output: \"{{steps.ping.output}}\" }\n",
    );
    let err = format!("{}/solo.log", dirs.root);
    let mut p = dirs.agentd("solo", &cfg, &err).spawn().unwrap();
    let spawn = wait_for("the child", || {
        events(
            &std::fs::read_to_string(&err).unwrap_or_default(),
            "instance.spawn",
        )
        .into_iter()
        .next()
    });
    let child = child_dir(&spawn);
    let private = child.parent().unwrap().to_path_buf();
    assert_eq!(
        private.parent(),
        Some(Path::new(&dirs.tmp())),
        "the child is in a directory of the parent's own under TMPDIR: {spawn}"
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&private).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{}", private.display());
    }
    wait_for("the child's retirement", || {
        events(
            &std::fs::read_to_string(&err).unwrap_or_default(),
            "instance.exited",
        )
        .into_iter()
        .next()
    });
    assert!(!child.exists(), "the retired child's directory stays");
    assert!(private.exists(), "the parent's directory went before it");
    let status = p.wait().unwrap();
    let log = std::fs::read_to_string(&err).unwrap_or_default();
    assert_eq!(status.code(), Some(0), "{log}");
    assert_eq!(caller_output(&log), "pong solo", "{log}");
    assert!(
        !private.exists(),
        "the parent's directory outlived it: {}",
        private.display()
    );
    assert!(
        !Path::new(&dirs.state()).join("subagents").exists(),
        "a memory-store parent wrote its children under the state directory"
    );
}

/// A child whose directory is too deep for the address its listener binds
/// the socket at — though `a2a.sock` itself would be short enough — is given
/// a socket in the temp directory instead, and answers. One with no socket
/// short enough anywhere is refused at the spawn, naming TMPDIR.
#[test]
fn a_child_too_deep_for_its_socket_is_given_one_it_can_bind() {
    let dirs = Dirs::new("inst-deep");
    let cfg = parent("run_until: idle, idle_grace: 1500ms", PING);
    // A TMPDIR of `len` bytes. The child's directory is 41 bytes deeper,
    // `/agentd-<26-character id>/inst-1`.
    let tmp = |len: usize| {
        let base = dirs.tmp();
        assert!(base.len() < len, "{base} is too deep to pad");
        let p = format!("{base}/{}", "p".repeat(len - base.len() - 1));
        std::fs::create_dir_all(&p).unwrap();
        p
    };
    // An 88-byte directory: `a2a.sock` in it is 97 bytes, and the address
    // the child stages it at is over the 108 a socket address holds.
    let deep = tmp(47);
    let (code, log) = dirs.run_with("deep", &cfg, &[("TMPDIR", &deep)]);
    assert_eq!(code, Some(0), "{log}");
    let spawn = &events(&log, "instance.spawn")[0];
    let socket = Path::new(spawn["socket"].as_str().unwrap());
    assert_eq!(socket.parent(), Some(Path::new(&deep)), "{spawn}");
    assert_eq!(caller_output(&log), "pong solo", "{log}");

    let deeper = tmp(95);
    let (_, log) = dirs.run_with("deeper", &cfg, &[("TMPDIR", &deeper)]);
    assert!(events(&log, "instance.spawn").is_empty(), "{log}");
    let refused = events(&log, "instance.spawn.refused");
    assert_eq!(refused.len(), 1, "{log}");
    let reason = refused[0]["reason"].as_str().unwrap();
    assert!(reason.contains("TMPDIR"), "{reason}");
}

/// A child that fails says why in its own log, and its parent's
/// `instance.exited` carries that cause: here a durable child whose socket
/// path is taken by a directory, so its listener cannot publish there.
#[test]
fn a_failed_childs_cause_is_in_its_parents_log() {
    let dirs = Dirs::new("inst-cause");
    let cfg = format!(
        "agent: {{ name: parent }}\nstore: {{ kind: file }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1500ms }}\n\
         observability: {{ log_level: info, log_content: true }}\n{ROOM}\
         workflows:\n  - name: caller\n    steps:\n{SPAWN}"
    );
    let child = Path::new(&dirs.state()).join("subagents/file/agentd/parent/inst-1");
    std::fs::create_dir_all(child.join("a2a.sock")).unwrap();
    let (code, log) = dirs.run("cause", &cfg);
    assert_eq!(code, Some(0), "{log}");
    let exited = events(&log, "instance.exited");
    assert_eq!(exited.len(), 1, "{log}");
    assert_eq!(exited[0]["status"], "failed", "{log}");
    let cause = exited[0]["cause"]
        .as_str()
        .unwrap_or_else(|| panic!("no cause: {}", exited[0]));
    assert!(
        cause.starts_with("a2a listen:") && cause.contains("a2a.sock"),
        "{cause}"
    );
}

/// A memory-store parent's child that crashed keeps its directory, its log
/// in it, until the parent exits — its socket goes at once — and the
/// parent's directory still goes with the parent.
#[test]
fn a_crashed_childs_directory_stays_until_its_parent_exits() {
    let dirs = Dirs::new("inst-crash");
    let cfg = parent("run_until: idle, idle_grace: 1500ms", SPAWN);
    let err = format!("{}/crash.log", dirs.root);
    let mut p = dirs.agentd("crash", &cfg, &err).spawn().unwrap();
    let spawn = wait_for("the child", || {
        events(
            &std::fs::read_to_string(&err).unwrap_or_default(),
            "instance.spawn",
        )
        .into_iter()
        .next()
    });
    let child = child_dir(&spawn);
    let private = child.parent().unwrap().to_path_buf();
    wait_for("the child's listener", || {
        let log = std::fs::read_to_string(child.join("log")).unwrap_or_default();
        events(&log, "proc.ready").into_iter().next()
    });
    let pid = spawn["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let exited = wait_for("the child's exit", || {
        events(
            &std::fs::read_to_string(&err).unwrap_or_default(),
            "instance.exited",
        )
        .into_iter()
        .next()
    });
    assert_eq!(exited["status"], "crashed", "{exited}");
    assert!(child.join("log").exists(), "the crashed child's log went");
    assert!(!child.join("a2a.sock").exists(), "its socket stayed");
    let status = p.wait().unwrap();
    let log = std::fs::read_to_string(&err).unwrap_or_default();
    assert_eq!(status.code(), Some(0), "{log}");
    assert!(
        !private.exists(),
        "the parent's directory outlived it: {}",
        private.display()
    );
}

/// A memory-store parent keeps its children's directory in its user's
/// runtime directory only when that is the user's own, as the XDG
/// specification has it: a 0700 directory it owns is used, and `/proc`,
/// which is not, leaves the children in TMPDIR.
#[test]
fn a_memory_parent_uses_only_a_runtime_dir_of_its_users_own() {
    use std::os::unix::fs::PermissionsExt;
    let dirs = Dirs::new("inst-xdg");
    let cfg = parent("run_until: idle, idle_grace: 1500ms", PING);
    let run = format!("{}/run", dirs.root);
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (xdg, base) in [(run.clone(), run.clone()), ("/proc".into(), dirs.tmp())] {
        let (code, log) = dirs.run_with("xdg", &cfg, &[("XDG_RUNTIME_DIR", &xdg)]);
        assert_eq!(code, Some(0), "XDG_RUNTIME_DIR={xdg}: {log}");
        assert_eq!(caller_output(&log), "pong solo", "{log}");
        let spawn = &events(&log, "instance.spawn")[0];
        let private = child_dir(spawn).parent().unwrap().to_path_buf();
        assert_eq!(
            private.parent(),
            Some(Path::new(&base)),
            "XDG_RUNTIME_DIR={xdg}: {spawn}"
        );
    }
}
