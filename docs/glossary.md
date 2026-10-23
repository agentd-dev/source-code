# Glossary

This page defines the terms the agentd documentation uses, one or two
sentences each, with a link to the page that explains each one. It is for any
reader who meets a word another page assumes. Configuration keys are in the
[Configuration reference](configuration.md).

## A

| Term | Meaning |
|---|---|
| **A2A** | The Agent2Agent protocol, version 1.0. It is how something reaches *in* to agentd: a peer agent, a parent delegating work, the TUI or web UI. See [The A2A channel](a2a.md). |
| **A2A listener** | The HTTPS (or loopback HTTP, or unix socket) endpoint `a2a.listen` opens. Without one, nothing outside the process can talk to the agent. See [The A2A channel](a2a.md). |
| **agent** | One agentd process: a supervisor, the instruction it follows, the tools its MCP servers bring, and the models it reaches. Its identity is `agent.name`. See [Architecture](architecture.md). |
| **Agent Card** | The public JSON document an A2A listener serves at its origin, describing the agent and how to call it. Anyone may read it without signing in. See [The A2A channel](a2a.md). |
| **approval** | How a gate is answered: by a person (`ask`), by a judge model on the `approve` role (`auto`), or by taking the ask's recommendation (`accept`). See [Human in the loop](human-in-the-loop.md). |
| **artifact** | A named piece of content agentd stores for a run or the agent, such as a step output too large to keep inline. The `artifact.*` tools manage them. See [Workflows: how runs work](workflows.md). |
| **ask** | A request for a person's answer: the `ask_human` tool, a workflow `human` step, or a `security.policies` rule with `action: ask`. An ask on a task turns it `input-required`. See [Human in the loop](human-in-the-loop.md). |

## B

| Term | Meaning |
|---|---|
| **breaker** | A per-endpoint circuit breaker. After repeated failures it opens, calls skip that endpoint, and it half-opens after a cooldown. See [Models, providers and roles](intelligence.md). |
| **budget** | A ceiling on tokens or model calls: per run (`limits.run`), per window or lifetime (`intelligence.budget`), per conversation, or per principal. The supervisor enforces it; the model never sees it as a suggestion. See [Control spend with budgets](budgets.md). |
| **build feature** | A cargo feature that compiles a capability in or out, such as `a2a` or `cel`. Release binaries carry all but `exec` and `internal-mocks`. See [Build features and crates](features.md). |

## C

| Term | Meaning |
|---|---|
| **checkpoint** | The durable record of a run's state, written to the store before each step's side effect. A restart resumes from it. See [Workflows: how runs work](workflows.md). |
| **compaction** | Summarising the oldest part of a conversation when it outgrows the model's window, on the `compaction` role. See [How a turn runs](agent-loop.md). |
| **company** | A business agentd acts at for one person under the Personal Agent Protocol draft, declared as a `services` entry of `kind: company` (the `companies` build feature, a preview). See [Act at companies for a person](act-at-companies.md). |
| **confidence** | The least sureness a decision is acted on with. For a `noul` it is the distance of the probability from 0.5, doubled. An answer under it is *unsure*. See [Models, providers and roles](intelligence.md). |
| **conversation** | A thread of messages with the agent's root, identified by an A2A `contextId` and owned by the caller who started it. Each message in it starts one turn. See [How a turn runs](agent-loop.md). |
| **correlate** | A start node that fires a run when a set of related stream events has arrived within a window: a join over events. See [React to changes and events](react-to-events.md). |

## D

| Term | Meaning |
|---|---|
| **daemon** | A long-lived instance that exits only when told to: it has an A2A listener or a long-lived start node, or `lifecycle.run_until: drained`. A listener, a goal or a long-lived start gets it the file store by default; `drained` alone does not change the store. Its instruction is the policy for the messages it takes: it runs nothing at startup unless it has `agent.prompt` or a `once` start. See [Jobs, daemons and triggers](modes-and-triggers.md). |
| **decide call** | A call that asks typed questions about a state — a `choice`, a `noul` or a `score` — instead of asking for text. See [Models, providers and roles](intelligence.md). |
| **decision guard** | A step's `when:` written as a statement for a model to decide (`{noul: "…"}`) rather than as CEL. The step runs only when the statement holds and the answer is sure. See [Workflows: how runs work](workflows.md). |
| **decision model** | A model of kind `decision`: it answers decide calls natively, with probabilities, and writes no text. It is reached on a provider with `dialect: decisions`. See [Models, providers and roles](intelligence.md). |
| **device grant** | The OAuth 2.0 device authorization grant on the A2A listener: a client shows a code, an operator approves it, and the client gets a session. See [The A2A channel](a2a.md). |
| **dialect** | The wire format a connection speaks: `openai`, `anthropic`, `bedrock` or `decisions`. See [Intelligence wire reference](intelligence-reference.md). |
| **directive** | A fenced block in an instruction document, such as `:::!workflow`, that declares machinery or shapes what the model reads. See [Instruction documents](directives.md). |
| **discovery chain** | The config files agentd loads when none is named: the user's, the project's (`agentd.yml`) and the machine-local one (`agentd.local.yml`). See [How configuration works](configuration-model.md). |
| **drain** | A graceful shutdown: admit no new work, let live work finish within `lifecycle.drain_timeout`, leave what still runs then for the next start, checkpoint, exit — 0 for a daemon, the work's code for a job. See [Operate a running daemon](operations.md). |

