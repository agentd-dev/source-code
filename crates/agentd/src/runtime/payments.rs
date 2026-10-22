// SPDX-License-Identifier: AGPL-3.0-only
//! A paying step's x402 payment, as the runtime makes it (RFC 0049 §3): the
//! payment request read, the spend policy asked, the approver's gate, the
//! ledger record and the audit chain written before the paid call leaves,
//! and the outcome handed to the step. What is signed and how is
//! `crate::payments`'; this module decides when, on the loop.
//!
//! **Transports.** A step's call goes out as it is; when it is answered
//! with a payment request, its transport hands the loop the
//! `PaymentRequired` and the call itself, as a [`PaidCall`], through
//! [`Runtime::pay_for`] — the one entry point. The `http` step's 402 is
//! unit 1's ([`Asked`]); the MCP and A2A transports of unit 2 call the same
//! entry with their own [`PaidCall`]. Nothing below asks which transport it
//! pays on.
//!
//! **The exchange.** The loop holds the request to the step and the policy
//! — the resource it names must be the call's, the definition the
//! operator's own, the request not paid already, an entry allowed — and
//! either refuses unsent, asks the approver, or pays: the ledger record
//! (`sending`) and the audit record (`authorized`) are on the disk before a
//! thread of its own signs and makes the call once more with the payment.
//! Its outcome ([`Paid`]) is recorded whatever became of the run, then
//! finishes the step if it still waits.
//!
//! **Never twice.** A step whose request has a `settled` or `unknown`
//! record is never paid again: [`Runtime::payment_precheck`] fails a
//! re-execution — a `retry:`, a replay after a crash — before anything is
//! sent. A restart reads a record still `sending` as `unknown`.
//!
//! **The approver's gate** is an A2A task of its own, owned by the approver
//! (`security.payments.approver`: `operator`, or one principal id), its text
//! agentd's, the server's words in it quoted and escaped. Only the approver
//! answers — an operator who is not the approver is refused, with no
//! override — no judge ever does, and unanswered it declines. The approval
//! covers the entry it showed, once: the policy is asked again when it
//! lands. A gate a previous life left open ends its step at the next start,
//! nothing paid.

use serde_json::{Value, json};

use super::reactor::Runtime;
use super::tools::ToolCaller;
use crate::engine::run::StepStatus;
use crate::payments::ledger::{Record, State};
use crate::payments::policy::{Choice, StepPayment};
use crate::payments::scheme;
use crate::payments::x402::{self, Required, Settlement};
use crate::state::now_ms;

/// The wait kind of a step whose payment waits for the approver.
pub(crate) const WAIT: &str = "payment_approval";
/// How often the ledger forgets what no cap looks back at.
const SWEEP_EVERY_MS: u64 = 3600 * 1000;
/// How many closed gates' tasks are remembered, to refuse a late answer.
const CLOSED_KEPT: usize = 256;
/// The longest stretch of a server's words, or of a value a step chose, a
/// gate shows.
pub(super) const SAID_SHOWN: usize = 300;

/// A paying step's call, as its transport made it: what a payment for it is
/// held to, and how it is made once more carrying the payment. The `http`
/// step's request is one; unit 2's MCP tool call and A2A message are the
/// others.
pub(crate) trait PaidCall: Send {
    /// Whether `url`, a `PaymentRequired`'s `resource.url`, names this
    /// call's resource: a payment for another resource is never made.
    fn is_resource(&self, url: &str) -> bool;
    /// The resource as a record, a log line and the approver's gate name
    /// it: nothing the step's author would not log (an HTTP query is left
    /// out).
    fn shown(&self) -> String;
    /// How the approver reads the call — `"GET https://…"` — every value in
    /// it that the step or the server chose quoted and escaped
    /// (`sec::render::quoted`): a URL a run templated, a tool's name, a
    /// peer's task id. None of it reads as agentd's own words.
    fn describe(&self) -> String;
    /// Make the call once more, carrying `payload` — the `PaymentPayload`,
    /// which the transport puts where it carries one — and read what came
    /// back. On a thread of its own: it may take the call's timeout.
    fn send_paid(&self, payload: &Value, run: &str, step: &str) -> Answer;
}

/// What came back from a paid call.
pub(crate) enum Answer {
    /// The server answered.
    Answered {
        /// The answer, as the step's output.
        output: Value,
        /// The settlement it carried, read; `Err` when what it carried is
        /// none.
        settlement: Option<Result<Settlement, String>>,
        /// It said, with no settlement, that it took no payment (on HTTP a
        /// 400 or a 402).
        declined: bool,
        /// What the step makes of the answer when it is not one it expects.
        error: Option<String>,
    },
    /// No answer, and whether the payment may have reached the server.
    Unanswered { why: String, may_have_sent: bool },
}

/// A 402 an `http` step that may pay was answered with.
pub struct Asked {
    pub(crate) run: String,
    pub(crate) step: String,
    /// The `PAYMENT-REQUIRED` header's value.
    pub(crate) header: String,
    /// The 402 response, as a step's output.
    pub(crate) first: Value,
    pub(crate) request: super::http_node::Request,
}

/// Named by its step alone: a request's headers carry its credentials.
impl std::fmt::Debug for Asked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Asked({}/{})", self.run, self.step)
    }
}

