// SPDX-License-Identifier: AGPL-3.0-only
//! The User signs in at a Company, and out (RFC 0047 §5.7) — the runtime's
//! half: the gate the User is asked at, and who is owed the outcome. What is
//! sent to the Company is the supervisor's (`companies::signin`), on a
//! thread of each sign-in's own, never on the loop.
//!
//! **Who asks.** The model's `company.sign_in` tool, in the User's own work
//! (`runtime::companies`), a workflow `tool` step calling it, or the
//! operator op `company.sign_in` — `agentd --login company:<name>`, and the
//! TUI or web UI — which only the User may send. The tool call is pending
//! while the gate is open, as `ask_human`'s is, and returns `{signed_in,
//! scope, requested}`, or `cancelled`, `denied`, `expired`,
//! `account_mismatch` or `interrupted`. The model never sees the code, the
//! link or a token: it sees the outcome.
//!
//! **The gate** is a Company gate (`runtime::human`): addressed to the
//! configured User, its text fixed in code with the Company's words quoted,
//! answered by the User alone — no operator stands in, no judge answers it,
//! whatever `agent.approval` or `agent.ask_human_fallback` say. It opens on
//! the task of the work that asks, or in unowned work with
//! `agent.ask_human_unowned: gate` on a task made for it — the run's, for a
//! workflow step, which the run settles — or on the op's own task; with
//! nobody to ask, the sign-in is refused before anything is sent to the
//! Company. It is the one gate on its task. The User signs in at the
//! Company, not at the gate: the supervisor closes it when the poll or the
//! callback ends, and the one answer it takes is `cancel` — cancelling its
//! task ends it too. A sign-in that is still pending when agentd stops ends
//! `interrupted` at the next start, and its gate says so: its device code
//! or verifier was in memory only.
//!
//! **Which account.** Each names the User's account at the Company it is
//! for by the label agentd keeps it under (`account`, RFC 0047 §9, P5b) —
//! `agentd --login company:<name>/<label>` — and the gate's text names it.
//!
//! **Signing out** (`company.sign_out`, the op, `agentd --logout
//! company:<name>`) revokes and forgets one account's Account Token; the
//! others stay signed in.
//!
//! Sign-ins and sign-outs are audit records (`company.sign_in`,
//! `company.sign_out`) naming who asked, the account by its label, and how
//! each ended.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use super::reactor::{PendingKind, Runtime, Target};
use super::tools::{ToolCaller, ToolOutcome};
use crate::companies::signin::{self, Ask, Outcome, Started, Step};
use crate::config::settings::companies::SignIn;
use crate::state::now_ms;

/// Who is owed a sign-in's or a sign-out's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owed {
    /// A tool call, pending as [`PendingKind::CompanySignIn`] while it runs.
    Call(Target),
    /// An operator op's task, which says it.
    Task(String),
}

/// A sign-in in progress.
pub(crate) struct SignInTrack {
    pub company: String,
    pub owed: Owed,
    /// The task the gate opens on — the asker's own, or the op's — or none,
    /// when a task of its own is made for it.
    pub on: Option<String>,
    /// The gate's task, while it is open.
    pub gate: Option<String>,
    /// The gate's task exists only for this sign-in.
    #[cfg_attr(not(feature = "a2a"), allow(dead_code))]
    pub standalone: bool,
    /// Set to stop the poll.
    pub cancel: Arc<AtomicBool>,
    pub kind: Option<SignIn>,
    pub requested: Vec<String>,
    /// Past this the sign-in ends `expired`, whatever the Company says.
    pub deadline_ms: Option<u64>,
    /// Who asked.
    pub principal: Option<String>,
    /// The label of the account it signs in to, once the supervisor knows
    /// it.
    pub account: Option<String>,
}

/// The shape a sign-in gate's answer takes: the User signs in at the
/// Company, and the one thing they may answer here is `cancel`.
#[cfg(feature = "a2a")]
pub(crate) fn sign_in_answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"decision": {"type": "string", "enum": ["cancel"]}},
        "required": ["decision"],
    })
}

