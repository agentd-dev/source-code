// SPDX-License-Identifier: AGPL-3.0-only
//! A Company's operations (RFC 0047 §5.8.1) — the runtime's half: the
//! `company.operation.*` tools, the User's approval gate, and who is owed
//! each outcome. What is sent to the Company is the supervisor's
//! (`companies::operations`), on a thread of each call's own, never on the
//! loop.
//!
//! **The tools.** `company.operation.get` reads an operation at the
//! Company's operations endpoint, `company.operation.cancel` cancels it
//! (Ops §7), and `company.operation.confirm {company, operation_id}`
//! confirms it — the only arguments it takes: the model cannot pass a
//! revision or `approved_by`, and the argument schema refuses either. Each
//! acts for the one User (`runtime::companies`), in the Company's
//! `limits.in_flight` and after its not-before instant, opens the
//! cross-Company gate first when the work holds another Company's content,
//! and what it answers is the Company's content: the context it reaches
//! carries the Company's origin.
//!
//! **The approval gate** is a Company gate (`runtime::human`): addressed to
//! the configured User, its text agentd's, answered by the User alone —
//! `approve` or `decline` — with no operator standing in and no judge,
//! whatever `agent.approval` or `agent.ask_human_fallback` say. It opens on
//! the task of the work that asks, or in unowned work with
//! `agent.ask_human_unowned: gate` on a task made for it; with nobody to
//! ask, nothing is confirmed. Only the User's `approve` sends the confirm,
//! with `approved_by: user` and the revision the gate showed, while the
//! Company still says of it what the gate showed; `decline` cancels the
//! operation at the Company (Ops §7: "The Personal Agent cancels an
//! operation to decline a proposal"). An answer is a message of its own: one
//! the task holds already — a client sending it again — answers nothing,
//! whatever the gate asks now. And it is bound to the question its task
//! asked: asked again — the terms changed — the User is asked on a task made
//! for the new question, the old one closed (cancelled when it was made for
//! its question; the work's own goes on working), and while the confirm
//! lives a task whose question is closed refuses every message, so a second
//! click on what the User was shown before never approves a revision they
//! were not shown. The tool call is pending while the gate is open; one that
//! a standing permission covers asks no one.
//!
//! **Withdrawn.** When the work that asked goes — its run or its task
//! cancelled — while the gate is open, nothing is confirmed. Once the confirm
//! is on its way, it is stopped before it is sent; if the Company took it, or
//! may have, agentd cancels the operation there (Ops §7: "If it has already
//! confirmed, it cancels"), and the records of the confirm and the cancel
//! are kept all the same.
//!
//! **The record.** The gate's question and answer, a standing permission's
//! use, the confirm with its revision and `approved_by` and how it went, a
//! withdrawal, and the final state are each an audit record
//! (`company.operation`, `company.standing`) naming the same
//! `operation_id` and revision — and the question, its answer and a confirm
//! the User approved the task that asked — agentd's own account of approval
//! against action, which the Company cannot see (Ops §8). A final state
//! reached after the call returned — an `in_progress` one read again — is
//! appended to the stream `company.<name>`, carrying the Company's origin.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use super::reactor::{PendingKind, Runtime, Target};
use super::tools::{ExecTarget, ToolCaller, ToolOutcome};
use crate::companies::ledger::Approval;
use crate::companies::operations::{Outcome, Step};
use crate::state::now_ms;

/// The longest the User's approval is waited for.
#[cfg(feature = "a2a")]
const ASK_FOR_MS: u64 = 24 * 3600 * 1000;

/// A confirm in progress.
pub(crate) struct OperationTrack {
    pub company: String,
    pub operation: String,
    /// Who is owed the outcome: the tool call.
    pub target: Target,
    /// The task the gate opens on — the asker's own — or none, when a task
    /// of its own is made for it; once asked, the task the latest question
    /// is on.
    pub on: Option<String>,
    /// The gate's task, while it is open.
    pub gate: Option<String>,
    /// `on` exists only for this confirm.
    #[cfg_attr(not(feature = "a2a"), allow(dead_code))]
    pub standalone: bool,
    /// The revision the gate asks about, and the digest of what it shows
    /// of it: what the User's approval covers.
    #[cfg_attr(not(feature = "a2a"), allow(dead_code))]
    pub revision: Option<(u64, String)>,
    /// Every task a question of this confirm was asked on, with the
    /// revision it showed, the latest last. Each question is a task of its
    /// own; one that is not the open gate is closed — answered, or asked
    /// again on another — and takes no answer while the confirm lives.
    pub asked: Vec<(String, u64)>,
    /// The User's approval this confirm goes on with: the revision, and the
    /// task whose question they answered.
    pub approved: Option<(u64, String)>,
    /// Who asked.
    pub principal: Option<String>,
    /// Why nobody can be asked, when nobody can.
    pub unanswerable: Option<&'static str>,
    /// Past this the gate ends unanswered.
    pub deadline_ms: Option<u64>,
    /// The context its answer is read into.
    pub reads: Option<super::companies::Reads>,
    pub trace: Option<crate::obs::trace::TraceContext>,
    /// A thread of the confirm is out: it is on its way.
    pub running: bool,
    /// Set to stop the confirm before it is sent.
    pub stop: Arc<AtomicBool>,
    /// The work that asked went while it was on its way: nobody is owed its
    /// outcome.
    pub withdrawn: bool,
    /// How the last confirm sent went, and of which revision.
    pub last: Option<(Outcome, u64)>,
}

