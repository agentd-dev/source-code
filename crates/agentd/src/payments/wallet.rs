// SPDX-License-Identifier: AGPL-3.0-only
//! The wallets agentd pays from, and who holds their keys (RFC 0049 D5,
//! D8). Only the supervisor signs.
//!
//! **Local custody** (`key_custody: local`, the default). A wallet's key is
//! a file, `key_file`, holding `{wallet, address, sealed}`: the 32-byte
//! secp256k1 key sealed under AES-256-GCM (`auth::seal`) with the associated
//! data `("payment-wallet-key", <wallet>, <address>)`, under
//! `security.payments.seal_key` — 32 bytes behind a `{{secret-file:PATH}}`
//! reference, read by the supervisor at start. The file is made `0600` by
//! [`create`] (`agentd --print-wallet-address`) and read by [`read`], which
//! refuses a file another user could read, one another uid owns, a
//! symlink, and one that does not open to the address it names. It is read
//! for each signature and the key wiped once it has signed (`SigningKey`
//! zeroizes on drop), so the plaintext key is in memory for one signature,
//! not for the daemon's life.
//!
//! **External custody** (`key_custody: external`). agentd holds no wallet
//! key: the external signer does (`docs/external-signer.md`). A wallet names
//! the signer's key id; `public_key` gives its secp256k1 JWK, from which
//! agentd derives the address, and `sign_typed_data` is sent the EIP-712
//! typed data — not a digest — so the signer reads what it signs.
//!
//! **A Solana wallet** (`chain: solana`) is the same, with an Ed25519 key:
//! its key file seals the 32-byte seed, bound to the base58 address, and is
//! made by [`create_svm`] and read by [`read_svm`] under the same checks;
//! `ring`'s key pair is made from it for one signature and dropped (`ring`
//! does not promise to wipe it). Its signer key answers `public_key` with an
//! Ed25519 JWK (`EdDSA`, RFC 8037), and `sign_solana_transaction` is sent
//! the transaction's message — the bytes signed, which the signer reads
//! whole — and answers the 64-byte signature.
//!
//! Either way agentd checks each signature against the wallet's address
//! before it is sent — recovering the EVM address from it, verifying the
//! Ed25519 one — so a key file swapped under the same name, or a signer
//! that signed with another key, is a refusal, never a payment from someone
//! else. Nothing here logs; no error names a key, a signature or a sealed
//! byte.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::evm::{self, Address};
use super::scheme::Account;
use super::svm::{self, Pubkey};
use crate::auth::seal::{SealKey, associated_data};
use crate::config::settings::payments as cfg;
use crate::sec::signer::{Signer, SignerError};

/// Why a wallet did not sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignError {
    /// The external signer did not answer: nothing was signed.
    Unavailable(String),
    /// The key file did not open, or the signer refused, or the signature is
    /// not the wallet's.
    Refused(String),
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignError::Unavailable(e) | SignError::Refused(e) => f.write_str(e),
        }
    }
}

/// Where a wallet's key is.
enum Source {
    File(PathBuf),
    Signer(String),
}

/// One wallet: where its key is, the chain family it pays on, the address
/// the configuration names, and the address its key is once known.
struct Wallet {
    source: Source,
    chain: cfg::Chain,
    want: Option<Account>,
    address: std::sync::Mutex<Option<Account>>,
}

/// Who holds the keys.
enum Custody {
    Local(Box<SealKey>),
    External(Signer),
}

/// Every configured wallet.
pub struct Wallets {
    custody: Custody,
    wallets: BTreeMap<String, Wallet>,
}

/// The seal key's 32 bytes from its secret's text: hex or base64.
pub fn seal_from_text(text: &str) -> Result<[u8; 32], String> {
    let bad = || "security.payments.seal_key: not 32 bytes in hex or base64".to_string();
    let t = text.trim();
    let bytes = if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        evm::unhex0x(&format!("0x{t}")).map_err(|_| bad())?
    } else {
        crate::b64::url_decode(&t.replace('+', "-").replace('/', "_").replace('=', ""))
            .map_err(|_| bad())?
    };
    bytes.try_into().map_err(|_| bad())
}

/// The seal key `security.payments.seal_key` names: a mounted file, never
/// the environment.
pub fn seal_key(p: &cfg::Payments) -> Result<SealKey, String> {
    let Some(crate::config::settings::Secret(r)) = &p.seal_key else {
        return Err("security.payments.seal_key is not set".into());
    };
    let text = crate::sec::secret::resolve(r, &|_| None)
        .map_err(|e| format!("security.payments.seal_key: {e}"))?;
    let mut raw = seal_from_text(&text)?;
    let key = SealKey::new(&raw);
    crate::sec::harden::wipe(&mut raw);
    key
}

