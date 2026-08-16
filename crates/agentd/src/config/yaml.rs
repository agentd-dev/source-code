// SPDX-License-Identifier: Apache-2.0
//! A hand-rolled **YAML subset** reader for config files → `serde_json::Value`.
//!
//! agentd's config file may be YAML (`--config agentd.yaml`) — but the
//! minimalism moat forbids a YAML *crate* (`serde_yaml` is also unmaintained),
//! so, like the cron parser, the HTTP client, and the JSON-with-comments
//! stripper, this is written on `std` alone and produces the same
//! `serde_json::Value` tree the JSON path yields. One document model, two
//! surface syntaxes; everything downstream (typed deserialization, schema
//! validation, hot reload) is format-agnostic.
//!
//! ## What is supported (the config-file subset)
//!
//! - block mappings (`key: value`, nesting by indentation) and block sequences
//!   (`- item`, including `- key: value` items and nested sequences, and a
//!   sequence at the same indent as its key);
//! - flow collections (`[a, b]`, `{k: v}`), nested and spanning lines;
//! - scalars: plain, single-quoted (`''` escape), double-quoted (JSON escapes +
//!   `\xHH`/`\uHHHH`/`\UHHHHHHHH`), and block scalars `|` / `>` with `-`/`+`
//!   chomping and an explicit indentation indicator;
//! - multi-line plain scalars (continuation lines fold with a space);
//! - comments (`#` at line start or after whitespace), blank lines, `---` /
//!   `...` document markers, `%` directives, a UTF-8 BOM, CRLF line endings;
//! - YAML 1.2 **core-schema** typing of plain scalars: `null`/`~`/empty → null,
//!   `true`/`false` (any case) → bool, decimal/`0x`/`0o` integers, floats;
//!   everything else is a string (`yes`/`no`/`on`/`off` are STRINGS — the
//!   1.1 footgun is deliberately absent).
//!
//! ## What is rejected, loudly
//!
//! Anchors/aliases (`&a`/`*a`), tags (`!!str`), merge keys (`<<`), complex keys
//! (`? `), multiple documents, tab indentation, multi-line quoted scalars, and
//! duplicate mapping keys — each is a parse error naming the line and column,
//! never a silent guess. Non-finite floats (`.inf`/`.nan`) are rejected because
//! JSON cannot carry them.

use serde_json::{Map, Number, Value};

/// A YAML parse error with a 1-based line and column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

impl std::fmt::Display for YamlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}, column {}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for YamlError {}

/// Parse one YAML document into a JSON value. An empty document is `null`.
pub fn parse(src: &str) -> Result<Value, YamlError> {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let raw: Vec<&str> = src
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let lines = logical_lines(&raw)?;
    let mut p = Parser { raw, lines, pos: 0 };
    let Some(first) = p.peek().cloned() else {
        return Ok(Value::Null);
    };
    let v = p.parse_node(first.indent, first.indent)?;
    if let Some(l) = p.peek() {
        return Err(err(l, 0, "unexpected content after the document root"));
    }
    Ok(v)
}

/// Parse a single inline YAML value (a scalar or a flow collection) — the shape
/// an env var or a `--flag` value carries: `12`, `true`, `[a, b]`, `{k: v}`,
/// `"quoted"`. A plain scalar that types as a string comes back verbatim
/// (trimmed).
pub fn parse_inline(src: &str) -> Result<Value, YamlError> {
    let line = Line {
        no: 1,
        indent: 0,
        text: src.trim().to_string(),
    };
    let (v, rest, _plain) = parse_inline_value(&line, &line.text, 0)?;
    if !rest.trim().is_empty() {
        return Err(err(
            &line,
            line.text.len() - rest.len(),
            "trailing characters after the value",
        ));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Logical lines
// ---------------------------------------------------------------------------

/// One significant source line: its 1-based number, indentation (spaces) and
/// the text after the indentation with any trailing comment removed.
#[derive(Debug, Clone)]
struct Line {
    no: usize,
    indent: usize,
    text: String,
}

fn err(l: &Line, col_in_text: usize, msg: impl Into<String>) -> YamlError {
    YamlError {
        line: l.no,
        col: l.indent + col_in_text + 1,
        msg: msg.into(),
    }
}

/// Reduce the raw lines to significant ones: drop blank and comment-only lines,
/// `%` directives and `---`/`...` markers (a second `---` is a multi-document
/// stream — rejected), measure the indentation and strip trailing comments.
/// Block-scalar bodies are re-read from the RAW lines by the parser (blank lines
/// and `#` are content there), so what happens to them here is irrelevant.
fn logical_lines(raw: &[&str]) -> Result<Vec<Line>, YamlError> {
    let mut out = Vec::new();
    let mut seen_doc_start = false;
    for (i, &line) in raw.iter().enumerate() {
        let no = i + 1;
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        if trimmed.trim().is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indent == 0 {
            if trimmed.starts_with('%') {
                continue; // %YAML / %TAG directives: ignored
            }
            if trimmed == "---" || trimmed.starts_with("--- ") {
                if seen_doc_start || !out.is_empty() {
                    return Err(YamlError {
                        line: no,
                        col: 1,
                        msg: "multiple YAML documents are not supported (one document per file)"
                            .into(),
                    });
                }
                seen_doc_start = true;
                let rest = trimmed[3..].trim();
                if rest.is_empty() {
                    continue;
                }
                // `--- {inline: doc}` — the rest IS the document root.
                out.push(Line {
                    no,
                    indent: 4,
                    text: strip_comment(rest),
                });
                continue;
            }
            if trimmed == "..." {
                break; // document end marker: nothing after it is content
            }
        }
        out.push(Line {
            no,
            indent,
            text: strip_comment(trimmed),
        });
    }
    Ok(out)
}

/// Strip a trailing ` # comment` (a `#` at the start or after whitespace, and
/// not inside single/double quotes), then trailing whitespace.
fn strip_comment(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2; // `''` escape
                    continue;
                }
                in_single = false;
            }
        } else if b == b'"' && quote_opens(bytes, i) {
            in_double = true;
        } else if b == b'\'' && quote_opens(bytes, i) {
            in_single = true;
        } else if b == b'#' && (i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
            return text[..i].trim_end().to_string();
        }
        i += 1;
    }
    text.trim_end().to_string()
}

