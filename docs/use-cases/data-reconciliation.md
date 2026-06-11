# A nightly reconciliation sentinel whose SQL the model never writes

> **Trigger:** cron, nightly · **Pattern:** declared query via MCP → structural diff → LLM explains drift · **Sample:** [`examples/use-cases/data-reconciliation.toml`](../../examples/use-cases/data-reconciliation.toml) · **Status:** runs today (`intel-remote,tools-http-tls,trigger-cron` + an MCP database server)

## The problem

Somewhere between the payment processor, the ledger, and the warehouse,
numbers drift. Not often — just often enough that finance runs a
reconciliation query before month-close and occasionally finds a
three-week-old discrepancy that's now an archaeology project. The check
is mechanical; the *noticing* is what fails. And the fashionable fix —
"let an AI agent query the database and look for anomalies" — makes
DBAs reach for the revoke button, correctly: free-form LLM SQL against
production is a privilege-escalation story with extra steps.

This workflow keeps the noticing and deletes the scary part.

## What the agent does

Every night at 02:30:

1. The reconciliation query — **declared verbatim in the workflow
   file** — runs through an MCP database server. Two things to savor
   about that sentence: the agent process never holds raw database
   credentials (the MCP server does), and the model never writes SQL
   (there is no code path from model output to the query string).
2. `diff_compute` structurally compares the result against the
   **committed expectation file**: which sources, which fields, what
   changed, from what, to what. Deterministic — no model in the loop
   for the comparison.
3. **No drift → terminate silently.** The channel only ever hears news.
4. Drift → *now* the LLM earns its keep, doing the one thing it's
   actually for here: turning `{"changed": {"stripe.cents": {"from":
   182733, "to": 174501}}}` into "Stripe's ledger total dropped ~8.2k
   cents overnight while order count held — check refunds issued after
   the 23:00 batch." That explanation, plus the raw diff, goes to the
   on-call channel.

## The separation of powers

Each component does only what it's trustworthy at:

| Component | Job | Explicitly not its job |
|---|---|---|
| Workflow TOML | Owns the SQL, the schedule, the expectation | — |
| MCP server | Holds credentials, executes the one allowlisted tool | Deciding what runs |
| `diff_compute` | Detecting change, deterministically | Judging significance |
| `llm_infer` | Explaining the diff in operator language | Querying, comparing, deciding |

A prompt injection hidden in a ledger memo field can, at absolute
worst, make the *explanation* weird. It cannot change what was queried,
what was compared, or whether the alert fires — those decisions never
pass through the model.

```toml
[[nodes]]
id = "run_query"
type = "call_mcp_tool"
server = "warehouse"
tool = "query"              # the ONLY tool the allowlist admits
args_from = "sql_args.parsed"   # parsed from a literal in this file
```

## Honest limits

- The expectation file is a committed artifact — update it when the
  business legitimately changes (new payment source), and that update
  is a reviewed diff. Self-adjusting baselines are deliberately absent:
  a sentinel that learns to accept drift isn't a sentinel.
- One query per workflow keeps the blast radius readable. A suite of
  reconciliations = one workflow each, composed under
  [`call`](../capabilities.md) or scheduled separately — not one
  mega-agent with database freedom.
- Aggregate-level checks, not row-level forensics. When the sentinel
  fires, the archaeology is still yours — it just starts the same
  night, not at month-close.
