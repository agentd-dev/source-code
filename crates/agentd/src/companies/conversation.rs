// SPDX-License-Identifier: AGPL-3.0-only
//! Conversations with a Company's agent (RFC 0047 §5.5.7, draft §7): a
//! client for the one conversation protocol the draft defines, `poppy`,
//! written against the specification because no SDK for it exists, and the
//! reader that follows each conversation agentd opens.
//!
//! Every request carries the User's DPoP Session Token and a proof
//! ([`super::request`]). A message's `id` is `msg_` and 128 random bits,
//! kept durably for the call that sends it before it is sent, so a retry —
//! the transport's, or a workflow step's `retry:` or replay after a restart,
//! whose call keeps its key — sends the same id and the Company answers it
//! as the message it already has (§7.3). Its
//! `sender` is always `agent` and `role` is never sent (§7.4, §7.8); its
//! `context` holds only what the configuration shares.
//!
//! **Reading.** The Company records each conversation as an ordered list of
//! events (§7.5), and two reads follow it, each from its own saved cursor:
//! the model's (`company.read`, and `company.converse` with `wait`), and
//! the reader — a thread per open conversation, at most
//! `security.companies.limits.conversations` in the instance, stopped when
//! the conversation closes and when the instance drains. The reader asks
//! for a stream (`Accept: text/event-stream`, §7.6) with `wait=30`, so a
//! Company that does not stream answers a long poll instead; a stream that
//! ends is opened again with `Last-Event-ID` set to the last event it
//! handled, in place of `cursor`. Each read:
//!
//! - skips event types agentd does not recognise (§7.5: "Personal Agents
//!   MUST skip event types they don't recognize") and events it handled
//!   before — a read after a reconnect can return them again;
//! - drops every `text-delta` piece unread: none reaches the model or the
//!   stream (§7.6: "It MUST NOT act on text from a `text-delta` event");
//! - never compares or sorts cursors, and saves the one it reached durably
//!   once it has handled the events before it — the reader once they are
//!   appended to the stream `company.<name>`, carrying the Company's origin;
//! - on `cursor_expired` reads again from the start, and says part of the
//!   conversation is missing (§7.5): on the log, on the stream, and to the
//!   model at its next read.
//!
//! **What the supervisor does with an event**, once, whichever read sees it
//! first: an `authorization` event (§7.11) is answered with the User's
//! Account Token when one is held whose scopes cover the event's — the
//! Session is renewed with it, which signs it in, and the conversation goes
//! on with the new token; otherwise it reaches the model as a fact, and the
//! model may call `company.sign_in`, which asks the User. The Company's
//! event never opens a gate by itself. A `user_requested` event (§7.10) is
//! answered in the conversation: the User is not available for a direct
//! conversation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::discover::{Listed, Verified};
use super::ledger::{Conversation, remember};
use super::request::{Answer, CallError, Repeat, Signed};
use super::wire::{self, conversation as conv, error as err, event};
use super::{Company, Supervisor};
use crate::config::settings::companies::Interface;

/// The longest a read or a reply is waited for, in seconds (Guides
/// #agent-conversations: "Read events with a long `wait`, such as 30
/// seconds, or a stream").
pub const MAX_WAIT: u32 = 30;
/// How long a stream may say nothing before the reader opens it again.
pub const STREAM_SILENCE: Duration = Duration::from_secs(45);
/// The longest the reader waits for its events to be appended to the stream
/// before it reads them again.
const DELIVERY_FOR: Duration = Duration::from_secs(10);
/// How many pages one read the model asks for takes while the Company says
/// it has more.
const PAGES: usize = 8;
/// The reader's wait after a failed read: a second, doubling to a minute.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// What agentd says in a conversation when the Company asks to talk with the
/// User, in P3 (§7.10: "If they decline or aren't available, it says so in
/// the conversation").
pub const NOT_AVAILABLE: &str = "The user is not available for a direct conversation.";

/// A message the model asks agentd to send.
#[derive(Debug, Clone, Default)]
pub struct Say {
    /// The conversation it goes to; none starts one.
    pub conversation: Option<String>,
    pub text: Option<String>,
    pub data: Option<Value>,
    /// What the message's `context` holds: the fields the configuration
    /// shares, in the draft's formats.
    pub context: Map<String, Value>,
    /// Seconds to wait for the Company's reply, at most [`MAX_WAIT`].
    pub wait: Option<u32>,
    /// The call's own idempotency key: a retry of the call sends the same
    /// message id.
    pub key: String,
    /// The span its `traceparent` header names
    /// ([`Supervisor::request_trace`]).
    pub trace: Option<crate::obs::trace::TraceContext>,
}

/// Events the reader appends to the stream `company.<name>`, and the
/// channel the runtime says it did on.
#[derive(Debug)]
pub struct Delivery {
    pub company: String,
    pub issuer: String,
    pub conversation: String,
    pub items: Vec<Item>,
    pub ack: std::sync::mpsc::SyncSender<Result<(), String>>,
}

/// One event for the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// Its id on the stream: the conversation's and the event's.
    pub id: String,
    /// The event's type, or `cursor_expired`.
    pub subject: String,
    /// `{company, conversation, event, note?}`.
    pub data: Value,
}

/// Where deliveries go: the runtime's loop.
pub type Sink = Arc<dyn Fn(Delivery) + Send + Sync>;

/// The running readers.
#[derive(Default)]
pub struct Readers {
    /// By `issuer|conversation`, each with the flag that stops it.
    running: BTreeMap<String, (String, Arc<AtomicBool>)>,
    /// Conversations being opened, counted against the limit until they are.
    opening: u32,
    /// The instance drains: no reader starts.
    stopped: bool,
}

/// A place for one conversation being opened; dropping it frees it.
struct Opening<'a>(&'a Supervisor);

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        let mut r = self.0.readers_lock();
        r.opening = r.opening.saturating_sub(1);
    }
}

