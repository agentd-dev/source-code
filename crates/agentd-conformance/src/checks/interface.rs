// SPDX-License-Identifier: AGPL-3.0-only
//! The display-client interface. A daemon with `a2a.events.enabled` serves the
//! observation plane on its A2A listener: the `agentd.events/SubscribeToEvents` feed
//! (hello → events, cursor replay), the taskless reads, and the
//! human-in-the-loop gate (`ask_human` → `input-required` → a `taskId` reply
//! resumes the asker). With the interface OFF the surface refuses and the core
//! A2A wire still answers — the default-OFF contract, so enabling an
//! observation plane is always a deliberate act and never a side effect.
//! And the browser path: CORS admits exactly the listed origins, admission is
//! not trust, and the public card is readable from any origin.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::checks::util::{
    feed_activation, feed_method, mock_llm, open, post, rpc, rpc_body, rpc_value, send_command,
    text_params, wait_ready, write_file,
};
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "interface/default-off-gate",
            category: Category::Interface,
            desc: "without a2a.events.enabled the feed's method is not offered (-32601 EXTENSION_NOT_DECLARED), and the core answers",
            run: default_off,
        },
        Check {
            id: "interface/feed-hello-and-replay",
            category: Category::Interface,
            desc: "the feed is refused until the events extension is activated (-32601 EXTENSION_NOT_ACTIVATED) and takes only {fromSeq} (any other member is -32602), then opens with a hello {seq, resume, resync, introspection, version} and replays from seq 0 a task event whose history holds the prompt",
            run: feed_replay,
        },
        Check {
            id: "interface/hitl-gate-roundtrip",
            category: Category::Interface,
            desc: "ask_human gates the task as input-required; a taskId reply resumes the turn",
            run: hitl_roundtrip,
        },
        Check {
            id: "interface/cors-exact-origins",
            category: Category::Interface,
            desc: "only a listed origin is granted (a2a-version preflighted, the challenge readable); an unlisted one — loopback too — is 403 with no grant; the card is public (ACAO *)",
            run: cors_exact_origins,
        },
    ]
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// One raw HTTP exchange; `(status, headers lowercased, body)`. By hand,
/// because a browser's request — its `Origin`, its preflight — is the thing
/// under test.
fn exchange(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, Vec<(String, String)>, String) {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: x\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(req.as_bytes()).expect("write request");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let code = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (code, headers, body.to_string())
}

