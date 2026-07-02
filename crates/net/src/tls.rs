// SPDX-License-Identifier: Apache-2.0
//! TLS wiring for `https://` transports — both directions. [feature: tls]
//!
//! rustls with the **ring** crypto provider (no cmake/C build dep). The
//! **client** side validates against bundled `webpki-roots` (so a scratch
//! container has trust anchors without a system cert store) or, for private
//! deployments, against a pinned CA ([`connect_with_ca`]); the **server** side
//! ([`TlsAcceptor`]) serves a PEM identity and can require client certificates
//! (mutual TLS) against a pinned client CA — the strong identity that mints the
//! management trust domain. Both directions return a `StreamOwned` that is
//! `Read + Write` — it drops straight into the transport-agnostic hand-rolled
//! HTTP machinery ([`crate::http`]).

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::OnceLock;

/// A blocking client-side TLS stream over TCP. `Read + Write`, so it satisfies
/// [`crate::http::Stream`].
pub type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// A blocking server-side TLS stream over an accepted TCP connection.
/// `Read + Write`, so the HTTP server framing runs over it unchanged.
pub type ServerTlsStream = StreamOwned<ServerConnection, TcpStream>;

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
    let config = match identity {
        None => client_config(),
        Some(id) => mtls_config(id)?,
    };
    connect_with_config(tcp, host, config)
}

/// [`connect`] against a **pinned CA** instead of the bundled public roots —
/// for private deployments where the server (a peer agentd, an internal
/// gateway) carries a certificate from an internal CA that never chains to
/// webpki. Trusts ONLY the CA(s) in `ca_pem`. `identity` presents a client
/// certificate (mutual TLS) when `Some`.
pub fn connect_with_ca(
    tcp: TcpStream,
    host: &str,
    ca_pem: &[u8],
    identity: Option<&ClientIdentity>,
) -> io::Result<TlsStream> {
    let roots = roots_from_pem(ca_pem)?;
    let builder =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring provides TLS 1.2 + 1.3")
            .with_root_certificates(roots);
    let config = match identity {
        None => Arc::new(builder.with_no_client_auth()),
        Some(id) => Arc::new(
            builder
                .with_client_auth_cert(id.certs.clone(), id.key.clone_key())
                .map_err(|e| io::Error::other(format!("mtls: bad client identity: {e}")))?,
        ),
    };
    connect_with_config(tcp, host, config)
}

fn connect_with_config(
    tcp: TcpStream,
    host: &str,
    config: Arc<ClientConfig>,
) -> io::Result<TlsStream> {
    let server_name = ServerName::try_from(host)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid TLS server name: {host}"),
            )
        })?
        .to_owned();
    let conn = ClientConnection::new(config, server_name)
        .map_err(|e| io::Error::other(format!("tls: {e}")))?;
    Ok(StreamOwned::new(conn, tcp))
}

/// Parse a PEM bundle of CA certificates into a root store.
fn roots_from_pem(ca_pem: &[u8]) -> io::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let mut added = 0usize;
    for cert in rustls_pemfile::certs(&mut io::Cursor::new(ca_pem)) {
        let cert = cert.map_err(|e| io::Error::other(format!("tls: bad CA PEM: {e}")))?;
        roots
            .add(cert)
            .map_err(|e| io::Error::other(format!("tls: bad CA certificate: {e}")))?;
        added += 1;
    }
    if added == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tls: no CERTIFICATE in CA PEM",
        ));
    }
    Ok(roots)
}

/// A server identity: the certificate chain + private key this listener
/// presents, parsed from mounted PEM (never inline — RFC 0012 §3.7).
pub struct ServerIdentity {
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl std::fmt::Debug for ServerIdentity {
    /// Never prints the private key (RFC 0012 §3.7 secret-freedom).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ServerIdentity {{ certs: {}, key: <redacted> }}",
            self.certs.len()
        )
    }
}

impl ServerIdentity {
    /// Load a server identity from PEM bytes (a cert chain + one private key —
    /// PKCS#8 / PKCS#1 / SEC1). Typically read from mounted secret files.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<ServerIdentity> {
        let certs = rustls_pemfile::certs(&mut io::Cursor::new(cert_pem))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::other(format!("tls: bad server cert PEM: {e}")))?;
        if certs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tls: no CERTIFICATE in server cert PEM",
            ));
        }
        let key = rustls_pemfile::private_key(&mut io::Cursor::new(key_pem))
            .map_err(|e| io::Error::other(format!("tls: bad server key PEM: {e}")))?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tls: no PRIVATE KEY in key PEM",
                )
            })?;
        Ok(ServerIdentity { certs, key })
    }
}

/// A reusable TLS acceptor for a serving listener: wraps each accepted TCP
/// connection in server-side TLS. With a client CA configured (mutual TLS) the
/// handshake REQUIRES a valid client certificate — a peer that presents none
/// (or an unverifiable one) fails the handshake and never reaches the protocol
/// layer; [`ServerTlsStream::conn::peer_certificates`] is then always `Some`
/// for accepted peers, the strong identity the embedder can gate trust on.
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
    client_auth: bool,
}

impl std::fmt::Debug for TlsAcceptor {
    /// Structural only — never key material (RFC 0012 §3.7).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TlsAcceptor {{ client_auth: {} }}", self.client_auth)
    }
}

impl TlsAcceptor {
    /// Build an acceptor from the server identity. `client_ca_pem` enables
    /// mutual TLS: peers must present a certificate chaining to one of the CAs
    /// in the bundle. Without it, any TLS client can connect (transport
    /// encryption only — the embedder must gate trust some other way, e.g. a
    /// bearer credential).
    pub fn new(identity: ServerIdentity, client_ca_pem: Option<&[u8]>) -> io::Result<TlsAcceptor> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("ring provides TLS 1.2 + 1.3");
        let client_auth = client_ca_pem.is_some();
        let builder = match client_ca_pem {
            Some(ca) => {
                let roots = roots_from_pem(ca)?;
                let verifier =
                    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                        .build()
                        .map_err(|e| io::Error::other(format!("tls: bad client CA: {e}")))?;
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        };
        let config = builder
            .with_single_cert(identity.certs, identity.key)
            .map_err(|e| io::Error::other(format!("tls: bad server identity: {e}")))?;
        Ok(TlsAcceptor {
            config: Arc::new(config),
            client_auth,
        })
    }

    /// Whether this acceptor requires client certificates (mutual TLS).
    pub fn requires_client_auth(&self) -> bool {
        self.client_auth
    }

    /// Wrap an accepted TCP connection in server-side TLS, driving the
    /// handshake to completion so failures (including a missing/invalid client
    /// certificate under mTLS) surface HERE, not on the first read.
    pub fn accept(&self, tcp: TcpStream) -> io::Result<ServerTlsStream> {
        let conn = ServerConnection::new(self.config.clone())
            .map_err(|e| io::Error::other(format!("tls: {e}")))?;
        let mut stream = StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            stream
                .conn
                .complete_io(&mut stream.sock)
                .map_err(|e| io::Error::other(format!("tls handshake: {e}")))?;
        }
        Ok(stream)
    }
}

/// Whether the accepted peer presented a (verified) client certificate — under
/// mTLS this is the transport-level identity trust can be minted from.
pub fn peer_presented_cert(stream: &ServerTlsStream) -> bool {
    stream.conn.peer_certificates().is_some()
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
