#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""A generic, configurable **tool-bridge** MCP server (RFC 0024 §6).

The reusable piece the whole benchmark orchestration rests on: because agentd is
MCP-native, standing up a benchmark environment is "serve the right tools over
MCP," not "write a new harness." This stub exposes an arbitrary tool set (from a
JSON spec) over the Streamable-HTTP transport agentd's client speaks, and returns
scripted (or echo) results — so a benchmark that provides its own functions
(BFCL) or a stubbed environment (MCP-Universe offline) is a data file, not code.

Speaks the minimal wire agentd expects (mirrors crates/agentd/src/mcp/mock_http.rs):
  * era probe: any unknown method (incl. `server/discover`) -> JSON-RPC -32601,
    so the client falls back to the legacy `initialize` handshake;
  * `initialize` -> capabilities {tools:{}} + serverInfo, stamping Mcp-Session-Id;
  * `notifications/initialized` (a POST notification) -> 202, no body;
  * `tools/list` -> the configured tools;
  * `tools/call` -> the tool's canned `result`, else an echo of its arguments;
  * `ping` -> {};  `GET` -> a held-open text/event-stream (no pushes).

Launch:  python3 bench/mcp_stub.py --addr-file <path> --tools <tools.json>
  tools.json: [{"name","description","inputSchema", "result"?}, ...]
