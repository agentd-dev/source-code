// SPDX-License-Identifier: AGPL-3.0-only
//! **Instruction attestation** — §7 of the Instruction Specification.
//!
//! A document that carries machinery is code; delivered over a network it is a
//! supply chain. This module implements the signatures that establish
//! authenticity and a capability CEILING (not authorization, which is §7.7):
//! JWS compact serializations (RFC 7515) over a claims object, Ed25519
//! (`alg: EdDSA`, RFC 8037), digests written `sha256:<hex>` (§7.2).
//!
//! Two signatures (§7.3): an offline **author** signature over the authored
//! digest, and an online **delivery** signature over the delivered bytes and
//! the resolution manifest (§7.4). The verification order (§7.6) and the hard
//! floor (§7.8) are enforced here; every failure is a refusal, never a
//! downgrade to a weaker unsigned path.
//!
//! The crypto is `ring` (Ed25519, SHA-256) and base64url, reused from AAuth —
//! the SAME `ring` rustls already resolves, so this adds no new dependency.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::aauth::b64;
use crate::aauth::{AgentKey, verify_ed25519};

/// The version claim every attestation carries (§7.2).
pub const SPEC_CLAIM: &str = "instruction/1";

/// Families never admissible in a document that arrived over the wire (§7.8),
/// signed or not — operator surface only.
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

/// An attestation's claims (§7.2). `typ` domain-separates an author signature
/// from a delivery one.
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
}

/// Sign a claims object into a JWS compact serialization (Ed25519/EdDSA). The
/// protected header carries `alg: EdDSA` and the claim's `typ`, so an author
/// signature can never be replayed as a delivery one or vice versa.
pub fn sign(key: &AgentKey, claims: &Claims) -> Result<String, String> {
    let header = serde_json::json!({ "alg": "EdDSA", "typ": claims.typ });
    let h = b64::url_nopad(
        serde_json::to_string(&header)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    );
    let p = b64::url_nopad(
        serde_json::to_string(claims)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    );
    let signing_input = format!("{h}.{p}");
    let sig = key.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", b64::url_nopad(&sig)))
}

/// Verify a JWS compact serialization against an Ed25519 public key, returning
/// the claims. Checks `alg: EdDSA`, the spec version, and that `typ` is the one
/// expected; every failure is a refusal (§7.6: refuse, never degrade).
pub fn verify(jws: &str, public_key: &[u8], want_typ: &str) -> Result<Claims, String> {
    let mut it = jws.split('.');
    let (h, p, s) = match (it.next(), it.next(), it.next(), it.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => {
            return Err("attestation: not a compact JWS (want three dot-separated parts)".into());
        }
    };
    let sig = b64::url_decode(s)?;
    verify_ed25519(public_key, format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| "attestation: signature does not verify against the pinned key".to_string())?;
    let header: Value = serde_json::from_slice(&b64::url_decode(h)?).map_err(|e| e.to_string())?;
    if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
        return Err("attestation: alg must be EdDSA (§7.2)".into());
    }
    let claims: Claims = serde_json::from_slice(&b64::url_decode(p)?)
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

/// The resolution manifest (§7.4) — the attested account of how the delivered
/// bytes were produced. Values appear as digests, not values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub authored: Authored,
    #[serde(default)]
    pub parameters: Vec<Value>,
    #[serde(default)]
    pub facts: Vec<Value>,
    pub variants: Variants,
    #[serde(default)]
    pub includes: Vec<Value>,
    #[serde(default)]
    pub limits: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Authored {
    pub version: String,
    pub digest: String,
}

/// The `when` variants kept and dropped for this reader. `dropped` is REQUIRED
/// (§7.4 rule 5): a reader must be able to tell that content was withheld, or
/// `when` is indistinguishable from censorship by a compromised resolver — so
/// there is deliberately no default, and a manifest that omits it is refused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Variants {
    #[serde(default)]
    pub kept: Vec<String>,
    pub dropped: Vec<String>,
}

/// Parse a resolution manifest from YAML, enforcing §7.4's shape (notably the
/// required `variants.dropped`).
pub fn parse_manifest(yaml: &str) -> Result<Manifest, String> {
    let v = crate::config::yaml::parse(yaml).map_err(|e| format!("manifest: invalid YAML: {e}"))?;
    serde_json::from_value(v).map_err(|e| {
        if e.to_string().contains("dropped") {
            "manifest: variants.dropped is REQUIRED (§7.4 rule 5) — a reader must be able to \
             tell content was withheld"
                .to_string()
        } else {
            format!("manifest: {e}")
        }
    })
}

