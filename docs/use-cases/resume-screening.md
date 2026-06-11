# Resume screening you could defend in front of a regulator

> **Trigger:** ATS webhook · **Pattern:** rubric score → advance flows, declines pause · **Sample:** [`examples/use-cases/resume-screening.toml`](../../examples/use-cases/resume-screening.toml) · **Status:** runs today (`intel-remote,schema,tools-http-tls`)

## The problem

AI resume screening is the use case everyone wants and nobody wants to
defend. The efficiency is real — hundreds of applications per posting,
most clearly not a fit. The risk is real too: hiring decisions are
regulated (NYC Local Law 144 audits automated screens; the EU AI Act
classes hiring AI as high-risk), and "the model rejected them and we
can't say why" is a sentence with legal consequences.

The trap is treating this as a model-quality problem. It's a *process*
problem: who decided, against what criteria, with what record?

## What the agent does

1. The ATS webhooks each new application.
2. One LLM step scores it against **the rubric written verbatim in the
   workflow** — relevant shipped work (double weight), stack depth,
   communication clarity — with the explicit instruction to ignore
   name, school prestige, gaps, and demographic signals. The output is
   schema-forced: `{score 1-5, strengths, gaps, recommendation:
   advance|decline}`.
3. The asymmetry that makes it defensible:
   - **advance** → flows back to the ATS automatically. A false
     positive costs an interviewer thirty minutes.
   - **decline** → the run **checkpoints**. A recruiter reads the
     scorecard and resumes it. *Their* resume action is the decision.
     **No candidate is rejected by a machine.**

```toml
[[edges]]
from = "gate"
when = "decline"
to = "human_owns_declines"   # pause_for_approval — a person decides
```

## The audit story is the product

Compliance for automated hiring tools comes down to three questions,
and this architecture answers each with an artifact rather than a
policy memo:

- *What are the criteria?* — The rubric is in the workflow file:
  version-controlled, diffable, and **ed25519-signable**, so the
  process that ran is provably the process that was approved. Every
  candidate is scored by the same prompt — no recruiter-by-recruiter
  drift.
- *What happened for this candidate?* — Run with `--record`: input,
  scorecard, gate decision, who resumed, timestamps. Per-candidate
  evidence, machine-readable.
- *Does it behave consistently?* — The [conformance
  suite](../CONFORMANCE.md) runs a corpus of test applications through
  the real workflow with pass-rate bars (`min_pass_rate`) — including
  paired applications that differ only in demographic signals, which
  must score identically. **Drift detection** re-runs the corpus after
  every model update and fails CI on regression; the model changing
  under you is the quiet killer of "we audited it once."

## Honest limits

- A screen, not a judge: it sorts the obvious so humans spend judgment
  where it's contested. The decline gate must *stay* human — that's not
  a v2 to automate away; it's the design.
- Bias mitigation here is structural (same rubric, paired-input tests,
  human declines), not a fairness proof. Run your jurisdiction's audit
  regime on top — the run records are exactly the dataset it needs.
- Resume text arrives via the ATS webhook; parsing PDFs upstream is the
  usual [document gap](GAP-ANALYSIS.md#7-document-parsing-pdf--docx-guidance).
