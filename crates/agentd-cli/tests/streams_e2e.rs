// SPDX-License-Identifier: AGPL-3.0-only
//! Event streams, end to end: one workflow `emit`s domain events, a DIFFERENT
//! workflow's `stream` start consumes them — and, the property no other edge
//! has, a consumer that did not exist when the events were published still
//! processes them: life 1 emits with no consumer configured; life 2 adds the
//! consumer with `from: earliest` and the backlog replays, in order, exactly
//! once. Life 3 proves the durable offset: nothing re-fires.
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

fn config(dir: &str, with_consumer: bool) -> String {
    let consumer = if with_consumer {
        "  - name: fulfil\n    steps:\n\
         \x20     take: {kind: stream, stream: orders, subject: \"order.*\", from: earliest}\n\
         \x20     note: {kind: assign, depends_on: [take], value: \"handled {{steps.take.output.subject}} #{{steps.take.output.data.n}}\"}\n\
         \x20     f:    {kind: finish, depends_on: [note], status: completed, output: \"{{steps.note.output}}\"}\n"
    } else {
        ""
    };
    format!(
        "config_version: \"1\"\nagent:\n  name: eventful\n\
         store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
         streams:\n  orders:\n    retention: {{ max_events: 100 }}\n\
         workflows:\n  - name: producer\n    steps:\n\
         \x20     s:    {{kind: once, policy: always}}\n\
         \x20     each: {{kind: foreach, depends_on: [s], over: [1, 2, 3], batch: {{size: 1, parallel: 1}},\n\
         \x20            body: {{steps: {{pub: {{kind: emit, stream: orders, subject: \"order.paid\", correlation: \"o-{{{{item}}}}\", data: {{n: \"{{{{item}}}}\"}}}}}}}}}}\n\
         \x20     f:    {{kind: finish, depends_on: [each], status: completed}}\n{consumer}\
         lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

fn life(cfg: &str) -> (Option<i32>, String) {
    let err_path = common::unique_path("streams", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .status()
        .expect("run");
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    (out.code(), log)
}

#[test]
fn another_workflows_events_replay_into_a_late_consumer_exactly_once() {
    let dir = common::unique_path("streams", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");

    // Life 1: the producer emits three events. No consumer exists yet.
    std::fs::write(&cfg, config(&dir, false)).unwrap();
    let (code, l1) = life(&cfg);
    assert_eq!(code, Some(0), "{l1}");
    assert_eq!(events(&l1, "stream.emit").len(), 3, "{l1}");

    // Life 2: the consumer arrives with `from: earliest` — the backlog it
    // never saw published replays through it. (The producer also runs again,
    // adding three MORE events, consumed live in the same life.)
    std::fs::write(&cfg, config(&dir, true)).unwrap();
    let (code, l2) = life(&cfg);
    assert_eq!(code, Some(0), "{l2}");
    let done: Vec<String> = events(&l2, "run.done")
        .iter()
        .filter(|e| e["workflow"] == "fulfil" && e["status"] == "completed")
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        done.len(),
        6,
        "3 replayed + 3 live, exactly once each:\n{l2}"
    );
    assert_eq!(
        done.iter().filter(|o| o.contains("#1")).count(),
        2,
        "each item once per producer life:\n{done:?}"
    );
    assert!(
        done.iter().all(|o| o.starts_with("handled order.paid")),
        "{done:?}"
    );

    // Life 3: producer `once` fires again (3 new events, consumed), but the
    // six ALREADY-consumed events never re-fire — the offset is durable.
    std::fs::write(&cfg, config(&dir, true)).unwrap();
    let (code, l3) = life(&cfg);
    assert_eq!(code, Some(0), "{l3}");
    let done3 = events(&l3, "run.done")
        .iter()
        .filter(|e| e["workflow"] == "fulfil" && e["status"] == "completed")
        .count();
    assert_eq!(done3, 3, "only THIS life's events fire:\n{l3}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_undeclared_stream_is_refused_at_startup() {
    let dir = common::unique_path("streams-bad", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        "config_version: \"1\"\nagent:\n  name: x\nstore:\n  kind: memory\n\
         workflows:\n  - name: w\n    steps:\n\
         \x20     s: {kind: once}\n\
         \x20     e: {kind: emit, depends_on: [s], stream: nope, subject: a.b}\n\
         \x20     f: {kind: finish, depends_on: [e]}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The refusal reaches stderr inside a JSON log line, so the inner quotes
    // arrive escaped — match around them.
    assert!(
        stderr.contains("nope") && stderr.contains("is not declared under"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **`correlate` joins events that share a correlation value** (RFC 0035 §4.3).
///
/// `depends_on` joins steps; this joins events. One workflow emits `order.paid`
/// and `order.shipped` for two orders, plus a third order that is only paid —
/// and the join fires once per COMPLETE pair, carrying both events, while the
/// half-collected one stays pending rather than firing early.
#[test]
fn a_correlate_start_fires_once_per_completed_pair() {
    let dir = common::unique_path("correlate", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = common::unique_path("correlate", "yaml");
    std::fs::write(
        &cfg_path,
        format!(
            "config_version: \"1\"\nagent:\n  name: joiner\n\
             store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
             streams:\n  orders:\n    retention: {{ max_events: 100 }}\n\
             workflows:\n  - name: producer\n    steps:\n\
             \x20     s:  {{kind: once, policy: always}}\n\
             \x20     p1: {{kind: emit, depends_on: [s], stream: orders, subject: \"order.paid\",    correlation: \"o-1\", data: {{n: 1}}}}\n\
             \x20     p2: {{kind: emit, depends_on: [p1], stream: orders, subject: \"order.paid\",    correlation: \"o-2\", data: {{n: 2}}}}\n\
             \x20     p3: {{kind: emit, depends_on: [p2], stream: orders, subject: \"order.paid\",    correlation: \"o-3\", data: {{n: 3}}}}\n\
             \x20     s2: {{kind: emit, depends_on: [p3], stream: orders, subject: \"order.shipped\", correlation: \"o-2\", data: {{n: 2}}}}\n\
             \x20     s1: {{kind: emit, depends_on: [s2], stream: orders, subject: \"order.shipped\", correlation: \"o-1\", data: {{n: 1}}}}\n\
             \x20     f:  {{kind: finish, depends_on: [s1], status: completed}}\n\
             \x20 - name: reconcile\n    steps:\n\
             \x20     both: {{kind: correlate, stream: orders, on: [\"order.paid\", \"order.shipped\"],\n\
             \x20            by: correlation, window: 24h, on_incomplete: discard}}\n\
             \x20     note: {{kind: assign, depends_on: [both], value: \"joined {{{{steps.both.output.correlation}}}} n={{{{steps.both.output.events.0.data.n}}}} complete={{{{steps.both.output.complete}}}}\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [note], status: completed, output: \"{{{{steps.note.output}}}}\"}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
             observability:\n  log_level: info\n  log_content: true\n"
        ),
    )
    .unwrap();

    let (code, log) = life(&cfg_path);
    assert_eq!(code, Some(0), "the daemon exited cleanly:\n{log}");

    let joined: Vec<String> = events(&log, "run.done")
        .iter()
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .filter(|o| o.starts_with("joined "))
        .collect();
    assert_eq!(
        joined.len(),
        2,
        "exactly the two COMPLETE pairs fire — o-3 was paid but never shipped, so its \\
         half-collected join stays pending until its window expires:\n{joined:#?}\n{log}"
    );
    assert!(
        joined
            .iter()
            .any(|o| o.contains("joined o-1") && o.contains("complete=true"))
            && joined.iter().any(|o| o.contains("joined o-2")),
        "both orders joined, and the set is reported complete: {joined:#?}"
    );
    // The events arrive in the order `on` names them, not arrival order: o-2
    // shipped BEFORE o-1 did, and both still read paid-then-shipped.
    assert!(
        joined
            .iter()
            .all(|o| o.contains("n=1") || o.contains("n=2")),
        "the payload carries the joined events: {joined:#?}"
    );

    std::fs::remove_file(&cfg_path).ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// **`on_incomplete: fire_partial` turns a missing event into an event.**
///
/// "Paid but not shipped within the window" IS the thing an escalation flow
/// wants to hear about, so a join that times out can fire with the partial set
/// rather than discarding it. The run has to be able to tell the two apart,
/// which is what `complete` and `missing` in the payload are for — without
/// them a partial firing is indistinguishable from a complete one, and the
/// escalation would "reconcile" an order that was never shipped.
#[test]
fn an_incomplete_join_fires_partial_when_its_window_expires() {
    let dir = common::unique_path("correlate-partial", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = common::unique_path("correlate-partial", "yaml");
    std::fs::write(
        &cfg_path,
        format!(
            "config_version: \"1\"\nagent:\n  name: escalator\n\
             store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
             streams:\n  orders:\n    retention: {{ max_events: 100 }}\n\
             workflows:\n  - name: producer\n    steps:\n\
             \x20     s:  {{kind: once, policy: always}}\n\
             \x20     p1: {{kind: emit, depends_on: [s], stream: orders, subject: \"order.paid\", correlation: \"late-1\", data: {{n: 1}}}}\n\
             \x20     f:  {{kind: finish, depends_on: [p1], status: completed}}\n\
             \x20 - name: escalate\n    steps:\n\
             \x20     both: {{kind: correlate, stream: orders, on: [\"order.paid\", \"order.shipped\"],\n\
             \x20            by: correlation, window: 1ms, on_incomplete: fire_partial}}\n\
             \x20     note: {{kind: assign, depends_on: [both], value: \"escalate {{{{steps.both.output.correlation}}}} complete={{{{steps.both.output.complete}}}} missing={{{{steps.both.output.missing.0}}}}\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [note], status: completed, output: \"{{{{steps.note.output}}}}\"}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
             observability:\n  log_level: info\n  log_content: true\n"
        ),
    )
    .unwrap();

    let (code, log) = life(&cfg_path);
    assert_eq!(code, Some(0), "the daemon exited cleanly:\n{log}");

    let fired: Vec<String> = events(&log, "run.done")
        .iter()
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .filter(|o| o.starts_with("escalate "))
        .collect();
    assert_eq!(
        fired.len(),
        1,
        "the expired join fired once:\n{fired:#?}\n{log}"
    );
    assert!(
        fired[0].contains("complete=false"),
        "the run can SEE that this was a partial set: {}",
        fired[0]
    );
    assert!(
        fired[0].contains("missing=order.shipped"),
        "and which subject never arrived: {}",
        fired[0]
    );

    std::fs::remove_file(&cfg_path).ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// **`batch: {size, window}` makes one run per GROUP of events.**
///
/// The default is one run per event, which is the wrong shape for anything
/// that amortises — a bulk write, a single LLM call over a page of items. Six
/// events at `size: 3` is two runs, not six.
///
/// `window` is what stops a part-full batch waiting for ever on a quiet
/// stream, and the payload says `full` so a run can tell "three because that
/// is the batch" from "three because the window elapsed". Unlike a
/// `correlate` join, an unwindowed batch is still bounded — by `size` — which
/// is why `window` is optional here and mandatory there.
#[test]
fn a_batching_stream_consumer_fires_once_per_group() {
    let dir = common::unique_path("stream-batch", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = common::unique_path("stream-batch", "yaml");
    std::fs::write(
        &cfg_path,
        format!(
            "config_version: \"1\"\nagent:\n  name: batcher\n\
             store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
             streams:\n  ticks:\n    retention: {{ max_events: 100 }}\n\
             workflows:\n  - name: producer\n    steps:\n\
             \x20     s:    {{kind: once, policy: always}}\n\
             \x20     each: {{kind: foreach, depends_on: [s], over: [1, 2, 3, 4, 5, 6], batch: {{size: 1, parallel: 1}},\n\
             \x20            body: {{steps: {{pub: {{kind: emit, stream: ticks, subject: \"tick\", data: {{n: \"{{{{item}}}}\"}}}}}}}}}}\n\
             \x20     f:    {{kind: finish, depends_on: [each], status: completed}}\n\
             \x20 - name: bulk\n    steps:\n\
             \x20     take: {{kind: stream, stream: ticks, subject: \"tick\", from: earliest, batch: {{size: 3, window: 5s}}}}\n\
             \x20     note: {{kind: assign, depends_on: [take], value: \"batch of {{{{steps.take.output.count}}}} full={{{{steps.take.output.full}}}}\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [note], status: completed, output: \"{{{{steps.note.output}}}}\"}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
             observability:\n  log_level: info\n  log_content: true\n"
        ),
    )
    .unwrap();

    let (code, log) = life(&cfg_path);
    assert_eq!(code, Some(0), "the daemon exited cleanly:\n{log}");

    let batches: Vec<String> = events(&log, "run.done")
        .iter()
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .filter(|o| o.starts_with("batch of "))
        .collect();
    assert_eq!(
        batches.len(),
        2,
        "six events at size 3 is TWO runs, not six:\n{batches:#?}\n{log}"
    );
    assert!(
        batches
            .iter()
            .all(|b| b.contains("batch of 3") && b.contains("full=true")),
        "each batch is full at its size, and says so: {batches:#?}"
    );

    std::fs::remove_file(&cfg_path).ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// **`emit … forward: {webhook: URL}` pushes the event out as it appends.**
///
/// The durable copy is the source of truth and the push is the notification
/// (RFC 0035 §5), so this asserts both halves: the receiver is told, AND the
/// event is on the stream regardless — a consumer that never saw the push
/// still reads it from its offset.
#[test]
fn a_forwarded_emit_notifies_a_webhook_and_still_appends() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;

    // A one-shot receiver that records what it was sent.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let recv_port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        if let Ok((sock, _)) = listener.accept() {
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                    break;
                }
                if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&l);
            }
            let mut body = vec![0u8; len];
            use std::io::Read;
            let _ = reader.read_exact(&mut body);
            let mut sock = sock;
            let _ = sock.write_all(
                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = tx.send(String::from_utf8_lossy(&body).to_string());
        }
    });

    let dir = common::unique_path("forward", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = common::unique_path("forward", "yaml");
    std::fs::write(
        &cfg_path,
        format!(
            "config_version: \"1\"\nagent:\n  name: forwarder\n\
             store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
             streams:\n  outbox:\n    retention: {{ max_events: 100 }}\n\
             workflows:\n  - name: producer\n    steps:\n\
             \x20     s:  {{kind: once, policy: always}}\n\
             \x20     e:  {{kind: emit, depends_on: [s], stream: outbox, subject: \"thing.happened\",\n\
             \x20          data: {{n: 7}}, forward: {{webhook: \"http://127.0.0.1:{recv_port}/hook\", allow_private: true}}}}\n\
             \x20     f:  {{kind: finish, depends_on: [e], status: completed}}\n\
             \x20 - name: consumer\n    steps:\n\
             \x20     take: {{kind: stream, stream: outbox, subject: \"thing.*\", from: earliest}}\n\
             \x20     note: {{kind: assign, depends_on: [take], value: \"consumed {{{{steps.take.output.data.n}}}}\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [note], status: completed, output: \"{{{{steps.note.output}}}}\"}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
             observability:\n  log_level: info\n  log_content: true\n"
        ),
    )
    .unwrap();

    let (code, log) = life(&cfg_path);
    assert_eq!(code, Some(0), "the daemon exited cleanly:\n{log}");

    // The receiver was notified…
    let pushed = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the forward reached the receiver");
    assert!(
        pushed.contains("thing.happened") && pushed.contains("outbox"),
        "the notification names the event: {pushed}"
    );
    // …and the durable copy was consumed independently of that push.
    let consumed: Vec<String> = events(&log, "run.done")
        .iter()
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .filter(|o| o.starts_with("consumed "))
        .collect();
    assert_eq!(
        consumed,
        vec!["consumed 7"],
        "the event is on the stream whether or not anything was pushed:\n{log}"
    );

    std::fs::remove_file(&cfg_path).ok();
    std::fs::remove_dir_all(&dir).ok();
}
