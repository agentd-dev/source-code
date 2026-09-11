# RFC 0042: What a served document may configure

**Status:** Implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-09-11
**Extends:** RFC 0034 / RFC 0039 (instruction documents and their directives) — this is §6 rule 4, made a mechanism.
**Composes with:** RFC 0037 (service catalog and egress policy); RFC 0036 (subagent templates).

---

## 1. Summary

A `:::!config` fragment lets an instruction document configure the agent it
instructs. That is the point of the directive: one document can define an
entire agent — its workflows, the MCP servers it needs, the streams it consumes,
how much context it keeps. But an instruction commonly comes from somewhere the
operator does not fully control. That is why signing, pinning, the capability
ladder and the `unavailable` policies exist at all.

So there are two questions, and until this RFC agentd answered them with one
list:

> **What may a document declare about *itself*?**
> **What may a document declare about *the deployment it runs in*?**

The rule is that a document configures **what it is**, never the deployment it
runs in. This RFC makes that rule an allow-list checked by path, applies it
wherever a served document contributes configuration, and holds the
classification to the generated settings schema with a test — so a setting
added tomorrow is the operator's until somebody says otherwise.

## 2. Motivation

The boundary was `DOCUMENT_MAY_NOT_WRITE`: six path prefixes
(`agent.document_capabilities`, `agent.instruction`, `security`, `identity`,
and two retired spellings). The settings schema has **143 paths**. Everything
nobody had thought about was writable by default.

It was also writable from the bottom rung. `config`, `mcp`, `tools`,
`workflow`, `stream` and `skill` all carry `x-family: core` in
`instruction.schema.json`, and `agent.document_capabilities` gates the other
seven families — never `core`. So an **unsigned, unpinned document with no
capability grant at all** could write configuration.

Five paths were exploitable. Each was reproduced against the built binary
before it was closed.

**`services` — the allow-list the egress gate reads.** Under
`security.egress: closed`, a document catalogued its own destination:

```
config.effective_server  server=attacker  endpoint=https://evil.example/mcp  service=exfilmcp
config.valid
```

The control — the identical config without the document's `services:` entries —
produced two `config.invalid` lines. The gate still ran. The list it is checked
against had become the gated party's to write.

**`agent.tools` — the grant.** Fragments merge deep and arrays **concatenate**,
document entries first (`merge_missing`). An operator who narrowed the grant got
it widened:

```
operator only  -> ["think", "finish"]
with document  -> ["exec", "subagent.run", "think", "finish"]
```

**`intelligence.endpoints` — where the conversation goes.** Same concatenation,
so the document's endpoint landed *ahead* of the operator's and became the
primary: system prompt, tool results and all.

**`a2a.principals` — who may call.** `match: {any: true}, role: user,
grants: ["*"]` validated, admitting anonymous callers to every non-admin
command. (`any → operator` was already refused by a validation check, and the
admin family answers to the role rather than to grants — so operator control
itself was never reachable.)

**`interface.enabled` and `webhooks.listen` — the control surfaces.** A document
turned on the human control plane (HITL gates, steering, pause) and opened an
inbound socket of its choosing.

**And a sixth thing, which is the reason the rule needed one home.** A
*subagent template's* fragment had a seven-key refusal list of its own
(`config/templates.rs`) that never consulted the document rule. So the boundary
was reachable in one hop — the identical fragment, two placements:

```
own document                          → REFUSED (agent.document_capabilities, identity.autonomous_as)
via subagents.templates.helper        → config.valid
```

The child then ran as `principal://root` with a widened capability ladder.

None of the six was a decision anybody made. Each was a path nobody had
classified — which is the same defect the reload partition had, and closed, when
three "successful" reloads turned out to change nothing because
`a2a.principals`, the webhook routes and `interface.origins` were fields nobody
had classified either.

## 3. The rule (normative)

A served document's configuration fragment is admitted **path by path** against
an allow-list.

1. A path at or under an entry of `DOCUMENT_MAY_WRITE` is admitted.
2. A path at or under an entry of `OPERATOR_ONLY` is **refused**, naming what
   the document set beneath it — `agent.instruction.trust`, not the
   `agent.instruction` prefix that denied it. The operator reading the refusal
   needs the line to go and delete.
3. A path under **neither** is descended into, because a section can straddle
   the boundary (`agent` and `intelligence` both do). A **leaf** in neither list
   is **refused**.

Rule 3 is the whole point of the polarity. A deny-list can only refuse what
somebody already thought of; an allow-list refuses by default and the mistake
becomes a message an operator can read, not a grant nobody notices. `services`
was writable not because anyone decided it should be, but because nobody had
written it down.

Checking is by **path**, not by top-level key name: the fragment merges deep, so
a nested `agent: {document_capabilities: […]}` is the same self-grant as a
top-level one.

