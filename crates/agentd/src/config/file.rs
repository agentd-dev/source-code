// SPDX-License-Identifier: AGPL-3.0-only
//! The declarative config **file** + its JSON Schema.
//!
//! One document, two syntaxes: **YAML** (`.yaml`/`.yml`, read by the
//! hand-rolled [`super::yaml`] subset reader — no `serde_yaml`, the minimalism
//! moat) or **JSON** with comments (`.json`/`.jsonc`); an unknown extension is
//! sniffed (`{`/`[` ⇒ JSON, else YAML). Both parse to the same
//! `serde_json::Value` document ([`read_document`]), which this module also
//! merges across the config chain ([`read_documents_checked`], RFC 7396) before
//! handing it to the typed [`super::settings::Settings`] — so validation, the schema,
//! the env/flag path bindings ([`super::paths`]) and hot reload are all
//! format-agnostic.
//!
//! The file carries the whole declarative agent — its instruction, its
//! workflows, its MCP servers, A2A peers, store, context, lifecycle, limits and
//! security policy. It **never** carries a literal secret: from a file a
//! credential MUST be a `{{secret:NAME}}` / `{{secret-file:PATH}}` reference, so
//! the document can be committed and mounted as it stands.
//!
//! Precedence: `built-in default < FILE < env < flag`. The file is loaded
//! first, then [`super::settings::load`] applies env and flags over it; a flag/env for
//! the same key wins. List-valued keys (`mcp.servers`, `a2a.peers`) *seed* the
//! list — repeatable `--mcp`/`--a2a-peer` flags **add to** the file's list
//! rather than replacing it, matching the repeatable-flag semantics operators
//! already expect.
//!
//! Each file is typed on its own before the merge (the settings' own
//! `deny_unknown_fields`), so a typo'd key (`max_token` vs `max_tokens`) is a
//! hard config error (exit 2) naming the file that carries it, instead of a
//! silently-ignored value — the single most common config footgun, closed at
//! parse time.

use serde_json::Value;
use std::path::Path;

/// The two config-file syntaxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// JSON, with `//` and `/* */` comments tolerated (jsonc).
    Json,
    /// The YAML subset [`super::yaml`] reads.
    Yaml,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Yaml => "yaml",
        }
    }

    /// Decide the format of a config document: the file extension when it is a
    /// known one (`.yaml`/`.yml` ⇒ YAML; `.json`/`.jsonc` ⇒ JSON), else by
    /// sniffing the text — a document whose first significant character (after
    /// whitespace and `//`/`/* */` comments) is `{` or `[` is JSON, anything
    /// else is YAML.
    pub fn detect(path: Option<&Path>, text: &str) -> Format {
        if let Some(ext) = path.and_then(|p| p.extension()).and_then(|e| e.to_str()) {
            match ext.to_ascii_lowercase().as_str() {
                "yaml" | "yml" => return Format::Yaml,
                "json" | "jsonc" => return Format::Json,
                _ => {}
            }
        }
        Format::sniff(text)
    }

    fn sniff(text: &str) -> Format {
        let t = text.strip_prefix('\u{feff}').unwrap_or(text);
        let bytes = t.as_bytes();
        let mut i = 0;
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            break;
        }
        match bytes.get(i) {
            Some(b'{') | Some(b'[') => Format::Json,
            _ => Format::Yaml,
        }
    }
}

/// Parse config text of the given format into its document (a JSON value). A
/// syntax error names the line/column; the document must be a mapping (object)
/// at the top level.
pub fn parse_document(text: &str, format: Format) -> Result<Value, String> {
    let doc = match format {
        Format::Json => {
            let stripped = strip_jsonc(text);
            serde_json::from_str::<Value>(&stripped)
                .map_err(|e| format!("config file parse error (json): {e}"))?
        }
        Format::Yaml => {
            super::yaml::parse(text).map_err(|e| format!("config file parse error (yaml): {e}"))?
        }
    };
    match doc {
        Value::Object(_) => Ok(doc),
        Value::Null if format == Format::Yaml => Ok(Value::Object(serde_json::Map::new())),
        other => Err(format!(
            "config file must be a mapping (an object) at the top level, got {}",
            kind_name(&other)
        )),
    }
}