/// The shape the approval gate's answer takes.
#[cfg(feature = "a2a")]
pub(crate) fn operation_answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"decision": {"type": "string", "enum": [APPROVE, DECLINE]}},
        "required": ["decision"],
    })
}

/// The User's two answers.
#[cfg(feature = "a2a")]
const APPROVE: &str = "approve";
#[cfg(feature = "a2a")]
const DECLINE: &str = "decline";

impl Runtime {
    /// `company.operation.get`, `.confirm` and `.cancel`: refused at once
    /// when the call is not one, else off the loop.
    pub(crate) fn company_operation_tool(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
    ) -> ToolOutcome {
        let refuse = |e: String| ToolOutcome::Ready(Value::String(format!("{name}: {e}")), true);
        let Some(sup) = self.companies.clone() else {
            return refuse("no Company is declared".into());
        };
        let entry = args["company"].as_str().unwrap_or_default().to_string();
        let Some(company) = sup.company(&entry) else {
            return refuse(format!(
                "{entry:?} is not a company entry (services.<name>.kind: company)"
            ));
        };
        let id = args["operation_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !crate::companies::conversation::is_id(&id) {
            return refuse(format!(
                "{id:?} is not an operation id (1 to 256 letters, digits, - and _)"
            ));
        }
        if let Err(e) = company.pace().check() {
            crate::obs::metrics::record_company_request(&company.name, "operations", "unsent");
            return ToolOutcome::Ready(Value::String(format!("{e}; the call was not sent")), true);
        }
        let reads = self.reads_of(caller);
        let unit = self.caller_trace(caller);
        if name == "company.operation.confirm" {
            return self.start_confirm(caller, company.name.clone(), id, reads, unit);
        }
        let Some(slot) = crate::companies::Company::enter(&company) else {
            crate::obs::metrics::record_company_request(&company.name, "operations", "unsent");
            return refuse(format!(
                "company {} has {} calls in flight (security.companies.limits.in_flight); retry when one ends",
                company.name,
                company.limit()
            ));
        };
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        let name = name.to_string();
        std::thread::Builder::new()
            .name(format!("company:{}:{name}", company.name))
            .spawn(move || {
                let sent = match sup.request_trace(&company, unit.as_ref(), None) {
                    Ok((sent, _)) => sent,
                    Err(e) => {
                        target.send(&tx, Value::String(format!("{name}: {e}")), true);
                        return;
                    }
                };
                let res = if name == "company.operation.cancel" {
                    sup.cancel_operation(&company, &id, sent.as_ref())
                } else {
                    sup.get_operation(&company, &id, sent.as_ref())
                };
                let outcome = match &res {
                    Ok(_) => "ok",
                    Err(e) => e.outcome(),
                };
                crate::obs::metrics::record_company_request(slot.company(), "operations", outcome);
                let read = res.as_ref().map_or_else(|e| e.read_the_company(), |_| true);
                if read && let (Some(into), Some(issuer)) = (reads, slot.issuer()) {
                    let _ = tx.send(super::events::Event::CompanyRead {
                        into,
                        origin: crate::companies::carry::origin(&issuer),
                        why: crate::companies::carry::why(slot.company()),
                    });
                }
                drop(slot);
                match res {
                    Ok(v) => target.send(&tx, v, false),
                    Err(e) => target.send(&tx, Value::String(e.to_string()), true),
                }
            })
            .ok();
        ToolOutcome::Executing
    }

    /// Track the confirm of `id` at `company` for `caller` and start it: the
    /// gate opens if only the User can approve it.
    fn start_confirm(
        &mut self,
        caller: &ToolCaller,
        company: String,
        id: String,
        reads: Option<super::companies::Reads>,
        trace: Option<crate::obs::trace::TraceContext>,
    ) -> ToolOutcome {
        let channel = self.human_channel(caller);
        #[cfg(feature = "a2a")]
        let on: Option<Option<String>> = match &channel {
            super::human::HumanChannel::Owned(t) => Some(Some(t.clone())),
            super::human::HumanChannel::Unowned
                if self.settings.agent.ask_human_unowned
                    == crate::config::settings::AskHumanUnowned::Gate =>
            {
                Some(None)
            }
            _ => None,
        };
        #[cfg(not(feature = "a2a"))]
        let on: Option<Option<String>> = None;
        if let Some(Some(t)) = &on
            && self.gate_open_on(t)
        {
            return ToolOutcome::Ready(
                Value::String(
                    "company.operation.confirm: an ask is already pending on this task".into(),
                ),
                true,
            );
        }
        let track = format!("{}@{}", self.next_id("operation"), self.process_nonce);
        let target = Self::target_of(caller);
        let standalone = match &target {
            Target::Step(..) => false,
            Target::Child(..) => on.as_ref().is_some_and(Option::is_none),
        };
        self.company_operations.insert(
            track.clone(),
            OperationTrack {
                company: company.clone(),
                operation: id.clone(),
                target,
                on: on.clone().flatten(),
                gate: None,
                standalone,
                revision: None,
                asked: Vec::new(),
                approved: None,
                principal: caller.principal.clone(),
                unanswerable: on.is_none().then(|| channel.why_unanswerable()),
                deadline_ms: None,
                reads,
                trace,
                running: false,
                stop: Arc::new(AtomicBool::new(false)),
                withdrawn: false,
                last: None,
            },
        );
        self.drive_confirm(&track, None);
        ToolOutcome::Deferred(PendingKind::CompanyOperation { id: track, company })
    }

