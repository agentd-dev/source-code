# RFC 0034: Instruction documents and directives

**Status:** Implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-23
**Part of:** the configuration surface (RFC 0017, RFC 0030 §3); adds a workflow source to RFC 0027 and a skill source to RFC 0028 §7; retirement extends RFC 0027 §9.

---

## 1. Summary

`agent.instruction` stops being an opaque string and becomes a specified
format: the **instruction document** — Markdown-shaped prose that MAY embed
**directives** in the colon-fence container syntax the ecosystem (MyST,
remark-directive, ChatGPT) has converged on:

```text
:::<type>{<attributes>}
<body>
:::
```

A directive carries machinery in the document that describes it: a
`:::workflow` body joins `workflows:` as if written there, a `:::skill` body
joins the skills catalogue with no server involved, `:::context` /
`:::example` give text a stated role. The model reads the **cleaned** text —
each machinery block replaced by a one-line note — so prose and machinery
cannot double-speak.

Two normative rules carry the design. **Directives execute by surface, not
by content** (§4): only operator-authored instruction text is extracted;
conversation text never is, unconditionally. And **directives are sugar over
existing pipelines, never a parallel mechanism** (§5): an embedded workflow
is validated, var-folded, hashed, pinned, reloaded and retired by exactly the
machinery that handles a config-file workflow, so the two cannot drift.

This RFC also specifies **retirement** (§7): the single exit path every
workflow definition takes when it is removed, replaced, or deleted — with a
per-workflow `unload:` policy — because instruction editing made "a
definition left" a routine event that had to stop being three inconsistent
behaviors.

## 2. Motivation

An agent is prose plus machinery, and they belong in one reviewable,
deployable, reloadable document. Separately: skills required an MCP server
even for three sentences of house style, and the definition-removal paths
were inconsistent to the point of defect — `workflow.delete` stranded its own
live runs; a reload leaked pinned definitions and orphaned MCP subscriptions;
a *failed* reload emptied the registry. Formalizing arrival forced
formalizing departure.

## 3. The grammar (normative)

An instruction document is UTF-8 text, interpreted line by line.

- **Opening fence.** A line whose column 0 is `:{3,}` immediately followed by
  a name matching `[A-Za-z0-9][A-Za-z0-9._-]*`, optionally followed by
  `{attributes}`, optionally followed by trailing whitespace. Anything else —
  an indented fence, a `:::` mid-line, a bare colon run — is prose.
- **Closing fence.** The next line consisting solely of `:{n,}` colons, where
  `n` is the opening run's length. A longer inner run therefore does NOT
  close an outer block: **nesting is by giving the outer fence more colons**,
  and a `::::context` block can quote a literal `:::workflow` verbatim.
- **Body.** Every line between the fences, verbatim. Its interpretation
  belongs to the directive.
- **Attributes.** Inside `{…}`: `key=value` (value runs to whitespace),
  `key="…"` with `\"` and `\\` escapes, or a bare `flag` (≡ `flag="true"`).
  Keys match `[A-Za-z0-9][A-Za-z0-9._-]*`.
- **Failure is loud.** An unknown directive name, an unclosed fence, a
  malformed attribute list, or a body that fails its directive's parser MUST
  refuse startup (exit `2`, RFC 0011 §5) naming the line and — for unknown
  names — the known set. A document that needs a literal column-0 fence
  indents it.

The grammar is deliberately the *container subset* of MyST: no roles, no
`:key:` option lines, no argument after the name. Compatibility note: any
plain Markdown instruction — including the `AGENTS.md`-style documents other
tools consume — is a valid instruction document *if* it contains no column-0
colon fences; the format is a strict superset of prose.

## 4. Surfaces and trust (normative)

Extraction runs on `agent.instruction` when it arrives as config: inline in a
file, via `--instruction` / `--instruction-file`, or through env layering.

- Conversation text — A2A messages, chat, tool results — is **never**
  extracted. Not configurable. Executing definitions out of the untrusted
  channel is prompt injection as a feature, and no flag should exist whose
  misuse is that.
