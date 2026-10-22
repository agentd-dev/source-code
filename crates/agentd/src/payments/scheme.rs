// SPDX-License-Identifier: AGPL-3.0-only
//! The chain families agentd pays on, each with x402's `exact` scheme (RFC
//! 0049 §2, §7) — the one place that tells them apart. A network's CAIP-2
//! namespace names its [`Family`]; the family reads an operator's asset and
//! payee, an `accepts` entry's token, `payTo` and `extra`, fixes what a
//! payment's ledger record holds before anything is signed ([`prepare`]),
//! and signs it into the `payload` of a `PaymentPayload` ([`sign`]).
//!
//! Unit 1 is EVM: `eip155` chains, ERC-20 tokens (`erc20` in CAIP-19), the
//! token's EIP-712 domain in `extra`, an EIP-3009 `TransferWithAuthorization`
//! signed by the wallet. Solana (`solana`, SPL `token`) is unit 2's: a
//! variant of each enum here, and nothing outside this module asks which
//! family it pays on.

use std::time::Duration;

use serde_json::{Value, json};

use super::caip::{AssetId, ChainId};
use super::evm::{self, Address};
use super::policy::Choice;
use super::wallet::{SignError, Wallets};
use super::x402::Authorization;

/// How long before a signature an EVM authorization's `validAfter` is: the
/// clock between agentd and the chain may differ by that much.
pub const SKEW_S: u64 = 30;

/// A chain family agentd pays on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `eip155` chains: ERC-20 tokens, EIP-3009 authorizations.
    Evm,
}

/// A token on its chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Evm { chain_id: u64, contract: Address },
}

/// An account: a payee's `payTo`, a wallet's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Account {
    Evm(Address),
}

impl std::fmt::Display for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Account::Evm(a) => f.write_str(&a.checksummed()),
        }
    }
}

impl Account {
    /// An address as the operator writes one: `0x` and 40 hex digits, held
    /// to EIP-55's checksum when its case is mixed.
    pub fn parse(s: &str) -> Result<Account, String> {
        if s.starts_with("0x") {
            return Address::parse(s).map(Account::Evm);
        }
        Err(format!(
            "{s:?} is not an address agentd pays (0x and 40 hex digits, an EVM account)"
        ))
    }

    /// The family it is an account of.
    pub fn family(&self) -> Family {
        match self {
            Account::Evm(_) => Family::Evm,
        }
    }
}

/// What an entry's `extra` says that a signature is made under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terms {
    /// The token's EIP-712 domain name and version.
    Evm { name: String, version: String },
}

/// What is fixed on the loop before a payment is signed, and written in its
/// ledger record first: the nonce, and until when the payment can land.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The nonce, as the record and the audit chain show it.
    pub nonce: String,
    /// Unix seconds after which the authorization can no longer settle.
    pub valid_before: u64,
    detail: Detail,
}

#[derive(Debug, Clone)]
enum Detail {
    Evm { nonce: [u8; 32], valid_after: u64 },
}

impl Family {
    /// The family of a network, when agentd pays on it.
    pub fn of(network: &ChainId) -> Option<Family> {
        match network.namespace.as_str() {
            "eip155" => Some(Family::Evm),
            _ => None,
        }
    }

    /// An allowed asset's token, from its CAIP-19 id.
    pub fn token(self, asset: &AssetId) -> Result<Token, String> {
        match self {
            Family::Evm => asset
                .erc20()
                .map(|(chain_id, contract)| Token::Evm { chain_id, contract })
                .ok_or_else(|| {
                    "not an ERC-20 token on an EVM chain (eip155:<chain id>/erc20:<contract>)"
                        .to_string()
                }),
        }
    }

    /// An entry's `asset`, on `network`: the token the server asks for.
    pub fn entry_token(self, network: &ChainId, asset: &str) -> Result<Token, String> {
        match self {
            Family::Evm => {
                let chain_id = network
                    .evm()
                    .ok_or_else(|| format!("{network} is not an EVM chain id"))?;
                let contract = Address::parse_any(asset)
                    .map_err(|_| format!("{asset:?} is not a token address"))?;
                Ok(Token::Evm { chain_id, contract })
            }
        }
    }

    /// An entry's `payTo`, whatever its case: servers miswrite EIP-55, and
    /// the bytes are what is allowed or not.
    pub fn entry_account(self, s: &str) -> Result<Account, String> {
        match self {
            Family::Evm => Address::parse_any(s)
                .map(Account::Evm)
                .map_err(|_| format!("{s:?} is not an address")),
        }
    }

