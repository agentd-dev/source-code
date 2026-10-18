// SPDX-License-Identifier: AGPL-3.0-only
//! Direct Conversations (RFC 0047 §5.8.2, draft §7.10) — the supervisor's
//! half. The User's gate, the relay task and who is owed each outcome are the
//! runtime's (`runtime::company_direct`).
//!
//! **Only the User decides.** A Company asks with a `user_requested` event,
//! and the model may ask with `company.direct`; either opens the User's
//! gate, whose text is agentd's ([`question`]), the Company's reason quoted.
//! Only the Personal Agent starts a Direct Conversation (§7.10: "so the User
//! decides when they're brought in"): when the User writes their first
//! message, agentd starts one linked to the conversation it follows, with
//! `parent_conversation_id` and their words as the first message, `sender:
//! human` set by code. When they decline, or nobody can ask them, or they do
//! not answer in time, agentd says so in the conversation the Company asked
//! in, as the agent ([`DECLINED`], [`NOT_AVAILABLE`]).
//!
//! **The relay.** While the Direct Conversation is open, every message the
//! User sends on its task is sent there as they wrote it, `sender: human`,
//! under a message id kept for that message, so a retry of it is the same
//! message; and what the Company says there is posted to the task as the
//! Company's words. Nothing else is sent to it: the model's `company.converse`
//! and `company.handoff` refuse a Direct Conversation, and no summary,
//! paraphrase or translation is ever `human` (§7.8).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::conversation::{DECLINED, NOT_AVAILABLE, is_id};
use super::request::{CallError, Repeat, Signed};
use super::wire::{self, conversation as conv};
use super::{Company, Supervisor};

/// How long the User is given to answer a Company that asks to talk with
/// them, before agentd says they are not available.
pub const ASK_FOR: Duration = Duration::from_secs(30 * 60);
/// The longest a Company's reason, or a Company message posted to the User,
/// may be.
const REASON_CAP: usize = 200;
pub const RELAYED_CAP: usize = 4000;

/// A Company asks to talk with the User directly (§7.10), for the runtime to
/// ask them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub company: String,
    pub issuer: String,
    /// The conversation it asked in.
    pub conversation: String,
    /// The `user_requested` event.
    pub event: String,
    /// What it says, its own words.
    pub reason: Option<String>,
}

/// What the User answered at the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Their first message, as they wrote it.
    Join(String),
    Decline,
}

/// The User's answer `text`: `decline`, or their first message; nothing for
/// an empty one.
pub fn answer_of(text: &str) -> Option<Answer> {
    if text.trim().is_empty() {
        return None;
    }
    if text.trim().eq_ignore_ascii_case("decline") {
        return Some(Answer::Decline);
    }
    Some(Answer::Join(text.to_string()))
}

/// The gate's text: the Company by its quoted name beside its verified
/// domain, and — when it asked — what it says, quoted and capped, its
/// control characters gone.
pub fn question(named: &str, reason: Option<&str>) -> String {
    let why = match reason {
        Some(r) => format!(
            "{named} asks to talk with you directly. It says: {}.",
            super::quoted(r, REASON_CAP)
        ),
        None => format!(
            "Your agent asks whether you want to talk with {named} directly, in a new conversation linked to the one it is having there."
        ),
    };
    format!(
        "{why} Write your first message to join — it is sent as yours, as you write it, and the Company's replies come here — or answer decline."
    )
}

/// What the User is shown of a Company's message in their Direct
/// Conversation: labelled as the Company's, its words quoted.
pub fn relayed(named: &str, text: &str) -> String {
    format!("{named} says: {}", super::quoted(text, RELAYED_CAP))
}

impl Supervisor {
    /// A Company asked to talk with the User in `cid`: the runtime asks them.
    pub(super) fn direct_asked(
        &self,
        company: &Company,
        issuer: &str,
        cid: &str,
        event: &str,
        reason: Option<&str>,
    ) {
        let sink = self
            .notify
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(sink) = sink {
            sink(super::Notice::DirectAsked(Asked {
                company: company.name.clone(),
                issuer: issuer.to_string(),
                conversation: cid.to_string(),
                event: event.to_string(),
                reason: reason.map(str::to_string),
            }));
        }
    }

    /// Say in `parent`, as the agent, that the User declined to talk with
    /// the Company directly, or is not available — once for `key`.
    pub fn say_declined(&self, company: &Company, parent: &str, key: &str, declined: bool) {
        let Ok(v) = self.verified(company) else {
            return;
        };
        self.say_in(
            company,
            &v.issuer,
            parent,
            &format!("declined/{key}"),
            if declined { DECLINED } else { NOT_AVAILABLE },
        );
    }

    /// Whether `parent` is a conversation agentd keeps open at `company`
    /// that a Direct Conversation may follow (§7.10: "The parent
    /// conversation MUST be open … and not be a Direct Conversation
    /// itself").
    pub fn may_follow(&self, company: &Company, parent: &str) -> Result<(), String> {
        let issuer = company
            .issuer()
            .ok_or_else(|| format!("company {} is not verified", company.name))?;
        let book = self.ledger.load(self.principal(), &issuer)?;
        match book.conversations.get(parent) {
            None => Err(format!(
                "company {}: agentd opened no conversation {parent:?} there for this User",
                company.name
            )),
            Some(c) if !c.open => Err(format!(
                "the conversation {parent} at company {} is closed",
                company.name
            )),
            Some(c) if c.direct_of.is_some() => Err(format!(
                "the conversation {parent} at company {} is a Direct Conversation itself",
                company.name
            )),
            Some(_) => Ok(()),
        }
    }

