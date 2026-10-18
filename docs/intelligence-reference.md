# Intelligence wire reference

This page lists exactly what agentd sends to a model endpoint and how it
reads the answer, for each wire dialect, and how it reacts to each kind of
failure. It is for integrators building a gateway or a compatible server,
and for operators reading a failed call. The concepts behind it are in
[Models, providers and roles](intelligence.md).

## Endpoints

| Form | Transport | Notes |
|---|---|---|
| `https://host[:port][/path][?query]` | TCP and TLS, server certificate verified | any host |
| `http://host[:port][/path][?query]` | plaintext TCP | `127.0.0.1`, `::1` or `localhost` only |
| `mock:final`, `mock:file:<playbook.json>` | the built-in mock model, run in-process and dialed on loopback | scripted answers, for trying an agent or a workflow without a provider; release binaries and the image carry it (`mock-models`) |

- **List.** `endpoints` is a YAML list or one comma-separated string. Order
  is failover priority; the first element is the primary.
- **Path.** A URL with a path is posted to as written, query string
  included. A URL with no path gets its dialect's default path.
- **Validation.** Every element of every connection is checked at load. Any
  other scheme, or `http://` to a non-loopback host, is refused.

| Dialect | Default path | Effective path |
|---|---|---|
| `openai` | `/v1/chat/completions` | as configured |
| `anthropic` | `/v1/messages` | as configured |
| `bedrock` | — | `/model/{modelId}/converse`, always |
| `decisions` | `/v1/decisions` | as configured |

Every request is `POST`, HTTP/1.1, with `Connection: close`, on a fresh
connection, non-streaming. Every request carries a `traceparent` header: a
span of its own in the trace of the unit of work it serves, and that trace's
`tracestate` when it has one. The configured `headers` are added to every
request, beside the headers the dialect sets.

## OpenAI-compatible dialect

**Headers:** `content-type: application/json`, and
`authorization: Bearer <token>` when a credential resolves.

| Body field | Value |
|---|---|
| `model` | the wire model |
| `max_tokens` | the per-call answer cap, for a chat model whose name does not start with `gpt-5`, `chatgpt-5`, `o1`, `o3` or `o4` |
| `max_completion_tokens` | the answer cap, for those names; for a reasoning model, the cap plus the thinking room |
| `temperature` | `0`; absent for a reasoning model |
| `reasoning_effort` | the effort, for a reasoning model |
| `messages` | `system`, `user`, `assistant` (with `tool_calls`), `tool` |
| `tools` | `[{type: function, function: {name, description, parameters}}]`, when the call has tools |
| `tool_choice` | `"auto"`, when the call has tools |
| `response_format` | `{type: json_schema, json_schema: {name: "answer", schema, strict}}`, when the answer has a schema |

- A tool's `parameters` is the MCP server's `inputSchema`, unchanged.
- An assistant tool call's `arguments` is sent as a JSON string.
- A tool result that is an error is sent with the content prefixed
  `ERROR: `.

**Response:** `choices[0].message.content` is the text;
`choices[0].message.tool_calls[]` are the tool calls, each `arguments` parsed
as JSON (an unparsable string is kept as `{"_raw": "<string>"}`);
`usage.prompt_tokens` and `usage.completion_tokens` are the tokens. A body
with an `error` object is a provider error.

| `finish_reason` | Read as |
|---|---|
| `stop` | final answer |
| `tool_calls` | tool use |
| `length` | answer cut at the cap |
| anything else | other |

## Anthropic dialect

**Headers:** `content-type: application/json`,
`anthropic-version: 2023-06-01`, and `x-api-key: <token>` when a credential
resolves.

| Body field | Value |
|---|---|
| `model` | the wire model |
| `system` | every system message, joined with a blank line |
| `max_tokens` | the answer cap; for a reasoning model, plus the thinking room |
| `thinking` | `{type: adaptive}`; `{type: enabled, budget_tokens}` when the model sets a `budget` |
| `output_config.effort` | the effort, for an adaptive reasoning model |
| `output_config.format` | `{type: json_schema, schema}`, when the answer's schema has a constrained form |
| `messages` | `user` and `assistant`; tool calls as `tool_use` blocks, results as `tool_result` blocks in a `user` message, with `is_error` |
| `tools` | `[{name, description, input_schema}]`, when the call has tools |

