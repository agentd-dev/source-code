# Delegate to subagents

A subagent is a separate agentd process that does one bounded piece of work,
such as reading a codebase or researching a topic, and hands back a short
answer, so the parent agent keeps its context for the conclusions. This guide
is for builders and operators who hand work to children. It shows how to let
the agent delegate, delegate from a workflow, narrow and limit a child,
collect its result, and offer operator-reviewed templates, including
templates that run a whole child daemon. Each configuration here loads with
`agentd --validate-config`, and each fragment says where it goes.

## Decide whether to delegate

Delegate when the work needs a lot of reading and the parent needs only the
conclusion. The child pays for the exploration in its own context window and
returns at most 8,000 bytes of text; the transcript of its search never
reaches the parent. The process boundary also confines the work: the child
reaches only the MCP servers you hand it, under its own budget, and a wedged
child costs one process. [Subagents and delegation](subagents.md) explains the
model.

Do not delegate in these cases:

- **The parent needs the raw material.** Delegation is lossy by design. If
  the next reasoning step needs the 300 files, read them in the parent.
- **The work is a pipeline.** A fixed sequence with retries, waits and
  branches is a [workflow](author-workflows.md), which is durable and
  resumable. A subagent is one bounded agent loop.
- **The child must delegate onward or act as the agent.** A child has no
  `subagent.*` tools and no A2A peers. It does sign as the agent: with
  `security.aauth` it carries the daemon's AAuth identity.

## Try it locally

This configuration runs one delegation end to end on your machine. It uses
`mock:final`, agentd's built-in test endpoint, which answers every model call
with a canned final answer, so no key or network is needed. Save it as
`delegate-demo.yaml`.

```yaml
# A local demo: "mock:final" is agentd's built-in test endpoint, which
# answers every model call with a canned final answer.
agent:
  name: delegate-demo

intelligence:
  endpoints: "mock:final"
  model: mock

workflows:
  - name: delegate
    steps:
      start:
        kind: once
      scan:
        kind: subagent
        depends_on: [start]
        mode: sync
        instruction: "Summarise the release notes in three bullet points."
        output_contract: "Three bullet points, plain text."
        limits: {steps: 10, tokens: 20000, deadline: 1m}
        timeout: 2m
      done:
        kind: finish
        depends_on: [scan]
        status: completed
        output: "{{steps.scan.output}}"
```

Run it:

```sh
agentd --config delegate-demo.yaml
```

agentd starts the workflow, spawns one child, waits for it, prints the run's
output and exits 0:

```json
{
  "error": null,
  "handle": "sub-1",
  "result": "mock-llm done",
  "status": "completed"
}
```

The log on stderr shows the child's life: `subagent.spawn` with its handle,
pid and depth, then `subagent.result` with its status and token count.

## Let the agent delegate

The agent's model can call `subagent.run` whenever it decides a question is
worth a child. The `subagent.*` tools are granted to the agent's
conversation and to workflows by default, so the configuration only has to
describe the agent, its servers and the limits on its children. Save this as
`auditor.yaml`.

```yaml
agent:
  name: auditor
  instruction: |
    You audit the repository for unsafe API calls. When a question
    needs many files read, delegate the reading to a subagent with
    subagent.run, hand it only the repo server, and ask for a short JSON
    answer. Keep your own context for the conclusions.

intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  models:
    smart: {model: claude-sonnet-5}
    fast:  {model: claude-haiku-4-5}
  roles:
    turn: smart

mcp:
  servers:
    - name: repo
      endpoint: https://mcp-git.internal/mcp
      tags: { "*": [sensitive] }
    - name: issues
      endpoint: https://mcp-issues.internal/mcp
      tags: { "*": [untrusted_input] }

subagents:
  defaults:
    model: fast
    priority: low

limits:
  run: {steps: 200, tokens: 1000000, deadline: 30m}
  subagents:
    breadth: 4
    total: 200
    rate: "4/1s"
```

