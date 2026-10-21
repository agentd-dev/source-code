// SPDX-License-Identifier: AGPL-3.0-only
//! **`AUTH_REQUIRED` at admission** (RFC 0044 §5.4 Door 2, §5.9): a caller
//! an inbound access token named asks for a workflow that statically
//! declares a delegated server — an `mcp.tool` or `mcp.resource` step that
//! names it — and holds no usable grant there, while a template would admit
//! them. Before any step runs, the task moves to `TASK_STATE_AUTH_REQUIRED`
//! with a single-use start link on the listener origin; the person signs in
//! at the IdP directly to agentd (A2A §7.6.1, §7.6.3), the sign-in is
//! bound to that very person — the token's subject, through
//! `equivalent_issuers` — and kept as their grant, and the run the task
//! asked for starts then, without a follow-up message.
//!
//! The decision reads the typed principal and the definition alone, never
//! the message. A delegated **call** that finds no grant never asks: it
//! fails `not_enrolled`, so a steered model cannot prompt anyone to sign in
//! (T33). Offers are paced per principal ([`OFFERS_PER_WINDOW`]).
//!
//! What the sign-in obtains authorizes the run it resumes and nothing more
//! (A2A §7.6.4): no credential reaches the caller, every later message on
//! the task is authenticated by its own token, and a message on the task
//! while it waits answers nothing — it is refused, and the sign-in still
//! waits. A held start lives in memory: a restart, or a sign-in not done
//! within [`crate::a2a::login::SIGN_IN_FOR`], fails the task. Cancelling
//! the task drops the held work and ends the sign-in: its link and its
//! callback start nothing.

use serde_json::{Value, json};

use super::reactor::Runtime;
use crate::a2a::Principal;
use crate::a2a::tasks::State;
use crate::delegation::{Acting, Code};
use crate::obs::trace::TraceContext;
use crate::state::now_ms;

/// The most admission sign-ins offered one principal in a window.
pub(crate) const OFFERS_PER_WINDOW: usize = 3;
/// The window [`OFFERS_PER_WINDOW`] counts over.
pub(crate) const OFFER_WINDOW_MS: u64 = 10 * 60 * 1000;

/// An inbox event held until its caller signs in: what starts the work
/// once the enrolment is kept.
pub(crate) struct HeldStart {
    pub kind: &'static str,
    pub principal: String,
    pub session: Option<String>,
    pub payload: Value,
    pub parent: Option<TraceContext>,
    /// The event is the task's turn (an `a2a` start's message), which the
    /// task follows by its inbox id.
    pub links_event: bool,
    pub idp: String,
    pub until_ms: u64,
}

/// Every server a workflow's steps name by `server:` — at any depth of a
/// `foreach`, `parallel` or `branch` body.
fn named_servers(
    steps: &std::collections::BTreeMap<String, crate::engine::model::Step>,
    out: &mut Vec<String>,
) {
    for s in steps.values() {
        if matches!(s.kind.as_str(), "mcp.tool" | "mcp.resource")
            && let Some(server) = s.spec.get("server").and_then(Value::as_str)
            && !out.iter().any(|x| x == server)
        {
            out.push(server.to_string());
        }
        if let Some(b) = &s.body {
            named_servers(&b.steps, out);
        }
        for b in s.branches.values() {
            named_servers(&b.steps, out);
        }
    }
}