/// A paid call's outcome.
pub struct Paid {
    pub(crate) run: String,
    pub(crate) step: String,
    /// The paid request: the step's idempotency key, and which payment of
    /// it this was.
    pub(crate) key: String,
    pub(crate) attempt: u32,
    pub(crate) sent: Sent,
    /// The answer to the paid call, as a step's output.
    pub(crate) response: Option<Value>,
    /// What the step makes of that answer, when it is not one it expects.
    pub(crate) judged: Option<String>,
}

impl std::fmt::Debug for Paid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Paid({}/{})", self.run, self.step)
    }
}

/// What became of a paid call.
pub(crate) enum Sent {
    Settled(Settlement),
    /// The server said it did not settle.
    Refused(Option<Settlement>, String),
    /// What landed is not known.
    Unknown(Option<Settlement>, String),
    /// Nothing left agentd.
    Unsent(String),
}

/// A payment chosen and on its way: what the approver is asked about, and
/// what is paid.
pub(crate) struct Pending {
    pub run: String,
    pub step: String,
    /// The paid request: the step's idempotency key.
    pub key: String,
    pub choice: Choice,
    pub required: Required,
    pub call: Box<dyn PaidCall>,
    pub step_payment: StepPayment,
}

/// A payment waiting for the approver.
pub(crate) struct Gate {
    pub pending: Pending,
    /// The gate's task.
    pub task: String,
    pub until_ms: u64,
    pub trace: Option<crate::obs::trace::TraceContext>,
}

/// The shape the approver's answer takes.
#[cfg(feature = "a2a")]
fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"decision": {"type": "string", "enum": ["approve", "decline"]}},
        "required": ["decision"],
    })
}

/// What becomes of a paid call, from what came back: settled, refused,
/// unknown or unsent (RFC 0049 D4) — the answer, and what the step makes of
/// it.
fn outcome(answer: Answer) -> (Sent, Option<Value>, Option<String>) {
    match answer {
        Answer::Unanswered {
            why,
            may_have_sent: false,
        } => (Sent::Unsent(why), None, None),
        Answer::Unanswered { why, .. } => {
            (Sent::Unknown(None, format!("no answer: {why}")), None, None)
        }
        Answer::Answered {
            output,
            settlement,
            declined,
            error,
        } => {
            let sent = match settlement {
                Some(Ok(s)) if s.pending() => {
                    let why = format!(
                        "the server says settlement is {} (transaction {})",
                        s.error_reason.as_deref().unwrap_or("not confirmed"),
                        if s.transaction.is_empty() {
                            "none"
                        } else {
                            &s.transaction
                        }
                    );
                    Sent::Unknown(Some(s), why)
                }
                Some(Ok(s)) if s.success => Sent::Settled(s),
                Some(Ok(s)) => {
                    let why = format!(
                        "the server did not settle it ({})",
                        s.error_reason.as_deref().unwrap_or("no reason given")
                    );
                    Sent::Refused(Some(s), why)
                }
                Some(Err(e)) => Sent::Unknown(None, format!("a settlement that is not one ({e})")),
                None if declined => Sent::Refused(
                    None,
                    "the server answered that it took no payment, and settled nothing".into(),
                ),
                None => Sent::Unknown(
                    None,
                    "the server answered with no settlement, so whether it settled is not known"
                        .into(),
                ),
            };
            (sent, Some(output), error)
        }
    }
}

impl Runtime {
    /// Before a paying step sends anything: the definition is the
    /// operator's own, the policy is there, and its request was not paid
    /// already — or may have been.
    pub(crate) fn payment_precheck(&self, run_id: &str, step_id: &str) -> Result<(), String> {
        let Some(p) = &self.payments else {
            return Err("payment needs security.payments, the spend policy it pays within".into());
        };
        let wf = self
            .definition_for_run(run_id)
            .ok_or("payment: the run's definition is gone")?;
        if !wf.configured || wf.document {
            return Err(crate::config::settings::payments::not_configured(
                &wf.name, step_id,
            ));
        }
        let key = crate::engine::run::idempotency_key(run_id, step_id);
        if let Some(r) = p.ledger().blocking(&key) {
            return Err(never_twice(r));
        }
        Ok(())
    }

    /// The caller a step of `run_id` acts as.
    fn step_caller(&self, run_id: &str, step_id: &str) -> ToolCaller {
        let r = self.runs.get(run_id);
        ToolCaller {
            run: Some(run_id.to_string()),
            step: Some(step_id.to_string()),
            req: r
                .and_then(|r| r.steps.get(step_id))
                .map_or(0, |s| u64::from(s.attempt)),
            principal: r.and_then(|r| r.principal.clone()),
            ctx: r.and_then(|r| r.conversation.clone()),
            msg_depth: r.map_or(0, |r| r.msg_depth),
            session: r.and_then(|r| r.session.clone()),
            ..Default::default()
        }
    }

    /// Whether the step still waits on what this module does for it.
    fn step_waiting(&self, run_id: &str, step_id: &str) -> bool {
        self.runs.get(run_id).is_some_and(|r| {
            !r.status.is_terminal()
                && r.steps.get(step_id).is_some_and(|s| {
                    matches!(s.status, StepStatus::Running | StepStatus::Suspended)
                })
        })
    }

