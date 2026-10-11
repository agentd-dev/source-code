# Connect model providers

This guide connects agentd to the model providers you use: OpenAI and any
OpenAI-compatible API, Anthropic, Amazon Bedrock, Azure OpenAI, Google, a
local server, and a decision model. It is for builders and operators who set
up an agent's model access, and each section gives one complete
configuration you can validate and run. After it you can point an agent at
one vendor or several, survive an endpoint outage, and keep every key out of
your files.

## What a connection is

A connection is where model calls go and how they authenticate: an ordered
list of endpoints, a wire dialect and a credential. agentd has one top-level
connection under `intelligence`, and you can add named ones under
`intelligence.providers`. A model you name in the config is reached on the
top-level connection unless it says `provider:`.

| Dialect | Speaks | Use it for |
|---|---|---|
| `openai` (default) | `POST …/chat/completions` | OpenAI, Azure OpenAI, Google's OpenAI-compatible endpoints, vLLM, Ollama, LiteLLM, most gateways |
| `anthropic` | `POST /v1/messages` | Anthropic's API |
| `bedrock` | Bedrock Converse, SigV4-signed | Amazon Bedrock, without a gateway |
| `decisions` | `{model, state, questions}` | A decision model, on a provider only |

Three rules hold for every connection:

- **The URL is posted to as written.** An endpoint with a path is used as
  it is, so write the full path:
  `https://api.openai.com/v1/chat/completions`. Only a host with no path
  gets the dialect's default path (`/v1/chat/completions`,
  `/v1/messages`, `/v1/decisions`).
- **HTTPS, or plaintext to loopback.** `https://` reaches any host.
  `http://` is accepted only for `127.0.0.1`, `::1` or `localhost`, for a
  server on the same machine. Anything else is refused at load.
- **Name a model.** A real connection with no model named is refused at
  load. Only the built-in test endpoint runs without one.

The models agentd calls must support native tool calling: it is the only
way agentd hands a model its tools. An endpoint that ignores `tools` ends
every turn on its first answer.

## Validate and test a connection

Validate the file first. It reads every `{{secret:NAME}}` reference from the
environment, so export the keys before you run it:

```bash
export OPENAI_API_KEY=sk-...
agentd --validate-config --config models.yaml
```

```
{"event":"config.valid","files":["models.yaml"]}
```

Then send one task. The answer goes to stdout and the process exits:

```bash
agentd --config models.yaml --instruction 'Reply with the word ready.'
```

Exit code `0` means the model answered. Exit code `4` means agentd could not
get an answer: the endpoint was unreachable, or it refused the credential.
The `step.done` log line on stderr carries the provider's status and body,
for example `intelligence HTTP 401: …`. The exit codes are listed in
[CLI, environment and exit codes](cli.md).

## OpenAI and OpenAI-compatible APIs

The default dialect needs only an endpoint, a key and a model:

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  token: "{{secret:OPENAI_API_KEY}}"
  model: gpt-5.1
```

The token is sent as `Authorization: Bearer <token>`. The same shape works
for any API that serves `/chat/completions` with tool calling: write its
full URL and its model name. A gateway that wants a different header takes
it under `headers` (see [Credentials](#credentials)).

## Anthropic

Set `dialect: anthropic` and give the host alone. agentd posts to
`/v1/messages` and sends the key as `x-api-key`:

```yaml
intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  model: claude-sonnet-5
```

agentd sends no `temperature` to a Claude model. Claude models answer at
their own default.

## Amazon Bedrock

The `bedrock` dialect talks to the Bedrock runtime directly and signs every
request with AWS SigV4. It needs an `auth` block of `kind: aws`, and refuses
to load without one. The model id travels in the URL, so `model` is a Bedrock
model id, an inference profile id, or an ARN:

```yaml
intelligence:
  endpoints: https://bedrock-runtime.us-east-1.amazonaws.com
  dialect: bedrock
  # A Bedrock model id or inference profile id (or its ARN).
  model: us.anthropic.claude-haiku-4-5-20251001-v1:0
  auth:
    kind: aws
    region: us-east-1
    service: bedrock
    source: irsa          # or env, imds, sso
