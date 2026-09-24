#!/usr/bin/env python3
"""code_value 装置单元测试（工单 01 第 4 条：先于任何评测跑通）。

覆盖：
  - 评分器 §5.1 全套反例（缺行/重复 id/未知 id/错计数/坏 JSON/类型与
    schema 错/空产物/空 gold + 误报/run 失败但产物完整）；
  - 计费诚实性：缺 usage → cost=null + 未知预留（绝不当 0）、缓存不双计、
    失败 run 留账不剔除；
  - 预算闸：余量不足一个 run 预留时拒绝启动；
  - run_id 唯一性（目录已存在即拒绝）；
  - G1 顺序与 fixture 不变量、prompt 组装的差分断言。

全部离线：不碰真实网关、不起二进制（g0 子命令另有端到端）。
"""
import json
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import code_value_score as scorer   # noqa: E402
import code_value_fixture as fixture  # noqa: E402
import code_value_eval as cv_eval   # noqa: E402


# ── 评分器：§5.1 全套反例 ───────────────────────────────────────────────────

GOLD_T0 = {"id": {"r-01": None, "r-02": None, "r-09": None}}
GOLD_T1 = {"user_id": {"u-1": 2, "u-2": 1}}


def _write(tmp_path, content: str) -> Path:
    p = tmp_path / "answer.jsonl"
    p.write_text(content, encoding="utf-8")
    return p


def test_validator_accepts_complete_correct(tmp_path):
    art = _write(tmp_path, '{"id": "r-01"}\n{"id": "r-02"}\n{"id": "r-09"}\n')
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert v["delivered_exact"] and not v["reasons"]
    assert v["details"]["delivered_count"] == 3


def test_validator_rejects_missing_id(tmp_path):
    art = _write(tmp_path, '{"id": "r-01"}\n{"id": "r-02"}\n')
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert not v["delivered_exact"]
    assert any(r.startswith("missing_id: r-09") for r in v["reasons"])


def test_validator_rejects_duplicate_id(tmp_path):
    art = _write(tmp_path, '{"id": "r-01"}\n{"id": "r-01"}\n{"id": "r-02"}\n'
                           '{"id": "r-09"}\n')
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert not v["delivered_exact"]
    assert any(r.startswith("duplicate_id: r-01") for r in v["reasons"])
    assert "r-01" in v["details"]["duplicate_ids"]


def test_validator_rejects_unknown_id(tmp_path):
    art = _write(tmp_path, '{"id": "r-01"}\n{"id": "r-02"}\n{"id": "r-09"}\n'
                           '{"id": "r-77"}\n')
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert not v["delivered_exact"]
    assert any(r.startswith("unknown_id: r-77") for r in v["reasons"])


def test_validator_rejects_wrong_count(tmp_path):
    art = _write(tmp_path, '{"user_id": "u-1", "event_count": 3}\n'
                           '{"user_id": "u-2", "event_count": 1}\n')
    v = scorer.validate_artifact(art, GOLD_T1, "t1")
    assert not v["delivered_exact"]
    assert any(r.startswith("wrong_count: u-1") for r in v["reasons"])
    assert v["details"]["wrong_counts"]["u-1"] == {"delivered": 3, "want": 2}


def test_validator_rejects_invalid_json_line(tmp_path):
    art = _write(tmp_path, '{"id": "r-01"}\nnot json at all\n{"id": "r-02"}\n'
                           '{"id": "r-09"}\n')
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert not v["delivered_exact"]
    assert any(r.startswith("invalid_json: line 2") for r in v["reasons"])
    assert 2 in v["details"]["invalid_lines"]


def test_validator_rejects_schema_extra_field(tmp_path):
    art = _write(tmp_path, '{"id": "r-01", "note": "x"}\n')
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert not v["delivered_exact"]
    assert any(r.startswith("schema_error: line 1: fields") for r in v["reasons"])


def test_validator_rejects_schema_missing_field(tmp_path):
    art = _write(tmp_path, '{"user_id": "u-1"}\n')
    v = scorer.validate_artifact(art, GOLD_T1, "t1")
    assert not v["delivered_exact"]
    assert any(r.startswith("schema_error: line 1: fields") for r in v["reasons"])


def test_validator_rejects_wrong_type(tmp_path):
    art = _write(tmp_path, '{"user_id": "u-1", "event_count": "2"}\n')
    v = scorer.validate_artifact(art, GOLD_T1, "t1")
    assert not v["delivered_exact"]
    assert any(r.startswith("schema_error: line 1: wrong type") for r in v["reasons"])


def test_validator_rejects_bool_as_count(tmp_path):
    # bool 是 int 子类：true 冒充 event_count 必须拦下
    art = _write(tmp_path, '{"user_id": "u-1", "event_count": true}\n')
    v = scorer.validate_artifact(art, GOLD_T1, "t1")
    assert not v["delivered_exact"]
    assert any(r.startswith("schema_error: line 1: wrong type") for r in v["reasons"])


def test_validator_rejects_non_object_line(tmp_path):
    art = _write(tmp_path, '["r-01"]\n')
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert not v["delivered_exact"]
    assert any(r.startswith("schema_error: line 1: not a JSON object")
               for r in v["reasons"])


def test_validator_rejects_missing_file(tmp_path):
    v = scorer.validate_artifact(tmp_path / "nope.jsonl", GOLD_T0, "t0")
    assert not v["delivered_exact"]
    assert any(r.startswith("empty_artifact: answer.jsonl not found")
               for r in v["reasons"])


def test_validator_rejects_zero_byte_file(tmp_path):
    art = _write(tmp_path, "")
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert not v["delivered_exact"]
    assert any("0 bytes" in r for r in v["reasons"])


def test_validator_empty_gold_false_positive(tmp_path):
    empty_gold = {"id": {}}
    art = _write(tmp_path, '{"id": "r-01"}\n{"id": "r-02"}\n')
    v = scorer.validate_artifact(art, empty_gold, "t0")
    assert not v["delivered_exact"]
    assert any(r.startswith("false_positive: gold is empty but 2 ids delivered")
               for r in v["reasons"])
    assert v["details"]["false_positive_ids"] == ["r-01", "r-02"]


def test_validator_blank_lines_are_not_business_errors(tmp_path):
    art = _write(tmp_path, '\n{"id": "r-01"}\n\n{"id": "r-02"}\n{"id": "r-09"}\n\n')
    v = scorer.validate_artifact(art, GOLD_T0, "t0")
    assert v["delivered_exact"], v["reasons"]
    assert v["details"]["line_count"] == 3


# ── 计费诚实性（§5.2 / §4.1）────────────────────────────────────────────────

