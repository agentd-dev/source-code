// SPDX-License-Identifier: AGPL-3.0-only
//! Who holds the companies preview's seal key (RFC 0047 §5.5.3; RFC 0044
//! §7.P2b): the key the User ID, Account Token and ledger records are
//! sealed with, and the record-id keys derived from it.
//!
//! Under `security.companies.key_custody: local` (the default) the key is
//! `security.companies.seal_key`'s 32 bytes, read by the supervisor: a
//! record is sealed with it, and each store's record-id key is the
//! HMAC-SHA256 of a fixed label under it. Under `key_custody: external`
//! agentd holds neither: the external signer named by
//! `security.companies.signer` wraps each record's own data key under its
//! key-encryption key (`signer.seal_key`), and derives the record-id keys
//! by the same HMAC under its derivation key (`signer.derive_key`) — the
//! `derive` op, asked once a process for each label and kept. The ids keep
//! their shape: 64 hex digits of an HMAC-SHA256.
//!
//! A record sealed under one custody does not open under the other. A
//! signer that does not answer is [`KeyError::Unavailable`] — never a key
//! that opens none of the store's records, which would start the User over
//! at a Company.

use std::sync::Mutex;

use serde_json::{Map, Value, json};

use crate::auth::seal::{SealKey, associated_data};
use crate::b64;
use crate::sec::signer::{Signer, SignerError};

/// Why a key did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// The record does not open with the key it names, or is sealed under
    /// the other custody.
    Refused(String),
    /// The external signer did not answer: nothing was sealed or opened.
    Unavailable(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Refused(e) | KeyError::Unavailable(e) => f.write_str(e),
        }
    }
}

impl From<KeyError> for String {
    fn from(e: KeyError) -> String {
        e.to_string()
    }
}

/// The seal key, wherever it is held.
pub enum Keyring {
    /// `security.companies.seal_key`'s bytes.
    Local {
        key: Box<SealKey>,
        raw: Box<[u8; 32]>,
    },
    /// The signer's key-encryption key `seal_key` and derivation key
    /// `derive_key`; the record-id keys it derived, by label.
    External {
        signer: Signer,
        seal_key: String,
        derive_key: String,
        derived: Mutex<Vec<(Vec<u8>, [u8; 32])>>,
    },
}

impl Drop for Keyring {
    fn drop(&mut self) {
        match self {
            Keyring::Local { raw, .. } => crate::sec::harden::wipe(&mut raw[..]),
            Keyring::External { derived, .. } => {
                for (_, k) in derived
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter_mut()
                {
                    crate::sec::harden::wipe(k);
                }
            }
        }
    }
}

/// What a record's data key is wrapped under: its own associated data.
fn wrap_ad(ad: &[u8]) -> Vec<u8> {
    associated_data(&[b"company-record-key", ad])
}

fn signer_error(e: SignerError) -> KeyError {
    match &e {
        SignerError::Refused { code, .. } if code == "unwrap_failed" => {
            KeyError::Refused(format!("a company record's data key: {e}"))
        }
        _ => KeyError::Unavailable(format!("security.companies.signer: {e}")),
    }
}

impl Keyring {
    /// `security.companies.seal_key`'s bytes.
    pub fn local(raw: [u8; 32]) -> Result<Keyring, String> {
        Ok(Keyring::Local {
            key: Box::new(
                SealKey::new(&raw).map_err(|e| format!("security.companies.seal_key: {e}"))?,
            ),
            raw: Box::new(raw),
        })
    }

    /// Where an operator sets the key, for a message that tells them to
    /// restore it: the local key's setting, or the signer's key id.
    pub fn setting(&self) -> String {
        match self {
            Keyring::Local { .. } => "security.companies.seal_key".into(),
            Keyring::External { seal_key, .. } => {
                format!("the signer's key {seal_key:?} (security.companies.signer.seal_key)")
            }
        }
    }

    /// The signer that holds `seal_key` and `derive_key`.
    pub fn external(signer: Signer, seal_key: &str, derive_key: &str) -> Keyring {
        Keyring::External {
            signer,
            seal_key: seal_key.to_string(),
            derive_key: derive_key.to_string(),
            derived: Mutex::default(),
        }
    }

