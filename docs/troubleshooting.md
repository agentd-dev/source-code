# Troubleshooting

When agentd refuses to start, a run fails or stalls, or a client cannot
connect, the cause is almost always written down: in the validation
verdict, the exit code, or a telemetry line on stderr. This guide is for
operators and builders chasing such a problem. It shows where to look first
and what the common messages mean, for configurations, workflow runs, MCP
servers, model endpoints and the browser UI.

## Validate first

Run validation before anything else. It reports every error at once, calls
no model, dials no MCP server and spends no token:

```sh
agentd --validate-config --config app.yaml
```

The verdict is written to **stderr**, one JSON line per problem, and the
exit code is `2` when anything is wrong:

```
{"event":"config.invalid","msg":"a2a.listen is https:// but a2a.tls.cert / a2a.tls.key are not set"}
{"event":"config.invalid","msg":"config file: intelligence.token carries an inline credential; use {{secret:NAME}} / {{secret-file:PATH}} (or set it from env/flag)"}
```

A normal start runs the same checks but stops at the first error. A script
that checks validity should read stderr and the exit code, not stdout.

When a value is not what you expect, print the assembled configuration and
where each setting came from (credentials redacted):

```sh
agentd --effective-config --config app.yaml
```

Values combine as built-in < files < environment < flags, so a forgotten
`AGENTD_*` variable or flag can override the file you are editing. The
effective configuration shows which layer set each value.

## Common configuration errors

| Message (abridged) | Fix |
|---|---|
| `parse error: unknown field \`url\`, expected one of \`name\`, \`endpoint\`, …` | a misspelt key; the message names the file and lists the keys that object takes |
| `{{secret:NAME}} is not set in the environment` | export the variable, or use `--env <file>` |
| `… carries an inline credential; use {{secret:NAME}}` | a credential written in a file; replace it with a secret reference |
| `store.kind is none but the instance is long-lived` | a listener or long-lived trigger needs a durable store: set `store.kind`, or remove `store.kind: none` to get the local file store |
| `a2a.listen is https:// but a2a.tls.cert / a2a.tls.key are not set` | add the certificate and key, or listen on loopback `http://` |
| `a2a.listen on a non-loopback address needs client auth` | set `a2a.bearer` or `a2a.tls.client_ca` |
| `a2a.listen binds a wildcard host … set a2a.url` | set `a2a.url` to the origin callers use |
| `a2a.cors.origins: "…/": an origin has no path` | write origins as `scheme://host[:port]`, no trailing slash |
| `a \`webhook\` node … is used but webhooks.listen is not set` | configure `webhooks.listen` |

Validation also refuses:

- a real (non-mock) model connection with no model named for the agent's
  turns: set `intelligence.model` (`--model`) or `intelligence.roles.turn`;
- a model connection's `auth` that cannot send a bearer: a static
  credential under its own header (use `intelligence.headers` instead), or
  a SPIFFE x509 SVID;
- an A2A principal rule that matches a client certificate (`san`, `sub`)
  without `a2a.tls.client_ca`; an `any` rule needs none;
- `a2a.listen`, `webhooks.listen` or `a2a.peers` in a binary built without
  the `a2a` feature.

