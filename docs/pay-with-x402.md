# Pay for things with x402

This guide lets a workflow pay for what it uses with x402 — an HTTP
resource, a tool on an MCP server, the work of another agent over A2A,
priced at a few cents — from a wallet agentd holds, and only within a spend
policy you declare. It is for
operators who run agentd against such resources. After it you can make a
wallet, say what agentd may spend, in which tokens and to whom, have a
workflow step pay, and have a person approve each payment above an amount
you choose.

> **Preview.** agentd pays as a client of x402 v2, pinned at the
> specification's commit `e7a3b4ff47`: over HTTP, MCP and A2A (the a2a-x402
> extension), with the `exact` scheme on EVM chains (an EIP-3009
> authorization) and on Solana (an SPL transfer). It is behind the `x402`
> build feature, which no release binary or image carries.

## What it is

A server that speaks x402 answers a request with `402 Payment Required` and
a `PAYMENT-REQUIRED` header listing the ways it takes payment. The client
signs a payment for one of them and sends the same request again with a
`PAYMENT-SIGNATURE` header. The server checks it, serves the resource,
settles the payment on chain and says how in a `PAYMENT-RESPONSE` header.
No account and no person is needed — which is why agentd pays only under a
spend policy.

| Term | Meaning |
|---|---|
| Wallet | An account agentd pays from, on EVM chains or on Solana, by a name you give it. Its key is sealed in a file only the supervisor opens, or held by an [external signer](external-signer.md). |
| Asset | A token agentd may pay in, named by its CAIP-19 id (`eip155:8453/erc20:0x8335…2913`, `solana:5eyk…KKdp/token:EPjF…Dt1v`), which also names its chain. Each has its own caps. |
| Payee | An address agentd may pay, by a name you give it. |
| Spend policy | `security.payments`: the wallets, assets and payees, the caps, and who approves. A payment outside it is refused before anything is signed. |
| Approver | The one person asked about a payment above `approve_above`. Nobody else answers, and no model or judge does. |
| Ledger | agentd's record of every payment, on the disk before the payment leaves. A payment whose outcome agentd cannot know is never made again. |

## When to use it

Use it when a resource answers `402` with an x402 v2 `PAYMENT-REQUIRED`, an
MCP tool answers with a payment request, or an A2A peer's task asks to be
paid, offering `exact` on an EVM chain or on Solana — USDC on Base or on
Solana, say — and you want a workflow you wrote to pay for it.

Use something else when:

