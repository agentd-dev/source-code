// SPDX-License-Identifier: AGPL-3.0-only
//! The draft's JSON Schemas as the fake Company holds itself to them: the
//! 19 schemas of the Company kit, vendored verbatim at the pin
//! (`tests/companies/vendor/schemas/`, see its `PIN`), and a hand-written
//! check for each regex `pattern` in them.
//!
//! [`crate::jsonschema`] has no regex engine and resolves `$ref` only within
//! one document, while these schemas use both: an HTTPS-URL prefix, the ID
//! alphabet, the 43-character `ath`, RFC 3339 instants, and references from
//! one file into another (`event.schema.json` into `message.schema.json`,
//! `mediated-sign-in-response.schema.json` into `token-response.schema.json`).
//! [`Kit`] is the [`Resolver`] that closes both gaps, so a document is held
//! to every rule its schema states. The forcing tests in
//! `tests/companies.rs` keep it that way across a re-pin: each pattern the
//! vendored schemas carry must have its check in [`PATTERNS`], and each
//! vendored file must be in [`SCHEMAS`].

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::Value;

use crate::jsonschema::{self, Resolver};

/// Every vendored schema, by its file name (which is also its `$id`, and
/// what another schema's `$ref` names).
pub const SCHEMAS: &[(&str, &str)] = &[
    (
        "authorization-server-metadata.schema.json",
        include_str!(
            "../../../tests/companies/vendor/schemas/authorization-server-metadata.schema.json"
        ),
    ),
    (
        "browser-assertion.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/browser-assertion.schema.json"),
    ),
    (
        "client-metadata.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/client-metadata.schema.json"),
    ),
    (
        "conversation-request.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/conversation-request.schema.json"),
    ),
    (
        "conversation-response.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/conversation-response.schema.json"),
    ),
    (
        "device-authorization-response.schema.json",
        include_str!(
            "../../../tests/companies/vendor/schemas/device-authorization-response.schema.json"
        ),
    ),
    (
        "dpop-proof.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/dpop-proof.schema.json"),
    ),
    (
        "error.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/error.schema.json"),
    ),
    (
        "event.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/event.schema.json"),
    ),
    (
        "events-response.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/events-response.schema.json"),
    ),
    (
        "mediated-code-request.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/mediated-code-request.schema.json"),
    ),
    (
        "mediated-sign-in-request.schema.json",
        include_str!(
            "../../../tests/companies/vendor/schemas/mediated-sign-in-request.schema.json"
        ),
    ),
    (
        "mediated-sign-in-response.schema.json",
        include_str!(
            "../../../tests/companies/vendor/schemas/mediated-sign-in-response.schema.json"
        ),
    ),
    (
        "message.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/message.schema.json"),
    ),
    (
        "oauth-error.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/oauth-error.schema.json"),
    ),
    (
        "poppy.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/poppy.schema.json"),
    ),
    (
        "protected-resource-metadata.schema.json",
        include_str!(
            "../../../tests/companies/vendor/schemas/protected-resource-metadata.schema.json"
        ),
    ),
    (
        "session-assertion.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/session-assertion.schema.json"),
    ),
    (
        "token-response.schema.json",
        include_str!("../../../tests/companies/vendor/schemas/token-response.schema.json"),
    ),
];

/// The hand-written check for one regex `pattern`, and the strings that
/// show it is the pattern's: it must accept every one of `accepts` and
/// none of `rejects`.
pub struct PatternCheck {
    /// The pattern exactly as the schemas write it.
    pub pattern: &'static str,
    /// What it checks, for a failure message.
    pub name: &'static str,
    pub check: fn(&str) -> bool,
    pub accepts: &'static [&'static str],
    pub rejects: &'static [&'static str],
}