impl Runtime {
    /// The IdP `principal` is to sign in at before workflow `workflow`
    /// starts for them, when it is to: the caller is an inbound token's,
    /// the workflow names a delegated server whose grant they lack —
    /// none, an expired one or a revoked one — and a template admits them
    /// at its IdP. `None` lets the start go ahead; whatever else keeps a
    /// call from being made is the call's to say.
    pub(crate) fn admission_needs(&self, principal: &Principal, workflow: &str) -> Option<String> {
        let sid = principal.session.as_deref()?;
        if !sid.starts_with(crate::a2a::INBOUND_SID_PREFIX) {
            return None;
        }
        let d = self.delegation.as_ref()?;
        let wf = self.workflows.get(workflow)?;
        let mut servers = Vec::new();
        named_servers(&wf.steps, &mut servers);
        let acting = Acting::new(Some(&principal.id), Some(sid));
        let who = d.who(acting).ok()?;
        for server in servers {
            let Some(s) = d.servers.get(&server) else {
                continue;
            };
            // An exchange of the caller's own token needs no grant.
            if s.kind
                == crate::delegation::ServerKind::Exchange(
                    crate::config::settings::delegation::ExchangeSubject::Inbound,
                )
            {
                continue;
            }
            match d.admit(acting, &server) {
                Ok(_) => continue,
                Err(e)
                    if matches!(
                        e.code,
                        Code::NotEnrolled | Code::NeedsReenroll | Code::Revoked
                    ) && d.eligible(&who, &s.idp).is_ok() =>
                {
                    return Some(s.idp.clone());
                }
                Err(_) => continue,
            }
        }
        None
    }

    /// Hold `held` — the start of `task`'s work — until its caller signs in
    /// at `held.idp`: a single-use start link on the listener origin, the
    /// task in `TASK_STATE_AUTH_REQUIRED` saying where, in agentd's words.
    /// `Err` is why no sign-in can be offered (paced, no sign-in served);
    /// the caller then starts the work as it would have.
    pub(crate) fn hold_for_sign_in(&mut self, task: &str, held: HeldStart) -> Result<(), String> {
        let now = now_ms();
        let offered = self
            .admission_offers
            .entry(held.principal.clone())
            .or_default();
        offered.retain(|t| now.saturating_sub(*t) < OFFER_WINDOW_MS);
        if offered.len() >= OFFERS_PER_WINDOW {
            return Err(format!(
                "{} was offered {OFFERS_PER_WINDOW} sign-ins in the last {}m",
                held.principal,
                OFFER_WINDOW_MS / 60_000
            ));
        }
        let sid = held
            .session
            .clone()
            .ok_or("the caller acts through no inbound session")?;
        let authority = self
            .a2a_serving
            .as_ref()
            .and_then(|s| s.authority.clone())
            .ok_or("this listener serves no sign-in")?;
        let (Some(signins), Some(issuer)) = (authority.signins(), authority.issuer()) else {
            return Err("this listener serves no sign-in at the IdP".into());
        };
        let link = signins.begin(
            issuer,
            crate::a2a::login::Purpose::Admit {
                sid,
                task: task.to_string(),
            },
            &held.idp,
            None,
        )?;
        offered.push(now);
        let text = admission_text(&held.idp, &link.uri, link.expires_in);
        if let Some(t) = self.tasks.get_mut(task) {
            t.sign_in_link = Some(json!({
                "idp": held.idp, "start_uri": link.uri,
                "expires_at": now + link.expires_in * 1000,
            }));
            t.security_gate = true;
            t.transition(State::AuthRequired, Some(text));
        }
        self.task_persist(task);
        self.task_sync(task);
        self.log.info(
            "a2a.task.auth_required",
            json!({"task": task, "principal": held.principal, "idp": held.idp,
                   "why": "admission", "expires_in": link.expires_in}),
        );
        let held = HeldStart {
            until_ms: now + crate::a2a::login::SIGN_IN_FOR.as_millis() as u64,
            ..held
        };
        self.held_starts.insert(task.to_string(), held);
        Ok(())
    }

    /// The sign-in task `task`'s admission waited on came back: its
    /// enrolment kept, the held work starts and the task goes on — or the
    /// task fails, saying why.
    pub(crate) fn admission_signed_in(&mut self, task: &str, result: Result<String, String>) {
        let Some(held) = self.held_starts.remove(task) else {
            return;
        };
        // A task that ended while its caller signed in — cancelled, say —
        // starts nothing: the enrolment is kept, the work is not done.
        if self.tasks.get(task).is_none_or(|t| t.state.is_terminal()) {
            self.log.info(
                "a2a.task.auth_required",
                json!({"task": task, "principal": held.principal, "decision": "ended"}),
            );
            return;
        }
        if let Some(t) = self.tasks.get_mut(task) {
            t.sign_in_link = None;
            t.security_gate = false;
        }
        match result {
            Ok(said) => {
                self.log.info(
                    "a2a.task.auth_required",
                    json!({"task": task, "principal": held.principal, "decision": "signed_in"}),
                );
                if let Some(t) = self.tasks.get_mut(task) {
                    t.transition(State::Working, Some(said));
                }
                self.release_held(task, held);
            }
            Err(e) => {
                self.log.info(
                    "a2a.task.auth_required",
                    json!({"task": task, "principal": held.principal, "decision": "refused", "why": e}),
                );
                self.held_failed(
                    task,
                    &format!("the sign-in this work waited on was refused: {e}"),
                );
            }
        }
    }