No `temperature` and no `tool_choice` are sent. While a turn is still calling
tools, an assistant message that came back with `thinking` or
`redacted_thinking` blocks is sent back to the same model with its content
unchanged, in order.

**Response:** `text` blocks are joined into the text; `tool_use` blocks are
the tool calls; `usage.input_tokens` and `usage.output_tokens` are the tokens.
A body with `type: error` is a provider error.

| `stop_reason` | Read as |
|---|---|
| `end_turn`, `stop_sequence` | final answer |
| `tool_use` | tool use |
| `max_tokens` | answer cut at the cap |
| anything else | other |

## Bedrock dialect

**Path:** `/model/{modelId}/converse`, where `modelId` is the wire model
percent-encoded as one path segment (`:` and `/` included). **Auth:** AWS
SigV4 over the method, host, path and body; `token` is ignored, and an
`auth: {kind: aws}` block is required. **Headers:** `content-type` and
`accept: application/json`.

| Body field | Value |
|---|---|
| `messages` | `user` and `assistant` turns, consecutive same-role turns merged; blocks `{text}`, `{toolUse: {toolUseId, name, input}}`, `{toolResult: {toolUseId, content: [{text}], status: success\|error}}` |
| `system` | `[{text}]`, one per system message |
| `inferenceConfig.maxTokens` | the answer cap; for a reasoning model, plus the thinking room |
| `inferenceConfig.temperature` | `0`, except for a Claude model and a reasoning model |
| `toolConfig.tools` | `[{toolSpec: {name, description, inputSchema: {json}}}]`, when the call has tools |
| `additionalModelRequestFields` | reasoning and output-format fields, by model family |

agentd tells model families apart by the provider segment of the model id,
after an optional cross-region prefix (`us.`, `eu.`, `global.`, …):

| Family | Model id | Reasoning fields | Output format |
|---|---|---|---|
| Claude | `anthropic.…` | `thinking` and `output_config.effort`, as on Anthropic | `output_config.format`, as on Anthropic |
| OpenAI open-weight | `openai.…` | `reasoning_effort` | none (asked in the prompt) |
| any other, or an ARN | — | none; a reasoning model is refused at load | none (asked in the prompt) |

**Response:** `output.message.content` blocks: `text` is joined into the
text, `toolUse` are the tool calls, and `reasoningContent` blocks mark a
message that is sent back within the turn. `usage.inputTokens` and
`usage.outputTokens` are the tokens. `stopReason` maps as on Anthropic.

## Decisions dialect

Allowed on a provider only, for models of `kind: decision`. **Headers:**
`Content-Type: application/json`, and `Authorization: Bearer <token>` when a
credential resolves.

```json
{
  "model": "typesafe/jev-1.13",
  "state": "The app crashes on login and I cannot work.",
  "questions": {
    "queue":  {"type": "choice", "instructions": "queue",
               "criteria": {"billing": "payment problems", "tech": "bugs and outages"}},
    "urgent": {"type": "noul", "instructions": "The customer cannot work at all."},
    "mood":   {"type": "score", "instructions": "mood",
               "criteria": ["calm", "annoyed", "angry"]}
  }
}
```

| Question | `instructions` | `criteria` |
|---|---|---|
| `choice` | the question's `instructions`, else its id | option → criterion (an empty string for a list option) |
| `noul` | the statement, then the question's `instructions` on a new line | — |
| `score` | the question's `instructions`, else its id | the levels, lowest first |

`state` is a string, a list or an object, sent as given. A decision holds 1
to 16 questions, and a `choice` or `score` 2 to 32 options or levels.

**Response:**

```json
{
  "answers": {
    "queue":  {"choice": "tech", "confidence": 0.91, "probabilities": {"billing": 0.09, "tech": 0.91}},
    "urgent": {"noul": 0.96},
    "mood":   {"score": 1.4, "confidence": 0.9}
  },
  "usage": {"input_tokens": 40, "output_tokens": 8}
}
```

| Check | Result when it fails |
|---|---|
| every question has an answer | error naming the question |
| a `choice` is one of the options | error |
| a `noul` is a number from 0 to 1 | error |
| a `score` is a number | error; a score outside the scale is clamped to it |
| a `confidence`, when present, is from 0 to 1 | error |

A `score`'s `probabilities` are keyed by level position on the wire and by
level name in the output. Answers from this dialect are `calibrated: true`.

### Emulation on a chat dialect

