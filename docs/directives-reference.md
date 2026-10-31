# Instruction document reference

agentd reads instruction documents in the instruction.md specification's
first edition, `instruction/1` (`spec: "1"`), with the official instruction.md
Rust SDK (`instruction-md`), pinned to specification commit `af8400f`. The
syntax, the checks and the agent text are the specification's and the SDK's;
this page is what agentd, as a host, does with a document: which blocks
become configuration, under which grants, and what it says about the rest.
For the task-by-task guide see
[Write and ship instruction documents](instruction-documents.md).

## The document

A document is Markdown with optional front matter (a YAML mapping between
`---` lines at the top) and optional backmatter (a `---` separator after a
blank line, then a YAML mapping whose first key is one of `authors`,
`owners`, `review`, `history`, `sources`, `approvals`, `notes`,
`test_results`, to the end of the file). Neither reaches the model.

Blocks are directives: a container `:::kind{attrs}` … `:::`, a leaf
`::kind{attrs}` on its own line, or a keyword line (`MUST:`, `SHOULD NOT:`,
`MUST (if …):`, `BECAUSE:`). Every block that declares something has an
`id`. The specification's registry defines the kinds, their attributes and
their families; an unknown kind, an unclosed block, an invalid attribute and
a duplicate `id` are errors and refuse the whole document.

The older `:::!kind` form is not `instruction/1` syntax: a document with such
a line is refused, naming it (`instruction.md/legacy-syntax`).

An `instruction/workflow` body is checked by the SDK against the language's
node registry (`invalid-workflow`, `limit-exceeded`,
`extension-unavailable`). What a valid workflow asks of this host — a start on
an event agentd raises, a server it declares — is agentd's own check, at
load.

## Blocks agentd maps

Configuration is taken from the document's top level and from inside its
layout blocks (`section`, `tabs`, `columns`, `grid`, `card`, `details`,
`steps`, …), never from inside an `example`, a `policy` or any other block
whose body is guidance.

| Block | Becomes | Grant |
|---|---|---|
| `:::mcp{id src tools allow deny}` (or `::mcp`) | an `mcp.servers` entry: `id` → `name`, `src` → `endpoint`, `tools` and `allow` → `allow`, `deny` → `exclude` | none |
| `::agent{id protocol=a2a endpoint binding src}` | an `a2a.peers` entry named `id`: `endpoint`, or the origin of an Agent Card `src` at `/.well-known/agent-card.json` | `identity` |
| `:::workflow{id dialect="instruction/workflow"}` | a `workflows[]` entry; the body is the definition, its `name` defaulting to the `id`. `agentd/workflow` is the same dialect. | none; `interface` for a route |
| `:::config{id dialect="agentd/config"}` | the document's configuration fragment, merged under the operator's | none |
| `:::skill{id title trigger}` with a body, no `src` | an inline skill named `id`: `description` (or `title`) and `trigger` | none |
| `::person{id}`, `::role{id}` | a person a `human` step may be addressed to: `to: <id>` or `to: "#<id>"` is answered by the operators | none |

Every other block is guidance, delivered as agent text and configuring
nothing: rules, policies, context, examples, layout, resources, data,
`::include` (agentd does not load what a document composes).

## Host-policy diagnostics

A connected object agentd will not activate stays in the agent text,
unresolved, and is reported as a host-policy diagnostic (§17): a
`config.warning` naming the line, the kind and `id`, the reason, and the code
`host-policy`. The load continues.

| Not activated | Why |
|---|---|
| `::agent` without the `identity` grant, or from a document that arrived over the wire | the grant, or the §7.8 floor |
| `::agent` with `protocol` other than `a2a`, a `binding` other than `JSONRPC`, or a `tenant` | agentd speaks A2A's JSON-RPC binding and takes the tenant from the card |
| `::agent` whose `src` is not an Agent Card at `/.well-known/agent-card.json` | agentd finds an agent by its endpoint or that card |
| `:::mcp` with no `src` (a `registry` or `server` reference) | agentd dials a server by its URL |
| `:::workflow` or `:::config` in another dialect | agentd runs `instruction/workflow` and reads `agentd/config` |
| `:::skill` with a `src` | packaged skills come from the configuration's `skills:` |

