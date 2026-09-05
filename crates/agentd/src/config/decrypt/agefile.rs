// SPDX-License-Identifier: AGPL-3.0-only
//! **age v1** (age-encryption.org/v1) — decrypt, and the encrypt half the
//! tests and operator tooling need.
//!
//! age is the format operators actually produce (`age -r age1… agent.md`), so
//! agentd speaks it natively: X25519 recipient stanzas and scrypt passphrase
//! stanzas, the HMAC'd header, the ChaCha20-Poly1305 STREAM payload, and the
//! ASCII armor. ChaCha20-Poly1305 / HKDF / HMAC / PBKDF2 are `ring`; the
//! X25519 ladder and scrypt's core are the sibling modules. The interop test
//! at the bottom drives the real `age` binary when one is installed.

use super::scrypt::scrypt;
use super::x25519;
use crate::config::envelope::{AGE_ARMOR, AGE_MAGIC};

const FILE_KEY_LEN: usize = 16;
const CHUNK: usize = 64 * 1024;
/// scrypt work factors above this are refused (memory = 128·8·2^n bytes; 20 is
/// already 1 GiB — enough for any legitimate file, a bound on a hostile one).
const MAX_SCRYPT_LOG_N: u8 = 20;

/// Decrypt an age file (binary or armored) with X25519 identities and/or a
/// passphrase. Every failure names what was missing or wrong.
pub fn decrypt(
    bytes: &[u8],
    identities: &[[u8; 32]],
    passphrase: Option<&str>,
) -> Result<Vec<u8>, String> {
    let bytes = unarmor_if_needed(bytes)?;
    let text_end = header_end(&bytes)?;
    let (header, payload) = bytes.split_at(text_end);
    let header_str =
        std::str::from_utf8(header).map_err(|_| "age: header is not UTF-8".to_string())?;

    // Parse: magic, stanzas, the MAC line.
    let mut lines = header_str.lines().peekable();
    if lines.next() != Some("age-encryption.org/v1") {
        return Err("age: missing version line".into());
    }
    let mut stanzas: Vec<(Vec<String>, Vec<u8>)> = Vec::new();
    let mut mac_line = None;
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("--- ") {
            mac_line = Some(rest.to_string());
            break;
        }
        let Some(rest) = line.strip_prefix("-> ") else {
            return Err(format!("age: unexpected header line {line:?}"));
        };
        let args: Vec<String> = rest.split(' ').map(str::to_string).collect();
        let mut body_b64 = String::new();
        // Body: full 64-char lines, then one final line shorter than 64.
        while let Some(l) = lines.peek() {
            if l.starts_with("-> ") || l.starts_with("--- ") {
                break;
            }
            let l = lines.next().unwrap();
            body_b64.push_str(l);
            if l.len() < 64 {
                break;
            }
        }
        let body = b64_decode_nopad(&body_b64).ok_or("age: bad stanza body base64")?;
        stanzas.push((args, body));
    }
    let mac_b64 = mac_line.ok_or("age: header has no MAC line")?;
    let mac = b64_decode_nopad(&mac_b64).ok_or("age: bad MAC base64")?;

    // Unwrap the file key with whichever stanza we hold material for.
    let file_key = unwrap_file_key(&stanzas, identities, passphrase)?;

    // Verify the header MAC (over everything through `---`).
    let mac_input_len = header_str
        .find("\n--- ")
        .map(|i| i + "\n---".len())
        .ok_or("age: malformed MAC line")?;
    let hmac_key = hkdf(&[], &file_key, b"header", 32);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &hmac_key);
    ring::hmac::verify(&key, &header[..mac_input_len], &mac)
        .map_err(|_| "age: header MAC does not verify — wrong key or corrupted file".to_string())?;

    // STREAM payload: 16-byte nonce, then 64 KiB chunks.
    if payload.len() < 16 {
        return Err("age: payload too short for its nonce".into());
    }
    let (nonce, mut ct) = payload.split_at(16);
    let payload_key = hkdf(nonce, &file_key, b"payload", 32);
    let mut out = Vec::new();
    let mut counter: u64 = 0;
    loop {
        let last = ct.len() <= CHUNK + 16;
        let take = if last { ct.len() } else { CHUNK + 16 };
        if take < 16 {
            return Err("age: truncated payload chunk".into());
        }
        let mut chunk = ct[..take].to_vec();
        let mut n = [0u8; 12];
        n[3..11].copy_from_slice(&counter.to_be_bytes());
        n[11] = u8::from(last);
        let pt = aead_open(&payload_key, &n, &mut chunk)?;
        if last && counter > 0 && pt.is_empty() {
            return Err("age: final chunk is empty".into());
        }
        out.extend_from_slice(pt);
        ct = &ct[take..];
        counter += 1;
        if last {
            break;
        }
    }
    Ok(out)
}

