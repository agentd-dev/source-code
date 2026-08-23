// SPDX-License-Identifier: AGPL-3.0-only
//! The **JSON Schema (Draft 2020-12) of the v2 settings document** (RFC 0030
//! §3) — hand-written (no `schemars`, the moat) and kept faithful to
//! [`super::Settings`] by the drift tests in `super::tests`. It is the single
//! source for the path bindings (env `AGENTD_<PATH>` names, `--<path>` flags,
//! `--help`), for `--config-schema=2`, and for agentctl's admission validation.
//!
//! Conventions: every object is `additionalProperties: false` (mirrors
//! `deny_unknown_fields`); durations are strings (`10m`, `500ms`, bare seconds
//! also accepted); secrets are strings that MUST be `{{secret:…}}` /
//! `{{secret-file:…}}` references when they come from a file (§5).

use serde_json::{Map, Value, json};

/// The schema's `x-agentd-contract-version` — the 2.0 config contract.
pub const SCHEMA_CONTRACT_VERSION: &str = "2.0";

/// The document version this schema describes (`config_version`).
pub const CONFIG_VERSION: &str = "2";

pub fn schema() -> Value {
    let duration = json!({ "type": ["string", "integer"], "description": "a duration: `10m`, `90s`, `500ms`, or bare seconds" });
    let secret = json!({ "type": "string", "description": "a secret — from a file it MUST be a `{{secret:NAME}}` / `{{secret-file:PATH}}` reference; env/flag values may be inline" });
    let string_map = json!({ "type": "object", "additionalProperties": { "type": "string" } });
    let tool_select = json!({
        "oneOf": [
            { "enum": ["all", "none"] },
            { "type": "array", "items": { "type": "string" } }
        ],
        "description": "`all` | `none` | a list of names"
    });
    let budget = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "windows": { "type": "array", "items": { "$ref": "#/$defs/BudgetWindow" } },
            "lifetime_tokens": { "type": "integer", "minimum": 0, "description": "hard ceiling; 0 = unbounded" },
            "scope": { "type": "array", "items": { "enum": ["instance", "run", "conversation", "principal"] } },
            "on_exhausted": { "enum": ["wait", "slow", "degrade", "refuse", "fail"] },
            "slow": { "type": "object", "additionalProperties": false, "properties": { "factor": { "type": "number", "exclusiveMinimum": 0, "maximum": 1 } } },
            "degrade": { "type": "object", "additionalProperties": false, "properties": { "model": { "type": "string" } } },
            "reserve": { "type": "object", "additionalProperties": false, "properties": {
                "estimate": { "enum": ["context", "fixed", "none"] },
                "fixed": { "type": "integer", "minimum": 0 } } }
        }
    });
    let mut properties = Map::new();
    properties.insert(
        "config_version".to_string(),
        json!({ "type": "string", "const": CONFIG_VERSION, "description": "the document version" }),
    );
    top_level_properties(
        &mut properties,
        &duration,
        &secret,
        &string_map,
        &tool_select,
        &budget,
    );
    let mut defs = Map::new();
    defs_properties(&mut defs, &secret, &string_map, &budget, &duration);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("https://agentd.dev/schema/config/{SCHEMA_CONTRACT_VERSION}"),
        "x-agentd-contract-version": SCHEMA_CONTRACT_VERSION,
        "title": "agentd settings (v2)",
        "description": "agentd 2.0 configuration document (YAML or JSON; several files merge in order; every path is also AGENTD_<PATH> and --<path>)",
        "type": "object",
        "additionalProperties": false,
        "properties": Value::Object(properties),
        "$defs": Value::Object(defs)
    })
}

