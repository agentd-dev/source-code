// SPDX-License-Identifier: AGPL-3.0-only
//! **§7 verification** — digests and JWS/Ed25519 attestation checks, feature
//! `sign` (`ring`).
//!
//! This is the VERIFY side the platform needs at publish and resolve time:
//! `sha256:<hex>` digests (§7.2), the author digest that excludes a
//! front-matter `signature:` line so a JWS can travel inside its own
//! document, and author/delivery claim verification in the §7.6 order — a
//! signature CAPS capability, never grants it, and every failure is a
//! refusal, never a downgrade. Signing (an author key held in memory) is a
//! deployment concern and lives with the consumer; agentd's
//! `config::attest` module is the daemon-side sibling of this code and
//! collapses onto it in a follow-up.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::Manifest;

/// The version claim every attestation carries (§7.2).
pub const SPEC_CLAIM: &str = "instruction/1";
/// Families never admissible in a document that arrived over the wire (§7.8).
pub const WIRE_FLOOR: &[&str] = &["compose", "identity"];

/// `sha256:<hex>` of some bytes (§7.2).
pub fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut s = String::with_capacity(7 + 64);
    s.push_str("sha256:");
    for b in d.as_ref() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The author digest (§7.2): the stored bytes with the front-matter
/// `signature:` line excluded, so the author JWS can ride in its own document.
pub fn author_digest(doc: &[u8]) -> String {
    digest(&strip_front_matter_signature(doc))
}

fn strip_front_matter_signature(doc: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(doc) else {
        return doc.to_vec();
    };
    let Some(rest) = text.strip_prefix("---\n") else {
        return doc.to_vec();
    };
    let Some(end) = rest.find("\n---") else {
        return doc.to_vec();
    };
    let (front, body) = rest.split_at(end);
    let kept: Vec<&str> = front
        .split('\n')
        .filter(|l| !l.starts_with("signature:"))
        .collect();
    format!("---\n{}{}", kept.join("\n"), body).into_bytes()
}

/// The front-matter `id`, which an attestation's `doc` claim must equal (§3.1).
pub fn front_matter_id(doc: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(doc).ok()?;
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end].split('\n').find_map(|l| {
        l.strip_prefix("id:")
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

/// An attestation's claims (§7.2). The three delivery-only fields (`aud`,
/// `manifest`, `author`) are absent on an author attestation and REQUIRED on
/// a delivery one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    pub spec: String,
    pub typ: String,
    pub doc: String,
    pub version: String,
    pub digest: String,
    pub capabilities: Vec<String>,
    #[serde(rename = "pub")]
    pub publisher: String,
    pub iat: u64,
    pub exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// Verify a compact JWS against an Ed25519 public key, requiring `want_typ` in
/// the CLAIM (domain separation, §7.2). Every failure is a refusal.
pub fn verify(jws: &str, public_key: &[u8], want_typ: &str) -> Result<Claims, String> {
    let mut it = jws.split('.');
    let (h, p, s) = match (it.next(), it.next(), it.next(), it.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => return Err("attestation: not a compact JWS (want three dot-separated parts)".into()),
    };
    let sig = b64url_decode(s).ok_or("attestation: bad signature base64")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| "attestation: signature does not verify against the pinned key".to_string())?;
    let header: Value = serde_json::from_slice(&b64url_decode(h).ok_or("bad header b64")?)
        .map_err(|e| e.to_string())?;
    if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
        return Err("attestation: alg must be EdDSA (§7.2)".into());
    }
    let claims: Claims = serde_json::from_slice(&b64url_decode(p).ok_or("bad claims b64")?)
        .map_err(|e| format!("attestation: malformed claims: {e}"))?;
    if claims.spec != SPEC_CLAIM {
        return Err(format!(
            "attestation: spec claim is {:?}, this reader implements {SPEC_CLAIM:?}",
            claims.spec
        ));
    }
    if claims.typ != want_typ {
        return Err(format!(
            "attestation: typ is {:?} but a {want_typ:?} signature was required — \
             domain separation (§7.2)",
            claims.typ
        ));
    }
    Ok(claims)
}