/// One pinned instruction source in operator configuration (§7.5). Pinning is
/// by key and publisher, never by URI. This is operator surface and MUST be
/// unreachable from `!config`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct InstructionSource {
    pub uri: String,
    pub publisher: String,
    #[serde(default)]
    pub author_keys: Vec<String>,
    #[serde(default)]
    pub delivery_keys: Vec<String>,
    #[serde(default)]
    pub max_capabilities: Vec<String>,
    #[serde(default)]
    pub freshness: Option<String>,
}

/// The outcome of verifying a signed, delivered document (§7.6): the effective
/// capability ceiling, and the verified claims + manifest for audit.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub effective: Vec<String>,
    pub manifest: Manifest,
    pub author: Claims,
    pub delivery: Claims,
}

/// Verify a delivered, signed document in the order §7.6 mandates (steps 2–6;
/// the caller has already done step 1, the front-matter version check, and does
/// steps 7–8, revocation freshness and the trifecta re-computation, against the
/// running process). Failure at any step is a refusal.
///
/// `bytes` is the delivered document; `delivery_jws`/`author_jws` its two
/// signatures; `manifest_yaml` the manifest the delivery signature covers;
/// `grant` the operator's `document_capabilities`; `src` the pinned trust
/// config; `author_pub`/`delivery_pub` the pinned public keys.
#[allow(clippy::too_many_arguments)]
pub fn verify_document(
    bytes: &[u8],
    delivery_jws: &str,
    author_jws: &str,
    manifest_yaml: &str,
    grant: &[String],
    src: &InstructionSource,
    author_pub: &[u8],
    delivery_pub: &[u8],
) -> Result<Verified, String> {
    // 2. Verify the delivery signature over the received bytes; recompute and
    //    compare the digest. A mismatch means the bytes are not what was signed.
    let delivery = verify(delivery_jws, delivery_pub, "delivery")?;
    let got = digest(bytes);
    if delivery.digest != got {
        return Err(format!(
            "attestation: the delivered bytes hash to {got}, but the delivery signature \
             covers {} — refuse (§7.6 step 2)",
            delivery.digest
        ));
    }
    // 3. Extract the manifest; verify the author signature over authored.digest
    //    against a pinned key for the CLAIMED publisher. An unpinned publisher
    //    is a refusal, not a weaker trust level.
    let manifest = parse_manifest(manifest_yaml)?;
    let author = verify(author_jws, author_pub, "author")?;
    if author.digest != manifest.authored.digest {
        return Err(format!(
            "attestation: the author signature covers {} but the manifest's authored.digest \
             is {} — the chain is broken (§7.3)",
            author.digest, manifest.authored.digest
        ));
    }
    if author.publisher != src.publisher {
        return Err(format!(
            "attestation: the author claims publisher {:?}, not the pinned {:?} — refuse \
             (§7.6 step 3)",
            author.publisher, src.publisher
        ));
    }
    // 4. Effective families = grant ∩ max_capabilities ∩ author ∩ delivery.
    //    A signature CAPS; it never grants (§7.2).
    let effective = intersect(&[
        grant,
        &src.max_capabilities,
        &author.capabilities,
        &delivery.capabilities,
    ]);
    Ok(Verified {
        effective,
        manifest,
        author,
        delivery,
    })
}

/// Admit (or refuse) one block's family under the effective ceiling and the
/// §7.8 hard floor — §7.6 step 5 (any block exceeding effective ⇒ refuse whole)
/// and step 6 (the floor). `over_the_wire` is true for a delivered document.
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