    /// What every record of a payment for `call` names before it is made.
    fn payment_target(&self, run: &str, step: &str, call: &dyn PaidCall) -> Value {
        let wf = self
            .runs
            .get(run)
            .map(|r| r.workflow.clone())
            .unwrap_or_default();
        json!({"run": run, "step": step, "workflow": wf, "url": call.shown()})
    }

    /// A payment refused unsent: the step fails saying why, and the chain
    /// records it.
    fn payment_refused(
        &mut self,
        run_id: &str,
        step_id: &str,
        first: Option<Value>,
        why: String,
        target: Value,
    ) {
        let trace = self.step_trace(run_id, step_id);
        self.step_log(run_id, step_id).warn(
            "payment.refused",
            json!({"run": run_id, "step": step_id, "why": why, "detail": target}),
        );
        self.payment_audit("refused", target, None, trace.as_ref());
        self.executing.remove(&format!("{run_id}/{step_id}"));
        self.finish_step_pub(run_id, step_id, StepStatus::Failed, first, Some(why), 0);
    }

    /// A paying step refused before its call is made — its definition is
    /// not the operator's own, or its request was paid already, or may have
    /// been ([`Runtime::payment_precheck`]): the step fails, and the chain
    /// records it.
    pub(crate) fn payment_refused_before(&mut self, run: &str, step: &str, why: String) {
        let wf = self
            .runs
            .get(run)
            .map(|r| r.workflow.clone())
            .unwrap_or_default();
        let target = json!({"run": run, "step": step, "workflow": wf});
        self.payment_refused(run, step, None, why, target);
    }

    /// A 402 asking a paying `http` step to pay: its `PAYMENT-REQUIRED`
    /// read, then [`Runtime::pay_for`].
    pub(crate) fn on_payment_required(&mut self, a: Asked) {
        let Asked {
            run,
            step,
            header,
            first,
            request,
        } = a;
        let required = x402::decode_header(&header)
            .map_err(|e| format!("http status 402, and its PAYMENT-REQUIRED is not one ({e})"));
        self.pay_for(&run, &step, required, Box::new(request), first);
    }

    /// Pay for `required` — the `PaymentRequired` a paying step's `call`
    /// was answered with, as its transport carried it, or why what it
    /// carried is none — or refuse to. The one entry point of every
    /// transport (RFC 0049 §3): `first` is the answer that asked, the
    /// step's output when nothing is paid.
    pub(crate) fn pay_for(
        &mut self,
        run: &str,
        step: &str,
        required: Result<Value, String>,
        call: Box<dyn PaidCall>,
        mut first: Value,
    ) {
        if !self.step_waiting(run, step) {
            self.executing.remove(&format!("{run}/{step}"));
            return;
        }
        let target = self.payment_target(run, step, &*call);
        let required = match required.and_then(Required::parse) {
            Ok(r) => r,
            Err(e) => {
                let why = format!(
                    "payment refused unsent: no x402 v2 payment request ({e}), and nothing was paid"
                );
                self.payment_refused(run, step, Some(first), why, target);
                return;
            }
        };
        first["payment_required"] = required.raw.clone();
        self.step_log(run, step).info(
            "payment.required",
            json!({"run": run, "step": step, "call": call.describe(),
                   "resource": crate::config::settings::url_locator(&required.url),
                   "accepts": required.accepts}),
        );
        // A payment for this resource, never another's (RFC 0049 D3).
        if !call.is_resource(&required.url) {
            let why = format!(
                "payment refused unsent (resource_mismatch): the server asks to be paid for {}, not for what this step called",
                crate::config::settings::url_locator(&required.url)
            );
            let mut t = target;
            t["reasons"] = json!(["resource_mismatch"]);
            self.payment_refused(run, step, Some(first), why, t);
            return;
        }
        if let Err(why) = self.payment_precheck(run, step) {
            self.payment_refused(run, step, Some(first), why, target);
            return;
        }
        let Some(p) = self.payments.clone() else {
            return;
        };
        let field = self
            .definition_for_run(run)
            .and_then(|wf| {
                self.runs
                    .get(run)
                    .and_then(|r| self.resolve_step(&wf, r, step))
                    .and_then(|(st, _)| st.spec.get("payment").cloned())
            })
            .unwrap_or(Value::Null);
        let step_payment = match crate::config::settings::payments::step_payment(
            &self.settings.security.payments,
            &field,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.payment_refused(run, step, Some(first), e, target);
                return;
            }
        };
        let key = crate::engine::run::idempotency_key(run, step);
        let chosen = {
            let ledger = p.ledger();
            let now = now_ms();
            crate::payments::policy::choose(
                &p.policy,
                &step_payment,
                &required.accepts,
                &|asset, payee| ledger.spent(asset, run, payee, now),
            )
        };
        let choice = match chosen {
            Ok(c) => c,
            Err(passed) => {
                let reasons: Vec<Value> = passed.iter().map(|x| x.to_json()).collect();
                let why = format!(
                    "payment refused unsent: no entry the server offers is one this step and security.payments allow — {}",
                    passed
                        .iter()
                        .map(|x| format!("entry {}: {}", x.index, x.reasons.join("; ")))
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
                let mut t = target;
                t["reasons"] = json!(reasons);
                self.payment_refused(run, step, Some(first), why, t);
                return;
            }
        };
        // Who is asked, and why.
        let mut ask = Vec::new();
        if choice.above_threshold
            && let Some(a) = p.policy.assets.get(&choice.asset)
            && let Some(t) = a.approve_above
        {
            ask.push(format!(
                "{} is above approve_above {}",
                a.show(choice.amount),
                a.show(t)
            ));
        }
        if step_payment.approve {
            ask.push("the step asks for approval of every payment (approve: true)".into());
        }
        if p.policy.ask_when_tainted {
            let caller = self.step_caller(run, step);
            let facts = self.caller_facts(&caller);
            if facts.tainted {
                ask.push(format!(
                    "the run has read outside text ({})",
                    facts.sources.join("; ")
                ));
            }
            if self.model_asked(&caller) {
                ask.push("a model asked for this run".into());
            }
        }
        let pending = Pending {
            run: run.to_string(),
            step: step.to_string(),
            key,
            choice,
            required,
            call,
            step_payment,
        };
        if ask.is_empty() {
            self.pay(pending, None);
        } else {
            self.ask_approver(pending, ask);
        }
    }

