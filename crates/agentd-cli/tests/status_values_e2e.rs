// SPDX-License-Identifier: AGPL-3.0-only
//! **What a display client reads off `status` besides the work.**
//!
//! A client draws its chrome from three facts only the daemon has: the build
//! it talks to (`version`), the prefix that makes a message preload a skill
//! (`skill_prefix`, so `@` completes to text that actually preloads), and the
//! memory values the operator chose to publish (`values`, for a
//! `memory:<key>` item). The last is the one with rules worth pinning:
//!
//! - only the keys `observability.status_values` lists are published — a
//!   workflow's other memory is not the chrome's to show;
//! - a key never written is left out, not shown empty;
//! - TTL is honoured, so a value whose producer stopped refreshing it
//!   disappears instead of reading as current;
//! - every named caller reads them, not only the operator, and the operator's
//!   feed carries them on its `status` event.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::SendMessage;

const TOKEN_OP: &str = "status-values-operator-token";
const TOKEN_USER: &str = "status-values-user-token";

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

/// The `status` document as `bearer` reads it.
fn status_as(addr: &str, bearer: &str) -> Value {
    let v = SendMessage::command("status", json!({}))
        .bearer(bearer)
        .post(addr);
    let doc = v["result"]["message"]["parts"][0]["data"].clone();
    assert!(doc.is_object(), "status as {bearer}: {v}");
    doc
}

/// Poll `status` as `bearer` until `pred` holds of it.
fn wait_status(addr: &str, bearer: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let doc = status_as(addr, bearer);
        if pred(&doc) {
            return doc;
        }
        assert!(Instant::now() < deadline, "{what}: {doc}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn status_values_and_skill_prefix_are_published() {
    // `publish` writes a listed key, a listed key with a short TTL, and a key
    // nobody listed; `never.set` is listed and never written. The daemon is
    // spawned on a probed port and talked to at the authority it confirmed
    // binding (`common::spawn_bound`), never at the probe.
    let cfg = common::unique_path("status-values", "yaml");
    let (mut daemon, addr) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            format!(
                "\
                 agent:\n  name: status-values\n  instruction: You are a test agent.\n  preflight: never\n\
                 intelligence:\n  endpoints: https://127.0.0.1:9\n  model: mock\n\
                 store:\n  kind: memory\n\
                 skills:\n  reference_prefix: \"#skill:\"\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n  events:\n    enabled: true\n\
                 \x20 principals:\n\
                 \x20   - id: op\n\
                 \x20     match: {{ bearer_ref: \"{{{{secret:STATUS_VALUES_OP}}}}\" }}\n\
                 \x20     role: operator\n\
                 \x20   - id: viewer\n\
                 \x20     match: {{ bearer_ref: \"{{{{secret:STATUS_VALUES_USER}}}}\" }}\n\
                 \x20     role: user\n\
                 observability:\n  log_level: info\n  status_values: [deploy.state, fleeting, never.set]\n\
                 workflows:\n\
                 \x20 - name: publish\n    steps:\n\
                 \x20     s: {{kind: once}}\n\
                 \x20     a: {{kind: memory.set, depends_on: [s], key: deploy.state, value: green}}\n\
                 \x20     b: {{kind: memory.set, depends_on: [a], key: fleeting, value: soon gone, ttl: 2s}}\n\
                 \x20     c: {{kind: memory.set, depends_on: [b], key: unlisted, value: not for the chrome}}\n\
                 \x20     f: {{kind: finish, depends_on: [c]}}\n\
                 lifecycle:\n  run_until: drained\n"
            ),
        )
        .unwrap();
        let stderr_path = common::unique_path("status-values-daemon", "log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg])
            .env("STATUS_VALUES_OP", TOKEN_OP)
            .env("STATUS_VALUES_USER", TOKEN_USER)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .spawn()
            .expect("spawn agentd");
        (
            Daemon {
                child,
                stderr_path: stderr_path.clone(),
            },
            stderr_path,
        )
    });

    // The operator's feed, opened before the values land, so it sees the
    // status event that carries them.
    let mut feed = common::a2a_open(
        &addr,
        &common::rpc_body(7, common::feed_method(), json!({"fromSeq": 0})),
        &[
            ("Authorization", &format!("Bearer {TOKEN_OP}")),
            ("A2A-Extensions", &common::feed_extensions()),
        ],
        Duration::from_secs(10),
    );

    let published = json!({"deploy.state": "green", "fleeting": "soon gone"});
    for bearer in [TOKEN_OP, TOKEN_USER] {
        let doc = wait_status(&addr, bearer, "the values never landed", |d| {
            d["values"] == published
        });
        assert_eq!(doc["skill_prefix"], "#skill:", "{doc}");
        assert_eq!(doc["version"], agentd::VERSION, "{doc}");
    }

    let mut status_event = None;
    common::read_frames(&mut feed, |v| {
        let ev = &v["result"]["event"];
        if ev["kind"] == "status" && ev["data"]["values"] == published {
            status_event = Some(ev["data"].clone());
            return false;
        }
        true
    });
    let status_event = status_event.expect("no status event carrying the values");
    assert_eq!(status_event["skill_prefix"], "#skill:", "{status_event}");
    assert_eq!(status_event["version"], agentd::VERSION, "{status_event}");

    // The TTL runs out (2 s, plus the one-second cache): the key goes, the
    // other stays.
    let doc = wait_status(&addr, TOKEN_USER, "the expired value is still shown", |d| {
        d["values"] == json!({"deploy.state": "green"})
    });
    assert!(doc["values"].get("unlisted").is_none(), "{doc}");

    assert!(daemon.alive(), "daemon still serving: {}", daemon.stderr());
    drop(feed);
    std::fs::remove_file(&cfg).ok();
}
