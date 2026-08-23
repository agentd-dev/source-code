// SPDX-License-Identifier: AGPL-3.0-only
//! **Colon-fence directives** in operator-authored text — the
//! `:::type{attrs}` … `:::` container syntax (the MyST / remark-directive /
//! ChatGPT subset), so an instruction can CARRY the machinery it describes:
//!
//! ```text
//! You triage the queue. Escalate anything risky.
//!
//! :::workflow
//! name: triage
//! steps:
//!   wake: { kind: subscribe, server: queue, uri: "queue://inbox" }
//!   act:  { kind: agent, depends_on: [wake], instruction: "triage it" }
//!   done: { kind: finish, depends_on: [act] }
//! :::
//! ```
//!
//! Design decisions, in order of load-bearing-ness:
//!
//! - **Directives are a property of the SURFACE, not the text.** This module
//!   only parses; the config layer runs it over operator-authored instruction
//!   text (inline, `--instruction-file`, a config file). Conversation text is
//!   never parsed — executing definitions out of untrusted input would be
//!   prompt injection as a feature.
//! - **Blocks are sugar over existing pipelines, never a parallel mechanism.**
//!   A `:::workflow` body joins `workflows:` exactly as an inline entry —
//!   same vars folding, validation, hashing, pinning, reload diffing and
//!   retirement. A `:::skill` joins the skills catalogue like a discovered
//!   one. Nothing here can drift from the real thing, because it IS the real
//!   thing.
//! - **Unknown names fail closed.** `:::worfklow` silently becoming prose is
//!   a trap; the known set is enumerated in the error. Text that legitimately
//!   needs a literal `:::` at column 0 can indent it.
//! - The grammar is the small end of MyST: `:::name{key=value key="v v"}` on
//!   one line, body verbatim, closed by a line of at least as many colons.
//!   Nest by giving the OUTER fence more colons. No roles, no `:key:` option
//!   lines — those are documentation-system surface, not config surface.

use serde_json::Value;
use std::collections::BTreeMap;

/// One parsed block.
#[derive(Debug, Clone, PartialEq)]
pub struct Directive {
    pub name: String,
    pub attrs: BTreeMap<String, String>,
    pub body: String,
    /// 1-based line of the opening fence, for error messages.
    pub line: usize,
}

/// What instruction-surface extraction produces.
#[derive(Debug, Default, PartialEq)]
pub struct Extraction {
    /// The text with directive machinery removed: `workflow`/`skill` blocks
    /// replaced by a one-line note (so prose and machinery cannot
    /// double-speak), `context`/`example` bodies kept, delimited with tags a
    /// model reads well.
    pub cleaned: String,
    /// `:::workflow` bodies, parsed to documents ready for `workflows:`.
    pub workflows: Vec<Value>,
    /// `:::skill{name}` bodies for the catalogue.
    pub skills: Vec<InlineSkill>,
    /// The config fragment the document's `:::config` / `:::mcp` /
    /// `:::stream` / `:::tools` blocks assemble — a v2 document subtree that
    /// merges UNDER the explicit config (an explicit key always wins), so a
    /// single instruction file can define the whole agent while a config
    /// file, env, or flag can still override any of it.
    pub config: Value,
}

/// A skill defined inline — the catalogue entry plus its body, no MCP server
/// involved.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InlineSkill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    pub body: String,
}

/// The names this surface understands. Fail-closed: anything else at a fence
/// is an error naming this set.
const KNOWN: &[&str] = &[
    "workflow", "skill", "context", "example", "config", "mcp", "stream", "tools",
];