fn unwrap_file_key(
    stanzas: &[(Vec<String>, Vec<u8>)],
    identities: &[[u8; 32]],
    passphrase: Option<&str>,
) -> Result<[u8; FILE_KEY_LEN], String> {
    let mut seen = Vec::new();
    for (args, body) in stanzas {
        match args.first().map(String::as_str) {
            Some("X25519") if args.len() == 2 => {
                seen.push("X25519");
                let eph: [u8; 32] = b64_decode_nopad(&args[1])
                    .and_then(|v| v.try_into().ok())
                    .ok_or("age: bad X25519 stanza ephemeral key")?;
                for id in identities {
                    let Ok(shared) = x25519::agree(id, &eph) else {
                        continue;
                    };
                    let mut salt = Vec::with_capacity(64);
                    salt.extend_from_slice(&eph);
                    salt.extend_from_slice(&x25519::public_key(id));
                    let wrap = hkdf(&salt, &shared, b"age-encryption.org/v1/X25519", 32);
                    let mut buf = body.clone();
                    if let Ok(fk) = aead_open(&wrap, &[0u8; 12], &mut buf) {
                        return fk
                            .try_into()
                            .map_err(|_| "age: wrapped file key has the wrong size".into());
                    }
                }
            }
            Some("scrypt") if args.len() == 3 => {
                seen.push("scrypt");
                let Some(pass) = passphrase else { continue };
                let salt = b64_decode_nopad(&args[1]).ok_or("age: bad scrypt salt")?;
                let log_n: u8 = args[2].parse().map_err(|_| "age: bad scrypt work factor")?;
                if log_n > MAX_SCRYPT_LOG_N {
                    return Err(format!(
                        "age: scrypt work factor {log_n} exceeds the cap {MAX_SCRYPT_LOG_N}"
                    ));
                }
                let mut full_salt = b"age-encryption.org/v1/scrypt".to_vec();
                full_salt.extend_from_slice(&salt);
                let wrap = scrypt(pass.as_bytes(), &full_salt, log_n, 8, 1, 32);
                let mut buf = body.clone();
                if let Ok(fk) = aead_open(&wrap, &[0u8; 12], &mut buf) {
                    return fk
                        .try_into()
                        .map_err(|_| "age: wrapped file key has the wrong size".into());
                }
                return Err("age: the passphrase does not open this file".into());
            }
            _ => seen.push("unknown"),
        }
    }
    Err(format!(
        "age: no configured identity opens this file (stanzas: {})",
        if seen.is_empty() {
            "none".into()
        } else {
            seen.join(", ")
        }
    ))
}

