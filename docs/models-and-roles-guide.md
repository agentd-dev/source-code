# Choose models for each job

An agent does more than answer: it triages incoming messages, summarises its
transcript, checks its goal, approves on your behalf and runs workflow steps.
This guide is for builders and operators who want each of those jobs on the
model that suits it: a frontier model for the answer, a small one for
triage, a reasoning model for hard problems, and a decision model for typed
choices. It assumes you have connected your providers
([Connect model providers](connect-models.md)).

## Declare your models

Give each model you use a short name under `intelligence.models`. The name is
what the rest of the configuration refers to; the provider only ever sees
the wire model. This page uses the same cast throughout:

| Name | Model | Kind | For |
|---|---|---|---|
| `smart` | `gpt-5.1` (`claude-sonnet-5` on Anthropic) | chat | the agent's answers |
| `fast` | `gpt-5-mini` (`claude-haiku-4-5` on Anthropic) | chat | triage, summaries, cheap steps |
| `deep` | `claude-opus-5-5` | reasoning | problems that need thought |
| `jev` | `typesafe/jev-1.13` | decision | typed choices and yes/no questions |

```yaml
intelligence:
  # The top-level connection: OpenAI.
  endpoints: https://api.openai.com/v1/chat/completions
  token: "{{secret:OPENAI_API_KEY}}"

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
    smart: {model: gpt-5.1, fallback: fast}
    fast:  {model: gpt-5-mini}
    deep:  {model: claude-opus-5-5, provider: anthropic, kind: reasoning, reasoning: {effort: medium}}
    jev:   {model: typesafe/jev-1.13, provider: openrouter, kind: decision, escalate: smart}

  roles:
    turn: smart
    think: deep
    decide: {model: jev, confidence: 0.7}
    preflight: fast
    compaction: fast
    goal: fast
    approve: smart
```

Each model entry takes `model` (required), `provider`, `kind`, `window`,
`fallback`, and for its kind `reasoning` or `escalate`. A model's `window`
tells compaction the real context size, so it does not guess from the name.
Every name is checked at load. Once any model is declared, a role, node or
route that names an undeclared one is refused, with the list of models that
exist. With no models declared, a role or node names a wire model on the
top-level connection directly.

## Bind each job to a role

A role is a job agentd does on its own. Bind it to a model by name, or as
`{model, confidence}` for a role that decides:

| Role | Runs | Accepts | Threshold |
|---|---|---|---|
| `turn` | the agent's turns, `agent`/`extract`/`summarize` nodes, subagents | chat, reasoning | — |
| `think` | the `think` tool and `think` nodes | chat, reasoning | — |
| `decide` | `decide`, `classify`, `route` and `judge` nodes, the `decide` tool, decision guards | any kind | `confidence` |
| `preflight` | triage of an incoming message before its turn | any kind | `confidence` |
| `compaction` | summarising the transcript when it grows too large | chat, reasoning | — |
| `goal` | the goal check | any kind | `confidence` |
| `approve` | asks answered on your behalf | any kind | `confidence` |

Every role defaults to `turn`, and no role inherits from another, so binding
`decide` moves only what decides. `turn` and `intelligence.model` are the
same setting: set one, or the load is refused with `say it once`. The flag
`--model` sets it too.

A node with no `model:` runs on its kind's role. A node's own `model:` wins:

```yaml
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
    fast:  {model: gpt-5-mini}
    deep:  {model: claude-opus-5-5, provider: anthropic, kind: reasoning}
  roles:
    turn: smart
    think: deep
    decide: fast

workflows:
  - name: support
    steps:
      start:
        kind: once
        inputs: {ticket: "I was charged twice for my invoice."}
      sort:                       # classify runs on roles.decide: fast
        kind: classify
        depends_on: [start]
        input: "{{ inputs.ticket }}"
        classes: [billing, bug, question]
      plan:                       # think runs on roles.think: deep
        kind: think
        depends_on: [sort]
        prompt: "Plan a reply to this ticket: {{ inputs.ticket }}"
        effort: high
      reply:                      # a node's model: wins over its role
        kind: agent
        depends_on: [plan]
        model: fast
        instruction: "Write the reply, following this plan: {{ steps.plan.output }}"
      done:
        kind: finish
        depends_on: [reply]
```

A subagent runs on the `model` its spawn names, else its template's, else
`subagents.defaults.model`, else `turn`. A budget's `degrade.model` is a
model reference too. All of these must be models that write, never a
decision model.

## Use a reasoning model

A reasoning model thinks before it answers. Declare it `kind: reasoning`,
with default `reasoning: {effort, budget}`; effort is `low`, `medium` (the
default) or `high`. It can serve anywhere a chat model can, and `think` is
the natural role for it: the agent calls the `think` tool when a problem
needs it, and pays for reasoning only then.

An `effort:` on a `think` or `agent` node, or in a `think` tool call,
overrides the model's effort for that call. Naming an effort where the model
does not reason is refused, at load for a node and as a tool error for a
call.

