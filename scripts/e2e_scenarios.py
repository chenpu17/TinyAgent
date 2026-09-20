#!/usr/bin/env python3
"""End-to-end scenario verification for the tiny-agent headless binary.

Spins up an in-process OpenAI-compatible SSE mock (threaded, so parallel
sub-agents are truly concurrent) and drives four loop-level scenarios through
the REAL CLI/HTTP/SSE stack — the layer unit tests (scripted MockProvider)
cannot reach:

  stuck    — identical call + identical result x6: stop_reason must be
             "stuck_loop", exactly 6 requests, exit code 1, and the nudge
             text must stay OUT of the NDJSON tool_result events (history-only)
  poll     — identical call whose OUTPUT progresses (state-mutating shell
             command): the stuck guardrail must NOT fire; max_turns wins
  parallel — one assistant turn fans out 10 `task` calls (wave cap 8): all
             10 ToolCall/ToolResult pairs land, request count is exactly
             2 (parent) + 10 (children)
  cancel   — interrupt (SIGINT; CTRL_BREAK on Windows) a slow request: exit 130

Exit code 0 iff every check passes. No network, no API key — safe for CI.

Usage:  python3 scripts/e2e_scenarios.py            # auto-builds if needed
        TA_BIN=/path/to/tiny-agent python3 scripts/e2e_scenarios.py
"""
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("TA_BIN", os.path.join(REPO, "target", "debug", "tiny-agent"))

failures = []


def check(name, cond, detail=""):
    print(f"[{'PASS' if cond else 'FAIL'}] {name} {detail}")
    if not cond:
        failures.append(name)


# ── scenario mock ──────────────────────────────────────────────────

REQ_COUNT = [0]
LOCK = threading.Lock()


def sse(chunks):
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"


def text_resp(text):
    return sse([
        {"choices": [{"index": 0, "finish_reason": None,
                      "delta": {"role": "assistant", "content": text}}]},
        {"choices": [{"index": 0, "finish_reason": "stop", "delta": {}}]},
    ])