/// Parse every top-level directive out of `text`. Returns the directives and
/// the text segments between them, or every problem found.
pub fn parse(text: &str) -> Result<(Vec<Segment>, Vec<Directive>), Vec<String>> {
    let mut errs = Vec::new();
    let mut segments = Vec::new();
    let mut directives = Vec::new();
    let mut plain = String::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some((fence_len, name, attr_src)) = open_fence(line) {
            if !KNOWN.contains(&name.as_str()) {
                errs.push(format!(
                    "line {}: unknown directive :::{name} (known: {})",
                    i + 1,
                    KNOWN.join(", ")
                ));
            }
            let attrs = match parse_attrs(&attr_src) {
                Ok(a) => a,
                Err(e) => {
                    errs.push(format!("line {}: :::{name}: {e}", i + 1));
                    BTreeMap::new()
                }
            };
            // Find the closing fence: a line of >= fence_len colons, nothing else.
            let open_line = i + 1;
            let mut body = String::new();
            let mut closed = false;
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                let t = l.trim_end();
                if t.len() >= fence_len && t.chars().all(|c| c == ':') {
                    closed = true;
                    break;
                }
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(l);
                i += 1;
            }
            if !closed {
                errs.push(format!(
                    "line {open_line}: :::{name} is never closed (expected a line of {fence_len}+ colons)"
                ));
            }
            if !plain.is_empty() {
                segments.push(Segment::Text(std::mem::take(&mut plain)));
            }
            segments.push(Segment::Directive(directives.len()));
            directives.push(Directive {
                name,
                attrs,
                body,
                line: open_line,
            });
            i += 1; // past the close
        } else {
            if !plain.is_empty() {
                plain.push('\n');
            }
            plain.push_str(line);
            i += 1;
        }
    }
    if !plain.is_empty() {
        segments.push(Segment::Text(plain));
    }
    if errs.is_empty() {
        Ok((segments, directives))
    } else {
        Err(errs)
    }
}

/// A run of plain text, or the index of a directive between runs.
#[derive(Debug, PartialEq)]
pub enum Segment {
    Text(String),
    Directive(usize),
}

/// `:::name{...}` at column 0 → `(fence length, name, attr source)`.
fn open_fence(line: &str) -> Option<(usize, String, String)> {
    let t = line.trim_end();
    let colons = t.chars().take_while(|c| *c == ':').count();
    if colons < 3 {
        return None;
    }
    let rest = &t[colons..];
    if rest.is_empty() {
        return None; // a bare fence line opens nothing (it can only close)
    }
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    if name.is_empty() {
        return None;
    }
    let after = &rest[name.len()..];
    let attrs = after.trim();
    if !attrs.is_empty() && !(attrs.starts_with('{') && attrs.ends_with('}')) {
        return None; // `::: three colons then prose` is prose, not a fence
    }
    let attr_src = attrs
        .strip_prefix('{')
        .and_then(|a| a.strip_suffix('}'))
        .unwrap_or("")
        .to_string();
    Some((colons, name, attr_src))
}

/// `key=value key="quoted value" flag` → map (`flag` → `"true"`).
fn parse_attrs(src: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut chars = src.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&c0) = chars.peek() else { break };
        if !(c0.is_ascii_alphanumeric() || c0 == '_' || c0 == '-') {
            return Err(format!("unexpected {c0:?} in attributes"));
        }
        let mut key = String::new();
        while chars
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        {
            key.push(chars.next().unwrap());
        }
        if chars.peek() == Some(&'=') {
            chars.next();
            let mut val = String::new();
            if chars.peek() == Some(&'"') {
                chars.next();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => {
                            if let Some(e) = chars.next() {
                                val.push(e);
                            }
                        }
                        _ => val.push(c),
                    }
                }
                if !closed {
                    return Err(format!("unterminated quote in {key}=\"…"));
                }
            } else {
                while chars.peek().is_some_and(|c| !c.is_whitespace()) {
                    val.push(chars.next().unwrap());
                }
            }
            out.insert(key, val);
        } else {
            out.insert(key, "true".to_string());
        }
    }
    Ok(out)
}

/// The fragment as a mutable map, created on first use (it stays `Null` for
/// documents that carry no config-defining blocks).
fn frag(config: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !config.is_object() {
        *config = Value::Object(Default::default());
    }
    config.as_object_mut().expect("just ensured")
}

/// A bare attribute value, typed the way YAML would read it — so
/// `{name=fs timeout=30s aauth=true}` behaves like the equivalent body keys.
fn attr_value(s: &str) -> Value {
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => s
            .parse::<i64>()
            .map(Into::into)
            .unwrap_or_else(|_| Value::String(s.to_string())),
    }
}

