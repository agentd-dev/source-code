# SPDX-License-Identifier: Apache-2.0
"""Graders for the agentd eval harness (RFC 0024).

The **tool-call grader** is the Phase-1 workhorse (BFCL-style): did the agent
emit the right tool call(s)? It reads agentd's `tool.call` telemetry — which
carries the (unprefixed, catalogue-verbatim) tool name and, under `--log-content`,
the JSON arguments — and matches against a ground truth.

Ground-truth shape (a BFCL-friendly subset):
    {"name": "get_weather", "args": {"city": "Paris", "unit": ["c", "celsius"]}}
  * `args` is optional; each named arg must be present with a matching value;
  * a value may be a LIST of acceptable values (BFCL allows several valid forms);
  * an arg absent from the ground truth is not constrained (extra args are ok).
A ground truth may itself be a LIST of alternatives — any one matching passes.

Not a full BFCL AST checker (no type coercion / nested-structure canon / optional
-param permutations); it covers the common single-call categories and is the
honest Phase-1 start. Point stricter categories at BFCL's own checker.

Dependency-free (Python 3 stdlib).
"""

from __future__ import annotations

import json
import re


def _norm(v):
    """Normalize a value for lenient comparison. For strings this canonicalizes
    the function-expression forms BFCL uses interchangeably — `x^2`≡`x**2`, an
    optional `lambda x:` prefix, and incidental whitespace — so a model that
    echoes the prompt's `2x^2` matches a ground truth of `2x**2`. Not the full
    BFCL AST checker, but it closes the common function-string gap; numbers are
    left to Python equality (1 == 1.0)."""
    if not isinstance(v, str):
        return v
    s = v.strip()
    s = re.sub(r"^lambda\s+\w+\s*:\s*", "", s)   # drop a lambda prefix
    s = s.replace("^", "**")                      # math power notation
    s = re.sub(r"\s+", "", s)                      # incidental whitespace
    return s


# BFCL's `standardize_string`: strip the punctuation/whitespace the leaderboard
# treats as insignificant, lowercase, and canonicalize quotes. This is what makes
# the AST checker case- and formatting-agnostic ("New York" ≡ "new_york" ≡
# "newyork"), so a model isn't marked wrong for cosmetic string differences.
_STD_RE = re.compile(r"[ ,./\-_*^]")


def _std(s: str) -> str:
    return _STD_RE.sub("", s).lower().replace("'", '"')


def _as_num(x):
    """Return x as a float for numeric comparison, or None if it isn't a plain
    number. Booleans are NOT numbers here (True != 1) so bool stays exact."""
    if isinstance(x, bool):
        return None
    if isinstance(x, (int, float)):
        return float(x)
    return None


def _deep_eq(actual, cand) -> bool:
    """Faithful BFCL AST value comparison of a concrete model value against ONE
    concrete acceptable value (the possible-answer alternatives are peeled off by
    `_arg_matches` before this is called). Mirrors BFCL's checkers:
      * numbers — int/float auto-convert (5 ≡ 5.0), bools stay exact;
      * strings — `standardize_string` equality, with the calculus `_norm` as an
        extra accepted form (`2x^2` ≡ `2x**2`);
      * lists/tuples — order-sensitive, element-wise, recursive (tuple ≡ list);
      * dicts — recursive; model keys must be expected, and every expected key
        must be present unless its value marks it optional (`""`)."""
    na, nc = _as_num(actual), _as_num(cand)
    if na is not None and nc is not None:
        return na == nc
    # Booleans are type-strict: `True` matches only a boolean `True`, never the
    # number 1 (Python's `True == 1` would otherwise leak through).
    if isinstance(cand, bool) or isinstance(actual, bool):
        return isinstance(cand, bool) and isinstance(actual, bool) and actual == cand
    if cand is None or actual is None:
        return actual == cand
    if isinstance(cand, str) and isinstance(actual, str):
        return _std(actual) == _std(cand) or _norm(actual) == _norm(cand)
    if isinstance(cand, (list, tuple)) and isinstance(actual, (list, tuple)):
        a, c = list(actual), list(cand)
        return len(a) == len(c) and all(_deep_eq(x, y) for x, y in zip(a, c))
    if isinstance(cand, dict) and isinstance(actual, dict):
        if any(k not in cand for k in actual):          # model emitted an unexpected key
            return False
        for k, v in cand.items():
            optional = v == "" or (isinstance(v, list) and "" in v)
            if k not in actual:
                if optional:
                    continue
                return False
            # a nested value may itself carry scalar alternatives (BFCL lists)
            if isinstance(v, list) and all(not isinstance(e, (list, dict)) for e in v):
                if not any(_deep_eq(actual[k], e) for e in v if e != ""):
                    return False
            elif not _deep_eq(actual[k], v):
                return False
        return True
    return actual == cand


