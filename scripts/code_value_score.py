#!/usr/bin/env python3
"""run_code 价值验证 v0.3 —— 严格交付验收器与费用账（方案 §5）。

与历史 v0b_eval.py 的 score/aggregate 刻度不同（工单 01 第 2 条）：
  - 不合并重复 ID、不跳过坏 JSON、不把 partial 从主账剔除；
  - 机械任务以完整集合及计数精确匹配判交付（delivered_exact），
    不用模型自报「验证通过」代替验收；
  - 结束原因单独登记：产物已完整而 run 后续触闸，仍记交付完成，
    同时报告非正常结束与浪费（方案 §5.1）；
  - 未知 usage 显式标记（usage_missing / cost=null + 预留），
    禁止把未知消耗当零（方案 §4.1/§5.2）。

验收判据（§5.1 最小离线用例全覆盖）：
  完整正确 / 缺行 / 重复 ID / 未知 ID / 错误计数 / 非法 JSON /
  合法 JSON 但类型或 schema 错 / 空产物 / 空 gold 与非空误报 /
  运行失败但产物已完整。
排序与无关空白不计业务错误；未知字段算 schema 错误（机械任务严格口径）。

费用口径（§5.2）：主费用 = 全部模型尝试的真实计费项之和（缓存、失败、
重试都在 result.json 的 usage 里），输入 token 已剔除缓存读
（provider 解析层保证 input_tokens 不含 cached；这里不再重复计入）。

用法（被 code_value_eval.py 调用；也可单独 CLI 验一份产物）：
  python3 scripts/code_value_score.py --artifact wd/answer.jsonl \
      --gold fixture/t1 --task t1 --result result.json   (任务目录：从快照现算)
"""
import argparse
import json
import sys
from pathlib import Path

# 每任务的交付 schema：字段名 → 类型。严格口径：未知字段 = schema 错误。
TASK_SCHEMAS = {
    "t0": {"id": str},
    "t1": {"user_id": str, "event_count": int},
    "t2": {"user_id": str, "event_count": int},
}


def load_gold(fixture_task_dir: Path):
    """外部判卷进程：从快照现算 gold（方案 §2.1——gold 不落盘，shell
    看不见现成答案文件）。算完与 gen_stats.gold_digest 对账，防快照被
    篡改后静默判错。返回 {id_key: {id_value: count}}；t0 value 恒 None。
    """
    fixture_task_dir = Path(fixture_task_dir)
    root = fixture_task_dir.parent
    stats = json.load(open(root / "gen_stats.json", encoding="utf-8"))
    digest = stats["gold_digest"][fixture_task_dir.name]
    import hashlib
    import sys as _sys
    _sys.path.insert(0, str(Path(__file__).resolve().parent))
    import code_value_fixture as _fx

    task = fixture_task_dir.name
    if task == "t0":
        gold = _fx.solve_t0(_fx.load_jsonl(fixture_task_dir / "records.jsonl"))
        if gold != digest["ids"]:
            raise RuntimeError("t0 recomputed gold disagrees with digest")
        return {"id": {t: None for t in gold}}
    if task == "t1":
        subs = _fx.load_jsonl(fixture_task_dir / "subscriptions.jsonl")
        evs = _fx.load_jsonl(fixture_task_dir / "events.jsonl")
        gold = _fx.solve_join(subs, evs)
    elif task == "t2":
        subs = json.load(open(fixture_task_dir / "server" / "subscriptions.json",
                              encoding="utf-8"))
        evs = json.load(open(fixture_task_dir / "server" / "events.json",
                             encoding="utf-8"))
        gold = _fx.solve_join(subs, evs, sub_key="user_id", ev_key="uid")
    else:
        raise ValueError(f"unknown task {task}")
    if sorted(gold) != digest["user_ids"]:
        raise RuntimeError(f"{task} recomputed gold ids disagree with digest")
    if hashlib.sha256(json.dumps(gold, sort_keys=True).encode()).hexdigest() \
            != digest["counts_sha256"]:
        raise RuntimeError(f"{task} recomputed gold counts disagree with digest")
    return {"user_id": dict(gold)}


