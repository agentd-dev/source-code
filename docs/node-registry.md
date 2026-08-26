# The node registry

Every workflow node agentd implements, what it needs, and what it does. The
tables are generated from the binary's own registry (`agentd --workflow-schema`),
so the required-field columns are what the parser actually enforces.

**72 kinds — 10 start nodes and 62 steps. All are implemented.** If this page and
the binary ever disagree, the binary is right; regenerate from
`agentd --workflow-schema`.

---

## Before the tables: six things that are easy to get wrong

These are the traps, in the order people hit them.

**1. References are `steps.<id>.output.<path>`.** A step's result is not at its
bare name. Writing `{{ hook.body.id }}` reads nothing; you want
`{{ steps.hook.output.body.id }}`. The namespaces are:

| Namespace | Holds |
|---|---|
| `steps.<id>.output` | what a completed step returned |
| `vars.<key>` | what `assign`/`transform` wrote (`writes:` names the key) |
| `inputs` | the run's inputs, from the start node's `inputs` mapping |
| `env` | instance, run id, instruction — plus, per step: `env.step`, `env.attempt`, and `env.idempotency_key` (stable across retries of the step; `env.ts` is NOT) |
| `memory.<key>` | durable memory, read through for keys the definition names |
| `signals` | recently delivered signals |

**Templates are resolved at RUN time, not validated at author time.** A typo in a
path is not a config error — it is a step failure at the moment the step runs.
`--validate-config` will not catch it.

**2. `switch` cases name ONE step, as a string.** `cases: {select: prepare}`,
not `cases: {select: [prepare]}`. A case value is compared against a step id, so
a list can never match anything; the loader refuses it outright, with a message
saying what to write instead of leaving a branch that is silently unreachable.

**3. A step's status can be `pruned`.** A branch nobody chose is `pruned`, not
`skipped`, and the difference is load-bearing: a *skipped* step satisfies its
dependents (that is how several start nodes work, and how uneven joins proceed),
while a *pruned* one does not. A step is pruned only when EVERY inbound path is
pruned — one live parent keeps it alive.

**4. Signals are edge-triggered.** `workflow.signal` wakes waiters that are
ALREADY suspended. A signal sent before the waiter parks is missed — there is no
buffering. Two sibling steps in one run, one signalling and one waiting, is a
race with no ordering primitive to fix it; signal across runs, or from a tool or
an operator, where the waiter is demonstrably parked first.

**5. CEL is a build feature.** `when`, `until`, `filter` and the `expr` of
`map`/`filter`/`reduce` need it. The release binaries ship with it enabled; a
binary built without it **exits 2** on a config that uses those fields rather
than ignoring the expression and running the step anyway.

**6. Nested steps are scoped.** A step inside a `foreach`/`parallel`/`race`/
`subgraph` gets a compound id — `each[0].work`, `par{branch}.work`,
`sub.work` — which is what you will see in the logs and what cancellation
matches on.

---

