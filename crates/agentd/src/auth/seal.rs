// SPDX-License-Identifier: AGPL-3.0-only
//! Sealing secrets at rest: AES-256-GCM with associated data, on
//! `ring::aead`. A sealed record is `nonce || ciphertext || tag`, the nonce
//! 96 random bits from the kernel CSPRNG. The associated data binds the
//! record to what it is for — a record kind, a client, an issuer, a
//! principal, a record version — so a record moved to another slot of the
//! store, or read as another kind, does not open.
//!
//! The primitive only: each record kind that is sealed names its own
//! associated data, built with [`associated_data`] so no two lists of parts
//! encode alike.

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};

/// The length of a seal key: 32 bytes.
pub const KEY_LEN: usize = 32;

/// An AES-256-GCM key for sealing and opening records.
pub struct SealKey(LessSafeKey);

impl SealKey {
    /// The key of 32 bytes; any other length is refused.
    pub fn new(bytes: &[u8]) -> Result<SealKey, String> {
        if bytes.len() != KEY_LEN {
            return Err(format!("seal key: {} bytes, not {KEY_LEN}", bytes.len()));
        }
        let key = UnboundKey::new(&AES_256_GCM, bytes)
            .map_err(|_| "seal key: refused by AES-256-GCM".to_string())?;
        Ok(SealKey(LessSafeKey::new(key)))
    }

    /// Seal `plaintext` under `ad`.
    pub fn seal(&self, ad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; NONCE_LEN];
        crate::sec::random::fill(&mut nonce)
            .map_err(|e| format!("seal: no OS randomness for a nonce: {e}"))?;
        let mut in_out = plaintext.to_vec();
        self.0
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(ad),
                &mut in_out,
            )
            .map_err(|_| "seal: AES-256-GCM failed".to_string())?;
        let mut out = Vec::with_capacity(NONCE_LEN + in_out.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&in_out);
        Ok(out)
    }

    /// Open a record [`Self::seal`] made under the same key and `ad`. Any
    /// other key, other associated data, or a changed byte is one refusal:
    /// the record does not say which.
    pub fn open(&self, ad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, String> {
        let refused =
            || "seal: the record does not open with this key and associated data".to_string();
        if sealed.len() < NONCE_LEN + AES_256_GCM.tag_len() {
            return Err(refused());
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| refused())?;
        let mut in_out = body.to_vec();
        let plain = self
            .0
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(ad),
                &mut in_out,
            )
            .map_err(|_| refused())?;
        Ok(plain.to_vec())
    }
}

/// Associated data from a list of parts, each length-prefixed (four bytes,
/// big-endian), so `("ab", "c")` and `("a", "bc")` differ.
pub fn associated_data(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(&(p.len() as u32).to_be_bytes());
        out.extend_from_slice(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ad(parts: &[&str]) -> Vec<u8> {
        let parts: Vec<&[u8]> = parts.iter().map(|p| p.as_bytes()).collect();
        associated_data(&parts)
    }

    #[test]
    fn a_record_opens_only_with_its_key_and_associated_data() {
        let key = SealKey::new(&[7u8; 32]).unwrap();
        let bound = ad(&[
            "company-account-token",
            "https://agent.example/c.json",
            "https://as.example",
            "operator",
            "1",
        ]);
        let sealed = key.seal(&bound, b"rt-secret").unwrap();
        assert!(
            !sealed.windows(9).any(|w| w == b"rt-secret"),
            "ciphertext only"
        );
        assert_eq!(key.open(&bound, &sealed).unwrap(), b"rt-secret");
        // Each seal draws its own nonce.
        assert_ne!(key.seal(&bound, b"rt-secret").unwrap(), sealed);

        // Another principal's slot, another kind, another key: refused.
        let other = ad(&[
            "company-account-token",
            "https://agent.example/c.json",
            "https://as.example",
            "user:sam",
            "1",
        ]);
        let e = key.open(&other, &sealed).unwrap_err();
        assert!(e.contains("does not open"), "{e}");
        assert!(key.open(&ad(&["company-user"]), &sealed).is_err());
        assert!(
            SealKey::new(&[8u8; 32])
                .unwrap()
                .open(&bound, &sealed)
                .is_err()
        );
        // A changed byte or a cut record: refused.
        let mut flipped = sealed.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(key.open(&bound, &flipped).is_err());
        assert!(key.open(&bound, &sealed[..20]).is_err());
        assert!(SealKey::new(&[0u8; 16]).is_err());
        // The encoding keeps part boundaries.
        assert_ne!(ad(&["ab", "c"]), ad(&["a", "bc"]));
    }
}
