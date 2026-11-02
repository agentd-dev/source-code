# RFC 0056: A document's own agents

**Status:** Accepted 2026-11-02 by the user (decisions in §9); implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-11-02
**Builds on:** RFC 0034 and instruction/1 (documents and their `agent`
object), RFC 0036 (subagent templates and instance children), RFC 0042 (the
document configuration boundary), RFC 0055 §3.1 (the operator's names stand)

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

A served instruction document may declare the team of agents agentd runs as
its subagents.

1. **Spelling.** An `agent` with no `protocol`, `src` or `endpoint`, written
   as a container, is an agent the host runs itself:

   ```markdown
   ::::agent{id=support title="Support lead"}
   You answer every customer who writes to support@.

   :::mcp{id=support-desk src="https://desk.example.com/mcp" tools="tickets.list,tickets.reply"}
   :::

   :::workflow{id=support-queue dialect="instruction/workflow"}
   name: support-queue
   steps:
     start: {kind: once}
     answer: {kind: agent, depends_on: [start], instruction: Answer every open ticket.}
     done: {kind: finish, depends_on: [answer]}
   :::
   ::::
   ```

   Its body is its instruction document: prose, and its own `:::mcp`,
   `:::skill`, `:::workflow` and `:::config`.
2. **Gate.** A new operator grant, `agent.document_capabilities: [agents]`,
   off by default. Each admitted agent becomes the template
   `subagents.templates.<id>`.
3. **Over the wire** (`url:`, `oci:`, `mcp:`), `agents` applies only when the
   document verified against a pinned publisher (`agent.instruction.trust`)
   and the author's signature attests `agents`.
4. **Live re-compose.** A reload makes a new agent spawnable, retires a
   removed agent's live children, and retires and starts again a changed
   agent's live singleton from its new composition.
5. **The operator's names stand.** An operator template of the same name is
   used; the document's agent is held as text.

## 2. Motivation

A document that describes a company — its officers, its teams, their tools
and their routines — describes a team of agents. Before this RFC agentd could
run it only as one agent: every `::agent` that was not an A2A peer was held
as text, and `subagents` was operator configuration a document could not
write (RFC 0042). The operator had to copy each agent's section of the
document into a `subagents.templates` entry by hand, and keep the two in
step on every edit. The document already says what each agent is; the host
should be able to run it as written, within limits the operator sets.

## 3. The mapping

### 3.1 Which `agent` is the host's

instruction/1's `agent` object names an agent. One with `protocol=a2a` and an
`endpoint` or an Agent Card `src` is an agent elsewhere, and becomes an
`a2a.peers` entry under the `identity` grant, as before. One with none of
`protocol`, `src` and `endpoint` is an agent the host runs itself
(`config/idoc.rs`, `Folder::own_agent`). It is written as a container: its
body is its instruction, and a leaf with no body is held, said. The reader
already refuses an `agent` without an `id`.

### 3.2 What it becomes

Each admitted agent becomes `subagents.templates.<id>` with:

- `instruction`: the body as written (`ConnectedObject::body`). When the
  parent document declares parameters in its front matter, the body is
  written under them (a front matter with `spec` and `parameters`), so a
  `${name}` in the body reads as it does in the document.
- `singleton: true` when the body carries machinery (a `:::workflow`, or a
  `:::mcp`, `:::config` or anything else that maps onto configuration). The
  tier follows from the existing rule (`config/templates.rs`): machinery makes
  an instance-tier child, prose a flat worker, and an instance template's
  default mode is `detached`.

Nothing else is set. The template carries a derived, unserialized marker
(`SubagentTemplate::document`, a `DocumentAgentOrigin` with the document's
line, the agent's title and whether the document came over the wire), never a
configuration key.

### 3.3 Its own channel

`subagents` stays in `OPERATOR_ONLY`: a `:::config` still cannot write it,
and `document_wrote_operator_config` still refuses one that tries. The agents
join the templates through their own channel (`document_agent_templates` in
`config/settings/mod.rs`), as `fold_document_servers` does for servers. They
join after the loop that resolves operator templates' `file:`, `dir:`,
`url:` and `oci:` sources, so an agent's body is never read as a path: the
operator's disk is never read on a document's behalf.

### 3.4 The blocks inside an agent

The blocks inside an agent's body are the agent's, never the parent's,
whether the agent is admitted or held. `agent` is no layout container, so
the fold's `Location::in_layout` test leaves them out of the parent's
configuration; the child reads them from its own instruction.

### 3.5 Compiled with no grants

A document agent's template is compiled (`templates::compile_one`) with no
grant families at all: no `interface`, no `identity`, no `agents`. An A2A
agent or an agent written in its body stays text, and a webhook route in it
refuses the document as it refuses any template (an instance child has no
listener). An operator's template keeps every grant but `agents`: a child
defines no children of its own. Everything RFC 0036 and RFC 0042 hold a
template to holds: `validate_instance_machinery` refuses operator
configuration, the sections the parent composes (`a2a`, `intelligence`,
`store`, `lifecycle`), delegated servers, Events bindings and webhook
routes, and checks egress and the trifecta over the child's servers.

### 3.6 Skills travel

A template's inline `:::skill` blocks reach its instance child: the parent
writes each as `<name>/SKILL.md` in the child's directory and composes the
child's `skills.dir` to it (`runtime/instances.rs`, `compose_instance_doc`).
Before this RFC they were stripped from the child's text and lost, for every
instance template.

## 4. Security

### 4.1 Off by default, the operator's grant

`agents` is in `idoc::FAMILIES` and the schema's `document_capabilities`
enum, and in no default. Without it an agent with no protocol is held as text
with a `host-policy` warning that names the grant.

### 4.2 What bounds a child

A child of a document agent is bounded by the operator at every point a
child of an operator template is:

- **How many.** `limits.subagents.instances` (breadth 2, total 8, `4/1h` by
  default) caps them at the one spawn chokepoint, and the load warns when the
  document defines more instance agents than breadth, total or the rate's
  burst allows.
- **What it runs on.** `subagents.defaults` (its model) and the parent's
  `intelligence` section, minus its budget, routes and inline tokens.
- **What it reaches.** Its servers are its own `:::mcp` blocks, untrusted
  unless they are the operator's (`untrust_template_servers`), adopted as the
  operator's server with its credential and ceiling when they match one
  (`adopt_operator_servers`), held to `security.egress` and the trifecta. It
  is composed the parent's `http.endpoints`, `security.egress`,
  `allow_trifecta` and AAuth identity, as any child is.
- **Who it can call.** No A2A peer: it reaches its parent over the channel
  it is handed at spawn and nothing else. Its parent reaches it as
  `instance:<handle>`, or by its template name while it is the live
  singleton; a configured peer of the same name wins.
- **What credentials it holds.** None of its own. A document from the wire
  names no secret in a child's machinery (§4.3); one the operator supplied
  is the operator's text and may, as the operator's own document may.

### 4.3 Over the wire: why `agents` is not wire-floor

`identity` is in `WIRE_FLOOR`: a document from the wire never makes an A2A
peer, whatever is granted or signed. A peer is an outbound identity at an
endpoint the document names, carrying the operator's credentials there, and
nothing narrower than "no" bounds it.

A document agent is not that. Everything it can reach is the operator's
already: the model the parent uses, the servers the egress policy admits,
the ceilings above. Nothing in it names a new identity or a new endpoint
beyond what an `:::mcp` block in the parent document could. What it adds is
that the document's prose and machinery run in more processes, which the
ceilings bound. So the right control is not a floor but provenance: over the
wire, `agents` applies only when the document verified against a publisher
the operator pinned **and** the author's signature attests `agents`
(`idoc::Admission`, which carries `attested`). An unpinned document, an
unsigned one, and one whose signature attests other families never uses
`agents`, whatever the operator granted. The §7.8 floor holds inside the
agent too: a child of a wire document whose machinery names a secret or
writes `a2a.peers` refuses the document (`wire_floor_effects`, at fold and
again at compile).

Every path a wire document takes agrees. The load folds a fetched `url:` or
`oci:` document with its verified attestation (`Folding::attested`); a
registry document (`mcp:`) folds as a `RegistryDocument` carrying the
`attested` the runtime's read verified (`verify_registry_read`); and the
runtime's own reads (`deliver_served`, the OCI re-pull) call
`extract_from_wire` with the same attestation.

## 5. Reload

`subagents.templates` is reloadable. After a reload commits
(`Runtime::commit_reload`), the templates are compiled old and new, and every
live instance child follows its template (`Runtime::recompose_children`):

- **Removed:** the child is retired (`subagent.template.retired`).
- **Changed composition, singleton:** the child is retired
  (`subagent.template.recomposed`), and once it has exited it is spawned
  again from the new template with what it was spawned with — its params,
  for the principal it worked for, carrying what it carried, detached
  (`subagent.template.respawned`, or `subagent.template.respawn_failed` with
  the refusal). A composition is the tier, prose, machinery, skills, gate
  channels and fields; where in its document an agent stands is not.
- **Changed composition, not a singleton:** the child keeps the composition
  it was spawned with until it retires.
- **Added:** the template is spawnable from the next spawn.

The rule applies to every template, the operator's and a document's. It
closes the gap where removing a singleton template left its child running
with its peer alias gone. A template added or changed is logged as
`subagent.template.composed`, as at load, and a served document's
`instruction.machinery.applied` names `agents_added` and `agents_removed`.

## 6. The operator's names stand

As for servers, peers and workflows in v1.21.1: an operator
`subagents.templates` entry of the same name stands, and the document's agent
is held as text with a `host-policy` warning naming it; the rest of the
document applies.

## 7. Workflows

Per the user's rule, the capability fits workflows:

- A document's workflow reaches its agents with
  `{kind: subagent, template: <id>}`. An instance child is detached, so a
  `parallel` of ten spawns returns at once.
- Once a singleton is live, `a2a.send` and `a2a.delegate` reach it by its
  id (`instance_peer_endpoint`).
- A child's own `:::workflow` blocks, a `once` start included, run inside the
  child daemon.
- A document workflow whose `once` start spawns its agents runs at boot, and
  for a served (`mcp:`) document at the boot reload that folds it. A reload
  arms only definitions it installed new or replaced, so an unchanged
  team-spawning workflow does not spawn again.

No new node: spawning, addressing and retiring a child are the existing
`subagent`, `a2a.send`/`a2a.delegate` and `subagent.retire`.

## 8. Observability

At load the parent logs one `subagent.template.composed` per document agent:
its name, `origin: document`, line, title, tier, and its servers, workflows
and skills. The team is in the log before any of it is spawned. A child's own
runs and steps are in the child's log (`log` in its directory), not the
parent's: what reaches the parent is the child's record (status and spend,
in the status document, the A2A feed and the TUI's subagents screen), a
`mode: sync` result, and the streams it mirrors.

## 9. Decisions (the user, 2026-11-02)

1. **Spelling:** an `agent` with no `protocol`, `src` or `endpoint`,
   written as a container whose body is its instruction document.
2. **Gate:** a new operator grant, `agents`, off by default; the operator's
   ceilings bound everything; children never reach A2A peers or credentials.
3. **Over the wire:** `agents` applies only with a verified pin whose author
   signature attests it; it is not in `WIRE_FLOOR`.
4. **Live re-compose** on reload, as in §5.
5. **The operator wins on names**, as in v1.21.1.

## 10. Alternatives considered

- **Let `:::config` write `subagents.templates`.** It would hand a document
  every template field — `limits`, `budget`, `servers`, `durable`,
  `mirror_streams` — and the boundary would have to classify each. The agent
  channel writes an instruction and `singleton`, and nothing else.
- **Put `agents` in `WIRE_FLOOR`.** It would make the capability useless for
  the case it is for, a published company document, while bounding nothing
  the ceilings do not already bound (§4.3).
- **Compile a document agent with the parent's grants.** A child would then
  define peers, routes or children of its own on the parent's grant, one hop
  from the operator's review.

## 11. Limits

- A child's references to the parent's people and sibling agents
  (`:person[…]{ref=#andrii}`) do not resolve in the child's text, and are
  said as `reference-optional` warnings at the parent's line. A child's
  `human` step cannot address the parent document's people.
- A child's `:::skill{src}` is not fetched.
- A changed definition's `once` start fires again on reload under
  `policy: always`, or once the earlier run was evicted; `workflow.resume` and
  `workflow.create` arm every workflow. A team-spawning workflow that runs
  again finds its singletons live and fails its spawns.
- A restart does not recompose: a durable child restored at boot runs its
  stored composition until a reload or its retirement.
