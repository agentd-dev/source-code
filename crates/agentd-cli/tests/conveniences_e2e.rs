// SPDX-License-Identifier: AGPL-3.0-only
//! The convenience batch, end to end — each capability exercised the way the
//! `examples/startup/` configs use it: `wait.on_timeout` routing, the
//! webhook→signal field, `memory.push`/`memory.shift` as a durable queue,
//! stream `rate:` pacing, the `human.asked` internal event, `switch
//! on_no_match: skip`, day/week durations, and the typed A2A
//! `command`/`args` round trip (an instance delegating to ITSELF over
//! loopback, schema checked at admission).
#![cfg(all(unix, feature = "workflow"))]

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
    let dir = common::unique_path("conv", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

#[test]
fn a_wait_timeout_routes_to_its_named_step_and_success_path_stays_pruned() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: t }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 400ms }\n\
         observability: { log_level: info, log_content: true }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:    { kind: once }\n\
        \x20     park: { kind: wait, depends_on: [s], on: signal, signal: nobody-fires-this, timeout: 300ms, on_timeout: plan_b }\n\
        \x20     ok:   { kind: assign, depends_on: [park], value: \"the reply came\" }\n\
        \x20     f1:   { kind: finish, depends_on: [ok], status: completed, output: \"{{steps.ok.output}}\" }\n\
        \x20     plan_b: { kind: assign, value: \"timed out, moving on\" }\n\
        \x20     f2:   { kind: finish, depends_on: [plan_b], status: completed, output: \"{{steps.plan_b.output}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !events(&log, "step.timeout_routed").is_empty(),
        "the timeout took the routing edge:\n{log}"
    );
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["status"], "completed", "{log}");
    assert_eq!(done[0]["output"], "timed out, moving on", "{log}");
}

#[test]
fn memory_push_and_shift_are_a_durable_queue() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: q }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 400ms }\n\
         observability: { log_level: info, log_content: true }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:  { kind: once }\n\
        \x20     p1: { kind: memory.push, depends_on: [s], key: q, value: first }\n\
        \x20     p2: { kind: memory.push, depends_on: [p1], key: q, value: second }\n\
        \x20     t1: { kind: memory.shift, depends_on: [p2], key: q }\n\
        \x20     t2: { kind: memory.shift, depends_on: [t1], key: q }\n\
        \x20     t3: { kind: memory.shift, depends_on: [t2], key: q }\n\
        \x20     f:  { kind: finish, depends_on: [t3], status: completed, output: \"{{steps.t1.output.value}}/{{steps.t2.output.value}}/{{steps.t3.output.found}}\" }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(
        done[0]["output"], "first/second/false",
        "FIFO + honest empty:\n{log}"
    );
}

#[test]
fn a_switch_with_on_no_match_skip_completes_instead_of_failing() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: sw }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 400ms }\n\
         observability: { log_level: info, log_content: true }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     v: { kind: assign, depends_on: [s], value: something-unexpected }\n\
        \x20     r: { kind: switch, depends_on: [v], on: \"{{steps.v.output}}\", cases: { known: act }, on_no_match: skip }\n\
        \x20     act: { kind: assign, depends_on: [r], value: acted }\n\
        \x20     f: { kind: finish, depends_on: [r], status: completed }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(
        done[0]["status"], "completed",
        "no-match is not a failure:\n{log}"
    );
    assert!(
        !log.contains("\"step\":\"act\"") || !log.contains("\"status\":\"done\",\"step\":\"act\""),
        "the unmatched branch stayed pruned:\n{log}"
    );
}

#[test]
fn day_and_week_durations_parse() {
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: d }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 300ms }\n\
         limits: { run: { deadline: 30d } }\n\
         streams: { s: { retention: { max_age: 2w } } }\n\
         observability: { log_level: info }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     e: { kind: emit, depends_on: [s], stream: s, subject: a.b }\n\
        \x20     f: { kind: finish, depends_on: [e], status: completed }\n",
    );
    assert_eq!(code, Some(0), "{log}");
}

