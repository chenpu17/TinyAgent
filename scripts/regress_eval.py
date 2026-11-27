#!/usr/bin/env python3
"""回归任务集评测（docs/kernel_improvement_plan.md §0.4）。

任务在 examples/regress/tasks/<id>/，每个任务一组文件：
  <id>.json     任务片段（task + limits）——与 `--batch` 目录里的任务文件同格式
  harness.json  评测侧数据，不进模型可见的配置：类别、底配置覆盖、零成本通道的
                脚本化模型（examples/mock_script.py 的格式）
  check.py      通过条件：`python3 check.py <result.json>`（cwd = 本次 run 的
                workdir），退出 0 = 通过。结论只来自它，不来自 CLI 的退出码
  fixture/      运行前拷进 workdir 的初始文件（可选）
  setup.py      生成数据：`python3 setup.py <workdir>`（可选）
  expected/     脚本经 {"$file": "expected/…"} 引用的参考文件（可选）

通道（mode）：
  mock  零成本。每次 run 起一个脚本化的 examples/mock_openai.py 或
        mock_anthropic.py，底配置由本脚本生成——不读用户的配置，不碰任何密钥。
  paid  底配置来自配方指向的配置文件。默认 dry-run；显式 --execute 才真跑；
        价表与费用停止值必须在 prepare 时冻结，缺一即 blocked_paid、run 拒绝放行。
        密钥只经配置里的 api_key_env 由 CLI 自己读取，本脚本不读值、不落盘。
        费用停止值：已花 + 下一个 run 的最坏预留 ≥ 停止值，就不再起新 run；用量
        报不全的 run 按预留记进已花，不当 0。

子命令：
  check      零成本自检，CI 门禁。任务集校验；每个任务 × openai/anthropic 两种线
             协议的脚本化正路径全部通过；「什么都不做」的模型必须过不了判卷；
             同一任务跑两次，汇总逐项相等；付费闸门逐条拒绝放行；mock-smoke 配方
             把 prepare → run → summarize → compare 走一遍。用的假付费配置指向
             127.0.0.1:9、密钥变量名从未设置——check 在任何环境下都发不出一次
             付费请求。
  prepare    冻结配方：二进制、评测脚本与任务文件的哈希，逐 run 配置，执行顺序
             （重复轮次间 AB/BA 交替）。
  run        执行冻结批次。默认 dry-run：只打印计划，不落任何文件。已有
             verdict.json 的 run 跳过（可续跑），半途的 run 目录挪开、绝不覆盖。
  summarize  把一个批次汇总成 summary.json（metrics 可复现；timing 不可）。
  compare    臂间对比表：一个批次内的各臂，或 --a/--b 两份 summary（跨内核版本）。

工件：artifacts/regress/<批次>/（manifest.json、runs/<task>/<arm>/<n>/ 下的
config.json、events.ndjson、run.log、result.json、verdict.json、workdir/）。
批次目录名带时间戳，已存在即拒绝——不覆盖旧记录。

用法：
  python3 scripts/regress_eval.py check
  python3 scripts/regress_eval.py prepare examples/regress/recipes/kernel-regress.json
  python3 scripts/regress_eval.py prepare <配方> --price-table prices.json --cost-stop 5
  python3 scripts/regress_eval.py prepare <付费配方> --mock anthropic   # 同一批任务零成本走一遍
  python3 scripts/regress_eval.py prepare <付费配方> --base-config <别的配置文件>
  python3 scripts/regress_eval.py run artifacts/regress/<批次>            # dry-run
  python3 scripts/regress_eval.py run artifacts/regress/<批次> --execute  # 付费
  python3 scripts/regress_eval.py compare --batch artifacts/regress/<批次>
  python3 scripts/regress_eval.py compare --a <旧 summary.json> --b <新 summary.json>

价表格式与 scripts/code_value_eval.py 相同：{currency, input, output,
cache_creation, cache_read}，每 token 单价。计费口径复用 code_value_score
（result.json 为主、events.ndjson 核账，未知用量绝不归零）。

TA_BIN 环境变量可指定被测二进制（默认 target/debug/tiny-agent）。
"""
import argparse
import copy
import datetime
import hashlib
import io
import json
import math
import os
import re
import shutil
import subprocess
import sys
import time
import traceback
from collections import Counter
from pathlib import Path

sys.dont_write_bytecode = True  # 评测不往仓库里落 __pycache__
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "examples"))
import code_value_score as scorer  # noqa: E402  计费口径与 code_value 评测同源
import mock_script  # noqa: E402  脚本格式校验与两个 mock 同源

REGRESS = ROOT / "examples" / "regress"
TASKS_DIR = REGRESS / "tasks"
RECIPES_DIR = REGRESS / "recipes"
ART = ROOT / "artifacts" / "regress"
BIN = Path(os.environ.get("TA_BIN") or ROOT / "target" / "debug" / "tiny-agent")
SCHEMA = "regress-eval-v1"
MOCKS = {name: ROOT / "examples" / f"mock_{name}.py" for name in ("openai", "anthropic")}
CHECK_PROVIDERS = ("openai", "anthropic")
MOCK_KEY_ENV = "TINYAGENT_REGRESS_MOCK_KEY"
FAKE_KEY_ENV = "TINYAGENT_REGRESS_FAKE_KEY"  # check 的假付费配置用；从不设置
MOCK_PORT_MARK = "<mock-port>"
WORKDIR_MARK = "<run-dir>/workdir"
# 什么工具都不调、直接宣布完成的「模型」
BAD_MODEL = {"turns": [{"text": "Done."}]}
TOKEN_AXES = ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens",
              "output_tokens", "reasoning_tokens")
# harness.json 的 config / mock.config 只能动这些底配置键：它们决定任务长什么样。
# 模型、端点、密钥变量名属于臂（arm），不属于任务。
TASK_CONFIG_KEYS = ("tools", "system_prompt", "max_tokens", "mcp_servers",
                    "inject_environment", "limits")
# 与 crates/core/src/config.rs 的 `Clamp` 一致：任务片段只能收紧资源上限。
LIMIT_CLAMPS = {
    "max_turns": "tighten", "max_output_chars": "tighten",
    "keep_recent_messages": "tighten", "stream_idle_secs": "tighten",
    "summary_preview_chars": "tighten",
    "timeout_secs": "tighten_or_init", "compress_threshold_chars": "tighten_or_init",
    "max_context_tokens": "tighten_or_init", "max_total_tokens": "tighten_or_init",
    "subagent_max_turns": "tighten_or_init",
    "shell_timeout_secs": "replace",
}
# `Limits::default()`。底配置没写的键按它参与 clamp。
LIMIT_DEFAULTS = {"max_turns": 30, "timeout_secs": 600, "max_output_chars": 30_000,
                  "compress_threshold_chars": 400_000, "keep_recent_messages": 8,
                  "summary_preview_chars": 200,
                  "max_context_tokens": 0, "max_total_tokens": 0, "stream_idle_secs": 30,
                  "shell_timeout_secs": 120, "subagent_max_turns": 25}
MOCK_LIMITS = {"max_turns": 60, "timeout_secs": 120, "compress_threshold_chars": 6000}
# 一个 run 的最坏情况输入窗口未知（两个阈值都没配）时的保守估计
UNKNOWN_WINDOW_TOKENS = 100_000
# 冻结进清单、run 前逐个核对的评测侧文件：改了任何一个都得重新 prepare
HARNESS_FILES = (
    "scripts/regress_eval.py",
    "examples/mock_script.py", "examples/mock_openai.py", "examples/mock_anthropic.py",
    "examples/regress/checklib.py", "examples/regress/mcp_server.py",
)
SETUP_TIMEOUT = 120
CHECK_TIMEOUT = 300
PLACEHOLDER = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}")


class RegressError(Exception):
    """用户可见的失败：参数、配方、冻结校验、付费闸门。退出码 2。"""


# ── 小工具 ────────────────────────────────────────────────────────────


def utcnow():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def stamp():
    # 微秒精度：check 里同一秒内会连着 prepare 好几个批次，秒级会撞名
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")


def read_json(path, where=""):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        raise RegressError(f"{where or path}: cannot read JSON: {e}")


def write_json(path, data, private=False):
    data = json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n"
    if private:
        fd = os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(data)
        return
    Path(path).write_text(data, encoding="utf-8")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def tail(text, limit=400):
    return (text or "").strip()[-limit:]


def first_line(text):
    lines = (text or "").strip().splitlines()
    return lines[0] if lines else ""