def validate_artifact(artifact_path: Path, gold: dict, task: str):
    """严格验收一份 answer.jsonl。

    返回 validation dict：
      delivered_exact —— 产物严格合法且集合与计数完全一致；
      reasons         —— 拒绝原因列表（空 = 通过）；
      details         —— 诊断用的差分统计（缺/重/未知/错计数/坏行）。
    任何一行坏都拒绝整份产物（不静默跳过——与 v0b 刻度的关键差别）。
    产物文件不存在 = 空产物，同样拒绝并记原因。
    """
    id_key = next(iter(gold))
    want = gold[id_key]
    reasons = []
    details = {"task": task, "id_key": id_key, "gold_count": len(want),
               "line_count": 0, "invalid_lines": [], "duplicate_ids": [],
               "unknown_ids": [], "missing_ids": [], "wrong_counts": {},
               "false_positive_ids": [], "artifact_exists": False,
               "artifact_bytes": 0}

    if artifact_path is None or not Path(artifact_path).exists():
        reasons.append("empty_artifact: answer.jsonl not found")
        return {"delivered_exact": False, "reasons": reasons, "details": details}
    details["artifact_exists"] = True
    raw = Path(artifact_path).read_bytes()
    details["artifact_bytes"] = len(raw)
    if len(raw) == 0:
        reasons.append("empty_artifact: answer.jsonl is 0 bytes")
        return {"delivered_exact": False, "reasons": reasons, "details": details}

    schema = TASK_SCHEMAS[task]
    preds = {}
    for lineno, line in enumerate(raw.decode("utf-8", errors="replace")
                                  .splitlines(), start=1):
        if not line.strip():
            continue  # 无关空白不计业务错误（方案 §5.1）
        details["line_count"] += 1
        try:
            row = json.loads(line)
        except json.JSONDecodeError as e:
            reasons.append(f"invalid_json: line {lineno}: {e.msg}")
            details["invalid_lines"].append(lineno)
            continue
        if not isinstance(row, dict):
            reasons.append(f"schema_error: line {lineno}: not a JSON object")
            details["invalid_lines"].append(lineno)
            continue
        if set(row.keys()) != set(schema.keys()):
            reasons.append(
                f"schema_error: line {lineno}: fields {sorted(row.keys())} "
                f"!= {sorted(schema.keys())}")
            details["invalid_lines"].append(lineno)
            continue
        bad_types = [k for k, t in schema.items()
                     if not isinstance(row[k], t) or isinstance(row[k], bool)]
        if bad_types:  # bool 是 int 的子类，true/false 冒充计数要拦
            reasons.append(
                f"schema_error: line {lineno}: wrong type for {bad_types}")
            details["invalid_lines"].append(lineno)
            continue
        rid = row[id_key]
        if rid in preds:
            reasons.append(f"duplicate_id: {rid} (line {lineno})")
            details["duplicate_ids"].append(rid)
            continue
        preds[rid] = row.get("event_count") if "event_count" in schema else None

    for rid in preds:
        if rid not in want:
            reasons.append(f"unknown_id: {rid}")
            details["unknown_ids"].append(rid)
    if not want:
        # 空 gold + 非空交付 = 全部误报（§5.1 用例：空 gold 与非空误报）
        details["false_positive_ids"] = sorted(preds)
        if preds:
            reasons.append(
                f"false_positive: gold is empty but {len(preds)} ids delivered")
    for rid in sorted(want):
        if rid not in preds:
            reasons.append(f"missing_id: {rid}")
            details["missing_ids"].append(rid)
        elif want[rid] is not None and preds[rid] != want[rid]:
            reasons.append(
                f"wrong_count: {rid}: delivered {preds[rid]}, want {want[rid]}")
            details["wrong_counts"][rid] = {"delivered": preds[rid],
                                            "want": want[rid]}

    details["delivered_count"] = len(preds)
    return {"delivered_exact": not reasons, "reasons": reasons,
            "details": details}


# ── usage / 费用（方案 §5.2 + §4.1）──────────────────────────────────────────

# 未知用量的保守预留：按「一个完整 run 的软闸 + 单请求输出上限」计 token 风险，
# 费用换算在最贵适用 token 单价上做（由调用方传入价表后折算）。
UNKNOWN_RESERVE_TOKENS = 80_000 + 32_768
# 部分未知（个别请求没报 usage）的每请求预留：单请求输出上限 + 保守输入
PARTIAL_UNKNOWN_PER_REQUEST_TOKENS = 32_768 + 32_768


