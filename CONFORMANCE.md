# agentd conformance

agentd's conformance is judged by **behaviour**, against the specifications and
the documented contracts it serves — never by agreeing with its own types. The
evidence is the black-box suite in
[`crates/agentd-conformance`](crates/agentd-conformance): it knows agentd only
as a binary, drives the real process over its public surfaces, and never links
the agentd library, so a regression in what a caller sees fails a check even
when the implementation's own unit tests still agree with themselves.

This is agentd's own suite. It makes no claim about any external control
plane's contract.

## What it covers

Each check drives the real binary and asserts one contract. The checks are
grouped into families — the process's exit-code table, the security gates, the
durable store, crash-and-restore durability, the internal tool registry, the
core A2A surface, the observation feed of the events extension, the extension
contract itself, and the authentication the card declares. Every check is
registered in [`src/checks/`](crates/agentd-conformance/src/checks) as a
`Check { id, category, desc, run }`, which both the test harness and the report
runner pick up; the tables below are those registrations, and a unit test of
the suite fails when a row here differs from the check it names, or names one
that does not exist.

Each check is host-independent: conformance is judged against the
specification, never the environment, so there are no capability-gated checks to
skip.

### `supervisor`

| Check | What it proves |
|---|---|
| `supervisor/exit-0-on-success` | a completed once job exits 0 |
| `supervisor/exit-2-on-bad-flag` | an unknown flag is a usage error → exit 2 |
| `supervisor/exit-4-on-intel-down` | an unreachable intelligence endpoint → exit 4 |

### `security`

| Check | What it proves |
|---|---|
| `security/trifecta-refused` | granting one agent all three lethal-trifecta legs is refused at startup → exit 2 |
| `security/secret-not-in-telemetry` | the intelligence token never appears in the JSON-lines telemetry |

### `store`

| Check | What it proves |
|---|---|
| `store/boots-against-mcp-store` | a job backed by an MCP store connects, runs, and completes |
| `store/persists-completed-run-across-restart` | a restarted instance restores the completed run from the store and does not re-fire the once start |

### `durability`

| Check | What it proves |
|---|---|
| `durability/sigkill-restore-and-finish` | a SIGKILL before and during a step is recovered; a later life restores and completes |

### `tools`

| Check | What it proves |
|---|---|
| `tools/internal-round-trip` | internal tools (memory.set, plan.create) round-trip to the supervisor and take effect |
| `tools/unknown-tool-errors-not-crashes` | an unknown tool the model invents is answered as an error; the run still completes |
| `tools/registry-introspection` | --capabilities lists the internal tool registry |

### `a2a-conversation`

| Check | What it proves |
|---|---|
| `a2a-conversation/version-and-vocabulary` | a request whose A2A-Version is absent, empty, 0.3 or 1.1 is -32009 VERSION_NOT_SUPPORTED with its id echoed (a 1.0 patch is served); a method outside the specification's and the declared extensions' — a made-up name, the A2A 0.3 `message/send` — is -32601; neither creates a task |
| `a2a-conversation/read-commands-create-no-task` | every command the card lists as a `message` reply is answered with an agent Message or refused, never with a task — streamed, one message frame — and the task list stays empty |
| `a2a-conversation/agent-card` | the agent card is discoverable, names the configured agent, advertises streaming and a JSONRPC interface at A2A 1.0 |
| `a2a-conversation/card-declares-only-what-it-implements` | streaming, which the card advertises, streams a task to a terminal state; push notifications, which it disclaims, are refused by CreateTaskPushNotificationConfig with -32003 |
| `a2a-conversation/push-delivers-a-stream-response` | with a2a.push.enabled the card advertises push notifications, and a webhook registered with CreateTaskPushNotificationConfig is POSTed the task as a StreamResponse {task} (application/a2a+json) through to its terminal state |
| `a2a-conversation/protocol-errors-use-the-specified-codes` | an unknown method is -32601 and an unknown task is -32001, rather than a generic failure |
| `a2a-conversation/tasks-are-proto3-json-on-every-path` | SendMessage, GetTask and ListTasks all return the same proto3-JSON `Task`: state under `status`, an RFC 3339 timestamp, `ROLE_AGENT` |
| `a2a-conversation/nl-message-becomes-task-artifact` | a natural-language message runs a turn; the answer is the task artifact, readable via GetTask/ListTasks |
| `a2a-conversation/input-required-is-core` | with no extension enabled, ask_human puts the task in TASK_STATE_INPUT_REQUIRED carrying the question, and a SendMessage naming the taskId resumes the turn to completion |
| `a2a-conversation/task-history-carries-the-turn` | Task.history holds, in order, the caller's prompt under its own messageId, the agent's superseded question and the caller's answer, never the current status; historyLength 0 omits it and 1 keeps the newest |