#[allow(clippy::too_many_arguments)]
fn top_level_properties(
    m: &mut Map<String, Value>,
    duration: &Value,
    secret: &Value,
    string_map: &Value,
    tool_select: &Value,
    budget: &Value,
) {
    m.insert("agent".to_string(), json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "name": { "type": "string", "description": "instance identity (falls back to the downward-API instance, then the hostname)" },
                    "instruction": { "type": "string", "description": "static text, or a single-token URI a configured MCP server serves (read + subscribed)" },
                    "prompt": { "type": "string", "description": "a one-shot task (--prompt): with no workflows configured the generated run executes this, while `instruction` stays the standing policy (the run's system prompt)" },
                    "preflight": { "enum": ["never", "auto", "always"] },
                    "wake_on": { "type": "array", "items": { "enum": ["a2a_message", "human_reply", "subagent_result", "workflow_finished", "workflow_failed", "instruction_updated", "budget_resumed"] } },
                    "on_workflow_finished": { "enum": ["ignore", "note", "think"] },
                    "tools": { "type": "object", "additionalProperties": false, "properties": {
                        "internal": tool_select, "mcp": tool_select, "code": tool_select } },
                    "max_parallel_turns": { "type": "integer", "minimum": 1 },
                    "conversation_budget": budget,
                    "ask_human_fallback": { "enum": ["wait", "pause", "idle", "fail", "finish", "stop", "auto"], "description": "what ask_human does with no human channel (and, for auto, on an unanswered gate timeout): wait (park until timeout), fail (default), or auto (an LLM judge answers on the operator's behalf, marked as auto)" },
                "approval": { "enum": ["ask", "auto", "accept"], "description": "whether a gate asks a person (ask), lets an LLM judge decide (auto), or takes the ask's recommendation (accept); runtime-settable via config.set" }
                }
            }));
    m.insert("intelligence".to_string(), json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "endpoints": { "oneOf": [ { "type": "array", "items": { "type": "string" } }, { "type": "string" } ], "description": "ordered endpoint list (failover); one comma-separated string is accepted" },
                    "model": { "type": "string" },
                    "dialect": { "enum": ["openai", "anthropic", "bedrock"], "description": "wire dialect; bedrock = native Amazon Bedrock Converse (pair with auth.kind=aws)" },
                    "token": secret,
                    "token_file": { "type": "string" },
                    "headers": string_map,
                    "auth": { "$ref": "#/$defs/Auth" },
                    "swap_policy": { "enum": ["finish-on-old", "restart-turn"] },
                    "structured_output": { "enum": ["auto", "json_schema", "tool", "prompt"] },
                    "budget": budget,
                    "pricing": { "type": "object", "additionalProperties": { "$ref": "#/$defs/Pricing" } },
                    "timeout": duration
                }
            }));
    m.insert(
        "mcp".to_string(),
        json!({
            "type": "object", "additionalProperties": false,
            "properties": {
                "servers": { "type": "array", "items": { "$ref": "#/$defs/McpServer" } },
                "default_timeout": duration
            }
        }),
    );
    m.insert("tools".to_string(), json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "disabled": { "type": "array", "items": { "type": "string" } },
                    "overrides": { "type": "object", "additionalProperties": { "$ref": "#/$defs/ToolOverride" } }
                }
            }));
    m.insert("store".to_string(), json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "kind": { "enum": ["mcp", "http", "file", "memory", "none"] },
                    "prefix": { "type": "string" },
                    "mcp": { "$ref": "#/$defs/StoreMcp" },
                    "http": { "$ref": "#/$defs/StoreHttp" },
                    "file": { "$ref": "#/$defs/StoreFile" },
                    "checkpoint": { "type": "object", "additionalProperties": false, "properties": { "debounce_ms": { "type": "integer", "minimum": 0 } } },
                    "retention": { "type": "object", "additionalProperties": false, "properties": {
                        "runs": { "type": "object", "additionalProperties": false, "properties": {
                            "keep_last": { "type": "integer", "minimum": 0, "description": "keep at most this many terminal runs" },
                            "ttl": duration } } } },
                    "durability": { "type": "object", "additionalProperties": false, "properties": {
                        "a2a": { "enum": ["strict", "eventual"] }, "steps": { "enum": ["strict", "eventual"] },
                        "work": { "enum": ["durable", "ephemeral"], "description": "default durability CLASS for runs + subagent records: ephemeral = nothing persists unless a workflow/spawn says durable: true (the fast path); default durable" } } },
                    "on_error": { "enum": ["halt", "degrade"] },
                    "audit": { "type": "boolean" },
                    "timeout": duration
                }
            }));
    m.insert(
        "memory".to_string(),
        json!({ "type": "object", "additionalProperties": false, "properties": {
                "max_value_bytes": { "type": "integer", "minimum": 1 },
                "list_default_limit": { "type": "integer", "minimum": 1 } } }),
    );
    m.insert("context".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "compact_at": { "type": "number", "exclusiveMinimum": 0, "maximum": 1 },
                "keep_last": { "type": "integer", "minimum": 0 },
                "model_window": { "type": "integer", "minimum": 1, "description": "the model's context window in tokens (overrides the value inferred from intelligence.model)" },
                "plan": { "type": "object", "additionalProperties": false, "properties": { "max_items": { "type": "integer", "minimum": 1 } } },
                "cards": { "type": "array", "items": { "enum": ["workflows", "skills", "memory", "services", "streams", "signals", "peers", "templates"] },
                           "description": "which environment cards the system prompt carries; unset = all — a node overrides per step with context: {cards: [...]}" } } }));
    m.insert("knowledge".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "server": { "type": "string" },
                "auto_context": { "type": "object", "additionalProperties": false, "properties": {
                    "on": { "enum": ["turn", "never"] }, "top_k": { "type": "integer", "minimum": 1 }, "max_bytes": { "type": "integer", "minimum": 1 } } } } }));
    m.insert("search".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": { "server": { "type": "string" } } }));
    m.insert(
        "skills".to_string(),
        json!({ "type": "object", "additionalProperties": false, "properties": {
                "sources": { "type": "array", "items": { "$ref": "#/$defs/SkillSource" } },
                "reference_prefix": { "type": "string" },
                "max_loaded": { "type": "integer", "minimum": 1 },
                "max_bytes": { "type": "integer", "minimum": 1 } } }),
    );
    m.insert(
        "streams".to_string(),
        json!({ "type": "object", "additionalProperties": {
        "type": "object", "additionalProperties": false, "properties": {
            "retention": { "type": "object", "additionalProperties": false, "properties": {
                "max_events": { "type": "integer", "minimum": 1 },
                "max_age": { "type": "string" } } } } } }),
    );
    m.insert(
        "services".to_string(),
        json!({ "type": "object", "additionalProperties": { "$ref": "#/$defs/Service" },
                "description": "the service catalog (RFC 0037): the named external services this deployment may use; mcp.servers entries reference entries via `service:` and may only narrow them" }),
    );
    m.insert("vars".to_string(), json!({ "type": "object", "additionalProperties": true,
                "description": "operator-defined constants; reference anywhere (and in workflows) as {{config.NAME}} — dotted paths reach nested values, unresolved references refuse startup" }));
    m.insert("workflows".to_string(), json!({ "type": "array", "items": { "$ref": "#/$defs/WorkflowRef" }, "description": "inline dialect-3 definitions or {name, file|uri} references" }));
    m.insert("limits".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "max_runs": { "type": "integer", "minimum": 1 },
                "run": { "type": "object", "additionalProperties": false, "properties": {
                    "steps": { "type": "integer", "minimum": 1 }, "tokens": { "type": "integer", "minimum": 1 }, "deadline": duration } },
                "subagents": { "type": "object", "additionalProperties": false, "properties": {
                    "depth": { "type": "integer", "minimum": 0 }, "breadth": { "type": "integer", "minimum": 1 },
                    "total": { "type": "integer", "minimum": 1 }, "rate": { "type": "string", "description": "`<burst>/<per>s`, e.g. `8/2s`" },
                    "instances": { "type": "object", "additionalProperties": false, "description": "instance-tier children (RFC 0036): defaults 2 live / 8 lifetime / 4/1h", "properties": {
                        "breadth": { "type": "integer", "minimum": 1 }, "total": { "type": "integer", "minimum": 1 }, "rate": { "type": "string" } } } } },
                "inline_max_bytes": { "type": "integer", "minimum": 1 },
                "step_timeout": duration,
                "workflow": { "type": "object", "additionalProperties": false, "properties": {
                    "fan_out": { "type": "integer", "minimum": 1, "description": "max concurrent lanes a foreach/batch body may use; a definition asking for more is refused at load" } } } } }));
    m.insert("lifecycle".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "run_until": { "enum": ["auto", "idle", "drained"] },
                "idle_grace": duration,
                "drain_timeout": duration,
                "run_id": { "type": "string" },
                "exit_code_map": { "type": "object", "additionalProperties": { "type": "integer", "minimum": 0, "maximum": 255 }, "description": "remap the policy exit codes (3/7 only): {\"3\": N, \"7\": N}" },
                "watch_config": { "type": "boolean" },
                "until_signal": { "type": "string", "description": "delivery of this signal begins graceful shutdown — the RFC 0036 instance-child retirement trigger" } } }));
    m.insert("subagents".to_string(), json!({ "type": "object", "additionalProperties": false,
                "description": "subagent templates + spawn policy (RFC 0036): operator-declared definitions the model may instantiate, filling declared params only",
                "properties": {
                "allow_freeform": { "type": "boolean", "description": "false = templates are the ONLY spawn path (freeform flat-tier instruction spawns are refused); default true" },
                "defaults": { "type": "object", "additionalProperties": false, "description": "applied to every spawn unless overridden at the template or call site", "properties": {
                    "model": { "type": "string" }, "priority": { "enum": ["low", "normal", "high"] },
                    "mode": { "enum": ["sync", "async", "detached", "warm"] },
                    "limits": { "type": "object", "additionalProperties": true },
                    "durable": { "type": "boolean", "description": "default durability class for spawns (false = memory-only records)" } } },
                "templates": { "type": "object", "additionalProperties": { "$ref": "#/$defs/SubagentTemplate" } } } }));
    m.insert("a2a".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "listen": { "type": "string", "description": "https://host:port (loopback http:// for dev)" },
                "tls": { "type": "object", "additionalProperties": false, "properties": {
                    "cert": { "type": "string" }, "key": { "type": "string" }, "client_ca": { "type": "string" } } },
                "bearer": secret,
                "principals": { "type": "array", "items": { "$ref": "#/$defs/Principal" } },
                "peers": { "type": "array", "items": { "$ref": "#/$defs/A2aPeer" } },
                "conversation_ttl": duration,
                "push": { "type": "object", "additionalProperties": false,
                    "description": "push notifications: a caller registers a webhook and agentd POSTs its task's updates there. Default-OFF — the URL comes from a peer, so making the request at all is the operator's decision.",
                    "properties": {
                    "enabled": { "type": "boolean", "description": "accept CreateTaskPushNotificationConfig and deliver on transitions" },
                    "allow_private": { "type": "boolean", "description": "permit webhook targets on private / loopback addresses (a separate and larger decision — a peer could otherwise reach agentd's own surfaces or a cloud metadata endpoint)" } } } } }));
    m.insert("interface".to_string(), json!({ "type": "object", "additionalProperties": false,
                "description": "The display-client (TUI/web-UI) surface (RFC 0032), served on the A2A listener. Default-OFF.",
                "properties": {
                "enabled": { "type": "boolean", "description": "serve the interface methods (SubscribeToEvents, interface.info, …)" },
                "debug": { "type": "boolean", "description": "expose extra debug information (transcripts, run step detail, the log ring, audit feed events); runtime-togglable via the config.set op" },
                "origins": { "type": "array", "items": { "type": "string" }, "description": "extra allowed browser origins (scheme://host[:port]) for a hosted web UI; loopback origins never need listing" },
                "display": { "type": "object", "additionalProperties": false,
                    "description": "what clients render in their chrome — ordered item lists for the top (header) and bottom (status bar) edges; unknown items are skipped",
                    "properties": {
                    "top": { "type": "array", "items": { "type": "string" } },
                    "bottom": { "type": "array", "items": { "type": "string" } } } },
                "pairing": { "type": "object", "additionalProperties": false,
                    "description": "pairing-code login: a rotating 6-digit code (shown to operators) a client exchanges for a session token — the low-friction alternative to copying a bearer",
                    "properties": {
                    "enabled": { "type": "boolean" },
                    "role": { "enum": ["operator", "user", "agent", "anonymous"], "description": "the role a paired session gets (operator or user; default operator)" },
                    "ttl": { "type": ["string", "integer"], "description": "session-token lifetime (default 12h)" } } } } }));
    m.insert("webhooks".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "listen": { "type": "string", "description": "https://host:port (loopback http:// for dev) — the inbound webhook surface" },
                "tls": { "type": "object", "additionalProperties": false, "properties": {
                    "cert": { "type": "string" }, "key": { "type": "string" }, "client_ca": { "type": "string" } } },
                "default_auth": { "$ref": "#/$defs/WebhookAuth" } } }));
    m.insert("goal".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "statement": { "type": "string", "description": "the goal in natural language (the LLM judge reads it)" },
                "check": { "type": "object", "additionalProperties": false, "properties": {
                    "every": duration, "condition": { "type": "string", "description": "a cheap CEL predicate over durable state, evaluated first" }, "via": { "enum": ["both", "condition", "agent"] } } },
                "stuck_after": { "type": "integer", "minimum": 1 },
                "on_achieved": { "$ref": "#/$defs/GoalAction" },
                "on_stuck": { "$ref": "#/$defs/GoalAction" } } }));
    m.insert("observability".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "log_level": { "enum": ["trace", "debug", "info", "warn", "error"] },
                "log_content": { "type": "boolean" },
                "otel": { "type": "object", "additionalProperties": false, "properties": {
                    "endpoint": { "type": "string" }, "traces": { "type": "boolean" }, "metrics": { "type": "boolean" }, "logs": { "type": "boolean" } } },
                "metrics_addr": { "type": "string" },
                "health_file": { "type": "string" },
                "report_file": { "type": "string" },
                "events_ring": { "type": "integer", "minimum": 1 },
                "audit": { "type": "object", "additionalProperties": false, "properties": {
                    "sink": { "type": "array", "items": { "enum": ["log", "store"] } } } },
                "traceparent": { "type": "string" } } }));
    m.insert("security".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "allow_trifecta": { "type": "boolean" },
                "workflows": { "type": "object", "additionalProperties": false, "properties": {
                    "immutable": { "type": "boolean", "description": "refuse workflow.create/update/delete at runtime — definitions become read-only for the model, subagents and operators alike; loading from config/file/url/dir is unaffected" } } },
                "tls_ca": { "type": "string" },
                "aauth": { "$ref": "#/$defs/AAuth" },
                "cgroup": { "type": "object", "additionalProperties": false, "properties": {
                    "spec": { "type": "string" }, "memory_max": { "type": "string" }, "pids_max": { "type": "string" } } },
                "exec": { "type": "object", "additionalProperties": false,
                    "description": "The guarded local command runner (default-OFF; needs --features exec).", "properties": {
                    "enabled": { "type": "boolean" },
                    "allow": { "type": "array", "items": { "type": "string" }, "description": "allow-listed command names (argv[0])" },
                    "workdir": { "type": "string" }, "timeout": duration,
                    "max_output": { "type": "integer" },
                    "env": { "type": "array", "items": { "type": "string" }, "description": "env var names passed through" } } },
                "egress": { "enum": ["open", "closed"], "description": "closed = an outbound MCP dial whose URL matches no services: catalog entry is refused (RFC 0037 §5); default open" } } }));
}

