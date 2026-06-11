# Capability gap analysis — what the use-case catalog demanded

Building the [fourteen use cases](README.md) against the real runtime
was a deliberate stress test: every place an article had to say "today
you need a workaround" is recorded here, with a concrete proposal and a
size. This is the catalog's engineering exhaust — and the honest answer
to "can agentd do X?" for the business-automation domain.

**Status legend:** ✅ closed · 🔶 proposed, sized, accepted direction ·
📘 guidance (deliberately out of runtime scope — the architecture
answer is documented instead).

**Scoreboard:** the catalog shipped 2026-06-11 with one gap closed
(§1, v1.1.0). The v1.2.0 wave closed four more — §2, §3, §4, §5 and
§9 — in the order this document recommended. §6 (secrets + OAuth2) is
the next planned wave; §7/§8 remain deliberate architecture guidance.

| # | Capability | Blocked / shaped which use cases | Status |
|---|---|---|---|
| 1 | Outbound HTTPS (`tools-http-tls`) | **all 11 SaaS-touching cases** | ✅ shipped v1.1.0 |
| 2 | Webhook response shaping (`respond` node) | voice (TwiML), Slack ack formats | ✅ shipped v1.2.0 |
| 3 | Fan-out over dynamic lists (`map` node) + array-index paths | churn per-account, localization, contract clauses | ✅ shipped v1.2.0 |
| 4 | Form-encoded / multipart webhook bodies | voice (Twilio), inbox (inbound-parse) | ✅ shipped v1.2.0 |
| 5 | Idempotency keys (exactly-once effect) | fraud review, any retried webhook | ✅ shipped v1.2.0 |
| 6 | Secrets providers + OAuth2 refresh | every SaaS API beyond static tokens | 🔶 next wave |
| 7 | Document parsing (PDF / DOCX) | invoices, contracts, resumes | 📘 MCP / upstream |
| 8 | Realtime + streaming I/O (websockets) | full-duplex voice | 📘 sidecar pattern |
| 9 | HTTP basic auth scheme | Twilio webhook auth option | ✅ shipped v1.2.0 |

---

## 1. Outbound HTTPS — ✅ closed in v1.1.0

**The finding.** Eleven of fourteen use cases terminate in a SaaS API —
Slack, CRM, helpdesk, shop, ATS, mail — and every one of those is
HTTPS-only. `http_request` was plaintext-only, which made the entire
catalog aspirational on contact.

**What shipped.** The `tools-http-tls` feature routes `https://` URLs
through ureq + rustls — the client stack `intel-remote` already
carried, so no new dependency tree and no async runtime. Full parity
with the plaintext path (policy allowlist, 1 MiB caps, non-2xx →
`error` branch, traceparent, dry-run), plus one deliberate posture
choice: **redirects are never followed** — the allowlist vetted the
exact URL, so a `Location` hop to an unvetted host surfaces as a 3xx
instead of being followed silently. Real-handshake round-trip tests
back it.

This is the pattern this document wants to repeat: the use cases name
the gap, the gap closes structurally, the articles upgrade from
"workaround" to "runs today."

## 2. Webhook response shaping — the `respond` node — ✅ closed in v1.2.0

**The finding.** A webhook reply from agentd is always the engine's
outcome JSON (`{status, final_value, …}`, 200/202/422/5xx). Callers
that *act on the response body* — Twilio expects TwiML XML, Slack slash
commands expect a specific JSON shape, some webhook providers verify a
challenge echo — can't be answered directly. The voice receptionist
needed a thin "TwiML bridge" service for exactly this.

**Proposal.** A terminal `respond` node that shapes the HTTP reply for
http-triggered runs:

```toml
[[nodes]]
id = "answer"
type = "respond"
status = 200
content_type = "text/xml"
body_template = """
<Response><Say>{{reply}}</Say><Gather input="speech" action="/voice/turn"/></Response>
"""
input_from = "classify.parsed"
```

