#!/usr/bin/env python3
"""run_code 价值验证 v0.3 —— 三题确定性小任务 fixture（方案 §2 / 工单 01 第 3 条）。

生成不可变快照（宿主侧，模型工作目录外）：
  T0  12 条平面记录（≤2 KB）——明确谓词筛选，输出目标 id 清单
  T1  128 条订阅 + 256 条事件（≤60 KB）——关联后输出 user_id + 事件数
  T2  80 条订阅 + 160 条事件——双源分页（每页 20，4+8 页，响应正文 ≤40 KB），
      数据只经 stdio MCP mock（scripts/code_value_mcp_server.py）暴露

共同业务规则：plan=="paid" 且 status=="active" 的订阅用户中，至少有一条
confirmed==true 且 severity>=2 的事件；输出 user_id 与满足条件的事件数。
T0 把同一规则压平到单条记录内（四个谓词同时成立）。

纪律（方案 §2.1）：
  - gold 由独立参考程序从快照解出，不进模型工作目录；
  - T2 的参考解必须走 MCP 许可入口（记录正常页数与可达性），
    不从服务器原始数据直读来冒充可解证明；
  - 确定性：固定 seed + 强制边界样例，任何机器上重生成逐字节一致；
  - 快照不可变：已存在且哈希吻合则复用，--force 才允许重生成。

用法：
  python3 scripts/code_value_fixture.py --root artifacts/code_value/fixture
  python3 scripts/code_value_fixture.py --root ... --force
  python3 scripts/code_value_fixture.py --root ... --selfcheck   # 只验不改
"""
import argparse
import hashlib
import json
import random
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURE_VERSION = "cv-fixture-v1"
SEED = 20260924
PAGE_SIZE = 20

# 规模与上限（方案 §2 表格；数据量是开发上限，不是收益阈值）
T0_MAX_BYTES = 2048
T1_MAX_BYTES = 60 * 1024
T2_MAX_BODY_BYTES = 40 * 1024

PLANS = ["paid", "free"]
STATUSES = ["active", "cancelled", "expired"]


