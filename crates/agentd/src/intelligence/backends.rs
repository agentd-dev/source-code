//! Named intelligence backends (RFC 0006 §3).
//!
//! `[[intelligence.backends]]` entries map a name to a transport.
//! `llm_infer` and `agent_loop` nodes address backends by that name;
//! `"default"` is reserved for the CLI-configured socket transports
//! (`--intel-unix` / `--intel-http`) so existing workflows keep
//! their meaning.
//!
//! API keys are **never** written in the TOML — `api_key_env` names
//! an environment variable, keeping workflow files shareable and
//! signable. Remote providers require the `intel-remote` Cargo
//! feature; a workflow that names one on a feature-off build fails
//! at startup with a rebuild hint, same pattern as every other
//! optional family.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::intelligence::client::ReloadableIntelClient;

/// `[intelligence]` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntelligenceConfig {
    #[serde(default)]
    pub backends: Vec<BackendDef>,
}

/// One named backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackendDef {
    pub name: String,
    pub provider: ProviderKind,
    /// Concrete model id (`claude-sonnet-4-6`, `gpt-4o`, …).
    /// Required for remote providers.
    #[serde(default)]
    pub model: Option<String>,
    /// Environment variable holding the API key. Required for
    /// `anthropic` / `openai` / `gemini`; optional for
    /// `openai-compatible` (local servers often run keyless).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Override the provider's default endpoint. Required for
    /// `openai-compatible`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Per-request output-token cap forwarded to the provider.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    Openai,
    Gemini,
    /// Any server speaking the OpenAI chat-completions dialect:
    /// vLLM, Ollama, LM Studio, gateways. `base_url` is required.
    OpenaiCompatible,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Openai => "openai",
            ProviderKind::Gemini => "gemini",
            ProviderKind::OpenaiCompatible => "openai-compatible",
        }
    }
}

impl BackendDef {
    /// Startup validation for a backend list: unique names, the
    /// `default` reservation, and per-provider required fields.
    pub fn validate_list(defs: &[BackendDef]) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for d in defs {
            if d.name.trim().is_empty() {
                return Err("intelligence.backends: `name` must be non-empty".into());
            }
            if d.name == "default" {
                return Err(
                    "intelligence.backends: the name `default` is reserved for the \
                     --intel-unix / --intel-http CLI transports"
                        .into(),
                );
            }
            if !seen.insert(d.name.as_str()) {
                return Err(format!(
                    "intelligence.backends: duplicate backend name `{}`",
                    d.name
                ));
            }
            if d.model.as_deref().unwrap_or("").is_empty() {
                return Err(format!(
                    "intelligence.backends.{}: `model` is required",
                    d.name
                ));
            }
            match d.provider {
                ProviderKind::Anthropic | ProviderKind::Openai | ProviderKind::Gemini => {
                    if d.api_key_env.as_deref().unwrap_or("").is_empty() {
                        return Err(format!(
                            "intelligence.backends.{}: `api_key_env` is required for {} \
                             (keys never live in the TOML)",
                            d.name,
                            d.provider.as_str()
                        ));
                    }
                }
                ProviderKind::OpenaiCompatible => {
                    if d.base_url.as_deref().unwrap_or("").is_empty() {
                        return Err(format!(
                            "intelligence.backends.{}: `base_url` is required for \
                             openai-compatible",
                            d.name
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Name → reloadable client map shared by the `llm_infer` and
/// `agent_loop` handlers.
pub type BackendMap = Arc<HashMap<String, Arc<ReloadableIntelClient>>>;

/// Wrap one shared client as the sole `default` backend. Tests and
/// embedders use this; the runtime builds the full map.
pub fn single_backend(client: crate::intelligence::client::IntelligenceRef) -> BackendMap {
    let mut m = HashMap::new();
    m.insert(
        "default".to_string(),
        Arc::new(ReloadableIntelClient::from_ref(client)),
    );
    Arc::new(m)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, provider: ProviderKind) -> BackendDef {
        BackendDef {
            name: name.into(),
            provider,
            model: Some("m1".into()),
            api_key_env: Some("KEY".into()),
            base_url: Some("http://x".into()),
            max_tokens: None,
        }
    }

    #[test]
    fn parses_from_toml() {
        let src = r#"
            [[backends]]
            name = "claude"
            provider = "anthropic"
            model = "claude-sonnet-4-6"
            api_key_env = "ANTHROPIC_API_KEY"

            [[backends]]
            name = "local"
            provider = "openai-compatible"
            model = "qwen3"
            base_url = "http://127.0.0.1:8000/v1"
        "#;
        let cfg: IntelligenceConfig = toml::from_str(src).unwrap();
        assert_eq!(cfg.backends.len(), 2);
        assert_eq!(cfg.backends[0].provider, ProviderKind::Anthropic);
        assert_eq!(cfg.backends[1].provider, ProviderKind::OpenaiCompatible);
        BackendDef::validate_list(&cfg.backends).unwrap();
    }

    #[test]
    fn default_name_is_reserved() {
        let d = def("default", ProviderKind::Anthropic);
        assert!(
            BackendDef::validate_list(&[d])
                .unwrap_err()
                .contains("reserved")
        );
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = BackendDef::validate_list(&[
            def("a", ProviderKind::Openai),
            def("a", ProviderKind::Gemini),
        ])
        .unwrap_err();
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn remote_providers_require_key_env() {
        let mut d = def("a", ProviderKind::Anthropic);
        d.api_key_env = None;
        assert!(
            BackendDef::validate_list(&[d])
                .unwrap_err()
                .contains("api_key_env")
        );
    }

    #[test]
    fn compatible_requires_base_url() {
        let mut d = def("a", ProviderKind::OpenaiCompatible);
        d.api_key_env = None;
        d.base_url = None;
        assert!(
            BackendDef::validate_list(&[d])
                .unwrap_err()
                .contains("base_url")
        );
    }

    #[test]
    fn model_required() {
        let mut d = def("a", ProviderKind::Openai);
        d.model = None;
        assert!(
            BackendDef::validate_list(&[d])
                .unwrap_err()
                .contains("model")
        );
    }
}
