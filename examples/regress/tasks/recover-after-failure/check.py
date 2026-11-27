#!/usr/bin/env python3
"""Pass condition for the `recover-after-failure` regress task.

    python3 check.py <result.json>        (cwd = the run's workdir)

Pass iff the notes are untouched, the command now succeeds, its output is the
one the notes imply (recomputed here, independently of the summarizer), and
`out/summary.txt` holds exactly that output.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.dont_write_bytecode = True  # 不往仓库里落 __pycache__
import checklib  # noqa: E402


def expected_lines(pristine_notes):
    """The output the notes imply, computed without the summarizer's code."""
    lines, total = [], 0
    for file in sorted(pristine_notes.glob("*.txt")):
        count = sum(1 for line in file.read_text(encoding="utf-8").splitlines()
                    if line.lstrip().startswith("- [ ] "))
        lines.append(f"{file.name}: {count} action items")
        total += count
    return lines + [f"total: {total}"]


def reasons(workdir):
    out = checklib.pristine_drift(Path(__file__).resolve().parent, ("notes",))
    proc = checklib.run([sys.executable, "tools/summarize.py", "notes"], cwd=workdir)
    if proc.returncode != 0:
        out.append(f"tools/summarize.py exited {proc.returncode}: "
                   f"{checklib.tail(proc.stdout + proc.stderr)}")
        return out
    produced = proc.stdout.strip().splitlines()
    expected = expected_lines(Path(__file__).resolve().parent / "fixture" / "notes")
    if produced != expected:
        out.append(f"output is {produced!r}, want {expected!r}")
    saved = workdir / "out" / "summary.txt"
    if not saved.is_file():
        out.append("out/summary.txt is missing")
    elif saved.read_text(encoding="utf-8").strip().splitlines() != produced:
        out.append("out/summary.txt does not match the command's output")
    return out


if __name__ == "__main__":
    sys.exit(checklib.finish(reasons(Path.cwd())))