### `events`

| Check | What it proves |
|---|---|
| `events/default-off-gate` | without a2a.events.enabled the card does not declare the events extension, its method is -32601 EXTENSION_NOT_DECLARED even to a caller activating it, and the core still answers |
| `events/feed-hello-and-replay` | with the feed on, the card declares the events extension; the feed takes only {fromSeq} (any other member is a plain -32602), then opens with a hello {seq, resume, resync, introspection, version} and replays from seq 0 a task event whose history holds the prompt |

### `extensions`

| Check | What it proves |
|---|---|
| `extensions/command-requires-activation` | a command whose extension the header does not activate is -32602 EXTENSION_NOT_ACTIVATED, one the message does not list is -32602 EXTENSION_NOT_MARKED, and neither runs nor creates a task |
| `extensions/method-requires-activation` | a method an extension declares is -32601 EXTENSION_NOT_ACTIVATED unless the header names that extension's URI exactly — naming the others or a look-alike does not — and is served once it does |
| `extensions/send-results-are-task-or-message` | every SendMessage result is exactly a task or a message, and every SendStreamingMessage frame exactly one of task, message, statusUpdate, artifactUpdate — for a conversation turn, a read command and a task command; a read streams as one message frame |
| `extensions/no-bare-agentd-metadata` | every metadata key in every reply is the URI of an extension the card declares; task annotations appear under theirs exactly when that extension is activated |

### `auth`

| Check | What it proves |
|---|---|
| `auth/card-declares-what-the-listener-enforces` | under no credential, a bearer, and a bearer beside an `any` rule, an uncredentialed call is served exactly when the card's securityRequirements admit it (none, or an empty alternative) and is otherwise a 401 challenging a declared scheme; the declared bearer is served and a wrong one is 401 |
| `auth/extended-card-needs-a-declared-credential` | on an `any`-rule daemon an uncredentialed GetExtendedAgentCard is 401 and the declared bearer reads it; on a no-auth loopback daemon the card's extendedAgentCard is not true and the method is -32004 |
| `auth/cors-exact-origins` | only a listed origin is granted (a2a-version preflighted, the challenge readable); its uncredentialed POST is a 401, never the implicit operator; an unlisted one — loopback too — is 403 with no grant; the card is public (ACAO *) |

## Running it

```sh
cargo test -p agentd-conformance             # every check, one #[test] per family
cargo run  -p agentd-conformance             # the same checks as a PASS/FAIL report
cargo run  -p agentd-conformance -- --json   # the machine-readable record
```

The suite builds the agentd binary itself, so no prior `cargo build` is needed.
CI runs it as part of `cargo test --workspace --all-features`.

## Beside the suite

Two further checks hold the A2A surface to the specification rather than to
agentd's reading of it:

- the listener's method table is compared, name by name, with the constants of
  the official `a2a-rs` SDK, so a method agentd invented or misspelled fails, and
  so does a specification method it forgot to answer;
- every task, message and card agentd emits is serialized by the types that SDK
  generates from the A2A protobuf, so the wire shape is the schema's.

## Adding a check

Append a `Check` to the relevant family's `checks()`. Its `run` function takes
`&Harness` and returns an `Outcome` (`pass` / `note` / `fail` /
`require(cond, why)`); the tests and the runner pick it up automatically, and
its row belongs in the family's table above, which the suite's own unit test
holds to the registration. A
change to a served surface — the exit codes, the A2A listener, the
configuration's admission rules, the store contract — comes with a check that
fails when the change is reverted.
