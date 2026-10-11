# Why agentd

This page is for the person deciding whether agentd belongs in their stack: a
CTO, a platform lead, an architect. It states the problem agentd solves, the
principles it is built on, what it is good for and what it will not do, how it
compares with the alternatives, and what adopting it costs, so you can judge
whether it fits.

## The problem: agents in production

An agent that does useful work holds real credentials and acts on them. That
is the product, and it is also the risk. Four things go wrong once an agent
leaves the demo:

- **What it reads can steer it.** A ticket, an email or a web page can carry
  instructions, and no model reliably tells an injected instruction from a
  legitimate one. Prompt injection is not a bug you patch; a guardrail that
  works 95% of the time fails in security terms.
- **It has to keep running.** Real work arrives on events and schedules, waits
  days for a reply, and must survive a crash, a redeploy and a node eviction
  without repeating what it already did.
- **It spends money per token.** A loop that never converges, or a flood of
  events, turns into a bill. Spend needs a ceiling that the model cannot argue
  with.
- **Someone has to answer for it.** Which decisions did a person approve, what
  did the agent say to whom, which tool did it call with what — an operator
  needs that record, and a reviewer needs to know what the agent could have
  done.

Most agent tooling starts from the loop: a model, a list of functions, a
process that runs them. Containment, durability, approvals and audit are left
to the application. agentd starts from the other end: it is the process that
runs the loop for you, under rules you configure.

## The principles

**The configuration is the fence.** An agent's authority is the set of MCP
servers its configuration declares, narrowed per step and per child. agentd
ships no tools of its own, so there is nothing ambient to remove. A change to
what an agent may do is a diff to one file, reviewed like code.

**The supervisor runs no agent loop.** One process owns state, credentials,
budgets and the process tree, and no conversation reaches it. The reasoning
runs in child processes it can kill, and limits are enforced by code that has
no model in it. The two opt-in judges that run beside the supervisor — the
goal check and automatic approvals — each put one structured question to a
model, with no tools, and can only answer that question.

**Unsafe combinations are refused, not warned about.** A configuration that
gives one agent untrusted input, sensitive data and a way to send data out —
the three legs of the "lethal trifecta" — does not start. A listener off the
local machine without client authentication does not start. A token written
inline where the file expects a secret reference does not load.
`agentd --validate-config` runs the same checks in CI, before anything runs,
and exits `2` on any of them.

**Durable by default.** Every unit of progress is written to a store before its
effect runs. A restart resumes runs, timers and waits. State transitions happen
exactly once; effects happen at least once, and each MCP tool call carries an
idempotency key so a well-behaved server can collapse a replay.

**Standard protocols, through their official SDKs.** Tools and context come in
over MCP; people and other agents reach in over A2A. agentd uses the official
Rust SDK of each protocol rather than its own reading of the specification,
because a misread protocol fails silently in the peer.

**A model for each job.** Turns, thinking, triage, compaction, the goal check
and approvals each run on the model their role names. A decision model answers
typed questions — classify, route, guard a step — without writing text, so
those jobs need not run on a frontier model. Every answer that has a JSON
Schema is held to it by the provider and checked again by agentd.

