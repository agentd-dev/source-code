// SPDX-License-Identifier: AGPL-3.0-only
//! The Solana half of the mock x402 resource server (RFC 0049 §9): what a
//! sponsor does with an `exact` payment on Solana [EXACT-SVM §2, §3.1],
//! with a fee-payer key of its own and no chain.
//!
//! [`Sponsor::entry`] writes the `accepts` entry it offers — its key as
//! `extra.feePayer`. [`Sponsor::verify`] holds a `PaymentPayload` to it:
//! the entry echoed exactly, a partially signed transaction whose first
//! key is the fee payer and whose first signature is still empty, the
//! wallet's signature verifying over the message, and the message x402's
//! exact transfer ([`svm::Transfer::read`]) of the mint asked, to `payTo`'s
//! associated token account, of the amount asked, with the memo asked, at
//! a compute-unit price within the reference sponsor's bound.
//! [`Sponsor::settle`] then signs as the fee payer — the transaction's id is
//! that signature — and answers the `SettlementResponse`; the same
//! transaction twice is refused (`duplicate_settlement`), as the
//! specification recommends.

use std::collections::HashSet;
use std::sync::Mutex;

use ring::signature::Ed25519KeyPair;
use serde_json::{Value, json};

use super::base58;
use super::svm::{self, Pubkey};

/// A verified payment, ready to settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The wallet that paid: the transfer's authority.
    pub payer: Pubkey,
    pub transfer: svm::Transfer,
    /// The message's bytes.
    pub message: Vec<u8>,
    /// The wallet's signature.
    pub signature: [u8; 64],
}

/// A sponsor: the fee payer an entry names, and what it settled.
pub struct Sponsor {
    key: Ed25519KeyPair,
    settled: Mutex<HashSet<Vec<u8>>>,
}

impl Sponsor {
    /// The sponsor of the Ed25519 seed `seed`.
    pub fn new(seed: &[u8; 32]) -> Result<Sponsor, String> {
        Ok(Sponsor {
            key: svm::key_pair(seed)?,
            settled: Mutex::new(HashSet::new()),
        })
    }

    pub fn fee_payer(&self) -> Pubkey {
        svm::pubkey_of(&self.key)
    }

    /// The `accepts` entry it offers: `amount` atomic units of `mint` on
    /// `network` to `pay_to`, with its key as the fee payer, and the memo
    /// and blockhash given.
    pub fn entry(
        &self,
        network: &str,
        mint: &Pubkey,
        pay_to: &Pubkey,
        amount: u64,
        memo: Option<&str>,
        blockhash: Option<&[u8; 32]>,
    ) -> Value {
        let mut extra = json!({"feePayer": self.fee_payer().to_string()});
        if let Some(m) = memo {
            extra["memo"] = json!(m);
        }
        if let Some(b) = blockhash {
            extra["recentBlockhash"] = json!(base58::encode(b));
        }
        json!({"scheme": "exact", "network": network, "amount": amount.to_string(),
               "asset": mint.to_string(), "payTo": pay_to.to_string(),
               "maxTimeoutSeconds": 60, "extra": extra})
    }