Export the key and check the file:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
agentd --validate-config --config auditor.yaml
```

When the model delegates, it sends arguments like these. The child connects
to `repo` only, may call only the two named tools, and runs on the `fast`
model from `subagents.defaults`:

```json
{
  "instruction": "List every call site of verify_token under src/auth.",
  "mode": "sync",
  "servers": ["repo"],
  "tools": ["search_code", "read_file"],
  "output_contract": "A JSON array of {file, line, snippet}. Nothing else.",
  "limits": {"steps": 80, "tokens": 400000, "deadline": "9m"}
}
```

Narrow in two steps:

- **`servers`** chooses which of the parent's MCP servers the child connects
  to. Omitting it hands the child every server the parent has, so name them.
  A name that matches no configured server is dropped without an error, which
  leaves a quieter child, never a wider one. To keep a server entirely out of
  reach, leave it out here: the child still connects to a server whose tools
  `tools` hides, and that server's trifecta tags still count.
- **`tools`** cuts individual tools inside those servers. Patterns are `*`,
  an exact name, or a prefix ending in `*`, matched against the names the
  agent's own catalogue shows: with a server's `ns` prefix, and as
  `<server>.<tool>` where a tool's name is already taken. The child enforces
  the grant on its catalogue and on every call.

A child sees each server as the agent does: the server's `allow` and
`exclude` and `tools.disabled` decide which of its tools exist for the child
too, and its tools carry the same names.

The child's instruction is the whole brief: it does not see the parent's
conversation. Pass background in `context`, a list of `{role, content}`
messages that seeds the child's transcript.

## Delegate from a workflow

A workflow `subagent` step spawns a child as part of a durable run. Its wait
is a step record, so it honours the step's `timeout` and resumes after a
restart. Save this as `scan.yaml`.

```yaml
agent:
  name: triage

intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  token: "{{secret:OPENAI_API_KEY}}"
  models:
    smart: {model: gpt-5.1}
    fast:  {model: gpt-5-mini}
  roles:
    turn: smart

mcp:
  servers:
    - name: repo
      endpoint: https://mcp-git.internal/mcp
      tags: { "*": [sensitive] }
    - name: tickets
      endpoint: https://mcp-tickets.internal/mcp
      tags: { "*": [untrusted_input] }

subagents:
  defaults: {model: fast}

store:
  kind: file
  file: {path: ./state}

workflows:
  - name: scan-callers
    steps:
      start:
        kind: once
      scan:
        kind: subagent
        depends_on: [start]
        mode: sync
        instruction: "List every call site of verify_token under src/auth."
        servers: [repo]
        tools: ["search_*", "read_file"]
        output_contract: "A JSON array of {file, line, snippet}. Nothing else."
        limits: {steps: 80, tokens: 400000, deadline: 9m, memory: 512MB, cpu: 5m}
        priority: low
        timeout: 10m
      check:
        kind: assert
        depends_on: [scan]
        condition: "steps.scan.output.status == 'completed'"
        message: "the scan did not finish"
      done:
        kind: finish
        depends_on: [check]
        status: completed
        output: "{{steps.scan.output.result}}"