/// What a wallet's key is sealed under: bound to the wallet and its address
/// — an EVM address in lowercase hex, a Solana one in base58.
fn ad(wallet: &str, address: &str) -> Vec<u8> {
    associated_data(&[b"payment-wallet-key", wallet.as_bytes(), address.as_bytes()])
}

/// Write `path`, a new key file of `wallet` naming `address` (as written in
/// the file, and as bound, `bound`), its 32 bytes sealed under `seal`:
/// `0600`, `O_EXCL`, synced.
fn write_sealed(
    path: &Path,
    seal: &SealKey,
    wallet: &str,
    address: &str,
    bound: &str,
    raw: &[u8; 32],
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let sealed = seal.seal(&ad(wallet, bound), raw)?;
    let body = json!({"wallet": wallet, "address": address,
                      "sealed": crate::b64::url_nopad(&sealed)});
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(format!("{body}\n").as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Make `path`, a new key file for `wallet` sealed under `seal`: a fresh
/// key from the kernel's CSPRNG, written `0600` with `O_EXCL`. Its address.
pub fn create(path: &Path, seal: &SealKey, wallet: &str) -> Result<Address, String> {
    let mut raw = [0u8; 32];
    let key = loop {
        crate::sec::random::fill(&mut raw).map_err(|e| format!("no OS randomness: {e}"))?;
        if let Ok(k) = evm::key_from_bytes(&raw) {
            break k;
        }
    };
    let address = evm::address_of_key(&key);
    let written = write_sealed(
        path,
        seal,
        wallet,
        &address.checksummed(),
        &address.lower(),
        &raw,
    );
    crate::sec::harden::wipe(&mut raw);
    written.map(|()| address)
}

/// Make `path`, a new key file for the Solana wallet `wallet` sealed under
/// `seal`: a fresh Ed25519 seed from the kernel's CSPRNG, written `0600`
/// with `O_EXCL`. Its address.
pub fn create_svm(path: &Path, seal: &SealKey, wallet: &str) -> Result<Pubkey, String> {
    let mut seed = [0u8; 32];
    crate::sec::random::fill(&mut seed).map_err(|e| format!("no OS randomness: {e}"))?;
    let address = svm::key_pair(&seed).map(|kp| svm::pubkey_of(&kp));
    let written = address.and_then(|a| {
        let text = a.to_string();
        write_sealed(path, seal, wallet, &text, &text, &seed).map(|()| a)
    });
    crate::sec::harden::wipe(&mut seed);
    written
}

/// The address `path` names, as written, and its sealed 32 bytes: a
/// regular file of agentd's uid that no one else can read, sealed for
/// `wallet` under `seal`, bound to the address `bound` reads it as. The
/// caller wipes the bytes.
fn open_sealed(
    path: &Path,
    seal: &SealKey,
    wallet: &str,
    bound: &dyn Fn(&str) -> Option<String>,
) -> Result<(String, [u8; 32]), String> {
    use std::os::unix::fs::MetadataExt;
    let at = || {
        format!(
            "security.payments.wallets.{wallet}.key_file {}",
            path.display()
        )
    };
    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "{}: no such file — make it with `agentd --print-wallet-address {wallet}`, which prints the address to fund",
                at()
            )
        } else {
            format!("{}: {e}", at())
        }
    })?;
    if !meta.file_type().is_file() {
        return Err(format!("{}: not a regular file", at()));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{}: mode {:o} — another user can read or write it; a wallet key file is 0600",
            at(),
            meta.mode() & 0o777
        ));
    }
    // SAFETY: geteuid has no failure mode.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        return Err(format!(
            "{}: owned by uid {}, not agentd's ({me})",
            at(),
            meta.uid()
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", at()))?;
    let v: Value =
        serde_json::from_str(&text).map_err(|_| format!("{}: not a wallet key file", at()))?;
    if v["wallet"] != wallet {
        return Err(format!(
            "{}: the key file is wallet {}'s, not {wallet}'s",
            at(),
            v["wallet"]
        ));
    }
    let address = v["address"].as_str().unwrap_or_default().to_string();
    let binding = bound(&address).ok_or_else(|| format!("{}: no address", at()))?;
    let sealed = v["sealed"]
        .as_str()
        .and_then(|s| crate::b64::url_decode(s).ok())
        .ok_or_else(|| format!("{}: no sealed key", at()))?;
    let mut raw = seal.open(&ad(wallet, &binding), &sealed).map_err(|_| {
        format!(
            "{}: does not open under security.payments.seal_key — another seal key, or a file of another wallet or address",
            at()
        )
    })?;
    let bytes = <[u8; 32]>::try_from(raw.as_slice())
        .map_err(|_| format!("{}: the sealed key is not 32 bytes", at()));
    crate::sec::harden::wipe(&mut raw);
    Ok((address, bytes?))
}

