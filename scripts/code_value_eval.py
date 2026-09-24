#!/usr/bin/env python3
"""run_code 价值验证 v0.3 —— 最小 runner（方案 §4/§6，工单 01 第 4-8 条）。

职责：读 manifest、A/B 启动、状态隔离、预算检查、封存、汇总。
默认 dry-run；付费执行必须显式 --execute（工单 01 第 5 条）。
不重造评测平台：复用 tiny-agent CLI 的事件流（--json NDJSON）、result.json
的 usage 账与 stdio MCP 协议；评分/计费在 code_value_score.py，数据在
code_value_fixture.py，T2 数据源在 code_value_mcp_server.py。

子命令：
  g0          零付费装置检查：fixture、A/B 清单差分、mock 联通
              （正常/失败/缺 usage）、manifest、G0 放行表。不碰真实网关。
  prepare-g1  冻结 G1 六跑配方（T0-B,A、T1-A,B、T2-B,A）：manifest、
              真实网关身份（不含密钥）、逐 run 配置。价表/费用停止值缺
              时标记 blocked_paid，run-g1 拒绝放行。
  run-g1      执行冻结配方。必须 --execute 且价表、费用停止值齐备；
              预算闸不足一个 80K run 的预留时不启动；不自动重启失败 run，
              不自动解锁下一阶段。

工件：artifacts/code_value/批次目录（manifest.json、configs/、runs/、
ledger.json、g0_gate.json）+ docs/code_value_reports/批次.md。
run_id 唯一、不覆盖旧记录（目录已存在即拒绝）。
"""
import argparse
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import code_value_fixture as fixture  # noqa: E402
import code_value_score as scorer  # noqa: E402

BIN = ROOT / "target" / "release" / "tiny-agent"
ART = ROOT / "artifacts" / "code_value"
REPORTS = ROOT / "docs" / "code_value_reports"
FIXTURE_ROOT = ART / "fixture"
EXPERIMENT_VERSION = "cv-eval-v1"


# 每 run 资源边界（方案 §4.1：所有臂相同；原生 token 闸是轮边界软闸）
RUN_LIMITS = {"max_turns": 20, "timeout_secs": 600,
              "max_output_chars": 60000, "max_total_tokens": 80000}
MAX_TOKENS_PER_REQUEST = 32768
# G1 固定顺序与计划包络（方案 §4.3：T0-B,A、T1-A,B、T2-B,A；6×80K=0.48M）
G1_RUNS = [("t0", "B"), ("t0", "A"), ("t1", "A"), ("t1", "B"),
           ("t2", "B"), ("t2", "A")]
G1_TOKEN_ENVELOPE = 480_000
# 在途超额风险上界（工单 01R：须含输入与输出，不能只算输出上限）。
# 软闸是轮边界语义：最后一轮请求可再消耗 max_tokens 输出 + 该轮完整
# 输入（上界取包络内单 run 软闸值——一个 run 的全部已知输入预算）。
INFLIGHT_OVERRUN_TOKENS = MAX_TOKENS_PER_REQUEST + RUN_LIMITS["max_total_tokens"]

API_KEY_ENV = "CODE_VALUE_KEY"


def write_json(path: Path, obj) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(obj, ensure_ascii=False, indent=1),
                    encoding="utf-8")