/// Read + parse a config file from a local path into its document, deciding the
/// format from the extension (else by sniffing the text). Errors name the path.
pub fn read_document(path: &str) -> Result<(Value, Format), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config file {path}: {e}"))?;
    let format = Format::detect(Some(Path::new(path)), &text);
    let doc = parse_document(&text, format).map_err(|e| format!("{path}: {e}"))?;
    Ok((doc, format))
}

/// Read several config files, in order, into ONE effective document: each later
/// file is merged over the previous ones with **JSON Merge Patch** semantics
/// (RFC 7396) — objects merge recursively, scalars and lists are REPLACED by the
/// later file, and a `null` value UNSETS the key. `check(doc, "config file
/// <path>")` — the settings typing — runs on every file before the merge, so an
/// unknown key is attributed to the file that carries it; the merged document
/// is returned with the `(path, format)` list.
pub fn read_documents_checked(
    paths: &[String],
    check: &dyn Fn(&Value, &str) -> Result<(), String>,
) -> Result<(Value, Vec<(String, Format)>), String> {
    let mut merged = Value::Object(serde_json::Map::new());
    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let (doc, format) = read_document(path)?;
        check(&doc, &format!("config file {path}"))?;
        merge_into(&mut merged, doc);
        loaded.push((path.clone(), format));
    }
    Ok((merged, loaded))
}

/// JSON Merge Patch (RFC 7396): `overlay` onto `base`. Objects merge key by key
/// (recursively); any other value — a scalar or a list — replaces what was
/// there; an explicit `null` removes the key. A non-object overlay replaces the
/// base wholesale.
pub fn merge_into(base: &mut Value, overlay: Value) {
    match overlay {
        Value::Object(over) => {
            if !base.is_object() {
                *base = Value::Object(serde_json::Map::new());
            }
            let map = base.as_object_mut().expect("just ensured an object");
            for (k, v) in over {
                match v {
                    Value::Null => {
                        map.remove(&k);
                    }
                    Value::Object(_) => {
                        let slot = map
                            .entry(k)
                            .or_insert(Value::Object(serde_json::Map::new()));
                        merge_into(slot, v);
                    }
                    other => {
                        map.insert(k, other);
                    }
                }
            }
        }
        other => *base = other,
    }
}

