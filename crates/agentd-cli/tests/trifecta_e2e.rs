// SPDX-License-Identifier: AGPL-3.0-only
//! **Streams carry the taint of what feeds them** (RFC 0045 §5.11.3), end to
//! end.
//!
//! The root-grant fold judges servers: a config whose servers are all tagged,
//! one of them `sensitive` + `egress`, passes it — no server reads untrusted
//! input. But a webhook or an A2A peer can append to a stream with `into:`, and
//! the workflow consuming that stream hands the appended text to an agent. That
//! is the whole trifecta in one run, and nothing judged it. Now every stream is
//! tainted by what the configuration lets feed it, and a workflow reading a
//! tainted stream into an agent that reaches `sensitive` + `egress` is refused
//! at load — by `--validate-config`, at startup, and on a reload, which keeps
//! the running configuration.
#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;

/// A sensitive + egress server — the two legs the root fold allows together —
/// and the stream an outside edge can append to. The root reads no run's
/// result back: holding `mail`, it would otherwise be refused beside every
/// workflow these tests judge.
const BASE: &str = "agent:\n  name: taint\n  instruction: Triage the inbox.\n\
     \x20 on_workflow_finished: ignore\n  tools: {internal: [workflow.create]}\n\
     store: {kind: memory}\n\
     mcp:\n  servers:\n\
     \x20   - {name: mail, endpoint: \"https://mail.invalid/mcp\", tags: {\"*\": [sensitive, egress]}}\n\
     \x20   - {name: notes, endpoint: \"https://notes.invalid/mcp\", tags: {\"*\": [sensitive]}}\n\
     streams:\n  inbox: {}\n";

/// The edges that append outside input to `inbox`.
const WEBHOOK_INTO: &str = "  - name: intake\n    steps:\n      hook: {kind: webhook, path: /in, into: {stream: inbox, subject: msg}}\n";
const A2A_INTO: &str = "  - name: intake\n    steps:\n      hook: {kind: a2a, command: note, into: {stream: inbox, subject: msg}}\n";

/// A consumer of `inbox` whose agent is handed `server`.
fn triage(server: &str) -> String {
    format!(
        "  - name: triage\n    steps:\n      s: {{kind: stream, stream: inbox}}\n      \
         a: {{kind: agent, depends_on: [s], instruction: \"Read it.\", servers: [{server}], tools: [\"{server}.*\"]}}\n      \
         f: {{kind: finish, depends_on: [a], status: completed}}\n"
    )
}

/// A config with both listeners on fixed ports — `--validate-config` binds
/// neither.
fn config(workflows: &[&str], extra: &str) -> String {
    format!(
        "{BASE}webhooks:\n  listen: http://127.0.0.1:9\n\
         a2a:\n  listen: http://127.0.0.1:9\n\
         workflows:\n{}{extra}",
        workflows.concat()
    )
}