/// Which read handled an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    Reader,
    Model,
}

/// One page of a conversation's events (§7.5), read as open issue #13 leaves
/// it: `events` required, the rest taken as given — the cursor the last
/// event's id when the page names none.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub events: Vec<Value>,
    pub cursor: Option<String>,
    pub has_more: bool,
    pub status: Value,
    pub responder: Value,
}

/// Whether `id` is an ID the draft allows (§7.2): 1 to 256 characters of the
/// URL-safe base64 alphabet. Anything else is never put in a URL.
pub fn is_id(id: &str) -> bool {
    (1..=256).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// A read's answer as a [`Page`]: refused without an `events` list.
pub fn page(v: &Value, asked: Option<&str>) -> Result<Page, String> {
    let events = v["events"]
        .as_array()
        .ok_or("the Company's read carries no events list")?
        .clone();
    let cursor = v["cursor"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            events
                .iter()
                .rev()
                .find_map(|e| e["id"].as_str().map(str::to_string))
        })
        .or_else(|| asked.map(str::to_string));
    Ok(Page {
        events,
        cursor,
        has_more: v["has_more"].as_bool().unwrap_or(false),
        status: v["status"].clone(),
        responder: v["responder"].clone(),
    })
}

/// Whether an event is one agentd hands on: an object with an id the draft
/// allows, of a type it recognises. Every other is skipped.
fn handed_on(e: &Value) -> Option<&str> {
    let id = e["id"].as_str().filter(|i| is_id(i))?;
    matches!(
        e["type"].as_str()?,
        event::MESSAGE | event::STATE | event::AUTHORIZATION | event::USER_REQUESTED
    )
    .then_some(id)
}

/// Whether `e` closes its conversation (§7.12).
fn closes(e: &Value) -> bool {
    e["type"] == event::STATE && e["status"] == wire::STATUS_CLOSED
}

/// `s`, percent-encoded for a query.
fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The URL of `tail` under the conversation `cid`, with `cursor` and `wait`.
fn url_of(ep: &Listed, cid: &str, tail: &str, cursor: Option<&str>, wait: Option<u32>) -> String {
    let mut url = format!("{}/{cid}/{tail}", ep.url.trim_end_matches('/'));
    let mut q = Vec::new();
    if let Some(c) = cursor {
        q.push(format!("{}={}", conv::CURSOR, pct(c)));
    }
    if let Some(w) = wait.filter(|w| *w > 0) {
        q.push(format!("{}={}", conv::WAIT, w.min(MAX_WAIT)));
    }
    if !q.is_empty() {
        url.push('?');
        url.push_str(&q.join("&"));
    }
    url
}

/// What a conversation request's refusal means (§7.13): the Company's error
/// code, said in agentd's words, its description quoted.
fn refusal(company: &str, cid: Option<&str>, status: u16, body: &[u8]) -> CallError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = v["error"].as_str().map(str::to_string);
    let said = v["error_description"]
        .as_str()
        .map(|d| format!(" — the Company says {}", super::quoted(d, 200)))
        .unwrap_or_default();
    let at = cid.map(|c| format!(" {c}")).unwrap_or_default();
    let message = match code.as_deref() {
        Some(err::CONVERSATION_NOT_FOUND) => format!(
            "company {company} has no conversation{at} for this User ({})",
            err::CONVERSATION_NOT_FOUND
        ),
        Some(err::CONVERSATION_CLOSED) => format!(
            "the conversation{at} at company {company} is closed and takes no more messages ({})",
            err::CONVERSATION_CLOSED
        ),
        Some(err::DIRECT_CONVERSATION_OPEN) => format!(
            "the conversation{at} at company {company} has a Direct Conversation open, and takes no messages until it closes ({})",
            err::DIRECT_CONVERSATION_OPEN
        ),
        Some(err::MESSAGE_ID_CONFLICT) => format!(
            "company {company} accepted a message with this id before, with other content ({}): a retry of a call sends the message it sent first",
            err::MESSAGE_ID_CONFLICT
        ),
        Some(err::INVALID_CURSOR) | Some(err::CURSOR_EXPIRED) | Some(err::INVALID_REQUEST) => {
            format!(
                "company {company} refused the request ({})",
                code.clone().unwrap_or_default()
            )
        }
        Some(other) => format!(
            "company {company} refused the request with {} (HTTP {status})",
            super::quoted(other, 64)
        ),
        None => format!("company {company} answered HTTP {status}"),
    };
    CallError::Failed {
        code,
        message: format!("{message}{said}"),
    }
}

impl Supervisor {
    pub(super) fn readers_lock(&self) -> std::sync::MutexGuard<'_, Readers> {
        self.readers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where the reader's events go.
    pub fn deliver_to(&self, sink: Sink) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// The conversation endpoint at `company`, its documents verified.
    fn endpoint(&self, company: &Company) -> Result<(Arc<Verified>, Listed), CallError> {
        let unsent = |e: String| CallError::Unsent(e);
        if !company
            .discovery
            .entry
            .interfaces
            .contains(&Interface::Agent)
        {
            return Err(unsent(format!(
                "company {} does not use the Company's agent (services.{}.interfaces lists no agent)",
                company.name, company.name
            )));
        }
        let v = self
            .verified(company)
            .map_err(|e| unsent(format!("company {} is unavailable: {e}", company.name)))?;
        let ep = v.conversation.clone().ok_or_else(|| {
            unsent(format!(
                "company {} lists no conversation protocol agentd speaks (type {})",
                company.name,
                wire::CONVERSATION_TYPE
            ))
        })?;
        Ok((v, ep))
    }

