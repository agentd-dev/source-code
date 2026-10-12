# Author workflows

This guide is for builders who write workflows. It shows how to start them
from a schedule, a webhook or an event, pass data between steps, branch and
guard, fan out over a list, call remote systems safely, wait for things, and
test the result before it touches anything real. Every complete sample
passes `agentd --validate-config`. For how runs work underneath, read
[Workflows: how runs work](workflows.md); for every field of every kind, the
[Workflow node reference](node-registry.md).

## The shape of a workflow

A workflow lives under `workflows:` in a configuration file. It has a
`name` and `steps`; each step has a `kind`, and every step but a start node
names what it `depends_on`. One step must be a `finish`.

```yaml
# A runnable workflow. "mock:final" is agentd's built-in test endpoint:
# it answers every model call with "mock-llm done", offline.
intelligence:
  endpoints: "mock:final"
workflows:
  - name: brief
    limits: {deadline: 5m}
    steps:
      start:
        kind: once
        inputs: {topic: "our next release"}
      draft:
        kind: agent
        depends_on: [start]
        instruction: "Write three bullet points about {{ inputs.topic }}."
      done:
        kind: finish
        depends_on: [draft]
        output: "{{ steps.draft.output }}"
```

Save it as `brief.yaml`, check it, and run it:

```sh
agentd --validate-config --config brief.yaml
agentd --config brief.yaml
```

The run's output goes to stdout and the telemetry to stderr:

```
mock-llm done
```

A `once` start fires when the process starts, so this configuration is a
job: it exits when the run ends, with an exit code that says how it ended
(see [CLI, environment and exit codes](cli.md)). With no `store`
configured, a one-shot job keeps its state in memory and agentd warns that
it is not durable. A configuration with a long-lived trigger, such as a
schedule or a webhook, keeps the process running and uses the local file
store by default.

> **Note.** `--validate-config` resolves every `{{secret:NAME}}` reference,
> so export the variables a sample names before you validate it. It checks
> the document, not the world: a `tool` step naming a tool that does not
> exist, or an `mcp.tool` step on a server that is down, is found at
> startup, which exits `2`.

## Choose a trigger

A start node decides when a run begins. Use the one that matches where the
work comes from:

| Work arrives as | Start kind | Keeps the process running |
|---|---|---|
| process start, once | `once` | no |
| a call from the agent, another workflow or an A2A client | `manual` | no |
| a fixed cadence | `schedule` (`cron`, `every` or `at`) | yes |
| "again when the last run finished" | `loop` | yes |
| an HTTP request | `webhook` | yes |
| an event on a declared stream | `stream` | yes |
| a set of related events | `correlate` | yes |
| a typed A2A command | `a2a` | yes |
| a named signal | `signal` | yes |
| an MCP resource change | `subscribe` | yes |
| a run finishing, a person being asked, shutdown | `event` | yes |

This configuration has three workflows: a weekday schedule, a consumer of a
stream, and a watcher that reacts when the first one fails.

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
streams:
  orders: {}
workflows:
  - name: morning-digest
    steps:
      tick: {kind: schedule, cron: "0 7 * * 1-5"}
      brief:
        kind: agent
        depends_on: [tick]
        instruction: "Summarise yesterday's open incidents."
      done: {kind: finish, depends_on: [brief], output: "{{ steps.brief.output }}"}
  - name: on-order
    steps:
      order: {kind: stream, stream: orders, subject: "order.created"}
      check:
        kind: classify
        depends_on: [order]
        input: "{{ steps.order.output.data }}"
        classes: [normal, suspicious]
      done: {kind: finish, depends_on: [check], output: "{{ steps.check.output.class }}"}
  - name: after-digest
    steps:
      finished:
        kind: event
        on: workflow.failed
        filter: 'payload.workflow == "morning-digest"'
      tell:
        kind: emit
        depends_on: [finished]
        note: "The morning digest failed: {{ steps.finished.output.payload.status }}"
      done: {kind: finish, depends_on: [tell]}
