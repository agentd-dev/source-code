// SPDX-License-Identifier: AGPL-3.0-only
//! The chain families agentd pays on, each with x402's `exact` scheme (RFC
//! 0049 §2, §7, §10) — the one place that tells them apart. A network's
//! CAIP-2 namespace names its [`Family`]; the family reads an operator's
//! asset and payee, an `accepts` entry's token, `payTo` and `extra`, fixes
//! what a payment's ledger record holds before anything is signed
//! ([`prepare`]), and signs it into the `payload` of a `PaymentPayload`
//! ([`sign`]).
//!
//! - **EVM**: `eip155` chains, ERC-20 tokens (`erc20` in CAIP-19), the
//!   token's EIP-712 domain in `extra`, an EIP-3009
//!   `TransferWithAuthorization` signed by the wallet.
//! - **Solana**: `solana` networks, SPL tokens (`token` in CAIP-19), the
//!   sponsor's `feePayer` and a blockhash in `extra` (or the operator's RPC
//!   endpoint asked for one), a `TransferChecked` transaction signed by the
//!   wallet and left for the sponsor to sign as fee payer
//!   ([`super::svm`]).
//!
//! Nothing outside this module asks which family it pays on.

use std::time::Duration;

use serde_json::{Value, json};

use super::caip::{AssetId, ChainId};
use super::evm::{self, Address};
use super::policy::{Asset, Choice};
use super::svm::{self, Pubkey, TokenProgram};
use super::wallet::{SignError, Wallets};
use super::x402::{Authorization, Entry};
use crate::config::settings::payments::parse_atomic;

/// How long before a signature an EVM authorization's `validAfter` is: the
/// clock between agentd and the chain may differ by that much.
pub const SKEW_S: u64 = 30;

/// A chain family agentd pays on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `eip155` chains: ERC-20 tokens, EIP-3009 authorizations.
    Evm,
    /// `solana` networks: SPL tokens, transfers a sponsor pays the fee of.
    Svm,
}

/// A token on its chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Evm {
        chain_id: u64,
        contract: Address,
    },
    /// The mint, and the program it belongs to: its transfers and its
    /// associated token accounts are that program's.
    Svm {
        mint: Pubkey,
        program: TokenProgram,
    },
}

impl Token {
    /// Whether an entry's `asset` is this token: the contract, in any case;
    /// the mint.
    pub fn is(&self, asset: &str) -> bool {
        match self {
            Token::Evm { contract, .. } => Address::parse_any(asset).is_ok_and(|a| a == *contract),
            Token::Svm { mint, .. } => Pubkey::parse(asset).is_ok_and(|m| m == *mint),
        }
    }
}

/// An account: a payee's `payTo`, a wallet's address. The two families are
/// told apart by how they are written — `0x` and 40 hex digits, or base58
/// of 32 bytes, an alphabet with no `0` — and never compared across: an EVM
/// payee is never a Solana entry's `payTo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Account {
    Evm(Address),
    Svm(Pubkey),
}

/// EIP-55's checksummed address, or base58: the form ledger records keep
/// and compare.
impl std::fmt::Display for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Account::Evm(a) => f.write_str(&a.checksummed()),
            Account::Svm(k) => write!(f, "{k}"),
        }
    }
}

impl Account {
    /// An address as the operator writes one: `0x` and 40 hex digits, held
    /// to EIP-55's checksum when its case is mixed, or a Solana key in
    /// base58.
    pub fn parse(s: &str) -> Result<Account, String> {
        if s.starts_with("0x") {
            Address::parse(s).map(Account::Evm)
        } else {
            Pubkey::parse(s).map(Account::Svm)
        }
    }

    /// The family it is an account of.
    pub fn family(&self) -> Family {
        match self {
            Account::Evm(_) => Family::Evm,
            Account::Svm(_) => Family::Svm,
        }
    }

    pub fn evm(&self) -> Option<Address> {
        match self {
            Account::Evm(a) => Some(*a),
            Account::Svm(_) => None,
        }
    }

    pub fn svm(&self) -> Option<Pubkey> {
        match self {
            Account::Svm(k) => Some(*k),
            Account::Evm(_) => None,
        }
    }
}

/// What an entry asks a signature to be made under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terms {
    /// The token's EIP-712 domain name and version.
    Evm { name: String, version: String },
    /// The SPL transfer: its token, payee, sponsor, memo and blockhash.
    Svm(svm::Terms),
}

/// What is fixed on the loop before a payment is signed, and written in its
/// ledger record first: the nonce, and until when the payment can land.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The nonce, as the record and the audit chain show it: on EVM the
    /// authorization's, on Solana the memo the transfer carries.
    pub nonce: String,
    /// Unix seconds after which the payment can no longer settle.
    pub valid_before: u64,
    detail: Detail,
}