It binds 127.0.0.1:0 and announces host:port to <addr-file> (same handshake as
agentd's built-in mocks), so bench/run.py can wire `--mcp name=http://<addr>`.

Dependency-free: Python 3 standard library only.
"""

from __future__ import annotations

import argparse
import json
import socket
import subprocess
import sys
import threading
from pathlib import Path

PROTOCOL_VERSION = "2025-06-18"
METHOD_NOT_FOUND = -32601


def _read_http(conn: socket.socket) -> tuple[str, bytes] | None:
    """Read one HTTP message -> (request-line, body). Minimal, Content-Length only."""
    conn.settimeout(30)
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = conn.recv(4096)
        if not chunk:
            return None
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    lines = head.decode("latin1").split("\r\n")
    request_line = lines[0]
    clen = 0
    for h in lines[1:]:
        k, _, v = h.partition(":")
        if k.strip().lower() == "content-length":
            clen = int(v.strip() or 0)
    body = rest
    while len(body) < clen:
        chunk = conn.recv(4096)
        if not chunk:
            break
        body += chunk
    return request_line, body[:clen]


def _write_json(conn: socket.socket, payload: dict, session: bool) -> None:
    body = json.dumps(payload).encode()
    head = (
        "HTTP/1.1 200 OK\r\n"
        "Content-Type: application/json\r\n"
        + ("Mcp-Session-Id: bench-stub\r\n" if session else "")
        + f"Content-Length: {len(body)}\r\n"
        "Connection: close\r\n\r\n"
    ).encode()
    conn.sendall(head + body)


def _ok(rid, result: dict) -> dict:
    return {"jsonrpc": "2.0", "id": rid, "result": result}


def _err(rid, code: int, message: str) -> dict:
    return {"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": message}}


def _dig(state: dict, dotted: str) -> tuple[dict, str]:
    """Walk `a.b.c` creating dicts, returning (parent, leaf-key)."""
    cur = state
    parts = dotted.split(".")
    for p in parts[:-1]:
        nxt = cur.get(p)
        if not isinstance(nxt, dict):
            nxt = {}
            cur[p] = nxt
        cur = nxt
    return cur, parts[-1]


def _get(state, dotted: str):
    cur = state
    for p in dotted.split("."):
        if not isinstance(cur, dict) or p not in cur:
            return None
        cur = cur[p]
    return cur


class Stub:
    """A stateful tool-bridge. Beyond serving canned tools, a tool may declare:

    * an `effect` over a shared JSON `state` — the τ²-bench / MCP-Universe shape,
      where write-actions mutate an environment graded by its final state:
        - {"set": "orders.o1.status" [, "value_arg": "status"]}  -> store at a path;
        - {"append": "cart.items" [, "value_arg": "item"]}       -> append to a list;
        - {"return": "orders.o1"}                                -> read a value out.
      The full state is written to `state_file` after each mutation for grading.

    * a `builtin` handler over a sandbox working directory (`workdir`) — the
      SWE-bench / Terminal-Bench shape, where the agent runs commands and edits
      files, graded by the resulting filesystem / a check command:
        - {"builtin": "bash"}        -> run `arguments.command` in the workdir;
        - {"builtin": "read_file"}   -> read `arguments.path`;
        - {"builtin": "write_file"}  -> write `arguments.path` = `arguments.content`.
      Builtins require `--workdir` (the sandbox); intended to run in a per-task
      throwaway dir (a container in a real run). read/write are confined to it.
    """

    def __init__(self, tools: list[dict], state_file: str | None = None,
                 workdir: str | None = None):
        # Tool list as advertised on tools/list (name/description/inputSchema).
        self.tools = [
            {"name": t["name"],
             "description": t.get("description", ""),
             "inputSchema": t.get("inputSchema", {"type": "object"})}
            for t in tools
        ]
        # name -> canned result (echoed if absent), state effect, or builtin.
        self.results = {t["name"]: t.get("result") for t in tools}
        self.effects = {t["name"]: t.get("effect") for t in tools if t.get("effect")}
        self.builtins = {t["name"]: t["builtin"] for t in tools if t.get("builtin")}
        self.state_file = state_file
        self.workdir = Path(workdir).resolve() if workdir else None
        self.lock = threading.Lock()
        # Seed the environment from the state file (the runner pre-populates it
        # with the task's initial state), then own it in memory.
        self.state = {}
        if state_file and Path(state_file).exists():
            try:
                self.state = json.loads(Path(state_file).read_text() or "{}")
            except json.JSONDecodeError:
                self.state = {}

    def _run_builtin(self, kind: str, args: dict) -> dict:
        """Sandboxed exec / file ops over `workdir`. Never raises — errors are
        returned as data so the model can adapt (MCP `isError` stays false)."""
        if self.workdir is None:
            return {"error": "builtin tools require the bridge's --workdir sandbox"}

        def _confined(rel: str) -> Path | None:
            p = (self.workdir / rel).resolve()
            root = str(self.workdir)
            return p if str(p) == root or str(p).startswith(root + "/") else None

        if kind == "bash":
            cmd = args.get("command") or args.get("cmd") or ""
            try:
                cp = subprocess.run(cmd, shell=True, cwd=self.workdir,
                                    capture_output=True, text=True, timeout=120)
                return {"stdout": cp.stdout, "stderr": cp.stderr, "exit": cp.returncode}
            except subprocess.TimeoutExpired:
                return {"stdout": "", "stderr": "timed out", "exit": 124}
        if kind == "read_file":
            p = _confined(args.get("path", ""))
            if p is None:
                return {"error": "path escapes the sandbox"}
            return {"content": p.read_text()} if p.exists() else {"error": "no such file"}
        if kind == "write_file":
            p = _confined(args.get("path", ""))
            if p is None:
                return {"error": "path escapes the sandbox"}
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(args.get("content", ""))
            return {"ok": True, "path": args.get("path")}
        return {"error": f"unknown builtin: {kind}"}

    def _apply_effect(self, name: str, args: dict):
        """Mutate/read state per the tool's effect; return an explicit read
        payload or None. Caller holds no lock; we take it here."""
        eff = self.effects.get(name)
        if not eff:
            return None
        with self.lock:
            value = args.get(eff["value_arg"]) if "value_arg" in eff else args
            read = None
            if "set" in eff:
                parent, key = _dig(self.state, eff["set"])
                parent[key] = value
            elif "append" in eff:
                parent, key = _dig(self.state, eff["append"])
                lst = parent.get(key)
                if not isinstance(lst, list):
                    lst = []
                    parent[key] = lst
                lst.append(value)
            elif "return" in eff:
                read = _get(self.state, eff["return"])
            if self.state_file:
                Path(self.state_file).write_text(json.dumps(self.state))
            return read

    def dispatch(self, req: dict) -> tuple[dict, bool]:
        method = req.get("method", "")
        rid = req.get("id")
        if method == "initialize":
            return _ok(rid, {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "bench-mcp-stub", "version": "0"},
            }), True
        if method == "ping":
            return _ok(rid, {}), False
        if method == "tools/list":
            return _ok(rid, {"tools": self.tools}), False
        if method == "tools/call":
            params = req.get("params") or {}
            name = params.get("name")
            args = params.get("arguments") or {}
            if name in self.builtins:                  # sandboxed exec / file ops
                payload = self._run_builtin(self.builtins[name], args)
            else:
                read = self._apply_effect(name, args)  # stateful mutate/read
                if read is not None:
                    payload = read
                elif self.results.get(name) is not None:
                    payload = self.results[name]
                else:
                    payload = {"ok": True, "tool": name, "arguments": args}
            return _ok(rid, {
                "content": [{"type": "text", "text": json.dumps(payload)}],
                "isError": False,
            }), False
        # Unknown (incl. `server/discover`) -> era-probe fallback signal.
        return _err(rid, METHOD_NOT_FOUND, f"unsupported: {method}"), False

    def handle(self, conn: socket.socket) -> None:
        try:
            got = _read_http(conn)
            if not got:
                return
            request_line, body = got
            if request_line.startswith("GET "):
                # The notification SSE stream: open it and hold it (no pushes).
                conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n"
                             b"Connection: close\r\n\r\n")
                try:
                    while conn.recv(1024):
                        pass
                except OSError:
                    pass
                return
            try:
                req = json.loads(body or b"{}")
            except json.JSONDecodeError:
                req = {}
            # A notification (no id) e.g. notifications/initialized -> 202.
            if "id" not in req:
                conn.sendall(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n"
                             b"Connection: close\r\n\r\n")
                return
            resp, session = self.dispatch(req)
            _write_json(conn, resp, session)
        finally:
            try:
                conn.close()
            except OSError:
                pass


def serve(addr_file: str, tools: list[dict], state_file: str | None = None,
          workdir: str | None = None) -> None:
    stub = Stub(tools, state_file, workdir)
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(64)
    host, port = srv.getsockname()
    Path(addr_file).write_text(f"{host}:{port}")
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=stub.handle, args=(conn,), daemon=True).start()


def main() -> int:
    ap = argparse.ArgumentParser(description="generic tool-bridge MCP stub (RFC 0024 §6)")
    ap.add_argument("--addr-file", required=True, help="write the bound host:port here")
    ap.add_argument("--tools", required=True, help="JSON file: [{name, description, inputSchema, result?, effect?}]")
    ap.add_argument("--state-file", default=None,
                    help="stateful env: seed from + persist the JSON environment state here")
    ap.add_argument("--workdir", default=None,
                    help="sandbox dir for `builtin` bash/file tools (SWE-bench shape)")
    args = ap.parse_args()
    tools = json.loads(Path(args.tools).read_text())
    serve(args.addr_file, tools, args.state_file, args.workdir)
    return 0


# --- self-checks (run: python3 bench/mcp_stub.py --selftest) -------------------

def _selftest() -> None:
    import tempfile
    with tempfile.TemporaryDirectory() as td:
        tools = [{"name": "bash", "builtin": "bash"},
                 {"name": "read_file", "builtin": "read_file"},
                 {"name": "write_file", "builtin": "write_file"}]
        stub = Stub(tools, workdir=td)

        def call(name, args):
            resp, _ = stub.dispatch({"id": 1, "method": "tools/call",
                                     "params": {"name": name, "arguments": args}})
            return json.loads(resp["result"]["content"][0]["text"])

        # bash runs in the sandbox and reports exit/stdout.
        r = call("bash", {"command": "echo pong > out.txt && echo done"})
        assert r["exit"] == 0 and "done" in r["stdout"], r
        # read_file sees what bash wrote.
        assert call("read_file", {"path": "out.txt"})["content"].strip() == "pong"
        # write_file then bash observes it.
        call("write_file", {"path": "sub/f.txt", "content": "hi"})
        assert call("bash", {"command": "cat sub/f.txt"})["stdout"].strip() == "hi"
        # path escape is refused.
        assert "error" in call("read_file", {"path": "../../etc/passwd"})
        # a bad command reports non-zero exit, not a crash.
        assert call("bash", {"command": "exit 3"})["exit"] == 3
        # builtins without a workdir are refused (explicit sandbox).
        assert "error" in Stub(tools)._run_builtin("bash", {"command": "echo x"})
    print("mcp_stub: all self-checks passed")


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        _selftest()
    else:
        sys.exit(main())