### Start nodes — what creates a run

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `once` | — | `policy` `inputs` | Fires when armed at boot/restore, or on `workflow.run`. `policy: ensure` skips if a run is already live. |
| `manual` | — | `inputs` | Fires only on `workflow.run` (a tool call or an A2A command). Nothing arms it. |
| `loop` | — | `interval` `delay` `until` `max_iterations` `backoff` `inputs` | Fires again each time the previous run finishes. `interval`/`delay` pace it, `until` and `max_iterations` stop it, `backoff` slows it after failures. |
| `schedule` | — | `cron` `every` `tz` `jitter` `catch_up` `at` `inputs` | Fires on a 5-field UTC cron or an `every` interval. `at` is one-shot and consumes itself. `catch_up` decides what a missed window does. |
| `subscribe` | `server` `uri` | `debounce_ms` `coalesce` `filter` `deliver` `on_no_listener` `window` `inputs` | Fires when an MCP resource changes (notify-then-read). `debounce_ms`/`coalesce` collapse bursts; `filter` drops uninteresting reads; `window: {samples: N}` delivers the last N read values (`output.window`) — the trend, not just the reading. |
| `signal` | `name` | `filter` `deliver` `inputs` | Fires on a named signal from another run, a tool, or an operator. |
| `event` | `on` | `filter` `inputs` | Fires on an internal event — `workflow.finished|failed`, `subagent.finished`, `budget.exhausted`, `config.reloaded`, `restore.done`, `human.asked`, `human.answered`, `human.timeout`, `lifecycle.shutdown` (the deinit hook: the drain waits for its runs). Output is `{event, payload: {…}}` — read `…output.payload.*`; the CEL `filter` sees the inner payload. |
| `stream` | `stream` | `subject` `filter` `from` `rate` `inputs` | Fires once per event on a declared stream — including events another workflow `emit`ted. `subject` matches exactly or by `prefix.*` glob; `from: earliest` replays the backlog into a consumer that did not exist when the events were published; the offset is durable, so a restart resumes where it left off, exactly once. A workflow never fires on its own emits; `rate: "<burst>/<per>"` paces consumption (events queue durably — `rate: "1/1d"` turns a stream into a worked-off daily queue). Output is the event: `…output.subject`, `…output.data.*`, `…output.correlation`. |
| `a2a` | — | `command` `roles` `schema` `inputs` | Fires when a principal sends a message whose command matches. Declaring `command` REGISTERS it as an A2A command the listener accepts; `schema` is the payload CONTRACT — a non-conforming command is refused at the listener, synchronously, naming the mismatch. `roles` narrows who may fire it. Output: `…output.args.*` (the typed payload), plus `parts`/`text`/`principal`. |
| `webhook` | `path` | `methods` `auth` `parallelism` `on_overflow` `rate` `idempotency` `respond` `filter` `signal` `inputs` | Fires on an inbound HTTP request at `path`. Needs `webhooks.listen`; a non-loopback listener must authenticate every route. `rate: "<burst>/<per>s"` throttles arrivals (429 + Retry-After past it). `signal: "name/{{ body.field }}"` also fires that signal with the payload — the webhook→signal relay as one field. |

### Control flow

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `switch` | `on` `cases` | `default` `on_no_match` | Routes to ONE named step per case. Case values and `default` are step-id STRINGS, not lists. The chosen branch runs even if its deps are not terminal; the others are skipped. No case and no default is a FAILURE unless `on_no_match: skip` — then the switch completes and every branch is pruned. |
| `parallel` | `branches` | `on_error` | Runs every branch concurrently. `on_error` decides whether one failure fails the step. |
| `foreach` | `over` `body` | `batch` `collect` `on_error` `as` | Runs `body` once per element of `over`. `batch {size, parallel, rate}` paces it; `collect` gathers outputs; `as` names the element. |
| `batch` | `over` `body` | `by` `size` `parallel` `rate` `collect` `on_error` | Like `foreach` but the body sees a GROUP of elements — `size`/`by` form the groups. |
| `iterate` | `body` | `while` `until` `max_iterations` `collect` | Repeats `body` while/until a condition, bounded by `max_iterations`. The loop primitive when there is no list to walk. |
| `race` | `branches` | `timeout` `min_success` | Runs branches concurrently and takes the first to finish. `min_success` requires more than one; losers are cancelled. |
| `join` | `handles` | `timeout` `min` `partials` | Awaits async `handles` (from `workflow {mode: async}` or `subagent`). `min` and `partials` decide what "enough" means. |
| `subgraph` | `body` | — | An inline nested graph. Scopes ids, so the same step names can repeat in different subgraphs. |
| `workflow` | `name` | `inputs` `mode` `start` `version` `cascade` | Starts another workflow as a child run. `mode: sync` blocks, `async` returns a handle for `join`, `detached` forgets it. `cascade` propagates cancellation. |
| `wait` | `on` | `server` `uri` `condition` `signal` `run` `subagent` `conversation` `webhook` `stream` `subject` `match` `timeout` `on_timeout` | Suspends until `on` resolves: `resource | condition | signal | run | subagent | message | event | webhook`. Durable — a restart resumes the wait. `on_timeout: <step>` makes the deadline an EXPECTED branch: the named step runs (forced), the wait's dependents stay unfired, the run continues. A signal wait's output is `{signal, payload, from}`. `on: event` parks on a declared `stream`, anchored where the log stood when it armed: `subject` globs, and the CEL `match` sees `event`, `inputs` and `vars` together — which is how a run waits for the reply about *its own* order rather than the first one to arrive. There is no `from: earliest`: resolving on an event that predates the run would break at-least-once for everything downstream. Its output is the event. |
| `sleep` | `duration` | — | Suspends for `duration`. Durable: the timer survives a restart. |
| `assert` | `condition` | `message` | Fails the run unless `condition` holds. A guard you want loud. |
| `fail` | — | `message` `code` | Ends the run as failed with `message`/`code`. |
| `noop` | — | — | Does nothing. A join point, a switch target, or a placeholder. |
| `checkpoint` | — | `name` | Forces a durable checkpoint here rather than at the next natural boundary. |
| `finish` | — | `status` `output` `reason` | Ends the run with `status` (`completed|failed|refused|cancelled`) and an optional `output`. |

