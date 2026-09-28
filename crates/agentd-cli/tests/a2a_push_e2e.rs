// SPDX-License-Identifier: AGPL-3.0-only
//! **Push notifications end to end**: a caller registers a webhook, and agentd
//! POSTs the task's updates to it.
//!
//! The interesting assertions are not that a delivery arrives — they are the
//! refusals. The URL comes from a peer, so the feature is off unless an operator
//! turned it on, and a target that points somewhere agentd should not reach is
//! rejected while the caller is still there to be told why.
#![cfg(feature = "a2a")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;

use common::{SendMessage, get_card, rpc};

use std::process::{Child, Command, Stdio};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One delivery, as the receiver saw it: the headers, then the body.
type Delivery = (Vec<(String, String)>, Value);

/// A webhook receiver: records every delivery, headers included.
struct Hook {
    url: String,
    seen: Arc<Mutex<Vec<Delivery>>>,
}

fn spawn_hook() -> Hook {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Delivery>>> = Arc::default();
    let out = Arc::clone(&seen);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let out = Arc::clone(&out);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let mut start = String::new();
                if r.read_line(&mut start).unwrap_or(0) == 0 {
                    return;
                }
                let mut headers = Vec::new();
                let mut len = 0usize;
                loop {
                    let mut l = String::new();
                    if r.read_line(&mut l).unwrap_or(0) == 0 {
                        return;
                    }
                    if l.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = l.split_once(':') {
                        let k = k.trim().to_ascii_lowercase();
                        if k == "content-length" {
                            len = v.trim().parse().unwrap_or(0);
                        }
                        headers.push((k, v.trim().to_string()));
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).ok();
                let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                out.lock().unwrap().push((headers, v));
                let _ = w.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            });
        }
    });
    Hook { url, seen }
}

