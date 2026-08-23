// SPDX-License-Identifier: AGPL-3.0-only
//! agentd durable state (RFC 0025) over the REAL wire: the `Durable` façade
//! → `McpStore` (default checkpointer profile) → `McpClient` (Streamable HTTP)
//! → the built-in mock MCP server (`--internal-mock-mcp-http`) as a subprocess.
//! Proves: entities/inbox/timers/manifest round-trip through `state.put/get/
//! list/delete`, the seq CAS is enforced end to end, a "crashed" instance
//! restores everything (pending inbox replays, done events do not, unindexed
//! records are found by `list`), and transient store faults are retried
//! (`mock.fault`) while persistent ones halt (or degrade, by policy).

mod common;

use agentd::config::v2 as cfg;
use agentd::mcp::client::McpClient;
use agentd::state::{Durable, InboxEvent, Kind, Policy, TimerRecord};
use agentd::store::{self, SharedStore, StoreError};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn connect(mock: &common::MockMcp) -> Arc<McpClient> {
    let mut c =
        McpClient::connect("ckpt", &mock.uri(), vec![], Duration::from_secs(5)).expect("connect");
    c.initialize().expect("initialize");
    Arc::new(c)
}

fn open_store(client: Arc<McpClient>) -> SharedStore {
    let settings: cfg::Store = serde_json::from_value(json!({
        "kind": "mcp",
        "mcp": {"server": "ckpt"},
        "timeout": "5s",
    }))
    .unwrap();
    let advertised: Vec<String> = client
        .list_tools()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        advertised.iter().any(|t| t == "state.delete"),
        "mock advertises the full profile: {advertised:?}"
    );
    let c2 = client.clone();
    let s = store::open(&settings, &move |name: &str| {
        (name == "ckpt").then(|| c2.clone() as Arc<dyn store::mcp::McpCall>)
    })
    .expect("open")
    .expect("a store");
    assert_eq!(s.kind(), "mcp");
    s
}

fn control(client: &McpClient, tool: &str, args: Value) -> Value {
    let res = client
        .call_tool_with_meta_within(tool, Some(args), json!({}), Duration::from_secs(5))
        .expect("control call");
    serde_json::from_str(&res.text()).unwrap_or(Value::Null)
}

fn durable(store: SharedStore, on_error: cfg::StoreOnError) -> Durable {
    Durable::new(
        store,
        "agentd",
        "e2e-inst",
        Policy {
            debounce: Duration::from_millis(0),
            on_error,
            retries: 3,
        },
        None,
    )
}

