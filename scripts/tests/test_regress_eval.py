#!/usr/bin/env python3
"""回归任务集评测（scripts/regress_eval.py）的离线单元测试。

不起被测二进制跑单个任务——那一层的全链路是 `python3 scripts/regress_eval.py
check`（CI 门禁）。这里钉住的是纯函数、闸门与判卷脚本本身：

  - 与内核的镜像关系：Clamp 分类与 Limits 默认值直接对着
    crates/core/src/config.rs 读，内核加了字段而这边没跟就红；
  - 配置装配：合并顺序（臂压过任务的 mock 调参）、片段只收紧、占位符与
    $file 展开、脱敏、预留量；
  - 任务集 / 配方 / 冻结 / 执行顺序（AB/BA 交替）的校验；
  - 付费闸门：缺价表或费用停止值、密钥变量缺席、冻结内容漂移，都在第一个
    run 之前拒绝；
  - 汇总与对比：未知费用不归零、通过率下降的判定、表格对齐；
  - 六个 check.py：没干活必判 FAIL、干了错活必判 FAIL、按脚本改对了必判 PASS；
    任务数据与期望答案自洽（long-output 的生成器、mcp-many-tools 的答案、
    cjk-docs 的参考答案）；
  - 最后把 `check` 整个跑一遍（被测二进制要先用
    `cargo build --locked --features code` 编出来，与 CI 的 e2e job 同款）。

运行：python3 -m pytest scripts/tests/test_regress_eval.py -q
"""
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "examples"))

import regress_eval as re_  # noqa: E402

CONFIG_RS = ROOT / "crates" / "core" / "src" / "config.rs"
TASK_IDS = ["cjk-docs", "long-output", "loop-suppression", "mcp-many-tools",
            "multi-file-edit", "recover-after-failure"]
RECIPE_NAMES = ["compression-handoff.json", "kernel-regress.json",
                "mock-smoke.json", "reasoning-carry.json", "summary-preview.json"]
PRICE = {"currency": "USD", "input": 1e-6, "output": 2e-6,
         "cache_creation": 1.5e-6, "cache_read": 1e-7}
# 一个从未设置的密钥变量名：付费闸门的测试靠它，发不出任何真实请求
NEVER_SET_KEY = "TINYAGENT_REGRESS_TEST_NEVER_SET"
NEEDS_BINARY = pytest.mark.skipif(not re_.BIN.is_file(), reason="target binary not built")


def load_module(name, path):
    """Import a task's check.py / setup.py under a private module name."""
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def task_by_id(task_id):
    return next(t for t in re_.load_tasks() if t["id"] == task_id)


def write_json(path, data):
    Path(path).write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
    return Path(path)


def child_env():
    return dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHONIOENCODING="utf-8")


def run_checker(task, workdir, result=None):
    """Run one task's check.py the way the harness does (cwd = workdir)."""
    result_path = write_json(Path(workdir) / "result.json", result or {
        "stop_reason": "end_turn", "success": True, "turns": 1, "tool_calls": []})
    return subprocess.run([sys.executable, str(task["check"]), str(result_path)],
                          cwd=str(workdir), capture_output=True, text=True,
                          encoding="utf-8", errors="replace", env=child_env(), timeout=180)


def in_workdir(command, workdir):
    return subprocess.run(command, cwd=str(workdir), capture_output=True, text=True,
                          encoding="utf-8", errors="replace", env=child_env(), timeout=180)


def pristine_workdir(task, tmp_path, name="workdir"):
    """A workdir as the harness lays it out before the agent runs."""
    workdir = tmp_path / name
    workdir.mkdir()
    if task["fixture"] is not None:
        shutil.copytree(task["fixture"], workdir, dirs_exist_ok=True)
    if task["setup"] is not None:
        proc = in_workdir([sys.executable, str(task["setup"]), str(workdir)], tmp_path)
        assert proc.returncode == 0, proc.stderr
    return workdir


def replay_edits(task, workdir):
    """Apply the edit_file calls of a task's scripted good path — the part of
    the script a checker can judge offline. A drifted old_string is an error:
    the script must still match its own fixture."""
    applied = 0
    for turn in task["script"]["turns"]:
        for call in turn.get("tools", []):
            if call["name"] != "edit_file":
                continue
            args = call["arguments"]
            path = workdir / args["path"]
            text = path.read_text(encoding="utf-8")
            assert text.count(args["old_string"]) == 1, f"{path}: the script's old_string drifted"
            path.write_text(text.replace(args["old_string"], args["new_string"]), encoding="utf-8")
            applied += 1
    return applied


def write_recipe(tmp_path, **overrides):
    recipe = {"name": "t", "mock": "openai", "arms": [{"name": "a", "config": {}}]}
    recipe.update(overrides)
    for key, value in list(recipe.items()):
        if value is None:
            del recipe[key]
    return write_json(tmp_path / "recipe.json", recipe)


# ── 与内核的镜像关系 ─────────────────────────────────────────────────────


def _without_comments(text):
    return re.sub(r"//[^\n]*", "", text)


def _rust_clamps():
    """field → Clamp kind, read off `Config::apply`
    (`li.<field> = Clamp::<Kind>.merge(li.<field>, …)`)."""
    text = _without_comments(CONFIG_RS.read_text(encoding="utf-8"))
    return dict(re.findall(r"li\.(\w+)\s*=\s*Clamp::(\w+)\.merge\(li\.\1\b", text))


def _rust_defaults():
    """field → default value, read off `impl Default for Limits` and the
    `default_*()` functions it calls."""
    text = _without_comments(CONFIG_RS.read_text(encoding="utf-8"))
    body = text[text.index("impl Default for Limits {"):]
    body = body[:body.index("\n}\n")]
    out = {}
    for field, init in re.findall(r"(\w+):\s*(default_\w+\(\)|\d+)", body):
        if init.endswith("()"):
            literal = re.search(r"fn " + init[:-2] + r"\(\) -> \w+\s*\{\s*([\d_]+)\s*\}", text)
            assert literal, f"cannot read the body of {init}"
            out[field] = int(literal.group(1).replace("_", ""))
        else:
            out[field] = int(init)
    return out


KIND = {"Tighten": "tighten", "TightenOrInit": "tighten_or_init", "Replace": "replace"}


def test_limit_clamps_mirror_the_kernel():
    rust = _rust_clamps()
    assert len(rust) == 11, "Config::apply should still clamp eleven limits fields"
    assert {k: KIND[v] for k, v in rust.items()} == re_.LIMIT_CLAMPS


def test_limit_defaults_mirror_the_kernel():
    assert re_.LIMIT_DEFAULTS == _rust_defaults()


@pytest.mark.parametrize("kind,base,patch,want", [
    # Tighten：v.min(base)；片段的 0 是最紧的值
    ("tighten", 30, 12, 12), ("tighten", 30, 99, 30), ("tighten", 30, 0, 0),
    # TightenOrInit：片段的 0 = 没给，沿用底配置
    ("tighten_or_init", 6000, 0, 6000),
    # 底配置 0 = 未设：采纳片段的有限值
    ("tighten_or_init", 0, 24000, 24000),
    ("tighten_or_init", 400_000, 24000, 24000), ("tighten_or_init", 6000, 24000, 6000),
    ("tighten_or_init", 0, 0, 0),
    # Replace：两个方向都照片段
    ("replace", 120, 30, 30), ("replace", 120, 900, 900),
])
def test_clamp_limit(kind, base, patch, want):
    assert re_.clamp_limit(kind, base, patch) == want


# ── 配置装配 ─────────────────────────────────────────────────────────────


def test_build_config_merges_in_order_and_clamps_the_fragment():
    task = task_by_id("long-output")
    assert task["fragment"]["limits"] == {"compress_threshold_chars": 24000, "max_turns": 60}
    base = re_.mock_base_config(re_.MOCK_PORT_MARK, "openai")
    arm = {"limits": {"keep_recent_messages": 0}, "max_tokens": 4096}
    config = re_.build_config(base, task, arm, task["mock_config"])
    assert config["task"] == task["fragment"]["task"]
    assert config["limits"] == {
        "max_turns": 60, "timeout_secs": 120, "max_output_chars": 30_000,
        "compress_threshold_chars": 6000, "keep_recent_messages": 0,
        "summary_preview_chars": 200,
        "max_context_tokens": 0, "max_total_tokens": 0, "stream_idle_secs": 30,
        "shell_timeout_secs": 120, "subagent_max_turns": 25}
    # 片段的 24000 只能收紧底配置的 6000，不能放宽
    assert config["max_tokens"] == 4096  # 臂
    assert config["tools"] == task["config"]["tools"]  # 任务
    assert config["provider"] == "openai" and config["api_key_env"] == re_.MOCK_KEY_ENV
    assert config["base_url"] == f"http://127.0.0.1:{re_.MOCK_PORT_MARK}"


def test_an_arm_beats_the_tasks_mock_tuning():
    """臂是实验变量：任务的 mock 专用调参不能把它改回去。"""
    task = task_by_id("long-output")
    assert task["mock_config"]["limits"]["keep_recent_messages"] == 4
    base = re_.mock_base_config(re_.MOCK_PORT_MARK, "openai")
    config = re_.build_config(base, task, {"limits": {"keep_recent_messages": 0}},
                              task["mock_config"])
    assert config["limits"]["keep_recent_messages"] == 0
    assert config["limits"]["compress_threshold_chars"] == 6000  # 臂没说的，mock 调参照旧


def test_build_config_with_a_loose_base_adopts_the_fragment_limit():
    task = task_by_id("long-output")
    base = re_.mock_base_config(re_.MOCK_PORT_MARK, "openai")
    base["limits"] = {}  # 默认 400_000 字符 / 30 轮
    config = re_.build_config(base, task, None, None)
    assert config["limits"]["compress_threshold_chars"] == 24000  # 片段生效
    assert config["limits"]["max_turns"] == 30  # 片段的 60 只能收紧到 30
    assert config["limits"]["max_context_tokens"] == 0  # 任务钉住：按字符阈值压缩


def test_build_config_expands_placeholders_and_files():
    base = re_.mock_base_config(re_.MOCK_PORT_MARK, "openai")
    config = re_.build_config(base, task_by_id("mcp-many-tools"), None, None)
    assert config["mcp_servers"] == [{"name": "regress", "command": sys.executable,
                                      "args": [str(re_.REGRESS / "mcp_server.py")]}]
    cjk = task_by_id("cjk-docs")
    script = re_.substitute(cjk["script"], cjk["dir"], True)
    written = [c for turn in script["turns"] for c in turn.get("tools", [])
               if c["name"] == "write_file"]
    assert len(written) == 2  # 另外三次 read_file
    assert all(isinstance(c["arguments"]["content"], str) and c["arguments"]["content"]
               for c in written)  # {"$file": …} 已经展开成文件内容
    assert "${" not in json.dumps(script, ensure_ascii=False)


def test_substitute_strictness():
    task = task_by_id("cjk-docs")
    assert re_.substitute("${HOME}/x", task["dir"], False) == "${HOME}/x"  # 底配置：原样保留
    with pytest.raises(re_.RegressError, match="unknown placeholder"):
        re_.substitute("${HOME}/x", task["dir"], True)  # 任务文件：拼错即错
    # 只展开值（字符串）：键原样保留
    assert re_.substitute({"${TASK_DIR}/x": "${REGRESS_DIR}"}, task["dir"], True) == {
        "${TASK_DIR}/x": str(re_.REGRESS)}
    assert re_.substitute(["run", "${PYTHON}", {"cwd": "${TASK_DIR}", "n": 3}], task["dir"],
                          True) == ["run", sys.executable, {"cwd": str(task["dir"]), "n": 3}]
    for escape in ("${TASK_DIR}/../mcp-many-tools/harness.json", "fixture", "../check.py",
                   "/etc/passwd", 5):  # 最后一个：类型不对也是可读的错误，不是 TypeError
        with pytest.raises(re_.RegressError, match="inside the task directory"):
            re_.substitute({"$file": escape}, task["dir"], True)
    # 一个对象里只有 $file 这一个键才展开；混着别的键原样保留
    assert re_.substitute({"$file": "check.py", "x": 1}, task["dir"], True) == {
        "$file": "check.py", "x": 1}


