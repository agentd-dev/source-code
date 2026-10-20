// SPDX-License-Identifier: AGPL-3.0-only
//! Account Tokens (RFC 0047 §5.6, draft §4.8): the refresh token a sign-in
//! gives agentd, which "stands for the User's approval for this Personal
//! Agent to use their account" at one Company.
//!
//! The User may hold several accounts at one Company — a personal and a
//! business one, say (draft §4.4: "A User with two accounts at the same
//! Company … has an Account Token for each and uses separate Sessions for
//! them"). Each is named by a **label** the User's side chooses when it signs
//! in ([`is_label`]; [`DEFAULT_LABEL`] when none is named): agentd's own
//! word, which never reaches the Company. At most [`MAX_ACCOUNTS`] are held
//! at one Company.
//!
//! One record per (User principal, Company issuer, label), kept durably as a
//! [`Kind::Cred`] record sealed with AES-256-GCM under
//! `security.companies.seal_key` — associated data `("company-account-token",
//! client_id, issuer, principal, label, "1")` — under an id that is an
//! HMAC-SHA256 of `issuer|principal|label` under a key derived from the seal
//! key, so a store that holds it can read neither the token nor which Company
//! or account it is for. The record also keeps the `session_id` of the
//! account's own Session. The seal key and the keys the ids are derived
//! under are agentd's, or the external signer's under `key_custody:
//! external` ([`super::keys`]). Which labels are held at a Company is one more
//! sealed record per (principal, issuer), under an id of its own key, and so
//! is which labels held an account that was given up since — signed out of,
//! lost to `invalid_grant` or taken by a reset — at most [`GONE_KEPT`], so a
//! later sign-in to one is known to be one the Company may sign in to
//! another account ([`Accounts::given_up`]). Load refuses a sign-in without
//! the seal key, so no Account Token is ever kept in the clear.
//!
//! The plaintext is read from the store by each request that uses it and
//! held only for that request: never cached, never logged, never put in a
//! URL, a model's context or a child's payload (§4.8: "MUST store Account
//! Tokens as secrets, and MUST NOT put them in model context, conversation
//! messages, logs, or URLs"). A token the Company rotates is kept here before
//! the response that carried it is used (§4.8: "The Personal Agent then MUST
//! use the new one from then on"), so a restart resumes with it. A token is
//! forgotten only when the Company says it is no longer good (`invalid_grant`)
//! or the User signs out of its account.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::keys::{KeyError, Keyring};
use crate::auth::seal::associated_data;
use crate::state::{Durable, Kind};

/// The record version the associated data names.
const VERSION: &str = "1";
/// The labels the record-id keys are derived from.
pub const ID_LABEL: &[u8] = b"agentd company-account record id";
pub const INDEX_LABEL: &[u8] = b"agentd company-account index id";
/// The label of an account a sign-in names none for.
pub const DEFAULT_LABEL: &str = crate::config::settings::companies::DEFAULT_ACCOUNT;
/// The most accounts the User holds at one Company.
pub const MAX_ACCOUNTS: usize = 8;
/// The most labels whose account was given up are kept, the latest; once one
/// is dropped, every label not held counts as given up.
pub const GONE_KEPT: usize = 64;

/// Whether `label` is one an account may be named by: 1 to 32 of `a`–`z`,
/// `0`–`9`, `_` and `-`.
pub fn is_label(label: &str) -> bool {
    crate::config::settings::companies::is_account_label(label)
}

/// Why `label` names no account, in words a refusal uses.
pub fn label_refused(label: &str) -> String {
    format!(
        "{} is not an account label (1 to 32 of a-z, 0-9, _ and -)",
        super::quoted(label, 40)
    )
}

/// One Account Token, as only the supervisor ever holds it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub token: String,
    /// The account scopes it grants.
    pub scope: Vec<String>,
    /// When it stops being good, if the Company said
    /// (`refresh_token_expires_in`), in epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// How the User signed in: `device` or `direct`.
    pub kind: String,
    /// The account's own Session, signed in with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("token", &"***")
            .field("scope", &self.scope)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("kind", &self.kind)
            .field("session_id", &self.session_id.as_ref().map(|_| "***"))
            .finish()
    }
}

impl Account {
    /// Whether it grants every one of `scopes`.
    pub fn covers(&self, scopes: &[String]) -> bool {
        scopes.iter().all(|s| self.scope.contains(s))
    }
}

/// Where Account Tokens are kept.
pub struct Accounts {
    durable: Arc<Durable>,
    client_id: String,
    /// The seal key, and the keys the ids of the records and of which
    /// labels are held are derived under ([`ID_LABEL`], [`INDEX_LABEL`]).
    key: Arc<Keyring>,
    /// Held while a record of which labels are held is read and written.
    lock: Mutex<()>,
}

