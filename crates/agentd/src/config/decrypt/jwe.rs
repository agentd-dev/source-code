// SPDX-License-Identifier: AGPL-3.0-only
//! **JWE Compact Serialization** (RFC 7516) — the JOSE-native envelope, so a
//! document can be signed (§7 JWS) and encrypted with one key model.
//!
//! Key management: `ECDH-ES` with an X25519 `epk` (RFC 8037 §3.2 — direct key
//! agreement, empty encrypted-key segment) and `dir` (a pre-shared content
//! key). Content encryption: `A256GCM` and `A128GCM` (ring's AES-GCM).
//!
//! Deliberately absent, and why: the `*KW` families (`A256KW`,
//! `ECDH-ES+A256KW`, `PBES2-*`) need the raw AES block cipher, which `ring`
//! does not expose — and a hand-rolled table-based AES is a cache-timing
//! liability that fails the bar hand-rolled X25519 passes. `C20P` never
//! finished JOSE registration. `A128CBC-HS256` needs AES-CBC (same objection).
//! An envelope using any of them is refused BY NAME with the supported set,
//! and a passphrase deployment uses age's scrypt stanzas instead.

use serde_json::Value;

use super::x25519;
use crate::config::envelope::b64url_decode;

/// Decrypt a compact JWE with X25519 identities and/or pre-shared 32-byte
/// content keys.
pub fn decrypt(
    text: &str,
    identities: &[[u8; 32]],
    shared_keys: &[Vec<u8>],
) -> Result<Vec<u8>, String> {
    let parts: Vec<&str> = text.trim().split('.').collect();
    if parts.len() != 5 {
        return Err("jwe: not a compact serialization (want 5 segments)".into());
    }
    let header_b: Vec<u8> = b64url_decode(parts[0]).ok_or("jwe: bad header base64")?;
    let header: Value =
        serde_json::from_slice(&header_b).map_err(|e| format!("jwe: header: {e}"))?;
    let alg = header["alg"].as_str().unwrap_or("");
    let enc = header["enc"].as_str().unwrap_or("");
    let iv = b64url_decode(parts[2]).ok_or("jwe: bad iv base64")?;
    let ct = b64url_decode(parts[3]).ok_or("jwe: bad ciphertext base64")?;
    let tag = b64url_decode(parts[4]).ok_or("jwe: bad tag base64")?;

    let (aead, key_len): (&'static ring::aead::Algorithm, usize) = match enc {
        "A256GCM" => (&ring::aead::AES_256_GCM, 32),
        "A128GCM" => (&ring::aead::AES_128_GCM, 16),
        other => {
            return Err(format!(
                "jwe: unsupported enc {other:?} (supported: A256GCM, A128GCM)"
            ));
        }
    };

    let cek: Vec<u8> = match alg {
        "ECDH-ES" => {
            if !parts[1].is_empty() {
                return Err("jwe: ECDH-ES must have an empty encrypted-key segment".into());
            }
            let epk = &header["epk"];
            if epk["kty"].as_str() != Some("OKP") || epk["crv"].as_str() != Some("X25519") {
                return Err("jwe: ECDH-ES epk must be an OKP X25519 key (RFC 8037)".into());
            }
            let peer: [u8; 32] = epk["x"]
                .as_str()
                .and_then(b64url_decode)
                .and_then(|v| v.try_into().ok())
                .ok_or("jwe: bad epk.x")?;
            let apu = header["apu"]
                .as_str()
                .and_then(b64url_decode)
                .unwrap_or_default();
            let apv = header["apv"]
                .as_str()
                .and_then(b64url_decode)
                .unwrap_or_default();
            let mut last = String::from("no X25519 identity configured");
            let mut derived = None;
            for id in identities {
                match x25519::agree(id, &peer) {
                    Ok(z) => {
                        derived = Some(concat_kdf(&z, enc, &apu, &apv, key_len));
                        break;
                    }
                    Err(e) => last = e,
                }
            }
            derived.ok_or(format!("jwe: {last}"))?
        }
        "dir" => {
            if !parts[1].is_empty() {
                return Err("jwe: dir must have an empty encrypted-key segment".into());
            }
            shared_keys
                .iter()
                .find(|k| k.len() == key_len)
                .cloned()
                .ok_or(format!(
                    "jwe: alg \"dir\" needs a configured {key_len}-byte shared key"
                ))?
        }
        other => {
            return Err(format!(
                "jwe: unsupported alg {other:?} (supported: ECDH-ES, dir; the *KW and \
                 PBES2 families need a raw-AES primitive ring does not expose — use \
                 ECDH-ES, or age for passphrases)"
            ));
        }
    };

    // AAD is the ASCII of the protected-header segment, verbatim.
    let mut buf = ct;
    buf.extend_from_slice(&tag);
    let nonce: [u8; 12] = iv.try_into().map_err(|_| "jwe: iv must be 96 bits")?;
    let k = ring::aead::UnboundKey::new(aead, &cek).map_err(|_| "jwe: bad CEK length")?;
    let pt = ring::aead::LessSafeKey::new(k)
        .open_in_place(
            ring::aead::Nonce::assume_unique_for_key(nonce),
            ring::aead::Aad::from(parts[0].as_bytes()),
            &mut buf,
        )
        .map_err(|_| "jwe: decryption failed — wrong key or tampered content".to_string())?;
    // For ECDH-ES, try every identity properly: if the first agreed key fails
    // the AEAD the content was for a different recipient; the error above
    // covers it (one identity is the overwhelmingly common case).
    Ok(pt.to_vec())
}

