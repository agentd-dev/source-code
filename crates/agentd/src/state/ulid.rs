// SPDX-License-Identifier: Apache-2.0
//! A dependency-free **ULID** (Universally Unique Lexicographically Sortable
//! Identifier): 48-bit ms timestamp + 80 random bits, Crockford base32, 26
//! chars, monotonic within one process for the same millisecond. Used for
//! inbox events, runs, artifacts, audit records — sortable by time in the
//! store's `list` and stable across restarts.

use std::sync::Mutex;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

struct Last {
    ms: u64,
    rand: u128, // low 80 bits used
}

static LAST: Mutex<Last> = Mutex::new(Last { ms: 0, rand: 0 });

/// A new ULID for `now`.
pub fn new() -> String {
    let ms = super::now_ms();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let rand = if ms == last.ms {
        // Same millisecond: increment (monotonic), wrapping inside 80 bits.
        (last.rand + 1) & ((1u128 << 80) - 1)
    } else {
        random80()
    };
    last.ms = ms;
    last.rand = rand;
    encode(ms, rand)
}

/// The timestamp (ms) encoded in a ULID, if it parses.
pub fn timestamp_ms(ulid: &str) -> Option<u64> {
    if ulid.len() != 26 {
        return None;
    }
    let mut ts: u64 = 0;
    for c in ulid.bytes().take(10) {
        let v = decode_char(c)?;
        ts = (ts << 5) | v as u64;
    }
    Some(ts)
}

fn encode(ms: u64, rand: u128) -> String {
    let mut out = [0u8; 26];
    let mut t = ms;
    for i in (0..10).rev() {
        out[i] = ALPHABET[(t & 31) as usize];
        t >>= 5;
    }
    let mut r = rand;
    for i in (10..26).rev() {
        out[i] = ALPHABET[(r & 31) as usize];
        r >>= 5;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_char(c: u8) -> Option<u8> {
    ALPHABET
        .iter()
        .position(|&a| a == c.to_ascii_uppercase())
        .map(|p| p as u8)
}

fn random80() -> u128 {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_ok();
    if !ok {
        // Fallback: a splitmix over time+pid+counter (never the primary path
        // on Linux, but never a duplicate within a process either).
        static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seed = super::now_ms()
            ^ (std::process::id() as u64).rotate_left(32)
            ^ CTR.fetch_add(0x9E37_79B9_7F4A_7C15, std::sync::atomic::Ordering::Relaxed);
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mix = || {
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^ (x >> 31)
        };
        let a = mix();
        let b = mix();
        buf[..8].copy_from_slice(&a.to_le_bytes());
        buf[8..].copy_from_slice(&b.to_le_bytes());
    }
    u128::from_le_bytes(buf) & ((1u128 << 80) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulids_are_26_chars_sortable_and_unique() {
        let a = new();
        let b = new();
        assert_eq!(a.len(), 26);
        assert!(a.bytes().all(|c| ALPHABET.contains(&c)));
        assert_ne!(a, b);
        assert!(a < b, "monotonic within a process: {a} < {b}");
        let ts = timestamp_ms(&a).unwrap();
        let now = super::super::now_ms();
        assert!(now >= ts && now - ts < 5_000);
        assert_eq!(timestamp_ms("short"), None);
        // Many in a tight loop stay unique and ordered.
        let mut prev = new();
        for _ in 0..1000 {
            let n = new();
            assert!(n > prev);
            prev = n;
        }
    }
}
