# Workflow language reference

This page lists the rules of the workflow document beyond the node kinds: its
top-level keys, durations and rates, templates and CEL, statuses, limits,
load-time errors, the build features that nodes depend on, and the tools and
A2A commands that manage workflows at runtime. It is for builders writing
workflows and for operators reading a load error. For each node kind's
fields, see the [Workflow node reference](node-registry.md).

## The workflow document

A workflow is one entry of `workflows:` in a configuration file, or a
standalone file the configuration points at. Its keys are a closed set; an
unknown key is a parse error.

| Key | Meaning |
|---|---|
| `name` | required; `[a-zA-Z_][a-zA-Z0-9_-]{0,63}`; one definition per name |
| `description` | free text; a workflow tool's description |
| `steps` | required; a map of step id to step |
| `armed` | default `true`; `false` loads the definition without arming its start nodes |
| `inputs` | `{schema}`: a JSON Schema each run's inputs must match; a mismatch creates no run |
| `outputs` | `{schema}`: a completed run's output must match, or the completing `finish` fails |
| `concurrency` | `{max_runs, on_overflow, scope}`: default 4 live runs (1 to 1024), `queue`, `workflow`. `on_overflow` is `queue`, `drop` or `replace` (cancel the oldest live run); `scope: key` counts runs per `key` |
| `key` | a template rendered from the trigger payload naming what a run is about; required by `scope: key` |
| `limits` | `{steps, tokens, deadline, budget}` for each run. `steps` and `tokens` default to `limits.run.*`; `budget` is a budget scoped to the run, as in [Control spend with budgets](budgets.md) |
| `priority` | `low`, `normal` (default) or `high`: under resource pressure `low` admissions shed first, and ready steps of higher-priority runs are scheduled first |
| `durable` | `false` keeps runs in memory only; unset follows `store.durability.work` (default durable) |
| `state` | `{<var>: {schema?, reducer?}}`: a schema every write to the run variable must match, and how concurrent writes combine (`overwrite`, `append`, `merge`, `union`) |
| `unload` | `{policy, timeout}`: what retirement does to live runs: `drain` (default; `timeout` bounds it, then the rest are cancelled), `cancel` or `detach` |
| `tool` | `{name, mode, grant}`: register the workflow as a tool; configuration only. `mode` is `sync` (default) or `async`; `grant` keys are `root`, `workflows`, `subagents`, `user`, `agent` |
| `act_for` | `{principal, via}`: the person the workflow's `schedule`, `loop` and `once` firings act for with nobody there (needs `xaa`); `principal` is `{source: oidc, issuer, sub}` or `{source: operator \| x509, id}`, `via` is `ciba` or `standing`; the operator's configuration only. See [Acting for a person](modes-and-triggers.md#acting-for-a-person) |

A `workflows:` entry can name a document instead of holding it:

| Entry | Loads |
|---|---|
| `{name, file: <path>}` | a file on disk |
| `{name, url: <https URL>, headers?, timeout?, allow_private?}` | a document fetched at startup; unreachable fails the load |
| `{name, uri: <MCP resource>}` | a resource a connected MCP server serves |
| `{dir: <path>}` or `{dir: {path, glob?, order?}}` | every matching file in a folder; `glob` defaults to `*.yaml,*.yml,*.json` (`**` recurses), `order` is `name` (default) or `date` |

A standalone file gets editor completion from the published schema with
`# yaml-language-server: $schema=https://agentd.dev/schema/workflow.json`
as its first line. `agentd --workflow-schema` prints the same schema from
the binary you run.

## Durations and rates

| Value | Accepted forms |
|---|---|
| a duration | an integer and a unit: `500ms`, `30s`, `10m`, `2h`, `1d`, `1w`; a bare number is seconds. In a workflow field also `{ms: <n>}`. No compound forms such as `1h30m` |
| a rate | `"<burst>/<per>"`: `"20/1s"`, `"10/1m"`, `"1/1d"`; a bare or `s`/`sec` window is seconds |

## Templates

Every field of a step is rendered against the run's data immediately before
the step runs, except the fields the step evaluates itself (below).

| Rule | Example | Result |
|---|---|---|
| A string that is exactly one placeholder yields the typed value | `"{{ steps.fetch.output.json }}"` | the object |
| Any other string interpolates; strings raw, other values as JSON | `"id={{ inputs.id }}"` | `"id=42"` |
| `|` supplies a fallback for a missing path, parsed as JSON if it parses | `"{{ inputs.limit | 10 }}"` | `10` |
| A missing path with no fallback fails the step | `"{{ steps.fetch.outptu }}"` | step error |
| A leading `/` reads a JSON-pointer path, for ids with dots | `"{{ /steps/refund.when/output }}"` | the guard's answer |
| `CEL:` evaluates an expression over the same names | `"CEL: size(steps.list.output) > 0"` | `true` |
| `{{secret:NAME}}` and `{{secret-file:PATH}}` are left as written | `"Bearer {{secret:TOKEN}}"` | unchanged |

Things to know about the rules:

- A fallback applies only when a path is missing. A step that was pruned,
  skipped or has not run yet has `output: null`, which is present, so the
  fallback does not apply.
- A render error happens after the attempt counter moved, so it uses up one
  `retry`.
- A path's step id is checked at load: one naming a step the workflow does
  not have is refused. What follows it is read at run time, so a typo there
  is a step failure when the step runs.
- Secret references resolve only where a node resolves them: an `http`
  step's `headers` and `sign.secret`, and a webhook's `auth`. Anywhere else
  the destination receives the literal text. A credential never lands in
  step data or logs.
- An output larger than `limits.inline_max_bytes` (default 65 536 bytes) is
  stored as an artifact and replaced by `{"$artifact": <id>, "size": <n>}`
  in the run record.

### Namespaces

| Name | Holds |
|---|---|
| `inputs` | the run's validated inputs |
| `run` | `id`, `workflow`, `start`, `principal`, `task`, `attempt`, `status` |
| `steps.<id>` | `status`, `output`, `error`, `attempt`; inside a body, siblings by their unscoped id |
| `vars` | the run variables `assign`, `transform` and `collect` wrote |
| `env` | `instance`, `run`, `ts` (the current time, in ms), `instruction`, `prompt`; per step also `step`, `attempt` and `idempotency_key` |
| `memory.<key>` | a durable memory key, read each time a step renders, for the keys the definition names |
| `item`, `index`, the `as` alias | the current element in a `foreach`, `batch`, `map`, `filter` or `reduce` |
| `batch`, `iteration`, `branch` | the batch number, the `iterate` iteration, the `parallel`/`race` branch name |

A start node's `inputs` mapping is rendered against a different pair:
`payload` (the trigger payload) and `env`.

`env.idempotency_key` is the same on every retry and replay of the step;
`env.ts` changes every attempt. Use the first for anything that must be
stable.

### Fields that are not rendered

These are evaluated by the step itself or hold nested definitions, so they
are passed as written: `assert.condition`, `validate.schema`,
`map`/`filter`/`reduce.expr`, `iterate.while`/`until`, every `body` and
`branches`, the `filter` of `subscribe`, `signal` and `event`,
`wait.condition`, `wait.match`, `think.check` and `switch.cases`.

## CEL

CEL expressions need the `cel` build feature, which the release binaries
include. A build without it refuses a configuration that uses CEL (exit `2`)
rather than skipping the expression. In a field that is always CEL, the
`CEL:` prefix is optional.

| Where | Sees |
|---|---|
| a step's `when` | the run's data: `inputs`, `run`, `steps`, `vars`, `env`, `memory` |
| `assert.condition`, a `CEL:` template | the run's data |
| `map`/`filter`/`reduce.expr` | the run's data, `item`, `index`, the `as` alias; `reduce` adds `acc` and its alias |
| `iterate.while`, `iterate.until` | the run's data, `iteration`, `last`, `results` |
| `loop.until` | `outcome` (`{ok, output}`) and `last` |
| `filter` on `signal` and `event` | `payload` |
| `filter` on `subscribe` | `content` |
| `wait.match` | `event`, `inputs`, `vars` |
| `wait.condition` | `runs`, `subagents`, `now_ms`, `signals` only |

A CEL error in a `when` guard fails the run. When a guard may meet missing
data, put the value in a run variable first with an `assign` whose template
has a fallback, and test the variable.

## Statuses

| Run status | Terminal | Reached by |
|---|---|---|
| `running` | no | the normal state, even when every step is suspended |
| `paused` | no | an operator hold, or a breakpoint |
| `completed` | yes | a `finish` with the default status |
| `failed` | yes | an unhandled step failure, a run limit or deadline, or a `finish` with another status |
| `refused` | yes | a `finish` with `status: refused`, or a restore that found neither the pinned definition nor one with its hash |
| `cancelled` | yes | an operator or a parent run, or a `finish` with `status: cancelled` |
| `stalled` | yes | nothing ready, nothing in flight, no failed step, and no `finish` reached |

| Step status | Terminal | Satisfies dependents | Meaning |
|---|---|---|---|
| `pending` | no | no | not started |
| `running` | no | no | in progress, checkpointed |
| `suspended` | no | no | waiting on a durable record: a wait, sleep, gate, timer or retry backoff |
| `done` | yes | yes | finished, or failed under `on_error: continue` |
| `skipped` | yes | yes | a sibling start node fired, `on_replay: skip`, or a false guard inside a body |
| `pruned` | yes | no | a guard declined, or a branch nobody chose |
| `failed` | yes | no | an error |
| `timeout` | yes | no | its deadline passed |
| `cancelled` | yes | no | the run ended first, or a race was lost |

When a workflow run decides how a one-shot process exits, its status maps
to the exit code: `completed` 0, `refused` 5, `stalled` 3, `cancelled` 1,
and `failed` by its cause: 7 for an exhausted budget, 124 for a passed
deadline, 4 for an unreachable model, 1 otherwise. The full table is in
[CLI, environment and exit codes](cli.md).

## Limits

| Limit | Value |
|---|---|
| node kinds | 74: 11 start nodes, 63 steps |
| top-level steps per workflow | 512 (body steps not counted) |
| body nesting depth | 4 |
| step id | 64 characters, `[a-zA-Z_][a-zA-Z0-9_-]{0,63}` |
| `retry.max` | capped at 20; backoff doubling stops growing after 10 doublings |
| step timeout when unset | `limits.step_timeout`, 600 s (model steps, `mcp.tool`) |
| `http` timeout when unset | 30 s |
| `a2a.send` / `a2a.delegate` timeout when unset | 30 s / 120 s |
| `human` gate timeout when unset | 24 h |
| `wait` with no `timeout` | waits indefinitely |
| live runs | 4 per workflow (1 to 1024), 8 per instance (`limits.max_runs`) |
| `foreach`/`batch` parallel lanes | default 4, at most `limits.workflow.fan_out` (8 when unset) |
| `foreach` / `batch` group size | default 1 / 10 |
| `iterate.max_iterations` | default and cap 10 000 |
| `subscribe.window.samples` | 1 to 256 |
| `stream.batch.size` | 2 to 1000 |
| `correlate.max_pending` | default 1000, at most 100 000 |
| `decide` | 16 questions, 2 to 32 options or levels each |
| chained `message` deliveries | `limits.max_message_depth`, default 8 |
| inline output size | `limits.inline_max_bytes`, default 65 536 bytes |

## Load-time errors

`agentd --validate-config` reports every error at once on stderr as
`config.invalid` lines and exits `2`. The messages for workflows:

| Message (abridged) | Cause |
|---|---|
| `unknown field "prompt" for kind "agent" (allowed: …)` | a field the kind does not take; the message lists the ones it does |
| `kind "agent" requires field "instruction"` | a required field is missing |
| `a \`finish\` step is required` | no `finish` in the workflow |
| `depends_on names unknown step "x"` | a typo in `depends_on` |
| `a non-start step must depend on something (unreachable root)` | a step with no `depends_on` that is not a start node or an `on_timeout` target |
| `not reachable from any start node` | no path from a start node |
| `cycle among steps [...]` | the graph is not acyclic |
| `on_timeout names unknown step`, `on_error goto names unknown step` | a routing edge to a step that does not exist |
| `switch case "a" must name ONE step as a string (got a list)` | `cases` values are step ids, not lists |
| `` `on` is fixed by the definition … so it cannot be templated `` | a templated `wait.on` |
| `wait.on "x" is none of resource, condition, …` | a wait on something agentd does not wait on |
| `` `wait {on: mcp_task}` needs `task` `` | a wait on an MCP task that names none |
| `detach is true or false, written out` | a `detach` that is not a boolean |
| `` `detach` cannot be combined with `cache` `` | a cached output would name another run's task |
| `input is taken as written`, `input.to: …` | an `input` that is templated, or addressed to someone who could never see the gate |
| `skills is a list of skill references` | a `skills:` entry that is not a non-empty string |
| `concurrency.scope: key needs a \`key:\` template` | `scope: key` without `key` |
| `workflow "x" is defined twice — by … and by …` | two sources define one name |
| `a schedule needs \`cron\`, \`every\` or \`at\`` | a schedule that could never fire |
| `parallel 16 exceeds limits.workflow.fan_out (8)` | a fan-out over the ceiling |
| `on_unsure: escalate, but its model (smart) names no \`escalate\` model` | nothing to escalate to |
| `a start node cannot carry a decision guard` | a `when: {noul}` on a trigger |
| `human.to: \`to\` names role user, who could never see the task` | a gate addressed to someone who cannot see it |
| `a \`webhook\` node (start or wait) is used but webhooks.listen is not set` | no webhook listener |
| `subscribes to "…" on mcp server "…", which no \`event_grants\` entry grants` | a subscribing wait outside an operator grant |
| `act_for beside a webhook start (act_for_trigger_not_allowed)` | `act_for` beside a start an outsider can fire, or a `webhook` wait |
| `act_for names a device principal (act_for_device_principal)` | a device principal, which delegates only through a live session |
| `act_for.via …: the template … allows no such firing` | the person's template does not allow the `via` |

Some checks need the live registry and run at startup rather than in
`--validate-config`; each exits `2`:

| Message | Cause |
|---|---|
| `tool "x" is unknown, disabled or not granted to workflows` | a `tool` step names a tool workflows cannot call |
| `` `detach` and `input` are about the MCP task a call becomes, and tool "x" is not on an MCP server's route `` | `detach` or `input` on a `tool` step whose tool no MCP server serves |
| `skills: skill "x" is ambiguous: name one of "a/x", "b/x"` | a step's bare skill name that two origins answer to |
| `mcp server "x" is not declared under mcp.servers` | an `mcp.tool` or `mcp.resource` step names a server the configuration does not declare |
| `stream "x" is not declared under \`streams:\`` | a `stream` start or an `emit` names an undeclared stream |
| `a template names step "x", which the workflow does not have` | a template path to a step id the workflow does not define |
| `on "x" is no event the runtime raises` | an `event` start on an event agentd never raises |

## Build features

A node or field that needs a feature the binary was built without fails
plainly: refused at load, or the step fails naming the feature. The release
binaries and the container image include every feature below.

| Feature | Needed by |
|---|---|
| `cel` | every CEL expression: `when` as a string, `assert`, `map`/`filter`/`reduce`, `iterate`, `loop.until`, start-node `filter`, `wait.match`, `wait.condition`, `CEL:` templates |
| `cron` | `schedule.cron` |
| `a2a` | the `webhook` start and `webhooks.listen`, `wait {on: webhook}`, `a2a.send`, `a2a.delegate`, the `a2a` start, and every human gate that is asked over the A2A listener |
| `mcp-events` | `wait {on: event, subscribe: …}` |

[Build features and crates](features.md) lists every feature.

## Managing workflows at runtime

The agent manages workflows through built-in tools. Each tool's default
grant says which callers are offered it: the agent's own turns (root),
workflow steps, and subagents.

| Tool | Callers | Arguments | Result |
|---|---|---|---|
| `workflow.run` | root, workflows | **`name`**, `inputs`, `start`, `wait`, `timeout` | `{run, status, output?, task?}` |
| `workflow.status` | all | `run` or `name` | `{runs}` |
| `workflow.list` | all | | `{workflows}` |
| `workflow.wait` | root, workflows | **`run`**, `timeout` | `{run, status, output}` |
| `workflow.cancel` | root, workflows | **`run`**, `reason` | `{ok, status}` |
| `workflow.signal` | all | **`name`**, `payload`, `run` | `{delivered: 0, note}` |
| `workflow.pause` | root | `run`, `name`, `before_step` | `{ok, break_before?}` |
| `workflow.resume` | root | `run`, `name` | `{ok}` |
| `workflow.create` | root | **`definition`**, `arm` | `{name, hash, armed}` |
| `workflow.update` | root | **`name`**, **`definition`** | `{name, hash}` |
| `workflow.delete` | root | **`name`** | `{ok}` |

- `workflow.signal` as a tool only records the signal on the durable inbox;
  waits and signal start nodes are woken on the next reactor pass, so
  `delivered` is always 0. Retrying because it was 0 sends the signal twice.
  The `workflow.signal` step delivers at once and counts.
- `workflow.pause` with `run` holds one run; with `name`, it disarms the
  workflow's start nodes. `before_step` sets a durable breakpoint: the run
  continues and pauses immediately before that step starts.
- `workflow.create`, `update` and `delete` act only on workflows created at
  runtime. A name the configuration defines is refused, naming where it is
  defined (`workflow.refused`); change it in the configuration and reload. A
  runtime definition may not carry a `tool:` block, and may open an inbound
  route (a `webhook` start or a `wait {on: webhook}`) only when the operator
  grants `interface` in `agent.document_capabilities`.
- When the configuration, after a reload or a restart, defines a name a
  runtime definition uses, the stored definition is discarded
  (`workflow.stored.shadowed`).

Over A2A, the [command extension](https://agentd.dev/a2a/ext/command) serves
these ops for workflows; the [A2A extensions](a2a-extensions.md) page has
the full table:

| Op | Who may call | Does |
|---|---|---|
| `workflow.run` | `user`, `agent`, or a grant | starts a workflow; the reply is the task it runs under |
| `workflow.status` | `user`, `agent`, or a grant | one run's status, or every run the caller started |
| `workflow.cancel` | `user`, or a grant | cancels one run |
| `workflow.signal` | a grant | delivers a signal |
| `run.get` | `user`, or a grant; needs `a2a.introspection.enabled` | one run with per-step status, timings, errors and output |
| `admin.pause`, `admin.resume` | operator | holds or releases the instance or one run |
| `admin.cancel` | operator | cancels any run |

A workflow whose `a2a` start declares a `command` adds that command as one
more op.

## Next steps

- [Workflow node reference](node-registry.md): every kind's fields and
  output.
- [Author workflows](author-workflows.md): the language in use.
- [Troubleshooting](troubleshooting.md): reading these errors in practice.