def test_redact_never_keeps_credentials():
    config = {"extra_headers": {"Authorization": "Bearer s3cret", "X-Tenant": "t"},
              "base_url": "https://gw.example/v1?api_key=url-secret-1#frag",
              "api_key_env": "MY_KEY", "mcp_servers": [
                  {"name": "s", "url": "https://m.example/mcp?token=url-secret-2",
                   "headers": {"Authorization": "header-secret-3"},
                   "env": {"API_TOKEN": "env-secret-4"}},
                  {"name": "stdio", "command": "npx", "args": ["-y", "x"]}]}
    shown = re_.redact(config)
    assert shown["extra_headers"] == {"Authorization": "<redacted>", "X-Tenant": "<redacted>"}
    assert shown["base_url"] == "https://gw.example/v1"
    assert shown["api_key_env"] == "MY_KEY"  # 变量名留着：run 的闸门要用
    server = shown["mcp_servers"][0]
    assert server["headers"] == {"Authorization": "<redacted>"}
    assert server["env"] == {"API_TOKEN": "<redacted>"}
    assert server["url"] == "https://m.example/mcp"
    assert shown["mcp_servers"][1] == config["mcp_servers"][1]
    dumped = json.dumps(shown)
    assert not [s for s in ("s3cret", "url-secret", "header-secret", "env-secret") if s in dumped]
    assert config["extra_headers"]["Authorization"] == "Bearer s3cret"  # 入参不动
    # mcp_servers 写成字典的形状同样脱敏
    shown = re_.redact({"mcp_servers": {
        "s": {"url": "https://m.example/mcp?token=url-secret-2", "env": {"API_TOKEN": "e-s"}}}})
    assert "url-secret" not in json.dumps(shown) and "e-s" not in json.dumps(shown)


def test_base_identity_holds_no_credential():
    identity = re_.base_identity({"provider": "openai", "model": "m", "api_key_env": "K",
                                  "base_url": "https://gw.example/v1?key=x",
                                  "extra_headers": {"Authorization": "Bearer y"},
                                  "limits": {"max_turns": 5}})
    assert identity == {"provider": "openai", "model": "m", "base_url": "https://gw.example/v1",
                        "api_key_env": "K", "limits": {"max_turns": 5}}
    assert re_.base_identity({"provider": "anthropic"}) == {
        "provider": "anthropic", "model": None, "base_url": None, "api_key_env": None,
        "limits": {}}


def test_reserve_tokens_and_cost():
    config = {"max_tokens": 1000, "limits": {"max_turns": 10, "max_context_tokens": 0,
                                             "compress_threshold_chars": 40_000}}
    # 窗口 = 40_000 / 4 = 10_000 token；每轮最坏 = 重读整个窗口 + 写满输出
    assert re_.reserve_tokens(config) == 10 * (1000 + 10_000)
    config["limits"]["max_context_tokens"] = 50_000  # 配了模型窗口就以它为准
    assert re_.reserve_tokens(config) == 10 * (1000 + 50_000)
    config["limits"]["max_total_tokens"] = 20_000  # 整个 run 的预算是硬上限
    assert re_.reserve_tokens(config) == 20_000
    # 两个阈值都没配：保守窗口；输出上限缺省 8192
    loose = {"limits": {"max_turns": 2, "max_context_tokens": 0, "compress_threshold_chars": 0}}
    assert re_.reserve_tokens(loose) == 2 * (8192 + re_.UNKNOWN_WINDOW_TOKENS)
    # 预留按四个单价里最贵的那个折算：宁可高估
    tiny = {"max_tokens": 1, "limits": {"max_turns": 2, "compress_threshold_chars": 4}}
    assert re_.reserve_tokens(tiny) == 4
    assert re_.reserve_cost(tiny, PRICE) == pytest.approx(4 * 2e-6)  # output 最贵
    assert re_.effective_limits({}) == re_.LIMIT_DEFAULTS
    assert re_.effective_limits({"limits": {"max_turns": None, "timeout_secs": 5}})[
        "timeout_secs"] == 5


def test_deep_merge_merges_dicts_and_replaces_lists():
    base = {"a": {"x": 1, "y": 2}, "l": [1, 2], "s": "b"}
    out = re_.deep_merge(base, {"a": {"y": 3, "z": 4}, "l": [9], "n": None})
    assert out == {"a": {"x": 1, "y": 3, "z": 4}, "l": [9], "s": "b", "n": None}
    assert base == {"a": {"x": 1, "y": 2}, "l": [1, 2], "s": "b"}
    assert re_.deep_merge({}, None) == {}


# ── 任务集 / 配方 / 冻结 ─────────────────────────────────────────────────


def test_task_set_loads_complete_and_sorted():
    tasks = re_.load_tasks()
    assert [t["id"] for t in tasks] == TASK_IDS
    assert len({t["category"] for t in tasks}) == 6
    for task in tasks:
        assert task["summary"] and task["fragment"]["task"].strip()
        assert task["script"]["turns"] and task["check"].is_file()
    assert [t["id"] for t in re_.load_tasks(["mcp-many-tools"])] == ["mcp-many-tools"]
    with pytest.raises(re_.RegressError, match="unknown task"):
        re_.load_tasks(["no-such-task"])


def test_fragment_limits_are_ints_within_the_clamp_table():
    for task in re_.load_tasks():
        for key, value in task["fragment"].get("limits", {}).items():
            assert key in re_.LIMIT_CLAMPS and isinstance(value, int) and value >= 0


def test_load_task_rejects_malformed_directories(tmp_path):
    good = task_by_id("multi-file-edit")
    base = tmp_path / "multi-file-edit"
    shutil.copytree(good["dir"], base)
    minimal = {"category": "c", "summary": "s", "script": {"turns": [{"text": "x"}]}}

    def broken(fragment=None, harness=None):
        write_json(base / "multi-file-edit.json",
                   good["fragment"] if fragment is None else fragment)
        write_json(base / "harness.json", minimal if harness is None else harness)
        return base

    assert re_.load_task(broken())["id"] == "multi-file-edit"
    with pytest.raises(re_.RegressError, match="may only carry task and limits"):
        re_.load_task(broken(fragment={"task": "t", "model": "oops"}))
    with pytest.raises(re_.RegressError, match="unknown limit"):
        re_.load_task(broken(fragment={"task": "t", "limits": {"wheels": 4}}))
    with pytest.raises(re_.RegressError, match="integer >= 0"):
        re_.load_task(broken(fragment={"task": "t", "limits": {"max_turns": -1}}))
    with pytest.raises(re_.RegressError, match="integer >= 0"):
        re_.load_task(broken(fragment={"task": "t", "limits": {"max_turns": True}}))
    with pytest.raises(re_.RegressError, match='non-empty "task"'):
        re_.load_task(broken(fragment={"task": "   "}))
    with pytest.raises(re_.RegressError, match="only tools, system_prompt"):
        re_.load_task(broken(harness=dict(minimal, config={"model": "oops"})))
    with pytest.raises(re_.RegressError, match="unknown key"):
        re_.load_task(broken(harness=dict(minimal, answer="x")))
    with pytest.raises(re_.RegressError, match='needs a "script"'):
        re_.load_task(broken(harness={"category": "c", "summary": "s"}))
    for bad in ({"turns": []}, {"turns": [{"usage": 1}]}, {"turns": [{"tools": []}]}):
        with pytest.raises(re_.RegressError, match="mock.bad_script"):
            re_.load_task(broken(harness=dict(minimal, mock={"bad_script": bad})))
    with pytest.raises(re_.RegressError, match="unknown key"):
        re_.load_task(broken(harness=dict(minimal, mock={"model": "x"})))
    (base / "check.py").unlink()
    with pytest.raises(re_.RegressError, match="check.py is missing"):
        re_.load_task(broken())


def test_committed_recipes_are_valid():
    assert sorted(p.name for p in re_.RECIPES_DIR.glob("*.json")) == RECIPE_NAMES
    recipes = {name: re_.load_recipe(re_.RECIPES_DIR / name) for name in RECIPE_NAMES}
    smoke = recipes["mock-smoke.json"]
    assert (smoke["mock"], smoke["base_config"], smoke["tasks"]) == (
        "openai", None, ["multi-file-edit"])
    assert [arm["config"] for arm in smoke["arms"]] == [{}, {"limits": {"max_turns": 1}}]
    # 付费配方：底配置来自使用者；至少三次重复（单次结果说明不了什么）
    paid = [r for name, r in recipes.items() if name != "mock-smoke.json"]
    assert len(paid) == 4
    assert all(r["base_config"] and r["mock"] is None and r["repeats"] >= 3 for r in paid)
    assert len({r["base_config"] for r in paid}) == 1
    regress = recipes["kernel-regress.json"]
    assert regress["tasks"] == TASK_IDS and len(regress["arms"]) == 1  # 没写 tasks = 全部
    arms = recipes["reasoning-carry.json"]["arms"]
    assert [arm["config"] for arm in arms] == [{"preserved_reasoning": False},
                                               {"preserved_reasoning": True}]
    assert "loop-suppression" not in recipes["reasoning-carry.json"]["tasks"]
    handoff = recipes["compression-handoff.json"]
    assert [arm["config"] for arm in handoff["arms"]] == [
        {}, {"limits": {"keep_recent_messages": 0}}]
    assert "loop-suppression" not in handoff["tasks"]
    # 摘要摘录宽度：两臂只差 summary_preview_chars，对照臂就是默认值
    preview = recipes["summary-preview.json"]
    assert [arm["config"] for arm in preview["arms"]] == [
        {}, {"limits": {"summary_preview_chars": 1000}}]
    assert "loop-suppression" not in preview["tasks"]
    # 对比配方的两臂必须真的不同，任务子集不能为空
    for recipe in (recipes["reasoning-carry.json"], recipes["compression-handoff.json"],
                   recipes["summary-preview.json"]):
        configs = {json.dumps(arm["config"], sort_keys=True) for arm in recipe["arms"]}
        assert len(configs) == len(recipe["arms"]) == 2 and recipe["tasks"]
    assert all(recipe["description"] for recipe in recipes.values())
    assert len({r["name"] for r in recipes.values()}) == len(recipes)


@pytest.mark.parametrize("overrides,message", [
    ({"base_config": "~/x.json"}, "exactly one of"),
    ({"mock": None}, "exactly one of"),
    ({"mock": "gemini"}, "must be one of"),
    ({"tasks": ["nope"]}, "unknown task"),
    ({"tasks": "multi-file-edit"}, "list of task ids"),
    ({"arms": []}, "non-empty list"),
    ({"arms": [{"name": "a"}, {"name": "a"}]}, "must be unique"),
    ({"arms": [{"name": "bad name"}]}, "needs a"),
    ({"arms": [{"name": "a", "config": []}]}, "must be an object"),
    ({"repeats": 0}, "integer >= 1"),
    ({"repeats": True}, "integer >= 1"),
    ({"name": "Bad Name"}, "lowercase"),
    ({"extra": 1}, "unknown key"),
])
def test_load_recipe_rejects_bad_recipes(tmp_path, overrides, message):
    with pytest.raises(re_.RegressError, match=message):
        re_.load_recipe(write_recipe(tmp_path, **overrides))


