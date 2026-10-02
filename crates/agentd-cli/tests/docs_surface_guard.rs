// SPDX-License-Identifier: AGPL-3.0-only
//! **The pages name only the surface agentd serves.**
//!
//! A page that shows a method the listener does not answer, or a URI no card
//! declares, sends its reader to `-32601` or to an extension nothing
//! activates — and nothing on the Rust side notices until a person follows
//! it. This reads every page a reader is pointed at and holds what it names to
//! the tables the listener, the card and the launcher are built from:
//!
//! - a JSON-RPC method a page names is one of the specification's eleven
//!   ([`SpecMethod::ALL`]) or an extension method ([`EXTENSION_METHODS`]) —
//!   MCP's own, read from the mcp crate, aside;
//! - an `https://agentd.dev/a2a/…` or `…/oauth/grant-type/…` URI is one
//!   agentd declares — an extension, the unix binding, the launch grant — or a
//!   file agentd.dev serves under one;
//! - the launcher's section of the interface page names every flag and
//!   override variable of [`LAUNCH_CONTRACT`] and the grant its clients sign
//!   in with;
//! - a node table lists, for each kind, only fields the node catalogue gives
//!   that kind.
//!
//! The check is positive: it asks whether a name is one agentd serves, never
//! whether it is on a list of names that went away. History — `CHANGELOG.md`,
//! `rfcs/`, `docs/design/` — is not scanned; it records what was.

use agentd::engine::model::kind_info;
use agentd::runtime::surface::launch::{LAUNCH_CONTRACT, LAUNCH_GRANT_TYPE};
use agentd::runtime::surface::{EXTENSION_METHODS, Ext, SpecMethod, UNIX_BINDING};
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file under `dir` (relative to the root) whose extension is `ext`,
/// descending into subdirectories when `deep`.
fn files_in(dir: &str, ext: &str, deep: bool, out: &mut Vec<PathBuf>) {
    fn walk(root: &Path, rel: &Path, ext: &str, deep: bool, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(root.join(rel)) else {
            return;
        };
        let mut entries: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| rel.join(e.file_name()))
            .collect();
        entries.sort();
        for e in entries {
            let abs = root.join(&e);
            let name = e.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if abs.is_dir() {
                if deep && !matches!(name, "node_modules" | "dist" | "target" | "__pycache__") {
                    walk(root, &e, ext, deep, out);
                }
            } else if e.extension().is_some_and(|x| x == ext) {
                out.push(e);
            }
        }
    }
    let root = workspace_root();
    assert!(
        root.join(dir).is_dir(),
        "{dir} is gone — update the scan set"
    );
    walk(&root, Path::new(dir), ext, deep, out);
}

/// The pages a reader is sent to: the docs and the extension specs, the
/// repository's and each crate's front page, the install script, the Agent
/// Skill, the site's text and pages, the client's README and the examples'.
fn scan_set() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    files_in("docs", "md", false, &mut files);
    files_in("docs/ext", "md", false, &mut files);
    for f in [
        "README.md",
        "SECURITY.md",
        "CONFORMANCE.md",
        "install.sh",
        "interface/README.md",
    ] {
        assert!(root.join(f).is_file(), "{f} is gone — update the scan set");
        files.push(PathBuf::from(f));
    }
    for krate in std::fs::read_dir(root.join("crates")).unwrap().flatten() {
        let readme = Path::new("crates")
            .join(krate.file_name())
            .join("README.md");
        if root.join(&readme).is_file() {
            files.push(readme);
        }
    }
    files_in("skills", "md", true, &mut files);
    files_in("web/public", "txt", false, &mut files);
    files_in("web/app", "jsx", true, &mut files);
    files_in("examples", "md", true, &mut files);
    assert!(
        files.len() > 50,
        "the scan must actually see the pages: {} files",
        files.len()
    );
    files
}

/// Each scanned file's lines, numbered from 1.
fn scanned_lines() -> Vec<(String, usize, String)> {
    let root = workspace_root();
    let mut out = Vec::new();
    for rel in scan_set() {
        let text = std::fs::read_to_string(root.join(&rel))
            .unwrap_or_else(|e| panic!("{}: {e}", rel.display()));
        for (n, line) in text.lines().enumerate() {
            out.push((rel.display().to_string(), n + 1, line.to_string()));
        }
    }
    out
}

// ---- methods ----------------------------------------------------------------

