# RFC 0048: The standards agentd speaks next — a survey and a plan

**Status:** Accepted 2026-10-19 — the user answered §6: build RFC 0044, prompts are not skills, x402 on both EVM and Solana, each RFC its own entry on the strip. §2 is done; the tracks follow §7.
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-10-19
**Decision records:** [ADR 0001](adr/0001-mcp-extensions-client-side-only.md) (MCP extensions: the client side only), [ADR 0002](adr/0002-the-standards-strip-lists-what-ships.md) (the standards strip lists what the release binaries speak), [ADR 0003](adr/0003-no-payment-without-a-spend-policy.md) (no payment without a spend policy)
**Builds on:** RFC 0044 (enterprise-managed authorization), RFC 0045 (MCP Events), RFC 0047 (acting at companies: its client metadata document, `private_key_jwt`, OpenAPI operation tools)

---

## 1. Summary

The user asked, on 2026-10-18, what agentd is missing from the list of
protocols and specifications it supports, what of that it can implement, and
for a small plan. This RFC is that plan. It records what each standard
requires of agentd *as a client* (ADR 0001), what agentd does today, and the
tracks that close the gap, in the order they should be built. Each track gets
its own RFC (or an amendment of an existing one) when it starts; this
document only scopes them.

The facts come from the primary sources, fetched on 2026-10-18: each
standard's own text, at the revision named in §3.

## 2. What already ships and is missing from the list

These need no code, only the strip on agentd.dev and its links (ADR 0002):

| Add or change | Links to | Note |
|---|---|---|
| "Instruction Specification" → **instruction.md** | `/docs/instruction-documents/` | the spec's own name (github.com/instruction-md/specification, Draft 1, revision 1.1) |
| **Workflows** — agentd's own workflow language | `/docs/workflow-language/` | agentd's DSL, marked as agentd's |
| **PID 1** — agentd as a container's or a device's init | `/docs/pid-1/` | a runtime role rather than a protocol, so it sits in the second row |
| **OpenAPI 3.0 · 3.1** | `/docs/act-at-companies/#its-operations-as-tools` | PREVIEW: a company's operation tools (RFC 0047 P5a), in the `companies` preview |
| **The IETF RFCs** agentd implements, each its own entry (Q4) | the OAuth sections of `docs/authentication.md`, and a new "Internet standards" table in `docs/interoperability.md` | OAuth 2.0 (6749, 6750), PKCE (7636), device grant (8628), AS metadata (8414), protected-resource metadata (9728), `iss` (9207), JWT client assertions (7523), revocation (7009), HTTP Message Signatures (9421), X25519 (7748) and JWE (7516), TLS 1.3 (8446); in the preview, DPoP (9449) and resource indicators (8707) |
| **JSON-RPC 2.0**, **JSON Schema 2020-12** | the interoperability sections that use them | the wire and the schemas of MCP, A2A and workflow inputs |

The OpenTelemetry GenAI semantic conventions need nothing: agentd's spans
already map to them (`invoke_agent`, `chat`, `execute_tool`). MCP Events is on
the strip already, as DRAFT (SEP-3415 is an open pull request, not Final).

## 3. The standards, and what agentd does today

| Standard | Revision checked | agentd today | Track |
|---|---|---|---|
| MCP Tasks — extension `io.modelcontextprotocol/tasks` (SEP-2663, Final) | ext-tasks `899c1bbc86`, spec 2026-07-28 | none: no `tasks` capability is declared, so a server's task is never created | **T** |
| Skills over MCP — extension `io.modelcontextprotocol/skills` (SEP-2640, Final) | ext-skills `c1aa87e4c3` | skills found from a server's prompts and from resources whose URI starts with `skill://` — an agentd convention, without `skills/list`, `skills/get` or the digest checks | **S** |
| Agent Skills — `SKILL.md` (agentskills.io, unversioned) | agentskills `69ef37e942` | `skills.dir` reads `<name>/SKILL.md`; the name rules, the optional fields and `.agents/skills/` are not checked against the spec | **S** |
| AGENTS.md (agents.md, Agentic AI Foundation; no formal spec) | agents.md `d001185d79` | not read | **G** |
| OAuth Client ID Metadata Document (draft -02); MCP 2026-07-28 prefers it to dynamic registration | -02, 2026-07-06 | for MCP servers, only an operator-configured `client_id`; the companies preview already publishes a client metadata document and signs `private_key_jwt` | **C** |
| MCP enterprise-managed authorization (Stable) on ID-JAG (draft -04) — Okta's "Cross App Access"; RFC 8693 on-behalf-of | EMA `e5eef546e4`; ID-JAG -04 | designed in RFC 0044 (Proposed); its P0 groundwork partly built by RFC 0047 P0 (RFC 8414, 9207, 7009, `private_key_jwt`) | **E** |
| x402 v2 (x402 Foundation) over HTTP, MCP and A2A | x402 specs `1566e13f3d` | none | **P** |
| AP2 0.2, ACP 2026-04-17, Verifiable Intent 0.1 | as listed in §5.5 | none | **P**, later |