/// Which labels are held at one Company, and which held an account that was
/// given up since.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Held {
    labels: Vec<String>,
    /// Not held now, and held before: the oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    gone: Vec<String>,
    /// More than [`GONE_KEPT`] were given up, and the oldest dropped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    gone_over: bool,
}

impl Accounts {
    /// `key` is the seal key.
    pub fn new(durable: Arc<Durable>, client_id: &str, key: Arc<Keyring>) -> Accounts {
        Accounts {
            durable,
            client_id: client_id.to_string(),
            key,
            lock: Mutex::new(()),
        }
    }

    fn id(&self, issuer: &str, principal: &str, label: &str) -> Result<String, KeyError> {
        self.key
            .record_id(ID_LABEL, &format!("{issuer}|{principal}|{label}"))
    }

    fn ad(&self, issuer: &str, principal: &str, label: &str) -> Vec<u8> {
        associated_data(&[
            b"company-account-token",
            self.client_id.as_bytes(),
            issuer.as_bytes(),
            principal.as_bytes(),
            label.as_bytes(),
            VERSION.as_bytes(),
        ])
    }

    fn index_id(&self, issuer: &str, principal: &str) -> Result<String, KeyError> {
        self.key
            .record_id(INDEX_LABEL, &format!("{issuer}|{principal}"))
    }

    fn index_ad(&self, issuer: &str, principal: &str) -> Vec<u8> {
        associated_data(&[
            b"company-accounts",
            self.client_id.as_bytes(),
            issuer.as_bytes(),
            principal.as_bytes(),
            VERSION.as_bytes(),
        ])
    }

    /// The sealed record at `id`, opened with `ad`; `what` names it in an
    /// error. A record that does not open with the seal key is an error, not
    /// a missing one.
    fn open(&self, id: &str, ad: &[u8], what: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(env) = self
            .durable
            .get(Kind::Cred, id)
            .map_err(|e| format!("company account tokens: {e}"))?
            .filter(|env| !env.state.is_null())
        else {
            return Ok(None);
        };
        self.key
            .open(ad, &env.state)
            .map(Some)
            .map_err(|e| match e {
                KeyError::Unavailable(e) => e,
                KeyError::Refused(_) => {
                    format!("{what} record does not open with security.companies' seal key")
                }
            })
    }

    fn put(&self, id: &str, ad: &[u8], bytes: &[u8]) -> Result<(), String> {
        let mut sealed = self.key.seal(ad, bytes)?;
        sealed.insert("v".into(), json!(VERSION));
        self.durable
            .put(Kind::Cred, id, serde_json::Value::Object(sealed), None)
            .map(|_| ())
            .map_err(|e| format!("company account tokens: {e}"))
    }

    fn read_held(&self, principal: &str, issuer: &str) -> Result<Held, String> {
        match self.open(
            &self.index_id(issuer, principal)?,
            &self.index_ad(issuer, principal),
            "a held-accounts",
        )? {
            Some(b) => {
                serde_json::from_slice(&b).map_err(|_| "a held-accounts record is not one".into())
            }
            None => Ok(Held::default()),
        }
    }

    fn write_held(&self, principal: &str, issuer: &str, held: &Held) -> Result<(), String> {
        let bytes = serde_json::to_vec(held).map_err(|e| e.to_string())?;
        self.put(
            &self.index_id(issuer, principal)?,
            &self.index_ad(issuer, principal),
            &bytes,
        )
    }

