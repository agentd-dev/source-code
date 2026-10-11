# Control spend with budgets

agentd counts every token its model calls spend and can stop, slow or
cheapen work before a limit is crossed. This guide is for operators and
builders who answer for that spend. It shows how to cap a single run, pace
an instance's spend per hour or day, set a lifetime ceiling, give each
caller and conversation its own allowance, and see where the tokens went.
Budgets are in tokens and requests; agentd keeps no prices.

## The controls

Two kinds of control work together. A **limit** bounds one unit of work and
stops it when the unit has spent its share. A **budget** is checked before a
model call starts, against windows that reset, and decides whether the call
runs now, later, on a cheaper model, or not at all.

| Control | Bounds | Kind |
|---|---|---|
| `limits.run.*` | one turn, one subagent, and by default one workflow run | limit |
| a workflow's `limits` | each run of that workflow | limit |
| a step's `budget` | one model step | limit |
| `intelligence.budget` | the whole instance, per window and for its lifetime | budget |
| `agent.conversation_budget` | each conversation a caller opens over A2A | budget |
| `a2a.principals[].quotas.budget` | everything done for one caller | budget |
| a workflow's `limits.budget` | each run of that workflow, per window | budget |
| `limits.subagents.*` | how many subagents, how deep, how fast | limit |

## Cap a turn or a run

`limits.run` bounds each unit of work the agent does. When a unit reaches a
limit it stops with that status, and a one-shot job exits with a budget code:

```yaml
intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  model: claude-sonnet-5

limits:
  run: {steps: 200, tokens: 500000, deadline: 15m}
  step_timeout: 5m

workflows:
  - name: digest
    limits: {tokens: 200000, deadline: 10m}
    steps:
      start: {kind: once}
      brief:
        kind: agent
        depends_on: [start]
        budget: 50000
        instruction: "Summarise yesterday's incidents in five bullets."
      done: {kind: finish, depends_on: [brief]}
```

| Key | Default | Bounds |
|---|---|---|
| `limits.run.steps` | 500 | model rounds in one turn or subagent; steps in a workflow run that sets no `limits.steps` |
| `limits.run.tokens` | 2,000,000 | tokens reported by one turn, subagent or model step; tokens in a workflow run that sets no `limits.tokens` |
| `limits.run.deadline` | 3600s | wall clock of one turn or subagent, a model call in flight included; a one-shot run takes it as its run deadline |
| `limits.step_timeout` | 10m | wall clock of one model step, unless the step sets `timeout` |
| workflow `limits.{steps,tokens,deadline}` | — | each run of that workflow; a run has no deadline unless its workflow sets one |
| step `budget` | — | tokens for that step's model loop |

The short flags are `--max-steps`, `--max-tokens` and `--deadline`, and the
environment variables `AGENTD_MAX_STEPS`, `AGENTD_MAX_TOKENS` and
`AGENTD_DEADLINE`. A one-shot run stopped by steps or tokens exits `7`; one
stopped by its deadline exits `124`. `--budget-exit-code <N>` remaps `7` (and
the partial-result code `3`) for an orchestrator that should not count a
budget stop as a failure. The full table is in
[CLI, environment and exit codes](cli.md).

## Pace spend with windows

`intelligence.budget.windows` caps what the instance spends per second,
minute, hour, day or week. A call that would overrun a window is held back
before it starts:

```yaml
intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  models:
    smart: {model: claude-sonnet-5}
    fast:  {model: claude-haiku-4-5, window: 200000}
  roles:
    turn: smart
  budget:
    windows:
      - {per: hour, tokens: 2000000}
      - {per: day, tokens: 20000000, reset: "06:00Z"}
    on_exhausted: degrade
    degrade: {model: fast}
```

Each window takes `per`, and `tokens`, `requests` or both:

- **Windows are fixed, not rolling.** An hour window is the clock hour. A
  `day` or `week` window resets at `reset` (`HH:MMZ`, UTC, default
  `00:00Z`), and weeks start on Monday.
- **`requests` counts admitted turns and model steps**, not the model calls
  inside them. A turn that makes ten calls is one request.
