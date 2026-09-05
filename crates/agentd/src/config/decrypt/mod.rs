// SPDX-License-Identifier: AGPL-3.0-only
//! **On-the-fly instruction decryption** (RFC 0041) — the dispatch over the
//! two envelope formats, and the operator's recipient-key material.
//!
//! Confidentiality is end-to-end: the author (or publisher) encrypts to the
//! agent's recipient key; every store and transport in between — a registry,
//! a CDN, an MCP server, a bucket — holds only ciphertext; this module opens
//! the envelope just before the plaintext enters the ordinary instruction
//! pipeline (parse → trust ladder → fold → deliver). Decrypting a document
//! grants it NOTHING: the trust ladder, the trifecta and the §7.8 wire floor
//! apply to the plaintext exactly as if it had arrived clear.
//!
//! Formats: **age v1** ([`agefile`]) and **JWE Compact** ([`jwe`]). Detection
//! lives in the always-compiled [`crate::config::envelope`], so a build
//! WITHOUT this feature still refuses ciphertext by name.

pub mod agefile;
pub mod jwe;
mod scrypt;
pub mod x25519;

use crate::config::envelope::{self, b64url_decode};
use crate::config::v2::Instruction;

/// The loaded recipient-key material, resolved from operator config once.
#[derive(Default)]
pub struct Keys {
    /// X25519 identities (age identities / JWE ECDH-ES recipients).
    pub x25519: Vec<[u8; 32]>,
    /// Pre-shared content keys (JWE `dir`).
    pub shared: Vec<Vec<u8>>,
    /// The passphrase for age scrypt stanzas, already secret-resolved.
    pub passphrase: Option<String>,
}

/// Decrypt `bytes` when they are an encrypted envelope; pass them through
/// untouched when they are not. The single choke point every instruction
/// source (inline, file, MCP resource, OCI blob) routes through.
pub fn maybe_decrypt(bytes: Vec<u8>, cfg: &Instruction) -> Result<Vec<u8>, String> {
    if !envelope::looks_encrypted(&bytes) {
        return Ok(bytes);
    }
    let keys = load_keys(cfg)?;
    if keys.x25519.is_empty() && keys.shared.is_empty() && keys.passphrase.is_none() {
        return Err(
            "the instruction is an encrypted envelope but no recipient key is configured — \
             set instruction.decrypt.keys (and/or instruction.decrypt.passphrase)"
                .to_string(),
        );
    }
    let head = {
        let mut b: &[u8] = &bytes;
        while let Some((f, r)) = b.split_first() {
            if f.is_ascii_whitespace() {
                b = r;
            } else {
                break;
            }
        }
        b
    };
    if head.starts_with(envelope::AGE_MAGIC) || head.starts_with(envelope::AGE_ARMOR) {
        agefile::decrypt(head, &keys.x25519, keys.passphrase.as_deref())
    } else {
        let text = std::str::from_utf8(head).map_err(|_| "jwe: not UTF-8")?;
        jwe::decrypt(text, &keys.x25519, &keys.shared)
    }
}

/// Load the recipient keys the operator configured. Each `keys` entry is a
/// file path (0600-style secret hygiene is the operator's) whose content is
/// one or more of: an `AGE-SECRET-KEY-1…` identity, 64 hex chars, or 43/44
/// chars of base64 — each a 32-byte key usable as BOTH an X25519 identity and
/// a JWE `dir` shared key (the envelope's algorithm decides which it plays).
/// `passphrase` resolves `{{secret:…}}` references before use.
pub fn load_keys(cfg: &Instruction) -> Result<Keys, String> {
    let Some(d) = &cfg.decrypt else {
        return Ok(Keys::default());
    };
    let mut keys = Keys::default();
    for path in &d.keys {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("instruction.decrypt.keys {path}: {e}"))?;
        let mut found = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let key = parse_key_line(line)
                .ok_or_else(|| format!("instruction.decrypt.keys {path}: unrecognized key line"))?;
            keys.shared.push(key.to_vec());
            keys.x25519.push(key);
            found = true;
        }
        if !found {
            return Err(format!(
                "instruction.decrypt.keys {path}: no key material in file"
            ));
        }
    }
    if let Some(p) = &d.passphrase {
        let resolved = crate::sec::secret::resolve(&p.0, &|k| std::env::var(k).ok())
            .map_err(|e| format!("instruction.decrypt.passphrase: {e}"))?;
        keys.passphrase = Some(resolved);
    }
    Ok(keys)
}

fn parse_key_line(line: &str) -> Option<[u8; 32]> {
    if line.to_ascii_uppercase().starts_with("AGE-SECRET-KEY-1") {
        return agefile::parse_identity(line).ok();
    }
    if line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&line[2 * i..2 * i + 2], 16).ok()?;
        }
        return Some(out);
    }
    b64url_decode(line).and_then(|v| v.try_into().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::v2::{Instruction, InstructionDecrypt};

    fn cfg_with_key(dir: &std::path::Path, content: &str) -> Instruction {
        let p = dir.join("key.txt");
        std::fs::write(&p, content).unwrap();
        Instruction {
            decrypt: Some(InstructionDecrypt {
                keys: vec![p.to_string_lossy().into_owned()],
                passphrase: None,
            }),
        }
    }

    #[test]
    fn plaintext_passes_through_untouched() {
        let doc = b"---\nspec: \"1\"\n---\nplain".to_vec();
        let out = maybe_decrypt(doc.clone(), &Instruction::default()).unwrap();
        assert_eq!(out, doc);
    }

    #[test]
    fn an_envelope_with_no_keys_is_refused_naming_the_config() {
        let sk = [2u8; 32];
        let enc = agefile::encrypt(b"x", &[x25519::public_key(&sk)], None).unwrap();
        let e = maybe_decrypt(enc, &Instruction::default()).unwrap_err();
        assert!(e.contains("instruction.decrypt.keys"), "{e}");
    }

    #[test]
    fn age_and_jwe_envelopes_open_with_a_configured_identity() {
        let dir = tempfile::tempdir().unwrap();
        let sk = [6u8; 32];
        let pk = x25519::public_key(&sk);
        let cfg = cfg_with_key(dir.path(), &agefile::encode_identity(&sk));
        let doc = b"---\nspec: \"1\"\n---\n# E2EE\n";

        let enc = agefile::encrypt(doc, &[pk], None).unwrap();
        assert_eq!(maybe_decrypt(enc, &cfg).unwrap(), doc);

        let jwe = jwe::encrypt_ecdh_es(doc, &pk).unwrap();
        assert_eq!(maybe_decrypt(jwe.into_bytes(), &cfg).unwrap(), doc);
    }

    #[test]
    fn hex_and_base64_key_lines_parse_and_dir_works() {
        let dir = tempfile::tempdir().unwrap();
        let key = [42u8; 32];
        let hexline: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let cfg = cfg_with_key(dir.path(), &format!("# a comment\n{hexline}\n"));
        let jwe = jwe::encrypt_dir(b"doc", &key).unwrap();
        assert_eq!(maybe_decrypt(jwe.into_bytes(), &cfg).unwrap(), b"doc");
        // Garbage key material is a config error, not a silent skip.
        let bad = cfg_with_key(dir.path(), "not a key\n");
        let enc = jwe::encrypt_dir(b"doc", &key).unwrap();
        assert!(maybe_decrypt(enc.into_bytes(), &bad).is_err());
    }
}