/// Encrypt for X25519 recipients and/or a passphrase — the half tests and
/// operator tooling use; a deployment normally encrypts with the `age` CLI.
pub fn encrypt(
    plaintext: &[u8],
    recipients: &[[u8; 32]],
    passphrase: Option<(&str, u8)>,
) -> Result<Vec<u8>, String> {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut file_key = [0u8; FILE_KEY_LEN];
    rng.fill(&mut file_key).map_err(|_| "rng")?;

    let mut header = String::from("age-encryption.org/v1\n");
    for pk in recipients {
        let mut eph = [0u8; 32];
        rng.fill(&mut eph).map_err(|_| "rng")?;
        let eph_pub = x25519::public_key(&eph);
        let shared = x25519::agree(&eph, pk)?;
        let mut salt = Vec::with_capacity(64);
        salt.extend_from_slice(&eph_pub);
        salt.extend_from_slice(pk);
        let wrap = hkdf(&salt, &shared, b"age-encryption.org/v1/X25519", 32);
        let body = aead_seal(&wrap, &[0u8; 12], &file_key)?;
        header.push_str(&format!(
            "-> X25519 {}\n{}\n",
            b64_encode_nopad(&eph_pub),
            b64_encode_nopad(&body)
        ));
    }
    if let Some((pass, log_n)) = passphrase {
        let mut salt16 = [0u8; 16];
        rng.fill(&mut salt16).map_err(|_| "rng")?;
        let mut full_salt = b"age-encryption.org/v1/scrypt".to_vec();
        full_salt.extend_from_slice(&salt16);
        let wrap = scrypt(pass.as_bytes(), &full_salt, log_n, 8, 1, 32);
        let body = aead_seal(&wrap, &[0u8; 12], &file_key)?;
        header.push_str(&format!(
            "-> scrypt {} {log_n}\n{}\n",
            b64_encode_nopad(&salt16),
            b64_encode_nopad(&body)
        ));
    }
    header.push_str("---");
    let hmac_key = hkdf(&[], &file_key, b"header", 32);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &hmac_key);
    let mac = ring::hmac::sign(&key, header.as_bytes());
    header.push_str(&format!(" {}\n", b64_encode_nopad(mac.as_ref())));

    let mut out = header.into_bytes();
    let mut nonce16 = [0u8; 16];
    rng.fill(&mut nonce16).map_err(|_| "rng")?;
    out.extend_from_slice(&nonce16);
    let payload_key = hkdf(&nonce16, &file_key, b"payload", 32);
    let chunks: Vec<&[u8]> = if plaintext.is_empty() {
        vec![&[]]
    } else {
        plaintext.chunks(CHUNK).collect()
    };
    for (i, chunk) in chunks.iter().enumerate() {
        let mut n = [0u8; 12];
        n[3..11].copy_from_slice(&(i as u64).to_be_bytes());
        n[11] = u8::from(i == chunks.len() - 1);
        out.extend_from_slice(&aead_seal(&payload_key, &n, chunk)?);
    }
    Ok(out)
}

// ── armor ────────────────────────────────────────────────────────────────────

const ARMOR_END: &[u8] = b"-----END AGE ENCRYPTED FILE-----";

fn unarmor_if_needed(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let trimmed: &[u8] = {
        let mut b = bytes;
        while let Some((f, r)) = b.split_first() {
            if f.is_ascii_whitespace() {
                b = r;
            } else {
                break;
            }
        }
        b
    };
    if !trimmed.starts_with(AGE_ARMOR) {
        return Ok(bytes.to_vec());
    }
    let s = std::str::from_utf8(trimmed).map_err(|_| "age: armored file is not UTF-8")?;
    let mut b64 = String::new();
    for line in s.lines().skip(1) {
        if line.as_bytes() == ARMOR_END {
            return crate::config::envelope::b64url_decode(&b64)
                .ok_or_else(|| "age: bad armor base64".into());
        }
        b64.push_str(line.trim());
    }
    Err("age: armored file has no END line".into())
}

/// The byte offset where the header text ends and the binary payload begins:
/// one past the newline that terminates the MAC line.
fn header_end(bytes: &[u8]) -> Result<usize, String> {
    if !bytes.starts_with(AGE_MAGIC) {
        return Err("age: not an age file".into());
    }
    // Find the `\n--- ` line, then its terminating newline.
    let hay = bytes;
    let needle = b"\n--- ";
    let start = hay
        .windows(needle.len())
        .position(|w| w == needle)
        .ok_or("age: header has no MAC line")?;
    let after = &hay[start + 1..];
    let nl = after
        .iter()
        .position(|&b| b == b'\n')
        .ok_or("age: unterminated MAC line")?;
    Ok(start + 1 + nl + 1)
}

// ── key parsing (bech32) ─────────────────────────────────────────────────────

/// Parse an `AGE-SECRET-KEY-1…` identity into its 32-byte X25519 scalar.
pub fn parse_identity(s: &str) -> Result<[u8; 32], String> {
    let (hrp, data) = bech32_decode(&s.trim().to_ascii_lowercase())?;
    if hrp != "age-secret-key-" {
        return Err(format!("age: not an identity (hrp {hrp:?})"));
    }
    data.try_into()
        .map_err(|_| "age: identity is not 32 bytes".into())
}

/// Parse an `age1…` recipient into its 32-byte X25519 public key.
pub fn parse_recipient(s: &str) -> Result<[u8; 32], String> {
    let (hrp, data) = bech32_decode(&s.trim().to_ascii_lowercase())?;
    if hrp != "age" {
        return Err(format!("age: not a recipient (hrp {hrp:?})"));
    }
    data.try_into()
        .map_err(|_| "age: recipient is not 32 bytes".into())
}

