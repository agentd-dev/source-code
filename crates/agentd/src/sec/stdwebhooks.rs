// SPDX-License-Identifier: AGPL-3.0-only
//! Standard Webhooks signatures — the profile MCP Events webhook delivery
//! uses (RFC 0045 §5.6.3; SEP-3415 "Webhook security"; the scheme itself:
//! https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md).
//!
//! A delivery carries `webhook-id`, `webhook-timestamp` (Unix seconds) and
//! `webhook-signature`: space-separated `v1,<base64 HMAC-SHA256>` values over
//! `id + "." + timestamp + "." + raw body`, keyed with the base64-decoded part
//! of a `whsec_` secret. Any one valid `v1,` value verifies (a sender rotating
//! its secret signs with both); other schemes (`v1a,`) are ignored.
//!
//! Hand-rolled over `crate::sha` — no new dependency.

use crate::sha::{ct_eq, hmac_sha256};

/// The prefix of a Standard Webhooks symmetric secret.
pub const SECRET_PREFIX: &str = "whsec_";
/// How far `webhook-timestamp` may be from now, either way [SEP: "SHOULD
/// reject deliveries whose `webhook-timestamp` is more than 5 minutes old"].
pub const TOLERANCE_S: u64 = 300;
/// The most signatures one header is read for: a sender rotating its secret
/// sends two, and nothing needs more.
const MAX_SIGNATURES: usize = 8;

/// A fresh secret: 32 bytes from the kernel CSPRNG, `whsec_<base64>`. The
/// draft wants 24–64 bytes, minted by the client [SEP: "Client SDKs SHOULD
/// generate it from a CSPRNG by default"].
pub fn mint_secret() -> std::io::Result<String> {
    let mut key = [0u8; 32];
    crate::sec::random::fill(&mut key)?;
    Ok(format!("{SECRET_PREFIX}{}", crate::b64::std_pad(&key)))
}

/// The key bytes of a `whsec_` secret: base64 that decodes to 24–64 bytes,
/// the only values a server accepts.
pub fn decode_secret(secret: &str) -> Result<Vec<u8>, String> {
    let b64 = secret
        .strip_prefix(SECRET_PREFIX)
        .ok_or("a Standard Webhooks secret starts with `whsec_`")?;
    let key = crate::b64::std_decode(b64)?;
    if !(24..=64).contains(&key.len()) {
        return Err(format!(
            "a Standard Webhooks secret decodes to 24–64 bytes, not {}",
            key.len()
        ));
    }
    Ok(key)
}

/// The `v1,` signature of one delivery.
pub fn sign(key: &[u8], id: &str, timestamp: u64, body: &[u8]) -> String {
    format!(
        "v1,{}",
        crate::b64::std_pad(&hmac_sha256(key, &signed_content(id, timestamp, body)))
    )
}

fn signed_content(id: &str, timestamp: u64, body: &[u8]) -> Vec<u8> {
    let mut content = format!("{id}.{timestamp}.").into_bytes();
    content.extend_from_slice(body);
    content
}

/// Why a delivery did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// A required header is absent: which.
    Missing(&'static str),
    /// `webhook-timestamp` is not a number of seconds.
    Timestamp,
    /// `webhook-timestamp` is more than [`TOLERANCE_S`] from now.
    Stale,
    /// No `v1,` signature matched under any key.
    Signature,
}

impl Reject {
    /// A stable word for logs and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Reject::Missing(_) => "missing_header",
            Reject::Timestamp => "bad_timestamp",
            Reject::Stale => "stale",
            Reject::Signature => "bad_signature",
        }
    }
}

/// The signature headers of one request.
pub struct Signed<'a> {
    pub id: Option<&'a str>,
    pub timestamp: Option<&'a str>,
    pub signature: Option<&'a str>,
}