```

Things the field names do not tell you:

- `cron` is read in UTC. A missed occurrence is skipped, never replayed.
  `at` is a delay, not a wall-clock time.
- An `event` start fires on `workflow.finished`, `workflow.failed`,
  `human.asked`, `human.answered` and `lifecycle.shutdown`. Its output is
  wrapped, `{event, payload}`, so downstream steps read
  `steps.<id>.output.payload.*`, while its `filter` sees `payload` directly.
  A workflow never fires on its own completion.
- A `stream` start resumes from a durable offset after a restart. It starts
  at new events unless you set `from: earliest`, and it never fires on events
  its own workflow emitted.

### Receive a webhook

A `webhook` start needs a listener. A loopback `http://` bind is allowed for
development; any other bind must be `https://` with `webhooks.tls`.

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
webhooks:
  listen: http://127.0.0.1:8088
workflows:
  - name: on-ci
    concurrency: {max_runs: 8, on_overflow: queue}
    inputs:
      schema:
        type: object
        required: [build]
        properties: {build: {type: string}}
    steps:
      hook:
        kind: webhook
        path: /hooks/ci
        methods: [POST]
        auth: {hmac: {secret: "{{secret:HOOK_SECRET}}", prefix: "sha256="}}
        inputs: {build: "{{ payload.body.build_id }}"}
      look:
        kind: http
        depends_on: [hook]
        url: "https://ci.example.com/api/builds/{{ inputs.build }}"
        headers: {Authorization: "Bearer {{secret:CI_TOKEN}}"}
      note:
        kind: summarize
        depends_on: [look]
        input: "{{ steps.look.output.json }}"
        length: three sentences
      done:
        kind: finish
        depends_on: [note]
        output: "{{ steps.note.output.summary }}"
```

The start node's `inputs` mapping is rendered against `payload` (the
request's `method`, `path`, `headers`, `body` and `raw_body`) and checked
against the workflow's `inputs.schema`. Send a signed request:

```sh
body='{"build_id":"b-123"}'
sig=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$HOOK_SECRET" | sed 's/.* //')
curl -s http://127.0.0.1:8088/hooks/ci -H "X-Signature: sha256=$sig" -H 'content-type: application/json' -d "$body"
```

```
{"run_id":"on-ci-01M4P4004VPT9M256ZXBGDGV3C","status":"accepted","trace_id":"f281fdc06f4dd1374e53be619f96e1fe","workflow":"on-ci"}
```

The route answers `202` once the run is durably queued, and `401` for a bad
signature. The answer names the run and the trace it is in, and its
`traceparent` header names the run's span. The run can still be refused
after the `202`, by the workflow's `concurrency` or its `inputs.schema`;
the `run.dropped` or `run.inputs.invalid` line then names that `run_id`. A repeated `Idempotency-Key`
header is answered as a duplicate, naming the same run, and starts nothing. `rate: "20/1s"` bounds how fast requests are admitted
(`429` with `Retry-After` past it), and `parallelism` how many run at once.

## Pass data between steps

Every field of a step is rendered against the run's data right before the
step runs. A step's result is at `steps.<id>.output`, never at its bare id:

| Write | Reads |
|---|---|
| `{{ inputs.order_id }}` | the run's validated inputs |
| `{{ steps.fetch.output.json.items }}` | a field of an earlier step's output |
| `{{ steps.fetch.status }}`, `.error` | how that step ended |
| `{{ vars.score }}` | a run variable an `assign` wrote |
| `{{ memory.feature_flags }}` | a durable memory key, read when the step renders |
| `{{ env.idempotency_key }}` | this step's retry-stable key |
| `{{ item }}`, `{{ index }}` | the current element inside a `foreach` body |

Three rules decide what you get:

- A string that is exactly one placeholder yields the typed value: an
  object stays an object, a number a number. Anything else interpolates
  into a string.
- `{{ path | fallback }}` supplies a value when the path is missing. A
  missing path with no fallback fails the step. A step that was pruned or
  skipped has `output: null`, which is present, so the fallback does not
  apply; test `steps.<id>.status` instead.
- A string that starts with `CEL:` is a CEL expression over the same names:
  `"CEL: inputs.amount > 500 ? 'review' : 'auto'"`.

A path's step is checked when the configuration loads: one naming a step
the workflow does not have is refused. What follows the step id is read at
run time, so a typo there fails the step when it runs. The [workflow language reference](workflow-language.md) has
the full rules, including the JSON-pointer form for ids with dots in them.

## Branch and guard

Use a `switch` to branch on a value you already have, a CEL `when` to skip a
step on a condition, and a decision guard when the condition is a judgement
about text. This support workflow does all three. Its decision guard runs on
a decision model; with none configured, the turn model answers it through
structured generation.

```yaml
intelligence:
  providers:
    anthropic:
      endpoints: https://api.anthropic.com
      dialect: anthropic
      token: "{{secret:ANTHROPIC_API_KEY}}"
    openrouter:
      endpoints: https://openrouter.ai/api/alpha/decisions
      dialect: decisions
      token: "{{secret:OPENROUTER_API_KEY}}"
  models:
    smart: {model: claude-sonnet-5, provider: anthropic}
    fast:  {model: claude-haiku-4-5, provider: anthropic}
    jev:   {kind: decision, provider: openrouter, model: typesafe/jev-1.13, escalate: smart}
  roles: {turn: smart, decide: jev}
