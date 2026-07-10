#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Workflow-lift ablation (RFC 0024 §5) — agentd's most distinctive evaluation.

Run the SAME task suite under several configurations and compare accuracy AND
cost, to map *where decomposition pays off*:

  * once      — a single ReAct loop (the baseline);
  * workflow  — the task wrapped in a one-agent graph (orchestration overhead);
  * fanout-N  — a `foreach` graph that fans the task across N parallel subagents
                and joins (agentd's real decomposition primitive).

The evidence on multi-agent systems is mixed — a single agent often matches or
beats a fan-out at a fraction of the cost, while genuinely-wide tasks win big —
so the honest output isn't "workflows win," it's a per-config **accuracy × cost**
table. Cost is summed across the whole subagent tree (every `loop.final`), so a
fan-out's N× token cost is visible even though the workflow driver reports its
own usage as 0.

Requires a `--features workflow` build of agentd:
    cargo build -p agentd-cli --features workflow
    python3 bench/ablate.py --tasks bench/tasks/ablation_smoke.jsonl --repeats 3

Dependency-free (Python 3 stdlib); reuses bench/run.py.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run  # noqa: E402


def _agent_body(instruction: str) -> dict:
    return {"start": "a", "nodes": {
        "a": {"kind": "agent", "instruction": instruction, "writes": "out", "edges": {"ok": "h"}},
        "h": {"kind": "halt", "status": "completed", "result_from": "out"}}}


def _linear_graph(instruction: str) -> dict:
    return _agent_body(instruction)


def _fanout_graph(instruction: str, n: int) -> dict:
    return {"start": "fan", "nodes": {
        "fan": {"kind": "foreach", "items": [{"i": i} for i in range(n)],
                "body": _agent_body(instruction),
                "parallel": n, "writes": "results", "edges": {"ok": "done", "error": "fail"}},
        "done": {"kind": "halt", "status": "completed", "result_from": "results"},
        "fail": {"kind": "halt", "status": "crashed"}}}


def apply_config(task: dict, config: dict) -> dict:
    """Transform the base (once) task into a config-specific variant. Outcome
    grading (state/files) is config-agnostic — the env end-state is what matters,
    however the work is structured — so the same `grade` applies across configs."""
    t = dict(task)
    kind = config["kind"]
    if kind == "once":
        t.pop("workflow", None)
        return t
    instr = task["instruction"]
    t["workflow"] = _linear_graph(instr) if kind == "linear" else _fanout_graph(instr, config["n"])
    t.pop("mode", None)
    return t


DEFAULT_CONFIGS = [
    {"name": "once", "kind": "once"},
    {"name": "workflow", "kind": "linear"},
    {"name": "fanout-3", "kind": "fanout", "n": 3},
]


def main() -> int:
    default_bin = str(Path(__file__).resolve().parents[1] / "target" / "debug" / "agentd")
    ap = argparse.ArgumentParser(description="workflow-lift ablation (RFC 0024 §5)")
    ap.add_argument("--agentd", default=default_bin, help="a --features workflow build of agentd")
    ap.add_argument("--tasks", default=str(Path(__file__).resolve().parent / "tasks" / "ablation_smoke.jsonl"))
    ap.add_argument("--repeats", type=int, default=1, help="runs per task (k for pass^k)")
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("--out", default=str(Path(__file__).resolve().parent / "ablation.json"))
    args = ap.parse_args()

    if not Path(args.agentd).exists():
        print(f"agentd not found: {args.agentd} (build with --features workflow)", file=sys.stderr)
        return 2
    tasks = [json.loads(l) for l in Path(args.tasks).read_text().splitlines()
             if l.strip() and not l.lstrip().startswith("//")]

    print(f"agentd: {args.agentd}  ({run.agentd_version(args.agentd)})")
    print(f"tasks: {len(tasks)}   repeats(k): {args.repeats}\n")

    # config -> [TaskScore]
    per_config = {}
    for cfg in DEFAULT_CONFIGS:
        per_config[cfg["name"]] = [
            run.score_task(args.agentd, apply_config(t, cfg), args.repeats, args.timeout)
            for t in tasks
        ]

    n = len(tasks) or 1
    base = "once"

    def rate(scores, attr):
        return sum(getattr(s, attr) for s in scores) / n

    base_tok = sum(s.mean_tokens for s in per_config[base]) / n
    print(f'{"config":<12}  pass@1  pass^k  tok/task  Δcost   verdict')
    print("-" * 58)
    summary = {}
    for cfg in DEFAULT_CONFIGS:
        s = per_config[cfg["name"]]
        p1 = rate(s, "pass_at_1")
        pk = rate(s, "pass_hat_k")
        tok = sum(x.mean_tokens for x in s) / n
        dcost = (tok / base_tok - 1.0) if base_tok else 0.0
        # honest verdict vs the baseline: did the extra cost buy accuracy?
        dp1 = p1 - rate(per_config[base], "pass_at_1")
        if cfg["name"] == base:
            verdict = "baseline"
        elif dp1 > 1e-9:
            verdict = f"+{dp1:.0%} acc"
        elif dcost > 0.05:
            verdict = "cost, no gain"
        else:
            verdict = "≈ baseline"
        print(f'{cfg["name"]:<12}  {p1:>5.0%}  {pk:>5.0%}  {tok:>7.1f}  {dcost:>+5.0%}   {verdict}')
        summary[cfg["name"]] = {"pass_at_1": p1, "pass_hat_k": pk,
                                "tokens_per_task": round(tok, 1), "cost_delta": round(dcost, 3)}
    print("-" * 58)
    print("\nverdict reads the workflow-lift: extra cost is only worth it when it "
          "buys accuracy — the 'where decomposition pays off' map.")

    Path(args.out).write_text(json.dumps({
        "stamp": {"agentd": args.agentd, "version": run.agentd_version(args.agentd),
                  "tasks_file": args.tasks, "repeats_k": args.repeats},
        "configs": summary,
        "per_task": {c: [{"id": s.id, "pass_at_1": s.pass_at_1, "tokens": s.mean_tokens}
                         for s in per_config[c]] for c in per_config},
    }, indent=2) + "\n")
    print(f"ablation -> {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