/// A quote character opens a quoted scalar only at a value/element start —
/// after `: `, `- `, `[`, `{`, `,`, or at the line start. Elsewhere (`it's`,
/// `5"`) it is plain-scalar content.
fn quote_opens(bytes: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && bytes[j - 1] == b' ' {
        j -= 1;
    }
    j == 0 || matches!(bytes[j - 1], b':' | b'-' | b'[' | b'{' | b',')
}

// ---------------------------------------------------------------------------
// The block parser
// ---------------------------------------------------------------------------

struct Parser<'s> {
    /// Every source line, CR-stripped, by index (`Line::no - 1`) — the block
    /// scalar reader needs the raw text (blank lines, `#`, trailing spaces).
    raw: Vec<&'s str>,
    lines: Vec<Line>,
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Line> {
        self.lines.get(self.pos)
    }

    /// Parse the node starting at the current line (which sits at `indent`).
    /// `parent_indent` is the indentation of the enclosing block — a block
    /// scalar's body must be indented deeper than THAT (relevant for `- |`,
    /// where the virtual line's indent is deeper than the dash).
    fn parse_node(&mut self, indent: usize, parent_indent: usize) -> Result<Value, YamlError> {
        let Some(line) = self.peek().cloned() else {
            return Ok(Value::Null);
        };
        check_tab(&line)?;
        if is_seq_item(&line.text) {
            return self.parse_sequence(indent);
        }
        if let Some(header) = line
            .text
            .strip_prefix('|')
            .or_else(|| line.text.strip_prefix('>'))
        {
            let literal = line.text.starts_with('|');
            self.pos += 1;
            return self.parse_block_scalar(&line, parent_indent, literal, header, 0);
        }
        if split_key(&line)?.is_some() {
            return self.parse_mapping(indent);
        }
        // A bare scalar / flow collection as the whole node (the document root,
        // or a sequence item's value).
        self.pos += 1;
        let text = self.gather_flow(&line, &line.text, indent)?;
        let (v, rest, plain) = parse_inline_value(&line, &text, 0)?;
        if !rest.trim().is_empty() {
            return Err(err(
                &line,
                text.len() - rest.len(),
                "trailing characters after the value",
            ));
        }
        if plain {
            return self.fold_plain_continuation(v, indent);
        }
        Ok(v)
    }

    fn parse_sequence(&mut self, indent: usize) -> Result<Value, YamlError> {
        let mut items = Vec::new();
        while let Some(line) = self.peek().cloned() {
            check_tab(&line)?;
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return Err(err(&line, 0, "unexpected indentation inside a sequence"));
            }
            if !is_seq_item(&line.text) {
                break; // a sibling mapping key at this indent — the caller decides
            }
            let rest = line.text[1..].trim_start_matches(' ');
            if rest.is_empty() {
                // `-` alone: the item is the nested node on the following lines.
                self.pos += 1;
                match self.peek() {
                    Some(next) if next.indent > indent => {
                        let ni = next.indent;
                        items.push(self.parse_node(ni, indent)?);
                    }
                    _ => items.push(Value::Null),
                }
                continue;
            }
            // `- key: v` / `- - x` / `- scalar` / `- |`: treat the rest as a
            // virtual line starting at the column where it begins, and parse
            // THAT as a node whose parent block is this sequence.
            let inner_indent = indent + (line.text.len() - rest.len());
            self.lines[self.pos] = Line {
                no: line.no,
                indent: inner_indent,
                text: rest.to_string(),
            };
            items.push(self.parse_node(inner_indent, indent)?);
        }
        Ok(Value::Array(items))
    }

    fn parse_mapping(&mut self, indent: usize) -> Result<Value, YamlError> {
        let mut map = Map::new();
        while let Some(line) = self.peek().cloned() {
            check_tab(&line)?;
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return Err(err(&line, 0, "unexpected indentation inside a mapping"));
            }
            if is_seq_item(&line.text) {
                return Err(err(
                    &line,
                    0,
                    "a sequence item is not allowed here (expected a `key: value`)",
                ));
            }
            let Some((key, rest)) = split_key(&line)? else {
                return Err(err(&line, 0, "expected a `key: value` mapping entry"));
            };
            if map.contains_key(&key) {
                return Err(err(&line, 0, format!("duplicate mapping key {key:?}")));
            }
            let rest = rest.to_string();
            self.pos += 1;
            let value = self.parse_mapping_value(&line, indent, &rest)?;
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }

    /// The value part of `key: <rest>` — inline, a nested block on the
    /// following lines, or a block scalar.
    fn parse_mapping_value(
        &mut self,
        line: &Line,
        indent: usize,
        rest: &str,
    ) -> Result<Value, YamlError> {
        let rest_col = line.text.len() - rest.len();
        if rest.is_empty() {
            // A nested block on the following more-indented lines — or a
            // sequence at the SAME indent (`key:\n- a` is legal YAML) — or null.
            return match self.peek() {
                Some(next) if next.indent > indent => {
                    let ni = next.indent;
                    self.parse_node(ni, indent)
                }
                Some(next) if next.indent == indent && is_seq_item(&next.text) => {
                    self.parse_sequence(indent)
                }
                _ => Ok(Value::Null),
            };
        }
        if let Some(header) = rest.strip_prefix('|').or_else(|| rest.strip_prefix('>')) {
            let literal = rest.starts_with('|');
            return self.parse_block_scalar(line, indent, literal, header, rest_col);
        }
        let text = self.gather_flow(line, rest, indent)?;
        let (v, tail, plain) = parse_inline_value(line, &text, rest_col)?;
        if !tail.trim().is_empty() {
            let col = rest_col + (text.len() - tail.len());
            return Err(err(line, col, "trailing characters after the value"));
        }
        if plain {
            return self.fold_plain_continuation(v, indent);
        }
        Ok(v)
    }

    /// A flow collection (`[`/`{`) may span lines: pull following lines that
    /// are indented deeper than the owning block until the brackets balance.
    fn gather_flow(
        &mut self,
        line: &Line,
        start: &str,
        indent: usize,
    ) -> Result<String, YamlError> {
        let t = start.trim_start();
        if !(t.starts_with('[') || t.starts_with('{')) {
            return Ok(start.to_string());
        }
        // Flow context is indentation-insensitive: continuation lines (and the
        // closing bracket) may sit at any column, as they do in JSON-style YAML.
        let _ = indent;
        let mut buf = start.to_string();
        while !flow_balanced(&buf) {
            match self.peek() {
                Some(next) => {
                    buf.push(' ');
                    buf.push_str(&next.text);
                    self.pos += 1;
                }
                None => {
                    return Err(err(
                        line,
                        line.text.len() - start.len(),
                        "unterminated flow collection (missing `]` or `}`)",
                    ));
                }
            }
        }
        Ok(buf)
    }

    /// A plain scalar continues on more-indented lines that are neither mapping
    /// entries nor sequence items; the pieces fold with a single space.
    fn fold_plain_continuation(&mut self, v: Value, indent: usize) -> Result<Value, YamlError> {
        let mut pieces: Vec<String> = Vec::new();
        while let Some(next) = self.peek() {
            if next.indent <= indent || is_seq_item(&next.text) || split_key(next)?.is_some() {
                break;
            }
            pieces.push(next.text.trim().to_string());
            self.pos += 1;
        }
        if pieces.is_empty() {
            return Ok(v);
        }
        // Folded text is always a string, whatever the first line typed as.
        let mut s = match v {
            Value::String(s) => s,
            Value::Null => String::new(),
            other => other.to_string(),
        };
        for p in pieces {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&p);
        }
        Ok(Value::String(s))
    }

    /// `key: |` / `key: >` (or a bare `|`/`>` node). `header` is what follows
    /// the indicator: `-`/`+` chomping and/or an explicit indentation digit.
    /// The body is every RAW line after the header that is blank or indented
    /// deeper than `parent_indent`.
    fn parse_block_scalar(
        &mut self,
        line: &Line,
        parent_indent: usize,
        literal: bool,
        header: &str,
        header_col: usize,
    ) -> Result<Value, YamlError> {
        let mut chomp = Chomp::Clip;
        let mut explicit_indent: Option<usize> = None;
        for ch in header.trim().chars() {
            match ch {
                '-' => chomp = Chomp::Strip,
                '+' => chomp = Chomp::Keep,
                d @ '1'..='9' => {
                    explicit_indent = Some(parent_indent + (d as usize - '0' as usize))
                }
                _ => {
                    return Err(err(
                        line,
                        header_col,
                        format!("bad block scalar header {:?}", header.trim()),
                    ));
                }
            }
        }
        // Collect the raw body.
        let mut body: Vec<&str> = Vec::new();
        let mut idx = line.no; // raw index of the line AFTER the header
        while idx < self.raw.len() {
            let r = self.raw[idx];
            let ind = r.len() - r.trim_start_matches(' ').len();
            if r.trim().is_empty() {
                body.push("");
                idx += 1;
                continue;
            }
            if ind <= parent_indent {
                break;
            }
            body.push(r);
            idx += 1;
        }
        // Skip the logical lines the body consumed.
        while self.peek().is_some_and(|l| l.no <= idx) {
            self.pos += 1;
        }
        // Block indentation: explicit, else the first non-blank body line's.
        let block_indent = explicit_indent.or_else(|| {
            body.iter()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start_matches(' ').len())
        });
        let Some(block_indent) = block_indent else {
            // An empty block scalar.
            return Ok(Value::String(match chomp {
                Chomp::Keep => "\n".repeat(body.len()),
                _ => String::new(),
            }));
        };
        let mut content: Vec<String> = Vec::with_capacity(body.len());
        for (k, l) in body.iter().enumerate() {
            if l.trim().is_empty() {
                content.push(String::new());
                continue;
            }
            let ind = l.len() - l.trim_start_matches(' ').len();
            if ind < block_indent {
                let bad = Line {
                    no: line.no + 1 + k,
                    indent: ind,
                    text: l.trim().to_string(),
                };
                return Err(err(
                    &bad,
                    0,
                    "block scalar line is indented less than the block",
                ));
            }
            content.push(l[block_indent..].to_string());
        }
        // Trailing blank lines are the chomping's business, not the text's.
        let trailing = content.iter().rev().take_while(|l| l.is_empty()).count();
        let text_lines = &content[..content.len() - trailing];
        let mut text = if literal {
            text_lines.join("\n")
        } else {
            fold_lines(text_lines)
        };
        match chomp {
            Chomp::Strip => {}
            Chomp::Clip => {
                if !text.is_empty() {
                    text.push('\n');
                }
            }
            Chomp::Keep => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&"\n".repeat(trailing));
            }
        }
        Ok(Value::String(text))
    }
}

