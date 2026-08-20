# The node registry

Every workflow node agentd implements, what it needs, and what it does. The
tables are generated from the binary's own registry (`agentd --workflow-schema`),
so the required-field columns are what the parser actually enforces.

**67 kinds — 9 start nodes and 58 steps. All are implemented.** If this page and
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
| `env` | instance, run id, instruction |
| `memory.<key>` | durable memory, read through for keys the definition names |
| `signals` | recently delivered signals |

**Templates are resolved at RUN time, not validated at author time.** A typo in a
path is not a config error — it is a step failure at the moment the step runs.
`--validate-config` will not catch it.

**2. `switch` cases name ONE step, as a string.** `cases: {select: prepare}`,
not `cases: {select: [prepare]}`. A list used to validate and then silently
match nothing at runtime; it is now refused at load time with a message saying
what to write.

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
`map`/`filter`/`reduce` need it. It ships in the release binaries from 2.3.0; on
an older binary a config using them **exits 2** rather than ignoring them.

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
| `subscribe` | `server` `uri` | `debounce_ms` `coalesce` `filter` `deliver` `on_no_listener` `inputs` | Fires when an MCP resource changes (notify-then-read). `debounce_ms`/`coalesce` collapse bursts; `filter` drops uninteresting reads. |
| `signal` | `name` | `filter` `deliver` `inputs` | Fires on a named signal from another run, a tool, or an operator. |
| `event` | `on` | `filter` `inputs` | Fires on an internal event — `workflow.finished|failed`, `subagent.finished`, `budget.exhausted`, `config.reloaded`, `restore.done`, `human.timeout`. |
| `a2a` | — | `command` `roles` `inputs` | Fires when a principal sends a message whose command matches. Declaring `command` REGISTERS it as an A2A command the listener accepts. `roles` narrows who may fire it. |
| `webhook` | `path` | `methods` `auth` `parallelism` `on_overflow` `idempotency` `respond` `filter` `inputs` | Fires on an inbound HTTP request at `path`. Needs `webhooks.listen`; a non-loopback listener must authenticate every route. |

### Control flow

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `switch` | `on` `cases` | `default` | Routes to ONE named step per case. Case values and `default` are step-id STRINGS, not lists. The chosen branch runs even if its deps are not terminal; the others are skipped. |
| `parallel` | `branches` | `on_error` | Runs every branch concurrently. `on_error` decides whether one failure fails the step. |
| `foreach` | `over` `body` | `batch` `collect` `on_error` `as` | Runs `body` once per element of `over`. `batch {size, parallel, rate}` paces it; `collect` gathers outputs; `as` names the element. |
| `batch` | `over` `body` | `by` `size` `parallel` `rate` `collect` `on_error` | Like `foreach` but the body sees a GROUP of elements — `size`/`by` form the groups. |
| `iterate` | `body` | `while` `until` `max_iterations` `collect` | Repeats `body` while/until a condition, bounded by `max_iterations`. The loop primitive when there is no list to walk. |
| `race` | `branches` | `timeout` `min_success` | Runs branches concurrently and takes the first to finish. `min_success` requires more than one; losers are cancelled. |
| `join` | `handles` | `timeout` `min` `partials` | Awaits async `handles` (from `workflow {mode: async}` or `subagent`). `min` and `partials` decide what "enough" means. |
| `subgraph` | `body` | — | An inline nested graph. Scopes ids, so the same step names can repeat in different subgraphs. |
| `workflow` | `name` | `inputs` `mode` `start` `version` `cascade` | Starts another workflow as a child run. `mode: sync` blocks, `async` returns a handle for `join`, `detached` forgets it. `cascade` propagates cancellation. |
| `wait` | `on` | `server` `uri` `condition` `signal` `run` `subagent` `conversation` `webhook` `timeout` | Suspends until `on` resolves: `resource | condition | signal | run | subagent | message | webhook`. Durable — a restart resumes the wait. |
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
| `mcp.tool` | `server` `tool` | `args` | Calls `tool` on a declared MCP `server` with `args`. The main way a workflow reaches the outside world. |
| `mcp.resource` | `server` `op` | `uri` `name` `arguments` `reference` `argument` | Reads MCP resources — `op: read|list|prompt|complete`. |
| `tool` | `name` | `args` | Calls a tool by registry name, wherever it lives (internal, code-registered, or MCP). |
| `http` | `url` | `method` `headers` `query` `body` `json` `timeout` `expect` `allow_private` `sign` | One outbound HTTP request. SSRF-guarded: resolved once and dialled by the vetted address. `allow_private` is a separate, larger decision. |
| `a2a.send` | `to` | `parts` `context` `timeout` | Notifies a peer and continues — fire-and-forget. Completes when the peer ACCEPTS the message. |
| `a2a.delegate` | `peer` `objective` | `output_contract` `timeout` | Delegates an objective to a peer and BLOCKS for the result. Request/response, where `a2a.send` is a notification. |
| `a2a.wait` | — | `conversation` `timeout` | Suspends until a message arrives on a `conversation`. The reply half of `a2a.send`. |
| `workflow.signal` | `name` | `payload` `run` | Sends a named signal. Edge-triggered: the waiter must already be suspended. |
| `workflow.wait` | `run` | `timeout` | Blocks until another `run` reaches a terminal status. |
| `workflow.cancel` | `run` | `reason` | Cancels another `run` with a `reason`. |
| `emit` | — | `note` `audit` `metric` `value` | Emits an internal event other workflows can start on (`kind: event`). |

