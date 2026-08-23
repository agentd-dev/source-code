# RFC 0037: Service catalog and egress policy

**Status:** Phases A + B implemented — A: the `services:` catalog, `service:` references with narrowing, the unconditional tag floor, `closed` egress over MCP dials + A2A push targets, catalog inheritance into RFC 0036 children, `agentd login service:<name>`, effective-surface output in `--validate-config`; B: all four entry kinds (`mcp`/`intelligence`/`peer`/`http` — kind-filtered matching), `a2a.peers[].service` references, `closed` coverage of intelligence endpoints/peers/the `http` step (+ its `methods:` ceiling)/the HTTP store, per-entry `breaker:` defaults, in-process pacing extended to turn-worker and subagent processes, and `examples/startup/services.yaml` as the reference deployment. Phase C stays contingent — see the implementation notes
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-23
**Part of:** the configuration surface (RFC 0030); hardens the trifecta gate's inputs (RFC 0012 §3); credentials ride RFC 0031; the recommended posture for template-bearing deployments (RFC 0036 §8).

---

## 1. Summary

Everything agentd can *do* to the outside world flows through a declared
endpoint — MCP servers, the intelligence API, A2A peers, the `http` step,
the HTTP store. But the declarations are scattered and on the honor
system: every config restates its servers, names its own secrets, and
asserts its own trifecta tags, and nothing anywhere says "these are the
only services this deployment talks to."

This RFC adds a **`services:` catalog** — a named registry of the external
services a deployment is allowed to use, each with its connection
settings, credentials, authoritative trifecta tags, and a tool-surface
**ceiling** — and an **egress policy** (`security.egress: open | closed`)
that makes the catalog enforceable at dial time:

```yaml
services:
  billing:
    kind: mcp
    endpoint: https://billing.internal/mcp
    auth: {kind: static, token: "{{secret:BILLING_MCP}}"}
    tags: {"*": [sensitive]}            # authoritative — a floor, not a suggestion
    allow: [charge_lookup, refund_create]  # the widest surface ANY consumer may get

mcp:
  servers:
    - {name: billing, service: billing, allow: [charge_lookup]}  # reference + narrow

security:
  egress: closed        # only catalogued endpoints may be dialed
```

The design rule is the house rule: **sugar over existing pipelines, never
a parallel mechanism.** The catalog dials nothing and registers nothing —
`mcp.servers` keeps connecting, the registry keeps admitting tools,
RFC 0031 keeps minting credentials. The catalog is the *authority those
mechanisms consult*: consumers reference entries and may only **narrow**
them, tags matched by endpoint apply as a floor even to inline
declarations, and in `closed` mode an outbound dial to an uncatalogued
endpoint is refused at the socket, not discouraged in review.

## 2. Motivation — three gaps, one section

**Tags are on the honor system.** The lethal-trifecta gate (RFC 0012) is
the deployment's most load-bearing check, and its inputs are
author-asserted: a config — or a hot-reloaded edit, or an RFC 0036
template — can point an MCP server at the billing system and simply not
write `sensitive`. The gate then reasons soundly from a false premise.
Binding endpoint → tags **once**, in a catalog the operator reviews,
makes tag-laundering a validation error instead of a latent incident.

**Dynamic composition has no endpoint ceiling.** RFC 0036 lets templates
define their own `:::mcp` machinery, gated by the trifecta check — but
nothing constrains *where those servers point*. Reviewing a template
means auditing URLs. With a catalog and `closed` mode, a template's
machinery can only draw from the operator's vocabulary, and template
review shrinks to "which catalog names does it use?" — the
narrowing-never-widening discipline, extended from tools to endpoints.

**The fleet repeats itself.** The eleven configs in
[`examples/startup/`](../examples/startup) restate the same servers,
credentials and tags per desk, and the README carries a hand-maintained
prose table of which secret each desk holds — precisely because there is
no machine-checkable inventory. Multi-file config merge already ships
(RFC 0030; RFC 7396 semantics), so one shared `services.yaml` in front of
each desk's config gives the whole fleet a single audit page, a single
place credentials are named, and a single diff when a service moves.

There is precedent in the config for exactly this shape of concern: A2A
push is default-OFF with an explicit `allow_private` escalation because a
peer-chosen URL is the shape of an SSRF. This RFC generalizes that
instinct — outbound reach is a *policy surface*, not an emergent property
of whatever configs accumulated.

