// SPDX-License-Identifier: Apache-2.0
//! base64 helpers for AAuth (RFC 0023): url-safe **unpadded** (JWK/JWT, per RFC
//! 4648 §5 + RFC 7515) and standard **padded** (the RFC 9421 `:…:` signature
//! byte-sequence). Hand-rolled — no `base64` crate (the minimalism moat).

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode(input: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(alphabet[(n >> 18 & 63) as usize] as char);
        out.push(alphabet[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(alphabet[(n >> 6 & 63) as usize] as char);
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[(n & 63) as usize] as char);
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// Standard base64, padded (RFC 9421 signature/byte-sequence values).
pub(crate) fn std_pad(input: &[u8]) -> String {
    encode(input, STD, true)
}

/// URL-safe base64, UNPADDED (JWK coordinates, JWT segments).
pub(crate) fn url_nopad(input: &[u8]) -> String {
    encode(input, URL, false)
}

/// Decode a URL-safe base64 string (padding tolerated) — for reading a stored
/// key seed. Standard `+/` are also accepted (lenient).
pub(crate) fn url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' => continue,
            _ => return Err("invalid base64 character".into()),
        };
        bits = bits << 6 | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_matches_known_vectors() {
        // RFC 4648 test vectors (standard, padded).
        assert_eq!(std_pad(b""), "");
        assert_eq!(std_pad(b"f"), "Zg==");
        assert_eq!(std_pad(b"fo"), "Zm8=");
        assert_eq!(std_pad(b"foo"), "Zm9v");
        assert_eq!(std_pad(b"foobar"), "Zm9vYmFy");
        // URL-safe unpadded round-trips.
        for v in [
            &b"".to_vec(),
            &b"f".to_vec(),
            &b"any32bytes-seed-goes-here-ok!!".to_vec(),
        ] {
            assert_eq!(url_decode(&url_nopad(v)).unwrap(), *v);
        }
        // URL alphabet uses -_ not +/ (0xfb 0xff 0xbf → "-_-_" region).
        assert_eq!(url_nopad(&[0xfb, 0xff]), "-_8");
    }
}
