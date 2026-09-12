# run_code 路由四臂矩阵：最终接受性验证报告

日期：2026-09-11 启动、2026-09-12 补齐。性质：[落地报告 §5.2](run_code_routing_report.md)
预先登记的**四臂主矩阵**，17 题 × 4 臂 × 3 重复 = 204 个 run，固定种子交错执行。

## 0. 批次身份与完整性

| 项 | 值 |
|---|---|
| 种子 / 交错 | 20260911，全 204 个组合混洗后 4 并发交错 |
| 模型 | glm-5.3-flash（网关默认采样，`reasoning_effort` 不调——2026-09-11 用户决定） |
| A `ta-mx-old-aw` | 旧契约二进制（276fb75 重编，sha `f2619497…`）+ agentic.md + on_demand |
| B `ta-mx-old-eager-aw` | 同 A 二进制 + agentic.md + **eager** |
| C `ta-mx-v2-aw` | v2 契约二进制（sha `7872a378…`）+ agentic-code.md + on_demand |
| D `ta-mx-v2-eager-aw` | 同 C 二进制 + agentic-code.md + **eager** |
| 提示词指纹 | agentic-code.md `311e2410…`；manifest `run-code-routing-v2` 全文件校验通过 |
| 判卷 | AppWorld 官方状态断言；cheat_flags 独立记录 |

**完整性披露**：原批在开跑约 2h42m 处遭遇测试模型网关持续 429（配额/限流），
[162]-[204] 共 43 个组合两次尝试均零成功请求、按基建失败保留现场；网关恢复后于
次日以同种子同配方增量补跑 43 个（161 个已完成组合指纹缓存命中跳过，零重复花钱）。
另 4 个 run（A awmostplay r1/r3、B awtop r1、C awphoto r1）首尝试 429 后重试成功，
`attempts=2`，其 client 墙钟含失败尝试（约 +30~60s），已个别核对不影响结论方向。
暖启动分布：A 6/51 暖（旧契约静态前缀命中 9 月历史批缓存），B/C/D 全冷——本报告
跨 run 对比一律用 conv_growth（= sum_unc − req1_unc，对缓存状态鲁棒），req1_unc 逐 run
记录在 `docs/run_code_routing_matrix_results.json`。

复现：`python3 scripts/run_code_routing_matrix.py --run`（增量，绝不重跑已完成组合）；
分析：`python3 scripts/run_code_routing_matrix_report.py --out docs/run_code_routing_matrix_results.json`。

## 1. 四臂总账

正确率（任务中位口径的汇总指标在表格下方；51 = 17 题 × 3）：

| 臂 | PASS/51 | 剔除 awthank | conv_growth 中位 | output 中位 | turns 中位 | client 墙钟中位 |
|---|---:|---:|---:|---:|---:|---:|
| A 旧契约 on_demand | 44 = **86%** | 44/48 = **92%** | 12,779 | 6,771 | 13.0 | 172s |
| B 旧契约 eager | 37 = 73% | 37/48 = 77% | 11,887 | 6,659 | 11.0 | 158s |
| C v2 on_demand | 43 = **84%** | 43/48 = **90%** | 12,340 | 6,974 | 11.0 | 177s |
| D v2 eager | 40 = 78% | 40/48 = 83% | 10,762 | 6,519 | 12.0 | 180s |

growth/output/turns/wall 均为「任务中位（剔除前缀二次计费伪影 run）的中位」，
任务为独立单元（17 题×3 重复不当作 51 个独立题）。

## 2. C−A：修复包效应（同为 on_demand，唯一差异=契约+提示词）

预登记阈值（§6）：轮次 ±2、output ±15%、growth ±20% 才算方向信号。

| 任务 | conv_growth | 判定 | 正确率 | 轮次 |
|---|---:|---|---|---|
| awlikeall（语义定集+批量写） | 28,740 → 13,875（**−52%**） | ↓省 | 3/3 → 3/3 | 13→11 |
| awlikesong（大集合批量写） | 43,767 → 12,184（**−72%**） | ↓省 | 2/3 → 2/3 | 18→15 |
| awthank（语义+日期窗+批量写） | 27,692 → 17,218（−38%） | ↓省 | 0/3 → 0/3 | 12→12 |
| awyears（跨库过滤计数） | 14,363 → 10,923（−24%） | ↓省 | 3/3 → 3/3 | 10→11 |
| awuniq | 6,241 → 10,995（+76%） | ↑贵 | 3/3 → 3/3 | 8→11 |
| awmostplay | 5,914 → 12,340（+109%） | ↑贵 | 2/3 → **3/3** | 10→14 |
| awlikes（小只读负例） | 3,214 → 5,422（+69%，绝对仅 +2.2K） | ↑贵 | 3/3 → 3/3 | 7→6 |
| 其余 10 题 | −18%~+18% | ≈平 | 总体 +2（awexport 2→3、awmostplay 2→3、awphoto 3→1、awrate 3→2） | 13→11 |

结论：

1. **正确率持平**：86% vs 84%，差 1 个 run（17 题×3 下的噪声量级）；剔除全臂皆灭的
   awthank 后 92% vs 90% 同样持平。
2. **目标形状任务结构性获益**：修复瞄准的"大集合/语义定集批量写"三题（awlikeall /
   awlikesong / awthank）growth −52%/−72%/−38%，与 §8 小样本批次方向、幅度一致；
   awyears 意外获益（−24%）。awlikesong 对旧直取 −72% 复现了历史结论（−72~−77%）。