### Data shaping (deterministic, no model, no network)

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `assign` | `value` | `writes` `mode` | Writes `value` into the run vars at `writes` (default: the step id). `mode: overwrite|append|merge`. |
| `transform` | `value` | `writes` `mode` | Identical to `assign`; the name reads better when the value is computed from other data. |
| `map` | `over` `expr` | `as` | Applies `expr` to every element of `over`. `as` names the element (default `item`). Needs CEL. |
| `filter` | `over` `expr` | `as` | Keeps elements of `over` whose `expr` is true. Needs CEL. |
| `reduce` | `over` `expr` | `initial` `as` `acc` | Folds `over` with `expr` from `initial`; `acc` names the accumulator. Needs CEL. |
| `sort` | `over` | `by` `order` | Orders `over`, optionally `by` a field and `order: asc|desc`. |
| `dedupe` | `over` | `by` | Removes duplicates from `over`, optionally `by` a key. |
| `chunk` | `value` `size` | `by` `overlap` | Splits `value` into pieces of `size`, with optional `overlap`. |
| `template` | — | `text` `value` | Renders `text` (or `value`) against the run data. The general-purpose string builder. |
| `parse` | `text` | `format` | Parses `text` into data — `format: json|yaml|…`. |
| `validate` | `value` `schema` | — | Checks `value` against a JSON `schema`; fails the step if it does not conform. |

### Durable state

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `memory.set` | `key` `value` | `ttl` | Writes a durable key/value, with optional `ttl`. |
| `memory.push` | `key` `value` | — | Appends to the ARRAY at `key` (created if absent) — the durable queue primitive. |
| `memory.shift` / `memory.pop` | `key` | — | Removes and returns the first / last element (`{found: false}` on empty — a drain loop just stops). |
| `memory.get` | `key` | — | Reads a durable key. The result is `{found, value}`. |
| `memory.list` | — | `prefix` `limit` | Lists keys under `prefix`, bounded by `limit`. |
| `memory.delete` | `key` | — | Removes a durable key. |
| `artifact.create` | `name` | `mime` `content` `from_step` `sensitive` | Stores a durable blob — `name`, `mime`, `content` or `from_step`. `sensitive` keeps it out of transcripts. |
| `artifact.get` | `id` | — | Reads an artifact by `id`. |
| `artifact.delete` | `id` | — | Removes an artifact by `id`. |

### Knowledge and search

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `knowledge.search` | `query` | `top_k` `filters` | Searches a configured knowledge source — `query`, `top_k`, `filters`. |
| `knowledge.get` | — | `id` `uri` | Fetches one knowledge document by `id`/`uri`. |
| `search.query` | `query` | `kind` `limit` `freshness` | Queries a configured search source — `query`, `limit`, `freshness`. |
| `search.fetch` | `url` | `max_bytes` | Fetches one `url` through the search source, bounded by `max_bytes`. |

