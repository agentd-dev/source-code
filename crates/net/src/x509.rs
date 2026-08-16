// SPDX-License-Identifier: AGPL-3.0-only
//! A **minimal, defensive X.509 field extractor** (RFC 0031 §7 / RFC 0029 §10.3).
//! Under mutual TLS the peer's *verified* leaf certificate carries the client
//! identity — its subject CN and its Subject Alternative Names (DNS, URI, IP).
//! The URI SAN is where a SPIFFE X.509-SVID puts `spiffe://…`. The serve
//! framework surfaces these so `a2a.principals` can match a caller by `san`/`sub`
//! (RFC 0029 §2), not just "a cert was presented".
//!
//! rustls verifies the chain; this only *reads* fields from an already-trusted
//! cert. It is a hand-rolled DER walk (the 3-dependency moat — no `x509-parser`):
//! every read is length-checked, DER long-form lengths are bounded to 4 bytes,
//! nothing recurses without shrinking the slice, and any malformed input yields
//! an empty/partial result rather than a panic. It parses ONLY the two fields we
//! need; it is NOT a general X.509 library.

/// The identity fields lifted from a verified peer certificate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerIdentity {
    /// The subject's Common Name (`2.5.4.3`), if present.
    pub subject_cn: Option<String>,
    /// The Subject Alternative Names: dNSName / URI / rfc822 as-is, iPAddress
    /// formatted. A SPIFFE ID rides here as a `spiffe://…` URI SAN.
    pub sans: Vec<String>,
}

/// A hostile-input ceiling on the SAN count (a verified cert never has this many).
const MAX_SANS: usize = 64;

/// The OID `2.5.4.3` (commonName), DER value bytes.
const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
/// The OID `2.5.29.17` (subjectAltName), DER value bytes.
const OID_SAN: &[u8] = &[0x55, 0x1D, 0x11];

/// Parse a DER-encoded certificate into its [`PeerIdentity`]. Best-effort: a
/// truncated / malformed cert yields whatever was recovered (often `default()`).
pub fn parse(cert_der: &[u8]) -> PeerIdentity {
    let mut out = PeerIdentity::default();
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature };
    // the first element of that SEQUENCE is the tbsCertificate SEQUENCE — unwrap
    // both to reach its fields.
    let Some(tbs) = Der::new(cert_der)
        .seq()
        .and_then(|body| Der::new(body).seq())
    else {
        return out;
    };
    // Collect TBSCertificate's top-level fields, then index them positionally.
    // TBSCertificate ::= SEQUENCE { [0] version DEFAULT, serialNumber, signature,
    //   issuer, validity, subject, subjectPublicKeyInfo, … [3] extensions }.
    let mut fields: Vec<(u8, &[u8])> = Vec::new();
    let mut r = Der::new(tbs);
    while let Some(f) = r.tlv() {
        fields.push(f);
        if fields.len() > 24 {
            break; // a real TBSCertificate has ~10 fields
        }
    }
    // `version` [0] (tag 0xA0) is optional; everything after shifts by its presence.
    let base = usize::from(fields.first().is_some_and(|(t, _)| *t == 0xA0));
    // subject Name is the 5th field after the (optional) version.
    if let Some((0x30, subject)) = fields.get(base + 4).copied() {
        out.subject_cn = subject_cn(subject);
    }
    // extensions is the `[3]` (tag 0xA3) field, after subjectPublicKeyInfo.
    if let Some((_, ext3)) = fields.iter().skip(base + 5).find(|(t, _)| *t == 0xA3) {
        out.sans = sans(ext3);
    }
    out
}