```

The `check` step matters. A `subagent` step fails only when the spawn is
refused or its `timeout` passes. Once the child runs, the step completes with
the child's outcome in `steps.scan.output`, whatever that outcome was: a
child that ran out of steps completes the step with `status: exhausted_steps`
and its last text as the result. Test `output.status` before you use
`output.result`.

The step takes the same fields as the tool: `instruction` or `template`,
`params`, `mode`, `servers`, `tools`, `limits`, `priority`, `context`,
`output_contract`, `output_schema` and `durable`. The
[workflow node reference](node-registry.md) lists them with the other nodes.

## Choose the child's model

A child runs on the first of these that is set:

1. the template's `model`, for a templated spawn;
2. `subagents.defaults.model`;
3. the agent's turn model (`intelligence.roles.turn`, else
   `intelligence.model`).

The reference is resolved like any other model reference: a declared model
name brings its provider, wire name and fallbacks; a name that is not
declared is a literal wire model on the top-level connection. A decision
model is refused here at load, because a subagent writes:

```text
subagents.defaults.model: "jev" is a decision model; this needs one that writes (a chat model)
```

A chat or reasoning model works. When a reload changes the intelligence
configuration, each running child receives its model resolved again and
applies it at its next turn boundary under `intelligence.swap_policy`. See
[Choose models for each job](models-and-roles-guide.md) for picking the cast.

## Set limits

Two kinds of limit apply to a child, and both are enforced:

| Limit | Kind | Enforced by |
|---|---|---|
| `steps`, `tokens`, `deadline` | Protocol budget | The child's own loop, which stops before it spends more; the supervisor audits it |
| `memory` (for example `512MB`) | Kernel cap, `RLIMIT_AS` | The kernel, set between fork and exec |
| `cpu` (for example `5m`) | Kernel cap, `RLIMIT_CPU` | The kernel: SIGXCPU at the limit, SIGKILL 5 seconds later |
| `priority` | Niceness and admission | `low` runs at nice +10 and is shed one pressure level early; `high` asks for nice -5 (needs `CAP_SYS_NICE`, skipped without it) |

Each of `steps`, `tokens` and `deadline` that a spawn leaves out comes from
`limits.run`: 500 steps, 2,000,000 tokens and a 1-hour deadline by default,
with a 1-second floor on the deadline. `subagents.defaults.limits` supplies
the whole `limits` object when neither the call nor its template passes
one. Limit keys are not checked at load, so a misspelled key is ignored. A
malformed `memory`, `cpu` or `priority` value refuses the spawn.

The parent caps how many children exist:

```yaml
# fragment: under limits:
subagents:
  depth: 1            # default 3; 0 refuses every spawn
  breadth: 4          # live children; default 8
  total: 500          # every record this process has made; default 64
  rate: "4/1s"        # burst 4, refilling 4 per second; default "8/2s"
  instances:          # instance-tier children, capped on their own
    breadth: 2        # default 2
    total: 8          # default 8
    rate: "4/1h"      # default "4/1h"
```

`total` counts every record in the registry, finished and restored ones
included, so a long-lived daemon eventually refuses spawns with nothing
running. Raise it for daemons. `--max-depth` on the command line sets
`limits.subagents.depth`. The [configuration reference](configuration.md)
lists every key.

Each refusal reaches the model as a tool result it can read and adapt to:

```text
subagent.run refused: 4 subagents live (limits.subagents.breadth = 4)
```

## Collect results

`mode` decides how the caller waits:

| Mode | Returns | Collect with |
|---|---|---|
| `sync` (default) | The result, when the child finishes | Nothing more |
| `async` | `{handle, status: "running"}` at once | `subagent.await`, `subagent.status`, or a workflow `wait {on: subagent}` |
| `detached` | The handle at once | Nobody waits; a crash does not revive it, a drain's stop does |
| `warm` | The handle at once | `subagent.send` for each further turn; see below |

A result is `{handle, status, result, error}`. A string result longer than
8,000 bytes is truncated and marked `… [truncated]`, and a string that
parses as JSON arrives as an object. An object or array result is not capped,
so bound its size in `output_contract`. `output_schema` is folded into the
brief as text and is not validated for a subagent; check the shape yourself.

To run children in parallel from a workflow, spawn them `async` and wait on
each handle:

```yaml
# fragment: under workflows[].steps:
a:
  kind: subagent
  depends_on: [start]
  mode: async
  instruction: "Summarise chapter one."
b:
  kind: subagent
  depends_on: [start]
  mode: async
  instruction: "Summarise chapter two."
wait_a:
  kind: wait
  depends_on: [a]
  on: subagent
  subagent: "{{steps.a.output.handle}}"
  timeout: 5m
wait_b:
  kind: wait
  depends_on: [b]
  on: subagent
  subagent: "{{steps.b.output.handle}}"
  timeout: 5m