/// Deep-merge `add` into `into`, LATER-WINS at every leaf (used between
/// directives in one document: the second `:::config` overrides the first,
/// like a later config file).
fn merge_over(into: &mut serde_json::Map<String, Value>, add: serde_json::Map<String, Value>) {
    for (k, v) in add {
        match (into.get_mut(&k), v) {
            (Some(Value::Object(dst)), Value::Object(src)) => merge_over(dst, src),
            (Some(slot), v) => *slot = v,
            (None, v) => {
                into.insert(k, v);
            }
        }
    }
}

/// Deep-merge `frag` into `doc`, DOC-WINS at every leaf — the fragment fills
/// what the explicit config left unsaid and never overrides what it said.
/// Lists are leaves (no splicing), with one deliberate exception:
/// `mcp.servers` entries APPEND when no explicit server has the same name —
/// declaring a server in the instruction must not require the config file to
/// have none.
pub fn merge_missing(
    doc: &mut serde_json::Map<String, Value>,
    frag: serde_json::Map<String, Value>,
    at_mcp: bool,
) {
    for (k, v) in frag {
        match (doc.get_mut(&k), v) {
            (Some(Value::Array(have)), Value::Array(add)) if at_mcp && k == "servers" => {
                let names: Vec<String> = have
                    .iter()
                    .filter_map(|s| s.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect();
                for entry in add {
                    let dup = entry
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| names.iter().any(|h| h == n));
                    if !dup {
                        have.push(entry);
                    }
                }
            }
            (Some(Value::Object(dst)), Value::Object(src)) => {
                merge_missing(dst, src, k == "mcp");
            }
            (Some(_), _) => {}
            (None, v) => {
                doc.insert(k, v);
            }
        }
    }
}

