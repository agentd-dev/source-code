# RFC 0033: The file store and instance identity

**Status:** Implemented (2.2 track)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-18
**Part of:** the durable-agent design; adds an adapter to RFC 0025 §4 and a `store.kind` to RFC 0030 §3.5.

---

## 1. Summary

agentd has three store adapters — `mcp`, `http`, `memory` — and refuses to
start a **long-lived** instance without one of the first two. So the first
`schedule` a new user writes exits `2` and asks them to stand up a coordination
backend before they have run anything. That is the wrong order: durability
should be the default a laptop already satisfies, and a shared backend should be
the thing you graduate to.

This RFC adds a fourth adapter, **`file`**, and makes it the default for
long-lived instances. It also names the thing that has been implicit until now:
an instance's **identity**, the key under which its state is found again after a
restart.

## 2. Why a local file store does not break the model

agentd "runs no code of its own": there is no `fs` tool, no shell, no local
execution the model can reach. That is a statement about **the agent's tools and
the trust boundary**, not a claim that the process avoids disk. agentd already
writes credentials (`auth/cache.rs`, `0600`, under the XDG state chain), AAuth
keys, and cgroup files.

A store is the runtime's own ledger. It is unreachable by the model, it holds no
capability, and it sits on the same side of the boundary as the credential
cache. The rule it must not break is the one that matters: **no tool the model
can call may touch it.** A file adapter satisfies that by construction — it is
not a tool.

## 3. Identity: what keys the state

Keys are already `<prefix>/<instance>/<kind>/<id>` (RFC 0025 §3.1), where
`instance` comes from `agent.name` (or the downward-API identity, RFC 0015 §6).
The file adapter uses the same key; identity is therefore **unchanged**, and a
config that moves from `file` to `mcp` keeps its keys.

### 3.1 Identity is `agent.name`, not a hash of the config

The obvious alternative — derive the directory from a hash of the configuration
— is rejected, and the reason generalises: **an identity key must be stable
across the changes you expect, and change only when you intend a different
instance.**

A config hash inverts that. Adding an MCP server, raising a limit, or fixing a
typo in the instruction all change the hash. The agent then starts fresh,
abandons its in-flight durable workflows, and orphans the previous state
forever. The failure is silent — a `restore.fresh` and a new directory, no error
— which is precisely the outcome durability exists to prevent, triggered by the
most ordinary edit a user makes. It also makes state undiscoverable: a directory
of hex names that cannot be mapped back to an agent.

`agent.name` is stable across edits, meaningful to a human, already in the
schema, and changing it is an unambiguous statement of intent.

### 3.2 Resume-by-id is an override, not the mechanism

A session id passed on the command line (`--resume <id>`) cannot be the primary
mechanism: when a Kubernetes Deployment restarts a pod, **nothing carries the id
from the previous life.** Identity must be derivable from configuration and
environment alone, or the flagship deployment mode silently starts fresh on
every restart.

It is a good *override*, and half of it already exists — the manifest carries a
`generation` counter, bumped on every restore (RFC 0025 §6). This RFC exposes
it: `--fresh` starts a new generation without resuming, and the generation is
reported at startup so an operator can see which life they are in.

### 3.3 The config hash, used correctly

The hash is kept — as a **signal, not a key.** The manifest records a digest of
the settings that shaped the durable state (workflows, store, limits). On
restore, a differing digest logs `store.config_changed` naming what moved. State
is still resumed; the operator is told. This is the safety the hash idea was
reaching for, without letting a routine edit orphan a running workflow.

## 4. The adapter

```yaml
store:
  kind: file            # the fourth adapter
  file:
    path: /var/lib/agentd   # optional; default below
```

**Root directory**, first that applies:

1. `store.file.path`
2. `$AGENTD_STATE_DIR`
3. `$XDG_STATE_HOME/agentd/state`
4. `$HOME/.local/state/agentd/state`
5. the OS temp dir (last resort; logged as non-durable)

The same chain `auth/cache.rs` already uses, so an operator learns it once.

**Layout.** One file per key: `<root>/<prefix>/<instance>/<kind>/<id>.json`.
Every path segment is percent-encoded, and a segment that decodes to `.`, `..`
or contains a separator is refused — ids reach this adapter from run ids and
task ids, so traversal is closed at the adapter, not assumed away upstream.