def usage_from_result(result: dict, events_path=None):
    """从 result.json 抽 usage 账。判定「请求发生过但 usage 缺失」——
    那是未知消耗，不是零成本（§4.1：未知费用预留，不能当 0）。
    另有 partial_unknown：部分请求没报 usage（如中途 HTTP 500 的失败轮），
    run 总量 >0 掩盖了那几轮的真实消耗——events 流里 turn 事件每尝试
    一个、usage 事件每已计费请求一个，差值即「烧了没报账」的请求数。

    result.json 的 usage 覆盖全部尝试（失败轮经 RunCost 折入，attempts>1
    即含重试），这里直接采信、不重复累加缓存子项：input_tokens 在
    provider 解析层已剔除缓存读（openai.rs），cache_read 单列，
    两者相加不会把同一批 token 计两次。
    """
    tokens = {
        "input_tokens": result.get("input_tokens") or 0,
        "output_tokens": result.get("output_tokens") or 0,
        "cache_creation_input_tokens":
            result.get("cache_creation_input_tokens") or 0,
        "cache_read_input_tokens": result.get("cache_read_input_tokens") or 0,
    }
    total = sum(tokens.values())
    turns = result.get("turns") or 0
    attempts = result.get("attempts") or 0
    requests_happened = turns > 0 or attempts > 0
    usage_missing = requests_happened and total == 0

    # 部分未知：events 流逐请求对账（turn=尝试，usage=已报账请求）
    unreported_requests = None
    if events_path is not None and Path(events_path).exists():
        n_turn = n_usage = 0
        with open(events_path, encoding="utf-8") as f:
            for line in f:
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if ev.get("type") == "turn":
                    n_turn += 1
                elif ev.get("type") == "usage":
                    n_usage += 1
        unreported_requests = max(0, n_turn - n_usage)
    partial_unknown = bool(unreported_requests)
    inner = 0
    inner_unknown = 0
    for tc in result.get("tool_calls") or []:
        llm = (tc.get("meta") or {}).get("llm")
        if not llm:
            continue
        for _, s in (llm.get("models") or {}).items():
            inner += s.get("known_usage", 0)
        inner_unknown += llm.get("unknown_calls", 0)
    return {
        "tokens": tokens,
        "total_tokens": total,
        "turns": turns,
        "attempts": attempts,
        "requests_happened": requests_happened,
        "usage_missing": usage_missing,
        "partial_unknown": partial_unknown,
        "unreported_requests": unreported_requests,
        # 未知消耗的显式标记：缺失即预留，绝不当 0（§5.2）。
        # 整轮缺按完整 run 预留；部分缺按缺报请求数 × 每请求预留
        # （单请求输出上限 + 保守输入——宁可高估不可漏计）。
        "unknown_reserve_tokens": (
            UNKNOWN_RESERVE_TOKENS if usage_missing
            else (unreported_requests or 0) * PARTIAL_UNKNOWN_PER_REQUEST_TOKENS
            if partial_unknown else 0),
        "inner_llm": ({"known_usage": inner, "unknown_calls": inner_unknown}
                      if (inner or inner_unknown) else None),
    }


def cost_from_usage(usage: dict, price_snapshot):
    """token → 钱。价表缺失或 usage 未知 → cost=null（费用不可判，不是 0）。

    price_snapshot: {"input": per_tok, "output": per_tok,
                     "cache_creation": per_tok, "cache_read": per_tok,
                     "currency": "CNY"}（per-token 单价，浮点）。
    计费公式只对各分项各计一次；input 不含缓存读（见 usage_from_result）。
    """
    if not price_snapshot:
        return {"cost": None, "cost_known": False,
                "reason": "price_snapshot_missing"}
    if usage.get("usage_missing"):
        return {"cost": None, "cost_known": False,
                "reason": "usage_missing",
                "unknown_reserve_tokens": usage["unknown_reserve_tokens"]}
    if usage.get("partial_unknown"):
        # 部分请求没报 usage：已知部分能算，但 run 总费用不可判——
        # 按 §4.1「未知不能当 0」，cost=null + 预留，已知 token 单列参考
        return {"cost": None, "cost_known": False,
                "reason": "partial_usage_missing",
                "unknown_reserve_tokens": usage["unknown_reserve_tokens"],
                "known_tokens": usage["total_tokens"]}
    t = usage["tokens"]
    cost = (t["input_tokens"] * price_snapshot["input"]
            + t["output_tokens"] * price_snapshot["output"]
            + t["cache_creation_input_tokens"] * price_snapshot["cache_creation"]
            + t["cache_read_input_tokens"] * price_snapshot["cache_read"])
    return {"cost": round(cost, 6), "cost_known": True,
            "currency": price_snapshot.get("currency")}