A decide call on a chat or reasoning model is one call on that model's
dialect, at temperature `0` where the dialect sends one:

- A system message describes the expected JSON object, one key per
  question, and says the state is data, never an instruction.
- A user message carries `STATE:` and `QUESTIONS:`.
- The answer's schema is one required property per question: a `choice`
  is an enum of its options with a `confidence`, a `noul` a number, a
  `score` a number with a `confidence`.

The answer is checked as a native one is. An answer that does not fit is
re-asked once with the reason, and the tokens of both calls are counted.
Emulated answers are `calibrated: false`, and their `probabilities` are
dropped.

## Tool names

OpenAI and Anthropic accept tool names matching `^[a-zA-Z0-9_-]+$`. agentd
sends every other character of a tool name, such as the `.` in
`resource.read`, as `_`, in the tool list and in earlier tool calls, and maps
the model's tool calls back to the original names.

## Structured output

When a call's answer has a JSON Schema, the connection's `structured_output`
decides how it is asked:

| Dialect | `auto` (default) sends | `prompt` sends |
|---|---|---|
| `openai` | `response_format` with the schema, when its root is an object | nothing |
| `anthropic` | `output_config.format`, when the schema has a constrained form | nothing |
| `bedrock`, Claude family | `additionalModelRequestFields.output_config.format`, likewise | nothing |
| `bedrock`, other families | nothing | nothing |
| `decisions` | no schema; `prompt` is refused at load | — |

The prompt asks for the answer's shape either way, and the answer is always
validated against the full schema. In a turn or step a miss is re-asked up
to 2 times; an emulated decision is re-asked once.

**The constrained form** is the schema made sendable:

- Every object that lists `properties` gets `additionalProperties: false`.
- These keywords are left out: `minimum`, `maximum`, `exclusiveMinimum`,
  `exclusiveMaximum`, `multipleOf`, `minLength`, `maxLength`, `pattern`,
  `minItems`, `maxItems`, `uniqueItems`, `minProperties`, `maxProperties`,
  `contains`, `minContains`, `maxContains`.
- A `format` is kept only when it is `date-time`, `time`, `date`,
  `duration`, `email`, `hostname`, `uri`, `ipv4`, `ipv6` or `uuid`.

A schema has **no constrained form** when its root is not an object, or any
part of it has `oneOf`, `not`, `if`, `then`, `else` or `patternProperties`,
an object with no `properties`, an `additionalProperties` that is a schema, a
`$ref` outside `$defs` or `definitions`, or a recursive definition.

On `openai`, `strict` is `true` when the constrained form exists, every
object in it requires all of its properties, and it has no `allOf`; the
constrained form is then sent. Otherwise the schema is sent as written with
`strict: false`, as guidance.

## Reasoning

A `kind: reasoning` model is asked with its `reasoning` defaults; an
`effort` on a node or a `think` call replaces the effort for that call.

| Effort | Thinking room, without a `budget` |
|---|---|
| `low` | 2,048 tokens |
| `medium` (default) | 8,192 tokens |
| `high` | 24,576 tokens |

A `budget` (at least 1,024) replaces the room and, on Anthropic and Claude on
Bedrock, selects the `thinking: {type: enabled, budget_tokens}` form. Every
dialect raises the output cap by the room, and budget admission counts it.
A fallback model is asked without the head model's reasoning, as the model
it is.

## Temperature and output caps

| Call | `temperature` | Answer cap |
|---|---|---|
| chat model on `openai` | `0` | 4,096 |
| chat model on Bedrock, non-Claude family | `0` | 4,096 |
| Claude model, on `anthropic` or Bedrock | none | 4,096 |
| reasoning model, any dialect | none | 4,096 plus the thinking room |
| preflight on a chat model | as above | 1,024 |

## Failures, retries and failover

| Outcome | Same endpoint | Next endpoint | Fallback model | `reason` label |
|---|---|---|---|---|
| connection refused or reset, DNS failure | no | yes | yes | `unreachable` |
| timeout | no | yes | yes | `timeout` |
| `429` | 2 retries | yes | yes | `other` |
| `5xx` | 2 retries | yes | yes | `5xx` |
| `401`, `403` | no | no | no | `auth` |
| the credential could not be produced; nothing was sent | no | no | no | `auth` |
| other `4xx` | no | no | no | `other` |
| malformed response body | no | no | no | `other` |

