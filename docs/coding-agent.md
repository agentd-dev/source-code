# agentd for software engineering — a coding agent you host

A recipe for the shape most people want first: **a pair-programming agent you
talk to in a terminal**, like Claude Code or Codex CLI — except the session
lives in a daemon you run, not in the CLI. That one difference is what you get
in exchange for wiring it up yourself: the work survives the client, several
people (or several of your devices) can watch the same session, and the same
instance can also run scheduled and event-triggered engineering chores.

```sh
agentd tui --config coding.yaml     # daemon + terminal UI, one command
```

> **Read this first if you are coming from Claude Code.** agentd ships **no
> file-editing tools of its own** — see [§2](#2-giving-it-hands). It is a
> runtime you assemble a coding agent on, not a coding agent in a box. The
> assembly is about twenty lines of YAML, and §1 is a complete working one.

---

## Contents

1. [The 5-minute setup](#1-the-5-minute-setup)
2. [Giving it hands: `exec` vs MCP](#2-giving-it-hands)
3. [Approvals — the permission prompt, server-side](#3-approvals)
4. [Keeping it bounded (cost, context, blast radius)](#4-keeping-it-bounded)
5. [Working practices](#5-working-practices)
6. [Beyond chat: chores that run themselves](#6-beyond-chat)
7. [Honest limits](#7-honest-limits)

---

## How this differs from a coding CLI

Neither column is "better" — they are different products. Pick deliberately.

| | Claude Code / Codex CLI | agentd + TUI |
|---|---|---|
| Where the session lives | in the CLI process | in the **daemon**; clients are thin projections |
| Quit the client | session ends | session continues; re-attach and it is still there |
| A second surface | — | terminal **and** browser **and** another machine, all live at once |
| Tools | built in, curated | **you wire them**: MCP servers over HTTPS, or the guarded `exec` runner |
| Approvals | built-in prompts | `ask_human` gates, rendered as answerable rows in *every* attached client |
| Recurring work | — | workflows on a schedule / webhook / signal, in the same instance |
| Guardrails | model + prompt | allow-list, workdir confinement, budgets, the Rule-of-Two trifecta check |
| Setup cost | near zero | ~20 lines of YAML |

---

## 1. The 5-minute setup

**Build.** Local command execution is deliberately absent from release
binaries, so a coding agent is a build-from-source setup:

```sh
cargo build --release --features a2a,exec
# a2a  → the listener the TUI/web UI attach to
# exec → the guarded local command runner (default-OFF even when compiled in)
```

**Configure** (`coding.yaml`):

```yaml
config_version: "1"

agent:
  name: coder
  # Your CLAUDE.md equivalent. Keep it about HOW to work, not what the task is.
  instruction: |
    You are a careful engineer working in the repository at /work.

    Explore before you edit: read the relevant files, run the tests, and say
    what you found. Prefer the smallest change that fixes the cause.

    Ask before you act destructively — anything that rewrites history, force
    pushes, deletes files, or touches credentials. Use ask_human for that and
    wait for the answer.

    When you finish, state what changed and how you verified it.
  preflight: never          # skip the intent classifier; you are the intent
  ask_human_fallback: wait  # a question with nobody to answer it parks, never guesses

intelligence:
  endpoints: https://api.openai.com/v1
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"   # a reference, never the key itself
  budget:
    windows: [{ per: day, tokens: 2000000 }]
    on_exhausted: refuse

store:
  kind: memory            # see §4 for making the session durable

a2a:
  listen: http://127.0.0.1:8420   # loopback ⇒ you are the operator, no credential

interface:
  enabled: true           # the TUI/web-UI surface (default OFF)
  debug: false            # turn on per-session with /set when you need internals

security:
  exec:
    enabled: true
    workdir: /work                       # every command is confined here
    allow: [git, rg, ls, cat, sed, cargo] # argv[0] allow-list; EMPTY denies all
    timeout: 120s
    max_output: 262144
    env: [PATH, HOME, LANG]              # the agent's own env never reaches the child

limits:
  run: { steps: 60, tokens: 400000, deadline: 30m }

lifecycle:
  run_until: drained      # a daemon: stay up between messages
```

**Run:**

```sh
export OPENAI_API_KEY=sk-…
agentd tui --config coding.yaml
```

That starts the daemon *and* the terminal UI, ties their lifetimes, and puts
the daemon's log lines in a file instead of your screen. Prefer them separate?
Run `agentd --config coding.yaml` in one shell and `agentd-tui --endpoint
http://127.0.0.1:8420` in another — now quitting the UI leaves the agent
working. See [interface.md](interface.md) for the client surface in full.

---

## 2. Giving it hands

agentd runs nothing of its own by default. A coding agent gets its abilities
from exactly two places, and they compose:

### Route A — the `exec` runner (local, guarded)

The pragmatic local choice, and what the config above uses. Every call is
fenced (details in [security.md §11](security.md)):

- **argv, never a shell** — no `sh -c`, so no globbing, `$(…)`, pipes, or
  command injection. Want a shell? Allow-list `bash` and call it explicitly:
  that is a conscious operator decision, not an accident.
- **allow-list on `argv[0]`**, empty by default (`enabled: true` alone runs
  nothing).
- **workdir confinement** — a requested `cwd` is canonicalized and must resolve
  inside `workdir`; no `..` or symlink escape.
- **timeout, output cap, minimal env** — the child never sees your secrets.

Practical allow-lists, in increasing order of trust:

```yaml
allow: [git, rg, ls, cat]                       # read-only reconnaissance
allow: [git, rg, ls, cat, cargo, npm, pytest]   # + run the tests
allow: [git, rg, ls, cat, sed, tee, cargo]      # + edit files in place
allow: [bash]                                   # everything; deliberate only
```

### Route B — MCP servers (structured, off-box)

Tools reached over **HTTPS** (agentd has no stdio transport — see
[mcp.md](mcp.md)). This is the better route when you want edits to be
structured rather than `sed`, or the work to happen in someone else's sandbox:

```yaml
mcp:
  servers:
    - name: fs
      endpoint: https://mcp-fs.internal/mcp
      tags: { "*": [sensitive] }
```

A useful hybrid: `exec` for reconnaissance and tests, an MCP server for writes.
You can also keep the *contract* and delegate the implementation — map `exec`
to an MCP server with `tools.overrides` and the command runs in that server's
sandbox instead of your machine.

> **The trifecta check.** `exec` carries `sensitive` + `egress` tags. Add an
> MCP server tagged `untrusted_input` (a web fetcher, an issue tracker) and
> agentd **refuses to start** unless you set `--allow-trifecta` — untrusted
> input plus sensitive powers plus an egress path is the exfiltration shape.
> That refusal is the feature; think before you override it.

---

## 3. Approvals

The analogue of a coding CLI's permission prompt — but it lives in the daemon,
so it reaches *every* attached client, and it survives a restart.

When the agent calls `ask_human` (your instruction should tell it when — see
§1), its task flips to `input-required` and the question renders as an
answerable row:

```
agent › Ready to force-push feature/rework over 3 commits. Proceed? [reply to continue]
› no — open a PR instead
```

Anything you type answers the newest gate; `#task-… your answer` answers a
specific one. Workflow `human` steps gate the same way and **survive a daemon
restart** — the run picks up where it stopped.

What happens when nobody is watching is a policy you set
(`agent.ask_human_fallback`):

| | behavior | when to use |
|---|---|---|
| `wait` | park until the ask times out | interactive sessions — you *will* come back (recommended here) |
| `fail` (default) | the ask errors; the agent decides what to do | headless/CI, where hanging for a day is worse |
| `auto` | an LLM judge answers conservatively on your behalf, always marked as auto | unattended runs where progress beats precision |

---

## 4. Keeping it bounded

- **Cost.** `intelligence.budget` windows with `on_exhausted: refuse` stop a
  runaway loop from spending your month. The working row shows spend live
  (`thinking · 12s · 1.2k tok`), and `/status` gives the totals.
- **Steps and time.** `limits.run.{steps,tokens,deadline}` bound a single turn.
- **Context.** Long sessions self-compact (`context.compact_at`); the
  conversation keeps a structured summary rather than truncating blindly.
- **Blast radius.** `workdir` + allow-list are the real fence; the model's
  cooperation is not a control. Start read-only and widen as you trust it.
- **Durability.** `store.kind: memory` keeps everything in the process — fine
  to start, but the session dies with the daemon. Point `store` at an MCP or
  HTTP store ([RFC 0025](../rfcs/0025-durable-state-and-store-adapters.md)) and
  conversations, tasks and workflow runs survive a restart — including a
  pending approval.

---

## 5. Working practices

- **One instance per repository.** `workdir` is the fence; two repos in one
  instance means the fence protects neither.
- **The instruction is your standing policy** — how to work, what to ask
  before, what "done" means. Task-specific detail belongs in the message, not
  the instruction. Reusable playbooks belong in skills (`@skill` in the
  composer pulls one in).
- **Start read-only.** `[git, rg, ls, cat]` for a day. Widen when the agent has
  earned it, not before.
- **Keep `interface.debug` off, and flip it when you need it:**
  `/set interface.debug true` opens the feed, per-step run detail and the log
  tail live, for every attached client at once. Turn it back off after.
- **`/pause` before you edit the tree yourself.** It parks dispatch (intake
  continues), so you and the agent are not writing the same file. `/resume`
  when you are done — nothing is lost, unlike a cancel.
- **Watch from a second surface.** Leave the TUI at your desk and open the web
  UI on another screen or your phone; both render the same session live. From
  another machine, `/pair` gives a rotating code — `agentd-tui --endpoint … --code 483921`
  — so no bearer needs copying around ([interface.md §4.3](interface.md)).
- **Delegate exploration.** "Check whether this pattern appears elsewhere" is a
  subagent's job: it runs in its own process with its own context and reports a
  distillate, so a wide search never floods the conversation you are reading.
  The Subagents screen shows them working; open one for its instruction and
  result.
- **Cancel early.** The working row tells you what it is doing; if it is on the
  wrong path, `Esc` cancels the task instead of paying for the whole turn.
- **Let it read its own telemetry.** Point `exec` at your test runner and the
  agent verifies its own change; the debug screen shows you the same tool calls
  it made, in order, with timings.

---

## 6. Beyond chat

The same instance that answers you in the TUI can run engineering chores on its
own — this is the part a coding CLI structurally cannot do. Workflows are
durable DAGs with schedule / webhook / signal triggers
([workflows](../rfcs/0027-workflow-dialect-3.md)):

```yaml
workflows:
  - name: nightly-audit
    steps:
      s: { kind: schedule, cron: "0 3 * * *" }
      audit: { kind: agent, depends_on: [s],
               instruction: "Run cargo audit and cargo outdated in /work; summarize what needs attention." }
      gate: { kind: human, question: "Apply the safe upgrades?", depends_on: [audit] }
      apply: { kind: agent, depends_on: [gate],
               instruction: "Apply only the upgrades the operator approved: {{steps.gate.output}}" }
      done:  { kind: finish, depends_on: [apply], status: completed }
```

You wake up to a question in your TUI instead of a stale report — the gate
waited overnight because the run is durable.

---

## 7. Honest limits

- **No token-by-token streaming.** You get live *activity* — phase, current
  tool, elapsed, spend — and then the finished answer, not a typewriter. This
  is deliberate; the reasoning and the alternatives are recorded in
  [RFC 0032 §17/§19](../rfcs/0032-interface-and-observation-plane.md).
- **No built-in editing tools.** §2 is not a formality: you choose the powers,
  and a config that grants none is an agent that can only talk.
- **`exec` is not in release binaries.** Build with `--features exec`, on
  purpose ([security.md](security.md)).
- **MCP servers must speak HTTP(S).** There is no stdio transport, so a
  stdio-only server needs a bridge you run.
- **Loopback is operator.** A local client is fully privileged with no
  credential — correct for your laptop, wrong for a shared host. Bind
  non-loopback and you must configure client auth (mTLS, bearer, or pairing).

---

## See also

- [interface.md](interface.md) — the TUI and web UI in full: screens, the
  composer's `/ @ # $`, pairing, debug mode, chrome configuration.
- [security.md](security.md) — the `exec` fence, the trifecta rule, secrets.
- [use-cases.md](use-cases.md) — the other shapes (triage, audit, fan-out).
- [configuration.md](configuration.md) — every key, and precedence.