/// Fold (`>`) block-scalar content lines: single line breaks between normal
/// lines become a space; blank lines become newlines; "more-indented" lines
/// (leading spaces beyond the block indent) keep their line breaks.
fn fold_lines(lines: &[String]) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(l);
            continue;
        }
        let prev = &lines[i - 1];
        if l.is_empty() {
            out.push('\n');
        } else if prev.is_empty() {
            out.push_str(l);
        } else if l.starts_with(' ') || prev.starts_with(' ') {
            out.push('\n');
            out.push_str(l);
        } else {
            out.push(' ');
            out.push_str(l);
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

/// A structural line (a mapping entry / sequence item / node start) may not be
/// indented with tabs. Block-scalar bodies never reach this check (they are read
/// raw), so a tab inside literal text is fine.
fn check_tab(line: &Line) -> Result<(), YamlError> {
    if line.text.starts_with('\t') {
        return Err(err(
            line,
            0,
            "tab indentation is not allowed in YAML (use spaces)",
        ));
    }
    Ok(())
}

/// `- item` / `-` — a block sequence entry.
fn is_seq_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// Do the flow brackets in `s` balance (outside quotes)? An empty/unbalanced
/// text says `false`; a text with more closers than openers says `true` (the
/// parser will then report the stray closer).
fn flow_balanced(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
        } else {
            match b {
                b'"' => in_double = true,
                b'\'' => in_single = true,
                b'[' | b'{' => depth += 1,
                b']' | b'}' => depth -= 1,
                _ => {}
            }
        }
        i += 1;
    }
    depth <= 0
}

