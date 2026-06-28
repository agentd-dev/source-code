import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { workflow, node } from "../src/index.js";

function classifier() {
  return workflow("doc_classifier")
    .policy({ fs: { write: ["/tmp/agent-out/**"] } })
    .start("main", "classify")
    .node("classify", node.llmInfer({ backend: "claude", prompt: "Classify the document.", inputFrom: "trigger", outputSchema: "inline" }))
    .node("route", node.switch({ expr: "classify.parsed.decision" }))
    .node("save", node.writeFile({ pathFrom: "route.value", contentFrom: "classify.content" }))
    .node("done", node.terminate())
    .edge("classify", "route")
    .edge("route", "save", { when: "invoice" })
    .edge("route", "done", { when: "spam" })
    .edge("save", "done");
}

test("emits the expected TOML shape", () => {
  const toml = classifier().toToml();
  assert.match(toml, /name = "doc_classifier"/);
  assert.match(toml, /\[policy\.fs\]\nwrite = \["\/tmp\/agent-out\/\*\*"\]/);
  assert.match(toml, /\[\[nodes\]\]\nid = "classify"\ntype = "llm_infer"/);
  assert.match(toml, /backend = "claude"/);
  assert.match(toml, /when = "invoice"/);
  // Undefined fields are omitted, not emitted as empty.
  assert.doesNotMatch(toml, /= undefined/);
});

test("rejects duplicate node ids", () => {
  assert.throws(() => workflow("x").node("a", node.merge()).node("a", node.terminate()));
});

// If a built `agent` binary is reachable, prove the generated TOML is
// not just well-shaped but *accepted by the real runtime*.
test("generated TOML validates against agent (when available)", (t) => {
  const candidates = [
    process.env.AGENT_BIN,
    "../../../target/debug/agent",
    "../../target/debug/agent",
  ].filter(Boolean);
  let bin;
  for (const c of candidates) {
    try {
      execFileSync(c, ["--version"], { stdio: "ignore" });
      bin = c;
      break;
    } catch {
      /* try next */
    }
  }
  if (!bin) return t.skip("agent binary not found");

  const dir = mkdtempSync(join(tmpdir(), "agent-sdk-"));
  const file = join(dir, "wf.toml");
  writeFileSync(file, classifier().toToml());
  // --validate-only exits 0 only if the workflow parses + validates.
  execFileSync(bin, ["--config", file, "--validate-only"], { stdio: "ignore" });
});
