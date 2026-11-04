# RFC 0057: Spending controls — allowances

**Status:** Accepted 2026-11-03 by the user (decisions in §9); phases A–C
implemented (phase C on `instruction-md` 0.2, S28 and S29); a document's
receipts in the consumer report are not done (§8)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-11-03
**Builds on:** RFC 0049 (x402 payments: wallets, assets, payees, the
ledger, the approver's gate), RFC 0047 (Companies and their operations),
RFC 0056 (a document's own agents: the gated document grant), and
instruction.md RFC-0041 (agent spending: wallets, allowances, budgets and
approvals, independent of payment rails), which the user accepted on
2026-11-03

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

An **allowance** says what agentd may spend, on what, over what period, and
who approves: the limits a person means, stated once, above the payment
rails. agentd already enforces per-asset caps on x402 payments (RFC 0049:
`per_payment`, `per_run`, `per_day`, `per_payee`, `approve_above`). An
allowance adds the limits people set — a budget per calendar period, a
count of payments, a window, a list of payees, the three tiers — and
counts them across rails.

```yaml
security:
  payments:
    allowances:
      groceries:
        title: Groceries
        currency: GBP
        wallets: [household]
        max: "120"
        budget: "400"
        per: month
        payees: [tesco, ocado]
        approve_above: "60"
        approver: nataliia
```

| Payment | What happens |
|---|---|
| up to `approve_above`, within every limit | agentd pays; a receipt is logged |
| above `approve_above`, up to `max`, within every limit | the approver is asked; agentd pays only on a yes |
| above `max`, over the budget or the count, outside the window, an unlisted payee, a rail that cannot enforce a limit | refused, naming the allowance and the limit |

A paying step names the allowance it draws on:
`payment: {allowance: groceries}`. A Company's priced operation is charged
to the allowance its company entry names. The effective limit is always the
smallest of the allowance, the operator's per-asset caps, and the step's
own `max`.

## 2. Motivation

The owner asked for spending an agent can be trusted with: a person sets
two numbers and a list, and the agent pays within them on its own, asks
above the first, and refuses past the second. RFC-0041 found that no
payment protocol carries all of those limits, so they have to live above
the protocols, with each rail enforcing what it can and the host enforcing
the rest. agentd is the host: it is the only party that sees every payment.

Today agentd's caps are per asset and look back 24 hours at most. They
can't say "£400 a month on groceries, from these two shops, ask Nataliia
above £60". They also change only on a restart, and RFC-0041 requires a
lowered limit to take effect at once.

## 3. The allowance

`security.payments.allowances.<id>`, every amount a decimal in the
allowance's currency:

| Field | Meaning |
|---|---|
| `title` | A label for receipts, gates and the status document |
| `currency` | ISO 4217 code. Every amount below is in it |
| `wallets` | The operator wallets it may draw on; default every wallet |
| `max` | The most one payment may be. Required |
| `budget`, `per` | The most paid per calendar period: `day`, `week` (ISO, from Monday), `month`, `quarter`, `year`, or `once` (the allowance's whole life). `budget` is required, `per` defaults to `once` |
| `times` | The most payments per period |
| `payees` | The payees it may pay: `security.payments.payees` names (x402) or `companies` names (PAP). Default every payee the operator allows |
| `categories` | Kinds of purchase. No rail agentd pays on can enforce them, so a payment under an allowance that sets them is refused `rail-cannot-enforce` (§6) |
| `from`, `until` | RFC 3339 instants: the window payments may happen in |
| `approve_above` | Above it, up to `max`, the approver is asked each time |
| `approver` | Who answers: `operator` or a principal id; default `security.payments.approver` |

Load checks: `max` ≤ `budget`; `approve_above` ≤ `max`; a known `per`; a
three-letter upper-case `currency`; `from` before `until`; every wallet and
payee named exists. Each is an error naming the field.

**Currency.** An allowance's amounts are money; an x402 payment is a token.
An asset is usable under an allowance only when it says what it is worth:
`security.payments.assets.<name>.currency: USD` declares one whole unit of
the token is one unit of that currency (a dollar stablecoin). An asset with
no `currency`, or another one, is not usable under the allowance:
`rail-cannot-enforce: currency`. agentd never converts between currencies.

**Periods** are calendar periods in UTC: a `month` budget renews at
00:00 UTC on the 1st. `once` never renews.

## 4. What an allowance means

1. **It narrows, never widens.** A payment under an allowance must also
   pass every check RFC 0049 makes: the asset's caps, the payee list, the
   step's `max`. The allowance can't create a wallet, raise a cap, or allow
   a payee the operator didn't.
2. **The ledger counts it.** Every ledger record of a payment made under an
   allowance names it (`allowance`, and the amount in the allowance's
   currency). What counts against the budget and the count is what counts
   against a cap today: settled, out, of unknown outcome, and refused until
   its authorization expires. A record under an allowance is kept until
   the period it fell in has ended (for `once`, while the allowance
   exists), not 25 hours.
3. **Keyed by id, not by version.** A changed allowance is the same
   allowance: what was spent this period still counts. Renaming it starts a
   new one.
4. **Changes apply live.** `allowances` reloads (a SIGHUP, or the file
   watch). A lowered budget below what was spent stops further payments at
   once; a raised one allows more. An approval still open is checked again
   when it is answered, against the allowance as it is then (RFC 0049
   already asks the policy again when an approval lands), so lowering or
   removing an allowance revokes approvals given under the old limits. A
   workflow naming an allowance a reload removed fails that reload, said,
   and the old configuration stays — the operator removes the step first.
   Wallets, keys, assets and payees stay restart-only.
5. **At most once** is RFC 0049's: a request with a settled or unknown
   record is never paid again.
6. **Evidence.** A payment's audit record and its `payment.*` events name
   the allowance, the tier it fell in (`auto`, `approved` with by whom, or
   `refused` with the limit), and the amount in the allowance's currency.

## 5. Rails

### 5.1 x402

A paying step (`http`, `mcp.tool`, `a2a.send`, `a2a.delegate`) draws on an
allowance with `payment: {allowance: <id>}`. `wallet` and `max` are then
optional: the wallet defaults to the allowance's only wallet (and must be
one of them), the step's `max` defaults to the allowance's `max` in each
asset whose `currency` is the allowance's. A step may still set a lower
`max`. The policy (`payments::policy`) adds the allowance's checks to every
`accepts` entry, and a passed-over entry names the allowance's reason too.

The tiers combine: the approver is asked when the asset's `approve_above`,
the allowance's `approve_above`, the step's `approve: true`, or the taint
rule says so. When the allowance has an approver it is the one asked.

### 5.2 A Company's operations

An operation a Company proposes carries free-form `terms`. The operator's
company entry says which term is the charge and which allowance it draws
on:

```yaml
companies:
  shop:
    spending: {allowance: groceries, amount: total}
```

`amount` names a term key whose value is `{"amount": "<decimal>",
"currency": "<ISO 4217>"}` (the Personal Agent Protocol's shape) or a
number in the allowance's currency. Before agentd asks the User to approve
an operation, or applies a standing permission to it, the charge is checked
against the allowance:

- over `max`, the budget or the count, outside the window, a company the
  allowance doesn't list, a currency that isn't the allowance's, or a
  charge term that's missing or unreadable: the operation is not confirmed,
  and is cancelled at the Company as a decline is, with the reason;
- above `approve_above`: a standing permission does not cover it, and the
  User is asked;
- otherwise as today: the User is asked, or a standing permission applies.

The allowance never approves an operation itself. The Personal Agent
Protocol requires the User's approval and agentd keeps that. A confirm sent
is a charge in the allowance's ledger. If agentd cancels the operation
later and the Company confirms the cancellation, the charge is credited
back.

### 5.3 A rail that cannot enforce a limit

Neither rail can enforce `categories`, and x402 can't hold a currency
without the asset's `currency`. agentd enforces everything else itself.
A limit that no rail and no host check can enforce refuses the payment,
`rail-cannot-enforce`, naming the limit. agentd never pays "approximately
within" an allowance.

## 6. Observability

- `payment.refused` and the policy's reasons name the allowance and the
  limit: `allowance groceries: budget 400 GBP per month, 385 spent, this
  payment 20`.
- The status document gains `payments.allowances`: for each allowance, the
  current period's start, spent, budget, payments and `times`. The
  operator-only `payment.allowances` op and tool answer the same.
- A reload that changes an allowance logs `payment.allowance.changed`
  (`id`, the fields that changed); one that removes it logs
  `payment.allowance.removed`.

## 7. Workflows

- Every paying step kind takes `payment: {allowance: <id>}` (§5.1).
- `{kind: tool, name: payment.allowances}` reads what is left, so a
  workflow can branch before it tries ("only order if 50 is left this
  month"). It is read-only. Nothing in a workflow changes an allowance.
- A Company operation's confirm (`company.operation.confirm`) is checked
  against its company's allowance whether a workflow or a model calls it.
- Phase C: a document's workflow may pay only under a document allowance
  (§8).

## 8. Phase C: a document's allowances

Built on instruction.md proposal S29 (the `wallet` and `allowance` kinds)
and S28 (protocol names for a wallet's `rails`) in the official SDK,
`instruction-md` 0.2; agentd reads documents only through it
(`config::idoc`), and maps what it reads in
`config::settings::payments::document_allowances`.

- **Grant.** `agent.document_capabilities: [spending]`, off by default.
  Over `url:`, `oci:` or `mcp:` it needs a trust pin and an author
  signature attesting `spending`, as `agents` does (RFC 0056 §4;
  `idoc::ATTESTED_OVER_THE_WIRE`). Without the grant, or over the wire
  without the attestation, the wallets and allowances are delivered as
  text only — the SDK's sentence — nothing is paid under them, and the
  load says so once, naming why.
- **Mapping.** A document's `::wallet{account=…}` names an operator wallet
  by `account`; one that names none is held, said (`host-policy`). Its
  `rails`, empty, are any rail; otherwise they must name x402 version 2
  (`x402`, `x402/2`), and `x402/1` alone is held, `protocol-unsupported`.
  A `:::allowance` becomes an allowance under the document's namespace
  (`doc:<id>`) in the same engine and ledger, keyed by its id, so a
  re-release never resets spend. Its amounts are read as parameters
  resolve; `max` absent is the budget; a date-only window is whole days in
  UTC (the SDK's `instant`); its `payees` are origin hosts (a domain as
  written, an `&service`'s `src` host), matched against the paid
  resource's host beside the operator's payee list, never instead of it —
  and only where agentd requested the resource itself (an `http` step,
  held to the payment request by `same_resource`); on an MCP tool or an
  A2A peer's task the server names the resource, so an allowance with
  `payees` is refused there, `rail-cannot-enforce: payees`;
  `categories` is refused on x402, `rail-cannot-enforce`. It never adds a
  payee or a wallet the operator didn't allow: one that draws on no
  operator wallet is held.
- **Reader errors.** An SDK error about an allowance (`allowance-*`,
  `reference-missing`, `reference-kind`) — the reader's, or the composer's
  once parameters resolve — refuses the document, as every reader error
  does. A document that `extends` another keeps its allowances as text:
  agentd composes no base, so it cannot check that one only narrows what it
  inherits, and `allowance-widened` cannot arise.
- **It only narrows.** Every asset cap, the operator's payee list and the
  step's `max` hold. `security.payments.document_ceiling` (operator-only,
  reloadable) names an operator allowance every document payment also
  counts against and must pass (the ledger record names it, `ceiling`);
  unset, the asset caps are the ceiling.
- **Approvers.** The document's `approver=#person` (or a role's name) is
  bound by the operator to a principal:
  `security.payments.approvers: {nataliia: <principal>}`, never an `agent:`
  one. An allowance whose approver isn't bound pays only up to
  `approve-above`, refuses above it `approver-unbound`, and the load warns
  once. `approve-above` with no approver asks `security.payments.approver`.
  The agent is never the approver. The document's approver answers only
  the document allowance's own tier:

  | Tier | What asks | Who is asked |
  |---|---|---|
  | document | above the document allowance's `approve-above` | its bound approver, else `security.payments.approver` |
  | ceiling | above the `document_ceiling` allowance's `approve_above` | that allowance's `approver`, else `security.payments.approver` |
  | operator | an asset's `approve_above`, `approve: true`, outside text, a model's request | `security.payments.approver` |

  Every approver a tier names must say yes, each on a gate of their own,
  in the order of the table (one approver of two tiers is asked once); a no
  declines; the record names each approval and who gave it. When the last
  yes lands every tier is judged again against the settings then, and a
  tier that now asks, or names another approver, is asked before anything
  is paid; an answer is held to the approver of the tiers its gate was
  opened for, as in force when it lands. Under an operator allowance the
  allowance's approver answers every trigger, as before.
- **Paying from a document.** A document's workflow with a paying step is
  refused (RFC 0049 D2) unless the document has an allowance in force; its
  step must then name one of them (`payment: {allowance: <id>}`, read as
  `doc:<id>`) and no wallet. A document step naming an operator allowance is
  refused, and the operator's workflows never pay under a `doc:` one.
- **Reload.** A re-release that changes an allowance applies at once
  (`payment.allowance.changed`, its `doc:` id); one that removes it fails
  the load of a step naming it, and an approval pending under it is refused
  when answered (`payment.allowance.removed`).
- **Observability.** `payments.allowances`, the `payment.allowances` op and
  tool mark each allowance `source: operator` or `source: document`, with
  the document's URI.
- **Not a Company's.** The Personal Agent Protocol work is parked: a
  document's allowance is not usable for a Company's operations
  (`companies.<name>.spending` naming one is refused).
- **Receipts** in the consumer report the registry reads: **not done** —
  the registry doesn't accept them yet.

## 9. Decisions (the user, 2026-11-03)

1. **Engine now, documents after the SDK.** Build allowances as operator
   configuration now; a document's allowances plug into the same engine once
   S29 is in the SDK. agentd doesn't parse the new kinds itself.
2. **Rails: x402 and the Personal Agent Protocol's priced operations.**
   Cards (Stripe Issuing, ACP with Shared Payment Tokens) and AP2 come later,
   as separate rails.
3. **The grant is like `agents`:** `agent.document_capabilities:
   [spending]`, off by default, over the wire only with a pin and an
   attesting signature.
4. **An unbound approver refuses:** above `approve_above` the allowance
   can't pay, and the load warns `approver-unbound`.

## 10. Phasing

- **A.** Allowances in configuration, the ledger by allowance and period,
  x402 under an allowance, live reload of `allowances`, the status and the
  `payment.allowances` read, docs.
- **B.** A Company's priced operations against an allowance.
- **C.** A document's wallets and allowances, on S29 (§8). Implemented;
  receipts in the consumer report are not.

## 11. Limits

- Periods are UTC calendar periods; a per-allowance time zone is not in
  this RFC.
- No currency conversion: an asset is usable only in the currency it says
  it is.
- `categories` can't be enforced on either rail, so setting them refuses
  every payment under the allowance.