def deep_merge(base, patch):
    """dict 递归合并；列表与标量整体替换。不改动入参。"""
    out = copy.deepcopy(base)
    for key, value in (patch or {}).items():
        if isinstance(value, dict) and isinstance(out.get(key), dict):
            out[key] = deep_merge(out[key], value)
        else:
            out[key] = copy.deepcopy(value)
    return out


def is_count(value):
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def is_cost_stop(value):
    """费用停止值：有限的正数。bool 不是数；inf 等于没有上限——都不算。"""
    return (isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(value) and value > 0)


# ── 任务集 ────────────────────────────────────────────────────────────


def _object(value, where):
    if not isinstance(value, dict):
        raise RegressError(f"{where}: must be an object")
    return value


def _config_overrides(value, where):
    _object(value, where)
    unknown = sorted(set(value) - set(TASK_CONFIG_KEYS))
    if unknown:
        raise RegressError(f"{where}: only {', '.join(TASK_CONFIG_KEYS)} may be overridden "
                           f"here (got {', '.join(unknown)}); model/endpoint keys belong to an arm")


def _script(value, where):
    try:
        mock_script.Script(_object(value, where).get("turns"))
    except ValueError as e:
        raise RegressError(f"{where}: {e}")


def load_task(task_dir):
    """读并校验一个任务目录。任何不合规格的地方都是 RegressError。"""
    task_dir = Path(task_dir)
    name = task_dir.name
    fragment = _object(read_json(task_dir / f"{name}.json", f"task {name}"), f"task {name}")
    unknown = sorted(set(fragment) - {"task", "limits"})
    if unknown:
        raise RegressError(f"task {name}: {name}.json may only carry task and limits (got {unknown})")
    if not isinstance(fragment.get("task"), str) or not fragment["task"].strip():
        raise RegressError(f"task {name}: {name}.json needs a non-empty \"task\" string")
    limits = _object(fragment.get("limits", {}), f"task {name}: limits")
    for key, value in limits.items():
        if key not in LIMIT_CLAMPS:
            raise RegressError(f"task {name}: unknown limit {key!r}")
        if not is_count(value):
            raise RegressError(f"task {name}: limits.{key} must be an integer >= 0")

    harness = _object(read_json(task_dir / "harness.json", f"task {name}"), f"task {name}")
    unknown = sorted(set(harness) - {"category", "summary", "config", "mock", "script"})
    if unknown:
        raise RegressError(f"task {name}: harness.json has unknown key(s) {unknown}")
    for key in ("category", "summary"):
        if not isinstance(harness.get(key), str) or not harness[key].strip():
            raise RegressError(f"task {name}: harness.json needs a non-empty \"{key}\" string")
    if "config" in harness:
        _config_overrides(harness["config"], f"task {name}: config")
    mock = _object(harness.get("mock", {}), f"task {name}: mock")
    unknown = sorted(set(mock) - {"config", "bad_script"})
    if unknown:
        raise RegressError(f"task {name}: harness.json mock has unknown key(s) {unknown}")
    if "config" in mock:
        _config_overrides(mock["config"], f"task {name}: mock.config")
    if "script" not in harness:
        raise RegressError(f"task {name}: harness.json needs a \"script\" (the scripted good path)")
    _script(harness["script"], f"task {name}: script")
    if "bad_script" in mock:
        _script(mock["bad_script"], f"task {name}: mock.bad_script")
    if not (task_dir / "check.py").is_file():
        raise RegressError(f"task {name}: check.py is missing")
    return {
        "id": name,
        "dir": task_dir,
        "category": harness["category"],
        "summary": harness["summary"],
        "fragment": fragment,
        "config": harness.get("config", {}),
        "mock_config": mock.get("config", {}),
        "script": harness["script"],
        "bad_script": mock.get("bad_script", BAD_MODEL),
        "check": task_dir / "check.py",
        "setup": task_dir / "setup.py" if (task_dir / "setup.py").is_file() else None,
        "fixture": task_dir / "fixture" if (task_dir / "fixture").is_dir() else None,
    }


def load_tasks(only=None):
    """全部任务（按 id 排序）。只有同时带 <id>.json 的目录才算任务。"""
    tasks = []
    for task_dir in sorted(p for p in TASKS_DIR.iterdir() if p.is_dir()):
        if (task_dir / f"{task_dir.name}.json").is_file():
            tasks.append(load_task(task_dir))
    ids = [t["id"] for t in tasks]
    missing = sorted(set(only or ()) - set(ids))
    if missing:
        raise RegressError(f"unknown task(s): {', '.join(missing)} (have: {', '.join(ids)})")
    return [t for t in tasks if not only or t["id"] in only]


def substitute(node, task_dir, strict):
    """展开评测侧引用：{"$file": "<任务目录下的相对路径>"} → 文件内容；
    ${PYTHON} / ${TASK_DIR} / ${REGRESS_DIR} → 解释器与两个目录的绝对路径。

    strict（任务自己的文件）：不认识的 ${…} 是错误——多半是拼错了。
    非 strict（底配置、臂配置）：属于使用者，原样保留。"""
    if isinstance(node, dict):
        if strict and set(node) == {"$file"}:
            rel = node["$file"]
            path = (task_dir / rel).resolve() if isinstance(rel, str) else None
            # 先验类型再拼路径：{"$file": 5} 要给可读的错误，不是 TypeError
            if path is None or task_dir.resolve() not in path.parents \
                    or not path.is_file():
                raise RegressError(f"task {Path(task_dir).name}: $file {rel!r} must name "
                                   f"a file inside the task directory")
            return path.read_text(encoding="utf-8")
        return {k: substitute(v, task_dir, strict) for k, v in node.items()}
    if isinstance(node, list):
        return [substitute(v, task_dir, strict) for v in node]
    if isinstance(node, str):
        values = {"PYTHON": sys.executable, "TASK_DIR": str(task_dir),
                  "REGRESS_DIR": str(REGRESS)}

        def sub(m):
            if m.group(1) in values:
                return values[m.group(1)]
            if strict:
                raise RegressError(f"task {Path(task_dir).name}: unknown placeholder {m.group(0)}")
            return m.group(0)

        return PLACEHOLDER.sub(sub, node)
    return node


# ── 配置装配 ──────────────────────────────────────────────────────────


def clamp_limit(kind, base, value):
    """crates/core/src/config.rs `Clamp` 的 Python 侧镜像。"""
    if kind == "replace":
        return value
    if kind == "tighten":
        return min(value, base)
    if value == 0:  # 片段没给：沿用底配置
        return base
    return value if base == 0 else min(value, base)  # 底配置 0 = 未设：采纳片段的值


def effective_limits(config):
    limits = {k: v for k, v in (config.get("limits") or {}).items() if v is not None}
    return dict(LIMIT_DEFAULTS, **limits)


def build_config(base, task, arm_config, extra=None):
    """一次 run 的完整配置：底配置 ← 任务覆盖 ←（mock 专用覆盖）← 臂覆盖 ←
    任务片段（task 文本 + 只收紧的 limits）。workdir 由调用方最后填。

    臂在 mock 专用覆盖之后：臂是实验变量，必须说了算——否则「把
    keep_recent_messages 压到 0」这样的臂会被任务的 mock 调参悄悄改回去。"""
    task_dir = task["dir"]
    config = deep_merge(substitute(base, task_dir, False),
                        substitute(task["config"], task_dir, True))
    if extra:
        config = deep_merge(config, substitute(extra, task_dir, True))
    config = deep_merge(config, substitute(arm_config or {}, task_dir, False))
    config["task"] = task["fragment"]["task"]
    limits = effective_limits(config)
    for key, value in task["fragment"].get("limits", {}).items():
        limits[key] = clamp_limit(LIMIT_CLAMPS[key], limits[key], value)
    config["limits"] = limits
    return config


def mock_base_config(port, provider):
    """零成本通道的底配置：本脚本自己生成，不读任何用户文件。"""
    return {
        "provider": provider,
        "api_key_env": MOCK_KEY_ENV,
        "model": "mock-model",
        "base_url": f"http://127.0.0.1:{port}",
        "inject_environment": False,
        "restrict_to_workdir": True,
        "tools": ["shell", "read_file", "write_file", "edit_file", "list_dir"],
        "limits": dict(MOCK_LIMITS),
    }


def strip_url_secrets(url):
    """去掉 query 与 fragment：有的网关把密钥放在那里。"""
    return url.split("?", 1)[0].split("#", 1)[0]


