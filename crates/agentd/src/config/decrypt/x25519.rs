// SPDX-License-Identifier: AGPL-3.0-only
//! **X25519 scalar multiplication** (RFC 7748), hand-rolled.
//!
//! Why hand-rolled when `ring` is in the tree: ring's agreement API is
//! deliberately *ephemeral-only* — a private key can be generated but never
//! imported — and decryption inherently agrees with a STATIC recipient key
//! (the agent's). So the recipient side of age and JWE `ECDH-ES` needs a
//! from-bytes X25519, and RFC 7748's Montgomery ladder is the one primitive
//! small and well-specified enough to own: ~150 lines of field arithmetic with
//! published test vectors, no secret-indexed table lookups (the reason
//! hand-rolling AES was rejected), and a conditional swap done with masks so
//! there is no secret-dependent branch.
//!
//! Verified three ways in the tests below: the RFC 7748 §5.2 iteration vectors,
//! the §6.1 Diffie-Hellman vector, and a live cross-check against `ring`'s own
//! ephemeral side (ring generates a keypair and agrees with our public key; our
//! ladder must derive the same shared secret from ring's public key).

/// Field element in GF(2^255 - 19): five 51-bit limbs, little-endian.
#[derive(Clone, Copy)]
struct Fe([u64; 5]);

const MASK51: u64 = (1 << 51) - 1;

impl Fe {
    const ZERO: Fe = Fe([0; 5]);
    const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    fn from_bytes(b: &[u8; 32]) -> Fe {
        let load = |i: usize| -> u64 {
            let mut v = [0u8; 8];
            v.copy_from_slice(&b[i..i + 8]);
            u64::from_le_bytes(v)
        };
        Fe([
            load(0) & MASK51,
            (load(6) >> 3) & MASK51,
            (load(12) >> 6) & MASK51,
            (load(19) >> 1) & MASK51,
            (load(24) >> 12) & MASK51,
        ])
    }

    fn to_bytes(mut self) -> [u8; 32] {
        self = self.carry();
        self = self.carry();
        // Freeze: subtract p if >= p, twice to be safe after carries.
        for _ in 0..2 {
            let mut borrow = 19u64; // add 19, propagate, then mask top — the
            // standard trick: t = h + 19; if t >= 2^255 then h -= p.
            let mut t = self.0;
            for limb in &mut t {
                let v = *limb + borrow;
                borrow = v >> 51;
                *limb = v & MASK51;
            }
            // borrow is now the carry out of the top limb: 1 iff h + 19 >= 2^255.
            let swap = borrow.wrapping_neg(); // all-ones if h >= p
            let mut reduced = self.0;
            let mut carry = 19u64;
            for r in &mut reduced {
                let v = *r + carry;
                carry = v >> 51;
                *r = v & MASK51;
            }
            for (limb, red) in self.0.iter_mut().zip(reduced) {
                *limb = (red & swap) | (*limb & !swap);
            }
        }
        let h = self.0;
        let mut out = [0u8; 32];
        let mut acc: u128 = 0;
        let mut bits = 0;
        let mut idx = 0;
        for limb in h {
            acc |= (limb as u128) << bits;
            bits += 51;
            while bits >= 8 && idx < 32 {
                out[idx] = (acc & 0xff) as u8;
                acc >>= 8;
                bits -= 8;
                idx += 1;
            }
        }
        // 5 × 51 = 255 bits: the final byte holds only 7 and must be flushed.
        if idx < 32 {
            out[idx] = (acc & 0xff) as u8;
        }
        out
    }

    fn carry(self) -> Fe {
        let mut h = self.0;
        let mut c: u64;
        for i in 0..4 {
            c = h[i] >> 51;
            h[i] &= MASK51;
            h[i + 1] += c;
        }
        c = h[4] >> 51;
        h[4] &= MASK51;
        h[0] += c * 19;
        Fe(h)
    }

    fn add(self, o: Fe) -> Fe {
        let mut h = [0u64; 5];
        for ((hi, a), b) in h.iter_mut().zip(self.0).zip(o.0) {
            *hi = a + b;
        }
        Fe(h).carry()
    }

