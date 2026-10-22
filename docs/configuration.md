# Configuration reference

This page lists every configuration path agentd accepts: its type, its
default, whether a reload applies it, and whether an instruction document may
set it. It is for operators writing a config and for anyone reviewing one. For
why configuration is layered and validated the way it is, read
[How configuration works](configuration-model.md); for the flags that are not
settings and the exit codes, read [CLI, environment and exit codes](cli.md).

## How to read this reference

Each section below is one top-level key of the document. Its table names each
path in full, so a search for `limits.run.steps` lands on its row.

| Column | Meaning |
|---|---|
| **Path** | The dotted path. `[]` marks an element of a list, `<name>` a key you choose in a map. |
| **Type** | `string`, `int`, `number`, `bool`, `duration`, `rate`, `size`, `secret`, a list (`[…]`), a map, or an enum written as `a \| b`. |
| **Default** | What applies when the path is unset. *unset* means the feature it controls is off. |
| **Class** | `live` — a reload applies it; `restart` — a reload that changes it is refused. Then `doc` — an instruction document's `:::!config` fragment may set it too; or `op` — operator only. |

The value syntax — durations, sizes, rates, secret references — is in
[Value syntax](#value-syntax). `agentd --config-schema` prints the same paths as
a JSON Schema (Draft 2020-12); when this page and that output disagree, the
binary is right.

> **Note.** Release binaries carry every build feature except `exec` and
> `internal-mocks`. A path that needs a feature says so; what each feature
> gates is in [Build features and crates](features.md).

## Paths, environment variables and flags

Every path is settable three ways, with names derived from the path. Nothing is
plumbed per field, so a path added to the schema gets all three.

| Source | Spelling for `limits.run.steps` |
|---|---|
| File (YAML or JSON) | `limits: {run: {steps: 40}}` |
| Environment | `AGENTD_LIMITS_RUN_STEPS=40` |
| Flag | `--limits.run.steps 40` — `.`, `_` and `-` are interchangeable, so `--limits-run-steps 40` is the same flag; `--flag=value` works too |

`agentd --help` prints the whole list under `CONFIG PATHS`. Precedence is
built-in default, then files, then environment, then flags; each layer changes
only the keys it sets.

Values from the environment and flags are typed by the schema:

| Path type | Accepted spellings | Example |
|---|---|---|
| int, number, bool | the literal | `AGENTD_LIMITS_MAX_RUNS=4` |
| enum | one of its values | `--lifecycle.run-until drained` |
| list | `[a, b]` or `a, b` | `AGENTD_TOOLS_DISABLED=exec,code.run` |
| object or map | a `{k: v}` flow literal, or JSON | `--mcp-servers '[{name: fs, endpoint: "https://mcp-fs.internal/mcp"}]'` |
| anything else | the verbatim string | `AGENTD_AGENT_NAME=triage` |

These rules hold for every path:

- A value that does not type is exit 2, naming its source:
  `invalid AGENTD_LIMITS_RUN_STEPS: expected an integer, got "many"`.
- Setting a list or map path **replaces** what the files declared. The named
  repeatable flags (`--mcp`, `--a2a-peer`, `--workflow`) **add** one element.
- A dotted flag reaches into a free-form map and sets one entry:
  `--intelligence.headers.x-team ops` keeps the other headers.
- List elements are not addressable: `--mcp-servers.0.name` is refused. Set
  the whole list, or use the named flag.
- An unknown flag is `unknown argument` and an unknown file key is
  `unknown field`, both exit 2. An `AGENTD_` variable that names no path is
  not read.

## Short aliases

The aliases below are shorter spellings for the paths operators reach for most.
The short environment name is read under the same `AGENTD_` prefix.

| Flag | Path | Short env var |
|---|---|---|
| `--instruction` | `agent.instruction` | `AGENTD_INSTRUCTION` |
| `--instruction.text`, `.file`, `.dir`, `.url`, `.oci`, `.mcp` | `agent.instruction.<source>` | — |
| `--instruction.glob`, `--instruction.order` | `agent.instruction.dir.glob`, `.dir.order` | — |
| `--instruction.refresh`, `--instruction.unavailable` | `agent.instruction.refresh`, `.unavailable` | — |
| `--prompt` | `agent.prompt` | `AGENTD_PROMPT` |
| `--prompt.text`, `.file`, `.dir`, `.url`, `.oci` | `agent.prompt.<source>` | — |
| `--prompt.glob`, `--prompt.order` | `agent.prompt.dir.glob`, `.dir.order` | — |
| `--intelligence` | `intelligence.endpoints` | `AGENTD_INTELLIGENCE` |
| `--intelligence-token` | `intelligence.token` | `AGENTD_INTELLIGENCE_TOKEN` |
| `--intelligence-token-file` | `intelligence.token_file` | `AGENTD_INTELLIGENCE_TOKEN_FILE` |
| `--model` | `intelligence.model` | `AGENTD_MODEL` |
| `--model-swap` | `intelligence.swap_policy` | `AGENTD_MODEL_SWAP` |
| `--budget-tokens-lifetime` | `intelligence.budget.lifetime_tokens` | `AGENTD_BUDGET_TOKENS` |
| `--mcp name=<endpoint>` | `mcp.servers` (adds one) | — |
| `--mcp-tags name=tag,tag` | `mcp.servers[].tags` of a declared server | — |
| `--a2a-peer name=<endpoint>` | `a2a.peers` (adds one) | — |
| `--workflow <file>` | `workflows` (adds `{name: <file stem>, file: <file>}`) | — |
| `--max-steps` | `limits.run.steps` | `AGENTD_MAX_STEPS` |
| `--max-tokens` | `limits.run.tokens` | `AGENTD_MAX_TOKENS` |
| `--deadline` | `limits.run.deadline` | `AGENTD_DEADLINE` |
| `--max-depth` | `limits.subagents.depth` | — |
| `--run-id` | `lifecycle.run_id` | `AGENTD_RUN_ID` |
| `--drain-timeout` | `lifecycle.drain_timeout` | `AGENTD_DRAIN_TIMEOUT` |
| `--watch-config` | `lifecycle.watch_config` | `AGENTD_WATCH_CONFIG` |
| `--budget-exit-code <N>` | `lifecycle.exit_code_map` (sets `{"3": N, "7": N}`) | — |
| `--listen` | `a2a.listen` | `AGENTD_A2A_LISTEN` (the derived name) |
| `--log-level` | `observability.log_level` | `AGENTD_LOG_LEVEL` |
| `--log-content` | `observability.log_content` | `AGENTD_LOG_CONTENT` |
| `--metrics-addr` | `observability.metrics_addr` | `AGENTD_METRICS_ADDR` |
| `--health-file` | `observability.health_file` | — |
| `--events-ring` | `observability.events_ring` | — |
| `--traceparent` | `observability.traceparent` | `AGENTD_TRACEPARENT` |
| `--allow-trifecta` | `security.allow_trifecta` | `AGENTD_ALLOW_TRIFECTA` |
| `--tls-ca` | `security.tls_ca` | `AGENTD_TLS_CA` |
| `--aauth-provider`, `--aauth-key-file`, `--aauth-enroll-token`, `--aauth-enroll-assertion-file`, `--aauth-person-server` | `security.aauth.*` | — |
| `--cgroup`, `--cgroup-memory-max`, `--cgroup-pids-max` | `security.cgroup.spec`, `.memory_max`, `.pids_max` | — |

These environment variables are read but are not paths:

| Variable | Meaning |
|---|---|
| `AGENTD_CONFIG` | Config files to load, `:`-separated (`a.yaml:b.yaml`), before any `--config`. |
| `AGENTD_INTELLIGENCE_TOKEN_<N>`, `AGENTD_INTELLIGENCE_TOKEN_<N>_FILE` | The credential for endpoint *N* of the top-level connection (endpoint 1 uses the bare names above). Never applied to `intelligence.providers`. |
| `AGENTD_STATE_DIR` | The file store's root when `store.file.path` is unset. |
| `AGENTD_POD_NAME` | The instance name when `agent.name` is unset (set it from the Kubernetes downward API). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The OTLP endpoint when `observability.otel.endpoint` is unset. |

The control flags (`--validate-config`, `--effective-config`, `--config-schema`,
`--login`, `--env`, `--fresh` and the rest) and the `tui`/`ui` launchers are in
[CLI, environment and exit codes](cli.md).

## Files, discovery and merge

### File formats

`--config <path>` (or `-c`, repeatable) loads YAML or JSON. The extension picks
the syntax: `.yaml` and `.yml` are YAML, `.json` and `.jsonc` are JSON with
`//` and `/* */` comments. Any other extension is sniffed: a document starting
with `{` or `[` is JSON.

YAML is read by agentd's own subset reader: mappings, sequences, flow
collections, quoted and plain scalars, `|` and `>` block scalars, comments,
YAML 1.2 core typing (`yes` and `on` are strings). Anchors, aliases, tags,
merge keys, multi-document streams, tab indentation and duplicate keys are
refused with a line and column. A document carries no version field.

### The discovery chain

When no config is named — no `--config` and no `AGENTD_CONFIG` — agentd loads
every rung of this chain that has a file, in this order:

| Rung | Files looked for |
|---|---|
| user | `$XDG_CONFIG_HOME/agentd/config.yml` (or `.yaml`), else `~/.config/agentd/config.yml` (or `.yaml`) |
| project | `./agentd.yml`, `./agentd.yaml`, `./.agentd.yml`, `./.agentd.yaml` |
| local | `./agentd.local.yml`, `./agentd.local.yaml` |

- Naming a config by flag or environment skips the whole chain.
- The project and local rungs are the working directory only; there is no walk
  up to parent directories and no `/etc`.
- Two spellings of one rung (`agentd.yml` beside `agentd.yaml`) is exit 2.
- A discovered file may not relax a security control: one that sets
  `security.allow_trifecta` is exit 2. Pass `--config` if you meant it.
- A discovered file that sets `intelligence.endpoints` or adds an
  `intelligence.providers` entry loads with a warning naming the file.
- `--help`, `--version`, `--config-schema` and `--workflow-schema` never
  discover a config.

### Conventional folders

Four folders beside the config fill in a setting you did not write:

| Folder | Fills in | Files |
|---|---|---|
| `workflows/` | `workflows` | `*.yaml`, `*.yml`, `*.json`, one workflow document each |
| `skills/` | `skills.dir` | `<name>/SKILL.md`, one Agent Skill each |
| `subagents/` | `subagents.templates` | `*.yaml`, `*.yml`, `*.json`, one template each, named by file stem |
| `context/` | `context.templates` | `*.md`, `*.txt`, `*.hbs`, the whole body as the template, named by stem |

A folder applies only when its setting is absent (an explicit empty list
counts as set) and only when it yields at least one file. agentd looks beside
each loaded config file, the last one first, and uses the first directory that
has the folder; with no config file, the working directory. Files load in
filename order.

`.agents/skills/` beside a loaded config file is read as well, as the Agent
Skills project scope: its `<name>/SKILL.md` skills sit beside those of
`skills.dir`, and win a name over them. It has no key, and is never read
from the working directory alone. A skill in either folder that breaks an
Agent Skills rule refuses the configuration
([Agent Skills](interoperability.md#agent-skills)).

### Several files

The files in play are every entry of `AGENTD_CONFIG`, then every `--config` in
argument order. They merge into one document, later over earlier, with JSON
Merge Patch rules (RFC 7396):

| In the later file | Effect |
|---|---|
| an object | merges key by key, recursively |
| a scalar or a list | replaces the earlier value |
| `null` | unsets the key, so the built-in default applies |

Each file is type-checked on its own, so an unknown key names the file it is
in. The merged document is the file layer; environment and flags still apply
on top.

### Editor schema

The schemas are published so an editor can complete keys and flag typos:

| Document | Schema URL |
|---|---|
| a config file | `https://agentd.dev/schema/config.json` |
| a standalone workflow file | `https://agentd.dev/schema/workflow.json` |

Put a modeline on the first line of the file:

```yaml
# yaml-language-server: $schema=https://agentd.dev/schema/config.json
agent:
  name: triage
```

Or map files to schemas once in VS Code's `settings.json` (the YAML extension;
Neovim's `yamlls` takes the same map):

```jsonc
"yaml.schemas": {
  "https://agentd.dev/schema/config.json": ["agentd.yml", "agentd.yaml", "agentd.local.yml", "*.agentd.yaml"],
  "https://agentd.dev/schema/workflow.json": ["workflows/*.yaml", "workflows/*.yml"]
}
```

In JetBrains IDEs the mapping is under *Settings → Languages & Frameworks →
Schemas and DTDs → JSON Schema Mappings*. The schema does not
check rules that span fields, such as a model name that is not declared;
`--validate-config` does. `agentd --config-schema` and
`agentd --workflow-schema` print the same schemas the site publishes.

## Value syntax

### Durations

A duration is a number with an optional unit. A bare number is seconds.

| Unit | Example | Meaning |
|---|---|---|
| `ms` | `250ms` | milliseconds |
| `s` | `90s` | seconds |
| `m` | `5m` | minutes |
| `h` | `2h` | hours |
| `d` | `30d` | days |
| `w` | `2w` | weeks |
| (none) | `30` | seconds |

An empty value, an unparsable number or an unknown unit is exit 2
(`unknown duration unit 'x' in 2x`).

### Rates

A rate is `<burst>/<per>`, where `<per>` is a duration: `60/1m` is a burst of
60 refilling over a minute, `1/1d` one per day.

### Sizes

`store.file.min_free` and `security.cgroup.memory_max` take a size: plain
bytes, or a number with a unit such as `256MB`, `512M`, `2G` or `1.5GiB`.

### Secret references

A credential never appears in a config file as a literal. It is a reference,
resolved at startup and never logged:

| Reference | Resolves to |
|---|---|
| `{{secret:NAME}}` | the environment variable `NAME` |
| `{{secret-file:PATH}}` | the contents of the file at `PATH` |

From the environment or a flag, a credential may be inline. The paths that
must be references in a file are listed in
[Credential-bearing paths](#credential-bearing-paths). An unset variable or a
missing file is exit 2 before the first dial, every gap reported at once:

```text
{"event":"config.invalid","msg":"{{secret:ANTHROPIC_API_KEY}} is not set in the environment (referenced at config.intelligence.token)"}
```

`--validate-config` reads the environment it runs in, so run it with the
daemon's. `--prompt-missing` asks for each missing `{{secret:NAME}}` on the
terminal instead ([cli.md](cli.md)).

### Environment substitution

`${VAR}` and `${VAR:-default}` in any string value are replaced from the
process environment when the document loads. Braces are required, so a bare
`$VAR` stays as written, and `$$` is a literal `$`. An unset variable with no
default is exit 2. The instruction text is exempt: there `${name}` is an
instruction parameter. Use `${…}` for plain values such as hostnames and
`{{secret:…}}` for credentials.

### Configuration variables

`{{config.NAME}}` names an entry under [`vars`](#vars) and is substituted at
load, in the config and in every workflow definition.

## agent

The agent's identity, its standing instruction, and how its conversation
behaves.

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `agent.name` | string | `$AGENTD_POD_NAME`, else `$HOSTNAME`, else `agentd` | restart · doc | The instance identity. The durable store keys every record by it, so renaming the agent starts a different instance. |
| `agent.description` | string | a generic sentence | live · op | The public Agent Card's description, readable without signing in. |
| `agent.instruction` | string or object | unset | live · op | The standing instruction, given to every turn. In a job with no workflow and no prompt it is also the task: agentd generates the workflow `main` (`once → agent → finish`) to run it. A daemon runs nothing for it at startup. See [The instruction source](#the-instruction-source). |
| `agent.prompt` | string or object | unset | live · op | A task, delivered as a message into the root conversation at every start: a job's whole work, or what a daemon does as soon as it starts. Takes the `text`, `file`, `dir`, `url` and `oci` sources of the instruction, classified the same way; no `mcp`. A prompt also suppresses the generated `main` workflow a bare `--instruction` job gets. |
| `agent.workspace` | path | unset | live · op | The workspace the agent works in: its [AGENTS.md](coding-agent.md#the-repositorys-agentsmd) files — the root's, then one in each directory down to agentd's working directory when that is inside it, the nearest last — reach every turn's system prompt as text, below the instruction (the template's `agents_md`). UTF-8, at most 32768 bytes together, a symlink followed only to a file inside the workspace; a file that breaks one refuses the load, naming it. Read again by a reload, and on a change with `lifecycle.watch_config`. |
| `agent.preflight` | `never \| auto \| always` | `auto` | live · doc | Triage of an inbound message before its turn, on `intelligence.roles.preflight`. `intelligence.routes` needs `auto` or `always`. |
| `agent.wake_on` | list of `a2a_message \| human_reply \| subagent_result \| workflow_finished \| workflow_failed \| instruction_updated \| budget_resumed` | `[a2a_message, human_reply, workflow_failed]` | live · doc | The events that wake the root conversation or leave it a note. `subagent_result` notes each child's result. |
| `agent.on_workflow_finished` | `ignore \| note \| think` | `ignore` | live · doc | What a finished run leaves in the root transcript, for the runs `wake_on` names: nothing, a note, or a note that starts a turn. Withheld when the run may carry outside input and the root holds `sensitive` and `egress`. |
| `agent.tools.internal`, `agent.tools.mcp`, `agent.tools.code` | `all \| none` or a list of names | `all` | live · op | Which internal, MCP and code-registered tools the root conversation is granted. |
| `agent.max_parallel_turns` | int | `4` | live · doc | Conversation turns that may run at once. |
| `agent.conversation_budget` | budget | unset | live · doc | A token budget per conversation, in the [budget shape](#budgets). |
| `agent.approval` | `ask \| auto \| accept` | `ask` | live · doc | How a gate is answered: by a person, by a judge on `intelligence.roles.approve`, or by taking the ask's recommendation (`accept` falls back to `auto` when the ask carries none). The operator can change it at runtime with `admin.set`. |
| `agent.ask_human_fallback` | `fail \| wait \| auto` | `fail` | live · doc | What an `ask_human` does when it does not open a gate: fail at once, park until its timeout, or let the judge answer. `auto` also answers an unaddressed gate that times out. No judge answers an ask whose run, subagent or peer conversation carries outside input; the ask fails (`human.judge.withheld`). `pause` and `idle` are accepted for `wait`, `finish` and `stop` for `fail`. |
| `agent.ask_human_unowned` | `gate \| fallback` | `fallback` | live · doc | What an ask nobody owns does (one raised by a schedule, webhook, stream or subagent): open a gate task on the A2A listener (`gate`, needs `a2a.listen`), or apply `ask_human_fallback`. |
| `agent.gate_channels` | map: workflow → step path → channel | `{}` | live · op | The channel a configured workflow's gate is announced on, where no instruction document declared one. |
| `agent.document_capabilities` | list of `material \| knowledge \| interface \| identity \| compute \| infra \| compose` | `[]` | restart · op | The instruction-document families the instruction may use (the trust ladder). Empty grants the default rung only. A served document opens a webhook route only under `interface`. See [Directive reference](directives-reference.md). |

### The instruction source

`agent.instruction` has a short form, a value agentd classifies, and a long
form that names the source.

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `agent.instruction.text` | string | — | live · op | The instruction itself, never read as a path or URI. |
| `agent.instruction.file` | path | — | live · op | A local file, read at load. Watched when `lifecycle.watch_config` is on. |
| `agent.instruction.dir` | path, or `{path, glob, order}` | — | live · op | A folder of documents combined into one instruction. |
| `agent.instruction.dir.path` | path | — | live · op | The folder. |
| `agent.instruction.dir.glob` | string | `*.md,*.markdown,*.txt,*.instruction` | live · op | Comma-separated globs relative to `path`; `**` recurses. |
| `agent.instruction.dir.order` | `name \| date` | `name` | live · op | Path order, or modification time oldest first. |
| `agent.instruction.url` | `https://` URL | — | live · op | A document fetched at load. |
| `agent.instruction.oci` | reference, or `{ref, cosign_key}` | — | live · op | An OCI artifact (`ghcr.io/acme/agent:v3`; `@sha256:…` pins it). `cosign_key` is a PEM public key file (P-256 or Ed25519) the artifact's cosign signature must verify against. Needs `oci`. |
| `agent.instruction.mcp` | URI, or `{server, resource, events}` | — | live · op | A resource a configured MCP server serves, read and subscribed. `server` picks the server to ask; `events` names MCP Events types whose occurrences re-read the instruction (needs `mcp-events`). |
| `agent.instruction.refresh` | `auto \| off \|` duration | `auto` | live · op | How often to re-read the source. |
| `agent.instruction.unavailable` | `auto \| keep \| freeze \| drain \| exit` | `auto` | live · op | What to do when the source stops answering after startup. |
| `agent.instruction.unenforceable` | `warn \| refuse \| ignore` | `warn` | live · op | What a `trust` pin that names no key file means at load (its author keys are `instruction://` URIs, or none). `refuse` is exit 2. A key file that cannot be read is always exit 2. |
| `agent.instruction.decrypt.keys` | list of paths | `[]` | live · op | Recipient key files for an encrypted envelope (age v1 or JWE): an `AGE-SECRET-KEY-1…` identity, 64 hex characters, or base64. Needs `decrypt`. |
| `agent.instruction.decrypt.passphrase` | secret | — | live · op | The passphrase for an age scrypt envelope. |
| `agent.instruction.trust[]` | list of pins | `[]` | **restart** · op | Who may sign the document. Needs `sign`. |
| `agent.instruction.trust[].uri` | string | — | restart · op | The document the pin applies to (`instruction://…`). |
| `agent.instruction.trust[].publisher` | string | — | restart · op | The publisher the author signature must claim. With one pinned, an unsigned document is refused. |
| `agent.instruction.trust[].author_keys`, `.delivery_keys` | list | `[]` | restart · op | Ed25519 key files (raw, hex or base64url), or `instruction://…keys.json` JWKS URIs. |
| `agent.instruction.trust[].reader` | string | — | restart · op | This consumer's reader id; delivery verification runs only when set. |
| `agent.instruction.trust[].max_capabilities` | list of families | — | restart · op | The ceiling on the families this source may attest. |
| `agent.instruction.trust[].freshness` | duration | — | restart · op | The revocation deadline: how long a signed document may go unconfirmed. When `refresh` is slower, the tighter one wins. |

Naming two sources is exit 2, and so is a long form that names none. To run
with no instruction, omit `agent.instruction`. `glob` and `order` belong inside
`dir`; written beside it they are an unknown field.

The short form is classified like this:

| The value | Read as |
|---|---|
| contains a newline | text |
| `file://…` | a file |
| `oci://…` | an OCI artifact |
| any other `scheme://…`, `https://` included | a resource a connected MCP server serves (`mcp`) |
| no whitespace and a document extension (`.md`, `.markdown`, `.txt`, `.instruction`) | a file |
| no whitespace, path-shaped (`/`, `./`, `../`, `~/`), ending in `/` or naming a folder that exists | a folder |
| no whitespace and path-shaped otherwise | a file |
| anything else | text |

> **Warning.** `agent.instruction: https://docs.example/agent.md` names an MCP
> resource, not a web page. To fetch a document over HTTPS, write
> `agent.instruction: {url: https://docs.example/agent.md}` or pass
> `--instruction.url`.

A named file or folder that does not exist is exit 2. A `file`, `dir`, `url`
or `oci` source that cannot be read at load is exit 2; an `mcp` source that no
connected server serves at startup is exit 6. The two spellings compose:
`--instruction ./agent.md --instruction.refresh 30s` keeps both.

What `refresh: auto` does for each source:

| Source | `auto` |
|---|---|
| a file | an inotify watch when `lifecycle.watch_config` is on, otherwise no re-read |
| a folder | watches the folder and its documents when `lifecycle.watch_config` is on |
| `url` | no poll; fetched again on each reload |
| `oci` pinned by `@sha256:` | never re-read |
| `oci` by tag | polls every 5 minutes |
| `mcp` | the server's `resources/updated` notification and any `events` it names, with a 5-minute poll as fallback |
| text | nothing to re-read |

A re-read updates the text the model reads. Machinery the document declares
(`:::!workflow`, `:::!mcp`, …) folds into configuration at load, so it changes
on a reload or a restart, not on a poll.

What `unavailable` does after a successful start:

| Value | Behaviour |
|---|---|
| `keep` | carry on with the last good instruction |
| `freeze` | serve live work, refuse new work |
| `drain` | finish live work, then exit (0 for a daemon) |
| `exit` | stop at once, non-zero |
| `auto` | `freeze` when the source is trust-pinned (a `publisher` under `trust`), otherwise `keep` |

A re-read whose signature fails to verify counts as the source not
answering. Each outcome logs one `instruction.unavailable` line. The how-to is
[Write and ship instruction documents](instruction-documents.md).

## intelligence

The model connections, the named models, the job each model does, and the
token budget. The concepts are in
[Models, providers and roles](intelligence.md); the task is
[Connect model providers](connect-models.md).

### Connections

The top-level `intelligence.*` keys describe the default connection. Each
`intelligence.providers.<name>` is a further connection with the same keys and
nothing more.

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `intelligence.endpoints` | list, or one comma-separated string | `[]` | live · op | Ordered endpoints for failover. Each is `https://host[:port][/path]`, a loopback `http://`, or `mock:` (the built-in mock model: `mock:final`, `mock:file:<playbook.json>`; needs `mock-models`, which release binaries carry). |
| `intelligence.dialect` | `openai \| anthropic \| bedrock \| decisions` | `openai` | live · doc | The wire format. `bedrock` pairs with `auth.kind: aws`; `decisions` is for a provider only. |
| `intelligence.token` | secret | — | live · op | The bearer for every endpoint of the connection. |
| `intelligence.token_file` | path | — | live · op | A file holding the bearer, read at every dial. `token` wins when both are set. |
| `intelligence.headers` | map | `{}` | live · op | Extra request headers. A credential-shaped header needs a reference. |
| `intelligence.auth` | object | — | live · op | A credential provider; see [Credentials](#credentials). Refused at load when the connection cannot send it as a bearer: a `static` credential under a `header` other than `Authorization` (put that in `intelligence.headers`), or a SPIFFE `x509` SVID. Needs the `oauth` build feature, which release binaries carry. |
| `intelligence.timeout` | duration | `120s` | live · doc | The ceiling on one call. |
| `intelligence.structured_output` | `auto \| prompt` | `auto` | live · doc | How a call whose answer has a JSON Schema is asked: natively where the dialect and schema allow, or in the prompt alone for a server that refuses a response format. The answer is validated either way. `prompt` is refused on a `decisions` provider. |
| `intelligence.providers.<name>` | object | — | live · op | A further connection: `endpoints`, `dialect`, `token`, `token_file`, `headers`, `auth`, `timeout`, `structured_output`. Egress-checked like `intelligence.endpoints`. |

Where a request goes:

| Endpoint written | Posted to |
|---|---|
| host only, `dialect: openai` | `<host>/v1/chat/completions` |
| host only, `dialect: anthropic` | `<host>/v1/messages` |
| `dialect: bedrock` | the Converse path for the model |
| host only, `dialect: decisions` | `<host>/v1/decisions` |
| any URL with a path, `bedrock` aside | that path, exactly as written |

So `https://api.openai.com/v1/chat/completions` and
`https://api.anthropic.com` are the forms to write; `https://api.openai.com/v1`
posts to `/v1` and fails.

> **Note.** A connection with real endpoints and no model named for it is
> refused at load. `mock:` endpoints need no model.

### Models and roles

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `intelligence.model` | string | unset | live · doc | The turn model: a declared model name, or a literal wire model on the top-level connection. The same setting as `roles.turn`; setting both is exit 2. |
| `intelligence.swap_policy` | `finish-on-old \| restart-turn` | `finish-on-old` | live · doc | When a reload moves the turn model: the in-flight turn finishes on the old model, or re-runs on the new one. Running subagents apply it at their next turn boundary. |
| `intelligence.models.<name>.model` | string | required | live · doc | The wire model name sent to the provider. |
| `intelligence.models.<name>.kind` | `chat \| reasoning \| decision` | `chat` | live · doc | A chat model generates; a reasoning model thinks first, at an effort; a decision model answers typed questions natively and writes nothing. |
| `intelligence.models.<name>.provider` | string | the top-level connection | live · op | The connection: a name under `intelligence.providers`. A model a document declares may not name one. |
| `intelligence.models.<name>.window` | int | inferred from the model name, else 128000 | live · doc | The context window in tokens. |
| `intelligence.models.<name>.fallback` | string | — | live · doc | The model asked when this one is unavailable (every endpoint failed or broken open). A cycle is exit 2. |
| `intelligence.models.<name>.escalate` | string | — | live · doc | A decision model's: the model an unsure answer is asked again on, once. |
| `intelligence.models.<name>.reasoning.effort` | `low \| medium \| high` | `medium` | live · doc | A reasoning model's default effort; a node's or `think` call's `effort` overrides it. |
| `intelligence.models.<name>.reasoning.budget` | int | by effort | live · doc | Thinking tokens, for the dialects that take a budget. |
| `intelligence.roles.turn` | model, or `{model}` | `intelligence.model` | live · doc | The agent's turns; `agent`, `extract` and `summarize` nodes; subagents that name no model. |
| `intelligence.roles.think` | model, or `{model}` | `roles.turn` | live · doc | The `think` tool and `think` nodes. |
| `intelligence.roles.decide` | model, or `{model, confidence}` | `roles.turn` | live · doc | `decide`, `classify`, `route` and `judge` nodes, decision guards and the `decide` tool. Its `confidence` is the default threshold of every deciding node. |
| `intelligence.roles.preflight` | model, or `{model, confidence}` | `roles.turn` | live · doc | Triage of an inbound message. |
| `intelligence.roles.compaction` | model, or `{model}` | `roles.turn` | live · doc | Summarising the transcript when it outgrows the window. |
| `intelligence.roles.goal` | model, or `{model, confidence}` | `roles.turn` | live · doc | The goal check. |
| `intelligence.roles.approve` | model, or `{model, confidence}` | `roles.turn` | live · op | Asks answered on the operator's behalf (`approval: auto`). A decision model here must set `confidence`. |
| `intelligence.routes` | map: model → criterion | `{}` | live · op | The models a turn may run on instead of `roles.turn`, each with when to use it. Preflight picks one per message. Each is a declared `chat` or `reasoning` model; 32 at most; refused with `agent.preflight: never`. |

A model reference is a declared name, or, when no model of that name exists, a
literal wire model on the top-level connection. Every reference is checked at
load. `confidence` is refused on `turn`, `think` and `compaction`. Choosing
models per job is the guide
[Choose models for each job](models-and-roles-guide.md); the wire details are
[Intelligence wire reference](intelligence-reference.md).

### Budgets

`intelligence.budget` is the instance's token budget. The same shape is
`agent.conversation_budget` (per conversation),
`a2a.principals[].quotas.budget` (per principal),
`subagents.templates.<name>.budget` (an instance-tier child's own) and a
workflow's `limits.budget`. Where a budget is declared decides what it
bounds. All `intelligence.budget` paths are live · doc.

| Path | Type | Default | What it does |
|---|---|---|---|
| `intelligence.budget.windows[]` | list | `[]` | Fixed windows: `{per, tokens, requests, reset}`. |
| `intelligence.budget.windows[].per` | `second \| minute \| hour \| day \| week` | required | The window length. `day` and `week` are calendar windows. |
| `intelligence.budget.windows[].tokens` | int | — | Tokens allowed per window. |
| `intelligence.budget.windows[].requests` | int | — | Model calls allowed per window. |
| `intelligence.budget.windows[].reset` | string | `00:00Z` | When a calendar window resets (UTC). |
| `intelligence.budget.on_exhausted` | `wait \| slow \| degrade \| refuse \| fail` | `wait` | What an admission does when a window is spent. |
| `intelligence.budget.slow.factor` | number, 0–1 | `0.5` | For `slow`: pace admissions to this share of the window rate. |
| `intelligence.budget.degrade.model` | string | — | For `degrade`: the model a squeezed turn moves to. Required with `degrade`. |
| `intelligence.budget.lifetime_tokens` | int | `0` (unbounded) | A ceiling across every run of the process. |
| `intelligence.budget.lifetime_exhausted` | `drain \| refuse \| exit` | `drain` | What the process does once the ceiling is spent: finish live work and exit (0 for a daemon), stay up refusing new work, or exit 7 at once. |

The how-to, with the exit codes and gauges each choice produces, is
[Control spend with budgets](budgets.md).

### Credentials

The `auth` block is the same wherever it appears: `intelligence.auth`,
`intelligence.providers.<name>.auth`, `mcp.servers[].auth`,
`services.<name>.auth` and `a2a.peers[].auth`. One mechanism per endpoint,
named by `auth.kind`.

| Mechanism | Required fields | Optional fields |
|---|---|---|
| `static` | `token` (a bearer), or `header` + `value` | — |
| `oauth2` | `client_id`, or `security.client` and `issuer`; `token_url` or `issuer` — on an MCP server or an MCP catalog entry neither, the server's RFC 9728 metadata naming it; for `grant: device`, `device_authorization_url` or `issuer` (or neither, on an MCP server); for `grant: client_credentials` with a `client_id`, `client_secret` or `client_auth: {method: private_key_jwt}` | `grant` (`device` default, `authorization_code`, `client_credentials`), `scopes`, `audience`, `resource` (`auto`, `omit` or a URI; unset, `auto` on an MCP server or MCP catalog entry, `omit` elsewhere), `authorization_url`, `client_auth` (`method`: `private_key_jwt` default with `key`, `kid`, `assertion_aud`; `client_secret_basic`; `client_secret_post`; with no `client_id`, `assertion_aud` only) |
| `aws` | `region`, `service` (for example `bedrock`); for `source: sso`, `sso_start_url`, `account_id`, `role_name` | `source` (`env` default, `static`, `imds`, `irsa`, `sso`) |
| `spiffe` | for `svid: jwt` (the default), `jwt_svid_file`; for `svid: x509`, `svid_file` + `key_file` | `audience` |

`token`, `value`, `client_secret` and `client_auth.key` must be references in a file. Interactive
grants are completed with `agentd --login <target>`, where the target is
`intelligence`, `intelligence:<provider>`, `mcp:<name>` or `service:<name>`.
`kind: oauth2`, and any `auth` block on an MCP server, need the `oauth`
feature. An `oauth2` block with no `client_id` presents agentd's
[Client ID Metadata Document](authentication.md#client-id-metadata-document)
(`security.client`) where its issuer's metadata advertises one. The guide is
[Outbound credentials](authentication.md).

## mcp

The MCP servers the agent's task tools come from. All `mcp.*` paths are
live · doc, except where a row says op.

| Path | Type | Default | What it does |
|---|---|---|---|
| `mcp.default_timeout` | duration | `60s` | Per-call timeout for a server that sets none. |
| `mcp.servers[].name` | string | required | Unique; `code` is reserved for code-registered tools. |
| `mcp.servers[].endpoint` | URL | required unless `service` | `https://host[:port][/path]`, or a loopback `http://`. Reached over Streamable HTTP; agentd spawns no process. |
| `mcp.servers[].service` | string | — | A `services:` entry to inherit endpoint, credential and tags from. `endpoint`, `auth` and `headers` beside it are refused. |
| `mcp.servers[].headers` | map | `{}` | Extra headers. A credential-shaped one needs a reference. Headers the MCP client sets itself (`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, `Mcp-Param-*`, `Mcp-Session-Id`, `Last-Event-ID`) are refused. |
| `mcp.servers[].tags` | map: glob → list of `untrusted_input \| sensitive \| egress` | `{}` | Capability tags for the Rule of Two. The union applies to every tool of the server; an untagged server counts as `untrusted_input`. |
| `mcp.servers[].ns` | string | — | A namespace prefix: tools register as `<ns>.<tool>`. |
| `mcp.servers[].allow` | list of globs | all | Admit only the advertised tools that match. |
| `mcp.servers[].exclude` | list of globs | `[]` | Never admit these; beats `allow`. |
| `mcp.servers[].timeout` | duration | `mcp.default_timeout` | Per-call timeout for this server. |
| `mcp.servers[].tasks` | `auto \| off` | `auto` | Whether the server may answer a long tool call with an MCP task (SEP-2663). `auto` declares the extension on 2026-07-28 connections, on the calls that have a time limit (a turn's, a workflow step's, a store adapter's); `off` never does, so the server answers every call inline. See [Long-running tool calls](connect-mcp.md#long-running-tool-calls). |
| `mcp.servers[].auth` | object | — | A credential provider; see [Credentials](#credentials). With `grant: id_jag` — or `grant: token_exchange`, for a server behind the person's own IdP — the server is reached with an enrolled person's grant or a caller's own token, by the supervisor alone; its fields are under [Enterprise-managed authorization](authentication.md#enterprise-managed-authorization), restart · op, and a served document may set none of them. Needs `xaa`. |
| `mcp.servers[].oauth` | `{client_id, client_secret, token_url, scope}` | — | An OAuth client-credentials shortcut. Needs `oauth`. |
| `mcp.servers[].aauth` | bool | `false` | Sign requests with the agent's AAuth identity. Needs `aauth`. |
| `mcp.servers[].events[]` | list | `[]` | MCP Events bindings (op). |
| `mcp.servers[].event_grants[]` | list | `[]` | Event types runtime subscriptions may be made for (op). |

`--mcp name=<endpoint>` adds one server; a spec with no `=` is exit 2. A server
down at startup is dialed again in the background and joins when it answers.
The how-to is [Connect MCP servers](connect-mcp.md); protocol support is
[MCP support reference](mcp-reference.md).

### MCP Events

Bindings and grants need the `mcp-events` feature, which tracks the draft
extension SEP-3415; a config that declares one says so at load. A served
document or a subagent template that declares either is refused.

`events[]` subscribes one event type with fixed arguments:

| Field | Type | Default | What it does |
|---|---|---|---|
| `name` | string | required | The event type, as the server's `events/list` names it. |
| `arguments` | object | `{}` | Subscription arguments, checked against the type's input schema. |
| `delivery` | list of `poll \| webhook` | required | Acceptable modes in preference order; the first the server offers is used. |
| `into` | `{stream, subject}` | required | A stream declared under `streams:`, and the subject each occurrence lands under. |
| `replay` | duration, or `none` | `1h` | How far back to catch up after downtime. |
| `rate` | rate | — | An intake ceiling. Required on an untagged or `untrusted_input` server. |
| `ttl` | duration | `1h` | Webhook only: the lease agentd asks for, refreshed at 80% of what the server grants. |

`event_grants[]` takes `name`, `delivery`, `into`, `replay`, `rate` and `ttl`
as above, plus:

| Field | Type | Default | What it does |
|---|---|---|---|
| `fixed` | object | `{}` | Arguments no runtime subscription may set. |
| `max` | int, 1–64 | `8` | Live runtime subscriptions under this grant. |
| `lifetime` | duration, up to 30d | `1d` | How long a tool-made subscription lives. |
| `tools` | bool | `false` | Offer the `events.*` tools for this type. |

`webhook` delivery also needs `webhooks.listen`, an `https://` `webhooks.url`,
no `webhooks.tls.client_ca`, and a credential on the server. The how-to is
[React to changes and events](react-to-events.md).

```yaml
# fragment: under mcp.servers (with a stream named issues under streams:)
- name: tracker
  endpoint: https://mcp-tracker.internal/mcp
  tags: {"*": [untrusted_input]}
  events:
    - name: issue.opened
      arguments: {project: web}
      delivery: [poll]
      into: {stream: issues, subject: issue.opened}
      rate: "30/1m"
```

## services

The service catalog: the external services this deployment may use, named once.
All `services` paths are live · op.

| Path | Type | Default | What it does |
|---|---|---|---|
| `services.<name>.kind` | `mcp \| intelligence \| peer \| http \| company` | `mcp` | The surface the entry serves. Matching is by kind. `company` needs the `companies` feature; see [Companies](#companies). |
| `services.<name>.endpoint` | URL | required | The connection URL, and the match base (scheme, authority and path prefix on segment boundaries). |
| `services.<name>.auth`, `.headers`, `.timeout` | — | — | Connection settings consumers inherit. |
| `services.<name>.tags` | map | `{}` | Authoritative tags, unioned into every server whose endpoint matches, referencing or not. |
| `services.<name>.allow` | list of globs | all | The tool ceiling; a consumer may only narrow it. |
| `services.<name>.exclude` | list of globs | `[]` | Never admitted, for every consumer. |
| `services.<name>.rate` | rate | — | Pacing toward the service, shared by every consumer in each process. |
| `services.<name>.methods` | list | any | `kind: http` only: the methods `http` steps may use. |
| `services.<name>.breaker` | `{failures, cooldown}` | — | `kind: mcp` only: the default breaker for `mcp.tool` steps against it. |
| `services.<name>.hosts` | list of host globs | the endpoint's host, the document's `organization.domain`, the subdomains of both, the issuer's host | `kind: company` only: every URL the Company's documents name must be on one of these hosts (`example.com`, `*.example.com`), and its host a plain one — no userinfo, and no `\`, which a browser reads as `/`. |
| `services.<name>.allow_private` | bool | `false` | `kind: company` only: admit loopback, private and link-local addresses. |
| `services.<name>.interfaces` | list of `mcp \| agent \| openapi` | `[mcp]` | `kind: company` only: the Company's interfaces agentd uses — its MCP servers, its agent (conversations, and the stream `company.<name>`), its OpenAPI APIs. See [A Company's agent and its APIs](#a-companys-agent-and-its-apis). |
| `services.<name>.mcp_names` | map of an MCP API `url` to a server name | — | `kind: company` only: names the Company's MCP servers when it lists more than one. A company entry whose `interfaces` lack `mcp` refuses it, and `rate`, `allow` and `exclude` with it: they shape only the Company's MCP servers. |
| `services.<name>.privacy` | `strict \| standard` | `strict` | `kind: company` only: under `strict` a Company sees none of agentd's correlators. Its calls carry their idempotency key keyed under the (User, Company) pair's secret and a trace of their own per unit of work and Company — no other `agent/*` `_meta` key, no `tracestate` — and the handshake and tool list carry no trace context and no `clientInfo.title`. `standard` sends agentd's usual correlators, the instance label and the process trace included, and is logged at boot (`company.privacy.standard`). |
| `services.<name>.sign_in` | list of `device \| direct` | `[]` | `kind: company` only: the sign-in types agentd may start with the User, the first the Company offers taken. `direct` needs `security.companies.client.serve`. `mediated` is refused: the User's password would pass through a gate, whose answer agentd keeps in the task's history and hands to the model. Needs `security.companies.seal_key` and `scopes`. |
| `services.<name>.scopes` | list of scopes | `[]` | `kind: company` only: the most account scopes agentd asks the User for there. A sign-in asks for what is needed within these and what its type grants; a held Account Token is used for no more, and one that grants none of these — or any, once the entry lists no `sign_in` — is not used: requests there are signed out, and the token is kept until the User signs out. |
| `services.<name>.api_allow` | list of `METHOD /path-template` | none | `kind: company` only: the OpenAPI operations agentd may call there, each as the description's `paths` names it (`GET /orders/{order_id}`; a parameter matches another of any name). An operation outside it, or outside the description, is refused unsent. Each one the description declares is also a tool of its own, named from its entry ([operation tools](#a-companys-operation-tools)). An operation listed twice, or two entries that make one tool name, are refused. Needs `openapi` in `interfaces`. |
| `services.<name>.api_names` | map of an OpenAPI API `url` to a server name | — | `kind: company` only: names the server a Company's operation tools are named under, by the URL its OpenAPI API is listed at. Without it the server is named after the entry, or `<entry>_api` when the entry's own MCP server has that name, which needs the Company to list one OpenAPI API. Needs `openapi` in `interfaces` and an operation in `api_allow`. |
| `services.<name>.context` | `{locale, time_zone, share_availability}` | inherits | `kind: company` only: what this Company's conversations share of the User's situation, each field over `security.companies.context`. Needs `agent` in `interfaces`. |

With `security.egress: closed`, every MCP dial, intelligence endpoint
(top-level and providers; `mock:` aside), A2A peer, `http` step, HTTP store,
workflow URL and A2A push target must match an entry of its kind.
`observability.otel.endpoint` is the one destination it does not cover.
`--validate-config` prints each consumer's effective endpoint, tool lists and
tags.

```yaml
services:
  billing:
    endpoint: https://billing.internal/mcp
    auth: {kind: static, token: "{{secret:BILLING_MCP_TOKEN}}"}
    tags: {"*": [sensitive]}
    allow: [charge_lookup, "invoice_*"]
    rate: "60/1m"
mcp:
  servers:
    - {name: money, service: billing, allow: [charge_lookup]}
security:
  egress: closed
```

Under `closed`, an intelligence endpoint needs a `kind: intelligence` entry of
its own. The hardening guide is [Harden a deployment](harden.md).

### Companies

A `kind: company` entry declares a Company agentd acts at for its one User,
under the Personal Agent Protocol draft agentd is pinned to
([RFC 0047](../rfcs/0047-acting-at-companies-for-a-person.md); the
`companies` feature, which no release binary carries). `endpoint` is
`https://<host>`, the domain whose `/.well-known/poppy.json` is read; a path is
refused. The supervisor discovers and verifies the Company after load (never
under `--validate-config`, which dials nothing), holds a signed-out Session
there, and connects to each MCP API the Company lists as an MCP server of its
own: named after the entry when there is one, by `mcp_names` when there are
several, with every tool registered as `<server>.<tool>`. A Company that does
not answer, or does not verify, is tried again with the backoff of a server
down at boot. The tools are the supervisor's alone: no turn worker or
subagent is handed a Company server to dial, and every call to one comes back
to the supervisor. They are offered only in work the configured User owns,
and in work nobody owns when the User is `operator` or an operator principal.
A workflow calls them with `tool` steps (`shop.search`), under the same rule;
an `mcp.tool` or `mcp.resource` step, a resource wait or a `subscribe` start
naming a Company's server is refused, and neither an instruction, a workflow
definition nor the store is read from one.

A Company's answer is its content: the conversation, run or subagent it
reaches carries the Company's origin from then on, and a call to another
Company from it waits for the configured User's answer
([Company gates](human-in-the-loop.md#company-gates)). A call that fails after
it was sent — no answer in the entry's `timeout`, a `5xx`, a result agentd
could not read — answers `outcome unknown` and is not sent again, by a step's
`retry:` or by a restart that finds the step still running. A `429` or `503`
with `Retry-After` holds every request to the Company until the wait is over.
`company.status` (a tool, and an operator op) says where each Company stands;
the operator op `company.user_id.reset` makes the User new at one, revoking
every Account Token held there first.

The User signs in at a Company themselves, at a gate agentd opens for them
([Company gates](human-in-the-loop.md#company-gates)), when an entry lists
`sign_in` and `scopes`: the model asks with the `company.sign_in` tool, a
workflow with a `tool` step, the operator with `agentd --login company:<name>`
([CLI](cli.md)). Device Sign-In shows the User a link and a code to check on the
Company's page; Direct Sign-In a link to it, which returns to the callback the
A2A listener serves. Neither ever opens a browser. The Account Token the sign-in
gives agentd is kept only sealed with `security.companies.seal_key`, read by the
supervisor alone for each request that uses it, and written back before use
when the Company rotates it. Its account's Session is signed in with it —
renewed, or started again when the Company ends one — until the Company
answers `invalid_grant` for it: a disconnect, a revocation, an expiry. Then it
is forgotten and work goes on signed out. `company.sign_out` and
`agentd --logout company:<name>` revoke and forget it. The User may hold up
to eight accounts at one Company, each named by a label a sign-in gives it
(`account`; `agentd --login company:<name>/<label>`) and each with an Account
Token and a Session of its own; the label never reaches the Company
([Several accounts](act-at-companies.md#several-accounts)).

#### A Company's agent and its APIs

An entry whose `interfaces` list `agent` talks with the Company's agent, in the
one conversation protocol the draft defines, `poppy`; one that lists
`openapi` calls the operations of the OpenAPI APIs the Company lists. Both are
reached through `company.*` tools — internal tools the supervisor serves, as
it serves a Company's MCP tools, under the same rules: the User's work only,
`limits.in_flight`, the Company's `Retry-After`, the cross-Company gate, and
the Company's origin on whatever reads the answer. Every request carries the
User's DPoP Session Token and a proof of its own, and no correlator of
agentd's. A request made for a unit of work names a trace in `traceparent`:
under `privacy: strict` the unit's own trace at this Company — the same for
each of the unit's requests there, linked to the unit's trace only in
agentd's telemetry — and under `standard` the unit's own. One that belongs to
no unit of work — a reader's — names the process trace under `standard`, and
none under `strict`. Such a Company is verified at boot too, and opens its
Session at the first call.

- **`company.converse {company, conversation?, account?, text?, data?, wait?}`** sends a
  message: with no `conversation` it starts one and answers its id. The
  message's id is `msg_` and 128 random bits, kept for the call before it is
  sent, so every send of the call — the transport's retry, and a workflow
  step's `retry:` or its replay after a restart, the step's own key holding
  across its attempts — carries the same id, and the Company answers it as the
  message it has (other content under it is `message_id_conflict`). A model's
  call is its turn's: a step's next attempt is a new turn, whose calls are
  new. Its `sender` is always `agent`, and its `context` holds only what
  `security.companies.context` (or the entry's own `context`) shares. With
  `wait`, at most 30 s, the reply is read too.
- **`company.read {company, conversation, account?, wait?}`** answers the events since
  the model's last read, waiting up to `wait` (default 30) while there are
  none; **`company.handoff`** asks for a person at the Company;
  **`company.close`** closes the conversation. A read asked to `wait` is
  waited for that long and 15 s more, whatever the entry's `timeout`.
- **The reader.** Each open conversation — at most `limits.conversations` in
  the instance, a new one refused until one closes — has a reader on a thread
  of its own: it asks for a stream (`text/event-stream`, a long poll from a
  Company that does not stream), opens it again with `Last-Event-ID` when it
  ends, and appends the conversation's `message`, `state`, `authorization`,
  `user_requested`, `direct_opened` and `direct_closed` events to the stream
  `company.<name>`, carrying the
  Company's origin, so a workflow can start from them (`stream`) or wait for
  one (`wait {on: event}`). It saves its cursor once they are appended, and a
  restart reads on from it. It stops when the conversation closes and when
  the instance drains. Each read skips event types agentd does not recognise
  and events it handled before, never hands on a `text-delta` piece, and on
  `cursor_expired` reads again from the start, telling the model that part of
  the conversation is missing. agentd declares `company.<name>` itself; a
  `streams:` entry of that name sets its retention.
- **What the supervisor does with an event**, once: an `authorization` event
  is answered with the User's Account Token when one is held whose scopes
  cover it — the Session is signed in, and the conversation goes on with the
  new token — and otherwise reaches the model as a note that `company.sign_in`
  asks the User; the Company's `authorization` event never opens a gate
  itself. A `user_requested` event asks the User whether to talk with the
  Company directly ([Direct Conversations](human-in-the-loop.md#direct-conversations)),
  when they can be asked — the A2A listener serves and
  `agent.ask_human_unowned` is `gate` — and is otherwise answered in the
  conversation: the User is not available for a direct conversation. An
  operation a Company message carries is kept as one the conversation
  proposed ([A Company's operations](#a-companys-operations)).
- **`company.api {company, api?, method, path, query?, json?, expect?, account?}`**
  calls one operation of an OpenAPI 3.0 or 3.1 description (JSON, at most
  1 MiB; an operation's `$ref`s are followed when it is called or described,
  within the description, at most 16 deep and to at most 20 000 values).
  Unless `method` and `path` name an operation of the description that
  `api_allow` lists, and the path parameters, query and body hold to its
  schemas, nothing is sent. Each path segment is read percent-decoded, and one
  that is `.` or `..`, or holds `/` or `\` once decoded, is refused: a server
  may resolve it to another operation. What is sent is the operation's
  template, each parameter percent-encoded into it. A write
  carries an `Idempotency-Key`, a UUID kept for the call, and is sent again
  after a lost answer only when its operation declares that header; any other
  write that may have run is `outcome unknown`. It answers `{status, json}`;
  a status outside `expect` (default 2xx) is an error. A Company's
  `WWW-Authenticate` errors are handled as for its MCP servers.
  **`company.api.describe {company, api?, operation?}`** lists the operations,
  or gives one's parameters and schemas; each operation `api_allow` lists
  names its operation tool.
- **The `http` step** with `auth: {kind: company, company: <name>}` is a
  `company.api` call made by the workflow: its `url` is the operation's (or
  its path under the API's server URL), with the step's `method`, `query`,
  `json` and `expect`, and `auth.account` the label of the User's account it
  acts with, when they hold several there. It takes no `headers`, `body`,
  `sign`, `allow_private` or `idempotency`: the credential, the body, the
  hosts and the key are agentd's.
- **The User's accounts.** Each of these tools, the operation tools and
  `company.operation.*` take an optional `account`, the label of the User's
  account to act with ([Several accounts](act-at-companies.md#several-accounts));
  a call naming none while several are held is refused unsent, and a
  conversation or an operation goes on in the account it belongs to.

```yaml
# fragment: a Company's agent and one of its APIs
security:
  companies:
    context: {locale: en-US, time_zone: America/Los_Angeles}
    limits: {conversations: 8}
services:
  shop:
    kind: company
    endpoint: https://example.com
    interfaces: [agent, openapi]
    api_allow: ["GET /orders/{order_id}", "POST /orders/{order_id}/exchanges"]
workflows:
  - name: exchange
    steps:
      s: {kind: once}
      ask: {kind: tool, name: company.converse, depends_on: [s],
            args: {company: shop, text: "The jacket from order 1042 is too thin. Can it be exchanged?", wait: 20}}
      order: {kind: http, depends_on: [s], url: "https://api.example.com/orders/1042",
              auth: {kind: company, company: shop}}
      f: {kind: finish, depends_on: [ask, order], status: completed}
```

#### A Company's operation tools

Each operation `api_allow` lists that the Company's description declares is
a tool of its own (RFC 0047 §9, P5a), offered once the description is read —
at boot, and for a Company down at boot once it comes up — as a Company's MCP
tools are offered once its server joins.

- **Its name** is the operator's: its `api_allow` entry in lower case, each
  run of characters other than `a-z` and `0-9` one `_`, none at either end, at
  most 64 characters — `GET /orders/{order_id}` is `get_orders_order_id` —
  under the server named after the entry (`<entry>_api` when the entry's MCP
  server has its name), or the one `api_names` gives the API: `shop.get_orders_order_id`. The `operationId`, summary and
  description go into the tool's description, never its name. So the names
  are known at load, and load refuses a tag key, a `security.policies` rule's
  `match.tool` or a workflow's `tool` step that names an operation tool the
  entry does not make, two `api_allow` entries that make one name, and a
  server name another server has.
- **Its input schema** is the operation's: its path and query parameters as
  properties, each with its schema and required as declared — nested under
  `path` and `query` when one's name is another's, `json`, `expect` or `account` — `json`
  for a JSON body, required as declared, `expect`, and `account`, the User's
  account it acts with; nothing else is taken.
  Header and cookie parameters are not taken, and an operation that requires
  one other than `Idempotency-Key` has no tool. Neither has one whose schemas
  do not resolve within the bounds above, or whose input schema is over
  16 KiB serialized.
- **Its description** starts with the note a Company's MCP tools carry — whose
  words follow — then the operation as `api_allow` lists it, then the
  Company's `operationId`, summary and description, each quoted, its control
  characters removed and at most 2000 characters.
- **A call** is the `company.api` call of the operation: its path parameters
  filled into the operation's template, its query and `json` as given, and
  everything after that `company.api`'s — the checks, the DPoP-signed
  request, the `Idempotency-Key`, `outcome unknown`, `Retry-After`, the
  privacy profile, the cross-Company gate, an operation the Company proposes.
  A path the description matches to another operation — a path parameter's
  value that is a fixed segment of that one's path — is refused unsent.
- **What holds `company.api` holds it.** A `security.policies` rule judges
  its call as its own and as `company.api`'s, under that name with
  `company.api`'s arguments (`{company, api, method, path, query, json,
  expect, account}`) as the call fills them and the tool's own tags; the first rule
  that matches either decides. `tools.disabled` naming `company.api`
  disables every operation tool, `tools.narrow` on `company.api` adds its
  tags and note to each, and `agent.tools` offers one to the conversation
  only where `agent.tools.internal` offers `company.api` and
  `agent.tools.mcp` offers the tool.
- **Its tags**, when a key of the entry's `tags` other than `*` names it (its
  name, or a glob of it), are those of each key that matches its name, `*`
  included; otherwise every tag the entry lists — with no `tags`,
  `untrusted_input` and `egress` — as `company.api` carries them. The Rule
  of Two at load counts all of the entry's tags, as for its servers.
- A fetch of the description that finds it changed lists the tools again:
  those that changed replace the old ones in the registry
  (`company.api.changed`), and a turn planned after sees them. An allowed
  operation with no tool is logged once (`company.api.not_offered`), and why
  none can be made at all (`company.api.unavailable`).

They reach turn workers and subagents as a Company's MCP tools do: routed to
the supervisor, a subagent granted the server handed their definitions.

```yaml
# fragment: a Company's MCP server, and the tools of its API's operations
services:
  shop:
    kind: company
    endpoint: https://example.com
    interfaces: [mcp, openapi]
    api_allow: ["GET /orders/{order_id}", "POST /orders/{order_id}/exchanges"]
    api_names: {"https://api.example.com/openapi.json": orders}
    tags: {"*": [untrusted_input, egress]}
workflows:
  - name: order
    steps:
      s: {kind: once}
      read: {kind: tool, name: orders.get_orders_order_id, depends_on: [s],
             args: {order_id: "1042"}}
      f: {kind: finish, depends_on: [read], status: completed}
```

#### A Company's operations

With `security.companies.client.extensions: [operations]` the client metadata
document lists the draft's Operations extension, version 1, and a Company may
propose an action — an exchange, a booking change, a cancellation — as an
operation instead of performing it: in an MCP tool's `structuredContent`, an
OpenAPI `202 {operation}` or a conversation message. agentd keeps each one it
sees in the User's ledger at the Company, with how it came and the digest of
the request that proposed it, and does nothing with it until it is confirmed.
Every company entry has the stream `company.<name>` then, where an
operation's final state is appended.

- **`company.operation.get {company, operation_id, account?}`** reads it at the
  Company's operations endpoint; **`company.operation.cancel`** cancels it.
- **`company.operation.confirm {company, operation_id, account?}`** confirms it, and
  takes nothing else: the model passes no revision and no approval. agentd
  reads the operation again and confirms its current revision only with the
  User's `approve` at their gate ([Operation approvals](human-in-the-loop.md#operation-approvals))
  or under a standing permission that covers it. The call waits while the
  User decides. `terms_changed` asks again; `user_approval_required` asks the
  User, never a permission; `in_progress` is read again after the Company's
  `Retry-After`, across a restart too, and its final state appended to the
  stream. A confirm that got no answer, or whose answer agentd could not
  keep, is `outcome unknown`, and the operation is read until it is final; a
  later confirm sends the one the User approved. A confirm whose run or task
  is cancelled on its way is not sent, or — once the Company took it —
  cancelled there.
- **Nothing proposes the same action twice while it is open.** A request whose
  digest — the tool and its arguments, or the method, path, query and body —
  proposed an operation that is not final is refused unsent, and an operation
  with the same terms in the same Session as an older one that is open is not
  confirmed. The same action asked for with other arguments and other terms
  is not recognised: the draft gives an operation no kind of action.

`security.companies.standing` lists the operator's standing permissions: an
operation one of them covers is confirmed with `approved_by:
standing_permission`, and the User is not asked. Each covers one Company, one
channel — `via: {tool: <glob>}`, a glob of a Company tool's registered name,
or `via: {api: "METHOD /path-template"}`, an operation `api_allow` lists —
and the term keys in `terms`. It applies only when the Company does not
require the User's own approval, every channel the operation came through is
`via` (one proposed in a conversation never matches), the revision has terms
and every top-level key in them is listed, `when` — CEL over `terms` and
`summary`, so it needs the `cel` feature — is true with every term it reads
present, it has not passed `expires`, and a use is free. This is asked
again at each confirm, a retry of one too: an earlier confirm by a
permission is never reused. A limited use is reserved with the confirm's
lock, so two calls racing for the last use confirm once; it is spent once
the Company records a confirmation by the permission, and given back when
the Company refuses the confirm, answers it without taking it, or the
operation ends with none — a cancel after a confirm it took gives nothing
back. A permission covers the User's accounts its `accounts` lists, by the
labels sign-ins gave them, and without the list the account labelled
`default` alone, and the signed-out Session: a second account is never
covered unlisted. A sign-in that may change the account under a label —
one with `new_account`, or one under a label whose account was signed out
of or otherwise given up, which goes in a new Session — suspends the
permissions covering that label until a restart. They are restart-only and
operator-only, like every `security.companies` path.

```yaml
# fragment: operations, and the exchanges agentd may confirm on its own
security:
  companies:
    client:
      id: https://agent.example/companies/client.json
      name: Dana's agent
      extensions: [operations]
    standing:
      - company: shop
        via: {tool: "shop.exchange_*"}
        terms: [order_id, item_id, replacement_sku, price_difference]
        when: "terms.price_difference.currency == 'USD' && double(terms.price_difference.amount) <= 20.0"
        expires: 2026-12-31T00:00:00Z
        uses: 3
services:
  shop:
    kind: company
    endpoint: https://example.com
```

A company entry refuses `auth`, `headers`, `methods` and `breaker`: the
protocol defines how agentd authenticates there. Its `tags` default to
`{"*": [untrusted_input, egress]}`. Adding, removing or changing a company
entry is a restart (`config.reload.restart_required`). It needs
`security.companies.client`, and a store that keeps the User ID: `store.kind`
`none` and `memory` are refused, and a remote store (`mcp`, `http`) needs
`security.companies.seal_key`. Once the store's User IDs are sealed, every
Company is unavailable without the key they were sealed with — a removed key
as much as a wrong one (`company.seal_key.mismatch`) — until the key is
restored, or `company.user_id.reset` is run for that Company: it starts over
there as a new User, the others staying unavailable until each is reset, and
once every Company has, the store takes the key.

```yaml
# fragment: a Company, and who agentd is to it
security:
  companies:
    client:
      id: https://agent.example/companies/client.json
      name: Dana's agent
services:
  shop:
    kind: company
    endpoint: https://example.com
    hosts: ["example.com", "*.example.com"]
    allow: ["search*", "order_*"]
```

```yaml
# fragment: the User may sign in at the Company with a link and a code
security:
  companies:
    seal_key: "{{secret-file:/run/secrets/agentd-company-seal}}"
services:
  shop:
    kind: company
    endpoint: https://example.com
    sign_in: [device]
    scopes: ["poppy:read", "poppy:write"]
```

## tools

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `tools.disabled` | list of tool names | `[]` | live · doc | Tools never registered. `company.api` here disables a Company's operation tools too. |
| `tools.narrow.<tool>.tags` | list of tags | — | live · doc | Trifecta tags to add to a tool; never removes one. Narrowing `company.api` narrows a Company's operation tools too. |
| `tools.narrow.<tool>.describe` | string | — | live · doc | A note appended beneath the tool's description. |
| `tools.overrides.<tool>.server` | string | — | live · op | Retarget a tool at a declared MCP server. |
| `tools.overrides.<tool>.tool` | string | — | live · op | The server's tool name. |
| `tools.overrides.<tool>.args` | string | — | live · op | A JSON template or `CEL: …` producing the arguments from `args` and `ctx`. |
| `tools.overrides.<tool>.result` | string | — | live · op | A JSON pointer, template or `CEL: …` mapping the result. |

## context

How the system prompt is built and when the transcript is compacted. All paths
are live · doc.

| Path | Type | Default | What it does |
|---|---|---|---|
| `context.template` | string | the built-in template | The system-prompt template. `agentd --context-template` prints the built-in one. |
| `context.templates.<name>` | string | — | Named alternates a node selects with `context: {template: <name>}`. |
| `context.compact_at` | number | `0.7` | Compact when the transcript reaches this share of the model window. |
| `context.keep_last` | int | `12` | Messages kept verbatim through a compaction. |
| `context.model_window` | int | the turn model's window | Overrides the window compaction measures against. |
| `context.plan.max_items` | int | `32` | Items a `plan.create` may hold. |
| `context.summarize.prompt` | string | built-in guidance | The summarizer's guidance. The summary's JSON shape is fixed; the model is `intelligence.roles.compaction`. |

The template language:

| Form | Meaning |
|---|---|
| `{{ expr }}` | interpolate |
| `{{#if expr}} … {{else}} … {{/if}}` | conditional; an empty value is false |
| `{{#each expr}} … {{/each}}` | loop; `this` is the element, `@index` its position |
| `{{! comment }}` | comment |

An expression is a path first and CEL second. Bare paths (`{{instance}}`,
`{{#each services}}`) work in any build; anything more needs the `cel`
feature, with two helpers CEL lacks: `take(list, n)` and `join(list, sep)`.
The data names are `instance`, `instruction`, `agents_md`, `workflows`,
`services`, `streams`, `templates`, `skills`, `peers`, `signals`, `memory`
and `tools.internal`. `agents_md` is the list of `agent.workspace`'s
AGENTS.md files, root first, each `{path, text}`.

```yaml
# fragment: under context:
template: |
  You are {{instance}}. {{#if egress_closed}}Egress is closed.{{/if}}
  ## Instruction
  {{instruction}}
  {{#if services}}
  ## Services
  {{#each take(services, 16)}}- {{this.name}}{{#if this.tags}} [{{join(this.tags, ", ")}}]{{/if}}
  {{/each}}{{/if}}
templates:
  minimal: "You are {{instance}}: {{instruction}}"
```

Order a template stable to volatile: providers cache on the request's literal
prefix. A malformed block or an unknown name is exit 2; a template that never
mentions `{{instruction}}` loads with a warning, and so does one that never
mentions `{{agents_md}}` while `agent.workspace` is set. How a turn uses it is
[How a turn runs](agent-loop.md).

## memory, knowledge, search and skills

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `memory.max_value_bytes` | int | `65536` | live · doc | The largest value a `memory.set` may store. |
| `memory.list_default_limit` | int | `100` | live · doc | Keys a `memory.list` returns when it sets no limit. |
| `knowledge.server` | string | — | live · doc | The MCP server behind the `knowledge.*` tools. |
| `knowledge.auto_context.on` | `turn \| never` | `never` | live · doc | Search the knowledge server before each turn and add the hits. |
| `knowledge.auto_context.top_k` | int | `5` | live · doc | Hits added per turn. |
| `knowledge.auto_context.max_bytes` | int | `16384` | live · doc | The most bytes of hits added per turn. |
| `search.server` | string | — | live · doc | The MCP server behind the `search.*` tools. |
| `skills.sources[].server` | string | required | live · op | An MCP server whose Skills extension (`io.modelcontextprotocol/skills`) skills are read from. Not `local`: that is the origin a reference names your own skills by. |
| `skills.sources[].filter` | string | — | live · op | One skill name, or a prefix ending in `*`: the server's skills to take. |
| `skills.sources[].dynamic` | `refuse \| allow` | `refuse` | live · op | Load a skill whose content is `dynamic`, which cannot be verified. |
| `skills.dir` | path | `skills/` beside the config, when it holds a skill | live · op | A folder of `<name>/SKILL.md` skills, the user scope. A folder that cannot be read refuses the configuration. |
| `skills.reference_prefix` | string | `@skill:` | live · doc | How a text references a skill. |
| `skills.max_loaded` | int | `8` | live · doc | Skills loaded into a context at once. |
| `skills.max_bytes` | int | `32768` | live · doc | The most text a skill body or file puts into context; the rest is cut. |

## store

Where durable state lives. Leave the section out and the shape of the instance
decides: a one-shot job gets `none`; a long-lived instance gets `file`.

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `store.kind` | `file \| mcp \| http \| memory \| none` | by shape | restart · doc | The adapter. `memory` is lost on exit; `none` keeps no state and is exit 2 on a long-lived instance. |
| `store.prefix` | string | `agentd` | restart · doc | The first key segment: `<prefix>/<instance>/<kind>/<id>`. |
| `store.timeout` | duration | `2s` | live · doc | Per-call timeout to the store. |
| `store.on_error` | `halt \| degrade` | `halt` | live · doc | What a failed durable write does: halt the daemon, or mark the store degraded, count the write as made and carry on. |
| `store.checkpoint.debounce_ms` | int | `250` | live · doc | Coalesce checkpoints written within this window. |
| `store.durability.work` | `durable \| ephemeral` | `durable` | live · doc | The default class for runs and subagent records. `ephemeral` keeps them in memory unless a workflow or spawn says `durable: true`. |
| `store.durability.a2a`, `store.durability.steps` | `strict` | `strict` | live · doc | `strict` is the only behaviour; `eventual` is exit 2. |
| `store.max_value_bytes` | int | unbounded | restart · doc | Refuse a durable write larger than this. Set it when the store's read limit is lower than its write limit. |
| `store.retention.runs.keep_last`, `.ttl` | int, duration | keep all | live · op | Drop terminal runs outside the newest N or older than the TTL. Nothing in flight is dropped. |
| `store.retention.tasks.keep_last`, `.ttl` | int, duration | keep all | live · op | The same for finished A2A tasks. `keep_last` is one bound across every principal; prefer `ttl` on a shared listener. A settled task nobody has read back is kept at least 30 seconds; the bound is applied when a task finishes and on a sweep every second. |
| `store.file.path` | path | see below | restart · op | The file store's root directory. |
| `store.file.min_free` | size | `256MB` | restart · op | Shed new work below this much free disk; warn at twice it. `"0"` disables. |
| `store.mcp.server` | string | required for `mcp` | restart · op | The MCP server whose tools hold state. |
| `store.mcp.put`, `.get`, `.list`, `.delete` | `{tool, args, ok, conflict, value, keys}` | `state.put`, `state.get`, `state.list`, `state.delete` | restart · op | The tool and argument mapping for each operation. |
| `store.http.base_url` | URL | required for `http` | restart · op | The key-value service. |
| `store.http.headers` | map | `{}` | restart · op | Request headers; credentials as references. |
| `store.http.get`, `.put`, `.list`, `.delete` | `{method, url, body, value, keys, conflict_status}` | method `GET`, `conflict_status` `409` | restart · op | The request for each operation. `get` and `put` are required. |

The file store's root is the first of: `store.file.path`, `$AGENTD_STATE_DIR`,
`$XDG_STATE_HOME/agentd/state`, `$HOME/.local/state/agentd/state`, the OS temp
directory. One process may hold a root; a second fails at startup naming the
first one's pid, so a fleet needs `mcp` or `http`. Files are `0600` in `0700`
directories, unencrypted. Running a durable daemon is
[Deploy](deployment.md); scaling it is [Scale out](scaling.md).

## streams

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `streams.<name>` | object | — | live · doc | Declares a durable event stream. An `emit` or `stream` start naming an undeclared stream is exit 2. |
| `streams.<name>.retention.max_events` | int | `10000` | live · doc | Trim from the head past this many events. |
| `streams.<name>.retention.max_age` | duration | — | live · doc | Trim events older than this. |

A stream fed by a webhook or A2A `into:` carries outside input; a workflow that
reads it into model-driven steps reaching `sensitive` and `egress` servers is
exit 2 ([Security model](security.md)).

## workflows

A `workflows:` entry is an inline definition or a reference. All `workflows`
paths are live · doc. The fields of an inline definition are in the
[Workflow language reference](workflow-language.md).

| Entry | Fields | What it does |
|---|---|---|
| inline | `name`, `steps`, … | The definition itself. |
| file | `name`, `file` | A local YAML or JSON file. |
| MCP resource | `name`, `uri` | A resource a connected server serves, read at startup. |
| URL | `name`, `url`, `headers`, `timeout` (default `20s`), `allow_private` (default `false`) | Fetched at startup; an unreachable URL or a body that does not parse refuses startup. `headers` follow the credential rules. |
| folder | `dir: <path>` or `dir: {path, glob, order}` | Every matching file becomes a workflow named by its own `name:`. `glob` defaults to `*.yaml,*.yml,*.json`; `order` is `name` or `date`. A folder that matches nothing is exit 2. |

A workflow name is one definition. Two definitions that resolve to one name,
from any two sources (including the instruction document's), are exit 2 at
startup and refuse a reload. `--validate-config` parses inline definitions and
local files and folders; `url` and `uri` definitions are checked at startup.
`security.workflows.immutable: true` makes the loaded set read-only to the
agent's `workflow.*` tools. The how-to is [Author workflows](author-workflows.md).

A definition's `act_for: {principal, via}` names the person its `schedule`,
`loop` and `once` firings act for with nobody there (needs `xaa`; [Work nobody
is there for](authentication.md#work-nobody-is-there-for)). It is the
operator's alone: an instruction document, a subagent template's machinery
and a definition `workflow.create` or `workflow.update` stored may not carry
one. It is part of the definition its digest is over, so a reload that
changes it, or anything else in the workflow, suspends its firings until the
person signs in again.

```yaml
# fragment: under workflows:
- name: inline-one
  steps:
    start: {kind: manual}
    done:  {kind: finish, depends_on: [start]}
- {name: triage, file: ./triage.yaml}
- name: from-service
  url: https://config.internal/workflows/sync.yaml
  headers: {authorization: "Bearer {{secret:WF_TOKEN}}"}
- dir: {path: ./scheduled, glob: "**/*.yaml", order: name}
```

## webhooks

The inbound webhook listener, for `webhook` start nodes and
`wait {on: webhook}`. Needs the `a2a` feature.

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `webhooks.listen` | `https://host:port`, or loopback `http://` | unset | restart · op | The listener. Required when any `webhook` node is used. |
| `webhooks.url` | `https://host[:port][/prefix]` | unset (built on `listen`) | restart · op | The URL callers reach the listener at, behind a proxy. Callback URLs are built on it. Not a wildcard host; loopback `http://` aside, `https://`. |
| `webhooks.tls.cert`, `.key`, `.client_ca` | paths | — | restart · op | The listener's TLS identity, and a client CA for mTLS. |
| `webhooks.default_auth.hmac.secret` | secret | — | live · op | HMAC-SHA256 over the body. |
| `webhooks.default_auth.hmac.header` | string | `X-Signature` | live · op | The header carrying the signature. |
| `webhooks.default_auth.hmac.prefix` | string | — | live · op | Stripped before the compare (`sha256=`). |
| `webhooks.default_auth.hmac.algo` | `sha256` | `sha256` | live · op | The algorithm. |
| `webhooks.default_auth.bearer` | secret | — | live · op | A static bearer. |
| `webhooks.default_auth.header.name`, `.equals` | string, secret | — | live · op | A named header that must equal a secret. |
| `webhooks.default_auth.none` | bool | `false` | live · op | No auth, loopback only. |
| `webhooks.default_auth.jwt` | `{issuer, audience, max_lifetime?}` | — | live · op | A person's access token from the `a2a.inbound_tokens` issuer, for this webhook's own `audience` (never one an A2A token is issued for), living at most `max_lifetime` (`5m`, at most `15m`), its `jti` never taken twice: the run is that person's, as `a2a.principals` names them. Taken only by a node of the operator's own configuration that names no auth: never by a served document's workflow or one `workflow.create`/`update` stored. Needs `a2a` and `oauth`. |

A non-loopback `webhooks.listen` needs auth on every route: a `default_auth`,
or one on each `webhook` node. Paths under `/hooks/_` are agentd's own. A route
answers as follows:

| Status | Meaning |
|---|---|
| `202` | kept: its run is in the durable inbox, or its event appended |
| `401` | authentication failed — under `auth.jwt`, a token presented before included |
| `200 duplicate` | a replay of a kept delivery (same idempotency key) within seven days |
| `413` | larger than `store.max_value_bytes` |
| `422` | the start node's `inputs` mapping cannot render it |
| `429` | refused for pressure, with `Retry-After` |
| `502` | a `respond: sync` route whose run failed |
| `503` | refused inside the loop (a freeze, a failed write, an `into:` append refused) |

## a2a

The A2A listener, the principals it admits, and the peers agentd dials. Needs
the `a2a` feature: a build without it refuses `a2a.listen`, `a2a.peers` and
`webhooks.listen` at load. The concepts are [The A2A channel](a2a.md); the API
is [A2A API reference](a2a-reference.md).

### Listener

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `a2a.listen` | `https://host:port`, loopback `http://host:port`, or `unix:///path` | unset | restart · op | The listener. An `https://` listen needs `a2a.tls.cert` and `.key`. A non-loopback bind needs `a2a.bearer` or `a2a.tls.client_ca`. |
| `a2a.url` | origin | the bind address | restart · op | The origin callers reach the listener at, published as the card's interface URL and the OAuth issuer: `scheme://host[:port]`, lowercase, no default port, no path, `https` unless loopback. Required for a wildcard bind (`0.0.0.0`, `::`). |
| `a2a.tls.cert`, `.key` | paths | — | restart · op | The listener's certificate and key. |
| `a2a.tls.client_ca` | path | — | restart · op | Verify client certificates: every caller then needs one. |
| `a2a.bearer` | secret | — | restart · op | A static operator bearer. |
| `a2a.cors.origins` | list of origins | `[]` | live · op | Browser origins admitted, matched exactly. `*` and paths are refused. A loopback UI must be listed too. |
| `a2a.events.enabled` | bool | `false` | restart · op | Declare the events extension and serve the observation feed the display clients render. |
| `a2a.introspection.enabled` | bool | `false` | live · op | Serve the operator introspection ops (transcripts, step detail, the log ring). `admin.set` can change it at runtime. |
| `a2a.push.enabled` | bool | `false` | live · op | Accept push-notification configs and deliver task updates to them. |
| `a2a.push.allow_private` | bool | `false` | live · op | Permit push targets on private or loopback addresses. |
| `a2a.resource` | URI | — | restart · op | The resource identifier enterprise access tokens are issued for (their `aud`), canonical, on the listener's origin; its RFC 9728 metadata is served under it. Needs `a2a.inbound_tokens`. |
| `a2a.inbound_tokens[]` | list | `[]` | restart · op | The authorization servers whose JWT access tokens (RFC 9068) name a caller: `{issuer, name?, profile?, audiences?, jwks_uri?, algs?, typ?, subject_claim?, tenant_claim?, clients, require_user_token?, max_token_age?, max_chain_depth?, flow?, scopes_to_skills?}`. A TCP listener and `a2a.resource` only. Needs `a2a` and `oauth` ([Enterprise access tokens](a2a.md#enterprise-access-tokens); every field in the [A2A API reference](a2a-reference.md#listener-settings)). |

With no credential at all, a loopback listener treats a local caller that
presents nothing as the operator and warns at startup. Any credential
mechanism ends that, and a request carrying `Origin` never gets it.

### Principals

`a2a.principals[]` maps a caller's evidence to a principal. All paths are
live · op.

| Path | Type | Default | What it does |
|---|---|---|---|
| `a2a.principals[].match.san` | glob | — | Matches a verified client certificate's SANs or subject. |
| `a2a.principals[].match.sub` | glob | — | Matches a verified client certificate's subject CN. |
| `a2a.principals[].match.bearer_ref` | secret | — | Matches a presented bearer. May not equal `a2a.bearer`, another rule's secret, or start with `agentd_at_`. |
| `a2a.principals[].match.any` | bool | `false` | Names every caller left; cannot grant `operator`. |
| `a2a.principals[].match.uid` | int | — | The unix listener's peer, by the uid the kernel names (`SO_PEERCRED`): the per-person operator `operator:<id>`. Alone in its `match`, with `role: operator` and an `id`, on a `unix://` listener. Needs `xaa`. |
| `a2a.principals[].match.spki` | 64 lowercase hex | — | A verified client certificate's key, the SHA-256 of its SubjectPublicKeyInfo: the rule admits that key alone, beside any `san` or `sub`. Needs `a2a.tls.client_ca`. With an `id` and `idp_subject`, a certificate principal that delegates; an operator one is `operator:<id>`. |
| `a2a.principals[].match.token_iss` | issuer | — | An enterprise access token from this `a2a.inbound_tokens` issuer; with `sub`, a glob over its subject. `role` `user` or `agent`, no `id` or `idp_subject`: the token names the person, `user:oidc:<iss>#<subject>` (`#<tenant>/<subject>` under `tenant_claim`). Without a rule, a token's caller is a `user`. The same rules name the person a webhook's `auth.jwt` token is. |
| `a2a.principals[].idp_subject` | `{iss, sub}` | — | The one IdP identity this principal is, exact: required to delegate. On a `match.uid` rule, or a `match.spki` one with an `id`; `iss` is an `identity.providers` issuer. Several rules may each carry one, one per person. Needs `xaa`. |
| `a2a.principals[].role` | `operator \| user \| agent \| anonymous` | required | The caller's role. |
| `a2a.principals[].id` | string | derived | The principal id (`<role>:<id>`), unique across rules; `1..128` of `A-Z a-z 0-9 . _ @ : / + -`. Required on `bearer_ref` and `any` rules; a certificate rule without one is `<role>:cn=<CN>` or `<role>:san=<first SAN>`. |
| `a2a.principals[].grants` | list of op globs | — | The operations the principal may call. |
| `a2a.principals[].labels` | map | `{}` | Operator-declared attributes carried into the run, the MCP `_meta` and the audit line. |
| `a2a.principals[].quotas.rate` | rate | — | Arrival quota at the listener; operators are exempt. |
| `a2a.principals[].quotas.budget` | budget | — | A token budget charged by the work done for this principal. |

A rule matches only a caller that every field its `match` names admits:
`san` and `sub` together admit a certificate both match, and `bearer_ref`
beside either admits the bearer only with a certificate they match. `any`
names every caller left, so it is a rule's only matcher. A glob has one `*`
for any run of characters. A `san` or `sub` rule matches only a certificate
the listener verified, so it needs `a2a.tls.client_ca` and is refused at
load without it; an `any` rule needs no CA. A `user` id may not reuse a name
ever approved through the device grant.

### Device grant

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `a2a.device_grant.enabled` | bool | `false` | restart · op | Serve the OAuth device authorization grant: a client shows a code, an operator approves it. |
| `a2a.device_grant.scopes` | list of `user \| operator` | `[user]` | restart · op | What a client may request. |
| `a2a.device_grant.token_ttl` | duration, 5m–30d | `8h` | restart · op | Session lifetime. |
| `a2a.device_grant.code_ttl` | duration, 1m–30m | `10m` | restart · op | How long a code waits for approval. |
| `a2a.device_grant.verification_uri` | URL | the listener's own page | restart · op | Where the approving person is sent. |
| `a2a.device_grant.rate` | rate | unlimited | restart · op | Per session principal. |
| `a2a.login.idp` | `identity.providers` entry | — | restart · op | Display clients sign people in at this IdP, each session the person it names, `user:oidc:<iss>#<sub>`; where a template admits them, the sign-in is their enrolment. The provider's `sign_in.grants` must include `authorization_code`. A TCP listener only, never beside `a2a.tls.client_ca`. Needs `xaa` and `a2a` ([The A2A channel](a2a.md#a-sign-in-at-the-identity-provider)). |
| `a2a.login.token_ttl` | duration, 5m–30d | `8h` | restart · op | Session lifetime. |

It needs an operator credential (`a2a.bearer`, or an operator rule with
`bearer_ref`) and is refused beside `a2a.tls.client_ca` and on a `unix://`
listener.

### Peers

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `a2a.peers[].name` | string | required | live · doc | The peer's name, unique. |
| `a2a.peers[].endpoint` | `https://`, loopback `http://`, or `unix:///path` | required unless `service` | live · doc | Where the peer listens. |
| `a2a.peers[].service` | string | — | live · doc | A `kind: peer` service entry to inherit from. |
| `a2a.peers[].headers` | map | `{}` | live · doc | Extra headers; credentials as references. |
| `a2a.peers[].auth` | object | — | live · doc | A credential provider. |
| `a2a.peers[].client_cert`, `.client_key` | paths | — | live · doc | A client certificate for an mTLS peer. |

The how-to is [Connect agents over A2A](connect-agents.md).

```yaml
# fragment: under a2a:
listen: https://0.0.0.0:8443
url: https://desk.example.com:8443
tls: {cert: /tls/cert.pem, key: /tls/key.pem}
bearer: "{{secret:A2A_OPERATOR_TOKEN}}"
cors: {origins: ["https://ui.example.com"]}
principals:
  - id: acme-portal
    match: {bearer_ref: "{{secret:ACME_PORTAL_TOKEN}}"}
    role: user
    labels: {tenant: acme}
    quotas: {rate: "30/60s", budget: {windows: [{per: day, tokens: 200000}]}}
device_grant: {enabled: true}
events: {enabled: true}
```

## Asking a person

Who answers an ask is spread over three keys, gathered here.

| Path | Default | Class | What it does |
|---|---|---|---|
| `agent.ask_human_unowned` | `fallback` | live · doc | See [agent](#agent). |
| `agent.ask_human_fallback` | `fail` | live · doc | See [agent](#agent). |
| `security.policies[].to` | `{role: operator}` | live · op | Which operator may answer an `action: ask` gate: `{role: operator}` narrowed by `labels`, or the id `operator`. |

An addressee must be able to see the task, and a task is visible only to its
owner and to operators. A `to:` naming any other principal, or a channel, is
exit 2. The how-to is [Human in the loop](human-in-the-loop.md).

## subagents

Templates the model may instantiate, and the defaults for every spawn. All
paths are live · op.

| Path | Type | Default | What it does |
|---|---|---|---|
| `subagents.allow_freeform` | bool | `true` | `false` makes templates the only spawn path. |
| `subagents.defaults.model` | string | `roles.turn` | The model a spawn runs on. |
| `subagents.defaults.mode` | `sync \| async \| detached \| warm` | `sync` | How the parent waits. |
| `subagents.defaults.priority` | `low \| normal \| high` | `normal` | Contention weight. |
| `subagents.defaults.durable` | bool | `store.durability.work` | `false` keeps the spawn's record in memory. |
| `subagents.defaults.limits` | object | — | Per-spawn limits (`max_steps`, `max_tokens`, `deadline`, `memory`, `cpu`). |
| `subagents.templates.<name>.instruction` | string, or `{text \| file \| dir \| url \| oci}` | required | The child's instruction document. Machinery in it (`:::!workflow`, `:::!mcp`, `:::!stream`, `:::!config`, `:::!tools`) makes the child an instance-tier daemon; otherwise it is a flat worker. |
| `subagents.templates.<name>.params.<param>` | `{type, required, default, enum, description}` | — | The only values the model may fill, checked at spawn. `type` is `string` (default), `number`, `integer` or `boolean`. |
| `subagents.templates.<name>.servers`, `.tools` | lists | the parent's | Flat tier: narrow the parent's grants. |
| `subagents.templates.<name>.model`, `.mode`, `.priority`, `.skills`, `.context` | — | the defaults | Per-template overrides. Instance tier supports `mode: detached`, and `sync` with `result`. |
| `subagents.templates.<name>.limits` | object | — | Flat tier: the spawn limits. Instance tier: `memory` and `cpu` only. |
| `subagents.templates.<name>.budget` | budget | — | Instance tier: the child's own budget. |
| `subagents.templates.<name>.output_contract`, `.output_schema` | string, object | — | What the child must return. |
| `subagents.templates.<name>.ttl` | duration | — | Instance tier: retire gracefully after this long. |
| `subagents.templates.<name>.until` | string | — | Instance tier: a signal name whose delivery in the child retires it. |
| `subagents.templates.<name>.singleton` | bool | `false` | One live child; its peer alias is the template name. |
| `subagents.templates.<name>.durable` | bool | `store.durability.work` | `false` keeps the record in memory. A parent with a `memory` or `none` store runs every instance child non-durable. |
| `subagents.templates.<name>.result.workflow` | string | — | Instance tier, `mode: sync`: resolve the spawn when the child's named workflow first completes. Needs a parent A2A listener. |
| `subagents.templates.<name>.mirror_streams` | list | — | Instance tier: child streams forwarded into the parent's same-named streams. Needs a parent A2A listener. |

The how-to is [Delegate to subagents](delegate-subagents.md).

## spawn

What every child process — turn worker, subagent, think child, instance — is
started with. A child's environment is an allow-list: the baseline (`PATH`,
`HOME`, `TMPDIR`, `TZ`, `LANG`, `AGENTD_CRED_DIR`, `RUST_LOG`, `NO_PROXY`,
`XDG_*`, `LC_*`, `SSL_CERT_*`, `OTEL_*`, `*_PROXY`), the variables its own
secret references and routed `aws` credentials read, and this list
([Security model](security.md#secrets)).

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `spawn.env_passthrough` | list of names and `PREFIX*` patterns | `[]` | live · op | Variables every child also gets, when agentd has them. `*` alone, and a name a supervisor-only secret is read from, are refused. |
| `spawn.uid`, `spawn.gid` | ints ≥ 1 | — (agentd's own) | restart · op | The uid and gid every worker runs as — turn workers, subagents, think children, an instance child's own workers, `exec` commands — with no supplementary group, inside its Landlock domain as before. Both or neither. Every capability is dropped with the uid, and `no_new_privs` set, so a worker can never set its uid back. Needs root, or `CAP_SETUID`, `CAP_SETGID` and `CAP_KILL` (with an instance-tier template, root); refused at load where agentd cannot drop to them, and for agentd's own uid, root's, or an external signer's ([Security model](security.md#a-container-deployment-with-a-worker-uid)). With `spawn.instance_uid`, an instance child's own workers run under that uid instead. |
| `spawn.instance_uid`, `spawn.instance_gid` | ints ≥ 1 | — (agentd's own) | restart · op | The uid and gid every instance-tier child daemon runs as, with no capability, inside its Landlock domain; its directory is made its own first, and its own workers run under it too. Both or neither. Needs agentd to run as root; refused for root's, agentd's own, an external signer's uid or `spawn.uid` ([Subagents](subagents.md#how-an-instance-child-reaches-its-parent)). |

## goal

A goal watchdog that checks progress and acts. All paths are live · doc. A
`goal` makes the instance long-lived.

| Path | Type | Default | What it does |
|---|---|---|---|
| `goal.statement` | string | — | The goal in natural language, for the judge. |
| `goal.check.every` | duration | `5m` | The check cadence. |
| `goal.check.condition` | CEL | — | A cheap predicate over durable state, evaluated first. Needs `cel`. |
| `goal.check.via` | `both \| condition \| agent` | `both` | CEL then the judge, CEL only, or the judge only. The judge runs on `intelligence.roles.goal`. |
| `goal.stuck_after` | int | `3` | Consecutive checks without progress before acting. |
| `goal.on_achieved` | `finish \| idle \| replan \| escalate`, or `{workflow}` | `finish` | What to do when achieved. |
| `goal.on_stuck` | same | `replan` | What to do when stuck. |

## limits

The hard bounds the supervisor enforces. All paths are live · doc.

| Path | Type | Default | What it does |
|---|---|---|---|
| `limits.max_runs` | int | `8` | Workflow runs live at once. |
| `limits.run.steps` | int | `500` | Steps per run. |
| `limits.run.tokens` | int | `2000000` | Tokens per run. |
| `limits.run.deadline` | duration | `3600s` | Wall-clock per run; it also bounds a model call in flight. |
| `limits.step_timeout` | duration | `600s` | A step's timeout when it sets none. |
| `limits.inline_max_bytes` | int | `65536` | A step output larger than this is stored as an artifact and replaced by a reference. |
| `limits.max_message_depth` | int | `8` | Chained `message` deliveries before one is refused. |
| `limits.workflow.fan_out` | int | `8` | Concurrent lanes a `foreach` or `batch` body may use; a definition asking for more is exit 2. |
| `limits.subagents.depth` | int | `3` | How deep delegation may nest. |
| `limits.subagents.breadth` | int | `8` | Live children at once. |
| `limits.subagents.total` | int | `64` | Children over the process's life. |
| `limits.subagents.rate` | rate | `8/2s` | Spawn rate. |
| `limits.subagents.instances.breadth` | int | `2` | Live instance-tier children. |
| `limits.subagents.instances.total` | int | `8` | Instance-tier children over the process's life. |
| `limits.subagents.instances.rate` | rate | `4/1h` | Instance-tier spawn rate. |

A one-shot that exhausts its steps or tokens exits 7, and one that passes its
deadline exits 124; [cli.md](cli.md) has the full table.

## lifecycle

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `lifecycle.run_until` | `auto \| idle \| drained` | `auto` | restart · doc | `idle`: a job that exits once no work remains. `drained`: a daemon that exits only on a signal; refused when nothing can give it work (no `a2a.listen`, workflow or `agent.prompt`; a webhook listener or a `goal` only starts workflows). `auto`: a daemon when the instance has an A2A listener or a long-lived start node, otherwise a job. |
| `lifecycle.idle_grace` | duration | `5s` | live · doc | How long an idle job waits before exiting. |
| `lifecycle.until_signal` | string | — | live · doc | A signal whose delivery begins a graceful shutdown. |
| `lifecycle.drain_timeout` | duration | `25s` | restart · op | The graceful drain budget on `SIGTERM`: how long work already running may finish before the drain stops it and leaves it for the next start. Keep it below the pod's `terminationGracePeriodSeconds`. |
| `lifecycle.run_id` | string | a fresh ULID | restart · op | Sent as `agent/run_id` in every MCP call's `_meta`, beside `agent/instance`. Set a stable one per unit of work (a Job name, an input hash) so servers can dedupe retries; the default correlates logs but dedupes nothing. |
| `lifecycle.exit_code_map` | map: `"3"` or `"7"` → 0–255 | `{}` | restart · op | Remap the policy exit codes 3 (partial) and 7 (budget). Any other key is exit 2. |
| `lifecycle.watch_config` | bool | `false` | restart · op | Reload when a config file, the instruction file or folder, a workflow file or folder, a skill file, or an `AGENTS.md` of `agent.workspace` on the way down to the working directory changes. Needs a `--config` file and the `config-watch` feature. |

The long-lived start kinds are every kind but `once` and `manual`; they are
listed in [Workflow node reference](node-registry.md) and explained in
[Jobs, daemons and triggers](modes-and-triggers.md).

## identity

Who autonomous work is done for. All paths are live · op.

| Path | Type | Default | What it does |
|---|---|---|---|
| `identity.autonomous_as` | string | `system` | The principal a schedule, webhook, stream or `once` firing runs as. It may not be the per-person operator of a delegation (`operator:<id>`, the `xaa` feature): autonomous work never acts with an enrolled person's grant. |
| `identity.labels` | map | `{}` | Labels stamped on that work. |
| `identity.providers.<name>` | object | — | An enterprise IdP agentd is a client of: `issuer`, `client_id` (required), `profile` (`generic \| okta`), `client_auth`, `client_secret`, `sign_in`, `subject`, `subjects`, `jwks_uri`, `equivalent_issuers` (the `a2a.inbound_tokens` issuers whose subjects are this provider's), `ciba`. Restart. Needs `xaa`; see [Enterprise-managed authorization](authentication.md#enterprise-managed-authorization). |
| `identity.providers.<name>.ciba` | `{enabled, request_expiry, per_day}` | `{false, 300s, 10}` | OpenID CIBA in poll mode at this IdP: how a `via: ciba` firing asks its person on their own device. `request_expiry` (1s–1h) is sent as `request_expiry` under `profile: okta`, else `requested_expiry`; `per_day` caps the requests one person is sent a day. The IdP's metadata must name its `backchannel_authentication_endpoint` and list `poll`. Restart. |
| `identity.delegation.seal_key`, `.seal_key_previous` | secret reference | — | 32 random bytes, hex or base64, from a mounted file or the startup prompt — never the environment. Every grant's refresh token is sealed with `seal_key`; `seal_key_previous` only opens. Required with a `grant: id_jag` or `token_exchange` server under `key_custody: local`; refused under `external`. Restart. |
| `identity.delegation.key_custody` | `local \| external` | `local` | Who holds the seal key and every `private_key_jwt` key under `identity.providers` and on a delegated server: agentd, from secret references, or the external signer. Under `external` each such key is named by `client_auth.kid` with no `key`. Changing it means enrolling again. Restart. |
| `identity.delegation.signer` | `{socket, uid, timeout, seal_key, seal_key_previous}` | — | `key_custody: external` only (required there): the signer's unix socket (an absolute path), the uid its process runs as (the socket's peer must be it; agentd's own uid is a load warning, since the peer check cannot then tell the signer from agentd's workers), the bound on a whole request, the connect included (`5s`, at most `60s`), and the ids of its key-encryption key and of the one before a rotation, which only unwraps ([The external signer protocol](external-signer.md)). Restart. |
| `identity.delegation.at_cache_cap` | duration | `60m` | The longest an access token is used, whatever `expires_in` says. Restart. |
| `identity.delegation.liveness_interval` | duration | `15m` | An access token is used only within this of the grant's last proof that its refresh token is alive. Restart. |
| `identity.delegation.consent_timeout` | duration | `24h` | How long a consent gate waits before it denies. Reload. |
| `identity.delegation.consent_session_max_age` | duration | `12h` | A display session answers a consent gate only while its own IdP sign-in is younger than this. Reload. |
| `identity.delegation.consent_per_minute` | int | `10` | How many consent gates may open for one person a minute; a call past that is refused rather than asked. At least 1. Reload. |
| `identity.delegation.templates[]` | list | `[]` | Who may enrol, for what: `{name, principals: {source, role?, labels?}, idp, servers: {<name>: {scopes, document_workflows?}}, ttl, budget: {per_day, per_month, per_run, per_minute}, subagents, approvers, agent_mode_servers}`. `source` is `operator` (a per-person operator), `oidc` (an `a2a.login` session), `x509` (a certificate principal its rule pins by key) or `device` (a device session a pin or the first binding makes eligible); `subagents` is `none` (default) or `inherit` (a subagent acts with the grant at the delegated servers it declared); `approvers` is `[principal]` (default) or `[principal, operator]`; `agent_mode_servers` are the agent-mode servers tagged `sensitive` or `egress` the person's work may still use (every other one is hidden from it); `document_workflows` (`{name, digest}`, the `hash` `workflow.list` shows) are workflows an instruction document defined whose calls to the server are not `document_sourced`; `per_minute` (default 4) paces mints per person and server; `clients` admits only the callers whose enterprise access token one of these front ends was issued (`source: oidc`); `unattended` (`never`, the default, `ciba` or `standing`, which allows `ciba` too) lets an `act_for` workflow's firings act with the grant with nobody there, `standing` at a `profile: okta` IdP only with `allow_against_idp_guidance: true`; `unattended_tools` are the delegated `<server>.<tool>`s tagged `egress` a firing may call (every other is refused). No template matching, no enrolment. Reload: a narrowed one holds its grants at the next call; one removed, or a server removed from one, revokes what it allowed. |
| `identity.delegation.device_pins.<name>` | `{iss, sub}` | — | The one IdP identity a device name delegates as. Needs `a2a.device_grant`. Reload: a changed or removed pin revokes what it bound. |
| `identity.delegation.device_first_binding` | bool | `false` | An unpinned device name binds to the identity of its first enrolment (a load warning: a forwarded sign-in link binds it to whoever completes it). Reload: turned off, it revokes what it bound. |

The acting principal and its labels reach the run record (`run.start` carries
`acting_for`), each MCP call's `_meta` (`agent/acting_for`, `agent/labels`) and
the audit line. A call to a delegated server carries the ID-JAG's `sub` as
`agent/acting_for` instead, and no labels. A run is charged to the principal it is for, so
`quotas.budget` covers work a person started, not only their turns. Labels are
an operator-declared, closed set. This is attribution and quota, not
multi-tenancy: a caller that needs a different tool surface needs a different
process.

## observability

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `observability.log_level` | `trace \| debug \| info \| warn \| error` | `info` | live · doc | The JSON-lines log level on stderr. |
| `observability.log_content` | bool | `false` | live · op | Log tool arguments and results, not only their lengths. |
| `observability.metrics_addr` | `host:port` or `:port` | unset | restart · op | Serve `/metrics`, `/healthz` and `/readyz`. Needs `metrics`. |
| `observability.health_file` | path | unset | restart · op | A liveness file rewritten every second; `alive` turns false when the supervisor has not ticked for 10 seconds. |
| `observability.events_ring` | int | `1024` | restart · op | Log lines held in memory for the introspection ops; allocated only when introspection is on. |
| `observability.traceparent` | W3C traceparent | a random trace | restart · op | Continue an upstream trace: the process's, and the work it was started to do. |
| `observability.otel.endpoint` | URL | `$OTEL_EXPORTER_OTLP_ENDPOINT` | restart · op | The OTLP/HTTP base URL, `http://` or `https://`; anything else is refused at load. Each run's spans are sent to `<endpoint>/v1/traces`. Needs `otel`; `https://` also needs `tls`. |
| `observability.otel.logs` | bool | `false` | restart · op | Also send the log lines to `<endpoint>/v1/logs`. |
| `observability.audit.sink` | list of `log \| store \| stream` | unset (no audit records) | live · op | Where audit records go. `store` needs a store; `stream` needs `audit.stream`. |
| `observability.audit.stream` | string | — | live · op | The declared stream `sink: [stream]` appends to. |
| `observability.audit.chain.path` | absolute path | — | restart · op | The hash-chained audit log: every audit record appended, each carrying the SHA-256 of the one before, verified at every start; a chain that does not hold refuses the start. Written whatever `sink` says. The tombstone register is anchored in it. Closed segments are kept beside it as `<path>.<first seq>`. Its head is kept in the durable store: with `store.kind: none` or `memory` the load warns that a cut tail is not caught. |
| `observability.audit.chain.rotate_at` | size | `64MiB` | restart · op | The size the open segment is closed at; at least `64KiB`. |
| `observability.audit.chain.retention` | duration | `400d` | restart · op | How long a closed segment is kept after its last record. Each deletion is a record naming the segment's last record; a segment removed by hand refuses the start. Each segment's header carries the register, so no revocation is forgotten. |
| `observability.runtime_events.stream` | string | required | live · doc | A declared stream that receives selected runtime events. |
| `observability.runtime_events.include` | list of families | `[]` | live · doc | Event families taken in full (the segment before the first dot). |
| `observability.runtime_events.sampled` | list of families | `[]` | live · doc | Families taken one in sixteen. |
| `observability.runtime_events.queue` | int | `512` | live · doc | Events that may queue between ticks before the tap drops and counts. |
| `observability.status_values` | list of memory keys | `[]` | live · op | Memory keys the `status` op publishes. Every caller of `status` sees them. |

The events and metrics are in
[Events and metrics reference](telemetry-reference.md); the guide is
[Monitor in production](observability.md).

## security

Every `security` path is restart · op but `security.policies`, which a
reload swaps whole or not at all ([Reload classes](#reload-classes)).

| Path | Type | Default | What it does |
|---|---|---|---|
| `security.allow_trifecta` | bool | `false` | Downgrade the lethal-trifecta refusals to an audited warning. Refused from a discovered config. |
| `security.egress` | `open \| closed` | `open` | `closed` refuses any outbound destination with no matching `services:` entry. |
| `security.tls_ca` | path | the bundled roots only | Extra PEM CA certificates trusted for every outbound `https://` dial, added to the bundled roots. Needs `tls`. |
| `security.workflows.immutable` | bool | `false` | Refuse `workflow.create`, `update` and `delete` at runtime. |
| `security.policies[]` | list | `[]` | Ordered verdicts on a tool call; the first match wins, no match is allow. |
| `security.policies[].match.tool` | glob | every tool | Tool name. A Company's operation tool's call also matches as `company.api`'s ([operation tools](#a-companys-operation-tools)). |
| `security.policies[].match.tags` | list of tags | — | Every listed tag must be on the tool. |
| `security.policies[].match.caller` | list of `root \| workflow \| subagent` | — | Who is calling. |
| `security.policies[].match.principal` | glob | — | The principal id, for calls that carry one. |
| `security.policies[].match.args` | CEL | — | A predicate over `args`, `tool` and `caller`. Needs `cel`. |
| `security.policies[].match.delegated` | bool | — | The call is made with an enrolled person's grant. |
| `security.policies[].match.tainted` | bool | — | The work making the call has read outside text — a delegated server's result (unless `trusted_output`), an `untrusted_input` tool's, a webhook's or a non-operator principal's message (a person's own, or a per-person operator's, only to work not acting with their grant), a peer's answer, an `http` step's response, knowledge context, a served instruction, a tainted memory entry ([Security model](security.md#acting-for-an-enrolled-person)). |
| `security.policies[].match.holds_delegated_data` | bool | — | A delegated server's result has reached the work making the call. |
| `security.policies[].match.any_tags` | list of tags | — | At least one listed tag is on the tool. |
| `security.policies[].match.unattended` | bool | — | Nobody is there to answer: a trigger started the work (a schedule, a webhook, a stream), not a principal's request. |
| `security.policies[].match.document_sourced` | bool | — | A workflow an instruction document defined makes the call — for a delegated call, one the person's template does not list for the server in `document_workflows`. |
| `security.policies[].match.mode` | `agent \| delegated` | — | Whose authority the work acts with: an enrolled person's grant, or agentd's own. |
| `security.policies[].action` | `allow \| deny \| ask \| shadow` | required | `shadow` refuses and says the call was held. |
| `security.policies[].question` | string | — | For `ask`: the question; `{{tool}}` (quoted), `{{caller}}` and `{{args}}` (the call's arguments as printable-ASCII JSON, at most 1,500 characters) are substituted — the gate carries agentd's badge, so what the work chose is shown escaped. |
| `security.policies[].timeout` | duration | — | For `ask`: how long to wait. |
| `security.policies[].on_timeout` | `allow \| deny \| ask \| shadow` | `deny` | What an unanswered ask becomes. |
| `security.policies[].to` | `{role: operator, labels}`, `operator` or `principal` | `{role: operator}` | Who may answer an `ask`. `principal` is the person whose grant the work acts with: the gate is their consent gate — agentd's own text (no `question`), never judged, denied unanswered — and needs a delegated server ([Enterprise-managed authorization](authentication.md#enterprise-managed-authorization)). |
| `security.policies[].once_per` | `run_tool` | — | For a `to: principal` rule: ask at the first use per run — or conversation, or subagent — and tool alone; the person's approval, kept with the run in the store, passes later calls of the tool of that shape there — after a restart too — while the rule, their grant and their template are unchanged, for at most a day. |
| `security.experimental.id_jag` | bool | `false` | Allow `grant: id_jag` and `grant: token_exchange` servers until ID-JAG reaches IETF working-group last call. Needs `xaa`. |
| `security.exec.enabled` | bool | `false` | Run the `exec` tool locally. Needs the `exec` feature, which no release binary carries. Beside a delegation (`identity.providers`, a seal key, a delegated server) only as `spawn.uid` inside Landlock, with every supervisor-only path closed to that uid; the load names each piece missing. |
| `security.exec.allow` | list | `[]` (deny all) | Allowed command names (`argv[0]`). |
| `security.exec.workdir` | path | — | The directory commands run in; a requested `cwd` must resolve inside it. |
| `security.exec.timeout` | duration | `30s` | Wall-clock per command. |
| `security.exec.max_output` | int | `1048576` | Captured stdout and stderr bytes. |
| `security.exec.env` | list of names | `[]` | Environment variables passed through. |
| `security.cgroup.spec` | `auto` or a path | unset | A cgroup-v2 parent for children: `auto` derives `<own cgroup>/agent`. Linux only, best-effort. |
| `security.cgroup.memory_max` | `max` or a size | — | Per-child `memory.max`. |
| `security.cgroup.pids_max` | `max` or a count | — | Per-child `pids.max`; counts threads. |
| `security.aauth.provider`, `.key_file`, `.enroll_token`, `.enroll_assertion_file`, `.person_server` | — | — | The AAuth agent identity. Needs `aauth`; see [AAuth agent identity](aauth.md). |
| `security.client.id` | HTTPS URL | — | agentd's OAuth client where it has no registration: the `client_id` of its [Client ID Metadata Document](authentication.md#client-id-metadata-document), and where the document is published. A path of its own, with no userinfo, query, fragment or `.`/`..` segment. An `oauth2` block with no `client_id` uses it. Needs `oauth`, like every `security.client` path. |
| `security.client.name`, `.logo_uri` | string, HTTPS URL | required, — | The document's `client_name` and `logo_uri`. |
| `security.client.jwks_uri` | HTTPS URL | `jwks.json` beside the document | The key set's URL. |
| `security.client.key_file` | path | `/var/lib/agentd/client-key` | The ES256 signing key (PKCS#8), created `0600` when absent. Read where a token request is made. |
| `security.client.published_key_files` | list of paths | `[]` | Keys whose public halves the key set lists and that are never signed with: the next key before a rotation, the previous one after. |
| `security.client.grants` | list of `device \| client_credentials \| authorization_code` | `[device]` | The grants the document lists in `grant_types`, with `refresh_token` beside `device` and `authorization_code`. A block that uses the document with another grant is refused at load. |
| `security.client.redirect_uris` | list of loopback URIs | `[]` | For `authorization_code`, and only with it: `http://127.0.0.1[:port]/path` or `http://[::1][:port]/path`. A login listens on the first one's port, or on a free port when it names none. |
| `security.client.serve` | bool | `false` | Serve the document and the key set on the A2A listener, at their URLs' paths. Needs `a2a`, and `a2a.url` the `client_id`'s origin. Otherwise publish what `agentd --print-client-metadata` writes. |
| `security.companies.user` | `operator` or a principal id | `operator` | The one User this instance acts for at every Company. `operator` is refused when `a2a.principals` holds more than one rule of role `operator`. Needs `companies`, like every `security.companies` path. |
| `security.companies.client.id` | HTTPS URL | required with a company entry | The `client_id`: the URL of the client metadata document. |
| `security.companies.client.name`, `.logo_uri` | string, HTTPS URL | required, — | The document's `client_name` and `logo_uri`. |
| `security.companies.client.jwks_uri` | HTTPS URL | `<client_id origin>/companies/jwks.json` | The key set's URL, on the `client_id`'s host. |
| `security.companies.client.key_file` | path | `/var/lib/agentd/company-client-key` | The ES256 signing key (PKCS#8), created `0600` when absent. Only the supervisor reads it. |
| `security.companies.client.published_key_files` | list of paths | `[]` | Keys whose public halves the key set lists and that are never signed with: the next key before a rotation, the previous one after. |
| `security.companies.client.redirect_uri` | HTTPS URL | `<client_id origin>/companies/callback` | Where Direct Sign-In returns the User, on the `client_id`'s host, with no query. Listed in the document's `redirect_uris` when a Company lists `direct`, which is `[]` otherwise. |
| `security.companies.client.serve` | bool | `false` | Serve the document and the key set on the A2A listener, at their URLs' paths, and the Direct Sign-In callback at `redirect_uri`'s when a Company lists `direct`. Needs `a2a`, and `a2a.url` the `client_id`'s origin. Otherwise publish what `agentd --print-company-client` writes. |
| `security.companies.client.extensions` | list of `operations` | `[]` | The extensions the document lists. `operations` (version 1): a Company may propose an action as an operation, which agentd confirms only with the User's approval or a standing permission ([A Company's operations](#a-companys-operations)). |
| `security.companies.standing[]` | list | `[]` | Standing permissions: an operation one covers is confirmed with `approved_by: standing_permission`, the User not asked. Each needs `company`, `via`, `terms`, `when` and `expires`. Needs `client.extensions: [operations]` and the `cel` feature. |
| `security.companies.standing[].company` | entry name | required | The company entry it covers. |
| `security.companies.standing[].via` | `{tool: <glob>}` or `{api: "METHOD /path-template"}` | required | The channel that proposed the operation: a glob of a Company tool's registered name (`shop.exchange_*`), or an operation `api_allow` lists. An operation proposed in a conversation never matches. |
| `security.companies.standing[].terms` | list of term keys | required | Every top-level term key it covers: a revision with another key, or with no terms, asks the User. |
| `security.companies.standing[].when` | CEL | required | A predicate over `terms` and `summary`; a term it reads that is missing is no match. |
| `security.companies.standing[].expires` | RFC 3339 time | required | When it stops applying. |
| `security.companies.standing[].uses` | int ≥ 1 | no limit | How many confirms it covers: each reserved with the confirm, spent once the Company records a confirmation by it, and given back when the Company does not take the confirm or the operation ends unconfirmed. Counted per permission as written: one changed starts its count again. |
| `security.companies.standing[].accounts` | list of account labels | `default` alone | The User's accounts at the Company it covers, by the labels sign-ins gave them (`[a-z0-9_-]{1,32}`; [Several accounts](act-at-companies.md#several-accounts)): an operation proposed in another account's Session asks the User. Without it, the account labelled `default` and the signed-out Session alone, so signing in to a second account never widens it. |
| `security.companies.seal_key` | secret reference | — | 32 random bytes, hex or base64. Seals the Account Tokens a sign-in gives agentd, and the User ID records, so the store cannot read them. Required with any `sign_in`, and with a remote store, under `key_custody: local`; refused under `external`. |
| `security.companies.key_custody` | `local \| external` | `local` | Who holds the seal key: agentd, from `seal_key`, or the external signer `signer` names, which wraps each record's data key and derives the record-id keys, so agentd holds neither ([The external signer protocol](external-signer.md)). A store sealed under one custody does not open under the other. Restart. |
| `security.companies.signer` | `{socket, uid, timeout, seal_key, derive_key}` | — | `key_custody: external` only (required there): the signer's unix socket (an absolute path), the uid its process runs as (agentd's own is a load warning), the bound on a request (`5s`, at most `60s`), its key-encryption key's id and its derivation key's id. Restart. |
| `security.companies.browsers` | list of server names | `[]` | Browser MCP servers to keep off every Company's hosts; each must name an `mcp.servers` entry. A call on one with any string argument naming a Company's host as a browser reads it — or a host that is not ASCII — is refused unsent (`company.browser.refused`); their tools are served by the supervisor. |
| `security.companies.limits.in_flight` | int | `4` | Company calls in flight per Company; one more is refused until one ends. |
| `security.companies.limits.conversations` | int | `16` | Open conversations in the instance, each followed by a reader; a new one is refused until one closes. |
| `security.companies.context.locale` | language tag | — | Shared in a conversation message's `context` as `locale` (RFC 5646, `en-US`). Needs an entry that uses `agent`. |
| `security.companies.context.time_zone` | IANA name | — | Shared as `time_zone` (`America/Los_Angeles`). |
| `security.companies.context.share_availability` | bool | `false` | Share `user_available`: whether the User has a client attached to the observation feed now. |
| `security.payments.approver` | `operator` or a principal id | `operator` | Who approves a payment the policy asks about: any operator principal, or that one principal (`user:alice`). A glob or a role is refused. Needs `x402`, like every `security.payments` path ([Payments](#payments)). |
| `security.payments.approval_timeout` | duration, 1s–7d | `1h` | How long the approver is waited for; unanswered, the payment is declined. |
| `security.payments.ask_when_tainted` | bool | `true` | Ask the approver about every payment of work that has read outside text, or that a model asked for, whatever its amount. |
| `security.payments.max_validity` | duration, 10s–1h | `5m` | The longest an authorization agentd signs is valid; the server's `maxTimeoutSeconds` when shorter. |
| `security.payments.key_custody` | `local \| external` | `local` | Who holds the wallet keys: agentd, each key file sealed under `seal_key`, or the external signer `signer` names ([The external signer protocol](external-signer.md#wallet-keys)). |
| `security.payments.seal_key` | secret reference | — | 32 random bytes, hex or base64: every wallet key file is sealed under it. A single `{{secret-file:…}}` reference, never inline and never the environment. Required under `key_custody: local`; refused under `external`. |
| `security.payments.signer` | `{socket, uid, timeout}` | — | `key_custody: external` only (required there): the signer's unix socket (an absolute path), the uid its process runs as (agentd's own is a load warning) and the bound on a request (`5s`, at most `60s`). |
| `security.payments.wallets.<name>.chain` | `eip155 \| solana` | `eip155` | The chain family it pays on: a secp256k1 key on EVM chains, an Ed25519 key on Solana. A step's `max` may name only assets of its wallet's family. |
| `security.payments.wallets.<name>.key_file` | absolute path | — | `key_custody: local`: the sealed key file, made `0600` by `agentd --print-wallet-address <name>` when absent. The start is refused when it is a symlink, another user owns it or can read it, or it does not open to its address. |
| `security.payments.wallets.<name>.key` | string | — | `key_custody: external`: the signer's key id. |
| `security.payments.wallets.<name>.address` | address | — | The wallet's address — EVM, or base58 on Solana; a key of another address refuses the start. |
| `security.payments.assets.<name>.asset` | CAIP-19 id | required | `eip155:<chain id>/erc20:<contract>`, or `solana:<genesis hash prefix>/token:<mint>`: the token, and the network it is on. |
| `security.payments.assets.<name>.token_program` | `spl-token \| token-2022` | `spl-token` | A Solana token's program, which decides its token accounts and moves it. Refused on an EVM asset. |
| `security.payments.assets.<name>.decimals` | int, 0–36 | required | The token's decimals: `"1"` is 10^decimals atomic units. |
| `security.payments.assets.<name>.limits` | `{per_payment, per_run, per_day, per_payee}` | `per_run`, `per_day`, `per_payee` required | Caps, each a decimal in whole units: one payment, one run's payments, every payment in any 24 hours, every payment to one payee in any 24 hours. |
| `security.payments.assets.<name>.approve_above` | decimal | — | Above this amount the approver is asked. |
| `security.payments.payees.<name>.address` | address | required | The address paid (`payTo`): an EVM address — a mixed-case one is held to its EIP-55 checksum — or a Solana address in base58. |
| `security.payments.payees.<name>.assets` | list of asset names | every asset | The assets it may be paid in, each on the address's chain family. |
| `security.payments.rpc.<network>` | URL | — | By Solana network (CAIP-2), the RPC endpoint asked for a recent blockhash (`getLatestBlockhash`) when a server's entry gives none: `https://`, or `http://` to this host, its `{{secret:NAME}}` references resolved when it is dialled. A network no asset is on is refused. |

The model these keys enforce is [Security model](security.md); the checklist is
[Harden a deployment](harden.md).

### Payments

`security.payments` is the spend policy every x402 payment is held to
(the `x402` feature, which no release binary carries): the wallets agentd
pays from and who holds their keys, the assets it may pay in with their
caps, the payees it may pay, and the one person who approves a payment
above an asset's `approve_above`. A payment outside it is refused before
anything is signed. It needs `observability.audit.chain` — every payment is
a record there, the one before it is sent flushed first — and a store that
outlives the process (`file`, `mcp` or `http`), which keeps the ledger. Every
path is restart · op: a served document cannot set it, and a reload does not
change it under a question the approver is reading.

A step pays only with its own `payment` field — `{wallet, max, approve?}`
on an `http` step ([node registry](node-registry.md#http)) — which is
literal and names a wallet and assets of the policy; an instruction
document's workflow with one is refused at load, and one `workflow.create`
or `workflow.update` stored fails the step without paying. The guide is
[Pay for things with x402](pay-with-x402.md).

## vars

`vars` holds named constants, any JSON value, nestable. Live · doc.

```yaml
vars:
  region: eu-1
  api_base: "https://api.eu-1.internal"
  batch: {size: 20}

agent:
  instruction: You keep the item index in sync.

intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5-mini
  token: "{{secret:OPENAI_API_KEY}}"

workflows:
  - name: sync
    steps:
      start: {kind: manual}
      pull:
        kind: http
        depends_on: [start]
        url: "{{config.api_base}}/items?region={{config.region}}"
      each:
        kind: batch
        depends_on: [pull]
        over: "{{steps.pull.output.json}}"
        size: "{{config.batch.size}}"      # one exact token keeps the number type
        body:
          steps:
            note: {kind: noop}
      done: {kind: finish, depends_on: [each]}
```

`{{config.NAME}}` is substituted at load, before validation and before a
workflow's hash, so a definition fetched from a URL and one written inline
resolve the same way. A value that is exactly one token keeps its type;
embedded tokens become text. An undefined reference is exit 2, every miss
reported together. The namespace is `config.` because `vars.` is the run
namespace an `assign` step writes. Values are not secrets: credentials stay
`{{secret:…}}`.

## Reload classes

A running daemon reloads on `SIGHUP` (the `hot-reload` feature) or a watched
file change. The reload validates the new document with the startup pipeline,
stages every change, and applies it whole or not at all. A change to any of
these paths, by the configuration or by the instruction's own `:::!config`,
refuses the reload with `config.reload.restart_required`:

- `agent.name`, `agent.document_capabilities`, `agent.instruction.trust`
- `store.kind`, `store.prefix`, `store.mcp`, `store.http`, `store.file`,
  `store.max_value_bytes`
- `lifecycle.run_until`, `lifecycle.drain_timeout`, `lifecycle.run_id`,
  `lifecycle.exit_code_map`, `lifecycle.watch_config`
- `a2a.listen`, `a2a.tls`, `a2a.bearer`, `a2a.url`, `a2a.device_grant`,
  `a2a.login`, `a2a.events`, `a2a.resource`, `a2a.inbound_tokens`
- `spawn.uid`, `spawn.gid`, `spawn.instance_uid`, `spawn.instance_gid`
- `webhooks.listen`, `webhooks.tls`, `webhooks.url`
- `observability.otel`, `observability.metrics_addr`,
  `observability.health_file`, `observability.events_ring`,
  `observability.traceparent`, `observability.audit.chain`
- `security` (every path but `security.policies`)
- `identity.providers`, and of `identity.delegation` its keys and caps:
  `seal_key`, `seal_key_previous`, `key_custody`, `signer`, `at_cache_cap`,
  `liveness_interval`
- a per-person operator's `a2a.principals` rule, added, removed or changed,
  while a delegation is configured
- a `services` entry of `kind: company`, added, removed or changed
- an `mcp.servers` entry of `grant: id_jag` or `token_exchange`, added,
  removed or changed

Every other path is applied live, as follows:

| Area | What a reload does |
|---|---|
| `intelligence` | Each call resolves against the live settings, and a reload that moves any connection rebuilds every one. An in-flight turn follows `swap_policy`; running subagents take the new model at their next turn boundary. A token that cannot be resolved refuses the reload. |
| `intelligence.routes` | Read as each turn is launched; a turn routed to a model the reload removed runs on `roles.turn`. |
| `agent.instruction` | Re-read, through the servers the reload would run on, and its machinery folded again. A resource that cannot be read refuses the reload. |
| `agent.workspace` | Its AGENTS.md files read again; the next turn's prompt carries them. A file the load cannot take refuses the reload. |
| `mcp.servers` | Added or changed servers are dialed beside the running set; unchanged ones keep their connection; removed ones disconnect when the reload applies. A server is unchanged only when everything its dial reads is (endpoint, headers, `auth`, `oauth`, `aauth`, `service`, the service's `rate`, `timeout`), so a rotated credential re-dials it and a tags-only change does not. |
| `tools`, `knowledge`, `search`, `skills` | The tool registry is rebuilt; the skills sources are listed again and both skills folders read again. A context keeps the skill bodies it loaded of the skills the new catalogue holds; a server's skill whose source was removed, or no longer admits it, is unloaded (`skills.unloaded`). |
| `workflows` | Definitions are read and parsed before anything is dialed, then reloaded and re-armed. Live runs stay pinned to the definition they started on; a changed definition whose `schedule` node is unchanged keeps its next deadline. A definition the agent stored that names something the reload removed is left out (`workflow.stored.invalid`) and kept in the store. |
| `a2a.principals` | Recompiled and swapped into the live listener, so a demotion or a rotated `bearer_ref` applies to the next request. |
| `security.policies` | Swapped whole at the next call (`security.policies.reloaded`); a list that does not load, or whose rules could hold a tool a running turn or subagent dials itself, refuses the reload and the old one stands. A gate already open keeps the rule that raised it. |
| `identity.delegation` templates, pins, first binding, consent timing | Taken at once: a narrowed template holds every grant it allowed to the new ceilings at the next call; a removed template, a server removed from one, a changed or removed pin and first binding turned off revoke the grants they allowed — at the IdP, tombstoned, off the loop — and drop the IdP proofs of those device names' sessions (`delegation.bindings.cleared`). A template's `unattended` and `unattended_tools` hold every firing from its next call. |
| a workflow's `act_for`, and the definition it is in | A firing acts only while its workflow's digest is the one the person consented to when they last signed in: a changed definition's firings start nothing (`start.unattended.suspended`) until they sign in again. A firing already running keeps the definition it started on. |
| `a2a.cors.origins` | Replaced in the live listener; removing an origin revokes it. |
| `webhooks.default_auth` and the `webhook` routes | The route table is rebuilt on every reload, so a rotated `{{secret-file:…}}` is picked up; each route's in-flight count and rate bucket carry over. |
| `store.retention` | The next sweep applies the new bound. |

An applied reload logs `config.reloaded` with the changed groups and is
audited as `config.reload`; a refused one logs `config.reload.invalid` or
`config.reload.restart_required` and changes nothing. A reload does not watch
TLS files: the A2A and webhook listeners re-read a changed `cert`, `key` or
`client_ca` file at the next connection, with no reload, and keep the last
good identity when the new files do not load. `security.tls_ca` takes a
restart. How a reload stages and commits is
[How configuration works](configuration-model.md); the operator's view is
[Operate a running daemon](operations.md).

## What a served document may write

An instruction document's `:::!config` fragment merges under the operator's
configuration. It may write only these paths; anything else is refused at load,
naming the line:

- `workflows`, `streams`, `mcp` (but not `mcp.servers[].events` or
  `event_grants`, nor a server of `grant: id_jag` or `token_exchange` or
  any of its fields — `auth.idp`, `.subject`, `.audience`, `.resource`,
  `.client_auth`, `.tool_scopes`,
  `.scope_mode`, `.tools_from`, `.tools_file`, `.id_jag`, `.id_jag_reuse`,
  `.on_revoked`, `.allow_unbound_at`, `.attribution`, `.max_at_age`,
  `.trusted_output`, `.dpop` — on any
  server, nor a reference to a secret under `identity.*` or on a delegated
  server; nor a workflow's `act_for`), `vars`, `context`, `goal`,
  `knowledge`, `search`, `memory`, `limits`
- `intelligence.model`, `intelligence.models` (a model it declares may not name
  a `provider`), `intelligence.roles.turn`, `.think`, `.decide`, `.preflight`,
  `.compaction`, `.goal`, `intelligence.dialect`, `intelligence.budget`,
  `intelligence.timeout`, `intelligence.structured_output`,
  `intelligence.swap_policy`
- `agent.name`, `agent.approval`, `agent.ask_human_fallback`,
  `agent.ask_human_unowned`, `agent.conversation_budget`,
  `agent.max_parallel_turns`, `agent.on_workflow_finished`,
  `agent.preflight`, `agent.wake_on`
- `a2a.peers`, `tools.narrow`, `tools.disabled`
- `skills.max_bytes`, `skills.max_loaded`, `skills.reference_prefix`
- `store.kind`, `store.durability`, `store.checkpoint`, `store.on_error`,
  `store.timeout`, `store.max_value_bytes`, `store.prefix`
- `lifecycle.run_until`, `lifecycle.idle_grace`, `lifecycle.until_signal`
- `observability.log_level`, `observability.runtime_events`

An operator's turn model wins over a document's; the document's is dropped
with a warning. The directive itself is in
[Directive reference](directives-reference.md).

## Credential-bearing paths

In a config file these must be `{{secret:…}}` or `{{secret-file:…}}`
references; a literal is exit 2. From the environment or a flag they may be
inline.

| Path | Message when inline in a file |
|---|---|
| `intelligence.token` | `config file: intelligence.token carries an inline credential; …` |
| `intelligence.providers.<name>.token` | `config file: intelligence.providers.<name>.token carries an inline credential; …` |
| `a2a.bearer` | `config file: a2a.bearer carries an inline credential; …` |
| `security.aauth.enroll_token` | `config file: security.aauth.enroll_token carries an inline credential; …` |
| `security.companies.seal_key` | `config file: security.companies.seal_key carries an inline credential; …` |
| `security.payments.seal_key` | `security.payments.seal_key: a single {{secret-file:PATH}} reference — never an inline key, and never the environment` (a `{{secret:…}}` reference too) |
| `mcp.servers[].oauth.client_secret` | `config file: mcp server '<name>' oauth.client_secret carries an inline credential; …` |
| `a2a.principals[].match.bearer_ref` | `config file: a2a.principals[0].match.bearer_ref carries an inline credential; …` |
| a webhook verifier's `bearer`, `header.equals` and `hmac.secret`: `webhooks.default_auth`, and the `auth` of every `webhook` start and webhook wait | `config file: workflows[0].steps.hook.auth.hmac.secret carries an inline credential; …` |
| every `auth.token`, `auth.client_secret`, `auth.value`, `auth.client_auth.key` | `<where>: auth.token carries an inline credential; use a {{secret:…}} reference` |
| a credential-shaped header (`Authorization` and similar) in `intelligence.headers`, a provider's, an MCP server's, a peer's or `store.http.headers` | `intelligence.headers['authorization'] looks like a credential but has an inline value; …` |

Credential values never reach the logs; `--effective-config` shows them as
`"<redacted>"` and a reference as written.

## Validation diagnostics

Validation runs after every layer merges and before any side effect: no MCP
dial, no model call, no socket bound. It collects every problem in one pass,
prints one line per problem on stderr, and exits 2. `--validate-config` runs it
alone and exits 0 or 2.

```text
{"event":"config.invalid","msg":"intelligence.roles.decide: \"smal\" is not a declared model (known: fast, smart)"}
{"event":"config.invalid","msg":"intelligence.routes: agent.preflight is never, so nothing would choose a route — set preflight to auto or always, or remove the routes"}
```

| Area | Example message (exit 2) |
|---|---|
| typing | `invalid AGENTD_LIMITS_RUN_STEPS: expected an integer, got "many"` |
| unknown key | `` config file c.yaml parse error: unknown field `model`, expected `prompt` `` |
| endpoints | `intelligence endpoint must be https://host[:port][/path] (got: ftp://nope)` · `plaintext http:// intelligence is allowed for loopback only (dev); use https://` |
| models | `intelligence.roles.turn ("smart") and intelligence.model ("smart") both name the turn model; say it once` · `intelligence.models: fallback cycle fast -> smart -> fast` |
| providers | `intelligence.providers.dec.structured_output: prompt — a decisions endpoint answers decisions natively and is sent no schema; …` |
| dialect | `intelligence.dialect: bedrock requires intelligence.auth.kind = aws (SigV4)` |
| auth | `intelligence: auth.token carries an inline credential; use a {{secret:…}} reference` · `` …: auth.kind oauth2 needs `client_id` `` |
| secrets | `{{secret:LLM_KEY}} is not set in the environment (referenced at config.intelligence.token)` |
| MCP servers | `--mcp: want name=endpoint (got: nope)` · `mcp.servers[]: duplicate server name 'web'` · `mcp.servers[]: the server name 'code' is reserved for code-registered tools` |
| references | `store.mcp.server 'state' is not a declared MCP server` |
| store | `store.kind is none but the instance is long-lived (…) — configure a durable store (store.kind: file \| mcp \| http), or drop store.kind to get the local file store by default` · `` store.durability.steps: `eventual` is not implemented — … `` |
| instruction | `agent.instruction names 2 sources (text, file) — name exactly one` · `agent.instruction names no source — …` |
| workflows | `workflow "w" step "s": unknown field "every" for kind "loop" (allowed: …)` · `workflows['w'] must have exactly one of file \| uri \| url \| steps (dir is a separate entry shape)` |
| webhooks | `` a `webhook` node (start or wait) is used but webhooks.listen is not set — configure webhooks.listen (https://host:port) `` · `webhooks.listen on a non-loopback address needs auth: …` |
| A2A listener | `a2a.listen is https:// but a2a.tls.cert / a2a.tls.key are not set` · `a2a.listen on a non-loopback address needs client auth: a2a.bearer or a2a.tls.client_ca (…)` · `a2a.listen binds a wildcard host (https://0.0.0.0:8443), which is no address a caller can reach: set a2a.url …` |
| A2A origin | `a2a.url: "https://Desk.example.com:443/" is not in canonical form; write "https://desk.example.com" …` · `` a2a.cors.origins: `*` is not allowed; list each exact origin (a loopback UI origin must be listed too) `` · `a2a.events.enabled requires a2a.listen (it is served on the A2A listener)` |
| principals | `a2a.principals[0]: match needs one of san \| sub \| bearer_ref \| any` · `a2a.principals[0]: a certificate rule (san \| sub) matches only a client certificate the listener verifies — set a2a.tls.client_ca, or match a bearer with bearer_ref` |
| build features | `a2a.listen needs the a2a build feature, which this binary was built without — …` · `…: cron schedules require the 'cron' build feature` |
| lifecycle | `lifecycle.exit_code_map: only the policy codes 3 and 7 are remappable (got key "5")` · `lifecycle.watch_config requires a config file (--config / AGENTD_CONFIG)` |
| trifecta | `lethal-trifecta refused: the root grant wires untrusted_input + sensitive + egress into one agent; narrow the tags or set security.allow_trifecta (audited)` |

Warnings use the same channel as `{"event":"config.warning",…}` and do not
change the exit code. Common ones: a one-shot with no durable store, a
`memory` store, a TCP listener with no credential, a discovered config that
sets an intelligence endpoint, and the MCP Events draft notice. The full event
list is in [Events and metrics reference](telemetry-reference.md).

## A complete example

A durable triage daemon: two model connections, an A2A listener with mTLS
principals, a subscription-driven workflow and a remote store. It validates
with the two API keys set.

```yaml
# yaml-language-server: $schema=https://agentd.dev/schema/config.json
# /etc/agentd/agentd.yaml — a durable triage daemon

agent:
  name: triage
  description: Triages the support inbox and escalates risky items.
  instruction: |
    You triage incoming support items. Escalate anything that mentions
    a refund over 500 EUR or a security problem.
  preflight: auto

intelligence:
  # The top-level connection: Anthropic.
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  providers:
    openrouter:
      endpoints: https://openrouter.ai/api/alpha/decisions
      dialect: decisions
      token: "{{secret:OPENROUTER_API_KEY}}"
  models:
    smart: {model: claude-sonnet-5, fallback: fast}
    fast:  {model: claude-haiku-4-5}
    deep:  {kind: reasoning, model: claude-opus-5-5, reasoning: {effort: high}}
    jev:   {kind: decision, model: typesafe/jev-1.13, provider: openrouter, escalate: smart}
  roles:
    turn: smart
    decide: jev
    preflight: {model: jev, confidence: 0.8}
    compaction: fast
  routes:
    fast: "a status question or one simple operation"
    deep: "multi-step analysis that needs careful reasoning"
  budget:
    windows: [{per: hour, tokens: 2000000}]

mcp:
  servers:
    - {name: tickets, endpoint: "https://mcp-tickets.internal/mcp", tags: {"*": [untrusted_input]}}
    - {name: state,   endpoint: "https://mcp-state.internal/mcp"}

store:                                   # a daemon is durable
  kind: mcp
  mcp: {server: state}
  retention:
    runs: {keep_last: 1000, ttl: 30d}

a2a:
  listen: https://0.0.0.0:8443
  url: https://triage.internal:8443      # a wildcard bind says where callers reach it
  tls: {cert: /tls/cert.pem, key: /tls/key.pem, client_ca: /tls/clients.pem}
  principals:
    - {match: {san: "spiffe://example.org/ops/*"},  role: operator}
    - {match: {san: "spiffe://example.org/team/*"}, role: user, grants: ["workflow.*"]}

workflows:
  - name: watch-inbox
    steps:
      wake:   {kind: subscribe, server: tickets, uri: "tickets://inbox"}
      triage: {kind: agent, depends_on: [wake], instruction: "Triage the new item."}
      done:   {kind: finish, depends_on: [triage]}

limits:
  max_runs: 8
  run: {steps: 80, tokens: 150000, deadline: 5m}
  subagents: {depth: 3}

lifecycle: {run_until: drained, drain_timeout: 20s}

observability:
  log_level: info
  health_file: /run/agentd/health
  metrics_addr: "127.0.0.1:9090"
  audit: {sink: [log, store]}

security:
  cgroup: {spec: auto, memory_max: 2G}
```

Check it before deploying:

```bash
agentd --validate-config --config /etc/agentd/agentd.yaml
```

```text
{"event":"config.valid","files":["/etc/agentd/agentd.yaml"]}
```

Any path can be overridden at deploy time without editing the file:

```bash
AGENTD_LIMITS_RUN_STEPS=120 agentd --config /etc/agentd/agentd.yaml --log-level debug
```

To try a config shape with no key and no network, point it at the built-in
mock model. `mock:final` answers every turn at once, and
`mock:file:<playbook.json>` answers from a script. Release binaries and the
image carry it:

```bash
agentd --prompt "Say hello." --intelligence mock:final
```

## Next steps

- [How configuration works](configuration-model.md) — layers, discovery and
  the reload model behind these tables.
- [Connect model providers](connect-models.md) — the shortest working
  intelligence configuration for each vendor.
- [CLI, environment and exit codes](cli.md) — the control flags and what each
  exit code means.
- [Build features and crates](features.md) — which build carries which paths.
