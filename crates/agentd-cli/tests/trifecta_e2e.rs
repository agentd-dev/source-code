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
/// and the stream an outside edge can append to.
const BASE: &str = "agent:\n  name: taint\n  instruction: Triage the inbox.\n\
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

/// A consumer defined in a FILE is not visible to `--validate-config`; the
/// loader checks it once the definitions resolve, and the start refuses.
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
    assert_names(
        &err,
        "webhook `into:` at workflow \\\"intake\\\" step \\\"hook\\\"",
    );
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

/// Whether some `config.reload.invalid` / `workflow.stored.invalid` line or
/// tool reply names the workflow, the stream, the producer and the server.
#[cfg(feature = "hot-reload")]
fn names_all(m: &str, workflow: &str, server: &str) -> bool {
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
    let port = common::free_port();
    let body = |tags: &str| {
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
    let original = body("sensitive");
    std::fs::write(&cfg, &original).unwrap();
    let d = Daemon::spawn(&cfg);
    d.wait_for(
        |d| {
            d.events("workflow.loaded")
                .iter()
                .any(|e| e["name"] == "triage")
        },
        "the consumer to load",
    );

    // `egress` joins the consumer's server: the trifecta is complete.
    std::fs::write(&cfg, body("sensitive, egress")).unwrap();
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