    /// The labels of the accounts the User holds at the Company `issuer`
    /// names, in the order they were first signed in to.
    pub fn labels(&self, principal: &str, issuer: &str) -> Result<Vec<String>, String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.read_held(principal, issuer)?.labels)
    }

    /// Whether the label `label` held an account at the Company `issuer`
    /// names that was given up since, and holds none now: a sign-in to it in
    /// a new Session may be to another account than that one.
    pub fn given_up(&self, principal: &str, issuer: &str, label: &str) -> Result<bool, String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let held = self.read_held(principal, issuer)?;
        let has = |v: &[String]| v.iter().any(|l| l == label);
        Ok(!has(&held.labels) && (held.gone_over || has(&held.gone)))
    }

    /// The User's Account Token for the account `label` at the Company
    /// `issuer` names, if one is kept. A record that does not open with the
    /// seal key is an error, not a missing token: the token is still the
    /// User's, at the Company.
    pub fn load(
        &self,
        principal: &str,
        issuer: &str,
        label: &str,
    ) -> Result<Option<Account>, String> {
        let Some(open) = self.open(
            &self.id(issuer, principal, label)?,
            &self.ad(issuer, principal, label),
            "an Account Token",
        )?
        else {
            return Ok(None);
        };
        serde_json::from_slice(&open)
            .map(Some)
            .map_err(|_| "an Account Token record is not one".to_string())
    }

    /// Keep `account` as the User's Account Token for `label` at `issuer`,
    /// sealed — in place of the one kept before. A label not held yet is
    /// refused once [`MAX_ACCOUNTS`] are.
    pub fn keep(
        &self,
        principal: &str,
        issuer: &str,
        label: &str,
        account: &Account,
    ) -> Result<(), String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut held = self.read_held(principal, issuer)?;
        let new = !held.labels.iter().any(|l| l == label);
        if new && held.labels.len() >= MAX_ACCOUNTS {
            return Err(full(&held.labels));
        }
        let bytes = serde_json::to_vec(account).map_err(|e| e.to_string())?;
        self.put(
            &self.id(issuer, principal, label)?,
            &self.ad(issuer, principal, label),
            &bytes,
        )?;
        if new {
            held.labels.push(label.to_string());
            held.gone.retain(|l| l != label);
            self.write_held(principal, issuer, &held)?;
        }
        Ok(())
    }

    /// Forget the User's Account Token for `label` at `issuer`: the label
    /// is given up.
    pub fn forget(&self, principal: &str, issuer: &str, label: &str) -> Result<(), String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.durable
            .delete(Kind::Cred, &self.id(issuer, principal, label)?)
            .map_err(|e| format!("company account tokens: {e}"))?;
        let mut held = self.read_held(principal, issuer)?;
        if held.labels.iter().any(|l| l == label) {
            held.labels.retain(|l| l != label);
            held.gone.retain(|l| l != label);
            held.gone.push(label.to_string());
            if held.gone.len() > GONE_KEPT {
                held.gone.remove(0);
                held.gone_over = true;
            }
            self.write_held(principal, issuer, &held)?;
        }
        Ok(())
    }
}