    /// What `extra` must say for agentd to sign an entry of this family, or
    /// every reason it does not (RFC 0049 §2): on EVM the `eip3009` transfer
    /// method — the default when `assetTransferMethod` is absent — and the
    /// token's EIP-712 `name` and `version`.
    pub fn terms(self, extra: &Value) -> Result<Terms, Vec<String>> {
        match self {
            Family::Evm => {
                let mut why = Vec::new();
                match extra.get("assetTransferMethod").and_then(Value::as_str) {
                    None | Some("eip3009") => {}
                    Some(m) => {
                        why.push(format!("transfer_method {m:?}: agentd signs only eip3009"))
                    }
                }
                let s = |k: &str| extra.get(k).and_then(Value::as_str).map(str::to_string);
                match (s("name"), s("version")) {
                    (Some(name), Some(version)) if why.is_empty() => {
                        Ok(Terms::Evm { name, version })
                    }
                    (n, v) => {
                        if n.is_none() || v.is_none() {
                            why.push("extra: no EIP-712 name and version of the token".into());
                        }
                        Err(why)
                    }
                }
            }
        }
    }
}

/// Fix `choice`'s payment at `now_s`, on the loop, before its record is
/// written: on EVM a nonce of 32 bytes from the kernel's CSPRNG, valid from
/// [`SKEW_S`] before now until the server's `maxTimeoutSeconds` or
/// `max_validity` after it, whichever is sooner.
pub fn prepare(choice: &Choice, max_validity: Duration, now_s: u64) -> Result<Prepared, String> {
    match choice.token {
        Token::Evm { .. } => {
            let mut nonce = [0u8; 32];
            crate::sec::random::fill(&mut nonce).map_err(|e| format!("no OS randomness: {e}"))?;
            let window = choice.max_timeout.min(max_validity.as_secs()).max(1);
            Ok(Prepared {
                nonce: evm::hex0x(&nonce),
                valid_before: now_s + window,
                detail: Detail::Evm {
                    nonce,
                    valid_after: now_s.saturating_sub(SKEW_S),
                },
            })
        }
    }
}

/// Sign `choice`'s payment as `prepared` fixed it, from `wallet`: the
/// `payload` member of the `PaymentPayload`. Off the loop — an external
/// signer may take its timeout to answer. On EVM, the EIP-3009
/// authorization's typed data, signed by the wallet and recovered to its
/// address before it is handed back.
pub fn sign(
    wallets: &Wallets,
    wallet: &str,
    choice: &Choice,
    prepared: &Prepared,
) -> Result<Value, SignError> {
    match (
        &choice.token,
        &choice.pay_to,
        &choice.terms,
        &prepared.detail,
    ) {
        (
            Token::Evm { chain_id, contract },
            Account::Evm(to),
            Terms::Evm { name, version },
            Detail::Evm { nonce, valid_after },
        ) => {
            let auth = Authorization {
                from: wallets.address(wallet)?,
                to: *to,
                value: choice.amount,
                valid_after: *valid_after,
                valid_before: prepared.valid_before,
                nonce: *nonce,
            };
            let td = auth.typed_data(name, version, *chain_id, contract);
            let sig = wallets.sign_typed_data(wallet, &td)?;
            Ok(json!({"signature": evm::hex0x(&sig), "authorization": auth.to_json()}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A network's namespace names its family; one agentd pays on no chain
    /// of is none.
    #[test]
    fn a_network_names_its_family() {
        let n = |s: &str| ChainId::parse(s).unwrap();
        assert_eq!(Family::of(&n("eip155:8453")), Some(Family::Evm));
        assert_eq!(
            Family::of(&n("bip122:000000000019d6689c085ae165831e93")),
            None
        );
        let usdc =
            AssetId::parse("eip155:8453/erc20:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap();
        let t = Family::Evm.token(&usdc).unwrap();
        assert_eq!(
            Family::Evm
                .entry_token(
                    &n("eip155:8453"),
                    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
                )
                .unwrap(),
            t,
            "the server's address in any case is the operator's token"
        );
        assert_ne!(
            Family::Evm
                .entry_token(&n("eip155:1"), "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913")
                .unwrap(),
            t,
            "the same contract on another chain is another token"
        );
        assert!(
            Family::Evm
                .token(&AssetId::parse("eip155:1/slip44:60").unwrap())
                .is_err()
        );
    }

    /// EVM terms: the token's domain name and version, `eip3009` alone.
    #[test]
    fn evm_terms_are_the_tokens_domain_and_eip3009() {
        assert_eq!(
            Family::Evm.terms(&json!({"name": "USDC", "version": "2"})),
            Ok(Terms::Evm {
                name: "USDC".into(),
                version: "2".into()
            })
        );
        assert!(
            Family::Evm
                .terms(&json!({"name": "USDC", "version": "2", "assetTransferMethod": "eip3009"}))
                .is_ok()
        );
        let why = Family::Evm
            .terms(&json!({"name": "USDC", "assetTransferMethod": "permit2"}))
            .unwrap_err();
        assert_eq!(why.len(), 2, "{why:?}");
        assert!(Family::Evm.terms(&Value::Null).is_err());
    }
}