fn header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn config(llm: &str, port: u16, feed: bool) -> String {
    let feed = if feed {
        "  events:\n    enabled: true\n"
    } else {
        ""
    };
    format!(
        "\
         agent:\n  name: iface-conf\n  instruction: You are a helpful test agent.\n  preflight: never\n\
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

    // The feed is not served — not even to a caller activating its
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

    let body = rpc_body(9, feed_method(), json!({"fromSeq": 0}));
    // The method belongs to the events extension: without activating it, it
    // is refused.
    let bare = post(&addr, &body, &[]);
    if !(bare.contains("-32601") && bare.contains("EXTENSION_NOT_ACTIVATED")) {
        return Outcome::fail(format!(
            "the feed without the events extension activated should be -32601 EXTENSION_NOT_ACTIVATED: {bare}"
        ));
    }
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

fn hitl_roundtrip(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(
        h,
        &tmp,
        &json!({
            "turns": [
                {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Proceed?"}}]},
                {"content": "Proceeded."}
            ]
        }),
    );
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &config(&llm.uri, port, true));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        text_params("Do the thing", None, true),
    );
    let task_id = sent["task"]["id"].as_str().expect("task id").to_string();
    // The gate appears…
    let deadline = Instant::now() + Duration::from_secs(8);
    let gated = loop {
        let t = rpc(&addr, 2, "GetTask", json!({"id": task_id}));
        if t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED" {
            break t;
        }
        assert!(Instant::now() < deadline, "no gate: {t}");
        std::thread::sleep(Duration::from_millis(80));
    };
    Outcome::require(
        gated["status"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|q| q.contains("Proceed?")),
        format!("the gate should carry the question: {gated}"),
    )
    .and(|| {
        // …and the taskId reply resumes the turn to completion.
        rpc(
            &addr,
            3,
            "SendMessage",
            text_params("yes", Some(&task_id), true),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = rpc(&addr, 4, "GetTask", json!({"id": task_id}));
            if t["status"]["state"] == "TASK_STATE_COMPLETED" {
                return Outcome::require(
                    t["artifacts"][0]["parts"][0]["text"]
                        .as_str()
                        .is_some_and(|a| a.contains("Proceeded")),
                    format!("the resumed turn's answer should land as the artifact: {t}"),
                );
            }
            if Instant::now() >= deadline {
                return Outcome::fail(format!("the reply should resume the turn: {t}"));
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    })
}

/// The browser path under the strict policy, on a no-auth loopback daemon that
/// lists one UI origin: the listed origin is granted and preflighted with
/// `a2a-version`; its uncredentialed POST is challenged (a browser is never
/// the implicit operator) with the grant and the challenge exposed; another
/// loopback port is refused like any foreign site; and the card is readable
/// from anywhere.
fn cors_exact_origins(h: &Harness) -> Outcome {
    const UI: &str = "http://127.0.0.1:4173";
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = config(&llm.uri, port, false).replace(
        "  listen: ",
        &format!("  cors:\n    origins: [\"{UI}\"]\n  listen: "),
    );
    let cfg = write_file(&tmp, "agentd.yaml", &cfg);
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let preflight = |origin: &str| {
        exchange(
            &addr,
            "OPTIONS",
            "/",
            &[
                ("Origin", origin),
                ("Access-Control-Request-Method", "POST"),
                (
                    "Access-Control-Request-Headers",
                    "content-type, a2a-version",
                ),
            ],
            "",
        )
    };
    let body = rpc_body(1, "ListTasks", json!({}));
    let post = |origin: &str| {
        exchange(
            &addr,
            "POST",
            "/",
            &[
                ("Origin", origin),
                ("Content-Type", "application/json"),
                ("A2A-Version", "1.0"),
            ],
            &body,
        )
    };

    let (code, hs, _) = preflight(UI);
    Outcome::require(
        code == 204
            && header(&hs, "access-control-allow-origin") == Some(UI)
            && header(&hs, "access-control-allow-headers")
                .is_some_and(|v| v.split(',').any(|h| h.trim() == "a2a-version")),
        format!("the listed origin's preflight should be granted with a2a-version: {code} {hs:?}"),
    )
    .and(|| {
        let (code, hs, reply) = post(UI);
        let exposed = header(&hs, "access-control-expose-headers").unwrap_or("");
        Outcome::require(
            code == 401
                && reply.contains("-31401")
                && header(&hs, "access-control-allow-origin") == Some(UI)
                && exposed.split(',').any(|h| h.trim() == "www-authenticate"),
            format!("a listed origin without a credential should get a readable 401 challenge: {code} {hs:?} {reply}"),
        )
    })
    .and(|| {
        for origin in ["http://127.0.0.1:9999", "https://evil.example", "null"] {
            for (what, (code, hs, _)) in [("preflight", preflight(origin)), ("POST", post(origin))] {
                if code != 403 || header(&hs, "access-control-allow-origin").is_some() {
                    return Outcome::fail(format!(
                        "an unlisted origin's {what} ({origin}) should be 403 with no grant: {code} {hs:?}"
                    ));
                }
            }
        }
        Outcome::pass()
    })
    .and(|| {
        let (code, hs, _) = exchange(
            &addr,
            "GET",
            "/.well-known/agent-card.json",
            &[("Origin", "https://evil.example")],
            "",
        );
        Outcome::require(
            code == 200
                && header(&hs, "access-control-allow-origin") == Some("*")
                && header(&hs, "access-control-allow-credentials").is_none(),
            format!("the public card should be readable from any origin, without credentials: {code} {hs:?}"),
        )
    })
}
