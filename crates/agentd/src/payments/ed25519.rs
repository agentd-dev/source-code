// SPDX-License-Identifier: AGPL-3.0-only
//! Whether 32 bytes are a point of Ed25519 (RFC 0049 §10): the one curve
//! question a Solana address asks that `ring` does not answer.
//!
//! A program-derived address — an associated token account among them — is
//! a SHA-256 that is *not* a point, so no private key can sign for it;
//! deriving one tries bump seeds until the hash is off the curve. `ring`
//! signs and verifies with Ed25519 but exposes no point decoding, and no
//! other crate is taken, so the check is hand-rolled here: field arithmetic
//! modulo `p = 2^255 - 19` in five 51-bit limbs, as `config::decrypt`'s
//! X25519 has, and one exponentiation.
//!
//! The rule is the Solana runtime's — curve25519-dalek's
//! `CompressedEdwardsY::decompress` succeeding: `y` is the low 255 bits read
//! modulo `p` (the sign bit, and whether `y` was written in its canonical
//! form, are not looked at), and the bytes are a point when `x² = (y² - 1) /
//! (d·y² + 1)` has a root, which is when `(y² - 1)·(d·y² + 1)` is zero or a
//! square — Euler's criterion. Every input here is public (keys, mints,
//! hashes), so nothing is held to constant time.

/// A field element: five 51-bit limbs, little-endian, each kept below 2^52
/// between operations.
#[derive(Clone, Copy)]
struct Fe([u64; 5]);

const MASK: u64 = (1 << 51) - 1;

/// `(p - 1) / 2 = 2^254 - 10`, little-endian.
const HALF_P_MINUS_1: [u8; 32] = {
    let mut e = [0xff; 32];
    e[0] = 0xf6;
    e[31] = 0x3f;
    e
};
/// `p - 2 = 2^255 - 21`, little-endian: `x^(p-2)` is `1/x`.
const P_MINUS_2: [u8; 32] = {
    let mut e = [0xff; 32];
    e[0] = 0xeb;
    e[31] = 0x7f;
    e
};

impl Fe {
    const ZERO: Fe = Fe([0; 5]);
    const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    fn small(n: u64) -> Fe {
        Fe([n, 0, 0, 0, 0])
    }

    /// The low 255 bits of `b`, little-endian.
    fn from_bytes(b: &[u8; 32]) -> Fe {
        let load = |i: usize| -> u64 {
            let mut v = [0u8; 8];
            v.copy_from_slice(&b[i..i + 8]);
            u64::from_le_bytes(v)
        };
        Fe([
            load(0) & MASK,
            (load(6) >> 3) & MASK,
            (load(12) >> 6) & MASK,
            (load(19) >> 1) & MASK,
            (load(24) >> 12) & MASK,
        ])
    }

    /// Each limb's carry into the next, the top one's folded back as 19
    /// (`2^255 ≡ 19`).
    fn carry(self) -> Fe {
        let mut h = self.0;
        for i in 0..4 {
            h[i + 1] += h[i] >> 51;
            h[i] &= MASK;
        }
        let c = h[4] >> 51;
        h[4] &= MASK;
        h[0] += c * 19;
        Fe(h)
    }

    /// The value's canonical representative, below `p`, as 32 bytes.
    fn to_bytes(self) -> [u8; 32] {
        let mut h = self.carry().0;
        while h.iter().any(|l| *l > MASK) {
            h = Fe(h).carry().0;
        }
        // Below 2^255 now; at least `p` exactly when adding 19 reaches 2^255.
        let mut q = (h[0] + 19) >> 51;
        for limb in &h[1..] {
            q = (limb + q) >> 51;
        }
        h[0] += 19 * q;
        for i in 0..4 {
            h[i + 1] += h[i] >> 51;
            h[i] &= MASK;
        }
        h[4] &= MASK;
        let mut out = [0u8; 32];
        let (mut acc, mut bits, mut at) = (0u128, 0, 0);
        for limb in h {
            acc |= u128::from(limb) << bits;
            bits += 51;
            while bits >= 8 && at < 32 {
                out[at] = acc as u8;
                acc >>= 8;
                bits -= 8;
                at += 1;
            }
        }
        if at < 32 {
            out[at] = acc as u8;
        }
        out
    }

    fn add(self, o: Fe) -> Fe {
        let mut h = self.0;
        for (a, b) in h.iter_mut().zip(o.0) {
            *a += b;
        }
        Fe(h).carry()
    }

    /// `self - o`, with `4p` added first so no limb goes below zero.
    fn sub(self, o: Fe) -> Fe {
        let four_p = [4 * (MASK - 18), 4 * MASK, 4 * MASK, 4 * MASK, 4 * MASK];
        let mut h = [0u64; 5];
        for i in 0..5 {
            h[i] = self.0[i] + four_p[i] - o.0[i];
        }
        Fe(h).carry()
    }

    fn mul(self, o: Fe) -> Fe {
        let (a, b) = (self.0, o.0);
        let m = |x: u64, y: u64| u128::from(x) * u128::from(y);
        let t = [
            m(a[0], b[0]) + 19 * (m(a[1], b[4]) + m(a[2], b[3]) + m(a[3], b[2]) + m(a[4], b[1])),
            m(a[0], b[1]) + m(a[1], b[0]) + 19 * (m(a[2], b[4]) + m(a[3], b[3]) + m(a[4], b[2])),
            m(a[0], b[2]) + m(a[1], b[1]) + m(a[2], b[0]) + 19 * (m(a[3], b[4]) + m(a[4], b[3])),
            m(a[0], b[3]) + m(a[1], b[2]) + m(a[2], b[1]) + m(a[3], b[0]) + 19 * m(a[4], b[4]),
            m(a[0], b[4]) + m(a[1], b[3]) + m(a[2], b[2]) + m(a[3], b[1]) + m(a[4], b[0]),
        ];
        let mut h = [0u64; 5];
        let mut c: u128 = 0;
        for i in 0..5 {
            let v = t[i] + c;
            h[i] = (v as u64) & MASK;
            c = v >> 51;
        }
        let v = u128::from(h[0]) + c * 19;
        h[0] = (v as u64) & MASK;
        h[1] += (v >> 51) as u64;
        Fe(h).carry()
    }