/// Run extraction over an instruction-surface text: parse, interpret the
/// known blocks, rebuild the text a model should see.
pub fn extract(text: &str) -> Result<Extraction, Vec<String>> {
    // The cheap gate: most instructions carry no fences at all.
    if !text.lines().any(|l| l.starts_with(":::")) {
        return Ok(Extraction {
            cleaned: text.to_string(),
            ..Default::default()
        });
    }
    let (segments, directives) = parse(text)?;
    let mut errs = Vec::new();
    let mut out = Extraction::default();
    for seg in &segments {
        match seg {
            Segment::Text(t) => out.cleaned.push_str(t),
            Segment::Directive(ix) => {
                let d = &directives[*ix];
                match d.name.as_str() {
                    "workflow" => match crate::config::yaml::parse(&d.body) {
                        Ok(mut doc) => {
                            if let Some(n) = d.attrs.get("name")
                                && let Some(o) = doc.as_object_mut()
                            {
                                o.insert("name".into(), Value::String(n.clone()));
                            }
                            if let Some(armed) = d.attrs.get("armed")
                                && let Some(o) = doc.as_object_mut()
                            {
                                o.insert("armed".into(), Value::Bool(armed == "true"));
                            }
                            let name = doc
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("?")
                                .to_string();
                            out.cleaned.push_str(&format!(
                                "[workflow \"{name}\" is loaded and runs autonomously]"
                            ));
                            out.workflows.push(doc);
                        }
                        Err(e) => errs.push(format!(
                            "line {}: :::workflow body is not valid YAML: {e}",
                            d.line
                        )),
                    },
                    "skill" => {
                        let Some(name) = d.attrs.get("name").cloned() else {
                            errs.push(format!(
                                "line {}: :::skill needs a name ({{name=…}})",
                                d.line
                            ));
                            continue;
                        };
                        out.cleaned.push_str(&format!(
                            "[skill \"{name}\" is available — reference it as @skill:{name}]"
                        ));
                        out.skills.push(InlineSkill {
                            name,
                            description: d.attrs.get("description").cloned().unwrap_or_default(),
                            when_to_use: d.attrs.get("when").cloned(),
                            body: d.body.clone(),
                        });
                    }
                    // Config-defining blocks: each folds into ONE fragment that
                    // the config layer merges UNDER the explicit document — so
                    // an instruction file alone can define the whole agent, and
                    // an explicit config key / env / flag still wins.
                    "config" => match crate::config::yaml::parse(&d.body) {
                        Ok(Value::Object(m)) => {
                            merge_over(frag(&mut out.config), m);
                            out.cleaned.push_str("[runtime configuration is applied]");
                        }
                        Ok(_) => errs.push(format!(
                            "line {}: :::config body must be a YAML mapping of config sections",
                            d.line
                        )),
                        Err(e) => errs.push(format!(
                            "line {}: :::config body is not valid YAML: {e}",
                            d.line
                        )),
                    },
                    "mcp" => {
                        let body = if d.body.trim().is_empty() {
                            Ok(Value::Object(serde_json::Map::new()))
                        } else {
                            crate::config::yaml::parse(&d.body)
                        };
                        match body {
                            Ok(Value::Object(mut m)) => {
                                for (k, v) in &d.attrs {
                                    m.insert(k.clone(), attr_value(v));
                                }
                                let Some(name) =
                                    m.get("name").and_then(Value::as_str).map(str::to_string)
                                else {
                                    errs.push(format!(
                                        "line {}: :::mcp needs a name ({{name=…}} or `name:` in the body)",
                                        d.line
                                    ));
                                    continue;
                                };
                                out.cleaned.push_str(&format!(
                                    "[mcp server \"{name}\" is connected; its tools are available]"
                                ));
                                let servers = frag(&mut out.config)
                                    .entry("mcp")
                                    .or_insert_with(|| Value::Object(Default::default()));
                                if let Some(o) = servers.as_object_mut() {
                                    o.entry("servers")
                                        .or_insert_with(|| Value::Array(Vec::new()))
                                        .as_array_mut()
                                        .expect("just made")
                                        .push(Value::Object(m));
                                }
                            }
                            Ok(_) => errs.push(format!(
                                "line {}: :::mcp body must be a YAML mapping (the mcp.servers entry)",
                                d.line
                            )),
                            Err(e) => errs.push(format!(
                                "line {}: :::mcp body is not valid YAML: {e}",
                                d.line
                            )),
                        }
                    }
                    "stream" => {
                        let Some(name) = d.attrs.get("name").cloned() else {
                            errs.push(format!(
                                "line {}: :::stream needs a name ({{name=…}})",
                                d.line
                            ));
                            continue;
                        };
                        let body = if d.body.trim().is_empty() {
                            Ok(Value::Object(serde_json::Map::new()))
                        } else {
                            crate::config::yaml::parse(&d.body)
                        };
                        match body {
                            Ok(v @ Value::Object(_)) => {
                                out.cleaned
                                    .push_str(&format!("[event stream \"{name}\" is declared]"));
                                let streams = frag(&mut out.config)
                                    .entry("streams")
                                    .or_insert_with(|| Value::Object(Default::default()));
                                if let Some(o) = streams.as_object_mut() {
                                    o.insert(name, v);
                                }
                            }
                            Ok(_) => errs.push(format!(
                                "line {}: :::stream body must be a YAML mapping (retention: …)",
                                d.line
                            )),
                            Err(e) => errs.push(format!(
                                "line {}: :::stream body is not valid YAML: {e}",
                                d.line
                            )),
                        }
                    }
                    "tools" => match crate::config::yaml::parse(&d.body) {
                        Ok(Value::Object(m)) => {
                            out.cleaned.push_str("[tool policy is applied]");
                            let tools = frag(&mut out.config)
                                .entry("tools")
                                .or_insert_with(|| Value::Object(Default::default()));
                            if let Some(o) = tools.as_object_mut() {
                                merge_over(o, m);
                            }
                        }
                        Ok(_) => errs.push(format!(
                            "line {}: :::tools body must be a YAML mapping (disabled/overrides)",
                            d.line
                        )),
                        Err(e) => errs.push(format!(
                            "line {}: :::tools body is not valid YAML: {e}",
                            d.line
                        )),
                    },
                    // Model-facing: the fence goes, the body stays, delimited
                    // with tags a model reads unambiguously.
                    "context" | "example" => {
                        let tag = if d.name == "context" {
                            "reference"
                        } else {
                            "example"
                        };
                        match d.attrs.get("title") {
                            Some(t) => out
                                .cleaned
                                .push_str(&format!("<{tag} title=\"{t}\">\n{}\n</{tag}>", d.body)),
                            None => out
                                .cleaned
                                .push_str(&format!("<{tag}>\n{}\n</{tag}>", d.body)),
                        }
                    }
                    _ => unreachable!("parse() rejects unknown names"),
                }
            }
        }
    }
    if errs.is_empty() { Ok(out) } else { Err(errs) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through_untouched() {
        let t = "just prose\nwith lines\nand a ::: mid-sentence is fine";
        let e = extract(t).unwrap();
        assert_eq!(e.cleaned, t);
        assert!(e.workflows.is_empty() && e.skills.is_empty());
    }

    #[test]
    fn a_workflow_block_becomes_a_document_and_a_note() {
        let t = "Do the thing.\n\n:::workflow{armed=true}\nname: w\nsteps:\n  s: {kind: once}\n  f: {kind: finish, depends_on: [s]}\n:::\n\nBe nice.";
        let e = extract(t).unwrap();
        assert_eq!(e.workflows.len(), 1);
        assert_eq!(e.workflows[0]["name"], "w");
        assert_eq!(e.workflows[0]["armed"], true);
        assert!(e.cleaned.contains("[workflow \"w\" is loaded"));
        assert!(!e.cleaned.contains(":::"), "{}", e.cleaned);
        assert!(e.cleaned.starts_with("Do the thing.") && e.cleaned.ends_with("Be nice."));
    }

    #[test]
    fn name_attr_overrides_the_body_name() {
        let t = ":::workflow{name=renamed}\nname: original\nsteps: {}\n:::";
        let e = extract(t).unwrap();
        assert_eq!(e.workflows[0]["name"], "renamed");
    }

    #[test]
    fn a_skill_block_joins_the_catalogue_with_a_reference_note() {
        let t = ":::skill{name=review description=\"how we review\" when=\"reviewing PRs\"}\nAlways check the tests first.\n:::";
        let e = extract(t).unwrap();
        assert_eq!(e.skills.len(), 1);
        assert_eq!(e.skills[0].name, "review");
        assert_eq!(e.skills[0].description, "how we review");
        assert_eq!(e.skills[0].when_to_use.as_deref(), Some("reviewing PRs"));
        assert_eq!(e.skills[0].body, "Always check the tests first.");
        assert!(e.cleaned.contains("@skill:review"));
    }

    #[test]
    fn context_and_example_keep_their_bodies_in_tags() {
        let t = ":::context{title=\"API notes\"}\nrate limit is 10/s\n:::\n:::example\nQ: hi\nA: hello\n:::";
        let e = extract(t).unwrap();
        assert!(
            e.cleaned
                .contains("<reference title=\"API notes\">\nrate limit is 10/s\n</reference>")
        );
        assert!(e.cleaned.contains("<example>\nQ: hi\nA: hello\n</example>"));
    }

    #[test]
    fn unknown_names_and_unclosed_fences_fail_closed_with_lines() {
        let e = extract(":::worfklow\nx: 1\n:::").unwrap_err();
        assert!(
            e[0].contains("line 1") && e[0].contains("unknown directive"),
            "{e:?}"
        );
        assert!(e[0].contains("workflow, skill, context, example"), "{e:?}");
        let e = extract("intro\n:::workflow\nname: w").unwrap_err();
        assert!(e.iter().any(|m| m.contains("never closed")), "{e:?}");
        let e = extract(":::workflow\n[not yaml\n:::").unwrap_err();
        assert!(e.iter().any(|m| m.contains("not valid YAML")), "{e:?}");
    }

    #[test]
    fn longer_outer_fences_nest_literal_inner_ones() {
        let t = "::::context\ninner literal:\n:::workflow\nnot parsed\n:::\ndone\n::::";
        let e = extract(t).unwrap();
        assert!(e.workflows.is_empty(), "inner fence is body text");
        assert!(e.cleaned.contains(":::workflow"), "{}", e.cleaned);
    }

    #[test]
    fn attributes_parse_quotes_escapes_and_flags() {
        let a = parse_attrs(r#"name=x title="a \"b\" c" armed flag-2=7"#).unwrap();
        assert_eq!(a["name"], "x");
        assert_eq!(a["title"], "a \"b\" c");
        assert_eq!(a["armed"], "true");
        assert_eq!(a["flag-2"], "7");
        assert!(parse_attrs("name=\"unterminated").is_err());
    }

    #[test]
    fn config_blocks_fold_into_one_fragment_later_wins() {
        let t = ":::config\nlimits: {max_runs: 5}\nstore: {kind: memory}\n:::\n\
                 prose between\n\
                 :::config\nlimits: {max_runs: 9}\n:::\n";
        let e = extract(t).unwrap();
        assert_eq!(e.config["limits"]["max_runs"], 9, "later block wins");
        assert_eq!(e.config["store"]["kind"], "memory");
        assert!(e.cleaned.contains("[runtime configuration is applied]"));
        assert!(
            !e.cleaned.contains("max_runs"),
            "machinery never reaches the model"
        );
    }

    #[test]
    fn mcp_stream_and_tools_blocks_build_the_fragment() {
        let t = ":::mcp{name=fs timeout=30s}\nendpoint: \"https://fs.internal/mcp\"\nallow: [\"read_*\", \"list_*\"]\nexclude: [\"read_secrets\"]\n:::\n\
                 :::stream{name=orders}\nretention: {max_events: 50}\n:::\n\
                 :::stream{name=alerts}\n:::\n\
                 :::tools\ndisabled: [\"exec\"]\n:::\n";
        let e = extract(t).unwrap();
        let srv = &e.config["mcp"]["servers"][0];
        assert_eq!(srv["name"], "fs");
        assert_eq!(srv["timeout"], "30s", "attrs merge over the body");
        assert_eq!(srv["allow"][0], "read_*");
        assert_eq!(srv["exclude"][0], "read_secrets");
        assert_eq!(e.config["streams"]["orders"]["retention"]["max_events"], 50);
        assert!(
            e.config["streams"]["alerts"].is_object(),
            "empty body = defaults"
        );
        assert_eq!(e.config["tools"]["disabled"][0], "exec");
        assert!(e.cleaned.contains("mcp server \"fs\""), "{}", e.cleaned);
        assert!(e.cleaned.contains("stream \"orders\""), "{}", e.cleaned);
    }

    #[test]
    fn the_fragment_merges_under_the_explicit_doc() {
        let mut doc = serde_json::json!({
            "limits": {"max_runs": 3},
            "mcp": {"servers": [{"name": "fs", "endpoint": "https://real"}]}
        });
        let frag = serde_json::json!({
            "limits": {"max_runs": 9, "step_timeout": "10s"},
            "mcp": {"servers": [
                {"name": "fs", "endpoint": "https://SHADOW"},
                {"name": "gh", "endpoint": "https://gh"}
            ]},
            "streams": {"orders": {}}
        });
        merge_missing(
            doc.as_object_mut().unwrap(),
            frag.as_object().unwrap().clone(),
            false,
        );
        assert_eq!(doc["limits"]["max_runs"], 3, "explicit config wins");
        assert_eq!(doc["limits"]["step_timeout"], "10s", "fragment fills gaps");
        let servers = doc["mcp"]["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2, "new server appends; same-name does not");
        assert_eq!(
            servers[0]["endpoint"], "https://real",
            "no shadowing by name"
        );
        assert_eq!(servers[1]["name"], "gh");
        assert!(doc["streams"]["orders"].is_object());
    }

    #[test]
    fn a_nameless_mcp_or_stream_block_fails_closed() {
        assert!(extract(":::mcp\nendpoint: \"https://x\"\n:::\n").is_err());
        assert!(extract(":::stream\nretention: {}\n:::\n").is_err());
        assert!(extract(":::config\n- a list\n:::\n").is_err());
    }
}