#[test]
fn a_paced_stream_consumer_leaves_the_backlog_queued() {
    // Three events, a consumer paced to a burst of 1: exactly one fires in
    // this life; the offset holds the other two for later lives.
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: paced }\nstore: { kind: memory }\n\
         lifecycle: { run_until: idle, idle_grace: 600ms }\n\
         observability: { log_level: info, log_content: true }\n\
         streams: { jobs: { retention: { max_events: 100 } } }\n\
         workflows:\n  - name: producer\n    steps:\n\
        \x20     s:  { kind: once }\n\
        \x20     e1: { kind: emit, depends_on: [s], stream: jobs, subject: t.a, data: { n: 1 } }\n\
        \x20     e2: { kind: emit, depends_on: [e1], stream: jobs, subject: t.a, data: { n: 2 } }\n\
        \x20     e3: { kind: emit, depends_on: [e2], stream: jobs, subject: t.a, data: { n: 3 } }\n\
        \x20     f:  { kind: finish, depends_on: [e3], status: completed }\n\
         \x20 - name: consumer\n    steps:\n\
        \x20     take: { kind: stream, stream: jobs, subject: \"t.*\", rate: \"1/1h\" }\n\
        \x20     f:    { kind: finish, depends_on: [take], status: completed }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let consumed = events(&log, "run.done")
        .iter()
        .filter(|e| e["workflow"] == "consumer")
        .count();
    assert_eq!(consumed, 1, "burst 1 per hour = one fire this life:\n{log}");
}

#[test]
fn mock_intelligence_runs_a_model_step_fully_offline() {
    // `intelligence.endpoints: mock:<script>` — the whole agent runs with no
    // key, no network, no second process. Debug builds always carry the
    // mock; release needs `--features internal-mocks`.
    let (code, log) = run_cfg(
        "config_version: \"1\"\nagent: { name: offline }\nstore: { kind: memory }\n\
         intelligence: { endpoints: \"mock:final\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     t: { kind: agent, depends_on: [s], instruction: \"say done\" }\n\
        \x20     f: { kind: finish, depends_on: [t], status: completed }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(
        done[0]["status"], "completed",
        "offline model turn ran:\n{log}"
    );
}

#[cfg(feature = "a2a")]
#[test]
fn a_typed_command_round_trips_and_its_schema_refuses_bad_payloads() {
    // ONE instance, peered to itself over loopback: `a2a.delegate {command,
    // args}` sends the DataPart, the `a2a` start's `schema` checks it at
    // admission, and the fired run reads `{{steps.cmd.output.args.*}}` typed.
    let port = common::free_port();
    let cfg = format!(
        "config_version: \"1\"\nagent: {{ name: self }}\nstore: {{ kind: memory }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 900ms }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n  peers:\n    - name: me\n      endpoint: http://127.0.0.1:{port}\n\
         workflows:\n  - name: caller\n    steps:\n\
        \x20     s:  {{ kind: once }}\n\
        \x20     good: {{ kind: a2a.delegate, depends_on: [s], peer: me, command: t.echo, args: {{ ticket: \"T-1\", severity: high }}, timeout: 30s }}\n\
        \x20     bad:  {{ kind: a2a.delegate, depends_on: [good], peer: me, command: t.echo, args: {{ wrong_field: 1 }}, timeout: 30s, on_error: continue }}\n\
        \x20     f:  {{ kind: finish, depends_on: [bad], status: completed, output: {{ echoed: \"{{{{steps.good.output}}}}\", refused: \"{{{{steps.bad.error | yes}}}}\" }} }}\n\
         \x20 - name: server\n    steps:\n\
        \x20     cmd: {{ kind: a2a, command: t.echo, roles: [agent, operator],\n\
        \x20             schema: {{ type: object, required: [ticket, severity], properties: {{ ticket: {{ type: string }}, severity: {{ enum: [low, high] }} }} }} }}\n\
        \x20     f:   {{ kind: finish, depends_on: [cmd], status: completed, output: \"got {{{{steps.cmd.output.args.ticket}}}} at {{{{steps.cmd.output.args.severity}}}}\" }}\n"
    );
    let (code, log) = run_cfg(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    let server: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "server").collect();
    assert_eq!(
        server.len(),
        1,
        "the good command fired ONE server run; the bad one was refused at admission:\n{log}"
    );
    assert_eq!(
        server[0]["output"], "got T-1 at high",
        "typed args reached the workflow:\n{log}"
    );
    let caller: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "caller").collect();
    assert_eq!(caller.len(), 1, "{log}");
    let out = &caller[0]["output"];
    assert_eq!(
        out["echoed"], "got T-1 at high",
        "the delegate got the run's output back:\n{log}"
    );
    assert!(
        out["refused"].as_str().unwrap_or("").contains("schema"),
        "the bad payload was refused WITH the mismatch: {out}\n{log}"
    );
}

