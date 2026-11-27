#!/usr/bin/env python3
"""Helpers shared by the check.py scripts of examples/regress/tasks/*/.

A check.py is the machine-decidable pass condition of one regress task.
scripts/regress_eval.py runs it after the agent, as

    python3 <task dir>/check.py <result.json>      (cwd = the run's workdir)

Exit status 0 = pass. Everything it needs to compare against comes from the
task's own directory (the pristine `fixture/`, the server's data tables), so
whatever the agent left in the workdir cannot bend the verdict.
"""
import hashlib
import os
import subprocess
import sys
from pathlib import Path


def pristine_drift(task_dir, paths):
    """Why the workdir's copy of `paths` differs from the task's `fixture/`.

    `paths` are relative names inside the fixture (a file or a directory).
    Returns a list of human-readable reasons; empty means byte-identical
    (extra files the agent created, such as __pycache__, are ignored).
    """
    reasons = []
    fixture = Path(task_dir) / "fixture"
    for rel in paths:
        src = fixture / rel
        if src.is_dir():
            for file in sorted(p for p in src.rglob("*") if p.is_file()):
                name = file.relative_to(fixture).as_posix()
                reasons += _differs(fixture / file, Path.cwd() / name)
        elif src.is_file():
            reasons += _differs(src, Path.cwd() / rel)
        else:
            reasons.append(f"fixture has no {rel!r}")
    return reasons


def _differs(pristine, actual):
    if not actual.is_file():
        return [f"{pristine.name} is missing"]
    if hashlib.sha256(pristine.read_bytes()).digest() != hashlib.sha256(actual.read_bytes()).digest():
        return [f"{pristine.name} was modified (must stay as shipped)"]
    return []


def run(cmd, cwd, timeout=120):
    """Run a command in the workdir and capture its output. A timeout comes
    back as exit status 124 instead of an exception."""
    env = os.environ.copy()
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["PYTHONIOENCODING"] = "utf-8"
    try:
        return subprocess.run(cmd, cwd=str(cwd), capture_output=True, text=True,
                              timeout=timeout, env=env)
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(cmd, 124, "", f"timed out after {timeout}s")


def tail(text, limit=300):
    text = (text or "").strip()
    return text[-limit:]


def finish(reasons):
    """Print the verdict and return the exit status."""
    if reasons:
        print("FAIL: " + "; ".join(reasons))
        return 1
    print("PASS")
    return 0
