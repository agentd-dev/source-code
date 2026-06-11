# Capability gap analysis — what the use-case catalog demanded

Building the [fourteen use cases](README.md) against the real runtime
was a deliberate stress test: every place an article had to say "today
you need a workaround" is recorded here, with a concrete proposal and a
size. This is the catalog's engineering exhaust — and the honest answer
to "can agentd do X?" for the business-automation domain.

**Status legend:** ✅ closed · 🔶 proposed, sized, accepted direction ·
📘 guidance (deliberately out of runtime scope — the architecture
answer is documented instead).

| # | Capability | Blocked / shaped which use cases | Status |
|---|---|---|---|
| 1 | Outbound HTTPS (`tools-http-tls`) | **all 11 SaaS-touching cases** | ✅ shipped v1.1.0 |
| 2 | Webhook response shaping (`respond` node) | voice (TwiML), Slack ack formats | 🔶 P1 |
| 3 | Fan-out over dynamic lists (`map` node) + array-index paths | churn per-account, localization, contract clauses | 🔶 P1 |
| 4 | Form-encoded / multipart webhook bodies | voice (Twilio), inbox (inbound-parse) | 🔶 P2 |
| 5 | Idempotency keys (exactly-once effect) | fraud review, any retried webhook | 🔶 P2 (roadmap, promoted) |
| 6 | Secrets providers + OAuth2 refresh | every SaaS API beyond static tokens | 🔶 P2 (roadmap) |
| 7 | Document parsing (PDF / DOCX) | invoices, contracts, resumes | 📘 MCP / upstream |
| 8 | Realtime + streaming I/O (websockets) | full-duplex voice | 📘 sidecar pattern |
| 9 | HTTP basic auth scheme | Twilio webhook auth option | 🔶 P3 |

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

## 2. Webhook response shaping — the `respond` node

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

Semantics: only meaningful on an http-triggered run (validator enforces
reachability only from http start nodes); at most one fires per run;
the run continues afterward if edges exist (respond-then-keep-working),
mirroring how webhook handlers ack fast and continue. Non-http triggers
treat it as a no-op pass-through. Sized: a node kind + trigger plumbing
for an early-write channel + validator rule + docs/tests — comfortably
one focused PR. **Deletes the bridge from the voice use case.**

## 3. Fan-out over dynamic lists — the `map` node

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

## 4. Form-encoded webhook bodies

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

## 5. Idempotency keys — exactly-once effect

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

## 9. HTTP basic auth — small

Twilio can't set custom headers but supports basic auth in webhook
URLs. agentd has bearer / HMAC / mTLS / OIDC — adding `basic` (RFC
7617, constant-time compare, same `tokens_env` plumbing as bearer) is a
contained addition that removes the last auth friction for
telephony-style callers. P3 because bearer-capable relays exist.

---

## Reading the table strategically

Close §2 (`respond`) and §4 (form bodies) and the **voice use case goes
end-to-end native** — the showcase with the most "wow" per line of
TOML. Close §3 (`map`) and the catalog's three list-shaped cases
upgrade, plus most unwritten ones ("for each row…"). §5 and §6 are the
production-hardening pair. That ordering — voice native, then map, then
hardening — is the recommended sequence, and the roadmap now reflects
it.
