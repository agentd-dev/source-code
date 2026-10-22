// SPDX-License-Identifier: AGPL-3.0-only
//! Where x402 rides on A2A: the a2a-x402 extension's standalone flow (the
//! x402 v2 A2A transport; RFC 0049 §7.2). A peer that charges answers with
//! a task in `input-required` whose status message's metadata says
//! `x402.payment.status: payment-required` and carries the
//! `PaymentRequired` under `x402.payment.required`; the client answers on
//! that task with a message whose metadata says `payment-submitted` and
//! carries the `PaymentPayload` under `x402.payment.payload`; the peer's
//! status messages then carry `x402.payment.receipts`, every settlement
//! response so far, the last one this payment's. A client that will not
//! pay answers on the task with a message whose metadata says
//! `payment-rejected` (v0.2 §5.4), and the peer ends the task.
//!
//! The extension's embedded flow — the payment request inside a higher
//! protocol's object in the task's artifacts (an AP2 mandate) — is not read:
//! such a task asks to be paid in a shape agentd does not pay.
//!
//! Only where things are, read from the peer's JSON as it sent it (the SDK's
//! types blank every `metadata`, see `a2a::peer`): what the objects say, and
//! whether to pay, is `crate::payments`' and `runtime::payments`'.

use a2a_types::TaskState;
use serde_json::{Value, json};

/// The extension URI the x402 v2 A2A transport names, which a client
/// activates in `A2A-Extensions`.
pub const EXTENSION: &str = "https://github.com/google-a2a/a2a-x402/v0.1";
/// The URI the extension's own specification names from its v0.2.
pub const EXTENSION_V02: &str =
    "https://github.com/google-agentic-commerce/a2a-x402/blob/main/spec/v0.2";
/// Both: a paying step activates each, and a peer's card that requires
/// either is one a paying step can talk to.
pub const EXTENSIONS: &[&str] = &[EXTENSION, EXTENSION_V02];

/// The metadata keys.
pub const STATUS: &str = "x402.payment.status";
pub const REQUIRED: &str = "x402.payment.required";
pub const PAYLOAD: &str = "x402.payment.payload";
pub const RECEIPTS: &str = "x402.payment.receipts";
pub const ERROR: &str = "x402.payment.error";

/// The `x402.payment.status` values agentd reads or writes.
pub const PAYMENT_REQUIRED: &str = "payment-required";
pub const PAYMENT_SUBMITTED: &str = "payment-submitted";
pub const PAYMENT_REJECTED: &str = "payment-rejected";
pub const PAYMENT_COMPLETED: &str = "payment-completed";
pub const PAYMENT_FAILED: &str = "payment-failed";

/// A task as the peer sent it: a `SendMessage` reply's `task`, or a bare one.
fn task(v: &Value) -> &Value {
    v.get("task").unwrap_or(v)
}

/// The metadata of a task's status message.
pub fn status_metadata(v: &Value) -> Option<&Value> {
    task(v)
        .pointer("/status/message/metadata")
        .filter(|m| m.is_object())
}

/// What the task's status message says of the payment.
pub fn payment_status(v: &Value) -> Option<&str> {
    status_metadata(v)?.get(STATUS)?.as_str()
}

/// Whether a task asks to be paid before it goes on: `input-required`, its
/// status message saying `payment-required`.
pub fn asks(v: &Value) -> bool {
    crate::a2a::peer::task_state_of(v) == TaskState::InputRequired
        && payment_status(v) == Some(PAYMENT_REQUIRED)
}

/// The payment request the task asks with, as sent: the standalone flow's
/// `x402.payment.required`. `None` in the embedded flow.
pub fn required_of(v: &Value) -> Option<&Value> {
    status_metadata(v)?.get(REQUIRED)
}

/// The last settlement response the task's status message carries, as
/// sent: this payment's, the receipts being the task's whole history.
pub fn last_receipt(v: &Value) -> Option<&Value> {
    status_metadata(v)?.get(RECEIPTS)?.as_array()?.last()
}

/// The peer's short error code, when the status message names one.
pub fn error_code(v: &Value) -> Option<&str> {
    status_metadata(v)?.get(ERROR)?.as_str()
}

/// The metadata of the message that pays: `payment-submitted`, and the
/// `PaymentPayload`.
pub fn submitted(payload: &Value) -> Value {
    json!({STATUS: PAYMENT_SUBMITTED, PAYLOAD: payload})
}

