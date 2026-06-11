# @agentd/sdk

Author [agentd](https://github.com/agentd-dev/source-code) workflows in
TypeScript or JavaScript and compile them to the TOML the runtime
executes. **TOML stays the compile target; this is the authoring
surface** — you write in your stack and inherit the Rust runtime's
guarantees (validation, policy, signing, budgets, observability).

Zero runtime dependencies. Ships as ESM JavaScript with TypeScript type
declarations — no build step.

## Use

```ts
import { workflow, node } from "@agentd/sdk";

const wf = workflow("doc_classifier")
  .policy({ fs: { write: ["/tmp/agentd-classified/**"] } })
  .start("main", "classify")
  .node("classify", node.llmInfer({
    backend: "claude",
    prompt: "Classify this document as invoice | contract | spam.",
    inputFrom: "trigger",
    outputSchema: "inline",
  }))
  .node("route", node.switch({ expr: "classify.parsed.decision" }))
  .node("file", node.writeFile({ pathFrom: "route.value", contentFrom: "classify.content" }))
  .node("done", node.terminate())
  .edge("classify", "route")
  .edge("route", "file", { when: "invoice" })
  .edge("route", "done", { when: "spam" })
  .edge("file", "done");

console.log(wf.toToml());
```

Then validate and run with the binary:

```bash
node build.mjs > classifier.toml
agentd --config classifier.toml --validate-only   # the runtime is the source of truth
agentd --config classifier.toml --input doc.json
```

The SDK emits the graph; the **runtime validator is authoritative** —
always `--validate-only` what you generate. The package's own test suite
round-trips its output through a real `agentd` binary to prove the TOML
is accepted, not just well-shaped.

## Node kinds

`llmInfer` · `agentLoop` · `switch` · `condition` · `merge` ·
`terminate` · `fail` · `pauseForApproval` · `readEnv` · `readFile` ·
`writeFile` · `createDir` · `parseJson` · `jsonSelect` ·
`templateRender` · `httpRequest` · `call` — mirroring the runtime's node
types (see `docs/capabilities.md`).

## Test

```bash
npm test                          # node --test
AGENTD_BIN=/path/to/agentd npm test   # also validate against a specific binary
```
