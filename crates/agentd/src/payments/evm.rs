// SPDX-License-Identifier: AGPL-3.0-only
//! EVM accounts, as x402's `exact` scheme signs with them (RFC 0049 D5):
//! Keccak-256, secp256k1 keys and their addresses, signatures as an EVM
//! contract checks them, and recovery.
//!
//! - An **address** is the last 20 bytes of the Keccak-256 of the 64-byte
//!   uncompressed public key (without its `0x04` prefix), written `0x` and
//!   40 hex digits. [`Address::checksummed`] writes EIP-55's mixed case;
//!   [`Address::parse`] takes either case and refuses a mixed-case one whose
//!   checksum is wrong, which is how EIP-55 catches a mistyped address.
//! - A **signature** is 65 bytes, `r ‖ s ‖ v`: RFC 6979 deterministic, `s`
//!   in the lower half of the curve order (EIP-2: a contract refuses the
//!   upper), `v` 27 or 28 for the parity of `k × G`'s y.
//! - **Recovery** (`ecrecover`) gives back the address that signed a digest;
//!   a signature with `s` in the upper half recovers nothing here, as it
//!   would at a contract.

use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use serde_json::{Value, json};
use sha3::{Digest, Keccak256};

/// Keccak-256 of `data` (the hash Ethereum calls `keccak256`, which is not
/// FIPS SHA3-256).
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// An EVM account address.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address(pub [u8; 20]);

impl Address {
    /// `0x` and 40 hex digits: all lower, all upper, or EIP-55's mixed case
    /// with a checksum that holds.
    pub fn parse(s: &str) -> Result<Address, String> {
        let hex = s
            .strip_prefix("0x")
            .ok_or_else(|| format!("{s:?} is not an address (0x and 40 hex digits)"))?;
        if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{s:?} is not an address (0x and 40 hex digits)"));
        }
        let mut a = [0u8; 20];
        for (i, b) in a.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                .map_err(|_| format!("{s:?} is not an address"))?;
        }
        let a = Address(a);
        let mixed = hex.bytes().any(|b| b.is_ascii_lowercase())
            && hex.bytes().any(|b| b.is_ascii_uppercase());
        if mixed && a.checksummed() != s {
            return Err(format!(
                "{s:?} has an EIP-55 checksum that does not hold (a mistyped address?); {} is the address with these digits",
                a.checksummed()
            ));
        }
        Ok(a)
    }

    /// `0x` and 40 hex digits in any case, the checksum not read: how an
    /// address a server sent is compared with the operator's, which were
    /// held to EIP-55 at load. Servers miswrite the case (x402's own A2A
    /// example does), and the bytes are what is allowed or not.
    pub fn parse_any(s: &str) -> Result<Address, String> {
        let lower = s.to_ascii_lowercase();
        if !s.starts_with("0x") {
            return Err(format!("{s:?} is not an address (0x and 40 hex digits)"));
        }
        Address::parse(&lower)
    }

    /// `0x` and 40 lowercase hex digits.
    pub fn lower(&self) -> String {
        hex0x(&self.0)
    }

    /// EIP-55: each letter upper case where the matching nibble of the
    /// Keccak-256 of the lowercase hex is 8 or more.
    pub fn checksummed(&self) -> String {
        let lower = crate::sha::to_hex(&self.0);
        let hash = keccak256(lower.as_bytes());
        let mut out = String::from("0x");
        for (i, c) in lower.chars().enumerate() {
            let nibble = (hash[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 0x0f;
            if c.is_ascii_alphabetic() && nibble >= 8 {
                out.push(c.to_ascii_uppercase());
            } else {
                out.push(c);
            }
        }
        out
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.checksummed())
    }
}

impl std::fmt::Debug for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.checksummed())
    }
}

/// `bytes` as `0x` and lowercase hex.
pub fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", crate::sha::to_hex(bytes))
}

/// `0x` and an even number of hex digits, as bytes.
pub fn unhex0x(s: &str) -> Result<Vec<u8>, String> {
    let hex = s
        .strip_prefix("0x")
        .ok_or_else(|| format!("{s:?} is not 0x-prefixed hex"))?;
    if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{s:?} is not 0x-prefixed hex of whole bytes"));
    }
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// The secp256k1 key of these 32 bytes: a scalar in `[1, n)`.
pub fn key_from_bytes(bytes: &[u8; 32]) -> Result<SigningKey, String> {
    SigningKey::from_slice(bytes)
        .map_err(|_| "not a secp256k1 private key (a scalar from 1 to n - 1)".to_string())
}