/// Render a 32-byte public key as an `age1…` recipient.
pub fn encode_recipient(pk: &[u8; 32]) -> String {
    bech32_encode("age", pk)
}

/// Render a 32-byte scalar as an `AGE-SECRET-KEY-1…` identity (upper-case, as
/// `age-keygen` prints it).
pub fn encode_identity(sk: &[u8; 32]) -> String {
    bech32_encode("age-secret-key-", sk).to_ascii_uppercase()
}

const BECH32: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn bech32_polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut chk: u32 = 1;
    for &v in values {
        let b = chk >> 25;
        chk = (chk & 0x1ffffff) << 5 ^ v as u32;
        for (i, g) in GEN.iter().enumerate() {
            if (b >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &str) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.bytes().map(|b| b >> 5).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|b| b & 31));
    out
}

fn bech32_decode(s: &str) -> Result<(String, Vec<u8>), String> {
    let pos = s.rfind('1').ok_or("bech32: no separator")?;
    let (hrp, data_str) = (&s[..pos], &s[pos + 1..]);
    let mut values = Vec::with_capacity(data_str.len());
    for c in data_str.bytes() {
        let v = BECH32
            .iter()
            .position(|&b| b == c)
            .ok_or("bech32: invalid character")?;
        values.push(v as u8);
    }
    let mut check = hrp_expand(hrp);
    check.extend_from_slice(&values);
    if bech32_polymod(&check) != 1 {
        return Err("bech32: checksum mismatch".into());
    }
    let data = &values[..values.len().saturating_sub(6)];
    // 5-bit → 8-bit, no padding bits allowed to be non-zero.
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &v in data {
        acc = acc << 5 | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok((hrp.to_string(), out))
}

fn bech32_encode(hrp: &str, data: &[u8]) -> String {
    // 8-bit → 5-bit with padding.
    let mut five = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &b in data {
        acc = acc << 8 | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            five.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        five.push(((acc << (5 - bits)) & 31) as u8);
    }
    let mut check = hrp_expand(hrp);
    check.extend_from_slice(&five);
    check.extend_from_slice(&[0; 6]);
    let polymod = bech32_polymod(&check) ^ 1;
    let mut s = String::from(hrp);
    s.push('1');
    for v in &five {
        s.push(BECH32[*v as usize] as char);
    }
    for i in 0..6 {
        s.push(BECH32[((polymod >> (5 * (5 - i))) & 31) as usize] as char);
    }
    s
}

// ── crypto helpers (ring) ────────────────────────────────────────────────────

pub(super) fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    struct Len(usize);
    impl ring::hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let mut out = vec![0u8; len];
    ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt)
        .extract(ikm)
        .expand(&[info], Len(len))
        .and_then(|okm| okm.fill(&mut out))
        .expect("hkdf-sha256 within output bounds");
    out
}

pub(super) fn aead_open<'a>(
    key: &[u8],
    nonce: &[u8; 12],
    ct: &'a mut [u8],
) -> Result<&'a [u8], String> {
    let k = ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, key)
        .map_err(|_| "bad key length")?;
    ring::aead::LessSafeKey::new(k)
        .open_in_place(
            ring::aead::Nonce::assume_unique_for_key(*nonce),
            ring::aead::Aad::empty(),
            ct,
        )
        .map(|pt| &*pt)
        .map_err(|_| "AEAD open failed".to_string())
}

pub(super) fn aead_seal(key: &[u8], nonce: &[u8; 12], pt: &[u8]) -> Result<Vec<u8>, String> {
    let k = ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, key)
        .map_err(|_| "bad key length")?;
    let mut buf = pt.to_vec();
    ring::aead::LessSafeKey::new(k)
        .seal_in_place_append_tag(
            ring::aead::Nonce::assume_unique_for_key(*nonce),
            ring::aead::Aad::empty(),
            &mut buf,
        )
        .map_err(|_| "AEAD seal failed")?;
    Ok(buf)
}

// ── base64 (std alphabet, unpadded — the age header form) ────────────────────

fn b64_decode_nopad(s: &str) -> Option<Vec<u8>> {
    if s.contains('=') {
        return None; // age forbids padding in stanza bodies
    }
    crate::config::envelope::b64url_decode(s)
}

