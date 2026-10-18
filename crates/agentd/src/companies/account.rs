// SPDX-License-Identifier: AGPL-3.0-only
//! Account Tokens (RFC 0047 §5.6, draft §4.8): the refresh token a sign-in
//! gives agentd, which "stands for the User's approval for this Personal
//! Agent to use their account" at one Company.
//!
//! One per (User principal, Company issuer), kept durably as a
//! [`Kind::Cred`] record sealed with AES-256-GCM under
//! `security.companies.seal_key` — associated data `("company-account-token",
//! client_id, issuer, principal, "1")` — under an id that is an HMAC-SHA256 of
//! `issuer|principal` under a key derived from the seal key, so a store that
//! holds it can read neither the token nor which Company it is for. Load
//! refuses a sign-in without the seal key, so no Account Token is ever kept
//! in the clear.
//!
//! The plaintext is read from the store by each request that uses it and
//! held only for that request: never cached, never logged, never put in a
//! URL, a model's context or a child's payload (§4.8: "MUST store Account
//! Tokens as secrets, and MUST NOT put them in model context, conversation
//! messages, logs, or URLs"). A token the Company rotates is kept here before
//! the response that carried it is used (§4.8: "The Personal Agent then MUST
//! use the new one from then on"), so a restart resumes with it. A token is
//! forgotten only when the Company says it is no longer good (`invalid_grant`)
//! or the User signs out.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::seal::{SealKey, associated_data};
use crate::b64;
use crate::state::{Durable, Kind};

/// The record version the associated data names.
const VERSION: &str = "1";

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
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("token", &"***")
            .field("scope", &self.scope)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("kind", &self.kind)
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
    key: SealKey,
    /// The key record ids are derived under.
    ids: [u8; 32],
}

impl Accounts {
    /// `seal` is `security.companies.seal_key`'s bytes.
    pub fn new(durable: Arc<Durable>, client_id: &str, seal: [u8; 32]) -> Result<Accounts, String> {
        Ok(Accounts {
            durable,
            client_id: client_id.to_string(),
            key: SealKey::new(&seal).map_err(|e| format!("security.companies.seal_key: {e}"))?,
            ids: crate::sha::hmac_sha256(&seal, b"agentd company-account record id"),
        })
    }

    fn id(&self, issuer: &str, principal: &str) -> String {
        crate::sha::hmac_sha256(&self.ids, format!("{issuer}|{principal}").as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn ad(&self, issuer: &str, principal: &str) -> Vec<u8> {
        associated_data(&[
            b"company-account-token",
            self.client_id.as_bytes(),
            issuer.as_bytes(),
            principal.as_bytes(),
            VERSION.as_bytes(),
        ])
    }

    /// The User's Account Token at the Company `issuer` names, if one is
    /// kept. A record that does not open with the seal key is an error, not
    /// a missing token: the token is still the User's, at the Company.
    pub fn load(&self, principal: &str, issuer: &str) -> Result<Option<Account>, String> {
        let Some(env) = self
            .durable
            .get(Kind::Cred, &self.id(issuer, principal))
            .map_err(|e| format!("company account tokens: {e}"))?
            .filter(|env| !env.state.is_null())
        else {
            return Ok(None);
        };
        let bytes =
            b64::url_decode(env.state["sealed"].as_str().unwrap_or_default()).unwrap_or_default();
        let open = self
            .key
            .open(&self.ad(issuer, principal), &bytes)
            .map_err(|_| {
                "an Account Token record does not open with security.companies.seal_key".to_string()
            })?;
        serde_json::from_slice(&open)
            .map(Some)
            .map_err(|_| "an Account Token record is not one".to_string())
    }

    /// Keep `account` as the User's Account Token at `issuer`, sealed — in
    /// place of the one kept before.
    pub fn keep(&self, principal: &str, issuer: &str, account: &Account) -> Result<(), String> {
        let bytes = serde_json::to_vec(account).map_err(|e| e.to_string())?;
        let sealed = self.key.seal(&self.ad(issuer, principal), &bytes)?;
        self.durable
            .put(
                Kind::Cred,
                &self.id(issuer, principal),
                json!({"v": VERSION, "sealed": b64::url_nopad(&sealed)}),
                None,
            )
            .map(|_| ())
            .map_err(|e| format!("company account tokens: {e}"))
    }

    /// Forget the User's Account Token at `issuer`.
    pub fn forget(&self, principal: &str, issuer: &str) -> Result<(), String> {
        self.durable
            .delete(Kind::Cred, &self.id(issuer, principal))
            .map_err(|e| format!("company account tokens: {e}"))
    }
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
        Accounts::new(durable, "https://a.example/c.json", [seal; 32]).unwrap()
    }

    fn account(token: &str) -> Account {
        Account {
            token: token.into(),
            scope: vec!["poppy:read".into()],
            expires_at_ms: None,
            kind: "device".into(),
        }
    }

    /// Sealed, the store holds neither the token nor the issuer it is for,
    /// and only the seal key it was kept under opens it — for that issuer
    /// and principal alone. A token kept in place of another replaces it, and
    /// a forgotten one is gone.
    #[test]
    fn an_account_token_is_kept_sealed_and_opens_only_where_it_was_kept() {
        let store = Arc::new(MemoryStore::new());
        let a = accounts(&store, 3);
        a.keep("user:dana", "https://as.one", &account("pat_secret_one"))
            .unwrap();
        let dump: String = store
            .dump()
            .into_iter()
            .map(|(k, _)| format!("{k} {:?}", store.get(&k, None)))
            .collect();
        assert!(!dump.contains("pat_secret_one"), "{dump}");
        assert!(!dump.contains("as.one"), "{dump}");
        assert_eq!(
            a.load("user:dana", "https://as.one").unwrap(),
            Some(account("pat_secret_one"))
        );
        assert_eq!(a.load("user:dana", "https://as.two").unwrap(), None);
        assert_eq!(a.load("operator", "https://as.one").unwrap(), None);
        let e = accounts(&store, 4)
            .load("user:dana", "https://as.one")
            .unwrap_or(None);
        assert_eq!(e, None, "another key derives other ids");
        // The record moved to another pair's id does not open there.
        let (from, to) = (
            a.id("https://as.one", "user:dana"),
            a.id("https://as.two", "user:dana"),
        );
        let env = a.durable.get(Kind::Cred, &from).unwrap().unwrap();
        a.durable.put(Kind::Cred, &to, env.state, None).unwrap();
        assert!(a.load("user:dana", "https://as.two").is_err());
        a.keep("user:dana", "https://as.one", &account("pat_secret_two"))
            .unwrap();
        assert_eq!(
            a.load("user:dana", "https://as.one")
                .unwrap()
                .unwrap()
                .token,
            "pat_secret_two"
        );
        a.forget("user:dana", "https://as.one").unwrap();
        assert_eq!(a.load("user:dana", "https://as.one").unwrap(), None);
        assert!(!format!("{:?}", account("pat_x")).contains("pat_x"));
    }

    #[test]
    fn an_account_token_covers_the_scopes_it_grants() {
        let mut a = account("t");
        a.scope = vec!["poppy:read".into(), "poppy:write".into()];
        assert!(a.covers(&["poppy:read".into()]));
        assert!(a.covers(&[]));
        assert!(!a.covers(&["addresses".into()]));
    }
}
