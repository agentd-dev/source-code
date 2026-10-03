// SPDX-License-Identifier: AGPL-3.0-only
//! **Instruction attestation** — §7 of the Instruction Specification.
//!
//! A document that carries machinery is code; delivered over a network it is a
//! supply chain. The signatures establish authenticity and a capability
//! CEILING (not authorization, which is §7.7): JWS compact serializations
//! (RFC 7515) over a claims object, Ed25519 (`alg: EdDSA`, RFC 8037), digests
//! written `sha256:<hex>` (§7.2).
//!
//! Two signatures (§7.3): an offline **author** signature over the authored
//! digest, and an online **delivery** signature over the delivered bytes and
//! the resolution manifest (§7.4). Verifying them in the §7.6 order, and the
//! §7.8 hard floor, are the reference implementation's
//! (`instruction_core::sign`), re-exported here: a second copy of the verify
//! side is a second place for the claims and the checks to skew, and the mock
//! registry signing one shape while the runtime verified another is exactly
//! how they did. Every failure is a refusal carrying its Appendix B code,
//! never a downgrade to a weaker unsigned path.
//!
//! What stays here is agentd's: signing with an [`AgentKey`], verifying a
//! document's own signature against `agent.instruction.trust` pins, reading a
//! key file, and the §7.7 freshness helpers. The crypto is `ring` (Ed25519)
//! and base64url, reused from AAuth — the SAME `ring` rustls already
//! resolves, so this adds no new dependency.

use instruction_core::Refusal;

use crate::aauth::AgentKey;
use crate::aauth::b64;

/// The §7.2 digests, the claims, and §7.6 verification — the crate's, so the
/// bytes agentd signs and the checks it verifies with are one definition.
/// Re-exported rather than imported where used: agentd-cli's integration
/// tests reach the crate only through `agentd::`.
pub use instruction_core::sign::{
    Claims, SPEC_CLAIM, Verified, WIRE_FLOOR, admit_family, verify, verify_author, verify_delivery,
    verify_document,
};
pub use instruction_core::{author_digest, digest, front_matter_id};

/// The front-matter `signature:` line's value — the author JWS a document
/// carries INSIDE itself. This is what makes verification transport-
/// independent: the same signed bytes verify whether they arrived as a file, a
/// folder entry, an `https://` fetch or an OCI artifact, because the proof
/// travels with the document rather than beside it in a protocol's metadata.
pub fn front_matter_signature(doc: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(doc).ok()?;
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end]
        .lines()
        .find_map(|l| l.strip_prefix("signature:"))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// Read an Ed25519 public key from a FILE — raw 32 bytes, 64 hex chars, or
/// base64url. The registry path can also resolve a JWKS over MCP; this half is
/// what a file/dir/url/oci source can use, with no client and no network.
pub fn load_key_file(path: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() == 32 {
        return Some(bytes);
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        return (0..32)
            .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok())
            .collect();
    }
    crate::config::envelope::b64url_decode(&text).filter(|v| v.len() == 32)
}

/// What verifying a document against the pinned publishers concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Authorship {
    /// No pin carries a publisher — nothing was asked for.
    Unpinned,
    /// Pins exist, but none of them names key material this build can read
    /// without a registry client (their `author_keys` are all `instruction://`
    /// JWKS URIs). The caller applies `agent.instruction.unenforceable`.
    NoLocalKeys,
    /// The document's own signature verified against a pinned publisher.
    Verified {
        publisher: String,
        doc: String,
        /// The author-attested capabilities, already capped by the pin's
        /// `max_capabilities`. A signature CAPS what a document may activate;
        /// it never widens it (§7.6 step 5).
        capabilities: Vec<String>,
    },
}