/// Why no more accounts are kept at a Company holding `labels`.
pub fn full(labels: &[String]) -> String {
    format!(
        "the User holds {MAX_ACCOUNTS} accounts there already ({}), the most agentd keeps at one Company: sign out of one first",
        labels.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::memory::MemoryStore;

    fn accounts(store: &Arc<MemoryStore>, seal: u8) -> Accounts {
        let durable = Arc::new(Durable::new(
            Arc::clone(store) as Arc<dyn Store>,
            "t/",
            "t",
            crate::state::Policy::default(),
            None,
        ));
        Accounts::new(
            durable,
            "https://a.example/c.json",
            Arc::new(Keyring::local([seal; 32]).unwrap()),
        )
    }

    fn account(token: &str) -> Account {
        Account {
            token: token.into(),
            scope: vec!["poppy:read".into()],
            expires_at_ms: None,
            kind: "device".into(),
            session_id: Some("ses_secret".into()),
        }
    }

    fn dump(store: &MemoryStore) -> String {
        store
            .dump()
            .into_iter()
            .map(|(k, _)| format!("{k} {:?}", store.get(&k, None)))
            .collect()
    }

    /// Sealed, the store holds neither the token, its Session, the issuer
    /// it is for nor its label, and only the seal key it was kept under
    /// opens it — for that issuer, principal and label alone. A token kept
    /// in place of another replaces it, and a forgotten one is gone.
    #[test]
    fn an_account_token_is_kept_sealed_and_opens_only_where_it_was_kept() {
        let store = Arc::new(MemoryStore::new());
        let a = accounts(&store, 3);
        a.keep(
            "user:dana",
            "https://as.one",
            "work-lbl",
            &account("pat_secret_one"),
        )
        .unwrap();
        let d = dump(&store);
        for hidden in ["pat_secret_one", "as.one", "work-lbl", "ses_secret"] {
            assert!(!d.contains(hidden), "{hidden}: {d}");
        }
        assert_eq!(
            a.load("user:dana", "https://as.one", "work-lbl").unwrap(),
            Some(account("pat_secret_one"))
        );
        assert_eq!(
            a.load("user:dana", "https://as.two", "work-lbl").unwrap(),
            None
        );
        assert_eq!(
            a.load("user:dana", "https://as.one", "default").unwrap(),
            None
        );
        assert_eq!(
            a.load("operator", "https://as.one", "work-lbl").unwrap(),
            None
        );
        let e = accounts(&store, 4)
            .load("user:dana", "https://as.one", "work-lbl")
            .unwrap_or(None);
        assert_eq!(e, None, "another key derives other ids");
        // The record moved to another label's id does not open there.
        let (from, to) = (
            a.id("https://as.one", "user:dana", "work-lbl").unwrap(),
            a.id("https://as.one", "user:dana", "default").unwrap(),
        );
        let env = a.durable.get(Kind::Cred, &from).unwrap().unwrap();
        a.durable.put(Kind::Cred, &to, env.state, None).unwrap();
        assert!(a.load("user:dana", "https://as.one", "default").is_err());
        a.durable.delete(Kind::Cred, &to).unwrap();
        a.keep(
            "user:dana",
            "https://as.one",
            "work-lbl",
            &account("pat_secret_two"),
        )
        .unwrap();
        assert_eq!(
            a.load("user:dana", "https://as.one", "work-lbl")
                .unwrap()
                .unwrap()
                .token,
            "pat_secret_two"
        );
        a.forget("user:dana", "https://as.one", "work-lbl").unwrap();
        assert_eq!(
            a.load("user:dana", "https://as.one", "work-lbl").unwrap(),
            None
        );
        assert!(!format!("{:?}", account("pat_x")).contains("pat_x"));
        assert!(!format!("{:?}", account("pat_x")).contains("ses_secret"));
    }

    /// Each label is held apart: forgetting one leaves the others, the
    /// labels are listed in the order first kept, and one more than
    /// [`MAX_ACCOUNTS`] is refused — while a held one is kept again.
    #[test]
    fn each_account_is_held_apart_up_to_the_most_kept() {
        let store = Arc::new(MemoryStore::new());
        let a = accounts(&store, 9);
        let (p, i) = ("operator", "https://as");
        a.keep(p, i, "home", &account("pat_home")).unwrap();
        a.keep(p, i, "work", &account("pat_work")).unwrap();
        assert_eq!(a.labels(p, i).unwrap(), ["home", "work"]);
        a.forget(p, i, "home").unwrap();
        assert_eq!(a.labels(p, i).unwrap(), ["work"]);
        assert_eq!(a.load(p, i, "work").unwrap().unwrap().token, "pat_work");
        assert!(a.labels(p, "https://other").unwrap().is_empty());
        for n in 0..MAX_ACCOUNTS - 1 {
            a.keep(p, i, &format!("a{n}"), &account("pat")).unwrap();
        }
        assert_eq!(a.labels(p, i).unwrap().len(), MAX_ACCOUNTS);
        let e = a.keep(p, i, "one-more", &account("pat")).unwrap_err();
        assert!(e.contains("sign out of one first"), "{e}");
        assert_eq!(a.load(p, i, "one-more").unwrap(), None, "nothing kept");
        a.keep(p, i, "work", &account("pat_work_2")).unwrap();
        assert_eq!(a.labels(p, i).unwrap().len(), MAX_ACCOUNTS);
    }

    /// A label whose account was given up is remembered, sealed, until it
    /// holds one again — and past [`GONE_KEPT`] of them, every label not
    /// held counts as given up.
    #[test]
    fn a_label_given_up_is_remembered_until_it_holds_an_account_again() {
        let store = Arc::new(MemoryStore::new());
        let a = accounts(&store, 5);
        let (p, i) = ("operator", "https://as");
        assert!(!a.given_up(p, i, "home").unwrap(), "never held");
        a.keep(p, i, "home", &account("pat_home")).unwrap();
        assert!(!a.given_up(p, i, "home").unwrap(), "held");
        a.forget(p, i, "home").unwrap();
        assert!(a.given_up(p, i, "home").unwrap());
        assert!(!a.given_up(p, "https://other", "home").unwrap());
        assert!(!dump(&store).contains("home"), "sealed");
        assert!(
            accounts(&store, 5).given_up(p, i, "home").unwrap(),
            "kept in the store"
        );
        a.keep(p, i, "home", &account("pat_home_2")).unwrap();
        assert!(!a.given_up(p, i, "home").unwrap(), "held again");
        for n in 0..=GONE_KEPT {
            let l = format!("g{n}");
            a.keep(p, i, &l, &account("pat")).unwrap();
            a.forget(p, i, &l).unwrap();
        }
        assert!(
            a.given_up(p, i, "g0").unwrap(),
            "dropped, and still counted"
        );
        assert!(a.given_up(p, i, "never-held").unwrap());
        assert!(!a.given_up(p, i, "home").unwrap(), "held");
    }

    #[test]
    fn an_account_token_covers_the_scopes_it_grants() {
        let mut a = account("t");
        a.scope = vec!["poppy:read".into(), "poppy:write".into()];
        assert!(a.covers(&["poppy:read".into()]));
        assert!(a.covers(&[]));
        assert!(!a.covers(&["addresses".into()]));
    }

    #[test]
    fn a_label_is_agentds_own_short_word() {
        for ok in ["default", "work", "home-2", "a_b", &"x".repeat(32)] {
            assert!(is_label(ok), "{ok}");
        }
        for bad in ["", "Work", "a b", "a/b", "é", &"x".repeat(33)] {
            assert!(!is_label(bad), "{bad}");
        }
    }
}
