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

/// Process-wide EXTRA trust anchors for outbound TLS, installed once at startup
/// ([`install_extra_ca`]) and honored by every client-side dial in this process
/// alongside the bundled webpki roots. This is deliberately process state, not a
/// per-connection parameter: a trust store is process-scoped (it is the private
/// deployment's replacement for the system CA bundle a scratch container lacks),
/// while per-peer material (a [`ClientIdentity`]) stays a parameter.
static EXTRA_CA: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();

/// Install additional PEM CA certificate(s) — an internal / cluster-local CA —
/// as process-wide trust anchors for **outbound** TLS, ADDED to the bundled
/// webpki roots. Returns the number of anchors installed.
///
/// Call once, at startup, **before the first outbound dial** (the default
/// no-identity client config is built once and cached; anchors installed after
/// that first dial are not retrofitted onto it). A second call with the *same*
/// bundle is an idempotent no-op; a second call with a *different* bundle is an
/// error — trust anchors are set-once, restart to change them.
pub fn install_extra_ca(ca_pem: &[u8]) -> io::Result<usize> {
    // Parse AND prove addability now (roots_from_pem add-validates each cert),
    // so a bad bundle fails fast at startup, never at a dial site.
    validate_ca_pem(ca_pem)?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut io::Cursor::new(ca_pem))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::other(format!("tls: bad CA PEM: {e}")))?;
    let n = certs.len();
    match EXTRA_CA.set(certs) {
        Ok(()) => Ok(n),
        Err(rejected) => {
            if EXTRA_CA.get().map(Vec::as_slice) == Some(rejected.as_slice()) {
                Ok(n) // same bundle re-installed — idempotent
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "tls: extra CA already installed with a different bundle (set-once; restart to change trust anchors)",
                ))
            }
        }
    }
}

/// The number of extra trust anchors installed (0 = pure webpki), for
/// startup logging / diagnostics.
pub fn extra_ca_count() -> usize {
    EXTRA_CA.get().map_or(0, Vec::len)
}

/// Side-effect-free content check for a CA PEM bundle (parseable + every cert
/// addable as a trust anchor) — the `--validate-config` half of
/// [`install_extra_ca`], which performs exactly this before installing.
/// Returns the anchor count.
pub fn validate_ca_pem(ca_pem: &[u8]) -> io::Result<usize> {
    roots_from_pem(ca_pem).map(|r| r.len())
}

/// Extend a root store with the installed extra anchors (no-op when none).
/// Anchors were add-validated at install, so failures here are unreachable;
/// they are ignored rather than panicking on the dial path.
fn extend_with_extra_ca(roots: &mut RootCertStore) {
    for cert in EXTRA_CA.get().into_iter().flatten() {
        let _ = roots.add(cert.clone());
    }
}

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
///
/// Built [`from_paths`](TlsAcceptor::from_paths), the acceptor is **live**: it
/// re-stats the PEM files (throttled) on accept and rebuilds its config when
/// they change — so a mounted-Secret rotation (cert-manager renewal swaps the
/// file atomically) is picked up with no restart and no dropped listener. A
/// failed reload keeps serving the last-good identity (observable via
/// [`last_reload_error`](TlsAcceptor::last_reload_error)).
pub struct TlsAcceptor {
    config: Mutex<Arc<ServerConfig>>,
    client_auth: bool,
    reload: Option<ReloadSource>,
}

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// How often, at most, an accept re-stats the identity files (throttles the
/// stat syscalls under high accept rates; rotation cadence is months).
const RELOAD_CHECK_TTL: Duration = Duration::from_secs(1);

/// The file-backed identity a live acceptor watches.
struct ReloadSource {
    cert: PathBuf,
    key: PathBuf,
    client_ca: Option<PathBuf>,
    state: Mutex<ReloadState>,
}

struct ReloadState {
    checked_at: Instant,
    mtimes: (SystemTime, SystemTime, Option<SystemTime>),
    generation: u64,
    last_error: Option<String>,
}

