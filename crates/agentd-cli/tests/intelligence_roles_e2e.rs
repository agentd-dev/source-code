// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0046 P1 — providers and roles, end to end.
//!
//! A model names the connection it is reached on (`provider:`), each job runs
//! on the model its role names, and every model reference — a role, a node's
//! `model:`, a fallback, `budget.degrade.model` — resolves through one
//! function to a connection, a wire model and a fallback chain. Two mock
//! providers answer differently, so each test reads in a run's OUTPUT which
//! connection served it, not only what the log says was asked.
#![cfg(unix)]

mod common;

use std::process::{Command, Stdio};

/// A mock playbook that answers every turn with `content`.
fn playbook(content: &str) -> String {
    let path = common::unique_path("roles-play", "json");
    let body = serde_json::json!({"turns": [{"content": content}]});
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

/// The intelligence section: the top-level connection answers `top`, the
/// `second` provider answers `second`.
fn intelligence(top: &str, second: &str, models_and_roles: &str) -> String {
    format!(
        "intelligence:\n  endpoints: \"mock:file:{top}\"\n  providers:\n    second: {{ endpoints: \"mock:file:{second}\" }}\n{models_and_roles}"
    )
}

/// Run a job-shaped config; returns (exit code, stdout JSON, stderr log).
fn run(cfg_text: &str) -> (Option<i32>, serde_json::Value, String) {
    let dir = common::unique_path("roles", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text.replace("__STATE__", &format!("{dir}/state"))).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), stdout, log)
}

const BASE: &str = "agent: { name: t }\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info }\n\
     lifecycle: { run_until: idle, idle_grace: 2s }\n";

