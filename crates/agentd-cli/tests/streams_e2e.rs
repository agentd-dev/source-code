// SPDX-License-Identifier: AGPL-3.0-only
//! Event streams, end to end: one workflow `emit`s domain events, a DIFFERENT
//! workflow's `stream` start consumes them — and, the property no other edge
//! has, a consumer that did not exist when the events were published still
//! processes them: life 1 emits with no consumer configured; life 2 adds the
//! consumer with `from: earliest` and the backlog replays, in order, exactly
//! once. Life 3 proves the durable offset: nothing re-fires.
#![cfg(unix)]

mod common;

use std::process::{Command, Stdio};

use serde_json::{Value, json};

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
        "agent:\n  name: eventful\n\
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
        "agent:\n  name: x\nstore:\n  kind: memory\n\
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
            "agent:\n  name: joiner\n\
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
            "agent:\n  name: escalator\n\
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
            "agent:\n  name: batcher\n\
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
            "agent:\n  name: forwarder\n\
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

/// Write `cfg` as a JSON config and run one life of the daemon on it.
fn life_json(cfg: &Value) -> (Option<i32>, String) {
    let path = common::unique_path("streams-cfg", "json");
    std::fs::write(&path, serde_json::to_vec(cfg).unwrap()).unwrap();
    let out = life(&path);
    let _ = std::fs::remove_file(&path);
    out
}

/// The outputs of the completed runs whose output starts with `prefix`.
fn outputs(log: &str, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = events(log, "run.done")
        .iter()
        .filter(|e| e["status"] == "completed")
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .filter(|o| o.starts_with(prefix))
        .collect();
    out.sort();
    out
}

/// A file store under `dir`, flushed on every write so each life starts from
/// exactly what the last one left.
fn file_store(dir: &str, min_free: Option<&str>) -> Value {
    let mut file = json!({"path": format!("{dir}/state")});
    if let Some(m) = min_free {
        file["min_free"] = json!(m);
    }
    json!({"kind": "file", "file": file, "checkpoint": {"debounce_ms": 0}})
}

/// A `once` workflow that emits `order.paid` and then `order.shipped` for
/// orders 1..=3, each carrying its order as the correlation.
fn order_producer() -> Value {
    let emits = |subject: &str| {
        json!({"steps": {"pub": {"kind": "emit", "stream": "orders", "subject": subject,
                                 "correlation": "o-{{item}}", "data": {"n": "{{item}}"}}}})
    };
    json!({"name": "producer", "steps": {
        "s": {"kind": "once", "policy": "always"},
        "paid": {"kind": "foreach", "depends_on": ["s"], "over": [1, 2, 3],
                 "batch": {"size": 1, "parallel": 1}, "body": emits("order.paid")},
        "shipped": {"kind": "foreach", "depends_on": ["paid"], "over": [1, 2, 3],
                    "batch": {"size": 1, "parallel": 1}, "body": emits("order.shipped")},
        "f": {"kind": "finish", "depends_on": ["shipped"], "status": "completed"}
    }})
}

/// The three ways a workflow consumes a stream: one run per event, one run
/// per batch, one run per joined set.
fn order_consumers() -> Vec<Value> {
    let finish = |dep: &str, output: &str| json!({"kind": "finish", "depends_on": [dep], "status": "completed", "output": output});
    vec![
        json!({"name": "each", "steps": {
            "take": {"kind": "stream", "stream": "orders", "subject": "order.paid", "from": "earliest"},
            "f": finish("take", "each #{{steps.take.output.data.n}}")
        }}),
        json!({"name": "bulk", "steps": {
            "take": {"kind": "stream", "stream": "orders", "subject": "order.*", "from": "earliest",
                     "batch": {"size": 3}},
            "f": finish("take", "bulk of {{steps.take.output.count}} from #{{steps.take.output.events.0.seq}}")
        }}),
        json!({"name": "join", "steps": {
            "both": {"kind": "correlate", "stream": "orders", "on": ["order.paid", "order.shipped"],
                     "by": "correlation", "window": "24h", "on_incomplete": "discard"},
            "f": finish("both", "join {{steps.both.output.correlation}} complete={{steps.both.output.complete}}")
        }}),
    ]
}

