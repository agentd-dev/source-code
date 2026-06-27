//! The work-claim / lease convention (RFC 0019 §3; the frozen `work.*` contract
//! RFC 0015 §5.6). Two levels:
//!
//!   * **Atomic single-grant (protocol-level)** — drive the mock `workmcp`
//!     coordination server directly over stdio JSON-RPC: a first `work.claim` of
//!     an item is granted; a second claim of the SAME item (before release) is
//!     refused; after `work.release`, a re-claim is granted again. This proves
//!     the single serializing point the whole convention rests on (RFC 0019 §3.1
//!     / §8 row 1).
//!
//!   * **End-to-end via agentd** — run a `cluster`-featured agentd reactive
//!     instance with `--claim <item>=work` against `workmcp` as the coordination
//!     server and `confmcp` as the source that emits `updated{item}`. Assert the
//!     instance claims THEN acks the item exactly once, observed via workmcp's
//!     persisted claim/ack counters (`claims_granted == 1`, `acks == 1`).
//!
//! A true two-instance race (two daemons, one workmcp, assert exactly one acks)
//! is the gold standard but is NOT attempted here — see the module note on
//! `e2e_claim_then_ack_once`. The single-instance claim→ack plus the atomic-grant
//! property is sufficient for v1.

use crate::{Category, Check, Harness, Outcome};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "work-claim/atomic-single-grant",
            category: Category::WorkClaim,
            desc: "work.claim grants once, refuses a held item, re-grants after release",
            run: atomic_single_grant,
        },
        Check {
            id: "work-claim/ack-consumes-lease",
            category: Category::WorkClaim,
            desc: "work.ack marks the item done and a stale-lease ack is a no-op",
            run: ack_consumes_lease,
        },
        Check {
            id: "work-claim/e2e-claim-then-ack-once",
            category: Category::WorkClaim,
            desc: "a cluster agentd reactive claims then acks a routed item exactly once",
            run: e2e_claim_then_ack_once,
        },
    ]
}

// ───────────────────────────── protocol-level ─────────────────────────────

/// Atomic single-grant (RFC 0019 §3.1 / §8 row 1): the serializing property.
fn atomic_single_grant(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let state = tmp.path().join("work.json");
    let item = "work:///item/atomic";
    let mut srv = WorkServer::start(h, &state, item);

    // First claim of a free item → granted, with a lease id.
    let r1 = srv.claim(item, 30_000);
    let granted1 = r1["granted"].as_bool().unwrap_or(false);
    let lease1 = r1["lease_id"].as_str().unwrap_or("").to_string();
    if !granted1 || lease1.is_empty() {
        return Outcome::fail(format!("first claim was not granted with a lease: {r1}"));
    }

    // Second claim of the SAME (still-held) item → refused. The single
    // serializing point: a held item is never double-granted.
    let r2 = srv.claim(item, 30_000);
    if r2["granted"].as_bool() != Some(false) {
        return Outcome::fail(format!("second claim of a held item was NOT refused: {r2}"));
    }

    // Release the lease → the item is re-claimable.
    let rel = srv.release(&lease1, "test");
    if rel["ok"].as_bool() != Some(true) {
        return Outcome::fail(format!("release of the held lease failed: {rel}"));
    }

    // Re-claim after release → granted again (a fresh lease).
    let r3 = srv.claim(item, 30_000);
    if r3["granted"].as_bool() != Some(true) || r3["lease_id"].as_str().unwrap_or("").is_empty() {
        return Outcome::fail(format!("re-claim after release was not granted: {r3}"));
    }

    // The counters confirm exactly one refusal across the two-grant sequence.
    let dump = srv.dump();
    Outcome::require(
        dump["claims_granted"].as_u64() == Some(2) && dump["claims_refused"].as_u64() == Some(1),
        format!(
            "expected 2 grants + 1 refusal, got grants={:?} refused={:?}",
            dump["claims_granted"], dump["claims_refused"]
        ),
    )
}

/// `work.ack` consumes the lease + marks the item done; a re-ack of the same
/// (now stale) lease is a no-op (RFC 0019 §3.5 redelivery collapse).
fn ack_consumes_lease(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let state = tmp.path().join("work.json");
    let item = "work:///item/ack";
    let mut srv = WorkServer::start(h, &state, item);

    let claim = srv.claim(item, 30_000);
    let lease = claim["lease_id"].as_str().unwrap_or("").to_string();
    if lease.is_empty() {
        return Outcome::fail(format!("claim produced no lease: {claim}"));
    }
    let ack1 = srv.ack(&lease);
    if ack1["ok"].as_bool() != Some(true) {
        return Outcome::fail(format!("first ack was not accepted: {ack1}"));
    }
    // A re-ack of the consumed lease is a no-op (already-acked collapse).
    let ack2 = srv.ack(&lease);
    if ack2["ok"].as_bool() != Some(false) {
        return Outcome::fail(format!(
            "a re-ack of a consumed lease was NOT a no-op: {ack2}"
        ));
    }
    let dump = srv.dump();
    Outcome::require(
        dump["acks"].as_u64() == Some(1),
        format!("expected exactly 1 ack, got {:?}", dump["acks"]),
    )
}

// ──────────────────────────── end-to-end via agentd ───────────────────────

