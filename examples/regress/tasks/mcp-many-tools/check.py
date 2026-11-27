#!/usr/bin/env python3
"""Pass condition for the `mcp-many-tools` regress task.

    python3 check.py <result.json>        (cwd = the run's workdir)

Pass iff `out/restock.json` equals the answer implied by the server's own data
tables (imported here from examples/regress/mcp_server.py, so the expectation
follows the data, never the agent's output).
"""
import json
import sys
from pathlib import Path

TASK_DIR = Path(__file__).resolve().parent
REGRESS_DIR = TASK_DIR.parents[1]
sys.path.insert(0, str(REGRESS_DIR))
sys.dont_write_bytecode = True
import mcp_server  # noqa: E402

LOW_BELOW = 20
TARGET = 20


def expected():
    """The restock plan the data tables imply."""
    items, cost = {}, 0
    for item in mcp_server.STOCK:
        east = mcp_server.STOCK[item]["east"]
        if east >= LOW_BELOW:
            continue
        source = max((w for w in mcp_server.WAREHOUSES if w != "east"),
                     key=lambda w: mcp_server.STOCK[item][w])
        units = TARGET - east
        items[item] = {"units": units, "from": source}
        cost += units * mcp_server.PRICES_CENTS[item]
    return {"items": items, "cost_cents": cost}


def same(a, b):
    """JSON equality without Python's `True == 1` shortcut (a bool is never
    the number the answer asks for). Key order does not matter."""
    if isinstance(a, bool) or isinstance(b, bool):
        return a is b
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(same(a[k], b[k]) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(same(x, y) for x, y in zip(a, b))
    return a == b


def reasons(workdir):
    file = workdir / "out" / "restock.json"
    if not file.is_file():
        return ["out/restock.json is missing"]
    try:
        got = json.loads(file.read_text(encoding="utf-8"))
    except ValueError as e:
        return [f"out/restock.json is not valid JSON: {e}"]
    want = expected()
    if not same(got, want):
        return [f"out/restock.json is {json.dumps(got, sort_keys=True)}, "
                f"want {json.dumps(want, sort_keys=True)}"]
    return []


if __name__ == "__main__":
    sys.path.insert(0, str(TASK_DIR.parents[1]))
    import checklib

    sys.exit(checklib.finish(reasons(Path.cwd())))