/// The answer a sign-in gate ends with.
#[cfg(feature = "a2a")]
const CANCEL: &str = "cancel";

impl Runtime {
    /// What `args` ask a sign-in for, held to the declared Companies.
    fn sign_in_ask(&self, args: &Value) -> Result<Ask, String> {
        let name = args["company"].as_str().unwrap_or_default();
        let sup = self
            .companies
            .as_ref()
            .ok_or("no Company is declared".to_string())?;
        if sup.company(name).is_none() {
            return Err(format!(
                "{name:?} is not a company entry (services.<name>.kind: company)"
            ));
        }
        Ok(Ask {
            company: name.to_string(),
            scopes: args["scopes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            account: args["account"].as_str().map(str::to_string),
            new_account: args["new_account"].as_bool().unwrap_or(false),
        })
    }

    /// A sign-in's id: this process's own, so a step a previous life left
    /// waiting on one never names a sign-in of this life.
    fn sign_in_id(&mut self) -> String {
        format!("{}@{}", self.next_id("signin"), self.process_nonce)
    }

    /// The target a tool call from `caller` is answered at.
    pub(crate) fn target_of(caller: &ToolCaller) -> Target {
        match (caller.node, &caller.run, &caller.step) {
            (Some(node), _, _) => Target::Child(node, caller.req),
            (None, Some(run), Some(step)) => Target::Step(run.clone(), step.clone()),
            _ => Target::Child(crate::supervisor::tree::NodeId(0), caller.req),
        }
    }

    /// The sign-in whose gate is on `task`, while it is open.
    pub(crate) fn sign_in_on(&self, task: &str) -> Option<String> {
        self.company_sign_ins
            .iter()
            .find(|(_, t)| t.gate.as_deref() == Some(task))
            .map(|(id, _)| id.clone())
    }

    /// The sign-ins on `task`: whose gate is open there, that will open it
    /// there, or that the op it runs started — what cancelling it ends.
    #[cfg(feature = "a2a")]
    pub(crate) fn sign_ins_on(&self, task: &str) -> Vec<String> {
        self.company_sign_ins
            .iter()
            .filter(|(_, t)| {
                t.gate.as_deref() == Some(task)
                    || t.on.as_deref() == Some(task)
                    || t.owed == Owed::Task(task.to_string())
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Whether the task `id` has ended, or is gone.
    #[cfg(feature = "a2a")]
    fn task_ended(&self, id: &str) -> bool {
        self.tasks.get(id).is_none_or(|t| t.state.is_terminal())
    }

    /// Without A2A there are no tasks.
    #[cfg(not(feature = "a2a"))]
    fn task_ended(&self, _id: &str) -> bool {
        false
    }

    /// Whether a gate is open on `task`: an ask, a policy or Company gate,
    /// a sign-in or an operation's approval — tracked there before its gate
    /// opens, and after its question there closed, too — or a Direct
    /// Conversation's.
    pub(crate) fn gate_open_on(&self, task: &str) -> bool {
        self.sign_in_on(task).is_some()
            || self
                .company_sign_ins
                .values()
                .any(|t| t.on.as_deref() == Some(task))
            || self.company_operations.values().any(|t| {
                t.on.as_deref() == Some(task)
                    || t.gate.as_deref() == Some(task)
                    || t.asked.iter().any(|(a, _)| a == task)
            })
            || self.company_directs.values().any(|t| t.task == task)
            || self
                .pending
                .iter()
                .any(|p| matches!(&p.kind, PendingKind::Human { task: t, .. } if t == task))
    }

    /// `company.sign_in {company, scopes?, new_account?}`, the internal tool:
    /// pending until the sign-in ends.
    pub(crate) fn company_sign_in_tool(
        &mut self,
        caller: &ToolCaller,
        args: &Value,
    ) -> ToolOutcome {
        let ask = match self.sign_in_ask(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::Ready(Value::String(format!("company.sign_in: {e}")), true);
            }
        };
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
                Value::String("company.sign_in: an ask is already pending on this task".into()),
                true,
            );
        }
        let id = self.sign_in_id();
        let company = ask.company.clone();
        self.start_sign_in(
            &id,
            ask,
            Owed::Call(Self::target_of(caller)),
            on.clone().flatten(),
            on.is_none().then(|| channel.why_unanswerable()),
            caller.principal.clone(),
        );
        ToolOutcome::Deferred(PendingKind::CompanySignIn { id, company })
    }

    /// The op `company.sign_in`: its task, `task`, is the gate, and says how
    /// the sign-in ended.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_sign_in_op(
        &mut self,
        task: &str,
        args: &Value,
        by: &str,
    ) -> Result<(), String> {
        let ask = self.sign_in_ask(args)?;
        let id = self.sign_in_id();
        self.start_sign_in(
            &id,
            ask,
            Owed::Task(task.to_string()),
            Some(task.to_string()),
            None,
            Some(by.to_string()),
        );
        Ok(())
    }

