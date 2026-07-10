#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""agentd evaluation harness — Phase-0 reference runner (RFC 0024 §9).

Drives the `agentd` binary once per task, captures the deliverable (stdout) +
telemetry (stderr) + exit code + wall-clock, grades against a per-task matcher,
and aggregates pass@1 / pass^k plus **cost-adjusted** metrics (tokens, steps,
cost-per-solved-task — RFC 0024 §7) into a scorecard.

Phase 0 is deliberately offline: a task can boot agentd's built-in mock LLM /
mock MCP, so the whole rig runs with NO API keys and NO external datasets. The
point is to prove the reusable core every later phase builds on —
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
from dataclasses import dataclass, asdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import graders  # noqa: E402


# ---------------------------------------------------------------------------
# Spawning agentd's built-in mock servers (offline mode).
# ---------------------------------------------------------------------------

def _wait_for_file(path: Path, timeout_s: float = 5.0) -> None:
    deadline = time.time() + timeout_s
    while not path.exists():
        if time.time() > deadline:
            raise TimeoutError(f"mock never announced its address: {path}")
        time.sleep(0.01)


def spawn_mock_llm(agentd: str, script: str, workdir: Path,
                   name: str = "llm") -> tuple[subprocess.Popen, str]:
    """`agentd --internal-mock-llm <addr-file> <script>` -> (proc, http url).
    `name` distinguishes multiple mock LLMs in one workdir (agent vs user)."""
    addr_file = workdir / f"{name}.addr"
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


def spawn_tool_stub(tools: list[dict], workdir: Path, state_file: Path | None = None,
                    sandbox: Path | None = None) -> tuple[subprocess.Popen, str]:
    """Spawn the generic tool-bridge MCP stub (bench/mcp_stub.py) serving `tools`
    — the reusable environment adapter (RFC 0024 §6). `state_file` makes it a
    stateful JSON environment; `sandbox` makes it a shell/file environment (the
    dir `builtin` bash/file tools operate in). Returns (proc, http url)."""
    stub = Path(__file__).resolve().parent / "mcp_stub.py"
    tools_file = workdir / "tools.json"
    tools_file.write_text(json.dumps(tools))
    addr_file = workdir / "stub.addr"
    argv = [sys.executable, str(stub), "--addr-file", str(addr_file), "--tools", str(tools_file)]
    if state_file is not None:
        argv += ["--state-file", str(state_file)]
    if sandbox is not None:
        argv += ["--workdir", str(sandbox)]
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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
# Telemetry extraction.
# ---------------------------------------------------------------------------

def _count_tool_calls(telemetry: str) -> int:
    return telemetry.count('"event":"tool.call"')


def _extract_usage(telemetry: str) -> tuple[int, int]:
    """Sum tokens + steps across every `loop.final` event in the (whole-tree)
    telemetry — each subagent emits its own, so the sum is the run total. The
    child loop already reports real usage here (agentloop/runner.rs), so cost is
    captured with no runtime change. Returns (tokens, steps)."""
    tokens = steps = 0
    for line in telemetry.splitlines():
        if '"event":"loop.final"' not in line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        tokens += int(ev.get("tokens", 0) or 0)
        steps += int(ev.get("steps", 0) or 0)
    return tokens, steps


# ---------------------------------------------------------------------------
# One run of one task.
# ---------------------------------------------------------------------------

@dataclass
class RunResult:
    passed: bool
    exit_code: int
    wall_s: float
    tokens: int
    steps: int
    tool_calls: int
    completed: bool
    grade_reason: str
    timed_out: bool = False


