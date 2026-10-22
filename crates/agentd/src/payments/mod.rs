// SPDX-License-Identifier: AGPL-3.0-only
//! Paying for things: agentd as a paying client of x402 v2, only within the
//! spend policy ADR 0003 asks for (RFC 0049): a resource over HTTP, an MCP
//! tool or an A2A peer, paid with the `exact` scheme on an EVM chain (an
//! EIP-3009 authorization) or on Solana (an SPL transfer).
//!
//! The pieces, each its own module:
//!
//! - [`x402`] — the wire: `PaymentRequired`, `PaymentPayload`,
//!   `SettlementResponse` as base64 JSON headers, and the authorization as
//!   EIP-712 typed data;
//! - [`scheme`] — the chain families agentd pays on with `exact`, the one
//!   place that tells them apart: EVM and Solana;
//! - [`eip712`], [`evm`], [`caip`] — typed-data hashing, secp256k1 keys,
//!   addresses and recovery, chain and asset ids;
//! - [`policy`] — which entry of a 402's `accepts` the step and the spend
//!   policy allow, or every reason none is;
//! - [`ledger`] — every paid request's record, on the disk before the
//!   signed request leaves, so a payment whose outcome is unknown is never
//!   made a second time;
//! - [`wallet`] — the keys: sealed files opened per signature, or the
//!   external signer;
//! - [`svm`], [`base58`], [`ed25519`] — the `exact` scheme on Solana (RFC
//!   0049 §10): the SPL transfer's transaction, associated token accounts
//!   and the curve check they need, and base58;
//! - `mock` — a resource server that asks to be paid, on EVM and Solana,
//!   and `svm_mock`, the Solana sponsor it settles through, for tests
//!   (debug and `internal-mocks` builds).
//!
//! The runtime's half — a paying step's exchange on any transport, the
//! approver's gate, the audit records — is `runtime::payments`. Everything
//! here runs in the supervisor: no worker builds, signs or sends a payment.

pub mod base58;
pub mod caip;
pub mod ed25519;
pub mod eip712;
pub mod evm;
pub mod ledger;
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;
pub mod policy;
pub mod scheme;
pub mod svm;
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod svm_mock;
pub mod wallet;
pub mod x402;

use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::config::settings::Settings;
use crate::obs::log::Logger;
use crate::state::Durable;

/// The payments a start opened, and the ledger's records it made
/// `unknown`.
pub type Opened = (Arc<Payments>, Vec<ledger::Record>);

/// The spend policy, the wallets and the ledger, held by the supervisor.
pub struct Payments {
    pub policy: policy::Policy,
    pub wallets: wallet::Wallets,
    /// Written on the supervisor's loop alone.
    pub ledger: Mutex<ledger::Ledger>,
}

impl Payments {
    /// `security.payments`, opened: the policy read, every wallet checked —
    /// a key file that does not open to its address, or a signer that
    /// refuses a key, refuses the start; a signer that does not answer is
    /// said, and asked again at the first payment — and the ledger read, a
    /// payment still out when the process stopped made `unknown`. `None`
    /// when no payment is configured; beside the payments, the records
    /// made `unknown`, for the audit chain to close.
    pub fn build(
        settings: &Settings,
        durable: Arc<Durable>,
        log: &Logger,
    ) -> Result<Option<Opened>, String> {
        let p = &settings.security.payments;
        if !p.declared() {
            return Ok(None);
        }
        let policy = policy::Policy::from_settings(p)?;
        let (wallets, unavailable) = wallet::Wallets::open(p).map_err(|e| e.to_string())?;
        for e in unavailable {
            log.warn(
                "payment.signer.unavailable",
                json!({"error": e, "msg": "no payment is made from a wallet until the signer names its address"}),
            );
        }
        let (ledger, unknown) = ledger::Ledger::open(durable)?;
        for r in &unknown {
            log.warn(
                "payment.ledger.restored",
                json!({"request": r.request, "run": r.run, "step": r.step, "workflow": r.workflow,
                       "asset": r.asset, "amount": r.amount, "payee": r.payee, "nonce": r.nonce,
                       "state": r.state.as_str(),
                       "msg": "agentd stopped while this payment was out: its outcome is unknown, and it is never made again"}),
            );
        }
        for name in p.wallets.keys() {
            if let Some(a) = wallets.known(name) {
                log.info(
                    "payment.wallet",
                    json!({"wallet": name, "address": a.to_string(),
                           "custody": if p.external().is_some() { "external" } else { "local" }}),
                );
            }
        }
        Ok(Some((
            Arc::new(Payments {
                policy,
                wallets,
                ledger: Mutex::new(ledger),
            }),
            unknown,
        )))
    }

    /// The ledger, locked.
    pub fn ledger(&self) -> std::sync::MutexGuard<'_, ledger::Ledger> {
        self.ledger.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `agentd --print-wallet-address <wallet>`: the address to fund. Under
/// local custody the key file is made `0600` when absent, sealed under the
/// seal key; under external custody the signer is asked. Dials nothing
/// else.
pub fn print_wallet_address(settings: &Settings, wallet: &str) -> Result<String, String> {
    let p = &settings.security.payments;
    let w = p
        .wallets
        .get(wallet)
        .ok_or_else(|| format!("{wallet:?} is not a security.payments.wallets entry"))?;
    use crate::config::settings::payments::Chain;
    match (p.external(), &w.key_file) {
        (None, Some(f)) => {
            let seal = wallet::seal_key(p)?;
            let path = std::path::Path::new(f);
            let exists = std::fs::symlink_metadata(path).is_ok();
            Ok(match (w.chain, exists) {
                (Chain::Eip155, true) => wallet::read(path, &seal, wallet)?.0.checksummed(),
                (Chain::Eip155, false) => wallet::create(path, &seal, wallet)?.checksummed(),
                (Chain::Solana, true) => wallet::read_svm(path, &seal, wallet)?.0.to_string(),
                (Chain::Solana, false) => wallet::create_svm(path, &seal, wallet)?.to_string(),
            })
        }
        _ => {
            let (wallets, _) = wallet::Wallets::open(p).map_err(|e| e.to_string())?;
            wallets
                .address(wallet)
                .map(|a| a.to_string())
                .map_err(|e| e.to_string())
        }
    }
}