/// The log lines with event `name`.
fn events(log: &str, name: &str) -> Vec<serde_json::Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A model on a provider is dialed there; a role binds a job to a model; a
/// node's `model:` beats its role.
#[test]
fn each_job_runs_on_its_roles_model_and_a_provider_model_is_dialed_there() {
    let (top, second) = (
        playbook(r#"{"from": "top"}"#),
        playbook(r#"{"from": "second"}"#),
    );
    let cfg = format!(
        "{BASE}{}workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     thought: {{ kind: think, prompt: \"think\", depends_on: [s] }}\n\
        \x20     named: {{ kind: think, prompt: \"named\", model: main, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [thought, named], status: completed, output: {{ thought: \"{{{{steps.thought.output.from}}}}\", named: \"{{{{steps.named.output.from}}}}\" }} }}\n",
        intelligence(
            &top,
            &second,
            "  models:\n    main: { model: wire-main }\n    deep: { model: wire-deep, provider: second }\n  roles: { turn: main, think: deep }\n"
        )
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(
        out["thought"], "second",
        "roles.think is on the second provider: {out}\n{log}"
    );
    assert_eq!(
        out["named"], "top",
        "a node's model: beats its role: {out}\n{log}"
    );
    let models = events(&log, "turn.model");
    assert!(
        models.iter().any(|m| m["role"] == "think"
            && m["model"] == "wire-deep"
            && m["provider"] == "second"),
        "{models:?}"
    );
    let _ = std::fs::remove_file(&top);
    let _ = std::fs::remove_file(&second);
}

/// `classify`, `route` and `judge` decide: with no `model:`, they run on
/// `roles.decide` — and nothing else moves with it.
#[test]
fn a_decision_node_runs_on_the_decide_role() {
    let (top, second) = (
        playbook(r#"{"class": "from-top", "confidence": 0.5}"#),
        playbook(r#"{"class": "from-second", "confidence": 0.9}"#),
    );
    let cfg = format!(
        "{BASE}{}workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     c: {{ kind: classify, input: hello, classes: [from-top, from-second], depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [c], status: completed, output: {{ got: \"{{{{steps.c.output.class}}}}\" }} }}\n",
        intelligence(
            &top,
            &second,
            "  models:\n    main: { model: wire-main }\n    judge: { model: wire-judge, provider: second }\n  roles: { turn: main, decide: judge }\n"
        )
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["got"], "from-second", "{log}");
    assert!(
        events(&log, "turn.model")
            .iter()
            .any(|m| m["role"] == "decide" && m["model"] == "wire-judge"),
        "{log}"
    );
    let _ = std::fs::remove_file(&top);
    let _ = std::fs::remove_file(&second);
}

/// A model whose connection is down is answered by its `fallback`, on the
/// fallback's own provider. Validated and never walked before RFC 0046 P1.
#[test]
fn an_unavailable_model_is_answered_by_its_fallback_on_another_provider() {
    let top = playbook(r#"{"from": "top"}"#);
    let cfg = format!(
        "{BASE}intelligence:\n  endpoints: \"mock:file:{top}\"\n  providers:\n\
        \x20   down: {{ endpoints: \"http://127.0.0.1:9\" }}\n\
        \x20 models:\n\
        \x20   main: {{ model: wire-main, provider: down, fallback: backup }}\n\
        \x20   backup: {{ model: wire-backup }}\n\
        \x20 roles: {{ turn: main }}\n\
        workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     t: {{ kind: think, prompt: \"x\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [t], status: completed, output: {{ got: \"{{{{steps.t.output.from}}}}\" }} }}\n"
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["got"], "top", "the fallback's provider answered: {log}");
    let fell = events(&log, "turn.model_fallback");
    assert!(
        fell.iter()
            .any(|f| f["model"] == "wire-main" && f["served_by"] == "wire-backup"),
        "{fell:?}\n{log}"
    );
    let _ = std::fs::remove_file(&top);
}

/// `budget.degrade.model` names a model like any reference: the turn moves to
/// its wire model on its provider. It used to reach the worker unresolved, so
/// a model's NAME was sent to the provider as if it were a model.
#[test]
fn a_budget_degrade_moves_the_turn_to_its_model_on_its_provider() {
    let (top, second) = (
        playbook(r#"{"from": "top"}"#),
        playbook(r#"{"from": "second"}"#),
    );
    let cfg = format!(
        "{BASE}{}  budget:\n    windows: [{{ per: day, tokens: 1 }}]\n    on_exhausted: degrade\n    degrade: {{ model: cheap }}\n\
        workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     t: {{ kind: think, prompt: \"x\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [t], status: completed, output: {{ got: \"{{{{steps.t.output.from}}}}\" }} }}\n",
        intelligence(
            &top,
            &second,
            "  models:\n    main: { model: wire-main }\n    cheap: { model: wire-cheap, provider: second }\n  roles: { turn: main }\n"
        )
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(
        out["got"], "second",
        "the degrade model's provider answered: {log}"
    );
    let degraded = events(&log, "turn.model_degraded");
    assert!(
        degraded
            .iter()
            .any(|d| d["to"] == "wire-cheap" && d["provider"] == "second"),
        "{degraded:?}\n{log}"
    );
    let _ = std::fs::remove_file(&top);
    let _ = std::fs::remove_file(&second);
}

/// A reload that moves a role, a model or its provider is what the next call
/// dials: nothing about a model is copied at startup. (A reload used to set
/// the turn model to the literal `intelligence.model`, ignoring the tiers.)
#[cfg(feature = "hot-reload")]
#[test]
fn a_reload_moves_a_role_to_another_model_and_provider() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    let (top, second) = (
        playbook(r#"{"from": "top"}"#),
        playbook(r#"{"from": "second"}"#),
    );
    let config = |think: &str| {
        format!(
            "agent: {{ name: r }}\nstore: {{ kind: memory }}\nobservability: {{ log_level: info }}\n\
             lifecycle: {{ run_until: drained, drain_timeout: 5s }}\n{}\
             workflows:\n\
             \x20 - name: tick\n    steps:\n\
             \x20     s: {{ kind: schedule, every: 1s }}\n\
             \x20     t: {{ kind: think, prompt: \"x\", depends_on: [s] }}\n\
             \x20     f: {{ kind: finish, depends_on: [t], status: completed }}\n",
            intelligence(
                &top,
                &second,
                &format!(
                    "  models:\n    main: {{ model: wire-main }}\n    deep: {{ model: wire-deep, provider: second }}\n  roles: {{ turn: main, think: {think} }}\n"
                )
            )
        )
    };
    let cfg_path = common::unique_path("roles-reload", "yaml");
    std::fs::write(&cfg_path, config("main")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd");
    let pid = child.id() as i32;
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                let _ = tx.send(v);
            }
        }
    });
    let wait = |pred: &dyn Fn(&serde_json::Value) -> bool, what: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(v) if pred(&v) => return v,
                Ok(_) => continue,
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    };
    let first = wait(
        &|v| v["event"] == "turn.model" && v["role"] == "think",
        "the first think turn",
    );
    assert_eq!(first["model"], "wire-main", "{first}");

    std::fs::write(&cfg_path, config("deep")).unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    let reloaded = wait(&|v| v["event"] == "config.reloaded", "config.reloaded");
    assert!(
        reloaded["changed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "intelligence"),
        "{reloaded}"
    );
    let moved = wait(
        &|v| v["event"] == "turn.model" && v["role"] == "think" && v["model"] == "wire-deep",
        "a think turn on the reloaded role's model",
    );
    assert_eq!(moved["provider"], "second", "{moved}");

    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = child.wait();
    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&top);
    let _ = std::fs::remove_file(&second);
}

/// A reload that moves the model a running warm subagent was spawned on is
/// handed to it, and applied at its next turn boundary (`intel.swap`). The
/// child always honoured the swap; nothing ever sent it, so a warm subagent
/// dialed what it was spawned with for as long as it lived.
#[cfg(feature = "hot-reload")]
#[test]
fn a_reload_hands_a_running_warm_subagent_its_model_resolved_again() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    // The root turn starts a warm helper, then — slowly, so the reload lands
    // first — sends it its next turn. The helper is told apart by its own
    // system prompt, which the root's transcript never contains.
    let play = common::unique_path("roles-swap-play", "json");
    std::fs::write(
        &play,
        serde_json::json!({
            "turns": [
                {"tool_calls": [{"name": "subagent.run", "arguments": {
                    "instruction": "stand by for instructions", "mode": "warm"}}]},
                {"delay_ms": 4000, "tool_calls": [{"name": "subagent.send",
                    "arguments": {"message": "carry on"},
                    "arguments_from_tool_result": {"handle": "/handle"}}]},
                {"content": "steered"}
            ],
            "match": [
                {"when_contains": "You are agentd, an autonomous agent.", "content": "standing by"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let config = |model: &str| {
        format!(
            "agent: {{ name: swap }}\nstore: {{ kind: memory }}\nobservability: {{ log_level: info }}\n\
             lifecycle: {{ run_until: drained, drain_timeout: 5s }}\n\
             intelligence:\n  endpoints: \"mock:file:{play}\"\n  models:\n    main: {{ model: {model} }}\n  roles: {{ turn: main }}\n\
             workflows:\n  - name: steer\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     a: {{ kind: agent, depends_on: [s], instruction: \"start a helper, then steer it\" }}\n\
             \x20     f: {{ kind: finish, depends_on: [a], status: completed }}\n"
        )
    };
    let cfg_path = common::unique_path("roles-swap", "yaml");
    std::fs::write(&cfg_path, config("wire-one")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd");
    let pid = child.id() as i32;
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                let _ = tx.send(v);
            }
        }
    });
    let wait = |pred: &dyn Fn(&serde_json::Value) -> bool, what: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(v) if pred(&v) => return v,
                Ok(_) => continue,
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    };
    let helper = wait(
        &|v| v["event"] == "subagent.spawn" && v["mode"] == "warm",
        "the warm helper",
    );

    std::fs::write(&cfg_path, config("wire-two")).unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    let sent = wait(&|v| v["event"] == "intel.swap.sent", "the swap sent");
    assert_eq!(sent["subagents"], 1, "{sent}");
    let swapped = wait(
        &|v| v["event"] == "intel.swap" && v["pid"] == helper["pid"],
        "the helper to apply the swap",
    );
    assert_eq!(swapped["model_from"], "wire-one", "{swapped}");
    assert_eq!(swapped["model_to"], "wire-two", "{swapped}");

    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = child.wait();
    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&play);
}

/// A provider dials with ITS credential. The `AGENTD_INTELLIGENCE_TOKEN*`
/// overrides are the top-level connection's: read for every provider, they
/// sent that key to every other vendor and replaced the provider's own.
#[test]
fn a_provider_dials_with_its_own_token_not_the_top_level_environment() {
    use std::io::{BufRead, BufReader, Read, Write};

    // A one-shot OpenAI-compatible endpoint that records the Authorization
    // header it was sent.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let (mut auth, mut len) = (String::new(), 0usize);
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("authorization:") {
                auth = line["authorization:".len()..].trim().to_string();
            } else if let Some(v) = lower.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let answer = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "{\"from\": \"provider\"}"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
        .to_string();
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        )
        .unwrap();
        auth
    });

    let top = playbook(r#"{"from": "top"}"#);
    let dir = common::unique_path("roles-token", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "{BASE}intelligence:\n  endpoints: \"mock:file:{top}\"\n  providers:\n\
             \x20   second: {{ endpoints: \"http://127.0.0.1:{port}\", token: \"{{{{secret:PROVIDER_KEY}}}}\" }}\n\
             \x20 models:\n    main: {{ model: wire-main }}\n    deep: {{ model: wire-deep, provider: second }}\n\
             \x20 roles: {{ turn: main, think: deep }}\n\
             workflows:\n\
             \x20 - name: w\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     t: {{ kind: think, prompt: \"x\", depends_on: [s] }}\n\
             \x20     f: {{ kind: finish, depends_on: [t], status: completed, output: {{ got: \"{{{{steps.t.output.from}}}}\" }} }}\n"
        )
        .replace("__STATE__", &format!("{dir}/state")),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("PROVIDER_KEY", "provider-key")
        .env("AGENTD_INTELLIGENCE_TOKEN", "TOP-LEVEL-KEY")
        .stdin(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(0), "{log}");
    let stdout: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    assert_eq!(stdout["got"], "provider", "{log}");
    assert_eq!(seen.join().unwrap(), "Bearer provider-key");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&top);
}