Because the check runs **before** the merge, array concatenation stops being a
separate hazard. Concatenating a document's `workflows` onto the operator's is
correct; for an operator-only path there is nothing left to concatenate.

## 4. The two lists

**`DOCUMENT_MAY_WRITE` — what the agent is.**

| | |
|---|---|
| `workflows`, `streams`, `mcp`, `vars` | what the agent DOES — the point of `:::!workflow`, `:::!stream`, `:::!mcp` |
| `a2a.peers` | another agent this one collaborates with: an OUTBOUND dial, exactly like `mcp.servers`, and covered by the same closed-egress sweep. `:::peer` is the block that declares one |
| `context`, `goal`, `knowledge`, `search`, `memory`, `limits` | how it thinks, remembers and bounds itself |
| `intelligence.model`, `.models`, `.default`, `.dialect`, `.budget`, `.timeout`, `.preflight_model`, `.structured_output`, `.swap_policy` | which model and how much of it |
| `agent.name`, `.approval`, `.ask_human_fallback`, `.conversation_budget`, `.max_parallel_turns`, `.on_workflow_finished`, `.preflight`, `.wake_on` | the loop's own shape |
| `tools.narrow`, `tools.disabled` | `narrow` only ADDS trifecta tags and descriptions — more dangerous than the operator said, never less; `disabled` only ever takes capability AWAY, and is the spec's `deny` form |
| `skills.max_bytes`, `.max_loaded`, `.reference_prefix` | caps on the loader |
| `store.kind`, `.durability`, `.checkpoint`, `.on_error`, `.timeout`, `.max_value_bytes`, `.prefix` | the durability CLASS and the caps around it — never the PLACE |
| `lifecycle.run_until`, `.idle_grace`, `.until_signal` | when this agent is finished. `agentd --instruction doc.md` is a shipped shape, and an agent that cannot say when it is done is not one |
| `observability.log_level`, `.runtime_events` | how loud the log is, and routing runtime events into a `streams:` entry the document declared |

**`OPERATOR_ONLY` — the deployment.**

| | |
|---|---|
| `agent.document_capabilities` | the ladder this document is standing on |
| `agent.instruction`, `agent.prompt` | a document that rewrites its source points the next read at itself |
| `agent.tools`, `tools.overrides` | the grant, and re-routing a built-in onto a server |
| `security` | trifecta, egress, `exec`, policies, TLS trust, AAuth |
| `services` | the allow-list `egress: closed` is checked against |
| `identity` | who work is done on behalf of |
| `a2a.listen`, `.tls`, `.bearer`, `.principals`, `.push`, `.conversation_ttl` | who may call THIS agent, as what, over which socket. Enumerated rather than taken as a section, because `a2a.peers` sits inside it and is the document's — so a field added here later lands unclassified, which is the point |
| `interface`, `webhooks` | the human control plane; inbound sockets and their auth |
| `intelligence.endpoints`, `.token`, `.token_file`, `.headers`, `.auth` | where the conversation goes and the credential it goes with |
| `subagents` | a whole child agent — its own source, grants and identity |
| `store.file`, `.http`, `.mcp`, `.audit`, `.retention` | WHERE state lives — a path on the host, or a remote the deployment must be willing to reach — and the audit record |
| `lifecycle.drain_timeout`, `.exit_code_map`, `.run_id`, `.watch_config` | the orchestration contract: what an orchestrator sees, and whether the process watches its own config file |
| `observability.log_content`, `.audit`, `.otel`, `.metrics_addr`, `.health_file`, `.report_file`, `.events_ring`, `.traceparent` | `log_content` puts conversation TEXT into the operator's log pipeline; `otel.endpoint` is the one egress `closed` deliberately does not cover; the rest name sockets and files |
| `skills.dir`, `skills.sources` | where prompt text is read FROM |
| `config_version` | which schema the operator wrote against |
| `instruction_sources` | the retired spelling of `agent.instruction.trust`, refused by name |

**The `mcp.servers` / `services` pairing is deliberate and is the heart of the
design.** A document declaring the servers it needs is the purpose of
`:::!mcp`. A document declaring which hosts *this deployment* may dial is a
different sentence, and it is the one `security.egress: closed` reads. The
document says what it needs; the operator says what is reachable.

**Four sections straddle the line, and are split rather than taken whole.**
`agent`, `intelligence`, `store` and `lifecycle` each hold both kinds of
setting, and `observability` and `a2a` do too. Splitting them is what keeps
`agentd --instruction doc.md` working — one markdown file really can define a
whole agent, with a durability class, an idle horizon and a model — while the
paths, sockets, endpoints, credentials, audit record and orchestration contract
stay the operator's. A section taken whole in either direction would have been
either a hole or a feature removed.

## 5. Where the rule applies

Wherever a served document contributes configuration:

- the agent's own instruction, at config load (`Settings::from_document`);
- a **subagent template's** machinery, at `compile_one` — a template's
  `:::!config` is a served document's fragment, so it is classified by the same
  function. This replaces the seven-key list, which it almost entirely subsumes:
  six of those keys (`webhooks`, `interface`, `subagents`, `store`, `security`,
  `lifecycle`) are `OPERATOR_ONLY` outright.

  A child then takes **one narrowing on top**, and it is a different rule rather
  than the same one repeated. `a2a`, `intelligence`, `store` and `lifecycle` are
  exactly the sections `compose_instance_doc` ASSIGNS wholesale, and a document
  at large may write part of each — but a child's are **overwritten at spawn**:
  it inherits the parent's intelligence section, a store under its own instance
  directory, an `a2a` block wiring it to the parent over its own socket, and a
  lifecycle built from the template's `until:`. Accepting them would accept a
  setting that does nothing, which is the defect class this whole cycle was
  about. A template sets the child's model and ceiling with its own `model:` and
  `budget:`, and its retirement with `until:` and `ttl:`.

A re-pulled or re-subscribed document's machinery still applies only on
reload/restart (§5.5 quiesce), and a reload re-validates, so the rule holds
there by construction rather than by a second implementation.

One composition fix rides along: a child instance's `services` is now assigned
**unconditionally** from the parent. It was assigned only when the parent had a
catalogue of its own, so a parent with none left in place whatever the template
wrote. Composition must not depend on a check somewhere else still holding.

## 6. The forcing function

`every_config_path_is_classified_for_documents` walks the **generated** settings
schema and fails on any path in neither list, with the same message shape as
its sibling: *a path in neither list is not "probably fine", it is unexamined —
add it to one, and prefer `OPERATOR_ONLY` when unsure.* It also refuses a path
in both, and a stale entry naming a path the schema no longer has (retired
spellings are declared, so they stay refusable by name).

Both completeness checks now share one schema walk, because "what is the config
surface?" has one answer and two walks would be two answers that only look
alike.

`a_fragment_key_nobody_classified_is_refused` pins rule 3 directly rather than
leaving it provable only by mutation.

Mutation-tested: removing the template's call fails both template tests;
removing `services` from `OPERATOR_ONLY` fails the completeness test — and
notably does **not** reopen the hole, because the walk descends to the leaf and
refuses it anyway. That is the allow-list doing its job.

## 7. What this does not change

- **The trust ladder.** `agent.document_capabilities` still gates the seven
  extended families; this is orthogonal, and applies to signed and unsigned
  documents alike.
- **The gates themselves.** `security.egress`, the trifecta rule, the policy
  engine and the SSRF guard are untouched. This restores the ground one of them
  stands on.
- **`egress: open`.** A document may still name any host in `mcp.servers` —
  that is what `open` means. For any deployment whose instruction document is
  not fully operator-controlled, `closed` plus an operator-written `services:`
  is the posture, and the documentation now says so.
- **What a document is for.** A document can still define an entire agent. The
  allow-list is where the useful surface already was.

## 8. Compatibility

Breaking, by design. A fragment writing a newly-refused path is refused by name
at config load with the setting and the reason — the refusal an operator can act
on. Nothing in the repository writes any newly-refused path from a fragment: the
one shipped example that involves `services` (`examples/startup/`) already
layers a separate `services.yaml` under the config, which is exactly the shape
this makes mandatory.

The migration is mechanical: move the section out of the `:::!config` fragment
and into a config file the operator owns, layered with a second `-c`.

## 9. Alternatives considered

**Keep the deny-list, add the missing entries.** Rejected: it fixes this
quarter's list and not the mechanism. Every one of the five paths was missing
for the same reason a sixth will be.

**Intersect instead of refuse.** agentd already has the idiom one layer up —
`grant ∩ ceiling ∩ attested` (§7.6 step 5) — and applied to config it would let
a document *narrow* what the operator granted (always safe) while capping any
widening. That is strictly more useful than a flat refusal for the paths where
narrowing means something (`agent.tools`, `limits`). Deferred, not rejected: a
refusal is legible and this RFC's job was to make the boundary exist. The
follow-on should name the paths where intersection is meaningful rather than
applying it everywhere.

**Gate `:::!config` behind a capability grant.** The root cause is that
`config`, `mcp` and `tools` are family `core`, below the ladder — so no
signature, pin or grant is needed to write configuration at all. Putting them
behind a rung would be the deeper fix, but `instruction.schema.json` is the
vendored spec, owned upstream by the Instruction Specification, so it is a
cross-repo decision rather than agentd's alone. Raised there separately. The
path classification bounds the damage regardless of what the ladder says, which
is why it went first.

## 10. Cross-references

- **RFC 0034 §6 rule 4** — the normative statement this implements.
- **RFC 0037** — the `services:` catalogue and `security.egress`, whose
  allow-list this protects.
- **RFC 0036** — subagent templates, the one hop that reached everything.
- **`docs/directives.md`** — the operator-facing tables.