def test_schedule_alternates_arms_between_repeats():
    rows = re_.schedule_of(["a", "b"], ["t1", "t2"], 3)
    assert [(r["repeat"], r["task"], r["arm"]) for r in rows] == [
        (1, "t1", "a"), (1, "t1", "b"), (1, "t2", "a"), (1, "t2", "b"),
        (2, "t1", "b"), (2, "t1", "a"), (2, "t2", "b"), (2, "t2", "a"),
        (3, "t1", "a"), (3, "t1", "b"), (3, "t2", "a"), (3, "t2", "b")]
    assert [r["order"] for r in rows] == list(range(1, 13))


@NEEDS_BINARY
def test_prepare_freezes_and_run_refuses_on_drift(tmp_path, monkeypatch):
    recipe = write_recipe(tmp_path, tasks=["multi-file-edit"], repeats=2, base_config=None,
                          arms=[{"name": "a", "config": {}},
                                {"name": "b", "config": {"limits": {"max_turns": 1}}}])
    batch, manifest = re_.prepare_batch(recipe, tmp_path / "batches")
    assert manifest["schema"] == re_.SCHEMA and manifest["batch_id"].startswith("t-")
    assert manifest["mode"] == "mock" and manifest["blocked_paid"] is False
    assert (manifest["mock_provider"], manifest["base_config"]) == ("openai", None)
    assert manifest["price_snapshot"] is None and manifest["cost_stop"] is None
    assert len(manifest["schedule"]) == 4
    assert manifest["binary"]["sha256"] == re_.sha256_file(re_.BIN)
    assert manifest["git"]["head"], "the manifest should record the commit under test"
    assert set(manifest["harness"]) == set(re_.HARNESS_FILES)
    files = manifest["tasks"]["multi-file-edit"]["files"]
    assert files["multi-file-edit.json"] and files["harness.json"]
    assert {"check.py", "fixture/run_tests.py", "fixture/tests/test_calc.py"} <= set(files)
    frozen = manifest["configs"]["multi-file-edit"]
    assert frozen["a"]["workdir"] == re_.WORKDIR_MARK
    assert frozen["a"]["base_url"] == f"http://127.0.0.1:{re_.MOCK_PORT_MARK}"
    # 片段的 max_turns: 30 收紧了 mock 底配置的 60；臂 b 再收到 1
    assert (frozen["a"]["limits"]["max_turns"], frozen["b"]["limits"]["max_turns"]) == (30, 1)
    assert frozen["a"]["task"] == task_by_id("multi-file-edit")["fragment"]["task"]
    assert sorted(p.name for p in batch.iterdir()) == ["manifest.json"]
    pristine = json.loads((batch / "manifest.json").read_text(encoding="utf-8"))

    def put_back(mutate=None):
        """Put a (possibly tampered) copy of the frozen manifest back."""
        manifest = json.loads(json.dumps(pristine))
        if mutate:
            mutate(manifest)
        write_json(batch / "manifest.json", manifest)

    def harness(manifest):
        manifest["harness"]["scripts/regress_eval.py"] = "0" * 64

    def binary(manifest):
        manifest["binary"]["sha256"] = "0" * 64

    def task_files(manifest):
        manifest["tasks"]["multi-file-edit"]["files"]["check.py"] = "0" * 64

    # 冻结内容里的任何一项变了：run 拒绝（dry-run 也拒绝——清单说的必须是跑的）
    for mutate, why in ((harness, "scripts/regress_eval.py changed since prepare"),
                        (binary, "changed since prepare (rebuilt?"),
                        (task_files, "task files of multi-file-edit changed since prepare")):
        put_back(mutate)
        with pytest.raises(re_.RegressError, match=re.escape(why)):
            re_.run_batch(batch, execute=False, out=io.StringIO())
        assert not (batch / "runs").exists()
        put_back()  # 改回去：没动过的清单照常放行
        re_.run_batch(batch, execute=False, out=io.StringIO())
    # 同名批次目录已存在：拒绝覆盖
    monkeypatch.setattr(re_, "stamp", lambda: "20260101T000000000000Z")
    re_.prepare_batch(recipe, tmp_path / "batches")
    with pytest.raises(re_.RegressError, match="cannot create batch directory"):
        re_.prepare_batch(recipe, tmp_path / "batches")
    assert len(list((tmp_path / "batches").iterdir())) == 2


@NEEDS_BINARY
def test_dry_run_writes_nothing(tmp_path):
    batch, _ = re_.prepare_batch(write_recipe(tmp_path, tasks=["multi-file-edit"]),
                                 tmp_path / "batches")
    sink = io.StringIO()
    assert re_.run_batch(batch, execute=False, out=sink) == {
        "done": 0, "ran": 0, "stopped": None}
    text = sink.getvalue()
    assert "[dry-run]" in text and "nothing was written" in text
    assert text.count("repeat 1") == 1 and "pending" in text
    assert not (batch / "runs").exists()
    assert sorted(p.name for p in batch.iterdir()) == ["manifest.json"]


@NEEDS_BINARY
def test_a_relative_batch_path_still_runs(tmp_path, monkeypatch):
    """批次目录按相对路径给出（文档里的写法）也必须能跑：子进程的 cwd 是 run 目录，
    配置文件的相对路径在那里解析不到（2026-11-26 第一次真机批次六个 run 全部
    `cannot read config file`——零成本通道用的是绝对临时目录，从没走过这条路）。"""
    batch, _ = re_.prepare_batch(write_recipe(tmp_path, tasks=["multi-file-edit"]),
                                 tmp_path / "root" / "batches")
    monkeypatch.chdir(tmp_path / "root")
    relative = Path("batches") / batch.name
    assert not relative.is_absolute()
    sink = io.StringIO()
    assert re_.run_batch(relative, execute=False, out=sink)["done"] == 0
    assert re_.run_batch(relative, execute=True, out=sink) == {
        "done": 1, "ran": 1, "stopped": None}
    verdict = json.loads((batch / "runs" / "multi-file-edit" / "a" / "1" / "verdict.json")
                         .read_text(encoding="utf-8"))
    assert verdict["passed"], verdict["check_output"]


@NEEDS_BINARY
def test_a_relative_base_config_path_is_frozen_absolute(tmp_path, monkeypatch):
    """底配置按相对路径给出：清单里记绝对路径——换一个 cwd 再 run，封存校验仍指向同一份文件。"""
    (tmp_path / "cfg").mkdir()
    base = write_json(tmp_path / "cfg" / "base.json",
                      {"provider": "openai", "api_key_env": NEVER_SET_KEY, "model": "m",
                       "base_url": "http://127.0.0.1:9"})
    monkeypatch.chdir(tmp_path / "cfg")
    batch, manifest = re_.prepare_batch(re_.RECIPES_DIR / "kernel-regress.json",
                                        tmp_path / "batches", base_config="base.json")
    assert manifest["base_config"]["path"] == str(base)
    assert Path(manifest["recipe"]["base_config"]).is_absolute()
    monkeypatch.chdir(tmp_path)
    assert re_.run_batch(batch, execute=False, out=io.StringIO())["done"] == 0
    assert re_.run_batch(Path("batches") / batch.name, execute=False, out=io.StringIO())["done"] == 0


@NEEDS_BINARY
def test_run_refuses_when_the_frozen_config_no_longer_matches(tmp_path):
    batch, _ = re_.prepare_batch(write_recipe(tmp_path, tasks=["multi-file-edit"]),
                                 tmp_path / "batches")
    manifest = json.loads((batch / "manifest.json").read_text(encoding="utf-8"))
    manifest["configs"]["multi-file-edit"]["a"]["max_tokens"] = 1
    write_json(batch / "manifest.json", manifest)
    with pytest.raises(re_.RegressError, match="does not match the frozen manifest"):
        re_.run_batch(batch, execute=True, out=io.StringIO())
    assert not (batch / "runs").exists()  # 拒绝在铺开任何 run 目录之前


@NEEDS_BINARY
def test_execute_run_refuses_a_mock_config_without_a_mock(tmp_path):
    task = task_by_id("multi-file-edit")
    config = re_.build_config(re_.mock_base_config(re_.MOCK_PORT_MARK, "openai"), task, None, None)
    config["workdir"] = str(tmp_path / "workdir")
    with pytest.raises(re_.RegressError, match="without a mock"):
        re_.execute_run(task, "a", 1, tmp_path / "run", config)
    assert not (tmp_path / "run").exists()


@NEEDS_BINARY
def test_prepare_base_config_override_and_mock_on_a_paid_recipe(tmp_path):
    config = {"provider": "anthropic", "api_key_env": "SOME_KEY", "model": "m",
              "base_url": "https://gw.example/v1?k=1"}
    base = write_json(tmp_path / "elsewhere.json", config)
    recipe = re_.RECIPES_DIR / "kernel-regress.json"
    batch, manifest = re_.prepare_batch(recipe, tmp_path / "batches", base_config=base)
    assert (manifest["mode"], manifest["blocked_paid"]) == ("paid", True)
    assert manifest["base_config"]["path"] == str(base)
    assert manifest["base_config"]["sha256"] == re_.sha256_file(base)
    assert manifest["base_config"]["identity"] == {
        "provider": "anthropic", "model": "m", "base_url": "https://gw.example/v1",
        "api_key_env": "SOME_KEY", "limits": {}}
    assert manifest["recipe"]["base_config"] == str(base)  # 清单记的是实际用的那一份
    assert manifest["configs"]["cjk-docs"]["baseline"]["provider"] == "anthropic"
    assert len(manifest["schedule"]) == len(TASK_IDS) * 3
    # 底配置在 prepare 之后被改过：run 拒绝
    write_json(base, dict(config, api_key_env="OTHER_KEY"))
    with pytest.raises(re_.RegressError, match="base config .* changed since prepare"):
        re_.run_batch(batch, execute=False, out=io.StringIO())
    write_json(base, config)
    # 付费配方 + --mock：同一批任务零成本走一遍
    batch, manifest = re_.prepare_batch(recipe, tmp_path / "batches", mock_provider="anthropic")
    assert (manifest["mode"], manifest["mock_provider"], manifest["blocked_paid"]) == (
        "mock", "anthropic", False)
    assert manifest["base_config"] is None and manifest["recipe"]["mock"] == "anthropic"
    assert manifest["configs"]["cjk-docs"]["baseline"]["provider"] == "anthropic"
    with pytest.raises(re_.RegressError, match="not both"):
        re_.prepare_batch(recipe, tmp_path / "batches", mock_provider="openai",
                          base_config=base)
    with pytest.raises(re_.RegressError, match="already a mock recipe"):
        re_.prepare_batch(re_.RECIPES_DIR / "mock-smoke.json", tmp_path / "batches",
                          mock_provider="openai")
    with pytest.raises(re_.RegressError, match="generates its own base config"):
        re_.prepare_batch(re_.RECIPES_DIR / "mock-smoke.json", tmp_path / "batches",
                          base_config=base)


def test_move_aside_never_overwrites(tmp_path):
    run = tmp_path / "runs" / "t" / "a" / "1"
    run.mkdir(parents=True)
    (run / "events.ndjson").write_text("x", encoding="utf-8")
    re_.move_aside(run)
    kept = sorted((tmp_path / "runs" / "t" / "a").iterdir())
    assert len(kept) == 1 and kept[0].name.startswith("1.incomplete-")
    assert (kept[0] / "events.ndjson").read_text(encoding="utf-8") == "x"
    assert not run.exists()


# ── run 的环境 ───────────────────────────────────────────────────────────


