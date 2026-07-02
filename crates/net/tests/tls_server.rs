// SPDX-License-Identifier: Apache-2.0
//! Loopback round-trip tests for the TLS **server** acceptor + pinned-CA client
//! (the HTTPS serving substrate for the target-vision pivot): a real handshake
//! over a real TCP socket, both plain-TLS and mutual-TLS, using the committed
//! test PKI under `tests/fixtures/` (see its README — test data, not secrets).
#![cfg(feature = "tls")]

use net::tls::{ClientIdentity, ServerIdentity, TlsAcceptor, connect_with_ca, peer_presented_cert};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {path}: {e}"))
}

fn server_identity() -> ServerIdentity {
    ServerIdentity::from_pem(&fixture("server.pem"), &fixture("server.key")).expect("server id")
}

fn client_identity() -> ClientIdentity {
    ClientIdentity::from_pem(&fixture("client.pem"), &fixture("client.key")).expect("client id")
}

/// Bind a loopback listener and serve ONE echo connection through `acceptor`
/// on a background thread. Returns the bound port.
fn spawn_echo(acceptor: TlsAcceptor) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (tcp, _) = listener.accept().expect("accept tcp");
        tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
        tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();
        // Under mTLS a bad peer fails the handshake HERE; the thread just ends.
        let Ok(mut tls) = acceptor.accept(tcp) else {
            return;
        };
        assert!(
            !acceptor.requires_client_auth() || peer_presented_cert(&tls),
            "an mTLS-accepted peer must have presented a verified cert"
        );
        let mut buf = [0u8; 5];
        if tls.read_exact(&mut buf).is_ok() {
            let _ = tls.write_all(&buf);
            let _ = tls.flush();
        }
    });
    port
}

fn dial(port: u16) -> TcpStream {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("tcp connect");
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();
    tcp
}

#[test]
fn tls_server_round_trips_against_a_pinned_ca_client() {
    let acceptor = TlsAcceptor::new(server_identity(), None).expect("acceptor");
    assert!(!acceptor.requires_client_auth());
    let port = spawn_echo(acceptor);

    // The client trusts ONLY the test CA (the server cert never chains to
    // webpki), and the server SAN covers 127.0.0.1.
    let mut tls = connect_with_ca(dial(port), "127.0.0.1", &fixture("ca.pem"), None)
        .expect("tls connect (pinned CA)");
    tls.write_all(b"hello").unwrap();
    tls.flush().unwrap();
    let mut echo = [0u8; 5];
    tls.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");
}

#[test]
fn mtls_requires_and_verifies_a_client_certificate() {
    // Without a client identity the handshake must FAIL (the server demands a
    // certificate chaining to the pinned client CA)…
    let acceptor = TlsAcceptor::new(server_identity(), Some(&fixture("ca.pem"))).expect("acceptor");
    assert!(acceptor.requires_client_auth());
    let port = spawn_echo(acceptor);
    let mut tls = connect_with_ca(dial(port), "127.0.0.1", &fixture("ca.pem"), None)
        .expect("client handshake starts");
    let failed = tls.write_all(b"hello").and_then(|_| {
        tls.flush()?;
        let mut echo = [0u8; 5];
        tls.read_exact(&mut echo)
    });
    assert!(failed.is_err(), "no client cert must not survive mTLS");

    // …and with the client identity the round-trip completes.
    let acceptor = TlsAcceptor::new(server_identity(), Some(&fixture("ca.pem"))).expect("acceptor");
    let port = spawn_echo(acceptor);
    let mut tls = connect_with_ca(
        dial(port),
        "127.0.0.1",
        &fixture("ca.pem"),
        Some(&client_identity()),
    )
    .expect("mtls connect");
    tls.write_all(b"hello").unwrap();
    tls.flush().unwrap();
    let mut echo = [0u8; 5];
    tls.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");
}

#[test]
fn acceptor_rejects_garbage_identity_and_ca() {
    assert!(ServerIdentity::from_pem(b"nope", b"nope").is_err());
    let err = TlsAcceptor::new(server_identity(), Some(b"not a ca")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

/// The `--tls-ca` end-to-end proof: the DEFAULT client dial (`connect`, webpki
/// roots — the path every intelligence/MCP/A2A dial uses) cannot reach a server
/// carrying a private-CA cert until [`install_extra_ca`] adds that CA as a
/// process-wide anchor; afterwards the same dial round-trips.
///
/// One test fn on purpose: the extra-CA registry is process-global set-once
/// state, and the default no-identity config is CACHED at its first build — so
/// the pre-install failure leg must use the per-call (identity) config build,
/// keeping the cached default config unbuilt until after install. No other test
/// in this binary uses plain `connect` (they pin via `connect_with_ca`, which
/// deliberately ignores the extra anchors), so installing here is safe.
#[test]
fn install_extra_ca_unlocks_the_default_dial_against_a_private_ca() {
    use net::tls::{connect, extra_ca_count, install_extra_ca};

    // BEFORE install: the private-CA server is untrusted. (Identity path — the
    // per-call config build — so the cached default config stays unbuilt.)
    assert_eq!(extra_ca_count(), 0);
    let port = spawn_echo(TlsAcceptor::new(server_identity(), None).expect("acceptor"));
    let err = connect(dial(port), "127.0.0.1", Some(&client_identity()))
        .and_then(|mut tls| tls.write_all(b"hello").and_then(|_| tls.flush()))
        .expect_err("webpki roots must not trust the fixture CA");
    assert!(
        format!("{err}").contains("UnknownIssuer") || format!("{err}").contains("certificate"),
        "want a trust failure, got: {err}"
    );

    // Install the private CA as a process-wide extra anchor.
    let n = install_extra_ca(&fixture("ca.pem")).expect("install fixture CA");
    assert!(n >= 1);

    // AFTER install: the identity path (fresh config per call) trusts it...
    let port = spawn_echo(TlsAcceptor::new(server_identity(), None).expect("acceptor"));
    let mut tls = connect(dial(port), "127.0.0.1", Some(&client_identity()))
        .expect("identity dial trusts the installed CA");
    tls.write_all(b"hello").unwrap();
    tls.flush().unwrap();
    let mut echo = [0u8; 5];
    tls.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");

    // ...and so does the DEFAULT (cached, no-identity) path, built AFTER install
    // — the install-before-first-dial contract every agentd process follows.
    let port = spawn_echo(TlsAcceptor::new(server_identity(), None).expect("acceptor"));
    let mut tls = connect(dial(port), "127.0.0.1", None).expect("default dial trusts the CA");
    tls.write_all(b"hello").unwrap();
    tls.flush().unwrap();
    let mut echo = [0u8; 5];
    tls.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");
}
