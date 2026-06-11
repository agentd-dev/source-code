//! Remote LLM providers (RFC 0006 §3) — `intel-remote` feature.
//!
//! One blocking HTTPS client (`ureq`, rustls-backed — no async
//! runtime enters the core) speaking three native dialects plus the
//! openai-compatible one:
//!
//! - **anthropic** — `POST {base}/v1/messages`, `x-api-key` header.
//! - **openai** — `POST {base}/v1/chat/completions`, bearer auth.
//! - **gemini** — `POST {base}/v1beta/models/{model}:generateContent`,
//!   `x-goog-api-key` header.
//! - **openai-compatible** — the openai dialect against any
//!   `base_url` (vLLM, Ollama, LM Studio, gateways); key optional.
//!
//! Every provider maps into the same [`Request`]/[`Response`] shapes
//! the socket transports use, so handlers never know which transport
//! served them. Timeouts ride the run's `--timeout-secs`.

use std::time::Duration;

use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::intelligence::backends::{BackendDef, ProviderKind};
use crate::intelligence::client::IntelligenceClient;
use crate::intelligence::protocol::{Request, Response, Usage};

impl std::fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material.
        f.debug_struct("RemoteClient")
            .field("kind", &self.kind.as_str())
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

pub struct RemoteClient {
    kind: ProviderKind,
    model: String,
    base_url: String,
    /// The secret NAME from `api_key_env` — resolved through the
    /// secrets registry *per request*, so a rotating source (OAuth2
    /// token, Vault-managed file) is always served fresh. Build time
    /// still probes it once, so a missing key is a startup error.
    api_key_name: Option<String>,
    default_max_tokens: Option<u32>,
    agent: ureq::Agent,
}

impl RemoteClient {
    /// Build from a validated [`BackendDef`]. Key resolution happens
    /// here — a named-but-unset env var is a startup error, not a
    /// first-request 401.
    pub fn from_def(def: &BackendDef, timeout: Duration) -> Result<Self> {
        // Probe the key once so a bad reference is a startup error —
        // but store the NAME: each request re-resolves through the
        // secrets registry, keeping OAuth2/file-rotated keys fresh.
        if let Some(var) = &def.api_key_env
            && let Err(e) = crate::secrets::resolve(var)
        {
            return Err(Error::Config(format!(
                "intelligence.backends.{}: {e}",
                def.name
            )));
        }
        let api_key_name = def.api_key_env.clone();
        let base_url = def
            .base_url
            .clone()
            .unwrap_or_else(|| default_base_url(def.provider).to_string())
            .trim_end_matches('/')
            .to_string();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(timeout)
            .build();
        Ok(Self {
            kind: def.provider,
            model: def.model.clone().expect("validate_list enforces model"),
            base_url,
            api_key_name,
            default_max_tokens: def.max_tokens,
            agent,
        })
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        req.max_tokens.or(self.default_max_tokens).unwrap_or(1024)
    }
}

fn default_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "https://api.anthropic.com",
        ProviderKind::Openai => "https://api.openai.com",
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com",
        // validate_list requires an explicit base_url here.
        ProviderKind::OpenaiCompatible => "",
    }
}

/// Split the runtime's message list into (system, turns) — every
/// provider wants the system prompt out-of-band or as a dedicated
/// role, and the socket protocol allows it inline.
fn split_system(req: &Request) -> (Option<&str>, Vec<(&str, &str)>) {
    let mut system = None;
    let mut turns = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        if m.role == "system" && system.is_none() {
            system = Some(m.content.as_str());
        } else {
            turns.push((m.role.as_str(), m.content.as_str()));
        }
    }
    (system, turns)
}