/// A check for every regex pattern in [`SCHEMAS`]. ECMA-262 regex, which
/// the schemas are written in: `\s` is Unicode white space and U+FEFF, and
/// `$` without the `m` flag is the end of the input.
pub const PATTERNS: &[PatternCheck] = &[
    PatternCheck {
        pattern: "^https://[^\\s/?#]+",
        name: "an HTTPS URL with a host",
        check: https_url,
        accepts: &[
            "https://a",
            "https://auth.example.com/oauth/token?x#y",
            "https://127.0.0.1:8443",
        ],
        rejects: &[
            "http://a",
            "https://",
            "https:///path",
            "https:// a",
            "https://?q",
            "https://#f",
            " https://a",
            "HTTPS://a",
        ],
    },
    PatternCheck {
        pattern: "^https://[^?#]+$",
        name: "an HTTPS URL without a query or fragment",
        check: https_url_no_query,
        accepts: &[
            "https://api.example.com/orders/ord_7Hk2/exchanges",
            "https://a",
        ],
        rejects: &["https://a/b?c", "https://a#f", "https://", "http://a/b"],
    },
    PatternCheck {
        pattern: "^[A-Z]+$",
        name: "an uppercase HTTP method",
        check: upper_method,
        accepts: &["POST", "GET"],
        rejects: &["post", "Get", "", "PO ST", "GET\n"],
    },
    PatternCheck {
        pattern: "^[A-Za-z0-9_-]{43}$",
        name: "a base64url SHA-256 digest (43 characters)",
        check: sha256_b64url,
        accepts: &["fUHyO2r2Z3DZ53EsNrWBb0xWXoaNy59IiKCAqksmQEo"],
        rejects: &[
            "fUHyO2r2Z3DZ53EsNrWBb0xWXoaNy59IiKCAqksmQE",
            "fUHyO2r2Z3DZ53EsNrWBb0xWXoaNy59IiKCAqksmQEo=",
            "fUHyO2r2Z3DZ53EsNrWBb0xWXoaNy59IiKCAqksmQE+",
        ],
    },
    PatternCheck {
        pattern: "^[A-Za-z0-9_-]{1,256}$",
        name: "an ID of 1 to 256 base64url characters",
        check: id_alphabet,
        accepts: &["msg_7Hk2", "a", "conv-1_A"],
        rejects: &["", "a/b", "a b", "ä", "id.1"],
    },
    PatternCheck {
        pattern: "^[0-9]{4}-[0-9]{2}-[0-9]{2}[Tt][0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]+)?([Zz]|[+-][0-9]{2}:[0-9]{2})$",
        name: "an RFC 3339 date-time",
        check: rfc3339,
        accepts: &[
            "2026-10-16T17:08:46Z",
            "2026-10-16t17:08:46.123+02:00",
            "2026-10-16T17:08:46.5z",
        ],
        rejects: &[
            "2026-10-16 17:08:46Z",
            "2026-10-16T17:08:46",
            "2026-10-16T17:08Z",
            "2026-10-16T17:08:46.Z",
            "2026-10-16T17:08:46+0200",
            "26-10-16T17:08:46Z",
        ],
    },
    PatternCheck {
        pattern: "^[\\x20\\x21\\x23-\\x5B\\x5D-\\x7E]+$",
        name: "an OAuth error code (RFC 6749 NQSCHAR)",
        check: oauth_error_code,
        accepts: &["invalid_grant", "a b", "!#[]~"],
        rejects: &["", "a\"b", "a\\b", "é", "a\tb"],
    },
    PatternCheck {
        pattern: "^[0-9]+\\.[0-9]+$",
        name: "a major.minor version",
        check: major_minor,
        accepts: &["0.1", "10.23"],
        rejects: &["0", "0.", ".1", "0.1.2", "v0.1", "0.x"],
    },
    PatternCheck {
        pattern: "^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?(\\.[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?)+$",
        name: "a dotted host name",
        check: dotted_host,
        accepts: &["example.com", "www.example.co.uk", "127.0.0.1", "a-b.c"],
        rejects: &[
            "localhost",
            "example.com.",
            ".example.com",
            "-a.com",
            "a-.com",
            "a..com",
            "a_b.com",
            "exa mple.com",
        ],
    },
    PatternCheck {
        pattern: "^[\\x21\\x23-\\x5B\\x5D-\\x7E]+$",
        name: "an OAuth scope token (RFC 6749 NQCHAR)",
        check: scope_token,
        accepts: &["poppy:read", "addresses", "!#~"],
        rejects: &["", "a b", "a\"b", "a\\b", "é"],
    },
];

/// The check for `pattern`, if [`PATTERNS`] has one.
pub fn check_for(pattern: &str) -> Option<&'static PatternCheck> {
    PATTERNS.iter().find(|p| p.pattern == pattern)
}

/// ECMA-262 `\s`.
fn ecma_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn https_url(s: &str) -> bool {
    s.strip_prefix("https://")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| !ecma_space(c) && !matches!(c, '/' | '?' | '#'))
}

fn https_url_no_query(s: &str) -> bool {
    s.strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.contains(['?', '#']))
}

fn upper_method(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_uppercase())
}

fn b64url_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn sha256_b64url(s: &str) -> bool {
    s.len() == 43 && s.bytes().all(b64url_char)
}

fn id_alphabet(s: &str) -> bool {
    (1..=256).contains(&s.len()) && s.bytes().all(b64url_char)
}

fn rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    let at = |i: usize, c: &[u8]| b.get(i).is_some_and(|x| c.contains(x));
    if !(digits(0..4)
        && at(4, b"-")
        && digits(5..7)
        && at(7, b"-")
        && digits(8..10)
        && at(10, b"Tt")
        && digits(11..13)
        && at(13, b":")
        && digits(14..16)
        && at(16, b":")
        && digits(17..19))
    {
        return false;
    }
    let mut i = 19;
    if at(i, b".") {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match b.get(i) {
        Some(b'Z' | b'z') => i + 1 == b.len(),
        Some(b'+' | b'-') => {
            digits(i + 1..i + 3) && at(i + 3, b":") && digits(i + 4..i + 6) && i + 6 == b.len()
        }
        _ => false,
    }
}

