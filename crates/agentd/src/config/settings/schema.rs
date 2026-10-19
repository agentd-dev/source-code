// SPDX-License-Identifier: AGPL-3.0-only
//! The **JSON Schema (Draft 2020-12) of the settings document** — hand-written
//! (no `schemars`, the moat) and held faithful to [`super::Settings`] by the
//! tests in `super::tests`, which walk both shapes object by object. It is the
//! single source for the path bindings (env `AGENTD_<PATH>` names, `--<path>`
//! flags, `--help`), for `--config-schema`, and for agentctl's admission
//! validation, so a section added here reaches all of them at once.
//!
//! Conventions: every object is `additionalProperties: false` (mirrors
//! `deny_unknown_fields`); durations are strings (`10m`, `500ms`, bare seconds
//! also accepted); secrets are strings that MUST be `{{secret:…}}` /
//! `{{secret-file:…}}` references when they come from a file, so a config
//! document is always safe to commit.

use serde_json::{Map, Value, json};

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
            "lifetime_exhausted": { "enum": ["drain", "refuse", "exit"], "description": "what the PROCESS does once lifetime_tokens is spent: drain (default — finish live work, exit as a drain does, 0 for a daemon, let an orchestrator restart with a fresh window), refuse (keep refusing every admission and stay up), exit (stop now, exit 7)" },
            "on_exhausted": { "enum": ["wait", "slow", "degrade", "refuse", "fail"] },
            "slow": { "type": "object", "additionalProperties": false, "properties": { "factor": { "type": "number", "exclusiveMinimum": 0, "maximum": 1 } } },
            "degrade": { "type": "object", "additionalProperties": false, "properties": { "model": { "type": "string" } } }
        }
    });
    let mut properties = Map::new();
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
        // The URL this document is SERVED at, so an editor that fetches the
        // `$id` gets this schema and not a 404.
        "$id": "https://agentd.dev/schema/config.json",
        "title": "agentd configuration",
        "description": "agentd configuration document (YAML or JSON; several files merge in order; every path is also AGENTD_<PATH> and --<path>)",
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
    // Hoisted out of the `agent` literal below: that `json!` sits at the
    // macro's recursion limit, and inlining one more object of this size tips
    // it over. Same trick as `duration` / `budget` / `tool_select`.
    let instruction_trust = json!({ "type": "array",
                            "description": "WHO may sign this document (§7.5): publisher + author/delivery keys + a per-source capability ceiling + a revocation deadline, per document. NOT a source — `file`/`dir`/`url`/`oci`/`mcp` above are the sources; these say who may sign what they serve. Operator surface only; restart-only.",
                            "items": { "type": "object", "additionalProperties": false, "properties": {
                "uri": { "type": "string", "description": "the document this pin applies to (instruction://…)" },
                "publisher": { "type": "string", "description": "the publisher the author signature must claim" },
                "author_keys": { "type": "array", "items": { "type": "string" }, "description": "author (offline) verification keys: key file paths (raw/hex/base64url Ed25519), or instruction://…keys.json JWKS uris fetched from the serving registry; empty + publisher set = discover from the read's publisherKeys" },
                "delivery_keys": { "type": "array", "items": { "type": "string" }, "description": "delivery (online) verification keys — same forms as author_keys" },
                "reader": { "type": "string", "description": "this consumer's reader id for the delivery aud check (§7.6 step 2), e.g. principal://… or agent://…; delivery verification runs only when set" },
                "max_capabilities": { "type": "array", "items": { "enum": ["material", "knowledge", "interface", "identity", "compute", "infra", "compose"] }, "description": "the per-source ceiling; effective families never exceed it" },
                "freshness": { "type": "string", "description": "the revocation re-check deadline (a duration, e.g. 15m)" } } } });
    m.insert("agent".to_string(), json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "name": { "type": "string", "description": "instance identity (falls back to the downward-API instance, then the hostname)" },
                    "description": { "type": "string", "description": "what this agent does, in a sentence — the public Agent Card's description. Operator-only: a served document cannot rewrite what the unauthenticated card claims" },
                    "instruction": { "oneOf": [
                    { "type": "string", "description": "short form: the instruction itself, a FILE path (no whitespace, path-shaped or a document extension), a DIRECTORY (path-shaped, trailing `/` or an existing folder), or a URI (`oci://`, `mcp://`, `instruction://`, `https://`). Every other setting takes its default." },
                    { "type": "object", "additionalProperties": false, "description": "long form: name the source explicitly and set everything about it here", "properties": {
                        "text": { "type": "string", "description": "the instruction itself, never read as a path or URI" },
                        "file": { "type": "string", "description": "a path on disk; watched when lifecycle.watch_config is on" },
                        "oci": oci_source(),
                        "url": { "type": "string", "description": "an https:// document, fetched at load (same key a workflow entry uses)" },
                        "dir": { "oneOf": [ { "type": "string", "description": "the folder path" }, { "type": "object", "additionalProperties": false, "required": ["path"], "properties": { "path": { "type": "string" }, "glob": { "type": "string", "description": "comma-separated globs relative to `path` (default `*.md,*.markdown,*.txt,*.instruction`); `**` recurses" }, "order": { "enum": ["name", "date"], "description": "name (default, path order) or date (mtime, oldest first)" } } } ], "description": "a folder of documents, combined into ONE document — `glob` and `order` live inside it because they qualify it and nothing else" },
                        "mcp": instruction_mcp_source(),
                        "refresh": { "type": "string", "description": "how often to re-read: `auto` (default — inotify for a file, never for a digest-pinned artifact, 5m for a mutable tag or served resource), `off`, or a duration" },
                        "unavailable": { "enum": ["auto", "keep", "freeze", "drain", "exit"], "description": "when the source stops answering after startup: auto (freeze when trust-pinned, else keep), keep, freeze (refuse new work), drain (finish live work then exit, 0 for a daemon), exit" },
                        "unenforceable": { "enum": ["warn", "refuse", "ignore"], "description": "what a `trust` pin that names no key file means at startup — `author_keys` that are instruction:// JWKS uris, or none, need the registry client a file/dir/url/oci load does not have: warn (default), refuse (exit 2), ignore. A key file that cannot be read always refuses (exit 2)" },
                        "trust": instruction_trust,
                        "decrypt": { "type": "object", "additionalProperties": false, "description": "recipient keys for an encrypted envelope (RFC 0041)", "properties": {
                            "keys": { "type": "array", "items": { "type": "string" }, "description": "key FILE paths — an AGE-SECRET-KEY-1… identity, 64 hex chars, or base64" },
                            "passphrase": { "type": "string", "description": "for age scrypt envelopes — a {{secret:…}} reference" } } } } }
                ] },
                    "prompt": { "oneOf": [
                    { "type": "string", "description": "short form: a task (--prompt), delivered as a message into the root conversation at every start — the text itself, a FILE path, a DIRECTORY, or an https:// document, classified exactly as `instruction` is. A job's prompt is the job, and a daemon's what it does as soon as it starts; `instruction` stays the standing policy every turn is given." },
                    { "type": "object", "additionalProperties": false, "description": "long form: name the source explicitly", "properties": {
                        "text": { "type": "string", "description": "the task itself, never read as a path or URI" },
                        "file": { "type": "string", "description": "a path on disk" },
                        "dir": { "oneOf": [ { "type": "string", "description": "the folder path" }, { "type": "object", "additionalProperties": false, "required": ["path"], "properties": { "path": { "type": "string" }, "glob": { "type": "string", "description": "comma-separated globs relative to `path` (default `*.md,*.markdown,*.txt,*.instruction`); `**` recurses" }, "order": { "enum": ["name", "date"], "description": "name (default, path order) or date (mtime, oldest first)" } } } ], "description": "a folder of documents, combined into ONE document — `glob` and `order` live inside it because they qualify it and nothing else" },
                        "url": { "type": "string", "description": "an https:// document, fetched at load" },
                        "oci": oci_source() } }
                ] },
                    "preflight": { "enum": ["never", "auto", "always"] },
                    "wake_on": { "type": "array", "items": { "enum": ["a2a_message", "human_reply", "subagent_result", "workflow_finished", "workflow_failed", "instruction_updated", "budget_resumed"] }, "description": "the events that wake (or leave a note for) the root conversation; default a2a_message, human_reply, workflow_failed. subagent_result is opt-in: it notes each child's result, and every turn of a warm child, in the root transcript — withheld there, as any read-back is, when the child may carry outside input and the root holds sensitive and egress" },
                    "on_workflow_finished": { "enum": ["ignore", "note", "think"], "description": "what a finished run leaves in the root transcript, for the runs wake_on names: ignore (default), note (a note with its output or error), or think (that note delivered, starting a turn). Withheld there, as any read-back is, when the run may carry outside input and the root holds sensitive and egress" },
                    "tools": { "type": "object", "additionalProperties": false, "properties": {
                        "internal": tool_select, "mcp": tool_select, "code": tool_select } },
                    "max_parallel_turns": { "type": "integer", "minimum": 1 },
                    "conversation_budget": budget,
                    "ask_human_fallback": { "enum": ["wait", "pause", "idle", "fail", "finish", "stop", "auto"], "description": "what ask_human does when it does not gate — no A2A listener, or an unowned ask under ask_human_unowned: fallback — and, for auto, when a gate that names no addressee times out: wait (park until timeout), fail (default), or auto (an LLM judge answers on the operator's behalf, marked as auto). An addressed gate, which every security.policies gate is, is never judged: it times out" },
                    "ask_human_unowned": { "enum": ["gate", "fallback"], "description": "what an ask no caller owns — an ask_human or a security.policies gate raised by a schedule, webhook, stream or subagent (a subagent is unowned even under a caller's turn) — does: gate (a gate task on the A2A listener, owned by the principal the unit works for or else the operator, who answers it; requires a2a.listen) or fallback (default — ask_human_fallback applies; a policy gate takes its on_timeout)" },
                "approval": { "enum": ["ask", "auto", "accept"], "description": "whether a gate asks a person (ask), lets an LLM judge decide (auto), or takes the ask's recommendation (accept); runtime-settable via admin.set" },
                "document_capabilities": { "type": "array", "items": { "enum": ["material", "knowledge", "interface", "identity", "compute", "infra", "compose"] }, "description": "instruction-document families this agent's instruction may use (the trust ladder). Empty grants only the default rung; naming a family admits its blocks. Fail-closed, restart-only." },
                "gate_channels": { "type": "object", "description": "the channel a configured workflow's gate is announced on: workflow name → step path → channel. How a subagent template's ::!human channels reach its instance child; an instruction document's own ::!human declarations stand where both name a step. Operator-only.", "additionalProperties": { "type": "object", "additionalProperties": { "type": "string", "minLength": 1 } } }
                }
            }));
    let endpoints = json!({ "oneOf": [ { "type": "array", "items": { "type": "string" } }, { "type": "string" } ], "description": "ordered endpoint list (failover); one comma-separated string is accepted" });
    let dialect = json!({ "enum": ["openai", "anthropic", "bedrock", "decisions"], "description": "wire dialect; bedrock = native Amazon Bedrock Converse (pair with auth.kind=aws); decisions = a decision model's endpoint, posted at its full URL (a provider only)" });
    let structured_output = json!({ "enum": ["auto", "prompt"], "description": "how a call whose answer has a JSON Schema is asked on this connection: auto (default) — natively where the dialect and the schema allow (OpenAI response_format, Anthropic output_config.format, a Claude model's on Bedrock), else in the prompt; prompt — in the prompt alone, for a server that refuses a response format. The answer is validated against the schema either way" });
    // A role that writes takes a model; one that decides, a threshold too.
    let writing_role = |what: &str| {
        json!({ "oneOf": [
        { "type": "string" },
        { "type": "object", "additionalProperties": false, "required": ["model"], "properties": {
            "model": { "type": "string" } } }
    ], "description": format!("the model {what} — a name, or {{model}}; default: roles.turn") })
    };
    let role = |what: &str| {
        json!({ "oneOf": [
        { "type": "string" },
        { "type": "object", "additionalProperties": false, "required": ["model"], "properties": {
            "model": { "type": "string" },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1, "description": "the least confidence the role's decisions are acted on with" } } }
    ], "description": format!("the model {what} — a name, or {{model, confidence}}; default: roles.turn") })
    };
    m.insert("intelligence".to_string(), json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "endpoints": endpoints,
                    "model": { "type": "string", "description": "the turn model — a declared `models:` name, or a literal model string; the same setting as roles.turn (set one)" },
                    "providers": { "type": "object", "description": "further connections, by name: a second vendor, a gateway. A model reaches one with `provider:`. Operator-only; egress-checked like intelligence.endpoints", "additionalProperties": {
                        "type": "object", "additionalProperties": false, "required": ["endpoints"], "properties": {
                            "endpoints": endpoints,
                            "dialect": dialect,
                            "token": secret,
                            "token_file": { "type": "string" },
                            "headers": string_map,
                            "auth": { "$ref": "#/$defs/Auth" },
                            "timeout": duration,
                            "structured_output": structured_output } } },
                    "models": { "type": "object", "description": "named models: cost/quality tiering inside one workflow without forking a process. A model names the connection it is reached on and cannot describe one", "additionalProperties": {
                        "type": "object", "additionalProperties": false, "required": ["model"], "properties": {
                            "kind": { "enum": ["chat", "decision", "reasoning"], "description": "chat (default) generates and emulates decisions; decision answers typed questions natively, on a `dialect: decisions` provider, and writes nothing; reasoning generates after thinking, with an effort" },
                            "reasoning": { "type": "object", "additionalProperties": false, "description": "a reasoning model's defaults; a node's or a think call's effort overrides the effort", "properties": {
                                "effort": { "enum": ["low", "medium", "high"] },
                                "budget": { "type": "integer", "minimum": 1024, "description": "thinking tokens, for the dialects that take a budget (anthropic, bedrock's Anthropic models); default by effort" } } },
                            "model": { "type": "string", "description": "the wire model name sent to the provider" },
                            "escalate": { "type": "string", "description": "a decision model's: the model an unsure answer is asked again on, once" },
                            "provider": { "type": "string", "description": "the connection: a name under intelligence.providers; default the top-level one. Operator-only" },
                            "window": { "type": "integer", "minimum": 1, "description": "this model's context window, so compaction stops guessing from the model NAME" },
                            "fallback": { "type": "string", "description": "the model used when this one is unavailable (every endpoint failed or broken open) — a ladder that walks down, across providers, instead of failing" } } } },
                    "roles": { "type": "object", "additionalProperties": false, "description": "which model each job runs on; each defaults to turn, and turn to intelligence.model", "properties": {
                        "turn": { "oneOf": [
                            { "type": "string" },
                            { "type": "object", "additionalProperties": false, "required": ["model"], "properties": { "model": { "type": "string" } } }
                        ], "description": "the model the agent's turns, agent/extract/summarize nodes and subagents run on; the same setting as intelligence.model (set one)" },
                        "think": writing_role("the think tool and think nodes run on"),
                        "decide": role("classify, route and judge nodes run on"),
                        "preflight": role("preflight triage of an inbound message runs on"),
                        "compaction": writing_role("summarising the transcript runs on"),
                        "goal": role("the goal check runs on"),
                        "approve": role("that answers asks on the operator's behalf (approval: auto) runs on. Operator-only") } },
                    "routes": { "type": "object", "maxProperties": 32, "additionalProperties": { "type": "string", "minLength": 1 }, "description": "the models a turn may run on instead of roles.turn — a declared chat or reasoning model → when to use it. Preflight chooses one per message it looks at (needs agent.preflight auto or always). Operator-only" },
                    "dialect": dialect,
                    "token": secret,
                    "token_file": { "type": "string" },
                    "headers": string_map,
                    "auth": { "$ref": "#/$defs/Auth" },
                    "swap_policy": { "enum": ["finish-on-old", "restart-turn"] },
                    "structured_output": structured_output,
                    "budget": budget,
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
                    "overrides": { "type": "object", "additionalProperties": { "$ref": "#/$defs/ToolOverride" } },
                    "narrow": { "type": "object", "additionalProperties": { "type": "object", "additionalProperties": false, "properties": {
                        "tags": { "type": "array", "items": { "enum": ["untrusted_input", "sensitive", "egress"] }, "description": "trifecta tags to ADD (never remove)" },
                        "describe": { "type": "string", "description": "operator annotation appended beneath the tool's own description" }
                    } }, "description": "append-only narrowing of an existing tool (a :::!override block): add tags, append a note; never widen" }
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
                    "retention": { "type": "object", "additionalProperties": false, "description": "what to keep once a record is finished; unset keeps everything", "properties": {
                        "runs": { "type": "object", "additionalProperties": false, "properties": {
                            "keep_last": { "type": "integer", "minimum": 0, "description": "keep at most this many terminal runs" },
                            "ttl": duration } },
                        "tasks": { "type": "object", "additionalProperties": false, "properties": {
                            "keep_last": { "type": "integer", "minimum": 0, "description": "keep at most this many terminal A2A tasks — one bound across every principal, so on a shared listener one caller's finished tasks can push out another's; prefer ttl there. A settled task nobody has read back yet is kept for at least 30s after it settles, whatever keep_last and ttl say, so a blocking SendMessage still gets its own answer" },
                            "ttl": duration } } } },
                    "durability": { "type": "object", "additionalProperties": false, "properties": {
                        "a2a": { "enum": ["strict", "eventual"] }, "steps": { "enum": ["strict", "eventual"] },
                        "work": { "enum": ["durable", "ephemeral"], "description": "default durability CLASS for runs + subagent records: ephemeral = nothing persists unless a workflow/spawn says durable: true (the fast path); default durable" } } },
                    "on_error": { "enum": ["halt", "degrade"] },
                    "timeout": duration,
                    "max_value_bytes": { "type": "integer", "minimum": 1, "description": "refuse a durable write larger than this; set it when the store's READ limit is lower than its write limit (an MCP store behind a broker often caps a tool RESULT well below its request body), so a checkpoint that could not be read back is refused at write time instead of failing the next restore" }
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
                "template": { "type": "string", "description": "the system-prompt template; unset = the built-in default, printed by `agentd --context-template`" },
                "templates": { "type": "object", "additionalProperties": { "type": "string" }, "description": "named alternates a node selects with context: {template: <name>}" },
                "summarize": { "type": "object", "additionalProperties": false, "description": "compaction's model-facing half", "properties": {
                    "prompt": { "type": "string", "description": "override the summarizer guidance; the JSON schema it must satisfy is fixed; the model it runs on is intelligence.roles.compaction" } } } } }));
    m.insert("knowledge".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "server": { "type": "string" },
                "auto_context": { "type": "object", "additionalProperties": false, "properties": {
                    "on": { "enum": ["turn", "never"] }, "top_k": { "type": "integer", "minimum": 1 }, "max_bytes": { "type": "integer", "minimum": 1 } } } } }));
    m.insert("search".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": { "server": { "type": "string" } } }));
    m.insert(
        "skills".to_string(),
        json!({ "type": "object", "additionalProperties": false, "properties": {
                "sources": { "type": "array", "items": { "$ref": "#/$defs/SkillSource" } },
                "dir": { "type": "string", "description": "a local folder of <name>/SKILL.md skills (Agent Skills, the user scope); `skills/` beside the config is adopted when this is unset, and `.agents/skills/` beside the config is read as the project scope" },
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
                "description": "the service catalog: the named external services this deployment may use; mcp.servers entries reference entries via `service:` and may only narrow them" }),
    );
    m.insert("vars".to_string(), json!({ "type": "object", "additionalProperties": true,
                "description": "operator-defined constants; reference anywhere (and in workflows) as {{config.NAME}} — dotted paths reach nested values, unresolved references refuse startup" }));
    m.insert("workflows".to_string(), json!({ "type": "array", "items": { "$ref": "#/$defs/WorkflowRef" }, "description": "inline workflow definitions, or {name, file|uri|url} / {name, dir} references" }));
    m.insert("limits".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "max_runs": { "type": "integer", "minimum": 1 },
                "run": { "type": "object", "additionalProperties": false, "properties": {
                    "steps": { "type": "integer", "minimum": 1 }, "tokens": { "type": "integer", "minimum": 1 }, "deadline": duration } },
                "subagents": { "type": "object", "additionalProperties": false, "properties": {
                    "depth": { "type": "integer", "minimum": 0 }, "breadth": { "type": "integer", "minimum": 1 },
                    "total": { "type": "integer", "minimum": 1 }, "rate": { "type": "string", "description": "`<burst>/<per>s`, e.g. `8/2s`" },
                    "instances": { "type": "object", "additionalProperties": false, "description": "instance-tier children: defaults 2 live / 8 lifetime / 4/1h", "properties": {
                        "breadth": { "type": "integer", "minimum": 1 }, "total": { "type": "integer", "minimum": 1 }, "rate": { "type": "string" } } } } },
                "inline_max_bytes": { "type": "integer", "minimum": 1 },
                "step_timeout": duration,
                "max_message_depth": { "type": "integer", "minimum": 1, "description": "how many chained `message` deliveries may run before one is refused (default 8) — the loop guard on message → turn → run → message" },
                "workflow": { "type": "object", "additionalProperties": false, "properties": {
                    "fan_out": { "type": "integer", "minimum": 1, "description": "max concurrent lanes a foreach/batch body may use; a definition asking for more is refused at load" } } } } }));
    m.insert("lifecycle".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "run_until": { "enum": ["auto", "idle", "drained"] },
                "idle_grace": duration,
                "drain_timeout": duration,
                "run_id": { "type": "string" },
                "exit_code_map": { "type": "object", "additionalProperties": { "type": "integer", "minimum": 0, "maximum": 255 }, "description": "remap the policy exit codes (3/7 only): {\"3\": N, \"7\": N}" },
                "watch_config": { "type": "boolean" },
                "until_signal": { "type": "string", "description": "delivery of this signal begins graceful shutdown — the retirement trigger a parent composes into an instance-tier child" } } }));
    m.insert("subagents".to_string(), json!({ "type": "object", "additionalProperties": false,
                "description": "subagent templates + spawn policy: operator-declared definitions the model may instantiate, filling declared params only",
                "properties": {
                "allow_freeform": { "type": "boolean", "description": "false = templates are the ONLY spawn path (freeform flat-tier instruction spawns are refused); default true" },
                "defaults": { "type": "object", "additionalProperties": false, "description": "applied to every spawn unless overridden at the template or call site", "properties": {
                    "model": { "type": "string" }, "priority": { "enum": ["low", "normal", "high"] },
                    "mode": { "enum": ["sync", "async", "detached", "warm"] },
                    "limits": { "type": "object", "additionalProperties": true },
                    "durable": { "type": "boolean", "description": "default durability class for spawns (false = memory-only records)" } } },
                "templates": { "type": "object", "additionalProperties": { "$ref": "#/$defs/SubagentTemplate" } } } }));
    // Hoisted out of the `a2a` literal for the same recursion-limit reason as
    // `instruction_trust`. The scope vocabulary is the enum's own list.
    let device_scopes: Vec<&str> = super::DeviceScope::ALL.iter().map(|s| s.as_str()).collect();
    let device_grant = json!({ "type": "object", "additionalProperties": false,
                    "description": "the OAuth 2.0 device authorization grant (RFC 8628): a client shows a code, an operator approves it, the client gets a session token. Needs an operator credential (a2a.bearer, or a principals rule with role operator and match.bearer_ref); refused with a2a.tls.client_ca and on a unix:// listener. Restart-only.",
                    "properties": {
                    "enabled": { "type": "boolean" },
                    "scopes": { "type": "array", "items": { "enum": device_scopes }, "description": "the scopes a client may request (default [user]); non-empty, no duplicates" },
                    "token_ttl": { "type": ["string", "integer"], "description": "session-token lifetime, 5m..30d (default 8h)" },
                    "code_ttl": { "type": ["string", "integer"], "description": "how long a device code waits for approval, 1m..30m (default 10m)" },
                    "verification_uri": { "type": "string", "description": "where the approving person is sent: an https:// URL, or http:// on a loopback host; unset = the listener's own page" },
                    "rate": { "type": "string", "description": "`<burst>/<per>s` applied to every session principal" } } });
    m.insert("a2a".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "listen": { "type": "string", "description": "https://host:port (loopback http:// for dev)" },
                "url": { "type": "string", "description": "the public origin callers reach this listener at (scheme://host[:port], no path, query or fragment; https unless the host is loopback) — the Agent Card's interface URL and the OAuth issuer. Required when a2a.listen binds a wildcard host (0.0.0.0, ::). Restart-only" },
                "cors": { "type": "object", "additionalProperties": false, "properties": {
                    "origins": { "type": "array", "items": { "type": "string" }, "description": "browser origins (scheme://host[:port]) allowed to call the listener, matched exactly; `*` and paths are refused, and a loopback UI origin must be listed too" } } },
                "device_grant": device_grant,
                "events": { "type": "object", "additionalProperties": false, "properties": {
                    "enabled": { "type": "boolean", "description": "declare the events extension and serve its observation feed; requires a2a.listen; restart-only" } } },
                "introspection": { "type": "object", "additionalProperties": false, "properties": {
                    "enabled": { "type": "boolean", "description": "serve the operator introspection ops (transcripts, run step detail, the log ring, audit records on the feed); requires a2a.listen; reloadable" } } },
                "tls": { "type": "object", "additionalProperties": false, "properties": {
                    "cert": { "type": "string" }, "key": { "type": "string" }, "client_ca": { "type": "string" } } },
                "bearer": secret,
                "principals": { "type": "array", "items": { "$ref": "#/$defs/Principal" } },
                "peers": { "type": "array", "items": { "$ref": "#/$defs/A2aPeer" } },
                "push": { "type": "object", "additionalProperties": false,
                    "description": "push notifications: a caller registers a webhook and agentd POSTs its task's updates there. Default-OFF — the URL comes from a peer, so making the request at all is the operator's decision.",
                    "properties": {
                    "enabled": { "type": "boolean", "description": "accept CreateTaskPushNotificationConfig and deliver on transitions" },
                    "allow_private": { "type": "boolean", "description": "permit webhook targets on private / loopback addresses (a separate and larger decision — a peer could otherwise reach agentd's own surfaces or a cloud metadata endpoint)" } } } } }));
    m.insert("webhooks".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "listen": { "type": "string", "description": "https://host:port (loopback http:// for dev) — the inbound webhook surface" },
                "url": { "type": "string", "description": "the URL the listener is reached at from outside (https://host[:port][/prefix]); callback URLs are built on it" },
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
                    "endpoint": { "type": "string" }, "logs": { "type": "boolean" } } },
                "metrics_addr": { "type": "string" },
                "health_file": { "type": "string" },
                "events_ring": { "type": "integer", "minimum": 1 },
                "audit": { "type": "object", "additionalProperties": false, "properties": {
                    "sink": { "type": "array", "items": { "enum": ["log", "store", "stream"] } },
                    "stream": { "type": "string", "description": "the declared stream `sink: [stream]` appends to — the supported path off the box, and the only sink a workflow can consume" } } },
                "runtime_events": { "type": "object", "additionalProperties": false, "properties": {
                    "stream": { "type": "string", "description": "declared stream the selected events land on" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "event families taken in full (the segment before the first dot); an unknown family is a startup error" },
                    "sampled": { "type": "array", "items": { "type": "string" }, "description": "event families taken at 1-in-16 — for high-rate families that arrive in storms" },
                    "queue": { "type": "integer", "minimum": 1, "description": "how many events may queue between ticks before the tap drops and counts (default 512)" } } },
                "traceparent": { "type": "string" },
                "status_values": { "type": "array", "items": { "type": "string" }, "description": "memory keys whose current values the status op publishes as status.values; operator-only" } } }));
    m.insert("identity".to_string(), json!({ "type": "object", "additionalProperties": false,
                "description": "who work is done ON BEHALF OF — including work nobody typed",
                "properties": {
                "autonomous_as": { "type": "string", "description": "the actor a schedule/webhook/stream firing is attributed to (default `system`); without it the attribution chain is dropped at its first hop" },
                "labels": { "type": "object", "additionalProperties": { "type": "string" }, "description": "labels stamped on autonomous work" } } }));
    m.insert("security".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "allow_trifecta": { "type": "boolean" },
                "policies": { "type": "array", "description": "ordered verdicts on a tool call; first match wins, no match is allow", "items": {
                    "type": "object", "additionalProperties": false, "properties": {
                        "match": { "type": "object", "additionalProperties": false, "properties": {
                            "tool": { "type": "string", "description": "tool-name glob; absent matches every tool" },
                            "tags": { "type": "array", "items": { "enum": ["untrusted_input", "sensitive", "egress"] }, "description": "every listed trifecta tag must be present on the tool" },
                            "caller": { "type": "array", "items": { "enum": ["root", "workflow", "subagent"] } },
                            "principal": { "type": "string", "description": "principal-id glob, for calls carrying one" },
                            "args": { "type": "string", "description": "CEL over `args`, `tool` and `caller` — the only place an ARGUMENT can be judged, since grants are name patterns" } } },
                        "action": { "enum": ["allow", "deny", "ask", "shadow"], "description": "shadow refuses and says the call was held; it never fabricates a result" },
                        "question": { "type": "string", "description": "the question put to a person for `ask`; {{tool}}, {{caller}} and {{args}} are substituted" },
                        "on_timeout": { "enum": ["allow", "deny", "ask", "shadow"], "description": "what an unanswered `ask` becomes (default deny)" },
                        "timeout": { "type": "string" },
                        "to": { "oneOf": [
                            { "type": "string", "description": "an operator addressee: the id `operator` (docs/configuration.md §12.6)" },
                            { "type": "object", "additionalProperties": false, "properties": {
                                "id": { "type": "string" }, "role": { "enum": ["operator", "user", "agent"] },
                                "labels": { "type": "object", "additionalProperties": { "type": "string" } } } } ],
                            "description": "which operator may answer an `ask` gate (only with action: ask): {role: operator} narrowed by labels, or the id `operator` — anyone else could never see the task and is refused (docs/configuration.md §12.6); unset = {role: operator}" } } } },
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
                "egress": { "enum": ["open", "closed"], "description": "closed = an outbound MCP dial whose URL matches no services: catalog entry is refused; default open" },
                "companies": companies() } }));
}