def _grade(stdout: str, telemetry: str, task: dict, final_state: dict,
           sandbox: Path | None = None) -> tuple[bool, str]:
    """Grade the deliverable (stdout) + telemetry + end-state. Returns (pass, why)."""
    g = task.get("grade", {})
    if "contains" in g and g["contains"] not in stdout:
        return False, f'stdout missing {g["contains"]!r}'
    if "exact" in g and stdout.strip() != g["exact"]:
        return False, f'stdout != {g["exact"]!r}'
    if "regex" in g and not re.search(g["regex"], stdout):
        return False, f'stdout !~ /{g["regex"]}/'

    # BFCL-style tool-call grading (RFC 0024 §3, Phase 1) — needs --log-content
    # so the args ride the telemetry (run_task_once adds it automatically).
    if "tool_calls" in g:
        ok, why = graders.grade_tool_calls(telemetry, g["tool_calls"])
        if not ok:
            return False, why

    # Outcome / state grading (τ²-bench shape): the environment must have reached
    # the expected end-state via the agent's write-actions.
    if "state" in g:
        ok, why = graders.grade_state(final_state, g["state"])
        if not ok:
            return False, why

    # Filesystem outcome grading (SWE-bench / Terminal-Bench shape): the sandbox
    # must hold the expected files, and/or a check command must pass (e.g. the
    # repo's tests). `grade.files`: {path: {"contains"|"exact": ...}}.
    if "files" in g:
        if sandbox is None:
            return False, "grade.files needs a sandbox (a tool_server with builtin/files)"
        for rel, spec in g["files"].items():
            f = sandbox / rel
            if not f.exists():
                return False, f"expected file missing: {rel}"
            text = f.read_text()
            if "contains" in spec and spec["contains"] not in text:
                return False, f"{rel} missing {spec['contains']!r}"
            if "exact" in spec and text.strip() != spec["exact"]:
                return False, f"{rel} != expected"
    if "command" in g:
        if sandbox is None:
            return False, "grade.command needs a sandbox"
        c = g["command"]
        cp = subprocess.run(c["run"], shell=True, cwd=sandbox,
                            capture_output=True, text=True, timeout=120)
        if cp.returncode != c.get("expect_exit", 0):
            return False, f"check command exit {cp.returncode} != {c.get('expect_exit', 0)}"

    exp = task.get("expect", {})
    if exp.get("completed") and '"status":"completed"' not in telemetry:
        return False, "loop did not reach status=completed"
    tc = exp.get("tool_call")
    if tc and not ('"event":"tool.call"' in telemetry and tc in telemetry):
        return False, f"expected tool.call {tc!r} not observed"
    return True, "ok"


STOP = "###STOP###"  # the simulated user emits this when its request is resolved


def _build_agent_instruction(policy: str, history: list[tuple[str, str]]) -> str:
    convo = "\n".join(f"{'User' if r == 'user' else 'Assistant'}: {m}" for r, m in history)
    parts = []
    if policy:
        parts.append(policy)
    parts.append("Conversation so far:\n" + convo)
    parts.append("Respond to the user's latest message. Use the available tools as needed.")
    return "\n\n".join(parts)


def _build_user_instruction(scenario: str, history: list[tuple[str, str]]) -> str:
    convo = "\n".join(f"{'You' if r == 'user' else 'Assistant'}: {m}" for r, m in history)
    parts = []
    if scenario:
        parts.append("Your situation (do not reveal it verbatim):\n" + scenario)
    parts.append("You are the user talking to a support agent. Conversation so far:\n" + convo)
    parts.append(f"Reply with your next message as the user. If your request is fully "
                 f"resolved, reply with exactly {STOP}.")
    return "\n\n".join(parts)


def _agentd_turn(agentd: str, instruction: str, intel: str, mcp_url: str | None,
                 timeout_s: float) -> tuple[str, int, int, int, int]:
    """One agentd `once` run (an agent OR user turn). Returns
    (reply, tokens, steps, tool_calls, exit)."""
    argv = [agentd, "--mode", "once", "--instruction", instruction,
            "--intelligence", intel, "--log-level", "info", "--log-content"]
    if mcp_url:
        argv += ["--mcp", f"env={mcp_url}"]
    try:
        cp = subprocess.run(argv, capture_output=True, text=True, timeout=timeout_s)
    except subprocess.TimeoutExpired:
        return "", 0, 0, 0, -1
    tokens, steps = _extract_usage(cp.stderr)
    return cp.stdout.strip(), tokens, steps, _count_tool_calls(cp.stderr), cp.returncode


