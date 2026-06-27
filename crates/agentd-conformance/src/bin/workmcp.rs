//! `workmcp <state-file> [item-uri]` — a minimal, spec-correct MCP server that
//! serves the FROZEN `work.*` coordination contract (RFC 0015 §5.6) with
//! **atomic single-grant** lease semantics, so the cross-instance claim
//! convention (RFC 0019 §3) is testable end-to-end. agentd connects to it as an
//! MCP client (the coordination server a `--claim` route names).
//!
//! It is modelled on `confmcp.rs`: a line-based stdio NDJSON JSON-RPC server,
//! independent of the agentd library on purpose. Beyond the handshake +
//! resource methods (so a reactive agentd can subscribe to the work item and be
//! poked into a reaction), it advertises and serves the four `work.*` tools:
//!
//!   * `work.claim{item, ttl_ms}` — grant a lease iff the item is unleased (or
//!     its lease expired / was released / it is idle after an ack). The grant is
//!     ATOMIC: a second claim of a still-held item is refused with
//!     `{granted:false, held_by}`. This is the single serializing point.
//!   * `work.renew{lease_id, ttl_ms}` — extend a live lease.
//!   * `work.ack{lease_id}` — mark the item done (the lease is consumed).
//!   * `work.release{lease_id, reason}` — free the item (re-claimable now).
//!
//! All state is in-process (a `HashMap` keyed by item URI). After every `work.*`
//! call it rewrites `<state-file>` with a JSON snapshot (per-item lease/acked +
//! the global `claims_granted` / `acks` counters) so the conformance check can
//! ASSERT "granted exactly once" / "acked exactly once" by reading the file —
//! no extra tool round-trip and no shared memory needed.
//!
//! Single-threaded except for the one `subscribe`-triggered `updated` push
//! `confmcp` also uses; the lease map lives on the main thread.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

/// One work item's lease state. `None` lease ⇒ the item is free (claimable).
struct Item {
    /// The currently-held lease, if any: `(lease_id, expires_at_ms)`.
    lease: Option<(String, u128)>,
    /// Whether this item has been acked (work done). An acked, un-leased item is
    /// reported "done" and re-claiming it is still granted (a fresh attempt) —
    /// the conformance model only needs the ack COUNT, which never double-counts.
    acked: bool,
}

/// The whole server's mutable state: per-item leases + global counters + the
/// monotonic lease-id sequence.
#[derive(Default)]
struct State {
    items: HashMap<String, Item>,
    /// Total successful `work.claim` grants (the "granted exactly once" signal).
    claims_granted: u64,
    /// Total `work.ack`s accepted (the "acked exactly once" signal).
    acks: u64,
    /// Total `work.release`s accepted.
    releases: u64,
    /// Total `work.claim`s refused because the item was already held.
    claims_refused: u64,
    /// Monotonic lease-id counter (lease ids are `L-<n>`).
    next_lease: u64,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let state_path = args.next().expect("usage: workmcp <state-file> [item-uri]");
    let item_uri = args.next().unwrap_or_else(|| "work:///item/1".to_string());

    let mut state = State::default();
    // NB: we do NOT seed-write the state file at startup. A reactive agentd
    // passes its `--mcp work=…` server to the spawned reaction too, so a SECOND
    // workmcp process exists (the child's) pointed at the SAME state file — but
    // the child never issues a `work.*` `tools/call`, so it must never write.
    // Writing only after a tools/call (below) keeps the file owned by whichever
    // process actually claims/acks (the supervisor's). The check treats a missing
    // file as "no calls yet" (zeros).