    /// `payload`, a `PaymentPayload`, held to `accepted`, the entry this
    /// sponsor offered, and to `decimals`, the mint's: what was paid, or
    /// why it is refused, in x402's reason words.
    pub fn verify(
        &self,
        accepted: &Value,
        payload: &Value,
        decimals: u8,
    ) -> Result<Verified, String> {
        if payload["x402Version"] != super::x402::VERSION {
            return Err("invalid_x402_version".into());
        }
        if payload["accepted"] != *accepted {
            return Err("invalid_payload: accepted is not the entry offered".into());
        }
        let extra = &accepted["extra"];
        if extra["feePayer"] != self.fee_payer().to_string() {
            return Err("fee_payer_not_managed".into());
        }
        let tx = payload["payload"]["transaction"]
            .as_str()
            .and_then(|t| crate::b64::std_decode(t).ok())
            .ok_or("invalid_payload: no base64 transaction")?;
        let tx = svm::Transaction::read(&tx)
            .map_err(|e| format!("transaction_could_not_be_decoded: {e}"))?;
        if tx.message.keys[0] != self.fee_payer() {
            return Err("fee_payer_mismatch".into());
        }
        if tx.signatures[0] != [0u8; 64] {
            return Err(
                "invalid_payload: the fee payer's signature is not left to the sponsor".into(),
            );
        }
        let t = svm::Transfer::read(&tx.message)
            .map_err(|e| format!("invalid_exact_svm_transaction: {e}"))?;
        // Two signers, so two signatures.
        if !svm::verify(&t.owner, &tx.signed, &tx.signatures[1]) {
            return Err("signature_invalid".into());
        }
        let mint = accepted["asset"]
            .as_str()
            .and_then(|a| Pubkey::parse(a).ok());
        if mint != Some(t.mint) {
            return Err("mint_mismatch".into());
        }
        let pay_to = accepted["payTo"]
            .as_str()
            .and_then(|a| Pubkey::parse(a).ok())
            .ok_or("recipient_mismatch")?;
        if !t.pays(&pay_to) {
            return Err("recipient_mismatch".into());
        }
        if accepted["amount"].as_str() != Some(t.amount.to_string().as_str())
            || t.decimals != decimals
        {
            return Err("amount_mismatch".into());
        }
        if t.unit_price > svm::MAX_COMPUTE_UNIT_PRICE {
            return Err("compute_price_too_high".into());
        }
        match extra["memo"].as_str() {
            Some(m) if t.memo != m.as_bytes() => return Err("memo_mismatch".into()),
            None if t.memo.len() < 2 * svm::MEMO_NONCE => {
                return Err("memo_missing: no random nonce of 16 bytes in hex".into());
            }
            _ => {}
        }
        Ok(Verified {
            payer: t.owner,
            transfer: t,
            signature: tx.signatures[1],
            message: tx.signed,
        })
    }

    /// Settle `v` on `network`: sign as the fee payer, and answer the
    /// `SettlementResponse` — the transaction's id, its fee payer's
    /// signature, in base58, and the wallet that paid. The same transaction
    /// settled twice is refused.
    pub fn settle(&self, v: &Verified, network: &str) -> Value {
        let fresh = self
            .settled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(v.message.clone());
        if !fresh {
            return json!({"success": false, "errorReason": "duplicate_settlement",
                          "transaction": "", "network": network,
                          "payer": v.payer.to_string()});
        }
        let id = svm::sign(&self.key, &v.message);
        json!({"success": true, "transaction": base58::encode(&id), "network": network,
               "payer": v.payer.to_string()})
    }