fn oauth_error_code(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, '\x20' | '\x21' | '\x23'..='\x5B' | '\x5D'..='\x7E'))
}

fn major_minor(s: &str) -> bool {
    s.split_once('.').is_some_and(|(a, b)| {
        let num = |x: &str| !x.is_empty() && x.bytes().all(|d| d.is_ascii_digit());
        num(a) && num(b)
    })
}

fn dotted_host(s: &str) -> bool {
    let label = |l: &str| {
        let b = l.as_bytes();
        !b.is_empty()
            && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
    };
    let labels: Vec<&str> = s.split('.').collect();
    labels.len() >= 2 && labels.into_iter().all(label)
}

fn scope_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, '\x21' | '\x23'..='\x5B' | '\x5D'..='\x7E'))
}

/// The vendored schemas, parsed, as the [`Resolver`] that finds a document
/// another one references and checks every pattern by [`PATTERNS`].
pub struct Kit {
    documents: BTreeMap<&'static str, Value>,
}

/// The kit, parsed once.
pub fn kit() -> &'static Kit {
    static KIT: OnceLock<Kit> = OnceLock::new();
    KIT.get_or_init(|| Kit {
        // A vendored schema that is not JSON is left out, and any document
        // naming it fails as an unknown schema; the forcing tests parse every
        // one of them first.
        documents: SCHEMAS
            .iter()
            .filter_map(|(name, text)| Some((*name, serde_json::from_str(text).ok()?)))
            .collect(),
    })
}

impl Kit {
    /// The schema the file `name` holds.
    pub fn schema(&self, name: &str) -> Option<&Value> {
        self.documents.get(name)
    }

    /// Validate `value` against the schema `name` (a file name, optionally
    /// with a `#/…` pointer into it, as a `$ref` would name it), every
    /// pattern and every reference into another file included.
    pub fn validate(&self, name: &str, value: &Value) -> Result<(), Vec<String>> {
        let reference = serde_json::json!({ "$ref": name });
        jsonschema::validate_with(&reference, value, self)
    }
}

impl Resolver for Kit {
    fn document(&self, uri: &str) -> Option<&Value> {
        self.documents.get(uri)
    }

    fn pattern(&self, pattern: &str, s: &str) -> Option<bool> {
        check_for(pattern).map(|p| (p.check)(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Each check takes the strings its pattern takes and refuses the ones
    /// it refuses.
    #[test]
    fn every_check_agrees_with_its_examples() {
        for p in PATTERNS {
            for s in p.accepts {
                assert!((p.check)(s), "{} ({}) refuses {s:?}", p.name, p.pattern);
            }
            for s in p.rejects {
                assert!(!(p.check)(s), "{} ({}) accepts {s:?}", p.name, p.pattern);
            }
        }
    }

    /// A document is held to its own schema's patterns and to every schema it
    /// references in another file: an event's message is checked by the
    /// message schema, whose `#/$defs/id` resolves in the message schema.
    #[test]
    fn a_document_is_checked_through_every_file_it_references() {
        let k = kit();
        let event = json!({
            "id": "evt_1", "type": "message", "created_at": "2026-10-16T17:08:46Z",
            "message": {"id": "msg_1", "sender": "agent", "role": "company", "text": "hi"}
        });
        assert!(k.validate("event.schema.json", &event).is_ok());
        let mut bad = event.clone();
        bad["message"]["id"] = json!("msg/1");
        let e = k.validate("event.schema.json", &bad).unwrap_err();
        assert!(
            e.iter()
                .any(|m| m.starts_with("/message/id: does not match")),
            "{e:?}"
        );
        let mut bad = event.clone();
        bad["created_at"] = json!("yesterday");
        assert!(k.validate("event.schema.json", &bad).is_err());
        let complete = json!({"status": "complete", "access_token": "a", "token_type": "DPoP",
            "expires_in": 60, "scope": "", "session_id": "s", "signed_in": true, "refresh_token": "r"});
        assert!(
            k.validate("mediated-sign-in-response.schema.json", &complete)
                .is_ok()
        );
        let mut bad = complete.clone();
        bad.as_object_mut().unwrap().remove("session_id");
        let e = k
            .validate("mediated-sign-in-response.schema.json", &bad)
            .unwrap_err();
        assert!(e.iter().any(|m| m.contains("\"session_id\"")), "{e:?}");
        assert!(
            k.validate(
                "dpop-proof.schema.json#/$defs/payload",
                &json!({"jti": "j", "htm": "post", "htu": "https://a", "iat": 1})
            )
            .is_err()
        );
        assert!(k.validate("nope.schema.json", &json!({})).is_err());
    }
}
