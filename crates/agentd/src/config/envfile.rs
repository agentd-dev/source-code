// SPDX-License-Identifier: AGPL-3.0-only
//! `.env` files (`--env <FILE>`, repeatable): a dependency-free dotenv subset.
//!
//! Lines are `KEY=VALUE` with optional `export ` prefix; `#` starts a comment
//! (a whole line, or trailing an *unquoted* value); single quotes are literal;
//! double quotes understand `\n` `\t` `\r` `\\` `\"`. There is **no `$VAR`
//! interpolation inside the file** — the config layer's `${VAR:-default}`
//! expansion already exists for that, and two interpolation passes with
//! different rules is how values get mangled silently.
//!
//! Precedence is the dotenv convention: the **real environment wins** over any
//! file (a deployment override beats the checked-in defaults file), and among
//! files the **later wins** for keys the environment does not pin. A malformed
//! line or an unreadable file is a startup refusal naming file and line —
//! fail-closed, like every other config error.

/// Parse one file's content. Returns pairs in file order.
pub fn parse(content: &str, file: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (i, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(eq) = line.find('=') else {
            return Err(format!("{file}:{}: expected KEY=VALUE, got {raw:?}", i + 1));
        };
        let key = line[..eq].trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            || key.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(format!("{file}:{}: invalid key {key:?}", i + 1));
        }
        let rest = line[eq + 1..].trim();
        let value = if let Some(q) = rest.strip_prefix('"') {
            // Double-quoted: escapes, must close, nothing but a comment after.
            let (v, after) = unescape_double(q)
                .ok_or_else(|| format!("{file}:{}: unterminated \" quote", i + 1))?;
            let after = after.trim();
            if !after.is_empty() && !after.starts_with('#') {
                return Err(format!(
                    "{file}:{}: unexpected trailing content {after:?}",
                    i + 1
                ));
            }
            v
        } else if let Some(q) = rest.strip_prefix('\'') {
            let end = q
                .find('\'')
                .ok_or_else(|| format!("{file}:{}: unterminated ' quote", i + 1))?;
            let after = q[end + 1..].trim();
            if !after.is_empty() && !after.starts_with('#') {
                return Err(format!(
                    "{file}:{}: unexpected trailing content {after:?}",
                    i + 1
                ));
            }
            q[..end].to_string()
        } else {
            // Unquoted: runs to a trailing comment or end of line.
            match rest.find(" #") {
                Some(h) => rest[..h].trim().to_string(),
                None => rest.to_string(),
            }
        };
        out.push((key.to_string(), value));
    }
    Ok(out)
}

/// `"…"` body → (value, remainder-after-closing-quote).
fn unescape_double(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &s[i + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            _ => out.push(c),
        }
    }
    None
}

/// Load every `--env` file in order and fold them into one map — later files
/// win. The caller applies the real-environment-wins rule (it knows the
/// environment; this function deliberately does not read it, so tests can).
pub fn load_files(paths: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut merged: Vec<(String, String)> = Vec::new();
    for path in paths {
        let content = std::fs::read_to_string(path).map_err(|e| format!("--env {path}: {e}"))?;
        for (k, v) in parse(&content, path)? {
            merged.retain(|(ek, _)| ek != &k);
            merged.push((k, v));
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dotenv_subset_parses_and_bad_lines_say_where() {
        let src = r#"
# a comment
FOO=bar
export QUOTED="a b\nc"
LIT='keep $THIS literal'
TRAIL=value # trailing comment
EMPTY=
DOTTED.KEY=ok
"#;
        let v = parse(src, "x.env").unwrap();
        let get = |k: &str| v.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get("FOO"), Some("bar"));
        assert_eq!(get("QUOTED"), Some("a b\nc"));
        assert_eq!(get("LIT"), Some("keep $THIS literal"));
        assert_eq!(get("TRAIL"), Some("value"));
        assert_eq!(get("EMPTY"), Some(""));
        assert_eq!(get("DOTTED.KEY"), Some("ok"));

        for (bad, what) in [
            ("JUSTAWORD", "expected KEY=VALUE"),
            ("2BAD=x", "invalid key"),
            ("Q=\"unterminated", "unterminated"),
            ("Q='unterminated", "unterminated"),
            ("Q=\"x\" extra", "trailing content"),
        ] {
            let e = parse(bad, "y.env").unwrap_err();
            assert!(e.contains(what), "{bad:?} → {e}");
            assert!(e.contains("y.env:1"), "names the location: {e}");
        }
    }

    #[test]
    fn later_files_win_within_the_env_layer() {
        let d = std::env::temp_dir().join(format!("agentd-envfile-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("a.env");
        let b = d.join("b.env");
        std::fs::write(&a, "K=from_a\nONLY_A=1\n").unwrap();
        std::fs::write(&b, "K=from_b\n").unwrap();
        let merged = load_files(&[
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let get = |k: &str| {
            merged
                .iter()
                .find(|(ek, _)| ek == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("K"), Some("from_b"));
        assert_eq!(get("ONLY_A"), Some("1"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
