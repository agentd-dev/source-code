//! The work-claim / lease client — agentd's participant half of the cross-instance
//! ownership convention (RFC 0019 §3; the `work.*` names + `_meta` keys are FROZEN
//! in RFC 0015 §5.6). [feature = "cluster"]
//!
//! agentd does **not** run a queue. Before a reactive worker processes a claim
//! route's item, it `work.claim`s it against a coordination MCP server (a declared
//! `--mcp` server) and proceeds only on a granted lease; it `work.ack`s on a
//! terminal `completed`, or `work.release`s on a non-terminal wind-down / drain.
//! A dead claimer's lease expires server-side and another replica re-claims. The
//! tools' *schemas* are the server's (discovered via `tools/list`); the names and
//! the `_meta` convention below are the frozen contract.
//!
//! **No secret / URL in `_meta` ever** (RFC 0015 §5.6 / RFC 0012 §3.7): the only
//! keys emitted are `agentd/claim_key`, `agentd/instance`, `agentd/shard` (omitted
//! if unsharded), and `traceparent` (if present). The item URI is a `work.claim`
//! *argument*, never a `_meta` value, and is never logged at info beyond the
//! claim-event lines the reactor already emits for routing.

use crate::config::ClaimStyle;
use crate::mcp::client::McpClient;
use crate::wire::mcp::Tool;
use serde_json::{Value, json};
use std::time::Duration;

/// The frozen `work.*` tool names (RFC 0015 §5.6). agentd *calls* these on the
/// coordination server; it never serves them.
pub const TOOL_CLAIM: &str = "work.claim";
pub const TOOL_RENEW: &str = "work.renew";
pub const TOOL_ACK: &str = "work.ack";
pub const TOOL_RELEASE: &str = "work.release";

/// A live, server-bound claim route. Built in `run_reactive` from a
/// [`crate::config::ClaimRoute`] once the coordination server is connected and
/// validated (`server_idx` is its index in the connected-server vec). `route_id`
/// is the stable per-route string folded into the claim-key derivation (the URI
/// in v1), so a redelivered item maps to the SAME key (RFC 0019 §3.5).
#[derive(Debug, Clone)]
pub struct ClaimSpec {
    pub server_idx: usize,
    pub ttl: Duration,
    pub renew_fraction: f64,
    pub style: ClaimStyle,
    pub route_id: String,
    /// Whether this claim route delivers into a warm `--continue` session (RFC
    /// 0019 §3.4): the lease is held across many deliveries for the session's
    /// life — claimed on the session's first delivery, renewed by the heartbeat
    /// while live, acked/released when the session ENDS — rather than
    /// claimed→settled within one synchronous spawn. Spawn-claims never need the
    /// heartbeat (settled inline within a tick); continue-claims do.
    pub continue_session: bool,
}

/// The outcome of a `work.claim` round-trip (RFC 0019 §3.2). `Granted` carries the
/// opaque lease id (used by renew/ack/release) and the server-authoritative TTL;
/// `Lost` means another replica owns the item (drop + count); `Error` is a
/// transport/protocol failure (the daemon keeps serving — RFC 0019 §8 row 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Granted {
        lease_id: String,
        expires_in_ms: u64,
    },
    Lost {
        held_by: Option<String>,
    },
    Error(String),
}

/// Claim one item: call `work.claim` with `{item, ttl_ms}` and the frozen
/// `_meta`. Parses `{granted:true, lease_id, expires_in_ms}` /
/// `{granted:false, held_by}` out of the result's `structuredContent` (preferred)
/// or its text `content[]` (fallback — the codebase parses both shapes). A
/// transport error maps to [`ClaimOutcome::Error`]; a tool-domain `isError:true`
/// or an unparseable body is treated as "not granted" (`Error`), never a silent
/// proceed. `meta` is the per-call `_meta` (built by the reactor), merged over the
/// persistent run-id stamp by [`McpClient::call_tool_with_meta`].
pub fn claim(client: &McpClient, item: &str, ttl: Duration, meta: Value) -> ClaimOutcome {
    let args = json!({ "item": item, "ttl_ms": ttl.as_millis() as u64 });
    match client.call_tool_with_meta(TOOL_CLAIM, Some(args), meta) {
        Ok(res) => {
            if res.is_error() {
                return ClaimOutcome::Error(format!("work.claim isError: {}", res.text()));
            }
            parse_claim_result(&result_value(&res))
        }
        Err(e) => ClaimOutcome::Error(e.to_string()),
    }
}