a2a:
  listen: http://127.0.0.1:8420
  bearer: "{{secret:A2A_BEARER}}"
workflows:
  - name: support
    inputs:
      schema:
        type: object
        required: [ticket, amount]
        properties: {ticket: {type: string}, amount: {type: number}}
    steps:
      start: {kind: manual}
      triage:
        kind: classify
        depends_on: [start]
        input: "{{ inputs.ticket }}"
        classes: {billing: "payment problems", tech: "bugs and outages"}
      route:
        kind: switch
        depends_on: [triage]
        on: "{{ steps.triage.output.class }}"
        cases: {billing: billing, tech: tech}
      billing:
        kind: agent
        depends_on: [route]
        model: fast
        instruction: "Answer this billing question: {{ inputs.ticket }}"
      tech:
        kind: agent
        depends_on: [route]
        instruction: "Diagnose this report: {{ inputs.ticket }}"
      refund:
        kind: agent
        depends_on: [billing]
        instruction: "Refund the order in this ticket: {{ inputs.ticket }}"
        when:
          noul: "The customer explicitly asks for their money back."
          state: "{{ inputs.ticket }}"
          confidence: 0.8
      large:
        kind: emit
        depends_on: [refund]
        when: "inputs.amount > 500"
        audit: {refund: "{{ inputs.amount }}"}
      done:
        kind: finish
        depends_on: [billing, tech, refund, large]
        output:
          queue: "{{ steps.triage.output.class }}"
```

How it behaves:

- The `switch` runs the case that matches and **prunes** the other. A
  pruned step does not satisfy its dependents, so everything that depends
  only on it is pruned too: when `tech` is chosen, `refund` and `large`
  fall away with `billing`.
- `done` depends on every arm. It runs once all of them have ended, as long
  as at least one is not pruned. A `finish` that depends only on a pruned
  branch is pruned itself, and the run ends `stalled`.
- The decision guard on `refund` becomes a step of its own, `refund.when`,
  which you can see in the run and the logs. `refund` runs only if the
  statement holds and the answer is at least 0.8 sure. An unsure answer is
  asked again on `jev`'s `escalate` model, and if it is still unsure the
  step is pruned. A guard never fails the run for doubt.
- `cases` values name one step each, as a string. A list is refused at load.

Use `route` or `classify` when the branch depends on meaning, and `switch`
when it depends on a value. `on_error: "goto:<step>"` and a wait's
`on_timeout` are routing edges too: they run their target even though its
dependencies have not finished.

## Fan out over a list

`foreach` runs a body once per element, with bounded parallelism, and
collects the results in order.

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
workflows:
  - name: dependency-audit
    steps:
      weekly: {kind: schedule, cron: "0 6 * * 1"}
      list:
        kind: http
        depends_on: [weekly]
        url: https://api.github.com/orgs/example/repos
        headers: {Authorization: "Bearer {{secret:GITHUB_TOKEN}}"}
      each:
        kind: foreach
        depends_on: [list]
        over: "{{ steps.list.output.json }}"
        as: repo
        batch: {size: 5, parallel: 3, rate: "10/1s"}
        on_error: continue
        collect: {into: reviews, mode: append}
        body:
          steps:
            manifest:
              kind: http
              url: "https://api.github.com/repos/{{ repo.full_name }}/contents/Cargo.toml"
              headers: {Authorization: "Bearer {{secret:GITHUB_TOKEN}}"}
            check:
              kind: judge
              depends_on: [manifest]
              input: "{{ steps.manifest.output.json }}"
              rubric: "Every pinned dependency is on a current major version."
      report:
        kind: summarize
        depends_on: [each]
        input: "{{ steps.each.output }}"
      done: {kind: finish, depends_on: [report], output: "{{ steps.report.output.summary }}"}
```

