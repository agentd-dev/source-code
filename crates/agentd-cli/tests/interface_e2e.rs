// SPDX-License-Identifier: AGPL-3.0-only
//! The **display surface** end to end: a daemon with `a2a.events.enabled`
//! (+ `a2a.introspection.enabled`) serves the display-client contract over its
//! real A2A listener — the card's extension declarations, the global
//! `SubscribeToEvents` SSE feed (cross-client transcript sync + cursor resume),
//! the taskless introspection reads (`conversation.get` with message bodies,
//! `run.get` with per-step detail, `debug.events` log-ring tail), the
//! browser-origin CORS path, the disabled-by-default gate, and the removed
//! pairing exchange. The `agentd tui|ui` launcher lives in `launcher_e2e`.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{SendMessage, error_of, get_card, rpc_result as rpc};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn command(addr: &str, op: &str, args: Value) -> Value {
    SendMessage::command(op, args).result(addr)
}

/// A read op: its reply is a Message carrying the document, and no task.
fn read(addr: &str, op: &str, args: Value) -> Value {
    let r = command(addr, op, args);
    assert!(r.get("task").is_none(), "{op} created a task: {r}");
    assert_eq!(r["message"]["role"], "ROLE_AGENT", "{op}: {r}");
    r["message"]["parts"][0]["data"].clone()
}

struct MockLlm {
    child: Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}
fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("iface-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("iface-mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}
fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("iface-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd daemon");
    Daemon { child, stderr_path }
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-iface", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

/// Spawn the daemon on a probed free port and return the authority IT actually
/// bound. The probe→bind gap is a real race under parallel CI (another process
/// can take the port), so a daemon whose bind lost is retried on a fresh port
/// rather than leaving the test talking to a stranger's listener.
fn spawn_bound(cfg_for: impl Fn(u16) -> String) -> (Daemon, String, String) {
    spawn_bound_with(cfg_for, spawn_daemon)
}

fn spawn_bound_with(
    cfg_for: impl Fn(u16) -> String,
    spawn: impl Fn(&str) -> Daemon,
) -> (Daemon, String, String) {
    for _ in 0..5 {
        let cfg = write_config(&cfg_for(free_port()));
        let daemon = spawn(&cfg);
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(15)) {
            return (daemon, addr, cfg);
        }
        std::fs::remove_file(&cfg).ok();
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

/// A loopback daemon (⇒ operator) with the feed on; `debug` (introspection) +
/// `extra` shape each test.
fn iface_config(llm: &str, port: u16, debug: bool, extra: &str) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: iface-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n  events:\n    enabled: true\n  introspection:\n    enabled: {debug}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n{extra}"
    )
}

/// [`iface_config`] with `a2a.cors.origins: [<origins>]`.
fn with_origins(llm: &str, port: u16, origins: &str) -> String {
    iface_config(llm, port, false, "").replace(
        "  events:\n    enabled: true\n",
        &format!("  cors:\n    origins: [{origins}]\n  events:\n    enabled: true\n"),
    )
}

/// Open a `SubscribeToEvents` SSE stream; frames (each a JSON-RPC response's
/// `result`) are appended to the shared vec until the connection closes or the
/// socket read times out.
fn subscribe_events(addr: &str, from_seq: u64, sink: Arc<Mutex<Vec<Value>>>) {
    let mut reader = common::subscribe_feed(addr, from_seq, Duration::from_secs(20));
    common::read_frames(&mut reader, |v| {
        if let Some(result) = v.get("result") {
            sink.lock().unwrap().push(result.clone());
        }
        true
    });
}

