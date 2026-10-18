// SPDX-License-Identifier: AGPL-3.0-only
//! JSON Web Keys: the public JWK of a P-256 key (RFC 7518 §6.2) and the RFC
//! 7638 thumbprint, the key id every party computes the same way from the
//! key alone.

use serde_json::{Value, json};

use crate::b64;

/// The public JWK of a P-256 point in uncompressed form (`0x04 || x || y`,
/// 65 bytes): `{kty: EC, crv: P-256, x, y}`, public members only.
pub fn p256_public(point: &[u8]) -> Result<Value, String> {
    if point.len() != 65 || point[0] != 0x04 {
        return Err("jwk: not an uncompressed P-256 point".to_string());
    }
    Ok(json!({
        "kty": "EC",
        "crv": "P-256",
        "x": b64::url_nopad(&point[1..33]),
        "y": b64::url_nopad(&point[33..65]),
    }))
}

/// The RFC 7638 thumbprint of a public JWK, base64url: SHA-256 over the
/// JSON object of the key type's required members only (§3.2 — `crv`,
/// `kty`, `x`, `y` for EC; `e`, `kty`, `n` for RSA; `crv`, `kty`, `x` for
/// OKP, RFC 8037 §2), in lexicographic order with no whitespace.
pub fn thumbprint(jwk: &Value) -> Result<String, String> {
    let kty = jwk["kty"].as_str().ok_or("jwk: no kty")?;
    let members: &[&str] = match kty {
        "EC" => &["crv", "kty", "x", "y"],
        "RSA" => &["e", "kty", "n"],
        "OKP" => &["crv", "kty", "x"],
        other => return Err(format!("jwk: no thumbprint for kty {other}")),
    };
    // A BTreeMap would order them too; the lists are written in order, so
    // the object is built as it will be hashed.
    let mut canon = String::from("{");
    for (i, m) in members.iter().enumerate() {
        let v = jwk[*m]
            .as_str()
            .ok_or_else(|| format!("jwk: no {m} member"))?;
        if i > 0 {
            canon.push(',');
        }
        canon.push_str(&Value::String((*m).to_string()).to_string());
        canon.push(':');
        canon.push_str(&Value::String(v.to_string()).to_string());
    }
    canon.push('}');
    Ok(b64::url_nopad(&crate::sha::sha256(canon.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7638 §3.1's own example: the RSA key of RFC 7517 Appendix A.1.
    #[test]
    fn the_rfc_7638_example_thumbprint() {
        let jwk = json!({
            "kty": "RSA",
            "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw",
            "e": "AQAB",
            "alg": "RS256",
            "kid": "2011-04-29"
        });
        assert_eq!(
            thumbprint(&jwk).unwrap(),
            "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs"
        );
    }

    /// The fixed key of `client_assertion`'s tests; the thumbprint is SHA-256
    /// of `{"crv":"P-256","kty":"EC","x":…,"y":…}`, computed outside agentd.
    #[test]
    fn a_p256_jwk_and_its_thumbprint() {
        let x = crate::b64::url_decode("oju-BcuS__N4lPlvFxju4TebIwPP9pimlotvlUKhidg").unwrap();
        let y = crate::b64::url_decode("NpG7_i3sOo44bi051HAqB4NcIN6hPnoiM75uRcZbKB4").unwrap();
        let mut point = vec![0x04];
        point.extend_from_slice(&x);
        point.extend_from_slice(&y);
        let jwk = p256_public(&point).unwrap();
        assert_eq!(
            jwk,
            json!({"kty": "EC", "crv": "P-256",
                   "x": "oju-BcuS__N4lPlvFxju4TebIwPP9pimlotvlUKhidg",
                   "y": "NpG7_i3sOo44bi051HAqB4NcIN6hPnoiM75uRcZbKB4"})
        );
        assert_eq!(
            thumbprint(&jwk).unwrap(),
            "wKKE6xkA6L25tJfgBaINQ_ezX5GyDZRaei-vXq-doPc"
        );
        // Extra members do not change it; a missing one is an error.
        let mut with_kid = jwk.clone();
        with_kid["kid"] = json!("k1");
        assert_eq!(thumbprint(&with_kid), thumbprint(&jwk));
        assert!(thumbprint(&json!({"kty": "EC", "crv": "P-256", "x": "a"})).is_err());
        assert!(p256_public(&point[1..]).is_err());
    }
}
