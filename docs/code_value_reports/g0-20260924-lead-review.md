# G0 评测负责人复核：暂不放行 G1，补四项边界

日期：2026-09-24。复核 HEAD：`c54a19dbe137238550cc92c18727b514110a699a`。
范围：当前 G0/G1 装置、封存工件与零网络离线反例；未改产品实现、未发起付费模型请求。

## 1. 裁决

**G0 已完成主要装置工作，但“装置侧无阻塞、只差价表即可开跑”不成立。当前不放行 G1。**
只派四项定向修补与回归，不扩场景、模型、样本或预算，不要求重跑全部 Rust 矩阵。
原始报告及批次保留；`g1-20260924-sealed-r2` 不直接补金额开跑，修补后创建新版本批次。

本次实测：`PYTHONDONTWRITEBYTECODE=1 /Users/chenpu/miniconda3/bin/python3 -m pytest scripts/tests/test_code_value_eval.py -q -p no:cacheprovider` → **58 passed**。
当前 release 二进制 SHA-256 与 r2 manifest 相同；本文不指控当前二进制已漂移，问题是执行前缺少拒绝漂移的检查。
当前 G1 r2 无 summary，价格、币种与费用停止值仍为 null。CI-only 检查不作为此次装置放行的新增障碍。

## 2. 已复现的四项阻塞

### P1-01：未知费用未进停止判断，丢失 result 时甚至被算成零

位置：`scripts/code_value_eval.py:994-1046`；`scripts/code_value_score.py:192-245,251-279,309-332`。

离线桩调用真实 `cmd_run_g1()`，替换模型进程入口，不调用网关。使用仅用于测试的单价与 TEST 币种：

- 每 run 已知 input=10,000，再有一个未报告 usage 的请求；已知费用=0.01，`cost_stop=0.001`。
- 六个 run **全部获准启动**；最终 `known_cost_total=0.0`，`triggered=false`。
- 原因：`partial_unknown` 使整次 cost=null，runner 只累加 `cost_known=True` 的 run，已知费用和未知风险都不进入货币停止判断。
- 另一个探针：events 已有一条 turn 与一条 usage（input=1234/output=56），但 result 缺失；得到 `requests_happened=false,total_tokens=0,cost_known=true,cost=0`。
- 一次已知费用=1 的成功加一次未知费用成功，聚合仍返回 `cost_per_success=0.5`。它不是全部尝试的成功成本，最多是需要明确命名的下界。

最小修法：

1. 已知费用与费用完整性分离；部分未知仍保留已知费用下界。缺失/损坏 result 使用事件证据核账；不能确认时标未知，不归零。
2. 首轮采用最简单的保守行为：任一 usage_missing/partial_unknown 或无法核账即停止启动后续 run、封存并回报。无需为此新增计费代理。
3. `cost_per_success` 有未知消耗时为 null；若提供上下界，用不同字段明确表示。
4. 启动前费用检查纳入已知费用、未知预留和下一 run 风险；明确是 run 间停止线，不承诺供应商硬扣费上限。
5. 价表及停止值校验有限数、非负/正值、完整项与币种一致。当前 `require_paid_prereqs()` 连 `price_snapshot={input:-1}, cost_stop=NaN` 也返回无缺项。
6. 预留上界须包含输入与输出、适用重试；不能将只有 32768 输出 token 的数字称为完整在途上界。

### P1-02：封存目前是记录，没有执行时验证

位置：`scripts/code_value_eval.py:942-958,973-1025`。

在临时复制的 r2 配方中，将 manifest 二进制哈希改为全零、配置模型改成 `UNFROZEN-MODEL`，真实入口仍调用了桩执行器并传入变更后的模型。
此外 prepare-g1 的 inventory problems 仅打印；`blocked_paid` 只由价格字段决定。执行时任务文本来自当前模块常量，fixture 来自公共路径，不核对封存内容。

最小修法：