**Writes are atomic**: serialise to `<file>.tmp.<pid>`, `fsync`, `rename` over
the target, then `fsync` the parent directory. A crash therefore leaves either
the previous envelope or the new one, never a partial. Directories are `0700`
and files `0600`: the state holds conversation content and tool results.

**CAS** (`put` must reject a `seq` that does not exceed the stored one) is
performed against the file's current contents under the instance lock (§4.1).

**`list`** walks the prefix directory; **`delete`** unlinks. Both are supported,
so the file store is the only adapter that implements the contract completely.

### 4.1 Single writer, enforced

Two processes sharing one directory would silently interleave writes; `mcp` and
`http` are designed for sharing, a directory is not. On open the adapter takes
an **exclusive `flock` on `<root>/.lock`** and fails with a clear message
naming the holder's pid if it is held:

```
store.file: /var/lib/agentd is locked by pid 4131 — another agentd is using
this state directory; give this instance its own agent.name or store.file.path
```

Failing fast is the whole point: a fleet that needs shared state needs `mcp` or
`http`, and finding that out at startup is much cheaper than finding it out from
corrupted runs.

## 5. Defaults

| instance shape | before | after |
|---|---|---|
| one-shot (no long-lived start node, no listener, no goal) | `none` | `none` — unchanged |
| long-lived (`loop`/`schedule`/`subscribe`/`signal`/`event`/`a2a`/`webhook` start, an `a2a.listen`/`webhooks.listen`, or a `goal`) | **exit 2** | `file` |

`store.kind` set explicitly always wins; this changes only what happens when it
is absent. The one-shot default is deliberately untouched: a one-shot run that
starts writing state to disk would surprise every existing user of it.

### 5.1 Honesty about what it guarantees

Durability is a property of the filesystem the directory is on, not of agentd. A
file store on a mounted volume survives anything; on a container's writable
layer it survives a process restart but **not** a reschedule. A store that
implies more than it delivers is worse than the current refusal, because the
refusal at least fails honestly.

So a defaulted file store logs, once, at startup:

```json
{"event":"store.file","path":"/var/lib/agentd/…","generation":3,
 "msg":"durable state is on the local filesystem; it survives a restart of this
 process but not a move to another host — use store.kind mcp|http for a fleet"}
```

`--capabilities` reports the same, and `docs/deployment.md` gains the volume
guidance for the container case.

## 6. Observability

- `store.file` at startup: path, generation, whether the path was defaulted.
- `store.config_changed` on a manifest digest mismatch, naming the sections.
- `agent_store_ops_total{op,kind="file"}` and the existing latency histogram
  gain `kind="file"`; no new metric names.

## 7. Security

The directory is `0700` and files `0600`. State is not encrypted at rest: it is
readable by the user the daemon runs as, exactly like the credential cache, and
an operator who needs more should point `store.file.path` at an encrypted
volume. No tool the model can call reaches the adapter. Path traversal is closed
at the adapter (§4). The lock file prevents two instances from interleaving.

## 8. Test plan

1. **Round trip**: put/get/list/delete against the trait, including a `seq`
   conflict.
2. **Atomicity**: a write interrupted between `tmp` and `rename` leaves the
   previous envelope intact and parseable.
3. **Traversal**: an id of `../../etc/passwd`, an absolute id, and an id with a
   NUL are refused, and nothing is written outside the root.
4. **Lock**: a second adapter on the same root fails with the pid message.
5. **Resume**: a long-lived instance with a `schedule` runs, is killed, restarts,
   and resumes its state with `generation` incremented — the property the whole
   RFC exists for.
6. **Default**: a long-lived config with no `store` starts (it used to exit 2)
   and logs `store.file`.
7. **One-shot is unchanged**: a one-shot config with no `store` writes nothing.
8. **Digest**: editing a workflow and restarting logs `store.config_changed` and
   still resumes.

## 9. What this does not do

- It is not a fleet store. Two replicas need `mcp` or `http`; §4.1 makes that a
  startup error rather than a corruption.
- It does not encrypt at rest (§7).
- It does not add history: like the other adapters it keeps the latest envelope
  per key, and `get(seq)` returns the latest.