fn wait_for<F: Fn(&[Value]) -> bool>(sink: &Arc<Mutex<Vec<Value>>>, secs: u64, pred: F) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        {
            let got = sink.lock().unwrap();
            if pred(&got) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "condition never held; frames: {:#?}",
                *got
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_introspection_reads_work_over_a2a() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "Hello from the mock."}]}));
    let extra = "workflows:\n  - name: greet\n    steps:\n      s: {kind: manual}\n      f: {kind: finish, depends_on: [s], output: \"done\"}\n";
    let (_daemon, addr, cfg) = spawn_bound(|port| iface_config(&llm.uri, port, true, extra));

    // Taskless: a read creates NO durable task. (Proto3 JSON omits a field at
    // its default value, so "no tasks" arrives as an absent `tasks` rather
    // than an empty array — which is exactly what is being asserted.)
    let count = |v: &Value| v["tasks"].as_array().map(Vec::len).unwrap_or(0);
    let tasks_before = count(&rpc(&addr, 2, "ListTasks", json!({})));
    let _ = read(&addr, "status", json!({}));
    let _ = read(&addr, "debug.events", json!({"limit": 1}));
    let tasks_after = count(&rpc(&addr, 4, "ListTasks", json!({})));
    assert_eq!(tasks_before, tasks_after, "reads are taskless");

    // The agent card advertises the surface (public discovery). Position is
    // not the claim — the command vocabulary is declared on every card — so
    // this asks whether the interface extension is THERE.
    let card = get_card(&addr);
    let uris: Vec<&str> = card["capabilities"]["extensions"]
        .as_array()
        .expect("the card declares its extensions")
        .iter()
        .filter_map(|e| e["uri"].as_str())
        .collect();
    assert!(
        uris.contains(&"https://agentd.dev/a2a/ext/interface/v1"),
        "the interface extension is declared: {uris:?}"
    );

    // A conversation turn, then read its transcript (debug).
    let sent = SendMessage::text("Say hello").result(&addr);
    let ctx = sent["task"]["contextId"].as_str().unwrap().to_string();
    assert_eq!(sent["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    let conv = read(&addr, "conversation.get", json!({"id": ctx}));
    let msgs = conv["conversation"]["messages"].as_array().unwrap();
    assert!(
        msgs.iter()
            .any(|m| m["role"] == "user" && m["text"].as_str().unwrap_or("").contains("Say hello")),
        "user message in transcript: {msgs:#?}"
    );
    assert!(
        msgs.iter().any(|m| m["role"] == "assistant"),
        "assistant reply in transcript"
    );

    // A run with per-step detail (debug).
    let run_task = command(&addr, "workflow.run", json!({"workflow": "greet"}));
    let run_task_id = run_task["task"]["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut run_id = String::new();
    while Instant::now() < deadline {
        let got = rpc(&addr, 9, "GetTask", json!({"id": run_task_id}));
        if got["status"]["state"] == "TASK_STATE_COMPLETED" {
            let ws = read(&addr, "workflow.status", json!({}));
            run_id = ws["runs"][0]["run"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_default();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!run_id.is_empty(), "workflow.status yielded the run id");
    let run = read(&addr, "run.get", json!({"run": run_id}));
    assert_eq!(run["run"]["status"], "completed", "{run}");
    let steps = run["run"]["steps"].as_object().unwrap();
    assert_eq!(steps["f"]["status"], "done", "per-step detail: {steps:?}");
    assert!(steps["f"]["finished"].is_u64());

    // The live log ring (debug) has lines, cursored.
    let ev = read(&addr, "debug.events", json!({"limit": 50}));
    let events = ev["events"].as_array().unwrap();
    assert!(!events.is_empty(), "the event ring is live");
    assert!(events[0]["seq"].is_u64() && events[0]["event"].is_string());
    let newest = ev["newest_seq"].as_u64().unwrap();
    let again = read(&addr, "debug.events", json!({"after": newest}));
    assert!(
        again["events"].as_array().unwrap().len() <= events.len(),
        "the cursor advances"
    );

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn subscribe_to_events_streams_cross_client_activity_and_resumes() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "The reply."}]}));
    let (_daemon, addr, cfg) = spawn_bound(|port| iface_config(&llm.uri, port, false, ""));

    // Client A: attach to the feed.
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let addr2 = addr.clone();
    std::thread::spawn(move || subscribe_events(&addr2, 0, sink));
    wait_for(&frames, 5, |f| f.iter().any(|v| v.get("hello").is_some()));
    {
        let f = frames.lock().unwrap();
        let hello = f.iter().find(|v| v.get("hello").is_some()).unwrap();
        assert_eq!(hello["hello"]["debug"], false);
        assert_eq!(hello["hello"]["resync"], false);
    }

    // Client B: send a prompt on a separate connection (blocking).
    let prompt = SendMessage::text("Ping across clients");
    let prompt_id = prompt.params()["message"]["messageId"].clone();
    let sent = prompt.result(&addr);
    assert_eq!(sent["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    // Client A observes B's prompt — in the history of the task it opened,
    // under the id B sent it with — AND the task reaching terminal with the
    // artifact: the cross-client transcript from the core Task, no polling and
    // no event of agentd's own restating what was said.
    wait_for(&frames, 10, |f| {
        f.iter().any(|v| {
            let first = &v["event"]["data"]["task"]["history"][0];
            v["event"]["kind"] == "task"
                && first["messageId"] == prompt_id
                && first["role"] == "ROLE_USER"
                && first["parts"][0]["text"] == "Ping across clients"
        })
    });
    {
        let f = frames.lock().unwrap();
        assert!(
            !f.iter().any(|v| v["event"]["kind"] == "message"),
            "the `message` feed kind is gone: {f:#?}"
        );
    }
    wait_for(&frames, 10, |f| {
        f.iter().any(|v| {
            v["event"]["kind"] == "task"
                && v["event"]["data"]["task"]["status"]["state"] == "TASK_STATE_COMPLETED"
                && v["event"]["data"]["task"]["artifacts"][0]["parts"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("The reply."))
        })
    });

    // Resume: a second subscriber from the observed cursor sees NOTHING old.
    let max_seq = {
        let f = frames.lock().unwrap();
        f.iter()
            .filter_map(|v| v["event"]["seq"].as_u64())
            .max()
            .unwrap()
    };
    let resumed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink2 = Arc::clone(&resumed);
    let addr3 = addr.clone();
    std::thread::spawn(move || subscribe_events(&addr3, max_seq, sink2));
    wait_for(&resumed, 5, |f| f.iter().any(|v| v.get("hello").is_some()));
    std::thread::sleep(Duration::from_millis(400));
    {
        let f = resumed.lock().unwrap();
        assert!(
            f.iter()
                .filter_map(|v| v["event"]["seq"].as_u64())
                .all(|s| s > max_seq),
            "no replayed event at or before the cursor: {f:#?}"
        );
    }

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn the_interface_is_gated_off_by_default() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    // Neither switch: the surface must refuse, the core must be untouched.
    let (_daemon, addr, cfg) = spawn_bound(|port| {
        format!(
            "config_version: \"1\"\n\
         agent:\n  name: iface-off\n  instruction: Test.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         lifecycle:\n  run_until: drained\n",
            llm.uri
        )
    });

    // The removed discovery op is refused by name, with its replacement…
    let (code, msg) = error_of(&SendMessage::command("interface.info", json!({})).post(&addr));
    assert_eq!(code, -32602);
    assert!(msg.contains("removed") && msg.contains("status"), "{msg}");
    // …introspection reads refuse, naming their own switch…
    let (code, msg) = error_of(&SendMessage::command("debug.events", json!({})).post(&addr));
    assert_eq!(code, -32004);
    assert!(msg.contains("a2a.introspection.enabled"), "{msg}");
    // …the stream refuses (as its SSE terminal frame)…
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let body = common::rpc_body(3, common::feed_method(), json!({}));
        let resp_or_stream = common::a2a_post(&addr, &body, &[]).body;
        // Either a plain error body or an SSE stream whose only frame is the error.
        assert!(
            resp_or_stream.contains("-32004") && resp_or_stream.contains("a2a.events.enabled"),
            "{resp_or_stream}"
        );
        drop(frames);
    }
    // …and the core surface still answers (status command untouched).
    let st = read(&addr, "status", json!({}));
    assert!(st["runs"].is_array(), "{st}");
    // The card promises nothing about the interface. Other extensions (the
    // command vocabulary) are still declared — the claim under test is that a
    // surface this instance will NOT serve is never advertised, which is what
    // makes the card a promise.
    let card = get_card(&addr);
    let uris: Vec<&str> = card["capabilities"]["extensions"]
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["uri"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !uris.iter().any(|u| u.contains("interface")),
        "the feed is off, so no interface extension may be advertised: {uris:?}"
    );
    assert!(
        uris.contains(&"https://agentd.dev/a2a/ext/command/v1"),
        "…while what this instance does serve is still declared: {uris:?}"
    );

    std::fs::remove_file(&cfg).ok();
}