    /// Reserve, record, sign and send: the ledger record and the audit
    /// record on the disk first, then a thread of its own.
    fn pay(&mut self, pending: Pending, approved_by: Option<String>) {
        let Pending {
            run,
            step,
            key,
            choice,
            required,
            call,
            step_payment,
        } = pending;
        let Some(p) = self.payments.clone() else {
            self.executing.remove(&format!("{run}/{step}"));
            return;
        };
        let target = self.payment_target(&run, &step, &*call);
        // The policy again, now: an approval does not outlast a cap another
        // payment reached while it was asked.
        if approved_by.is_some() {
            let again = {
                let ledger = p.ledger();
                let now = now_ms();
                crate::payments::policy::choose(
                    &p.policy,
                    &step_payment,
                    std::slice::from_ref(&choice.entry),
                    &|asset, payee| ledger.spent(asset, &run, payee, now),
                )
            };
            if let Err(passed) = again {
                let why = format!(
                    "payment refused unsent after its approval: {}",
                    passed[0].reasons.join("; ")
                );
                let mut t = target;
                t["reasons"] = json!(passed.iter().map(|x| x.to_json()).collect::<Vec<_>>());
                self.payment_refused(&run, &step, None, why, t);
                return;
            }
        }
        if let Some(r) = p.ledger().blocking(&key) {
            let why = never_twice(r);
            self.payment_refused(&run, &step, None, why, target);
            return;
        }
        // The nonce and the window are fixed here, on the loop, so the
        // record holds them before anything is signed.
        let prepared = match scheme::prepare(&choice, p.policy.max_validity, now_ms() / 1000) {
            Ok(x) => x,
            Err(e) => {
                self.payment_refused(
                    &run,
                    &step,
                    None,
                    format!("payment refused unsent: {e}"),
                    target,
                );
                return;
            }
        };
        let wf = self
            .runs
            .get(&run)
            .map(|r| r.workflow.clone())
            .unwrap_or_default();
        let attempt = p.ledger().next_attempt(&key);
        let record = Record {
            request: key.clone(),
            attempt,
            run: run.clone(),
            step: step.clone(),
            workflow: wf,
            wallet: step_payment.wallet.clone(),
            network: choice.network.clone(),
            asset: choice.asset.clone(),
            asset_id: choice.asset_id.clone(),
            payee: choice.payee.clone(),
            pay_to: choice.pay_to.to_string(),
            amount: choice.amount.to_string(),
            nonce: prepared.nonce.clone(),
            valid_before: prepared.valid_before,
            url: call.shown(),
            state: State::Sending,
            transaction: None,
            payer: None,
            error_reason: None,
            approved_by: approved_by.clone(),
            at_ms: now_ms(),
            ended_ms: None,
        };
        let mut t = record_target(&record);
        if let Err(e) = p.ledger().put(record.clone()) {
            t["error"] = json!(e);
            self.payment_refused(&run, &step, None, format!("payment refused unsent: {e}"), t);
            return;
        }
        let trace = self.step_trace(&run, &step);
        if let Err(e) = self.payment_audit_required(
            "authorized",
            t.clone(),
            approved_by.as_deref(),
            trace.as_ref(),
        ) {
            let mut unsent = record;
            unsent.state = State::Unsent;
            unsent.error_reason = Some(e.clone());
            unsent.ended_ms = Some(now_ms());
            let _ = p.ledger().put(unsent);
            self.payment_refused(
                &run,
                &step,
                None,
                format!("payment refused unsent: the audit chain did not take its record ({e})"),
                t,
            );
            return;
        }
        self.step_log(&run, &step).info(
            "payment.sent",
            json!({"run": run, "step": step, "wallet": step_payment.wallet, "network": choice.network,
                   "asset": choice.asset, "amount": choice.amount.to_string(), "payee": choice.payee,
                   "nonce": prepared.nonce, "valid_before": prepared.valid_before,
                   "attempt": attempt, "approved_by": approved_by}),
        );
        self.executing
            .insert(format!("{run}/{step}"), std::time::Instant::now());
        let tx = self.events_tx.clone();
        let wallet = step_payment.wallet;
        std::thread::Builder::new()
            .name("step:pay".into())
            .spawn(move || {
                let (sent, response, judged) =
                    match scheme::sign(&p.wallets, &wallet, &choice, &prepared) {
                        Err(e) => (Sent::Unsent(e.to_string()), None, None),
                        Ok(signed) => {
                            let payload = x402::payload(&required.raw, &choice.entry, signed);
                            outcome(call.send_paid(&payload, &run, &step))
                        }
                    };
                let _ = tx.send(super::events::Event::PaymentDone(Box::new(Paid {
                    run,
                    step,
                    key,
                    attempt,
                    sent,
                    response,
                    judged,
                })));
            })
            .ok();
    }