- **A model should decide to pay.** agentd has no tool a model calls to
  pay. Write the paying workflow yourself and [expose it as a
  tool](author-workflows.md#expose-a-workflow-as-a-tool): you choose the
  request, the bounds and the wallet, and the model only starts it — and the
  approver is asked about every payment of such a run.
- **The server takes Permit2, `upto` or a facilitator's API.** agentd pays
  only `exact` — with EIP-3009 on EVM chains, with an SPL transfer on
  Solana — and passes every other entry over.
- **Nobody can approve.** A payment above `approve_above`, or one of work
  that read outside text, waits for the approver over A2A. Without an A2A
  listener it is refused.

## Build agentd with the feature

The release binaries and the image do not carry `x402`. Build a binary from
source with the release feature set plus `x402`:

```bash
cargo build --release --locked -p agentd-cli --features "a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel,sign,oci,decrypt,mcp-events,mock-models,x402"
```

`x402` brings `oauth` with it, and two crates no other feature uses: `k256`
for secp256k1 signatures and `sha3` for Keccak-256. Solana adds none: its
Ed25519 signatures are `ring`'s. Keep `a2a`: the approver answers over it. [Build features and crates](features.md) lists every
feature.

A binary without the feature refuses the configuration at load, with exit
code `2`:

```text
{"event":"config.invalid","msg":"security.payments needs the `x402` feature (build with --features x402)"}
```

## Make a wallet

A wallet's key is a file holding the key sealed under AES-256-GCM, with a
seal key you keep elsewhere — a mounted secret, 32 random bytes in hex:

```bash
install -d -m 700 /run/secrets /var/lib/agentd/wallets
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > /run/secrets/agentd-payments-seal
chmod 600 /run/secrets/agentd-payments-seal
```

With the configuration below written, ask agentd for the wallet's address.
It makes the key file, `0600`, when there is none, and prints the address to
fund:

```bash
agentd -c agentd.yaml --print-wallet-address ops
```

```text
0x4E8010b8F36D2Ea3a38D1c2A6A9336D237E33B0d
```

Send the wallet only what you mean it to spend. agentd refuses to start when
the key file is a symlink, belongs to another user, can be read by another
user, or does not open under the seal key to the address it names. It opens
the file for each signature and wipes the key after, so the plaintext key is
in memory for one signature. The seal key comes only from a file — never the
environment — and agentd's workers can open neither file.

## Declare the spend policy

A complete configuration: one wallet, USDC on Base, one payee, and a
workflow whose `http` step may pay for its request.

```yaml
# needs: ./payments-seal.key
agent: {name: buyer, preflight: never}
intelligence: {endpoints: "mock:final", model: mock}
store: {kind: file, file: {path: /var/lib/agentd/state}}
a2a:
  listen: http://127.0.0.1:8080
  bearer: "{{secret:A2A_BEARER}}"
observability:
  audit: {chain: {path: /var/lib/agentd/audit/audit.log}}
security:
  payments:
    approver: operator
    approval_timeout: 1h
    max_validity: 5m
    seal_key: "{{secret-file:./payments-seal.key}}"
    wallets:
      ops: {key_file: /var/lib/agentd/wallets/ops.key}
    assets:
      usdc-base:
        asset: "eip155:8453/erc20:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
        decimals: 6
        limits: {per_payment: "0.10", per_run: "1.00", per_day: "20.00", per_payee: "5.00"}
        approve_above: "0.25"
    payees:
      weather: {address: "0x209693Bc6afc0C5328bA36FaF03C514EF312287C"}
workflows:
  - name: forecast
    steps:
      start: {kind: manual}
      get:
        kind: http
        depends_on: [start]
        url: https://api.weather.example/forecast?city=Lisbon
        payment: {wallet: ops, max: "0.05 usdc-base"}
      done: {kind: finish, depends_on: [get], output: "{{ steps.get.output.json }}"}
```

- **Amounts** are decimals in the token's whole units (`"0.25"` is 250 000
  of a 6-decimal token), read exactly — no floating point, and a digit past
  `decimals` is refused rather than rounded.
- **Caps.** `per_payment` bounds one payment, `per_run` one run's payments,
  `per_day` every payment in any 24 hours, `per_payee` every payment to one
  payee in any 24 hours. `per_run`, `per_day` and `per_payee` are required.
  Two assets' amounts are never added.
- **What counts** against a cap is every payment that left agentd or may
  have — settled, out, or of unknown outcome — and one the server refused,
  until its authorization expires: the server still holds it signed.
- **The step's `max`** names the assets it pays in and at most how much, as
  `"<amount> <asset>"` or a list of them. It is literal: a template there is
  refused at load, so nothing a run reads raises what it may spend. A step
  cannot widen the policy.
- **The policy** is the operator's alone and changes only on a restart:
  a served instruction document cannot set it, and an instruction
  document's workflow with a paying step is refused at load.

