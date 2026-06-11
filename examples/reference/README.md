# Reference deployments

Two production-shaped agents that exercise the full substrate —
authenticated triggers, a frontier model bounded to one node, fail-closed
policy, per-run budgets, a dedicated audit sink, and (for the webhook)
durable human-in-the-loop. These are the *dogfood*: what an actual
deployment looks like, not a minimal teaser.

Both validate on the default build; running them needs a binary built
with the capabilities they use (see each section).

| Deployment | Trigger | Shape |
|---|---|---|
| [`issue-triage.toml`](issue-triage.toml) | HTTP webhook (HMAC) | Classify a GitHub issue; pause for a human on high severity, file the rest automatically. |
| [`digest-report.toml`](digest-report.toml) | Interval (24h) | Read the day's events, summarize into a markdown digest, write it. |

---

## 1. `issue-triage.toml` — webhook-driven triage

A GitHub webhook (HMAC-SHA256 over the raw body) fires a single
`llm_infer` classification into `{area, severity, summary}`, schema-
enforced. **High** severity routes through a `pause_for_approval` node —
the run checkpoints and stops (exit 7) until a human resumes it — before
being recorded as urgent; **medium/low** are filed automatically.

```bash
# Validate (default build is enough):
agentd --config examples/reference/issue-triage.toml --validate-only

# Run (serve mode is inferred from [[http_routes]]). Needs a build with
# a hosted model + JSON-Schema enforcement; --state-dir lets the
# high-severity pause checkpoint durably:
cargo build --release -p agentd \
  --features "intel-remote,schema,server-tls"

ANTHROPIC_API_KEY=…  GITHUB_WEBHOOK_SECRET=… \
  agentd --config examples/reference/issue-triage.toml \
         --bind 0.0.0.0:8080 --state-dir /var/lib/agentd/state
```

Drive it (GitHub-style signature):

```bash
body='{"action":"opened","issue":{"number":42,"title":"crash on save","body":"stack trace…"}}'
sig=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$GITHUB_WEBHOOK_SECRET" | cut -d' ' -f2)
curl -sS -X POST localhost:8080/webhook/github \
     -H "X-Hub-Signature-256: sha256=$sig" -d "$body"
```

A high-severity result returns a `paused` outcome with a run id; resume
once a human has eyeballed it:

```bash
agentd --config examples/reference/issue-triage.toml \
       --state-dir /var/lib/agentd/state --resume exec-…
```

**What the policy guarantees.** The run can write under
`/var/lib/agentd/triage/` and nowhere else; it has no shell and no
outbound HTTP compiled in. A hostile issue body can change *what the
model says*, never *what the process can do*.

## 2. `digest-report.toml` — scheduled report

Every 24h: render the inbox path, `read_file` it, `parse_json`,
summarize with one bounded `llm_infer` step, and `write_file` a markdown
digest. No inbound network surface at all — the interval trigger is the
only way in. A missing inbox routes to a declared `fail` rather than
summarizing nothing.

```bash
agentd --config examples/reference/digest-report.toml --validate-only

cargo build --release -p agentd --features "intel-remote,trigger-cron"

# Seed an inbox, then run (serve mode is inferred from the interval trigger):
mkdir -p /var/lib/agentd/digest/inbox /var/lib/agentd/digest/reports
echo '{"deploys":3,"incidents":1,"prs_merged":12}' > /var/lib/agentd/digest/inbox/events.json

ANTHROPIC_API_KEY=… agentd --config examples/reference/digest-report.toml
cat /var/lib/agentd/digest/reports/digest.md
```

---

## Deploying for real

Both are single-binary processes — drop them behind the hardened
[`systemd` unit](../../packaging/) or the
[distroless container image](../../docs/operations.md#84-container-image-ghcr):

```ini
# /etc/default/agentd  (systemd EnvironmentFile)
AGENTD_ARGS=--config /etc/agentd/issue-triage.toml --bind 0.0.0.0:8080 --state-dir /var/lib/agentd/state
ANTHROPIC_API_KEY=…        # injected from your secret store, never the TOML
GITHUB_WEBHOOK_SECRET=…
```

Operational guidance — drain on `SIGTERM`, hot-reload policy/TLS on
`SIGHUP`, `/healthz` + `/metrics`, k8s pod shape — lives in
[`docs/operations.md`](../../docs/operations.md). Gate either workflow on
a measured `pass_rate` before letting it run unattended:
[`docs/CONFORMANCE.md`](../../docs/CONFORMANCE.md).
