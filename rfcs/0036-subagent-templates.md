# RFC 0036: Subagent templates and instance-tier children

**Status:** Draft
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-23
**Part of:** the subagent process model (RFC 0009, RFC 0003, RFC 0026 §6); instruction documents (RFC 0034) become the definition carrier; A2A wiring rides RFC 0029 and the unix-socket listener (2.5.0); retirement extends RFC 0034 §7.

---

## 1. Summary

agentd has a strict hierarchy of statefulness: a **subagent** is a flat
ReAct worker (one instruction, one result, no orchestration self-tools), a
**workflow** coordinates steps inside an instance, and an **instance** is
the only thing that owns workflows, signals, webhooks, schedules and
streams. Today the last tier is static — instances exist because an
operator started a process. There is no way for a running agent to bring up
"a desk": a supervised peer with its own workflows and event surface,
provisioned when an incident opens or a deal starts, retired when it
closes.

This RFC adds that capability without adding a new concept. A new
`subagents:` configuration section declares **templates** — named,
operator-authored definitions whose body is a full **RFC 0034 instruction
document**:

```yaml
subagents:
  templates:
    incident-room:
      instruction: |
        You are the war room for incident {{params.incident_id}} …
        :::workflow
        …
        :::
      params:
        incident_id: {type: string, required: true}
      ttl: 3d
```

One resolution rule gives the section two tiers with one surface. A
template whose instruction carries **no config-defining directives** spawns
today's flat worker — same chokepoint, same caps, same distilled result. A
template whose instruction defines machinery (`:::workflow`, `:::mcp`,
`:::stream`, `:::config`, `:::tools`) spawns an **instance-tier child**: a
full reactor under the supervisor tree, with its own workflows, signal
parks, schedules, streams and durable store, wired to the parent as an A2A
peer over a unix socket, retired by `ttl`/`until` through the graceful
retirement path.

The load-bearing security decision: **the model never authors a child's
definition — it instantiates a template the operator declared, filling only
the declared `params` holes.** A model that can write configuration can
grant itself tools, which turns every prompt injection into a privilege
escalation; templates close that by construction (§8).

## 2. Motivation

Three pressures, one gap.

**The org chart wants to breathe.** The eleven-desk company in
[`examples/startup/`](../examples/startup) is static by design — every desk
is a reviewed file. But real operations have *episodic* roles: an incident
war room that exists for the life of one incident, an onboarding desk per
enterprise customer, a deal room per qualified lead. Modeling these as
permanently-running instances wastes budget and blurs audit trails;
modeling them as flat subagents fails the moment the role needs to park on
a signal for three days or consume a stream — a flat worker has none of
that machinery, deliberately (`NoSelfTools`, RFC 0009: *delegation is the
reactor's job, not a child's*).

**The definition already exists.** RFC 0034 made a single instruction
document a complete instance: prose plus `:::workflow`, `:::mcp`,
`:::stream`, `:::config`. What is missing is not a format but a *spawner* —
today the only consumer of an instruction document is `agentd -c` at the
process manager's hand.

**The spawn surface deserves an operator home.** Subagent behavior is
currently shaped only by per-call step fields and `limits.subagents.*`
caps. There is no durable place to declare "this is what a research worker
in this company looks like" — every workflow that spawns one restates the
instruction, the server grants, the limits. Templates give the operator one
reviewable definition and give the model one narrow verb.

What this RFC deliberately does **not** do is widen the flat subagent. The
flat tier's invariant — no in-child orchestration, one loop, one result —
is what makes it cheap to reason about and safe to spawn at depth. The new
capability lives in a new tier, behind the same chokepoint.

## 3. Decision

1. **A new top-level `subagents:` section** holds `templates:` (named
   definitions), `defaults:` (settings applied to every spawn), and
   section policy (`allow_freeform`). Caps stay in `limits.subagents`,
   which gains an `instances:` sub-object.
2. **A template's `instruction:` is a full RFC 0034 instruction document.**
   Directive extraction runs **once, at parent boot**, on the
   operator-authored template text. The extracted machinery is frozen per
   template; `params` values fold in at spawn as *data* through the
   existing template engine and are never re-parsed for directives (§8.2).
3. **One resolution rule, two tiers.** No config-defining directives →
   **flat tier**: the existing RFC 0009 worker, unchanged semantics.
   Config-defining directives present → **instance tier**: a full reactor
   child. Validation states each template's resolved tier at boot;
   `agentd validate` prints it.
