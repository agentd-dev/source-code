# agentd

**Run AI agents the way you run services.** agentd is one small Rust binary
that runs an agent from a configuration file: a one-shot job that exits with
a meaningful code, or a daemon that wakes on schedules, webhooks, MCP
resource changes and upstream events. Its tools come from MCP servers you
name, people and other agents reach it over A2A, and the reasoning runs in
child processes a supervisor can budget and kill.

[Documentation](https://agentd.dev/docs/) ·
[Quickstart](https://agentd.dev/docs/getting-started/) ·
[Why agentd](https://agentd.dev/docs/why-agentd/) ·
[Releases](https://github.com/agentd-dev/source-code/releases)

## Who it is for

Teams that want agents in production with the discipline they already
apply to services: reviewed configuration, bounded spend, a known blast
radius, durable work and an audit trail. It fits event-driven back-office
work, scheduled jobs, served workers other systems call, multi-agent teams,
a coding agent you host yourself, and edge devices where the agent is the
only thing the machine runs.

It is not a library you write an agent in, and not a hosted service.

## What sets it apart

- **No ambient tools.** Every capability comes from an MCP server you
  declared, so a run's blast radius is the servers in its configuration.
- **Reasoning runs where it can be killed.** A supervisor that never talks
  to a model holds the state and the credentials, enforces budgets and
  deadlines, and kills a loop that misbehaves.
- **Durable by default.** Workflows and conversations are checkpointed to a
  store; a restart resumes work instead of repeating it.
- **The right model for each job.** Chat, reasoning and decision models,
  each bound to a role — the agent's turns, deep thinking, typed decisions,
  triage, approvals — with fallback chains across providers.
- **Operable.** Configuration is validated before anything runs, the exit
  code is an API, and every event is a JSON line on stderr.
- **Open protocols.** MCP for tools and context, A2A for people and agents,
  OpenAI-compatible, Anthropic and Bedrock model wires, OTLP and Prometheus
  for telemetry.

## Install

```bash
curl -fsSL https://agentd.dev/install.sh | sh
```

Linux on x86-64 and arm64. The container image is
`ghcr.io/agentd-dev/agentd`, and the terminal and web clients are
`npm install -g @agentd-dev/cli`. [Install](https://agentd.dev/docs/install/)
covers every option, including building from source.

## Quickstart

```bash
export OPENAI_API_KEY="paste-your-key-here"
agentd \
  --instruction "Explain in three sentences what the Model Context Protocol is for." \
  --intelligence https://api.openai.com/v1/chat/completions \
  --model gpt-5.1 \
  --intelligence-token '{{secret:OPENAI_API_KEY}}'
```

The answer prints on stdout, telemetry streams to stderr as JSON lines, and
the exit code says how the run ended. The
[Quickstart](https://agentd.dev/docs/getting-started/) continues from here:
a tool from a public MCP server, the same agent as a daemon, and the
terminal UI attached to it.

## Where to go next

| You want to… | Start with |
|---|---|
| decide whether agentd fits | [Why agentd](https://agentd.dev/docs/why-agentd/), [Use cases](https://agentd.dev/docs/use-cases/), [Security at a glance](https://agentd.dev/docs/security-overview/) |
| build your first agents | [Quickstart](https://agentd.dev/docs/getting-started/), [Your first workflow](https://agentd.dev/docs/first-workflow/), [Build a coding agent](https://agentd.dev/docs/coding-agent/) |
| connect models and tools | [Connect model providers](https://agentd.dev/docs/connect-models/), [Connect MCP servers](https://agentd.dev/docs/connect-mcp/) |
| deploy and operate it | [Deploy](https://agentd.dev/docs/deployment/), [Monitor in production](https://agentd.dev/docs/observability/), [Harden a deployment](https://agentd.dev/docs/harden/) |
| understand how it works | [Architecture](https://agentd.dev/docs/architecture/), [Security model](https://agentd.dev/docs/security/) |
| look something up | [Configuration reference](https://agentd.dev/docs/configuration/), [CLI and exit codes](https://agentd.dev/docs/cli/), [Workflow node reference](https://agentd.dev/docs/node-registry/) |
| build on it | [Embed agentd in Rust](https://agentd.dev/docs/embedding/), [Build a client](https://agentd.dev/docs/build-a-client/) |

The documentation lives in [`docs/`](docs/) and is published at
[agentd.dev/docs](https://agentd.dev/docs/). Design decisions are recorded
in [`rfcs/`](rfcs/).

## Crates

| Crate | Responsible for |
|---|---|
| `agentd-cli` | The `agentd` binary |
| `agentd-core` | The engine: supervisor, agent loop, workflows, intelligence, A2A, state |
| `agentd-mcp` | The MCP client over the official Rust SDK, and a raw HTTP listener |
| `agentd-net` | Blocking HTTP/1.1 and SSE, unix and TLS connects, the SSRF guard |
| `agentd-instruction` | The Instruction Specification: parse, validate, deliver, verify |

## License

AGPL-3.0-only — see [LICENSE](LICENSE). `agentd-instruction` is
`MIT OR Apache-2.0`, and the conformance corpus under Apache-2.0; see
[NOTICE](NOTICE). Commercial licensing (for proprietary embedding or
AGPL-free use) and commercial support are available: agent@agentd.dev.
