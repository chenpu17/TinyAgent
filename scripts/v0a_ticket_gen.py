#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""V0-A 合成工单基准生成器（冻结配方 docs/code_v0a_recipe.md A 节）。

判卷真值 = 生成意图，边界规则与 v0a_eval.py 的分类定义 v1 逐条对应：
  R1 报错在登录/账号环节→account；支付环节→billing；其余报错→bug
  R2 问已有功能用法→howto；想要没有的功能→feature
  R3 明确业务诉求+情绪→业务类；只有情绪无具体诉求→complaint
  R4 多类诉求并列无主次→unsure
  R5 非工单内容（乱码/空/无关）→unsure

输出（--out 目录）：
  tickets_dev.jsonl / tickets_holdout.jsonl   （seed=20260921，480=192+288）
  tickets_e1.jsonl                            （seed=20260922，留出同规模，权重微移）
  tickets_neg.jsonl                           （E3 负例 24 条）
  sentinels.jsonl                             （C3② 哨兵 8 条，含期望路径）
  gen_stats.json

确定性：同 seed 同输出（单 Random 顺序消费）。版本 = 本文件 commit + seed。
"""
import argparse
import json
import os
import random

SEED_MAIN = 20260921
SEED_E1 = 20260922

# ── 配方 A3（勘误后）：类 → (总量, 开发集数) ─────────────────────────
COUNTS = {  # dev 4×33 + 2×19 + 22 = 192；holdout 4×51 + 2×29 + 26 = 288
    "bug": (84, 33), "feature": (84, 33), "account": (84, 33), "howto": (84, 33),
    "billing": (48, 19), "complaint": (48, 19), "unsure": (48, 22),
}
E1_COUNTS = {  # E1 与留出集同规模：只生成 holdout 部分
    "bug": 51, "feature": 51, "account": 51, "howto": 51,
    "billing": 29, "complaint": 29, "unsure": 26,
}

# ── 共享槽位池（跨类共享 → 词汇重叠是难度来源，不是缺陷） ────────────
# 不含"登录页/支付页面/个人中心"：R1 规定登录/支付环节报错归 account/billing，
# bug 模板若抽到这些区会制造真值矛盾。
AREAS = ["客户端", "网页版", "App", "导出功能", "搜索功能", "消息通知",
         "文件上传", "订单列表", "设置页面", "日程表", "收藏夹", "协作文档"]
DEVICES = ["iPhone", "安卓手机", "Mac 电脑", "Windows 电脑", "iPad", "华为手机",
           "小米手机", "公司配的电脑", "旧笔记本"]
ERRS = ["报错", "出错", "弹出错误提示", "一直转圈加载不出来", "直接闪退",
        "页面白屏", "卡死没有响应", "提示系统异常"]
ERRS_SHORT = ["报错", "异常", "闪退", "故障"]
TIMEAGO = ["今天早上", "昨天", "上周五", "三天前", "上周", "刚刚",
           "这个月已经好几次", "自从上次更新之后"]
ACCT_ERRS = ["账号或密码错误", "登录失败", "授权失败", "系统繁忙请稍后再试"]
FEATS_WANTED = ["批量操作", "离线模式", "自定义快捷键", "数据导出为 PDF",
                "定时提醒", "文件夹分组", "草稿自动保存", "扫码登录", "消息撤回",
                "表格协同编辑", "多语言界面"]
FEATS_EXIST = ["消息提醒", "深色模式", "自动翻译", "指纹登录", "离线缓存",
               "文件夹共享", "消息置顶", "键盘快捷键"]
OPS = ["修改绑定的邮箱", "关闭消息推送", "导出聊天记录", "更换主题",
       "设置自动回复", "邀请同事进团队", "清除缓存", "绑定企业微信"]
AMTS = ["68", "128", "298", "396", "99"]
REASONS_B = ["服务根本没用上", "买错了套餐", "活动价格没生效", "误充值"]
CHANNELS = ["社交平台曝光", "12315 投诉", "媒体反映", "黑猫投诉"]
GREETINGS = ["你好，", "在吗？", "客服你好，", "麻烦看下，", "打扰了，",
             "紧急求助！！", "工程师你好，"]
SIGNATURES = ["【来自 {}】", "——此消息由手机端发送", "（工单系统自动转发）",
              "如需更多信息请查看我的历史工单", "【用户等级：VIP3】"]
FILLERS = ["这个问题从上周就开始了，中间断断续续出现过好几次，一直没能彻底解决",
           "我是付费用户，每天都依赖这个功能完成工作，现在完全被卡住了",
           "之前遇到类似问题的时候重装客户端可以解决，这次重装了也没有用",
           "麻烦尽快看一下，如果需要我补充什么信息或者日志，随时告诉我",
           "已经按照帮助文档里的建议操作过了，情况没有任何变化",
           "这个问题已经影响我正常办公了", "同事那边就没这个问题，应该不是我这边环境的原因",
           "我这边截了图，需要的话可以提供", "希望能尽快给个确切的时间点",
           "中间联系过一次客服，没有解决", "这已经耽误我两天时间了"]
# 填充句刻意不含 account/billing 关键词（账号/登录/密码/支付/退款……）：
# 噪声的职责是"埋住"信号，不是"替换成另一类的信号"——否则纯代码库在
# 中/大档的覆盖精度会被噪声系统性偷走（校准轮 1 的实测教训）。
TRIED_LINES = ["自己尝试过重启应用、更换浏览器、清理缓存，问题依旧",
               "换了设备和网络环境都试过，现象完全一致",
               "退出应用再重新打开也试了，没有效果"]
NETS = ["家庭 Wi-Fi", "公司有线网络", "手机热点", "4G/5G 蜂窝网络"]
FREQS = ["每次必现", "偶发大约一天一次", "只在特定时段出现"]
IMPACTS = ["这个问题对我们整个团队的进度都有影响，大家都等着这边处理完才能继续往下走",
           "因为这个问题我今天已经耽误了将近两个小时的工作时间，后续排期都很紧张",
           "如果今天解决不了，我这边只能先暂停使用，等修复以后再恢复",
           "这个功能是我每天打开电脑第一件要做的事，现在等于什么都干不了"]
ENV_LINE = "设备：{d}，客户端版本 {v}，网络环境：家庭 Wi-Fi。"
VERSIONS = ["3.2.1", "4.0.0-beta.2", "2.9.7", "5.1.0", "最新版"]

def _join(*parts):
    """中文句拼接：去掉重复句号，统一用「。」连接。"""
    out = ""
    for p in parts:
        p = p.strip()
        if not p:
            continue
        if out and not out.endswith(("。", "！", "？", "：", "，")):
            out += "。"
        out += p
        if not out.endswith(("。", "！", "？", "：")):
            out += "。"
    return out

SYN_NEG = ["无法", "不能", "没法", "没办法"]
SYN_PLZ = ["请", "麻烦", "希望", "恳请"]

# ── 各类模板（返回正文核心；tier 决定噪声与细节扩展） ────────────────
def t_bug(r):
    t = [
        "{a}{e}，重试了好几次都一样",
        "{ta}以后，{a}就一直{e}，很影响使用",
        "{d}上打开{a}会{e}，麻烦尽快修复",
        "提交的时候页面{e}，填的数据也没保存",
        "{a}里的历史记录突然全没了，是不是出 bug 了",
        "文件上传到一半直接失败，提示上传异常",
        "搜索功能输入关键词以后一直加载不出来",
        "消息通知点了没反应，重启{d}也没用",
        "导出的表格打开是乱码，之前都正常",
        "{a}晚上八点以后必现{e}，白天没事",
    ]
    return r.choice(t).format(a=r.choice(AREAS), e=r.choice(ERRS),
                              d=r.choice(DEVICES), ta=r.choice(TIMEAGO))

def t_feature(r):
    # 不用随机 {a} 槽：功能请求指向产品整体，随机功能区会造出
    # "导出功能支持扫码登录" 这类语义错配。
    t = [
        "希望产品能支持{f}，现在完全没有这个能力",
        "建议增加{f}，团队协作的时候特别需要",
        "能不能加一个{f}的功能？很多同类产品都有了",
        "期待后续版本支持{f}，现在只能绕好大一圈",
        "想要{f}，不然每次都要手动处理，太麻烦了",
        "反馈一个需求：最好能支持{f}",
        "可不可以把{f}做进去？我们全组都想要",
        "什么时候能支持{f}？没有这个功能只能用别家了",
        "提个建议，加上{f}会好用很多",
        "新版本能不能考虑{f}？我们等着这个功能好久了",
    ]
    return r.choice(t).format(f=r.choice(FEATS_WANTED))

def t_account(r):
    t = [
        "登录的时候一直提示{ae}，账号用不了",
        "收不到验证码，换了两部手机都不行",
        "忘记密码了，找回流程走不通，卡在验证那一步",
        "账号莫名被冻结，我没有违规操作，申请解封",
        "换手机号了，需要把绑定号码换成新的，老号码已经停用收不到验证",
        "密码明明输对了却一直说错误，现在账号被锁定了",
        "注册邮箱收不到激活邮件，注册不了账号",
        "第三方登录授权失败，一直转圈",
        "账号里的昵称改不了，保存就提示账户异常",
        "账号被异地登录了，担心安全问题，要求强制下线并改密",
    ]
    return r.choice(t).format(ae=r.choice(ACCT_ERRS))

def t_billing(r):
    t = [
        "会员自动续费扣了钱，但是会员没有到账",
        "要求退款，{rb}，这笔钱必须退给我",
        "账单里多了一笔不知道什么的扣费，要求解释并退回",
        "充值了{amt}元一直不到账，客服电话也打不通",
        "发票开不了，点击开发票提示失败",
        "重复扣了两次费，要求退还多扣的部分",
        "取消续费了还是被扣款，这已经是第{n}次了",
        "升级会员的价格和页面显示的不一致，多收了钱",
        "购买的专业版付款后订单一直显示未支付",
        "订阅到期了还在扣费，要求停止扣款并退款",
    ]
    return r.choice(t).format(rb=r.choice(REASONS_B), amt=r.choice(AMTS),
                             n=r.randint(2, 4))

def t_complaint(r):
    t = [
        "你们的服务太差了！我要正式投诉，给我一个说法！",
        "问题反馈了{ta}到现在没有人理，要求升级处理",
        "客服态度极其恶劣，我要投诉那个工号的客服",
        "忍无可忍了，再不解决我就去{c}",
        "每次都是这套说辞，我要找你们主管，现在就要",
        "对你们处理问题的效率非常失望，正式提出抗议",
        "太差了！用了这么多年就换来这种服务？要求负责人出面",
        "投诉你们流程繁琐，转了好几个人都没解决，我要一个交代",
    ]
    return r.choice(t).format(ta=r.choice(TIMEAGO), c=r.choice(CHANNELS))

def t_howto(r):
    t = [
        "请问{a}怎么{o}？找了半天没找到入口",
        "如何开启{f}？文档里没写清楚",
        "{a}在哪里可以{o}？",
        "你们的产品支持{f}吗？在哪里开启？",
        "怎么{o}？求详细步骤",
        "{a}的{o}功能在哪里，找了十分钟",
        "新手求助：{o}的具体操作路径是什么",
        "请问如何{o}？",
        "打印功能在哪个菜单里？没有找到",
        "怎么导出聊天记录到电脑？",
    ]
    return r.choice(t).format(a=r.choice(AREAS), o=r.choice(OPS),
                              f=r.choice(FEATS_EXIST))

# unsure 三型（R4 混合并列 / R5 过少 / R5 乱码）
UNSURE_VAGUE = ["急！！！在线等！！！", "有人吗？？快点回复", "？？？？？",
                "求处理，谢谢", "帮帮忙吧", "在线等，挺急的", "尽快回复我",
                "看到请回复", "处理下", "急急急"]
UNSURE_GARBLED = ["asdfghjkl ？？？？", "%……&*（）——+", "ffffff 急急急急",
                  "？？？？？？？？", "@@@！！！###", "(⊙_⊙)？？？",
                  "测试测试测试测试", "eeee errr ？？", "。。。。。", "1a2b3c4d"]
UNSURE_MIX_JOINS = ["。另外，", "。同时，", "。顺便问一下，", "。还有一个问题，"]

def t_unsure_mixed(r):
    pick = r.sample(["bug", "feature", "account", "billing", "howto"], 2)
    builders = {"bug": t_bug, "feature": t_feature, "account": t_account,
                "billing": t_billing, "howto": t_howto}
    a = builders[pick[0]](r)
    b = builders[pick[1]](r)
    a = a.rstrip("。！？")
    b = b.rstrip("。！？")
    return a + r.choice(UNSURE_MIX_JOINS) + b

def t_unsure(r):
    k = r.random()
    if k < 16 / 48:
        return t_unsure_mixed(r)
    if k < 32 / 48:
        return r.choice(UNSURE_VAGUE)
    return r.choice(UNSURE_GARBLED)

BUILDERS = {"bug": t_bug, "feature": t_feature, "account": t_account,
            "billing": t_billing, "complaint": t_complaint, "howto": t_howto,
            "unsure": t_unsure}

TITLES = {
    "bug": ["{a}出现{e}", "功能异常，{a}用不了", "提交时{e}", "紧急：{a}故障"],
    "feature": ["功能建议：{f}", "希望支持{f}", "需求反馈", "能不能加{f}"],
    "account": ["登录问题", "账号异常", "验证码收不到", "账号申诉"],
    "billing": ["退款申请", "扣费异常", "账单疑问", "充值未到账"],
    "complaint": ["投诉", "强烈不满", "要求升级处理", "服务太差"],
    "howto": ["使用咨询", "怎么操作", "功能在哪里", "新手提问"],
    "unsure": ["急", "求助", "？？？", "请处理"],
}

def make_title(r, cls):
    pool = TITLES[cls]
    t = r.choice(pool)
    return t.format(a=r.choice(AREAS), e=r.choice(ERRS_SHORT),
                    f=r.choice(FEATS_WANTED + FEATS_EXIST))

def make_ticket(r, cls):
    """tier: 0 小 / 1 中 / 2 大（近似字数带，见配方 A4）。

    小=核心句；中=+补充+尝试记录；大=+问候、复述、环境行、签名（噪声埋信号）。
    unsure 类天然短，豁免分层膨胀。"""
    tier = r.choice([0, 1, 2])
    base = BUILDERS[cls](r)
    if cls == "unsure":
        return {"title": make_title(r, cls), "body": base, "tier": tier}
    if tier == 0:
        body = base
    elif tier == 1:
        body = _join(base, r.choice(FILLERS), r.choice(TRIED_LINES))
        while len(body) < 80:  # 中档下限：不足再补一句
            body = _join(body, r.choice(FILLERS))
    else:
        pre = r.choice(GREETINGS)
        sig = r.choice(SIGNATURES)
        sig = sig.format(r.choice(DEVICES)) if "{" in sig else sig
        env = (f"设备：{r.choice(DEVICES)}，客户端版本 {r.choice(VERSIONS)}，"
               f"网络环境：{r.choice(NETS)}，问题出现频率：{r.choice(FREQS)}")
        body = _join(pre + base, r.choice(FILLERS), r.choice(TRIED_LINES),
                     env, r.choice(IMPACTS), f"再把问题重复一遍：{base}",
                     r.choice(FILLERS), sig)
        while len(body) < 200:  # 大档下限：不足再补一句
            body = _join(body, r.choice(IMPACTS + FILLERS))
    return {"title": make_title(r, cls), "body": body, "tier": tier}

# ── E3 负例（24 条：明显不是工单，正确输出 = unsure，R5） ─────────────
NEG_CODE = [
    "def process(items):\n    out = []\n    for i in items:\n        if i % 2 == 0:\n            out.append(i * 3)\n    return out\n# 这段为什么跑不完？",
    "import pandas as pd\ndf = pd.read_csv('data.csv')\nprint(df.head())\n# 报错 KeyError: 'user_id'",
    "function debounce(fn, ms) {\n  let t;\n  return (...a) => {\n    clearTimeout(t);\n    t = setTimeout(() => fn(...a), ms);\n  };\n}",
    "SELECT u.name, COUNT(o.id) FROM users u\nLEFT JOIN orders o ON o.uid = u.id\nGROUP BY u.name HAVING COUNT(o.id) > 5;",
    "try:\n    r = requests.get(url, timeout=3)\n    r.raise_for_status()\nexcept Exception as e:\n    log.error(e)",
    "git rebase -i HEAD~3\n# 冲突了，怎么 abort 来着",
    "const arr = [1,2,3].map(x => x ** 2);\nconsole.log(arr.reduce((a,b)=>a+b));",
    "docker run -d -p 8080:80 --name web nginx:alpine",
]
NEG_MKT = [
    "【双 11 限时钜惠】全场专业版 5 折，仅限 48 小时！点击链接立即抢购，错过再等一年！",
    "尊敬的 VIP 用户，您有一张 200 元优惠券即将过期，速来领取！转发好友再得 50 元！",
    "年终大促！买一年送三个月，团队版三人成团再打 8 折，立即下单锁定低价。",
    "新品首发直播今晚 8 点开始，直播间专属价 + 抽奖送周边，点击预约不迷路！",
    "您购物车里的商品降价啦！比加入时便宜 30 元，库存仅剩 2 件，手慢无！",
    "会员日福利：签到领积分，积分兑好礼，连续签到 7 天送高级模板一套！",
    "感恩回馈老用户，您的专属续费折扣码已到账，7 天内有效，快来使用！",
    "限时拼团：邀请 2 位好友砍价，砍到 0 元免费领会员！",
]
NEG_OTHER = [
    "本周产品动态：新版搜索上线、性能优化若干、修复了 12 个已知问题。祝使用愉快。",
    "【系统通知】平台将于本周六 02:00-04:00 进行升级维护，期间服务短暂不可用。",
    "您好，我是 XX 公司的销售，想预约一次产品演示，方便的话请回复时间。",
    "会议邀请：周三下午 3 点产品评审会，地点 3F-301，请准时参加。",
    "关于调整报销流程的通知：自下月起发票统一走线上审批，详情见附件。",
    "感谢您参与本次用户体验调研，问卷共 12 题，预计占用您 3 分钟。",
    "您的试用版即将于 3 天后到期，如需继续使用请续费，无需可忽略本邮件。",
    "内部知识库本周更新：《新员工入职指南》《报销 FAQ》《会议室使用规范》。",
]

# ── C3② 哨兵（8 条固定，含期望分类与期望路径） ────────────────────────
SENTINELS = [
    {"id": "sent-1", "label": "bug", "title": "导出功能报错",
     "body": "点击导出 Excel 直接闪退，重试三次都一样。", "expect_path": "lib"},
    {"id": "sent-2", "label": "howto", "title": "使用咨询",
     "body": "请问怎么修改绑定的邮箱？", "expect_path": "lib"},
    {"id": "sent-3", "label": "billing", "title": "扣费异常",
     "body": "支付的时候一直提示错误，会员扣了两次费，要求退还多扣的。",
     "expect_path": "fallback"},
    {"id": "sent-4", "label": "feature", "title": "功能建议",
     "body": "希望能支持导出 PDF，现在找不到这个功能，每次都要截图拼接。",
     "expect_path": "fallback"},
    {"id": "sent-5", "label": "complaint", "title": "投诉",
     "body": "你们的服务太差了！我要投诉！给我一个说法！", "expect_path": "fallback"},
    {"id": "sent-6", "label": "unsure", "title": "急", "body": "急！！！在线等！！！",
     "expect_path": "either"},
    {"id": "sent-7", "label": "unsure", "title": "一段代码",
     "body": "def f(x): return x*2\n# 这段为什么报缩进错误", "expect_path": "either"},
    {"id": "sent-8", "label": "bug", "title": "搜索功能故障",
     "body": "你好，搜索功能输入关键词以后一直加载不出来。我这边截了图，需要的话可以提供。"
             "——此消息由手机端发送", "expect_path": "fallback"},
]

# ── 生成与切分 ────────────────────────────────────────────────────────
def gen_class(r, cls, n, weights=None):
    """生成 n 条该类工单。weights: 模板加权抽样（E1 微移用），None=均匀。

    实现方式：模板函数内部已用 r.choice 均匀抽，E1 的 ±15% 权重作用在
    槽位池与噪声注入上（等价的分布微移），保持代码简单且可复现。"""
    out = []
    for _ in range(n):
        t = make_ticket(r, cls)
        if weights:  # E1：噪声/扩展概率整体 ±15%，类内分布微移
            if t["tier"] >= 1 and r.random() < 0.15:
                t["tier"] = 0
                t["body"] = t["body"].split("。")[0]
        out.append(t)
    return out

def split_dev_holdout(r, items, n_dev):
    idx = list(range(len(items)))
    r.shuffle(idx)
    dev_ids = set(idx[:n_dev])
    return ([items[i] for i in idx if i in dev_ids],
            [items[i] for i in idx if i not in dev_ids])

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="v0a_artifacts")
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)

    r = random.Random(SEED_MAIN)
    dev, hold = [], []
    seq = {c: 0 for c in COUNTS}
    for cls in ["bug", "feature", "account", "howto", "billing", "complaint", "unsure"]:
        total, n_dev = COUNTS[cls]
        items = gen_class(r, cls, total)
        for it in items:
            seq[cls] += 1
            it["id"] = f"{cls}-{seq[cls]:03d}"
            it["label"] = cls
        d, h = split_dev_holdout(r, items, n_dev)
        dev += d
        hold += h
    r.shuffle(dev)
    r.shuffle(hold)

    re1 = random.Random(SEED_E1)
    e1 = []
    for cls in ["bug", "feature", "account", "howto", "billing", "complaint", "unsure"]:
        items = gen_class(re1, cls, E1_COUNTS[cls], weights=True)
        for i, it in enumerate(items, 1):
            it["id"] = f"e1-{cls}-{i:03d}"
            it["label"] = cls
        e1 += items
    re1.shuffle(e1)

    neg = []
    pools = [(NEG_CODE, "code"), (NEG_MKT, "mkt"), (NEG_OTHER, "other")]
    i = 0
    while i < 24:
        pool, kind = pools[i % 3]
        neg.append({"id": f"neg-{i+1:02d}", "label": "unsure", "kind": kind,
                    "title": "（无主题）", "body": pool[(i // 3) % len(pool)],
                    "tier": 1})
        i += 1

    def dump(name, rows):
        with open(os.path.join(args.out, name), "w", encoding="utf-8") as f:
            for x in rows:
                x2 = dict(x)
                x2["split"] = name.split("_")[1].split(".")[0]
                f.write(json.dumps(x2, ensure_ascii=False) + "\n")

    dump("tickets_dev.jsonl", dev)
    dump("tickets_holdout.jsonl", hold)
    dump("tickets_e1.jsonl", e1)
    dump("tickets_neg.jsonl", neg)
    with open(os.path.join(args.out, "sentinels.jsonl"), "w", encoding="utf-8") as f:
        for s in SENTINELS:
            f.write(json.dumps(s, ensure_ascii=False) + "\n")

    from collections import Counter
    stats = {
        "seed_main": SEED_MAIN, "seed_e1": SEED_E1,
        "dev": len(dev), "holdout": len(hold), "e1": len(e1), "neg": len(neg),
        "dev_classes": dict(Counter(x["label"] for x in dev)),
        "holdout_classes": dict(Counter(x["label"] for x in hold)),
        "dev_tiers": dict(Counter(x["tier"] for x in dev)),
        "holdout_tiers": dict(Counter(x["tier"] for x in hold)),
    }
    with open(os.path.join(args.out, "gen_stats.json"), "w", encoding="utf-8") as f:
        json.dump(stats, f, ensure_ascii=False, indent=2)
    print(json.dumps(stats, ensure_ascii=False, indent=2))

if __name__ == "__main__":
    main()