/// Claim one item, dispatching on the route's [`ClaimStyle`] (RFC 0019 §3.3 / RFC
/// 0015 §5.6). `Tool` (the default + implemented path) calls `work.claim`.
///
/// `Resource` is a **documented stub** in v1, and DELIBERATELY so: RFC 0015 §5.6
/// freezes the *direction* ("`work.claim` degenerates to a conditional CAS
/// `tools/call` the server exposes, observed after a `resources/read`") but does
/// NOT freeze the CAS tool's NAME or its compare-and-set argument shape. Building
/// it would mean inventing an unfrozen server-side contract that two servers
/// could interpret differently — a path that could **double-grant**, which is the
/// one thing the claim convention must never do (RFC 0019 §8 row 1 / §10). So
/// rather than half-build it, a `resource`-style claim returns a loud `Error`
/// (the daemon skips the delivery, keeps serving) — never a silent proceed.
///
/// A `resource`-style route also fails startup validation today
/// (`advertises_work_tools` requires `work.claim`+`work.ack`, which a pure
/// resource-lease server need not advertise → exit 2), so this is a
/// belt-and-braces second guard, not the primary gate. When the CAS contract is
/// frozen, the implementation slots in HERE behind the same `ClaimOutcome` so the
/// gate (and the whole lifecycle) is untouched.
pub fn claim_styled(
    client: &McpClient,
    style: ClaimStyle,
    item: &str,
    ttl: Duration,
    meta: Value,
) -> ClaimOutcome {
    match style {
        ClaimStyle::Tool => claim(client, item, ttl, meta),
        ClaimStyle::Resource => resource_style_unimplemented(),
    }
}

/// The `resource`-style (CAS) documented-stub outcome (RFC 0015 §5.6): a loud
/// `Error`, never a silent grant. Factored out so the stub message has one home
/// and a test can assert it without a live client.
fn resource_style_unimplemented() -> ClaimOutcome {
    ClaimOutcome::Error(
        "claim.style=resource (CAS) is not implemented in v1 — the compare-and-set \
         contract is not frozen (RFC 0015 §5.6); use claim.style=tool"
            .into(),
    )
}

/// Extend a held lease (RFC 0019 §3.3). Best-effort: returns the transport error
/// string on failure so the caller can log/count it; never panics. (In the
/// synchronous-spawn v1 this is currently a documented no-op at the call site —
/// the function exists so a future progress-aware path can heartbeat without a
/// new contract.)
pub fn renew(client: &McpClient, lease_id: &str, ttl: Duration) -> Result<(), String> {
    let args = json!({ "lease_id": lease_id, "ttl_ms": ttl.as_millis() as u64 });
    call_lease_tool(client, TOOL_RENEW, args, Value::Null)
}

/// Ack a completed item (RFC 0019 §3.3 / §3.5): the durable side effect, keyed on
/// `agentd/claim_key`, is committed; a redelivered-but-already-acked item is a
/// server-side no-op. Carries `_meta.agentd/claim_key` so the server can collapse
/// the ack on the SAME item-derived key (RFC 0015 §5.6).
pub fn ack(client: &McpClient, lease_id: &str, claim_key: &str) -> Result<(), String> {
    let args = json!({ "lease_id": lease_id });
    let meta = json!({ "agentd/claim_key": claim_key });
    call_lease_tool(client, TOOL_ACK, args, meta)
}

/// Release a held lease without completing (drain / non-terminal wind-down, RFC
/// 0019 §3.3 / §6): the item is immediately re-claimable. `reason` is surfaced to
/// the server (`"draining"` / `"wind-down"`).
pub fn release(client: &McpClient, lease_id: &str, reason: &str) -> Result<(), String> {
    let args = json!({ "lease_id": lease_id, "reason": reason });
    call_lease_tool(client, TOOL_RELEASE, args, Value::Null)
}