    /// Run the confirm `track` on a thread of its own, from `given`, the
    /// User's approval when they gave one: each step comes back to the loop.
    fn drive_confirm(&mut self, track: &str, given: Option<Approval>) {
        let Some(t) = self.company_operations.get_mut(track) else {
            return;
        };
        let (Some(sup), company, id) = (
            self.companies.clone(),
            t.company.clone(),
            t.operation.clone(),
        ) else {
            return;
        };
        let Some(company) = sup.company(&company) else {
            return;
        };
        t.running = true;
        let stop = Arc::clone(&t.stop);
        let unit = t.trace.clone();
        let tx = self.events_tx.clone();
        let track = track.to_string();
        std::thread::Builder::new()
            .name(format!("company-confirm:{}", company.name))
            .spawn(move || {
                let send = |step: Step| {
                    let _ = tx.send(super::events::Event::CompanyOperation {
                        id: track.clone(),
                        step,
                    });
                };
                let Some(slot) = crate::companies::Company::enter(&company) else {
                    return send(Step::Done(Err(format!(
                        "company {} has {} calls in flight (security.companies.limits.in_flight); retry when one ends",
                        company.name,
                        company.limit()
                    ))));
                };
                let sent = match sup.request_trace(&company, unit.as_ref(), None) {
                    Ok((sent, _)) => sent,
                    Err(e) => return send(Step::Done(Err(e))),
                };
                sup.confirm_operation(&company, &id, given, sent.as_ref(), &stop, &send);
                drop(slot);
            })
            .ok();
    }

    /// A confirm moved on: its gate opens, a standing permission or the
    /// Company's answer is said and recorded, or it ended.
    pub(crate) fn on_company_operation(&mut self, track: &str, step: Step) {
        let Some(t) = self.company_operations.get_mut(track) else {
            return;
        };
        let (company, op, principal) =
            (t.company.clone(), t.operation.clone(), t.principal.clone());
        let trace = t.trace.clone();
        match step {
            Step::Ask {
                question,
                revision,
                digest,
            } => {
                // The confirm's thread ended here, asking.
                t.running = false;
                if t.withdrawn {
                    self.end_withdrawn(track);
                    return;
                }
                if let Some(why) = t.unanswerable {
                    self.end_confirm(
                        track,
                        Err(format!(
                            "company {company}: operation {op} needs the User's approval of its terms, and nobody can be asked ({why}) — it was NOT confirmed"
                        )),
                    );
                    return;
                }
                #[cfg(feature = "a2a")]
                self.open_operation_gate(track, &question, revision, digest);
                #[cfg(not(feature = "a2a"))]
                {
                    let _ = (question, revision, digest);
                    self.end_confirm(
                        track,
                        Err(format!(
                            "company {company}: operation {op} needs the User's approval, and no A2A listener can ask them — it was NOT confirmed"
                        )),
                    );
                }
            }
            Step::Standing {
                rule,
                revision,
                uses_left,
            } => {
                self.log.in_trace(trace.as_ref()).info(
                    "company.standing.used",
                    json!({"company": company, "rule": rule, "operation_id": op, "revision": revision, "uses_left": uses_left}),
                );
                self.audit(super::audit::AuditEvent {
                    action: "company.standing",
                    target: json!({"company": company, "rule": rule, "operation_id": op, "revision": revision, "uses_left": uses_left}),
                    outcome: "used",
                    principal: Some(crate::companies::wire::operations::APPROVED_BY_STANDING),
                    role: None,
                    request_id: None,
                    sid: None,
                    trace: trace.as_ref(),
                    link: None,
                });
                self.log.in_trace(trace.as_ref()).info(
                    "company.operation.approved",
                    json!({"company": company, "operation_id": op, "revision": revision,
                           "approved_by": crate::companies::wire::operations::APPROVED_BY_STANDING}),
                );
            }
            Step::Confirmed {
                revision,
                approved_by,
                outcome,
                state,
                error,
            } => {
                t.last = Some((outcome, revision));
                // The User's approval names the task whose question it
                // answered, as their answer's record does.
                let asked_on = t
                    .approved
                    .as_ref()
                    .filter(|(r, _)| {
                        *r == revision
                            && approved_by == crate::companies::wire::operations::APPROVED_BY_USER
                    })
                    .map(|(_, task)| task.clone());
                self.log.in_trace(trace.as_ref()).info(
                    "company.operation.confirmed",
                    json!({"company": company, "operation_id": op, "revision": revision,
                           "approved_by": approved_by, "outcome": outcome.audit(), "state": state, "error": error}),
                );
                let mut target = json!({"company": company, "operation_id": op, "revision": revision,
                                        "approved_by": approved_by, "state": state, "error": error});
                if let Some(task) = asked_on {
                    target["task"] = json!(task);
                }
                self.audit(super::audit::AuditEvent {
                    action: "company.operation",
                    target,
                    outcome: outcome.audit(),
                    principal: principal.as_deref(),
                    role: None,
                    request_id: None,
                    sid: None,
                    trace: trace.as_ref(),
                    link: None,
                });
            }
            Step::Done(result) => {
                if t.withdrawn {
                    self.end_withdrawn(track);
                } else {
                    self.end_confirm(track, result);
                }
            }
        }
    }

