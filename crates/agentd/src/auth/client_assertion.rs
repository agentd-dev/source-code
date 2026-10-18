// SPDX-License-Identifier: AGPL-3.0-only
//! `private_key_jwt` client authentication (RFC 7523 §2.2, OpenID Connect
//! Core §9): the client proves who it is at an authorization server by a
//! short-lived JWT signed with its own key, instead of sending a secret the
//! server could leak. ES256 on `ring`'s ECDSA P-256 — the crypto rustls
//! already links, so no new crate.
//!
//! The key is PKCS#8 (`-----BEGIN PRIVATE KEY-----`, or the DER it wraps).
//! Its `kid` is the RFC 7638 thumbprint of its public JWK unless the
//! operator names one. An assertion carries `iss` = `sub` = the `client_id`,
//! the `aud` the caller names, `iat`, an `exp` a minute later and a 128-bit
//! `jti` from the kernel CSPRNG, so every request — every retry included —
//! signs a fresh one.

use std::path::Path;

use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};

use crate::auth::jwk;
use crate::b64;

/// `client_assertion_type` for a JWT client assertion (RFC 7523 §2.2).
pub const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// How long an assertion is good for: a minute, which covers a slow
/// round-trip and some clock skew and no more (RFC 7523 §3 leaves it to the
/// client to keep it short).
pub const LIFETIME_S: u64 = 60;

/// An ES256 signing key and the public JWK and `kid` it is published under.
pub struct ClientKey {
    pair: EcdsaKeyPair,
    jwk: Value,
    kid: String,
}

impl ClientKey {
    /// A new P-256 key, as the PKCS#8 document a key file holds.
    pub fn generate_pkcs8() -> Result<Vec<u8>, String> {
        let rng = ring::rand::SystemRandom::new();
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map(|d| d.as_ref().to_vec())
            .map_err(|_| "client key: generation failed".to_string())
    }

    /// The key a PKCS#8 document holds; it must carry its public key, as
    /// `openssl genpkey` and [`Self::generate_pkcs8`] write one.
    pub fn from_pkcs8(der: &[u8]) -> Result<ClientKey, String> {
        let rng = ring::rand::SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der, &rng)
            .map_err(|e| format!("client key: not a PKCS#8 P-256 key ({e})"))?;
        ClientKey::from_pair(pair)
    }

    /// The key from its raw scalar and uncompressed public point — the form
    /// a JWK's `d`, `x` and `y` give, for a fixed key in a test.
    pub fn from_private_and_public(d: &[u8], point: &[u8]) -> Result<ClientKey, String> {
        let rng = ring::rand::SystemRandom::new();
        let pair = EcdsaKeyPair::from_private_key_and_public_key(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            d,
            point,
            &rng,
        )
        .map_err(|e| format!("client key: not a P-256 key pair ({e})"))?;
        ClientKey::from_pair(pair)
    }

    fn from_pair(pair: EcdsaKeyPair) -> Result<ClientKey, String> {
        let jwk = jwk::p256_public(pair.public_key().as_ref())?;
        let kid = jwk::thumbprint(&jwk)?;
        Ok(ClientKey { pair, jwk, kid })
    }

    /// The key in `text`: a PKCS#8 PEM block, or base64 of the DER.
    pub fn from_pem(text: &str) -> Result<ClientKey, String> {
        let t = text.trim();
        let body = match t.strip_prefix("-----BEGIN PRIVATE KEY-----") {
            Some(rest) => rest
                .split("-----END PRIVATE KEY-----")
                .next()
                .unwrap_or_default(),
            None if t.starts_with("-----BEGIN") => {
                return Err(
                    "client key: not a PKCS#8 `BEGIN PRIVATE KEY` block (convert an `EC PRIVATE KEY` with `openssl pkcs8 -topk8 -nocrypt`)"
                        .to_string(),
                );
            }
            None => t,
        };
        let b64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let der = b64::std_decode(&b64).map_err(|e| format!("client key: {e}"))?;
        ClientKey::from_pkcs8(&der)
    }

    /// The key in the PEM file at `path`, or a new one written there `0600`
    /// when there is no file: the file is the client's identity at every
    /// authorization server that registered its public key, so it outlives
    /// the process.
    pub fn load_or_create(path: &Path) -> Result<ClientKey, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => ClientKey::from_pem(&text).map_err(|e| format!("{e}: {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let der = ClientKey::generate_pkcs8()?;
                write_new(path, pem(&der).as_bytes())?;
                ClientKey::from_pkcs8(&der)
            }
            Err(e) => Err(format!("client key: read {}: {e}", path.display())),
        }
    }

    /// The same key published under `kid` instead of its thumbprint.
    pub fn with_kid(mut self, kid: &str) -> ClientKey {
        self.kid = kid.to_string();
        self
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The public JWK, with its `kid`, `alg` and `use`: what a JWKS lists.
    pub fn public_jwk(&self) -> Value {
        let mut j = self.jwk.clone();
        j["kid"] = json!(self.kid);
        j["alg"] = json!("ES256");
        j["use"] = json!("sig");
        j
    }

    /// The uncompressed public point, for a verifier.
    pub fn public_point(&self) -> &[u8] {
        self.pair.public_key().as_ref()
    }

    /// A compact JWS (RFC 7515) over `claims`: the header is `header` with
    /// `alg: ES256` and this key's `kid`.
    pub fn sign_jwt(&self, mut header: Value, claims: &Value) -> Result<String, String> {
        header["alg"] = json!("ES256");
        header["kid"] = json!(self.kid);
        let input = format!(
            "{}.{}",
            b64::url_nopad(header.to_string().as_bytes()),
            b64::url_nopad(claims.to_string().as_bytes())
        );
        let rng = ring::rand::SystemRandom::new();
        let sig = self
            .pair
            .sign(&rng, input.as_bytes())
            .map_err(|_| "client key: signing failed".to_string())?;
        Ok(format!("{input}.{}", b64::url_nopad(sig.as_ref())))
    }

    /// A `private_key_jwt` client assertion for `client_id` at the
    /// authorization server `aud` names.
    pub fn client_assertion(&self, client_id: &str, aud: &str) -> Result<String, String> {
        let iat = crate::auth::cache::now_ms() / 1000;
        let jti = crate::sec::random::b64url_token(16)
            .map_err(|e| format!("client assertion: no OS randomness for its jti: {e}"))?;
        self.sign_jwt(
            json!({"typ": "JWT"}),
            &json!({
                "iss": client_id,
                "sub": client_id,
                "aud": aud,
                "iat": iat,
                "exp": iat + LIFETIME_S,
                "jti": jti,
            }),
        )
    }
}