Workflow errors, such as `unknown field "prompt" for kind "agent"` or `a
non-start step must depend on something`, are listed with their causes in
the [workflow language reference](workflow-language.md#load-time-errors).

### Errors that only startup finds

`--validate-config` checks the document, not the world. A few checks need
the live registry or the network and run when the process starts:

| At startup | Exit | Meaning |
|---|---|---|
| `tool "x" is unknown, disabled or not granted to workflows` | 2 | a `tool` step names a tool workflows cannot call |
| `mcp server "x" is not connected` | 2 | an `mcp.tool` step names a server that is not declared, or was down when the process started |
| `stream "x" is not declared under \`streams:\`` | 2 | a `stream` start names an undeclared stream |
| the durable store does not connect | 6 | an MCP or HTTP store is unreachable |

## Read the exit code

A one-shot job's exit code says how it ended without parsing a log. The
common codes:

| Code | Usually means | Look at |
|---|---|---|
| 2 | configuration or usage error | the `config.invalid` lines |
| 3 | partial: a run stalled | `run.stalled` and the step that never became ready |
| 4 | the model was unreachable, or refused the credential, after retries | the `run.done` error, `intel` in its text |
| 6 | a required MCP server is down, or the durable store did not open | `mcp.connect.fail`, `proc.exit` |
| 7 | a budget was exhausted: steps or tokens | the run's limits |
| 124 | the run's deadline passed | `limits.deadline`, `--deadline` |
| 1 | anything else, such as a failed step | the `run.done` error |

[CLI, environment and exit codes](cli.md) has the full contract.

## Debug a run that failed or stalled

Telemetry is JSON lines on stderr; the run's output is on stdout. Capture
stderr and filter it with `jq`:

```sh
agentd --config app.yaml 2> telemetry.ndjson
jq -c 'select(.event=="run.done") | {workflow, status, err}' telemetry.ndjson
jq -c 'select(.event=="step.done" and .status!="done") | {run, step, status, err}' telemetry.ndjson
jq -c 'select(.level=="warn" or .level=="error") | {event, err, error, msg}' telemetry.ndjson
```

`run.done` carries the error that ended the run, and it names the step:

```
{"workflow":"hello","status":"failed","err":"step \"greet\" failed: turn failed: intel: intelligence HTTP 404: {\"error\":{\"message\":\"Invalid URL (POST /v1)\",\"type\":\"invalid_request_error\"}}"}
```

The events that explain a run, in the order they usually help:

| Event | Tells you |
|---|---|
| `run.start`, `run.done` | the run began; how it ended, with `status` and `err` |
| `step.start`, `step.done` | each step's attempt, status, tokens and error |
| `run.stalled` | nothing was ready and nothing in flight; `blocked_by` names the first failed step, if any |
| `step.retry` | a retry, with its backoff |
| `step.goto`, `step.timeout_routed` | a routing edge was taken |
| `breaker.open`, `breaker.probe`, `breaker.closed` | a circuit breaker changed state |
| `step.rate_wait` | a step is waiting for a rate token |
| `wait.resolved` | a wait ended, with its status |
| `human.ask`, `human.answer.rejected`, `human.ask.failed`, `human.judge.withheld` | a gate opened, refused an answer, failed, or was kept from a judge |
| `run.deadline`, `run.refused`, `run.dropped` | the run hit its deadline; a restore refused it; concurrency dropped a start |
| `start.inputs.invalid`, `run.inputs.invalid` | a trigger's inputs did not render, or did not match `inputs.schema`; no run was created |
| `restore.step.replay`, `restore.step.skipped`, `restore.step.failed` | what a restart did with a step caught running |

For a run that is still live, attach the [terminal or web UI](interface.md)
and turn on introspection (`/set a2a.introspection.enabled true`): the run
detail shows each step's status, timings, errors and output, and the debug
view tails the live log. Over A2A the same detail is the `run.get` command;
the agent itself can call `workflow.status`. Turn introspection off when you
are done: it exposes message bodies and internals.

### A stalled run

A run that ends with `no ready step and no finish reached` reached a point
where nothing was ready and nothing was in flight. The usual causes:

- **The `finish` sits behind a pruned branch.** A step whose every
  dependency was pruned is pruned too, and a `finish` that depends only on
  an untaken `switch` case or a false guard never runs. Make the `finish`
  depend on every arm, or give each arm its own `finish`.
- **A step depends on a wait's `on_timeout` target.** The target runs only
  when the timeout routes to it. When the wait succeeds instead, a step that
  depends on the target never becomes ready. Give the timeout branch its own
  path to a `finish`.
- **A step failed first.** The error then names it: `step "x" failed first:
  …`. The run ends `failed`, not `stalled`. Look at that step, not the one
  that never ran.

## Workflow traps

These behave as designed, and surprise people anyway:

- **Read outputs at `steps.<id>.output`.** `{{ hook.body.id }}` reads
  nothing; `{{ steps.hook.output.body.id }}` does. Paths are not checked at
  load, so a typo fails the step when it runs, and uses up a retry.
- **A fallback does not cover a pruned step.** A pruned or skipped step has
  `output: null`, which is a value, so `{{ steps.x.output | "none" }}`
  yields null. Test `steps.x.status`.
- **`switch` cases name one step as a string.** `cases: {a: prepare}`, not
  `cases: {a: [prepare]}`; a list is refused at load.
- **`on_error: continue` marks a failed step `done`.** Its `error` is set
  and its output is `{"error": …}`, so `status == "done"` is not proof of
  success; test `steps.<id>.error`.
- **A `human` step outputs the reply as text.** Branch with a `switch` on
  an `enum` answer; a path into the reply never resolves. See
  [Human in the loop](human-in-the-loop.md#route-on-the-answer).
- **`think` always answers with JSON.** A `think` step whose model replies
  in prose fails after two re-asks (`the answer was not valid JSON`). Use an
  `agent` step for free text.
- **Signals are edge-triggered.** A signal sent before the waiter is
  suspended is missed. The `workflow.signal` tool returns `delivered: 0`
  every time, because delivery happens on the next reactor pass; do not
  retry because of it.
- **`wait {on: condition}` sees only `runs`, `subagents`, `now_ms` and
  `signals`.** A condition on `vars` fails the step when it is evaluated.
- **`join` treats an unknown handle as finished**, with `{"error": "unknown
  handle"}`, so a typo satisfies the join at once.
- **A CEL error in a `when` guard fails the run**, for example
  `No such key` on a variable that has not been written yet.
- **`cache` never stores a step that suspends or nests**: waits, gates,
  sleeps, subagents and every body kind. A cached result is still read.
- **A secret reference is literal outside headers.** `{{secret:NAME}}`
  resolves in an `http` step's `headers` and `sign.secret` and a webhook's
  `auth`; in a `json` body or an `mcp.tool` argument it is sent as written.
- **Nested steps have scoped ids.** A step inside a body appears in logs as
  `each[3].check` or `par{fast}.call`.
- **Workflows loaded from files get no default store.** An instance whose
  long-lived workflows all come from `file:` or `dir:` entries starts with
  no durable store. Set `store.kind` explicitly.
- **A `race` ignores `min_success`.** It completes with the first branch
  to succeed.

## MCP connection problems

A server that fails to connect is logged as `mcp.connect.fail`, with the
server's own refusal, and left out of the tool catalogue; the rest of the
instance carries on. Two exceptions stop the process: the server backing
the durable store exits `6`, and a workflow whose `mcp.tool` step names the
server exits `2` (`mcp server "x" is not connected`). A server that comes
up later is dialled again (after 1 s, doubling to 60 s) and joins when it
answers (`mcp.joined`).

| Symptom | Cause and fix |
|---|---|
| `mcp.connect.fail` naming `401` | the server wants a credential. For an OAuth server, run `agentd --login mcp:<name>` once to cache a token; otherwise set the server's `auth` |
| `mcp.connect.fail` naming `-32020` or `-32021` | a 2026-07-28 server refused agentd's request itself; the message names the reason |
| a refused probe followed by a working connection | normal: a server on an MCP revision before 2026-07-28 does not know `server/discover`, and is reached through the `initialize` handshake |
| `start.subscribe.unsupported`, or a resource wait failing with a missing capability | the server does not advertise resource subscriptions, so nothing fires |
| `mcp.listen.unavailable` | the server does not hold a listen stream, so it cannot push tool-list changes |
| a header refused at load | `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Session-Id` and the other headers the client sets itself cannot be configured |

[MCP support reference](mcp-reference.md) has the full refusal table and
the revisions agentd speaks.

## Model errors

| Symptom | Cause and fix |
|---|---|
| `intelligence HTTP 404` | the endpoint path is wrong. A URL with a path is posted to exactly as written, so an OpenAI-compatible endpoint must be the full path, such as `https://api.openai.com/v1/chat/completions`, not `…/v1`. Anthropic is `https://api.anthropic.com` with `dialect: anthropic` |
| `intelligence HTTP 401` or `403` | the token is wrong or lacks access; check the secret the connection names |
| `no model is named for the agent's turns` at load | set `intelligence.model` (`--model`) or `intelligence.roles.turn` |
| `model "fsat" is not a declared model (known: smart)` at load | a step's or role's `model:` names no model under `intelligence.models`; the message lists the ones that exist |
| exit `4` after retries | the endpoint was unreachable or kept failing; the run's error carries the last response |
| `turn.model_fallback` | the model failed and its `fallback` answered |

[Connect model providers](connect-models.md) covers endpoints, dialects and
credentials.

## Browser UI connection problems

The web UI is an A2A client in the browser, so the browser's rules apply.

| Symptom | Cause and fix |
|---|---|
| the connect screen reports a CORS failure, or the daemon answers `403` | the page's origin is not in `a2a.cors.origins`. List it exactly: scheme, host and port, no path. Loopback origins such as `http://127.0.0.1:4173` must be listed too; only the page `agentd ui` launches is admitted unlisted |
| Chrome or Edge fails on a page served from a public origin to a local daemon | Private Network Access: the daemon answers the preflight only for a listed origin, so list the page's origin |
| Safari cannot reach `http://localhost` from an HTTPS page | Safari blocks it, and the page cannot work around it. Serve the daemon over TLS, or use `agentd ui` locally |
| the page asks to sign in on a loopback daemon | a browser is never the implicit operator. Enable `a2a.device_grant` and approve the code from a terminal (`/approve <code> <name>`), or use `agentd ui`, whose tab signs in with a launch code |
| `✗ unauthenticated`, `✗ forbidden`, `✗ incompatible` in a client's status bar | the credential is missing or revoked; the principal may not observe the agent; or the endpoint is not an A2A 1.0 agent. The client stops and names the fix |
| `◐ polling` instead of `● live` | the daemon serves no events feed; set `a2a.events.enabled: true` |

[Host the web UI](hosting-the-ui.md) and
[Use the terminal and web UI](interface.md) have the details.

## Next steps

- [Workflow language reference](workflow-language.md): every load-time
  error and status.
- [Events and metrics reference](telemetry-reference.md): every log event
  and its fields.
- [Monitor in production](observability.md): probes, metrics and alerts.
- [Operate a running daemon](operations.md): reloads, drains and live
  state.