    /// A paid call's outcome: the ledger, the chain, the log, then the step
    /// if it still waits.
    pub(crate) fn on_payment_done(&mut self, done: Paid) {
        let Paid {
            run,
            step,
            key,
            attempt,
            sent,
            response,
            judged,
        } = done;
        self.executing.remove(&format!("{run}/{step}"));
        let Some(p) = self.payments.clone() else {
            return;
        };
        let Some(mut record) = p.ledger().get(&key, attempt).cloned() else {
            return;
        };
        let (state, outcome, settlement, why) = match &sent {
            Sent::Settled(s) => (State::Settled, "settled", Some(s.clone()), None),
            Sent::Refused(s, why) => (State::Refused, "failed", s.clone(), Some(why.clone())),
            Sent::Unknown(s, why) => (State::Unknown, "unknown", s.clone(), Some(why.clone())),
            Sent::Unsent(why) => (State::Unsent, "unsent", None, Some(why.clone())),
        };
        record.state = state;
        record.ended_ms = Some(now_ms());
        if let Some(s) = &settlement {
            record.transaction = Some(s.transaction.clone()).filter(|t| !t.is_empty());
            record.payer = s.payer.clone();
            record.error_reason = s.error_reason.clone();
        }
        if record.error_reason.is_none() {
            record.error_reason = why.clone();
        }
        let kept = p.ledger().put(record.clone());
        let mut t = record_target(&record);
        if let Err(e) = &kept {
            t["ledger_error"] = json!(e);
        }
        let trace = self.step_trace(&run, &step);
        self.payment_audit(outcome, t, record.approved_by.as_deref(), trace.as_ref());
        let line = self.step_log(&run, &step);
        let detail = json!({"run": run, "step": step, "asset": record.asset, "amount": record.amount,
                            "payee": record.payee, "nonce": record.nonce, "transaction": record.transaction,
                            "payer": record.payer, "error_reason": record.error_reason, "why": why});
        match state {
            State::Settled => line.info("payment.settled", detail),
            State::Unknown => line.warn("payment.unknown", detail),
            _ => line.warn("payment.failed", detail),
        }
        if !self.step_waiting(&run, &step) {
            return;
        }
        let payment = json!({
            "state": state.as_str(), "wallet": record.wallet, "network": record.network,
            "asset": record.asset, "amount": record.amount, "payee": record.payee,
            "pay_to": record.pay_to, "nonce": record.nonce, "transaction": record.transaction,
            "payer": record.payer, "error_reason": record.error_reason,
        });
        let mut output = response.unwrap_or(Value::Null);
        if !output.is_object() {
            output = json!({});
        }
        output["payment"] = payment;
        let error = match &sent {
            Sent::Settled(_) => judged.map(|e| {
                format!(
                    "{e}: the payment settled ({}), and the answer is not one the step expects — the request is never paid again",
                    record.transaction.as_deref().unwrap_or("no transaction given")
                )
            }),
            Sent::Refused(_, why) => Some(format!("payment failed: {why}")),
            Sent::Unknown(_, why) => Some(format!(
                "payment outcome unknown ({why}): agentd never makes it again"
            )),
            Sent::Unsent(why) => Some(format!("payment not sent: {why}")),
        };
        let status = if error.is_some() {
            StepStatus::Failed
        } else {
            StepStatus::Done
        };
        self.finish_step_pub(&run, &step, status, Some(output), error, 0);
    }

    /// Ask the approver about `pending`, on a task of their own; refused
    /// unsent when nobody can be asked.
    fn ask_approver(&mut self, pending: Pending, why: Vec<String>) {
        #[cfg(feature = "a2a")]
        if self.a2a_sink.is_some() {
            self.open_payment_gate(pending, why);
            return;
        }
        let mut t = self.payment_target(&pending.run, &pending.step, &*pending.call);
        t["asked_because"] = json!(why);
        t["reasons"] = json!(["no_approver"]);
        self.payment_refused(
            &pending.run,
            &pending.step,
            None,
            format!(
                "payment refused unsent (no_approver): it needs the approver's approval ({}), and no A2A listener serves to ask them",
                why.join("; ")
            ),
            t,
        );
    }