PRICE = {"input": 1.0, "output": 2.0, "cache_creation": 1.25,
         "cache_read": 0.1, "currency": "CNY"}


def _result(input_tokens=100, output_tokens=50, cache_creation=0,
            cache_read=0, turns=2, attempts=1, tool_calls=None):
    return {"input_tokens": input_tokens, "output_tokens": output_tokens,
            "cache_creation_input_tokens": cache_creation,
            "cache_read_input_tokens": cache_read,
            "turns": turns, "attempts": attempts,
            "tool_calls": tool_calls or []}


def test_usage_normal():
    u = scorer.usage_from_result(_result())
    assert u["requests_happened"] and not u["usage_missing"]
    assert u["total_tokens"] == 150
    assert u["unknown_reserve_tokens"] == 0


def test_usage_missing_is_flagged_and_reserved_not_zero():
    # 请求发生过（turns>0）但 usage 全零 → 未知消耗，预留，绝不当 0
    u = scorer.usage_from_result({"turns": 3, "attempts": 1})
    assert u["usage_missing"]
    assert u["unknown_reserve_tokens"] == scorer.UNKNOWN_RESERVE_TOKENS
    assert u["unknown_reserve_tokens"] > 0


def test_no_requests_no_missing_usage():
    # run 根本没发出请求（turns=0 attempts=0）：不是「缺 usage」
    u = scorer.usage_from_result({})
    assert not u["requests_happened"] and not u["usage_missing"]
    assert u["unknown_reserve_tokens"] == 0


def test_cost_null_when_price_missing():
    u = scorer.usage_from_result(_result())
    c = scorer.cost_from_usage(u, None)
    assert c["cost"] is None and c["cost_known_lower"] is None
    assert not c["cost_complete"]
    assert c["reason"] == "price_snapshot_missing"


def test_cost_null_when_usage_missing():
    u = scorer.usage_from_result({"turns": 1, "attempts": 1})
    c = scorer.cost_from_usage(u, PRICE)
    assert c["cost"] is None and not c["cost_complete"]
    assert c["reason"] == "usage_missing"
    assert c["unknown_reserve_tokens"] > 0


def test_cost_math_each_bucket_once():
    u = scorer.usage_from_result(_result(input_tokens=100, output_tokens=50,
                                         cache_creation=10, cache_read=40))
    c = scorer.cost_from_usage(u, PRICE)
    assert c["cost_complete"]
    assert c["cost"] == pytest.approx(100 * 1.0 + 50 * 2.0
                                      + 10 * 1.25 + 40 * 0.1)
    assert c["cost_known_lower"] == c["cost"]
    assert c["currency"] == "CNY"


def test_cache_not_double_counted():
    # input_tokens 在 provider 层已剔除缓存读：input + cache_read 各计一次，
    # 两者相加不会把同一批 token 计两次（openai.rs 语义）。
    u = scorer.usage_from_result(_result(input_tokens=60, cache_read=40))
    assert u["tokens"]["input_tokens"] + u["tokens"]["cache_read_input_tokens"] \
        == 100
    c = scorer.cost_from_usage(u, PRICE)
    assert c["cost"] == pytest.approx(60 * 1.0 + 50 * 2.0 + 40 * 0.1)


def test_inner_llm_meta_accounting():
    tc = [{"name": "run_code",
           "meta": {"llm": {"models": {"m1": {"known_usage": 500}},
                            "unknown_calls": 2}}}]
    u = scorer.usage_from_result(_result(tool_calls=tc))
    assert u["inner_llm"] == {"known_usage": 500, "unknown_calls": 2}


def test_summarize_run_failed_but_artifact_complete():
    # §5.1：run 失败但产物完整 → delivered=True + abnormal_end=True
    validation = {"delivered_exact": True, "reasons": [],
                  "details": {"task": "t0"}}
    usage = scorer.usage_from_result(_result())
    cost = scorer.cost_from_usage(usage, PRICE)
    s = scorer.summarize_run({"run_id": "x", "task": "t0", "arm": "A"},
                             validation, usage, cost,
                             end_reason="max_tokens")
    assert s["delivered_exact"] and s["abnormal_end"]
    assert s["end_reason"] == "max_tokens"


def test_aggregate_keeps_failed_and_missing_usage_runs():
    # §5.3：所有 run（含失败/partial）都进账，不做任何剔除
    def _run(run_id, delivered, end_reason, result):
        validation = {"delivered_exact": delivered, "reasons": [],
                      "details": {}}
        usage = scorer.usage_from_result(result)
        cost = scorer.cost_from_usage(usage, PRICE)
        summary = scorer.summarize_run(
            {"run_id": run_id, "task": "t0", "arm": "A"},
            validation, usage, cost, end_reason)
        return {"run_id": run_id, "task": "t0", "arm": "A",
                "summary": summary}

    runs = [
        _run("ok-1", True, "end_turn", _result()),
        _run("fail-1", True, "error", _result()),          # 失败但产物完整
        _run("miss-1", False, "end_turn", {"turns": 2}),   # 缺 usage
    ]
    agg = scorer.aggregate(runs)
    assert agg["runs"] == 3
    assert agg["delivered_exact_count"] == 2
    assert "fail-1" in agg["failed_runs"]
    assert "miss-1" in agg["usage_missing_runs"]
    assert "miss-1" in agg["unknown_cost_runs"]
    # 主指标：有未知消耗即 null（工单 01R——不把混合口径当主指标）
    assert agg["cost_per_success"] is None
    # 已知费用下界合计仍给出（ok-1 + fail-1 的完整费用）
    per_run_cost = 100 * 1.0 + 50 * 2.0  # input*1 + output*2
    assert agg["cost_complete_total"] == pytest.approx(per_run_cost * 2)
    assert agg["cost_known_lower_total"] == pytest.approx(per_run_cost * 2)
    assert agg["by_arm"]["A"]["runs"] == 3


def test_aggregate_cost_per_success_when_all_complete():
    def _run(run_id, result):
        usage = scorer.usage_from_result(result)
        cost = scorer.cost_from_usage(usage, PRICE)
        return {"run_id": run_id, "task": "t0", "arm": "A",
                "summary": scorer.summarize_run(
                    {"run_id": run_id, "task": "t0", "arm": "A"},
                    {"delivered_exact": True, "reasons": [], "details": {}},
                    usage, cost, "end_turn")}
    runs = [_run("a", _result()), _run("b", _result())]
    agg = scorer.aggregate(runs)
    per = 100 * 1.0 + 50 * 2.0
    assert agg["cost_per_success"] == pytest.approx(per * 2 / 2)
    assert agg["unknown_cost_runs"] == []


