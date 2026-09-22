#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""V0-A 评测装置（冻结配方 docs/code_v0a_recipe.md 执行器）。

四路线（D1，全部经真实网关的实验脚本模拟，③④ 复刻 LlmTool 语义）：
  ① 外层逐条：glm-5.3-flash 逐条独立调用（网关默认 temperature）
  ② 外层+纯代码库：lib 覆盖项零模型成本；模糊项回落 glm
  ③ ②+内层单发：模糊项回落 deepseek-flash，temperature=0、labels 词表校验
  ④ ③+批内并行：模糊项以 8 路有界波并行（模拟 callBatch 波语义）

纪律：NDJSON 逐条 checkpoint 可续跑（C5 部分交付）；429/5xx/网络错重试 1 次
隔 5s，最终失败保留计分母（B5）；usage 原样记录不伪造（A-L2b 口径）。
密钥经环境变量 TA_V0A_KEY 注入（读自 测试模型.md，不硬编码）。

用法：
  python3 scripts/v0a_eval.py run  --route 1 --split holdout --repeat 1
  python3 scripts/v0a_eval.py run-all            # 冻结全序列（B→C→D）
  python3 scripts/v0a_eval.py judge               # 汇总判卷 + 门槛核对
  python3 scripts/v0a_eval.py sentinels           # C3② 哨兵链路健康
