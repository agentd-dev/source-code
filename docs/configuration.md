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
| `skills/` | `skills.dir` | `<name>.md`, or `<name>/SKILL.md` |
| `subagents/` | `subagents.templates` | `*.yaml`, `*.yml`, `*.json`, one template each, named by file stem |
| `context/` | `context.templates` | `*.md`, `*.txt`, `*.hbs`, the whole body as the template, named by stem |

A folder applies only when its setting is absent (an explicit empty list
counts as set) and only when it yields at least one file. agentd looks beside
each loaded config file, the last one first, and uses the first directory that
has the folder; with no config file, the working directory. Files load in
filename order.

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
| `oauth2` | `client_id`; `token_url` or `issuer`; for `grant: device`, `device_authorization_url` or `issuer`; for `grant: client_credentials`, `client_secret` | `grant` (`device` default, `authorization_code`, `client_credentials`), `scopes`, `audience`, `authorization_url` |
| `aws` | `region`, `service` (for example `bedrock`); for `source: sso`, `sso_start_url`, `account_id`, `role_name` | `source` (`env` default, `static`, `imds`, `irsa`, `sso`) |
| `spiffe` | for `svid: jwt` (the default), `jwt_svid_file`; for `svid: x509`, `svid_file` + `key_file` | `audience` |

