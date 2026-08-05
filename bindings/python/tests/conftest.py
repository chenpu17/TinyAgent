"""Pytest fixtures: a configurable OpenAI-compatible SSE mock.

`examples/mock_openai.py` is intentionally rigid (hardcoded `write_file`,
no usage frame), so it can't exercise a *user-defined* tool, the cancel path,
or 4-axis usage accounting. `ScriptedMock` below is a small, in-process
`ThreadingHTTPServer` that plays back arbitrary scripted SSE responses per
request — enough to drive every binding code path offline, no API key.

One test (`test_chat_one_shot_against_project_mock`) still spawns the real
`examples/mock_openai.py` as a regression guard against the project asset.
"""
from __future__ import annotations

import json
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# Repo root (…/TinyAgent): tests/ → python/ → bindings/ → root.
REPO_ROOT = Path(__file__).resolve().parents[3]
MOCK_OPENAI = REPO_ROOT / "examples" / "mock_openai.py"


def _free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


# ── SSE response builders (returned as bytes; responders write them) ──────────

def sse_bytes(chunks: list, *, done: bool = True) -> bytes:
    out = b"".join(f"data: {json.dumps(c)}\n\n".encode() for c in chunks)
    if done:
        out += b"data: [DONE]\n\n"
    return out


def tool_call_sse(call_id: str, name: str, arguments: dict) -> bytes:
    """A streaming tool_call: name first, then arguments, then finish=tool_calls."""
    return sse_bytes([
        {"choices": [{"index": 0, "finish_reason": None, "delta": {
            "role": "assistant",
            "tool_calls": [{"index": 0, "id": call_id, "type": "function",
                            "function": {"name": name, "arguments": ""}}]}}]},
        {"choices": [{"index": 0, "finish_reason": None, "delta": {
            "tool_calls": [{"index": 0, "function": {
                "arguments": json.dumps(arguments)}}]}}]},
        {"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {}}]},
    ])


def text_sse(content: str, *, usage: dict | None = None, finish: str = "stop") -> bytes:
    """A final text answer, optionally carrying a usage frame on the finish chunk
    (DeepSeek/vLLM-style: usage attached to the last choice-bearing chunk)."""
    chunks = [
        {"choices": [{"index": 0, "finish_reason": None, "delta": {
            "role": "assistant", "content": content}}]},
        {"choices": [{"index": 0, "finish_reason": finish, "delta": {}}]},
    ]
    if usage is not None:
        chunks[-1]["usage"] = usage
    return sse_bytes(chunks)


def write_sse(handler: BaseHTTPRequestHandler, body: bytes) -> None:
    """Write a complete SSE response body with Content-Length (the shape
    examples/mock_openai.py uses, and what reqwest blocking reads cleanly)."""
    handler.send_response(200)
    handler.send_header("Content-Type", "text/event-stream")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


# ── ScriptedMock: plays back per-request responders ──────────────────────────

class ScriptedMock:
    """An OpenAI-compatible SSE mock.

    `script` is a list of responders; the i-th request gets `script[i]` (the
    last entry is reused once exhausted). A responder is `callable(handler,
    body) -> None` that writes the full HTTP response via the handler. Capture
    `mock.requests` for assertions about what the agent sent.
    """

    def __init__(self, script: list):
        self.script = script
        self.requests: list[dict] = []
        self._lock = threading.Lock()
        # Set on stop() so a "hang" responder can unblock and let its handler
        # thread exit promptly instead of lingering until process teardown.
        self._stop = threading.Event()
        self.port = _free_port()
        self._srv = ThreadingHTTPServer(("127.0.0.1", self.port), self._handler_cls())
        self._thread = threading.Thread(target=self._srv.serve_forever, daemon=True)

    def _handler_cls(self):
        mock = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self):  # noqa: N802 (BaseHTTPRequestHandler convention)
                if self.path != "/chat/completions":
                    self.send_error(404)
                    return
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length)) if length else {}
                with mock._lock:
                    mock.requests.append(body)
                    responder = mock.script[min(len(mock.requests) - 1, len(mock.script) - 1)]
                responder(self, body)

            def log_message(self, *args):  # silence stderr noise
                pass

        return H

    def start(self) -> "ScriptedMock":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._stop.set()
        self._srv.shutdown()
        self._srv.server_close()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"


# ── project mock (examples/mock_openai.py) as a subprocess ───────────────────

class ProjectMock:
    """Drives the real `examples/mock_openai.py` in a subprocess — a regression
    guard that the binding still works against the project's own mock asset."""

    def __init__(self):
        self.port = _free_port()
        self._proc = subprocess.Popen(
            [sys.executable, str(MOCK_OPENAI), str(self.port)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self._wait_ready(timeout=10.0)

    def _wait_ready(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{self.port}/", timeout=0.5)
                return  # any HTTP response means the server is listening
            except urllib.error.HTTPError:
                # mock_openai.py only handles POST; a GET yields 501 — but a 4xx/5xx
                # response still proves the server is up and accepting connections,
                # so treat it as ready rather than looping until the deadline.
                return
            except Exception as e:  # noqa: BLE001 — connection refused while booting
                last = e
                time.sleep(0.05)
        raise RuntimeError(f"mock_openai.py did not come up on port {self.port}: {last}")

    def stop(self) -> None:
        self._proc.terminate()
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"


# ── config-dict helper ───────────────────────────────────────────────────────

def openai_config(base_url: str, *, tools: list[str] | None = None,
                  max_turns: int = 6, timeout_secs: int = 30,
                  stream_idle_secs: int = 30, workdir: str | None = None) -> dict:
    """A config dict pointing at `base_url`. The real key is passed to the
    binding explicitly (`api_key=`), so `api_key_env` is a never-read dummy —
    but the field is required by Config, so it must be present."""
    cfg: dict = {
        "provider": "openai",
        "api_key_env": "TA_PYTEST_UNUSED_KEY",
        "base_url": base_url,
        "model": "mock-model",
        "max_tokens": 1024,
        "limits": {
            "max_turns": max_turns,
            "timeout_secs": timeout_secs,
            "max_output_chars": 20000,
            "stream_idle_secs": stream_idle_secs,
        },
    }
    if tools is not None:
        cfg["tools"] = tools
    if workdir is not None:
        cfg["workdir"] = workdir
    return cfg