/// Verify a document against the trust pins, using the signature it carries.
///
/// The rule is deliberately strict, because the alternative is a bypass: when
/// an operator has pinned a publisher, an UNSIGNED document — or one signed by
/// somebody else, or naming a `doc` id no pin covers — is refused. Matching on
/// the document's own id alone would let an attacker dodge every pin by
/// omitting a line.
///
/// Refusals carry their Appendix B code as the crate's do: a signature from a
/// publisher no pin names is `unpinned-publisher`, bytes the signature does
/// not cover are `digest-mismatch`, a refusal from the crate's [`verify`]
/// keeps its own, and the conditions without a row are `attestation`.
pub fn verify_authored(
    bytes: &[u8],
    pins: &[InstructionSource],
    now: u64,
) -> Result<Authorship, Refusal> {
    let pinned: Vec<&InstructionSource> = pins.iter().filter(|p| !p.publisher.is_empty()).collect();
    if pinned.is_empty() {
        return Ok(Authorship::Unpinned);
    }
    // Only FILE key material is usable here: resolving a JWKS needs the
    // registry client, which a file/oci/url load does not have.
    let keys: Vec<(&InstructionSource, Vec<u8>)> = pinned
        .iter()
        .flat_map(|p| {
            p.author_keys
                .iter()
                .filter_map(|k| load_key_file(k).map(|key| (*p, key)))
        })
        .collect();
    if keys.is_empty() {
        return Ok(Authorship::NoLocalKeys);
    }
    let jws = front_matter_signature(bytes).ok_or_else(|| {
        Refusal::new(
            "attestation",
            format!(
                "the instruction carries no front-matter `signature:`, but {} pinned \
                 publisher(s) are configured — an unsigned document is not from a pinned \
                 publisher (§7.6)",
                pinned.len()
            ),
        )
    })?;
    // The refusal of the last pin tried: every pin is a chance to verify, so
    // only when none does is the document refused, and with the last reason.
    let mut last = Refusal::new(
        "attestation",
        "no pinned key verifies this document's signature",
    );
    for (pin, key) in &keys {
        let claims = match verify_author(&jws, key) {
            Ok(c) => c,
            Err(e) => {
                last = e;
                continue;
            }
        };
        if claims.publisher != pin.publisher {
            last = Refusal::new(
                "unpinned-publisher",
                format!(
                    "the signature claims publisher {:?}, not the pinned {:?}",
                    claims.publisher, pin.publisher
                ),
            );
            continue;
        }
        if claims.exp < now {
            return Err(Refusal::new(
                "attestation",
                "the author signature has expired",
            ));
        }
        // The signature covers the AUTHORED digest — the document with its own
        // `signature:` line excluded, which is what lets it travel inside.
        let want = author_digest(bytes);
        if claims.digest != want {
            return Err(Refusal::new(
                "digest-mismatch",
                format!(
                    "the signature covers {} but these bytes author-hash to {want} — refuse",
                    claims.digest
                ),
            ));
        }
        let pin_doc = pin.uri.split('@').next().unwrap_or(&pin.uri);
        if !pin.uri.is_empty() && claims.doc != pin_doc {
            last = Refusal::new(
                "attestation",
                format!(
                    "the signature is for {:?}, not the pinned {pin_doc:?}",
                    claims.doc
                ),
            );
            continue;
        }
        let mut caps = claims.capabilities;
        if !pin.max_capabilities.is_empty() {
            caps.retain(|c| pin.max_capabilities.contains(c));
        }
        return Ok(Authorship::Verified {
            publisher: claims.publisher,
            doc: claims.doc,
            capabilities: caps,
        });
    }
    Err(last)
}

/// Sign a claims object into a JWS compact serialization (Ed25519/EdDSA). The
/// protected header carries `alg: EdDSA` and the claim's `typ`, so an author
/// signature can never be replayed as a delivery one or vice versa.
pub fn sign(key: &AgentKey, claims: &Claims) -> Result<String, String> {
    sign_kid(key, claims, None)
}

