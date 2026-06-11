// Type declarations for @agentd/sdk. The implementation is plain ESM
// JavaScript (zero build); these give TypeScript authors full types.

export type NodeSpec = { type: string; [field: string]: string | number | boolean | string[] | undefined };

export interface Policy {
  fs?: { read?: string[]; write?: string[]; delete?: string[] };
  env?: { read_keys?: string[] };
  http?: { urls?: string[]; methods?: string[] };
  shell?: { commands?: string[] };
  mcp?: { tools?: string[]; resources?: string[] };
}

export const node: {
  llmInfer(o: { backend: string; prompt: string; inputFrom?: string; outputSchema?: string }): NodeSpec;
  agentLoop(o: {
    backend: string;
    instructions?: string;
    instructionsFrom?: string;
    tools: string[];
    maxSteps: number;
    maxTokens?: number;
  }): NodeSpec;
  switch(o: { expr: string }): NodeSpec;
  condition(o: { expr: string }): NodeSpec;
  merge(): NodeSpec;
  terminate(): NodeSpec;
  fail(o?: { reason?: string }): NodeSpec;
  pauseForApproval(o?: { reason?: string }): NodeSpec;
  readEnv(o: { key: string }): NodeSpec;
  readFile(o: { pathFrom: string }): NodeSpec;
  writeFile(o: { pathFrom: string; contentFrom: string }): NodeSpec;
  createDir(o: { pathFrom: string }): NodeSpec;
  parseJson(o: { inputFrom: string }): NodeSpec;
  jsonSelect(o: { inputFrom: string; path: string }): NodeSpec;
  templateRender(o: { template: string; inputFrom?: string }): NodeSpec;
  httpRequest(o: { method: string; urlFrom: string; bodyFrom?: string }): NodeSpec;
  call(o: { workflow: string; inputFrom?: string; start?: string }): NodeSpec;
  /** Shape the HTTP reply of an http-triggered run (TwiML, Slack shapes, challenge echoes). */
  respond(o: { bodyTemplate: string; status?: number; contentType?: string; inputFrom?: string }): NodeSpec;
  /** Run one sub-workflow per element of a context-resolved array, bounded by maxItems. */
  map(o: { itemsFrom: string; workflow: string; maxItems: number; start?: string; maxConcurrent?: number }): NodeSpec;
};

export class Workflow {
  constructor(name: string);
  name: string;
  start(name: string, entryNode: string, source?: "manual" | "http" | "event"): this;
  node(id: string, spec: NodeSpec): this;
  edge(from: string, to: string, opts?: { when?: string }): this;
  policy(p: Policy): this;
  /** Compile to workflow TOML — validate it with `agentd --validate-only`. */
  toToml(): string;
}

export function workflow(name: string): Workflow;