/// The address `path` names, and its key: a regular file of agentd's uid
/// that no one else can read, sealed for `wallet` under `seal`, whose key's
/// address is the one it names.
pub fn read(
    path: &Path,
    seal: &SealKey,
    wallet: &str,
) -> Result<(Address, k256::ecdsa::SigningKey), String> {
    let (text, mut raw) = open_sealed(path, seal, wallet, &|a| {
        Address::parse(a).ok().map(|a| a.lower())
    })?;
    let key = evm::key_from_bytes(&raw);
    crate::sec::harden::wipe(&mut raw);
    let key = key?;
    let address = Address::parse(&text)?;
    if evm::address_of_key(&key) != address {
        return Err(format!(
            "security.payments.wallets.{wallet}.key_file {}: the key is not the key of the address the file names",
            path.display()
        ));
    }
    Ok((address, key))
}

/// The Solana address `path` names, and its key pair: [`read`]'s checks,
/// for an Ed25519 seed.
pub fn read_svm(
    path: &Path,
    seal: &SealKey,
    wallet: &str,
) -> Result<(Pubkey, ring::signature::Ed25519KeyPair), String> {
    let (text, mut seed) = open_sealed(path, seal, wallet, &|a| {
        Pubkey::parse(a).ok().map(|a| a.to_string())
    })?;
    let kp = svm::key_pair(&seed);
    crate::sec::harden::wipe(&mut seed);
    let kp = kp?;
    let address = Pubkey::parse(&text)?;
    if svm::pubkey_of(&kp) != address {
        return Err(format!(
            "security.payments.wallets.{wallet}.key_file {}: the key is not the key of the address the file names",
            path.display()
        ));
    }
    Ok((address, kp))
}

fn signer_error(e: SignerError) -> SignError {
    match e {
        SignerError::Unavailable(m) => {
            SignError::Unavailable(format!("security.payments.signer: {m}"))
        }
        e => SignError::Refused(format!("security.payments.signer: {e}")),
    }
}

impl Wallets {
    /// Every wallet, checked: each local key file read, opened and held to
    /// its address, each signer key's address asked. A key file that does not
    /// open, a key whose address is not the one configured, or a signer that
    /// refuses a key is an error; a signer that does not answer leaves its
    /// wallets to be asked at their first use, and is named in `Ok`'s list.
    pub fn open(p: &cfg::Payments) -> Result<(Wallets, Vec<String>), SignError> {
        let custody = match p.external() {
            Some(sg) => Custody::External(sg.signer()),
            None => Custody::Local(Box::new(seal_key(p).map_err(SignError::Refused)?)),
        };
        let mut wallets = BTreeMap::new();
        for (name, w) in &p.wallets {
            let source = match (&w.key_file, &w.key) {
                (Some(f), _) => Source::File(PathBuf::from(f)),
                (None, Some(k)) => Source::Signer(k.clone()),
                (None, None) => {
                    return Err(SignError::Refused(format!(
                        "security.payments.wallets.{name}: no key"
                    )));
                }
            };
            let want = w.address.as_deref().and_then(|a| Account::parse(a).ok());
            wallets.insert(
                name.clone(),
                Wallet {
                    source,
                    chain: w.chain,
                    want,
                    address: std::sync::Mutex::new(None),
                },
            );
        }
        let all = Wallets { custody, wallets };
        let mut unavailable = Vec::new();
        for name in all.wallets.keys() {
            match all.address(name) {
                Ok(_) => {}
                Err(SignError::Unavailable(e)) => unavailable.push(e),
                Err(e) => return Err(e),
            }
        }
        Ok((all, unavailable))
    }