    /// `task` was cancelled while its start was held: the held work is
    /// dropped and the sign-in it waited on ended, so its link and its
    /// callback start nothing.
    pub(crate) fn cancel_held(&mut self, task: &str) {
        let Some(held) = self.held_starts.remove(task) else {
            return;
        };
        if let Some(signins) = self
            .a2a_serving
            .as_ref()
            .and_then(|s| s.authority.as_ref())
            .and_then(|a| a.signins())
        {
            signins.end_where(
                |p| matches!(p, crate::a2a::login::Purpose::Admit { task: t, .. } if t == task),
                "the task this sign-in was for was cancelled",
            );
        }
        if let Some(t) = self.tasks.get_mut(task) {
            t.sign_in_link = None;
            t.security_gate = false;
        }
        self.log.info(
            "a2a.task.auth_required",
            json!({"task": task, "principal": held.principal, "decision": "cancelled"}),
        );
    }

    /// Start the work `held` holds, as it would have started at admission.
    fn release_held(&mut self, task: &str, held: HeldStart) {
        if self.tasks.get(task).is_none_or(|t| t.state.is_terminal()) {
            return;
        }
        match self.accept_event_through(
            held.kind,
            Some(held.principal),
            held.session,
            held.payload,
            held.parent.as_ref(),
        ) {
            Ok((inbox_id, trace)) => {
                if held.links_event {
                    self.event_to_task.insert(inbox_id, task.to_string());
                }
                if let Some(t) = self.tasks.get_mut(task) {
                    t.transition(State::Working, None);
                    t.set_trace(trace);
                }
                self.task_persist(task);
                self.task_sync(task);
            }
            Err(e) => self.held_failed(task, &format!("the work could not start: inbox: {e}")),
        }
    }

    fn held_failed(&mut self, task: &str, why: &str) {
        if let Some(t) = self.tasks.get_mut(task)
            && !t.state.is_terminal()
        {
            t.sign_in_link = None;
            t.security_gate = false;
            t.transition(State::Failed, Some(why.to_string()));
        }
        self.task_persist(task);
        self.task_sync(task);
    }

    /// Every pass: a held start whose sign-in was not done in time fails
    /// its task.
    pub(crate) fn expire_admissions(&mut self) {
        let now = now_ms();
        let late: Vec<String> = self
            .held_starts
            .iter()
            .filter(|(_, h)| now >= h.until_ms)
            .map(|(t, _)| t.clone())
            .collect();
        for task in late {
            self.held_starts.remove(&task);
            self.held_failed(
                &task,
                "the sign-in this work waited on was not done in time; ask again",
            );
        }
    }

    /// At start: a task the last life left waiting on an admission sign-in
    /// held nothing that survived the restart, so it fails, saying so. A
    /// step-up's is its consent gate's, which the gates' own rebuild ends.
    pub(crate) fn end_restored_admissions(&mut self) {
        let waiting: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, t)| {
                t.state == State::AuthRequired
                    && t.consent_gate.is_none()
                    && t.sign_in_link.is_some()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for task in waiting {
            self.held_failed(
                &task,
                "a restart ended the sign-in this work waited on; ask again",
            );
        }
    }
}

/// What a task waiting on an admission sign-in says: agentd's own words.
fn admission_text(idp: &str, uri: &str, expires_in: u64) -> String {
    format!(
        "Sign in to continue: this work acts with your grant at {idp}, and you hold none yet. Open {uri} in your browser within {expires_in}s and sign in there; agentd binds the sign-in to you alone, and the work goes on by itself — no message answers this."
    )
}
