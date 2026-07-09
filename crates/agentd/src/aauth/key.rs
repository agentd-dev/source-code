// SPDX-License-Identifier: Apache-2.0
//! The agent's Ed25519 identity key (RFC 0023 §keys) — generate, persist, load,
//! sign, and export the public JWK. Backed by `ring` (the same crypto provider
//! rustls already links; no new crate). The private key is a 32-byte seed,
//! stored base64url in a 0600 file; the file is the agent's durable identity.

use super::b64;
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::path::Path;

/// An Ed25519 signing key. Cheap to hold; `sign` is the hot path (every MCP
/// request). The seed never leaves this struct except back to its own file.
pub struct AgentKey {
    pair: Ed25519KeyPair,
    seed: [u8; 32],
}

impl AgentKey {
    /// Generate a fresh key from the system CSPRNG.
    pub fn generate() -> Result<AgentKey, String> {
        let rng = ring::rand::SystemRandom::new();
        // ring gives us a PKCS#8 doc; we keep the raw 32-byte seed (bytes 16..48
        // of the v2 PKCS#8 Ed25519 encoding) for a compact, portable key file.
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| "aauth: key generation failed".to_string())?;
        let doc = pkcs8.as_ref();
        // PKCS#8 v2 Ed25519: the 32-byte seed is the OCTET STRING at a fixed
        // offset; ring emits a stable layout. Re-derive from the seed so the
        // stored form and the in-memory pair are guaranteed consistent.
        let seed: [u8; 32] = doc
            .get(16..48)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| "aauth: unexpected pkcs8 layout".to_string())?;
        AgentKey::from_seed(&seed)
    }

    /// Build from a raw 32-byte seed.
    pub fn from_seed(seed: &[u8]) -> Result<AgentKey, String> {
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| "aauth: seed must be 32 bytes".to_string())?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|_| "aauth: invalid Ed25519 seed".to_string())?;
        Ok(AgentKey { pair, seed })
    }

    /// Load a key from `path`, or GENERATE + persist one if it does not exist —
    /// the "durable key, kept safely" of RFC 0023 §Step 0. The file is the
    /// base64url seed; created 0600.
    pub fn load_or_create(path: &Path) -> Result<AgentKey, String> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("aauth: read key {}: {e}", path.display()))?;
            let seed = b64::url_decode(text.trim())
                .map_err(|e| format!("aauth: key file {}: {e}", path.display()))?;
            return AgentKey::from_seed(&seed);
        }
        let key = AgentKey::generate()?;
        key.persist(path)?;
        Ok(key)
    }

    /// Write the seed (base64url) to `path` with 0600 perms (best-effort on
    /// non-unix). Refuses to overwrite an existing file.
    pub fn persist(&self, path: &Path) -> Result<(), String> {
        if path.exists() {
            return Err(format!("aauth: key file {} already exists", path.display()));
        }
        std::fs::write(path, b64::url_nopad(&self.seed))
            .map_err(|e| format!("aauth: write key {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// The 32-byte public key.
    pub fn public_bytes(&self) -> &[u8] {
        self.pair.public_key().as_ref()
    }

    /// The public key as an Ed25519 JWK (`{kty:OKP, crv:Ed25519, x:…}`), the
    /// form the Agent Provider enrolls (RFC 0023 §enroll) and the JWK thumbprint
    /// is computed from.
    pub fn public_jwk(&self) -> serde_json::Value {
        serde_json::json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "x": b64::url_nopad(self.public_bytes()),
        })
    }

    /// The RFC 7638 JWK thumbprint (SHA-256 over the canonical member set),
    /// base64url — the stable key id (`jkt`) an Agent Provider keys on.
    pub fn thumbprint(&self) -> String {
        // Canonical JWK (lexicographic keys, no whitespace) per RFC 7638.
        let canon = format!(
            r#"{{"crv":"Ed25519","kty":"OKP","x":"{}"}}"#,
            b64::url_nopad(self.public_bytes())
        );
        let digest = ring::digest::digest(&ring::digest::SHA256, canon.as_bytes());
        b64::url_nopad(digest.as_ref())
    }

    /// Sign `msg` (the RFC 9421 signature base) → the raw 64-byte signature.
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.pair.sign(msg).as_ref().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_persist_load_round_trips_and_signs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.key");
        let key = AgentKey::load_or_create(&path).expect("create");
        assert!(path.exists());
        let pub1 = key.public_bytes().to_vec();
        let tp1 = key.thumbprint();

        // Re-load: same identity.
        let key2 = AgentKey::load_or_create(&path).expect("reload");
        assert_eq!(key2.public_bytes(), &pub1[..]);
        assert_eq!(key2.thumbprint(), tp1);

        // A signature verifies against the public key (ring's own verifier).
        let msg = b"the signature base";
        let sig = key.sign(msg);
        assert_eq!(sig.len(), 64);
        let vk = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &pub1);
        vk.verify(msg, &sig).expect("valid signature");
        // A tampered message fails.
        assert!(vk.verify(b"other", &sig).is_err());
    }

    #[test]
    fn jwk_and_thumbprint_shapes() {
        let seed = [7u8; 32];
        let key = AgentKey::from_seed(&seed).unwrap();
        let jwk = key.public_jwk();
        assert_eq!(jwk["kty"], "OKP");
        assert_eq!(jwk["crv"], "Ed25519");
        assert!(jwk["x"].as_str().unwrap().len() >= 43); // 32 bytes → 43 b64url chars
        assert!(!key.thumbprint().is_empty());
    }
}
