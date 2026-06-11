# The morning KPI digest that writes itself

> **Trigger:** cron, weekdays 07:00 · **Pattern:** fetch → narrate → deliver · **Sample:** [`examples/use-cases/exec-digest.toml`](../../examples/use-cases/exec-digest.toml) · **Status:** runs today (`intel-remote,tools-http-tls,trigger-cron`)

## The problem

Every leadership team has a morning ritual: someone opens four
dashboards, squints, and types "quick numbers update 🧵" into Slack.
The numbers were always available; the *narrative* — what moved, why it
matters, what to watch — is the part that costs a person's morning. And
the days it gets skipped are, by Murphy, the days something moved.

## What the agent does

At 07:00 on weekdays, with no inbound network surface at all:

1. `GET` the KPI snapshot from your metrics API.
2. One bounded LLM step turns the JSON into a three-section markdown
   brief: **What moved** (numbers and direction), **Why it matters**
   (one insight each), **Watch today** (max three items).
3. Post it to the leadership Slack channel and file a dated copy on
   disk — the searchable archive of every morning's story.

The model sees data and writes prose. The schedule, the data source,
the destination, and the spend are all declared in the workflow, out of
its reach.

```toml
[[triggers]]
type = "cron"
schedule = "0 7 * * 1-5"
start_node = "morning"
```

## The quiet advantages of the boring architecture

- **It's a daemon with one job.** No inbound HTTP routes exist in this
  workflow — the only way in is the clock. Attack surface: zero
  listening sockets beyond `/healthz`.
- **The cost is a budget, not a surprise.** `max_llm_tokens = 25000`
  per run, every run. A digest costs what it costs, predictably —
  multiply by 21 workdays and that's the monthly line item, known in
  advance. The [conformance suite's cost forecasting](../CONFORMANCE.md)
  does that arithmetic for you.
- **A bad morning fails loudly, not weirdly.** Metrics API down →
  declared `fail` node with a clear reason in the audit log, and no
  digest — instead of a hallucinated digest about data that didn't
  arrive. The `when = "error"` edge from the fetch is the difference.

## Variations that stay in bounds

- Pull from several sources and let a `parallel` node fan the fetches
  out concurrently (see [content-localization](content-localization.md)
  for the pattern).
- Weekly board pack: same graph, `schedule = "0 16 * * 5"`, a longer
  prompt, and the file becomes the draft your CFO edits instead of
  writes.
- Anomaly-only mode: add a [`diff_compute` gate like the churn
  monitor's](churn-monitor.md) so quiet days post nothing — alert
  fatigue is a design choice, not a fate.

## Honest limits

Slack's incoming webhooks happily accept the brief; richer Block Kit
formatting means shaping the POST body precisely, which `template_render`
handles. What the runtime won't do is *be* the dashboard — it writes the
morning's story; your metrics stack keeps the charts.
