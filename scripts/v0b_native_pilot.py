#!/usr/bin/env python3
"""V0-B native pilot (first slice): drive the REAL tiny-agent binary through
the E0 / E2 / E3 arms of docs/code_llm_eval_assembly.md on the V0-A ticket
corpus, against the real test gateway (测试模型.md).

What this closes that V0-A could not: V0-A's routes ③④ were SIMULATED in
python (scripts/v0a_eval.py drove single-shot calls itself). Here the whole
route — corpus reading, lib drafting (saveLib), boundary judging via the
native `llm` tool, callBatch fan-out — runs through the shipped binary's
agent loop + run_code sandbox + gates. The task text is method-neutral
(state the goal, not the route); the route is the model's own choice under
each arm's gate set, steered only by agentic-code.md + the built-in skill.

Pilot scope, honestly: ONE round per arm, no interleaving, no cascade tier
(single inner model), synthetic corpus (template-pool caveat inherited from
V0-A), gateway has no monetary pricing (token + wall-clock only). The full
V0-B campaign (≥3 repeats, interleaved, lifecycle probes, four-route
comparison) is NOT this script's claim.

Usage:
  python3 scripts/v0b_native_pilot.py --smoke     # 6-ticket mini holdout
  python3 scripts/v0b_native_pilot.py --arm e2     # one full arm
  python3 scripts/v0b_native_pilot.py --all        # e0, e2, e3 sequential
Artifacts: v0b_pilot/<arm>/ (config, events.ndjson, result.json,
predictions.jsonl, summary.json, run.log)
"""
import argparse
import collections
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "release" / "tiny-agent"
ART = ROOT / "v0b_pilot"
V0A = ROOT / "v0a_artifacts"
VOCAB = ["bug", "feature", "account", "billing", "howto", "complaint", "unsure"]

OUTER_MODEL = "glm-5.3-flash"
INNER_MODEL = "deepseek-flash"   # single tier — no cascade in the pilot

TASK_TEXT = (
    "workdir 里有 dev.jsonl（带 label 金标的历史工单）与 holdout.jsonl（待分类工单）。"
    "请为 holdout 的每条工单判定 label，词表固定为 bug / feature / account / billing "
    "/ howto / complaint / unsure。判定标准从 dev 的金标归纳。"
    "把结果写到 predictions.jsonl：每行一个 JSON 对象 {\"id\": \"...\", \"label\": \"...\"}，"
    "行数与 holdout 完全一致、id 一一对应、label 必须在词表内。"
    "完成后只回复紧凑摘要（总条数、各类计数、准确率自估）。"
)

ARMS = {
    # gate sets per the assembly doc's E-matrix. E1 (include-only) is a
    # separate arm the full campaign runs; the pilot takes the three that
    # bracket the llm capability question.
    "e0": {"include": False, "llm": False, "call_batch": False},
    "e2": {"include": True, "llm": True, "call_batch": False},
    "e3": {"include": True, "llm": True, "call_batch": True},
    # Mechanism probe — NOT an E-matrix arm. The E2 run showed glm-5.3-flash
    # routes this corpus to shell+python rules and never touches llm even with
    # the gate on; before drawing routing conclusions, this forced-route run
    # validates the native llm/callBatch path itself in the field (first
    # real-gateway use of the M3 primitive) and yields the route's numbers
    # when taken. Explicit-instruction upper bound, never a matrix arm.
    "probe": {"include": True, "llm": True, "call_batch": True},
}

PROBE_TASK = (
    "用 run_code 沙箱完成本次分类（先 read_skill 加载 run-code 技能）。判别策略分层："
    "先从 dev.jsonl 金标归纳确定性规则（纯 JS，在沙箱里跑）；规则覆盖不了的剩余条目"
    "逐项用 tools.call('llm', {...}) 判别——labels 声明固定词表、prompt 里同写"
    "'输出必须是这些词之一'、max_tokens 设小（≤64）；条目多时用 tools.callBatch "
    "并发跑（8/波）。holdout 全部 288 条都要有 label（规则或 llm），"
    "写 predictions.jsonl（每行 {\"id\", \"label\"}）。完成后只回复紧凑摘要。"
)


def gateway():
    kv = {}
    for line in (ROOT / "测试模型.md").read_text(encoding="utf-8").splitlines():
        m = re.match(r'\s*(?:#\s*)?([^=]+?)\s*=\s*"(.*)"\s*$', line)
        if m:
            kv[m.group(1).strip()] = m.group(2).strip()
    return kv["base url"], kv["api key"]


