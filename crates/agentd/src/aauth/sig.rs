// SPDX-License-Identifier: Apache-2.0
//! RFC 9421 HTTP Message Signatures — the signing core AAuth wraps around every
//! MCP request (RFC 0023 §Step 4). We produce the three headers the guide
//! specifies:
//!
//! ```text
//! Signature-Input: sig=("@method" "@authority" "@path" "signature-key");created=<now>
//! Signature: sig=:<base64 ed25519 over the signature base>:
//! Signature-Key: sig=jwt;jwt="<agent_token>"
//! ```
//!
//! Only the derived components `@method`/`@authority`/`@path` plus the
//! `signature-key` param are covered by default (the guide's minimum); a caller
//! that must cover `content-digest` adds it explicitly. The Ed25519 signing is
//! delegated to [`AgentKey`](super::key::AgentKey), so this module is pure,
//! deterministic string assembly (unit-tested against a hand-computed base).

use super::b64;
use super::key::AgentKey;

/// What the `Signature-Key` presents — the guide's `sig=jwt;jwt="…"` for a
/// request (the agent/auth token), or `sig=hwk` presenting the raw public key
/// for the enroll/agent-token calls to the Agent Provider (§Step 1/2).
pub enum SigKey<'a> {
    /// Present a JWT (agent token or user-scoped auth token) as the key id.
    Jwt(&'a str),
    /// Present the raw Ed25519 public key (hardware/holder key scheme) — used
    /// before the agent has a token (enroll) and for the token request itself.
    Hwk,
}

impl SigKey<'_> {
    /// The `Signature-Key` header value (the structured-field member).
    fn header_value(&self, key: &AgentKey) -> String {
        match self {
            SigKey::Jwt(tok) => format!("sig=jwt;jwt=\"{tok}\""),
            // hwk: present the public JWK as a base64url string param so the
            // verifier can check the signature without a prior enrollment.
            SigKey::Hwk => {
                let jwk = serde_json::to_string(&key.public_jwk()).unwrap_or_default();
                format!("sig=hwk;jwk=\"{}\"", b64::url_nopad(jwk.as_bytes()))
            }
        }
    }
}

/// The signature headers for one request. `authority` is the `Host` value
/// (host[:port]); `path` is the request-target path (with query, if any).
/// The `Content-Digest` header value (RFC 9530) for a body:
/// `sha-256=:<base64 sha256>:`. Covered by the signature when the server
/// requires body integrity (RFC 0023 §content-digest).
pub fn content_digest(body: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, body);
    format!("sha-256=:{}:", b64::std_pad(d.as_ref()))
}

/// `created` is unix seconds (clock must be sane, ±60s of the verifier). When
/// `digest` is `Some`, a `Content-Digest` header is added and covered (body
/// integrity, RFC 0023 §content-digest). Returns the header pairs.
pub fn sign_request(
    key: &AgentKey,
    method: &str,
    authority: &str,
    path: &str,
    sigkey: SigKey<'_>,
    created: u64,
    digest: Option<&str>,
) -> Vec<(String, String)> {
    let key_hdr = sigkey.header_value(key);
    // The covered-components list + params (the `Signature-Input` value). The
    // `signature-key` covered component ties the signature to the presented key;
    // `content-digest` is covered only when a body digest is present.
    let covered = if digest.is_some() {
        r#"("@method" "@authority" "@path" "content-digest" "signature-key")"#
    } else {
        r#"("@method" "@authority" "@path" "signature-key")"#
    };
    let sig_params = format!("{covered};created={created}");

    // The signature base (RFC 9421 §2.5): one line per covered component in
    // list order, then the `@signature-params` line with the SAME params.
    let digest_line = digest
        .map(|d| format!("\"content-digest\": {d}\n"))
        .unwrap_or_default();
    let base = format!(
        "\"@method\": {method}\n\
         \"@authority\": {authority}\n\
         \"@path\": {path}\n\
         {digest_line}\
         \"signature-key\": {key_hdr}\n\
         \"@signature-params\": {sig_params}"
    );

    let signature = key.sign(base.as_bytes());
    let mut headers = vec![
        ("Signature-Input".into(), format!("sig={sig_params}")),
        (
            "Signature".into(),
            format!("sig=:{}:", b64::std_pad(&signature)),
        ),
        ("Signature-Key".into(), key_hdr),
    ];
    if let Some(d) = digest {
        headers.push(("Content-Digest".into(), d.to_string()));
    }
    headers
}

