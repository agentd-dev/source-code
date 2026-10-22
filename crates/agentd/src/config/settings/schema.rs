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
                    "workspace": { "type": "string", "description": "the operator's workspace, a directory: its AGENTS.md files, from its root down to agentd's working directory when that is inside it (root first, the nearest last), reach the system prompt as text below the instruction (RFC 0053). UTF-8, at most 32768 bytes together, a symlink followed only inside the workspace; read again on reload. Operator-only" },
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
    m.insert("spawn".to_string(), json!({ "type": "object", "additionalProperties": false,
                "description": "what every child process — turn worker, subagent, think child, instance — is started with: beyond its allow-listed environment, the baseline (PATH, HOME, TMPDIR, TZ, LANG, AGENTD_CRED_DIR, RUST_LOG, NO_PROXY, XDG_*, LC_*, SSL_CERT_*, OTEL_*, *_PROXY) and the variables its own secret references and routed aws credentials read, the passthrough (reloadable); and the uid and gid a worker runs as (restart-only). Operator-only",
                "properties": {
                "env_passthrough": { "type": "array", "items": { "type": "string" }, "description": "variables every child also gets, each a name or a PREFIX* pattern; one a supervisor-only secret is read from is refused" },
                "uid": { "type": "integer", "minimum": 1, "description": "the uid every worker runs as instead of agentd's — turn workers, subagents, think children, an instance child's own workers, exec commands — with gid and no supplementary group; needs root, or CAP_SETUID and CAP_SETGID (instance-tier templates need root), and is refused at load where agentd cannot. Restart-only" },
                "gid": { "type": "integer", "minimum": 1, "description": "the gid every worker runs as, with uid. Restart-only" },
                "instance_uid": { "type": "integer", "minimum": 1, "description": "the uid every instance child (a whole daemon a template spawns) runs as instead of agentd's, with instance_gid and no supplementary group: its directory is made its own, it answers its parent only over the channel it is handed at spawn, and its own workers run as it. Needs agentd to run as root; never agentd's uid, a signer's or spawn.uid. Restart-only" },
                "instance_gid": { "type": "integer", "minimum": 1, "description": "the gid every instance child runs as, with instance_uid. Restart-only" } } }));
    // Hoisted out of the `a2a` literal for the same recursion-limit reason as
    // `instruction_trust`. The scope vocabulary is the enum's own list.
    let device_scopes: Vec<&str> = super::DeviceScope::ALL.iter().map(|s| s.as_str()).collect();
    let inbound_tokens = inbound_tokens_schema();
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
                "resource": { "type": "string", "description": "the resource identifier inbound access tokens are issued for (RFC 0044 §5.9; needs the a2a and oauth features): every token's aud, and the resource of the listener's RFC 9728 protected-resource metadata, served at its path-inserted well-known URL. An https:// URI in canonical form on the listener's origin. Restart-only" },
                "inbound_tokens": inbound_tokens,
                "login": { "type": "object", "additionalProperties": false, "required": ["idp"],
                    "description": "the IdP-federated display-client sign-in (RFC 0044 §5.4; needs the xaa feature): a display client signs in at the IdP by authorization code with PKCE through a single-use start link on the listener origin, its session is user:oidc:<iss>#<sub>, and where a template admits it the sign-in is its enrolment. Beside the device grant or alone: either way the listener serves the session store, /oauth2/revoke and its metadata, declares the bearer scheme and grants no implicit operator. Refused with a2a.tls.client_ca and on a unix:// listener. Restart-only",
                    "properties": {
                    "idp": { "type": "string", "description": "the identity.providers entry people sign in at; its sign_in.grants must include authorization_code" },
                    "token_ttl": { "type": ["string", "integer"], "description": "session-token lifetime, 5m..30d (default 8h)" } } },
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
                    "stream": { "type": "string", "description": "the declared stream `sink: [stream]` appends to — the supported path off the box, and the only sink a workflow can consume" },
                    "chain": { "type": "object", "additionalProperties": false, "required": ["path"],
                        "description": "the hash-chained audit log (RFC 0044 P2a): every audit record appended to a file only the supervisor writes, each line carrying the SHA-256 of the line before, verified from the oldest kept record to the head the store keeps at every start (a chain that does not hold refuses the start); the tombstone register is anchored in it. Written whatever sink says. Restart-only",
                        "properties": {
                        "path": { "type": "string", "description": "an absolute path; closed segments are kept beside it as <path>.<first seq>" },
                        "rotate_at": { "type": "string", "description": "the size the open segment is closed at (64MiB default, at least 64KiB)" },
                        "retention": { "type": ["string", "integer"], "description": "how long a closed segment is kept after its last record (default 400d); each segment's header carries the register, so retention forgets no revocation" } } } } },
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
                "labels": { "type": "object", "additionalProperties": { "type": "string" }, "description": "labels stamped on autonomous work" },
                "providers": { "type": "object", "additionalProperties": provider(), "description": "the enterprise identity providers agentd is a client of (RFC 0044; needs the xaa feature): where an enrolled person signs in, and where their grant is exchanged for an ID-JAG. Restart-only, operator-only" },
                "delegation": delegation() } }));
    m.insert("security".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "allow_trifecta": { "type": "boolean" },
                "policies": { "type": "array", "description": "ordered verdicts on a tool call; first match wins, no match is allow", "items": {
                    "type": "object", "additionalProperties": false, "properties": {
                        "match": { "type": "object", "additionalProperties": false, "properties": {
                            "tool": { "type": "string", "description": "tool-name glob; absent matches every tool" },
                            "tags": { "type": "array", "items": { "enum": ["untrusted_input", "sensitive", "egress"] }, "description": "every listed trifecta tag must be present on the tool" },
                            "caller": { "type": "array", "items": { "enum": ["root", "workflow", "subagent"] } },
                            "principal": { "type": "string", "description": "principal-id glob, for calls carrying one" },
                            "args": { "type": "string", "description": "CEL over `args`, `tool` and `caller` — the only place an ARGUMENT can be judged, since grants are name patterns" },
                            "delegated": { "type": "boolean", "description": "the call is made with an enrolled person's grant (RFC 0044)" },
                            "tainted": { "type": "boolean", "description": "the work making the call has read outside text: an untrusted_input tool's result, a delegated server's (unless trusted_output), a webhook's or a non-operator's A2A message, a peer's answer, knowledge context, a served instruction, a tainted memory entry" },
                            "holds_delegated_data": { "type": "boolean", "description": "a delegated server's result has reached the work making the call" },
                            "any_tags": { "type": "array", "items": { "enum": ["untrusted_input", "sensitive", "egress"] }, "description": "at least one listed trifecta tag is on the tool" },
                            "unattended": { "type": "boolean", "description": "nobody is there to answer: a trigger started the work (a schedule, a webhook, a stream), not a principal's request" },
                            "document_sourced": { "type": "boolean", "description": "the call is made in a run of a workflow an instruction document defined — for a delegated call, one the person's template does not list for the server by name and digest (document_workflows)" },
                            "mode": { "enum": ["agent", "delegated"], "description": "whose authority the work acts with: an enrolled person's grant (delegated) or agentd's own (agent)" } } },
                        "action": { "enum": ["allow", "deny", "ask", "shadow"], "description": "shadow refuses and says the call was held; it never fabricates a result" },
                        "question": { "type": "string", "description": "the question put to a person for `ask`; {{tool}}, {{caller}} and {{args}} are substituted" },
                        "on_timeout": { "enum": ["allow", "deny", "ask", "shadow"], "description": "what an unanswered `ask` becomes (default deny)" },
                        "timeout": { "type": "string" },
                        "to": { "oneOf": [
                            { "type": "string", "description": "an operator addressee: the id `operator` (docs/configuration.md §12.6); or `principal`, the person whose grant the work acts with (RFC 0044)" },
                            { "type": "object", "additionalProperties": false, "properties": {
                                "id": { "type": "string" }, "role": { "enum": ["operator", "user", "agent"] },
                                "labels": { "type": "object", "additionalProperties": { "type": "string" } } } } ],
                            "description": "who may answer an `ask` gate (only with action: ask): {role: operator} narrowed by labels, or the id `operator` — anyone else could never see the task and is refused (docs/configuration.md §12.6); `principal` opens the person's consent gate — agentd's own text, never judged, denied unanswered (no `question`, on_timeout deny); unset = {role: operator}" },
                        "once_per": { "enum": ["run_tool"], "description": "a `to: principal` rule asks at the first use per run (or conversation, or subagent) and tool alone: the person's approval passes later calls of the tool there" } } } },
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
                "client": client(),
                "companies": companies(),
                "payments": payments(),
                "experimental": { "type": "object", "additionalProperties": false, "description": "features built on drafts, each off until turned on",
                    "properties": { "id_jag": { "type": "boolean", "description": "allow `grant: id_jag` servers (RFC 0044; needs the xaa feature) until ID-JAG reaches IETF working-group last call" } } } } }));
}