```

Each `wait` step's output is the child's `{handle, status, result, error}`.

The agent's conversation can learn of results without polling. Add
`subagent_result` to `agent.wake_on` and each finished child appends a note
of at most 400 characters to the root conversation. The list replaces the
default (`a2a_message`, `human_reply`, `workflow_failed`), so keep the
entries you rely on:

```yaml
# fragment: under agent:
wake_on: [a2a_message, human_reply, workflow_failed, subagent_result]
```

A plan item bound to a handle with `plan.update` advances on its own either
way. Notes are withheld from a conversation that holds both sensitive and
egress tools when the child may carry outside input
([Security model](security.md)).

A child that ends on its budget records `exhausted_steps`,
`exhausted_tokens` or `deadline`, and one that dies without reporting
(killed by a signal, out of memory, torn down by the supervisor) records
`failed`. Either way the record is finished: `subagent.await` and
`wait {on: subagent}` answer, and the child's `breadth` slot is free.

A drain is the exception. A child runs on through it and records its result
as usual; one the drain stops — still running at `lifecycle.drain_timeout`,
or `warm` and so stopped as the drain begins — records `parked`. That record
is not finished: a waiting step waits on, and the next start spawns the
child again. A `sync` child that a turn's `subagent.run` was waiting on
records `cancelled` instead. The drain stops that turn as well, and the
turn's replay calls `subagent.run` again, so the parked child would run
beside the new one.

## Keep a session warm

A `warm` child stays alive after its first turn and runs one more turn for
each message you send it, keeping its conversation. Use it for an
assistant that reacts to a stream of events with context it built earlier.

```json
{"instruction": "Watch the deploy and explain each failure you are sent.",
 "mode": "warm", "servers": ["repo"],
 "limits": {"steps": 30, "tokens": 200000, "deadline": "8h"}}
```

Then steer it with `subagent.send {handle, message}`. Messages sent during a
turn queue in order. Each turn gets a fresh step, token and deadline budget.

Three edges to plan for:

- **The supervisor's hard deadline is armed once.** It is the spawn's
  deadline plus 60 seconds, from the moment of the spawn. A session meant to
  live for hours needs a `deadline` that covers its whole life, as above.
- **A warm child's final result is null.** When it closes, its last record
  is overwritten with a null result. Read each turn's output from the root
  notes (`subagent_result` in `agent.wake_on`) or poll `subagent.status`
  while it is alive.
- **A drain stops it.** A warm child never ends on its own, so a drain
  cancels it as the drain begins and records it `parked`. The next start
  spawns it again from its instruction; the conversation it built is not
  kept.

## Stop a child

`subagent.kill {handle, reason}` ends a flat child within about 7 seconds,
wherever it is:

1. At once, the record becomes `cancelled`, with `reason` as its error, and
   the child is sent a cancel. Whatever waits on the child, such as
   `subagent.await`, a `wait {on: subagent}` step or the turn that ran it
   `sync`, is answered `cancelled` then.
2. A child between steps, waiting for the supervisor to answer a tool call
   or (warm) for its next message, reads the cancel and exits. It is never
   signalled.
3. A child inside a model call or an MCP tool call cannot read the cancel
   until that call returns. If it is still running 5 seconds after the kill,
   its process group is sent `SIGTERM`, and 2 seconds after that `SIGKILL`.
   Each signal is logged as `subagent.kill.escalated`, with the `signal`.

The record stays `cancelled` whatever the child does in those seconds: a
result it reports, or its death by a signal, does not change it, and the
next start does not spawn it again. The child reports each model call's
tokens as the call returns, so a child the signal ends is charged, and its
record counts, every call it finished; the call it was inside reports
nothing, though the provider may bill it.

On shutdown a child runs on until `lifecycle.drain_timeout` and is killed
only if it is still running then; a warm child is cancelled at once, then
sent SIGTERM at 5 seconds and SIGKILL at 7. A child the drain stops is
recorded `parked`, not `cancelled`, unless it is a `sync` child a stopped
turn was waiting on. A child already killed stays `cancelled`.
`subagent.kill` works on flat children only; retire an instance child with
`subagent.retire`.

## Offer reviewed templates

A template is a definition the operator writes and the model fills in. The
model chooses the template and supplies its declared parameters; it cannot
change the instruction, the servers or the limits. Set
`allow_freeform: false` and templates become the only way to spawn. Save this
as `research-desk.yaml`.

```yaml
agent:
  name: research-desk

intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  models:
    smart: {model: claude-sonnet-5}
    fast:  {model: claude-haiku-4-5}
  roles:
    turn: smart

mcp:
  servers:
    - name: search
      endpoint: https://mcp-search.internal/mcp
      tags: { "*": [untrusted_input] }
    - name: notes
      endpoint: https://mcp-notes.internal/mcp
      tags: { "*": [sensitive] }

subagents:
  allow_freeform: false        # templates are the only way to spawn
  defaults:
    model: fast
    priority: low
  templates:
    researcher:
      instruction: |
        Research {{params.topic}}. Read sources with the search tools and
        reply with a brief: five bullet points, each with its source URL.
      params:
        topic: {type: string, required: true, description: "What to research"}
      servers: [search]
      tools: ["search", "fetch_page"]
      mode: sync
      limits: {steps: 40, tokens: 200000, deadline: 10m}
      output_contract: "Five bullet points, each ending with a source URL."

store:
  kind: file
  file: {path: ./state}

limits:
  subagents: {breadth: 4, total: 500}
```

The model spawns it with a template and parameters only:

```json
{"template": "researcher", "params": {"topic": "post-quantum TLS adoption"}}
```

A workflow step does the same with `template:` and `params:`. How the pieces
combine:

- **Parameters** are the only holes. Each declared parameter has a `type`
  (`string`, the default, `number`, `integer` or `boolean`), and may be
  `required`, have a `default` or an `enum`. An unknown or ill-typed value
  refuses the spawn. Every `{{params.X}}` in the template must name a
  declared parameter, or the configuration is refused at load.
- **Values are data.** A parameter is folded into the text and never parsed
  for directives. A value that would add a block, an author note or end
  matter to the text refuses the spawn.
- **The template owns the grant.** A call that names a template may not pass
  `servers` or `tools`, and the template's `model` is the child's model. Its
  `limits`, `mode`, `priority`, `output_contract`, `output_schema` and
  `durable` apply where the call sets none of its own, and its `context`
  comes before the call's.
- **The instruction can live elsewhere.** `instruction` takes the same
  sources as `agent.instruction`: `{text: …}`, `{file: …}`, `{url: …}`,
  `{oci: …}` or `{dir: …}`. A bare string that looks like a path or a URI
  names a document rather than being one, so write prose that could be
  mistaken for a path as `{text: …}`.

With only flat templates the validator also prints a warning that
`a2a.listen` is unset. It concerns instance-tier templates, described next,
and you can ignore it here.

A served instruction document cannot define templates; `subagents` is
operator configuration. A document can refer to one, with the `compose`
grant in `agent.document_capabilities`. It declares `::!agent{name=…
template=…}`, and its workflows name it as `@agent/<name>`:

```markdown
You run the research desk. When a question needs sources, open a
researcher for it.

::!agent{name=desk template=researcher}

:::!workflow{name=research}
steps:
  ask:
    kind: a2a
    command: research.ask
    roles: [operator]
  brief:
    kind: subagent
    depends_on: [ask]
    template: "@agent/desk"
    params: {topic: "{{steps.ask.output.args.topic}}"}
  done:
    kind: finish
    depends_on: [brief]
    status: completed
    output: "{{steps.brief.output.result}}"
:::
```

[Write and ship instruction documents](instruction-documents.md) covers the
grant and the document boundary.

> **Note.** A template's text is an instruction document, and its `when`
> and `unless` conditions are decided by the model the child runs: the
> template's `model`, else `subagents.defaults.model`, else the agent's.

## Run a child daemon

A template whose instruction defines machinery, such as a `:::!workflow`, a
`:::!stream` or a `:::!mcp` block, spawns an instance-tier child: a full
agentd daemon with its own workflows, streams and store, supervised by the
parent and wired to it over A2A on a unix socket. Use it for long-lived
work with its own lifecycle, such as one war room per incident. This
configuration opens a room when an operator sends `incident.open`, briefs
it, and mirrors its timeline into the parent. Save it as
`incident-desk.yaml`.

```yaml
agent:
  name: incident-desk

intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  models:
    smart: {model: claude-sonnet-5}
    fast:  {model: claude-haiku-4-5}
  roles:
    turn: smart

a2a:
  listen: unix:///run/agentd/incident-desk.sock

store:
  kind: file
  file: {path: /var/lib/agentd/incident-desk}

streams:
  timeline: {}

subagents:
  templates:
    incident-room:
      instruction: |
        You are the war room for incident {{params.id}}. Keep the timeline
        current and answer the on-call engineer's questions about it.

        :::!stream{name=timeline}
        :::

        :::!workflow{name=on-update}
        steps:
          cmd:
            kind: a2a
            command: incident.update
            roles: [operator]
          log:
            kind: emit
            depends_on: [cmd]
            stream: timeline
            subject: "incident/{{params.id}}"
            data: "{{steps.cmd.output}}"
          done:
            kind: finish
            depends_on: [log]
            status: completed
            output: "{{steps.cmd.output}}"
        :::
      params:
        id: {type: string, required: true}
      model: fast
      budget:
        windows: [{per: day, tokens: 200000}]
        on_exhausted: refuse
      limits: {memory: 1GB, cpu: 30m}
      ttl: 3d
      until: "closed/{{params.id}}"
      mirror_streams: [timeline]

limits:
  subagents:
    instances: {breadth: 4, total: 50, rate: "10/1h"}

workflows:
  - name: open-incident
    steps:
      start:
        kind: a2a
        command: incident.open
        roles: [operator]
        schema:
          type: object
          required: [id, summary]
          properties:
            id: {type: string}
            summary: {type: string}
      open:
        kind: subagent
        depends_on: [start]
        template: incident-room
        params: {id: "{{steps.start.output.args.id}}"}
      brief:
        kind: a2a.send
        depends_on: [open]
        to: "{{steps.open.output.peer}}"
        command: incident.update
        args: {note: "{{steps.start.output.args.summary}}"}
        retry: {max: 5, backoff: 1s}
      done:
        kind: finish
        depends_on: [brief]
        status: completed
        output: "{{steps.open.output.handle}}"
