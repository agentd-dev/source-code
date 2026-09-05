// SPDX-License-Identifier: AGPL-3.0-only
//! A signed author + delivery round-trip over an instruction document (§7).
//!
//! `cargo run -p agentd-core --example sign_roundtrip --features sign -- <doc.md>`
//!
//! Prints the two JWS compact serializations (and their decoded header +
//! claims), the resolution manifest, and the effective capability ceiling — the
//! artifacts §7.2 and §7.4 describe. Keys are derived from fixed seeds so the
//! run is reproducible; a real deployment holds an offline author key and an
//! online delivery key.

#[cfg(feature = "sign")]
fn main() {
    use agentd::aauth::AgentKey;
    use agentd::config::attest::{self, Authored, Claims, Manifest, Variants};
    use serde_json::Value;

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/root/instruction-md/specification/samples/house-style.md".to_string());
    let bytes = std::fs::read(&path).expect("read the instruction document");

    // `doc` MUST equal the document's front-matter `id` (§3.1).
    let doc = attest::front_matter_id(&bytes).unwrap_or_else(|| "instruction://ins_unknown".into());

    // Offline author key; online delivery key.
    let author = AgentKey::from_seed(&[1u8; 32]).unwrap();
    let delivery = AgentKey::from_seed(&[2u8; 32]).unwrap();

    // Author digest excludes a front-matter `signature:` line (§7.2). With no
    // per-reader resolution the delivered bytes are the authored bytes.
    let authored_digest = attest::author_digest(&bytes);
    let delivered_digest = attest::digest(&bytes);

    let now = 1_757_000_000u64;
    let version = "ver_01K003";
    let publisher = "https://instruction.md/pub/acme";

    let author_claims = Claims {
        spec: attest::SPEC_CLAIM.into(),
        typ: "author".into(),
        doc: doc.clone(),
        version: version.into(),
        digest: authored_digest.clone(),
        capabilities: vec!["material".into()],
        publisher: publisher.into(),
        iat: now,
        exp: now + 31_536_000, // author: long-lived
        aud: None,
        manifest: None,
        author: None,
    };
    let author_jws = attest::sign(&author, &author_claims).unwrap();

    let manifest = Manifest {
        authored: Authored {
            version: version.into(),
            digest: authored_digest.clone(),
        },
        parameters: vec![],
        facts: vec![],
        variants: Variants {
            kept: vec![],
            dropped: vec![],
        },
        includes: vec![],
        limits: serde_json::json!({ "include_depth": 0, "include_bytes": bytes.len() }),
    };

    let delivery_claims = Claims {
        spec: attest::SPEC_CLAIM.into(),
        typ: "delivery".into(),
        doc: doc.clone(),
        version: version.into(),
        digest: delivered_digest.clone(),
        capabilities: vec!["material".into()], // ⊆ author
        publisher: publisher.into(),
        iat: now,
        exp: now + 3_600, // delivery: within hours — attests one resolution
        aud: Some("principal://usr_7".into()),
        manifest: Some(manifest.clone()),
        author: Some(author_jws.clone()),
    };
    let delivery_jws = attest::sign(&delivery, &delivery_claims).unwrap();

    let src = attest::InstructionSource {
        uri: doc.clone(),
        publisher: publisher.into(),
        author_keys: vec![],
        delivery_keys: vec![],
        max_capabilities: vec!["material".into()],
        freshness: Some("15m".into()),
    };

    let verified = attest::verify_document(
        &bytes,
        &delivery_jws,
        "principal://usr_7",
        now + 60,
        &["material".into()],
        &src,
        author.public_bytes(),
        delivery.public_bytes(),
    )
    .expect("the round-trip verifies");

    // Minimal base64url decode for the demo output only.
    fn b64_decode(s: &str) -> Vec<u8> {
        let mut bits = 0u32;
        let mut n = 0;
        let mut out = Vec::new();
        for c in s.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'-' => 62,
                b'_' => 63,
                _ => continue,
            };
            bits = bits << 6 | v as u32;
            n += 6;
            if n >= 8 {
                n -= 8;
                out.push((bits >> n) as u8);
            }
        }
        out
    }
    let decode =
        |seg: &str| -> Value { serde_json::from_slice(&b64_decode(seg)).unwrap_or(Value::Null) };
    let parts: Vec<&str> = author_jws.split('.').collect();
    let dparts: Vec<&str> = delivery_jws.split('.').collect();

    println!("document:         {path}");
    println!("doc (=fm id):     {doc}");
    println!("authored digest:  {authored_digest}");
    println!("delivered digest: {delivered_digest}");
    println!("\n── AUTHOR JWS ──\n{author_jws}");
    println!("  header: {}", decode(parts[0]));
    println!("  claims: {}", decode(parts[1]));
    println!("\n── DELIVERY JWS ──\n{delivery_jws}");
    println!("  header: {}", decode(dparts[0]));
    println!("  claims: {}", decode(dparts[1]));
    println!(
        "\n── verified ──\neffective (grant ∩ max ∩ author ∩ delivery) = {:?}",
        verified.effective
    );
}

#[cfg(not(feature = "sign"))]
fn main() {
    eprintln!("build with --features sign");
}
