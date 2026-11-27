#!/usr/bin/env python3
"""End-to-end MCP client checks for the tiny-agent headless binary.

Drives the REAL binary (CLI + agent loop + MCP client over stdio and
Streamable HTTP) against two zero-dependency mocks that this file also
contains: an OpenAI-compatible SSE "LLM" that calls tools on script, and an
MCP server (tools `fast` / `slow` / `env`) that writes a request log.

What it pins, on the wire and through the real process:

  A  the tool table the model sees, per allow_tools / deny_tools
  B  config errors name the server, the key and the fix
  C  Ctrl-C during a slow stdio call: notifications/cancelled, exit 130
  D  call_timeout_secs: the message names the bound and the key
  E  boot timeout: connect_timeout_secs vs call_timeout_secs
  F  HTTP: the negotiated protocol version on every request after initialize
  H  HTTP: a lost session (404) is recovered with ONE re-send
  J  HTTP: the 202 + standing-stream shape, and a cancel through it
  K  credential hygiene: what the shell and a stdio MCP server inherit
  I  HTTP: the idle ping, four server shapes (waits ~65 s; opt-in with --idle)

Usage:
    python3 scripts/e2e_mcp.py              # every group but I (~15 s)
    python3 scripts/e2e_mcp.py --idle       # including I
    python3 scripts/e2e_mcp.py A K          # only these groups
    TA_BIN=/path/to/tiny-agent python3 scripts/e2e_mcp.py

Exit status is 0 only when every check passed. The Ctrl-C checks need a POSIX
signal model and are skipped on Windows.
"""
import json
import os
import queue
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("TA_BIN", os.path.join(REPO, "target", "debug", "tiny-agent"))
SELF = os.path.abspath(__file__)
KEY_ENV = "TA_E2E_KEY"        # the dummy "API key" variable
PREFIX = "MCP24"              # every task starts with it; the mock LLM reads the run key after it
POSIX = os.name != "nt"

# ── the mock MCP server ─────────────────────────────────────────────
# Started as `e2e_mcp.py __mock-mcp stdio` (by the agent, from the config) or
# `e2e_mcp.py __mock-mcp http <port>` (by the driver). Behaviour knobs are
# environment variables (TA_E2E_MCP_*), set per run by whoever starts it.

MCP_LOG = os.environ.get("TA_E2E_MCP_LOG")
MCP_VERSION = os.environ.get("TA_E2E_MCP_VERSION", "2025-03-26")
MCP_FORGET = os.environ.get("TA_E2E_MCP_FORGET")
MCP_PING = os.environ.get("TA_E2E_MCP_PING", "ok")            # ok | error | hang
MCP_STANDALONE = os.environ.get("TA_E2E_MCP_STANDALONE") == "1"
MCP_BOOT = os.environ.get("TA_E2E_MCP_BOOT", "0")             # seconds, or `never`

MCP_TOOLS = [
    {"name": "fast", "description": "Return at once.",
     "inputSchema": {"type": "object", "properties": {"x": {"type": "string"}},
                     "required": ["x"]}},
    {"name": "slow", "description": "Sleep for `secs` seconds, then return.",
     "inputSchema": {"type": "object", "properties": {"secs": {"type": "number"}},
                     "required": ["secs"]}},
    {"name": "env", "description": "Return one variable of this server's environment.",
     "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}},
                     "required": ["name"]}},
]
MCP_LOCK = threading.Lock()     # guards SESSION / CANCELS / STREAMS
MCP_LOG_LOCK = threading.Lock()
MCP_SESSION = {"id": None, "n": 0}
MCP_CANCELS = {}                # request id (str) -> threading.Event
MCP_STREAMS = []                # live standing GET streams, each with a Queue


def mcp_log(**entry):
    if not MCP_LOG:
        return
    entry["t"] = round(time.time(), 3)
    with MCP_LOG_LOCK:
        with open(MCP_LOG, "a") as f:
            f.write(json.dumps(entry) + "\n")


def rpc_reply(mid, result):
    return {"jsonrpc": "2.0", "id": mid, "result": result}


def rpc_error(mid, code, message):
    return {"jsonrpc": "2.0", "id": mid, "error": {"code": code, "message": message}}


def rpc_text(text, is_error=False):
    return {"content": [{"type": "text", "text": text}], "isError": is_error}


def mcp_slow_call(mid, args):
    """Sleep `secs`; a cancellation notice for `mid` wakes it early."""
    ev = threading.Event()
    key = str(mid)
    with MCP_LOCK:
        MCP_CANCELS[key] = ev
    try:
        try:
            want = max(0.0, min(float(args.get("secs", 1)), 600.0))
        except (TypeError, ValueError):
            want = 1.0
        deadline = time.time() + want
        while time.time() < deadline:
            if ev.wait(timeout=0.1):
                mcp_log(method="tools/call", id=mid, status="aborted",
                        note="aborted by cancellation")
                return rpc_text("slow:cancelled", is_error=True)
        return rpc_text("slow:done")
    finally:
        with MCP_LOCK:
            MCP_CANCELS.pop(key, None)


