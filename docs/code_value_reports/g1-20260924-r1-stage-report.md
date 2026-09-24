# 工单 02 阶段报告——G1 六跑（T2 重跑对待放行）

日期：2026-09-24。批次：`g1-20260924-r1`（六跑）+ `g1-20260924-r1-t2retry`（T2 重跑对，已备好未执行）。
experiment_version：`cv-eval-v1`。HEAD：`61adafe`（T2 Seatbelt 修复后）。

## 这批要改变的唯一决定

run_code 在三题上的自然 A/B 配对是否有可检验信号（方案 §4.3 分流表）。

## 已执行 / 未执行

- **已执行 6 runs**（付费，真实网关 vol-glm-5.3-flash）：t0-B、t0-A、t1-A、t1-B 全部正常完成；**t2-B、t2-A 死于装置错误**（Seatbelt 挡死 MCP 服务器，零请求零 token）——按方案 §4.3 第一行处置：原记录保留 **invalid**，装置已修（`61adafe`），重跑对已备好。
- **未执行**：T2 重跑对（2 runs，消耗诊断储备）。付费执行被本机权限系统拦下，待放行。
- **诊断储备消耗**：2/4（T2 重跑对占位；跑完即 4/4 用满或按分流表另议）。

## 费用

| 口径 | 值 |
|---|---|
| 已知费用下界合计（六跑全账） | **0.0280 CNY** |
| 四跑有效口径 cost_per_success | 0.0070 CNY/成功 |
| 六跑全账口径 cost_per_success（含 2 个 0 成本 invalid） | 0.0070 CNY（ledger 值，T2 重跑后需重算） |
| 未知预留 | 0（四跑账目全部完整：usage_complete，无 partial） |
| 停止线 | 1.34 CNY，未触发（0.0280 + 0 < 1.34） |
| 研发总消耗 | 01R 修复零付费；T2 装置修复零付费（g0 回归验证） |

## 软 token 闸 / 超额 / 在途风险

四跑 tokens：8,329 / 12,991 / 12,462 / 11,449——全部远低于 80K/run 软闸，闸未触发。在途风险上界 112,768 tok/run（ledger 已单列，未发生）。

## 结果（逐任务 A/B，不写总胜场）

| 任务/run_id | A/B | exact 交付 | 结束原因 | 实际路线/code 有效承接 | 费用 CNY | 墙钟 | 失败类别 |
|---|---|---|---|---|---|---|---|
| t0-B | B | ✅ | end_turn | shell×2 直读 records.jsonl | 0.0053 | 25.3s | — |
| t0-A | A | ✅ | end_turn | shell×2；**run_code 0 次调用**（未采用） | 0.0095 | 28.9s | — |
| t1-A | A | ✅ | end_turn | shell×2；**run_code 0 次调用**（未采用） | 0.0066 | 20.0s | — |
| t1-B | B | ✅ | end_turn | shell×3 | 0.0066 | 30.6s | — |
| t2-B | B | ❌ | no_result | 装置错误：MCP 被 Seatbelt 挡死 | 0（invalid） | — | **装置失效** |
| t2-A | A | ❌ | no_result | 同上 | 0（invalid） | — | **装置失效** |

**关键观察（T0/T1）**：A 臂 run_code **零采用**——两臂实际路线完全相同（shell 直读数据文件写答案）。A 臂没有因 code 可用而多花钱（t0-A 0.0095 vs t0-B 0.0053 的差异来自输出长度，不是工具路线）；t1 两臂费用几乎相同（0.0066 vs 0.0066）。**在 T0/T1 上「code 可用」没有改变模型行为**——这两题 shell 一条路就够，符合任务设计（数据是本地文件，shell 天然可达）。

**T2（待重跑）**：T2 是唯一「数据只经 MCP 分页、shell 不可达」的任务——正是 run_code 设计要证明价值的场景（guest 循环分页 vs 模型手工翻页）。原记录 invalid，重跑对是本批次的关键数据点。

## 装置 / 产品 / 基础设施问题

- **装置错误（已修，`61adafe`）**：01R 的 Seatbelt deny 面（scripts/ + fixture/ 整目录）挡死 T2 的 MCP 服务器。根因：T2 的 MCP 路径从未进过零付费联通——g0 的四个 mock 场景全是 t0/t1。修复：literal allow 洞（服务器脚本/解释器/数据文件）+ mcp_readable 探针 + **g0 补 t2-A-mcp-plumbing 场景**（真 MCP 在 Seatbelt 下握手、模型经 mcp_t2data_* 展示名调分页、写出答案，全绿）。
- **产品问题**：无（四跑全部正常交付）。
- **基础设施**：付费执行被本机权限系统拦（Production Deploy 分类），需人工放行。

## 本轮是否改过配方

**改过装置，未改配方**。A/B 配方（提示词/工具/schema/模型/limits）与冻结时逐字一致（verify_frozen_recipe 全过）。旧版本数据位置：`g1-20260924-r1/runs/t2-{B,A}`（invalid 原记录保留）。

## 客户需求证据状态

`synthetic / demand_unverified`（方案 §2：无客户材料，不进客户价值宣称）。

## 复核人 / 未验证范围

- 复核工件：`g1-20260924-r1/`（六跑全套 events/result/summary/usage/validation/completed）、`g0-20260924-t2-seatbelt-regression/`（装置修复的零付费验证）、71 例回归。
- 未验证：T2 重跑对（待放行）；真实模型效果结论（装置合格 ≠ 特性价值成立）。

## 建议

**修一个问题 → 放行 T2 重跑对**（三选一里的「修一个问题」已完成，剩执行）：

```
python3 scripts/code_value_eval.py run-g1 --batch artifacts/code_value/g1-20260924-r1-t2retry --execute
```

- 可证伪假设：T2 上 A 臂（run_code 可用）经 guest 循环分页读 MCP 数据，与 B 臂（模型手工翻页）相比交付质量相同或更好、费用更低或相当（方案 §4.3 分流表第 2/3 行）。
- 具体新增 runs：2（t2-B-retry1 / t2-A-retry1，配方同源已冻结）。
- 额度：预计 ~0.03-0.05 CNY（按 t0/t1 水位）；停止线 1.34 CNY。
- 停止条件：装置再失效即停（不再消耗储备）；两臂都成功且接近 → 按分流表「停止此任务族扩量」；A 成功 B 失败 → 标「预算内完成性信号」。

跑完 T2 对后：六跑数据齐（T2 用重跑对、原对标 invalid），出最终分流建议，停在交接点。诊断储备不再自行启动，G2 不开启。
