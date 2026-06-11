# A churn early-warning system that only speaks when something changed

> **Trigger:** cron, Mondays 08:00 · **Pattern:** score → diff vs last week → alert on movement · **Sample:** [`examples/use-cases/churn-monitor.toml`](../../examples/use-cases/churn-monitor.toml) · **Status:** runs today (`intel-remote,schema,tools-http-tls,trigger-cron`)

## The problem

Customer-success teams know the churn signals — login decay, seats
shrinking, support friction, failed payments. The problem is cadence:
nobody re-reads every account's usage every week, so risk is discovered
at renewal time, which is to say too late. The naive fix — a weekly
AI-generated "risk report" — fails differently: a wall of prose every
Monday that says roughly what it said last Monday trains everyone to
stop reading.

The fix for *that* is the interesting part: **alert on the delta, not
the state.**

## What the agent does

Monday, 08:00:

1. Pull the weekly usage export from your analytics API.
2. One schema-enforced LLM step scores it: `{risk_band:
   calm|elevated|urgent, top_accounts, summary}`. Structured, so it can
   be compared — not an essay.
3. `diff_compute` — a structural, deterministic diff, no model involved
   — compares this week's scoring against last week's snapshot on disk:
   exactly which fields changed, from what, to what.
4. **Nothing changed → terminate silently.** No message. The channel
   only ever hears news.
5. Something moved → the alert posts with the diff attached: "risk_band
   changed: calm → elevated; top_accounts changed: …", and the new
   snapshot replaces the old.

First run? The missing-snapshot case is a *declared edge* (`read_file`'s
`error` branch) that saves a baseline and exits — not a crash.

## The design point: memory you can read

The agent's "memory" is a JSON file at
`/var/lib/agentd/churn/latest.json`. That's deliberately humble, and it
buys three things expensive memory systems struggle with:

- **You can read it.** When the agent says risk rose, `cat` the
  snapshot and see exactly what it rose *from*.
- **The comparison isn't a vibe.** The LLM judges this week; the *diff*
  is computed structurally by `diff_compute`. The model never gets the
  chance to say "roughly unchanged" about a band that flipped.
- **You can reset it.** Bad baseline after a data incident? Delete the
  file; next Monday re-seeds. State management as `rm`.

```toml
[[nodes]]
id = "delta"
type = "diff_compute"
left_from = "prev_parsed.parsed"   # last week
right_from = "score.parsed"        # this week

[[nodes]]
id = "changed"
type = "condition"
expr = "delta.unchanged"           # true → say nothing
```

## Honest limits

- Scoring happens over the export as one document. Per-account fan-out
  ("score these 400 accounts independently, alert on the five movers")
  wants the proposed `map` node + array-index paths
  ([gap analysis §3](GAP-ANALYSIS.md#3-fan-out-over-dynamic-lists--the-map-node)) —
  today you'd shard by team with one declared branch each, or let the
  prompt handle the roll-up as this sample does.
- One snapshot of memory (last week) is the right amount for a delta
  alert. Trend lines over quarters belong in your analytics stack, not
  in an agent's pocket.