3. **新增代码路线的任务付了入场费**：awuniq/awmostplay 是"直取本来便宜"的 M 题，
   v2 触发文案把部分 run 推入代码路线（awmostplay rc 1→5），growth 翻倍但绝对值
   （12K 级）仍低于批量写三题的收益量级；awmostplay 正确率反升（2/3→3/3）。
4. 轮次中位 13→11；墙钟中位 172→177s 总体持平，但 **awclean 形状的墙钟回归仍在**
   （159→241s，+52%）：长代码生成的串行解码是已知未解议题（§10），本矩阵再次证实
   提示词层不可解、未引入新恶化。
5. awphoto 为高方差摆动题：A 3/3、B 3/3、C 1/3、D 3/3；C 的两次失败是"直答太快判错"
   （819/412 output、5-8 轮、不用 run_code），与 §9 负例批（v2 3/3 PASS）和旧历史批
   （2/3）对比可见该题 ±1~2/3 的批间摆动，不构成 v2 回归证据，但如实记录。

## 3. eager 效应（B−A 与 D−C，同契约下唯一差异=注入形态）

| 对比 | 正确率 | conv_growth | 形态 |
|---|---|---|---|
| B−A（旧契约） | 86% → 73%（**−13pp**，44→37） | −7% | 7 题掉分：awfollow/awlikeall/awexpuniq/awrate/awclean/awtop 3/3→2/3、awexport 2/3→1/3 |
| D−C（v2 契约） | 84% → 78%（−6pp，43→40） | −13% | awclean 3/3→**0/3**、awexpuniq 3/3→1/3、awrate 2/3→1/3、awpurge 3/3→2/3；awphoto 1/3→3/3、awprefix 2/3→3/3、awlikesong 2/3→3/3 |

- **eager 在两种契约下都以正确率换 growth**：旧契约 −13pp、v2 −6pp，growth 仅省
  7-13%。省的主要机制是合成历史让模型更早进代码路线（turns 13→11）。
- **D awclean 0/3 的失败形态**（轨迹核对）：谓词语义判错被代码批量放大——r1 保留集
  判成 3 首（gold 为 2 首）、全部"验证通过"后交卷，状态断言失败；r2 cheat FAIL。
  与 §5.3 预判一致：语义层错误 + 代码层快速执行 = 批量错改，on_demand 下模型有更多
  对话级证据做语义核对，反而更稳。
- **eager 抬高小题成本下限**：awlikes（干净负例）growth A 3.2K → B 8.0K / D 10.8K
  ——技能正文常驻对话 + 诱导小题也走代码，每题多付 ~5-8K，正确率虽保 3/3，但负例
  面上是纯损耗。§9 的"零误触发"结论只适用于 on_demand。
- B−A 与 D−C 方向一致、v2 上损害更小（合成历史与新契约的触发文案部分互补），但
  两种契约下 eager 都**不满足"同等正确率下省成本"的接受标准**（落地报告 §5.3）。

## 4. 任务层异常与记账

- **awthank 全臂 0/3（12/12 FAIL）**：与路由无关的套件性质——语义定集+日期窗+评论+点赞
  四层叠加，直取路线（A：0 次 run_code、output 14-22K）同样全灭。代码路线把它从
  27.7K growth 压到 17K 但仍判 FAIL。该题应从"路由收益"叙事中剔除，留作套件难度标记。
- **cheat_flags 4 例全部 FAIL 判卷**（B awrate r3、C awlikesong r3、D awclean r2、
  D awexpuniq r3）：诚实失败，未美化任何臂的正确率。
- **前缀二次计费伪影 13 run**（非首请求 uncached ≥15K，接近静态前缀量级）：A awclean r1、
  A awexpuniq r1/r2、B awexpuniq r1、B awlikes r1、B awlikesong r2、B awpurge r2、B awsum r2、
  C awclean r2、C awexpuniq r2/r3、D awexpuniq r3、D awprefix r2——growth 中位计算已剔除。
  注意与"胖轮次"区分：awlikesong 直取的每请求带回整页歌单，>8K uncached 是合法增量，
  阈值取 15K 才不误伤（A awlikesong r2/r3 的 43-45K growth 是真实值）。
- **A awlikesong r1 早退失败**：32s、295 output、5 次调用后停止，判 FAIL 合法（任务未完成），
  疑似瞬时流错误，单例不作趋势证据。

## 5. 判定（门 6 关闭）

1. **C（v2 契约 + agentic-code.md + on_demand）通过最终接受性验证**：正确率与旧契约持平
   （−1 run 噪声级），目标形状任务 growth −24%~−72%，轮次下降，墙钟总体持平。
   v2 自此从"候选"转为**定型交付**；部署形态为 on_demand。
2. **eager 不作为默认**：两种契约下均以 6-13pp 正确率换 7-13% growth，且抬高小题
   成本下限，不满足接受标准。保留为采集/对照形态（落地报告 §5.4 不变）。
3. 遗留议题不变：awclean 形状墙钟 +52%（长代码生成串行解码），提示词层已证不可解
   （v3/v4 两连败 + 本矩阵复现），候选方向仍是技能层最小探测示例或模型侧差异，
   均需新单变量实验设计；awthank 为套件级难题，与路由解耦。
