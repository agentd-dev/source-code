# Contract review where counsel only reads the deviations

> **Trigger:** watched folder · **Pattern:** extract clauses → playbook gate → file or escalate · **Sample:** [`examples/use-cases/contract-review.toml`](../../examples/use-cases/contract-review.toml) · **Status:** runs today (`intel-remote,schema,trigger-fs-watch`)

## The problem

Sales wants the deal signed this week. Legal wants to read every line.
Both are right: most inbound paper is your own template or a benign
variant, and the one contract with an uncapped-liability clause buried
in section 14.3 is precisely why legal reads everything. So counsel
becomes the bottleneck for a stack of documents that are 90% routine —
the worst possible use of the most expensive reading time in the
company.

Legal teams formalized the answer years ago: the **playbook** — the
list of clauses you care about and the positions you accept. What's
been missing is a reader that applies the playbook tirelessly and
*knows what it's not allowed to decide*.

## What the agent does

1. A contract (text export) lands in the intake folder; the workflow
   fires.
2. One schema-enforced LLM step extracts the playbook clauses —
   liability cap, auto-renewal, governing law — and renders one verdict
   the schema forces to an enum: `risk: "standard" | "review"`. The
   prompt's rule is strict: *any* deviation or *any* missing clause →
   `review`.
3. `standard` → a clause memo is filed next to the contract,
   automatically.
4. `review` → the run checkpoints for counsel. They read the memo —
   which names exactly which clause deviated and what it says — then
   resume to file, or kill the run and pick up the phone.

Counsel's queue shrinks to the documents that actually contain
decisions.

## Why this isn't "AI lawyering"

The line this design refuses to cross is the one that matters
professionally: **the model never approves anything.** It extracts and
compares; the only two outcomes it can trigger are "file a memo" and
"wake up a lawyer." The asymmetry is structural —

```toml
[[edges]]
from = "gate"
when = "review"
to = "counsel_review"     # pause_for_approval — a human resumes
```

— and the failure direction is chosen deliberately: uncertain
extraction fails *toward* counsel (schema repair → declared failure →
human), never toward silent filing. A false "review" costs ten minutes;
a false "standard" was the thing we built the gate to prevent.

The memo trail is its own win: every contract gets a structured record
of what its key clauses said *at intake*, searchable later when someone
asks "which agreements auto-renew in Q1?"

## Honest limits

- Text in, judgment out. PDFs and DOCX need an upstream extraction step
  or a document-parsing MCP server
  ([gap analysis §7](GAP-ANALYSIS.md#7-document-parsing-pdf--docx-guidance)).
- The playbook lives in the prompt — three clauses here, but real
  playbooks run to dozens. At that scale, split per-clause-family
  workflows composed with `call`, or wait for the `map` node to fan out
  per clause ([§3](GAP-ANALYSIS.md#3-fan-out-over-dynamic-lists--the-map-node)).
- This triages; it does not redline. Drafting markup belongs in
  counsel's editor, with the memo open beside it.