def redact(config):
    """可以写进清单的配置副本。逐 run 的真实配置只在 run 目录里（0600）。

    凭据一律不进清单：extra_headers 与 MCP 条目的 headers / env 只留键名
    （与内核 Config::fingerprint / config 展示同一口径），URL 去掉 query 与
    fragment（有的网关把密钥放在那里）。"""
    out = copy.deepcopy(config)

    def blank(value):
        return {k: "<redacted>" for k in value} if isinstance(value, dict) else value

    if isinstance(out.get("extra_headers"), dict):
        out["extra_headers"] = blank(out["extra_headers"])
    if isinstance(out.get("base_url"), str):
        out["base_url"] = strip_url_secrets(out["base_url"])
    servers = out.get("mcp_servers")
    servers = servers.values() if isinstance(servers, dict) else (servers or [])
    for server in servers:
        if not isinstance(server, dict):
            continue
        for key in ("headers", "env"):
            if isinstance(server.get(key), dict):
                server[key] = blank(server[key])
        if isinstance(server.get("url"), str):
            server["url"] = strip_url_secrets(server["url"])
    return out


def base_identity(config):
    """清单里的底配置身份：不含任何密钥值。"""
    return {
        "provider": config.get("provider"),
        "model": config.get("model"),
        "base_url": strip_url_secrets(config["base_url"]) if config.get("base_url") else None,
        "api_key_env": config.get("api_key_env"),
        "limits": config.get("limits") or {},
    }


def reserve_tokens(config):
    """一个 run 的最坏情况 token 量：每轮都可能把整个窗口重读一遍并写满输出。
    窗口 = max_context_tokens，没配则按 compress_threshold_chars 的 4 字符/token。"""
    limits = effective_limits(config)
    window = limits["max_context_tokens"] or limits["compress_threshold_chars"] // 4 \
        or UNKNOWN_WINDOW_TOKENS
    reserve = limits["max_turns"] * ((config.get("max_tokens") or 8192) + window)
    return min(reserve, limits["max_total_tokens"]) if limits["max_total_tokens"] else reserve


def max_unit_price(price):
    return max(price[k] for k in ("input", "output", "cache_creation", "cache_read"))


def reserve_cost(config, price):
    """预留量按四个单价里最贵的那个折算：宁可高估。"""
    return reserve_tokens(config) * max_unit_price(price)


def charged_so_far(record, price):
    """一个 run 记到「已花」里的数：已知费用的下界 + 未知用量的预留（同上折算）。

    未知的那部分绝不当 0——否则用量报不全的 run 花得再多也顶不到停止值
    （与 code_value_eval 的 cost_stop 口径一致）。"""
    return ((record["cost_known_lower"] or 0.0)
            + (record.get("unknown_reserve_tokens") or 0) * max_unit_price(price))


# ── 配方 / 冻结 ────────────────────────────────────────────────────────


def load_recipe(path):
    path = Path(path)
    recipe = _object(read_json(path, f"recipe {path}"), f"recipe {path}")
    unknown = sorted(set(recipe) - {"name", "description", "base_config", "mock",
                                    "tasks", "arms", "repeats"})
    if unknown:
        raise RegressError(f"recipe {path}: unknown key(s) {unknown}")
    name = recipe.get("name")
    if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9][a-z0-9-]*", name):
        raise RegressError(f"recipe {path}: \"name\" must be lowercase letters, digits and dashes")
    if bool(recipe.get("base_config")) == bool(recipe.get("mock")):
        raise RegressError(f"recipe {path}: give exactly one of \"base_config\" (paid) or \"mock\"")
    if recipe.get("mock") and recipe["mock"] not in MOCKS:
        raise RegressError(f"recipe {path}: \"mock\" must be one of {', '.join(MOCKS)}")
    tasks = recipe.get("tasks", [])
    if not isinstance(tasks, list) or not all(isinstance(t, str) for t in tasks):
        raise RegressError(f"recipe {path}: \"tasks\" must be a list of task ids")
    known = [t["id"] for t in load_tasks()]
    unknown = sorted(set(tasks) - set(known))
    if unknown:
        raise RegressError(f"recipe {path}: unknown task(s) {unknown}")
    arms = recipe.get("arms")
    if not isinstance(arms, list) or not arms:
        raise RegressError(f"recipe {path}: \"arms\" must be a non-empty list")
    names = []
    for arm in arms:
        _object(arm, f"recipe {path}: arm")
        if not isinstance(arm.get("name"), str) or \
                not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", arm["name"]):
            raise RegressError(f"recipe {path}: every arm needs a \"name\" "
                               f"(letters, digits, _ . -)")
        if "config" in arm:
            _object(arm["config"], f"recipe {path}: arm {arm['name']} config")
        names.append(arm["name"])
    if len(set(names)) != len(names):
        raise RegressError(f"recipe {path}: arm names must be unique")
    repeats = recipe.get("repeats", 1)
    if not is_count(repeats) or repeats < 1:
        raise RegressError(f"recipe {path}: \"repeats\" must be an integer >= 1")
    return {
        "name": name,
        "description": recipe.get("description", ""),
        "base_config": recipe.get("base_config"),
        "mock": recipe.get("mock"),
        "tasks": list(tasks) or known,
        "arms": [{"name": a["name"], "label": a.get("label", a["name"]),
                  "config": a.get("config", {})} for a in arms],
        "repeats": repeats,
    }


def binary_identity():
    if not BIN.is_file():
        raise RegressError(f"binary not found: {BIN} (build it, or point TA_BIN at it)")
    try:
        proc = subprocess.run([str(BIN), "--version", "--json"], capture_output=True,
                              text=True, encoding="utf-8", errors="replace", timeout=60)
    except (OSError, subprocess.SubprocessError) as e:
        raise RegressError(f"cannot run {BIN} --version --json: {e}")
    try:
        info = json.loads(proc.stdout)
    except ValueError:
        info = {"error": tail(proc.stdout + proc.stderr, 300)}
    return {"path": str(BIN), "sha256": sha256_file(BIN), "version": info}


def git_head():
    # macOS：仓库里有中文文件名，git 命令一律带 -c core.precomposeunicode=false
    git = ["git", "-c", "core.precomposeunicode=false"]
    try:
        head = subprocess.run(git + ["rev-parse", "HEAD"], cwd=str(ROOT), capture_output=True,
                              text=True, timeout=30).stdout.strip()
        status = subprocess.run(git + ["status", "--porcelain"], cwd=str(ROOT),
                                capture_output=True, text=True, timeout=60).stdout
        return {"head": head or None, "dirty": bool(status.strip())}
    except (OSError, subprocess.SubprocessError):
        return {"head": None, "dirty": None}


def task_files(task_dir):
    """任务目录里的文件 → {相对路径: sha256}。__pycache__ 不算。"""
    files = {}
    for path in sorted(Path(task_dir).rglob("*")):
        if path.is_file() and "__pycache__" not in path.parts and path.suffix != ".pyc":
            files[path.relative_to(task_dir).as_posix()] = sha256_file(path)
    return files


def harness_hashes():
    return {rel: sha256_file(ROOT / rel) for rel in HARNESS_FILES}


def schedule_of(arms, tasks, repeats):
    """执行顺序：重复轮次在外、任务居中、臂在最内；偶数轮把臂序倒过来（AB/BA），
    让时间上的漂移（限流、缓存冷暖）均匀落在两臂上。"""
    rows, order = [], 0
    for repeat in range(1, repeats + 1):
        ordered = list(arms) if repeat % 2 else list(reversed(arms))
        for task in tasks:
            for arm in ordered:
                order += 1
                rows.append({"order": order, "task": task, "arm": arm, "repeat": repeat})
    return rows