    /// The confirm `track` ended after the work that asked withdrew it:
    /// nobody is owed its outcome. One the Company took, or may have, is
    /// cancelled there (Ops §7); one stopped before it was sent confirmed
    /// nothing. Either is an audit record, `withdrawn`.
    fn end_withdrawn(&mut self, track: &str) {
        let Some(t) = self.company_operations.remove(track) else {
            return;
        };
        self.pending.retain(|p| p.target != t.target);
        let confirmed = matches!(t.last, Some((Outcome::Taken | Outcome::Unknown, _)));
        let revision = t
            .last
            .map(|(_, r)| r)
            .or(t.revision.as_ref().map(|(r, _)| *r));
        let said = if confirmed {
            format!(
                "company {}: the work that asked to confirm operation {} was withdrawn after the confirm was sent; agentd asks the Company to cancel it",
                t.company, t.operation
            )
        } else {
            format!(
                "company {}: the confirm of operation {} was withdrawn — it was NOT confirmed",
                t.company, t.operation
            )
        };
        #[cfg(feature = "a2a")]
        if t.standalone
            && let Some(task) = t.on.clone()
        {
            use crate::a2a::tasks::State;
            if let Some(rec) = self.tasks.get_mut(&task) {
                rec.company_gate = None;
                rec.ask_schema = None;
                if !rec.state.is_terminal() {
                    rec.transition(State::Failed, Some(said.clone()));
                }
            }
            self.task_persist(&task);
            self.task_sync(&task);
        }
        #[cfg(not(feature = "a2a"))]
        let _ = said;
        self.log.in_trace(t.trace.as_ref()).info(
            "company.operation.withdrawn",
            json!({"company": t.company, "operation_id": t.operation, "revision": revision, "confirmed": confirmed}),
        );
        self.audit(super::audit::AuditEvent {
            action: "company.operation",
            target: json!({"company": t.company, "operation_id": t.operation, "revision": revision,
                           "cancel": confirmed}),
            outcome: "withdrawn",
            principal: t.principal.as_deref(),
            role: None,
            request_id: None,
            sid: None,
            trace: t.trace.as_ref(),
            link: None,
        });
        if !confirmed {
            return;
        }
        let Some(sup) = self.companies.clone() else {
            return;
        };
        let Some(company) = sup.company(&t.company) else {
            return;
        };
        let (id, unit, log) = (t.operation, t.trace, self.log.clone());
        std::thread::Builder::new()
            .name(format!("company-withdraw:{}", company.name))
            .spawn(move || {
                let sent = sup
                    .request_trace(&company, unit.as_ref(), None)
                    .ok()
                    .and_then(|(s, _)| s);
                let cancelled = sup.cancel_operation(&company, &id, sent.as_ref());
                log.in_trace(unit.as_ref()).info(
                    "company.operation.cancelled",
                    match &cancelled {
                        Ok(v) => json!({"company": company.name, "operation_id": id, "state": v["state"]}),
                        Err(e) => json!({"company": company.name, "operation_id": id, "error": e.to_string()}),
                    },
                );
            })
            .ok();
    }

