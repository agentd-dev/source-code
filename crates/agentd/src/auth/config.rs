//! The `[auth]` block in the workflow TOML, parsed into typed defs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::auth::AuthRef;
use crate::error::{Error, Result};

/// Complete auth configuration — bearer and HMAC definitions keyed
/// by operator-facing name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default)]
    pub bearer: HashMap<String, BearerDef>,
    #[serde(default)]
    pub hmac: HashMap<String, HmacDef>,
}

impl AuthConfig {
    /// Check that every [`AuthRef`] in `refs` resolves to a defined
    /// binding. Called at startup so operators learn about missing
    /// bindings before the first request.
    pub fn validate(&self, refs: &[AuthRef]) -> Result<()> {
        for r in refs {
            match r {
                AuthRef::None | AuthRef::MTls => {}
                AuthRef::Bearer { name } => {
                    if !self.bearer.contains_key(name) {
                        return Err(Error::Config(format!(
                            "auth ref `bearer:{name}` is not defined in [auth.bearer]"
                        )));
                    }
                }
                AuthRef::Hmac { name } => {
                    if !self.hmac.contains_key(name) {
                        return Err(Error::Config(format!(
                            "auth ref `hmac:{name}` is not defined in [auth.hmac]"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bearer
// ---------------------------------------------------------------------------

/// One bearer-token binding. Tokens resolve to a flattened set at
/// verification time — newline-separated in an env var, or literal
/// in `tokens` (tests-only; omit from production configs).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BearerDef {
    #[serde(default)]
    pub tokens_env: Option<String>,
    #[serde(default)]
    pub tokens: Vec<String>,
}

impl BearerDef {
    /// Materialise the current token set. Literal `tokens` (tests)
    /// and the newline-separated `tokens_env` (production) both
    /// contribute; empty strings are filtered.
    pub fn tokens(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .tokens
            .iter()
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
            .collect();
        if let Some(var) = &self.tokens_env {
            if let Ok(raw) = std::env::var(var) {
                for line in raw.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// HMAC
// ---------------------------------------------------------------------------

/// One HMAC-SHA256 webhook binding.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HmacDef {
    /// Env var that holds the HMAC key (as a UTF-8 string). The
    /// bytes of the string are used as the key — same convention as
    /// GitHub / Stripe webhooks.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// Literal secret (tests-only; do not use in production).
    #[serde(default)]
    pub secret: Option<String>,
    /// HTTP header carrying the signature. Case-insensitive match
    /// at request time. Defaults to `X-Agent-Signature`.
    #[serde(default)]
    pub header: Option<String>,
    /// String prefix stripped from the signature header value before
    /// hex decoding (e.g. `"sha256="`). Defaults to `"sha256="`.
    #[serde(default)]
    pub prefix: Option<String>,
}

impl HmacDef {
    pub fn effective_header(&self) -> &str {
        self.header.as_deref().unwrap_or("X-Agent-Signature")
    }

    pub fn effective_prefix(&self) -> &str {
        self.prefix.as_deref().unwrap_or("sha256=")
    }

    /// Resolve the secret bytes. Returns `None` if neither
    /// `secret_env` nor literal `secret` is configured (or the env
    /// var is unset).
    pub fn secret_bytes(&self) -> Option<Vec<u8>> {
        if let Some(var) = &self.secret_env {
            if let Ok(val) = std::env::var(var) {
                if !val.is_empty() {
                    return Some(val.into_bytes());
                }
            }
        }
        self.secret.as_ref().and_then(|s| {
            if s.is_empty() {
                None
            } else {
                Some(s.clone().into_bytes())
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_def_aggregates_env_and_literal_tokens() {
        let unique_var = "AGENTD_AUTH_TEST_TOKENS_AGG";
        unsafe { std::env::set_var(unique_var, "from-env\n\nalso-from-env") };
        let def = BearerDef {
            tokens_env: Some(unique_var.to_string()),
            tokens: vec!["from-literal".into(), "".into()],
        };
        let tokens = def.tokens();
        unsafe { std::env::remove_var(unique_var) };
        assert!(tokens.contains(&"from-literal".to_string()));
        assert!(tokens.contains(&"from-env".to_string()));
        assert!(tokens.contains(&"also-from-env".to_string()));
        assert!(!tokens.iter().any(|t| t.is_empty()));
    }

    #[test]
    fn hmac_def_defaults_for_header_and_prefix() {
        let def = HmacDef::default();
        assert_eq!(def.effective_header(), "X-Agent-Signature");
        assert_eq!(def.effective_prefix(), "sha256=");
    }

    #[test]
    fn hmac_def_prefers_env_over_literal() {
        let unique_var = "AGENTD_AUTH_TEST_SECRET_PREF";
        unsafe { std::env::set_var(unique_var, "env-secret") };
        let def = HmacDef {
            secret_env: Some(unique_var.to_string()),
            secret: Some("literal-secret".into()),
            ..HmacDef::default()
        };
        let bytes = def.secret_bytes().unwrap();
        unsafe { std::env::remove_var(unique_var) };
        assert_eq!(&bytes, b"env-secret");
    }

    #[test]
    fn validate_catches_missing_bindings() {
        let cfg = AuthConfig::default();
        let err = cfg
            .validate(&[AuthRef::Bearer {
                name: "missing".into(),
            }])
            .unwrap_err();
        assert!(format!("{err}").contains("bearer:missing"));
    }

    #[test]
    fn validate_accepts_defined_bindings() {
        let mut cfg = AuthConfig::default();
        cfg.bearer.insert("ops".into(), BearerDef::default());
        cfg.hmac.insert("github".into(), HmacDef::default());
        cfg.validate(&[
            AuthRef::Bearer { name: "ops".into() },
            AuthRef::Hmac {
                name: "github".into(),
            },
            AuthRef::None,
            AuthRef::MTls,
        ])
        .unwrap();
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let toml = r#"
            [bearer.ops]
            tokens_env = "X"
            surprise = 1
        "#;
        assert!(toml::from_str::<AuthConfig>(toml).is_err());
    }

    #[test]
    fn full_block_parses() {
        let src = r#"
            [bearer.ops]
            tokens_env = "OPS_TOKENS"

            [hmac.github]
            secret_env = "GH_SECRET"
            header = "X-Hub-Signature-256"
            prefix = "sha256="
        "#;
        let cfg: AuthConfig = toml::from_str(src).unwrap();
        assert!(cfg.bearer.contains_key("ops"));
        let h = cfg.hmac.get("github").unwrap();
        assert_eq!(h.effective_header(), "X-Hub-Signature-256");
    }
}