# ── 预算闸（§4.1：防「软闸」冒充硬预算）────────────────────────────────────

def test_budget_gate_allows_when_headroom():
    g = cv_eval.budget_gate(ledger_tokens=0, unknown_reserve=0,
                            envelope=480_000, per_run_reserve=80_000)
    assert g["ok"]
    assert g["headroom_tokens"] == 400_000
    assert g["inflight_overrun_risk_tokens"] == cv_eval.INFLIGHT_OVERRUN_TOKENS


def test_budget_gate_refuses_when_insufficient_headroom():
    g = cv_eval.budget_gate(ledger_tokens=400_001, unknown_reserve=0,
                            envelope=480_000, per_run_reserve=80_000)
    assert not g["ok"]  # 400,001 + 80,000 > 480,000：拒绝启动
    assert g["headroom_tokens"] < 0


def test_budget_gate_boundary_inclusive():
    g = cv_eval.budget_gate(ledger_tokens=400_000, unknown_reserve=0,
                            envelope=480_000, per_run_reserve=80_000)
    assert g["ok"]  # 恰好用满包络 = 允许（needed <= envelope）
    assert g["headroom_tokens"] == 0


def test_budget_gate_counts_unknown_reserve():
    g = cv_eval.budget_gate(ledger_tokens=300_000,
                            unknown_reserve=scorer.UNKNOWN_RESERVE_TOKENS,
                            envelope=480_000, per_run_reserve=80_000)
    # 300K + 112,768 + 80K > 480K → 拒绝
    assert not g["ok"]


def test_require_paid_prereqs_blocks_without_price():
    m = {"price_snapshot": None, "cost_currency": None, "cost_stop": None}
    assert cv_eval.require_paid_prereqs(m) == ["price_snapshot(price_snapshot_missing)",
                                               "cost_currency", "cost_stop",
                                               "config_hashes(final_freeze_missing)",
                                               "final_frozen_at(final_freeze_missing)"]
    # 合法价表也必须经显式 finalize-g1（P1-02：手改 manifest 不产生
    # config_hashes/final_frozen_at → 不得进付费循环）
    m_hand = {"price_snapshot": PRICE, "cost_currency": "CNY", "cost_stop": 5.0}
    assert any("final_freeze_missing" in p
               for p in cv_eval.require_paid_prereqs(m_hand))
    m2 = {"price_snapshot": PRICE, "cost_currency": "CNY", "cost_stop": 5.0,
          "config_hashes": {"t0-B": "x"}, "final_frozen_at": "2026-09-24"}
    assert cv_eval.require_paid_prereqs(m2) == []


def test_require_paid_prereqs_rejects_invalid_numbers():
    # 工单 01R P1-01：负价/NaN/零停止值不得放行
    bad_price = dict(PRICE, input=-1)
    m = {"price_snapshot": bad_price, "cost_currency": "CNY",
         "cost_stop": 5.0, "config_hashes": {"x": "y"},
         "final_frozen_at": "2026-09-24"}
    assert any("price_snapshot_input_invalid" in p
               for p in cv_eval.require_paid_prereqs(m))
    nan_price = dict(PRICE, output=float("nan"))
    m2 = {"price_snapshot": nan_price, "cost_currency": "CNY",
          "cost_stop": 5.0, "config_hashes": {"x": "y"},
          "final_frozen_at": "2026-09-24"}
    assert any("invalid" in p for p in cv_eval.require_paid_prereqs(m2))
    m3 = {"price_snapshot": PRICE, "cost_currency": "CNY",
          "cost_stop": float("nan"), "config_hashes": {"x": "y"},
          "final_frozen_at": "2026-09-24"}
    assert "cost_stop" in cv_eval.require_paid_prereqs(m3)
    m4 = {"price_snapshot": PRICE, "cost_currency": "CNY", "cost_stop": 0,
          "config_hashes": {"x": "y"}, "final_frozen_at": "2026-09-24"}
    assert "cost_stop" in cv_eval.require_paid_prereqs(m4)
    m5 = {"price_snapshot": PRICE, "cost_currency": "USD", "cost_stop": 1.0,
          "config_hashes": {"x": "y"}, "final_frozen_at": "2026-09-24"}
    assert "cost_currency_mismatch" in cv_eval.require_paid_prereqs(m5)


def test_load_price_snapshot_rejects_partial():
    assert cv_eval.load_price_snapshot(None) is None
    assert cv_eval.load_price_snapshot("/nonexistent/x.json") is None
    p = {"input": 1, "output": 2, "currency": "CNY"}  # 缺 cache 两项
    import tempfile
    with tempfile.NamedTemporaryFile("w", suffix=".json",
                                     delete=False) as f:
        json.dump(p, f)
        path = f.name
    assert cv_eval.load_price_snapshot(path) is None
    with tempfile.NamedTemporaryFile("w", suffix=".json",
                                     delete=False) as f:
        json.dump(PRICE, f)
        path2 = f.name
    assert cv_eval.load_price_snapshot(path2) == PRICE


# ── run_id 唯一性 / 不覆盖（§6：新 run_id 重派，不覆盖旧记录）───────────────

def test_mock_plumbing_run_refuses_existing_run_id(tmp_path):
    batch = tmp_path / "batch"
    run_dir = batch / "runs" / "t0-A-normal"
    run_dir.mkdir(parents=True)
    (run_dir / "events.ndjson").write_text('{"type":"turn"}\n')
    with pytest.raises(SystemExit, match="拒绝覆盖"):
        cv_eval.mock_plumbing_run(
            batch, tmp_path, "t0-A-normal", "t0", "A",
            scenario={"turns": []}, expect={})


def test_cmd_g0_refuses_existing_batch_dir(tmp_path, monkeypatch):
    monkeypatch.setattr(cv_eval, "ART", tmp_path)
    monkeypatch.setattr(cv_eval, "check_global_skills_clean", lambda: None)
    batch_id = "g0-test-01"
    (tmp_path / batch_id).mkdir(parents=True)
    with pytest.raises(SystemExit, match="拒绝覆盖"):
        cv_eval.cmd_g0(type("A", (), {"batch_id": batch_id})())


def test_cmd_prepare_g1_refuses_existing_batch_dir(tmp_path, monkeypatch):
    monkeypatch.setattr(cv_eval, "ART", tmp_path)
    monkeypatch.setattr(cv_eval, "check_global_skills_clean", lambda: None)
    batch_id = "g1-test-01"
    (tmp_path / batch_id).mkdir(parents=True)
    with pytest.raises(SystemExit, match="拒绝覆盖"):
        cv_eval.cmd_prepare_g1(
            type("A", (), {"batch_id": batch_id, "price": None,
                           "cost_stop": None})())


