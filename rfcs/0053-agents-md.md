# RFC 0053: AGENTS.md — a workspace's notes for agents, in the prompt as text

**Status:** Accepted 2026-10-19, as track G of RFC 0048 (§5.5); implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-10-19
**Builds on:** RFC 0038 (the system-prompt template, ordered stable to volatile), RFC 0042 (what a served document may set), RFC 0048 §5.5
**Standards:** AGENTS.md, [agents.md](https://agents.md) and [agentsmd/agents.md at `d001185d79`](https://github.com/agentsmd/agents.md/tree/d001185d79) (2026-09-10, MIT, stewarded by the Agentic AI Foundation under the Linux Foundation), cited as [A]. It has no formal specification and no version: an `AGENTS.md` is "just standard Markdown" with no required fields; in a monorepo "the closest AGENTS.md to the edited file wins"; and explicit user chat prompts override everything.

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

Many repositories keep notes for coding agents in an `AGENTS.md` at their
root, and more in subdirectories: how to build and test, conventions, what
not to touch [A]. agentd did not read them. Now an operator names a
workspace, and agentd reads its `AGENTS.md` files — the root's, then each
one on the way down to agentd's working directory — into the system prompt
as text, below the instruction, nearest last.

They grant nothing. An `AGENTS.md` is plain text the model reads; it is
never an instruction document, so it configures no tool, service or policy,
and a `:::!` block inside it is a line of text.

## 2. Design decisions

**D1. `agent.workspace`, a directory.** The key names the workspace the
agent works in: the root `AGENTS.md` files are read from. It sits under
`agent`, beside `instruction` and `prompt`, the other texts the operator
gives the agent, and not under `context`: RFC 0042 lets a served document
write the whole `context` section, and a document must not choose which
directory of the host becomes prompt text. `agent.workspace` is
operator-only (`OPERATOR_ONLY`, beside `agent.prompt`, by the same
argument) and reloadable (`RELOADABLE_PATHS`): the files are read at load,
which every reload runs again, and the prompt is rendered from the live
settings at each turn. A relative path is taken from agentd's working
directory.

**D2. Root down to the working directory, nearest last.** agentd edits no
files, so [A]'s "the closest AGENTS.md to the edited file" has no file to
measure from; the directory agentd works in stands for it. That is its
process's working directory — where it was started, a container's
`WORKDIR`, a unit's `WorkingDirectory=` — the convention coding agents use
for `AGENTS.md`. When it lies inside the workspace, agentd reads the
`AGENTS.md` of the workspace's root, then of each directory below it down
to the working directory, in that order; otherwise, the root's alone.
Started in `services/billing`, it reads `AGENTS.md`,
`services/AGENTS.md`, `services/billing/AGENTS.md`. A sibling's, or one
below the working directory, is not read. Both directories are taken with
their symlinks resolved, so every directory walked is inside the workspace.
`security.exec.workdir` is not the working directory: it confines a
command, and is off in the release binaries.

**D3. Delivered as instruction text, stable, below the instruction.** The
template data gains `agents_md`, the list of `{path, text}`, root first,
each `path` taken from the workspace's root. The built-in template renders
it as `## AGENTS.md` directly after `## Instruction` and before anything a
turn changes, since the files change only on reload and providers cache
the prompt's literal prefix (RFC 0038). Each file comes under `### <path>`,
after a line telling the model what they are: the workspace's notes,
nearest last, which wins between them; below the instruction, which wins
over them; and below what the user asks in the conversation [A]. They are
never above the instruction. A custom template that never names
`agents_md` while a workspace is set loads with a warning, as one that
drops `instruction` does.

**D4. Text, granting nothing.** The files are read by `config::agents_md`,
which returns their text, and nothing passes them to the instruction parser:
a `:::!config` or `:::!workflow` block in one reaches the model as
written and defines nothing. RFC 0042's boundary needs no new rule, because
AGENTS.md never becomes a document.

**D5. Limits.** UTF-8 only, a leading byte-order mark dropped; at most
32 KiB (32768 bytes) across the files together, counted as they are on disk
before any text is decoded, each read no further than one byte past what
the cap leaves, so a file past it is refused as too large and never cut; a
file that holds only whitespace adds nothing. A symlinked
`AGENTS.md` is followed only to a file inside the workspace, and what is
not a regular file is not opened. A file that breaks a rule refuses the
load — the start, exit `2`, or the reload, which keeps the running prompt —
naming `agent.workspace` and the file. A workspace that is not a directory
is refused the same way.

**D6. Reload and watch.** A reload reads the files again; one that changes
them reports `agent.workspace` in `config.reloaded`. With
`lifecycle.watch_config`, each directory on the way down is watched for
`AGENTS.md` by name, so one added, edited or replaced there triggers a
reload.

**D7. Out of scope.** The instance's own turns carry the files; a subagent
template's child is another agent with its own instruction, and a node's
`system:` replaces the prompt. Gating a workspace by trust (agents.md's
advice for untrusted repositories) is the operator's choice of what to name;
the guide says to name only a repository whose notes the operator would put
in the instruction.

## 3. Configuration

```yaml
agent:
  instruction: ./coder.md
  workspace: /srv/repo          # its AGENTS.md files reach every turn
```

Docs: [docs/coding-agent.md](../docs/coding-agent.md#the-repositorys-agentsmd)
(the guide), docs/configuration.md (`agent.workspace`, the `agents_md`
data, the reload table), docs/interoperability.md (the documents table).

## 4. Exit criteria

- Unit tests (`config::agents_md`): the order — root first, each directory
  down to the working directory, nearest last — with a sibling and a
  deeper file left out and a working directory outside the workspace
  reading the root's alone; a working directory reached through a symlink;
  the cap at exactly 32768 bytes and one past it, a file larger than the
  cap read no further, and one past it refused as too large whether the
  cap cuts a multibyte character or falls in trailing whitespace; the
  byte-order mark; a file that is not UTF-8, a
  directory named `AGENTS.md`, a symlink out of the workspace refused and
  one inside it followed; a workspace that is missing or a file. The
  built-in template orders `## AGENTS.md` after the instruction and before
  a turn's extra text.
- End to end (`crates/agentd-cli/tests/agents_md_e2e.rs`) against the mock
  model that answers with its system prompt: a turn started in
  `services/billing` carries the three files under their paths, in order,
  below the instruction, and a `:::!workflow` block in one as text with no
  workflow of its name; started outside the workspace, the root's alone; a
  file that is not UTF-8 refuses the start; a custom template without
  `agents_md` warns; with `hot-reload`, a changed file is a reload that
  changes `agent.workspace` and a broken one a refused reload.
- The strip lists AGENTS.md (no version), linking to the guide's section.