#[cfg(feature = "a2a")]
#[test]
fn a_webhook_start_with_a_signal_field_wakes_the_parked_run() {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let port = common::free_port();
    let dir = common::unique_path("conv-whsig", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\nagent: {{ name: wh }}\nstore: {{ kind: memory }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 800ms }}\n\
             observability: {{ log_level: info, log_content: true }}\n\
             webhooks: {{ listen: \"http://127.0.0.1:{port}\" }}\n\
             workflows:\n  - name: waiter\n    steps:\n\
            \x20     s:    {{ kind: once }}\n\
            \x20     park: {{ kind: wait, depends_on: [s], on: signal, signal: \"go/k1\", timeout: 30s }}\n\
            \x20     f:    {{ kind: finish, depends_on: [park], status: completed, output: \"woken by {{{{steps.park.output.payload.body.who}}}}\" }}\n\
             \x20 - name: relay\n    steps:\n\
            \x20     hook: {{ kind: webhook, path: /go, signal: \"go/{{{{ body.key }}}}\" }}\n\
            \x20     f:    {{ kind: finish, depends_on: [hook], status: completed }}\n"
        ),
    )
    .unwrap();
    let err_path = common::unique_path("conv-whsig", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn");
    // Wait for the waiter to park, then POST the webhook.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if log.contains("\"step\":\"park\"") {
            break;
        }
        assert!(Instant::now() < deadline, "waiter never parked:\n{log}");
        std::thread::sleep(Duration::from_millis(30));
    }
    let body = r#"{"key": "k1", "who": "the webhook"}"#;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    write!(
        s,
        "POST /go HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    drop(s);
    let status = child.wait().expect("exit");
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(0), "{log}");
    let done = events(&log, "run.done");
    let waiter: Vec<&Value> = done.iter().filter(|e| e["workflow"] == "waiter").collect();
    assert_eq!(
        waiter[0]["output"], "woken by the webhook",
        "the templated signal name matched and carried the payload:\n{log}"
    );
}

#[test]
fn a_human_gate_opening_fires_the_asked_event() {
    use std::time::{Duration, Instant};
    let dir = common::unique_path("conv-asked", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        "config_version: \"1\"\nagent: { name: gates, ask_human_fallback: wait }\nstore: { kind: memory }\n\
         lifecycle: { run_until: drained }\n\
         observability: { log_level: info, log_content: true }\n\
         workflows:\n  - name: approval\n    steps:\n\
        \x20     s:    { kind: once }\n\
        \x20     gate: { kind: human, depends_on: [s], question: \"ship it?\", timeout: 1h }\n\
        \x20     f:    { kind: finish, depends_on: [gate], status: completed }\n\
        \x20 - name: notifier\n    steps:\n\
        \x20     saw: { kind: event, on: human.asked }\n\
        \x20     n:   { kind: assign, depends_on: [saw], value: \"someone should answer: {{steps.saw.output.payload.question}}\" }\n\
        \x20     f:   { kind: finish, depends_on: [n], status: completed, output: \"{{steps.n.output}}\" }\n",
    )
    .unwrap();
    let err_path = common::unique_path("conv-asked", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    let log = loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if events(&log, "run.done")
            .iter()
            .any(|e| e["workflow"] == "notifier")
        {
            break log;
        }
        assert!(Instant::now() < deadline, "notifier never fired:\n{log}");
        std::thread::sleep(Duration::from_millis(30));
    };
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
    let _ = std::fs::remove_file(&err_path);
    let _ = std::fs::remove_dir_all(&dir);
    let notified: Vec<Value> = events(&log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "notifier")
        .collect();
    assert_eq!(notified.len(), 1, "{log}");
    assert_eq!(
        notified[0]["output"], "someone should answer: ship it?",
        "the event carried the question:\n{log}"
    );
}