    /// Open the confirm `track`'s gate for `revision` with `question`,
    /// addressed to the User: the first on the task it opens on, or a task of
    /// its own; one asked again — the terms changed — on a task made for it,
    /// the question before it closed. An answer is bound to the question its
    /// task asked, so no answer to an earlier one approves this revision.
    #[cfg(feature = "a2a")]
    fn open_operation_gate(&mut self, track: &str, question: &str, revision: u64, digest: String) {
        use crate::a2a::tasks::{CompanyGate, CompanyGateKind, State};
        let Some(t) = self.company_operations.get(track) else {
            return;
        };
        let (company, op, on, first_standalone, target, principal, trace) = (
            t.company.clone(),
            t.operation.clone(),
            t.on.clone(),
            t.standalone,
            t.target.clone(),
            t.principal.clone(),
            t.trace.clone(),
        );
        let earlier = t.asked.last().cloned();
        let (task, standalone) = match &earlier {
            None => (
                self.company_gate_task(
                    on,
                    Some(&target),
                    principal.as_deref(),
                    &format!("company-operation-{track}"),
                ),
                first_standalone,
            ),
            // Seen by whoever could see the question before it.
            Some((old, _)) => {
                let owner = self
                    .tasks
                    .get(old)
                    .and_then(|t| t.principal.clone())
                    .or(principal.clone());
                let task = self.company_gate_task(
                    None,
                    None,
                    owner.as_deref(),
                    &format!("company-operation-{track}"),
                );
                (task, true)
            }
        };
        if let Some((old, was)) = earlier {
            self.close_operation_question(track, &old, was, revision, &task);
        }
        if let Some(t) = self.tasks.get_mut(&task) {
            t.ask_schema = Some(operation_answer_schema());
            t.ask_channel = None;
            t.company_gate = Some(CompanyGate {
                company: company.clone(),
                kind: CompanyGateKind::Operation {
                    operation: op.clone(),
                },
                standalone,
            });
            t.transition(State::InputRequired, Some(question.to_string()));
        }
        self.task_persist(&task);
        self.task_sync(&task);
        if let Some(t) = self.company_operations.get_mut(track) {
            t.gate = Some(task.clone());
            t.on = Some(task.clone());
            t.standalone = standalone;
            t.revision = Some((revision, digest));
            t.asked.push((task.clone(), revision));
            t.deadline_ms = Some(now_ms().saturating_add(ASK_FOR_MS));
        }
        self.log.in_trace(trace.as_ref()).info(
            "company.operation.asked",
            json!({"company": company, "operation_id": op, "revision": revision, "task": task}),
        );
        self.audit(super::audit::AuditEvent {
            action: "company.operation",
            target: json!({"company": company, "operation_id": op, "revision": revision, "task": task}),
            outcome: "asked",
            principal: principal.as_deref(),
            role: None,
            request_id: None,
            sid: None,
            trace: trace.as_ref(),
            link: None,
        });
    }

    /// The confirm whose approval gate is open on `task`.
    #[cfg(feature = "a2a")]
    pub(crate) fn operation_gate_on(&self, task: &str) -> Option<String> {
        self.company_operations
            .iter()
            .find(|(_, t)| t.gate.as_deref() == Some(task))
            .map(|(id, _)| id.clone())
    }

    /// The confirm a question of which `task` asked, now closed — answered,
    /// or asked again on another task — while the confirm lives.
    #[cfg(feature = "a2a")]
    pub(crate) fn operation_closed_on(&self, task: &str) -> Option<String> {
        self.company_operations
            .iter()
            .find(|(_, t)| {
                t.gate.as_deref() != Some(task) && t.asked.iter().any(|(a, _)| a == task)
            })
            .map(|(id, _)| id.clone())
    }

    /// The question `old` of the confirm `track`, about revision `was`, is
    /// asked again about `revision` on `task`: it closes, saying where the
    /// question went. A task made for it is cancelled; the work's own goes
    /// on working, and takes no answer while the confirm lives.
    #[cfg(feature = "a2a")]
    fn close_operation_question(
        &mut self,
        track: &str,
        old: &str,
        was: u64,
        revision: u64,
        task: &str,
    ) {
        use crate::a2a::tasks::State;
        let Some(t) = self.company_operations.get_mut(track) else {
            return;
        };
        let standalone = t.standalone;
        if t.gate.as_deref() == Some(old) {
            t.gate = None;
        }
        let said = format!(
            "This question, about revision {was} of operation {} at {}, is closed and approves nothing more: agentd asks again, about revision {revision}, on task {task}.",
            t.operation, t.company
        );
        if let Some(rec) = self.tasks.get_mut(old) {
            rec.company_gate = None;
            rec.ask_schema = None;
            if !rec.state.is_terminal() {
                rec.transition(
                    if standalone {
                        State::Canceled
                    } else {
                        State::Working
                    },
                    Some(said),
                );
            }
        }
        self.task_persist(old);
        self.task_sync(old);
    }

