#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""BFCL -> agentd bench task converter (RFC 0024 Phase 1).

Turns Berkeley Function-Calling Leaderboard entries into runner tasks: each
function becomes a tool served by the generic MCP stub (bench/mcp_stub.py), the
question becomes the instruction, and the ground-truth call becomes a
`grade.tool_calls` matcher. Because agentd exposes MCP tools to the model by
their verbatim catalogue name, BFCL function names map straight through.

Real usage (you supply a model endpoint + the BFCL data):

    python3 bench/bfcl.py \
        --questions BFCL_v3_simple.json \
        --answers possible_answer/BFCL_v3_simple.json \
        --intelligence https://gateway.example/v1 --model claude-opus-4-8 \
        --out bench/tasks/bfcl_simple.jsonl
    AGENT_INTELLIGENCE_TOKEN=sk-... python3 bench/run.py \
        --tasks bench/tasks/bfcl_simple.jsonl --repeats 5

Both inputs are JSON or JSON-lines, joined on `id`. This covers BFCL's
single/parallel *AST* categories (name + argument match, with several acceptable
values per param and optional params); executable / multi-turn categories want
BFCL's own runtime and are out of scope for this converter.

Dependency-free (Python 3 stdlib).
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def _load(path: str) -> list[dict]:
    text = Path(path).read_text().strip()
    if not text:
        return []
    if text[0] == "[":                       # a JSON array
        return json.loads(text)
    return [json.loads(l) for l in text.splitlines() if l.strip()]  # JSON-lines


def _instruction(question) -> str:
    """BFCL `question` is a list of turns (each a list of role/content msgs), or a
    flat message list. Concatenate the user turns."""
    msgs = []
    def walk(x):
        if isinstance(x, dict):
            if x.get("role") in (None, "user") and "content" in x:
                msgs.append(str(x["content"]))
        elif isinstance(x, list):
            for y in x:
                walk(y)
    walk(question)
    return "\n".join(msgs) if msgs else str(question)


# BFCL uses Python-ish type names in its schemas; providers want JSON Schema.
_TYPE_MAP = {"dict": "object", "float": "number", "tuple": "array", "any": "string"}


def _norm_schema(s):
    """Recursively normalize a BFCL parameter schema to JSON Schema (dict→object,
    float→number, tuple→array) so OpenAI/Anthropic accept it."""
    if not isinstance(s, dict):
        return s
    out = dict(s)
    t = out.get("type")
    if isinstance(t, str):
        out["type"] = _TYPE_MAP.get(t, t)
    if isinstance(out.get("properties"), dict):
        out["properties"] = {k: _norm_schema(v) for k, v in out["properties"].items()}
    if "items" in out:
        out["items"] = _norm_schema(out["items"])
    return out


def _tools(functions: list[dict]) -> list[dict]:
    return [{
        "name": f["name"],
        "description": f.get("description", ""),
        "inputSchema": _norm_schema(f.get("parameters", {"type": "object"})),
    } for f in functions]


def _expected(ground_truth: list[dict]):
    """BFCL ground truth: [{func_name: {param: [acceptable values], ...}}, ...].
    A param whose acceptable set includes "" (BFCL's omittable marker) is optional
    and not required. Single call -> one matcher; multiple -> an `all` sequence."""
    calls = []
    for entry in ground_truth:
        for func_name, params in entry.items():
            args = {}
            for param, accepted in (params or {}).items():
                vals = accepted if isinstance(accepted, list) else [accepted]
                if "" in vals or None in vals:       # optional param — don't require
                    continue
                args[param] = vals
            calls.append({"name": func_name, "args": args})
    if len(calls) == 1:
        return calls[0]
    return {"all": calls}


def convert(entry: dict, answer: dict, intelligence: str, model: str | None) -> dict:
    task = {
        "id": entry["id"],
        "instruction": _instruction(entry.get("question", "")),
        "intelligence": intelligence,
        "tool_server": {"name": "bfcl", "tools": _tools(entry.get("function", []))},
        "grade": {"tool_calls": _expected(answer.get("ground_truth", []))},
    }
    if model:
        task["model"] = model
    return task


def main() -> int:
    ap = argparse.ArgumentParser(description="BFCL -> agentd bench tasks (RFC 0024)")
    ap.add_argument("--questions", required=True)
    ap.add_argument("--answers", required=True)
    ap.add_argument("--intelligence", required=True, help="model endpoint URL")
    ap.add_argument("--model", default=None)
    ap.add_argument("--out", default="-", help="output .jsonl (default: stdout)")
    args = ap.parse_args()

    answers = {a["id"]: a for a in _load(args.answers)}
    tasks = []
    for e in _load(args.questions):
        ans = answers.get(e["id"])
        if ans is None:
            print(f"skip {e['id']}: no ground truth", file=sys.stderr)
            continue
        tasks.append(convert(e, ans, args.intelligence, args.model))

    out = "\n".join(json.dumps(t) for t in tasks) + "\n"
    if args.out == "-":
        sys.stdout.write(out)
    else:
        Path(args.out).write_text(out)
        print(f"wrote {len(tasks)} tasks -> {args.out}", file=sys.stderr)
    return 0


# --- self-check (run: python3 bench/bfcl.py --selftest) ------------------------

def _selftest() -> None:
    entry = {
        "id": "simple_0",
        "question": [[{"role": "user", "content": "What is the area of a triangle base 10 height 5?"}]],
        "function": [{
            "name": "calc_area",
            "description": "area of a triangle",
            "parameters": {"type": "object",
                           "properties": {"base": {"type": "integer"},
                                          "height": {"type": "integer"},
                                          "unit": {"type": "string"}},
                           "required": ["base", "height"]},
        }],
    }
    answer = {"id": "simple_0",
              "ground_truth": [{"calc_area": {"base": [10], "height": [5], "unit": ["cm", ""]}}]}
    t = convert(entry, answer, "https://gw/v1", "some-model")
    assert t["id"] == "simple_0"
    assert "area of a triangle" in t["instruction"]
    assert t["tool_server"]["tools"][0]["name"] == "calc_area"
    assert t["tool_server"]["tools"][0]["inputSchema"]["required"] == ["base", "height"]
    exp = t["grade"]["tool_calls"]
    assert exp["name"] == "calc_area"
    assert exp["args"]["base"] == [10] and exp["args"]["height"] == [5]
    assert "unit" not in exp["args"]          # optional ("" acceptable) -> dropped
    assert t["model"] == "some-model" and t["intelligence"] == "https://gw/v1"

    # Parallel (multi-call) ground truth -> an `all` sequence.
    multi = {"id": "x", "ground_truth": [{"a": {}}, {"b": {"p": [1]}}]}
    got = _expected(multi["ground_truth"])
    assert "all" in got and len(got["all"]) == 2
    print("bfcl: all self-checks passed")


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        _selftest()
    else:
        sys.exit(main())