The `reason` label is on `agent_intel_errors_total`. A one-shot job whose
model cannot be reached, refuses the credential, or cannot be sent one,
exits `4`.

| Constant | Value |
|---|---|
| Same-endpoint retries | 2, after 250 ms and 500 ms, or the endpoint's `Retry-After` when longer |
| A `Retry-After` over 30 s | no retry; the endpoint is out of rotation until it passes, at most 5 min |
| Breaker opens after | 3 consecutive failover-class failures |
| Breaker cooldown | 5 s, doubling on each consecutive open, at most 60 s |
| Half-open | one probe after the cooldown; success closes, failure reopens |
| After a success | the lowest-index healthy endpoint becomes active |
| Per-call ceiling | the connection's `timeout` (default 120 s), and the run's deadline |
| Daemon all-down backoff | up to 8 more sweeps, 1 s doubling to 30 s, ±25% jitter |

The all-down backoff applies to a long-lived instance when every endpoint is
down. It is skipped when the cause is an authentication failure and when the
model has a fallback, which is asked at once instead.

## Credentials

| Connection | Endpoint | Credential, first that resolves |
|---|---|---|
| top-level | 1 | `AGENTD_INTELLIGENCE_TOKEN`, `AGENTD_INTELLIGENCE_TOKEN_FILE`, the `auth` bearer, `token`, `token_file` |
| top-level | *N* ≥ 2 | `AGENTD_INTELLIGENCE_TOKEN_<N>`, `AGENTD_INTELLIGENCE_TOKEN_<N>_FILE`; otherwise none |
| a provider | every | the `auth` bearer, `token`, `token_file` |

- An `aws` `auth` block signs every request of every endpoint instead.
- `token_file` and the `auth` bearer are read each time a turn, step or
  judge starts; a SigV4 signature is computed per request. One that cannot
  be produced then — no login, a token file that cannot be read, an `aws`
  source with no credentials — fails the call without sending it, as an
  authentication error: no failover to another endpoint, no fallback model.
  The error names the target (`intelligence`, `intelligence:<provider>`) and
  the cause, `auth.unavailable` says so once a minute while it lasts, and the
  next call tries again.
- A `token_file` that cannot be read stops the start, or the reload, that
  would use it. An `AGENTD_INTELLIGENCE_TOKEN_<N>_FILE` is read when a worker
  builds its client, so an unreadable one fails the turn or step that uses
  it, and `--validate-config` does not see it.
- In a config file, `token` and credential-shaped `headers` (`authorization`,
  `x-api-key`, `api-key`, `token`, `secret`, `password`, and names ending in
  `-token`, `_token`, `-key` or `_key`) must be `{{secret:NAME}}` or
  `{{secret-file:PATH}}` references.
- `auth` kinds that apply to a model connection: `static` with `token`,
  `oauth2`, `aws`, `spiffe` with `svid: jwt`. Any other is refused at load,
  because it cannot send a bearer: a `static` credential under its own
  `header` (put that in `headers`) or a SPIFFE `x509` SVID.
- No credential is ever logged; health reports carry an endpoint's index and
  scheme only.

## Flags and environment variables

| Flag | Path | Environment |
|---|---|---|
| `--intelligence <list>` | `intelligence.endpoints` | `AGENTD_INTELLIGENCE` |
| `--intelligence-token <token>` | `intelligence.token` | `AGENTD_INTELLIGENCE_TOKEN` |
| `--intelligence-token-file <path>` | `intelligence.token_file` | `AGENTD_INTELLIGENCE_TOKEN_FILE` |
| — | endpoint *N* of the top-level connection | `AGENTD_INTELLIGENCE_TOKEN_<N>`, `AGENTD_INTELLIGENCE_TOKEN_<N>_FILE` |
| `--model <name>` | `intelligence.model` | `AGENTD_MODEL` |
| `--model-swap <policy>` | `intelligence.swap_policy` | `AGENTD_MODEL_SWAP` |
| `--budget-tokens-lifetime <n>` | `intelligence.budget.lifetime_tokens` | `AGENTD_BUDGET_TOKENS` |
| `--login <target>` | — | — |
| `--logout <target>` | — | — |

Every other `intelligence` path is a flag and a variable too: dots and
underscores become dashes in the flag and underscores in the variable, as
`--intelligence-dialect` and `AGENTD_INTELLIGENCE_DIALECT`, or
`--intelligence-roles-turn` and `AGENTD_INTELLIGENCE_ROLES_TURN`. A flag
wins over the environment, which wins over files. `--login` and `--logout`
take `intelligence` or `intelligence:<provider>`. The full list is in
`agentd --help` and the [Configuration reference](configuration.md).