/// One raw HTTP exchange: `method path` with `headers` and `body`; the status,
/// the headers (names lowercased) and the body. Written by hand because a
/// browser's requests — a preflight, a POST with `Origin` — are what is under
/// test, and no helper should add or drop a header behind the test's back.
fn exchange(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, Vec<(String, String)>, String) {
    use std::io::Read;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: x\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(req.as_bytes()).unwrap();
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

fn varies_on_origin(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(k, v)| {
        k == "vary"
            && v.split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("origin"))
    })
}

const UI_BEARER: &str = "iface-cors-bearer";

/// [`iface_config`] with `a2a.cors.origins: [<origins>]` and `a2a.bearer`.
fn with_origins_and_bearer(llm: &str, port: u16, origins: &str) -> String {
    with_origins(llm, port, origins).replace(
        "  cors:\n",
        "  bearer: \"{{secret:IFACE_CORS_BEARER}}\"\n  cors:\n",
    )
}

fn spawn_daemon_with_bearer(config: &str) -> Daemon {
    let stderr_path = common::unique_path("iface-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .env("IFACE_CORS_BEARER", UI_BEARER)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd daemon");
    Daemon { child, stderr_path }
}

/// The browser path end to end, under the strict origin policy.
///
/// A listed origin's preflight is granted with every header a call carries
/// (`a2a-version` included) and PNA when asked; any other origin — another
/// loopback port as much as a foreign site, and `Origin: null` — is refused
/// 403 with no grant and no body. A listed origin's POST is granted, with the
/// headers a page must read exposed, whether it succeeds (with the bearer)
/// or is challenged (without one: a browser is never the implicit operator,
/// and the page has to be able to read why). And a body sent as anything but
/// JSON is 415, which is what makes every browser call preflighted.
#[test]
fn cors_and_content_type() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (_daemon, addr, cfg) = spawn_bound_with(
        |port| with_origins_and_bearer(&llm.uri, port, "\"https://ui.example\""),
        spawn_daemon_with_bearer,
    );
    let preflight = |origin: &str, pna: bool| {
        let mut h = vec![
            ("Origin", origin),
            ("Access-Control-Request-Method", "POST"),
            (
                "Access-Control-Request-Headers",
                "content-type, authorization, a2a-version",
            ),
        ];
        if pna {
            h.push(("Access-Control-Request-Private-Network", "true"));
        }
        exchange(&addr, "OPTIONS", "/", &h, "")
    };

    // Preflight from the configured origin → 204 + grant.
    let (code, headers, _) = preflight("https://ui.example", false);
    assert_eq!(code, 204, "{headers:?}");
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://ui.example")
    );
    assert!(varies_on_origin(&headers), "{headers:?}");
    // Every request the TS clients make carries `a2a-version`, and it is not
    // a CORS-safelisted header: a preflight that does not allow it fails
    // every browser call.
    let allowed: Vec<&str> = header(&headers, "access-control-allow-headers")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .collect();
    for h in [
        "content-type",
        "authorization",
        "last-event-id",
        "a2a-extensions",
        "a2a-version",
    ] {
        assert!(
            allowed.contains(&h),
            "the preflight must allow {h}: {headers:?}"
        );
    }
    // Exposure is read from the actual response; on a preflight it is noise.
    assert_eq!(header(&headers, "access-control-expose-headers"), None);
    // PNA is not volunteered…
    assert_eq!(
        header(&headers, "access-control-allow-private-network"),
        None
    );
    // …but a page on a PUBLIC origin reaching a daemon on loopback is the
    // shape Chrome gates, so it is granted when asked for.
    let (code, headers, _) = preflight("https://ui.example", true);
    assert_eq!(code, 204);
    assert_eq!(
        header(&headers, "access-control-allow-private-network"),
        Some("true")
    );

    // Anything unlisted is refused, PNA request or not — loopback included:
    // no local port is trusted for being local.
    for origin in [
        "https://evil.example",
        "http://127.0.0.1:4173",
        "http://localhost:4173",
        "null",
    ] {
        let (code, headers, body) = preflight(origin, true);
        assert_eq!(code, 403, "{origin}: {headers:?}");
        assert_eq!(
            header(&headers, "access-control-allow-origin"),
            None,
            "{origin}"
        );
        assert_eq!(
            header(&headers, "access-control-allow-private-network"),
            None,
            "{origin}"
        );
        assert!(varies_on_origin(&headers), "{origin}: {headers:?}");
        assert!(body.is_empty(), "{origin}: {body:?}");
    }

    let body = common::rpc_body(1, "ListTasks", json!({}));
    let post = |origin: &str, bearer: bool, ct: &str| {
        let auth = format!("Bearer {UI_BEARER}");
        let mut h = vec![
            ("Origin", origin),
            ("Content-Type", ct),
            ("A2A-Version", common::A2A_VERSION),
        ];
        if bearer {
            h.push(("Authorization", auth.as_str()));
        }
        exchange(&addr, "POST", "/", &h, &body)
    };
    let exposed = |headers: &[(String, String)]| -> Vec<String> {
        header(headers, "access-control-expose-headers")
            .unwrap_or("")
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .collect()
    };

    // The listed origin with the bearer: served, and granted.
    let (code, headers, reply) = post("https://ui.example", true, "application/json");
    assert_eq!(code, 200, "{reply}");
    let v: Value = serde_json::from_str(&reply).unwrap();
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://ui.example")
    );
    assert!(varies_on_origin(&headers), "{headers:?}");
    for h in ["a2a-extensions", "retry-after", "www-authenticate"] {
        assert!(exposed(&headers).iter().any(|e| e == h), "{h}: {headers:?}");
    }

    // Without a credential: challenged — and the page can read the challenge.
    let (code, headers, reply) = post("https://ui.example", false, "application/json");
    assert_eq!(code, 401, "{reply}");
    let v: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(v["error"]["code"], -31401, "{v}");
    assert!(
        header(&headers, "www-authenticate").is_some(),
        "{headers:?}"
    );
    assert_eq!(
        header(&headers, "access-control-allow-origin"),
        Some("https://ui.example")
    );
    for h in ["a2a-extensions", "retry-after", "www-authenticate"] {
        assert!(exposed(&headers).iter().any(|e| e == h), "{h}: {headers:?}");
    }

    // Any other origin: the rebind 403, bearer or not — nothing granted,
    // nothing said.
    for origin in ["https://evil.example", "http://127.0.0.1:4173", "null"] {
        let (code, headers, reply) = post(origin, true, "application/json");
        assert_eq!(code, 403, "{origin}: {reply}");
        assert_eq!(
            header(&headers, "access-control-allow-origin"),
            None,
            "{origin}"
        );
        assert_eq!(header(&headers, "access-control-expose-headers"), None);
        assert!(varies_on_origin(&headers), "{origin}: {headers:?}");
        assert!(reply.is_empty(), "{origin}: {reply:?}");
    }

    // The content type: `text/plain` is what a page may POST cross-origin
    // without a preflight, so it is refused — before anything is parsed.
    for ct in ["text/plain", "application/x-www-form-urlencoded"] {
        let (code, _, reply) = post("https://ui.example", true, ct);
        assert_eq!(code, 415, "{ct}: {reply}");
        assert!(reply.is_empty(), "{ct}: {reply:?}");
    }

    std::fs::remove_file(&cfg).ok();
}