impl std::fmt::Debug for TlsAcceptor {
    /// Structural only — never key material (RFC 0012 §3.7).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TlsAcceptor {{ client_auth: {}, live: {} }}",
            self.client_auth,
            self.reload.is_some()
        )
    }
}

/// Build a server config from identity + optional client-CA PEM — shared by the
/// static constructor and every live reload.
fn build_server_config(
    identity: ServerIdentity,
    client_ca_pem: Option<&[u8]>,
) -> io::Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring provides TLS 1.2 + 1.3");
    let builder = match client_ca_pem {
        Some(ca) => {
            let roots = roots_from_pem(ca)?;
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .map_err(|e| io::Error::other(format!("tls: bad client CA: {e}")))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let config = builder
        .with_single_cert(identity.certs, identity.key)
        .map_err(|e| io::Error::other(format!("tls: bad server identity: {e}")))?;
    Ok(Arc::new(config))
}

fn mtime(path: &Path) -> io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

impl TlsAcceptor {
    /// Build a **static** acceptor from in-memory PEM. `client_ca_pem` enables
    /// mutual TLS: peers must present a certificate chaining to one of the CAs
    /// in the bundle. Without it, any TLS client can connect (transport
    /// encryption only — the embedder must gate trust some other way, e.g. a
    /// bearer credential). No rotation: prefer [`TlsAcceptor::from_paths`] for
    /// a mounted identity.
    pub fn new(identity: ServerIdentity, client_ca_pem: Option<&[u8]>) -> io::Result<TlsAcceptor> {
        let client_auth = client_ca_pem.is_some();
        Ok(TlsAcceptor {
            config: Mutex::new(build_server_config(identity, client_ca_pem)?),
            client_auth,
            reload: None,
        })
    }

    /// Build a **live** acceptor from PEM file paths: the identity (and client
    /// CA, when given) is re-read when the files change, so a rotated mounted
    /// Secret is served without a restart. Whether client certs are REQUIRED is
    /// fixed by `client_ca`'s presence at build time (the *content* may rotate;
    /// the auth posture may not). A reload failure keeps the last-good identity.
    pub fn from_paths(
        cert: &Path,
        key: &Path,
        client_ca: Option<&Path>,
    ) -> io::Result<TlsAcceptor> {
        let identity = ServerIdentity::from_pem(&std::fs::read(cert)?, &std::fs::read(key)?)?;
        let ca_pem = client_ca.map(std::fs::read).transpose()?;
        let config = build_server_config(identity, ca_pem.as_deref())?;
        let mtimes = (mtime(cert)?, mtime(key)?, client_ca.map(mtime).transpose()?);
        Ok(TlsAcceptor {
            config: Mutex::new(config),
            client_auth: client_ca.is_some(),
            reload: Some(ReloadSource {
                cert: cert.to_path_buf(),
                key: key.to_path_buf(),
                client_ca: client_ca.map(Path::to_path_buf),
                state: Mutex::new(ReloadState {
                    checked_at: Instant::now(),
                    mtimes,
                    generation: 0,
                    last_error: None,
                }),
            }),
        })
    }

    /// Whether this acceptor requires client certificates (mutual TLS).
    pub fn requires_client_auth(&self) -> bool {
        self.client_auth
    }

    /// How many live reloads have been applied (0 = the initial identity; a
    /// static acceptor never advances). For status surfaces / tests.
    pub fn reload_generation(&self) -> u64 {
        self.reload.as_ref().map_or(0, |r| {
            r.state.lock().unwrap_or_else(|e| e.into_inner()).generation
        })
    }

    /// The most recent reload failure, if the CURRENT state is degraded (a
    /// successful reload clears it). The acceptor keeps serving the last-good
    /// identity through failures — this is how the embedder can see them.
    pub fn last_reload_error(&self) -> Option<String> {
        self.reload.as_ref().and_then(|r| {
            r.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_error
                .clone()
        })
    }

    /// Force an immediate file check (bypasses the throttle) — for tests and
    /// an explicit reload signal. No-op on a static acceptor.
    pub fn force_reload_check(&self) {
        self.maybe_reload(Duration::ZERO);
    }

    /// Throttled check-and-swap: re-stat the identity files at most once per
    /// `ttl`; on an mtime change, re-read + rebuild and swap the config in.
    /// Every failure (stat, read, parse) records `last_error` and KEEPS the
    /// last-good config — a rotation must never take the listener down.
    fn maybe_reload(&self, ttl: Duration) {
        let Some(src) = &self.reload else { return };
        let mut st = src.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.checked_at.elapsed() < ttl {
            return;
        }
        st.checked_at = Instant::now();
        let stat = (|| -> io::Result<_> {
            Ok((
                mtime(&src.cert)?,
                mtime(&src.key)?,
                src.client_ca.as_deref().map(mtime).transpose()?,
            ))
        })();
        let mtimes = match stat {
            Ok(m) => m,
            Err(e) => {
                st.last_error = Some(format!("stat: {e}"));
                return;
            }
        };
        if mtimes == st.mtimes {
            return;
        }
        let rebuilt = (|| -> io::Result<Arc<ServerConfig>> {
            let identity =
                ServerIdentity::from_pem(&std::fs::read(&src.cert)?, &std::fs::read(&src.key)?)?;
            let ca_pem = src.client_ca.as_deref().map(std::fs::read).transpose()?;
            build_server_config(identity, ca_pem.as_deref())
        })();
        match rebuilt {
            Ok(config) => {
                *self.config.lock().unwrap_or_else(|e| e.into_inner()) = config;
                st.mtimes = mtimes;
                st.generation += 1;
                st.last_error = None;
            }
            Err(e) => {
                st.last_error = Some(format!("reload: {e}"));
            }
        }
    }

    /// Wrap an accepted TCP connection in server-side TLS, driving the
    /// handshake to completion so failures (including a missing/invalid client
    /// certificate under mTLS) surface HERE, not on the first read. A live
    /// acceptor first applies any pending identity rotation (throttled).
    pub fn accept(&self, tcp: TcpStream) -> io::Result<ServerTlsStream> {
        self.maybe_reload(RELOAD_CHECK_TTL);
        let config = self
            .config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let conn =
            ServerConnection::new(config).map_err(|e| io::Error::other(format!("tls: {e}")))?;
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
    extend_with_extra_ca(&mut roots);
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
/// Incorporates the [`install_extra_ca`] anchors present at FIRST use — which is
/// why install must precede the first dial (documented there).
fn client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            extend_with_extra_ca(&mut roots);
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

    /// One test fn on purpose: EXTRA_CA is process-global set-once state, so the
    /// whole install lifecycle must be exercised in a deterministic order (the
    /// test harness runs separate #[test] fns in parallel).
    #[test]
    fn install_extra_ca_lifecycle() {
        let ca = include_bytes!("../tests/fixtures/ca.pem");
        // Bad input fails BEFORE the set-once slot is consumed.
        assert_eq!(
            install_extra_ca(b"not a pem").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            install_extra_ca(b"").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(extra_ca_count(), 0);
        // A valid CA installs and is counted.
        let n = install_extra_ca(ca).expect("fixture CA installs");
        assert!(n >= 1);
        assert_eq!(extra_ca_count(), n);
        // Re-installing the SAME bundle is an idempotent no-op.
        assert_eq!(install_extra_ca(ca).expect("idempotent"), n);
        // A DIFFERENT bundle (same CA twice = a different anchor vec) is refused.
        let mut doubled = ca.to_vec();
        doubled.extend_from_slice(ca);
        assert_eq!(
            install_extra_ca(&doubled).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        // The anchors flow into a fresh root store build.
        let mut roots = RootCertStore::empty();
        extend_with_extra_ca(&mut roots);
        assert_eq!(roots.len(), n);
    }

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