### Integration — reaching outside

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `mcp.tool` | `server` `tool` | `args` `idempotency` `breaker` `rate` | Calls `tool` on a declared MCP `server` with `args`. The main way a workflow reaches the outside world. Always attaches a retry-stable `agent/idempotency_key` in `_meta`; `idempotency: {value: …}` substitutes an application key. |
| `mcp.resource` | `server` `op` | `uri` `name` `arguments` `reference` `argument` | Reads MCP resources — `op: read|list|prompt|complete`. |
| `tool` | `name` | `args` | Calls a tool by registry name, wherever it lives (internal, code-registered, or MCP). |
| `http` | `url` | `method` `headers` `query` `body` `json` `timeout` `expect` `allow_private` `sign` `idempotency` `breaker` `rate` | One outbound HTTP request. SSRF-guarded: resolved once and dialled by the vetted address. `allow_private` is a separate, larger decision. `idempotency: {header: NAME}` (or `{query: NAME}`) sends a retry-stable derived key; `value:` overrides it with an application key. |
| `a2a.send` | `to` | `parts` `command` `args` `context` `timeout` `idempotency` `breaker` `rate` | Notifies a peer and continues — fire-and-forget. Completes when the peer ACCEPTS the message. `idempotency: true` pins the A2A `messageId` across retries so the peer can deduplicate. `command` + `args` send the TYPED DataPart the peer's `a2a` start matches on — deterministic dispatch, not prose the peer's model interprets. |
| `a2a.delegate` | `peer` | `objective` `command` `args` `output_contract` `timeout` `idempotency` `breaker` `rate` | Delegates an objective to a peer and BLOCKS for the result. Request/response, where `a2a.send` is a notification. With `command` + `args` the payload is TYPED (and checked against the command's declared `schema` at the peer); the delegate blocks until the command's run finishes and returns its output. `idempotency: true` pins the `messageId` across retries. |
| `a2a.wait` | — | `conversation` `timeout` | Suspends until a message arrives on a `conversation`. The reply half of `a2a.send`. |
| `message` | `to` | `text` `parts` `wait` `timeout` `on_timeout` | Delivers into one of THIS instance's own conversations, so a run can hand work to the agent rather than only the reverse. `to` is a context id, `root`, or `new` (a fresh conversation). The delivery takes the same readers, durability and per-context lock as an inbound A2A message. `wait: reply` parks on the answer; without it the step completes once the delivery is durable and the turn happens on its own schedule. Chained deliveries are capped by `limits.max_message_depth` (default 8) — see [Loops](#message-loops). |
| `workflow.signal` | `name` | `payload` `run` | Sends a named signal. Edge-triggered: the waiter must already be suspended. |
| `workflow.wait` | `run` | `timeout` | Blocks until another `run` reaches a terminal status. |
| `workflow.cancel` | `run` | `reason` | Cancels another `run` with a `reason`. |
| `emit` | — | `stream` `subject` `data` `correlation` `note` `audit` `metric` `value` | With `stream`/`subject` (they travel together, or a load error): publishes an event to a declared stream that any workflow's `stream` start can consume — durable, replayable, exactly-once downstream (the event id is the step's idempotency key, so a crash-replayed emit lands under the same id and consumers drop the copy). Without them: writes `note` to the root transcript, logs `audit` as an audit record, and returns `value` as the step output. |

