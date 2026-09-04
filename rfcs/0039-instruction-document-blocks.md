# RFC 0039: The instruction document as the whole agent — blocks, nesting, and the trust ladder

**Status:** Draft
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-09-04
**Extends:** RFC 0034 (instruction documents and directives) — this is dialect 2 of that surface.
**Depends on:** RFC 0027 (workflow dialect 3), RFC 0028 (tool registry), RFC 0029 (A2A), RFC 0032 (interface), RFC 0035 (event streams), RFC 0037 (service catalog & egress), RFC 0038 (system-prompt template).

---

**Normative home:** the **Instruction Document Specification**
(https://github.com/instruction-md/spec, published `main`; spec text CC-BY 4.0, conformance corpus Apache-2.0
at `conformance/LICENSE`). agentd is the
reference runtime implementation; this RFC remains its design rationale and
implementation record. **Where the two differ, the spec governs.**

## 1. Summary

RFC 0034 made one claim: **an agent is a document.** `agent.instruction` stopped
being a prompt and became a specified format — Markdown prose with `:::`
containers that fold into configuration, so a workflow, a skill, an MCP server
or a stream could be declared *where it is explained*.

Eight block kinds shipped. They cover what an agent *says* and what it *runs*.
They do not cover what it *is made of*: the code it executes, the files it
needs, the image it runs inside, the data it learns from, the people it asks,
the surface it renders, or the identity it borrows. Those still live in a
config file beside the document, which means the document is a description of
half an agent and an operator reads two artifacts to understand one system.

This RFC completes the surface. It adds **23 block kinds in seven families**
(plus four sub-blocks — `case`, `signature`, `schema`, `preview` — that exist
only inside a parent), makes **nesting semantic** (a longer fence contains
shorter ones, and the parser recurses), gives every block an **identity and a
reference syntax** so blocks can compose, and introduces the thing that makes
all of it safe to ship: a **trust ladder** that gates each family behind an
explicit operator grant, fail-closed, with the blast radius stated per rung.

The target is a single file that a person can read top to bottom, that
`agentd --config agent.md` can run, and that says out loud everything the agent
can reach.

### 1.1 What this is not

It is not a programming language, a package manager, or a build system. Three
constraints hold the design in place and are repeated throughout:

- **The dependency moat holds.** agentd links libc, serde and serde_json by
  default and nothing else. No block embeds a language runtime, a container
  engine, a vector index, a git implementation, or a template engine. Every
  block that *executes* something delegates to an MCP server, to the gated
  `exec` runner, or to an OCI runtime addressed as a service. A `:::function`
  block is a **declaration plus a dispatch target**, never an interpreter.
- **The lethal-trifecta gate holds.** Every new capability carries
  `untrusted_input` / `sensitive` / `egress` tags, folds into the same
  startup check, and can turn a config that used to boot into exit `2`. That is
  the point: a document that can now read the web and write files must say so.
- **Extraction is by surface.** Blocks are read from operator-authored
  instruction text only — never from conversation, tool output, a fetched
  document, or a peer's message. This RFC widens what a block can *do*, which
  makes that rule load-bearing rather than merely correct. §9 makes it
  enforceable rather than conventional.

---

## 2. Motivation — what an operator has to know today

Take a plausible agent: it watches a repository, runs a linter in a pinned
container, indexes the team handbook for retrieval, asks a human before opening
a pull request, and renders a small approval card in the TUI.

Today that agent is a `.md` instruction with four directives and a `.yml` with
sixty lines the document never mentions: the OCI image, the git credentials, the
knowledge server, the embedding model, the interface display config, the egress
policy for the linter's network access. The document explains *why*; the YAML
decides *what*. Neither is complete, they drift, and the drift is silent —
which is the failure mode this project has spent three releases eliminating
everywhere else.

The specific costs:

1. **The agent cannot be handed over as one artifact.** "Run this agent" is two
   files and a paragraph about which environment variables to set.
2. **Prose and configuration disagree.** The document says "we lint in a
   container pinned to 1.7"; the YAML pins 1.6. Nothing checks.
3. **Capability is invisible at the point of reading.** An operator reviewing
   the document cannot see that a tool reaches the internet, because that fact
   lives in a service catalog in another file.
4. **The model is shown a story about itself that omits its own limbs.** RFC
   0038 renders the system prompt from the environment; anything declared only
   in YAML is present in behaviour and absent from the narration.