#[test]
fn durable_state_round_trips_and_restores_over_http_mcp() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let client = connect(&mock);

    // ---- life 1: a fresh instance writes state, then "crashes" (drop) ----
    let (e_pending_id, e_done_id) = {
        let d = durable(open_store(client.clone()), cfg::StoreOnError::Halt);
        let r = d.restore().expect("fresh restore");
        assert!(r.manifest.is_none(), "fresh instance");
        assert_eq!(d.manifest().generation, 1);

        assert_eq!(
            d.put(
                Kind::Run,
                "r1",
                json!({"status": "running", "step": 1}),
                Some("h1".into())
            )
            .unwrap(),
            1
        );
        assert_eq!(
            d.put(
                Kind::Run,
                "r1",
                json!({"status": "running", "step": 2}),
                Some("h1".into())
            )
            .unwrap(),
            2
        );
        assert_eq!(
            d.put(Kind::Context, "root", json!({"messages": ["hi"]}), None)
                .unwrap(),
            1
        );
        assert_eq!(
            d.put(Kind::Task, "task-1", json!({"state": "working"}), None)
                .unwrap(),
            1
        );
        let pending = InboxEvent::new(
            "a2a_message",
            Some("user:alice".into()),
            json!({"text": "hello"}),
        );
        let done = InboxEvent::new(
            "start_fired",
            None,
            json!({"workflow": "w", "node": "tick"}),
        );
        d.inbox_put(&pending).unwrap();
        d.inbox_put(&done).unwrap();
        d.inbox_done(&done.id).unwrap();
        d.timer_arm(&TimerRecord {
            id: "t-1".into(),
            deadline_ms: 4_102_444_800_000,
            owner: json!({"run": "r1", "node": "sleep"}),
            payload: json!({"reason": "sleep"}),
        })
        .unwrap();
        d.manifest_update(|m| {
            m.starts
                .insert("w.tick".into(), json!({"last_fired": 17, "iteration": 3}));
            m.budget = json!({"tokens": {"day": 1234}});
        });
        assert!(d.flush(true).unwrap());
        // Written after the last flush → not indexed; `list` must find it.
        assert_eq!(
            d.put(Kind::Artifact, "art-1", json!({"mime": "text/plain"}), None)
                .unwrap(),
            1
        );
        // A read-back through the wire.
        let env = d.get(Kind::Run, "r1").unwrap().unwrap();
        assert_eq!(env.seq, 2);
        assert_eq!(env.state["step"], json!(2));
        assert_eq!(env.instance, "e2e-inst");
        assert_eq!(env.hash.as_deref(), Some("h1"));
        (pending.id, done.id)
    };

    // The wire carried the profile's tools (the mock logs every state.* call).
    let ops = control(&client, "mock.ops", json!({}));
    let ops: Vec<String> = ops["ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(ops.iter().any(|o| o == "state.put"));
    assert!(ops.iter().any(|o| o == "state.get"));
    assert!(
        ops.iter().any(|o| o == "state.delete"),
        "inbox_done deletes over the wire: {ops:?}"
    );

    // ---- life 2: restore from the store alone ----
    let d2 = durable(open_store(client.clone()), cfg::StoreOnError::Halt);
    let r = d2.restore().expect("restore");
    let m = r.manifest.as_ref().expect("manifest");
    assert_eq!(m.generation, 2);
    assert_eq!(m.starts["w.tick"]["iteration"], json!(3));
    assert_eq!(m.budget["tokens"]["day"], json!(1234));
    assert!(r.lost.is_empty(), "nothing lost: {:?}", r.lost);
    let pending = r.inbox_pending();
    assert_eq!(pending.len(), 1, "only the un-acked event replays");
    assert_eq!(pending[0].id, e_pending_id);
    assert_eq!(pending[0].principal.as_deref(), Some("user:alice"));
    assert!(
        !r.of(Kind::Inbox).iter().any(|e| e.id == e_done_id),
        "the done event is gone"
    );
    assert_eq!(r.timers().len(), 1);
    assert_eq!(r.timers()[0].owner["node"], json!("sleep"));
    assert_eq!(r.of(Kind::Run).len(), 1);
    assert_eq!(r.of(Kind::Run)[0].seq, 2);
    assert_eq!(r.of(Kind::Context).len(), 1);
    assert_eq!(r.of(Kind::Task).len(), 1);
    assert!(
        r.unindexed
            .iter()
            .any(|u| u.kind == "artifact" && u.id == "art-1"),
        "list found the unflushed artifact: {:?}",
        r.unindexed
    );
    assert_eq!(r.of(Kind::Artifact).len(), 1);
    // The seq map is warm: the sequence continues, no conflict.
    assert_eq!(
        d2.put(
            Kind::Run,
            "r1",
            json!({"status": "done"}),
            Some("h1".into())
        )
        .unwrap(),
        3
    );
    assert_eq!(
        d2.put(
            Kind::Artifact,
            "art-1",
            json!({"mime": "text/plain", "size": 3}),
            None
        )
        .unwrap(),
        2
    );
    // Ack the replayed event; a third life sees an empty inbox.
    d2.inbox_done(&e_pending_id).unwrap();
    d2.flush(true).unwrap();

    // ---- a stale writer (a second instance with an old seq) is refused ----
    let d_stale = durable(open_store(client.clone()), cfg::StoreOnError::Halt);
    // Never restored, never touched `r1`: its first put adopts the live seq
    // (3) and writes 4 — a restore gap, not split-brain…
    assert_eq!(
        d_stale
            .put(Kind::Run, "r1", json!({"status": "stale?"}), None)
            .unwrap(),
        4
    );
    // …but now d2 (which believes it owns seq 3) conflicts: split-brain is fatal.
    assert!(matches!(
        d2.put(Kind::Run, "r1", json!({"status": "x"}), None),
        Err(StoreError::Conflict(_))
    ));

    // ---- life 3: the inbox is drained; the generation keeps counting ----
    let d3 = durable(open_store(client.clone()), cfg::StoreOnError::Halt);
    let r3 = d3.restore().unwrap();
    assert_eq!(r3.manifest.as_ref().unwrap().generation, 3);
    assert!(r3.inbox_pending().is_empty());
}

