// SPDX-License-Identifier: AGPL-3.0-only
//! A compact, dependency-free SHA-256 (FIPS 180-4) — content identity: workflow
//! hashes, skill body hashes, artifact digests. A checkpoint envelope binds the
//! graph it was taken from by
//! `sha256(canonical graph JSON)`; resume refuses a mismatch. Hand-rolled like
//! the cron parser and FNV-1a (the minimalism moat): ~60 lines, byte-oriented,
//! verified against the FIPS/NIST test vectors below. Also backs
//! [`hmac_sha256`] for inbound webhook signature verification (RFC 2104);
//! agentd's own outbound request signing (RFC 9421) uses `ring` under aauth.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of `bytes` — the raw 32-byte digest.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Pad: message || 0x80 || zeros || 64-bit big-endian bit length.
    let bitlen = (bytes.len() as u64).wrapping_mul(8);
    let mut msg = bytes.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    let mut w = [0u32; 64];
    // `as_chunks` rather than `chunks_exact`: the chunk size is a constant, so
    // the compiler hands back fixed-size ARRAYS and `from_be_bytes` takes one
    // directly — no indexing and no bounds checks to elide. The padding above
    // makes the length an exact multiple of 64, so the remainder is empty by
    // construction; the compression still runs once PER block.
    let (blocks, _) = msg.as_chunks::<64>();
    for block in blocks {
        let (words, _) = block.as_chunks::<4>();
        for (i, c) in words.iter().enumerate() {
            w[i] = u32::from_be_bytes(*c);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *s = s.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = sha256(bytes);
    let mut out = String::with_capacity(64);
    for b in d {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// HMAC-SHA256 (RFC 2104), hand-rolled over [`sha256`] — for inbound **webhook
/// signature verification** (GitHub/Stripe-style `X-Signature: sha256=…`). Kept
/// dependency-free like the rest of the moat; agentd's own outbound request
/// signing (RFC 9421) uses `ring` under `--features aauth`.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(BLOCK + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_digest = sha256(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_digest);
    sha256(&outer)
}

/// Constant-time byte-slice equality (short-circuits on a length mismatch only).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Lowercase hex of a byte slice.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    /// A message spanning SEVERAL blocks.
    ///
    /// The single-block vectors cannot catch a compression loop that flattens
    /// the word schedule across every block at once: that only goes wrong from
    /// block two, where `w` is indexed past 64. This vector covers it.
    #[test]
    fn a_multi_block_message_digests_correctly() {
        // openssl: printf 'a%.0s' {1..200} | sha256sum
        assert_eq!(
            sha256_hex(&[b'a'; 200]),
            "c2a908d98f5df987ade41b5fce213067efbcc21ef2240212a41e54b5e7c28ae5"
        );
        // The boundary cases either side of one block: 64 bytes pads into a
        // SECOND block, 63 fits in one.
        assert_eq!(
            sha256_hex(&[b'b'; 64]),
            "a0fab1377f49a759b57f63318262ebe89fabfc990e8e93ceac2984561482b9d4"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 63]),
            "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34"
        );
    }

    use super::sha256_hex;

    /// FIPS 180-4 / NIST CAVP known-answer vectors.
    #[test]
    fn known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // The NIST million-'a' vector — exercises many blocks + the padding
        // boundary paths.
        assert_eq!(
            sha256_hex("a".repeat(1_000_000).as_bytes()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231() {
        use super::{ct_eq, hmac_sha256, to_hex};
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A > block-size key (case 6) hashes the key first; just assert it runs
        // and constant-time-compares to itself.
        let long = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert!(ct_eq(&long, &long.clone()));
        assert!(!ct_eq(&mac, &long));
    }
}
