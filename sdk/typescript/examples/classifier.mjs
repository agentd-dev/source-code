// Author a workflow in TypeScript/JavaScript, print the TOML agentd runs.
//
//   node examples/classifier.mjs > classifier.toml
//   agentd --config classifier.toml --validate-only
//   agentd --config classifier.toml --input doc.json

import { workflow, node } from "../src/index.js";

const wf = workflow("doc_classifier")
  .policy({
    fs: { write: ["/tmp/agentd-classified/**"] },
  })
  .start("main", "classify")
  .node(
    "classify",
    node.llmInfer({
      backend: "claude",
      prompt: "Classify this document as invoice | contract | spam.",
      inputFrom: "trigger",
      outputSchema: "inline",
    }),
  )
  .node("route", node.switch({ expr: "classify.parsed.decision" }))
  .node("approve", node.pauseForApproval({ reason: "Confirm the classification before filing." }))
  .node("file", node.writeFile({ pathFrom: "route.value", contentFrom: "classify.content" }))
  .node("done", node.terminate())
  .edge("classify", "route")
  .edge("route", "approve", { when: "invoice" })
  .edge("route", "done", { when: "spam" })
  .edge("approve", "file")
  .edge("file", "done");

process.stdout.write(wf.toToml());