def agentic_code_prompt():
    body = (ROOT / "examples/prompts/agentic-code.md").read_text(encoding="utf-8")
    return body.split("\n---\n", 1)[1].strip()


def stage_workdir(arm, n_holdout):
    wd = ART / arm / "wd"
    if wd.exists():
        subprocess.run(["rm", "-rf", str(wd)], check=True)
    wd.mkdir(parents=True)
    dev = [json.loads(l) for l in open(V0A / "tickets_dev.jsonl", encoding="utf-8")]
    hold = [json.loads(l) for l in open(V0A / "tickets_holdout.jsonl", encoding="utf-8")]
    hold = hold[:n_holdout]
    with open(wd / "dev.jsonl", "w", encoding="utf-8") as f:
        for r in dev:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    with open(wd / "holdout.jsonl", "w", encoding="utf-8") as f:
        for r in hold:
            # strip the gold — the task must not see answers
            f.write(json.dumps({k: v for k, v in r.items() if k != "label"},
                               ensure_ascii=False) + "\n")
    gold = {r["id"]: r["label"] for r in hold}
    (ART / arm / "gold.json").write_text(json.dumps(gold, ensure_ascii=False))
    return wd, gold


def write_config(arm, wd):
    base, key = gateway()
    g = ARMS[arm]
    cfg = {
        "provider": "openai", "base_url": base, "api_key_env": "V0B_KEY",
        # 4096 after the E2 first run ended stop_reason=max_tokens at 2048
        # (glm thinking-burn pattern, V0-A finding #1 at the outer layer);
        # predictions were complete, only the summary turn was cut.
        "model": OUTER_MODEL, "max_tokens": 4096,
        "system_prompt": agentic_code_prompt(),
        "workdir": str(wd),
        "tools": ["shell", "read_file", "write_file", "edit_file", "list_dir"],
        "inject_environment": False,
        "limits": {"max_turns": 120, "timeout_secs": 5400,
                   "max_output_chars": 60000, "max_total_tokens": 4_000_000},
        "code": {"enabled": True, "skill_injection": "on_demand",
                 "include": g["include"], "call_batch": g["call_batch"]},
    }
    if g["llm"]:
        cfg["code"]["llm"] = {"enabled": True, "default_model": INNER_MODEL,
                              "models": [INNER_MODEL]}
    p = ART / arm / "config.json"
    p.write_text(json.dumps(cfg, ensure_ascii=False, indent=2))
    return p


def run_arm(arm, n_holdout, timeout_s):
    wd, gold = stage_workdir(arm, n_holdout)
    cfg = write_config(arm, wd)
    env = dict(os.environ, V0B_KEY=gateway()[1])
    ev_path = ART / arm / "events.ndjson"
    t0 = time.time()
    task = PROBE_TASK if arm == "probe" else TASK_TEXT
    with open(ev_path, "w") as evf, open(ART / arm / "run.log", "w") as logf:
        p = subprocess.run(
            [str(BIN), "--config", str(cfg), "--task", task,
             "--session", "none", "--json", "--output", str(ART / arm / "result.json")],
            stdout=evf, stderr=logf, env=env, timeout=timeout_s)
    wall = round(time.time() - t0, 1)
    return {"arm": arm, "rc": p.returncode, "wall": wall, "gold": gold, "wd": wd}


def parse_events(path):
    evs = []
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if line.startswith("{"):
            try:
                evs.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return evs


def score(arm, gold):
    pred_path = ART / arm / "wd" / "predictions.jsonl"
    preds = {}
    if pred_path.exists():
        for line in open(pred_path, encoding="utf-8"):
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
                preds[r["id"]] = r.get("label")
            except (json.JSONDecodeError, KeyError):
                pass
    recs = []
    for tid, label in gold.items():
        pred = preds.get(tid)
        recs.append({"id": tid, "label": label,
                     "pred": pred if pred in VOCAB else None,
                     "valid": pred in VOCAB})
    tp, fp, fn = collections.Counter(), collections.Counter(), collections.Counter()
    for r in recs:
        if r["pred"] is not None and r["pred"] == r["label"]:
            tp[r["label"]] += 1
        else:
            fn[r["label"]] += 1
            if r["pred"] is not None:
                fp[r["pred"]] += 1
    f1s = []
    for c in VOCAB:
        p = tp[c] / (tp[c] + fp[c]) if tp[c] + fp[c] else 0.0
        rl = tp[c] / (tp[c] + fn[c]) if tp[c] + fn[c] else 0.0
        f1s.append(2 * p * rl / (p + rl) if p + rl else 0.0)
    n = len(recs)
    valid = sum(r["valid"] for r in recs)
    correct = sum(r["pred"] == r["label"] for r in recs)
    return {
        "n": n, "answered": len(preds), "valid_rate": round(valid / n, 4),
        "acc": round(correct / n, 4), "macro_f1": round(sum(f1s) / len(f1s), 4),
        "recall": {c: round(tp[c] / (tp[c] + fn[c]), 3) if tp[c] + fn[c] else None
                   for c in VOCAB},
        "missing": n - len(preds),
    }