## 4. Out of scope, and why

- **Serving any MCP extension** (ADR 0001). agentd is an MCP client.
- **Visa Trusted Agent Protocol**: its terms are Visa's, not an open licence.
- **Mastercard Agent Pay**: no open specification was found.
- **Agent Client Protocol** (editor ↔ agent), **AG-UI**, **MCP Apps**: each
  puts agentd in a server or UI role the user did not ask for; noted for a
  later look.
- **The MCP Registry (`server.json`)**, **Web Bot Auth**, **AsyncAPI**,
  **CloudEvents**, **UCP**, **NLWeb**, **OASF**: real specifications with no
  agentd use case today.

## 5. The tracks

Each track is client-side, behind the existing feature or a new one, ships
with its docs and conformance checks, and follows the house rules (no new
crate without the user's word, no legacy shims, secrets as references).

### 5.1 T — MCP Tasks (client)

On 2026-07-28 connections, agentd declares `io.modelcontextprotocol/tasks`
on `tools/call` and accepts either answer: a `CallToolResult` as today, or a
`CreateTaskResult`. A task is a durable wait, as an A2A task already is: its
`taskId`, `status`, `ttlMs` and `pollIntervalMs` are kept in the store, polled
with `tasks/get` at the server's interval (or pushed by `subscriptions/listen`
`notifications/tasks`), and resumed after a restart. `completed` hands the
inline `result` to the turn or step that called; `failed` is the tool's error.
`input_required` opens a gate as elicitation already does, answered with
`tasks/update`, every input request key deduplicated across polls. A run
cancelled, or a step timed out, sends `tasks/cancel`. `Mcp-Name` carries the
`taskId` on Streamable HTTP. A `CreateTaskResult` for any other method is an
invalid response. Older revisions (2025-11-25's experimental core tasks) are
not implemented: the extension replaced them.

*Size:* medium. *Depends on:* nothing new. *Recommendation:* build first.
*Built:* [RFC 0050](0050-mcp-tasks.md), 2026-10-19.

### 5.2 S — Skills: the Skills extension and SKILL.md

The client of SEP-2640: `skills/list` and `skills/get` once a server declares
the extension; each file checked against its `digest` and `size`, the
frontmatter compared field by field; a skill named by (server, `uri`)
everywhere; its content tagged with its origin and treated as untrusted;
`allowed-tools` and any frontmatter that widens permissions ignored; the
512-file and 16 MiB minimums accepted. Local skills held to agentskills.io:
the `name` rules, `description`, the optional fields, `.agents/skills/` as a
project scope beside `skills/`, and project skills only from a trusted
configuration. `scripts/` are never run (agentd runs no local code).

The prompts-as-skills and `skill://`-resource discovery agentd uses today are
agentd's own convention. Under the no-legacy rule they go once the extension
lands — a user decision (§6, Q2).

*Size:* small to medium. *Recommendation:* second.
*Built:* [RFC 0051](0051-skills.md), 2026-10-19.

### 5.3 C — Client ID Metadata Document for MCP servers

agentd publishes one client metadata document (the companies preview's,
generalised): `client_id` its HTTPS URL, `client_name`, `redirect_uris`,
`token_endpoint_auth_method: private_key_jwt` with its JWKS — printed for
static hosting (`--print-client-metadata <dir>`) or served by agentd's public
listener. At an MCP server's authorization server, the order MCP 2026-07-28
gives: a pre-registered client first; the document's URL as `client_id` where
the server advertises `client_id_metadata_document_supported`; dynamic
registration is not implemented (MCP deprecates it); else the operator is
told what to register. Stored credentials are keyed by the authorization
server's issuer.

*Size:* small. *Depends on:* a public URL the authorization server can fetch.
*Recommendation:* third; it is also EMA's client identity.
*Built:* [RFC 0052](0052-client-id-metadata-document.md), 2026-10-19.

### 5.4 E — Enterprise-managed authorization: RFC 0044

RFC 0044 already designs it end to end: single sign-on at the enterprise IdP,
the RFC 8693 exchange for an ID-JAG with the MCP authorization server's issuer
as `audience`, the RFC 7523 grant there, no refresh token (the ID-JAG is
presented again, then renewed), the extension declared per request, and the
RFC 8693 on-behalf-of exchange for the same-domain case. This plan proposes
accepting it and building it in its own phases, after C, with its P0 trimmed
to what RFC 0047 P0 did not build. Its open questions are mostly how Okta
behaves, which only a test tenant answers.

*Size:* large. *Recommendation:* fourth, once the user accepts RFC 0044 (§6, Q1).

### 5.5 G — AGENTS.md

AGENTS.md is plain Markdown with no fields: the nearest file to the work
wins. agentd reads it as an instruction source for a workspace an operator
names (`agent.context.agents_md: <dir>`, say): the files from the workspace
root down to the working directory, nearest last, delivered as instruction
text in the prompt's stable part. It grants nothing: AGENTS.md never
configures tools, services or policies (RFC 0042's boundary holds, as for any
served document).

*Size:* small. *Recommendation:* with S, or after it.
*Built:* [RFC 0053](0053-agents-md.md), 2026-10-19.

### 5.6 P — Payments: x402 first, behind a spend policy

A new RFC (0049) before any code, under ADR 0003. Its scope:

- **x402 v2 as a paying client** on the three transports the spec defines:
  HTTP (`PAYMENT-REQUIRED` → `PAYMENT-SIGNATURE` → `PAYMENT-RESPONSE`), MCP
  (a tool result with `isError` and a `PaymentRequired`, retried with
  `_meta["x402/payment"]`), and A2A (the a2a-x402 extension, whose header
  name predates A2A 1.0.1 and needs a note upstream — drafted locally).
- **One scheme and network first**: `exact` with EIP-3009 on an EVM chain
  (USDC), or the Solana binding. EVM signing needs secp256k1 and Keccak-256,
  which `ring` does not have — a crate decision for the user (§6, Q3).
- **The spend policy the spec leaves out**: caps per run, per day and per
  payee; a payee and asset allow-list; the User's approval above a threshold,
  through the gates RFC 0047 P4 built for operations; every payment an audit
  record; a payment whose outcome is unknown is never made twice.
- **Custody**: the wallet key held by the supervisor only, as Account Tokens
  are, sealed at rest.
- **Later**: AP2 mandates (SD-JWT) and ACP checkout as a buyer agent, once
  x402 is in use; Verifiable Intent as AP2's companion.

*Size:* large. *Recommendation:* design now, build after Q3.

## 6. Questions for the user, and the answers (2026-10-19)

1. **Q1 — RFC 0044.** Accept it, and build it after C? — *"accept and
   build"*: RFC 0044 is accepted, and track E builds it.
2. **Q2 — Skills discovery.** When the Skills extension lands, remove the
   prompts-as-skills and `skill://` heuristics (no legacy), or keep prompts as
   a second source? — *"we should not treat prompts as skills"*: track S
   removes them; skills come from the Skills extension and from local
   `SKILL.md` folders only.
3. **Q3 — Payment signing.** Which first network for x402, and may agentd take
   a RustCrypto crate (`k256` and `sha3`) for EVM signatures? The Solana
   binding signs with Ed25519, which `ring` has, but builds Solana
   transactions. — *"both"*: EVM (with `k256` and `sha3`) and Solana, both
   under ADR 0003.
4. **Q4 — The strip's RFCs.** One "IETF RFCs" entry linking to the table, or
   each RFC as its own entry? — *"show separately"*: each RFC is its own
   entry.

## 7. Order

| # | Track | Gate to start |
|---|---|---|
| 0 | the strip and the "Internet standards" table (§2) | none — done with this RFC |
| 1 | T — MCP Tasks | this RFC accepted |
| 2 | S — Skills extension and SKILL.md | Q2 answered: prompts are not skills |
| 3 | C — CIMD | none |
| 4 | G — AGENTS.md | none |
| 5 | E — RFC 0044 | Q1 answered: accepted |
| 6 | P — RFC 0049, then x402 on EVM and Solana | Q3 answered: both |

## 8. References

- MCP Tasks extension: https://github.com/modelcontextprotocol/ext-tasks (SEP-2663)
- MCP Skills extension: https://github.com/modelcontextprotocol/ext-skills (SEP-2640)
- MCP enterprise-managed authorization: https://github.com/modelcontextprotocol/ext-auth
- ID-JAG: https://datatracker.ietf.org/doc/draft-ietf-oauth-identity-assertion-authz-grant/
- RFC 8693: https://www.rfc-editor.org/rfc/rfc8693
- CIMD: https://datatracker.ietf.org/doc/draft-ietf-oauth-client-id-metadata-document/
- Agent Skills: https://agentskills.io/specification
- AGENTS.md: https://agents.md
- instruction.md: https://github.com/instruction-md/specification
- x402: https://github.com/x402-foundation/x402
- AP2: https://github.com/google-agentic-commerce/AP2 · a2a-x402: https://github.com/google-agentic-commerce/a2a-x402
- ACP: https://github.com/agentic-commerce-protocol/agentic-commerce-protocol
- Verifiable Intent: https://github.com/agent-intent/verifiable-intent
