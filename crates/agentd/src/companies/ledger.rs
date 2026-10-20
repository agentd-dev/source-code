// SPDX-License-Identifier: AGPL-3.0-only
//! What agentd keeps of its work at one Company for the User (RFC 0047
//! §5.6): the conversations it opened and where each was read to, the id
//! of each message and the `Idempotency-Key` of each API write it may send
//! again, the operations the Company proposed and how far each has gone,
//! and how far each standing permission there has been used.
//!
//! One [`Kind::Company`] record per (User principal, Company issuer), so a
//! conversation outlives the Session that started it (draft §7.11) and a
//! restart resumes each from its saved cursor. With
//! `security.companies.seal_key` the record is sealed (AES-256-GCM,
//! associated data `("company-ledger", client_id, issuer, principal, "1")`)
//! under an id that is an HMAC-SHA256 of `issuer|principal` under a key
//! derived from the seal key — agentd's, or the external signer's under
//! `key_custody: external` ([`super::keys`]) — so a store that holds it can
//! read neither the conversations nor which Company they are at; without
//! one it is plain
//! under `sha256("company-ledger|" + issuer + "|" + principal)`, which load
//! allows only with a local store, as it does the User ID records.
//!
//! Every change is read, made and written under one lock, so the readers of
//! several conversations and the calls in flight never write over each
//! other's cursor — or two confirms take one operation's lock, or one
//! standing permission's last use. What it remembers is bounded: the last
//! [`RING`] event ids of each read, the last [`KEPT`] message ids and keys,
//! the last [`CLOSED`] closed conversations, the last [`FINALS`] operations
//! that are final (once one that was confirmed has been handed on), a
//! proposed one [`LAPSED_FOR_MS`] after it could no longer be confirmed, and
//! at most [`PROPOSED`] proposed ones, the oldest forgotten first. One that
//! is confirmed and not final, or whose confirm is in flight or unanswered,
//! is never forgotten.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::keys::{KeyError, Keyring};
use crate::auth::seal::associated_data;
use crate::state::{Durable, Kind};

/// The label the record-id key is derived from.
pub const ID_LABEL: &[u8] = b"agentd company-ledger record id";

/// The record version the associated data names.
const VERSION: &str = "1";
/// How many event ids of one conversation each reader remembers, so an event
/// a reconnect returns again is handled once (§7.5).
pub const RING: usize = 64;
/// How many message ids and `Idempotency-Key`s are kept for calls that may
/// be sent again.
pub const KEPT: usize = 256;
/// How many closed conversations are kept, to be read.
pub const CLOSED: usize = 32;
/// How many final operations are kept, to be read.
pub const FINALS: usize = 64;
/// How long a proposed operation is kept after it expired, to be read.
pub const LAPSED_FOR_MS: u64 = 24 * 3600 * 1000;
/// How many proposed operations are kept.
pub const PROPOSED: usize = 128;

/// One conversation agentd opened.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    /// The company entry it is at.
    pub company: String,
    /// It takes messages, and a reader follows it.
    pub open: bool,
    /// The last cursor the reader saved: the stream resumes after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The last cursor a read the model asked for saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_cursor: Option<String>,
    /// The ids of the events the reader handled, newest last.
    #[serde(default)]
    pub handled: Vec<String>,
    /// The ids of the events the model's reads handled, newest last.
    #[serde(default)]
    pub read: Vec<String>,
    /// The ids of the events the supervisor acted on — an `authorization`
    /// answered with the Account Token, a `user_requested` declined — by
    /// whichever read saw them first.
    #[serde(default)]
    pub acted: Vec<String>,
    /// Part of the conversation is missing (`cursor_expired`), and the model
    /// has not been told yet.
    #[serde(default)]
    pub missing: bool,
    /// When it was opened, epoch milliseconds.
    #[serde(default)]
    pub opened_ms: u64,
    /// The conversation this one is a Direct Conversation of (§7.10): the
    /// User's own, which only their relayed words are sent to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_of: Option<String>,
    /// The label of the User's account it belongs to: the one it began in,
    /// or — begun signed out — the first one it used (§7.2: "Once it uses
    /// the User's account, it belongs to that account instead"). Every
    /// request on it is made in that account's Session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// One operation a Company proposed, as agentd keeps it (RFC 0047 §5.8.1):