fn kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Strip line (`//`) and block (`/* */`) comments from JSON-with-comments,
/// preserving string literals (a `//` inside a `"…"` is data, not a comment).
/// Byte-oriented and minimal — the moat forbids a jsonc *crate*.
///
/// **Everything kept is copied as a SLICE of `src`, never as `byte as char`.**
/// That distinction is the whole UTF-8 story: `0xE2 as char` is U+00E2 ('â'), so
/// a byte-wise copy silently mojibake's an em-dash, an accented name or any CJK
/// text into its Latin-1 shadow — and the result is still valid JSON, so nothing
/// ever reports an error and the agent runs on a subtly wrong instruction.
/// Slicing carries the whole multibyte sequence through untouched.
///
/// Scanning stays byte-wise, which is safe because every byte this function
/// *matches on* (`"`, `\`, `/`, `*`, `\n`) is ASCII, and an ASCII byte can never
/// occur inside a multibyte UTF-8 sequence (continuation bytes are all ≥ 0x80).
/// So a comment boundary is always a char boundary and the slices below can
/// never split a character.
fn strip_jsonc(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    // Start of the run of bytes not yet copied out. A run is broken only by a
    // comment; everything else is emitted verbatim by slicing `src[run..i]`.
    let mut run = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                // Skip the escape AND the escaped byte without inspecting it, so
                // a \" cannot end the string. Both stay in the current run.
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // line comment → skip to end of line (keep the newline for line counts).
            out.push_str(&src[run..i]);
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            run = i;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            // block comment → skip to the closing */.
            out.push_str(&src[run..i]);
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            // Step past the `*/`. On an UNTERMINATED comment `i` is left mid-text,
            // so clamp to the end — `bytes.len()` is always a char boundary, and
            // the malformed document is serde_json's error to report, not ours.
            i = (i + 2).min(bytes.len());
            run = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&src[run..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The settings typing the loader runs on every file.
    fn settings(doc: Value) -> Result<(), String> {
        super::super::settings::Settings::from_document(doc, "config file").map(|_| ())
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields: a typo'd key is a hard error, not silently ignored.
        let e =
            settings(parse_document(r#"{ "max_token": 5 }"#, Format::Json).unwrap()).unwrap_err();
        assert!(e.contains("max_token"), "names the key: {e}");
        // Same for YAML — the typo is named, whatever the syntax.
        let e = settings(parse_document("max_token: 5\n", Format::Yaml).unwrap()).unwrap_err();
        assert!(e.contains("max_token"), "got: {e}");
    }

    #[test]
    fn yaml_and_json_documents_parse_identically() {
        let yaml = r#"
# one document, in YAML
intelligence:
  model: claude-opus-4
  headers:
    anthropic-version: "2023-06-01"
limits:
  run:
    steps: 200
mcp:
  servers:
    - name: web
      endpoint: https://web.example.com/mcp
      headers:
        Authorization: "Bearer {{secret:WEB_TOKEN}}"
      tags:
        "*": [untrusted_input]
observability:
  log_level: info
"#;
        let json = r#"{
            "intelligence": { "model": "claude-opus-4",
                              "headers": { "anthropic-version": "2023-06-01" } },
            "limits": { "run": { "steps": 200 } },
            "mcp": { "servers": [
                { "name": "web", "endpoint": "https://web.example.com/mcp",
                  "headers": { "Authorization": "Bearer {{secret:WEB_TOKEN}}" },
                  "tags": { "*": ["untrusted_input"] } }
            ] },
            "observability": { "log_level": "info" }
        }"#;
        let from_yaml = parse_document(yaml, Format::Yaml).expect("yaml parses");
        let from_json = parse_document(json, Format::Json).expect("json parses");
        assert_eq!(from_yaml, from_json, "one document model, two syntaxes");
        settings(from_yaml).expect("and it is a settings document");
    }

    #[test]
    fn format_detection_by_extension_then_sniff() {
        assert_eq!(
            Format::detect(Some(Path::new("/etc/agentd/config.yaml")), "{}"),
            Format::Yaml
        );
        assert_eq!(Format::detect(Some(Path::new("c.YML")), "{}"), Format::Yaml);
        assert_eq!(
            Format::detect(Some(Path::new("c.json")), "model: x"),
            Format::Json
        );
        assert_eq!(
            Format::detect(Some(Path::new("c.jsonc")), "model: x"),
            Format::Json
        );
        // Unknown extension / no path: sniff the first significant character.
        assert_eq!(
            Format::detect(Some(Path::new("agentd.conf")), "  { \"a\": 1 }"),
            Format::Json
        );
        assert_eq!(Format::detect(None, "// jsonc\n{ \"a\": 1 }"), Format::Json);
        assert_eq!(Format::detect(None, "/* c */ [1]"), Format::Json);
        assert_eq!(Format::detect(None, "# yaml\nmodel: x\n"), Format::Yaml);
        assert_eq!(Format::detect(None, "model: x\n"), Format::Yaml);
        assert_eq!(Format::detect(None, ""), Format::Yaml);
    }

    #[test]
    fn merge_follows_json_merge_patch() {
        let mut base = json!({
            "model": "base",
            "limits": {"max_steps": 1, "max_depth": 2},
            "subscribe": ["a", "b"],
            "headers": {"h1": "v1"},
            "log_level": "info"
        });
        merge_into(
            &mut base,
            json!({
                "model": "over",                    // scalar: replaced
                "limits": {"max_steps": 9},         // object: merged (max_depth kept)
                "subscribe": ["c"],                 // list: REPLACED, not appended
                "headers": {"h2": "v2"},            // map: merged
                "log_level": null                   // null: unset
            }),
        );
        assert_eq!(
            base,
            json!({
                "model": "over",
                "limits": {"max_steps": 9, "max_depth": 2},
                "subscribe": ["c"],
                "headers": {"h1": "v1", "h2": "v2"}
            })
        );
        // A scalar in the way of an object overlay is replaced by the object.
        let mut base = json!({"limits": 5});
        merge_into(&mut base, json!({"limits": {"max_steps": 1}}));
        assert_eq!(base, json!({"limits": {"max_steps": 1}}));
    }

    #[test]
    fn multiple_files_merge_in_order_later_wins() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.yaml");
        let prod = dir.path().join("prod.yaml");
        let extra = dir.path().join("extra.json");
        std::fs::write(
            &base,
            "intelligence: {model: base}\nlimits:\n  run: {steps: 1, tokens: 2}\n",
        )
        .unwrap();
        std::fs::write(
            &prod,
            "intelligence: {model: prod}\nlimits:\n  run: {steps: 9}\n",
        )
        .unwrap();
        std::fs::write(
            &extra,
            r#"{ "observability": {"log_level": "warn"}, "limits": { "run": { "tokens": null } } }"#,
        )
        .unwrap();
        let paths: Vec<String> = [&base, &prod, &extra]
            .iter()
            .map(|p| p.to_str().unwrap().to_string())
            .collect();
        let check = |doc: &Value, source: &str| {
            super::super::settings::Settings::from_document(doc.clone(), source).map(|_| ())
        };
        let (doc, loaded) = read_documents_checked(&paths, &check).unwrap();
        assert_eq!(
            doc,
            json!({
                "intelligence": {"model": "prod"},
                "limits": {"run": {"steps": 9}},
                "observability": {"log_level": "warn"}
            })
        );
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].1, Format::Yaml);
        assert_eq!(loaded[2].1, Format::Json);
        // An unknown key is attributed to the file that carries it.
        std::fs::write(&prod, "modle: typo\n").unwrap();
        let e = read_documents_checked(&paths, &check).unwrap_err();
        assert!(e.contains("prod.yaml") && e.contains("modle"), "{e}");
        // A missing file is an error naming it.
        let e = read_documents_checked(&["/no/such/agentd.yaml".to_string()], &check).unwrap_err();
        assert!(e.contains("/no/such/agentd.yaml"), "{e}");
    }

    #[test]
    fn a_non_mapping_document_is_rejected() {
        let e = parse_document("- a\n- b\n", Format::Yaml).unwrap_err();
        assert!(e.contains("mapping"), "{e}");
        let e = parse_document("[1, 2]", Format::Json).unwrap_err();
        assert!(e.contains("mapping"), "{e}");
        // An empty YAML file is an empty config (nothing set) — not an error.
        assert_eq!(
            parse_document("# nothing yet\n", Format::Yaml).unwrap(),
            json!({})
        );
        // A YAML syntax error names the line.
        let e = parse_document("a: 1\n\tb: 2\n", Format::Yaml).unwrap_err();
        assert!(e.contains("(yaml)") && e.contains("line 2"), "{e}");
    }

    #[test]
    fn malformed_json_is_an_error() {
        let e = parse_document("{ not json", Format::Json).unwrap_err();
        assert!(e.contains("parse error (json)"), "{e}");
    }

    #[test]
    fn jsonc_comments_are_stripped() {
        let src = r#"{
            // a line comment
            "model": "m", /* block */ "max_tokens": 10,
            "subscribe": ["http://x//path"]  // a // inside a string is data
        }"#;
        let doc = parse_document(src, Format::Json).unwrap();
        assert_eq!(doc["model"], json!("m"));
        assert_eq!(doc["max_tokens"], json!(10));
        // The `//` inside the string literal survived (not treated as a comment).
        assert_eq!(doc["subscribe"], json!(["http://x//path"]));
    }

    #[test]
    fn non_ascii_round_trips_through_the_jsonc_stripper() {
        // Silent corruption is the worst failure mode: mojibake'd text is still
        // valid JSON, so a byte-wise stripper would hand the agent a subtly wrong
        // instruction with nothing reporting an error. Every string here must come
        // back byte-identical, INCLUDING the ones pressed up against a comment —
        // that adjacency is exactly where a byte-wise stripper splits a sequence.
        let model = "Ünïcøde — 日本語 μοντέλο";
        let src = format!(
            "{{\n  /* 日本語 block */\"model\": \"{model}\",/*é*/\n  \"subscribe\": [\"fs:file:///wätch/收件箱\"] // — trailing 日本語\n}}"
        );
        let doc = parse_document(&src, Format::Json).unwrap();
        assert_eq!(doc["model"], json!(model), "mojibake in the value");
        assert_eq!(doc["subscribe"], json!(["fs:file:///wätch/收件箱"]));
        // The stripper itself must be the identity on a comment-free document.
        let plain = format!("{{ \"model\": \"{model}\" }}");
        assert_eq!(strip_jsonc(&plain), plain);
        // A \-escape adjacent to multibyte text must not eat the following byte.
        let doc = parse_document("{ \"model\": \"a\\\"—\\\\é\" }", Format::Json).unwrap();
        assert_eq!(doc["model"], json!("a\"—\\é"));
    }
}
