# RFC 0038: The system-prompt template

**Status:** Implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-24
**Part of:** the context surface (RFC 0026 §contexts); renders the environment RFCs 0035 (streams), 0036 (subagent templates), 0037 (services) expose; expressions reuse the CEL seam of RFC 0027.

---

## 1. Summary

An agent's system prompt is the most load-bearing text in the product and was
the least configurable thing in it: assembled in Rust, section by section,
with a fixed list of on/off names. This RFC replaces that with **data plus a
template**.

The runtime exposes what it knows — instance, instruction, workflows,
services, streams, subagent templates, skills, peers, parked signals, memory
keys — and a small template language renders it:

```text
{{ expr }}                              interpolate
{{#if expr}} … {{else}} … {{/if}}
{{#each expr}} … {{/each}}              `this` is the element, `@index` its position
{{! comment }}
```

Two rules keep it small. **Expressions resolve as a path first and CEL
second**: `{{instance}}` and `{{#each services}}` are bare lookups that work
in any build, and only a real expression (`take(services, 16)`,
`size(peers) > 0`) needs `--features cel`. And **the built-in default is
written in this language** — `agentd --context-template` prints it — so an
override starts as a copy rather than a guess, and there is no privileged
formatting the operator cannot reach.

The section-selection list this replaces (`context.cards`) is removed.

## 2. Motivation

Three pressures.

**Operators need the prompt to say something else.** Wording, ordering,
what appears at all, house framing — none of it was reachable, and the
`cards` list only answered "which sections", never "how". A prompt override
is the most requested knob in every agent runtime for a reason: the built-in
text is always slightly wrong for somebody's deployment.

**The prompt is a cache-economics decision.** Providers cache on the literal
prefix of a request (Anthropic's prompt caching, OpenAI's automatic prefix
caching), so a section that changes between turns invalidates the cache for
**everything after it**. That makes section ORDER a cost decision, not a
taste one — and the old fixed order was wrong: parked signals (which change
whenever a run parks or resolves) rendered *before* peers and subagent
templates (which change only on reload), so ordinary coordination traffic
was invalidating configuration-derived text on every turn.

**The built-in text was drifting from reality.** The persona line recited a
hardcoded internal-tool list, so an instance that narrowed
`agent.tools.internal` still told the model it had `subagent.*` — briefing it
on tools it would then be refused. Data-plus-template fixes the class of bug:
what the prompt claims is derived from what the registry grants.

## 3. Decision

1. **The system prompt is a template over environment data.** The runtime
   builds a data map per turn (§4); a template renders it (§5). No section is
   privileged: the persona line, the instruction and every environment block
   are template text.
2. **The default template ships in the binary, in the same language**, and is
   printed by `agentd --context-template`. It uses **bare paths only** — no
   CEL — because a build without `--features cel` must still render a system
   prompt. List caps and joined text therefore live in the DATA (`tags_text`,
   `tools_text`, `params_text`, `internal_text`; lists capped at 16, peers at
   24); CEL is for authors who want more. A unit test asserts the shipped
   default stays CEL-free, because the failure mode is silent.
3. **The default is ordered by volatility** (§6): persona and instruction,
   then configuration-derived sections, then live state. Custom templates may
   order however they like and pay their own cache cost.
4. **Expressions are a path first, CEL second.** A bare path never needs the
   `cel` feature; anything else is refused at CONFIG LOAD with the feature
   message on a build without it — never mis-rendered at turn time.
5. **Two CEL helpers are registered** on every evaluation context: `take(list,
   n)` (CEL has no slicing) and `join(list, sep)` (CEL has no join). They are
   available to workflow expressions too — a `take` in a `filter:` is the same
   idea as a `take` in a prompt.
6. **Templates are named and selectable per node.** `context.template` is the
   instance default; `context.templates.<name>` are alternates a step selects
   with `context: {template: <name>}` — an extraction step can drop the whole
   environment without inlining a template.
7. **Parsing happens at config load, not at turn time.** A malformed block,
   an unknown block tag, or a reference to a name the runtime does not export
   refuses startup naming the problem. Rendering is memoized per source text.
8. **A template that never references `{{instruction}}` warns on every boot.**
   It stays legal — some deployments really do want a bare prompt — but
   silently losing standing policy is the failure that still looks like a
   working agent.