/// Verify an AUTHOR attestation.
pub fn verify_author(jws: &str, public_key: &[u8]) -> Result<Claims, String> {
    verify(jws, public_key, "author")
}

/// Verify a DELIVERY attestation.
pub fn verify_delivery(jws: &str, public_key: &[u8]) -> Result<Claims, String> {
    verify(jws, public_key, "delivery")
}

/// The verified outcome: the effective ceiling and both claim sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub effective: Vec<String>,
    pub manifest: Manifest,
    pub author: Claims,
    pub delivery: Claims,
}

/// Verify a delivered, signed document in the §7.6 order (steps 2–6). The
/// caller supplies the pinned trust — `publisher` and `max_capabilities` —
/// rather than a runtime config type, so any consumer can call this.
#[allow(clippy::too_many_arguments)]
pub fn verify_document(
    bytes: &[u8],
    delivery_jws: &str,
    reader: &str,
    now: u64,
    grant: &[String],
    publisher: &str,
    max_capabilities: &[String],
    author_pub: &[u8],
    delivery_pub: &[u8],
) -> Result<Verified, String> {
    let delivery = verify_delivery(delivery_jws, delivery_pub)?;
    if delivery.aud.as_deref() != Some(reader) {
        return Err(format!(
            "attestation: this delivery is for {:?}, not this reader {reader:?} (§7.6 step 2)",
            delivery.aud.as_deref().unwrap_or("<none>")
        ));
    }
    if delivery.exp < now {
        return Err("attestation: the delivery signature has expired (§7.6 step 2)".into());
    }
    let got = digest(bytes);
    if delivery.digest != got {
        return Err(format!(
            "attestation: the delivered bytes hash to {got}, but the delivery signature \
             covers {} — refuse (§7.6 step 2)",
            delivery.digest
        ));
    }
    if let Some(id) = front_matter_id(bytes)
        && id != delivery.doc
    {
        return Err(format!(
            "attestation: doc {:?} does not equal the document's front-matter id {id:?} (§3.1)",
            delivery.doc
        ));
    }
    let manifest = delivery
        .manifest
        .clone()
        .ok_or("attestation: the delivery claims carry no manifest (§7.3)")?;
    let author_jws = delivery
        .author
        .clone()
        .ok_or("attestation: the delivery claims carry no author signature (§7.3)")?;
    let author = verify_author(&author_jws, author_pub)?;
    if author.exp < now {
        return Err("attestation: the author signature has expired (§7.6 step 3)".into());
    }
    if author.doc != delivery.doc || author.version != delivery.version {
        return Err(
            "attestation: the author and delivery attestations name different documents \
             (§7.6 step 3)"
                .into(),
        );
    }
    if author.digest != manifest.authored.digest {
        return Err(format!(
            "attestation: the author signature covers {} but the manifest's authored.digest \
             is {} — the chain is broken (§7.3)",
            author.digest, manifest.authored.digest
        ));
    }
    if author.publisher != publisher {
        return Err(format!(
            "attestation: the author claims publisher {:?}, not the pinned {publisher:?} — \
             refuse (§7.6 step 3)",
            author.publisher
        ));
    }
    if let Some(over) = delivery
        .capabilities
        .iter()
        .find(|c| !author.capabilities.contains(c))
    {
        return Err(format!(
            "delivery: capabilities [{over}] exceed the author attestation {:?}",
            author.capabilities
        ));
    }
    let effective: Vec<String> = grant
        .iter()
        .filter(|c| {
            max_capabilities.contains(c)
                && author.capabilities.contains(c)
                && delivery.capabilities.contains(c)
        })
        .cloned()
        .collect();
    Ok(Verified {
        effective,
        manifest,
        author,
        delivery,
    })
}

