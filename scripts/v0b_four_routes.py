#!/usr/bin/env python3
"""四路线外部对照 runner（code_llm_eval_assembly.md §4 / code_llm_design.md §8.1）。

预登记（2026-09-23，code_v0b_report.md §6 唯一保留验证项，产品放行）：

问题：同一语料（v0b_artifacts v1.5 holdout 288）、同一任务口径（v0b_eval
TASK_TEXT 原文，含 v2.2 边界句）、同一质量门槛下，四路线的质量与成本对照
——裁决「run_code 降低总成本」价值主张（当前反面证据：基线手判最便宜）。

路线与调优资源（预登记，不许中途加码）：
  r1 外层直接处理     glm-5.3-flash 单次调用，dev+holdout 全量进上下文直判
                      （1 请求，max_tokens 32768）
  r2 强模型适度分组   glm-5.3（非 flash）：1 次归纳判定标准 + 每 24 条一组
                      ×12 组（13 请求，标准文件 groups 间复用）
  r3 普通脚本+模型API+文件库：dev 自动归纳规则库 lib.json（关键词→label
                      统计）+ 逐条 deepseek-flash 判别（288 请求，并发 4）
  r4 TinyAgent 编排   v0b_eval.py --arms e2 --max-tokens 32768（另行执行，
                      本脚本不覆盖）

公平性纪律：四路线同数据权限（dev+holdout）、同业务标准（TASK_TEXT 原文
嵌入）、同质量门槛（predictions 288 行、词表内、v0b_eval.score 计分）；
各路线外部实现合理调优但预先限定如上。成本分 route 记录 usage（已知量），
单位成功成本 = 总成本 / 成功交付（完整合法产物才算成功）。

工件：v0b_routes/<route>/{summary.json, predictions.jsonl, ...}。
烟雾门：--holdout 8 先跑三路线再放大（mini 纪律）。
"""
import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from v0b_eval import CORPUS, TASK_TEXT, VOCAB, score  # noqa: E402

ART = ROOT / "v0b_routes"
OUTER_FLASH = "glm-5.3-flash"
STRONG = "glm-5.3"
INNER = "deepseek-flash"


def gateway():
    kv = {}
    for line in (ROOT / "测试模型.md").read_text(encoding="utf-8").splitlines():
        m = __import__("re").match(r'\s*(?:#\s*)?([^=]+?)\s*=\s*"(.*)"\s*$', line)
        if m:
            kv[m.group(1).strip()] = m.group(2).strip()
    return kv["base url"], kv["api key"]


def chat(base, key, model, messages, max_tokens, usage):
    """单次 OpenAI-compatible 调用；5xx/网络错重试一次；usage 记账。"""
    body = json.dumps({"model": model, "messages": messages,
                       "max_tokens": max_tokens}).encode()
    last = None
    for attempt in range(2):
        try:
            req = urllib.request.Request(
                base.rstrip("/") + "/chat/completions", data=body,
                headers={"Content-Type": "application/json",
                         "Authorization": f"Bearer {key}"})
            with urllib.request.urlopen(req, timeout=600) as r:
                resp = json.load(r)
            u = resp.get("usage") or {}
            usage.append({"model": model, "in": u.get("prompt_tokens", 0),
                          "out": u.get("completion_tokens", 0)})
            return resp["choices"][0]["message"]["content"] or ""
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError,
                OSError, KeyError, json.JSONDecodeError) as e:
            last = e
            code = getattr(e, "code", None)
            if code is not None and 400 <= code < 500 and code != 429:
                raise   # 确定性 4xx 不重试（与核心 retry 分类同纪律）
            time.sleep(5 * (attempt + 1))
    raise RuntimeError(f"chat failed after retry: {last}")


def parse_preds(text):
    """从回复中抠 predictions 行（容忍围栏/前后杂文）。"""
    preds = {}
    for line in text.splitlines():
        line = line.strip().strip("`")
        if not line.startswith("{"):
            continue
        try:
            r = json.loads(line)
            if "id" in r and "label" in r:
                preds[r["id"]] = r["label"]
        except json.JSONDecodeError:
            pass
    return preds


def load_corpus(n_holdout):
    dev = [json.loads(l) for l in open(CORPUS / "tickets_dev.jsonl", encoding="utf-8")]
    hold = [json.loads(l) for l in open(CORPUS / "tickets_holdout.jsonl", encoding="utf-8")][:n_holdout]
    return dev, hold


def fmt_tickets(rows, with_label):
    out = []
    for r in rows:
        item = {"id": r["id"], "title": r["title"], "body": r["body"]}
        if with_label:
            item["label"] = r["label"]
        out.append(json.dumps(item, ensure_ascii=False))
    return "\n".join(out)


# ---------------- 路线 ----------------

def route_r1(base, key, dev, hold, out_dir):
    """外层直接处理：dev+holdout 全量一次进上下文。"""
    usage = []
    prompt = (
        TASK_TEXT + "\n\n（本环境无 workdir 工具，数据直接内嵌如下。）\n\n"
        "== dev.jsonl（带 label 金标）==\n" + fmt_tickets(dev, True) +
        "\n\n== holdout.jsonl（待分类）==\n" + fmt_tickets(hold, False) +
        "\n\n现在直接输出 predictions.jsonl 的全部内容：每行一个 JSON 对象 "
        '{\"id\": \"...\", \"label\": \"...\"}，共 %d 行，不要输出其他内容。'
        % len(hold))
    text = chat(base, key, OUTER_FLASH,
                [{"role": "user", "content": prompt}], 32768, usage)
    (out_dir / "response.txt").write_text(text, encoding="utf-8")
    return parse_preds(text), usage


