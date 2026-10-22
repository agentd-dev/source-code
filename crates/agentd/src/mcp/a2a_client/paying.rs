// SPDX-License-Identifier: AGPL-3.0-only
//! A send or a delegation that may pay (RFC 0049 §7.2): it activates
//! a2a-x402 on every request, and a peer's task that asks to be paid is
//! handed back with the connection it asked on — the card's terms, the
//! tenant, the credentials — for the supervisor to decide whether to pay.
//! Nothing here decides that, or signs.
//!
//! [`PaidPeer::pay`] answers the task: one `SendMessage` on it, the
//! message's metadata `payment-submitted` and the `PaymentPayload`, marked
//! with a2a-x402; then the task is followed — until this payment's receipt
//! is in hand, or, for a delegation, until the task ends — and what came of
//! it said as plainly as the ledger needs: whether the message never left,
//! was refused, or left with no readable answer, and the receipt, if one
//! came. [`PaidPeer::reject`] answers it with `payment-rejected` instead.

use super::{
    DelegateOutcome, Follow, Followed, HttpConn, HttpEp, POLL_INTERVAL, PeerAuth, delegate_with,
    mint_message_id, request_timeout, terminal_outcome, transport_error,
};
use crate::a2a::peer as a2a;
use crate::a2a::x402;
use crate::config::A2aEndpoint;
use ::mcp::rpc::{Id, Request};
use serde_json::Value;
use std::time::Instant;

/// What a send or a delegation that may pay came to: the peer's answer, or
/// its task — as the peer sent it — asking to be paid first, with the
/// connection it asked on.
pub enum Paying<T> {
    Answered(T),
    Asked(Box<PaidPeer>, Value),
}

/// A peer's task that asked to be paid, on the connection it asked on.
pub struct PaidPeer {
    conn: HttpConn,
    task: String,
    context: Option<String>,
    /// The status message that asked: a later one that asks again is
    /// another message.
    asking: Option<String>,
    /// How many receipts the task carried when it asked: the ones after
    /// them are this payment's.
    receipts: usize,
}

/// How far a payment message got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reached {
    /// It never left: the peer could not be reached, or no credential was.
    Unsent(String),
    /// The peer answered that it did not take it: an HTTP 4xx, or a JSON-RPC
    /// error other than an internal one.
    Refused(String),
    /// It left and no readable answer came, or the peer failed inside: the
    /// peer may have acted on it.
    Unanswered(String),
    /// The peer took it.
    Accepted,
}

/// What a payment came to.
pub struct PaidAnswer {
    pub reached: Reached,
    /// The task as last read.
    pub task: Option<Value>,
    /// This payment's settlement response, as sent, once a status message
    /// carried one.
    pub receipt: Option<Value>,
    /// The task asked to be paid again: the peer did not take this payment.
    pub asked_again: bool,
    /// For a delegation, what the task ended as; `None` when the following
    /// stopped before it ended.
    pub outcome: Option<DelegateOutcome>,
}

/// [`super::send`] for a step that may pay: a2a-x402 activated, and a reply
/// that is a task asking to be paid handed back.
pub fn send_paying(
    endpoint: &A2aEndpoint,
    auth: PeerAuth,
    parts: &Value,
    context: Option<&str>,
    message_id: Option<&str>,
    deadline: Instant,
) -> Paying<Result<Value, String>> {
    let ep = match HttpEp::of(endpoint) {
        Ok(ep) => ep,
        Err(e) => return Paying::Answered(Err(e)),
    };
    let mut conn = HttpConn::new(ep, auth);
    conn.offered = x402::EXTENSIONS.iter().map(|u| u.to_string()).collect();
    let message_id = message_id
        .map(str::to_string)
        .unwrap_or_else(mint_message_id);
    let params = match a2a::send_message_params_parts(parts, context, &message_id, None) {
        Ok(p) => p,
        Err(e) => return Paying::Answered(Err(e)),
    };
    use super::Caller as _;
    match conn.call("SendMessage", params, deadline) {
        Ok(reply) if x402::asks(&reply) => {
            let task = reply.get("task").cloned().unwrap_or(reply);
            Paying::Asked(Box::new(PaidPeer::new(conn, &task)), task)
        }
        other => Paying::Answered(other),
    }
}