def sha256_file(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def dump_jsonl(path: Path, rows) -> None:
    with open(path, "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def load_jsonl(path: Path):
    return [json.loads(l) for l in open(path, encoding="utf-8") if l.strip()]


# ── T0：12 条平面记录 ──────────────────────────────────────────────────────
# 强制覆盖每个谓词边界：sev==2 入 / sev==1 出 / confirmed=false 出 /
# free 出 / cancelled、expired 出。目标 3/12：非空且非全集。

def t0_records():
    return [
        {"id": "r-01", "plan": "paid", "status": "active", "severity": 3, "confirmed": True},
        {"id": "r-02", "plan": "paid", "status": "active", "severity": 2, "confirmed": True},
        {"id": "r-03", "plan": "paid", "status": "active", "severity": 1, "confirmed": True},
        {"id": "r-04", "plan": "paid", "status": "active", "severity": 3, "confirmed": False},
        {"id": "r-05", "plan": "free", "status": "active", "severity": 3, "confirmed": True},
        {"id": "r-06", "plan": "paid", "status": "cancelled", "severity": 3, "confirmed": True},
        {"id": "r-07", "plan": "paid", "status": "expired", "severity": 2, "confirmed": True},
        {"id": "r-08", "plan": "free", "status": "active", "severity": 1, "confirmed": False},
        {"id": "r-09", "plan": "paid", "status": "active", "severity": 4, "confirmed": True},
        {"id": "r-10", "plan": "free", "status": "cancelled", "severity": 3, "confirmed": True},
        {"id": "r-11", "plan": "paid", "status": "active", "severity": 2, "confirmed": False},
        {"id": "r-12", "plan": "free", "status": "active", "severity": 2, "confirmed": True},
    ]


def solve_t0(records):
    """独立参考解 T0：四个谓词同时成立。"""
    return sorted(r["id"] for r in records
                  if r["plan"] == "paid" and r["status"] == "active"
                  and r["severity"] >= 2 and r["confirmed"] is True)


T1_FORCED_SUBS = [
    # (idx, plan, status, 说明)
    (1, "paid", "active", "两条合格事件 count=2"),
    (2, "paid", "active", "sev==2 边界入"),
    (3, "paid", "active", "sev==1 出"),
    (4, "paid", "active", "confirmed=false 出"),
    (5, "free", "active", "有合格事件但 free 出"),
    (6, "paid", "cancelled", "有合格事件但 cancelled 出"),
    (7, "paid", "active", "零事件出"),
    (8, "paid", "active", "一条合格加一条 sev==1 count=1"),
]


def t1_data():
    rng = random.Random(SEED)
    subs = []
    for i in range(1, 129):
        subs.append({"user_id": f"u-{i:04d}", "plan": rng.choice(PLANS),
                     "status": rng.choice(STATUSES)})
    for idx, plan, status, _ in T1_FORCED_SUBS:
        subs[idx - 1] = {"user_id": f"u-{idx:04d}", "plan": plan, "status": status}

    events = []

    def ev(n, uid, sev, conf):
        events.append({"event_id": f"e-{n:05d}", "user_id": uid,
                       "severity": sev, "confirmed": conf})

    # 强制边界事件（u-0001..u-0008；u-0007 刻意零事件）
    ev(1, "u-0001", 2, True)
    ev(2, "u-0001", 4, True)
    ev(3, "u-0002", 2, True)
    ev(4, "u-0003", 1, True)
    ev(5, "u-0004", 3, False)
    ev(6, "u-0005", 3, True)
    ev(7, "u-0006", 2, True)
    ev(8, "u-0008", 2, True)
    ev(9, "u-0008", 1, True)
    # 孤儿事件：user_id 不在任何订阅里，即便形状合格也必须忽略
    ev(10, "u-9901", 3, True)
    ev(11, "u-9902", 2, True)
    # 其余事件随机填充（固定 seed，确定性；只落在非保留用户 u-0009..u-0128，
    # 让 u-0001..u-0008 的边界语义不被随机事件污染）
    for n in range(12, 257):
        ev(n, f"u-{rng.randint(9, 128):04d}", rng.randint(1, 4), rng.random() < 0.6)
    return subs, events


def solve_join(subs, events, sub_key="user_id", ev_key="user_id"):
    """独立参考解（T1/T2 共用业务规则）：paid+active 订阅用户中统计
    confirmed==true 且 severity>=2 的事件数，只保留 count>=1 的用户。
    事件经 ev_key 关联订阅的 sub_key。"""
    eligible = {s[sub_key] for s in subs
                if s["plan"] == "paid" and s["status"] == "active"}
    counts = {}
    for e in events:
        uid = e[ev_key]
        if uid in eligible and e["confirmed"] is True and e["severity"] >= 2:
            counts[uid] = counts.get(uid, 0) + 1
    return {uid: counts[uid] for uid in sorted(counts)}


# ── T2：80 条订阅 + 160 条事件，双源分页（每页 20 → 4 + 8 页）──────────────
# 事件源关联字段刻意用 uid（方案 §2 允许不同源不同 join 字段名；不藏 schema：
# 任务文本与工具描述都写明）。服务器原始数据在模型 workdir 之外。

T2_FORCED_SUBS = [
    (1, "paid", "active", "两条合格 count=2"),
    (2, "paid", "active", "sev==2 边界入"),
    (3, "paid", "active", "sev==1 出"),
    (4, "paid", "active", "confirmed=false 出"),
    (5, "free", "active", "有合格事件但 free 出"),
    (6, "paid", "cancelled", "有合格事件但 cancelled 出"),
    (7, "paid", "active", "零事件出"),
    (8, "paid", "active", "一条合格加一条 sev==1 count=1"),
]


def t2_data():
    rng = random.Random(SEED + 2)
    subs = []
    for i in range(1, 81):
        subs.append({"user_id": f"u-{i:04d}", "plan": rng.choice(PLANS),
                     "status": rng.choice(STATUSES)})
    for idx, plan, status, _ in T2_FORCED_SUBS:
        subs[idx - 1] = {"user_id": f"u-{idx:04d}", "plan": plan, "status": status}

    events = []

    def ev(n, uid, sev, conf):
        # 事件源关联字段是 uid（与订阅源的 user_id 不同名，任务文本写明）
        events.append({"event_id": f"e-{n:05d}", "uid": uid,
                       "severity": sev, "confirmed": conf})

    ev(1, "u-0001", 2, True)
    ev(2, "u-0001", 4, True)
    ev(3, "u-0002", 2, True)
    ev(4, "u-0003", 1, True)
    ev(5, "u-0004", 3, False)
    ev(6, "u-0005", 3, True)
    ev(7, "u-0006", 2, True)
    ev(8, "u-0008", 2, True)
    ev(9, "u-0008", 1, True)
    ev(10, "u-9901", 3, True)
    ev(11, "u-9902", 2, True)
    # 随机填充只落在非保留用户 u-0009..u-0080（同 T1 的边界保护）
    for n in range(12, 161):
        ev(n, f"u-{rng.randint(9, 80):04d}", rng.randint(1, 4), rng.random() < 0.6)
    return subs, events


def mcp_server_command(root: Path, log_path: Path = None):
    """T2 的 stdio MCP mock 启动参数（config 的 mcp_servers 与参考解共用）。"""
    args = ["python3", str(ROOT / "scripts" / "code_value_mcp_server.py"),
            "--subscriptions", str(root / "t2" / "server" / "subscriptions.json"),
            "--events", str(root / "t2" / "server" / "events.json")]
    if log_path is not None:
        args += ["--log", str(log_path)]
    return args


def solve_t2_via_mcp(root: Path):
    """独立参考解 T2：必须走 MCP 许可入口（方案 §4.2 放行表第 1 条）。

    起一个真实的 stdio MCP mock 子进程，按协议 initialize → tools/list →
    逐页 tools/call 读完全部数据，再在客户端算 gold。记录正常页数与
    可达性，不从服务器原始数据直读。返回 (gold, reach)。
    """
    # 工单 01R：参考解顺带开启服务端日志——与客户端 reach 记录对账
    log_path = root / "t2" / "server" / "reference_solve_events.ndjson"
    cmd = mcp_server_command(root, log_path)
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                            text=True)
    next_id = [1]

    def call(method, params=None):
        msg = {"jsonrpc": "2.0", "id": next_id[0], "method": method}
        next_id[0] += 1
        if params is not None:
            msg["params"] = params
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("MCP mock closed stdout unexpectedly")
        resp = json.loads(line)
        if "error" in resp:
            raise RuntimeError(f"MCP mock error: {resp['error']}")
        return resp["result"]

    try:
        info = call("initialize", {"protocolVersion": "2025-03-26",
                                   "capabilities": {}, "clientInfo": {"name": "ref-solver"}})
        tools = call("tools/list")
        names = sorted(t["name"] for t in tools["tools"])
        subs, events = [], []
        pages = {}
        for name, sink in (("subscriptions_page", subs), ("events_page", events)):
            n_pages = 0
            page = 1
            body_bytes = 0
            while True:
                out = call("tools/call", {"name": name,
                                          "arguments": {"page": page}})
                if out.get("isError"):
                    raise RuntimeError(f"{name} page {page}: {out['content']}")
                payload = json.loads(out["content"][0]["text"])
                sink.extend(payload["records"])
                body_bytes += len(out["content"][0]["text"])
                n_pages += 1
                if not payload["has_more"]:
                    break
                page += 1
            pages[name] = {"pages": n_pages, "body_bytes": body_bytes}
        gold = solve_join(subs, events, sub_key="user_id", ev_key="uid")
        reach = {"server_info": info.get("serverInfo", {}),
                 "tool_names": names, "paging": pages,
                 "total_body_bytes": sum(p["body_bytes"] for p in pages.values())}
    finally:
        proc.stdin.close()
        proc.wait(timeout=10)
    # 服务端侧证（try 外读日志——进程已收尾，日志落定）：tools/call 页
    # 计数与客户端 reach["paging"] 对账（工单 01R）
    if log_path.exists():
        import collections
        server_pages = collections.Counter()
        for line in log_path.read_text(encoding="utf-8").splitlines():
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            server_pages[rec.get("tool")] += 1
        reach["server_log_pages"] = dict(server_pages)
        reach["server_log"] = str(log_path.relative_to(root))
    return gold, reach


# ── 任务文本（两臂逐字相同；冻结进 task_hash）──────────────────────────────
# 交付纪律句（写出完整即完成、不追加复核）与输出契约写在公共任务文本里，
# 不藏在臂差分里；A/B 收到的字节完全一致。

TASK_TEXTS = {
    "t0": (
        "workdir 里有 records.jsonl，共 12 条记录，每条形如 "
        '{"id": "...", "plan": "...", "status": "...", "severity": <整数>, '
        '"confirmed": <布尔>}。\n'
        "请找出同时满足以下全部条件的记录 id：\n"
        '- plan 为 "paid"\n'
        '- status 为 "active"\n'
        "- severity >= 2\n"
        "- confirmed 为 true\n"
        '把结果写到 answer.jsonl：每行一个 JSON 对象 {"id": "..."}，'
        "与目标记录一一对应，不重不漏，不要输出无关字段。\n"
        "写出完整 answer.jsonl 并自检行数与条件后即视为完成，"
        "直接给紧凑摘要结束（目标条数即可），不要追加逐条复核。"
    ),
    "t1": (
        "workdir 里有 subscriptions.jsonl（128 条订阅）与 events.jsonl（256 条事件）。\n"
        "订阅每行："
        '{"user_id": "...", "plan": "paid" 或 "free", '
        '"status": "active"、"cancelled" 或 "expired"}\n'
        "事件每行："
        '{"event_id": "...", "user_id": "...", '
        '"severity": <整数>, "confirmed": <布尔>}\n'
        '请找出：plan 为 "paid" 且 status 为 "active" 的订阅用户中，'
        "至少有一条 confirmed 为 true 且 severity >= 2 事件的用户。\n"
        "事件里出现的 user_id 若没有对应订阅记录则忽略。\n"
        "把结果写到 answer.jsonl：每行一个 JSON 对象 "
        '{"user_id": "...", "event_count": <该用户满足条件的事件条数>}，'
        "按 user_id 不重不漏，不要输出无关字段。\n"
        "写出完整 answer.jsonl 并自检后即视为完成，"
        "直接给紧凑摘要结束（目标用户数即可），不要追加逐条复核。"
    ),
    "t2": (
        "本任务没有 shell，文件工具限制在 workdir 内。"
        "数据只能通过两个 MCP 分页工具获取：\n"
        '- mcp_t2data_subscriptions_page，入参 {"page": <从 1 起的页号>}：'
        "订阅源，每页 20 条，记录字段 user_id / plan / status\n"
        '- mcp_t2data_events_page，入参 {"page": <从 1 起的页号>}：'
        "事件源，每页 20 条，记录字段 event_id / uid / severity / confirmed"
        "（注意：事件源用 uid 关联用户，与订阅源的 user_id 对应）\n"
        "两个工具都返回 "
        '{"page": ..., "total_pages": ..., "total_records": ..., '
        '"has_more": ..., "records": [...]}；页号超出范围会返回错误。'
        "订阅源共 80 条，事件源共 160 条。\n"
        '请找出：plan 为 "paid" 且 status 为 "active" 的订阅用户中，'
        "至少有一条 confirmed 为 true 且 severity >= 2 事件的用户"
        "（事件经 uid 关联）。\n"
        "把结果写到 answer.jsonl：每行一个 JSON 对象 "
        '{"user_id": "...", "event_count": <该用户满足条件的事件条数>}，'
        "按 user_id 不重不漏，不要输出无关字段。\n"
        "写出完整 answer.jsonl 并自检后即视为完成，"
        "直接给紧凑摘要结束（目标用户数即可），不要追加逐条复核。"
    ),
}


# ── G2 新实例（方案 §4.4：每类 2 新实例；同规模同部署条件）────────────────
# 边界语义与 G1 完全一致：强制边界记录逐谓词覆盖、孤儿事件、随机填充只落
# 非保留用户。随机体 seed 派生自 G1（SEED + 100 + 实例号）——数据是新的，
# 刻意不与 G1 任何实例重合；规模不变（可比性）。T0 无随机体，两实例各自
# 手写变体（同谓词覆盖、不同取值/位置/目标数）。

G2_INSTANCE_IDS = ("g2i1", "g2i2")


def _g2_seed(instance: str) -> int:
    return SEED + 100 + G2_INSTANCE_IDS.index(instance)


# T0 手写变体：每实例 12 条，谓词边界全覆盖（sev==2 入 / sev==1 出 /
# confirmed=false 出 / free 出 / cancelled 出 / expired 出），目标非空非全集。
_G2_T0_VARIANTS = {
    "g2i1": [
        ("r-01", "paid", "active", 5, True),    # 入
        ("r-02", "paid", "active", 2, True),    # sev==2 边界入
        ("r-03", "paid", "active", 1, True),    # sev==1 出
        ("r-04", "paid", "active", 6, False),   # confirmed=false 出
        ("r-05", "free", "active", 7, True),    # free 出
        ("r-06", "paid", "cancelled", 2, True),  # cancelled 出
        ("r-07", "paid", "expired", 3, True),   # expired 出
        ("r-08", "free", "expired", 1, False),
        ("r-09", "paid", "active", 4, True),    # 入
        ("r-10", "free", "active", 9, True),
        ("r-11", "paid", "active", 2, False),   # confirmed=false 出
        ("r-12", "free", "cancelled", 2, True),
    ],
    "g2i2": [
        ("r-01", "free", "active", 3, True),
        ("r-02", "paid", "active", 2, True),    # sev==2 边界入
        ("r-03", "paid", "active", 1, True),    # sev==1 出
        ("r-04", "paid", "active", 4, False),   # confirmed=false 出
        ("r-05", "paid", "active", 3, True),    # 入
        ("r-06", "free", "active", 2, True),
        ("r-07", "paid", "cancelled", 4, True),  # cancelled 出
        ("r-08", "paid", "expired", 2, True),   # expired 出
        ("r-09", "paid", "active", 2, True),    # 入
        ("r-10", "paid", "active", 1, False),   # sev==1 且 confirmed=false 出
        ("r-11", "free", "expired", 3, True),
        ("r-12", "paid", "active", 5, True),    # 入
    ],
}


def g2_t0_records(instance: str):
    """G2 的 T0 实例：12 条手写变体（同谓词覆盖，不同取值/位置）。"""
    return [{"id": i, "plan": p, "status": s, "severity": sev,
             "confirmed": c}
            for i, p, s, sev, c in _G2_T0_VARIANTS[instance]]


def g2_t1_data(instance: str):
    """G2 的 T1 实例：128 订阅 + 256 事件，边界语义同 G1（新随机体）。"""
    rng = random.Random(_g2_seed(instance))
    subs = []
    for i in range(1, 129):
        subs.append({"user_id": f"u-{i:04d}", "plan": rng.choice(PLANS),
                     "status": rng.choice(STATUSES)})
    for idx, plan, status, _ in T1_FORCED_SUBS:
        subs[idx - 1] = {"user_id": f"u-{idx:04d}", "plan": plan,
                         "status": status}
    events = []

    def ev(n, uid, sev, conf):
        events.append({"event_id": f"e-{n:05d}", "user_id": uid,
                       "severity": sev, "confirmed": conf})

    ev(1, "u-0001", 2, True)
    ev(2, "u-0001", 4, True)
    ev(3, "u-0002", 2, True)
    ev(4, "u-0003", 1, True)
    ev(5, "u-0004", 3, False)
    ev(6, "u-0005", 3, True)
    ev(7, "u-0006", 2, True)
    ev(8, "u-0008", 2, True)
    ev(9, "u-0008", 1, True)
    ev(10, "u-9901", 3, True)   # 孤儿事件
    ev(11, "u-9902", 2, True)
    for n in range(12, 257):
        ev(n, f"u-{rng.randint(9, 128):04d}", rng.randint(1, 4),
           rng.random() < 0.6)
    return subs, events


def g2_t2_data(instance: str):
    """G2 的 T2 实例：80 订阅 + 160 事件（4+8 页），边界语义同 G1。"""
    rng = random.Random(_g2_seed(instance))
    subs = []
    for i in range(1, 81):
        subs.append({"user_id": f"u-{i:04d}", "plan": rng.choice(PLANS),
                     "status": rng.choice(STATUSES)})
    for idx, plan, status, _ in T2_FORCED_SUBS:
        subs[idx - 1] = {"user_id": f"u-{idx:04d}", "plan": plan,
                         "status": status}
    events = []

    def ev(n, uid, sev, conf):
        events.append({"event_id": f"e-{n:05d}", "uid": uid,
                       "severity": sev, "confirmed": conf})

    ev(1, "u-0001", 2, True)
    ev(2, "u-0001", 4, True)
    ev(3, "u-0002", 2, True)
    ev(4, "u-0003", 1, True)
    ev(5, "u-0004", 3, False)
    ev(6, "u-0005", 3, True)
    ev(7, "u-0006", 2, True)
    ev(8, "u-0008", 2, True)
    ev(9, "u-0008", 1, True)
    ev(10, "u-9901", 3, True)   # 孤儿 uid
    ev(11, "u-9902", 2, True)
    for n in range(12, 161):
        ev(n, f"u-{rng.randint(9, 80):04d}", rng.randint(1, 4),
           rng.random() < 0.6)
    return subs, events


def g2_data(instance: str, task: str):
    """G2 实例数据入口：t0 返回 records；t1/t2 返回 (subs, events)。"""
    if task == "t0":
        return g2_t0_records(instance)
    if task == "t1":
        return g2_t1_data(instance)
    if task == "t2":
        return g2_t2_data(instance)
    raise ValueError(f"unknown task {task}")


def g2_gold(instance: str, task: str):
    """G2 实例的 gold（判卷进程现算；不落盘）。形状与 scorer.load_gold 一致。"""
    data = g2_data(instance, task)
    if task == "t0":
        return {"id": {t: None for t in solve_t0(data)}}
    subs, events = data
    if task == "t1":
        return {"user_id": solve_join(subs, events)}
    # t2：事件源关联字段是 uid（与 G1 同款跨源字段名差异）
    return {"user_id": solve_join(subs, events, sub_key="user_id",
                                  ev_key="uid")}


def g2_serialize(instance: str, task: str) -> dict:
    """G2 实例数据的落盘字节（stage_workdir_g2 / g2-sealed 写出的同名文件）。

    哈希封存与铺设共用这一序列化——保证「prepare 封存的哈希」与「run 时
    铺进 workdir 的字节」逐字节一致（数据漂移即校验失败）。
    """
    data = g2_data(instance, task)
    out = {}
    if task == "t0":
        out["records.jsonl"] = "".join(
            json.dumps(r, ensure_ascii=False) + "\n" for r in data).encode()
    elif task == "t1":
        subs, events = data
        out["subscriptions.jsonl"] = "".join(
            json.dumps(r, ensure_ascii=False) + "\n"
            for r in subs).encode()
        out["events.jsonl"] = "".join(
            json.dumps(r, ensure_ascii=False) + "\n"
            for r in events).encode()
    elif task == "t2":
        subs, events = data
        out["subscriptions.json"] = json.dumps(
            subs, ensure_ascii=False).encode()
        out["events.json"] = json.dumps(events, ensure_ascii=False).encode()
    else:
        raise ValueError(f"unknown task {task}")
    return out


def g2_gold_digest() -> dict:
    """全部 G2 实例的 gold 摘要（hash-only，不可逆；同 gen_stats.gold_digest
    纪律）。判卷进程重算 gold 后对此对账，防生成模块被篡改后静默判错。"""
    digest = {}
    for instance in G2_INSTANCE_IDS:
        for task in ("t0", "t1", "t2"):
            gold = g2_gold(instance, task)
            canon = json.dumps(gold, sort_keys=True, ensure_ascii=False)
            digest[f"{instance}-{task}"] = {
                "count": len(next(iter(gold.values()))),
                "gold_sha256": hashlib.sha256(canon.encode()).hexdigest()}
    return digest


# ── 模型工作目录铺设（gold 永不进入 workdir）───────────────────────────────

def stage_workdir(task: str, wd: Path, root: Path):
    """把任务允许的数据铺进全新 workdir。每次 run 前调用（状态重置）。"""
    if wd.exists():
        shutil.rmtree(wd)
    wd.mkdir(parents=True)
    if task == "t0":
        shutil.copyfile(root / "t0" / "records.jsonl", wd / "records.jsonl")
    elif task == "t1":
        shutil.copyfile(root / "t1" / "subscriptions.jsonl", wd / "subscriptions.jsonl")
        shutil.copyfile(root / "t1" / "events.jsonl", wd / "events.jsonl")
    elif task == "t2":
        pass  # 数据只经 MCP；workdir 留空（answer.jsonl 由模型写出）
    else:
        raise ValueError(f"unknown task {task}")


def stage_workdir_g2(instance: str, task: str, wd: Path):
    """G2 实例的工作目录铺设：数据现生成（g2_serialize 同款字节）。

    t2 的数据不进 workdir——只经实例专属 MCP 服务器（数据文件在 run_dir
    内、Seatbelt literal allow 放行），与 G1 的部署条件一致。
    """
    if wd.exists():
        shutil.rmtree(wd)
    wd.mkdir(parents=True)
    if task == "t2":
        return
    for name, blob in g2_serialize(instance, task).items():
        (wd / name).write_bytes(blob)


# ── 生成与自检 ──────────────────────────────────────────────────────────────

def generate(root: Path) -> dict:
    root.mkdir(parents=True, exist_ok=True)
    (root / "t0").mkdir(parents=True, exist_ok=True)
    (root / "t1").mkdir(parents=True, exist_ok=True)
    (root / "t2" / "server").mkdir(parents=True, exist_ok=True)

    t0 = t0_records()
    dump_jsonl(root / "t0" / "records.jsonl", t0)
    t0_gold = solve_t0(t0)

    t1_subs, t1_events = t1_data()
    dump_jsonl(root / "t1" / "subscriptions.jsonl", t1_subs)
    dump_jsonl(root / "t1" / "events.jsonl", t1_events)
    t1_gold = solve_join(t1_subs, t1_events)

    t2_subs, t2_events = t2_data()
    (root / "t2" / "server" / "subscriptions.json").write_text(
        json.dumps(t2_subs, ensure_ascii=False), encoding="utf-8")
    (root / "t2" / "server" / "events.json").write_text(
        json.dumps(t2_events, ensure_ascii=False), encoding="utf-8")
    # T2 的 gold 必须来自 MCP 许可入口上的参考解（方案 §4.2 第 1 条）
    t2_gold, reach = solve_t2_via_mcp(root)

    # 方案 §2.1 + 工单 01R P1-04：gold 不落盘，摘要也不得含答案明文——
    # gen_stats 位于评测 shell 可读面（restrict_to_workdir 不约束 shell 的
    # OS 读权限），所以这里只记计数与规范化答案的不可逆哈希。判卷进程
    # 在产物封存后从快照重算 gold，对哈希对账。
    def _digest(gold_dict):
        canon = json.dumps(gold_dict, sort_keys=True, ensure_ascii=False)
        return hashlib.sha256(canon.encode()).hexdigest()

    gold_digest = {
        "t0": {"count": len(t0_gold), "gold_sha256": _digest(
            {t: None for t in t0_gold})},
        "t1": {"count": len(t1_gold), "gold_sha256": _digest(t1_gold)},
        "t2": {"count": len(t2_gold), "gold_sha256": _digest(t2_gold)},
    }

    stats = {
        "version": FIXTURE_VERSION,
        "seed": SEED,
        "page_size": PAGE_SIZE,
        "t0": {"records": len(t0), "bytes": (root / "t0" / "records.jsonl").stat().st_size,
               "gold_count": len(solve_t0(t0))},
        "t1": {"subs": len(t1_subs), "events": len(t1_events),
               "bytes": (root / "t1" / "subscriptions.jsonl").stat().st_size
                      + (root / "t1" / "events.jsonl").stat().st_size,
               "gold_count": len(solve_join(t1_subs, t1_events))},
        "t2": {"subs": len(t2_subs), "events": len(t2_events),
               "gold_count": len(t2_gold),
               "reach": reach},
        "gold_digest": gold_digest,
        "task_text_sha256": {k: hashlib.sha256(v.encode()).hexdigest()
                             for k, v in TASK_TEXTS.items()},
        "file_sha256": {str(p.relative_to(root)): sha256_file(p)
                        for p in sorted(root.rglob("*")) if p.is_file()
                        and p.name not in ("gen_stats.json",
                                           "reference_solve_events.ndjson")},
    }
    stats["selfcheck_problems"] = selfcheck(root, gold_digest)
    (root / "gen_stats.json").write_text(
        json.dumps(stats, ensure_ascii=False, indent=1), encoding="utf-8")
    return stats

def selfcheck(root: Path, gold_digest=None):
    """离线自检（方案 §2 实施约束 + §4.2 放行表第 1 条）。返回问题列表。

    gold_digest：generate() 传入的内存摘要（此时 gen_stats 尚未落盘）；
    --selfcheck 独立跑时为 None——从盘上 gen_stats 读。
    """
    problems = []
    t0 = load_jsonl(root / "t0" / "records.jsonl")
    if len(t0) != 12:
        problems.append(f"t0 records {len(t0)} != 12")
    if (root / "t0" / "records.jsonl").stat().st_size > T0_MAX_BYTES:
        problems.append("t0 exceeds 2KB cap")
    g0 = solve_t0(t0)
    if not g0:
        problems.append("t0 gold empty")
    if len(g0) == len(t0):
        problems.append("t0 gold is full set")
    if g0 != ["r-01", "r-02", "r-09"]:
        problems.append(f"t0 boundary samples wrong: {g0}")
    if (root / "t0" / "gold.json").exists():
        problems.append("t0 gold.json exists on disk (must not — §2.1)")
    digest0 = gold_digest if gold_digest is not None else \
        json.load(open(root / "gen_stats.json",
                       encoding="utf-8")).get("gold_digest", {})
    canon0 = json.dumps({t: None for t in g0}, sort_keys=True,
                        ensure_ascii=False)
    if hashlib.sha256(canon0.encode()).hexdigest() \
            != digest0.get("t0", {}).get("gold_sha256"):
        problems.append("t0 recomputed gold disagrees with gen_stats digest")

    t1_subs = load_jsonl(root / "t1" / "subscriptions.jsonl")
    t1_events = load_jsonl(root / "t1" / "events.jsonl")
    if (len(t1_subs), len(t1_events)) != (128, 256):
        problems.append(f"t1 sizes {(len(t1_subs), len(t1_events))} != (128, 256)")
    t1_bytes = ((root / "t1" / "subscriptions.jsonl").stat().st_size
                + (root / "t1" / "events.jsonl").stat().st_size)
    if t1_bytes > T1_MAX_BYTES:
        problems.append(f"t1 {t1_bytes}B exceeds 60KB cap")
    g1 = solve_join(t1_subs, t1_events)
    if not g1 or len(g1) >= 128:
        problems.append("t1 gold empty or full")
    for uid, want in (("u-0001", 2), ("u-0002", 1), ("u-0008", 1)):
        if g1.get(uid) != want:
            problems.append(f"t1 boundary {uid}: {g1.get(uid)} != {want}")
    for uid in ("u-0003", "u-0004", "u-0005", "u-0006", "u-0007",
                "u-9901", "u-9902"):
        if uid in g1:
            problems.append(f"t1 {uid} must not be in gold")
    # gold 不落盘（方案 §2.1）：对账 = 重算结果的不可逆哈希 vs gen_stats
    # （generate() 内存传入；独立 --selfcheck 时从盘上 gen_stats 读）
    digest = gold_digest if gold_digest is not None else \
        json.load(open(root / "gen_stats.json",
                       encoding="utf-8")).get("gold_digest", {})
    canon = json.dumps(g1, sort_keys=True, ensure_ascii=False)
    if hashlib.sha256(canon.encode()).hexdigest() \
            != digest.get("t1", {}).get("gold_sha256"):
        problems.append("t1 recomputed gold disagrees with gen_stats digest")
    if len(g1) != digest.get("t1", {}).get("count"):
        problems.append("t1 recomputed gold count disagrees with gen_stats")
    if (root / "t1" / "gold.json").exists():
        problems.append("t1 gold.json exists on disk (must not — §2.1)")
    return problems


def selfcheck_t2(root: Path, gold_digest=None):
    """T2 专项自检：MCP 许可入口可解性 + 分页契约 + 隔离。返回 (problems, reach)。"""
    problems = []
    t2_subs = json.load(open(root / "t2" / "server" / "subscriptions.json",
                             encoding="utf-8"))
    t2_events = json.load(open(root / "t2" / "server" / "events.json",
                               encoding="utf-8"))
    if (len(t2_subs), len(t2_events)) != (80, 160):
        problems.append(f"t2 sizes {(len(t2_subs), len(t2_events))} != (80, 160)")
    direct = solve_join(t2_subs, t2_events, sub_key="user_id", ev_key="uid")
    gold2, reach = solve_t2_via_mcp(root)
    if gold2 != direct:
        problems.append("t2 MCP reference solve disagrees with direct compute")
    if not gold2 or len(gold2) >= 80:
        problems.append("t2 gold empty or full")
    for uid, want in (("u-0001", 2), ("u-0002", 1), ("u-0008", 1)):
        if gold2.get(uid) != want:
            problems.append(f"t2 boundary {uid}: {gold2.get(uid)} != {want}")
    for uid in ("u-0003", "u-0004", "u-0005", "u-0006", "u-0007",
                "u-9901", "u-9902"):
        if uid in gold2:
            problems.append(f"t2 {uid} must not be in gold")
    pages = reach["paging"]
    if pages.get("subscriptions_page", {}).get("pages") != 4:
        problems.append(f"t2 subscriptions pages != 4: {pages}")
    if pages.get("events_page", {}).get("pages") != 8:
        problems.append(f"t2 events pages != 8: {pages}")
    if reach["total_body_bytes"] > T2_MAX_BODY_BYTES:
        problems.append(f"t2 body {reach['total_body_bytes']}B exceeds 40KB cap")
    digest = gold_digest if gold_digest is not None else \
        json.load(open(root / "gen_stats.json",
                       encoding="utf-8")).get("gold_digest", {})
    canon = json.dumps(gold2, sort_keys=True, ensure_ascii=False)
    if hashlib.sha256(canon.encode()).hexdigest() \
            != digest.get("t2", {}).get("gold_sha256"):
        problems.append("t2 recomputed gold disagrees with gen_stats digest")
    if len(gold2) != digest.get("t2", {}).get("count"):
        problems.append("t2 recomputed gold count disagrees with gen_stats")
    if (root / "t2" / "gold.json").exists():
        problems.append("t2 gold.json exists on disk (must not — §2.1)")
    return problems, reach


def verify_immutable(root: Path) -> bool:
    """已存在的快照是否与 gen_stats 记录的哈希一致（不可变复用门）。"""
    stats_path = root / "gen_stats.json"
    if not stats_path.exists():
        return False
    stats = json.load(open(stats_path, encoding="utf-8"))
    if stats.get("version") != FIXTURE_VERSION:
        return False
    for rel, want in stats.get("file_sha256", {}).items():
        p = root / rel
        if not p.exists() or sha256_file(p) != want:
            return False
    return True


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", default="artifacts/code_value/fixture")
    ap.add_argument("--force", action="store_true",
                    help="快照已存在也重生成（默认哈希吻合即复用）")
    ap.add_argument("--selfcheck", action="store_true",
                    help="只跑自检，不生成不修改")
    args = ap.parse_args()
    root = Path(args.root)
    if not root.is_absolute():
        root = ROOT / root
    if args.selfcheck:
        if not verify_immutable(root):
            sys.exit("fixture missing or hash mismatch — generate first")
        problems = selfcheck(root) + selfcheck_t2(root)[0]
        for p in problems:
            print("[fixture] PROBLEM: " + p)
        sys.exit(1 if problems else 0)
    if root.exists() and verify_immutable(root) and not args.force:
        print(f"[fixture] {root} unchanged (hashes match) — reuse")
        return
    if root.exists():
        shutil.rmtree(root)
    stats = generate(root)
    problems = stats["selfcheck_problems"] \
        + selfcheck_t2(root, stats.get("gold_digest"))[0]
    if problems:
        for p in problems:
            print("[fixture] PROBLEM: " + p, file=sys.stderr)
        sys.exit(1)
    print("[fixture] generated " + str(root) + " version=" + FIXTURE_VERSION
          + " gold: t0=" + str(stats["t0"]["gold_count"])
          + " t1=" + str(stats["t1"]["gold_count"])
          + " t2=" + str(stats["t2"]["gold_count"]))


if __name__ == "__main__":
    main()
