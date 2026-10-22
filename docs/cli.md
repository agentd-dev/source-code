# CLI, environment and exit codes

This page lists everything the `agentd` binary accepts on its command line
and from its environment, and every exit code it can end with. It is for
operators writing unit files and manifests, and for integrators scripting
around the binary. The values here match `agentd --help`; the per-setting
keys live in the [Configuration reference](configuration.md).

## Synopsis

```text
agentd --config <settings.yaml> [--config <overlay.yaml> …] [--<path> <value> …]
agentd --prompt <TEXT> --intelligence <URL> --model <NAME>
agentd --instruction <TEXT> --intelligence <URL> --model <NAME> [--mcp name=<endpoint> …]
agentd tui|ui [launcher flags] --config <settings.yaml> [--<path> <value> …]
agentd <ask>
```

- The first form runs whatever the configuration describes: a one-shot job
  or a long-lived daemon. See [Jobs, daemons and triggers](modes-and-triggers.md).
- `--prompt` delivers one task as a message to the agent's root context, with
  its full tool surface, and exits when the instance has nothing long-lived
  armed. The turn's outcome is the exit code, as a run's is: `4` when the
  model cannot be reached, `7` when a budget refuses or ends the turn, `124`
  past the deadline.
- `--instruction` without a workflow or a prompt is the one-shot sugar:
  agentd generates a `once → agent → finish` workflow named `main`, and the
  process exits when that run ends, with its code. A daemon (an A2A
  listener, or `lifecycle.run_until: drained`) gets no such workflow: there
  the instruction is the policy for the messages it takes, and it runs
  nothing at startup unless it has `--prompt` or a `once` start.
