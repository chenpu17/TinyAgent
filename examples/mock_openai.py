#!/usr/bin/env python3
"""Minimal OpenAI-compatible SSE mock for testing tiny-agent end-to-end.

Behavior:
  - First request (no `tool` role message): asks the agent to run `write_file`
    on mock_output.txt.
  - Second request (has a `tool` message): streams a final text answer.

Usage: python3 mock_openai.py [port]   (default 8791)
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8791


def sse(chunks):
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"


def tool_call_response():
    return sse([
        {"choices": [{"index": 0, "finish_reason": None, "delta": {
            "role": "assistant",
            "tool_calls": [{"index": 0, "id": "call_mock_1", "type": "function",
                            "function": {"name": "write_file", "arguments": ""}}]}}]},
        {"choices": [{"index": 0, "finish_reason": None, "delta": {
            "tool_calls": [{"index": 0, "function": {
                "arguments": json.dumps({"path": "mock_output.txt", "content": "mock ok\n"})}}]}}]},
        {"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {}}]},
    ])


def final_response():
    return sse([
        {"choices": [{"index": 0, "finish_reason": None, "delta": {
            "role": "assistant", "content": "Task complete: wrote mock_output.txt."}}]},
        {"choices": [{"index": 0, "finish_reason": "stop", "delta": {}}]},
    ])


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/chat/completions":
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        has_tool_msg = any(m.get("role") == "tool" for m in body["messages"])
        payload = (final_response() if has_tool_msg else tool_call_response()).encode()

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    print(f"mock OpenAI server on http://127.0.0.1:{PORT}", file=sys.stderr)
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