/// The card at the spec's one discovery path.
///
/// Read anonymously from a bearer-protected daemon, it is a 200 whose body is
/// an `AgentCard`, with an ETag and `Cache-Control`; asking again with that
/// ETag is a 304. The tag follows the card, not the daemon: a reload that
/// changes what the card says changes it, a reload that only changes the
/// loaded workflows (which the public card does not list) leaves it alone.
/// And the pre-1.0 path is simply not there.
#[test]
#[cfg(feature = "hot-reload")]
fn well_known_card() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let wf_dir = common::unique_path("iface-card-workflows", "d");
    std::fs::create_dir_all(&wf_dir).unwrap();
    let wf = |name: &str| {
        format!(
            "name: {name}\nsteps:\n  s: {{ kind: manual }}\n  \
             f: {{ kind: finish, depends_on: [s], status: completed }}\n"
        )
    };
    std::fs::write(format!("{wf_dir}/a.yaml"), wf("first")).unwrap();
    let config = |port: u16, description: &str| {
        with_origins_and_bearer(&llm.uri, port, "\"https://ui.example\"").replace(
            "  name: iface-e2e\n",
            &format!("  name: iface-e2e\n  description: {description}\n"),
        ) + &format!("workflows:\n  - dir: {{ path: \"{wf_dir}\", glob: \"*.yaml\" }}\n")
    };
    let (daemon, addr, cfg) =
        spawn_bound_with(|port| config(port, "Before."), spawn_daemon_with_bearer);
    let port: u16 = addr.rsplit(':').next().unwrap().parse().unwrap();
    let path = "/.well-known/agent-card.json";
    let get = |extra: &[(&str, &str)]| exchange(&addr, "GET", path, extra, "");

    let (code, headers, body) = get(&[("Accept", "application/json")]);
    assert_eq!(code, 200, "{body}");
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    assert_eq!(
        header(&headers, "cache-control"),
        Some("public, max-age=60")
    );
    let card: Value = serde_json::from_str(&body).unwrap();
    // The spec's required AgentCard fields, as the SDK's type names them.
    assert_eq!(card["name"], "iface-e2e", "{card}");
    assert_eq!(card["description"], "Before.", "{card}");
    for field in [
        "version",
        "capabilities",
        "defaultInputModes",
        "defaultOutputModes",
        "skills",
    ] {
        assert!(card.get(field).is_some(), "{field}: {card}");
    }
    let iface = &card["supportedInterfaces"][0];
    assert_eq!(iface["protocolBinding"], "JSONRPC", "{card}");
    assert_eq!(iface["protocolVersion"], "1.0", "{card}");
    assert!(
        iface["url"].as_str().is_some_and(|u| u.starts_with("http")),
        "{card}"
    );
    let etag = header(&headers, "etag").expect("an ETag").to_string();

    // Same card, same tag; the tag asked back is a 304 with no body.
    let (code, again, _) = get(&[]);
    assert_eq!(code, 200);
    assert_eq!(header(&again, "etag"), Some(etag.as_str()));
    let (code, headers, body) = get(&[("If-None-Match", &etag)]);
    assert_eq!(code, 304, "{body}");
    assert!(body.is_empty(), "{body:?}");
    assert_eq!(header(&headers, "etag"), Some(etag.as_str()));
    // HEAD answers as GET does, without the body.
    let (code, headers, body) = exchange(&addr, "HEAD", path, &[], "");
    assert_eq!(code, 200);
    assert_eq!(header(&headers, "etag"), Some(etag.as_str()));
    assert!(body.is_empty(), "{body:?}");

    let reloads = || {
        std::fs::read_to_string(&daemon.stderr_path)
            .unwrap_or_default()
            .matches("\"event\":\"config.reloaded\"")
            .count()
    };
    let reload = || {
        let before = reloads();
        unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while reloads() == before {
            assert!(
                Instant::now() < deadline,
                "the daemon never reloaded:\n{}",
                std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // A workflow-only reload: the public card does not list workflows, so
    // the card — and its tag — are unchanged.
    std::fs::write(format!("{wf_dir}/b.yaml"), wf("second")).unwrap();
    reload();
    let log = std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default();
    assert!(
        log.lines()
            .any(|l| l.contains("\"event\":\"workflow.loaded\"")
                && l.contains("\"name\":\"second\""))
            && log
                .lines()
                .any(|l| l.contains("\"event\":\"config.reloaded\"")
                    && l.contains("\"changed\":[\"workflows\"]")),
        "the reload was not a workflow-only one that loaded the new workflow, so it proves nothing:\n{log}"
    );
    let (code, headers, _) = get(&[]);
    assert_eq!(code, 200);
    assert_eq!(
        header(&headers, "etag"),
        Some(etag.as_str()),
        "a workflow reload changed the public card"
    );

    // A reload that changes what the card says changes the tag, and the old
    // tag is no longer a 304.
    std::fs::write(&cfg, config(port, "After.")).unwrap();
    reload();
    let (code, headers, body) = get(&[("If-None-Match", &etag)]);
    assert_eq!(code, 200, "the old tag still matched");
    let card: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(card["description"], "After.", "{card}");
    assert_ne!(header(&headers, "etag"), Some(etag.as_str()));

    // The 0.2.x path is gone.
    let (code, _, _) = exchange(&addr, "GET", "/.well-known/agent.json", &[], "");
    assert_eq!(code, 404);

    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&wf_dir).ok();
}

/// The pairing exchange is gone, under every name it answered to.
///
/// `Pair` was an anonymous, undeclared JSON-RPC method that minted operator
/// session tokens; `a2a.device_grant` replaced it. What is pinned here is that
/// no spelling issues a credential — on a no-auth loopback daemon, where the
/// caller is the operator and reaches the dispatcher, and on a bearer-protected
/// one, where the anonymous admission that let a code holder in is gone too.
/// (Which error code a removed method gets is the listener's vocabulary, and
/// is pinned where that is.)
#[test]
fn pair_is_gone() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let names = ["Pair", "interface.pair", "a2a.Pair", "a2a.interface.pair"];

    let (_daemon, addr, cfg) = spawn_bound(|port| iface_config(&llm.uri, port, false, ""));
    for (i, name) in names.iter().enumerate() {
        let reply = common::rpc(&addr, i as i64 + 1, name, json!({"code": "000000"}));
        assert!(
            reply.get("error").is_some() && reply.get("result").is_none(),
            "{name} answered: {reply}"
        );
        assert!(
            !reply.to_string().contains("pat-") && !reply.to_string().contains("token"),
            "{name} issued a credential: {reply}"
        );
    }
    std::fs::remove_file(&cfg).ok();

    let (_daemon, addr, cfg) = spawn_bound_with(
        |port| {
            format!(
                "config_version: \"1\"\n\
         agent:\n  name: pair-gone\n  instruction: Test.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n  bearer: \"{{{{secret:PAIRB}}}}\"\n\
         lifecycle:\n  run_until: drained\n",
                llm.uri
            )
        },
        |cfg| {
            let stderr_path = common::unique_path("pair-daemon", "log");
            let errf = std::fs::File::create(&stderr_path).unwrap();
            let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
                .args(["--config", cfg])
                .env("PAIRB", "server-secret-bearer")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(errf))
                .spawn()
                .expect("spawn daemon");
            Daemon { child, stderr_path }
        },
    );
    for (i, name) in names.iter().enumerate() {
        let reply = common::a2a_post(
            &addr,
            &common::rpc_body(i as i64 + 1, name, json!({"code": "000000"})),
            &[],
        );
        assert_eq!(
            reply.status, 401,
            "an uncredentialed {name} is refused before any dispatch: {}",
            reply.body
        );
    }
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn admin_set_toggles_introspection_live() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    // Introspection starts OFF.
    let (_daemon, addr, cfg) = spawn_bound(|port| iface_config(&llm.uri, port, false, ""));

    let (code, _) = error_of(&SendMessage::command("debug.events", json!({})).post(&addr));
    assert_eq!(code, -32004);

    // `admin.set a2a.introspection.enabled true` flips it at runtime…
    let set = command(
        &addr,
        "admin.set",
        json!({"path": "a2a.introspection.enabled", "value": true}),
    );
    assert_eq!(
        set["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{set}"
    );
    // …and the introspection reads work — including the log ring, installed on
    // toggle.
    let ev = read(&addr, "debug.events", json!({"limit": 10}));
    assert!(ev["events"].is_array(), "{ev}");

    // The removed paths are not runtime-settable; the error names what is.
    for path in [
        "interface.debug",
        "interface.display.bottom",
        "intelligence.model",
    ] {
        let (code, msg) = error_of(
            &SendMessage::command("admin.set", json!({"path": path, "value": "x"})).post(&addr),
        );
        assert_eq!(code, -32602, "{path}");
        assert!(
            msg.contains("not runtime-settable") && msg.contains("a2a.introspection.enabled"),
            "{path}: {msg}"
        );
    }
    // …and the op it replaced is refused by name.
    let (code, msg) = error_of(
        &SendMessage::command(
            "config.set",
            json!({"path": "a2a.introspection.enabled", "value": true}),
        )
        .post(&addr),
    );
    assert_eq!(code, -32602);
    assert!(msg.contains("admin.set"), "{msg}");

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_live_subagent_is_observable_and_drillable() {
    // The root delegates to a sync subagent (mock tool_calls); the interface
    // then shows it: `subagent` feed/section data + the `subagent.get` detail.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {"instruction": "count to three", "mode": "sync"}}]},
            {"content": "delegated and done"}
        ],
        "match": [
            {"when_contains": "You are agentd, an autonomous agent.", "content": "three"}
        ]
    }));
    let (_daemon, addr, cfg) = spawn_bound(|port| iface_config(&llm.uri, port, true, ""));

    // Missing handle → non-disclosing not-found.
    let (code, _) =
        error_of(&SendMessage::command("subagent.get", json!({"handle": "nope"})).post(&addr));
    assert_eq!(code, -32001);

    // Drive the delegating turn (blocking → returns when the tree settles).
    let sent = SendMessage::text("count for me").result(&addr);
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );

    // The status section lists the subagent; drill into it.
    let subs = read(&addr, "status", json!({}))["subagents"].clone();
    let handle = subs[0]["handle"]
        .as_str()
        .expect("a subagent exists")
        .to_string();
    // Drill in and wait for the terminal state — the daemon can answer status
    // queries faster than a subagent completes, so "already completed by the
    // time we ask" is a timing artifact, not a contract.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let got = loop {
        let got = read(&addr, "subagent.get", json!({"handle": handle}));
        if got["subagent"]["status"] == "completed" {
            break got;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "subagent never completed: {got}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let sub = &got["subagent"];
    assert_eq!(sub["status"], "completed", "{got}");
    assert_eq!(sub["mode"], "sync");
    assert!(
        sub["instruction"]
            .as_str()
            .unwrap()
            .contains("count to three"),
        "{got}"
    );
    assert!(
        sub["result"].is_object() || sub["result"].is_string(),
        "{got}"
    );

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn live_activity_reports_phase_tool_and_tokens_on_the_feed() {
    // A turn that calls a tool then answers: the feed must carry `activity`
    // events naming the phase and the TOOL, with tokens accruing — the data
    // behind the clients' working row.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "memory.set", "arguments": {"key": "k", "value": 1}}]},
            {"content": "Stored it."}
        ]
    }));
    let (_daemon, addr, cfg) = spawn_bound(|port| iface_config(&llm.uri, port, false, ""));

    // Attach FIRST so the activity events stream as they happen.
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let addr2 = addr.clone();
    std::thread::spawn(move || subscribe_events(&addr2, 0, sink));
    wait_for(&frames, 5, |f| f.iter().any(|v| v.get("hello").is_some()));

    let sent = SendMessage::text("Remember k=1").result(&addr);
    assert_eq!(
        sent["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{sent}"
    );

    let activity = |f: &[Value]| -> Vec<Value> {
        f.iter()
            .filter(|v| v["event"]["kind"] == "activity")
            .map(|v| v["event"]["data"].clone())
            .collect()
    };
    // The tool phase names the tool the model actually called.
    wait_for(&frames, 10, |f| {
        activity(f)
            .iter()
            .any(|a| a["phase"] == "tool" && a["tool"] == "memory.set")
    });
    // Thinking is reported too — and the unit answering the A2A message binds
    // to its task (a workflow step turn reports alongside it, unbound).
    wait_for(&frames, 10, |f| {
        activity(f).iter().any(|a| a["phase"] == "thinking")
    });
    let task_id = sent["task"]["id"].as_str().unwrap_or_default().to_string();
    wait_for(&frames, 10, |f| {
        activity(f).iter().any(|a| a["task"] == task_id.as_str())
    });
    // …and the unit's record disappears when the turn ends.
    wait_for(&frames, 10, |f| {
        f.iter().any(|v| v["event"]["kind"] == "activity.removed")
    });

    let got = frames.lock().unwrap();
    let acts = activity(&got);
    assert!(
        acts.iter()
            .all(|a| a["started_ms"].as_u64().is_some_and(|t| t > 0)),
        "clients tick elapsed from started_ms: {acts:#?}"
    );
    let bound = acts
        .iter()
        .find(|a| a["task"] == task_id.as_str())
        .expect("the A2A turn's activity binds to its task");
    // The conversation is the one the task names. A send that names none gets
    // the id a2a-rs mints for it (a UUIDv4), so the check is against
    // the task rather than a spelling of agentd's own.
    assert!(
        bound["ctx"].as_str().is_some_and(|c| !c.is_empty())
            && bound["ctx"] == sent["task"]["contextId"],
        "…and to its conversation: {bound} / {sent}"
    );
    // Tokens accrue on the record (the mock reports usage per round).
    assert!(
        acts.iter().any(|a| a["tokens_in"].as_u64().unwrap_or(0)
            + a["tokens_out"].as_u64().unwrap_or(0)
            > 0),
        "activity carries the turn's spend: {acts:#?}"
    );
    // Deliberately COARSE: a handful of events, not a stream (the replay ring
    // must stay meaningful — this is the property token streaming would break).
    assert!(
        acts.len() <= 24,
        "activity is change-triggered, not a token stream: {} events",
        acts.len()
    );
    drop(got);
    std::fs::remove_file(&cfg).ok();
}