    /// The conversation `cid` at `issuer`, as kept: only one agentd opened
    /// is read or written to.
    fn kept(&self, company: &Company, issuer: &str, cid: &str) -> Result<Conversation, CallError> {
        let book = self
            .ledger
            .load(self.principal(), issuer)
            .map_err(CallError::Unsent)?;
        book.conversations.get(cid).cloned().ok_or_else(|| {
            CallError::Unsent(format!(
                "company {}: agentd opened no conversation {cid:?} there for this User",
                company.name
            ))
        })
    }

    /// A place for a new conversation, while fewer than the limit are open.
    fn opening(&self) -> Result<Opening<'_>, CallError> {
        let mut r = self.readers_lock();
        if r.running.len() as u32 + r.opening >= self.conversation_limit {
            return Err(CallError::Unsent(format!(
                "{} conversations are open (security.companies.limits.conversations); close one with company.close before starting another",
                self.conversation_limit
            )));
        }
        r.opening += 1;
        Ok(Opening(self))
    }

    /// `company.converse`: send `say` — a new conversation's first message,
    /// or the next one in `say.conversation` — and, with `wait`, read the
    /// Company's reply.
    pub fn converse(
        self: &Arc<Self>,
        company: &Arc<Company>,
        say: &Say,
    ) -> Result<Value, CallError> {
        if say.text.is_none() && say.data.is_none() {
            return Err(CallError::Unsent(
                "a message needs text or data (draft §7.4)".to_string(),
            ));
        }
        let (v, ep) = self.endpoint(company)?;
        let issuer = v.issuer.clone();
        let place = match &say.conversation {
            None => Some(self.opening()?),
            Some(cid) => {
                let c = self.kept(company, &issuer, cid)?;
                if !c.open {
                    return Err(CallError::Unsent(format!(
                        "the conversation {cid} at company {} is closed and takes no more messages",
                        company.name
                    )));
                }
                None
            }
        };
        let id = self
            .ledger
            .update(self.principal(), &issuer, |b| b.message_id(&say.key))
            .map_err(CallError::Unsent)?;
        let mut message = json!({"id": id, "sender": wire::SENDER_AGENT});
        if let Some(t) = &say.text {
            message["text"] = json!(t);
        }
        if let Some(d) = &say.data {
            message["data"] = d.clone();
        }
        if !say.context.is_empty() {
            message["context"] = Value::Object(say.context.clone());
        }
        let body = serde_json::to_vec(&json!({"message": message})).unwrap_or_default();
        // A new conversation's id comes back in the answer to its first
        // message, which is not asked to wait: a waiting answer is a read of
        // events, which need not name the conversation (open issue #13).
        let read_cursor = match &say.conversation {
            Some(cid) => self.kept(company, &issuer, cid)?.read_cursor,
            None => None,
        };
        let url = match &say.conversation {
            None => ep.url.clone(),
            Some(cid) => url_of(&ep, cid, conv::MESSAGES, read_cursor.as_deref(), say.wait),
        };
        let headers = [
            ("Content-Type", "application/json".to_string()),
            ("Accept", "application/json".to_string()),
        ];
        let reply = self.send_signed(
            company,
            &Signed {
                method: "POST",
                url: &url,
                headers: &headers,
                body: &body,
                resource: ep.resource.as_deref(),
                repeat: Repeat::Deduplicated,
                what: "the message",
                held: say
                    .conversation
                    .as_ref()
                    .and(say.wait)
                    .filter(|w| *w > 0)
                    .map(|w| Duration::from_secs(u64::from(w.min(MAX_WAIT)))),
                trace: say.trace.as_ref(),
            },
        )?;
        if !reply.is_success() {
            let e = refusal(
                &company.name,
                say.conversation.as_deref(),
                reply.status,
                &reply.body,
            );
            if let Some(cid) = &say.conversation {
                self.refused_on(company, &issuer, cid, &e);
            }
            return Err(e);
        }
        let answer: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        let cid = match &say.conversation {
            Some(c) => c.clone(),
            None => {
                let cid = answer["conversation_id"]
                    .as_str()
                    .filter(|c| is_id(c))
                    .ok_or_else(|| CallError::Failed {
                        code: None,
                        message: format!(
                            "company {} answered the conversation's first message without a conversation_id agentd can use",
                            company.name
                        ),
                    })?
                    .to_string();
                self.ledger
                    .update(self.principal(), &issuer, |b| {
                        b.open(&cid, &company.name);
                        Ok(())
                    })
                    .map_err(CallError::Unsent)?;
                self.log().info(
                    "company.conversation.opened",
                    json!({"company": company.name}),
                );
                self.log().debug(
                    "company.conversation.id",
                    json!({"company": company.name, "conversation": cid}),
                );
                self.follow(company, &issuer, &cid);
                drop(place);
                cid
            }
        };
        let mut out = json!({
            "company": company.name,
            "conversation": cid,
            "status": answer["status"],
            "responder": answer["responder"],
            "events": [],
            "notes": [],
        });
        match (&say.conversation, say.wait) {
            // The answer to a message sent with `wait` is a read (§7.3) —
            // from a Company that honours it.
            (Some(_), Some(w)) if w > 0 && answer["events"].is_array() => {
                let p = page(&answer, read_cursor.as_deref()).map_err(|e| CallError::Failed {
                    code: None,
                    message: format!("company {}: {e}", company.name),
                })?;
                let (events, notes) =
                    self.take_for_model(company, &issuer, &cid, p.events, p.cursor)?;
                out["events"] = json!(events);
                out["notes"] = json!(notes);
            }
            (None, Some(w)) if w > 0 => {
                let read = self.read(company, &cid, w, say.trace.as_ref())?;
                out["events"] = read["events"].clone();
                out["notes"] = read["notes"].clone();
                out["status"] = read["status"].clone();
                out["responder"] = read["responder"].clone();
            }
            _ => {}
        }
        self.missing_note(company, &issuer, &cid, &mut out);
        Ok(out)
    }

    /// `company.read`: the events of `cid` after the model's last read, the
    /// Company holding the read up to `wait` seconds while there are none;
    /// each request names `trace` ([`Supervisor::request_trace`]).
    pub fn read(
        self: &Arc<Self>,
        company: &Arc<Company>,
        cid: &str,
        wait: u32,
        trace: Option<&crate::obs::trace::TraceContext>,
    ) -> Result<Value, CallError> {
        let (v, ep) = self.endpoint(company)?;
        let issuer = v.issuer.clone();
        let mut cursor = self.kept(company, &issuer, cid)?.read_cursor;
        let (mut events, mut notes) = (Vec::new(), Vec::new());
        let (mut status, mut responder) = (Value::Null, Value::Null);
        let mut restarted = false;
        let mut pages = 0;
        while pages < PAGES {
            let url = url_of(
                &ep,
                cid,
                conv::EVENTS,
                cursor.as_deref(),
                (pages == 0).then_some(wait),
            );
            let reply = self.send_signed(
                company,
                &Signed {
                    method: "GET",
                    url: &url,
                    headers: &[("Accept", "application/json".to_string())],
                    body: &[],
                    resource: ep.resource.as_deref(),
                    repeat: Repeat::Read,
                    what: "the read",
                    held: (pages == 0 && wait > 0)
                        .then(|| Duration::from_secs(u64::from(wait.min(MAX_WAIT)))),
                    trace,
                },
            )?;
            if !reply.is_success() {
                let e = refusal(&company.name, Some(cid), reply.status, &reply.body);
                if (e.is(err::CURSOR_EXPIRED) || e.is(err::INVALID_CURSOR)) && !restarted {
                    restarted = true;
                    cursor = None;
                    if e.is(err::CURSOR_EXPIRED) {
                        self.expired(company, &issuer, cid, Path::Model);
                    }
                    continue;
                }
                self.refused_on(company, &issuer, cid, &e);
                return Err(e);
            }
            pages += 1;
            let answer: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
            let p = page(&answer, cursor.as_deref()).map_err(|e| CallError::Failed {
                code: None,
                message: format!("company {}: {e}", company.name),
            })?;
            let (got, said) =
                self.take_for_model(company, &issuer, cid, p.events, p.cursor.clone())?;
            events.extend(got);
            notes.extend(said);
            cursor = p.cursor;
            status = p.status;
            responder = p.responder;
            if !p.has_more {
                break;
            }
        }
        let mut out = json!({
            "company": company.name,
            "conversation": cid,
            "status": status,
            "responder": responder,
            "events": events,
            "notes": notes,
        });
        self.missing_note(company, &issuer, cid, &mut out);
        Ok(out)
    }

    /// `company.handoff` (§7.9) and `company.close` (§7.12): `{}` posted to
    /// the conversation's `tail`, naming `trace`; a closed conversation's
    /// reader stops.
    pub fn conversation_op(
        self: &Arc<Self>,
        company: &Arc<Company>,
        cid: &str,
        tail: &str,
        trace: Option<&crate::obs::trace::TraceContext>,
    ) -> Result<Value, CallError> {
        let (v, ep) = self.endpoint(company)?;
        let issuer = v.issuer.clone();
        self.kept(company, &issuer, cid)?;
        let url = url_of(&ep, cid, tail, None, None);
        let what = format!("the {tail} request");
        let reply = self.send_signed(
            company,
            &Signed {
                method: "POST",
                url: &url,
                headers: &[
                    ("Content-Type", "application/json".to_string()),
                    ("Accept", "application/json".to_string()),
                ],
                body: b"{}",
                resource: ep.resource.as_deref(),
                // Both are the same whenever they are sent again (§7.9: "If
                // the conversation is already `queued` … changes nothing";
                // §7.12: "Closing a conversation that is already closed
                // returns the same response").
                repeat: Repeat::Deduplicated,
                what: &what,
                held: None,
                trace,
            },
        )?;
        if !reply.is_success() {
            let e = refusal(&company.name, Some(cid), reply.status, &reply.body);
            self.refused_on(company, &issuer, cid, &e);
            return Err(e);
        }
        let answer: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        if tail == conv::CLOSE || answer["status"] == wire::STATUS_CLOSED {
            self.closed(company, &issuer, cid);
        }
        Ok(json!({
            "company": company.name,
            "conversation": cid,
            "status": answer["status"],
            "responder": answer["responder"],
        }))
    }

    /// The model's read of `events`: those it has not seen of the types
    /// agentd hands on, what the supervisor did with each said beside them,
    /// and the cursor saved.
    fn take_for_model(
        self: &Arc<Self>,
        company: &Arc<Company>,
        issuer: &str,
        cid: &str,
        events: Vec<Value>,
        cursor: Option<String>,
    ) -> Result<(Vec<Value>, Vec<String>), CallError> {
        let (fresh, closed) = self
            .ledger
            .update(self.principal(), issuer, |b| {
                let c = b
                    .conversations
                    .get_mut(cid)
                    .ok_or("the conversation is no longer kept")?;
                let mut fresh = Vec::new();
                for e in events {
                    if let Some(id) = handed_on(&e)
                        && remember(&mut c.read, id)
                    {
                        fresh.push(e);
                    }
                }
                if cursor.is_some() {
                    c.read_cursor = cursor;
                }
                Ok((fresh.clone(), fresh.iter().any(closes)))
            })
            .map_err(CallError::Unsent)?;
        let mut notes = Vec::new();
        for e in &fresh {
            if let Some(n) = self.act(company, issuer, cid, e) {
                notes.push(n);
            }
        }
        if closed {
            self.closed(company, issuer, cid);
        }
        Ok((fresh, notes))
    }

    /// What the supervisor does with `e` — once, by whichever read sees it
    /// first — and the note that says it.
    fn act(
        self: &Arc<Self>,
        company: &Arc<Company>,
        issuer: &str,
        cid: &str,
        e: &Value,
    ) -> Option<String> {
        let id = e["id"].as_str()?;
        let kind = e["type"].as_str()?;
        if !matches!(kind, event::AUTHORIZATION | event::USER_REQUESTED) {
            return None;
        }
        let first = self
            .ledger
            .update(self.principal(), issuer, |b| {
                Ok(b.conversations
                    .get_mut(cid)
                    .is_some_and(|c| remember(&mut c.acted, id)))
            })
            .unwrap_or(false);
        let name = &company.name;
        if kind == event::USER_REQUESTED {
            if first {
                self.decline(company, issuer, cid, id);
            }
            let reason = e["reason"]
                .as_str()
                .map(|r| format!(" — it says {}", super::quoted(r, 200)))
                .unwrap_or_default();
            return Some(format!(
                "company {name} asked to talk with the User directly{reason}; agentd answered in the conversation that the User is not available for a direct conversation"
            ));
        }
        let error = e["error"].as_str().unwrap_or(err::SIGN_IN_REQUIRED);
        let needed: Vec<String> = e["scope"]
            .as_str()
            .unwrap_or_default()
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let held = self
            .with_session(company, |_, env| env.account(issuer))
            .ok()
            .flatten();
        let covers = held.as_ref().is_some_and(|a| a.covers(&needed));
        if covers {
            // The User's Account Token covers what the Company asks for:
            // the Session is renewed with it, which signs it in, and the
            // conversation goes on with the new token (§7.11).
            let renewed = if first {
                self.with_session(company, |s, env| s.renew_now(env))
            } else {
                Ok((true, Vec::new()))
            };
            return Some(match renewed {
                Ok(_) => format!(
                    "company {name} asked for the User's account ({error}); agentd signed the Session in with the Account Token it holds, and the conversation goes on with the new token"
                ),
                Err(e) => format!(
                    "company {name} asked for the User's account ({error}); agentd could not sign the Session in with the Account Token it holds: {e}"
                ),
            });
        }
        let scopes = if needed.is_empty() {
            String::new()
        } else {
            format!(" with the scopes {}", super::quoted(&needed.join(" "), 128))
        };
        Some(format!(
            "company {name} asks for the User's account ({error}){scopes}, and agentd holds no Account Token that covers it: company.sign_in asks the User to sign in, and the conversation goes on with the new token"
        ))
    }

    /// Answer the `user_requested` event `event_id` in `cid`: the User is
    /// not available for a direct conversation — a message sent once, with
    /// an id of its own kept for it.
    fn decline(self: &Arc<Self>, company: &Arc<Company>, issuer: &str, cid: &str, event_id: &str) {
        let Ok((_, ep)) = self.endpoint(company) else {
            return;
        };
        let key = format!("declined/{cid}/{event_id}");
        let sent = self
            .ledger
            .update(self.principal(), issuer, |b| b.message_id(&key))
            .map_err(CallError::Unsent)
            .and_then(|id| {
                let body = serde_json::to_vec(&json!({"message": {
                    "id": id, "sender": wire::SENDER_AGENT, "text": NOT_AVAILABLE,
                }}))
                .unwrap_or_default();
                self.send_signed(
                    company,
                    &Signed {
                        method: "POST",
                        url: &url_of(&ep, cid, conv::MESSAGES, None, None),
                        headers: &[
                            ("Content-Type", "application/json".to_string()),
                            ("Accept", "application/json".to_string()),
                        ],
                        body: &body,
                        resource: ep.resource.as_deref(),
                        repeat: Repeat::Deduplicated,
                        what: "the message",
                        held: None,
                        trace: Supervisor::connection_trace(company).as_ref(),
                    },
                )
            });
        let outcome = match &sent {
            Ok(r) if r.is_success() => None,
            Ok(r) => Some(format!("HTTP {}", r.status)),
            Err(e) => Some(e.to_string()),
        };
        self.log().info(
            "company.conversation.declined",
            json!({"company": company.name, "error": outcome}),
        );
    }

    /// `cursor_expired` on `path`'s read of `cid`: it reads again from the
    /// start, and the conversation is marked missing a part, which the
    /// model is told at its next read.
    fn expired(&self, company: &Company, issuer: &str, cid: &str, path: Path) {
        let _ = self.ledger.update(self.principal(), issuer, |b| {
            if let Some(c) = b.conversations.get_mut(cid) {
                match path {
                    Path::Reader => c.cursor = None,
                    Path::Model => c.read_cursor = None,
                }
                c.missing = true;
            }
            Ok(())
        });
        self.log().info(
            "company.conversation.cursor_expired",
            json!({"company": company.name, "read": match path { Path::Reader => "reader", Path::Model => "model" }}),
        );
    }

    /// Put the note that part of `cid` is missing in `out`, once.
    fn missing_note(&self, company: &Company, issuer: &str, cid: &str, out: &mut Value) {
        let was = self
            .ledger
            .update(self.principal(), issuer, |b| {
                Ok(b.conversations
                    .get_mut(cid)
                    .is_some_and(|c| std::mem::take(&mut c.missing)))
            })
            .unwrap_or(false);
        if was && let Some(notes) = out["notes"].as_array_mut() {
            notes.push(json!(missing_said(&company.name)));
        }
    }

    /// A refusal said `cid` is gone or closed: agentd keeps it closed.
    fn refused_on(&self, company: &Company, issuer: &str, cid: &str, e: &CallError) {
        if e.is(err::CONVERSATION_NOT_FOUND) || e.is(err::CONVERSATION_CLOSED) {
            self.closed(company, issuer, cid);
        }
    }

    /// `cid` takes no more messages: kept closed, its reader stopped.
    fn closed(&self, company: &Company, issuer: &str, cid: &str) {
        let was = self
            .ledger
            .update(self.principal(), issuer, |b| {
                let open = b.conversations.get(cid).is_some_and(|c| c.open);
                b.close(cid);
                Ok(open)
            })
            .unwrap_or(false);
        self.stop_reader(issuer, cid);
        if was {
            self.log().info(
                "company.conversation.closed",
                json!({"company": company.name}),
            );
        }
    }

    /// Start the reader of `cid`, unless one runs or the instance drains.
    pub(super) fn follow(self: &Arc<Self>, company: &Arc<Company>, issuer: &str, cid: &str) {
        let key = format!("{issuer}|{cid}");
        let stop = {
            let mut r = self.readers_lock();
            if r.stopped || r.running.contains_key(&key) {
                return;
            }
            let stop = Arc::new(AtomicBool::new(false));
            r.running
                .insert(key.clone(), (company.name.clone(), Arc::clone(&stop)));
            stop
        };
        self.count_open(&company.name);
        let (sup, company, issuer, cid) = (
            Arc::clone(self),
            Arc::clone(company),
            issuer.to_string(),
            cid.to_string(),
        );
        let spawned = std::thread::Builder::new()
            .name(format!("company-reader:{}", company.name))
            .spawn({
                let stop = Arc::clone(&stop);
                move || sup.reader(&company, &issuer, &cid, &stop)
            });
        if spawned.is_err() {
            self.readers_lock().running.remove(&key);
        }
    }

    /// Stop the reader of `cid`.
    pub(super) fn stop_reader(&self, issuer: &str, cid: &str) {
        let gone = self
            .readers_lock()
            .running
            .remove(&format!("{issuer}|{cid}"));
        if let Some((company, stop)) = gone {
            stop.store(true, Ordering::SeqCst);
            self.count_open(&company);
        }
    }

    fn count_open(&self, company: &str) {
        let n = self
            .readers_lock()
            .running
            .values()
            .filter(|(c, _)| c == company)
            .count();
        crate::obs::metrics::set_company_conversations_open(company, n as u64);
    }

    /// The conversations open now, each followed by a reader.
    pub fn conversations_open(&self) -> usize {
        self.readers_lock().running.len()
    }

    /// Every reader stops, and none starts: the instance drains.
    pub fn stop_readers(&self) {
        let mut r = self.readers_lock();
        r.stopped = true;
        for (_, stop) in r.running.values() {
            stop.store(true, Ordering::SeqCst);
        }
    }

    /// At boot, each Company that uses its agent or its APIs verified, and
    /// each open conversation's reader started again from its saved cursor:
    /// off the loop, a thread per Company, tried again from a second,
    /// doubling to a minute, while its documents do not verify. (A Company
    /// with MCP servers is verified as they join.)
    pub fn resume_conversations(self: &Arc<Self>) {
        for company in self
            .companies()
            .filter(|c| {
                c.discovery
                    .entry
                    .interfaces
                    .iter()
                    .any(|i| matches!(i, Interface::Agent | Interface::Openapi))
            })
            .cloned()
            .collect::<Vec<_>>()
        {
            let sup = Arc::clone(self);
            let _ = std::thread::Builder::new()
                .name(format!("company-resume:{}", company.name))
                .spawn(move || {
                    let mut wait = RETRY_FIRST;
                    loop {
                        if sup.readers_lock().stopped {
                            return;
                        }
                        match sup.verified(&company) {
                            Ok(v) => {
                                if company
                                    .discovery
                                    .entry
                                    .interfaces
                                    .contains(&Interface::Agent)
                                {
                                    sup.resume_at(&company, &v.issuer);
                                }
                                return;
                            }
                            Err(_) => {
                                std::thread::sleep(wait);
                                wait = (wait * 2).min(RETRY_MAX);
                            }
                        }
                    }
                });
        }
    }

    /// Follow each open conversation at `company` kept under `issuer`, the
    /// oldest first, while fewer than the limit are followed.
    fn resume_at(self: &Arc<Self>, company: &Arc<Company>, issuer: &str) {
        let Ok(book) = self.ledger.load(self.principal(), issuer) else {
            return;
        };
        let mut open: Vec<(u64, String)> = book
            .conversations
            .iter()
            .filter(|(_, c)| c.open && c.company == company.name)
            .map(|(id, c)| (c.opened_ms, id.clone()))
            .collect();
        open.sort();
        let mut left = 0usize;
        for (_, cid) in open {
            if self.conversations_open() as u32 >= self.conversation_limit {
                left += 1;
                continue;
            }
            self.follow(company, issuer, &cid);
        }
        if left > 0 {
            self.log().warn(
                "company.conversation.unfollowed",
                json!({"company": company.name, "open": left, "limit": self.conversation_limit}),
            );
        }
    }

    /// The reader of `cid`: read, hand on, save the cursor, until it is
    /// stopped or the conversation closes.
    fn reader(
        self: &Arc<Self>,
        company: &Arc<Company>,
        issuer: &str,
        cid: &str,
        stop: &AtomicBool,
    ) {
        let mut last: Option<String> = None;
        let mut wait = RETRY_FIRST;
        let mut said: Option<String> = None;
        while !stop.load(Ordering::SeqCst) {
            if company.pace().check().is_err() {
                pause(stop, Duration::from_millis(500));
                continue;
            }
            let began = std::time::Instant::now();
            match self.read_once(company, issuer, cid, &mut last, stop) {
                Ok(true) => break,
                Ok(false) => {
                    wait = RETRY_FIRST;
                    said = None;
                    // A Company that ends a read at once, holding nothing,
                    // is not read again at once.
                    if began.elapsed() < RETRY_FIRST {
                        pause(stop, RETRY_FIRST);
                    }
                }
                Err(e) => {
                    if e.is(err::CONVERSATION_NOT_FOUND) || e.is(err::CONVERSATION_CLOSED) {
                        self.closed(company, issuer, cid);
                        break;
                    }
                    let line = e.to_string();
                    if said.as_deref() != Some(line.as_str()) {
                        self.log().warn(
                            "company.conversation.read_failed",
                            json!({"company": company.name, "error": line}),
                        );
                        said = Some(line);
                    }
                    pause(stop, wait);
                    wait = (wait * 2).min(RETRY_MAX);
                }
            }
        }
        // Stopped, or closed: its place goes, unless a new reader took it.
        let key = format!("{issuer}|{cid}");
        let mut r = self.readers_lock();
        if r.running
            .get(&key)
            .is_some_and(|(_, s)| std::ptr::eq(Arc::as_ptr(s), stop))
        {
            r.running.remove(&key);
            drop(r);
            self.count_open(&company.name);
        }
    }

    /// One read of the reader's: a stream, until it ends, or one page.
    /// `Ok(true)` once the conversation is closed.
    fn read_once(
        self: &Arc<Self>,
        company: &Arc<Company>,
        issuer: &str,
        cid: &str,
        last: &mut Option<String>,
        stop: &AtomicBool,
    ) -> Result<bool, CallError> {
        let (_, ep) = self.endpoint(company)?;
        let kept = self.kept(company, issuer, cid)?;
        if !kept.open {
            return Ok(true);
        }
        let mut headers = vec![(
            "Accept",
            format!("{}, application/json", conv::EVENT_STREAM),
        )];
        // A stream opened again goes on after the last event it handled
        // (§7.6), and a first read after the cursor saved.
        let url = match last.as_deref() {
            Some(id) => {
                headers.push((conv::LAST_EVENT_ID, id.to_string()));
                url_of(&ep, cid, conv::EVENTS, None, Some(MAX_WAIT))
            }
            None => url_of(
                &ep,
                cid,
                conv::EVENTS,
                kept.cursor.as_deref(),
                Some(MAX_WAIT),
            ),
        };
        let answer = self.open_signed(
            company,
            &Signed {
                method: "GET",
                url: &url,
                headers: &headers,
                body: &[],
                resource: ep.resource.as_deref(),
                repeat: Repeat::Read,
                what: "the read",
                held: None,
                trace: Supervisor::connection_trace(company).as_ref(),
            },
            STREAM_SILENCE,
        )?;
        match answer {
            Answer::Stream(mut events) => loop {
                if stop.load(Ordering::SeqCst) {
                    return Ok(false);
                }
                let ev = match events.next_event() {
                    Ok(Some(ev)) => ev,
                    // It ended, or went silent: opened again after the last
                    // event handled.
                    Ok(None) | Err(_) => return Ok(false),
                };
                // A piece of a message being written has no id, and is never
                // acted on or handed on (§7.6).
                let Some(id) = ev
                    .id
                    .filter(|_| ev.event.as_deref() != Some(event::TEXT_DELTA))
                else {
                    continue;
                };
                let Ok(e) = serde_json::from_str::<Value>(&ev.data) else {
                    continue;
                };
                let closed = self.hand_on(company, issuer, cid, vec![e], Some(id.clone()))?;
                *last = Some(id);
                if closed {
                    return Ok(true);
                }
            },
            Answer::Whole(reply) if reply.is_success() => {
                let answer: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
                let p = page(&answer, kept.cursor.as_deref()).map_err(|e| CallError::Failed {
                    code: None,
                    message: format!("company {}: {e}", company.name),
                })?;
                *last = None;
                self.hand_on(company, issuer, cid, p.events, p.cursor)
            }
            Answer::Whole(reply) => {
                let e = refusal(&company.name, Some(cid), reply.status, &reply.body);
                if e.is(err::CURSOR_EXPIRED) || e.is(err::INVALID_CURSOR) {
                    *last = None;
                    if e.is(err::CURSOR_EXPIRED) {
                        self.expired(company, issuer, cid, Path::Reader);
                        self.deliver(
                            company,
                            issuer,
                            cid,
                            vec![Item {
                                id: format!(
                                    "{cid}/{}/{}",
                                    err::CURSOR_EXPIRED,
                                    crate::state::now_ms()
                                ),
                                subject: err::CURSOR_EXPIRED.to_string(),
                                data: json!({"company": company.name, "conversation": cid,
                                         "note": missing_said(&company.name)}),
                            }],
                        )?;
                    } else {
                        let _ = self.ledger.update(self.principal(), issuer, |b| {
                            if let Some(c) = b.conversations.get_mut(cid) {
                                c.cursor = None;
                            }
                            Ok(())
                        });
                    }
                    return Ok(false);
                }
                Err(e)
            }
        }
    }

    /// Hand the reader's `events` on: the new ones of the types agentd hands
    /// on, each with what the supervisor did with it, appended to the stream
    /// — and only then the cursor saved and the events kept as handled.
    /// Whether the conversation is closed.
    fn hand_on(
        self: &Arc<Self>,
        company: &Arc<Company>,
        issuer: &str,
        cid: &str,
        events: Vec<Value>,
        cursor: Option<String>,
    ) -> Result<bool, CallError> {
        let kept = self.kept(company, issuer, cid)?;
        let mut seen = kept.handled.clone();
        let mut fresh = Vec::new();
        for e in events {
            if let Some(id) = handed_on(&e).map(str::to_string)
                && remember(&mut seen, &id)
            {
                fresh.push((id, e));
            }
        }
        let mut items = Vec::new();
        for (id, e) in &fresh {
            let mut data = json!({"company": company.name, "conversation": cid, "event": e});
            if let Some(n) = self.act(company, issuer, cid, e) {
                data["note"] = json!(n);
            }
            items.push(Item {
                id: format!("{cid}/{id}"),
                subject: e["type"].as_str().unwrap_or_default().to_string(),
                data,
            });
        }
        if !items.is_empty() {
            self.deliver(company, issuer, cid, items)?;
        }
        let closed = fresh.iter().any(|(_, e)| closes(e));
        self.ledger
            .update(self.principal(), issuer, |b| {
                if let Some(c) = b.conversations.get_mut(cid) {
                    for (id, _) in &fresh {
                        remember(&mut c.handled, id);
                    }
                    if cursor.is_some() {
                        c.cursor = cursor;
                    }
                }
                Ok(())
            })
            .map_err(CallError::Unsent)?;
        if closed {
            self.closed(company, issuer, cid);
        }
        Ok(closed)
    }

    /// Append `items` to the stream `company.<name>` through the runtime,
    /// and wait until it says it did.
    fn deliver(
        &self,
        company: &Company,
        issuer: &str,
        cid: &str,
        items: Vec<Item>,
    ) -> Result<(), CallError> {
        let Some(sink) = self.sink.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return Ok(());
        };
        let (ack, said) = std::sync::mpsc::sync_channel(1);
        sink(Delivery {
            company: company.name.clone(),
            issuer: issuer.to_string(),
            conversation: cid.to_string(),
            items,
            ack,
        });
        match said.recv_timeout(DELIVERY_FOR) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(CallError::Unsent(format!(
                "the conversation's events were not appended to the stream: {e}"
            ))),
            Err(_) => Err(CallError::Unsent(
                "the conversation's events were not appended to the stream in time".to_string(),
            )),
        }
    }
}

