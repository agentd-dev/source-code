// SPDX-License-Identifier: MIT OR Apache-2.0
//! **The §7.4 resolution manifest** (S7): the account of every input that
//! shaped one delivery, in one shape, with one signed byte form.
//!
//! The shape is strict. A field §7.4 requires has no default, so a manifest
//! that omits one — `variants.dropped`, `unresolved`, a `limits` count — does
//! not deserialise, and a delivery that embeds it is refused rather than read
//! as empty. A manifest from another producer that adds a diagnostic (`line`)
//! parses, and the diagnostic is dropped: it is not part of the signed form.
//!
//! The fields are declared in the order the corpus's `manifest.json` lists
//! them, so the pretty output of a delivery's manifest is that file's bytes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The §7.4 resolution manifest of one delivery.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub authored: Authored,
    /// Every parameter substituted in the delivered text, sorted by name.
    pub parameters: Vec<ParameterUse>,
    /// Every fact a `when` or `unless` compared, kept or dropped, sorted by
    /// key — consulted, not supplied.
    pub facts: Vec<Fact>,
    pub variants: Variants,
    /// Every transclusion, in pre-order as delivery met it; a target included
    /// twice appears twice.
    pub includes: Vec<Include>,
    pub limits: Limits,
    /// The `${name}` placeholders the delivered text leaves as written, and
    /// the delivered document's override targets found nowhere (S24), sorted.
    pub unresolved: Vec<String>,
    /// The `kind/name` rules an override silenced (S24), sorted; absent when
    /// none, so a manifest from before overrides is byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overridden: Vec<String>,
}

/// The authored document: its §7.2 author digest and, when the producer
/// knows it, its version. A registry knows the version; a file reader does
/// not, and writes none rather than an empty string.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Authored {
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One parameter substituted in the delivered text.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParameterUse {
    pub name: String,
    /// The document's declared source (`::param{source=…}`, front-matter
    /// `parameters[].source`), `static` when undeclared.
    pub source: String,
    /// The digest of the value used — never the value.
    pub value_digest: String,
}

/// One fact a variant condition compared.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Fact {
    pub key: String,
    pub value_digest: String,
}

/// The variants kept and dropped for this reader, by a per-kind counter in
/// document order (`when#1`, `unless#2`, `otherwise#1`). `dropped` is
/// REQUIRED (§7.4 rule 5): a reader must be able to tell content was
/// withheld, or `when` is indistinguishable from censorship by a compromised
/// resolver — so it has no default.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Variants {
    pub kept: Vec<String>,
    pub dropped: Vec<String>,
}

/// One transclusion: the `id` or `uri` attribute as written, and the digest
/// of the authored text inlined.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Include {
    pub target: String,
    pub digest: String,
}

/// How far the includes reached: the deepest level, and the UTF-8 bytes of
/// every inlined text, nested ones and repeats counted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Limits {
    pub include_depth: u64,
    pub include_bytes: u64,
}

impl Manifest {
    /// The form a delivery attestation embeds and a verifier recomputes
    /// (S7 §3): this manifest without `authored.version`, which a registry
    /// knows and a reader recomputing locally does not.
    pub fn signed_form(&self) -> Manifest {
        let mut m = self.clone();
        m.authored.version = None;
        m
    }

    /// The signed form's bytes: RFC 8785 (JCS) canonical JSON — members
    /// sorted, no whitespace, integers as integers, strings with only the
    /// escapes JSON requires, no trailing newline. The writer sorts the keys
    /// itself rather than trusting `serde_json::Map`'s order, which a
    /// downstream crate enabling `preserve_order` would turn into insertion
    /// order — and the signature with it.
    pub fn canonical(&self) -> String {
        let v = serde_json::to_value(self.signed_form()).expect("a manifest is JSON");
        let mut out = String::new();
        jcs(&v, &mut out);
        out
    }
}