## 3. Decision

1. **A new top-level `services:` section** — a map of named catalog
   entries. Phase A defines `kind: mcp`; the namespace reserves
   `intelligence`, `peer` and `http` (Phase B). Entries carry connection
   settings (`endpoint`, `auth`, `headers`, `timeout`), authoritative
   `tags`, a tool-surface ceiling (`allow` / `exclude`), and per-instance
   outbound pacing (`rate`).
2. **Consumers reference, and only narrow.** `mcp.servers` entries gain a
   `service:` field. A referencing entry inherits the catalog's
   connection settings and may not restate them (`endpoint`, `auth`,
   `headers` on a referencing entry are refused). Its effective `allow`
   is the intersection with the catalog ceiling; its `exclude` is the
   union; its `tags` may add, never remove. `ns` and `timeout` stay
   consumer-local.
3. **Catalog tags are a floor, always.** Whenever any outbound MCP
   declaration — referencing or inline — resolves to an endpoint that
   matches a catalog entry, the entry's tags are unioned into its
   effective tag set before the trifecta gate runs. This rule does not
   wait for `closed` mode; it is the tag-laundering fix and it is
   unconditional.
4. **`security.egress: open | closed`** (default `open`). In `closed`
   mode, an outbound dial whose resolved URL matches no catalog entry is
   refused — at dial time, by URL (scheme + host + path prefix), not by
   name — and the refusal is a tool-result/startup error naming the URL
   and the section that would fix it. Redirects are re-checked;
   credential-machinery URLs that an entry's own `auth` implies (its
   token endpoint, its issuer's discovery documents) are part of the
   entry. `open` mode changes nothing except Decision 3.
5. **The catalog is inherited, never extended, downtree.** Subagents and
   RFC 0036 instance-tier children receive the parent's catalog in their
   composed payload. A template's `:::mcp` machinery resolves against it
   under the same rules; in `closed` mode a template may only reference.
   Nothing a child or a model can author adds an entry.
6. **Small configs owe nothing.** `services:` absent → today's behavior,
   exactly, minus nothing. The section is for deployments that have
   earned the ceremony.

## 4. The `services:` section

```yaml
services:
  billing:
    kind: mcp                              # Phase A: mcp only
    endpoint: https://billing.internal/mcp # match base for dial-time checks
    auth: {kind: static, token: "{{secret:BILLING_MCP}}"}   # RFC 0031 Auth
    headers: {X-Env: prod}
    tags: {"*": [sensitive]}               # authoritative floor
    allow: [charge_lookup, refund_create, invoice_*]   # ceiling (registry globs)
    exclude: [refund_bulk]                 # exclude beats allow, as today
    rate: "60/1m"                          # this instance's pacing toward the service
    timeout: 30s

  helpdesk:
    kind: mcp
    endpoint: https://desk.example.com/mcp
    auth: {kind: oauth2, ...}
    tags: {"*": [untrusted_input]}
```

Field semantics:

- **`endpoint`** is both the connection URL for referencing consumers and
  the match base for Decision 3/4: a URL matches an entry when scheme and
  host are equal and the path extends the entry's path. Two entries whose
  endpoints are prefix-comparable are a validation error (matching must
  be unambiguous).
- **`auth`** is the RFC 0031 provider, defined once. Interactive
  providers log in as `agentd login service:<name>`; every referencing
  consumer shares the cached credential. A desk that references `billing`
  never holds `BILLING_MCP` in its own file — the per-config blast radius
  shrinks to the services that config actually uses.
- **`tags`** use the existing tool-pattern → tag-list shape and the
  existing flattening. The catalog asserts them; consumers may add
  stricter ones; nothing removes them.
- **`allow` / `exclude`** are the existing registry globs with the
  existing precedence, evaluated as a ceiling: the effective admitted set
  for a consumer is `(catalog.allow ∩ consumer.allow) − (catalog.exclude
  ∪ consumer.exclude)`.
- **`rate`** paces this *instance's* aggregate calls toward the service
  (one token bucket shared by all of the instance's consumers, riding the
  existing bucket machinery). Cross-instance coordination is explicitly
  out of scope — that is a control-plane concern (RFC 0019), and
  pretending a per-process bucket is a global limit would be a lie in
  config form.

