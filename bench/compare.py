#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Diff two agentd bench scorecards side-by-side (RFC 0024 §1).

The whole evaluation thesis is *comparative*: a score belongs to a
`model × harness` configuration, so the useful output is always a delta —
  * model A vs model B (agentd fixed), or
  * agentd vs a reference scaffold (model fixed), or
  * plain `once` vs a fan-out `workflow` (the §5 workflow ablation).

    python3 bench/compare.py baseline.json candidate.json

Dependency-free (Python 3 stdlib).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def load(path: str) -> dict:
    return json.loads(Path(path).read_text())


def label(card: dict) -> str:
    s = card.get("stamp", {})
    return f'{s.get("config", "?")}@{s.get("version", "?")}'


def _delta(a: float | None, b: float | None, pct: bool = False, invert: bool = False) -> str:
    """b - a, arrow points the 'better' way (invert=True → lower is better)."""
    if a is None or b is None:
        return "  n/a"
    d = b - a
    better = (d < 0) if invert else (d > 0)
    arrow = "→" if abs(d) < 1e-9 else ("▲" if better else "▼")
    val = f"{d:+.1%}" if pct else f"{d:+.1f}"
    return f"{arrow} {val}"


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: compare.py <baseline.json> <candidate.json>", file=sys.stderr)
        return 2
    a, b = load(sys.argv[1]), load(sys.argv[2])
    la, lb = label(a), label(b)
    sa, sb = a["summary"], b["summary"]

    print(f"baseline   A = {la}   ({sys.argv[1]})")
    print(f"candidate  B = {lb}   ({sys.argv[2]})\n")

    rows = [
        ("pass@1",            sa["pass_at_1"], sb["pass_at_1"], True,  False),
        ("pass^k",            sa["pass_hat_k"], sb["pass_hat_k"], True, False),
        ("cost/solved (tok)", sa.get("cost_per_solved_tokens"),
                              sb.get("cost_per_solved_tokens"), False, True),
        ("total tokens",      sa.get("total_tokens"), sb.get("total_tokens"), False, True),
    ]
    metric_w = max(len(r[0]) for r in rows)
    print(f'{"metric".ljust(metric_w)}   {"A":>10}  {"B":>10}   B−A')
    print("-" * (metric_w + 34))
    for name, av, bv, pct, invert in rows:
        fa = "n/a" if av is None else (f"{av:.1%}" if pct else f"{av:.1f}")
        fb = "n/a" if bv is None else (f"{bv:.1%}" if pct else f"{bv:.1f}")
        print(f'{name.ljust(metric_w)}   {fa:>10}  {fb:>10}   {_delta(av, bv, pct, invert)}')

    # Per-task pass regressions / fixes (the actionable diff).
    ta = {t["id"]: t["pass_at_1"] for t in a["tasks"]}
    tb = {t["id"]: t["pass_at_1"] for t in b["tasks"]}
    fixed = sorted(t for t in tb if tb[t] and not ta.get(t, False))
    broke = sorted(t for t in tb if not tb[t] and ta.get(t, False))
    if fixed:
        print(f'\n  B fixes ({len(fixed)}): {", ".join(fixed)}')
    if broke:
        print(f'  B regresses ({len(broke)}): {", ".join(broke)}')
    if not fixed and not broke:
        print("\n  no per-task pass/fail changes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