- URI-fetched instructions (RFC 0028 §3) and skill bodies are **inert** in
  this version: fences in them reach the model as prose. A future opt-in MAY
  extend extraction to them, in trust order, with workflow-defining
  directives still answering to `security.workflows.immutable` — a skill
  server updating overnight must not rewrite a locked agent's standing
  orders.
- `security.workflows.immutable` does not block instruction-embedded
  workflows: the instruction IS operator config, which is exactly what the
  lock tells the model to go edit.

## 5. The directive registry

| name | body | effect |
|---|---|---|
| `workflow` | a dialect-3 YAML document (RFC 0027) | appended to `workflows:` before validation — var folding, fail-closed validation, content hashing, run pinning, retirement, all identical to an inline entry. Attributes: `name` overrides the body's, `armed=false` loads disarmed. Cleaned text: `[workflow "<name>" is loaded and runs autonomously]`. A directive-carrying instruction generates **no sugar `main` workflow** (RFC 0030 §5) — it declared its machinery. |
| `skill` | Markdown | an **inline skill** (RFC 0028 §7): catalogued with `source.kind = inline`, body on the entry, no server round-trip. Attributes: `name` (required), `description`, `when`. Referenced as `@skill:<name>`; inline wins a name collision with a discovered skill. Cleaned text: `[skill "<name>" is available — reference it as @skill:<name>]`. |
| `context` | text | kept in the cleaned document, wrapped `<reference title="…">…</reference>` — material that is true rather than imperative. |
| `example` | text | kept, wrapped `<example>…</example>` — what good output looks like. |

Names are a **closed set**; extending it is a spec change. Reserved for
future registration, in intended order: `approval` (HITL policy, RFC 0032
§14 adjacency), `memory` (idempotent durable-memory seeding), `schedule`
(sugar over a `schedule`-start workflow).

## 6. Reload semantics

The instruction is in the hot-reloadable partition (RFC 0030 §6); extraction
runs on every load. On reload, each embedded workflow diffs **by content
hash** against the running registry: unchanged is a no-op; changed arms the
new version (new runs use it) and retires the old (§7); removed retires.
Inline skills re-enter the catalogue; a changed body has a new hash and
contexts load the new text on next reference. `--validate-config` runs the
same extraction, so a broken directive is caught with every other config
error, before anything runs.

## 7. Retirement (normative, all workflows)

Every definition leaves through one path, whatever removed it — a reload
that dropped or changed it, `workflow.delete`, or an instruction edit:

1. Starts are disarmed; MCP resource subscriptions are released unless
   another armed workflow subscribes the same `(server, uri)`.
2. The definition is pinned for its live runs (they execute the hash they
   started with, RFC 0027 §9).
3. New runs stop being admitted.
4. The definition's own policy applies —
   `unload: {policy: drain | cancel | detach, timeout}`, default `drain`:
   live runs finish, bounded by `timeout` (then cancelled); `cancel` cancels
   now; `detach` pins and forgets.
5. The pin is garbage-collected when its last run reaches a terminal status.
   Telemetry: `workflow.retiring` (policy, live-run count) at the start,
   `workflow.unloaded` at the end.

Pins are process state. Across a restart, a restored run whose definition
changed on disk meets `resume_policy` (RFC 0027 §9), not the pin — restart
already had a contract, and retirement does not widen it.

## 8. What this is not

Not a template language (directives declare; `{{config.*}}` and CEL compute
where they already do). Not full MyST. Not an escape from review — a
directive-carried workflow ships in the same file, diffs in the same review,
and answers to the same locks as everything else the operator deploys.

## 9. Compatibility

Documents without column-0 colon fences are unaffected — the extraction gate
is a single line scan. Existing configs, `AGENTS.md`-style prose documents,
and every instruction written before this RFC parse as pure prose. The
`Settings` surface gains no new config key: extraction output (`workflows:`
entries, the inline-skill set, the cleaned text) is derived state.
