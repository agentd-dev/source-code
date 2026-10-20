# Workflow node reference

This page lists every workflow node kind agentd implements: the fields it
takes, what it outputs, and a short sample. There are 74 kinds: 11 start
nodes and 63 steps. It is for builders looking up a field;
[Author workflows](author-workflows.md) shows how the kinds fit together.

## How to read this page

- **Fields** lists the kind's own fields. A **bold** field is required.
  Every step also takes the
  [fields every step accepts](#fields-every-step-accepts).
- **Output** is what later steps read at `steps.<id>.output`.
- Samples are fragments of a workflow's `steps:` map. They name an MCP
  server `github`, a stream `orders` and an A2A peer `reviewer`, which a
  complete configuration declares.
- The parser refuses an unknown field and names the allowed set, so a typo
  is a load error, never a silently ignored setting.
- `agentd --workflow-schema` prints the same registry as JSON Schema, from
  the binary you run. When this page and the binary disagree, the binary is
  right.

```sh
agentd --workflow-schema | jq '.["$defs"].kinds | keys | length'
```

```
74
```

## Fields every step accepts

| Field | Effect |
|---|---|
| `kind` | the node kind; required |
| `depends_on` | the steps this one waits for. A non-start step with none is refused as an unreachable root, except a wait's `on_timeout` target |
| `when` | a CEL guard, or a [decision guard](#decision-guards). When it does not hold, the step is pruned, and so is everything that depends only on it. Inside a nested body it is skipped instead |
| `retry` | `{max, backoff}`: retries after a failure or a timeout. `max` is capped at 20; the backoff doubles per attempt with ±20% jitter, on a durable timer |
| `timeout` | a duration bounding the step. A model step, an `mcp.tool` call and a wait use it; unset, a model step and an `mcp.tool` call get `limits.step_timeout` (600 s) |
| `on_error` | `fail` (default) fails the run; `continue` marks the step done with `error` set and output `{"error": …}`; `"goto:<step>"` runs that step instead, even though its dependencies have not finished |
| `on_replay` | what restore does with a step a crash caught running: `retry` (default), `skip`, or `fail` |
| `output_schema` | a JSON Schema the step's output must match, or the step fails. On a model step it also shapes the answer |
| `cache` | `{key?, ttl?}`: reuse an earlier output. The key is a template (default: the rendered fields), shared across runs of the workflow; no `ttl` keeps it forever. Never written for a step that suspends or nests |
| `budget` | a token allowance for this step's model calls |
| `skills` | skills loaded into a model step's prompt: references `<name>`, `<origin>/<name>` (a `skills.sources` server, or `local` for agentd's own) or `<origin>/<path>`. One that two skills answer to is refused at load, naming both |
| `description` | free text |
| `idempotent`, `otel` | accepted, and read by nothing. Use `on_replay` to control a replay |

Two more fields are accepted only on some kinds; on any other kind they are
an unknown field:

| Field | Kinds | Effect |
|---|---|---|
| `breaker` | `http`, `mcp.tool`, `a2a.send`, `a2a.delegate` | `{failures, cooldown}`: after N consecutive failures, every run fails fast for the cooldown, then one probe is let through. Durable, per instance, keyed by the step's unscoped id |
| `rate` | the four above, plus `batch`, `stream` and `webhook` (`foreach` takes it inside its `batch` field) | `"<burst>/<per>"`, such as `"10/1s"` or `"1/1d"`. On a remote call, the step waits for a token without using an attempt; on a fan-out it paces element starts; on a start node it paces intake |

## Start nodes

A start node is a trigger. It does not run inside a run: its output is the
trigger payload, read by later steps at `steps.<start-id>.output`. When a
workflow has several start nodes, a run fires one and marks the others
`skipped`. Every start node takes `inputs`, a mapping rendered against
`{payload, env}` that becomes the run's `inputs` namespace; `once` passes it
through unrendered and `manual` takes the inputs its caller supplies.

### `once`

Fires once, when the workflow is armed at startup or restore.

- **Fields:** `policy`, `inputs`.
- **Output:** `{fired_at}`, in milliseconds since the epoch.
- `policy: ensure` (default) does not fire if a run is live, a run ever
  started at this node, or its start event is still queued.
  `policy: always` fires on every arm.

```yaml
# fragment: under workflows[].steps
start: {kind: once, inputs: {region: eu-west-1}}
```

### `manual`

Fires only when asked: the `workflow.run` tool, a `workflow` step, a
workflow tool call, or the `workflow.run` A2A command.

- **Fields:** `inputs` (ignored; the caller supplies inputs).
- **Output:** `{requested_by}`: who asked for the run.

```yaml
# fragment: under workflows[].steps
start: {kind: manual}
```

### `loop`

Fires as soon as it is armed, then again each time the previous run
finishes.

- **Fields:** `interval`, `delay`, `until`, `max_iterations`, `backoff`,
  `inputs`.
- **Output:** `{iteration}`.
- `until` is CEL over `outcome` (`{ok, output}`) and `last` (the last
  output); when true, the loop stops. After a failed run it waits
  `backoff.initial` instead of `interval`. `delay` holds the first firing.

```yaml
# fragment: under workflows[].steps
poll: {kind: loop, interval: 5m, until: "outcome.ok && last.done == true", backoff: {initial: 15m}}
```

### `schedule`

Fires on a clock.

- **Fields:** `cron`, `every`, `at`, `inputs`.
- **Output:** `{scheduled_for}`, in milliseconds since the epoch.
- `cron` is five fields, read in UTC. `every` is a fixed interval. `at` is
  a delay before the first firing; beside `cron` it is the first
  occurrence, and the cron takes over after it.
- The next occurrence is computed from the current time, so a missed one is
  skipped, never replayed.
- Refused at load: no clock at all, an `every` or `at` that is not a
  duration, a `cron` that does not parse, and `every` beside another clock.

```yaml
# fragment: under workflows[].steps
tick: {kind: schedule, cron: "0 7 * * 1-5"}
```

### `subscribe`

Fires when an MCP resource changes. On a notification agentd reads the
resource again and applies the filter to what it read.

- **Fields:** **`server`**, **`uri`**, `debounce_ms`, `filter`, `window`,
  `inputs`.
- **Output:** `{server, uri, content}`, plus `window` when set.
- `filter` is CEL over `content`. `debounce_ms` keeps the newest read and
  fires when the window closes.
- `window: {samples: N}` (1 to 256) keeps a durable ring of the last N reads
  and delivers it as `window`, oldest first. Debouncing drops firings; the
  window keeps every sample.
- A server that does not advertise resource subscriptions is a loud failure
  (`start.subscribe.unsupported`), not a watch that never fires.

```yaml
# fragment: under workflows[].steps
changed: {kind: subscribe, server: github, uri: "repo://example/app/releases", debounce_ms: 2000}
```

### `signal`

Fires on a named signal.

- **Fields:** **`name`**, `filter`, `inputs`.
- **Output:** `{signal, payload}`, and `principal` when a principal sent it.
- `filter` is CEL over `payload`. A signal also wakes waits suspended on it;
  see [`wait`](#wait).

```yaml
# fragment: under workflows[].steps
go: {kind: signal, name: deploy-approved, filter: "payload.env == 'prod'"}
```

### `event`

Fires on an internal event.

- **Fields:** **`on`**, `filter`, `inputs`.
- **Output:** `{event, payload}`. Downstream steps read
  `steps.<id>.output.payload.*`; the `filter` sees `payload` directly.
- Events that fire it:

| `on` | Payload |
|---|---|
| `workflow.finished` | `{run, workflow, status}` for a completed run |
| `workflow.failed` | the same, for a run that failed, stalled, was cancelled or was refused |
| `human.asked` | `{task, question, deadline_ms, channel}`; `channel` is the one an instruction document declared for the gate, else `null` |
| `human.answered` | `{task, via}` |
| `lifecycle.shutdown` | `{reason}`; a drain waits for the runs it starts |

- An event about a workflow never fires that workflow's own start, so a
  watcher on `workflow.finished` cannot trigger itself.
- `on` is one of these; any other name is refused at load.
- An `inputs` mapping that fails to render refuses to fire
  (`start.inputs.invalid`).

```yaml
# fragment: under workflows[].steps
failed: {kind: event, on: workflow.failed, filter: 'payload.workflow == "nightly"'}
```

### `stream`

Fires once per event on a declared stream, including events another
workflow emitted.

- **Fields:** **`stream`**, `subject`, `filter`, `from`, `rate`, `batch`,
  `inputs`.
- **Output:** the event: `subject`, `data`, `correlation`. With `batch`:
  `{events, count, full}`.
- `subject` matches exactly or by a `prefix.*` glob. The offset is durable,
  so a restart resumes where it left off; a recently seen event id is
  dropped, so a replayed emit is not consumed twice.
- `from: new` (default) starts at the next event; `from: earliest` replays
  the retained history once, when the consumer first arms.
- `rate` paces consumption; events queue durably, so `"1/1d"` works a
  stream off as a daily queue.
- `batch: {size: 2..1000, window?}` makes one run per group. `full: false`
  means the window elapsed first. `batch` and `rate` together are refused.
- A workflow never fires on its own emits.
- The run continues the trace of the event's `traceparent`, the span it was
  emitted in. A `batch` run starts a trace of its own, and its `start.fired`
  line names each member's `traceparent` in `links`.

```yaml
# fragment: under workflows[].steps
order: {kind: stream, stream: orders, subject: "order.*", batch: {size: 50, window: 30s}}
```

### `correlate`

Fires when a set of events sharing one correlation value has arrived. It
joins events the way `depends_on` joins steps.

- **Fields:** **`stream`**, **`on`**, **`window`**, `by`, `on_incomplete`,
  `filter`, `max_pending`, `inputs`. (The schema marks `window` optional;
  the load refuses a correlate without one.)
- **Output:** `{events, correlation, complete, missing}`, with `events` in
  `on` order.
- `on` lists two or more subject patterns. `by` is a dot path into the
  event; the default is the event's own `correlation`, which `emit` sets.
- Half-collected sets live in durable start state for at most `window`.
  `on_incomplete: fire_partial` fires what arrived when the window expires;
  `discard` (default) drops it. Check `complete` before treating a firing
  as a finished join.
- `max_pending` (default 1000, at most 100 000) caps open sets; past it,
  new correlation values are refused and logged.
- It reads the stream from its earliest retained event when it first arms,
  because half of a pair may already be there.
- The run starts a trace of its own; its `start.fired` line names each
  event's `traceparent` in `links`.

```yaml
# fragment: under workflows[].steps
paid_and_shipped:
  kind: correlate
  stream: orders
  on: ["order.paid", "order.shipped"]
  window: 48h
  on_incomplete: fire_partial
```

### `a2a`

Fires when a principal sends a message carrying a declared command.

- **Fields:** `command`, `roles`, `schema`, `into`, `inputs`.
- **Output:** `{command, args, text, parts, principal, role, conversation,
  task, message_id}`; `args` is the typed payload without its `op`.
- Declaring `command` registers it on the listener, and the extended agent
  card lists it for callers who may fire it. A built-in op name is refused.
  With no `command`, the start takes any plain message the listener
  receives.
- `schema` is the payload's contract: a non-conforming command is refused
  at the listener, naming the mismatch. `roles` narrows who may fire it.
- `into: {stream, subject}` appends the message to a stream instead of
  starting a run, after the principal and roles checks. The event starts a
  trace of its own, linked to the message's span.
- The run continues the trace the request's `traceparent` header names.

```yaml
# fragment: under workflows[].steps
review:
  kind: a2a
  command: review.start
  roles: [agent]
  schema: {type: object, required: [pr], properties: {pr: {type: integer}}}
  inputs: {pr: "{{ payload.args.pr }}"}
```

### `webhook`

Fires on an inbound HTTP request.

- **Fields:** **`path`**, `methods`, `auth`, `parallelism`, `on_overflow`,
  `rate`, `idempotency`, `respond`, `filter`, `signal`, `into`, `inputs`.
- **Output:** `{method, path, headers, body, raw_body}`; `body` is parsed
  JSON when it parses.
- Needs `webhooks.listen`. A non-loopback listener must authenticate every
  route: `auth` is `{hmac: {secret, header?, prefix?}}`, `{bearer}`,
  `{header: {name, equals}}`, or `{none: true}` on loopback only;
  `webhooks.default_auth` applies when a route sets none.
- The `Idempotency-Key` header deduplicates by default: a repeat is answered
  `200` as a duplicate and starts nothing. `idempotency: false` turns it off;
  a string names another header.
- `respond: ack` (default) answers `202` once the run is queued;
  `respond: sync` holds the response until the run ends.
- An answer that started a run names it: `run_id` and `trace_id` in the
  body, and a `traceparent` response header naming the run's span. So does
  a duplicate of a delivery that started one. A `202`'s `run_id` is the run
  the delivery was queued as: `concurrency.on_overflow: drop` or the
  workflow's `inputs.schema` can still refuse it, and the `run.dropped` or
  `run.inputs.invalid` line names it as `run`.
- `rate` answers `429` with `Retry-After` past the burst. `parallelism`
  bounds concurrent runs; past it `on_overflow` decides: reject with `503`
  (default), `queue` or `drop`.
- `signal: "name"` also fires that signal with the payload.
- `into: {stream, subject}` appends the request to a durable stream instead
  of starting a run, after verification, filtering and rate limiting;
  it answers `202 {"status":"appended"}`, with the event's `trace_id` and
  its span in a `traceparent` header, or `503` when the append is refused.
  It cannot be combined with `respond: sync`.
- The run continues the trace the request's `traceparent` header names. An
  `into:` event starts a trace of its own, linked to it, and a `signal:`
  records it as a link on each run it wakes.

```yaml
# fragment: under workflows[].steps
hook:
  kind: webhook
  path: /hooks/ci
  methods: [POST]
  auth: {hmac: {secret: "{{secret:HOOK_SECRET}}", prefix: "sha256="}}
  rate: "20/1s"
```

## Control flow

### `switch`

Routes to one step by value.

- **Fields:** **`on`**, **`cases`**, `default`, `on_no_match`.
- **Output:** `{case, goto, skipped}`; `skipped` lists the steps it pruned.
- `on` is rendered and compared, as a string, with the case keys. Each case
  value names one step id as a string; a list is refused at load.
- The chosen step runs even if its dependencies have not finished. Every
  other case target and `default` is pruned, with its whole tail.
- No match and no `default` fails the step, unless `on_no_match: skip`,
  which prunes every target and completes.

```yaml
# fragment: under workflows[].steps
route:
  kind: switch
  depends_on: [triage]
  on: "{{ steps.triage.output.class }}"
  cases: {billing: billing, tech: tech}
  default: human_review
```

### `parallel`

Runs named branches at once and keeps every result.

- **Fields:** **`branches`**, `on_error`, `min_success`.
- **Output:** an object keyed by branch name, each branch's result, plus
  `_errors` when a branch failed.
- A branch failure fails the step only when `on_error` is `fail` (default).
  `min_success: n` fails it when fewer than `n` branches succeed; its
  `on_error` then decides what follows, as for any failure.

```yaml
# fragment: under workflows[].steps
gather:
  kind: parallel
  depends_on: [start]
  on_error: continue
  branches:
    issues: {steps: {list: {kind: mcp.tool, server: github, tool: list_issues, args: {state: open}}}}
    prs:    {steps: {list: {kind: mcp.tool, server: github, tool: list_pull_requests, args: {state: open}}}}
```

### `race`

Runs named branches at once and keeps the first to finish.

- **Fields:** **`branches`**, `timeout`.
- **Output:** `{winner, output}`. The other branches are cancelled.
- If every branch fails, the step fails; if `timeout` passes first, it ends
  `timeout`.

```yaml
# fragment: under workflows[].steps
fastest:
  kind: race
  depends_on: [start]
  timeout: 30s
  branches:
    primary:  {steps: {get: {kind: http, url: "https://api.example.com/rates"}}}
    fallback: {steps: {get: {kind: http, url: "https://backup.example.com/rates"}}}
```

### `foreach`

Runs a body once per element of a list.

- **Fields:** **`over`**, **`body`**, `batch`, `collect`, `on_error`, `as`.
- **Output:** an array as long as the input, in input order. A failed
  element under `on_error: continue` fills its slot with `{index, error}`.
- `over` is an array; an object becomes `[{key, value}]` and null an empty
  list. `as` names the element (`item` and `index` are always set).
- `batch: {size, parallel, rate}`: `size` defaults to 1, `parallel` to 4.
  Asking for more than `limits.workflow.fan_out` (8 when unset) is refused
  at load for a definition written inline; anything else is clamped to it.
- `collect: {into, mode}` also writes the output to a run variable.

```yaml
# fragment: under workflows[].steps
each:
  kind: foreach
  depends_on: [list]
  over: "{{ steps.list.output.json }}"
  as: repo
  batch: {parallel: 3}
  on_error: continue
  body:
    steps:
      check: {kind: judge, input: "{{ repo }}", rubric: "The repository has a licence file."}
```

### `batch`

Like `foreach`, but the body sees a group of elements.

- **Fields:** **`over`**, **`body`**, `by`, `size`, `parallel`, `rate`,
  `collect`, `on_error`.
- **Output:** an array with one entry per group.
- `size` (default 10) groups consecutive elements; `by` groups by a dotted
  key, in order of first appearance. Inside the body, `item` is the group's
  array, not one record.

```yaml
# fragment: under workflows[].steps
per_owner:
  kind: batch
  depends_on: [list]
  over: "{{ steps.list.output.json }}"
  by: owner.login
  body:
    steps:
      sum: {kind: summarize, input: "{{ item }}"}
```

### `iterate`

Repeats a body while or until a condition holds.

- **Fields:** **`body`**, `while`, `until`, `max_iterations`, `collect`.
- **Output:** the last iteration's result; with `collect`, the array of
  every result.
- `while` is evaluated before each iteration and `until` after it, as CEL
  over the run's data plus `iteration`, `last` and `results`.
  `max_iterations` defaults to and is capped at 10 000.

```yaml
# fragment: under workflows[].steps
refine:
  kind: iterate
  depends_on: [draft]
  until: "last.verdict == 'pass'"
  max_iterations: 3
  body:
    steps:
      rewrite: {kind: agent, instruction: "Tighten this draft: {{ steps.draft.output }}"}
      score:   {kind: judge, depends_on: [rewrite], input: "{{ steps.rewrite.output }}", rubric: "Under 200 words and accurate."}
```

### `subgraph`

An inline nested graph, so step ids can repeat in different subgraphs.

- **Fields:** **`body`**.
- **Output:** the body's sinks: one sink's output directly, several as an
  object keyed by sink id.
- A decision guard on a `subgraph` whose body has a step named `when` is
  refused, because both would be `<id>.when`.

```yaml
# fragment: under workflows[].steps
prepare:
  kind: subgraph
  depends_on: [start]
  body:
    steps:
      fetch: {kind: http, url: "https://api.example.com/config"}
      parse: {kind: parse, depends_on: [fetch], text: "{{ steps.fetch.output.body }}", format: yaml}
```

### `join`

Waits for async handles: child runs or subagents.

- **Fields:** **`handles`**, `timeout`, `min`, `partials`.
- **Output:** an object keyed by handle: `{kind, status, output, error}` for
  a run, `{kind, status, result, error}` for a subagent. With `partials` and
  a timeout, `{partial: true, results}`.
- `min` defaults to every handle. A handle that names no run or subagent
  counts as finished with `{"error": "unknown handle"}`, so a typo does not
  block the join.

```yaml
# fragment: under workflows[].steps
gather:
  kind: join
  depends_on: [fan]
  handles: ["{{ steps.fan.output.run }}"]
  timeout: 1h
  partials: true
```

### `workflow`

Starts another workflow as a child run.

- **Fields:** **`name`**, `inputs`, `mode`, `start`, `version`, `cascade`.
- **Output:** with `mode: sync` (default), `{run, status, output, error}`
  when the child ends; with `async` or `detached`, `{run, workflow, mode}`
  at once.
- `start` picks the child's start node (default: its `manual`, else the
  first). `version` is a definition hash prefix the child must match.
  `cascade` (default `true`) cancels the child with the parent.
- A durable parent waiting on a non-durable child is warned about at load:
  after a restart the child is gone and the wait fails.

```yaml
# fragment: under workflows[].steps
charge:
  kind: workflow
  depends_on: [start]
  name: charge-order
  inputs: {order_id: "{{ inputs.order_id }}", amount_cents: 1200}
```

### `finish`

Ends the run.

- **Fields:** `status`, `output`, `reason`.
- **Output:** `output`, which becomes the run's output.
- `status` is `completed` (default), `refused` or `cancelled`; any other
  value fails the run. A completing run's output is checked against the
  workflow's `outputs.schema`, and a mismatch fails the step instead.
- Every step still running or pending is cancelled. Every workflow needs at
  least one `finish`, including one that a `loop` or `schedule` restarts.

```yaml
# fragment: under workflows[].steps
done: {kind: finish, depends_on: [report], output: "{{ steps.report.output }}"}
```

### `fail`

Fails the step with a message, which fails the run unless `on_error` routes
it.

- **Fields:** `message`, `code`.
- **Output:** `code`. The error is `message` (default
  `deliberate failure`).

```yaml
# fragment: under workflows[].steps
stop: {kind: fail, depends_on: [check], message: "no owner on record", code: NO_OWNER}
```

### `assert`

Fails unless a CEL condition holds.

- **Fields:** **`condition`**, `message`.
- **Output:** `true`. On failure, the error is `message` or
  `assertion failed: <condition>`.

```yaml
# fragment: under workflows[].steps
sane: {kind: assert, depends_on: [fetch], condition: "steps.fetch.output.status == 200"}
```

### `noop`

Does nothing: a join point, a switch target or a placeholder.

- **Fields:** none.
- **Output:** `null`.

```yaml
# fragment: under workflows[].steps
joined: {kind: noop, depends_on: [a, b]}
```

### `checkpoint`

Forces a durable write of the run here.

- **Fields:** `name`.
- **Output:** `null`.

```yaml
# fragment: under workflows[].steps
saved: {kind: checkpoint, depends_on: [transform], name: after-transform}
```

## Wait and coordinate

### `wait`

Suspends until something happens. The wait is durable: a restart resumes it
with its deadline intact.

- **Fields:** **`on`**, `server`, `uri`, `condition`, `signal`, `run`,
  `subagent`, `conversation`, `from`, `webhook`, `stream`, `subject`,
  `subscribe`, `match`, `task`, `timeout`, `on_timeout`.
- `on` is written out, never templated: what a wait listens on decides what
  it opens, which load checks. An `on` that is none of the forms below is
  refused at load.
- `timeout` ends the wait with status `timeout`. `on_timeout: <step>` makes
  that an expected branch: the named step runs, and the wait's dependents do
  not. The target has no `depends_on`; give it its own path to a `finish`.

| `on` | Also takes | Resolves when | Output |
|---|---|---|---|
| `signal` | `signal` | that signal is sent while the wait is suspended | `{signal, payload, from}` |
| `event` | `stream`, `subject`, `match` | a matching event is appended after the wait armed | the event |
| `event` | `subscribe`, `match` | an occurrence of the wait's own MCP Events subscription arrives | the event |
| `resource` | `server`, `uri` | the MCP resource changes | `{server, uri, content}`; `content` is null if the re-read failed |
| `condition` | `condition` | the CEL condition is true | `{satisfied: true}` |
| `run` | `run` | that run ends | `{run, status, output, error}` |
| `subagent` | `subagent` | that subagent ends | `{handle, status, result, error}` |
| `message` | `conversation`, `from` | a message arrives on the conversation | `{conversation, contextId, message, principal}` |
| `webhook` | `webhook: {path?, auth?, emit_url_to?}` | the callback route it opens is called | `{signal, payload, from}`; `payload` is the request |
| `deadline` | `timeout` | the timeout passes; this ends `done`, not `timeout` | `{waited_ms}` |
| `mcp_task` | `task` | the MCP task a step of this run detached ends, or already has | `{task, server, tool, status, ok, result?, error?}` |

Notes per form:

- **`event`**: `match` is CEL over `event`, `inputs` and `vars`, so a run
  can wait for the event about its own order. The wait is anchored where
  the stream stood when it armed; there is no `from: earliest`.
- **`event` with `subscribe`** (`{server, event, arguments}`): makes its own
  MCP Events subscription inside an operator's `event_grants` entry, and
  lets it go when the step leaves the wait. A `subscribe` no grant covers is
  refused at load. Occurrences arrive up to a poll interval late, so read
  the current state first and wait only for what comes after. See
  [React to changes and events](react-to-events.md).
- **`condition`**: the CEL sees only `runs`, `subagents`, `now_ms` and
  `signals` (recent signals by name), not the run's own data. A condition
  that reads `vars` fails the step when it is evaluated.
- **`message`**: `conversation` is a runtime key, the `contextId` the sender
  used, or `"*"` for any; unset, the run's own. `from` names whose message
  counts, in the addressee syntax of a `human` gate's `to`; unset, only the
  run's principal, an operator or the runtime wakes it. `from` with any
  other `on` is a load error. The run keeps its trace; the `wait.resolved`
  line names the span the message was accepted in as `link`, as it does for
  `a2a.wait`.
- **`webhook`**: opens a one-shot callback route on the webhook listener,
  at a fixed `path` or a generated one. The URL is built on `webhooks.url`
  (else the listener's bind), and `emit_url_to` writes it to a run variable
  so another step can hand it out. The route and its `auth` references are
  in the durable wait record, so a restart serves it again. Needs the `a2a`
  build feature.
- **`mcp_task`**: `task` is the handle a detaching step outputs,
  `steps.<id>.output.task.id` (see [`mcp.tool`](#mcptool)). The wait ends
  `done` however the task ended, as `wait {on: run}` does: `status` is
  `completed`, `failed` or `cancelled` — `failed` too when agentd stopped
  following the task, with `error` saying why — and `ok` holds only for a
  completed task whose result is no error. A task that ended before the wait
  armed resolves it at once, and any number of waits may read one task. The
  wait's `timeout` ends the wait, not the task, which stays the run's. A
  `task` that names no task this run detached fails the step, and so does an
  empty one: a detaching step the server answered at once made none.

```yaml
# fragment: under workflows[].steps
shipped:
  kind: wait
  depends_on: [ship]
  on: event
  stream: orders
  subject: order.shipped
  match: "event.data.id == inputs.order_id"
  timeout: 24h
  on_timeout: chase
```

### `sleep`

Suspends for a fixed time, on a durable timer.

- **Fields:** **`duration`**.
- **Output:** `{slept_ms}`.

```yaml
# fragment: under workflows[].steps
cool_off: {kind: sleep, depends_on: [deploy], duration: 10m}
```

### `human`

Asks a person a question and suspends until they answer, across restarts.
[Human in the loop](human-in-the-loop.md) covers who is asked and how they
answer.

- **Fields:** **`question`**, `schema`, `to`, `timeout`.
- **Output:** the answer: in the `schema`'s shape when the step declares
  one (an object whose fields `steps.<gate>.output.<field>` reads, or a
  boolean or number), else the text the person sent.
- `schema` is enforced: a reply that does not match re-asks with the
  reason.
- `to` names who must answer: `{role: operator}`, optionally narrowed by
  `labels`, or the id `operator`. A `to` that names anyone else is refused
  at load. In an instruction document, `to: "@human/<name>"` addresses one of
  the document's `::!human` declarations.
- `timeout` defaults to 24 hours. A gate that times out fails the step;
  route that with `on_error`.
- A gate in a run that no A2A caller owns, such as one a schedule started,
  needs `agent.ask_human_unowned: gate`; otherwise
  `agent.ask_human_fallback` (default `fail`) decides.
- `reply_uri` is refused at load: nothing implements it.

```yaml
# fragment: under workflows[].steps
approve:
  kind: human
  depends_on: [draft]
  question: "Publish these notes?"
  schema: {type: string, enum: [publish, drop]}
  to: {role: operator, labels: {team: release}}
  timeout: 4h
```

### `message`

Delivers a message into one of this instance's own conversations, so a run
can hand work to the agent.

- **Fields:** **`to`**, `text`, `parts`, `wait`, `timeout`, `on_timeout`.
- **Output:** `{delivered, conversation, depth}`; with `wait: reply`, the
  reply.
- `to` is a context id, `root`, or `new` (a fresh conversation). One of
  `text` or `parts` is required.
- The delivery takes the same readers, durability and per-conversation lock
  as an inbound A2A message. Without `wait`, the step completes once the
  delivery is durable.
- **The loop guard.** A message can start a turn, and a turn can start a
  run that sends a message. Each delivery adds one to a hop depth the run
  inherits from whatever caused it, and a delivery past
  `limits.max_message_depth` (default 8) is refused before it is written,
  failing the step with the limit named. `message.send` also refuses to
  deliver into the conversation its own caller is running in.

```yaml
# fragment: under workflows[].steps
hand_off: {kind: message, depends_on: [summary], to: root, text: "Nightly summary: {{ steps.summary.output.summary }}"}
```

### `workflow.signal`

Sends a named signal at once.

- **Fields:** **`name`**, `payload`, `run`.
- **Output:** `{delivered}`, the number of waits it resolved.
- Without `run`, it reaches every live run; it also fires matching `signal`
  start nodes. Signals are edge-triggered: a wait that is not yet suspended
  misses it. Two sibling steps in one run, one signalling and one waiting,
  race; signal across runs instead.

```yaml
# fragment: under workflows[].steps
open: {kind: workflow.signal, depends_on: [ready], name: warehouse-open, payload: {site: dub1}}
```

### `workflow.wait`

Waits for another run to end.

- **Fields:** **`run`**, `timeout`.
- **Output:** `{run, status, output, error}`.

```yaml
# fragment: under workflows[].steps
child_done: {kind: workflow.wait, depends_on: [fan], run: "{{ steps.fan.output.run }}", timeout: 2h}
```

### `workflow.cancel`

Cancels another run.

- **Fields:** **`run`**, `reason`.
- **Output:** `{ok: true, run}`. An unknown run fails the step.

```yaml
# fragment: under workflows[].steps
stop_child: {kind: workflow.cancel, depends_on: [late], run: "{{ steps.fan.output.run }}", reason: "superseded"}
```

### `a2a.wait`

Waits for a message from a named peer: the reply half of `a2a.send`.

- **Fields:** **`from`**, `conversation`, `timeout`.
- **Output:** `{conversation, contextId, message, principal}`.
  `conversation` is the runtime's key, which a `message` step can answer
  in; `contextId` is the name the sender used.
- `from` is required because a conversation id is the sender's to choose:
  without it, anyone who names the conversation could answer. It uses the
  addressee syntax (`agent:reviewer`, `user:*`,
  `{role: agent, labels: {team: ops}}`); a malformed one is a load error.
- `conversation` as in `wait {on: message}`; a run with none of its own and
  no `conversation` fails the step.

```yaml
# fragment: under workflows[].steps
answer: {kind: a2a.wait, depends_on: [ask], from: "agent:reviewer", conversation: "*", timeout: 1h}
```

## Data shaping

These steps are deterministic: no model, no network. Except for `transform`
and `template`, they are pure data steps that ride the run's checkpoint
instead of writing their own.

| Kind | Fields | Output |
|---|---|---|
| `assign` | **`value`**, `writes`, `mode` | `value`, also written to run variable `writes` (default: the step id) |
| `transform` | **`value`**, `writes`, `mode` | the same as `assign` |
| `map` | **`over`**, **`expr`**, `as` | an array: `expr` applied to each element |
| `filter` | **`over`**, **`expr`**, `as` | the elements for which `expr` is true |
| `reduce` | **`over`**, **`expr`**, `initial`, `as`, `acc` | the folded value |
| `sort` | **`over`**, `by`, `order` | the sorted array |
| `dedupe` | **`over`**, `by` | the array without duplicates, first kept |
| `chunk` | **`value`**, **`size`**, `by`, `overlap` | an array of pieces |
| `template` | `text`, `value` | the rendered `text`, or `value` |
| `parse` | **`text`**, `format` | the parsed data |
| `validate` | **`value`**, **`schema`** | `value`, or a failure naming the mismatch |

- `assign` `mode`: `overwrite` (default), `append`, `merge` or `union`. A
  declared `state` schema for the variable is checked first.
- `map`, `filter` and `reduce`: `expr` is CEL (a `CEL:` prefix is optional),
  or a `{{…}}` template. The element is `item` and the `as` alias, with
  `index`; `reduce`'s accumulator is `acc` and the `acc` alias, starting at
  `initial`. `filter`'s `expr` must yield a boolean.
- `sort`: `by` is a dotted path; `order` is `asc` (default) or `desc`.
- `chunk`: an array is split into slices of `size` elements; a string by
  `by`: `chars` (default), `lines`, `words` or `tokens` (about 1.3 tokens per
  word). `overlap` repeats that many items between pieces.
- `parse` `format`: `json`, `yaml`, `csv` (first row is the header; output
  is an array of objects), `lines`, or `auto` (default: JSON, then YAML,
  then lines).
- `validate`: `schema` is taken as written, not rendered.

```yaml
# fragment: under workflows[].steps
rows:   {kind: parse, depends_on: [fetch], text: "{{ steps.fetch.output.body }}", format: csv}
open:   {kind: filter, depends_on: [rows], over: "{{ steps.rows.output }}", expr: "item.status == 'open'"}
ids:    {kind: map, depends_on: [open], over: "{{ steps.open.output }}", expr: "item.id"}
total:  {kind: reduce, depends_on: [open], over: "{{ steps.open.output }}", initial: 0, expr: "acc + int(item.amount)"}
oldest: {kind: sort, depends_on: [open], over: "{{ steps.open.output }}", by: created, order: asc}
owners: {kind: dedupe, depends_on: [open], over: "{{ steps.open.output }}", by: owner}
pages:  {kind: chunk, depends_on: [fetch], value: "{{ steps.fetch.output.body }}", size: 400, by: words, overlap: 40}
count:  {kind: assign, depends_on: [ids], writes: open_count, value: "CEL: size(steps.ids.output)"}
same:   {kind: transform, depends_on: [ids], writes: open_ids, value: "{{ steps.ids.output }}"}
line:   {kind: template, depends_on: [total], text: "{{ vars.open_count }} open, {{ steps.total.output }} owed"}
shape:  {kind: validate, depends_on: [rows], value: "{{ steps.rows.output }}", schema: {type: array, maxItems: 10000}}
```

## Durable state

These call the instance's durable store, through the same contracts as the
agent's `memory.*` and `artifact.*` tools.

| Kind | Fields | Output |
|---|---|---|
| `memory.get` | **`key`** | `{found, key, value, meta}` |
| `memory.set` | **`key`**, **`value`**, `ttl` | `{ok, key, meta}` |
| `memory.push` | **`key`**, **`value`** | `{ok, key, length}`; appends to the array at `key`, creating it |
| `memory.shift` | **`key`** | `{found, value, remaining}`; removes the first element |
| `memory.pop` | **`key`** | `{found, value, remaining}`; removes the last element |
| `memory.list` | `prefix`, `limit` | `{keys, truncated}` |
| `memory.delete` | **`key`** | `{ok, key}` |
| `artifact.create` | **`name`**, `mime`, `content`, `from_step`, `sensitive` | `{id, name, size, sha256}` |
| `artifact.get` | **`id`** | `{id, name, mime, content, size, sha256}` |
| `artifact.delete` | **`id`** | `{ok}` |

`memory.push` with `memory.shift` is a durable queue; on an empty queue
`shift` returns `{found: false}`, so a drain loop stops. `artifact.create`
takes its content from `content` or from another step's output by id with
`from_step`; `sensitive` keeps it out of transcripts.

```yaml
# fragment: under workflows[].steps
enqueue: {kind: memory.push, depends_on: [start], key: queue/emails, value: "{{ inputs.email }}"}
next:    {kind: memory.shift, depends_on: [enqueue], key: queue/emails}
seen:    {kind: memory.set, depends_on: [next], key: "seen/{{ inputs.email }}", value: true, ttl: 30d}
check:   {kind: memory.get, depends_on: [seen], key: "seen/{{ inputs.email }}"}
recent:  {kind: memory.list, depends_on: [check], prefix: seen/, limit: 50}
last:    {kind: memory.pop, depends_on: [recent], key: queue/emails}
forget:  {kind: memory.delete, depends_on: [last], key: "seen/{{ inputs.email }}"}
report:  {kind: artifact.create, depends_on: [recent], name: seen.json, mime: application/json, from_step: recent}
read:    {kind: artifact.get, depends_on: [report], id: "{{ steps.report.output.id }}"}
drop:    {kind: artifact.delete, depends_on: [read], id: "{{ steps.report.output.id }}"}
```

## Knowledge and search

These call the knowledge and search sources the instance configures
(`knowledge.server`, `search.server`).

| Kind | Fields | Output |
|---|---|---|
| `knowledge.search` | **`query`**, `top_k`, `filters` | `{hits: [{id, uri, title, score, snippet, metadata}]}` |
| `knowledge.get` | `id`, `uri` | `{content, mime, metadata}` |
| `search.query` | **`query`**, `limit`, `freshness` | `{results: [{title, url, snippet, source, published}]}` |
| `search.fetch` | **`url`**, `max_bytes` | `{content, mime, final_url}` |

`freshness` is a hint such as `day` or `week`. The registry also lists
`kind` for `search.query` (the search tool's `web`, `docs` or `code`), but a
step cannot set it: `kind` is the step's own node kind.

```yaml
# fragment: under workflows[].steps
lookup: {kind: knowledge.search, depends_on: [start], query: "{{ inputs.question }}", top_k: 5}
doc:    {kind: knowledge.get, depends_on: [lookup], id: "{{ steps.lookup.output.hits.0.id }}"}
web:    {kind: search.query, depends_on: [start], query: "{{ inputs.question }}", limit: 5, freshness: week}
page:   {kind: search.fetch, depends_on: [web], url: "{{ steps.web.output.results.0.url }}", max_bytes: 200000}
```

## Integration

### `mcp.tool`

Calls a tool on a declared MCP server: the main way a workflow reaches the
outside world.

- **Fields:** **`server`**, **`tool`**, `args`, `idempotency`, `breaker`,
  `rate`, `detach`, `input`.
- **Output:** the tool's result. A result the server marks as an error
  fails the step. With `detach: true` and a call the server makes a task
  of, `{task: {server, id, status}}`.
- Every call carries a retry-stable `agent/idempotency_key` in `_meta`, with
  `agent/attempt` beside it. `idempotency: {value}` replaces the key with an
  application key.
- A server the configuration does not declare is refused at load. A
  declared server that is down when the step runs fails the step; one down
  at startup joins when it comes up. A server in the `services:` catalog
  with a `rate` paces calls.
- A call the server makes an MCP task of is waited for durably, bounded by
  the step's `timeout` (default `limits.step_timeout`), which cancels the
  task when it passes, and with it any question the task asks.
- `detach: true` finishes the step as soon as the server makes the task:
  `id` is agentd's handle for it (`mt_…`, never the server's task id) and
  `status` the one the server gave. The run owns the task: a
  [`wait {on: mcp_task}`](#wait) collects how it ended, a `tool` step reads
  it with `mcp.task.get` and cancels it with `mcp.task.cancel`, and the run's
  end — completed, failed or cancelled — cancels it while it is open. A
  server that answers at once answers the step as it would without
  `detach`, and so does one past its 32 open tasks, one with `tasks: off`,
  and a Company's or a delegated server, which are offered no tasks.
  `detach` is `true` or `false` as written, and refused beside `cache`.
- `input: {to?, timeout?}` puts the task's questions (form elicitations) to
  a person through the gate a [`human`](#human) step opens, under the same
  rules: `to` names who answers, in a `human` gate's forms, and `timeout`
  (default 24 h) how long they have, after which the answer is `cancel`.
  Over A2A each question opens on a task of its own in the run's
  conversation, never on the run's task, so it neither holds up nor is held
  up by the run's `human` steps and policy gates; a client following the
  run finds it among the conversation's tasks (`ListTasks` with its
  `contextId` and `TASK_STATE_INPUT_REQUIRED`). A step that waits on its
  own task bounds the question by its `timeout` too, and the gate shows
  whichever deadline comes first; `detach` the task for a question that may
  take longer. The answer goes back to the server as the elicitation's
  `accept`. Without `input` the question is declined. `input` is taken as
  written, not templated.

```yaml
# fragment: under workflows[].steps
label:
  kind: mcp.tool
  depends_on: [triage]
  server: github
  tool: add_labels
  args: {issue: "{{ inputs.issue }}", labels: ["{{ steps.triage.output.class }}"]}
  retry: {max: 3, backoff: 2s}
```

```yaml
# fragment: under workflows[].steps
build:
  kind: mcp.tool
  depends_on: [triage]
  server: github
  tool: run_workflow
  args: {workflow: release.yml}
  detach: true                       # finishes with {task: {server, id, status}}
  input: {to: operator, timeout: 2h} # the task's questions go to the operator
built:
  kind: wait
  depends_on: [build]
  on: mcp_task
  task: "{{ steps.build.output.task.id }}"
  timeout: 3h
```

### `mcp.resource`

Reads MCP resources and prompts.

- **Fields:** **`server`**, **`op`**, `uri`, `name`, `arguments`,
  `reference`, `argument`.
- **Output** by `op`:

| `op` | Uses | Output |
|---|---|---|
| `read` | `uri` | `{uri, text, json, contents}`; `json` is `text` parsed, or null |
| `list` | | `{resources}` |
| `templates` | | `{templates}` |
| `prompt` | `name`, `arguments` | `{description, messages, text}` |
| `complete` | `reference`, `argument` | the completion result |

```yaml
# fragment: under workflows[].steps
readme: {kind: mcp.resource, depends_on: [start], server: github, op: read, uri: "repo://example/app/README.md"}
```

### `tool`

Calls any tool by its registry name: built-in, code-registered or MCP.

- **Fields:** **`name`**, `args`, `detach`, `input`.
- **Output:** the tool's result.
- The tool must be granted to workflows. One that is unknown, disabled or
  not granted exits `2` at startup.
- On a tool of an MCP server's route, a call the server makes a task of is
  waited for, and `detach` and `input` work as on
  [`mcp.tool`](#mcptool). On any other tool they exit `2` at startup.
- `mcp.task.get` and `mcp.task.cancel` reach a task a step of the same run
  detached, by the `id` its output named: `get` reads its status and, once
  it ended, its result, leaving it for the run's waits; `cancel` sends
  `tasks/cancel`, and the task is followed until the server says it ended.
- The call's `agent/idempotency_key` is the step's: the same across its
  retries and a restart's replay. A `company.*` call keeps the message id or
  `Idempotency-Key` it minted under it, so a retry sends the same one.

```yaml
# fragment: under workflows[].steps
plan: {kind: tool, depends_on: [start], name: plan.get, args: {}}
```

### `http`

One outbound HTTP request.

- **Fields:** **`url`**, `method`, `headers`, `query`, `body`, `json`,
  `timeout`, `expect`, `allow_private`, `sign`, `idempotency`, `breaker`,
  `rate`, `auth`.
- **Output:** `{status, ok, headers, body, json}`; `json` is the body parsed,
  or null.
- `method` defaults to `GET`. Success is a status from 200 to 399 unless
  `expect` lists codes; any other status fails the step with the output
  still recorded. `timeout` defaults to 30 s.
- `json` is serialised with `Content-Type: application/json` and wins over
  `body`. `query` values are percent-encoded.
- The target is resolved once and dialled by the vetted address; private,
  loopback and link-local addresses are refused unless the step sets
  `allow_private: true`. Under `security.egress: closed` the URL must match
  a `kind: http` service, whose `methods` list is also a ceiling.
- `headers` and `sign.secret` resolve `{{secret:NAME}}` references; nothing
  else in the step does.
- `sign: {secret, header?, prefix?}` adds an HMAC-SHA256 of the body,
  default header `X-Signature`, prefix `sha256=`.
- `idempotency: {header}` or `{query}` sends the step's key there;
  `value` replaces the derived key.
- The request carries the step's span as `traceparent` and `tracestate`,
  unless `headers` sets either: then it carries what `headers` set and
  nothing of agentd's.
- `auth: {kind: company, company: <entry>}` calls a Company's OpenAPI API
  for its User instead (the `companies` feature): the step is a
  `company.api` call, under its rules — `url` an operation of the
  description that `api_allow` lists (or its path under the API's server
  URL), the query and `json` held to its schemas, the User's DPoP Session
  Token, an `Idempotency-Key` on a write — and its output is the tool's,
  `{status, json}`. `headers`, `body`, `sign`, `allow_private` and
  `idempotency` are refused with it. See
  [A Company's agent and its APIs](configuration.md#a-companys-agent-and-its-apis).

```yaml
# fragment: under workflows[].steps
notify:
  kind: http
  depends_on: [summary]
  method: POST
  url: https://hooks.example.com/agentd
  json: {text: "{{ steps.summary.output.summary }}"}
  sign: {secret: "{{secret:NOTIFY_SECRET}}"}
  expect: [200, 202]
```

### `a2a.send`

Sends a message to a peer and continues: a notification.

- **Fields:** **`to`**, `parts`, `command`, `args`, `context`, `timeout`,
  `idempotency`, `breaker`, `rate`.
- **Output:** the peer's answer to the send.
- `to` names an `a2a.peers` entry. `command` with `args` sends the typed
  payload a peer's `a2a` start matches. `idempotency: true` pins the message
  id across retries so the peer can deduplicate. `timeout` defaults to 30 s.
- The request carries the send's span as `traceparent` and `tracestate`; an
  agentd peer continues it.
- Needs the `a2a` build feature.

```yaml
# fragment: under workflows[].steps
ask: {kind: a2a.send, depends_on: [start], to: reviewer, parts: [{text: "Review PR {{ inputs.pr }}"}], idempotency: true}
```

### `a2a.delegate`

Delegates an objective to a peer and waits for the result: request and
response.

- **Fields:** **`peer`**, `objective`, `command`, `args`, `output_contract`,
  `timeout`, `idempotency`, `breaker`, `rate`.
- **Output:** the peer's result, parsed as JSON when it parses.
- With `command` and `args`, the payload is typed and checked against the
  command's schema at the peer, and the step returns that command's run
  output. `timeout` defaults to 120 s.
- Every request of the delegation — the card read, the send, each poll —
  carries one span of the run's as `traceparent` and `tracestate`; an
  agentd peer's task and run continue it.
- Needs the `a2a` build feature.

```yaml
# fragment: under workflows[].steps
review:
  kind: a2a.delegate
  depends_on: [start]
  peer: reviewer
  command: review.start
  args: {pr: "{{ inputs.pr }}"}
  timeout: 30m
```

### `emit`

Publishes an event to a stream, or writes a note or an audit record.

- **Fields:** `stream`, `subject`, `data`, `correlation`, `forward`,
  `note`, `audit`, `value`.
- **Output:** with `stream`, `{id, seq, stream, subject}`; without, `value`.
- `stream` and `subject` go together. The event id is the step's idempotency
  key, so a replayed emit lands under the same id and consumers drop the
  copy. The stream must be declared under `streams:`.
- `forward: {webhook: <url>, allow_private?}` or `forward: {peer: <name>}`
  pushes the event out as it is appended. The durable copy is the source of
  truth, so a failed forward is logged and does not fail the step. A peer
  receives it as a `stream.forwarded` command. One destination, not both.
- The event records the span it was emitted in as `traceparent` and
  `tracestate`, which a consuming run continues; a forward sends a span of
  it as HTTP headers, on the webhook POST or the peer's A2A request.
- Without `stream`: `note` writes to the root conversation's transcript,
  `audit` logs an `audit.emit` record, and `value` becomes the output.

```yaml
# fragment: under workflows[].steps
shipped: {kind: emit, depends_on: [ship], stream: orders, subject: order.shipped, correlation: "{{ inputs.order_id }}", data: {id: "{{ inputs.order_id }}"}}
```

## Intelligence

Model steps run on the model their `model:` field names, an
`intelligence.models` entry or a wire model name. Without `model:`, a step
runs on its kind's role: `think` on `roles.think`; `decide`, `classify`,
`route` and `judge` on `roles.decide`; everything else on `roles.turn`. A
decision model serves only the deciding kinds; any other model step refuses
one at load. An unknown model is exit `2`. See
[Models, providers and roles](intelligence.md).

### `agent`

A full agentic turn with tools: think, call, observe, repeat.

- **Fields:** **`instruction`**, `output_contract`, `output_schema`,
  `tools`, `servers`, `limits`, `context`, `skills`, `system`, `model`,
  `effort`.
- **Output:** the final answer text, or a JSON value with `output_schema`.
- `tools` and `servers` narrow what it may reach. `context` is a list of seed
  messages, or `{template, seed}` where `template` names a
  `context.templates` entry for this step's system prompt. `effort` sets a
  reasoning model's effort for this step.
- An `agent` step takes `instruction`; `prompt` belongs to `think`, and the
  wrong one is a load error.

```yaml
# fragment: under workflows[].steps
draft:
  kind: agent
  depends_on: [start]
  instruction: "Draft release notes for {{ inputs.tag }}."
  servers: [github]
  model: smart
```

### `think`

A tool-free model call, at most three rounds.

- **Fields:** **`prompt`**, `output_schema`, `reads`, `check`, `retries`,
  `skills`, `system`, `model`, `effort`.
- **Output:** a JSON value. An answer that is not JSON, or does not match
  `output_schema`, is asked again twice and then fails the step.
- `reads` lists paths of run data to put in front of the prompt.
- `check` and `retries` are accepted and read by nothing.

```yaml
# fragment: under workflows[].steps
risk:
  kind: think
  depends_on: [draft]
  prompt: "Rate the rollout risk of this change."
  reads: [steps.draft.output]
  output_schema: {type: object, required: [risk], properties: {risk: {enum: [low, medium, high]}}}
```

### `subagent`

A child process running its own loop with narrowed tools. The supervisor
can always kill it.

- **Fields:** **`instruction`** or **`template`** (never both), `params`,
  `mode`, `tools`, `servers`, `limits`, `priority`, `context`,
  `output_contract`, `output_schema`, `skills`, `durable`.
- **Output:** with `mode: sync` (default), `{handle, status, result, error}`
  when the child ends; with `async`, `detached` or `warm`, the handle record.
- `template` instantiates a `subagents.templates` entry; `params` fill its
  declared holes and are schema-checked, and `tools`/`servers` are refused
  because the template defines the grant.
- `limits` takes `steps`, `tokens` and `deadline`, plus OS caps `memory`
  (such as `512MB`) and `cpu` (a duration). `priority` is `low`, `normal` or
  `high`, mapped to niceness; low sheds first under pressure.

```yaml
# fragment: under workflows[].steps
research:
  kind: subagent
  depends_on: [start]
  instruction: "Find the three most-cited papers on {{ inputs.topic }}."
  servers: [github]
  limits: {steps: 30, deadline: 10m, memory: 512MB}
```

### `classify`

Puts an input into one of a set of classes.

- **Fields:** **`input`**, **`classes`**, `prompt`, `skills`, `model`,
  `confidence`, `on_unsure`.
- **Output:** `{class, confidence, reason}` on a chat model; on a decision
  model `{class, confidence, probabilities, calibrated: true}`, with no
  `reason`.
- `classes` is a list, or a map of class to what it covers.

```yaml
# fragment: under workflows[].steps
triage: {kind: classify, depends_on: [start], input: "{{ inputs.ticket }}", classes: {billing: "payment problems", tech: "bugs and outages"}}
```

### `extract`

Pulls structured data out of text. It has no tools, so it is the safe way to
read untrusted text.

- **Fields:** **`input`**, **`output_schema`**, `prompt`, `skills`, `model`.
- **Output:** an object matching `output_schema`.

```yaml
# fragment: under workflows[].steps
fields:
  kind: extract
  depends_on: [hook]
  input: "{{ steps.hook.output.body }}"
  output_schema: {type: object, required: [email], properties: {email: {type: string}, plan: {type: string}}}
```

### `summarize`

Shortens text.

- **Fields:** **`input`**, `length`, `prompt`, `skills`, `model`.
- **Output:** `{summary}`.
- `length` is free text, such as `three sentences` (default
  `a short paragraph`). Facts, names, numbers and identifiers are kept
  verbatim.

```yaml
# fragment: under workflows[].steps
brief: {kind: summarize, depends_on: [fetch], input: "{{ steps.fetch.output.body }}", length: five bullet points}
```

### `judge`

Judges an input against a rubric.

- **Fields:** **`input`**, **`rubric`**, `levels`, `prompt`, `skills`,
  `model`, `confidence`, `on_unsure`.
- **Output:** on a chat model, `{verdict, score, confidence, reasons}`,
  plus `level` with `levels`. On a decision model the verdict is a
  statement's probability: `score` is that probability on a 0 to 10 scale,
  or with `levels` the position on them plus `level`; there are no
  `reasons`.
- `verdict` is `pass` or `fail`.

```yaml
# fragment: under workflows[].steps
check: {kind: judge, depends_on: [draft], input: "{{ steps.draft.output }}", rubric: "Accurate, under 200 words, no marketing language.", levels: [poor, fair, good]}
```

### `route`

Picks one of a set of choices for an input: the model-driven alternative to
`switch`.

- **Fields:** **`input`**, **`choices`**, `prompt`, `skills`, `model`,
  `confidence`, `on_unsure`.
- **Output:** `{choice, confidence, reason}` on a chat model; on a decision
  model, `{choice, confidence, probabilities, calibrated: true}`.

```yaml
# fragment: under workflows[].steps
where: {kind: route, depends_on: [start], input: "{{ inputs.ticket }}", choices: [sales, support, security]}
```

### `decide`

Asks several typed questions about one state in one call.

- **Fields:** **`state`**, **`questions`**, `model`, `confidence`,
  `on_unsure`.
- **Output:** one entry per question, plus `model` (what answered) and
  `escalated`.
- `state` is text, a list of texts, or a JSON object. At most 16 questions,
  each with 2 to 32 options or levels.

| Question | Asks | Answer entry |
|---|---|---|
| `{choice: [a, b]}` or `{choice: {a: "criterion", …}}` | which option applies | `{choice, probabilities?, confidence, calibrated}` |
| `{noul: "<statement>"}` | how likely the statement is true | `{noul, confidence, calibrated}`; `noul` is the probability |
| `{score: [low, …, high]}` | where on the scale, lowest first | `{score, level, probabilities?, confidence, calibrated}` |

Each question may add `instructions`: guidance for that question (for a
`choice`, the question text itself, which defaults to the question's id).

```yaml
# fragment: under workflows[].steps
triage:
  kind: decide
  depends_on: [start]
  model: jev
  state: "{{ inputs.ticket }}"
  questions:
    queue:  {choice: {billing: "payment problems", tech: "bugs and outages"}}
    urgent: {noul: "The customer cannot work at all."}
    mood:   {score: [calm, annoyed, angry, furious]}
    refund: {noul: "The customer asks for money back.", instructions: "Only an explicit request counts."}
  confidence: 0.75
  on_unsure: escalate
```

### Confidence and escalation

The deciding kinds share two fields:

- **`confidence`** is the least sureness an answer is acted on with: a
  choice's or score's confidence, or for a `noul` its distance from a coin
  flip, doubled (`|p − 0.5| × 2`). Unset, `roles.decide`'s confidence
  applies, if it has one. A value outside 0 to 1 counts as none.
- **`on_unsure`** says what an answer under it does: `escalate` asks once
  more on the model's `escalate` model and takes that answer as final
  (`escalated: true`); `fail` fails the step; `continue` passes the answer
  on with `unsure: true`. The default is `escalate` when the model names an
  `escalate` model, else `fail`. `escalate` on a model with none is refused
  at load.

On a decision model, answers are native and `calibrated: true`. On a chat
model, `decide` is one structured call with every question in it, and the
presets compare `confidence` with the answer's own self-reported confidence
(`calibrated: false`); there is nothing to escalate to.

### Decision guards

A step's `when:` may be a decision instead of CEL:

```yaml
# fragment: under workflows[].steps
refund:
  kind: agent
  depends_on: [triage]
  instruction: "Refund the order."
  when:
    noul: "The customer explicitly asks for their money back."
    state: "{{ inputs.ticket }}"   # default: the outputs of depends_on, by step id
    confidence: 0.8                # default: roles.decide's
    model: jev                     # default: roles.decide
```

- At load the guard becomes a `decide` step of its own, `refund.when`, with
  the guarded step's dependencies and one question, `holds`. The guarded
  step then depends on it alone. Read its answer at
  `{{ /steps/refund.when/output }}` (the JSON-pointer form, because the id
  has a dot).
- The step runs only when `noul` is at least 0.5 and the answer is sure.
  Otherwise it is pruned exactly as a false CEL guard prunes it; in a nested
  body, it is skipped.
- Without `state`, the guard decides about the dependencies' outputs keyed by
  step id; with no dependencies, the run's inputs; in a `foreach` or `batch`
  body, the element.
- An unsure answer is escalated when the model names an `escalate` model,
  and otherwise prunes the step. A guard never fails the run for doubt.
- A step that a `switch` case, `on_error: goto` or `on_timeout` routes to may
  carry a guard: the edge asks it first. A case not chosen has its guard
  pruned unasked.
- Refused at load: an unknown field, an empty `noul`, a `confidence`
  outside 0 to 1, a model the deciding kinds refuse, a guard on a start
  node, and a guard on a `subgraph` whose body has a step named `when`.
- Event matches (`filter`, a wait's `match`) stay CEL. To decide about an
  event, guard the first step after it.

## Choosing between near-neighbours

| If you want | Use | Not |
|---|---|---|
| a peer to do something, and you need the answer | `a2a.delegate` | `a2a.send` |
| a peer to know something, and you carry on | `a2a.send` | `a2a.delegate` |
| to branch on data you already have | `switch` | `route` |
| to branch on meaning, not a value | `route` or `classify` | `switch` |
| to skip a step on a judgement about text | a decision guard | `route` and a `switch` |
| to read untrusted text safely | `extract` | `agent` |
| a child that can be killed | `subagent` | `agent` |
| to walk a list | `foreach` | `iterate` |
| to repeat until a condition | `iterate` | `foreach` |
| to run branches and take the first | `race` | `parallel` |
| to run branches and keep them all | `parallel` | `race` |
| to wait for an event about this run | `wait {on: event}` with `match` | a second workflow |
| to start a long MCP call and collect it later | `detach: true` and `wait {on: mcp_task}` | an `mcp.tool` step that waits |
| to join several events that never had a run | `correlate` | `wait` |

## Next steps

- [Author workflows](author-workflows.md): the kinds put together, with
  complete samples.
- [Workflow language reference](workflow-language.md): templates, CEL,
  statuses, limits and the workflow tools.
- [Workflows: how runs work](workflows.md): the scheduler, durability and
  retirement.