/// `security.companies` (RFC 0047): the one User, the client identity
/// every Company sees, the seal key. Needs the `companies` feature.
fn companies() -> Value {
    json!({ "type": "object", "additionalProperties": false,
        "description": "acting at Companies for one User (RFC 0047; needs the companies feature): who the User is, the client identity every Company sees, and the key the User ID records are sealed with. Restart-only, operator-only",
        "properties": {
            "user": { "type": "string", "description": "the User this instance acts for at every Company: `operator` (default; refused when a2a.principals holds more than one operator rule) or a principal id `<role>:<id>`" },
            "client": { "type": "object", "additionalProperties": false, "required": ["id", "name"], "properties": {
                "id": { "type": "string", "description": "the client_id: the HTTPS URL of the client metadata document" },
                "name": { "type": "string", "description": "client_name, shown on Companies' consent pages" },
                "logo_uri": { "type": "string", "description": "an HTTPS URL" },
                "jwks_uri": { "type": "string", "description": "the key set's HTTPS URL, on the client_id's host; default <client_id origin>/companies/jwks.json" },
                "key_file": { "type": "string", "description": "the ES256 signing key (PKCS#8), created 0600 when absent; default /var/lib/agentd/company-client-key" },
                "published_key_files": { "type": "array", "items": { "type": "string" }, "description": "keys whose public halves are in the key set but are never signed with: the next key before a rotation, the previous one after it" },
                "redirect_uri": { "type": "string", "description": "where Direct Sign-In returns the User, an HTTPS URL on the client_id's host; default <client_id origin>/companies/callback. Listed in the document's redirect_uris when a Company lists direct" },
                "serve": { "type": "boolean", "description": "serve the client metadata document and key set on the A2A listener at their URLs' paths, and the Direct Sign-In callback when a Company lists direct (needs a2a, and a2a.url the client_id's origin); otherwise publish what --print-company-client writes" },
                "extensions": { "type": "array", "items": { "enum": ["operations"] }, "description": "the extensions the client metadata document lists: operations (version 1) — a Company proposes an action, and agentd confirms it only with the User's approval of its terms or a standing permission; default none" } } },
            "seal_key": { "type": "string", "description": "32 random bytes as hex or base64, given as a secret reference: seals the Account Tokens a sign-in gives agentd, and the User ID records, so a store cannot read them. Required with a sign_in, and with a remote store (mcp, http)" },
            "context": company_context("what the messages agentd sends in a Company's conversations share of the User's situation, each field in the draft's format; nothing is shared that is not set. Needs an entry that uses `agent`"),
            "browsers": { "type": "array", "items": { "type": "string" }, "description": "browser MCP servers to keep off every Company's hosts; each names an mcp.servers entry" },
            "limits": { "type": "object", "additionalProperties": false, "properties": {
                "in_flight": { "type": "integer", "minimum": 1, "description": "Company calls in flight per Company (default 4); one more is refused until one ends" },
                "conversations": { "type": "integer", "minimum": 1, "description": "open conversations per instance, each followed by a reader (default 16); a new one is refused until one closes" } } },
            "standing": { "type": "array", "description": "standing permissions: an operation one of them covers is confirmed with approved_by standing_permission, without asking the User. Each covers one Company, one channel and every term key of the revision; never when the Company requires the User's approval. Needs client.extensions [operations] and the cel feature",
                "items": { "type": "object", "additionalProperties": false, "required": ["company", "via", "terms", "when", "expires"], "properties": {
                    "company": { "type": "string", "description": "the company entry" },
                    "via": { "type": "object", "additionalProperties": false, "description": "the channel that proposed the operation: exactly one of tool (a glob of the Company tool's registered name, shop.exchange_*) or api (an api_allow operation, METHOD /path-template); an operation proposed in a conversation never matches",
                        "properties": { "tool": { "type": "string" }, "api": { "type": "string" } } },
                    "terms": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "every term key it covers: a revision with a key outside them, or with no terms, asks the User" },
                    "when": { "type": "string", "description": "a CEL predicate over terms and summary; a term it reads that is missing is no match" },
                    "expires": { "type": "string", "description": "an RFC 3339 time it stops applying at" },
                    "uses": { "type": "integer", "minimum": 1, "description": "how many confirms it covers, each reserved before the confirm and given back when the Company refuses it; default no limit but expires" },
                    "accounts": { "type": "array", "minItems": 1, "items": { "type": "string", "pattern": "^[a-z0-9_-]{1,32}$" }, "description": "the labels of the User's accounts at the Company it covers, as a sign-in names them (agentd's own words, never sent); default the account labelled default alone, and the signed-out Session — so signing in to a second account never widens it" } } } } } })
}