---

## 3. The model

### 3.1 A document is a tree of blocks

```
document        := (prose | block)*
block           := fence attrs? newline body fence
fence           := ":"{3,}                  ; length defines containment
body            := (prose | block | code-fence)*
```

**Fence length is depth.** A block opened with `::::` is closed only by a run of
four or more colons, so it may contain `:::` blocks. This is already how the
parser tokenizes (RFC 0034's close rule is "a line of ≥ open-length colons"),
but today the contained text is *body*, not structure. Dialect 2 recurses into
it.

That is a behaviour change for any document that quotes directive syntax inside
a block, so it is gated:

```yaml
agent:
  instruction_dialect: 2      # default 1 until the phase-D cut
```

Under dialect 2, a block that wants to quote a fence without it being parsed
uses a `verbatim` attribute: `::::context{title="How to write a skill" verbatim}`.

### 3.2 Every block has identity

```
:::function{name=lint lang=python}
```

`name` is the block's identity within the document. It is unique per kind, and
`kind/name` is the fully-qualified reference: `function/lint`, `file/pyproject`,
`runtime/py311`.

### 3.3 Blocks reference each other with `@`

```
:::test{name=lint-passes target=@function/lint fixture=@fixture/clean-repo}
```

A `@ref` is resolved at load, fail-closed: an unresolvable reference is a config
error naming the line and the missing target. References are **acyclic** and
checked as such — a cycle is refused, not detected at runtime.

Unqualified `@name` resolves within the kind the attribute expects, so
`target=@lint` is legal where a function is expected. The qualified form is
always available for disambiguation.

### 3.4 Blocks nest to express containment, not inheritance

Nesting means "this belongs to that", not "this inherits from that". A
`:::case` inside a `:::test` is one of that test's cases. A `:::override`
inside a `:::mcp` overrides that server's elements. There is no implicit
attribute inheritance, because inherited attributes are exactly the kind of
action-at-a-distance that makes a document unreadable — the thing this whole
surface exists to prevent.

### 3.5 Markdown is not decoration

Three Markdown constructs become semantic inside blocks:

- **Fenced code** inside a `:::function` is the function body, and the code
  fence's info string is the language when the block does not declare one.
- **A table** inside a `:::data` or `:::fixture` block is parsed as rows —
  header row as field names. A table is the most readable way to write ten test
  vectors and should not require a YAML detour.
- **A heading** inside a `:::context` block becomes the card's section
  structure, so a long reference card retains its shape when rendered into the
  prompt (RFC 0038 already renders sections; this feeds it).

---

## 4. The trust ladder

This is the load-bearing section. Everything else is syntax.

RFC 0034's blocks are inert: they declare workflows, skills, servers and
streams that the existing config surface already accepted. This RFC adds blocks
that write files, execute code, pull images, and read the network. A document
that can do those things is a program, and a program that arrives as a document
is precisely the shape of a supply-chain problem.

So capability is **granted by the operator in configuration, not claimed by the
document**, and each rung names its blast radius:

```yaml
agent:
  instruction_dialect: 2
  document_capabilities: [material, knowledge, interface]   # default: []
```

| Rung | Grants the block families | Blast radius when granted |
|---|---|---|
| *(none — default)* | `workflow` `skill` `context` `example` `config` `mcp` `stream` `tools` `override` | today's RFC 0034 surface, plus narrowing |
| `material` | `file` `data` `media` `asset` | the document can materialize bytes into the workspace |
| `knowledge` | `knowledge` `index` `source` | the document can name what is retrieved and from where |
| `interface` | `endpoint` `ui` `human` `channel` | the document can open listeners and address people |
| `identity` | `peer` `policy` `secret-ref` | the document can name credentials and principals |
| `compute` | `runtime` `function` `test` `fixture` | **the document can cause code to execute** |
| `infra` | `git` `volume` `image` | the document can mount state and pull images |
| `compose` | `agent` | the document can spawn children that carry its grants |

`override` sits on the default rung deliberately: §5.7 constrains it to
*narrowing* only — add a tag, disable a tool, tighten an enum, rewrite a
description. A block that can only make an agent more careful needs no grant to
use, and gating it would push operators toward the blunter instrument
(disabling the server outright) for a safety improvement.

`agent` gets its own rung rather than riding `compute`, because spawning a child
is not the same risk as running code: a child inherits this document's grants,
so `compose` is the rung that says "this document may hand its capabilities to
something else". A child may never hold a grant its parent lacks.

Rules that make the ladder real rather than decorative:

1. **Fail-closed and specific.** A block whose family is not granted is a config
   error naming the block, the line, the family, and the exact key to add. Not a
   warning, not a skip.
2. **A grant is not a blank cheque.** `compute` lets a document *declare* a
   function; whether it may run still passes the tool registry, the egress
   policy (RFC 0037), and the trifecta gate. Two independent gates, because one
   gate is a single point of failure.
3. **Grants are restart-only.** Widening what a document may do is not a hot
   reload, for the same reason `interface.enabled` is not (RFC-0034-era
   v1.4.0): a capability an operator believes they revoked, still live in the
   running process, is the worst class of lie this project ships against.
4. **`--capabilities` reports the granted set and every block that used it**, so
   a control plane can see what a document actually reached for.
5. **The document cannot grant itself anything.** `:::config` may not write
   `agent.document_capabilities`, `security.*`, or `services.*`. This is the
   one place where `:::config` is not a general config fragment, and it is
   enforced by an explicit deny-list, not by hoping.

---

## 5. The block families

### 5.1 Compute — `runtime`, `function`, `test`, `fixture`

The design question: how does a document define executable code without agentd
linking a language runtime? Answer: **a function is a declaration bound to a
runtime, and a runtime is a service.** agentd never interprets; it dispatches.

````markdown
:::runtime{name=py311 kind=oci}
image: ghcr.io/acme/py311@sha256:3f0a…       # digest-pinned, not a tag
service: sandbox                              # a services[] entry (RFC 0037)
resources: { cpu: "1", memory: 512Mi, timeout: 30s }
network: none                                 # none | egress-policy
mounts:
  - { file: @file/pyproject, at: /work/pyproject.toml }
:::

::::function{name=lint runtime=@runtime/py311}
Lint a diff and return findings as JSON.

```python
import json, sys
def main(diff: str) -> dict:
    findings = [l for l in diff.splitlines() if l.startswith("+") and "TODO" in l]
    return {"count": len(findings), "lines": findings}
```

:::signature
input:  { diff: string }
output: { count: integer, lines: [string] }
:::
::::
````

A `:::function` becomes a **code-registered tool** (RFC 0022's `agentd::tools`
precedence: self > code > MCP), so the model calls `lint` like any other tool
and the registry's shadowing rules apply unchanged.

`image` **must be digest-pinned**. A mutable tag in an instruction document is a
remote-code-execution channel that looks like documentation, and the validator
refuses one by name.

Tests are first-class because a function nobody exercised is the artifact class
this project keeps finding:

```markdown
::::test{name=lint-catches-todo target=@function/lint}
:::case{name=finds-one}
given:  { diff: "+ // TODO: fix\n- old line" }
expect: { count: 1 }
:::
:::case{name=ignores-removals}
given:  { diff: "- // TODO: gone" }
expect: { count: 0 }
:::
::::

:::fixture{name=clean-repo}
| path        | content        |
|-------------|----------------|
| README.md   | # clean        |
| src/main.py | print("hi")    |
:::
```

`agentd --test` runs every declared case and exits non-zero on failure — so a
document is verifiable before it is trusted, and CI can gate on the document
itself. Cases run in the function's declared runtime, under the same limits.

### 5.2 Material — `file`, `data`, `media`, `asset`

```markdown
:::file{name=pyproject path=pyproject.toml mode=0644}
[project]
name = "acme-lint"
:::

:::data{name=slo format=table}
| plan       | first_response | resolution |
|------------|----------------|------------|
| enterprise | 1h             | 8h         |
| team       | 8h             | 3d         |
:::

:::media{name=logo kind=image src=https://cdn.acme.example/logo.png sha256=9f2c…}
Used in the approval card. Alt text: the Acme wordmark.
:::

:::asset{name=model-card kind=pdf src=file://./cards/mc.pdf}
:::
```

- `file` **materializes** into the run workspace. It is `material`-gated,
  path-confined (no `..`, no absolute paths, no symlink targets), and refuses to
  overwrite a file it did not itself write in this run.
- `data` is structured constant data — a table, YAML, or CSV — addressable as
  `{{data.slo}}` in templates and referenceable by workflows. It never leaves
  the document, which is what distinguishes it from `knowledge`.
- `media` and `asset` are **references with integrity**, not embedded bytes. A
  remote `src` requires a `sha256`, because an unpinned remote asset is the same
  mutable-tag problem in a different costume. Inline base64 is permitted below a
  size cap for genuinely small things (an icon), and refused above it with a
  pointer at `asset`.

### 5.3 Knowledge — `knowledge`, `index`, `source`

```markdown
:::knowledge{name=handbook server=kb}
auto_context: { enabled: true, max_chunks: 4 }
:::

::::index{name=handbook-index knowledge=@knowledge/handbook}
Chunking and retrieval policy for the handbook.

:::source{name=confluence kind=mcp server=confluence space=ENG}
tags: [untrusted_input]        # it is other people's prose
:::
:::source{name=repo-docs kind=git repo=@git/handbook path=docs/**}
:::
embedding: { model: @model/embed-small, dims: 768 }
chunk:     { size: 800, overlap: 120, by: heading }
rerank:    { model: @model/rerank, top_k: 4 }
::::
```

agentd links no vector store and no embedding library. `index` is a **policy
declaration** consumed by a knowledge MCP server that implements the RFC 0021
checkpointer-style profile; agentd validates the shape, resolves the refs,
tags the sources, and hands it over. The value is that retrieval policy is
readable next to the prose that depends on it, and that every source carries
its trifecta tags where a reviewer sees them.

`source` is where `untrusted_input` most often enters an agent, and declaring it
inline is the point: a reviewer reading "we index Confluence" sees, on the same
screen, that this makes the agent's context attacker-influenced.

### 5.4 Interface — `endpoint`, `ui`, `human`, `channel`

```markdown
:::endpoint{name=ticket-hook kind=webhook path=/hooks/ticket methods=[POST]}
auth: { hmac: { secret: "{{secret:HOOK_SECRET}}" } }
into: { stream: tickets, subject: ticket.created }
rate: "20/1s"
:::

::::ui{name=approval kind=card}
Rendered by a display client when this gate is open (RFC 0032).

:::schema
type: object
properties:
  summary: { type: string, title: "What will happen" }
  risk:    { type: string, enum: [low, medium, high] }
  approve: { type: boolean, title: "Approve this action" }
required: [summary, approve]
:::
:::preview
┌─ Approve deploy? ──────────────────────┐
│ What will happen: ship v1.6.0 to prod  │
│ Risk: medium                           │
│ [ ] Approve this action                │
└────────────────────────────────────────┘
:::
::::

:::human{name=oncall role=approver}
reach: { channel: @channel/ops-slack, escalate_after: 15m }
may:   [approve_deploy, answer_question]
:::

:::channel{name=ops-slack kind=mcp server=slack target="#ops"}
tags: [egress, untrusted_input]
:::
```

`endpoint` unifies what RFC 0035 §5 spread across a workflow's `webhook` node
and the `webhooks:` config: the listener and its binding declared together,
where the prose explains them.

`ui` is the piece with no home today. RFC 0032 gives display clients an
observation feed and taskless reads, but the *shape* of an approval is decided
by the client. A `ui` block lets the agent's author ship the schema **and a
text preview**, so a reviewer sees what a human will be asked before anyone is
asked, and a client renders a card it did not have to guess. The `preview` is
plain text on purpose — reviewable in a diff, and a client that cannot render
the schema can always render the preview.

`human` names a *role*, never a person's contact details. Reachability goes
through a `channel`, which is an MCP server like everything else, so the
document names "oncall" and the deployment decides who that is.

### 5.5 Identity — `peer`, `policy`, `secret-ref`

```markdown
:::peer{name=deployer endpoint=https://deploy.internal:8443}
auth: { kind: spiffe, svid: spiffe://acme/agents/triage }
grants: [workflow.run:deploy]
:::

:::policy{name=egress}
mode: closed
allow:
  - { kind: http, host: api.acme.example, methods: [GET] }
  - { kind: mcp,  server: @mcp/zendesk }
:::

:::secret-ref{name=hook kind=file path=/var/run/secrets/hook}
Rotated by the platform; never rendered into the prompt or the log.
:::
```

`secret-ref` declares *where a secret comes from*, never its value — the
existing `{{secret:…}}` / `{{secret-file:…}}` rule is unchanged and a literal
credential in a document is refused exactly as it is in a config file today.

### 5.6 Infrastructure — `git`, `volume`, `image`

```markdown
:::git{name=handbook url=https://github.com/acme/handbook ref=main}
auth: { kind: static, token: "{{secret:GH_TOKEN}}" }
sparse: [docs/**]
readonly: true
:::

:::volume{name=work kind=ephemeral size=1Gi}
:::

:::image{name=py311 digest=sha256:3f0a… registry=ghcr.io/acme/py311}
verify: { cosign: { issuer: https://token.actions.githubusercontent.com } }
:::
```

agentd implements no git client and no registry client. `git` resolves to a git
MCP server; `image` is consumed by whatever runs the OCI runtime. What agentd
owns is the **declaration, the pinning rules, and the refusal**: an unpinned
digest, a writable clone in a `readonly` context, or an unverifiable signature
is a config error at load.

### 5.7 Composition — `agent`, `override`

```markdown
:::agent{name=reviewer template=code-reviewer}
params: { depth: thorough }
ttl: 30m
:::
```

`agent` instantiates an RFC 0036 subagent template from the document, so a
document that decomposes into children says so where it explains why.

**`override` is the sub-block the MCP surface has been missing.** MCP servers
describe their own tools, and those descriptions are written by someone else for
everyone — not for this agent. Today an operator can disable a tool or remap it
(`tools.overrides`), but cannot say "this tool is fine, its description is
misleading in our context":

```markdown
::::mcp{name=zendesk}
endpoint: https://mcp.internal.example/zendesk
auth: { kind: static, token: "{{secret:ZENDESK_TOKEN}}" }

:::override{target=create_ticket}
description: >
  Opens a ticket in the ENG queue. Use this for engineering escalations only —
  billing has its own queue and a different SLA.
tags: [sensitive]
params:
  priority: { default: normal, enum: [low, normal, high] }
:::

:::override{target=delete_ticket}
disabled: true
reason: "Deletion is a compliance decision; we tombstone instead."
:::
::::
```

An `override` may **narrow, never widen**: it can add trifecta tags, disable a
tool, constrain a parameter, or rewrite a description. It cannot remove a tag
the server declared, re-enable something policy disabled, or widen an enum. The
asymmetry is the whole safety property — a document may make an agent more
careful, never less.

---

## 6. What the model is told

RFC 0038 renders the system prompt from data. Blocks feed it, and the rule is
that **the model sees the contract, never the machinery**:

| Block | What the model sees |
|---|---|
| `function` | the tool, its signature, its description |
| `runtime` `image` `volume` `git` | nothing — infrastructure is not context |
| `file` `data` | `data` is addressable; `file` is a fact about the workspace |
| `knowledge` `index` `source` | retrieval exists and what it covers, not chunk sizes |
| `ui` | nothing — it is for the human's client |
| `human` `channel` | who may be asked, and for what |
| `peer` `policy` `secret-ref` | nothing |
| `test` `fixture` | nothing — they are for CI |
| `context` `example` `skill` | rendered as today |

A document's prose is rendered as prose; a block's *presence* is rendered as the
one-line acknowledgement RFC 0034 already emits (`[workflow "x" is loaded…]`),
extended per kind. Machinery stays out of the context window because it costs
tokens on every turn and buys the model nothing.

---

## 7. Validation — what is refused

Every rule here is a refusal at load, with the line, the block, and what to
write instead:

1. A block whose family is not granted (§4).
2. An unresolvable `@ref`, or a reference cycle.
3. A duplicate `kind/name`.
4. A mutable image tag; an unpinned remote `media`/`asset`; an unverified
   signature where `verify` is declared.
5. A `file` path that escapes the workspace, is absolute, or collides.
6. An `override` that widens (removes a tag, re-enables, expands an enum).
7. A `:::config` touching `document_capabilities`, `security.*` or `services.*`.
8. A literal credential anywhere in the document.
9. A `function` with no `runtime`, or a `runtime` naming an undeclared service.
10. A `test` whose `target` is not a function, or a `case` outside a `test`.
11. A block nested where its parent does not accept it (`case` in `mcp`).
12. Dialect 2 syntax in a dialect 1 document, and vice versa.

And one that is a *warning*, deliberately: a `function` with no `test`. Refusing
would be moralizing; saying nothing repeats the mistake the project keeps
finding.

---

## 8. Interaction with existing contracts

- **RFC 0034** — dialect 1 documents keep working unchanged and unparsed-nested.
- **RFC 0037** — every executing or reaching block resolves through the service
  catalog; `policy` is a document-local view of the same egress rule, and the
  stricter of the two wins.
- **RFC 0028** — `function` enters the registry at the `code` tier; `override`
  is a registry decoration applied after the server's own descriptor.
- **RFC 0032** — `ui` gives the display client a schema and a preview it
  currently has to invent.
- **RFC 0035** — `endpoint` is the declarative form of the webhook/A2A `into:`
  binding.
- **RFC 0011 exit codes** — a rejected document is exit `2`, like any invalid
  config. There is no partial load.

---

## 9. Security — the part that decides whether this ships

The threat model changes the moment a document can cause execution.

**Provenance.** Extraction is by surface (RFC 0034), but §5 makes that rule
carry real weight, so it becomes checkable: an instruction loaded from a file
agentd read at startup is *operator surface*; anything reaching the instruction
by any other path — a resource-backed instruction that a server can change, a
`config.set`, a peer's message, a tool result — is **not**, and blocks in it are
refused rather than executed. A resource-backed instruction URI may declare only
dialect-1 families unless the operator opts in explicitly per URI.

**Signing.** A document that carries `compute` or `infra` blocks may be required
to be signed:

```yaml
agent:
  document_signature: { required: true, keys: [/etc/agentd/keys/authors.pem] }
```

Unsigned or badly-signed, it does not load. This is the same posture as the
image digest rule one level up: the document is code, so it gets code's
supply-chain treatment.

**Blast radius on grant.** Granting `compute` to an agent that also has
`untrusted_input` in its context and `egress` in its tools is the lethal
trifecta with extra steps. The existing gate already catches the tool-level
fold; this RFC adds the document level to the same computation, so granting
`compute` can turn a booting config into exit `2` — and should.

**What an attacker gets from a compromised document.** Code execution inside a
declared runtime, with that runtime's network policy and mounts, and nothing
else. Not the agent's credentials (they resolve at dispatch, not in the
sandbox), not the store, not the reactor. The sandbox boundary is the security
boundary, which is why `runtime` requires a service and refuses `network: any`.

