#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""V0-B 合成工单语料生成器（正式轮语料；修 V0-B 试点发现的模板泄漏）。

设计（相对 scripts/v0a_ticket_gen.py 的变更，判真值语义 R1–R5 不变）：

1. **dev↔holdout 模板族不相交**（阻塞性修复）。V0-A 的 dev/holdout 共用同一批
   模板帧与噪声池，精确标题/正文匹配即可从 dev 迁移到 holdout（试点 E0 全关臂
   靠它拿满 1.0）。本生成器：
   - dev 族 = V0-A 模板族（BUILDERS/TITLES/FILLERS 等按原样复用——历史分布
     连续性：规则归纳源头不变，试点/V0-A 的 dev 侧结论可对照），新 seed 重抽；
   - holdout 族 = 全新模板帧/标题帧/噪声池（FILLERS_HOLD 等），与 dev 族
     逐句不相交——正文核心句、标题、噪声句在两 split 间均无逐字迁移面。
2. **槽位池 = 共享基座 ∪ holdout 增量词**。类指示词汇（退款/验证码/报错……）
   必须跨 split 迁移，否则任务不可解；holdout 增量词（新区位/新报错词/新功能
   名……）制造词汇移位——dev 归纳的关键词规则只覆盖基座部分。
3. **噪声型新增**（"更严移位"的噪声维）：holdout 大档的新增点——问候/签名/
   环境行格式不同、核心句可被埋在文中（不总在句首）、复述用不同引导语。
4. **E1 v2 移位方向偏难**（修 V0-A §5"移位偏易"遗留）：大档加权、必加噪声行、
   增量槽词加权、信号必埋深。方向如实登记为偏难。
5. 计数结构与 V0-A 相同（dev 192 / holdout 288 / e1 288）。**id 不透明**（shuffle
   后连续号 `d-0001`/`h-0001`/`e1-0001`，与 label 零相关）——首跑实测 glm 会在
   规则变难时主动验证 id 前缀与 label 的相关并走捷径，金标不得进入待判 id。
   确定性：单 Random 顺序消费，版本 = 本文件 commit + seed。

自检（--check）：帧不相交（holdout 核心句/标题不出现在 dev 正文/标题中）、
id 唯一、词表合法、精确匹配覆盖 = 0、dev 二元关键词规则对 holdout 的 F1
（非饱和探针——预期中值，1.0 即仍有泄漏）。

输出（--out 目录，默认 v0b_artifacts/）：
  tickets_dev.jsonl / tickets_holdout.jsonl / tickets_e1.jsonl
  tickets_neg.jsonl（V0-A 负例沿用）/ sentinels.jsonl（C3② 固定契约沿用）
  gen_stats.json
