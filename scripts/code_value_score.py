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
import math
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
        canon = json.dumps({t: None for t in gold}, sort_keys=True,
                           ensure_ascii=False)
        if hashlib.sha256(canon.encode()).hexdigest() != digest["gold_sha256"]:
            raise RuntimeError("t0 recomputed gold disagrees with digest")
        if len(gold) != digest["count"]:
            raise RuntimeError("t0 recomputed gold count disagrees with digest")
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
    canon = json.dumps(gold, sort_keys=True, ensure_ascii=False)
    if hashlib.sha256(canon.encode()).hexdigest() != digest["gold_sha256"]:
        raise RuntimeError(f"{task} recomputed gold disagrees with digest")
    if len(gold) != digest["count"]:
        raise RuntimeError(f"{task} recomputed gold count disagrees with digest")
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


def _events_usage(events_path):
    """从 events.ndjson 核账：turn/usage 事件计数 + usage 帧的 token 累计。

    返回 (n_turn, n_usage, tokens_dict)。events 是进程侧证据——result.json
    缺失/损坏时它是唯一账本（工单 01R P1-01：不能因 result 缺失归零）。
    """
    n_turn = n_usage = 0
    tokens = {"input_tokens": 0, "output_tokens": 0,
              "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
    if events_path is None or not Path(events_path).exists():
        return n_turn, n_usage, tokens
    with open(events_path, encoding="utf-8") as f:
        for line in f:
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            et = ev.get("type")
            if et == "turn":
                n_turn += 1
            elif et == "usage":
                n_usage += 1
                for k in tokens:
                    tokens[k] += ev.get(k) or 0
    return n_turn, n_usage, tokens


def usage_from_result(result: dict, events_path=None):
    """抽 usage 账并判定完整性。证据链：result.json 为主，events.ndjson
    为核账与兜底（result 缺失/损坏时 events 是唯一账本——不归零）。

    完整性三态（工单 01R P1-01：已知费用与费用完整性分开）：
      complete        —— 全部请求都报了账；
      partial_unknown —— 部分请求没报 usage（如中途 HTTP 500 的失败轮）；
      usage_missing   —— 请求发生过但没有任何已知 token。
    未知消耗绝不归零：unknown_reserve_tokens 按口径预留。
    """
    result_ok = isinstance(result, dict) and bool(result)
    tokens = {
        "input_tokens": (result.get("input_tokens") if result_ok else 0) or 0,
        "output_tokens": (result.get("output_tokens") if result_ok else 0) or 0,
        "cache_creation_input_tokens":
            (result.get("cache_creation_input_tokens")
             if result_ok else 0) or 0,
        "cache_read_input_tokens":
            (result.get("cache_read_input_tokens") if result_ok else 0) or 0,
    }
    total = sum(tokens.values())
    turns = (result.get("turns") if result_ok else 0) or 0
    attempts = (result.get("attempts") if result_ok else 0) or 0

    # events 证据：请求计数 + 已报账 token（result 缺失时的兜底账本）
    n_turn, n_usage, ev_tokens = _events_usage(events_path)

    # result 缺失/损坏但 events 有账：以 events 为准核账（不归零）
    result_missing = not result_ok
    if result_missing and (n_turn > 0 or n_usage > 0):
        tokens = dict(ev_tokens)
        total = sum(tokens.values())
        turns = n_turn
        attempts = max(attempts, n_turn)

    requests_happened = (turns > 0 or attempts > 0
                         or n_turn > 0 or n_usage > 0)
    usage_missing = requests_happened and total == 0

    # 部分未知：turn=尝试，usage=已报账请求；差值即「烧了没报账」的请求数
    unreported_requests = None
    if n_turn > 0 or n_usage > 0:
        unreported_requests = max(0, n_turn - n_usage)
    partial_unknown = bool(unreported_requests) and not usage_missing

    inner = 0
    inner_unknown = 0
    for tc in ((result.get("tool_calls") or []) if result_ok else []):
        llm = (tc.get("meta") or {}).get("llm")
        if not llm:
            continue
        for _, s in (llm.get("models") or {}).items():
            inner += s.get("known_usage", 0)
        inner_unknown += llm.get("unknown_calls", 0)

    # 预留口径：整轮缺按完整 run 预留；部分缺按缺报请求数 × 每请求预留
    # （单请求输出上限 + 保守输入——宁可高估不可漏计）。
    reserve = 0
    if usage_missing:
        reserve = UNKNOWN_RESERVE_TOKENS
    elif partial_unknown:
        reserve = unreported_requests * PARTIAL_UNKNOWN_PER_REQUEST_TOKENS

    # 完整性结论：任一未知（整缺/部分缺/内层 LLM 未知调用）即不完整
    complete = (not usage_missing and not partial_unknown
                and inner_unknown == 0)
    return {
        "tokens": tokens,
        "total_tokens": total,
        "turns": turns,
        "attempts": attempts,
        "requests_happened": requests_happened,
        "result_missing": result_missing,
        "usage_missing": usage_missing,
        "partial_unknown": partial_unknown,
        "unreported_requests": unreported_requests,
        "usage_complete": complete,
        # 未知消耗的显式标记：缺失即预留，绝不当 0（§5.2）
        "unknown_reserve_tokens": reserve,
        "inner_llm": ({"known_usage": inner, "unknown_calls": inner_unknown}
                      if (inner or inner_unknown) else None),
    }


def cost_from_usage(usage: dict, price_snapshot):
    """token → 钱。已知部分永远算出下界；完整性单独标记（工单 01R）。

    返回：
      cost_known_lower —— 已知 token 的费用（下界，完整性无关）；
      cost             —— 完整时 == 下界；不完整时 None（总费用不可判）；
      cost_complete    —— 费用是否完整（usage 完整且价表合法）；
      reason           —— 不完整/不可算的原因。
    价表缺失/非法 → 下界也不可算（cost_known_lower=None, reason 说明）。
    计费公式各分项各计一次；input 不含缓存读（见 usage_from_result）。
    """
    valid, why = validate_price_snapshot(price_snapshot)
    if not valid:
        return {"cost": None, "cost_known_lower": None,
                "cost_complete": False, "reason": why}
    t = usage["tokens"]
    lower = (t["input_tokens"] * price_snapshot["input"]
             + t["output_tokens"] * price_snapshot["output"]
             + t["cache_creation_input_tokens"]
             * price_snapshot["cache_creation"]
             + t["cache_read_input_tokens"] * price_snapshot["cache_read"])
    lower = round(lower, 6)
    if not usage.get("usage_complete"):
        reason = ("usage_missing" if usage.get("usage_missing")
                  else "partial_usage_missing")
        return {"cost": None, "cost_known_lower": lower,
                "cost_complete": False, "reason": reason,
                "unknown_reserve_tokens": usage["unknown_reserve_tokens"],
                "known_tokens": usage["total_tokens"]}
    return {"cost": lower, "cost_known_lower": lower,
            "cost_complete": True,
            "currency": price_snapshot.get("currency")}


def validate_price_snapshot(price_snapshot):
    """价表校验（工单 01R P1-01：负价/NaN/缺项/缺币种不得放行）。

    返回 (ok, reason)。要求五项齐全、四个单价为有限非负数、币种非空。
    """
    if not price_snapshot or not isinstance(price_snapshot, dict):
        return False, "price_snapshot_missing"
    keys = ("input", "output", "cache_creation", "cache_read")
    for k in keys:
        v = price_snapshot.get(k)
        if not isinstance(v, (int, float)) or isinstance(v, bool):
            return False, f"price_snapshot_{k}_not_number"
        if not math.isfinite(v) or v < 0:
            return False, f"price_snapshot_{k}_invalid"
    cur = price_snapshot.get("currency")
    if not isinstance(cur, str) or not cur.strip():
        return False, "price_snapshot_currency_missing"
    return True, None


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

    费用口径（工单 01R P1-01）：
      cost_per_success —— 主指标。任一 run 费用不完整（未知消耗）时为
        null（不把「已知费用/全部成功数」这种混合口径当主指标）；
      cost_known_lower_total —— 已知费用下界合计（完整性无关，永远给出）；
      unknown_cost_runs —— 费用不完整的 run 清单（必须连同下界一起读）。
    """
    delivered = [r for r in runs if r["summary"]["delivered_exact"]]
    complete = [r for r in runs if r["summary"]["cost"]["cost_complete"]]
    unknown = [r["run_id"] for r in runs
               if not r["summary"]["cost"]["cost_complete"]]
    total_complete = sum(r["summary"]["cost"]["cost"] or 0 for r in complete)
    lower_total = sum(r["summary"]["cost"]["cost_known_lower"] or 0
                      for r in runs)
    return {
        "runs": len(runs),
        "delivered_exact_count": len(delivered),
        # 主指标：只有全部 run 费用完整才有值；否则 null + 下界单列
        "cost_per_success": (round(total_complete / len(delivered), 6)
                             if delivered and len(complete) == len(runs)
                             else None),
        "cost_complete_total": round(total_complete, 6),
        "cost_known_lower_total": round(lower_total, 6),
        "known_delivered_count": len([r for r in delivered
                                      if r["summary"]["cost"]["cost_complete"]]),
        "cost_currency": (complete[0]["summary"]["cost"].get("currency")
                          if complete else None),
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
            "cost_complete_total": round(
                sum(r["summary"]["cost"]["cost"] or 0
                    for r in sub
                    if r["summary"]["cost"]["cost_complete"]), 6),
            "cost_known_lower_total": round(
                sum(r["summary"]["cost"]["cost_known_lower"] or 0
                    for r in sub), 6),
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
