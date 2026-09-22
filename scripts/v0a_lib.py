#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""V0-A 纯代码库（路线②③④共用的"代码资产"）。

起草于开发集校准轮（2026-09-21，见 docs/code_v0a_recipe.md C/D 节），冻结后
不得按留出集成绩修改。规则镜像分类定义 v1 的边界规则（R1–R5）：
  R1 报错在登录/账号环节→account；支付环节→billing；其余→bug
  R2 问已有功能用法→howto；想要没有的→feature（问法词是 feature 的排除项）
  R3 明确业务诉求+情绪→业务类（complaint 排除一切具体诉求词）
  R4 多类竞争 = 模糊 → 回落模型（这正是边界该交模型的场景）
  R5 无 CJK 字符 = 结构性乱码 → unsure（信息过少/混合由模型判）
精度优先：宁可少覆盖，不可错配。
"""
import re

_QUESTION = r"(怎么|如何|在哪里|哪儿|哪个菜单|支不支持|操作路径|请问|新手求助)"
_FEE = r"(退款|退费|扣费|扣款|账单|发票|续费|充值|付款|支付失败|多扣|重复扣|价格|会员)"
_ACCT = r"(登录|密码|验证码|账号|绑定|解绑|换绑|找回|冻结|封禁|解封|注销|激活邮件|异地登录)"

# (rule_id, label, [必要正则（全部命中）], [排除正则（任一命中即失效）])
_RULES = [
    ("acct-login", "account", [_ACCT], [_FEE, _QUESTION]),
    ("bill-fee", "billing", [_FEE], []),
    ("feat-req", "feature",
     [r"(希望|建议|期待|想要|能不能加|可不可以|加一个|新增|做进去|功能反馈|需求反馈|等着这个功能)"],
     [_QUESTION]),
    ("howto-q", "howto", [_QUESTION], []),
    ("bug-err", "bug",
     [r"(报错|出错|错误提示|提示错误|异常|闪退|崩溃|白屏|卡死|加载不出来|丢失|没了|乱码|没反应|用不了)"],
     [_ACCT, _FEE]),
    ("comp-escalate", "complaint",
     [r"(投诉|给我一个说法|给个说法|升级处理|曝光|315|负责人出面|正式抗议|极其恶劣|忍无可忍|太差了|要个交代)"],
     [_FEE, _ACCT, r"(报错|闪退|崩溃|异常|加载不出来)"]),
    ("unsure-empty", "unsure", [], []),  # 占位：结构性判断在 classify 内做
]


def classify(title, body):
    """→ (label | None, rule_id | None)。None = 模糊 → 回落（外层/内层）。

    单类命中→该类；多类命中→模糊（R4）；全不命中→模糊。
    另：正文无任何 CJK 字符 → 结构性乱码 → unsure（R5）。"""
    text = (title or "") + " " + (body or "")
    if body and not re.search(r"[一-鿿]", body):
        return "unsure", "unsure-empty"
    # R4：混合标记（双诉求连接词）→ 强制模糊，交模型判 unsure
    if re.search(r"(另外|还有一个问题|顺便问一下|同时问)", body or ""):
        return None, None
    hits = []
    for rid, label, need, forbid in _RULES:
        if label == "unsure":
            continue
        if all(re.search(p, text) for p in need) and not any(
                re.search(p, text) for p in forbid):
            hits.append((rid, label))
    if len(hits) == 1:
        return hits[0][1], hits[0][0]
    return None, None


if __name__ == "__main__":
    # 开发集自检：覆盖率 / 覆盖精度 / 模糊率（校准轮用）
    import json
    import sys
    path = sys.argv[1] if len(sys.argv) > 1 else "v0a_artifacts/tickets_dev.jsonl"
    rows = [json.loads(l) for l in open(path, encoding="utf-8")]
    cov = correct = 0
    by_label = {}
    for x in rows:
        lbl, rid = classify(x["title"], x["body"])
        if lbl is not None:
            cov += 1
            correct += (lbl == x["label"])
            d = by_label.setdefault(x["label"], [0, 0])
            d[0] += (lbl == x["label"])
            d[1] += 1
    print(f"coverage {cov}/{len(rows)} = {cov/len(rows):.0%}, "
          f"precision on covered {correct}/{cov} = {correct/cov:.0%}, "
          f"ambiguous {(len(rows)-cov)/len(rows):.0%}")
    for k, (c, n) in sorted(by_label.items()):
        print(f"  {k:10s} covered {n:3d} correct {c:3d} ({c/n:.0%})")