With Anthropic as your only provider, `smart` is `claude-sonnet-5` and
`deep` thinks adaptively at the effort you set:

```yaml
intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  models:
    smart: {model: claude-sonnet-5}
    deep:  {model: claude-opus-5-5, kind: reasoning, reasoning: {effort: high}}
  roles:
    turn: smart
    think: deep
```

Three things to know:

- **`budget` is for models that take a thinking budget.** It sets the
  thinking tokens directly (at least 1024). Current Claude models think
  adaptively and refuse a budget, so leave it out for them.
- **On Bedrock the model id must name its family.** agentd sends Claude
  and OpenAI open-weight models different reasoning fields, so a reasoning
  model whose id is an application inference profile ARN, which names no
  family, is refused at load.
- **Thinking costs tokens.** agentd raises the call's output cap by the
  thinking room (`budget`, else 2048, 8192 or 24576 tokens by effort), and
  budget admission counts it. See [Control spend with budgets](budgets.md).

The model's reasoning stays within the turn that produced it. It is never
logged, never stored in the transcript, and never sent to another model.

## Use a decision model

A decision model answers typed questions about a state, with calibrated
probabilities, and writes no text. It is fast and cheap for the choices an
agent makes all day. Put it on a `dialect: decisions` provider, declare it
`kind: decision`, and bind it to `decide`:

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
    jev:   {model: typesafe/jev-1.13, provider: openrouter, kind: decision, escalate: smart}
  roles:
    turn: smart
    decide: {model: jev, confidence: 0.7}

workflows:
  - name: triage
    steps:
      start:
        kind: once
        inputs: {ticket: "The app crashes on login and I cannot work."}
      triage:
        kind: decide
        depends_on: [start]
        state: "{{ inputs.ticket }}"
        questions:
          queue:  {choice: {billing: "payment problems", tech: "bugs and outages"}}
          urgent: {noul: "The customer cannot work at all."}
          mood:   {score: [calm, annoyed, angry]}
        confidence: 0.8
        on_unsure: escalate
      page:
        kind: agent
        depends_on: [triage]
        when:
          noul: "The customer cannot work at all."
          state: "{{ inputs.ticket }}"
        instruction: "Open an incident for: {{ inputs.ticket }}"
      done:
        kind: finish
        depends_on: [page]
```

A `decide` node asks up to 16 questions about one `state` in one call. A
`choice` picks one option, a `noul` gives the probability that a statement is
true, and a `score` places the state on a scale of levels. On a decision
model, `classify`, `route` and `judge` are the one-question forms of the same
call. Each answer is logged on an
`intel.decision` line, with its confidence; the state is not logged:

```
{"event":"intel.decision","model":"typesafe/jev-1.13","escalated":false,
 "answers":{"queue":{"choice":"tech","confidence":0.91,"calibrated":true},
 "urgent":{"noul":0.96,"confidence":0.92,"calibrated":true},
 "mood":{"score":1.4,"level":"annoyed","confidence":0.9,"calibrated":true}},
 "unsure":[], …}
```

### Confidence and escalation

`confidence` is the least sureness an answer is acted on with. For a `noul`
it is the distance from a coin flip, doubled: a probability of 0.9 has
confidence 0.8. A node's `confidence` wins; without one, the `decide` role's
applies. `on_unsure` says what an answer under it does, and needs a
`confidence` on the same node:

| `on_unsure` | An unsure answer |
|---|---|
| `escalate` | is asked once more on the model's `escalate` model, whose answer is final |
| `fail` | fails the node |
| `continue` | is handed on, marked `unsure: true` |

The default is `escalate` when the model names an `escalate` model, else
`fail`, so a threshold you set is never quietly ignored. `on_unsure: escalate`
on a node whose model names no `escalate` model is refused at load. A
decision model's `escalate` and `fallback` may be chat models.

### Without a decision model

Every deciding job also runs on a chat model. agentd asks all the questions
in one structured call and checks the answer the same way. The confidence is
then the model's own report (`calibrated: false`), and there is nothing to
escalate to. So a workflow written against `decide` works before you add a
decision model, and gets cheaper and better calibrated when you do.

The agent reaches the same contract through the `decide` tool, on the
`decide` role. An unsure answer is escalated when it can be, and otherwise
handed back marked `unsure` for the agent to weigh.

## Guard a step with a decision

A step's `when:` can be a decision instead of a CEL expression, as `page`
shows above. The guard runs as a `decide` step of its own, `page.when`, with
one question. The guarded step runs only when the statement holds
(probability at least 0.5) and the answer is sure; otherwise it is pruned,
with whatever depends on it alone.

| Field | Default |
|---|---|
| `noul` | required: the statement the step runs on |
| `state` | the outputs of the step's dependencies, by step id; with none, the run's inputs |
| `confidence` | the `decide` role's |
| `model` | the `decide` role's model |

An unsure guard is escalated when the model names an `escalate` model, and
otherwise prunes the step. A guard never fails the run for doubt. A guard
whose model is a chat model, such as the turn model when nothing binds
`decide`, is answered through a structured call. The
[Workflow node reference](node-registry.md) lists the rules for guards on
routed steps and the cases refused at load.

## Put the deciding roles on a decision model

`preflight`, `goal` and `approve` decide too, and run natively on a decision
model:

```yaml
agent:
  approval: auto