"""
import argparse
import json
import os
import random
import sys
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import v0a_ticket_gen as v0a  # dev 族 = V0-A 模板族原样复用

SEED_MAIN = 20260923   # dev + holdout（单 Random 顺序消费）
SEED_E1 = 20260924     # e1（偏难移位）

COUNTS_DEV = {  # 4×33 + 2×19 + 22 = 192
    "bug": 33, "feature": 33, "account": 33, "howto": 33,
    "billing": 19, "complaint": 19, "unsure": 22,
}
COUNTS_HOLD = {  # 4×51 + 2×29 + 26 = 288
    "bug": 51, "feature": 51, "account": 51, "howto": 51,
    "billing": 29, "complaint": 29, "unsure": 26,
}
COUNTS_E1 = dict(COUNTS_HOLD)

# ── holdout 增量槽位（词汇移位：dev 规则只覆盖共享基座部分） ──────────
AREAS_X = ["视频会议", "考勤打卡", "审批流程", "在线表格", "任务看板",
           "共享日历", "公告栏", "工作台首页"]
ERRS_X = ["点不动", "一直转圈没反应", "内容显示不全", "数据不同步",
          "加载到一半停住", "按钮灰色不可点"]
FEATS_WANTED_X = ["深色模式跟随系统", "全局消息搜索", "多端实时同步",
                  "消息收藏夹导出", "看板自定义列", "会议纪要自动生成"]
FEATS_EXIST_X = ["勿扰模式", "屏幕共享", "文档评论", "日程共享", "密码锁屏"]
OPS_X = ["注销账号", "导出考勤记录", "调整看板列宽", "退出团队",
         "修改会议密码", "关闭自动续费提醒"]
ACCT_ERRS_X = ["身份验证失败", "提示账号状态异常", "扫码登不上去"]
REASONS_B_X = ["续费日期记错多充了", "家庭版买成了个人版", "试用结束直接扣费"]
CHANNELS_X = ["工信部投诉", "消费者协会", "老板那边施压"]

def merge(base, extra, extra_w=1):
    """基座 ∪ 增量；extra_w>1 时增量加权（e1 偏难用）。"""
    return list(base) + list(extra) * extra_w


# ── holdout 族模板（帧与 V0-A 逐句不相交；语义对应 R1–R5） ────────────
# 约束沿用 V0-A：bug/howto 帧不指名登录/支付环节（R1 真值矛盾防护）。
def th_bug(r, hard=False):
    t = [
        "用着用着{a}就{e}了，等了十分钟也没恢复",
        "{a}这边{e}，换了个时间段再来还是老样子",
        "一进{a}就{e}，根本进不去正常界面",
        "点开{a}以后{e}，页面上的按钮全都失效了",
        "团队里三个人都反映{a}{e}，应该不是个例",
        "{a}从上个星期开始{e}，一直没修复",
        "编辑到一半{a}{e}，前面的内容也丢了",
        "想用{a}的时候它{e}，每次都要刷新好几遍",
    ]
    areas = merge(v0a.AREAS, AREAS_X, 2 if hard else 1)
    errs = merge(v0a.ERRS, ERRS_X, 2 if hard else 1)
    return r.choice(t).format(a=r.choice(areas), e=r.choice(errs))

def th_feature(r, hard=False):
    t = [
        "要是能{f}就好了，现在这块完全空白",
        "强烈希望官方把{f}排上日程",
        "别的工具早就支持{f}了，咱们什么时候跟上",
        "一个小愿望：把{f}加上，日常使用会顺手非常多",
        "每次到这一步都得手工来，要是产品自带{f}能省好多事",
        "恳请评估一下{f}的可行性，用户群里问的人很多",
        "现在这种做法太绕了，直接支持{f}不行吗",
        "产品经理看过来：{f}是刚需，别家都有",
    ]
    fw = merge(v0a.FEATS_WANTED, FEATS_WANTED_X, 2 if hard else 1)
    return r.choice(t).format(f=r.choice(fw))

def th_account(r, hard=False):
    t = [
        "输完密码以后提示{ae}，反复试了好几遍",
        "验证码等了五分钟都没收到，换个手机再试还是收不到",
        "通过找回密码的链接重设，最后一步总是{ae}",
        "新手机登不上旧账号，提示需要原设备确认，原设备早扔了",
        "改绑手机号流程走完显示成功，实际还是老号码在登录",
        "子账号突然全部掉线，主账号也提示{ae}",
        "激活邮件被退回，注册流程卡在第一步",
        "两步验证的备用码丢了，现在完全进不去账号",
    ]
    ae = merge(v0a.ACCT_ERRS, ACCT_ERRS_X, 2 if hard else 1)
    return r.choice(t).format(ae=r.choice(ae))

def th_billing(r, hard=False):
    t = [
        "这期账单比往常多了一笔，查不到对应订单，要求核实并退回",
        "{rb}，申请把这笔费用原路退回",
        "支付页面显示成功，账户里却查不到这笔订单",
        "会员权益没有生效但是钱已经扣了，请尽快处理",
        "同一笔订单扣了两次费用，多扣的那笔必须退",
        "开发票的入口点进去是空白，发票一直开不出来",
        "续费提醒说今天到期，昨天已经手动续过了今天又扣了一次",
        "退款申请提交了三天没有任何进度，钱还没到账",
    ]
    rb = merge(v0a.REASONS_B, REASONS_B_X, 2 if hard else 1)
    return r.choice(t).format(rb=r.choice(rb))

def th_complaint(r, hard=False):
    t = [
        "给你们提了这么多次意见，一次像样的回复都没有，我要一个正式答复",
        "处理速度慢到离谱，同一件事催了四遍，我要投诉到底",
        "换了个客服又是从头描述一遍问题，这种流程我受够了",
        "说了多少次了还没解决，再这样我只能去{c}了",
        "这就是你们对老用户的态度？我要求你们领导给我回电",
        "每次都说加急，每次都没下文，今天必须给我个说法",
        "工单来回踢皮球，一个星期了毫无进展，正式投诉",
    ]
    ch = merge(v0a.CHANNELS, CHANNELS_X, 2 if hard else 1)
    return r.choice(t).format(c=r.choice(ch))

def th_howto(r, hard=False):
    t = [
        "新手一枚，想问下{a}要怎么{o}？",
        "{f}这个功能是有还是没有？有的话从哪里进",
        "找了一圈设置项，没弄明白在哪里可以{o}",
        "求助：{a}下面能不能{o}？步骤是什么",
        "公司刚迁过来，请教一下{o}的操作入口在哪",
        "客服在吗，想咨询{a}如何{o}，谢谢",
        "看了帮助中心还是没找到，到底怎么{o}",
    ]
    areas = merge(v0a.AREAS, AREAS_X, 2 if hard else 1)
    ops = merge(v0a.OPS, OPS_X, 2 if hard else 1)
    fe = merge(v0a.FEATS_EXIST, FEATS_EXIST_X, 2 if hard else 1)
    return r.choice(t).format(a=r.choice(areas), o=r.choice(ops), f=r.choice(fe))

TH_BUILDERS = {"bug": th_bug, "feature": th_feature, "account": th_account,
               "billing": th_billing, "complaint": th_complaint, "howto": th_howto}

# ── 锚词稀释子族（v1.2：规则层覆盖上限的真来源） ──────────────────────
# 口语化改写，弱化/去类指示锚词（报错/退款/验证码/投诉……）；真值仍 = 生成
# 意图且对读者无歧义。共享基座槽位的锚词迁移只能覆盖标准子族——稀释子族
# 是语义残差层（llm 路线的价值面）。
def th_bug_dil(r, hard=False):
    t = [
        "昨晚开始这边整个卡住了，什么都点不了，等了快一个小时",
        "一直转圈进不去正常界面，刷新也没用",
        "按了没反应，换个入口还是一样",
        "内容加载一半就停住了，等半天也不动",
        "编辑的东西突然全没了，之前都没出过这种事",
        "每次到这一步就卡死，只能整个重来一遍",
    ]
    areas = merge(v0a.AREAS, AREAS_X)
    if r.random() < 0.3:
        return r.choice(t) + f"（{r.choice(areas)}）"
    return r.choice(t)

def th_feature_dil(r, hard=False):
    t = [
        "能不能把这两步合成一步？每次都要来回切换太折腾了",
        "这个操作系统能不能记住我上次的选择，每次都要重设",
        "两边内容对不上，得手动同步一遍，这块什么时候能自动化",
        "每次找人帮忙都得截图加文字描述一遍，太费劲了",
        "导出来的东西格式乱七八糟，还得二次加工",
        "这么基础的能力居然要绕一大圈才能做到",
    ]
    return r.choice(t)

def th_account_dil(r, hard=False):
    t = [
        "换了新手机以后，原来那个空间怎么都进不去了",
        "扫码那个界面一直出不来，换了好几个浏览器",
        "注册到一半就断了，再试又得从头来",
        "里面的资料想挪到另一个身份下，找不到任何入口",
        "按提示操作完还是进不去，卡在最后一步",
        "一登录就把我踢出来，来来回回好几次",
    ]
    return r.choice(t)

def th_billing_dil(r, hard=False):
    t = [
        "钱付了半天东西没到，订单状态也看不出个所以然",
        "上个月多了一笔支出，找不到对应的开头记录",
        "说好的价格跟最后收的不一样，差了一截",
        "买完以后权益没变化，找谁处理",
        "到日子了它自己又收了一次，之前明明关过",
        "想开个凭证一直开不出来，每次点进去都是空的",
    ]
    return r.choice(t)

def th_complaint_dil(r, hard=False):
    t = [
        "第三天了，还是没人理我，你们就这样对待客户？",
        "每次来都是同一套话术，事情一点没推进，我受够了",
        "说二十四小时回复，现在七十二小时了，呵呵",
        "来回踢皮球，谁都不接这个事，给大家提个醒吧",
        "处理成这样，我还怎么敢续费用下去",
        "你们内部到底有没有人跟进？给个准话",
    ]
    return r.choice(t)

def th_howto_dil(r, hard=False):
    t = [
        "这块在哪里设置啊，翻了半天设置项",
        "想把我这边的排期给同事看，要怎么弄",
        "刚开通不会用，有没有入门的走法说明",
        "这两个长得太像了，分不清该用哪个",
        "之前设置过的东西想改回来，忘记从哪儿进的",
        "能不能做到自动保存？在哪里看有没有开",
    ]
    return r.choice(t)

TH_DIL = {"bug": th_bug_dil, "feature": th_feature_dil, "account": th_account_dil,
          "billing": th_billing_dil, "complaint": th_complaint_dil,
          "howto": th_howto_dil}

# ── 锚词冲突子族（v1.2：关键词规则的反例——锚词在场但 R1–R5 判另一类） ──
# 每条真值唯一且由 R1–R5 决定；按目标类分池（占各类自身配额）：
# R1 支付环节报错→billing（"报错"锚词误导 bug 规则）；R3 明确业务诉求+情绪
# →业务类（"态度/不满"锚词误导 complaint）；R2 想要没有的功能→feature；
# R1 登录环节报错→account；howto 问的是"投诉入口"这个用法本身。
CONFLICT = {
    "billing": [
        "支付那一步一直报错重试了好几次，钱倒是扣了，先把这笔处理掉",
        "退款诉求：会员扣费两个月都没到账，态度什么的我不追究，钱退我",
    ],
    "bug": [
        "消息通知里点链接会直接退出应用，这个赶紧修一下，别的都能用",
        "导出的发票文件打开是乱码，财务那边催得急",
    ],
    "feature": [
        "对现有体验很不满：日程不支持共享给我们组造成了很大麻烦，把共享加上比什么都强",
    ],
    "complaint": [
        "报错截图我都整理好了发过三遍，没有一个工程师联系我，我要正式投诉这个处理流程",
    ],
    "account": [
        "支付页点不动根本走不到付款，先让我能登录再说别的",
    ],
    "howto": [
        "投诉入口在哪里？我要自助提交一个工单",
    ],
}

P_DIL = 0.35   # 稀释子族占比（holdout/e1 非 unsure 类）
P_CONF = 0.10  # 冲突子族占比

TITLES_HOLD = {
    "bug": ["{a}有问题", "{a}不好用了", "反复{e}", "用不了{a}"],
    "feature": ["产品建议", "希望增加{f}", "提个需求", "{f}什么时候有"],
    "account": ["登不上", "手机号换绑", "账号求助", "验证问题"],
    "billing": ["多扣费了", "费用疑问", "退款进度", "发票问题"],
    "complaint": ["不满", "要求答复", "投诉处理慢", "客服流程问题"],
    "howto": ["请教一下", "功能入口", "操作问题", "咨询使用"],
    "unsure": ["在吗", "在线等", "帮个忙", "尽快回"],
}

# 中性标题池（v1.3）：与全部类标题池（含 unsure 的短句）逐字不相交。
# v1.2 实测：每类 4 个固定标题帧 = 标签预言机——deepseek 终判器只查标题表
# 就 288/288（正文难度全部作废；冲突票还被类标题剧透答案）。v1.3 规则：
# 稀释/冲突票标题一律中性（难度住在正文）；标准票 60% 类标题 / 40% 中性
# （类标题是合法部分信号，与锚词同待遇，规则可利用但覆盖有上限）。
NEUTRAL_TITLES = ["问题反馈", "请看一下", "处理一下", "有个情况", "反馈一下",
                  "麻烦看看", "来个人看看", "尽快处理"]

# ── holdout 噪声族（新噪声型：句式与 V0-A 不逐字重合） ────────────────
GREETINGS_HOLD = ["hi，", "负责人好，", "同学你好，", "IT 部门同事你们好，",
                  "第三次发消息了，", "版主看这里，", "各位大大，"]
SIGS_HOLD = ["【来自 {} 网页版】", "（代部门同事提交）", "【优先级：高】",
             "——有问题随时电话联系我", "【工号反馈渠道】"]
FILLERS_HOLD = [
    "这个问题之前提过一次，当时说会跟进，后来就没有下文了",
    "我们整个部门都在用这个功能，现在大家的进度都停在这里",
    "试过换账号、换浏览器、换电脑，能想到的都试了",
    "如果需要录屏或者日志，我可以马上提供",
    "麻烦给个大概的处理时间，我好安排这边的计划",
    "昨天还能正常用，今天一早就不行了",
    "截图我已经放在附件里了，麻烦认真看一下",
    "这个问题每个月都会冒出来一次，真的很影响信任感",
    "身边同事用的都没事，就我这边一直这样",
    "希望能有个彻底的解决方案，不要每次只是临时恢复",
]
TRIED_LINES_HOLD = [
    "按官网 FAQ 的步骤逐条核对过，没有遗漏",
    "卸载重装了客户端，问题照旧",
    "清理了本地缓存和网络 DNS，仍然复现",
]
ENV_HOLD = "【环境信息】设备：{d}；版本 {v}；网络：{n}；复现频率：{q}"
NETS_HOLD = ["办公室 Wi-Fi", "家用宽带", "手机 4G", "公司 VPN"]
FREQS_HOLD = ["十次里八次", "偶尔一次", "每次必现", "只在工作时间出现"]
IMPACTS_HOLD = [
    "因为这事我已经加了两天的班，再不解决真的要影响交付了",
    "团队里十几个人都等着这个功能恢复，每天都在问进度",
    "客户那边已经在催了，这个再卡下去要出大问题",
    "我个人可以等，但整个项目排期等不起",
]
UNSURE_VAGUE_HOLD = ["在吗在吗", "快点回复啊", "急事！！", "有人处理吗",
                     "等着用呢", "怎么没人", "尽快！！", "看过来"]
UNSURE_GARBLED_HOLD = ["？？？在吗？？", "ddddddd ！！", "（（）））!!!",
                       "这是个测试嘛", "呃呃呃呃呃", "111222333",
                       "谁谁谁？？", "、、、、、、"]
UNSURE_MIX_JOINS_HOLD = ["。对了还有，", "。顺便，", "。另外一件事，", "。一起问了，"]

def _join(*parts):
    return v0a._join(*parts)

def th_unsure(r, hard=False):
    k = r.random()
    if k < 13 / 26:
        pick = r.sample(["bug", "feature", "account", "billing", "howto"], 2)
        a = TH_BUILDERS[pick[0]](r, hard).rstrip("。！？")
        b = TH_BUILDERS[pick[1]](r, hard).rstrip("。！？")
        return a + r.choice(UNSURE_MIX_JOINS_HOLD) + b
    if k < 20 / 26:
        return r.choice(UNSURE_VAGUE_HOLD)
    return r.choice(UNSURE_GARBLED_HOLD)

TH_BUILDERS["unsure"] = th_unsure


def make_title_hold(r, cls, kind="std"):
    """v1.3：稀释/冲突票标题一律中性；标准票 60% 类标题 / 40% 中性。"""
    if kind in ("conf", "dil") or r.random() < 0.4:
        return r.choice(NEUTRAL_TITLES)
    t = r.choice(TITLES_HOLD[cls])
    areas = merge(v0a.AREAS, AREAS_X)
    errs = merge(v0a.ERRS_SHORT, ["失灵", "打不开"])
    fw = merge(v0a.FEATS_WANTED + v0a.FEATS_EXIST,
               FEATS_WANTED_X + FEATS_EXIST_X)
    return t.format(a=r.choice(areas), e=r.choice(errs), f=r.choice(fw))


def pick_subfamily(r, cls):
    if cls == "unsure":
        return "std"
    u = r.random()
    if u < P_CONF and cls in CONFLICT:
        return "conf"
    if u < P_CONF + P_DIL:
        return "dil"
    return "std"


def hold_core(r, cls, hard, kind):
    if kind == "conf":
        return r.choice(CONFLICT[cls])
    if kind == "dil":
        return TH_DIL[cls](r, hard)
    return TH_BUILDERS[cls](r, hard)


def make_ticket_hold(r, cls, hard=False):
    """holdout 族票据。大档新增：核心句可埋深（不总在句首）、复述引导语不同。
    hard（e1 偏难）：大档必选、必加一行噪声、增量槽已在 builder 里加权。"""
    tier = 2 if hard else r.choice([0, 1, 2])
    kind = pick_subfamily(r, cls)
    base = hold_core(r, cls, hard, kind)
    if cls == "unsure":
        return {"title": make_title_hold(r, cls, kind), "body": base, "tier": tier}
    if tier == 0:
        body = base
    elif tier == 1:
        body = _join(base, r.choice(FILLERS_HOLD), r.choice(TRIED_LINES_HOLD))
        while len(body) < 80:
            body = _join(body, r.choice(FILLERS_HOLD))
    else:
        pre = r.choice(GREETINGS_HOLD)
        sig = r.choice(SIGS_HOLD)
        sig = sig.format(r.choice(NETS_HOLD)) if "{" in sig else sig
        env = ENV_HOLD.format(d=r.choice(v0a.DEVICES), v=r.choice(v0a.VERSIONS),
                              n=r.choice(NETS_HOLD), q=r.choice(FREQS_HOLD))
        bury = r.random() < (1.0 if hard else 0.4)  # 核心句埋深：不总在句首
        if bury:
            # 核心句必须在场（判真值=生成意图），只是不居首：问候+首噪声之后
            lead = _join(pre + r.choice(FILLERS_HOLD), base)
        else:
            lead = _join(pre + base)
        body = _join(lead, r.choice(TRIED_LINES_HOLD), env,
                     r.choice(IMPACTS_HOLD), r.choice(FILLERS_HOLD))
        if not bury:
            body = _join(body, f"再描述一次：{base}")
        if hard:
            body = _join(body, r.choice(IMPACTS_HOLD))
        body = _join(body, sig)
        while len(body) < 200:
            body = _join(body, r.choice(FILLERS_HOLD + IMPACTS_HOLD))
    return {"title": make_title_hold(r, cls, kind), "body": body, "tier": tier}


def gen_split(r, counts, family, hard=False, prefix=None):
    """family: 'dev'（V0-A 族）| 'hold'（holdout 族）。prefix 缺省按 family，
    e1 复用 holdout 族但打自己的前缀。

    **id 不透明**（连续号，shuffle 之后编号）：正式轮首跑实测 glm 在帧不相交、
    规则变难时会主动找捷径——验证 id 前缀与 dev label 的相关后直接
    `label = id.split('-')[1]` 拿满。金标不得以任何形式进入待判文件的 id。"""
    out = []
    if prefix is None:
        prefix = {"dev": "d", "hold": "h"}[family]
    for cls in ["bug", "feature", "account", "howto", "billing", "complaint", "unsure"]:
        for _ in range(counts[cls]):
            if family == "dev":
                t = v0a.make_ticket(r, cls)
            else:
                t = make_ticket_hold(r, cls, hard)
            out.append((t, cls))
    r.shuffle(out)
    for i, (t, cls) in enumerate(out, 1):
        t["id"] = f"{prefix}-{i:04d}"
        t["label"] = cls
    return [t for t, _ in out]


def core_sentences(rows):
    """粗提核心句：正文按句切的第一/最后一个非噪声短句（泄漏自检用，
    只需捕捉'帧逐字迁移'，不做精确语言学切分）。"""
    noise_words = ("设备", "环境信息", "版本", "再描述一次", "再把问题重复一遍",
                   "——", "【", "（")
    out = set()
    for x in rows:
        for s in x["body"].replace("！", "。").replace("？", "。").split("。"):
            s = s.strip()
            if 6 <= len(s) <= 40 and not any(w in s for w in noise_words):
                out.add(s)
        out.add(x["title"])
    return out


def check(dev, hold, e1):
    problems = []
    ids = [x["id"] for x in dev + hold + e1]
    if len(ids) != len(set(ids)):
        problems.append("id 重复")
    vocab = set(v0a.COUNTS) | {"unsure"}
    # id 卫生：待判文件的 id 不得携带类名（正式轮首跑实测 glm 会找 id 捷径）
    for x in hold + e1:
        if any(c in x["id"] for c in vocab):
            problems.append(f"id 含类名: {x['id']}")
            break
    for x in dev + hold + e1:
        if x["label"] not in vocab:
            problems.append(f"非法 label: {x['label']}")
            break
    # 帧不相交：holdout/e1 的句子与标题不得逐字出现在 dev 句集里
    dev_cores = core_sentences(dev)
    leaked = sum(1 for x in hold + e1
                 if x["title"] in dev_cores
                 or any(s.strip() in dev_cores
                        for s in x["body"].split("。")))
    if leaked:
        problems.append(f"holdout/e1 有 {leaked} 条与 dev 逐字重叠")
    # 精确匹配覆盖（E0 式攻击）：holdout 整条 (title, body) 在 dev 中出现
    dev_pairs = {(x["title"], x["body"]) for x in dev}
    exact = sum(1 for x in hold if (x["title"], x["body"]) in dev_pairs)
    if exact:
        problems.append(f"精确匹配覆盖 {exact} 条（应为 0）")
    # 非饱和探针：dev 二元关键词规则迁移到 holdout 的 F1（预期中值；1.0 = 仍泄漏）
    def bigrams(text):
        t = text.replace(" ", "")
        return {t[i:i+2] for i in range(len(t) - 1)}
    bg = Counter()
    for x in dev:
        for g in bigrams(x["title"] + x["body"]):
            bg[(x["label"], g)] += 1
    cls_tot = Counter(x["label"] for x in dev)
    # 每类取 dev 出现 ≥3 次且不在他类出现的独占二元组作规则
    rules = {}
    for (c, g), n in bg.items():
        if n >= 3 and all(not bg.get((o, g)) for o in cls_tot if o != c):
            rules.setdefault(c, set()).add(g)
    tp, fp, fn = Counter(), Counter(), Counter()
    for x in hold:
        gs = bigrams(x["title"] + x["body"])
        hits = {c: len(gs & g) for c, g in rules.items()}
        best = max(hits, key=lambda c: hits[c]) if any(hits.values()) else None
        if best == x["label"]:
            tp[x["label"]] += 1
        else:
            fn[x["label"]] += 1
            if best:
                fp[best] += 1
    f1s = []
    for c in cls_tot:
        p = tp[c] / (tp[c] + fp[c]) if tp[c] + fp[c] else 0.0
        rc = tp[c] / (tp[c] + fn[c]) if tp[c] + fn[c] else 0.0
        f1s.append(2 * p * rc / (p + rc) if p + rc else 0.0)
    probe_f1 = round(sum(f1s) / len(f1s), 4)
    return problems, probe_f1, leaked


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", default="v0b_artifacts")
    ap.add_argument("--check", action="store_true",
                    help="只对已生成文件跑泄漏自检，不重新生成")
    args = ap.parse_args()

    def load(split):
        return [json.loads(l) for l in
                open(os.path.join(args.out, f"tickets_{split}.jsonl"),
                     encoding="utf-8")]

    if args.check:
        dev, hold, e1 = load("dev"), load("holdout"), load("e1")
        problems, probe_f1, leaked = check(dev, hold, e1)
        print(json.dumps({"problems": problems, "kw_probe_macro_f1": probe_f1,
                          "verbatim_overlap": leaked}, ensure_ascii=False, indent=2))
        sys.exit(1 if problems else 0)

    os.makedirs(args.out, exist_ok=True)
    r = random.Random(SEED_MAIN)
    dev = gen_split(r, COUNTS_DEV, family="dev")
    hold = gen_split(r, COUNTS_HOLD, family="hold")
    re1 = random.Random(SEED_E1)
    e1 = gen_split(re1, COUNTS_E1, family="hold", hard=True, prefix="e1")

    def dump(name, rows, split):
        with open(os.path.join(args.out, name), "w", encoding="utf-8") as f:
            for x in rows:
                x2 = dict(x)
                x2["split"] = split
                f.write(json.dumps(x2, ensure_ascii=False) + "\n")

    dump("tickets_dev.jsonl", dev, "dev")
    dump("tickets_holdout.jsonl", hold, "holdout")
    dump("tickets_e1.jsonl", e1, "e1")
    # 负例与哨兵：V0-A 固定内容沿用（C3② 契约），负例属于 V0-A E3 实验遗留，
    # 生成保持文件集完整，正式 V0-B 轮未用不强制。
    neg = [{"id": f"neg-{i+1:02d}", "label": "unsure", "split": "neg",
            **p} for i, p in enumerate(
        [{"title": "（无主题）", "body": b, "tier": 1, "kind": k}
         for pool, k in [(v0a.NEG_CODE, "code"), (v0a.NEG_MKT, "mkt"),
                         (v0a.NEG_OTHER, "other")]
         for b in pool][:24])]
    dump("tickets_neg.jsonl", neg, "neg")
    dump("sentinels.jsonl",
         [{**s, "split": "sentinel"} for s in v0a.SENTINELS], "sentinel")

    problems, probe_f1, leaked = check(dev, hold, e1)
    stats = {
        "seed_main": SEED_MAIN, "seed_e1": SEED_E1,
        "version": "v0b-corpus v1.3（dev=V0-A 族新抽；新族偏难移位；id 不透明；锚词稀释 35% + 冲突 10%；稀释/冲突票标题中性、标准票 60/40）",
        "dev": len(dev), "holdout": len(hold), "e1": len(e1), "neg": len(neg),
        "dev_classes": dict(Counter(x["label"] for x in dev)),
        "holdout_classes": dict(Counter(x["label"] for x in hold)),
        "e1_classes": dict(Counter(x["label"] for x in e1)),
        "holdout_tiers": dict(Counter(x["tier"] for x in hold)),
        "e1_tiers": dict(Counter(x["tier"] for x in e1)),
        "kw_probe_macro_f1": probe_f1,
        "verbatim_overlap": leaked,
        "check_problems": problems,
    }
    with open(os.path.join(args.out, "gen_stats.json"), "w", encoding="utf-8") as f:
        json.dump(stats, f, ensure_ascii=False, indent=2)
    print(json.dumps(stats, ensure_ascii=False, indent=2))
    if problems:
        sys.exit(1)


if __name__ == "__main__":
    main()