impl IntelligenceClient for RemoteClient {
    fn complete(&self, req: &Request) -> Result<Response> {
        // Per-request resolution: a rotated file or refreshed OAuth2
        // token is picked up on the very next call, no reload needed.
        let api_key = match &self.api_key_name {
            Some(name) => Some(
                crate::secrets::resolve(name)
                    .map_err(|e| Error::Intelligence(format!("{}: {e}", self.kind.as_str())))?,
            ),
            None => None,
        };
        let (url, body, headers): (String, Value, Vec<(&str, String)>) = match self.kind {
            ProviderKind::Anthropic => {
                let (system, turns) = split_system(req);
                let mut body = json!({
                    "model": self.model,
                    "max_tokens": self.effective_max_tokens(req),
                    "messages": turns.iter().map(|(role, content)| {
                        json!({ "role": role, "content": content })
                    }).collect::<Vec<_>>(),
                });
                if let Some(s) = system {
                    body["system"] = json!(s);
                }
                if let Some(t) = req.temperature {
                    body["temperature"] = json!(t);
                }
                (
                    format!("{}/v1/messages", self.base_url),
                    body,
                    vec![
                        ("x-api-key", api_key.clone().unwrap_or_default()),
                        ("anthropic-version", "2023-06-01".into()),
                    ],
                )
            }
            ProviderKind::Openai | ProviderKind::OpenaiCompatible => {
                let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len());
                for m in &req.messages {
                    messages.push(json!({ "role": m.role, "content": m.content }));
                }
                let mut body = json!({
                    "model": self.model,
                    "max_tokens": self.effective_max_tokens(req),
                    "messages": messages,
                });
                if let Some(t) = req.temperature {
                    body["temperature"] = json!(t);
                }
                let mut headers = Vec::new();
                if let Some(k) = &api_key {
                    headers.push(("authorization", format!("Bearer {k}")));
                }
                (
                    format!("{}/v1/chat/completions", self.base_url),
                    body,
                    headers,
                )
            }
            ProviderKind::Gemini => {
                let (system, turns) = split_system(req);
                let mut body = json!({
                    "contents": turns.iter().map(|(role, content)| {
                        // Gemini spells the assistant role "model".
                        let role = if *role == "assistant" { "model" } else { role };
                        json!({ "role": role, "parts": [{ "text": content }] })
                    }).collect::<Vec<_>>(),
                    "generationConfig": { "maxOutputTokens": self.effective_max_tokens(req) },
                });
                if let Some(s) = system {
                    body["system_instruction"] = json!({ "parts": [{ "text": s }] });
                }
                if let Some(t) = req.temperature {
                    body["generationConfig"]["temperature"] = json!(t);
                }
                (
                    format!(
                        "{}/v1beta/models/{}:generateContent",
                        self.base_url, self.model
                    ),
                    body,
                    vec![("x-goog-api-key", api_key.clone().unwrap_or_default())],
                )
            }
        };

        let mut call = self
            .agent
            .post(&url)
            .set("content-type", "application/json");
        for (k, v) in &headers {
            if !v.is_empty() {
                call = call.set(k, v);
            }
        }

        let resp = call.send_string(&body.to_string()).map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let detail = r.into_string().unwrap_or_default();
                Error::Intelligence(format!(
                    "{} returned {code}: {}",
                    self.kind.as_str(),
                    truncate(&detail, 400)
                ))
            }
            other => Error::Intelligence(format!("{}: {other}", self.kind.as_str())),
        })?;

        let raw = resp
            .into_string()
            .map_err(|e| Error::Intelligence(format!("read response: {e}")))?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| Error::Intelligence(format!("response is not JSON: {e}")))?;
        parse_response(self.kind, &v)
    }
}