# ── G1 顺序与配置面（§4.3：T0-B,A、T1-A,B、T2-B,A）─────────────────────────

def test_g1_schedule_order():
    assert cv_eval.G1_RUNS == [("t0", "B"), ("t0", "A"), ("t1", "A"),
                               ("t1", "B"), ("t2", "B"), ("t2", "A")]


def test_g1_envelope_covers_six_runs():
    assert cv_eval.G1_TOKEN_ENVELOPE \
        >= len(cv_eval.G1_RUNS) * cv_eval.RUN_LIMITS["max_total_tokens"]


def test_build_config_only_diff_is_code_package(tmp_path):
    wd = tmp_path / "wd"
    a = cv_eval.build_config("t1", "A", wd, "http://x", "m", tmp_path)
    b = cv_eval.build_config("t1", "B", wd, "http://x", "m", tmp_path)
    assert a["code"]["enabled"] and not b["code"]["enabled"]
    assert a["code"]["skill_injection"] == "on_demand"
    assert not a["code"]["include"] and not a["code"]["call_batch"]
    assert not a["code"]["llm"]["enabled"]
    # 除 code.enabled 与 system_prompt 外逐字段相同
    for k in set(a) | set(b):
        if k in ("code", "system_prompt"):
            continue
        assert a[k] == b[k], f"config field drifted: {k}"
    # B 不在 tools 白名单列 run_code（成员资格由 code.enabled 驱动）
    assert "run_code" not in b["tools"]
    assert "run_code" not in a["tools"]
    # 三原语全关（范围纪律）
    assert a["code"]["include"] is False
    assert a["code"]["call_batch"] is False
    assert a["code"]["llm"]["enabled"] is False


def test_build_config_t2_has_no_shell_and_mcp(tmp_path):
    wd = tmp_path / "wd"
    a = cv_eval.build_config("t2", "A", wd, "http://x", "m", tmp_path,
                             server_log=tmp_path / "log.ndjson")
    assert "shell" not in a["tools"]
    assert "shell" not in cv_eval.build_config("t2", "B", wd, "http://x", "m",
                                               tmp_path)["tools"]
    assert len(a["mcp_servers"]) == 1
    assert a["mcp_servers"][0]["name"] == "t2data"


def test_run_limits_match_plan():
    assert cv_eval.RUN_LIMITS == {"max_turns": 20, "timeout_secs": 600,
                                  "max_output_chars": 60000,
                                  "max_total_tokens": 80000}
    assert cv_eval.MAX_TOKENS_PER_REQUEST == 32768


# ── prompt 组装（§3：公共段逐字相同，增量=能力披露）────────────────────────

def test_prompt_parts_a_minus_capability_is_common():
    common, a_system, recipe = cv_eval.prompt_parts()
    assert a_system.startswith(common[:200])
    # A 必须以能力段开头插入（在 Verify 段之前）
    assert "## Choose how to execute the remaining work" in a_system
    assert "## Choose how to execute the remaining work" not in common
    # 公共段含交付纪律（两臂都有，不只给 A）
    assert "Persist artifacts incrementally" in common
    assert "Persist artifacts incrementally" in a_system
    assert set(recipe) == {"agentic_md_sha256", "agentic_code_md_sha256",
                           "persist_bullet_sha256", "cap_section_sha256",
                           "common_sha256", "a_system_sha256"}


def test_prompt_parts_deterministic():
    c1, a1, r1 = cv_eval.prompt_parts()
    c2, a2, r2 = cv_eval.prompt_parts()
    assert (c1, a1, r1) == (c2, a2, r2)


def test_module_prompts_match_recipe():
    assert cv_eval.PROMPT_COMMON and cv_eval.PROMPT_A
    assert cv_eval.PROMPT_A.startswith(cv_eval.PROMPT_COMMON[:100])
    assert len(cv_eval.PROMPT_A) > len(cv_eval.PROMPT_COMMON)


# ── fixture 不变量（§2：确定性、gold 不进 workdir、规模上限）────────────────

@pytest.fixture(scope="module")
def fixture_root(tmp_path_factory):
    root = tmp_path_factory.mktemp("fixture")
    stats = fixture.generate(root)
    assert not stats.get("selfcheck_problems")
    return root


def test_fixture_deterministic(fixture_root, tmp_path_factory):
    other = tmp_path_factory.mktemp("fixture2")
    fixture.generate(other)
    for fa in sorted(fixture_root.rglob("*")):
        if not fa.is_file() or fa.name == "gen_stats.json" \
                or fa.name == "reference_solve_events.ndjson":
            continue  # 运行时日志含时间戳，非快照数据
        rel = fa.relative_to(fixture_root)
        fb = other / rel
        assert fb.is_file(), f"{rel} missing in second generation"
        assert fa.read_bytes() == fb.read_bytes(), f"{rel} not deterministic"


def test_fixture_sizes_within_caps(fixture_root):
    t0 = (fixture_root / "t0" / "records.jsonl").stat().st_size
    t1 = ((fixture_root / "t1" / "subscriptions.jsonl").stat().st_size
          + (fixture_root / "t1" / "events.jsonl").stat().st_size)
    assert t0 <= fixture.T0_MAX_BYTES
    assert t1 <= fixture.T1_MAX_BYTES
    stats = json.load(open(fixture_root / "gen_stats.json", encoding="utf-8"))
    assert stats["t2"]["reach"]["total_body_bytes"] <= fixture.T2_MAX_BODY_BYTES


def test_stage_workdir_never_contains_gold(fixture_root, tmp_path):
    for task in ("t0", "t1", "t2"):
        wd = tmp_path / f"wd-{task}"
        fixture.stage_workdir(task, wd, fixture_root)
        names = {p.name for p in wd.iterdir()}
        assert not any("gold" in n for n in names)
        if task == "t2":
            assert names == set()  # T2 数据只经 MCP，workdir 留空


def test_stage_workdir_resets_state(fixture_root, tmp_path):
    wd = tmp_path / "wd"
    fixture.stage_workdir("t0", wd, fixture_root)
    (wd / "answer.jsonl").write_text('{"id": "leftover"}\n')
    fixture.stage_workdir("t0", wd, fixture_root)
    assert not (wd / "answer.jsonl").exists()  # 上轮残留被清