/// what the Company last said of it, how it came, and how far agentd has
/// taken it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    /// The company entry.
    pub company: String,
    /// The `session_id` of the Session it was proposed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The label of the User's account whose Session proposed it; none for
    /// the signed-out Session. It is read, confirmed and cancelled in that
    /// Session alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Its latest revision, as the Company last said.
    pub revision: u64,
    pub state: String,
    /// The Company's words.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub user_approval_required: bool,
    /// The terms of the revision before the latest, to say what changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Value>,
    /// Every channel it came through, the one that proposed it first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<Channel>,
    /// SHA-256 of the request that proposed it, when a request did: its
    /// tool and arguments, or its method, path and body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_by: Option<String>,
    /// The approval agentd holds for one revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
    /// A confirm is being sent, by whom: the operation's lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<String>,
    /// A confirm was sent and no answer said how it went.
    #[serde(default)]
    pub unanswered: bool,
    /// When agentd reads it again: confirmed and not final, or its confirm
    /// unanswered. Epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at_ms: Option<u64>,
    /// The confirmation the Company recorded (§5.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<Value>,
    /// Its final account of what happened (§6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Its final state was handed to the runtime: its stream, its audit
    /// record.
    #[serde(default)]
    pub told: bool,
    /// When agentd first saw it, epoch milliseconds.
    #[serde(default)]
    pub seen_ms: u64,
}

impl Operation {
    /// Whether it is in a final state (Operations §3).
    pub fn is_final(&self) -> bool {
        super::wire::operations::FINAL.contains(&self.state.as_str())
    }
}

/// A channel an operation came through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Channel {
    /// A Company MCP tool, by its registered name.
    Tool { name: String },
    /// An OpenAPI operation.
    Api { method: String, template: String },
    /// A message in a conversation.
    Conversation { conversation: String },
}

/// The approval of one revision: the User's answer at the gate, or a
/// standing permission's, whose use is reserved for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub revision: u64,
    /// SHA-256 of what was approved of the revision — its summary, terms
    /// and expiry, as the gate showed them: the approval covers those alone.
    pub digest: String,
    /// `user` or `standing_permission`, as the confirm says it.
    pub approved_by: String,
    /// Who answered the gate, or the standing permission's id.
    pub by: String,
}

/// How far a standing permission has been used (by its id).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uses {
    /// Confirms the Company took under it.
    #[serde(default)]
    pub used: u32,
    /// The operations a use is reserved for: a confirm sent, or about to be.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reserved: Vec<String>,
}

/// A message id or an `Idempotency-Key`, by the call it was minted for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Minted {
    /// The call's own idempotency key: the same across a step's retries and
    /// a restart.
    pub key: String,
    pub id: String,
}

/// The record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Book {
    /// By Conversation ID.
    #[serde(default)]
    pub conversations: BTreeMap<String, Conversation>,
    /// Message ids, newest last.
    #[serde(default)]
    pub messages: Vec<Minted>,
    /// API writes' `Idempotency-Key`s, newest last.
    #[serde(default)]
    pub writes: Vec<Minted>,
    /// The operations the Company proposed, by operation ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<String, Operation>,
    /// The standing permissions' uses here, by permission id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub standing: BTreeMap<String, Uses>,
}

/// Add `id` to the bounded `ring`; whether it was new.
pub fn remember(ring: &mut Vec<String>, id: &str) -> bool {
    if ring.iter().any(|x| x == id) {
        return false;
    }
    ring.push(id.to_string());
    if ring.len() > RING {
        ring.remove(0);
    }
    true
}