def sha256_file(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def git_out(*args) -> str:
    r = subprocess.run(["git", "-c", "core.precomposeunicode=false",
                        *args], cwd=ROOT, capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else ""


def fingerprint() -> dict:
    """封存用环境指纹：HEAD、工作树差分哈希、二进制哈希与编译特性。"""
    head = git_out("rev-parse", "HEAD") or "unknown"
    diff = subprocess.run(
        ["git", "-c", "core.precomposeunicode=false", "diff", "HEAD"],
        cwd=ROOT, capture_output=True, text=True).stdout
    untracked = git_out("status", "--short")
    dirty_material = (diff + "\n" + untracked).encode()
    version = subprocess.run([str(BIN), "--version", "--json"],
                             capture_output=True, text=True)
    features = {}
    try:
        features = json.loads(version.stdout)
    except json.JSONDecodeError:
        pass
    return {
        "code_head": head,
        "dirty_diff_hash": hashlib.sha256(dirty_material).hexdigest(),
        "dirty": bool(diff or untracked),
        "binary_hash": sha256_file(BIN) if BIN.exists() else None,
        "binary_manifest": features,
    }


def gateway_identity():
    """从 测试模型.md 取真实网关身份（不含密钥）。G1 冻结用。

    密钥只在 run-g1 --execute 时按 api_key_env 注入进程环境，
    绝不写进任何工件。
    """
    kv = {}
    for line in (ROOT / "测试模型.md").read_text(encoding="utf-8").splitlines():
        m = re.match(r'\s*(?:#\s*)?([^=]+?)\s*=\s*"(.*)"\s*$', line)
        if m:
            kv[m.group(1).strip()] = m.group(2).strip()
    base = kv.get("base url", "")
    model = kv.get("模型名称", "")
    ident = {"base_url_host": base.split("//")[-1].split("/")[0] if base else None,
             "model": model or None,
             "api_key_env": API_KEY_ENV,
             "note": "identity only; the key never enters artifacts"}
    secret = {"base_url": base, "api_key": kv.get("api key", "")}
    return ident, secret


def prompt_body(name: str) -> str:
    """examples/prompts/<name>.md 的正文（--- 之后的 prompt body）。"""
    raw = (ROOT / "examples" / "prompts" / f"{name}.md").read_text(encoding="utf-8")
    return raw.split("\n---\n", 1)[1].strip()


PERSIST_ANCHOR = "  unchanged.\n"
VERIFY_ANCHOR = "## Verify before you declare done"


def prompt_parts():
    """两臂系统提示词的机械组装（方案 §3：公共段逐字相同，增量=现行
    code 能力说明；零自撰文本，全部切片自两份现行 prompt 文件）。

    common = agentic.md 正文 + 「增量落盘」bullet（逐字取自 agentic-code.md
    的 Persistence 段——通用交付纪律必须放公共部分，不能只给 A）。
    a_system = common + 「Choose how to execute the remaining work」整段
    （现行 code 能力说明，含 read_skill(run-code) 指引；A 独有）。
    agentic-code.md 里 Think 段的少量措辞改写刻意不采用——两臂公共段
    以 agentic.md 的现行基线措辞为准，差分只留能力披露。
    """
    base = prompt_body("agentic")
    code_full = prompt_body("agentic-code")
    m = re.search(r"^- Persist artifacts incrementally:.*?finish and report\.$",
                  code_full, re.M | re.S)
    assert m, "persist bullet not found in agentic-code.md"
    persist_bullet = m.group(0)
    m2 = re.search(r"^## Choose how to execute the remaining work$.*?(?=^## )",
                   code_full, re.M | re.S)
    assert m2, "capability section not found in agentic-code.md"
    cap_section = m2.group(0).rstrip() + "\n"

    assert base.count(PERSIST_ANCHOR) == 1
    common = base.replace(PERSIST_ANCHOR,
                          PERSIST_ANCHOR + persist_bullet + "\n", 1)
    assert common.count(VERIFY_ANCHOR) == 1
    a_system = common.replace(VERIFY_ANCHOR,
                              cap_section + "\n" + VERIFY_ANCHOR, 1)
    # 自检：A 减去能力段 == 公共段（差分仅剩能力披露）
    assert a_system.replace(cap_section + "\n", "", 1) == common
    recipe = {
        "agentic_md_sha256": hashlib.sha256(
            (ROOT / "examples" / "prompts" / "agentic.md").read_bytes()).hexdigest(),
        "agentic_code_md_sha256": hashlib.sha256(
            (ROOT / "examples" / "prompts" / "agentic-code.md").read_bytes()).hexdigest(),
        "persist_bullet_sha256": hashlib.sha256(persist_bullet.encode()).hexdigest(),
        "cap_section_sha256": hashlib.sha256(cap_section.encode()).hexdigest(),
        "common_sha256": hashlib.sha256(common.encode()).hexdigest(),
        "a_system_sha256": hashlib.sha256(a_system.encode()).hexdigest(),
    }
    return common, a_system, recipe


def check_global_skills_clean() -> None:
    """全局技能目录若存在旧 run-code 技能会污染两臂工具面（v0b 同款守卫）。"""
    g = Path.home() / ".config" / "tinyagent" / "skills" / "run-code"
    if g.exists():
        sys.exit(f"全局技能目录存在旧副本会覆盖装置：{g} —— 先清理")


def isolated_env(extra: dict = None) -> dict:
    """装置运行环境：XDG_CONFIG_HOME 指向空目录，隔离宿主全局技能
    （~/.config/tinyagent/skills 里的 tw-demo 等会让 B 臂也长出
    read_skill/list_skill + intro，破坏冻结差分）。run-g1 真实跑与
    inventory/mock 联通都必须走这里。"""
    env = dict(os.environ)
    xdg = ART / "xdg-isolated"
    (xdg / "tinyagent").mkdir(parents=True, exist_ok=True)
    env["XDG_CONFIG_HOME"] = str(xdg)
    if extra:
        env.update(extra)
    return env


def build_config(task: str, arm: str, wd: Path, base_url: str, model: str,
                 fixture_root: Path, server_log: Path = None) -> dict:
    """A/B 配置：唯一差异 = code 可用包（enabled + on_demand skill）。

    A/B 用同一个 code-feature 二进制、同模型、同 limits、同公共任务文本
    （任务文本经 --task 注入，不进 system prompt，两臂逐字相同）。
    B 不在 tools 白名单里列 run_code（成员资格由 code.enabled 驱动）。
    """
    tools = (["shell", "read_file", "write_file", "edit_file", "list_dir"]
             if task in ("t0", "t1")
             else ["read_file", "write_file", "edit_file", "list_dir"])
    cfg = {
        "provider": "openai",
        "base_url": base_url,
        "api_key_env": API_KEY_ENV,
        "model": model,
        "max_tokens": MAX_TOKENS_PER_REQUEST,
        "system_prompt": (PROMPT_A if arm == "A" else PROMPT_COMMON),
        "workdir": str(wd),
        "tools": tools,
        "restrict_to_workdir": True,
        "inject_environment": False,
        "limits": dict(RUN_LIMITS),
        "code": {
            "enabled": arm == "A",
            "skill_injection": "on_demand",
            "include": False,
            "call_batch": False,
            "llm": {"enabled": False},
        },
    }
    if task == "t2":
        cmd = fixture.mcp_server_command(fixture_root, server_log)
        cfg["mcp_servers"] = [{"name": "t2data",
                               "command": cmd[0], "args": cmd[1:]}]
    return cfg




def run_binary(cfg_path: Path, task_text: str, run_dir: Path, env: dict,
               timeout_s: int, seatbelt_profile: Path = None):
    """跑一次 headless run，封存 events.ndjson / stderr.log / result.json。

    seatbelt_profile 非空时经 sandbox-exec 启动（P1-04 答案隔离）。
    返回 (rc, wall_seconds)。进程外超时：杀进程、如实记 timeout（产物在
    终止后按现状评分——截止后的补写不接收，因为进程已死）。
    """
    t0 = time.time()
    argv = [str(BIN), "--config", str(cfg_path), "--task", task_text,
            "--session", "none", "--json", "--output",
            str(run_dir / "result.json")]
    if seatbelt_profile is not None:
        argv = ["sandbox-exec", "-f", str(seatbelt_profile), *argv]
    with open(run_dir / "events.ndjson", "w") as evf, \
         open(run_dir / "stderr.log", "w") as logf:
        try:
            p = subprocess.run(argv, stdout=evf, stderr=logf, env=env,
                               timeout=timeout_s)
            rc = p.returncode
        except subprocess.TimeoutExpired:
            rc = 124
    return rc, round(time.time() - t0, 1)


def score_run(run_dir: Path, task: str, arm: str, fixture_root: Path,
              price) -> dict:
    """验收 + 计账（score 模块），写 validation/usage/summary.json。

    arm 由调用方显式传入（run_id 尾段是 normal/failure 等场景名，不是臂）。
    events.ndjson 一并传入：usage_from_result 用 turn/usage 事件差检出
    「部分请求没报账」的未知消耗（§4.1）。
    """
    result = {}
    rp = run_dir / "result.json"
    if rp.exists():
        try:
            result = json.load(open(rp, encoding="utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            # P1-01：损坏的 result 与缺失同罪——不崩、不归零，events 核账
            result = {}
    gold = scorer.load_gold(fixture_root / task)
    validation = scorer.validate_artifact(run_dir / "wd" / "answer.jsonl",
                                          gold, task)
    usage = scorer.usage_from_result(result, run_dir / "events.ndjson")
    cost = scorer.cost_from_usage(usage, price)
    write_json(run_dir / "validation.json", validation)
    write_json(run_dir / "usage.json", usage)
    summary = scorer.summarize_run(
        {"run_id": run_dir.name, "task": task, "arm": arm},
        validation, usage, cost,
        end_reason=result.get("stop_reason") or "no_result")
    write_json(run_dir / "summary.json", summary)
    return summary


def gold_as_answer_lines(fixture_root: Path, task: str) -> str:
    """把 gold 渲染成 answer.jsonl 的正确内容（mock「完美模型」写出用）。

    gold 从快照现算（scorer.load_gold 同源——方案 §2.1 gold 不落盘）。
    """
    gold = scorer.load_gold(fixture_root / task)
    id_key = next(iter(gold))
    if task == "t0":
        return "".join(json.dumps({"id": t}, ensure_ascii=False) + "\n"
                       for t in gold[id_key])
    return "".join(
        json.dumps({"user_id": u, "event_count": c}, ensure_ascii=False) + "\n"
        for u, c in gold[id_key].items())


# 模块级初始化（build_config 引用；须在 prompt_parts 定义后执行）
PROMPT_COMMON, PROMPT_A, PROMPT_RECIPE = prompt_parts()


def start_mock_server(run_dir: Path, scenario: dict):
    """起场景化 mock provider（127.0.0.1），返回 (proc, base_url)。"""
    scenario_path = run_dir / "scenario.json"
    write_json(scenario_path, scenario)
    proc = subprocess.Popen(
        [sys.executable, str(ROOT / "scripts" / "code_value_mock_provider.py"),
         "--scenario", str(scenario_path)],
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    port = proc.stdout.readline().strip()
    if not port.isdigit():
        proc.kill()
        raise RuntimeError(f"mock provider did not report a port: {port!r}")
    return proc, f"http://127.0.0.1:{port}"


def mock_plumbing_run(batch_dir: Path, fixture_root: Path, name: str,
                      task: str, arm: str, scenario: dict, expect: dict):
    """一次零付费联通 run：mock provider + 真二进制 + 评分 + 封存。

    expect 里是装置断言（delivered / usage_missing / run_error_tool /
    run_code_live 等），失败大声报错——G0 要的是证明，不是绿灯。
    """
    run_dir = batch_dir / "runs" / f"{name}"
    claim_run_slot(run_dir)   # P1-03：原子 started 登记 + 中断记录不可覆盖
    wd = run_dir / "wd"
    fixture.stage_workdir(task, wd, fixture_root)
    server_log = run_dir / "server_events.ndjson"
    proc, base_url = start_mock_server(run_dir, scenario)
    try:
        cfg = build_config(task, arm, wd, base_url, "mock-model",
                           fixture_root, server_log)
        cfg_path = run_dir / "config_redacted.json"
        write_json(cfg_path, cfg)
        env = isolated_env({"CODE_VALUE_KEY": "mock-not-a-real-key"})
        # P1-04：mock 联通同样过 Seatbelt——证明隔离下装置全链路可用
        # （T2 场景的 MCP 数据源经 literal allow 洞放行，探针验证）
        mcp_cmd = (cfg.get("mcp_servers") or [{}])[0]
        mcp_command = ([mcp_cmd["command"]] + mcp_cmd.get("args", [])
                       if cfg.get("mcp_servers") else None)
        sb = seatbelt_profile(run_dir, wd, mcp_command)
        sb_check = verify_seatbelt(sb, wd, mcp_command)
        write_json(run_dir / "seatbelt_check.json", sb_check)
        rc, wall = run_binary(cfg_path, fixture.TASK_TEXTS[task], run_dir,
                              env, timeout_s=RUN_LIMITS["timeout_secs"] + 60,
                              seatbelt_profile=sb)
    finally:
        proc.terminate()
        proc.wait(timeout=5)

    result = {}
    rp = run_dir / "result.json"
    if rp.exists():
        try:
            result = json.load(open(rp, encoding="utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            result = {}   # 损坏与缺失同罪：events 核账，不崩不归零
    summary = score_run(run_dir, task, arm, fixture_root, price=None)
    summary.update({"wall_s": wall, "rc": rc, "provider": "mock",
                    "paid_model_requests": 0,
                    "instance_id": 1, "repetition": 1})

    # 装置断言：证据不够即 fail（G0 放行表 3/4 条）
    checks = {}
    def check(key, ok, note=""):
        checks[key] = {"ok": bool(ok), "note": note}

    check("delivered_exact", summary["delivered_exact"] is expect["delivered"],
          "; ".join(summary["reject_reasons"])[:300])
    check("usage_missing", summary["usage"]["usage_missing"]
          is expect.get("usage_missing", False))
    if "partial_unknown" in expect:
        # 失败轮（HTTP 500 无 usage 帧）烧了没报账：必须检出并预留，
        # cost=null 而非把失败请求当 0（§4.1）
        check("partial_unknown",
              summary["usage"]["partial_unknown"]
              is expect["partial_unknown"]
              and summary["usage"]["unknown_reserve_tokens"] > 0
              and summary["cost"]["cost"] is None,
              json.dumps({"unreported": summary["usage"]["unreported_requests"],
                          "reserve": summary["usage"]["unknown_reserve_tokens"],
                          "cost_reason": summary["cost"].get("reason")}))
    tool_calls = result.get("tool_calls") or []
    names = [(c.get("name"), c.get("is_error")) for c in tool_calls]
    if "run_code_live" in expect:  # A 臂：run_code 真实执行成功
        rc_calls = [c for c in tool_calls if c.get("name") == "run_code"]
        ok = bool(rc_calls) and not rc_calls[0]["is_error"]
        check("run_code_live", ok is expect["run_code_live"],
              f"run_code calls: {names}")
    if "run_code_absent_error" in expect:  # B 臂：调不存在的工具 → 错误结果
        rc_calls = [c for c in tool_calls if c.get("name") == "run_code"]
        ok = bool(rc_calls) and rc_calls[0]["is_error"]
        check("run_code_absent_error", ok is expect["run_code_absent_error"],
              f"run_code calls: {names}")
    if expect.get("mcp_tools_live"):
        # t2：MCP 工具在 Seatbelt 隔离下真实执行成功（服务器活着且返回
        # 分页数据，不是空调用）——方案 §4.3「有效承接」的装置侧前提
        mcp_names = ("mcp_t2data_subscriptions_page",
                     "mcp_t2data_events_page")
        mcp_calls = [c for c in tool_calls
                     if c.get("name") in mcp_names]
        ok = bool(mcp_calls) and not any(c["is_error"] for c in mcp_calls)
        check("mcp_tools_live", ok is expect["mcp_tools_live"],
              f"mcp calls: {names}")
    check("events_recorded", (run_dir / "events.ndjson").stat().st_size > 0)
    check("seatbelt_isolation", sb_check["ok"],
          json.dumps(sb_check))
    check("abnormal_end", summary["abnormal_end"]
          is expect.get("abnormal_end", False), summary["end_reason"])
    if expect.get("usage_positive"):
        check("usage_positive", summary["usage"]["total_tokens"] > 0,
              f"total={summary['usage']['total_tokens']}")
    if expect.get("cost_reserved_not_zero"):
        check("cost_reserved_not_zero",
              summary["cost"]["cost"] is None
              and summary["usage"]["unknown_reserve_tokens"] > 0,
              json.dumps(summary["cost"]))
    write_json(run_dir / "apparatus_checks.json", checks)
    mark_run_completed(run_dir)   # P1-03：completed 独立记录
    bad = [k for k, v in checks.items() if not v["ok"]]
    if bad:
        print(f"[g0] PLUMBING FAIL {name}: {bad}", file=sys.stderr)
    return {"run_id": name, "task": task, "arm": arm, "summary": summary,
            "checks": checks, "ok": not bad, "run_dir": str(run_dir)}


def collect_inventory(batch_dir: Path, configs: list) -> dict:
    """驱动 Rust 侧测试导出 A/B 工具清单 + 系统提示词（真实构造的 Agent）。"""
    spec_path = batch_dir / "inventory_spec.json"
    out_path = batch_dir / "inventory.json"
    write_json(spec_path, {"out": str(out_path), "configs": configs})
    env = isolated_env({"CODE_VALUE_INVENTORY_SPEC": str(spec_path)})
    r = subprocess.run(
        ["cargo", "test", "--features", "code,mock",
         "--test", "code_value_apparatus", "--", "--nocapture"],
        cwd=ROOT, env=env, capture_output=True, text=True)
    if r.returncode != 0 or not out_path.exists():
        print(r.stdout[-3000:], file=sys.stderr)
        print(r.stderr[-3000:], file=sys.stderr)
        sys.exit("inventory collection failed (cargo test)")
    return json.load(open(out_path, encoding="utf-8"))


def analyze_inventory(inventory: dict) -> dict:
    """只读差分检查（方案 §3：除能力披露及 code 可用性外的意外差异一律阻止）。

    工具面：A − B 必须恰好等于 run_code + read_skill + list_skill（skill
    对随内置 run-code 技能索引出现）；T2 两臂都含两个 MCP 分页工具。
    系统提示词：B == 公共段（B 无技能索引，assemble 不追加任何内容）；
    A == 公共段 + 能力段 + 技能索引 intro（render_intro 追加）。
    guest 可调用工具只在 A 有；T2-A 的 guest registry 必须含 MCP 工具。
    """
    problems = []
    diff_summary = {}
    tasks = sorted({v["task"] for v in inventory.values()})
    expected_extra = {"run_code", "read_skill", "list_skill"}
    for task in tasks:
        a = inventory.get(f"{task}-A")
        b = inventory.get(f"{task}-B")
        if not a or not b:
            problems.append(f"{task}: inventory missing an arm")
            continue
        a_tools, b_tools = set(a["agent_tool_names"]), set(b["agent_tool_names"])
        extra, missing = a_tools - b_tools, b_tools - a_tools
        if extra != expected_extra:
            problems.append(f"{task}: A-only tools {sorted(extra)} "
                            f"!= {sorted(expected_extra)}")
        if missing:
            problems.append(f"{task}: B has tools A lacks: {sorted(missing)}")
        if a["request_tool_names"]:
            if set(a["request_tool_names"][0]) != a_tools:
                problems.append(f"{task}-A: request tools != agent tools")
        if b["request_tool_names"]:
            if set(b["request_tool_names"][0]) != b_tools:
                problems.append(f"{task}-B: request tools != agent tools")
        if a["guest_tools"] is None:
            problems.append(f"{task}-A: guest_tools missing")
        if b["guest_tools"] is not None:
            problems.append(f"{task}-B: guest_tools must be null (no sandbox)")
        if task == "t2":
            for t in ("mcp_t2data_subscriptions_page", "mcp_t2data_events_page"):
                if t not in a_tools or t not in b_tools:
                    problems.append(f"t2: {t} missing from an arm")
                guest_names = {g.get("name") for g in (a["guest_tools"] or [])}
                if t not in guest_names:
                    problems.append(f"t2-A guest registry lacks {t}")
        sa, sb = a["system_prompt"] or "", b["system_prompt"] or ""
        if sb != PROMPT_COMMON:
            problems.append(f"{task}-B: system prompt != frozen common base")
        if not sa.startswith(PROMPT_A):
            problems.append(f"{task}-A: system prompt lacks frozen capability "
                            "section prefix")
        diff_summary[task] = {
            "a_only_tools": sorted(extra), "b_only_tools": sorted(missing),
            "a_system_len": len(sa), "b_system_len": len(sb),
            "a_skill_intro_len": len(sa) - len(PROMPT_A),
            "guest_tool_count": len(a["guest_tools"] or []),
        }
    return {"problems": problems, "per_task": diff_summary}


# ── 预算闸（方案 §4.1：防「软闸」冒充硬预算）────────────────────────────────

def budget_gate(ledger_tokens: int, unknown_reserve: int, envelope: int,
                per_run_reserve: int) -> dict:
    """启动前额度检查：已知用量 + 未知预留 + 一个 run 的预留 ≤ 包络，
    否则不启动。原生 token 闸是轮边界软闸——在途超额风险单列上报。"""
    needed = ledger_tokens + unknown_reserve + per_run_reserve
    return {
        "ok": needed <= envelope,
        "ledger_tokens": ledger_tokens,
        "unknown_reserve_tokens": unknown_reserve,
        "per_run_reserve_tokens": per_run_reserve,
        "envelope_tokens": envelope,
        "headroom_tokens": envelope - needed,
        "inflight_overrun_risk_tokens": INFLIGHT_OVERRUN_TOKENS,
        "note": "原生闸在轮边界生效，最后一轮可再消耗至单请求输出上限；"
                "该上界不是供应商扣费硬上限",
    }


def require_paid_prereqs(manifest: dict) -> list:
    """付费执行前置（方案 §4.1 + 工单 01R P1-01）。返回缺失/非法项。

    价表：五项齐全、单价有限非负、币种非空且与 cost_currency 一致；
    停止值：有限正数。存在性检查不够——负价/NaN 同样拒绝。
    """
    problems = []
    price = manifest.get("price_snapshot")
    ok, why = scorer.validate_price_snapshot(price)
    if not ok:
        problems.append(f"price_snapshot({why})")
    cur = manifest.get("cost_currency")
    if not isinstance(cur, str) or not cur.strip():
        problems.append("cost_currency")
    elif ok and price.get("currency") != cur:
        problems.append("cost_currency_mismatch")
    stop = manifest.get("cost_stop")
    if (not isinstance(stop, (int, float)) or isinstance(stop, bool)
            or not math.isfinite(stop) or stop <= 0):
        problems.append("cost_stop")
    # P1-02：价表补全必须经显式 finalize-g1——手改 manifest 填价表
    # （无 config_hashes / final_frozen_at）不得进付费循环。
    if not manifest.get("config_hashes"):
        problems.append("config_hashes(final_freeze_missing)")
    if not manifest.get("final_frozen_at"):
        problems.append("final_frozen_at(final_freeze_missing)")
    return problems


def load_price_snapshot(path) -> dict:
    """价表文件 → {input, output, cache_creation, cache_read, currency}
    （per-token 单价）。缺任一分项即视为价表不完整（返回 None）。"""
    if not path:
        return None
    p = Path(path)
    if not p.exists():
        return None
    raw = json.load(open(p, encoding="utf-8"))
    ok, _why = scorer.validate_price_snapshot(raw)
    return raw if ok else None


# ── manifest（方案 §6.1 必填字段全集）───────────────────────────────────────

MANIFEST_FIELDS = [
    "batch_id", "stage", "experiment_version", "code_head", "dirty_diff_hash",
    "binary_hash", "fixture_hash", "task_hash", "prompt_hashes", "prompt_diff",
    "tool_inventory", "feature_flags", "model", "gateway_identity", "sampling",
    "price_snapshot", "cost_currency", "cost_stop", "token_stop", "turn_limit",
    "wall_limit", "retry_policy", "run_schedule", "success_rule", "gate_rule",
    "demand_status",
]


def build_manifest(batch_id: str, stage: str, fp: dict, fixture_stats: dict,
                   inv: dict, model: str, gateway: dict, price: dict,
                   run_schedule: list) -> dict:
    m = {
        "batch_id": batch_id,
        "stage": stage,
        "experiment_version": EXPERIMENT_VERSION,
        "code_head": fp["code_head"],
        "dirty_diff_hash": fp["dirty_diff_hash"],
        "binary_hash": fp["binary_hash"],
        "fixture_hash": fixture_stats["file_sha256"],
        "task_hash": fixture_stats["task_text_sha256"],
        "prompt_hashes": PROMPT_RECIPE,
        "prompt_diff": {"kind": "A-only capability section + skill intro",
                        "a_only_section_sha256": PROMPT_RECIPE["cap_section_sha256"]},
        "tool_inventory": {k: {"agent_tool_names": v["agent_tool_names"],
                               "guest_tools": v["guest_tools"]}
                           for k, v in inv.items()},
        "feature_flags": {"code": True, "include": False, "call_batch": False,
                          "inner_llm": False,
                          "binary_features": fp["binary_manifest"].get("features")},
        "model": model,
        "gateway_identity": gateway,
        "sampling": {"max_tokens": MAX_TOKENS_PER_REQUEST,
                     "reasoning_effort": None,
                     "temperature": None,
                     "note": "provider defaults; frozen before G1"},
        "price_snapshot": price,
        "cost_currency": (price or {}).get("currency"),
        "cost_stop": None,  # G1 放行前由评测负责人填（¥ 数值）
        "token_stop": {"per_run_soft_gate": RUN_LIMITS["max_total_tokens"],
                       "stage_envelope": (G1_TOKEN_ENVELOPE
                                          if stage == "g1" else 0),
                       "inflight_overrun_risk": INFLIGHT_OVERRUN_TOKENS,
                       "kind": "soft (turn-boundary); not a billing hard cap"},
        "turn_limit": RUN_LIMITS["max_turns"],
        "wall_limit": RUN_LIMITS["timeout_secs"],
        "retry_policy": {"native_retries": 0,
                         "runner_auto_restart": False,
                         "note": "供应商原生重试若存在由网关决定并计费；"
                                 "runner 不自动重启失败 run"},
        "run_schedule": run_schedule,
        "success_rule": "delivered_exact: 窗口内 answer.jsonl 严格合法且 "
                        "目标集合与计数完全一致（code_value_score.validate_artifact）",
        "gate_rule": "budget_gate: 已知用量+未知预留+单run预留≤包络；"
                     "价表/币种/费用停止值缺任一即拒绝付费执行",
        "demand_status": "synthetic / demand_unverified（方案 §1.1）",
    }
    # 价表三件（price_snapshot/cost_currency/cost_stop）：
    #   G0 —— 零付费装置检查，合法为空（显式 null，不是漏填）；
    #   G1 —— 缺失时照样冻结 manifest，但 blocked_paid=True，
    #         require_paid_prereqs 会让 run-g1 --execute 拒绝放行
    #         （方案 §4.1：不填价表不开付费运行）。
    # 其余字段任何阶段都必须有值。
    allowed_null = ("price_snapshot", "cost_currency", "cost_stop")
    missing = [f for f in MANIFEST_FIELDS
               if m.get(f) is None and f not in allowed_null]
    if missing:
        sys.exit(f"manifest 必填字段缺失：{missing}")
    return m


# ── g0：零付费装置检查（工单 01）────────────────────────────────────────────

def cmd_g0(args):
    check_global_skills_clean()
    if not BIN.exists():
        sys.exit(f"missing {BIN} — cargo build --release --features code")
    batch_id = args.batch_id or time.strftime("g0-%Y%m%d-%H%M%S")
    batch_dir = ART / batch_id
    if batch_dir.exists():
        sys.exit(f"batch 目录已存在，拒绝覆盖：{batch_dir}")
    batch_dir.mkdir(parents=True)
    print(f"[g0] batch {batch_id} -> {batch_dir}")

    # 1. 环境指纹
    fp = fingerprint()
    if "code" not in (fp["binary_manifest"].get("features") or []):
        sys.exit("二进制缺 code feature — cargo build --release --features code")
    write_json(batch_dir / "fingerprint.json", fp)

    # 2. fixture（不可变快照 + 许可入口参考解）
    r = subprocess.run([sys.executable, str(ROOT / "scripts" / "code_value_fixture.py"),
                        "--root", str(FIXTURE_ROOT)], cwd=ROOT,
                       capture_output=True, text=True)
    print(r.stdout.strip())
    if r.returncode != 0:
        print(r.stderr, file=sys.stderr)
        sys.exit("fixture generation/selfcheck failed")
    fixture_stats = json.load(open(FIXTURE_ROOT / "gen_stats.json",
                                   encoding="utf-8"))

    # 3. 六份 A/B 配置（mock 联通与 inventory 共用同一配方；base_url 逐 run 覆写）
    #    Config::load 校验 workdir 存在——inventory 只做只读构造，给一个空占位目录
    placeholder = batch_dir / "wd-placeholder"
    placeholder.mkdir(parents=True, exist_ok=True)
    configs_spec = []
    for task, arm in [("t0", "A"), ("t0", "B"), ("t1", "A"), ("t1", "B"),
                      ("t2", "A"), ("t2", "B")]:
        cfg = build_config(task, arm, placeholder,
                           "http://127.0.0.1:1", "mock-model", FIXTURE_ROOT,
                           batch_dir / "server_events.ndjson")
        p = batch_dir / "configs" / f"{task}-{arm}.json"
        write_json(p, cfg)
        configs_spec.append({"key": f"{task}-{arm}", "path": str(p),
                             "task": task, "arm": arm})

    # 4. A/B 工具清单与提示词差分（真实构造的 Agent，非白名单转抄）
    print("[g0] collecting A/B inventory (cargo test) ...")
    inventory = collect_inventory(batch_dir, configs_spec)
    inv_analysis = analyze_inventory(inventory)
    write_json(batch_dir / "inventory_analysis.json", inv_analysis)
    for task in ("t0", "t1", "t2"):
        (batch_dir / "prompts").mkdir(exist_ok=True)
        (batch_dir / "prompts" / f"{task}-A-system.txt").write_text(
            inventory[f"{task}-A"]["system_prompt"] or "", encoding="utf-8")
        (batch_dir / "prompts" / f"{task}-B-system.txt").write_text(
            inventory[f"{task}-B"]["system_prompt"] or "", encoding="utf-8")
    if inv_analysis["problems"]:
        for p in inv_analysis["problems"]:
            print(f"[g0] INVENTORY PROBLEM: {p}", file=sys.stderr)

    # 5. 零付费联通（正常 ×2 / 失败 / 缺 usage）
    print("[g0] mock plumbing runs ...")
    plumbing = []
    gold_t0 = gold_as_answer_lines(FIXTURE_ROOT, "t0")
    gold_t1 = gold_as_answer_lines(FIXTURE_ROOT, "t1")
    plumbing.append(mock_plumbing_run(
        batch_dir, FIXTURE_ROOT, "t1-A-normal", "t1", "A",
        scenario={
            "turns": [
                {"kind": "tool_call", "id": "c1", "name": "run_code",
                 "arguments": {"code": "return tools.list()"},
                 "usage": {"prompt_tokens": 500, "completion_tokens": 40,
                           "cached_tokens": 100}},
                {"kind": "tool_call", "id": "c2", "name": "write_file",
                 "arguments": {"path": "answer.jsonl", "content": gold_t1},
                 "usage": {"prompt_tokens": 900, "completion_tokens": 60,
                           "cached_tokens": 300}},
                {"kind": "text", "text": "done: 18 users",
                 "usage": {"prompt_tokens": 1200, "completion_tokens": 30,
                           "cached_tokens": 800}},
            ]},
        expect={"delivered": True, "run_code_live": True,
                "usage_positive": True, "abnormal_end": False}))
    plumbing.append(mock_plumbing_run(
        batch_dir, FIXTURE_ROOT, "t1-B-normal", "t1", "B",
        scenario={
            "turns": [
                {"kind": "tool_call", "id": "c1", "name": "run_code",
                 "arguments": {"code": "return 1"},
                 "usage": {"prompt_tokens": 400, "completion_tokens": 10,
                           "cached_tokens": 0}},
                {"kind": "tool_call", "id": "c2", "name": "write_file",
                 "arguments": {"path": "answer.jsonl", "content": gold_t1},
                 "usage": {"prompt_tokens": 800, "completion_tokens": 50,
                           "cached_tokens": 200}},
                {"kind": "text", "text": "done: 18 users",
                 "usage": {"prompt_tokens": 1000, "completion_tokens": 25,
                           "cached_tokens": 600}},
            ]},
        expect={"delivered": True, "run_code_absent_error": True,
                "usage_positive": True, "abnormal_end": False}))
    plumbing.append(mock_plumbing_run(
        batch_dir, FIXTURE_ROOT, "t0-A-failure", "t0", "A",
        scenario={
            "turns": [
                {"kind": "tool_call", "id": "c1", "name": "write_file",
                 "arguments": {"path": "answer.jsonl", "content": gold_t0},
                 "usage": {"prompt_tokens": 600, "completion_tokens": 45,
                           "cached_tokens": 0}},
                {"kind": "http_error", "status": 500, "body": "mock boom"},
            ]},
        expect={"delivered": True, "usage_positive": True,
                "abnormal_end": True, "partial_unknown": True}))
    plumbing.append(mock_plumbing_run(
        batch_dir, FIXTURE_ROOT, "t0-B-missing-usage", "t0", "B",
        scenario={
            "turns": [
                {"kind": "tool_call", "id": "c1", "name": "write_file",
                 "arguments": {"path": "answer.jsonl", "content": gold_t0}},
                {"kind": "text", "text": "done: 3 records"},
            ]},
        expect={"delivered": True, "usage_missing": True,
                "cost_reserved_not_zero": True, "abnormal_end": False}))

    # t2 联通：MCP 数据源在 Seatbelt deny 面内（scripts/ + fixture/）——
    # 本场景证明 literal allow 洞放行数据源、真 MCP 服务器在隔离下可起、
    # 模型侧经 MCP 工具读到分页数据后写出答案（g0-20260924-r1 的 T2 付费
    # 跑死于此路径，补进零付费联通防回归）。工具名用模型可见的 sanitize
    # 展示名（mcp_<server>_<tool>），不是 MCP wire 名。
    gold_t2 = gold_as_answer_lines(FIXTURE_ROOT, "t2")
    plumbing.append(mock_plumbing_run(
        batch_dir, FIXTURE_ROOT, "t2-A-mcp-plumbing", "t2", "A",
        scenario={
            "turns": [
                {"kind": "tool_call", "id": "c1",
                 "name": "mcp_t2data_subscriptions_page",
                 "arguments": {"page": 1},
                 "usage": {"prompt_tokens": 500, "completion_tokens": 30,
                           "cached_tokens": 0}},
                {"kind": "tool_call", "id": "c2",
                 "name": "mcp_t2data_events_page",
                 "arguments": {"page": 1},
                 "usage": {"prompt_tokens": 700, "completion_tokens": 30,
                           "cached_tokens": 100}},
                {"kind": "tool_call", "id": "c3", "name": "write_file",
                 "arguments": {"path": "answer.jsonl", "content": gold_t2},
                 "usage": {"prompt_tokens": 900, "completion_tokens": 60,
                           "cached_tokens": 300}},
                {"kind": "text", "text": "done: 8 users",
                 "usage": {"prompt_tokens": 1100, "completion_tokens": 25,
                           "cached_tokens": 500}},
            ]},
        expect={"delivered": True, "mcp_tools_live": True,
                "usage_positive": True, "abnormal_end": False}))

    runs_for_ledger = [{"run_id": p_["run_id"], "task": p_["task"],
                        "arm": p_["arm"], "summary": p_["summary"]}
                       for p_ in plumbing]
    ledger = scorer.aggregate(runs_for_ledger)
    ledger["paid_model_requests"] = 0
    ledger["note"] = "mock plumbing runs; zero paid model requests"
    write_json(batch_dir / "ledger.json", ledger)
    return batch_dir, batch_id, fp, fixture_stats, inventory, inv_analysis, \
        plumbing, ledger


def run_cmd_capture(cmd: list, cwd=ROOT) -> dict:
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    tail = (r.stdout + r.stderr).strip().splitlines()
    return {"cmd": " ".join(cmd), "rc": r.returncode,
            "tail": tail[-8:] if tail else []}


def cmd_g0_finish(ctx) -> int:
    """G0 收尾：定向机制测试 → manifest → 放行表 → 报告。返回 0/1。"""
    (batch_dir, batch_id, fp, fixture_stats, inventory, inv_analysis,
     plumbing, ledger) = ctx
    batch_dir = Path(batch_dir)

    # 定向机制测试作为回归证据（方案 §4.2：复用现有 run_code_skill/lib_api）
    print("[g0] mechanism regression tests ...")
    tests = {}
    tests["rust_code_mock"] = run_cmd_capture(
        ["cargo", "test", "--features", "code,mock"])
    tests["pytest_apparatus"] = run_cmd_capture(
        [sys.executable, "-m", "pytest", "scripts/tests/test_code_value_eval.py",
         "-q"])
    write_json(batch_dir / "regression_tests.json", tests)

    # manifest
    manifest = build_manifest(batch_id, "g0", fp, fixture_stats, inventory,
                              model="mock-model",
                              gateway={"base_url_host": "127.0.0.1",
                                       "model": "mock-model",
                                       "api_key_env": API_KEY_ENV,
                                       "note": "G0 uses the local mock only"},
                              price=None,
                              run_schedule=[{"run_id": p_["run_id"],
                                             "task": p_["task"],
                                             "arm": p_["arm"]}
                                            for p_ in plumbing])
    manifest["cost_stop"] = 0.0  # G0 无付费执行
    write_json(batch_dir / "manifest.json", manifest)

    # G0 放行表（方案 §4.2 六项）
    gate = {}
    gate["1_reference_solves"] = {
        "ok": not fixture_stats.get("selfcheck_problems"),
        "evidence": "fixture gen_stats.selfcheck_problems == [] ; "
                    f"t2 MCP reach: {fixture_stats['t2']['reach']['paging']}"}
    gate["2_validator_rejects_bad"] = {
        "ok": tests["pytest_apparatus"]["rc"] == 0,
        "evidence": "scripts/tests/test_code_value_eval.py（§5.1 反例全套）"}
    gate["3_mock_ab_plumbing"] = {
        "ok": (not inv_analysis["problems"])
              and all(p_["ok"] for p_ in plumbing),
        "evidence": "inventory_analysis.json problems==[] ; "
                    "runs/*/apparatus_checks.json 全绿"}
    fail_kept = next(p_ for p_ in plumbing if p_["run_id"] == "t0-A-failure")
    miss_kept = next(p_ for p_ in plumbing if p_["run_id"] == "t0-B-missing-usage")
    gate["4_failure_and_missing_usage_kept"] = {
        "ok": (
            # 失败/缺 usage 的 run 都在主账里（不剔除、不选择性删除）
            ledger["runs"] == len(plumbing)
            and fail_kept["run_id"] in ledger["failed_runs"]
            # 失败 run 的真实成本不清零
            and fail_kept["summary"]["usage"]["total_tokens"] > 0
            and fail_kept["summary"]["delivered_exact"]
            and fail_kept["summary"]["abnormal_end"]
            # 失败轮没报 usage 的请求：检出 partial_unknown 并预留，
            # cost=null（失败请求的真实消耗不当 0——§4.1）。
            # G0 无价表时 cost_reason 是 price_snapshot_missing（价表闸
            # 先触发）——断言只钉「cost=null + partial 检出 + 预留>0」，
            # partial_usage_missing 路径由 pytest 单测钉住。
            and fail_kept["summary"]["usage"]["partial_unknown"]
            and fail_kept["summary"]["usage"]["unreported_requests"] >= 1
            and fail_kept["summary"]["cost"]["cost"] is None
            and fail_kept["summary"]["usage"]["unknown_reserve_tokens"] > 0
            # 缺 usage 不当零计：cost=null + 预留 > 0
            and miss_kept["summary"]["usage"]["usage_missing"]
            and miss_kept["summary"]["cost"]["cost"] is None
            and miss_kept["summary"]["usage"]["unknown_reserve_tokens"] > 0
        ),
        "evidence": "ledger.json 含全部 4 run（无剔除）；"
                    "t0-A-failure 在 failed_runs 且 usage>0、"
                    "delivered+abnormal_end、partial_unknown 检出"
                    "（unreported≥1，cost=null reason=partial_usage_missing）；"
                    "t0-B-missing-usage cost=null 且 reserve>0"}
    # G0 零付费：价表三件合法为空（显式 null + 原因，不是漏填）；
    # 其余字段必须全有值。G1 冻结时价表缺失 → blocked_paid，run-g1 拒绝。
    g0_null_ok = {"price_snapshot", "cost_currency", "cost_stop"}
    sealed_missing = [f for f in MANIFEST_FIELDS
                      if manifest.get(f) is None and f not in g0_null_ok]
    gate["5_sealed"] = {
        "ok": not sealed_missing,
        "evidence": f"manifest.json（{len(MANIFEST_FIELDS)} 字段；"
                    f"价表三件 G0 合法 null：price_snapshot/cost_currency/"
                    f"cost_stop —— G1 冻结时缺失即 blocked_paid）"}
    gate["6_env_blockers_reported"] = {
        "ok": True,
        "evidence": "本机已知边界见 g0 报告「环境阻塞」节（musl/audit 属 CI 门）"}
    gate_ok = all(v["ok"] for v in gate.values())
    write_json(batch_dir / "g0_gate.json",
               {"all_ok": gate_ok, "items": gate,
                "paid_model_requests": 0})

    write_g0_report(batch_dir, batch_id, ctx, tests, gate, gate_ok, manifest)
    print(f"[g0] gate {'PASS' if gate_ok else 'FAIL'} — "
          f"{batch_dir / 'g0_gate.json'}")
    return 0 if gate_ok else 1


def write_g0_report(batch_dir: Path, batch_id: str, ctx, tests, gate,
                    gate_ok, manifest) -> None:
    (batch_dir, batch_id, fp, fixture_stats, inventory, inv_analysis,
     plumbing, ledger) = ctx
    lines = []
    add = lines.append
    add(f"# G0 装置检查报告 —— {batch_id}")
    add("")
    add(f"- 日期：{time.strftime('%Y-%m-%d %H:%M:%S')}")
    add(f"- HEAD：`{fp['code_head']}`（dirty={fp['dirty']}，"
        f"diff 哈希 `{fp['dirty_diff_hash'][:12]}…`）")
    add(f"- 二进制：`{fp['binary_hash'][:12]}…` features="
        f"{fp['binary_manifest'].get('features')}")
    add(f"- 放行表：**{'全部通过' if gate_ok else '存在不通过项'}**；"
        f"paid_model_requests=0")
    add("")
    add("## 三题与参考解")
    for t in ("t0", "t1", "t2"):
        s = fixture_stats[t]
        add(f"- **{t}**：gold {s['gold_count']}；"
            + (f"页数 {fixture_stats['t2']['reach']['paging']}"
               if t == "t2" else
               f"数据 {s.get('bytes', s.get('records'))}B"))
    add("")
    add("## A/B 公平性（inventory 只读差分）")
    for task, d in inv_analysis["per_task"].items():
        add(f"- {task}：A-only 工具 {d['a_only_tools']}；"
            f"系统提示词差 = 能力段 + 技能 intro（+{d['a_skill_intro_len']} 字符）；"
            f"guest 工具 {d['guest_tool_count']} 个")
    if inv_analysis["problems"]:
        add("- **差分问题**：")
        for p in inv_analysis["problems"]:
            add(f"  - {p}")
    add("")
    add("## 零付费联通（mock provider）")
    for p_ in plumbing:
        checks = "、".join(f"{k}={'OK' if v['ok'] else 'FAIL'}"
                           for k, v in p_["checks"].items())
        add(f"- `{p_['run_id']}`：delivered="
            f"{p_['summary']['delivered_exact']} end={p_['summary']['end_reason']} "
            f"tokens={p_['summary']['usage']['total_tokens']} "
            f"usage_missing={p_['summary']['usage']['usage_missing']} —— {checks}")
    add("")
    add("## 回归测试")
    for name, t in tests.items():
        add(f"- `{t['cmd']}` → rc={t['rc']}")
    add("")
    add("## 环境阻塞与未验证边界")
    add("- musl 静态构建与 cargo-audit 仅 CI 复现（本机不可验证，非本装置阻塞）")
    add("- mock 通过 ≠ 真实模型效果成立；装置合格 ≠ 特性价值成立（方案 §1）")
    add("- T2 参考解可达性：4+8 页、正文 "
        f"{fixture_stats['t2']['reach']['total_body_bytes']}B ≤ 40KB")
    add("")
    add("## 建议")
    add("- " + ("可进入 G1 六跑准备（价表与费用停止值由评测负责人补齐后 "
                "run-g1 --execute 才会放行）" if gate_ok else
                "先修放行表不通过项，再谈 G1"))
    REPORTS.mkdir(parents=True, exist_ok=True)
    (REPORTS / f"{batch_id}.md").write_text("\n".join(lines) + "\n",
                                            encoding="utf-8")


# ── prepare-g1 / run-g1（工单 02 的装置面；G0 通过后由评测负责人派发）────────

def cmd_prepare_g1(args):
    check_global_skills_clean()
    batch_id = args.batch_id or time.strftime("g1-%Y%m%d-%H%M%S")
    batch_dir = ART / batch_id
    if batch_dir.exists():
        sys.exit(f"batch 目录已存在，拒绝覆盖：{batch_dir}")
    batch_dir.mkdir(parents=True)
    fp = fingerprint()
    if "code" not in (fp["binary_manifest"].get("features") or []):
        sys.exit("二进制缺 code feature")
    write_json(batch_dir / "fingerprint.json", fp)

    gateway, secret = gateway_identity()
    if not gateway["model"] or not gateway["base_url_host"]:
        sys.exit("测试模型.md 缺 base url / 模型名称 —— 无法冻结网关身份")

    price = load_price_snapshot(args.price)
    schedule = [{"order": i, "run_id": f"{task}-{arm}", "task": task,
                 "arm": arm} for i, (task, arm) in enumerate(G1_RUNS)]

    # 逐 run 配置（真实网关；密钥经 api_key_env 注入，不落盘）。
    # workdir 目录先建出来：Config::load 校验存在性，而 run-g1 每次跑前
    # 会 stage_workdir 重建内容（状态重置）。
    configs_spec = []
    for s in schedule:
        wd = batch_dir / "runs" / s["run_id"] / "wd"
        wd.mkdir(parents=True, exist_ok=True)
        cfg = build_config(s["task"], s["arm"], wd, secret["base_url"],
                           gateway["model"], FIXTURE_ROOT,
                           batch_dir / "runs" / s["run_id"] / "server_events.ndjson")
        p = batch_dir / "configs" / f"{s['run_id']}.json"
        write_json(p, cfg)
        configs_spec.append({"key": s["run_id"], "path": str(p),
                             "task": s["task"], "arm": s["arm"]})

    inventory = collect_inventory(batch_dir, configs_spec)
    inv_analysis = analyze_inventory(inventory)
    write_json(batch_dir / "inventory_analysis.json", inv_analysis)
    if inv_analysis["problems"]:
        for p in inv_analysis["problems"]:
            print(f"[prepare-g1] INVENTORY PROBLEM: {p}", file=sys.stderr)

    fixture_stats = json.load(open(FIXTURE_ROOT / "gen_stats.json",
                                   encoding="utf-8"))
    manifest = build_manifest(batch_id, "g1", fp, fixture_stats, inventory,
                              model=gateway["model"], gateway=gateway,
                              price=price, run_schedule=schedule)
    manifest["cost_stop"] = args.cost_stop
    missing = require_paid_prereqs(manifest)
    manifest["blocked_paid"] = bool(missing)
    manifest["blocked_paid_missing"] = missing
    write_json(batch_dir / "manifest.json", manifest)

    gate = budget_gate(0, 0, G1_TOKEN_ENVELOPE,
                       RUN_LIMITS["max_total_tokens"])
    write_json(batch_dir / "budget_gate.json", gate)
    print(f"[prepare-g1] {batch_id}：schedule {len(schedule)} runs，"
          f"envelope {G1_TOKEN_ENVELOPE}，"
          f"blocked_paid={manifest['blocked_paid']}({missing or '无'})")
    if missing:
        print("[prepare-g1] 补齐价表（--price，含 input/output/cache_creation/"
              "cache_read/currency 五项）与 --cost-stop 后方可 run-g1 --execute",
              file=sys.stderr)
    return 0


def seatbelt_profile(run_dir: Path, wd: Path,
                     mcp_command: list = None) -> Path:
    """P1-04 答案隔离：macOS Seatbelt（sandbox-exec）配置，评测进程的
    shell 不可达答案面。

    机制：allow default + 对答案面的显式 deny（实测本机 macOS 27 语义：
    deny 按 realpath 匹配，/tmp 符号链接须写 /private/tmp）。答案面 =
    fixture 快照（gold digest）、全部历史批次（mock 场景明文答案 +
    wd/answer.jsonl）、参考解脚本与方案文档、网关凭证文件。
    workdir 与 run_dir（events/result 落盘处）保持可写。
    mcp_command：T2 的 stdio MCP 服务器启动命令。deny scripts/ 与
    fixture/ 会挡住它——对服务器脚本、解释器与其 --subscriptions/
    --events 数据文件打 literal allow 洞（数据是任务输入面，模型本该
    经 MCP 读；fixture 目录级 deny 仍挡 gen_stats/gold digest）。
    返回 profile 路径；非 macOS 或 sandbox-exec 缺失返回 None（调用方
    必须视为阻塞——工单 01R：无法隔离就明确回报，不静默降级）。
    """
    if sys.platform != "darwin":
        return None
    import shutil as _sh
    if not _sh.which("sandbox-exec"):
        return None
    # 当前批次名：run_dir = ART/<batch>/runs/<id>（parent 是 "runs" 不是批次）
    try:
        batch_name = run_dir.resolve().relative_to(
            ART.resolve()).parts[0]
    except ValueError:
        batch_name = run_dir.parent.name
    deny_dirs = [FIXTURE_ROOT] + sorted(
        d for d in ART.iterdir()
        if d.is_dir() and d.name not in ("fixture", "xdg-isolated",
                                         batch_name))
    lines = ["(version 1)", "(allow default)",
             ";; P1-04 答案隔离：答案面不可读（gold digest / 历史 mock"
             " 场景明文 / 参考解 / 凭证）"]
    for d in deny_dirs:
        lines.append(f'(deny file-read* (subpath "{d.resolve()}"))')
    # 同批次兄弟 run 目录：已完成 run 的 wd/answer.jsonl 是 gold 明文，
    # 后续 run 经 shell 可读即批次内答案泄漏——逐个拒绝（本 run 目录
    # 保持可写：events/result/wd 落盘处）。
    runs_root = run_dir.parent
    if runs_root.is_dir() and runs_root.name == "runs" \
            and runs_root.parent.parent.resolve() == ART.resolve():
        for sib in sorted(runs_root.iterdir()):
            if sib.is_dir() and sib.resolve() != run_dir.resolve():
                lines.append(f'(deny file-read* (subpath "{sib.resolve()}"))')
    for d in (ROOT / "docs", ROOT / "scripts", ROOT / "src", ROOT / "tests"):
        lines.append(f'(deny file-read* (subpath "{d.resolve()}"))')
    # git 历史：GOLD_T0 字面量与 fixture 生成器（含 solve_*）已进提交——
    # 不拒绝 .git 则模型可 `git show HEAD:scripts/...` 直读答案
    lines.append(f'(deny file-read* (subpath "{(ROOT / ".git").resolve()}"))')
    # MCP 例外（literal allow 实测可穿透 subpath deny）：服务器脚本本体 +
    # 解释器 + 数据文件。数据是任务输入面；digest 仍被目录级 deny 挡。
    if mcp_command:
        script = next((a for a in mcp_command
                       if str(a).endswith(".py")), None)
        if script:
            lines.append(f'(allow file-read* '
                         f'(literal "{Path(script).resolve()}"))')
        interp = _sh.which(mcp_command[0]) if mcp_command else None
        if interp:
            lines.append(f'(allow file-read* '
                         f'(literal "{Path(interp).resolve()}"))')
        for flag, val in zip(mcp_command, mcp_command[1:]):
            if flag in ("--subscriptions", "--events") and Path(val).exists():
                lines.append(f'(allow file-read* '
                             f'(literal "{Path(val).resolve()}"))')
    lines.append(f'(deny file-read* (literal "'
                 f'{(ROOT / "测试模型.md").resolve()}"))')
    prof = run_dir / "seatbelt.sb"
    prof.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return prof


def verify_seatbelt(profile: Path, wd: Path,
                    mcp_command: list = None) -> dict:
    """P1-04：隔离不是「配置写了」而是「实测不可达」。启动前用真实
    sandbox-exec 探针验证：答案面读不出内容 + workdir 可写 + （T2）
    MCP 服务器脚本与数据文件可读——隔离不能把自己的数据源挡死。"""
    if profile is None:
        return {"ok": False, "reason": "seatbelt_unavailable",
                "note": "非 macOS 或无 sandbox-exec——按工单 01R 视为阻塞"}
    probe_secret = FIXTURE_ROOT / "gen_stats.json"
    r = subprocess.run(["sandbox-exec", "-f", str(profile), "/bin/cat",
                        str(probe_secret)], capture_output=True, text=True)
    secret_leaks = r.returncode == 0 and bool(r.stdout.strip())
    w = subprocess.run(["sandbox-exec", "-f", str(profile), "/bin/sh", "-c",
                        f'echo x > "{wd}/.sb_probe" && cat "{wd}/.sb_probe"'],
                       capture_output=True, text=True)
    wd_ok = w.returncode == 0 and "x" in w.stdout
    mcp_ok = True
    if mcp_command:
        script = next((a for a in mcp_command
                       if str(a).endswith(".py")), None)
        if script:
            m = subprocess.run(["sandbox-exec", "-f", str(profile), "/bin/cat",
                                str(script)], capture_output=True, text=True)
            mcp_ok = m.returncode == 0 and bool(m.stdout.strip())
        for flag, val in zip(mcp_command, mcp_command[1:]):
            if flag in ("--subscriptions", "--events"):
                m = subprocess.run(["sandbox-exec", "-f", str(profile),
                                    "/bin/cat", str(val)],
                                   capture_output=True, text=True)
                mcp_ok = mcp_ok and m.returncode == 0 and bool(m.stdout.strip())
    ok = (not secret_leaks) and wd_ok and mcp_ok
    return {"ok": ok, "secret_leaks": secret_leaks, "workdir_writable": wd_ok,
            "mcp_readable": mcp_ok, "probe_rc": r.returncode}


def reserve_cost(reserve_tokens: int, price) -> float:
    """未知预留 token 折算成货币风险（按最贵适用单价——输出价）。

    预留是保守上界：输入/输出构成未知，按价表里最贵的分项折算。
    价表缺失时返回 0 并由调用方的完整性检查兜底（无价表不会进付费循环）。
    """
    if not price or not reserve_tokens:
        return 0.0
    top = max(price.get(k, 0) or 0
              for k in ("input", "output", "cache_creation", "cache_read"))
    return reserve_tokens * top


def verify_frozen_recipe(batch_dir: Path, manifest: dict) -> list:
    """P1-02：执行时封存校验——任何模型请求前核对实际执行内容。

    记录过哈希 ≠ 通过校验：这里对当前环境重算并逐项比对 manifest。
    漂移项列表非空 → cmd_run_g1 拒绝执行（不进入 run 循环）。
    校验面（工单 01R 第二项）：
      二进制哈希 / 逐 run 配置内容哈希 / 任务文本哈希 / prompt 哈希 /
      工具清单合法（inventory problems == []）/ 模型与网关身份 /
      六跑顺序与冻结 schedule 一致 / 价表经最终冻结步骤补全。
    无关文档提交或历史目录变化不使批次失效（不比对 HEAD）。
    """
    problems = []
    fp = fingerprint()
    if manifest.get("binary_hash") and fp["binary_hash"] != manifest["binary_hash"]:
        problems.append("binary_hash_drift: 实际二进制与冻结记录不一致")
    if manifest.get("fixture_hash"):
        cur = {}
        froot = FIXTURE_ROOT
        if (froot / "gen_stats.json").exists():
            cur = json.load(open(froot / "gen_stats.json",
                                 encoding="utf-8")).get("file_sha256", {})
        if cur != manifest["fixture_hash"]:
            problems.append("fixture_drift: 快照哈希与冻结记录不一致")
    if manifest.get("task_hash"):
        cur_tasks = {k: hashlib.sha256(v.encode()).hexdigest()
                     for k, v in fixture.TASK_TEXTS.items()}
        if cur_tasks != manifest["task_hash"]:
            problems.append("task_text_drift: 任务文本与冻结记录不一致")
    if manifest.get("prompt_hashes"):
        _, _, recipe = prompt_parts()
        if recipe != manifest["prompt_hashes"]:
            problems.append("prompt_drift: 提示词配方与冻结记录不一致")
    inv_path = batch_dir / "inventory_analysis.json"
    if not inv_path.exists():
        problems.append("inventory_missing: 批次无工具清单分析")
    else:
        inv = json.load(open(inv_path, encoding="utf-8"))
        if inv.get("problems"):
            problems.append(f"inventory_invalid: {inv['problems']}")
    # 逐 run 配置：内容哈希 + 模型一致（防手改清单绕过冻结）
    for s in manifest.get("run_schedule", []):
        cfg_path = batch_dir / "configs" / f"{s['run_id']}.json"
        if not cfg_path.exists():
            problems.append(f"config_missing: {s['run_id']}")
            continue
        cfg = json.load(open(cfg_path, encoding="utf-8"))
        if cfg.get("model") != manifest.get("model"):
            problems.append(
                f"config_model_drift: {s['run_id']} 模型 "
                f"{cfg.get('model')!r} != 冻结 {manifest.get('model')!r}")
        frozen_hash = (manifest.get("config_hashes") or {}).get(s["run_id"])
        if frozen_hash:
            cur_hash = hashlib.sha256(
                json.dumps(cfg, sort_keys=True, ensure_ascii=False)
                .encode()).hexdigest()
            if cur_hash != frozen_hash:
                problems.append(f"config_drift: {s['run_id']} 配置被改动")
    # schedule 顺序与冻结时一致
    if manifest.get("run_schedule"):
        expect = [(s["task"], s["arm"]) for s in manifest["run_schedule"]]
        if expect != G1_RUNS:
            problems.append(f"schedule_drift: {expect} != 冻结顺序 {G1_RUNS}")
    return problems


def freeze_final(batch_dir: Path, manifest: dict, price: dict,
                 cost_stop: float) -> dict:
    """P1-02：价格补全的显式最终冻结步骤。

    价表/停止值不是「手改 manifest 就生效」：经此函数写入时同步登记
    config_hashes（逐 run 配置内容哈希）与冻结时间，run-g1 执行时按
    verify_frozen_recipe 重算比对。
    """
    manifest = dict(manifest)
    manifest["price_snapshot"] = price
    manifest["cost_currency"] = price.get("currency")
    manifest["cost_stop"] = cost_stop
    manifest["config_hashes"] = {
        s["run_id"]: hashlib.sha256(
            json.dumps(json.load(open(batch_dir / "configs" /
                                      f"{s['run_id']}.json",
                                      encoding="utf-8")),
                       sort_keys=True, ensure_ascii=False).encode()).hexdigest()
        for s in manifest["run_schedule"]}
    manifest["final_frozen_at"] = time.strftime("%Y-%m-%d %H:%M:%S")
    return manifest


def claim_run_slot(run_dir: Path) -> None:
    """P1-03：启动前原子登记 started；中断记录不可覆盖。

    判据不是「有没有 summary」——已有 started/events/result/产物而未正常
    封存的 run 是「进程已工作但崩溃」的中断态，证据必须保留。新尝试用
    新 run_id；准备阶段的空 workdir（无任何证据文件）可以复用。
    started.json 用 O_EXCL 原子创建：并发双执行者只有一个能登记成功。
    """
    if run_dir.exists():
        evidence = [n for n in ("started.json", "events.ndjson",
                                "result.json", "summary.json")
                    if (run_dir / n).exists()]
        if evidence:
            sys.exit(
                f"run 目录已有证据文件 {evidence}，拒绝覆盖（中断/已完成"
                f"记录必须保留，新尝试用新 run_id）：{run_dir}")
    run_dir.mkdir(parents=True, exist_ok=True)
    started = run_dir / "started.json"
    try:
        # O_EXCL：原子「不存在即创建」——并发第二个执行者在此失败
        fd = os.open(started, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    except FileExistsError:
        sys.exit(f"run 已被其他执行者登记（并发禁止）：{started}")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump({"started_at": time.strftime("%Y-%m-%d %H:%M:%S"),
                   "pid": os.getpid()}, f, ensure_ascii=False)


def mark_run_completed(run_dir: Path) -> None:
    """P1-03：completed 独立记录（summary.json 落盘后调用）。"""
    write_json(run_dir / "completed.json",
               {"finished_at": time.strftime("%Y-%m-%d %H:%M:%S")})


def cmd_finalize_g1(args):
    """P1-02：价格补全的显式最终冻结步骤（不是手改 manifest）。

    读价表文件 + 停止值 → 校验 → freeze_final 写回 manifest（含逐 run
    配置哈希与冻结时间）。之后 run-g1 --execute 才可能通过校验。
    """
    batch_dir = Path(args.batch)
    manifest_path = batch_dir / "manifest.json"
    if not manifest_path.exists():
        sys.exit(f"没有冻结的 manifest：{manifest_path}（先 prepare-g1）")
    manifest = json.load(open(manifest_path, encoding="utf-8"))
    if manifest.get("stage") != "g1":
        sys.exit(f"manifest stage != g1：{manifest.get('stage')}")
    price = load_price_snapshot(args.price)
    if price is None:
        sys.exit("价表缺失/非法（需五项：input/output/cache_creation/"
                 "cache_read/currency，单价有限非负）")
    if args.cost_stop is None or args.cost_stop <= 0:
        sys.exit("费用停止值必须为有限正数")
    drift = verify_frozen_recipe(batch_dir, manifest)
    if drift:
        for d in drift:
            print(f"[finalize-g1] RECIPE DRIFT: {d}", file=sys.stderr)
        sys.exit("冻结配方已漂移 —— 重新 prepare-g1 后再 finalize")
    manifest = freeze_final(batch_dir, manifest, price, args.cost_stop)
    manifest["blocked_paid"] = False
    write_json(manifest_path, manifest)
    print(f"[finalize-g1] {batch_dir.name} 最终冻结完成：价表 "
          f"{price['currency']}，停止值 {args.cost_stop}；"
          "run-g1 --execute 将按此校验")
    return 0


def cmd_run_g1(args):
    """执行冻结的 G1 配方。付费执行三重门：--execute、价表齐备、预算闸。"""
    batch_dir = Path(args.batch)
    manifest_path = batch_dir / "manifest.json"
    if not manifest_path.exists():
        sys.exit(f"没有冻结的 manifest：{manifest_path}（先 prepare-g1）")
    manifest = json.load(open(manifest_path, encoding="utf-8"))
    if manifest.get("stage") != "g1":
        sys.exit(f"manifest stage != g1：{manifest.get('stage')}")
    if not args.execute:
        print("[run-g1] dry-run（默认）：只打印将执行的顺序，不发起任何请求")
        for s in manifest["run_schedule"]:
            print(f"  order {s['order']}: {s['run_id']}")
        return 0
    missing = require_paid_prereqs(manifest)
    if missing:
        sys.exit(f"付费前置缺失/非法 {missing} —— 方案 §4.1：不填价表不开付费运行")

    # P1-02：执行时封存校验——任何模型请求前核对实际执行内容。
    # 记录过哈希不等于通过校验；漂移即拒绝（不进入 run 循环）。
    drift = verify_frozen_recipe(batch_dir, manifest)
    if drift:
        for d in drift:
            print(f"[run-g1] RECIPE DRIFT: {d}", file=sys.stderr)
        sys.exit("冻结配方校验失败 —— 拒绝执行（重新 prepare-g1 或显式 finalize-g1）")

    price = manifest["price_snapshot"]
    cost_stop = manifest["cost_stop"]
    currency = manifest["cost_currency"]
    ledger_tokens, unknown_reserve = 0, 0
    known_lower_total = 0.0
    reserve_cost_total = 0.0
    runs_done = []
    stop_reason = None
    for s in manifest["run_schedule"]:
        gate = budget_gate(ledger_tokens, unknown_reserve,
                           manifest["token_stop"]["stage_envelope"],
                           manifest["token_stop"]["per_run_soft_gate"])
        if not gate["ok"]:
            print(f"[run-g1] 预算闸拒绝启动 {s['run_id']}：{gate}",
                  file=sys.stderr)
            stop_reason = "budget_gate"
            break
        run_dir = batch_dir / "runs" / s["run_id"]
        claim_run_slot(run_dir)   # P1-03：原子登记 started，拒绝覆盖中断记录
        wd = run_dir / "wd"
        fixture.stage_workdir(s["task"], wd, FIXTURE_ROOT)
        cfg_path = batch_dir / "configs" / f"{s['run_id']}.json"
        cfg = json.load(open(cfg_path, encoding="utf-8"))
        cfg["workdir"] = str(wd)          # 指到本 run 的全新 workdir
        cfg_path = run_dir / "config_redacted.json"
        write_json(cfg_path, cfg)
        _, secret = gateway_identity()
        env = isolated_env({"CODE_VALUE_KEY": secret["api_key"]})
        # P1-04：答案隔离——Seatbelt 实测不可达才启动；不可用即阻塞，
        # 不静默降级（工单 01R：无法隔离就回报，不把答案可达当盲测）。
        # T2 的 MCP 数据源在 deny 面内（scripts/ + fixture/）——把服务器
        # 命令传进去打 literal allow 洞，探针同时验证数据源可读。
        mcp_cmd = (cfg.get("mcp_servers") or [{}])[0]
        mcp_command = ([mcp_cmd["command"]] + mcp_cmd.get("args", [])
                       if cfg.get("mcp_servers") else None)
        sb = seatbelt_profile(run_dir, wd, mcp_command)
        sb_check = verify_seatbelt(sb, wd, mcp_command)
        write_json(run_dir / "seatbelt_check.json", sb_check)
        if not sb_check["ok"]:
            sys.exit(f"[run-g1] 答案隔离不可用/未通过（{sb_check}）——"
                     "阻塞：本机无法提供隔离时不启动有答案泄漏的盲测")
        print(f"[run-g1] {s['run_id']} ...", flush=True)
        rc, wall = run_binary(cfg_path, fixture.TASK_TEXTS[s["task"]],
                              run_dir, env,
                              timeout_s=RUN_LIMITS["timeout_secs"] + 60,
                              seatbelt_profile=sb)
        summary = score_run(run_dir, s["task"], s["arm"], FIXTURE_ROOT, price)
        summary.update({"wall_s": wall, "rc": rc, "provider": "real",
                        "order": s["order"], "instance_id": 1,
                        "repetition": 1,
                        "paid_model_requests":
                            summary["usage"]["turns"] or 1})
        write_json(run_dir / "summary.json", summary)
        mark_run_completed(run_dir)   # P1-03：completed 独立于 started
        runs_done.append({"run_id": s["run_id"], "task": s["task"],
                          "arm": s["arm"], "summary": summary})
        ledger_tokens += summary["usage"]["total_tokens"]
        unknown_reserve += summary["usage"]["unknown_reserve_tokens"]
        # 已知费用下界永远累计（P1-01：部分未知不抹掉已知费用）；
        # 未知预留按价表折算成货币风险一并进停止判断。
        known_lower_total += summary["cost"]["cost_known_lower"] or 0
        reserve_cost_total += reserve_cost(
            summary["usage"]["unknown_reserve_tokens"], price)
        print(f"[run-g1] {s['run_id']}：delivered="
              f"{summary['delivered_exact']} end={summary['end_reason']} "
              f"tokens={summary['usage']['total_tokens']} "
              f"cost_lower={known_lower_total:.4f} "
              f"reserve={reserve_cost_total:.4f} {currency}")
        # 停止判断（工单 01R 首轮保守策略）：
        #   a) 费用：已知下界 + 未知预留折算 ≥ 停止值 → 停；
        #   b) 完整性：任一 run 出现无法核账的用量（usage_missing /
        #      partial_unknown）→ 立即停止启动后续 run，保留证据回报。
        # 这是 run 间停止线，不是供应商扣费硬上限（在途风险另列）。
        if known_lower_total + reserve_cost_total >= cost_stop:
            print(f"[run-g1] 费用停止线触发：lower="
                  f"{known_lower_total:.4f} + reserve="
                  f"{reserve_cost_total:.4f} ≥ {cost_stop} {currency}"
                  " —— 不再启动后续 run", file=sys.stderr)
            stop_reason = "cost_stop"
            break
        if not summary["usage"]["usage_complete"]:
            print(f"[run-g1] 用量无法完整核账（"
                  f"{summary['cost'].get('reason')}）——保守停止后续 run，"
                  "证据已封存，交评测负责人裁决", file=sys.stderr)
            stop_reason = "usage_incomplete"
            break
    if runs_done:
        ledger = scorer.aggregate(runs_done)
        ledger["budget_gate_final"] = budget_gate(
            ledger_tokens, unknown_reserve,
            manifest["token_stop"]["stage_envelope"], 0)
        ledger["cost_stop"] = {
            "limit": cost_stop, "currency": currency,
            "known_lower_total": round(known_lower_total, 6),
            "unknown_reserve_cost": round(reserve_cost_total, 6),
            "triggered": stop_reason == "cost_stop",
            "stop_reason": stop_reason,
            "kind": "run 间停止线；非供应商扣费硬上限（在途风险见 "
                    "budget_gate_final.inflight_overrun_risk_tokens）"}
        write_json(batch_dir / "ledger.json", ledger)
    print(f"[run-g1] 完成 {len(runs_done)} runs"
          + (f"（停止原因：{stop_reason}）" if stop_reason else "")
          + "；不自动诊断、不自动进入 G2（方案 §4.3：读完六次才由评测负责人决定）")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    g0 = sub.add_parser("g0", help="零付费装置检查")
    g0.add_argument("--batch-id", default=None)
    g0.set_defaults(func=lambda a: cmd_g0_finish(cmd_g0(a)))
    pg1 = sub.add_parser("prepare-g1", help="冻结 G1 六跑配方（不执行）")
    pg1.add_argument("--batch-id", default=None)
    pg1.add_argument("--price", default=None,
                     help="价表 JSON（input/output/cache_creation/cache_read/"
                          "currency，per-token 单价）")
    pg1.add_argument("--cost-stop", type=float, default=None,
                     help="阶段费用停止值（与价表同币种）")
    pg1.set_defaults(func=cmd_prepare_g1)
    fg1 = sub.add_parser("finalize-g1",
                         help="价格补全的显式最终冻结步骤（P1-02）")
    fg1.add_argument("--batch", required=True)
    fg1.add_argument("--price", required=True)
    fg1.add_argument("--cost-stop", type=float, required=True)
    fg1.set_defaults(func=cmd_finalize_g1)
    rg1 = sub.add_parser("run-g1", help="执行冻结配方（默认 dry-run）")
    rg1.add_argument("--batch", required=True, help="prepare-g1 的批次目录")
    rg1.add_argument("--execute", action="store_true",
                     help="付费执行开关；缺省永远 dry-run")
    rg1.set_defaults(func=cmd_run_g1)
    args = ap.parse_args()
    sys.exit(args.func(args))


if __name__ == "__main__":
    main()
