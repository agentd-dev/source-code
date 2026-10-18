// SPDX-License-Identifier: AGPL-3.0-only
//! What agentd keeps of its work at one Company for the User (RFC 0047
//! §5.6): the conversations it opened and where each was read to, and the
//! id of each message and the `Idempotency-Key` of each API write it may
//! send again.
//!
//! One [`Kind::Company`] record per (User principal, Company issuer), so a
//! conversation outlives the Session that started it (draft §7.11) and a
//! restart resumes each from its saved cursor. With
//! `security.companies.seal_key` the record is sealed (AES-256-GCM,
//! associated data `("company-ledger", client_id, issuer, principal, "1")`)
//! under an id that is an HMAC-SHA256 of `issuer|principal` under a key
//! derived from the seal key, so a store that holds it can read neither the
//! conversations nor which Company they are at; without one it is plain
//! under `sha256("company-ledger|" + issuer + "|" + principal)`, which load
//! allows only with a local store, as it does the User ID records.
//!
//! Every change is read, made and written under one lock, so the readers of
//! several conversations and the calls in flight never write over each
//! other's cursor. What it remembers is bounded: the last [`RING`] event ids
//! of each read, the last [`KEPT`] message ids and keys, and the last
//! [`CLOSED`] closed conversations.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::seal::{SealKey, associated_data};
use crate::b64;
use crate::state::{Durable, Kind};

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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Keep `id` as an open conversation at `company`, unless it is kept.
    pub fn open(&mut self, id: &str, company: &str) {
        self.conversations
            .entry(id.to_string())
            .or_insert_with(|| Conversation {
                company: company.to_string(),
                open: true,
                opened_ms: crate::state::now_ms(),
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
    seal: Option<(SealKey, [u8; 32])>,
    lock: Mutex<()>,
}

impl Ledger {
    /// `seal`, when given, is `security.companies.seal_key`'s bytes.
    pub fn new(
        durable: Arc<Durable>,
        client_id: &str,
        seal: Option<[u8; 32]>,
    ) -> Result<Ledger, String> {
        let seal = match seal {
            Some(raw) => Some((
                SealKey::new(&raw).map_err(|e| format!("security.companies.seal_key: {e}"))?,
                crate::sha::hmac_sha256(&raw, b"agentd company-ledger record id"),
            )),
            None => None,
        };
        Ok(Ledger {
            durable,
            client_id: client_id.to_string(),
            seal,
            lock: Mutex::new(()),
        })
    }

    fn id(&self, issuer: &str, principal: &str) -> String {
        match &self.seal {
            Some((_, key)) => {
                crate::sha::hmac_sha256(key, format!("{issuer}|{principal}").as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect()
            }
            None => {
                crate::sha::sha256_hex(format!("company-ledger|{issuer}|{principal}").as_bytes())
            }
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
            .get(Kind::Company, &self.id(issuer, principal))
            .map_err(|e| format!("company ledger: {e}"))?
            .filter(|env| !env.state.is_null())
        else {
            return Ok(Book::default());
        };
        let bytes = match &self.seal {
            Some((key, _)) => {
                let sealed = b64::url_decode(env.state["sealed"].as_str().unwrap_or_default())
                    .unwrap_or_default();
                key.open(&self.ad(issuer, principal), &sealed)
                    .map_err(|_| {
                        "a company ledger record does not open with security.companies.seal_key"
                            .to_string()
                    })?
            }
            None => serde_json::to_vec(&env.state["book"]).map_err(|e| e.to_string())?,
        };
        serde_json::from_slice(&bytes).map_err(|_| "a company ledger record is not one".to_string())
    }

    fn write(&self, principal: &str, issuer: &str, book: &Book) -> Result<(), String> {
        let state = match &self.seal {
            Some((key, _)) => {
                let bytes = serde_json::to_vec(book).map_err(|e| e.to_string())?;
                let sealed = key.seal(&self.ad(issuer, principal), &bytes)?;
                json!({"v": VERSION, "sealed": b64::url_nopad(&sealed)})
            }
            None => json!({"v": VERSION, "book": book}),
        };
        self.durable
            .put(Kind::Company, &self.id(issuer, principal), state, None)
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
            .delete(Kind::Company, &self.id(issuer, principal))
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
        Ledger::new(durable, "https://a.example/c.json", seal.map(|s| [s; 32])).unwrap()
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
            b.open("cnv_secret_one", "shop");
            Ok(())
        })
        .unwrap();
        let d = dump(&store);
        assert!(
            !d.contains("cnv_secret_one") && !d.contains("as.one"),
            "{d}"
        );
        let book = l.load("user:dana", "https://as.one").unwrap();
        assert!(book.conversations["cnv_secret_one"].open);
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
                    b.open(&id, "shop");
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