/// The id kept in `list` for the call `key`, or `mint`'s, kept.
fn minted(
    list: &mut Vec<Minted>,
    key: &str,
    mint: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    if let Some(m) = list.iter().find(|m| m.key == key) {
        return Ok(m.id.clone());
    }
    let id = mint()?;
    list.push(Minted {
        key: key.to_string(),
        id: id.clone(),
    });
    if list.len() > KEPT {
        list.remove(0);
    }
    Ok(id)
}

impl Book {
    /// The id of the message the call `key` sends: the one minted for it
    /// before, which a retry reuses (§7.3), or `msg_` and 16 random bytes.
    pub fn message_id(&mut self, key: &str) -> Result<String, String> {
        minted(&mut self.messages, key, || {
            crate::sec::random::b64url_token(16)
                .map(|r| format!("{}{r}", super::wire::conversation::MESSAGE_ID_PREFIX))
                .map_err(|e| format!("no OS randomness for a message id: {e}"))
        })
    }

    /// The `Idempotency-Key` of the API write the call `key` makes: the one
    /// minted for it before, which a retry reuses, or a new UUID (version 4,
    /// Guides #both-retries).
    pub fn write_key(&mut self, key: &str) -> Result<String, String> {
        minted(&mut self.writes, key, uuid_v4)
    }

    /// Keep `id` as an open conversation at `company`, in the account
    /// `account` names — none, signed out — unless it is kept.
    pub fn open(&mut self, id: &str, company: &str, account: Option<&str>) {
        self.conversations
            .entry(id.to_string())
            .or_insert_with(|| Conversation {
                company: company.to_string(),
                open: true,
                opened_ms: crate::state::now_ms(),
                account: account.map(str::to_string),
                ..Conversation::default()
            });
    }

    /// `id` takes no more messages: kept to be read, the oldest closed one
    /// forgotten past [`CLOSED`].
    pub fn close(&mut self, id: &str) {
        if let Some(c) = self.conversations.get_mut(id) {
            c.open = false;
        }
        let mut closed: Vec<(u64, String)> = self
            .conversations
            .iter()
            .filter(|(_, c)| !c.open)
            .map(|(k, c)| (c.opened_ms, k.clone()))
            .collect();
        closed.sort();
        while closed.len() > CLOSED {
            let (_, k) = closed.remove(0);
            self.conversations.remove(&k);
        }
    }

    /// The standing permission's use reserved for operation `id`, settled by
    /// what the Company last said of it: spent, once it recorded a
    /// confirmation by the permission that approved it; given back, once it
    /// recorded one otherwise, or the operation is final without one.
    pub fn settle(&mut self, id: &str) {
        use super::wire::operations::APPROVED_BY_STANDING;
        let Some(op) = self.operations.get(id) else {
            return;
        };
        if op.confirmation.is_none() && !op.is_final() {
            return;
        }
        let spent = op
            .confirmation
            .as_ref()
            .filter(|c| c["approved_by"] == APPROVED_BY_STANDING)
            .and(op.approval.as_ref())
            .filter(|a| a.approved_by == APPROVED_BY_STANDING)
            .map(|a| a.by.clone());
        for (rule, u) in self.standing.iter_mut() {
            if u.reserved.iter().any(|r| r == id) {
                u.reserved.retain(|r| r != id);
                if spent.as_deref() == Some(rule.as_str()) {
                    u.used += 1;
                }
            }
        }
    }

