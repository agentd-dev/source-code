// SPDX-License-Identifier: AGPL-3.0-only
//! A signed author + delivery round-trip over an instruction document (§7).
//!
//! `cargo run -p agentd-core --example sign_roundtrip --features sign -- <doc.md>`
//!
//! Prints the two JWS compact serializations, the resolution manifest, the
//! decoded claims, and the effective capability ceiling — the artifacts §7.2
//! and §7.4 describe. Keys are derived from fixed seeds so the run is
//! reproducible; a real deployment holds an offline author key and an online
//! delivery key.

#[cfg(feature = "sign")]
fn main() {
    use agentd::aauth::AgentKey;
    use agentd::config::attest::{self, Claims};

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/root/instruction-md/specification/samples/house-style.md".to_string());
    let bytes = std::fs::read(&path).expect("read the instruction document");

    // Offline author key; online delivery key.
    let author = AgentKey::from_seed(&[1u8; 32]).unwrap();
    let delivery = AgentKey::from_seed(&[2u8; 32]).unwrap();

    // With no per-reader resolution, the delivered bytes are the authored
    // bytes, so one digest serves both. A resolving server would deliver
    // different bytes and record the difference in the manifest.
    let dig = attest::digest(&bytes);

    let now = 1_757_000_000u64;
    let publisher = "https://instruction.md/pub/acme";
    let author_claims = Claims {
        spec: attest::SPEC_CLAIM.into(),
        typ: "author".into(),
        doc: "instruction://ins_housestyle".into(),
        version: "ver_01K003".into(),
        digest: dig.clone(),
        capabilities: vec!["material".into()],
        publisher: publisher.into(),
        iat: now,
        exp: now + 31_536_000,
    };
    let delivery_claims = Claims {
        typ: "delivery".into(),
        ..author_claims.clone()
    };

    let author_jws = attest::sign(&author, &author_claims).unwrap();
    let delivery_jws = attest::sign(&delivery, &delivery_claims).unwrap();

    let manifest = format!(
        "authored:   {{ version: {}, digest: \"{}\" }}\n\
         parameters: []\n\
         facts:      []\n\
         variants:   {{ kept: [], dropped: [] }}\n\
         includes:   []\n\
         limits:     {{ include_depth: 0, include_bytes: {} }}\n",
        author_claims.version,
        dig,
        bytes.len()
    );

    let src = attest::InstructionSource {
        uri: "instruction://ins_housestyle".into(),
        publisher: publisher.into(),
        author_keys: vec![],
        delivery_keys: vec![],
        max_capabilities: vec!["material".into()],
        freshness: Some("15m".into()),
    };

    let verified = attest::verify_document(
        &bytes,
        &delivery_jws,
        &author_jws,
        &manifest,
        &["material".into()],
        &src,
        author.public_bytes(),
        delivery.public_bytes(),
    )
    .expect("the round-trip verifies");

    println!("document:        {path}");
    println!("authored digest: {dig}");
    println!("\n── author JWS (typ=author, offline key) ──\n{author_jws}");
    println!("\n── delivery JWS (typ=delivery, online key) ──\n{delivery_jws}");
    println!("\n── resolution manifest (§7.4) ──\n{manifest}");
    println!("── verified ──");
    println!("author.capabilities   = {:?}", verified.author.capabilities);
    println!(
        "delivery.capabilities = {:?}",
        verified.delivery.capabilities
    );
    println!(
        "effective (grant ∩ max ∩ author ∩ delivery) = {:?}",
        verified.effective
    );
    println!("manifest.authored     = {:?}", verified.manifest.authored);
}

#[cfg(not(feature = "sign"))]
fn main() {
    eprintln!("build with --features sign");
}