/// A conversation's `context` (RFC 0047 §5.5.7): `security.companies.context`
/// and a company entry's own.
fn company_context(description: &str) -> Value {
    json!({ "type": "object", "additionalProperties": false, "description": description,
        "properties": {
            "locale": { "type": "string", "description": "a language tag (RFC 5646), such as en-US" },
            "time_zone": { "type": "string", "description": "an IANA time zone name, such as America/Los_Angeles" },
            "share_availability": { "type": "boolean", "description": "share user_available: whether the User has a client attached to agentd now (default false)" } } })
}

/// `agent.instruction.mcp`: a served resource, and the MCP Events types
/// whose occurrences re-read it.
fn instruction_mcp_source() -> Value {
    json!({ "oneOf": [ { "type": "string" }, { "type": "object", "additionalProperties": false, "required": ["resource"], "properties": { "server": { "type": "string", "description": "which configured MCP server to ask; omitted = whichever one serves it" }, "resource": { "type": "string", "description": "the resource URI, e.g. instruction://ins_1@stable" }, "events": { "type": "array", "description": "MCP Events types the instruction's server offers whose occurrences mean the instruction may have changed: each is polled for, and an occurrence re-reads the instruction (at most once a second), beside the resource subscription and the refresh timer. A server that does not offer one refuses it, said once in mcp.events.refused; the instruction is still followed the other ways. Requires the mcp-events feature.", "items": { "oneOf": [ { "type": "string", "minLength": 1, "description": "the event type, as the server's events/list names it" }, { "type": "object", "additionalProperties": false, "required": ["name"], "properties": { "name": { "type": "string", "minLength": 1 }, "arguments": { "type": "object", "description": "the subscription arguments, checked against the type's inputSchema" } } } ] } } } } ], "description": "a resource a configured MCP server serves, read and subscribed — the URI alone, or {server, resource, events} when it matters which server is asked or the instruction follows its updates by MCP Events" })
}

