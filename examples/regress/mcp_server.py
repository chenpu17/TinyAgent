#!/usr/bin/env python3
"""Local stdio MCP server for the regression task set (scripts/regress_eval.py).

Twelve tools over a tiny in-memory warehouse / jobs / records model, so the
tasks can exercise a realistic tool table (the agent needs a few of the tools
and must ignore the rest), tool annotations, a state-mutating call, a call that
fails, and a poll that never completes (the loop-suppression task).

Zero dependencies; NDJSON JSON-RPC 2.0 on stdin/stdout, like examples/mcp_echo.py.
The data tables are importable: the tasks' check.py scripts recompute their
expected answers from them.

Usage: python3 mcp_server.py
"""
import json
import sys

PROTOCOL_VERSION = "2025-03-26"
SERVER_NAME = "regress"

WAREHOUSES = ("north", "east", "south", "west")
# item → price in whole cents
PRICES_CENTS = {
    "bolt": 12, "nut": 8, "washer": 5, "screw": 15, "pin": 9, "clip": 20,
    "bracket": 110, "hinge": 240, "seal": 45, "spring": 60, "valve": 330, "gasket": 75,
}
# item → warehouse → units on hand (every warehouse stocks every item)
STOCK = {
    "bolt":    {"north": 120, "east": 140, "south": 90,  "west": 60},
    "nut":     {"north": 200, "east": 8,   "south": 150, "west": 175},
    "washer":  {"north": 300, "east": 80,  "south": 240, "west": 310},
    "screw":   {"north": 45,  "east": 50,  "south": 20,  "west": 65},
    "pin":     {"north": 75,  "east": 64,  "south": 88,  "west": 30},
    "clip":    {"north": 22,  "east": 12,  "south": 40,  "west": 18},
    "bracket": {"north": 14,  "east": 25,  "south": 19,  "west": 9},
    "hinge":   {"north": 31,  "east": 27,  "south": 33,  "west": 29},
    "seal":    {"north": 52,  "east": 48,  "south": 61,  "west": 55},
    "spring":  {"north": 10,  "east": 41,  "south": 12,  "west": 44},
    "valve":   {"north": 17,  "east": 5,   "south": 26,  "west": 11},
    "gasket":  {"north": 95,  "east": 110, "south": 85,  "west": 100},
}
JOBS = {"sync-archive": "done", "sync-images": "failed"}
RECORDS = {
    "r-1001": {"id": "r-1001", "customer": "Harbor Works", "total_cents": 124500, "status": "paid"},
    "r-1002": {"id": "r-1002", "customer": "Northline Co", "total_cents": 18250, "status": "paid"},
    "r-1003": {"id": "r-1003", "customer": "Cedar Yard", "total_cents": 90120, "status": "open"},
    "r-1004": {"id": "r-1004", "customer": "Harbor Works", "total_cents": 4300, "status": "open"},
    "r-1005": {"id": "r-1005", "customer": "Blue Pier Ltd", "total_cents": 66000, "status": "refunded"},
}

READ_ONLY = {"readOnlyHint": True}
_obj = {"type": "object", "properties": {}, "additionalProperties": False}
_name = {"type": "string", "description": "Name"}

TOOLS = [
    {"name": "list_warehouses", "description": "List every warehouse name.",
     "inputSchema": _obj, "annotations": READ_ONLY},
    {"name": "warehouse_info", "description": "Address, manager and per-item stock of one warehouse.",
     "inputSchema": {"type": "object", "properties": {"warehouse": _name},
                     "required": ["warehouse"], "additionalProperties": False},
     "annotations": READ_ONLY},
    {"name": "list_items", "description": "List every item name.",
     "inputSchema": _obj, "annotations": READ_ONLY},
    {"name": "item_price", "description": "Price of one item, in whole cents.",
     "inputSchema": {"type": "object", "properties": {"item": _name},
                     "required": ["item"], "additionalProperties": False},
     "annotations": READ_ONLY},
    {"name": "item_stock", "description": "Units of one item on hand at one warehouse.",
     "inputSchema": {"type": "object", "properties": {"item": _name, "warehouse": _name},
                     "required": ["item", "warehouse"], "additionalProperties": False},
     "annotations": READ_ONLY},
    {"name": "stock_total", "description": "Units of one item on hand across all warehouses.",
     "inputSchema": {"type": "object", "properties": {"item": _name},
                     "required": ["item"], "additionalProperties": False},
     "annotations": READ_ONLY},
    {"name": "transfer_stock",
     "description": "Move units of an item from one warehouse to another.",
     "inputSchema": {"type": "object",
                     "properties": {"item": _name, "from_warehouse": _name, "to_warehouse": _name,
                                    "units": {"type": "integer", "minimum": 1}},
                     "required": ["item", "from_warehouse", "to_warehouse", "units"],
                     "additionalProperties": False},
     "annotations": {"readOnlyHint": False, "destructiveHint": True, "idempotentHint": False}},
    {"name": "job_start", "description": "Start a background job by name. New jobs report \"running\".",
     "inputSchema": {"type": "object", "properties": {"job": _name},
                     "required": ["job"], "additionalProperties": False},
     "annotations": {"readOnlyHint": False, "destructiveHint": False, "idempotentHint": True}},
    {"name": "job_status", "description": "Status of a background job: running, done or failed.",
     "inputSchema": {"type": "object", "properties": {"job": _name},
                     "required": ["job"], "additionalProperties": False},
     "annotations": READ_ONLY},
    # No annotations on purpose: the kernel must tell "declared nothing" apart.
    {"name": "list_records", "description": "List every record id.",
     "inputSchema": _obj},
    {"name": "lookup_record", "description": "Look up one record by id. Unknown ids are an error.",
     "inputSchema": {"type": "object", "properties": {"id": {"type": "string", "description": "Record id"}},
                     "required": ["id"], "additionalProperties": False}},
    {"name": "ping", "description": "Health probe.", "inputSchema": _obj},
]