## E

| Term | Meaning |
|---|---|
| **effort** | How hard a reasoning model thinks: `low`, `medium` or `high`. A model sets a default; a node or `think` call may override it. See [Models, providers and roles](intelligence.md). |
| **egress** | One of the three capability tags: the tool can move data out or change external state. Also `security.egress`, which with `closed` refuses any destination the service catalog does not list. See [Security model](security.md). |
| **emit** | A workflow step that appends an event to a declared stream. See [Workflows: how runs work](workflows.md). |
| **escalate** | Asking an unsure decision once more on the model's `escalate` model and taking that answer as final. See [Models, providers and roles](intelligence.md). |
| **extension** | An A2A protocol extension agentd declares on its card, such as the events feed or the command extension. See [A2A extensions](a2a-extensions.md). |

## F – H

| Term | Meaning |
|---|---|
| **fallback** | The model a call moves to when its own model is unavailable — every endpoint failed or its breakers are open — walking down a chain across providers. See [Models, providers and roles](intelligence.md). |
| **gate** | A point where work waits for an answer: an ask turned into an `input-required` task the owner, or a named operator, answers. See [Human in the loop](human-in-the-loop.md). |
| **generate call** | A call that asks a model for text, tool calls or JSON. A chat or reasoning model serves it. See [Models, providers and roles](intelligence.md). |
| **goal** | An optional watchdog (`goal:`) that checks on a cadence whether a stated goal is achieved or stuck, and acts. It runs on the `goal` role. See [Configuration reference](configuration.md#goal). |
| **grant** | What a principal, a subagent or a workflow tool may call. A subagent's grant can only narrow its parent's. See [Security model](security.md). |
| **harness** | The supervisor seen as the agent's containment: the part that enforces budgets, deadlines and the process tree, and never reads model output as instruction. See [Supervision and containment](harness.md). |

## I – J

| Term | Meaning |
|---|---|
| **implicit operator** | On a loopback listener with no credential configured, a local caller that presents nothing is treated as the operator. Any credential mechanism, or a request from a browser, ends it. See [The A2A channel](a2a.md). |
| **inbox** | The durable queue of events waiting to become work: messages that start turns, firings that start runs. An event is written to the store before it is queued. See [How a turn runs](agent-loop.md). |
| **instance** | One running agentd process and the durable state it owns, identified by `agent.name`. See [Architecture](architecture.md). |
| **instance-tier child** | A subagent that is a full agentd daemon of its own, because its template's instruction declares machinery. It talks to its parent over A2A on a unix socket. See [Subagents and delegation](subagents.md). |
| **instruction** | The agent's standing policy, `agent.instruction`: text, a file, a folder, an HTTPS document, an OCI artifact or an MCP resource. See [Write and ship instruction documents](instruction-documents.md). |
| **instruction document** | A Markdown document written to the Instruction Specification: prose the model reads, plus directives that can declare workflows, servers and more. agentd is its reference implementation. See [Instruction documents](directives.md). |
| **intelligence** | agentd's name for model access as a whole: connections, models, roles and budgets, under `intelligence:`. See [Models, providers and roles](intelligence.md). |
| **job** | A one-shot instance: it does its work and exits once nothing is left. A `--prompt` or bare `--instruction` run is a job. See [Jobs, daemons and triggers](modes-and-triggers.md). |

## K – M

| Term | Meaning |
|---|---|
| **kill ladder** | The supervisor's bounded teardown of a child: cancel, `SIGTERM` to its process group, `SIGKILL`, reap. See [Supervision and containment](harness.md). |
| **lethal trifecta** | One agent holding all three capability tags — `untrusted_input`, `sensitive` and `egress` — at once. agentd refuses it at load unless `security.allow_trifecta` is set. See [Security model](security.md). |
| **machinery** | What a directive declares that becomes configuration — a workflow, an MCP server, a stream — as opposed to prose the model reads. It folds in at load. See [Instruction documents](directives.md). |
| **MCP** | The Model Context Protocol. agentd is an MCP client: every task tool and resource comes from an MCP server you declare. See [MCP: tools and context](mcp.md). |
| **MCP Events** | A draft MCP extension (SEP-3415) through which a server's event occurrences reach declared streams, polled or delivered by webhook. See [React to changes and events](react-to-events.md). |
| **MCP server** | A remote service, reached over Streamable HTTP, that offers tools, resources and prompts. agentd dials it and spawns no local process. See [Connect MCP servers](connect-mcp.md). |
| **mock endpoint** | The built-in mock model, `mock:final` or `mock:file:<playbook.json>`: scripted answers for trying an agent or a workflow without a provider. Release binaries and the image carry it (the `mock-models` feature). See [Build features and crates](features.md). |
| **model** | A named wire model on a connection (`intelligence.models.<name>`), of kind `chat`, `reasoning` or `decision`. See [Models, providers and roles](intelligence.md). |

## N – O

| Term | Meaning |
|---|---|
| **node** | One step kind in a workflow, such as `agent`, `http`, `switch` or `schedule`. `agentd --workflow-schema` prints the catalogue a build carries. See [Workflow node reference](node-registry.md). |
| **noul** | A decision question that asks for the probability that a statement is true. It holds at 0.5 or above. See [Models, providers and roles](intelligence.md). |
| **operator** | Whoever deploys and controls the agent, and the A2A role with the most authority. Operator-only settings are the ones an instruction document may not write. See [Security model](security.md). |

## P

| Term | Meaning |
|---|---|
| **peer** | Another A2A agent this one dials, declared under `a2a.peers`. See [Connect agents over A2A](connect-agents.md). |
| **Personal Agent Protocol** | A draft protocol under which a company publishes the MCP servers, agent and APIs a person's agent may use, and the agent acts there for that person, the User, who signs in and approves actions themselves. agentd supports draft 0.1, pinned, as a preview. See [Act at companies for a person](act-at-companies.md). |
| **policy** | A `security.policies` rule: a verdict — allow, deny, ask or shadow — on a tool call, matched by tool, tags, caller, principal or arguments. See [Security model](security.md). |
| **preflight** | Triage of an inbound message before its turn, on the `preflight` role: its intent, risk, whether it needs a plan, and which route to take. See [How a turn runs](agent-loop.md). |
| **principal** | Who a request is from: an id, a role (`operator`, `user`, `agent` or `anonymous`) and grants, resolved from its credential by `a2a.principals`. Work is owned by and charged to its principal. See [The A2A channel](a2a.md). |
| **provider** | A named model connection under `intelligence.providers`, with its own endpoints, dialect, credential, failover and breakers. See [Models, providers and roles](intelligence.md). |

## R

| Term | Meaning |
|---|---|
| **reasoning model** | A model of kind `reasoning`: it generates like a chat model after thinking, at an effort. See [Models, providers and roles](intelligence.md). |
| **reload** | Applying a changed configuration to a running daemon on `SIGHUP` or a watched file change: validated, staged, and applied whole or refused. See [How configuration works](configuration-model.md). |
| **resource** | A piece of context an MCP server serves under a URI, which agentd can read and subscribe to. An instruction may be one. See [MCP: tools and context](mcp.md). |
| **role** | A job the agent does — `turn`, `think`, `decide`, `preflight`, `compaction`, `goal`, `approve` — and the model it runs on. Every role defaults to `turn`. See [Models, providers and roles](intelligence.md). |
| **round** | One iteration inside a turn: a model call and the tool calls it asks for. `limits.run.steps` bounds how many rounds a turn may take. See [How a turn runs](agent-loop.md). |
| **route** | A model a turn may run on instead of the `turn` role, chosen per message by preflight from `intelligence.routes`. See [Models, providers and roles](intelligence.md). |
| **Rule of Two** | agentd's structural rule: any two capability tags may sit together in one agent, never all three. See [Security model](security.md). |
| **run** | One execution of a workflow, from its start node to `finish`, with its state checkpointed as it goes. See [Workflows: how runs work](workflows.md). |

## S

| Term | Meaning |
|---|---|
| **sensitive** | One of the three capability tags: the tool reaches private data or privileged systems. See [Security model](security.md). |
| **service catalog** | `services:`, the named external services a deployment may use, with authoritative tags and a tool ceiling. Under `security.egress: closed` it is the allow-list. See [Harden a deployment](harden.md). |
| **signal** | A named message delivered to waiting runs or `signal` start nodes, for coordination (`workflow.signal`). Also an operating-system signal such as `SIGTERM`. See [Jobs, daemons and triggers](modes-and-triggers.md). |
| **skill** | Guidance the model loads on demand, in the Agent Skills format (a folder led by a `SKILL.md`): from an MCP server's Skills extension, a `skills/` or `.agents/skills/` folder, or a `:::!skill` directive. A skill grants no tool. See [Agent Skills](interoperability.md#agent-skills). |
| **start node** | The step that fires a workflow's runs: `once`, `manual`, `loop`, `schedule`, `subscribe`, `signal`, `event`, `stream`, `correlate`, `a2a` or `webhook`. Also called a trigger. See [Jobs, daemons and triggers](modes-and-triggers.md). |
| **step** | One node instance in a run. A step's output is recorded before the run moves on. See [Workflows: how runs work](workflows.md). |
| **store** | Where durable state lives: the local filesystem, an MCP server, an HTTP key-value service, memory, or nowhere. See [Architecture](architecture.md). |
| **stream** | A declared, durable sequence of events (`streams:`), appended by `emit`, webhooks, A2A and MCP Events, and consumed by `stream` and `correlate` starts. See [React to changes and events](react-to-events.md). |
| **structured generation** | Asking a provider to hold a model's answer to a JSON Schema, natively where the dialect allows. The answer is validated against the schema either way. See [Models, providers and roles](intelligence.md). |
| **subagent** | A child process the agent spawns to do delegated work in its own context and budget, returning a distilled result. See [Subagents and delegation](subagents.md). |
| **subscription** | A standing request to an MCP server to be told when a resource changes, or, with MCP Events, when an event occurs. See [MCP: tools and context](mcp.md). |
| **supervisor** | The single-threaded reactor process that holds all durable state and credentials, builds every turn and runs no agent loop: turns and subagents run in child processes. Its only model calls are the goal check and the approval judge, which carry no tools. See [Supervision and containment](harness.md). |

## T

| Term | Meaning |
|---|---|
| **tags** | The capability labels an operator puts on an MCP server: `untrusted_input`, `sensitive`, `egress`, and `trusted`, which is no leg and marks a server whose results are not outside text. The trifecta check and the taint engine read them; nothing the model or a server says does. See [Security model](security.md). |
| **taint** | The `untrusted_input` a stream carries because of what feeds it, such as a webhook body. A workflow that reads a tainted stream into steps holding `sensitive` and `egress` is refused at load. At runtime, work is tainted once outside text reached it: an `untrusted_input` or untagged server's result, a webhook's body, a peer's answer. See [Security model](security.md). |
| **task** | The A2A unit of work a message creates: it has a state (`working`, `input-required`, `completed`, …) and is visible to its owner and to operators. See [A2A API reference](a2a-reference.md). |
| **template** | An operator-declared subagent definition, `subagents.templates.<name>`, with the only parameters the model may fill. See [Subagents and delegation](subagents.md). |
| **tool** | A callable the model may use: an MCP server's tool, one of agentd's internal tools (`memory.*`, `workflow.*`, …), a code-registered tool, or a workflow registered as a tool. See [MCP: tools and context](mcp.md). |
| **trigger** | What wakes the agent to start work; in a workflow, its start node. See [Jobs, daemons and triggers](modes-and-triggers.md). |
| **trust ladder** | The families of instruction-document blocks an operator grants with `agent.document_capabilities`; a document may use only what is granted. See [Instruction documents](directives.md). |
| **turn** | agentd's unit of conversational work: one message in, one reply out, with as many rounds of model and tool calls between as the work needs. It runs in a short-lived child process. See [How a turn runs](agent-loop.md). |
| **turn worker** | The short-lived child process that runs one turn's rounds against an immutable snapshot the supervisor built. See [How a turn runs](agent-loop.md). |

## U – W

| Term | Meaning |
|---|---|
| **untrusted_input** | One of the three capability tags: the tool returns content from a source nobody controls, such as web pages or inbound mail. An untagged server counts as one, and its results taint the work that reads them. See [Security model](security.md). |
| **warm subagent** | A subagent in `warm` mode: it stays alive and takes one turn per message (`subagent.send`). See [Subagents and delegation](subagents.md). |
| **webhook** | An inbound HTTP request on `webhooks.listen` that fires a `webhook` start node, answers a `wait`, or appends to a stream. See [React to changes and events](react-to-events.md). |
| **workflow** | A named, validated graph of steps whose execution state is checkpointed before each side effect, so a run survives a crash. See [Workflows: how runs work](workflows.md). |
| **workflow tool** | A workflow registered as one tool (`workflows[].tool`), so a single apparent call can carry retries, a breaker and a human gate. See [Workflows: how runs work](workflows.md). |

## Next steps

- [Overview](README.md) — what agentd is and where to start.
- [Architecture](architecture.md) — how these parts fit together.
- [Configuration reference](configuration.md) — every key behind these terms.
