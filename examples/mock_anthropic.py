#!/usr/bin/env python3
"""Minimal Anthropic /v1/messages SSE mock for testing tiny-agent's
Anthropic provider (request serialization + SSE stream parsing).

Behavior:
  - Request without tool_result blocks → respond with a text block +
    a tool_use block (write_file mock_anthropic.txt), streamed as
    proper content_block_start/delta/stop events (input_json_delta
    split into two chunks to exercise reassembly).
  - Request containing a tool_result → stream a final text answer.

Threaded (ThreadingHTTPServer) so concurrent batch workers (--jobs N) are
served in parallel instead of serialized behind a single connection.

Usage: python3 mock_anthropic.py [port]   (default 8793)
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8793


def sse_lines(events):
    out = []
    for name, payload in events:
        out.append(f"event: {name}\ndata: {json.dumps(payload)}\n\n")
    return "".join(out)


def tool_turn():
    args = json.dumps({"path": "mock_anthropic.txt", "content": "anthropic mock ok\n"})
    half = len(args) // 2
    return sse_lines([
        ("message_start", {"type": "message_start", "message": {"id": "msg_1", "role": "assistant"}}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                 "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                 "delta": {"type": "text_delta", "text": "Writing the file."}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("content_block_start", {"type": "content_block_start", "index": 1,
                                 "content_block": {"type": "tool_use", "id": "toolu_1",
                                                   "name": "write_file", "input": {}}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 1,
                                 "delta": {"type": "input_json_delta", "partial_json": args[:half]}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 1,
                                 "delta": {"type": "input_json_delta", "partial_json": args[half:]}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 1}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ("message_stop", {"type": "message_stop"}),
    ])


def final_turn():
    return sse_lines([
        ("message_start", {"type": "message_start", "message": {"id": "msg_2", "role": "assistant"}}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                 "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                 "delta": {"type": "text_delta", "text": "Done: file written and verified."}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ("message_stop", {"type": "message_stop"}),
    ])


def has_tool_result(body):
    for m in body.get("messages", []):
        content = m.get("content")
        if isinstance(content, list):
            if any(b.get("type") == "tool_result" for b in content):
                return True
    return False


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v1/messages":
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))

        # Sanity-check key parts of tiny-agent's request serialization.
        assert body["stream"] is True
        assert "x-api-key" in self.headers
        assert self.headers.get("anthropic-version") == "2023-06-01"
        first = body["messages"][0]["content"][0]
        assert first["type"] == "text"

        payload = (final_turn() if has_tool_result(body) else tool_turn()).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    print(f"mock Anthropic server on http://127.0.0.1:{PORT}", file=sys.stderr)
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
