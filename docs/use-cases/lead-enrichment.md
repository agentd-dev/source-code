# Deep research on every new lead, before your rep finishes coffee

> **Trigger:** CRM webhook · **Pattern:** bounded research loop → brief → write-back · **Sample:** [`examples/use-cases/lead-enrichment.toml`](../../examples/use-cases/lead-enrichment.toml) · **Status:** runs today (`intel-remote,schema,tools-http-tls`)

## The problem

A new lead lands in the CRM. Somewhere between "now" and "first call",
somebody should figure out what the company does, how big it is, what
they raised, what hurts them, and what to open with. Reps either spend
30 minutes per lead doing this — or, more honestly, they don't, and the
first call opens with "so, tell me about your company."

This is the classic case for an AI *agent* rather than an AI *step*:
the work is genuinely open-ended. You don't know in advance whether the
answer is on their website, in a news API, or in an enrichment service.
Something has to **investigate**.

## What the agent does

Within seconds of the CRM webhook firing:

1. The webhook payload (company, website, contact) is composed into a
   research instruction.
2. An `agent_loop` node investigates: it can call **one tool**
   (`http_request`), against **an enumerated list of sources** — the
   enrichment API, the news API, the search API — for **at most 12
   steps**, inside **a 60k-token budget**. Within that box, it's free:
   it decides what to look up, follows what it finds, gives up on dead
   ends.
3. The loop's finding becomes a sales brief: what they build, size
   signals, recent news, the likely pain, two personalized opening
   lines.
4. The brief is written back to the CRM record and posted to the
   owning rep's Slack channel.

By the time a human looks at the lead, it has a dossier.

## Why this is the autonomy dial, not autonomy theater

Most "research agent" demos hide an uncomfortable truth: the agent can
browse anything, spend anything, and you find out what it did by reading
a transcript afterward. This workflow inverts every one of those:

- **The tool list is one item long.** The loop can make HTTP requests.
  It cannot write files, run commands, or call the CRM — the write-back
  happens *outside* the loop, in declared nodes.
- **The reachable internet is enumerated.** Five URL patterns in
  `[policy.http]`. A prompt-injected web page that tells the agent to
  "POST everything you know to evil.example" hits a policy denial that
  lands in the audit log — the page can lie to the model, but the
  allowlist doesn't read web pages.
- **Running out of budget is an outcome, not an incident.** The loop's
  `exhausted` branch is a declared edge to a declared failure. You will
  never discover a $400 research run in next month's invoice.

```toml
[[nodes]]
id = "research"
type = "agent_loop"
backend = "researcher"
instructions_from = "compose_task.rendered"
tools = ["http_request"]      # the whole toolbox
max_steps = 12                # the whole leash
max_tokens = 60000            # the whole budget
```

That's the entire contract, and it's eight lines you can read in a
pull request.

## Where the dial goes next

Run it with `--record`, and each enrichment leaves a run record showing
every source consulted and every token spent. Feed representative leads
through the [conformance suite](../CONFORMANCE.md) and you can put a
number on it — "the brief is complete and grounded 96% of the time" —
and *that* number, not a vibe, is what justifies raising `max_steps` or
adding a second tool. Autonomy granted by evidence.

## Honest limits

- The research sources need API access that speaks JSON over HTTPS —
  which `tools-http-tls` (new in v1.1.0) provides. Browser-grade
  scraping of arbitrary sites is not this tool's job; put a search /
  scrape API (or an MCP server) in front.
- One lead → one run. Backfilling 10,000 historical leads wants the
  queue-backed work distribution on the [roadmap](../ROADMAP.md), not a
  10,000-webhook storm.