    /// The whole transaction `v` and its settlement make: both signatures.
    pub fn signed(&self, v: &Verified) -> Vec<u8> {
        svm::transaction(&[svm::sign(&self.key, &v.message), v.signature], &v.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payments::policy;
    use crate::payments::svm::tests::{MAINNET, PAY_TO, USDC};

    /// A local Solana wallet, its seal key in a file, and a policy of USDC
    /// on mainnet paying `PAY_TO`: `(dir, settings, wallets, policy)`.
    fn setup() -> (
        tempfile::TempDir,
        crate::config::settings::payments::Payments,
        crate::payments::wallet::Wallets,
        policy::Policy,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let seal = dir.path().join("seal");
        std::fs::write(&seal, "42".repeat(32)).unwrap();
        std::fs::set_permissions(&seal, std::fs::Permissions::from_mode(0o600)).unwrap();
        let p: crate::config::settings::payments::Payments = serde_json::from_value(json!({
            "seal_key": format!("{{{{secret-file:{}}}}}", seal.display()),
            "wallets": {"sol": {"chain": "solana", "key_file": dir.path().join("sol.key").display().to_string()}},
            "assets": {"usdc-sol": {
                "asset": format!("{MAINNET}/token:{USDC}"), "decimals": 6,
                "limits": {"per_payment": "0.10", "per_run": "0.25", "per_day": "1.00", "per_payee": "0.50"}}},
            "payees": {"weather": {"address": PAY_TO}},
            "rpc": {MAINNET: "https://rpc.example/solana"}
        }))
        .unwrap();
        let seal_key = crate::payments::wallet::seal_key(&p).unwrap();
        crate::payments::wallet::create_svm(&dir.path().join("sol.key"), &seal_key, "sol").unwrap();
        let (wallets, unavailable) = crate::payments::wallet::Wallets::open(&p).unwrap();
        assert!(unavailable.is_empty());
        let pol = policy::Policy::from_settings(&p).unwrap();
        (dir, p, wallets, pol)
    }

    fn step() -> policy::StepPayment {
        policy::StepPayment {
            wallet: "sol".into(),
            max: vec![("usdc-sol".into(), 100_000)],
            approve: false,
        }
    }

    fn none(_: &str, _: &crate::payments::scheme::Account) -> policy::Spent {
        policy::Spent::default()
    }

    /// The `PaymentPayload` of a transfer `wallets`' `sol` signs.
    fn paid(
        wallets: &crate::payments::wallet::Wallets,
        t: &svm::Terms,
        amount: u128,
        memo: &str,
        required: &Value,
        accepted: &Value,
    ) -> Result<Value, crate::payments::wallet::SignError> {
        svm::signed_transfer(wallets, "sol", t, amount, memo)
            .map(|p| crate::payments::x402::payload(required, accepted, p))
    }

    /// End to end with no chain: the sponsor offers an entry, the policy
    /// chooses it, the wallet signs the transfer, the sponsor verifies the
    /// payload and settles it — once — and the fully signed transaction
    /// carries both signatures, each verifying.
    #[test]
    fn a_payment_the_sponsor_offered_verifies_and_settles_once() {
        let (_dir, _p, wallets, pol) = setup();
        let sponsor = Sponsor::new(&[5u8; 32]).unwrap();
        let mint = Pubkey::parse(USDC).unwrap();
        let pay_to = Pubkey::parse(PAY_TO).unwrap();
        let entry = sponsor.entry(MAINNET, &mint, &pay_to, 40_000, None, Some(&[3u8; 32]));
        let required = json!({"x402Version": 2, "resource": {"url": "https://api.example/data"},
                              "accepts": [entry.clone()]});
        let choice = policy::choose(&pol, &step(), std::slice::from_ref(&entry), &none).unwrap();
        let crate::payments::scheme::Terms::Svm(terms) = &choice.terms else {
            panic!("a Solana entry's terms");
        };
        assert_eq!(terms.fee_payer, sponsor.fee_payer());
        let memo = terms.memo_for(&[0xcd; 32]);
        let payload = paid(
            &wallets,
            terms,
            choice.amount,
            &memo,
            &required,
            &choice.entry,
        )
        .unwrap();
        let v = sponsor.verify(&entry, &payload, 6).unwrap();
        assert_eq!(
            Some(crate::payments::scheme::Account::Svm(v.payer)),
            wallets.known("sol")
        );
        assert_eq!(
            (v.transfer.amount, v.transfer.memo.as_slice()),
            (40_000, memo.as_bytes())
        );
        let settled = sponsor.settle(&v, MAINNET);
        assert_eq!(settled["success"], true);
        let s = crate::payments::x402::Settlement::from_header(
            &crate::payments::x402::encode_header(&settled),
        )
        .unwrap();
        assert!(s.success && !s.pending());
        assert_eq!(s.payer.as_deref(), Some(v.payer.to_string().as_str()));
        let full = sponsor.signed(&v);
        let full = svm::Transaction::read(&full).unwrap();
        let (sigs, bytes) = (&full.signatures, &full.signed);
        assert_eq!(
            base58::encode(&sigs[0]),
            s.transaction,
            "the id is the fee payer's signature"
        );
        assert!(svm::verify(&sponsor.fee_payer(), bytes, &sigs[0]));
        assert!(svm::verify(&v.payer, bytes, &sigs[1]));
        assert_eq!(
            sponsor.settle(&v, MAINNET)["errorReason"],
            "duplicate_settlement"
        );
    }

    /// The sponsor refuses each payment that is not the one it offered: a
    /// signature that does not verify, another amount, payee, mint or
    /// memo, a fee payer's slot already filled, another fee payer, and an
    /// entry not echoed exactly.
    #[test]
    fn the_sponsor_refuses_what_it_did_not_offer() {
        let (_dir, _p, wallets, pol) = setup();
        let sponsor = Sponsor::new(&[5u8; 32]).unwrap();
        let mint = Pubkey::parse(USDC).unwrap();
        let pay_to = Pubkey::parse(PAY_TO).unwrap();
        let entry = sponsor.entry(
            MAINNET,
            &mint,
            &pay_to,
            40_000,
            Some("inv-7"),
            Some(&[3u8; 32]),
        );
        let required = json!({"x402Version": 2, "resource": {"url": "https://api.example/data"},
                              "accepts": [entry.clone()]});
        let choice = policy::choose(&pol, &step(), std::slice::from_ref(&entry), &none).unwrap();
        let crate::payments::scheme::Terms::Svm(terms) = &choice.terms else {
            panic!("a Solana entry's terms");
        };
        assert_eq!(
            terms.memo_for(&[0; 32]),
            "inv-7",
            "the server's memo, not a nonce"
        );
        let sign = |t: &svm::Terms, amount: u128, memo: &str| {
            paid(&wallets, t, amount, memo, &required, &entry).unwrap()
        };
        let good = sign(terms, 40_000, "inv-7");
        assert!(sponsor.verify(&entry, &good, 6).is_ok());
        let tx_of = |p: &Value| {
            crate::b64::std_decode(p["payload"]["transaction"].as_str().unwrap()).unwrap()
        };
        let with_tx = |tx: Vec<u8>| {
            let mut p = good.clone();
            p["payload"]["transaction"] = json!(crate::b64::std_pad(&tx));
            p
        };
        // A flipped bit of the wallet's signature.
        let mut tx = tx_of(&good);
        tx[1 + 64] ^= 1;
        assert_eq!(
            sponsor.verify(&entry, &with_tx(tx), 6).unwrap_err(),
            "signature_invalid"
        );
        // A flipped bit of the message: the amount.
        let mut tx = tx_of(&good);
        let at = tx.len() - 1 - 1 - "inv-7".len() - 3 - 2;
        tx[at] ^= 1;
        assert!(sponsor.verify(&entry, &with_tx(tx), 6).is_err());
        // The fee payer's slot filled.
        let mut tx = tx_of(&good);
        tx[1] = 1;
        assert!(
            sponsor
                .verify(&entry, &with_tx(tx), 6)
                .unwrap_err()
                .contains("fee payer's signature")
        );
        // Another amount, payee, mint, memo, signed as such.
        assert_eq!(
            sponsor
                .verify(&entry, &sign(terms, 39_999, "inv-7"), 6)
                .unwrap_err(),
            "amount_mismatch"
        );
        assert_eq!(
            sponsor.verify(&entry, &good, 9).unwrap_err(),
            "amount_mismatch"
        );
        let mut other = terms.clone();
        other.pay_to = sponsor.fee_payer();
        assert_eq!(
            sponsor
                .verify(&entry, &sign(&other, 40_000, "inv-7"), 6)
                .unwrap_err(),
            "recipient_mismatch"
        );
        let mut other = terms.clone();
        other.mint = Pubkey::parse("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB").unwrap();
        assert_eq!(
            sponsor
                .verify(&entry, &sign(&other, 40_000, "inv-7"), 6)
                .unwrap_err(),
            "mint_mismatch"
        );
        assert_eq!(
            sponsor
                .verify(&entry, &sign(terms, 40_000, "inv-8"), 6)
                .unwrap_err(),
            "memo_mismatch"
        );
        // Another sponsor's entry; an entry not echoed as offered.
        let elsewhere = Sponsor::new(&[6u8; 32]).unwrap();
        assert_eq!(
            elsewhere.verify(&entry, &good, 6).unwrap_err(),
            "fee_payer_not_managed"
        );
        let mut changed = good.clone();
        changed["accepted"]["amount"] = json!("1");
        assert!(
            sponsor
                .verify(&entry, &changed, 6)
                .unwrap_err()
                .contains("accepted")
        );
    }

    /// Under external custody: the reference signer's Ed25519 key names its
    /// address (`public_key`, `EdDSA`), signs the transfer its policy allows
    /// — which the sponsor verifies — and refuses, `denied`, one to a payee,
    /// of a mint or past an amount it does not sign for, and any message not
    /// of the transfer's shape. agentd sends it the message, never a key.
    #[test]
    fn the_external_signer_signs_only_the_transfer_its_policy_allows() {
        use crate::sec::signer::{Signer, SignerError};
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        let seed = [7u8; 32];
        let knobs = json!({"keys": {"sol": {"alg": "EdDSA", "hex": crate::sha::to_hex(&seed),
            "assets": [format!("{MAINNET}/token:{USDC}")], "payees": [PAY_TO], "max_value": "50000"},
            "seal": {"use": "wrap"}}});
        let _m = crate::sec::signer::mock::serve(&sock, &knobs).unwrap();
        // SAFETY: geteuid has no failure mode.
        let uid = unsafe { libc::geteuid() };
        let p: crate::config::settings::payments::Payments = serde_json::from_value(json!({
            "key_custody": "external",
            "signer": {"socket": sock.display().to_string(), "uid": uid},
            "wallets": {"sol": {"chain": "solana", "key": "sol"}},
            "assets": {"usdc-sol": {"asset": format!("{MAINNET}/token:{USDC}"), "decimals": 6,
                "limits": {"per_run": "1", "per_day": "1", "per_payee": "1"}}},
            "payees": {"weather": {"address": PAY_TO}}
        }))
        .unwrap();
        let (wallets, unavailable) = crate::payments::wallet::Wallets::open(&p).unwrap();
        assert!(unavailable.is_empty());
        let owner = svm::pubkey_of(&svm::key_pair(&seed).unwrap());
        assert_eq!(
            wallets.known("sol"),
            Some(crate::payments::scheme::Account::Svm(owner))
        );
        let sponsor = Sponsor::new(&[5u8; 32]).unwrap();
        let mint = Pubkey::parse(USDC).unwrap();
        let pay_to = Pubkey::parse(PAY_TO).unwrap();
        let entry = sponsor.entry(MAINNET, &mint, &pay_to, 40_000, None, Some(&[3u8; 32]));
        let required = json!({"x402Version": 2, "resource": {"url": "https://api.example/data"},
                              "accepts": [entry.clone()]});
        let pol = policy::Policy::from_settings(&p).unwrap();
        let choice = policy::choose(&pol, &step(), std::slice::from_ref(&entry), &none).unwrap();
        let crate::payments::scheme::Terms::Svm(terms) = &choice.terms else {
            panic!("a Solana entry's terms");
        };
        let memo = terms.memo_for(&[1; 32]);
        let payload = paid(&wallets, terms, 40_000, &memo, &required, &entry).unwrap();
        assert_eq!(sponsor.verify(&entry, &payload, 6).unwrap().payer, owner);
        // Past its policy: refused by the signer, nothing signed.
        let denied = |r: Result<Value, crate::payments::wallet::SignError>| matches!(r, Err(crate::payments::wallet::SignError::Refused(m)) if m.contains("denied"));
        assert!(denied(paid(
            &wallets, terms, 50_001, &memo, &required, &entry
        )));
        let mut elsewhere = terms.clone();
        elsewhere.pay_to = sponsor.fee_payer();
        assert!(denied(paid(
            &wallets, &elsewhere, 1, &memo, &required, &entry
        )));
        let mut usdt = terms.clone();
        usdt.mint = Pubkey::parse("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB").unwrap();
        assert!(denied(paid(&wallets, &usdt, 1, &memo, &required, &entry)));
        let s = Signer::new(&sock, uid, None);
        let m = terms.message(&owner, 1, b"m", [3; 32]).unwrap();
        let mut extra = m.clone();
        extra.instructions.push(extra.instructions[3].clone());
        for bytes in [
            extra.serialize(),
            b"not a message".to_vec(),
            m.serialize()[1..].to_vec(),
        ] {
            assert!(matches!(
                s.sign_solana_transaction("sol", &bytes),
                Err(SignerError::Refused { code, .. }) if code == "denied"
            ));
        }
        // An EVM op of a Solana key, and a Solana op of no Solana key.
        assert!(
            matches!(s.sign_typed_data("sol", &json!({})), Err(SignerError::Refused { code, .. }) if code == "wrong_use")
        );
        assert!(
            matches!(s.sign_solana_transaction("seal", &m.serialize()), Err(SignerError::Refused { code, .. }) if code == "wrong_use")
        );
    }

    /// The policy holds a Solana entry as it holds an EVM one: an unlisted
    /// mint, payee or network is passed over unsent, each named; so is an
    /// entry with no blockhash on a network with no RPC endpoint, an EVM
    /// payee on Solana, and a step that does not pay in the asset.
    #[test]
    fn the_policy_refuses_an_unlisted_mint_payee_or_network() {
        let (_dir, p, _wallets, pol) = setup();
        let sponsor = Sponsor::new(&[5u8; 32]).unwrap();
        let mint = Pubkey::parse(USDC).unwrap();
        let pay_to = Pubkey::parse(PAY_TO).unwrap();
        let entry = sponsor.entry(MAINNET, &mint, &pay_to, 40_000, None, None);
        let ok = policy::choose(&pol, &step(), std::slice::from_ref(&entry), &none).unwrap();
        let crate::payments::scheme::Terms::Svm(t) = &ok.terms else {
            panic!("a Solana entry's terms");
        };
        assert_eq!(
            t.rpc.as_deref(),
            Some("https://rpc.example/solana"),
            "no blockhash: the RPC is asked"
        );
        let refused = |e: Value, pol: &policy::Policy| {
            policy::choose(pol, &step(), &[e], &none).unwrap_err()[0]
                .reasons
                .join("; ")
        };
        let with = |f: &dyn Fn(&mut Value)| {
            let mut e = entry.clone();
            f(&mut e);
            e
        };
        for (e, want) in [
            (
                with(&|e| e["asset"] = json!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB")),
                "asset",
            ),
            (
                with(&|e| e["payTo"] = json!("EwWqGE4ZFKLofuestmU4LDdK7XM1N4ALgdZccwYugwGd")),
                "not an allowed payee",
            ),
            (
                with(&|e| e["payTo"] = json!("0x209693Bc6afc0C5328bA36FaF03C514EF312287C")),
                "payee",
            ),
            (
                with(&|e| e["network"] = json!("solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1")),
                "network",
            ),
            (
                with(&|e| {
                    e["extra"].as_object_mut().unwrap().remove("feePayer");
                }),
                "feePayer",
            ),
            (with(&|e| e["amount"] = json!("100001")), "max"),
        ] {
            let why = refused(e, &pol);
            assert!(why.contains(want), "{want}: {why}");
        }
        let mut no_rpc = pol.clone();
        no_rpc.rpc.clear();
        assert!(refused(entry.clone(), &no_rpc).contains("no recentBlockhash"));
        // A step whose max names the asset is held to the wallet's chain at
        // load: an EVM wallet does not pay a Solana asset.
        let mut evm = p.clone();
        evm.wallets.get_mut("sol").unwrap().chain =
            crate::config::settings::payments::Chain::Eip155;
        let e = crate::config::settings::payments::step_payment(
            &evm,
            &json!({"wallet": "sol", "max": "0.05 usdc-sol"}),
        )
        .unwrap_err();
        assert!(e.contains("solana chains"), "{e}");
    }
}