```

Pick the credential `source` that matches where agentd runs:

| `source` | Credentials from | Login |
|---|---|---|
| `env` | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` | none |
| `imds` | the EC2 instance role (IMDSv2), refreshed | none |
| `irsa` | the EKS pod's web identity, exchanged with STS, refreshed | none |
| `sso` | IAM Identity Center, cached temporary credentials | `agentd --login intelligence` |

For `sso`, add the portal and role, then log in once on a machine with a
terminal:

```yaml
intelligence:
  endpoints: https://bedrock-runtime.us-east-1.amazonaws.com
  dialect: bedrock
  model: us.anthropic.claude-haiku-4-5-20251001-v1:0
  auth:
    kind: aws
    region: us-east-1
    service: bedrock
    source: sso
    sso_start_url: https://my-org.awsapps.com/start
    account_id: "123456789012"
    role_name: AgentdBedrock
```

```bash
agentd --login intelligence --config bedrock.yaml
```

A gateway in front of Bedrock that speaks the OpenAI wire and checks AWS IAM
takes the same `auth` block with the default dialect and the gateway's
`service` name (for API Gateway, `execute-api`).

## Azure OpenAI

Azure OpenAI speaks the OpenAI wire on a per-deployment URL. Write the whole
URL, query string included; agentd sends it as written. Azure takes its key
in an `api-key` header:

```yaml
intelligence:
  endpoints: https://my-resource.openai.azure.com/openai/deployments/my-gpt-5-1/chat/completions?api-version=2024-10-21
  model: gpt-5.1
  headers:
    api-key: "{{secret:AZURE_OPENAI_API_KEY}}"
```

To authenticate with Microsoft Entra ID instead of a key, use an `oauth2`
block. The client-credentials grant suits a service with an app
registration and refreshes on its own:

```yaml
intelligence:
  endpoints: https://my-resource.openai.azure.com/openai/deployments/my-gpt-5-1/chat/completions?api-version=2024-10-21
  model: gpt-5.1
  auth:
    kind: oauth2
    grant: client_credentials
    issuer: https://login.microsoftonline.com/<tenant-id>/v2.0
    client_id: <app-client-id>
    client_secret: "{{secret:ENTRA_CLIENT_SECRET}}"
    scopes: ["https://cognitiveservices.azure.com/.default"]
```

For a person at a terminal, `grant: device` with a public client works the
same way after `agentd --login intelligence`.

## Google Vertex AI and the Gemini API

Google serves Gemini models on OpenAI-compatible endpoints, so the default
dialect reaches them with no gateway. On Vertex AI the endpoint carries your
project and region, and Vertex names a Gemini model `google/<model>`:

```yaml
intelligence:
  endpoints: https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/endpoints/openapi/chat/completions
  # Vertex names a Gemini model google/<model>.
  model: google/<gemini-model>
  auth:
    kind: oauth2
    grant: authorization_code
    issuer: https://accounts.google.com
    client_id: <desktop-client-id>.apps.googleusercontent.com
    client_secret: "{{secret:GOOGLE_CLIENT_SECRET}}"
    scopes: ["https://www.googleapis.com/auth/cloud-platform"]
```

The `authorization_code` grant runs a browser sign-in once:
`agentd --login intelligence` prints the URL, catches the redirect on a
loopback port, and caches the token. On a workload with no browser, have
your platform write a current access token to a file and name it in
`token_file`. agentd reads the file each time a turn or step starts, so a
rotated token needs no restart.

The Gemini API takes an API key as a bearer token:

```yaml
intelligence:
  endpoints: https://generativelanguage.googleapis.com/v1beta/openai/chat/completions
  token: "{{secret:GEMINI_API_KEY}}"
  model: <gemini-model>
```

