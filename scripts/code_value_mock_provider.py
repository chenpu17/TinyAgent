#!/usr/bin/env python3
"""code_value G0 联通验证的 OpenAI 兼容 SSE mock（方案 §4.2 放行表第 3/4 条）。

与 examples/mock_openai.py 的差别：按场景文件逐轮出牌，而不是固定两步。
零付费装置检查专用——真实网关绝不指向这里之外（runner 在 mock 模式强制
base_url=127.0.0.1 本进程，paid_model_requests 恒 0）。

场景 JSON：
  {"turns": [
     {"kind": "tool_call", "id": "c1", "name": "write_file",
      "arguments": {"path": "...", "content": "..."},
      "usage": {"prompt_tokens": 100, "completion_tokens": 40,
                "cached_tokens": 20}},          # usage 可省 = 缺 usage 用例
     {"kind": "text", "text": "done", "usage": {...}},
     {"kind": "http_error", "status": 500, "body": "boom",
      "usage": {...}},                          # 失败用例（错误前的账单面）
     {"kind": "tool_error", "id": "c2", "name": "run_code",
      "arguments": {"code": "return tools.list()"},
      "result_text": "unknown tool"}            # 工具层错误：以正常 tool_calls
                                                # 响应返回，错误由宿主标注
  ]}
turns 耗尽后再来请求 → 500（场景写错会大声失败，不会静默循环）。

用法：python3 code_value_mock_provider.py --scenario S.json [--port 8799]
端口 0 = 内部分配，实际端口打印在 stderr 首行（runner 解析）。
"""
import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def sse(chunks):
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"


def tool_call_chunk(cid, name, args_json):
    return {"choices": [{"index": 0, "finish_reason": None, "delta": {
        "role": "assistant",
        "tool_calls": [{"index": 0, "id": cid, "type": "function",
                        "function": {"name": name, "arguments": args_json}}]}}]}


def usage_frame(usage):
    # OpenAI 形状：prompt_tokens 是总量（含缓存），cached_tokens 是子集；
    # 宿主解析层会把 input 拆成 non-cached + cache_read（openai.rs 语义）
    return {"choices": [{"index": 0, "finish_reason": "stop", "delta": {}}],
            "usage": {"prompt_tokens": usage["prompt_tokens"],
                      "completion_tokens": usage["completion_tokens"],
                      "prompt_tokens_details": {
                          "cached_tokens": usage.get("cached_tokens", 0)}}}


def build_turn(turn):
    """一个场景条目 → SSE 文本（或 (status, body) 的 HTTP 错误）。"""
    kind = turn["kind"]
    if kind == "tool_call":
        chunks = [tool_call_chunk(turn["id"], turn["name"],
                                  json.dumps(turn["arguments"]))]
        chunks.append({"choices": [{"index": 0, "finish_reason": "tool_calls",
                                    "delta": {}}]})
        if turn.get("usage"):
            chunks.append(usage_frame(turn["usage"]))
        return 200, sse(chunks)
    if kind == "text":
        chunks = [{"choices": [{"index": 0, "finish_reason": None, "delta": {
            "role": "assistant", "content": turn["text"]}}]},
                  {"choices": [{"index": 0, "finish_reason": "stop",
                                "delta": {}}]}]
        if turn.get("usage"):
            chunks.append(usage_frame(turn["usage"]))
        return 200, sse(chunks)
    if kind == "http_error":
        return turn.get("status", 500), turn.get("body", "mock http error")
    raise ValueError(f"unknown scenario kind: {kind}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--scenario", required=True)
    ap.add_argument("--port", type=int, default=0)
    args = ap.parse_args()
    turns = json.load(open(args.scenario, encoding="utf-8"))["turns"]
    lock = threading.Lock()
    state = {"next": 0}

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            if self.path != "/chat/completions":
                self.send_error(404)
                return
            self.rfile.read(int(self.headers["Content-Length"]))
            with lock:
                i = state["next"]
                state["next"] += 1
            if i >= len(turns):
                body = f"scenario exhausted at turn {i}"
                payload = body.encode()
                self.send_response(500)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return
            status, out = build_turn(turns[i])
            if status != 200:
                payload = str(out).encode()
                self.send_response(status)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return
            payload = out.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *a):
            pass

    srv = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"{srv.server_address[1]}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