/// `security.workflows.immutable`: the agent may RUN its workflows, never
/// rewrite them.
///
/// A workflow is a STANDING instruction — what happens when a schedule fires or
/// a webhook lands, unattended. An agent that can rewrite one changes what
/// happens next time, and the change outlives the conversation that caused it.
/// Where definitions are reviewed before they ship — a file in git, a config a
/// deploy applies — self-update is a hole in that review.
///
/// Driven through the MODEL, because that is the only path that can reach these
/// tools: they are not exposed over A2A, and a workflow `tool` step is refused
/// the grant at load.
#[test]
fn an_immutable_daemon_refuses_the_model_rewriting_its_workflows() {
    let llm = spawn_mock_llm(&json!({"turns": [
        {"tool_calls": [{"name": "workflow.create", "arguments": {"definition": {
            "name": "sneaky",
            "steps": {"go": {"kind": "manual"},
                      "fin": {"kind": "finish", "depends_on": ["go"], "status": "completed"}}
        }}}]},
        {"content": "could not change it"}
    ]}));
    let (daemon, addr, _cfg) = spawn_bound(|port| {
        iface_config(
            &llm.uri,
            port,
            true,
            "security:\n  workflows:\n    immutable: true\n",
        )
    });

    let _ = SendMessage::text("add a workflow called sneaky").post_raw(&addr);

    // The refusal is AUDITED, not merely returned to the model — an operator
    // reading the log should see that the agent tried, which is the point of
    // logging a refusal at all.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut log = String::new();
    while Instant::now() < deadline {
        log = std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default();
        if log.contains("workflow.locked") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        log.contains("\"event\":\"workflow.locked\""),
        "the model's workflow.create was not refused (no audit line):\n{log}"
    );
    // And nothing was DEFINED: a refusal that still writes is not a refusal.
    // Checked on the definition event, not on the name — the name appears in
    // the log either way, because the attempt itself is recorded.
    assert!(
        !log.contains("\"event\":\"workflow.defined\""),
        "a locked daemon defined the workflow anyway:\n{log}"
    );
}

