# RFC 0057: Spending controls — allowances

**Status:** Accepted 2026-11-03 by the user (decisions in §9); phases A–B
being built, phase C waits on the specification
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

Waiting on instruction.md proposal S29 (the `wallet` and `allowance` kinds)
in the official SDK; agentd reads documents only through it. Then:

- **Grant.** `agent.document_capabilities: [spending]`, off by default.
  Over `url:`, `oci:` or `mcp:` it needs a trust pin and an author
  signature attesting `spending`, as `agents` does (RFC 0056 §4). A
  document arriving any other way never spends.
- **Mapping.** A document's `::wallet{account=…}` names an operator wallet
  by `account`; one that names none is held, said. A `:::allowance`
  becomes an allowance under the document's namespace (`doc:<id>`), whose
  every limit narrows the operator allowances and caps it draws on. It
  never adds a payee or a wallet the operator didn't allow.
- **Approvers.** The document's `approver=#person` is bound by the operator
  to a principal: `security.payments.approvers: {nataliia: <principal>}`.
  An allowance whose approver isn't bound pays only below `approve_above`,
  and the load warns `approver-unbound`. The agent is never the approver.
- **Paying from a document.** A document's workflow with a paying step is
  refused today (RFC 0049 D2). Under the grant, it is allowed only when the
  step names a document allowance.
- **Receipts** go into the consumer report the registry reads.

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
- **C.** A document's wallets and allowances, after S29 (§8).

## 11. Limits

- Periods are UTC calendar periods; a per-allowance time zone is not in
  this RFC.
- No currency conversion: an asset is usable only in the currency it says
  it is.
- `categories` can't be enforced on either rail, so setting them refuses
  every payment under the allowance.