    let stdin = io::stdin();
    let stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        let method = req["method"].as_str().unwrap_or("");
        let id = req.get("id").cloned();
        // Requests carry an id; notifications don't and get no reply.
        let Some(id) = id else { continue };

        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-11-25",
                // Advertise BOTH resources (so a reactive agentd can subscribe to
                // the work item) and tools (the work.* coordination surface).
                "capabilities": {"resources": {"subscribe": true, "listChanged": true}, "tools": {}},
                "serverInfo": {"name": "workmcp", "version": "1.0.0"}
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": tool_defs()}),
            "resources/list" => json!({"resources": [{"uri": item_uri, "name": "work-item"}]}),
            "resources/read" => {
                json!({"contents": [{"uri": item_uri, "mimeType": "text/plain", "text": "work pending"}]})
            }
            "resources/subscribe" | "resources/unsubscribe" => json!({}),
            "tools/call" => {
                let res = handle_tool_call(&req["params"], &mut state);
                // Persist the snapshot after every state-mutating call so the
                // check can assert grant-once / ack-once from the file.
                write_state(&state_path, &state);
                res
            }
            _ => {
                reply(
                    &stdout,
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}}),
                );
                continue;
            }
        };
        reply(
            &stdout,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        );

        // After a subscribe, push one resources/updated for the work item so a
        // reactive agentd fires a reaction (which then runs the claim gate). Same
        // pattern confmcp uses to make the client path observable.
        if method == "resources/subscribe" {
            let uri = item_uri.clone();
            std::thread::spawn(move || {
                let stdout = io::stdout();
                std::thread::sleep(std::time::Duration::from_millis(150));
                let note = json!({"jsonrpc": "2.0", "method": "notifications/resources/updated", "params": {"uri": uri}});
                reply(&stdout, note);
            });
        }
    }
}

/// The four frozen `work.*` tools (RFC 0015 §5.6) plus a `work.dump` introspection
/// tool the conformance check can call to read counters via a tool round-trip
/// (an alternative to the state file). Minimal input schemas — the server owns
/// the schema; the names are the frozen contract.
fn tool_defs() -> Value {
    json!([
        {
            "name": "work.claim",
            "description": "atomically lease one item",
            "inputSchema": {
                "type": "object",
                "properties": {"item": {"type": "string"}, "ttl_ms": {"type": "integer"}},
                "required": ["item"]
            }
        },
        {
            "name": "work.renew",
            "description": "extend a held lease",
            "inputSchema": {
                "type": "object",
                "properties": {"lease_id": {"type": "string"}, "ttl_ms": {"type": "integer"}},
                "required": ["lease_id"]
            }
        },
        {
            "name": "work.ack",
            "description": "mark work done; the side effect is committed",
            "inputSchema": {
                "type": "object",
                "properties": {"lease_id": {"type": "string"}},
                "required": ["lease_id"]
            }
        },
        {
            "name": "work.release",
            "description": "relinquish a lease without completing; re-claimable now",
            "inputSchema": {
                "type": "object",
                "properties": {"lease_id": {"type": "string"}, "reason": {"type": "string"}},
                "required": ["lease_id"]
            }
        },
        {
            "name": "work.dump",
            "description": "introspection: the lease counters + per-item state",
            "inputSchema": {"type": "object"}
        }
    ])
}

/// Dispatch one `tools/call`. Returns the JSON-RPC `result` for the call: a
/// `CallToolResult` carrying the body BOTH as `structuredContent` (preferred by
/// agentd's claim parser) AND as a text `content[]` JSON string (the fallback
/// shape) — so it parses regardless of which path agentd takes.
fn handle_tool_call(params: &Value, state: &mut State) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];
    match name {
        "work.claim" => {
            let item = args["item"].as_str().unwrap_or("").to_string();
            let ttl_ms = args["ttl_ms"].as_u64().unwrap_or(30_000);
            tool_result(claim(state, &item, ttl_ms))
        }
        "work.renew" => {
            let lease_id = args["lease_id"].as_str().unwrap_or("");
            let ttl_ms = args["ttl_ms"].as_u64().unwrap_or(30_000);
            tool_result(renew(state, lease_id, ttl_ms))
        }
        "work.ack" => {
            let lease_id = args["lease_id"].as_str().unwrap_or("");
            tool_result(ack(state, lease_id))
        }
        "work.release" => {
            let lease_id = args["lease_id"].as_str().unwrap_or("");
            tool_result(release(state, lease_id))
        }
        "work.dump" => tool_result(dump(state)),
        other => json!({
            "content": [{"type": "text", "text": format!("unknown tool: {other}")}],
            "isError": true
        }),
    }
}