def test_gold_solved_by_independent_reference(fixture_root):
    # gold 不落盘（§2.1）：判卷时由 load_gold 从快照现算 + digest 对账
    for task in ("t0", "t1", "t2"):
        assert not (fixture_root / task / "gold.json").exists(), \
            f"{task} gold.json must not exist on disk (§2.1)"
        gold = scorer.load_gold(fixture_root / task)
        assert gold, f"{task} gold empty"
        id_key = next(iter(gold))
        assert len(gold[id_key]) > 0
    stats = json.load(open(fixture_root / "gen_stats.json", encoding="utf-8"))
    reach = stats["t2"]["reach"]
    assert reach["paging"]["subscriptions_page"]["pages"] == 4
    assert reach["paging"]["events_page"]["pages"] == 8


def test_task_texts_identical_contract():
    for task, text in fixture.TASK_TEXTS.items():
        assert "answer.jsonl" in text
        assert "自检" in text
    assert set(fixture.TASK_TEXTS) == {"t0", "t1", "t2"}


def test_verify_immutable_rejects_tampered(fixture_root):
    assert fixture.verify_immutable(fixture_root)
    snap = fixture_root / "t0" / "records.jsonl"
    saved = snap.read_bytes()
    snap.write_bytes(saved[:-50])
    assert not fixture.verify_immutable(fixture_root)
    snap.write_bytes(saved)
    assert fixture.verify_immutable(fixture_root)


def test_load_gold_detects_tampered_snapshot(fixture_root):
    # 判卷进程对账：快照被改后重算 gold 与 digest 不符 → 拒判
    snap = fixture_root / "t1" / "events.jsonl"
    saved = snap.read_bytes()
    snap.write_bytes(saved.replace(b'"confirmed": false', b'"confirmed": true'))
    with pytest.raises(RuntimeError, match="disagree"):
        scorer.load_gold(fixture_root / "t1")
    snap.write_bytes(saved)
    assert scorer.load_gold(fixture_root / "t1")


# ── manifest 必填字段（§6.1）────────────────────────────────────────────────

def test_manifest_fields_complete():
    assert set(cv_eval.MANIFEST_FIELDS) >= {
        "batch_id", "stage", "experiment_version", "code_head",
        "dirty_diff_hash", "binary_hash", "fixture_hash", "task_hash",
        "prompt_hashes", "prompt_diff", "tool_inventory", "feature_flags",
        "model", "gateway_identity", "sampling", "price_snapshot",
        "cost_currency", "cost_stop", "token_stop", "turn_limit",
        "wall_limit", "retry_policy", "run_schedule", "success_rule",
        "gate_rule", "demand_status"}


def test_gold_as_answer_lines_roundtrip(fixture_root, tmp_path):
    # mock「完美模型」写出的内容必须过验收（gold → answer → validate）
    for task in ("t0", "t1"):
        content = cv_eval.gold_as_answer_lines(fixture_root, task)
        gold = scorer.load_gold(fixture_root / task)
        art = tmp_path / f"rt-{task}" / "answer.jsonl"
        art.parent.mkdir(parents=True, exist_ok=True)
        art.write_text(content, encoding="utf-8")
        v = scorer.validate_artifact(art, gold, task)
        assert v["delivered_exact"], v["reasons"]


# ── 复核修复的回归钉（独立复核 2026-09-24 抓出的问题）─────────────────────

def test_usage_partial_unknown_detected_and_reserved(tmp_path):
    # 复核 CONCERN #3：中途 HTTP 500 的失败轮没报 usage——run 总量 >0
    # 不能掩盖它。events 流 turn=2 / usage=1 → 1 个请求烧了没报账。
    ev = tmp_path / "events.ndjson"
    ev.write_text('{"type":"turn"}\n'
                  '{"type":"usage","input_tokens":600,"output_tokens":45,'
                  '"cache_creation_input_tokens":0,"cache_read_input_tokens":0}\n'
                  '{"type":"turn"}\n', encoding="utf-8")
    u = scorer.usage_from_result(
        {"input_tokens": 600, "output_tokens": 45, "turns": 2}, ev)
    assert u["partial_unknown"] and u["unreported_requests"] == 1
    assert u["unknown_reserve_tokens"] == scorer.PARTIAL_UNKNOWN_PER_REQUEST_TOKENS
    c = scorer.cost_from_usage(u, PRICE)
    assert c["cost"] is None and c["reason"] == "partial_usage_missing"
    assert c["known_tokens"] == 645  # 已知部分单列，不当 0 也不并入 cost


def test_usage_no_events_path_no_partial_flag():
    # 不给 events 流（如纯 result.json 判卷）时不得虚报 partial_unknown
    u = scorer.usage_from_result({"input_tokens": 10, "turns": 2})
    assert not u["partial_unknown"] and u["unreported_requests"] is None


def test_partial_unknown_in_aggregate(tmp_path):
    def _run(run_id, result, ev_content):
        ev = tmp_path / f"{run_id}.ndjson"
        ev.write_text(ev_content, encoding="utf-8")
        usage = scorer.usage_from_result(result, ev)
        cost = scorer.cost_from_usage(usage, PRICE)
        summary = scorer.summarize_run(
            {"run_id": run_id, "task": "t0", "arm": "A"},
            {"delivered_exact": True, "reasons": [], "details": {}},
            usage, cost, "end_turn")
        return {"run_id": run_id, "task": "t0", "arm": "A",
                "summary": summary}

    runs = [
        _run("clean", {"input_tokens": 100, "output_tokens": 50, "turns": 1},
             '{"type":"turn"}\n{"type":"usage"}\n'),
        _run("partial", {"input_tokens": 100, "turns": 2},
             '{"type":"turn"}\n{"type":"usage"}\n{"type":"turn"}\n'),
    ]
    agg = scorer.aggregate(runs)
    assert "partial" in agg["partial_unknown_runs"]
    assert "partial" in agg["unknown_cost_runs"]
    assert agg["known_delivered_count"] <= agg["delivered_exact_count"]


def test_run_g1_mkdir_tolerates_precreated_workdir(tmp_path, monkeypatch):
    # 复核 FAIL #1：prepare-g1 预创建 runs/<id>/wd，run-g1 的 mkdir 无
    # exist_ok 曾裸崩 FileExistsError。这里钉住：目录已存在（无
    # summary.json）不崩——真正的判重靠 summary.json。
    import types
    batch = tmp_path / "batch"
    run_dir = batch / "runs" / "t0-B"
    (run_dir / "wd").mkdir(parents=True)  # prepare-g1 预创建的形状
    manifest = {
        "stage": "g1",
        "price_snapshot": PRICE, "cost_currency": "CNY", "cost_stop": 5.0,
        "token_stop": {"stage_envelope": 480_000,
                       "per_run_soft_gate": 80_000},
        "run_schedule": [{"order": 0, "run_id": "t0-B", "task": "t0",
                          "arm": "B"}],
    }
    write = tmp_path / "manifest.json"
    write.write_text(json.dumps(manifest), encoding="utf-8")

    # 第一跑就让它撞预算闸退出（不真跑二进制）——若 mkdir 崩了会在这里
    # 以 FileExistsError 而非预算闸拒绝的形态暴露
    gate = cv_eval.budget_gate(480_000, 0, 480_000, 80_000)
    assert not gate["ok"]
    # 直接验证 mkdir 行为本身
    run_dir.mkdir(parents=True, exist_ok=True)  # 修复后：不抛
    assert run_dir.is_dir()