    fn sub(self, o: Fe) -> Fe {
        // Add 2p before subtracting so limbs never underflow.
        let p2: [u64; 5] = [
            2 * (MASK51 - 18), // 2 * (2^51 - 19)
            2 * MASK51,
            2 * MASK51,
            2 * MASK51,
            2 * MASK51,
        ];
        let mut h = [0u64; 5];
        for i in 0..5 {
            h[i] = self.0[i] + p2[i] - o.0[i];
        }
        Fe(h).carry().carry()
    }

    fn mul(self, o: Fe) -> Fe {
        let a = self.0;
        let b = o.0;
        let m = |x: u64, y: u64| x as u128 * y as u128;
        // Schoolbook with 19-fold wraparound.
        let mut t = [0u128; 5];
        t[0] = m(a[0], b[0]) + 19 * (m(a[1], b[4]) + m(a[2], b[3]) + m(a[3], b[2]) + m(a[4], b[1]));
        t[1] = m(a[0], b[1]) + m(a[1], b[0]) + 19 * (m(a[2], b[4]) + m(a[3], b[3]) + m(a[4], b[2]));
        t[2] = m(a[0], b[2]) + m(a[1], b[1]) + m(a[2], b[0]) + 19 * (m(a[3], b[4]) + m(a[4], b[3]));
        t[3] = m(a[0], b[3]) + m(a[1], b[2]) + m(a[2], b[1]) + m(a[3], b[0]) + 19 * m(a[4], b[4]);
        t[4] = m(a[0], b[4]) + m(a[1], b[3]) + m(a[2], b[2]) + m(a[3], b[1]) + m(a[4], b[0]);
        // Carry chain over u128 partials.
        let mut h = [0u64; 5];
        let mut carry: u128 = 0;
        for i in 0..5 {
            let v = t[i] + carry;
            h[i] = (v as u64) & MASK51;
            carry = v >> 51;
        }
        // carry wraps to limb 0 multiplied by 19.
        let mut h0 = h[0] as u128 + carry * 19;
        h[0] = (h0 as u64) & MASK51;
        h0 >>= 51;
        h[1] += h0 as u64;
        Fe(h).carry()
    }

    fn square(self) -> Fe {
        self.mul(self)
    }

    /// Multiplicative inverse via Fermat: a^(p-2), p-2 = 2^255 - 21.
    fn invert(self) -> Fe {
        // Simple square-and-multiply over the fixed exponent bits (not
        // secret-dependent: the exponent is a public constant).
        // p - 2 = 2^255 - 21 → binary: 253 ones, then 0,1,0,1,1 (low bits of
        // …fffeb). Compute via the standard chain: 250 ones + tail.
        let mut result = Fe::ONE;
        let base = self;
        // exponent bytes little-endian of p-2:
        let mut e = [0xffu8; 32];
        e[0] = 0xeb;
        e[31] = 0x7f;
        // left-to-right over bits 254..=0
        for i in (0..255).rev() {
            result = result.square();
            if (e[i / 8] >> (i % 8)) & 1 == 1 {
                result = result.mul(base);
            }
        }
        result
    }

    fn cswap(a: &mut Fe, b: &mut Fe, swap: u64) {
        let mask = swap.wrapping_neg();
        for i in 0..5 {
            let x = mask & (a.0[i] ^ b.0[i]);
            a.0[i] ^= x;
            b.0[i] ^= x;
        }
    }
}

/// RFC 7748 scalar clamping.
fn clamp(scalar: &[u8; 32]) -> [u8; 32] {
    let mut s = *scalar;
    s[0] &= 248;
    s[31] &= 127;
    s[31] |= 64;
    s
}

