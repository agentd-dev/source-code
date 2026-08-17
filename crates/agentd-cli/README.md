# agentd

An agent runtime. One static binary: give it an instruction and tools from
remote MCP servers, and it runs the agentic loop — think, call a tool, observe,
self-correct — as a one-shot job or a long-lived daemon.

```console
$ cargo install agentd-cli
$ agentd --prompt "triage the newest issue and label it" \
    --mcp github=https://mcp-github.internal/mcp \
    --intelligence https://gateway.internal/v1
```

Or without Rust: `curl -fsSL https://agentd.dev/install.sh | sh`, or
`docker run --rm ghcr.io/agentd-dev/agentd:latest --help`.

*(The crate is `agentd-cli` because `agentd` on crates.io belongs to an
unrelated project. The binary it installs is `agentd`.)*

## What it is

**It runs no code of its own.** Every capability is a tool on an MCP server you
named. Nothing is implicit — what the agent can do is exactly what you wired.

**The supervisor holds no model.** Lifecycle, limits and the process tree belong
to a component that cannot be prompted; the reasoning runs in child processes it
can always kill. A jailbroken model still meets a process that will not
negotiate.

**It stays up.** Durable workflows checkpoint before every effect, so a killed
host resumes in-flight runs rather than starting over. A `human` step suspends a
run and renders as an answerable question in every attached client — and the run
survives a restart while it waits.

**You can attach to it.** `agentd tui -c agent.yaml` runs the daemon and a
terminal UI together; there is a web UI too. The daemon holds all the state, so
quitting a client leaves the agent working.

## In 20 lines

```yaml
config_version: "2"
agent:
  name: triage
  instruction: You triage incoming issues. Be precise; ask if unsure.
intelligence:
  endpoints: https://gateway.internal/v1
  model: gpt-5.1
mcp:
  servers:
    - { name: github, endpoint: https://mcp-github.internal/mcp }
store: { kind: mcp, mcp: { server: github } }
workflows:
  - name: triage
    steps:
      new:  { kind: subscribe, server: github, uri: "github:///issues" }
      work: { kind: agent, depends_on: [new], instruction: "Triage {{payload.title}}" }
      done: { kind: finish, depends_on: [work], status: completed }
```

`agentd --validate-config -c agent.yaml` checks it — including the workflow
bodies — before anything runs.

## Documentation

<https://agentd.dev/docs/> — getting started, configuration, workflows, MCP,
A2A, security, deployment. Licensed under Apache-2.0.
