// SPDX-License-Identifier: Apache-2.0
//! TLS client wiring for `https://` intelligence/MCP. RFC 0006 §transports.
//! [feature: tls]
//!
//! rustls with the **ring** crypto provider (no cmake/C build dep) + bundled
//! `webpki-roots` (so a scratch container has trust anchors without a system
//! cert store). Returns a `StreamOwned` that is `Read + Write` — it drops
//! straight into the transport-agnostic hand-rolled HTTP client
//! ([`crate::http`]). The recommended container shape still terminates TLS
//! at a sidecar (unix transport), so most builds link none of this.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::OnceLock;

/// A blocking TLS stream over TCP. `Read + Write`, so it satisfies
/// [`crate::http::Stream`].
pub type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// A resolved client identity for mutual TLS: a cert chain + private key, parsed
/// from mounted PEM (never inline; RFC 0012 §3.7 secret-freedom).
#[derive(Clone)]
pub struct ClientIdentity {
    certs: Vec<CertificateDer<'static>>,
    key: Arc<PrivateKeyDer<'static>>,
}

impl std::fmt::Debug for ClientIdentity {
    /// Never prints the private key (RFC 0012 §3.7 secret-freedom).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ClientIdentity {{ certs: {}, key: <redacted> }}",
            self.certs.len()
        )
    }
}

impl ClientIdentity {
    /// Load a client identity from PEM bytes (a cert chain + one private key —
    /// PKCS#8 / PKCS#1 / SEC1). Typically read from mounted secret files.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<ClientIdentity> {
        let certs = rustls_pemfile::certs(&mut io::Cursor::new(cert_pem))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::other(format!("mtls: bad client cert PEM: {e}")))?;
        if certs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mtls: no CERTIFICATE in client cert PEM",
            ));
        }
        let key = rustls_pemfile::private_key(&mut io::Cursor::new(key_pem))
            .map_err(|e| io::Error::other(format!("mtls: bad client key PEM: {e}")))?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "mtls: no PRIVATE KEY in key PEM",
                )
            })?;
        Ok(ClientIdentity {
            certs,
            key: Arc::new(key),
        })
    }
}

/// Wrap an established TCP connection in TLS, validating the server cert
/// against the bundled roots for `host` (which must be the SNI / cert name).
/// `identity` presents a client certificate (mutual TLS) when `Some`.
pub fn connect(
    tcp: TcpStream,
    host: &str,
    identity: Option<&ClientIdentity>,
) -> io::Result<TlsStream> {
    let server_name = ServerName::try_from(host)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid TLS server name: {host}"),
            )
        })?
        .to_owned();
    let config = match identity {
        None => client_config(),
        Some(id) => mtls_config(id)?,
    };
    let conn = ClientConnection::new(config, server_name)
        .map_err(|e| io::Error::other(format!("tls: {e}")))?;
    Ok(StreamOwned::new(conn, tcp))
}

/// Build a per-identity mTLS config (the same root store + ring provider, plus
/// the client cert/key). Not cached — client identities vary per endpoint and
/// connections are infrequent.
fn mtls_config(id: &ClientIdentity) -> io::Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let key = id.key.clone_key();
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring provides TLS 1.2 + 1.3")
            .with_root_certificates(roots)
            .with_client_auth_cert(id.certs.clone(), key)
            .map_err(|e| io::Error::other(format!("mtls: bad client identity: {e}")))?;
    Ok(Arc::new(config))
}

/// Build the client config once (root store + ring provider) and reuse it.
fn client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring provides TLS 1.2 + 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_identity_rejects_pem_without_cert_or_key() {
        // No CERTIFICATE block.
        let err = ClientIdentity::from_pem(b"not a pem", b"not a pem").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // A cert but a keyless key PEM.
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        let err = ClientIdentity::from_pem(cert.as_bytes(), b"").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
