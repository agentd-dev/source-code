# Two finance demos, run by agentd

Two short demos in which agentd does the work with its own machinery: MCP
Events, MCP resource subscriptions, workflows, A2A between two agentd
instances, the human-in-the-loop gate, and the terminal UI on screen. The
company, the vendors, the bank and the people are fictional (Northwind
Supplies Ltd.); so is every number.

| Demo | The story | Files |
|---|---|---|
| **(a) Accounts payable** | Invoices arrive from the AP system. agentd decides each one under an approval policy the controller owns on instruction.md. Above the limit, it holds the invoice and asks the controller (or the CFO). The controller raises the limit and releases v2 of the policy; seconds later the same amount is approved without asking. | `ap-agent.yaml`, `instruction/`, `standins/payables.mjs` |
| **(b) Month-end close** | A bank line lands in the ERP's feed. agentd asks a second agentd, the reconciliation agent, which lines nothing in the ledger explains, drafts one journal entry per line, and stops at a review gate. It cannot post: the tool is never granted. | `close-agent.yaml`, `recon-agent.yaml`, `standins/erp.mjs` |

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
| Start | the close agent subscribes to `erp://bank-feed/today` | a `subscribe` start on an MCP resource (`start.subscribe.armed`) |
| Four bank lines land | the workflow wakes once for the burst | `notifications/resources/updated` → agentd reads the resource again and fires the run (notify-then-read, `debounce_ms: 2000`) |
| "Which lines are unmatched?" | the recon agent's log shows the request and its answer | `a2a.delegate` with a typed command (`recon.unmatched`) to a second agentd, whose `a2a` start admits only the close agent's credential, granted that one command |
| Drafts | four journal entries appear in the ERP as drafts | an `agent` step with `erp.journal.draft` and an `output_schema` |
| "Can it post?" | no | the ERP has `journal.post`, as real ERPs do; `close-agent.yaml`'s server `allow` list never names it, so the tool does not exist for this agent. A model that tries is refused inside agentd and the ERP never sees the call (the rehearsal's mock tries, on purpose) |
| Review | the question in the controller's TUI: review and post in the ERP | a `human` gate addressed to the controller; posting stays a person's act |

## Prerequisites

- **agentd** from `main` or a release binary: the release feature set has
  everything used here (`mcp-events`, `a2a`, `cel`, `oauth`, `mock-models`).
  Check with `agentd --version`.
- **Node.js 20 or later** and npm, for the stand-ins and the TUI; `curl`, for
  the rehearsal.
- **The TUI**, `agentd-tui` from `@agentd-dev/cli`. From this repository:

  ```sh
  (cd interface && npm ci && npm run build:node)   # then: node interface/dist/tui/cli.js
  ```

  or `npm install -g @agentd-dev/cli`, which puts `agentd-tui` on `PATH`.
- **For the recording:** `OPENAI_API_KEY` in the environment (the configs
  name the model `gpt-5.6-sol` and reference the key only as
  `{{secret:OPENAI_API_KEY}}`), and for demo (a), the instruction.md mirror
  with the policy published and an agent credential for it.

Every command below runs from the repository root. Start the stand-ins'
dependencies once:

```sh
(cd examples/demos/standins && npm ci)
```

### The demo credentials

```sh
. examples/demos/env.sh
```