fn defs_properties(
    m: &mut Map<String, Value>,
    secret: &Value,
    string_map: &Value,
    budget: &Value,
    duration: &Value,
) {
    m.insert("BudgetWindow".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["per"], "properties": {
                "per": { "enum": ["second", "minute", "hour", "day", "week"] },
                "tokens": { "type": "integer", "minimum": 1 },
                "requests": { "type": "integer", "minimum": 1 },
                "reset": { "type": "string", "pattern": "^[0-9]{2}:[0-9]{2}Z$", "description": "calendar-window reset time (UTC), e.g. 00:00Z" } } }));
    m.insert("Pricing".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "input_per_1k": { "type": "number", "minimum": 0 }, "output_per_1k": { "type": "number", "minimum": 0 }, "currency": { "type": "string" } } }));
    m.insert("McpServer".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["name"],
                "oneOf": [ { "required": ["endpoint"] }, { "required": ["service"] } ],
                "properties": {
                "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" },
                "endpoint": { "type": "string" },
                "service": { "type": "string", "description": "reference a services: catalog entry — inherit its connection settings (restating endpoint/auth/headers is refused) and narrow its tool ceiling (RFC 0037)" },
                "ns": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$", "description": "tool namespace prefix (`ns.tool`)" },
                "headers": string_map,
                "tags": { "type": "object", "additionalProperties": { "type": "array", "items": { "enum": ["untrusted_input", "sensitive", "egress"] } } },
                "allow": { "type": "array", "items": { "type": "string" }, "description": "admit only advertised tools matching these globs" },
                "exclude": { "type": "array", "items": { "type": "string" }, "description": "never admit advertised tools matching these globs (beats allow)" },
                "aauth": { "type": "boolean" },
                "oauth": { "type": "object", "additionalProperties": false, "required": ["token_url", "client_id", "client_secret"], "properties": {
                    "token_url": { "type": "string" }, "client_id": { "type": "string" }, "client_secret": secret, "scope": { "type": "string" } } },
                "auth": { "$ref": "#/$defs/Auth" },
                "timeout": duration } }));
    m.insert("Auth".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["kind"],
                "description": "A unified credential provider (RFC 0031).", "properties": {
                "kind": { "enum": ["static", "oauth2", "aws", "spiffe"] },
                "issuer": { "type": "string" }, "token_url": { "type": "string" },
                "device_authorization_url": { "type": "string" }, "authorization_url": { "type": "string" },
                "client_id": { "type": "string" }, "client_secret": secret,
                "grant": { "enum": ["device", "authorization_code", "client_credentials"] },
                "scopes": { "type": "array", "items": { "type": "string" } }, "audience": { "type": "string" },
                "token": secret, "header": { "type": "string" }, "value": secret,
                "region": { "type": "string" }, "service": { "type": "string" },
                "source": { "enum": ["env", "static", "imds", "irsa", "sso"] },
                "sso_start_url": { "type": "string" }, "account_id": { "type": "string" }, "role_name": { "type": "string" },
                "svid": { "enum": ["jwt", "x509"] }, "jwt_svid_file": { "type": "string" },
                "svid_file": { "type": "string" }, "key_file": { "type": "string" } } }));
    m.insert("ToolOverride".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["server", "tool"], "properties": {
                "server": { "type": "string" }, "tool": { "type": "string" },
                "args": { "type": "string", "description": "a JSON template or `CEL: …` producing the MCP tool arguments from `args`/`ctx`" },
                "result": { "type": "string", "description": "a JSON pointer / template / `CEL: …` mapping the CallToolResult to the internal output schema" } } }));
    m.insert("WebhookAuth".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "hmac": { "type": "object", "additionalProperties": false, "properties": {
                    "secret": secret, "header": { "type": "string", "description": "the header carrying the signature (default X-Signature)" }, "algo": { "enum": ["sha256"] }, "prefix": { "type": "string", "description": "a prefix stripped before the constant-time compare, e.g. sha256=" } } },
                "bearer": secret,
                "header": { "type": "object", "additionalProperties": false, "properties": { "name": { "type": "string" }, "equals": secret } },
                "none": { "type": "boolean", "description": "loopback-only, no auth (dev) — explicit opt-in" } } }));
    m.insert("GoalAction".to_string(), json!({ "oneOf": [
                { "enum": ["finish", "idle", "replan", "escalate"] },
                { "type": "object", "additionalProperties": false, "required": ["workflow"], "properties": { "workflow": { "type": "string" } } } ] }));
    m.insert("StoreOp".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["tool"], "properties": {
                "tool": { "type": "string" }, "args": { "type": "string" }, "ok": { "type": "string" }, "conflict": { "type": "string" },
                "value": { "type": "string" }, "keys": { "type": "string" } } }));
    m.insert("StoreMcp".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["server"], "properties": {
                "server": { "type": "string" },
                "put": { "$ref": "#/$defs/StoreOp" }, "get": { "$ref": "#/$defs/StoreOp" },
                "list": { "$ref": "#/$defs/StoreOp" }, "delete": { "$ref": "#/$defs/StoreOp" } } }));
    m.insert("HttpOp".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["url"], "properties": {
                "method": { "enum": ["GET", "PUT", "POST", "DELETE"] }, "url": { "type": "string" }, "body": { "type": "string" },
                "value": { "type": "string" }, "keys": { "type": "string" }, "conflict_status": { "type": "integer", "minimum": 100, "maximum": 599 } } }));
    m.insert("StoreHttp".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["base_url"], "properties": {
                "base_url": { "type": "string" }, "headers": string_map,
                "get": { "$ref": "#/$defs/HttpOp" }, "put": { "$ref": "#/$defs/HttpOp" },
                "list": { "$ref": "#/$defs/HttpOp" }, "delete": { "$ref": "#/$defs/HttpOp" } } }));
    // RFC 0033 §4: the only knob is where the state lives, and it is optional —
    // an omitted `path` resolves through $AGENTD_STATE_DIR / $XDG_STATE_HOME.
    m.insert("StoreFile".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "min_free": { "type": "string", "description": "shed new work below this much free disk (256MB, 1.5GiB, bytes; 0 disables); warn at twice it" },
                "path": { "type": "string", "description": "the state root; default $AGENTD_STATE_DIR, else $XDG_STATE_HOME/agentd/state, else $HOME/.local/state/agentd/state, else the OS temp dir" } } }));
    m.insert("SkillSource".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["server"], "properties": {
                "server": { "type": "string" }, "discover": { "enum": ["prompts", "resources", "auto"] }, "filter": { "type": "string" } } }));
    m.insert("WorkflowRef".to_string(), json!({ "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string" }, "armed": { "type": "boolean" },
                "file": { "type": "string", "description": "a path on disk" },
                "uri": { "type": "string", "description": "an MCP resource (mcp://<server>/<uri>, or one a connected server serves)" },
                "url": { "type": "string", "description": "fetched over HTTP(S) at startup; fail-closed if unreachable" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" }, "description": "headers for `url` — credential values must be {{secret:…}} references" },
                "timeout": duration,
                "allow_private": { "type": "boolean", "description": "permit `url` to resolve to a private/loopback address" },
                "dir": { "type": "string", "description": "load every matching file in a directory" },
                "glob": { "type": "string", "description": "comma-separated globs relative to `dir` (default `*.yaml,*.yml,*.json`); `**` recurses" } },
                "additionalProperties": true,
                "description": "a {name, file|uri|url} reference, a {dir, glob} directory, or an inline dialect-3 definition (RFC 0027)" }));
    m.insert("Principal".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["match", "role"], "properties": {
                "match": { "type": "object", "additionalProperties": false, "properties": {
                    "san": { "type": "string" }, "sub": { "type": "string" }, "bearer_ref": { "type": "string" }, "aauth_agent": { "type": "string" }, "any": { "type": "boolean" } } },
                "role": { "enum": ["operator", "user", "agent", "anonymous"] },
                "grants": { "type": "array", "items": { "type": "string" } },
                "quotas": { "type": "object", "additionalProperties": false, "properties": {
                    "rate": { "type": "string" }, "budget": budget } } } }));
    m.insert("SubagentTemplate".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["instruction"],
                "description": "an operator-declared subagent definition (RFC 0036): `instruction` is a full RFC 0034 document — no config-defining directives = the flat worker; machinery (:::workflow/:::mcp/:::stream/:::config/:::tools) = an instance-tier child",
                "properties": {
                "instruction": { "type": "string", "description": "the definition; {{params.X}} holes fold in at spawn as data, never re-parsed for directives" },
                "params": { "type": "object", "additionalProperties": { "$ref": "#/$defs/ParamSpec" }, "description": "the ONLY holes the model may fill, schema-validated at spawn" },
                "servers": { "type": "array", "items": { "type": "string" }, "description": "flat tier: narrowing server grants from the parent's set" },
                "tools": { "type": "array", "items": { "type": "string" }, "description": "flat tier: narrowing tool grants" },
                "limits": { "type": "object", "additionalProperties": true, "description": "flat tier: the per-spawn limits object; instance tier: OS caps only (memory, cpu)" },
                "mode": { "enum": ["sync", "async", "detached", "warm"], "description": "instance tier supports detached only (phase A)" },
                "model": { "type": "string" }, "priority": { "enum": ["low", "normal", "high"] },
                "skills": { "type": "array", "items": { "type": "string" } },
                "context": { "type": "array", "items": { "type": "object" } },
                "output_contract": { "type": "string" }, "output_schema": { "type": "object", "additionalProperties": true },
                "budget": budget,
                "ttl": { "type": ["string", "integer"], "description": "instance tier: retire after this long (graceful drain)" },
                "until": { "type": "string", "description": "instance tier: a signal name (templated over params) whose delivery in the child retires it" },
                "singleton": { "type": "boolean", "description": "one live child; its A2A peer alias is the template name" },
                "durable": { "type": "boolean", "description": "false = memory-only record (an instance child runs on a memory store; no restore-respawn); absent = the store.durability.work default" } } }));
    m.insert("ParamSpec".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "type": { "enum": ["string", "number", "integer", "boolean"] },
                "required": { "type": "boolean" },
                "default": {},
                "enum": { "type": "array" },
                "description": { "type": "string" } } }));
    m.insert("Service".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["endpoint"],
                "description": "a service-catalog entry (RFC 0037 §4): connection settings, one shared credential, authoritative trifecta tags (a floor for any matching endpoint), and a tool-surface ceiling consumers can only narrow",
                "properties": {
                "kind": { "enum": ["mcp"], "description": "phase A: mcp only (intelligence/peer/http reserved)" },
                "endpoint": { "type": "string", "description": "the connection URL and the dial-time match base (scheme + authority + path prefix)" },
                "headers": string_map,
                "tags": { "type": "object", "additionalProperties": { "type": "array", "items": { "enum": ["untrusted_input", "sensitive", "egress"] } },
                          "description": "authoritative — unioned into any consumer whose endpoint matches, referencing or inline, open or closed" },
                "allow": { "type": "array", "items": { "type": "string" }, "description": "the CEILING: the widest advertised-tool surface any consumer may get" },
                "exclude": { "type": "array", "items": { "type": "string" }, "description": "never admitted, unioned into every consumer (beats allow)" },
                "auth": { "$ref": "#/$defs/Auth" },
                "rate": { "type": "string", "description": "per-instance pacing toward the service (`<burst>/<per>`, e.g. `60/1m`)" },
                "timeout": duration } }));
    m.insert("A2aPeer".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["name", "endpoint"], "properties": {
                "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" }, "endpoint": { "type": "string" },
                "headers": string_map, "client_cert": { "type": "string" }, "client_key": { "type": "string" },
                "auth": { "$ref": "#/$defs/Auth" } } }));
    m.insert("AAuth".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["provider"], "properties": {
                "provider": { "type": "string" }, "key_file": { "type": "string" }, "enroll_token": secret,
                "enroll_assertion_file": { "type": "string" }, "person_server": { "type": "string" } } }));
}
