#!/usr/bin/env python3
"""Analyze the four-arm routing matrix campaign (offline, read-only).

Per protocol docs/run_code_routing_analysis.md §6:
- conv_growth = sum_uncached - req1_unc is the primary cross-run metric
  (cache-robust); req1_unc flags cold/warm provider cache per run
- growth outliers with a double-billed prefix (req-level anomaly) are
  reported, medians taken over repeats per task, tasks as independent units
- judged FAILs and cheat flags are first-class data, never dropped
"""
import argparse
import json
import statistics as st
from pathlib import Path

ARM_ORDER = ["ta-mx-old-aw", "ta-mx-old-eager-aw", "ta-mx-v2-aw", "ta-mx-v2-eager-aw"]
ARM_SHORT = {"ta-mx-old-aw": "A", "ta-mx-old-eager-aw": "B",
             "ta-mx-v2-aw": "C", "ta-mx-v2-eager-aw": "D"}
# 任务形状预标注（docs/run_code_routing_analysis.md §5）
SHAPE = {
    "awlikes": "S", "awsum": "S", "awphoto": "S",
    "awlikeall": "M+J", "awthank": "M+J",
}


def run_metrics(run_dir: Path):
    run = json.loads((run_dir / "run.json").read_text())
    res = json.loads((run_dir / "wd" / "result.json").read_text())
    proxy = [json.loads(l) for l in (run_dir / "proxy.jsonl").read_text().strip().splitlines()
             if json.loads(l).get("usage")]
    unc = [p["usage"]["prompt_tokens"]
           - (p["usage"].get("prompt_tokens_details") or {}).get("cached_tokens", 0)
           for p in proxy]
    req1 = unc[0]
    # run 内前缀二次计费检测：除首请求外仍有 >8K 的 uncached 请求 → growth 不可比
    re_billed = sum(1 for u in unc[1:] if u > 8000)
    calls = res["tool_calls"]
    return {
        "verdict": run.get("verdict"), "cheat": bool(run.get("cheat_flags")),
        "turns": res["turns"],
        "growth": sum(unc) - req1, "req1_unc": req1, "warm": req1 < 2000,
        "rebilled_prefix": re_billed,
        "output": res["output_tokens"],
        "wall": run["wall_segs"]["client"],
        "attempts": run.get("attempts", 1),
        "run_code": sum(1 for c in calls if c["name"] == "run_code"),
        "read_skill": sum(1 for c in calls if c["name"] == "read_skill"),
        "outer_calls": len(calls),
        "gen_s": sum(p["dur_ms"] for p in proxy) / 1000,
        "llm_reqs": len(proxy),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bench-root", type=Path, default=Path(__file__).resolve().parents[1].parent / "TinyAgentBench")
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()
    runs = args.bench_root / "results" / "runs"

    per_arm_task = {a: {} for a in ARM_ORDER}
    missing = []
    for arm in ARM_ORDER:
        for task_dir in sorted((runs / arm).iterdir()):
            if not task_dir.is_dir():
                continue
            reps = {}
            for rep_dir in sorted(task_dir.iterdir()):
                run_json = rep_dir / "run.json"
                if not run_json.exists():
                    missing.append(f"{arm}/{task_dir.name}/{rep_dir.name} (no run.json)")
                    continue
                try:
                    if not json.loads(run_json.read_text()).get("complete"):
                        missing.append(f"{arm}/{task_dir.name}/{rep_dir.name} (incomplete/infra)")
                        continue
                except json.JSONDecodeError:
                    missing.append(f"{arm}/{task_dir.name}/{rep_dir.name} (bad run.json)")
                    continue
                try:
                    reps[rep_dir.name] = run_metrics(rep_dir)
                except (OSError, KeyError, json.JSONDecodeError) as exc:
                    missing.append(f"{arm}/{task_dir.name}/{rep_dir.name} (unreadable: {exc})")
            per_arm_task[arm][task_dir.name] = reps

    def task_med(arm, task, key):
        reps = per_arm_task.get(arm, {}).get(task, {})
        vals = [r[key] for r in reps.values()]
        return st.median(vals) if vals else None

    tasks = sorted({t for a in ARM_ORDER for t in per_arm_task[a]})
    print(f"{'task':10s} {'shape':5s} | " + " | ".join(
        f"{ARM_SHORT[a]} pss {'grw':>6s} {'out':>5s} {'trn':>5s} {'wal':>4s} {'rc':>2s}"
        for a in ARM_ORDER))
    agg = {a: {"pass": [], "growth": [], "output": [], "turns": [], "wall": []}
           for a in ARM_ORDER}
    for t in tasks:
        cells = []
        for a in ARM_ORDER:
            reps = per_arm_task.get(a, {}).get(t, {})
            npass = sum(1 for r in reps.values() if r["verdict"] == "PASS")
            cells.append(f"{npass}/{len(reps)}  {task_med(a,t,'growth') or 0:>6.0f} "
                         f"{task_med(a,t,'output') or 0:>5.0f} {task_med(a,t,'turns') or 0:>5.0f} "
                         f"{task_med(a,t,'wall') or 0:>4.0f} {task_med(a,t,'run_code') or 0:>2.0f}")
            if reps:
                agg[a]["pass"].append(npass / len(reps))
                for k in ("growth", "output", "turns", "wall"):
                    v = task_med(a, t, k)
                    if v is not None:
                        agg[a][k].append(v)
        print(f"{t:10s} {SHAPE.get(t,'M'):5s} | " + " | ".join(cells))

    print("\n== per-arm aggregate (median over task-medians; tasks as units)")
    for a in ARM_ORDER:
        n = len(agg[a]["pass"])
        print(f"{ARM_SHORT[a]} {a:22s} n_tasks={n:2d} pass={st.mean(agg[a]['pass']):.0%} "
              f"growth={st.median(agg[a]['growth']):.0f} output={st.median(agg[a]['output']):.0f} "
              f"turns={st.median(agg[a]['turns']):.1f} wall={st.median(agg[a]['wall']):.0f}s")

    print("\n== shape subsets (S = small/negative, M+J = semantic-set)")
    for shape in ("S", "M+J"):
        sub = [t for t in tasks if SHAPE.get(t) == shape]
        if not sub:
            continue
        print(f"[{shape}] " + ", ".join(sub))
        for a in ARM_ORDER:
            passes = [sum(1 for r in per_arm_task.get(a, {}).get(t, {}).values() if r["verdict"] == "PASS")
                      / max(1, len(per_arm_task.get(a, {}).get(t, {})))
                      for t in sub if per_arm_task.get(a, {}).get(t)]
            print(f"  {ARM_SHORT[a]} pass={st.mean(passes):.0%}" if passes else f"  {ARM_SHORT[a]} (no data)")

    anomalies = [(a, t, rp, m) for a in ARM_ORDER for t, reps in per_arm_task[a].items()
                 for rp, m in reps.items()
                 if m["rebilled_prefix"] or m["cheat"] or m["attempts"] > 1]
    if anomalies:
        print("\n== anomalies (rebilled prefix / cheat flags / retried walls)")
        for a, t, rp, m in anomalies:
            why = []
            if m["rebilled_prefix"]:
                why.append(f"rebilled={m['rebilled_prefix']}")
            if m["cheat"]:
                why.append("cheat")
            if m["attempts"] > 1:
                why.append(f"attempts={m['attempts']}(wall inflated)")
            print(f"  {a}/{t}/{rp} {' '.join(why)}")
    if missing:
        print(f"\n== incomplete runs: {len(missing)}")
        for m in missing[:12]:
            print(f"  {m}")

    if args.out:
        payload = {a: {t: reps for t, reps in per_arm_task[a].items()} for a in ARM_ORDER}
        args.out.write_text(json.dumps(payload, ensure_ascii=False, indent=1) + "\n")
        print(f"\nper-run metrics written: {args.out}")


if __name__ == "__main__":
    main()