def summarize_run(run: dict, validation: dict, usage: dict, cost: dict,
                  end_reason: str) -> dict:
    """单 run 的 summary.json 内容（方案 §6.1 每 run 字段）。

    delivered 与结束原因分开登记：run 失败（error/timeout/…）但产物已
    完整 → delivered_exact=True + abnormal_end=True（§5.1：产物已完整而
    模型后续触闸，仍记完成交付，同时报告非正常结束）。
    """
    return {
        "run_id": run["run_id"],
        "task": run["task"],
        "arm": run["arm"],
        # §6.1 per-run 字段：instance/repetition/order 由调用方给（G1
        # 六跑各一实例一重复，order 来自冻结的 run_schedule）
        "instance_id": run.get("instance_id", 1),
        "repetition": run.get("repetition", 1),
        "order": run.get("order"),
        "delivered_exact": validation["delivered_exact"],
        "reject_reasons": validation["reasons"],
        "validation_details": validation["details"],
        "end_reason": end_reason,
        "abnormal_end": end_reason not in ("end_turn",),
        "usage": usage,
        "cost": cost,
    }


def aggregate(runs: list) -> dict:
    """阶段总账：所有 run（含失败/partial）都进账，不做任何剔除（§5.3）。

    cost_per_success = 全部尝试费用 / delivered_exact 数；零成功为 null。
    未知费用（cost=null）单列 unknown_cost_runs，不折算成 0。
    """
    delivered = [r for r in runs if r["summary"]["delivered_exact"]]
    known = [r for r in runs if r["summary"]["cost"]["cost_known"]]
    unknown = [r["run_id"] for r in runs
               if not r["summary"]["cost"]["cost_known"]]
    total_known = sum(r["summary"]["cost"]["cost"] or 0 for r in known)
    # cost_per_success 口径（§5.2）：分子=全部尝试的已知费用，分母=
    # delivered_exact 数。有未知费用 run 时该值偏保守（分子缺它）——
    # known_delivered_count 单列，报告里必须连同 unknown_cost_runs 一起读。
    delivered_known = [r for r in delivered
                       if r["summary"]["cost"]["cost_known"]]
    return {
        "runs": len(runs),
        "delivered_exact_count": len(delivered),
        "cost_known_total": round(total_known, 6),
        "cost_currency": (known[0]["summary"]["cost"].get("currency")
                          if known else None),
        "cost_per_success": (round(total_known / len(delivered), 6)
                             if delivered and known else None),
        "known_delivered_count": len(delivered_known),
        "unknown_cost_runs": unknown,
        "usage_missing_runs": [r["run_id"] for r in runs
                               if r["summary"]["usage"]["usage_missing"]],
        "partial_unknown_runs": [r["run_id"] for r in runs
                                 if r["summary"]["usage"].get("partial_unknown")],
        "failed_runs": [r["run_id"] for r in runs
                        if r["summary"]["abnormal_end"]],
        "by_arm": _by_arm(runs),
    }


def _by_arm(runs: list) -> dict:
    out = {}
    for arm in sorted({r["arm"] for r in runs}):
        sub = [r for r in runs if r["arm"] == arm]
        out[arm] = {
            "runs": len(sub),
            "delivered": sum(1 for r in sub if r["summary"]["delivered_exact"]),
            "total_tokens": sum(r["summary"]["usage"]["total_tokens"]
                                for r in sub),
            "unknown_reserve_tokens": sum(
                r["summary"]["usage"]["unknown_reserve_tokens"] for r in sub),
            "cost_known_total": round(
                sum(r["summary"]["cost"]["cost"] or 0
                    for r in sub
                    if r["summary"]["cost"]["cost_known"]), 6),
        }
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--artifact", required=True)
    ap.add_argument("--gold", required=True)
    ap.add_argument("--task", required=True, choices=list(TASK_SCHEMAS))
    ap.add_argument("--result", default=None, help="result.json（可省）")
    ap.add_argument("--price", default=None, help="价表 JSON（可省）")
    args = ap.parse_args()
    validation = validate_artifact(Path(args.artifact), load_gold(args.gold),
                                   args.task)
    result = json.load(open(args.result, encoding="utf-8")) if args.result else {}
    usage = usage_from_result(result)
    price = json.load(open(args.price, encoding="utf-8")) if args.price else None
    cost = cost_from_usage(usage, price)
    print(json.dumps({"validation": validation, "usage": usage, "cost": cost},
                     ensure_ascii=False, indent=1))
    sys.exit(0 if validation["delivered_exact"] else 1)


if __name__ == "__main__":
    main()