Replace `<gemini-model>` with a model your project can use.

## A local server: vLLM, Ollama, LiteLLM

A server on the same machine is reached over plaintext loopback HTTP and
usually needs no key. vLLM serves on port 8000 by default:

```yaml
intelligence:
  endpoints: http://127.0.0.1:8000/v1/chat/completions
  model: openai/gpt-oss-20b
```

Ollama serves its OpenAI-compatible API on port 11434:

```yaml
intelligence:
  endpoints: http://127.0.0.1:11434/v1/chat/completions
  model: gpt-oss:20b
```

A LiteLLM proxy on port 4000 is the same shape, and fronts any provider
LiteLLM supports. A gateway in the same pod also keeps provider keys out of
agentd's process: the gateway holds them, and agentd dials it with no key.
Start the server with tool calling enabled for the model; agentd sends its
tools on every call. A server on another host needs `https://`.

## A gateway with a private certificate authority

agentd verifies every endpoint's certificate against the bundled public
roots. A gateway whose certificate comes from your own CA needs that CA
added with `security.tls_ca` (flag `--tls-ca`), a PEM file:

```yaml
security:
  tls_ca: /etc/agentd/corp-ca.pem
intelligence:
  endpoints: https://llm-gateway.corp.example/v1/chat/completions
  token: "{{secret:GATEWAY_TOKEN}}"
  model: gpt-5.1
```

The CA is added to the public roots, for every outbound HTTPS call the
process makes. It is read once at startup; a missing or unreadable file
stops the start with exit `2`.

## A decision model

A decision model answers typed questions about a state and writes no text
(see [Models, providers and roles](intelligence.md)). It lives on a provider
with `dialect: decisions`, whose endpoint is the decisions resource's full
URL. The top-level connection cannot be a decisions connection, because it
serves the agent's turns:

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  token: "{{secret:OPENAI_API_KEY}}"
  providers:
    openrouter:
      endpoints: https://openrouter.ai/api/alpha/decisions
      dialect: decisions
      token: "{{secret:OPENROUTER_API_KEY}}"
  models:
    smart: {model: gpt-5.1}
    jev:   {model: typesafe/jev-1.13, kind: decision, provider: openrouter, escalate: smart}
  roles:
    turn: smart
    decide: jev
```

A model on a `decisions` provider must be `kind: decision`, and a decision
model must be on one. [Choose models for each job](models-and-roles-guide.md)
shows where decision models serve.

## A test endpoint with no network

The built-in mock model answers on loopback with no key and no network. It
is a test endpoint, not a model: use it to try an agent or a workflow before
you have a provider, and in CI.

```yaml
intelligence:
  # The built-in mock model: a test endpoint, no key, no network.
  endpoints: mock:final
  model: gpt-5.1
```

`mock:final` answers every call at once with `mock-llm done`.
`mock:file:<playbook.json>` answers from a scripted playbook, including
decisions; [Author workflows](author-workflows.md#test-a-workflow) shows
one. The release binaries and the container image include the mock (the
`mock-models` build feature); see [Build features and crates](features.md).

## Use several vendors at once

Add a provider for each further connection, and point a model at it with
`provider:`. Each provider has its own endpoints, dialect, credential,
headers, `auth` block and `timeout`:

```yaml
intelligence:
  # The top-level connection.
  endpoints: https://api.openai.com/v1/chat/completions
  token: "{{secret:OPENAI_API_KEY}}"

  # Further connections, each with its own endpoints and credential.
  providers:
    anthropic:
      endpoints: https://api.anthropic.com
      dialect: anthropic
      token: "{{secret:ANTHROPIC_API_KEY}}"
      timeout: 180s

  models:
    smart: {model: gpt-5.1, fallback: deep}
    fast:  {model: gpt-5-mini}
    deep:  {model: claude-opus-5-5, provider: anthropic, kind: reasoning}

  roles:
    turn: smart
    think: deep
    preflight: fast
    compaction: fast
