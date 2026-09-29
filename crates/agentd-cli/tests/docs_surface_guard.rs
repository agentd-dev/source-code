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
//!   ([`SpecMethod::ALL`]) or an extension method ([`EXTENSION_METHODS`]);
//! - an `https://agentd.dev/a2a/…` or `…/oauth/grant-type/…` URI is one
//!   agentd declares — an extension, the unix binding, the launch grant — or a
//!   file agentd.dev serves under one;
//! - the launcher's section of the interface page names every flag and
//!   override variable of [`LAUNCH_CONTRACT`] and the grant its clients sign
//!   in with.
//!
//! The check is positive: it asks whether a name is one agentd serves, never
//! whether it is on a list of names that went away. History — `CHANGELOG.md`,
//! `rfcs/`, `docs/design/` — is not scanned; it records what was.

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

/// The method names a line uses: each backticked method-like token, each
/// value of a `method` key that is PascalCase or `agentd.`-namespaced (MCP's
/// `tools/call` and friends are lowercase and not the listener's), and each
/// `agentd.<namespace>/<Method>` token.
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
            if value.starts_with(|c: char| c.is_ascii_uppercase()) || value.starts_with("agentd.") {
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
fn is_declared(uri: &str, declared: &[&str]) -> bool {
    let uri = uri.trim_end_matches('/');
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
        let lower = line.to_lowercase();
        let names_launcher = lower.contains("agentd tui") || lower.contains("agentd ui");
        if names_launcher && (lower.contains("was removed") || lower.contains("deprecated")) {
            found.push(format!("{file}:{n}: says the launcher is gone: {line}"));
        }
        if line.contains("--spawn") {
            found.push(format!("{file}:{n}: --spawn: {line}"));
        }
    }
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

    let page = "# T\n## Launcher\nx\n```\n# sample\n```\n### Sub\ny\n## Next\nz\n";
    assert_eq!(
        section(page, "launcher").as_deref(),
        Some("x\n```\n# sample\n```\n### Sub\ny")
    );
    assert_eq!(
        slugify(" Fullscreen (default) vs `--inline`"),
        "fullscreen-default-vs---inline"
    );
}