"""
import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from v0a_lib import classify as lib_classify  # noqa: E402

BASE_URL = "http://193.112.219.133:4100/one/v1"
OUTER = "glm-5.3-flash"
INNER = "deepseek-flash"
VOCAB = ["bug", "feature", "account", "billing", "howto", "complaint", "unsure"]
MAX_TOKENS = 512        # 内层单发（冻结，LD-4 默认量级；deepseek 实测思考 ~100 tok）
MAX_TOK_OUTER = 2048    # 外层 glm 思考烧穿 512 会空输出（M0 冒烟已预言），给足余量
WAVE = 8
ART = "v0a_artifacts"
CKPT = os.path.join(ART, "checkpoints")

# 分类定义 v1 —— 与生成器边界规则逐条对应；四路线共用（同等业务知识，D1）
DEFINITIONS = """你是客服工单分类器。阅读工单，输出且仅输出一个标签词。
标签定义：
- bug：产品功能故障——报错、异常、闪退、数据丢失或错误、无法完成已有操作。
- feature：功能请求——希望增加目前没有的能力，或改进现有功能的设计。
- account：账号问题——登录、密码、验证码、绑定/解绑、封禁申诉等账号本身的问题。
- billing：计费问题——扣费、退款、账单、发票、会员续费、价格等费用问题。
- howto：使用咨询——询问已有功能怎么用、在哪里、支不支持。
- complaint：投诉升级——核心诉求是表达强烈不满、要求投诉或升级处理或威胁曝光。
- unsure：无法可靠分类——多个不同诉求并列无主次、乱码或空内容、信息过少、非工单内容。
边界规则：
1. 报错发生在登录或账号环节→account；发生在支付或扣费环节→billing；其余报错→bug。
2. 问"能不能、支不支持"：功能已存在问用法→howto；功能不存在想要→feature。
3. 有明确业务诉求加上情绪化表达→按业务诉求分类；只有情绪、无具体诉求→complaint。
4. 多个不同类诉求并列且无主次→unsure。
5. 非工单内容（代码、广告、系统通知、无关内容）→unsure。
输出契约：只输出一个标签词，不要输出任何其他文字、标点或解释。"""


def call_model(model, user, temperature=None, max_tokens=None):
    """单发调用 → dict(text/in/out/finish/latency/status)。B5 重试一次。"""
    body = {"model": model, "stream": False,
            "max_tokens": max_tokens or MAX_TOKENS,
            "messages": [{"role": "system", "content": DEFINITIONS},
                         {"role": "user", "content": user}]}
    if temperature is not None:
        body["temperature"] = temperature
    data = json.dumps(body).encode()
    key = os.environ.get("TA_V0A_KEY", "")
    t0, last = time.time(), ""
    for attempt in (1, 2):
        try:
            req = urllib.request.Request(
                BASE_URL + "/chat/completions", data=data,
                headers={"Content-Type": "application/json",
                         "Authorization": "Bearer " + key})
            with urllib.request.urlopen(req, timeout=90) as resp:
                j = json.loads(resp.read())
            usage = j.get("usage") or {}
            ch = (j.get("choices") or [{}])[0]
            text = (ch.get("message") or {}).get("content") or ""
            return {"text": text.strip(), "in": usage.get("prompt_tokens", 0),
                    "out": usage.get("completion_tokens", 0),
                    "finish": ch.get("finish_reason"),
                    "latency": round(time.time() - t0, 3), "status": "ok"}
        except urllib.error.HTTPError as e:
            last = f"HTTP {e.code}"
            if not (e.code == 429 or e.code >= 500):
                break  # 4xx（非 429）确定性失败：不重试
        except Exception as e:  # 网络错/超时
            last = type(e).__name__
        if attempt == 1:
            time.sleep(5)
    return {"text": "", "in": 0, "out": 0, "finish": "error",
            "latency": round(time.time() - t0, 3), "status": "error:" + last}


def extract_label(text):
    """词表校验（LD-18 的脚本版）：取回复中最先出现的词表词；无 → None（无效输出）。"""
    if not text:
        return None
    t = text.lower()
    best, pos = None, len(t)
    for v in VOCAB:
        i = t.find(v)
        if i != -1 and i < pos:
            pos, best = i, v
    return best


def ticket_user(x):
    return f"工单标题：{x.get('title', '')}\n工单正文：{x.get('body', '')}"


def load_split(split):
    name = {"dev": "tickets_dev.jsonl", "holdout": "tickets_holdout.jsonl",
            "e1": "tickets_e1.jsonl", "neg": "tickets_neg.jsonl"}[split]
    return [json.loads(l) for l in open(os.path.join(ART, name), encoding="utf-8")]


def one_item(route, x):
    """单条工单经路线 route 的完整处理 → 记录 dict。"""
    rec = {"id": x["id"], "label": x["label"], "tier": x.get("tier", -1)}
    if route >= 2:
        lbl, rid = lib_classify(x.get("title", ""), x.get("body", ""))
        if lbl is not None:
            rec.update(pred=lbl, path="lib", rule=rid, valid=True,
                       in_tok=0, out_tok=0, latency=0.0, finish="lib",
                       status="ok", model="")
            return rec
    model = OUTER if route in (1, 2) else INNER
    temp = None if route in (1, 2) else 0.0
    mt = MAX_TOK_OUTER if route in (1, 2) else MAX_TOKENS
    r = call_model(model, ticket_user(x), temperature=temp, max_tokens=mt)
    pred = extract_label(r["text"])
    rec.update(pred=pred, path="model", model=model,
               valid=pred is not None, in_tok=r["in"], out_tok=r["out"],
               latency=r["latency"], finish=r["finish"], status=r["status"],
               reply_head=r["text"][:40])
    return rec


def ckpt_path(route, split, repeat):
    return os.path.join(CKPT, f"r{route}-{split}-{repeat}.ndjson")


def run_route(route, split, repeat, limit=0):
    os.makedirs(CKPT, exist_ok=True)
    ck = ckpt_path(route, split, repeat)
    done = set()
    if os.path.exists(ck):
        done = {json.loads(l)["id"] for l in open(ck, encoding="utf-8")}
    items = [x for x in load_split(split) if x["id"] not in done]
    if limit:
        items = items[:limit]
    t0 = time.time()
    n_new = 0
    with open(ck, "a", encoding="utf-8") as f:
        if route == 4:  # 波语义：8 路有界并行，结果按提交序落盘
            for i in range(0, len(items), WAVE):
                wave = items[i:i + WAVE]
                with ThreadPoolExecutor(max_workers=WAVE) as ex:
                    for rec in ex.map(lambda x: one_item(route, x), wave):
                        f.write(json.dumps(rec, ensure_ascii=False) + "\n")
                        f.flush()
                        n_new += 1
        else:
            for x in items:
                rec = one_item(route, x)
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")
                f.flush()
                n_new += 1
    wall = round(time.time() - t0, 1)
    meta = {"route": route, "split": split, "repeat": repeat, "n_new": n_new,
            "n_done_before": len(done), "wall": wall,
            "ts": time.strftime("%Y-%m-%d %H:%M:%S")}
    mfile = ck.replace(".ndjson", ".meta.json")
    old = {}
    if os.path.exists(mfile):
        old = json.load(open(mfile, encoding="utf-8"))
    old["last"] = meta
    old.setdefault("runs", []).append(meta)
    json.dump(old, open(mfile, "w", encoding="utf-8"), ensure_ascii=False, indent=1)
    print(json.dumps(meta, ensure_ascii=False))


def run_all():
    """冻结全序列：B 留出 3 次交错重复 → C E1 分布移位 → D 负例+哨兵。"""
    phases = []
    for rep in (1, 2, 3):
        for route in (1, 2, 3, 4):
            phases.append((route, "holdout", rep))
    for route in (1, 2, 3, 4):
        phases.append((route, "e1", 1))
        phases.append((route, "neg", 1))
    for route, split, rep in phases:
        print(f"=== route {route} {split} rep{rep} ===", flush=True)
        run_route(route, split, rep)


def records(route, split, repeat):
    p = ckpt_path(route, split, repeat)
    return [json.loads(l) for l in open(p, encoding="utf-8")]


def metrics(recs):
    """宏 F1 / 逐类召回 / 有效率 / 分层 / token 与时延账。"""
    import collections
    tp = collections.Counter()
    fp = collections.Counter()
    fn = collections.Counter()
    n = len(recs)
    valid = sum(1 for r in recs if r["valid"])
    correct = sum(1 for r in recs if r["pred"] == r["label"])
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
        r_ = tp[c] / (tp[c] + fn[c]) if tp[c] + fn[c] else 0.0
        f1s.append(2 * p * r_ / (p + r_) if p + r_ else 0.0)
    tiers = {}
    for r in recs:
        d = tiers.setdefault(r.get("tier", -1), [0, 0])
        d[1] += 1
        d[0] += (r["pred"] == r["label"])
    tokin = sum(r["in_tok"] for r in recs)
    tokout = sum(r["out_tok"] for r in recs)
    n_model = sum(1 for r in recs if r["path"] == "model")
    seq_latency = round(sum(r["latency"] for r in recs), 1)
    return {
        "n": n, "macro_f1": round(sum(f1s) / len(f1s), 4),
        "acc": round(correct / n, 4) if n else 0,
        "valid_rate": round(valid / n, 4) if n else 0,
        "recall": {c: round(tp[c] / (tp[c] + fn[c]), 3) if tp[c] + fn[c] else None
                   for c in VOCAB},
        "tier_acc": {str(k): round(v[0] / v[1], 3) for k, v in sorted(tiers.items())},
        "tok_in": tokin, "tok_out": tokout,
        "tok_per_item": round((tokin + tokout) / n, 1) if n else 0,
        "tok_per_correct": round((tokin + tokout) / correct, 1) if correct else None,
        "n_model_calls": n_model, "n_lib": n - n_model,
        "seq_latency_s": seq_latency,
        "avg_latency_s": round(seq_latency / n_model, 2) if n_model else 0,
    }


def wall_of(route, split, repeat):
    m = ckpt_path(route, split, repeat).replace(".ndjson", ".meta.json")
    if os.path.exists(m):
        return json.load(open(m, encoding="utf-8"))["last"]["wall"]
    return None


def judge():
    out = {"holdout": {}, "e1": {}, "neg": {}, "gates": {}}
    for rep in (1, 2, 3):
        for route in (1, 2, 3, 4):
            out["holdout"][f"r{route}-p{rep}"] = metrics(
                records(route, "holdout", rep))
    for split in ("e1", "neg"):
        for route in (1, 2, 3, 4):
            out[split][f"r{route}"] = metrics(records(route, split, 1))
    # 门槛核对（C1/C2/D3/D4，冻结值）
    hs = [out["holdout"][f"r{r}-p{p}"] for r in (1, 2, 3, 4) for p in (1, 2, 3)]
    f1 = {r: [out["holdout"][f"r{r}-p{p}"]["macro_f1"] for p in (1, 2, 3)]
          for r in (1, 2, 3, 4)}
    gates = {
        "C1_macro_f1_ge_0.85": {f"r{r}": f1[r] for r in f1},
        "C1_pass": {f"r{r}": all(x >= 0.85 for x in f1[r]) for r in f1},
        "C2_valid_rate_ge_0.98": {f"r{r}-p1": out["holdout"][f"r{r}-p1"]["valid_rate"]
                                  for r in (1, 2, 3, 4)},
        "C2_minority_recall_ge_0.75": {
            f"r{r}-p{p}": [out["holdout"][f"r{r}-p{p}"]["recall"]["billing"],
                           out["holdout"][f"r{r}-p{p}"]["recall"]["complaint"]]
            for r in (1, 2, 3, 4) for p in (1, 2, 3)},
        "D3_route3_tokperitem_le_50pct_route1": None,
        "D4_route4_wall_vs_route1": None,
    }
    a = out["holdout"]["r1-p1"]
    c = out["holdout"]["r3-p1"]
    if a["tok_per_item"] and c["tok_per_item"] is not None:
        gates["D3_route3_tokperitem_le_50pct_route1"] = round(
            c["tok_per_item"] / a["tok_per_item"], 3)
    w1 = wall_of(1, "holdout", 1)
    w4 = wall_of(4, "holdout", 1)
    if w1 and w4:
        gates["D4_route4_wall_vs_route1"] = round(w4 / w1, 3)
    out["gates"] = gates
    json.dump(out, open(os.path.join(ART, "results.json"), "w", encoding="utf-8"),
              ensure_ascii=False, indent=1)
    print(json.dumps(out, ensure_ascii=False, indent=1))


def sentinels():
    """C3②：每路线 8 条哨兵——分类正确 + 期望路径一致（链路健康，非准确率）。"""
    sents = [json.loads(l) for l in
             open(os.path.join(ART, "sentinels.jsonl"), encoding="utf-8")]
    out = {}
    for route in (1, 2, 3, 4):
        rows = []
        for x in sents:
            rec = one_item(route, x)
            path = rec["path"] if rec["path"] == "lib" else "fallback"
            ok = rec["pred"] == x["label"] and (
                x["expect_path"] in ("either", path))
            rows.append({"id": x["id"], "ok": ok, "pred": rec["pred"],
                         "path": path, "want": x["expect_path"]})
        out[f"r{route}"] = rows
        print(f"route{route}: {sum(r['ok'] for r in rows)}/{len(rows)} 哨兵通过")
    json.dump(out, open(os.path.join(ART, "sentinel_results.json"), "w",
                        encoding="utf-8"), ensure_ascii=False, indent=1)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--route", type=int, required=True, choices=(1, 2, 3, 4))
    r.add_argument("--split", required=True, choices=("dev", "holdout", "e1", "neg"))
    r.add_argument("--repeat", type=int, default=1)
    r.add_argument("--limit", type=int, default=0)
    sub.add_parser("run-all")
    sub.add_parser("judge")
    sub.add_parser("sentinels")
    a = ap.parse_args()
    if a.cmd == "run":
        run_route(a.route, a.split, a.repeat, a.limit)
    elif a.cmd == "run-all":
        run_all()
    elif a.cmd == "judge":
        judge()
    else:
        sentinels()


if __name__ == "__main__":
    main()