- 在任何任务进程/模型请求启动前核对执行相关源码、二进制、fixture、任务文本、工具/schema/skill、配置和 prompt 的内容哈希。记录过哈希不等于通过校验。
- inventory 不合法直接阻止冻结可执行批次。执行重新验证，不只相信 manifest 的布尔标记。
- 验证六跑顺序与允许的开关、模型、预算；价格补全是显式最终冻结步骤，不能靠任意手改清单绕过验证。
- 文档提交或无关旧目录不必因 HEAD 不同一律拒绝；关键是绑定实际执行内容，保存可重建的相关源码快照/版本。

### P1-03：没有 summary 的中断运行会被覆盖

位置：`scripts/code_value_eval.py:1005-1025`；`scripts/code_value_fixture.py:336-345`。

临时构造已有 events 和 answer、但无 summary 的 run：入口允许再次启动，`stage_workdir()` 删除原产物，事件文件被覆盖。
这正是“进程已请求/已产生工作，但在写 summary 前崩溃”的状态，不是尚未启动的空目录。

最小修法：启动前原子创建 started 状态；存在 started/events/result/产物而未正常封存时拒绝覆盖，标 interrupted/unknown 并纳入风险账。
准备阶段的空 workdir 可以存在；started 与 completed 必须分开。新尝试用新 ID，原证据不可变。
异常、超时、判卷异常也必须先落下终态/账目；并发启动同一批次只允许一个执行者。

### P1-04：gold_digest 仍含答案明文，shell 可以直接取得

位置：`scripts/code_value_fixture.py:377-411`；工件 `artifacts/code_value/fixture/gen_stats.json`。

- `gold_digest.t0.ids` 是完整目标 ID 列表；t1/t2 的 `user_ids` 也是目标集合明文，并不是摘要哈希。
- 本次从 G1 t0-B 的 workdir 通过普通 shell 读取 t0 的三个答案 ID，完全不读任务 records 文件，写出产物后验收器给 `delivered_exact=true`。
- `restrict_to_workdir` 不约束 shell 的整个 OS 读权限；现有 G0 场景/历史 answer 工件也可能包含同题答案，必须一并处理。

最小修法：gen_stats 只保留计数与规范化结果的不可逆摘要；judge 在任务产物封存后重算。
历史 mock 场景与答案不能处于评测 shell 可读面。采用现有受限执行环境/独立身份，或其他能实际验证不可达的最小方案；只换目录、删一个 gold.json 不算修复。
若当前宿主无法提供这项隔离，明确回报阻塞，不把答案可达的运行当作公平盲测。

## 3. 为什么 58 例全绿没有覆盖这些问题

`scripts/tests/test_code_value_eval.py:619-644` 直接验证 mkdir，而没有调用 runner；`:647-660` 手写一个费用循环，而没有调用 `cmd_run_g1()`。
它们不能检测 runner 入口与费用判断的缺陷。新回归必须调用实际入口；可以替换 `run_binary` 为不会联网的桩，不能把待检控制逻辑也替换掉。
本次探针位于 `/tmp/code-value-g0-review/probe.py`，可在当前工作区离线重放；该临时文件不是长期封存材料，团队须将相应反例转为仓库回归测试。

## 4. 工单 01R 的最小验收

1. 修上述四项，补真实入口离线回归，原 58 例继续通过。无需付费调用。
2. 同步核验：result 丢失/损坏、部分未知、费用不足、非法价表、配方漂移、重复启动、中断后再启动、答案文件可达。
3. 用新 batch_id 做一次必要的零付费封存验证；不覆盖 manual/sealed/r2。T2 参考解顺带开启已有 server log，与客户端 4+8 页计数对账，不新增模型运行。
4. 出具具体价格来源：本项目实际网关的 `vol-glm-5.3-flash` 路由，不能直接用名称相似的官方价推断；记录日期、计费单位、缓存语义、币种和停止值，尚无来源就保持未放行。
5. 回报“修复前反例 → 修复后结果 → 测试入口/工件 → 未验证边界”；不把 commit 数、测试总数当成放行证据。

这四项修完，才进行 G1 最终冻结与六跑派工；不因此扩大 v0.3 首批样本或开启 G2。