/// Unix seconds now (for `created`). Isolated so tests can inject a fixed time.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_base_is_deterministic_and_verifies() {
        let key = AgentKey::from_seed(&[3u8; 32]).unwrap();
        let hdrs = sign_request(
            &key,
            "POST",
            "mcp.example",
            "/mcp",
            SigKey::Jwt("TOK"),
            1_700_000_000,
            None,
        );
        let map: std::collections::HashMap<_, _> = hdrs.iter().cloned().collect();

        // The three headers exist with the expected shapes.
        assert_eq!(
            map["Signature-Input"],
            "sig=(\"@method\" \"@authority\" \"@path\" \"signature-key\");created=1700000000"
        );
        assert_eq!(map["Signature-Key"], "sig=jwt;jwt=\"TOK\"");
        let sig_hdr = &map["Signature"];
        assert!(sig_hdr.starts_with("sig=:") && sig_hdr.ends_with(':'));

        // Reconstruct the base a verifier would build and check the Ed25519 sig.
        let base = "\"@method\": POST\n\
             \"@authority\": mcp.example\n\
             \"@path\": /mcp\n\
             \"signature-key\": sig=jwt;jwt=\"TOK\"\n\
             \"@signature-params\": (\"@method\" \"@authority\" \"@path\" \"signature-key\");created=1700000000";
        let raw = sig_hdr.trim_start_matches("sig=:").trim_end_matches(':');
        let sig = b64::url_decode(raw).unwrap(); // std b64 decodes fine here (no -_)
        let vk =
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key.public_bytes());
        vk.verify(base.as_bytes(), &sig)
            .expect("signature verifies");
    }

    #[test]
    fn content_digest_is_added_and_covered() {
        let key = AgentKey::from_seed(&[5u8; 32]).unwrap();
        let body = br#"{"jsonrpc":"2.0","method":"tools/call"}"#;
        let digest = content_digest(body);
        assert!(digest.starts_with("sha-256=:") && digest.ends_with(':'));
        let hdrs = sign_request(
            &key,
            "POST",
            "mcp.example",
            "/mcp",
            SigKey::Jwt("TOK"),
            1,
            Some(&digest),
        );
        let map: std::collections::HashMap<_, _> = hdrs.iter().cloned().collect();
        // The Content-Digest header is emitted and the input covers it.
        assert_eq!(map["Content-Digest"], digest);
        assert!(map["Signature-Input"].contains("\"content-digest\""));
        // The signature verifies over the digest-covering base.
        let base = format!(
            "\"@method\": POST\n\"@authority\": mcp.example\n\"@path\": /mcp\n\
             \"content-digest\": {digest}\n\"signature-key\": sig=jwt;jwt=\"TOK\"\n\
             \"@signature-params\": (\"@method\" \"@authority\" \"@path\" \"content-digest\" \"signature-key\");created=1"
        );
        let raw = map["Signature"]
            .trim_start_matches("sig=:")
            .trim_end_matches(':');
        let sig = b64::url_decode(raw).unwrap();
        let vk =
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key.public_bytes());
        vk.verify(base.as_bytes(), &sig)
            .expect("digest-covering signature verifies");
    }

    #[test]
    fn hwk_scheme_presents_the_public_jwk() {
        let key = AgentKey::from_seed(&[9u8; 32]).unwrap();
        let hdrs = sign_request(&key, "POST", "apd.example", "/enroll", SigKey::Hwk, 1, None);
        let map: std::collections::HashMap<_, _> = hdrs.iter().cloned().collect();
        assert!(map["Signature-Key"].starts_with("sig=hwk;jwk=\""));
        // The presented jwk decodes back to this key's public JWK.
        let raw = map["Signature-Key"]
            .trim_start_matches("sig=hwk;jwk=\"")
            .trim_end_matches('"');
        let jwk_bytes = b64::url_decode(raw).unwrap();
        let jwk: serde_json::Value = serde_json::from_slice(&jwk_bytes).unwrap();
        assert_eq!(jwk, key.public_jwk());
    }
}