/// A reload REVISES the CORS allowlist — the third instance of the same defect.
///
/// The origin list (then `interface.origins`, now `a2a.cors.origins`) was captured into the listener's app state at spawn and
/// never re-read, and it was not restart-only either: an operator who removed
/// an origin to revoke a web client's access got `config.reloaded` success and
/// a listener that kept granting the old origin. Found while classifying the
/// config surface after fixing the same shape in `a2a.principals` and the
/// webhook routes.
///
/// Revocation is the direction under test, for the same reason as the other
/// two: a grant that fails to apply is an inconvenience, a revocation that
/// fails to apply is a security hole that looks closed.
#[test]
#[cfg(feature = "hot-reload")]
fn a_reload_revokes_a_web_origin_and_the_grant_stops() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (daemon, addr, cfg) =
        spawn_bound(|port| with_origins(&llm.uri, port, "\"https://ui.example\""));

    let preflight = |origin: &str| -> (u16, bool) {
        let (code, headers, _) = exchange(
            &addr,
            "OPTIONS",
            "/",
            &[
                ("Origin", origin),
                ("Access-Control-Request-Method", "POST"),
            ],
            "",
        );
        (
            code,
            header(&headers, "access-control-allow-origin") == Some(origin),
        )
    };

    let (code, granted) = preflight("https://ui.example");
    assert_eq!(code, 204);
    assert!(
        granted,
        "the configured origin is granted before the reload"
    );

    // Revoke it: the allowlist now names a different origin entirely.
    let port: u16 = addr.rsplit(':').next().unwrap().parse().unwrap();
    std::fs::write(
        &cfg,
        with_origins(&llm.uri, port, "\"https://other.example\""),
    )
    .unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    let deadline = Instant::now() + Duration::from_secs(10);
    let stderr = || std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default();
    while !stderr().contains("\"event\":\"config.reloaded\"") {
        assert!(
            Instant::now() < deadline,
            "the daemon never reloaded:\n{}",
            stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The revocation is live — the next preflight is refused outright. Before
    // the fix this still answered with the grant, because the listener held
    // the list it was spawned with.
    let (code, granted) = preflight("https://ui.example");
    assert_eq!(code, 403, "the revoked origin is refused");
    assert!(!granted, "the revoked origin is no longer granted");
    // And the newly named one is.
    let (_, granted) = preflight("https://other.example");
    assert!(granted, "the newly allowed origin is granted");

    std::fs::remove_file(&cfg).ok();
}

/// A reload that turns introspection on arms the log ring the reads tail, as
/// `admin.set` does, and tells the feed which paths it moved. Without it the flag flips but `debug.events` answers
/// that the ring is not installed until the next restart — a reload that
/// reports success and changes nothing an operator can use.
#[test]
#[cfg(feature = "hot-reload")]
fn a_reload_that_turns_introspection_on_arms_the_ring() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (daemon, addr, cfg) = spawn_bound(|port| iface_config(&llm.uri, port, false, ""));
    let (code, _) = error_of(&SendMessage::command("debug.events", json!({})).post(&addr));
    assert_eq!(code, -32004, "introspection starts off");

    // A client watching the feed is told what the reload moved.
    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let addr2 = addr.clone();
    std::thread::spawn(move || subscribe_events(&addr2, 0, sink));
    wait_for(&frames, 5, |f| f.iter().any(|v| v.get("hello").is_some()));

    let port: u16 = addr.rsplit(':').next().unwrap().parse().unwrap();
    std::fs::write(&cfg, iface_config(&llm.uri, port, true, "")).unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGHUP) };
    let deadline = Instant::now() + Duration::from_secs(10);
    let stderr = || std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default();
    while !stderr().contains("\"event\":\"config.reloaded\"") {
        assert!(
            Instant::now() < deadline,
            "the daemon never reloaded:\n{}",
            stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let ev = read(&addr, "debug.events", json!({"limit": 10}));
    assert!(ev["events"].is_array(), "{ev}");
    wait_for(&frames, 5, |f| {
        f.iter().any(|v| {
            v["event"]["kind"] == "config"
                && v["event"]["data"]["source"] == "reload"
                && v["event"]["data"]["paths"]
                    .as_array()
                    .is_some_and(|p| p.iter().any(|x| x == "a2a.introspection.enabled"))
        })
    });

    std::fs::remove_file(&cfg).ok();
}