def prepare_batch(recipe_path, batch_root, name=None, mock_provider=None,
                  price=None, cost_stop=None, base_config=None):
    """冻结配方 → <batch_root>/<name>-<时间戳>/manifest.json。"""
    recipe = load_recipe(recipe_path)
    if mock_provider and base_config:
        raise RegressError("give either --mock or --base-config, not both")
    if mock_provider:
        if recipe["mock"]:
            raise RegressError("--mock: the recipe is already a mock recipe")
        recipe["mock"] = mock_provider
    if base_config:
        if recipe["mock"]:
            raise RegressError("--base-config: a mock recipe generates its own base config")
        # 清单里记绝对路径：run 可能换一个 cwd，封存校验与取配置都按它找文件
        recipe["base_config"] = str(Path(base_config).expanduser().resolve())
    if recipe["mock"]:
        mode, base, base_file = "mock", mock_base_config(MOCK_PORT_MARK, recipe["mock"]), None
    else:
        mode = "paid"
        base_file = Path(recipe["base_config"]).expanduser().resolve()
        base = _object(read_json(base_file, "base config"), "base config")
        base_file = {"path": str(base_file), "sha256": sha256_file(base_file),
                     "identity": base_identity(base)}
    if price is not None:
        ok, why = scorer.validate_price_snapshot(price)
        if not ok:
            raise RegressError(f"price table invalid: {why}")
    if cost_stop is not None and not is_cost_stop(cost_stop):
        raise RegressError("cost stop must be a positive number")

    tasks = load_tasks(recipe["tasks"])
    configs = {}
    for task in tasks:
        configs[task["id"]] = {}
        for arm in recipe["arms"]:
            extra = task["mock_config"] if mode == "mock" else None
            config = redact(build_config(base, task, arm["config"], extra))
            config["workdir"] = WORKDIR_MARK
            configs[task["id"]][arm["name"]] = config
    name = name or recipe["name"]
    batch_dir = Path(batch_root) / f"{name}-{stamp()}"
    manifest = {
        "schema": SCHEMA,
        "batch_id": batch_dir.name,
        "created_at": utcnow(),
        "mode": mode,
        "mock_provider": recipe["mock"],
        "description": recipe["description"],
        "base_config": base_file,
        "binary": binary_identity(),
        "git": git_head(),
        "harness": harness_hashes(),
        "recipe": recipe,
        "tasks": {t["id"]: {"category": t["category"], "summary": t["summary"],
                            "files": task_files(t["dir"])} for t in tasks},
        "arms": recipe["arms"],
        "repeats": recipe["repeats"],
        "configs": configs,
        "schedule": schedule_of([a["name"] for a in recipe["arms"]],
                                [t["id"] for t in tasks], recipe["repeats"]),
        "price_snapshot": price,
        "cost_stop": cost_stop,
        # 付费通道没有冻结价表与费用停止值：run --execute 一律拒绝
        "blocked_paid": mode == "paid" and not (price and cost_stop is not None),
    }
    try:
        batch_dir.mkdir(parents=True, exist_ok=False)
    except OSError as e:
        raise RegressError(f"cannot create batch directory {batch_dir}: {e}")
    write_json(batch_dir / "manifest.json", manifest)
    return batch_dir, manifest


def verify_frozen(manifest):
    """run 前的封存校验：实际要执行的东西必须仍是 prepare 时冻结的那一份。"""
    problems = []
    binary = manifest.get("binary") or {}
    path = Path(binary.get("path", ""))
    if not path.is_file():
        problems.append(f"binary {path} is gone")
    elif sha256_file(path) != binary.get("sha256"):
        problems.append(f"binary {path} changed since prepare (rebuilt? re-prepare the batch)")
    for rel, digest in (manifest.get("harness") or {}).items():
        if not (ROOT / rel).is_file() or sha256_file(ROOT / rel) != digest:
            problems.append(f"{rel} changed since prepare")
    for task_id, info in (manifest.get("tasks") or {}).items():
        if task_files(TASKS_DIR / task_id) != info.get("files"):
            problems.append(f"task files of {task_id} changed since prepare")
    base = manifest.get("base_config")
    if base:
        if not Path(base["path"]).is_file() or sha256_file(base["path"]) != base["sha256"]:
            problems.append(f"base config {base['path']} changed since prepare")
    if problems:
        raise RegressError("the frozen batch no longer matches the working tree "
                           "(re-prepare it):\n  " + "\n  ".join(problems))


# ── 执行 ──────────────────────────────────────────────────────────────


class Mock:
    """一次 run 专用的脚本化 mock LLM 进程。端口由系统分配，从 stderr 首行读回。"""

    def __init__(self, provider, script_path, log_path):
        self._log = open(log_path, "w", encoding="utf-8")
        self.proc = subprocess.Popen(
            [sys.executable, str(MOCKS[provider]), "0", "--script", str(script_path)],
            stdout=subprocess.DEVNULL, stderr=self._log, env=child_env())
        self.port = None
        deadline = time.monotonic() + 20
        while self.port is None:
            if self.proc.poll() is not None:
                why = f"mock {provider} exited at startup: {tail(self._read_log())}"
                self.close()
                raise RegressError(why)
            if time.monotonic() > deadline:
                self.stop()
                raise RegressError(f"mock {provider} did not announce its port within 20 s")
            m = re.search(r":(\d+)\s*$", first_line(self._read_log()))
            if m:
                self.port = int(m.group(1))
            else:
                time.sleep(0.02)

    def _read_log(self):
        self._log.flush()
        return Path(self._log.name).read_text(encoding="utf-8", errors="replace")

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.close()

    def close(self):
        self._log.close()


def child_env(extra=None):
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
    env.update(extra or {})
    return env


def run_env(run_dir, extra=None):
    """被测二进制（连同它拉起的工具与 MCP 子进程）的环境：继承当前环境，再隔开两样东西。"""
    env = child_env(extra)
    run_dir = Path(run_dir).resolve()
    # 用户级 skills 不进评测——配置目录指到 run 目录内的空目录，两条通道一视同仁
    # （付费 A/B 的两臂不该看到开发者本机碰巧有的技能表）；要评技能就在底配置的
    # [skill] 里显式列出目录。
    xdg = run_dir / ".xdg"
    xdg.mkdir(exist_ok=True)
    env["XDG_CONFIG_HOME"] = str(xdg)
    # 批次目录默认在仓库里（artifacts/regress/<批次>/runs/…）：agent 在 workdir 里发的 git
    # 命令不得解析到外层仓库——第一次真机批次里模型跑了 `git status --short; git diff`，
    # 外层仓库的未提交 diff 被发给了模型，换成 `git stash` 就是改坏开发者的工作区。
    # git 向上找仓库时不进入名单里的目录：workdir 自己是仓库时照常被发现。挡不住的
    # 只剩「先 cd 到批次的祖先目录再 git」——那得 agent 知道并主动写出外层的绝对路径。
    env["GIT_CEILING_DIRECTORIES"] = os.pathsep.join(
        str(p) for p in [run_dir, *run_dir.parents])
    return env


def run_timeout(config):
    return effective_limits(config)["timeout_secs"] + 120


def move_aside(run_dir):
    """半途的 run 目录（没有 verdict.json）挪到一边：从不覆盖已有的东西。"""
    target = run_dir.with_name(f"{run_dir.name}.incomplete-{stamp()}")
    Path(run_dir).rename(target)


def execute_run(task, arm, repeat, run_dir, config, mock_provider=None, script=None):
    """铺开一个 run 目录 → 跑被测二进制 → 跑 check.py → 写 verdict.json。
    `config` 是完整的 run 配置（workdir 与 mock 的 base_url 在这里最后填）。"""
    run_dir = Path(run_dir)
    if not mock_provider and MOCK_PORT_MARK in str(config.get("base_url")):
        raise RegressError("a mock-profile config reached execute_run without a mock to talk to")
    run_dir.parent.mkdir(parents=True, exist_ok=True)
    if run_dir.exists():
        move_aside(run_dir)
    run_dir.mkdir()
    workdir = run_dir / "workdir"
    config = copy.deepcopy(config)
    config["workdir"] = str(workdir)
    verdict = {"schema": SCHEMA, "task": task["id"], "arm": arm, "repeat": repeat,
               "started_at": utcnow(), "exit_code": None, "check_exit": None,
               "passed": False, "check_output": "", "failure": None}
    mock = None
    try:
        workdir.mkdir()
        if task["fixture"] is not None:
            shutil.copytree(task["fixture"], workdir, dirs_exist_ok=True)
        if task["setup"] is not None:
            proc = subprocess.run([sys.executable, str(task["setup"]), str(workdir)],
                                  capture_output=True, text=True, encoding="utf-8",
                                  errors="replace", env=child_env(), timeout=SETUP_TIMEOUT)
            if proc.returncode != 0:
                verdict["failure"] = f"setup.py exited {proc.returncode}: {tail(proc.stderr)}"
        env = run_env(run_dir)
        if mock_provider:
            script_path = run_dir / "mock-script.json"
            write_json(script_path,
                       substitute(task["script"] if script is None else script, task["dir"], True))
            mock = Mock(mock_provider, script_path, run_dir / "mock.log")
            config["base_url"] = f"http://127.0.0.1:{mock.port}"
            env[MOCK_KEY_ENV] = "mock"
        write_json(run_dir / "config.json", config, private=True)
        cmd = [str(BIN), "--config", str(run_dir / "config.json"), "--json",
               "--output", str(run_dir / "result.json"), "--session", "none"]
        try:
            with open(run_dir / "events.ndjson", "w", encoding="utf-8") as out, \
                    open(run_dir / "run.log", "w", encoding="utf-8") as err:
                proc = subprocess.run(cmd, cwd=str(run_dir), env=env, stdout=out, stderr=err,
                                      timeout=run_timeout(config))
            verdict["exit_code"] = proc.returncode
        except subprocess.TimeoutExpired:
            verdict["exit_code"] = 124
            verdict["failure"] = "the run outlived limits.timeout_secs + 120 s (harness timeout)"
    finally:
        if mock is not None:
            mock.stop()
    result_path = run_dir / "result.json"
    if result_path.is_file():
        try:
            proc = subprocess.run([sys.executable, str(task["check"]), str(result_path)],
                                  cwd=str(workdir), capture_output=True, text=True,
                                  encoding="utf-8", errors="replace", env=child_env(),
                                  timeout=CHECK_TIMEOUT)
            verdict["check_exit"] = proc.returncode
            verdict["check_output"] = tail(proc.stdout + proc.stderr, 2000)
            verdict["passed"] = proc.returncode == 0
        except subprocess.TimeoutExpired:
            verdict["failure"] = f"check.py did not finish within {CHECK_TIMEOUT} s"
    else:
        verdict["failure"] = verdict["failure"] or \
            f"no result.json (exit code {verdict['exit_code']})"
        verdict["check_output"] = tail((run_dir / "run.log").read_text(encoding="utf-8"), 2000)
    verdict["finished_at"] = utcnow()
    write_json(run_dir / "verdict.json", verdict)
    return verdict


