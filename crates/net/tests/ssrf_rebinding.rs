// SPDX-License-Identifier: AGPL-3.0-only
//! DNS-rebinding tests for the SSRF guard.
//!
//! The guarded URLs are the ones agentd does not choose: an A2A push
//! notification target a peer registered, the `url` of an `http` workflow node a
//! model filled in. An attacker who supplies one of those also controls the DNS
//! that answers it, and can answer twice — a public address for whoever is
//! checking, `169.254.169.254` for whoever is connecting. A guard that resolves,
//! approves, and then lets the caller dial the *name* is therefore worth nothing
//! against the only attacker it exists to stop.
//!
//! These tests drive both halves through an injected [`ResolveFn`] that changes
//! its answer between them, and assert the strong property: not merely that the
//! dial returns an error, but that **no connection ever arrives** at the private
//! address the second answer pointed to. A real `TcpListener` on loopback is the
//! witness — if the guard leaks, it accepts.

use net::ssrf::{
    self, ResolveFn, connect_addrs, connect_vetted_with, resolve_guarded, resolve_guarded_with,
};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::Duration;

/// Short: every dial in this file is either refused before the syscall or aimed
/// at a listener on this machine, so nothing here should ever wait.
const T: Duration = Duration::from_millis(500);

/// A public address the guard must accept — the historic `example.com` A record,
/// used as a literal so no test in this file ever touches real DNS.
fn public() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))
}

/// Bind a loopback listener and hand back it plus its address. This stands in
/// for "the private thing the attacker wants reached": if a connection is ever
/// accepted on it, the guard has failed.
fn honeypot() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback honeypot");
    l.set_nonblocking(true).expect("nonblocking honeypot");
    let a = l.local_addr().expect("honeypot addr");
    (l, a)
}

/// `true` iff nothing has connected to the honeypot. Non-blocking, so a clean
/// `WouldBlock` is the "nobody came" answer.
fn untouched(l: &TcpListener) -> bool {
    matches!(l.accept(), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock)
}

// ---------------------------------------------------------------------------
// The rebinding shape: answer #1 public, answer #2 private.
// ---------------------------------------------------------------------------

static REBIND_CALLS: AtomicUsize = AtomicUsize::new(0);
static REBIND_PORT: AtomicU16 = AtomicU16::new(0);

/// The hostile authoritative server. First query (the guard's) gets a public
/// address; every query after it gets the honeypot on loopback. Statics rather
/// than a closure because [`ResolveFn`] is a bare `fn` pointer — deliberately,
/// so the seam cannot smuggle state into production.
fn rebinding_resolver(_host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    if REBIND_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
        Ok(vec![SocketAddr::new(public(), port)])
    } else {
        Ok(vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            REBIND_PORT.load(Ordering::SeqCst),
        )])
    }
}

#[test]
fn rebinding_second_answer_never_reaches_the_private_address() {
    let (listener, addr) = honeypot();
    REBIND_PORT.store(addr.port(), Ordering::SeqCst);
    REBIND_CALLS.store(0, Ordering::SeqCst);

    // Half one: the guard asks, and is told something public. This is the
    // check a peer's `SetTaskPushNotificationConfig` passes.
    let vetted: ResolveFn = rebinding_resolver;
    let approved = resolve_guarded_with("rebind.example", 443, false, vetted)
        .expect("hostile first answer is public, so the guard must approve it");
    assert_eq!(approved, vec![SocketAddr::new(public(), 443)]);

    // The hostile server re-arms. This is the point of the test: it serves the
    // truth to whoever asks *first* — the delivery's own guard — and the
    // honeypot to whoever asks *next*, which in a leaky guard is the connect
    // re-resolving the name. There must be no next lookup: the dial takes the
    // addresses the guard vetted and never touches DNS again. Without this
    // re-arm the test would pass for the wrong reason (the delivery guard
    // itself would see loopback), and would not distinguish the two worlds.
    REBIND_CALLS.store(0, Ordering::SeqCst);

    // Half two: the delivery. Whether the public address it vetted answers is
    // beside the point — the load-bearing assertion is that the honeypot the
    // second answer pointed at is never touched. That is the property that
    // separates a guard which carries its result forward from one that resolves,
    // approves, and then lets the connect ask DNS again.
    let _ = connect_vetted_with("rebind.example", 443, T, false, vetted);
    assert!(
        untouched(&listener),
        "a connection reached the honeypot: the guard resolved, approved, and \
         then let the dial re-resolve"
    );
    assert_eq!(
        REBIND_CALLS.load(Ordering::SeqCst),
        1,
        "the delivery must resolve exactly once; a second lookup is the \
         rebinding window"
    );
}

#[test]
fn dialling_the_vetted_addresses_ignores_a_later_answer() {
    // The whole invariant: what the guard approved is what gets dialled. Here
    // the approved list is public and unreachable, so the connect fails on the
    // network — but it fails aimed at 93.184.216.34, never at the honeypot the
    // resolver would now hand out.
    let (listener, addr) = honeypot();
    let approved = vec![SocketAddr::new(public(), 443)];
    let _ = connect_addrs(
        "rebind.example",
        &approved,
        Duration::from_millis(50),
        false,
    );
    assert!(untouched(&listener), "dial must not consult DNS at all");
    assert_ne!(approved[0], addr);
}

// ---------------------------------------------------------------------------
// A tampered / re-used address list is re-checked at the syscall boundary.
// ---------------------------------------------------------------------------