def cost(arm):
    """Outer usage from result.json; inner llm from the run_code meta ledger."""
    out = {"outer": None, "inner": None, "turns": None, "tool_calls": None,
           "stop_reason": None}
    rp = ART / arm / "result.json"
    inner = {"calls": 0, "settled": 0, "unknown": 0, "models": {}}
    if rp.exists():
        r = json.load(open(rp, encoding="utf-8"))
        # result.json carries usage at the TOP LEVEL (input_tokens /
        # output_tokens / cache_*), not under a "usage" key.
        out["outer"] = {"in": r.get("input_tokens"), "out": r.get("output_tokens"),
                        "cache_w": r.get("cache_creation_input_tokens"),
                        "cache_r": r.get("cache_read_input_tokens")}
        out["turns"] = r.get("turns")
        out["tool_calls"] = len(r.get("tool_calls") or [])
        out["stop_reason"] = r.get("stop_reason")
        # The LD-12 llm ledger rides each run_code entry's audit META in
        # result.json (NOT the NDJSON tool_result content — that is the
        # guest's return value). Sum across all run_code calls.
        for tc in r.get("tool_calls") or []:
            if tc.get("name") != "run_code":
                continue
            llm = (tc.get("meta") or {}).get("llm")
            if not llm:
                continue
            inner["calls"] += llm.get("calls", 0)
            inner["settled"] += llm.get("settled", 0)
            for m, s in (llm.get("models") or {}).items():
                agg = inner["models"].setdefault(m, {"known_usage": 0, "known_calls": 0})
                agg["known_usage"] += s.get("known_usage", 0)
                agg["known_calls"] += s.get("known_calls", 0)
                inner["unknown"] += s.get("unknown_calls", 0)
    out["inner"] = inner if inner["calls"] else None
    return out


def lifecycle_notes(arm):
    """What the native lib channel actually did (saveLib usage, INDEX.md)."""
    notes = {}
    idx = ART / arm / "wd" / "lib" / "INDEX.md"
    if idx.exists():
        notes["index"] = idx.read_text(encoding="utf-8")
    libs = sorted(str(p.relative_to(ART / arm / "wd"))
                  for p in (ART / arm / "wd" / "lib").glob("*.js")) if idx.exists() else []
    notes["libs"] = libs
    return notes if notes.get("index") else {"libs": []}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--smoke", action="store_true")
    mode.add_argument("--arm", choices=list(ARMS))
    mode.add_argument("--all", action="store_true")
    args = ap.parse_args()
    if not BIN.exists():
        sys.exit(f"missing {BIN} — cargo build --release --features code")
    ART.mkdir(exist_ok=True)
    arms = ["e0", "e2", "e3"] if args.all else ([args.arm] if args.arm else ["e2"])
    n = 6 if args.smoke else 288
    timeout_s = 600 if args.smoke else 5400
    for arm in arms:
        print(f"[v0b] arm {arm} ({n} holdout tickets) ...", flush=True)
        try:
            r = run_arm(arm, n, timeout_s)
        except subprocess.TimeoutExpired:
            print(f"[v0b] arm {arm} TIMED OUT after {timeout_s}s", flush=True)
            continue
        s = score(arm, r["gold"])
        c = cost(arm)
        summary = {"arm": arm, "n": n, "rc": r["rc"], "wall_s": r["wall"],
                   "score": s, "cost": c, "lifecycle": lifecycle_notes(arm)}
        (ART / arm / "summary.json").write_text(
            json.dumps(summary, ensure_ascii=False, indent=2))
        print(json.dumps(summary, ensure_ascii=False, indent=2), flush=True)
        if args.smoke:
            print("[v0b] smoke done — inspect v0b_pilot/<arm>/summary.json")
            return
    print("[v0b] all requested arms done — summaries under v0b_pilot/*/summary.json")


if __name__ == "__main__":
    main()
