// SPDX-License-Identifier: AGPL-3.0-only
// The MCP client itself lives in the reusable `mcp` crate; re-export it so that
// `crate::mcp::client::{McpClient, McpError}` resolves here. [`from_spec`] below is
// the agentd-side integration — config, auth and identity — that belongs in this crate.
pub use ::mcp::client;
// Re-export the transport module so the agentd integration (and its tests) can
// name the `RequestSigner` seam that credential providers plug into.
pub use ::mcp::http;

/// The **pace registry** behind every `rate:` on an endpoint: one
/// process-global set of token buckets. An MCP server's is seeded at the
/// [`from_spec`] chokepoint — the ONLY place every process (reactor, turn
/// worker, flat subagent) constructs its MCP clients — so a rated server is
/// paced wherever the call actually executes; a model connection's, a peer's
/// and an `http.endpoints` entry's are taken by key where they are dialed.
pub mod pace {
    use crate::supervisor::tree::TokenBucket;
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Reg = HashMap<String, (String, String)>; // key → (where it is set, rate)
    static REG: Mutex<Option<Reg>> = Mutex::new(None);
    static BUCKETS: Mutex<Option<HashMap<String, (String, TokenBucket)>>> = Mutex::new(None);

    /// Called from `from_spec` for a server carrying a `rate:`.
    pub fn register(server: &str, rate: &str) {
        let mut g = REG.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(HashMap::new).insert(
            server.to_string(),
            (format!("mcp server '{server}' rate"), rate.to_string()),
        );
    }

    /// Spend one token toward MCP server `server`, if it is rated. `Ok(())`
    /// when unrated or a token was available; `Err(msg)` (a refusal the
    /// caller reports as a tool error, never a crash) when the bucket is dry.
    pub fn take(server: &str) -> Result<(), String> {
        let (at, rate) = {
            let g = REG.lock().unwrap_or_else(|e| e.into_inner());
            match g.as_ref().and_then(|m| m.get(server)) {
                Some((a, r)) => (a.clone(), r.clone()),
                None => return Ok(()),
            }
        };
        take_rated(&format!("mcp:{server}"), &at, &rate).map_err(|retry| {
            format!("{at} exceeded ({rate} paces this process); retry in ~{retry}s")
        })
    }

    /// Spend one token from the bucket `key`, made from `rate` (set at `at`)
    /// on first use and remade when the rate changes. `Err` carries the
    /// seconds until the next token.
    pub fn take_rated(key: &str, at: &str, rate: &str) -> Result<(), u32> {
        let Ok((burst, per_s)) = crate::supervisor::tree::parse_rate(rate) else {
            // Validation refuses an unparseable rate; one that got past it
            // paces nothing rather than stopping every call.
            let _ = at;
            return Ok(());
        };
        let mut g = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.get_or_insert_with(HashMap::new);
        let slot = m.entry(key.to_string()).or_insert_with(|| {
            (
                rate.to_string(),
                TokenBucket::new(burst, f64::from(burst) / per_s),
            )
        });
        if slot.0 != rate {
            *slot = (
                rate.to_string(),
                TokenBucket::new(burst, f64::from(burst) / per_s),
            );
        }
        if slot.1.try_take() {
            Ok(())
        } else {
            Err((per_s / f64::from(burst.max(1))).ceil().max(1.0) as u32)
        }
    }

    /// Wait for a token from the bucket `key` (see [`take_rated`]): for the
    /// calls that are paced rather than refused — a model call.
    pub fn wait_rated(key: &str, at: &str, rate: &str) {
        while let Err(retry) = take_rated(key, at, rate) {
            std::thread::sleep(std::time::Duration::from_millis(
                (u64::from(retry) * 1000).min(250),
            ));
        }
    }
}

/// A `prompts/get` result's text: the text parts of its messages, a blank
/// line between messages.
pub fn prompt_text(messages: &[serde_json::Value]) -> String {
    use serde_json::Value;
    let mut out = String::new();
    for m in messages {
        let text = match m.get("content").unwrap_or(&Value::Null) {
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&text);
        }
    }
    out
}

/// Name in `meta` the principal an MCP request is made for — the authority
/// of the work, never a label a caller wrote — as its scope
/// ([`::mcp::http::SCOPE_META`]): a request signer that holds per-person
/// credentials (AAuth's Case C tokens) presents and earns them for that
/// principal alone. The transport takes it out before the request is sent.
pub(crate) fn scope(meta: &mut serde_json::Value, principal: Option<&str>) {
    if let (Some(p), Some(m)) = (principal, meta.as_object_mut()) {
        m.insert(::mcp::http::SCOPE_META.into(), serde_json::json!(p));
    }
}

/// The `_meta` every `tools/call` on a connection carries, whichever process
/// dialled it — the reactor at startup or on reload, or a subagent: the run id
/// and instance, so a backing service can recognize a retried call and
/// deduplicate its side effect. One builder, because each dial site keeping
/// its own copy is how a subagent dropped the instance.
///
/// The trace is not here: it is per request ([`from_spec`]), since a
/// connection serves many units of work.
pub fn tool_meta(run_id: &str, instance: &str) -> serde_json::Value {
    serde_json::json!({"agent/run_id": run_id, "agent/instance": instance})
}

