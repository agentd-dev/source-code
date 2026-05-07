//! Workflow signature verification (RFC 0002).
//!
//! Verifies a detached Ed25519 signature over the raw workflow TOML
//! bytes before the DAG validator runs. Opt-in via the `signing`
//! Cargo feature. When the feature is off, this module compiles to
//! a no-op `verify_or_skip` that accepts every workflow.
//!
//! Design intent: fail-closed when the operator declares
//! `[signing].required = true` (or passes `--signing-required`).
//! Everything else — algorithm selection, keyless Sigstore, ECDSA
//! — is deferred to RFC 0002 v2.

use serde::{Deserialize, Serialize};

/// The workflow's `[signing]` block. All fields optional — callers
/// derive meaning from `required`. When the block is absent from the
/// TOML, this type is not present on the parsed doc.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SigningConfig {
    /// Fail-closed when true: absent signature → refuse to start.
    /// Default false so new workflows can opt in without breaking
    /// existing deployments mid-release.
    #[serde(default)]
    pub required: bool,

    /// Inline PEM-encoded public key. Exactly one of
    /// `public_key_pem` / `public_key_file` must be set if the block
    /// is present.
    #[serde(default)]
    pub public_key_pem: Option<String>,

    /// Filesystem path to a PEM-encoded public key. Resolved
    /// relative to the current working directory at load time.
    #[serde(default)]
    pub public_key_file: Option<std::path::PathBuf>,

    /// Only "ed25519" in v1. Rejected at load if set to anything
    /// else so a stray `algorithm = "ecdsa"` doesn't silently
    /// downgrade to the Ed25519 verifier.
    #[serde(default)]
    pub algorithm: Option<String>,
}

/// Verification outcome, used by the runtime entrypoint.
pub enum Outcome {
    /// Signature verified (or signing was not required and absent).
    Ok,
    /// Signing was required but could not be satisfied. Contains a
    /// reason for the audit event.
    Refused(String),
}

/// Callable surface: "I have a workflow doc and its raw TOML bytes;
/// tell me whether to proceed." `sig_bytes` is the decoded signature
/// (not base64), typically loaded from `<config>.toml.sig`.
#[cfg(feature = "signing")]
pub fn verify_or_skip(
    doc_signing: Option<&SigningConfig>,
    toml_bytes: &[u8],
    signature_source: SignatureSource<'_>,
    force_required: bool,
) -> Outcome {
    match doc_signing {
        Some(cfg) => verifier::verify(cfg, toml_bytes, signature_source, force_required),
        None => {
            if force_required {
                Outcome::Refused(
                    "signing required by CLI/env but workflow declares no [signing] block".into(),
                )
            } else {
                Outcome::Ok
            }
        }
    }
}

/// Stub used when the `signing` feature is off — always accepts.
/// Keeps the call site in `runtime::load_workflow` uniform.
#[cfg(not(feature = "signing"))]
pub fn verify_or_skip(
    doc_signing: Option<&SigningConfig>,
    _toml_bytes: &[u8],
    _signature_source: SignatureSource<'_>,
    force_required: bool,
) -> Outcome {
    if force_required || doc_signing.is_some_and(|c| c.required) {
        Outcome::Refused(
            "workflow requires signature verification but this build \
             lacks the `signing` Cargo feature"
                .into(),
        )
    } else {
        Outcome::Ok
    }
}

/// Where the signature bytes come from. The two shapes mirror the
/// two workflow sources: external `.sig` on disk, or the signature
/// baked in via `AGENTD_EMBED_CONFIG_SIG` at build time.
#[derive(Debug)]
pub enum SignatureSource<'a> {
    /// Path to an external `.sig` file (base64 on disk). Read +
    /// decoded by the verifier.
    FilePath(std::path::PathBuf),
    /// Already decoded raw signature bytes — used for embedded
    /// workflows where `build.rs` baked the bytes via `include_bytes!`.
    RawBytes(&'a [u8]),
    /// No signature provided; the verifier decides whether that's
    /// fatal based on `required`.
    None,
}