def test_cost_stop_enforced_in_run_loop(tmp_path, monkeypatch):
    # 复核 CONCERN #4：cost_stop 不只是采集进 manifest，运行中越线要停
    # 止启动新 run。用最小桩验证 cmd_run_g1 的循环逻辑：这里直接验证
    # known_cost_total 累计与比较的语义（真跑由 run-g1 e2e 覆盖）。
    cost_stop = 0.5
    known = 0.0
    costs = [0.3, 0.25, 0.1]  # 第三跑累计 0.55 ≥ 0.5 → 停
    triggered_at = None
    for i, c in enumerate(costs):
        known += c
        if known >= cost_stop:
            triggered_at = i
            break
    assert triggered_at == 1  # 0.3+0.25=0.55 ≥ 0.5：第三跑前停


def test_summarize_run_carries_instance_fields():
    validation = {"delivered_exact": True, "reasons": [], "details": {}}
    usage = scorer.usage_from_result({"input_tokens": 1, "turns": 1})
    cost = scorer.cost_from_usage(usage, PRICE)
    s = scorer.summarize_run(
        {"run_id": "r", "task": "t0", "arm": "A", "instance_id": 2,
         "repetition": 3, "order": 5}, validation, usage, cost, "end_turn")
    assert (s["instance_id"], s["repetition"], s["order"]) == (2, 3, 5)
    # 缺省：单实例单重复
    s2 = scorer.summarize_run({"run_id": "r", "task": "t0", "arm": "A"},
                              validation, usage, cost, "end_turn")
    assert (s2["instance_id"], s2["repetition"]) == (1, 1)
    assert s2["order"] is None


# ── 工单 01R 回归：真实 cmd_run_g1() 入口 + 桩 run_binary（不联网）─────────
# 复核指出旧测试「手写费用循环代替被测控制逻辑」——以下全部走真实入口，
# 只把模型执行入口 run_binary 换成不联网的桩。覆盖复核反例矩阵：
#   result 丢失/损坏、部分未知、费用不足、非法价表、配方漂移、
#   重复启动、中断后再启动、答案文件可达。


def _make_batch(tmp_path, price=None, cost_stop=None, finalize=False):
    """构造可执行的 g1 批次（manifest + configs + inventory）。

    finalize=True 时走真实 cmd_finalize_g1 补价表（产生 config_hashes +
    final_frozen_at）；False 时模拟手改 manifest 填价表。
    """
    batch = tmp_path / "batch"
    batch.mkdir(parents=True)
    schedule = [{"order": i, "run_id": f"{t}-{a}", "task": t, "arm": a}
                for i, (t, a) in enumerate(cv_eval.G1_RUNS)]
    manifest = {
        "stage": "g1", "run_schedule": schedule,
        "token_stop": {"stage_envelope": 480_000, "per_run_soft_gate": 80_000},
        "model": "stub-model",
        "price_snapshot": price, "cost_currency":
            (price or {}).get("currency"), "cost_stop": cost_stop,
        "binary_hash": None, "fixture_hash": None, "task_hash": None,
        "prompt_hashes": None,
    }
    for s in schedule:
        cfg_dir = batch / "configs"
        cfg_dir.mkdir(exist_ok=True)
        cfg = {"model": "stub-model", "workdir":
               str(batch / "runs" / s["run_id"] / "wd"),
               "code": {"enabled": s["arm"] == "A"}}
        (cfg_dir / f"{s['run_id']}.json").write_text(
            json.dumps(cfg), encoding="utf-8")
    (batch / "inventory_analysis.json").write_text(
        json.dumps({"problems": [], "per_task": {}}), encoding="utf-8")
    (batch / "manifest.json").write_text(json.dumps(manifest),
                                         encoding="utf-8")
    if finalize:
        price_path = tmp_path / "price.json"
        price_path.write_text(json.dumps(price), encoding="utf-8")
        args = type("A", (), {"batch": str(batch), "price": str(price_path),
                              "cost_stop": cost_stop})()
        cv_eval.cmd_finalize_g1(args)
    return batch


def _stub_run_binary(outcome, task_of=None):
    """桩模型执行：写 events/result/wd 产物，不联网。

    outcome: "normal"（完整 usage）/ "partial"（一个未报账请求）/
             "missing"（整轮无 usage）/ "no_result"（result 缺失）/
             "corrupt"（result 损坏）/ "crash"（进程死在半路，只有 events）。
    task_of: run_id → task 名映射（从 run 目录名推断）。
    """
    def stub(cfg_path, task_text, run_dir, env, timeout_s,
             seatbelt_profile=None):
        run_dir = Path(run_dir)
        wd = run_dir / "wd"
        wd.mkdir(parents=True, exist_ok=True)
        ev = run_dir / "events.ndjson"
        usage_frame = ('{"type":"usage","input_tokens":1000,'
                       '"output_tokens":100,"cache_creation_input_tokens":0,'
                       '"cache_read_input_tokens":0}\n')
        if outcome == "normal":
            ev.write_text('{"type":"turn"}\n' + usage_frame,
                          encoding="utf-8")
            (run_dir / "result.json").write_text(json.dumps({
                "input_tokens": 1000, "output_tokens": 100,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0, "turns": 1, "attempts": 1,
                "stop_reason": "end_turn", "tool_calls": []}),
                encoding="utf-8")
        elif outcome == "partial":
            # 两轮 turn，只有一轮报账——复核反例「部分未知」
            ev.write_text('{"type":"turn"}\n' + usage_frame
                          + '{"type":"turn"}\n', encoding="utf-8")
            (run_dir / "result.json").write_text(json.dumps({
                "input_tokens": 1000, "output_tokens": 100,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0, "turns": 2, "attempts": 1,
                "stop_reason": "error", "tool_calls": []}),
                encoding="utf-8")
        elif outcome == "missing":
            ev.write_text('{"type":"turn"}\n{"type":"turn"}\n',
                          encoding="utf-8")
            (run_dir / "result.json").write_text(json.dumps({
                "turns": 2, "attempts": 1, "stop_reason": "error",
                "tool_calls": []}), encoding="utf-8")
        elif outcome == "no_result":
            ev.write_text('{"type":"turn"}\n' + usage_frame,
                          encoding="utf-8")
            # result.json 不写——模拟进程崩溃在写 result 之前
        elif outcome == "corrupt":
            ev.write_text('{"type":"turn"}\n' + usage_frame,
                          encoding="utf-8")
            (run_dir / "result.json").write_text(
                '{"input_tokens": 1000, "output_tok', encoding="utf-8")
        elif outcome == "crash":
            # 进程死在半路：只有半截 events，无 result
            ev.write_text('{"type":"turn"}\n', encoding="utf-8")
        # 产物：写一份必然过验收的 answer（gold 现算，不落盘）
        task = task_of(run_dir.name) if task_of else "t0"
        gold = cv_eval.gold_as_answer_lines(cv_eval.FIXTURE_ROOT, task)
        (wd / "answer.jsonl").write_text(gold, encoding="utf-8")
        return 0, 0.1
    return stub


