// SPDX-License-Identifier: AGPL-3.0-only
//! An `a2a.send` or `a2a.delegate` step's payment (RFC 0049 §7.2), over the
//! a2a-x402 extension: the peer's task that asks to be paid goes to the
//! loop, and once the payment is allowed it is answered on that task — the
//! message's metadata `payment-submitted` and the `PaymentPayload` — on the
//! connection it asked on. The settlement is the receipt the peer's status
//! messages carry: a send follows the task until that receipt, a
//! delegation until the task ends.
//!
//! The payment request's `resource` is recorded, not held to anything: the
//! payment goes back on the peer's own task, over the connection that
//! asked, so it can pay for nothing else.
//!
//! The step's output is `{result, payment}`: the answer a send or a
//! delegation always has — the peer's reply, the delegation's result — and
//! the payment, `null` when the peer asked for none. The peer is asked for
//! none of the task's history once it is paid, and the task as the output
//! carries no `x402.payment.payload`, wherever the peer put one.
//!
//! A payment agentd will not make — refused, or its approval declined or
//! timed out — is answered on the peer's task with `payment-rejected`, on
//! the same connection, so the peer ends the task.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Answer, Asked, PaidCall};
use crate::a2a::x402 as wire;
use crate::mcp::a2a_client::{DelegateOutcome, PaidPeer, Reached};
use crate::payments::x402::Settlement;

/// The longest a `payment-rejected` waits for the peer to take it, within
/// the step's `timeout`.
const REJECT_WITHIN: Duration = Duration::from_secs(10);

/// The peer's task that asked to be paid, and how its paid answer goes.
pub(crate) struct PeerTask {
    pub peer: String,
    pub paid: Box<PaidPeer>,
    /// A delegation follows the task to its end; a send, to the payment's
    /// receipt.
    pub delegate: bool,
    /// How long the paid answer is followed: the step's `timeout`.
    pub timeout: Duration,
    /// The step's hop.
    pub hop: Option<crate::obs::trace::TraceContext>,
}

impl PaidCall for PeerTask {
    /// Any: the payment goes back on the peer's own task, over the
    /// connection that asked, so it pays for nothing else.
    fn is_resource(&self, _url: &str) -> bool {
        true
    }

    /// The peer's endpoint.
    fn shown(&self) -> String {
        self.paid.endpoint()
    }

    /// The peer's task id quoted, as the peer chose it; the peer and its
    /// endpoint are the configured ones.
    fn describe(&self) -> String {
        format!(
            "the task {} of the A2A peer {} ({})",
            crate::sec::render::quoted(self.paid.task_id(), super::SAID_SHOWN),
            self.peer,
            self.paid.endpoint()
        )
    }

    /// The paid answer on the peer's task, followed. The receipt is the
    /// settlement; `payment-failed`, or the task asking again, the peer's
    /// word that it took none; an HTTP 4xx, or a JSON-RPC error other than
    /// an internal one, to the payment message a refusal; no answer the
    /// payment may have reached.
    fn send_paid(&mut self, payload: &Value, run: &str, step: &str) -> Answer {
        let start = crate::obs::otel::now_unix_nanos();
        let deadline = Instant::now() + self.timeout;
        let mut a = self.paid.pay(payload, self.delegate, deadline);
        // The task as the step's output: never the payment it was handed.
        if let Some(t) = a.task.as_mut() {
            wire::remove_payloads(t);
        }
        let task = self.paid.task_id().to_string();
        let answer = match a.reached {
            Reached::Unsent(why) => Answer::Unanswered {
                why,
                may_have_sent: false,
            },
            Reached::Unanswered(why) => Answer::Unanswered {
                why: format!("no answer to the payment message: {why}"),
                may_have_sent: true,
            },
            Reached::Refused(e) => Answer::Answered {
                output: json!({"result": null}),
                settlement: None,
                declined: Some(format!("the peer refused the payment message ({e})")),
                error: None,
            },
            Reached::Accepted => {
                let failed = a
                    .task
                    .as_ref()
                    .and_then(|t| wire::payment_status(t))
                    .is_some_and(|s| s == wire::PAYMENT_FAILED);
                let code = a
                    .task
                    .as_ref()
                    .and_then(|t| wire::error_code(t))
                    .unwrap_or("no code given")
                    .to_string();
                let declined = if a.asked_again {
                    Some("the peer asks to be paid again: it did not take this payment".into())
                } else if failed {
                    Some(format!("the peer says the payment failed ({code})"))
                } else {
                    None
                };
                let (result, error) = if self.delegate {
                    match a.outcome {
                        Some(DelegateOutcome::Distillate(text)) => (
                            serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)),
                            None,
                        ),
                        Some(DelegateOutcome::Error(e)) => (Value::Null, Some(e)),
                        None => (
                            Value::Null,
                            Some(format!(
                                "a2a: the peer's task {task} did not end within the step's timeout after the payment"
                            )),
                        ),
                    }
                } else {
                    (a.task.clone().unwrap_or(Value::Null), None)
                };
                Answer::Answered {
                    output: json!({"result": result}),
                    settlement: a.receipt.as_ref().map(Settlement::from_value),
                    declined,
                    error,
                }
            }
        };
        let ok = matches!(
            &answer,
            Answer::Answered {
                settlement: Some(Ok(s)),
                error: None,
                ..
            } if s.success
        );
        crate::obs::otel::span(
            self.hop.as_ref(),
            &format!("a2a.pay {}", self.peer),
            crate::obs::otel::Kind::Client,
            start,
        )
        .attr("rpc.system", "jsonrpc")
        .attr("rpc.method", "SendMessage")
        .attr("agentd.a2a.peer", self.peer.as_str())
        .attr("agentd.run", run)
        .attr("agentd.step", step)
        .end(ok);
        answer
    }

    /// A peer is told, so it ends its task.
    fn rejects(&self) -> bool {
        true
    }

    /// `payment-rejected` on the peer's task, over the connection that
    /// asked.
    fn reject(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + self.timeout.min(REJECT_WITHIN);
        self.paid.reject(deadline)
    }
}

/// The asking of a peer's task that asks to be paid, for the loop.
pub(crate) fn asked(run: String, step: String, mut task: Value, call: PeerTask) -> Asked {
    wire::remove_payloads(&mut task);
    let id = call.paid.task_id().to_string();
    let required = wire::required_of(&task).cloned().ok_or_else(|| {
        format!(
            "a2a: the peer's task {id} asks to be paid with no {} — the extension's embedded flow, which agentd does not pay",
            wire::REQUIRED
        )
    });
    Asked {
        run,
        step,
        required,
        first: json!({"result": task}),
        call: Box::new(call),
    }
}
