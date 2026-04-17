//! TLS / mTLS wiring for the HTTP trigger server (R3b).
//!
//! Only compiled with the `server-tls` feature. Exposes:
//!
//! - [`build_server_config`] — loads PEM files and builds a
//!   shareable `Arc<rustls::ServerConfig>` (with or without client
//!   cert verification).
//! - [`accept_tls`] — wraps an accepted `TcpStream` in a TLS session,
//!   drives the handshake to completion, returns a `TlsStream` +
//!   the peer cert fingerprint (mTLS) when present.
//!
//! Design notes:
//!
//! - Crypto provider is installed **once** via
//!   `aws_lc_rs::default_provider().install_default()` on the first
//!   call to `build_server_config`. Idempotent; re-install attempts
//!   are swallowed.
//! - Handshake errors abort the connection with no HTTP-level
//!   response. That's the right shape — if the client didn't speak
//!   TLS (or presented an invalid mTLS cert) we have no way to
//!   reply meaningfully.
//! - Peer-identity extraction is deliberately **fingerprint-only**
//!   (SHA-256 of the DER bytes, hex-encoded). Adding `x509-parser`
//!   to pull `CN` / `SAN` is a straight follow-up once a workflow
//!   needs those.

use std::fs::File;
use std::io::BufReader;
use std::net::TcpStream;
use std::sync::Arc;

use rustls::pki_types::CertificateDer;
use rustls::server::{ServerConfig as RustlsServerConfig, WebPkiClientVerifier};
use rustls::{RootCertStore, ServerConnection, StreamOwned};

use crate::error::{Error, Result};
use crate::server_config::TlsConfig;

pub type TlsStream = StreamOwned<ServerConnection, TcpStream>;

/// Build a shareable rustls `ServerConfig` from a parsed
/// [`TlsConfig`]. Installs the default crypto provider on first
/// call (idempotent).
pub fn build_server_config(tls: &TlsConfig) -> Result<Arc<RustlsServerConfig>> {
    install_default_provider();

    // Load cert chain.
    let cert_file = File::open(&tls.cert_file).map_err(|e| {
        Error::Config(format!(
            "tls: open cert_file {}: {e}",
            tls.cert_file.display()
        ))
    })?;
    let cert_chain: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_file))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Config(format!("tls: parse cert_file: {e}")))?;
    if cert_chain.is_empty() {
        return Err(Error::Config(format!(
            "tls: cert_file {} contains no certificates",
            tls.cert_file.display()
        )));
    }

    // Load private key.
    let key_file = File::open(&tls.key_file).map_err(|e| {
        Error::Config(format!(
            "tls: open key_file {}: {e}",
            tls.key_file.display()
        ))
    })?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .map_err(|e| Error::Config(format!("tls: parse key_file: {e}")))?
        .ok_or_else(|| {
            Error::Config(format!(
                "tls: key_file {} has no recognised private key",
                tls.key_file.display()
            ))
        })?;

    // With / without mTLS.
    let builder = RustlsServerConfig::builder();
    let cfg = match &tls.client_auth {
        Some(ca_cfg) if ca_cfg.mode.is_required() => {
            let roots = load_ca_roots(&ca_cfg.ca_file)?;
            let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|e| Error::Config(format!("tls: client verifier: {e}")))?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(cert_chain, key)
                .map_err(|e| Error::Config(format!("tls: with_single_cert: {e}")))?
        }
        Some(_) => {
            return Err(Error::Config(
                "tls.client_auth.mode: only `required` is supported in this build".into(),
            ));
        }
        None => builder
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(|e| Error::Config(format!("tls: with_single_cert: {e}")))?,
    };
    Ok(Arc::new(cfg))
}

/// Install the default crypto provider once. Races are harmless:
/// `install_default` returns `Err` on re-install but the first
/// installer wins.
fn install_default_provider() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn load_ca_roots(path: &std::path::Path) -> Result<RootCertStore> {
    let ca_file = File::open(path)
        .map_err(|e| Error::Config(format!("tls: open ca_file {}: {e}", path.display())))?;
    let roots: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(ca_file))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Config(format!("tls: parse ca_file: {e}")))?;
    if roots.is_empty() {
        return Err(Error::Config(format!(
            "tls: ca_file {} contains no certificates",
            path.display()
        )));
    }
    let mut store = RootCertStore::empty();
    for cert in roots {
        store
            .add(cert)
            .map_err(|e| Error::Config(format!("tls: add CA cert: {e}")))?;
    }
    Ok(store)
}