**As shipped**, one deliberate refinement to the sketch: `respond` sets
the reply's *shape*, not its *timing* — the reply is written when the
run completes, and a run that ends `Failed`/`TimedOut` ignores it (a
failure can't masquerade as a clean answer). Early-ack-then-continue
would have needed a mid-run write channel through the engine; the
actual use cases (TwiML, Slack shapes, challenge echoes) all want "the
whole response is the answer," so the simpler, more bounded semantics
won. **It deleted the bridge from the voice use case** — see the
rewritten [voice article](voice-receptionist.md).

## 3. Fan-out over dynamic lists — the `map` node — ✅ closed in v1.2.0

**The finding.** `parallel` fans out over **declared** branches —
perfect for "our four locales," wrong for "whatever accounts the export
contains." Three articles hit it: churn wants per-account scoring,
contract review wants per-clause fan-out at playbook scale,
localization wants config-driven locale lists. The deeper primitive is
also missing: `resolve_path` can't index arrays (`items.0.id`), so
even *addressing* the third result positionally needs a workaround.

**Proposal.** Two pieces, smallest-first:

1. **Array-index context paths** (`results.2.label`) in
   `resolve_path` / `walk_path` — small, already on the roadmap,
   unblocks manual list handling immediately.
2. A **`map` node**: run one sub-workflow per element of a
   context-resolved array, bounded by a mandatory `max_items` (the
   validator refuses an unbounded map), concurrency capped, joining
   `{results: […], ok}` like `parallel`:

```toml
[[nodes]]
id = "score_each"
type = "map"
items_from = "accounts.parsed"
workflow = "score-account.toml"
max_items = 500          # required — the bound is the point
max_concurrent = 8
```

Budgets stay process-wide (a map can't out-spend its envelope). This is
the single highest-leverage substrate addition for business automation:
"for each X, do the bounded thing" is half the genre.

## 4. Form-encoded webhook bodies — ✅ closed in v1.2.0

**The finding.** The HTTP trigger parses JSON bodies (or empty). Twilio
posts `application/x-www-form-urlencoded`; SendGrid/SES inbound-parse
post `multipart/form-data`. Both currently need a relay that re-posts
as JSON — a real piece of infrastructure for what is, semantically, a
parsing concern.

**Proposal.** Content-type-aware body parsing in the trigger:
urlencoded → flat JSON object (string values); multipart → fields as
JSON + attachments **dropped with an audit note** in v1 (attachment
handling is the document-parsing question, §7). Fail-closed: unknown
content types still 400. Small, self-contained, kills two relays in
the catalog.

## 5. Idempotency keys — exactly-once effect — ✅ closed in v1.2.0

**The finding.** Webhook providers deliver at-least-once; today the
duty to dedupe lands on the downstream API ("make fulfill idempotent on
order_id"). Correct, standard — and exactly the kind of duty a bounded
runtime should own at its boundary. The fraud-review article documents
the workaround in production terms.

**Proposal** (promoted from the roadmap's scale-out section, because
single-node deployments want it too): an optional per-route key —

```toml
[[http_routes]]
path = "/orders/created"
idempotency_key = "trigger.order.id"   # or "body_sha256"
```

— checked against the run-state store (`--state-dir`, which durable
execution already requires): seen key → replay the recorded outcome
(200, same body), don't re-execute. Retention window configurable.
At-least-once delivery collapses to exactly-once *effect* at the
workflow boundary, per route, opt-in.

## 6. Secrets providers + OAuth2 refresh

**The finding.** Every SaaS API in the catalog authenticates; today
that means long-lived tokens in env vars (`api_key_env`,
`tokens_env`) — fine for webhook secrets and Anthropic keys, strained
for OAuth2 APIs (Salesforce, Google) whose tokens expire hourly.
Workaround: a sidecar (Vault Agent template, cloud secret CSI) refreshes
the env/file out-of-process — works, and hot-reload re-reads on SIGHUP.

**Proposal** (already on the [roadmap](../ROADMAP.md)): pluggable
secret *sources* resolving into the same env-style indirection at load
time, behind a feature — and an `oauth2-client-credentials` source as
the first non-trivial provider (fetch + cache + refresh-before-expiry
in-process). Secrets stay out of workflow TOML, Debug impls, and audit
logs — those invariants don't move.

## 7. Document parsing (PDF / DOCX) — 📘 guidance

Invoices, contracts, and resumes arrive as PDFs. Parsing them is a
heavy, churning dependency tree (or an OCR service) that would bloat
the single-binary appliance and its threat surface — so it stays out of
the runtime **deliberately**. The supported patterns, in order:

1. **Upstream extraction** — the scanner/email pipeline drops text +
   metadata; the workflow reads text (what the samples model).
2. **A document-parsing MCP server** — `call_mcp_tool` with the same
   allowlist posture as the database example; the parser's dependencies
   live in the parser's process.
3. For image-grade input, a vision-capable model behind `intel-remote`
   reading a file your pipeline staged.

Not a roadmap item; an architecture answer.

## 8. Realtime / streaming workloads — 📘 architecture guidance

Full-duplex voice (Twilio Media Streams ↔ OpenAI Realtime-class
models), live transcription, token-streaming UIs: these are
long-lived-socket workloads, and a bounded request/response engine
should not pretend otherwise — a websocket pump inside the engine would
erode exactly the run-bounded guarantees the rest of this catalog
depends on.

**The supported architecture:** a small streaming **sidecar** owns the
socket and the audio; agentd remains the **decision plane** — the
sidecar calls the workflow's HTTP route at decision points (intent
formed, call ended, escalation wanted) and gets governed, audited,
policy-bounded answers. The voice article shows the split. If a
first-party sidecar reference implementation proves popular, it belongs
in `examples/`, not in the engine.

## 9. HTTP basic auth — ✅ closed in v1.2.0

Twilio can't set custom headers but supports basic auth in webhook
URLs. agentd has bearer / HMAC / mTLS / OIDC — adding `basic` (RFC
7617, constant-time compare, same `tokens_env` plumbing as bearer) is a
contained addition that removes the last auth friction for
telephony-style callers. P3 because bearer-capable relays exist.

---

## Reading the table strategically

The recommended sequence — voice native (§2+§4+§9), then `map` (§3),
then hardening (§5) — **is exactly what the v1.2.0 wave shipped**, each
with tests, docs, and an upgraded article as the proof. What remains:
§6 (secrets providers + OAuth2 refresh) is the next planned wave; §7
and §8 stay architecture guidance on purpose. This document's job now
is to stay honest the same way the maturity page does: when a use case
hits a wall, the wall gets a number here.
