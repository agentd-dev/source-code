---
name: agentd
description: Install, configure, run and operate agentd — the durable agent daemon. Use when setting up agentd, writing or reviewing an agentd YAML config, wiring tools via MCP or the exec runner, attaching the TUI/web UI, building a coding agent on it, or diagnosing a non-zero agentd exit code.
---

# agentd

agentd is a single static binary that runs an LLM agent as a **daemon**: state
(conversations, tasks, workflow runs) lives in the process, not in a CLI. It
links no unix/vsock/stdio transport and — by default — **executes nothing
locally**; every capability is something the operator wires on. Terminal and
browser UIs attach to it as thin clients and render the same live state.

## The rule that prevents most mistakes

**Validate before you run.** Config is checked in full before any LLM call:

```sh
agentd --validate-config -c agent.yaml   # exit 0 = good, 2 = bad, in milliseconds
```

Never debug a config by running it and watching what happens. Validate, read the
error (it names the path), fix, re-validate. When unsure what a key is called:

```sh
agentd --config-schema | less    # every key, type and default
agentd --capabilities              # what THIS binary was built with
```

`--capabilities` matters because features are compile-time: a config using
`a2a:` on a binary built without it is a startup error, not a silent no-op.

## Install

```sh
curl -fsSL https://agentd.dev/install.sh | sh      # Linux amd64/arm64, checksum-verified
```

Release binaries deliberately **omit `exec`** (the local command runner) and
`cel`. Anything needing those is a source build:

```sh
cargo build -p agentd-cli --release --features a2a,exec
```

Docker: `ghcr.io/agentd-dev/agentd:<version>` (multi-arch, cosign-signed).

## A minimal working config

```yaml
config_version: "1"          # required

agent:
  name: helper
  instruction: |             # standing policy: HOW to work, not today's task
    You are concise and verify before you claim.

intelligence:
  endpoints: https://api.openai.com/v1    # OpenAI-compatible wire; a comma-list is failover
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"      # a REFERENCE — never paste the key
  budget:
    windows: [{ per: day, tokens: 2000000 }]
    on_exhausted: refuse

limits:
  run: { steps: 50, tokens: 200000, deadline: 10m }

lifecycle:
  run_until: drained         # stay up as a daemon; omit for one-shot-and-exit
```

Every config path is also a flag and an env var, with
`built-in < file < env < flag` precedence:
`agent.instruction` ⇄ `--agent-instruction` ⇄ `AGENTD_AGENT_INSTRUCTION`.
Multiple `-c` files merge in order (later wins) — use that for overlays rather
than templating one file.

## Secrets

Use references. `{{secret:NAME}}` reads env `NAME`; `{{secret-file:/path}}`
reads a file. Resolved at startup, never logged, never echoed by `/config`. An
inline literal key in YAML is a review defect — say so.

## Giving it tools

agentd has no built-in file or shell tools. Two routes, and they compose:

**MCP servers over HTTP(S)** — the default route. There is no stdio transport;
a stdio-only server needs a bridge you run.

```yaml
mcp:
  servers:
    - name: fs
      endpoint: https://mcp-fs.internal/mcp
      tags: { "*": [sensitive] }
```

**The `exec` runner** — local commands, off at two layers (the `exec` cargo
feature *and* `security.exec.enabled`). argv only, never a shell:

```yaml
security:
  exec:
    enabled: true
    workdir: /work                  # canonicalized; no `..` or symlink escape
    allow: [git, rg, ls, cat]       # argv[0] allow-list; EMPTY denies everything
    timeout: 120s
    env: [PATH, HOME, LANG]         # the agent's own env never reaches the child
```

Start read-only and widen. Adding `bash` grants everything — a deliberate
operator decision, never a default.

**The service catalog** — past a handful of servers, name each external
service ONCE in `services:` (endpoint, one shared credential, authoritative
trifecta tags, a tool ceiling) and reference it: `mcp.servers: [{name: money,
service: billing, allow: [charge_lookup]}]`. Consumers may only narrow;
catalog tags apply to any matching endpoint even inline (no tag-laundering);
`security.egress: closed` refuses any uncatalogued dial. One `agentd login
service:billing` serves every consumer.