/// A deterministic, item-derived claim key (RFC 0019 §3.5 / RFC 0015 §5.6). Stable
/// per `(item_uri, route_id)` so a redelivered item maps to the SAME key — the
/// dedupe key every downstream side-effect `tools/call` then rides (RFC 0011 §6.2),
/// making the first claimer and a post-expiry second claimer write under one key.
///
/// Reuses the SINGLE fleet-wide FNV-1a/64 ([`crate::cluster::shard::fnv1a64`]) — no
/// second hash. Two FNV passes (the second over the first's hex + the route id)
/// give a 32-hex digest with a stable, run-id-like length and a vanishing
/// collision probability across the item×route space.
pub fn derive_claim_key(item_uri: &str, route_id: &str) -> String {
    use crate::cluster::shard::fnv1a64;
    let seed = format!("{item_uri}\0{route_id}");
    let h1 = fnv1a64(seed.as_bytes());
    // A second pass over the first digest + the route id de-correlates the two
    // halves so distinct items/routes spread across the whole 128-bit-ish space.
    let mix = format!("{h1:016x}\0{route_id}");
    let h2 = fnv1a64(mix.as_bytes());
    format!("{h1:016x}{h2:016x}")
}

/// The live validation predicate (RFC 0015 §5.6 / RFC 0019 §3.6, edge cases 4-5):
/// a coordination server is a valid claim server iff it advertises BOTH
/// `work.claim` AND `work.ack` in its `tools/list`. A server that is *up but
/// lacks* them is an operator wiring mistake → exit 2 (the reactor maps it); a
/// server that is *down* never reaches this predicate (the connect loop exits 6).
pub fn advertises_work_tools(tools: &[Tool]) -> bool {
    let has = |name: &str| tools.iter().any(|t| t.name == name);
    has(TOOL_CLAIM) && has(TOOL_ACK)
}

// ---- internals ----