    /// The address `wallet` pays from: known, or read from its key file or
    /// asked of the signer now — off the loop, since a signer may take its
    /// timeout to answer.
    pub fn address(&self, wallet: &str) -> Result<Account, SignError> {
        let w = self
            .wallets
            .get(wallet)
            .ok_or_else(|| SignError::Refused(format!("no wallet {wallet:?}")))?;
        if let Some(a) = *w.address.lock().unwrap_or_else(|e| e.into_inner()) {
            return Ok(a);
        }
        let address = match (&self.custody, &w.source, w.chain) {
            (Custody::Local(seal), Source::File(path), cfg::Chain::Eip155) => {
                Account::Evm(read(path, seal, wallet).map_err(SignError::Refused)?.0)
            }
            (Custody::Local(seal), Source::File(path), cfg::Chain::Solana) => {
                Account::Svm(read_svm(path, seal, wallet).map_err(SignError::Refused)?.0)
            }
            (Custody::External(signer), Source::Signer(k), chain) => {
                let pk = signer.public_key(k).map_err(signer_error)?;
                let (alg, what) = match chain {
                    cfg::Chain::Eip155 => ("ES256K", "secp256k1"),
                    cfg::Chain::Solana => ("EdDSA", "Ed25519"),
                };
                if pk.alg != alg {
                    return Err(SignError::Refused(format!(
                        "security.payments.wallets.{wallet}: the signer's key {k:?} is {}, not {alg} ({what})",
                        pk.alg
                    )));
                }
                let derived = match chain {
                    cfg::Chain::Eip155 => evm::address_of_jwk(&pk.jwk).map(Account::Evm),
                    cfg::Chain::Solana => svm::pubkey_of_jwk(&pk.jwk).map(Account::Svm),
                };
                derived.map_err(|e| {
                    SignError::Refused(format!(
                        "security.payments.wallets.{wallet}: the signer's key {k:?}: {e}"
                    ))
                })?
            }
            _ => {
                return Err(SignError::Refused(format!(
                    "security.payments.wallets.{wallet}: no key for its custody"
                )));
            }
        };
        if let Some(want) = w.want
            && want != address
        {
            return Err(SignError::Refused(format!(
                "security.payments.wallets.{wallet}: its key is {address}'s, and the configuration says {want}"
            )));
        }
        *w.address.lock().unwrap_or_else(|e| e.into_inner()) = Some(address);
        Ok(address)
    }