/// The subject's CN: `Name ::= SEQUENCE OF RDN (SET) OF ATV (SEQ { OID, value })`.
fn subject_cn(name_der: &[u8]) -> Option<String> {
    let mut rdns = Der::new(name_der);
    while let Some((t, set)) = rdns.tlv() {
        if t != 0x31 {
            continue; // RelativeDistinguishedName is a SET
        }
        let mut atvs = Der::new(set);
        while let Some((t2, atv)) = atvs.tlv() {
            if t2 != 0x30 {
                continue;
            }
            let mut ir = Der::new(atv);
            let Some((0x06, oid)) = ir.tlv() else {
                continue;
            };
            if oid == OID_CN
                && let Some((_, val)) = ir.tlv()
            {
                return Some(String::from_utf8_lossy(val).into_owned());
            }
        }
    }
    None
}

/// The SANs from the `[3]` extensions field: find the SAN extension's OCTET
/// STRING, which wraps `SEQUENCE OF GeneralName`; lift the string/IP forms.
fn sans(ext3_der: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    // [3] EXPLICIT wraps one SEQUENCE OF Extension.
    let Some(exts_seq) = Der::new(ext3_der).seq() else {
        return out;
    };
    let mut exts = Der::new(exts_seq);
    while let Some((t, ext)) = exts.tlv() {
        if t != 0x30 {
            continue; // Extension ::= SEQUENCE { OID, critical?, value }
        }
        let mut er = Der::new(ext);
        let Some((0x06, oid)) = er.tlv() else {
            continue;
        };
        if oid != OID_SAN {
            continue;
        }
        // The extnValue OCTET STRING (skips an optional `critical` BOOLEAN).
        let mut value = None;
        while let Some((vt, v)) = er.tlv() {
            if vt == 0x04 {
                value = Some(v);
            }
        }
        let Some(octets) = value else { continue };
        // extnValue wraps SEQUENCE OF GeneralName.
        let Some(gnames) = Der::new(octets).seq() else {
            continue;
        };
        let mut gr = Der::new(gnames);
        while let Some((gt, gv)) = gr.tlv() {
            let entry = match gt {
                // rfc822Name [1], dNSName [2], uniformResourceIdentifier [6] — IA5.
                0x81 | 0x82 | 0x86 => Some(String::from_utf8_lossy(gv).into_owned()),
                0x87 => fmt_ip(gv), // iPAddress [7]
                _ => None,
            };
            if let Some(e) = entry {
                out.push(e);
                if out.len() >= MAX_SANS {
                    return out;
                }
            }
        }
    }
    out
}

/// Format a 4- or 16-byte iPAddress SAN; other lengths → `None`.
fn fmt_ip(bytes: &[u8]) -> Option<String> {
    match bytes.len() {
        4 => Some(std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()),
        16 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(bytes);
            Some(std::net::Ipv6Addr::from(o).to_string())
        }
        _ => None,
    }
}

/// A minimal DER TLV reader over a byte slice — definite lengths only.
struct Der<'a> {
    b: &'a [u8],
}

