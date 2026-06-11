# Business automation, the bounded way — a use-case catalog

Fourteen working patterns for putting an AI agent on real business
work: phones, leads, tickets, invoices, incidents, contracts, hiring,
orders, audits, inboxes, and ledgers. Each is a short article for a
general audience plus a **validated sample workflow** you can read in
one sitting and run with your own keys.

They all share one architecture, and it's the reason any of them is
deployable: **the model contributes judgment inside a single bounded
step; everything that acts — routes, sends, executes, pays — is a
declared graph under a fail-closed policy.** The AI can be wrong;
what it can *do* about it is enumerated before anything runs. Human
sign-off is a durable checkpoint where it belongs, not a hope.

## The catalog

| Use case | Trigger | The pattern in one line | Status |
|---|---|---|---|
| [AI voice receptionist](voice-receptionist.md) | Twilio webhook | speech → intent → declared routing → native TwiML reply; the caller can't talk the graph into anything | Runs today, end to end (v1.2.0: `respond` + form bodies + basic auth) |
| [Lead deep-research](lead-enrichment.md) | CRM webhook | bounded `agent_loop` investigates allowlisted sources → brief lands in CRM | Runs today |
| [Support triage](support-triage.md) | Helpdesk webhook | classify + draft → confidence gate → send or human review | Runs today |
| [Invoice approval](invoice-approval.md) | Watched folder | schema'd extraction → threshold gate → book or controller sign-off | Runs today |
| [Executive digest](exec-digest.md) | Cron 07:00 | metrics → narrative → Slack + dated archive; zero inbound surface | Runs today |
| [Churn early-warning](churn-monitor.md) | Cron weekly | score → structural diff vs last week → alert only on movement | Runs today |
| [Content localization](content-localization.md) | Watched folder | parallel fan-out per locale → all-or-nothing join → editor's veto | Runs today |
| [Incident copilot](incident-copilot.md) | Alert webhook | read → hypothesize → post → human resume → one argv-pinned mitigation | Runs today |
| [Contract review](contract-review.md) | Watched folder | playbook clause extraction → deviations pause for counsel | Runs today |
| [Resume screening](resume-screening.md) | ATS webhook | rubric scoring; advances flow, **every decline needs a human** | Runs today |
| [Order fraud review](fraud-review.md) | Order webhook | three risk lanes: fulfill / queue / hold; "why" attached to each | Runs today; [idempotency gap](GAP-ANALYSIS.md#5-idempotency-keys--exactly-once-effect) named |
| [Compliance evidence](compliance-evidence.md) | Cron monthly | pinned posture check → control mapping → signed-process evidence bundle | Runs today |
| [Inbox concierge](inbox-concierge.md) | Email webhook | category + confidence gates → send / pause / drop spam silently | Runs today; [multipart gap](GAP-ANALYSIS.md#4-form-encoded-webhook-bodies) named |
| [Data reconciliation](data-reconciliation.md) | Cron nightly | declared SQL via MCP → structural diff → LLM explains drift only | Runs today |

**"Runs today"** means: the sample validates against the real binary in
CI, and executes on a build with the listed Cargo features — most need
`intel-remote` (hosted models), `schema` (enforced outputs), and
`tools-http-tls` (HTTPS to SaaS APIs, new in v1.1.0). Where a use case
hits a genuine runtime gap, the article says so and links the
[gap analysis](GAP-ANALYSIS.md) — which is the honest map of what's
missing and what we propose to do about it.

## How to read these

Each article runs the same arc:

1. **The problem** — why this work is painful and why naive automation
   (scripted *or* AI-freeform) fails at it.
2. **What the agent does** — the steps, concretely.
3. **Why you can trust it** — the part that's usually hand-waved:
   which structural property (policy allowlist, schema enforcement,
   durable human gate, declared argv, budget) forecloses which failure.
4. **Honest limits** — what this doesn't do, and whether that's a gap
   (linked) or a design choice (defended).

## Run one in five minutes

```bash
# Build with the capability set the catalog uses
cargo build --release -p agentd \
  --features "intel-remote,schema,tools-http-tls,trigger-cron,trigger-fs-watch"

# Validate any sample (no keys needed)
agentd --config examples/use-cases/support-triage.toml --validate-only

# Walk a graph with every side effect stubbed
agentd --config examples/use-cases/churn-monitor.toml --mode once \
       --start weekly --dry-run

# Run for real: export your keys, then
ANTHROPIC_API_KEY=… agentd --config examples/use-cases/exec-digest.toml
```

New to agentd entirely? Start with the [quickstart](../quickstart.md),
then come back and pick the use case that hurts most.