/// The address of a public key.
pub fn address_of(vk: &VerifyingKey) -> Address {
    let point = vk.to_sec1_point(false);
    let bytes = point.as_bytes();
    // `0x04 ‖ x ‖ y`: the address hashes x and y.
    let hash = keccak256(&bytes[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&hash[12..]);
    Address(a)
}

/// The address a private key signs as.
pub fn address_of_key(sk: &SigningKey) -> Address {
    address_of(sk.verifying_key())
}

/// The 65-byte signature of `digest` by `sk`: RFC 6979, low S, `v` 27/28.
pub fn sign_digest(sk: &SigningKey, digest: &[u8; 32]) -> [u8; 65] {
    let (sig, rid) = sk.sign_prehash_recoverable(digest);
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&sig.to_bytes());
    out[64] = 27 + rid.to_byte();
    out
}

/// The address whose key signed `digest` as `sig` (`ecrecover`). `v` is 27
/// or 28 (0 and 1 are taken too, as many wallets write them); an `s` in the
/// upper half of the order, a recovery id that reduced `x`, and a point at
/// infinity recover nothing.
pub fn recover(digest: &[u8; 32], sig: &[u8]) -> Result<Address, String> {
    if sig.len() != 65 {
        return Err(format!("a signature of {} bytes, not 65", sig.len()));
    }
    let v = match sig[64] {
        27 | 28 => sig[64] - 27,
        0 | 1 => sig[64],
        v => return Err(format!("a signature whose v is {v}, not 27 or 28")),
    };
    let s = Signature::from_slice(&sig[..64]).map_err(|_| "r or s is not in [1, n)".to_string())?;
    if s.normalize_s() != s {
        return Err("a signature whose s is in the upper half of the order (EIP-2)".into());
    }
    let rid = RecoveryId::from_byte(v).ok_or("no recovery id")?;
    VerifyingKey::recover_from_prehash(digest, &s, rid)
        .map(|vk| address_of(&vk))
        .map_err(|_| "the signature recovers no key".to_string())
}

/// A secp256k1 public key as a JWK (RFC 8812): `kty: EC`, `crv: secp256k1`.
pub fn jwk_of(vk: &VerifyingKey) -> Value {
    let point = vk.to_sec1_point(false);
    let bytes = point.as_bytes();
    json!({"kty": "EC", "crv": "secp256k1",
           "x": crate::b64::url_nopad(&bytes[1..33]),
           "y": crate::b64::url_nopad(&bytes[33..65])})
}