def tool_calls_resp(calls):
    chunks = []
    for idx, (cid, name, args) in enumerate(calls):
        chunks.append({"choices": [{"index": 0, "finish_reason": None, "delta": {
            "role": "assistant",
            "tool_calls": [{"index": idx, "id": cid, "type": "function",
                            "function": {"name": name, "arguments": ""}}]}}]})
        chunks.append({"choices": [{"index": 0, "finish_reason": None, "delta": {
            "tool_calls": [{"index": idx, "function": {"arguments": args}}]}}]})
    chunks.append({"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {}}]})
    return sse(chunks)


SHELL_STUCK = json.dumps({"command": "echo still-waiting"})
# Same command string every turn, but the OUTPUT advances each call — a
# polling call making progress must not trip the stuck guardrail.
#
# Windows note: the shell tool runs commands via `cmd /C` there, and the sh
# spelling below would be metacharacter soup for cmd (2>/dev/null, $((…)) are
# cmd syntax) — every call would return the same error text and the guardrail
# would (rightly) fire. Command strings also can't carry double quotes through
# the MSVCRT arg quoting, which rules out powershell -Command "…" one-liners.
# `echo tick-%time%-%random%` keeps the contract (same call, advancing output):
# each tool call spawns a fresh cmd.exe, so %time%/%random% re-expand per call.
SHELL_POLL = json.dumps({
    "command": ("echo tick-%time%-%random%" if os.name == "nt" else
                "sh -c 'n=$(cat n.txt 2>/dev/null || echo 0); "
                "n=$((n+1)); echo $n > n.txt; echo tick-$n'"),
})


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        if self.path != "/chat/completions":
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with LOCK:
            REQ_COUNT[0] += 1
        msgs = body["messages"]
        has_tool_msg = any(m.get("role") == "tool" for m in msgs)
        first_user = ""
        for m in msgs:
            if m.get("role") == "user":
                c = m.get("content")
                first_user = c if isinstance(c, str) else json.dumps(c)
                break

        if "STUCK_E2E" in first_user:
            payload = tool_calls_resp([("call_s0", "shell", SHELL_STUCK)])
        elif "POLL_E2E" in first_user:
            payload = tool_calls_resp([("call_q0", "shell", SHELL_POLL)])
        elif has_tool_msg:
            payload = text_resp("done")
        elif "PARALLEL_E2E" in first_user:
            calls = [(f"call_p{i}", "task",
                      json.dumps({"name": f"child{i}",
                                  "description": f"child {i}",
                                  "prompt": f"child {i} do work"}))
                     for i in range(10)]
            payload = tool_calls_resp(calls)
        elif "SLOW_E2E" in first_user:
            time.sleep(6)
            payload = text_resp("slow done")
        else:
            payload = text_resp("child done")

        data = payload.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


# ── driver ─────────────────────────────────────────────────────────


def requests_served():
    with LOCK:
        return REQ_COUNT[0]


def run_agent(cfg, task, timeout=90):
    env = dict(os.environ, E2E_KEY="dummy")
    p = subprocess.run(
        [BIN, "--config", cfg, "--task", task, "--session", "none", "--json"],
        capture_output=True, text=True, env=env, timeout=timeout)
    events = []
    for line in p.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return p, events


def done_event(events):
    return next((e for e in events if e.get("type") == "done"), None)


def main():
    if not os.path.exists(BIN):
        print(f"[setup] building {BIN} ...")
        subprocess.run(["cargo", "build"], cwd=REPO, check=True)

    # Ephemeral port: no collision with a dev's local mock or parallel CI jobs.
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}"

    root = tempfile.mkdtemp(prefix="ta-e2e-")

    def write_cfg(name, tools, max_turns=50):
        workdir = os.path.join(root, f"wd-{name}")
        os.makedirs(workdir, exist_ok=True)
        cfg = {
            "provider": "openai", "base_url": base, "api_key_env": "E2E_KEY",
            "model": "mock-model", "max_tokens": 4096, "workdir": workdir,
            "tools": tools,
            "limits": {"max_turns": max_turns, "timeout_secs": 60,
                       "max_output_chars": 10000},
        }
        path = os.path.join(root, f"cfg-{name}.json")
        with open(path, "w") as f:
            json.dump(cfg, f)
        return path

    # stuck: identical call + identical result -> stuck_loop at 6
    before = requests_served()
    p, ev = run_agent(write_cfg("stuck", ["shell"]), "STUCK_E2E keep checking")
    d = done_event(ev)
    check("stuck: stop_reason", d and d.get("stop_reason") == "stuck_loop", f"got {d}")
    check("stuck: exactly 6 requests", requests_served() - before == 6,
          f"delta={requests_served() - before}")
    tr_events = [e for e in ev if e.get("type") == "tool_result"]
    check("stuck: 6 tool_result events", len(tr_events) == 6, f"got {len(tr_events)}")
    check("stuck: nudge NOT in event stream",
          all("system notice" not in json.dumps(e) for e in tr_events))
    check("stuck: exit code 1 (not retried, not 130)", p.returncode == 1,
          f"rc={p.returncode}")

    # poll: same call, progressing output -> NOT stuck; max_turns wins
    before = requests_served()
    p, ev = run_agent(write_cfg("poll", ["shell"], max_turns=8), "POLL_E2E watch")
    d = done_event(ev)
    check("poll: max_turns_reached (not stuck_loop)",
          d and d.get("stop_reason") == "max_turns_reached", f"got {d}")
    check("poll: 8 requests", requests_served() - before == 8,
          f"delta={requests_served() - before}")
    outs = [e.get("content", "") for e in tr_events_of(ev)]
    check("poll: outputs progressed", len(outs) == 8 and len(set(outs)) == 8,
          f"{len(set(outs))} distinct")

    # parallel: 10 task calls in one turn (wave cap 8 -> 2 waves)
    before = requests_served()
    p, ev = run_agent(write_cfg("parallel", ["task"]), "PARALLEL_E2E fan out")
    d = done_event(ev)
    calls = [e for e in ev if e.get("type") == "tool_call"]
    results = [e for e in ev if e.get("type") == "tool_result"]
    check("parallel: end_turn", d and d.get("stop_reason") == "end_turn", f"got {d}")
    check("parallel: 10 calls + 10 results", len(calls) == 10 and len(results) == 10,
          f"{len(calls)}/{len(results)}")
    check("parallel: all child results",
          len(results) == 10 and all("child done" in r.get("content", "") for r in results))
    check("parallel: 12 requests (2 parent + 10 children)",
          requests_served() - before == 12, f"delta={requests_served() - before}")

    # cancel: interrupt a slow request -> exit 130.
    # Windows: send_signal(SIGINT) raises ValueError there (Python 3.14) and
    # CTRL_C is disabled for a new process group anyway — CTRL_BREAK_EVENT
    # aimed at the child's CREATE_NEW_PROCESS_GROUP group rides the same
    # SetConsoleCtrlHandler path (src/signal.rs treats events 0 and 1 alike).
    # Unlike POSIX SIGINT it can't EINTR a blocked read; the run breaks at the
    # next chunk boundary (the in-flight request is capped by timeout_secs),
    # which stays well inside the communicate timeout below.
    env = dict(os.environ, E2E_KEY="dummy")
    creationflags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
    proc = subprocess.Popen([BIN, "--config", write_cfg("cancel", ["shell"]),
                             "--task", "SLOW_E2E wait", "--session", "none"],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
                            creationflags=creationflags)
    time.sleep(1.5)
    proc.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
    proc.communicate(timeout=30)
    check("cancel: exit 130", proc.returncode == 130, f"rc={proc.returncode}")

    server.shutdown()
    print()
    if failures:
        print(f"E2E FAILURES: {failures}")
        return 1
    print("E2E ALL PASS")
    return 0


def tr_events_of(events):
    return [e for e in events if e.get("type") == "tool_result"]


if __name__ == "__main__":
    sys.exit(main())
