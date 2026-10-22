# Harden a deployment

This guide turns agentd's [security model](security.md) into configuration:
a checklist, one complete hardened configuration that validates, and the
reasoning, variations and traps for each item. It is for operators preparing
an agent that reads outside input, holds real credentials, or is reachable
from anywhere but one developer's machine. When you finish, your
configuration refuses the dangerous shapes at load, before any side effect.

## The checklist

| Do this | Because |
|---|---|
| [Run agentd in a sandbox](#run-agentd-in-a-sandbox) with an egress policy and resource limits | agentd has no in-binary sandbox |
| [Tag every MCP server](#tag-every-mcp-server), one server per tag profile | the Rule of Two judges servers by their tags |
| [Catalog every service and close egress](#catalog-every-service-and-close-egress) | tags become a floor and the endpoint list an allow-list |
| [Keep outside input away from the actors](#keep-outside-input-away-from-the-actors) | a run that reads a stream is judged with everything it can reach |
| [Put policies on risky calls](#put-policies-on-risky-calls) | a verdict on the call and its arguments |
| [Keep approvals with people](#keep-approvals-with-people) | an approval is the operator's decision, not the agent's |
| [Lock the A2A listener](#lock-the-a2a-listener) | without a credential every local process is the operator |
| [Authenticate every webhook route](#authenticate-every-webhook-route) | a route answers whoever reaches it |
| [Leave exec off](#leave-exec-off) | a local command runner is `sensitive` + `egress` on its own |
| [Write every secret as a reference](#write-every-secret-as-a-reference) | validation catches only the shapes it knows |
| [Pin where the instruction comes from](#pin-where-the-instruction-comes-from) | the instruction is the agent's standing policy |
| [Freeze workflow definitions](#freeze-workflow-definitions) | the agent cannot rewrite its own machinery |
| [Record an audit trail](#record-an-audit-trail) | who did what, on whose authority |
| [Validate in CI](#validate-in-ci) | the same check startup runs, before you deploy |

## A hardened configuration

This is an issue-triage agent that reads a public tracker and its docs,
labels issues and comments on them. It holds two legs of the trifecta
(`untrusted_input` and `egress`), dials only catalogued endpoints, asks a
person before it posts a comment, names the model any approval judge would
run on, serves people over TLS with the device grant, and keeps an audit
trail.

```yaml
# needs: triage.md
agent:
  name: triage
  instruction:
    file: triage.md
  approval: ask
  ask_human_unowned: gate

intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  token: "{{secret:OPENAI_API_KEY}}"
  models:
    smart: { model: gpt-5.1 }
  roles:
    turn: smart
    approve: smart

services:
  openai:
    kind: intelligence
    endpoint: https://api.openai.com/v1/chat/completions
  tracker:
    endpoint: https://mcp.tracker.example.com/mcp
    tags: { "*": [untrusted_input, egress] }
    auth: { kind: static, token: "{{secret:TRACKER_TOKEN}}" }
    allow: ["get_*", "list_*", "search_*", "add_comment", "set_labels"]
  docs:
    endpoint: https://mcp.docs.example.com/mcp
    tags: { "*": [untrusted_input] }

mcp:
  servers:
    - { name: tracker, service: tracker, ns: tracker }
    - { name: docs, service: docs, ns: docs }

security:
  egress: closed
  policies:
    - match: { tool: "tracker.add_comment" }
      action: ask
      question: "Post this comment? {{args}}"
      timeout: 30m
      on_timeout: deny
    - match: { tags: [egress], caller: [subagent] }
      action: deny
  workflows:
    immutable: true

a2a:
  listen: https://0.0.0.0:8443
  url: https://triage.example.com:8443
  bearer: "{{secret:AGENTD_OPERATOR_TOKEN}}"
  tls:
    cert: /etc/agentd/tls/server.crt
    key: /etc/agentd/tls/server.key
  device_grant:
    enabled: true
    scopes: [user]
    token_ttl: 8h

store:
  kind: file
  file: { path: /var/lib/agentd/triage }

observability:
  audit:
    sink: [log, store]
```

Save the agent's instruction as `triage.md` in the directory you start
agentd from (a relative `file:` path resolves against the working directory),
set the three referenced secrets in the environment, and validate:

```sh
agentd --validate-config --config agentd.yaml
```

The last line of the output is the verdict:

```text
{"event":"config.valid","files":["agentd.yaml"]}
```

Before it, one `config.effective_server` line per MCP server shows the
endpoint, tags and tool ceiling each server ended up with after the catalog
was applied. The TLS files are read when the listener starts, not at
validation.

## Run agentd in a sandbox

agentd is not the security boundary; the environment around it is. agentd
has no seccomp profile, no namespaces and no chroot of its own, so give it
those from the platform:

- run it as a non-root user, with a read-only root filesystem and only the
  store directory writable, all capabilities dropped and privilege escalation
  off ([the Kubernetes example](../examples/k8s/deployment-reactive.yaml) has
  the security context);
- set memory and process limits on the container or unit, and consider
  `security.cgroup`, which puts each child process agentd spawns in its own
  cgroup leaf, with optional memory and process limits, that agentd can kill
  as one ([Supervision and containment](harness.md));
- put an egress policy (a NetworkPolicy or firewall) in front of the process.
  `security.egress: closed` governs the dials agentd makes from configuration;
  it does not stop an MCP server, or a command, from reaching anywhere else.

Treat every MCP server you declare as code you run at agentd's privilege, and
vet it as you would a dependency.

## Tag every MCP server

Tag each server with every leg it carries: `untrusted_input` if it returns
text strangers wrote, `sensitive` if it reaches private data or privileged
systems, `egress` if it can send data out or change something outside. Any
two legs may sit in one agent; all three refuse to load.

Three traps:

- **Glob keys do not split a server.** Tags apply to every tool of the
  server. `{"read_*": [untrusted_input], "send_*": [egress]}` makes both tools
  carry both tags. To keep a reader and a sender apart, declare them as two
  servers.
- **An untagged server counts as `untrusted_input`.** Leaving tags off is not
  neutral: the server is a leg in the check, and its results taint the work
  that reads them, so policies on `tainted`, the person's consent rules and
  payment approvals all apply to it. Tag a server that carries no leg
  `trusted` (`tags: {"*": [trusted]}`) once you have judged that nobody
  outside writes what it answers.
- **The budget is the whole instance.** The load folds every declared server
  together. An instance with a reader, a secrets server and a sender does not
  start, even if you meant to hand each to a different subagent. Split it into
  separate agentd instances per risk profile.

When all three legs are present the load says so and exits 2:

```text
{"event":"config.invalid","msg":"lethal-trifecta refused: the root grant wires untrusted_input + sensitive + egress into one agent; narrow the tags or set security.allow_trifecta (audited)"}
```

`security.allow_trifecta: true` lifts every trifecta check at once and emits
nothing when it is used. Do not set it to get past this message; split the
instance instead.

`tools.narrow.<tool>.tags` adds a tag to a single tool (it can never remove
one). Those per-tool tags reach [policies](#put-policies-on-risky-calls), not
the load-time check.

## Catalog every service and close egress

Put every external service in `services:`, with its endpoint, its tags, its
one credential and its tool ceiling, and have servers reference entries by
`service:`. Then set `security.egress: closed`.

The catalog makes tags a floor. Any MCP declaration whose endpoint matches an
entry gets the entry's tags added before the Rule-of-Two check, including one
an instruction document or a subagent template declares inline, so nobody can
under-tag a catalogued system. The entry's `allow` and `exclude` are a
ceiling a consumer can narrow and never widen.

Closed egress makes the catalog an allow-list. Every endpoint agentd dials
from configuration must match an entry of its kind: `mcp` for MCP servers,
`intelligence` for model endpoints and every provider, `peer` for A2A peers,
and `http` for `http` steps, stream `forward` targets, workflow `url:`
references, the HTTP store, A2A push targets and an instruction fetched by
`url:` or `oci:`. A literal endpoint is judged at load. Every `http` step URL
is checked again at the dial, where a matching entry's `methods` list is also
a ceiling.

The trap is the model endpoint: it needs an `intelligence` entry like any
other destination, or the load refuses it.

```text
{"event":"config.invalid","msg":"security.egress is closed and intelligence endpoint (https://api.openai.com/v1/chat/completions) matches no `kind: intelligence` services: catalog entry — catalog the endpoint to allow it"}
```

Closed egress does not cover `observability.otel.endpoint` (the load warns),
or credential plumbing: OAuth token and discovery endpoints, AWS metadata and
STS, and the AAuth provider. The built-in `mock:` test endpoint opens no
socket and needs no entry.

## Keep outside input away from the actors

A webhook or A2A route with `into:` appends a caller's text to a stream, and
every run that reads the stream carries that text. agentd judges such a run
with everything its model-driven steps can reach, and the runs it starts, and
refuses the configuration when they reach `sensitive` and `egress` together.
The pattern that passes: read the outside text with a step that has no tools,
and act with a deterministic step whose target is fixed.

```yaml
agent:
  instruction: "Acknowledge customer mail."

intelligence:
  endpoints: ["mock:final"]

mcp:
  servers:
    - name: mail
      endpoint: https://mcp-mail.internal.example.com/mcp
      tags: { "*": [sensitive, egress] }

webhooks:
  listen: http://127.0.0.1:9450
  default_auth:
    hmac: { secret: "{{secret:HOOK_SECRET}}" }

streams:
  inbox: {}

workflows:
  - name: intake
    steps:
      hook:
        kind: webhook
        path: /hooks/mail
        into: { stream: inbox, subject: mail.received }

  - name: triage
    steps:
      take: { kind: stream, stream: inbox, subject: "mail.*" }
      read:
        kind: extract
        depends_on: take
        input: "{{ steps.take.output.data }}"
        output_schema:
          type: object
          required: [sender, topic]
          properties:
            sender: { type: string }
            topic: { enum: [billing, outage, other] }
      ack:
        kind: mcp.tool
        depends_on: read
        server: mail
        tool: send_template
        args:
          to: "{{ steps.read.output.sender }}"
          template: "ack-{{ steps.read.output.topic }}"
      done: { kind: finish, depends_on: [ack], status: completed }
```

This sample uses `mock:final`, the built-in test endpoint, so it validates
without a model account. The `extract` step reads the mail with no tools, and
its `output_schema` constrains what reaches the `mcp.tool` arguments. Replace
`read` and `ack` with one `agent` step, `reply`, that holds `servers: [mail]`,
and the load refuses it, naming the stream, what feeds it and the server that
brought the other two legs:

```text
{"event":"config.invalid","msg":"workflow \"triage\": lethal-trifecta refused — it consumes stream \"inbox\" (fed by webhook `into:` at workflow \"intake\" step \"hook\"); it is run by workflow \"triage\" step \"reply\" (through workflow.run), and its model-driven steps reach mcp server \"mail\" [sensitive, egress]: untrusted input + sensitive + egress in one run. …"}
```

The message also shows a second trap: an `agent` step can call
`workflow.run`, `workflow.signal`, `subagent.run`, `subagent.send` and
`message.send`, and the check counts what those reach. A `tools:` list on the
step that leaves them out narrows its reach, though it cannot help a step
that itself holds both other legs, as `reply` does.

A context that holds `sensitive` and `egress` and reads such a run's result
back (`workflow.status`, a synchronous workflow tool, `subagent.await`) gets
the status with a marker in place of the text, and agentd logs
`readback.withheld`. That is expected; it is the same rule applied on the way
back.

## Put policies on risky calls

`security.policies` is an ordered list of verdicts on individual calls. The
first matching rule wins, and no match is allow. Use it for what tags and
grants cannot say: an argument condition, a person's sign-off, or a rule for
one caller.

```yaml
# fragment: under security:
policies:
  # Never delete outside /tmp (a CEL guard over the call's arguments).
  - match: { tool: "fs.delete*", args: "!args.path.startsWith('/tmp/')" }
    action: deny
  # A person signs off on anything a subagent sends out.
  - match: { tags: [egress], caller: [subagent] }
    action: ask
    question: "{{caller}} wants {{tool}} with {{args}}. Allow?"
    timeout: 15m
    on_timeout: deny
  # Signed-in users never reach billing tools through the model.
  - match: { tool: "billing.*", principal: "user:*" }
    action: deny
  # Refunds go to the finance operators, not to whoever is on duty.
  - match: { tool: "billing.refund" }
    action: ask
    to: { role: operator, labels: { team: finance } }
    on_timeout: deny
```

How rules match:

- `tool` is a glob over the tool's registered name. MCP tools keep their bare
  name unless the server sets `ns:`, so `fs.delete*` needs `ns: fs` on that
  server. Set `ns` on every server you write policies for.
- `tags` must all be present on the tool; `caller` is `root`, `workflow` or
  `subagent`; `principal` is a glob over the principal id (`user:*`,
  `agent:ci-bot`) of calls made on a principal's behalf.
- `args` is a CEL expression over `args`, `tool` and `caller`. A guard that
  does not compile refuses the load, and one that fails at call time refuses
  the call, never "no match". The release binaries include the `cel` feature.
- `to:` must name someone who can see the task: leave it out for the
  operators, or narrow to operators by the labels their principal rule
  carries. A `to:` naming a user or agent role is refused at load.

The verdicts: `ask` holds the call until a person decides, and an `ask`
nobody can be asked (no listener, or work nobody owns) takes `on_timeout`,
logged as `tool.policy.unanswerable`. `shadow` refuses and says the call was
held, without inventing a result. Policies apply to every caller, subagents
included. They live in the `security` section, which is restart-only.

## Keep approvals with people

`agent.approval` decides who answers the asks the agent itself makes with
`ask_human`. Leave it at `ask`, the default, so a person decides.

- `auto` hands each ask to a model judge on the `intelligence.roles.approve`
  model, and `accept` takes the ask's recommended answer. Neither touches an
  addressed ask (`to:`) or a policy `ask`, and no judge answers an ask whose
  run, subagent or peer conversation carries outside input: that ask fails and
  agentd logs `human.judge.withheld`.
- If you do use `auto`, bind `roles.approve` to a model you chose. The role is
  operator-only, so a served document cannot pick its own judge.
- `ask_human_unowned: gate` puts asks from work nobody owns (schedules,
  webhooks, subagents) on the listener for an operator, instead of taking the
  `ask_human_fallback`. It needs an A2A listener.
- The operator can change `agent.approval` at runtime with `admin.set`; the
  next reload restores the file's value. Anyone who reaches a no-credential
  loopback listener is the operator, which is one more reason to
  [lock the listener](#lock-the-a2a-listener).

[Human in the loop](human-in-the-loop.md) covers asks, gates and answering
them.

## Lock the A2A listener

Choose the listener by who calls it.

| Callers | Listener |
|---|---|
| processes on this host, one user | `unix:///run/agentd/<name>.sock`: the kernel admits only the daemon's own uid |
| people, from anywhere | `https://` with `a2a.tls`, `a2a.bearer` for the approving operator, and `a2a.device_grant` |
| services and agents with certificates | `https://` with `a2a.tls.client_ca` and certificate principal rules |
| development, one person | `http://127.0.0.1:<port>`, which loads with a warning that every local process is the operator |

The hardened configuration above serves people. For services, require a
client certificate and name callers by it:

```yaml
# fragment: under a2a:
listen: https://0.0.0.0:8443
url: https://triage.example.com:8443
tls:
  cert: /etc/agentd/tls/server.crt
  key: /etc/agentd/tls/server.key
  client_ca: /etc/agentd/tls/clients-ca.pem
principals:
  - { match: { san: "spiffe://acme/ops/deployer" }, role: operator }
  - { match: { san: "spiffe://acme/*" }, role: user, grants: [workflow.run] }
  - id: ci-bot
    match: { bearer_ref: "{{secret:CI_TOKEN}}" }
    role: agent
    labels: { team: platform }
    quotas: { rate: "20/1m" }
```

The load enforces the edges: a non-loopback bind needs `a2a.bearer` or
`a2a.tls.client_ca`, plaintext is loopback-only, a wildcard bind needs
`a2a.url`, the device grant needs an operator credential to approve with, and
a certificate rule needs `client_ca`. The traps:

- **Certificate rules are first-match, in the order written.** Put the narrow
  rule first; swap the two `san` rules above and the deployer is a user.
- **`client_ca` makes a certificate mandatory for every caller**, bearer-only
  and browser clients included, so the device grant refuses to load beside
  it. An instance has one listener: serve people and certificate-holding
  services from separate instances.
- **Never put a reverse proxy in front of a no-credential loopback
  listener.** The proxy's relayed requests are local and present nothing, so
  each one is the operator.
- **List browser origins exactly** in `a2a.cors.origins`; `*` is refused.
- **Push notifications are off** until `a2a.push.enabled`, and a push target
  on a private address needs `a2a.push.allow_private` as well. Leave both off
  unless a caller needs them.
- **Retention is shared.** On a listener several callers use, bound
  `store.retention.tasks` with `ttl`, so one caller's finished tasks cannot
  push out another's.

[Connect agents over A2A](connect-agents.md) walks through sign-in and
delegation.

## Authenticate every webhook route

A non-loopback `webhooks.listen` refuses to start while any route is
unauthenticated. Set `webhooks.default_auth`, or `auth` on each `webhook`
step, and prefer HMAC over the raw body:

```yaml
# fragment: under webhooks:
listen: https://0.0.0.0:9443
url: https://hooks.example.com
tls: { cert: /etc/agentd/tls/hooks.crt, key: /etc/agentd/tls/hooks.key }
default_auth:
  hmac:
    secret: "{{secret:GITHUB_WEBHOOK_SECRET}}"
    header: X-Hub-Signature-256
    prefix: "sha256="
```

agentd computes HMAC-SHA256 only. A sender that signs another way (Stripe's
timestamped scheme, Twilio's HMAC-SHA1 over the URL) needs a small relay that
verifies its scheme and signs again. Give each route a `rate`, and route
outside text into a stream with `into:` when it should wait for a consumer;
the stream then carries the taint described
[above](#keep-outside-input-away-from-the-actors).

A served instruction document opens a webhook route only when the operator
grants `interface` in `agent.document_capabilities`. Leave that grant out
unless the document is meant to define the agent's public surface.

## Leave exec off

The `exec` tool runs local commands only in a binary built with the `exec`
feature **and** with `security.exec.enabled: true`. The release binaries and
the image do not include the feature, so the safest posture is the default.
To run commands, prefer mapping `exec` onto an MCP server that runs them in
its own sandbox (`tools.overrides`), and tag that server.

If you build with the feature and enable the local runner, fence it:

```yaml
# fragment: under security:
exec:
  enabled: true
  allow: [git, ls, cat]     # exact argv[0] matches; empty runs nothing
  workdir: /workspace       # a requested cwd must resolve inside it
  timeout: 30s              # a request may shorten it, never extend it
  max_output: 1048576       # bytes of stdout and stderr kept
  env: [PATH, HOME]         # the only variables the command receives
```

The local runner counts as `sensitive` + `egress`, so beside any
untrusted-input server the load refuses it. Never allow-list a shell or an
interpreter: the guard is argv-not-shell, and `bash` puts the whole injection
surface back. A misconfiguration surfaces as an error at the first call, not
at startup.

## Write every secret as a reference

Write every credential as `{{secret:NAME}}` (an environment variable) or
`{{secret-file:PATH}}` (a mounted file, re-read at each use, so rotation
needs no restart). A reference that does not resolve is exit 2 at load,
naming the reference.

Validation refuses an inline credential in the paths it knows:

```text
{"event":"config.invalid","msg":"mcp server 'tracker' header 'X-Api-Key' looks like a credential but has an inline value; use a {{secret:…}} reference"}
{"event":"config.invalid","msg":"config file: intelligence.token carries an inline credential; use {{secret:NAME}} / {{secret-file:PATH}} (or set it from env/flag)"}
```

The paths it knows are the model and provider tokens, `a2a.bearer`, the
AAuth enrollment token, every `auth:` block's `token`, `value` and
`client_secret`, an MCP server's `oauth.client_secret`, and header keys that
look like credentials. Two traps:

- **A header named something else is not checked.** A bearer pasted into
  `X-Session` passes.
- **Webhook route secrets and a principal's `bearer_ref` are not checked.**
  Write them as references yourself.

Tools that help:

- `--env <FILE>` loads a dotenv file into the process environment (the real
  environment wins).
- `--prompt-missing` asks for each missing `{{secret:NAME}}` on the terminal,
  echo off, and keeps the answers in memory only.
- `agentd --effective-config --config agentd.yaml` prints the assembled
  configuration with where each value came from, credentials redacted.

[Outbound credentials](authentication.md) covers OAuth, AWS and SPIFFE
credentials that replace static tokens.

## Pin where the instruction comes from

The instruction decides what the agent does, so verify who wrote it, and,
for an OCI artifact, who pushed it and which bytes:

```yaml
# fragment: under agent:
instruction:
  oci:
    ref: ghcr.io/acme/triage-agent@sha256:4f1c0de6a3b5e9d27c8a1f3e6b0d9c2a7e5f8b1d4c6a9e2f7b3d0c5a8e1f4b29
    cosign_key: /etc/agentd/keys/acme-cosign.pub
  trust:
    - publisher: acme
      author_keys: [/etc/agentd/keys/acme-author.pub]
      max_capabilities: [material, knowledge]
document_capabilities: [material, knowledge]
```

- **A digest** (`@sha256:…`) pins the exact artifact, and removes the
  mutable-tag question for less than any signature check costs.
- **`cosign_key`** verifies who pushed the artifact.
- **`trust`** verifies who wrote the document: with a publisher pinned, an
  unsigned document, or one signed by anyone else, is refused at load.
- **`document_capabilities`** is what the document may activate beyond the
  default rung, and a signature can only narrow it (`max_capabilities` and
  what the author attested cap it further). Grant the fewest families the
  document needs.

An unsigned document under a pin is refused before anything reads it:

```text
agentd: config file agentd.yaml: agent.instruction: the instruction carries no front-matter `signature:`, but 1 pinned publisher(s) are configured — an unsigned document is not from a pinned publisher (§7.6)
```

Under `egress: closed`, the registry must match an `http` catalog entry
before agentd fetches the artifact. The pull happens at load, so the OCI
sample above needs the registry reachable to validate.
[Write and ship instruction documents](instruction-documents.md) shows how to
sign a document and push it.

## Freeze workflow definitions

`security.workflows.immutable: true` refuses `workflow.create`,
`workflow.update` and `workflow.delete` at runtime, for the model, subagents
and operators alike. Definitions still load from configuration, files,
folders and URLs. Set it when your workflows are reviewed code and the agent
should not write new ones.

Without it, a definition the agent stores is still held to the Rule of Two
and to the `interface` grant for routes, but it is machinery the agent wrote.

## Record an audit trail

`observability.audit.sink` turns on the audit record: every A2A call, every
principal-driven tool call and command, configuration reloads, restores,
store conflicts and kills, each as
`{ts, principal, role, action, target, outcome, request_id, trace, instance}`
plus the session id for a signed-in caller.

| Sink | Where it goes |
|---|---|
| `log` | an `audit` line in the JSON log |
| `store` | an append-only record in the durable store, never overwritten; needs a store |
| `stream` | the declared stream `observability.audit.stream` names, which a workflow can consume or forward |

Use `stream` to ship the trail off the box. A failed store write is logged as
`audit.store.fail` and never fails the audited action.
[Monitor in production](observability.md) covers the log and metrics around
it.

## Validate in CI

Run the same validation startup runs on every change to the configuration:

```sh
agentd --validate-config --config agentd.yaml
```

It reports every problem in one pass and exits 2 on any, before any side
effect: the Rule of Two, closed egress, inline credentials, unresolved
references (set the referenced variables in the CI job, with dummy values if
need be), policy guards, addressees and listener rules. Workflows fetched
from `url:` and `uri:` are judged when they resolve at startup, so keep
reviewed workflows in files beside the configuration where CI can see them.

## Next steps

- [Security model](security.md): why each of these controls exists and what it
  does not cover.
- [Outbound credentials](authentication.md): authenticate to models, MCP
  servers and peers without static secrets.
- [Deploy](deployment.md): run the hardened configuration on Kubernetes,
  systemd or a container.
- [Configuration reference](configuration.md): every key used here.