    /// Track the sign-in `id` and start it on a thread of its own: the
    /// supervisor begins it, the gate is opened when it is ready
    /// ([`Self::on_company_sign_in`]), and a Device Sign-In is polled to its
    /// end. `unanswerable` says why nobody can be asked, when nobody can.
    fn start_sign_in(
        &mut self,
        id: &str,
        ask: Ask,
        owed: Owed,
        on: Option<String>,
        unanswerable: Option<&'static str>,
        principal: Option<String>,
    ) {
        let cancel = Arc::new(AtomicBool::new(false));
        // A gate task made for a workflow step's sign-in is the run's task,
        // which the run's completion settles; the op's own task, and one
        // made for a turn, end with the sign-in.
        let standalone = match &owed {
            Owed::Task(_) => true,
            Owed::Call(Target::Step(..)) => false,
            Owed::Call(_) => on.is_none(),
        };
        self.company_sign_ins.insert(
            id.to_string(),
            SignInTrack {
                company: ask.company.clone(),
                owed,
                on,
                gate: None,
                standalone,
                cancel: Arc::clone(&cancel),
                kind: None,
                requested: Vec::new(),
                deadline_ms: None,
                principal,
                account: None,
            },
        );
        let Some(sup) = self.companies.clone() else {
            self.end_sign_in(id, Outcome::Failed("no Company is declared".into()));
            return;
        };
        let tx = self.events_tx.clone();
        let id = id.to_string();
        std::thread::Builder::new()
            .name(format!("company-sign-in:{}", ask.company))
            .spawn(move || {
                let send = |step: Step| {
                    let _ = tx.send(super::events::Event::CompanySignIn {
                        id: id.clone(),
                        step,
                    });
                };
                let started = sup
                    .begin_sign_in(&ask, &id, unanswerable)
                    .map(|(label, s)| {
                        send(Step::Account(label));
                        s
                    });
                let step = match started {
                    Err(e) => Step::Done(Outcome::Failed(e)),
                    Ok(Started::Held(o)) => Step::Done(o),
                    Ok(Started::Direct {
                        question,
                        requested,
                    }) => Step::Gate {
                        question,
                        kind: SignIn::Direct,
                        requested,
                        for_ms: signin::DIRECT_FOR.as_millis() as u64,
                    },
                    Ok(Started::Device {
                        question,
                        requested,
                        poll,
                    }) => {
                        send(Step::Gate {
                            question,
                            kind: SignIn::Device,
                            requested,
                            for_ms: poll.expires_in().as_millis() as u64,
                        });
                        Step::Done(sup.poll_device(poll, &cancel))
                    }
                };
                send(step);
            })
            .ok();
    }

    /// A sign-in moved on: its gate opens, or it ended.
    pub(crate) fn on_company_sign_in(&mut self, id: &str, step: Step) {
        match step {
            Step::Account(label) => {
                if let Some(t) = self.company_sign_ins.get_mut(id) {
                    t.account = Some(label);
                }
            }
            Step::Done(outcome) => self.end_sign_in(id, outcome),
            Step::Gate {
                question,
                kind,
                requested,
                for_ms,
            } => {
                let Some(t) = self.company_sign_ins.get_mut(id) else {
                    // Ended meanwhile: cancelled, or its asker gone.
                    return;
                };
                t.kind = Some(kind);
                t.requested = requested.clone();
                t.deadline_ms = Some(now_ms().saturating_add(for_ms));
                #[cfg(feature = "a2a")]
                self.open_sign_in_gate(id, &question);
                #[cfg(not(feature = "a2a"))]
                {
                    let _ = question;
                    self.end_sign_in(
                        id,
                        Outcome::Failed("no A2A listener to ask the User on".into()),
                    );
                }
            }
        }
    }

