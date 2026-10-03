// SPDX-License-Identifier: MIT OR Apache-2.0
//! **§7.2 digests** — `sha256:<hex>`, in every build.
//!
//! A digest is not crypto policy: the §7.4 resolution manifest every delivery
//! produces is made of them, and a manifest whose digests depended on a
//! feature would be two manifests for one delivery — the skew S7 exists to
//! end. So SHA-256 (`ring`, already resolved by any TLS consumer) is a normal
//! dependency, and the `sign` feature gates only what verifies signatures.

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

/// The author digest (§7.2): the stored bytes with the front-matter
/// `signature:` line excluded, so the author JWS can ride in its own
/// document. End matter is authored bytes like any other, so it is included.
pub fn author_digest(doc: &[u8]) -> String {
    digest(&strip_front_matter_signature(doc))
}

/// The bytes with exactly the top-level front-matter `signature:` line
/// removed; with no front matter there is nothing to remove.
pub fn strip_front_matter_signature(doc: &[u8]) -> Vec<u8> {
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

/// The front-matter `id`, which an attestation's `doc` claim must equal (§3.1).
pub fn front_matter_id(doc: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(doc).ok()?;
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end].split('\n').find_map(|l| {
        l.strip_prefix("id:")
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_sha256_hex() {
        assert_eq!(
            digest(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn the_author_digest_excludes_the_signature_line_and_keeps_end_matter() {
        let doc = "---\nspec: \"1\"\nid: instruction://ins_x\n---\nbody\n\n---\nowners: [a]\n---\n";
        let signed = doc.replacen("---\nbody", "signature: a.b.c\n---\nbody", 1);
        assert_ne!(signed, doc);
        assert_eq!(
            author_digest(signed.as_bytes()),
            author_digest(doc.as_bytes())
        );
        assert_eq!(author_digest(doc.as_bytes()), digest(doc.as_bytes()));
        assert_eq!(
            front_matter_id(signed.as_bytes()).as_deref(),
            Some("instruction://ins_x")
        );
    }
}