```

A model's name (`smart`) stays inside agentd; the provider receives the wire
model (`gpt-5.1`). Providers and their endpoints are the operator's: an
instruction document can declare models and bind roles, but cannot add a
provider or point a model at one. An `oauth2` provider is logged into under
its own target, `agentd --login intelligence:<provider>`.

## Survive an endpoint outage

Give a connection several endpoints and agentd fails over between them. The
list order is the priority: agentd uses the first healthy endpoint and
returns to it as soon as it recovers.

```yaml
intelligence:
  endpoints:
    - https://llm-gw-a.example.com/v1/chat/completions
    - https://llm-gw-b.example.com/v1/chat/completions
  token: "{{secret:GW_A_KEY}}"      # endpoint 1
  model: gpt-5.1
```

On the top-level connection each endpoint has its own credential, set by
position in the environment: endpoint 2 reads `AGENTD_INTELLIGENCE_TOKEN_2`
or `AGENTD_INTELLIGENCE_TOKEN_2_FILE`, endpoint 3 `…_3`, and so on. `token`
belongs to endpoint 1 only, so a backup never dials with the primary's key,
and an endpoint with no variable is sent no token. Configured `headers` and
an `aws` signature apply to every endpoint.

```bash
export GW_A_KEY=...
export AGENTD_INTELLIGENCE_TOKEN_2_FILE=/var/run/secrets/gw-b/token
```

Every endpoint of a provider dials with the provider's one `token` or `auth`.
The positional variables belong to the top-level connection only.

agentd moves to the next endpoint on a refused or reset connection, a
timeout, a `429` or a `5xx`. It does not fail over on a `401`, `403`, other
`4xx` or a malformed answer, because those repeat on every endpoint. An
endpoint that fails three times in a row is skipped for a cooldown that
starts at 5 seconds and doubles to 60. The exact rules are in the
[Intelligence wire reference](intelligence-reference.md).

> **Note.** Failover sends the same request, with the same model name, to
> every endpoint in the list. Use it for replicas of one service. To fall
> back to a different model or vendor, use a model `fallback`.

## Fall back to another model

A model's `fallback` names the model to ask when it is unavailable: every
endpoint of its connection failed, or every breaker is open. In the
several-vendors sample, `smart` falls back to `deep`, on the `anthropic`
provider. The fallback can have a fallback of its own, and a cycle is
refused at load. Each call
tries the first model again first, so the chain recovers by itself, and the
`turn.model_fallback` log line says which model served. An authentication
failure is never handed down the chain; fix the credential instead.

## Credentials

A connection takes its credential in one of four forms:

| Form | Sent as | Read |
|---|---|---|
| `token: "{{secret:NAME}}"` | the dialect's key header | at startup and on reload |
| `token_file: <path>` | the dialect's key header | each time a turn or step starts |
| `headers: {Name: "{{secret:NAME}}"}` | that header, as written | at startup and on reload |
| `auth: {kind: …}` | a refreshing bearer, or a SigV4 signature | bearer: each time a turn or step starts; SigV4: every request |

The dialect's key header is `Authorization: Bearer` on `openai` and
`decisions`, and `x-api-key` on `anthropic`. Bedrock ignores `token` and
signs with `auth`.

- **A file names a secret; it never holds one.** In a config file,
  `token` and any credential-shaped header must be a `{{secret:NAME}}` or
  `{{secret-file:PATH}}` reference. An inline key is refused at load. A
  value from a flag or the environment may be literal.
- **`token` wins over `token_file`** when both are set, with a warning.
- **The `auth` kinds that apply to a model connection** are `static` with
  a `token`, `oauth2`, `aws`, and `spiffe` with `svid: jwt`. A kind the
  connection cannot apply, such as `static` with `header`/`value` or
  `spiffe` with `svid: x509`, is refused at load. Send a custom key header
  through `headers` instead.
- **Log in before you start.** agentd does not prompt for a login while it
  runs. Run `agentd --login intelligence` (or `intelligence:<provider>`)
  first; the token is cached and refreshed in memory.

The [Outbound credentials](authentication.md) guide covers every `auth`
field, the login cache and the other endpoints that take an `auth` block.

## Structured answers on a strict server

When a call's answer has a JSON Schema, agentd asks the provider to hold the
model to it: `response_format` on the OpenAI wire, `output_config.format` on
Anthropic. Some OpenAI-compatible servers refuse `response_format`. Set
`structured_output: prompt` on that connection and agentd asks for the shape
in the prompt alone:

```yaml
intelligence:
  endpoints: http://127.0.0.1:4000/v1/chat/completions
  model: gpt-5-mini
  structured_output: prompt