```

A spawn returns `{handle, status: "running", tier: "instance", peer,
socket}`. The handle (`inst-<n>`) is also the child's A2A peer name, so
`a2a.send`, `a2a.delegate` and `subagent.send` reach it by that name. With
`singleton: true` the template name is an alias for its one live child, and
a second spawn is refused while it lives.

> **Note.** The spawn returns when the child process starts, before its
> listener is up, so the first command can fail with "cannot reach peer
> socket". Give it a `retry`, as `brief` does.

The child's prose is its policy, not a task: a template whose machinery
declares no workflow runs nothing until it is sent a message, so give it a
`:::!workflow` with a `once` start for work at spawn, or send it a first
message as `brief` does.

What the instance tier needs and offers:

| Field | What it does | Notes |
|---|---|---|
| `mode` | `detached` (default) returns when the child starts; `sync` waits for a result | `warm` and `async` are refused |
| `result: {workflow: <name>}` | With `mode: sync`, resolves the spawn with the output of the named workflow's first completion in the child | Needs `a2a.listen` on the parent |
| `mirror_streams` | Forwards each event of these child streams into the parent's stream of the same name, with source `instance:<handle>` | The stream must exist on both sides; needs `a2a.listen` |
| `model`, `budget` | The child's turn model and its own token budget | The child gets the parent's intelligence section without its budget or routes |
| `limits` | `memory` and `cpu` only | Token ceilings belong in `budget` |
| `ttl`, `until` | Retire after a duration, or when the named signal is delivered in the child | `until` must use a parameter unless `singleton: true` |
| `durable` | `false` runs the child on a memory store, unmetered and not restarted | Default follows `store.durability.work`; a parent with a `memory` or `none` store runs every child this way |

`servers` and `tools` are refused on an instance template: its own
`:::!mcp` blocks declare its servers. A child's MCP servers are checked
against the parent's service catalogue and egress posture, and the
template's machinery may not write operator-only configuration, define
`a2a`, `intelligence`, `store` or `lifecycle`, declare MCP Events bindings,
or open webhook routes; the parent composes those, and external events reach
the child through the parent. A durable child's token use is charged against
the parent's budget every five seconds and once more when it exits, whether
a workflow step or a model's `subagent.run` spawned it. A `human` step in
the template's workflows addressed `@human/<name>` to one of the template's
`::!human` declarations keeps its channel: the parent writes it into the
child's `agent.gate_channels` ([Human in the loop](human-in-the-loop.md)).

Each child has a directory of its own, mode 0700: its composed
`config.json` (mode 0600), its `log`, its `a2a.sock` and, for a durable
child, its store in `state/`. The directory belongs to the parent that
spawned the child, and no other agent is given it:

- A durable child's directory is kept with the parent's durable state, by
  the parent's `store.prefix` and instance name, the way its store keys that
  state, so the parent finds it again when it restarts. With a file store it
  is `subagents/file/<prefix>/<instance>/<handle>/` under the store's root.
  With an `mcp` or `http` store it is
  `subagents/<kind>-<digest>/<prefix>/<instance>/<handle>/` under the default
  state directory, where the digest stands for the store's address.
  `<prefix>` and `<instance>` are each one percent-encoded path segment: the
  children of an agent named `desk/room` are in `desk%2Froom/`.
- A non-durable child's directory is `<handle>/` in a directory of the
  parent process's own, `agentd-<id>/`. That is under `$XDG_RUNTIME_DIR` when
  it is a directory the user owns with mode 0700, as the XDG specification
  requires, and under the system temporary directory otherwise. A child of a
  parent whose store is `memory` or `none` is always non-durable. The
  child's directory is removed when it exits, unless it failed or crashed:
  then it stays, its log in it, until the parent exits. The parent's
  directory is removed when the parent exits. A parent that does not exit
  normally, because it was killed with SIGKILL or crashed, leaves its
  directory behind, and no later process uses it.

The child's listener binds its socket in a staging directory beside
`a2a.sock`, at a path up to 23 bytes longer than the child's directory, and
a unix socket address holds 107 bytes on Linux (103 on macOS). When that
path would not fit, the socket is in the system temporary directory
instead, under a name derived from the child's directory. A spawn for which neither
fits is refused, naming `TMPDIR`. When a child fails, the parent's
`instance.exited` event carries the `cause` the child logged as it exited.

The parent must be reachable for the child's reports: on a unix listener
the kernel identifies the child, and on a TCP listener with authentication
`a2a.bearer` must be a `{{secret:…}}` reference, or the spawn is refused.

`subagent.retire {handle}`, `ttl` and `until` retire a child: it receives
SIGTERM, drains its runs, checkpoints and exits. The parent kills it only if
it is still running five seconds after the parent's
`lifecycle.drain_timeout`. Its record closes as `retired`, and a durable
child's directory stays on disk.

The parent's own drain stops a child the same way but does not retire it:
the child receives SIGTERM as the drain begins and drains itself, and the
parent waits for it within its own `drain_timeout`, killing it only if it is
still running then. Its record says `parked`, and a durable child starts
again from its composed configuration when the parent restarts, as every
live durable instance child does.

> **Warning.** The parent's load checks a template's tier rules and its
> boundary, but not the steps of its workflows. Those are validated when a
> spawn composes the child's configuration, and an error refuses that spawn
> with `composes an invalid config`. Spawn each instance template once in a
> test environment before you rely on it.

## Avoid these traps

- **A missing template is found at spawn.** A step's `template` that names
  no declared template, and a freeform spawn under `allow_freeform: false`,
  are refused when the step runs, not at load.
- **Unknown server names are dropped.** A misspelled entry in `servers`
  leaves the child without that server and raises no error.
- **`subagent.await` ignores its `timeout`.** Bound the wait with a workflow
  `wait {on: subagent}` or a `subagent` step, which honour theirs.
- **A long `sync` call from a turn is bounded by that turn.** If the turn's
  worker is torn down first, the reply is lost. Prefer `async` with
  `subagent.await`, or a workflow step.
- **The spawn rate is read once.** A reload does not change
  `limits.subagents.rate`.
- **Restarts redo work.** A restored child starts its instruction again, and
  a new spawn after a restart can take a restored record's handle. Make
  delegated work idempotent.
- **Skills do not reach a child.** A spawn's `skills` is accepted and not
  used, and a template's `:::!skill` block is stripped from the child's text.
  Put what the child needs in its instruction or `context`.

## The subagent tools

The `subagent.*` tools are granted to the agent's conversation and to
workflows, never to subagents. A tool call that names a handle acts only for
the principal that spawned it.

| Tool | Arguments | Returns |
|---|---|---|
| `subagent.run` | `instruction` or `template` with `params`; optional `mode`, `servers`, `tools` (not with `template`), `limits` (`steps`, `tokens`, `deadline`, `memory`, `cpu`), `priority` (`low`, `normal`, `high`), `context`, `output_contract`, `output_schema`, `durable` | `{handle, status, result, error}` for `sync`; `{handle, status}` otherwise; instance children add `tier`, `peer`, `socket` |
| `subagent.status` | `handle` | `{handle, status, mode, result, tokens, error}`; safe to poll |
| `subagent.await` | `handle` (`timeout` is accepted and not applied) | `{handle, status, result, error}` once the child is finished |
| `subagent.send` | `handle`, `message` | `{ok, handle}` for a warm child, which queues the message; an instance child receives it over A2A and the reply adds `delivery: "async"` |
| `subagent.kill` | `handle`, optional `reason` | `{ok, handle}`; flat children only |
| `subagent.retire` | `handle` | `{ok, handle, status: "retiring"}`; instance children only |
| `subagent.list` | none | `{subagents: [...]}`: handle, mode, status, the first 80 characters of the instruction, created |

Over A2A, `subagent.send`, `subagent.kill` and `subagent.status` are command
operations, each held to the caller's grants, and an introspection read
returns one subagent without a task; see the
[A2A API reference](a2a-reference.md).

## Watch children run

Each child logs to the daemon's log. The events to watch:

| Event | When |
|---|---|
| `subagent.spawn` | A flat child started: `handle`, `mode`, `pid`, `depth`, `servers` (a count), `priority`, `memory_bytes`, `cpu_seconds` |
| `subagent.turn` | A warm child finished a turn |
| `subagent.result` | A flat child finished: `handle`, `status`, `tokens`, `err` |
| `subagent.kill`, `subagent.respawn` | A kill request; a re-spawn after restart |
| `subagent.kill.escalated` | A killed child had not stopped at its grace and was signalled: `handle`, `signal` (`SIGTERM`, then `SIGKILL`) |
| `child.unhealthy` | Liveness tore a child down: `node`, `health` |
| `intel.swap.sent` | A reload handed running children their model again |
| `instance.spawn`, `instance.retire`, `instance.exited`, `instance.respawn` | An instance child's lifecycle |
| `instance.result`, `instance.mirror`, `instance.metered` | A sync result, a mirrored event, a budget charge |

The `agent_subagents_spawned_total` and `agent_subagents_exited_total`
metrics count every child process, turn and think workers included. Count
subagents from these events or from `subagent.list`. The
[events and metrics reference](telemetry-reference.md) lists every field.

## Next steps

- [Subagents and delegation](subagents.md): the process tree, the trust
  boundary and the lifecycle behind this guide.
- [Author workflows](author-workflows.md): when the work is a pipeline.
- [Connect agents over A2A](connect-agents.md): talk to separate agents
  rather than children.
- [Harden a deployment](harden.md): tags, the trifecta and policies around
  delegation.