/// [`super::delegate`] for a step that may pay: a2a-x402 activated, and a
/// task asking to be paid handed back.
#[allow(clippy::too_many_arguments)]
pub fn delegate_paying(
    endpoint: &A2aEndpoint,
    auth: PeerAuth,
    objective: &str,
    command: Option<&Value>,
    output_contract: Option<&str>,
    message_id: Option<&str>,
    deadline: Instant,
    follow: Follow<'_>,
) -> Paying<DelegateOutcome> {
    match delegate_with(
        endpoint,
        auth,
        x402::EXTENSIONS,
        objective,
        command,
        output_contract,
        message_id,
        deadline,
        &follow,
    ) {
        (Some(conn), Followed::Asked(task)) => {
            Paying::Asked(Box::new(PaidPeer::new(conn, &task)), task)
        }
        (None, Followed::Asked(task)) => Paying::Answered(DelegateOutcome::Error(
            x402::unpaid_error(&a2a::task_id_of(&task)),
        )),
        (_, Followed::Done(outcome)) => Paying::Answered(outcome),
    }
}

impl PaidPeer {
    fn new(conn: HttpConn, task: &Value) -> PaidPeer {
        PaidPeer {
            conn,
            task: a2a::task_id_of(task),
            context: task
                .get("contextId")
                .and_then(Value::as_str)
                .map(str::to_string),
            asking: asking_message(task),
            receipts: receipts(task).len(),
        }
    }

    /// The peer's task.
    pub fn task_id(&self) -> &str {
        &self.task
    }

    /// The peer's endpoint, as a line names it: no userinfo, no query.
    pub fn endpoint(&self) -> String {
        crate::config::settings::url_locator(&self.conn.ep.url)
    }

