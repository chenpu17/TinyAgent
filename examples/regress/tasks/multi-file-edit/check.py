#!/usr/bin/env python3
"""Pass condition for the `multi-file-edit` regress task.

    python3 check.py <result.json>        (cwd = the run's workdir)

Pass iff the test suite and the runner are byte-identical to the fixture and
the whole suite passes. `result.json` is accepted for interface uniformity.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.dont_write_bytecode = True  # 不往仓库里落 __pycache__
import checklib  # noqa: E402

EXPECTED = "tests: 10, failures: 0, errors: 0"


def reasons(workdir):
    out = checklib.pristine_drift(Path(__file__).resolve().parent, ("tests", "run_tests.py"))
    proc = checklib.run([sys.executable, "run_tests.py"], cwd=workdir)
    lines = proc.stdout.strip().splitlines()
    if proc.returncode != 0:
        out.append(f"run_tests.py exited {proc.returncode}: "
                   f"{checklib.tail(proc.stdout or proc.stderr)}")
    summary = lines[-1] if lines else ""
    if summary != EXPECTED:
        out.append(f"summary line is {summary!r}, want {EXPECTED!r}")
    return out


if __name__ == "__main__":
    sys.exit(checklib.finish(reasons(Path.cwd())))