    /// The task a Company gate opens on: `on`, the asker's own — or, when no
    /// caller owns the work that asks, a task of its own, owned by whom the
    /// work runs for, else the operator: for a workflow step's call
    /// (`target`), the run's task, which the run's completion settles; else
    /// one in the conversation `ctx`.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_gate_task(
        &mut self,
        on: Option<String>,
        target: Option<&Target>,
        principal: Option<&str>,
        ctx: &str,
    ) -> String {
        use crate::a2a::tasks::Link;
        if let Some(task) = on {
            return task;
        }
        let principal = crate::a2a::Principal {
            id: principal.unwrap_or("operator").to_string(),
            role: crate::config::settings::Role::Operator,
            grants: Vec::new(),
            rate: None,
            budget: None,
            labels: Default::default(),
            session: None,
            typed: None,
        };
        // A freshly minted id, never the listener's reservation: that
        // belongs to the task the request being served creates.
        let reserved = self.reserved_task_id.take();
        let tid = match target {
            Some(Target::Step(run, _)) => {
                let ctx = format!("run-{run}");
                let tid = self.task_create(&ctx, &principal, Link::Run { id: run.clone() }, None);
                // The run's completion drives this task terminal.
                if let Some(r) = self.runs.get_mut(run) {
                    r.task = Some(tid.clone());
                    r.touch();
                }
                tid
            }
            _ => self.task_create(
                ctx,
                &principal,
                Link::Turn {
                    ctx: ctx.to_string(),
                },
                None,
            ),
        };
        self.reserved_task_id = reserved;
        tid
    }

    /// Open the sign-in `id`'s gate with `question`: on the task it opens on,
    /// or a task of its own, addressed to the User.
    #[cfg(feature = "a2a")]
    fn open_sign_in_gate(&mut self, id: &str, question: &str) {
        use crate::a2a::tasks::{SignInGate, State};
        let Some(t) = self.company_sign_ins.get(id) else {
            return;
        };
        let (company, on, standalone, owed, principal) = (
            t.company.clone(),
            t.on.clone(),
            t.standalone,
            t.owed.clone(),
            t.principal.clone(),
        );
        let target = match &owed {
            Owed::Call(t) => Some(t.clone()),
            Owed::Task(_) => None,
        };
        let task = self.company_gate_task(
            on,
            target.as_ref(),
            principal.as_deref(),
            &format!("company-sign-in-{id}"),
        );
        if let Some(t) = self.tasks.get_mut(&task) {
            t.ask_schema = Some(sign_in_answer_schema());
            t.ask_channel = None;
            t.sign_in = Some(SignInGate {
                company: company.clone(),
                standalone,
            });
            t.transition(State::InputRequired, Some(question.to_string()));
        }
        self.task_persist(&task);
        self.task_sync(&task);
        let Some(tr) = self.company_sign_ins.get_mut(id) else {
            return;
        };
        tr.gate = Some(task.clone());
        let (kind, requested, account) = (
            tr.kind.map(SignIn::as_str),
            tr.requested.clone(),
            tr.account.clone(),
        );
        self.log.info(
            "company.sign_in.requested",
            json!({"company": company, "account": account, "kind": kind, "scopes": requested, "task": task}),
        );
        self.audit(super::audit::AuditEvent {
            action: "company.sign_in",
            target: json!({"company": company, "account": account, "kind": kind, "task": task}),
            outcome: "requested",
            principal: principal.as_deref(),
            role: None,
            request_id: None,
            sid: None,
            trace: None,
            link: None,
        });
    }

    /// The sign-in `id` ended with `outcome`: its gate closed, its poll
    /// stopped, its asker told, and the end said.
    pub(crate) fn end_sign_in(&mut self, id: &str, outcome: Outcome) {
        let Some(track) = self.company_sign_ins.remove(id) else {
            return;
        };
        track.cancel.store(true, Ordering::SeqCst);
        if let Some(sup) = &self.companies {
            sup.forget_direct(id);
        }
        let company = track.company.clone();
        let account = track.account.as_deref();
        let result = outcome.result(&company, account);
        // The gate closes, and a task of its own ends with it.
        #[cfg(feature = "a2a")]
        {
            let task = match (&track.gate, &track.owed) {
                (Some(g), _) => Some(g.clone()),
                (None, Owed::Task(t)) => Some(t.clone()),
                _ => None,
            };
            if let Some(task) = task {
                self.close_sign_in_task(
                    &task,
                    &company,
                    account,
                    track.standalone,
                    &outcome,
                    result.as_ref().ok(),
                );
            }
        }
        let kind = track.kind.map(SignIn::as_str);
        match &outcome {
            Outcome::SignedIn { scope, requested } => self.log.info(
                "company.signed_in",
                json!({"company": company, "account": account, "kind": kind, "requested": requested, "granted": scope}),
            ),
            other => self.log.info(
                "company.sign_in.failed",
                json!({"company": company, "account": account, "kind": kind, "reason": other.word(),
                       "error": match other { Outcome::Failed(e) => Some(e.as_str()), _ => None }}),
            ),
        }
        self.audit(super::audit::AuditEvent {
            action: "company.sign_in",
            target: json!({"company": company, "account": account, "kind": kind}),
            outcome: outcome.word(),
            principal: track.principal.as_deref(),
            role: None,
            request_id: None,
            sid: None,
            trace: None,
            link: None,
        });
        if track.gate.is_some() {
            crate::obs::metrics::record_company_gate(&company, "sign_in", outcome.word());
        }
        if let Owed::Call(target) = &track.owed {
            self.pending.retain(|p| p.target != *target);
            match result {
                Ok(v) => self.reply(target, v, false),
                Err(e) => self.reply(target, Value::String(e), true),
            }
        }
    }

    /// Close the sign-in gate at `company` on `task`: a task of its own ends
    /// with the outcome, one that tracks other work goes back to working,
    /// saying it.
    #[cfg(feature = "a2a")]
    fn close_sign_in_task(
        &mut self,
        task: &str,
        company: &str,
        account: Option<&str>,
        standalone: bool,
        outcome: &Outcome,
        result: Option<&Value>,
    ) {
        use crate::a2a::tasks::State;
        let Some(t) = self.tasks.get_mut(task) else {
            return;
        };
        t.sign_in = None;
        t.ask_schema = None;
        let said = outcome.said(company, account);
        if t.state.is_terminal() {
            // Its work ended first: nothing to move on.
        } else if standalone {
            if let Some(r) = result {
                t.set_result(r.clone());
            }
            let state = match outcome {
                Outcome::SignedIn { .. } => State::Completed,
                Outcome::Cancelled => State::Canceled,
                _ => State::Failed,
            };
            t.transition(state, Some(said));
        } else {
            t.transition(State::Working, Some(said));
        }
        self.task_persist(task);
        self.task_sync(task);
    }

    /// The User's answer `text` on the sign-in gate `task` (`id`'s), from
    /// `principal`: only the User's `cancel` is taken, and anything else is
    /// refused with the gate left open.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_sign_in_answer(
        &mut self,
        id: &str,
        task: &str,
        principal: &crate::a2a::Principal,
        text: &str,
        message: &Value,
    ) -> Value {
        let to = self.company_user();
        if !to.matches(principal) {
            let want = to.describe();
            self.log.info(
                "human.answer.not_addressed",
                json!({"task": task, "from": principal.id, "addressee": want}),
            );
            self.audit(super::audit::AuditEvent {
                action: "company.sign_in",
                target: json!({"task": task}),
                outcome: "not_addressed",
                principal: Some(&principal.id),
                role: None,
                request_id: None,
                sid: principal.session.as_deref(),
                trace: None,
                link: None,
            });
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!(
                    "this sign-in is for {want}; your answer was not recorded and the gate is still open"
                ),
            );
        }
        let shaped = super::human::shape_answer(text, &sign_in_answer_schema());
        if shaped["decision"] != CANCEL {
            let company = self
                .company_sign_ins
                .get(id)
                .map(|t| t.company.clone())
                .unwrap_or_default();
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!(
                    "you sign in at {company} on the Company's own page, and this closes by itself once you have; the one answer it takes is cancel"
                ),
            );
        }
        if let Some(t) = self.tasks.get_mut(task) {
            t.record_inbound(message);
        }
        self.end_sign_in(id, Outcome::Cancelled);
        self.task_reply(task)
    }

    /// A Direct Sign-In callback reached the listener: its `state` is
    /// looked up and spent here, and only one that names a sign-in goes on —
    /// the supervisor checks it and exchanges its code off the loop, and the
    /// sign-in ends with the answer. One whose `state` names none ends
    /// nothing, and costs no thread.
    pub(crate) fn on_company_callback(&mut self, query: String) {
        let Some(sup) = self.companies.clone() else {
            return;
        };
        let Some(returned) = sup.direct_returned(&query) else {
            return;
        };
        let tx = self.events_tx.clone();
        std::thread::Builder::new()
            .name("company-callback".into())
            .spawn(move || {
                let (id, outcome) = sup.direct_finish(returned);
                let _ = tx.send(super::events::Event::CompanySignIn {
                    id,
                    step: Step::Done(outcome),
                });
            })
            .ok();
    }

    /// Each tick: a sign-in whose asker is gone, or whose gate's task ended
    /// — cancelled, say — is cancelled, and one past its time ends
    /// `expired`.
    pub(crate) fn poll_company_sign_ins(&mut self) {
        if self.company_sign_ins.is_empty() {
            return;
        }
        let now = now_ms();
        let ends: Vec<(String, Outcome)> = self
            .company_sign_ins
            .iter()
            .filter_map(|(id, t)| {
                let gone = match &t.owed {
                    Owed::Call(Target::Child(node, _)) => self.children.get(*node).is_none(),
                    Owed::Call(Target::Step(run, step)) => !self
                        .runs
                        .get(run)
                        .and_then(|r| r.steps.get(step))
                        .is_some_and(|s| s.status == crate::engine::run::StepStatus::Suspended),
                    Owed::Task(task) => self.task_ended(task),
                };
                if gone || t.gate.as_deref().is_some_and(|g| self.task_ended(g)) {
                    Some((id.clone(), Outcome::Cancelled))
                } else if t.deadline_ms.is_some_and(|d| now >= d) {
                    Some((id.clone(), Outcome::Expired))
                } else {
                    None
                }
            })
            .collect();
        for (id, outcome) in ends {
            self.end_sign_in(&id, outcome);
        }
    }

    /// After a restore: every sign-in gate a previous life left open ends
    /// `interrupted`, its task saying so — the sign-in's device code or
    /// verifier went with that life.
    #[cfg(feature = "a2a")]
    pub(crate) fn interrupt_company_sign_ins(&mut self) {
        let open: Vec<(String, bool, String)> = self
            .tasks
            .values()
            .filter(|t| !t.state.is_terminal())
            .filter_map(|t| {
                t.sign_in
                    .as_ref()
                    .map(|g| (t.id.clone(), g.standalone, g.company.clone()))
            })
            .collect();
        for (task, standalone, company) in open {
            self.close_sign_in_task(
                &task,
                &company,
                None,
                standalone,
                &Outcome::Interrupted,
                None,
            );
            self.log.info(
                "company.sign_in.failed",
                json!({"company": company, "reason": Outcome::Interrupted.word(), "task": task}),
            );
        }
    }

    /// `company.sign_out {company}`, the internal tool: off the loop.
    pub(crate) fn company_sign_out_tool(
        &mut self,
        caller: &ToolCaller,
        args: &Value,
    ) -> ToolOutcome {
        match self.start_sign_out(
            args,
            Owed::Call(Self::target_of(caller)),
            caller.principal.clone(),
        ) {
            Ok(()) => ToolOutcome::Executing,
            Err(e) => ToolOutcome::Ready(Value::String(format!("company.sign_out: {e}")), true),
        }
    }

    /// Sign the User out at the Company `args` names, on a thread: the
    /// outcome goes to `owed`.
    pub(crate) fn start_sign_out(
        &mut self,
        args: &Value,
        owed: Owed,
        by: Option<String>,
    ) -> Result<(), String> {
        let name = args["company"].as_str().unwrap_or_default().to_string();
        let account = args["account"].as_str().map(str::to_string);
        let sup = self
            .companies
            .clone()
            .ok_or("no Company is declared".to_string())?;
        let company = sup.company(&name).ok_or_else(|| {
            format!("{name:?} is not a company entry (services.<name>.kind: company)")
        })?;
        let tx = self.events_tx.clone();
        std::thread::Builder::new()
            .name(format!("company-sign-out:{name}"))
            .spawn(move || {
                let outcome = sup.sign_out(&company, account.as_deref());
                let _ = tx.send(super::events::Event::CompanySignedOut {
                    owed,
                    company: name,
                    by,
                    outcome,
                });
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// A sign-out finished: said, audited, and its outcome given to whom it
    /// is owed.
    pub(crate) fn on_company_signed_out(
        &mut self,
        owed: Owed,
        company: &str,
        by: Option<&str>,
        outcome: Result<Value, crate::companies::signin::SignOutError>,
    ) {
        use crate::companies::signin::SignOutError;
        let (word, revoked) = match &outcome {
            Ok(v) if v["signed_out"] == true => ("signed_out", true),
            Ok(_) => ("not_signed_in", false),
            Err(SignOutError::Unrevoked { .. }) => ("revocation_failed", false),
            Err(SignOutError::Refused(_)) => ("refused", false),
        };
        let account = match &outcome {
            Ok(v) => v["account"].as_str().map(str::to_string),
            Err(SignOutError::Unrevoked { account, .. }) => Some(account.clone()),
            Err(SignOutError::Refused(_)) => None,
        };
        let outcome = outcome.map_err(|e| e.to_string());
        if matches!(word, "signed_out" | "revocation_failed") {
            self.log.info(
                "company.signed_out",
                json!({"company": company, "account": account, "by": by, "revoked": revoked,
                       "error": outcome.as_ref().err()}),
            );
        }
        self.audit(super::audit::AuditEvent {
            action: "company.sign_out",
            target: json!({"company": company, "account": account}),
            outcome: word,
            principal: by,
            role: None,
            request_id: None,
            sid: None,
            trace: None,
            link: None,
        });
        match owed {
            Owed::Call(target) => match outcome {
                Ok(v) => self.reply(&target, v, false),
                Err(e) => self.reply(&target, Value::String(e), true),
            },
            #[cfg(feature = "a2a")]
            Owed::Task(task) => {
                use crate::a2a::tasks::State;
                if let Some(t) = self.tasks.get_mut(&task) {
                    match &outcome {
                        Ok(v) => {
                            t.set_result(v.clone());
                            let said = match (v["signed_out"] == true, account.as_deref()) {
                                (true, Some(l)) => format!(
                                    "Signed out of the account {l:?} at {company}: its Account Token is revoked there and forgotten here."
                                ),
                                (true, None) => format!(
                                    "Signed out at {company}: the Account Token is revoked there and forgotten here."
                                ),
                                (false, Some(l)) => format!(
                                    "The User was not signed in to the account {l:?} at {company}."
                                ),
                                (false, None) => {
                                    format!("The User was not signed in at {company}.")
                                }
                            };
                            t.transition(State::Completed, Some(said));
                        }
                        Err(e) => t.transition(State::Failed, Some(e.clone())),
                    }
                }
                self.task_persist(&task);
                self.task_sync(&task);
            }
            #[cfg(not(feature = "a2a"))]
            Owed::Task(_) => {}
        }
    }
}