    /// Whether the signer holds the keys: a derivation and a wrap and unwrap
    /// of a probe. `Ok` under a local key.
    pub fn probe(&self) -> Result<(), KeyError> {
        let Keyring::External {
            signer, seal_key, ..
        } = self
        else {
            return Ok(());
        };
        self.derive(b"agentd company-user record id")?;
        let ad = wrap_ad(b"company-signer-probe");
        let wrapped = signer
            .wrap(seal_key, &[0u8; 32], &ad)
            .map_err(|e| match e {
                SignerError::Unavailable(_) => signer_error(e),
                e => KeyError::Refused(format!("security.companies.signer: {e}")),
            })?;
        match signer.unwrap(seal_key, &wrapped, &ad) {
            Ok(p) if p == [0u8; 32] => Ok(()),
            Ok(_) => Err(KeyError::Refused(
                "security.companies.signer: an unwrapped probe is not what was wrapped".into(),
            )),
            Err(e @ SignerError::Unavailable(_)) => Err(signer_error(e)),
            Err(e) => Err(KeyError::Refused(format!("security.companies.signer: {e}"))),
        }
    }

    /// The record-id key for `label`: HMAC-SHA256 of it under the seal key's
    /// bytes, or under the signer's derivation key — asked once a process.
    pub fn derive(&self, label: &[u8]) -> Result<[u8; 32], KeyError> {
        match self {
            Keyring::Local { raw, .. } => Ok(crate::sha::hmac_sha256(&raw[..], label)),
            Keyring::External {
                signer,
                derive_key,
                derived,
                ..
            } => {
                if let Some((_, k)) = derived
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|(l, _)| l == label)
                {
                    return Ok(*k);
                }
                let k = signer.derive(derive_key, label).map_err(|e| match e {
                    SignerError::Unavailable(_) => signer_error(e),
                    e => KeyError::Refused(format!("security.companies.signer: {e}")),
                })?;
                derived
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((label.to_vec(), k));
                Ok(k)
            }
        }
    }

    /// `plaintext` sealed under `ad`: the record's fields — `sealed`, and
    /// under an external signer `dek`, the data key it was sealed with as
    /// the signer wrapped it.
    pub fn seal(&self, ad: &[u8], plaintext: &[u8]) -> Result<Map<String, Value>, KeyError> {
        let mut out = Map::new();
        match self {
            Keyring::Local { key, .. } => {
                let sealed = key.seal(ad, plaintext).map_err(KeyError::Refused)?;
                out.insert("sealed".into(), json!(b64::url_nopad(&sealed)));
            }
            Keyring::External {
                signer, seal_key, ..
            } => {
                let mut dek = [0u8; 32];
                crate::sec::random::fill(&mut dek).map_err(|e| {
                    KeyError::Refused(format!("no OS randomness for a data key: {e}"))
                })?;
                let wrapped = signer.wrap(seal_key, &dek, &wrap_ad(ad));
                let sealed = SealKey::new(&dek).and_then(|k| k.seal(ad, plaintext));
                crate::sec::harden::wipe(&mut dek);
                let wrapped = wrapped.map_err(signer_error)?;
                let sealed = sealed.map_err(KeyError::Refused)?;
                out.insert("sealed".into(), json!(b64::url_nopad(&sealed)));
                out.insert("dek".into(), json!(b64::url_nopad(&wrapped)));
            }
        }
        Ok(out)
    }

    /// What the record `state` holds, sealed under `ad`.
    pub fn open(&self, ad: &[u8], state: &Value) -> Result<Vec<u8>, KeyError> {
        let sealed = b64::url_decode(state["sealed"].as_str().unwrap_or_default())
            .map_err(|_| KeyError::Refused("a company record is not sealed".into()))?;
        let dek = state["dek"].as_str();
        let refused = || {
            KeyError::Refused(
                "a company record does not open with security.companies' seal key".into(),
            )
        };
        match (self, dek) {
            (Keyring::Local { key, .. }, None) => key.open(ad, &sealed).map_err(|_| refused()),
            (Keyring::Local { .. }, Some(_)) => Err(KeyError::Refused(
                "a company record sealed under an external signer, and security.companies.key_custody is local".into(),
            )),
            (Keyring::External { .. }, None) => Err(KeyError::Refused(
                "a company record sealed under a local seal key, and security.companies.key_custody is external".into(),
            )),
            (
                Keyring::External {
                    signer, seal_key, ..
                },
                Some(w),
            ) => {
                let wrapped = b64::url_decode(w).map_err(|_| refused())?;
                let mut dek = signer
                    .unwrap(seal_key, &wrapped, &wrap_ad(ad))
                    .map_err(signer_error)?;
                let opened = SealKey::new(&dek).and_then(|k| k.open(ad, &sealed));
                crate::sec::harden::wipe(&mut dek);
                opened.map_err(|_| refused())
            }
        }
    }

    /// The hex record id of `text` under the record-id key for `label`.
    pub fn record_id(&self, label: &[u8], text: &str) -> Result<String, KeyError> {
        let mut k = self.derive(label)?;
        let id = crate::sha::hmac_sha256(&k, text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        crate::sec::harden::wipe(&mut k);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer_at(dir: &std::path::Path) -> (crate::sec::signer::mock::Mock, Signer) {
        let sock = dir.join("signer.sock");
        let m = crate::sec::signer::mock::serve(&sock, &crate::sec::signer::mock::default_knobs())
            .unwrap();
        // SAFETY: getuid has no failure mode.
        (m, Signer::new(&sock, unsafe { libc::getuid() }, None))
    }

    /// Under an external signer a record holds its data key wrapped, opens
    /// through the signer alone, and not under a local key; a signer that is
    /// down is an outage, never a key that opens nothing; the record ids
    /// keep their shape.
    #[test]
    fn a_record_sealed_through_the_signer_opens_only_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let (_m, signer) = signer_at(dir.path());
        let ext = Keyring::external(signer, "seal", "derive");
        ext.probe().unwrap();
        let ad = associated_data(&[b"company-user", b"x"]);
        let state = Value::Object(ext.seal(&ad, b"usr_secret").unwrap());
        assert!(state["dek"].is_string(), "{state}");
        assert!(!state.to_string().contains("usr_secret"));
        assert_eq!(ext.open(&ad, &state).unwrap(), b"usr_secret");
        let other = associated_data(&[b"company-user", b"y"]);
        assert!(matches!(
            ext.open(&other, &state),
            Err(KeyError::Refused(_))
        ));
        let local = Keyring::local([7; 32]).unwrap();
        assert!(matches!(local.open(&ad, &state), Err(KeyError::Refused(_))));
        let mine = Value::Object(local.seal(&ad, b"x").unwrap());
        assert!(matches!(ext.open(&ad, &mine), Err(KeyError::Refused(_))));
        // Ids: 64 hex digits, one per text, as under a local key.
        let id = ext
            .record_id(b"agentd company-user record id", "iss|p")
            .unwrap();
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            id,
            ext.record_id(b"agentd company-user record id", "iss|p")
                .unwrap()
        );
        assert_ne!(
            id,
            ext.record_id(b"agentd company-user record id", "iss|q")
                .unwrap()
        );
        assert_eq!(local.record_id(b"l", "t").unwrap().len(), 64);
        // Down: an outage, typed apart from a refusal.
        let gone = Keyring::external(
            Signer::new(
                dir.path().join("gone.sock"),
                0,
                Some(std::time::Duration::from_millis(300)),
            ),
            "seal",
            "derive",
        );
        assert!(matches!(
            gone.open(&ad, &state),
            Err(KeyError::Unavailable(_))
        ));
        assert!(matches!(
            gone.seal(&ad, b"x"),
            Err(KeyError::Unavailable(_))
        ));
        assert!(matches!(gone.derive(b"l"), Err(KeyError::Unavailable(_))));
        assert!(matches!(gone.probe(), Err(KeyError::Unavailable(_))));
        // A signer that holds no such key refuses, and that is no outage.
        let wrong = Keyring::external(ext_signer(dir.path()), "nope", "derive");
        assert!(matches!(wrong.probe(), Err(KeyError::Refused(_))));
    }

    fn ext_signer(dir: &std::path::Path) -> Signer {
        // SAFETY: getuid has no failure mode.
        Signer::new(dir.join("signer.sock"), unsafe { libc::getuid() }, None)
    }
}