fn config(llm: &str, port: u16, push: &str) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: push-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{push}\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
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
    let pb = common::unique_path("push-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("push-mock-llm", "addr");
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

/// Spawn on a probed free port and return the authority it actually bound; the
/// probe→bind gap is a real race under parallel CI, so a lost bind is retried.
fn boot(cfg_for: impl Fn(u16) -> String) -> (Daemon, String, String) {
    for _ in 0..5 {
        let path = common::unique_path("agentd-push", "yaml");
        std::fs::write(&path, cfg_for(free_port())).unwrap();
        let stderr_path = common::unique_path("push-daemon", "log");
        let errf = std::fs::File::create(&stderr_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf))
            .spawn()
            .expect("spawn agentd");
        let daemon = Daemon { child, stderr_path };
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(15)) {
            return (daemon, addr, path);
        }
        std::fs::remove_file(&path).ok();
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

fn wait_for<T>(mut f: impl FnMut() -> Option<T>, secs: u64, what: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The header a delivery carried, by (lower-case) name.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn a_registered_webhook_receives_the_task_and_the_callers_credentials() {
    // A deliberately slow turn, so the webhooks are registered while the task
    // is still working and the transition to `completed` is a real delivery
    // rather than a race with one.
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "done at last", "delay_ms": 2500}]}));
    let hook = spawn_hook();
    // `allow_private` because the receiver in this test is loopback, which is
    // exactly the decision the flag exists to make explicit.
    let (_daemon, addr, cfg_path) = boot(|p| {
        config(
            &llm.uri,
            p,
            "  push:\n    enabled: true\n    allow_private: true\n",
        )
    });

    // A natural-language send that returns as soon as the task exists, so the
    // work is still in flight when the webhook is attached.
    let sent = SendMessage::text("take your time")
        .return_immediately()
        .post(&addr);
    let task_id = sent["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a task: {sent}"))
        .to_string();

    // The spec's request *is* the config: a flat `TaskPushNotificationConfig`,
    // authentication in the 1.0 shape — one scheme, its credentials.
    let register = |id: i64, auth: Value| {
        rpc(
            &addr,
            id,
            "CreateTaskPushNotificationConfig",
            json!({"taskId": task_id, "url": hook.url, "token": "caller-token", "authentication": auth}),
        )
    };
    let bearer = register(2, json!({"scheme": "Bearer", "credentials": "x"}));
    assert!(
        bearer.get("error").is_none(),
        "registration should succeed: {bearer}"
    );
    let config_id = bearer["result"]["id"]
        .as_str()
        .expect("a config id")
        .to_string();
    // The read-back names the scheme it accepted and never the credentials.
    assert_eq!(
        bearer["result"]["authentication"],
        json!({"scheme": "Bearer"}),
        "{bearer}"
    );
    // Any token scheme is honoured, not only Bearer.
    let basic = register(3, json!({"scheme": "Basic", "credentials": "dTpw"}));
    assert!(basic.get("error").is_none(), "{basic}");

    // Authentication that cannot be sent as given is refused, never dropped:
    // dropping it would send the webhook out unauthenticated.
    for (n, auth) in [
        json!({"credentials": "no-scheme"}),
        json!({"scheme": "Bearer x", "credentials": "k"}),
        json!({"scheme": "Bearer", "credentials": "k\r\nx-evil: 1"}),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = register(10 + n as i64, auth.clone());
        assert_eq!(refused["error"]["code"], -32602, "{auth}: {refused}");
    }

    // The turn finishes on its own; that transition is what gets delivered —
    // once to each registered config.
    let deliveries = wait_for(
        || {
            let seen = hook.seen.lock().unwrap();
            let auths: Vec<String> = seen
                .iter()
                .filter_map(|(h, _)| header(h, "authorization").map(str::to_string))
                .collect();
            (auths.iter().any(|a| a == "Bearer x") && auths.iter().any(|a| a == "Basic dTpw"))
                .then(|| seen.clone())
        },
        20,
        "a delivery to each config",
    );
    for (headers, body) in &deliveries {
        // The body is a `StreamResponse` carrying the task — the union a
        // streaming caller reads — and the task is the one registered for.
        assert_eq!(body["task"]["id"], task_id.as_str(), "{body}");
        assert!(body["task"]["status"]["state"].as_str().is_some(), "{body}");
        assert_eq!(
            header(headers, "content-type"),
            Some("application/a2a+json"),
            "{headers:?}"
        );
        // The caller's token comes back too (the legacy courtesy header), so
        // the receiver can tell a real delivery from a stray POST.
        assert_eq!(
            header(headers, "x-a2a-notification-token"),
            Some("caller-token"),
            "{headers:?}"
        );
    }

    // Get and Delete name one config, always. Without an id they are refused —
    // not read as "any" or, on a delete, as "all of them".
    let get = |id: i64, params: Value| rpc(&addr, id, "GetTaskPushNotificationConfig", params);
    let delete =
        |id: i64, params: Value| rpc(&addr, id, "DeleteTaskPushNotificationConfig", params);
    let list = |id: i64| {
        rpc(
            &addr,
            id,
            "ListTaskPushNotificationConfigs",
            json!({"taskId": task_id}),
        )["result"]["configs"]
            .as_array()
            .map_or(0, Vec::len)
    };
    assert_eq!(list(20), 2);
    assert_eq!(get(21, json!({"taskId": task_id}))["error"]["code"], -32602);
    assert_eq!(
        delete(22, json!({"taskId": task_id}))["error"]["code"],
        -32602
    );
    assert_eq!(list(23), 2, "an id-less delete removed nothing");
    // An id the task does not have is "not found", on both.
    let unknown = json!({"taskId": task_id, "id": "no-such-config"});
    assert_eq!(get(24, unknown.clone())["error"]["code"], -32001);
    assert_eq!(delete(25, unknown)["error"]["code"], -32001);

    let named = json!({"taskId": task_id, "id": config_id});
    let got = get(26, named.clone());
    assert_eq!(got["result"]["id"], config_id.as_str(), "{got}");
    assert!(!got.to_string().contains("\"x\""), "no credentials: {got}");
    let deleted = delete(27, named);
    assert_eq!(deleted["result"], json!({}), "{deleted}");
    assert_eq!(list(28), 1, "exactly the named config went");

    std::fs::remove_file(&cfg_path).ok();
}

