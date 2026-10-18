// SPDX-License-Identifier: AGPL-3.0-only
//! User IDs (RFC 0047 §5.5.3, draft §4.2): the pseudonymous ID each Company
//! knows the User by, one per (User principal, Company issuer).
//!
//! An ID is `usr_` and 16 random bytes, base64url, minted the first time the
//! pair needs one — random, so it is not derived from anything about the
//! person or the principal, keyed or not (§4.2: "MUST NOT contain or be
//! derived from personal information … even with a keyed hash") — and kept
//! durably as a [`Kind::Cred`] record, so the Company sees the same User
//! across Sessions and restarts. The record also holds the Session's
//! `session_id`, and a random secret of the pair's own.
//!
//! With `security.companies.seal_key` the record is sealed (AES-256-GCM,
//! associated data `("company-user", client_id, issuer, principal, "1")`)
//! and its id is an HMAC-SHA256 of `issuer|principal` under a key derived
//! from the seal key, so a store that holds it can read neither the User ID
//! nor which Company it is for. Without one the id is
//! `sha256("company-user|" + issuer + "|" + principal)` and the record is
//! plain, which load allows only with a local store. A plain record is
//! sealed the first time it is read with a seal key set. A seal key that
//! opens no sealed record makes every Company unavailable rather than mint a
//! new User ID, because a new User ID is a new User at every Company: a
//! sealed check record says which key the store's records were sealed with,
//! and a store that holds one with no seal key set is refused the same way.
//! The operator starts over one Company at a time
//! ([`UserIds::start_over_at`]); once every Company has, the store takes the
//! key ([`UserIds::start_over`]).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::seal::{SealKey, associated_data};
use crate::b64;
use crate::state::{Durable, Kind};

/// The record version the associated data names.
const VERSION: &str = "1";

/// One User at one Company.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRecord {
    pub user_id: String,
    /// A random secret of this (User, Company) pair's own, never sent.
    pub secret: String,
    /// The Session agentd last held there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// The seal key's 32 bytes, decoded from its secret: 64 hex digits, or
/// base64 (standard or URL-safe, padded or not).
pub fn seal_bytes(text: &str) -> Result<[u8; 32], String> {
    let bad = || "security.companies.seal_key: not 32 bytes in hex or base64".to_string();
    let t = text.trim();
    let bytes = if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        (0..32)
            .map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).map_err(|_| bad()))
            .collect::<Result<Vec<u8>, String>>()?
    } else {
        b64::url_decode(&t.replace('+', "-").replace('/', "_").replace('=', ""))
            .map_err(|_| bad())?
    };
    bytes.try_into().map_err(|_| bad())
}

/// Where User IDs are kept.
pub struct UserIds {
    durable: Arc<Durable>,
    client_id: String,
    seal: Option<(SealKey, [u8; 32])>,
}

impl UserIds {
    /// `seal`, when given, is `security.companies.seal_key`'s bytes: the
    /// records are sealed with it, under ids keyed by a key derived from it.
    pub fn new(
        durable: Arc<Durable>,
        client_id: &str,
        seal: Option<[u8; 32]>,
    ) -> Result<UserIds, String> {
        let seal = match seal {
            Some(raw) => Some((
                SealKey::new(&raw).map_err(|e| format!("security.companies.seal_key: {e}"))?,
                crate::sha::hmac_sha256(&raw, b"agentd company-user record id"),
            )),
            None => None,
        };
        Ok(UserIds {
            durable,
            client_id: client_id.to_string(),
            seal,
        })
    }

    fn plain_id(issuer: &str, principal: &str) -> String {
        crate::sha::sha256_hex(format!("company-user|{issuer}|{principal}").as_bytes())
    }