/// Call a lease tool (renew/ack/release), mapping a transport error OR a
/// tool-domain `isError:true` to an `Err(message)`. These are best-effort
/// lifecycle calls: the caller logs/counts the error and relies on the lease TTL
/// as the backstop (RFC 0019 §6).
fn call_lease_tool(
    client: &McpClient,
    tool: &str,
    args: Value,
    extra_meta: Value,
) -> Result<(), String> {
    match client.call_tool_with_meta(tool, Some(args), extra_meta) {
        Ok(res) if res.is_error() => Err(format!("{tool} isError: {}", res.text())),
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// The machine-readable body of a `CallToolResult`: prefer `structuredContent`,
/// else parse the concatenated text `content[]` as JSON (the two shapes the
/// codebase already handles). Returns `Value::Null` when neither yields an object.
fn result_value(res: &crate::wire::mcp::CallToolResult) -> Value {
    if let Some(sc) = &res.structured_content {
        return sc.clone();
    }
    serde_json::from_str(&res.text()).unwrap_or(Value::Null)
}

/// Parse the `work.claim` result body into a [`ClaimOutcome`] (RFC 0015 §5.6). A
/// missing/false `granted` with no usable shape is treated as `Lost` only when the
/// body is a recognisable object; a wholly unparseable body is an `Error` (we
/// never silently proceed on an ambiguous claim).
fn parse_claim_result(v: &Value) -> ClaimOutcome {
    let Some(obj) = v.as_object() else {
        return ClaimOutcome::Error("work.claim: unparseable result body".into());
    };
    match obj.get("granted").and_then(Value::as_bool) {
        Some(true) => {
            let lease_id = obj
                .get("lease_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if lease_id.is_empty() {
                return ClaimOutcome::Error("work.claim granted without a lease_id".into());
            }
            let expires_in_ms = obj
                .get("expires_in_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            ClaimOutcome::Granted {
                lease_id,
                expires_in_ms,
            }
        }
        Some(false) => {
            let held_by = obj
                .get("held_by")
                .and_then(Value::as_str)
                .map(str::to_string);
            ClaimOutcome::Lost { held_by }
        }
        None => ClaimOutcome::Error("work.claim result missing 'granted'".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::mcp::CallToolResult;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            title: None,
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
        }
    }

    #[test]
    fn derive_claim_key_is_deterministic_and_distinct() {
        // Same (item, route) → same key, every time (redelivery dedup, §3.5).
        let a = derive_claim_key("file:///inbox/42.json", "route-1");
        let b = derive_claim_key("file:///inbox/42.json", "route-1");
        assert_eq!(a, b);
        // run-id-like length: 32 hex chars (two FNV passes).
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Distinct items differ; distinct routes for the same item differ.
        assert_ne!(a, derive_claim_key("file:///inbox/43.json", "route-1"));
        assert_ne!(a, derive_claim_key("file:///inbox/42.json", "route-2"));
    }

    #[test]
    fn advertises_work_tools_requires_claim_and_ack() {
        assert!(advertises_work_tools(&[
            tool("work.claim"),
            tool("work.ack")
        ]));
        assert!(advertises_work_tools(&[
            tool("work.claim"),
            tool("work.ack"),
            tool("work.renew"),
            tool("other"),
        ]));
        // Missing either → not a claim server (exit 2 in the reactor).
        assert!(!advertises_work_tools(&[tool("work.claim")]));
        assert!(!advertises_work_tools(&[tool("work.ack")]));
        assert!(!advertises_work_tools(&[]));
    }

    #[test]
    fn parse_claim_granted_lost_and_error() {
        // granted with a lease.
        let g = parse_claim_result(
            &json!({"granted": true, "lease_id": "L-1", "expires_in_ms": 30000}),
        );
        assert_eq!(
            g,
            ClaimOutcome::Granted {
                lease_id: "L-1".into(),
                expires_in_ms: 30000
            }
        );
        // granted but no lease → error (never proceed on an ambiguous grant).
        assert!(matches!(
            parse_claim_result(&json!({"granted": true})),
            ClaimOutcome::Error(_)
        ));
        // lost, with and without held_by.
        assert_eq!(
            parse_claim_result(&json!({"granted": false, "held_by": "pod-xyz"})),
            ClaimOutcome::Lost {
                held_by: Some("pod-xyz".into())
            }
        );
        assert_eq!(
            parse_claim_result(&json!({"granted": false})),
            ClaimOutcome::Lost { held_by: None }
        );
        // missing `granted` / unparseable → error.
        assert!(matches!(
            parse_claim_result(&json!({"other": 1})),
            ClaimOutcome::Error(_)
        ));
        assert!(matches!(
            parse_claim_result(&json!("nope")),
            ClaimOutcome::Error(_)
        ));
    }

    #[test]
    fn resource_style_claim_is_a_loud_stub_never_a_silent_grant() {
        // A `resource`-style claim must NEVER silently proceed (the CAS contract
        // is unfrozen — RFC 0015 §5.6); it is a loud `Error` the gate skips on,
        // so it can never double-grant. (The tool arm is covered end-to-end by
        // the work_claim conformance suite, which needs a live coordination
        // server.)
        match resource_style_unimplemented() {
            ClaimOutcome::Error(e) => {
                assert!(e.contains("resource"), "stub error names the style: {e}");
                assert!(e.contains("not frozen"), "stub error explains why: {e}");
            }
            other => panic!("resource-style must be a loud Error, got {other:?}"),
        }
    }

    #[test]
    fn result_value_prefers_structured_then_text_json() {
        // structuredContent wins.
        let res = CallToolResult {
            content: vec![json!({"type": "text", "text": "{\"granted\":false}"})],
            is_error: None,
            structured_content: Some(json!({"granted": true, "lease_id": "L"})),
        };
        assert_eq!(result_value(&res)["granted"], json!(true));
        // text content[] JSON is the fallback when structuredContent is absent.
        let res2 = CallToolResult {
            content: vec![
                json!({"type": "text", "text": "{\"granted\":true,\"lease_id\":\"L2\"}"}),
            ],
            is_error: None,
            structured_content: None,
        };
        assert_eq!(result_value(&res2)["lease_id"], json!("L2"));
    }
}