/// One JSON value in RFC 8785 form. Members sort by their names' UTF-16 code
/// units (§3.2.3); every name here is ASCII, where that is byte order, but
/// the rule is the RFC's. Strings take `serde_json`'s escaping, which is
/// JCS's: `\"`, `\\`, the short forms for `\b \f \n \r \t`, `\u00xx` in lower
/// case for any other control character, everything else as UTF-8.
fn jcs(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut members: Vec<(&String, &Value)> = map.iter().collect();
            members.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, (k, v)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("a string is JSON"));
                out.push(':');
                jcs(v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                jcs(v, out);
            }
            out.push(']');
        }
        // Every number in a manifest is a u64 count, which serde_json writes
        // as an integer — no fraction, no exponent.
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            authored: Authored {
                digest: "sha256:aa".into(),
                version: Some("ver_1".into()),
            },
            parameters: vec![ParameterUse {
                name: "env".into(),
                source: "static".into(),
                value_digest: "sha256:bb".into(),
            }],
            facts: vec![Fact {
                key: "agent".into(),
                value_digest: "sha256:cc".into(),
            }],
            variants: Variants {
                kept: vec!["when#1".into()],
                dropped: vec!["unless#1".into()],
            },
            includes: vec![Include {
                target: "ins_house".into(),
                digest: "sha256:dd".into(),
            }],
            limits: Limits {
                include_depth: 1,
                include_bytes: 812,
            },
            unresolved: vec!["missing".into()],
            overridden: Vec::new(),
        }
    }

    /// Keys sorted at every level, no whitespace, integers bare, no
    /// `authored.version`, no empty `overridden`, no trailing newline.
    #[test]
    fn the_canonical_form_is_rfc_8785_of_the_signed_form() {
        assert_eq!(
            sample().canonical(),
            "{\"authored\":{\"digest\":\"sha256:aa\"},\
             \"facts\":[{\"key\":\"agent\",\"value_digest\":\"sha256:cc\"}],\
             \"includes\":[{\"digest\":\"sha256:dd\",\"target\":\"ins_house\"}],\
             \"limits\":{\"include_bytes\":812,\"include_depth\":1},\
             \"parameters\":[{\"name\":\"env\",\"source\":\"static\",\"value_digest\":\"sha256:bb\"}],\
             \"unresolved\":[\"missing\"],\
             \"variants\":{\"dropped\":[\"unless#1\"],\"kept\":[\"when#1\"]}}"
        );
        // The signed form drops the version and nothing else.
        let signed = sample().signed_form();
        assert_eq!(signed.authored.version, None);
        assert_eq!(
            Manifest {
                authored: Authored {
                    version: Some("ver_1".into()),
                    ..signed.authored.clone()
                },
                ..signed
            },
            sample()
        );
        // An overridden list, once there, is signed — sorted among the keys.
        let m = Manifest {
            overridden: vec!["should/brevity".into()],
            ..sample()
        };
        assert!(
            m.canonical()
                .contains("\"limits\":{\"include_bytes\":812,\"include_depth\":1},\"overridden\":[\"should/brevity\"],\"parameters\""),
            "{}",
            m.canonical()
        );
    }

    /// Strings carry only the escapes JSON requires: a quote, a backslash,
    /// the short control escapes, `\u00xx` in lower case for the rest; a
    /// non-ASCII character and `/` are written as themselves.
    #[test]
    fn the_canonical_form_escapes_only_what_json_requires() {
        let m = Manifest {
            unresolved: vec!["q\"b\\n\n\t\u{1}\u{1f}é/€\u{7f}".into()],
            ..Manifest::default()
        };
        assert!(
            m.canonical()
                .contains(r#""unresolved":["q\"b\\n\n\t\u0001\u001fé/€"#),
            "{}",
            m.canonical()
        );
        assert!(m.canonical().contains("\u{7f}\"]"), "DEL is not escaped");
    }

    /// Members sort by their names' UTF-16 code units at every depth — the
    /// writer's order, not the map's: U+10000 is a surrogate pair in UTF-16,
    /// so it sorts before U+FF61, where its UTF-8 bytes (and a `BTreeMap`)
    /// put it after (RFC 8785 §3.2.3).
    #[test]
    fn the_canonical_writer_sorts_members_itself() {
        let mut map = serde_json::Map::new();
        map.insert("b".into(), Value::from(1u64));
        map.insert(
            "a".into(),
            serde_json::json!({"\u{ff61}": [], "\u{10000}": u64::MAX}),
        );
        let mut out = String::new();
        jcs(&Value::Object(map), &mut out);
        assert_eq!(
            out,
            "{\"a\":{\"\u{10000}\":18446744073709551615,\"\u{ff61}\":[]},\"b\":1}"
        );
    }

    fn parses(json: &str) -> Result<Manifest, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    const FULL: &str = r#"{"authored":{"digest":"sha256:aa"},"parameters":[],"facts":[],
        "variants":{"kept":[],"dropped":[]},"includes":[],
        "limits":{"include_depth":0,"include_bytes":0},"unresolved":[]}"#;

    /// Every field §7.4 requires is required: a manifest missing one does not
    /// deserialise, nor does `limits: {}` — the shape a pre-S7 registry sent.
    #[test]
    fn a_manifest_missing_a_required_field_is_refused() {
        assert!(parses(FULL).is_ok());
        for (from, to) in [
            (r#""kept":[],"dropped":[]"#, r#""kept":[]"#),
            (r#","unresolved":[]"#, ""),
            (
                r#""limits":{"include_depth":0,"include_bytes":0}"#,
                r#""limits":{}"#,
            ),
            (
                r#""limits":{"include_depth":0,"include_bytes":0}"#,
                r#""limits":{"include_depth":0}"#,
            ),
            (r#""includes":[],"#, ""),
            (r#""facts":[],"#, ""),
            (r#""parameters":[],"#, ""),
        ] {
            let bad = FULL.replace(from, to);
            assert_ne!(bad, FULL, "{from} is in the sample");
            let e = parses(&bad).unwrap_err();
            assert!(e.contains("missing field"), "{to}: {e}");
        }
    }

    /// A producer's diagnostic — `line` — parses and is dropped: it is not
    /// part of the shape, so it is never signed.
    #[test]
    fn an_unknown_diagnostic_parses_and_is_dropped() {
        let with_line = FULL
            .replace(r#""kept":[]"#, r#""kept":[],"line":11"#)
            .replace(r#""unresolved":[]"#, r#""unresolved":[],"line":3"#);
        let m = parses(&with_line).unwrap();
        assert_eq!(m, parses(FULL).unwrap());
        assert!(!m.canonical().contains("line"));
    }
}
