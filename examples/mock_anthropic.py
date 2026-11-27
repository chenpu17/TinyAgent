#!/usr/bin/env python3
"""Minimal Anthropic /v1/messages SSE mock for testing tiny-agent's
Anthropic provider (request serialization + SSE stream parsing).

Behavior:
  - Request without tool_result blocks → respond with a text block +
    a tool_use block (write_file mock_anthropic.txt), streamed as
    proper content_block_start/delta/stop events (input_json_delta
    split into two chunks to exercise reassembly).
  - Request containing a tool_result → stream a final text answer.
  - `--script FILE`: scripted mode — the mock follows a fixed per-request
    script instead (format: examples/mock_script.py), reports usage, and
    answers compression summary requests with a fixed summary. A turn's
    "reasoning" is streamed as a signed thinking block.

Threaded (ThreadingHTTPServer) so concurrent batch workers (--jobs N) are
served in parallel instead of serialized behind a single connection.

Usage: python3 mock_anthropic.py [port] [--script FILE]   (default port 8793)
       Port 0 picks a free port; the port actually bound is printed on stderr.
"""
import hashlib
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.dont_write_bytecode = True  # mock_script 是 import 进来的：不往 examples/ 落 __pycache__

SCRIPT = None


def parse_argv(argv):
    """[port] [--script FILE] → (port, script). Anything else is a usage error."""
    port, script, i = 8793, None, 0
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


# ── scripted mode ────────────────────────────────────────────────────


def scripted_turn(body):
    """The SSE text for one request under a script (usage included)."""
    import mock_script

    if mock_script.is_summary(body):
        turn, req = {"text": mock_script.SUMMARY_TEXT}, None
    else:
        turn, req = SCRIPT.next_turn(), SCRIPT.count
    calls = turn.get("tools") or []
    text, reasoning = turn.get("text", ""), turn.get("reasoning", "")
    usage = mock_script.usage_for(body, reply_chars=len(text), n_calls=len(calls),
                                  reasoning_chars=len(reasoning), override=turn.get("usage"))

    # Anthropic's `input_tokens` is the uncached share of the prompt; cache
    # reads are their own axis (the kernel keeps that invariant on Usage).
    events = [("message_start", {"type": "message_start", "message": {
        "id": f"msg_{req}", "role": "assistant",
        "usage": {"input_tokens": usage["prompt"] - usage["cached"],
                  "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": usage["cached"],
                  "output_tokens": 1}}})]

    def block(index, start, deltas):
        events.append(("content_block_start", {"type": "content_block_start", "index": index,
                                               "content_block": start}))
        for delta in deltas:
            events.append(("content_block_delta", {"type": "content_block_delta", "index": index,
                                                   "delta": delta}))
        events.append(("content_block_stop", {"type": "content_block_stop", "index": index}))

    index = 0
    if reasoning:
        # Signed thinking block: the kernel keeps the signature and (under
        # preserved_reasoning) echoes the block back with the next request.
        sig = "sig-" + hashlib.sha256(f"{req}:{index}".encode()).hexdigest()[:24]
        block(index, {"type": "thinking", "thinking": ""},
              [{"type": "thinking_delta", "thinking": reasoning},
               {"type": "signature_delta", "signature": sig}])
        index += 1
    if text:
        block(index, {"type": "text", "text": ""}, [{"type": "text_delta", "text": text}])
        index += 1
    for call in calls:
        args = json.dumps(call["arguments"])
        half = max(1, len(args) // 2)  # two input_json_delta pieces: exercise reassembly
        block(index, {"type": "tool_use", "id": f"toolu_{req}_{index}", "name": call["name"],
                      "input": {}},
              [{"type": "input_json_delta", "partial_json": args[:half]},
               {"type": "input_json_delta", "partial_json": args[half:]}])
        index += 1
    events.append(("message_delta", {"type": "message_delta",
                                     "delta": {"stop_reason": "tool_use" if calls else "end_turn"},
                                     "usage": {"output_tokens": usage["completion"]}}))
    events.append(("message_stop", {"type": "message_stop"}))
    return sse_lines(events)


def shape_problem(body, headers):
    """Why the request is not shaped the way tiny-agent's Anthropic provider
    sends one (None when it is). Answered as HTTP 400 with the reason, so a
    serialization regression shows up as a readable failure instead of a
    dropped connection."""
    try:
        if body.get("stream") is not True:
            return '"stream" is not true'
        if headers.get("x-api-key") is None:
            return "no x-api-key header"
        if headers.get("anthropic-version") != "2023-06-01":
            return f"anthropic-version is {headers.get('anthropic-version')!r}"
        first = body["messages"][0]["content"][0]
        if first.get("type") != "text":
            return f"the first message's first block is {first.get('type')!r}, not a text block"
    except (KeyError, IndexError, TypeError, AttributeError) as e:
        return f"malformed request: {e!r}"
    return None


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v1/messages":
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        problem = shape_problem(body, self.headers)
        if problem:
            payload = json.dumps({"type": "error", "error": {
                "type": "invalid_request_error",
                "message": f"mock_anthropic: {problem}"}}).encode()
            self.send_response(400)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if SCRIPT is None:
            payload = (final_turn() if has_tool_result(body) else tool_turn()).encode()
        else:
            payload = scripted_turn(body).encode()
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
    print(f"mock Anthropic server on http://127.0.0.1:{server.server_address[1]}",
          file=sys.stderr, flush=True)
    server.serve_forever()
