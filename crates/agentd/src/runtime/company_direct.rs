// SPDX-License-Identifier: AGPL-3.0-only
//! The User talks with a Company themselves (RFC 0047 §5.8.2, draft §7.10)
//! — the runtime's half: the User's gate, the relay task, and who is owed
//! each outcome. What is sent to the Company is the supervisor's
//! (`companies::direct`), on a thread of its own, never on the loop.
//!
//! **Who asks.** A Company, with a `user_requested` event in a
//! conversation — at most one question per conversation at a time — or the
//! model, with `company.direct {company, conversation}` in the User's work.
//! Either opens the same gate, on a task of its own owned by the User:
//! addressed to the configured User, its text agentd's with the Company's
//! reason quoted, answered by the User alone, never by a judge or an
//! operator who is not the User. A Company's question is asked when the A2A
//! listener serves and `agent.ask_human_unowned: gate` lets work nobody
//! owns ask; otherwise agentd says in the conversation that the User is not
//! available. A `user_requested` in the User's own Direct Conversation asks
//! nothing (`companies::conversation`). The model's call is pending while
//! the gate is open, and answers `joined` with the Direct Conversation's
//! id, `declined`, `unanswered`, `interrupted`, or `closed` when the User
//! cancelled the task while their first message was on its way.
//!
//! **The answer** is the User's first message, which starts the Direct
//! Conversation with it as written, `sender: human`; or `decline`, which
//! agentd says in the conversation the Company asked in, as does a question
//! nobody answers in 30 minutes, one whose task is cancelled, and one a
//! restart finds open. While the first message is on its way the task takes
//! no other — one sent then is refused, to be sent again in a moment — and
//! the message is kept on the task, so a restart sends it again under the
//! id it was sent with, which the Company answers with the conversation it
//! started (§7.3).
//!
//! **The relay.** The gate's task becomes the Direct Conversation's: every
//! message the User sends on it is sent to the Company as they wrote it,
//! `sender: human`, and everything the Company says there is posted to it,
//! labelled as the Company's. It ends when the Direct Conversation closes —
//! the Company's `state` event, or the parent's `direct_closed` — and
//! cancelling the task closes it at the Company. A relay outlives a restart:
//! its task names the Direct Conversation, whose reader resumes.

use serde_json::{Value, json};

use super::reactor::{PendingKind, Runtime, Target};
use super::tools::{ToolCaller, ToolOutcome};
#[cfg(feature = "a2a")]
use crate::companies::direct;
use crate::companies::direct::Asked;
use crate::state::now_ms;

/// One Direct Conversation the User is asked about, or talks in.
pub(crate) struct DirectTrack {
    pub company: String,
    /// The conversation it follows.
    pub parent: String,
    /// The Company's `user_requested` event, when the Company asked.
    pub event: Option<String>,
    /// The model's `company.direct` call, when the model asked.
    pub owed: Option<Target>,
    /// The gate's task, then the relay's.
    pub task: String,
    /// Past this a question nobody answered ends: the User is not available.
    pub deadline_ms: Option<u64>,
    /// The Direct Conversation, once the User joined.
    pub joined: Option<String>,
    /// The User's first message is on its way.
    pub joining: bool,
    /// The User cancelled the task while it was: the Direct Conversation
    /// it starts is closed.
    pub closing: bool,
    /// The label of the User's account the model's `company.direct` named:
    /// the one the conversation it follows belongs to, or — when that one
    /// belongs to none yet — the one it goes in.
    pub account: Option<String>,
}

/// What a Direct Conversation's thread hands back to the loop.
#[derive(Debug, Clone)]
pub enum DirectStep {
    /// The User's first message started the Direct Conversation, or the
    /// Company refused it.
    Joined(Result<String, String>),
    /// A message the User wrote in it did not reach the Company.
    Undelivered(String),
}

impl Runtime {
    /// The Direct Conversation whose question is open on `task`.
    #[cfg(feature = "a2a")]
    pub(crate) fn direct_gate_on(&self, task: &str) -> Option<String> {
        self.company_directs
            .iter()
            .find(|(_, t)| t.task == task && t.joined.is_none() && !t.joining)
            .map(|(id, _)| id.clone())
    }