/// The intersection of several capability lists, preserving the first list's
/// order. An empty list contributes nothing but the empty set.
fn intersect(lists: &[&[String]]) -> Vec<String> {
    let Some((first, rest)) = lists.split_first() else {
        return Vec::new();
    };
    first
        .iter()
        .filter(|c| rest.iter().all(|l| l.contains(c)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AgentKey {
        AgentKey::from_seed(&[7u8; 32]).unwrap()
    }

    fn claims(typ: &str, dig: &str, caps: &[&str]) -> Claims {
        Claims {
            spec: SPEC_CLAIM.into(),
            typ: typ.into(),
            doc: "instruction://ins_42".into(),
            version: "ver_01K003".into(),
            digest: dig.into(),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            publisher: "https://instruction.md/pub/acme".into(),
            iat: 1_757_000_000,
            exp: 1_788_536_000,
        }
    }

    #[test]
    fn digest_is_sha256_hex() {
        // Known vector: SHA-256("") = e3b0c442...
        assert_eq!(
            digest(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn an_author_signature_round_trips() {
        let k = key();
        let c = claims("author", &digest(b"the document"), &["material", "compute"]);
        let jws = sign(&k, &c).unwrap();
        let back = verify(&jws, k.public_bytes(), "author").unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn a_tampered_signature_is_refused() {
        let k = key();
        let jws = sign(&k, &claims("author", &digest(b"x"), &["material"])).unwrap();
        // Flip a byte in the payload segment.
        let mut parts: Vec<String> = jws.split('.').map(String::from).collect();
        parts[1].push('A');
        let tampered = parts.join(".");
        assert!(verify(&tampered, k.public_bytes(), "author").is_err());
        // A different key does not verify.
        let other = AgentKey::from_seed(&[9u8; 32]).unwrap();
        assert!(verify(&jws, other.public_bytes(), "author").is_err());
    }

    #[test]
    fn a_delivery_signature_cannot_stand_in_for_an_author_one() {
        let k = key();
        let jws = sign(&k, &claims("delivery", &digest(b"x"), &["material"])).unwrap();
        let e = verify(&jws, k.public_bytes(), "author").unwrap_err();
        assert!(e.contains("domain separation"), "{e}");
    }

    #[test]
    fn the_manifest_requires_variants_dropped() {
        // dropped present → ok.
        let ok = "authored: { version: v1, digest: \"sha256:aa\" }\nvariants: { kept: [], dropped: [when#1] }";
        assert!(parse_manifest(ok).is_ok());
        // dropped absent → refused, naming the rule.
        let bad = "authored: { version: v1, digest: \"sha256:aa\" }\nvariants: { kept: [] }";
        let e = parse_manifest(bad).unwrap_err();
        assert!(e.contains("variants.dropped is REQUIRED"), "{e}");
    }

    #[test]
    fn a_signature_caps_it_never_grants() {
        let dig = digest(b"doc");
        let author = key();
        let delivery = AgentKey::from_seed(&[8u8; 32]).unwrap();
        let a_jws = sign(&author, &claims("author", &dig, &["material", "compute"])).unwrap();
        let manifest = format!(
            "authored: {{ version: ver_01K003, digest: \"{dig}\" }}\nvariants: {{ kept: [], dropped: [] }}"
        );
        let d_jws = sign(
            &delivery,
            &claims("delivery", &dig, &["material", "compute"]),
        )
        .unwrap();
        let src = InstructionSource {
            uri: "instruction://ins_42".into(),
            publisher: "https://instruction.md/pub/acme".into(),
            author_keys: vec![],
            delivery_keys: vec![],
            // The operator's per-source ceiling grants only material.
            max_capabilities: vec!["material".into()],
            freshness: None,
        };
        let v = verify_document(
            b"doc",
            &d_jws,
            &a_jws,
            &manifest,
            &["material".into(), "compute".into()],
            &src,
            author.public_bytes(),
            delivery.public_bytes(),
        )
        .unwrap();
        // Effective = grant ∩ max_capabilities ∩ author ∩ delivery = {material}.
        assert_eq!(v.effective, vec!["material".to_string()]);
        // compute exceeds the ceiling → the document is refused whole.
        assert!(admit_family("compute", &v.effective, true).is_err());
        assert!(admit_family("material", &v.effective, true).is_ok());
    }

    #[test]
    fn a_digest_mismatch_is_refused() {
        let delivery = key();
        // The delivery claim covers a digest that is not the bytes'.
        let d_jws = sign(
            &delivery,
            &claims("delivery", "sha256:deadbeef", &["material"]),
        )
        .unwrap();
        let e = verify_document(
            b"the real bytes",
            &d_jws,
            "unused.unused.unused",
            "authored: { version: v, digest: x }\nvariants: { dropped: [] }",
            &["material".into()],
            &InstructionSource {
                uri: "u".into(),
                publisher: "p".into(),
                author_keys: vec![],
                delivery_keys: vec![],
                max_capabilities: vec![],
                freshness: None,
            },
            delivery.public_bytes(),
            delivery.public_bytes(),
        )
        .unwrap_err();
        assert!(e.contains("delivered bytes hash to"), "{e}");
    }

    #[test]
    fn the_hard_floor_refuses_compose_and_identity_over_the_wire() {
        // Even with the family in the effective set, the wire floor refuses it.
        let eff = vec![
            "compose".to_string(),
            "identity".to_string(),
            "material".to_string(),
        ];
        assert!(admit_family("compose", &eff, true).is_err());
        assert!(admit_family("identity", &eff, true).is_err());
        assert!(admit_family("material", &eff, true).is_ok());
        // Operator surface (not over the wire): the floor does not apply.
        assert!(admit_family("compose", &eff, false).is_ok());
    }
}