def route_r2(base, key, dev, hold, out_dir):
    """强模型适度分组：1 次归纳标准 + 每组 24 条。"""
    usage = []
    criteria = chat(base, key, STRONG, [{"role": "user", "content":
        "以下是带金标的历史工单。请归纳一份判定标准（markdown，≤40 行）："
        "每个 label 的根因域定义、易混边界的裁决规则。词表固定为 "
        "bug / feature / account / billing / howto / complaint / unsure。\n\n"
        + fmt_tickets(dev, True)}], 32768, usage)
    (out_dir / "criteria.md").write_text(criteria, encoding="utf-8")
    groups = [hold[i:i + 24] for i in range(0, len(hold), 24)]
    preds = {}

    def one_group(g):
        prompt = ("按以下判定标准为每条工单判定 label（词表 bug / feature / "
                  "account / billing / howto / complaint / unsure）。\n\n"
                  "== 判定标准 ==\n" + criteria +
                  "\n\n== 工单 ==\n" + fmt_tickets(g, False) +
                  '\n\n每条输出一行 {"id": "...", "label": "..."}，'
                  "不要输出其他内容。")
        return parse_preds(chat(base, key, STRONG,
                                [{"role": "user", "content": prompt}],
                                32768, usage))
    with ThreadPoolExecutor(max_workers=4) as ex:
        for p in ex.map(one_group, groups):
            preds.update(p)
    return preds, usage


def route_r3(base, key, dev, hold, out_dir):
    """普通脚本+模型API+文件库：dev 规则库 lib.json + 逐条判别。"""
    usage = []
    # 文件库：dev 上每 label 的高区分度特征（ascii 词 + 中文 bigram）
    import re
    def toks(text):
        return set(re.findall(r"[a-zA-Z0-9]+", text)) | {
            text[i:i + 2] for i in range(len(text) - 1)
            if "一" <= text[i] <= "鿿" and "一" <= text[i + 1] <= "鿿"}
    kw = {}
    for r in dev:
        kw.setdefault(r["label"], Counter())
        for w in toks(r["title"] + " " + r["body"]):
            kw[r["label"]][w] += 1
    df = Counter(w for c in kw.values() for w in c)
    lib = {}
    for lab, c in kw.items():
        lib[lab] = [w for w, n in c.most_common(40)
                    if n >= 2 and df[w] < 3][:20]
    (out_dir / "lib.json").write_text(
        json.dumps(lib, ensure_ascii=False, indent=1), encoding="utf-8")
    lib_text = json.dumps(lib, ensure_ascii=False, indent=1)

    def one(row):
        prompt = ("按以下特征库与边界口径判定工单 label。\n\n== 边界口径 ==\n"
                  + TASK_TEXT.split("边界口径：")[1].split("把结果写到")[0].strip()
                  + "\n\n== 特征库（关键词 → label）==\n" + lib_text +
                  "\n\n== 工单 ==\n" + json.dumps(
                      {"id": row["id"], "title": row["title"], "body": row["body"]},
                      ensure_ascii=False) +
                  '\n\n只输出一行 {"id": "...", "label": "..."}。')
        # 输出档统一 32K（产品拍板 2026-09-23：思考税在 4096/8192 档均实测烧穿）
        return parse_preds(chat(base, key, INNER,
                                [{"role": "user", "content": prompt}],
                                32768, usage))
    preds = {}
    raws = open(out_dir / "responses.jsonl", "w", encoding="utf-8")
    with ThreadPoolExecutor(max_workers=4) as ex:
        for row, p in zip(hold, ex.map(one, hold)):
            preds.update(p)
            raws.write(json.dumps({"id": row["id"], "pred": p.get(row["id"])},
                                  ensure_ascii=False) + "\n")
    raws.close()
    return preds, usage


def summarize(route, out_dir, preds, usage, gold):
    # score() 读 <run_dir>/wd/predictions.jsonl——写进 wd 层保持单一计分源
    wd = out_dir / "wd"
    wd.mkdir(exist_ok=True)
    (wd / "predictions.jsonl").write_text(
        "".join(json.dumps({"id": i, "label": preds[i]}, ensure_ascii=False) + "\n"
                for i in preds), encoding="utf-8")
    s = score(out_dir, gold)
    tok = Counter()
    for u in usage:
        tok[u["model"]] += u["in"] + u["out"]
    complete = len(preds) == len(gold) and all(p in VOCAB for p in preds.values())
    summary = {"route": route, "score": s, "usage": usage,
               "tokens_by_model": dict(tok), "total_tokens": sum(tok.values()),
               "complete": complete, "n_requests": len(usage)}
    (out_dir / "summary.json").write_text(
        json.dumps(summary, ensure_ascii=False, indent=1), encoding="utf-8")
    return summary


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--routes", default="r1,r2,r3")
    ap.add_argument("--holdout", type=int, default=288)
    args = ap.parse_args()
    base, key = gateway()
    ART.mkdir(exist_ok=True)
    dev, hold = load_corpus(args.holdout)
    gold = {r["id"]: r["label"] for r in hold}
    for route in args.routes.split(","):
        out_dir = ART / route
        out_dir.mkdir(parents=True, exist_ok=True)
        fn = {"r1": route_r1, "r2": route_r2, "r3": route_r3}[route]
        print(f"[4route] {route} @ {len(hold)} ...", flush=True)
        t0 = time.time()
        try:
            preds, usage = fn(base, key, dev, hold, out_dir)
        except Exception as e:
            print(f"[4route] {route} FAILED: {e}", flush=True)
            continue
        s = summarize(route, out_dir, preds, usage, gold)
        print(f"[4route] {route} -> f1={s['score']['macro_f1']} "
              f"answered={s['score']['answered']}/{len(gold)} "
              f"tokens={s['total_tokens']} wall={time.time()-t0:.0f}s", flush=True)


if __name__ == "__main__":
    main()