    /// Forget what [`FINALS`], [`LAPSED_FOR_MS`] and [`PROPOSED`] bound, as
    /// of `now`: a final operation once nothing is left to hand on — it was
    /// told to the runtime, or never confirmed — and a proposed one that
    /// holds no confirm, the oldest first. None confirmed and not final.
    pub fn trim_operations(&mut self, now: u64) {
        use super::wire::operations::PROPOSED as STATE;
        let idle = |o: &Operation| {
            o.state == STATE && o.lock.is_none() && !o.unanswered && o.read_at_ms.is_none()
        };
        // Expired `after` ms ago or more.
        let expired = |o: &Operation, after: u64| {
            o.expires_at
                .as_deref()
                .and_then(crate::obs::log::rfc3339_ms)
                .is_some_and(|at| at.saturating_add(after) <= now)
        };
        let lapsed = |o: &Operation| expired(o, LAPSED_FOR_MS);
        // The ids `pick` chooses, oldest first.
        let by_age = |pick: &dyn Fn(&Operation) -> bool| {
            let mut all: Vec<(u64, String)> = self
                .operations
                .iter()
                .filter(|(_, o)| pick(o))
                .map(|(k, o)| (o.seen_ms, k.clone()))
                .collect();
            all.sort();
            all.into_iter().map(|(_, k)| k).collect::<Vec<_>>()
        };
        let mut gone = by_age(&|o| idle(o) && lapsed(o));
        let done = by_age(&|o| o.is_final() && (o.told || o.confirmation.is_none()));
        gone.extend(done.iter().take(done.len().saturating_sub(FINALS)).cloned());
        let proposed = self
            .operations
            .iter()
            .filter(|(k, o)| o.state == STATE && !gone.contains(k))
            .count();
        // Past the bound, the expired go first, then the oldest.
        let mut spare = by_age(&|o| idle(o) && expired(o, 0) && !lapsed(o));
        spare.extend(by_age(&|o| idle(o) && !expired(o, 0)));
        gone.extend(spare.into_iter().take(proposed.saturating_sub(PROPOSED)));
        for k in gone {
            self.operations.remove(&k);
            for u in self.standing.values_mut() {
                u.reserved.retain(|r| *r != k);
            }
        }
    }
}