def extract_tool_calls(telemetry: str) -> list[dict]:
    """Parse every `tool.call` event -> [{"name", "args"}]. `args` needs the run
    to pass `--log-content` (else it is absent and treated as {})."""
    calls = []
    for line in telemetry.splitlines():
        if '"event":"tool.call"' not in line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        args = {}
        raw = ev.get("args")
        if isinstance(raw, str):
            try:
                args = json.loads(raw)
            except json.JSONDecodeError:
                args = {}
        elif isinstance(raw, dict):
            args = raw
        calls.append({"name": ev.get("tool"), "args": args})
    return calls


def _arg_matches(actual, accepted) -> bool:
    """Does the model's arg value match any BFCL-acceptable value? `accepted` is
    the possible-answer list of alternatives (a lone value is wrapped). Each is
    compared with the faithful BFCL AST rules (`_deep_eq`): standardized strings,
    int/float coercion, order-sensitive lists, recursive dicts."""
    accept = accepted if isinstance(accepted, list) else [accepted]
    return any(_deep_eq(actual, cand) for cand in accept)


def _call_matches(call: dict, expected: dict) -> bool:
    if call["name"] != expected.get("name"):
        return False
    for key, want in (expected.get("args") or {}).items():
        if key not in call["args"]:
            return False
        if not _arg_matches(call["args"][key], want):
            return False
    return True