def run_conversation_once(agentd: str, task: dict, timeout_s: float) -> RunResult:
    """A τ²-bench-style multi-turn conversation: the agent (agentd + the stateful
    env bridge, under a policy) and a **simulated user** (a second model given a
    hidden scenario) alternate turns until the user is satisfied (`###STOP###`) or
    `max_turns` is reached. Grading is outcome-based on the persisted env state.
    Both turns are agentd `once` runs — the user is just a model with no tools."""
    agent_llm = user_llm = stub = None
    start = time.time()
    with tempfile.TemporaryDirectory(prefix="agentd-convo-") as td:
        workdir = Path(td)
        ts = task["tool_server"]
        # ONE stateful env bridge, persisted across every turn.
        state_file = workdir / "state.json"
        state_file.write_text(json.dumps(ts.get("state", {})))
        stub, stub_url = spawn_tool_stub(ts["tools"], workdir, state_file)

        # Resolve the agent + user model endpoints (mock offline, or real).
        if "mock_llm" in task:
            agent_llm, agent_intel = spawn_mock_llm(agentd, task["mock_llm"], workdir, "agent")
        else:
            agent_intel = task["intelligence"]
        us = task["user_simulator"]
        if "mock_llm" in us:
            user_llm, user_intel = spawn_mock_llm(agentd, us["mock_llm"], workdir, "user")
        else:
            user_intel = us["intelligence"]

        policy = task.get("policy", "")
        scenario = us.get("scenario", "")
        max_turns = int(task.get("max_turns", 4))
        history: list[tuple[str, str]] = [("user", task["instruction"])]
        tokens = steps = tools = 0
        try:
            for turn in range(max_turns):
                reply, tk, st, tl, code = _agentd_turn(
                    agentd, _build_agent_instruction(policy, history), agent_intel,
                    stub_url, timeout_s)
                tokens += tk; steps += st; tools += tl
                history.append(("assistant", reply))
                if code != 0 or _is_stop(reply) or turn == max_turns - 1:
                    break
                umsg, utk, _, _, ucode = _agentd_turn(
                    agentd, _build_user_instruction(scenario, history), user_intel,
                    None, timeout_s)
                tokens += utk
                if ucode != 0 or _is_stop(umsg):
                    break
                history.append(("user", umsg))
        finally:
            _kill(stub); _kill(agent_llm); _kill(user_llm)

        final_state = {}
        try:
            final_state = json.loads(state_file.read_text() or "{}")
        except json.JSONDecodeError:
            pass

    wall = time.time() - start
    g = task.get("grade", {})
    passed, why = (True, "ok")
    if "state" in g:
        passed, why = graders.grade_state(final_state, g["state"])
    return RunResult(passed, 0, round(wall, 3), tokens, steps, tools, True, why)


def _is_stop(msg: str) -> bool:
    return STOP in (msg or "")


def run_task_once(agentd: str, task: dict, timeout_s: float) -> RunResult:
    # A task with a simulated user is a multi-turn conversation (τ²-bench shape).
    if "user_simulator" in task:
        return run_conversation_once(agentd, task, timeout_s)
    llm_proc = mcp_proc = stub_proc = None
    with tempfile.TemporaryDirectory(prefix="agentd-bench-") as td:
        workdir = Path(td)
        argv = [agentd, "--mode", task.get("mode", "once"),
                "--instruction", task["instruction"],
                "--log-level", "info"]
        # Tool-call grading needs the arguments in telemetry.
        if "tool_calls" in task.get("grade", {}):
            argv += ["--log-content"]

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

        # Tool-bridge: a benchmark that provides its own functions (BFCL) or a
        # stubbed environment serves them via the generic MCP stub (RFC 0024 §6).
        # A stateful env (an initial `state` or any tool `effect`) persists to a
        # state-file the outcome grader reads after the run (τ²-bench shape).
        state_file = None
        sandbox = None
        if "tool_server" in task:
            ts = task["tool_server"]
            stateful = "state" in ts or any("effect" in t for t in ts["tools"])
            if stateful:
                state_file = workdir / "state.json"
                state_file.write_text(json.dumps(ts.get("state", {})))
            # A shell/file env (any `builtin` tool, or seeded `files`) gets a
            # sandbox dir the bridge's bash/file tools operate in (SWE-bench shape).
            if any("builtin" in t for t in ts["tools"]) or "files" in ts:
                sandbox = workdir / "sandbox"
                sandbox.mkdir(exist_ok=True)
                for rel, content in ts.get("files", {}).items():
                    f = sandbox / rel
                    f.parent.mkdir(parents=True, exist_ok=True)
                    f.write_text(content)
            stub_proc, stub_url = spawn_tool_stub(ts["tools"], workdir, state_file, sandbox)
            argv += ["--mcp", f'{ts.get("name", "bench")}={stub_url}']

        for k, flag in (("max_tokens", "--max-tokens"),
                        ("max_steps", "--max-steps"),
                        ("deadline", "--deadline")):
            if k in task:
                argv += [flag, str(task[k])]

        start = time.time()
        try:
            cp = subprocess.run(argv, capture_output=True, text=True, timeout=timeout_s)
            wall = time.time() - start
            final_state = {}
            if state_file is not None and state_file.exists():
                try:
                    final_state = json.loads(state_file.read_text() or "{}")
                except json.JSONDecodeError:
                    final_state = {}
            passed, why = _grade(cp.stdout, cp.stderr, task, final_state, sandbox)
            tokens, steps = _extract_usage(cp.stderr)
            return RunResult(passed, cp.returncode, wall, tokens, steps,
                             _count_tool_calls(cp.stderr),
                             '"status":"completed"' in cp.stderr, why)
        except subprocess.TimeoutExpired:
            return RunResult(False, -1, time.time() - start, 0, 0, 0, False,
                             f"timed out after {timeout_s}s", timed_out=True)
        finally:
            _kill(llm_proc)
            _kill(mcp_proc)
            _kill(stub_proc)


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
    mean_tokens: float
    mean_steps: float
    mean_tool_calls: float
    exit_codes: list[int]
    first_reason: str