/// If `line.text` is a `key: rest` mapping entry, return `(key, rest)`. The key
/// is plain (up to the first `: ` / trailing `:` outside quotes) or quoted. A
/// `:` not followed by a space (`http://x`, `12:30`) does not split.
fn split_key(line: &Line) -> Result<Option<(String, &str)>, YamlError> {
    let text = line.text.as_str();
    if text.starts_with('[') || text.starts_with('{') || is_seq_item(text) {
        return Ok(None);
    }
    if text.starts_with("? ") || text == "?" {
        return Err(err(
            line,
            0,
            "complex mapping keys (`? `) are not supported",
        ));
    }
    if text.starts_with('"') || text.starts_with('\'') {
        let (key, after) = parse_quoted(line, text, 0)?;
        let after_trim = after.trim_start();
        if let Some(rest) = after_trim.strip_prefix(':')
            && (rest.is_empty() || rest.starts_with(' '))
        {
            return Ok(Some((key, rest.trim_start_matches(' '))));
        }
        return Ok(None);
    }
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b':' && (i + 1 == bytes.len() || bytes[i + 1] == b' ') {
            let key = text[..i].trim_end();
            if key.is_empty() {
                return Err(err(line, i, "empty mapping key"));
            }
            if key.starts_with('&') || key.starts_with('*') || key.starts_with('!') {
                return Err(err(line, 0, "anchors, aliases and tags are not supported"));
            }
            if key == "<<" {
                return Err(err(line, 0, "merge keys (`<<`) are not supported"));
            }
            let rest = text[i + 1..].trim_start_matches(' ');
            return Ok(Some((key.to_string(), rest)));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Inline values: scalars + flow collections
// ---------------------------------------------------------------------------

/// Parse an inline value at the start of `s` (a slice of the line's text
/// starting at column `col`). Returns the value, the unconsumed remainder, and
/// whether the value was an unquoted PLAIN scalar (which may fold onto the
/// following lines).
fn parse_inline_value<'a>(
    line: &Line,
    s: &'a str,
    col: usize,
) -> Result<(Value, &'a str, bool), YamlError> {
    let t = s.trim_start_matches(' ');
    let col = col + (s.len() - t.len());
    if t.is_empty() {
        return Ok((Value::Null, t, false));
    }
    match t.as_bytes()[0] {
        b'"' | b'\'' => {
            let (v, rest) = parse_quoted(line, t, col)?;
            Ok((Value::String(v), rest, false))
        }
        b'[' => parse_flow_seq(line, t, col).map(|(v, r)| (v, r, false)),
        b'{' => parse_flow_map(line, t, col).map(|(v, r)| (v, r, false)),
        b'&' | b'*' | b'!' => Err(err(
            line,
            col,
            "anchors, aliases and tags are not supported",
        )),
        b'@' | b'`' => Err(err(
            line,
            col,
            "reserved indicator at the start of a plain scalar",
        )),
        _ => Ok((type_plain(t, line, col)?, "", true)),
    }
}

/// Plain scalar inside a flow collection: ends at `,`, `]`, `}` or (in a map,
/// `:` followed by space/punctuation). Returns the raw text and the remainder.
fn take_flow_plain(s: &str, in_map: bool) -> (&str, &str) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b',' || b == b']' || b == b'}' {
            break;
        }
        if in_map
            && b == b':'
            && (i + 1 == bytes.len() || matches!(bytes[i + 1], b' ' | b',' | b'}' | b']'))
        {
            break;
        }
        i += 1;
    }
    (s[..i].trim(), &s[i..])
}