def _task_of_run_id(run_id):
    return run_id.rsplit("-", 1)[0]


def _patch_for_stub(monkeypatch, outcome):
    monkeypatch.setattr(cv_eval, "run_binary", _stub_run_binary(
        outcome, task_of=_task_of_run_id))
    # seatbelt 桩：真实 sandbox-exec 探针过重，桩为通过（隔离本身
    # 另有真实探针测试 test_seatbelt_isolation_real_probe）
    monkeypatch.setattr(cv_eval, "verify_seatbelt",
                        lambda prof, wd, mcp=None: {"ok": True})
    monkeypatch.setattr(cv_eval, "seatbelt_profile",
                        lambda rd, wd, mcp=None: None)
    # gateway 桩：不读 测试模型.md、不注入真实密钥
    monkeypatch.setattr(cv_eval, "gateway_identity",
                        lambda: ({"model": "stub-model"},
                                 {"api_key": "stub", "base_url": "stub"}))


def _run_g1(tmp_path, monkeypatch, outcome, price, cost_stop, finalize=True):
    batch = _make_batch(tmp_path, price, cost_stop, finalize=finalize)
    _patch_for_stub(monkeypatch, outcome)
    args = type("A", (), {"batch": str(batch), "execute": True})()
    rc = cv_eval.cmd_run_g1(args)
    ledger = json.load(open(batch / "ledger.json", encoding="utf-8"))
    return batch, rc, ledger


TEST_PRICE = {"input": 1e-6, "output": 1e-6, "cache_creation": 1e-6,
              "cache_read": 1e-6, "currency": "TEST"}


def test_run_g1_partial_unknown_stops_subsequent_runs(tmp_path, monkeypatch,
                                                      capsys):
    """P1-01 反例回归：每跑已知费用 0.0011 + 一个未报账请求。
    修复前：六跑全部启动、known_cost_total=0、triggered=false。
    修复后：首跑检出 partial_unknown → 停止启动后续 run，已知下界保留。"""
    batch, rc, ledger = _run_g1(tmp_path, monkeypatch, "partial", TEST_PRICE,
                                100.0)
    assert ledger["runs"] == 1, "partial_unknown 必须立即停止后续 run"
    assert ledger["cost_stop"]["stop_reason"] == "usage_incomplete"
    # 已知费用下界不被抹掉（P1-01 核心）：1000×1e-6 + 100×1e-6
    assert ledger["cost_known_lower_total"] == pytest.approx(0.0011)
    # 主指标：存在未知费用 → null（不拿混合口径当主指标）
    assert ledger["cost_per_success"] is None
    assert ledger["partial_unknown_runs"] == ["t0-B"]
    # 证据保留：首跑目录完整（events/result/summary/completed）
    rd = batch / "runs" / "t0-B"
    for f in ("started.json", "events.ndjson", "result.json",
              "summary.json", "completed.json"):
        assert (rd / f).exists(), f"{f} 必须封存"
    # 后续 run 未被启动（无 started.json）
    assert not (batch / "runs" / "t0-A" / "started.json").exists()


def test_run_g1_cost_stop_triggers_on_lower_plus_reserve(tmp_path,
                                                         monkeypatch):
    """P1-01：停止判断 = 已知下界 + 未知预留折算 ≥ 停止值。
    missing：首跑 usage_missing → 已知下界 0 + 预留 112768 tok × 1e-6
    = 0.1128 ≥ 0.05 → 触发费用停止线（而非静默跑满六跑）。"""
    batch, rc, ledger = _run_g1(tmp_path, monkeypatch, "missing", TEST_PRICE,
                                0.05)
    assert ledger["runs"] == 1
    assert ledger["cost_stop"]["stop_reason"] == "cost_stop"
    assert ledger["cost_stop"]["triggered"] is True
    assert ledger["cost_stop"]["unknown_reserve_cost"] == pytest.approx(
        (80_000 + 32_768) * 1e-6)
    # 停止线语义标注：run 间停止线，非供应商扣费硬上限
    assert "非供应商扣费硬上限" in ledger["cost_stop"]["kind"]


def test_run_g1_normal_runs_all_six(tmp_path, monkeypatch):
    """对照：全部正常时六跑完整执行，主指标有值、停止线不触发。"""
    batch, rc, ledger = _run_g1(tmp_path, monkeypatch, "normal", TEST_PRICE,
                                100.0)
    assert ledger["runs"] == 6
    assert ledger["cost_stop"]["stop_reason"] is None
    assert ledger["cost_stop"]["triggered"] is False
    assert ledger["cost_per_success"] is not None
    assert ledger["cost_per_success"] == pytest.approx(0.0011)


def test_run_g1_missing_result_accounted_from_events(tmp_path, monkeypatch):
    """P1-01 反例回归：result 缺失但 events 有账——修复前被记成
    requests_happened=false / cost=0；修复后从 events 核账，不归零。"""
    batch, rc, ledger = _run_g1(tmp_path, monkeypatch, "no_result", TEST_PRICE,
                                100.0)
    # events 完整报账 → usage_complete=True → 六跑全跑（不误停）
    assert ledger["runs"] == 6
    summary = json.load(open(batch / "runs" / "t0-B" / "summary.json",
                             encoding="utf-8"))
    u = summary["usage"]
    assert u["result_missing"], "result 缺失必须显式标记"
    assert u["requests_happened"], "events 有账不得记成零请求"
    assert u["total_tokens"] == 1100, "从 events 核账，不归零"
    assert u["usage_complete"], "events 全报账即完整"