/// Pull `content` + `usage` out of a provider response.
fn parse_response(kind: ProviderKind, v: &Value) -> Result<Response> {
    let missing =
        |what: &str| Error::Intelligence(format!("{} response missing {what}", kind.as_str()));
    match kind {
        ProviderKind::Anthropic => {
            let content = v["content"][0]["text"]
                .as_str()
                .ok_or_else(|| missing("content[0].text"))?
                .to_string();
            Ok(Response {
                content,
                usage: Usage {
                    prompt_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
                    completion_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
                },
            })
        }
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => {
            let content = v["choices"][0]["message"]["content"]
                .as_str()
                .ok_or_else(|| missing("choices[0].message.content"))?
                .to_string();
            Ok(Response {
                content,
                usage: Usage {
                    prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                    completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
                },
            })
        }
        ProviderKind::Gemini => {
            let content = v["candidates"][0]["content"]["parts"][0]["text"]
                .as_str()
                .ok_or_else(|| missing("candidates[0].content.parts[0].text"))?
                .to_string();
            Ok(Response {
                content,
                usage: Usage {
                    prompt_tokens: v["usageMetadata"]["promptTokenCount"].as_u64().unwrap_or(0)
                        as u32,
                    completion_tokens: v["usageMetadata"]["candidatesTokenCount"]
                        .as_u64()
                        .unwrap_or(0) as u32,
                },
            })
        }
    }
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

// ---------------------------------------------------------------------------
// Tests — fake provider servers over loopback HTTP
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::protocol::Message;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot fake provider: accepts a single request, captures it,
    /// answers with `response_body`.
    fn fake_provider(response_body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 65536];
            let mut seen = Vec::new();
            loop {
                let n = s.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
                if let Some(pos) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&seen[..pos]).to_string();
                    let want: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    while seen.len() < pos + 4 + want {
                        let n = s.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        seen.extend_from_slice(&buf[..n]);
                    }
                    break;
                }
            }
            let body = response_body.as_bytes();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
            s.write_all(body).unwrap();
            s.flush().unwrap();
            String::from_utf8_lossy(&seen).to_string()
        });
        (base, handle)
    }

    fn req() -> Request {
        Request {
            model: "default".into(),
            messages: vec![
                Message {
                    role: "system".into(),
                    content: "be brief".into(),
                },
                Message {
                    role: "user".into(),
                    content: "hello".into(),
                },
            ],
            max_tokens: Some(64),
            temperature: None,
        }
    }

    fn client(kind: ProviderKind, base: &str, key: Option<&str>) -> RemoteClient {
        let def = BackendDef {
            name: "t".into(),
            provider: kind,
            model: Some("test-model".into()),
            api_key_env: None,
            base_url: Some(base.to_string()),
            max_tokens: None,
        };
        let mut c = RemoteClient::from_def(&def, Duration::from_secs(5)).unwrap();
        // Keys are now resolved by NAME per request; park the literal
        // in a per-kind env var and reference it (env fallback path).
        c.api_key_name = key.map(|literal| {
            // Var name derived from the literal: parallel tests with
            // different keys never clobber each other.
            let var = format!(
                "AGENTD_TEST_PROVIDER_KEY_{}",
                literal.replace(['-', ':'], "_").to_uppercase()
            );
            // SAFETY: test-scoped env mutation, unique var per key.
            unsafe { std::env::set_var(&var, literal) };
            var
        });
        c
    }

    #[test]
    fn anthropic_request_and_response_shapes() {
        let (base, server) = fake_provider(
            r#"{"content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":7,"output_tokens":3}}"#,
        );
        let c = client(ProviderKind::Anthropic, &base, Some("sk-test"));
        let out = c.complete(&req()).unwrap();
        assert_eq!(out.content, "hi");
        assert_eq!(out.usage.prompt_tokens, 7);
        assert_eq!(out.usage.completion_tokens, 3);

        let seen = server.join().unwrap();
        assert!(seen.contains("POST /v1/messages"), "{seen}");
        assert!(seen.to_lowercase().contains("x-api-key: sk-test"));
        assert!(seen.contains(r#""system":"be brief""#));
        assert!(seen.contains(r#""model":"test-model""#));
        // system message must NOT appear in the messages array
        assert!(!seen.contains(r#"{"role":"system""#));
    }

    #[test]
    fn openai_request_and_response_shapes() {
        let (base, server) = fake_provider(
            r#"{"choices":[{"message":{"role":"assistant","content":"yo"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        );
        let c = client(ProviderKind::Openai, &base, Some("sk-oai"));
        let out = c.complete(&req()).unwrap();
        assert_eq!(out.content, "yo");
        assert_eq!(out.usage.prompt_tokens, 5);

        let seen = server.join().unwrap();
        assert!(seen.contains("POST /v1/chat/completions"), "{seen}");
        assert!(seen.to_lowercase().contains("authorization: bearer sk-oai"));
        // serde_json's default map is ordered alphabetically — assert
        // on content, not key order.
        assert!(seen.contains(r#""role":"system""#));
        assert!(seen.contains(r#""content":"be brief""#));
    }

    #[test]
    fn openai_compatible_works_keyless() {
        let (base, server) =
            fake_provider(r#"{"choices":[{"message":{"content":"local"}}],"usage":{}}"#);
        let c = client(ProviderKind::OpenaiCompatible, &base, None);
        let out = c.complete(&req()).unwrap();
        assert_eq!(out.content, "local");
        let seen = server.join().unwrap();
        assert!(!seen.to_lowercase().contains("authorization:"));
    }

    #[test]
    fn gemini_request_and_response_shapes() {
        let (base, server) = fake_provider(
            r#"{"candidates":[{"content":{"parts":[{"text":"g"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1}}"#,
        );
        let c = client(ProviderKind::Gemini, &base, Some("g-key"));
        let out = c.complete(&req()).unwrap();
        assert_eq!(out.content, "g");
        assert_eq!(out.usage.completion_tokens, 1);

        let seen = server.join().unwrap();
        assert!(
            seen.contains("POST /v1beta/models/test-model:generateContent"),
            "{seen}"
        );
        assert!(seen.to_lowercase().contains("x-goog-api-key: g-key"));
        assert!(seen.contains("system_instruction"));
    }

    #[test]
    fn provider_error_status_surfaces_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = r#"{"error":{"message":"bad key"}}"#;
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        let c = client(ProviderKind::Openai, &base, Some("bad"));
        let err = c.complete(&req()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("bad key"));
    }

    #[test]
    fn unset_key_env_is_startup_error() {
        let def = BackendDef {
            name: "x".into(),
            provider: ProviderKind::Anthropic,
            model: Some("m".into()),
            api_key_env: Some("AGENTD_TEST_DEFINITELY_UNSET_KEY_VAR".into()),
            base_url: None,
            max_tokens: None,
        };
        let err = RemoteClient::from_def(&def, Duration::from_secs(1)).unwrap_err();
        assert!(format!("{err}").contains("unset or empty"));
    }
}