fn parse_flow_seq<'a>(line: &Line, s: &'a str, col: usize) -> Result<(Value, &'a str), YamlError> {
    let mut rest = &s[1..];
    let mut items = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Err(err(line, col, "unterminated flow sequence (missing `]`)"));
        }
        if let Some(r) = rest.strip_prefix(']') {
            return Ok((Value::Array(items), r));
        }
        let (v, r) = parse_flow_element(line, rest, col + (s.len() - rest.len()), false)?;
        items.push(v);
        rest = r.trim_start();
        if let Some(r) = rest.strip_prefix(',') {
            rest = r;
        } else if !rest.starts_with(']') {
            return Err(err(
                line,
                col + (s.len() - rest.len()),
                "expected `,` or `]` in a flow sequence",
            ));
        }
    }
}

fn parse_flow_map<'a>(line: &Line, s: &'a str, col: usize) -> Result<(Value, &'a str), YamlError> {
    let mut rest = &s[1..];
    let mut map = Map::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Err(err(line, col, "unterminated flow mapping (missing `}`)"));
        }
        if let Some(r) = rest.strip_prefix('}') {
            return Ok((Value::Object(map), r));
        }
        let key_col = col + (s.len() - rest.len());
        let (key, r) = if rest.starts_with('"') || rest.starts_with('\'') {
            parse_quoted(line, rest, key_col)?
        } else {
            let (k, r) = take_flow_plain(rest, true);
            if k.is_empty() {
                return Err(err(line, key_col, "empty key in a flow mapping"));
            }
            (k.to_string(), r)
        };
        if map.contains_key(&key) {
            return Err(err(line, key_col, format!("duplicate mapping key {key:?}")));
        }
        rest = r.trim_start();
        let value = if let Some(r) = rest.strip_prefix(':') {
            let (v, r2) =
                parse_flow_element(line, r.trim_start(), col + (s.len() - r.len()), true)?;
            rest = r2;
            v
        } else {
            Value::Null // `{a, b}` — a key with no value is null
        };
        map.insert(key, value);
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix(',') {
            rest = r;
        } else if !rest.starts_with('}') {
            return Err(err(
                line,
                col + (s.len() - rest.len()),
                "expected `,` or `}` in a flow mapping",
            ));
        }
    }
}

/// One element inside a flow collection: a nested collection, a quoted scalar,
/// or a plain scalar terminated by the flow punctuation.
fn parse_flow_element<'a>(
    line: &Line,
    s: &'a str,
    col: usize,
    in_map: bool,
) -> Result<(Value, &'a str), YamlError> {
    match s.as_bytes().first() {
        None => Ok((Value::Null, s)),
        Some(b'[') => parse_flow_seq(line, s, col),
        Some(b'{') => parse_flow_map(line, s, col),
        Some(b'"') | Some(b'\'') => {
            let (v, r) = parse_quoted(line, s, col)?;
            Ok((Value::String(v), r))
        }
        Some(b'&') | Some(b'*') | Some(b'!') => Err(err(
            line,
            col,
            "anchors, aliases and tags are not supported",
        )),
        Some(_) => {
            let (raw, r) = take_flow_plain(s, in_map);
            Ok((type_plain(raw, line, col)?, r))
        }
    }
}

