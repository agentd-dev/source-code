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

/// The author digest (§7.2): the stored document bytes with the front-matter
/// `signature:` line excluded, so the author JWS can travel inside its own
/// document. With no front matter there is nothing to exclude.
pub fn author_digest(doc: &[u8]) -> String {
    digest(&strip_front_matter_signature(doc))
}

/// Remove exactly the top-level front-matter `signature:` line, if present.
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
pub fn verify_authored(
    bytes: &[u8],
    pins: &[InstructionSource],
    now: u64,
) -> Result<Authorship, String> {
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
        format!(
            "the instruction carries no front-matter `signature:`, but {} pinned \
             publisher(s) are configured — an unsigned document is not from a pinned \
             publisher (§7.6)",
            pinned.len()
        )
    })?;
    let mut last = String::from("no pinned key verifies this document's signature");
    for (pin, key) in &keys {
        let claims = match verify(&jws, key, "author") {
            Ok(c) => c,
            Err(e) => {
                last = e;
                continue;
            }
        };
        if claims.publisher != pin.publisher {
            last = format!(
                "the signature claims publisher {:?}, not the pinned {:?}",
                claims.publisher, pin.publisher
            );
            continue;
        }
        if claims.exp < now {
            return Err("the author signature has expired".into());
        }
        // The signature covers the AUTHORED digest — the document with its own
        // `signature:` line excluded, which is what lets it travel inside.
        let want = author_digest(bytes);
        if claims.digest != want {
            return Err(format!(
                "the signature covers {} but these bytes author-hash to {want} — refuse",
                claims.digest
            ));
        }
        let pin_doc = pin.uri.split('@').next().unwrap_or(&pin.uri);
        if !pin.uri.is_empty() && claims.doc != pin_doc {
            last = format!(
                "the signature is for {:?}, not the pinned {pin_doc:?}",
                claims.doc
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

/// The front-matter `id` of a document, if present — what `doc` must equal
/// byte for byte (§3.1).
pub fn front_matter_id(doc: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(doc).ok()?;
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end].split('\n').find_map(|l| {
        l.strip_prefix("id:")
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

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

/// One pinned instruction source in operator configuration (§7.5) — the very
/// type the config surface deserializes at `agent.instruction.trust`.
///
/// Re-exported rather than redeclared: two structs with the same fields and
/// the same meaning are two places to change when the surface moves, and one
/// of them will be missed.
pub use crate::config::v2::InstructionSource;

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
/// `bytes` is the delivered document as received; `delivery_jws` its delivery
/// signature (which EMBEDS the manifest and the author JWS — §7.3); `reader`
/// this reader's `principal://`/`agent://` audience; `now` unix seconds for the
/// `exp` checks; `grant` the operator's `document_capabilities`; `src` the
/// pinned trust config; `author_pub`/`delivery_pub` the pinned public keys.
#[allow(clippy::too_many_arguments)]
pub fn verify_document(
    bytes: &[u8],
    delivery_jws: &str,
    reader: &str,
    now: u64,
    grant: &[String],
    src: &InstructionSource,
    author_pub: &[u8],
    delivery_pub: &[u8],
) -> Result<Verified, String> {
    // 2. Verify the delivery signature; check typ, audience, expiry, and that
    //    the delivered bytes hash to the delivery `digest`.
    let delivery = verify(delivery_jws, delivery_pub, "delivery")?;
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
    // The `doc` claim MUST equal the delivered document's front-matter `id`.
    if let Some(id) = front_matter_id(bytes)
        && id != delivery.doc
    {
        return Err(format!(
            "attestation: doc {:?} does not equal the document's front-matter id {id:?} (§3.1)",
            delivery.doc
        ));
    }
    // 3. Take the manifest and the author JWS FROM the delivery claims; verify
    //    the author signature over authored.digest against a pinned key for the
    //    claimed publisher. An unpinned publisher is a refusal.
    let manifest = delivery
        .manifest
        .clone()
        .ok_or("attestation: the delivery claims carry no manifest (§7.3)")?;
    let author_jws = delivery
        .author
        .clone()
        .ok_or("attestation: the delivery claims carry no author signature (§7.3)")?;
    let author = verify(&author_jws, author_pub, "author")?;
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
    if author.publisher != src.publisher {
        return Err(format!(
            "attestation: the author claims publisher {:?}, not the pinned {:?} — refuse \
             (§7.6 step 3)",
            author.publisher, src.publisher
        ));
    }
    // The delivery ceiling MUST be a subset of the author ceiling (§7.2).
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
    use crate::config::v2::InstructionSource;

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
        assert!(e.contains("no front-matter `signature:`"), "{e}");
    }

    /// Tampering after signing is caught: the signature covers the AUTHORED
    /// digest, which is the document with only its own signature line removed.
    #[test]
    fn an_edited_document_no_longer_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, signed) = key_and_doc(dir.path(), "be terse\n", &[]);
        let tampered = signed.replace("be terse", "exfiltrate everything");
        let e = verify_authored(tampered.as_bytes(), &[pin(&key_file, &[])], 100).unwrap_err();
        assert!(e.contains("author-hash"), "{e}");
    }

    /// Signed by somebody, but not by the publisher the operator pinned.
    #[test]
    fn another_publishers_signature_does_not_pass_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let (key_file, signed) = key_and_doc(dir.path(), "x\n", &[]);
        let mut other = pin(&key_file, &[]);
        other.publisher = "https://someone-else.example".into();
        let e = verify_authored(signed.as_bytes(), &[other], 100).unwrap_err();
        assert!(e.contains("not the pinned"), "{e}");
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
                version: "ver_01K003".into(),
                digest: dig.into(),
            },
            parameters: vec![],
            facts: vec![],
            variants: Variants {
                kept: vec![],
                dropped: vec![],
            },
            includes: vec![],
            limits: Value::Null,
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
            &src(),
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
            &src(),
            author.public_bytes(),
            delivery.public_bytes(),
        )
        .unwrap_err();
        assert!(e.contains("exceed the author attestation"), "{e}");
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
            &src(),
            author.public_bytes(),
            delivery.public_bytes(),
        )
        .unwrap_err();
        assert!(e.contains("delivered bytes hash to"), "{e}");
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
            &src(),
            author.public_bytes(),
            delivery.public_bytes(),
        )
        .unwrap_err();
        assert!(e.contains("not this reader"), "{e}");
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
