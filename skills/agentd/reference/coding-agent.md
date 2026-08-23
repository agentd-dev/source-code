# Recipe: a coding agent you host

A pair-programming agent you talk to in a terminal — like Claude Code or Codex
CLI, except the session lives in a daemon. The work survives the client, several
surfaces can watch it at once, and the same instance runs scheduled chores.

agentd ships **no file-editing tools of its own**. You choose the powers; a
config that grants none is an agent that can only talk.

## 1. Build (release binaries have no `exec`)

```sh
cargo build -p agentd-cli --release --features a2a,exec
# a2a  → the listener the TUI/web UI attach to
# exec → the guarded local command runner (still default-OFF in config)
```

## 2. Configure

```yaml
config_version: "1"

agent:
  name: coder
  instruction: |
    You are a careful engineer working in the repository at /work.

    Explore before you edit: read the relevant files, run the tests, and say
    what you found. Prefer the smallest change that fixes the cause.

    Ask before you act destructively — anything that rewrites history, force
    pushes, deletes files, or touches credentials. Use ask_human and wait.

    When you finish, state what changed and how you verified it.
  preflight: never            # skip the intent classifier; you are the intent
  ask_human_fallback: wait    # a question with nobody to answer it parks

intelligence:
  endpoints: https://api.openai.com/v1
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"
  budget:
    windows: [{ per: day, tokens: 2000000 }]
    on_exhausted: refuse

store:
  kind: memory                # see §5 — the session dies with the daemon

a2a:
  listen: http://127.0.0.1:8420   # loopback ⇒ you are the operator

interface:
  enabled: true
  debug: false                # flip per-session with /set when you need internals

security:
  exec:
    enabled: true
    workdir: /work                        # every command is confined here
    allow: [git, rg, ls, cat, sed, cargo] # argv[0] allow-list; EMPTY denies all
    timeout: 120s
    max_output: 262144
    env: [PATH, HOME, LANG]               # your secrets never reach the child

limits:
  run: { steps: 60, tokens: 400000, deadline: 30m }

lifecycle:
  run_until: drained          # a daemon: stay up between messages
```

## 3. Run

```sh
agentd --validate-config -c coding.yaml   # always first
export OPENAI_API_KEY=sk-…
agentd tui -c coding.yaml                 # daemon + terminal UI, one command
```

Keep them separate so quitting the UI leaves the agent working:

```sh
agentd -c coding.yaml &
agentd-tui --endpoint http://127.0.0.1:8420
```

## 4. The allow-list ladder

Widen only as the agent earns it:

```yaml
allow: [git, rg, ls, cat]                       # read-only reconnaissance
allow: [git, rg, ls, cat, cargo, npm, pytest]   # + run the tests
allow: [git, rg, ls, cat, sed, tee, cargo]      # + edit files in place
allow: [bash]                                   # everything; deliberate only
```

`exec` runs argv directly — no shell, so no globbing, `$(…)`, pipes or command
injection. Allow-listing `bash` gives all of that back on purpose.

Prefer structured edits or someone else's sandbox? Use an MCP server over HTTPS
for writes and keep `exec` for reconnaissance and tests.

**Trifecta:** `exec` carries `sensitive` + `egress` tags. Add an MCP server
tagged `untrusted_input` (a web fetcher, an issue tracker) and agentd refuses to
start without `--allow-trifecta`. That refusal is the feature.

## 5. Bounding it

- **Cost** — `intelligence.budget` with `on_exhausted: refuse`.
- **Steps/time** — `limits.run.{steps,tokens,deadline}` bound one turn.
- **Context** — long sessions self-compact (`context.compact_at`).
- **Blast radius** — `workdir` + allow-list are the real fence. Model
  cooperation is not a control.
- **Durability** — `store.kind: memory` loses everything on restart, including a
  pending approval. Point `store` at an MCP/HTTP store for a session that
  survives.

## 6. Approvals

When the agent calls `ask_human`, its task flips to `input-required` and renders
as an answerable row in *every* attached client:

```
agent › Ready to force-push feature/rework over 3 commits. Proceed? [reply to continue]
› no — open a PR instead
```

Typing answers the newest gate; `#task-… your answer` answers a specific one.
Workflow `human` steps gate the same way and survive a daemon restart.

## 7. Practices

- **One instance per repository** — `workdir` is the fence; two repos in one
  instance means it protects neither.
- **The instruction is standing policy**, not today's task.
- **Start read-only** for a day.
- **`/pause` before you edit the tree yourself**; `/resume` after. Nothing is
  lost, unlike a cancel.
- **Delegate exploration to subagents** — they run in their own context and
  report a distillate, so a wide search never floods your conversation.
- **Watch from a second surface** — `/pair` gives a rotating code, so no bearer
  needs copying to another machine.

## 8. Honest limits

- No token-by-token streaming: you get live *activity* (phase, tool, elapsed,
  spend) and then the finished answer.
- No built-in editing tools — §4 is the whole story.
- `exec` is not in release binaries, on purpose.
- MCP servers must speak HTTP(S); no stdio transport.
- Loopback is operator: correct for a laptop, wrong for a shared host.