def test_every_run_gets_its_own_config_dir(tmp_path):
    """两条通道都不继承开发者本机的配置目录：`[skill]` 的全局查找跟着它走，否则本机真实的
    技能目录会漏进轨迹（2026-11-26 第一次真机批次：付费 run 的 run.log 里出现了开发者本机
    技能目录的告警，技能表也进了 system prompt）。"""
    run_dir = tmp_path / "runs" / "t" / "a" / "1"
    run_dir.mkdir(parents=True)
    env = re_.run_env(run_dir)
    assert env["XDG_CONFIG_HOME"] == str(run_dir / ".xdg")
    assert (run_dir / ".xdg").is_dir() and not any((run_dir / ".xdg").iterdir())
    assert env["PYTHONDONTWRITEBYTECODE"] == "1"


@pytest.mark.skipif(shutil.which("git") is None, reason="git not installed")
def test_git_commands_in_a_run_never_reach_the_outer_repo(tmp_path):
    """批次目录默认在仓库里（artifacts/regress/<批次>/runs/…），agent 在 workdir 里发的 git
    命令不能解析到外层仓库。2026-11-26 第一次真机批次，模型跑了 `git status --short; git diff`，
    外层仓库三万字符的未提交 diff 被发给了模型——换成 `git stash` / `git reset --hard` 就是
    直接改坏开发者的工作区。"""
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q", str(repo)], check=True)
    (repo / "wip.txt").write_text("uncommitted work\n", encoding="utf-8")
    run_dir = repo / "artifacts" / "regress" / "b" / "runs" / "t" / "a" / "1"
    workdir = run_dir / "workdir"
    workdir.mkdir(parents=True)

    def toplevel(cwd, env=None):
        proc = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=str(cwd), env=env,
                              capture_output=True, text=True)
        return proc.returncode, proc.stdout.strip()

    assert toplevel(workdir) == (0, str(repo.resolve()))  # 不设防：看得到外层仓库
    env = re_.run_env(run_dir)
    for cwd in (workdir, run_dir, run_dir / "..", run_dir / ".." / ".." / ".."):
        assert toplevel(cwd, env)[0] != 0, cwd
    # workdir 自己是仓库时照常发现
    subprocess.run(["git", "init", "-q", str(workdir)], check=True, env=env)
    assert toplevel(workdir, env) == (0, str(workdir.resolve()))


@NEEDS_BINARY
def test_a_run_inside_a_repository_cannot_see_it(tmp_path, monkeypatch):
    """同上，走真实的被测二进制：脚本化的第一轮先在 workdir 里问 git 仓库在哪。"""
    load = re_.load_tasks
    probe = {"tools": [{"name": "shell", "arguments": {
        "command": "git rev-parse --show-toplevel; echo rc=$?"}}]}

    def probing(only=None):
        tasks = load(only)
        for task in tasks:
            if task["id"] == "multi-file-edit":
                task["script"]["turns"].insert(0, probe)
        return tasks

    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q", str(repo)], check=True)
    monkeypatch.setattr(re_, "load_tasks", probing)
    batch, _ = re_.prepare_batch(write_recipe(repo, tasks=["multi-file-edit"]), repo / "batches")
    assert re_.run_batch(batch, execute=True, out=io.StringIO()) == {
        "done": 1, "ran": 1, "stopped": None}
    result = json.loads((batch / "runs" / "multi-file-edit" / "a" / "1" / "result.json")
                        .read_text(encoding="utf-8"))
    out = result["tool_calls"][0]["output"]
    assert "rc=128" in out and str(repo) not in out


# ── 付费闸门 ─────────────────────────────────────────────────────────────


def paid_manifest(**overrides):
    config = {"provider": "openai", "api_key_env": NEVER_SET_KEY, "model": "m",
              "base_url": "http://127.0.0.1:9", "tools": ["read_file"]}
    manifest = {"mode": "paid", "blocked_paid": False, "price_snapshot": PRICE,
                "cost_stop": 1.0, "configs": {"t": {"a": config, "b": dict(config)}}}
    manifest.update(overrides)
    return manifest


def test_paid_gates_refuse_one_by_one(monkeypatch):
    monkeypatch.delenv(NEVER_SET_KEY, raising=False)
    manifest = paid_manifest()
    with pytest.raises(re_.RegressError, match=f"environment variable {NEVER_SET_KEY}"):
        re_.paid_gates(manifest)
    monkeypatch.setenv(NEVER_SET_KEY, "a-value-the-harness-must-never-read")
    assert re_.paid_gates(manifest) == PRICE  # 只看变量在不在
    monkeypatch.delenv(NEVER_SET_KEY)
    for override, message in (
            ({"blocked_paid": True}, "^blocked_paid"),
            ({"price_snapshot": None}, "price table in the manifest is invalid"),
            ({"price_snapshot": dict(PRICE, input=-1)}, "price table in the manifest is invalid"),
            ({"cost_stop": None}, "has no cost stop"),
            ({"cost_stop": 0}, "has no cost stop"),
            # bool 不是数；inf 等于没有上限
            ({"cost_stop": True}, "has no cost stop"),
            ({"cost_stop": float("inf")}, "has no cost stop")):
        with pytest.raises(re_.RegressError, match=message):
            re_.paid_gates(paid_manifest(**override))
    # 每个臂的配置都要有 api_key_env
    monkeypatch.setenv(NEVER_SET_KEY, "present")  # 第一个臂放行，才轮得到下一个臂被查
    for arm in ("a", "b"):
        manifest = paid_manifest()
        manifest["configs"]["t"][arm] = {"tools": ["read_file"]}
        with pytest.raises(re_.RegressError, match="no api_key_env"):
            re_.paid_gates(manifest)
    # 任何一个臂的密钥变量缺席都拒绝（第二个臂用的是别的变量）
    manifest = paid_manifest()
    manifest["configs"]["t"]["b"]["api_key_env"] = NEVER_SET_KEY + "_2"
    with pytest.raises(re_.RegressError, match=NEVER_SET_KEY + "_2"):
        re_.paid_gates(manifest)


def test_charged_so_far_counts_unknown_usage():
    """已花 = 已知下界 + 未知用量的预留。用量报不全的 run 不能只记下界。"""
    unit = 2e-6  # PRICE 里最贵的单价（output）
    assert re_.max_unit_price(PRICE) == unit
    assert re_.charged_so_far({"cost_known_lower": 0.00125, "unknown_reserve_tokens": 0},
                              PRICE) == pytest.approx(0.00125)
    # 请求发生过、一个 token 都没报：按整 run 预留
    missing = {"cost_known_lower": 0.0,
               "unknown_reserve_tokens": re_.scorer.UNKNOWN_RESERVE_TOKENS}
    assert re_.charged_so_far(missing, PRICE) == pytest.approx(
        re_.scorer.UNKNOWN_RESERVE_TOKENS * unit)
    # 两个 turn 一个没报账：下界 + 一个请求的预留
    partial = {"cost_known_lower": 0.001,
               "unknown_reserve_tokens": re_.scorer.PARTIAL_UNKNOWN_PER_REQUEST_TOKENS}
    assert re_.charged_so_far(partial, PRICE) == pytest.approx(
        0.001 + re_.scorer.PARTIAL_UNKNOWN_PER_REQUEST_TOKENS * unit)
    assert re_.charged_so_far({"cost_known_lower": None, "unknown_reserve_tokens": 0},
                              PRICE) == 0.0


def fake_paid(tmp_path, **kwargs):
    """A paid batch whose base config points at 127.0.0.1:9 with a key variable
    that is never set — nothing it could accidentally reach."""
    config = {"provider": "openai", "api_key_env": NEVER_SET_KEY, "model": "m",
              "base_url": "http://127.0.0.1:9", "tools": ["read_file"]}
    base = write_json(tmp_path / "base.json", config)
    recipe = write_recipe(tmp_path, name="paid-t", base_config=str(base), mock=None,
                          tasks=["multi-file-edit"])
    return re_.prepare_batch(recipe, tmp_path / "batches", **kwargs)


@NEEDS_BINARY
def test_paid_batch_without_price_or_cost_stop_is_blocked(tmp_path, monkeypatch):
    monkeypatch.delenv(NEVER_SET_KEY, raising=False)
    batch, manifest = fake_paid(tmp_path)
    assert manifest["blocked_paid"] is True
    assert manifest["price_snapshot"] is None and manifest["cost_stop"] is None
    assert manifest["base_config"]["identity"]["api_key_env"] == NEVER_SET_KEY
    with pytest.raises(re_.RegressError, match="^blocked_paid"):
        re_.run_batch(batch, execute=True, out=io.StringIO())
    assert not (batch / "runs").exists()
    # 即使密钥变量在环境里、价表事后被塞进清单：没有费用停止值照样不放行
    manifest["blocked_paid"] = False
    manifest["price_snapshot"] = PRICE
    write_json(batch / "manifest.json", manifest)
    monkeypatch.setenv(NEVER_SET_KEY, "present")
    with pytest.raises(re_.RegressError, match="has no cost stop"):
        re_.run_batch(batch, execute=True, out=io.StringIO())
    assert not (batch / "runs").exists()


@NEEDS_BINARY
def test_paid_batch_with_key_absent_is_refused_before_any_run(tmp_path, monkeypatch):
    monkeypatch.delenv(NEVER_SET_KEY, raising=False)
    batch, manifest = fake_paid(tmp_path, price=PRICE, cost_stop=1.0)
    assert (manifest["blocked_paid"], manifest["price_snapshot"], manifest["cost_stop"]) == (
        False, PRICE, 1.0)
    with pytest.raises(re_.RegressError, match=f"environment variable {NEVER_SET_KEY}"):
        re_.run_batch(batch, execute=True, out=io.StringIO())
    assert not (batch / "runs").exists()
    # dry-run 不需要密钥：照常打印计划
    sink = io.StringIO()
    re_.run_batch(batch, execute=False, out=sink)
    assert "cost stop: 1.0 USD (reserve-checked before every run)" in sink.getvalue()
    assert not (batch / "runs").exists()
    # --only 选不到任何 run：拒绝
    with pytest.raises(re_.RegressError, match="nothing to run"):
        re_.run_batch(batch, execute=False, only=["cjk-docs"], out=io.StringIO())


@NEEDS_BINARY
def test_cost_stop_counts_usage_that_was_never_reported(tmp_path, monkeypatch):
    """用量报不全的 run 按预留记进已花。端点是 127.0.0.1:9（没人监听）：连接被拒的
    run 一个 token 都没报，已知费用是 0——停止值不能因此放行下一个 run。"""
    monkeypatch.setenv(NEVER_SET_KEY, "dummy-for-a-refused-connection")
    base = write_json(tmp_path / "base.json", {
        "provider": "openai", "api_key_env": NEVER_SET_KEY, "model": "m",
        "base_url": "http://127.0.0.1:9", "tools": ["read_file"]})
    arm = {"max_tokens": 64, "limits": {"max_turns": 2, "compress_threshold_chars": 4000}}
    recipe = write_recipe(tmp_path, name="paid-t", base_config=str(base), mock=None,
                          tasks=["multi-file-edit"], repeats=2, arms=[{"name": "a", "config": arm}])
    batch, manifest = re_.prepare_batch(recipe, tmp_path / "batches", price=PRICE, cost_stop=0.1)
    reserve = re_.reserve_cost(manifest["configs"]["multi-file-edit"]["a"], PRICE)
    assert reserve < 0.1  # 起跑前的预留顶不到停止值：第一个 run 放行
    sink = io.StringIO()
    assert re_.run_batch(batch, execute=True, out=sink) == {
        "done": 1, "ran": 1, "stopped": "cost stop 0.1 USD: spent 0.2255"}
    text = sink.getvalue()
    assert text.count("[run]") == 1 and "FAIL (exit 1, error, 1 turns" in text
    assert "[stopped] cost stop 0.1 USD: spent 0.2255; no further run was started" in text
    assert not (batch / "runs" / "multi-file-edit" / "a" / "2").exists()
    summary = json.loads((batch / "summary.json").read_text(encoding="utf-8"))
    (run,) = summary["runs"]
    assert run["passed"] is False and run["metrics"]["turns"] == 1
    assert (run["cost"], run["cost_known_lower"], run["cost_complete"]) == (None, 0.0, False)
    assert run["unknown_reserve_tokens"] == re_.scorer.UNKNOWN_RESERVE_TOKENS
    assert re_.charged_so_far(run, PRICE) == pytest.approx(0.225536)
    assert summary["all"]["cost_known_lower_sum"] == 0.0  # 已知的部分照实是 0
    # 续跑：第一个 run 已有结论、跳过；第二个照旧起不来
    sink = io.StringIO()
    assert re_.run_batch(batch, execute=True, out=sink) == {
        "done": 1, "ran": 0, "stopped": "cost stop 0.1 USD: spent 0.2255 + reserve 0.0043 "
                                        "would reach it"}
    assert sink.getvalue().count("[skip]") == 1


