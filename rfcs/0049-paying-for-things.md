# RFC 0049: Paying for things — x402 as a paying client, only within a spend policy

**Status:** Accepted 2026-10-22, as track P of RFC 0048 (§5.6); unit 1 (HTTP, EVM) implemented, unit 2 (MCP, A2A, Solana) to come
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-10-22
**Builds on:** ADR 0003 (no payment without a spend policy — binding here), RFC 0044 P2a (the external signer, the audit chain, Landlock over supervisor-only paths), RFC 0047 (gates addressed to one person that no judge or operator answers, sealed records), RFC 0042 (what a served document may set), RFC 0054 (capabilities reach workflows first)
**Standards:** x402 v2 (x402 Foundation), pinned at [`e7a3b4ff47`](https://github.com/x402-foundation/x402/tree/e7a3b4ff47dd67c3a3a7521307ed071d34ce9527/specs) (2026-10-22, the latest commit to touch `specs/`): the core specification `x402-specification-v2.md` [X], the HTTP transport `transports-v2/http.md` [X-HTTP], the MCP transport `transports-v2/mcp.md` [X-MCP], the A2A transport `transports-v2/a2a.md` [X-A2A], the `exact` scheme `schemes/exact/scheme_exact.md` [EXACT] and its EVM and SVM bindings `scheme_exact_evm.md` [EXACT-EVM] and `scheme_exact_svm.md` [EXACT-SVM]. RFC 0048 surveyed `1566e13f3d` (2026-10-06); the two commits since change the core specification's wording only (a §9 link, an Express API name) and the SVM `batch-settlement` scheme, which agentd does not use. [EIP-712](https://eips.ethereum.org/EIPS/eip-712) (typed structured data hashing and signing), [EIP-3009](https://eips.ethereum.org/EIPS/eip-3009) (`transferWithAuthorization`), [EIP-55](https://eips.ethereum.org/EIPS/eip-55) (checksummed addresses), [CAIP-2](https://github.com/ChainAgnostic/CAIPs/blob/main/CAIPs/caip-2.md) (chain ids) and [CAIP-19](https://github.com/ChainAgnostic/CAIPs/blob/main/CAIPs/caip-19.md) (asset ids), RFC 6979 (deterministic ECDSA), SEC 1 (secp256k1).

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

x402 lets a server answer a request with "402 Payment Required" and a list
of ways to pay; the client signs a payment, sends the request again with it,
and the server settles it on a chain and serves the resource [X §2]. An
agent with a wallet can buy an API call, a document or a tool result with no
account and no person present.

That is also the problem ADR 0003 names. x402 covers replay protection
only, and leaves "client-side budget management" out of scope [X, Document
Scope]: an agent that can pay can be talked into paying, by a prompt, a tool
result or a server's own text. So agentd pays only behind the feature `x402`,
off by default and not in the release binaries, and only under a spend
policy the operator declares — caps, an allow-list, a person's approval
above a threshold, a key only the supervisor holds, an audit record of each
payment, and never a second payment of a request whose first may have
landed.

The user's decision, RFC 0048 §6 Q3 (2026-10-19): *"both"* — x402 on EVM
(with the RustCrypto crates `k256` and `sha3`, which they approved for EVM
signatures) and on Solana, both under ADR 0003. Their rule for every new
capability (RFC 0054): it reaches workflows first. This RFC designs all
three transports and both chain families, and splits the build in two:

| Unit | Transport | Chain family | Status |
|---|---|---|---|
| 1 | HTTP — the `http` workflow step | EVM: `exact` with EIP-3009 | implemented |
| 2 | MCP — the `mcp.tool` step; A2A — `a2a.send` and `a2a.delegate` | Solana: `exact` on SVM | designed here (§7) |

The spend policy, the ledger, custody, the approval gate and the audit
records are built once, in unit 1, for both.

## 2. What x402 asks of a paying client, and agentd

| Requirement | agentd (unit 1) |
|---|---|
| A 402 with `PAYMENT-REQUIRED`, a base64 `PaymentRequired`: `x402Version: 2`, `resource`, `accepts[]` [X-HTTP] | Read from the header only (the body is the server's own [X-HTTP, Response Body]); standard or URL-safe base64, padded or not. `x402Version` other than 2, no `accepts`, or a `resource` with no `url` is no payment request: the 402 is the step's error. |
| Pick one entry of `accepts` [X §5.1] | The first, in the server's order, that the step and the spend policy allow (§4); each one passed over is named with its reason. |
| `exact` on EVM: the `eip3009` transfer method, the default when `extra.assetTransferMethod` is absent; `extra.name` and `extra.version` the token's EIP-712 domain [EXACT-EVM §1] | Only `eip3009`. `permit2` and `erc7710` entries are passed over (`transfer_method`); one without `extra.name` or `extra.version` too. |
| Never construct a payment for a `paymentFlow` the client does not recognise; prefer `authorization` [X §6.1, EXACT] | Only `authorization` (the default when absent): the payment settles after the resource is served, so a failed handler costs nothing. `upfront` and `escrow` are passed over (`payment_flow`). |
| The EIP-3009 authorization: `from`, `to` = `payTo`, `value` = `amount`, `validAfter`, `validBefore`, a 32-byte random `nonce`; the 65-byte signature of its EIP-712 typed data [X §5.2, EXACT-EVM §1] | Domain `{name, version, chainId, verifyingContract}` from `extra`, the network's chain id and `asset`. `validAfter` 30 s before the signature (clock skew between agentd and the chain), `validBefore` `maxTimeoutSeconds` after it, at most `security.payments.max_validity` (default 5 min). The nonce from the kernel's CSPRNG. secp256k1, RFC 6979, low S, `v` 27 or 28. |
| `PaymentPayload`: `x402Version`, `resource`, `accepted` (the entry chosen), `payload`, and the server's `extensions` echoed with at least the info received [X §5.1, §5.2] | As written. `accepted` is the entry exactly as the server sent it; `extensions` the server's, unchanged. |
| Retry with `PAYMENT-SIGNATURE`, the base64 `PaymentPayload` [X-HTTP] | Once, to the same URL, with the same method, headers and body. No redirect is followed — agentd's client follows none — and a 402 is never followed to another resource: `resource.url` must name the URL agentd asked (D3). |
| Read `PAYMENT-RESPONSE`: `success`, `transaction`, `network`, `payer`, `errorReason` [X §5.3, X-HTTP] | Into the audit record and the step's output. `settlement_pending` — broadcast, not confirmed [X §9] — is an unknown outcome, kept with its transaction hash. |
| Replay protection is the nonce, the time window and the signature [X §10.1] | That is all x402 gives; the rest is §4. |

## 3. Design decisions

**D1. A feature off by default, and two crates.** Feature `x402`, outside
the release set as `companies` and `xaa` are, implying `oauth` (whose `ring`
seals the wallet keys and whose signer client is extended for EVM). EVM
signing needs secp256k1 and Keccak-256, which `ring` does not have: `k256`
(features `ecdsa` only: RFC 6979 signing, recovery, no `std`, no `getrandom`
— agentd draws key bytes from its own CSPRNG) and `sha3` (default features
off) are optional dependencies `x402` alone enables, as the user approved.
Their RustCrypto dependencies come with them; none is in the release graph.

**D2. Where agentd pays: a workflow's `http` step, and nothing else.** An
`http` step gains `payment: {wallet, max, approve?}`: `wallet` a configured
wallet, `max` one typed amount or a list of them (`"0.05 usdc-base"`, a
decimal in the asset's whole units and a configured asset's name), `approve:
true` to ask the approver whatever the amount. It permits paying for that
step's request, in the assets `max` names, at most those amounts — and only
within the spend policy, which a step cannot widen. Without `payment`, a 402
is the step's error, the `PaymentRequired` decoded into its output as
`payment_required`. The field is literal: a template in it is refused at
load, so run data — a webhook's payload, a model's answer — never raises
what a step may spend. A paying step must be in a workflow the operator's
own configuration defines: one an instruction document defines is refused at
load (RFC 0042: a served document configures no payment), and one
`workflow.create` or `workflow.update` wrote, or the store restored, fails
the step without paying.

*A model-facing tool that pays* — decided against, in both units. A model's
tool arguments are chosen by the model from text that may be anyone's, and
ADR 0003 is about exactly that agent. A model reaches a payment only through
a paying workflow the operator wrote and exposed as a tool (`tool:` on the
workflow), granted like any tool: the operator chose the request, the
bounds and the wallet, and the model only starts it. Such a run is work a
model asked for, so it is treated as tainted (D7).

**D3. The exchange, and the request it binds to.** The first request goes
out as the step defines it. On a 402 with a `PAYMENT-REQUIRED` and a
`payment` field, agentd decodes it, checks that `resource.url` names the
request — same scheme, host, port and path, and, when it carries a query,
the same query — chooses an entry (§4), and, past the approval gate (D6),
signs and sends the request once more with `PAYMENT-SIGNATURE`. The step's
output is the second response with `payment` beside it: `{state, wallet,
network, asset, amount, payee, pay_to, nonce, transaction, payer,
error_reason}`. A `resource.url` naming another resource is refused unsent
(`resource_mismatch`): a server that answers for `/a` with a payment for
`/b` is not paid.

What follows the 402 is the transport's only at its two ends. The `http`
step hands the loop the decoded `PaymentRequired` and its request, as a
`runtime::payments::PaidCall` — which says whether a `resource.url` is the
call's, how a record names the call, and how to make it once more carrying
a `PaymentPayload` and read the settlement back — through one entry point,
`Runtime::pay_for`. Everything between, from the policy to the ledger, the
gate and the audit chain, asks no question of the transport, so unit 2's
MCP and A2A calls are two more `PaidCall`s (§7). The chain family is the
same kind of seam: `payments::scheme::Family`, read from the network's
CAIP-2 namespace, alone reads an operator's asset and payee, an entry's
token, `payTo` and `extra`, fixes a payment's nonce and window before its
record is written, and signs it.

**D4. The paid request, and never twice.** A paid request is the run's step:
its idempotency key (`engine::run::idempotency_key(run, step)`), stable
across attempts and restarts. Each payment of it is a ledger record (D9),
numbered from 1, in one of five states:

| State | When | A later attempt of the step |
|---|---|---|
| `sending` | written, and on the disk, before the signed request leaves | — (a restart reads it as `unknown`) |
| `settled` | `PAYMENT-RESPONSE` says `success: true` | fails `payment_made`, sending nothing |
| `unknown` | no answer, a timeout, a 5xx, `settlement_pending`, a success with no `PAYMENT-RESPONSE`, one that is not JSON, or a restart that finds `sending` | fails `payment_unknown`, sending nothing |
| `refused` | the server answered that it did not settle: a `PAYMENT-RESPONSE` with `success: false`, no transaction and no `settlement_pending`, or a 400 or a 402 with none | may pay again, a new authorization with a new nonce |
| `unsent` | nothing left agentd: the signer refused or did not answer, the dial failed, the audit chain did not take the `authorized` record | may pay again |

The checks come before any request: a step re-run by its `retry:`, by an
`on_replay` after a crash, or by a restart finds the record and fails,
saying what the record says. ADR 0003's "a payment whose outcome is unknown
is never made a second time" is the `unknown` row; `settled` follows from
it. A `refused` payment's authorization stays signed and valid until its
`validBefore`, held by a server that could still submit it, so it counts
against every cap until then (§4).

**D5. Custody: the supervisor's alone, sealed at rest.** A wallet is named
by the operator (`security.payments.wallets.<name>`). Under `key_custody:
local` (the default) its secp256k1 key is a file, `key_file`, that holds
`{wallet, address, sealed}`: the 32-byte key sealed under AES-256-GCM
(`auth::seal`, as Account Tokens and grants are) with the associated data
`("payment-wallet-key", <wallet>, <address>)`, under
`security.payments.seal_key` — 32 bytes as a `{{secret-file:PATH}}`
reference, never the environment, read by the supervisor at start. The file
is made `0600` by `agentd --print-wallet-address <wallet>` when absent
(which prints the address to fund) and refused at start when another user
can read it or it does not open to its address. It is opened **for each
signature** and the key wiped after it: a payment is rare, and the
plaintext key then lives in memory for one signature, not the daemon's
life. Under `key_custody: external` agentd holds no wallet key at all: the
signer of `docs/external-signer.md` does (D8). Either way the key — or the
socket — is a supervisor-only path (`Settings::supervisor_only_paths`):
no child can open it, the seal key file or the store root where the ledger
is, so a worker cannot read a key or rewrite the ledger. Workers never sign:
the `http` step runs in the supervisor, which alone builds, signs and sends
the payment; the signature, the authorization and the key appear in no log
line, no child's payload and no prompt.

*Why a sealed file rather than a raw key file read with `0600` checks.*
ADR 0003 says sealed at rest; a raw key in a file is protected only by its
mode bits, and copied by any backup of the disk. Sealing it under a key that
lives elsewhere (a mounted secret) costs one AES-GCM open per payment, and
reuses the sealing every other supervisor-only record already has.

**D6. The person's approval above a threshold.** An asset's `approve_above`,
a step's `approve: true`, and tainted work (D7) send the payment to
`security.payments.approver` — `operator` (the default) or one principal id
— through an addressed gate: an A2A task of its own, owned by the approver,
`input-required`, marked as agentd's own security gate, whose text is
agentd's (the amount in whole and atomic units, the asset and its CAIP-19
id, the network, the payee's name and address, the request, the wallet, the
workflow, run and step, and why it asks), and what the step or the server
chose in it — the request's method and URL, the server's description —
quoted and escaped on one line (`sec::render`), so it cannot pass for
agentd's words. It takes `approve` or `decline` and nothing
else. Only the approver answers: a principal who is not — an operator
included, when the approver is not `operator` — is refused and the gate left
open, with no override; no judge answers it, whatever `agent.approval` or
`agent.ask_human_fallback` say; unanswered within
`security.payments.approval_timeout` (default 1 h) it declines. The approval
covers the entry it showed, for that request, once: the policy is checked
again when it lands — a cap another payment reached meanwhile still refuses
— and the authorization is signed then, its window from then. A gate a
previous life left open ends the step at the next start (`interrupted`),
nothing paid; the step's `retry:` asks again. Without an A2A listener
nobody can be asked, and such a payment is refused unsent (`no_approver`);
the load says so.

**D7. Taint.** With `security.payments.ask_when_tainted` (default `true`),
a payment from work that has read outside text (`runtime::taint` — an
`http` response, a webhook's payload, an A2A peer's answer, the result of
an MCP server tagged `untrusted_input`, a Company's answer) or that a model
asked for (`Runtime::model_asked`: a run a model's tool call started, D2)
goes to the approver whatever its amount: such work may have been steered
to the request. An MCP server's results are outside text only when its
`tags` say `untrusted_input`. An operator who accepts the risk within the
caps turns `ask_when_tainted` off.

**D8. The external signer signs typed data, not a digest.** Two additions to
`docs/external-signer.md`, with no version field (the protocol is agentd's
own): `public_key` answers an EVM key with `alg: "ES256K"` and its public
JWK (`kty: EC`, `crv: secp256k1`, RFC 8812), from which agentd derives the
address; and a new op, `sign_typed_data {key, typed_data}` → `{sig}`, the
65-byte `r ‖ s ‖ v` over the EIP-712 digest. agentd sends the typed data —
`{types, primaryType, domain, message}`, as `eth_signTypedData_v4` takes it
— so the signer computes the digest itself and can read what it signs. A
signer MUST sign only an EIP-3009 `TransferWithAuthorization` of exactly
its six fields, under a domain of exactly `name`, `version`, `chainId` and
`verifyingContract`, for a (chain, contract) configured for that key, with
`from` the key's own address; SHOULD hold `to` to its payees and `value` to
a maximum; MUST refuse a `validBefore` past its own window and a nonce that
is not 32 bytes; and answers anything else `denied`. agentd checks that a
returned signature recovers to the wallet's address before it sends it. The
reference signer (`--internal-mock-signer`) holds to every rule.

**D9. The ledger.** One record per payment in the store (`cred` kind,
`payment/ledger/<request>.<attempt>`, the request the step's idempotency
key — a hash of the run and the step — and the attempt from 1; plain: it
holds no secret), read into memory at start, a `sending` one turned
`unknown` then. A refused payment's record stays beside the next attempt's,
so its amount counts against the caps until its `validBefore`. Each holds
the request key and attempt, the run, step and workflow, the wallet, network, asset,
payee, `payTo`, amount, nonce, `validBefore`, the URL (no query), the state,
the transaction, payer and `errorReason` when known, who approved, and when.
A write the store did not keep — an error, or a degraded store that only
said so — refuses the payment unsent. `store.kind: memory` is refused at
load beside `security.payments`: a ledger that a restart forgets cannot
keep "never twice". A record is kept while its run is live and for 25 hours
at least; older ones are swept.

**D10. Every payment an audit record, before it is made.** `security.payments`
needs `observability.audit.chain` (refused at load without it). Each step
of a payment is a `payment` record in the chain, flushed to the disk before
the next step goes on, as delegation's are: `refused` (unsent, with every
entry's reason), `asked`, `not_addressed` (an answer from someone who is
not the approver), `approved` and `declined` (naming who), `withdrawn` (a
gate whose step went another way), `interrupted` (a gate a restart ended,
nothing paid), `authorized` (written before the signed request leaves: the
record of a payment that may land), then `settled`, `failed`, `unknown` or
`unsent` — `unknown` too at the next start, for a payment still `sending`
when agentd stopped, so every `authorized` record is closed. Each names the
paid request — its run, step, workflow and the trace of the step's span —
and the wallet, network, asset, payee, amount and nonce; `settled` the
transaction and payer. A chain that cannot take the `authorized` record
refuses the payment unsent.

## 4. The spend policy

```yaml
# fragment: under security:
payments:
  approver: operator            # or one principal id
  approval_timeout: 1h
  ask_when_tainted: true
  max_validity: 5m
  seal_key: "{{secret-file:/run/secrets/agentd-payments-seal}}"
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
```

- **Amounts** are decimals in an asset's whole units (`"0.25"`), read
  exactly with its declared `decimals` into atomic units — no float, and a
  digit past `decimals` refused — and every comparison is in atomic units.
  An asset's caps are its own: amounts of two assets are never added.
- **Assets** are allow-listed by CAIP-19 (`eip155:<chain>/erc20:<contract>`),
  which also names the network, by CAIP-2: an entry on a network or for an
  asset no asset names is passed over (`network`, `asset`). Unit 1 takes
  `eip155` chains and `erc20` assets; `solana` and `token` (SPL) come with
  unit 2.
- **Payees** are allow-listed by address, each optionally to some assets
  (`assets: [usdc-base]`); `payTo` is compared as 20 bytes, whatever its
  case. An entry for another payee is passed over (`payee`).
- **Caps.** `per_payment` bounds one payment; `per_run` the payments of one
  run; `per_day` all payments in any 24 hours; `per_payee` all payments to
  one payee in any 24 hours — each counted from the ledger over every
  payment whose authorization left agentd or may have (`sending`, `unknown`,
  `settled`), and a `refused` one until its `validBefore`. The check and the
  reservation are one step on the supervisor's loop, so two steps cannot
  both pass a cap the second one breaks. An entry past a cap is passed over
  (`per_payment`, `per_run`, `per_day`, `per_payee`); so is one past the
  step's `max` (`max`).
- **Refused unsent.** When no entry passes, the step fails `payment refused`,
  naming each entry and its reasons, nothing is signed, and the audit record
  says so.

Configuration is operator-only (`security` is, RFC 0042) and restart-only:
the caps, the allow-lists and the keys are what a payment is judged by, and
a reload that changed them under a gate the approver is reading would
approve something else than what they read.

## 5. What is sent, what is logged

- **Sent:** to the resource server, the step's request, then the same with
  `PAYMENT-SIGNATURE`. To the external signer, the typed data. Nothing else
  leaves for a payment — agentd talks to no facilitator and no chain: the
  server settles [X §7].
- **Logged:** `payment.required` (the 402 read: the entries, never a
  signature), `payment.refused`, `payment.asked`, `payment.approved`,
  `payment.declined`, `payment.interrupted`, `payment.sent`,
  `payment.settled`, `payment.failed`, `payment.unknown`,
  `payment.ledger.restored`, `payment.ledger.swept`, `payment.wallet`,
  `payment.signer.unavailable`. None carries the signature,
  the authorization's signature or a key; the nonce, which the chain makes
  public once it settles, is in the audit record.
- **Output:** the step's `payment` object (D3).

## 6. Threats

| Threat | What holds |
|---|---|
| A prompt, a tool result or a server's text talks the agent into paying | No model-facing paying tool (D2); a paying step only in the operator's own workflow, its bounds literal; the allow-lists and caps (§4); tainted work asks (D7). |
| A server asks for more than it should, or for another resource | `max`, `per_payment`, the caps; `resource.url` bound to the request (D3). |
| A server names a payee or a token of its own | Payees and assets allow-listed; a domain name or version that is not the token's makes a signature the token refuses. |
| A payment that may have landed is made again | The ledger's states (D4), on the disk before the request leaves. |
| A worker or a plugin reads the key, or signs | The key file, the seal key and the store root are supervisor-only paths under Landlock (D5); only the supervisor signs; the external signer's own policy (D8). |
| A served document configures payments | `security` is operator-only; a document's paying step is refused at load (D2). |
| The audit trail is rewritten | The hash-chained audit log, its records flushed before the payment goes on (D10). |
| A server holds a signed authorization it said failed | It counts against the caps until its `validBefore`, at most `max_validity` away. |

Out of reach: a server that takes the payment and serves nothing (x402's
`authorization` flow settles after the handler; agentd keeps the record a
dispute needs), and a chain or token contract that misbehaves.

## 7. Unit 2: MCP, A2A and Solana

Designed here; built next, on unit 1's policy, ledger, custody, gate and
audit, unchanged. Where unit 2 plugs in:

- **A transport** is a `PaidCall` its step hands `Runtime::pay_for(run,
  step, required, call, first)` — `required` the `PaymentRequired` as the
  transport carried it, or why what it carried is none; `first` the answer
  that asked, the step's output when nothing is paid. Before the step's
  call is made, `Runtime::payment_precheck` and, when it refuses,
  `Runtime::payment_refused_before`. A step kind that takes `payment` is
  listed in `config::settings::payments::PAYING_KINDS` and its field list.
- **A chain family** is a variant of `payments::scheme::Family` and of its
  `Token`, `Account`, `Terms` and prepared payment, with its arm in each of
  their functions; the policy, the ledger, the gate and the audit chain
  read none of them.

- **MCP** [X-MCP]: a `tools/call` result with `isError: true` whose
  `structuredContent` (else `content[0].text` as JSON) is a
  `PaymentRequired`; the call is sent again with the `PaymentPayload` in
  `params._meta["x402/payment"]`, and the settlement read from
  `result._meta["x402/payment-response"]`. Where agentd pays: an `mcp.tool`
  step's `payment` field, of the same shape. `resource.url` (`mcp://tool/<name>`
  in the spec's examples) must name the tool called. A turn's call of the
  same tool is never paid (D2): its `PaymentRequired` is the tool's error.
- **A2A** [X-A2A] — the a2a-x402 extension: a task in `input-required`
  whose status message carries `x402.payment.status: payment-required` and
  `x402.payment.required`; agentd answers on the task with
  `x402.payment.status: payment-submitted` and `x402.payment.payload`, and
  reads `x402.payment.receipts`. Where agentd pays: the `a2a.send` and
  `a2a.delegate` steps' `payment` field. The extension's activation header
  in its specification is `X-A2A-Extensions`, which predates A2A 1.0.1's
  `A2A-Extensions`; agentd sends what A2A 1.0.1 defines, and the mismatch is
  drafted as a note for upstream, locally, for the user to file (RFC 0048
  §5.6).
- **Solana** [EXACT-SVM]: assets `solana:<genesis>/token:<mint>`, payees by
  base58 address. The client builds the versioned transaction the binding
  describes — compute-budget instructions, an SPL `TransferChecked` of
  `amount` from the wallet's associated token account to `payTo`'s, a Memo
  (`extra.memo`, else a random nonce) — with `extra.feePayer` as fee payer,
  signs it with Ed25519 (`ring`, no new crate) and sends it partially
  signed. A recent blockhash comes from `extra.recentBlockhash`, else from
  `getLatestBlockhash` at an RPC endpoint the operator names for the
  network. The external signer gains `sign_solana_transaction`, holding the
  message to that shape. Two things unit 2 settles: deriving an associated
  token account needs an off-curve check of an Ed25519 point, which `ring`
  does not expose (hand-rolled field arithmetic, as `config::decrypt` does
  for X25519, or a decision for the user); and a Solana authorization has no
  `validBefore` — its blockhash expires in about 60 to 90 seconds — so the
  ledger's "until it can no longer land" is the blockhash's last valid block
  height, read from the RPC.
- **Not in either unit:** the `upto`, `batch-settlement` and
  `auth-capture` schemes, Permit2 and ERC-7710, the other networks, the
  facilitator and discovery APIs, and the extensions x402 lists (each echoed
  as received, none acted on).

## 8. How each ADR 0003 rule is met

| ADR 0003 | Met by |
|---|---|
| Caps per run, per day and per payee, and an allow-list of payees and assets; a payment outside them refused unsent | §4: `per_run`, `per_day`, `per_payee` (and `per_payment`, and the step's `max`), assets by CAIP-19, payees by address; refused before anything is signed. |
| The User's approval above a threshold, through an addressed human gate no model or judge answers | D6: `approve_above`, to `security.payments.approver` alone — no judge, no operator override unless the approver is `operator`, declined unanswered. Also tainted work (D7) and `approve: true`. |
| The signing key held by the supervisor alone, sealed at rest, never in a model's context, a child's payload or a log | D5: sealed under AES-256-GCM, opened per signature in the supervisor, a supervisor-only path; or held by the external signer (D8). Asserted end to end: no key, signature or authorization in any frame to a child, and none in the log. |
| Every payment an audit record, tied to the request that asked | D10: `payment` records in the hash-chained audit log, the `authorized` one flushed before the request leaves, each naming the run, step, workflow and trace. |
| A payment whose outcome is unknown is never made a second time | D4 and D9: the `unknown` state is final, on the disk before the request leaves, and a restart turns `sending` into it. |
| A feature off by default | D1: `x402`, outside the release set. |

## 9. Exit criteria (unit 1)

- Unit tests: EIP-712 hashing against the specification's own `Mail`
  example (type hash, struct hash, domain separator, digest) and its
  signature with the key the example derives; EIP-3009
  `TransferWithAuthorization` hashing, word by word as the token contract
  does, and [X-HTTP]'s own `PAYMENT-SIGNATURE` example, a real signature
  that recovers to its payer; secp256k1 signing that is RFC 6979
  deterministic and low-S, recovering (ecrecover) to the wallet's address;
  EIP-55 checksums against the specification's addresses; CAIP-2 and
  CAIP-19 parsing; amounts parsed exactly; the policy's every reason; the
  ledger's states across a reopen; the sealed key file's mode and address
  checks; the reference signer refusing every typed data but the allowed
  shape.
- A mock x402 resource server (`agentd --internal-mock-x402`, in debug and
  `internal-mocks` builds) that answers 402 with `PAYMENT-REQUIRED`,
  verifies `PAYMENT-SIGNATURE` by recovering the signer and holding the
  authorization to what it asked, and answers `PAYMENT-RESPONSE` — steered
  to settle, refuse, stall, answer `settlement_pending`, or name another
  resource.
- End to end (`crates/agentd-cli/tests/x402_e2e.rs`): a workflow pays and
  gets the resource; refused unsent past a cap, for an unlisted payee,
  asset or network; asked above `approve_above`, paying only on the
  approver's `approve` and not on an operator's; concurrent steps never
  passing a cap together; never twice after an unknown outcome; a restart
  mid-payment; no key, signature or
  authorization in a child's payload or the log; the external signer end to
  end, the reference signer refusing a typed data of another shape.
- A conformance family, `payments`, each check quoting the x402 or ADR 0003
  sentence it proves.
- Docs: `docs/pay-with-x402.md` (the guide), configuration, security, the
  node registry, telemetry, the external signer, features,
  interoperability; the strip lists x402 v2 as a preview.