def run_config(manifest, task, arm_name, run_dir):
    """run 时的真实配置：底配置重新读（付费）/ 现生成（mock），再核对它与清单里
    的脱敏副本一致——清单说的就是跑的。"""
    frozen = manifest["configs"][task["id"]][arm_name]
    if manifest["mode"] == "mock":
        base = mock_base_config(MOCK_PORT_MARK, manifest["mock_provider"])
        extra = task["mock_config"]
    else:
        base = read_json(manifest["base_config"]["path"], "base config")
        extra = None
    arm = next(a for a in manifest["arms"] if a["name"] == arm_name)
    config = build_config(base, task, arm["config"], extra)
    config["workdir"] = str(Path(run_dir) / "workdir")
    shown = redact(deep_merge(config, {"workdir": WORKDIR_MARK}))
    if shown != frozen:
        raise RegressError(f"config of {task['id']}/{arm_name} does not match the frozen manifest")
    return config


def paid_gates(manifest):
    """付费通道放行前的全部闸门。任何一条不过都是 RegressError，且发生在
    第一个 run 启动之前。返回冻结的价表。"""
    if manifest.get("blocked_paid"):
        raise RegressError("blocked_paid: this batch was prepared without --price-table and "
                           "--cost-stop; re-prepare it with both")
    price = manifest.get("price_snapshot")
    ok, why = scorer.validate_price_snapshot(price)
    if not ok:
        raise RegressError(f"price table in the manifest is invalid: {why}")
    if not is_cost_stop(manifest.get("cost_stop")):
        raise RegressError("the manifest has no cost stop; re-prepare it with --cost-stop")
    for config in next(iter(manifest["configs"].values())).values():
        key_env = config.get("api_key_env")
        if not key_env:
            raise RegressError("the base config has no api_key_env; the CLI cannot authenticate")
        if not os.environ.get(key_env):
            # 只看变量在不在；值由 CLI 读，本脚本不读
            raise RegressError(f"environment variable {key_env} is not set (the paid channel "
                               "needs it; the harness never reads the key itself)")
    return price


def run_batch(batch_dir, execute=False, only=None, out=sys.stdout):
    """执行一个冻结批次。返回 {done, ran, stopped}。"""
    # 一律解析成绝对路径：子进程的 cwd 是 run 目录，相对路径给出的批次目录在那里
    # 解析不到（配置文件、workdir 都会指错地方）
    batch_dir = Path(batch_dir).expanduser().resolve()
    manifest = read_json(batch_dir / "manifest.json", f"batch {batch_dir}")
    if manifest.get("schema") != SCHEMA:
        raise RegressError(f"batch {batch_dir}: unsupported manifest schema "
                           f"{manifest.get('schema')!r}")
    verify_frozen(manifest)
    tasks = {t["id"]: t for t in load_tasks(list(manifest["tasks"]))}
    schedule = [s for s in manifest["schedule"] if not only or s["task"] in only]
    if not schedule:
        raise RegressError("nothing to run for this selection")
    paid = manifest["mode"] == "paid"
    runs_root = batch_dir / "runs"

    def state(entry):
        run_dir = runs_root / entry["task"] / entry["arm"] / str(entry["repeat"])
        if (run_dir / "verdict.json").is_file():
            return "done", run_dir
        return ("incomplete" if run_dir.exists() else "pending"), run_dir

    if not execute:
        out.write(f"[dry-run] {batch_dir.name}: {manifest['mode']}"
                  + (f" ({manifest['mock_provider']} mock)" if manifest["mock_provider"] else "")
                  + f", {len(schedule)} run(s), binary {manifest['binary']['sha256'][:12]}\n")
        for entry in schedule:
            out.write(f"  {entry['order']:>3}. {entry['task']:<20} {entry['arm']:<14} "
                      f"repeat {entry['repeat']}  {state(entry)[0]}\n")
        if paid:
            if manifest.get("blocked_paid"):
                out.write("blocked_paid: prepared without --price-table and --cost-stop; "
                          "--execute will be refused\n")
            else:
                out.write(f"cost stop: {manifest['cost_stop']} "
                          f"{manifest['price_snapshot']['currency']}"
                          " (reserve-checked before every run)\n")
        out.write("nothing was written; pass --execute to run\n")
        return {"done": sum(1 for e in schedule if state(e)[0] == "done"), "ran": 0,
                "stopped": None}

    price = paid_gates(manifest) if paid else None
    # 续跑：已经落账的费用先累加进已花（未知用量按预留计，不当 0）
    spent = sum(charged_so_far(r, price) for r in collect_runs(batch_dir, manifest)) \
        if price else 0.0
    done = ran = 0
    stopped = None
    for entry in schedule:
        status, run_dir = state(entry)
        if status == "done":
            done += 1
            out.write(f"[skip] {entry['task']}/{entry['arm']} repeat {entry['repeat']}: "
                      "already has a verdict\n")
            continue
        task = tasks[entry["task"]]
        config = run_config(manifest, task, entry["arm"], run_dir)
        if price:
            reserve = reserve_cost(config, price)
            if spent + reserve >= manifest["cost_stop"]:
                stopped = (f"cost stop {manifest['cost_stop']} {price['currency']}: spent "
                           f"{spent:.4f} + reserve {reserve:.4f} would reach it")
                break
        out.write(f"[run] {entry['task']}/{entry['arm']} repeat {entry['repeat']} …\n")
        out.flush()
        verdict = execute_run(task, entry["arm"], entry["repeat"], run_dir, config,
                              mock_provider=manifest["mock_provider"])
        ran += 1
        record = record_of(run_dir, manifest)
        if price:
            spent += charged_so_far(record, price)
        out.write(f"       {'PASS' if verdict['passed'] else 'FAIL'}"
                  f" (exit {verdict['exit_code']}, {record['metrics']['stop_reason']}, "
                  f"{record['metrics']['turns']} turns"
                  + (f", {record['cost_known_lower']:.4f} {price['currency']}" if price else "")
                  + ")\n")
        if not verdict["passed"]:
            out.write("       " + verdict["check_output"].replace("\n", "\n       ") + "\n")
        out.flush()
        if price and spent >= manifest["cost_stop"]:
            stopped = f"cost stop {manifest['cost_stop']} {price['currency']}: spent {spent:.4f}"
            break
    if stopped:
        out.write(f"[stopped] {stopped}; no further run was started\n")
    out.write(f"{done + ran} run(s) done ({ran} in this call); summary: "
              f"{summarize_batch(batch_dir)}\n")
    return {"done": done + ran, "ran": ran, "stopped": stopped}


# ── 汇总 / 对比 ────────────────────────────────────────────────────────


def read_result(run_dir):
    path = Path(run_dir) / "result.json"
    if not path.is_file():
        return None
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except ValueError:
        return None
    return value if isinstance(value, dict) and value else None


