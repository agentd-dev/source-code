// SPDX-License-Identifier: Apache-2.0
//! TLS client wiring for `https://` intelligence/MCP. RFC 0006 §transports.
//! [feature: tls]
//!
//! rustls with the **ring** crypto provider (no cmake/C build dep) + bundled
//! `webpki-roots` (so a scratch container has trust anchors without a system
//! cert store). Returns a `StreamOwned` that is `Read + Write` — it drops
//! straight into the transport-agnostic hand-rolled HTTP client
//! ([`crate::net::http`]). The recommended container shape still terminates TLS
//! at a sidecar (unix transport), so most builds link none of this.

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::OnceLock;

/// A blocking TLS stream over TCP. `Read + Write`, so it satisfies
/// [`crate::net::http::Stream`].
pub type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// Wrap an established TCP connection in TLS, validating the server cert
/// against the bundled roots for `host` (which must be the SNI / cert name).
pub fn connect(tcp: TcpStream, host: &str) -> io::Result<TlsStream> {
    let server_name = ServerName::try_from(host)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid TLS server name: {host}"),
            )
        })?
        .to_owned();
    let conn = ClientConnection::new(client_config(), server_name)
        .map_err(|e| io::Error::other(format!("tls: {e}")))?;
    Ok(StreamOwned::new(conn, tcp))
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
