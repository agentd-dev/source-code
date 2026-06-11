# Order fraud review in three lanes — and what webhook retries teach us about honesty

> **Trigger:** order webhook · **Pattern:** risk score → three-lane switch → fulfill / queue / hold · **Sample:** [`examples/use-cases/fraud-review.toml`](../../examples/use-cases/fraud-review.toml) · **Status:** runs today (`intel-remote,schema,tools-http-tls`); idempotency is a named gap

## The problem

Every order is a race between two costs: hold a good order and you
annoy a customer and slow revenue; release a bad one and you eat the
chargeback. Pure-rules engines age badly (fraudsters read your rules
faster than you write them); pure-ML scores are opaque exactly when the
fraud team asks "why did this one get through?"; manual review of
everything doesn't survive your first good sales day.

What fraud teams actually run is **lanes**: obvious-fine flows,
obvious-bad stops, ambiguous gets a human. The question is what decides
the lane, and whether you can explain it on Thursday.

## What the agent does

The order webhook arrives (HMAC-verified, rate-limited), and within a
30-second budget — an order is waiting:

1. One schema-enforced LLM step weighs the signals fraud analysts
   actually use — billing/shipping mismatch, value vs account age, rush
   shipping on resellables, disposable email — and must commit to
   `{band: low|medium|high, signals: "…in plain language"}`.
2. A `switch` routes the lane:
   - **low** → `POST /fulfill`. Ship it.
   - **medium** → the run **checkpoints into the fraud queue**. An
     analyst resumes to fulfill, or kills it to refund.
   - **high** → `POST /hold`, alert the channel. *No automatic
     fulfillment path exists from this lane* — not as policy, as graph
     shape: there is no edge from `high` to `fulfill`.
3. The `signals` field rides along to the shop API and the alert — the
   "why" is attached to every decision at decision time, not
   reconstructed for the chargeback dispute.

## Why the lanes belong in the graph

The LLM contributes judgment; the **lanes are declared edges**. That
split is what makes the system tunable under pressure: after a bad
week, tightening "medium" to start at a lower threshold is a prompt
edit reviewed in a pull request — the routing, the queue, the audit
trail don't move. The model can be replaced entirely (or A/B'd via a
second [backend](../capabilities.md)) without touching the lanes.

## The honest part: webhooks lie about "once"

Every webhook provider redelivers — timeouts, retries, at-least-once
semantics. Deliver the same order twice to a naive automation and it
fulfills twice. Today, this workflow's answer is the industry-standard
one: **the fulfill endpoint must be idempotent on `order_id`** —
absorbing the duplicate downstream.

That works, but it's a duty the runtime should take on: an
**idempotency key** on the trigger (order id or content hash), deduped
in the run-state store, making redelivery collapse to exactly-once
*effect* at the workflow boundary. It's a named proposal in the
[gap analysis §5](GAP-ANALYSIS.md#5-idempotency-keys--exactly-once-effect),
and e-commerce is the use case that earns it the priority.

## Honest limits

- The LLM sees what the webhook carries. Velocity features ("third
  card on this device today") live in your fraud data platform — fetch
  them with an allowlisted `http_request` before scoring, same pattern.
- Sub-second decisioning at checkout is a different sport; this design
  reviews orders post-placement, pre-fulfillment, where a 5-second
  budget is generous.