`token`, `value` and `client_secret` must be references in a file. Interactive
grants are completed with `agentd --login <target>`, where the target is
`intelligence`, `intelligence:<provider>`, `mcp:<name>` or `service:<name>`.
`kind: oauth2`, and any `auth` block on an MCP server, need the `oauth`
feature. The guide is [Outbound credentials](authentication.md).

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
| `mcp.servers[].auth` | object | — | A credential provider; see [Credentials](#credentials). |
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
| `services.<name>.kind` | `mcp \| intelligence \| peer \| http` | `mcp` | The surface the entry serves. Matching is by kind. |
| `services.<name>.endpoint` | URL | required | The connection URL, and the match base (scheme, authority and path prefix on segment boundaries). |
| `services.<name>.auth`, `.headers`, `.timeout` | — | — | Connection settings consumers inherit. |
| `services.<name>.tags` | map | `{}` | Authoritative tags, unioned into every server whose endpoint matches, referencing or not. |
| `services.<name>.allow` | list of globs | all | The tool ceiling; a consumer may only narrow it. |
| `services.<name>.exclude` | list of globs | `[]` | Never admitted, for every consumer. |
| `services.<name>.rate` | rate | — | Pacing toward the service, shared by every consumer in each process. |
| `services.<name>.methods` | list | any | `kind: http` only: the methods `http` steps may use. |
| `services.<name>.breaker` | `{failures, cooldown}` | — | `kind: mcp` only: the default breaker for `mcp.tool` steps against it. |

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

## tools

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `tools.disabled` | list of tool names | `[]` | live · doc | Tools never registered. |
| `tools.narrow.<tool>.tags` | list of tags | — | live · doc | Trifecta tags to add to a tool; never removes one. |
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
The data names are `instance`, `instruction`, `workflows`, `services`,
`streams`, `templates`, `skills`, `peers`, `signals`, `memory` and
`tools.internal`.

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
mentions `{{instruction}}` loads with a warning. How a turn uses it is
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
| `skills.sources[].server` | string | required | live · op | A server skills are discovered from. |
| `skills.sources[].discover` | `prompts \| resources \| auto` | `auto` | live · op | Which MCP surface carries the skills. |
| `skills.sources[].filter` | string | — | live · op | Narrow the skills discovered. |
| `skills.dir` | path | `skills/` beside the config, when present | live · op | A local folder of skill files (`<name>.md` or `<name>/SKILL.md`). A local skill wins a name collision. |
| `skills.reference_prefix` | string | `@skill:` | live · doc | How the model references a skill. |
| `skills.max_loaded` | int | `8` | live · doc | Skills loaded into context at once. |
| `skills.max_bytes` | int | `32768` | live · doc | A skill body is truncated past this. |

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

A non-loopback `webhooks.listen` needs auth on every route: a `default_auth`,
or one on each `webhook` node. Paths under `/hooks/_` are agentd's own. A route
answers as follows:

| Status | Meaning |
|---|---|
| `202` | kept: its run is in the durable inbox, or its event appended |
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

With no credential at all, a loopback listener treats a local caller that
presents nothing as the operator and warns at startup. Any credential
mechanism ends that, and a request carrying `Origin` never gets it.

### Principals

`a2a.principals[]` maps a caller's evidence to a principal. All paths are
live · op.

| Path | Type | Default | What it does |
|---|---|---|---|
| `a2a.principals[].match.san` | glob | — | Matches a verified client certificate's SANs or subject. |
| `a2a.principals[].match.sub` | string | — | Matches a verified client certificate's subject CN exactly. |
| `a2a.principals[].match.bearer_ref` | secret | — | Matches a presented bearer. May not equal `a2a.bearer`, another rule's secret, or start with `agentd_at_`. |
| `a2a.principals[].match.any` | bool | `false` | Names every caller left; cannot grant `operator`. |
| `a2a.principals[].role` | `operator \| user \| agent \| anonymous` | required | The caller's role. |
| `a2a.principals[].id` | string | derived | The principal id (`<role>:<id>`), unique across rules; `1..128` of `A-Z a-z 0-9 . _ @ : / + -`. Required on `bearer_ref` and `any` rules; a certificate rule without one is `<role>:cn=<CN>` or `<role>:san=<first SAN>`. |
| `a2a.principals[].grants` | list of op globs | — | The operations the principal may call. |
| `a2a.principals[].labels` | map | `{}` | Operator-declared attributes carried into the run, the MCP `_meta` and the audit line. |
| `a2a.principals[].quotas.rate` | rate | — | Arrival quota at the listener; operators are exempt. |
| `a2a.principals[].quotas.budget` | budget | — | A token budget charged by the work done for this principal. |

Each `match` names exactly one matcher. A `san` or `sub` rule matches only a
certificate the listener verified, so it needs `a2a.tls.client_ca` and is
refused at load without it; an `any` rule needs no CA. A `user` id may not
reuse a name ever approved through the device grant.

### Device grant

| Path | Type | Default | Class | What it does |
|---|---|---|---|---|
| `a2a.device_grant.enabled` | bool | `false` | restart · op | Serve the OAuth device authorization grant: a client shows a code, an operator approves it. |
| `a2a.device_grant.scopes` | list of `user \| operator` | `[user]` | restart · op | What a client may request. |
| `a2a.device_grant.token_ttl` | duration, 5m–30d | `8h` | restart · op | Session lifetime. |
| `a2a.device_grant.code_ttl` | duration, 1m–30m | `10m` | restart · op | How long a code waits for approval. |
| `a2a.device_grant.verification_uri` | URL | the listener's own page | restart · op | Where the approving person is sent. |
| `a2a.device_grant.rate` | rate | unlimited | restart · op | Per session principal. |

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
| `security.policies[].to` | `{role: operator}` | restart · op | Which operator may answer an `action: ask` gate: `{role: operator}` narrowed by `labels`, or the id `operator`. |

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
| `lifecycle.watch_config` | bool | `false` | restart · op | Reload when a config file, the instruction file or folder, a workflow file or folder, or a skill file changes. Needs a `--config` file and the `config-watch` feature. |

The long-lived start kinds are every kind but `once` and `manual`; they are
listed in [Workflow node reference](node-registry.md) and explained in
[Jobs, daemons and triggers](modes-and-triggers.md).

## identity

Who autonomous work is done for. All paths are live · op.

| Path | Type | Default | What it does |
|---|---|---|---|
| `identity.autonomous_as` | string | `system` | The principal a schedule, webhook, stream or `once` firing runs as. |
| `identity.labels` | map | `{}` | Labels stamped on that work. |

The acting principal and its labels reach the run record (`run.start` carries
`acting_for`), each MCP call's `_meta` (`agent/acting_for`, `agent/labels`) and
the audit line. A run is charged to the principal it is for, so
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
| `observability.runtime_events.stream` | string | required | live · doc | A declared stream that receives selected runtime events. |
| `observability.runtime_events.include` | list of families | `[]` | live · doc | Event families taken in full (the segment before the first dot). |
| `observability.runtime_events.sampled` | list of families | `[]` | live · doc | Families taken one in sixteen. |
| `observability.runtime_events.queue` | int | `512` | live · doc | Events that may queue between ticks before the tap drops and counts. |
| `observability.status_values` | list of memory keys | `[]` | live · op | Memory keys the `status` op publishes. Every caller of `status` sees them. |

The events and metrics are in
[Events and metrics reference](telemetry-reference.md); the guide is
[Monitor in production](observability.md).

## security

Every `security` path is restart · op.

| Path | Type | Default | What it does |
|---|---|---|---|
| `security.allow_trifecta` | bool | `false` | Downgrade the lethal-trifecta refusals to an audited warning. Refused from a discovered config. |
| `security.egress` | `open \| closed` | `open` | `closed` refuses any outbound destination with no matching `services:` entry. |
| `security.tls_ca` | path | the bundled roots only | Extra PEM CA certificates trusted for every outbound `https://` dial, added to the bundled roots. Needs `tls`. |
| `security.workflows.immutable` | bool | `false` | Refuse `workflow.create`, `update` and `delete` at runtime. |
| `security.policies[]` | list | `[]` | Ordered verdicts on a tool call; the first match wins, no match is allow. |
| `security.policies[].match.tool` | glob | every tool | Tool name. |
| `security.policies[].match.tags` | list of tags | — | Every listed tag must be on the tool. |
| `security.policies[].match.caller` | list of `root \| workflow \| subagent` | — | Who is calling. |
| `security.policies[].match.principal` | glob | — | The principal id, for calls that carry one. |
| `security.policies[].match.args` | CEL | — | A predicate over `args`, `tool` and `caller`. Needs `cel`. |
| `security.policies[].action` | `allow \| deny \| ask \| shadow` | required | `shadow` refuses and says the call was held. |
| `security.policies[].question` | string | — | For `ask`: the question; `{{tool}}`, `{{caller}}` and `{{args}}` are substituted. |
| `security.policies[].timeout` | duration | — | For `ask`: how long to wait. |
| `security.policies[].on_timeout` | `allow \| deny \| ask \| shadow` | `deny` | What an unanswered ask becomes. |
| `security.policies[].to` | `{role: operator, labels}` or `operator` | `{role: operator}` | Who may answer an `ask`. |
| `security.exec.enabled` | bool | `false` | Run the `exec` tool locally. Needs the `exec` feature, which no release binary carries. |
| `security.exec.allow` | list | `[]` (deny all) | Allowed command names (`argv[0]`). |
| `security.exec.workdir` | path | — | The directory commands run in; a requested `cwd` must resolve inside it. |
| `security.exec.timeout` | duration | `30s` | Wall-clock per command. |
| `security.exec.max_output` | int | `1048576` | Captured stdout and stderr bytes. |
| `security.exec.env` | list of names | `[]` | Environment variables passed through. |
| `security.cgroup.spec` | `auto` or a path | unset | A cgroup-v2 parent for children: `auto` derives `<own cgroup>/agent`. Linux only, best-effort. |
| `security.cgroup.memory_max` | `max` or a size | — | Per-child `memory.max`. |
| `security.cgroup.pids_max` | `max` or a count | — | Per-child `pids.max`; counts threads. |
| `security.aauth.provider`, `.key_file`, `.enroll_token`, `.enroll_assertion_file`, `.person_server` | — | — | The AAuth agent identity. Needs `aauth`; see [AAuth agent identity](aauth.md). |

The model these keys enforce is [Security model](security.md); the checklist is
[Harden a deployment](harden.md).

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
  `a2a.events`
- `webhooks.listen`, `webhooks.tls`, `webhooks.url`
- `observability.otel`, `observability.metrics_addr`,
  `observability.health_file`, `observability.events_ring`,
  `observability.traceparent`
- `security` (every path)

Every other path is applied live, as follows:

| Area | What a reload does |
|---|---|
| `intelligence` | Each call resolves against the live settings, and a reload that moves any connection rebuilds every one. An in-flight turn follows `swap_policy`; running subagents take the new model at their next turn boundary. A token that cannot be resolved refuses the reload. |
| `intelligence.routes` | Read as each turn is launched; a turn routed to a model the reload removed runs on `roles.turn`. |
| `agent.instruction` | Re-read, through the servers the reload would run on, and its machinery folded again. A resource that cannot be read refuses the reload. |
| `mcp.servers` | Added or changed servers are dialed beside the running set; unchanged ones keep their connection; removed ones disconnect when the reload applies. A server is unchanged only when everything its dial reads is (endpoint, headers, `auth`, `oauth`, `aauth`, `service`, the service's `rate`, `timeout`), so a rotated credential re-dials it and a tags-only change does not. |
| `tools`, `knowledge`, `search`, `skills` | The tool registry is rebuilt and the skills catalogue discovered again. |
| `workflows` | Definitions are read and parsed before anything is dialed, then reloaded and re-armed. Live runs stay pinned to the definition they started on; a changed definition whose `schedule` node is unchanged keeps its next deadline. A definition the agent stored that names something the reload removed is left out (`workflow.stored.invalid`) and kept in the store. |
| `a2a.principals` | Recompiled and swapped into the live listener, so a demotion or a rotated `bearer_ref` applies to the next request. |
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
  `event_grants`), `vars`, `context`, `goal`, `knowledge`, `search`, `memory`,
  `limits`
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
| `mcp.servers[].oauth.client_secret` | `config file: mcp server '<name>' oauth.client_secret carries an inline credential; …` |
| `a2a.principals[].match.bearer_ref` | `config file: a2a.principals[0].match.bearer_ref carries an inline credential; …` |
| a webhook verifier's `bearer`, `header.equals` and `hmac.secret`: `webhooks.default_auth`, and the `auth` of every `webhook` start and webhook wait | `config file: workflows[0].steps.hook.auth.hmac.secret carries an inline credential; …` |
| every `auth.token`, `auth.client_secret`, `auth.value` | `<where>: auth.token carries an inline credential; use a {{secret:…}} reference` |
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