def record_of(run_dir, manifest):
    """一个 run 的账单行：metrics（可复现）与 timing（不可复现）分开。"""
    run_dir = Path(run_dir)
    verdict = read_json(run_dir / "verdict.json", "verdict")
    result = read_result(run_dir)
    price = manifest.get("price_snapshot") if manifest.get("mode") == "paid" else None
    usage = scorer.usage_from_result(result, run_dir / "events.ndjson")
    cost = scorer.cost_from_usage(usage, price) if price else {
        "cost": None, "cost_known_lower": None, "cost_complete": False}
    calls = (result or {}).get("tool_calls") or []
    tokens = dict(usage["tokens"])
    tokens["reasoning_tokens"] = int((result or {}).get("reasoning_tokens") or 0)
    # result.json 缺失（二进制被杀、没写出来）：轮数以 events 里数出来的为准，
    # 不写成 0——崩溃的 run 不是「什么都没做」
    turns = int((result or {}).get("turns") or 0) or int(usage["turns"] or 0)
    return {
        "task": verdict["task"],
        "arm": verdict["arm"],
        "repeat": verdict["repeat"],
        "passed": bool(verdict.get("passed")),
        "exit_code": verdict.get("exit_code"),
        # 失败时留着判卷的原话；通过时不占地方
        "check_output": "" if verdict.get("passed") else verdict.get("check_output", ""),
        "metrics": {
            "passed": bool(verdict.get("passed")),
            "stop_reason": (result or {}).get("stop_reason"),
            "turns": turns,
            "tokens": {axis: int(tokens.get(axis) or 0) for axis in TOKEN_AXES},
            "total_tokens": int(usage["total_tokens"]),
            "compressions": len((result or {}).get("compressions") or []),
            "tool_calls": len(calls),
            "tool_errors": sum(1 for c in calls if c.get("is_error")),
        },
        "timing": {
            "elapsed_ms": int(round(((result or {}).get("elapsed_secs") or 0) * 1000)),
            "tool_ms": [c["duration_ms"] for c in calls if is_count(c.get("duration_ms"))],
        },
        "cost": cost["cost"],
        "cost_known_lower": cost["cost_known_lower"],
        "cost_complete": cost["cost_complete"],
        "unknown_reserve_tokens": usage["unknown_reserve_tokens"],
    }


def collect_runs(batch_dir, manifest):
    rows = []
    for entry in manifest["schedule"]:
        run_dir = Path(batch_dir) / "runs" / entry["task"] / entry["arm"] / str(entry["repeat"])
        if (run_dir / "verdict.json").is_file():
            row = record_of(run_dir, manifest)
            row["order"] = entry["order"]
            rows.append(row)
    return rows


def aggregate(rows):
    if not rows:
        return None
    n = len(rows)

    def mean(get):
        return sum(get(r) for r in rows) / n

    passed = sum(1 for r in rows if r["passed"])
    return {
        "runs": n,
        "passed": passed,
        "pass_rate": passed / n,
        "stop_reasons": dict(sorted(Counter(r["metrics"]["stop_reason"] or "none"
                                           for r in rows).items())),
        "turns_mean": mean(lambda r: r["metrics"]["turns"]),
        "tokens_mean": {axis: mean(lambda r, a=axis: r["metrics"]["tokens"][a]) for axis in TOKEN_AXES},
        "total_tokens_mean": mean(lambda r: r["metrics"]["total_tokens"]),
        "compressions_mean": mean(lambda r: r["metrics"]["compressions"]),
        "tool_calls_mean": mean(lambda r: r["metrics"]["tool_calls"]),
        # 工具报错次数：模型自己重试补救后任务照样能过，但这是内核退步的早期信号
        "tool_errors_mean": mean(lambda r: r["metrics"]["tool_errors"]),
        "elapsed_ms_mean": mean(lambda r: r["timing"]["elapsed_ms"]),
        # 任一 run 的用量不完整，总费用就不可判：给 None，不把未知当 0
        "cost_sum": sum(r["cost"] for r in rows) if all(r["cost"] is not None for r in rows) else None,
        "cost_known_lower_sum": (None if any(r["cost_known_lower"] is None for r in rows)
                                 else sum(r["cost_known_lower"] for r in rows)),
    }


def summarize_batch(batch_dir):
    batch_dir = Path(batch_dir).expanduser().resolve()  # 返回的 summary 路径是绝对的
    manifest = read_json(batch_dir / "manifest.json", f"batch {batch_dir}")
    rows = collect_runs(batch_dir, manifest)
    arms = [a["name"] for a in manifest["arms"]]
    tasks = list(manifest["tasks"])

    def agg(arm=None, task=None):
        return aggregate([r for r in rows
                          if (arm is None or r["arm"] == arm)
                          and (task is None or r["task"] == task)])

    summary = {
        "schema": SCHEMA,
        "batch_id": manifest["batch_id"],
        "created_at": utcnow(),
        "mode": manifest["mode"],
        "mock_provider": manifest.get("mock_provider"),
        "binary_sha256": manifest["binary"]["sha256"],
        "currency": (manifest.get("price_snapshot") or {}).get("currency"),
        "arms": arms,
        "tasks": tasks,
        "repeats": manifest["repeats"],
        "runs": rows,
        "all": aggregate(rows),
        "arm_totals": {arm: agg(arm=arm) for arm in arms},
        "task_totals": {task: agg(task=task) for task in tasks},
        "cells": {arm: {task: agg(arm=arm, task=task) for task in tasks} for arm in arms},
    }
    path = batch_dir / "summary.json"
    write_json(path, summary)
    return path


def compare_rows(summary, other=None):
    """对比行。other 为 None：批次内，每个任务一臂一行，Δ 相对同任务的第一个臂
    （`*` 行是各臂在全部任务上的合计）；否则跨批次（旧 → 新），只在两边都有的
    task × arm 上比。"""
    if other is None:
        arms, tasks = summary["arms"], ["*"] + list(summary["tasks"])

        def cells(s, task, arm):
            return [s["arm_totals"][arm]] if task == "*" else [s["cells"][arm].get(task)]
    else:
        arms = [a for a in summary["arms"] if a in other["arms"]]
        tasks = ["*"] + [t for t in other["tasks"] if t in summary["tasks"]]

        def cells(s, task, arm):
            return [s["arm_totals"][arm]] if task == "*" else [s["cells"][arm].get(task)]

    rows = []
    for task in tasks:
        base = cells(summary, task, arms[0])[0] if arms else None
        for arm in arms:
            row = {"task": task, "arm": arm}
            mine = cells(summary, task, arm)[0]
            if other is None:
                if mine:
                    row["cell"] = mine
                if base and mine and arm != arms[0]:
                    row.update(deltas(base, mine))
            else:
                theirs = cells(other, task, arm)[0]
                if mine or theirs:
                    row.update(a=mine, b=theirs)
                if mine and theirs:
                    row.update(deltas(mine, theirs))
            rows.append(row)
    return rows


def deltas(old, new):
    change = new["total_tokens_mean"] - old["total_tokens_mean"]
    return {
        "d_pass": new["pass_rate"] - old["pass_rate"],
        "d_turns": new["turns_mean"] - old["turns_mean"],
        "d_tokens": change,
        "d_tokens_pct": change / old["total_tokens_mean"] if old["total_tokens_mean"] else None,
    }


def fmt_tokens(mean):
    if mean is None:
        return "-"
    return f"{mean / 1_000_000:.2f}M" if mean >= 1_000_000 else \
        f"{mean / 1000:.1f}k" if mean >= 100 else f"{mean:.0f}"


def fmt_money(value, currency):
    return "-" if value is None else f"{value:.4f} {currency or ''}".strip()


def fmt_pct(rate):
    return "-" if rate is None else f"{rate * 100:+.0f}%"


WITHIN_COLUMNS = (
    ("task", "<"), ("arm", "<"), ("runs", ">"), ("pass", ">"), ("turns", ">"),
    ("input", ">"), ("cache_r", ">"), ("cache_w", ">"), ("output", ">"), ("compr", ">"),
    ("calls", ">"), ("errs", ">"), ("cost", ">"), ("|", "<"), ("d_pass", ">"),
    ("d_turns", ">"), ("d_tokens", ">"),
)
CROSS_COLUMNS = (
    ("task", "<"), ("arm", "<"), ("pass A -> B", ">"), ("turns A -> B", ">"),
    ("tokens A -> B", ">"), ("compr A -> B", ">"), ("errs A -> B", ">"),
    ("cost A -> B", ">"), ("|", "<"), ("d_pass", ">"), ("d_turns", ">"), ("d_tokens", ">"),
)


def _table(columns, rows):
    """按列对齐打印。每列宽取表头与各行里最宽的那个：名字再长也不会错位；
    表头、分隔线与每一行等长。"""
    widths = [max([len(header)] + [len(row[i]) for row in rows])
              for i, (header, _) in enumerate(columns)]
    gutter = "  "

    def line(cells):
        return gutter.join(
            f"{cell:<{width}}" if align == "<" else f"{cell:>{width}}"
            for cell, (_, align), width in zip(cells, columns, widths)).rstrip()

    print(line([header for header, _ in columns]))
    print("-" * (sum(widths) + len(gutter) * (len(columns) - 1)))
    for row in rows:
        print(line(row))


