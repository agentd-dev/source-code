// SPDX-License-Identifier: Apache-2.0
//! SSRF classifier (RFC 0012 — security posture, §"SSRF guard").
//!
//! A *pure* address classifier plus a DNS-resolving host guard. The
//! acceptance bar from assessment §4 M6 is blunt: "HTTP client refuses
//! RFC-1918 / link-local by default". This module is the mechanism; it is
//! complete and tested but currently has no call site, because the only
//! outbound HTTP (`intel/client.rs`) targets the operator-configured endpoint
//! and is exempt. It exists to guard any future model/agent-supplied URL,
//! composed at the call site that introduces one.
//!
//! ## What "non-global" means here
//!
//! [`is_global`] returns `false` — i.e. the address is *blocked* — for
//! any address an attacker could pivot to from inside the appliance's
//! network namespace:
//!
//! * loopback (`127.0.0.0/8`, `::1`)
//! * RFC-1918 private (`10/8`, `172.16/12`, `192.168/16`)
//! * link-local (`169.254/16`, `fe80::/10`) — this is the cloud
//!   metadata range (`169.254.169.254`)
//! * IPv6 unique-local (`fc00::/7`)
//! * unspecified (`0.0.0.0`, `::`)
//! * multicast and the IPv4 limited broadcast (`255.255.255.255`)
//! * "this network" `0.0.0.0/8` and the IETF/benchmark documentation
//!   ranges, which never route on the public Internet
//! * **any IPv4-mapped / IPv4-compatible IPv6** whose embedded v4
//!   address is itself non-global — `::ffff:127.0.0.1` and friends are
//!   a classic guard bypass, so we unwrap before classifying.
//!
//! We deliberately do NOT lean on `std`'s unstable `IpAddr::is_global`
//! (feature `ip`, issue #27709) — it is not available on our MSRV and
//! its semantics drift. Every range below is spelled out by hand from
//! primitives that are stable on Rust 1.88, in the same
//! enumerate-the-bytes spirit as the rest of the crate.
//!
//! ## Logging posture
//!
//! Hosts and IPs are *operational* identifiers, not tool/instruction
//! content, so the diagnostic carries the host and the offending class
//! — never request bodies, headers, or secrets. The codebase is
//! content-capture-off by default and this module keeps that contract.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A host failed the SSRF guard, or could not be resolved at all.
///
/// `Clone`/`Eq` so callers can compare and surface it without owning a
/// socket; `host` is the operator/tool-supplied authority (not secret),
/// `reason` is a short human class string (e.g. `"loopback"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsrfError {
    /// The host authority that was guarded (no port).
    pub host: String,
    /// Short class of the failure, e.g. `"link-local 169.254.169.254"`.
    pub reason: String,
}

impl std::fmt::Display for SsrfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "host `{}` rejected by SSRF guard: {}",
            self.host, self.reason
        )
    }
}

impl std::error::Error for SsrfError {}

fn reject(host: &str, reason: impl Into<String>) -> SsrfError {
    SsrfError {
        host: host.to_string(),
        reason: reason.into(),
    }
}

// ---------------------------------------------------------------------------
// Pure classifier
// ---------------------------------------------------------------------------

/// `true` iff `ip` is a globally routable unicast address that is safe
/// to dial from inside the appliance — i.e. *not* in any of the blocked
/// ranges documented on this module.
///
/// Pure: no DNS, no I/O. This is the single source of truth; the host
/// guard composes it over every resolved address.
pub fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => is_global_v6(v6),
    }
}