goal:
  statement: "Every open ticket has a reply."
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
    jev:   {model: typesafe/jev-1.13, provider: openrouter, kind: decision, escalate: smart}
  roles:
    turn: smart
    preflight: {model: jev, confidence: 0.6}
    goal: {model: jev, confidence: 0.8}
    approve: {model: jev, confidence: 0.9}
```

| Role | On a decision model | An unsure answer |
|---|---|---|
| `preflight` | the message's intent, whether it needs a plan, its risk, the route (with routes set), and one yes/no per catalogued skill, as one decision | no verdict: the message is treated as a task |
| `goal` | `achieved` and `stuck` as two statements about the agent's status | neither achieved nor stuck |
| `approve` | a closed ask (an enum or yes/no answer) as a choice; an open question goes to the `escalate` model when that model writes | the ask stays undecided |

On a decision model, preflight asks about at most 13 skills (12 when routes
are set); a `preflight.skills_capped` line says when more were left out. A
plan or clarifying question the message calls for is then the turn's to
write, and the turn is told.

`approve` on a decision model must set a `confidence`, and an escalated
answer is held to it too. Whatever the model, the approve judge never
answers an ask whose run, subagent or conversation carries outside input:
the ask fails with `human.judge.withheld`. See
[Human in the loop](human-in-the-loop.md).

## Route each turn to the right model

`intelligence.routes` lets preflight pick the model for each message. Name
each route's model and when to use it:

```yaml
agent:
  preflight: always
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
    fast:  {model: gpt-5-mini}
    deep:  {model: claude-opus-5-5, provider: anthropic, kind: reasoning, reasoning: {effort: high}}
  roles:
    turn: smart
    preflight: fast
  routes:
    fast: "a quick answer, a status question or one simple operation"
    deep: "multi-step work, analysis, or anything that needs careful reasoning"
```

Preflight answers one more question for each message it looks at: which
route. The turn that answers that message runs on the chosen model instead
of `roles.turn`, and agentd logs it:

```
{"event":"turn.routed","ctx":"root","model":"fast", …}
{"event":"turn.model","role":"turn","model":"gpt-5-mini", …}
```

The turn stays on `roles.turn` when preflight gives no verdict, when the
route's confidence is under the preflight role's (`preflight.route_unsure`),
or when a reload removed the route while the message waited
(`turn.route_removed`). Only that turn moves; the next message is routed on
its own.

- **Each route is a declared chat or reasoning model**, with a criterion;
  32 routes at most.
- **Routes need preflight.** With `agent.preflight: never` they are refused
  at load. Under the default `auto`, preflight looks only at long messages,
  messages that ask for work (build, fix, deploy, review, …) and messages in
  a conversation with an open plan; the others run on `roles.turn`. Set
  `always` to route every message.
- **The transcript fits the smallest window.** Any route may answer the
  next message, so compaction keeps the transcript within the smallest
  window among the turn model and the routes. Declare each model's `window`.

## What an instruction document may set

An instruction document can choose *which* model does what, but never *where*
it lives. A `:::!config` block may declare models and bind every role except
`approve`:

```markdown
# Support agent

Answer billing questions from customers.

:::!config
intelligence:
  models:
    fast: {model: gpt-5-mini}
  roles:
    preflight: fast
    compaction: fast
:::
```

The operator keeps the rest. A document cannot write `intelligence.providers`,
`intelligence.routes`, `roles.approve`, endpoints, credentials or headers, and a
model it declares cannot name a `provider`. A document that tries is refused
at load, and the message names each path. The approve judge resolves in the
operator's configuration, so a document cannot pick the judge of its own
gates; only an operator who names no model at all leaves that choice to the
document. When the operator names the turn model, in either spelling, the
document's is dropped with a warning. See
[Instruction documents](directives.md).

## Check what you configured

Print the assembled configuration and where each setting came from:

```bash
agentd --effective-config --config models.yaml
```

At run time each turn, workflow step, preflight, compaction, and `think` or
`decide` call logs its role, wire model and provider on a `turn.model` line:

```
{"event":"turn.model","agent_path":"preflight/root","role":"preflight","model":"typesafe/jev-1.13","provider":"openrouter", …}
```

Roles, models and routes reload live, so you can move a job to another model
with `SIGHUP` and no restart.

## Next steps

- [Triage with a decision model](decision-triage.md): a tutorial that
  builds a triage workflow end to end.
- [Control spend with budgets](budgets.md): cap what each job may spend.
- [Models, providers and roles](intelligence.md): how resolution, fallback
  and escalation work.
- [Workflow node reference](node-registry.md): every field of the deciding
  and generating nodes.