    /// The address of `wallet` when it is known, without asking anyone.
    pub fn known(&self, wallet: &str) -> Option<Account> {
        *self
            .wallets
            .get(wallet)?
            .address
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The 65-byte signature of the EIP-712 `typed_data` by `wallet`: the
    /// local key read and wiped, or the signer asked. It must recover to the
    /// wallet's address, or it is refused.
    pub fn sign_typed_data(&self, wallet: &str, typed_data: &Value) -> Result<[u8; 65], SignError> {
        let address = self
            .address(wallet)?
            .evm()
            .ok_or_else(|| SignError::Refused(format!("wallet {wallet} is not an EVM wallet")))?;
        let w = &self.wallets[wallet];
        let digest = super::eip712::digest(typed_data)
            .map_err(|e| SignError::Refused(format!("the typed data: {e}")))?;
        let sig = match (&self.custody, &w.source) {
            (Custody::Local(seal), Source::File(path)) => {
                let (now, key) = read(path, seal, wallet).map_err(SignError::Refused)?;
                if now != address {
                    return Err(SignError::Refused(format!(
                        "security.payments.wallets.{wallet}: the key file now holds another address's key"
                    )));
                }
                evm::sign_digest(&key, &digest)
            }
            (Custody::External(signer), Source::Signer(k)) => signer
                .sign_typed_data(k, typed_data)
                .map_err(signer_error)?,
            _ => return Err(SignError::Refused("no key for this custody".into())),
        };
        match evm::recover(&digest, &sig) {
            Ok(a) if a == address => Ok(sig),
            _ => Err(SignError::Refused(format!(
                "the signature is not wallet {wallet}'s ({})",
                address.checksummed()
            ))),
        }
    }

    /// The 64-byte Ed25519 signature of a Solana transaction's `message` by
    /// `wallet`: the local key read and dropped, or the signer asked. It
    /// must verify under the wallet's address, or it is refused.
    pub fn sign_svm(&self, wallet: &str, message: &[u8]) -> Result<[u8; 64], SignError> {
        let address = self
            .address(wallet)?
            .svm()
            .ok_or_else(|| SignError::Refused(format!("wallet {wallet} is not a Solana wallet")))?;
        let w = &self.wallets[wallet];
        let sig = match (&self.custody, &w.source) {
            (Custody::Local(seal), Source::File(path)) => {
                let (now, kp) = read_svm(path, seal, wallet).map_err(SignError::Refused)?;
                if now != address {
                    return Err(SignError::Refused(format!(
                        "security.payments.wallets.{wallet}: the key file now holds another address's key"
                    )));
                }
                svm::sign(&kp, message)
            }
            (Custody::External(signer), Source::Signer(k)) => signer
                .sign_solana_transaction(k, message)
                .map_err(signer_error)?,
            _ => return Err(SignError::Refused("no key for this custody".into())),
        };
        if svm::verify(&address, message, &sig) {
            Ok(sig)
        } else {
            Err(SignError::Refused(format!(
                "the signature is not wallet {wallet}'s ({address})"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal() -> SealKey {
        SealKey::new(&[9u8; 32]).unwrap()
    }

    /// A key file made 0600 opens to its address, and to nothing else: not
    /// under another seal key, not for another wallet's name, not once
    /// another user can read it, and not as a symlink.
    #[test]
    fn a_sealed_key_file_opens_only_as_itself() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.key");
        let address = create(&path, &seal(), "ops").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&address.checksummed()));
        let (a, key) = read(&path, &seal(), "ops").unwrap();
        assert_eq!(a, address);
        assert_eq!(evm::address_of_key(&key), address);
        // The raw key is nowhere in the file.
        let raw = key.to_bytes();
        assert!(!text.contains(&crate::sha::to_hex(&raw)));
        assert!(!text.contains(&crate::b64::url_nopad(&raw)));
        assert!(create(&path, &seal(), "ops").is_err(), "never overwritten");
        assert!(read(&path, &SealKey::new(&[8u8; 32]).unwrap(), "ops").is_err());
        assert!(read(&path, &seal(), "other").is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(
            read(&path, &seal(), "ops")
                .unwrap_err()
                .contains("another user")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link.key");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(
            read(&link, &seal(), "ops")
                .unwrap_err()
                .contains("regular file")
        );
        // A file naming another address than its key's.
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["address"] = json!("0x209693Bc6afc0C5328bA36FaF03C514EF312287C");
        let forged = dir.path().join("forged.key");
        std::fs::write(&forged, v.to_string()).unwrap();
        std::fs::set_permissions(&forged, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read(&forged, &seal(), "ops").is_err());
    }

    /// A Solana wallet's key file is made 0600 and opens to its base58
    /// address, and to nothing else: not as an EVM key file, not for another
    /// wallet, not under another seal key; its key signs as that address.
    #[test]
    fn a_solana_key_file_opens_only_as_itself() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sol.key");
        let address = create_svm(&path, &seal(), "sol").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&address.to_string()));
        let (a, kp) = read_svm(&path, &seal(), "sol").unwrap();
        assert_eq!(a, address);
        let sig = svm::sign(&kp, b"message");
        assert!(svm::verify(&address, b"message", &sig));
        assert!(
            create_svm(&path, &seal(), "sol").is_err(),
            "never overwritten"
        );
        assert!(read(&path, &seal(), "sol").is_err(), "no EVM key");
        assert!(read_svm(&path, &seal(), "other").is_err());
        assert!(read_svm(&path, &SealKey::new(&[8u8; 32]).unwrap(), "sol").is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o604)).unwrap();
        assert!(
            read_svm(&path, &seal(), "sol")
                .unwrap_err()
                .contains("another user")
        );
        // An EVM key file is no Solana key.
        let evm_path = dir.path().join("ops.key");
        create(&evm_path, &seal(), "ops").unwrap();
        assert!(read_svm(&evm_path, &seal(), "ops").is_err());
        // A file naming another address than its key's.
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["address"] = json!("2wKupLR9q6wXYppw8Gr2NvWxKBUqm4PPJKkQfoxHDBg4");
        let forged = dir.path().join("forged.key");
        std::fs::write(&forged, v.to_string()).unwrap();
        std::fs::set_permissions(&forged, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_svm(&forged, &seal(), "sol").is_err());
    }

    /// The seal key is 32 bytes, as hex or base64.
    #[test]
    fn the_seal_key_reads_as_hex_or_base64() {
        let hex = "07".repeat(32);
        assert_eq!(seal_from_text(&hex).unwrap(), [7u8; 32]);
        assert_eq!(
            seal_from_text(&crate::b64::std_pad(&[7u8; 32])).unwrap(),
            [7u8; 32]
        );
        assert!(seal_from_text("07").is_err());
        assert!(seal_from_text(&"07".repeat(31)).is_err());
    }
}
