// SPDX-License-Identifier: AGPL-3.0-only
//! **scrypt** (RFC 7914) — the KDF age's passphrase stanzas use.
//!
//! `ring` carries the outer layers (PBKDF2-HMAC-SHA256) but not scrypt itself;
//! the remainder — BlockMix over the Salsa20/8 core, and ROMix — is ~80 lines
//! of pure, well-specified computation with published test vectors, so it is
//! hand-rolled rather than pulling a crate (the minimalism moat). A KDF's
//! mixing core is public data flow (no secret-indexed lookups), so the usual
//! argument against hand-rolled ciphers does not apply here.

use std::num::NonZeroU32;

/// scrypt(password, salt, N=2^log_n, r, p) → `dk_len` bytes.
pub fn scrypt(password: &[u8], salt: &[u8], log_n: u8, r: u32, p: u32, dk_len: usize) -> Vec<u8> {
    let n: usize = 1usize << log_n;
    let block_bytes = 128 * r as usize;
    // B = PBKDF2-HMAC-SHA256(password, salt, 1, p * 128 * r)
    let mut b = vec![0u8; p as usize * block_bytes];
    pbkdf2(password, salt, &mut b);
    for chunk in b.chunks_mut(block_bytes) {
        romix(chunk, n, r as usize);
    }
    let mut dk = vec![0u8; dk_len];
    pbkdf2(password, &b, &mut dk);
    dk
}

fn pbkdf2(password: &[u8], salt: &[u8], out: &mut [u8]) {
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(1).unwrap(),
        salt,
        password,
        out,
    );
}

/// ROMix (RFC 7914 §5): N iterations of fill, N of integerify-and-mix.
fn romix(block: &mut [u8], n: usize, r: usize) {
    let words = block_to_words(block);
    let wlen = words.len();
    let mut x = words;
    let mut v = vec![0u32; n * wlen];
    for i in 0..n {
        v[i * wlen..(i + 1) * wlen].copy_from_slice(&x);
        blockmix(&mut x, r);
    }
    for _ in 0..n {
        // Integerify: the first word of the last 64-byte sub-block, mod N.
        let j = (x[(2 * r - 1) * 16] as usize) & (n - 1);
        for (xi, vi) in x.iter_mut().zip(&v[j * wlen..(j + 1) * wlen]) {
            *xi ^= vi;
        }
        blockmix(&mut x, r);
    }
    words_to_block(&x, block);
}

/// scryptBlockMix (RFC 7914 §4): Salsa20/8 chained over 2r 64-byte sub-blocks,
/// output shuffled even-then-odd.
fn blockmix(b: &mut [u32], r: usize) {
    let mut x: [u32; 16] = b[(2 * r - 1) * 16..].try_into().unwrap();
    let mut out = vec![0u32; b.len()];
    for i in 0..2 * r {
        for (xw, bw) in x.iter_mut().zip(&b[i * 16..(i + 1) * 16]) {
            *xw ^= bw;
        }
        salsa20_8(&mut x);
        let dst = if i % 2 == 0 { i / 2 } else { r + i / 2 };
        out[dst * 16..(dst + 1) * 16].copy_from_slice(&x);
    }
    b.copy_from_slice(&out);
}

/// The Salsa20/8 core: 4 double-rounds plus the feed-forward add.
fn salsa20_8(x: &mut [u32; 16]) {
    let input = *x;
    for _ in 0..4 {
        // Column round.
        qr(x, 4, 0, 12, 8);
        qr(x, 9, 5, 1, 13);
        qr(x, 14, 10, 6, 2);
        qr(x, 3, 15, 11, 7);
        // Row round.
        qr(x, 1, 0, 3, 2);
        qr(x, 6, 5, 4, 7);
        qr(x, 11, 10, 9, 8);
        qr(x, 12, 15, 14, 13);
    }
    for (o, i) in x.iter_mut().zip(input) {
        *o = o.wrapping_add(i);
    }
}

#[inline]
fn qr(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[a] ^= x[b].wrapping_add(x[c]).rotate_left(7);
    x[d] ^= x[a].wrapping_add(x[b]).rotate_left(9);
    x[c] ^= x[d].wrapping_add(x[a]).rotate_left(13);
    x[b] ^= x[c].wrapping_add(x[d]).rotate_left(18);
}

fn block_to_words(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect()
}

fn words_to_block(w: &[u32], out: &mut [u8]) {
    for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(w) {
        *chunk = word.to_le_bytes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 7914 §12 test vectors (the two fast enough for a unit test).
    #[test]
    fn rfc7914_vectors() {
        // scrypt("", "", N=16, r=1, p=1, 64)
        let dk = scrypt(b"", b"", 4, 1, 1, 64);
        assert_eq!(
            dk,
            hex(
                "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442\
                 fcd0069ded0948f8326a753a0fc81f17e8d3e0fb2e0d3628cf35e20c38d18906"
            )
        );
        // scrypt("password", "NaCl", N=1024, r=8, p=16, 64)
        let dk = scrypt(b"password", b"NaCl", 10, 8, 16, 64);
        assert_eq!(
            dk,
            hex(
                "fdbabe1c9d3472007856e7190d01e9fe7c6ad7cbc8237830e77376634b373162\
                 2eaf30d92e22a3886ff109279d9830dac727afb94a83ee6d8360cbdfa2cc0640"
            )
        );
    }
}