impl<'a> Der<'a> {
    fn new(b: &'a [u8]) -> Der<'a> {
        Der { b }
    }

    /// Read one TLV, returning `(tag, value)` and advancing past it. `None` on a
    /// truncated header/value or a disallowed (indefinite / >4-byte) length.
    fn tlv(&mut self) -> Option<(u8, &'a [u8])> {
        let (&tag, rest) = self.b.split_first()?;
        let (&l0, rest) = rest.split_first()?;
        let (len, rest) = if l0 & 0x80 == 0 {
            (l0 as usize, rest)
        } else {
            let n = (l0 & 0x7f) as usize;
            if n == 0 || n > 4 || rest.len() < n {
                return None; // indefinite form / oversized length — not valid DER here
            }
            let mut len = 0usize;
            for &byte in &rest[..n] {
                len = (len << 8) | byte as usize;
            }
            (len, &rest[n..])
        };
        if rest.len() < len {
            return None;
        }
        let (val, after) = rest.split_at(len);
        self.b = after;
        Some((tag, val))
    }

    /// Read one SEQUENCE (tag `0x30`) and return its contents.
    fn seq(&mut self) -> Option<&'a [u8]> {
        match self.tlv() {
            Some((0x30, v)) => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a PEM cert fixture to DER (test-only; the runtime gets DER from
    /// rustls directly). A tiny base64 decoder keeps the test dependency-free.
    fn pem_to_der(pem: &str) -> Vec<u8> {
        let b64: String = pem
            .lines()
            .skip_while(|l| !l.contains("BEGIN CERTIFICATE"))
            .skip(1)
            .take_while(|l| !l.contains("END CERTIFICATE"))
            .collect();
        b64_decode(&b64)
    }

    fn b64_decode(s: &str) -> Vec<u8> {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let val = |c: u8| T.iter().position(|&t| t == c);
        let mut out = Vec::new();
        let mut buf = 0u32;
        let mut bits = 0u32;
        for &c in s.as_bytes() {
            if c == b'=' || c.is_ascii_whitespace() {
                continue;
            }
            let Some(v) = val(c) else { continue };
            buf = (buf << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
            }
        }
        out
    }

    #[test]
    fn extracts_subject_cn_from_a_real_cert() {
        // The mTLS client fixture: subject CN = "agentctl-test", no SAN.
        let der = pem_to_der(include_str!("../tests/fixtures/client.pem"));
        let id = parse(&der);
        assert_eq!(id.subject_cn.as_deref(), Some("agentctl-test"));
        assert!(
            id.sans.is_empty(),
            "client fixture has no SAN: {:?}",
            id.sans
        );
    }

    #[test]
    fn extracts_dns_and_ip_sans_from_a_real_cert() {
        // The server fixture: CN = localhost, SAN = DNS:localhost + IP 127.0.0.1 + ::1.
        let der = pem_to_der(include_str!("../tests/fixtures/server.pem"));
        let id = parse(&der);
        assert_eq!(id.subject_cn.as_deref(), Some("localhost"));
        assert!(id.sans.contains(&"localhost".to_string()), "{:?}", id.sans);
        assert!(id.sans.contains(&"127.0.0.1".to_string()), "{:?}", id.sans);
        assert!(id.sans.iter().any(|s| s == "::1"), "{:?}", id.sans);
    }

    #[test]
    fn malformed_input_never_panics() {
        assert_eq!(parse(&[]), PeerIdentity::default());
        assert_eq!(parse(&[0x30]), PeerIdentity::default()); // truncated SEQUENCE
        assert_eq!(parse(&[0x30, 0x80]), PeerIdentity::default()); // indefinite length
        // A long-form length that overruns the buffer must not read past it.
        assert_eq!(
            parse(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]),
            PeerIdentity::default()
        );
    }

    #[test]
    fn parses_a_synthetic_spiffe_uri_san() {
        // A hand-built SAN extension value: SEQUENCE { [6] URI "spiffe://td/wl" }.
        let uri = b"spiffe://example.org/workload/api";
        let mut gname = vec![0x86, uri.len() as u8];
        gname.extend_from_slice(uri);
        let mut gseq = vec![0x30, gname.len() as u8];
        gseq.extend_from_slice(&gname);
        let mut octet = vec![0x04, gseq.len() as u8];
        octet.extend_from_slice(&gseq);
        // Extension ::= SEQ { OID 2.5.29.17, extnValue OCTET STRING }
        let mut ext = vec![0x06, 0x03, 0x55, 0x1D, 0x11];
        ext.extend_from_slice(&octet);
        let mut extseq = vec![0x30, ext.len() as u8];
        extseq.extend_from_slice(&ext);
        let mut exts_seq = vec![0x30, extseq.len() as u8];
        exts_seq.extend_from_slice(&extseq);
        // `sans` receives the CONTENTS of the `[3]` field (what `parse` stores):
        // the SEQUENCE OF Extension, not the 0xA3 wrapper.
        assert_eq!(
            sans(&exts_seq),
            vec!["spiffe://example.org/workload/api".to_string()]
        );
    }
}