fn b64_encode_nopad(b: &[u8]) -> String {
    const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(STD[(n >> 18 & 63) as usize] as char);
        out.push(STD[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(STD[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(STD[(n & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x25519_round_trip_binary() {
        let sk = [5u8; 32];
        let pk = x25519::public_key(&sk);
        let doc = b"---\nspec: \"1\"\n---\n# Secret agent\n";
        let enc = encrypt(doc, &[pk], None).unwrap();
        assert!(enc.starts_with(AGE_MAGIC));
        let dec = decrypt(&enc, &[sk], None).unwrap();
        assert_eq!(dec, doc);
        // The wrong identity refuses, naming the stanza kinds.
        let e = decrypt(&enc, &[[9u8; 32]], None).unwrap_err();
        assert!(e.contains("no configured identity"), "{e}");
        // A flipped payload byte fails the AEAD.
        let mut bad = enc.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(decrypt(&bad, &[sk], None).is_err());
        // A flipped header byte fails the MAC.
        let mut bad = enc.clone();
        let i = bad.iter().position(|&b| b == b'>').unwrap();
        bad[i - 1] = b'!';
        assert!(decrypt(&bad, &[sk], None).is_err());
    }

    #[test]
    fn scrypt_passphrase_round_trip() {
        let doc = b"secret instructions";
        let enc = encrypt(doc, &[], Some(("correct horse", 10))).unwrap();
        assert_eq!(decrypt(&enc, &[], Some("correct horse")).unwrap(), doc);
        let e = decrypt(&enc, &[], Some("wrong")).unwrap_err();
        assert!(e.contains("passphrase"), "{e}");
        // No passphrase configured → the refusal names what the file needs.
        let e = decrypt(&enc, &[[1u8; 32]], None).unwrap_err();
        assert!(e.contains("scrypt"), "{e}");
    }

    #[test]
    fn multi_chunk_and_empty_payloads_round_trip() {
        let sk = [7u8; 32];
        let pk = x25519::public_key(&sk);
        // > one 64 KiB chunk.
        let big: Vec<u8> = (0..(CHUNK + 1234)).map(|i| (i % 251) as u8).collect();
        let enc = encrypt(&big, &[pk], None).unwrap();
        assert_eq!(decrypt(&enc, &[sk], None).unwrap(), big);
        // Empty plaintext.
        let enc = encrypt(b"", &[pk], None).unwrap();
        assert_eq!(decrypt(&enc, &[sk], None).unwrap(), b"");
    }

    #[test]
    fn bech32_keys_round_trip() {
        let sk = [3u8; 32];
        let pk = x25519::public_key(&sk);
        let ident = encode_identity(&sk);
        assert!(ident.starts_with("AGE-SECRET-KEY-1"));
        assert_eq!(parse_identity(&ident).unwrap(), sk);
        let rec = encode_recipient(&pk);
        assert!(rec.starts_with("age1"));
        assert_eq!(parse_recipient(&rec).unwrap(), pk);
        // A corrupted character fails the checksum.
        let mut bad = rec.clone();
        let last = bad.pop().unwrap();
        bad.push(if last == 'q' { 'p' } else { 'q' });
        assert!(parse_recipient(&bad).is_err());
    }

    /// Interop with the REAL `age` binary, when one is installed: age encrypts
    /// to our recipient → we decrypt; we encrypt → age decrypts. Skips (with a
    /// note) where age is absent — the round-trip tests above still hold.
    #[test]
    fn interop_with_the_age_binary_when_present() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        if Command::new("age").arg("--version").output().is_err() {
            eprintln!("age binary not installed; interop test skipped");
            return;
        }
        let sk = [11u8; 32];
        let pk = x25519::public_key(&sk);
        let doc = b"---\nspec: \"1\"\n---\n# Interop\n";

        // age → us.
        let mut child = Command::new("age")
            .args(["-r", &encode_recipient(&pk)])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(doc).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(decrypt(&out.stdout, &[sk], None).unwrap(), doc);

        // us → age.
        let enc = encrypt(doc, &[pk], None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ident_path = dir.path().join("id.txt");
        std::fs::write(&ident_path, format!("{}\n", encode_identity(&sk))).unwrap();
        let mut child = Command::new("age")
            .args(["-d", "-i", ident_path.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&enc).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "age could not decrypt our file: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, doc);
    }
}
