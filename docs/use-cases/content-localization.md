# Ship marketing copy in every language at once — with one editor's veto

> **Trigger:** watched folder · **Pattern:** parallel fan-out per locale → join → human gate · **Sample:** [`examples/use-cases/content-localization.toml`](../../examples/use-cases/content-localization.toml) · **Status:** runs today (`intel-remote,trigger-fs-watch`)

## The problem

The English launch post is done. Now German, French, Japanese — each a
translation vendor, a two-day turnaround, and a small risk that the
idiom lands wrong. LLMs translate marketing copy remarkably well, and
the temptation is a quick script: loop over languages, call the API,
publish. The script works until the day one language silently fails,
half the locales publish, and the German page 404s during the launch.

Fan-out work needs *join semantics* — all of it lands, or none of it
publishes — and someone with taste should hold the final gate.

## What the agent does

1. Drop finished English copy into the inbox folder; the workflow fires
   on file creation.
2. A `parallel` node runs **one sub-workflow per locale,
   concurrently** — each branch is its own complete, validated workflow
   file (`localize-de.toml`, `localize-fr.toml`) doing one bounded
   translation step. Branches share the parent's policy and budget
   envelope: the fan-out is scheduling, not new authority.
3. The joined bundle — locale results in declared order — is written to
   disk as one artifact.
4. The run **pauses for editorial sign-off**. A human reads the bundle
   (or the run record), then resumes to publish. One veto covers every
   language.
5. If **any** branch fails, the `error` edge routes to a declared
   failure: *nothing* publishes. No half-launched page sets.

```toml
[[nodes]]
id = "translate_all"
type = "parallel"
branches = [
  { workflow = "examples/use-cases/localize-de.toml", input_from = "load" },
  { workflow = "examples/use-cases/localize-fr.toml", input_from = "load" },
]
```

## Why the all-or-nothing matters

The join is the contract. `{results, ok}` comes back only when every
branch completed; one failure means the workflow takes the failure
path, the bundle never lands, and the audit log names which branch
broke. The failure mode of the naive script — *partial success
discovered by customers* — is structurally unrepresentable here.

And because each locale is a separate workflow file, the German
reviewer can read exactly what the German branch does, the prompt
included, without spelunking code. Adding Japanese is: write
`localize-ja.toml`, add one line to `branches`, send the pull request.

## The budget angle nobody mentions

All branches draw on **one** `max_llm_tokens` envelope. Localizing to
ten languages doesn't create ten budgets that each look small — it
spends one budget that was sized for the job. Fan-out without
fan-out-billing-surprise.

## Honest limits

- Locales are declared branches — perfect for the stable set a company
  actually ships. "Fan out over whatever list arrives in the data" is
  the proposed `map` node
  ([gap analysis §3](GAP-ANALYSIS.md#3-fan-out-over-dynamic-lists--the-map-node)).
- The publish step after editorial sign-off is your CMS API call —
  swap the terminate for an `http_request` to your CMS (HTTPS via
  `tools-http-tls`).