- `over` takes an array (an object becomes `[{key, value}]`). `as` names
  the element; `item` and `index` are always available too.
- The output is an array as long as the input, in input order. With
  `on_error: continue`, an element that failed fills its slot with
  `{index, error}` instead of failing the step.
- `batch.parallel` defaults to 4, and asking for more than
  `limits.workflow.fan_out` (8 when unset) is refused at load. `rate`
  paces how fast elements start.
- Inside the body, `steps.<sibling>` reads the current element's sibling.
  The logs show each instance under a scoped id such as `each[3].check`.

Use `batch` instead when the body should see groups of elements (`size`, or
`by` a key), `iterate` to repeat until a condition holds, `parallel` to run
named branches and keep every result, and `race` to keep the first.

## Call remote systems safely

Four fields make a remote call survive the real world. They apply to
`http`, `mcp.tool`, `a2a.send` and `a2a.delegate`:

| Field | Handles | What it does |
|---|---|---|
| `retry: {max, backoff}` | transient faults | retries this step in this run, with doubling backoff and ±20% jitter, on a durable timer |
| `breaker: {failures, cooldown}` | an outage | after N consecutive failures, fails fast across every run for the cooldown, then lets one probe through |
| `rate: "<burst>/<per>"` | a quota | makes the step wait for a token instead of failing; the wait uses no attempt |
| `idempotency` | replays | sends a key that is the same on every retry and replay of this step |

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
a2a:
  listen: http://127.0.0.1:8420
  bearer: "{{secret:A2A_BEARER}}"
mcp:
  servers:
    - name: billing
      endpoint: https://billing.example.com/mcp
      auth: {kind: static, token: "{{secret:BILLING_TOKEN}}"}
workflows:
  - name: charge-order
    inputs:
      schema:
        type: object
        required: [order_id, amount_cents]
        properties: {order_id: {type: string}, amount_cents: {type: integer}}
    steps:
      start: {kind: manual}
      charge:
        kind: http
        depends_on: [start]
        method: POST
        url: https://payments.example.com/v1/charges
        headers: {Authorization: "Bearer {{secret:PAYMENTS_TOKEN}}"}
        json: {order: "{{ inputs.order_id }}", amount: "{{ inputs.amount_cents }}"}
        idempotency: {header: Idempotency-Key}
        retry: {max: 3, backoff: 2s}
        breaker: {failures: 5, cooldown: 60s}
        rate: "10/1s"
        on_replay: fail
      record:
        kind: mcp.tool
        depends_on: [charge]
        server: billing
        tool: record_payment
        args: {order: "{{ inputs.order_id }}", charge: "{{ steps.charge.output.json.id }}"}
        idempotency: {value: "{{ inputs.order_id }}"}
        retry: {max: 3, backoff: 1s}
      done:
        kind: finish
        depends_on: [record]
        output: {charge: "{{ steps.charge.output.json.id }}"}