    /// The gate: a task owned by the approver, in `input-required`, agentd's
    /// words; the step suspended on it.
    #[cfg(feature = "a2a")]
    fn open_payment_gate(&mut self, pending: Pending, why: Vec<String>) {
        use crate::a2a::tasks::{Link, State as TaskState};
        let Some(p) = self.payments.clone() else {
            return;
        };
        let approver = p.policy.approver.clone();
        // A gate's id names its wait: with none, nobody can be asked.
        let id = match crate::sec::random::b64url_token(9) {
            Ok(id) => id,
            Err(e) => {
                let mut t = self.payment_target(&pending.run, &pending.step, &*pending.call);
                t["asked_because"] = json!(why);
                t["reasons"] = json!(["no_approver"]);
                self.payment_refused(
                    &pending.run,
                    &pending.step,
                    None,
                    format!(
                        "payment refused unsent (no_approver): it needs the approver's approval ({}), and no gate could be opened to ask them (the system's random source: {e})",
                        why.join("; ")
                    ),
                    t,
                );
                return;
            }
        };
        let role = match approver.split_once(':').map(|(r, _)| r) {
            Some("user") => crate::config::settings::Role::User,
            Some("agent") => crate::config::settings::Role::Agent,
            _ => crate::config::settings::Role::Operator,
        };
        let owner = crate::a2a::Principal {
            id: approver.clone(),
            role,
            grants: Vec::new(),
            rate: None,
            budget: None,
            labels: Default::default(),
            session: None,
            typed: None,
        };
        // A freshly minted id, never the listener's reservation: that is
        // the task the request being served creates.
        let reserved = self.reserved_task_id.take();
        let ctx = format!("payment-{id}");
        let task = self.task_create(&ctx, &owner, Link::Turn { ctx: ctx.clone() }, None);
        self.reserved_task_id = reserved;
        let (run, step) = (pending.run.clone(), pending.step.clone());
        let wf = self
            .runs
            .get(&run)
            .map(|r| r.workflow.clone())
            .unwrap_or_default();
        let question = gate_text(
            &p.policy,
            &pending,
            p.wallets.known(&pending.step_payment.wallet),
            &wf,
            &why,
        );
        let trace = self.step_trace(&run, &step);
        if let Some(t) = self.tasks.get_mut(&task) {
            t.ask_schema = Some(answer_schema());
            t.security_gate = true;
            t.ask_channel = None;
            t.transition(TaskState::InputRequired, Some(question));
            if let Some(tr) = &trace {
                t.set_trace(tr.clone());
            }
        }
        self.task_persist(&task);
        self.task_sync(&task);
        let until_ms = now_ms() + p.policy.approval_timeout.as_millis() as u64;
        self.executing.remove(&format!("{run}/{step}"));
        self.suspend_wait(
            &run,
            &step,
            json!({"kind": WAIT, "gate": id, "task": task, "until_ms": until_ms}),
        );
        let c = &pending.choice;
        let t = json!({"run": run, "step": step, "workflow": wf, "task": task, "approver": approver,
                       "asset": c.asset, "amount": c.amount.to_string(), "payee": c.payee,
                       "network": c.network, "url": pending.call.shown(), "asked_because": why});
        self.step_log(&run, &step).info("payment.asked", t.clone());
        self.payment_audit("asked", t, None, trace.as_ref());
        self.payment_gates.insert(
            id,
            Gate {
                pending,
                task,
                until_ms,
                trace,
            },
        );
    }

    /// The gate open on `task`, if one is.
    #[cfg(feature = "a2a")]
    pub(crate) fn payment_gate_on(&self, task: &str) -> Option<String> {
        self.payment_gates
            .iter()
            .find(|(_, g)| g.task == task)
            .map(|(id, _)| id.clone())
    }

    /// Whether `task` was a payment gate's, now closed.
    #[cfg(feature = "a2a")]
    pub(crate) fn payment_gate_closed(&self, task: &str) -> bool {
        self.payment_closed.iter().any(|t| t == task)
    }