---

## 10. Phasing

- **A — structure.** Dialect 2: semantic nesting, `@refs`, identity, the
  capability ladder, `--capabilities` reporting. No new executing block. This is
  the whole safety skeleton and is useful alone: `override` ships here, because
  it needs nesting and nothing else.
- **B — material & knowledge.** `file` `data` `media` `asset` `knowledge`
  `index` `source`. Nothing executes; the risk is bounded by the workspace.
- **C — interface, identity & composition.** `endpoint` `ui` `human` `channel`
  `peer` `policy` `secret-ref` `agent`. Listeners, people, and children.
- **D — compute & infra.** `runtime` `function` `test` `fixture` `git` `volume`
  `image`, `agentd --test`, document signing. Last on purpose: everything before
  it is declaration, and this is the rung where a document becomes a program.

Each phase is independently shippable and independently refusable.

---

## 11. Open questions

1. **Does `data` belong in the prompt or only in templates?** A table the model
   can see is useful; a table it sees on every turn is a token cost that grows
   with the document. Lean: template-addressable by default, `context=true` to
   render it.
2. **Should `function` support a non-OCI local tier** (the gated `exec` runner)
   for operators who accept the risk and have no container runtime? Lean: yes,
   behind `security.exec` *and* `compute`, so it takes two independent grants.
3. **Is `test` agentd's job at all**, or should `agentd --test` emit a plan that
   an external runner executes? Lean: agentd runs it, because a test that needs
   a second tool to run is a test nobody runs.
4. **How does a document express *versioned* prose** — a skill that changed
   meaning between releases? Streams solved this for events; documents may need
   the same. Deferred.
5. **Multi-document composition.** RFC 0034's conventional folders already load
   many files; does `@ref` cross file boundaries, and if so what is the
   namespace? Lean: refs are document-local in phase A, with an explicit
   `import` block if a real need appears.