fn orders_config(dir: &str, workflows: Vec<Value>, min_free: Option<&str>) -> Value {
    json!({
        "agent": {"name": "consumers"},
        "store": file_store(dir, min_free),
        "streams": {"orders": {"retention": {"max_events": 100}}},
        "workflows": workflows,
        "lifecycle": {"run_until": "idle", "idle_grace": "900ms"},
        "observability": {"log_level": "info", "log_content": true}
    })
}

/// **A consumer advances only past events it actually fired** (RFC 0045 D1).
///
/// A start refused under resource pressure used to be consumed anyway: the
/// offset moved before the firing, the firing was shed, and the event was
/// gone. Here every consumer shape meets a shedding daemon with events
/// waiting — one run per event, a batch, a join — and fires NOTHING; the next
/// life, with the pressure gone, fires every one of them exactly once.
#[test]
fn a_shed_firing_leaves_its_events_on_the_stream_for_every_consumer_shape() {
    let dir = common::unique_path("streams-shed", "d");
    std::fs::create_dir_all(&dir).unwrap();

    // Life 1: the events, with no consumer yet.
    let (code, l1) = life_json(&orders_config(&dir, vec![order_producer()], None));
    assert_eq!(code, Some(0), "{l1}");
    assert_eq!(events(&l1, "stream.emit").len(), 6, "{l1}");

    // Life 2: the consumers arrive on a daemon that sheds everything (a
    // min_free no real filesystem has).
    let (code, l2) = life_json(&orders_config(&dir, order_consumers(), Some("999999GB")));
    assert_eq!(code, Some(0), "{l2}");
    assert!(!events(&l2, "pressure.shed").is_empty(), "{l2}");
    assert!(
        events(&l2, "start.fired").is_empty(),
        "nothing fires while shedding:\n{l2}"
    );
    let shed = events(&l2, "start.shed");
    let by = |w: &str| {
        shed.iter()
            .filter(|e| e["workflow"] == w)
            .cloned()
            .collect::<Vec<_>>()
    };
    // Each consumer offers the same thing every pass; the refusal is written
    // once, when it starts being held, and names what it holds.
    let each = by("each");
    assert_eq!(each.len(), 1, "one line per hold, not one per tick:\n{l2}");
    assert_eq!(each[0]["stream"], "orders", "{l2}");
    assert_eq!(each[0]["seq"], 1, "{l2}");
    assert!(
        each[0]["event_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{l2}"
    );
    let bulk = by("bulk");
    assert_eq!(bulk.len(), 1, "{l2}");
    assert_eq!(
        (&bulk[0]["from"], &bulk[0]["to"], &bulk[0]["events"]),
        (&json!(1), &json!(3), &json!(3)),
        "{l2}"
    );
    let join = by("join");
    assert_eq!(join.len(), 1, "{l2}");
    assert_eq!(join[0]["stream"], "orders", "{l2}");
    assert!(
        join[0]["correlation"]
            .as_str()
            .is_some_and(|c| c.starts_with("o-")),
        "{l2}"
    );

    // Life 3: no pressure. Every event the shed life held fires now, once.
    let (code, l3) = life_json(&orders_config(&dir, order_consumers(), None));
    assert_eq!(code, Some(0), "{l3}");
    assert_eq!(
        outputs(&l3, "each "),
        ["each #1", "each #2", "each #3"],
        "{l3}"
    );
    assert_eq!(
        outputs(&l3, "bulk "),
        ["bulk of 3 from #1", "bulk of 3 from #4"],
        "{l3}"
    );
    assert_eq!(
        outputs(&l3, "join "),
        [
            "join o-1 complete=true",
            "join o-2 complete=true",
            "join o-3 complete=true"
        ],
        "{l3}"
    );

    // Life 4: and they were consumed — nothing fires twice.
    let (code, l4) = life_json(&orders_config(&dir, order_consumers(), None));
    assert_eq!(code, Some(0), "{l4}");
    assert!(events(&l4, "start.fired").is_empty(), "{l4}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// **An event whose `inputs` cannot render is discarded, by name.**
///
/// It is the one refusal that is not temporary — the same payload fails the
/// same mapping every time — so holding it would wedge the consumer behind it
/// for ever. The consumer moves past it, and the line it writes names the
/// stream, the seq and the event id: that line is the event's only record.
#[test]
fn an_event_whose_inputs_cannot_render_is_skipped_with_a_line_naming_it() {
    let dir = common::unique_path("streams-inputs", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let consumer = |inputs: Value| {
        json!({"name": "each", "steps": {
            "take": {"kind": "stream", "stream": "orders", "subject": "order.paid",
                     "from": "earliest", "inputs": inputs},
            "f": {"kind": "finish", "depends_on": ["take"], "status": "completed", "output": "each fired"}
        }})
    };
    let broken = consumer(json!({"who": "{{payload.no_such_field.at_all}}"}));
    let (code, l1) = life_json(&orders_config(&dir, vec![order_producer(), broken], None));
    assert_eq!(code, Some(0), "{l1}");
    let invalid = events(&l1, "start.inputs.invalid");
    let seqs: Vec<&Value> = invalid.iter().map(|e| &e["seq"]).collect();
    assert_eq!(
        seqs,
        [&json!(1), &json!(2), &json!(3)],
        "each paid event once:\n{l1}"
    );
    assert!(
        invalid.iter().all(|e| e["stream"] == "orders"
            && e["workflow"] == "each"
            && e["event_id"].as_str().is_some_and(|id| !id.is_empty())),
        "{l1}"
    );
    assert!(outputs(&l1, "each ").is_empty(), "{l1}");

    // The offset moved past them: a consumer whose mapping is fixed does not
    // go back for events it already discarded.
    let fixed = consumer(json!({"who": "{{payload.subject}}"}));
    let (code, l2) = life_json(&orders_config(&dir, vec![fixed], None));
    assert_eq!(code, Some(0), "{l2}");
    assert!(events(&l2, "start.fired").is_empty(), "{l2}");
    assert!(events(&l2, "start.inputs.invalid").is_empty(), "{l2}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A consumer of `ticks`, a stream that keeps only its last three events.
fn tail_config(dir: &str, producer: bool, consumer: bool, extra: Value) -> Value {
    let mut workflows = Vec::new();
    if producer {
        workflows.push(json!({"name": "producer", "steps": {
            "s": {"kind": "once", "policy": "always"},
            "each": {"kind": "foreach", "depends_on": ["s"], "over": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
                     "batch": {"size": 1, "parallel": 1},
                     "body": {"steps": {"pub": {"kind": "emit", "stream": "ticks", "subject": "tick",
                                                "data": {"n": "{{item}}"}}}}},
            "f": {"kind": "finish", "depends_on": ["each"], "status": "completed"}
        }}));
    }
    if consumer {
        workflows.push(json!({"name": "tail", "steps": {
            "take": {"kind": "stream", "stream": "ticks", "from": "earliest"},
            "f": {"kind": "finish", "depends_on": ["take"], "status": "completed",
                  "output": "tail #{{steps.take.output.data.n}}"}
        }}));
    }
    let mut cfg = json!({
        "agent": {"name": "tail"},
        "store": file_store(dir, None),
        "streams": {"ticks": {"retention": {"max_events": 3}}},
        "workflows": workflows,
        "lifecycle": {"run_until": "idle", "idle_grace": "900ms"},
        "observability": {"log_level": "info", "log_content": true}
    });
    if let (Some(cfg), Some(extra)) = (cfg.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            cfg.insert(k.clone(), v.clone());
        }
    }
    cfg
}

/// Lives 1 and 2 of the trim tests: the consumer anchors at the start of an
/// empty stream, then ten events arrive while it is away — and retention
/// keeps only the last three.
fn consumer_falls_behind(dir: &str) {
    let (code, l1) = life_json(&tail_config(dir, false, true, json!({})));
    assert_eq!(code, Some(0), "{l1}");
    let (code, l2) = life_json(&tail_config(dir, true, false, json!({})));
    assert_eq!(code, Some(0), "{l2}");
    assert_eq!(events(&l2, "stream.emit").len(), 10, "{l2}");
}

/// **Retention trimming past a consumer is said, not done in silence**
/// (RFC 0045 D3). The seven events the consumer never read are gone; the line
/// says which and how many, once, and the three that remain fire.
#[test]
fn a_consumer_trimmed_past_logs_what_it_skipped() {
    let dir = common::unique_path("streams-trim", "d");
    std::fs::create_dir_all(&dir).unwrap();
    consumer_falls_behind(&dir);

    let (code, l3) = life_json(&tail_config(&dir, false, true, json!({})));
    assert_eq!(code, Some(0), "{l3}");
    let skipped = events(&l3, "stream.consumer.skipped");
    assert_eq!(skipped.len(), 1, "{l3}");
    let s = &skipped[0];
    assert_eq!(
        (&s["stream"], &s["workflow"], &s["node"]),
        (&json!("ticks"), &json!("tail"), &json!("take")),
        "{l3}"
    );
    assert_eq!(
        (&s["from"], &s["to"], &s["events"]),
        (&json!(1), &json!(7), &json!(7)),
        "{l3}"
    );
    assert_eq!(
        outputs(&l3, "tail "),
        ["tail #10", "tail #8", "tail #9"],
        "{l3}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **`agent_stream_lag` shows a consumer falling behind** (RFC 0045 D3): one
/// that was trimmed past and is now held by a shedding daemon still has the
/// three retained events to consume, and the gauge says three.
#[cfg(feature = "metrics")]
#[test]
fn the_lag_gauge_counts_what_a_held_consumer_has_not_consumed() {
    let dir = common::unique_path("streams-lag", "d");
    std::fs::create_dir_all(&dir).unwrap();
    consumer_falls_behind(&dir);

    let mut cfg = tail_config(
        &dir,
        false,
        true,
        json!({"observability": {"log_level": "info", "metrics_addr": "127.0.0.1:0"},
               "lifecycle": {"run_until": "drained"}}),
    );
    cfg["store"]["file"]["min_free"] = json!("999999GB");
    let cfg_path = common::unique_path("streams-lag", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("streams-lag", "log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn");
    let log = || std::fs::read_to_string(&err_path).unwrap_or_default();
    let want = "agent_stream_lag{stream=\"ticks\",consumer=\"tail/take\"} 3";
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut scraped = String::new();
    while std::time::Instant::now() < deadline {
        let addr = events(&log(), "metrics.serving")
            .first()
            .and_then(|e| e["addr"].as_str().map(str::to_string));
        if let Some(addr) = addr {
            scraped = common::http_get(&addr, "/metrics").body;
            if scraped.contains(want) {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // A few more passes: the skip forward persisted, so it is not found, and
    // reported, again on each of them.
    std::thread::sleep(std::time::Duration::from_millis(600));
    let _ = child.kill();
    let _ = child.wait();
    let l3 = log();
    assert!(scraped.contains(want), "{scraped}\n{l3}");
    assert_eq!(events(&l3, "stream.consumer.skipped").len(), 1, "{l3}");
    assert!(events(&l3, "start.fired").is_empty(), "{l3}");

    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&err_path);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A freshness freeze holds a consumer; it does not consume** (RFC 0045 D1).
///
/// The §7.7 freeze refuses new work while a signed instruction source cannot
/// be confirmed. A stream consumer used to treat that refusal as done and
/// move on. Here the registry dies, the freeze sets in, and only THEN does a
/// run that was already live emit three events: the consumer holds all three,
/// and the next life — on an instruction that needs no registry — fires each
/// exactly once.
///
/// The producer waits for the freeze on the runtime-events stream rather than
/// on a clock, so the order the test depends on is the order it observes.
#[cfg(feature = "internal-mocks")]
#[test]
fn a_frozen_consumer_holds_its_events_until_it_can_fire_them() {
    let dir = common::unique_path("streams-frozen", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let mut mock = common::spawn_mock_mcp("mock://watched", false);
    let consumer = json!({"name": "each", "steps": {
        "take": {"kind": "stream", "stream": "orders", "subject": "order.paid", "from": "earliest"},
        "f": {"kind": "finish", "depends_on": ["take"], "status": "completed",
              "output": "each #{{steps.take.output.data.n}}"}
    }});
    let producer = json!({"name": "producer", "steps": {
        "s": {"kind": "once", "policy": "always"},
        "frozen": {"kind": "wait", "depends_on": ["s"], "on": "event", "stream": "_rt",
                   "subject": "instruction.unavailable", "timeout": "30s"},
        "paid": {"kind": "foreach", "depends_on": ["frozen"], "over": [1, 2, 3],
                 "batch": {"size": 1, "parallel": 1},
                 "body": {"steps": {"pub": {"kind": "emit", "stream": "orders", "subject": "order.paid",
                                            "data": {"n": "{{item}}"}}}}},
        "f": {"kind": "finish", "depends_on": ["paid"], "status": "completed"}
    }});
    let cfg = json!({
        "agent": {"name": "frozen", "preflight": "never",
                  "instruction": {"mcp": "instruction://ins_mock@stable", "refresh": "1s",
                                  "unavailable": "freeze"},
                  "document_capabilities": ["compute"]},
        "mcp": {"servers": [{"name": "registry", "endpoint": format!("{}/mcp", mock.uri())}]},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": file_store(&dir, None),
        "streams": {"orders": {"retention": {"max_events": 100}},
                    "_rt": {"retention": {"max_events": 1000}}},
        "workflows": [producer, consumer.clone()],
        "lifecycle": {"run_until": "drained"},
        "observability": {"log_level": "info", "log_content": true,
                          "runtime_events": {"stream": "_rt", "include": ["instruction"]}}
    });
    let cfg_path = common::unique_path("streams-frozen", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("streams-frozen", "log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn");
    let log = || std::fs::read_to_string(&err_path).unwrap_or_default();
    let wait_for = |what: &str, cond: &dyn Fn(&str) -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let l = log();
            if cond(&l) {
                return l;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}:\n{l}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    wait_for("the instruction to load", &|l| {
        !events(l, "instruction.loaded").is_empty()
    });
    mock.stop();
    wait_for("the producer to finish under the freeze", &|l| {
        events(l, "run.done")
            .iter()
            .any(|e| e["workflow"] == "producer")
            && !events(l, "start.frozen").is_empty()
    });
    // Let a few more passes go by: a held consumer must stay held, quietly.
    std::thread::sleep(std::time::Duration::from_millis(600));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    let l1 = log();
    assert_eq!(events(&l1, "stream.emit").len(), 3, "{l1}");
    assert!(
        outputs(&l1, "each ").is_empty(),
        "nothing fires while frozen:\n{l1}"
    );
    let frozen: Vec<Value> = events(&l1, "start.frozen")
        .into_iter()
        .filter(|e| e["workflow"] == "each")
        .collect();
    assert_eq!(frozen.len(), 1, "one line per hold:\n{l1}");
    assert_eq!(
        (&frozen[0]["stream"], &frozen[0]["seq"]),
        (&json!("orders"), &json!(1)),
        "{l1}"
    );

    // Life 2: an instruction with nothing to re-read, so nothing freezes.
    let (code, l2) = life_json(&json!({
        "agent": {"name": "frozen", "instruction": "You consume orders.", "preflight": "never"},
        "store": file_store(&dir, None),
        "streams": {"orders": {"retention": {"max_events": 100}}},
        "workflows": [consumer],
        "lifecycle": {"run_until": "idle", "idle_grace": "900ms"},
        "observability": {"log_level": "info", "log_content": true}
    }));
    assert_eq!(code, Some(0), "{l2}");
    assert_eq!(
        outputs(&l2, "each "),
        ["each #1", "each #2", "each #3"],
        "{l2}"
    );

    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&err_path);
    let _ = std::fs::remove_dir_all(&dir);
}