#[derive(Debug, Clone)]
enum Detail {
    Evm { nonce: [u8; 32], valid_after: u64 },
    Svm,
}

impl Family {
    /// The family of a network, when agentd pays on it.
    pub fn of(network: &ChainId) -> Option<Family> {
        match network.namespace.as_str() {
            "eip155" => Some(Family::Evm),
            "solana" => Some(Family::Svm),
            _ => None,
        }
    }

    /// An allowed asset's token, from its CAIP-19 id and, on Solana, the
    /// token program the operator names (`spl-token` when none).
    pub fn token(self, asset: &AssetId, program: Option<&str>) -> Result<Token, String> {
        match self {
            Family::Evm => asset
                .erc20()
                .map(|(chain_id, contract)| Token::Evm { chain_id, contract })
                .ok_or_else(|| {
                    "not an ERC-20 token on an EVM chain (eip155:<chain id>/erc20:<contract>)"
                        .to_string()
                }),
            Family::Svm => {
                let mint = svm::mint_of(asset).ok_or_else(|| {
                    "not an SPL token on Solana (solana:<genesis hash prefix>/token:<mint>)"
                        .to_string()
                })?;
                let program = TokenProgram::parse(program.unwrap_or(TokenProgram::Spl.name()))?;
                Ok(Token::Svm { mint, program })
            }
        }
    }

    /// An entry's `payTo`: on EVM whatever its case — servers miswrite
    /// EIP-55, and the bytes are what is allowed or not — on Solana a key.
    pub fn entry_account(self, s: &str) -> Result<Account, String> {
        match self {
            Family::Evm => Address::parse_any(s)
                .map(Account::Evm)
                .map_err(|_| format!("{s:?} is not an address")),
            Family::Svm => Pubkey::parse(s)
                .map(Account::Svm)
                .map_err(|_| format!("{s:?} is not a Solana address")),
        }
    }

    /// What `e` asks a signature of this family to be made under, or every
    /// reason agentd cannot sign it (RFC 0049 §2, §10).
    ///
    /// On EVM, `extra`'s `eip3009` transfer method — the default when
    /// `assetTransferMethod` is absent — and the token's EIP-712 `name` and
    /// `version`. On Solana, the allowed `asset` the entry pays in and the
    /// payee `pay_to` it pays (`None` when either is not, and has been said
    /// why), `extra.feePayer`, a memo that fits, a blockhash in
    /// `extra.recentBlockhash` or `rpc` — the network's RPC endpoint — to
    /// ask for one, and an amount a token transfer can move.
    pub fn terms(
        self,
        e: &Entry,
        asset: Option<&Asset>,
        pay_to: Option<&Account>,
        rpc: Option<&str>,
    ) -> Result<Terms, Vec<String>> {
        let extra = &e.extra;
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
            Family::Svm => {
                let mut why = Vec::new();
                if parse_atomic(&e.amount).is_ok_and(|a| a > u128::from(u64::MAX)) {
                    why.push("amount: past 64 bits, the most a Solana token transfer moves".into());
                }
                let Some((a, Token::Svm { mint, program })) = asset.map(|a| (a, a.token)) else {
                    return Err(why);
                };
                let t = svm::Terms::read(
                    e,
                    mint,
                    program,
                    a.decimals,
                    pay_to.and_then(Account::svm),
                    rpc,
                    &mut why,
                );
                match t {
                    Some(t) if why.is_empty() => Ok(Terms::Svm(t)),
                    _ => Err(why),
                }
            }
        }
    }
}

