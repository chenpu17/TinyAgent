#!/usr/bin/env python3
"""Pass condition for the `long-output` regress task.

    python3 check.py <result.json>        (cwd = the run's workdir)

Pass iff `data/` is exactly what setup.py generated (the agent changed
nothing), `out/counts.tsv` holds the true counts (taken from setup.py's table,
not from anything the agent wrote), and the run really went through the long
conversation the task is about: at least MIN_COMPRESSIONS history
compressions are on record.
"""
import importlib.util
import json
import sys
from pathlib import Path

TASK_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(TASK_DIR.parents[1]))
sys.dont_write_bytecode = True  # 不往仓库里落 __pycache__（setup.py 也是 import 进来的）
import checklib  # noqa: E402

# 判卷线是「至少真的压缩过一次、结果仍然全对」，不要求更多：内核修复之后一次折叠折得多，
# 正确完成的 run 常常只折叠 1 次（docs/kernel_improvement_plan.md §0.0.5）。
MIN_COMPRESSIONS = 1


def setup_module():
    spec = importlib.util.spec_from_file_location("regress_long_output_setup", TASK_DIR / "setup.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def reasons(workdir, result_path):
    setup = setup_module()
    data = setup.build()
    issues = []
    for name, text in data.items():
        file = workdir / "data" / name
        if not file.is_file():
            issues.append(f"data/{name} is missing")
        elif file.read_text(encoding="utf-8") != text:
            issues.append(f"data/{name} was modified (must stay as generated)")
    want = [f"{name}\t{text.count(' SIGNAL ')}" for name, text in sorted(data.items())]
    want.append(f"total\t{sum(setup.COUNTS)}")
    counts = workdir / "out" / "counts.tsv"
    if not counts.is_file():
        issues.append("out/counts.tsv is missing")
    else:
        got = [line.rstrip("\r") for line in
               counts.read_text(encoding="utf-8").splitlines() if line.strip()]
        if got != want:
            wrong = [f"line {no}: {line!r}, want {expected!r}"
                     for no, (line, expected) in enumerate(zip(got, want), 1) if line != expected]
            if len(got) != len(want):
                # 行数放最前：多出来的行数多半就是后面一串错行的原因
                wrong.insert(0, f"{len(got)} lines, want {len(want)}")
            issues.append("out/counts.tsv is wrong: " + "; ".join(wrong[:3]))
    result = json.loads(Path(result_path).read_text(encoding="utf-8"))
    compressions = result.get("compressions") or []
    if len(compressions) < MIN_COMPRESSIONS:
        issues.append(f"{len(compressions)} history compressions on record, want at least "
                      f"{MIN_COMPRESSIONS} (the task is meant to run through a long conversation)")
    return issues


if __name__ == "__main__":
    sys.exit(checklib.finish(reasons(Path.cwd(), sys.argv[1])))
