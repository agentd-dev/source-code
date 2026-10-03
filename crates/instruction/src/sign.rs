// SPDX-License-Identifier: AGPL-3.0-only
//! **§7 verification** — JWS/Ed25519 attestation checks, feature `sign`.
//!
//! This is the VERIFY side the platform needs at publish and resolve time:
//! author/delivery claim verification in the §7.6 order, over the §7.2
//! digests ([`crate::digest()`], [`crate::author_digest`]) every build has — a
//! signature CAPS capability, never grants it, and every failure is a
//! refusal, never a downgrade. Signing (an author key held in memory) is a
//! deployment concern and lives with the consumer; agentd's
//! `config::attest` signs with its own key type and re-exports everything
//! here, and its registry read verifies a delivery with [`verify_document`].
//!
//! Every refusal is a [`Refusal`] with no line, carrying the code Appendix B
//! gives its condition (§1.4). The §7 conditions Appendix B has no row for —
//! a JWS that does not parse or verify, an expired signature, a broken chain —
//! carry the non-catalogue code `attestation`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Manifest;
use crate::Refusal;
use crate::digest::{digest, front_matter_id};

/// The version claim every attestation carries (§7.2). It also versions the
/// signed form of the manifest a delivery embeds (S7 §3): that form carries
/// no discriminator of its own, so the revision the signer followed is the
/// one this claim names, and a change to the shape or its canonical bytes
/// must bring a discriminator inside the manifest.
pub const SPEC_CLAIM: &str = "instruction/1";
/// Families never admissible in a document that arrived over the wire (§7.8),
/// signed or not — operator surface only.
pub const WIRE_FLOOR: &[&str] = &["compose", "identity"];

/// An attestation's claims (§7.2). `typ` domain-separates an author signature
/// from a delivery one. The three delivery-only fields (`aud`, `manifest`,
/// `author`) are absent on an author attestation and REQUIRED on a delivery
/// one: a delivery signature covers the delivered bytes, the audience, the
/// resolution manifest, and the author signature it was resolved from (§7.3).
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
    /// Delivery only: the reader, as a `principal://` or `agent://` URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    /// Delivery only: the §7.4 manifest, embedded so the signature covers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
    /// Delivery only: the author signature's JWS compact serialization, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// A §7 refusal Appendix B has no row for. One constructor, so the code is
/// written once and every such site reads as what it is.
fn attestation(message: impl Into<String>) -> Refusal {
    Refusal::new("attestation", message)
}

/// Verify a compact JWS against an Ed25519 public key, requiring `want_typ` in
/// the CLAIM (domain separation, §7.2). Checks `alg: EdDSA` and the spec
/// version; every failure is a refusal (§7.6: refuse, never degrade).
pub fn verify(jws: &str, public_key: &[u8], want_typ: &str) -> Result<Claims, Refusal> {
    let mut it = jws.split('.');
    let (h, p, s) = match (it.next(), it.next(), it.next(), it.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => {
            return Err(attestation(
                "attestation: not a compact JWS (want three dot-separated parts)",
            ));
        }
    };
    let sig = b64url_decode(s).ok_or_else(|| attestation("attestation: bad signature base64"))?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| {
            attestation("attestation: signature does not verify against the pinned key")
        })?;
    let header: Value = serde_json::from_slice(
        &b64url_decode(h).ok_or_else(|| attestation("attestation: bad header base64"))?,
    )
    .map_err(|e| attestation(format!("attestation: malformed header: {e}")))?;
    if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
        return Err(attestation("attestation: alg must be EdDSA (§7.2)"));
    }
    let payload = b64url_decode(p).ok_or_else(|| attestation("attestation: bad claims base64"))?;
    let malformed =
        |e: serde_json::Error| attestation(format!("attestation: malformed claims: {e}"));
    // A manifest without `variants.dropped` is a condition of its own
    // (§7.4 rule 5, `manifest-dropped-missing`). The typed claims would only
    // say "missing field" from inside serde, so the claims are read loosely
    // first to name it.
    let loose: Value = serde_json::from_slice(&payload).map_err(malformed)?;
    if loose.get("manifest").is_some_and(Value::is_object)
        && loose
            .pointer("/manifest/variants/dropped")
            .is_none_or(Value::is_null)
    {
        return Err(Refusal::new(
            "manifest-dropped-missing",
            "manifest: variants.dropped is required (§7.4 rule 5)",
        ));
    }
    // From the bytes again rather than from `loose`: serde's error then names
    // the line and column, as it always has.
    let claims: Claims = serde_json::from_slice(&payload).map_err(malformed)?;
    if claims.spec != SPEC_CLAIM {
        return Err(attestation(format!(
            "attestation: spec claim is {:?}, this reader implements {SPEC_CLAIM:?}",
            claims.spec
        )));
    }
    if claims.typ != want_typ {
        return Err(Refusal::new(
            "signature-typ-mismatch",
            format!(
                "attestation: typ is {:?} but a {want_typ:?} signature was required — \
                 domain separation (§7.2)",
                claims.typ
            ),
        ));
    }
    Ok(claims)
}

