# Compliance evidence that collects itself — by a process auditors can read

> **Trigger:** cron, monthly · **Pattern:** pinned check → control mapping → dated bundle · **Sample:** [`examples/use-cases/compliance-evidence.toml`](../../examples/use-cases/compliance-evidence.toml) · **Status:** runs today (`intel-remote,tools-shell,trigger-cron,signing`)

## The problem

SOC 2 and ISO 27001 don't fail companies for weak security; they fail
them for **undocumented** security. The month before the audit, an
engineer burns a week screenshotting patch levels and exporting user
lists — evidence that's stale the day it's collected and collected the
month it's needed. Compliance platforms automate some of it; the gaps
get filled by hand, quarterly, grudgingly.

Here's the recursive trick this workflow plays: when the collector is a
**signed, policy-bounded, audited workflow**, the *mechanism* becomes
the strongest piece of evidence in the bundle.

## What the agent does

First of the month, 06:00:

1. `shell_run` executes the posture-check script — patch level, disk
   encryption, listening ports, user audit; whatever your script
   gathers. The command is **argv-pinned in the workflow** and the
   *only* path `[policy.shell]` allows this process to execute, ever.
2. One bounded LLM step maps raw output to your control set — access
   control, patch management, encryption at rest, logging — with the
   auditor-grade instruction: *state what the output evidences, or
   state plainly that evidence is missing. Never embellish.*
3. Two files land in the evidence directory, dated: the narrative
   mapping and the **raw output beside it** — so the auditor can always
   check the prose against the source.
4. A non-zero exit from the check script routes to a declared failure:
   "evidence run is invalid." A failed check never becomes a
   quietly-pretty bundle.

## Why the mechanism is the evidence

Walk an auditor through what produced the bundle:

- **The workflow is ed25519-signed.** The collection process that ran
  is cryptographically the one that was approved — change a line, the
  signature breaks (`signing` feature, verified before execution).
- **The capability surface is enumerable.** This binary can execute
  exactly one program, write exactly one directory, and (for the
  mapping step) call one LLM backend. That's not a policy document
  claiming restraint; it's a fail-closed allowlist in the artifact
  itself.
- **Every run leaves a run record** (`--record`): what ran, what it
  read, what it wrote, timestamped. The audit log (`agentd::audit`
  JSONL, redacted) is the meta-evidence — evidence about the evidence
  collection.

"Who collects your evidence, and how do you know it does what's
documented?" — most companies answer with a wiki page. This answers
with a signature check.

## Honest limits

- The LLM writes the *narrative*; the raw output ships alongside
  precisely because narratives don't satisfy auditors alone. Never file
  the prose without the source — the workflow doesn't let you.
- One host per process is the deployment shape. Fleet-wide evidence
  (every production node, monthly) wants the
  [scale-out roadmap](../ROADMAP.md) — or, today, this same signed
  workflow in the systemd unit of each box, writing to a shared sink.
- Continuous-compliance platforms do breadth; this does depth and
  custody. They're complementary — this fills the gaps those platforms
  leave to "manual collection."