/// Every JSON-RPC method the listener answers.
fn answered() -> Vec<&'static str> {
    SpecMethod::ALL
        .iter()
        .map(|m| m.name())
        .chain(EXTENSION_METHODS.iter().map(|(m, ..)| *m))
        .collect()
}

/// The verbs A2A's method names begin with — and the ones a made-up method
/// would. A backticked `VerbNoun` is how a page names a method; a single
/// word (`Cancel`, `Ping`) is some other protocol's message.
const METHOD_VERBS: &[&str] = &[
    "Get",
    "Send",
    "List",
    "Cancel",
    "Subscribe",
    "Resubscribe",
    "Create",
    "Delete",
    "Set",
    "Update",
    "Pair",
];

/// PascalCase of two humps or more that starts with a method verb.
fn method_like(token: &str) -> bool {
    let humps = token.chars().filter(char::is_ascii_uppercase).count();
    token.chars().all(|c| c.is_ascii_alphanumeric())
        && token.starts_with(|c: char| c.is_ascii_uppercase())
        && humps >= 2
        && METHOD_VERBS.iter().any(|v| {
            token
                .strip_prefix(v)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
        })
}

/// MCP's method and notification names, read from the constants the mcp
/// crate speaks them by: a page shows them in MCP examples, and they are not
/// the listener's to answer.
fn mcp_methods() -> &'static [String] {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let src = std::fs::read_to_string(workspace_root().join("crates/mcp/src/wire.rs")).unwrap();
        let (_, module) = src
            .split_once("pub mod method {")
            .expect("crates/mcp/src/wire.rs keeps its method names in `pub mod method`");
        let module = module.split("\n}").next().unwrap();
        let names: Vec<String> = module
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub const "))
            .filter_map(|l| l.split_once("&str = \"")?.1.split_once('"'))
            .map(|(name, _)| name.to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "tools/call") && names.len() > 10,
            "the mcp crate's method names were not found: {names:?}"
        );
        names
    })
}

/// A value of a `method` key that is a JSON-RPC method name: PascalCase (an
/// uppercase letter, then a lowercase one — `POST` is an HTTP verb), an
/// `agentd.` namespace, or a lowercase `ns/method` path — A2A 0.x's
/// `message/send` shape — that is not one of MCP's.
fn rpc_method_value(value: &str) -> bool {
    let mut chars = value.chars();
    let pascal = chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let path = value.starts_with(|c: char| c.is_ascii_lowercase())
        && value.contains('/')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-'));
    pascal || value.starts_with("agentd.") || (path && !mcp_methods().iter().any(|m| m == value))
}