/// The `private_key_jwt` / client-secret client authentication an IdP or a
/// resource's authorization server knows agentd by.
fn client_auth_schema(secret: &Value) -> Value {
    json!({ "type": "object", "additionalProperties": false,
        "description": "how the client authenticates at the token and revocation endpoints; with no client_id, only assertion_aud", "properties": {
        "method": { "enum": ["private_key_jwt", "client_secret_basic", "client_secret_post"] },
        "key": secret, "kid": { "type": "string" },
        "assertion_aud": { "enum": ["token_endpoint", "issuer"] },
        "alg": { "enum": ["ES256", "RS256"], "description": "private_key_jwt: the key's algorithm; default ES256" } } })
}

/// The fields an `auth` block of `grant: id_jag` adds (RFC 0044).
fn auth_delegation_properties(duration: &Value) -> Map<String, Value> {
    let v = json!({
        "idp": { "type": "string", "description": "grant id_jag: the identity.providers entry the person's grant is at" },
        "tool_scopes": { "type": "object", "additionalProperties": { "type": "array", "items": { "type": "string" } }, "description": "grant id_jag: the scopes each tool needs beyond scopes" },
        "scope_mode": { "enum": ["server", "per_tool"], "description": "grant id_jag: one token per server with the union of scopes (default), or one per distinct scope set" },
        "tools_from": { "enum": ["first_grant", "file"], "description": "grant id_jag: the tool catalogue from the first delegated connection (default), or tools_file" },
        "tools_file": { "type": "string", "description": "grant id_jag, tools_from file: a tools/list result" },
        "id_jag": { "enum": ["discover", "assume"], "description": "grant id_jag: require the ID-JAG profile in the authorization server's metadata (default), or assume it" },
        "id_jag_reuse": { "enum": ["auto", "never"], "description": "grant id_jag: present an unexpired ID-JAG again after a 401 (default auto)" },
        "on_revoked": { "enum": ["fail", "ask"], "description": "a workflow step's call when the grant it needs was revoked or has expired, or the session its run acted through ended: fail (default), or ask — the step waits, up to identity.delegation.consent_timeout, for the person to enrol again or sign in through a live session, and is made again; never for a model's call or work that read outside text" },
        "allow_unbound_at": { "type": "boolean", "description": "grant id_jag: accept an ID-JAG that does not name the pinned resource (warned and recorded)" },
        "attribution": { "enum": ["sub", "none"], "description": "grant id_jag: what agent/acting_for carries — the ID-JAG's sub (default), or nothing" },
        "max_at_age": duration,
        "trusted_output": { "type": "boolean", "description": "grant id_jag: nobody outside can write what this server answers, so its results mark the work as holding delegated data and do not taint it (default false: delegated SaaS content is attacker-writable)" },
        "subject": { "enum": ["grant", "inbound"], "description": "grant token_exchange: what is exchanged — an access token for agentd the enrolled grant's refresh token gets at this authorization server (grant, the default), or the caller's verified inbound A2A token, whose issuer must be this authorization server (inbound): held in memory for the work, never sent anywhere else, and the call fails subject_expired once it expires" },
        "dpop": { "enum": ["auto", "off"], "description": "grant id_jag: DPoP (RFC 9449) on the authorization-server and MCP legs — auto (default) binds the access token to a key agentd holds in memory where the server's protected-resource metadata and its authorization server's metadata both advertise ES256 proofs; off never does, and a server that takes bound tokens alone is then refused" }
    });
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// `identity.providers.<name>` (RFC 0044): an enterprise IdP.
fn provider() -> Value {
    let secret = json!({ "type": "string", "description": "a secret reference: {{secret-file:PATH}}, or {{secret:NAME}} entered at the startup prompt — never the environment" });
    json!({ "type": "object", "additionalProperties": false, "required": ["issuer", "client_id"], "properties": {
        "issuer": { "type": "string", "description": "the IdP's issuer, pinned (https; plain http only to a loopback host). Its RFC 8414 and OpenID metadata are both read, and merged" },
        "profile": { "enum": ["generic", "okta"], "description": "okta: ID-JAGs come only from the org authorization server (an /oauth2/<id> issuer is refused), keys at {issuer}/oauth2/v1/keys, subject refresh_token. Default generic" },
        "client_id": { "type": "string", "description": "agentd's client at the IdP (Okta: the AI agent's client ID)" },
        "client_auth": client_auth_schema(&secret),
        "client_secret": secret,
        "sign_in": { "type": "object", "additionalProperties": false, "properties": {
            "grants": { "type": "array", "items": { "enum": ["device", "authorization_code"] }, "description": "the enrolment grants, in the order tried; default [device]" },
            "scopes": { "type": "array", "items": { "type": "string" }, "description": "default [openid, offline_access]; openid is required" } } },
        "subject": { "enum": ["refresh_token", "id_token"], "description": "what the token exchange presents: the sealed refresh token, or a fresh ID token from a refresh with no scope. Default refresh_token under okta, id_token otherwise" },
        "subjects": { "enum": ["public", "pairwise"], "description": "public (default): an ID-JAG's sub must equal the grant's; pairwise: recorded, not compared" },
        "jwks_uri": { "type": "string", "description": "the IdP's key set, pinned; default the OpenID metadata's (okta: {issuer}/oauth2/v1/keys)" },
        "equivalent_issuers": { "type": "array", "items": { "type": "string" }, "description": "a2a.inbound_tokens issuers whose subject claim is this IdP's sub (RFC 0044 §5.9): a caller such a token names holds the grant its subject holds here. Subjects are never matched across issuers not listed; an issuer is one provider's at most" },
        "ciba": { "type": "object", "additionalProperties": false, "description": "OpenID CIBA in poll mode at this IdP (RFC 0044 §2.8): how a firing of an act_for workflow with via ciba asks its person to approve it on their own device. The IdP's metadata names its backchannel_authentication_endpoint and lists poll",
            "properties": {
                "enabled": { "type": "boolean", "description": "default false" },
                "request_expiry": { "type": ["string", "integer"], "description": "how long a request waits for the person, 1s to 1h (default 300s); sent as request_expiry under profile okta, else requested_expiry" },
                "per_day": { "type": "integer", "minimum": 1, "description": "how many requests one person is sent a day (default 10)" } } } } })
}

/// `a2a.inbound_tokens` (RFC 0044 §5.9): hoisted out of the `a2a` literal,
/// which sits at the macro's recursion limit.
fn inbound_tokens_schema() -> Value {
    let duration = json!({ "type": ["string", "integer"], "description": "a duration: `10m`, `90s`, `500ms`, or bare seconds" });
    json!({ "type": "array", "description": "the enterprise authorization servers whose JWT access tokens (RFC 9068) name a caller (RFC 0044 §5.9; needs the a2a and oauth features). Each issuer is pinned: keys only from its own key set, an asymmetric allow-list of algorithms, a profile's typ, the listed clients alone; a token passes as user:oidc:<iss>#<subject>, and one issued to a client alone never names a user. The card declares an oauth2 scheme per issuer whose metadata and keys are loaded. Restart-only",
        "items": { "type": "object", "additionalProperties": false, "required": ["issuer", "clients"], "properties": {
            "issuer": { "type": "string", "description": "the issuer, pinned: its metadata's issuer and every token's iss must equal it byte for byte (https; plain http only to a loopback host)" },
            "name": { "type": "string", "description": "the card's scheme name; default enterprise for the first issuer, enterprise-<n> for the n-th" },
            "profile": { "enum": ["generic", "okta"], "description": "generic (default, RFC 9068): typ at+jwt, sub, client_id, scope; okta (a custom authorization server): no typ or at+jwt, the person in uid, the client in cid, the scopes in scp, keys at {issuer}/v1/keys" },
            "audiences": { "type": "array", "items": { "type": "string" }, "description": "the aud values a token may carry; default [a2a.resource]. Never a client id" },
            "jwks_uri": { "type": "string", "description": "the key set, pinned on the issuer's origin; default the issuer's metadata's jwks_uri. jku, x5u and keys embedded in a token are never read" },
            "algs": { "type": "array", "items": { "enum": ["RS256", "ES256", "EdDSA"] }, "description": "the algorithms a token may be signed with; default [RS256, ES256]. none and every HMAC algorithm are refused" },
            "typ": { "oneOf": [ { "enum": ["none"] }, { "type": "array", "items": { "type": "string" } } ], "description": "the JOSE typ a token may carry: none (no typ, or at+jwt), or the values listed (with at+jwt); default at+jwt (generic) or none (okta)" },
            "subject_claim": { "type": "string", "description": "the claim naming the person; default sub (generic), uid (okta)" },
            "tenant_claim": { "type": "string", "description": "the claim naming a multi-tenant issuer's tenant, put into the principal: user:oidc:<iss>#<tenant>/<subject>, so one subject in two tenants is two people" },
            "clients": { "type": "array", "items": { "type": "string" }, "description": "the front-end clients (azp, cid or client_id) that may call; a token from any other is refused" },
            "require_user_token": { "type": "boolean", "description": "refuse a token issued to a client alone (default true); false names such a caller an agent, never a user" },
            "max_token_age": duration,
            "max_chain_depth": { "type": "integer", "minimum": 0, "maximum": 16, "description": "how deep a token's act chain may nest (default 4); a deeper one is refused, never cut" },
            "flow": { "enum": ["authorization_code", "device_code"], "description": "the flow the card declares for getting a token here, from the issuer's metadata; default authorization_code (with PKCE)" },
            "scopes_to_skills": { "type": "object", "additionalProperties": { "type": "array", "items": { "type": "string" } }, "description": "what each scope lets its holder use: conversation, workflow:<name glob>, command:<op>. A call no scope of the token covers is a 403 insufficient_scope naming the scopes that would; empty, the scopes narrow nothing" } } } })
}

/// `identity.delegation` (RFC 0044 §5.5).
fn delegation() -> Value {
    let duration = json!({ "type": ["string", "integer"], "description": "a duration: `10m`, `90s`, `500ms`, or bare seconds" });
    let template = template_schema(&duration);
    let secret = json!({ "type": "string", "description": "a secret reference: {{secret-file:PATH}}, or {{secret:NAME}} entered at the startup prompt — never the environment" });
    json!({ "type": "object", "additionalProperties": false,
        "description": "acting for enrolled people through their IdP (RFC 0044; needs the xaa feature): the seal key, the token caps, who may enrol for what and which device names are whose. Operator-only; the templates, device pins, first binding and the consent ages reload, the rest is restart-only",
        "properties": {
            "seal_key": secret,
            "seal_key_previous": secret,
            "key_custody": { "enum": ["local", "external"], "description": "who holds the seal key and every private_key_jwt key under identity.providers and on a delegated server: agentd, from secret references (local, the default), or the external signer (external, with signer; the keys are then named by kid, and no key or seal_key is given)" },
            "signer": { "type": "object", "additionalProperties": false, "required": ["socket", "uid", "seal_key"],
                "description": "the external signer (key_custody external; docs/external-signer.md): agentd signs client assertions and wraps each grant's data key through it, and never holds a private key or the key-encryption key",
                "properties": {
                "socket": { "type": "string", "description": "the signer's unix socket, an absolute path" },
                "uid": { "type": "integer", "minimum": 0, "description": "the uid the signer runs as: the socket's peer (SO_PEERCRED) must be it, or nothing is sent" },
                "timeout": duration,
                "seal_key": { "type": "string", "description": "the signer's key-encryption key each grant's data key is wrapped under" },
                "seal_key_previous": { "type": "string", "description": "the key-encryption key before a rotation: unwraps, never wraps" } } },
            "at_cache_cap": duration,
            "liveness_interval": duration,
            "consent_timeout": duration,
            "consent_session_max_age": { "type": ["string", "integer"], "description": "how recent a display session's IdP sign-in must be for it to answer a consent gate (default 12h). Reloadable" },
            "consent_per_minute": { "type": "integer", "minimum": 1, "description": "how many consent gates may open for one person a minute (default 10); a call past that is refused rather than asked. Reloadable" },
            "device_pins": { "type": "object", "description": "RFC 0043 device names (auth.device.approve as:), each pinned to the one IdP identity that may enrol it; each session using the grant proves it again. Reloadable: a changed or removed pin revokes the name's grant and drops its sessions' proofs",
                "additionalProperties": { "type": "object", "additionalProperties": false, "required": ["iss", "sub"], "properties": {
                    "iss": { "type": "string" }, "sub": { "type": "string" } } } },
            "device_first_binding": { "type": "boolean", "description": "let an unpinned device name bind to whoever completes its first IdP enrolment (default false: a forwarded sign-in link lands the proof on the session that started it). Needs a2a.device_grant. Reloadable: turning it off revokes the grants it bound" },
            "templates": { "type": "array", "description": "who may enrol, for what; no matching template, no enrolment. Reloadable: narrowing applies at the next mint; a removed template, or a server removed from one, revokes its grants",
                "items": template } } })
}

/// One `identity.delegation.templates[]` entry: hoisted out of
/// [`delegation`], whose `json!` would otherwise pass the macro's recursion
/// limit.
fn template_schema(duration: &Value) -> Value {
    json!({ "type": "object", "additionalProperties": false, "required": ["name", "principals", "idp", "servers"], "properties": {
                    "name": { "type": "string" },
                    "principals": { "type": "object", "additionalProperties": false, "description": "every field named must match", "properties": {
                        "source": { "enum": ["operator", "oidc", "x509", "device"], "description": "operator (a per-person operator), oidc (an a2a.login session, or a caller an a2a.inbound_tokens token names), x509 (a certificate principal its a2a.principals rule pins by key and idp_subject) or device (an RFC 0043 device session)" },
                        "role": { "enum": ["operator", "user"] },
                        "labels": { "type": "object", "additionalProperties": { "type": "string" } } } },
                    "idp": { "type": "string", "description": "the identity.providers entry its principals bind to" },
                    "servers": { "type": "object", "additionalProperties": { "type": "object", "additionalProperties": false, "properties": {
                        "scopes": { "type": "array", "items": { "type": "string" }, "description": "the scope ceiling on this server" },
                        "document_workflows": { "type": "array", "description": "workflows an instruction document defined, by name and definition digest, whose calls to this server are not document_sourced: the document-sourced consent rule does not ask about them",
                            "items": { "type": "object", "additionalProperties": false, "required": ["name", "digest"], "properties": {
                                "name": { "type": "string" }, "digest": { "type": "string", "description": "the definition digest workflow.list shows: 64 lowercase hex" } } } } } },
                        "description": "each grant: id_jag server it allows, by name" },
                    "subagents": { "enum": ["none", "inherit"], "description": "whether a subagent, think child or workflow-as-tool run of the person's work acts with their grant: none (default), or inherit — on the delegated servers the child declares, its calls made by the supervisor, holding no token" },
                    "approvers": { "type": "array", "items": { "enum": ["principal", "operator"] }, "description": "who may answer the person's consent gates: [principal] (default) or [principal, operator], an operator's answer recorded as such" },
                    "agent_mode_servers": { "type": "array", "items": { "type": "string" }, "description": "agent-mode servers tagged sensitive or egress that stay visible to the person's delegated work; every other one is hidden from it (mode mixing, RFC 0044 §5.3)" },
                    "clients": { "type": "array", "items": { "type": "string" }, "description": "the front-end clients whose inbound tokens may act with these grants (an oidc template at an IdP an a2a.inbound_tokens issuer maps to); a caller another client's token names is admitted by no template. Empty, every client the issuer allows" },
                    "unattended": { "enum": ["never", "ciba", "standing"], "description": "whether a firing of an act_for workflow may act with these grants with nobody there (RFC 0044 §5.12 E): never (default); ciba, each firing approved by the person on their own device; standing, the grant used with nobody asked — which allows via ciba too. A firing acts only while the workflow is the definition the person consented to when they last signed in" },
                    "unattended_tools": { "type": "array", "items": { "type": "string" }, "description": "the delegated tools, <server>.<tool>, a firing may call when they are tagged egress; every other is refused" },
                    "allow_against_idp_guidance": { "type": "boolean", "description": "unattended standing at a profile okta IdP, which Okta advises against: refused without this (default false)" },
                    "ttl": duration,
                    "budget": { "type": "object", "additionalProperties": false, "properties": {
                        "per_day": { "type": "integer", "minimum": 1, "description": "mints per day per server; default 16" },
                        "per_month": { "type": "integer", "minimum": 1, "description": "mints per month per server; default 200" },
                        "per_run": { "type": "integer", "minimum": 1, "description": "mints per run; default 12" },
                        "per_minute": { "type": "integer", "minimum": 1, "description": "mints asked for per person and server in any minute; default 4" } } } } })
}

/// `security.client` (RFC 0052): agentd's OAuth client where it has no
/// registration, published as a Client ID Metadata Document.
fn client() -> Value {
    json!({ "type": "object", "additionalProperties": false, "required": ["id", "name"],
        "description": "agentd's OAuth client where it has no registration (RFC 0052; needs the oauth feature): an auth block of kind oauth2 that names no client_id presents this Client ID Metadata Document's URL as its client_id, with private_key_jwt, at an authorization server whose metadata advertises client_id_metadata_document_supported. Restart-only, operator-only",
        "properties": {
            "id": { "type": "string", "description": "the client_id: the HTTPS URL the document is published at — a path of its own, no userinfo, query, fragment or dot segment (draft §3)" },
            "name": { "type": "string", "description": "client_name, shown on authorization servers' consent pages" },
            "logo_uri": { "type": "string", "description": "an HTTPS URL" },
            "jwks_uri": { "type": "string", "description": "the key set's HTTPS URL; default jwks.json beside the document" },
            "key_file": { "type": "string", "description": "the ES256 signing key (PKCS#8), created 0600 when absent; default /var/lib/agentd/client-key" },
            "published_key_files": { "type": "array", "items": { "type": "string" }, "description": "keys whose public halves are in the key set but are never signed with: the next key before a rotation, the previous one after it" },
            "redirect_uris": { "type": "array", "items": { "type": "string" }, "description": "loopback redirect URIs for the authorization_code grant, http://127.0.0.1[:port]/path or http://[::1][:port]/path: with a port, the login listens on it; without one, on any free port, for an authorization server that allows any port on a loopback address (RFC 8252 §7.3)" },
            "grants": { "type": "array", "items": { "enum": ["device", "authorization_code", "client_credentials"] }, "description": "the grants the document lists in grant_types (with refresh_token beside device and authorization_code); a block that uses the document with a grant it does not list is refused at load. Default [device]" },
            "serve": { "type": "boolean", "description": "serve the document and key set on the A2A listener at their URLs' paths (needs a2a, and a2a.url the client_id's origin); otherwise publish what --print-client-metadata writes" } } })
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
            "seal_key": { "type": "string", "description": "32 random bytes as hex or base64, given as a secret reference: seals the Account Tokens a sign-in gives agentd, and the User ID records, so a store cannot read them. Required with a sign_in, and with a remote store (mcp, http), unless key_custody is external" },
            "key_custody": { "enum": ["local", "external"], "description": "who holds the seal key: agentd, from seal_key (local, the default), or the external signer (external, with signer and no seal_key), which wraps each record's data key and derives the record-id keys" },
            "signer": { "type": "object", "additionalProperties": false, "required": ["socket", "uid", "seal_key", "derive_key"],
                "description": "the external signer that holds the seal key (key_custody external; docs/external-signer.md): agentd never holds the key-encryption key or the derivation key",
                "properties": {
                "socket": { "type": "string", "description": "the signer's unix socket, an absolute path" },
                "uid": { "type": "integer", "minimum": 0, "description": "the uid the signer runs as: the socket's peer (SO_PEERCRED) must be it, or nothing is sent" },
                "timeout": { "type": ["string", "integer"], "description": "the bound on one request, 1ms to 60s, a duration: `5s`, `500ms`, or bare seconds; default 5s" },
                "seal_key": { "type": "string", "description": "the signer's key-encryption key each record's data key is wrapped under" },
                "derive_key": { "type": "string", "description": "the signer's HMAC-SHA256 key the record-id keys are derived under (the derive op)" } } },
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

/// `security.payments` (RFC 0049): the spend policy every payment is held
/// to, the wallets and who holds their keys, the approver. Needs the `x402`
/// feature.
fn payments() -> Value {
    let amount = json!({ "type": "string", "description": "a decimal in the asset's whole units (\"0.25\"), at most `decimals` digits after the point" });
    json!({ "type": "object", "additionalProperties": false,
        "description": "paying for things with x402 (RFC 0049; needs the x402 feature): the spend policy every payment is held to — assets by CAIP-19, payees by address, caps per payment, run, day and payee, the approver above a threshold — and the wallets. Restart-only, operator-only. Needs observability.audit.chain and a durable store",
        "properties": {
            "approver": { "type": "string", "description": "who approves a payment the policy asks about: `operator` (default: any operator principal) or one principal id `<role>:<id>`, the only one who can — no judge answers, and no operator but the approver" },
            "approval_timeout": { "type": ["string", "integer"], "description": "how long the approver is waited for before the payment is declined, 1s to 7d; default 1h" },
            "ask_when_tainted": { "type": "boolean", "description": "a payment from work that has read outside text, or that a model asked for, is asked of the approver whatever its amount; default true" },
            "max_validity": { "type": ["string", "integer"], "description": "the longest an authorization agentd signs is valid (validBefore after now), 10s to 1h; the server's maxTimeoutSeconds when shorter; default 5m" },
            "key_custody": { "enum": ["local", "external"], "description": "who holds the wallet keys: agentd, each key file sealed under seal_key (local, the default), or the external signer (external, with signer and no seal_key)" },
            "seal_key": { "type": "string", "description": "32 random bytes as hex or base64, a single {{secret-file:PATH}} reference (never the environment): every wallet key file is sealed under it. key_custody local only" },
            "signer": { "type": "object", "additionalProperties": false, "required": ["socket", "uid"],
                "description": "the external signer that holds the wallet keys (key_custody external; docs/external-signer.md): it signs only EIP-712 typed data of an EIP-3009 TransferWithAuthorization, or a Solana transaction message of an SPL transfer, its own policy allows",
                "properties": {
                "socket": { "type": "string", "description": "the signer's unix socket, an absolute path" },
                "uid": { "type": "integer", "minimum": 0, "description": "the uid the signer runs as: the socket's peer (SO_PEERCRED) must be it, or nothing is sent" },
                "timeout": { "type": ["string", "integer"], "description": "the bound on one request, 1ms to 60s; default 5s" } } },
            "wallets": { "type": "object", "description": "the wallets agentd pays from, by name (1 to 32 of a-z, 0-9, - and _)", "additionalProperties": { "type": "object", "additionalProperties": false, "properties": {
                "chain": { "enum": ["eip155", "solana"], "description": "the chain family it pays on, by CAIP-2 namespace: eip155 (the default; a secp256k1 key) or solana (an Ed25519 key); a step pays from it only in assets on that family's chains" },
                "key_file": { "type": "string", "description": "key_custody local: the sealed key file, an absolute path, made 0600 by `agentd --print-wallet-address <name>` when absent; refused at start when another user can read it or it does not open to its address" },
                "key": { "type": "string", "description": "key_custody external: the signer's key id" },
                "address": { "type": "string", "description": "the wallet's address (0x and 40 hex digits on eip155, 32 bytes in base58 on solana); when given, a key of another address refuses the start" } } } },
            "assets": { "type": "object", "description": "the assets agentd pays in, by name; amounts of two assets are never added", "additionalProperties": { "type": "object", "additionalProperties": false, "required": ["asset", "decimals", "limits"], "properties": {
                "asset": { "type": "string", "description": "the CAIP-19 asset type, eip155:<chain id>/erc20:<contract> or solana:<genesis hash prefix>/token:<mint>: names the network (CAIP-2) and the token" },
                "decimals": { "type": "integer", "minimum": 0, "maximum": 36, "description": "the token's decimals: \"1\" is 10^decimals atomic units" },
                "token_program": { "enum": ["spl-token", "token-2022"], "description": "a Solana token's program, whose transfer and associated token accounts it uses; default spl-token. Solana assets only" },
                "limits": { "type": "object", "additionalProperties": false, "required": ["per_run", "per_day", "per_payee"], "properties": {
                    "per_payment": amount, "per_run": amount, "per_day": amount, "per_payee": amount },
                    "description": "per_payment: one payment; per_run: one run's payments; per_day: all payments in any 24 hours; per_payee: all payments to one payee in any 24 hours — each counting every payment that left agentd or may have" },
                "approve_above": { "type": "string", "description": "above this amount (whole units) the approver is asked" } } } },
            "payees": { "type": "object", "description": "the addresses agentd may pay, by name", "additionalProperties": { "type": "object", "additionalProperties": false, "required": ["address"], "properties": {
                "address": { "type": "string", "description": "the payTo address: 0x and 40 hex digits on an EVM chain, 32 bytes in base58 on Solana" },
                "assets": { "type": "array", "items": { "type": "string" }, "description": "the assets it may be paid in, each on its address's chain family; default every asset" } } } },
            "rpc": { "type": "object", "additionalProperties": { "type": "string" },
                "description": "by Solana network (CAIP-2, e.g. solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp), the RPC endpoint asked for a recent blockhash (getLatestBlockhash) when a server's entry gives no extra.recentBlockhash: https://, or http:// to this host; {{secret:NAME}} references in it resolved when it is dialled. Without one, an entry with no blockhash is passed over" } } })
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
                "client_id": { "type": "string", "description": "the registered client; unset, agentd's Client ID Metadata Document (security.client) is the client, where the issuer's metadata advertises it" }, "client_secret": secret,
                "client_auth": client_auth_schema(secret),
                "grant": { "enum": ["device", "authorization_code", "client_credentials", "id_jag", "token_exchange"], "description": "id_jag (mcp.servers only; needs the xaa feature): an enrolled person's grant, exchanged at their IdP for an ID-JAG and redeemed here; token_exchange (likewise): an RFC 8693 on-behalf-of exchange at the server's own authorization server, which is the person's IdP — both served by the supervisor alone" },
                "scopes": { "type": "array", "items": { "type": "string" } }, "audience": { "type": "string" },
                "resource": { "type": "string", "description": "oauth2: the RFC 8707 resource every authorization, token, refresh and client-credentials request carries — auto (the endpoint, canonical), omit, or a URI; unset, auto on an MCP server or MCP catalog entry and omit elsewhere" },
                "token": secret, "header": { "type": "string" }, "value": secret,
                "region": { "type": "string" }, "service": { "type": "string" },
                "source": { "enum": ["env", "static", "imds", "irsa", "sso"] },
                "sso_start_url": { "type": "string" }, "account_id": { "type": "string" }, "role_name": { "type": "string" },
                "svid": { "enum": ["jwt", "x509"] }, "jwt_svid_file": { "type": "string" },
                "svid_file": { "type": "string" }, "key_file": { "type": "string" } } }));
    // Beside the rest, rather than in the literal above: that `json!` sits at
    // the macro's recursion limit.
    if let Some(p) = m
        .get_mut("Auth")
        .and_then(|a| a.get_mut("properties"))
        .and_then(Value::as_object_mut)
    {
        p.extend(auth_delegation_properties(duration));
    }
    m.insert("ToolOverride".to_string(), json!({ "type": "object", "additionalProperties": false, "required": ["server", "tool"], "properties": {
                "server": { "type": "string" }, "tool": { "type": "string" },
                "args": { "type": "string", "description": "a JSON template or `CEL: …` producing the MCP tool arguments from `args`/`ctx`" },
                "result": { "type": "string", "description": "a JSON pointer / template / `CEL: …` mapping the CallToolResult to the internal output schema" } } }));
    m.insert("WebhookAuth".to_string(), json!({ "type": "object", "additionalProperties": false, "properties": {
                "hmac": { "type": "object", "additionalProperties": false, "properties": {
                    "secret": secret, "header": { "type": "string", "description": "the header carrying the signature (default X-Signature)" }, "algo": { "enum": ["sha256"] }, "prefix": { "type": "string", "description": "a prefix stripped before the constant-time compare, e.g. sha256=" } } },
                "bearer": secret,
                "header": { "type": "object", "additionalProperties": false, "properties": { "name": { "type": "string" }, "equals": secret } },
                "none": { "type": "boolean", "description": "loopback-only, no auth (dev) — explicit opt-in" },
                "jwt": { "type": "object", "additionalProperties": false, "required": ["issuer", "audience"],
                    "description": "a person's JWT from an a2a.inbound_tokens issuer (RFC 0044 §5.9): verified as an A2A token is, for this webhook's own audience, short-lived, its jti never accepted twice; the run is the person's",
                    "properties": {
                    "issuer": { "type": "string", "description": "an a2a.inbound_tokens issuer" },
                    "audience": { "type": "string", "description": "the webhook's own resource identifier — never one an A2A token is issued for" },
                    "max_lifetime": { "type": ["string", "integer"], "description": "how long a token may live, iat to exp (default 5m, at most 15m)" } } } } }));
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
                    "san": { "type": "string" }, "sub": { "type": "string" }, "bearer_ref": { "type": "string" }, "any": { "type": "boolean" },
                    "uid": { "type": "integer", "minimum": 0, "description": "the unix A2A listener's peer uid (SO_PEERCRED): the per-person operator operator:<id> (RFC 0044; needs the xaa feature)" },
                    "spki": { "type": "string", "pattern": "^[0-9a-f]{64}$", "description": "a client certificate's key, the SHA-256 of its SubjectPublicKeyInfo: the rule admits that key alone. Needs a2a.tls.client_ca; with idp_subject, a certificate principal that delegates — an operator one is operator:<id> (RFC 0044)" },
                    "token_iss": { "type": "string", "description": "an a2a.inbound_tokens issuer: the rule names the callers its tokens name, sub then a glob over the issuer's subject claim; role user or agent, no id (RFC 0044 §5.9; needs the a2a and oauth features)" } } },
                "idp_subject": { "type": "object", "additionalProperties": false, "required": ["iss", "sub"],
                    "description": "the one IdP identity this principal is, exact: required to delegate, on a uid or an spki rule (RFC 0044; needs the xaa feature)",
                    "properties": { "iss": { "type": "string" }, "sub": { "type": "string" } } },
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