def test_price_table_must_be_valid(tmp_path):
    base = write_json(tmp_path / "base.json", {"provider": "openai", "api_key_env": "X"})
    recipe = write_recipe(tmp_path, name="p", base_config=str(base), mock=None)
    for price in (dict(PRICE, input=-1), dict(PRICE, input=float("nan")),
                  dict(PRICE, currency=""), dict(PRICE, cache_read=True),
                  {k: v for k, v in PRICE.items() if k != "cache_read"}):
        with pytest.raises(re_.RegressError, match="price table invalid"):
            re_.prepare_batch(recipe, tmp_path / "b", price=price, cost_stop=1.0)
    for stop in (0, -1, True, "5", float("inf"), float("nan")):
        with pytest.raises(re_.RegressError, match="positive number"):
            re_.prepare_batch(recipe, tmp_path / "b", price=PRICE, cost_stop=stop)
    assert not (tmp_path / "b").exists()  # 拒绝都发生在建目录之前


@NEEDS_BINARY
def test_prepare_pairs_the_price_inputs(tmp_path, monkeypatch):
    monkeypatch.setattr(re_, "ART", tmp_path / "art")
    prices = str(tmp_path / "prices.json")
    write_json(prices, PRICE)
    args = SimpleNamespace(recipe=write_recipe(tmp_path), name=None, mock=None,
                           base_config=None, price_table=None, cost_stop=1.0)
    with pytest.raises(re_.RegressError, match="go together"):
        re_.cmd_prepare(args)  # 只给了费用停止值
    args.price_table, args.cost_stop = prices, None
    with pytest.raises(re_.RegressError, match="go together"):
        re_.cmd_prepare(args)  # 只给了价表
    args.price_table = None
    assert re_.cmd_prepare(args) == 0  # mock 配方、什么都不给：合法
    assert len(list((tmp_path / "art").iterdir())) == 1
    args.mock, args.price_table, args.cost_stop = "anthropic", prices, 1.0
    with pytest.raises(re_.RegressError, match="no price table"):
        re_.cmd_prepare(args)  # mock 批次不花钱：不接受价表
    assert len(list((tmp_path / "art").iterdir())) == 1


# ── 汇总 / 对比 ──────────────────────────────────────────────────────────


def make_run(root, task, arm, repeat, *, passed=True, result=None, events=None):
    """A finished run directory with the three files the summary reads."""
    run_dir = root / "runs" / task / arm / str(repeat)
    run_dir.mkdir(parents=True)
    data = {"stop_reason": "end_turn", "success": True, "turns": 3, "attempts": 1,
            "input_tokens": 1000, "cache_read_input_tokens": 500,
            "cache_creation_input_tokens": 0, "output_tokens": 100,
            "reasoning_tokens": 20, "elapsed_secs": 1.5, "tool_calls": [
                {"name": "shell", "is_error": False, "duration_ms": 12},
                {"name": "read_file", "is_error": True}]}
    data.update(result or {})
    write_json(run_dir / "result.json", data)
    (run_dir / "events.ndjson").write_text(
        events if events is not None else
        '{"type":"turn"}\n{"type":"usage","input_tokens":1000}\n', encoding="utf-8")
    write_json(run_dir / "verdict.json", {
        "task": task, "arm": arm, "repeat": repeat, "passed": passed,
        "exit_code": 0 if passed else 1, "check_output": "" if passed else "FAIL: x"})
    return run_dir


def make_batch(tmp_path, mode="paid", price=PRICE):
    batch = tmp_path / "b"
    batch.mkdir()
    write_json(batch / "manifest.json", {
        "schema": re_.SCHEMA, "batch_id": "b-1", "mode": mode, "mock_provider": None,
        "arms": [{"name": "a", "label": "a", "config": {}},
                 {"name": "b", "label": "b", "config": {}}],
        "repeats": 2, "price_snapshot": price if mode == "paid" else None,
        "binary": {"sha256": "f" * 64}, "tasks": {"t1": {}, "t2": {}},
        "schedule": re_.schedule_of(["a", "b"], ["t1", "t2"], 2)})
    return batch


def test_record_separates_reproducible_metrics_from_timing(tmp_path):
    row = re_.record_of(make_run(tmp_path, "t1", "a", 1), {"mode": "mock"})
    assert (row["task"], row["arm"], row["repeat"], row["passed"], row["exit_code"]) == (
        "t1", "a", 1, True, 0)
    assert row["metrics"] == {
        "passed": True, "stop_reason": "end_turn", "turns": 3,
        "tokens": {"input_tokens": 1000, "cache_read_input_tokens": 500,
                   "cache_creation_input_tokens": 0, "output_tokens": 100, "reasoning_tokens": 20},
        "total_tokens": 1600, "compressions": 0, "tool_calls": 2, "tool_errors": 1}
    assert row["timing"] == {"elapsed_ms": 1500, "tool_ms": [12]}
    assert (row["cost"], row["cost_known_lower"], row["cost_complete"]) == (None, None, False)
    assert row["check_output"] == ""  # 通过时不占地方
    row = re_.record_of(make_run(tmp_path, "t1", "a", 2, passed=False), {"mode": "mock"})
    assert row["passed"] is False and row["check_output"] == "FAIL: x"