#[test]
fn connect_addrs_refuses_a_private_address_without_connecting() {
    // Simulates a caller that got its addresses from anywhere else — a cache, a
    // redirect, a second resolution it did itself. The classifier runs again
    // immediately before `connect_timeout`, so the honeypot stays silent.
    let (listener, addr) = honeypot();
    let err = connect_addrs("attacker.example", &[addr], T, false)
        .expect_err("loopback must be refused even when handed in directly");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(untouched(&listener));
}

#[test]
fn one_private_entry_refuses_the_whole_dial() {
    // Deny-all-the-aliases: a mixed list must not fall through to the private
    // entry when the public one refuses to connect, and must not be dialled at
    // all. Otherwise the attacker just answers with both and waits.
    let (listener, addr) = honeypot();
    let mixed = vec![SocketAddr::new(public(), 443), addr];
    let err = connect_addrs("mixed.example", &mixed, Duration::from_millis(50), false)
        .expect_err("a mixed answer is refused whole");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(untouched(&listener));
}

#[test]
fn empty_address_list_is_an_error_not_a_panic() {
    let err = connect_addrs("nowhere.example", &[], T, false).expect_err("nothing to dial");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

// ---------------------------------------------------------------------------
// The dial path still works where it is supposed to.
// ---------------------------------------------------------------------------

#[test]
fn allow_private_still_reaches_a_loopback_listener() {
    // The operator escape hatch (`security.egress.allow_private`) has to keep
    // dialling private targets, and this is also the proof that `connect_addrs`
    // really opens a socket rather than always erroring.
    let (listener, addr) = honeypot();
    let stream = connect_addrs("localhost", &[addr], T, true).expect("escape hatch must connect");
    assert_eq!(stream.peer_addr().expect("peer addr"), addr);
    // Timeouts are set on the returned stream, as `http::connect_tcp` does.
    assert_eq!(stream.read_timeout().expect("read timeout"), Some(T));
    assert_eq!(stream.write_timeout().expect("write timeout"), Some(T));
    // And the connection did arrive.
    listener
        .set_nonblocking(false)
        .expect("blocking for the accept");
    listener
        .accept()
        .expect("honeypot accepts the allowed dial");
}

/// A benign resolver: every name is the same public address.
fn public_resolver(_host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    Ok(vec![SocketAddr::new(public(), port)])
}

#[test]
fn ordinary_public_names_are_approved_with_their_addresses() {
    let addrs = resolve_guarded_with("api.example.com", 443, false, public_resolver)
        .expect("a public name must pass");
    assert_eq!(addrs, vec![SocketAddr::new(public(), 443)]);
    // The port the caller asked for is carried through — the dial uses these
    // addresses verbatim, so a dropped port would silently retarget it.
    let alt = resolve_guarded_with("api.example.com", 8443, false, public_resolver)
        .expect("a non-default port must pass too");
    assert_eq!(alt[0].port(), 8443);
}

#[test]
fn public_ip_literals_resolve_without_dns() {
    // `std_resolve` short-circuits literals, so this exercises the real
    // production resolver without depending on a network in CI.
    let addrs = resolve_guarded("93.184.216.34", 443, false).expect("public literal");
    assert_eq!(addrs, vec![SocketAddr::new(public(), 443)]);
    let v6 = resolve_guarded("[2606:4700:4700::1111]", 443, false).expect("bracketed public v6");
    assert_eq!(v6.len(), 1);
    assert_eq!(v6[0].port(), 443);
}

// ---------------------------------------------------------------------------
// The flat refusals the guard owes on every call, rebinding aside.
// ---------------------------------------------------------------------------

#[test]
fn the_classic_refusals_still_hold() {
    for host in [
        "127.0.0.1",         // loopback
        "[::1]",             // loopback, as a URL writes it
        "169.254.169.254",   // link-local — the cloud metadata endpoint
        "10.0.0.5",          // RFC-1918
        "192.168.1.1",       // RFC-1918
        "172.16.0.1",        // RFC-1918
        "0.0.0.0",           // unspecified
        "240.0.0.1",         // reserved
        "[fe80::1]",         // IPv6 link-local
        "[fc00::1]",         // IPv6 unique-local
        "::ffff:127.0.0.1",  // the IPv4-mapped bypass
        "[::ffff:10.0.0.1]", // ...bracketed
        "",                  // no host at all
    ] {
        assert!(
            resolve_guarded(host, 443, false).is_err(),
            "`{host}` must be refused by resolve_guarded"
        );
        assert!(
            ssrf::guard_host(host, false).is_err(),
            "`{host}` must be refused by guard_host"
        );
    }
}

#[test]
fn a_resolver_answering_private_is_refused_even_on_the_first_call() {
    fn private_resolver(_host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            port,
        )])
    }
    let err = resolve_guarded_with("metadata.example", 80, false, private_resolver)
        .expect_err("link-local answer refused");
    assert_eq!(err.host, "metadata.example");
    assert!(err.reason.contains("link-local"), "reason: {}", err.reason);
}

#[test]
fn an_empty_resolver_answer_is_refused_not_treated_as_clean() {
    fn empty_resolver(_host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(Vec::new())
    }
    let err = resolve_guarded_with("void.example", 443, false, empty_resolver)
        .expect_err("no addresses is a refusal, not a pass");
    assert!(
        err.reason.contains("no addresses"),
        "reason: {}",
        err.reason
    );
}