def mcp_handle(msg, transport, via, headers):
    """One JSON-RPC message -> (http_status, payload | None).

    `payload is None` = a notification, or a request the mock never answers.
    """
    method = msg.get("method")
    has_id = msg.get("id") is not None
    mid = msg.get("id") if has_id else None
    params = msg.get("params") or {}
    entry = {
        "transport": transport, "via": via, "method": method, "id": mid,
        "params": params,
        "hdr": {k.lower(): v for k, v in (headers or {}).items()
                if k.lower() in ("mcp-session-id", "mcp-protocol-version")},
    }
    if not has_id:
        if method == "notifications/cancelled":
            with MCP_LOCK:
                ev = MCP_CANCELS.get(str(params.get("requestId")))
            entry["aborted"] = bool(ev)
            if ev:
                ev.set()
        mcp_log(status="notification", **entry)
        return 200, None

    if method == "initialize":
        if MCP_BOOT == "never":
            mcp_log(status="never", **entry)
            time.sleep(3600)
            return 200, None
        if float(MCP_BOOT or 0) > 0:
            time.sleep(float(MCP_BOOT))
        with MCP_LOCK:
            MCP_SESSION["n"] += 1
            MCP_SESSION["id"] = f"sess-{MCP_SESSION['n']}"
            sid = MCP_SESSION["id"]
            MCP_STREAMS.clear()
        if MCP_FORGET and os.path.exists(MCP_FORGET):
            # A fresh initialize re-arms the server: the NEXT request works.
            try:
                os.unlink(MCP_FORGET)
            except OSError:
                pass
        entry["session"] = sid
        mcp_log(status=200, **entry)
        return 200, rpc_reply(mid, {
            "protocolVersion": MCP_VERSION, "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-mcp", "version": "0"},
        })
    if method == "tools/list":
        mcp_log(status=200, **entry)
        return 200, rpc_reply(mid, {"tools": MCP_TOOLS})
    if method == "ping":
        if MCP_PING == "hang":
            mcp_log(status="hang", **entry)
            time.sleep(3600)
            return 200, None
        mcp_log(status=200, **entry)
        if MCP_PING == "error":
            return 200, rpc_error(mid, -32601, "Method not found")
        return 200, rpc_reply(mid, {})
    if method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "fast":
            mcp_log(status=200, **entry)
            return 200, rpc_reply(mid, rpc_text(f"fast:{args.get('x', '')}"))
        if name == "env":
            # What THIS process inherited — the credential-hygiene probe (K).
            mcp_log(status=200, **entry)
            return 200, rpc_reply(
                mid, rpc_text(os.environ.get(str(args.get("name", "")), "<unset>")))
        if name == "slow":
            mcp_log(status="running", **entry)   # logged at ARRIVAL
            out = mcp_slow_call(mid, args)
            mcp_log(status=200, **entry)
            return 200, rpc_reply(mid, out)
        mcp_log(status=200, **entry)
        return 200, rpc_error(mid, -32602, f"Unknown tool: {name}")
    mcp_log(status=200, **entry)
    return 200, rpc_error(mid, -32601, "Method not found")


def mock_mcp_stdio():
    out_lock = threading.Lock()

    def send(obj):
        # The parent may already be gone (it exits right after a Ctrl-C): a
        # write to a closed pipe is the normal end of a cancelled run.
        try:
            with out_lock:
                sys.stdout.write(json.dumps(obj) + "\n")
                sys.stdout.flush()
        except OSError:
            pass

    def worker(msg):
        try:
            _, payload = mcp_handle(msg, "stdio", "stdio", {})
            if payload is not None:
                send(payload)
        except Exception as e:  # keep serving whatever one request did
            send(rpc_error(msg.get("id"), -32603, f"mock failure: {e}"))

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        threading.Thread(target=worker, args=(msg,), daemon=True).start()