def _blank(columns, task, arm, note):
    """一个没有任何 run 的行：只有说明，其余是「-」。"""
    row = [task, arm, note] + ["-"] * (len(columns) - 3)
    row[columns.index(("|", "<"))] = "|"
    return row


def print_rows(rows, currency, cross=False):
    """对比表。cross=False：批次内各臂（Δ 相对同任务的第一个臂）；cross=True：
    跨批次 A → B。d_* 一列的「-」= 基线臂自身，或没法算。"""
    if not cross:
        table = []
        for r in rows:
            if "cell" not in r:
                table.append(_blank(WITHIN_COLUMNS, r["task"], r["arm"], "(no runs)"))
                continue
            c, t = r["cell"], r["cell"]["tokens_mean"]
            table.append([
                r["task"], r["arm"], str(c["runs"]), f"{c['passed']}/{c['runs']}",
                f"{c['turns_mean']:.1f}", fmt_tokens(t["input_tokens"]),
                fmt_tokens(t["cache_read_input_tokens"]), fmt_tokens(t["cache_creation_input_tokens"]),
                fmt_tokens(t["output_tokens"]), f"{c['compressions_mean']:.1f}",
                f"{c['tool_calls_mean']:.1f}", f"{c['tool_errors_mean']:.1f}",
                fmt_money(c["cost_sum"], currency), "|",
                fmt_pct(r.get("d_pass")), "-" if "d_pass" not in r else f"{r['d_turns']:+.1f}",
                fmt_pct(r.get("d_tokens_pct"))])
        _table(WITHIN_COLUMNS, table)
        return
    table = []
    for r in rows:
        if "a" not in r:
            table.append(_blank(CROSS_COLUMNS, r["task"], r["arm"], "(no runs on both sides)"))
            continue
        a, b = r["a"], r["b"]
        if a is None or b is None:
            table.append(_blank(CROSS_COLUMNS, r["task"], r["arm"],
                                f"(no runs on side {'A' if a is None else 'B'})"))
            continue
        table.append([
            r["task"], r["arm"], f"{a['passed']}/{a['runs']} -> {b['passed']}/{b['runs']}",
            f"{a['turns_mean']:.1f} -> {b['turns_mean']:.1f}",
            f"{fmt_tokens(a['total_tokens_mean'])} -> {fmt_tokens(b['total_tokens_mean'])}",
            f"{a['compressions_mean']:.1f} -> {b['compressions_mean']:.1f}",
            f"{a['tool_errors_mean']:.1f} -> {b['tool_errors_mean']:.1f}",
            f"{fmt_money(a['cost_sum'], currency)} -> {fmt_money(b['cost_sum'], currency)}", "|",
            fmt_pct(r["d_pass"]), f"{r['d_turns']:+.1f}", fmt_pct(r["d_tokens_pct"])])
    _table(CROSS_COLUMNS, table)


def pass_drops(rows, max_drop):
    """超过容差的通过率下降：[(task, arm, drop)]。"""
    return [(r["task"], r["arm"], -r["d_pass"]) for r in rows
            if r.get("d_pass") is not None and r["d_pass"] < -max_drop - 1e-9]


# ── check（CI 门禁） ───────────────────────────────────────────────────


def check_config(task, provider):
    return build_config(mock_base_config(MOCK_PORT_MARK, provider), task, None, task["mock_config"])


def gate_probe(root, task_id, priced):
    """付费闸门探针：prepare 一个假付费批次，断言 run --execute 在启动任何 run
    之前被拒绝。配置指向 127.0.0.1:9、密钥变量从未设置——即使哪条闸门失守，
    也发不出一次付费请求。返回 (ok, detail)。"""
    config = {"provider": "openai", "api_key_env": FAKE_KEY_ENV, "model": "gate-probe",
              "base_url": "http://127.0.0.1:9", "tools": ["read_file"],
              "limits": {"max_turns": 2, "timeout_secs": 20}}
    (root / "gate-probe-config.json").write_text(json.dumps(config), encoding="utf-8")
    recipe = {"name": "gate-probe", "base_config": str(root / "gate-probe-config.json"),
              "tasks": [task_id], "arms": [{"name": "a", "config": {}}], "repeats": 1}
    (root / f"gate-probe-{int(priced)}.json").write_text(json.dumps(recipe), encoding="utf-8")
    price = {"currency": "USD", "input": 1e-6, "output": 2e-6,
             "cache_creation": 1.5e-6, "cache_read": 1e-7}
    batch, manifest = prepare_batch(root / f"gate-probe-{int(priced)}.json", root / "batches",
                                    price=price, cost_stop=1.0 if priced else None)
    if manifest["blocked_paid"] != (not priced):
        return False, f"blocked_paid should be {not priced}, got {manifest['blocked_paid']}"
    saved = os.environ.pop(FAKE_KEY_ENV, None)
    try:
        buffer = io.StringIO()
        run_batch(batch, execute=False, out=buffer)  # dry-run：只打印
        quiet = "nothing was written" in buffer.getvalue() and not (batch / "runs").exists()
        try:
            run_batch(batch, execute=True, out=io.StringIO())
            message = ""
        except RegressError as e:
            message = first_line(str(e))
    finally:
        if saved is not None:
            os.environ[FAKE_KEY_ENV] = saved
    want = "environment variable" if priced else "blocked_paid"
    if not quiet:
        return False, "the dry-run wrote something under runs/"
    if not message.startswith(want):
        return False, (f"run --execute was not refused by the {want!r} gate: "
                       f"{message or 'it started running'}")
    if (batch / "runs").exists():
        return False, "runs/ exists although the run was refused"
    return True, "refused before any run: " + message


def flow_probe(root):
    """mock-smoke 配方：零成本走一遍付费通道的同款代码路径。臂 b 把 max_turns 压到 1，
    必须被判卷判失败——证明流水线抓得住回归。"""
    batch, manifest = prepare_batch(RECIPES_DIR / "mock-smoke.json", root / "batches")
    result = run_batch(batch, execute=True, out=io.StringIO())
    if result["ran"] != 2 or result["stopped"]:
        return False, f"expected 2 runs, got {result['ran']} (stopped: {result['stopped']})"
    summary = read_json(batch / "summary.json", "summary")
    a = summary["cells"]["a"]["multi-file-edit"]
    b = summary["cells"]["b"]["multi-file-edit"]
    if not (a and a["passed"] == 1 and b and b["passed"] == 0):
        return False, (f"arm a {a and a['passed']}/{a and a['runs']} passed, arm b "
                       f"{b and b['passed']}/{b and b['runs']} passed")
    rows = compare_rows(summary)
    print_rows(rows, summary.get("currency"))
    drops = pass_drops(rows, 0.0)
    if drops != [("*", "b", 1.0), ("multi-file-edit", "b", 1.0)]:
        return False, f"compare should flag arm b on both rows, got {drops}"
    if b["stop_reasons"] != {"max_turns_reached": 1}:
        return False, f"arm b should stop on max_turns, got {b['stop_reasons']}"
    # 续跑：两个 run 都已有 verdict，再执行一次不该启动任何 run
    again = run_batch(batch, execute=True, out=io.StringIO())
    if again != {"done": 2, "ran": 0, "stopped": None}:
        return False, f"resume should skip both runs, got {again}"
    return True, (f"{manifest['batch_id']}: arm a passes, arm b (max_turns = 1) is caught; "
                  "a second --execute starts 0 runs")