`env.sh` makes three local tokens once, under `$AGENTD_DEMO_HOME/tokens`
(default `/tmp/agentd-demos/tokens`, mode 600), and exports them for the
configs' `{{secret:…}}` references: `CONTROLLER_TOKEN` (Morgan Ellis, the
financial controller), `CFO_TOKEN` (Sam Okafor, the CFO) and
`RECON_PEER_TOKEN` (the close agent's credential at the recon agent). The
TUIs sign in with the same files. Source it in every terminal that starts an
agent or a TUI.

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
export IMD_TOKEN_URL=http://localhost:8080/realms/instruction-md-agents/protocol/openid-connect/token
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

Five terminals: the stand-ins (as above), the recon agent, the close agent,
the controller's TUI, and the presenter.

```sh
# 1. the stand-ins, if not already running
(cd examples/demos/standins && npm start)

# 2. the reconciliation agent (A2A on 127.0.0.1:8422)
. examples/demos/env.sh
agentd -c examples/demos/recon-agent.yaml 2> >(tee "$AGENTD_DEMO_HOME/recon-agent.log" >&2)

# 3. the close agent (A2A on 127.0.0.1:8421)
. examples/demos/env.sh
agentd -c examples/demos/close-agent.yaml 2> >(tee "$AGENTD_DEMO_HOME/close-agent.log" >&2)

# 4. the controller's TUI, on the close agent (120x32)
. examples/demos/env.sh
tmux new-session -s close-tui -x 120 -y 32 \
  "node interface/dist/tui/cli.js --endpoint http://127.0.0.1:8421 --bearer-file $AGENTD_DEMO_HOME/tokens/controller.token"

# 5. the presenter
node examples/demos/standins/control.mjs bankline --count 4   # BL-0904..0907 land, one update
node examples/demos/standins/control.mjs state erp            # the feed, and the drafts: none posted
```

**The live beats:**

1. Four bank lines land: the ERP terminal shows `resources/updated`; the
   close agent's log shows `start.fired` with `"kind":"subscribe"`.
2. The recon agent's log shows the `unmatched-lines` run start and finish:
   the A2A delegation and its answer.
3. The ERP terminal shows four `journal.draft` lines, `JE-DRAFT-001` to
   `004`, and no `journal.post`.
4. The review question appears in the controller's TUI. Answer it like the
   AP gate: `#`, Tab, `reviewed`, Enter.
5. `control.mjs state erp`: four drafts, nothing posted. Posting is the
   controller's, in the ERP.

`control.mjs bankline --amount -12.00 --description "CARD TERMINAL FEE"`
adds a line of your own.

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
examples/demos/rehearse.sh            # both demos; or: rehearse.sh ap | rehearse.sh close
AGENTD=./target/release/agentd examples/demos/rehearse.sh
```

`rehearse.sh` starts the stand-ins and the agents on the demo's own loopback
ports, 8701–8703 and 8420–8422 (stop a live demo first; it checks), runs
every beat above, and asserts each one from
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
  played locally.

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

Drop the two `--intelligence` flags to rehearse on the real model instead
(it spends tokens). The same flags work for `recon-agent.yaml` and
`close-agent.yaml`, with `mock/recon-agent.json` and `mock/close-agent.json`.

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
  answers with a scripted list for the four scripted bank lines.
- `close-agent.json` drafts the four entries and then tries
  `erp.journal.post`, so the rehearsal can show the refusal.

The real model reads the invoice and the vendor, matches the bank lines
against the cash book itself, and drafts what it finds.

## Reset between takes

The stand-ins hold everything in memory: restart them and the invoices and
bank lines start again from the fixtures. agentd keeps its state under
`$AGENTD_DEMO_HOME/<agent>`; a stand-in restarted under a running agentd is
fine (its event cursors carry the stand-in's start, so agentd logs
`mcp.events.gap` and carries on). For a clean take, stop everything and:

```sh
rm -rf "$AGENTD_DEMO_HOME"/ap-agent "$AGENTD_DEMO_HOME"/close-agent "$AGENTD_DEMO_HOME"/recon-agent
```

The tokens under `$AGENTD_DEMO_HOME/tokens` stay; delete that folder too to
make new ones.

## Files

| Path | What it is |
|---|---|
| `ap-agent.yaml` | demo (a): the policy followed over MCP from the mirror, the payables server and its `invoice.created` binding, the `invoice-intake` workflow, the controller and CFO principals, the A2A listener the TUI attaches to |
| `ap-agent.local.yaml` | the rehearsal layer: the `instruction-md` server pointed at the registry stand-in |
| `close-agent.yaml` | demo (b): the ERP server (no `journal.post`), the `subscribe` start, the recon peer, the drafting step, the review gate |
| `recon-agent.yaml` | demo (b)'s second agentd: the `recon.unmatched` command over A2A, read-only ERP tools |
| `instruction/ap-policy-v1.md`, `-v2.md` | the approval policy, v1 and v2 (only the limit differs) |
| `env.sh` | the demo's local tokens |
| `rehearse.sh` | both demos end to end on the mock model, every beat asserted |
| `mock/*.json` | the mock model's playbooks |
| `standins/payables.mjs` | a stand-in for an ERP's accounts-payable module (the `payables` server): `invoices.list/get/approve/hold`, `vendors.get`, and `invoice.created` as MCP Events |
| `standins/erp.mjs` | the ERP: `erp://bank-feed/today` (subscribable), `bank_feed.lines`, `ledger.cash_entries`, `accounts.list`, `journal.draft/list/post` |
| `standins/registry.mjs` | the rehearsal's instruction registry: one local file as a subscribable resource, with `md.instruction.activity` events |
| `standins/control.mjs` | the presenter's buttons |
| `standins/lib.mjs` | what the stand-ins share: serving the SDK's handler on loopback, and the MCP Events server side |
| `standins/fixtures/*.json` | the fictional data |

The stand-ins are built on the official MCP TypeScript SDK,
`@modelcontextprotocol/server` 2.4.0 (its v2 line, the one that speaks MCP
2026-07-28, which MCP Events needs; the older `@modelcontextprotocol/sdk`
1.x stops at 2025-11-25). The SDK does not implement the MCP Events
extension (SEP-3415, a draft), so `lib.mjs` serves `events/list` and
`events/poll` as custom requests, in the shapes agentd's client reads.