/// A PKCS#8 document as a PEM block.
pub fn pem(der: &[u8]) -> String {
    let b = b64::std_pad(der);
    let mut out = String::from("-----BEGIN PRIVATE KEY-----\n");
    for line in b.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END PRIVATE KEY-----\n");
    out
}

/// Create `path` with `bytes`, `0600` from the start, refusing a file that
/// appeared meanwhile.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    open.open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| format!("client key: write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    /// A fixed P-256 key: the scalar is SHA-256 of `agentd client-assertion
    /// test key` mod n, and the point and the thumbprint below were computed
    /// from it outside agentd (affine arithmetic over P-256, SHA-256 of the
    /// RFC 7638 member object).
    fn fixed() -> ClientKey {
        let d = b64::url_decode("SAsSZyGSTzXo9qwgRARVhyT8HXCBad22tDLRa6hauPs").unwrap();
        let mut point = vec![0x04];
        point.extend(b64::url_decode("oju-BcuS__N4lPlvFxju4TebIwPP9pimlotvlUKhidg").unwrap());
        point.extend(b64::url_decode("NpG7_i3sOo44bi051HAqB4NcIN6hPnoiM75uRcZbKB4").unwrap());
        ClientKey::from_private_and_public(&d, &point).unwrap()
    }

    fn part(jwt: &str, i: usize) -> Value {
        let seg = jwt.split('.').nth(i).unwrap();
        serde_json::from_slice(&b64::url_decode(seg).unwrap()).unwrap()
    }

    /// An assertion from a fixed key verifies with `ring`'s P-256 verifier
    /// over the public key alone, names the key by its RFC 7638 thumbprint,
    /// and carries the RFC 7523 claims.
    #[test]
    fn an_es256_assertion_verifies_and_names_its_key_by_thumbprint() {
        let key = fixed();
        assert_eq!(key.kid(), "wKKE6xkA6L25tJfgBaINQ_ezX5GyDZRaei-vXq-doPc");
        let jwt = key
            .client_assertion("https://agent.example/client.json", "https://as.example")
            .unwrap();
        let (input, sig) = jwt.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.public_point())
            .verify(input.as_bytes(), &b64::url_decode(sig).unwrap())
            .expect("the signature verifies");
        let mut tampered = input.to_string();
        tampered.push('x');
        assert!(
            UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.public_point())
                .verify(tampered.as_bytes(), &b64::url_decode(sig).unwrap())
                .is_err()
        );
        let header = part(&jwt, 0);
        assert_eq!(
            header,
            json!({"alg": "ES256", "typ": "JWT", "kid": key.kid()})
        );
        let claims = part(&jwt, 1);
        assert_eq!(claims["iss"], "https://agent.example/client.json");
        assert_eq!(claims["sub"], claims["iss"]);
        assert_eq!(
            claims["aud"], "https://as.example",
            "one string, not a list"
        );
        let iat = claims["iat"].as_u64().unwrap();
        assert_eq!(claims["exp"].as_u64().unwrap(), iat + LIFETIME_S);
        let jti = claims["jti"].as_str().unwrap();
        assert_eq!(b64::url_decode(jti).unwrap().len(), 16, "128 bits");
        // Every assertion is fresh.
        let again = key
            .client_assertion("https://agent.example/client.json", "https://as.example")
            .unwrap();
        assert_ne!(part(&again, 1)["jti"], claims["jti"]);
        // The JWKS entry: public members, the kid, nothing private.
        let pub_jwk = key.public_jwk();
        assert_eq!(pub_jwk["kid"], key.kid());
        assert!(pub_jwk.get("d").is_none());
        assert_eq!(key.with_kid("k-2026").kid(), "k-2026");
    }

    #[test]
    fn a_key_file_is_created_0600_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.key");
        let a = ClientKey::load_or_create(&path).unwrap();
        let b = ClientKey::load_or_create(&path).unwrap();
        assert_eq!(a.kid(), b.kid(), "the same identity after a restart");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("-----BEGIN PRIVATE KEY-----\n"), "{text}");
        assert_eq!(ClientKey::from_pem(&text).unwrap().kid(), a.kid());
        let e = ClientKey::from_pem(
            "-----BEGIN EC PRIVATE KEY-----\nAA==\n-----END EC PRIVATE KEY-----",
        )
        .err()
        .unwrap();
        assert!(e.contains("PKCS#8"), "{e}");
        assert!(ClientKey::from_pem("not a key").is_err());
    }
}