/// The address of a secp256k1 public JWK, as the external signer describes
/// a key; a JWK with a private member, another curve, or a point not on
/// secp256k1 is refused.
pub fn address_of_jwk(jwk: &Value) -> Result<Address, String> {
    if jwk.get("d").is_some() {
        return Err("the JWK carries a private member".into());
    }
    if jwk["kty"] != "EC" || jwk["crv"] != "secp256k1" {
        return Err("not a secp256k1 JWK (kty EC, crv secp256k1)".into());
    }
    let coord = |k: &str| -> Result<Vec<u8>, String> {
        jwk[k]
            .as_str()
            .and_then(|s| crate::b64::url_decode(s).ok())
            .filter(|b| b.len() == 32)
            .ok_or_else(|| format!("the JWK's {k} is not 32 base64url bytes"))
    };
    let mut sec1 = vec![0x04];
    sec1.extend(coord("x")?);
    sec1.extend(coord("y")?);
    VerifyingKey::from_sec1_bytes(&sec1)
        .map(|vk| address_of(&vk))
        .map_err(|_| "the JWK's point is not on secp256k1".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keccak-256 is the original Keccak, not FIPS SHA3-256: the empty
    /// string's hash, and "hello"'s, as every Ethereum client has them.
    #[test]
    fn keccak256_is_ethereums() {
        assert_eq!(
            hex0x(&keccak256(b"")),
            "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
        assert_eq!(
            hex0x(&keccak256(b"hello")),
            "0x1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8"
        );
    }

    /// EIP-55's own examples: every one parses in each case, writes back in
    /// its checksum, and one letter's case changed is refused.
    #[test]
    fn eip55_checksums_are_the_specifications() {
        for s in [
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
            "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
        ] {
            let a = Address::parse(s).unwrap();
            assert_eq!(a.checksummed(), s);
            assert_eq!(Address::parse(&s.to_lowercase()).unwrap(), a);
            assert_eq!(
                Address::parse(&format!("0x{}", s[2..].to_uppercase())).unwrap(),
                a
            );
        }
        let bad = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAeD";
        assert!(Address::parse(bad).unwrap_err().contains("checksum"));
        // A server's address is read whatever its case: x402's own A2A
        // example miswrites Base USDC's checksum.
        let usdc = Address::parse_any("0x833589fCD6eDb6E08f4c7C32D4f71b54bda02913").unwrap();
        assert_eq!(
            usdc.checksummed(),
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
        );
        assert!(Address::parse_any("833589fCD6eDb6E08f4c7C32D4f71b54bda02913").is_err());
        for not in [
            "5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0x5aAeb6",
            "0xzz",
        ] {
            assert!(Address::parse(not).is_err(), "{not}");
        }
    }

    /// The well-known key 0x…01 is the generator itself, whose address
    /// every tool agrees on.
    #[test]
    fn the_key_one_is_the_generators_address() {
        let mut k = [0u8; 32];
        k[31] = 1;
        let sk = key_from_bytes(&k).unwrap();
        assert_eq!(
            address_of_key(&sk).checksummed(),
            "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
        );
        assert!(key_from_bytes(&[0u8; 32]).is_err(), "zero is no key");
        assert!(
            key_from_bytes(&[0xff; 32]).is_err(),
            "past the order is no key"
        );
    }

    /// Signing is RFC 6979 deterministic — the same key and digest sign the
    /// same 65 bytes, another digest others — low-S, `v` 27 or 28, and
    /// recovers to the signer's address; a high-S twin, a changed digest and
    /// a changed byte recover nothing or another address.
    #[test]
    fn signatures_are_deterministic_low_s_and_recover_to_the_signer() {
        let sk = key_from_bytes(&[0x11; 32]).unwrap();
        let me = address_of_key(&sk);
        let d = keccak256(b"agentd x402");
        let a = sign_digest(&sk, &d);
        assert_eq!(a, sign_digest(&sk, &d), "RFC 6979: the same signature");
        assert_ne!(a, sign_digest(&sk, &keccak256(b"other")));
        assert!(a[64] == 27 || a[64] == 28);
        assert_eq!(recover(&d, &a).unwrap(), me);
        // v as 0/1 is the same signature.
        let mut z = a;
        z[64] -= 27;
        assert_eq!(recover(&d, &z).unwrap(), me);
        // The high-S twin (n - s, the other parity) is refused (EIP-2).
        let sig = Signature::from_slice(&a[..64]).unwrap();
        let (r, s) = sig.split_scalars();
        let high = Signature::from_scalars(r.to_bytes(), (-*s).to_bytes()).unwrap();
        let mut h = [0u8; 65];
        h[..64].copy_from_slice(&high.to_bytes());
        h[64] = if a[64] == 27 { 28 } else { 27 };
        assert!(recover(&d, &h).unwrap_err().contains("upper half"));
        // Another digest recovers someone else.
        assert_ne!(recover(&keccak256(b"other"), &a).ok(), Some(me));
        let mut bad = a;
        bad[64] = 29;
        assert!(recover(&d, &bad).is_err());
        assert!(recover(&d, &a[..64]).is_err());
    }

    /// RFC 6979 draws `k` from the key and the digest alone, so a signature
    /// is fixed bytes: the secp256k1/SHA-256 vectors every implementation
    /// is checked against (key 1, "Satoshi Nakamoto" and the "tears in
    /// rain" line), and each recovers to the key's address.
    #[test]
    fn rfc6979_signatures_match_the_published_vectors() {
        let mut k = [0u8; 32];
        k[31] = 1;
        let sk = key_from_bytes(&k).unwrap();
        for (msg, r, s) in [
            (
                "Satoshi Nakamoto",
                "934b1ea10a4b3c1757e2b0c017d0b6143ce3c9a7e6a4a49860d7a6ab210ee3d8",
                "2442ce9d2b916064108014783e923ec36b49743e2ffa1c4496f01a512aafd9e5",
            ),
            (
                "All those moments will be lost in time, like tears in rain. Time to die...",
                "8600dbd41e348fe5c9465ab92d23e3db8b98b873beecd930736488696438cb6b",
                "547fe64427496db33bf66019dacbf0039c04199abb0122918601db38a72cfc21",
            ),
        ] {
            let d = crate::sha::sha256(msg.as_bytes());
            let sig = sign_digest(&sk, &d);
            assert_eq!(crate::sha::to_hex(&sig[..32]), r, "{msg}: r");
            assert_eq!(crate::sha::to_hex(&sig[32..64]), s, "{msg}: s");
            assert_eq!(recover(&d, &sig).unwrap(), address_of_key(&sk));
        }
    }

    /// A key's JWK names its address; a private member or another curve is
    /// refused.
    #[test]
    fn a_jwk_names_its_address() {
        let sk = key_from_bytes(&[0x22; 32]).unwrap();
        let jwk = jwk_of(sk.verifying_key());
        assert_eq!(address_of_jwk(&jwk).unwrap(), address_of_key(&sk));
        let mut private = jwk.clone();
        private["d"] = json!("AA");
        assert!(address_of_jwk(&private).is_err());
        let mut p256 = jwk.clone();
        p256["crv"] = json!("P-256");
        assert!(address_of_jwk(&p256).is_err());
        let mut off = jwk;
        off["y"] = json!(crate::b64::url_nopad(&[1u8; 32]));
        assert!(address_of_jwk(&off).is_err(), "a point off the curve");
    }
}