`::tool`, `::service`, `::source`, and every object above, are listed by kind
under `document.declarations` in `agentd --capabilities`.

## What the model reads

The model reads the agent text (`instruction-text/1`) the SDK produces from
the document, byte for byte:

| Construct | The model reads |
|---|---|
| Front matter, backmatter, comments | nothing |
| A rule | its canonical prefix (`MUST:`, `SHOULD NOT:`, `NEVER:`, `MAY:`, `GUARDRAIL:`), its condition as `(if …)`, then `BECAUSE: …` |
| `${name}` | the supplied or declared default value, where the specification permits substitution |
| An inline reference `:mcp[label]{ref=#id}` | `label (mcp; #id)` |
| A policy, an instruction, a context | `### Policy: Title`, then its body |
| An example | `### Example: Title`, then its body quoted as Markdown |
| Any other block | `### kind: Title`, an `Attributes:` JSON line, then its body; a structured body (a workflow, a config) as a fenced block |

Plain prose with nothing the agent text would change is delivered as
written.

## Grants

`agent.document_capabilities` names what a document may use beyond the
default. `instruction/1` grants nothing by itself; these are agentd's.

| Grant | Admits |
|---|---|
| `identity` | `::agent` → `a2a.peers` |
| `interface` | a workflow with a `webhook` start or a `wait {on: webhook}`, however the document writes it |

A document that arrived over the wire (`url:`, `oci:`, `mcp:`) never uses
`identity`, whatever the operator granted or the author signed. Nor may it
write `a2a.peers` through a `:::config`, or name a secret
(`{{secret:…}}`, `{{secret-file:…}}`) in its configuration or workflows: the
load is refused, since the secret would go wherever the document points it.

A verified author signature caps the grant to the capabilities it attests;
it never widens it.

## Configuration a document may write

A `:::config` fragment, and every block that maps onto configuration, may
write only these paths. A path is checked with everything
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
| `agent.gate_channels` | Where gates are announced. A document never puts a channel on a gate. |
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
the instruction's :::config writes intelligence.endpoints — operator configuration is not a document's to set (§6 rule 4). …
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
| A subagent template's blocks face the same boundary. | It also may not write `a2a`, `intelligence`, `store` or `lifecycle`, which the parent composes for the child. |

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
`mcp` source is refused. `agent.prompt` drops backmatter from every source,
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
re-read updates the delivered text; configuration applies on a reload or restart.

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

Signatures are JWS compact serializations, Ed25519 (`alg: EdDSA`), with
`spec: "instruction/1"` claims, over digests written `sha256:<hex>`. The
author digest is the document's stored bytes as they are; the signature
travels beside the document: a registry read's metadata, a file's
`<path>.jws`, an OCI layer's `md.instruction/signature` annotation. A
delivery signature names its `instruction-resolved/1` manifest (format,
status, document, dependencies, resolved digest) and embeds the author
signature, whose digest must be the manifest document's. Under a pin, a
registry read of a version the registry marks revoked is refused, and the
running instruction is kept.

The checks are the instruction.md SDK's. A refused verification carries a
code agentd gives the SDK's reason: `signature-typ-mismatch`,
`audience-mismatch`, `digest-mismatch`, `unpinned-publisher`,
`delivery-ceiling-exceeded`, and `attestation` for any other, such as a
signature that does not parse or has expired. agentd logs the message.

A binary built without the `sign` feature refuses any pin with a publisher.
The release binaries include it.

## Log events

These log lines follow an instruction through its life. See
[Events and metrics reference](telemetry-reference.md) for the full catalogue.

| Event | When | Fields |
|---|---|---|
| `instruction.read` | A document is read at load. | `reader`, `spec`, `digest`, `diagnostics`, `agent_text_bytes`, `mcp_servers`, `a2a_peers`, `workflows`, `skills`, `people`, `not_activated` |
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
- [Workflow language reference](workflow-language.md): what a `:::workflow`
  body may contain.
- [Configuration reference](configuration.md): every other setting.