9. **Rendering never fails a turn, and never yields an empty prompt.** A
   render error logs `prompt.render.fail` and falls back to the built-in; if
   even that fails it falls back further to persona + instruction assembled
   without the engine. An agent whose system prompt silently vanished still
   looks like it is working and answers without its standing policy, so the
   floor is non-negotiable. A missing path renders empty (an instance with no
   peers yet is normal, not an error).
10. **`context.cards` is removed**, not deprecated: it shipped hours before
    this RFC in v1.0.0 with no users, and carrying two selection mechanisms
    would be worse than the break.
11. **Compaction gains the same treatment at its own scale** (§7):
    `context.summarize.prompt` overrides the summarizer's guidance and
    `context.summarize.model` runs it on a cheaper model. The summary's JSON
    schema is NOT overridable — it is parsed back into the context.

## 4. The data

| Name | Shape | Volatility |
|---|---|---|
| `instance` | string | static |
| `instruction` | string (the cleaned instruction document) | reload |
| `extra` | string — the per-turn slot (step skills, knowledge block) | per turn |
| `tools` | `{internal, internal_text}` — the granted internal-tool families, from the registry | reload |
| `workflows` | `[{name, description}]` | reload |
| `services` | `[{name, kind, tags, tags_text, tools, tools_text, rate}]` — never credentials | reload |
| `egress_closed` | bool | reload |
| `streams` | list of names | reload |
| `templates` | `[{name, tier, params, params_text}]` | reload |
| `skills` | pre-rendered catalogue + this context's loaded bodies | per context |
| `peers` | `[{name, note}]` — configured peers **and** live instance children | live |
| `signals` | `{waiting: [{name, run, step}], recent: [name], any: bool}` | live |
| `memory` | `{keys: [name], keys_text}` | live |

Every list ships with a `_text` twin (the joined form) and is capped (16;
peers 24), so the default template needs no expressions at all — see §3.2.

Credentials are absent by construction: the services entry carries names,
tags, tool ceilings and pacing, never tokens.

## 5. The language

Deliberately two block forms and interpolation — everything else is CEL's
job. `{{#each}}` iterates a list (an object yields its values, which is what
"each server" means); inside a block `this` is the element and `@index` its
position. Truthiness extends CEL's with emptiness, so `{{#if services}}`
means "non-empty" — the check an author actually wants.

Guards, all fail-closed at load: unbalanced blocks, unknown block tags,
closing tags with no opening block, empty `{{}}`, nesting past 8 levels, and
unknown root references. At render, output is capped (256 KiB) so a runaway
`{{#each}}` cannot blow the context window or the bill on its own.

## 6. Ordering is the cache contract

The default renders, in order: persona → instruction → per-turn extra →
workflows → services → streams → subagent templates → skills → peers →
signals → memory. The first group changes only on reload; the last three
change turn to turn. This is the property to preserve when overriding, and
the one thing a custom template can silently cost you — hence the ordering
note in the docs and the e2e test that asserts the shipped default holds it.

## 7. Compaction

`plan_compaction` keeps its fold rules and its schema; only the *guidance*
and the *model* become configurable. An override that asks for a different
output shape produces a schema refusal and the existing fallback path, which
is why the schema is documented as fixed rather than exposed.

## 8. Alternatives considered

- **Keep `cards`, add ordering.** Cheapest, but never answers "how does a
  section read", which is most of the ask.
- **Inline `{{CEL: …}}` in prose.** Works, but puts logic in the middle of
  text and makes CEL a hard dependency for templating. Rejected in favour of
  block forms with path-first resolution.
- **A full template engine (Handlebars, Tera).** A dependency the moat
  forbids, and a language to own forever. The block grammar here is ~200
  lines because CEL does the hard half.
- **Cards as JSON blobs.** Fewer knobs, but loses the framing sentence that
  makes a section land, and reads worse to a model than prose.

## 9. Cross-references

RFC 0026 (contexts, turns) · RFC 0027 (CEL seam, template engine) ·
RFC 0034 (instruction documents — the `instruction` this renders) ·
RFC 0035/0036/0037 (the streams, subagent templates and services the
environment data exposes).