/// X25519(scalar, u): the Montgomery ladder.
pub fn scalarmult(scalar: &[u8; 32], u: &[u8; 32]) -> [u8; 32] {
    let k = clamp(scalar);
    let mut ub = *u;
    ub[31] &= 127; // mask the unused top bit of the u-coordinate
    let x1 = Fe::from_bytes(&ub);
    let mut x2 = Fe::ONE;
    let mut z2 = Fe::ZERO;
    let mut x3 = x1;
    let mut z3 = Fe::ONE;
    let mut swap = 0u64;
    const A24: u64 = 121665;

    for t in (0..255).rev() {
        let kt = ((k[t / 8] >> (t % 8)) & 1) as u64;
        swap ^= kt;
        Fe::cswap(&mut x2, &mut x3, swap);
        Fe::cswap(&mut z2, &mut z3, swap);
        swap = kt;

        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.sub(z2);
        let bb = b.square();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).square();
        z3 = x1.mul(da.sub(cb).square());
        x2 = aa.mul(bb);
        let mut a24e = e;
        // a24 * e — multiply by small constant via a one-limb Fe.
        a24e = a24e.mul(Fe([A24, 0, 0, 0, 0]));
        z2 = e.mul(aa.add(a24e));
    }
    Fe::cswap(&mut x2, &mut x3, swap);
    Fe::cswap(&mut z2, &mut z3, swap);
    x2.mul(z2.invert()).to_bytes()
}

/// The public key for a private scalar: X25519(scalar, 9).
pub fn public_key(scalar: &[u8; 32]) -> [u8; 32] {
    let mut base = [0u8; 32];
    base[0] = 9;
    scalarmult(scalar, &base)
}

/// Diffie-Hellman: the shared secret between our private scalar and a peer's
/// public u-coordinate. An all-zero result (a small-order peer point) is
/// refused, as RFC 7748 §6.1 requires.
pub fn agree(private: &[u8; 32], peer_public: &[u8; 32]) -> Result<[u8; 32], String> {
    let shared = scalarmult(private, peer_public);
    if shared.iter().all(|&b| b == 0) {
        return Err("x25519: the peer public key has small order — refused".into());
    }
    Ok(shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn rfc7748_section_5_2_vector() {
        let k = hex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
        let u = hex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
        let out = scalarmult(&k, &u);
        assert_eq!(
            out,
            hex("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552")
        );
    }

    #[test]
    fn rfc7748_section_6_1_diffie_hellman() {
        let a_priv = hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let b_priv = hex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        let a_pub = public_key(&a_priv);
        let b_pub = public_key(&b_priv);
        assert_eq!(
            a_pub,
            hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
        );
        assert_eq!(
            b_pub,
            hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
        );
        let shared = hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        assert_eq!(agree(&a_priv, &b_pub).unwrap(), shared);
        assert_eq!(agree(&b_priv, &a_pub).unwrap(), shared);
    }

    #[test]
    fn cross_checks_against_ring_ephemeral_agreement() {
        // ring generates an ephemeral pair and agrees with OUR static public
        // key; our ladder must derive the same secret from ring's public key.
        use ring::agreement::{EphemeralPrivateKey, UnparsedPublicKey, X25519, agree_ephemeral};
        let rng = ring::rand::SystemRandom::new();
        let ours_priv = {
            use ring::rand::SecureRandom;
            let mut b = [0u8; 32];
            rng.fill(&mut b).unwrap();
            b
        };
        let ours_pub = public_key(&ours_priv);

        let ring_priv = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
        let ring_pub: [u8; 32] = ring_priv
            .compute_public_key()
            .unwrap()
            .as_ref()
            .try_into()
            .unwrap();
        let ring_shared: Vec<u8> =
            agree_ephemeral(ring_priv, &UnparsedPublicKey::new(&X25519, ours_pub), |s| {
                s.to_vec()
            })
            .unwrap();
        let our_shared = agree(&ours_priv, &ring_pub).unwrap();
        assert_eq!(ring_shared.as_slice(), our_shared.as_slice());
    }

    #[test]
    fn small_order_points_are_refused() {
        let priv_ = [7u8; 32];
        assert!(agree(&priv_, &[0u8; 32]).is_err());
        let mut one = [0u8; 32];
        one[0] = 1;
        assert!(agree(&priv_, &one).is_err());
    }
}