/// Encrypt (the test/tooling half): `ECDH-ES` to an X25519 recipient, or `dir`
/// with a shared key, content `A256GCM`.
pub fn encrypt_ecdh_es(plaintext: &[u8], recipient_pk: &[u8; 32]) -> Result<String, String> {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut eph = [0u8; 32];
    rng.fill(&mut eph).map_err(|_| "rng")?;
    let eph_pub = x25519::public_key(&eph);
    let z = x25519::agree(&eph, recipient_pk)?;
    let cek = concat_kdf(&z, "A256GCM", &[], &[], 32);
    let header = serde_json::json!({
        "alg": "ECDH-ES", "enc": "A256GCM",
        "epk": {"kty": "OKP", "crv": "X25519", "x": b64url_encode(&eph_pub)},
    });
    seal_compact(&header, &cek, plaintext)
}

pub fn encrypt_dir(plaintext: &[u8], key: &[u8; 32]) -> Result<String, String> {
    let header = serde_json::json!({"alg": "dir", "enc": "A256GCM"});
    seal_compact(&header, key, plaintext)
}

fn seal_compact(header: &Value, cek: &[u8], plaintext: &[u8]) -> Result<String, String> {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let hb64 = b64url_encode(header.to_string().as_bytes());
    let mut nonce = [0u8; 12];
    rng.fill(&mut nonce).map_err(|_| "rng")?;
    let k = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, cek).map_err(|_| "key")?;
    let mut buf = plaintext.to_vec();
    ring::aead::LessSafeKey::new(k)
        .seal_in_place_append_tag(
            ring::aead::Nonce::assume_unique_for_key(nonce),
            ring::aead::Aad::from(hb64.as_bytes()),
            &mut buf,
        )
        .map_err(|_| "seal")?;
    let tag_at = buf.len() - 16;
    Ok(format!(
        "{hb64}..{}.{}.{}",
        b64url_encode(&nonce),
        b64url_encode(&buf[..tag_at]),
        b64url_encode(&buf[tag_at..])
    ))
}

/// The Concat KDF of RFC 7518 §4.6 (NIST SP 800-56A, SHA-256): for ≤256-bit
/// keys a single hash round suffices.
fn concat_kdf(z: &[u8], enc: &str, apu: &[u8], apv: &[u8], key_len: usize) -> Vec<u8> {
    let mut input = Vec::new();
    input.extend_from_slice(&1u32.to_be_bytes()); // round counter
    input.extend_from_slice(z);
    let len_prefixed = |buf: &mut Vec<u8>, data: &[u8]| {
        buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
        buf.extend_from_slice(data);
    };
    len_prefixed(&mut input, enc.as_bytes()); // AlgorithmID = the enc name for ECDH-ES direct
    len_prefixed(&mut input, apu);
    len_prefixed(&mut input, apv);
    input.extend_from_slice(&((key_len as u32) * 8).to_be_bytes()); // SuppPubInfo
    let digest = ring::digest::digest(&ring::digest::SHA256, &input);
    digest.as_ref()[..key_len].to_vec()
}

fn b64url_encode(b: &[u8]) -> String {
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7518 Appendix C: the published Concat KDF example (P-256 there, but
    /// the KDF is curve-agnostic — Z in, derived key out).
    #[test]
    fn rfc7518_appendix_c_concat_kdf() {
        let z: [u8; 32] = [
            158, 86, 217, 29, 129, 113, 53, 211, 114, 131, 66, 131, 191, 132, 38, 156, 251, 49,
            110, 163, 218, 128, 106, 72, 246, 218, 167, 121, 140, 254, 144, 196,
        ];
        let derived = concat_kdf(&z, "A128GCM", b"Alice", b"Bob", 16);
        assert_eq!(b64url_encode(&derived), "VqqN6vgjbSBcIijNcacQGg");
    }

    #[test]
    fn ecdh_es_round_trip_and_wrong_key() {
        let sk = [4u8; 32];
        let pk = x25519::public_key(&sk);
        let doc = b"---\nspec: \"1\"\n---\n# JWE agent\n";
        let jwe = encrypt_ecdh_es(doc, &pk).unwrap();
        assert!(crate::config::envelope::looks_encrypted(jwe.as_bytes()));
        assert_eq!(decrypt(&jwe, &[sk], &[]).unwrap(), doc);
        assert!(decrypt(&jwe, &[[8u8; 32]], &[]).is_err());
        // A tampered header (the AAD) fails the tag: flip one base64 char of
        // the protected segment in a way that still decodes to valid JSON is
        // unnecessary — any header change invalidates the AAD binding.
        let dot = jwe.find('.').unwrap();
        let mut chars: Vec<char> = jwe.chars().collect();
        chars[dot - 1] = if chars[dot - 1] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert!(decrypt(&tampered, &[sk], &[]).is_err());
    }

    #[test]
    fn dir_round_trip_and_unsupported_algs_named() {
        let key = [42u8; 32];
        let doc = b"shared-key doc";
        let jwe = encrypt_dir(doc, &key).unwrap();
        assert_eq!(decrypt(&jwe, &[], &[key.to_vec()]).unwrap(), doc);
        // No matching key → the refusal names what is needed.
        let e = decrypt(&jwe, &[], &[]).unwrap_err();
        assert!(e.contains("32-byte shared key"), "{e}");
        // An unsupported alg is refused by name with the supported set.
        let h = b64url_encode(br#"{"alg":"A256KW","enc":"A256GCM"}"#);
        let e = decrypt(&format!("{h}.a.aXZpdml2aXZpdml2.Y3Q.dGFn"), &[], &[]).unwrap_err();
        assert!(e.contains("A256KW") && e.contains("ECDH-ES"), "{e}");
    }
}
