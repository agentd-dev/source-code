// SPDX-License-Identifier: AGPL-3.0-only
//! DPoP (RFC 9449): a P-256 key made in supervisor memory and written
//! nowhere, and a fresh proof for every request it signs — at the last
//! moment, once the method and URL are final. The Personal Agent Protocol
//! draft (RFC 0047, §4.3) binds a User's Session at each Company to one;
//! a delegated MCP server whose authorization server and protected-resource
//! metadata advertise DPoP (RFC 0044 T23) binds the access token it is
//! called with to another.
//!
//! A proof is a JWT with the public key in its header (`typ: dpop+jwt`,
//! `alg: ES256`, `jwk`) and the claims `jti` (128 random bits), `htm`,
//! `htu` (the URL without query or fragment), `iat`, `ath` over an access
//! token sent with it, and `nonce` when the server asked for one. A proof
//! to a token endpoint never has `ath` (RFC 9449 §4.3).
//!
//! Nonces are kept per origin: learned from any `DPoP-Nonce` a response
//! carries, and used in every proof to that origin from then on.

use std::collections::BTreeMap;
use std::sync::Mutex;

use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};

use crate::auth::jwk;
use crate::b64;

/// A proof's JOSE `typ` (RFC 9449 §4.2).
pub const TYP: &str = "dpop+jwt";
/// The one algorithm agentd signs proofs with.
pub const ALG: &str = "ES256";

/// A DPoP key: the pair, its public JWK and that JWK's RFC 7638 thumbprint.
pub struct DpopKey {
    pair: EcdsaKeyPair,
    jwk: Value,
    jkt: String,
}

impl DpopKey {
    /// A new key, held only in this process.
    pub fn generate() -> Result<DpopKey, String> {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| "dpop: key generation failed".to_string())?;
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .map_err(|_| "dpop: the generated key does not load".to_string())?;
        let jwk = jwk::p256_public(pair.public_key().as_ref())?;
        let jkt = jwk::thumbprint(&jwk)?;
        Ok(DpopKey { pair, jwk, jkt })
    }

    /// The RFC 7638 thumbprint a token bound to this key is bound to.
    pub fn thumbprint(&self) -> &str {
        &self.jkt
    }

    /// A proof for `method` at `url`, with `ath` over `token` when one is
    /// sent with it and the origin's `nonce` when there is one.
    pub fn proof(
        &self,
        method: &str,
        url: &str,
        token: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<String, String> {
        let jti = crate::sec::random::b64url_token(16)
            .map_err(|e| format!("dpop: no OS randomness for a jti: {e}"))?;
        let mut claims = json!({
            "jti": jti,
            "htm": method.to_ascii_uppercase(),
            "htu": htu(url)?,
            "iat": crate::auth::cache::now_ms() / 1000,
        });
        if let Some(t) = token {
            claims["ath"] = json!(b64::url_nopad(&crate::sha::sha256(t.as_bytes())));
        }
        if let Some(n) = nonce {
            claims["nonce"] = json!(n);
        }
        let header = json!({"typ": TYP, "alg": ALG, "jwk": self.jwk});
        let input = format!(
            "{}.{}",
            b64::url_nopad(header.to_string().as_bytes()),
            b64::url_nopad(claims.to_string().as_bytes())
        );
        let sig = self
            .pair
            .sign(&ring::rand::SystemRandom::new(), input.as_bytes())
            .map_err(|_| "dpop: signing failed".to_string())?;
        Ok(format!("{input}.{}", b64::url_nopad(sig.as_ref())))
    }
}

/// A proof's `htu`: the scheme, the authority as the `Host` header names it
/// and the path — no query, no fragment (§4.3).
pub fn htu(url: &str) -> Result<String, String> {
    let u = crate::net::http::Url::parse(url)?;
    let path = u.path.split(['?', '#']).next().unwrap_or("/");
    Ok(format!("{}://{}{path}", u.scheme, u.host_header()))
}

/// The nonces servers issued, by origin.
#[derive(Default)]
pub struct Nonces(Mutex<BTreeMap<String, String>>);

impl Nonces {
    /// The nonce to put in a proof to `url`'s origin, when one was issued.
    pub fn get(&self, url: &str) -> Option<String> {
        let origin = crate::config::settings::companies::origin(url)?.to_ascii_lowercase();
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&origin)
            .cloned()
    }

    /// Keep `nonce`, which a response from `url`'s origin carried.
    pub fn learn(&self, url: &str, nonce: &str) {
        if let Some(origin) = crate::config::settings::companies::origin(url) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(origin.to_ascii_lowercase(), nonce.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    fn parts(proof: &str) -> (Value, Value, String, Vec<u8>) {
        let mut it = proof.split('.');
        let (h, c, s) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let json = |seg: &str| serde_json::from_slice(&b64::url_decode(seg).unwrap()).unwrap();
        (
            json(h),
            json(c),
            format!("{h}.{c}"),
            b64::url_decode(s).unwrap(),
        )
    }

    /// A proof is §4.3's: the header's public key verifies it, `htm` is
    /// uppercase, `htu` drops the query and fragment and keeps a non-default
    /// port, `ath` is the token's hash, and every proof has its own `jti`.
    #[test]
    fn a_proof_carries_what_the_draft_lists_and_verifies_with_its_key() {
        let key = DpopKey::generate().unwrap();
        let p = key
            .proof(
                "post",
                "https://API.example:8443/orders/1?x=1#f",
                Some("tok"),
                Some("n1"),
            )
            .unwrap();
        let (h, c, input, sig) = parts(&p);
        assert_eq!(h["typ"], "dpop+jwt");
        assert_eq!(h["alg"], "ES256");
        assert!(h["jwk"].get("d").is_none(), "public members only");
        assert_eq!(jwk::thumbprint(&h["jwk"]).unwrap(), key.thumbprint());
        let mut point = vec![0x04];
        point.extend(b64::url_decode(h["jwk"]["x"].as_str().unwrap()).unwrap());
        point.extend(b64::url_decode(h["jwk"]["y"].as_str().unwrap()).unwrap());
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
            .verify(input.as_bytes(), &sig)
            .expect("the proof verifies with the key in its header");
        assert_eq!(c["htm"], "POST");
        assert_eq!(c["htu"], "https://API.example:8443/orders/1");
        assert_eq!(c["nonce"], "n1");
        assert_eq!(
            c["ath"],
            b64::url_nopad(&crate::sha::sha256(b"tok")),
            "ath is the token's SHA-256"
        );
        assert!(c["jti"].as_str().unwrap().len() >= 22, "128 bits");
        let again = parts(&key.proof("GET", "https://a.example/", None, None).unwrap()).1;
        assert_ne!(again["jti"], c["jti"], "a fresh jti for every proof");
        assert!(again.get("ath").is_none() && again.get("nonce").is_none());
        assert_eq!(again["htu"], "https://a.example/");
    }

    #[test]
    fn a_nonce_is_kept_for_its_origin() {
        let n = Nonces::default();
        n.learn("https://auth.example/token", "abc");
        assert_eq!(n.get("https://AUTH.example/other").as_deref(), Some("abc"));
        assert_eq!(n.get("https://auth.example:8443/token"), None);
        assert_eq!(n.get("https://mcp.example/"), None);
    }
}
