#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""
tail-server.py — the file half of the line-processing example.

agentd runs no local I/O, so it cannot tail a file. This server owns the file
and hands agentd LINES, which is the same split the voice example makes with
audio: the edge owns the device, the daemon owns what happens after.

    /mcp/lines   resource `file://<path>` (subscribable) + tools

Four things this has to get right, and they are the reason a tailer is not
three lines of `tail -f`:

  1. **A partial last line is not a line.** A writer appending a CSV row is not
     atomic; delivering `alice,42` before the newline arrives means processing
     half a record. Bytes after the final `\n` are held back until it lands.
  2. **The offset is a BYTE offset, not a line count.** Lines vary in length
     and a restart has to resume mid-file without re-reading it.
  3. **Rotation and truncation.** If the inode changes or the file shrinks
     below the offset, the file was rotated or rewritten: reset to zero rather
     than tailing a file nobody writes to any more, or seeking past the end.
  4. **Who holds the offset.** Both work, and the example shows both: the
     server can remember it (`read_new`), or the workflow can pass one in
     (`read_since`) and keep it in agentd's durable memory. The second survives
     a restart of THIS process too, which is the honest default when the
     server is a sidecar that may be redeployed.

    python3 tail-server.py --watch /data/inbox.csv

Standard library only.
"""

from __future__ import annotations

import argparse
import csv
import io
import json
import os
import queue
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PROTOCOL_VERSION = "2025-11-25"
SUPPORTED = {PROTOCOL_VERSION, "2025-06-18", "2025-03-26", "2024-11-05"}

METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
RESOURCE_NOT_FOUND = -32002


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", file=sys.stderr, flush=True)


class Tail:
    """One watched file: its identity, its offset, and how to read forward."""

    def __init__(self, path: str, poll_ms: int):
        self.path = os.path.abspath(path)
        self.uri = f"file://{self.path}"
        self.poll = poll_ms / 1000.0
        self.lock = threading.Lock()
        self.offset = 0            # the server's own cursor, for `read_new`
        self.inode: int | None = None
        self.subscribers: list[queue.Queue] = []

    # -- reading ---------------------------------------------------------

    def _stat(self):
        try:
            st = os.stat(self.path)
            return st.st_ino, st.st_size
        except FileNotFoundError:
            return None, 0

    def read_from(self, start: int) -> dict:
        """Complete lines from byte `start`. Never returns a partial tail."""
        inode, size = self._stat()
        if inode is None:
            return {"lines": [], "next_offset": 0, "eof": True, "rotated": False}
        rotated = False
        if self.inode is not None and inode != self.inode:
            rotated = True          # a new file under the same name
        if start > size:
            rotated = True          # truncated out from under us
        if rotated:
            start = 0
        with open(self.path, "rb") as f:
            f.seek(start)
            blob = f.read()
        # Hold back anything after the last newline: it is a line in progress.
        cut = blob.rfind(b"\n")
        if cut < 0:
            return {
                "lines": [],
                "next_offset": start,
                "eof": True,
                "rotated": rotated,
            }
        complete, _partial = blob[: cut + 1], blob[cut + 1 :]
        lines = complete.decode("utf-8", "replace").splitlines()
        return {
            "lines": lines,
            "next_offset": start + cut + 1,
            "eof": True,
            "rotated": rotated,
        }

    # -- watching --------------------------------------------------------

    def watch(self) -> None:
        """Poll for growth and push one notification per change.

        Polling, not inotify: this file may live on a network mount where
        inotify reports nothing, and the notification carries no payload
        anyway — agentd re-reads current state, so a missed edge costs
        latency, never data.
        """
        last = None
        while True:
            inode, size = self._stat()
            now = (inode, size)
            if now != last and inode is not None:
                if last is not None:
                    self.notify()
                self.inode = inode
                last = now
            time.sleep(self.poll)

    def notify(self) -> None:
        with self.lock:
            subs = list(self.subscribers)
        note = {
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": {"uri": self.uri},
        }
        for q in subs:
            q.put(note)
        if subs:
            log(f"changed {self.path} -> {len(subs)} subscriber(s)")


def text_result(payload) -> dict:
    return {"content": [{"type": "text", "text": json.dumps(payload)}]}


def jsonrpc_result(rid, result):
    return {"jsonrpc": "2.0", "id": rid, "result": result}


def jsonrpc_error(rid, code, message, data=None):
    err = {"code": code, "message": message}
    if data is not None:
        err["data"] = data
    return {"jsonrpc": "2.0", "id": rid, "error": err}


TOOLS = [
    {
        "name": "read_since",
        "description": (
            "Complete lines from a byte offset. Returns {lines, next_offset, "
            "rotated}. The CALLER holds the offset, so it survives a restart "
            "of this server."
        ),
        "inputSchema": {
            "type": "object",
            "required": ["after"],
            "properties": {"after": {"type": "integer", "minimum": 0}},
        },
    },
    {
        "name": "read_new",
        "description": (
            "Complete lines since this SERVER last handed any out. Simpler, "
            "but the cursor dies with this process."
        ),
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "parse_csv",
        "description": "Parse CSV lines into objects using a header row.",
        "inputSchema": {
            "type": "object",
            "required": ["lines"],
            "properties": {
                "lines": {"type": "array", "items": {"type": "string"}},
                "header": {"type": "array", "items": {"type": "string"}},
            },
        },
    },
    {
        "name": "append",
        "description": (
            "Append one line. The write goes through THIS process, which owns "
            "the file, so a workflow writing back cannot interleave with the "
            "tailer's own reads."
        ),
        "inputSchema": {
            "type": "object",
            "required": ["line"],
            "properties": {"line": {"type": "string"}},
        },
    },
]


def handle_rpc(req: dict, tail: Tail) -> tuple[dict, bool]:
    rid, method = req.get("id"), req.get("method", "")
    params = req.get("params") or {}

    if method == "initialize":
        asked = params.get("protocolVersion")
        version = asked if asked in SUPPORTED else PROTOCOL_VERSION
        return jsonrpc_result(rid, {
            "protocolVersion": version,
            # `resources.subscribe` is the load-bearing one: agentd freezes the
            # negotiated set and will never send `resources/subscribe` to a
            # server that did not advertise it — it degrades to nothing, in
            # silence.
            "capabilities": {"resources": {"subscribe": True, "listChanged": False},
                             "tools": {}},
            "serverInfo": {"name": "tail-server", "version": "1.0.0"},
        }), True

    if method == "ping":
        return jsonrpc_result(rid, {}), False

    if method == "tools/list":
        return jsonrpc_result(rid, {"tools": TOOLS}), False

    if method == "resources/list":
        return jsonrpc_result(rid, {"resources": [
            {"uri": tail.uri, "name": os.path.basename(tail.path),
             "mimeType": "text/plain"}
        ]}), False

    if method == "resources/read":
        uri = params.get("uri", "")
        if uri != tail.uri:
            return jsonrpc_error(rid, RESOURCE_NOT_FOUND,
                                 f"no such resource: {uri}", {"uri": uri}), False
        inode, size = tail._stat()
        body = json.dumps({"path": tail.path, "size": size,
                           "exists": inode is not None})
        return jsonrpc_result(rid, {"contents": [
            {"uri": tail.uri, "mimeType": "application/json", "text": body}
        ]}), False

    if method in ("resources/subscribe", "resources/unsubscribe"):
        log(f"{method.split('/')[-1]} {params.get('uri','')}")
        return jsonrpc_result(rid, {}), False

    if method == "tools/call":
        return handle_tool(rid, params, tail), False

    return jsonrpc_error(rid, METHOD_NOT_FOUND, f"unsupported: {method}"), False


def handle_tool(rid, params: dict, tail: Tail) -> dict:
    name = params.get("name", "")
    args = params.get("arguments") or {}

    if name == "read_since":
        out = tail.read_from(int(args.get("after", 0)))
        log(f"read_since after={args.get('after')} -> {len(out['lines'])} line(s)")
        return jsonrpc_result(rid, text_result(out))

    if name == "read_new":
        with tail.lock:
            out = tail.read_from(tail.offset)
            tail.offset = out["next_offset"]
        log(f"read_new -> {len(out['lines'])} line(s), cursor {tail.offset}")
        return jsonrpc_result(rid, text_result(out))

    if name == "parse_csv":
        lines = [str(x) for x in (args.get("lines") or [])]
        header = args.get("header")
        rows = list(csv.reader(io.StringIO("\n".join(lines))))
        if header is None:
            if not rows:
                return jsonrpc_result(rid, text_result({"rows": []}))
            header, rows = rows[0], rows[1:]
        out = [dict(zip(header, r)) for r in rows if r]
        return jsonrpc_result(rid, text_result({"rows": out, "header": header}))

    if name == "append":
        line = str(args.get("line", "")).rstrip("\n")
        with tail.lock, open(tail.path, "a", encoding="utf-8") as f:
            f.write(line + "\n")
        log(f"append {line!r}")
        return jsonrpc_result(rid, text_result({"appended": line}))

    return jsonrpc_error(rid, INVALID_PARAMS, f"no such tool: {name}")


# ── HTTP: Streamable MCP ────────────────────────────────────────────────────


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    tail: Tail = None  # set on the server instance

    def log_message(self, *_):  # the server does its own logging
        pass

    def _endpoint(self) -> str | None:
        path = self.path.split("?", 1)[0].rstrip("/")
        return "lines" if path == "/mcp/lines" else None

    def do_POST(self):
        endpoint = self._endpoint()
        if endpoint is None:
            return self._plain(404, "no such endpoint")
        length = int(self.headers.get("Content-Length") or 0)
        try:
            frame = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError:
            return self._plain(400, "bad json")
        # A notification (no `id`) gets 202 and no body — including
        # `notifications/initialized`, which completes the handshake.
        if "id" not in frame:
            return self._plain(202, "")
        response, session = handle_rpc(frame, self.tail)
        body = json.dumps(response).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        if session:
            self.send_header("Mcp-Session-Id", "tail")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        """The long-lived server→client SSE stream."""
        endpoint = self._endpoint()
        if endpoint is None:
            return self._plain(404, "no such endpoint")
        self.close_connection = True
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        q: queue.Queue = queue.Queue()
        with self.tail.lock:
            self.tail.subscribers.append(q)
        log("notification stream opened")
        try:
            while True:
                try:
                    note = q.get(timeout=20)
                except queue.Empty:
                    # An SSE comment keeps middleboxes from reaping an idle
                    # stream. A client that ignores comments is unaffected.
                    self.wfile.write(b": keep-alive\n\n")
                    self.wfile.flush()
                    continue
                self.wfile.write(f"data: {json.dumps(note)}\n\n".encode())
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            with self.tail.lock:
                if q in self.tail.subscribers:
                    self.tail.subscribers.remove(q)
            log("notification stream closed")

    def _plain(self, code: int, msg: str):
        body = msg.encode()
        self.send_response(code)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)




def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--watch", required=True, help="the file to tail")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8770)
    ap.add_argument("--poll-ms", type=int, default=250)
    args = ap.parse_args()

    tail = Tail(args.watch, args.poll_ms)
    os.makedirs(os.path.dirname(tail.path) or ".", exist_ok=True)
    if not os.path.exists(tail.path):
        open(tail.path, "a").close()
    tail.inode, _ = tail._stat()

    Handler.tail = tail
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    threading.Thread(target=tail.watch, daemon=True).start()
    log(f"mcp   http://{args.host}:{args.port}/mcp/lines")
    log(f"watch {tail.path}")
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        pass
    log("bye")


if __name__ == "__main__":
    main()