4. **Instantiation is by name + params only.** The `subagent` step and the
   `subagent.run` tool gain `template` and `params`; `params` are validated
   against the declared schema and a mismatch is a synchronous refusal
   naming the field (the RFC 0029 §command-schema discipline). Free-form
   `instruction:` spawns remain possible **only** for the flat tier, and
   only while `subagents.allow_freeform` is true (default). There is no
   free-form instance spawn, ever.
5. **An instance-tier child is the same binary re-exec'd in instance mode**
   (extending RFC 0009's same-binary rule), handed a *composed, structured
   settings payload* — never a document to re-parse. It owns a durable
   store under the parent's state directory, boots through the full
   validation path including the lethal-trifecta gate, and is supervised,
   reaped and restart-governed like any child (RFC 0003).
6. **Wiring is A2A over a unix socket, both ways, automatically.** The
   parent allocates the socket, registers the child as a peer named by its
   handle (aliased to the template name for `singleton: true` templates),
   and injects itself as the child's peer `parent`. Typed commands and
   conversations work in both directions from the first tick.
7. **Instance children have no public listeners in Phase A.** Template
   machinery declaring `webhook` starts, a `webhooks:` listener, or an
   `interface:`/`a2a:` TCP listener is refused at parent boot. External
   events enter through the parent's static, HMAC-verified routes and are
   forwarded as typed commands or signals.
8. **Lifecycle is declarative.** `ttl:` (duration) and `until:` (a signal
   name, templated over params) retire the child through the RFC 0034 §7
   graceful path: stop admitting starts, drain live runs bounded by
   `lifecycle.drain_timeout`, final checkpoint, clean exit, reap.
   `subagent.retire {handle}` forces the same path. The durable store
   outlives the child per the parent's retention policy — the audit trail
   is the point.
9. **Budget is a grant, not a hope.** An instance template carries a
   `budget:` (the RFC 0030 `Budget` shape). The child enforces it as its
   own (`on_exhausted: refuse`), and the child's reported usage counts
   against the parent's windows through the existing hierarchical
   token accounting (RFC 0003/0009) — a spawned desk cannot out-spend its
   sponsor.
10. **Caps extend, refusals stay tool results.** Instance children are
    counted and refused at the same chokepoint under
    `limits.subagents.instances.{breadth, total, rate}` (defaults 2 / 8 /
    conservative). An instance child may spawn flat subagents (it is a full
    reactor) but may **not** declare templates of its own in Phase A — no
    recursive fleets.
11. **The vestigial `workflow` field on the `subagent` node is removed.**
    It has been accepted and silently ignored since the v1 in-child
    workflow driver was deleted; this RFC deletes it from the registry so
    the field's *name* can never again promise what `template` now
    actually delivers.

## 4. The `subagents:` section

```yaml
subagents:
  # Section policy. `allow_freeform: false` makes templates the ONLY spawn
  # path — the strongest posture: every child the model can create is a
  # definition the operator reviewed.
  allow_freeform: true

  # Applied to every spawn (flat and templated) unless overridden at the
  # template or call site. The durable home for "what a worker here is".
  defaults:
    model: small-fast
    priority: low
    limits: {max_tokens: 50000, deadline: 10m}

  templates:
    # ---- flat tier: no config-defining directives ----------------------
    researcher:
      instruction: |
        Research {{params.topic}} and return a source-linked brief.
        :::context
        House style: cite primary sources; flag anything paywalled.
        :::
      params:
        topic: {type: string, required: true}
      servers: [search]          # narrowing grant — subset of the parent's
      tools: [search.query]
      limits: {max_tokens: 80000, deadline: 15m}
      mode: sync                 # sync | async | detached | warm

    # ---- instance tier: directives define machinery --------------------
    incident-room:
      instruction: |
        You are the dedicated war room for incident
        {{params.incident_id}} (severity {{params.severity}}).
        Keep the timeline; coordinate with the parent desk; propose the
        postmortem when the all-clear arrives.

        :::mcp
        servers:
          logs: {url: "https://logs.internal/mcp", tags: [sensitive]}
        :::
        :::stream
        timeline: {retention: {max_age: 30d}}
        :::
        :::workflow
        name: on-update
        start: {a2a: {command: incident.update, schema: {…}}}
        steps: {…}
        :::
        :::workflow
        name: all-clear
        start: {signal: "resolved/{{params.incident_id}}"}
        steps: {…}
        :::
      params:
        incident_id: {type: string, required: true}
        severity: {type: string, enum: [low, high], default: low}
      budget:
        windows: [{per: day, tokens: 200000}]
        on_exhausted: refuse
      limits: {memory_bytes: 536870912, cpu_seconds: 3600}
      ttl: 3d
      until: "closed/{{params.incident_id}}"
      singleton: false           # true → peer alias = template name
```

Field semantics, by tier:

| field | flat tier | instance tier |
|---|---|---|
| `instruction` | the worker's objective document | the child instance's full definition |
| `params` | declared holes, schema-validated | same |
| `servers` / `tools` | narrowing grants from the parent's set | refused — the child's `:::mcp` declares its own servers, gated by its own trifecta check |
| `limits` | protocol `Limits` (`max_steps`, `max_tokens`, `deadline`, `memory_bytes`, `cpu_seconds`) | OS caps only (`memory_bytes`, `cpu_seconds`); token ceilings live in `budget` |
| `budget` | refused (use `limits.max_tokens`) | the RFC 0030 `Budget`, enforced in-child, drawn against the parent |
| `mode` | `sync \| async \| detached \| warm` | `detached` only (Phase A); handles still work |
| `ttl` / `until` | refused (a worker has `deadline`) | graceful retirement triggers |
| `model`, `priority`, `skills`, `context` | as today's step fields | folded into the composed settings |

Caps:

```yaml
limits:
  subagents:
    depth: 3
    breadth: 8
    total: 64
    rate: "10/1m"
    instances:                 # NEW — instance-tier children only
      breadth: 2               # live at once
      total: 8                 # lifetime
      rate: "4/1h"
```

## 5. Instantiation

The `subagent` step and `subagent.run` tool gain the template form:

```yaml
steps:
  war_room:
    subagent: {}               # kind selector, as today
    template: incident-room
    params:
      incident_id: "{{ output.alert_id }}"
      severity: high
    mode: detached
```

Rules at the chokepoint (all refusals are tool results, RFC 0009 §7):

- `template` and `instruction` are mutually exclusive on one call.
- `params` are validated against the template's schema **synchronously**;
  unknown keys, missing required keys, and type/enum mismatches are refused
  naming the field. Undeclared `{{params.*}}` references in the template
  are a **boot-time** validation failure, not a spawn-time one.
- Flat-tier caps apply to flat spawns; `instances.*` caps to instance
  spawns; both share the pressure-shedding check.
- The spawn returns the existing handle. `subagent.await` works for both
  tiers (instance tier: resolves at retirement with the child's final
  status). `subagent.send` to an instance child delivers over the socket as
  an A2A message into the child's conversation surface; typed commands go
  through the auto-registered peer like any A2A call.

## 6. The instance-tier child

**Composition (at parent boot).** For each instance template the parent
runs RFC 0034 extraction once on the authored text, producing frozen
machinery fragments plus cleaned prose with `{{params.*}}` holes. The
composed result is validated as a complete settings document — including
the lethal-trifecta gate over the template's own `:::mcp` set — so a
template that cannot legally boot fails the *parent's* startup, naming the
template. What ships to the child at spawn is this **structured, composed
payload** (settings + prose + folded params), never re-parsed text.

**Identity and store.** The child's instance name is
`<agent.name>/<template>/<handle>`; its file store (RFC 0033) lives under
the parent's state directory at `subagents/<handle>/`. Restart-on-crash is
allowed for instance children (state is durable; identity is the handle)
under the RFC 0003 restart governor; flat workers stay non-restartable.

**Wiring.** Child A2A listens on a parent-allocated unix socket
(`subagents/<handle>/a2a.sock`). The parent registers peer `<handle>`
(alias `<template>` when `singleton: true` — a second live spawn of a
singleton is refused); the child receives peer `parent`. The child's
outbound principal is `subagent:<template>/<handle>` (RFC 0029), so the
parent's `principals:` rules can scope what a spawned desk may ask of it.

**Events in, evidence out.** No public listeners (Decision 7): external
webhooks stay on the parent's static routes and arrive in the child as
typed commands or forwarded signals. The child's audit log and store are
its own; the parent's observation feed (RFC 0032) carries the lifecycle
events (`subagent.spawn` with `template` + `tier`, retirement, restart,
budget refusals) so the TUI shows the fleet without merging feeds.

**Retirement.** First of `ttl` elapsing, the `until` signal firing in the
child, `subagent.retire`, or parent shutdown (cascade): the child stops
admitting starts, drains within `lifecycle.drain_timeout`, checkpoints,
exits cleanly, and is reaped. The store directory is retained per the
parent's retention policy; `subagent/<handle>` records the terminal status
and where the evidence lives.

## 7. Validation (fail-closed, at parent boot)

Refused with the template named: config-defining directives when
`allow_freeform`-only flat fields are also present in ways that conflict
(e.g. `servers:` on an instance template); `webhook` starts or any TCP
listener in composed machinery; `subagents.templates` inside a template
(no recursive fleets, Phase A); `{{params.X}}` where `X` is undeclared;
a `budget:` on a flat template or `limits.max_tokens` on an instance one;
trifecta violations in the composed MCP set; `singleton: true` combined
with an `until:` that does not reference params (a fixed signal on a
reusable room is almost certainly a bug); peer-name collisions between a
template alias and a configured `a2a.peers` entry.

## 8. Security model

**8.1 Authorship.** The model's entire creative surface is: pick a declared
template, fill declared holes. `allow_freeform: false` extends this to the
flat tier. Widening never happens at spawn time: flat grants must be
subsets of the parent's, and instance capability comes only from machinery
the operator wrote and the trifecta gate passed.

**8.2 Injection ordering.** Extraction happens once, on operator-authored
text, at boot (RFC 0034 §4: directives execute by surface, not content).
Params fold in afterward as engine *data* — a param value containing
`:::mcp` is three punctuation characters in a string, because nothing ever
re-parses the composed result as a document. This ordering is normative.

**8.3 Trifecta across processes.** Each child is gated on its own composed
set, and the *split* across parent and children is real isolation (separate
processes, separate stores, narrowed principals). The residual is the
classic confused deputy — a parent holding `untrusted_input` commanding a
child holding `egress` — and the mitigation is the same one the startup
example documents: typed command contracts that answer the question asked,
plus the parent's own gate still applying to the parent's own set. The RFC
makes the residual a documentation requirement, not a silent property.

**8.4 Resources.** Instance children carry rlimits/cgroup caps like any
child (RFC 0003), sit under the tree-token ceiling, and count against
`instances.*` caps at one unforgeable chokepoint.

## 9. Phases

- **Phase A** — the `subagents:` section (templates, defaults,
  `allow_freeform`); flat-tier templates; instance tier with `detached`
  mode, unix-socket A2A auto-wiring, `ttl`/`until` retirement,
  budget-as-grant, `instances.*` caps; registry changes (`template`,
  `params`; `workflow` field removed); validation set of §7.
- **Phase B** — `mode: sync` for instance children via a declared
  `result: {workflow: <name>}` (spawn, wait for that workflow's first
  completed run, return its output); stream bridging (a parent stream
  mirrored into the child and/or the reverse, riding RFC 0035 bindings);
  fleet views in status/TUI.
- **Phase C** (contingent) — free-form instance specs behind a mandatory
  `human` gate; port-bearing children with allocation policy; nested
  fleets. None is scheduled; each needs its own justification.

## 10. Alternatives considered

- **Widen the flat subagent** (give it signals/workflows): rejected — the
  `NoSelfTools` invariant is what keeps depth-N spawning analyzable; a
  "slightly orchestrating" worker is the worst of both tiers.
- **A separate top-level `instance_templates:` + `instance` node**:
  rejected in favor of one `subagents:` section — one concept ("a child
  this agent may create"), one chokepoint, one handle namespace, one caps
  family; the tier is a property of the definition, not a second API.
- **Process-manager-only** (systemd template units over RFC 0034 docs):
  already works, stays the right answer for *standing* desks; this RFC is
  for children whose lifecycle belongs to a workflow, not an operator.

## 11. Cross-references

RFC 0003 (process tree, restart governor, hierarchical token accounting) ·
RFC 0009 (spawn chokepoint, payload, caps) · RFC 0025/0033 (durable store,
file store identity) · RFC 0026 §6 (`subagent.*` tools) · RFC 0029 (peers,
principals, typed commands) · RFC 0030 (settings schema, `Budget`) ·
RFC 0034 (instruction documents, extraction surface rule, retirement) ·
RFC 0035 (streams; Phase B bridging).
