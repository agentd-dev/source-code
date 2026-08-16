# RFC 0028: The tool registry, internal tools, overrides, knowledge/search/skills

**Status:** Proposed (agentd 2.0 track — implemented in phase P3, extended in P4)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-16
**Part of:** the durable-agent design (`docs/design/01-durable-agent-plan.md` §3.7, §3.16); supersedes RFC 0005 §self-tools (`subagent.*`, `subscribe`, `resource.read`, `workflow.define/patch/run` as the self surface) and extends RFC 0022 §4 (code-registered tools, precedence).

---

## 1. Summary

One **tool registry** serves the root agent, workflow steps and subagents.
Three tiers with dispatch precedence **internal > code > MCP**. Every tool
carries JSON Schemas for input and output and a **grant** (who may call it).
**Internal tools** are contracts: a built-in implementation by default,
**overridable** by a mapped MCP tool (`tools.overrides`) and **disable-able**
(`tools.disabled`). Knowledge, search and skills are remote capabilities
behind MCP with fixed contracts (§5–§7).

## 2. Registry

```
ToolSpec { name, description, input_schema, output_schema, class: Internal|Code|Mcp{server},
           grant: {root: bool, workflows: bool, subagents: bool, a2a_roles: [role]}, disabled: bool,
           impl: BuiltIn | Mapped{server, tool, args, result} | Code | Mcp }
```

- Names are dotted (`memory.get`); MCP tools are `<ns>.<tool>` when the server
  declares `ns`, else `<server>.<tool>` when a collision exists, else the bare
  tool name (RFC 0022 §4 precedence keeps first-party names unstealable).
- LLM-facing definitions are generated from the registry (wire-sanitized
  names, RFC 0006 fix in 1.4.0).
- Args are validated against `input_schema` before dispatch; results against
  `output_schema` after (schema failure ⇒ tool error result, never a panic).
- Every dispatch is an **effect** with an idempotency key (RFC 0025 §7),
  a span, and an audit event when the caller is a principal.

## 3. Internal tools (contracts)

| Tool | Input | Output | Built-in | Notes |
|---|---|---|---|---|
| `instruction.read` | `{}` | `{text, source: static\|resource, uri?, version?}` | yes | `agent.instruction` is one field: a single-token URI a configured MCP server serves ⇒ read + subscribed; else static text |
| `instruction.subscribe` | `{uri?}` | `{subscribed, uri}` | yes | re-instruction wakes the root |
| `subagent.run` | `{instruction, mode: sync\|async\|detached\|warm, workflow?, tools?, servers?, limits?, context?, output_contract?, skills?}` | `{handle, status, result?}` | yes | RFC 0026 §6 |
| `subagent.send` / `subagent.kill` / `subagent.status` / `subagent.await` / `subagent.list` | `{handle, message}` / `{handle, reason?}` / `{handle}` / `{handle, timeout?}` / `{}` | … | yes | |
| `code.run` | `{language, code, files?, timeout?}` | `{stdout, stderr, exit_code, files?}` | **no** (mapping-only; disabled unless mapped) | RFC 0012 posture: no local execution |
| `memory.get` / `set` / `list` / `delete` | `{key}` / `{key, value, ttl?}` / `{prefix?, limit?}` / `{key}` | `{value?, meta}` / `{ok}` / `{keys}` / `{ok}` | yes | RFC 0025 `memory` kind; size caps |
| `artifact.create` / `get` / `delete` / `list` | `{name, mime, content\|from_step, sensitive?}` / `{id}` / `{id}` / `{}` | `{id, size, sha256}` / … | yes | RFC 0025 `artifact`; A2A delivery |
| `workflow.run` / `create` / `update` / `delete` / `list` / `status` / `cancel` / `pause` / `resume` / `signal` / `wait` | per RFC 0027 §10 | | yes | `create/update/delete` restricted by grant |
| `plan.create` / `get` / `update` / `clear` | `{goal, items}` / `{}` / `{item, status?, note?, bind?, insert?, reorder?}` / `{}` | the plan | yes | RFC 0026 §5.3 |
| `ask_human` | `{question, schema?, to?, timeout?}` | `{reply}` | yes | A2A `input-required` on the conversation/task; overridable |
| `sleep` | `{duration}` | `{slept_ms}` | yes | durable timer |
| `await` | `{condition: CEL, on?: [resource\|memory\|step\|signal], timeout?}` | `{satisfied, value?}` | yes | durable |
| `context.compact` | `{target_tokens?, keep_last?}` | `{version, est_tokens}` | yes | |
| `think` | `{prompt, output_schema?, reads?, skills?}` | the object | yes | structured, no tools |
| `finish` | `{status, output?, reason?, exit?}` | `{}` | yes | terminates the calling unit |
| `status` | `{}` | instance/runs/subagents/budget summary | yes | also an A2A command |
| `knowledge.search` / `get` / `list` | §5 | | **no** (profile) | |
| `search.query` / `fetch` | §6 | | **no** (profile) | |
| `skills.list` / `load` / `unload` | `{}` / `{name, version?}` / `{name}` | `{skills}` / `{loaded}` / `{ok}` | yes (over MCP prompts/resources) | §7 |