## Configuration keys

Connection keys, on `intelligence` and on each `intelligence.providers.<name>`:

| Key | Values | Default |
|---|---|---|
| `endpoints` | list, or comma-separated string | required on a provider |
| `dialect` | `openai`, `anthropic`, `bedrock`, `decisions` (providers only) | `openai` |
| `token` | a secret reference in a file | — |
| `token_file` | a path | — |
| `headers` | name → value | — |
| `auth` | an `auth` block | — |
| `timeout` | a duration | `120s` |
| `structured_output` | `auto`, `prompt` | `auto` |

Model keys, on `intelligence.models.<name>`:

| Key | Values | Default |
|---|---|---|
| `model` | the wire model | required |
| `provider` | a provider name | the top-level connection |
| `kind` | `chat`, `reasoning`, `decision` | `chat` |
| `window` | context window in tokens; not on a decision model | guessed from the wire model's name |
| `fallback` | a model name; for a chat or reasoning model, not a decision model | — |
| `escalate` | a model name; decision models only | — |
| `reasoning` | `{effort: low\|medium\|high, budget: ≥1024}`; reasoning models only | effort `medium` |

The rest of the section, on `intelligence` only:

| Key | Values | Default |
|---|---|---|
| `model` | the turn model: a model name or a wire model | — |
| `roles.<role>` | a model name; `{model, confidence}` for `decide`, `preflight`, `goal`, `approve` | the turn model |
| `routes` | model name → when to use it; at most 32 | — |
| `swap_policy` | `finish-on-old`, `restart-turn` | `finish-on-old` |
| `budget` | see [Control spend with budgets](budgets.md) | — |

Every key under `intelligence` reloads live. An instruction document may
write `model`, `models` (without `provider`), every role but `approve`,
`dialect`, `budget`, `timeout`, `structured_output` and `swap_policy`. The
other keys are the operator's.

When a context window is needed, agentd takes `context.model_window`, else
the smallest declared `window` among the turn model and the routes, else a
guess from the wire model's name: a name containing `claude` 200,000;
`gpt-4.1` or `gemini` 1,000,000; anything else 128,000.

## Events and metrics

| Event | Fields |
|---|---|
| `turn.model` | `agent_path`, `role`, `model`, `provider` |
| `turn.model_fallback` | `turn`, `model`, `served_by` |
| `turn.model_degraded` | `turn`, `from`, `to`, `provider` |
| `turn.routed` | `ctx`, `model` |
| `turn.route_removed` | `ctx`, `model`, `reason` |
| `turn.effort_ignored` | `agent_path`, `effort`, `model` |
| `intel.decision` | `model`, `escalated`, `answers`, `unsure`; `role` for a judge |
| `intel.decision.escalate` | `turn`, `from`, `to`, `unsure` |
| `intel.health` | `all_down` |
| `intel.swap` | `kind`, `model_from`, `model_to`, `endpoint_change`, `policy` |
| `intel.swap.sent` | `subagents`, `policy` |
| `auth.unavailable`, `auth.recovered` | `target`, `err`, `failures`: a connection's credential could not be produced, and when it is again |
| `preflight.route_unsure`, `preflight.skills_capped` | the route's choice and confidence; `asked`, `catalogued` |

| Metric | Type |
|---|---|
| `agent_intel_calls_total` | counter |
| `agent_intel_errors_total{reason}` | counter |
| `agent_intel_up` | gauge |
| `agent_intel_all_down` | gauge |
| `agent_tokens_total{type}` | counter |

They count every model call of the instance: a turn worker or a subagent
reports its calls, their errors and what the last one found of its endpoint
to the supervisor, whose `/metrics` serves them.
`agent_intel_call_duration_ms` is reserved: a scrape shows only its `# HELP`
and `# TYPE` lines. The full catalog
is in the [Events and metrics reference](telemetry-reference.md).

## Next steps

- [Connect model providers](connect-models.md): a complete configuration
  for each dialect.
- [Models, providers and roles](intelligence.md): why the wire behaves
  this way.
- [Outbound credentials](authentication.md): every `auth` field.
- [CLI, environment and exit codes](cli.md): the exit codes these
  failures produce.