#[test]
fn transient_store_faults_are_retried_and_persistent_ones_follow_policy() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let client = connect(&mock);

    // Halt policy: 2 injected faults < 3 retries → the put succeeds.
    let d = durable(open_store(client.clone()), cfg::StoreOnError::Halt);
    d.restore().unwrap();
    control(&client, "mock.fault", json!({"count": 2}));
    assert_eq!(d.put(Kind::Run, "r", json!({"n": 1}), None).unwrap(), 1);
    assert!(!d.is_degraded());
    // 10 faults exhaust the retries → Io surfaces (halt).
    control(&client, "mock.fault", json!({"count": 10}));
    assert!(matches!(
        d.put(Kind::Run, "r", json!({"n": 2}), None),
        Err(StoreError::Io(_))
    ));
    control(&client, "mock.fault", json!({"count": 0}));

    // Degrade policy: the write is skipped, the instance flags it and goes on
    // (the intended seq is reserved so the next write does not reuse it).
    let dd = durable(open_store(client.clone()), cfg::StoreOnError::Degrade);
    dd.restore().unwrap();
    control(&client, "mock.fault", json!({"count": 10}));
    assert_eq!(dd.put(Kind::Run, "q", json!({"n": 1}), None).unwrap(), 1);
    assert!(dd.is_degraded());
    control(&client, "mock.fault", json!({"count": 0}));
    assert_eq!(dd.put(Kind::Run, "q", json!({"n": 2}), None).unwrap(), 2);
    assert!(!dd.is_degraded(), "a successful write clears the flag");
    // What landed is seq 2 only (seq 1 was lost while degraded).
    let env = dd.get(Kind::Run, "q").unwrap().unwrap();
    assert_eq!(env.seq, 2);
    assert_eq!(env.state["n"], json!(2));
}

/// The write-ahead contract under a real crash: a process dies (SIGKILL, via the
/// `AGENTD_TEST_KILL_AT` kill point) right after an inbox event lands and before
/// it is acknowledged; the next life replays it exactly once. The test re-executes
/// its own binary as the "agent" — the kill point fires inside the child.
#[test]
fn a_kill_between_inbox_put_and_ack_replays_the_event_after_restart() {
    const KILL_AT: &str = "inbox.after_put";
    if std::env::var("AGENTD_TEST_KILL_AT").as_deref() == Ok(KILL_AT) {
        // ---- the child: life 1 — accept an event, die before acking it ----
        let uri = std::env::var("AGENTD_TEST_MOCK_URI").expect("mock uri");
        let mut c = McpClient::connect("ckpt", &uri, vec![], Duration::from_secs(5)).unwrap();
        c.initialize().unwrap();
        let d = durable(open_store(Arc::new(c)), cfg::StoreOnError::Halt);
        d.restore().unwrap();
        let ev = InboxEvent::new(
            "a2a_message",
            Some("user:bob".into()),
            json!({"text": "do the thing"}),
        );
        d.inbox_put(&ev).unwrap(); // ← SIGKILL fires inside (kill point after the put)
        d.inbox_done(&ev.id).unwrap(); // never reached
        panic!("the kill point did not fire");
    }
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let exe = std::env::current_exe().unwrap();
    let status = std::process::Command::new(exe)
        .args([
            "--exact",
            "a_kill_between_inbox_put_and_ack_replays_the_event_after_restart",
            "--nocapture",
        ])
        .env("AGENTD_TEST_KILL_AT", KILL_AT)
        .env("AGENTD_TEST_MOCK_URI", mock.uri())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn child life");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "the child must die at the kill point: {status:?}"
        );
    }
    // ---- life 2: the event is durable and pending; it replays once ----
    let client = connect(&mock);
    let d2 = durable(open_store(client.clone()), cfg::StoreOnError::Halt);
    let r = d2.restore().unwrap();
    assert_eq!(r.manifest.as_ref().unwrap().generation, 2);
    let pending = r.inbox_pending();
    assert_eq!(
        pending.len(),
        1,
        "accepted ⇒ durable, even though the process died before acking"
    );
    assert_eq!(pending[0].payload["text"], json!("do the thing"));
    assert_eq!(pending[0].principal.as_deref(), Some("user:bob"));
    d2.inbox_done(&pending[0].id).unwrap();
    // ---- life 3: acknowledged ⇒ it does not replay again ----
    let d3 = durable(open_store(client), cfg::StoreOnError::Halt);
    assert!(d3.restore().unwrap().inbox_pending().is_empty());
}
