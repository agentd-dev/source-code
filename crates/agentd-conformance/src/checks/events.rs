// SPDX-License-Identifier: AGPL-3.0-only
//! The observation feed. A daemon with `a2a.events.enabled` declares the events
//! extension on its card and serves the extension's one method,
//! `agentd.events/SubscribeToEvents`: a stream that opens with a hello and
//! replays the ring from a cursor. With the switch OFF the card does not
//! declare the extension, its method is refused, and the core A2A wire still
//! answers — the default-OFF contract, so an observation plane is always a
//! deliberate act and never a side effect.
//!
//! That the method is refused until the request activates its extension is the
//! extensions family's to prove, for every method an extension declares.

use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::checks::util::{
    EVENTS, declared_extensions, feed_activation, feed_method, free_port, get_card, mock_llm, open,
    post, rpc_body, rpc_value, send_command, text_params, wait_ready, write_file,
};
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "events/default-off-gate",
            category: Category::Events,
            desc: "without a2a.events.enabled the card does not declare the events extension, its method is -32601 EXTENSION_NOT_DECLARED even to a caller activating it, and the core still answers",
            run: default_off,
        },
        Check {
            id: "events/feed-hello-and-replay",
            category: Category::Events,
            desc: "with the feed on, the card declares the events extension; the feed takes only {fromSeq} (any other member is a plain -32602), then opens with a hello {seq, resume, resync, introspection, version} and replays from seq 0 a task event whose history holds the prompt",
            run: feed_replay,
        },
    ]
}

pub(crate) fn config(llm: &str, port: u16, feed: bool) -> String {
    let feed = if feed {
        "  events:\n    enabled: true\n"
    } else {
        ""
    };
    format!(
        "\
         agent:\n  name: events-conf\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{feed}\
         lifecycle:\n  run_until: drained\n"
    )
}

fn default_off(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &config(&llm.uri, port, false));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    // The card does not offer what the instance does not serve…
    let card = get_card(&addr);
    Outcome::require(
        !declared_extensions(&card).iter().any(|u| u == EVENTS),
        format!("the card should not declare the events extension while it is off: {card}"),
    )
    .and(|| {
        // …and the feed is not served — not even to a caller activating its
        // extension, which this instance does not declare…
        let feed = post(
            &addr,
            &rpc_body(1, feed_method(), json!({})),
            &[("A2A-Extensions", feed_activation())],
        );
        Outcome::require(
            feed.contains("-32601") && feed.contains("EXTENSION_NOT_DECLARED"),
            format!("the feed should be -32601 EXTENSION_NOT_DECLARED while disabled: {feed}"),
        )
    })
    .and(|| {
        // …and the core surface is untouched: status still answers, as a
        // Message carrying the document.
        let st = send_command(&addr, 3, "status");
        Outcome::require(
            st["result"]["message"]["parts"][0]["data"]["runs"].is_array()
                && st["result"].get("task").is_none(),
            format!("the core status command should still answer with a Message: {st}"),
        )
    })
}

fn feed_replay(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "The reply."}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &config(&llm.uri, port, true));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let card = get_card(&addr);
    if !declared_extensions(&card).iter().any(|u| u == EVENTS) {
        return Outcome::fail(format!(
            "with a2a.events.enabled the card should declare the events extension: {card}"
        ));
    }

    // Create history FIRST, then subscribe from seq 0 — the ring must replay
    // to the late joiner a `task` event whose history holds the prompt,
    // identified by the id it was sent with.
    let params = text_params("Replay me", None, false);
    let prompt_id = params["message"]["messageId"].clone();
    let sent = rpc_value(&addr, 1, "SendMessage", params);
    assert_eq!(
        sent["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );

    // Its params are `{fromSeq?}` and nothing else: a member the method does
    // not define is refused, as plain JSON, rather than replayed from the
    // start.
    let stray = post(
        &addr,
        &rpc_body(10, feed_method(), json!({"cursor": 0})),
        &[("A2A-Extensions", feed_activation())],
    );
    if !(stray.contains("-32602") && stray.contains("params.cursor")) || stray.contains("data:") {
        return Outcome::fail(format!(
            "the feed given `cursor` should be a plain -32602 naming params.cursor: {stray}"
        ));
    }
    let body = rpc_body(9, feed_method(), json!({"fromSeq": 0}));
    let s = open(&addr, &body, &[("A2A-Extensions", feed_activation())]);
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(s);
    let mut hello = Value::Null;
    let mut saw_hello = false;
    let mut saw_prompt = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && !(saw_hello && saw_prompt) {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(v) = serde_json::from_str::<Value>(data.trim())
        {
            let r = &v["result"];
            if let Some(h) = r.get("hello") {
                saw_hello = true;
                hello = h.clone();
            }
            let first = &r["event"]["data"]["task"]["history"][0];
            if r["event"]["kind"] == "task"
                && first["messageId"] == prompt_id
                && first["parts"][0]["text"] == "Replay me"
            {
                saw_prompt = true;
            }
        }
    }
    Outcome::require(
        saw_hello,
        "the stream should open with a hello frame".to_string(),
    )
    .and(|| {
        // Exactly the five fields, the switch as it stands (off here) and the
        // build's version, and nothing else.
        let mut fields: Vec<&str> = hello
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        fields.sort_unstable();
        Outcome::require(
            fields == ["introspection", "resume", "resync", "seq", "version"]
                && hello["introspection"] == false
                && hello["version"].as_str().is_some_and(|v| !v.is_empty()),
            format!("the hello should be {{seq, resume, resync, introspection, version}}: {hello}"),
        )
    })
    .and(|| {
        Outcome::require(
            saw_prompt,
            "the ring should replay, from seq 0, a task event whose history holds the prompt"
                .to_string(),
        )
    })
}