/// Atomic claim (RFC 0019 §3.2). Grants iff the item has no LIVE lease (free, or
/// its lease expired / was released). A still-held item is refused — the single
/// serializing point that makes exactly-one-owner hold across replicas.
fn claim(state: &mut State, item: &str, ttl_ms: u64) -> Value {
    let now = now_ms();
    let entry = state.items.entry(item.to_string()).or_insert(Item {
        lease: None,
        acked: false,
    });
    // A live (un-expired) lease means another claimer holds it → refuse.
    if let Some((held_lease, expires_at)) = &entry.lease
        && *expires_at > now
    {
        state.claims_refused += 1;
        return json!({"granted": false, "held_by": held_lease});
    }
    // Free (or expired/released): grant a fresh lease. Granting after an ack is a
    // fresh attempt; the ack COUNT is what the check asserts, never re-incremented.
    state.next_lease += 1;
    let lease_id = format!("L-{}", state.next_lease);
    let expires_at = now + ttl_ms as u128;
    entry.lease = Some((lease_id.clone(), expires_at));
    entry.acked = false;
    state.claims_granted += 1;
    json!({"granted": true, "lease_id": lease_id, "expires_in_ms": ttl_ms})
}

/// Extend a live lease. `ok:true` if the lease exists on some item.
fn renew(state: &mut State, lease_id: &str, ttl_ms: u64) -> Value {
    let now = now_ms();
    for item in state.items.values_mut() {
        if let Some((lid, expires_at)) = &mut item.lease
            && lid == lease_id
        {
            *expires_at = now + ttl_ms as u128;
            return json!({"ok": true, "expires_in_ms": ttl_ms});
        }
    }
    json!({"ok": false, "reason": "no such lease"})
}

/// Ack a completed item (RFC 0019 §3.3/§3.5): consume the lease + mark done. A
/// re-ack of a stale lease is a no-op `ok:false` (a redelivered-but-already-acked
/// item collapses — the server-side dedupe the contract relies on).
fn ack(state: &mut State, lease_id: &str) -> Value {
    for item in state.items.values_mut() {
        if let Some((lid, _)) = &item.lease
            && lid == lease_id
        {
            item.lease = None;
            item.acked = true;
            state.acks += 1;
            return json!({"ok": true, "acked": true});
        }
    }
    json!({"ok": false, "reason": "no such lease (already acked/released?)"})
}

/// Release a held lease (RFC 0019 §3.3/§6): free the item (re-claimable now).
fn release(state: &mut State, lease_id: &str) -> Value {
    for item in state.items.values_mut() {
        if let Some((lid, _)) = &item.lease
            && lid == lease_id
        {
            item.lease = None;
            state.releases += 1;
            return json!({"ok": true, "released": true});
        }
    }
    json!({"ok": false, "reason": "no such lease"})
}

/// Introspection body: the global counters + per-item lease/acked state. Same
/// shape `write_state` persists, so a check can read either path.
fn dump(state: &State) -> Value {
    snapshot(state)
}

/// The persisted / introspectable state snapshot.
fn snapshot(state: &State) -> Value {
    let now = now_ms();
    let items: Value = state
        .items
        .iter()
        .map(|(uri, it)| {
            let leased = it.lease.as_ref().is_some_and(|(_, exp)| *exp > now);
            (
                uri.clone(),
                json!({
                    "leased": leased,
                    "lease_id": it.lease.as_ref().map(|(l, _)| l.clone()),
                    "acked": it.acked,
                }),
            )
        })
        .collect::<serde_json::Map<String, Value>>()
        .into();
    json!({
        "claims_granted": state.claims_granted,
        "acks": state.acks,
        "releases": state.releases,
        "claims_refused": state.claims_refused,
        "items": items,
    })
}

/// Wrap a body as a `CallToolResult` carrying it BOTH as `structuredContent` and
/// as a text `content[]` JSON string (the two shapes agentd's claim parser
/// accepts, `cluster::claim::result_value`).
fn tool_result(body: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": body.to_string()}],
        "structuredContent": body,
    })
}

/// Atomically rewrite the state file with the current snapshot. Best-effort: a
/// failed write never breaks the server (the check would observe stale state and
/// fail loudly, which is correct).
fn write_state(path: &str, state: &State) {
    let snap = snapshot(state).to_string();
    // Write to a temp sibling + rename so a concurrent reader never sees a torn
    // file (the conformance check polls this while the daemon runs).
    let tmp = format!("{path}.tmp");
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        && writeln!(f, "{snap}").is_ok()
    {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn reply(stdout: &io::Stdout, msg: Value) {
    let mut w = stdout.lock();
    let _ = writeln!(w, "{msg}");
    let _ = w.flush();
}
