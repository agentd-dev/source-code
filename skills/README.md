# Agent Skills

Skills that teach an **AI coding assistant** (Claude Code and friends) how to
work with agentd — how to install it, write and validate a config, wire tools,
attach the TUI, and read a non-zero exit code.

> **Not to be confused with agentd's own `skills:` config section.** Those are
> instruction bundles the *agent* loads at runtime, and the stock binary
> discovers them from **MCP servers** — there are deliberately no local skill
> directories in agentd itself (RFC 0028 §7). This directory is for the tools
> that help you build with agentd, not for agentd.

| skill | use it when |
|---|---|
| [`agentd/`](agentd/SKILL.md) | installing, configuring, running or debugging agentd; building a coding agent on it |

## Installing

Copy (or symlink) the skill into your assistant's skills directory — for Claude
Code that is `~/.claude/skills/` for personal use, or `.claude/skills/` in a
repository to share it with everyone working there:

```sh
cp -r skills/agentd ~/.claude/skills/
```

The assistant reads the frontmatter `description` to decide when the skill is
relevant, then loads `SKILL.md`; the files under `reference/` are pulled in only
when they are actually needed.

## Editing them

Keep `SKILL.md` short and put detail in `reference/` — the body costs context
every time the skill triggers. State facts that are checkable against the repo
(a flag, a config path, an exit code) rather than advice that ages, and verify
each one before writing it down: a skill that confidently teaches a key that
does not exist is worse than no skill.