/// As [`sign`], naming the key in the protected header. A publisher that
/// rotates keys serves a key SET, so the header must say which member signed
/// — a verifier selects on the JWS's own `kid`, never on metadata beside it.
pub fn sign_kid(key: &AgentKey, claims: &Claims, kid: Option<&str>) -> Result<String, String> {
    let header = match kid {
        Some(k) => serde_json::json!({ "alg": "EdDSA", "typ": claims.typ, "kid": k }),
        None => serde_json::json!({ "alg": "EdDSA", "typ": claims.typ }),
    };
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

/// The resolution manifest (§7.4, S7) — the attested account of how the
/// delivered bytes were produced, values as digests. The reference
/// implementation's types, so the shape a delivery embeds is the one the
/// crate produces: strict, `variants.dropped` (§7.4 rule 5), `unresolved` and
/// both `limits` counts required, and a manifest without one refused.
pub use instruction_core::{Authored, Manifest, Variants};

/// One pinned instruction source in operator configuration (§7.5) — the very
/// type the config surface deserializes at `agent.instruction.trust`.
///
/// Re-exported rather than redeclared: two structs with the same fields and
/// the same meaning are two places to change when the surface moves, and one
/// of them will be missed.
pub use crate::config::settings::InstructionSource;

// ── §7.7 revocation: authorization is current membership, re-checked ─────────

/// A signed source's re-check interval in seconds (§7.7): its `freshness`,
/// parsed. `None` if the source declares none.
pub fn freshness_secs(src: &InstructionSource) -> Option<u64> {
    src.freshness
        .as_deref()
        .and_then(|s| crate::config::parse_duration(s).ok())
        .map(|d| d.as_secs())
}

/// Whether authorization is STALE — the deadline (`last_ok + freshness`) has
/// passed at `now` (§7.7 rule 2). Past it the runtime refuses NEW work and lets
/// live work run out (§5.5); a successful re-read resets `last_ok`. A failed or
/// unreachable read is staleness, never revocation.
pub fn is_stale(last_ok: u64, freshness_secs: u64, now: u64) -> bool {
    now.saturating_sub(last_ok) >= freshness_secs
}

/// The families whose class MUST re-check on the interval (§7.7 rule 1):
/// `compute` and `infra` — code and mounted state. Other classes SHOULD.
pub fn must_recheck(family: &str) -> bool {
    matches!(family, "compute" | "infra")
}

/// The families that LEFT the effective set between two reconciles — the
/// control plane revokes documents, and the runtime retracts the derived state
/// of anything that left (§7.7 rule 3, §5.5). A full reconcile at reconnect
/// honours offline revocations (§7.7 rule 4).
pub fn families_retracted(before: &[String], after: &[String]) -> Vec<String> {
    before
        .iter()
        .filter(|f| !after.contains(f))
        .cloned()
        .collect()
}

#[cfg(test)]
mod authored_tests {
    use super::*;
    use crate::config::settings::InstructionSource;

    fn key_and_doc(dir: &std::path::Path, body: &str, caps: &[&str]) -> (String, String) {
        // A real Ed25519 key pair, a real author JWS, a real document that
        // carries it — no mocks: the point is that the bytes verify.
        let key = AgentKey::generate().unwrap();
        let pub_path = dir.join("author.pub");
        std::fs::write(&pub_path, key.public_bytes()).unwrap();
        let doc_head = format!("---\nspec: \"1\"\nid: instruction://ins_1\n---\n{body}");
        let claims = Claims {
            spec: SPEC_CLAIM.into(),
            typ: "author".into(),
            doc: "instruction://ins_1".into(),
            version: "1".into(),
            digest: author_digest(doc_head.as_bytes()),
            capabilities: caps.iter().map(|c| (*c).to_string()).collect(),
            publisher: "https://pub.example".into(),
            iat: 1,
            exp: u64::MAX,
            aud: None,
            manifest: None,
            author: None,
        };
        let jws = sign(&key, &claims).unwrap();
        // …and now the document carries its own signature.
        let signed = doc_head.replacen("---\n{body}", "", 0);
        let signed = signed.replacen(
            "id: instruction://ins_1\n",
            &format!("id: instruction://ins_1\nsignature: {jws}\n"),
            1,
        );
        (pub_path.to_string_lossy().into_owned(), signed)
    }

    fn pin(key_file: &str, max: &[&str]) -> InstructionSource {
        InstructionSource {
            uri: "instruction://ins_1".into(),
            publisher: "https://pub.example".into(),
            author_keys: vec![key_file.to_string()],
            delivery_keys: vec![],
            reader: None,
            max_capabilities: max.iter().map(|c| (*c).to_string()).collect(),
            freshness: None,
        }
    }

    /// The signature travels INSIDE the document, so the same bytes verify
    /// whatever carried them — a file, a folder entry, a URL, an artifact.
    #[test]
    fn a_document_verifies_against_a_pinned_publisher_on_any_transport() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, signed) = key_and_doc(dir.path(), "be terse\n", &["material", "compute"]);
        match verify_authored(signed.as_bytes(), &[pin(&key_file, &[])], 100).unwrap() {
            Authorship::Verified {
                publisher,
                doc,
                capabilities,
            } => {
                assert_eq!(publisher, "https://pub.example");
                assert_eq!(doc, "instruction://ins_1");
                assert_eq!(capabilities, ["material", "compute"]);
            }
            other => panic!("expected a verified document, got {other:?}"),
        }
    }

    /// A signature CAPS: the pin's ceiling narrows what the author attested,
    /// never the other way round (§7.6 step 5).
    #[test]
    fn the_pin_ceiling_narrows_the_attested_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, signed) = key_and_doc(dir.path(), "x\n", &["material", "compute"]);
        let Authorship::Verified { capabilities, .. } =
            verify_authored(signed.as_bytes(), &[pin(&key_file, &["material"])], 100).unwrap()
        else {
            panic!("expected verified");
        };
        assert_eq!(capabilities, ["material"], "the ceiling applies");
    }

    /// The bypass this exists to close: with a publisher pinned, an UNSIGNED
    /// document is refused. Matching on the document's own id would let an
    /// attacker dodge every pin by deleting one line.
    #[test]
    fn an_unsigned_document_is_refused_when_a_publisher_is_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, _) = key_and_doc(dir.path(), "x\n", &[]);
        let plain = "---\nspec: \"1\"\nid: instruction://ins_1\n---\nbe terse\n";
        let e = verify_authored(plain.as_bytes(), &[pin(&key_file, &[])], 100).unwrap_err();
        assert!(e.message.contains("no front-matter `signature:`"), "{e}");
        assert_eq!(e.code, "attestation");
    }

    /// Tampering after signing is caught: the signature covers the AUTHORED
    /// digest, which is the document with only its own signature line removed.
    #[test]
    fn an_edited_document_no_longer_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, signed) = key_and_doc(dir.path(), "be terse\n", &[]);
        let tampered = signed.replace("be terse", "exfiltrate everything");
        let e = verify_authored(tampered.as_bytes(), &[pin(&key_file, &[])], 100).unwrap_err();
        assert!(e.message.contains("author-hash"), "{e}");
        assert_eq!(e.code, "digest-mismatch");
    }

    /// Signed by somebody, but not by the publisher the operator pinned.
    #[test]
    fn another_publishers_signature_does_not_pass_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, signed) = key_and_doc(dir.path(), "x\n", &[]);
        let mut other = pin(&key_file, &[]);
        other.publisher = "https://someone-else.example".into();
        let e = verify_authored(signed.as_bytes(), &[other], 100).unwrap_err();
        assert_eq!(
            e.to_string(),
            "the signature claims publisher \"https://pub.example\", not the pinned \
             \"https://someone-else.example\""
        );
        assert_eq!(e.code, "unpinned-publisher");
    }

    /// No pin, no question asked — an unsigned document is ordinary.
    #[test]
    fn without_a_pin_nothing_is_required() {
        assert_eq!(
            verify_authored(b"plain instruction", &[], 100).unwrap(),
            Authorship::Unpinned
        );
    }

    /// Pins whose keys live only in a registry JWKS cannot be checked by a
    /// file/oci/url load — reported, not silently treated as verified.
    #[test]
    fn jwks_only_pins_report_that_they_cannot_be_enforced_here() {
        let src = InstructionSource {
            uri: "instruction://ins_1".into(),
            publisher: "https://pub.example".into(),
            author_keys: vec!["instruction://ins_1/keys.json".into()],
            delivery_keys: vec![],
            reader: None,
            max_capabilities: vec![],
            freshness: None,
        };
        assert_eq!(
            verify_authored(b"anything", &[src], 100).unwrap(),
            Authorship::NoLocalKeys
        );
    }
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
            aud: None,
            manifest: None,
            author: None,
        }
    }

    fn manifest(dig: &str) -> Manifest {
        Manifest {
            authored: Authored {
                digest: dig.into(),
                version: Some("ver_01K003".into()),
            },
            ..Manifest::default()
        }
    }

    /// A full delivery attestation embedding the manifest + the author JWS.
    fn delivery_claims(dig: &str, caps: &[&str], author_jws: &str) -> Claims {
        Claims {
            aud: Some("principal://usr_7".into()),
            manifest: Some(manifest(dig)),
            author: Some(author_jws.into()),
            exp: 1_757_003_600,
            ..claims("delivery", dig, caps)
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
        assert!(e.message.contains("domain separation"), "{e}");
        assert_eq!(e.code, "signature-typ-mismatch");
    }

    /// The delivery manifest is the S7 shape: one that omits
    /// `variants.dropped` (§7.4 rule 5) or `unresolved` is not a manifest,
    /// so claims embedding it do not parse and the delivery is refused.
    #[test]
    fn the_manifest_requires_dropped_and_unresolved() {
        let ok = serde_json::to_value(manifest("sha256:aa")).unwrap();
        assert!(serde_json::from_value::<Manifest>(ok.clone()).is_ok());
        let mut no_dropped = ok.clone();
        no_dropped["variants"]
            .as_object_mut()
            .unwrap()
            .remove("dropped");
        let mut no_unresolved = ok;
        no_unresolved.as_object_mut().unwrap().remove("unresolved");
        for bad in [no_dropped, no_unresolved] {
            let e = serde_json::from_value::<Manifest>(bad).unwrap_err();
            assert!(e.to_string().contains("missing field"), "{e}");
        }
    }

    fn src() -> InstructionSource {
        InstructionSource {
            uri: "instruction://ins_42".into(),
            publisher: "https://instruction.md/pub/acme".into(),
            author_keys: vec![],
            delivery_keys: vec![],
            reader: None,
            max_capabilities: vec!["material".into()],
            freshness: None,
        }
    }

    #[test]
    fn a_signature_caps_it_never_grants() {
        let dig = digest(b"doc");
        let author = key();
        let delivery = AgentKey::from_seed(&[8u8; 32]).unwrap();
        let a_jws = sign(&author, &claims("author", &dig, &["material", "compute"])).unwrap();
        // The delivery ceiling ⊆ author; it embeds the manifest + author JWS.
        let d_jws = sign(&delivery, &delivery_claims(&dig, &["material"], &a_jws)).unwrap();
        let v = verify_document(
            b"doc",
            &d_jws,
            "principal://usr_7",
            1_757_000_100,
            &["material".into(), "compute".into()],
            &src().publisher,
            &src().max_capabilities,
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
    fn a_delivery_ceiling_exceeding_the_author_is_refused() {
        let dig = digest(b"doc");
        let author = key();
        let delivery = AgentKey::from_seed(&[8u8; 32]).unwrap();
        // Author attests only material; delivery claims compute too.
        let a_jws = sign(&author, &claims("author", &dig, &["material"])).unwrap();
        let d_jws = sign(
            &delivery,
            &delivery_claims(&dig, &["material", "compute"], &a_jws),
        )
        .unwrap();
        let e = verify_document(
            b"doc",
            &d_jws,
            "principal://usr_7",
            1_757_000_100,
            &["material".into(), "compute".into()],
            &src().publisher,
            &src().max_capabilities,
            author.public_bytes(),
            delivery.public_bytes(),
        )
        .unwrap_err();
        assert!(e.message.contains("exceed the author attestation"), "{e}");
        assert_eq!(e.code, "delivery-ceiling-exceeded");
    }

    #[test]
    fn a_digest_mismatch_is_refused() {
        let author = key();
        let delivery = key();
        let a_jws = sign(&author, &claims("author", "sha256:deadbeef", &["material"])).unwrap();
        // The delivery claim covers a digest that is not the bytes'.
        let d_jws = sign(
            &delivery,
            &delivery_claims("sha256:deadbeef", &["material"], &a_jws),
        )
        .unwrap();
        let e = verify_document(
            b"the real bytes",
            &d_jws,
            "principal://usr_7",
            1_757_000_100,
            &["material".into()],
            &src().publisher,
            &src().max_capabilities,
            author.public_bytes(),
            delivery.public_bytes(),
        )
        .unwrap_err();
        assert!(e.message.contains("delivered bytes hash to"), "{e}");
        assert_eq!(e.code, "digest-mismatch");
    }

    #[test]
    fn the_delivery_must_be_addressed_to_this_reader() {
        let dig = digest(b"doc");
        let author = key();
        let delivery = AgentKey::from_seed(&[8u8; 32]).unwrap();
        let a_jws = sign(&author, &claims("author", &dig, &["material"])).unwrap();
        let d_jws = sign(&delivery, &delivery_claims(&dig, &["material"], &a_jws)).unwrap();
        // aud is principal://usr_7; a different reader is refused.
        let e = verify_document(
            b"doc",
            &d_jws,
            "principal://someone_else",
            1_757_000_100,
            &["material".into()],
            &src().publisher,
            &src().max_capabilities,
            author.public_bytes(),
            delivery.public_bytes(),
        )
        .unwrap_err();
        assert!(e.message.contains("not this reader"), "{e}");
        assert_eq!(e.code, "audience-mismatch");
    }

    #[test]
    fn the_author_digest_excludes_the_front_matter_signature_line() {
        let unsigned = "---\nspec: \"1\"\nid: instruction://ins_x\n---\nbody\n";
        let signed =
            "---\nspec: \"1\"\nid: instruction://ins_x\nsignature: eyJ.abc.def\n---\nbody\n";
        // The signed document hashes the same as the unsigned one — the JWS can
        // travel inside its own front matter.
        assert_eq!(
            author_digest(signed.as_bytes()),
            author_digest(unsigned.as_bytes())
        );
        assert_eq!(
            front_matter_id(signed.as_bytes()).as_deref(),
            Some("instruction://ins_x")
        );
    }

    #[test]
    fn freshness_revocation_logic() {
        let src = InstructionSource {
            uri: "u".into(),
            publisher: "p".into(),
            author_keys: vec![],
            delivery_keys: vec![],
            reader: None,
            max_capabilities: vec![],
            freshness: Some("15m".into()),
        };
        assert_eq!(freshness_secs(&src), Some(900));
        // Not yet due, then due at the deadline.
        assert!(!is_stale(1000, 900, 1000 + 899));
        assert!(is_stale(1000, 900, 1000 + 900));
        // compute/infra MUST re-check; others SHOULD.
        assert!(must_recheck("compute") && must_recheck("infra"));
        assert!(!must_recheck("material"));
        // A family that left the effective set is retracted.
        assert_eq!(
            families_retracted(&["compute".into(), "material".into()], &["material".into()]),
            vec!["compute".to_string()]
        );
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
