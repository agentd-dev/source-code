// SPDX-License-Identifier: AGPL-3.0-only
//! Base58, as Solana writes keys, mints, blockhashes and signatures (RFC
//! 0049 §10): Bitcoin's alphabet — the digits and letters without `0`, `O`,
//! `I` and `l` — big-endian, each leading zero byte a leading `1`. No
//! checksum: Solana's base58 has none, so a mistyped key is a different key,
//! and what an operator writes is held to its length (32 bytes) and the
//! allow-lists instead.

/// Bitcoin's alphabet.
const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// The longest text [`decode`] reads: a 64-byte signature is at most 88
/// characters, and nothing Solana writes is longer.
pub const MAX_TEXT: usize = 128;

/// `bytes` in base58.
pub fn encode(bytes: &[u8]) -> String {
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    // Base-58 digits, least significant first.
    let mut digits: Vec<u8> = Vec::with_capacity(bytes.len() * 138 / 100 + 1);
    for &b in &bytes[zeros..] {
        let mut carry = u32::from(b);
        for d in &mut digits {
            carry += u32::from(*d) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::with_capacity(zeros + digits.len());
    out.extend(std::iter::repeat_n('1', zeros));
    out.extend(
        digits
            .iter()
            .rev()
            .map(|d| ALPHABET[usize::from(*d)] as char),
    );
    out
}

/// The bytes `text` is: every character in the alphabet, at most
/// [`MAX_TEXT`] of them.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    if text.len() > MAX_TEXT {
        return Err(format!(
            "{} characters of base58 (at most {MAX_TEXT})",
            text.len()
        ));
    }
    let zeros = text.bytes().take_while(|c| *c == b'1').count();
    // Bytes, least significant first.
    let mut bytes: Vec<u8> = Vec::with_capacity(text.len());
    for c in text.bytes().skip(zeros) {
        let mut carry = ALPHABET
            .iter()
            .position(|a| *a == c)
            .ok_or_else(|| format!("{text:?} is not base58"))? as u32;
        for b in &mut bytes {
            carry += u32::from(*b) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut out = vec![0u8; zeros];
    out.extend(bytes.iter().rev());
    Ok(out)
}

/// The 32 bytes `text` is — a key, a mint or a blockhash — or why not.
pub fn decode32(text: &str) -> Result<[u8; 32], String> {
    let b = decode(text)?;
    <[u8; 32]>::try_from(b.as_slice())
        .map_err(|_| format!("{text:?} is {} bytes of base58, not 32", b.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bitcoin Core's base58 vectors (`base58_encode_decode.json`): each
    /// encodes to its text and decodes back, leading zeros as leading `1`s.
    #[test]
    fn bitcoins_vectors() {
        for (hex, text) in [
            ("", ""),
            ("61", "2g"),
            ("626262", "a3gV"),
            ("636363", "aPEr"),
            (
                "73696d706c792061206c6f6e6720737472696e67",
                "2cFupjhnEsSn59qHXstmK2ffpLv2",
            ),
            (
                "00eb15231dfceb60925886b67d065299925915aeb172c06647",
                "1NS17iag9jJgTHD1VXjvLCEnZuQ3rJDE9L",
            ),
            ("516b6fcd0f", "ABnLTmg"),
            ("bf4f89001e670274dd", "3SEo3LWLoPntC"),
            ("572e4794", "3EFU7m"),
            ("ecac89cad93923c02321", "EJDM8drfXA6uyA"),
            ("10c8511e", "Rt5zm"),
            ("00000000000000000000", "1111111111"),
        ] {
            let bytes: Vec<u8> = (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
                .collect();
            assert_eq!(encode(&bytes), text, "{hex}");
            assert_eq!(decode(text).unwrap(), bytes, "{text}");
        }
    }

    /// Solana's own ids: the system program is 32 zero bytes, and the SPL
    /// Token program's id is the bytes every Solana client has for it.
    #[test]
    fn solanas_program_ids() {
        assert_eq!(encode(&[0u8; 32]), "11111111111111111111111111111111");
        let token = decode32("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        assert_eq!(
            crate::sha::to_hex(&token),
            "06ddf6e1d765a193d9cbe146ceeb79ac1cb485ed5f5b37913a8cf5857eff00a9"
        );
        assert_eq!(
            encode(&token),
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );
    }

    /// A character outside the alphabet (`0`, `O`, `I`, `l`, a space), text
    /// past the bound, or 32 bytes asked of another length: refused.
    #[test]
    fn what_is_not_base58() {
        for bad in ["0", "O", "I", "l", "2g ", "+", "é"] {
            assert!(decode(bad).is_err(), "{bad:?}");
        }
        assert!(decode(&"2".repeat(MAX_TEXT + 1)).is_err());
        assert!(decode32("2g").unwrap_err().contains("not 32"));
        assert!(decode32(&encode(&[1u8; 33])).is_err());
        assert_eq!(decode32(&encode(&[9u8; 32])).unwrap(), [9u8; 32]);
    }
}
