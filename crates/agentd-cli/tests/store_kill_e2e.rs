// SPDX-License-Identifier: AGPL-3.0-only
//! **A kept stream event is never overwritten**, across a crash at the worst
//! instant.
//!
//! An append writes the event record at once, under a key derived from the
//! stream head, but the head itself lives in the manifest, which is flushed
//! debounced. A process killed between the two leaves an event past the head
//! the next life does not know about. The next append derives the same key —
//! and the store's restore-gap rule, which adopts a record it finds on a key
//! it has not touched yet, used to overwrite the event with the new one.
//!
//! Life 1 here dies (SIGKILL, via the `stream.after_append` kill point) right
//! after its event lands and before the head is recorded. Life 2 appends
//! again, with a consumer reading the stream from the start: the first life's
//! event must still be there, and the new one must take the next seq.
#![cfg(all(unix, any(feature = "internal-mocks", debug_assertions)))]

mod common;

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

use serde_json::Value;

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// One producer that emits `item` once per life; with `consumer`, a second
/// workflow that reads the stream from the start. The checkpoint debounce is
/// long, so nothing but the end of the process records the stream head —
/// which is exactly the window under test.
///
/// The producer's runs are not durable: a restored run would replay its emit
/// under the same event id, and an overwrite by an identical copy would hide
/// the loss. What the window threatens is a DIFFERENT event landing on the
/// kept one's key, so life 2's append is a new run with a new event.
fn config(dir: &str, item: &str, consumer: bool) -> String {
    let consumer = if consumer {
        "  - name: fulfil\n    steps:\n\
         \x20     take: {kind: stream, stream: orders, subject: \"order.*\", from: earliest}\n\
         \x20     f:    {kind: finish, depends_on: [take], status: completed, output: \"got {{steps.take.output.data.item}} at {{steps.take.output.seq}}\"}\n"
    } else {
        ""
    };
    format!(
        "agent:\n  name: kept\n\
         store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 60000\n\
         streams:\n  orders:\n    retention: {{ max_events: 100 }}\n\
         workflows:\n  - name: producer\n    durable: false\n    steps:\n\
         \x20     s:   {{kind: once, policy: always}}\n\
         \x20     pub: {{kind: emit, depends_on: [s], stream: orders, subject: \"order.paid\", data: {{item: {item}}}}}\n\
         \x20     f:   {{kind: finish, depends_on: [pub], status: completed}}\n{consumer}\
         lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

fn life(cfg: &str, kill_at: Option<&str>) -> (std::process::ExitStatus, String) {
    let err_path = common::unique_path("store-kill", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--config", cfg]);
    if let Some(point) = kill_at {
        cmd.env("AGENTD_TEST_KILL_AT", point);
    }
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .status()
        .expect("run");
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    (status, log)
}

#[test]
fn an_event_kept_before_a_crash_survives_and_the_next_append_takes_a_new_seq() {
    let dir = common::unique_path("store-kill", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");

    // Life 1: the event lands, then the process dies before the head does.
    std::fs::write(&cfg, config(&dir, "first", false)).unwrap();
    let (status, l1) = life(&cfg, Some("stream.after_append"));
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "life 1 dies at the kill point:\n{l1}"
    );

    // Life 2: append again, and read the whole stream.
    std::fs::write(&cfg, config(&dir, "second", true)).unwrap();
    let (status, l2) = life(&cfg, None);
    assert_eq!(status.code(), Some(0), "{l2}");
    let emitted: Vec<u64> = events(&l2, "stream.emit")
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert_eq!(
        emitted,
        [2],
        "the new append steps over the kept event:\n{l2}"
    );
    let recovered = events(&l2, "stream.head.recovered");
    assert_eq!(recovered.len(), 1, "the recovery is logged:\n{l2}");
    assert_eq!(recovered[0]["from"], 1);
    assert_eq!(recovered[0]["to"], 1);
    let mut got: Vec<String> = events(&l2, "run.done")
        .iter()
        .filter(|e| e["workflow"] == "fulfil")
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .collect();
    got.sort();
    assert_eq!(
        got,
        ["got first at 1", "got second at 2"],
        "both events are on the stream, each under its own seq:\n{l2}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