/// IPv4 classification. Blocked ranges are spelled out from RFC-3330 /
/// RFC-1918 / RFC-3927 rather than via `std`'s unstable helpers.
fn is_global_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();

    // "This host on this network" — 0.0.0.0/8 (covers 0.0.0.0).
    if a == 0 {
        return false;
    }
    // Loopback 127.0.0.0/8, private 10/8 + 172.16/12 + 192.168/16,
    // link-local 169.254/16, broadcast, multicast 224/4 + reserved
    // 240/4, all unspecified — std covers these and they are stable.
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_documentation()
    {
        return false;
    }
    // Carrier-grade NAT (RFC-6598) 100.64.0.0/10 — shared address
    // space, not globally routable; `is_shared` is unstable so unfold
    // the prefix by hand.
    if a == 100 && (64..=127).contains(&b) {
        return false;
    }
    // Reserved 240.0.0.0/4 (minus the broadcast already caught) — never
    // a routable destination.
    if a >= 240 {
        return false;
    }
    true
}

/// IPv6 classification. We first peel IPv4-mapped (`::ffff:0:0/96`) and
/// IPv4-compatible (`::/96`) forms back to v4 and re-run the v4 rules —
/// this is the bypass that bites naive guards.
fn is_global_v6(ip: Ipv6Addr) -> bool {
    // `::ffff:a.b.c.d` — classify the embedded v4 address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_global_v4(v4);
    }
    // `::a.b.c.d` (deprecated IPv4-compatible, plus ::1 / ::). `to_ipv4`
    // also yields the mapped form, but the mapped case is handled
    // above; here it catches the compatible range. ::1 and :: classify
    // as loopback/unspecified v4-side too, but we also guard them
    // directly below for clarity.
    if let Some(v4) = ip.to_ipv4() {
        return is_global_v4(v4);
    }

    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }

    let segments = ip.segments();
    // Link-local unicast fe80::/10 (top 10 bits == 1111 1110 10).
    if (segments[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    // Unique-local fc00::/7 (top 7 bits == 1111 110x).
    if (segments[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    // Documentation 2001:db8::/32 — never globally routable.
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Host guard (DNS-resolving)
// ---------------------------------------------------------------------------

/// Resolve `host` and reject if *any* resolved address is non-global.
///
/// This is the deny-all-the-aliases stance: a hostname that resolves to
/// both a public and a private address is rejected, because an attacker
/// who controls DNS could otherwise race the second connect (a DNS
/// rebinding pivot). Resolution uses `std`'s `ToSocketAddrs`; we append
/// `:0` because the resolver needs a port grammar even though we ignore
/// it.
///
/// `allow_private == true` is the operator escape hatch — it skips the
/// check entirely without even resolving, so trusted localhost/private
/// gateways (the configured intelligence endpoint) keep working. Callers
/// that take a MODEL/AGENT-supplied URL MUST pass `false`.
///
/// Pure-ish: the only side effect is DNS resolution. No bytes are sent.
pub fn guard_host(host: &str, allow_private: bool) -> Result<(), SsrfError> {
    if allow_private {
        return Ok(());
    }
    if host.is_empty() {
        return Err(reject(host, "empty host"));
    }

    // A bare IP literal short-circuits DNS — `ToSocketAddrs` would
    // resolve it too, but parsing first lets us reject `127.0.0.1`
    // without a syscall and yields a sharper class string.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_addr(host, ip);
    }
    // IPv6 literals are commonly bracketed in URLs (`[::1]`).
    if let Some(stripped) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
        && let Ok(ip) = stripped.parse::<IpAddr>()
    {
        return check_addr(host, ip);
    }

    let addrs = (host, 0u16)
        .to_socket_addrs()
        .map_err(|e| reject(host, format!("resolve failed: {e}")))?;

    let mut saw_any = false;
    for sa in addrs {
        saw_any = true;
        check_addr(host, sa.ip())?;
    }
    if !saw_any {
        return Err(reject(host, "no addresses resolved"));
    }
    Ok(())
}

/// Classify one resolved address, turning a non-global result into a
/// typed rejection with a short class string.
fn check_addr(host: &str, ip: IpAddr) -> Result<(), SsrfError> {
    if is_global(ip) {
        Ok(())
    } else {
        Err(reject(host, format!("{} ({ip})", class_of(ip))))
    }
}

/// Best-effort human label for *why* an address is non-global. Purely
/// cosmetic — `is_global` remains the authority on the boolean.
fn class_of(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() {
                "unspecified"
            } else if v4.is_loopback() {
                "loopback"
            } else if v4.is_private() {
                "private (RFC-1918)"
            } else if v4.is_link_local() {
                "link-local"
            } else if v4.is_broadcast() {
                "broadcast"
            } else if v4.is_multicast() {
                "multicast"
            } else {
                "reserved"
            }
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                return class_of(IpAddr::V4(v4));
            }
            if v6.is_unspecified() {
                "unspecified"
            } else if v6.is_loopback() {
                "loopback"
            } else if v6.is_multicast() {
                "multicast"
            } else {
                "link-local/unique-local"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — LITERAL IPs only, never DNS.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().expect("test ipv6 literal"))
    }

    #[test]
    fn public_v4_is_global() {
        assert!(is_global(v4(8, 8, 8, 8)));
        assert!(is_global(v4(1, 1, 1, 1)));
        assert!(is_global(v4(93, 184, 216, 34))); // example.com historic
        assert!(is_global(v4(172, 15, 255, 255))); // just below 172.16/12
        assert!(is_global(v4(172, 32, 0, 1))); // just above 172.31
        assert!(is_global(v4(11, 0, 0, 1))); // just above 10/8
        assert!(is_global(v4(192, 167, 255, 255))); // just below 192.168/16
        assert!(is_global(v4(192, 169, 0, 1))); // just above 192.168/16
        assert!(is_global(v4(100, 63, 255, 255))); // just below CGNAT 100.64/10
        assert!(is_global(v4(100, 128, 0, 1))); // just above CGNAT
    }

    #[test]
    fn loopback_blocked() {
        assert!(!is_global(v4(127, 0, 0, 1)));
        assert!(!is_global(v4(127, 255, 255, 255)));
        assert!(!is_global(v6("::1")));
    }

    #[test]
    fn rfc1918_blocked() {
        // 10/8
        assert!(!is_global(v4(10, 0, 0, 0)));
        assert!(!is_global(v4(10, 255, 255, 255)));
        // 172.16/12
        assert!(!is_global(v4(172, 16, 0, 0)));
        assert!(!is_global(v4(172, 16, 0, 1)));
        assert!(!is_global(v4(172, 31, 255, 255)));
        // 192.168/16
        assert!(!is_global(v4(192, 168, 0, 1)));
        assert!(!is_global(v4(192, 168, 255, 255)));
    }

    #[test]
    fn link_local_and_metadata_blocked() {
        assert!(!is_global(v4(169, 254, 0, 1)));
        // The cloud metadata endpoint — the whole point of M6.
        assert!(!is_global(v4(169, 254, 169, 254)));
        assert!(!is_global(v4(169, 254, 255, 255)));
        // IPv6 link-local fe80::/10 — both ends of the prefix.
        assert!(!is_global(v6("fe80::1")));
        assert!(!is_global(v6("febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff")));
    }

    #[test]
    fn unique_local_blocked() {
        // fc00::/7 covers fc00:: and fd00::.
        assert!(!is_global(v6("fc00::1")));
        assert!(!is_global(v6("fd12:3456:789a::1")));
        assert!(!is_global(v6("fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff")));
    }

    #[test]
    fn unspecified_blocked() {
        assert!(!is_global(v4(0, 0, 0, 0)));
        assert!(!is_global(v4(0, 1, 2, 3))); // 0/8 "this network"
        assert!(!is_global(v6("::")));
    }

    #[test]
    fn multicast_and_broadcast_blocked() {
        assert!(!is_global(v4(224, 0, 0, 1)));
        assert!(!is_global(v4(239, 255, 255, 255)));
        assert!(!is_global(v4(255, 255, 255, 255))); // limited broadcast
        assert!(!is_global(v6("ff02::1")));
    }

    #[test]
    fn reserved_v4_blocked() {
        assert!(!is_global(v4(240, 0, 0, 1)));
        assert!(!is_global(v4(255, 0, 0, 1)));
    }

    #[test]
    fn ipv4_mapped_bypass_is_caught() {
        // ::ffff:127.0.0.1 must classify as loopback, not as a global
        // v6 address. This is the headline bypass.
        assert!(!is_global(v6("::ffff:127.0.0.1")));
        assert!(!is_global(v6("::ffff:10.0.0.1")));
        assert!(!is_global(v6("::ffff:169.254.169.254")));
        assert!(!is_global(v6("::ffff:192.168.1.1")));
        // A mapped *public* v4 stays global.
        assert!(is_global(v6("::ffff:8.8.8.8")));
    }

    #[test]
    fn ipv4_compatible_bypass_is_caught() {
        // ::a.b.c.d (deprecated) — embedded private v4 must be blocked.
        assert!(!is_global(v6("::10.0.0.1")));
        assert!(!is_global(v6("::169.254.169.254")));
    }

    #[test]
    fn public_v6_is_global() {
        assert!(is_global(v6("2606:4700:4700::1111"))); // 1.1.1.1 v6
        assert!(is_global(v6("2001:4860:4860::8888"))); // google dns v6
    }

    #[test]
    fn ipv6_documentation_blocked() {
        assert!(!is_global(v6("2001:db8::1")));
    }

    // --- guard_host over literals (no DNS) ---

    #[test]
    fn guard_rejects_ip_literals() {
        assert!(guard_host("127.0.0.1", false).is_err());
        assert!(guard_host("10.0.0.5", false).is_err());
        assert!(guard_host("169.254.169.254", false).is_err());
        assert!(guard_host("::1", false).is_err());
        assert!(guard_host("[::1]", false).is_err()); // bracketed
        assert!(guard_host("[fe80::1]", false).is_err());
        assert!(guard_host("::ffff:127.0.0.1", false).is_err());
    }

    #[test]
    fn guard_allows_public_ip_literals() {
        assert!(guard_host("8.8.8.8", false).is_ok());
        assert!(guard_host("1.1.1.1", false).is_ok());
        assert!(guard_host("[2606:4700:4700::1111]", false).is_ok());
    }

    #[test]
    fn allow_private_skips_everything() {
        // The operator escape hatch — must not even fail on a literal
        // private address, since the intel endpoint is often localhost.
        assert!(guard_host("127.0.0.1", true).is_ok());
        assert!(guard_host("10.0.0.5", true).is_ok());
        assert!(guard_host("", true).is_ok());
        assert!(guard_host("anything.invalid", true).is_ok());
    }

    #[test]
    fn empty_host_rejected_when_guarded() {
        assert!(guard_host("", false).is_err());
    }

    #[test]
    fn error_carries_host_and_class() {
        let err = guard_host("169.254.169.254", false).unwrap_err();
        assert_eq!(err.host, "169.254.169.254");
        assert!(err.reason.contains("link-local"), "reason: {}", err.reason);
        // Display must surface both without panicking.
        let shown = err.to_string();
        assert!(shown.contains("169.254.169.254"));
        assert!(shown.contains("SSRF guard"));
    }

    #[test]
    fn class_labels_are_specific() {
        assert_eq!(class_of(v4(127, 0, 0, 1)), "loopback");
        assert_eq!(class_of(v4(10, 0, 0, 1)), "private (RFC-1918)");
        assert_eq!(class_of(v4(169, 254, 1, 1)), "link-local");
        assert_eq!(class_of(v4(0, 0, 0, 0)), "unspecified");
        assert_eq!(class_of(v4(224, 0, 0, 1)), "multicast");
        assert_eq!(class_of(v4(255, 255, 255, 255)), "broadcast");
        assert_eq!(class_of(v4(240, 0, 0, 1)), "reserved");
        // mapped v6 borrows the v4 label.
        assert_eq!(class_of(v6("::ffff:10.0.0.1")), "private (RFC-1918)");
    }
}