/// End-to-end: a `cluster` agentd reactive instance with `--claim` claims a
/// routed item and acks it exactly once.
///
/// Topology: `confmcp` is the SOURCE (serves the item URI + pushes one
/// `updated{item}` after subscribe, firing a reaction); `workmcp` is the
/// COORDINATION server the `--claim` route names. A mock LLM scripted `"final"`
/// completes the reaction in one step → terminal `completed` → `work.ack`.
///
/// We assert from workmcp's persisted state: `claims_granted == 1` and
/// `acks == 1` — the item was claimed once and acked once.
///
/// NB: a true TWO-instance race (two daemons, one workmcp, assert exactly one
/// acks) is the gold standard but is NOT attempted here — coordinating two
/// reactive daemons + the source push deterministically in this harness is
/// flaky (the source pushes once per subscriber connection, so each daemon would
/// see its own update and both would race the SAME item; asserting the timing of
/// "exactly one wins" without flakiness needs more machinery). The single-grant
/// PROPERTY is proven at the protocol level above; the e2e here proves agentd
/// drives the claim→ack lifecycle correctly. The two-instance race is a
/// documented follow-up.
fn e2e_claim_then_ack_once(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let state = tmp.path().join("work.json");
    let item = "work:///item/e2e";

    // The mock LLM completes the reaction in one step (no tool calls needed).
    let llm = h.mock_llm("final");

    // SOURCE: confmcp serves `item` and pushes one updated after subscribe.
    let src_rec = tmp.path().join("src.jsonl");
    let source = format!(
        "src={} {} {}",
        h.confmcp().display(),
        src_rec.display(),
        item
    );
    // COORDINATION: workmcp serves the frozen work.* tools + the same item URI.
    let coord = format!(
        "work={} {} {}",
        h.workmcp().display(),
        state.display(),
        item
    );

    // The cluster build is REQUIRED: `--claim` exits 2 on a default build.
    let daemon = h.spawn_cluster(&[
        "--mode",
        "reactive",
        "--subscribe",
        item,
        "--claim",
        &format!("{item}=work"),
        "--instruction",
        "handle the work item",
        "--intelligence",
        &llm.uri,
        "--model",
        "m",
        "--mcp",
        &source,
        "--mcp",
        &coord,
        "--log-level",
        "info",
    ]);

    // Poll workmcp's persisted counters until the item is claimed AND acked, or
    // time out. The state file is written only after a work.* call (so the
    // child's inherited workmcp never clobbers it).
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = Value::Null;
    loop {
        if let Some(s) = read_state(&state) {
            let granted = s["claims_granted"].as_u64().unwrap_or(0);
            let acks = s["acks"].as_u64().unwrap_or(0);
            last = s.clone();
            if granted >= 1 && acks >= 1 {
                drop(daemon);
                // Exactly-once: claimed once, acked once (not redelivered/duped).
                return Outcome::require(
                    granted == 1 && acks == 1,
                    format!(
                        "expected claims_granted==1 && acks==1, got granted={granted} acks={acks}: {s}"
                    ),
                );
            }
        }
        if Instant::now() >= deadline {
            drop(daemon);
            return Outcome::fail(format!(
                "timed out waiting for claim+ack; last workmcp state: {last}"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Read + parse the workmcp state snapshot; `None` if the file is absent (no
/// `work.*` call has landed yet) or mid-rename.
fn read_state(path: &Path) -> Option<Value> {
    let s = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(s.trim()).ok()
}

// ───────────────────────── a stdio JSON-RPC client ────────────────────────

/// A minimal line-based JSON-RPC client over a child's stdio, to drive `workmcp`
/// directly for the protocol-level checks. Built around raw JSON (never agentd's
/// codec) — a conformance probe, not a peer.
struct WorkServer {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    id: i64,
}

impl WorkServer {
    /// Spawn `workmcp <state> <item>` and complete the MCP handshake.
    fn start(h: &Harness, state: &Path, item: &str) -> WorkServer {
        let mut child = Command::new(h.workmcp())
            .arg(state)
            .arg(item)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn workmcp");
        let stdin = child.stdin.take().expect("workmcp stdin");
        let reader = BufReader::new(child.stdout.take().expect("workmcp stdout"));
        let mut s = WorkServer {
            child,
            stdin,
            reader,
            id: 0,
        };
        let _ = s.call("initialize", json!({}));
        s
    }

    /// Send a JSON-RPC request, return the `result` value (panics on no reply).
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let line = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
            .to_string();
        writeln!(self.stdin, "{line}").expect("write to workmcp");
        self.stdin.flush().ok();
        // Skip any notification lines (no id) until our response arrives.
        loop {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf).expect("read from workmcp");
            assert!(n != 0, "workmcp closed stdout before replying to {method}");
            let Ok(v) = serde_json::from_str::<Value>(&buf) else {
                continue;
            };
            if v.get("id").and_then(Value::as_i64) == Some(self.id) {
                return v["result"].clone();
            }
        }
    }

    /// Call a `work.*` tool, returning the parsed `structuredContent` body.
    fn tool(&mut self, name: &str, args: Value) -> Value {
        let res = self.call("tools/call", json!({"name": name, "arguments": args}));
        // workmcp returns the body as structuredContent (and text content[]).
        res["structuredContent"].clone()
    }

    fn claim(&mut self, item: &str, ttl_ms: u64) -> Value {
        self.tool("work.claim", json!({"item": item, "ttl_ms": ttl_ms}))
    }
    fn ack(&mut self, lease_id: &str) -> Value {
        self.tool("work.ack", json!({"lease_id": lease_id}))
    }
    fn release(&mut self, lease_id: &str, reason: &str) -> Value {
        self.tool(
            "work.release",
            json!({"lease_id": lease_id, "reason": reason}),
        )
    }
    fn dump(&mut self) -> Value {
        self.tool("work.dump", json!({}))
    }
}

impl Drop for WorkServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
