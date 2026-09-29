// SPDX-License-Identifier: AGPL-3.0-only
//! **No identifier agentd owns carries a version.**
//!
//! An extension URI, a grant type, a schema URL, a config key, an envelope, a
//! module name: each is agentd's own, and a version in it is a second name for
//! the same thing that sooner or later gets a first name beside it — and then
//! both are answered, forever. An incompatible change takes a NEW name
//! instead. Only versions an external specification requires stay (A2A's
//! `A2A-Version: 1.0`, an OCI media type's `.v1`, a cgroup or HTTP version),
//! and none of those is spelled the way this scan looks for.
//!
//! The scan is by spelling, not by meaning, so it is hand-written and exact:
//! an `agentd.dev` path with a `v<N>` segment or a `<name>-<N>.json` schema,
//! one of agentd's extension or grant names followed by `/v<N>`, and the
//! retired version-carrying identifiers by name. It lists every hit as
//! `file:line`.
#![cfg(unix)]

use std::path::{Path, PathBuf};

/// Where agentd's own identifiers live, relative to the workspace root: the
/// code, and every page and client that names them to a reader.
///
/// History — `CHANGELOG.md`, the RFCs before 0043 and their index,
/// `docs/design/` — is never scanned: it records what was, versions included.
const ROOTS: &[&str] = &[
    "crates",
    "examples",
    "scripts",
    "bench",
    ".github",
    "packaging",
    "web/public/schema",
    "web/public/a2a",
    "web/public/llms.txt",
    "web/lib",
    "web/app",
    "contrib/schemastore-catalog-entry.json",
    "Dockerfile",
    "install.sh",
    "interface",
    "docs",
    "skills",
    "README.md",
    "SECURITY.md",
    "CONFORMANCE.md",
    "rfcs/0043-a2a-boundary-and-device-authorization.md",
    "rfcs/0044-enterprise-managed-authorization.md",
];

/// Trees under the roots that are not agentd's to spell: build output,
/// dependencies, the design notes (history, like the older RFCs), and the
/// vendored upstream Instruction Specification corpus, which is checked
/// against the published one byte for byte.
const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "docs/design",
    "crates/agentd-cli/tests/instruction-spec-corpus",
    "crates/instruction/tests",
];

/// The names agentd gives its extensions, its unix binding and its launch
/// grant — each of which once carried a `/v<N>` suffix.
const OWNED_NAMES: &[&str] = &[
    "command",
    "events",
    "task-annotations",
    "interface",
    "jsonrpc-unix",
    "launch",
];

/// Identifiers that existed only to carry a version, refused by name.
const RETIRED_TOKENS: &[&str] = &[
    "config_version",
    "x-agentd-contract-version",
    "CONFIG_VERSION",
    "SCHEMA_CONTRACT_VERSION",
    "ENVELOPE_VERSION",
    "METRICS_SCHEMA",
    "metrics_schema",
    "EVENTS_SCHEMA",
    "EXIT_CODES",
    "DIALECT",
    "config::v2",
    "config/v2/",
    "run_v2",
    "runtime_v2",
    "engine_v3",
    "vnd.instruction.document.v1",
    "--config-schema=",
    "\"runtime\": \"1\"",
    "\"runtime\":\"1\"",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `v` followed by one or more digits and nothing else.
fn is_version_segment(seg: &str) -> bool {
    seg.strip_prefix('v')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Each `agentd.dev/…` path on the line that carries a version: a `v<N>`
/// segment anywhere, or a schema published as `<name>-<N>.json`.
fn versioned_agentd_paths(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(i) = rest.find("agentd.dev/") {
        let tail = &rest[i + "agentd.dev/".len()..];
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
        let path = tail[..end].trim_end_matches(['.', ',', ';', ':']);
        let segs: Vec<&str> = path.split('/').collect();
        let schema_versioned = segs.first() == Some(&"schema")
            && segs.last().is_some_and(|f| {
                f.strip_suffix(".json")
                    .and_then(|stem| stem.rsplit_once('-'))
                    .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            });
        if segs.iter().any(|s| is_version_segment(s)) || schema_versioned {
            out.push(format!("agentd.dev/{path}"));
        }
        rest = &tail[end..];
    }
    out
}

/// `<name>/v<N>` for one of agentd's own names, the name standing alone.
fn versioned_owned_names(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for name in OWNED_NAMES {
        let needle = format!("{name}/v");
        let mut from = 0;
        while let Some(i) = line[from..].find(&needle) {
            let at = from + i;
            let before = line[..at].chars().next_back();
            let digits: String = line[at + needle.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() && !before.is_some_and(|c| is_word(c) || c == '-') {
                out.push(format!("{name}/v{digits}"));
            }
            from = at + needle.len();
        }
    }
    out
}

/// The retired tokens on the line, each standing alone: a token that begins
/// or ends with a word character must not continue a longer word there.
fn retired_tokens(line: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    for token in RETIRED_TOKENS {
        let mut from = 0;
        while let Some(i) = line[from..].find(token) {
            let at = from + i;
            let end = at + token.len();
            let starts_word = token.starts_with(is_word);
            let ends_word = token.ends_with(is_word);
            let before_ok = !starts_word || !line[..at].chars().next_back().is_some_and(is_word);
            let after_ok = !ends_word || !line[end..].chars().next().is_some_and(is_word);
            if before_ok && after_ok {
                out.push(*token);
                break;
            }
            from = end;
        }
    }
    out
}

/// Each `AGENT_<X>` variable the line names. `AGENTD_*` is the only prefix
/// agentd reads; the older `AGENT_*` spelling was a second name for every
/// variable, and a page that still offers it tells a reader to set something
/// nothing reads. Checked in prose only: the code and its tests name those
/// variables on purpose, to prove they are ignored.
fn legacy_env_prefix(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = line[from..].find("AGENT_") {
        let at = from + i;
        let end = at + "AGENT_".len();
        let before = line[..at].chars().next_back();
        if !before.is_some_and(is_word)
            && line[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
        {
            let name: String = line[at..].chars().take_while(|c| is_word(*c)).collect();
            out.push(format!("{name} (agentd reads AGENTD_* only)"));
        }
        from = end;
    }
    out
}

/// A page a person reads, rather than code: where [`legacy_env_prefix`]
/// applies.
fn is_prose(rel: &Path) -> bool {
    rel == Path::new("install.sh")
        || rel
            .extension()
            .is_some_and(|e| matches!(e.to_str(), Some("md" | "mdx" | "txt" | "jsx")))
}

/// Every hit on one line, described.
fn hits(line: &str) -> Vec<String> {
    let mut out = versioned_agentd_paths(line);
    out.extend(versioned_owned_names(line));
    out.extend(retired_tokens(line).into_iter().map(str::to_string));
    out
}

fn skipped(rel: &Path) -> bool {
    let rel_str = rel.to_string_lossy();
    rel.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("target" | "node_modules" | "dist")
        )
    }) || SKIP_DIRS.iter().any(|d| rel_str.starts_with(d))
        || rel == Path::new("crates/agentd-cli/tests/unversioned_guard.rs")
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let abs = root.join(rel);
    if skipped(rel) {
        return;
    }
    if abs.is_dir() {
        let Ok(rd) = std::fs::read_dir(&abs) else {
            return;
        };
        let mut entries: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| rel.join(e.file_name()))
            .collect();
        entries.sort();
        for e in entries {
            walk(root, &e, out);
        }
    } else if abs.is_file() {
        out.push(rel.to_path_buf());
    }
}

