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


class Stub:
    def __init__(self, tools: list[dict]):
        # Tool list as advertised on tools/list (name/description/inputSchema).
        self.tools = [
            {"name": t["name"],
             "description": t.get("description", ""),
             "inputSchema": t.get("inputSchema", {"type": "object"})}
            for t in tools
        ]
        # name -> canned result (echoed if absent).
        self.results = {t["name"]: t.get("result") for t in tools}

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
            canned = self.results.get(name)
            payload = canned if canned is not None else {"ok": True, "tool": name, "arguments": args}
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


def serve(addr_file: str, tools: list[dict]) -> None:
    stub = Stub(tools)
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
    ap.add_argument("--tools", required=True, help="JSON file: [{name, description, inputSchema, result?}]")
    args = ap.parse_args()
    tools = json.loads(Path(args.tools).read_text())
    serve(args.addr_file, tools)
    return 0


if __name__ == "__main__":
    sys.exit(main())