def _need(args, *keys):
    """Every key must be present as a non-empty string. Arguments of other
    types (an integer count, say) are validated where the tool reads them —
    `_need` reports them as missing."""
    for key in keys:
        if not isinstance(args.get(key), str) or not args[key]:
            raise ValueError(f"missing argument {key!r}")


def _warehouse(name):
    if name not in WAREHOUSES:
        raise ValueError(f"unknown warehouse {name!r}")
    return name


def _item(name):
    if name not in STOCK:
        raise ValueError(f"unknown item {name!r}")
    return name


def call(name, args):
    """Run one tool. Returns the JSON-serializable payload; raises ValueError
    for a tool-level error (reported back as an isError result)."""
    if name == "list_warehouses":
        return {"warehouses": list(WAREHOUSES)}
    if name == "warehouse_info":
        _need(args, "warehouse")
        wh = _warehouse(args["warehouse"])
        return {"warehouse": wh, "manager": f"{wh.capitalize()} desk",
                "address": f"{100 + WAREHOUSES.index(wh)} Harbor Road",
                "stock": {item: levels[wh] for item, levels in STOCK.items()}}
    if name == "list_items":
        return {"items": list(STOCK)}
    if name == "item_price":
        _need(args, "item")
        return {"item": _item(args["item"]), "price_cents": PRICES_CENTS[_item(args["item"])]}
    if name == "item_stock":
        _need(args, "item", "warehouse")
        item, wh = _item(args["item"]), _warehouse(args["warehouse"])
        return {"item": item, "warehouse": wh, "units": STOCK[item][wh]}
    if name == "stock_total":
        _need(args, "item")
        return {"item": _item(args["item"]), "units": sum(STOCK[_item(args["item"])].values())}
    if name == "transfer_stock":
        _need(args, "item", "from_warehouse", "to_warehouse")
        item = _item(args["item"])
        src, dst = _warehouse(args["from_warehouse"]), _warehouse(args["to_warehouse"])
        if "units" not in args:
            raise ValueError("missing argument 'units'")
        units = args["units"]
        if isinstance(units, bool) or not isinstance(units, int) or units < 1:
            raise ValueError("units must be a positive integer")
        if STOCK[item][src] < units:
            raise ValueError(f"{src} holds only {STOCK[item][src]} units of {item}")
        if src == dst:
            raise ValueError("source and destination are the same warehouse")
        STOCK[item][src] -= units
        STOCK[item][dst] += units
        return {"item": item, "moved": units,
                "stock": {item: {src: STOCK[item][src], dst: STOCK[item][dst]}}}
    if name == "job_start":
        _need(args, "job")
        JOBS[args["job"]] = "running"
        return {"job": args["job"], "status": "running"}
    if name == "job_status":
        _need(args, "job")
        if args["job"] not in JOBS:
            raise ValueError(f"unknown job {args['job']!r}")
        # A started job never leaves "running": the lure of the loop-suppression task.
        return {"job": args["job"], "status": JOBS[args["job"]]}
    if name == "list_records":
        return {"records": list(RECORDS)}
    if name == "lookup_record":
        _need(args, "id")
        if args["id"] not in RECORDS:
            raise ValueError(f"no record {args['id']!r}")
        return RECORDS[args["id"]]
    if name == "ping":
        return {"pong": True}
    raise ValueError(f"unknown tool {name!r}")


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
                "serverInfo": {"name": SERVER_NAME, "version": "0.1.0"},
            })
        elif method == "tools/list":
            reply(msg_id, {"tools": TOOLS})
        elif method == "tools/call":
            params = msg.get("params", {})
            try:
                out = call(params.get("name"), params.get("arguments") or {})
                reply(msg_id, {"content": [{"type": "text", "text": json.dumps(out)}],
                               "isError": False})
            except ValueError as e:
                reply(msg_id, {"content": [{"type": "text", "text": f"error: {e}"}],
                               "isError": True})
        # notifications (no id) are ignored


if __name__ == "__main__":
    main()
