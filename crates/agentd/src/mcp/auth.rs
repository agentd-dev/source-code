// SPDX-License-Identifier: Apache-2.0
//! Auth material resolution for remote MCP endpoints (RFC 0012 §3.7).
//!
//! An [`crate::config::McpServerSpec`] carries secret-**free** header templates
//! (e.g. `Authorization: Bearer {{secret:MCP_TOKEN}}`); this materializes them to
//! real request headers at connect time — so the manifest/spawn-payload never
//! holds a credential and a rotated `{{secret-file:…}}` is picked up on the next
//! (re)connect. Bearer / API-key auth rides here; mutual-TLS (a client cert) and
//! OAuth 2.1 client-credentials are separate axes threaded in alongside.

use crate::sec::secret;

/// Resolve every `{{secret:NAME}}` / `{{secret-file:PATH}}` ref in each header
/// VALUE against the process environment + filesystem, returning materialized
/// `(name, value)` headers ready for the wire. Header names pass through as-is.
/// An unresolved ref is an `Err` that names the ref but never the resolved value
/// (RFC 0012 §3.7 secret-freedom).
pub fn resolve_headers(templates: &[(String, String)]) -> Result<Vec<(String, String)>, String> {
    let env = |k: &str| std::env::var(k).ok();
    templates
        .iter()
        .map(|(name, value)| Ok((name.clone(), secret::resolve(value, &env)?)))
        .collect()
}

/// Pre-flight (for `--validate-config` / startup): every header template must
/// resolve, without retaining the bytes. Same diagnostics as [`resolve_headers`].
pub fn headers_resolvable(templates: &[(String, String)]) -> Result<(), String> {
    resolve_headers(templates).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn resolves_env_secret_in_header_value() {
        // SAFETY: single-threaded test; unique var name avoids cross-test races.
        unsafe { std::env::set_var("MCP_AUTH_TEST_TOKEN", "s3cr3t") };
        let headers = resolve_headers(&[(
            "Authorization".into(),
            "Bearer {{secret:MCP_AUTH_TEST_TOKEN}}".into(),
        )])
        .unwrap();
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer s3cr3t");
        unsafe { std::env::remove_var("MCP_AUTH_TEST_TOKEN") };
    }

    #[test]
    fn resolves_secret_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "file-token").unwrap();
        let tmpl = format!("Bearer {{{{secret-file:{}}}}}", f.path().to_str().unwrap());
        let headers = resolve_headers(&[("x-api-key".into(), tmpl)]).unwrap();
        assert_eq!(headers[0].1, "Bearer file-token");
    }

    #[test]
    fn plain_header_passes_through() {
        let headers =
            resolve_headers(&[("Accept".into(), "application/json".into())]).unwrap();
        assert_eq!(headers[0].1, "application/json");
    }

    #[test]
    fn missing_secret_is_an_error_without_the_value() {
        let err = resolve_headers(&[(
            "Authorization".into(),
            "Bearer {{secret:DEFINITELY_UNSET_MCP_VAR}}".into(),
        )])
        .unwrap_err();
        assert!(err.contains("DEFINITELY_UNSET_MCP_VAR"));
        assert!(!err.contains("Bearer"), "the value must not leak: {err}");
    }
}