/// Name, in a request's `_meta`, the unit of work it is made for: a span of
/// its own under `unit`'s, with the unit's flags and `tracestate` — returned,
/// so the caller can record the request as that span. A request with none
/// gets the process trace ([`from_spec`]).
pub fn stamp_trace(
    meta: &mut serde_json::Value,
    unit: Option<&crate::obs::trace::TraceContext>,
) -> Option<crate::obs::trace::TraceContext> {
    let call = unit.map(crate::obs::trace::TraceContext::child)?;
    if let Some(m) = meta.as_object_mut() {
        m.extend(call.meta());
    }
    Some(call)
}

/// The W3C trace context of one MCP request that names none of its own: the
/// process's span ([`crate::obs::trace::process`]) — in the supervisor what
/// belongs to no unit of work (a boot-time list, a re-dial, a subscription
/// the connection holds for every waiter, an MCP Events poll), in a turn
/// worker or a subagent the unit it serves. That span is exported (the
/// supervisor's at exit, a worker's as its `invoke_agent`), so the server's
/// span for the request has its parent in the collector. A request made for
/// a unit of work names a span of its own under the unit's, and that wins.
pub(crate) fn process_request_meta() -> serde_json::Map<String, serde_json::Value> {
    crate::obs::trace::process()
        .map(|t| t.meta())
        .unwrap_or_default()
}

/// Build an MCP client from a declared [`crate::config::McpServerSpec`]: resolve
/// its secret-free `{{secret:…}}` auth header templates (via [`auth`]) and connect
/// to the spec's remote `endpoint`, stamping agentd's client identity. The
/// config/auth-coupled counterpart of the crate's transport-only
/// [`client::McpClient::connect`]. Call `initialize` on the result before use.
/// The workload label every MCP session carries in `clientInfo.title`
/// (RFC-0028 §3.3 consumer presence) — the instance name, set once at startup.
static WORKLOAD: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Set the workload label sessions announce; first caller wins (idempotent).
pub fn set_workload_label(label: &str) {
    let _ = WORKLOAD.set(label.to_string());
}

/// The workload label sessions announce, once it is set.
#[cfg(feature = "companies")]
pub(crate) fn workload_label() -> Option<String> {
    WORKLOAD.get().cloned()
}

pub fn from_spec(
    spec: &crate::config::McpServerSpec,
    timeout: std::time::Duration,
) -> Result<client::McpClient, client::McpError> {
    use client::{McpClient, McpError};
    // A Company's server is dialled by the supervisor alone, with the
    // Session's Bearer token (`companies::mcp`): a spec that reached anyone
    // else is refused here, before any socket opens.
    if spec.company.is_some() {
        return Err(McpError::Transport(format!(
            "mcp server '{}' is a Company's, reached only through the supervisor",
            spec.name
        )));
    }
    // A delegated server is reached with an enrolled person's minted token,
    // by the supervisor's pool alone (RFC 0044 §5.7): never here, by anyone.
    if spec.delegated {
        return Err(McpError::Transport(format!(
            "mcp server '{}' is delegated (grant: id_jag), reached only through the supervisor",
            spec.name
        )));
    }
    if spec.endpoint.trim().is_empty() {
        return Err(McpError::Transport(format!(
            "mcp server '{}' has no endpoint",
            spec.name
        )));
    }
    let headers = auth::resolve_headers(&spec.headers).map_err(McpError::Transport)?;
    // AAuth: sign requests to this server with the agent identity.
    // Per-server opt-in — `spec.aauth == Some(false)` opts out; otherwise the
    // global default is "sign all when an identity is configured". The signing
    // path is absent entirely without `--features aauth`.
    #[cfg(feature = "aauth")]
    let aauth_signer = if spec.aauth == Some(false) {
        None
    } else {
        let s = crate::aauth::signer();
        // Learn the server's discovery metadata (content-digest requirement)
        // once at connect (best-effort). Only when we will actually sign it.
        if s.is_some()
            && let Some(client) = crate::aauth::installed()
        {
            let authority = ::mcp::http::authority_of(&spec.endpoint);
            client.discover(&authority, &spec.endpoint);
        }
        s
    };
    #[cfg(not(feature = "aauth"))]
    let aauth_signer: Option<std::sync::Arc<dyn ::mcp::http::RequestSigner>> = None;

    // Credential precedence: the unified `auth:` block wins (static / oauth2
    // device-login / client-credentials), then the narrower `oauth:`
    // client-credentials shortcut, then per-server AAuth signing. An endpoint
    // presents exactly one mechanism — they are never combined. The `auth:` and
    // `oauth:` paths are absent without `--features oauth`.
    #[cfg(feature = "oauth")]
    let signer: Option<std::sync::Arc<dyn ::mcp::http::RequestSigner>> = if let Some(a) = &spec.auth
    {
        // A server keys its credential cache as `mcp:<name>`.
        let target = format!("mcp:{}", spec.name);
        crate::auth::device::signer_for(a, &target, timeout).map_err(McpError::Transport)?
    } else if let Some(o) = &spec.oauth {
        Some(std::sync::Arc::new(oauth::OAuthBearerSigner::new(
            &format!("mcp:{}", spec.name),
            o.clone(),
            timeout,
        )) as std::sync::Arc<dyn ::mcp::http::RequestSigner>)
    } else {
        aauth_signer
    };
    #[cfg(not(feature = "oauth"))]
    let signer = aauth_signer;
    // A rated server is paced within THIS process; registered here because
    // every process builds its clients through this one function.
    if let Some(rate) = &spec.rate {
        pace::register(&spec.name, rate);
    }
    let client = McpClient::connect_signed(&spec.name, &spec.endpoint, headers, timeout, signer)?
        .with_client_info(::mcp::wire::Implementation {
            name: "agentd".into(),
            version: crate::VERSION.into(),
            title: WORKLOAD.get().cloned(),
        })
        .with_request_meta(std::sync::Arc::new(process_request_meta))
        // A server may answer a call with an MCP task unless the operator
        // said `tasks: off` (RFC 0050).
        .with_tasks(spec.tasks.is_auto());
    // SPIFFE X.509-SVID mTLS: set the transport client identity from the
    // SPIRE-written cert + key when the server declares `auth: {kind: spiffe,
    // svid: x509}`. Needs `--features tls` (mTLS); a JWT-SVID rides the signer seam.
    #[cfg(feature = "tls")]
    let client = match spiffe_x509_identity(spec)? {
        Some(id) => client.with_identity(id),
        None => client,
    };
    // A worker starts the way the supervisor reached the server.
    let client = match spec.first_request {
        Some(first) => client.start_with(first),
        None => client,
    };
    Ok(client)
}

