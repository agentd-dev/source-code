# Beacon: a software startup with two employees

Beacon sells uptime monitoring to small SaaS teams. It has a CEO, a CTO, and
**eleven agentd instances doing every other job in the company**. This directory
is the complete configuration: one YAML file per role, wired together over
A2A, driven by third-party webhooks, paced by schedules and signals, audited
by durable event streams — and every instance is also a colleague you can
open a chat with.

This is an EXAMPLE, not an endorsement of firing anyone: its purpose is to
show every major agentd mechanism doing a real job in one coherent system.
The MCP endpoints (`mcp.helpdesk.example`, …) are illustrative — point them
at your real servers.

## The org chart

```mermaid
flowchart TD
    CEO([CEO / CTO — the humans])
    COS[chief-of-staff<br/>standups · retros · briefs]
    SUP[support<br/>tickets L2]
    L1[support-l1<br/>SMS + voice]
    BO[support-backoffice<br/>accounts + logs]
    ENG[engineering<br/>fix → CI → ship]
    QA[qa<br/>verify + nightly sweep]
    SRE[sre<br/>incidents + postmortems]
    SAL[sales<br/>deals + cadences]
    FIN[finance<br/>invoices + dunning]
    MKT[marketing<br/>content + outreach]
    OUT[outbox<br/>ALL outbound mail/posts]

    CEO -. chat / approvals .-> COS & SUP & ENG & SRE & SAL & FIN & MKT
    COS --> SUP & L1 & BO & ENG & QA & SRE & SAL & FIN & MKT
    L1 --> BO
    L1 --> SUP
    SUP --> ENG
    SUP --> SAL
    QA <--> ENG
    SRE --> ENG
    SAL --> FIN
    SAL --> OUT
    FIN --> OUT
    MKT --> OUT
    MKT --> SAL
    COS --> OUT
```

## Where each mechanism earns its keep

| Mechanism | Where to look |
|---|---|
| **Third-party webhooks** | helpdesk tickets (`support`), GitHub Actions verdicts (`engineering`), monitoring alerts firing AND resolving (`sre`), website leads + email replies (`sales`), Stripe payments (`finance`), outreach replies (`marketing`), Twilio SMS + call transcripts (`support-l1`). Every route is HMAC-authenticated. |
| **Signals** | the incident run parks on `resolved/<alert_id>` until the all-clear webhook fires it (`sre`); each sales deal parks on `reply/<lead_id>` between cadence touches; marketing threads park on `mkt-reply/<person_id>`. A `wait {on: signal}` is durable — restarts don't lose a 10-day wait — and a deadline is an expected branch: `on_timeout: <step>` routes escalation (`sre` pages the CTO, `marketing` closes the file). The webhook→signal relays are ONE FIELD on the webhook start (`signal: "resolved/{{ body.alert_id }}"`). |
| **Workflows** | everywhere; the richest DAGs are `engineering/fix-bug` (agent → CI signal → switch → QA delegate), `finance/dunning` (delegate → durable sleep → check → switch → human gate), `sales/deal` (a month-long run per deal). |
| **Event streams** | `support/escalations` decouples triage from follow-through; `sre/incidents` replays every incident through the postmortem desk exactly once; `finance/ledger` is the journal the monthly close reads; `outbox/sent` is the audit ledger of everything the company ever said. |
| **MCP servers** | one per integration, declared in the desk that uses it, with its endpoint, credential, **trifecta tags** and tool ceiling (infra's `allow:` names the safe verbs only; github's `exclude:` drops the destructive ones). Each desk's file holds only its own servers, so its trifecta check sees exactly what it can reach. |
| **Typed A2A commands** | every cross-desk call a schema can hold uses `command:` + `args:` — the wire DataPart the receiving `a2a` start matches on, checked against its declared `schema:` at the listener (a malformed bug report is refused synchronously, naming the field). Judgment asks (the chief of staff standup questions) stay prose on purpose. |
| **Durable queues** | the content calendar is a `memory.push`/`memory.shift` array — the daily slot pops one item as a data step, no model call; `human.asked` events turn every waiting approval into an email through the outbox (`finance/gate-notifier`). |
| **Conversational experience** | every instance sets `a2a.events.enabled` — `agentd-tui --endpoint http://127.0.0.1:<port>` opens a chat with that colleague. The chief-of-staff's `cos.brief` command answers "what's happening?" with live answers from every desk. |
| **Human-in-the-loop** | refunds (`support`), production ships (`engineering`), stuck incidents page the CTO (`sre`), >20% discounts (`sales`), write-offs (`finance`), the weekly content calendar (`marketing`). `ask_human_fallback: wait` means an unanswered gate parks durably instead of guessing. |

## Voice, concretely