Referencing from `mcp.servers`:

```yaml
mcp:
  servers:
    - name: billing            # the registry name, as today
      service: billing         # the catalog entry
      allow: [charge_lookup]   # narrows the ceiling
      ns: money                # consumer-local, as today
```

`service:` and `endpoint:` on one entry are mutually exclusive. An inline
(non-referencing) server remains legal in `open` mode and is still
subject to Decision 3 if its endpoint matches an entry.

**Fleet layout.** The intended deployment shape for a multi-instance
company is one shared catalog file merged in front of each desk:

```
agentd -c services.yaml -c support.yaml     # every desk, same first file
```

Hot reload classes follow `mcp.servers` (RFC 0017/0030): adding an entry
is reloadable; changing an endpoint or auth takes effect on the next
(re)connect via the existing warm refresh.

## 5. Egress policy

`security.egress: closed` extends enforcement from "declarations must
match the catalog" to "dials must match the catalog":

- **Phase A surface: MCP.** Every outbound MCP connection — configured
  servers, template machinery, store `kind: mcp` backends — is checked at
  dial time. Off-catalog → refused, named, fail-closed.
- **Phase B surfaces:** `intelligence.endpoints` (as `kind:
  intelligence` entries), `a2a.peers` (`kind: peer`), the workflow `http`
  step and the HTTP store (`kind: http` entries with method ceilings).
  Until Phase B, `closed` mode *documents* that these surfaces are
  uncovered rather than silently implying otherwise — `agentd validate`
  says so.
- **A2A push** (caller-chosen webhook URLs) keeps its own default-off
  gate; in `closed` mode a registered push URL must additionally match
  the catalog. Two locks, one door.
- **Escape hatch:** none in-config by design. The way to allow an
  endpoint in `closed` mode is to catalog it — that is the point. (The
  operator who needs a temporary exception edits the catalog, which is
  exactly the reviewable event it should be.)

## 6. Validation (fail-closed, at boot)

Refused with the entry or consumer named: `service:` referencing an
unknown entry; `endpoint`/`auth`/`headers` restated on a referencing
consumer; a consumer `allow` fully outside the catalog ceiling (an empty
effective set is almost certainly a mistake); prefix-ambiguous entry
pairs; unknown `kind`; unparseable tags (existing rule); in `closed`
mode, any inline outbound declaration that matches no entry — reported at
boot for configured surfaces, at dial time for computed ones. `agentd
validate` prints the effective tool surface and tag set per consumer
(catalog ∩ consumer), so review reads the *outcome*, not the inputs.

## 7. Security model

- **The gate's inputs become trustworthy.** Trifecta evaluation consumes
  effective tags (Decision 3), so a config or template cannot launder a
  catalogued endpoint by under-tagging it. The gate itself is unchanged.
- **Model authorship stays at zero.** The catalog joins the set of things
  only operators write: templates (RFC 0036) fill `params`; nothing a
  model emits can add, widen, or re-point an entry. With `closed` mode
  and `subagents.allow_freeform: false`, every capability a child can
  hold traces to two reviewed documents — the template and the catalog.
- **Credential hygiene by construction.** Secrets are named once, in one
  file, with one owner. Rotating a credential is a one-line diff; a
  leaked desk config discloses references, not names of every secret in
  the company.
- **Honest limits.** Dial-time checks bind what *agentd* dials; a
  compromised MCP server can still reach anywhere its host allows —
  network-level egress control (netpol, proxies) remains complementary,
  and the catalog is what makes such rules *derivable* (the entry list is
  the allow-list). DNS-level tricks (a catalogued hostname re-resolving
  elsewhere) are the network layer's problem, and the RFC does not
  pretend otherwise.

## 8. Phases

- **Phase A** — the `services:` section (`kind: mcp`); `service:`
  references with narrowing semantics; the unconditional tag floor;
  `security.egress: closed` for all MCP dials; catalog inheritance into
  subagents and instance children; `agentd login service:<name>`; the
  validation set of §6; `validate` printing effective surfaces.
- **Phase B** — `kind: intelligence`, `kind: peer`, `kind: http` (method
  ceilings for the `http` step and HTTP store); per-entry breaker/health
  defaults shared by consumers; the startup example refactored onto a
  shared `services.yaml` as the reference deployment.
