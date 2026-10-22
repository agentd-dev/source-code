// SPDX-License-Identifier: AGPL-3.0-only
//! Chain and asset ids (RFC 0049 §4): CAIP-2, how x402 v2 names a network
//! [X §11.1], and CAIP-19, how the operator names an asset it allows.
//!
//! - CAIP-2 `chain_id`: `namespace:reference`, the namespace
//!   `[-a-z0-9]{3,8}`, the reference `[-_a-zA-Z0-9]{1,32}`.
//! - CAIP-19 `asset_type`: `chain_id/asset_namespace:asset_reference`, the
//!   asset namespace `[-a-z0-9]{3,8}`, the reference `[-.%a-zA-Z0-9]{1,128}`.
//!   An `asset_id` (a `/token_id` after it, one NFT) is not taken: x402 pays
//!   in fungible tokens.
//!
//! [`ChainId::evm`] and [`AssetId::erc20`] read the `eip155` forms unit 1
//! pays on: a chain id in decimal with no leading zero, and a token
//! contract's address.

use super::evm::Address;

/// A CAIP-2 chain id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChainId {
    pub namespace: String,
    pub reference: String,
}

/// A CAIP-19 asset type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AssetId {
    pub chain: ChainId,
    pub namespace: String,
    pub reference: String,
}

fn namespace(s: &str) -> bool {
    (3..=8).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl ChainId {
    pub fn parse(s: &str) -> Result<ChainId, String> {
        let bad =
            || format!("{s:?} is not a CAIP-2 chain id (namespace:reference, e.g. eip155:8453)");
        let (ns, reference) = s.split_once(':').ok_or_else(bad)?;
        let ok_ref = (1..=32).contains(&reference.len())
            && reference
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !namespace(ns) || !ok_ref {
            return Err(bad());
        }
        Ok(ChainId {
            namespace: ns.to_string(),
            reference: reference.to_string(),
        })
    }

    /// The EIP-155 chain id of an `eip155` chain.
    pub fn evm(&self) -> Option<u64> {
        (self.namespace == "eip155"
            && !(self.reference.len() > 1 && self.reference.starts_with('0'))
            && self.reference.bytes().all(|b| b.is_ascii_digit()))
        .then(|| self.reference.parse().ok())
        .flatten()
        .filter(|n| *n > 0)
    }
}

impl std::fmt::Display for ChainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.namespace, self.reference)
    }
}

impl AssetId {
    pub fn parse(s: &str) -> Result<AssetId, String> {
        let bad = || {
            format!(
                "{s:?} is not a CAIP-19 asset type (chain_id/namespace:reference, e.g. eip155:8453/erc20:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913)"
            )
        };
        let (chain, asset) = s.split_once('/').ok_or_else(bad)?;
        let chain = ChainId::parse(chain).map_err(|_| bad())?;
        let (ns, reference) = asset.split_once(':').ok_or_else(bad)?;
        let ok_ref = (1..=128).contains(&reference.len())
            && reference
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'%'));
        if !namespace(ns) || !ok_ref {
            return Err(bad());
        }
        Ok(AssetId {
            chain,
            namespace: ns.to_string(),
            reference: reference.to_string(),
        })
    }

    /// An ERC-20 token on an EVM chain: the chain id and the contract.
    pub fn erc20(&self) -> Option<(u64, Address)> {
        if self.namespace != "erc20" {
            return None;
        }
        Some((self.chain.evm()?, Address::parse(&self.reference).ok()?))
    }
}

impl std::fmt::Display for AssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}:{}", self.chain, self.namespace, self.reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CAIP-2's own examples parse and print back; the networks x402 names
    /// read as EVM chains or not; malformed ids are refused.
    #[test]
    fn caip2_chain_ids() {
        for s in [
            "eip155:1",
            "eip155:8453",
            "bip122:000000000019d6689c085ae165831e93",
            "cosmos:cosmoshub-3",
            "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
            "chainstd:8c3444cf8970a9e41a706fab93e7a6c4",
        ] {
            assert_eq!(ChainId::parse(s).unwrap().to_string(), s);
        }
        assert_eq!(ChainId::parse("eip155:84532").unwrap().evm(), Some(84532));
        assert_eq!(
            ChainId::parse("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp")
                .unwrap()
                .evm(),
            None
        );
        assert_eq!(ChainId::parse("eip155:08453").unwrap().evm(), None);
        assert_eq!(ChainId::parse("eip155:0").unwrap().evm(), None);
        for bad in [
            "eip155",
            "EIP155:1",
            "ei:1",
            "toolongns:1",
            "eip155:",
            "eip155:a/b",
            &format!("eip155:{}", "1".repeat(33)),
        ] {
            assert!(ChainId::parse(bad).is_err(), "{bad}");
        }
    }

    /// CAIP-19's examples parse; an ERC-20 asset reads as its chain and
    /// contract; an NFT's token id, a bad namespace or a bad contract is no
    /// ERC-20 here.
    #[test]
    fn caip19_asset_types() {
        for s in [
            "eip155:1/slip44:60",
            "bip122:000000000019d6689c085ae165831e93/slip44:0",
            "cosmos:cosmoshub-3/slip44:118",
            "eip155:1/erc20:0x6b175474e89094c44da98b954eedeac495271d0f",
            "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp/token:EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        ] {
            assert_eq!(AssetId::parse(s).unwrap().to_string(), s);
        }
        let usdc =
            AssetId::parse("eip155:8453/erc20:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap();
        let (chain, contract) = usdc.erc20().unwrap();
        assert_eq!(chain, 8453);
        assert_eq!(
            contract.checksummed(),
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
        );
        assert!(
            AssetId::parse("eip155:1/slip44:60")
                .unwrap()
                .erc20()
                .is_none()
        );
        assert!(
            AssetId::parse("eip155:1/erc721:0x06012c8cf97BEaD5deAe237070F9587f8E7A266d")
                .unwrap()
                .erc20()
                .is_none(),
            "an NFT contract is no ERC-20 token"
        );
        assert!(
            AssetId::parse("eip155:1/erc20:0x1234")
                .unwrap()
                .erc20()
                .is_none()
        );
        for bad in [
            "eip155:1",
            "eip155:1/erc20",
            "eip155:1/ERC20:0x6b175474e89094c44da98b954eedeac495271d0f",
            "eip155:1/erc721:0x06012c8cf97BEaD5deAe237070F9587f8E7A266d/771769",
            "eip155:1/erc20:0x6b17 5474",
        ] {
            assert!(AssetId::parse(bad).is_err(), "{bad}");
        }
    }
}
