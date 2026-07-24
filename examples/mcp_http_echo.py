#!/usr/bin/env python3
"""MCP test server implementing BOTH HTTP transports, for tiny-agent e2e tests.

  POST /mcp       — Streamable HTTP (2025-03-26): JSON-RPC in, JSON response out
                    (notifications get bare 202). Sets Mcp-Session-Id on initialize.
  GET  /sse       — legacy HTTP+SSE (2024-11-05): long-lived event stream;
                    first event is `endpoint`, JSON-RPC responses flow as `message` events.
  POST /messages  — legacy counterpart: accepts messages, replies 202.

Exposes one tool: `echo`. Also a protocol reference implementation.

Usage: python3 mcp_http_echo.py [port]   (default 8792)
"""
import json
import queue
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8792
PROTOCOL_VERSION = "2025-03-26"

TOOLS = [{
    "name": "echo",
    "description": "Echo back the given text.",
    "inputSchema": {
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
    },
}]


def handle_rpc(msg):
    """Pure JSON-RPC handler shared by both transports. Returns result or None."""
    method = msg.get("method", "")
    if method == "initialize":
        return {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mcp-http-echo", "version": "0.1.0"},
        }
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        text = msg.get("params", {}).get("arguments", {}).get("text", "")
        return {"content": [{"type": "text", "text": f"echo: {text}"}], "isError": False}
    return None


# Per-connection queues for legacy SSE response delivery.
sse_clients = []
sse_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _body(self):
        return json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))

    def do_POST(self):
        if self.path == "/mcp":
            self._streamable()
        elif self.path.startswith("/messages"):
            self._legacy_post()
        else:
            self.send_error(404)

    def do_GET(self):
        if self.path == "/sse":
            self._legacy_sse()
        elif self.path == "/mcp":
            # Streamable HTTP servers may reject GET; we do (spec-compliant).
            self.send_error(405)
        else:
            self.send_error(404)

    # ── Streamable HTTP ──────────────────────────────────────────────
    def _streamable(self):
        msg = self._body()
        if "id" not in msg:  # notification
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        result = handle_rpc(msg)
        payload = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}).encode()
        self.send_response(200)
        if msg.get("method") == "initialize":
            self.send_header("Mcp-Session-Id", "test-session-1")
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    # ── legacy HTTP+SSE ──────────────────────────────────────────────
    def _legacy_sse(self):
        q = queue.Queue()
        with sse_lock:
            sse_clients.append(q)
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "keep-alive")
            self.end_headers()
            self.wfile.write(b"event: endpoint\ndata: /messages\n\n")
            self.wfile.flush()
            while True:
                data = q.get()  # blocks; response push from POST /messages
                self.wfile.write(f"event: message\ndata: {data}\n\n".encode())
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with sse_lock:
                sse_clients.remove(q)

    def _legacy_post(self):
        msg = self._body()
        if "id" in msg:
            result = handle_rpc(msg)
            data = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result})
            with sse_lock:
                for q in sse_clients:
                    q.put(data)
        self.send_response(202)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    print(f"MCP http-echo on http://127.0.0.1:{PORT} (/mcp + /sse)", file=sys.stderr)
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