/// `ListTaskPushNotificationConfigs` pages on the wire, not only inside the
/// runtime: a2a-rs 0.10 drops the request's `pageSize` and `pageToken`, so a
/// listing passed down to it would answer everything to a caller that asked
/// for two, accept a page size of 101, and never say whether more remained.
#[test]
fn push_configs_page_on_the_wire() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "ok"}]}));
    let (_daemon, addr, cfg_path) = boot(|p| {
        config(
            &llm.uri,
            p,
            "  push:\n    enabled: true\n    allow_private: true\n",
        )
    });
    let sent = SendMessage::text("hello").return_immediately().post(&addr);
    let task_id = sent["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a task: {sent}"))
        .to_string();
    let mut registered = Vec::new();
    for n in 0..5 {
        let r = rpc(
            &addr,
            100 + n,
            "CreateTaskPushNotificationConfig",
            json!({"taskId": task_id, "id": format!("pc-{n}"), "url": "http://127.0.0.1:9/hook"}),
        );
        assert!(r.get("error").is_none(), "{r}");
        registered.push(format!("pc-{n}"));
    }
    let list = |id: i64, extra: Value| {
        let mut params = json!({"taskId": task_id});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        rpc(&addr, id, "ListTaskPushNotificationConfigs", params)
    };

    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    let mut token = String::new();
    for page in 0.. {
        assert!(page < 5, "the listing never ended: {seen:?}");
        let r = list(200 + page, json!({"pageSize": 2, "pageToken": token}));
        let result = &r["result"];
        let configs = result["configs"]
            .as_array()
            .unwrap_or_else(|| panic!("configs: {r}"));
        sizes.push(configs.len());
        seen.extend(
            configs
                .iter()
                .map(|c| c["id"].as_str().unwrap().to_string()),
        );
        token = result["nextPageToken"]
            .as_str()
            .unwrap_or_else(|| panic!("nextPageToken is always present: {r}"))
            .to_string();
        if token.is_empty() {
            break;
        }
    }
    assert_eq!(sizes, [2, 2, 1], "pages of the size asked for");
    seen.sort();
    assert_eq!(seen, registered, "every config once, no duplicates");

    let all = list(300, json!({}));
    assert_eq!(
        all["result"]["configs"].as_array().map(Vec::len),
        Some(5),
        "{all}"
    );
    assert_eq!(all["result"]["nextPageToken"], "", "{all}");
    for (n, bad) in [
        json!({"pageSize": 101}),
        json!({"pageSize": -1}),
        json!({"pageToken": "not-a-token"}),
    ]
    .into_iter()
    .enumerate()
    {
        let r = list(310 + n as i64, bad.clone());
        assert_eq!(r["error"]["code"], -32602, "{bad}: {r}");
    }

    std::fs::remove_file(&cfg_path).ok();
}

#[test]
fn a_target_agentd_should_not_reach_is_refused_at_registration() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    // Enabled, but WITHOUT allow_private: the ordinary production posture.
    let (_daemon, addr, cfg_path) = boot(|p| config(&llm.uri, p, "  push:\n    enabled: true\n"));

    // Any task will do; a natural-language send is the one that makes one
    // (`status` is a read, and a read creates none).
    let sent = SendMessage::text("hello").post(&addr);
    let task_id = sent["result"]["task"]["id"].as_str().unwrap().to_string();

    // The cloud metadata endpoint: the canonical thing a peer would like agentd
    // to fetch on its behalf.
    let refused = rpc(
        &addr,
        2,
        "CreateTaskPushNotificationConfig",
        json!({"taskId": task_id, "url": "http://169.254.169.254/latest/meta-data/"}),
    );
    assert_eq!(
        refused["error"]["code"], -32602,
        "a link-local target must be refused with a reason: {refused}"
    );

    std::fs::remove_file(&cfg_path).ok();
}

#[test]
fn push_is_off_unless_an_operator_turns_it_on() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (_daemon, addr, cfg_path) = boot(|p| config(&llm.uri, p, ""));

    // The card is a promise: with the feature off it must not claim the
    // capability.
    let card = get_card(&addr);
    assert_eq!(card["capabilities"]["pushNotifications"], false, "{card}");

    // Any task will do; a natural-language send is the one that makes one
    // (`status` is a read, and a read creates none).
    let sent = SendMessage::text("hello").post(&addr);
    let task_id = sent["result"]["task"]["id"].as_str().unwrap().to_string();

    // …and asking anyway is a clean refusal, not a silent no-op.
    let refused = rpc(
        &addr,
        3,
        "CreateTaskPushNotificationConfig",
        json!({"taskId": task_id, "url": "https://hooks.example/x"}),
    );
    assert!(
        refused["error"]["code"].as_i64().is_some(),
        "a disclaimed capability must refuse: {refused}"
    );

    std::fs::remove_file(&cfg_path).ok();
}