/// Verify an AUTHOR attestation.
pub fn verify_author(jws: &str, public_key: &[u8]) -> Result<Claims, Refusal> {
    verify(jws, public_key, "author")
}

/// Verify a DELIVERY attestation.
pub fn verify_delivery(jws: &str, public_key: &[u8]) -> Result<Claims, Refusal> {
    verify(jws, public_key, "delivery")
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
/// the caller has already done step 1, the front-matter version check, and
/// does steps 7–8, revocation freshness and the trifecta re-computation,
/// against the running process). Failure at any step is a refusal.
///
/// `bytes` is the delivered document as received; `delivery_jws` its delivery
/// signature (which EMBEDS the manifest and the author JWS — §7.3); `reader`
/// this reader's `principal://`/`agent://` audience; `now` unix seconds for
/// the `exp` checks; `grant` the operator's `document_capabilities`. The
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
) -> Result<Verified, Refusal> {
    // 2. Verify the delivery signature; check typ, audience, expiry, and that
    //    the delivered bytes hash to the delivery `digest`.
    let delivery = verify_delivery(delivery_jws, delivery_pub)?;
    if delivery.aud.as_deref() != Some(reader) {
        return Err(Refusal::new(
            "audience-mismatch",
            format!(
                "attestation: this delivery is for {:?}, not this reader {reader:?} (§7.6 step 2)",
                delivery.aud.as_deref().unwrap_or("<none>")
            ),
        ));
    }
    if delivery.exp < now {
        return Err(attestation(
            "attestation: the delivery signature has expired (§7.6 step 2)",
        ));
    }
    let got = digest(bytes);
    if delivery.digest != got {
        return Err(Refusal::new(
            "digest-mismatch",
            format!(
                "attestation: the delivered bytes hash to {got}, but the delivery signature \
                 covers {} — refuse (§7.6 step 2)",
                delivery.digest
            ),
        ));
    }
    // The `doc` claim MUST equal the delivered document's front-matter `id`.
    if let Some(id) = front_matter_id(bytes)
        && id != delivery.doc
    {
        return Err(attestation(format!(
            "attestation: doc {:?} does not equal the document's front-matter id {id:?} (§3.1)",
            delivery.doc
        )));
    }
    // 3. Take the manifest and the author JWS FROM the delivery claims; verify
    //    the author signature over authored.digest against the pinned key for
    //    the claimed publisher. An unpinned publisher is a refusal.
    let manifest = delivery
        .manifest
        .clone()
        .ok_or_else(|| attestation("attestation: the delivery claims carry no manifest (§7.3)"))?;
    let author_jws = delivery.author.clone().ok_or_else(|| {
        attestation("attestation: the delivery claims carry no author signature (§7.3)")
    })?;
    let author = verify_author(&author_jws, author_pub)?;
    if author.exp < now {
        return Err(attestation(
            "attestation: the author signature has expired (§7.6 step 3)",
        ));
    }
    if author.doc != delivery.doc || author.version != delivery.version {
        return Err(attestation(
            "attestation: the author and delivery attestations name different documents \
             (§7.6 step 3)",
        ));
    }
    if author.digest != manifest.authored.digest {
        return Err(attestation(format!(
            "attestation: the author signature covers {} but the manifest's authored.digest \
             is {} — the chain is broken (§7.3)",
            author.digest, manifest.authored.digest
        )));
    }
    if author.publisher != publisher {
        return Err(Refusal::new(
            "unpinned-publisher",
            format!(
                "attestation: the author claims publisher {:?}, not the pinned {publisher:?} — \
                 refuse (§7.6 step 3)",
                author.publisher
            ),
        ));
    }
    // The delivery ceiling MUST be a subset of the author ceiling (§7.2).
    if let Some(over) = delivery
        .capabilities
        .iter()
        .find(|c| !author.capabilities.contains(c))
    {
        return Err(Refusal::new(
            "delivery-ceiling-exceeded",
            format!(
                "delivery: capabilities [{over}] exceed the author attestation {:?}",
                author.capabilities
            ),
        ));
    }
    // 4. Effective families = grant ∩ max_capabilities ∩ author ∩ delivery,
    //    in the grant's order. A signature CAPS; it never grants (§7.2).
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

/// Admit (or refuse) one block's family under the effective ceiling and the
/// §7.8 hard floor — §7.6 step 5 (any block exceeding effective ⇒ refuse
/// whole) and step 6 (the floor). `over_the_wire` is true for a delivered
/// document.
pub fn admit_family(
    family: &str,
    effective: &[String],
    over_the_wire: bool,
) -> Result<(), Refusal> {
    if over_the_wire && WIRE_FLOOR.contains(&family) {
        return Err(Refusal::new(
            "wire-floor",
            format!(
                "family {family:?} is never admissible in a document that arrived over the wire \
                 (§7.8 hard floor) — operator surface only"
            ),
        ));
    }
    if !effective.iter().any(|e| e == family) {
        return Err(Refusal::new(
            "ungranted-family",
            format!(
                "family {family:?} exceeds the attested and granted ceiling {effective:?} — the \
                 document is refused whole (§7.6 step 5)"
            ),
        ));
    }
    Ok(())
}

/// A JWS segment's bytes: base64url WITHOUT padding (RFC 7515 §2), and
/// only that — no standard-alphabet `+` or `/`, no `=`, no length no
/// encoding produces, no nonzero bits left over. Exactly one spelling per
/// byte string, so a signature segment cannot be re-spelled (a `=` and
/// anything after it, an alphabet swapped) and still verify.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 {
        return None;
    }
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        bits = bits << 6 | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    // The bits past the last whole byte are padding a canonical encoder
    // writes as zero.
    (bits & ((1 << nbits) - 1) == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Authored, author_digest};

    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    fn b64url_encode(b: &[u8]) -> String {
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

    /// Sign any claims JSON — not only the typed [`Claims`], so a test can
    /// hand the verifier a shape the typed claims could never serialize.
    fn sign_value(seed: &[u8; 32], claims: &Value) -> String {
        let kp = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed).unwrap();
        let header = serde_json::json!({"alg": "EdDSA", "typ": claims["typ"]});
        let h = b64url_encode(header.to_string().as_bytes());
        let p = b64url_encode(claims.to_string().as_bytes());
        let input = format!("{h}.{p}");
        format!(
            "{input}.{}",
            b64url_encode(kp.sign(input.as_bytes()).as_ref())
        )
    }

    fn sign_with(seed: &[u8; 32], claims: &Claims) -> String {
        sign_value(seed, &serde_json::to_value(claims).unwrap())
    }

    fn pubkey(seed: &[u8; 32]) -> Vec<u8> {
        use ring::signature::KeyPair;
        ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
            .unwrap()
            .public_key()
            .as_ref()
            .to_vec()
    }

    const DOC: &[u8] = b"---\nspec: \"1\"\nid: instruction://ins_x\n---\nbody\n";
    const AUTHOR: [u8; 32] = [1; 32];
    const DELIVERY: [u8; 32] = [2; 32];
    const PUBLISHER: &str = "https://pub.example";
    const READER: &str = "agent://a1";

    fn author_claims(caps: &[&str]) -> Claims {
        Claims {
            spec: SPEC_CLAIM.into(),
            typ: "author".into(),
            doc: "instruction://ins_x".into(),
            version: "v1".into(),
            digest: author_digest(DOC),
            capabilities: caps.iter().map(|c| (*c).to_string()).collect(),
            publisher: PUBLISHER.into(),
            iat: 1,
            exp: u64::MAX,
            aud: None,
            manifest: None,
            author: None,
        }
    }

    /// A delivery of [`DOC`] to [`READER`], embedding an author signature
    /// over `author_caps` and the manifest that chains to it.
    fn delivery_claims(author_caps: &[&str], caps: &[&str]) -> Claims {
        let manifest = Manifest {
            authored: Authored {
                digest: author_digest(DOC),
                version: Some("v1".into()),
            },
            ..Manifest::default()
        };
        Claims {
            typ: "delivery".into(),
            digest: digest(DOC),
            capabilities: caps.iter().map(|c| (*c).to_string()).collect(),
            exp: 1000,
            aud: Some(READER.into()),
            manifest: Some(manifest),
            author: Some(sign_with(&AUTHOR, &author_claims(author_caps))),
            ..author_claims(author_caps)
        }
    }

    /// `verify_document` with the fixture's trust: the pinned publisher, a
    /// `material` ceiling, and a `material` + `compute` grant.
    fn verify_fixture(bytes: &[u8], d_jws: &str, reader: &str) -> Result<Verified, Refusal> {
        verify_document(
            bytes,
            d_jws,
            reader,
            500,
            &["material".into(), "compute".into()],
            PUBLISHER,
            &["material".into()],
            &pubkey(&AUTHOR),
            &pubkey(&DELIVERY),
        )
    }

    /// A refusal's code, its line (none: §7 refuses the document whole) and
    /// its Display, which must stay the text operators have always read.
    fn assert_refusal(r: &Refusal, code: &str, text: &str) {
        assert_eq!(r.code, code, "{r}");
        assert_eq!(r.line, None, "{r}");
        assert_eq!(r.to_string(), text);
    }

    #[test]
    fn the_full_verification_chain_holds_and_caps() {
        let d_jws = sign_with(
            &DELIVERY,
            &delivery_claims(&["material", "compute"], &["material"]),
        );
        let v = verify_fixture(DOC, &d_jws, READER).unwrap();
        // grant ∩ max ∩ author ∩ delivery = {material}: a signature caps.
        assert_eq!(v.effective, vec!["material".to_string()]);
        assert!(admit_family("material", &v.effective, true).is_ok());
        // The author digest excludes a signature: line.
        let signed = b"---\nspec: \"1\"\nid: instruction://ins_x\nsignature: a.b.c\n---\nbody\n";
        assert_eq!(author_digest(signed), author_digest(DOC));
    }

    #[test]
    fn a_signature_of_the_other_typ_is_signature_typ_mismatch() {
        let d_jws = sign_with(&DELIVERY, &delivery_claims(&["material"], &["material"]));
        let e = verify_author(&d_jws, &pubkey(&DELIVERY)).unwrap_err();
        assert_refusal(
            &e,
            "signature-typ-mismatch",
            "attestation: typ is \"delivery\" but a \"author\" signature was required — \
             domain separation (§7.2)",
        );
    }

    #[test]
    fn a_delivery_for_another_reader_is_audience_mismatch() {
        let d_jws = sign_with(&DELIVERY, &delivery_claims(&["material"], &["material"]));
        let e = verify_fixture(DOC, &d_jws, "agent://someone-else").unwrap_err();
        assert_refusal(
            &e,
            "audience-mismatch",
            "attestation: this delivery is for \"agent://a1\", not this reader \
             \"agent://someone-else\" (§7.6 step 2)",
        );
    }

    #[test]
    fn bytes_the_delivery_does_not_cover_are_digest_mismatch() {
        let d_jws = sign_with(&DELIVERY, &delivery_claims(&["material"], &["material"]));
        let other = b"---\nspec: \"1\"\nid: instruction://ins_x\n---\nanother body\n";
        let e = verify_fixture(other, &d_jws, READER).unwrap_err();
        assert_refusal(
            &e,
            "digest-mismatch",
            &format!(
                "attestation: the delivered bytes hash to {}, but the delivery signature \
                 covers {} — refuse (§7.6 step 2)",
                digest(other),
                digest(DOC)
            ),
        );
    }

    /// A manifest without `variants.dropped` names its own condition rather
    /// than failing inside serde as malformed claims — with no `dropped`,
    /// with a null one, and with no `variants` at all.
    #[test]
    fn a_manifest_without_dropped_is_manifest_dropped_missing() {
        let good = serde_json::to_value(delivery_claims(&["material"], &["material"])).unwrap();
        let mut no_dropped = good.clone();
        no_dropped["manifest"]["variants"]
            .as_object_mut()
            .unwrap()
            .remove("dropped");
        let mut null_dropped = good.clone();
        null_dropped["manifest"]["variants"]["dropped"] = Value::Null;
        let mut no_variants = good;
        no_variants["manifest"]
            .as_object_mut()
            .unwrap()
            .remove("variants");
        for bad in [no_dropped, null_dropped, no_variants] {
            let e = verify_fixture(DOC, &sign_value(&DELIVERY, &bad), READER).unwrap_err();
            assert_refusal(
                &e,
                "manifest-dropped-missing",
                "manifest: variants.dropped is required (§7.4 rule 5)",
            );
        }
    }

    #[test]
    fn an_author_from_another_publisher_is_unpinned_publisher() {
        let d_jws = sign_with(&DELIVERY, &delivery_claims(&["material"], &["material"]));
        let e = verify_document(
            DOC,
            &d_jws,
            READER,
            500,
            &["material".into()],
            "https://someone-else.example",
            &["material".into()],
            &pubkey(&AUTHOR),
            &pubkey(&DELIVERY),
        )
        .unwrap_err();
        assert_refusal(
            &e,
            "unpinned-publisher",
            "attestation: the author claims publisher \"https://pub.example\", not the pinned \
             \"https://someone-else.example\" — refuse (§7.6 step 3)",
        );
    }

    #[test]
    fn a_delivery_wider_than_its_author_is_delivery_ceiling_exceeded() {
        let d_jws = sign_with(
            &DELIVERY,
            &delivery_claims(&["material"], &["material", "compute"]),
        );
        let e = verify_fixture(DOC, &d_jws, READER).unwrap_err();
        assert_refusal(
            &e,
            "delivery-ceiling-exceeded",
            "delivery: capabilities [compute] exceed the author attestation [\"material\"]",
        );
    }

    #[test]
    fn a_floor_family_over_the_wire_is_wire_floor() {
        // In the effective set, and refused anyway: the floor is not a grant.
        let eff = vec!["compose".to_string(), "identity".to_string()];
        assert_refusal(
            &admit_family("compose", &eff, true).unwrap_err(),
            "wire-floor",
            "family \"compose\" is never admissible in a document that arrived over the wire \
             (§7.8 hard floor) — operator surface only",
        );
        assert_eq!(
            admit_family("identity", &eff, true).unwrap_err().code,
            "wire-floor"
        );
        // The operator surface is not the wire.
        assert!(admit_family("compose", &eff, false).is_ok());
    }

    #[test]
    fn a_family_outside_the_ceiling_is_ungranted_family() {
        assert_refusal(
            &admit_family("compute", &["material".into()], false).unwrap_err(),
            "ungranted-family",
            "family \"compute\" exceeds the attested and granted ceiling [\"material\"] — the \
             document is refused whole (§7.6 step 5)",
        );
    }

    /// The §7 conditions Appendix B has no row for carry `attestation`.
    #[test]
    fn a_condition_without_an_appendix_b_row_is_attestation() {
        assert_refusal(
            &verify("a.b", &pubkey(&AUTHOR), "author").unwrap_err(),
            "attestation",
            "attestation: not a compact JWS (want three dot-separated parts)",
        );
        let a_jws = sign_with(&AUTHOR, &author_claims(&["material"]));
        assert_refusal(
            &verify_author(&a_jws, &pubkey(&DELIVERY)).unwrap_err(),
            "attestation",
            "attestation: signature does not verify against the pinned key",
        );
        let d_jws = sign_with(&DELIVERY, &delivery_claims(&["material"], &["material"]));
        let late = verify_document(
            DOC,
            &d_jws,
            READER,
            1001,
            &["material".into()],
            PUBLISHER,
            &["material".into()],
            &pubkey(&AUTHOR),
            &pubkey(&DELIVERY),
        )
        .unwrap_err();
        assert_refusal(
            &late,
            "attestation",
            "attestation: the delivery signature has expired (§7.6 step 2)",
        );
    }

    /// A JWS segment is unpadded base64url and nothing else: a signature
    /// re-spelled — padded, `=` and anything after it, the standard
    /// alphabet, nonzero leftover bits — decodes to the same bytes a lax
    /// decoder would verify, so each is refused.
    #[test]
    fn a_jws_segment_is_unpadded_base64url_only() {
        assert_eq!(b64url_decode("QQ"), Some(vec![0x41]));
        assert_eq!(b64url_decode("-_8"), Some(vec![0xfb, 0xff]));
        for bad in ["QR", "QQ==", "Q", "A", "QUJDA", "+/8", "Q Q"] {
            assert_eq!(b64url_decode(bad), None, "{bad:?}");
        }
        let a_jws = sign_with(&AUTHOR, &author_claims(&["material"]));
        verify_author(&a_jws, &pubkey(&AUTHOR)).unwrap();
        let (input, sig) = a_jws.rsplit_once('.').unwrap();
        // 64 bytes are 86 characters, the last carrying 4 bits of padding.
        assert_eq!(sig.len(), 86);
        let last = sig.as_bytes()[85];
        let i = URL.iter().position(|c| *c == last).unwrap();
        let flipped = format!("{}{}", &sig[..85], URL[i ^ 1] as char);
        let swapped = sig.replacen('-', "+", 1).replacen('_', "/", 1);
        assert_ne!(swapped, sig, "the fixture signature has a `-` or `_`");
        for respelled in [
            format!("{sig}="),
            format!("{sig}=x"),
            format!("{sig}=="),
            swapped,
            flipped,
        ] {
            assert_refusal(
                &verify_author(&format!("{input}.{respelled}"), &pubkey(&AUTHOR)).unwrap_err(),
                "attestation",
                "attestation: bad signature base64",
            );
        }
        let (h, rest) = a_jws.split_once('.').unwrap();
        let (p, _) = rest.split_once('.').unwrap();
        assert_eq!(
            verify_author(&format!("{h}=.{p}.{sig}"), &pubkey(&AUTHOR))
                .unwrap_err()
                .code,
            "attestation"
        );
    }
}
