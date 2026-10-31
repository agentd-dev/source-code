// SPDX-License-Identifier: AGPL-3.0-only
//! **Instruction attestation** — instruction/1 author and delivery
//! signatures (§14.1, §19).
//!
//! JWS compact serializations (RFC 7515) over a claims object, Ed25519
//! (`alg: EdDSA`, RFC 8037), digests written `sha256:<hex>`. An **author**
//! signature binds a version's authored digest — the stored bytes as they
//! are; in instruction/1 a signature travels BESIDE the document, never in
//! it. A **delivery** signature binds what one reader received (the
//! delivered text's digest), its audience, the resolution it came from (its
//! `instruction-resolved/1` manifest) and the embedded author signature.
//!
//! The checks are the instruction.md SDK's ([`instruction_md::verify_author`],
//! [`instruction_md::verify_delivery`]): one definition of the claims and
//! their order for agentd and every other reader. What stays here is
//! agentd's: the trust pins (`agent.instruction.trust`) and what a verified
//! signature caps, the transports' sidecars, reading a key file, signing with
//! an [`AgentKey`] (tests and tooling), and the freshness helpers. Every
//! failure is a refusal with a stable code, never a downgrade to a weaker
//! unsigned path.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::aauth::AgentKey;
use crate::b64;
use crate::config::idoc::Refusal;

pub use instruction_md::{author_digest, digest, front_matter_id};

/// The `spec` claim of every instruction/1 attestation.
pub const SPEC_CLAIM: &str = "instruction/1";

/// An attestation's claims, typed. `pub` is the publisher.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claims {
    pub spec: String,
    pub typ: String,
    pub doc: String,
    #[serde(default)]
    pub version: String,
    pub digest: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(rename = "pub", default)]
    pub publisher: String,
    #[serde(default)]
    pub iat: u64,
    #[serde(default)]
    pub exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    /// A delivery's resolution: `{format: instruction-resolved/1, status,
    /// document: {uri, sha256, version?}, dependencies, resolved_digest}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Value>,
    /// A delivery's embedded author JWS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Verify an author signature over the authored bytes with a raw Ed25519
/// public key: the SDK's checks (EdDSA, `spec`, `typ`, expiry, the digest,
/// the `doc`, the publisher), then the claims typed.
pub fn verify_author(
    jws: &str,
    key: &[u8],
    authored: &[u8],
    doc: &str,
    publisher: &str,
    now: u64,
) -> Result<Claims, Refusal> {
    let r = instruction_md::verify_author(
        jws,
        authored,
        &[instruction_md::public_jwk(key, "")],
        &instruction_md::VerifyOptions {
            doc: doc.to_string(),
            publisher: publisher.to_string(),
            now: Some(now as i64),
        },
    );
    typed(r)
}

/// A verification refused carries the SDK's code (`instruction_md::VERIFY_CODES`)
/// and its reason.
fn refused(r: &instruction_md::VerifyResult) -> Refusal {
    Refusal::new(r.code, r.reason.clone())
}

/// Verified claims, typed.
fn claims(c: Option<serde_json::Map<String, Value>>) -> Result<Claims, Refusal> {
    serde_json::from_value(Value::Object(c.unwrap_or_default()))
        .map_err(|e| Refusal::new("malformed", format!("the claims do not read: {e}")))
}

fn typed(r: instruction_md::VerifyResult) -> Result<Claims, Refusal> {
    if !r.ok {
        return Err(refused(&r));
    }
    claims(r.claims)
}

/// A verified delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub delivery: Claims,
    /// The author attestation the delivery embeds, verified with it.
    pub author: Claims,
    /// What the document may activate: grant ∩ ceiling ∩ author ∩ delivery
    /// — a signature caps, it never grants.
    pub effective: Vec<String>,
}

/// Verify a delivery signature over the delivered bytes and its embedded
/// author signature (the SDK's §7.6 order), for reader `aud`, then cap the
/// operator's `granted` families by the pin's `max`, the author's and the
/// delivery's capabilities.
#[allow(clippy::too_many_arguments)]
pub fn verify_delivery(
    jws: &str,
    delivered: &[u8],
    delivery_key: &[u8],
    author_key: &[u8],
    aud: &str,
    publisher: &str,
    now: u64,
    granted: &[String],
    max: &[String],
) -> Result<Delivery, Refusal> {
    let r = instruction_md::verify_delivery(
        jws,
        delivered,
        &[instruction_md::public_jwk(delivery_key, "")],
        &[instruction_md::public_jwk(author_key, "")],
        &instruction_md::DeliveryVerifyOptions {
            aud: aud.to_string(),
            authored: None,
            publisher: publisher.to_string(),
            now: Some(now as i64),
        },
    );
    if !r.ok {
        return Err(refused(&r));
    }
    // The SDK verified the embedded author attestation with the delivery and
    // hands back its claims.
    let author = claims(r.author)?;
    let delivery = claims(r.claims)?;
    let effective = granted
        .iter()
        .filter(|g| max.is_empty() || max.contains(g))
        .filter(|g| author.capabilities.contains(g) && delivery.capabilities.contains(g))
        .cloned()
        .collect();
    Ok(Delivery {
        delivery,
        author,
        effective,
    })
}