class MockMcpHttp(BaseHTTPRequestHandler):
    """Streamable HTTP: POST /mcp answers on its own body (the default) or,
    with TA_E2E_MCP_STANDALONE=1, with 202 + a push on the standing GET."""
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _hdrs(self):
        return {"mcp-session-id": self.headers.get("Mcp-Session-Id"),
                "mcp-protocol-version": self.headers.get("MCP-Protocol-Version")}

    def _send(self, status, payload=None, session=None, ctype="application/json"):
        body = b"" if payload is None else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        if session:
            self.send_header("Mcp-Session-Id", session)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)
            self.wfile.flush()

    def _session_check(self):
        """None = fine, else the (status, message) to answer with."""
        sid = self.headers.get("Mcp-Session-Id")
        if not sid:
            return 400, "Bad Request: Mcp-Session-Id header required"
        with MCP_LOCK:
            known = sid == MCP_SESSION["id"]
        if not known or (MCP_FORGET and os.path.exists(MCP_FORGET)):
            return 404, "Session not found"
        return None

    def do_POST(self):
        if self.path != "/mcp":
            self._send(404, {"json": "Not Found"})
            return
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        try:
            msg = json.loads(raw) if raw else None
        except json.JSONDecodeError:
            msg = None
        if msg is None:
            self._send(400, {"json": "Bad Request: invalid JSON"})
            return
        if "method" in msg and msg.get("method") != "initialize":
            bad = self._session_check()
            if bad:
                mcp_log(transport="http", via="post", method=msg.get("method"),
                        id=msg.get("id"), params=msg.get("params"), status=bad[0],
                        hdr=self._hdrs())
                self._send(bad[0], {"json": bad[1]})
                return
        push = (MCP_STANDALONE and "method" in msg
                and msg.get("method") != "initialize" and msg.get("id") is not None)
        if push:
            # Logged at ARRIVAL (the answer may be a minute away).
            mcp_log(transport="http", via="post->stream", method=msg.get("method"),
                    id=msg.get("id"), params=msg.get("params"), status=202,
                    hdr=self._hdrs())
            self._send(202)
            hdrs = self.headers

            def later():
                # A 202 promises delivery on the standing stream, so wait
                # (bounded) until one is connected before answering: a reply
                # pushed while no GET is open has nowhere to go.
                deadline = time.time() + 10
                while time.time() < deadline:
                    with MCP_LOCK:
                        if MCP_STREAMS:
                            break
                    time.sleep(0.005)
                _, payload = mcp_handle(msg, "http", "post->stream", hdrs)
                if payload is not None:
                    with MCP_LOCK:
                        targets = list(MCP_STREAMS)
                    for q in targets:
                        q.put(payload)
            threading.Thread(target=later, daemon=True).start()
            return
        status, payload = mcp_handle(msg, "http", "post", self.headers)
        if payload is None:
            self._send(202)
        else:
            self._send(status, payload,
                       session=MCP_SESSION["id"] if msg.get("method") == "initialize" else None)

    def do_GET(self):
        if self.path != "/mcp" or not MCP_STANDALONE:
            self._send(405, {"json": "Method Not Allowed"})
            return
        bad = self._session_check()
        if bad:
            mcp_log(transport="http", via="get", method=None, id=None, params=None,
                    status=bad[0], hdr=self._hdrs())
            self._send(bad[0], {"json": bad[1]})
            return
        with MCP_LOCK:
            sid = MCP_SESSION["id"]
            forget = MCP_FORGET
        mcp_log(transport="http", via="get", method=None, id=None, params=None,
                status=200, hdr=self._hdrs())
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        q = queue.Queue()
        with MCP_LOCK:
            MCP_STREAMS.append(q)
        try:
            while True:
                try:
                    item = q.get(timeout=0.5)
                except queue.Empty:
                    item = None
                with MCP_LOCK:
                    alive = q in MCP_STREAMS and sid == MCP_SESSION["id"]
                if not alive or (forget and os.path.exists(forget)):
                    break
                if item is not None:
                    self.wfile.write(f"event: message\ndata: {json.dumps(item)}\n\n".encode())
                    self.wfile.flush()
        except OSError:
            pass
        finally:
            with MCP_LOCK:
                if q in MCP_STREAMS:
                    MCP_STREAMS.remove(q)

    def do_DELETE(self):
        self._send(200, {})


def mock_mcp_main():
    if sys.argv[2] == "stdio":
        mock_mcp_stdio()
    else:
        server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[3])), MockMcpHttp)
        server.daemon_threads = True
        print("ready", flush=True)
        server.serve_forever()


# ── the scripted "LLM" ──────────────────────────────────────────────

LLM_LOCK = threading.Lock()
LLM_LOG = []     # {"run", "tools", "tool_msgs", "t"} per chat request
SCRIPTS = {}     # run key -> [{"delay": s?, "tool": display name, "args": {…}}]


def sse(chunks):
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"