    /// The Direct Conversation `task`'s first message is starting.
    #[cfg(feature = "a2a")]
    pub(crate) fn direct_joining_on(&self, task: &str) -> Option<String> {
        self.company_directs
            .iter()
            .find(|(_, t)| t.task == task && t.joined.is_none() && t.joining)
            .map(|(id, _)| id.clone())
    }

    /// A message on `task` while its first message is on its way: refused,
    /// to be sent again once the conversation has started — never a turn of
    /// the model's.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_direct_joining(
        &self,
        id: &str,
        task: &str,
        principal: &crate::a2a::Principal,
    ) -> Value {
        if let Some(refused) = self.not_the_user(task, principal, "company.direct") {
            return refused;
        }
        let named = self
            .company_directs
            .get(id)
            .map(|t| self.company_named(&t.company))
            .unwrap_or_default();
        super::a2a_server::err_obj(
            ::mcp::rpc::INVALID_PARAMS,
            &format!(
                "the conversation with {named} is starting with your first message; this one was not sent — send it again in a moment"
            ),
        )
    }

    /// The Direct Conversation relayed through `task`.
    #[cfg(feature = "a2a")]
    pub(crate) fn direct_relay_on(&self, task: &str) -> Option<String> {
        self.company_directs
            .iter()
            .find(|(_, t)| t.task == task && t.joined.is_some())
            .map(|(id, _)| id.clone())
    }

    /// The Direct Conversations on `task`: what cancelling it ends.
    #[cfg(feature = "a2a")]
    pub(crate) fn directs_on(&self, task: &str) -> Vec<String> {
        self.company_directs
            .iter()
            .filter(|(_, t)| t.task == task)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// `company.direct {company, conversation}`: ask the User whether to talk
    /// with the Company directly, after `conversation` — pending until they
    /// answer.
    pub(crate) fn company_direct_tool(&mut self, caller: &ToolCaller, args: &Value) -> ToolOutcome {
        let refuse =
            |e: String| ToolOutcome::Ready(Value::String(format!("company.direct: {e}")), true);
        let Some(sup) = self.companies.clone() else {
            return refuse("no Company is declared".into());
        };
        let name = args["company"].as_str().unwrap_or_default().to_string();
        if sup.company(&name).is_none() {
            return refuse(format!(
                "{name:?} is not a company entry (services.<name>.kind: company)"
            ));
        }
        let parent = args["conversation"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !crate::companies::conversation::is_id(&parent) {
            return refuse(format!("{parent:?} is not a conversation id"));
        }
        // A Direct Conversation goes in the account of the one it follows,
        // or the one named while that belongs to none: an account it cannot
        // go in is refused before the User is asked.
        let account = args["account"].as_str().map(str::to_string);
        if let Some(c) = sup.company(&name)
            && let Err(e) = sup.may_follow_in(&c, &parent, account.as_deref())
        {
            return refuse(e);
        }
        let channel = self.human_channel(caller);
        #[cfg(feature = "a2a")]
        let askable = match &channel {
            super::human::HumanChannel::Owned(_) => true,
            super::human::HumanChannel::Unowned => {
                self.settings.agent.ask_human_unowned
                    == crate::config::settings::AskHumanUnowned::Gate
            }
            super::human::HumanChannel::None => false,
        };
        #[cfg(not(feature = "a2a"))]
        let askable = false;
        if !askable {
            return refuse(format!(
                "only the User decides whether to talk with company {name} directly, and nobody can ask them ({})",
                channel.why_unanswerable()
            ));
        }
        if self
            .company_directs
            .values()
            .any(|t| t.company == name && t.parent == parent)
        {
            return refuse(format!(
                "the User is already asked about, or in, a Direct Conversation after {parent} at company {name}"
            ));
        }
        let id = format!("{}@{}", self.next_id("direct"), self.process_nonce);
        let target = Self::target_of(caller);
        if !self.open_direct_gate(&id, &name, &parent, None, None, Some(target)) {
            return refuse("no A2A listener to ask the User on".into());
        }
        if let Some(t) = self.company_directs.get_mut(&id) {
            t.account = account;
        }
        ToolOutcome::Deferred(PendingKind::CompanyDirect { id, company: name })
    }

    /// A Company asked to talk with the User (§7.10): their gate, unless one
    /// is open for the conversation already.
    pub(crate) fn on_company_direct_asked(&mut self, a: Asked) {
        if self
            .company_directs
            .values()
            .any(|t| t.company == a.company && t.parent == a.conversation)
        {
            self.log.info(
                "company.direct.repeated",
                json!({"company": a.company, "note": "a question about this conversation is open already"}),
            );
            return;
        }
        let id = format!("{}@{}", self.next_id("direct"), self.process_nonce);
        if !self.open_direct_gate(
            &id,
            &a.company,
            &a.conversation,
            Some(a.event.clone()),
            a.reason.as_deref(),
            None,
        ) {
            self.say_unavailable(&a.company, &a.conversation, &a.event);
        }
    }

    /// Open the question `id` — whether the User talks with `company`
    /// directly after `parent` — on a task of its own owned by the User.
    /// Whether it opened: without an A2A listener nobody can be asked.
    fn open_direct_gate(
        &mut self,
        id: &str,
        company: &str,
        parent: &str,
        event: Option<String>,
        reason: Option<&str>,
        owed: Option<Target>,
    ) -> bool {
        #[cfg(feature = "a2a")]
        {
            use crate::a2a::tasks::{CompanyGate, CompanyGateKind, State};
            if self.a2a_sink.is_none() {
                return false;
            }
            let Some(sup) = self.companies.clone() else {
                return false;
            };
            let named = sup
                .company(company)
                .map(|c| c.named())
                .unwrap_or_else(|| crate::companies::quoted(company, 64));
            let user = self.settings.security.companies.user().to_string();
            let task =
                self.company_gate_task(None, None, Some(&user), &format!("company-direct-{id}"));
            if let Some(t) = self.tasks.get_mut(&task) {
                t.ask_schema = None;
                t.ask_channel = None;
                t.company_gate = Some(CompanyGate {
                    company: company.to_string(),
                    kind: CompanyGateKind::Direct {
                        conversation: parent.to_string(),
                        joining: None,
                        joined: None,
                    },
                    standalone: true,
                });
                t.transition(State::InputRequired, Some(direct::question(&named, reason)));
            }
            self.task_persist(&task);
            self.task_sync(&task);
            self.company_directs.insert(
                id.to_string(),
                DirectTrack {
                    company: company.to_string(),
                    parent: parent.to_string(),
                    event,
                    owed,
                    task: task.clone(),
                    deadline_ms: Some(now_ms() + direct::ASK_FOR.as_millis() as u64),
                    joined: None,
                    joining: false,
                    closing: false,
                    account: None,
                },
            );
            self.log.info(
                "company.direct.requested",
                json!({"company": company, "task": task, "by": if reason.is_some() { "company" } else { "agent" }}),
            );
            self.audit(super::audit::AuditEvent {
                action: "company.direct",
                target: json!({"company": company, "task": task}),
                outcome: "asked",
                principal: None,
                role: None,
                request_id: None,
                sid: None,
                trace: None,
                link: None,
            });
            true
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = (id, company, parent, event, reason, owed);
            false
        }
    }

    /// Say in `parent` at `company` that the User is not available — once
    /// for `key` — off the loop.
    fn say_unavailable(&self, company: &str, parent: &str, key: &str) {
        self.say_off_loop(company, parent, key, false);
    }

    fn say_off_loop(&self, company: &str, parent: &str, key: &str, declined: bool) {
        let Some(sup) = self.companies.clone() else {
            return;
        };
        let Some(company) = sup.company(company) else {
            return;
        };
        let (parent, key) = (parent.to_string(), key.to_string());
        std::thread::Builder::new()
            .name(format!("company-direct:{}", company.name))
            .spawn(move || sup.say_declined(&company, &parent, &key, declined))
            .ok();
    }

    /// The User's answer `text` on the question `task` (`id`'s), from
    /// `principal`: their first message, or `decline`; anything from anyone
    /// else is refused with the question left open.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_direct_answer(
        &mut self,
        id: &str,
        task: &str,
        principal: &crate::a2a::Principal,
        text: &str,
        message: &Value,
    ) -> Value {
        use crate::a2a::tasks::State;
        if let Some(refused) = self.not_the_user(task, principal, "company.direct") {
            return refused;
        }
        let Some(answer) = direct::answer_of(text) else {
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "write your first message to the Company, or answer decline; the question is still open",
            );
        };
        if let Some(t) = self.tasks.get_mut(task) {
            t.record_inbound(message);
        }
        let Some(t) = self.company_directs.get(id) else {
            return self.task_reply(task);
        };
        let (company, parent, event) = (t.company.clone(), t.parent.clone(), t.event.clone());
        let word = match &answer {
            direct::Answer::Join(_) => "join",
            direct::Answer::Decline => "decline",
        };
        self.log.info(
            "company.direct.asked",
            json!({"company": company, "answer": word, "by": principal.id}),
        );
        self.audit(super::audit::AuditEvent {
            action: "company.direct",
            target: json!({"company": company, "task": task}),
            outcome: word,
            principal: Some(&principal.id),
            role: None,
            request_id: None,
            sid: principal.session.as_deref(),
            trace: None,
            link: None,
        });
        crate::obs::metrics::record_company_gate(&company, "direct", word);
        match answer {
            direct::Answer::Decline => {
                let key = event.unwrap_or_else(|| id.to_string());
                self.say_off_loop(&company, &parent, &key, true);
                let named = self.company_named(&company);
                if let Some(rec) = self.tasks.get_mut(task) {
                    rec.company_gate = None;
                    rec.transition(
                        State::Completed,
                        Some(format!(
                            "You declined to talk with {named} directly; agentd said so in the conversation."
                        )),
                    );
                }
                self.task_persist(task);
                self.task_sync(task);
                self.end_direct(
                    id,
                    json!({"company": company, "conversation": parent, "outcome": "declined"}),
                    false,
                );
            }
            direct::Answer::Join(words) => {
                if let Some(t) = self.company_directs.get_mut(id) {
                    t.joining = true;
                    t.deadline_ms = None;
                }
                if let Some(rec) = self.tasks.get_mut(task) {
                    if let Some(g) = rec.company_gate.as_mut() {
                        g.kind = crate::a2a::tasks::CompanyGateKind::Direct {
                            conversation: parent.clone(),
                            joining: Some(words.clone()),
                            joined: None,
                        };
                    }
                    rec.transition(
                        State::Working,
                        Some("Starting the conversation with your message…".into()),
                    );
                }
                self.task_persist(task);
                self.task_sync(task);
                self.start_join(id, task, &company, &parent, words);
            }
        }
        self.task_reply(task)
    }

    /// Start the Direct Conversation `id` after `parent` at `company` with
    /// the User's `words`, off the loop: under the message id kept for its
    /// task, the same however often it is sent.
    #[cfg(feature = "a2a")]
    fn start_join(&self, id: &str, task: &str, company: &str, parent: &str, words: String) {
        let Some(sup) = self.companies.clone() else {
            return;
        };
        let Some(c) = sup.company(company) else {
            return;
        };
        let tx = self.events_tx.clone();
        let account = self.company_directs.get(id).and_then(|t| t.account.clone());
        let (id, key, parent) = (id.to_string(), format!("direct/{task}"), parent.to_string());
        std::thread::Builder::new()
            .name(format!("company-direct:{}", c.name))
            .spawn(move || {
                let joined = sup
                    .join_direct(&c, &parent, account.as_deref(), &words, &key)
                    .map_err(|e| e.to_string());
                let _ = tx.send(super::events::Event::CompanyDirect {
                    id,
                    step: DirectStep::Joined(joined),
                });
            })
            .ok();
    }

    /// Refused unless `principal` is the configured User: their answer is
    /// presented to the Company as the User's own (RFC 0047 §5.7).
    #[cfg(feature = "a2a")]
    fn not_the_user(
        &self,
        task: &str,
        principal: &crate::a2a::Principal,
        action: &str,
    ) -> Option<Value> {
        let to = self.company_user();
        if to.matches(principal) {
            return None;
        }
        let want = to.describe();
        self.log.info(
            "human.answer.not_addressed",
            json!({"task": task, "from": principal.id, "addressee": want}),
        );
        self.audit(super::audit::AuditEvent {
            action,
            target: json!({"task": task}),
            outcome: "not_addressed",
            principal: Some(&principal.id),
            role: None,
            request_id: None,
            sid: principal.session.as_deref(),
            trace: None,
            link: None,
        });
        Some(super::a2a_server::err_obj(
            ::mcp::rpc::INVALID_PARAMS,
            &format!(
                "this conversation is {want}'s; your message was not sent and was not recorded"
            ),
        ))
    }

    /// The User's message `text` on the relay task `task` (`id`'s): sent to
    /// the Company as they wrote it, `sender: human`, under a message id
    /// kept for their A2A message, off the loop.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_direct_relay(
        &mut self,
        id: &str,
        task: &str,
        principal: &crate::a2a::Principal,
        text: &str,
        message: &Value,
    ) -> Value {
        if let Some(refused) = self.not_the_user(task, principal, "company.direct") {
            return refused;
        }
        // A message the task holds already — a client sending it again, or
        // the first message the conversation started with — is not sent
        // again.
        if let Some(mid) = message["messageId"].as_str()
            && self.tasks.get(task).is_some_and(|t| t.holds_inbound(mid))
        {
            return self.task_reply(task);
        }
        if text.trim().is_empty() {
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "the message has no text to send",
            );
        }
        if let Some(t) = self.tasks.get_mut(task) {
            t.record_inbound(message);
        }
        self.task_persist(task);
        self.task_sync(task);
        let Some(t) = self.company_directs.get(id) else {
            return self.task_reply(task);
        };
        let (Some(sup), Some(cid)) = (self.companies.clone(), t.joined.clone()) else {
            return self.task_reply(task);
        };
        let Some(company) = sup.company(&t.company) else {
            return self.task_reply(task);
        };
        let key = format!(
            "relay/{cid}/{}",
            message["messageId"].as_str().unwrap_or_default()
        );
        let (tx, id, words) = (self.events_tx.clone(), id.to_string(), text.to_string());
        std::thread::Builder::new()
            .name(format!("company-relay:{}", company.name))
            .spawn(move || {
                if let Err(e) = sup.relay_words(&company, &cid, &words, &key) {
                    let _ = tx.send(super::events::Event::CompanyDirect {
                        id,
                        step: DirectStep::Undelivered(e.to_string()),
                    });
                }
            })
            .ok();
        self.task_reply(task)
    }

    /// A Direct Conversation moved on.
    pub(crate) fn on_company_direct(&mut self, id: &str, step: DirectStep) {
        let Some(t) = self.company_directs.get_mut(id) else {
            return;
        };
        let (company, parent, task, closing) = (
            t.company.clone(),
            t.parent.clone(),
            t.task.clone(),
            t.closing,
        );
        match step {
            DirectStep::Joined(Ok(cid)) if closing => {
                // The User cancelled the task while it started: closed at
                // the Company.
                if let Some(sup) = self.companies.clone()
                    && let Some(c) = sup.company(&company)
                {
                    let cid = cid.clone();
                    std::thread::Builder::new()
                        .name(format!("company-direct-close:{company}"))
                        .spawn(move || {
                            let _ = sup.conversation_op(
                                &c,
                                &cid,
                                None,
                                crate::companies::wire::conversation::CLOSE,
                                None,
                            );
                        })
                        .ok();
                }
                self.log.info(
                    "company.direct.closed",
                    json!({"company": company, "by": "cancel"}),
                );
                self.end_direct(
                    id,
                    json!({"company": company, "conversation": parent, "outcome": "closed", "direct": cid}),
                    false,
                );
            }
            DirectStep::Joined(Ok(cid)) => {
                if let Some(t) = self.company_directs.get_mut(id) {
                    t.joining = false;
                    t.joined = Some(cid.clone());
                }
                #[cfg(feature = "a2a")]
                {
                    use crate::a2a::tasks::{CompanyGateKind, State};
                    let named = self.company_named(&company);
                    if let Some(rec) = self.tasks.get_mut(&task) {
                        if let Some(g) = rec.company_gate.as_mut() {
                            g.kind = CompanyGateKind::Direct {
                                conversation: parent.clone(),
                                joining: None,
                                joined: Some(cid.clone()),
                            };
                        }
                        rec.transition(
                            State::InputRequired,
                            Some(format!(
                                "You are talking with {named} directly. What you write here is sent to the Company as yours, as you write it, and its replies come here. Cancel this task to close the conversation."
                            )),
                        );
                    }
                    self.task_persist(&task);
                    self.task_sync(&task);
                }
                self.log.info(
                    "company.direct.joined",
                    json!({"company": company, "task": task}),
                );
                self.log.debug(
                    "company.direct.conversation",
                    json!({"company": company, "parent": parent, "conversation": cid}),
                );
                if let Some(target) = self.company_directs.get_mut(id).and_then(|t| t.owed.take()) {
                    self.pending.retain(|p| p.target != target);
                    self.reply(
                        &target,
                        json!({"company": company, "conversation": parent, "outcome": "joined", "direct": cid}),
                        false,
                    );
                }
            }
            DirectStep::Joined(Err(e)) => {
                #[cfg(feature = "a2a")]
                {
                    use crate::a2a::tasks::State;
                    let named = self.company_named(&company);
                    if let Some(rec) = self.tasks.get_mut(&task)
                        && !rec.state.is_terminal()
                    {
                        rec.company_gate = None;
                        rec.transition(
                            State::Failed,
                            Some(format!(
                                "The conversation with {named} could not be started: {e}"
                            )),
                        );
                    }
                    self.task_persist(&task);
                    self.task_sync(&task);
                }
                self.end_direct(id, Value::String(format!("company.direct: {e}")), true);
            }
            DirectStep::Undelivered(e) => {
                self.log.warn(
                    "company.direct.undelivered",
                    json!({"company": company, "error": e}),
                );
                #[cfg(feature = "a2a")]
                {
                    use crate::a2a::tasks::State;
                    let named = self.company_named(&company);
                    if let Some(rec) = self.tasks.get_mut(&task) {
                        rec.transition(
                            State::InputRequired,
                            Some(format!("Your message did not reach {named}: {e}")),
                        );
                    }
                    self.task_persist(&task);
                    self.task_sync(&task);
                }
            }
        }
    }

    /// The Company's events in the conversation `cid` at `company`: what it
    /// says in a Direct Conversation is posted to the User's relay task as
    /// its words, and the relay ends when the conversation closes.
    pub(crate) fn relay_company_events(&mut self, company: &str, cid: &str, events: &[Value]) {
        let Some((id, task)) = self
            .company_directs
            .iter()
            .find(|(_, t)| t.company == company && t.joined.as_deref() == Some(cid))
            .map(|(id, t)| (id.clone(), t.task.clone()))
        else {
            // The parent's `direct_closed` ends the relay of the Direct
            // Conversation it names.
            let closed: Vec<String> = events
                .iter()
                .filter(|e| e["type"] == crate::companies::wire::event::DIRECT_CLOSED)
                .filter_map(|e| e["conversation_id"].as_str().map(str::to_string))
                .collect();
            for d in closed {
                if let Some(id) = self
                    .company_directs
                    .iter()
                    .find(|(_, t)| {
                        t.company == company
                            && t.parent == cid
                            && t.joined.as_deref() == Some(d.as_str())
                    })
                    .map(|(id, _)| id.clone())
                {
                    self.end_relay(&id);
                }
            }
            return;
        };
        let named = self.company_named(company);
        let mut closed = false;
        for e in events {
            match e["type"].as_str() {
                Some(t)
                    if t == crate::companies::wire::event::MESSAGE
                        && e["message"]["role"] == crate::companies::wire::ROLE_COMPANY =>
                {
                    let text = e["message"]["text"].as_str().unwrap_or_default();
                    if text.is_empty() {
                        continue;
                    }
                    #[cfg(feature = "a2a")]
                    {
                        use crate::a2a::tasks::State;
                        if let Some(rec) = self.tasks.get_mut(&task) {
                            rec.transition(
                                State::InputRequired,
                                Some(direct::relayed(&named, text)),
                            );
                        }
                    }
                    #[cfg(not(feature = "a2a"))]
                    let _ = (&named, &task, text);
                }
                Some(t)
                    if t == crate::companies::wire::event::STATE
                        && e["status"] == crate::companies::wire::STATUS_CLOSED =>
                {
                    closed = true;
                }
                _ => {}
            }
        }
        #[cfg(feature = "a2a")]
        {
            self.task_persist(&task);
            self.task_sync(&task);
        }
        if closed {
            self.end_relay(&id);
        }
    }

    /// The Direct Conversation `id` closed: its relay task ends.
    fn end_relay(&mut self, id: &str) {
        let Some(t) = self.company_directs.remove(id) else {
            return;
        };
        let named = self.company_named(&t.company);
        #[cfg(feature = "a2a")]
        {
            use crate::a2a::tasks::State;
            if let Some(rec) = self.tasks.get_mut(&t.task) {
                rec.company_gate = None;
                if !rec.state.is_terminal() {
                    rec.transition(
                        State::Completed,
                        Some(format!("The conversation with {named} has closed.")),
                    );
                }
            }
            self.task_persist(&t.task);
            self.task_sync(&t.task);
        }
        #[cfg(not(feature = "a2a"))]
        let _ = named;
        self.log
            .info("company.direct.closed", json!({"company": t.company}));
    }

    /// The Direct Conversation `id` ended before it was joined: its asker,
    /// when the model asked, is told `result`.
    fn end_direct(&mut self, id: &str, result: Value, is_error: bool) {
        let Some(t) = self.company_directs.remove(id) else {
            return;
        };
        if let Some(target) = t.owed {
            self.pending.retain(|p| p.target != target);
            self.reply(&target, result, is_error);
        }
    }

    /// The User cancelled the task `task`: a question on it ends, the User
    /// not available; a Direct Conversation relayed through it is closed at
    /// the Company.
    #[cfg(feature = "a2a")]
    pub(crate) fn cancel_directs_on(&mut self, task: &str) {
        for id in self.directs_on(task) {
            let Some(t) = self.company_directs.get(&id) else {
                continue;
            };
            let (company, parent, event, joined) = (
                t.company.clone(),
                t.parent.clone(),
                t.event.clone(),
                t.joined.clone(),
            );
            let joining = t.joining;
            match joined {
                // Its first message is on its way: what it starts is closed
                // once it has.
                None if joining => {
                    if let Some(t) = self.company_directs.get_mut(&id) {
                        t.closing = true;
                    }
                }
                Some(cid) => {
                    if let Some(sup) = self.companies.clone()
                        && let Some(c) = sup.company(&company)
                    {
                        std::thread::Builder::new()
                            .name(format!("company-direct-close:{company}"))
                            .spawn(move || {
                                let _ = sup.conversation_op(
                                    &c,
                                    &cid,
                                    None,
                                    crate::companies::wire::conversation::CLOSE,
                                    None,
                                );
                            })
                            .ok();
                    }
                    self.company_directs.remove(&id);
                    self.log.info(
                        "company.direct.closed",
                        json!({"company": company, "by": "cancel"}),
                    );
                }
                None => {
                    self.say_unavailable(&company, &parent, &event.unwrap_or_else(|| id.clone()));
                    crate::obs::metrics::record_company_gate(&company, "direct", "unanswered");
                    self.end_direct(
                        &id,
                        json!({"company": company, "conversation": parent, "outcome": "unanswered"}),
                        false,
                    );
                }
            }
        }
    }

    /// Each tick: a question nobody answered in time ends — the User is not
    /// available, said in the conversation — and one whose asker is gone no
    /// longer owes it an answer.
    pub(crate) fn poll_company_directs(&mut self) {
        if self.company_directs.is_empty() {
            return;
        }
        let now = now_ms();
        let due: Vec<String> = self
            .company_directs
            .iter()
            .filter(|(_, t)| {
                t.joined.is_none() && !t.joining && t.deadline_ms.is_some_and(|d| now >= d)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            let Some(t) = self.company_directs.get(&id) else {
                continue;
            };
            let (company, parent, event, task) = (
                t.company.clone(),
                t.parent.clone(),
                t.event.clone(),
                t.task.clone(),
            );
            self.say_unavailable(&company, &parent, &event.unwrap_or_else(|| id.clone()));
            crate::obs::metrics::record_company_gate(&company, "direct", "unanswered");
            #[cfg(feature = "a2a")]
            {
                use crate::a2a::tasks::State;
                let named = self.company_named(&company);
                if let Some(rec) = self.tasks.get_mut(&task) {
                    rec.company_gate = None;
                    if !rec.state.is_terminal() {
                        rec.transition(
                            State::Failed,
                            Some(format!(
                                "{named} asked to talk with you, and the question went unanswered; agentd said you are not available."
                            )),
                        );
                    }
                }
                self.task_persist(&task);
                self.task_sync(&task);
            }
            #[cfg(not(feature = "a2a"))]
            let _ = task;
            self.end_direct(
                &id,
                json!({"company": company, "conversation": parent, "outcome": "unanswered"}),
                false,
            );
        }
        let gone: Vec<String> = self
            .company_directs
            .iter()
            .filter(|(_, t)| match &t.owed {
                Some(Target::Child(node, _)) => self.children.get(*node).is_none(),
                Some(Target::Step(run, step)) => !self
                    .runs
                    .get(run)
                    .and_then(|r| r.steps.get(step))
                    .is_some_and(|s| s.status == crate::engine::run::StepStatus::Suspended),
                None => false,
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            if let Some(t) = self.company_directs.get_mut(&id) {
                t.owed = None;
            }
        }
    }

    /// After a restore: a question a previous life left open ends — the User
    /// not available, said in the conversation — and a relay is tracked
    /// again on its task.
    #[cfg(feature = "a2a")]
    pub(crate) fn restore_company_directs(&mut self) {
        use crate::a2a::tasks::{CompanyGateKind, State};
        type Found = (String, String, String, Option<String>, Option<String>);
        let found: Vec<Found> = self
            .tasks
            .values()
            .filter(|t| !t.state.is_terminal())
            .filter_map(|t| match &t.company_gate {
                Some(g) => match &g.kind {
                    CompanyGateKind::Direct {
                        conversation,
                        joining,
                        joined,
                    } => Some((
                        t.id.clone(),
                        g.company.clone(),
                        conversation.clone(),
                        joining.clone(),
                        joined.clone(),
                    )),
                    _ => None,
                },
                None => None,
            })
            .collect();
        for (task, company, parent, joining, joined) in found {
            match (joining, joined) {
                (_, Some(cid)) => {
                    let id = format!("{}@{}", self.next_id("direct"), self.process_nonce);
                    self.company_directs.insert(
                        id,
                        DirectTrack {
                            company,
                            parent,
                            event: None,
                            owed: None,
                            task,
                            deadline_ms: None,
                            joined: Some(cid),
                            joining: false,
                            closing: false,
                            account: None,
                        },
                    );
                }
                // The User's first message was on its way: sent again under
                // the id it was sent with, so the Company answers with the
                // conversation it started, if it did (§7.3).
                (Some(words), None) => {
                    let id = format!("{}@{}", self.next_id("direct"), self.process_nonce);
                    self.company_directs.insert(
                        id.clone(),
                        DirectTrack {
                            company: company.clone(),
                            parent: parent.clone(),
                            event: None,
                            owed: None,
                            task: task.clone(),
                            deadline_ms: None,
                            joined: None,
                            joining: true,
                            closing: false,
                            account: None,
                        },
                    );
                    self.log.info(
                        "company.direct.resumed",
                        json!({"company": company, "task": task}),
                    );
                    self.start_join(&id, &task, &company, &parent, words);
                }
                (None, None) => {
                    self.say_unavailable(&company, &parent, &format!("restored/{task}"));
                    if let Some(rec) = self.tasks.get_mut(&task) {
                        rec.company_gate = None;
                        rec.transition(
                            State::Failed,
                            Some("The question was interrupted: agentd restarted before you answered, and said in the conversation that you are not available.".into()),
                        );
                    }
                    self.task_persist(&task);
                    self.task_sync(&task);
                    crate::obs::metrics::record_company_gate(&company, "direct", "interrupted");
                }
            }
        }
    }

    /// How the User is shown `company`: its quoted name beside its verified
    /// domain.
    fn company_named(&self, company: &str) -> String {
        self.companies
            .as_ref()
            .and_then(|s| s.company(company))
            .map(|c| c.named())
            .unwrap_or_else(|| crate::companies::quoted(company, 64))
    }
}