### Intelligence — the steps that cost tokens

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `think` | `prompt` | `output_schema` `reads` `check` `retries` `skills` `system` | One model call. `output_schema` shapes the answer; `check`/`retries` re-ask until it conforms. |
| `agent` | `instruction` | `output_contract` `output_schema` `tools` `servers` `limits` `context` `skills` `system` | A full agentic loop with tools — think, call, observe, repeat. `tools`/`servers` narrow what it may reach. `context` is a seed-message array, or the object form `{template: <name>, seed: […]}` — `template` names an entry in `context.templates` to render this step's system prompt with, in place of the instance default. |
| `subagent` | `instruction` *or* `template` | `params` `mode` `tools` `servers` `limits` `priority` `context` `output_contract` `output_schema` `skills` `durable` | A child PROCESS running its own loop, with narrowed tools and trust. The supervisor can always kill it. `template` instantiates a `subagents.templates` entry — `params` fill its declared holes (schema-checked), `tools`/`servers` are refused (the template defines the grant), and an instance-tier template spawns a full child daemon whose handle is an A2A peer name. `limits` adds OS caps — `memory` (RLIMIT_AS), `cpu` (RLIMIT_CPU) — beside `steps`/`tokens`/`deadline`; `priority: low\|normal\|high` maps to niceness and sheds low first under pressure. |
| `classify` | `input` `classes` | `prompt` `skills` | Puts `input` into one of `classes`. |
| `extract` | `input` `output_schema` | `prompt` `skills` | Pulls `input` into the shape of `output_schema`. No tools — the safest way to read untrusted text. |
| `summarize` | `input` | `length` `prompt` `skills` | Shortens `input` to `length`. |
| `judge` | `input` `rubric` | `prompt` `skills` | Scores `input` against a `rubric`. |
| `route` | `input` `choices` | `prompt` `skills` | Picks one of `choices` for `input`. The model-driven alternative to `switch`. |
| `human` | `question` | `schema` `to` `timeout` | Asks a person `question` and suspends durably until they answer — the answer can arrive after a restart. `schema` is ENFORCED on the reply: a mismatch re-asks with the reason rather than being accepted. `to` names **who must answer** (see [Addressed gates](#addressed-gates)); omit it and any watcher may answer. `reply_uri` is refused at load — nothing implements it. |

---

## Cross-cutting fields

Every step accepts these regardless of kind:

| Field | Effect |
|---|---|
| `depends_on` | the DAG edge. A non-start step with no dependency is refused as an unreachable root |
| `when` | a CEL guard; the step is skipped when it is false (needs `cel`) |
| `retry` | `{max, backoff}` — retry on failure: exponential doubling with deterministic ±20% jitter, durable timer between attempts |
| `breaker` | `{failures, cooldown}` — cross-run circuit breaker, on the remote-effect kinds only (`http`, `mcp.tool`, `a2a.send`, `a2a.delegate`): opens after N consecutive failures, fails fast, one probe per cooldown; durable per `workflow/step` |
| `rate` | `"<burst>/<per>s"` — outbound throttle on the same kinds: the step WAITS (durable timer, no attempt consumed) for a token, so fan-outs drain at the declared pace instead of bursting |
| `timeout` | bound the step; a suspended step resumes as timed out |
| `on_error` | `fail` (default) · `continue` · `goto:<step>` |
| `output_schema` | validate the step's output; also SHAPES the answer for model kinds |
| `cache` | `{key, ttl}` — reuse a previous result |
| `budget` | a per-step token/step allowance |
| `skills` | skills to load for a model step |
| `on_replay` | what restore does with a step caught in flight by a crash: `retry` (default), `skip`, or `fail` |
| `idempotent` | parsed and validated, but nothing reads it — use `on_replay` to control a replay |
| `description`, `otel` | documentation and trace attributes |

## Durability, and what a restart does

Every effectful step checkpoints before its effect (pure data steps replay
deterministically from the last checkpoint instead of writing one each). On
restore:

- A step that was **Running** is replayed (`on_replay` decides: retry, skip, fail).
- A step that was **Suspended** — `wait`, `sleep`, `human`, `a2a.wait` — resumes
  waiting, with its deadline intact. A human gate opened before a restart can
  still be answered after it.
- A **timer** whose durable record went missing is repaired at restore rather
  than leaving the step unreachable.

Effects can therefore run twice. Give a step whose effect must not repeat
`on_replay: fail` (or `skip`), or make the underlying tool idempotent — every
remote-effect step already carries a retry-stable idempotency key for that.

## Addressed gates

A `human` gate — and `ask_human` — normally reaches whoever is watching this
agent's tasks. `to` narrows that to a named decider:

```yaml
approve:
  kind: human
  question: "Refund {{ inputs.order_id }} for {{ inputs.amount }}?"
  schema: {type: object, required: [approved], properties: {approved: {type: boolean}}}
  to: "*@finance.example"                      # a principal-id glob
  # or, when identity is better described than enumerated:
  # to: {role: user, labels: {team: finance}}
  timeout: 24h
  on_timeout: escalate
```

A reply from anyone else is **refused with an explanation** and the gate stays
open, rather than the answer vanishing into the conversation. Conditions are
ANDed, so adding one always narrows — an operator tightening a gate never
widens it by accident. Labels are the durable form (people change, teams do
not) and come from `a2a.principals[].labels`.

Three declarations are load errors rather than accepted-and-ignored, because
each produces a gate that *looks* routed and is not: one that names nobody
(`to: {}`), one that names `role: anonymous` — precisely the identity nothing
vouches for — and any typo in a field or role name.

An addressed gate is **never auto-answered**, whatever `agent.approval` says. A
model judge standing in for the finance lead makes the record a lie, and an
operator who set `approval: auto` was making a statement about the agent's own
asks, not about a gate that names someone.

**An operator can still answer**, and this is deliberate. Refusing them would
be theatre — an operator can already rewrite the config, the store or the
definition — so what matters instead is that it is *visible*: the answer is
recorded as `operator_override`, logged, and audited under the id of whoever
actually replied. The audit line names the person rather than "human", which
is what makes "the finance lead approved this refund" a record instead of a
claim.

Both the addressee and the answer schema live in the run's **durable wait
record**, so a restart rebuilds the gate exactly as declared. That matters more
than it sounds: a gate whose enforcement lived only in memory would quietly
weaken on restart, accepting anyone and anything.

## Message loops

`message` closes a cycle the runtime did not previously have: a run can start a
turn, and a turn can start a run. Left alone that re-arms forever —
message → turn → `workflow.run` → finish → message — and it is not something
pressure shedding can hold, because shedding queues new turns while the chain
keeps adding more.

The guard is **hop depth**, not volume. Twenty unrelated workflows greeting the
operator are not a loop; one workflow greeting itself is. A run inherits the
depth of the work that caused it, each delivery adds one, and a delivery past
`limits.max_message_depth` (default 8) is refused **before it becomes durable**.
The step fails and names the limit, so a chain that would have spun silently is
a visibly broken workflow instead.

Two refusals fall out of the same rule: `message.send` will not deliver into the
conversation its own caller is running in, and `on_workflow_finished: think`
continues the run's chain rather than starting a fresh one.

## Choosing between near-neighbours

| If you want | Use | Not |
|---|---|---|
| a peer to do something, and you need the answer | `a2a.delegate` | `a2a.send` |
| a peer to know something, and you carry on | `a2a.send` | `a2a.delegate` |
| to branch on data you already have | `switch` | `route` (a model call) |
| to branch on meaning, not a value | `route` / `classify` | `switch` |
| to read untrusted text safely | `extract` (no tools) | `agent` (has tools) |
| a child that can be killed | `subagent` (a process) | `agent` (in-process loop) |
| to walk a list | `foreach` | `iterate` |
| to repeat until a condition | `iterate` | `foreach` |
| to run branches and take the first | `race` | `parallel` |
| to run branches and need them all | `parallel` | `race` |

## Regenerating this page

```sh
agentd --workflow-schema | jq '.["$defs"].kinds'
```

The same schema is published at `https://agentd.dev/schema/workflow.json` for
editor autocomplete — see
[Editor autocomplete](workflows.md#editor-autocomplete).

Each entry carries `fields`, `required`, `start` and `implemented`. That is the
authority; this page is prose around it.