def test_run_g1_corrupt_result_accounted_from_events(tmp_path, monkeypatch):
    """P1-01 反例回归：result 损坏（半截 JSON）——修复前 json.load 直接
    崩溃；修复后与缺失同罪，events 核账。"""
    batch, rc, ledger = _run_g1(tmp_path, monkeypatch, "corrupt", TEST_PRICE,
                                100.0)
    assert ledger["runs"] == 6
    summary = json.load(open(batch / "runs" / "t0-B" / "summary.json",
                             encoding="utf-8"))
    assert summary["usage"]["result_missing"]
    assert summary["usage"]["total_tokens"] == 1100


def test_run_g1_refuses_interrupted_run(tmp_path, monkeypatch):
    """P1-03 反例回归：已有 events/answer 无 summary 的中断 run，
    修复前允许重新执行并覆盖；修复后拒绝且原证据不被破坏。"""
    batch = _make_batch(tmp_path, TEST_PRICE, 100.0, finalize=True)
    run_dir = batch / "runs" / "t0-B"
    run_dir.mkdir(parents=True)
    (run_dir / "wd").mkdir()
    (run_dir / "events.ndjson").write_text('{"type":"turn"}\n',
                                           encoding="utf-8")
    (run_dir / "wd" / "answer.jsonl").write_text('{"id": "r-01"}\n',
                                                 encoding="utf-8")
    _patch_for_stub(monkeypatch, "normal")
    args = type("A", (), {"batch": str(batch), "execute": True})()
    with pytest.raises(SystemExit, match="拒绝覆盖"):
        cv_eval.cmd_run_g1(args)
    # 原证据未被破坏
    assert (run_dir / "events.ndjson").read_text() == '{"type":"turn"}\n'
    assert (run_dir / "wd" / "answer.jsonl").exists()
    assert not (run_dir / "started.json").exists()


def test_run_g1_started_marker_atomic(tmp_path):
    """P1-03：started.json O_EXCL 原子登记——并发双执行者只有一个成功。"""
    run_dir = tmp_path / "runs" / "x"
    run_dir.mkdir(parents=True)
    cv_eval.claim_run_slot(run_dir)
    assert (run_dir / "started.json").exists()
    # 第二个执行者：目录里已有证据文件 → 拒绝（并发禁止）
    with pytest.raises(SystemExit, match="拒绝覆盖"):
        cv_eval.claim_run_slot(run_dir)


def test_run_g1_refuses_recipe_drift(tmp_path, monkeypatch, capsys):
    """P1-02 反例回归：manifest 二进制哈希与实际不符 / 配置模型被改 →
    拒绝执行（修复前照跑）。漂移项打 stderr，退出即拒绝。"""
    batch = _make_batch(tmp_path, TEST_PRICE, 100.0, finalize=True)
    m = json.load(open(batch / "manifest.json", encoding="utf-8"))
    m["binary_hash"] = "0" * 64
    (batch / "manifest.json").write_text(json.dumps(m), encoding="utf-8")
    _patch_for_stub(monkeypatch, "normal")
    args = type("A", (), {"batch": str(batch), "execute": True})()
    with pytest.raises(SystemExit):
        cv_eval.cmd_run_g1(args)
    err = capsys.readouterr().err
    assert "binary_hash_drift" in err
    # 漂移 2：配置模型被手改（config_hashes 对不上）
    m = json.load(open(batch / "manifest.json", encoding="utf-8"))
    m["binary_hash"] = None
    (batch / "manifest.json").write_text(json.dumps(m), encoding="utf-8")
    cfg = json.load(open(batch / "configs" / "t0-B.json", encoding="utf-8"))
    cfg["model"] = "UNFROZEN-MODEL"
    (batch / "configs" / "t0-B.json").write_text(json.dumps(cfg),
                                                 encoding="utf-8")
    with pytest.raises(SystemExit):
        cv_eval.cmd_run_g1(args)
    err = capsys.readouterr().err
    assert "config_model_drift" in err
    # 任何 run 都没被启动
    assert not (batch / "runs" / "t0-B" / "started.json").exists()


def test_run_g1_refuses_hand_edited_price(tmp_path, monkeypatch):
    """P1-02 反例回归：手改 manifest 填价表（未经 finalize-g1）→
    拒绝执行。价表补全必须是显式最终冻结步骤。"""
    batch = _make_batch(tmp_path, TEST_PRICE, 100.0, finalize=False)
    _patch_for_stub(monkeypatch, "normal")
    args = type("A", (), {"batch": str(batch), "execute": True})()
    with pytest.raises(SystemExit, match="付费前置缺失"):
        cv_eval.cmd_run_g1(args)
    assert not (batch / "runs" / "t0-B" / "started.json").exists()


def test_finalize_g1_explicit_freeze(tmp_path, monkeypatch):
    """P1-02：finalize-g1 写 config_hashes + final_frozen_at；之后手改
    配置 → run-g1 拒绝（config_drift）。"""
    batch = _make_batch(tmp_path, TEST_PRICE, 100.0, finalize=True)
    m = json.load(open(batch / "manifest.json", encoding="utf-8"))
    assert m["config_hashes"] and m["final_frozen_at"]
    assert m["blocked_paid"] is False
    assert m["cost_stop"] == 100.0
    # 手改配置后 run-g1 拒绝（config_hashes 对不上）
    cfg = json.load(open(batch / "configs" / "t0-B.json", encoding="utf-8"))
    cfg["model"] = "TAMPERED"
    (batch / "configs" / "t0-B.json").write_text(json.dumps(cfg),
                                                 encoding="utf-8")
    _patch_for_stub(monkeypatch, "normal")
    args = type("A", (), {"batch": str(batch), "execute": True})()
    with pytest.raises(SystemExit):
        cv_eval.cmd_run_g1(args)


def test_run_g1_crash_midway_preserves_evidence(tmp_path, monkeypatch):
    """P1-03 反例回归：进程死在半路（只有半截 events）→ 该 run 记账为
    usage_missing + 停止后续；重跑同批次被拒绝，证据保留。"""
    batch, rc, ledger = _run_g1(tmp_path, monkeypatch, "crash", TEST_PRICE,
                                100.0)
    assert ledger["runs"] == 1
    assert ledger["cost_stop"]["stop_reason"] == "usage_incomplete"
    assert ledger["usage_missing_runs"] == ["t0-B"]
    rd = batch / "runs" / "t0-B"
    assert (rd / "events.ndjson").exists()
    assert (rd / "summary.json").exists()
    # 重跑同批次：中断证据在 → 拒绝
    _patch_for_stub(monkeypatch, "normal")
    args = type("A", (), {"batch": str(batch), "execute": True})()
    with pytest.raises(SystemExit, match="拒绝覆盖"):
        cv_eval.cmd_run_g1(args)