    /// A message on `task`, whose question of the confirm `track` is
    /// closed: refused, unrecorded. It can only answer what that task
    /// showed, and nothing it says approves a later revision.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_operation_closed(&self, track: &str, task: &str) -> Value {
        let Some(t) = self.company_operations.get(track) else {
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "this approval has ended",
            );
        };
        let was = t
            .asked
            .iter()
            .rev()
            .find(|(a, _)| a == task)
            .map_or(0, |(_, r)| *r);
        let now = match (&t.gate, &t.revision) {
            (Some(g), Some((r, _))) => {
                format!("the approval of revision {r} is asked on task {g} now; answer it there")
            }
            _ => "the confirm goes on with the answer already given".to_string(),
        };
        super::a2a_server::err_obj(
            ::mcp::rpc::INVALID_PARAMS,
            &format!(
                "task {task} asked about revision {was} of operation {} at {}, and that question is closed: {now}. This message was not recorded and approves nothing.",
                t.operation, t.company
            ),
        )
    }

    /// The User's answer `text` on the approval gate `task` (`track`'s), from
    /// `principal`: only the User's `approve` or `decline` is taken, and
    /// anything else is refused with the gate left open.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_operation_answer(
        &mut self,
        track: &str,
        task: &str,
        principal: &crate::a2a::Principal,
        text: &str,
        message: &Value,
    ) -> Value {
        let to = self.company_user();
        let Some(t) = self.company_operations.get(track) else {
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "this approval has ended",
            );
        };
        let (company, op, asked, trace, questions) = (
            t.company.clone(),
            t.operation.clone(),
            t.revision.clone(),
            t.trace.clone(),
            t.asked.clone(),
        );
        let revision = asked.as_ref().map(|(r, _)| *r);
        if !to.matches(principal) {
            let want = to.describe();
            self.log.info(
                "human.answer.not_addressed",
                json!({"task": task, "from": principal.id, "addressee": want}),
            );
            self.audit(super::audit::AuditEvent {
                action: "company.operation",
                target: json!({"company": company, "operation_id": op, "revision": revision, "task": task}),
                outcome: "not_addressed",
                principal: Some(&principal.id),
                role: None,
                request_id: None,
                sid: principal.session.as_deref(),
                trace: trace.as_ref(),
                link: None,
            });
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!(
                    "this approval is for {want}; your answer was not recorded and the gate is still open"
                ),
            );
        }
        // A message this task, or one an earlier question was on, holds
        // already answered an earlier question — a client sending it again —
        // and answers nothing now.
        if let Some(mid) = message["messageId"].as_str()
            && std::iter::once(task)
                .chain(questions.iter().map(|(q, _)| q.as_str()))
                .any(|q| self.tasks.get(q).is_some_and(|t| t.holds_inbound(mid)))
        {
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!(
                    "the message {mid:?} was taken already, as the answer to an earlier question; the gate asks about revision {} now and is still open — answer it with a new message",
                    revision.unwrap_or_default()
                ),
            );
        }
        let shaped = super::human::shape_answer(text, &operation_answer_schema());
        let decision = shaped["decision"].as_str().unwrap_or_default().to_string();
        if decision != APPROVE && decision != DECLINE {
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "answer approve or decline; nothing else approves an operation, and the gate is still open",
            );
        }
        let Some((revision, digest)) = asked else {
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "this approval is not open",
            );
        };
        if let Some(t) = self.tasks.get_mut(task) {
            t.record_inbound(message);
        }
        self.log.in_trace(trace.as_ref()).info(
            if decision == APPROVE {
                "company.operation.approved"
            } else {
                "company.operation.declined"
            },
            json!({"company": company, "operation_id": op, "revision": revision,
                   "approved_by": (decision == APPROVE).then_some(crate::companies::wire::operations::APPROVED_BY_USER),
                   "by": principal.id}),
        );
        self.audit(super::audit::AuditEvent {
            action: "company.operation",
            target: json!({"company": company, "operation_id": op, "revision": revision, "task": task}),
            outcome: if decision == APPROVE { "approved" } else { "declined" },
            principal: Some(&principal.id),
            role: None,
            request_id: None,
            sid: principal.session.as_deref(),
            trace: trace.as_ref(),
            link: None,
        });
        crate::obs::metrics::record_company_gate(&company, "operation", &decision);
        // The gate closes: the confirm, or the cancel, goes on without it.
        self.close_operation_gate(track, "answered", None);
        if decision == APPROVE
            && let Some(t) = self.company_operations.get_mut(track)
        {
            t.approved = Some((revision, task.to_string()));
        }
        if decision == APPROVE {
            self.drive_confirm(
                track,
                Some(Approval {
                    revision,
                    digest,
                    approved_by: crate::companies::wire::operations::APPROVED_BY_USER.into(),
                    by: principal.id.clone(),
                }),
            );
        } else {
            self.drive_decline(track);
        }
        self.task_reply(task)
    }

    /// The User declined `track`'s operation: it is cancelled at the Company
    /// (Ops §7), off the loop, and the asker told it was NOT confirmed.
    #[cfg(feature = "a2a")]
    fn drive_decline(&mut self, track: &str) {
        let Some(t) = self.company_operations.get(track) else {
            return;
        };
        let (Some(sup), company, id, unit) = (
            self.companies.clone(),
            t.company.clone(),
            t.operation.clone(),
            t.trace.clone(),
        ) else {
            return;
        };
        let Some(company) = sup.company(&company) else {
            return;
        };
        let tx = self.events_tx.clone();
        let track = track.to_string();
        std::thread::Builder::new()
            .name(format!("company-decline:{}", company.name))
            .spawn(move || {
                let sent = sup
                    .request_trace(&company, unit.as_ref(), None)
                    .ok()
                    .and_then(|(s, _)| s);
                let cancelled = sup.cancel_operation(&company, &id, sent.as_ref());
                let said = match &cancelled {
                    Ok(v) => format!(
                        "company {}: the User declined operation {id} — it was NOT confirmed, and agentd cancelled it at the Company (it is {})",
                        company.name,
                        v["state"].as_str().unwrap_or("unknown")
                    ),
                    Err(e) => format!(
                        "company {}: the User declined operation {id} — it was NOT confirmed; cancelling it at the Company failed: {e}",
                        company.name
                    ),
                };
                let _ = tx.send(super::events::Event::CompanyOperation {
                    id: track,
                    step: Step::Done(Err(said)),
                });
            })
            .ok();
    }

    /// Close `track`'s gate, if it is open: a task of its own ends — with
    /// `ended`, its outcome, once the confirm has one — one that tracks other
    /// work goes back to working.
    fn close_operation_gate(
        &mut self,
        track: &str,
        why: &str,
        ended: Option<&Result<Value, String>>,
    ) {
        #[cfg(feature = "a2a")]
        {
            use crate::a2a::tasks::State;
            let Some(t) = self.company_operations.get_mut(track) else {
                return;
            };
            let Some(task) = t.gate.take() else {
                return;
            };
            let standalone = t.standalone;
            if let Some(task_rec) = self.tasks.get_mut(&task) {
                task_rec.company_gate = None;
                task_rec.ask_schema = None;
                if task_rec.state.is_terminal() {
                    // Its work ended first.
                } else if standalone && let Some(r) = ended {
                    match r {
                        Ok(v) => {
                            task_rec.set_result(v.clone());
                            task_rec.transition(State::Completed, Some(why.to_string()));
                        }
                        Err(e) => task_rec.transition(State::Failed, Some(e.clone())),
                    }
                } else {
                    task_rec.transition(State::Working, Some(why.to_string()));
                }
            }
            self.task_persist(&task);
            self.task_sync(&task);
        }
        #[cfg(not(feature = "a2a"))]
        let _ = (track, why, ended);
    }

    /// The confirm `track` ended with `result`: its gate closed, its asker
    /// told — the context it reaches carrying the Company's origin.
    fn end_confirm(&mut self, track: &str, result: Result<Value, String>) {
        let said = match &result {
            Ok(v) => v["note"].as_str().unwrap_or("ended").to_string(),
            Err(e) => e.clone(),
        };
        self.close_operation_gate(track, &said, Some(&result));
        // A task of its own, kept for the confirm after the User answered.
        #[cfg(feature = "a2a")]
        if let Some(t) = self.company_operations.get(track)
            && t.standalone
            && let Some(task) = t.on.clone()
        {
            use crate::a2a::tasks::State;
            if let Some(rec) = self.tasks.get_mut(&task)
                && !rec.state.is_terminal()
            {
                match &result {
                    Ok(v) => {
                        rec.set_result(v.clone());
                        rec.transition(State::Completed, Some(said.clone()));
                    }
                    Err(e) => rec.transition(State::Failed, Some(e.clone())),
                }
                self.task_persist(&task);
                self.task_sync(&task);
            }
        }
        let Some(t) = self.company_operations.remove(track) else {
            return;
        };
        if let (Some(into), Some(sup)) = (t.reads.clone(), self.companies.clone())
            && let Some(issuer) = sup.company(&t.company).and_then(|c| c.issuer())
        {
            self.company_read(
                into,
                crate::companies::carry::origin(&issuer),
                crate::companies::carry::why(&t.company),
            );
        }
        let outcome = match &result {
            Ok(_) => "ok",
            Err(e) if super::companies::is_outcome_unknown(e) => "unknown",
            Err(_) => "error",
        };
        crate::obs::metrics::record_company_request(&t.company, "operations", outcome);
        self.pending.retain(|p| p.target != t.target);
        match result {
            Ok(v) => self.reply(&t.target, v, false),
            Err(e) => self.reply(&t.target, Value::String(e), true),
        }
    }

    /// Each tick: a confirm whose asker is gone, or whose gate's task ended
    /// — cancelled, say — ends with nothing confirmed while its gate is open,
    /// and one past its time ends unanswered; one on its way is withdrawn,
    /// stopped before it is sent (`end_withdrawn` says how it ended). And
    /// the same for the Direct Conversations' questions
    /// (`runtime::company_direct`).
    pub(crate) fn poll_company_gates(&mut self) {
        if !self.company_operations.is_empty() {
            let now = now_ms();
            let withdraw: Vec<String> = self
                .company_operations
                .iter()
                .filter(|(_, t)| t.running && !t.withdrawn && self.confirm_asker_gone(t))
                .map(|(id, _)| id.clone())
                .collect();
            for id in withdraw {
                if let Some(t) = self.company_operations.get_mut(&id) {
                    t.withdrawn = true;
                    t.stop.store(true, Ordering::SeqCst);
                }
            }
            let ends: Vec<(String, String, &'static str)> = self
                .company_operations
                .iter()
                .filter(|(_, t)| !t.running)
                .filter_map(|(id, t)| {
                    if self.confirm_asker_gone(t) {
                        Some((
                            id.clone(),
                            format!(
                                "company {}: the approval of operation {} was cancelled — it was NOT confirmed",
                                t.company, t.operation
                            ),
                            "cancelled",
                        ))
                    } else if t.gate.is_some() && t.deadline_ms.is_some_and(|d| now >= d) {
                        Some((
                            id.clone(),
                            format!(
                                "company {}: the User did not answer the approval of operation {} in time — it was NOT confirmed",
                                t.company, t.operation
                            ),
                            "unanswered",
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            for (id, why, answer) in ends {
                if let Some(t) = self.company_operations.get(&id) {
                    crate::obs::metrics::record_company_gate(&t.company, "operation", answer);
                }
                self.end_confirm(&id, Err(why));
            }
        }
        self.poll_company_directs();
    }

    /// Whether the work that asked for the confirm `t` is gone — its child
    /// or its step — or the User cancelled the task it is asked on: its gate
    /// while that is open, a task of its own while it is on its way.
    fn confirm_asker_gone(&self, t: &OperationTrack) -> bool {
        let gone = match &t.target {
            Target::Child(node, _) => self.children.get(*node).is_none(),
            Target::Step(run, step) => !self
                .runs
                .get(run)
                .and_then(|r| r.steps.get(step))
                .is_some_and(|s| s.status == crate::engine::run::StepStatus::Suspended),
        };
        let cancelled = t.gate.as_deref().is_some_and(|g| self.gate_task_ended(g))
            || (t.running
                && t.standalone
                && t.on.as_deref().is_some_and(|g| self.gate_task_ended(g)));
        gone || cancelled
    }

    /// Whether the task `id` has ended, or is gone.
    pub(crate) fn gate_task_ended(&self, id: &str) -> bool {
        #[cfg(feature = "a2a")]
        {
            self.tasks.get(id).is_none_or(|t| t.state.is_terminal())
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = id;
            false
        }
    }

    /// The supervisor told the loop something: an operation's final state —
    /// appended to the stream `company.<name>` carrying the Company's origin,
    /// and an audit record — or a Company's request to talk with the User.
    pub(crate) fn on_company_notice(&mut self, n: crate::companies::Notice) {
        match n {
            crate::companies::Notice::Final(f) => {
                let stream = crate::config::settings::companies::stream_of(&f.company);
                let mut carries = super::withhold::Carries::default();
                carries.add([crate::companies::carry::origin(&f.issuer)], || {
                    crate::companies::carry::why(&f.company)
                });
                let appended = self
                    .append_carrying(
                        &stream,
                        "operation",
                        Some(&f.operation_id),
                        f.data.clone(),
                        &format!("{}/{}", f.operation_id, f.state),
                        &format!("company:{}", f.company),
                        Some(&carries),
                        None,
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                self.audit(super::audit::AuditEvent {
                    action: "company.operation",
                    target: json!({"company": f.company, "operation_id": f.operation_id,
                                   "revision": f.revision, "state": f.state}),
                    outcome: "final",
                    principal: None,
                    role: None,
                    request_id: None,
                    sid: None,
                    trace: None,
                    link: None,
                });
                let _ = f.ack.try_send(appended);
            }
            crate::companies::Notice::DirectAsked(a) => self.on_company_direct_asked(a),
            // The operation tools made from it are listed again (`join`).
            crate::companies::Notice::DescriptionChanged(url) => {
                self.joining.description_changed(&url)
            }
            crate::companies::Notice::ApisChanged(entry) => {
                let servers: Vec<String> = self
                    .settings
                    .mcp
                    .servers
                    .iter()
                    .filter(|s| {
                        s.company.as_ref().is_some_and(|c| c.entry == entry)
                            && crate::config::settings::companies::is_api_server(
                                &self.settings,
                                &s.name,
                            )
                    })
                    .map(|s| s.name.clone())
                    .collect();
                self.joining.apis_changed(&servers)
            }
        }
    }

    /// After a restore: every operation's approval gate a previous life left
    /// open ends `interrupted`, its task saying so — nothing was confirmed
    /// from it.
    #[cfg(feature = "a2a")]
    pub(crate) fn interrupt_company_operation_gates(&mut self) {
        use crate::a2a::tasks::{CompanyGateKind, State};
        let open: Vec<(String, bool, String, String)> = self
            .tasks
            .values()
            .filter(|t| !t.state.is_terminal())
            .filter_map(|t| match &t.company_gate {
                Some(g) => match &g.kind {
                    CompanyGateKind::Operation { operation } => Some((
                        t.id.clone(),
                        g.standalone,
                        g.company.clone(),
                        operation.clone(),
                    )),
                    _ => None,
                },
                None => None,
            })
            .collect();
        for (task, standalone, company, operation) in open {
            let said = format!(
                "The approval of operation {operation} at {company} was interrupted: agentd restarted before you answered, and nothing was confirmed. Ask again to confirm it."
            );
            if let Some(t) = self.tasks.get_mut(&task) {
                t.company_gate = None;
                t.ask_schema = None;
                t.transition(
                    if standalone {
                        State::Failed
                    } else {
                        State::Working
                    },
                    Some(said),
                );
            }
            self.task_persist(&task);
            self.task_sync(&task);
            crate::obs::metrics::record_company_gate(&company, "operation", "interrupted");
            self.log.info(
                "company.operation.interrupted",
                json!({"company": company, "operation_id": operation, "task": task}),
            );
        }
    }
}
