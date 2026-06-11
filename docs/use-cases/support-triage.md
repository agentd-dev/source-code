# Support triage where the AI drafts and humans keep the veto

> **Trigger:** helpdesk webhook · **Pattern:** classify → confidence gate → send or pause · **Sample:** [`examples/use-cases/support-triage.toml`](../../examples/use-cases/support-triage.toml) · **Status:** runs today (`intel-remote,schema,tools-http-tls`)

## The problem

Support teams don't drown in hard tickets; they drown in the hundred
easy ones between each hard one. "How do I reset my API key" deserves a
fast, correct, warm answer — it does not deserve twenty minutes of a
senior engineer's queue time. But full auto-reply is how you get the
other failure: a confidently wrong bot answer on a P1, screenshotted,
viral.

The question every support lead actually asks: *can the AI answer the
easy ones and reliably know which ones aren't easy?*

## What the agent does

1. The helpdesk webhooks every new ticket to the workflow.
2. One schema-enforced LLM step does three jobs at once: classifies
   severity (`p1|p2|p3`) and topic, drafts a reply in your voice, and —
   the load-bearing part — rates its own **confidence** as `high` or
   `low`. The schema makes that a forced binary, not a hedge.
3. A `switch` on confidence routes the graph:
   - **high** → the reply posts to the ticket immediately.
   - **low** → the run **checkpoints and stops** (`pause_for_approval`).
     A human reads the draft — in the helpdesk, or via `agentd inspect`
     — edits if needed, and resumes the run to send.
4. Every classification, draft, and decision lands in the audit log.

## The confidence gate is the product

Two design choices make this deployable where naive auto-reply isn't:

- **The gate is a declared edge, not a vibe.** "When confidence is low,
  a human approves" is written in the workflow file — reviewable in a
  pull request, signable, and impossible for the model to route around.
  The model can be wrong about its confidence; it cannot *act* on that
  wrongness beyond producing a draft a human reads.
- **The pause is durable.** A paused run is a checkpoint on disk with a
  run id, not a thread blocking in memory. Restart the daemon, come
  back Monday — `--resume RUN_ID` picks up exactly where it stopped.
  Human-in-the-loop that survives a deploy.

```toml
[[edges]]
from = "gate"
when = "high"
to = "reply_url"        # straight to the customer

[[edges]]
from = "gate"
when = "low"
to = "human_review"     # checkpoint; a person owns the send
```

## Turning the dial as trust grows

Day one, you might route *everything* through `human_review` — the
agent as a drafting assistant. After a month of audit logs showing the
`high` drafts were shippable, flip the `high` edge to post directly.
After a quarter, maybe `p3 + high` skips review and `p1` never does.
Each step is a one-line edge change with a paper trail — and the
[conformance suite](../CONFORMANCE.md) can hold a regression bar over
the classifier (`min_pass_rate`) so a model update that degrades triage
fails CI instead of failing customers.

## Honest limits

- Replies post via the helpdesk's HTTPS API (`tools-http-tls`). If your
  helpdesk only does email-out, pair this with the
  [inbox concierge](inbox-concierge.md) pattern instead.
- Retrieval is the next increment: wiring a knowledge-base MCP server
  (`read_mcp_resource`) into the draft step grounds answers in your
  docs. The pattern doesn't change — one more bounded read before the
  same gate.
