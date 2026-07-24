#!/usr/bin/env python3
"""Minimal MCP stdio server (NDJSON, JSON-RPC 2.0) for testing tiny-agent.

Exposes one tool: `echo` — returns its `text` argument back.
Also serves as a reference for the wire protocol tiny-agent implements.

Usage: python3 mcp_echo.py
"""
import json
import sys

PROTOCOL_VERSION = "2025-03-26"

TOOLS = [{
    "name": "echo",
    "description": "Echo back the given text.",
    "inputSchema": {
        "type": "object",
        "properties": {"text": {"type": "string", "description": "Text to echo"}},
        "required": ["text"],
    },
}]


def reply(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()


def main():
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
                "serverInfo": {"name": "mcp-echo", "version": "0.1.0"},
            })
        elif method == "tools/list":
            reply(msg_id, {"tools": TOOLS})
        elif method == "tools/call":
            args = msg.get("params", {}).get("arguments", {})
            reply(msg_id, {
                "content": [{"type": "text", "text": f"echo: {args.get('text', '')}"}],
                "isError": False,
            })
        # notifications (no id) are ignored


if __name__ == "__main__":
    main()