```

- `retry.max` is the number of retries after the first attempt, capped at
  20. A step's `timeout` and a `wait` that times out count as failures.
- The breaker's state is durable and shared by every run and every fan-out
  instance of the step, per instance of agentd. A fast-fail's error starts
  with `breaker open`, so `on_error: continue` and a `switch` on the error
  give you a fallback route.
- The default idempotency key is derived from the run and the step. An
  `mcp.tool` step always sends it in the call's `_meta`; an `http` step
  sends it only in the header or query parameter you name. `value:`
  replaces it with an application key, which also collapses two runs that
  try the same operation.
- `on_replay: fail` fails the step instead of re-sending it when a crash
  caught it mid-call. Use it for an effect that must never run twice
  without a person looking.
- An `http` step succeeds on a status from 200 to 399 unless `expect` lists
  codes, times out after 30 s by default, and refuses private and loopback
  addresses unless the step sets `allow_private: true`.

> **Note.** `{{secret:NAME}}` resolves in an `http` step's `headers` and
> `sign.secret`, and in a webhook's `auth`. Anywhere else in a step, such as
> a `json` body or an `mcp.tool` argument, it is sent as the literal text.

## Wait for a signal, an event or a deadline

A `wait` suspends the run durably until something happens, and costs
nothing while it waits. A `sleep` waits a fixed time.

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
a2a:
  listen: http://127.0.0.1:8420
  bearer: "{{secret:A2A_BEARER}}"
streams:
  orders: {}
workflows:
  - name: fulfil
    inputs:
      schema:
        type: object
        required: [order_id]
        properties: {order_id: {type: string}}
    steps:
      start: {kind: manual}
      window:
        kind: wait
        depends_on: [start]
        on: signal
        signal: warehouse-open
        timeout: 12h
      ship:
        kind: emit
        depends_on: [window]
        stream: orders
        subject: order.ship_requested
        correlation: "{{ inputs.order_id }}"
        data: {id: "{{ inputs.order_id }}"}
      shipped:
        kind: wait
        depends_on: [ship]
        on: event
        stream: orders
        subject: order.shipped
        match: "event.data.id == inputs.order_id"
        timeout: 24h
        on_timeout: chase
      chase:
        kind: agent
        instruction: "Order {{ inputs.order_id }} has not shipped in a day. Draft a note to the warehouse."
      cool_off:
        kind: sleep
        depends_on: [shipped]
        duration: 10m
      done:
        kind: finish
        depends_on: [cool_off]
        output: {order: "{{ inputs.order_id }}", shipped: true}
      late:
        kind: finish
        depends_on: [chase]
        output: {order: "{{ inputs.order_id }}", shipped: false}
```

- `on: event` waits on a declared stream from the moment it arms. `match`
  is CEL over `event`, `inputs` and `vars`, which is how a run waits for the
  event about its own order rather than the first one to arrive.
- `on_timeout` makes the deadline a branch: `chase` runs, and the wait's
  dependents do not. The timeout target has no `depends_on`; it is reached
  only by the edge. Give it its own path to a `finish`. When the wait
  succeeds instead, the target is pruned, so a step that depends on both
  paths still runs; when it times out, that step does not, because the wait
  ended without succeeding.
- Signals are edge-triggered: a signal wakes only waits that are already
  suspended. Send one with the `workflow.signal` tool, a `workflow.signal`
  step, `/signal <name>` in the terminal UI, or a webhook's `signal:` field.
- `wait` also takes `on: resource`, `condition`, `run`, `subagent`,
  `message`, `webhook` and `deadline`. The node reference lists each one's
  fields and output.

To wait for a person instead, use a `human` step; see
[Human in the loop](human-in-the-loop.md).

## One run per entity, and typed state

A `key` names what a run is about. With `concurrency.scope: key`, the run
limit counts runs about the same key, so each account is processed one at a
time while different accounts run in parallel. A `state` block types the
run's variables and says how concurrent writes combine.

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
webhooks:
  listen: http://127.0.0.1:8088
workflows:
  - name: sync-account
    key: "{{ payload.body.account_id }}"
    concurrency: {scope: key, max_runs: 1, on_overflow: queue}
    state:
      score: {schema: {type: integer, minimum: 0, maximum: 100}}
      notes: {reducer: append}
    steps:
      hook:
        kind: webhook
        path: /hooks/accounts
        auth: {bearer: "{{secret:HOOK_TOKEN}}"}
        inputs: {account: "{{ payload.body.account_id }}"}
      fetch:
        kind: http
        depends_on: [hook]
        url: "https://crm.example.com/api/accounts/{{ inputs.account }}"
        headers: {Authorization: "Bearer {{secret:CRM_TOKEN}}"}
      score:
        kind: assign
        depends_on: [fetch]
        writes: score
        value: "{{ steps.fetch.output.json.health }}"
      note:
        kind: assign
        depends_on: [fetch]
        writes: notes
        mode: append
        value: "synced at {{ env.ts }}"
      done:
        kind: finish
        depends_on: [score, note]
        output: {score: "{{ vars.score }}"}