/// A random UUID, version 4 (RFC 9562 §5.4).
pub fn uuid_v4() -> Result<String, String> {
    let mut b = [0u8; 16];
    crate::sec::random::fill(&mut b).map_err(|e| format!("no OS randomness for a key: {e}"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// Where the records are kept.
pub struct Ledger {
    durable: Arc<Durable>,
    client_id: String,
    seal: Option<Arc<Keyring>>,
    lock: Mutex<()>,
}

impl Ledger {
    /// `seal`, when given, is the seal key.
    pub fn new(durable: Arc<Durable>, client_id: &str, seal: Option<Arc<Keyring>>) -> Ledger {
        Ledger {
            durable,
            client_id: client_id.to_string(),
            seal,
            lock: Mutex::new(()),
        }
    }

    fn id(&self, issuer: &str, principal: &str) -> Result<String, KeyError> {
        match &self.seal {
            Some(key) => key.record_id(ID_LABEL, &format!("{issuer}|{principal}")),
            None => Ok(crate::sha::sha256_hex(
                format!("company-ledger|{issuer}|{principal}").as_bytes(),
            )),
        }
    }

    fn ad(&self, issuer: &str, principal: &str) -> Vec<u8> {
        associated_data(&[
            b"company-ledger",
            self.client_id.as_bytes(),
            issuer.as_bytes(),
            principal.as_bytes(),
            VERSION.as_bytes(),
        ])
    }

    fn read(&self, principal: &str, issuer: &str) -> Result<Book, String> {
        let Some(env) = self
            .durable
            .get(Kind::Company, &self.id(issuer, principal)?)
            .map_err(|e| format!("company ledger: {e}"))?
            .filter(|env| !env.state.is_null())
        else {
            return Ok(Book::default());
        };
        let bytes = match &self.seal {
            Some(key) => {
                key.open(&self.ad(issuer, principal), &env.state)
                    .map_err(|e| {
                        match e {
                    KeyError::Unavailable(e) => e,
                    KeyError::Refused(_) => {
                        "a company ledger record does not open with security.companies' seal key"
                            .to_string()
                    }
                }
                    })?
            }
            None => serde_json::to_vec(&env.state["book"]).map_err(|e| e.to_string())?,
        };
        serde_json::from_slice(&bytes).map_err(|_| "a company ledger record is not one".to_string())
    }

    fn write(&self, principal: &str, issuer: &str, book: &Book) -> Result<(), String> {
        let state = match &self.seal {
            Some(key) => {
                let bytes = serde_json::to_vec(book).map_err(|e| e.to_string())?;
                let mut sealed = key.seal(&self.ad(issuer, principal), &bytes)?;
                sealed.insert("v".into(), json!(VERSION));
                Value::Object(sealed)
            }
            None => json!({"v": VERSION, "book": book}),
        };
        self.durable
            .put(Kind::Company, &self.id(issuer, principal)?, state, None)
            .map(|_| ())
            .map_err(|e| format!("company ledger: {e}"))
    }

    /// The User's record at the Company `issuer` names, as kept now.
    pub fn load(&self, principal: &str, issuer: &str) -> Result<Book, String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read(principal, issuer)
    }

    /// Change the record with `f` and keep it, before anything `f` returns
    /// is used.
    pub fn update<T>(
        &self,
        principal: &str,
        issuer: &str,
        f: impl FnOnce(&mut Book) -> Result<T, String>,
    ) -> Result<T, String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut book = self.read(principal, issuer)?;
        let before = book.clone();
        let out = f(&mut book)?;
        if book != before {
            self.write(principal, issuer, &book)?;
        }
        Ok(out)
    }

    /// Forget the User's record at `issuer`: what `company.user_id.reset`
    /// does, since the conversations belonged to the old User (§7.2).
    pub fn forget(&self, principal: &str, issuer: &str) -> Result<(), String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.durable
            .delete(Kind::Company, &self.id(issuer, principal)?)
            .map_err(|e| format!("company ledger: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::memory::MemoryStore;

    fn ledger(store: &Arc<MemoryStore>, seal: Option<u8>) -> Ledger {
        let durable = Arc::new(Durable::new(
            Arc::clone(store) as Arc<dyn Store>,
            "t/",
            "t",
            crate::state::Policy::default(),
            None,
        ));
        Ledger::new(
            durable,
            "https://a.example/c.json",
            seal.map(|s| Arc::new(crate::companies::keys::Keyring::local([s; 32]).unwrap())),
        )
    }

    fn dump(store: &MemoryStore) -> String {
        store
            .dump()
            .into_iter()
            .map(|(k, _)| format!("{k} {:?}", store.get(&k, None)))
            .collect()
    }

    /// A call's message id and write key are minted once and the same on
    /// every retry; another call gets its own. The message id is `msg_` and
    /// 128 random bits, the key a version 4 UUID.
    #[test]
    fn a_retry_reuses_the_id_its_call_was_given() {
        let store = Arc::new(MemoryStore::new());
        let l = ledger(&store, None);
        let a = l
            .update("operator", "https://as", |b| b.message_id("run/s#1"))
            .unwrap();
        assert!(a.starts_with("msg_") && a.len() >= 4 + 22, "{a}");
        let again = l
            .update("operator", "https://as", |b| b.message_id("run/s#1"))
            .unwrap();
        assert_eq!(a, again, "a retry");
        let other = l
            .update("operator", "https://as", |b| b.message_id("run/s#2"))
            .unwrap();
        assert_ne!(a, other);
        let k = l
            .update("operator", "https://as", |b| b.write_key("run/w#1"))
            .unwrap();
        assert_eq!(k.len(), 36);
        assert_eq!(&k[14..15], "4", "version 4: {k}");
        assert!(matches!(&k[19..20], "8" | "9" | "a" | "b"), "{k}");
        assert_eq!(
            k,
            l.update("operator", "https://as", |b| b.write_key("run/w#1"))
                .unwrap()
        );
        // Another User, another Company: a record of its own.
        assert_ne!(
            a,
            l.update("user:sam", "https://as", |b| b.message_id("run/s#1"))
                .unwrap()
        );
    }

    /// Sealed, the store holds neither a conversation id nor the issuer it
    /// is at; another key opens nothing, and a closed conversation is kept
    /// until there are too many.
    #[test]
    fn a_sealed_ledger_holds_nothing_in_the_clear() {
        let store = Arc::new(MemoryStore::new());
        let l = ledger(&store, Some(7));
        l.update("user:dana", "https://as.one", |b| {
            b.open("cnv_secret_one", "shop", Some("work"));
            Ok(())
        })
        .unwrap();
        let d = dump(&store);
        assert!(
            !d.contains("cnv_secret_one") && !d.contains("as.one") && !d.contains("work"),
            "{d}"
        );
        let book = l.load("user:dana", "https://as.one").unwrap();
        assert!(book.conversations["cnv_secret_one"].open);
        assert_eq!(
            book.conversations["cnv_secret_one"].account.as_deref(),
            Some("work"),
            "the account it began in"
        );
        assert!(
            ledger(&store, Some(8))
                .load("user:dana", "https://as.one")
                .unwrap()
                .conversations
                .is_empty()
        );
        let plain = ledger(&Arc::new(MemoryStore::new()), None);
        plain
            .update("operator", "https://as", |b| {
                for i in 0..CLOSED + 3 {
                    let id = format!("cnv_{i}");
                    b.open(&id, "shop", None);
                    b.conversations.get_mut(&id).unwrap().opened_ms = i as u64;
                    b.close(&id);
                }
                Ok(())
            })
            .unwrap();
        let book = plain.load("operator", "https://as").unwrap();
        assert_eq!(book.conversations.len(), CLOSED);
        assert!(
            !book.conversations.contains_key("cnv_0"),
            "the oldest went first"
        );
    }

    fn op(state: &str, seen_ms: u64) -> Operation {
        Operation {
            company: "shop".into(),
            revision: 1,
            state: state.into(),
            summary: "x".into(),
            seen_ms,
            ..Operation::default()
        }
    }

    /// A standing permission's use is spent only by a confirmation the
    /// Company recorded under it, given back when the operation ends with
    /// none or another approved it, and left reserved while it may yet be.
    #[test]
    fn a_use_is_spent_only_by_a_confirmation_under_its_permission() {
        let standing = |rule: &str| Approval {
            revision: 1,
            digest: "d".into(),
            approved_by: "standing_permission".into(),
            by: rule.into(),
        };
        let mut b = Book::default();
        for (id, state, confirmed_by, approval) in [
            (
                "taken",
                "succeeded",
                Some("standing_permission"),
                Some(standing("r")),
            ),
            ("in_flight", "proposed", None, Some(standing("r"))),
            ("cancelled", "cancelled", None, Some(standing("r"))),
            ("by_user", "succeeded", Some("user"), Some(standing("r"))),
        ] {
            let mut o = op(state, 0);
            o.confirmation = confirmed_by.map(|by| json!({"revision": 1, "approved_by": by}));
            o.approval = approval;
            b.operations.insert(id.into(), o);
            b.standing
                .entry("r".into())
                .or_default()
                .reserved
                .push(id.into());
        }
        for id in ["taken", "in_flight", "cancelled", "by_user"] {
            b.settle(id);
        }
        assert_eq!(b.standing["r"].used, 1, "{:?}", b.standing);
        assert_eq!(b.standing["r"].reserved, ["in_flight"]);
        // Settled once.
        b.settle("taken");
        assert_eq!(b.standing["r"].used, 1);
    }

    /// What the operations kept is bounded: final ones with nothing left to
    /// hand on past [`FINALS`] — declined and expired ones among them — a
    /// proposed one [`LAPSED_FOR_MS`] after it expired, and proposed ones past
    /// [`PROPOSED`], the expired first. None whose confirm is in flight or
    /// unanswered, and none confirmed and not final.
    #[test]
    fn what_the_operations_kept_is_bounded() {
        let now = crate::obs::log::rfc3339_ms("2026-10-10T00:00:00Z").unwrap();
        let mut b = Book::default();
        for i in 0..FINALS as u64 + 5 {
            b.operations
                .insert(format!("declined_{i:03}"), op("cancelled", 100 + i));
        }
        let mut told = op("succeeded", 1);
        told.confirmation = Some(json!({}));
        told.told = true;
        b.operations.insert("told".into(), told.clone());
        let untold = Operation {
            told: false,
            ..told.clone()
        };
        b.operations.insert("untold".into(), untold);
        b.operations.insert("running".into(), op("in_progress", 0));
        let lapsed = |at: &str| Operation {
            expires_at: Some(at.into()),
            ..op("proposed", 2)
        };
        b.operations
            .insert("lapsed".into(), lapsed("2026-10-08T00:00:00Z"));
        b.operations.insert(
            "lapsed_unanswered".into(),
            Operation {
                unanswered: true,
                ..lapsed("2026-10-08T00:00:00Z")
            },
        );
        b.operations.insert(
            "lapsed_locked".into(),
            Operation {
                lock: Some("life/1".into()),
                ..lapsed("2026-10-08T00:00:00Z")
            },
        );
        b.standing
            .entry("r".into())
            .or_default()
            .reserved
            .push("lapsed".into());
        b.trim_operations(now);
        let kept = |k: &str| b.operations.contains_key(k);
        let declined = b
            .operations
            .keys()
            .filter(|k| k.starts_with("declined_"))
            .count();
        assert_eq!(declined + usize::from(kept("told")), FINALS);
        assert!(!kept("declined_000") && kept(&format!("declined_{:03}", FINALS + 4)));
        assert!(kept("untold") && kept("running"), "nothing left unsaid");
        assert!(!kept("lapsed"), "a day after it expired");
        assert!(
            b.standing["r"].reserved.is_empty(),
            "its reservation with it"
        );
        assert!(kept("lapsed_unanswered") && kept("lapsed_locked"));
        // Past the bound on proposed ones, the expired go first, then the
        // oldest; one whose confirm is in flight stays.
        let mut b = Book::default();
        b.operations.insert(
            "locked".into(),
            Operation {
                lock: Some("life/1".into()),
                ..op("proposed", 0)
            },
        );
        for i in 0..PROPOSED as u64 {
            b.operations
                .insert(format!("open_{i:03}"), op("proposed", 10 + i));
        }
        b.operations.insert(
            "expired".into(),
            Operation {
                expires_at: Some("2026-10-09T23:00:00Z".into()),
                ..op("proposed", 9_999)
            },
        );
        b.trim_operations(now);
        assert_eq!(b.operations.len(), PROPOSED);
        assert!(kept_in(&b, "locked") && !kept_in(&b, "expired"));
        assert!(!kept_in(&b, "open_000") && kept_in(&b, "open_001"));
    }

    fn kept_in(b: &Book, k: &str) -> bool {
        b.operations.contains_key(k)
    }

    /// An event id handled once is not handled again, and the ring keeps the
    /// newest.
    #[test]
    fn an_event_is_handled_once() {
        let mut ring = Vec::new();
        assert!(remember(&mut ring, "evt_1"));
        assert!(!remember(&mut ring, "evt_1"));
        for i in 0..RING {
            remember(&mut ring, &format!("e{i}"));
        }
        assert_eq!(ring.len(), RING);
        assert!(remember(&mut ring, "evt_1"), "forgotten past the ring");
    }
}
