# A shared-inbox concierge that knows which emails aren't its to answer

> **Trigger:** inbound-email webhook · **Pattern:** classify → category + confidence gates → send / pause / drop · **Sample:** [`examples/use-cases/inbox-concierge.toml`](../../examples/use-cases/inbox-concierge.toml) · **Status:** runs today (`intel-remote,schema,tools-http-tls`); multipart ingestion is a named gap

## The problem

Every `support@`, `hello@`, and `info@` inbox is the same sediment:
order-status questions with answers in the FAQ, refund requests that
follow one procedure, the occasional partnership email that actually
matters, and spam. A human triages it between real tasks, which means
either slowly or resentfully — usually both. Auto-responders made it
worse ("we received your email!"); naive AI auto-reply makes it
dangerous: the bot that cheerfully "resolves" a furious customer, or
answers a phishing probe with account details.

The job isn't "answer email with AI." It's **sort email by what kind of
answer it deserves** — and let the AI fully handle only the kind it
provably handles well.

## What the agent does

Inbound mail arrives as a webhook (SES / SendGrid inbound-parse,
relayed as JSON):

1. One schema-enforced LLM step classifies the category
   (`order-status | refund | partnership | spam | other`), drafts a
   reply in your voice, and rates its confidence — with the prompt rule
   that anything ambiguous, or anything needing account access, is
   `low`.
2. **Spam terminates.** Not "reply politely declining" — terminate, no
   send, no tokens spent arguing. A declared dead end is a feature.
3. Everything else funnels through the confidence gate:
   - **high** → the reply sends through your mail provider's API.
   - **low** → the run checkpoints; the inbox owner reads the draft,
     edits in place or not, and resumes to send. The partnership email
     never gets a bot answer — it gets a human with a head start.

## The two-gate trick

One model call powers two *independent declared gates* — category and
confidence. That's the difference between a classifier and a policy:
"refunds can auto-send, partnerships never do" is two edges; "we got
burned, route refunds to humans for a while" is a one-line change with
a git history. The model's judgment feeds the gates; it doesn't get to
*be* the gates.

```toml
[[edges]]
from = "by_category"
when = "spam"
to = "drop_spam"        # terminate — silence is the correct reply

[[edges]]
from = "gate"
when = "low"
to = "owner_review"     # checkpoint until a person owns the send
```

## Honest limits

- **Multipart ingestion.** Inbound-parse services post
  `multipart/form-data`; agentd's HTTP trigger parses JSON bodies. A
  thin relay (or your provider's JSON mode) bridges it today; native
  form/multipart parsing is the proposal in
  [gap analysis §4](GAP-ANALYSIS.md#4-form-encoded-webhook-bodies).
- **Threading.** This triages message-by-message. Holding a multi-turn
  conversation thread is real memory work — today the pattern is
  "include the thread in the webhook payload"; proper conversational
  state is control-plane-roadmap territory.
- Attachments are the usual
  [document gap](GAP-ANALYSIS.md#7-document-parsing-pdf--docx-guidance):
  parse upstream or via an MCP server.
