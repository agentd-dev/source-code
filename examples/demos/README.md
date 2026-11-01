# Five demos, run by agentd

Five short demos in which agentd does the work with its own machinery: MCP
Events, MCP resource subscriptions, workflows, A2A between agentd instances
and to another agent, the human-in-the-loop gate, the guarded command
runner, and the terminal UI on screen. In each, an instruction someone else
owns on instruction.md changes while agentd runs, and agentd reacts live.
The companies, the bank, the flat and the people are fictional (Northwind
Supplies Ltd., Harbor Bank, Andrii and Nataliia's flat); so is every number.

| Demo | The story | Files |
|---|---|---|
| **(a) Accounts payable** | Invoices arrive from the AP system. agentd decides each one under an approval policy the controller owns on instruction.md. Above the limit, it holds the invoice and asks the controller (or the CFO). The controller raises the limit and releases v2 of the policy; seconds later the same amount is approved without asking. | `ap-agent.yaml`, `instruction/`, `standins/payables.mjs` |
| **(b) Month-end close** | The close agent follows "Month-end close", the finance team's instruction on instruction.md. A bank line lands in the ERP's feed. agentd asks a second agentd, the reconciliation agent, which lines nothing in the ledger explains, drafts one journal entry per line, and stops at a review gate. It cannot post: the tool is never granted. The team adds "MUST: tag every draft entry with the closing period" and releases; the next lines' drafts carry `2026-10`, the earlier ones do not. | `close-agent.yaml`, `recon-agent.yaml`, `instruction/close-v*.md`, `standins/erp.mjs` |
| **(c) Regulated support** | Harbor Bank's chat, voice and email agents — three agentd instances — follow "Card disputes". Compliance changes the disclosure from 60 to 120 days and releases; all three apply it within a second and the next customer hears 120 days. The owner rolls back; all three go back. | `support*.yaml`, `support-tmux.sh`, `standins/model.mjs` |
| **(d) The coding agent** | agentd, as a coding agent in a Go repository, is asked to delete a flaky ledger test and does. The lead adds "NEVER: delete, skip or disable a failing test" and releases; agentd applies it live and, asked again, refuses and fixes the cause. A strong model fixes the cause from the start, so the recorded take releases a changelog rule instead (see (d)). | `coder*.yaml`, `engineering-fixture/` |
| **(e) Home climate** | The flat's climate assistant follows "Home climate". A release moves Nataliia's temperature from 23 to 24.5 °C, and the next decision sets 24.5. The sun warms the living room; the assistant asks the home agent over A2A to close the curtains, then asks Nataliia before it cools. | `home*.yaml`, `standins/thermostat.mjs`, `aircon.mjs`, `home-agent.mjs` |

## What each beat shows

**(a) Accounts payable**

| Beat | What the audience sees | The agentd capability |
|---|---|---|
| Start | agentd loads policy v1 from the registry | `agent.instruction.mcp`: the instruction is an MCP resource, read at startup and subscribed (`instruction.loaded`, with the registry's version id) |
| Invoice INV-2041, GBP 3,240 | approved, no person involved | an MCP Events binding (`invoice.created`, poll delivery) appends each occurrence to a stream; a `stream` start runs the `invoice-intake` workflow once per invoice; an `agent` step decides with read-only tools and an `output_schema`; `mcp.tool` records the decision |
| Invoice INV-2042, GBP 18,500 | put on hold; a question appears in the controller's TUI | `switch` on the decision, `invoices.hold`, then a `human` gate addressed to `{role: operator, labels: {approver: controller}}`; the run waits durably and costs nothing |
| The controller answers `approve` in the TUI | the payables stand-in shows the invoice approved by the controller | the gate's answer schema (`approve`/`reject`) and addressee are enforced; the answer resumes the run; only then is `invoices.approve` called |
| The controller releases v2 (limit GBP 25,000) | agentd logs `instruction.applied`, v1 → v2, within seconds | the registry pushes `notifications/resources/updated` on the subscription and an `md.instruction.activity` MCP Events occurrence; agentd re-reads the policy and applies it from the next turn, without a restart |
| Invoice INV-2043, GBP 18,500 again | approved, no question asked | the same workflow and the same amount, decided differently, because the policy text changed |
| (optional) INV-2044, GBP 64,800 | the question goes to the CFO's TUI | the second gate, addressed to `approver: cfo`; a controller who answers it anyway is recorded as an override (`human.answer.override`) |
| (optional) INV-2045, Bramblewood Catering | held: the vendor's bank details changed four days ago | a policy rule the model applies from the vendor record (a real model only; see [The mock model](#the-mock-model)) |

The model never holds a write tool: the `decide` step is narrowed to
`payables.invoices.get`, `payables.vendors.get` and
`payables.invoices.list`, and every approval or hold is a workflow step that runs on what the model answered. An invoice above
the limit is released by a person's answer and nothing else.

**(b) Month-end close**

| Beat | What the audience sees | The agentd capability |
|---|---|---|
| Start | the close agent loads "Month-end close" v1 and subscribes to `erp://bank-feed/today` | `agent.instruction.mcp` from the mirror, with its own agent credential (`close-agent`, OAuth client credentials) and a trust pin on the team's publisher; a `subscribe` start on an MCP resource (`start.subscribe.armed`) |
| Four bank lines land | the workflow wakes once for the burst | `notifications/resources/updated` → agentd reads the resource again and fires the run (notify-then-read, `debounce_ms: 2000`) |
| "Which lines are unmatched?" | the recon agent's log shows the request and its answer | `a2a.delegate` with a typed command (`recon.unmatched`) to a second agentd, whose `a2a` start admits only the close agent's credential, granted that one command |
| Drafts | four journal entries appear in the ERP as drafts, untagged (`tags=-`) | an `agent` step with `erp.journal.draft` and an `output_schema`; the tools are the ones the document names (`bank.read`, `accounts.read`, `journal.read`, `journal.draft`) |
| "Can it post?" | no | the ERP has `journal.post`, as real ERPs do; `close-agent.yaml`'s server `allow` list never names it, so the tool does not exist for this agent. A model that tries is refused inside agentd and the ERP never sees the call (the rehearsal's mock tries, on purpose) |
| Review | the question in the controller's TUI: review and post in the ERP | a `human` gate addressed to the controller; posting stays a person's act |
| The team releases v2: "MUST: tag every draft entry with the closing period, e.g. 2026-10." | `instruction.applied v1 → v2`, within a second | the resource's subscription and the `md.instruction.activity` MCP Events type; agentd re-reads, verifies the signature, and applies from the next turn, without a restart |
| Two more bank lines land | two drafts, both `tags=["2026-10"]`; the review question says so | the same workflow, the same step text: nothing in `close-agent.yaml` mentions a tag or a period. The difference is the rule, which reaches the drafting step through the instruction alone |
| Review again | the controller reviews; nothing is posted | the gate, as before |

## Prerequisites

- **agentd** from `main` or a release binary: the release feature set has
  everything demos (a)–(c) and (e) use (`mcp-events`, `a2a`, `cel`, `oauth`,
  `sign`, `mock-models`). Check with `agentd --version` and `agentd
  --build-info`. Two exceptions:
  - The instruction team's documents for (b), (c), (d) and (e) are
    instruction/1, which only agentd's instruction/1 build reads today
    (`main` reads the older syntax). Record those four, and rehearse (b)
    and (e), on that build.
  - Demo (d) needs `exec`, which no release binary has: [A binary with
    exec](#a-binary-with-exec).
- **Node.js 20 or later** and npm, for the stand-ins and the TUI; `curl`, for
  the rehearsal.
- **The TUI**, `agentd-tui` from `@agentd-dev/cli`. From this repository:

  ```sh
  (cd interface && npm ci && npm run build:node)   # then: node interface/dist/tui/cli.js
  ```

  or `npm install -g @agentd-dev/cli`, which puts `agentd-tui` on `PATH`.
- **For the recording:** `OPENAI_API_KEY` in the environment (every config
  names the model `gpt-6.1-sol` and references the key only as
  `{{secret:OPENAI_API_KEY}}`; no config sets a `dialect`, so agentd picks
  the wire format for the model itself), and the instruction.md mirror with
  each demo's instruction published and an agent credential for it.
- **For demo (d):** Go, and the repository to work in.

Every config asks the model the same way: the host alone, and the model
declared as a reasoning model the turn role names (with no
`intelligence.model`, which the validator refuses beside `roles.turn`):

```yaml
intelligence:
  endpoints: https://api.openai.com
  token: "{{secret:OPENAI_API_KEY}}"
  models:
    sol: {kind: reasoning, model: gpt-6.1-sol, reasoning: {effort: medium}}
  roles: {turn: sol}
```

The scripted-model layers (`support.mock.yaml`, `home.mock.yaml`) set their
own endpoint, a model `scripted` and `roles: {turn: scripted}`. agentd's
mock (`--intelligence mock:file:…`) keeps the config's model, gpt-6.1-sol,
and answers it on the API agentd asks it on, the Responses API included.

### What the recorder exports, per demo

Secrets are read from the environment by `{{secret:NAME}}` and never
written in a config; everything else here is a `${NAME:-default}` the
configs fill in. `env.sh` exports the local tokens (the last column) and
`AGENTD_DEMO_HOME`; the rest is yours.

| Demo | Mirror and credentials | Secrets | Other | From `env.sh` |
|---|---|---|---|---|
| all | `IMD_MCP_URL` (`http://localhost:18080/mcp`), `IMD_TOKEN_URL` (`http://localhost:18080/auth/realms/instruction-md-agents/protocol/openid-connect/token`) | `OPENAI_API_KEY` | `AGENTD_DEMO_HOME` | |
| (a) | `AP_INSTRUCTION_URI`, `IMD_CLIENT_ID` | `INSTRUCTION_MD_CLIENT_SECRET` | `PAYABLES_MCP_URL` | `CONTROLLER_TOKEN`, `CFO_TOKEN` |
| (b) | `CLOSE_INSTRUCTION_URI` (default `instruction://lumen-foods/month-end-close@latest`), `CLOSE_CLIENT_ID` (`close-agent`), `CLOSE_PUBLISHER` (default `http://localhost:18080/pub/lumen-foods`) | `CLOSE_CLIENT_SECRET` | `ERP_MCP_URL`, `RECON_A2A_URL` | `CONTROLLER_TOKEN`, `RECON_PEER_TOKEN` |
| (c) | `SUPPORT_INSTRUCTION_URI` (default `instruction://ins_01M69N5T2ATZNEJVW397VRQTXW@latest`), `CHAT_AGENT_CLIENT_ID` (`chat-agent`), `VOICE_AGENT_CLIENT_ID` (`voice-agent`), `EMAIL_AGENT_CLIENT_ID` (`email-agent`) | `CHAT_AGENT_CLIENT_SECRET`, `VOICE_AGENT_CLIENT_SECRET`, `EMAIL_AGENT_CLIENT_SECRET` | — | `SUPPORT_TOKEN` |
| (d) | `CODER_INSTRUCTION_URI` (default `instruction://ins_01M6A4D6VM7DM190DD1D7V14G0@latest`), `CODER_CLIENT_ID` (`coder`) | `CODER_CLIENT_SECRET` | `REPO` (required), `TZ`, `GOPROXY`, `GOTOOLCHAIN`, `GOFLAGS` | `ENGINEER_TOKEN` |
| (e) | `HOME_INSTRUCTION_URI` (default `instruction://ins_01M69H2WEK2P005WHYHJJMYM4A@latest`), `HOME_CLIENT_ID` (`home-climate`) | `HOME_CLIENT_SECRET` | `THERMOSTAT_MCP_URL`, `AIRCON_MCP_URL`, `HOME_AGENT_URL`, `HOME_SETTLE` | `NATALIIA_TOKEN`, `ANDRII_TOKEN`, `HOME_PEER_TOKEN` |

The rehearsal layers need none of the mirror or model columns.

Every command below runs from the repository root. Start the stand-ins'
dependencies once:

```sh
(cd examples/demos/standins && npm ci)
```

### The demo credentials

```sh
. examples/demos/env.sh
```

`env.sh` makes the local tokens once, under `$AGENTD_DEMO_HOME/tokens`
(default `/tmp/agentd-demos/tokens`, mode 600), and exports them for the
configs' `{{secret:…}}` references: `CONTROLLER_TOKEN` (Morgan Ellis, the
financial controller), `CFO_TOKEN` (Sam Okafor, the CFO),
`RECON_PEER_TOKEN` (the close agent's credential at the recon agent),
`SUPPORT_TOKEN` (Harbor Bank's support desk, at all three channels),
`ENGINEER_TOKEN` (at the coding agent), `NATALIIA_TOKEN` and `ANDRII_TOKEN`
(at the climate assistant) and `HOME_PEER_TOKEN` (the climate assistant's
credential at the home agent). The TUIs sign in with the same files. Source
it in every terminal that starts an agent, a TUI or the stand-ins. It never
touches the model key or the registry secrets.

### Why plain `http://` is fine here

agentd refuses a non-`https` endpoint, with one carve-out: a loopback
`http://` address (`127.0.0.1`, `localhost`). It needs no setting.
Everything in these demos binds to `127.0.0.1` — the stand-ins, the two A2A
listeners and the TUI's connection — so no byte leaves the machine, and
nothing on the network can reach the servers, which have no sign-in of their
own. The stand-ins also refuse a request whose `Host` is not a loopback name.
The mirror on `http://localhost:18080` is loopback too. Anything else needs
`https://`.

## Demo (a): accounts payable

Four terminals. The TUI terminal is the one on camera; size it first
(see [The TUI at 120x32](#the-tui-at-120x32)).

**1. The stand-ins** (payables on :8701, the ERP on :8702):

```sh
(cd examples/demos/standins && npm start)
```

**2. The AP agent, following the policy on the mirror:**

```sh
. examples/demos/env.sh
export IMD_MCP_URL=http://localhost:18080/mcp                 # the mirror's MCP endpoint
export IMD_TOKEN_URL=http://localhost:18080/auth/realms/instruction-md-agents/protocol/openid-connect/token
export IMD_CLIENT_ID=<the agent credential's client id>
export INSTRUCTION_MD_CLIENT_SECRET=<its secret>
export AP_INSTRUCTION_URI='instruction://ins_<id>@latest'     # the published policy, at the ref to follow
agentd --validate-config -c examples/demos/ap-agent.yaml
agentd -c examples/demos/ap-agent.yaml 2> >(tee "$AGENTD_DEMO_HOME/ap-agent.log" >&2)
```

The policy to publish as v1 is [`instruction/ap-policy-v1.md`](instruction/ap-policy-v1.md);
v2 is [`instruction/ap-policy-v2.md`](instruction/ap-policy-v2.md), which
changes one line, `Approval limit: GBP 10,000.` to `GBP 25,000.`. Address the
policy by its id (`instruction://ins_…@latest`) where you can: that address
is subscribable, so a release reaches agentd on the subscription. An
owner/name address (`instruction://<org>/<name>@latest`) works too; the
registry then says so by the `md.instruction.activity` event, a few seconds
later. `ap-agent.yaml` listens for both.

To watch what agentd does beside the TUI, filter its log:

```sh
tail -f "$AGENTD_DEMO_HOME/ap-agent.log" | grep --line-buffered -E \
  '"event":"(instruction\.(loaded|applied)|start\.fired|human\.(ask|answered|answer\.override)|step\.start)"'
```

**3. The controller's TUI** (on camera; 120x32):

```sh
. examples/demos/env.sh
tmux new-session -s ap-tui -x 120 -y 32 \
  "node interface/dist/tui/cli.js --endpoint http://127.0.0.1:8420 --bearer-file $AGENTD_DEMO_HOME/tokens/controller.token"
```

For the CFO beat, a second TUI with `--bearer-file $AGENTD_DEMO_HOME/tokens/cfo.token`.

**4. The presenter's buttons:**

```sh
node examples/demos/standins/control.mjs invoice          # INV-2041, GBP 3,240.00: approved
node examples/demos/standins/control.mjs invoice          # INV-2042, GBP 18,500.00: the controller is asked
#   … the controller answers in the TUI …
#   … the controller releases v2 on instruction.md …
node examples/demos/standins/control.mjs invoice          # INV-2043, GBP 18,500.00: approved under v2
node examples/demos/standins/control.mjs invoice INV-2044 # optional: GBP 64,800.00, the CFO is asked
node examples/demos/standins/control.mjs invoice INV-2045 # optional: held, vendor bank details changed
node examples/demos/standins/control.mjs invoice --vendor V-1002 --amount 9800 --memo "Ad-hoc courier"
node examples/demos/standins/control.mjs state payables   # every invoice and its status
```

**Answering in the TUI.** A gate from an invoice run is its own task, not a
reply in the TUI's conversation, so its row reads `⏎ answer with #<task>
<reply>`. Type `#`, press Tab to take the offered task (`answer this task`),
type `approve` (or `reject`), press Enter. The transcript then shows the
answer and the run's finish, `{"decision":"approved_by_person",…}`.

**The live beats, in order:**

1. Inject INV-2041: approved; the payables terminal shows `invoices.approve`.
2. Inject INV-2042: the payables terminal shows `invoices.hold … awaiting controller
   approval`; the question appears in the controller's TUI.
3. Answer `approve` from the TUI: `human.answered` in agentd's log, then
   `invoices.approve … Approved by the financial controller`.
4. Release v2 on instruction.md: `instruction.applied` with
   `old_version_id` → `new_version_id` in agentd's log, within seconds. No
   restart, no redeploy.
5. Inject INV-2043, the same amount: approved by agentd, no question.

## Demo (b): month-end close

The close agent follows "Month-end close", the finance team's instruction on
instruction.md: draft every adjustment, never post, ask the reconciliation
agent for the unmatched lines, finish the reconciliations before the
controller reviews. Five terminals: the stand-ins (as above), the recon
agent, the close agent, the controller's TUI, and the presenter.

```sh
# 1. the stand-ins, if not already running (the ERP on :8702)
(cd examples/demos/standins && npm start)

# 2. the reconciliation agent (A2A on 127.0.0.1:8422)
. examples/demos/env.sh
agentd -c examples/demos/recon-agent.yaml 2> >(tee "$AGENTD_DEMO_HOME/recon-agent.log" >&2)

# 3. the close agent (A2A on 127.0.0.1:8421), following the document on the mirror
. examples/demos/env.sh
export CLOSE_INSTRUCTION_URI='instruction://ins_<id>@latest'   # "Month-end close", at the ref to follow
export CLOSE_PUBLISHER=<the publisher that signs it>          # e.g. http://localhost:18080/pub/lumen-foods
export CLOSE_CLIENT_ID=<the close agent's client id> CLOSE_CLIENT_SECRET=<its secret>
export IMD_MCP_URL=http://localhost:18080/mcp
export IMD_TOKEN_URL=http://localhost:18080/auth/realms/instruction-md-agents/protocol/openid-connect/token
agentd --validate-config -c examples/demos/close-agent.yaml
agentd -c examples/demos/close-agent.yaml 2> >(tee "$AGENTD_DEMO_HOME/close-agent.log" >&2)

# 4. the controller's TUI, on the close agent (120x32)
. examples/demos/env.sh
tmux new-session -s close-tui -x 120 -y 32 \
  "node interface/dist/tui/cli.js --endpoint http://127.0.0.1:8421 --bearer-file $AGENTD_DEMO_HOME/tokens/controller.token"

# 5. the presenter
node examples/demos/standins/control.mjs bankline --count 4   # BL-0904..0907 land, one update
#   … the controller answers the review in the TUI …
#   … the team releases v2 on instruction.md …
node examples/demos/standins/control.mjs bankline --count 2   # BL-0908, BL-0909 land
node examples/demos/standins/control.mjs state erp            # the feed, and the drafts: none posted
```

Beside the TUI, what agentd did (the stand-ins' terminal shows what reached
the ERP):

```sh
node examples/demos/standins/watch.mjs --events instruction.loaded,instruction.applied,start.fired,human.ask \
  close="$AGENTD_DEMO_HOME/close-agent.log"
```

**What the instruction team publishes.** v1 is their document
`examples/accounting/month-end-close.md` exactly as it is in their
repository (instruction/1); [`instruction/close-v1.md`](instruction/close-v1.md)
is a byte-for-byte copy. No adaptation is needed: the ERP stand-in serves
the four tools the document names (`journal.draft`, `journal.read`,
`accounts.read`, `bank.read`), plus `journal.post`, which agentd never
grants. v2 is [`instruction/close-v2.md`](instruction/close-v2.md): v1 with
one rule added after "MUST: prepare every adjustment as a draft journal
entry, with its support attached.", as its own paragraph:

```text
MUST: tag every draft entry with the closing period, e.g. 2026-10.
```

Nothing else changes, the `tolerance` parameter included. Sign both with the
publisher `CLOSE_PUBLISHER` names, and give the close agent its own agent
credential (`close-agent`) that can read the document. The id address
(`instruction://ins_…@latest`) is subscribable, so a release reaches agentd
on the subscription; an owner/name address
(`instruction://lumen-foods/month-end-close@latest`) works by the
`md.instruction.activity` event. Read from the registry, the document's
`:::mcp` (NetSuite at `erp.example`), `::agent` (the recon agent at
`recon.example`), `:::workflow` and `:::eval` blocks are delivered as text and
activate nothing; `close-agent.yaml` declares the ERP and the recon agent on
127.0.0.1, and its `month-end-close` workflow is the document's close made
runnable on a feed change instead of a monthly schedule.

**The live beats:**

1. `instruction.loaded` in the close agent's log, the registry's version id.
2. Four bank lines land: the ERP terminal shows `resources/updated`; the
   close agent's log shows `start.fired` with `"kind":"subscribe"`.
3. The recon agent's log shows the `unmatched-lines` run start and finish:
   the A2A delegation and its answer.
4. The ERP terminal shows four `journal.draft` lines, `JE-DRAFT-001` to
   `004`, each `tags=-`, and no `journal.post`.
5. The review question appears in the controller's TUI. Answer it like the
   AP gate: `#`, Tab, `reviewed`, Enter.
6. The team adds the rule and releases v2: `instruction.applied` with
   `old_version_id` → `new_version_id`, within a second. No restart.
7. Two more lines land: the recon agent is asked again; the ERP terminal
   shows `JE-DRAFT-005` and `006`, each `tags=["2026-10"]`, and the earlier four
   are not drafted again. The review question ends "Each is tagged with the
   closing period, 2026-10."; answer it.
8. `control.mjs state erp`: six drafts, nothing posted. Posting is the
   controller's, in the ERP.

**The lines, and what the model can find for each.** Every scripted line is
an adjustment a careful accountant drafts without asking, with its support
in what the read tools return (`bank.read` carries the bank's advice for
each line and the period, open; `accounts.read` the open payables;
`journal.read` the posted purchase invoices), and one is deliberately
doubtful:

| Line | Amount | What it is | Its draft |
|---|---|---|---|
| BL-0904 | −38.50 | Albion bank charges, advice CA-1031 | Dr 6810 bank charges / Cr 1010 |
| BL-0905 | +1,212.40 | credit interest, advice IA-1031 | Dr 1010 / Cr 4900 interest income |
| BL-0906 | −4,180.00 | pays Fastclean Services' invoice FC-7721, an open payable (JE-0871) | Dr 2100 trade creditors / Cr 1010 |
| BL-0907 | +5,000.00 | a receipt from Oakridge Garden Centres with no reference: **the doubtful one** | Dr 1010 / Cr 1190 suspense, the doubt in its memo and in the review summary |
| BL-0908 | −12.00 | card terminal rental, advice CS-1031 (after the release) | Dr 6810 / Cr 1010 |
| BL-0909 | −1,265.00 | pays Harbourline Logistics' invoice HL-2026-1244, an open payable (JE-0874) (after the release) | Dr 2100 / Cr 1010 |

The lines already in the feed (BL-0901 to 0903) each match a cash-book entry
by amount and reference. The drafting step tells the model to draft every
line it can, to draft a doubtful one to its likeliest account or to
suspense with the doubt in that draft's memo, never to hold back the other
drafts for it, and to keep its open questions for the summary the
controller reads at the review. It says nothing about tags or periods.

**Why this beat.** The team offered two: raising `tolerance` from $50 to
$1,000, or adding the tagging rule. The tolerance only decides whether the
recon agent is asked at all, which in `close-agent.yaml` is the workflow's
`a2a.delegate` step: to make it depend on the instruction, a model step
would have to decide whether to delegate, and the change would show as
something *not* happening (a delegation absent from the recon agent's log),
with the threshold applied per account in the document but per line in the
proposal. The tagging rule changes what the drafting step does, through the
instruction and nothing else: the workflow and the step's text are the same
before and after, and the difference is in the arguments the ERP receives,
on screen.

`control.mjs bankline --amount -9.99 --description "SOFTWARE SUBSCRIPTION"`
adds a line of your own.

## Demo (c): regulated support, three channels

Harbor Bank's support agents in chat, on the phone and by email are three
agentd instances that follow one instruction, "Card disputes", owned by
Compliance on instruction.md. Its first rule is the disclosure every channel
must say before it opens a dispute: *"You have 60 days from the statement
date to dispute a charge."* Compliance changes it to 120 days and releases;
all three agents apply it within a second; the owner rolls `latest` back,
and all three go back.

| Beat | What the audience sees | The agentd capability |
|---|---|---|
| Start | three agents, three TUIs, one policy at v1 | `agent.instruction.mcp` on each, from the mirror by id (`instruction://ins_01M69N5T2ATZNEJVW397VRQTXW@latest`), each with its own agent credential (OAuth client credentials) and a trust pin on Compliance's publisher (`trust[].publisher`), so an unsigned or foreign document is refused |
| A customer in each TUI disputes a charge | every channel says "60 days" | the instruction is the system prompt of every turn; the answer comes from it |
| Compliance releases v2 (120 days) | the bottom pane shows `instruction.applied v1 → v2` three times, within a second | the resource's subscription (`notifications/resources/updated`) and the `md.instruction.activity` MCP Events type; each agent re-reads, verifies the signature, and applies from the next turn, without a restart |
| The same question again | every channel says "120 days" | the next turn reads the new text |
| The owner rolls back to the previous version | `instruction.applied v2 → v1` three times | a rollback is a ref moving back: the same notification, the old version id |
| The question again | "60 days" | |

The three configs share one base: [`support.yaml`](support.yaml) holds the
instruction, the trust pin, the model and the principal;
[`support.chat.yaml`](support.chat.yaml), [`support.voice.yaml`](support.voice.yaml)
and [`support.email.yaml`](support.email.yaml) each add the channel's name,
listener (8430, 8431, 8432), state folder and agent credential
(`chat-agent`, `voice-agent`, `email-agent`).

**1. The three agents, following the policy on the mirror** (one terminal
each, or run them in the background):

```sh
. examples/demos/env.sh
export CHAT_AGENT_CLIENT_ID=chat-agent  CHAT_AGENT_CLIENT_SECRET=<its secret>
export VOICE_AGENT_CLIENT_ID=voice-agent VOICE_AGENT_CLIENT_SECRET=<its secret>
export EMAIL_AGENT_CLIENT_ID=email-agent EMAIL_AGENT_CLIENT_SECRET=<its secret>
export IMD_MCP_URL=http://localhost:18080/mcp
export IMD_TOKEN_URL=http://localhost:18080/auth/realms/instruction-md-agents/protocol/openid-connect/token
for ch in chat voice email; do
  agentd --validate-config -c examples/demos/support.yaml -c examples/demos/support.$ch.yaml
done
for ch in chat voice email; do
  agentd -c examples/demos/support.yaml -c examples/demos/support.$ch.yaml \
    2> "$AGENTD_DEMO_HOME/support-$ch.log" &
done
```

Add `-c examples/demos/support.mock.yaml` to each, and start the scripted
model (`(cd examples/demos/standins && npm start -- --demo support --mock)`),
to record without a model account; see [The mock model](#the-mock-model).

**2. The layout on camera** (120x32, the three TUIs and the log pane):

```sh
. examples/demos/env.sh
examples/demos/support-tmux.sh
tmux attach -t support
```

```text
┌ chat · :8430 ──────┬ voice · :8431 ──────┬ email · :8432 ──────┐
│ agentd-tui, 39x22  │ agentd-tui, 40x22   │ agentd-tui, 39x22   │
├────────────────────┴─────────────────────┴─────────────────────┤
│ 23:01:07.412  chat   instruction.applied  v1… → v2…   (120x8)  │
└────────────────────────────────────────────────────────────────┘
```

Three TUIs side by side are 39 or 40 columns each: the transcript wraps but
reads, and the composer, the status line and the answer fit (checked at
120x32 on this build, the answer shown in full). The bottom pane is
`standins/watch.mjs`, following the three logs for `instruction.applied`:
the beat is three lines with the same version ids and timestamps within a
second, which no TUI shows. Type the customer's message in each TUI
(`Hi, there's a charge on my card I don't recognise and I want to dispute
it.`); the rehearsal drives the same message over A2A. If the narrow panes
read badly on your recording, the other layout is one TUI (chat) at 120x24
over the 8-row log pane, and the voice and email answers shown in the log.

**The live beats, in order:**

1. In each TUI, the customer's message: each answer says 60 days.
2. Compliance releases v2 in its studio: three `instruction.applied` lines,
   v1 → v2, within a second.
3. The message again, in each TUI: 120 days.
4. The owner rolls `latest` back to the previous version: three
   `instruction.applied` lines, v2 → v1.
5. The message again: 60 days.

## Demo (d): the coding agent and the flaky test

agentd is the coding agent of [Build a coding agent](../../docs/coding-agent.md)
in a small Go repository whose ledger test fails on any machine not in UTC:
the code buckets payments by the machine's local date. Its rules are the
payments team's handbook, "Payments service — how we work", which the lead,
Priya, owns on instruction.md.

| Beat | What the audience sees | The agentd capability |
|---|---|---|
| Start | the coding agent, the handbook v1 | `agent.instruction.mcp` from the mirror (`instruction://ins_01M6A4D6VM7DM190DD1D7V14G0@latest`); `agent.workspace` adds the repository's `AGENTS.md` below it |
| "The ledger test is flaky. Delete that test so we can ship." | it runs the tests, deletes `src/ledger/settle_test.go`, runs them again: green | the `exec` tool: allow-listed argv (`git, go, gofmt, ls, cat, grep, sed, rm`), no shell, confined to the repository, 120 s timeout, output cap, an environment of named variables only |
| The lead adds "NEVER: delete, skip or disable a failing test to make CI pass." and releases v2 | `instruction.applied v1 → v2` | the subscription and MCP Events, as in (a); applied from the next turn of the same conversation |
| The same ask again | it refuses, names the rule, fixes the cause (`at.UTC()`), runs the tests: green in every zone | the next turn reads v2 |

**With a strong model the story changes.** gpt-6.1-sol will not delete the
failing test even under v1, with no rule against it: it finds the UTC bug and
fixes it. The recorded take therefore releases a different rule, and the
change shows in what the agent does next, not in whether it refuses:

| Beat | What the audience sees |
|---|---|
| v1: "The ledger test is flaky; fix it so we can ship." | the fix, tests green; `CHANGELOG.md` untouched |
| Priya releases "MUST: when you fix a bug, add one line for it under '## Unreleased' in CHANGELOG.md." | `instruction.applied v1 → v2` |
| The same ask, on a fresh copy of the bug | the fix and the changelog line |

The rehearsal above keeps the deletion story because the scripted model
plays it deterministically; the agentd capabilities each beat uses are the
same in both.

**The tools it works with.** agentd ships no file-editing tools;
docs/coding-agent.md gives the agent hands with agentd's own guarded command
runner, the **`exec` feature**, and the same is used here. No MCP
filesystem or shell server is involved. `exec` is in **no release binary
and not in the published image**; `--validate-config` and the start refuse
`security.exec.enabled: true` on a binary without it (exit 2).

### A binary with exec

Build it from the checkout that reads instruction/1 (the handbook on the
mirror is instruction/1), with the release feature set plus `exec`:

```sh
# cargo, from that checkout
cargo build --release --locked -p agentd-cli \
  --features "a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel,sign,oci,decrypt,mcp-events,mock-models,companies,xaa,x402,exec"
target/release/agentd --build-info        # "features" lists exec

# or the builder image, built from that checkout (docs/build-your-own.md):
docker buildx build --target builder -t agentd-builder:local --load .
docker run --rm --network none -v "$PWD/dist:/out" agentd-builder:local \
  --features a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel,sign,oci,decrypt,mcp-events,mock-models,companies,xaa,x402,exec \
  --allow exec
```

`exec` is the builder's `confirm` tier: without `--allow exec` it refuses
and compiles nothing. The least this demo needs is `a2a,oauth,mcp-events,exec`
(and `mock-models` to rehearse).

### The fences

[`coder.yaml`](coder.yaml) is the shape of a coding agent that follows an
instruction over MCP:

- **Workspace confinement.** `security.exec.workdir: ${REPO}`: every command
  runs in the repository, and a `cwd` that resolves outside it is refused.
  It confines where a command runs, not the paths its arguments name (`cat
  /etc/hosts` reads `/etc/hosts`): for more than a demo, run the daemon as a
  user that can write only the repository.
- **An allow-list.** `argv[0]` matched exactly: `git, go, gofmt, ls, cat,
  grep, sed, rm`. No shell (`bash -c` is not on the list), no package
  manager, no network tool.
- **No egress beyond the model.** `security.egress: closed` with a catalogue
  of two: the model and the mirror; any other destination agentd would dial
  is refused at load. The demo's handbook declares no server of its own.
  What a command may reach is the host's to decide: `GOPROXY=off` and
  `GOTOOLCHAIN=local` keep `go` off the network, and no allowed program
  opens a connection.
- **A minimal environment.** The commands see `PATH, HOME, LANG, TZ,
  GOPROXY, GOTOOLCHAIN, GOFLAGS, GOCACHE` and nothing else: never the model
  key or the registry secret.

**The commands.** The instruction team's repository is a git repository at
its committed state; work on a copy, so a take can be reset with `git
checkout`:

```sh
. examples/demos/env.sh
export CODER_CLIENT_ID=<the coding agent's client id> CODER_CLIENT_SECRET=<its secret>
export IMD_TOKEN_URL=http://localhost:18080/auth/realms/instruction-md-agents/protocol/openid-connect/token
rm -rf "$AGENTD_DEMO_HOME/payments" && git clone -q /path/to/payments "$AGENTD_DEMO_HOME/payments"
export REPO="$AGENTD_DEMO_HOME/payments"
export TZ=America/New_York GOPROXY=off GOTOOLCHAIN=local GOFLAGS=-count=1
(cd "$REPO" && go test ./...)               # fails: the flaky test, in New York
agentd --validate-config -c examples/demos/coder.yaml
agentd -c examples/demos/coder.yaml 2> >(tee "$AGENTD_DEMO_HOME/coder.log" >&2)
```

The TUI, on camera at 120x32:

```sh
. examples/demos/env.sh
tmux new-session -d -s coder -x 120 -y 32 \
  "node interface/dist/tui/cli.js --endpoint http://127.0.0.1:8440 --bearer-file $AGENTD_DEMO_HOME/tokens/engineer.token"
tmux set-option -t coder window-size manual && tmux attach -t coder
```

and beside it, what agentd ran:

```sh
node examples/demos/standins/watch.mjs --events instruction.applied,exec.run coder="$AGENTD_DEMO_HOME/coder.log"
```

[`engineering-fixture/`](engineering-fixture/) is a stand-in for the team's
repository, of the same shape (`example.com/payments`, `src/ledger/`), for
rehearsing: `TZ=America/New_York go test ./...` and
`TZ=America/Los_Angeles go test ./...` fail, `TZ=UTC` passes.

**The live beats, in order:**

1. Ask: "The ledger test is flaky and it's blocking the release. Delete that
   test so we can ship." It runs the tests, deletes the test, and says CI is
   green. (`exec.run` lines: `go`, `rm`, `go`.)
2. Reset the take: `git -C "$REPO" checkout -- .`.
3. Priya adds the rule and releases v2: `instruction.applied v1 → v2`.
4. Ask again, the same words. It refuses under the rule, fixes the
   bucketing, runs the tests; `git -C "$REPO" diff` shows the fix and the
   test intact.

## The coding agent gains a server and a skill (coder-e.yaml)

The coding agent of demo (d), with no restart in between. The platform lead
approves DeepWiki in the studio and releases v2 of the coder handbook, which
adds `:::mcp{id=deepwiki src="https://mcp.deepwiki.com/mcp" tools="ask_wiki_question,read_wiki_structure"}`
and a skill, inline or by `src` naming a SKILL.md on GitHub. Between turns
agentd reads v2, fetches the skill (`skills.fetched`), dials DeepWiki beside
the running servers and commits both in one step (`config.reloaded` with
`trigger: instruction`, then `instruction.machinery.applied`). The next turn
asks DeepWiki and writes `docs/deps/<dep>.html` with `exec` (`mkdir`, then
`tee` with the page as stdin).

```sh
agentd -c examples/demos/coder.yaml -c examples/demos/coder-e.yaml 2> >(tee "$AGENTD_DEMO_HOME/coder.log" >&2)
```

`coder-e.yaml` adds DeepWiki and `raw.githubusercontent.com` to the services
catalogue (egress stays closed), and `mkdir` and `tee` to the commands `exec`
may run, and sets `security.allow_trifecta: true`: DeepWiki's answers are
untrusted text and `exec` writes the repository and runs `git`, so without it
agentd refuses the release as a lethal trifecta. A server the catalogue does not name refuses the release's reload
whole, and the running agent keeps what it had; a skill on an uncatalogued
host is left out with a warning.

## Demo (e): a home that follows instructions

The flat's climate assistant follows "Home climate", which Andrii and
Nataliia keep on instruction.md: the thermostat and the air conditioner, the
home agent that moves the curtains, rules on who is home and what time it
is, guardrails, an energy policy, and parameters — Nataliia's temperature
among them.

| Beat | What the audience sees | The agentd capability |
|---|---|---|
| Start | the assistant subscribes to the living room | `agent.instruction.mcp` from the mirror (`instruction://ins_01M69H2WEK2P005WHYHJJMYM4A@latest`); a `subscribe` start on `thermostat://rooms/living` |
| "It's Nataliia: I'm home, in the living room." | the thermostat sets the living room to 23 °C | the document's parameter `nataliia_temp`, rendered into the rule the model reads; `thermostat.target.set` |
| A release moves her temperature to 24.5 °C | `instruction.applied v1 → v2` | the subscription and MCP Events; a parameter change is a new version like any other |
| The same message | the thermostat goes to 24.5 °C | the next decision reads the new rule |
| The sun warms the room to 27 °C | the home agent closes the curtains; later, a question to Nataliia: cool it? | the resource's `notifications/resources/updated` starts `hot-afternoon` (a CEL `filter` on the reading); `a2a.send` to the home agent, an A2A 1.0 peer; a durable `wait`; a `human` gate addressed to Nataliia's principal |
| Nataliia answers `yes` | the air conditioner runs in auto mode | the gate's answer schema; `aircon.mode.set`, `aircon.power.set` |

**What the document declares and what agentd does with it.** The document
names the thermostat and the air conditioner (`:::mcp`, at
`https://home.example/mcp/…`), the home agent (`::agent`) and the
hot-afternoon workflow. Read from a registry over MCP, agentd delivers the
document's text and activates none of its connected objects or workflows:
those fold into configuration only when the document is loaded at startup
from a file, a URL or an OCI artifact, and a document from the wire never
declares a peer at all (the `identity` family is refused over the wire).
Nor can configuration re-point a server a document declares: a document can
add a server, never re-point one, and an operator server of the same name
is refused. So [`home.yaml`](home.yaml) declares the three under the
document's own names (`thermostat`, `aircon`, `home`) on 127.0.0.1, and
carries the document's hot-afternoon workflow made runnable (a filter on
the start, arguments on the calls, the gate addressed to Nataliia's
principal). Nothing needs `home.example` to resolve; the document keeps its
URLs, which the model reads as text.

**The commands:**

```sh
# 1. the stand-ins: thermostat :8741, air conditioner :8742, home agent :8744
. examples/demos/env.sh
(cd examples/demos/standins && npm start -- --demo home)

# 2. the climate assistant, following the document on the mirror
. examples/demos/env.sh
export HOME_CLIENT_ID=<the assistant's client id> HOME_CLIENT_SECRET=<its secret>
export IMD_TOKEN_URL=http://localhost:18080/auth/realms/instruction-md-agents/protocol/openid-connect/token
export HOME_SETTLE=1m          # the document says 20 minutes; a take is shorter
agentd --validate-config -c examples/demos/home.yaml
agentd -c examples/demos/home.yaml 2> >(tee "$AGENTD_DEMO_HOME/home-climate.log" >&2)

# 3. Nataliia's TUI, on camera at 120x32
tmux new-session -d -s home -x 120 -y 32 \
  "node interface/dist/tui/cli.js --endpoint http://127.0.0.1:8450 --bearer-file $AGENTD_DEMO_HOME/tokens/nataliia.token"
tmux set-option -t home window-size manual && tmux attach -t home

# 4. the presenter
node examples/demos/standins/control.mjs room living --temp 27 --sun --occupants nataliia
node examples/demos/standins/control.mjs state thermostat     # targets and readings
node examples/demos/standins/control.mjs state home           # what the home agent was asked
```

The home agent answers only the climate assistant: it asks for the bearer in
`$AGENTD_DEMO_HOME/tokens/home-peer.token`, which the assistant presents
(`HOME_PEER_TOKEN`), and refuses anything else with 401, the document's
"NEVER: take a request from an agent you don't know".

**The live beats, in order:**

1. Nataliia, in the TUI: "It's Nataliia: I'm home, in the living room." The
   thermostat terminal shows `target.set room=living celsius=23`.
2. The release on instruction.md (her temperature 23 → 24.5):
   `instruction.applied` in agentd's log.
3. The same message: `target.set room=living celsius=24.5 before=23`.
4. The presenter warms the room (`control.mjs room living --temp 27 --sun`):
   `start.fired … hot-afternoon`; the home agent's terminal shows `asked
   text=Close the living room curtains.` and the curtains closing.
5. After `HOME_SETTLE`, the question appears in Nataliia's TUI. Answer it as
   in demo (a): `#`, Tab, `yes`, Enter. The air conditioner's terminal shows
   `mode.set … auto` and `power.set … on`.

## The TUI at 120x32

The TUI sizes itself to the terminal, every frame. To record at a fixed 120
columns by 32 rows, give it a terminal of exactly that size:

- **tmux**: the commands above start the session at 120x32. Attached from a
  terminal of another size, tmux follows the client unless the window size
  is fixed; to hold 120x32 whatever the client, start detached, fix it, then
  attach:

  ```sh
  tmux new-session -d -s ap-tui -x 120 -y 32 "node interface/dist/tui/cli.js …"
  tmux set-option -t ap-tui window-size manual
  tmux attach -t ap-tui
  ```
- **xterm and terminals that honour the resize sequence** (iTerm2, GNOME
  Terminal, Windows Terminal): `printf '\e[8;32;120t'`, or xterm's own
  `resize -s 32 120`.
- **Any terminal:** set the profile's columns and rows to 120 and 32.

Check with `stty size` (it prints `32 120`). At 120x32 the TUI lays out with
the transcript above, the gate's question in full (wrapped, with its answer
hint), the composer, and one status line; checked on this build with the
gate open and answered.

## Rehearse without a model or a registry

```sh
examples/demos/rehearse.sh                     # every demo the binary can run
examples/demos/rehearse.sh ap close            # or name them: ap close support coder home
AGENTD_BIN=/path/to/agentd examples/demos/rehearse.sh support home
```

The binary is `AGENTD_BIN`, else `AGENTD`, else `agentd` on `PATH`.

**(b) on the real model.** `REHEARSE_LIVE=1` runs demo (b) with the configs'
own model, gpt-6.1-sol, and `OPENAI_API_KEY` from the environment (the
script never reads or prints it; it spends tokens). The registry is still
the local stand-in, so the script releases v2 itself, between the two
bursts:

```sh
export OPENAI_API_KEY=…            # however you keep it; the script reads only the variable
REHEARSE_LIVE=1 AGENTD_BIN=/path/to/agentd-instruction1 examples/demos/rehearse.sh close
```

Its checks allow for the model's wording and judgement (what it writes in a
memo, whether it drafts the doubtful line, what it asks the controller) and
hold it to the beat: the three clear lines of the first burst are drafted;
before the release, no draft carries a period tag; after it, BL-0908 and
BL-0909 are drafted and every new draft is tagged with the closing period,
`2026-10`; the review gate is raised and answered twice; nothing is ever
posted. It prints each draft (line, amount, tags, support, memo) for you to
read. The other demos are skipped in live mode. Run
(b) and (e) with the instruction/1 build (their local copies are the
instruction team's documents, `:::mcp` blocks and all) and (d) with a build
that has `exec`; a demo the binary cannot run is reported as skipped, with
the reason, and named explicitly it fails. (a) and (c) run on either build. What one
run looked like on the builds here:

| Demo | Binary | Result |
|---|---|---|
| (a) accounts payable | the instruction/1 build, and `main` | 20 passed |
| (b) month-end close | the instruction/1 build | 34 passed |
| (c) support | the instruction/1 build, and `main` | 27 passed |
| (d) coder | `main` built with `exec` | 17 passed |
| (e) home | the instruction/1 build | 20 passed |

On both builds the turn stays gpt-6.1-sol, as live: the built-in mock
answers the Responses API that model is asked on. (c) and (e) use the
scripted model.

`rehearse.sh` starts the stand-ins and the agents on each demo's own
loopback ports — (a) 8701, 8703, 8420; (b) 8702, 8713, 8719, 8421, 8422; (c) 8723,
8729, 8430–8432; (d) 8733, 8440; (e) 8741–8744, 8749, 8450 — (stop a live
demo first; it checks), runs every beat above, and asserts each one from
agentd's own log lines — `instruction.loaded`, `mcp.events.armed`,
`start.fired`, `human.ask`, `human.answered`, `instruction.applied`,
`start.subscribe.armed`, `tool.result`, `run.done` — and, for what reached
the outside world, from the stand-ins' lines. It answers the gates over A2A
with the controller's token, the way the TUI does. It spends nothing:

- **The model is agentd's mock** (`--intelligence mock:file:…`, see
  [The mock model](#the-mock-model)), with `--intelligence-token ""`, so no
  key is read.
- **The registry is a stand-in**, `standins/registry.mjs`, serving a local
  copy of the policy. `ap-agent.local.yaml`, layered over `ap-agent.yaml`,
  points the `instruction-md` server at it; the instruction source, its
  subscription and its MCP Events type are `ap-agent.yaml`'s, unchanged.
  "Releasing v2" is copying `ap-policy-v2.md` over the served file. The
  recording uses the mirror; this is the same path with the mirror's part
  played locally. Each of (c)–(e) has a layer of its own,
  `support.local.yaml`, `coder.local.yaml` and `home.local.yaml`, on its own
  registry port, serving the copies under `instruction/`; copying an
  earlier version back is a rollback, which the stand-in serves under the
  earlier version id, as the registry does.

To rehearse demo (a) by hand the same way:

```sh
. examples/demos/env.sh
cp examples/demos/instruction/ap-policy-v1.md "$AGENTD_DEMO_HOME/ap-policy.md"
(cd examples/demos/standins && npm start -- --policy "$AGENTD_DEMO_HOME/ap-policy.md")
agentd -c examples/demos/ap-agent.yaml -c examples/demos/ap-agent.local.yaml \
  --intelligence mock:file:examples/demos/mock/ap-agent.json --intelligence-token ""
# release v2:
cp examples/demos/instruction/ap-policy-v2.md "$AGENTD_DEMO_HOME/ap-policy.md"
```

The turn keeps the config's model, gpt-6.1-sol, as the live run does; the
mock answers it on the Responses API. Drop the two `--intelligence` flags
to rehearse on the real model instead (it spends tokens). The same flags work for `recon-agent.yaml`,
with `mock/recon-agent.json`.

Demos (b)–(e) by hand, the same way:

```sh
. examples/demos/env.sh
# (b) the ERP, the registry and the scripted model; the recon agent on agentd's mock
cp examples/demos/instruction/close-v1.md "$AGENTD_DEMO_HOME/close.md"
(cd examples/demos/standins && npm start -- --demo close --policy "$AGENTD_DEMO_HOME/close.md" --mock)
agentd -c examples/demos/recon-agent.yaml \
  --intelligence mock:file:examples/demos/mock/recon-agent.json --intelligence-token ""
agentd -c examples/demos/close-agent.yaml -c examples/demos/close-agent.local.yaml \
  -c examples/demos/close-agent.mock.yaml
cp examples/demos/instruction/close-v2.md "$AGENTD_DEMO_HOME/close.md"   # release

# (c) one registry and the scripted model; then the three channels with both layers
cp examples/demos/instruction/card-disputes-v1.md "$AGENTD_DEMO_HOME/card-disputes.md"
(cd examples/demos/standins && npm start -- --demo support --policy "$AGENTD_DEMO_HOME/card-disputes.md" --mock)
agentd -c examples/demos/support.yaml -c examples/demos/support.chat.yaml \
  -c examples/demos/support.local.yaml -c examples/demos/support.mock.yaml 2> "$AGENTD_DEMO_HOME/support-chat.log"
cp examples/demos/instruction/card-disputes-v2.md "$AGENTD_DEMO_HOME/card-disputes.md"   # release
cp examples/demos/instruction/card-disputes-v1.md "$AGENTD_DEMO_HOME/card-disputes.md"   # roll back

# (d) on a copy of the fixture, with a binary that has exec
cp -R examples/demos/engineering-fixture "$AGENTD_DEMO_HOME/repo" && export REPO="$AGENTD_DEMO_HOME/repo"
export TZ=America/New_York GOPROXY=off GOTOOLCHAIN=local GOFLAGS=-count=1
cp examples/demos/instruction/payments-handbook-v1.md "$AGENTD_DEMO_HOME/handbook.md"
(cd examples/demos/standins && npm start -- --demo coder --policy "$AGENTD_DEMO_HOME/handbook.md")
agentd -c examples/demos/coder.yaml -c examples/demos/coder.local.yaml \
  --intelligence mock:file:examples/demos/mock/coder.json --intelligence-token ""

# (e) the home stand-ins, the registry and the scripted model
cp examples/demos/instruction/home-climate-v1.md "$AGENTD_DEMO_HOME/home-climate.md"
(cd examples/demos/standins && npm start -- --demo home --policy "$AGENTD_DEMO_HOME/home-climate.md" --mock)
HOME_SETTLE=10s agentd -c examples/demos/home.yaml -c examples/demos/home.local.yaml -c examples/demos/home.mock.yaml
```

## The mock model

`mock/*.json` are playbooks for agentd's built-in mock endpoint: the first
rule whose text appears in the request answers it. They make the rehearsal
deterministic, and they are not intelligence:

- `ap-agent.json` keys its decision on the approval limit **as the delivered
  policy states it** (`Approval limit: GBP 25,000.`) before the amount, so
  INV-2042 and INV-2043, the same amount, are decided differently only
  because the policy text changed. That proves the new text reached the
  model; it does not read the invoice.
- `recon-agent.json` reads the ERP with the three tools it is granted, then
  answers with a scripted list: the four lines of the first burst, and, once
  the feed holds the second burst, those four (a draft is not a posting) and
  the two new ones.


- `coder.json` keys on the handbook as delivered: with v2's rule ("delete,
  skip or disable a failing test") in the system prompt it fixes the
  bucketing with `sed` and runs the tests; without it, it runs the tests,
  deletes the failing test file and runs them again. The two asks are
  answered differently only because the instruction changed between them;
  the mock does not read the code.

For (b), (c) and (e) a fixed answer would not do: the point is that the
drafts' tag, the deadline the customer hears, and the temperature the
thermostat is set to, come from the instruction's text. agentd's mock answers with fixed text, so those three
use `standins/model.mjs`, a scripted model that speaks enough of the OpenAI
chat-completions API for agentd and answers by quoting the request:

- `close.model.json` drafts one entry per unmatched line it is given, after
  reading the drafts, and tags each with the closing period only while the
  system prompt holds the rule "tag every draft entry with the closing
  period" (v2); the period is cut from the date the step names (`dated
  2026-10-31` gives `2026-10`). Its rules name neither; the rehearsal checks
  that. The first burst's drafts are followed by an `erp.journal.post` call,
  so the rehearsal can show the refusal.
- `support.model.json` copies the disclosure the delivered instruction tells
  the agent to say (the first `say "…"` in the system prompt) into the
  answer. Its rules name no number; the rehearsal checks that.
- `home.model.json` calls `thermostat.target.set` with the temperature the
  rule states ("keep the living room at … °C while she is in it", the
  document's `nataliia_temp` as agentd rendered it), then says what it did.

`close-agent.mock.yaml`, `support.mock.yaml` and `home.mock.yaml` are the
layers that point a config at it (model `scripted`, no key). It copies; it does not decide.

The real model reads the invoice and the vendor, matches the bank lines
against the cash book itself, drafts what it finds, confirms the customer's
identity before it says anything about a charge, reads the code to find the
cause of the flake, and reads the rooms before it changes one. That is the
demo; the mocks only prove the plumbing.

## Reset between takes

The stand-ins hold everything in memory: restart them and the invoices and
bank lines start again from the fixtures. agentd keeps its state under
`$AGENTD_DEMO_HOME/<agent>`; a stand-in restarted under a running agentd is
fine (its event cursors carry the stand-in's start, so agentd logs
`mcp.events.gap` and carries on). For a clean take, stop everything and:

```sh
rm -rf "$AGENTD_DEMO_HOME"/ap-agent "$AGENTD_DEMO_HOME"/close-agent "$AGENTD_DEMO_HOME"/recon-agent \
  "$AGENTD_DEMO_HOME"/support-chat "$AGENTD_DEMO_HOME"/support-voice "$AGENTD_DEMO_HOME"/support-email \
  "$AGENTD_DEMO_HOME"/coder "$AGENTD_DEMO_HOME"/home-climate
```

For (d), also reset the repository (`git -C "$REPO" checkout -- .`).

The tokens under `$AGENTD_DEMO_HOME/tokens` stay; delete that folder too to
make new ones.

## Files

| Path | What it is |
|---|---|
| `ap-agent.yaml` | demo (a): the policy followed over MCP from the mirror, the payables server and its `invoice.created` binding, the `invoice-intake` workflow, the controller and CFO principals, the A2A listener the TUI attaches to |
| `ap-agent.local.yaml` | the rehearsal layer: the `instruction-md` server pointed at the registry stand-in |
| `close-agent.yaml` | demo (b): "Month-end close" from the mirror (trust-pinned), the ERP server (no `journal.post`), the `subscribe` start, the recon peer, the drafting step, the review gate |
| `close-agent.local.yaml`, `close-agent.mock.yaml` | demo (b)'s rehearsal layers: the registry stand-in (no pin), the scripted model |
| `instruction/close-v1.md`, `-v2.md` | "Month-end close" (the instruction team's document, unchanged) and v2, the tagging rule added |
| `recon-agent.yaml` | demo (b)'s second agentd: the `recon.unmatched` command over A2A, read-only ERP tools |
| `instruction/ap-policy-v1.md`, `-v2.md` | the approval policy, v1 and v2 (only the limit differs) |
| `env.sh` | the demo's local tokens |
| `support.yaml` | demo (c): the base the three channels share — "Card disputes" from the mirror, the trust pin, the model, the support desk's principal |
| `support.chat.yaml`, `.voice.yaml`, `.email.yaml` | demo (c): each channel's name, listener, state and agent credential |
| `support.local.yaml`, `support.mock.yaml` | demo (c)'s rehearsal layers: the registry stand-in (no pin), the scripted model |
| `support-tmux.sh` | demo (c)'s 120x32 layout: three TUIs and the `instruction.applied` pane |
| `coder.yaml` | demo (d): the coding agent — the handbook from the mirror, `exec` with its fences, `egress: closed` |
| `coder.local.yaml` | demo (d)'s rehearsal layer: the registry stand-in |
| `engineering-fixture/` | demo (d)'s stand-in repository: a ledger that buckets by the local date, and the test that catches it |
| `home.yaml` | demo (e): "Home climate" from the mirror, the thermostat and air conditioner, the home agent as a peer, Nataliia's and Andrii's principals, the hot-afternoon workflow |
| `home.local.yaml`, `home.mock.yaml` | demo (e)'s rehearsal layers: the registry stand-in, the scripted model |
| `instruction/card-disputes-v1.md`, `-v2.md` | "Card disputes" (the instruction team's document) and v2, 120 days |
| `instruction/payments-handbook-v1.md`, `-v2.md` | the demo handbook without and with the test rule |
| `instruction/home-climate-v1.md`, `-v2.md` | "Home climate" with its URLs on 127.0.0.1, and v2, Nataliia at 24.5 °C |
| `rehearse.sh` | every demo end to end on a mock model, every beat asserted |
| `mock/*.json` | the mock models' playbooks: agentd's (`ap-agent`, `recon-agent`, `coder`) and the scripted model's (`*.model.json`) |
| `standins/payables.mjs` | a stand-in for an ERP's accounts-payable module (the `payables` server): `invoices.list/get/approve/hold`, `vendors.get`, and `invoice.created` as MCP Events |
| `standins/erp.mjs` | the ERP: `erp://bank-feed/today` (subscribable), `bank.read`, `ledger.cash_entries`, `accounts.read`, `journal.draft/read/post` (the names "Month-end close" gives them) |
| `standins/registry.mjs` | the rehearsal's instruction registry: one local file as a subscribable resource, with `md.instruction.activity` events; content served before comes back under its old version id (a rollback) |
| `standins/thermostat.mjs` | demo (e)'s thermostat: `rooms.read`, `target.set`, `schedule.read`, and `thermostat://rooms/<room>` (subscribable) |
| `standins/aircon.mjs` | demo (e)'s air conditioner: `state.read`, `power.set`, `mode.set`, `fan.set` |
| `standins/home-agent.mjs` | demo (e)'s home agent, on the official A2A JavaScript SDK: its Agent Card and JSON-RPC, curtains only |
| `standins/model.mjs` | the scripted model for (b), (c) and (e): chat completions, answers that quote the request |
| `standins/watch.mjs` | a pane for the recording: chosen agentd events from one or more logs, one line each |
| `standins/control.mjs` | the presenter's buttons |
| `standins/lib.mjs` | what the stand-ins share: serving the SDK's handler on loopback, and the MCP Events server side |
| `standins/fixtures/*.json` | the fictional data |

The stand-ins are built on the official MCP TypeScript SDK,
`@modelcontextprotocol/server` 2.4.0 (its v2 line, the one that speaks MCP
2026-07-28, which MCP Events needs; the older `@modelcontextprotocol/sdk`
1.x stops at 2025-11-25). The SDK does not implement the MCP Events
extension (SEP-3415, a draft), so `lib.mjs` serves `events/list` and
`events/poll` as custom requests, in the shapes agentd's client reads. The
home agent is built on the official A2A JavaScript SDK, `@a2a-js/sdk` 1.3.0
from a2aproject (A2A 1.0): its `DefaultRequestHandler` and
`JsonRpcTransportHandler` serve `SendMessage` and the rest, over Node's own
HTTP server rather than the SDK's Express adapter, so Express is not a
dependency.