- **Counters survive a restart** when the instance has a durable store, so
  restarting never reopens a spent day.

Several windows apply together; the first one that would be overrun decides.

## Decide what happens when a window is spent

`on_exhausted` says what a held-back turn or step does:

| `on_exhausted` | The turn or step |
|---|---|
| `wait` (default) | waits until the window resets, then runs |
| `slow` | is paced to `slow.factor` (default 0.5) of the window's even rate, so spend spreads across the window; waits for the reset once the window is spent |
| `degrade` | runs on `degrade.model` instead, and is still counted |
| `refuse` | is not run: a turn is dropped with a note in its conversation, a step fails |
| `fail` | the same as `refuse`, with the reason `budget exhausted` |

`degrade` needs `degrade.model`, a declared model that writes. The degrade is
logged where it happens, at dispatch and in the worker:

```
{"event":"budget.degraded","ctx":"root","model":"fast", …}
{"event":"turn.model_degraded","from":"claude-sonnet-5","to":"claude-haiku-4-5", …}
```

Inside a running turn, each model call is admitted too. A wait there pauses
the turn between rounds, a degrade moves the rest of the turn to the degrade
model, and a refusal ends the turn.

## Set a lifetime ceiling

`lifetime_tokens` bounds everything the instance spends over its life. It
never resets, so the question it answers is what the process does once it is
spent:

```yaml
intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  model: claude-sonnet-5
  budget:
    lifetime_tokens: 500000000
    lifetime_exhausted: drain
```

| `lifetime_exhausted` | The instance |
|---|---|
| `drain` (default) | finishes the work in flight, then exits `0`, for an orchestrator to restart with a fresh allowance |
| `refuse` | stays up and refuses every new admission |
| `exit` | stops at once and exits `7` |

The unit that crosses the ceiling fails either way, and agentd logs one
`budget.lifetime_exhausted` line with the policy that applied. Set the ceiling
from the command line with `--budget-tokens-lifetime <N>` or
`AGENTD_BUDGET_TOKENS`; `0` is unbounded. A ceiling is the backstop for an
agent that reaches a provider with no metering gateway in between.

## Give each caller, conversation and run an allowance

A budget can also cover one conversation, one caller or one workflow's
runs: where you declare it decides what it covers. Each sub-budget takes the
same fields as `intelligence.budget` and is checked before the instance's,
so the tightest one binds and its own `on_exhausted` decides:

```yaml
intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  model: claude-sonnet-5
  budget:
    windows: [{per: day, tokens: 20000000}]

agent:
  # Each conversation a caller opens over A2A.
  conversation_budget:
    windows: [{per: day, tokens: 300000}]
    on_exhausted: refuse

a2a:
  listen: http://127.0.0.1:8080
  principals:
    - id: acme-portal
      match: {bearer_ref: "{{secret:ACME_PORTAL_TOKEN}}"}
      role: user
      # Every turn and run done for this caller.
      quotas: {budget: {windows: [{per: day, tokens: 2000000}], on_exhausted: refuse}}

workflows:
  - name: nightly
    # Each run of this workflow.
    limits:
      budget: {windows: [{per: hour, tokens: 1000000}], on_exhausted: fail}
    steps:
      start: {kind: schedule, cron: "0 2 * * *"}
      work: {kind: agent, depends_on: [start], instruction: "Reconcile yesterday's invoices."}
      done: {kind: finish, depends_on: [work]}
```

| Sub-budget | Charged with |
|---|---|
| `agent.conversation_budget` | the turns of each conversation a caller opens; not the agent's own root context |
| `a2a.principals[].quotas.budget` | the turns in that caller's conversations, and the runs started for that caller |
| a workflow's `limits.budget` | the model steps of each run of that workflow |

> **Warning.** A `lifetime_tokens` on a sub-budget, once spent, applies the
> instance's `intelligence.budget.lifetime_exhausted` policy, so by default
> the whole instance drains. Give conversations, callers and runs windows,
> and keep lifetime ceilings on `intelligence.budget`.

## What is charged

Every model call agentd makes is charged, to the instance budget and to the
sub-budgets of the work it ran for:

| Work | Admitted before it runs | Charged to |
|---|---|---|
| a turn | yes | instance, conversation, caller |
| a workflow model step, a decision guard | yes | instance, run, caller |
| the `think` and `decide` tools | yes | instance, conversation |
| preflight, compaction | no | instance, conversation |
| the goal check, the approve judge | no | instance |
| a subagent | no, its usage is charged as it reports | instance |

Admission works on an estimate, and the charge on what the provider
reports. The estimate for a turn or step is its context, its system prompt
and a 4,096-token allowance for the answer. Each call inside it adds the
tool schemas, the answer's schema and, for a reasoning model, the thinking
room (2,048, 8,192 or 24,576 tokens by effort, or the model's `budget`). A
decision that is escalated, and an emulated decision that is asked twice,
are charged for both calls.

## Cap subagents

Subagents spend from the same instance budget, and `limits.subagents` bounds
how many the agent may start. A spawn over a limit is refused as a tool
result, so the agent can adapt:

```yaml
intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  model: claude-sonnet-5
limits:
  subagents:
    depth: 2
    breadth: 4
    total: 32
    rate: "4/2s"
subagents:
  defaults:
    limits: {tokens: 200000, steps: 100, deadline: 10m}
```

| Key | Default | Bounds |
|---|---|---|
| `limits.subagents.depth` | 3 | how deep delegation may nest (`--max-depth`) |
| `limits.subagents.breadth` | 8 | subagents alive at once |
| `limits.subagents.total` | 64 | subagents over the instance's life |
| `limits.subagents.rate` | `8/2s` | spawns: a burst, refilled over the period |
| `limits.subagents.instances` | 2 live, 8 total, `4/1h` | child agentd instances |

Each subagent's own steps, tokens and deadline come from its spawn's
`limits`, else `subagents.defaults.limits`, else `limits.run`. See
[Delegate to subagents](delegate-subagents.md).

## Watch spend

The `budget` section of the instance status shows every window's cap, use,
reservation and next reset, the lifetime balance, each sub-budget and what
is waiting. Ask for it with the `status` command over A2A; see
[Operate a running daemon](operations.md).

The log tells you when a budget acted:

| Event | When |
|---|---|
| `budget.wait` | a turn or step is held until a window opens |
| `budget.degraded` | a turn or step is admitted on the degrade model |
| `turn.model_degraded` | a running turn moved to the degrade model |
| `budget.refused` | a turn was not run |
| `budget.lifetime_exhausted` | the lifetime ceiling was reached, once |

For attribution, `turn.model` names the model each turn or step runs on, and
the worker's `turn.done` line under the same `agent_path` carries its
`tokens_in` and `tokens_out`. `proc.exit` carries the process totals, and the
`agent_tokens_total{type="in"|"out"}` counter tracks them for Prometheus. To
put a price on it, apply your rate card to those counts downstream; agentd
records which model spent the tokens and leaves the money to you. Event and
metric fields are in the
[Events and metrics reference](telemetry-reference.md).

> **Limit.** agentd exports an `agent_budget_tokens_remaining` gauge but
> does not set it, so it reads `0`. Read the remaining balance from the
> status `budget` section.

## Traps

- **A window smaller than one estimate never admits.** Every turn reserves
  its context plus a 4,096-token answer allowance first. A `tokens` window
  or a lifetime ceiling below that refuses the first turn, or with `wait`
  holds it forever. Size windows at many times one turn.
- **A unit that dies is not charged.** A worker that is killed before it
  reports its usage has its reservation released, so what it spent never
  reaches the windows. A turn that merely fails still reports and is
  charged.
- **Reservations live in memory.** The durable counters carry settled
  usage only. If a window reads exhausted while recorded use is low, look at
  the `reserved` figure in status.
- **`requests` is not a provider's request rate.** It counts admitted turns
  and steps; one turn can make many calls.

## Next steps

- [Choose models for each job](models-and-roles-guide.md): put cheap jobs
  on cheap models before you cap them.
- [Monitor in production](observability.md): alert on the budget events
  and token counters.
- [CLI, environment and exit codes](cli.md): what each budget stop exits
  with.
- [Configuration reference](configuration.md): every budget and limit key.