#[test]
fn no_agentd_identifier_carries_a_version() {
    let root = workspace_root();
    let mut files = Vec::new();
    for r in ROOTS {
        assert!(root.join(r).exists(), "{r} is gone — update ROOTS");
        walk(&root, Path::new(r), &mut files);
    }
    assert!(
        files.len() > 300,
        "the scan must actually see the tree: {} files",
        files.len()
    );
    let mut found = Vec::new();
    for rel in &files {
        // Binary files carry no identifiers worth reading.
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        let prose = is_prose(rel);
        for (n, line) in text.lines().enumerate() {
            let mut line_hits = hits(line);
            if prose {
                line_hits.extend(legacy_env_prefix(line));
            }
            for h in line_hits {
                found.push(format!("{}:{}: {h}", rel.display(), n + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "agentd-owned identifiers carrying a version (give an incompatible \
         change a new name instead):\n{}",
        found.join("\n")
    );
}

/// The matchers themselves: each shape is caught, and what merely resembles
/// one is not.
#[test]
fn the_scan_finds_each_shape_and_nothing_else() {
    for caught in [
        "\"https://agentd.dev/a2a/ext/command/v2\"",
        "see https://agentd.dev/schema/config-1.json.",
        "`https://agentd.dev/oauth/grant-type/launch/v1`",
        "the command/v2 envelope",
        "task-annotations/v1 facts",
        "config_version: \"1\"",
        "use crate::config::v2::Settings;",
        "  \"runtime\": \"1\",",
        "pub const ENVELOPE_VERSION: u32 = 2;",
        "--config-schema=1",
    ] {
        assert!(!hits(caught).is_empty(), "missed: {caught}");
    }
    for caught in ["set `AGENT_MODEL` instead", "AGENT_INTELLIGENCE_TOKEN=…"] {
        assert!(!legacy_env_prefix(caught).is_empty(), "missed: {caught}");
    }
    for clean in [
        "AGENTD_MODEL",
        "SUBAGENT_ENV",
        "AGENT_",
        "the AGENT_md file",
    ] {
        assert!(
            legacy_env_prefix(clean).is_empty(),
            "false hit on {clean}: {:?}",
            legacy_env_prefix(clean)
        );
    }
    for clean in [
        "\"https://agentd.dev/a2a/ext/command\"",
        "https://agentd.dev/schema/config.json",
        "https://agentd.dev/docs/interface#launcher",
        "subcommand/v2 is not ours",
        "anthropic.claude-3-5-sonnet-20241022-v2:0",
        "/v2/repo/manifests/latest",
        "A2A-Version: 1.0",
        "application/vnd.oci.image.manifest.v1+json",
        "my_config_version_field",
        "DIALECTS",
        "cgroup v2 memory.max",
    ] {
        assert!(
            hits(clean).is_empty(),
            "false hit on {clean}: {:?}",
            hits(clean)
        );
    }
}