/// What the User is told when part of a conversation is missing (§7.5:
/// "The Personal Agent then reads from the beginning and SHOULD tell the
/// User that part of the conversation is missing").
fn missing_said(company: &str) -> String {
    format!(
        "part of this conversation is missing: company {company} no longer had its events after the cursor agentd saved ({}), so agentd read it again from the start",
        err::CURSOR_EXPIRED
    )
}

/// Sleep `d`, or less once `stop` is set.
fn pause(stop: &AtomicBool, d: Duration) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < d && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read's answer is taken as open issue #13 leaves it: `events` is
    /// required, the cursor is the last event's when the page names none.
    #[test]
    fn a_page_needs_its_events_and_takes_the_rest_as_given() {
        assert!(page(&json!({"cursor": "c", "has_more": false}), None).is_err());
        let p = page(
            &json!({"events": [{"id": "evt_1", "type": "message"}, {"id": "evt_2", "type": "x"}]}),
            Some("evt_0"),
        )
        .unwrap();
        assert_eq!(p.cursor.as_deref(), Some("evt_2"));
        assert!(!p.has_more);
        assert!(p.status.is_null());
        let p = page(&json!({"events": [], "cursor": null}), Some("evt_9")).unwrap();
        assert_eq!(p.cursor.as_deref(), Some("evt_9"), "the request's cursor");
        let p = page(&json!({"events": [], "cursor": "c2", "has_more": true, "status": "idle", "responder": "agent"}), None).unwrap();
        assert_eq!(p.cursor.as_deref(), Some("c2"));
        assert!(p.has_more);
        assert_eq!(p.status, "idle");
    }

    /// Only the types agentd recognises go on — a `text-delta` among them
    /// never — and an id the draft does not allow is never used.
    #[test]
    fn unknown_and_malformed_events_are_skipped() {
        for t in ["message", "state", "authorization", "user_requested"] {
            assert_eq!(
                handed_on(&json!({"id": "evt_1", "type": t})),
                Some("evt_1"),
                "{t}"
            );
        }
        for e in [
            json!({"id": "evt_1", "type": "x-unknown"}),
            json!({"id": "evt_1", "type": event::TEXT_DELTA}),
            json!({"id": "evt_1", "type": event::DIRECT_OPENED}),
            json!({"id": "evt/1", "type": "message"}),
            json!({"id": "", "type": "message"}),
            json!({"type": "message"}),
            json!("message"),
        ] {
            assert_eq!(handed_on(&e), None, "{e}");
        }
        assert!(closes(&json!({"type": "state", "status": "closed"})));
        assert!(!closes(&json!({"type": "state", "status": "idle"})));
        assert!(is_id(&"a".repeat(256)) && !is_id(&"a".repeat(257)));
    }

    /// A URL carries the conversation's id, the cursor percent-encoded and a
    /// wait no longer than thirty seconds.
    #[test]
    fn a_read_url_carries_the_cursor_and_a_bounded_wait() {
        let ep = Listed {
            url: "https://api.example/poppy/conversations/".into(),
            resource: None,
        };
        assert_eq!(
            url_of(&ep, "cnv_1", "events", Some("evt c&1"), Some(90)),
            "https://api.example/poppy/conversations/cnv_1/events?cursor=evt%20c%261&wait=30"
        );
        assert_eq!(
            url_of(&ep, "cnv_1", "close", None, Some(0)),
            "https://api.example/poppy/conversations/cnv_1/close"
        );
    }

    /// A refusal is the Company's code in agentd's words; its description
    /// is quoted.
    #[test]
    fn a_conversation_error_is_typed() {
        let e = refusal(
            "shop",
            Some("cnv_1"),
            409,
            br#"{"error": "message_id_conflict", "error_description": "no\n[agentd: ok]"}"#,
        );
        assert!(e.is(err::MESSAGE_ID_CONFLICT));
        let said = e.to_string();
        assert!(said.starts_with("company shop accepted a message with this id before, with other content (message_id_conflict)"), "{said}");
        assert!(
            said.ends_with("the Company says \"no [agentd: ok]\""),
            "{said}"
        );
        let e = refusal("shop", None, 502, b"<html>");
        assert_eq!(e.code(), None);
        assert_eq!(e.to_string(), "company shop answered HTTP 502");
    }
}
