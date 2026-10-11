# FAQ

Short answers to the questions evaluators and newcomers ask most, each with a
link to the page that has the full answer. If your question is about a
specific setting, the [Configuration reference](configuration.md) is faster.

## The product

### Is agentd an agent framework?

No. A framework is a library you write an agent in, and the agent runs inside
your application. agentd is a runtime you configure and operate: you give it an
instruction, models and MCP servers, and it runs the agent as its own service,
with its own process tree, store, budgets and network surface. If you want to
embed the engine anyway, it is a Rust crate; see [Embed agentd in
Rust](embedding.md).

### How does it compare with other options?

[Why agentd](why-agentd.md) compares it with agent frameworks, hosted agent
platforms, workflow engines and coding CLIs, and says when each is the better
choice.

### What is the license?

AGPL-3.0-only, for the binary, the engine crates and the npm display clients.
The `agentd-instruction` crate is MIT OR Apache-2.0. Commercial licensing and
commercial support are available from agent@agentd.dev.

### Is there a hosted version?

No. agentd is software you run. The web UI is a static bundle you can serve
from any static host; the page connects from the browser directly to your own
daemon, so the host never sees a request to an agent. See [Host the web
UI](hosting-the-ui.md).

### Does agentd send data anywhere?

agentd dials the endpoints its configuration names — model providers, MCP
servers, A2A peers, a remote store, an OTLP collector — and the token services
their credentials need. It sends no usage telemetry of its own.

## Models and tools

### Which models does it work with?

