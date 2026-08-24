# The two-person company

*A use case, worked all the way through: an eleven-agent software company
whose only employees are the CEO and CTO — why it is shaped the way it is,
what each design decision buys, and what a technical leader should actually
take from it. The complete, validated configuration lives in
[`examples/startup/`](https://github.com/agentd-dev/source-code/tree/main/examples/startup);
this page is the reasoning behind it.*

---

## The claim, stated carefully

A small SaaS business generates a surprising amount of work that is neither
strategy nor invention: triaging tickets, chasing failed payments, verifying
fixes on staging, posting the changelog, qualifying inbound leads, updating
the status page at 3am, writing Monday's standup. None of it is optional,
none of it is why the founders started the company, and nearly all of it has
the same shape — **an event arrives, judgment is applied, an action follows,
and someone must be able to prove later what happened**.

That shape is automatable today. Not with one heroic "do everything" agent —
with a *company* of narrow ones: each with a written job description, a
bounded set of tools, a budget, an audit trail, and a boss. The example this
page walks through runs eleven agentd instances — support (tickets), a
front-line SMS/voice desk, a back office, engineering, QA, SRE, sales,
finance, marketing, an outbox, and a chief of staff — and keeps exactly two
humans: the CEO and the CTO, who between them hold every decision that
spends money, ships to production, or abandons revenue.

What this page is **not** is a claim that the judgment is free. Every agent
here runs on a frontier model doing real reasoning, and the design question
worth studying is not "can a model answer a ticket" — it obviously can — but
what surrounds the model so that ten thousand tickets, dunning cycles, and
incidents later, the company is still coherent, still auditable, and still
safe. That is an architecture problem, and it is the interesting part.

## Why eleven small agents beat one large one

The obvious design is a single agent with every tool the company owns. It
fails three different ways at once.

**It fails as security.** An agent that reads customer-authored text
(tickets, lead forms, phone transcripts), holds sensitive stores (the books,
the account records, the codebase), *and* can send email is a
prompt-injection kill chain assembled and loaded: a hostile ticket instructs
the model, the model reads the sensitive store, the mail tool exfiltrates
it. agentd refuses to boot that combination — the [lethal-trifecta
gate](security.md) rejects any instance whose MCP servers together hold
`untrusted_input` + `sensitive` + `egress`. This is the single most
load-bearing fact in the whole example, because it means **the org chart is
not a metaphor — it is the security architecture**. Two of the eleven
instances exist *because* the gate refused the alternative:

- The **back office** holds the account records and the service logs and
  nothing else — no webhooks, no mail, no customer text. The desks that talk
  to customers query it through two typed A2A commands and get back minimal
  structured facts, never raw records. The residual risk (what it returns
  does flow onward to customers) is written down next to the mitigation (its
  contract answers the question asked, nothing more) instead of being
  discovered in an incident review.
- The **outbox** is the company's one mouth. Every email, every LinkedIn, X,
  or Instagram post, every dunning notice leaves through this one instance,
  which holds egress and *nothing else*. That buys three things no policy
  document can: the trifecta stays split by construction rather than by
  promise; there is one durable, replayable ledger of everything the company
  ever said (`outbox`'s `sent` stream); and the company's voice has one
  plug to pull.

**It fails as operations.** One agent is one failure domain, one token
budget, one context window absorbing every concern at once. Eleven instances
restart independently, are budgeted independently (the config gives
engineering 4M tokens a day and the outbox 300k — that line item *is* the
payroll), and can be paused, redeployed, or downgraded one desk at a time.

**It fails as management.** A job description you can review is a
`agent.instruction` field; a tool grant you can audit is an `mcp.servers`
list. When each instance is one role, the diff that changes what "sales" is
allowed to do is three lines in one file, reviewed like any code change.
When everything is one agent, every change is a change to everything.

## The nervous system: events in, judgment, action out

Every desk in the example is driven by the same loop, implemented by
different mechanisms at each edge:

**Events arrive as webhooks.** The helpdesk posts new tickets, GitHub
Actions posts CI verdicts, the monitoring stack posts alerts firing *and*
resolving, Stripe posts payment outcomes, the website posts leads, Twilio
posts SMS and call transcripts. Every route is HMAC-authenticated — the
question "is this really Stripe?" is answered by cryptography, not by the
model. Structured payloads are checked with a `validate` step (a JSON-schema
gate that costs zero tokens), and only *prose* goes through model-based
extraction.

**Long-running processes park on signals.** The pattern that makes
month-long business processes cheap: a run does its work, then suspends —
durably — until the world answers. An incident run parks on
`resolved/<alert_id>` until the all-clear webhook fires it. A sales deal
parks up to four days on `reply/<lead_id>`, nudges once, parks seven more.
The park costs nothing while suspended and survives every restart, and the
deadline is an *expected branch*, not an error: `on_timeout:` routes the
incident to "page the CTO" and the cold lead to "close the file honestly".
The relay from webhook to signal is one field on the webhook start —
`signal: "resolved/{{ body.alert_id }}"`.

**Desks call each other with typed commands.** Support files a bug with
engineering as `command: eng.bug, args: {ticket, severity, summary}` — a
data part the receiving workflow matches deterministically, validated
against the command's declared schema *at the listener*, so a malformed
report is refused synchronously with the field named, not discovered three
steps into a run. The same call, made as prose, would reach the peer's
*model* and depend on its mood. The example keeps prose for exactly the
calls that deserve it — the chief of staff's standup question is a judgment
ask, not an RPC — and that distinction (registry calls vs. model asks) is
worth internalizing: it is the difference between your company's API and
your company's conversations.

**History lands on streams.** Four durable event streams carry the
company's institutional memory: every support escalation, every closed
incident (which the postmortem desk consumes *exactly once*, even across
restarts and redeploys), every money event (the monthly close reads the
`ledger` stream as its journal), and every outbound message. A stream
consumer that did not exist when the events were written can still replay
them — which means "add a postmortem process" is a config change, applied
retroactively to every incident already on the stream.

## What stays human, and how it stays sane

Six decisions in the example are gated on a person: refunds, production
ships, discounts past 20%, revenue write-offs, stuck incidents, and the
weekly content calendar. The pattern for each is identical: a `human` step
parks the run — durably, for days if needed — until the CEO or CTO answers
in a chat window, and the answer is schema-checked (a gate that asks for
`approve | decline` cannot proceed on "maybe later").

Two design choices make this workable at two humans:

- **Approvals batch at the right granularity.** The CEO approves marketing's
  *week* — one gate for seven pieces of content — not every keystroke. The
  CTO's ship gate fires only after CI passed *and* QA verified on staging.
  The humans see decisions, not drafts.
- **Gates never wait in silence.** The moment any gate opens, a `human.asked`
  event fires inside the instance, and a three-step workflow turns it into
  an email through the outbox. Nobody has to be watching a terminal for the
  company to route its decisions to its decision-makers.

And each agent is also simply *a colleague you can talk to*: every instance
serves a chat interface, and the chief of staff's `cos.brief` command
fans out to every desk and answers "what's happening right now?" with live
evidence. The morning standup and the Friday retro are the same mechanism on
a schedule — with the retro explicitly framed as *proposals* the CEO may
apply by editing a config, because nothing about the company changes except
through reviewed configuration.

## Voice, without pretending

The front-line desk answers SMS and phone calls, and the honest architecture
matters: agentd does not process audio. A small bridge service terminates
Twilio Media Streams and holds an OpenAI realtime voice session; agentd is
the brain behind it. Mid-call, when the voice model needs a fact it does not
have — "what plan am I on?", "why did my check fail at 3am?" — the bridge
fires an A2A command; the desk queries the back office and returns one
speakable sentence. After hangup, the transcript arrives as a webhook and a
wrap-up workflow decides: resolved, or escalated to the ticket desk with the
transcript attached. The voice model handles the milliseconds; the durable
runtime handles the minutes and the follow-through.

## What you actually need to run this

The example is deliberately concrete about its dependencies, because "just
automate it" pitches usually are not:

- **An intelligence endpoint** — any OpenAI-compatible API (or Anthropic, or
  Bedrock). Each instance carries its own model choice and daily token
  budget with a hard `on_exhausted: refuse`.
- **MCP servers for each integration** — the helpdesk, GitHub, the CRM,
  billing, email, a social cross-poster, web search, Twilio, your logs.
  These are the hands; agentd deliberately ships no tools of its own, so
  every capability is a declared, tagged, auditable dependency. Where a
  server offers more verbs than a role needs, the config narrows it at the
  registry (`allow: [get_*, restart_service, rollback_deploy]` on the SRE's
  infra server — the destructive verbs are *absent*, not discouraged).
- **A place to run** — eleven small processes on one box is fine; state is
  files on disk. (The example's README carries the full secrets-and-scopes
  inventory — which token each desk holds, how narrow it must be, and the
  one webhook-signature caveat — and every config's header documents the
  MCP tool contract that desk's workflows actually rely on.) The whole company dry-runs offline with the built-in mock
  intelligence (`AGENT_INTELLIGENCE=mock:final`) and `curl` as the webhook
  source, which is also how you rehearse changes to it.

## How to adopt this without betting the company

Nobody should deploy eleven agents on a Tuesday. The example decomposes into
adoption stages that each pay for themselves:

1. **Start with one closed loop where the event, the action, and the audit
   trail are all machine-legible** — dunning is the canonical first hire:
   Stripe webhook in, two polite emails with durable five- and seven-day
   sleeps between, and a human write-off gate at the end. Low judgment, high
   tedium, immediate audit trail.
2. **Add the outbox before you add the second communicating agent.** The
   choke point is cheap on day one and impossible to retrofit culturally
   once five agents each have their own mail credentials.
3. **Let the trifecta gate design your org.** When an agent needs a tool
   combination the gate refuses, that refusal is telling you where a
   role boundary belongs. Split the role; wire the halves with typed
   commands.
4. **Move judgment to gates before you move it to models.** Every place you
   are unsure whether the agent should decide, make it a `human` gate first;
   you can widen autonomy later by deleting a gate, which is a much better
   Tuesday than adding one after an incident.
5. **Only then scale out the conversational roles** — support, sales,
   marketing — where model judgment is the product and the guardrails
   (injection discipline in every instruction, budgets, the egress split)
   are already in place.

## The honest limits

The example encodes its own caveats, and they belong in any executive
summary. The agents' judgment is bounded by their instructions and their
models — a support desk will misread a novel situation a human would catch,
which is why escalation paths exist and why every instruction says *what to
do when unsure*. The MCP endpoints in the example are illustrative; the
integration work of standing up real ones is genuine engineering. Token
budgets are real money and want the same review payroll gets. And two humans
remain the constitutional layer for a reason: every gate in the example is a
place where the founders decided the cost of a wrong automated decision
exceeded the cost of an interruption. That inventory — *which decisions are
gates* — is the most important document the example produces, and it is
written in YAML you can diff.

## Where to go from here

- [`examples/startup/`](https://github.com/agentd-dev/source-code/tree/main/examples/startup)
  — the eleven configs, each validated against the binary, with the
  reasoning annotated inline.
- [Security](security.md) — the trifecta gate, tags, and what "no tools of
  its own" buys.
- [Workflows](workflows.md) and the [node registry](node-registry.md) — the
  full vocabulary the configs draw from.
- [Events and streams](workflows.md) — the durable event fabric the
  institutional memory rides on.
- [The hiring example](https://github.com/agentd-dev/source-code/tree/main/examples/hiring)
  — the same trifecta discipline at two instances instead of eleven, with
  mTLS between them.
