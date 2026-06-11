# Invoice intake that books the routine and escalates the rest

> **Trigger:** watched folder · **Pattern:** extract → threshold gate → book or pause · **Sample:** [`examples/use-cases/invoice-approval.toml`](../../examples/use-cases/invoice-approval.toml) · **Status:** runs today (`intel-remote,schema,tools-http-tls,trigger-fs-watch`)

## The problem

Accounts payable is a pipeline of judgment calls that are 95% routine:
vendor, amount, PO number, book it. The 5% — an amount that's too big,
a PO that's missing, a vendor nobody recognizes — is why a human reads
all 100%. Automating the 95% with classic OCR templates breaks every
time a vendor redesigns their invoice; automating it with an
unsupervised LLM means the one hallucinated amount ends up in your
ledger.

The shape you actually want: **a machine that extracts and routes, a
threshold that's written down, and a human who only sees exceptions.**

## What the agent does

1. Your scanner/email pipeline drops invoice text into a watched folder;
   the workflow fires on file creation (`fs_watch`, debounced).
2. One LLM step extracts `{vendor, amount, currency, po_number,
   requires_review}` — schema-enforced, with up to two repair rounds if
   the model emits something malformed. The prompt encodes the policy:
   over $2,500, missing PO, or *any uncertainty* → `requires_review:
   true`. The schema makes that a hard boolean.
3. A `condition` node routes on the boolean:
   - **false** → POST to the accounting API; the invoice is booked.
   - **true** → the run checkpoints (`pause_for_approval`) until the
     controller resumes it after a look.
4. Either way, the structured record is archived next to the books, and
   the audit log holds the whole story per invoice.

```toml
[[nodes]]
id = "gate"
type = "condition"
expr = "extract.parsed.requires_review"
```

One boolean, one declared edge each way. That's the approval policy —
versioned, diffable, signable.

## Why finance can sign off on this

- **The threshold is in the file, not in the model's head.** When the
  controller asks "what auto-books?", the answer is a line in a
  version-controlled workflow, not "whatever the AI feels is fine."
  Changing $2,500 to $5,000 is a reviewed pull request.
- **Uncertainty fails toward humans.** The schema's `requires_review`
  must be `true` when extraction is shaky — and even a model that
  somehow emits garbage gets caught by schema validation and bounded
  repair, then a declared failure. There is no path where "couldn't
  parse it" becomes "booked it anyway."
- **The budgets protect the books and the bill.** `max_fs_write_mb`,
  token caps, and a 90-second deadline per invoice mean a poison file
  can't wedge the pipeline or run up a model bill.
- **Replayable evidence.** Run with `--record` and every booking has a
  machine-readable trail: file in, fields out, gate decision, API
  response. Auditors love this more than they love you.

## Honest limits

- The sample reads **text** (exports from your scanner or email parser).
  Native PDF/image parsing is deliberately not in the runtime — put an
  OCR step upstream, or wire a document-parsing MCP server
  ([gap analysis §7](GAP-ANALYSIS.md#7-document-parsing-pdf--docx-guidance)).
- Approvals happen by resuming a run id. A queue UI for pending
  approvals is on the control-plane roadmap — today, `agentd
  --list-checkpoints` and the `/inspect` page are the workbench.