```

- The key is rendered from the trigger payload. A firing whose key does not
  render counts against the workflow as a whole rather than a shared
  "unkeyed" bucket.
- An `assign` that breaks a declared schema fails at that step, not later
  where something reads the value.
- A declared reducer makes concurrent writes to that variable a policy. A
  step that writes it with a contradicting `mode` is refused at load.

## Call one workflow from another

A `workflow` step starts another workflow as a child run. `mode: sync` (the
default) waits for it and outputs `{run, status, output, error}`;
`mode: async` returns `{run, workflow, mode}` at once, for a later `join`.

```yaml
# fragment: under workflows[].steps
fan:
  kind: workflow
  depends_on: [start]
  name: charge-order
  mode: async
  inputs: {order_id: "{{ inputs.order_id }}", amount_cents: 1200}
gather:
  kind: join
  depends_on: [fan]
  handles: ["{{ steps.fan.output.run }}"]
  timeout: 1h
```

Cancelling the parent cancels a child it started, unless the step sets
`cascade: false`. `version:` pins the child to a definition hash prefix.

## Expose a workflow as a tool

A `tool:` block registers a workflow as a tool the agent, other workflows
or subagents can call. The workflow's input schema becomes the tool's
arguments, and a `sync` call returns the run's output.

```yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
agent:
  ask_human_unowned: gate
a2a:
  listen: http://127.0.0.1:8420
  bearer: "{{secret:A2A_BEARER}}"
workflows:
  - name: issue-refund
    description: Refund an order once the finance team approves it.
    inputs:
      schema:
        type: object
        required: [order_id, amount_cents]
        properties:
          order_id: {type: string}
          amount_cents: {type: integer}
    outputs:
      schema: {type: object, required: [refund_id]}
    tool:
      name: billing.refund
      mode: sync
      grant: {root: true, workflows: true, subagents: true}
    steps:
      start: {kind: manual}
      approve:
        kind: human
        depends_on: [start]
        question: "Refund {{ inputs.amount_cents }} cents on order {{ inputs.order_id }}?"
        schema: {type: string, enum: [approve, reject]}
        to: {role: operator, labels: {team: finance}}
        timeout: 24h
        on_error: continue          # a lapsed gate is a "no", not a failed run
      decide:
        kind: switch
        depends_on: [approve]
        on: "{{ steps.approve.output }}"
        cases: {approve: refund}
        default: refused
      refund:
        kind: http
        depends_on: [decide]
        method: POST
        url: https://payments.example.com/v1/refunds
        headers: {Authorization: "Bearer {{secret:PAYMENTS_TOKEN}}"}
        json: {order: "{{ inputs.order_id }}", amount: "{{ inputs.amount_cents }}"}
        idempotency: {header: Idempotency-Key, value: "refund-{{ inputs.order_id }}"}
      refused: {kind: finish, depends_on: [decide], status: refused, reason: "finance declined"}
      done:
        kind: finish
        depends_on: [refund]
        output: {refund_id: "{{ steps.refund.output.json.id }}"}
```

- `grant` says which callers are offered the tool: `root` (the agent's own
  turns), `workflows`, `subagents`, and `user` and `agent` (turns that a
  principal with that A2A role drives). It defaults to `root` and
  `workflows`.
- The tool's security tags are derived from what its steps reach, never
  declared; here the `http` step makes it `egress`.
- Only the configuration can register a workflow tool. A `tool:` block in a
  definition written by `workflow.create` or `workflow.update` is refused, as
  is a name that shadows a built-in tool or that two workflows claim.
- `outputs.schema` is checked at the `finish` that completes the run; a
  completed run whose output does not match fails instead.
- A run a tool call starts has no A2A task behind it, so its gate needs
  `agent.ask_human_unowned: gate` and a listener to be asked on.

## Keep workflows in their own files

A workflow can live in its own file, which also gets editor completion from
the published schema:

```yaml
# yaml-language-server: $schema=https://agentd.dev/schema/workflow.json
name: nightly
description: Close stale tickets every night.
steps:
  tick: {kind: schedule, cron: "30 2 * * *"}
  sweep:
    kind: agent
    depends_on: [tick]
    instruction: "Close tickets with no activity for 30 days."
  done: {kind: finish, depends_on: [sweep]}