```

Either way agentd validates the answer against the schema and asks again
when it does not fit. A provider takes the same key, and
`structured_output: prompt` on a `decisions` provider is refused, because a
decision endpoint is sent no schema.

## Closed egress

Under `security.egress: closed`, every endpoint of every connection must
match a `kind: intelligence` entry in the service catalog. An endpoint
matches an entry whose endpoint is its origin or a prefix of it:

```yaml
security:
  egress: closed

services:
  openai:     {kind: intelligence, endpoint: "https://api.openai.com"}
  anthropic:  {kind: intelligence, endpoint: "https://api.anthropic.com"}

intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  token: "{{secret:OPENAI_API_KEY}}"
  providers:
    anthropic:
      endpoints: https://api.anthropic.com
      dialect: anthropic
      token: "{{secret:ANTHROPIC_API_KEY}}"
  models:
    smart: {model: gpt-5.1}
    deep:  {model: claude-opus-5-5, provider: anthropic, kind: reasoning}
  roles: {turn: smart, think: deep}
```

An uncatalogued endpoint is refused at load and the message names it. See
[Harden a deployment](harden.md) for the rest of the egress policy.

## Change models without a restart

Everything under `intelligence` reloads live: send `SIGHUP`, or run with
`--watch-config` and edit the file. Each turn and step that starts after the
reload uses the new settings. A running subagent is handed its model again
and switches at its next turn boundary; a model call in flight is never cut.
`intelligence.swap_policy` (flag `--model-swap`) says what that subagent
does with a turn caught by a model change: `finish-on-old` (the default)
finishes it on the old model, `restart-turn` discards it and runs it again
on the new one.

## Common errors

| Symptom | Cause | Fix |
|---|---|---|
| `intelligence HTTP 404` | the URL has a partial path, such as `…/v1` | write the full path, `…/v1/chat/completions` |
| `plaintext http:// intelligence is allowed for loopback only` | `http://` to a remote host | use `https://` |
| `… is not set in the environment` at validation | a `{{secret:NAME}}` with no variable | export the variable |
| `intelligence.token carries an inline credential` | a key written in the file | use `"{{secret:NAME}}"` |
| `dialect: bedrock requires intelligence.auth.kind = aws (SigV4)` | Bedrock with no `aws` block | add `auth: {kind: aws, …}` |
| `intelligence HTTP 401` at the first call, exit 4 | a wrong or missing key, or no login yet | check the key; run `agentd --login …` |
| every turn ends after one answer, no tools called | the model or server ignores `tools` | enable tool calling, or use a model that has it |

## Next steps

- [Choose models for each job](models-and-roles-guide.md): bind roles,
  reasoning and decision models, and per-turn routing.
- [Control spend with budgets](budgets.md): cap tokens per run, per window
  and for the instance's lifetime.
- [Models, providers and roles](intelligence.md): how agentd resolves a
  model, fails over and falls back.
- [Intelligence wire reference](intelligence-reference.md): what agentd
  sends to each dialect, and every flag and environment variable.