### Intelligence — the steps that cost tokens

| Kind | Required | Other fields | What it does |
|---|---|---|---|
| `think` | `prompt` | `output_schema` `reads` `check` `retries` `skills` `system` | One model call. `output_schema` shapes the answer; `check`/`retries` re-ask until it conforms. |
| `agent` | `instruction` | `output_contract` `output_schema` `tools` `servers` `limits` `context` `skills` `system` | A full agentic loop with tools — think, call, observe, repeat. `tools`/`servers` narrow what it may reach. |
| `subagent` | `instruction` | `mode` `workflow` `tools` `servers` `limits` `context` `output_contract` `output_schema` `skills` | A child PROCESS running its own loop, with narrowed tools and trust. The supervisor can always kill it. |
| `classify` | `input` `classes` | `prompt` `skills` | Puts `input` into one of `classes`. |
| `extract` | `input` `output_schema` | `prompt` `skills` | Pulls `input` into the shape of `output_schema`. No tools — the safest way to read untrusted text. |
| `summarize` | `input` | `length` `prompt` `skills` | Shortens `input` to `length`. |
| `judge` | `input` `rubric` | `prompt` `skills` | Scores `input` against a `rubric`. |
| `route` | `input` `choices` | `prompt` `skills` | Picks one of `choices` for `input`. The model-driven alternative to `switch`. |
| `human` | `question` | `schema` `to` `timeout` `reply_uri` | Asks a person `question` and suspends durably until they answer. `to` targets a channel; the answer can arrive after a restart. |
---

## Cross-cutting fields

Every step accepts these regardless of kind:

| Field | Effect |
|---|---|
| `depends_on` | the DAG edge. A non-start step with no dependency is refused as an unreachable root |
| `when` | a CEL guard; the step is skipped when it is false (needs `cel`) |
| `retry` | `{max, backoff}` — retry the step on failure |
| `timeout` | bound the step; a suspended step resumes as timed out |
| `on_error` | `fail` (default) · `continue` · `goto:<step>` |
| `output_schema` | validate the step's output; also SHAPES the answer for model kinds |
| `cache` | `{key, ttl}` — reuse a previous result |
| `budget` | a per-step token/step allowance |
| `skills` | skills to load for a model step |
| `idempotent`, `on_replay` | how a replayed step behaves after a crash (`retry`\|`skip`\|`fail`) |
| `description`, `otel` | documentation and trace attributes |

## Durability, and what a restart does

Every step checkpoints before its effect. On restore:

- A step that was **Running** is replayed (`on_replay` decides: retry, skip, fail).
- A step that was **Suspended** — `wait`, `sleep`, `human`, `a2a.wait` — resumes
  waiting, with its deadline intact. A human gate opened before a restart can
  still be answered after it.
- A **timer** whose durable record went missing is repaired at restore rather
  than leaving the step unreachable.

Effects can therefore run twice. Mark the ones that must not with `idempotent`,
or make the underlying tool idempotent.

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

Each entry carries `fields`, `required`, `start` and `implemented`. That is the
authority; this page is prose around it.