def _subset(actual, expected) -> bool:
    """Is `expected` contained in `actual`? Dicts match key-by-key recursively
    (extra keys in `actual` are fine); lists must match element-for-element in
    order; scalars must be equal. The outcome-grading primitive (τ²-bench shape:
    did the environment reach the expected end-state)."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            return False
        return all(k in actual and _subset(actual[k], v) for k, v in expected.items())
    if isinstance(expected, list):
        if not isinstance(actual, list) or len(actual) != len(expected):
            return False
        return all(_subset(a, e) for a, e in zip(actual, expected))
    return actual == expected


def grade_state(final_state: dict, expected: dict) -> tuple[bool, str]:
    """Outcome-based grading (τ²-bench / MCP-Universe): the write-actions the agent
    took must have left the environment in the expected end-state. `expected` is a
    partial spec — a subset of the final state."""
    if _subset(final_state or {}, expected):
        return True, "ok"
    return False, f"final state did not satisfy {expected}; got {final_state}"


def _at(state, dotted: str):
    cur = state
    for p in dotted.split("."):
        if not isinstance(cur, dict) or p not in cur:
            return None
        cur = cur[p]
    return cur


def grade_state_absent(final_state: dict, paths: list[str]) -> tuple[bool, str]:
    """Policy-refusal grading (τ²-bench): each dotted path must be ABSENT / null in
    the final state — the agent must NOT have taken a forbidden write-action (e.g.
    cancelling a shipped order the policy forbids)."""
    for p in paths:
        if _at(final_state or {}, p) is not None:
            return False, f"forbidden action was taken: {p} = {_at(final_state, p)!r}"
    return True, "ok"


def grade_env(final_state: dict, grade: dict) -> tuple[bool, str]:
    """Combined environment grading: `state` (required end-state) AND `state_absent`
    (forbidden actions). Both must hold."""
    if "state" in grade:
        ok, why = grade_state(final_state, grade["state"])
        if not ok:
            return ok, why
    if "state_absent" in grade:
        ok, why = grade_state_absent(final_state, grade["state_absent"])
        if not ok:
            return ok, why
    return True, "ok"


def grade_tool_calls(telemetry: str, expected) -> tuple[bool, str]:
    """`expected` is a ground-truth call, a list of acceptable alternatives, or a
    list of REQUIRED calls (all must appear) when tagged {"all": [...]}."""
    calls = extract_tool_calls(telemetry)

    # Sequence mode: every listed call must appear (in any order).
    if isinstance(expected, dict) and "all" in expected:
        for want in expected["all"]:
            if not any(_call_matches(c, want) for c in calls):
                return False, f"required call not seen: {want.get('name')}"
        return True, "ok"

    alts = expected if isinstance(expected, list) else [expected]
    for alt in alts:
        if any(_call_matches(c, alt) for c in calls):
            return True, "ok"
    got = [c["name"] for c in calls]
    return False, f"no call matched {[a.get('name') for a in alts]}; got {got or 'none'}"


# --- self-checks (run: python3 bench/graders.py) -------------------------------

def _selftest() -> None:
    tele = (
        '{"event":"tool.call","tool":"get_weather","args":"{\\"city\\":\\"Paris\\",\\"unit\\":\\"c\\"}"}\n'
        '{"event":"tool.result","tool":"get_weather"}\n'
    )
    ok, _ = grade_tool_calls(tele, {"name": "get_weather", "args": {"city": "Paris"}})
    assert ok
    # value alternatives (BFCL "several valid forms")
    ok, _ = grade_tool_calls(tele, {"name": "get_weather", "args": {"unit": ["c", "celsius"]}})
    assert ok
    # wrong value fails
    ok, _ = grade_tool_calls(tele, {"name": "get_weather", "args": {"city": "London"}})
    assert not ok
    # wrong name fails
    ok, why = grade_tool_calls(tele, {"name": "get_time"})
    assert not ok and "get_weather" in why
    # alternatives: any one matches
    ok, _ = grade_tool_calls(tele, [{"name": "get_time"}, {"name": "get_weather"}])
    assert ok
    # 'all' sequence mode
    tele2 = tele + '{"event":"tool.call","tool":"send","args":"{}"}\n'
    ok, _ = grade_tool_calls(tele2, {"all": [{"name": "get_weather"}, {"name": "send"}]})
    assert ok
    ok, _ = grade_tool_calls(tele, {"all": [{"name": "get_weather"}, {"name": "send"}]})
    assert not ok

    # function-string normalization (BFCL calculus args): 2x^2 matches 2x**2, and
    # a lambda form matches its bare expression.
    ftele = '{"event":"tool.call","tool":"deriv","args":"{\\"f\\":\\"2x^2\\"}"}\n'
    ok, _ = grade_tool_calls(ftele, {"name": "deriv", "args": {"f": ["2x**2", "lambda x: 2x**2"]}})
    assert ok
    ltele = '{"event":"tool.call","tool":"deriv","args":"{\\"f\\":\\"lambda x: x**3\\"}"}\n'
    ok, _ = grade_tool_calls(ltele, {"name": "deriv", "args": {"f": ["x**3"]}})
    assert ok
    # but a genuinely different expression still fails (no false positive)
    ok, _ = grade_tool_calls(ftele, {"name": "deriv", "args": {"f": ["3x**2"]}})
    assert not ok

    # --- faithful BFCL AST value matching (the fidelity upgrade) ----------------
    # string standardization: case + the punctuation/whitespace BFCL ignores.
    stele = '{"event":"tool.call","tool":"f","args":"{\\"city\\":\\"New York\\"}"}\n'
    for gt in ("new york", "new_york", "New-York", "newyork"):
        ok, _ = grade_tool_calls(stele, {"name": "f", "args": {"city": [gt]}})
        assert ok, gt
    ok, _ = grade_tool_calls(stele, {"name": "f", "args": {"city": ["Boston"]}})
    assert not ok                                    # a different string still fails
    # int/float auto-conversion (5 ≡ 5.0), but bools stay exact (True ≢ 1).
    ntele = '{"event":"tool.call","tool":"f","args":"{\\"n\\":5,\\"flag\\":true}"}\n'
    ok, _ = grade_tool_calls(ntele, {"name": "f", "args": {"n": [5.0]}})
    assert ok
    ok, _ = grade_tool_calls(ntele, {"name": "f", "args": {"flag": [1]}})
    assert not ok                                    # bool is not the number 1
    # lists: order-sensitive, element-wise, with standardized string elements.
    ltele2 = '{"event":"tool.call","tool":"f","args":"{\\"xs\\":[\\"Red\\",\\"Blue\\"]}"}\n'
    ok, _ = grade_tool_calls(ltele2, {"name": "f", "args": {"xs": [["red", "blue"]]}})
    assert ok
    ok, _ = grade_tool_calls(ltele2, {"name": "f", "args": {"xs": [["blue", "red"]]}})
    assert not ok                                    # order matters (BFCL AST)
    # nested dict: recursive, and an unexpected key fails.
    dtele = '{"event":"tool.call","tool":"f","args":"{\\"loc\\":{\\"city\\":\\"Paris\\"}}"}\n'
    ok, _ = grade_tool_calls(dtele, {"name": "f", "args": {"loc": [{"city": ["paris"]}]}})
    assert ok

    # outcome / state grading (τ²-bench shape)
    final = {"orders": {"o1": {"status": "cancelled", "total": 42}}, "log": ["a", "b"]}
    ok, _ = grade_state(final, {"orders": {"o1": {"status": "cancelled"}}})
    assert ok                                        # partial subset matches
    ok, _ = grade_state(final, {"orders": {"o1": {"status": "shipped"}}})
    assert not ok                                    # wrong value
    ok, _ = grade_state(final, {"orders": {"o2": {}}})
    assert not ok                                    # missing key
    ok, _ = grade_state(final, {"log": ["a", "b"]})
    assert ok
    ok, _ = grade_state(final, {"log": ["a"]})
    assert not ok                                    # list length must match

    # policy-refusal grading: a forbidden action must be absent
    assert grade_state_absent({"refunded": "A2"}, ["cancelled"])[0]      # cancel not taken -> ok
    assert not grade_state_absent({"cancelled": "A2"}, ["cancelled"])[0]  # cancel taken -> fail
    # combined env grading (state AND state_absent)
    assert grade_env({"cancelled": "A1"}, {"state": {"cancelled": "A1"}, "state_absent": ["refunded"]})[0]
    assert not grade_env({"cancelled": "A1", "refunded": "A1"},
                         {"state": {"cancelled": "A1"}, "state_absent": ["refunded"]})[0]
    print("graders: all self-checks passed")


if __name__ == "__main__":
    _selftest()