#[cfg(feature = "signing")]
mod verifier {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    /// Entry: resolve the pinned key, resolve the signature bytes,
    /// run Ed25519 verification.
    pub(super) fn verify(
        cfg: &SigningConfig,
        toml_bytes: &[u8],
        sig_source: SignatureSource<'_>,
        force_required: bool,
    ) -> Outcome {
        // Algorithm gate. Only Ed25519 in v1; defaulted when absent.
        let alg = cfg.algorithm.as_deref().unwrap_or("ed25519");
        if !alg.eq_ignore_ascii_case("ed25519") {
            return audit_refuse(
                cfg,
                "unsupported",
                format!(
                    "signing.algorithm `{alg}` not recognised in this build (only `ed25519` is supported)"
                ),
            );
        }

        let required = cfg.required || force_required;

        // Resolve signature bytes.
        let sig_bytes = match resolve_signature(&sig_source) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                if required {
                    return audit_refuse(
                        cfg,
                        "sig_missing",
                        "signature file missing and signing is required".into(),
                    );
                }
                // Soft-warn path — declared but not enforced.
                tracing::warn!(
                    target: "agentd::audit",
                    event = "signing.bypassed",
                    reason = "signature absent; [signing].required = false",
                );
                return Outcome::Ok;
            }
            Err(e) => {
                return audit_refuse(cfg, "sig_malformed", format!("signature read failed: {e}"));
            }
        };

        // Resolve public key.
        let pubkey = match load_public_key(cfg) {
            Ok(k) => k,
            Err(e) => {
                return audit_refuse(cfg, "pubkey_malformed", e);
            }
        };
        let fingerprint = fingerprint_hex(pubkey.as_bytes());

        // Enforce Ed25519 signature length (64 bytes).
        let sig = match sig_bytes.as_slice().try_into() {
            Ok(arr) => Signature::from_bytes(arr),
            Err(_) => {
                return audit_refuse_fp(
                    cfg,
                    &fingerprint,
                    "sig_malformed",
                    format!(
                        "signature is {} bytes; Ed25519 requires exactly 64",
                        sig_bytes.len()
                    ),
                );
            }
        };

        // Verify.
        match pubkey.verify(toml_bytes, &sig) {
            Ok(()) => {
                tracing::info!(
                    target: "agentd::audit",
                    event = "signing.verified",
                    key_fingerprint = %fingerprint,
                );
                Outcome::Ok
            }
            Err(e) => audit_refuse_fp(
                cfg,
                &fingerprint,
                "verification_failed",
                format!("ed25519 verify: {e}"),
            ),
        }
    }

    fn resolve_signature(source: &SignatureSource<'_>) -> Result<Option<Vec<u8>>, String> {
        match source {
            SignatureSource::None => Ok(None),
            SignatureSource::RawBytes(bytes) => {
                if bytes.is_empty() {
                    Ok(None)
                } else {
                    // Embedded path pre-decodes; accept as-is.
                    Ok(Some(bytes.to_vec()))
                }
            }
            SignatureSource::FilePath(path) => {
                if !path.exists() {
                    return Ok(None);
                }
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| format!("open {}: {e}", path.display()))?;
                // The file is base64 with optional trailing newline.
                let trimmed = raw.trim();
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(trimmed)
                    .map_err(|e| format!("base64 decode {}: {e}", path.display()))?;
                Ok(Some(decoded))
            }
        }
    }

    fn load_public_key(cfg: &SigningConfig) -> Result<VerifyingKey, String> {
        let pem = match (&cfg.public_key_pem, &cfg.public_key_file) {
            (Some(inline), None) => inline.clone(),
            (None, Some(path)) => std::fs::read_to_string(path)
                .map_err(|e| format!("read public_key_file {}: {e}", path.display()))?,
            (Some(_), Some(_)) => {
                return Err(
                    "exactly one of signing.public_key_pem / signing.public_key_file may be set"
                        .into(),
                );
            }
            (None, None) => {
                return Err(
                    "signing block present but no public_key_pem / public_key_file configured"
                        .into(),
                );
            }
        };

        // Accept two PEM shapes: Ed25519 SPKI (the standard
        // `-----BEGIN PUBLIC KEY-----` container) and bare
        // `-----BEGIN ED25519 PUBLIC KEY-----`. OpenSSL emits SPKI
        // by default (`openssl pkey -pubout`) so that's the happy
        // path.
        use ed25519_dalek::pkcs8::DecodePublicKey;
        VerifyingKey::from_public_key_pem(&pem).map_err(|e| format!("parse public key PEM: {e}"))
    }

    fn fingerprint_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut out = String::with_capacity(16);
        for b in digest.iter().take(8) {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    fn audit_refuse(cfg: &SigningConfig, event_kind: &'static str, msg: String) -> Outcome {
        tracing::error!(
            target: "agentd::audit",
            event = format!("signing.{event_kind}").as_str(),
            required = cfg.required,
            reason = %msg,
        );
        Outcome::Refused(msg)
    }

    fn audit_refuse_fp(
        cfg: &SigningConfig,
        fingerprint: &str,
        event_kind: &'static str,
        msg: String,
    ) -> Outcome {
        tracing::error!(
            target: "agentd::audit",
            event = format!("signing.{event_kind}").as_str(),
            required = cfg.required,
            key_fingerprint = fingerprint,
            reason = %msg,
        );
        Outcome::Refused(msg)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "signing"))]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::pkcs8::{EncodePublicKey, spki::der::pem::LineEnding};
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair() -> SigningKey {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        SigningKey::from_bytes(&bytes)
    }

    fn pubkey_pem(k: &SigningKey) -> String {
        k.verifying_key().to_public_key_pem(LineEnding::LF).unwrap()
    }

    fn sign(k: &SigningKey, msg: &[u8]) -> Vec<u8> {
        k.sign(msg).to_bytes().to_vec()
    }

    fn encode_sig(raw: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    #[test]
    fn verifies_correct_signature() {
        let key = keypair();
        let toml = b"name = \"t\"\n";
        let sig = sign(&key, toml);
        let cfg = SigningConfig {
            required: true,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: None,
            algorithm: None,
        };
        let source = SignatureSource::RawBytes(&sig);
        assert!(matches!(
            verify_or_skip(Some(&cfg), toml, source, false),
            Outcome::Ok
        ));
    }

    #[test]
    fn refuses_tampered_bytes() {
        let key = keypair();
        let sig = sign(&key, b"original");
        let cfg = SigningConfig {
            required: true,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: None,
            algorithm: None,
        };
        match verify_or_skip(
            Some(&cfg),
            b"tampered",
            SignatureSource::RawBytes(&sig),
            false,
        ) {
            Outcome::Refused(_) => {}
            _ => panic!("should refuse"),
        }
    }

    #[test]
    fn refuses_wrong_signature_length() {
        let key = keypair();
        let cfg = SigningConfig {
            required: true,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: None,
            algorithm: None,
        };
        let short = vec![0u8; 32];
        match verify_or_skip(Some(&cfg), b"hi", SignatureSource::RawBytes(&short), false) {
            Outcome::Refused(m) => assert!(m.contains("Ed25519 requires exactly 64")),
            _ => panic!("should refuse"),
        }
    }

    #[test]
    fn refuses_missing_sig_when_required() {
        let key = keypair();
        let cfg = SigningConfig {
            required: true,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: None,
            algorithm: None,
        };
        match verify_or_skip(Some(&cfg), b"x", SignatureSource::None, false) {
            Outcome::Refused(m) => assert!(m.contains("required")),
            _ => panic!("should refuse"),
        }
    }

    #[test]
    fn soft_warns_when_not_required_and_sig_missing() {
        let key = keypair();
        let cfg = SigningConfig {
            required: false,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: None,
            algorithm: None,
        };
        assert!(matches!(
            verify_or_skip(Some(&cfg), b"x", SignatureSource::None, false),
            Outcome::Ok
        ));
    }

    #[test]
    fn refuses_unsupported_algorithm() {
        let key = keypair();
        let cfg = SigningConfig {
            required: true,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: None,
            algorithm: Some("ecdsa-p256".into()),
        };
        match verify_or_skip(Some(&cfg), b"x", SignatureSource::None, false) {
            Outcome::Refused(m) => assert!(m.contains("ecdsa-p256")),
            _ => panic!("should refuse"),
        }
    }

    #[test]
    fn force_required_overrides_absent_signing_block() {
        match verify_or_skip(None, b"x", SignatureSource::None, true) {
            Outcome::Refused(m) => assert!(m.contains("CLI/env")),
            _ => panic!("should refuse"),
        }
    }

    #[test]
    fn absent_signing_block_is_ok_when_not_forced() {
        assert!(matches!(
            verify_or_skip(None, b"x", SignatureSource::None, false),
            Outcome::Ok
        ));
    }

    #[test]
    fn rejects_both_pem_and_file() {
        let key = keypair();
        let cfg = SigningConfig {
            required: true,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: Some("/nope".into()),
            algorithm: None,
        };
        let sig = sign(&key, b"x");
        match verify_or_skip(Some(&cfg), b"x", SignatureSource::RawBytes(&sig), false) {
            Outcome::Refused(m) => assert!(m.contains("exactly one")),
            _ => panic!("should refuse"),
        }
    }

    #[test]
    fn file_path_base64_roundtrip() {
        let key = keypair();
        let toml = b"name = \"p\"\n";
        let sig = sign(&key, toml);
        let b64 = encode_sig(&sig);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("workflow.toml.sig");
        std::fs::write(&path, b64).unwrap();

        let cfg = SigningConfig {
            required: true,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: None,
            algorithm: None,
        };
        match verify_or_skip(Some(&cfg), toml, SignatureSource::FilePath(path), false) {
            Outcome::Ok => {}
            Outcome::Refused(m) => panic!("should pass: {m}"),
        }
    }

    #[test]
    fn rejects_malformed_base64_in_file() {
        let key = keypair();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("workflow.toml.sig");
        std::fs::write(&path, b"!!! not base64").unwrap();

        let cfg = SigningConfig {
            required: true,
            public_key_pem: Some(pubkey_pem(&key)),
            public_key_file: None,
            algorithm: None,
        };
        match verify_or_skip(Some(&cfg), b"x", SignatureSource::FilePath(path), false) {
            Outcome::Refused(m) => assert!(m.contains("base64")),
            _ => panic!("should refuse"),
        }
    }
}
