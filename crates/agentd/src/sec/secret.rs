// SPDX-License-Identifier: Apache-2.0
//! File-based secret refs (RFC 0017 §6, riding RFC 0006 §6 / RFC 0012 §3.7).
//!
//! Secrets are env/file only — **never** in the config file, **never** logged
//! (RFC 0012 §3.7). This module is the file-backed half of the secret front
//! door: it reads a credential from a mounted file (a Kubernetes `Secret`
//! volume), trims the trailing newline kubelet leaves on a projected file, and
//! resolves the two interpolation tokens a declared header value may carry:
//!
//! - `{{secret:NAME}}` — the value of process env var `NAME`.
//! - `{{secret-file:PATH}}` — the contents of the mounted file at `PATH`,
//!   re-read at the moment of use so a rotation takes effect without a restart
//!   (RFC 0017 §6.1/§6.2).
//!
//! Both are `read_local` only (RFC 0011 §3.1): a filesystem path, no URL
//! scheme, no network. The **template** (`{{secret:…}}` / `{{secret-file:…}}`)
//! is structural and may live in the config file or a flag; the **resolved
//! value** is materialized only at the instant of use and is never retained,
//! never logged. A reference is structural; the value is not in the file — so
//! the RFC 0011/0012 "the file is secret-free" invariant holds exactly.

/// Read a credential from a mounted file, trimming a single trailing newline
/// (kubelet projects a Secret value verbatim; an editor/`echo` commonly appends
/// a `\n`). Errors carry the path but NOT the contents (RFC 0012 §3.7 — a
/// secret never reaches a log/error line).
pub fn read_token_file(path: &str) -> Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read secret file {path}: {e}"))?;
    Ok(trim_token(&raw).to_string())
}

/// Trim a trailing `\n` (and a `\r\n`) from a file-read token. Only the final
/// line-ending is stripped — interior whitespace is part of the credential.
fn trim_token(s: &str) -> &str {
    s.strip_suffix('\n')
        .map(|t| t.strip_suffix('\r').unwrap_or(t))
        .unwrap_or(s)
}

/// Resolve every `{{secret:NAME}}` / `{{secret-file:PATH}}` token in `template`
/// against `env` (the process environment) and the local filesystem, returning
/// the materialized string. Plain text passes through unchanged. A bad token
/// (missing env var, unreadable file, or an unterminated `{{`) is an `Err` with
/// a message that names the ref but NOT the resolved value (RFC 0012 §3.7).
///
/// This is the runtime resolver — it is called at the moment of use, so a
/// rotated `{{secret-file:…}}` is picked up on the next call. `--validate-config`
/// and startup call [`refs_resolvable`] for the side-effect-free pre-flight.
pub fn resolve(template: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    if !template.contains("{{") {
        return Ok(template.to_string());
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let close = after
            .find("}}")
            .ok_or_else(|| "unterminated secret ref '{{' (want '{{secret:NAME}}')".to_string())?;
        let token = after[..close].trim();
        out.push_str(&resolve_one(token, env)?);
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Resolve a single `secret:NAME` / `secret-file:PATH` token body (without the
/// surrounding braces). Any other token is an error — a literal `{{…}}` that is
/// not a secret ref is rejected rather than silently passed through, so a typo
/// can't smuggle braces onto the wire.
fn resolve_one(token: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    if let Some(path) = token.strip_prefix("secret-file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err("empty {{secret-file:}} path".to_string());
        }
        read_token_file(path)
    } else if let Some(name) = token.strip_prefix("secret:") {
        let name = name.trim();
        if name.is_empty() {
            return Err("empty {{secret:}} name".to_string());
        }
        env(name).ok_or_else(|| format!("{{{{secret:{name}}}}} is not set in the environment"))
    } else {
        Err(format!(
            "unknown interpolation token '{{{{{token}}}}}' (want {{{{secret:NAME}}}} or {{{{secret-file:PATH}}}})"
        ))
    }
}

/// Does `value` contain at least one `{{secret:…}}` / `{{secret-file:…}}` ref?
/// Used by the validator to distinguish a (legal) secret *reference* from an
/// (illegal) inline secret-shaped scalar in a declared header (RFC 0017 §3.1).
pub fn has_secret_ref(value: &str) -> bool {
    value.contains("{{secret:") || value.contains("{{secret-file:")
}

/// Side-effect-free-as-possible pre-flight for `--validate-config` / startup:
/// every ref in `template` must resolve (the env var is set; the file exists and
/// is readable). Returns the same diagnostics `resolve` would, without retaining
/// the resolved bytes. A `{{secret-file:…}}` IS read here (it must exist to be
/// valid, RFC 0017 §6.2 — "missing/unreadable at startup → exit 2"), but the
/// contents are dropped immediately.
pub fn refs_resolvable(template: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    resolve(template, env).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn trims_one_trailing_newline_only() {
        assert_eq!(trim_token("tok\n"), "tok");
        assert_eq!(trim_token("tok\r\n"), "tok");
        assert_eq!(trim_token("tok"), "tok");
        // interior + a blank trailing line: only the final \n goes.
        assert_eq!(trim_token("a b\n\n"), "a b\n");
        // no over-trim of interior whitespace.
        assert_eq!(trim_token("  tok  \n"), "  tok  ");
    }

    #[test]
    fn read_token_file_reads_and_trims() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "super-secret").unwrap();
        let v = read_token_file(f.path().to_str().unwrap()).unwrap();
        assert_eq!(v, "super-secret");
    }

    #[test]
    fn read_token_file_missing_is_error_without_contents() {
        let e = read_token_file("/no/such/secret/file").unwrap_err();
        assert!(e.contains("cannot read secret file"));
    }

    #[test]
    fn resolve_passthrough_and_env_ref() {
        let env = env_of(&[("ANTHROPIC_API_KEY", "k-123")]);
        assert_eq!(resolve("plain text", &env).unwrap(), "plain text");
        assert_eq!(
            resolve("x-api-key: {{secret:ANTHROPIC_API_KEY}}", &env).unwrap(),
            "x-api-key: k-123"
        );
        // a bare token with surrounding text on both sides.
        assert_eq!(
            resolve("Bearer {{secret:ANTHROPIC_API_KEY}}!", &env).unwrap(),
            "Bearer k-123!"
        );
    }

    #[test]
    fn resolve_file_ref_reads_fresh_and_trims() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "file-tok").unwrap();
        let path = f.path().to_str().unwrap();
        let env = env_of(&[]);
        let tmpl = format!("Bearer {{{{secret-file:{path}}}}}");
        assert_eq!(resolve(&tmpl, &env).unwrap(), "Bearer file-tok");
    }

    #[test]
    fn resolve_missing_env_is_error_and_does_not_leak_value() {
        let env = env_of(&[]);
        let e = resolve("{{secret:NOPE}}", &env).unwrap_err();
        assert!(e.contains("NOPE"));
        assert!(e.contains("not set"));
    }

    #[test]
    fn resolve_unknown_token_and_unterminated_are_errors() {
        let env = env_of(&[]);
        assert!(resolve("{{bogus:x}}", &env).is_err());
        assert!(resolve("{{secret:", &env).is_err());
        assert!(resolve("{{secret:}}", &env).is_err());
        assert!(resolve("{{secret-file:}}", &env).is_err());
    }

    #[test]
    fn has_secret_ref_detects_both_kinds() {
        assert!(has_secret_ref("{{secret:X}}"));
        assert!(has_secret_ref("Bearer {{secret-file:/p}}"));
        assert!(!has_secret_ref("plain value"));
        assert!(!has_secret_ref("2023-06-01"));
    }
}