/// One `oci:` node everywhere a document can come from: the agent's own
/// instruction, a one-shot prompt and a subagent template all pull through
/// the same resolver, so they take the same cosign key.
fn oci_source() -> Value {
    json!({ "oneOf": [
        { "type": "string", "description": "an OCI artifact — ghcr.io/acme/agent:v3 (the oci:// is implied)" },
        { "type": "object", "additionalProperties": false, "required": ["ref"], "properties": {
            "ref": { "type": "string", "description": "the artifact reference; `@sha256:…` pins it immutably" },
            "cosign_key": { "type": "string", "description": "public key FILE (PEM `PUBLIC KEY`, P-256 or Ed25519) the artifact's cosign signature must verify against — who PUSHED it, as distinct from `trust`, which pins who WROTE the document" } } }
    ], "description": "an OCI artifact: the reference, or {ref, cosign_key} to verify the artifact signature too" })
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
    m.insert("McpServer".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["name"],
                "oneOf": [ { "required": ["endpoint"] }, { "required": ["service"] } ],
                "properties": {
                "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" },
                "endpoint": { "type": "string" },
                "service": { "type": "string", "description": "reference a services: catalog entry — inherit its connection settings (restating endpoint/auth/headers is refused) and narrow its tool ceiling" },
                "ns": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$", "description": "tool namespace prefix (`ns.tool`)" },
                "headers": string_map,
                "tags": { "type": "object", "additionalProperties": { "type": "array", "items": { "enum": ["untrusted_input", "sensitive", "egress"] } } },
                "allow": { "type": "array", "items": { "type": "string" }, "description": "admit only advertised tools matching these globs" },
                "exclude": { "type": "array", "items": { "type": "string" }, "description": "never admit advertised tools matching these globs (beats allow)" },
                "aauth": { "type": "boolean" },
                "oauth": { "type": "object", "additionalProperties": false, "required": ["token_url", "client_id", "client_secret"], "properties": {
                    "token_url": { "type": "string" }, "client_id": { "type": "string" }, "client_secret": secret, "scope": { "type": "string" } } },
                "auth": { "$ref": "#/$defs/Auth" },
                "timeout": duration,
                "tasks": { "enum": ["auto", "off"], "description": "whether the server may answer a tool call with an MCP task (the Tasks extension, SEP-2663): auto (default) declares the extension on 2026-07-28 connections, on the calls that have a bound (a turn's, a workflow step's, a store adapter's), off never does, so the server must answer every call inline (RFC 0050)" },
                "events": { "type": "array", "items": { "$ref": "#/$defs/EventBinding" }, "description": "MCP Events bindings (RFC 0045): each event type subscribed with fixed arguments, every occurrence appended to a declared stream; requires the mcp-events build feature" },
                "event_grants": { "type": "array", "items": { "$ref": "#/$defs/EventGrant" }, "description": "event types runtime subscriptions may be made for — by a wait that subscribes or the events.* tools — and what each of them gets fixed (RFC 0045 §5.15)" } } }));
    let event_into = json!({ "type": "object", "additionalProperties": false, "required": ["stream", "subject"], "properties": {
                "stream": { "type": "string", "description": "a stream declared under streams:" },
                "subject": { "type": "string" } } });
    let delivery = json!({ "type": "array", "minItems": 1, "items": { "enum": ["poll", "webhook"] }, "description": "the acceptable delivery modes, in preference order" });
    let replay = json!({ "type": ["string", "integer"], "description": "how far back to catch up after downtime (maxAgeMs): a duration, or `none` to always start from now; default 1h" });
    let event_rate = json!({ "type": "string", "description": "intake ceiling `<burst>/<per>`; required on an untagged or untrusted_input server" });
    m.insert("EventBinding".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["name", "delivery", "into"], "properties": {
                "name": { "type": "string", "description": "the event type, as the server's events/list names it" },
                "arguments": { "type": "object", "description": "subscription arguments, validated against the type's inputSchema" },
                "delivery": delivery.clone(),
                "into": event_into.clone(),
                "replay": replay.clone(),
                "rate": event_rate.clone(),
                "ttl": duration } }));
    m.insert("EventGrant".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["name", "delivery", "into"], "properties": {
                "name": { "type": "string", "description": "the event type, as the server's events/list names it" },
                "fixed": { "type": "object", "description": "arguments no runtime subscription may set" },
                "delivery": delivery,
                "into": event_into,
                "replay": replay,
                "rate": event_rate,
                "ttl": duration,
                "max": { "type": "integer", "minimum": 1, "maximum": 64, "description": "live runtime subscriptions under this grant; default 8" },
                "lifetime": duration,
                "tools": { "type": "boolean", "description": "offer the events.* tools for this event type" } } }));
    m.insert("Auth".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["kind"],
                "description": "A unified credential provider.", "properties": {
                "kind": { "enum": ["static", "oauth2", "aws", "spiffe"] },
                "issuer": { "type": "string" }, "token_url": { "type": "string" },
                "device_authorization_url": { "type": "string" }, "authorization_url": { "type": "string" },
                "client_id": { "type": "string" }, "client_secret": secret,
                "client_auth": { "type": "object", "additionalProperties": false,
                    "description": "how the client authenticates at the token and revocation endpoints", "properties": {
                    "method": { "enum": ["private_key_jwt", "client_secret_basic", "client_secret_post"] },
                    "key": secret, "kid": { "type": "string" },
                    "assertion_aud": { "enum": ["token_endpoint", "issuer"] } } },
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
    // The file store deliberately exposes one knob — where the state lives —
    // and even that is optional: an omitted `path` resolves through
    // $AGENTD_STATE_DIR / $XDG_STATE_HOME, so durability needs no config.
    m.insert("StoreFile".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "min_free": { "type": "string", "description": "shed new work below this much free disk (256MB, 1.5GiB, bytes; 0 disables); warn at twice it" },
                "path": { "type": "string", "description": "the state root; default $AGENTD_STATE_DIR, else $XDG_STATE_HOME/agentd/state, else $HOME/.local/state/agentd/state, else the OS temp dir" } } }));
    m.insert("SkillSource".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["server"], "properties": {
                "server": { "type": "string", "description": "a declared MCP server whose Skills extension (io.modelcontextprotocol/skills) skills are read from; not `local`, the origin a reference names the operator's own skills by" },
                "filter": { "type": "string", "description": "one skill name, or a prefix ending in *" },
                "dynamic": { "enum": ["refuse", "allow"], "description": "load a skill whose content is dynamic, which cannot be verified (default refuse)" } } }));
    // A `workflows[]` entry is either a REFERENCE (`file`/`uri`/`url`/`dir`) or
    // an inline definition — and most people write them inline, which is where
    // an editor's completion earns its keep. So the workflow document's own
    // properties are folded in beside the reference fields, from the same
    // `KINDS`-derived schema the validator uses. Without this an inline
    // workflow was `additionalProperties: true`: no completion for `steps`, no
    // node kinds, and a typo caught only at startup.
    //
    // Merged rather than expressed as a `oneOf` on purpose. A `oneOf` would
    // let the schema also catch "you gave both `file` and `steps`" — which the
    // loader already refuses with a better message ("one entry, one source") —
    // at the cost of ambiguous completion in every editor, since neither
    // branch matches a half-written entry. Completion is the job here.
    let workflow_doc = crate::engine::model::workflow_schema();
    let mut wf_ref = json!({ "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string" }, "armed": { "type": "boolean" },
                "file": { "type": "string", "description": "a path on disk" },
                "uri": { "type": "string", "description": "an MCP resource (mcp://<server>/<uri>, or one a connected server serves)" },
                "url": { "type": "string", "description": "fetched over HTTP(S) at startup; fail-closed if unreachable" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" }, "description": "headers for `url` — credential values must be {{secret:…}} references" },
                "timeout": duration,
                "allow_private": { "type": "boolean", "description": "permit `url` to resolve to a private/loopback address" },
                "dir": { "oneOf": [ { "type": "string" }, { "type": "object", "additionalProperties": false, "required": ["path"], "properties": { "path": { "type": "string" }, "glob": { "type": "string", "description": "comma-separated globs relative to `path` (default `*.yaml,*.yml,*.json`); `**` recurses" }, "order": { "enum": ["name", "date"], "description": "name (default, path order) or date (mtime, oldest first)" } } } ], "description": "load every matching file in a directory; `glob` and `order` live inside the object form" } },
                "additionalProperties": false,
                "description": "a {name, file|uri|url} reference, a {dir} directory (a path, or {path, glob, order}), or an inline workflow definition" });
    if let (Some(dst), Some(src)) = (
        wf_ref["properties"].as_object_mut(),
        workflow_doc.get("properties").and_then(Value::as_object),
    ) {
        for (k, v) in src {
            // The reference fields win where the names collide (`name`,
            // `armed`, `description`): those are the config layer's own.
            dst.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    m.insert("WorkflowRef".to_string(), wf_ref);
    // The workflow document's own `$defs` (`step`, `kinds`) come along, so the
    // `#/$defs/step` references inside the folded properties still resolve.
    if let Some(src) = workflow_doc.get("$defs").and_then(Value::as_object) {
        for (k, v) in src {
            m.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    m.insert("Principal".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["match", "role"], "properties": {
                "id": { "type": "string", "pattern": "^[A-Za-z0-9._@:/+-]{1,128}$", "description": "the principal id this rule's callers act as (<role>:<id>) and own their work by; unique across rules. Required for bearer_ref and any rules; without it a certificate rule derives <role>:cn=<CN> or <role>:san=<first SAN>" },
                "match": { "type": "object", "additionalProperties": false, "properties": {
                    "san": { "type": "string" }, "sub": { "type": "string" }, "bearer_ref": { "type": "string" }, "any": { "type": "boolean" } } },
                "role": { "enum": ["operator", "user", "agent", "anonymous"] },
                "grants": { "type": "array", "items": { "type": "string" } },
                "quotas": { "type": "object", "additionalProperties": false, "properties": {
                    "rate": { "type": "string", "description": "`<burst>/<per>s` arrival quota; operators are exempt" }, "budget": budget } },
                "labels": { "type": "object", "additionalProperties": { "type": "string" }, "description": "operator-declared attributes carried into the run, the MCP `_meta` and the audit line" } } }));
    m.insert("SubagentTemplate".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["instruction"],
                "description": "an operator-declared subagent definition: `instruction` is a full instruction document — no config-defining directives = the flat worker; machinery (:::workflow/:::mcp/:::stream/:::config/:::tools) = an instance-tier child",
                "properties": {
                "instruction": { "oneOf": [
                    { "type": "string", "description": "the definition; {{params.X}} holes fold in at spawn as data, never re-parsed for directives. A path-shaped value or a URI names the document instead of being it — the same classification agent.instruction uses." },
                    { "type": "object", "additionalProperties": false, "description": "name the source explicitly: the load-time sources agent.instruction takes", "properties": {
                        "text": { "type": "string", "description": "the definition itself, never read as a path or URI" },
                        "file": { "type": "string", "description": "a path on disk" },
                        "dir": { "oneOf": [ { "type": "string", "description": "the folder path" }, { "type": "object", "additionalProperties": false, "required": ["path"], "properties": { "path": { "type": "string" }, "glob": { "type": "string", "description": "comma-separated globs relative to `path` (default `*.md,*.markdown,*.txt,*.instruction`); `**` recurses" }, "order": { "enum": ["name", "date"], "description": "name (default, path order) or date (mtime, oldest first)" } } } ], "description": "a folder of documents, combined into ONE document — `glob` and `order` live inside it because they qualify it and nothing else" },
                        "url": { "type": "string", "description": "an https:// document, fetched at load" },
                        "oci": oci_source() } }
                ] },
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
                "durable": { "type": "boolean", "description": "false = memory-only record (an instance child runs on a memory store; no restore-respawn); absent = the store.durability.work default" },
                "result": { "type": "object", "additionalProperties": false, "required": ["workflow"], "properties": { "workflow": { "type": "string" } },
                            "description": "instance mode: sync — resolve the spawn when the child's named workflow first completes, returning its output (needs a parent A2A listener)" },
                "mirror_streams": { "type": "array", "items": { "type": "string" },
                            "description": "child streams mirrored into the parent's same-named streams (declared on both sides; needs a parent A2A listener)" } } }));
    m.insert(
        "ParamSpec".to_string(),
        json!({ "type": "object", "additionalProperties": false, "properties": {
                "type": { "enum": ["string", "number", "integer", "boolean"] },
                "required": { "type": "boolean" },
                "default": {},
                "enum": { "type": "array" },
                "description": { "type": "string" } } }),
    );
    m.insert("Service".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["endpoint"],
                "description": "a service-catalog entry: connection settings, one shared credential, authoritative trifecta tags (a floor for any matching endpoint), and a tool-surface ceiling consumers can only narrow",
                "properties": {
                "kind": { "enum": ["mcp", "intelligence", "peer", "http", "company"],
                          "description": "which surface this entry serves; one host may appear under several kinds with different trust budgets. `company` (RFC 0047, the companies feature): a Company agentd acts at for its User — `endpoint` is the https://<host> whose discovery document is read" },
                "endpoint": { "type": "string", "description": "the connection URL and the dial-time match base (scheme + authority + path prefix)" },
                "headers": string_map,
                "tags": { "type": "object", "additionalProperties": { "type": "array", "items": { "enum": ["untrusted_input", "sensitive", "egress"] } },
                          "description": "authoritative — unioned into any consumer whose endpoint matches, referencing or inline, open or closed" },
                "allow": { "type": "array", "items": { "type": "string" }, "description": "the CEILING: the widest advertised-tool surface any consumer may get" },
                "exclude": { "type": "array", "items": { "type": "string" }, "description": "never admitted, unioned into every consumer (beats allow)" },
                "auth": { "$ref": "#/$defs/Auth" },
                "rate": { "type": "string", "description": "per-instance pacing toward the service (`<burst>/<per>`, e.g. `60/1m`)" },
                "timeout": duration,
                "methods": { "type": "array", "items": { "type": "string" },
                             "description": "`kind: http` only — the METHOD ceiling for `http` steps against this entry (e.g. [GET, POST]); absent = any method" },
                "breaker": { "type": "object", "additionalProperties": false,
                             "properties": { "failures": { "type": "integer", "minimum": 1 }, "cooldown": duration },
                             "description": "`kind: mcp` only — a default breaker policy for `mcp.tool` steps against this entry; a step's own `breaker:` wins" },
                "hosts": { "type": "array", "items": { "type": "string" },
                           "description": "`kind: company` only — host globs (`example.com`, `*.example.com`) every endpoint the Company's documents name must match; default the endpoint's host, the document's organization.domain, their subdomains and the issuer's host" },
                "allow_private": { "type": "boolean", "description": "`kind: company` only — admit loopback, private and link-local addresses (an on-premises Company); default false" },
                "interfaces": { "type": "array", "items": { "enum": ["mcp", "agent", "openapi"] }, "description": "`kind: company` only — the Company's interfaces agentd uses: mcp (its MCP servers), agent (conversations with its Company Agent, through company.converse and the company.<name> stream), openapi (its OpenAPI APIs, through company.api); default [mcp]" },
                "mcp_names": { "type": "object", "additionalProperties": { "type": "string" }, "description": "`kind: company` only — a server name for each MCP API url the Company lists, when it lists more than one; with one, the server is named after the entry" },
                "privacy": { "enum": ["strict", "standard"], "description": "`kind: company` only — strict (default) or standard, which sends agentd's usual correlators and is logged at boot" },
                "sign_in": { "type": "array", "items": { "enum": ["device", "direct", "mediated"] },
                             "description": "`kind: company` only — the sign-in types agentd may start with the User, in the order it tries them: device (a link and a code) or direct (the Company's page, back to the served callback; needs client.serve). mediated is refused. Needs scopes and security.companies.seal_key" },
                "scopes": { "type": "array", "items": { "type": "string" },
                            "description": "`kind: company` only — the most account scopes agentd asks the User for there; a sign-in asks for what is needed within these and what the sign-in type grants" },
                "api_allow": { "type": "array", "items": { "type": "string" },
                               "description": "`kind: company` only — the OpenAPI operations agentd may call there, each `METHOD /path-template` as the description's paths name it (`GET /orders/{order_id}`); one outside them, or outside the description, is refused unsent. Each one the description declares is also a tool of its own, `<server>.<method>_<path>` (`shop.get_orders_order_id`). Needs `openapi` in interfaces; default none" },
                "api_names": { "type": "object", "additionalProperties": { "type": "string" }, "description": "`kind: company` only — a server name for each OpenAPI API url the Company lists, under which its operation tools are named, when it lists more than one or the entry's name is taken; with one, the server is named after the entry. Needs api_allow" },
                "context": company_context("`kind: company` only — what this Company's conversations share of the User's situation, each field over security.companies.context. Needs `agent` in interfaces") } }));
    m.insert("A2aPeer".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["name", "endpoint"], "properties": {
                "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" }, "endpoint": { "type": "string" },
                "headers": string_map, "client_cert": { "type": "string" }, "client_key": { "type": "string" },
                "service": { "type": "string", "description": "a `services:` entry of `kind: peer` supplying the endpoint, auth and tags" },
                "auth": { "$ref": "#/$defs/Auth" } } }));
    m.insert("AAuth".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["provider"], "properties": {
                "provider": { "type": "string" }, "key_file": { "type": "string" }, "enroll_token": secret,
                "enroll_assertion_file": { "type": "string" }, "person_server": { "type": "string" } } }));
}
