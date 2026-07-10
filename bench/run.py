#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""agentd evaluation harness — Phase-0 reference runner (RFC 0024 §9).

Drives the `agentd` binary once per task, captures the deliverable (stdout) +
telemetry (stderr) + exit code + wall-clock, grades against a per-task matcher,
and aggregates pass@1 / pass^k / wall-clock / tool-calls into a scorecard.

Phase 0 is deliberately offline: a task can boot agentd's built-in mock LLM /
mock MCP, so the whole rig runs with NO API keys and NO external datasets. The
point of Phase 0 is to prove the reusable core every later phase builds on —
drive agentd -> capture deliverable -> grade -> aggregate telemetry.

Pointing at a REAL model + REAL MCP servers is a *data* change, not a code
change: give a task an `intelligence` URL (+ optional `model`, `mcp: [...]`)
instead of `mock_llm`/`mock_mcp`. See bench/README.md.

Dependency-free: Python 3 standard library only (RFC 0024 §2 — tooling stays out
of the moat).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field, asdict
from pathlib import Path


# ---------------------------------------------------------------------------
# Spawning agentd's built-in mock servers (offline mode).
# ---------------------------------------------------------------------------

def _wait_for_file(path: Path, timeout_s: float = 5.0) -> None:
    deadline = time.time() + timeout_s
    while not path.exists():
        if time.time() > deadline:
            raise TimeoutError(f"mock never announced its address: {path}")
        time.sleep(0.01)