/// The metadata of the message that says agentd will not pay:
/// `payment-rejected`, and nothing else — no reason, so a peer learns
/// nothing of the spend policy or the approver.
pub fn rejected() -> Value {
    json!({STATUS: PAYMENT_REJECTED})
}

/// `v` with every `x402.payment.payload` member taken out, at any depth. A
/// peer may hand agentd's own payment message back — in the task's
/// `history`, which many servers return whole, or in a status message —
/// and the signed payload in it is never a step's output, a log line or a
/// model's context.
pub fn remove_payloads(v: &mut Value) {
    match v {
        Value::Object(o) => {
            o.remove(PAYLOAD);
            o.values_mut().for_each(remove_payloads);
        }
        Value::Array(a) => a.iter_mut().for_each(remove_payloads),
        _ => {}
    }
}

/// What a step that may not pay fails with when the peer asks to be paid.
pub fn unpaid_error(task_id: &str) -> String {
    format!(
        "a2a: the peer asks to be paid (a2a-x402) before its task {task_id} goes on, and this step has no payment field — an a2a.send or a2a.delegate step pays only with one, within security.payments (RFC 0049)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(state: &str, status: &str) -> Value {
        json!({"task": {"id": "task-123", "contextId": "c",
            "status": {"state": state, "message": {"messageId": "m", "role": "ROLE_AGENT",
                "parts": [{"text": "Payment is required to generate the image."}],
                "metadata": {STATUS: status, REQUIRED: {"x402Version": 2, "accepts": [{}]}}}}}})
    }

    /// A task asks to be paid when it waits for input and its status
    /// message says payment-required — and only then; the request is read
    /// from the metadata, as sent.
    #[test]
    fn a_task_that_asks_to_be_paid() {
        let t = asked("TASK_STATE_INPUT_REQUIRED", PAYMENT_REQUIRED);
        assert!(asks(&t));
        assert!(asks(&t["task"]), "a bare task too");
        assert_eq!(required_of(&t).unwrap()["x402Version"], 2);
        assert!(!asks(&asked("TASK_STATE_WORKING", PAYMENT_REQUIRED)));
        assert!(!asks(&asked(
            "TASK_STATE_INPUT_REQUIRED",
            PAYMENT_COMPLETED
        )));
        let mut plain = asked("TASK_STATE_INPUT_REQUIRED", PAYMENT_REQUIRED);
        plain["task"]["status"]["message"]["metadata"] = json!({});
        assert!(!asks(&plain), "a question that is not a payment");
    }

    /// The receipts are the task's history: the last one is this payment's.
    #[test]
    fn the_last_receipt_is_this_payments() {
        let mut t = asked("TASK_STATE_COMPLETED", PAYMENT_COMPLETED);
        t["task"]["status"]["message"]["metadata"][RECEIPTS] = json!([
            {"success": false, "errorReason": "insufficient_funds", "transaction": ""},
            {"success": true, "transaction": "0xab"}]);
        assert_eq!(last_receipt(&t).unwrap()["transaction"], "0xab");
        assert_eq!(payment_status(&t), Some(PAYMENT_COMPLETED));
        let m = submitted(&json!({"x402Version": 2}));
        assert_eq!(m[STATUS], PAYMENT_SUBMITTED);
        assert_eq!(m[PAYLOAD]["x402Version"], 2);
        assert_eq!(rejected(), json!({STATUS: "payment-rejected"}));
    }

    /// The payment message a peer hands back, in its history or its status,
    /// loses its payload and nothing else.
    #[test]
    fn a_payload_handed_back_is_taken_out() {
        let sent = submitted(&json!({"payload": {"signature": "0xsig"}}));
        let mut t = json!({"task": {"id": "t", "status": {"state": "TASK_STATE_COMPLETED",
            "message": {"messageId": "m", "metadata": {STATUS: PAYMENT_COMPLETED, PAYLOAD: sent[PAYLOAD].clone()}}},
            "history": [{"messageId": "a", "parts": [{"text": "the report"}]},
                        {"messageId": "p", "metadata": sent.clone()}]}});
        remove_payloads(&mut t);
        assert!(!t.to_string().contains("0xsig"), "{t}");
        assert_eq!(
            t["task"]["history"][1]["metadata"][STATUS],
            PAYMENT_SUBMITTED
        );
        assert_eq!(t["task"]["history"][0]["parts"][0]["text"], "the report");
        assert_eq!(payment_status(&t), Some(PAYMENT_COMPLETED));
    }
}