    /// An answer on the gate `id`'s task: the approver's `approve` pays,
    /// their `decline` refuses; anyone else is refused and the gate stays.
    #[cfg(feature = "a2a")]
    pub(crate) fn payment_answer(
        &mut self,
        id: &str,
        task: &str,
        principal: &crate::a2a::Principal,
        text: &str,
        message: &Value,
    ) -> Value {
        use super::a2a_server::err_obj;
        let Some(p) = self.payments.clone() else {
            return err_obj(::mcp::rpc::INVALID_PARAMS, "this approval has ended");
        };
        let approver = p.policy.approver.clone();
        let theirs = if approver == crate::config::settings::payments::OPERATOR {
            principal.is_operator()
        } else {
            principal.id == approver
        };
        let Some(g) = self.payment_gates.get(id) else {
            return err_obj(::mcp::rpc::INVALID_PARAMS, "this approval has ended");
        };
        let trace = g.trace.clone();
        let c = &g.pending.choice;
        let base = json!({"run": g.pending.run, "step": g.pending.step, "task": task, "approver": approver,
                          "asset": c.asset, "amount": c.amount.to_string(), "payee": c.payee});
        if !theirs {
            self.log.in_trace(trace.as_ref()).info(
                "human.answer.not_addressed",
                json!({"task": task, "from": principal.id, "addressee": approver}),
            );
            let mut t = base;
            t["by"] = json!(principal.id);
            self.payment_audit("not_addressed", t, Some(&principal.id), trace.as_ref());
            return err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!(
                    "this payment's approval is for {approver}; your answer was not recorded and the gate is still open"
                ),
            );
        }
        let shaped = super::human::shape_answer(text, &answer_schema());
        let decision = shaped["decision"].as_str().unwrap_or_default().to_string();
        if decision != "approve" && decision != "decline" {
            return err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "answer approve or decline; nothing else answers a payment's approval, and the gate is still open",
            );
        }
        let Some(g) = self.payment_gates.remove(id) else {
            return err_obj(::mcp::rpc::INVALID_PARAMS, "this approval has ended");
        };
        if let Some(t) = self.tasks.get_mut(task) {
            t.record_inbound(message);
        }
        let approved = decision == "approve";
        let note = if approved {
            format!("approved by {}", principal.id)
        } else {
            format!("declined by {}; nothing was paid", principal.id)
        };
        self.close_payment_task(task, crate::a2a::tasks::State::Completed, &note);
        let mut t = base;
        t["by"] = json!(principal.id);
        if let Some(sid) = &principal.session {
            t["sid"] = json!(sid);
        }
        let outcome = if approved { "approved" } else { "declined" };
        self.log
            .in_trace(trace.as_ref())
            .info(&format!("payment.{outcome}"), t.clone());
        self.payment_audit(outcome, t, Some(&principal.id), trace.as_ref());
        let (run, step) = (g.pending.run.clone(), g.pending.step.clone());
        if !self.step_waiting(&run, &step) {
            return self.task_reply(task);
        }
        if let Some(r) = self.runs.get_mut(&run)
            && let Some(st) = r.steps.get_mut(&step)
        {
            st.status = StepStatus::Running;
            st.wait = None;
            r.touch();
        }
        if approved {
            self.pay(g.pending, Some(principal.id.clone()));
        } else {
            self.finish_step_pub(
                &run,
                &step,
                StepStatus::Failed,
                None,
                Some(format!(
                    "payment declined by {}: nothing was paid",
                    principal.id
                )),
                0,
            );
        }
        self.task_reply(task)
    }

    /// Close a gate's task, and remember it closed.
    fn close_payment_task(&mut self, task: &str, state: crate::a2a::tasks::State, note: &str) {
        #[cfg(feature = "a2a")]
        {
            if let Some(t) = self.tasks.get_mut(task) {
                t.ask_schema = None;
                t.security_gate = false;
                if !t.state.is_terminal() {
                    t.transition(state, Some(note.to_string()));
                }
            }
            self.task_persist(task);
            self.task_sync(task);
        }
        #[cfg(not(feature = "a2a"))]
        let _ = (state, note);
        self.payment_closed.push_back(task.to_string());
        while self.payment_closed.len() > CLOSED_KEPT {
            self.payment_closed.pop_front();
        }
    }

    /// The per-tick pass: a gate past its deadline declines, a gate whose
    /// step went another way closes, a step a previous life left waiting
    /// on a gate ends, and the ledger forgets, hourly, what no cap needs.
    pub(crate) fn payments_tick(&mut self) {
        if self.payments.is_none() {
            return;
        }
        let now = now_ms();
        let ended: Vec<(String, bool)> = self
            .payment_gates
            .iter()
            .filter_map(|(id, g)| {
                let waiting = self.runs.get(&g.pending.run).is_some_and(|r| {
                    !r.status.is_terminal()
                        && r.steps.get(&g.pending.step).is_some_and(|s| {
                            s.status == StepStatus::Suspended
                                && s.wait.as_ref().is_some_and(|w| w["gate"] == id.as_str())
                        })
                });
                if !waiting {
                    Some((id.clone(), false))
                } else if now >= g.until_ms {
                    Some((id.clone(), true))
                } else {
                    None
                }
            })
            .collect();
        for (id, timed_out) in ended {
            let Some(g) = self.payment_gates.remove(&id) else {
                continue;
            };
            let (run, step) = (g.pending.run.clone(), g.pending.step.clone());
            let t = json!({"run": run, "step": step, "task": g.task, "asset": g.pending.choice.asset,
                           "amount": g.pending.choice.amount.to_string(), "payee": g.pending.choice.payee});
            if timed_out {
                self.close_payment_task(
                    &g.task,
                    crate::a2a::tasks::State::Failed,
                    "no answer in time: declined, and nothing was paid",
                );
                self.step_log(&run, &step).info(
                    "payment.declined",
                    json!({"run": run, "step": step, "task": g.task, "by": "on_timeout"}),
                );
                let mut a = t;
                a["by"] = json!("on_timeout");
                self.payment_audit("declined", a, None, g.trace.as_ref());
                self.finish_step_pub(
                    &run,
                    &step,
                    StepStatus::Failed,
                    None,
                    Some("payment declined: the approver did not answer in time (security.payments.approval_timeout); nothing was paid".into()),
                    0,
                );
            } else {
                self.close_payment_task(
                    &g.task,
                    crate::a2a::tasks::State::Canceled,
                    "the step that asked went another way; nothing was paid",
                );
                self.payment_audit("withdrawn", t, None, g.trace.as_ref());
            }
        }
        // A gate this process does not hold was a previous life's.
        let mut interrupted = Vec::new();
        for (rid, r) in &self.runs {
            if r.status.is_terminal() {
                continue;
            }
            for (sid, st) in &r.steps {
                if st.status == StepStatus::Suspended
                    && let Some(w) = &st.wait
                    && w["kind"] == WAIT
                    && !self
                        .payment_gates
                        .contains_key(w["gate"].as_str().unwrap_or_default())
                {
                    interrupted.push((
                        rid.clone(),
                        sid.clone(),
                        w["task"].as_str().unwrap_or_default().to_string(),
                    ));
                }
            }
        }
        for (run, step, task) in interrupted {
            self.close_payment_task(
                &task,
                crate::a2a::tasks::State::Failed,
                "agentd restarted while this was asked: nothing was paid",
            );
            self.step_log(&run, &step).info(
                "payment.interrupted",
                json!({"run": run, "step": step, "task": task}),
            );
            let wf = self
                .runs
                .get(&run)
                .map(|r| r.workflow.clone())
                .unwrap_or_default();
            let trace = self.step_trace(&run, &step);
            self.payment_audit(
                "interrupted",
                json!({"run": run, "step": step, "workflow": wf, "task": task}),
                None,
                trace.as_ref(),
            );
            self.finish_step_pub(
                &run,
                &step,
                StepStatus::Failed,
                None,
                Some("payment interrupted: agentd restarted while its approval was asked, and nothing was paid; a retry asks again".into()),
                0,
            );
        }
        if now.saturating_sub(self.payments_swept_ms) >= SWEEP_EVERY_MS {
            self.payments_swept_ms = now;
            let live: Vec<String> = self
                .runs
                .iter()
                .filter(|(_, r)| !r.status.is_terminal())
                .map(|(id, _)| id.clone())
                .collect();
            if let Some(p) = &self.payments {
                let n = p.ledger().sweep(now, &|run| live.iter().any(|l| l == run));
                if n > 0 {
                    self.log.info("payment.ledger.swept", json!({"records": n}));
                }
            }
        }
    }

    /// The payments the last life left out, made `unknown` at start
    /// ([`crate::payments::Payments::build`]): each an `unknown` record in
    /// the chain, so the chain says what became of every `authorized` one.
    pub(crate) fn payments_restored(&self, restored: Vec<Record>) {
        for r in restored {
            let trace = self.step_trace(&r.run, &r.step);
            self.payment_audit(
                "unknown",
                record_target(&r),
                r.approved_by.as_deref(),
                trace.as_ref(),
            );
        }
    }

    /// A `payment` audit record: chained, and flushed to the disk before
    /// this returns, as every payment record is.
    fn payment_audit(
        &self,
        outcome: &str,
        target: Value,
        principal: Option<&str>,
        trace: Option<&crate::obs::trace::TraceContext>,
    ) {
        self.audit(super::audit::AuditEvent {
            action: "payment",
            target,
            outcome,
            principal,
            role: None,
            request_id: None,
            sid: None,
            trace,
            link: None,
        });
    }

    /// [`Self::payment_audit`] of a record the payment may not go on
    /// without: the chain's refusal is the caller's.
    fn payment_audit_required(
        &self,
        outcome: &str,
        target: Value,
        principal: Option<&str>,
        trace: Option<&crate::obs::trace::TraceContext>,
    ) -> Result<(), String> {
        self.audit_required(super::audit::AuditEvent {
            action: "payment",
            target,
            outcome,
            principal,
            role: None,
            request_id: None,
            sid: None,
            trace,
            link: None,
        })
    }
}