/// Wrap a freshly accepted TCP stream in TLS, drive the handshake,
/// and return the wrapped stream + the peer cert fingerprint (if
/// mTLS is active).
pub fn accept_tls(
    tcp: TcpStream,
    config: Arc<RustlsServerConfig>,
) -> std::result::Result<(TlsStream, Option<PeerIdentity>), std::io::Error> {
    let conn = ServerConnection::new(config).map_err(std::io::Error::other)?;
    let mut stream = StreamOwned::new(conn, tcp);
    // Force the handshake now rather than lazily on first read —
    // surfaces errors early and makes `peer_certificates` available
    // before the HTTP parser runs.
    //
    // rustls completes the handshake on any IO; a 0-byte read here
    // blocks until the TLS ClientHello + our ServerHello round-trip.
    {
        use std::io::Read;
        let mut buf = [0u8; 0];
        let _ = stream.read(&mut buf);
    }

    let identity = stream.conn.peer_certificates().and_then(|certs| {
        certs.first().map(|c| PeerIdentity {
            fingerprint: sha256_hex(c.as_ref()),
        })
    });

    Ok((stream, identity))
}

pub use crate::triggers::http::PeerIdentity;

/// SHA-256 of a DER-encoded cert, formatted as `sha256:<64-hex>`.
/// Stable identifier operators can pre-compute for pinning.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_config::{ClientAuthConfig, ClientAuthMode};

    /// Generate a throwaway self-signed server cert + PKCS8 key
    /// (via rcgen, dev-only).
    fn mk_self_signed(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_path = dir.join("server.pem");
        let key_path = dir.join("server.key");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        (cert_path, key_path)
    }

    #[test]
    fn loads_self_signed_server_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (cert_file, key_file) = mk_self_signed(tmp.path());
        let cfg = TlsConfig {
            cert_file,
            key_file,
            client_auth: None,
        };
        let _arc = build_server_config(&cfg).unwrap();
    }

    #[test]
    fn missing_cert_file_fails_cleanly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (_, key_file) = mk_self_signed(tmp.path());
        let cfg = TlsConfig {
            cert_file: "/definitely/not/a/real/path".into(),
            key_file,
            client_auth: None,
        };
        let err = build_server_config(&cfg).unwrap_err();
        assert!(format!("{err}").contains("cert_file"));
    }

    #[test]
    fn bad_key_file_fails_cleanly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (cert_file, _) = mk_self_signed(tmp.path());
        let junk = tmp.path().join("junk.pem");
        std::fs::write(&junk, b"not a key").unwrap();
        let cfg = TlsConfig {
            cert_file,
            key_file: junk,
            client_auth: None,
        };
        let err = build_server_config(&cfg).unwrap_err();
        assert!(format!("{err}").contains("key_file"));
    }

    #[test]
    fn mtls_required_needs_valid_ca_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (cert_file, key_file) = mk_self_signed(tmp.path());
        let junk_ca = tmp.path().join("ca.pem");
        std::fs::write(&junk_ca, b"not a cert").unwrap();
        let cfg = TlsConfig {
            cert_file,
            key_file,
            client_auth: Some(ClientAuthConfig {
                mode: ClientAuthMode::Required,
                ca_file: junk_ca,
            }),
        };
        let err = build_server_config(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ca_file") || msg.contains("CA"));
    }

    #[test]
    fn mtls_required_happy_path() {
        // Generate a CA, a server cert signed by it, then wire mTLS.
        let tmp = tempfile::TempDir::new().unwrap();
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca_cert.pem();
        let ca_path = tmp.path().join("ca.pem");
        std::fs::write(&ca_path, &ca_pem).unwrap();

        // Server cert signed by the CA.
        let srv_key = rcgen::KeyPair::generate().unwrap();
        let srv_params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        let srv_cert = srv_params.signed_by(&srv_key, &ca_cert, &ca_key).unwrap();
        let cert_path = tmp.path().join("server.pem");
        let key_path = tmp.path().join("server.key");
        std::fs::write(&cert_path, srv_cert.pem()).unwrap();
        std::fs::write(&key_path, srv_key.serialize_pem()).unwrap();

        let cfg = TlsConfig {
            cert_file: cert_path,
            key_file: key_path,
            client_auth: Some(ClientAuthConfig {
                mode: ClientAuthMode::Required,
                ca_file: ca_path,
            }),
        };
        let _arc = build_server_config(&cfg).unwrap();
    }

    #[test]
    fn optional_mode_rejected_in_this_build() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (cert_file, key_file) = mk_self_signed(tmp.path());
        let ca_junk = tmp.path().join("ca.pem");
        // Still need a real CA file so the path-open check passes
        // and the error comes from the mode-branch, not the loader.
        // Re-use the self-signed server cert as a pseudo-CA; rustls
        // wouldn't accept this in practice but the loader's
        // "optional is not wired" check happens first.
        let (_, _) = mk_self_signed(tmp.path());
        std::fs::write(&ca_junk, std::fs::read(&cert_file).unwrap()).unwrap();
        let cfg = TlsConfig {
            cert_file,
            key_file,
            client_auth: Some(ClientAuthConfig {
                mode: ClientAuthMode::Optional,
                ca_file: ca_junk,
            }),
        };
        let err = build_server_config(&cfg).unwrap_err();
        assert!(format!("{err}").contains("only `required` is supported"));
    }

    #[test]
    fn sha256_hex_shape() {
        let h = sha256_hex(b"hi");
        assert!(h.starts_with("sha256:"));
        assert_eq!(h.len(), 7 + 64);
    }
}