**Small and self-contained.** agentd is one statically linked Linux binary
that runs in a `FROM scratch` image with no shell.
[Engineering choices](why-rust.md#the-numbers) has its measured size, memory
and start-up time, and what they were measured on.

## What it is good for

| Shape | What agentd adds |
|---|---|
| One-shot jobs in CI or a Kubernetes `Job` | an exit code a scheduler can branch on, a hard budget, a validated config |
| Reactive daemons on MCP resources, webhooks or MCP Events | idle at near-zero cost, durable runs, at-least-once delivery |
| Scheduled audits and sweeps | in-process schedules or an external `CronJob`, the same binary either way |
| Triage and routing | cheap decision models for classification and step guards |
| Pipelines that fan out | bounded parallel children, each with a narrowed tool set |
| Multi-agent teams | A2A between instances, typed commands, the trifecta split across processes |
| Served workers | an A2A endpoint other systems call, with principals and per-caller ownership |
| A coding agent you host | a session in a daemon, approvals in every attached client, a fenced command runner |
| Edge devices and appliances | a supervisor that reaps, restarts and drains, small enough to be the init process |

[Use cases](use-cases.md) shows each shape with a configuration and a
runnable example.

## Agent turns and workflows

agentd runs both. An agent turn lets the model choose the sequence of tool
calls; use it when the task is open-ended. A workflow is a declared graph of
steps; use it when the work must survive a crash, when steps can run in
parallel, when something must wait — for a signal, a reply, a person — or when
each step needs its own timeout, retry and budget. Deterministic steps cost no
tokens. A workflow fixes its sequence when it is written, so a step cannot
invent a decision nobody anticipated; that rigidity is the price of
resumability.

## What it deliberately does not do

State the limits up front, so you size what surrounds agentd correctly.

- **It is not a sandbox.** agentd uses no seccomp, namespaces or chroot. The
  container, VM or host policy around it is the sandbox; agentd bounds what an
  agent can reach through its tools.
- **It does not detect injection.** There is no classifier. The defence is
  structural: what a compromised loop can reach, not what it intends.
- **It does not give you exactly-once effects.** It gives exactly-once state
  transitions and an idempotency key; collapsing a replay is the remote
  server's job.
- **It does not scale one instance to thousands of connections.** An instance
  is sized for tens of concurrent children. You scale out with more instances.
- **It does not account in currency.** Budgets are in tokens and requests.
  The model of every turn is on the log line, so a rate card applied downstream
  gives you money.
- **It does not stream tokens to clients.** Attached clients see live activity
  — phase, current tool, spend — and then the finished answer.
- **It runs no local commands in a shipped binary.** The guarded command
  runner exists only in a build from source with the `exec` feature, and only
  when the configuration enables it.
- **It speaks MCP over HTTP only.** There is no stdio transport; a stdio-only
  MCP server needs a bridge.
- **It offers no Python or JavaScript authoring SDK.** Agents are
  configuration and instruction documents. The engine embeds in Rust.

## How it compares

| | Agent framework (a library) | Hosted agent platform | Workflow engine | agentd |
|---|---|---|---|---|
| Where the loop runs | in your application process | in the vendor's cloud | not a model loop; your code in workers | in killable child processes under a supervisor you run |
| Where tools come from | functions you register | the platform's connectors | your activity code | MCP servers you declare; none built in |
| State across restarts | yours to build | the platform's | the engine's core strength | a store you choose: file, MCP or HTTP |
| Containment | yours to build | the platform's policy | not its concern | budgets, kill ladder, trifecta refusal, egress allow-list |
| How you change behaviour | edit and redeploy code | the vendor's console | edit and redeploy code | edit a reviewed config or instruction document |
| Protocols | varies | the vendor's | the engine's SDK | MCP out, A2A in |
| Who operates it | you, as part of your app | the vendor | you | you, as one more service |

Choose something else when it fits better:

- **An agent framework** when the agent is a feature inside your application
  and you want to write it in Python or TypeScript, call by call.
- **A hosted platform** when you would rather not run the infrastructure and
  can accept the vendor's connectors, policies and data handling.
- **A workflow engine** when the work is mostly deterministic code with a
  model call here and there, and you already operate one. agentd's workflows
  are declared in configuration and carry agent-specific steps, but they are
  not a general-purpose replacement for an engine your code is written
  against.
- **A coding CLI** when you want a coding agent in a box with zero setup.
  agentd asks you to wire the tools, and in exchange the session lives in a
  daemon several people can attach to.

## What adoption costs

**License.** agentd is AGPL-3.0-only: the binary, the engine crates and the
npm display clients. One crate, `agentd-instruction` (the Instruction
Specification's reference parser), is MIT OR Apache-2.0 so that other
implementations can link it. Commercial licensing — for proprietary embedding
or use without the AGPL's terms — and commercial support are available from
agent@agentd.dev. Ask your counsel how the AGPL applies to your deployment.

**Platforms.** Prebuilt binaries are static Linux builds for amd64 and arm64.
The container image `ghcr.io/agentd-dev/agentd` is multi-arch, cosign-signed
and carries an SPDX SBOM attestation. There are no prebuilt macOS or Windows
binaries. A source build needs Rust 1.96 or newer and a C compiler. The
terminal and web clients are one npm package, `@agentd-dev/cli`.

**What you supply.** A model provider account (or an OpenAI-compatible
gateway in front of yours). An MCP server for each system the agent touches —
standing those up is real integration work. For a daemon in a container, a
store that outlives the container: an MCP or HTTP store, or a persistent
volume for the file store.

**Skills.** Writing agents means writing YAML and Markdown instruction
documents; expressions in guards and filters use CEL. Operating agentd needs
the skills any service needs: exit codes, JSON-lines logs, Prometheus metrics,
OTLP export. Rust is needed only to embed the engine or to build the local
command runner.

## Next steps

- [Use cases](use-cases.md) — the shapes people deploy, with examples.
- [Security at a glance](security-overview.md) — the containment model for
  reviewers.
- [Interoperability](interoperability.md) — protocols, providers and
  platforms.
- [Quickstart](getting-started.md) — a first agent running.
