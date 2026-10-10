// SPDX-License-Identifier: AGPL-3.0-only
//! **Encrypted-envelope detection** (RFC 0041) — always compiled, no crypto.
//!
//! Detection is separate from decryption on purpose: a binary built WITHOUT
//! `--features decrypt` must still *recognize* an encrypted instruction and
//! refuse it by name, never fall through to treating ciphertext as prose.
//! (Fail-safe: the worst outcome of a false positive is a clear refusal; the
//! worst outcome of a miss is garbage delivered to a model as its instruction.)

/// The age v1 binary header magic (age-encryption.org/v1).
pub const AGE_MAGIC: &[u8] = b"age-encryption.org/v1";
/// The age ASCII-armor opening line.
pub const AGE_ARMOR: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----";

/// Whether these bytes are an encrypted envelope this runtime knows the shape
/// of: an age v1 file (binary or armored), or a JWE Compact Serialization
/// (five dot-separated base64url segments whose protected header decodes to a
/// JSON object carrying `enc`).
pub fn looks_encrypted(bytes: &[u8]) -> bool {
    let head = trim_start(bytes);
    head.starts_with(AGE_MAGIC) || head.starts_with(AGE_ARMOR) || looks_like_jwe(head)
}

fn trim_start(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\n' || b[i] == b'\r' || b[i] == b'\t') {
        i += 1;
    }
    &b[i..]
}

/// A JWE compact serialization: ASCII, exactly five `.`-separated segments,
/// each non-empty base64url (the ciphertext may be long — only the shape and
/// the header are inspected), and the first segment decodes to a JSON object
/// with an `enc` member (which a JWS — three segments, `alg` only — never has).
fn looks_like_jwe(bytes: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(bytes) else {
        return false;
    };
    let s = s.trim();
    let parts: Vec<&str> = s.split('.').collect();
    // header.encrypted_key.iv.ciphertext.tag — the encrypted key is EMPTY for
    // `dir` and `ECDH-ES` (direct key agreement), and only those three of the
    // five are always present.
    if parts.len() != 5 || parts[0].is_empty() || parts[3].is_empty() || parts[4].is_empty() {
        return false;
    }
    let b64url = |p: &str| {
        p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !parts.iter().all(|p| b64url(p)) {
        return false;
    }
    let Ok(header) = crate::b64::url_decode(parts[0]) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&header) else {
        return false;
    };
    v.get("enc").is_some()
}

/// Wrap a BINARY age file in its standard ASCII armor, so an envelope can
/// travel through string-shaped config (`agent.instruction`) losslessly. The
/// output is exactly what `age --armor` produces: std base64 with padding,
/// 64-column lines, between the BEGIN/END markers.
pub fn armor(bytes: &[u8]) -> String {
    let b64 = crate::b64::std_pad(bytes);
    let mut out = String::from("-----BEGIN AGE ENCRYPTED FILE-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str("-----END AGE ENCRYPTED FILE-----\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_recognizes_the_three_shapes_and_nothing_else() {
        assert!(looks_encrypted(b"age-encryption.org/v1\n-> X25519 abc\n"));
        assert!(looks_encrypted(
            b"-----BEGIN AGE ENCRYPTED FILE-----\nabc\n"
        ));
        // A JWE: header {"alg":"dir","enc":"A256GCM"} = eyJhbGciOiJkaXIiLCJlbmMiOiJBMjU2R0NNIn0
        assert!(looks_encrypted(
            b"eyJhbGciOiJkaXIiLCJlbmMiOiJBMjU2R0NNIn0..aXZpdml2aXZpdml2.Y3Q.dGFn"
        ));
        // …but an empty ciphertext fails the shape.
        assert!(!looks_encrypted(
            b"eyJhbGciOiJkaXIiLCJlbmMiOiJBMjU2R0NNIn0..aXY..dGFn"
        ));
        // A JWS (three segments) is NOT an envelope.
        assert!(!looks_encrypted(
            b"eyJhbGciOiJFZERTQSJ9.eyJzcGVjIjoiMSJ9.c2ln"
        ));
        // Ordinary documents are not.
        assert!(!looks_encrypted(b"---\nspec: \"1\"\n---\n# Agent\n"));
        assert!(!looks_encrypted(b"Just prose. With. Dots. In. It."));
        assert!(!looks_encrypted(b""));
    }
}