Every payment is a record in the [audit chain](security.md#the-audit-chain),
so `observability.audit.chain` is required, and so is a store that outlives
the process (`file`, `mcp` or `http`): the ledger is kept there.

## What a payment looks like

Four kinds of step can pay, each over its own transport, and each with the
same `payment` field, policy, approver, ledger and audit records:

| Step | What asks to be paid | How the payment goes |
|---|---|---|
| `http` | a `402` with a `PAYMENT-REQUIRED` header | the request again, with `PAYMENT-SIGNATURE` |
| `mcp.tool` | a tool result with `isError: true` carrying a `PaymentRequired` ([below](#pay-for-a-tool-on-an-mcp-server)) | the call again, with the payment in `_meta["x402/payment"]` |
| `a2a.delegate`, `a2a.send` | the peer's task in `input-required`, asking to be paid ([below](#pay-another-agent-over-a2a)) | a message on the peer's task carrying the payment |

An `http` step sends its request as written. When the answer is `402` with a
`PAYMENT-REQUIRED`, agentd:

1. checks that the resource it names is the URL the step asked — scheme,
   host, port, path and, when it names one, the query. A payment for another
   resource is refused (`resource_mismatch`);
2. takes the first entry, in the server's order, that the step and the
   policy allow: `exact`, the `eip3009` transfer method, the
   `authorization` payment flow, an allowed asset on its network, an allowed
   payee, within `max` and every cap;
3. asks the approver when it must ([Approve payments](#approve-payments));
4. writes the payment to the ledger and the audit chain, signs an EIP-3009
   `TransferWithAuthorization` — valid from 30 s before now until the
   server's `maxTimeoutSeconds` or `max_validity`, whichever is sooner, with
   a random 32-byte nonce — and sends the request once more, with the same
   method, headers and body and the `PAYMENT-SIGNATURE` header;
5. reads `PAYMENT-RESPONSE` into the step's output and the audit chain.

The step's output is the second answer with a `payment` member:

```json
{"status": 200, "json": {"forecast": "sun"},
 "payment": {"state": "settled", "wallet": "ops", "network": "eip155:8453",
             "asset": "usdc-base", "amount": "40000", "payee": "weather",
             "pay_to": "0x209693Bc6afc0C5328bA36FaF03C514EF312287C",
             "nonce": "0x51d7…e0b3", "transaction": "0x4882…333a",
             "payer": "0x4E8010b8F36D2Ea3a38D1c2A6A9336D237E33B0d",
             "error_reason": null}}
```

When no entry passes, nothing is signed and the step fails naming each entry
and every reason:

```text
payment refused unsent: no entry the server offers is one this step and security.payments allow — entry 0: per_day: 0.03 usdc-base with 19.98 usdc-base in the last 24 hours is more than 20 usdc-base
```

A step without `payment` that is answered `402` fails, with the server's
`PAYMENT-REQUIRED` decoded into its output as `payment_required`.

## Never twice

A paid request is the step itself, and a payment agentd cannot know the
outcome of is never made again:

| The paid request | Its record | A retry, a replay or a restart of the step |
|---|---|---|
| is out | `sending`, on the disk before it left | — a restart reads it as `unknown` |
| settled | `settled` | fails `payment_made`, sending nothing |
| got no answer, a timeout, a 5xx, `settlement_pending`, a success with no `PAYMENT-RESPONSE` | `unknown` | fails `payment_unknown`, sending nothing |
| was refused: `success: false` with no transaction, or a 400 or 402 with none | `refused` | may pay again, with a new nonce |
| never left: the signer refused, the dial failed | `unsent` | may pay again |

`settlement_pending` means the transaction was broadcast and may still
confirm, so agentd treats it as unknown. Find the payment by its nonce and
transaction in the audit chain, and settle the question on chain yourself.

## Approve payments

The approver is asked, over A2A, about a payment:

- above the asset's `approve_above`;
- of a step that says `approve: true`;
- of work that has read outside text — an `http` response, a webhook's
  payload, an A2A peer's answer, the result of an MCP server tagged
  `untrusted_input` or not tagged at all — or that a model asked for, with
  `ask_when_tainted: true` (the default). Such work may have been steered to
  the request. An MCP server's results count unless its `tags` leave
  `untrusted_input` out, so tag a server whose results nobody outside
  writes with the legs it carries, or `trusted` when it carries none.

The question is a task of its own, owned by the approver, in
`input-required`, marked as agentd's own security gate. Its text is
agentd's: the amount in whole and atomic units, the token's CAIP-19 id, the
network, the payee's name and address, the request, the wallet, the
workflow, run and step, and why it asks. What the step or the server chose
— the request's method and URL, the server's description — is quoted and
escaped on one line, so it cannot pass for agentd's words. It takes
`approve` or `decline`.

- **Only the approver answers.** `approver: operator` (the default) is any
  operator principal; `approver: "user:alice"` is that principal alone. Any
  other caller — an operator included — is refused, and the gate stays open.
  No judge and no `agent.approval` setting answers it.
- **Unanswered** within `approval_timeout` (default 1h), it declines. A
  peer that asked over A2A is told `payment-rejected`.
- **An approval covers the entry it showed, once.** The policy is checked
  again when it lands — a cap another payment reached meanwhile still
  refuses — and the payment is signed then.
- **A restart** while a gate is open ends its step, nothing paid. The step's
  `retry:` asks again.

The approver answers it as any gate, from the terminal UI, the web UI or
an A2A message on the task: see [Payment
approvals](human-in-the-loop.md#payment-approvals) for the gate from the
approver's side, and [Answer from a
client](human-in-the-loop.md#answer-from-a-client).

## Keep the key with a signer

With `key_custody: external`, agentd holds no wallet key. A wallet names a
key of the [external signer](external-signer.md), which answers its address
and signs EIP-712 typed data — the whole authorization, not a digest, so it
can read what it signs and refuse anything but the shape and the bounds you
give it:

```yaml
# fragment: under security:
payments:
  key_custody: external
  signer: {socket: /run/agentd-signer/signer.sock, uid: 991}
  wallets:
    ops: {key: wallet-ops-2026}
```

agentd checks that every signature recovers to the wallet's address before
it sends it.

## Pay on Solana

On Solana, x402's `exact` scheme moves an SPL token — USDC, say — from your
wallet's token account to the server's, in a transaction your wallet signs
and the server's sponsor completes. The sponsor is the fee payer the
server's entry names: it pays the network fee and signs last, so your
wallet holds the token and needs no SOL. agentd signs only the transfer the
entry asks for, and only when the spend policy allows it.

```yaml
# fragment: under security:
payments:
  approver: operator
  seal_key: "{{secret-file:/run/secrets/agentd-payments-seal}}"
  wallets:
    sol: {chain: solana, key_file: /var/lib/agentd/wallets/sol.key}
  assets:
    usdc-solana:
      asset: "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp/token:EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
      decimals: 6
      token_program: spl-token
      limits: {per_payment: "0.10", per_run: "1.00", per_day: "20.00", per_payee: "5.00"}
      approve_above: "0.25"
  payees:
    weather: {address: "2wKupLR9q6wXYppw8Gr2NvWxKBUqm4PPJKkQfoxHDBg4"}
  rpc:
    "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp": "https://api.mainnet-beta.solana.com"
```

```yaml
# fragment: a step of a workflow in your own configuration
fetch:
  kind: http
  url: https://api.example.com/weather
  payment: {wallet: sol, max: "0.05 usdc-solana"}
```

- **`chain: solana`** makes the wallet an Ed25519 key. A step pays from it
  only in assets on Solana; a step that names an EVM asset with it is
  refused at load.
- **`asset`** is the token's CAIP-19 id: the network (`solana:` and the
  first 32 characters of the genesis hash; the line above is mainnet) and
  the mint. **`decimals`** must be the mint's: the transfer states them, and
  the token program refuses a transfer whose decimals are wrong.
- **`token_program`** is the program the mint belongs to: `spl-token` (the
  default; USDC and USDT) or `token-2022`. It decides both token accounts'
  addresses and the program that moves the token.
- **`payees`** take a Solana address in base58. A payee is paid at its
  associated token account for the mint, the account the server checks.
- **`rpc`** names, by network, the endpoint agentd asks for a recent
  blockhash (`getLatestBlockhash`, at `finalized`) when the server's entry
  gives none. It may hold a `{{secret:NAME}}` reference, resolved when it is
  dialled, and is logged without its query. Without an endpoint for the
  network, an entry with no blockhash is passed over.

`agentd -c agentd.yaml --print-wallet-address sol` makes the key file and
prints the base58 address. Send the token to it from any wallet: that
creates the wallet's associated token account, the transfer's source.

The transaction is a version-0 message with the server's `extra.feePayer`
as fee payer and your wallet as the second signer, and exactly these
instructions, in this order:

| Instruction | What it does |
|---|---|
| SetComputeUnitLimit 20 000 | the compute budget the sponsor pays for |
| SetComputeUnitPrice 1 microlamport | the priority fee the sponsor pays |
| `TransferChecked` | `amount` of the mint at `decimals`, from your wallet's associated token account to `payTo`'s, your wallet the authority |
| Memo | the server's `extra.memo`, or 16 random bytes in hex |

The fee payer appears in no instruction, so its signature can move nothing
of its own, and agentd never pays a fee. agentd reads the message it built
back before it signs — anything but this shape is refused, unsigned — signs
it, leaves the fee payer's signature empty, and sends the transaction in
base64 as the payment's `transaction`. Besides the reasons any entry is
passed over, a Solana entry is passed over when `extra.feePayer` is missing
or not an address, `extra.memo` is not a string of at most 256 bytes, it
has no blockhash and no `rpc` endpoint is named for its network, or the
amount is past 64 bits; and refused unsigned when `extra.feePayer` is your
wallet itself.

A Solana transfer has no expiry time of its own — its blockhash expires
about 60 to 90 seconds after it — so a payment the server says it did not
settle counts against your caps for five minutes, the time agentd allows
for that blockhash to expire. The record's nonce is the transfer's memo.

Under `key_custody: external` the signer holds the wallet's Ed25519 key
(`sol: {chain: solana, key: agentd-sol-2026}`): agentd asks it for the
key's public JWK (`EdDSA`) to learn the address, sends it the transaction's
message to sign (`sign_solana_transaction`), and verifies the signature
under the wallet's address before it sends anything.

## Pay for a tool on an MCP server

Give the `mcp.tool` step a `payment` field:

```yaml
# fragment: under workflows[].steps
report:
  kind: mcp.tool
  depends_on: [start]
  server: research
  tool: market_report
  args: {ticker: "{{ inputs.ticker }}"}
  payment: {wallet: ops, max: "0.05 usdc-base"}
```

The first call goes out as written. When the tool answers that it must be
paid, agentd reads the payment request from the result's
`structuredContent`, or from its first text part when the server sends no
structured content. The request must be for this tool —
`resource.url` `mcp://tool/market_report` — or it is refused. Once the
policy allows the payment, the call is made again: the same tool, the same
arguments and the same `_meta`, with the payment beside them. The server's
settlement comes back in the result's `_meta["x402/payment-response"]`.

The step's output is `{result, payment}`: `result` is the tool's result, as
an `mcp.tool` step's output always is, and `payment` is what was paid, or
`null` when the tool asked for nothing:

```yaml
# fragment: under workflows[].steps
summary:
  kind: assign
  depends_on: [report]
  value: "{{ steps.report.output.result.summary }} (paid {{ steps.report.output.payment.amount }}, {{ steps.report.output.payment.transaction }})"
```

A paying call is never made an MCP task: it goes out without the Tasks
extension, and `detach` and `input` are refused beside `payment`. A call to
a delegated server is never paid. A paid call that gets no answer, or an
answer with no settlement, is an unknown outcome, and is never made again;
an HTTP 4xx, or the payment request again, is a refusal.

A model's own call of a tool that charges is never paid; the model reads
the payment request as the tool's error. To let a model use a paid tool,
write a workflow with a paying `mcp.tool` step and expose it as a tool. Its
payments are asked of the approver, because a model asked for them.

## Pay another agent over A2A

Give the `a2a.delegate` or `a2a.send` step a `payment` field:

```yaml
# fragment: under workflows[].steps
images:
  kind: a2a.delegate
  depends_on: [start]
  peer: illustrator
  objective: "A cover image for {{ inputs.title }}"
  timeout: 5m
  payment: {wallet: ops, max: "0.50 usdc-base"}
```

A step that may pay activates the a2a-x402 extension on every request it
makes, so a peer whose card requires the extension accepts it. When the
peer's task stops in `input-required` and asks to be paid, the step hands
the request to the supervisor. Once the policy allows the payment, agentd
answers on the peer's own task with a message carrying the payment, over
the same connection. A delegation then follows the task to its end; a send
follows it until the peer's receipt for the payment arrives. The receipt is
the last of the task's `x402.payment.receipts`.

The step's output is `{result, payment}`: `result` is the delegation's
result or the peer's reply, and `payment` what was paid, or `null` when the
peer asked for nothing. `timeout` bounds how long the step follows the task
after it pays; a receipt that has not come by then is an unknown outcome,
and the payment is never made again. `payment-failed`, or the peer asking
to be paid again, is a refusal.

Once it pays, agentd asks the peer for none of the task's history, and the
task it keeps has every `x402.payment.payload` taken out: a peer that hands
the payment message back puts no signed payment in the step's output or
the log.

When agentd will not pay — the policy refuses the payment, or the
approver declines it or does not answer in time — it answers on the peer's
task with a message whose metadata says `x402.payment.status:
payment-rejected` and nothing else, so the peer ends the task. The peer
learns no reason: nothing of the policy or the approver. A payment that
may have left — its outcome unknown — is never rejected, and nor is a
request already paid. An idle job waits for the peer to take the message
before it exits.

A delegation that may not pay ends as soon as the peer asks to be paid, and
says so, telling the peer nothing: a step with no `payment` field does not
speak a2a-x402. A peer whose card requires a2a-x402 is refused before
anything is sent to it.

agentd speaks A2A 1.0.1 and x402 v2: it activates the extension with the
`A2A-Extensions` header, under both of the extension's URIs, and does not
pay a request of x402 version 1. The approver's question names what is paid
for, with the peer's task id or the tool's name quoted — `the task "…" of
the A2A peer illustrator (https://…)`, or `a call of the tool
"market_report" on the MCP server research` — and the audit records' `url`
the peer's endpoint, or the tool's `mcp://tool/<name>`. RFC 0049 §7.2 lists
where the extension's documents differ from A2A 1.0.1.

## What it does not do

- **It pays no one else's way.** Only `exact`, with EIP-3009 on EVM chains
  and an SPL transfer on Solana: Permit2, ERC-7710, `upto`,
  `batch-settlement`, `auth-capture` and the facilitator and discovery APIs
  are not built. Entries of another scheme, method or flow are passed over.
- **On Solana, a plain wallet only.** agentd builds and reads no address
  lookup table, pays from no smart wallet (Squads, Swig) and creates no
  token account: a `payTo` whose token account does not exist makes a
  transfer the sponsor refuses, and nothing is paid.
- **It does not settle.** agentd talks to no facilitator, and to a chain
  only through a Solana `rpc` endpoint, for a blockhash: the server
  settles. A server that takes the payment and serves nothing is
  beyond reach — the `authorization` flow settles after the resource is
  served, and the audit chain keeps what a dispute needs.
- **It follows no redirect,** and pays only for the URL the step asked, the
  tool it called, or the peer task that asked.
- **It rejects only what it decided not to pay.** A peer is told nothing
  when a payment could not be recorded, signed or sent, or no gate could be
  opened to ask about it — the step's `retry:` may still make it — nor for
  a gate a restart ended: the connection that asked is gone with the
  process. Its task stays in `input-required` until the peer ends it.
- **A paying step in a loop pays once per element:** a request is the step
  in its run, so each element of a `foreach` is its own request.

## Next steps

- [Security model: payments](security.md#payments): what holds around a
  payment, and what does not.
- [Configuration reference](configuration.md#payments): every
  `security.payments` key.
- [The node registry](node-registry.md#http): the other fields of the
  `http`, `mcp.tool`, `a2a.send` and `a2a.delegate` steps.
- [Telemetry reference](telemetry-reference.md#payment): the `payment.*`
  events and audit records.
