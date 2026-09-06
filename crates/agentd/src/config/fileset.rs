// SPDX-License-Identifier: AGPL-3.0-only
//! **A directory of documents as one source** — `dir:` + `glob:` + `order:`.
//!
//! Workflows have loaded a folder this way for releases; instructions now do
//! too, and both call THIS code rather than each growing their own walk. A
//! second implementation of "which files, in what order" is how two settings
//! that look alike start behaving differently.

/// How a matched set is ordered. The order is part of the contract, not an
/// accident of the filesystem: concatenated documents read in this order, and
/// `read_dir` returns entries in whatever sequence the OS feels like.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// By path, ascending — stable across machines, and what a `01-`, `02-`
    /// naming convention exists to exploit.
    #[default]
    Name,
    /// By modification time, oldest first — newest material reads last.
    Date,
}

/// A folder source: the path, and the two settings that only mean anything
/// with a folder to apply them to.
///
/// `glob` and `order` live INSIDE `dir` rather than beside it because they
/// qualify it and nothing else — a `glob:` with no folder configures nothing,
/// and a flat spelling has to refuse that combination at load instead of
/// making it unsayable. The short spelling is the common case:
///
/// ```yaml
/// dir: ./instructions                    # every document, in name order
/// dir: { path: ./instructions, glob: "*.md", order: date }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dir {
    /// `dir: ./instructions`
    Path(String),
    /// `dir: { path, glob?, order? }`
    Detailed {
        path: String,
        glob: Option<String>,
        order: Option<Order>,
    },
}

impl Dir {
    pub fn path(&self) -> &str {
        match self {
            Dir::Path(p) | Dir::Detailed { path: p, .. } => p,
        }
    }
    pub fn glob(&self) -> Option<&str> {
        match self {
            Dir::Path(_) => None,
            Dir::Detailed { glob, .. } => glob.as_deref(),
        }
    }
    pub fn order(&self) -> Order {
        match self {
            Dir::Path(_) => Order::default(),
            Dir::Detailed { order, .. } => order.unwrap_or_default(),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Dir {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Long {
            path: String,
            #[serde(default)]
            glob: Option<String>,
            #[serde(default)]
            order: Option<Order>,
        }
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Path(String),
            Long(Long),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Path(p) => Dir::Path(p),
            Raw::Long(Long { path, glob, order }) => Dir::Detailed { path, glob, order },
        })
    }
}

/// Expand a workflow directory into the files it contains.
///
/// `pattern` is a comma-separated list of shell-style globs relative to `dir`.
/// `**` crosses directory boundaries, so `**/*.yaml` walks the tree and
/// `*.yaml` does not — the distinction people already expect from every other
/// tool that takes a glob.
///
/// Results are SORTED. A directory listing is in whatever order the filesystem
/// feels like, and load order decides which of two same-named workflows is
/// reported as the duplicate — a diagnostic that changed between machines would
/// be worse than useless.
pub fn expand_dir(dir: &str, pattern: &str) -> Result<Vec<String>, String> {
    let root = std::path::Path::new(dir);
    if !root.is_dir() {
        return Err(format!("not a directory ({})", root.display()));
    }
    let pats: Vec<&str> = pattern
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let recursive = pats.iter().any(|p| p.contains("**"));
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = std::fs::read_dir(&d).map_err(|e| e.to_string())?;
        for ent in rd.flatten() {
            let path = ent.path();
            if path.is_dir() {
                if recursive {
                    stack.push(path);
                }
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rels = rel.to_string_lossy();
            if glob_match_any(pattern, &rels) {
                out.push(path.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// [`expand_dir`], then ordered. `Order::Date` reads each file's mtime; a
/// file whose mtime cannot be read sorts as the epoch rather than failing the
/// whole load, since an unreadable timestamp is not a reason to refuse a
/// document that reads fine.
pub fn expand_dir_ordered(dir: &str, pattern: &str, order: Order) -> Result<Vec<String>, String> {
    let mut files = expand_dir(dir, pattern)?;
    if order == Order::Date {
        let mtime = |p: &String| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        };
        files.sort_by_key(mtime);
    }
    Ok(files)
}

/// Any of a comma-separated pattern list matches. The list form is what every
/// `glob:` setting takes, so the split lives here rather than at each caller.
pub fn glob_match_any(patterns: &str, text: &str) -> bool {
    patterns
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|p| glob_match(p, text))
}

/// The default `glob:` for a folder of INSTRUCTION documents — the same
/// extensions `classify_instruction` treats as a document.
pub const DOCUMENT_GLOB: &str = "*.md,*.markdown,*.txt,*.instruction";

/// Shell-style glob matching: `*` within a segment, `**` across segments, `?`
/// for one character. Small on purpose — a workflow directory does not need
/// brace expansion or character classes, and a dependency for this would be a
/// poor trade in a tree that counts them.
pub fn glob_match(pat: &str, text: &str) -> bool {
    // `**/x` should also match a bare `x` at the root: people write it meaning
    // "at any depth", which includes none.
    if let Some(rest) = pat.strip_prefix("**/")
        && glob_match(rest, text)
    {
        return true;
    }
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), text.chars().collect());
    fn go(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => {
                let doubled = p.get(1) == Some(&'*');
                let rest = if doubled { &p[2..] } else { &p[1..] };
                // A single `*` stops at a separator; `**` does not.
                let mut i = 0;
                loop {
                    if go(rest, &t[i..]) {
                        return true;
                    }
                    if i >= t.len() {
                        return false;
                    }
                    if !doubled && t[i] == '/' {
                        return false;
                    }
                    i += 1;
                }
            }
            Some('?') if !t.is_empty() => go(&p[1..], &t[1..]),
            Some(c) if t.first() == Some(c) => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(&p, &t)
}

#[cfg(test)]
mod glob_tests {
    use super::glob_match;

    #[test]
    fn a_single_star_stays_inside_one_segment_and_double_crosses() {
        // The distinction people expect from every other tool that takes a glob.
        assert!(glob_match("*.yaml", "nightly.yaml"));
        assert!(
            !glob_match("*.yaml", "team/nightly.yaml"),
            "* must not cross /"
        );
        assert!(glob_match("**/*.yaml", "team/nightly.yaml"));
        assert!(glob_match("**/*.yaml", "a/b/c/deep.yaml"));
        // `**/x` means "at any depth", and no depth is a depth — otherwise a
        // recursive pattern silently skips the files at the root.
        assert!(glob_match("**/*.yaml", "nightly.yaml"));

        assert!(glob_match("flows/*.json", "flows/a.json"));
        assert!(!glob_match("flows/*.json", "flows/a.yaml"));
        assert!(!glob_match("*.yaml", "yaml"), "the dot is literal");
        assert!(glob_match("?.yaml", "a.yaml"));
        assert!(!glob_match("?.yaml", "ab.yaml"));
        // A pattern with no wildcard is an exact name.
        assert!(glob_match("nightly.yaml", "nightly.yaml"));
        assert!(!glob_match("nightly.yaml", "nightly.yml"));
    }
}