**Subagent templates** — declare what a worker looks like in
`subagents.templates` (the model instantiates by name and fills declared,
schema-checked `params` only; `allow_freeform: false` makes templates the
only spawn path). A template whose instruction embeds machinery
(`:::workflow`, `:::mcp`, …) spawns a full **instance child**: its own
workflows and store, an A2A peer over a unix socket, retired by
`ttl`/`until`/`subagent.retire`.

**The system prompt is a template** — `agentd --context-template` prints the
built-in one, written in a two-block language (`{{#if}}`, `{{#each}}`,
interpolation) over the agent's environment (services, workflows, peers,
signals, memory, granted tools). Override it with `context.template`, name
alternates in `context.templates` for a node to pick with
`context: {template: <name>}`. Expressions are a path first and CEL second,
so bare lookups need no build feature. Order it stable-to-volatile: providers
cache on the literal prefix, so a section that changes every turn invalidates
everything after it.

### The trifecta refusal is a feature

If a config combines **untrusted input** + **sensitive powers** + an **egress
path**, agentd refuses to start unless `--allow-trifecta` is passed. That is the
exfiltration shape. Never add the flag to silence a startup error without
saying, in words, which of the three legs the operator is accepting.

## Attaching a UI

```sh
agentd tui -c agent.yaml     # daemon + fullscreen terminal UI (--inline for in-place)
agentd ui  -c agent.yaml     # daemon + web UI in a browser
```

Both need `interface.enabled: true` (the subcommand sets it) and an `a2a`
listener. Detached instead — the daemon keeps working when the client quits:

```sh
agentd -c agent.yaml &
npm i -g @agentd-dev/cli && agentd-tui --endpoint http://127.0.0.1:8420
```

Loopback callers are the operator with no credential. **Binding non-loopback
requires client auth** (mTLS, bearer, or a rotating pairing code) — never
suggest `0.0.0.0` without it.

Clients are projections: N of them show the same state, and none of them hold
capabilities. If something is missing in the UI, fix the daemon, not the client.

## Diagnosing a failure

The exit code *is* the terminal status — branch on it, don't parse stdout:

| code | meaning | first thing to check |
|---|---|---|
| 0 | completed | — |
| 2 | bad config | run `--validate-config`; it names the path |
| 3 | partial result | `limits.run` too tight? |
| 4 | intelligence unreachable / auth failed | endpoint URL, token reference resolving |
| 5 | refused | preflight/policy — read the reason on stderr |
| 6 | a required MCP server is down | that server's health; `required: false` to soften |
| 7 | budget hit (steps/tokens/deadline) | `limits.run`, `intelligence.budget` |
| 124 | supervisor hard-kill backstop | a child that would not self-terminate |

stdout carries the result; **stderr carries JSON-lines telemetry**, one event
per line, trace-correlated. Filter it rather than reading it raw:

```sh
agentd -c agent.yaml 2>&1 >/dev/null | grep -F '"level":"error"'
```

## Building a coding agent on it

The common ask ("something like Claude Code, but hosted"). Requires a source
build with `exec`; the complete recipe, allow-list ladder and approval wiring is
in [reference/coding-agent.md](reference/coding-agent.md).

Key points: `preflight: never` (you are the intent), `ask_human_fallback: wait`
(a question with nobody to answer it parks instead of guessing), one instance
per repository (`workdir` is the fence), and `store` pointed somewhere durable
if the session should survive a restart.

## Reference

- [reference/config.md](reference/config.md) — the config sections that matter,
  with the keys worth knowing in each.
- [reference/coding-agent.md](reference/coding-agent.md) — the full
  coding-agent recipe.
- Full docs: <https://agentd.dev/docs/> · configuration, security, interface,
  workflows, MCP, and the RFCs behind them.

## Working on the agentd repo itself

Rust workspace; `crates/agentd` (core), `agentd-cli`, `mcp`, `net`, plus the
Node clients in `interface/`.

- `cargo test --all-features` before claiming anything is green — and
  `cargo build` with **default** features too: the default build has a
  3-dependency moat (libc, serde, serde_json) that `--all-features` masks.
- New network/crypto work reuses `ring` behind a feature; adding a dependency to
  the default build breaks the moat and needs an explicit decision.
- Commit and push only when asked.