- `tui` and `ui` run the daemon plus a display client. See
  [Subcommands](#subcommands).
- An *ask* prints something and exits without starting the runtime. See
  [Asks](#asks).

A flag takes its value as the next argument (`--drain-timeout 25s`). Only
`-c`/`--config` and `--env` also accept the `=` form; `--drain-timeout=25s`
is `unknown argument: --drain-timeout`, exit 2. In a container `args:` list,
write the flag and its value as two entries.

## Subcommands

`agentd tui` and `agentd ui` are a launcher. They run the daemon exactly as
`agentd <args>` would, start one display client beside it, sign the client in
with a single-use launch code, and tie the two lifetimes together. They add
no setting: the live feed needs `a2a.events.enabled` and the debug panes
`a2a.introspection.enabled` in your own configuration.

| Flag | Subcommand | Meaning |
|---|---|---|
| `--daemon-log <PATH>` | `tui`, `ui` | Where the daemon's JSON-lines output goes while the client holds the terminal. Default `$XDG_RUNTIME_DIR/agentd-<sub>-<pid>.log`, else the same name in the temp directory. |
| `--port <N>` | `ui` | Loopback port the web UI is served on. Default `4173`. |
| `--no-open` | `ui` | Print the sign-in URL instead of opening a browser. |

Every other argument goes to the daemon. The launcher needs a loopback
`http(s)` listener with a fixed port and no `a2a.tls.client_ca`, and refuses
anything else before it spawns a process (exit 2). The client binary is
`agentd-tui` or `agentd-ui` on `PATH`, or the path in `AGENTD_TUI_BIN` /
`AGENTD_UI_BIN`. A build without the `a2a` feature has no launcher and exits 2.
[Use the terminal and web UI](interface.md) covers the clients.

## Asks

An ask answers a question and exits. None of them binds a socket or calls a
model, and only `--login`, `--logout` and the `--delegation-*` commands dial
anything: the authorization server's metadata, token and revocation
endpoints — or, for a `company:<name>` or `idp:<name>` target and the
delegation commands, the running daemon's unix A2A listener. Every ask reads
the configuration files you name, so a file that does not parse is exit 2 even
for `--help`. Only `--validate-config`, `--capabilities`,
`--effective-config`, `--print-client-metadata`, `--print-company-client`, `--print-wallet-address`
and the `--delegation-*` commands also run validation.

| Flag | Prints | Stream | Exit |
|---|---|---|---|
| `-h`, `--help` | Usage, the named flags, and the `CONFIG PATHS` table | stdout | 0 |
| `-V`, `--version` | `agentd <version>` | stdout | 0 |
| `--validate-config` | One `config.valid` line, or one `config.invalid` line per problem, plus `config.warning` lines | stderr | 0 valid, 2 invalid |
| `--effective-config` | The assembled document, the layer each setting came from, and the instruction's `:::!config` fragment. On an invalid configuration it still prints, and reports the errors on stderr. Credential-shaped values are `"<redacted>"`. | stdout (errors on stderr) | 0 |
| `--config-schema` | The settings JSON Schema (Draft 2020-12) | stdout | 0 |
| `--workflow-schema` | The workflow JSON Schema and node registry | stdout | 0 |
| `--context-template` | The built-in system-prompt template | stdout | 0 |
| `--capabilities` | The capabilities manifest: what this binary and configuration serve | stdout | 0, or 2 on an invalid configuration |
| `--extensions` | Every A2A extension and binding URI agentd publishes, with its spec and schema location | stdout | 0 |
| `--extension-schema <name>` | The schema bundle of one extension (`command`, `events`, `task-annotations`) | stdout | 0, or 2 for an unknown name |
| `--login <target>` | Runs an interactive OAuth device login and caches the token. Targets: `intelligence`, `intelligence:<provider>`, `mcp:<name>`, `service:<name>`. Needs the `oauth` feature. | stdout | 0, or 2 on failure |
| `--logout <target>` | Evicts a cached credential, then revokes it at its authorization server when the server's metadata lists a revocation endpoint (RFC 7009; needs the `oauth` feature). A revocation that fails exits 2, the credential evicted. | stdout | 0, or 2 on failure |
| `--login company:<name>[/<label>]` | Asks the running daemon, over its unix A2A listener, to have the User sign in at the Company `<name>` — to the account agentd calls `<label>` (`[a-z0-9_-]{1,32}`), or, without one, the only account held there, else `default` ([Several accounts](act-at-companies.md#several-accounts)) — then prints what the daemon says: where to sign in and the code to check, then how it ended. The label rides in the target after a `/`, which neither an entry's name nor a label holds, so no flag of its own is added. The sign-in happens in the daemon; this process holds no code or token. Needs `a2a.listen` a unix socket, `security.companies.user: operator` (any User can ask the agent instead, whose `company.sign_in` tool opens the same gate, and one who is an operator principal can send the [`company.sign_in` command op](a2a-reference.md#command-ops) from their own A2A client; the TUI and web UI have no company sign-in command), and the `companies` and `a2a` features. | stdout | 0 signed in, 1 ended otherwise or no daemon answered, 2 on a configuration that cannot reach it or a label that is not one |
| `--logout company:<name>[/<label>]` | Asks the running daemon, the same way, to sign the User out of one account at `<name>` — the one `<label>` names, or the only one held: its Account Token is revoked there and forgotten, and every other account stays signed in. With several held and no label it signs out of none. | stdout | 0 signed out or nobody signed in, 1 when the revocation failed, several accounts are held and none is named, or no daemon answered, 2 as for `--login` |
| `--login idp:<name>[/<grant>]` | Asks the running daemon, over its unix A2A listener, to sign the per-person operator in at the IdP `<name>` and keep their delegation grant ([Enterprise-managed authorization](authentication.md#enterprise-managed-authorization)) — by `device` or `authorization_code` after a `/`, else the provider's first that its IdP supports — then prints where to sign in and how it ended. The sign-in happens in the daemon; this process holds no code or token. A sign-in whose `iss` and `sub` are not the operator's `idp_subject` pin keeps nothing. Run again, it renews the grant — what a step-up asks for — while the grant is the same identity, unexpired, under the template and ceilings configured now, and otherwise replaces it, revoking the refresh token it replaces at the IdP: how an expired grant, or one enrolled before the pin changed, is renewed. Needs `a2a.listen` a unix socket, an `a2a.principals` rule with `match.uid` and `idp_subject`, and the `xaa` and `a2a` features. | stdout | 0 enrolled, 1 ended otherwise or no daemon answered, 2 on a configuration that cannot reach it |
| `--delegation-list` | Asks the running daemon for its delegation grants — never a secret — and the open consent gates, as JSON; a gate's arguments, the text agentd renders of it, its `nonce` and `args_hash` only to the person who answers it. Needs the `xaa` and `a2a` features. | stdout | 0, 1 when no daemon answered, 2 as for `--login idp:` |
| `--delegation-approve <gate>`, `--delegation-deny <gate>` | The per-person operator's answer to one consent gate: reads the gate's nonce and argument hash from `--delegation-list`, prints the gate as agentd rendered it — its own lines, and what the work chose quoted and escaped — and sends the answer bound to them. An approved call runs; a denied one is never made. No other operator can answer, and no message on the task is an answer. Needs the `xaa` and `a2a` features. | stdout | 0, 1 when the daemon refused the answer or no gate is open by that name, 2 as for `--login idp:` |
| `--delegation-revoke <principal \| idp:<name>#<sub>>` | Revokes the grants the target names — one principal's, or every grant of one IdP subject (offboarding): RFC 7009 at the IdP, retried, then the tombstone, the sealed record and every token. Prints, per grant, whether the IdP acknowledged. Needs the `xaa` and `a2a` features. | stdout | 0, 1 when no grant matches or no daemon answered |
| `--delegation-forget <principal>` | Erases a person ([Enterprise-managed authorization](authentication.md#enterprise-managed-authorization)): every consent gate open for them denied, their grants revoked as `--delegation-revoke` revokes them, then their personal key destroyed, so what it sealed in the audit chain — the arguments their consent gates showed — is unreadable from then on, while the chain still verifies. Needs the `xaa` and `a2a` features. | stdout | 0, 1 when no daemon answered or the erasure failed |
| `--delegation-audit` | Prints the caller's own consent records from the audit chain as JSON, the latest first, each with what its gate showed opened under their personal key — `unreadable` once they were forgotten. Needs `observability.audit.chain`, and the `xaa` and `a2a` features. | stdout | 0, 1 when no daemon answered, 2 as for `--login idp:` |
| `--delegation-lint` | Prints, from the configuration alone, the Resource connections each IdP must have for its templates to work — exactly those — and what would refuse or narrow a delegated call. Dials nothing. Needs the `xaa` feature. | stdout | 0, or 2 when a finding refuses delegated calls |
| `--print-client-metadata <dir>` | Writes `client.json` (agentd's [Client ID Metadata Document](authentication.md#client-id-metadata-document)) and `jwks.json` (its key set, public halves only) for `security.client` into `<dir>`, for static hosting at the `client_id` and `jwks_uri`, and prints their paths. Creates the signing key `0600` when it is absent. Dials nothing. Needs the `oauth` feature. | stdout | 0, or 2 on an invalid configuration |
| `--print-company-client <dir>` | Writes `client.json` (the client metadata document) and `jwks.json` (its key set, public halves only) for `security.companies.client` into `<dir>`, for static hosting at the `client_id` and `jwks_uri`, and prints their paths. Creates the signing key `0600` when it is absent. Dials nothing. Needs the `companies` feature. | stdout | 0, or 2 on an invalid configuration |
| `--print-wallet-address <wallet>` | Prints the address of the `security.payments` wallet `<wallet>`, the address to fund: an EVM address, or a Solana wallet's base58 key. Under `key_custody: local` it makes the wallet's sealed key file `0600` when it is absent, from the kernel's random source, sealed under `security.payments.seal_key`; under `external` it asks the signer for the key's address. Dials nothing else. Needs the `x402` feature ([Pay for things with x402](pay-with-x402.md)). | stdout | 0, or 2 on an invalid configuration or a key file that does not open |

> **Note.** `--validate-config` writes its verdict to **stderr**. A CI step
> that captures stdout to check validity sees nothing; branch on the exit
> code instead.

`--validate-config` resolves every `{{secret:NAME}}` reference the document
uses. In CI, set each referenced variable to a placeholder value.

`--context-template` is an ask, so the setting `context.template` is reached
from the command line as `--context.template <value>`.

## Process flags

These flags shape one invocation. They have no configuration path, so no
file or environment variable can pin them.

| Flag | Meaning |
|---|---|
| `-c`, `--config <PATH>` | Load a YAML or JSON configuration file. Repeatable; later files override earlier ones key by key, and a list replaces the list under it. Also read from `AGENTD_CONFIG` (`a.yaml:b.yaml`), whose files load before any `--config`. |
| `--env <FILE>` | Load a dotenv file into this process's environment before anything reads it. Repeatable; later files win, and the real environment always wins over a file. `KEY=VALUE`, an `export` prefix, `#` comments, `'…'` literals and `"…"` with escapes. A malformed line is exit 2 naming the file and line. |
| `--fresh` | Start a new durable-store generation instead of resuming. The previous generation stays on the store. |
| `--prompt-missing` | Ask on `/dev/tty`, with echo off, for each `{{secret:NAME}}` the startup check finds missing. Values stay in memory; a restart asks again. Refused without a controlling terminal. |

## Named flags

The named flags are short spellings for the settings an operator reaches for
most. Each sets one configuration path. The **Env** column is the short
environment name; the derived name (`AGENTD_<PATH>`) works too and wins over
the short one when both are set.

| Flag | Path | Env | Default |
|---|---|---|---|
| `--instruction <VALUE>` | `agent.instruction` | `AGENTD_INSTRUCTION` | none |
| `--instruction.text <TEXT>` | `agent.instruction.text` | | |
| `--instruction.file <PATH>` | `agent.instruction.file` | | |
| `--instruction.url <URL>` | `agent.instruction.url` | | |
| `--instruction.oci <REF>` | `agent.instruction.oci` | | |
| `--instruction.mcp <URI>` | `agent.instruction.mcp` | | |
| `--instruction.dir <DIR>` | `agent.instruction.dir` | | |
| `--instruction.glob <GLOBS>` | `agent.instruction.dir.glob` | | |
| `--instruction.order <name\|date>` | `agent.instruction.dir.order` | | |
| `--instruction.refresh <auto\|off\|DUR>` | `agent.instruction.refresh` | | `auto` |
| `--instruction.unavailable <POLICY>` | `agent.instruction.unavailable` | | `auto` |
| `--prompt <VALUE>` | `agent.prompt` | `AGENTD_PROMPT` | none |
| `--prompt.text`, `.file`, `.url`, `.oci`, `.dir` | `agent.prompt.*` | | |
| `--prompt.glob`, `--prompt.order` | `agent.prompt.dir.glob`, `.order` | | |
| `--intelligence <LIST>` | `intelligence.endpoints` | `AGENTD_INTELLIGENCE` | none |
| `--intelligence-token <T>` | `intelligence.token` | `AGENTD_INTELLIGENCE_TOKEN` | none |
| `--intelligence-token-file <PATH>` | `intelligence.token_file` | `AGENTD_INTELLIGENCE_TOKEN_FILE` | none |
| `--model <NAME>` | `intelligence.model` | `AGENTD_MODEL` | none |
| `--model-swap <POLICY>` | `intelligence.swap_policy` | `AGENTD_MODEL_SWAP` | `finish-on-old` |
| `--budget-tokens-lifetime <N>` | `intelligence.budget.lifetime_tokens` | `AGENTD_BUDGET_TOKENS` | `0` (unbounded) |
| `--mcp name=<endpoint>` | `mcp.servers` (adds one) | | |
| `--mcp-tags name=tag,tag` | `mcp.servers[].tags` | | |
| `--a2a-peer name=<endpoint>` | `a2a.peers` (adds one) | | |
| `--workflow <FILE>` | `workflows` (adds one) | | |
| `--max-steps <N>` | `limits.run.steps` | `AGENTD_MAX_STEPS` | `500` |
| `--max-tokens <N>` | `limits.run.tokens` | `AGENTD_MAX_TOKENS` | `2000000` |
| `--deadline <DUR>` | `limits.run.deadline` | `AGENTD_DEADLINE` | `3600s` |
| `--max-depth <N>` | `limits.subagents.depth` | | `3` |
| `--run-id <ID>` | `lifecycle.run_id` | `AGENTD_RUN_ID` | a new ULID |
| `--drain-timeout <DUR>` | `lifecycle.drain_timeout` | `AGENTD_DRAIN_TIMEOUT` | `25s` |
| `--watch-config` | `lifecycle.watch_config` | `AGENTD_WATCH_CONFIG` | `false` |
| `--budget-exit-code <N>` | `lifecycle.exit_code_map` (both `3` and `7`) | | none |
| `--listen <TARGET>` | `a2a.listen` | `AGENTD_A2A_LISTEN` | off |
| `--log-level <LEVEL>` | `observability.log_level` | `AGENTD_LOG_LEVEL` | `info` |
| `--log-content` | `observability.log_content` | `AGENTD_LOG_CONTENT` | `false` |
| `--metrics-addr <ADDR>` | `observability.metrics_addr` | `AGENTD_METRICS_ADDR` | off |
| `--health-file <PATH>` | `observability.health_file` | | off |
| `--events-ring <N>` | `observability.events_ring` | | `1024` |
| `--traceparent <W3C>` | `observability.traceparent` | `AGENTD_TRACEPARENT` | a random trace |
| `--allow-trifecta` | `security.allow_trifecta` | `AGENTD_ALLOW_TRIFECTA` | `false` |
| `--tls-ca <PATH>` | `security.tls_ca` | `AGENTD_TLS_CA` | bundled roots only |
| `--aauth-provider`, `--aauth-key-file`, `--aauth-enroll-token`, `--aauth-enroll-assertion-file`, `--aauth-person-server` | `security.aauth.*` | | |
| `--cgroup <auto\|PATH>` | `security.cgroup.spec` | | off |
| `--cgroup-memory-max <SIZE>` | `security.cgroup.memory_max` | | |
| `--cgroup-pids-max <N>` | `security.cgroup.pids_max` | | |

Notes on a few of them:

- `--model` names the turn model: a name declared under
  `intelligence.models`, or a literal model string. It is the same setting
  as `intelligence.roles.turn`; setting both is refused.
- A connection that is not the built-in `mock:` endpoint must name a model.
  A configuration that leaves it out is refused at load (exit 2).
- `--intelligence` takes a comma-separated failover list. An OpenAI-compatible
  endpoint is written in full, for example
  `https://api.openai.com/v1/chat/completions`: a URL with a path is posted to
  as written.
- `AGENTD_INTELLIGENCE_TOKEN_<N>` and `AGENTD_INTELLIGENCE_TOKEN_<N>_FILE`
  give endpoint *N* of the top-level list its own credential (endpoint 1 uses
  the bare names). They never apply to `intelligence.providers`.
- `--instruction` reads its value as text, a file path, a folder or a URI.
  An `oci://` URI pulls an OCI artifact; any other URI names an MCP resource,
  including an `https://` one. To fetch a document over HTTPS, use
  `--instruction.url`. [Write and ship instruction
  documents](instruction-documents.md) covers the sources.
- `--budget-exit-code N` (0–255) remaps the policy codes 3 and 7 to `N`, for
  example `0` so a budget stop is not a pod failure. It touches nothing else.
- `--deadline` bounds the whole run, including a model call in flight. A
  one-shot run takes it as its run deadline.

## Every setting as a flag and an environment variable

Every path in the configuration schema is also a flag and an environment
variable, derived from the path. `agentd --help` prints the full table under
`CONFIG PATHS`, and the [Configuration reference](configuration.md) lists
each path with its type and default.

| Source | Name for the path `limits.run.steps` |
|---|---|
| file | `limits: { run: { steps: 5 } }` |
| environment | `AGENTD_LIMITS_RUN_STEPS` |
| flag | `--limits.run.steps 5`, `--limits.run-steps 5` or `--limits-run-steps 5` |

The rules:

- Values are typed by the schema. Integers, numbers and booleans parse; an
  enum is checked against its set; a list takes `[a, b]` or `a, b`; an object
  takes a `{k: v}` or JSON literal. A value that does not type is exit 2
  naming its source:
  `invalid AGENTD_LIMITS_RUN_STEPS: expected an integer, got "many"`.
- A flag or variable **sets** its path. For a list or map that means it
  replaces what the files declared. The repeatable named flags `--mcp`,
  `--a2a-peer` and `--workflow` **add** one entry instead.
- A dotted flag reaches into a free-form map and keeps the key's spelling:
  `--intelligence.headers.x-team ops` sets one header.
- Array elements cannot be addressed: `--mcp.servers.0.name` is refused. Set
  the whole list, or use the repeatable flag.
- An unknown flag is `unknown argument: <flag>`, exit 2. An `AGENTD_`
  variable that names no path is not read.
- A boolean flag given alone means `true`: `--a2a.events.enabled`.

## Precedence

Layers apply key by key, lowest first:

```text
built-in default  <  config files (in order)  <  environment  <  flags
```

Inside the environment layer, a derived name (`AGENTD_INTELLIGENCE_MODEL`)
wins over a short name (`AGENTD_MODEL`) for the same path. Flags apply in
argument order. `${VAR}` and `${VAR:-default}` in any string of a file expand
from the process environment; an unset variable with no default is exit 2.
[How configuration works](configuration-model.md) explains the layers, the
discovery chain and secret references.

## Environment variables

Besides `AGENTD_<PATH>` and the short names above, agentd reads these
variables.

| Variable | Read for |
|---|---|
| `AGENTD_CONFIG` | Configuration files, `:`-separated, loaded before any `--config` |
| `AGENTD_INTELLIGENCE_TOKEN_<N>`, `…_<N>_FILE` | The credential for endpoint *N* of `intelligence.endpoints` |
| `AGENTD_STATE_DIR` | The file store's directory when `store.file.path` is unset |
| `XDG_STATE_HOME` | Fallback base: `$XDG_STATE_HOME/agentd/state` and `…/agentd/creds` |
| `HOME` | Last fallback base: `$HOME/.local/state/agentd/state` and `…/creds` |
| `AGENTD_CRED_DIR` | The OAuth credential cache directory |
| `AGENTD_POD_NAME` | The instance name when `agent.name` is unset (set it from the downward API) |
| `HOSTNAME` | The instance name when neither `agent.name` nor `AGENTD_POD_NAME` is set; the last fallback is `agentd` |
| `AGENTD_POD_UID`, `AGENTD_POD_NAMESPACE`, `AGENTD_NODE_NAME` | Descriptive pod identity, never required |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The OTLP collector when `observability.otel.endpoint` is unset |
| `XDG_RUNTIME_DIR` | Where `tui`/`ui` put the daemon log by default, and, when it is a directory the user owns with mode 0700, where a parent keeps its non-durable instance children |
| `TMPDIR` | The temporary directory: a parent's non-durable instance children when `XDG_RUNTIME_DIR` is not the user's own, and an instance child's socket when its own directory is too deep for one |
| `AGENTD_TUI_BIN`, `AGENTD_UI_BIN` | The display client binary the launcher starts |
| `DOCKER_CONFIG` | Registry credentials for `oci://` instruction pulls |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE` | AWS SigV4 credentials for an endpoint whose `auth.source` reads them |

A `{{secret:NAME}}` reference in the configuration reads the variable
`NAME`, and `{{secret-file:PATH}}` reads a file. See
[Outbound credentials](authentication.md).

## Durations

Every duration-typed value, on the command line or in a file, is a number
with an optional unit: `ms`, `s`, `m`, `h`, `d` or `w`. A bare number means
seconds.

| Input | Meaning |
|---|---|
| `250ms` | 250 milliseconds |
| `30` | 30 seconds |
| `5m` | 5 minutes |
| `2h` | 2 hours |
| `1d` | 1 day |
| `2w` | 2 weeks |

An unknown unit is exit 2: `unknown duration unit 'x' in 2x`.

## Signals

| Signal | Effect |
|---|---|
| `SIGTERM`, `SIGINT` | Begin a graceful drain: stop admitting work, let what runs finish, checkpoint, exit. Bounded by `lifecycle.drain_timeout`; what still runs then is stopped and left for the next start, not failed. A daemon exits 0; a job exits with its work's code. |
| A second `SIGTERM` or `SIGINT` | Force the drain: the child process groups are killed at once, and their work is left for the next start. |
| `SIGHUP` | Reload the configuration files, validate first, apply the reloadable settings. Needs the `hot-reload` feature; without it `SIGHUP` keeps its default action and ends the process. |

[Operate a running daemon](operations.md) covers drain and reload in
practice.

## Exit codes

The exit code is a stable contract: a scheduler can branch on it without
reading a log. A code's meaning never changes; a new outcome gets a new code.
The *intent* column is the retry decision a control plane compiles each code
into.

| Code | Meaning | Intent |
|---|---|---|
| `0` | Success. A job finished `completed`, or a daemon drained cleanly after `SIGTERM`, `SIGINT`, `admin.drain` or a `drain` policy. | complete |
| `1` | Generic failure. A run failed for a reason with no code of its own, or was cancelled. | retriable |
| `2` | Configuration or usage error. Validation failed, a flag is unknown, a `file`, `url` or `oci` instruction could not be read at load, TLS material is unreadable, a listener could not bind, or the tool registry or workflow set does not build. Retrying reproduces it. | terminal |
| `3` | Partial result. The run stalled, a loop was detected, the result was flagged partial, or the job ended with a run unfinished — a job a drain cut short among them. | policy |
| `4` | Model unreachable. Every model endpoint failed, or refused its credentials, after retries, or the credential a model connection's `auth` presents could not be produced (no login, a source that does not answer), so its calls were not sent. | retriable |
| `5` | Refused. The task cannot be done, or was refused. | terminal |
| `6` | A required MCP server is down: something agentd cannot run without failed. The durable store did not open or restore, or the MCP resource the instruction names could not be read at startup. Also the instruction source becoming unreachable under `agent.instruction.unavailable: exit`. | retriable |
| `7` | A budget was exhausted: the run's steps or tokens, a subagent-tree budget, or the instance's lifetime token budget under `lifetime_exhausted: exit`. | policy |
| `124` | The run's deadline passed (`--deadline`, `limits.run.deadline`). The number echoes `timeout(1)`. | policy |
| `137` | Killed by the OS with `SIGKILL` (128 + 9), often on out-of-memory. | infra |
| `143` | Killed by the OS with `SIGTERM` (128 + 15) before a clean drain finished. | infra |

How to read them:

- **A clean drain is 0, not 143.** agentd never exits 137 or 143 itself;
  the kernel sets them when it kills the process. A rule keyed on 143 to mean
  "terminated" never fires on a graceful shutdown.
- **Intents.** *terminal* means a retry never helps (`FailJob`). *retriable*
  is usually transient (leave it to `backoffLimit`). *policy* is yours to
  decide. *infra* is a resource or grace-period fix, never a retry rule.
- **An unrecognised code is retriable.** A reader that meets a code it does
  not know backs off rather than giving up.
- **The remap is narrow.** `--budget-exit-code` and `lifecycle.exit_code_map`
  change only 3 and 7. `124` and every other code pass through unchanged. The
  durable run record keeps the canonical code either way.
- **Only a required MCP server is fatal.** A tool server that is down at
  boot is not: agentd logs `mcp.connect.fail`, runs without that server's
  tools, and adds them when the server comes up (`mcp.joined`). Exit 6 is for
  the store and the instruction source, which agentd cannot run safely
  without.
- **A daemon exits 0 on a drain** even when individual runs failed, or the
  drain stopped work it resumes next start; their outcomes are in
  `run.done` and in the run records. Only a job's exit code reflects its
  run.
- **A job a drain cut short does not exit 0.** A drain ends a job with the
  code an idle exit would, so a run it left unfinished reads 3, and so
  does a turn or subagent it stopped or work it never let start; a run
  that had already failed keeps its own code. A Job controller must not
  count work that did not happen as a success. A job keeps no state by
  default, so its next run starts the work again.

A one-shot job's code comes from the run the job started. When a job starts
several runs, a run that did not succeed decides the code, so a partial
success never reads as clean.

## Next steps

- [Deploy](deployment.md): turn the exit codes into a `podFailurePolicy`.
- [Operate a running daemon](operations.md): drain, pause, reload.
- [Configuration reference](configuration.md): every path, type and default.
- [Events and metrics reference](telemetry-reference.md): what the process
  writes while it runs.