/// The manifest a delivery of `text` names: its `instruction-resolved/1`
/// resolution (`Resolved::manifest`: format, status, document, dependencies
/// and digest, never its body), as the SDK resolves it with nothing to
/// compose.
pub fn manifest_of(text: &str, uri: &str, version: &str) -> Value {
    instruction_md::resolve(
        text,
        &instruction_md::ResolveOptions {
            uri: uri.to_string(),
            version: version.to_string(),
            ..instruction_md::ResolveOptions::default()
        },
    )
    .resolved
    .manifest()
    .to_json()
}

/// The author signature that travels beside a document file: the JWS in
/// `<path>.jws`, when there is one. An unreadable sidecar is an error — a
/// signature that cannot be read is not the same as none.
pub fn signature_beside(path: &str) -> Result<Option<String>, String> {
    let sidecar = format!("{path}.jws");
    match std::fs::read_to_string(&sidecar) {
        Ok(s) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{sidecar}: {e}")),
    }
}

/// Read an Ed25519 public key from a FILE — raw 32 bytes, 64 hex chars, or
/// base64url. The registry path can also resolve a JWKS over MCP; this half is
/// what a file/dir/url/oci source can use, with no client and no network.
pub fn load_key_file(path: &str) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if bytes.len() == 32 {
        return Ok(bytes);
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    let key = if text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        (0..32)
            .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok())
            .collect()
    } else {
        crate::b64::url_decode(&text).ok().filter(|v| v.len() == 32)
    };
    key.ok_or_else(|| {
        format!("{path}: not an Ed25519 public key (raw 32 bytes, 64 hex characters, or base64url)")
    })
}

/// Whether an `author_keys` entry names a registry JWKS rather than a key file.
pub fn is_jwks_uri(entry: &str) -> bool {
    entry.starts_with("instruction://")
}

/// What verifying a document against the pinned publishers concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Authorship {
    /// No pin carries a publisher — nothing was asked for.
    Unpinned,
    /// Pins exist, but none of them names a key file: their `author_keys` are
    /// `instruction://` JWKS URIs, or empty (discovered from a registry read),
    /// which need the registry client. The caller applies
    /// `agent.instruction.unenforceable`.
    NoLocalKeys,
    /// The document's signature verified against a pinned publisher.
    Verified {
        publisher: String,
        doc: String,
        /// The author-attested capabilities, already capped by the pin's
        /// `max_capabilities`. A signature CAPS what a document may activate;
        /// it never widens it.
        capabilities: Vec<String>,
    },
}

