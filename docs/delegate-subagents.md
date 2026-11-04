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
Each turn's output is the child's `result` until the next one, so
`subagent.status` reads the answer to the message before it.

A live instance child takes messages too: `subagent.send` delivers the
message over the child's socket, and the child answers it in a turn of its
own. When it has answered, `subagent.status` carries the answer as `reply`,
and the daemon logs `subagent.turn` with `tier: instance`. The child's
`result` stays its own report, the one a `sync` spawn waits for. A child that
has not answered within 10 minutes leaves its reply as it was, and the log
line says why (`status: failed`, `err`).

The reply is the newest send's: `reply_to` names the send it answers, by
the id that send was answered with — the task of a `subagent.send` command
over A2A, else the `message_id` in the tool's answer — so a client shows a
reply only against its own send. A later send replaces an earlier one's
reply, never the other way round: a slow turn that ends after a newer send
was made does not take its place. One thread per child carries its sends in
order and reads the answer to the newest, looking at the child's task after
500 ms, then at twice the wait each time up to 5 s. At most 32 children's
answers are read at once; a send to another child past that still goes, and
its answer says `followed: false` with a `note`.

In the TUI, open the child on the subagents screen and press `m` or `Enter`:
the message field is in the detail view, and what you sent and what the
child answered stay above it
([Use the terminal and web UI](interface.md#watch-tasks-and-subagents)).

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

`subagents` is operator configuration: a document's `:::config` cannot write
it. Under the `agents` grant a document defines agents of its own instead
([A document's own agents](#a-documents-own-agents)). A document's workflow
refers to a template, the operator's or its own, by name in a `subagent`
step:

```markdown
You run the research desk. When a question needs sources, open a
researcher for it.

:::workflow{id=research dialect=instruction/workflow}
name: research
steps:
  ask:
    kind: a2a
    command: research.ask
    roles: [operator]
  brief:
    kind: subagent
    depends_on: [ask]
    template: researcher
    params: {topic: "{{steps.ask.output.args.topic}}"}
  done:
    kind: finish
    depends_on: [brief]
    status: completed
    output: "{{steps.brief.output.result}}"
:::
```

[Write and ship instruction documents](instruction-documents.md) covers the
document boundary.

## Run a child daemon

A template whose instruction maps onto configuration, such as a
`:::workflow`, a `:::config` or a `:::mcp` block, spawns an instance-tier
child: a full
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

        :::config{id=streams dialect=agentd/config}
        streams: {timeline: {}}
        :::

        :::workflow{id=on-update dialect=instruction/workflow}
        name: on-update
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
`:::workflow` with a `once` start for work at spawn, or send it a first
message as `brief` does.

What the instance tier needs and offers:

| Field | What it does | Notes |
|---|---|---|
| `mode` | `detached` (default) returns when the child starts; `sync` waits for a result | `warm` and `async` are refused |
| `result: {workflow: <name>}` | With `mode: sync`, resolves the spawn with the output of the named workflow's first completion in the child | Needs `a2a.listen` on the parent |
| `mirror_streams` | Forwards each event of these child streams into the parent's stream of the same name, with source `instance:<handle>` | The stream must exist on both sides; needs `a2a.listen` |
| `model`, `budget` | The child's turn model and its own token budget | The child gets the parent's intelligence section without its budget or routes, and the parent's `http.endpoints` whole |
| `limits` | `memory` and `cpu` only | Token ceilings belong in `budget` |
| `ttl`, `until` | Retire after a duration, or when the named signal is delivered in the child | `until` must use a parameter unless `singleton: true` |
| `durable` | `false` runs the child on a memory store, unmetered and not restarted | Default follows `store.durability.work`; a parent with a `memory` or `none` store runs every child this way |

`servers` and `tools` are refused on an instance template: its own
`:::mcp` blocks declare its servers. A child's MCP servers are checked
against the parent's `mcp.servers` and egress posture, and the
template's machinery may not write operator-only configuration, define
`a2a`, `intelligence`, `store` or `lifecycle`, declare MCP Events bindings,
or open webhook routes; the parent composes those, and external events reach
the child through the parent. A durable child's token use is charged against
the parent's budget every five seconds and once more when it exits, whether
a workflow step or a model's `subagent.run` spawned it. A `human` step in
the template's workflows addressed to one of the template's `::person`s is
the operators', as in the agent's own document
([Human in the loop](human-in-the-loop.md)).

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

A parent that idle-exits or is killed does not wait: the child drains on the
SIGTERM the parent's death sends, and may still hold its state when the
parent starts again. The respawn then waits for the previous process to exit
and logs `instance.respawn_wait` with its pid. Calls to the child fail until
it is back, so give them a `retry`. A previous process still running after
the parent's `lifecycle.drain_timeout` plus five seconds is given up: the
record closes `failed` with that pid in its error, and the parent logs
`instance.respawn_failed`.

> **Warning.** The parent's load checks a template's tier rules and its
> boundary, but not the steps of its workflows. Those are validated when a
> spawn composes the child's configuration: the configuration is checked,
> and each of the child's workflows is compiled and its `tool` steps checked
> against the tools the child will have (built in, or served by an MCP
> server the parent has tools from). An error refuses that spawn with
> `composes an invalid config`. A tool step's name is not checked while the
> child composes a server the parent has no tools from; the child then
> checks it when it starts. Spawn each instance template once in a test
> environment before you rely on it.

When a reload changes the templates, the live instance children follow. A
child whose template the reload removed is retired, and the parent logs
`subagent.template.retired`. A `singleton` child whose template's
composition changed (its prose, machinery, skills or fields) is retired,
logged as `subagent.template.recomposed`, and started again from the new
template once it has exited, with the params it was spawned with, logged as
`subagent.template.respawned`. The new child replaces the old one, so the
instance ceilings (breadth, total, rate) do not count it again. Everything
that could still refuse it is checked before the old child is retired:
pressure, params the new template refuses, and the new configuration,
composed and checked as a spawn checks it, its workflows compiled. A refused
recompose leaves the old child running, and the parent logs
`subagent.template.recompose_refused` with the reason (`why`), so a bad
document release does not take a live agent down. A template that is no
longer a singleton, or whose agent became prose-only, retires its child and
does not start another (`subagent.template.retired`). Other children of a
changed template keep the composition they were spawned with until they
retire.

## A document's own agents

An instruction document can define the team of agents agentd runs for it.
An `agent` written as a container, with no `protocol`, `src` or `endpoint`,
is an agent agentd runs itself. Its body is its instruction document: prose,
and its own `:::mcp`, `:::skill`, `:::workflow` and `:::config` blocks.

```markdown
::::agent{id=support title="Support lead"}
You answer every customer who writes to support@.

:::mcp{id=support-desk title="Helpdesk" src="https://desk.example.com/mcp" tools="tickets.list,tickets.reply"}
Every customer conversation. A reply here goes to the customer.
:::

:::workflow{id=support-queue dialect="instruction/workflow"}
name: support-queue
steps:
  start: {kind: once}
  open: {kind: mcp.tool, depends_on: [start], server: support-desk, tool: tickets.list}
  answer: {kind: agent, depends_on: [open], instruction: Answer every open ticket.}
  done: {kind: finish, depends_on: [answer]}
:::
::::
```

The operator grants it, off by default:

```yaml
# needs: ./company.md
agent:
  name: company
  instruction: ./company.md
  document_capabilities: [agents]
limits:
  subagents:
    instances: {breadth: 10, total: 20, rate: "10/1h"}
intelligence:
  endpoints: "https://api.openai.com/v1/chat/completions"
  token: "{{secret:OPENAI_API_KEY}}"
  model: gpt-5.1
```

Each agent becomes the template `subagents.templates.<id>`, its body the
instruction, read under the document's declared parameters, so a `${name}`
in the body reads as it does in the document. An agent whose body carries
machinery is an instance-tier child and a `singleton`; one that is only prose
is a flat worker. The parent logs `subagent.template.composed` for each at
load, with its tier, servers, workflows and skills, before any is spawned.

- **The blocks are the agent's.** Nothing inside an agent's body configures
  the parent, whether the agent is admitted or held.
- **The child has no grants.** Its body is compiled with none, so an A2A
  agent or an agent written in it stays text. It is held to the boundary
  every template is held to: no operator configuration, none of the sections
  the parent composes, no webhook route.
- **The operator's ceilings hold.** `limits.subagents.instances` (breadth 2,
  total 8 and `4/1h` by default) and `subagents.defaults` bound the team; the
  load warns when the document defines more instance agents than they allow.
  A child's servers are judged against the parent's `mcp.servers` and egress
  as any template's are. A child gets no A2A peer, and no credential but the
  ones the parent's own servers and model carry.
- **The operator's names stand.** An operator template of the same name is
  used, and the document's agent is held as text with a `host-policy`
  warning; the rest of the document applies. So is an agent named as one
  of the operator's `a2a.peers` or as the operator's `agent.name`.
- **Over the wire** (`url:`, `oci:`, `mcp:`), the agents run only when the
  document verified against a pinned publisher (`agent.instruction.trust`)
  and the author's signature attests `agents`. A child of such a document
  names no secret: one that does refuses the document.
- **Skills travel.** The agent's `:::skill` blocks reach its child as skills
  of its own.
- **Names resolve.** A reference in an agent's text to a person or another
  agent the document declares (`:person[Andrii]{ref=#andrii}`) reads as it
  does in the document, not `[unresolved]`. It is a name only: the child's
  `human` steps cannot address that person.

### Reporting lines

An agent written inside another reports to it. Write the outer fence
longer:

```markdown
:::::agent{id=cmo title="Chief marketing officer"}
You make the company known.

::::agent{id=community title="Community lead"}
You lead the forum.
::::
:::::
```

`community` is its own template, composed exactly as a top-level agent and
under the same grant, and labelled `reports_to: cmo`. Nesting goes to any
depth; `reports_to` is the agent it is written in. Each agent's text is its
own: the report's whole block is cut out of the manager's instruction, so
adding or removing a report leaves the manager's composition as it was. The
line is a label, nothing more: the manager gets no tools for its report and
no peer to it. It is on `subagent.template.composed`, `instance.spawn`, the
child's record, and the TUI's subagents screen, which lists each child's
template and who it reports to. A new document version that moves an agent
to another manager changes no one's composition, so nothing restarts: the
live child's record takes its new `reports_to`, the TUI sees it in the
`subagent` feed item, and the parent logs
`subagent.template.reports_to_changed` (`template`, `handle`, `from`, `to`).
A report whose manager is held, by an
operator template of its name say, keeps its label, and the load says so.

The operator can draw the same lines in the configuration, for its own
templates and its A2A peers, with `reports_to` and `title`
([Draw the org chart](#draw-the-org-chart)). An operator template that
holds a document agent of its name carries the operator's line and title,
not the document's; a document agent the operator does not name keeps the
one its nesting gives it. A document cannot set either field: `subagents`
is the operator's, and a `:::config` that writes `a2a.peers[].reports_to`
or `.title` is refused.

### Start the team with the document

By default a document agent starts when a workflow or the model spawns it.
The operator can have agentd start them instead:

```yaml
# fragment: under subagents
document_agents: {start: on_compose}
```

With `start: on_compose`, agentd starts each instance-tier document agent's
singleton at load, once the listeners are up, and each one a reload adds,
with no workflow. A reload that turns `on_compose` on starts each one whose
singleton is not live, the agents already composed included. A changed agent is respawned and a removed one retired, as
any reload does. Every start obeys `limits.subagents.instances`: one refused
is logged as `subagent.template.start_refused`, and the others still start.
A workflow that spawns an agent already live gets the live one back
(`already_live: true`, logged as `subagent.template.already_live`) rather
than an error. Prose-only agents are workers a step gives a task, and are
not started.

The document's own workflows start the team. A `subagent` step names an
agent by its id. An instance agent's spawn is detached, so a `parallel` of
ten spawns returns at once; once its singleton is live, `a2a.send` and
`a2a.delegate` reach it by the same name. A prose-only agent is a flat
worker, spawned in `subagents.defaults.mode` (`sync` unless set). Each child
runs its own `:::workflow` blocks, a `once` start included, in its own
daemon.

```markdown
:::workflow{id=open-the-company dialect=instruction/workflow}
name: open-the-company
steps:
  start: {kind: once}
  team:
    kind: parallel
    depends_on: [start]
    branches:
      support: {steps: {run: {kind: subagent, template: support}}}
      billing: {steps: {run: {kind: subagent, template: billing}}}
  brief:
    kind: a2a.delegate
    depends_on: [team]
    peer: support
    command: support.brief
    retry: {max: 6, backoff: 1s}
:::
```

When the document changes, a reload recomposes the team by the rule above:
a new agent can be spawned, a removed agent's live child is retired, and a
changed agent's live child is retired and started again from its new body.
The document's workflow that spawned the team does not run again unless its
own definition changed.

## Draw the org chart

Who reports to whom is a label the operator writes on each template and
each A2A peer: `reports_to` names the manager, and `title` says what the
role is. This agent's own title is `agent.title`; it sits at the top, so it
has no `reports_to`. A reporting line grants nothing. A manager gets no tool for its
reports, no peer route to them and no authority over them, and a report
gets nothing from its manager; what each can do is what it could do
without the line. agentd checks the lines, carries them, and shows them.

A small company: this agent is the CEO; a CTO and a CMO report to it, a
writer reports to the CMO, and outside counsel, a remote A2A agent,
reports to the CEO. Save it as `company.yaml`.

```yaml
agent:
  name: ceo
  title: Chief executive
  instruction: |
    You run a small company. Hand engineering questions to the cto
    template and marketing to the cmo template.

intelligence:
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  model: claude-sonnet-5

subagents:
  templates:
    cto:
      title: Chief technology officer
      reports_to: ceo
      instruction: You answer engineering questions for the CEO.
    cmo:
      title: Chief marketing officer
      reports_to: ceo
      instruction: You plan how the company is made known.
    writer:
      title: Staff writer
      reports_to: cmo
      instruction: |
        Write {{params.piece}} in the company's voice.
      params:
        piece: {type: string, required: true}

a2a:
  peers:
    - name: legal
      endpoint: https://legal.example.com
      title: Outside counsel
      reports_to: ceo
```

A `reports_to` names one of three things:

- **another template**, by its `subagents.templates` name, a document
  agent's included;
- **an A2A peer**, by its `a2a.peers` name;
- **this agent**, by its `agent.name`.

The load refuses, naming the field:

- a name that is none of them (with no `agent.name` set, this agent has no
  name to be named by, and the error says so);
- a template or a peer that reports to itself;
- a name that two of them share in the operator's configuration, a
  template and a peer called `cmo` say, which a line cannot tell apart:
  rename one;
- a cycle, with its path: `reporting lines form a cycle: cto (template) →
  legal (peer) → cto (template)`. A document agent's derived line counts
  here too.

A document agent the operator does not name keeps the line its nesting
gives it ([Reporting lines](#reporting-lines)); an operator template of the
same name carries the operator's.

A document cannot make the operator's lines ambiguous. The operator's names
stand across kinds: a document's A2A agent named as an operator template or
as this agent's `agent.name`, a document agent named as an operator peer,
and a document's `agent.name` that names an operator template or peer are
each held, with a `host-policy` warning, and a line naming that name reaches
the operator's entry.

### Read the chart

The status document, as the operator reads it, has the whole chart in one
list, `org`: this agent first (`kind: self`), then every template, then
every peer, each with its `title` and `reports_to` (null at the top). A
client draws the hierarchy from it without joining other lists. For the
company above:

```json
{
  "org": [
    {"name": "ceo", "kind": "self", "title": "Chief executive", "reports_to": null},
    {"name": "cmo", "kind": "template", "title": "Chief marketing officer", "reports_to": "ceo"},
    {"name": "cto", "kind": "template", "title": "Chief technology officer", "reports_to": "ceo"},
    {"name": "writer", "kind": "template", "title": "Staff writer", "reports_to": "cmo"},
    {"name": "legal", "kind": "peer", "title": "Outside counsel", "reports_to": "ceo"}
  ]
}
```

`org` is in the operator's view only; a caller who is not the operator
reads the instance's facts and its own work, as before. The Agent Card does
not carry the chart: who reports to whom inside the company is not
published to strangers.

The line also travels with each child. A template's `reports_to` is on its
`instance.spawn`, on the child's record, in `subagent.list` and
`subagent.get`, in the A2A feed's `subagent` items, and in the TUI's
subagents screen, which shows a `reports to` column. At load agentd logs
the chart once as `agent.org` when anyone in it has a line or a title:
this agent with an `agent.title`, a template or a peer.

A workflow reads the chart with a `status` tool step. No new node is
needed. A run started by the operator, or by a `once` or `schedule` start,
reads the operator's view; one acting for another caller reads that
caller's, which has no `org`:

```yaml
# fragment: under workflows
- name: org-chart
  steps:
    ask: {kind: a2a, command: org.chart, roles: [operator]}
    read: {kind: tool, depends_on: [ask], name: status, args: {}}
    done:
      kind: finish
      depends_on: [read]
      status: completed
      output: "{{steps.read.output.org}}"
```

### Move a line

Both fields reload. A reload that changes a template's `reports_to` or
`title` changes no one's composition, so nothing restarts: every live child
of that template, an instance child or a flat worker, keeps running, its record takes the new `reports_to`, the
feed's `subagent` item carries it, and the parent logs
`subagent.template.reports_to_changed` (`template`, `handle`, `from`,
`to`). A reload that moves a peer's line logs `a2a.peer.reports_to_changed`
(`peer`, `from`, `to`) and lists `a2a.peers` among what changed. Either way
the status document's `org` is the new chart at once, and `agent.org` is
logged again. `agent.title` reloads the same way: a reload that changes it
lists `agent.title` among what changed, and `org`'s `self` entry carries
the new title.

A served document's `:::config` cannot set `agent.title`, as it cannot set
a peer's `reports_to` or `title`: where this agent stands in its
organisation is the operator's to say, and the load refuses the document,
naming the field.

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
- **Skills do not reach a flat child.** A spawn's `skills` is accepted and
  not used, and a flat template's `:::skill` block is stripped from the
  worker's text. Put what the worker needs in its instruction or `context`.
  An instance template's `:::skill` blocks reach the child as its own
  skills, composed as its `skills.inline` under the ids the template gave them.
- **A changed `once` workflow runs again.** A reload starts the `once` start
  of a definition it installed new or replaced. Under `policy: always`, or
  once `store.retention.runs` has evicted the earlier run, a team-spawning
  workflow whose
  definition changed spawns again, and a live singleton refuses the spawn.
  `workflow.resume` arms only the workflow it resumed, and
  `workflow.create` and `workflow.update` only the one they define; none of
  them fires a `once` start that already fired for the same definition in
  this process's life.

## The subagent tools

The `subagent.*` tools are granted to the agent's conversation and to
workflows, never to subagents. A tool call that names a handle acts only for
the principal that spawned it.

| Tool | Arguments | Returns |
|---|---|---|
| `subagent.run` | `instruction` or `template` with `params`; optional `mode`, `servers`, `tools` (not with `template`), `limits` (`steps`, `tokens`, `deadline`, `memory`, `cpu`), `priority` (`low`, `normal`, `high`), `context`, `output_contract`, `output_schema`, `durable` | `{handle, status, result, error}` for `sync`; `{handle, status}` otherwise; instance children add `tier`, `peer`, `socket` |
| `subagent.status` | `handle` | `{handle, status, mode, result, tokens, error}`, and an instance child's answer to its newest answered `subagent.send` as `reply`, with `reply_to` naming that send; safe to poll |
| `subagent.await` | `handle` (`timeout` is accepted and not applied) | `{handle, status, result, error}` once the child is finished |
| `subagent.send` | `handle`, `message` | `{ok, handle}` for a warm child, which queues the message; an instance child receives it over A2A and the reply adds `delivery: "async"` and the `message_id` it went as (`followed: false` and a `note` when its answer is not read back); a warm child's answer is its `result`, an instance child's its `reply` |
| `subagent.log` | `handle`; optional `lines` (default 200, at most 1000), `after` | `{handle, source, lines, cursor}`, and `skipped` or `note`; read-only, the operator's alone ([Read a child's log](#read-a-childs-log)) |
| `subagent.kill` | `handle`, optional `reason` | `{ok, handle}`; flat children only |
| `subagent.retire` | `handle` | `{ok, handle, status: "retiring"}`; instance children only |
| `subagent.list` | none | `{subagents: [...]}`: handle, mode, status, the first 80 characters of the instruction, created, and for a templated child its template, tier and `reports_to` (the template's reporting line, the operator's or its document's) |

Over A2A, `subagent.send`, `subagent.kill` and `subagent.status` are command
operations, each held to the caller's grants, and an introspection read
returns one subagent without a task. `subagent.log` is one too, the
operator's alone; see the [A2A API reference](a2a-reference.md).

## Watch children run

The daemon's log says what happens to each child. A flat child's lines are
in it. An instance child is a daemon of its own and writes its own log,
`log` in its directory: its runs and steps are there, not in the parent's.
What reaches the parent from an instance child is its record (status and
spend, which the A2A feed and the TUI's subagents screen show), the result
of a `mode: sync` spawn, and the streams it mirrors (`mirror_streams`). The
events to watch:

| Event | When |
|---|---|
| `subagent.spawn` | A flat child started: `handle`, `mode`, `pid`, `depth`, `servers` (a count), `priority`, `memory_bytes`, `cpu_seconds` |
| `subagent.turn` | A warm child finished a turn, or an instance child answered a message (`tier: instance`; `status: failed` and `err` when it did not) |
| `subagent.result` | A flat child finished: `handle`, `status`, `tokens`, `err` |
| `subagent.kill`, `subagent.respawn` | A kill request; a re-spawn after restart |
| `subagent.kill.escalated` | A killed child had not stopped at its grace and was signalled: `handle`, `signal` (`SIGTERM`, then `SIGKILL`) |
| `child.unhealthy` | Liveness tore a child down: `node`, `health` |
| `intel.swap.sent` | A reload handed running children their model again |
| `instance.spawn`, `instance.retire`, `instance.exited`, `instance.respawn`, `instance.respawn_wait`, `instance.respawn_failed` | An instance child's lifecycle; `instance.spawn` names its `template`, `reports_to`, and its absolute `dir` and `log` |
| `instance.result`, `instance.mirror`, `instance.metered` | A sync result, a mirrored event, a budget charge |
| `subagent.template.composed` | A template the instruction document defined, at load or when a reload added or changed it: `template`, `title`, `reports_to`, `origin`, `line`, `tier`, `singleton`, the counts `servers`, `workflows`, `skills`, and their names in `server_names`, `workflow_names`, `skill_names` |
| `subagent.template.started`, `subagent.template.start_refused`, `subagent.template.already_live` | Under `start: on_compose`, a document agent started, or refused by a ceiling (`error`); a spawn that found its singleton live and got it back |
| `subagent.template.retired`, `subagent.template.recomposed`, `subagent.template.respawned`, `subagent.template.respawn_failed` | A reload removed or changed a live child's template: `template`, `handle`, `why`; a respawn names the child it replaces (`was`) |
| `subagent.template.reports_to_changed` | A reload moved a live child's template to another manager, in the document or in the operator's `reports_to`: `template`, `handle`, `from`, `to`; the child keeps running |
| `a2a.peer.reports_to_changed` | A reload moved a peer's `reports_to`: `peer`, `from`, `to` |
| `agent.org` | The org chart, `org`, as the status document has it: at load and after a reload that moved it, when anyone in it (this agent, a template or a peer) has a line or a title |

### Read a child's log

`subagent.log {handle}` answers the newest lines of one child's log and a
cursor: pass the cursor back as `after` and the next read has only what was
written since. Each line is one of agentd's own JSON log lines, as written.
Where the lines come from is the child's record, never the caller:

- **An instance child**'s are its own log file, `log` in its directory
  (`source: file`). At most its last 256 KiB are read, and a line still being
  written waits for the next read. Only whole lines are returned: a line
  longer than the whole 256 KiB is passed over, and in its place is one
  `{"truncated": true, "bytes": N}` line; the next read begins after its
  end. The child owns its directory, so the parent reads only the file it
  opened for the child's log at spawn: a symlink, a FIFO or another file put
  at that path is refused, `the child's log is not the file agentd opened
  for it`, and nothing of it is read. The cause a failed child's
  `instance.exited` line names is read by the same rule.
- **A flat worker** writes into this daemon's log. Its lines here are this
  daemon's lines about it — those naming its `handle`, or written in its
  `agent_path` — from the in-memory log ring (`source: ring`), which the
  daemon keeps while `a2a.introspection.enabled` is on. Without the ring the
  answer has no lines and a `note` saying so; the lines are in the daemon's
  stderr all the same.

It is the operator's alone, over A2A and as a tool: a turn or a run acting
for any other principal is refused. A non-durable instance child's directory
goes when it exits cleanly, and its log with it; a failed child's stays until
the parent exits.

In the TUI, `l` on a child's detail view opens the log pane, which follows
new lines while it is open. A workflow reads it with a tool step — here, the
last lines of a reviewer that did not finish, kept with the run's output:

```yaml
# fragment: under workflows:
- name: review
  steps:
    start: {kind: manual}
    review:
      kind: subagent
      depends_on: [start]
      mode: async
      instruction: "Review the open pull request for correctness regressions."
    done:
      kind: wait
      depends_on: [review]
      on: subagent
      subagent: "{{steps.review.output.handle}}"
      timeout: 30m
    tail:
      kind: tool
      depends_on: [done]
      when: "steps.done.output.status != 'completed'"
      name: subagent.log
      args: {handle: "{{steps.review.output.handle}}", lines: 50}
    report:
      kind: finish
      depends_on: [done, tail]
      output: {status: "{{steps.done.output.status}}", last_lines: "{{steps.tail.output.lines | null}}"}
```

What a child logged can carry what it was handed and what it made of it
(with `observability.log_content: true`, its tool arguments and results), so
a read of its log is a read-back of the child, held to the same rules as
`subagent.status` ([Security model](security.md)).

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
