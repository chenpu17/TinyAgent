#!/usr/bin/env python3
"""Minimal OpenAI-compatible SSE mock for testing tiny-agent end-to-end.

Behavior:
  - First request (no `tool` role message): asks the agent to run `write_file`
    on mock_output.txt.
  - Second request (has a `tool` message): streams a final text answer.
  - `--script FILE`: scripted mode — the mock follows a fixed per-request
    script instead (format: examples/mock_script.py), reports a usage chunk,
    and answers compression summary requests with a fixed summary.

Threaded (ThreadingHTTPServer) so concurrent batch workers (--jobs N) are
served in parallel instead of serialized behind a single connection — a
single-threaded mock silently masks concurrency bugs.

Usage: python3 mock_openai.py [port] [--script FILE]   (default port 8791)
       Port 0 picks a free port; the port actually bound is printed on stderr.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.dont_write_bytecode = True  # mock_script 是 import 进来的：不往 examples/ 落 __pycache__

SCRIPT = None


def parse_argv(argv):
    """[port] [--script FILE] → (port, script). Anything else is a usage error."""
    port, script, i = 8791, None, 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--script" and i + 1 < len(argv):
            script, i = argv[i + 1], i + 2
        elif arg.startswith("--script="):
            script, i = arg[len("--script="):], i + 1
        elif arg.isdigit():
            port, i = int(arg), i + 1
        else:
            sys.exit(f"usage: {sys.argv[0]} [port] [--script FILE]")
    return port, script


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


# ── scripted mode ────────────────────────────────────────────────────


def scripted_response(body):
    """The SSE chunks for one request under a script (incl. the usage chunk)."""
    import mock_script

    if mock_script.is_summary(body):
        turn, req = {"text": mock_script.SUMMARY_TEXT}, None
    else:
        turn, req = SCRIPT.next_turn(), SCRIPT.count
    calls = turn.get("tools") or []
    text, reasoning = turn.get("text", ""), turn.get("reasoning", "")
    usage = mock_script.usage_for(body, reply_chars=len(text), n_calls=len(calls),
                                  reasoning_chars=len(reasoning), override=turn.get("usage"))

    delta = lambda **d: {"choices": [{"index": 0, "finish_reason": None, "delta": d}]}  # noqa: E731
    chunks = [delta(role="assistant", content="")]
    if reasoning:
        chunks.append(delta(reasoning_content=reasoning))
    if text:
        chunks.append(delta(content=text))
    for i, call in enumerate(calls):
        args = json.dumps(call["arguments"])
        half = max(1, len(args) // 2)  # two argument deltas: exercise reassembly
        chunks.append(delta(tool_calls=[{"index": i, "id": f"call_{req}_{i}", "type": "function",
                                         "function": {"name": call["name"], "arguments": ""}}]))
        for piece in (args[:half], args[half:]):
            chunks.append(delta(tool_calls=[{"index": i, "function": {"arguments": piece}}]))
    chunks.append({"choices": [{"index": 0, "finish_reason": "tool_calls" if calls else "stop",
                                "delta": {}}]})
    chunks.append({"choices": [], "usage": {
        "prompt_tokens": usage["prompt"],
        "completion_tokens": usage["completion"],
        "total_tokens": usage["prompt"] + usage["completion"],
        "prompt_tokens_details": {"cached_tokens": usage["cached"]},
        "completion_tokens_details": {"reasoning_tokens": usage["reasoning"]},
    }})
    return chunks


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/chat/completions":
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if SCRIPT is None:
            has_tool_msg = any(m.get("role") == "tool" for m in body["messages"])
            payload = (final_response() if has_tool_msg else tool_call_response()).encode()
        else:
            payload = sse(scripted_response(body)).encode()

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    PORT, script = parse_argv(sys.argv[1:])
    if script:
        import mock_script
        SCRIPT = mock_script.load_script(script)
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"mock OpenAI server on http://127.0.0.1:{server.server_address[1]}",
          file=sys.stderr, flush=True)
    server.serve_forever()