/// Why a request with this record is never paid again.
fn never_twice(r: &Record) -> String {
    match r.state {
        State::Settled => format!(
            "payment_made: this step's request was paid already ({} of {} to {}, transaction {}) — agentd never pays a request twice",
            r.amount,
            r.asset,
            r.payee,
            r.transaction.as_deref().unwrap_or("not given")
        ),
        _ => format!(
            "payment_unknown: this step's request was paid, or may have been, and what landed is not known ({}) — agentd never makes such a payment a second time",
            r.error_reason.as_deref().unwrap_or("no answer")
        ),
    }
}

/// What every record of a payment names.
fn record_target(r: &Record) -> Value {
    json!({"request": r.request, "attempt": r.attempt, "run": r.run, "step": r.step,
           "workflow": r.workflow, "wallet": r.wallet, "network": r.network, "asset": r.asset,
           "asset_id": r.asset_id, "payee": r.payee, "pay_to": r.pay_to, "amount": r.amount,
           "nonce": r.nonce, "valid_before": r.valid_before, "url": r.url, "state": r.state.as_str(),
           "transaction": r.transaction, "payer": r.payer, "error_reason": r.error_reason,
           "approved_by": r.approved_by})
}

/// What the approver reads: agentd's words, the server's quoted.
#[cfg(feature = "a2a")]
fn gate_text(
    policy: &crate::payments::policy::Policy,
    pending: &Pending,
    from: Option<crate::payments::evm::Address>,
    workflow: &str,
    why: &[String],
) -> String {
    let c = &pending.choice;
    let asset = policy.assets.get(&c.asset);
    let whole = asset.map_or_else(|| c.amount.to_string(), |a| a.show(c.amount));
    let mut q = format!(
        "Pay {whole} ({} atomic units of {}) on {} to the payee {} ({}) for {}, from the wallet {}{}? The workflow {workflow:?} asks, in run {}, step {:?}. It asks you because {}.",
        c.amount,
        c.asset_id,
        c.network,
        c.payee,
        c.pay_to,
        pending.call.describe(),
        pending.step_payment.wallet,
        from.map_or_else(String::new, |a| format!(" ({})", a.checksummed())),
        pending.run,
        pending.step,
        why.join("; and "),
    );
    if let Some(d) = &pending.required.description {
        q.push_str(&format!(
            " The server describes it as {}.",
            crate::sec::render::quoted(d, SAID_SHOWN)
        ));
    }
    q.push_str(" Answer approve or decline.");
    q
}
