# Directive reference

This page is the lookup for the instruction-document syntax agentd reads: the
block forms and fence rules, every machinery kind with its grant and what
agentd does with it, the prose and structural kinds, what the model reads for
each construct, which configuration a document may write, and every
`agent.instruction` setting. agentd implements the
[Instruction Specification](https://github.com/instruction-md/specification)
version 1, registry revision 1.1; the specification holds the full grammar.

For the ideas behind these rules, see [Instruction documents](directives.md).
For a worked example, see
[Write and ship instruction documents](instruction-documents.md).

## Forms

Every block is written in one of these forms. A kind accepts only the forms
listed for it in the tables below; any other is refused, naming the forms it
takes.

| Form | Syntax | Used for |
|---|---|---|
| Container | `:::kind{attrs}` … `:::` | A block with a body. |
| Leaf | `::kind{attrs}` | A block with no body, on one line. |
| Set | `:::kind[]{attrs}` … `:::`, holding a Markdown table or a definition list | Several blocks of one kind. Each row or term is one member. |
| Section | `## !kind name {attrs}` | A Markdown section. It ends at the next heading of the same or a higher level, or at the next sigiled heading, fence or leaf. For a kind with a YAML or code body, the body is the one fenced code block in the section and the prose is its description; for a Markdown body, such as `!skill`, the section's text is the body. |
| Keyword | `KEYWORD: text` at the start of a line | A rule or note in one paragraph. |
| Alert | `> [!KEYWORD]` followed by quoted lines | A note in GitHub alert style. |

Machinery kinds are written with the `!` sigil (`:::!workflow`,
`::!human{…}`, `## !workflow nightly`). Prose and structural kinds are written
without it.

## Fences and attributes

- A fence opens at column 0: three or more colons, a kind, and optional
  `{attributes}`. A `:::` in the middle of a line, or indented, is text.
- A fence closes at a line of at least as many colons and nothing else. A
  block left open is refused: `:::!stream is never closed`.
- A longer fence contains a shorter one: a `:::must` inside a `::::context` is
  a nested block. A fence of the same length is not nesting; its opener is
  text in the outer body, and the first bare closer ends the outer block.
- Machinery folds into configuration from the document's top level only. A
  machinery block nested in any body is refused.
- `verbatim` on a block delivers its body unparsed: a `::::context{verbatim}`
  around a `:::!workflow` delivers those lines as text, and loads no
  workflow.
- Attributes are `key=value`, `key="a quoted value"` (with `\"` escapes), or a
  bare `flag`, which means `"true"`. Keys are read in lower case. A key written
  twice is refused. A value written as a YAML flow mapping or sequence
  (`{max_events: 100}`, `[a, b]`) is parsed as one.
- On a machinery block the attributes are folded over the YAML body, so an
  attribute wins over a body key of the same name.
- A machinery body is YAML, a table, code, Markdown or text, as the kind
  defines. A YAML body that does not parse refuses the document, naming the
  block's line.

## Front matter and end matter

Front matter is a `---` YAML block on the document's first line. It is never
delivered. agentd reads these keys:

| Key | Meaning |
|---|---|
| `spec` | The specification version the document pins. `"1"` is the only version agentd implements; any other is refused. Optional. |
| `delivery.labels` | The label style: `bold` (default), `plain` or `tags`. |
| `id` | The document's id. A signature's `doc` claim names it. |
| `signature` | The author signature, a JWS compact serialization. It is excluded from the digest it signs. |

End matter is a closing `---` YAML block after a blank line at the end of the
document: its record, such as owners, approvals and a changelog. It is never
delivered, the author signature covers it, and malformed end matter refuses
the document.

An author note runs from `<!--` at column 0 to the first line holding `-->`.
It is never parsed and never delivered, so a commented-out block does not
load.

## Machinery kinds

Every machinery kind except `!config` and `!tools` requires a `name`
attribute. A name is unique per kind within a document.

### Core kinds

These need no grant.

| Kind | Forms | Body | What agentd does | The model reads |
|---|---|---|---|---|
| `!workflow` | container, section | YAML | Adds the body to `workflows:` exactly as an inline entry: same variable folding, validation, content hash and run pinning. `armed=false` loads it disarmed. In the section form, the prose becomes the workflow's `description`. A name in the body does not name the block; the fence's does. | `[workflow "<name>" is loaded and runs autonomously]` |
| `!skill` | container, set, section | Markdown | Adds an inline skill to the skill catalogue, with `description` and `trigger` (`when` is an alias; `trigger` wins when both are set). The catalogue holds the body as delivered. An inline skill wins a name over a folder's skill; a name it shares with a server's skill is referenced as `local/<name>`. | `[skill "<name>" is available — reference it as @skill/<name>]`, or with a trigger `[skill "<name>" is available — use it when <trigger>; reference it as @skill/<name>]` |
| `!config` | container | YAML | A configuration fragment, held to the [document boundary](#configuration-a-document-may-write). Several `!config` blocks fold in order; a later block's top-level section replaces an earlier block's section whole. | `[runtime configuration is applied]` |
| `!mcp` | container, leaf | YAML | One `mcp.servers[]` entry. `allow` and `deny` (stored as `exclude`) filter the server's advertised tool names; an excluded tool does not exist. Takes `override` sub-blocks. | `[mcp server "<name>" is connected; its tools are available]` |
| `!stream` | container, leaf, set | YAML | One `streams.<name>` entry. An empty body means the defaults. | `[event stream "<name>" is declared]` |
| `!tools` | container | YAML | Merges into `tools:`. A document may set `disabled` and `narrow` only. | `[tool policy is applied]` |
| `!eval` | container, section | YAML | Nothing: test cases for an evaluation harness. The body must parse as YAML, and a `target`, when written, must resolve. A bare `:::eval` is prose. | nothing |
| `!data` | container | Table | Recorded as a declaration. The rows are not delivered to the model. | `[data "<name>" is available]` |

The skill acknowledgement says `@skill/<name>`, the specification's
spelling. agentd preloads a skill when a message holds
`skills.reference_prefix` followed by its name, `@skill:<name>` by default,
and the model can always load one with the `skills.load` tool.

### Extended kinds

Each extended kind needs its family in `agent.document_capabilities`. agentd
acts on the kinds whose entry starts with "Acts". The others are
declarations: parsed, grant-checked, refused when unsafe, and counted under
`document` in `agentd --capabilities`, with no runtime effect.

| Kind | Family | Forms | Body | What agentd does | The model reads |
|---|---|---|---|---|---|
| `!file` | `material` | container | Text | Declaration. Requires `path`. | `[file "<name>" is present at <path>]` |
| `!media` | `material` | container, leaf | Markdown | Declaration. A remote `src` requires `sha256`. | `[media "<name>" is available]` |
| `!asset` | `material` | leaf | none | Declaration. A remote `src` requires `sha256`. | `[asset "<name>" is available]` |
| `!knowledge` | `knowledge` | container, leaf | YAML | Declaration. | `[knowledge base "<name>" is available]` |
| `!retrieval` | `knowledge` | container | YAML | Declaration. | nothing |
| `!source` | `knowledge` | container, leaf, set | YAML | Declaration. | nothing |
| `!endpoint` | `interface` | container | YAML | Acts: becomes a workflow `endpoint-<name>` with one `webhook` start, from `path`, `methods`, `auth`, `into`, `rate` and `respond`. It loads only with `into: {stream, subject}`; served on the operator's `webhooks.listen`. | `[endpoint "<name>" listens at <path>]` |
| `!ui` | `interface` | container | Markdown | Acts: a step's `schema: "@ui/<name>"` takes the block's `schema` sub-block. | nothing |
| `!human` | `interface` | container, leaf, set | YAML | Acts: a `human` step with `to: "@human/<name>"`, at any depth, goes to the human's `principal` (the operators when none) and is announced on its `channel`. A reference to an undeclared human is refused. | `[human role "<name>" may be asked]`, with ` (may: …)` when `may` is set |
| `!channel` | `interface` | container, leaf, set | YAML | Declaration. | `[channel "<name>" is available for <target>]` |
| `!peer` | `identity` | container, leaf, set | YAML | Acts: one `a2a.peers[]` entry. | nothing |
| `!policy` | `identity` | container | YAML | Declaration. | nothing |
| `!secret-ref` | `identity` | leaf, set | none | Acts: a string `"@secret-ref/<name>"` anywhere in the folded configuration becomes `{{secret:<name>}}`, or `{{secret-file:<path>}}` when the block has `kind=file`. | nothing |
| `!runtime` | `compute` | container | YAML | Declaration. `network: any` is refused. | nothing |
| `!function` | `compute` | container, section | Code | Declaration. Requires `runtime`. No tool is registered for it. | `[tool "<name>" is available]` |
| `!test` | `compute` | container, section | Markdown | Declaration. Requires `target`. | nothing |
| `!fixture` | `compute` | container | Table | Declaration. | nothing |
| `!git` | `infra` | container, leaf | YAML | Declaration. | nothing |
| `!volume` | `infra` | leaf | none | Declaration. | nothing |
| `!image` | `infra` | leaf | none | Declaration. Requires `digest=sha256:…`; a mutable tag is refused. | nothing |
| `!agent` | `compose` | container, leaf, set | YAML | Acts: a step's `template: "@agent/<name>"` takes the block's `template`. It references a subagent template; it cannot define one. | `[subagent "<name>" is available]` |

A set of machinery delivers one line naming its members, such as
`[2 event streams are declared: orders, refunds]`. A set of a kind with no
acknowledgement delivers nothing.

In any machinery body, a line holding a recognisable literal credential (an
API key, an AWS access key, a GitHub or Slack token) is refused:
`a literal credential is never allowed (API key) — use a secret-ref`.

### Sub-blocks

A sub-block lives inside its parent's body, is written without the sigil,
and has no identity of its own in the document. The parent's fence must be
longer than the sub-block's:

```markdown
::::!mcp{name=crm}
endpoint: "https://crm.example.com/mcp"

:::override{target=delete_contact}
disabled: true
:::

:::override{target=send_email}
tags: [egress]
description: "Sends email to a customer. Confirm the address first."
:::
::::
```

| Sub-block | Parent | What it does |
|---|---|---|
| `override{target=<tool>}` | `!mcp` | Narrows one of the server's tools: `disabled: true` adds it to `tools.disabled`; `tags` and `description` add trifecta tags and an operator annotation under `tools.narrow`. It can only make a tool more careful: `disabled: false` is refused. Needs no grant. Delivers nothing. |
| `case` | `!test` | One test case. |
| `signature` | `!function` | The function's signature. |
| `schema` | `!ui` | The schema a `schema: "@ui/<name>"` step reference takes. |
| `preview` | `!ui` | A preview of the interface. |

## Prose kinds

Prose kinds need no grant and carry no sigil. A rule may be named in
brackets (`MUST[escalate]: …`) or with `name=`, so that other blocks can
refer to it; the name is never delivered.

| Kind | Keyword | Forms | Delivered as (bold style) |
|---|---|---|---|
| `must` | `MUST` | container, keyword, alert | `**MUST:** …` |
| `never` | `NEVER`, `MUST NOT` | container, keyword, alert | `**NEVER:** …` |
| `should` | `SHOULD`; `SHOULD NOT` or `{not}` | container, keyword, alert | `**SHOULD:** …`, or `**SHOULD NOT:** …` |
| `may` | `MAY` | container, keyword, alert | `**MAY:** …` |
| `always` | `ALWAYS` | container, keyword, alert | `**MUST:** …` |
| `avoid` | `AVOID` | container, keyword, alert | `**SHOULD NOT:** …` |
| `guardrail` | `GUARDRAIL` | container, keyword, alert | `**GUARDRAIL:** …` |
| `note`, `info` | `NOTE`, `INFO` | container, keyword, alert | `**NOTE:** …` |
| `tip`, `important`, `warning`, `caution` | the same in capitals | container, keyword, alert | `**TIP:** …` and so on |
| `example` | `EXAMPLE` | container, keyword, alert | `**EXAMPLE:**` on its own line, then the body as written; `**EXAMPLE (avoid):**` with `{avoid}` |
| `context` | — | container | The body inside `<reference>` tags, with the `title` attribute when given |
| `output` | — | container | `**OUTPUT (<format>) — <title>:**` on its own line, then the body. Its `schema` attribute is never delivered. |
| `glossary` | — | container (definition list) | One `**term** — definition` line per entry |
| `form` | — | container | `**Inputs to collect — <title>**`, then the parameters its body references, with their descriptions |
| `tool` | — | container, leaf | Its label, capability and allow/deny policy, then the body. Requires `cap`. |

A rule's strength orders overrides: `guardrail` is strongest, then `never`,
`must` and `always`, then `should` and `avoid`, then `may`.

## Structural kinds

| Kind | Forms | What it does |
|---|---|---|
| `when` | container | Delivers its body when every [fact](#facts-for-when-and-unless) it names matches. A fact agentd does not supply keeps the body. A value lists admissible values: `agent="claude, gpt"` matches either. |
| `unless` | container | Delivers its body unless every fact it names is known and matches. |
| `otherwise` | container | After a run of `when`/`unless` blocks, delivers its body when nothing in the run was kept. |
| `param` | leaf, set | Declares a typed parameter: `::param{name=limit type=number default=5}`. Requires `name`. |
| `include` | leaf | Transcludes another document by `id` or `uri`. agentd resolves none when it delivers a document itself, so the model reads `> _(included instruction not available)_`. |

## What the model reads

agentd delivers a document only when it holds something delivery renders,
removes or refuses: a block, a keyword or alert line, a `BECAUSE:` line, an
inline reference, an author note, front matter or end matter. Otherwise the
model reads it byte for byte. This table shows what each revision 1.1
construct becomes, in the default bold label style.

| Construct | Written | The model reads |
|---|---|---|
| Named rule | `MUST[escalate]: page the on-call.` | `**MUST:** page the on-call.` |
| Condition | `MUST (if the customer is on a paid plan): reply within an hour.` | `**MUST (if the customer is on a paid plan):** reply within an hour.` |
| Reason | a `BECAUSE:` paragraph after a rule, or `because=` on a container | `**BECAUSE:** …` on the line after the rule |
| Negated rule | `SHOULD NOT: …` or `:::should{not}` | `**SHOULD NOT:** …` |
| Aliases | `ALWAYS: …`, `AVOID: …` | `**MUST:** …`, `**SHOULD NOT:** …` |
| Variants | `:::unless{model="gpt-5.1"}`, then `:::otherwise` | the `unless` body unless the model is `gpt-5.1`; else the `otherwise` body |
| Output | `:::output{format=json title="Reply"}` | `**OUTPUT (json) — Reply:**`, then the body |
| Example to avoid | `:::example{avoid}` | `**EXAMPLE (avoid):**`, then the body as written |
| Typed parameter | `::param{name=limit type=number default=5}` and `${limit}` | `5`. A value that does not fit the type leaves the placeholder. |
| Override | `:::must{name=b overrides="must/a"}` | rule `b`; rule `a` and its reason are not delivered. A guardrail, or a rule stronger than its overrider, cannot be overridden. |
| Label style | front matter `delivery: {labels: plain}` or `{labels: tags}` | `MUST: x`, or `<must>x</must>` |
| Inline reference | `[[must/escalate]]`, `[[human/lead\|the lead]]`, `[the lead](#human/lead)` | `escalate`, `the lead`, `the lead` |
| Author note | `<!-- … -->` at column 0 | nothing |
| End matter | a closing `---` YAML block | nothing |
| Machinery | `:::!workflow{name=digest}` … | its acknowledgement line |

Examples and `verbatim` bodies keep their keywords and references as written.
A `${…}` inside fenced code is not substituted. Inline references degrade in
prose, never inside a code span or fenced code.

### Facts for when and unless

agentd supplies three of the specification's context keys:

| Key | Value |
|---|---|
| `host` | Always `agentd`. |
| `model` | The model the agent's turns use, as the provider receives it: `intelligence.roles.turn`, else `intelligence.model`, with a declared model resolved to its `model`. |
| `agent` | That model's family, when its id names one: `claude`, `gpt` (`gpt-*`, and `o1`, `o3`, `o4-…`), `gemini`, `llama` or `mistral`. |

`environment` and `locale` are not supplied, and neither is `agent` for a
model id agentd does not recognise. A condition on a key that is not supplied
keeps its content. The facts come from the operator's `intelligence` section,
read before the document's `:::!config` merges under it. A re-pulled OCI
document and a registry read are decided by the same facts. A subagent
template is decided by the model its child runs: the template's own `model:`,
else `subagents.defaults.model`, else the agent's.

## Grants

`agent.document_capabilities` lists the extended families a document may use.
It is operator-only and restart-only.

| Family | Kinds |
|---|---|
| (none needed) | `!workflow`, `!skill`, `!config`, `!mcp`, `!stream`, `!tools`, `!eval`, `!data`, and the `override` sub-block |
| `material` | `!file`, `!media`, `!asset` |
| `knowledge` | `!knowledge`, `!retrieval`, `!source` |
| `interface` | `!endpoint`, `!ui`, `!human`, `!channel` |
| `identity` | `!peer`, `!policy`, `!secret-ref` |
| `compute` | `!runtime`, `!function`, `!test`, `!fixture` |
| `infra` | `!git`, `!volume`, `!image` |
| `compose` | `!agent` |

A block from an ungranted family refuses the document:

```text
line 1: `:::!file` needs the `material` capability — add it to `agent.document_capabilities`
```

The families a document may use are the grant, intersected with the source's
`max_capabilities`, with what the author signature attests when a publisher
is pinned, and with what the delivery signature attests on a registry read
with a `reader`. A signed folder is capped by its narrowest file.

A document that arrived over the wire (a `url:`, `oci:` or `mcp:` source, or
a subagent template's `url:` or `oci:` source) never uses `compose` or
`identity`, whatever the grant or the signature says: the specification's
floor. Each such block is refused:

```text
line 3: `:::!agent` is in the `compose` family, which a document that arrived over the wire never uses (§7.8) — declare it in the operator's configuration
```

Nor does such a document reach them through the blocks every document may
use: a `url:` or `oci:` document whose `:::!config` writes `a2a.peers`, or
that names a secret (`{{secret:…}}`, `{{secret-file:…}}`, `@secret-ref/…`)
in a header, a step or a declaration, is refused at load, since the secret
would go wherever the document points it.

> **Note.** A name outside this table grants nothing and is not refused at
> load. Spell the family exactly: `compose`, not `composition`.

## Configuration a document may write

A `:::!config` fragment, and every machinery block that folds into
configuration, may write only these paths. A path is checked with everything
under it.

| A document may write | Why |
|---|---|
| `workflows`, `streams`, `mcp`, `vars` | What the agent does. |
| `a2a.peers` | Another agent this one dials: an outbound connection, like an MCP server. |
| `context`, `goal`, `knowledge`, `search`, `memory`, `limits` | How it thinks, remembers and bounds itself. |
| `intelligence.model`, `.models`, `.roles.turn`, `.roles.think`, `.roles.decide`, `.roles.preflight`, `.roles.compaction`, `.roles.goal`, `.dialect`, `.budget`, `.timeout`, `.structured_output`, `.swap_policy` | Which model, and how much of it. |
| `agent.name`, `.approval`, `.ask_human_fallback`, `.ask_human_unowned`, `.conversation_budget`, `.max_parallel_turns`, `.on_workflow_finished`, `.preflight`, `.wake_on` | The agent loop's own shape. |
| `tools.narrow`, `tools.disabled` | `narrow` only adds trifecta tags and annotations; `disabled` only takes tools away. |
| `skills.max_bytes`, `.max_loaded`, `.reference_prefix` | Caps on the skill loader. |
| `store.kind`, `.durability`, `.checkpoint`, `.on_error`, `.timeout`, `.max_value_bytes`, `.prefix` | The durability class and the caps around it, never the place. |
| `lifecycle.run_until`, `.idle_grace`, `.until_signal` | When the agent is finished. |
| `observability.log_level`, `.runtime_events` | How loud the log is, and routing runtime events into a stream the document declared. |

Everything else is the operator's. A path on neither list is refused too.

| Operator only | Why |
|---|---|
| `agent.document_capabilities` | The grants that decide which blocks this document may use. |
| `agent.instruction`, `agent.prompt` | The source, trust pins and decryption keys. A document that rewrote them would point the next read at itself. |
| `agent.description` | What the public Agent Card tells strangers this agent is. |
| `agent.gate_channels` | Where gates the document did not declare are announced. A document announces its own by declaring the `::!human`. |
| `agent.tools`, `tools.overrides` | The tool grant, and re-routing a built-in onto a server. Lists concatenate when merged, so a document could only widen the grant. |
| `security` | The gates: trifecta, egress, exec, policies, TLS trust, AAuth. |
| `services` | The catalogue `security.egress: closed` is checked against. |
| `identity` | Who work is done on behalf of. |
| `a2a.listen`, `.url`, `.tls`, `.bearer`, `.principals`, `.device_grant`, `.cors`, `.push`, `.events`, `.introspection` | Who may talk to this agent, as what, over which socket, and what the listener shows its clients. |
| `webhooks` | Inbound sockets and the auth on them. |
| `intelligence.endpoints`, `.token`, `.token_file`, `.headers`, `.auth`, `.providers` | Where model calls go, and the credential they go with. |
| `intelligence.roles.approve` | The model that answers asks on the operator's behalf. |
| `intelligence.routes` | The models preflight may move a turn onto. |
| `subagents` | A whole child agent. A document references a template; it does not define one. |
| `store.file`, `.http`, `.mcp`, `.retention` | Where state lives, and how long finished work is kept. |
| `lifecycle.drain_timeout`, `.exit_code_map`, `.run_id`, `.watch_config` | The orchestration contract. |
| `observability.log_content`, `.audit`, `.otel`, `.metrics_addr`, `.health_file`, `.events_ring`, `.traceparent`, `.status_values` | Conversation text in the log, telemetry sinks, sockets and files on the host, and which memory status readers see. |
| `skills.dir`, `skills.sources` | Where skills are read from. |

A refusal names every path the document wrote that it may not:

```text
the instruction's :::!config writes intelligence.endpoints — operator configuration is not a document's to set (§6 rule 4). …
```

### Further rules for documents

| Rule | Refusal or effect |
|---|---|
| A model a document declares names no `provider`. | `writes intelligence.models.<name>.provider` is refused. |
| A document cannot bind or grant MCP Events. | `the instruction declares mcp.servers[<name>].events — MCP Events bindings and grants are the operator's to declare` |
| A workflow with a `webhook` start or a `wait {on: webhook}` needs the `interface` grant, however the document writes it, including workflows it references by `file:`, `url:`, `uri:` or `dir:`. | `… opens an inbound route on the operator's webhook listener — a served document may open one only under the interface grant` |
| The fragment merges under the operator's configuration. | At every key, a config file, environment variable or flag wins. |
| A turn model the operator names wins, in either spelling (`intelligence.model`, `intelligence.roles.turn`). | The document's is dropped. A warning names a spelling the operator did not write. |
| MCP servers a document declares are added to the operator's list. | A name the operator already uses is refused: `duplicate server name`. |
| Two definitions of one workflow name, from any two sources, are refused. | The message names the workflow and both sources. |
| A subagent template's machinery faces the same boundary. | It also may not write `a2a`, `intelligence`, `store` or `lifecycle`, which the parent composes for the child. |

## Instruction source settings

`agent.instruction` is the agent's standing instruction. It takes the short
form, a single value, or the long form, an object naming one source plus its
settings. It is reloadable, except `trust`, which is restart-only.

### Sources

The long form names exactly one source. Naming two is refused; naming none,
when the object has settings, is refused too. To run without an instruction,
omit `agent.instruction`.

| Key | Flag | Value | Read |
|---|---|---|---|
| `text` | `--instruction.text` | The instruction itself, never read as a path or URI. | — |
| `file` | `--instruction.file` | A path. `~/` expands to `$HOME`. | At load and on each reload. |
| `dir` | `--instruction.dir` | A folder path, or `{path, glob, order}`. | At load and on each reload. |
| `dir.glob` | `--instruction.glob` | Comma-separated globs relative to `path`. `**` recurses. Default `*.md,*.markdown,*.txt,*.instruction`. | — |
| `dir.order` | `--instruction.order` | `name` (default, path order) or `date` (modification time, oldest first). | — |
| `url` | `--instruction.url` | An `https://` URL (plain `http://` to a loopback host only), fetched with a plain `GET` and no credential. Up to 3 redirects are followed, each held to the same rule and to `security.egress`; a redirect from `https://` to `http://` is refused, and so is a body over 8 MiB. | At load and on each reload. |
| `oci` | `--instruction.oci` | `ghcr.io/acme/agent:v3` or `…@sha256:…` (`oci://` implied), or `{ref, cosign_key}`. | At load; a tag is polled. |
| `mcp` | `--instruction.mcp` | A resource URI any connected server serves, or `{server, resource, events}`. `events` lists MCP Events types, as a name or `{name, arguments}`, whose occurrences re-read the instruction. | At runtime; followed by notification, poll and events. |

### Short form

A short-form value is classified, first match wins:

| The value | Read as |
|---|---|
| contains a newline | text |
| `file://…` | a file |
| a single-token `oci://…` | an OCI artifact |
| any other single-token `scheme://…`, `https://` included | a resource an MCP server serves |
| no whitespace, ending in `.md`, `.markdown`, `.txt` or `.instruction` | a file |
| no whitespace, starting `/`, `./`, `../` or `~/`, and ending in `/` or naming an existing folder | a folder |
| no whitespace, starting `/`, `./`, `../` or `~/` | a file |
| anything else | text |

A value read as a file that does not exist is refused, naming both readings.

`agent.prompt` and a subagent template's `instruction` take the same
classification and the load-time sources `text`, `file`, `dir`, `url` and
`oci`. For them, a short-form `https://` value is fetched as `url`, and an
`mcp` source is refused. `agent.prompt` drops end matter from every source,
with a warning; nothing else in a prompt is processed.

### Behaviour

| Key | Flag | Values | Default |
|---|---|---|---|
| `refresh` | `--instruction.refresh` | `auto`, `off`, or a duration. | `auto` |
| `unavailable` | `--instruction.unavailable` | `auto`, `keep`, `freeze`, `drain`, `exit`. | `auto` |
| `unenforceable` | `--agent-instruction-unenforceable` | `warn`, `refuse`, `ignore`. | `warn` |
| `decrypt.keys` | `--agent-instruction-decrypt-keys` | Key files, each an `AGE-SECRET-KEY-1…` identity, 64 hex characters, or base64. | — |
| `decrypt.passphrase` | `--agent-instruction-decrypt-passphrase` | A `{{secret:NAME}}` reference, for age scrypt envelopes. | — |
| `trust` | `--agent-instruction-trust` | A list of trust pins. Restart-only. | — |

`refresh: auto` does this per source:

| Source | `auto` |
|---|---|
| `text` | Nothing to re-read. |
| `file`, `dir` | No poll. Watched when `lifecycle.watch_config` is on; a change reloads. |
| `url` | No poll. Fetched again on each reload. |
| `oci` with a tag | Polls every 5 minutes. |
| `oci` with a digest | Never re-read. |
| `mcp` | `notifications/resources/updated`, any `events`, and a poll every 5 minutes. |

A duration or `off` changes the poll for `oci` and `mcp` sources. A trust
pin's `freshness` still re-checks the source when `refresh` is `off`. A
re-read updates the delivered text; machinery applies on a reload or restart.

`unavailable` applies after startup, once the source has gone unconfirmed
past its deadline:

| Value | Behaviour |
|---|---|
| `keep` | Carry on with the last good instruction. |
| `freeze` | Finish live work, refuse new work until a re-read succeeds. |
| `drain` | Finish live work, then exit (`0` for a daemon). |
| `exit` | Exit `6` at once. |
| `auto` | `freeze` when a trust pin names a publisher for this source, `keep` otherwise. |

At startup an unreadable source stops the start: exit `2` for a load-time
source, exit `6` for an `mcp` source. See [CLI, environment and exit
codes](cli.md) for every exit code.

`unenforceable` says what a trust pin means when it names no key file this
load can use: its author keys are registry `instruction://…keys.json` URIs,
or none at all. `warn` logs one warning and loads the document unverified;
`refuse` exits `2`; `ignore` says nothing. A key file that cannot be read is
not this case: the document is refused, exit `2`, whatever `unenforceable`
says.

### Trust pins

Each entry of `agent.instruction.trust`:

| Field | Meaning |
|---|---|
| `uri` | The document the pin applies to, such as `instruction://acme/support`. A signature's `doc` claim must name it. |
| `publisher` | The publisher the author signature must claim. A pin without one enforces nothing. |
| `author_keys` | Author verification keys: files holding a raw 32-byte, hex or base64url Ed25519 public key, or `instruction://…keys.json` URIs a registry serves. Empty on a registry source: discovered from the read's `publisherKeys` pointer. |
| `delivery_keys` | Delivery verification keys, in the same forms. |
| `reader` | This agent's reader id, such as `agent://support-desk`. Setting it turns on the delivery check on registry reads. |
| `max_capabilities` | This source's ceiling on the grant. |
| `freshness` | The revocation deadline, a duration. When `refresh` is shorter, the shorter wins. |

Signatures are JWS compact serializations, Ed25519 (`alg: EdDSA`), over
digests written `sha256:<hex>`. The author digest is the document's bytes
with only the front-matter `signature:` line left out, so end matter is
signed with the text. The manifest a delivery signature embeds must be the
specification's S7 signed form; a delivery that embeds any other shape is
refused wherever a pin sets `reader`. Under a pin, a registry read of a
version the registry marks revoked is refused, and the running instruction is
kept.

A refused verification carries the specification's Appendix B code in the
`agentd-instruction` crate: `signature-typ-mismatch`, `audience-mismatch`,
`digest-mismatch`, `manifest-dropped-missing`, `unpinned-publisher`,
`delivery-ceiling-exceeded`, `wire-floor` or `ungranted-family`, and
`attestation` for a condition without a row, such as a signature that does
not parse or has expired. agentd logs the refusal's message.

A binary built without the `sign` feature refuses any pin with a publisher.
The release binaries include it.

## Log events

These log lines follow an instruction through its life. See
[Events and metrics reference](telemetry-reference.md) for the full catalogue.

| Event | When | Fields |
|---|---|---|
| `instruction.loaded` | An OCI, folder or MCP instruction is read. | `uri` and `manifest_digest` (OCI); `dir`, `files`, `glob`, `order` (folder); `server`, `uri`, `version_id`, `resolution` (MCP); `bytes`, `version` |
| `instruction.verified` | A registry read's signatures verify. | `uri`, `publisher`, `kid`, `capabilities`, `delivery_checked` |
| `instruction.applied` | A registry read moves to a new version. | `uri`, `old_version_id`, `new_version_id`, `delivered_digest` |
| `instruction.updated` | A re-read of an MCP instruction changed its text. | `version` |
| `instruction.event` | An MCP Events occurrence triggers a re-read. | `server`, `event_type` |
| `instruction.reread.fail` | A re-read failed. | `server`, `uri`, `reason`, `err` |
| `instruction.subscribe.unsupported` | The serving MCP server offers no subscriptions. | `server`, `uri` |
| `instruction.unavailable` | The source went unconfirmed past its deadline. | `uri`, `server`, `err`, `policy`, `trust_pinned` |
| `freshness.armed` | The re-read timer is set. | `every_ms` |
| `freshness.recovered` | A re-read succeeded after a freeze. | `uri` |
| `skills.discovered` | Inline skills joined the catalogue. | `origin: "instruction"`, `skills` |

With `agent.wake_on` including `instruction_updated`, a changed instruction
also leaves a note for the root conversation.

## Next steps

- [Write and ship instruction documents](instruction-documents.md): the
  task-by-task guide.
- [Instruction documents](directives.md): why the boundary and the trust rule
  are drawn where they are.
- [Workflow language reference](workflow-language.md): what a `!workflow`
  body may contain.
- [Configuration reference](configuration.md): every other setting.
