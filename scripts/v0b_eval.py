#!/usr/bin/env python3
"""V0-B 正式轮 runner（配方 = docs/code_llm_eval_assembly.md §2/§2.1/§5）。

与试点（scripts/v0b_native_pilot.py，单轮）的区别：多臂交错重复、E0′ 装置
部署 + 运行期指纹门、语料版本戳、逐 run 聚合。臂定义（装配文档 §2）：

  e0  门全关 + 内置技能                  现行基线
  e0p 门全关 + E0′ 磁盘技能（三问边界句）  文案/路由效应（主假设臂）
  e1  e0p 的门 + include=true + 内置技能  库复用增量（cold；warm 用 --warm-lib）
  e2  e1 + llm                            语义批量
  e3  e2 + call_batch                     并发墙钟

用法：
  python3 scripts/v0b_eval.py --arms e0,e0p --repeats 3   # 先答路由主假设
  python3 scripts/v0b_eval.py --arms e0,e0p,e1,e2,e3 --repeats 3
  python3 scripts/v0b_eval.py --arms e1 --warm-lib v0b_eval/e1-r0/wd/lib

工件：v0b_eval/<arm>-r<rep>/{config,events.ndjson,result.json,run.log,
summary.json} + v0b_eval/agg.json（跨重复聚合）。语料 v0b_artifacts/
（scripts/v0b_corpus.py 生成；gen_stats 摘要进每 run 版本戳）。

诚实边界：交错 = 臂内顺序在每重复内轮转；任务状态每 run 重置（workdir 重铺，
warm 臂显式预置库）；墙钟含网关时延波动，聚合报 min/median/max 不只报均值。
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "release" / "tiny-agent"
ART = ROOT / "v0b_eval"
CORPUS = ROOT / "v0b_artifacts"
E0P_SKILL = ROOT / "examples" / "eval" / "e0prime-skill" / "run-code"
VOCAB = ["bug", "feature", "account", "billing", "howto", "complaint", "unsure"]

OUTER_MODEL = "glm-5.3-flash"
INNER_MODEL = "deepseek-flash"   # 单档（级联档位是正式轮后续项）

# 装配文档 §3 冻结指纹——e0p 臂跑前校验，漂移即拒跑
E0P_SHA = "0fdee8815ce6f16eb4204bb09d553efb672d1335691a16e2b46b3490c101d9e6"

TASK_TEXT = (
    "workdir 里有 dev.jsonl（带 label 金标的历史工单）与 holdout.jsonl（待分类工单）。"
    "请为 holdout 的每条工单判定 label，词表固定为 bug / feature / account / billing "
    "/ howto / complaint / unsure。判定标准从 dev 的金标归纳。"
    "把结果写到 predictions.jsonl：每行一个 JSON 对象 {\"id\": \"...\", \"label\": \"...\"}，"
    "行数与 holdout 完全一致、id 一一对应、label 必须在词表内。"
    "先写出完整 predictions 再做总结（完成后只回复紧凑摘要：总条数、各类计数、准确率自估）。"
    "predictions.jsonl 写出并通过行数/词表自检后即视为完成，直接给摘要结束，"
    "不要追加逐条复核。开工前先 read_skill 加载 run-code 技能再规划做法。"
)
# 末三句是协议级干预（所有臂同文，不是臂变量）：
# 1. 完成纪律（试点发现 3：E3 未写产物即烧穿）；
# 2. 无复核句（v2.1）：两次实测死在产物已完整、复核/格式化轮烧穿 cap
#    （v1.2 ds@4096 死在格式化轮；v1.4 ds@8192 写完 288 行 review 表后
#    死在逐条复核轮）——复核是模型自觉行为，明确"自检即完成"；
# 3. 技能拉起——门控披露（LIB/BATCH/LLM 段）与 E0′ 边界句全部骑在技能正文上，
#    模型不 read_skill 则干预零曝光（v1.1 批实测 6/6 零 read_skill；试点 E2
#    同因 LLM 披露未进上下文）。统一拉起后，臂间差异才真正回到"读到的技能
#    内容"（e0p 的磁盘变体同名覆盖内置）。


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


def corpus_version():
    p = CORPUS / "gen_stats.json"
    if not p.exists():
        sys.exit(f"missing {p} — python3 scripts/v0b_corpus.py --out v0b_artifacts")
    s = json.load(open(p, encoding="utf-8"))
    if s.get("check_problems"):
        sys.exit("corpus self-check has problems — regenerate first")
    return {k: s[k] for k in ("seed_main", "seed_e1", "version",
                              "kw_probe_macro_f1")}


def stage_workdir(run_dir, n_holdout, warm_lib=None):
    wd = run_dir / "wd"
    if wd.exists():
        shutil.rmtree(wd)
    wd.mkdir(parents=True)
    dev = [json.loads(l) for l in open(CORPUS / "tickets_dev.jsonl", encoding="utf-8")]
    hold = [json.loads(l) for l in open(CORPUS / "tickets_holdout.jsonl", encoding="utf-8")]
    hold = hold[:n_holdout]
    with open(wd / "dev.jsonl", "w", encoding="utf-8") as f:
        for r in dev:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    with open(wd / "holdout.jsonl", "w", encoding="utf-8") as f:
        for r in hold:
            f.write(json.dumps({k: v for k, v in r.items() if k != "label"},
                               ensure_ascii=False) + "\n")
    if warm_lib:
        shutil.copytree(warm_lib, wd / "lib")   # 预置上轮产出的库（warm 臂）
    gold = {r["id"]: r["label"] for r in hold}
    (run_dir / "gold.json").write_text(json.dumps(gold, ensure_ascii=False))
    return wd, gold


def stage_e0p_skill(run_dir):
    """E0′ 装置：磁盘技能覆盖内置（装配文档 §3）。跑前指纹门 + 全局目录防串。"""
    global_skills = Path.home() / ".config" / "tinyagent" / "skills" / "run-code"
    if global_skills.exists():
        sys.exit(f"全局技能目录存在旧副本会覆盖装置：{global_skills} —— 先清理")
    dst = run_dir / "wd" / ".tinyagent" / "skills" / "run-code"
    shutil.copytree(E0P_SKILL, dst)
    digest = hashlib.sha256((dst / "SKILL.md").read_bytes()).hexdigest()
    if digest != E0P_SHA:
        sys.exit(f"E0′ 指纹漂移：{digest} != {E0P_SHA} —— 先修装置再跑")


def write_config(run_dir, arm, wd, max_tokens, outer_model):
    base, key = gateway()
    code = {"enabled": True, "skill_injection": "on_demand",
            "include": False, "call_batch": False}
    if arm in ("e1", "e2", "e3"):
        code["include"] = True
    if arm in ("e2", "e3"):
        code["llm"] = {"enabled": True, "default_model": INNER_MODEL,
                       "models": [INNER_MODEL]}
    if arm == "e3":
        code["call_batch"] = True
    cfg = {
        "provider": "openai", "base_url": base, "api_key_env": "V0B_KEY",
        "model": outer_model, "max_tokens": max_tokens,
        "system_prompt": agentic_code_prompt(),
        "workdir": str(wd),
        "tools": ["shell", "read_file", "write_file", "edit_file", "list_dir"],
        "inject_environment": False,
        "limits": {"max_turns": 120, "timeout_secs": 5400,
                   "max_output_chars": 60000, "max_total_tokens": 4_000_000},
        "code": code,
    }
    p = run_dir / "config.json"
    p.write_text(json.dumps(cfg, ensure_ascii=False, indent=2))
    return p


def run_once(arm, rep, n_holdout, warm_lib, corpus_ver, timeout_s, max_tokens, outer_model):
    run_dir = ART / f"{arm}-r{rep}"
    run_dir.mkdir(parents=True, exist_ok=True)
    wd, gold = stage_workdir(run_dir, n_holdout, warm_lib)
    if arm == "e0p":
        stage_e0p_skill(run_dir)
    cfg = write_config(run_dir, arm, wd, max_tokens, outer_model)
    env = dict(os.environ, V0B_KEY=gateway()[1])
    t0 = time.time()
    with open(run_dir / "events.ndjson", "w") as evf, \
         open(run_dir / "run.log", "w") as logf:
        p = subprocess.run(
            [str(BIN), "--config", str(cfg), "--task", TASK_TEXT,
             "--session", "none", "--json", "--output", str(run_dir / "result.json")],
            stdout=evf, stderr=logf, env=env, timeout=timeout_s)
    return {"arm": arm, "rep": rep, "rc": p.returncode,
            "wall": round(time.time() - t0, 1), "gold": gold, "run_dir": run_dir,
            "corpus": corpus_ver, "warm": bool(warm_lib),
            "max_tokens": max_tokens, "outer_model": outer_model}


def score(run_dir, gold):
    pred_path = run_dir / "wd" / "predictions.jsonl"
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
    tp, fp, fn = Counter(), Counter(), Counter()
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


def cost(run_dir):
    """外层 usage 从 result.json 顶层；内层 llm 从 run_code 的 meta.llm 台账。
    路由观察：工具调用名计数（run_code/read_skill/shell……）。"""
    out = {"outer": None, "inner": None, "turns": None, "tool_calls": None,
           "stop_reason": None, "routes": None}
    rp = run_dir / "result.json"
    inner = {"calls": 0, "settled": 0, "unknown": 0, "models": {}}
    routes = Counter()
    if rp.exists():
        r = json.load(open(rp, encoding="utf-8"))
        out["outer"] = {"in": r.get("input_tokens"), "out": r.get("output_tokens"),
                        "cache_w": r.get("cache_creation_input_tokens"),
                        "cache_r": r.get("cache_read_input_tokens")}
        out["turns"] = r.get("turns")
        out["tool_calls"] = len(r.get("tool_calls") or [])
        out["stop_reason"] = r.get("stop_reason")
        for tc in r.get("tool_calls") or []:
            routes[tc.get("name")] += 1
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
    out["routes"] = dict(routes)
    return out


def aggregate(all_runs):
    """跨重复聚合：min/median/max（墙钟与 token 波动大，只报均值会撒谎）。"""
    agg = {}
    for arm in sorted({s["arm"] for s in all_runs}):
        runs = [s for s in all_runs if s["arm"] == arm]

        def stat(key, sub=None):
            vals = []
            for r in runs:
                v = r[key] if sub is None else ((r[key] or {}).get(sub))
                if isinstance(v, (int, float)):
                    vals.append(v)
            if not vals:
                return None
            vals.sort()
            med = vals[len(vals) // 2]
            return {"min": vals[0], "median": med, "max": vals[-1]}

        agg[arm] = {
            "repeats": len(runs),
            "macro_f1": stat("score", "macro_f1"),
            "valid_rate": stat("score", "valid_rate"),
            "wall_s": stat("wall_s"),
            "outer_in": stat("outer", "in"),
            "outer_out": stat("outer", "out"),
            "outer_cache_r": stat("outer", "cache_r"),
            "inner_calls": [r["inner"]["calls"] for r in runs if r["inner"]],
            "routes": [r["routes"] for r in runs],
            "stop_reasons": [r["stop_reason"] for r in runs],
        }
    return agg


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--arms", default="e0,e0p",
                    help="逗号分隔：e0,e0p,e1,e2,e3（先答路由主假设默认 e0,e0p）")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--holdout", type=int, default=288)
    ap.add_argument("--warm-lib", default=None,
                    help="e1 warm 臂预置库目录（缺省 cold）")
    ap.add_argument("--max-tokens", type=int, default=4096,
                    help="外层 max_tokens（装置参数，进版本戳；8192 用于探测"
                         "完成墙是 cap 还是探索长度）")
    ap.add_argument("--outer-model", default=OUTER_MODEL,
                    help="外层模型（默认 glm-5.3-flash；deepseek-flash 为完成墙"
                         "探针——glm 在 4096/8192 两档均烧满任意 cap 不完成）")
    ap.add_argument("--timeout", type=int, default=5400)
    args = ap.parse_args()
    if not BIN.exists():
        sys.exit(f"missing {BIN} — cargo build --release --features code")
    arms = [a.strip() for a in args.arms.split(",") if a.strip()]
    corpus_ver = corpus_version()
    ART.mkdir(exist_ok=True)
    print(f"[v0b] corpus {corpus_ver['version']} (kw_probe={corpus_ver['kw_probe_macro_f1']})")

    all_runs = []
    for rep in range(args.repeats):
        # 交错：每重复内臂顺序轮转（rep0 顺序、rep1 轮转一位……），摊网关时段效应
        order = arms[rep % len(arms):] + arms[:rep % len(arms)]
        for arm in order:
            run_dir = ART / f"{arm}-r{rep}"
            print(f"[v0b] arm {arm} rep {rep} ...", flush=True)
            try:
                r = run_once(arm, rep, args.holdout, args.warm_lib,
                             corpus_ver, args.timeout, args.max_tokens, args.outer_model)
            except subprocess.TimeoutExpired:
                print(f"[v0b] arm {arm} rep {rep} TIMED OUT", flush=True)
                continue
            s = score(run_dir, r["gold"])
            c = cost(run_dir)
            # stream_idle = 网关断流（基建失败≠模型失败）：同 (arm,rep) 重跑一次，
            # 两次记录都进 runs（重跑覆盖工件目录，失败记录在 log 里留痕）。
            if c["stop_reason"] == "stream_idle":
                print(f"[v0b]   stream_idle — rerunning once", flush=True)
                r = run_once(arm, rep, args.holdout, args.warm_lib,
                             corpus_ver, args.timeout, args.max_tokens, args.outer_model)
                s = score(run_dir, r["gold"])
                c = cost(run_dir)
            summary = {"arm": arm, "rep": rep, "rc": r["rc"], "wall_s": r["wall"],
                       "warm": r["warm"], "corpus": corpus_ver,
                       "score": s, **c}
            (run_dir / "summary.json").write_text(
                json.dumps(summary, ensure_ascii=False, indent=2))
            all_runs.append(summary)
            one = {"f1": s["macro_f1"], "valid": s["valid_rate"],
                   "routes": c["routes"], "stop": c["stop_reason"]}
            print(f"[v0b]   -> {json.dumps(one, ensure_ascii=False)}", flush=True)

    if all_runs:
        agg = aggregate(all_runs)
        (ART / "agg.json").write_text(
            json.dumps({"runs": all_runs, "aggregate": agg},
                       ensure_ascii=False, indent=2))
        print(json.dumps(agg, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