Grants default: root — all; workflows — all but `finish` (a run uses the
`finish` step); subagents — `memory.*`, `artifact.*`, `ask_human`, `sleep`,
`await`, `think`, `knowledge.*`, `search.*`, `skills.*` (configurable per
spawn); A2A roles per RFC 0029 §5.

## 4. Overrides and disabling

```yaml
tools:
  disabled: [code.run, workflow.delete]
  overrides:
    memory.get: { server: mem, tool: search, args: 'CEL: {"query": args.key, "limit": 1}', result: 'CEL: {"value": result.structuredContent.results[0].text}' }
    ask_human:  { server: slack, tool: post_and_wait, args: '{"channel": "#ops", "text": "{{args.question}}"}', result: '{"reply": {{result.structuredContent.reply}}}' }
```

- The contract (name, schemas, semantics for callers) is unchanged; the
  implementation becomes `Mapped`: `args` (template or `CEL:`) builds the MCP
  tool arguments from `args`/`ctx` (`ctx` = `{instance, run?, ctx?, principal?}`);
  `result` maps the `CallToolResult` back to the internal `output_schema`
  (validated).
- Startup validation: the server is declared, the tool is advertised (or
  `--validate-config` warns when the server is unreachable), the mapping
  compiles, disabled ∩ overrides = ∅.
- A disabled tool is absent from LLM definitions and refused by workflow
  validation.

## 5. Knowledge profile

`knowledge.search {query, top_k?, filters?} → {hits: [{id, uri, title, score,
snippet, metadata}]}`; `knowledge.get {id|uri} → {content, mime, metadata}`;
`knowledge.list {prefix?} → {docs: [{id, uri, title}]}`. Config `knowledge.server`
names an MCP server that advertises these tools (or `tools.overrides` maps
another server). `knowledge.auto_context {on: turn|never, top_k, max_bytes}`
retrieves for an incoming message before the turn and injects hits as a
labelled system block with sources.

## 6. Search profile

`search.query {query, kind?: web|docs|code, limit?, freshness?} → {results:
[{title, url, snippet, source, published?}]}`; `search.fetch {url, max_bytes?}
→ {content, mime, final_url}`. Fetching happens on the search server; the grant
carrying `search.*` is `untrusted_input + egress` for the trifecta gate.

## 7. Skills

A **skill** = `{name, description, when_to_use?, arguments?, body (Markdown),
resources?, hash, source}`. Sources (`skills.sources[]`): MCP servers, discovered
via **prompts** (`prompts/list` catalogue, `prompts/get` body with arguments) or
**resources** (`skill://<name>` URIs, or `mimeType: text/x-skill+markdown`; an
optional `skill://` index), `discover: prompts|resources|auto`, over the latest
dialect the server speaks (modern 2026-07-28 included). Discovery at startup
and on `list_changed`; catalogue cached (name, description, hash); bodies
fetched on load and cached by hash (never stored).

References `@skill:<name>` (`skills.reference_prefix`) in `agent.instruction`,
in `agent`/`think` steps (`skills: [name]`), or in a chat message are resolved
before the turn and **preloaded**; the model may `skills.load`. The loaded set
`[{name, hash}]` is part of the context state (RFC 0026 §5.1); limits
`skills.max_loaded` (8), `skills.max_bytes` (32 KiB); compaction evicts unused
bodies. Unknown references are reported (`skill.unknown`) — never silently
ignored.

## 8. Observability & security

Spans per dispatch (`tool.call {name, class, mapped}`); metrics
`agentd_tool_calls_total{name,class,result}` (un-reserving the RFC 0016 name);
audit for principal-driven calls; secrets never appear in args/results logs
(lengths only, unless `--log-content`). Mapped tools inherit the server's
trifecta tags.

## 9. Test plan

Registry precedence and collisions; schema validation both ways; each internal
tool's built-in behaviour on the memory store; overrides against the mock MCP
(mapping, error mapping, unreachable server); disabled tools in LLM defs and
workflow validation; skills discovery over legacy + modern mocks, `@skill:`
resolution, load/evict; knowledge/search profiles against mock servers with
`auto_context`.