/// Verify one delivery against each of `keys` in turn; returns the index of
/// the key that verified. The body is the raw bytes as received, never a
/// re-serialization.
pub fn verify(
    signed: &Signed<'_>,
    body: &[u8],
    keys: &[&[u8]],
    now_s: u64,
) -> Result<usize, Reject> {
    let id = signed.id.ok_or(Reject::Missing("webhook-id"))?;
    let ts = signed
        .timestamp
        .ok_or(Reject::Missing("webhook-timestamp"))?;
    let header = signed
        .signature
        .ok_or(Reject::Missing("webhook-signature"))?;
    let ts: u64 = ts.trim().parse().map_err(|_| Reject::Timestamp)?;
    if ts.abs_diff(now_s) > TOLERANCE_S {
        return Err(Reject::Stale);
    }
    let content = signed_content(id, ts, body);
    let offered: Vec<&str> = header
        .split(' ')
        .filter_map(|s| s.strip_prefix("v1,"))
        .take(MAX_SIGNATURES)
        .collect();
    for (i, key) in keys.iter().enumerate() {
        let want = crate::b64::std_pad(&hmac_sha256(key, &content));
        // Every offered value is compared, matched or not, so the time taken
        // does not say which one matched.
        let mut hit = false;
        for sig in &offered {
            hit |= ct_eq(sig.as_bytes(), want.as_bytes());
        }
        if hit {
            return Ok(i);
        }
    }
    Err(Reject::Signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generated with the reference `standardwebhooks` Python library
    /// (`Webhook.sign`); the first is the vector its own tests use.
    const VECTORS: &[(&str, &str, u64, &str, &str)] = &[
        (
            "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw",
            "msg_p5jXN8AQM9LWM0D4loKWxJek",
            1614265330,
            r#"{"test": 2432232314}"#,
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=",
        ),
        (
            "whsec_AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
            "evt_789",
            1739980800,
            r#"{"eventId":"evt_789","name":"incident.created","timestamp":"2026-02-19T16:00:00Z","data":{"incidentId":"INC-1234"},"cursor":"cursor_xyz"}"#,
            "v1,T71DgGqlYSf2VLMcUz+I5N8N6ACcVUG52FqeE7rfMic=",
        ),
        (
            "whsec_AwoRGB8mLTQ7QklQV15lbHN6gYiPlp2kq7K5wMfO1dzj6vH4/wYNFBsiKTA3PkVMU1phaG92fYSLkpmgp661vA==",
            "msg_verification_abc",
            1739980800,
            r#"{"type":"verification","challenge":"n0nce-1"}"#,
            "v1,G5TDBflUQZelhMBjiAnA/JzW5hl/yCbJ7xUXGCrPSoc=",
        ),
    ];

    fn signed<'a>(id: &'a str, ts: &'a str, sig: &'a str) -> Signed<'a> {
        Signed {
            id: Some(id),
            timestamp: Some(ts),
            signature: Some(sig),
        }
    }

    #[test]
    fn the_reference_vectors_sign_and_verify() {
        for (secret, id, ts, body, sig) in VECTORS {
            let key = decode_secret(secret).unwrap();
            assert_eq!(sign(&key, id, *ts, body.as_bytes()), *sig, "{id}");
            let t = ts.to_string();
            assert_eq!(
                verify(&signed(id, &t, sig), body.as_bytes(), &[&key], *ts),
                Ok(0)
            );
        }
    }

    #[test]
    fn a_rotation_header_verifies_under_either_key_and_names_which() {
        let (s1, id, ts, body, sig1) = VECTORS[1];
        let (s2, ..) = VECTORS[2];
        let (k1, k2) = (decode_secret(s1).unwrap(), decode_secret(s2).unwrap());
        let sig2 = sign(&k2, id, ts, body.as_bytes());
        let both = format!("{sig2} {sig1}");
        let t = ts.to_string();
        let s = signed(id, &t, &both);
        assert_eq!(verify(&s, body.as_bytes(), &[&k1], ts), Ok(0));
        assert_eq!(verify(&s, body.as_bytes(), &[&k1, &k2], ts), Ok(0));
        assert_eq!(verify(&s, body.as_bytes(), &[&k2], ts), Ok(0));
        let only_v1a = format!("v1a,{}", &sig1[3..]);
        assert_eq!(
            verify(&signed(id, &t, &only_v1a), body.as_bytes(), &[&k1], ts),
            Err(Reject::Signature)
        );
    }

    #[test]
    fn a_tampered_body_id_or_timestamp_fails() {
        let (secret, id, ts, body, sig) = VECTORS[1];
        let key = decode_secret(secret).unwrap();
        let t = ts.to_string();
        let tampered = body.replace("1234", "1235");
        assert_eq!(
            verify(&signed(id, &t, sig), tampered.as_bytes(), &[&key], ts),
            Err(Reject::Signature)
        );
        assert_eq!(
            verify(&signed("evt_790", &t, sig), body.as_bytes(), &[&key], ts),
            Err(Reject::Signature)
        );
        let t1 = (ts + 1).to_string();
        assert_eq!(
            verify(&signed(id, &t1, sig), body.as_bytes(), &[&key], ts),
            Err(Reject::Signature)
        );
    }

    #[test]
    fn the_timestamp_window_is_five_minutes_either_way() {
        let (secret, id, _, body, _) = VECTORS[1];
        let key = decode_secret(secret).unwrap();
        let now = 1_800_000_000;
        for (skew, ok) in [(-301i64, false), (-300, true), (299, true), (301, false)] {
            let ts = (now as i64 + skew) as u64;
            let sig = sign(&key, id, ts, body.as_bytes());
            let t = ts.to_string();
            let got = verify(&signed(id, &t, &sig), body.as_bytes(), &[&key], now);
            assert_eq!(got.is_ok(), ok, "skew {skew}: {got:?}");
            if !ok {
                assert_eq!(got, Err(Reject::Stale));
            }
        }
        assert_eq!(
            verify(&signed(id, "soon", "v1,x"), body.as_bytes(), &[&key], now),
            Err(Reject::Timestamp)
        );
    }

    #[test]
    fn every_header_is_required() {
        let key = [7u8; 32];
        for (s, which) in [
            (
                Signed {
                    id: None,
                    timestamp: Some("1"),
                    signature: Some("v1,x"),
                },
                "webhook-id",
            ),
            (
                Signed {
                    id: Some("a"),
                    timestamp: None,
                    signature: Some("v1,x"),
                },
                "webhook-timestamp",
            ),
            (
                Signed {
                    id: Some("a"),
                    timestamp: Some("1"),
                    signature: None,
                },
                "webhook-signature",
            ),
        ] {
            assert_eq!(verify(&s, b"{}", &[&key], 1), Err(Reject::Missing(which)));
        }
    }

    #[test]
    fn secrets_are_minted_and_checked_by_the_drafts_rule() {
        let s = mint_secret().unwrap();
        assert!(s.starts_with("whsec_"));
        assert_eq!(decode_secret(&s).unwrap().len(), 32);
        assert_ne!(s, mint_secret().unwrap());
        // 23 and 65 bytes are refused; a missing prefix and bad base64 too.
        let short = format!("whsec_{}", crate::b64::std_pad(&[1u8; 23]));
        let long = format!("whsec_{}", crate::b64::std_pad(&[1u8; 65]));
        assert!(decode_secret(&short).is_err());
        assert!(decode_secret(&long).is_err());
        assert!(decode_secret("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").is_err());
        assert!(decode_secret("whsec_not*base64").is_err());
    }
}