/// Fix `choice`'s payment at `now_s`, on the loop, before its record is
/// written: a nonce of 32 bytes from the kernel's CSPRNG. On EVM it is the
/// authorization's, valid from [`SKEW_S`] before now until the server's
/// `maxTimeoutSeconds` or `max_validity` after it, whichever is sooner. On
/// Solana the transfer's memo is `extra.memo` or the nonce's first bytes,
/// and — a transaction has no `validBefore`; its blockhash expires — it is
/// counted as able to land until [`svm::LANDS_WITHIN_S`] after now.
pub fn prepare(choice: &Choice, max_validity: Duration, now_s: u64) -> Result<Prepared, String> {
    let mut nonce = [0u8; 32];
    crate::sec::random::fill(&mut nonce).map_err(|e| format!("no OS randomness: {e}"))?;
    match &choice.terms {
        Terms::Evm { .. } => {
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
        Terms::Svm(t) => Ok(Prepared {
            nonce: t.memo_for(&nonce),
            valid_before: now_s + svm::LANDS_WITHIN_S,
            detail: Detail::Svm,
        }),
    }
}

/// Sign `choice`'s payment as `prepared` fixed it, from `wallet`: the
/// `payload` member of the `PaymentPayload`. Off the loop — an external
/// signer may take its timeout to answer, and a Solana RPC endpoint its
/// own. On EVM, the EIP-3009 authorization's typed data, signed by the
/// wallet and recovered to its address before it is handed back; on
/// Solana, the transfer's transaction, signed by the wallet and verified
/// under its address, the fee payer's signature left for the sponsor.
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
            let from = wallets.address(wallet)?.evm().ok_or_else(|| {
                SignError::Refused(format!("wallet {wallet} is not an EVM wallet"))
            })?;
            let auth = Authorization {
                from,
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
        (Token::Svm { .. }, Account::Svm(_), Terms::Svm(t), Detail::Svm) => {
            svm::signed_transfer(wallets, wallet, t, choice.amount, &prepared.nonce)
        }
        _ => Err(SignError::Refused(
            "the entry's token, payee and terms are of different chain families".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A network's namespace names its family; one agentd pays on no chain
    /// of is none. An entry's token is the operator's in any case of the
    /// contract, and only on its own chain.
    #[test]
    fn a_network_names_its_family() {
        let n = |s: &str| ChainId::parse(s).unwrap();
        assert_eq!(Family::of(&n("eip155:8453")), Some(Family::Evm));
        assert_eq!(
            Family::of(&n("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp")),
            Some(Family::Svm)
        );
        assert_eq!(
            Family::of(&n("bip122:000000000019d6689c085ae165831e93")),
            None
        );
        let usdc =
            AssetId::parse("eip155:8453/erc20:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap();
        let t = Family::Evm.token(&usdc, None).unwrap();
        assert!(
            t.is("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            "the server's address in any case is the operator's token"
        );
        assert!(!t.is("0x036CbD53842c5426634e7929541eC2318f3dCF7e"));
        assert!(
            Family::Evm
                .token(&AssetId::parse("eip155:1/slip44:60").unwrap(), None)
                .is_err()
        );
        let sol = AssetId::parse(
            "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp/token:EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        )
        .unwrap();
        let t = Family::Svm.token(&sol, Some("token-2022")).unwrap();
        assert!(matches!(
            t,
            Token::Svm {
                program: TokenProgram::Token2022,
                ..
            }
        ));
        assert!(t.is("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"));
        assert!(Family::Svm.token(&sol, Some("token-2023")).is_err());
        assert!(Family::Svm.token(&usdc, None).is_err());
        assert!(Family::Evm.token(&sol, None).is_err());
    }

    /// EVM terms: the token's domain name and version, `eip3009` alone.
    #[test]
    fn evm_terms_are_the_tokens_domain_and_eip3009() {
        let terms = |extra: Value| {
            let e = Entry::parse(&json!({"scheme": "exact", "network": "eip155:8453",
                "amount": "1", "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
                "payTo": "0x209693Bc6afc0C5328bA36FaF03C514EF312287C", "maxTimeoutSeconds": 60,
                "extra": extra}))
            .unwrap();
            Family::Evm.terms(&e, None, None, None)
        };
        assert_eq!(
            terms(json!({"name": "USDC", "version": "2"})),
            Ok(Terms::Evm {
                name: "USDC".into(),
                version: "2".into()
            })
        );
        assert!(
            terms(json!({"name": "USDC", "version": "2", "assetTransferMethod": "eip3009"}))
                .is_ok()
        );
        let why = terms(json!({"name": "USDC", "assetTransferMethod": "permit2"})).unwrap_err();
        assert_eq!(why.len(), 2, "{why:?}");
        assert!(terms(Value::Null).is_err());
    }

    /// Each family's account reads as itself and prints back; an entry's
    /// `payTo` is read as an account of the network's family alone, and
    /// the operator's EVM address is held to EIP-55.
    #[test]
    fn accounts_of_both_families() {
        let evm = "0x209693Bc6afc0C5328bA36FaF03C514EF312287C";
        let sol = "2wKupLR9q6wXYppw8Gr2NvWxKBUqm4PPJKkQfoxHDBg4";
        assert_eq!(Account::parse(evm).unwrap().to_string(), evm);
        assert_eq!(Account::parse(sol).unwrap().to_string(), sol);
        assert_eq!(Account::parse(sol).unwrap().family(), Family::Svm);
        assert_eq!(
            Family::Evm.entry_account(&evm.to_lowercase()).unwrap(),
            Account::parse(evm).unwrap()
        );
        assert!(
            Account::parse(&evm.replace('B', "b")).is_err(),
            "EIP-55 held for the operator's"
        );
        assert!(Family::Evm.entry_account(sol).is_err());
        assert!(Family::Svm.entry_account(evm).is_err());
        assert_eq!(
            Family::Svm.entry_account(sol).unwrap(),
            Account::parse(sol).unwrap()
        );
        for bad in [
            "",
            "0x12",
            "2wKupLR9q6wXYppw8Gr2NvWxKBUqm4PPJKkQfox",
            "not an address",
        ] {
            assert!(Account::parse(bad).is_err(), "{bad:?}");
        }
    }
}