def text_resp(text):
    return sse([
        {"choices": [{"index": 0, "finish_reason": None,
                      "delta": {"role": "assistant", "content": text}}]},
        {"choices": [{"index": 0, "finish_reason": "stop", "delta": {}}],
         "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}},
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


class MockLlm(BaseHTTPRequestHandler):
    """One chat endpoint: the Nth reply of a run calls the Nth scripted tool
    (N = tool messages seen so far); past the script it answers with text."""
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        msgs = body["messages"]
        task = next((m.get("content") for m in msgs if m.get("role") == "user"), "")
        if not isinstance(task, str):   # content blocks: [{"type": "text", "text": …}]
            task = " ".join(b.get("text", "") for b in task if isinstance(b, dict))
        parts = task.split()
        run_key = parts[1] if len(parts) > 1 and parts[0] == PREFIX else "?"
        done = sum(1 for m in msgs if m.get("role") == "tool")
        with LLM_LOCK:
            LLM_LOG.append({"run": run_key,
                            "tools": [t["function"]["name"] for t in body.get("tools", [])],
                            "tool_msgs": done, "t": round(time.time(), 3)})
            steps = SCRIPTS.get(run_key, [])
        if done < len(steps):
            step = steps[done]
            if step.get("delay"):
                time.sleep(step["delay"])
            payload = tool_calls_resp([(f"call_{done}", step["tool"], json.dumps(step["args"]))])
        else:
            payload = text_resp("final-answer")
        data = payload.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


# ── driver helpers ──────────────────────────────────────────────────

failures = []
ROOT = None            # temp dir: configs, logs, workdirs
LLM_BASE = None
MOCKS = []             # HTTP mock MCP servers to kill at the end
ONLY = set()           # group letters to run (empty = all non-opt-in)


def check(name, cond, detail=""):
    print(f"[{'PASS' if cond else 'FAIL'}] {name}  {detail}", flush=True)
    if not cond:
        failures.append(name)


def skipped(name, why):
    print(f"[SKIP] {name}  {why}", flush=True)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def agent_env():
    return dict(os.environ, **{KEY_ENV: "dummy"})


def write_cfg(name, mcp, timeout_secs=300, tools=(), extra=None):
    """One run config. `extra` is merged over the top level (K sets
    env_passthrough through it); `tools` replaces the empty whitelist."""
    wd = os.path.join(ROOT, f"wd-{name}")
    os.makedirs(wd, exist_ok=True)
    cfg = {
        "provider": "openai", "base_url": LLM_BASE, "api_key_env": KEY_ENV,
        "model": "mock-model", "max_tokens": 4096, "workdir": wd, "tools": list(tools),
        "limits": {"max_turns": 20, "timeout_secs": timeout_secs,
                   "max_output_chars": 10000},
        "mcp_servers": [mcp],
    }
    cfg.update(extra or {})
    path = os.path.join(ROOT, f"cfg-{name}.json")
    with open(path, "w") as f:
        json.dump(cfg, f)
    return path


def stdio_server(log, env=None, **extra):
    s = {"name": "srv", "command": sys.executable,
         "args": ["-X", "utf8", SELF, "__mock-mcp", "stdio"],
         "env": dict({"TA_E2E_MCP_LOG": log}, **(env or {}))}
    s.update(extra)
    return s


def start_http(log, env=None):
    port = free_port()
    e = dict(os.environ, TA_E2E_MCP_LOG=log, **(env or {}))
    proc = subprocess.Popen([sys.executable, "-X", "utf8", SELF, "__mock-mcp", "http", str(port)],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=e)
    if proc.stdout.readline().strip() != "ready":
        sys.exit("the mock MCP server did not come up")
    MOCKS.append(proc)
    return f"http://127.0.0.1:{port}/mcp"


def parse_events(text):
    events = []
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return events


def read_log(path):
    rows = []
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        rows.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
    return rows


def wait_log(path, pred, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        rows = read_log(path)
        if pred(rows):
            return rows
        time.sleep(0.02)
    return read_log(path)


def argv(cfg, key):
    return [BIN, "--config", cfg, "--task", f"{PREFIX} {key}", "--session", "none", "--json"]


def run(cfg, key, timeout=120):
    t0 = time.time()
    p = subprocess.run(argv(cfg, key), capture_output=True, text=True,
                       env=agent_env(), timeout=timeout)
    return p, parse_events(p.stdout), time.time() - t0


def popen(cfg, key):
    return subprocess.Popen(argv(cfg, key), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, env=agent_env())


def results(ev):
    return [e for e in ev if e.get("type") == "tool_result"]


def done(ev):
    return next((e for e in ev if e.get("type") == "done"), None)


def tail(s, n=240):
    return " | ".join(s.strip().splitlines()[-3:])[-n:]


def posts(rows):
    return [x for x in rows if x["via"] in ("post", "post->stream")]


def cancel_notices(rows):
    return [x for x in rows if x["method"] == "notifications/cancelled"]


# ── scenarios ───────────────────────────────────────────────────────

def scen_filters():
    cases = [
        ("none", {}, {"mcp_srv_fast", "mcp_srv_slow"}),
        ("allow", {"allow_tools": ["fast"]}, {"mcp_srv_fast"}),
        ("deny", {"deny_tools": ["slow"]}, {"mcp_srv_fast"}),
    ]
    for tag, extra, want in cases:
        key = f"filter{tag}"
        cfg = write_cfg(f"filter-{tag}", stdio_server(os.path.join(ROOT, f"{key}.jsonl"), **extra))
        SCRIPTS[key] = [{"tool": "mcp_srv_fast", "args": {"x": tag}}]
        p, ev, _ = run(cfg, key)
        with LLM_LOCK:
            mine = [r for r in LLM_LOG if r["run"] == key]
        got = set(mine[0]["tools"]) & {"mcp_srv_fast", "mcp_srv_slow"} if mine else set()
        check(f"A[{tag}] the model sees exactly the allowed tools", got == want,
              f"got {sorted(got)} want {sorted(want)}")
        res = results(ev)
        check(f"A[{tag}] the kept tool still runs end to end",
              p.returncode == 0 and res and "fast:" + tag in res[0]["content"]
              and not res[0]["is_error"] and done(ev) is not None,
              f"rc={p.returncode} {res[:1]} {tail(p.stderr) if p.returncode else ''}")
        seq = [x["method"] for x in read_log(os.path.join(ROOT, f"{key}.jsonl"))[:3]]
        check(f"A[{tag}] stdio handshake order",
              seq == ["initialize", "notifications/initialized", "tools/list"], f"{seq}")


def scen_config_errors():
    cfg = write_cfg("badallow", stdio_server(os.path.join(ROOT, "badallow.jsonl"),
                                             allow_tools=["nosuch"]))
    p, _, _ = run(cfg, "badallow", timeout=60)
    check("B an allow_tools name the server does not expose fails the assembly",
          p.returncode != 0 and all(s in p.stderr for s in
                                    ("mcp server 'srv'", "allow_tools", "nosuch")),
          f"rc={p.returncode} {tail(p.stderr)}")
    cfg = write_cfg("badlist", stdio_server(os.path.join(ROOT, "badlist.jsonl"),
                                            allow_tools=["ok", ""]))
    p, _, _ = run(cfg, "badlist", timeout=60)
    check("B an empty name in a tool list is a config error",
          p.returncode != 0 and '"allow_tools"' in p.stderr and "empty tool name" in p.stderr,
          f"rc={p.returncode} {tail(p.stderr)}")
    cfg = write_cfg("zero", stdio_server(os.path.join(ROOT, "zero.jsonl"), call_timeout_secs=0))
    p, _, _ = run(cfg, "zero", timeout=60)
    check("B call_timeout_secs 0 names the key and the default",
          p.returncode != 0 and '"call_timeout_secs"' in p.stderr
          and "must be at least 1" in p.stderr and "default, 120s" in p.stderr,
          f"rc={p.returncode} {tail(p.stderr)}")


def scen_cancel_stdio():
    key = "cancelstdio"
    log = os.path.join(ROOT, f"{key}.jsonl")
    cfg = write_cfg("cancel-stdio", stdio_server(log))
    SCRIPTS[key] = [{"tool": "mcp_srv_slow", "args": {"secs": 60}}]
    proc = popen(cfg, key)
    wait_log(log, lambda r: any(x["method"] == "tools/call" for x in r))
    check("C the slow call reached the stdio server",
          any(x["method"] == "tools/call" for x in read_log(log)))
    t0 = time.time()
    proc.send_signal(signal.SIGINT)
    out, err = proc.communicate(timeout=40)
    dt = time.time() - t0
    rows = read_log(log)
    call = next((x for x in rows if x["method"] == "tools/call"), None)
    cancels = cancel_notices(rows)
    check("C Ctrl-C exits 130", proc.returncode == 130, f"rc={proc.returncode} {tail(err)}")
    check("C the run ended promptly, not after the 60 s sleep", dt < 10, f"{dt:.1f}s")
    check("C the server was told which request is cancelled",
          len(cancels) == 1 and call is not None
          and cancels[0]["params"] == {"requestId": call["id"],
                                       "reason": "cancelled by the client"},
          f"cancels={[(c['params'], c.get('aborted')) for c in cancels]} "
          f"call_id={call and call['id']}")
    check("C the server aborted the work", cancels and cancels[0].get("aborted") is True,
          f"{[c.get('aborted') for c in cancels]}")
    ev = parse_events(out)
    check("C the tool result the model would have seen is an error",
          results(ev) and results(ev)[0]["is_error"],
          f"{[(r['is_error'], r['content'][:90]) for r in results(ev)]} done={done(ev)}")


def scen_call_timeout():
    key = "ctimeout"
    log = os.path.join(ROOT, f"{key}.jsonl")
    cfg = write_cfg("ctimeout", stdio_server(log, call_timeout_secs=2))
    SCRIPTS[key] = [{"tool": "mcp_srv_slow", "args": {"secs": 60}}]
    p, ev, secs = run(cfg, key)
    res = results(ev)
    check("D a call timeout names the bound and the config key",
          res and res[0]["is_error"] and "timed out after 2s" in res[0]["content"]
          and "limit: call_timeout_secs" in res[0]["content"],
          f"{[r['content'][:200] for r in res]}")
    check("D the run carries on and finishes",
          p.returncode == 0 and done(ev) is not None and secs < 30,
          f"rc={p.returncode} {secs:.1f}s done={done(ev)}")
    check("D a timeout is not a cancellation notice (nothing to revoke on stdio)",
          not cancel_notices(read_log(log)))


def scen_boot_timeout():
    for tag, extra, want in (
            ("connect", {"connect_timeout_secs": 2}, "limit: connect_timeout_secs"),
            ("call", {"call_timeout_secs": 2}, "limit: call_timeout_secs")):
        log = os.path.join(ROOT, f"boot{tag}.jsonl")
        cfg = write_cfg(f"boot-{tag}", stdio_server(log, {"TA_E2E_MCP_BOOT": "never"}, **extra))
        t0 = time.time()
        p = subprocess.run(argv(cfg, "boot"), capture_output=True, text=True,
                           env=agent_env(), timeout=90)
        dt = time.time() - t0
        check(f"E[{tag}] a server that never boots fails naming the key that set the bound",
              p.returncode != 0 and "initialize" in p.stderr and want in p.stderr and dt < 15,
              f"rc={p.returncode} {dt:.1f}s {tail(p.stderr)}")


def scen_http_version():
    for tag, version, warn in (("known", "2025-06-18", False), ("unknown", "2099-01-01", True)):
        key = f"httpver{tag}"
        log = os.path.join(ROOT, f"{key}.jsonl")
        url = start_http(log, {"TA_E2E_MCP_VERSION": version})
        cfg = write_cfg(f"http-ver-{tag}", {"name": "srv", "url": url, "transport": "http"})
        SCRIPTS[key] = [{"tool": "mcp_srv_fast", "args": {"x": tag}}]
        p, ev, _ = run(cfg, key)
        rows = posts(read_log(log))
        after = [x for x in rows if x["method"] != "initialize"]
        check(f"F[{tag}] every request after initialize carries the negotiated version",
              after and all(x["hdr"].get("mcp-protocol-version") == version for x in after),
              f"{[(x['method'], x['hdr']) for x in rows]}")
        init = next((x for x in rows if x["method"] == "initialize"), None)
        check(f"F[{tag}] the client offers 2025-03-26 and never second-guesses the answer",
              init is not None and init["params"].get("protocolVersion") == "2025-03-26",
              f"client offered {init and init['params'].get('protocolVersion')}")
        check(f"F[{tag}] a warning only for a revision this client was not verified against",
              ("not verified" in p.stderr) is warn and (version in p.stderr) is warn,
              f"stderr={tail(p.stderr)}")
        res = results(ev)
        check(f"F[{tag}] the call works",
              p.returncode == 0 and res and "fast:" + tag in res[0]["content"],
              f"rc={p.returncode} {res[:1]}")


def scen_http_recover():
    key = "recover"
    log = os.path.join(ROOT, f"{key}.jsonl")
    forget = os.path.join(ROOT, "forget")
    url = start_http(log, {"TA_E2E_MCP_FORGET": forget})
    cfg = write_cfg("http-recover", {"name": "srv", "url": url, "transport": "http"})
    SCRIPTS[key] = [{"delay": 1.0, "tool": "mcp_srv_fast", "args": {"x": "r"}}]
    proc = popen(cfg, key)
    wait_log(log, lambda r: any(x["method"] == "tools/list" for x in r))
    open(forget, "w").close()   # the server "forgets" its session before the call
    out, err = proc.communicate(timeout=60)
    ev = parse_events(out)
    rows = posts(read_log(log))
    seq = [(x["method"], x["status"]) for x in rows]
    want = [("initialize", 200), ("notifications/initialized", "notification"),
            ("tools/list", 200), ("tools/call", 404), ("initialize", 200),
            ("notifications/initialized", "notification"), ("tools/call", 200)]
    check("H a 404 on a carried session is answered by a new handshake and ONE re-send",
          seq == want, f"{seq}")
    res = results(ev)
    check("H the re-sent call succeeds: the model never sees the 404",
          proc.returncode == 0 and res and not res[0]["is_error"]
          and "fast:r" in res[0]["content"], f"rc={proc.returncode} {res[:1]} {tail(err)}")
    check("H the recovery is on the record (one warn line)",
          err.count("starting a new session") == 1 and "tools/call" in err, tail(err))
    sids = [x["hdr"].get("mcp-session-id") for x in rows]
    check("H the re-sent call rides the NEW session id",
          len(sids) == 7 and sids[3] == "sess-1" and sids[4] is None
          and sids[6] == "sess-2", f"{sids}")


def scen_http_stream():
    key = "stream"
    log = os.path.join(ROOT, f"{key}.jsonl")
    url = start_http(log, {"TA_E2E_MCP_STANDALONE": "1"})
    cfg = write_cfg("http-stream", {"name": "srv", "url": url, "transport": "http"})
    SCRIPTS[key] = [{"tool": "mcp_srv_fast", "args": {"x": "s"}}]
    p, ev, _ = run(cfg, key)
    rows = read_log(log)
    gets = [x for x in rows if x["via"] == "get"]
    check("J the standing GET is opened and carries the negotiated version",
          gets and gets[0]["status"] == 200
          and gets[0]["hdr"].get("mcp-protocol-version") == "2025-03-26",
          f"{[(g['status'], g['hdr']) for g in gets[:3]]}")
    check("J the call is accepted with 202 and answered on the stream",
          [x.get("status") for x in rows if x["method"] == "tools/call"] == [202, 200],
          f"{[(x['method'], x.get('status')) for x in rows if x['method'] == 'tools/call']}")
    res = results(ev)
    check("J the result arrives", p.returncode == 0 and res and "fast:s" in res[0]["content"],
          f"rc={p.returncode} {res[:1]} {tail(p.stderr) if p.returncode else ''}")
    if not POSIX:
        return
    # … and a Ctrl-C while a 202-parked call is still running on the server.
    key = "streamcancel"
    SCRIPTS[key] = [{"tool": "mcp_srv_slow", "args": {"secs": 60}}]
    proc = popen(cfg, key)
    wait_log(log, lambda r: any(x["method"] == "tools/call" and x.get("status") == 202
                                and x["params"].get("name") == "slow" for x in r))
    proc.send_signal(signal.SIGINT)
    t0 = time.time()
    out, err = proc.communicate(timeout=40)
    dt = time.time() - t0
    rows = read_log(log)
    slow = [x for x in rows if x["method"] == "tools/call" and x.get("status") == 202
            and x["params"].get("name") == "slow"]
    cancels = cancel_notices(rows)
    check("J Ctrl-C during a 202-parked call exits 130, promptly",
          proc.returncode == 130 and dt < 10, f"rc={proc.returncode} {dt:.1f}s {tail(err)}")
    check("J the cancellation notice is POSTed for the parked request",
          len(slow) == 1 and len(cancels) == 1
          and cancels[0]["params"] == {"requestId": slow[0]["id"],
                                       "reason": "cancelled by the client"}
          and cancels[0].get("aborted") is True,
          f"cancels={[(c['params'], c.get('aborted')) for c in cancels]} "
          f"slow_id={[s['id'] for s in slow]}")


def scen_idle():
    """≥60 s idle, four server shapes, in parallel (one 62 s wait in total)."""
    idle_script = [{"tool": "mcp_srv_fast", "args": {"x": "a"}},
                   {"delay": 62, "tool": "mcp_srv_fast", "args": {"x": "b"}}]
    shapes = [
        ("direct", {}),
        ("ping-error", {"TA_E2E_MCP_PING": "error"}),
        ("stream", {"TA_E2E_MCP_STANDALONE": "1"}),
        ("silent", {"TA_E2E_MCP_PING": "hang"}),
    ]
    running = []
    for tag, env in shapes:
        key = f"idle{tag.replace('-', '')}"
        log = os.path.join(ROOT, f"{key}.jsonl")
        url = start_http(log, env)
        cfg = write_cfg(f"idle-{tag}", {"name": "srv", "url": url, "transport": "http"})
        SCRIPTS[key] = idle_script
        running.append((tag, key, log, popen(cfg, key), time.time()))
    for tag, key, log, proc, t0 in running:
        out, err = proc.communicate(timeout=150)
        dt = time.time() - t0
        res = results(parse_events(out))
        seen, uniq = set(), []
        for x in posts(read_log(log)):   # the 202 shape logs a request twice
            k = (x["method"], x.get("id"))
            if k not in seen:
                seen.add(k)
                uniq.append(x)
        calls = [x for x in uniq if x["method"] == "tools/call"]
        pings = [x for x in uniq if x["method"] == "ping"]
        if tag != "silent":
            check(f"I[{tag}] an idle connection is pinged once before the next call",
                  len(calls) == 2 and len(pings) == 1
                  and calls[0]["t"] < pings[0]["t"] < calls[1]["t"]
                  and pings[0]["t"] - calls[0]["t"] >= 60,
                  f"calls={[round(c['t'] - t0, 1) for c in calls]} "
                  f"pings={[round(p['t'] - t0, 1) for p in pings]}")
            check(f"I[{tag}] the call goes through and the run finishes",
                  proc.returncode == 0 and len(res) == 2 and not any(r["is_error"] for r in res)
                  and res[1]["content"].endswith("fast:b"),
                  f"rc={proc.returncode} {dt:.0f}s {[r['content'][:60] for r in res]} "
                  f"{tail(err) if proc.returncode else ''}")
            check(f"I[{tag}] no 'unreachable' failure for a server that answered the ping",
                  "unreachable" not in err, tail(err))
        else:
            first_ping = pings[0]["t"] if pings else None
            check("I[silent] a server that stays silent is not sent the call",
                  len(calls) == 1 and len(pings) == 1, f"calls={len(calls)} pings={len(pings)}")
            check("I[silent] the call fails in ~5 s with a message that names what was NOT sent",
                  len(res) == 2 and res[1]["is_error"] and "unreachable after" in res[1]["content"]
                  and "'tools/call' was not sent" in res[1]["content"]
                  and first_ping is not None and dt - (first_ping - t0) < 20,
                  f"rc={proc.returncode} {dt:.0f}s {res[1]['content'][:220] if len(res) > 1 else res}")


def scen_hygiene():
    """The variable `api_key_env` names is missing from what the tools spawn
    (kernel plan 2.5): the shell and a stdio MCP server, by default; present
    again with env_passthrough; a server entry's own `env` hands it over."""
    if POSIX:
        shell_probe = f'printenv {KEY_ENV} || echo "<unset>"'
        hidden, shown = "<unset>", "dummy"
    else:
        shell_probe = f"echo [%{KEY_ENV}%]"
        hidden, shown = "[]", "[dummy]"

    def go(tag, mcp_env=None, extra=None):
        key = f"hygiene{tag}"
        cfg = write_cfg(f"hygiene-{tag}",
                        stdio_server(os.path.join(ROOT, f"{key}.jsonl"), env=mcp_env),
                        tools=["shell"], extra=extra)
        SCRIPTS[key] = [{"tool": "shell", "args": {"command": shell_probe}},
                        {"tool": "mcp_srv_env", "args": {"name": KEY_ENV}}]
        return run(cfg, key)

    def first_line(res):
        """The shell's own output: the tool's first line is its `$ command` echo."""
        lines = res["content"].splitlines()
        return (lines[1] if len(lines) > 1 else "").strip()

    def brief(res):
        return [r["content"][:70] for r in res]

    # 1) The default: the model drives the shell and the server; neither
    #    finds the variable (the agent itself has it — case 2 is the control).
    p, ev, _ = go("default")
    res = results(ev)
    check("K the shell tool cannot read the key variable",
          len(res) == 2 and first_line(res[0]) == hidden and not res[0]["is_error"],
          f"rc={p.returncode} {brief(res)} {tail(p.stderr) if p.returncode else ''}")
    check("K a stdio MCP server cannot read the key variable",
          len(res) == 2 and res[1]["content"].strip() == hidden and not res[1]["is_error"],
          f"{brief(res)}")
    note = [ln for ln in p.stderr.splitlines() if "hidden from tool subprocesses" in ln]
    check("K the startup note names the hidden variable, once",
          p.returncode == 0 and done(ev) is not None and len(note) == 1 and KEY_ENV in note[0],
          f"rc={p.returncode} note={note} {tail(p.stderr) if p.returncode else ''}")
    # … and the unattended-run hint is printed with it — once, only while
    # restrict_to_workdir is off (case 4 shows the restricted side).
    check("K the unattended hint names restrict_to_workdir, once, while it is off",
          p.stderr.count("restrict_to_workdir is off") == 1
          and '"restrict_to_workdir": true' in p.stderr, tail(p.stderr))

    # 2) env_passthrough naming it: visible again to both — the control that
    #    proves "<unset>" above was the stripping, not a missing variable.
    p, ev, _ = go("pass", extra={"env_passthrough": [KEY_ENV]})
    res = results(ev)
    check("K env_passthrough hands the variable back to both",
          p.returncode == 0 and len(res) == 2 and first_line(res[0]) == shown
          and res[1]["content"].strip() == shown and not any(r["is_error"] for r in res),
          f"rc={p.returncode} {brief(res)} {tail(p.stderr) if p.returncode else ''}")
    check("K nothing is announced as hidden while the variable is passed through",
          "hidden from tool subprocesses" not in p.stderr, tail(p.stderr))

    # 3) A server entry that sets the variable in its own `env` gets that
    #    value — handed over on purpose — while the shell still gets nothing.
    p, ev, _ = go("configured", mcp_env={KEY_ENV: "handed-over"})
    res = results(ev)
    check("K a server entry's own env wins, for that server only",
          p.returncode == 0 and len(res) == 2 and first_line(res[0]) == hidden
          and res[1]["content"].strip() == "handed-over",
          f"rc={p.returncode} {brief(res)} {tail(p.stderr) if p.returncode else ''}")

    # 4) A restricted config prints no unattended hint (the hint itself was
    #    checked on the default run above). A hint, not a default change.
    p, ev, _ = go("restricted", extra={"restrict_to_workdir": True})
    check("K a restricted config prints no unattended hint",
          p.returncode == 0 and done(ev) is not None and "restrict_to_workdir is off" not in p.stderr,
          f"rc={p.returncode} {tail(p.stderr) if p.returncode else ''}")


# ── main ────────────────────────────────────────────────────────────

def main():
    global ROOT, LLM_BASE, ONLY
    args = [a for a in sys.argv[1:]]
    with_idle = "--idle" in args
    ONLY = {a.upper() for a in args if a != "--idle"}
    if not os.path.exists(BIN):
        print(f"[setup] building {BIN} ...")
        subprocess.run(["cargo", "build", "--locked", "-p", "tiny-agent"], cwd=REPO, check=True)
    ROOT = tempfile.mkdtemp(prefix="ta-e2e-mcp-")
    server = ThreadingHTTPServer(("127.0.0.1", free_port()), MockLlm)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    LLM_BASE = f"http://127.0.0.1:{server.server_address[1]}"

    plan = [
        ("A", "tool table: allow_tools / deny_tools (stdio)", scen_filters),
        ("B", "config errors name the server, the key and the fix", scen_config_errors),
        ("C", "Ctrl-C during a slow stdio call", scen_cancel_stdio),
        ("D", "call_timeout_secs", scen_call_timeout),
        ("E", "boot timeout: connect vs call key", scen_boot_timeout),
        ("F", "HTTP: negotiated protocol version", scen_http_version),
        ("H", "HTTP: session lost and recovered", scen_http_recover),
        ("J", "HTTP: standing-stream shape (202) + cancel", scen_http_stream),
        ("K", "credential hygiene: what shell / stdio MCP children inherit", scen_hygiene),
        ("I", "HTTP: idle ping (waits ~65 s)", scen_idle),
    ]
    try:
        for letter, title, fn in plan:
            if ONLY and letter not in ONLY:
                continue
            if letter == "I" and not with_idle:
                skipped("I idle ping", "opt-in: pass --idle")
                continue
            if letter in ("C", "J") and not POSIX and letter not in ONLY:
                skipped(f"{letter} {title}", "Ctrl-C checks need a POSIX signal model")
                continue
            print(f"\n== {letter}: {title}", flush=True)
            t0 = time.time()
            try:
                fn()
            except Exception as e:  # a crashed scenario is a failed scenario
                check(f"{letter} scenario crashed", False, f"{type(e).__name__}: {e}")
            print(f"   ({time.time() - t0:.1f}s)", flush=True)
    finally:
        for proc in MOCKS:
            proc.kill()
        shutil.rmtree(ROOT, ignore_errors=True)
    print(f"\n==== {'ALL PASS' if not failures else 'FAILED'}: {len(failures)} failed ====")
    for name in failures:
        print("   FAIL", name)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "__mock-mcp":
        mock_mcp_main()
    else:
        main()