/// Admit one block family under the effective ceiling + the §7.8 wire floor.
pub fn admit_family(family: &str, effective: &[String], over_the_wire: bool) -> Result<(), String> {
    if over_the_wire && WIRE_FLOOR.contains(&family) {
        return Err(format!(
            "family {family:?} is never admissible in a document that arrived over the wire \
             (§7.8 hard floor) — operator surface only"
        ));
    }
    if !effective.iter().any(|e| e == family) {
        return Err(format!(
            "family {family:?} exceeds the attested and granted ceiling {effective:?} — the \
             document is refused whole (§7.6 step 5)"
        ));
    }
    Ok(())
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        bits = bits << 6 | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Authored, Variants};

    fn b64url_encode(b: &[u8]) -> String {
        const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in b.chunks(3) {
            let n = (chunk[0] as u32) << 16
                | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
                | chunk.get(2).copied().unwrap_or(0) as u32;
            out.push(URL[(n >> 18 & 63) as usize] as char);
            out.push(URL[(n >> 12 & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(URL[(n >> 6 & 63) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(URL[(n & 63) as usize] as char);
            }
        }
        out
    }

    fn sign_with(seed: &[u8; 32], claims: &Claims) -> String {
        let kp = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed).unwrap();
        let header = serde_json::json!({"alg": "EdDSA", "typ": claims.typ});
        let h = b64url_encode(header.to_string().as_bytes());
        let p = b64url_encode(serde_json::to_string(claims).unwrap().as_bytes());
        let input = format!("{h}.{p}");
        format!(
            "{input}.{}",
            b64url_encode(kp.sign(input.as_bytes()).as_ref())
        )
    }

    fn pubkey(seed: &[u8; 32]) -> Vec<u8> {
        use ring::signature::KeyPair;
        ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
            .unwrap()
            .public_key()
            .as_ref()
            .to_vec()
    }

    #[test]
    fn the_full_verification_chain_holds_and_caps() {
        let doc = b"---\nspec: \"1\"\nid: instruction://ins_x\n---\nbody\n";
        let dig = digest(doc);
        let author_claims = Claims {
            spec: SPEC_CLAIM.into(),
            typ: "author".into(),
            doc: "instruction://ins_x".into(),
            version: "v1".into(),
            digest: author_digest(doc),
            capabilities: vec!["material".into(), "compute".into()],
            publisher: "https://pub.example".into(),
            iat: 1,
            exp: u64::MAX,
            aud: None,
            manifest: None,
            author: None,
        };
        let a_jws = sign_with(&[1; 32], &author_claims);
        let manifest = Manifest {
            authored: Authored {
                version: "v1".into(),
                digest: author_digest(doc),
            },
            variants: Variants::default(),
            ..Manifest::default()
        };
        let delivery_claims = Claims {
            typ: "delivery".into(),
            digest: dig,
            capabilities: vec!["material".into()],
            exp: 1000,
            aud: Some("agent://a1".into()),
            manifest: Some(manifest),
            author: Some(a_jws),
            ..author_claims.clone()
        };
        let d_jws = sign_with(&[2; 32], &delivery_claims);
        let v = verify_document(
            doc,
            &d_jws,
            "agent://a1",
            500,
            &["material".into(), "compute".into()],
            "https://pub.example",
            &["material".into()],
            &pubkey(&[1; 32]),
            &pubkey(&[2; 32]),
        )
        .unwrap();
        // grant ∩ max ∩ author ∩ delivery = {material}: a signature caps.
        assert_eq!(v.effective, vec!["material".to_string()]);
        assert!(
            admit_family("compose", &v.effective, true).is_err(),
            "wire floor"
        );
        // The wrong audience refuses.
        let e = verify_document(
            doc,
            &d_jws,
            "agent://someone-else",
            500,
            &["material".into()],
            "https://pub.example",
            &["material".into()],
            &pubkey(&[1; 32]),
            &pubkey(&[2; 32]),
        )
        .unwrap_err();
        assert!(e.contains("not this reader"), "{e}");
        // The author digest excludes a signature: line.
        let signed = b"---\nspec: \"1\"\nid: instruction://ins_x\nsignature: a.b.c\n---\nbody\n";
        assert_eq!(author_digest(signed), author_digest(doc));
    }
}