    fn sealed_id(key: &[u8; 32], issuer: &str, principal: &str) -> String {
        crate::sha::hmac_sha256(key, format!("{issuer}|{principal}").as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn ad(&self, issuer: &str, principal: &str) -> Vec<u8> {
        associated_data(&[
            b"company-user",
            self.client_id.as_bytes(),
            issuer.as_bytes(),
            principal.as_bytes(),
            VERSION.as_bytes(),
        ])
    }

    /// Check the seal key is the one the store's records were sealed with,
    /// writing the check record the first time a key is set. A key that does
    /// not open it, or no key for a store that has one, is
    /// `company.seal_key.mismatch`: either would mint a new User ID.
    pub fn check_seal(&self) -> Result<(), String> {
        let id = crate::sha::sha256_hex(b"company-seal-check");
        let ad = associated_data(&[b"company-seal-check", self.client_id.as_bytes()]);
        let check = self
            .durable
            .get(Kind::Cred, &id)
            .map_err(|e| e.to_string())?
            .filter(|env| !env.state.is_null());
        let Some((key, _)) = &self.seal else {
            return match check {
                Some(_) => Err("the store's User ID records are sealed, and security.companies.seal_key is not set: set the key they were sealed with (without it every Company would see a new User)".to_string()),
                None => Ok(()),
            };
        };
        match check {
            Some(env) => {
                let sealed = env.state["sealed"].as_str().unwrap_or_default();
                let bytes = b64::url_decode(sealed).unwrap_or_default();
                key.open(&ad, &bytes).map(|_| ()).map_err(|_| {
                    "security.companies.seal_key opens none of the sealed User ID records: restore the key the store was sealed with (a new key would make a new User at every Company)".to_string()
                })
            }
            _ => {
                let sealed = key.seal(&ad, b"ok")?;
                self.durable
                    .put(
                        Kind::Cred,
                        &id,
                        json!({"sealed": b64::url_nopad(&sealed)}),
                        None,
                    )
                    .map(|_| ())
                    .map_err(|e| format!("company user records: {e}"))
            }
        }
    }

    /// The User's record at the Company `issuer` names, minted when there is
    /// none.
    pub fn load_or_mint(&self, principal: &str, issuer: &str) -> Result<UserRecord, String> {
        if let Some(r) = self.load(principal, issuer)? {
            return Ok(r);
        }
        let token = |n| {
            crate::sec::random::b64url_token(n)
                .map_err(|e| format!("company user id: no OS randomness: {e}"))
        };
        let rec = UserRecord {
            user_id: format!("usr_{}", token(16)?),
            secret: token(32)?,
            session_id: None,
        };
        self.save(principal, issuer, &rec)?;
        Ok(rec)
    }

    /// The record, if one is kept: opened when sealed, and a plain one
    /// sealed now when a seal key is set.
    pub fn load(&self, principal: &str, issuer: &str) -> Result<Option<UserRecord>, String> {
        let plain = Self::plain_id(issuer, principal);
        match &self.seal {
            None => self.read_plain(&plain),
            Some((key, idk)) => {
                let id = Self::sealed_id(idk, issuer, principal);
                if let Some(env) = self
                    .durable
                    .get(Kind::Cred, &id)
                    .map_err(|e| e.to_string())?
                    && !env.state.is_null()
                {
                    let bytes = b64::url_decode(env.state["sealed"].as_str().unwrap_or_default())
                        .unwrap_or_default();
                    let open = key.open(&self.ad(issuer, principal), &bytes).map_err(|_| {
                        "a sealed User ID record does not open with security.companies.seal_key"
                            .to_string()
                    })?;
                    return serde_json::from_slice(&open)
                        .map(Some)
                        .map_err(|e| format!("a User ID record: {e}"));
                }
                // A record kept before the seal key was set: sealed now, and
                // the plain one removed.
                match self.read_plain(&plain)? {
                    Some(rec) => {
                        self.save(principal, issuer, &rec)?;
                        self.durable
                            .delete(Kind::Cred, &plain)
                            .map_err(|e| format!("company user records: {e}"))?;
                        Ok(Some(rec))
                    }
                    None => Ok(None),
                }
            }
        }
    }

    fn read_plain(&self, id: &str) -> Result<Option<UserRecord>, String> {
        match self
            .durable
            .get(Kind::Cred, id)
            .map_err(|e| e.to_string())?
        {
            Some(env) if !env.state.is_null() => serde_json::from_value(env.state)
                .map(Some)
                .map_err(|e| format!("a User ID record: {e}")),
            _ => Ok(None),
        }
    }

    /// Forget the pair's record — its User ID, its secret and its
    /// `session_id`: the Company sees a new User the next time one is
    /// needed. Both of the ids it may be kept under are removed.
    pub fn forget(&self, principal: &str, issuer: &str) -> Result<(), String> {
        let mut ids = vec![Self::plain_id(issuer, principal)];
        if let Some((_, idk)) = &self.seal {
            ids.push(Self::sealed_id(idk, issuer, principal));
        }
        for id in ids {
            self.durable
                .delete(Kind::Cred, &id)
                .map_err(|e| format!("company user records: {e}"))?;
        }
        Ok(())
    }

    /// The id of the record that says the operator started over at the
    /// company entry `entry` with the current seal key (or with none).
    fn started_id(&self, entry: &str) -> String {
        let what = format!("company-start-over|{entry}");
        match &self.seal {
            Some((_, idk)) => Self::sealed_id(idk, &what, "check"),
            None => crate::sha::sha256_hex(what.as_bytes()),
        }
    }

    fn started_ad(&self, entry: &str) -> Vec<u8> {
        associated_data(&[
            b"company-start-over",
            self.client_id.as_bytes(),
            entry.as_bytes(),
        ])
    }

    /// Whether the operator started over at the company entry `entry` with
    /// this seal key while it opened none of the store's records
    /// ([`Self::start_over_at`]).
    pub fn started_over(&self, entry: &str) -> bool {
        let Ok(Some(env)) = self.durable.get(Kind::Cred, &self.started_id(entry)) else {
            return false;
        };
        match &self.seal {
            Some((key, _)) => {
                let bytes = b64::url_decode(env.state["sealed"].as_str().unwrap_or_default())
                    .unwrap_or_default();
                key.open(&self.started_ad(entry), &bytes).is_ok()
            }
            None => env.state["started_over"] == true,
        }
    }

    /// The operator starts over at the company entry `entry` with the
    /// current seal key, which opens none of the store's records
    /// (`company.user_id.reset`): that Company sees a new User from its next
    /// call. Every other Company stays unavailable until it is reset too.
    pub fn start_over_at(&self, entry: &str) -> Result<(), String> {
        let value = match &self.seal {
            Some((key, _)) => {
                let sealed = key.seal(&self.started_ad(entry), b"ok")?;
                json!({"sealed": b64::url_nopad(&sealed)})
            }
            None => json!({"started_over": true}),
        };
        self.durable
            .put(Kind::Cred, &self.started_id(entry), value, None)
            .map(|_| ())
            .map_err(|e| format!("company user records: {e}"))
    }

    /// Every one of `entries` has started over: the store takes the current
    /// key — its check record sealed again with it, the entries' records of
    /// starting over removed.
    pub fn start_over(&self, entries: &[&str]) -> Result<(), String> {
        self.durable
            .delete(Kind::Cred, &crate::sha::sha256_hex(b"company-seal-check"))
            .map_err(|e| format!("company user records: {e}"))?;
        self.check_seal()?;
        for e in entries {
            self.durable
                .delete(Kind::Cred, &self.started_id(e))
                .map_err(|e| format!("company user records: {e}"))?;
        }
        Ok(())
    }

    /// Keep `rec` for the pair, sealed when a seal key is set.
    pub fn save(&self, principal: &str, issuer: &str, rec: &UserRecord) -> Result<(), String> {
        let (id, value): (String, Value) = match &self.seal {
            None => (
                Self::plain_id(issuer, principal),
                serde_json::to_value(rec).map_err(|e| e.to_string())?,
            ),
            Some((key, idk)) => {
                let bytes = serde_json::to_vec(rec).map_err(|e| e.to_string())?;
                let sealed = key.seal(&self.ad(issuer, principal), &bytes)?;
                (
                    Self::sealed_id(idk, issuer, principal),
                    json!({"v": VERSION, "sealed": b64::url_nopad(&sealed)}),
                )
            }
        };
        self.durable
            .put(Kind::Cred, &id, value, None)
            .map(|_| ())
            .map_err(|e| format!("company user records: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;

    fn durable(store: Arc<MemoryStore>) -> Arc<Durable> {
        Arc::new(Durable::new(
            store,
            "t/",
            "t",
            crate::state::Policy::default(),
            None,
        ))
    }

    /// Every key and record the store holds, as one string.
    fn all_state(store: &MemoryStore) -> String {
        use crate::store::Store;
        store
            .dump()
            .into_iter()
            .map(|(k, _)| format!("{k} {:?}", store.get(&k, None)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_user_id_is_random_stable_and_per_issuer() {
        let store = Arc::new(MemoryStore::new());
        let ids = UserIds::new(
            durable(Arc::clone(&store)),
            "https://a.example/c.json",
            None,
        )
        .unwrap();
        let a = ids.load_or_mint("operator", "https://as.one").unwrap();
        assert!(
            a.user_id.starts_with("usr_") && a.user_id.len() == 4 + 22,
            "{a:?}"
        );
        assert!(!a.user_id.contains("operator"));
        assert_eq!(
            ids.load_or_mint("operator", "https://as.one").unwrap(),
            a,
            "stable"
        );
        let b = ids.load_or_mint("operator", "https://as.two").unwrap();
        assert_ne!(a.user_id, b.user_id, "one per Company");
        // A second User ID store over the same records reads the same User.
        let again = UserIds::new(
            durable(Arc::clone(&store)),
            "https://a.example/c.json",
            None,
        )
        .unwrap();
        assert_eq!(again.load("operator", "https://as.one").unwrap(), Some(a));
    }

    /// Sealed, the store holds neither the User ID nor the issuer; a plain
    /// record is sealed when first read with the key; and neither another
    /// key nor none passes the check.
    #[test]
    fn a_sealed_record_names_nothing_and_opens_only_with_its_key() {
        let store = Arc::new(MemoryStore::new());
        let plain = UserIds::new(
            durable(Arc::clone(&store)),
            "https://a.example/c.json",
            None,
        )
        .unwrap();
        plain.check_seal().unwrap();
        let rec = plain.load_or_mint("user:dana", "https://as.one").unwrap();
        let sealed = UserIds::new(
            durable(Arc::clone(&store)),
            "https://a.example/c.json",
            Some([9u8; 32]),
        )
        .unwrap();
        sealed.check_seal().unwrap();
        // Once sealed, a store is refused to a reader without the key, which
        // would mint new User IDs.
        let e = plain.check_seal().unwrap_err();
        assert!(e.contains("seal_key is not set"), "{e}");
        assert_eq!(
            sealed.load("user:dana", "https://as.one").unwrap(),
            Some(rec.clone())
        );
        let dump = all_state(&store);
        assert!(
            !dump.contains(&rec.user_id),
            "no User ID in plaintext: {dump}"
        );
        assert!(!dump.contains("as.one"), "no issuer in plaintext");
        // The record was sealed, not lost: a reader with the key finds the
        // same User, and one without it finds none.
        let again = UserIds::new(
            durable(Arc::clone(&store)),
            "https://a.example/c.json",
            Some([9u8; 32]),
        )
        .unwrap();
        assert_eq!(
            again.load("user:dana", "https://as.one").unwrap(),
            Some(rec.clone())
        );
        assert_eq!(plain.load("user:dana", "https://as.one").unwrap(), None);
        let wrong = UserIds::new(
            durable(Arc::clone(&store)),
            "https://a.example/c.json",
            Some([7u8; 32]),
        )
        .unwrap();
        let e = wrong.check_seal().unwrap_err();
        assert!(e.contains("opens none of the sealed"), "{e}");
    }

    /// A forgotten pair is a new User the next time. With a key that opens
    /// none of the store's records, starting over at one Company is that
    /// Company's alone; once every Company has, the store takes the key.
    #[test]
    fn a_reset_forgets_the_user_and_starting_over_takes_the_new_key() {
        let store = Arc::new(MemoryStore::new());
        let old = UserIds::new(
            durable(Arc::clone(&store)),
            "https://a.example/c.json",
            Some([1u8; 32]),
        )
        .unwrap();
        old.check_seal().unwrap();
        let first = old.load_or_mint("operator", "https://as.one").unwrap();
        old.forget("operator", "https://as.one").unwrap();
        assert_eq!(old.load("operator", "https://as.one").unwrap(), None);
        let second = old.load_or_mint("operator", "https://as.one").unwrap();
        assert_ne!(first.user_id, second.user_id, "a new User");
        assert_ne!(first.secret, second.secret);
        let new = UserIds::new(
            durable(Arc::clone(&store)),
            "https://a.example/c.json",
            Some([2u8; 32]),
        )
        .unwrap();
        assert!(new.check_seal().is_err());
        new.start_over_at("shop").unwrap();
        assert!(new.started_over("shop"));
        assert!(!new.started_over("travel"), "another Company's own reset");
        assert!(
            !old.started_over("shop"),
            "said with the key it was made with"
        );
        // A record at the id that does not open with the key is not one.
        new.durable
            .put(
                Kind::Cred,
                &new.started_id("travel"),
                json!({"sealed": b64::url_nopad(b"not sealed with the key")}),
                None,
            )
            .unwrap();
        assert!(!new.started_over("travel"), "a record that does not open");
        assert!(new.check_seal().is_err(), "the store keeps the old key");
        new.start_over_at("travel").unwrap();
        new.start_over(&["shop", "travel"]).unwrap();
        new.check_seal().unwrap();
        assert!(
            !new.started_over("shop"),
            "done with once the store took the key"
        );
        assert_eq!(new.load("operator", "https://as.one").unwrap(), None);
        assert!(old.check_seal().is_err());
    }

    #[test]
    fn a_seal_key_is_hex_or_base64() {
        assert_eq!(seal_bytes(&"ab".repeat(32)).unwrap(), [0xab; 32]);
        assert_eq!(seal_bytes(&b64::std_pad(&[1u8; 32])).unwrap(), [1; 32]);
        assert_eq!(seal_bytes(&b64::url_nopad(&[1u8; 32])).unwrap(), [1; 32]);
        assert!(seal_bytes("short").is_err());
        assert!(seal_bytes(&b64::url_nopad(&[1u8; 31])).is_err());
    }
}