/// Verify a document against the trust pins, with the author signature that
/// travelled beside it (`signature`: a file's `<path>.jws`, an OCI layer's
/// annotation).
///
/// The rule is deliberately strict, because the alternative is a bypass: when
/// an operator has pinned a publisher, an UNSIGNED document — or one signed by
/// somebody else, or naming a `doc` id no pin covers — is refused.
pub fn verify_authored(
    bytes: &[u8],
    signature: Option<&str>,
    pins: &[InstructionSource],
    now: u64,
) -> Result<Authorship, Refusal> {
    let pinned: Vec<&InstructionSource> = pins.iter().filter(|p| !p.publisher.is_empty()).collect();
    if pinned.is_empty() {
        return Ok(Authorship::Unpinned);
    }
    // Only FILE key material is usable here: resolving a JWKS needs the
    // registry client, which a file/oci/url load does not have. A key file
    // that cannot be read refuses the document rather than dropping out: a
    // pin left with no key would verify nothing, and an unsigned document
    // would then load under it.
    let mut keys: Vec<(&InstructionSource, Vec<u8>)> = Vec::new();
    for p in &pinned {
        for k in p.author_keys.iter().filter(|k| !is_jwks_uri(k)) {
            let key = load_key_file(k).map_err(|e| {
                Refusal::new(
                    "unknown-key",
                    format!(
                        "the author key pinned for publisher {:?} cannot be used — {e}",
                        p.publisher
                    ),
                )
            })?;
            keys.push((*p, key));
        }
    }
    if keys.is_empty() {
        return Ok(Authorship::NoLocalKeys);
    }
    let jws = signature.ok_or_else(|| {
        Refusal::new(
            "author-missing",
            format!(
                "the instruction carries no author signature beside it (a file's `<path>.jws`, \
                 an OCI layer's `md.instruction/signature` annotation), but {} pinned \
                 publisher(s) are configured — an unsigned document is not from a pinned \
                 publisher",
                pinned.len()
            ),
        )
    })?;
    // The refusal of the last pin tried: every pin is a chance to verify, so
    // only when none does is the document refused, and with the last reason.
    let mut last = Refusal::new(
        "bad-signature",
        "no pinned key verifies this document's signature",
    );
    for (pin, key) in &keys {
        let pin_doc = pin.uri.split('@').next().unwrap_or(&pin.uri);
        match verify_author(jws, key, bytes, pin_doc, &pin.publisher, now) {
            Ok(claims) => {
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
            // Bytes the signature does not cover are refused at once: no
            // other pin makes them the signed ones.
            Err(e) if e.code == "digest-mismatch" => return Err(e),
            Err(e) => last = e,
        }
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

/// One pinned instruction source in operator configuration — the very type
/// the config surface deserializes at `agent.instruction.trust`.
pub use crate::config::settings::InstructionSource;

// ── revocation: authorization is current membership, re-checked ─────────────

/// A signed source's re-check interval in seconds: its `freshness`, parsed.
/// `None` if the source declares none.
pub fn freshness_secs(src: &InstructionSource) -> Option<u64> {
    src.freshness
        .as_deref()
        .and_then(|s| crate::config::parse_duration(s).ok())
        .map(|d| d.as_secs())
}

/// Whether authorization is STALE — the deadline (`last_ok + freshness`) has
/// passed at `now`. Past it the runtime refuses NEW work and lets live work
/// run out; a successful re-read resets `last_ok`. A failed or unreachable
/// read is staleness, never revocation.
pub fn is_stale(last_ok: u64, freshness_secs: u64, now: u64) -> bool {
    now.saturating_sub(last_ok) >= freshness_secs
}

/// The families that LEFT the effective set between two reconciles — the
/// control plane revokes documents, and the runtime retracts the derived state
/// of anything that left. A full reconcile at reconnect honours offline
/// revocations.
pub fn families_retracted(before: &[String], after: &[String]) -> Vec<String> {
    before
        .iter()
        .filter(|f| !after.contains(f))
        .cloned()
        .collect()
}

/// The current time, for callers that verify now.
pub fn now() -> u64 {
    now_secs()
}

#[cfg(test)]
mod authored_tests {
    use super::*;
    use crate::config::settings::InstructionSource;

    const DOC: &str = "---\nspec: \"1\"\nid: instruction://ins_1\n---\nbe terse\n";

    /// A real Ed25519 key pair and a real author JWS over the document's
    /// bytes — no mocks: the point is that the bytes verify.
    fn key_and_sig(dir: &std::path::Path, doc: &str, caps: &[&str]) -> (String, String) {
        let key = AgentKey::generate().unwrap();
        let pub_path = dir.join("author.pub");
        std::fs::write(&pub_path, key.public_bytes()).unwrap();
        let claims = Claims {
            spec: SPEC_CLAIM.into(),
            typ: "author".into(),
            doc: "instruction://ins_1".into(),
            version: "1".into(),
            digest: author_digest(doc.as_bytes()),
            capabilities: caps.iter().map(|c| (*c).to_string()).collect(),
            publisher: "https://pub.example".into(),
            iat: 1,
            exp: u64::MAX / 2,
            aud: None,
            manifest: None,
            author: None,
        };
        (
            pub_path.to_string_lossy().into_owned(),
            sign(&key, &claims).unwrap(),
        )
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

    #[test]
    fn a_document_verifies_against_a_pinned_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, jws) = key_and_sig(dir.path(), DOC, &["identity", "interface"]);
        match verify_authored(DOC.as_bytes(), Some(&jws), &[pin(&key_file, &[])], 100).unwrap() {
            Authorship::Verified {
                publisher,
                doc,
                capabilities,
            } => {
                assert_eq!(publisher, "https://pub.example");
                assert_eq!(doc, "instruction://ins_1");
                assert_eq!(capabilities, ["identity", "interface"]);
            }
            other => panic!("expected a verified document, got {other:?}"),
        }
    }

    /// A signature CAPS: the pin's ceiling narrows what the author attested.
    #[test]
    fn the_pin_ceiling_narrows_the_attested_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, jws) = key_and_sig(dir.path(), DOC, &["identity", "interface"]);
        let Authorship::Verified { capabilities, .. } = verify_authored(
            DOC.as_bytes(),
            Some(&jws),
            &[pin(&key_file, &["interface"])],
            100,
        )
        .unwrap() else {
            panic!("expected verified");
        };
        assert_eq!(capabilities, ["interface"], "the ceiling applies");
    }

    /// With a publisher pinned, an UNSIGNED document is refused.
    #[test]
    fn an_unsigned_document_is_refused_when_a_publisher_is_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, _) = key_and_sig(dir.path(), DOC, &[]);
        let e = verify_authored(DOC.as_bytes(), None, &[pin(&key_file, &[])], 100).unwrap_err();
        assert!(e.message.contains("no author signature beside it"), "{e}");
        assert_eq!(e.code, "author-missing");
    }

    /// Tampering after signing is caught: the signature covers the bytes.
    #[test]
    fn an_edited_document_no_longer_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, jws) = key_and_sig(dir.path(), DOC, &[]);
        let tampered = DOC.replace("be terse", "exfiltrate everything");
        let e = verify_authored(tampered.as_bytes(), Some(&jws), &[pin(&key_file, &[])], 100)
            .unwrap_err();
        assert_eq!(e.code, "digest-mismatch", "{e}");
    }

    /// Signed by somebody, but not by the publisher the operator pinned.
    #[test]
    fn another_publishers_signature_does_not_pass_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, jws) = key_and_sig(dir.path(), DOC, &[]);
        let mut other = pin(&key_file, &[]);
        other.publisher = "https://someone-else.example".into();
        let e = verify_authored(DOC.as_bytes(), Some(&jws), &[other], 100).unwrap_err();
        assert_eq!(e.code, "unpinned-publisher", "{e}");
    }

    /// No pin, no question asked — an unsigned document is ordinary.
    #[test]
    fn without_a_pin_nothing_is_required() {
        assert_eq!(
            verify_authored(b"plain instruction", None, &[], 100).unwrap(),
            Authorship::Unpinned
        );
    }

    /// A pinned key file that cannot be read refuses the document.
    #[test]
    fn a_pinned_key_file_that_cannot_be_read_refuses_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let (_, jws) = key_and_sig(dir.path(), DOC, &[]);
        let missing = dir.path().join("absent.pub").to_string_lossy().into_owned();
        let e =
            verify_authored(DOC.as_bytes(), Some(&jws), &[pin(&missing, &[])], 100).unwrap_err();
        assert!(e.to_string().contains("cannot be used"), "{e}");
    }

    #[test]
    fn jwks_only_pins_report_that_they_cannot_be_enforced_here() {
        let mut src = pin("x", &[]);
        src.author_keys = vec!["instruction://ins_1/keys.json".into()];
        assert_eq!(
            verify_authored(b"anything", None, &[src], 100).unwrap(),
            Authorship::NoLocalKeys
        );
    }

    #[test]
    fn the_signature_beside_a_file_is_its_jws_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("agent.md");
        let doc = doc.to_string_lossy();
        assert_eq!(signature_beside(&doc).unwrap(), None);
        std::fs::write(format!("{doc}.jws"), "eyJ.abc.def\n").unwrap();
        assert_eq!(
            signature_beside(&doc).unwrap().as_deref(),
            Some("eyJ.abc.def")
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

    /// A delivery of `doc` for `principal://usr_7`, embedding its manifest
    /// and the author JWS.
    fn delivery_claims(doc: &[u8], caps: &[&str], author_jws: &str) -> Claims {
        let text = std::str::from_utf8(doc).unwrap();
        Claims {
            aud: Some("principal://usr_7".into()),
            manifest: Some(manifest_of(text, "instruction://ins_42", "ver_01K003")),
            author: Some(author_jws.into()),
            exp: 1_757_003_600,
            ..claims("delivery", &digest(doc), caps)
        }
    }

    const NOW: u64 = 1_757_000_100;
    const DOC: &[u8] = b"be terse\n";

    #[test]
    fn digest_is_sha256_hex() {
        assert_eq!(
            digest(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn an_author_signature_round_trips() {
        let k = key();
        let c = claims("author", &digest(DOC), &["identity"]);
        let jws = sign(&k, &c).unwrap();
        let back = verify_author(&jws, k.public_bytes(), DOC, "", "", NOW).unwrap();
        assert_eq!(back, c);
        // A different key does not verify.
        let other = AgentKey::from_seed(&[9u8; 32]).unwrap();
        assert!(verify_author(&jws, other.public_bytes(), DOC, "", "", NOW).is_err());
    }

    #[test]
    fn a_delivery_signature_cannot_stand_in_for_an_author_one() {
        let k = key();
        let jws = sign(&k, &claims("delivery", &digest(DOC), &[])).unwrap();
        let e = verify_author(&jws, k.public_bytes(), DOC, "", "", NOW).unwrap_err();
        assert_eq!(e.code, "typ-mismatch", "{e}");
    }

    #[test]
    fn a_signature_caps_it_never_grants() {
        let author = key();
        let delivery = AgentKey::from_seed(&[8u8; 32]).unwrap();
        let a_jws = sign(
            &author,
            &claims("author", &digest(DOC), &["identity", "interface"]),
        )
        .unwrap();
        let d_jws = sign(&delivery, &delivery_claims(DOC, &["interface"], &a_jws)).unwrap();
        let v = verify_delivery(
            &d_jws,
            DOC,
            delivery.public_bytes(),
            author.public_bytes(),
            "principal://usr_7",
            "https://instruction.md/pub/acme",
            NOW,
            &["identity".into(), "interface".into()],
            &[],
        )
        .unwrap();
        // grant ∩ author ∩ delivery = {interface}.
        assert_eq!(v.effective, vec!["interface".to_string()]);
        assert_eq!(v.author.capabilities, ["identity", "interface"]);
    }

    #[test]
    fn a_delivery_ceiling_exceeding_the_author_is_refused() {
        let author = key();
        let delivery = AgentKey::from_seed(&[8u8; 32]).unwrap();
        let a_jws = sign(&author, &claims("author", &digest(DOC), &["interface"])).unwrap();
        let d_jws = sign(
            &delivery,
            &delivery_claims(DOC, &["interface", "identity"], &a_jws),
        )
        .unwrap();
        let e = verify_delivery(
            &d_jws,
            DOC,
            delivery.public_bytes(),
            author.public_bytes(),
            "principal://usr_7",
            "",
            NOW,
            &[],
            &[],
        )
        .unwrap_err();
        assert_eq!(e.code, "capabilities-exceeded", "{e}");
    }

    #[test]
    fn a_digest_mismatch_is_refused() {
        let author = key();
        let delivery = AgentKey::from_seed(&[8u8; 32]).unwrap();
        let a_jws = sign(&author, &claims("author", &digest(DOC), &[])).unwrap();
        let d_jws = sign(&delivery, &delivery_claims(DOC, &[], &a_jws)).unwrap();
        let e = verify_delivery(
            &d_jws,
            b"the real bytes",
            delivery.public_bytes(),
            author.public_bytes(),
            "principal://usr_7",
            "",
            NOW,
            &[],
            &[],
        )
        .unwrap_err();
        assert_eq!(e.code, "digest-mismatch", "{e}");
    }

    #[test]
    fn the_delivery_must_be_addressed_to_this_reader() {
        let author = key();
        let delivery = AgentKey::from_seed(&[8u8; 32]).unwrap();
        let a_jws = sign(&author, &claims("author", &digest(DOC), &[])).unwrap();
        let d_jws = sign(&delivery, &delivery_claims(DOC, &[], &a_jws)).unwrap();
        let e = verify_delivery(
            &d_jws,
            DOC,
            delivery.public_bytes(),
            author.public_bytes(),
            "principal://someone_else",
            "",
            NOW,
            &[],
            &[],
        )
        .unwrap_err();
        assert_eq!(e.code, "audience-mismatch", "{e}");
    }

    /// In instruction/1 the author digest is the stored bytes as they are.
    #[test]
    fn the_author_digest_is_the_stored_bytes() {
        let doc = "---\nspec: \"1\"\nid: instruction://ins_x\n---\nbody\n";
        assert_eq!(author_digest(doc.as_bytes()), digest(doc.as_bytes()));
        assert_eq!(
            front_matter_id(doc.as_bytes()).as_deref(),
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
        assert!(!is_stale(1000, 900, 1000 + 899));
        assert!(is_stale(1000, 900, 1000 + 900));
        assert_eq!(
            families_retracted(
                &["identity".into(), "interface".into()],
                &["interface".into()]
            ),
            vec!["identity".to_string()]
        );
    }
}