def cmd_check(args):
    identity = binary_identity()
    print(f"binary: {BIN} (sha256 {identity['sha256'][:12]}, "
          f"version {json.dumps(identity['version'], ensure_ascii=False)})")
    failures = []

    def report(name, ok, detail=""):
        ok = bool(ok)
        print(f"[{'PASS' if ok else 'FAIL'}] {name}" + (f": {detail}" if detail else ""))
        if not ok:
            failures.append(name)
        return ok

    root = ART / f"check-{os.getpid()}"
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    runs = root / "runs"
    try:
        tasks = load_tasks()
        report("task set", True, f"{len(tasks)} tasks: " + ", ".join(t["id"] for t in tasks))
        # 1. 正路径：每个任务 × 两种线协议
        first = {}
        for task in tasks:
            for provider in CHECK_PROVIDERS:
                run_dir = runs / task["id"] / provider / "1"
                verdict = execute_run(task, provider, 1, run_dir, check_config(task, provider),
                                      mock_provider=provider)
                m = record_of(run_dir, {"mode": "mock"})["metrics"]
                report(f"good path: {task['id']} x {provider}", verdict["passed"],
                       f"{m['stop_reason']}, {m['turns']} turns, {m['compressions']} "
                       f"compression(s), {m['tool_calls']} tool call(s)"
                       + ("" if verdict["passed"] else " -- " + verdict["check_output"]))
                if provider == CHECK_PROVIDERS[0]:
                    first[task["id"]] = m
        # 2. 「什么都不做」的模型：run 自己是成功的，判卷必须不通过
        for task in tasks:
            run_dir = runs / task["id"] / "bad" / "1"
            verdict = execute_run(task, "bad", 1, run_dir, check_config(task, "openai"),
                                  mock_provider="openai", script=task["bad_script"])
            m = record_of(run_dir, {"mode": "mock"})["metrics"]
            report(f"bad model rejected: {task['id']}",
                   not verdict["passed"] and m["stop_reason"] == "end_turn",
                   f"the run itself ended {m['stop_reason']}; verdict: "
                   f"{first_line(verdict['check_output'])}")
        # 3. 可复现：同一任务再跑一次，metrics 逐项相等
        for task in tasks:
            run_dir = runs / task["id"] / CHECK_PROVIDERS[0] / "2"
            execute_run(task, CHECK_PROVIDERS[0], 2, run_dir, check_config(task, CHECK_PROVIDERS[0]),
                        mock_provider=CHECK_PROVIDERS[0])
            again = record_of(run_dir, {"mode": "mock"})["metrics"]
            diff = sorted(k for k in first[task["id"]] if first[task["id"]][k] != again[k])
            report(f"reproducible: {task['id']}", not diff,
                   "two runs, identical metrics" if not diff else "differs in: " + ", ".join(diff))
        # 4. 付费闸门
        report("paid gate: no price table and no cost stop -> --execute refused",
               *gate_probe(root, tasks[0]["id"], priced=False))
        report("paid gate: key variable absent -> --execute refused",
               *gate_probe(root, tasks[0]["id"], priced=True))
        # 5. mock-smoke 配方：prepare → run → summarize → compare
        report("mock-smoke recipe: prepare -> run -> summarize -> compare", *flow_probe(root))
    except RegressError as e:
        report("check aborted", False, str(e))
    except Exception:  # 评测脚本自己的 bug：报出来、留住现场，别只剩一段 traceback
        report("check crashed", False, "traceback saved to " + str(root / "crash.txt"))
        (root / "crash.txt").write_text(traceback.format_exc(), encoding="utf-8")
        print(traceback.format_exc(), file=sys.stderr)
    finally:
        if failures or args.keep:
            print(f"artifacts kept: {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)
    print()
    if failures:
        print(f"REGRESS CHECK FAILURES ({len(failures)}): " + ", ".join(failures))
        return 1
    print("REGRESS CHECK ALL PASS")
    return 0


def cmd_compare(args):
    if args.batch:
        if args.a or args.b:
            raise RegressError("compare: --batch cannot be combined with --a/--b")
        summary = read_json(Path(args.batch) / "summary.json", "summary")
        other = None
    elif args.a and args.b:
        summary = read_json(args.a, "summary A")
        other = read_json(args.b, "summary B")
    else:
        raise RegressError("compare: give --batch, or both --a and --b")
    rows = compare_rows(summary, other)
    if args.json:
        print(json.dumps(rows, ensure_ascii=False, indent=2))
    else:
        if other is None:
            print(f"== batch {summary['batch_id']} ({summary['mode']}"
                  + (f", {summary['mock_provider']} mock" if summary.get("mock_provider") else "")
                  + ") ==")
        else:
            print(f"== A: {summary['batch_id']}  ->  B: {other['batch_id']} ==")
        print_rows(rows, summary.get("currency"), cross=other is not None)
    drops = pass_drops(rows, args.max_pass_drop)
    side = "cell" if other is None else "b"  # --min-pass 看的是被考核的那一侧
    low = [(r["task"], r["arm"], r[side]) for r in rows
           if r.get(side) and r[side]["pass_rate"] < args.min_pass - 1e-9]
    failed = False
    if drops:
        failed = True
        print(f"pass rate dropped by more than {args.max_pass_drop:.0%}: "
              + ", ".join(f"{t}/{a} (-{d:.0%})" for t, a, d in drops))
    if low:
        failed = True
        print(f"pass rate below {args.min_pass:.0%}: "
              + ", ".join(f"{t}/{a} ({c['passed']}/{c['runs']})" for t, a, c in low))
    return 1 if failed else 0


# ── CLI ───────────────────────────────────────────────────────────────


def load_price(path):
    if not path:
        return None
    raw = read_json(path, f"price table {path}")
    ok, why = scorer.validate_price_snapshot(raw)
    if not ok:
        raise RegressError(f"price table {path} invalid: {why}")
    return raw


def cmd_prepare(args):
    price = load_price(args.price_table)
    if (price is None) != (args.cost_stop is None):
        raise RegressError("--price-table and --cost-stop go together")
    if args.mock and args.price_table:
        raise RegressError("--mock batches are free: no price table")
    batch, manifest = prepare_batch(args.recipe, ART, name=args.name, mock_provider=args.mock,
                                    price=price, cost_stop=args.cost_stop,
                                    base_config=args.base_config)
    print(f"batch: {batch}")
    print(f"mode: {manifest['mode']}"
          + (f" ({manifest['mock_provider']} mock)" if manifest["mock_provider"] else ""))
    print(f"runs: {len(manifest['schedule'])} ({len(manifest['tasks'])} task(s) x "
          f"{len(manifest['arms'])} arm(s) x {manifest['repeats']} repeat(s))")
    base = manifest["base_config"]
    if base:
        who = base["identity"]
        print(f"model: {who['provider']} {who['model']} via "
              f"{who['base_url'] or 'the provider default endpoint'}; key: ${who['api_key_env']}")
    if manifest["blocked_paid"]:
        print("blocked_paid: prepared without --price-table and --cost-stop; run --execute will "
              "be refused. To run the paid channel, re-prepare with both.")
    elif manifest["mode"] == "paid":
        print(f"cost stop: {manifest['cost_stop']} {price['currency']} "
              "(reserve-checked before every run)")
    print("next: python3 scripts/regress_eval.py run <batch>            # dry-run\n"
          "      python3 scripts/regress_eval.py run <batch> --execute  # runs (the paid channel costs money)")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="回归任务集评测（examples/regress/tasks）", formatter_class=argparse.RawTextHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("check", help="zero-cost self-check (CI gate)")
    p.add_argument("--keep", action="store_true",
                   help="keep the artifacts under artifacts/regress/check-<pid>/")
    p = sub.add_parser("prepare", help="freeze a recipe")
    p.add_argument("recipe")
    p.add_argument("--name", help="batch name (default: the recipe's)")
    p.add_argument("--mock", choices=sorted(MOCKS),
                   help="run a paid recipe's tasks against the scripted mock (free)")
    p.add_argument("--base-config",
                   help="paid channel: use this config file as the base config "
                        "instead of the one the recipe names")
    p.add_argument("--price-table",
                   help="price table JSON {currency, input, output, cache_creation, cache_read}, per token")
    p.add_argument("--cost-stop", type=float,
                   help="stop starting runs once this much currency is spent plus reserved")
    p = sub.add_parser("run", help="execute a frozen batch (dry-run by default)")
    p.add_argument("batch")
    p.add_argument("--execute", action="store_true",
                   help="really run (the paid channel costs money)")
    p.add_argument("--only", action="append", default=[], metavar="TASK",
                   help="only these task(s) (repeatable)")
    p = sub.add_parser("summarize", help="write summary.json for a batch")
    p.add_argument("batch")
    p = sub.add_parser("compare", help="arm comparison table")
    p.add_argument("--batch", help="compare the arms of one batch")
    p.add_argument("--a", help="summary.json of the old kernel")
    p.add_argument("--b", help="summary.json of the new kernel")
    p.add_argument("--json", action="store_true", help="machine-readable rows")
    p.add_argument("--max-pass-drop", type=float, default=0.0, metavar="X",
                   help="exit 1 if a pass rate drops by more than X against the baseline (0-1)")
    p.add_argument("--min-pass", type=float, default=0.0, metavar="X",
                   help="exit 1 if any cell's pass rate is below X (0-1)")
    args = parser.parse_args(argv)
    try:
        if args.command == "check":
            return cmd_check(args)
        if args.command == "prepare":
            return cmd_prepare(args)
        if args.command == "run":
            run_batch(args.batch, execute=args.execute, only=args.only or None)
            return 0
        if args.command == "summarize":
            print(summarize_batch(args.batch))
            return 0
        if args.command == "compare":
            return cmd_compare(args)
    except RegressError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