agentd does not process audio. `support-l1` pairs with a small bridge
service that terminates Twilio Media Streams and holds the OpenAI realtime
voice session; agentd is the brain behind it. Mid-call, the bridge fires the
`l1.lookup` A2A command whenever the voice model needs a fact ("what plan am
I on?"), and `support-l1` delegates to the back office and returns one
speakable sentence. After hangup the bridge posts the transcript webhook and
the wrap-up workflow resolves or escalates to the ticket desk.

## The trifecta discipline (why the org is shaped like this)

agentd refuses to boot an instance whose servers hold all three of
`untrusted_input` + `sensitive` + `egress` — that combination is the
prompt-injection kill chain. The org chart IS the mitigation:

| Instance | untrusted | sensitive | egress |
|---|---|---|---|
| support (tickets) | ✔ | — | ✔ |
| support-l1 (SMS/voice) | ✔ | — | ✔ |
| **support-backoffice** | — | ✔ | — |
| engineering / qa / sre¹ | — | ✔ | ¹ |
| sales / marketing | ✔ | ✔ | — |
| finance | — | ✔ | — |
| **outbox** | — | — | ✔ |
| chief-of-staff | — | — | — |

¹ sre's only egress is the public status page.

Two instances exist *because* of the gate: the **back office** (the desks
that read customer text cannot also hold accounts and logs) and the
**outbox** (the desks that read outside-authored text cannot also hold the
mail server). The residual risk is honest and documented in both files:
what the back office returns does flow onward to customers, so its contract
is minimal structured facts, never raw records — and everything the company
sends leaves one auditable choke point that can be rate-limited, held, or
unplugged.

## Ports

| Instance | A2A (chat + peers) | Webhooks |
|---|---|---|
| support | 8441 | 9441 |
| engineering | 8442 | 9442 |
| qa | 8443 | — |
| sre | 8444 | 9444 |
| sales | 8445 | 9445 |
| finance | 8446 | 9446 |
| outbox | 8447 | — |
| chief-of-staff | 8448 | — |
| marketing | 8449 | 9449 |
| support-l1 | 8450 | 9450 |
| support-backoffice | 8451 | — |

## Secrets, servers, and what each must be able to do

**Each desk's `mcp.servers` is its machine-checkable inventory.** Every
server entry carries its endpoint, its credential (an env var behind
`{{secret:NAME}}`, resolved at startup, redacted in logs), its trifecta tags
and its tool ceiling. Reviewing what a desk can touch means reading its
`mcp:` block; each desk header lists the secrets it needs and how narrow each
token must be. A server two desks share (the CRM, for sales and marketing) is
declared in both, the same way. Uncomment `security: { egress: closed }` in
a desk to turn its sections into an allow-list: every MCP dial must then
match one of its `mcp.servers`, every model call its `intelligence`
endpoint, every peer its `a2a.peers`, and anything else refuses.

**The webhook-signature caveat, once:** agentd verifies a generic HMAC of the
raw body against a named header. GitHub's `X-Hub-Signature-256` matches that
scheme natively. Stripe (`t=…,v1=HMAC(ts.body)`) and Twilio (HMAC-SHA1 over
URL+params) do not — front those two routes with a thin relay that verifies
the provider's scheme and re-signs the raw body (or use your gateway's
transform). Each file's header says which side of that line it is on. And
never expose an UNSIGNED route: fake alerts aimed at an agent that can
restart services, or fake leads aimed at one that emails people, are attacks,
not noise.

## Running it

Each desk is one instance and one file. Export the secrets the desk headers
name, then:

```console
$ cd examples/startup
$ for f in support engineering qa sre sales finance outbox chief-of-staff \
           marketing support-l1 support-backoffice; do
    agentd -c "$f.yaml" &
  done
```

Everything listens on loopback: third-party webhooks reach it through
whatever tunnel or ingress you already use, and in production you would put
the A2A mesh on `https://` with mTLS principals (see `examples/hiring/` for
that pattern) or on `unix:///` sockets for co-located instances. State is
durable under `/var/lib/agentd/startup/<name>` — kill any process mid-run
and restart it; sleeps, waits, gates, and stream offsets resume.

Talk to a colleague:

```console
$ agentd-tui --endpoint http://127.0.0.1:8448     # the chief of staff
> what's happening today?
```

**Dry-run the whole company offline** — the mock model endpoint answers
without a provider:

```console
$ export ALERT_WEBHOOK_SECRET=dry-run-secret
$ AGENTD_INTELLIGENCE=mock:final agentd \
    -c examples/startup/sre.yaml
$ body='{"alert_id":"a1","service":"probes","severity":"critical"}'
$ sig=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$ALERT_WEBHOOK_SECRET" -r | cut -d' ' -f1)
$ curl -X POST 127.0.0.1:9444/alerts/firing -H "X-Alert-Signature: sha256=$sig" -d "$body"
```

No key, no network: `mock:<script>` answers in-process. Every webhook is
signed (`webhooks.default_auth`), so the request carries an HMAC of its body.

Every config validates against the binary:

```console
$ cd examples/startup
$ for f in *.yaml; do agentd --validate-config -c "$f" || echo "$f"; done
```

## What is deliberately simplified

- The cadences are two touches; real sequences are longer but the same
  wait-signal-switch shape.
- `engineering` re-runs CI once on a red build; a real loop would use
  `iterate` with a bound.
- Instruction documents (RFC 0034) could fold each of these files into one
  markdown file per role (`:::config` + `:::workflow` + prose); the YAML
  form is used here because eleven files of it are easier to diff.
- One human answers every gate. At this company, that is rather the point.