Anything that speaks the OpenAI-compatible Chat Completions wire (OpenAI,
Azure OpenAI, vLLM, Ollama, LM Studio, most gateways), Anthropic's Messages
API, and Amazon Bedrock's Converse API. Decision models such as Jev are reached
with their own `decisions` wire. Other providers work through an
OpenAI-compatible gateway. The model must support native tool calling. See
[Interoperability](interoperability.md#model-providers).

### Can one agent use several models?

Yes. Name several providers and models, then give each job a role: turns,
thinking, triage, compaction, the goal check, approvals and decisions can each
run on a different model. A preflight step can route each incoming message to
a fast or a deep model. See [Models, providers and roles](intelligence.md).

### Can I try it without a model provider?

Yes. The release binaries and the image carry a mock model endpoint that
answers with no key and no network. `--intelligence mock:final` answers every
call at once, and `mock:file:<playbook.json>` answers from a script you write,
decisions included. It is for trying a configuration or a workflow, not a
model. [Your first workflow](first-workflow.md#run-it-without-a-model-account)
runs on it.

### Can I run the model locally?

Yes. Point the endpoint at a local server: for a vLLM or Ollama on the same
host, `http://127.0.0.1:8000/v1/chat/completions` (plain `http://` is allowed
to loopback only), or an `https://` endpoint on your network.

### Where do the tools come from?

From MCP servers you declare. agentd ships no file, shell or HTTP tools of its
own; its built-in tools manage the agent itself — memory, plans, artifacts,
subagents, workflows, approvals. See [MCP: tools and context](mcp.md).

### Can it run commands on the machine it runs on?

Only in a binary you build yourself. The guarded `exec` runner is not in the
release binaries or the image. Build with the `exec` feature and set
`security.exec.enabled`; the runner then executes allow-listed commands,
without a shell, inside a confined working directory. Alternatively, run an MCP
server on loopback that exposes exactly the commands you want. See [Build a
coding agent](coding-agent.md).

### Can I use an MCP server that speaks stdio?

Not directly. agentd speaks MCP over HTTP(S) only. Run the server behind a
bridge that exposes Streamable HTTP, on loopback or on your network.

## Running it

### What platforms does it run on?

Linux on amd64 and arm64, as a static binary or the container image
`ghcr.io/agentd-dev/agentd`. There are no prebuilt macOS or Windows binaries.
The terminal and web clients are an npm package and run wherever Node.js does.
See [Install](install.md).

### Does it need Kubernetes?

No. It is one binary with no runtime dependencies: run it from a shell, under
systemd, in Docker, or in Kubernetes. The exit codes and the drain behaviour
are designed to fit a Kubernetes `Job`, `CronJob` or `Deployment` when you use
one. See [Deploy](deployment.md).

### Does it need a database?

No. Durable state goes to a store behind a four-operation contract: a local
file store (the default for a long-lived instance), any MCP server with
storage tools, or a plain HTTP service. A daemon in a container should use a
store that outlives the container.

### What happens when it crashes or restarts?

A daemon resumes. Every accepted message, fired trigger, workflow step and
timer is written to the store before its effect runs, so a restart re-arms
timers, re-opens tasks and continues runs from their last checkpoint. State
transitions happen exactly once; effects happen at least once, with an
idempotency key on every tool call. A workflow's human gate is rebuilt after a
restart. A question an agent asked in the middle of a conversation turn is
not: the turn ends with the process, and the answer starts a fresh turn.
A one-shot job with no store re-runs from scratch. See [Workflows: how runs
work](workflows.md).

### How do I talk to a running agent?

`agentd tui --config agent.yaml` starts the daemon and a terminal UI together;
`agentd ui` does the same with a browser. Any A2A 1.0 client can talk to it
too. See [Use the terminal and web UI](interface.md).

### Can several replicas share the work?

Yes, without a coordination protocol of agentd's own. Partition the work at
the source, or use the queue's own claim and lease semantics from a workflow
step. Two writers on one store namespace cannot corrupt each other: a
conflicting write stops the instance that lost. See [Scale out](scaling.md).

### Can it run offline?

With a local model and local MCP servers, yes: agentd needs nothing but the
endpoints you configure. Workflows made only of data steps and tool calls run
with no model at all.

## Cost and control

### How is spend controlled?

At several levels, all in tokens and requests:

- each turn and run has step, token and deadline limits (by default 500
  steps, 2,000,000 tokens and one hour);
- `intelligence.budget` caps the instance over time windows and says what
  happens when a window is spent: wait, slow down, degrade to a cheaper model,
  refuse, or fail;
- conversations and A2A callers can carry budgets of their own;
- roles put cheap models on cheap jobs.

agentd does not convert tokens to money; the model of every turn is on the
log line, so you can apply a rate card downstream. See [Control spend with
budgets](budgets.md).

### How do I stop a runaway agent?

It stops itself at its budget. A run that exhausts its steps or tokens ends,
and so does a run past its deadline, an in-flight model call included; a
one-shot job then exits `7` or `124`, and a daemon carries on with its other
work. By hand, `SIGTERM` drains the daemon — in-flight work finishes, nothing
new starts, and it exits `0` — and an operator can pause, cancel or drain over
A2A. See
[Operate a running daemon](operations.md).

### Can a person approve what the agent does?

Yes. The agent can ask a person (`ask_human`), a workflow can stop at a
`human` step, and `security.policies` can hold any tool call — every call to an
`egress` tool, say — until someone approves it. Questions appear as answerable
rows in every attached client. See [Human in the loop](human-in-the-loop.md).

## Security

### What stops a prompt injection from doing damage?

Structure, not detection. The agent can only use the servers you declared; an
agent holding untrusted input, sensitive data and a way to send data out does
not start; untrusted text is read in a child process that returns only a
summary; and the supervisor that holds the credentials runs no agent loop.
See [Security at a glance](security-overview.md).

### Is agentd a sandbox?

No. It bounds what an agent can reach through its tools, and contains its
reasoning in killable processes, but it uses no seccomp, namespaces or chroot.
Run it inside a container or VM with its own limits.

## What is not supported

| Not supported | Instead |
|---|---|
| MCP `sampling`, `roots`, `tasks` and `logging` | none; a server's request is dropped |
| MCP over stdio | a bridge that serves Streamable HTTP |
| agentd as an MCP server | reach it over A2A |
| A2A 0.3, and the A2A gRPC and HTTP+JSON bindings | the A2A 1.0 JSON-RPC binding |
| prebuilt macOS and Windows binaries | build from source, or run the container image |
| native Gemini or Cohere wires | an OpenAI-compatible gateway |
| local commands in shipped binaries | a build with the `exec` feature, or an MCP server |
| token-by-token streaming to clients | live activity, then the finished answer |
| budgets in currency | token budgets and a rate card applied downstream |
| exactly-once side effects | idempotency keys a server can honour |
| sandboxing | the container or VM around agentd |

## Next steps

- [Why agentd](why-agentd.md) — the problem, the principles and the fit.
- [Quickstart](getting-started.md) — a first agent running.
- [Glossary](glossary.md) — the terms these pages use.