/// Build a mutual-TLS client identity from a `kind: spiffe, svid: x509` auth
/// block (the SPIRE-written cert + key files). `None` for any other auth.
#[cfg(feature = "tls")]
fn spiffe_x509_identity(
    spec: &crate::config::McpServerSpec,
) -> Result<Option<crate::net::tls::ClientIdentity>, client::McpError> {
    use client::McpError;
    let Some(a) = &spec.auth else {
        return Ok(None);
    };
    if a.kind != "spiffe" || a.svid.as_deref() != Some("x509") {
        return Ok(None);
    }
    let cert_path = a
        .svid_file
        .as_deref()
        .ok_or_else(|| McpError::Transport("spiffe x509: svid_file is required".into()))?;
    let key_path = a
        .key_file
        .as_deref()
        .ok_or_else(|| McpError::Transport("spiffe x509: key_file is required".into()))?;
    let cert = std::fs::read(cert_path)
        .map_err(|e| McpError::Transport(format!("spiffe svid_file: {e}")))?;
    let key = std::fs::read(key_path)
        .map_err(|e| McpError::Transport(format!("spiffe key_file: {e}")))?;
    crate::net::tls::ClientIdentity::from_pem(&cert, &key)
        .map(Some)
        .map_err(|e| McpError::Transport(format!("spiffe svid: {e}")))
}

// Auth material resolution for remote MCP endpoints: materialize secret-free
// `{{secret:…}}` header templates into wire headers at connect time.
pub mod auth;
pub mod elicit;
// OAuth 2.1 client-credentials (M2M) token source for endpoints sitting behind an
// OAuth gateway. Feature-gated; dependency-free.
#[cfg(feature = "oauth")]
pub mod oauth;
// Built-in Streamable HTTP mock MCP server (the hidden `--internal-mock-mcp-http`
// mode) for the test + conformance suites: serves a one-resource reactive MCP on
// a loopback port, so the harness drives agentd's real HTTP transport end to end.
// In debug it is always present (so `cargo test` needs no flag); in release it
// ships only under `internal-mocks`, so the production binary carries no test
// scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock_events;
#[cfg(all(
    any(feature = "internal-mocks", debug_assertions),
    feature = "mcp-events",
    feature = "tls"
))]
pub mod mock_hooks;
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock_http;
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock_skills;
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock_tasks;
// The tool that answers as an x402 resource server would (`mock.paid`).
#[cfg(all(any(feature = "internal-mocks", debug_assertions), feature = "x402"))]
pub mod mock_x402;
// Where x402 rides on MCP: a tool's payment request, the paid call's `_meta`,
// the settlement (RFC 0049 §7.1).
pub mod x402;

// agentd-as-A2A-client: the remote-A2A-agent delegation backend. Reads a
// declared peer's card, sends the objective, follows the task to a terminal
// state and returns the distillate (the module doc has the whole sequence). The
// serving side of A2A is `runtime::a2a_server`.
#[cfg(feature = "a2a")]
pub mod a2a_client;

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn a_prompts_text_is_its_messages_text_parts() {
        assert_eq!(
            super::prompt_text(&[
                json!({"content": "a"}),
                json!({"content": {"type": "text", "text": "b"}}),
                json!({"content": [{"type": "text", "text": "c"}, {"type": "image"}, {"type": "text", "text": "d"}]}),
                json!({"content": {"type": "image"}}),
            ]),
            "a\n\nb\n\nc\nd"
        );
    }
}