- **Phase C** (contingent) — control-plane-coordinated cross-instance
  pacing (RFC 0019); signed/centrally-distributed catalogs. Neither is
  scheduled.

## 9. Alternatives considered

- **Ceilings directly on `mcp.servers`, no catalog** — fixes narrowing
  but not repetition, gives `closed` mode no vocabulary, and leaves tags
  author-asserted per config. The catalog is the smallest thing that
  fixes all three.
- **Network-layer egress only** — necessary in depth, but it cannot see
  tool names or trifecta tags, does not travel with the config, and
  cannot make `validate` print the effective surface. Config-level policy
  and network policy answer different questions; this RFC makes the
  first derivable into the second.
- **A secrets-manager integration** — orthogonal: RFC 0031 owns *how*
  credentials resolve; this RFC owns *where they are named and who may
  use them*.

## 10a. Implementation notes (Phase B, as shipped)

- **Kind-filtered matching.** An MCP dial matches only `kind: mcp` entries,
  an `http` step only `kind: http`, and so on — one host may serve several
  kinds; ambiguity is judged per kind. Kind-specific vocabulary is
  validated: `allow`/`exclude`/`tags`/`breaker` are mcp-only, `methods`
  http-only; a `peer` endpoint is judged by A2A rules (unix allowed).
- **Pacing now covers every process.** `from_spec` — the one client
  chokepoint in the reactor, each turn worker, and each flat subagent —
  seeds a per-process registry from the spec's carried service+rate, and all
  four dispatch sites (workflow `mcp.tool` steps, reactor mapped tools, the
  worker's in-loop calls, the subagent's in-loop calls) draw from one
  bucket per service per process. Cross-process coordination remains a
  Phase C control-plane concern, as §4 said.
- **Per-entry `breaker:` is a POLICY default, not shared state.** An
  `mcp.tool` step against a referencing server inherits the entry's
  `{failures, cooldown}` when it declares none; breaker STATE stays keyed
  per workflow/step (the existing semantics — a breaker measures one step's
  calls). Gate and recorder resolve the policy through one function, so
  they cannot disagree.
- **The `http` step is enforced at two points**: literal URLs at load,
  templated URLs at execution — plus the `methods:` ceiling in either
  egress mode. HTTP-store ops that do not build on `{base_url}` are judged
  at load too. The one surface deliberately outside `closed` is
  `observability.otel.endpoint` (telemetry export is operator plumbing);
  validation says so instead of implying coverage.
- **`kind: intelligence` coverage skips `mock:`** (the in-process test
  endpoint has no socket). A catalog-referencing peer's credential caches
  under `service:<entry>` — which also makes `agentd login` work for peers
  for the first time (standalone peers still cache under `a2a:<name>`,
  which `login` does not mint; catalog them to log in).

## 10. Implementation notes (Phase A, as shipped)

- **`rate:` paces this process's workflow `mcp.tool` steps.** A dry bucket is
  a step *failure* (a refusal the workflow's `retry:` absorbs), never a hang.
  Two honest bounds: model-initiated MCP calls run inside per-turn worker
  processes and are not paced in Phase A, and rate changes take a restart
  (the buckets are process-lifetime, the spawn-bucket precedent). §4's
  "shared by every consumer of the entry in this process" is exactly what
  shipped, no more.
- **Login canonicalization**: `agentd login mcp:<name>` on a referencing
  server canonicalizes to `service:<entry>` (as does `--logout`), so the
  cached credential always lands under the key the connect path reads.
- **Dial-time enforcement** is boot validation for configured servers and
  template machinery, a startup-loop backstop before any socket, and a
  registration-time check for A2A push targets. The `closed`-mode uncovered
  surfaces (§5 Phase B) are named in a validation warning, not implied away.

## 11. Cross-references

RFC 0012 (trifecta gate — consumes effective tags) · RFC 0017/0030
(config schema, merge, reload classes) · RFC 0019 (cross-instance
coordination, Phase C) · RFC 0027 (`http` step, Phase B) · RFC 0031
(credential providers, `agentd login`) · RFC 0036 (templates; `closed`
catalogs as the recommended posture for template-bearing deployments).