```

The configuration names files one by one or scans a folder:

```yaml
# needs: workflows/nightly.yaml, workflows/weekly-report.yaml
intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
store:
  kind: file
workflows:
  - name: nightly
    file: workflows/nightly.yaml
  - dir: {path: workflows, glob: "weekly*.yaml"}
```

- `--workflow <file>` adds one more from the command line.
- A name is one definition. Two sources that define the same name are
  refused at load, naming both.
- A reload re-reads the files that `file:` and `dir:` entries name, so an
  edited workflow file takes effect without a restart.
- Set `store` explicitly when your long-lived workflows come from files.
  The file store is chosen by default only for workflows written inline.

## Test a workflow

Test a workflow offline by pointing it at the built-in mock endpoint with a
playbook. A playbook scripts model answers in order (`turns`) and decision
answers by matching text in the request (`decisions`).

```json
{
  "turns": [{"content": "Refund issued for order 1042."}],
  "decisions": [
    {"when_contains": "money back",
     "answers": {"holds": {"type": "noul", "noul": 0.93, "confidence": 0.86}}},
    {"when_contains": "billing",
     "answers": {"queue": {"type": "choice", "choice": "billing", "confidence": 0.9}}}
  ]
}
```

Save that as `playbook.json`, and this as `support-test.yaml` beside it:

```yaml
# Test endpoints: "mock:file:<playbook>" answers model calls and decisions
# from the playbook, offline. Swap in real providers for production.
intelligence:
  endpoints: "mock:file:playbook.json"
  providers:
    jevp:
      endpoints: "mock:file:playbook.json"
      dialect: decisions
  models:
    smart: {model: gpt-5.1}
    jev: {kind: decision, provider: jevp, model: typesafe/jev-1.13}
  roles: {turn: smart, decide: jev}
workflows:
  - name: support
    steps:
      start:
        kind: once
        inputs: {ticket: "Order 1042 was charged twice. I want my money back."}
      triage:
        kind: decide
        depends_on: [start]
        state: "{{ inputs.ticket }}"
        questions:
          queue: {choice: {billing: "payment problems", tech: "bugs and outages"}}
      refund:
        kind: agent
        depends_on: [triage]
        instruction: "Refund the order in this ticket: {{ inputs.ticket }}"
        when:
          noul: "The customer asks for their money back."
          state: "{{ inputs.ticket }}"
      done:
        kind: finish
        depends_on: [triage, refund]
        output:
          queue: "{{ steps.triage.output.queue.choice }}"
          refund: "{{ steps.refund.status }}"
```

Run it and read each step's outcome from the telemetry:

```sh
agentd --config support-test.yaml 2> telemetry.ndjson
jq -c 'select(.event=="step.done") | {step, status, err}' telemetry.ndjson
```

```
{
  "queue": "billing",
  "refund": "done"
}
{"step":"triage","status":"done","err":null}
{"step":"refund.when","status":"done","err":null}
{"step":"refund","status":"done","err":null}
```

Change the guard's `noul` to `0.2` in the playbook and run it again: the
output shows `"refund": "pruned"`. A short checklist before you ship:

1. `agentd --validate-config` reports every error at once; fix them all.
2. Run against `mock:file:` with one playbook per branch you care about.
3. Start the real configuration once: a `tool` step naming a tool that does
   not exist, or an `mcp.tool` on a server that is down, exits `2` at
   startup.
4. Watch a real run in the [terminal or web UI](interface.md) before you
   trust it unattended.

## Next steps

- [Workflow node reference](node-registry.md): every kind's fields and
  output.
- [Human in the loop](human-in-the-loop.md): gates, approvals and who may
  answer.
- [Troubleshooting](troubleshooting.md): when a run stalls or fails.
- [Workflow language reference](workflow-language.md): templates, CEL,
  statuses and limits.
