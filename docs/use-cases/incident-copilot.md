# An incident copilot with a hypothesis in the channel before you've opened your laptop

> **Trigger:** alertmanager webhook · **Pattern:** gather → assess → post → human gate → one pinned mitigation · **Sample:** [`examples/use-cases/incident-copilot.toml`](../../examples/use-cases/incident-copilot.toml) · **Status:** runs today (`intel-remote,schema,tools-http-tls,tools-shell`)

## The problem

The first ten minutes of an incident are spent re-establishing context
a machine already has: which service, what does the runbook say, what's
in the recent logs. The on-call does this at 03:12, half-awake, while
the incident channel fills with "any update?"

Letting an AI *act* during an incident is rightly terrifying — an agent
that "fixes" things at 3am is an incident generator. But an AI that
*reads and thinks* while the human walks to the keyboard? That's just a
very fast first responder who never sleeps.

## What the agent does

The alert webhook fires (HMAC-verified), and before the on-call's
laptop lid is open:

1. Reads the service's **runbook** and the **recent log tail** — the
   exact two files the human would open first. (Logs missing? That's a
   declared edge; it assesses on the runbook alone rather than dying.)
2. One schema-enforced LLM step produces `{hypothesis, blast_radius,
   proposed_mitigation}` — and `proposed_mitigation` may only name the
   **one pre-approved action** or `none`.
3. The assessment posts to the incident channel immediately. Pure
   read-and-think; nothing has been touched.
4. The run **checkpoints**. If the on-call agrees a restart is right,
   they resume the run — and only then does `shell_run` execute
   `/usr/local/bin/restart-app`.

## Three locks on the dangerous part

The mitigation step is where every "AI ops" pitch goes to die in
security review. Here it survives because three independent layers each
forbid the failure mode:

1. **The workflow declares the argv.** `command =
   "/usr/local/bin/restart-app"` — a literal in the file. The model's
   output is *never* interpolated into the command line. A prompt
   injection in the logs can corrupt the hypothesis; it cannot compose
   a shell command, because no code path builds one from model text.
2. **The policy allowlists one canonical path.** Even if the workflow
   were edited, `[policy.shell] commands =
   ["/usr/local/bin/restart-app"]` is the universe of executables. The
   default build doesn't even compile `tools-shell` in.
3. **A human authorizes by resuming.** The pause is durable (a
   checkpoint with a run id), the resume is logged with the audit
   trail, and "who approved the 3am restart" has an answer.

```toml
[[nodes]]
id = "mitigate"
type = "shell_run"
command = "/usr/local/bin/restart-app"   # declared, not derived
timeout_secs = 60
```

Model proposes; graph constrains; human disposes.

## The postmortem writes itself

Run with `--record` and every incident leaves a machine-readable
timeline: alert in, files read, hypothesis, who resumed, mitigation
exit code, milliseconds each. Paste it into the `/inspect` page and
the timeline renders. The "what did the bot do" section of the
postmortem becomes an attachment instead of an argument.

## Honest limits

- One pre-approved mitigation is a feature, not a limitation — but
  teams with several safe actions want one workflow per action class,
  or the proposed `respond`/parameterized-action work to stay
  argv-pinned ([gap analysis](GAP-ANALYSIS.md)).
- The copilot reads runbook + logs. Live queries against metrics
  systems fit naturally as allowlisted `http_request` reads or an MCP
  server — same pattern, more context.