    /// `self^e`, `e` little-endian: square and multiply from the top bit.
    fn pow(self, e: &[u8; 32]) -> Fe {
        let mut r = Fe::ONE;
        for i in (0..256).rev() {
            r = r.mul(r);
            if (e[i / 8] >> (i % 8)) & 1 == 1 {
                r = r.mul(self);
            }
        }
        r
    }

    fn eq(self, o: Fe) -> bool {
        self.to_bytes() == o.to_bytes()
    }
}

/// Edwards25519's `d = -121665 / 121666`.
fn d() -> Fe {
    Fe::ZERO
        .sub(Fe::small(121_665))
        .mul(Fe::small(121_666).pow(&P_MINUS_2))
}

/// Whether `bytes`, read as a compressed Edwards `y`, is a point of the
/// curve — the Solana runtime's `is_on_curve`. An address that is not can
/// have no private key.
pub fn is_on_curve(bytes: &[u8; 32]) -> bool {
    let y = Fe::from_bytes(bytes);
    let yy = y.mul(y);
    let u = yy.sub(Fe::ONE);
    let v = d().mul(yy).add(Fe::ONE);
    let chi = u.mul(v).pow(&HALF_P_MINUS_1);
    chi.eq(Fe::ZERO) || chi.eq(Fe::ONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex32(h: &str) -> [u8; 32] {
        let v: Vec<u8> = (0..32)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        v.try_into().unwrap()
    }

    /// The field: `d` is RFC 8032's constant, `x · 1/x = 1`, `p` and `p + 1`
    /// read as 0 and 1, and arithmetic round-trips through the bytes.
    #[test]
    fn the_field_is_gf_2_255_minus_19() {
        assert_eq!(
            crate::sha::to_hex(&d().to_bytes()),
            "a3785913ca4deb75abd841414d0a700098e879777940c78c73fe6f2bee6c0352"
        );
        let x = Fe::from_bytes(&[0x5a; 32]);
        assert!(x.mul(x.pow(&P_MINUS_2)).eq(Fe::ONE));
        let p = hex32("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f");
        assert!(Fe::from_bytes(&p).eq(Fe::ZERO));
        let mut p1 = p;
        p1[0] += 1;
        assert!(Fe::from_bytes(&p1).eq(Fe::ONE));
        assert_eq!(
            Fe::small(5).sub(Fe::small(7)).add(Fe::small(2)).to_bytes(),
            [0u8; 32]
        );
        let mut minus_one = p;
        minus_one[0] -= 1;
        assert_eq!(Fe::ZERO.sub(Fe::ONE).to_bytes(), minus_one);
    }

    /// Whether bytes are a point, as @solana/web3.js's `isOnCurve` (noble's
    /// strict decoding) answers for canonical `y`: small values, values just
    /// below `p`, and Solana's program ids — the SPL Token program's id is
    /// a point, the compute-budget program's is not.
    #[test]
    fn points_and_not_as_solana_reads_them() {
        let small = [
            true, true, false, true, true, true, true, false, false, true, true, false,
        ];
        for (i, want) in small.iter().enumerate() {
            let mut b = [0u8; 32];
            b[0] = i as u8;
            assert_eq!(is_on_curve(&b), *want, "y = {i}");
        }
        for (i, want) in [true, false, true, true, true, true].iter().enumerate() {
            let mut b = [0xff; 32];
            b[0] = 0xec - i as u8;
            b[31] = 0x7f;
            assert_eq!(is_on_curve(&b), *want, "y = p - {}", 1 + i);
        }
        for (id, want) in [
            (
                "06ddf6e1d765a193d9cbe146ceeb79ac1cb485ed5f5b37913a8cf5857eff00a9",
                true,
            ),
            (
                "8c97258f4e2489f1bb3d1029148e0d830b5a1399daff1084048e7bd8dbe9f859",
                true,
            ),
            (
                "0306466fe5211732ffecadba72c39be7bc8ce5bbc5f7126b2c439b3a40000000",
                false,
            ),
            (
                "054a535a992921064d24e87160da387c7c35b5ddbc92bb81e41fa8404105448d",
                true,
            ),
            (
                "06ddf6e1ee758fde18425dbce46ccddab61afc4d83b90d27febdf928d8a18bfc",
                true,
            ),
            (
                "c6fa7af3bedbad3a3d65f36aabc97431b1bbe4c2d2f6e0e47ca60203452f5d61",
                true,
            ),
        ] {
            assert_eq!(is_on_curve(&hex32(id)), want, "{id}");
        }
    }

    /// As the runtime decodes: the sign bit is not looked at, and a `y`
    /// written past `p` is read modulo `p` — `p` itself is `y = 0`, whose
    /// `x² = -1` has a root.
    #[test]
    fn the_sign_bit_and_a_non_canonical_y_are_read_as_dalek_does() {
        for i in 0..12u8 {
            let mut b = [0u8; 32];
            b[0] = i;
            let plain = is_on_curve(&b);
            b[31] |= 0x80;
            assert_eq!(is_on_curve(&b), plain, "y = {i} with the sign bit");
        }
        let p = hex32("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f");
        assert!(is_on_curve(&p));
    }
}