/// Run agentd with a scrubbed environment; `(exit code, stderr)`.
fn agentd(args: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    for (k, _) in std::env::vars() {
        if k.starts_with("AGENTD_") || k.starts_with("AGENT_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd.args(args).output().expect("spawn agentd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn validate(dir: &Path, body: &str) -> (i32, String) {
    let cfg = dir.join("agent.yaml");
    std::fs::write(&cfg, body).unwrap();
    agentd(&["--config", &cfg.to_string_lossy(), "--validate-config"])
}

/// The refusal names everything the operator has to look at: the workflow,
/// the stream, what feeds it, and the servers that brought the other legs.
fn assert_names(err: &str, producer: &str) {
    for want in [
        "lethal-trifecta refused",
        "workflow \\\"triage\\\"",
        "stream \\\"inbox\\\"",
        producer,
        "mcp server \\\"mail\\\" [sensitive, egress]",
    ] {
        assert!(
            err.contains(want),
            "the refusal does not name {want:?}:\n{err}"
        );
    }
}

#[test]
fn a_webhook_into_fed_workflow_handing_an_agent_sensitive_and_egress_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let (code, err) = validate(t.path(), &config(&[WEBHOOK_INTO, &triage("mail")], ""));
    assert_eq!(code, 2, "{err}");
    assert_names(
        &err,
        "webhook `into:` at workflow \\\"intake\\\" step \\\"hook\\\"",
    );
    // The control: the same stream into an agent holding `sensitive` alone is
    // two legs, and loads. Without it this would pass for a check that refused
    // every `into:`.
    let (code, err) = validate(t.path(), &config(&[WEBHOOK_INTO, &triage("notes")], ""));
    assert_eq!(code, 0, "{err}");
}

#[test]
fn an_a2a_into_fed_workflow_is_refused_the_same_way() {
    let t = tempfile::tempdir().unwrap();
    let (code, err) = validate(t.path(), &config(&[A2A_INTO, &triage("mail")], ""));
    assert_eq!(code, 2, "{err}");
    assert_names(
        &err,
        "A2A `into:` at workflow \\\"intake\\\" step \\\"hook\\\"",
    );
}

#[test]
fn the_explicit_allow_loads_it() {
    let t = tempfile::tempdir().unwrap();
    let (code, err) = validate(
        t.path(),
        &config(
            &[WEBHOOK_INTO, &triage("mail")],
            "security:\n  allow_trifecta: true\n",
        ),
    );
    assert_eq!(code, 0, "{err}");
}

#[test]
fn a_stream_nothing_outside_feeds_loads() {
    let t = tempfile::tempdir().unwrap();
    // A schedule emits into `inbox`: agentd's own clock, no outside input.
    let tick = "  - name: tick\n    steps:\n      s: {kind: schedule, every: 1m}\n      \
                e: {kind: emit, depends_on: [s], stream: inbox, subject: t}\n      \
                f: {kind: finish, depends_on: [e], status: completed}\n";
    let (code, err) = validate(t.path(), &config(&[tick, &triage("mail")], ""));
    assert_eq!(code, 0, "{err}");
}

/// A consumer defined in a local FILE is read by `--validate-config` and
/// refused there, and the start refuses it too.
#[test]
fn a_file_defined_consumer_refuses_the_start() {
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join("triage.yaml");
    std::fs::write(
        &file,
        "name: triage\nsteps:\n  s: {kind: stream, stream: inbox}\n  \
         a: {kind: agent, depends_on: [s], instruction: \"Read it.\", servers: [mail], tools: [\"mail.*\"]}\n  \
         f: {kind: finish, depends_on: [a], status: completed}\n",
    )
    .unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(
        &cfg,
        format!(
            "{BASE}lifecycle: {{run_until: drained}}\n\
             webhooks:\n  listen: http://127.0.0.1:{}\n\
             workflows:\n{WEBHOOK_INTO}  - name: triage\n    file: {}\n",
            common::free_port(),
            file.display()
        ),
    )
    .unwrap();
    let (code, err) = agentd(&["--config", &cfg.to_string_lossy(), "--validate-config"]);
    assert_eq!(code, 2, "{err}");
    assert_names(
        &err,
        "webhook `into:` at workflow \\\"intake\\\" step \\\"hook\\\"",
    );
    // A start that is NOT refused serves its webhook until stopped, so it is
    // given a deadline rather than waited on.
    let log = t.path().join("daemon.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg.to_string_lossy()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("spawn agentd");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code().unwrap_or(-1);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the start was not refused:\n{}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    };
    let err = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(code, 2, "{err}");
    // Refused while the configuration loads, so it is the plain usage line.
    assert!(names_all(&err, "triage", "mail"), "{err}");
}

/// A daemon whose stderr log a test reads, stopped on drop.
#[cfg(feature = "hot-reload")]
struct Daemon {
    child: std::process::Child,
    log: std::path::PathBuf,
}

#[cfg(feature = "hot-reload")]
impl Daemon {
    fn spawn(cfg: &Path) -> Daemon {
        let log = cfg.with_extension("log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg.to_string_lossy()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("spawn daemon");
        Daemon { child, log }
    }
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn events(&self, name: &str) -> Vec<serde_json::Value> {
        self.log()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["event"] == name)
            .collect()
    }
    fn wait_for(&self, pred: impl Fn(&Daemon) -> bool, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !pred(self) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}:\n{}",
                self.log()
            );
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    fn sighup(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
    }
}

#[cfg(feature = "hot-reload")]
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while std::time::Instant::now() < deadline && !matches!(self.child.try_wait(), Ok(Some(_)))
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a refusal — the start's, or a `config.reload.invalid` /
/// `workflow.stored.invalid` line or tool reply — names the workflow, the
/// stream, the producer and the server. Read as written: a refusal carried
/// inside another message (a reload's error quotes the load's) has its quotes
/// escaped once more.
fn names_all(m: &str, workflow: &str, server: &str) -> bool {
    let m = m.replace("\\\"", "\"");
    m.contains(&format!("workflow \"{workflow}\""))
        && m.contains("stream \"inbox\"")
        && m.contains("webhook `into:` at workflow \"intake\"")
        && m.contains(&format!("mcp server \"{server}\""))
}

/// A reload that gives the consumer's server the `egress` leg completes the
/// trifecta, and is refused; restoring the original then reloads to
/// "nothing", so no section of the refused reload had applied.
#[cfg(feature = "hot-reload")]
#[test]
fn a_reload_introducing_the_path_is_refused_and_the_running_config_stays() {
    let a = common::spawn_mock_mcp("mock://a", false);
    let t = tempfile::tempdir().unwrap();
    // The consumer lives in a file: the refusal then comes from the loader's
    // check over the resolved set, which a reload runs too.
    let file = t.path().join("triage.yaml");
    std::fs::write(
        &file,
        "name: triage\nsteps:\n  s: {kind: stream, stream: inbox}\n  \
         a: {kind: agent, depends_on: [s], instruction: \"Read it.\", servers: [a], tools: [\"a.*\"]}\n  \
         f: {kind: finish, depends_on: [a], status: completed}\n",
    )
    .unwrap();
    let body = |port: u16, tags: &str| {
        format!(
            "agent:\n  name: taint\n  instruction: Triage the inbox.\n\
             store: {{kind: memory}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: info\n\
             mcp:\n  servers:\n    - {{name: a, endpoint: \"{}\", ns: a, tags: {{\"*\": [{tags}]}}}}\n\
             webhooks:\n  listen: http://127.0.0.1:{port}\n\
             streams:\n  inbox: {{}}\n\
             workflows:\n{WEBHOOK_INTO}  - name: triage\n    file: {}\n",
            a.uri(),
            file.display()
        )
    };
    let cfg = t.path().join("agent.yaml");
    // The port is the one the daemon reports binding, not a probe: another
    // test can take a probed port before the daemon binds it, and a daemon
    // that lost that race is retried on a fresh one.
    let (d, bound) = common::spawn_listener_bound("webhooks.listen", |port| {
        std::fs::write(&cfg, body(port, "sensitive")).unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.log.to_string_lossy().into_owned();
        (d, log)
    });
    let port: u16 = bound
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in {bound}"));
    let original = body(port, "sensitive");
    d.wait_for(
        |d| {
            d.events("workflow.loaded")
                .iter()
                .any(|e| e["name"] == "triage")
        },
        "the consumer to load",
    );

    // `egress` joins the consumer's server: the trifecta is complete.
    std::fs::write(&cfg, body(port, "sensitive, egress")).unwrap();
    d.sighup();
    d.wait_for(
        |d| !d.events("config.reload.invalid").is_empty(),
        "the refusal",
    );
    assert!(
        d.events("config.reload.invalid").iter().any(|e| e["error"]
            .as_str()
            .is_some_and(|m| names_all(m, "triage", "a") && m.contains("[sensitive, egress]"))),
        "the refusal does not name the workflow, stream, producer and server:\n{}",
        d.log()
    );
    assert!(d.events("config.reloaded").is_empty(), "{}", d.log());

    // The running config stayed: putting the original back changes nothing.
    std::fs::write(&cfg, &original).unwrap();
    d.sighup();
    d.wait_for(
        |d| !d.events("config.reloaded").is_empty(),
        "the restoring reload",
    );
    assert_eq!(
        d.events("config.reloaded")[0]["changed"],
        serde_json::json!(["nothing"]),
        "the refused reload had applied:\n{}",
        d.log()
    );
}

/// A definition the agent writes is held to the same line: `workflow.create`
/// refuses one that would read the tainted stream into sensitive + egress,
/// and one it stored while that was two legs is left out — said out loud —
/// by the reload that adds the third, without vetoing the operator's reload.
#[cfg(feature = "hot-reload")]
#[test]
fn an_agent_written_definition_is_held_to_the_same_check() {
    let t = tempfile::tempdir().unwrap();
    let play = t.path().join("play.json");
    let def = |name: &str, servers: &[&str]| {
        serde_json::json!({"name": name, "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "Read it.",
                  "servers": servers, "tools": []},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}}})
    };
    let create = |d: serde_json::Value| serde_json::json!({"tool_calls": [{"name": "workflow.create", "arguments": {"definition": d}}]});
    std::fs::write(
        &play,
        serde_json::json!({"turns": [
            create(def("reader", &["a"])),
            create(def("leaker", &["a", "b"])),
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let port = common::free_port();
    let body = |a_tags: &str| {
        format!(
            "agent:\n  name: taint\n  prompt: go\n  instruction: Triage the inbox.\n\
             \x20 on_workflow_finished: ignore\n  tools: {{internal: [workflow.create]}}\n\
             intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
             store: {{kind: memory}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: info\n  log_content: true\n\
             mcp:\n  servers:\n\
             \x20   - {{name: a, endpoint: \"https://a.invalid/mcp\", tags: {{\"*\": [{a_tags}]}}}}\n\
             \x20   - {{name: b, endpoint: \"https://b.invalid/mcp\", tags: {{\"*\": [egress]}}}}\n\
             webhooks:\n  listen: http://127.0.0.1:{port}\n\
             streams:\n  inbox: {{}}\n\
             workflows:\n{WEBHOOK_INTO}",
            play.display()
        )
    };
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, body("sensitive")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|d| !d.events("turn.reply").is_empty(), "the agent's turn");
    let defined: Vec<_> = d
        .events("workflow.defined")
        .iter()
        .map(|e| e["name"].clone())
        .collect();
    assert_eq!(defined, [serde_json::json!("reader")], "{}", d.log());
    assert!(
        d.events("turn.reply").iter().any(|e| e["text"]
            .as_str()
            .is_some_and(|m| names_all(m, "leaker", "b"))),
        "the refusal did not say why:\n{}",
        d.log()
    );

    // `a` gains `egress`: the stored `reader` now completes the trifecta. The
    // reload applies — a stored definition cannot veto it — and `reader` is
    // left out, naming why.
    std::fs::write(&cfg, body("sensitive, egress")).unwrap();
    d.sighup();
    d.wait_for(|d| !d.events("config.reloaded").is_empty(), "the reload");
    assert!(
        d.events("workflow.stored.invalid")
            .iter()
            .any(|e| e["name"] == "reader"
                && e["errors"].as_array().is_some_and(|es| es
                    .iter()
                    .any(|m| m.as_str().is_some_and(|m| names_all(m, "reader", "a"))))),
        "{}",
        d.log()
    );
}

/// A reload that changes only the subagent templates still re-checks the
/// definitions: a template that starts mirroring a child's stream into
/// `inbox` taints it, and the stored consumer reading `inbox` into
/// sensitive + egress is left out by that reload.
#[cfg(all(feature = "hot-reload", feature = "a2a"))]
#[test]
fn a_reload_that_only_adds_a_mirror_rechecks_the_stored_definitions() {
    let t = tempfile::tempdir().unwrap();
    let play = t.path().join("play.json");
    std::fs::write(
        &play,
        serde_json::json!({"turns": [
            {"tool_calls": [{"name": "workflow.create", "arguments": {"definition": {
                "name": "reader", "steps": {
                    "s": {"kind": "stream", "stream": "inbox"},
                    "a": {"kind": "agent", "depends_on": ["s"], "instruction": "Read it.",
                          "servers": ["mail"], "tools": []},
                    "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}}}}}]},
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let a2a = common::free_port();
    let body = |templates: &str| {
        format!(
            "agent:\n  name: taint\n  prompt: go\n  instruction: Triage the inbox.\n\
             intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
             store: {{kind: memory}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: info\n\
             a2a: {{ listen: \"http://127.0.0.1:{a2a}\" }}\n\
             mcp:\n  servers:\n\
             \x20   - {{name: mail, endpoint: \"https://mail.invalid/mcp\", tags: {{\"*\": [sensitive, egress]}}}}\n\
             streams:\n  inbox: {{}}\n\
             workflows:\n\
             \x20 - name: tick\n    steps:\n      s: {{kind: schedule, every: 1h}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\n\
             {templates}",
            play.display()
        )
    };
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, body("")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|d| !d.events("turn.reply").is_empty(), "the agent's turn");
    assert!(
        d.events("workflow.defined")
            .iter()
            .any(|e| e["name"] == "reader"),
        "an untainted stream into sensitive + egress is two legs:\n{}",
        d.log()
    );

    std::fs::write(
        &cfg,
        body(
            "subagents:\n  templates:\n    desk:\n      instruction: |\n        The desk.\n        \
             :::!stream{name=inbox}\n        retention: { max_events: 10 }\n        :::\n      \
             mirror_streams: [inbox]\n",
        ),
    )
    .unwrap();
    d.sighup();
    d.wait_for(|d| !d.events("config.reloaded").is_empty(), "the reload");
    assert!(
        d.events("workflow.stored.invalid")
            .iter()
            .any(|e| e["name"] == "reader"
                && e["errors"].as_array().is_some_and(|es| es.iter().any(|m| m
                    .as_str()
                    .is_some_and(|m| m.contains("mirror_streams of subagent template \"desk\""))))),
        "{}",
        d.log()
    );
}

/// A reload that changes only the service catalog still re-checks the
/// definitions: an instance template's servers resolve against it, so a
/// catalog entry gaining `egress` hands a stored consumer that spawns the
/// template the other leg — and that reload leaves it out.
#[cfg(all(feature = "hot-reload", feature = "a2a"))]
#[test]
fn a_reload_that_only_retags_a_service_rechecks_the_stored_definitions() {
    let t = tempfile::tempdir().unwrap();
    let play = t.path().join("play.json");
    std::fs::write(
        &play,
        serde_json::json!({"turns": [
            {"tool_calls": [{"name": "workflow.create", "arguments": {"definition": {
                "name": "reader", "steps": {
                    "s": {"kind": "stream", "stream": "inbox"},
                    "c": {"kind": "subagent", "depends_on": ["s"], "template": "desk"},
                    "f": {"kind": "finish", "depends_on": ["c"], "status": "completed"}}}}}]},
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let a2a = common::free_port();
    let body = |tags: &str| {
        format!(
            "agent:\n  name: taint\n  prompt: go\n  instruction: Triage the inbox.\n\
             intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
             store: {{kind: memory}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: info\n\
             a2a: {{ listen: \"http://127.0.0.1:{a2a}\" }}\n\
             services:\n  crm: {{kind: mcp, endpoint: \"https://crm.invalid/mcp\", tags: {{\"*\": [{tags}]}}}}\n\
             streams:\n  inbox: {{}}\n\
             subagents:\n  templates:\n    desk:\n      instruction: |\n        The desk.\n        \
             :::!stream{{name=inbox}}\n        retention: {{ max_events: 10 }}\n        :::\n        \
             :::!mcp{{name=crm}}\n        service: crm\n        :::\n      \
             mirror_streams: [inbox]\n\
             workflows:\n\
             \x20 - name: tick\n    steps:\n      s: {{kind: schedule, every: 1h}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\n",
            play.display()
        )
    };
    let cfg = t.path().join("agent.yaml");
    std::fs::write(&cfg, body("sensitive")).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(|d| !d.events("turn.reply").is_empty(), "the agent's turn");
    assert!(
        d.events("workflow.defined")
            .iter()
            .any(|e| e["name"] == "reader"),
        "a mirrored stream into a sensitive service is two legs:\n{}",
        d.log()
    );

    std::fs::write(&cfg, body("sensitive, egress")).unwrap();
    d.sighup();
    d.wait_for(|d| !d.events("config.reloaded").is_empty(), "the reload");
    assert!(
        d.events("workflow.stored.invalid")
            .iter()
            .any(|e| e["name"] == "reader"
                && e["errors"].as_array().is_some_and(|es| es.iter().any(|m| m
                    .as_str()
                    .is_some_and(|m| m.contains("mcp server \"crm\" of template \"desk\""))))),
        "{}",
        d.log()
    );
}

/// The tools that hand text on are the steps that do, spelled as tools: a
/// reader narrowed to `notes` that can call `workflow.run` starts the run
/// holding `mail`, and one that can call `message.send` hands the text to the
/// root grant. Both are refused; without them the reader loads.
#[test]
fn a_reader_holding_a_tool_that_hands_text_on_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let send = "  - name: send\n    steps:\n      s: {kind: manual}\n      \
                a: {kind: agent, depends_on: [s], instruction: \"Send it.\", servers: [mail], tools: [\"mail.*\"]}\n      \
                f: {kind: finish, depends_on: [a], status: completed}\n";
    let reader = |extra: &str| {
        format!(
            "  - name: triage\n    steps:\n      s: {{kind: stream, stream: inbox}}\n      \
             a: {{kind: agent, depends_on: [s], instruction: \"Read it.\", servers: [notes], tools: [\"notes.*\"{extra}]}}\n      \
             f: {{kind: finish, depends_on: [a], status: completed}}\n"
        )
    };
    let (code, err) = validate(t.path(), &config(&[WEBHOOK_INTO, &reader(""), send], ""));
    assert_eq!(code, 0, "{err}");
    for tool in ["workflow.run", "message.send"] {
        let (code, err) = validate(
            t.path(),
            &config(&[WEBHOOK_INTO, &reader(&format!(", \"{tool}\"")), send], ""),
        );
        assert_eq!(code, 2, "{tool}: {err}");
        assert!(err.contains("lethal-trifecta refused"), "{tool}: {err}");
    }
}

// ---- results read back (RFC 0045 §5.11.3, "follow it") --------------------

/// `fetch` reads `inbox` into an agent holding `notes` alone — two legs, and
/// allowed — and is a sync tool, so its reply is what it read, worked over.
/// Not granted to the root, which these tests keep from reading back.
const FETCH: &str = "  - name: fetch\n    tool: {name: inbox.next, grant: {root: false}}\n    steps:\n      \
     s: {kind: manual}\n      \
     w: {kind: wait, depends_on: [s], on: event, stream: inbox}\n      \
     a: {kind: agent, depends_on: [w], instruction: \"Read it.\", servers: [notes], tools: [\"notes.*\"]}\n      \
     f: {kind: finish, depends_on: [a], status: completed}\n";

/// [`FETCH`], handing what it read to a child, whose result is that text.
const FETCH_SPAWNS: &str = "  - name: fetch\n    steps:\n      \
     s: {kind: manual}\n      \
     w: {kind: wait, depends_on: [s], on: event, stream: inbox}\n      \
     c: {kind: subagent, depends_on: [w], mode: async, instruction: \"Read it.\", servers: [notes], tools: [\"notes.*\"]}\n      \
     f: {kind: finish, depends_on: [c], status: completed}\n";

/// `workflows` refused at load naming the caller, its read of `fetch` and
/// `edge`, and loaded under the explicit allow.
fn refused_and_allowed(dir: &Path, workflows: &[&str], edge: &str) {
    let (code, err) = validate(dir, &config(workflows, ""));
    assert_eq!(code, 2, "{edge}: {err}");
    // An edge that may read any workflow names every tainted one it reads,
    // together.
    for want in [
        "workflow \\\"caller\\\": lethal-trifecta refused",
        "it reads back the result of workflow",
        "\\\"fetch\\\"",
        edge,
        "mcp server \\\"mail\\\" [sensitive, egress]",
    ] {
        assert!(err.contains(want), "{edge}: no {want:?} in\n{err}");
    }
    let (code, err) = validate(
        dir,
        &config(workflows, "security:\n  allow_trifecta: true\n"),
    );
    assert_eq!(code, 0, "{edge}: {err}");
}

/// A manually started workflow whose step `r` reads something back before
/// an agent holding `mail` — both other legs — acts.
fn caller(read: &str) -> String {
    format!(
        "  - name: caller\n    steps:\n      s: {{kind: manual}}\n      \
         r: {{depends_on: [s], {read}}}\n      \
         a: {{kind: agent, depends_on: [r], instruction: \"Act.\", servers: [mail], tools: [\"mail.*\"]}}\n      \
         f: {{kind: finish, depends_on: [a], status: completed}}\n"
    )
}

/// Every spelling of a result read back makes its caller carry the text the
/// run was handed: refused at load naming the edge, and loaded under the
/// explicit allow.
#[test]
fn a_caller_reading_back_a_tainted_result_is_refused_through_each_path() {
    let t = tempfile::tempdir().unwrap();
    for (read, edge) in [
        (
            "kind: agent, instruction: \"Look.\", servers: [], tools: [inbox.next]",
            "(through its tool \\\"inbox.next\\\")",
        ),
        ("kind: workflow, name: fetch", "step \\\"r\\\""),
        (
            "kind: tool, name: workflow.run, args: {name: fetch, wait: true}",
            "step \\\"r\\\"",
        ),
        (
            "kind: agent, instruction: \"Look.\", servers: [], tools: [workflow.run]",
            "(through workflow.run)",
        ),
        (
            "kind: workflow.wait, run: \"{{inputs.run}}\"",
            "step \\\"r\\\"",
        ),
        (
            "kind: wait, on: run, run: \"{{inputs.run}}\"",
            "step \\\"r\\\"",
        ),
        (
            "kind: agent, instruction: \"Look.\", servers: [], tools: [workflow.wait]",
            "(through workflow.wait)",
        ),
        (
            "kind: agent, instruction: \"Look.\", servers: [], tools: [workflow.status]",
            "(through workflow.status)",
        ),
        // A plan item bound to a run is settled with its output.
        (
            "kind: agent, instruction: \"Look.\", servers: [], tools: [plan.update]",
            "(through plan.update)",
        ),
        (
            "kind: tool, name: plan.update, args: {item: 1, bind: {run: \"{{inputs.run}}\"}}",
            "step \\\"r\\\"",
        ),
    ] {
        refused_and_allowed(t.path(), &[WEBHOOK_INTO, FETCH, &caller(read)], edge);
    }
    // A `join` reads back the run a `workflow.run` without `wait` started —
    // which hands back only its id.
    let join = |r: &str| {
        format!(
            "  - name: caller\n    steps:\n      s: {{kind: manual}}\n      \
             r0: {{kind: tool, depends_on: [s], name: workflow.run, args: {{name: fetch}}}}\n      \
             r: {{depends_on: [r0], {r}}}\n      \
             a: {{kind: agent, depends_on: [r], instruction: \"Act.\", servers: [mail], tools: [\"mail.*\"]}}\n      \
             f: {{kind: finish, depends_on: [a], status: completed}}\n"
        )
    };
    refused_and_allowed(
        t.path(),
        &[
            WEBHOOK_INTO,
            FETCH,
            &join("kind: join, handles: [\"{{steps.r0.output.run}}\"]"),
        ],
        "step \\\"r\\\"",
    );
    let (code, err) = validate(
        t.path(),
        &config(
            &[WEBHOOK_INTO, FETCH, &join("kind: sleep, duration: 1s")],
            "",
        ),
    );
    assert_eq!(code, 0, "{err}");
    // A child's result read back by handle is its spawner's.
    for tool in ["subagent.status", "subagent.await", "subagent.list"] {
        refused_and_allowed(
            t.path(),
            &[
                WEBHOOK_INTO,
                FETCH_SPAWNS,
                &caller(&format!(
                    "kind: agent, instruction: \"Look.\", servers: [], tools: [{tool}]"
                )),
            ],
            &format!("(through {tool})"),
        );
    }
    // The control: a child it never reads back hands it nothing.
    let (code, err) = validate(
        t.path(),
        &config(
            &[
                WEBHOOK_INTO,
                FETCH,
                &caller("kind: workflow, name: fetch, mode: detached"),
            ],
            "",
        ),
    );
    assert_eq!(code, 0, "{err}");
}

/// The root conversation reads results back too — by default, the note a
/// failed run leaves in its transcript — and holding `mail` it is refused,
/// naming the edge; a root that reads nothing back loads.
#[test]
fn the_root_reading_back_a_tainted_result_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let quiet = config(&[WEBHOOK_INTO, FETCH], "");
    let (code, err) = validate(t.path(), &quiet);
    assert_eq!(code, 0, "{err}");
    let (code, err) = validate(
        t.path(),
        &quiet.replace("  on_workflow_finished: ignore\n", ""),
    );
    assert_eq!(code, 2, "{err}");
    for want in [
        "the root conversation: lethal-trifecta refused",
        "it reads back the result of workflow \\\"fetch\\\"",
        "(the note `agent.on_workflow_finished` writes into its transcript)",
        "mcp server \\\"mail\\\" [sensitive, egress]",
    ] {
        assert!(err.contains(want), "no {want:?} in\n{err}");
    }
}

/// A reload that changes only the workflows re-derives what the workflow
/// tools carry. A new producer taints the stream `fetch` reads; the registry
/// is kept (no tool or server moved), and before, so were the tags derived
/// from the set that had no producer.
#[cfg(all(feature = "hot-reload", feature = "a2a"))]
#[test]
fn a_workflow_only_reload_retags_the_workflow_tools() {
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let body = |port: u16, workflows: &str| {
        format!(
            "agent:\n  name: taint\n  instruction: Triage the inbox.\n\
             store: {{kind: memory}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: info\n\
             a2a: {{ listen: \"http://127.0.0.1:{port}\" }}\n\
             streams:\n  inbox: {{}}\n\
             workflows:\n{FETCH}{workflows}"
        )
    };
    let (d, bound) = common::spawn_bound(|port| {
        std::fs::write(&cfg, body(port, "")).unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.log.to_string_lossy().into_owned();
        (d, log)
    });
    let port: u16 = bound
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in {bound}"));
    let tags = |d: &Daemon| -> Vec<serde_json::Value> {
        d.events("registry.workflow_tools")
            .iter()
            .filter(|e| e["tool"] == "inbox.next")
            .map(|e| e["tags"].clone())
            .collect()
    };
    assert_eq!(tags(&d), [serde_json::json!([])], "{}", d.log());

    std::fs::write(&cfg, body(port, A2A_INTO)).unwrap();
    d.sighup();
    d.wait_for(|d| !d.events("config.reloaded").is_empty(), "the reload");
    assert_eq!(
        tags(&d)[1..],
        [serde_json::json!(["untrusted_input"])],
        "{}",
        d.log()
    );
}

/// A reload that replaces a definition whose run is still live keeps that
/// run's taint in what the tools carry until the run lands: its result is
/// still there to read back. The old `fetch` takes an A2A peer's text
/// directly; the new one does not, so the set the reload stages alone would
/// leave `inbox.next` untainted while the old run still waits. So does a
/// restart, which restores the run pinned to the old definition.
#[cfg(all(feature = "hot-reload", feature = "a2a"))]
#[test]
fn a_reload_keeps_a_retired_definitions_taint_while_its_run_is_live() {
    let t = tempfile::tempdir().unwrap();
    let cfg = t.path().join("agent.yaml");
    let state = t.path().join("state");
    let fetch = |peer: &str| {
        format!(
            "  - name: fetch\n    tool: {{name: inbox.next, grant: {{root: false}}}}\n    steps:\n{peer}      \
             s: {{kind: manual}}\n      \
             w: {{kind: wait, depends_on: [s], on: signal, signal: go}}\n      \
             f: {{kind: finish, depends_on: [w], status: completed}}\n"
        )
    };
    let body = |port: u16, workflow: &str| {
        format!(
            "agent:\n  name: taint\n  instruction: Triage the inbox.\n\
             store: {{kind: file, file: {{path: {}}}}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: info\n\
             a2a: {{ listen: \"http://127.0.0.1:{port}\" }}\n\
             workflows:\n{workflow}",
            state.display()
        )
    };
    let peer = "      peer: {kind: a2a, command: poke}\n";
    let (d, bound) = common::spawn_bound(|port| {
        std::fs::write(&cfg, body(port, &fetch(peer))).unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.log.to_string_lossy().into_owned();
        (d, log)
    });
    let addr = common::wait_a2a_bound(&d.log.to_string_lossy());
    let port: u16 = bound
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in {bound}"));
    let last = |d: &Daemon| -> serde_json::Value {
        d.events("registry.workflow_tools")
            .iter()
            .rfind(|e| e["tool"] == "inbox.next")
            .map(|e| e["tags"].clone())
            .unwrap_or_default()
    };
    let tainted = serde_json::json!(["untrusted_input"]);
    assert_eq!(last(&d), tainted, "{}", d.log());
    let started =
        common::SendMessage::command("workflow.run", serde_json::json!({"workflow": "fetch"}))
            .return_immediately()
            .post(&addr);
    assert!(started.get("error").is_none(), "{started}");
    d.wait_for(
        |d| d.events("step.start").iter().any(|e| e["step"] == "w"),
        "the run waiting",
    );

    std::fs::write(&cfg, body(port, &fetch(""))).unwrap();
    d.sighup();
    d.wait_for(
        |d| !d.events("workflow.retiring").is_empty(),
        "the retirement",
    );
    assert_eq!(last(&d), tainted, "{}", d.log());

    // Restarted on the new definition, the run comes back pinned to the old.
    drop(d);
    let (d, _) = common::spawn_bound(|port| {
        std::fs::write(&cfg, body(port, &fetch(""))).unwrap();
        let d = Daemon::spawn(&cfg);
        let log = d.log.to_string_lossy().into_owned();
        (d, log)
    });
    let addr = common::wait_a2a_bound(&d.log.to_string_lossy());
    d.wait_for(
        |d| !d.events("workflow.pin_restored").is_empty(),
        "the restored pin",
    );
    assert_eq!(last(&d), tainted, "{}", d.log());

    let sent = common::SendMessage::command("workflow.signal", serde_json::json!({"name": "go"}))
        .post(&addr);
    assert!(sent.get("error").is_none(), "{sent}");
    d.wait_for(
        |d| last(d) == serde_json::json!([]),
        "the taint to go with the run",
    );
}

/// A definition the agent writes moves the workflow tools' tags too: one
/// that appends an A2A peer's text to `inbox` taints what `fetch`'s tool
/// returns, though it is no tool itself. Re-derived at the write, as a
/// reload re-derives them — and at the delete, which takes the taint away.
#[test]
fn a_definition_the_agent_writes_retags_the_workflow_tools() {
    let t = tempfile::tempdir().unwrap();
    let play = t.path().join("play.json");
    std::fs::write(
        &play,
        serde_json::json!({"turns": [
            {"tool_calls": [{"name": "workflow.create", "arguments": {"definition": {
                "name": "feed", "steps": {
                    "hook": {"kind": "a2a", "command": "note",
                             "into": {"stream": "inbox", "subject": "msg"}}}}}}]},
            {"tool_calls": [{"name": "workflow.delete", "arguments": {"name": "feed"}}]},
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: taint\n  prompt: go\n  instruction: Triage the inbox.\n\
             intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
             store: {{kind: memory}}\n\
             lifecycle: {{run_until: idle, idle_grace: 300ms}}\n\
             observability:\n  log_level: info\n\
             streams:\n  inbox: {{}}\n\
             workflows:\n{FETCH}",
            play.display()
        ),
    )
    .unwrap();
    let (code, err) = agentd(&["--config", &cfg.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    let tags: Vec<serde_json::Value> = err
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == "registry.workflow_tools" && v["tool"] == "inbox.next")
        .map(|v| v["tags"].clone())
        .collect();
    assert_eq!(
        tags,
        [
            serde_json::json!([]),
            serde_json::json!(["untrusted_input"]),
            serde_json::json!([])
        ],
        "{err}"
    );
}

/// A retired definition's live runs keep their taint in the read-back
/// contracts until the last of them lands. `hold` takes an A2A peer's text
/// directly, so `workflow.status` — which may return its run's output —
/// carries `untrusted_input`, and a rule on that tag denies it. Deleting
/// `hold` while a run of it still waits keeps the tag (the run's result is
/// still readable); the run finishing takes it away.
#[test]
fn a_retired_definitions_live_runs_keep_the_read_back_taint() {
    let t = tempfile::tempdir().unwrap();
    let play = t.path().join("play.json");
    let call = |name: &str, args: serde_json::Value| serde_json::json!({"tool_calls": [{"name": name, "arguments": args}]});
    let status = || call("workflow.status", serde_json::json!({}));
    std::fs::write(
        &play,
        serde_json::json!({"turns": [
            call("workflow.create", serde_json::json!({"definition": {
                "name": "hold", "steps": {
                    "peer": {"kind": "a2a", "command": "poke"},
                    "m": {"kind": "manual"},
                    "w": {"kind": "wait", "depends_on": ["m"], "on": "signal", "signal": "go"},
                    "f": {"kind": "finish", "depends_on": ["w"], "status": "completed"}}}})),
            call("workflow.run", serde_json::json!({"name": "hold", "start": "m"})),
            status(),
            call("workflow.delete", serde_json::json!({"name": "hold"})),
            status(),
            call("workflow.signal", serde_json::json!({"name": "go"})),
            call("sleep", serde_json::json!({"duration": "1s"})),
            status(),
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let cfg = t.path().join("agent.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: taint\n  prompt: go\n  instruction: Look after it.\n\
             intelligence: {{ endpoints: \"mock:file:{}\", model: mock }}\n\
             store: {{kind: memory}}\n\
             lifecycle: {{run_until: idle, idle_grace: 300ms}}\n\
             observability:\n  log_level: info\n\
             security:\n  policies:\n\
             \x20   - {{match: {{tool: workflow.status, tags: [untrusted_input]}}, action: deny}}\n",
            play.display()
        ),
    )
    .unwrap();
    let (code, err) = agentd(&["--config", &cfg.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    let denied: Vec<bool> = err
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == "tool.result" && v["tool"] == "workflow.status")
        .map(|v| v["is_error"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(denied, [true, true, false], "{err}");
}
