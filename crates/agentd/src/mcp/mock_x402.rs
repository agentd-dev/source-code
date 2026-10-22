// SPDX-License-Identifier: AGPL-3.0-only
//! `mock.paid`: the built-in mock MCP server's tool that charges, answering
//! as an x402 v2 resource server does over MCP (RFC 0049 §7.1).
//!
//! A call without `_meta["x402/payment"]` is answered `isError: true` with
//! the `PaymentRequired` in `structuredContent` and, JSON-encoded, in
//! `content[0].text`. A call with one is verified as a server verifies a
//! payment (`payments::mock::verify`) — a payment that does not verify, or a
//! nonce seen before, is answered with the payment request again — and then
//! answered as the call's `outcome` argument says:
//!
//! - `settle` (the default): the tool's result, and `_meta["x402/payment-response"]`
//!   with `success: true`, a transaction and the payer;
//! - `refuse`: the payment request again (`Settlement failed`), no result;
//! - `fail`: an error result whose settlement says `success: false` with no
//!   transaction;
//! - `pending`: the result, its settlement `settlement_pending` with a
//!   transaction;
//! - `silent`: the result, with no settlement at all;
//! - `stall`: nothing for `stall_ms` (default 5 s), then as `settle`.
//!
//! The other arguments shape the request: `amount` (atomic units, default
//! `10000`), `pay_to`, `asset`, `network`, `resource` (default
//! `mcp://tool/mock.paid`) and `description`. Every call leaves a line on
//! stderr: `MOCK_X402_REQUIRED <arguments>`, or `MOCK_X402_PAID {nonce,
//! from, value, outcome}` once a payment verifies, or `MOCK_X402_INVALID
//! <reason>` — never a signature.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

/// The payee the mock is paid to unless a call names another.
pub const PAY_TO: &str = "0x209693Bc6afc0C5328bA36FaF03C514EF312287C";
/// The token it asks for: USDC on Base Sepolia.
pub const ASSET: &str = "0x036CbD53842c5426634e7929541eC2318f3dCF7e";
/// Its network.
pub const NETWORK: &str = "eip155:84532";

/// Every nonce a payment that verified carried: a second use is refused,
/// as a server's replay protection refuses it.
fn nonces() -> &'static Mutex<BTreeSet<String>> {
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    SEEN.get_or_init(Mutex::default)
}

/// The entry `mock.paid` offers for `args`.
pub fn entry(args: &Value) -> Value {
    let s = |k: &str, d: &str| args.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
    json!({"scheme": "exact", "network": s("network", NETWORK), "amount": s("amount", "10000"),
           "asset": s("asset", ASSET), "payTo": s("pay_to", PAY_TO), "maxTimeoutSeconds": 60,
           "extra": {"name": "USDC", "version": "2"}})
}

/// The `PaymentRequired` `mock.paid` answers `args` with.
fn required(args: &Value, error: &str) -> Value {
    let s = |k: &str, d: &str| args.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
    json!({"x402Version": 2, "error": error,
           "resource": {"url": s("resource", "mcp://tool/mock.paid"),
                        "description": s("description", "A report that costs a cent"),
                        "mimeType": "application/json"},
           "accepts": [entry(args)]})
}

/// A tool result asking to be paid.
fn asking(args: &Value, error: &str) -> Value {
    let r = required(args, error);
    json!({"isError": true, "structuredContent": r,
           "content": [{"type": "text", "text": r.to_string()}]})
}

/// The `tools/call` result for `mock.paid`, called with `params`.
pub fn paid(params: &Value) -> Value {
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let Some(payload) = params.pointer("/_meta/x402~1payment") else {
        eprintln!("MOCK_X402_REQUIRED {args}");
        return asking(&args, "Payment required to access this resource");
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let offered = entry(&args);
    let v = match crate::payments::mock::verify(&offered, payload, now) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("MOCK_X402_INVALID {e}");
            return asking(&args, &e);
        }
    };
    if !nonces()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(v.nonce.clone())
    {
        eprintln!("MOCK_X402_INVALID duplicate_nonce");
        return asking(&args, "DUPLICATE_NONCE");
    }
    let outcome = args
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("settle");
    eprintln!(
        "MOCK_X402_PAID {}",
        json!({"nonce": v.nonce, "from": v.payer, "value": v.value.to_string(),
               "outcome": outcome})
    );
    let network = offered["network"].clone();
    let transaction =
        crate::payments::evm::hex0x(&crate::payments::evm::keccak256(v.nonce.as_bytes()));
    let result = |settlement: Option<Value>| {
        let report = json!({"report": "the paid report", "value": v.value.to_string()});
        let mut r = json!({"isError": false, "structuredContent": report,
                           "content": [{"type": "text", "text": report.to_string()}]});
        if let Some(s) = settlement {
            r["_meta"] = json!({"x402/payment-response": s});
        }
        r
    };
    let settled = json!({"success": true, "transaction": transaction, "network": network,
                         "payer": v.payer});
    match outcome {
        "refuse" => asking(&args, "Settlement failed"),
        "fail" => json!({"isError": true,
            "content": [{"type": "text", "text": "insufficient funds"}],
            "_meta": {"x402/payment-response": {"success": false, "errorReason": "insufficient_funds",
                      "transaction": "", "network": network, "payer": v.payer}}}),
        "pending" => result(Some(
            json!({"success": false, "errorReason": "settlement_pending",
                                        "transaction": transaction, "network": network}),
        )),
        "silent" => result(None),
        "stall" => {
            let ms = args.get("stall_ms").and_then(Value::as_u64).unwrap_or(5000);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            result(Some(settled))
        }
        _ => result(Some(settled)),
    }
}
