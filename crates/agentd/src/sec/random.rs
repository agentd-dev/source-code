// SPDX-License-Identifier: AGPL-3.0-only
//! OS randomness for secrets: session tokens, launch codes, internal keys a
//! caller must not be able to guess.
//!
//! Read straight from the kernel's CSPRNG (`/dev/urandom`) — no dependency,
//! and nothing seeded in-process that a fork or a snapshot could duplicate.
//! Unlike the id generators elsewhere (a ULID only needs to be unique), there
//! is deliberately **no fallback**: a token derived from the clock and the pid
//! is one an attacker can enumerate, so a host without an entropy source gets
//! an error, never a weaker token.

use std::io;

/// Fill `buf` from the OS CSPRNG.
pub fn fill(buf: &mut [u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(buf)
    }
    #[cfg(not(unix))]
    {
        let _ = buf;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no OS entropy source (agentd reads /dev/urandom)",
        ))
    }
}

/// `n_bytes` of OS randomness as lowercase hex (`2 * n_bytes` characters).
///
/// # Panics
///
/// When the entropy source cannot be read. A secret is either unguessable or
/// not issued: every caller mints something a stranger must not guess, and
/// no answer to "what do I hand out instead" is better than stopping.
pub fn hex_token(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    if let Err(e) = fill(&mut buf) {
        panic!("OS randomness unavailable, refusing to mint a token: {e}");
    }
    crate::sha::to_hex(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_tokens_are_distinct_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let t = hex_token(32);
            assert_eq!(t.len(), 64, "{t}");
            assert!(
                t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{t}"
            );
            assert!(seen.insert(t.clone()), "duplicate token {t}");
        }
    }

    #[test]
    fn fill_writes_every_byte() {
        // Random data has about 16 zero bytes in 4 KiB; a stub that returns
        // Ok without reading leaves all 4096.
        let mut buf = vec![0u8; 4096];
        fill(&mut buf).expect("OS randomness");
        assert!(buf.iter().filter(|&&b| b == 0).count() < 256, "unfilled");
        let mut empty = [];
        fill(&mut empty).expect("an empty fill is a no-op");
    }
}