def spawn_mock_llm(agentd: str, script: str, workdir: Path) -> tuple[subprocess.Popen, str]:
    """`agentd --internal-mock-llm <addr-file> <script>` -> (proc, http url)."""
    addr_file = workdir / "llm.addr"
    proc = subprocess.Popen(
        [agentd, "--internal-mock-llm", str(addr_file), script],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    _wait_for_file(addr_file)
    return proc, "http://" + addr_file.read_text().strip()


def spawn_mock_mcp(agentd: str, uri: str, emit: bool, workdir: Path) -> tuple[subprocess.Popen, str]:
    """`agentd --internal-mock-mcp-http <addr-file> <uri> [--no-emit]` -> (proc, http url)."""
    addr_file = workdir / "mcp.addr"
    args = [agentd, "--internal-mock-mcp-http", str(addr_file), uri]
    if not emit:
        args.append("--no-emit")
    proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    _wait_for_file(addr_file)
    return proc, "http://" + addr_file.read_text().strip()


def _kill(proc: subprocess.Popen | None) -> None:
    if proc and proc.poll() is None:
        proc.kill()
        try:
            proc.wait(timeout=2)
        except Exception:
            pass


# ---------------------------------------------------------------------------
# One run of one task.
# ---------------------------------------------------------------------------

@dataclass
class RunResult:
    passed: bool
    exit_code: int
    wall_s: float
    tool_calls: int
    completed: bool
    grade_reason: str
    timed_out: bool = False


def _count_tool_calls(telemetry: str) -> int:
    return telemetry.count('"event":"tool.call"')


def _grade(stdout: str, telemetry: str, task: dict) -> tuple[bool, str]:
    """Grade the deliverable (stdout) + telemetry assertions. Returns (pass, why)."""
    g = task.get("grade", {})
    if "contains" in g and g["contains"] not in stdout:
        return False, f'stdout missing {g["contains"]!r}'
    if "exact" in g and stdout.strip() != g["exact"]:
        return False, f'stdout != {g["exact"]!r}'
    if "regex" in g and not re.search(g["regex"], stdout):
        return False, f'stdout !~ /{g["regex"]}/'

    exp = task.get("expect", {})
    if exp.get("completed") and '"status":"completed"' not in telemetry:
        return False, "loop did not reach status=completed"
    tc = exp.get("tool_call")
    if tc and not ('"event":"tool.call"' in telemetry and tc in telemetry):
        return False, f"expected tool.call {tc!r} not observed"
    return True, "ok"


def run_task_once(agentd: str, task: dict, timeout_s: float) -> RunResult:
    llm_proc = mcp_proc = None
    with tempfile.TemporaryDirectory(prefix="agentd-bench-") as td:
        workdir = Path(td)
        argv = [agentd, "--mode", "once",
                "--instruction", task["instruction"],
                "--log-level", "info"]

        # Intelligence: built-in mock (offline) or a real endpoint (Phase 1+).
        if "mock_llm" in task:
            llm_proc, intel = spawn_mock_llm(agentd, task["mock_llm"], workdir)
        else:
            intel = task["intelligence"]
            if task.get("model"):
                argv += ["--model", task["model"]]
        argv += ["--intelligence", intel]

        # Tools: built-in mock MCP (offline) or real --mcp servers (Phase 1+).
        if "mock_mcp" in task:
            m = task["mock_mcp"]
            mcp_proc, mcp_url = spawn_mock_mcp(
                agentd, m["uri"], m.get("emit", False), workdir)
            argv += ["--mcp", f'{m.get("name", "mock")}={mcp_url}']
        for spec in task.get("mcp", []):
            argv += ["--mcp", spec]

        for k, flag in (("max_tokens", "--max-tokens"),
                        ("max_steps", "--max-steps"),
                        ("deadline", "--deadline")):
            if k in task:
                argv += [flag, str(task[k])]

        start = time.time()
        try:
            cp = subprocess.run(argv, capture_output=True, text=True, timeout=timeout_s)
            wall = time.time() - start
            passed, why = _grade(cp.stdout, cp.stderr, task)
            return RunResult(passed, cp.returncode, wall,
                             _count_tool_calls(cp.stderr),
                             '"status":"completed"' in cp.stderr, why)
        except subprocess.TimeoutExpired:
            return RunResult(False, -1, time.time() - start, 0, False,
                             f"timed out after {timeout_s}s", timed_out=True)
        finally:
            _kill(llm_proc)
            _kill(mcp_proc)


# ---------------------------------------------------------------------------
# Aggregation across tasks and repeats.
# ---------------------------------------------------------------------------

@dataclass
class TaskScore:
    id: str
    pass_at_1: bool
    pass_hat_k: bool           # solved on EVERY one of k runs (reliability)
    runs: int
    mean_wall_s: float
    mean_tool_calls: float
    exit_codes: list[int]
    first_reason: str


def score_task(agentd: str, task: dict, repeats: int, timeout_s: float) -> TaskScore:
    results = [run_task_once(agentd, task, timeout_s) for _ in range(repeats)]
    n = len(results)
    return TaskScore(
        id=task["id"],
        pass_at_1=results[0].passed,
        pass_hat_k=all(r.passed for r in results),
        runs=n,
        mean_wall_s=round(sum(r.wall_s for r in results) / n, 3),
        mean_tool_calls=round(sum(r.tool_calls for r in results) / n, 2),
        exit_codes=[r.exit_code for r in results],
        first_reason=results[0].grade_reason,
    )


def agentd_version(agentd: str) -> str:
    try:
        return subprocess.run([agentd, "--version"], capture_output=True, text=True,
                              timeout=10).stdout.strip()
    except Exception:
        return "unknown"


def main() -> int:
    default_bin = str(Path(__file__).resolve().parents[1] / "target" / "debug" / "agentd")
    ap = argparse.ArgumentParser(description="agentd evaluation harness — Phase-0 runner")
    ap.add_argument("--agentd", default=os.environ.get("AGENTD_BIN", default_bin),
                    help="path to the agentd binary (default: target/debug/agentd)")
    ap.add_argument("--tasks", default=str(Path(__file__).resolve().parent / "tasks" / "smoke.jsonl"),
                    help="task suite (.jsonl, one task per line)")
    ap.add_argument("--repeats", type=int, default=1, help="runs per task (k for pass^k)")
    ap.add_argument("--timeout", type=float, default=60.0, help="per-run wall-clock cap (s)")
    ap.add_argument("--config", default="once", help="label for this harness config (stamped)")
    ap.add_argument("--out", default=str(Path(__file__).resolve().parent / "scorecard.json"))
    args = ap.parse_args()

    if not Path(args.agentd).exists():
        print(f"agentd binary not found: {args.agentd}\n"
              f"build it first: cargo build -p agentd-cli", file=sys.stderr)
        return 2

    tasks = [json.loads(line) for line in Path(args.tasks).read_text().splitlines()
             if line.strip() and not line.lstrip().startswith("//")]

    print(f"agentd: {args.agentd}")
    print(f"version: {agentd_version(args.agentd)}")
    print(f"config: {args.config}   tasks: {len(tasks)}   repeats(k): {args.repeats}\n")

    scores = [score_task(args.agentd, t, args.repeats, args.timeout) for t in tasks]

    # Printed table.
    w = max((len(s.id) for s in scores), default=4)
    print(f'{"task".ljust(w)}  pass@1  pass^k  wall_s  tools  exits       why')
    print("-" * (w + 48))
    for s in scores:
        print(f'{s.id.ljust(w)}  '
              f'{"PASS" if s.pass_at_1 else "FAIL":>6}  '
              f'{"PASS" if s.pass_hat_k else "FAIL":>6}  '
              f'{s.mean_wall_s:>6}  {s.mean_tool_calls:>5}  '
              f'{str(s.exit_codes):<11} {"" if s.pass_at_1 else s.first_reason}')

    n = len(scores) or 1
    p1 = sum(s.pass_at_1 for s in scores) / n
    pk = sum(s.pass_hat_k for s in scores) / n
    print("-" * (w + 48))
    print(f'\npass@1: {p1:.1%}   pass^{args.repeats}: {pk:.1%}   '
          f'({sum(s.pass_at_1 for s in scores)}/{len(scores)} tasks)')

    scorecard = {
        "stamp": {
            "agentd": args.agentd,
            "version": agentd_version(args.agentd),
            "config": args.config,
            "tasks_file": args.tasks,
            "repeats_k": args.repeats,
        },
        "summary": {"pass_at_1": p1, "pass_hat_k": pk, "tasks": len(scores)},
        "tasks": [asdict(s) for s in scores],
    }
    Path(args.out).write_text(json.dumps(scorecard, indent=2) + "\n")
    print(f"scorecard -> {args.out}")

    return 0 if p1 == 1.0 else 1


if __name__ == "__main__":
    sys.exit(main())