/// Parse a quoted scalar starting at `s[0]` (`"` or `'`). Returns the decoded
/// text and the remainder after the closing quote.
fn parse_quoted<'a>(line: &Line, s: &'a str, col: usize) -> Result<(String, &'a str), YamlError> {
    let quote = s.as_bytes()[0];
    let mut out = String::new();
    let mut chars = s[1..].char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let abs = 1 + i;
        if quote == b'\'' {
            if c == '\'' {
                if let Some(&(_, '\'')) = chars.peek() {
                    chars.next();
                    out.push('\'');
                    continue;
                }
                return Ok((out, &s[abs + 1..]));
            }
            out.push(c);
            continue;
        }
        if c == '"' {
            return Ok((out, &s[abs + 1..]));
        }
        if c == '\\' {
            let Some((_, e)) = chars.next() else {
                return Err(err(
                    line,
                    col + abs,
                    "unterminated escape in a double-quoted scalar",
                ));
            };
            match e {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                'a' => out.push('\u{7}'),
                'b' => out.push('\u{8}'),
                'e' => out.push('\u{1b}'),
                'f' => out.push('\u{c}'),
                'v' => out.push('\u{b}'),
                ' ' => out.push(' '),
                '/' => out.push('/'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'x' | 'u' | 'U' => {
                    let n = match e {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let mut code = 0u32;
                    for _ in 0..n {
                        let Some((_, h)) = chars.next() else {
                            return Err(err(line, col + abs, "truncated \\x/\\u escape"));
                        };
                        let Some(d) = h.to_digit(16) else {
                            return Err(err(line, col + abs, "bad hex digit in \\x/\\u escape"));
                        };
                        code = code * 16 + d;
                    }
                    let Some(ch) = char::from_u32(code) else {
                        return Err(err(line, col + abs, "escape is not a valid unicode scalar"));
                    };
                    out.push(ch);
                }
                other => {
                    return Err(err(
                        line,
                        col + abs,
                        format!("unknown escape `\\{other}` in a double-quoted scalar"),
                    ));
                }
            }
            continue;
        }
        out.push(c);
    }
    Err(err(
        line,
        col,
        "unterminated quoted scalar (multi-line quoted scalars are not supported)",
    ))
}

/// Type a plain scalar per the YAML 1.2 core schema.
fn type_plain(raw: &str, line: &Line, col: usize) -> Result<Value, YamlError> {
    let t = raw.trim();
    if t.is_empty() || t == "~" || t == "null" || t == "Null" || t == "NULL" {
        return Ok(Value::Null);
    }
    match t {
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        _ => {}
    }
    if let Some(n) = parse_int(t) {
        return Ok(n);
    }
    if is_float(t) {
        return match t.parse::<f64>() {
            Ok(f) if f.is_finite() => Number::from_f64(f)
                .map(Value::Number)
                .ok_or_else(|| err(line, col, "float not representable")),
            _ => Err(err(line, col, format!("float {t:?} is out of range"))),
        };
    }
    let lower = t.trim_start_matches(['-', '+']).to_ascii_lowercase();
    if lower == ".inf" || lower == ".nan" {
        return Err(err(
            line,
            col,
            format!("non-finite float {t:?} cannot be represented in JSON"),
        ));
    }
    Ok(Value::String(t.to_string()))
}

/// `[-+]?[0-9]+`, `0x[0-9a-fA-F]+`, `0o[0-7]+` → a JSON number (i64, else u64).
fn parse_int(t: &str) -> Option<Value> {
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, digits) = if let Some(h) = body.strip_prefix("0x") {
        (16, h)
    } else if let Some(o) = body.strip_prefix("0o") {
        (8, o)
    } else {
        (10, body)
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let mag = u64::from_str_radix(digits, radix).ok()?;
    if neg {
        if mag <= i64::MAX as u64 + 1 {
            return Some(Value::from(-(mag as i128) as i64));
        }
        return None;
    }
    Some(Value::from(mag))
}

/// `[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?` — a float candidate
/// (a bare integer is NOT one; `parse_int` runs first).
fn is_float(t: &str) -> bool {
    let s = t.strip_prefix(['-', '+']).unwrap_or(t);
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    if let Some(e) = exp {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        if e.is_empty() || !e.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    let (int, frac) = match mant.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mant, None),
    };
    let int_ok = int.bytes().all(|b| b.is_ascii_digit());
    match frac {
        Some(f) => {
            int_ok && f.bytes().all(|b| b.is_ascii_digit()) && !(int.is_empty() && f.is_empty())
        }
        None => exp.is_some() && !int.is_empty() && int_ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn y(src: &str) -> Value {
        parse(src).unwrap_or_else(|e| panic!("yaml parse failed: {e}\n---\n{src}"))
    }

    fn e(src: &str) -> YamlError {
        parse(src).expect_err("expected a parse error")
    }

    #[test]
    fn empty_and_scalar_documents() {
        assert_eq!(y(""), Value::Null);
        assert_eq!(y("# only a comment\n\n"), Value::Null);
        assert_eq!(y("42"), json!(42));
        assert_eq!(y("--- hello\n"), json!("hello"));
        assert_eq!(
            y("---\nkey: v\n...\nignored: after end\n"),
            json!({"key": "v"})
        );
    }

    #[test]
    fn nested_mappings_and_sequences() {
        let v = y(r#"
model: gpt-5
limits:
  max_steps: 5
  max_depth: 2
mcp_servers:
  - name: fs
    endpoint: https://fs.example/mcp
    tags:
      "*": [untrusted_input, egress]
  - name: q
    endpoint: "https://q.example/mcp"
subscribe:
- queue://inbox
- queue://dead-letter
"#);
        assert_eq!(
            v,
            json!({
                "model": "gpt-5",
                "limits": {"max_steps": 5, "max_depth": 2},
                "mcp_servers": [
                    {"name": "fs", "endpoint": "https://fs.example/mcp",
                     "tags": {"*": ["untrusted_input", "egress"]}},
                    {"name": "q", "endpoint": "https://q.example/mcp"}
                ],
                "subscribe": ["queue://inbox", "queue://dead-letter"]
            })
        );
    }

    #[test]
    fn nested_sequences_and_null_items() {
        assert_eq!(y("- - a\n  - b\n- - c\n"), json!([["a", "b"], ["c"]]));
        assert_eq!(y("-\n  x: 1\n- \n-\n"), json!([{"x": 1}, null, null]));
        assert_eq!(
            y("- x: 1\n  y: 2\n- z: 3\n"),
            json!([{"x": 1, "y": 2}, {"z": 3}])
        );
        // A sequence nested under a key at deeper indent.
        assert_eq!(y("k:\n  - a\n  - b\n"), json!({"k": ["a", "b"]}));
    }

    #[test]
    fn scalar_typing_follows_the_core_schema() {
        let v = y(r#"
n: null
t: ~
e:
b1: true
b2: False
i1: 42
i2: -7
i3: 0x1F
i4: 0o17
f1: 1.5
f2: -2.
f3: .5
f4: 1e3
s1: yes
s2: on
s3: 12:30
s4: 1_000
s5: hello world
s6: 3 apples
big: 18446744073709551615
"#);
        assert_eq!(v["n"], Value::Null);
        assert_eq!(v["t"], Value::Null);
        assert_eq!(v["e"], Value::Null);
        assert_eq!(v["b1"], json!(true));
        assert_eq!(v["b2"], json!(false));
        assert_eq!(v["i1"], json!(42));
        assert_eq!(v["i2"], json!(-7));
        assert_eq!(v["i3"], json!(31));
        assert_eq!(v["i4"], json!(15));
        assert_eq!(v["f1"], json!(1.5));
        assert_eq!(v["f2"], json!(-2.0));
        assert_eq!(v["f3"], json!(0.5));
        assert_eq!(v["f4"], json!(1000.0));
        assert_eq!(v["s1"], json!("yes"));
        assert_eq!(v["s2"], json!("on"));
        assert_eq!(v["s3"], json!("12:30"));
        assert_eq!(v["s4"], json!("1_000"));
        assert_eq!(v["s5"], json!("hello world"));
        assert_eq!(v["s6"], json!("3 apples"));
        assert_eq!(v["big"], json!(18446744073709551615u64));
    }

    #[test]
    fn quoted_scalars_and_escapes() {
        let v = y(r##"
a: "line\nbreak \"q\" \u00e9 \x41"
b: 'it''s # not a comment'
c: "#not a comment"
d: plain # a comment
e: "true"
f: '42'
"##);
        assert_eq!(v["a"], json!("line\nbreak \"q\" é A"));
        assert_eq!(v["b"], json!("it's # not a comment"));
        assert_eq!(v["c"], json!("#not a comment"));
        assert_eq!(v["d"], json!("plain"));
        assert_eq!(v["e"], json!("true"));
        assert_eq!(v["f"], json!("42"));
    }

    #[test]
    fn urls_and_colons_do_not_split_keys() {
        let v = y("endpoint: https://host:8443/mcp\ntime: 12:30\nk: a:b\n");
        assert_eq!(v["endpoint"], json!("https://host:8443/mcp"));
        assert_eq!(v["time"], json!("12:30"));
        assert_eq!(v["k"], json!("a:b"));
    }

    #[test]
    fn flow_collections_inline_and_multiline() {
        let v = y(r#"
a: [1, "two", three, [4, 5], {x: y}]
b: {k: v, n: 2, list: [a, b], "quoted key": 'q', empty}
c: [
  one,
  two,
]
d: []
e: {}
"#);
        assert_eq!(v["a"], json!([1, "two", "three", [4, 5], {"x": "y"}]));
        assert_eq!(
            v["b"],
            json!({"k": "v", "n": 2, "list": ["a", "b"], "quoted key": "q", "empty": null})
        );
        assert_eq!(v["c"], json!(["one", "two"]));
        assert_eq!(v["d"], json!([]));
        assert_eq!(v["e"], json!({}));
    }

    #[test]
    fn block_scalars_literal_and_folded() {
        let v = y("lit: |\n  line one\n  line two\n\n  after blank\nnext: 1\n");
        assert_eq!(v["lit"], json!("line one\nline two\n\nafter blank\n"));
        assert_eq!(v["next"], json!(1));

        let v = y("fold: >\n  a b\n  c d\n\n  e\n");
        assert_eq!(v["fold"], json!("a b c d\ne\n"));

        // Chomping: strip / keep; explicit indentation indicator.
        let v = y("s: |-\n  x\n  y\n\n\nk: |+\n  z\n\n\nafter: 2\n");
        assert_eq!(v["s"], json!("x\ny"));
        assert_eq!(v["k"], json!("z\n\n\n"));
        assert_eq!(v["after"], json!(2));

        let v = y("ind: |2\n    keep two extra\n   one extra\n");
        assert_eq!(v["ind"], json!("  keep two extra\n one extra\n"));

        // `#` and blank lines inside a block scalar are content, not comments.
        let v = y("script: |\n  echo hi # not a comment\n\n  # nor this\n");
        assert_eq!(
            v["script"],
            json!("echo hi # not a comment\n\n# nor this\n")
        );

        // A block scalar as a sequence item.
        let v = y("- |\n  first\n- second\n");
        assert_eq!(v, json!(["first\n", "second"]));

        // Empty block scalar.
        assert_eq!(y("e: |\nnext: 1\n"), json!({"e": "", "next": 1}));
    }

    #[test]
    fn plain_scalars_fold_over_continuation_lines() {
        let v =
            y("instruction: read the report\n  and summarize it\n  in three bullets\nmodel: m\n");
        assert_eq!(
            v["instruction"],
            json!("read the report and summarize it in three bullets")
        );
        assert_eq!(v["model"], json!("m"));
    }

    #[test]
    fn comments_bom_crlf_and_directives() {
        let src =
            "\u{feff}%YAML 1.2\r\n---\r\n# comment\r\nkey: value # trailing\r\n\r\nother: 2\r\n";
        assert_eq!(y(src), json!({"key": "value", "other": 2}));
    }

    #[test]
    fn errors_name_line_and_column() {
        let x = e("a: 1\n\tb: 2\n");
        assert_eq!(x.line, 2);
        assert!(x.msg.contains("tab"), "{x}");

        let x = e("a: 1\na: 2\n");
        assert_eq!(x.line, 2);
        assert!(x.msg.contains("duplicate"), "{x}");

        let x = e("a: &anchor 1\n");
        assert!(x.msg.contains("anchors"), "{x}");
        let x = e("a: *alias\n");
        assert!(x.msg.contains("anchors"), "{x}");
        let x = e("a: !!str 1\n");
        assert!(x.msg.contains("tags") || x.msg.contains("anchors"), "{x}");
        let x = e("<<: {a: 1}\n");
        assert!(x.msg.contains("merge"), "{x}");
        let x = e("? complex\n: key\n");
        assert!(x.msg.contains("complex"), "{x}");

        let x = e("a: 1\n---\nb: 2\n");
        assert!(x.msg.contains("multiple"), "{x}");

        let x = e("a: [1, 2\n");
        assert!(x.msg.contains("unterminated"), "{x}");
        let x = e("a: \"unterminated\n");
        assert!(x.msg.contains("unterminated"), "{x}");

        let x = e("a: .inf\n");
        assert!(x.msg.contains("non-finite"), "{x}");

        let x = e("a:\n  b: 1\n c: 2\n");
        assert_eq!(x.line, 3);
        assert!(x.msg.contains("indentation"), "{x}");

        let x = e("- a\nb: 1\n");
        assert!(x.msg.contains("unexpected content"), "{x}");
    }

    #[test]
    fn json_is_valid_yaml_for_the_flow_subset() {
        // A JSON document is (for our subset) a flow collection — parses the same.
        let src = r#"{"model": "m", "limits": {"max_steps": 3}, "subscribe": ["a", "b"], "flag": true, "n": null}"#;
        let from_yaml = y(src);
        let from_json: Value = serde_json::from_str(src).unwrap();
        assert_eq!(from_yaml, from_json);
    }

    #[test]
    fn inline_values_for_env_and_flags() {
        assert_eq!(parse_inline("12").unwrap(), json!(12));
        assert_eq!(parse_inline(" true ").unwrap(), json!(true));
        assert_eq!(parse_inline("[a, b, 3]").unwrap(), json!(["a", "b", 3]));
        assert_eq!(parse_inline("{k: v}").unwrap(), json!({"k": "v"}));
        assert_eq!(parse_inline("plain text").unwrap(), json!("plain text"));
        assert_eq!(parse_inline("\"12\"").unwrap(), json!("12"));
        assert!(parse_inline("[1, 2").is_err());
        assert!(parse_inline("\"a\" tail").is_err());
    }
}
