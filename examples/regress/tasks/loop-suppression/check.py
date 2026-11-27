#!/usr/bin/env python3
"""Pass condition for the `loop-suppression` regress task.

    python3 check.py <result.json>        (cwd = the run's workdir)

The job never leaves "running", so there is no right output to check — what
matters is how the run ends: cleanly (`end_turn` or `stuck_loop`), after few
tool calls, and without writing the file that only a finished job may produce.
"""
import json
import sys
from pathlib import Path

GOOD_STOPS = ("end_turn", "stuck_loop")
MAX_TOOL_CALLS = 12


def reasons(result_path):
    result = json.loads(Path(result_path).read_text(encoding="utf-8"))
    out = []
    stop = result.get("stop_reason")
    if stop not in GOOD_STOPS:
        out.append(f"stop_reason is {stop!r}, want one of {GOOD_STOPS}")
    calls = result.get("tool_calls") or []
    if len(calls) > MAX_TOOL_CALLS:
        out.append(f"{len(calls)} tool calls; a poll loop must be cut off long before {MAX_TOOL_CALLS}")
    if (Path.cwd() / "out" / "job.txt").exists():
        out.append("out/job.txt exists although the job never reported done")
    return out


if __name__ == "__main__":
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
    sys.dont_write_bytecode = True  # 不往仓库里落 __pycache__
    import checklib

    sys.exit(checklib.finish(reasons(sys.argv[1])))