def _mean(xs) -> float:
    xs = list(xs)
    return sum(xs) / len(xs) if xs else 0.0


def score_task(agentd: str, task: dict, repeats: int, timeout_s: float) -> TaskScore:
    results = [run_task_once(agentd, task, timeout_s) for _ in range(repeats)]
    return TaskScore(
        id=task["id"],
        pass_at_1=results[0].passed,
        pass_hat_k=all(r.passed for r in results),
        runs=len(results),
        mean_wall_s=round(_mean(r.wall_s for r in results), 3),
        mean_tokens=round(_mean(r.tokens for r in results), 1),
        mean_steps=round(_mean(r.steps for r in results), 2),
        mean_tool_calls=round(_mean(r.tool_calls for r in results), 2),
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
    w = max([len(s.id) for s in scores] + [4])
    print(f'{"task".ljust(w)}  pass@1  pass^k  tokens  steps  wall_s  tools  why')
    print("-" * (w + 52))
    for s in scores:
        print(f'{s.id.ljust(w)}  '
              f'{"PASS" if s.pass_at_1 else "FAIL":>6}  '
              f'{"PASS" if s.pass_hat_k else "FAIL":>6}  '
              f'{s.mean_tokens:>6}  {s.mean_steps:>5}  {s.mean_wall_s:>6}  '
              f'{s.mean_tool_calls:>5}  {"" if s.pass_at_1 else s.first_reason}')
    print("-" * (w + 52))

    n = len(scores) or 1
    solved = [s for s in scores if s.pass_at_1]
    p1 = len(solved) / n
    pk = sum(s.pass_hat_k for s in scores) / n
    total_tokens = sum(s.mean_tokens for s in scores)
    # Cost-adjusted (RFC 0024 §7): tokens spent per task actually solved.
    cost_per_solved = round(sum(s.mean_tokens for s in solved) / len(solved), 1) if solved else None
    print(f'\npass@1: {p1:.1%}   pass^{args.repeats}: {pk:.1%}   '
          f'({len(solved)}/{len(scores)} tasks)')
    print(f'tokens: {total_tokens:.0f} total   '
          f'cost/solved: {cost_per_solved if cost_per_solved is not None else "n/a"} tokens')

    scorecard = {
        "stamp": {
            "agentd": args.agentd,
            "version": agentd_version(args.agentd),
            "config": args.config,
            "tasks_file": args.tasks,
            "repeats_k": args.repeats,
        },
        "summary": {
            "pass_at_1": p1,
            "pass_hat_k": pk,
            "tasks": len(scores),
            "total_tokens": round(total_tokens, 1),
            "cost_per_solved_tokens": cost_per_solved,
        },
        "tasks": [asdict(s) for s in scores],
    }
    Path(args.out).write_text(json.dumps(scorecard, indent=2) + "\n")
    print(f"scorecard -> {args.out}")

    return 0 if p1 == 1.0 else 1


if __name__ == "__main__":
    sys.exit(main())