    /// Answer the task with `payload`, then follow it until this payment's
    /// receipt is in hand — or, `to_end`, until the task ends — or until
    /// `deadline`. A poll that fails is tried again until then: the payment
    /// has left, and only the peer's word says what became of it.
    pub fn pay(&mut self, payload: &Value, to_end: bool, deadline: Instant) -> PaidAnswer {
        let mut answer = PaidAnswer {
            reached: Reached::Accepted,
            task: None,
            receipt: None,
            asked_again: false,
            outcome: None,
        };
        let tenant = self.conn.tenant.clone();
        let params = a2a::send_message_params_on_task(
            &self.task,
            self.context.as_deref(),
            "Payment authorization (x402).",
            x402::submitted(payload),
            x402::EXTENSIONS,
            &mint_message_id(),
            tenant.as_deref(),
        );
        let reply = match self.send(params, deadline) {
            Ok(v) => v,
            Err(reached) => {
                answer.reached = reached;
                return answer;
            }
        };
        // A message is a whole answer: the peer tracked no work past it.
        if let Some(a2a::Reply::Message(_)) = a2a::reply_of(&reply) {
            answer.receipt = reply
                .pointer("/message/metadata")
                .and_then(|m| m.get(x402::RECEIPTS))
                .and_then(Value::as_array)
                .and_then(|r| r.last())
                .cloned();
            answer.outcome = Some(DelegateOutcome::Distillate(a2a::reply_text_of(&reply)));
            return answer;
        }
        let get = a2a::get_task_params_without_history(&self.task, tenant.as_deref());
        let mut seen = reply.get("task").cloned().unwrap_or(reply);
        loop {
            let r = receipts(&seen);
            if r.len() > self.receipts {
                answer.receipt = r.last().cloned();
            }
            if x402::asks(&seen) && asking_message(&seen) != self.asking {
                answer.asked_again = true;
                answer.task = Some(seen);
                return answer;
            }
            if let Some(outcome) = terminal_outcome(&seen) {
                answer.outcome = Some(outcome);
                answer.task = Some(seen);
                return answer;
            }
            if (!to_end && answer.receipt.is_some()) || Instant::now() >= deadline {
                answer.task = Some(seen);
                return answer;
            }
            std::thread::sleep(
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
            use super::Caller as _;
            if let Ok(t) = self.conn.call("GetTask", get.clone(), deadline) {
                seen = t;
            }
        }
    }

    /// Tell the peer agentd will not pay what its task asked: one
    /// `SendMessage` on the task and in its context, its metadata
    /// `payment-rejected` (a2a-x402 v0.2 §5.4) and nothing else, marked with
    /// the extension, on the connection that asked. Only that the peer took
    /// it is read back; `Err` says why it did not.
    pub fn reject(&mut self, deadline: Instant) -> Result<(), String> {
        let tenant = self.conn.tenant.clone();
        let params = a2a::send_message_params_on_task(
            &self.task,
            self.context.as_deref(),
            "Payment requirements rejected (x402).",
            x402::rejected(),
            x402::EXTENSIONS,
            &mint_message_id(),
            tenant.as_deref(),
        );
        match self.send(params, deadline) {
            Ok(_) | Err(Reached::Accepted) => Ok(()),
            Err(Reached::Unsent(e) | Reached::Refused(e) | Reached::Unanswered(e)) => Err(e),
        }
    }

    /// The paying `SendMessage`: its result, or how far it got.
    fn send(&mut self, params: Value, deadline: Instant) -> Result<Value, Reached> {
        let conn = &mut self.conn;
        let id = conn.next_id;
        conn.next_id += 1;
        let timeout = request_timeout(deadline);
        let marked = a2a::extensions_of(&params);
        let req = Request::new(Id::Num(id), "SendMessage", Some(params));
        let body = serde_json::to_vec(&req)
            .map_err(|e| Reached::Unsent(format!("a2a: encode SendMessage: {e}")))?;
        let owned = conn
            .request_headers("POST", &conn.ep.path, &body, &marked)
            .map_err(Reached::Unsent)?;
        let mut stream = conn.connect(timeout).map_err(Reached::Unsent)?;
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        headers.extend(owned.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let resp = crate::net::http::send(
            &mut *stream,
            &conn.ep.host_header,
            "POST",
            &conn.ep.path,
            &headers,
            &body,
        )
        .map_err(|e| Reached::Unanswered(transport_error("SendMessage", &e)))?;
        let status = format!("a2a: SendMessage HTTP {}", resp.status);
        if resp.status >= 500 {
            return Err(Reached::Unanswered(status));
        }
        if !resp.is_success() {
            return Err(Reached::Refused(status));
        }
        let response: ::mcp::rpc::Response = serde_json::from_slice(&resp.body)
            .map_err(|e| Reached::Unanswered(format!("a2a: SendMessage bad reply: {e}")))?;
        if let Some(err) = response.error {
            let why = format!("a2a: SendMessage rpc error {}: {}", err.code, err.message);
            return Err(if err.code == ::mcp::rpc::INTERNAL_ERROR {
                Reached::Unanswered(why)
            } else {
                Reached::Refused(why)
            });
        }
        Ok(response.result.unwrap_or(Value::Null))
    }
}

/// The id of the task's status message, as sent.
fn asking_message(task: &Value) -> Option<String> {
    task.get("task")
        .unwrap_or(task)
        .pointer("/status/message/messageId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The receipts the task's status message carries, as sent.
fn receipts(task: &Value) -> Vec<Value> {
    x402::status_metadata(task)
        .and_then(|m| m.get(x402::RECEIPTS))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}
