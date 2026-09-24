#!/usr/bin/env python3
"""code_value T2 的 stdio MCP mock（两个逻辑数据源一个进程，方案 §2）。

协议骨架复用 examples/mcp_echo.py（NDJSON + JSON-RPC 2.0）。暴露两个分页工具：

  subscriptions_page({page})  订阅源，每页 20 条，记录字段 user_id/plan/status
  events_page({page})         事件源，每页 20 条，记录字段 event_id/uid/…
                              severity/confirmed（join 字段刻意叫 uid）

返回 text content 是一个 JSON 串：
  {"source": ..., "page": p, "page_size": 20, "total_pages": N,
   "total_records": M, "has_more": bool, "records": [...]}

纪律：固定快照、稳定排序、正常游标、无随机故障（方案 §2 实施约束）；
页号超范围返回 isError + 明确文案。--log 把每次 tools/call 追加成 NDJSON
（server_events 证据：MCP 实际调用数）。

用法：python3 code_value_mcp_server.py --subscriptions S.json --events E.json [--log L.ndjson]
"""
import argparse
import json
import sys
import time
from pathlib import Path

PROTOCOL_VERSION = "2025-03-26"
PAGE_SIZE = 20


def load(path):
    return json.load(open(path, encoding="utf-8"))


def paged(source, records, page):
    total_pages = (len(records) + PAGE_SIZE - 1) // PAGE_SIZE
    chunk = records[(page - 1) * PAGE_SIZE: page * PAGE_SIZE]
    return {
        "source": source,
        "page": page,
        "page_size": PAGE_SIZE,
        "total_pages": total_pages,
        "total_records": len(records),
        "has_more": page < total_pages,
        "records": chunk,
    }


def build_tools():
    subs_desc = (
        "List subscriptions, 20 records per page. Record fields: "
        "user_id, plan (paid|free), status (active|cancelled|expired). "
        "Returns {page, page_size, total_pages, total_records, has_more, records}."
    )
    events_desc = (
        "List events, 20 records per page. Record fields: event_id, uid, "
        "severity (int), confirmed (bool). NOTE: the join field is `uid`, "
        "corresponding to the subscriptions source's `user_id`. "
        "Returns {page, page_size, total_pages, total_records, has_more, records}."
    )
    page_schema = {
        "type": "object",
        "properties": {"page": {"type": "integer",
                                "description": "1-based page number"}},
        "required": ["page"],
    }
    return [
        {"name": "subscriptions_page", "description": subs_desc,
         "inputSchema": page_schema},
        {"name": "events_page", "description": events_desc,
         "inputSchema": page_schema},
    ]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--subscriptions", required=True)
    ap.add_argument("--events", required=True)
    ap.add_argument("--log", default=None,
                    help="append each tools/call as NDJSON (server-side evidence)")
    args = ap.parse_args()

    data = {"subscriptions_page": load(args.subscriptions),
            "events_page": load(args.events)}

    def reply(msg_id, result):
        sys.stdout.write(json.dumps(
            {"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
        sys.stdout.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = msg.get("method", "")
        msg_id = msg.get("id")
        if method == "initialize":
            reply(msg_id, {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "code-value-t2", "version": "0.1.0"},
            })
        elif method == "tools/list":
            reply(msg_id, {"tools": build_tools()})
        elif method == "tools/call":
            params = msg.get("params", {})
            name = params.get("name", "")
            page_args = params.get("arguments", {}).get("page")
            if args.log:
                with open(args.log, "a", encoding="utf-8") as lf:
                    lf.write(json.dumps({"ts": time.time(), "tool": name,
                                         "page": page_args}) + "\n")
            if name not in data:
                reply(msg_id, {"content": [{"type": "text",
                                            "text": f"unknown tool: {name}"}],
                               "isError": True})
                continue
            if not isinstance(page_args, int) or isinstance(page_args, bool) \
                    or page_args < 1:
                reply(msg_id, {
                    "content": [{"type": "text",
                                 "text": "invalid page: pass an integer >= 1"}],
                    "isError": True})
                continue
            total_pages = (len(data[name]) + PAGE_SIZE - 1) // PAGE_SIZE
            if page_args > total_pages:
                reply(msg_id, {
                    "content": [{"type": "text",
                                 "text": f"page {page_args} out of range: "
                                         f"{name} has {total_pages} pages"}],
                    "isError": True})
                continue
            source = "subscriptions" if name == "subscriptions_page" else "events"
            reply(msg_id, {
                "content": [{"type": "text",
                             "text": json.dumps(
                                 paged(source, data[name], page_args),
                                 ensure_ascii=False)}],
                "isError": False,
            })
        # notifications (no id) are ignored


if __name__ == "__main__":
    main()