def test_record_prices_a_complete_run_and_never_zeros_unknown_usage(tmp_path):
    manifest = {"mode": "paid", "price_snapshot": PRICE}
    known = 1000e-6 + 500e-7 + 100 * 2e-6  # input × 1e-6 + cache_read × 1e-7 + output × 2e-6
    row = re_.record_of(make_run(tmp_path, "t1", "a", 1), manifest)
    assert row["cost"] == pytest.approx(known)  # input 不含缓存读，各计一次
    assert row["cost_known_lower"] == row["cost"] and row["cost_complete"] is True
    assert row["unknown_reserve_tokens"] == 0
    # 两个 turn 只有一个报了 usage：那一轮烧了多少不可知 → 总费用 None，下界照给
    row = re_.record_of(make_run(tmp_path, "t1", "a", 2, events=(
        '{"type":"turn"}\n{"type":"usage"}\n{"type":"turn"}\n')), manifest)
    assert (row["cost"], row["cost_complete"]) == (None, False)
    assert row["cost_known_lower"] == pytest.approx(known)
    assert row["unknown_reserve_tokens"] == re_.scorer.PARTIAL_UNKNOWN_PER_REQUEST_TOKENS
    # 请求发生过但一个 token 都没报：整个 run 按口径预留
    row = re_.record_of(make_run(tmp_path, "t1", "a", 3, events="", result={
        "input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 0}), manifest)
    assert (row["cost"], row["cost_known_lower"], row["cost_complete"]) == (None, 0.0, False)
    assert row["unknown_reserve_tokens"] == re_.scorer.UNKNOWN_RESERVE_TOKENS
    # result.json 整个缺失：events 是唯一账本——不归零
    run_dir = make_run(tmp_path, "t1", "a", 4)
    (run_dir / "result.json").unlink()
    row = re_.record_of(run_dir, manifest)
    assert row["metrics"]["tokens"]["input_tokens"] == 1000  # events 里报了的账
    assert (row["metrics"]["turns"], row["metrics"]["stop_reason"]) == (1, None)
    assert row["timing"] == {"elapsed_ms": 0, "tool_ms": []}
    assert row["cost"] == pytest.approx(1000e-6) and row["cost_complete"] is True
    # result.json 损坏：同上
    run_dir = make_run(tmp_path, "t1", "a", 5)
    (run_dir / "result.json").write_text("{truncated", encoding="utf-8")
    assert re_.record_of(run_dir, manifest)["metrics"]["tokens"]["input_tokens"] == 1000


def test_aggregate_reports_unknown_cost_as_unknown(tmp_path):
    batch = make_batch(tmp_path)
    make_run(batch, "t1", "a", 1)
    make_run(batch, "t1", "a", 2, events='{"type":"turn"}\n{"type":"usage"}\n{"type":"turn"}\n')
    manifest = json.loads((batch / "manifest.json").read_text(encoding="utf-8"))
    rows = re_.collect_runs(batch, manifest)
    assert [r["order"] for r in rows] == [1, 6]  # 第二轮里臂序反过来，a 排在 b 之后
    cell = re_.aggregate(rows)
    assert (cell["runs"], cell["passed"], cell["pass_rate"]) == (2, 2, 1.0)
    assert cell["cost_sum"] is None  # 有一个 run 用量不完整：总费用不可判
    assert cell["cost_known_lower_sum"] == pytest.approx(2 * (1000e-6 + 500e-7 + 200e-6))
    assert cell["stop_reasons"] == {"end_turn": 2}
    assert cell["tokens_mean"]["input_tokens"] == 1000 and cell["turns_mean"] == 3.0
    assert cell["tool_calls_mean"] == 2.0 and cell["tool_errors_mean"] == 1.0
    assert cell["compressions_mean"] == 0.0 and re_.aggregate([]) is None


def test_summary_and_comparison_flag_a_pass_rate_drop(tmp_path):
    batch = make_batch(tmp_path)
    make_run(batch, "t1", "a", 1)
    make_run(batch, "t1", "a", 2)
    make_run(batch, "t1", "b", 1)
    make_run(batch, "t1", "b", 2, passed=False, result={
        "stop_reason": "stuck_loop", "turns": 7, "input_tokens": 4000})
    make_run(batch, "t2", "a", 1, result={"turns": 5})
    assert re_.summarize_batch(batch) == batch / "summary.json"
    summary = json.loads((batch / "summary.json").read_text(encoding="utf-8"))
    assert (summary["all"]["runs"], summary["all"]["passed"]) == (5, 4)
    assert summary["currency"] == "USD" and summary["arms"] == ["a", "b"]
    assert summary["cells"]["b"]["t1"]["passed"] == 1
    assert summary["cells"]["b"]["t1"]["stop_reasons"] == {"end_turn": 1, "stuck_loop": 1}
    assert summary["cells"]["b"]["t2"] is None  # 一个 run 都没跑
    assert summary["cells"]["a"]["t1"]["turns_mean"] == 3.0
    assert summary["cells"]["a"]["t2"]["turns_mean"] == 5.0
    assert summary["arm_totals"]["a"]["runs"] == 3 and summary["task_totals"]["t1"]["runs"] == 4
    # 汇总里的每一行都不带耗时之外的不可复现量……runs 是账单行，metrics 可复现、timing 不可
    assert set(summary["runs"][0]["timing"]) == {"elapsed_ms", "tool_ms"}
    rows = re_.compare_rows(summary)
    assert [(r["task"], r["arm"]) for r in rows] == [
        (t, a) for t in ("*", "t1", "t2") for a in "ab"]
    by = {(r["task"], r["arm"]): r for r in rows}
    # `*` 行是各臂在全部任务上的合计，不是整批的数字
    assert (by[("*", "a")]["cell"]["runs"], by[("*", "a")]["cell"]["passed"]) == (3, 3)
    assert (by[("*", "b")]["cell"]["runs"], by[("*", "b")]["cell"]["passed"]) == (2, 1)
    assert "d_pass" not in by[("t1", "a")] and "d_pass" not in by[("*", "a")]  # 基线臂没有 Δ
    assert by[("t1", "b")]["d_pass"] == pytest.approx(-0.5)
    # b 在 t1 上：轮数均值 (3 + 7) / 2 = 5，token 均值 (1600 + 4600) / 2 = 3100
    assert by[("t1", "b")]["d_turns"] == pytest.approx(2.0)
    assert by[("t1", "b")]["d_tokens"] == pytest.approx(1500.0)
    assert by[("t1", "b")]["d_tokens_pct"] == pytest.approx(1500 / 1600)
    assert by[("*", "b")]["d_pass"] == pytest.approx(-0.5)
    assert by[("*", "b")]["d_tokens"] == pytest.approx(1500.0)
    assert "cell" not in by[("t2", "b")] and "d_pass" not in by[("t2", "b")]
    assert [(t, a) for t, a, _ in re_.pass_drops(rows, 0.0)] == [("*", "b"), ("t1", "b")]
    assert [round(d, 6) for *_, d in re_.pass_drops(rows, 0.0)] == [0.5, 0.5]
    assert re_.pass_drops(rows, 0.6) == []
    # 跨批次：只在两边都有的 task × arm 上比；同一份 summary 自比，Δ 全 0
    cross = re_.compare_rows(summary, summary)
    assert [(r["task"], r["arm"]) for r in cross] == [
        (t, a) for t in ("*", "t1", "t2") for a in "ab"]
    assert all(r["d_pass"] == 0 and r["d_turns"] == 0 and r["d_tokens"] == 0
               for r in cross if "a" in r)
    assert "a" not in cross[5]  # t2 × b：两边都没有 run
    # 跨批次的臂取交集、任务取交集
    other = dict(summary, arms=["b"], tasks=["t2", "zzz"],
                 cells={"b": {"t2": None}}, arm_totals={"b": None})
    rows = re_.compare_rows(summary, other)
    assert [(r["task"], r["arm"]) for r in rows] == [("*", "b"), ("t2", "b")]


def test_min_pass_and_max_drop_exit_codes(tmp_path, capsys):
    batch = make_batch(tmp_path)
    make_run(batch, "t1", "a", 1)
    make_run(batch, "t1", "a", 2, passed=False)
    make_run(batch, "t1", "b", 1, passed=False)
    re_.summarize_batch(batch)

    def compare(**overrides):
        """(exit code, stdout) of `compare --json --batch …`."""
        args = SimpleNamespace(batch=str(batch), a=None, b=None, json=True,
                               max_pass_drop=0.0, min_pass=0.0)
        vars(args).update(overrides)
        capsys.readouterr()
        code = re_.cmd_compare(args)
        return code, capsys.readouterr().out

    # a：合计 1/2；b：合计 0/1（相对 a 掉了 50%）
    assert compare()[0] == 1  # 容差 0
    assert compare(max_pass_drop=0.6)[0] == 0  # 容差放宽到 60%：没有下降越线，也不设下限
    code, out = compare(max_pass_drop=0.6, min_pass=0.3)
    assert code == 1 and "pass rate below 30%: */b (0/1)" in out  # b 的 0% 低于 30%
    code, out = compare(max_pass_drop=1.0, min_pass=1.0)
    assert code == 1 and "pass rate below 100%" in out  # a 的 50% 也没到
    code, out = compare(max_pass_drop=1.0, min_pass=0.0)
    assert code == 0
    rows = json.loads(out)  # 纯 JSON：没有告警行混在后面
    assert [(r["task"], r["arm"]) for r in rows] == [
        (t, a) for t in ("*", "t1", "t2") for a in "ab"]
    assert rows[1]["d_pass"] == -0.5 and "d_pass" not in rows[0]
    code, out = compare(json=False, max_pass_drop=1.0, min_pass=0.0)
    assert code == 0 and out.lstrip().startswith("== batch b-1 (paid) ==")
    assert "d_pass" in out and "A -> B" not in out


def test_compare_argument_validation(tmp_path):
    args = SimpleNamespace(batch=None, a="x", b=None, json=False, max_pass_drop=0.0, min_pass=0.0)
    with pytest.raises(re_.RegressError, match="both --a and --b"):
        re_.cmd_compare(args)
    args.batch = str(tmp_path)
    with pytest.raises(re_.RegressError, match="cannot be combined"):
        re_.cmd_compare(args)
    args.a = None
    with pytest.raises(re_.RegressError, match="cannot read JSON"):
        re_.cmd_compare(args)  # 还没 summarize


def test_comparison_tables_stay_aligned(tmp_path, capsys):
    batch = make_batch(tmp_path)
    make_run(batch, "t1", "a", 1)
    make_run(batch, "t1", "b", 1)
    summary = json.loads(re_.summarize_batch(batch).read_text(encoding="utf-8"))

    def table(*summaries, cross):
        capsys.readouterr()
        re_.print_rows(re_.compare_rows(*summaries), "USD", cross=cross)
        return capsys.readouterr().out.splitlines()

    within = table(summary, cross=False)
    assert len({len(line) for line in within}) == 1  # 表头、分隔线、每一行等长
    assert within[0].startswith("task") and "d_pass" in within[0]
    assert within[0].endswith("d_tokens")
    assert within[1] == "-" * len(within[1])
    assert [line.split()[:2] for line in within[2:]] == [
        [t, a] for t in ("*", "t1", "t2") for a in "ab"]
    assert within[2].split()[-4:] == ["|", "-", "-", "-"]  # 基线臂：没有 Δ
    assert within[3].split()[-4:] == ["|", "+0%", "+0.0", "+0%"]  # 同一份配置：Δ 全 0
    assert within[3].split()[2:6] == ["1", "1/1", "3.0", "1.0k"]  # `*` 行 = 该臂全部任务的合计
    assert within[3].split()[10:13] == ["2.0", "1.0", "0.0013"]  # calls errs cost
    assert within[6].split()[2:4] == ["(no", "runs)"] and within[6].split()[-4:] == [
        "|", "-", "-", "-"]
    assert within[-1].split()[2:4] == ["(no", "runs)"]  # t2 × b：一个 run 都没跑
    assert within[5].split()[-4:] == ["|", "+0%", "+0.0", "+0%"]  # t1 × b 相对 a
    # 每个 run 平均一次工具报错：那一列不许被藏起来
    assert "errs" in within[0]

    cross = table(summary, summary, cross=True)
    assert len({len(line) for line in cross}) == 1
    assert cross[0].count("A -> B") == 6  # pass/turns/tokens/compr/errs/cost 六列都是 A -> B
    assert cross[0].endswith("d_tokens")
    assert cross[3].split()[2:4] == ["1/1", "->"] and cross[3].split()[4] == "1/1"
    assert cross[3].split()[-4:] == ["|", "+0%", "+0.0", "+0%"]
    assert cross[-1].split()[2:7] == ["(no", "runs", "on", "both", "sides)"]

    # B 侧少了一个格子：写明是哪一侧没有
    other = json.loads(json.dumps(summary))
    other["runs"] = [r for r in other["runs"] if not (r["task"] == "t1" and r["arm"] == "b")]
    other["cells"]["b"]["t1"] = None
    other["arm_totals"]["b"] = None
    mixed = table(summary, other, cross=True)
    assert len({len(line) for line in mixed}) == 1
    assert mixed[3].split()[2:7] == ["(no", "runs", "on", "side", "B)"]
    assert mixed[5].split()[2:7] == ["(no", "runs", "on", "side", "B)"]
    assert mixed[7].split()[2:7] == ["(no", "runs", "on", "both", "sides)"]


# ── 判卷脚本 ─────────────────────────────────────────────────────────────


@pytest.mark.parametrize("task_id", [t for t in TASK_IDS if t != "loop-suppression"])
def test_checkers_reject_an_untouched_workdir(tmp_path, task_id):
    task = task_by_id(task_id)
    workdir = pristine_workdir(task, tmp_path)
    proc = run_checker(task, workdir)
    assert proc.returncode == 1, proc.stdout + proc.stderr
    assert proc.stdout.startswith("FAIL:")
    assert "Traceback" not in proc.stderr  # 判失败要给出理由，不是脚本自己崩了


def test_multi_file_edit_starts_broken_and_the_checker_judges_the_tests(tmp_path):
    task = task_by_id("multi-file-edit")
    workdir = pristine_workdir(task, tmp_path)
    # 任务是真题：原样的测试套件跑不过
    proc = in_workdir([sys.executable, "run_tests.py"], workdir)
    assert proc.returncode == 1
    assert proc.stdout.strip().splitlines()[-1] == "tests: 10, failures: 0, errors: 2"
    assert "ERROR: test_calc.TestCore.test_div_by_zero_is_a_value_error" in proc.stdout
    untouched = run_checker(task, workdir)
    assert untouched.returncode == 1
    assert "summary line is 'tests: 10, failures: 0, errors: 2'" in untouched.stdout
    # 脚本里的改法就是正解：两处 edit_file 落下去，套件全绿，判卷通过
    assert replay_edits(task, workdir) == 2
    proc = in_workdir([sys.executable, "run_tests.py"], workdir)
    assert proc.returncode == 0 and proc.stdout.strip().endswith(
        "tests: 10, failures: 0, errors: 0")
    passed = run_checker(task, workdir)
    assert passed.returncode == 0 and passed.stdout.strip() == "PASS", passed.stdout
    # 为了凑绿去改测试文件：即使全绿也不过
    test_file = workdir / "tests" / "test_calc.py"
    test_file.write_text(test_file.read_text(encoding="utf-8") + "\n# touched\n", encoding="utf-8")
    proc = run_checker(task, workdir)
    assert proc.returncode == 1 and "test_calc.py was modified" in proc.stdout
    # 删掉测试文件、改 runner 同理
    test_file.unlink()
    assert "test_calc.py is missing" in run_checker(task, workdir).stdout
    (workdir / "run_tests.py").write_text("print('tests: 10, failures: 0, errors: 0')\n",
                                          encoding="utf-8")
    assert "run_tests.py was modified" in run_checker(task, workdir).stdout
    assert "fixture" not in {p.name for p in workdir.iterdir()}  # 判卷不依赖 workdir 里的副本


def test_recover_after_failure_fixture_is_really_broken(tmp_path):
    task = task_by_id("recover-after-failure")
    workdir = pristine_workdir(task, tmp_path)
    command = [sys.executable, "tools/summarize.py", "notes"]
    proc = in_workdir(command, workdir)
    assert proc.returncode == 1 and "No module named 'notelib'" in proc.stderr
    untouched = run_checker(task, workdir)
    assert untouched.returncode == 1 and "tools/summarize.py exited 1" in untouched.stdout
    # 脚本里的改法就是正解；期望输出由判卷独立重算
    assert replay_edits(task, workdir) == 1
    proc = in_workdir(command, workdir)
    assert proc.returncode == 0, proc.stderr
    lines = proc.stdout.strip().splitlines()
    assert lines[-1] == "total: 9" and len(lines) == 4
    checker = load_module("regress_check_recover", task["check"])
    assert lines == checker.expected_lines(task["fixture"] / "notes")
    out = workdir / "out"
    out.mkdir()
    (out / "summary.txt").write_text(proc.stdout, encoding="utf-8")
    passed = run_checker(task, workdir)
    assert passed.returncode == 0 and passed.stdout.strip() == "PASS", passed.stdout
    # 保存的输出与命令的实际输出不符 / 没保存 / 为了能跑而动了 notes：都不过
    (out / "summary.txt").write_text(proc.stdout.replace("total:", "TOTAL:"), encoding="utf-8")
    assert "does not match the command's output" in run_checker(task, workdir).stdout
    (out / "summary.txt").unlink()
    assert "out/summary.txt is missing" in run_checker(task, workdir).stdout
    (out / "summary.txt").write_text(proc.stdout, encoding="utf-8")
    note = workdir / "notes" / "2026-11-03.txt"
    note.write_text(note.read_text(encoding="utf-8") + "- [ ] tampered\n", encoding="utf-8")
    proc = run_checker(task, workdir)
    assert "2026-11-03.txt was modified" in proc.stdout
    # 期望值只来自任务自己的 fixture：改了 workdir 里的 notes 也骗不过「输出对不对」
    in_workdir([sys.executable, "-c", "pass"], workdir)
    (out / "summary.txt").write_text(proc.stdout.replace("total: 9", "total: 10"),
                                     encoding="utf-8")
    assert "does not match the command's output" in run_checker(task, workdir).stdout


GOOD_RESTOCK = {"items": {"nut": {"units": 12, "from": "north"},
                          "clip": {"units": 8, "from": "south"},
                          "valve": {"units": 15, "from": "south"}}, "cost_cents": 5206}


def test_mcp_many_tools_answer_follows_the_server_tables():
    task = task_by_id("mcp-many-tools")
    checker = load_module("regress_check_mcp", task["check"])
    assert checker.expected() == {
        "items": {"clip": {"units": 8, "from": "south"},
                  "nut": {"units": 12, "from": "north"},
                  "valve": {"units": 15, "from": "south"}},
        "cost_cents": 5206}
    # 脚本写出的答案就是服务器数据推出的答案
    answer = [c for turn in task["script"]["turns"] for c in turn.get("tools", [])
              if c["name"] == "write_file"]
    assert len(answer) == 1
    assert json.loads(answer[0]["arguments"]["content"]) == checker.expected()
    assert len(checker.mcp_server.TOOLS) == 12  # 十二个工具里只该用四个
    # 布尔不是数字（Python 里 True == 1）；键的顺序无所谓；8.0 就是 8
    assert checker.same({"units": 1}, {"units": True}) is False
    assert checker.same({"units": True}, {"units": 1}) is False
    assert checker.same({"a": 1, "b": 2}, {"b": 2, "a": 1}) is True
    assert checker.same({"units": 8}, {"units": 8.0}) is True
    assert checker.same({"units": 8}, {"units": 8, "from": "x"}) is False
    assert checker.same([1, "a"], [1, "a"]) is True and checker.same([1], [1, 1]) is False


@pytest.mark.parametrize("payload", [
    dict(GOOD_RESTOCK, cost_cents=5206 + 1),  # 账算错
    {"items": {"nut": {"units": 13, "from": "north"}, "clip": {"units": 8, "from": "south"},
               "valve": {"units": 15, "from": "south"}}, "cost_cents": 5206},  # 多搬
    {"items": {"nut": {"units": 12, "from": "south"}, "clip": {"units": 8, "from": "south"},
               "valve": {"units": 15, "from": "south"}}, "cost_cents": 5206},  # 来源仓库错
    {"items": {"nut": GOOD_RESTOCK["items"]["nut"]}, "cost_cents": 2250},  # 漏了两项
    {"items": {}, "cost_cents": 0},  # 空计划
    dict(GOOD_RESTOCK, items=dict(GOOD_RESTOCK["items"], bolt={  # 多出一项不缺的货品
        "units": 12, "from": "north"})),
    dict(GOOD_RESTOCK, items=dict(GOOD_RESTOCK["items"],
                                  nut={"units": True, "from": "north"})),  # 布尔不是 12
    {"Items": GOOD_RESTOCK["items"], "cost_cents": 5206},  # 键名
    {"items": GOOD_RESTOCK["items"]},  # 没给费用
    dict(GOOD_RESTOCK, note="done"),  # 多出来的键
    [GOOD_RESTOCK],  # 顶层形状
])
def test_mcp_many_tools_checker_rejects_wrong_answers(tmp_path, payload):
    task = task_by_id("mcp-many-tools")
    (tmp_path / "out").mkdir()
    write_json(tmp_path / "out" / "restock.json", payload)
    proc = run_checker(task, tmp_path)
    assert proc.returncode == 1 and "out/restock.json is" in proc.stdout and "want" in proc.stdout


def test_mcp_many_tools_checker_accepts_the_exact_answer(tmp_path):
    task = task_by_id("mcp-many-tools")
    checker = load_module("regress_check_mcp", task["check"])
    out = tmp_path / "out"
    out.mkdir()
    # 键的顺序与脚本的写法不同、数字写成 12.0：语义相同，照过
    near = json.loads(json.dumps(GOOD_RESTOCK))
    near["items"]["nut"]["units"] = 12.0
    write_json(out / "restock.json", near)
    assert checker.reasons(tmp_path) == []
    assert run_checker(task, tmp_path).returncode == 0
    (out / "restock.json").write_text("{not json", encoding="utf-8")
    proc = run_checker(task, tmp_path)
    assert proc.returncode == 1 and "out/restock.json is not valid JSON" in proc.stdout
    (out / "restock.json").unlink()
    assert "out/restock.json is missing" in run_checker(task, tmp_path).stdout


# mock MCP server 每个工具的一次良构调用（参数取自服务器自己的数据表）
WELL_FORMED = {
    "list_warehouses": {},
    "warehouse_info": {"warehouse": "east"},
    "list_items": {},
    "item_price": {"item": "nut"},
    "item_stock": {"item": "nut", "warehouse": "east"},
    "stock_total": {"item": "nut"},
    "transfer_stock": {"item": "nut", "from_warehouse": "north", "to_warehouse": "east",
                       "units": 12},
    "job_start": {"job": "nightly-export"},
    "job_status": {"job": "sync-archive"},
    "list_records": {},
    "lookup_record": {"id": "r-1001"},
    "ping": {},
}


def test_every_mock_mcp_tool_accepts_a_well_formed_call():
    """脚本化模型只走任务需要的那几个工具——没被脚本调过的工具等于没测过。2026-11-26
    第一次真机批次：`transfer_stock` 的 `_need` 要求每个参数都是非空字符串，整数 `units`
    被判成 `missing argument 'units'`，真实模型白撞 25 次（一次 run 的全部工具错误）；零成本通道
    一直是绿的。"""
    server = load_module("regress_mcp_server_probe", re_.REGRESS / "mcp_server.py")
    assert set(WELL_FORMED) == {tool["name"] for tool in server.TOOLS}  # 新工具必须补一条样例
    for name, args in WELL_FORMED.items():
        server.call(name, dict(args))  # 工具级错误是 ValueError（线上即 isError 结果）
    transfer = WELL_FORMED["transfer_stock"]
    east = server.STOCK["nut"]["east"]
    server.call("transfer_stock", dict(transfer, units=3))
    assert server.STOCK["nut"]["east"] == east + 3
    # 类型约束照旧：字符串、布尔、零、负数、缺失都不是合法的 units
    for bad in ("12", True, 0, -1, None):
        args = dict(transfer, units=bad)
        if bad is None:
            del args["units"]
        with pytest.raises(ValueError):
            server.call("transfer_stock", args)


def test_loop_suppression_checker_judges_how_the_run_ends(tmp_path):
    task = task_by_id("loop-suppression")
    polls = [{"name": "mcp_regress_job_status"}] * 7
    giving_up = {"stop_reason": "stuck_loop", "tool_calls": polls, "turns": 7}
    assert run_checker(task, tmp_path, giving_up).returncode == 0
    assert run_checker(task, tmp_path, dict(giving_up, stop_reason="end_turn")).returncode == 0
    # 烧完了轮次预算才停：不算
    burned = run_checker(task, tmp_path, dict(giving_up, stop_reason="max_turns_reached"))
    assert burned.returncode == 1 and "stop_reason is 'max_turns_reached'" in burned.stdout
    # 轮询太多次才放弃：不算
    assert run_checker(task, tmp_path, dict(giving_up, tool_calls=polls * 2)).returncode == 1
    # 活没干完却写了「完成」文件：不过
    out = tmp_path / "out"
    out.mkdir()
    (out / "job.txt").write_text("nightly-export: running\n", encoding="utf-8")
    assert "job never reported done" in run_checker(task, tmp_path, giving_up).stdout
    (out / "job.txt").unlink()
    # 「什么都不做」在这个任务上反而能过——所以它必须自带一个更坏的坏模型
    assert run_checker(task, tmp_path).returncode == 0
    calls = [c for turn in task["bad_script"]["turns"] for c in turn.get("tools", [])]
    assert calls == [{"name": "write_file", "arguments": {
        "path": "out/job.txt", "content": "nightly-export: running"}}]


def test_bad_model_scripts_cannot_pass_any_checker():
    """「什么都不做」过不了的任务用默认坏模型；过得了的任务必须自带一个更坏的。"""
    for task in re_.load_tasks():
        assert task["bad_script"]["turns"]
        calls = [c for turn in task["bad_script"]["turns"] for c in turn.get("tools", [])]
        if task["id"] == "loop-suppression":
            assert calls  # 什么都不做能过 loop-suppression，所以它的坏模型要写文件
        else:
            assert not calls and task["bad_script"] is re_.BAD_MODEL


def test_long_output_generator_matches_its_count_table():
    task = task_by_id("long-output")
    setup = load_module("regress_long_output_setup", task["setup"])
    data = setup.build()
    names = sorted(data)
    assert names == [f"part_{i:02d}.txt" for i in range(1, 17)]
    assert [data[name].count(" SIGNAL ") for name in names] == list(setup.COUNTS)
    assert sum(setup.COUNTS) == 54
    assert setup.build() == data  # 确定性：与 Python 版本的 random 无关
    assert [data[name].count("\n") for name in names] == [setup.LINES_PER_FILE] * 16
    assert all(4_000 < len(text) < 6_000 for text in data.values())  # 约 4 KB 一份
    joined = "\n".join(data.values())
    # 整词匹配：近似词都在数据里，但都不能被 " SIGNAL " 命中
    assert all(word in joined for word in ("RESIGNAL", "SIGNALLED", "signal strength",
                                           "SIGNAL_COUNT"))
    # 脚本每一步写进 counts.tsv 的数都来自这张表：逐个文件核对，末行是累计
    written = [c["arguments"]["content"] for turn in task["script"]["turns"]
               for c in turn.get("tools", []) if c["name"] == "write_file"]
    assert len(written) == 3
    covered = 0
    for table in written:
        rows = [line.split("\t") for line in table.strip().splitlines()]
        so_far = [name for name, _ in rows[:-1]]
        assert so_far == names[:len(so_far)] and len(so_far) > covered  # 只增不减
        covered = len(so_far)
        assert [int(count) for _, count in rows[:-1]] == list(setup.COUNTS[:covered])
        assert rows[-1] == ["total", str(sum(setup.COUNTS[:covered]))]
    assert covered == 16 and written[-1].count("\n") == 17  # 16 个文件 + total


def test_long_output_checker_needs_the_counts_and_the_compressions(tmp_path):
    task = task_by_id("long-output")
    setup = load_module("regress_long_output_setup", task["setup"])
    workdir = pristine_workdir(task, tmp_path)
    out = workdir / "out"
    out.mkdir()
    names = sorted(setup.build())

    def table_for(counts):
        return "".join(f"{name}\t{count}\n" for name, count in zip(names, counts)) \
            + f"total\t{sum(counts)}\n"

    good = {"compressions": [{}, {}, {}]}
    (out / "counts.tsv").write_text(table_for(setup.COUNTS), encoding="utf-8")
    passed = run_checker(task, workdir, good)
    assert passed.returncode == 0 and passed.stdout.strip() == "PASS", passed.stdout
    # 长对话确实发生过：一次都没压缩不算。只折叠 1 次是过的——内核修复之后正确完成的 run
    # 常常只折叠 1 次，判卷线因此是 1（docs/kernel_improvement_plan.md §0.0.5）
    proc = run_checker(task, workdir, {"compressions": [{}]})
    assert proc.returncode == 0 and proc.stdout.strip() == "PASS", proc.stdout
    proc = run_checker(task, workdir, {})
    assert proc.returncode == 1 and "0 history compressions on record, want at least 1" in proc.stdout
    # 求和错
    (out / "counts.tsv").write_text(table_for(setup.COUNTS).replace("total\t54", "total\t53"),
                                    encoding="utf-8")
    assert "out/counts.tsv is wrong" in run_checker(task, workdir, good).stdout
    # 求和对、单项错：不过（不能靠凑总数蒙混）
    swapped = list(setup.COUNTS)
    swapped[0], swapped[1] = swapped[0] + 1, swapped[1] - 1
    (out / "counts.tsv").write_text(table_for(swapped), encoding="utf-8")
    proc = run_checker(task, workdir, good)
    assert proc.returncode == 1 and "line 1: 'part_01.txt" in proc.stdout \
        and "want 'part_01.txt" in proc.stdout
    # 少一行 / 多一行
    (out / "counts.tsv").write_text(
        "".join(line + "\n" for line in table_for(setup.COUNTS).splitlines()[1:]),
        encoding="utf-8")
    assert "16 lines, want 17" in run_checker(task, workdir, good).stdout
    # 顺手改了数据 / 删了数据
    (out / "counts.tsv").write_text(table_for(setup.COUNTS), encoding="utf-8")
    (workdir / "data" / "part_01.txt").write_text("SIGNAL \n", encoding="utf-8")
    assert "data/part_01.txt was modified" in run_checker(task, workdir, good).stdout
    (workdir / "data" / "part_16.txt").unlink()
    assert "data/part_16.txt is missing" in run_checker(task, workdir, good).stdout
    # 没交文件
    (out / "counts.tsv").unlink()
    assert "out/counts.tsv is missing" in run_checker(task, workdir, good).stdout


# ── cjk-docs：参考答案与判卷规则 ─────────────────────────────────────────


@pytest.fixture(scope="module")
def cjk():
    return load_module("regress_check_cjk", task_by_id("cjk-docs")["check"])


def cjk_pair(name="user-guide.md"):
    """(source, reference answer) of one cjk-docs document."""
    task = task_by_id("cjk-docs")
    return ((task["dir"] / "fixture" / "docs" / name).read_text(encoding="utf-8"),
            (task["dir"] / "expected" / name).read_text(encoding="utf-8"))


def test_cjk_reference_answers_are_clean(cjk):
    assert cjk.DOCS == ("user-guide.md", "install-guide.md")
    assert cjk.glossary_terms() == cjk.TERMS
    for name in cjk.DOCS:
        src, want = cjk_pair(name)
        assert cjk.doc_issues(name, src, want) == []
        assert cjk.doc_issues(name, src, src)  # 原文本身不合规
        # 只改了该改的：行数、标题文字、行内代码都不变
        assert len(src.splitlines()) == len(want.splitlines())
        assert [t for _, t in cjk.scan(src)[2]] == [t for _, t in cjk.scan(want)[2]]
        assert cjk.scan(src)[1] == cjk.scan(want)[1]


def test_cjk_script_writes_exactly_the_reference_answers():
    task = task_by_id("cjk-docs")
    script = re_.substitute(task["script"], task["dir"], True)
    written = [c for turn in script["turns"] for c in turn.get("tools", [])
               if c["name"] == "write_file"]
    assert [c["arguments"]["path"] for c in written] == ["out/user-guide.md", "out/install-guide.md"]
    for call in written:
        want = (task["dir"] / "expected" / Path(call["arguments"]["path"]).name).read_text(
            encoding="utf-8")
        assert call["arguments"]["content"] == want
    # 没给 shell：只能用 read_file / write_file / list_dir 完成任务
    assert sorted(task["config"]["tools"]) == ["list_dir", "read_file", "write_file"]


def test_cjk_rules_catch_each_kind_of_mistake(cjk):
    src, want = cjk_pair()
    lines = want.splitlines(keepends=True)
    han = "[㐀-䶿一-鿿]"
    assert "账号" in want and "帐号" not in want
    assert any("non-standard wording left: 帐号" in i
               for i in cjk.doc_issues("d", src, want.replace("账号", "帐号")))
    headings = [i for i, line in enumerate(lines) if line.startswith("## ")]
    assert len(headings) >= 3 and lines[0].startswith("# ")

    def issues(changes):
        """The reference answer with some lines replaced (line number → new text)."""
        return cjk.doc_issues("d", src, "".join(
            changes.get(i, line) for i, line in enumerate(lines)))

    # 标题层级：### 不行，只能是 ##
    assert "d: headings that are not `## `: '### 简介'" in issues(
        {headings[0]: "###" + lines[headings[0]][2:]})
    # 第一个标题降了一级：文档没有 H1 了
    assert issues({0: "## " + lines[0][2:]})[0] == \
        "d: the first heading must be the document's single H1"
    # 少一个标题 / 改了标题文字
    dropped = "".join(line for i, line in enumerate(lines) if i != headings[1])
    assert any("heading texts differ" in i for i in cjk.doc_issues("d", src, dropped))
    assert any("heading texts differ" in i for i in issues({headings[1]: "## 概览\n"}))
    # 标点：紧邻中文的半角逗号没换——只触发这一条规则
    comma = next(i for i, line in enumerate(lines) if re.search(han + "，", line))
    ascii_comma = re.sub("(" + han + ")，", r"\1, ", lines[comma], count=1)
    found = issues({comma: ascii_comma})
    rule = f"d: line {comma + 1}: ASCII ',' next to Chinese text"
    assert rule in found
    assert [i for i in found if "content differs" not in i] == [rule]
    # 反过来，英文句子里的逗号不许动
    english = next(i for i, line in enumerate(lines) if ", " in line and "，" not in line)
    assert issues({english: lines[english].replace(", ", "，", 1)})
    # 句末的半角句点
    period = next(i for i, line in enumerate(lines) if re.search(han + "。", line))
    assert any("ASCII '.' ending a Chinese sentence" in i
               for i in issues({period: re.sub("(" + han + ")。", r"\1.", lines[period],
                                                 count=1)}))
    # 增删内容
    assert any("content differs" in i
               for i in issues({comma: lines[comma] + "多出来的一句。\n"}))
    # 行内代码：改了不行，「顺手修」代码里的半角逗号也不行
    assert "`tiny-agent login`" in want
    assert "d: inline code spans differ from the source" in cjk.doc_issues(
        "d", src, want.replace("`tiny-agent login`", "`tiny-agent signin`"))
    assert '你是助手,请简洁回答使用者的问题' in want
    fixed = want.replace("你是助手,请简洁回答使用者的问题", "你是助手，请简洁回答用户的问题")
    assert "d: inline code spans differ from the source" in cjk.doc_issues("d", src, fixed)


def test_cjk_checker_passes_the_reference_and_rejects_shortcuts(tmp_path):
    task = task_by_id("cjk-docs")
    workdir = pristine_workdir(task, tmp_path)
    out = workdir / "out"
    out.mkdir()
    proc = run_checker(task, workdir)
    assert proc.returncode == 1 and "out/user-guide.md is missing" in proc.stdout
    # 参考答案：通过
    for name in ("user-guide.md", "install-guide.md"):
        shutil.copy(task["dir"] / "expected" / name, out / name)
    proc = run_checker(task, workdir)
    assert proc.returncode == 0 and proc.stdout.strip() == "PASS", proc.stdout
    # 「捷径」：把 docs/ 原样拷到 out/——规则一条没改
    for name in ("user-guide.md", "install-guide.md"):
        shutil.copy(task["dir"] / "fixture" / "docs" / name, out / name)
    proc = run_checker(task, workdir)
    assert proc.returncode == 1 and "non-standard wording left:" in proc.stdout \
        and "帐号" in proc.stdout
    # 只交了一份
    shutil.copy(task["dir"] / "expected" / "install-guide.md", out / "install-guide.md")
    shutil.copy(task["dir"] / "expected" / "user-guide.md", out / "user-guide.md")
    (out / "install-guide.md").unlink()
    assert "out/install-guide.md is missing" in run_checker(task, workdir).stdout
    shutil.copy(task["dir"] / "expected" / "install-guide.md", out / "install-guide.md")
    # 为了凑「原样」去改 docs/ 下的源文件：不过
    source = workdir / "docs" / "user-guide.md"
    source.write_text(source.read_text(encoding="utf-8") + "\n多出来的一行。\n", encoding="utf-8")
    proc = run_checker(task, workdir)
    assert proc.returncode == 1 and "user-guide.md was modified" in proc.stdout
    source.unlink()
    assert "user-guide.md is missing" in run_checker(task, workdir).stdout


def test_glossary_drift_fails_the_checker(tmp_path):
    """check.py 的术语表与任务自带的 glossary.md 对不上：check.py 自己就退出 1。"""
    task = task_by_id("cjk-docs")
    # 保持 tasks/<id>/ 的深度：check.py 经 TASK_DIR.parents[1] 找 checklib
    root = tmp_path / "regress"
    drifted = root / "tasks" / "cjk-docs"
    shutil.copytree(task["dir"], drifted)
    shutil.copy(re_.REGRESS / "checklib.py", root / "checklib.py")
    glossary = drifted / "fixture" / "docs" / "glossary.md"
    assert "| 密码 | 口令 |" in glossary.read_text(encoding="utf-8")
    glossary.write_text(glossary.read_text(encoding="utf-8").replace(
        "| 密码 | 口令 |", "| 密码 | 口令、暗号 |"), encoding="utf-8")
    workdir = tmp_path / "work"
    (workdir / "out").mkdir(parents=True)
    shutil.copytree(drifted / "fixture" / "docs", workdir / "docs")
    proc = run_checker({"check": drifted / "check.py"}, workdir)
    assert proc.returncode == 1 and "no longer matches" in proc.stdout


@NEEDS_BINARY
def test_zero_cost_check_passes_end_to_end(capsys):
    """CI 门禁原样跑一遍：任务集、6 任务 × 2 协议的正路径、坏模型、可复现、
    付费闸门、mock-smoke 流水线。任何环境下的 check 都发不出付费请求。"""
    before = set(re_.ART.glob("check-*"))
    assert re_.cmd_check(SimpleNamespace(keep=False)) == 0
    out = capsys.readouterr().out
    assert "REGRESS CHECK ALL PASS" in out and "[FAIL]" not in out
    assert out.count("[PASS] good path:") == 6 * len(re_.CHECK_PROVIDERS)
    assert out.count("[PASS] bad model rejected:") == 6
    assert out.count("[PASS] reproducible:") == 6
    assert set(re_.ART.glob("check-*")) == before  # 成功后不留现场


def test_no_bytecode_cache_is_left_in_the_task_tree():
    """评测与判卷全链路跑完之后（本文件最后一个用例）：仓库的 examples/ 下没有
    __pycache__ / .pyc——mock、check.py、setup.py 都是 import 进来的。"""
    top = ROOT / "examples" / "__pycache__"
    stray = (list(top.glob("*")) if top.is_dir() else []) + [
        p for p in re_.REGRESS.rglob("*") if p.name == "__pycache__" or p.suffix == ".pyc"]
    assert stray == []