/// The method names a line uses: each backticked method-like token, each
/// value of a `method` key that names a JSON-RPC method ([`rpc_method_value`])
/// and each `agentd.<namespace>/<Method>` token.
fn method_names(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Backticked spans.
    let mut parts = line.split('`');
    parts.next();
    while let (Some(inside), rest) = (parts.next(), parts.next()) {
        if method_like(inside) {
            out.push(inside.to_string());
        }
        if rest.is_none() {
            break;
        }
    }
    // `"method": "X"` in JSON, `method: "X"` in a script.
    for key in ["\"method\"", "method"] {
        let mut from = 0;
        while let Some(i) = line[from..].find(key) {
            let at = from + i + key.len();
            from = at;
            let rest = line[at..].trim_start();
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(q) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
                continue;
            };
            let Some(value) = rest[1..].split(q).next() else {
                continue;
            };
            if rpc_method_value(value) {
                out.push(value.to_string());
            }
        }
    }
    // `agentd.<ns>/<Method>` — `agentd.dev/…` is the site, not a namespace.
    let mut from = 0;
    while let Some(i) = line[from..].find("agentd.") {
        let at = from + i;
        from = at + "agentd.".len();
        if line[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '/')
        {
            continue;
        }
        let tail = &line[from..];
        let ns: String = tail
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')
            .collect();
        if ns.is_empty() || ns == "dev" || !tail[ns.len()..].starts_with('/') {
            continue;
        }
        let method: String = tail[ns.len() + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !method.is_empty() {
            out.push(format!("agentd.{ns}/{method}"));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn docs_name_only_methods_agentd_answers() {
    let answered = answered();
    let mut found = Vec::new();
    for (file, n, line) in scanned_lines() {
        for m in method_names(&line) {
            if !answered.contains(&m.as_str()) {
                found.push(format!("{file}:{n}: {m}"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "pages name methods the listener does not answer (it answers {}):\n{}",
        answered.join(", "),
        found.join("\n")
    );
}

// ---- URIs -------------------------------------------------------------------

/// The URI namespaces agentd names its own protocol identifiers under.
const OWNED_URI_PREFIXES: &[&str] = &[
    "https://agentd.dev/a2a/ext/",
    "https://agentd.dev/a2a/binding/",
    "https://agentd.dev/oauth/grant-type/",
];

/// Each URI under [`OWNED_URI_PREFIXES`] the line names. A bare prefix names
/// the namespace, not a URI in it, and is not returned.
fn owned_uris(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for prefix in OWNED_URI_PREFIXES {
        let mut from = 0;
        while let Some(i) = line[from..].find(prefix) {
            let at = from + i;
            let tail = &line[at..];
            let end = tail
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(
                            c,
                            '"' | '\'' | '`' | ')' | '(' | '>' | '<' | ',' | ']' | '#' | '?' | '\\'
                        )
                })
                .unwrap_or(tail.len());
            // Sentence punctuation after a URL is not part of it.
            let uri = tail[..end].trim_end_matches(['.', ',', ';', ':']);
            if uri.len() > prefix.len() {
                out.push(uri.to_string());
            }
            from = at + prefix.len();
        }
    }
    out
}

/// Every protocol URI agentd declares: its extensions, its unix binding and
/// its launch grant.
fn declared_uris() -> Vec<&'static str> {
    Ext::ALL
        .iter()
        .map(|e| e.uri())
        .chain([UNIX_BINDING, LAUNCH_GRANT_TYPE])
        .collect()
}

/// Whether `uri` is a declared URI, or a file agentd.dev publishes under one
/// (the schema bundle and golden examples beside each extension's spec).
///
/// Byte for byte: a peer's negotiation compares URIs exactly, so a trailing
/// slash names an extension nothing activates.
fn is_declared(uri: &str, declared: &[&str]) -> bool {
    if declared.contains(&uri) {
        return true;
    }
    declared.iter().any(|d| {
        uri.strip_prefix(d)
            .is_some_and(|rest| rest.starts_with('/'))
            && uri
                .strip_prefix("https://agentd.dev/")
                .is_some_and(|path| workspace_root().join("web/public").join(path).is_file())
    })
}

#[test]
fn every_agentd_uri_named_is_one_agentd_declares() {
    let declared = declared_uris();
    let mut found = Vec::new();
    let mut seen = 0;
    for (file, n, line) in scanned_lines() {
        for uri in owned_uris(&line) {
            seen += 1;
            if !is_declared(&uri, &declared) {
                found.push(format!("{file}:{n}: {uri}"));
            }
        }
    }
    assert!(seen > 0, "the pages name none of agentd's URIs at all");
    assert!(
        found.is_empty(),
        "pages name agentd URIs no card or grant declares (declared: {}):\n{}",
        declared.join(", "),
        found.join("\n")
    );
}

// ---- the launcher -----------------------------------------------------------

/// A heading's anchor as the site and GitHub compute it: lowercase, spaces to
/// hyphens, everything but letters, digits, `-` and `_` dropped.
fn slugify(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// The section of `text` under the heading whose anchor is `slug`: every line
/// up to the next heading of the same or a higher level. Headings inside a
/// fenced block are sample output, not structure.
fn section(text: &str, slug: &str) -> Option<String> {
    let mut fenced = false;
    let mut level = None;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let heading = (!fenced)
            .then(|| {
                let hashes = line.chars().take_while(|c| *c == '#').count();
                (hashes > 0 && line[hashes..].starts_with(' ')).then(|| (hashes, &line[hashes..]))
            })
            .flatten();
        match (level, heading) {
            (None, Some((h, title))) if slugify(title) == slug => level = Some(h),
            (Some(l), Some((h, _))) if h <= l => break,
            (Some(_), _) => out.push(line),
            _ => {}
        }
    }
    level.map(|_| out.join("\n"))
}

#[test]
fn the_launcher_contract_is_documented() {
    // `LAUNCHER_DOCS` is where a missing client sends the operator.
    let anchor = agentd::runtime::surface::launch::LAUNCHER_DOCS
        .rsplit_once('#')
        .map(|(_, a)| a)
        .expect("LAUNCHER_DOCS names an anchor");
    let page = std::fs::read_to_string(workspace_root().join("docs/interface.md")).unwrap();
    let launcher = section(&page, anchor)
        .unwrap_or_else(|| panic!("docs/interface.md has no heading whose anchor is #{anchor}"));
    let mut missing = Vec::new();
    for client in LAUNCH_CONTRACT {
        for name in
            client
                .argv
                .iter()
                .copied()
                .chain([client.bin, client.bin_env, client.client_id])
        {
            if !launcher.contains(name) {
                missing.push(format!("agentd {}: {name}", client.sub));
            }
        }
    }
    if !launcher.contains(LAUNCH_GRANT_TYPE) {
        missing.push(format!("the launch grant type {LAUNCH_GRANT_TYPE}"));
    }
    assert!(
        missing.is_empty(),
        "docs/interface.md#{anchor} does not document what the launcher hands its clients:\n{}",
        missing.join("\n")
    );

    // The launcher is a kept surface: no page may say otherwise, or send a
    // reader to the flag it replaced.
    let mut found = Vec::new();
    for (file, n, line) in scanned_lines() {
        if says_launcher_gone(&line) {
            found.push(format!("{file}:{n}: says the launcher is gone: {line}"));
        }
        if line.contains("--spawn") {
            found.push(format!("{file}:{n}: --spawn: {line}"));
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// Whether a line names `agentd tui` or `agentd ui` and, in the same line,
/// says it went away — in any of the ways a page words that.
fn says_launcher_gone(line: &str) -> bool {
    let lower = line.to_lowercase();
    let names_launcher = ["agentd tui", "agentd ui"].iter().any(|l| {
        lower.match_indices(l).any(|(at, _)| {
            !lower[at + l.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
        })
    });
    names_launcher
        && ["removed", "deprecated", "no longer", "gone", "replaced by"]
            .iter()
            .any(|claim| lower.contains(claim))
}

// ---- node fields ------------------------------------------------------------

/// The cells of a markdown table row, or `None` when the line is not one. An
/// escaped `\|` (a value list such as `ensure\|always`) stays in its cell.
fn table_cells(line: &str) -> Option<Vec<String>> {
    let row = line.trim().strip_prefix('|')?.strip_suffix('|')?;
    let mut cells = vec![String::new()];
    let mut escaped = false;
    for c in row.chars() {
        if c == '|' && !escaped {
            cells.push(String::new());
        } else {
            cells.last_mut().unwrap().push(c);
        }
        escaped = c == '\\';
    }
    Some(cells.into_iter().map(|c| c.trim().to_string()).collect())
}

/// The field names a node-table cell lists: each backticked name, read up to a
/// `:` (`policy: ensure` names `policy`). Parentheses hold values and asides —
/// `from` (`new` \| `earliest`), (needs `--features cron`) — not fields, so
/// they are dropped first; and what is not a bare snake_case name, such as
/// `workflow.finished`, is a value too.
fn listed_fields(cell: &str) -> Vec<String> {
    listed_names(cell)
        .into_iter()
        .map(|t| t.split(':').next().unwrap_or("").trim().to_string())
        .filter(|t| {
            t.starts_with(|c: char| c.is_ascii_lowercase())
                && t.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .collect()
}

/// Every backticked name in a cell, outside parentheses.
fn listed_names(cell: &str) -> Vec<String> {
    let mut outside = String::new();
    let (mut depth, mut in_tick) = (0usize, false);
    for c in cell.chars() {
        match c {
            '`' => in_tick = !in_tick,
            '(' if !in_tick => depth += 1,
            ')' if !in_tick && depth > 0 => {
                depth -= 1;
                continue;
            }
            _ => {}
        }
        if depth == 0 {
            outside.push(c);
        }
    }
    outside
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The kinds a node-table row's first cell names. One row may stand for a
/// family — `memory.get` / `.set` / `.push`, `noop` (no fields) / `checkpoint`
/// — so asides in parentheses are dropped and a name starting with `.` takes
/// the family of the name before it (`.set` after `memory.get` is
/// `memory.set`). The names are returned as written out; whether each is a
/// kind is the caller's question.
fn kinds_in_cell(cell: &str) -> Vec<String> {
    let mut family = String::new();
    listed_names(cell)
        .into_iter()
        .map(|name| {
            let full = match name.strip_prefix('.') {
                Some(member) => format!("{family}.{member}"),
                None => name,
            };
            family = full.split('.').next().unwrap_or("").to_string();
            full
        })
        .collect()
}

/// A node table — one whose first column is a kind — lists, in its field
/// columns, only fields the catalogue gives the kinds of that row. The
/// catalogue ([`KINDS`](agentd::engine::model::KINDS)) is what the loader
/// accepts, so a field a page offers and the catalogue lacks is one a reader
/// copies into a document the loader then refuses; and a field the catalogue
/// kept only for a page to describe is one that parses and does nothing. A row
/// that names a family of kinds is held to what the family takes together; a
/// row in such a table that names no kind the catalogue knows is itself a
/// finding, because skipping it would leave its fields unread. Prose outside a
/// node table, and a kind table with no field column, are not read.
#[test]
fn node_tables_name_only_fields_the_kind_accepts() {
    let mut found = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut rows = 0;
    // The header of the table the previous line belongs to, if any.
    let mut header: Option<(String, Vec<String>)> = None;
    for (file, n, line) in scanned_lines() {
        let Some(cells) = table_cells(&line) else {
            header = None;
            continue;
        };
        let Some((_, head)) = header.as_ref().filter(|(f, _)| *f == file) else {
            header = Some((file, cells));
            continue;
        };
        let is_field_column = |title: &String| {
            let title = title.to_lowercase();
            ["field", "required", "option"]
                .iter()
                .any(|w| title.contains(w))
        };
        if !head[0].to_lowercase().contains("kind")
            || !head.iter().skip(1).any(is_field_column)
            || cells
                .iter()
                .all(|c| c.chars().all(|c| matches!(c, '-' | ':')))
        {
            continue;
        }
        let names = kinds_in_cell(&cells[0]);
        let infos: Vec<_> = names.iter().filter_map(|k| kind_info(k)).collect();
        if names.is_empty() || infos.len() != names.len() {
            found.push(format!(
                "{file}:{n}: a node-table row must name kinds the catalogue knows, got {}",
                cells[0]
            ));
            continue;
        }
        rows += 1;
        seen.insert(file.clone());
        let mut takes: Vec<&str> = infos
            .iter()
            .flat_map(|i| i.fields.iter().copied())
            .collect();
        takes.sort_unstable();
        takes.dedup();
        let named = infos.iter().map(|i| i.name).collect::<Vec<_>>().join(" / ");
        for (title, cell) in head.iter().zip(&cells).skip(1) {
            if !is_field_column(title) {
                continue;
            }
            for f in listed_fields(cell) {
                if !takes.contains(&f.as_str()) {
                    found.push(format!(
                        "{file}:{n}: `{named}` has no field `{f}` (it takes: {})",
                        takes.join(", ")
                    ));
                }
            }
        }
    }
    // The scan must reach the catalogue's own pages, or it proves nothing.
    for page in [
        "docs/node-registry.md",
        "docs/workflows.md",
        "docs/configuration.md",
        "web/public/llms.txt",
    ] {
        assert!(seen.contains(page), "no node table read in {page}");
    }
    assert!(rows > 100, "only {rows} node-table rows read");
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// The matchers themselves: each shape is caught, and what merely resembles
/// one is not.
#[test]
fn the_matchers_find_each_shape_and_nothing_else() {
    for (line, want) in [
        ("call `GetAgentCard` first", "GetAgentCard"),
        ("then `SubscribeToEvents`", "SubscribeToEvents"),
        (r#"{"jsonrpc":"2.0","method":"Pair","params":{}}"#, "Pair"),
        (r#"  "method": "agentd.feed/Watch","#, "agentd.feed/Watch"),
        ("const m = { method: 'ListEverything' };", "ListEverything"),
        ("the agentd.interface/Pair method", "agentd.interface/Pair"),
        (r#"{"method":"message/send","params":{}}"#, "message/send"),
        (r#"  "method": "tasks/resubscribe","#, "tasks/resubscribe"),
        (
            r#"{"method": "agent/getAuthenticatedExtendedCard"}"#,
            "agent/getAuthenticatedExtendedCard",
        ),
    ] {
        assert!(
            method_names(line).contains(&want.to_string()),
            "missed {want} in {line}: {:?}",
            method_names(line)
        );
    }
    for clean in [
        "control messages (`Cancel`, `Ping`, `ToolResult`)",
        "see https://agentd.dev/a2a/ext/events",
        r#"{"method": "tools/call"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/resources/updated"}"#,
        r#"{"method": "POST"}"#,
        "  method: \"GET\"",
        "a `DataPart` on a `Task`",
        "`SendMessage` and `agentd.events/SubscribeToEvents`",
    ] {
        let names = method_names(clean);
        let answered = answered();
        assert!(
            names.iter().all(|n| answered.contains(&n.as_str())),
            "false hit on {clean}: {names:?}"
        );
    }

    let declared = declared_uris();
    for bad in [
        "`https://agentd.dev/a2a/ext/command/next`",
        "https://agentd.dev/a2a/ext/interface.",
        "(https://agentd.dev/oauth/grant-type/device)",
        "https://agentd.dev/a2a/ext/events/schema-1.json",
        "`https://agentd.dev/a2a/ext/command/`",
    ] {
        let uris = owned_uris(bad);
        assert!(
            !uris.is_empty() && uris.iter().any(|u| !is_declared(u, &declared)),
            "missed {bad}: {uris:?}"
        );
    }
    for good in [
        "the [command](https://agentd.dev/a2a/ext/command) extension.",
        "https://agentd.dev/a2a/ext/command/schema.json",
        "`https://agentd.dev/a2a/binding/jsonrpc-unix`",
        "// https://agentd.dev/a2a/ext/<name>/ — a spec page",
    ] {
        let uris = owned_uris(good);
        assert!(
            uris.iter().all(|u| is_declared(u, &declared)),
            "false hit on {good}: {uris:?}"
        );
    }

    for gone in [
        "`agentd tui` was removed in 2.0",
        "`agentd tui` has been removed",
        "`agentd ui` is removed",
        "agentd tui is gone",
        "agentd ui no longer exists",
        "agentd tui is deprecated",
        "`agentd ui` was replaced by `agentd-ui`",
    ] {
        assert!(says_launcher_gone(gone), "missed: {gone}");
    }
    for kept in [
        "`agentd tui` starts the daemon and the terminal client",
        "the removed `--spawn` flag",
        "`agentd-ui` is no longer bundled",
        "agentd uinput is gone",
    ] {
        assert!(!says_launcher_gone(kept), "false hit: {kept}");
    }

    let page = "# T\n## Launcher\nx\n```\n# sample\n```\n### Sub\ny\n## Next\nz\n";
    assert_eq!(
        section(page, "launcher").as_deref(),
        Some("x\n```\n# sample\n```\n### Sub\ny")
    );
    assert_eq!(
        slugify(" Fullscreen (default) vs `--inline`"),
        "fullscreen-default-vs---inline"
    );

    assert_eq!(
        table_cells("| `once` | `policy: ensure\\|always` |").unwrap(),
        ["`once`", "`policy: ensure\\|always`"]
    );
    assert_eq!(table_cells("`once` | not a row"), None);
    for (cell, want) in [
        (
            "**`server`**, **`uri`**, `debounce_ms`, `filter`",
            &["server", "uri", "debounce_ms", "filter"][..],
        ),
        (
            "`cron: \"0 2 * * *\"` (needs `--features cron`), or `every: 1h`",
            &["cron", "every"][..],
        ),
        (
            "`stream` (required), `from` (`new` \\| `earliest`)",
            &["stream", "from"][..],
        ),
        ("**`on`**: `workflow.finished`, `human.asked`", &["on"][..]),
        (
            "`cron` `every` `at` `inputs`",
            &["cron", "every", "at", "inputs"][..],
        ),
    ] {
        assert_eq!(listed_fields(cell), want, "in {cell}");
    }
    for (cell, want) in [
        ("`once`", &["once"][..]),
        (
            "`memory.get` / `.set` / `.pop`",
            &["memory.get", "memory.set", "memory.pop"][..],
        ),
        (
            "`knowledge.search` / `.get`, `search.query` / `.fetch`",
            &[
                "knowledge.search",
                "knowledge.get",
                "search.query",
                "search.fetch",
            ][..],
        ),
        (
            "`noop` (no fields) / `checkpoint`",
            &["noop", "checkpoint"][..],
        ),
        ("Kind", &[][..]),
    ] {
        assert_eq!(kinds_in_cell(cell), want, "in {cell}");
    }
}
