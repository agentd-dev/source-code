#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Aggregate many bench scorecards into one table (RFC 0024).

A benchmarking campaign produces one scorecard per (model × benchmark) cell;
this pivots them into a single sorted table of pass@1 / pass^k / cost-per-solved
so a model×benchmark matrix reads at a glance.

    python3 bench/matrix.py card_a.json card_b.json ...

Dependency-free (Python 3 stdlib).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: matrix.py <scorecard.json> ...", file=sys.stderr)
        return 2
    rows = []
    for c in sys.argv[1:]:
        d = json.loads(Path(c).read_text())
        s, st = d["summary"], d["stamp"]
        rows.append((st.get("config", "?"), s["pass_at_1"], s["pass_hat_k"],
                     s.get("cost_per_solved_tokens"), s["tasks"]))
    w = max([len(r[0]) for r in rows] + [6])
    print(f'{"config":<{w}}  pass@1  pass^k  cost/solved(tok)  tasks')
    print("-" * (w + 38))
    for cfg, p1, pk, cost, n in sorted(rows):
        cost_s = "n/a" if cost is None else f"{cost:.0f}"
        print(f'{cfg:<{w}}  {p1:>5.0%}  {pk:>5.0%}  {cost_s:>15}  {n:>5}')
    return 0


if __name__ == "__main__":
    sys.exit(main())