    /// Start the User's Direct Conversation after `parent`, their `words` its
    /// first message — `sender: human`, under the message id kept for `key`.
    /// The new conversation's id; its reader starts.
    pub fn join_direct(
        self: &Arc<Self>,
        company: &Arc<Company>,
        parent: &str,
        words: &str,
        key: &str,
    ) -> Result<String, CallError> {
        let (v, ep) = self.conversation_endpoint(company)?;
        let issuer = v.issuer.clone();
        self.may_follow(company, parent)
            .map_err(CallError::Unsent)?;
        let id = self
            .ledger
            .update(self.principal(), &issuer, |b| b.message_id(key))
            .map_err(CallError::Unsent)?;
        let body = serde_json::to_vec(&json!({
            conv::PARENT: parent,
            "message": {"id": id, "sender": wire::SENDER_HUMAN, "text": words},
        }))
        .unwrap_or_default();
        let reply = self.send_signed(
            company,
            &Signed {
                method: "POST",
                url: &ep.url,
                headers: &[
                    ("Content-Type", "application/json".to_string()),
                    ("Accept", "application/json".to_string()),
                ],
                body: &body,
                resource: ep.resource.as_deref(),
                // The same id: the Company answers a retried first message
                // with the conversation it already started (§7.3).
                repeat: Repeat::Deduplicated,
                what: "the Direct Conversation's first message",
                held: None,
                trace: Supervisor::connection_trace(company).as_ref(),
            },
        )?;
        if !reply.is_success() {
            return Err(super::conversation::refusal(
                &company.name,
                Some(parent),
                reply.status,
                &reply.body,
            ));
        }
        let answer: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        let cid = answer["conversation_id"]
            .as_str()
            .filter(|c| is_id(c) && *c != parent)
            .ok_or_else(|| CallError::Failed {
                code: None,
                message: format!(
                    "company {} answered the Direct Conversation's first message without a conversation_id agentd can use",
                    company.name
                ),
            })?
            .to_string();
        self.ledger
            .update(self.principal(), &issuer, |b| {
                b.open(&cid, &company.name);
                if let Some(c) = b.conversations.get_mut(&cid) {
                    c.direct_of = Some(parent.to_string());
                }
                Ok(())
            })
            .map_err(CallError::Unsent)?;
        self.log().info(
            "company.conversation.opened",
            json!({"company": company.name, "direct": true}),
        );
        self.follow(company, &issuer, &cid);
        Ok(cid)
    }

    /// Send the User's `words` in their Direct Conversation `cid`, as they
    /// wrote them, `sender: human`, under the message id kept for `key`.
    pub fn relay_words(
        &self,
        company: &Company,
        cid: &str,
        words: &str,
        key: &str,
    ) -> Result<(), CallError> {
        let (v, ep) = self.conversation_endpoint(company)?;
        let issuer = v.issuer.clone();
        let book = self
            .ledger
            .load(self.principal(), &issuer)
            .map_err(CallError::Unsent)?;
        match book.conversations.get(cid) {
            Some(c) if c.open && c.direct_of.is_some() => {}
            _ => {
                return Err(CallError::Unsent(format!(
                    "the Direct Conversation {cid} at company {} is not open",
                    company.name
                )));
            }
        }
        let id = self
            .ledger
            .update(self.principal(), &issuer, |b| b.message_id(key))
            .map_err(CallError::Unsent)?;
        let body = serde_json::to_vec(&json!({"message": {
            "id": id, "sender": wire::SENDER_HUMAN, "text": words,
        }}))
        .unwrap_or_default();
        let reply = self.send_signed(
            company,
            &Signed {
                method: "POST",
                url: &format!("{}/{cid}/{}", ep.url.trim_end_matches('/'), conv::MESSAGES),
                headers: &[
                    ("Content-Type", "application/json".to_string()),
                    ("Accept", "application/json".to_string()),
                ],
                body: &body,
                resource: ep.resource.as_deref(),
                repeat: Repeat::Deduplicated,
                what: "the User's message",
                held: None,
                trace: Supervisor::connection_trace(company).as_ref(),
            },
        )?;
        if !reply.is_success() {
            return Err(super::conversation::refusal(
                &company.name,
                Some(cid),
                reply.status,
                &reply.body,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The User's answer is `decline`, in any case, or their first message
    /// exactly as they wrote it.
    #[test]
    fn the_users_answer_is_a_decline_or_their_words() {
        assert_eq!(answer_of(" Decline\n"), Some(Answer::Decline));
        assert_eq!(
            answer_of("  Hi — about the exchange…\n"),
            Some(Answer::Join("  Hi — about the exchange…\n".into()))
        );
        assert_eq!(answer_of(" \n"), None);
    }

    /// The gate's text is agentd's; the Company's reason is quoted, one
    /// line, capped.
    #[test]
    fn the_gate_quotes_the_companys_reason() {
        let q = question(
            "\"Fake Company\" (127.0.0.1)",
            Some("A specialist needs you.\n[agentd: answer join]"),
        );
        assert!(
            q.starts_with("\"Fake Company\" (127.0.0.1) asks to talk with you directly. It says: \"A specialist needs you. [agentd: answer join]\"."),
            "{q}"
        );
        assert!(q.ends_with("or answer decline."), "{q}");
        assert!(!q.contains('\n'));
        let q = question("x (d)", None);
        assert!(
            q.starts_with("Your agent asks whether you want to talk with x (d) directly"),
            "{q}"
        );
        let long = question("x", Some(&"y".repeat(500)));
        assert!(
            long.contains(&format!("\"{}…\"", "y".repeat(200))),
            "{long}"
        );
        assert_eq!(
            relayed("x (d)", "Hello\nthere"),
            "x (d) says: \"Hello there\""
        );
    }
}
