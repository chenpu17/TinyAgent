# code.llm：沙箱内 LLM 调用原语 技术设计（草案）

| 项 | 值 |
|---|---|
| 状态 | v0.7.4 读数登记稿（M2/M3/M4 落码；V0-B 四臂主批 + e3 复查 + e1 库臂已跑：e2 一致性增量成立，A③/include 零效应登记）；产品收益部分验收 |
| 版本 | v0.7.4 |
| 日期 | 2026-09-23 |
| 关联文档 | `docs/run_code_design.md`（D 编号）、`docs/finetune_support_design.md`（FD 编号、附录 A 即规格纪律）、`docs/run_code_routing_report.md`（评测方法论）、`docs/code_memory_design.md`（姊妹稿，共享纪律见 §10） |
| 代码基线 | master @ edfb9bc（其后行号会漂移，以代码为准） |
| 决策编号 | 本文 LD-n 独立编号；引用 run_code_design 写 D-n、finetune_support 写 FD-n |

修订记录（历史仅说明演进，当前正文与决策表为规格）：

- **v0.7.5（外部审核缺陷修复轮：终止契约 / setter 验证 / panic 审计 / 批墙钟）**：
  读数来源 = 外部代码审核报告（基线 c555074 / 0.4.1，7 项可复现运行时缺陷，
  本轮全部修复；探针与观测在审核方环境）。
  ① **LD-7/LD-17 终止契约补全（审核 P1-1）**：`provider.chat` 返回 Ok 不等于
  响应完整——stop_reason ∈ {stream_error, stream_truncated, stream_idle,
  cancelled, max_tokens}（与外层 agent 的 aborted 词表同源）时，LlmTool 返回
  **kind="llm" 错误**：消息点名 stop_reason 并附截断文本（过 max_output_chars
  截断），部分 usage 照实结算并随错误封套折叠（usage_in_shared_budget=true，
  形状同 llm_output miss 路径）；无 usage 保留预占（LD-17 row 2 同款）。
  **max_tokens 定约为不完整输出错误**（审核方确认）：调用方的 cap 切断了生成，
  残缺文本不以成功身份返回，调用方自决是否提高 max_tokens 重问。终止检查位于
  结算之后、labels 校验之前——截断的 "yes" 不得过词表校验冒充判定。
  LD-17 矩阵增行「transport Ok 但 stop_reason 异常终止」：charge = actual
  （有 usage）否则 reserve。新增模型可见错误文案登记于本条：
  `llm: response ended abnormally (stop_reason={sr}) — the output is incomplete
  and was not returned as a result; partial text: {text}` /
  `llm: output hit the max_tokens cap (stop_reason=max_tokens) — …`。
  ② **LD-5 入口收敛（审核 P2-5）**：llm 不变式（models 非空、default_model
  必填且 ∈ models、首现去重）提取为 `CodeConfig::normalize_and_validate_llm`，
  `Config::validate_and_normalize` 与 `Agent::set_code_config` 共用（**先验证
  再替换**）；LlmTool 省略 model 分支对最终模型一律校验（注册表快照可能早于
  任何验证落地）。绑定侧 set_code_config 透传同一 Err，无需各自改动。
  ③ **批 panic 占位审计（审核 P2-7）**：`Tool` trait 新增
  `panic_audit_row(input, ctx)`（默认 None）；LlmTool 覆写——从 worker 实际
  派发的**有效输入**重推 (model, est)，占位 ToolOutput 附 `llm_call` 行
  （actual:null，不伪造实际用量），经完成尾折叠进 meta.llm，使 ReserveGuard
  Drop 保留的预占可在总账对账。错误 class 仍 "tool"（worker 崩溃不是 llm
  分类失败）。release panic=abort 下进程直接结束，本条只覆盖 unwind 构建。
  ④ **callBatch 墙钟检查点（审核 P2-4）**：波内**每项 inline 执行前、每次
  worker 派发前**刷新 cancel+deadline 闩锁（新增 `batch_wave_interrupt_refresh`，
  刻意不短路既有 budget 闩锁——prep 已计数的 admission 不受 call-limit 拒绝
  牵连）；未启动项以 kind=deadline/cancel halted:true 拒绝并配平事件，在飞
  调用允许完成。派发对齐：refused-before-spawn 的条目不再与 join 循环 zip
  错位（`launched` 数组取代 spawn 位置 zip）。
- **v0.7.4（V0-B 正式轮读数登记：A③ 零效应 + include 零采用；规格文字未动，
  修订讨论开启）**：读数来源 `docs/code_v0b_report.md` §5（四臂主批
  v1.4 / 协议 v2.1 / 外层 deepseek-flash@8192 交错 ×3 + e3 预算复查 + e1
  库臂 cold ×6 / warm ×3）。
  ① **E2 一致性增量成立**（本战役主读数）：e2 三跑 F1 极差 0.012、3/3
  干净完成，同批 gates-off 臂在 0↔0.97 翻转——分层设计（规则层 + llm
  兜底）的可靠性主张实测坐实；代价 4.8× 墙钟 + **二阶路由方差**（高效跑
  116 内层调用规则先 / 低效跑 835 调用 llm 全量判——"何时停用 llm"未解）。
  LD-17/18 账本与 labels 契约 ~4000 内层调用 0–1 unknown 实地成立。
  ② **A③ 三问边界句（E0′ 装置）两批皆零效应**：完成率 3/6 vs e0 5/6
  （反向）、质量上限持平、路由行为差异未观察到（"试走 lib/llm 撞诚实
  拒绝"事件为零）。**处置：A③ 冻结文字与 E0′ 装置保留不动**（§8.2 纪律：
  读数仅覆盖 deepseek 外层 + 单任务形状，证据不足不强行判负），但 E0′
  降出主假设位——后续战役不再作为首要臂，除非换外层模型重测翻案。
  ③ **include 库通道摊销假设双端不成立（此任务形状）**：cold 零建库 0/6
  （单次任务无建库动机）；warm 预置批准库（真实 saveLib 登记 + 负责人
  批准 + 跨会话 include 准入探针通过）后 **include() 零采用 0/3**——完成跑
  读库源码后手工移植成 Python（知识复用发生、通道复用没有）。归因 =
  substrate 错配：include 只在 QuickJS 沙箱内生效，外层偏好 shell+Python。
  **修订方向登记（未决）**：include 的价值面在"沙箱内已有代码路径"的场景
  （e2/e3 的 llm 判别循环里 include 判别库——「库与复用」段本就点名判别库
  是最高价值形态），不在"外层从零选执行路线"的场景；后续探针 = e2 臂 +
  预置判别库（含 verify 探针）的 warm 形状。v1 不为此扩 Python 沙箱
  （run_code = QuickJS 是既定决策，姊妹稿 D 编号同口径）。
  ④ 装置面增补：runner `--max-total-tokens`/`--inner-model` 参数化（进 run
  版本戳）、e3 预算包络登记（装配文档 §2.2）、warm 库装置
  `examples/eval/e1-warm-lib/`（sha256 指纹登记）。
- **v0.7.3（M4 契约与评测装配落码）**：
  ① **A③ 三问边界句冻结**（"落码即冻结"条款于本版生效）：载体 = E0′ 实验
  装置变体 `examples/eval/e0prime-skill/run-code/SKILL.md`（磁盘同名覆盖
  内置技能的既有机制；all-off 形态——门控三段手工剔除，磁盘技能不做标记
  剔除），装配与指纹登记在 `docs/code_llm_eval_assembly.md`。文字变更仍走
  本修订记录。
  ② **正式披露按 gate 的规则定案**：内置技能 ungated 基面**不含**三问边界
  （改基面即破坏 A-L8 钉住的门关字节一致基线）；三问全文只存在于 E0′ 装置
  与本文 A③；门开臂的路线教学由各门控段承担（LIB 段教 INDEX 复用、LLM 段
  教语义批量、BATCH 段教并发形态）。E0′ 变体在门全关下点名 lib/llm 路线是
  刻意的文案效应测量设计（§8：试走该路线得诚实 unknown_tool/保留名拒绝）。
  ③ 契约汇总补缺（M4 审计产出）：LD-7 的 `kind="llm"` 此前只在错误封套与
  测试里、两个教学面（run_code 描述【LLM 判别】overlay 与 skill LLM 段
  引言）均未点名——已补（"未命中 llm_output、其余失败 llm，均无 retryable
  读 message 自判"）。
  ④ **附录 A⑤ 新增**：run_code 描述的 LLM overlay 段 verbatim 钉住（与 A④
  callBatch 行对称）——三面同源（描述 / skill / 附录）对 llm 面完整。
  ⑤ 评测装配：`docs/code_llm_eval_assembly.md`（E0–E3 臂配置表、E1
  cold/warm、E2 判别模型档位与 meta.llm 成本归集、四路线协议、测量协议
  沿用 req1_unc 勘误口径、门关回归清单）；`scripts/check_eval_apparatus.py`
  装置漂移门（指纹 vs 登记 / A③ 逐字一致 / all-off 变体无门控内容渗漏 /
  frontmatter 名=目录名）进 CI 文档门 job。
- **v0.7.2（M3 llm 落码）**：LD-1～19 契约落码，语义差异与补充如下——
  ① **⑨ 定案：单次调用错误封套不补 `halted`**。预算拒绝（kind="budget"）的恢复
  语义（沙箱仍活着、可继续 write_file 落盘收尾）由错误 message 文本承载；机读
  `halted` 字段只在批量封套存在。理由与 ⑨ 相同不再议：单次封套不受任何 gate
  门控，加字段立即改变全关对照臂（A-L8）的错误形状。
  ② 批量封套 halted 矩阵落码：budget 类 entry **恒带** `halted`（true = latch
  态：call-limit/deadline/cancel；false = token 预占拒绝）；其余错误类仅
  halted=true 时携带（additive）。llm 的错误类（`llm`/`llm_output`）与 `tool`
  类同列"非 halt 态"。
  ③ 错误类的传递通道：LlmTool 在 `ToolOutput.meta` 写 `llm_err`，新 Tool trait
  方法 `envelope_error_kind()`（默认 None → 沿用 tool/unknown_tool 分类）读出，
  run_code 的 dispatch 收尾是唯一读者——prelude glue 与既有工具零改动。
  ④ LD-12 审计落码形状：result.json `code.llm` 配置锚点（enabled/
  default_model/models，仅启用时落、additive）+ `meta.llm` **聚合**台账
  （calls/reserved/settled/over_reserve + per-model known_usage/known_calls/
  unknown_calls）；per-call 明细行（model/reserved/actual/known/over_reserve）
  经完成尾折叠为聚合，成功与终止两个出口都折叠（失败出口审计纪律）。
  ⑤ LD-16 落码形态：`Agent.provider` 内部由 Box 改 Arc（公共 API 仍是 Box，
  try_new 处转换）；cell 晚填充收敛在 `Agent::set_code_tool` 单点（LD-16 文中
  "build_agent_with_provider 回填"由该点实现，绑定宿主 enable_code 同路）。
  ⑥ LD-3 落码位置：顶层撞名检查在 `Agent::try_new`（工具表已到手、先于任何
  装配），不在 `validate_tool_names` 尾部——后者在 assemble 末尾，触达时
  RunCodeTool 已构造；guest 侧注册表构建时再查一次（防 extra_tools 之外的
  直入路径），文案带"名字保留"提示。
  ⑦ 描述/技能披露落码：批量段 readers 的 "read_file / llm 等 batchable 工具"
  仅当 llm gate 开启才点名（关闭时保持 ⑦（v0.7.1）的"read_file 等"字面——
  封闭清单诚实，BATCH_DESC_SUFFIX 基字面不动、运行时替换）；描述【LLM 判别】
  overlay 段与技能「LLM 判别」段同随 `code.llm.enabled`。技能批量段的 llm
  批量形态在 call_batch 关时用指针措辞（不出现 `tools.callBatch` 字样，守
  A-L8 的 gate 各管各的段纪律）。
  ⑧ e2e 落码（A-L4）：`scripts/e2e_scenarios.py` 增 llmbatch 场景——8 entry
  callBatch × llm 过真实 CLI/HTTP/SSE 栈，请求数精确核算（2 外层 + 8 内层）、
  服务端按模型注入延迟并记录**服务窗口**，时间区间重叠断言真并发；CI e2e
  job 构建改 `--features code`。坑位记录：socketserver 默认 listen backlog 5
  在 8 路瞬发新连接下丢 SYN（表现为随机 connection reset 的 entry 错误），
  `request_queue_size=64` 解决——批扇出场景的 mock 服务端都要带。
  ⑨ 配置落码：`LlmConfig` 三字段（deny_unknown_fields），解析归一做首现去重、
  enabled 时的不变式校验（models 空/缺 default/default ∉ models 均为可行动
  报错）；batch 片段拒绝改 `llm.enabled`/`default_model`，`models` 只能收紧
  （交集、保 base 序）且交集丢 default_model 直接报错。
  ⑩ v0.7.1 ⑩（批结果宿主侧驻留上限）**M3 未包含**，维持推迟：跨 entry 字节
  预算/逐 entry 写回随评测门再议；llm entry 的驻留由 max_tokens ≤8192 tokens
  钳制、read_file 沿用 max_output_chars，风险仍如 ⑩ 所述有界未消除。
- **v0.7.1（M2 callBatch 落码）**：§4 契约落码，语义差异与补充如下——
  ① `batchable()` 能力轴（LD-19）：`Tool::batchable()` 默认 false，read_file 与 MCP
  `readOnlyHint` 覆写 opt-in，缺注解=false；`SharedTool`（extra_tools 包装）转发
  `batchable`，否则宿主注入工具的 opt-in 在 guest 注册表里丢失。
  ② LD-11 路由落码：波引擎只收 `ToolConcurrency::Parallel` 的 batchable 工具；
  v1 白名单全部 Sequential（read_file / MCP），批内按提交序**串行**执行（每 entry
  契约一致、无提速），真正并发等 M3 的 Parallel `llm`。并发本身已用进程内
  Parallel mock 工具 × 8×120ms 的 host 侧时间戳重叠测试钉住（时间区间重叠才是
  并发证明，功能断言钉不住串行化退化）。
  ③ gate-off 保留名直调（LD-19 v0.5）：`__call_batch` 关闸时**不特判**，落穿
  Phase B 按未知工具报错——与未编 feature 的旧二进制逐字一致（"明确拒绝"取该
  形态；include 家族的点名拒答是姊妹稿自己的决策，两稿各守各的记录）。
  ④ entry 语义细节：缺 `args` 键视作 `{}`（单次调用 glue 的 undefined→{} 对齐），
  args 非对象才是 shape 错误；unknown 工具 entry 与单次调用同款（after_tool
  executed=true、"unknown tool: {name}"）；非 batchable entry 发
  after_tool(executed=false)（§4"与 Deny 同语义"）；hooks Deny 同样发
  after_tool(executed=false)——**不发 after 的只有 deny_paths/fs_write_dirs 层**
  （该层在单次调用里也从不过 hooks 的 after，Hooks 契约两侧一致）；call-limit
  撞线在首个超额 entry 处 latch，文案与单次调用共用同一 helper（byte-identical）。
  ⑤ 事件顺序落码：ToolCall 在 prep 串行段按提交序；ToolResult——Sequential
  entry 主线程完成即发，Parallel entry 经每波有界 sync_channel（scope 闭包内
  创建 + CancelOnUnwind + panic→占位错误条目，同主循环并行批的 unwind 纪律）
  drain 时按到达序发。
  ⑥ `MAX_BATCH_ENTRIES=4096`：shape guard（非规格数字；1 MiB 代码上限已约束
  规模），超限整调用 kind="batch" 错误。
  ⑦ A④/技能批量段文案落码差异：不提 `llm`（M3 前注册表没有它，封闭清单
  纪律），作"read_file 等 batchable 工具"；M3 落地时随白名单同步更新。
  ⑧ 配套：`ToolCtx`/`ToolOutput` 补 `derive(Clone)`（字段全 Arc/Copy，worker
  与 entry 复制所需，共享语义不变）。
  ⑨ LD-9 `halted` 字段的**单次调用面**推迟：M2 落码只给批量封套带 `halted`
  （LD-9 的批量面）；单次调用错误封套维持旧形状、不带该字段——单次封套不受
  `code.call_batch` 门控（feature 编译即在），加字段立即改变门全关时的模型可见
  错误形状，破坏 A-L8"关=与旧版一致"对照臂；且单次调用的 halted 语义与 LD-17
  预占（budget 不 latch vs call-limit latch）耦合，属 M3 面。M3/评测门再议是否
  给单次调用补 halted，届时 A-L8 对照臂以 M3 版本重新钉。
  ⑩ 批结果的宿主侧累积上限推迟（外部实现复审 #5）：M2 组装把全部 entry 的
  output（各 ≤ ctx 输出预算）同时驻留 `finals` + 结果数组 + 串行化三份，全部
  发生在 guest 内存限额可见的最后一次 alloc 之前——大批 × 大输出工具（MCP 截
  到 2M）在低配 mem limiter 上是 OOM 面。跨 entry 字节预算/逐 entry 写回的设
  计与 LD-17 预占结算同批（M3），v1 白名单（read_file/MCP）下单 entry 输出由
  `max_output_chars` 钳制、`max_tool_calls` 默认 200，风险有界但未消除。
- **v0.7**：质量政策与交付状态落地；V0 拆为 V0-A 方法验证 / V0-B 集成验证，
  预登记门槛并增加变化任务；可信资产改为宿主治理契约；预算改称估算预占，
  分离实测与未知用量并防重复计账；同步正文、LD 决策、测试与附录。
- **v0.6**：首发场景收窄、总成本及外部对照、实际输出抽检、V0 前置、核心与
  宿主分工。v0.7 进一步消除 V0 的实施依赖循环与遗留过强承诺。
- **v0.5**：结算路径、共享 Provider cell、labels、callBatch 只读范围补全。
  当时的“条件硬封顶”不作为当前保证，原因见 LD-17。
- **v0.4**：引入预算预占、同实例 Provider、封闭词表与分期评测。
- **v0.1–v0.3**：确定无状态内层调用、既有工具通道、批量部分执行与路由边界。

本稿描述待实现行为；`0579564` 是核对既有机制的代码基线，不代表本稿功能已存在。
姊妹稿 `code_memory_design.md` 同为 v0.7；LD-1～19 保留编号。

---

## 0. 立项论证：native 原语的增量

今天可以通过一个提供判别工具的 MCP server 验证“代码编排 + 内层 LLM”这条
路线。native 实现的增量在于：

1. **低部署成本**：嵌入式低配 Linux 与宿主嵌入是主要交付方向；同一二进制内
   复用 Provider，减少额外进程和配置。模型本身仍可能运行在远端或外部服务。
2. **统一用量归因与派发前预算检查**：普通 MCP 路径不自动把外部模型用量纳入
   TinyAgent 账本。native 路径由 LD-17 预占/结算，实测 usage 经既有折叠点进入
   totals，已结算的预算不重复累计；未知用量单列，超额可观察。

`BudgetGate` 计量 token，不计货币；预占使用估算，不能保证所有模型的输入
上界，外层与子 agent 也保留既有事后记账路径。因此本方案**不承诺整个 run 的
token 或货币硬封顶**。预占能控制并发派发时的额度竞争，审计能暴露偏差，两者
都有独立价值。MCP 若额外实现用量传递和预算协议，也可接近这种能力；优势应由
V0-B 的集成与维护成本实测证明，不把协议差异宣传成不可复制壁垒。

hooks 与工具 trace 是共用基础设施：MCP 和 native 内层调用都经过它们，不能
把这部分重复算作 native 的独占收益。

## 1. 定位与价值主张

产品方向：**面向宿主集成的轻量 Agent 执行内核，让重复语义任务形成可检查、
可维护的程序资产，并在满足质量门槛时降低总成本。**这是待验证目标。

外层模型负责目标理解、策略与例外；代码负责循环、数据处理和聚合；内层 LLM
负责有界语义判断；文件库保存经过验证的程序。`llm` 扩展能力，`callBatch`
改善吞吐，`include` 减少重复生成。三者分别验证增量，再测组合收益。

**首发选低风险、只读的工单分类/打标试点**：输入单元相对独立、标签标准明确、
错误可计量、相同流程反复出现；输出分类文件与需复核项，不自动删除、发送或
改变业务状态。字段抽取等生成任务是后续候选，其结果校验由具体库负责；v1
的 labels 契约仅覆盖封闭分类，不代替通用结构化抽取校验。

外层逐项消费全文时，上下文输入至少随输入规模增长；代码侧处理可让外层只接收
有界摘要与证据指针。但内层仍需处理数据，抽检也消耗上下文。不能把“聚合摘要
有界”推导成全流程成本 O(1)：固定比例的外层抽检仍随规模增长。

**价值分两层验证**：固定任务上的 warm 库证明程序摊销；同领域需求变化、接口
变化和不适用任务上的正确复用/修订/放弃，才证明 Agent 的增量。原语容易复制，
长期价值来自领域程序与质量证据、低维护负担和宿主集成，不能由功能齐全推定。

超 context 语料处理作为能力延伸：仅在分块所保留的信息足以完成任务、聚合规则
明确时适用，并保留原文定位。全局排序、按 ID 去重等确定性运算仍适合代码；
只有未被分块表示保留的跨项语义关系需要外层另设计策略，不能将所有全局处理
笼统排除，也不能承诺分块摘要无损。

## 2. 总体架构

```
LLM → tool_use: run_code {code}（外层仍使用既有工具循环）
  └─ guest JS: tools.call("llm", {prompt, system?, model?, max_tokens?, labels?})
       └─ host_call_tool 拦截层（既有次序零变动）
          → 入口检查（halted/cancel/墙钟/调用计数）→ hooks 闸门（ctx.subagent="__code__"）
          → deny/fs 层（llm 无路径参数，天然不适用）→ LlmTool::run
             ├─ 前置①：Provider 解引用（RunCodeTool 晚填充的外层同一实例，LD-16）
             ├─ 前置②：BudgetGate 估算预占（LD-17；batch 在同一派发准备段完成一次）：
             │    reserve = est_in(prompt+system) + max_tokens
             │    gate.reserve(reserve)：used + reserve > cap → kind="budget"（halted:false，不 latch）
             ├─ ChatRequest { model: 调用侧 model（须 ∈ allowlist）, max_tokens,
             │    per_request_timeout: 120s, cancel, cache:false, reasoning_effort:None }
             │    → provider.chat(...)（外层同一 Provider 实例，非流式消费）
             └─ 结算（LD-17 矩阵）：gate.settle(reserve, charge)：used − reserve + charge
                usage 完整时 charge = actual；缺失 / panic → charge = reserve（估算，非实测）；
                未启动 entry / 派发前取消 → 全额退（防泄漏）；actual > reserve → 净加差额
                + 红旗审计（trace + result.json over_reserve 计数）；
                ToolOutput { content: {text, label?, usage, model} 的 JSON,
                            usage: 实际上报或 None, usage_in_shared_budget: true }
       ← 折叠点（既有代码）：totals 饱和合并进 run_code 自身账单（诚实成本）
```

guest 侧无新 import、无新保留名（`llm` 是注册表里的普通工具，走 `tools.call`
通道）。宿主侧新增一个 `LlmTool`（`CopyBytesTool` 先例：retain 之后注入的
guest-only 工具，永不进顶层工具表）。

**实现注记（外部评审"非流式路径"）**：LlmTool 复用 provider 层既有 chat
路径，不自建 HTTP——provider 内部已处理流式聚合与 per-request timeout（SSE 三层
限额纪律）。若实现中发现非流式单发的快路径缺失，**补在 provider 层**（单一事实
源，非流式是普遍需求而非本工具私有），不在工具内另起请求栈。

## 3. 关键决策记录

| # | 决策 | 理由 | 否决的备选 |
|---|---|---|---|
| LD-1 | 形态 = guest-only 工具，经 `tools.call("llm", …)` 通道 | hooks 闸门、ToolCall/ToolResult 事件对、meta trace、额度计数全部免费继承（与 MCP/文件工具同一条 dispatch 面） | prelude 独立命名空间 `llm.call()`（要么丢 gate/审计，要么走保留名截获——截获点在 gate 之前，更糟）；host 侧新事件通道（破坏前端零适配原则） |
| LD-2 | 无状态单发：无工具、无历史、无流式；一轮返回 | 有界成本原语，递归深度构造性为 1——这是 D11 排除物（`task`，无界 agent 递归成本炸弹）的有界版本，不是绕过 | 小型嵌套 agent（= `task`，撞 D11）；带 messages 数组的对话式 API（状态拼装是代码的活，开放面大） |
| LD-3 | 命名保留 `"llm"`：`code.llm.enabled=true` 时 `validate_tool_names` 追加拒绝撞名（Err + 提示） | `__tools_list` 保留名先例（feature 开启即拒）；关闭时用户 MCP 命名 `llm` 仍合法 | 撞名时静默遮蔽（dispatch 首匹配，模型看到的是假 llm） |
| LD-4 | 参数 = `prompt` / `system?` / `model?` / `max_tokens?` / `labels?`；max_tokens 默认 1024、范围 1～8192。v1 不暴露 temperature 与 reasoning_effort，内部请求 temperature=0、reasoning_effort=None；provider 不支持时按明确错误或已记录的兼容修正处理 | 减少调用侧参数；temperature=0 不保证确定性。当前 ChatRequest 无 temperature 字段，按请求控制的支持须在 provider 层补齐，不能修改外层调用默认值 | 自由生成参数全量透出；静默宣称不支持的参数已生效 |
| LD-5 | `model` 缺省 = `code.llm.default_model`，**启用时必填**；调用可显式传 model 覆盖（须 ∈ allowlist） | 成本分层是本特性的价值主张，缺省回落外层贵模型自相矛盾；"哪些模型可被沙箱用"是部署决策，显式配置 | 缺省回退外层 agent 的 model（方便但贵模型误用） |
| LD-6 | 返回 `{text, usage, model}`，声明 labels 时增规范 `label`；usage 为 provider 实际上报对象或 null，详细估算与完整性状态进入宿主审计 | 未知不是零，不把预占伪装成实测；JSON 自动解析沿用 D4 | 只回 text；用 reserve 填造 input/output token |
| LD-7 | LlmTool 本身单发、不做自动语义重试；失败 kind=llm，不增加 retryable 字段。provider 既有 400 请求整形可保留，必须可审计。guest 不写忙等重试；unsure/非法输出可按场景预算升级模型，系统性限流交外层/宿主延后处理 | 区分新语义尝试与请求整形，避免隐藏消费；缩批/换模型不一定解除同网关配额，不能作为万能恢复 | 工具内隐藏退避循环；无界 guest 重试；宣称失败必定零成本 |
| LD-8 | 三层运行约束：token 估算预占（LD-17）、`max_tool_calls` 计数、墙钟与单请求超时；不增加独立 `max_llm_calls` | 调用数已计入公共额度；预占限制派发，不能宣传为实际成本硬封顶 | 重复调用数旋钮；将软墙钟或估算说成硬上限 |
| LD-9 | LlmTool 调用**前**的额度关口升级为 **LD-17 预占**（v0.3 的 `tokens_left<=0` 弱前查作废）：预占失败 → `kind="budget"` 错误（**不 latch halted**）。**封套 additive 字段 `halted: bool`**：来自 latch 态（call-limit / deadline / cancel）的错误 `halted:true`；budget 类（预占拒绝）`halted:false`；非 halt 态的单次错误（tool/llm/llm_output/unknown_tool/hooks/denied）不带该字段。**v0.3 论证修正（保留）**：两种耗尽的恢复语义真实不同——call-limit latch 后 halted 拒纳**一切**后续调用（含 write_file，`run_code.rs:566-577` 入口检查），guest 只能以 return 收尾；budget 拒纳（含预占失败）不 latch，沙箱仍活着（预占失败无在飞占用，额度被结算释放后可复检），可 write_file 落盘收尾。共用 budget 类的成立理由**只是**：词表零增量 + 两者同属"额度耗尽"的 dispatch 级类别；行为差异由 `e.halted` 机读承载 | 预占失败是瞬时可复检的状态（不像 call-limit 是计数终态），latch 无必要且会误杀"落盘收尾"路径 | 新增 `kind="tokens"` 词表值（halted 字段已承载机读区分）；latch budget 拒纳（杀掉落盘收尾）；靠文案区分（FT 依赖面上最脆的解法） |
| LD-10 | per-request timeout = **120s 硬编码**（对齐 MCP `CALL_TIMEOUT` 先例 `src/mcp.rs:16`；**不与墙钟取 min**——在飞调用越过墙钟正是既有诚实边界公式的第二项，MCP 调用在飞越墙同款；深层推理模型单发合法超 60s，取 min 会截断它们）。映射到 `ChatRequest::per_request_timeout`（`provider/mod.rs:293` 既有字段，压缩摘要同款用法） | CLAUDE.md 纪律"每个请求必带 per-request timeout 兜底"；与 MCP 工具同口径 | 60s + 墙钟 min（截断深层推理；与 MCP 口径不一致）；可配超时（配置面膨胀） |
| LD-11 | `ToolConcurrency::Parallel`（可重入 + 轮询 cancel，既有承诺语义） | 请求无状态；Send+Sync 仅保证内存安全，自定义 Provider 可内部串行（LD-16）；这是 callBatch 的路由承诺，真实吞吐需验证 | Sequential（并行判别不存在） |
| LD-12 | result.json 的 `code.llm` 与 NDJSON 起始配置锚点记录 enabled/default_model/allowlist；完成审计记录按实际模型归因的已知 usage、未知调用数、预算估算占用、over_reserve 及配置版本 | 多模型总成本可追溯；未知用量时明确总量不完整。完整性信息必须在成功和失败出口都可见；金额由评测装置按已记录价目换算 | 仅记录 default_model 却无法识别实际覆盖模型；将未知消费计为零 |
| LD-13 | precheck 零新增检出项 | llm 是显式工具调用，不是环境语法/确定性桩——与 MCP 调用同一检出待遇（不检） | 检出 `tools.call("llm")`（误杀合法代码） |
| LD-14 | 注册时机：guest 注册表惰性构建时按**当时** `snapshot().llm.enabled` 决定是否含 llm；其后热翻转不感知 | 注册表 OnceLock 每世系一次（既有缓存纪律）；与 MCP 热增删同款文档化快照边界 | 每次调用重建注册表（破坏缓存纪律）；LlmTool 常驻 + 运行时自拒（tools.list 会展示不可用工具，误导） |
| LD-15 | callBatch 带评测门：`code.call_batch: bool` **默认 false**，评测收官后保留或移除再议 | S6 常备规则首个适用对象：callBatch 是 LLM 可见变更（描述/技能会教它，即使 guest 从未调用，描述存在本身即移动路由分布）——无门则新二进制跑不出旧行为对照臂 | 无门直接落码（对照臂不可复现） |
| LD-16 | **（v0.4；v0.5 补共享 cell 形态）Provider 拓扑 = 外层同一实例**：内层 llm 调用走构造外层 agent 的**同一个 `Arc<dyn Provider>`**，经 RunCodeTool 新增的 `provider: Arc<OnceLock<Arc<dyn Provider>>>` 晚填充——**必须是 Arc 包裹的共享 cell**（与 registry 缓存同款 clone 共享语义：RunCodeTool 的 clone 们共享同一 cell；普通 OnceLock 字段会在 clone 间分裂，晚填充只写进 Agent 持有的实例、guest 实际调用的 clone 仍无 Provider——专家 2 复审指认）。填充点：`build_agent_with_provider` / `build_agent_with` 在 `Agent::try_new` 后回填；注册表惰性构建发生在首跑，晚填充先于任何调用。**每次调用经 `ChatRequest.model` 指定 allowlist 内模型**（`provider/mod.rs:287` 逐请求字段），不建 provider 池。推论：①自定义 Provider（两绑定）天然收到内层调用，serve 不了该 model 就诚实报错（→ kind="llm"）；②mock 走同一分流；③计费/日志/重试语义与外层单一路径，不分裂。**并发面文档化（v0.5）**：`Send + Sync`（`provider/mod.rs:310`）只保证内存层面的并发安全，**不保证自定义 Provider 的逻辑并发能力**——串行实现的 Provider 照常工作，系统接受其吞吐下降（不做并发性强制）；**Provider panic** 的预算/审计一致性由 ReserveGuard 的 Drop 结算保证（LD-17：guard 释放时未显式结算 → 全额保守计），panic 经 callBatch worker 的既有 unwind 纪律传播不静默 | 单实例多模型是 Provider 界既有形态（model 是请求字段不是构造参数）；自定义 Provider 是绑定宿主的一等公民，内层绕开它们 = 行为分裂；晚填充避免改 assemble 签名（RunCodeTool 今天在 assemble 期构造、Provider 在其后才进 Agent，`lib.rs:133-147`） | 按配置为内层另构 OpenAI/Anthropic 实例（自定义 Provider/mock/计费全部分裂）；provider 池按 model 键缓存（同一实例已够，多一层状态）；普通 OnceLock 字段（clone 分裂，专家 2 复审否决） |
| LD-17 | **估算预占 + 一次结算**：派发前原子预占输入估算与输出限额；已知 usage 按实测结算，未知按 reserve 占预算并单列；未派发全退；actual 超 reserve 如实记账并报警。详细账本与矩阵见 §3.1 | 限制并行过度派发；预算利用率与诚实用量分别维护。估算没有普适上界，不承诺 run 级硬封顶；退款是必要释放路径 | 仅事后记账；所有路径永不退款；预算估算混入实测 totals；依赖红旗反向证明不会超额 |
| LD-18 | **（v0.4；v0.5 补全）`labels` 受约束输出契约**：调用侧可选声明 `labels: string[]`（非空、≤16 项；**声明自身先过宿主同款归一化（case-fold）并查碰撞——["Yes","yes"] 归一后重叠 → 参数错误拒绝**，v0.5）。声明时宿主做**最小归一化**（trim → 去包裹引号/反引号 → case-fold）后校验输出 ∈ labels：命中 → 返回增 `label`（规范值）；未命中/空/多项 → 封套错误 **`kind="llm_output"`**（非 retryable，guest 决定降级路径：换模型/升外层/标记跳过）。**（v0.5 固定）宿主不向 prompt 注入 labels**——校验是宿主的活，"输出必须是词表之一"的提示是调用方代码的活（prompt 单一作者 = guest 代码，审计面清晰；宿主注入会让 prompt 有两个作者，用量与归因都模糊）；skill 教声明 labels 时 prompt 里同写词表要求。**未声明 labels = 生成模式**（明确定位，非缺陷）：返回任意文本并可驱动代码分支——判别场景 skill 教必声明，不做 API 强制（判别/生成二分 API 是面膨胀）。skill 教 unsure 显式词 + 单独计数进聚合摘要（宿主不聚合，统计是代码的活） | 词表封闭从提示词纪律升为运行时契约——级联判别（flash 先判、unsure 升级）的分支可靠性不再依赖模型听懂 skill；归一化刻意最小（只修排版 tics，不猜语义：抽取首个 token 之类的"聪明"归一化会把模型漂移静默洗白，违背诚实原则）；labels 自碰撞检查同理——不做则 ["Yes","yes"] 在校验时产生不可判定的匹配 | json/schema 通用结构化输出（开面大、网关支持不一，LD-4 已否）；正则抽取式归一化（洗白漂移）；宿主自动重问一次（= 内部重试，撞 LD-7）；宿主注入词表提示（prompt 双作者，审计面模糊）；强制判别必须声明（二分 API 面膨胀） |
| LD-19 | **（v0.4；v0.5 白名单修正）callBatch v1 只读收窄**：新能力轴 `Tool::batchable() -> bool` **默认 false**；**v1 白名单 = read_file / llm 两类真只读工具**（v0.4 曾列 todo 读操作——**v0.5 移除**：todo 的"读"是参数级语义（读 vs 写 todo 取决于入参），工具级 bool 表达不了"该工具的所有输入都只读"；参数级判定留 v2 再议）；MCP 工具映射 `annotations.readOnlyHint`，**缺注解 = false；有注解也只视为调度提示，不是副作用保证**（服务端声明，非安全证明——无事务三句话对全部 batchable 工具仍全量适用，v0.5 措辞降格）。批量中含非 batchable 工具 → 该 entry 得 `kind="batch"` 错误条目（单条不中止，与 Deny 同语义）；**副作用批量不提供**——guest 写顺序 for 循环（skill 教），顺序性即次序保证；文档三句话显式：**无原子性、无回滚、部分执行是正常结果**。**保留名直调钉住（v0.5）**：`__call_batch` 被 guest 直接 `tools.call` 调用 = 走与 prelude 包装完全相同的全链（保留名本就是唯一入口，直调不是绕过）；gate 关闭（`code.call_batch=false`）时直调保留名 → 拒（与未启用 feature 的行为一致，A-L8 钉） | 通用 `[{name,args}]` 会被模型理解成"批量=事务"（专家 2 P9）；两个动机场景（分页拉取、判别 fan-out）本来全是只读——收窄零动机损失；能力轴是本库既有 idiom（concurrency / available_to_subagent / available_to_code / origin 四先例） | 双 API（只读并行 + 顺序副作用批）——顺序副作用批相对 guest for 循环无增量，纯 API 面；allow_batch 配置（又一面配置）；把 readOnlyHint=true 当安全证明（调度提示 ≠ 副作用保证） |

### 3.1 LD-17 账本与结算契约

预算账本与用量账本分开：`used` 包含已结算额度和在飞预占，`tokens_left`
据此决定是否可派发；provider 的已知 usage 才进入实测 totals。无完整 usage
时，预算保留 reserve，审计标注 `usage_unknown` 与 `estimated_budget_tokens`；
不能构造假的输入/输出 usage。已知合计只能称“已知用量”，不得称完整账单。

`reserve = est_in(prompt + system) + max_tokens`。v1 输入估算沿用启发式并留协议
余量，须用目标模型校准；ASCII chars/4、宽字符 1:1、固定协议余量都不是数学
上界。输出与 reasoning 的计量和截断语义必须逐 provider 核验。自定义 Provider
需遵守请求限额并诚实报告；违反则显式记录，不能靠接口契约假设消除实际风险。

| 路径 | 预算结算 charge | 用量与审计 |
|---|---|---|
| 完成且完整 usage 已知（含 labels 校验失败） | actual，退/补差额 | 记录实测，非法输出仍有成本 |
| 已派发但 usage 缺失、请求失败且无 usage、Provider panic | reserve | 标记未知与估算；有已知部分则另记已知部分，不冒充完整 |
| 已预占但尚未派发即取消/跳过 | 0，全退 | executed=false；无模型请求 |
| 在飞取消 | 有完整 usage 则 actual，否则 reserve | 记录取消及完整性状态 |
| actual > reserve | actual，允许账本超过 cap | over_reserve 记录模型、reserve、actual；后续派发按剩余额度拒绝 |

每个调用只有一个 reservation owner，单调状态为未派发/已派发/已结算；
ReserveGuard 的 Drop 按状态兜底，所有显式结算和兜底路径只能生效一次。
单次与 batch 使用同一准备/执行/结算路径，batch 不得在 gate 与 LlmTool 各预占一次。
预占使用 hooks Modify 后的有效请求；修改 model/max_tokens 后重新做参数与 allowlist 校验。

**防重复记账**：当前 `run_code.rs` 会把 `usage_in_shared_budget=false` 的 usage
再次 `share`。LlmTool 已自行结算预算，故返回 `usage_in_shared_budget=true`；
既有折叠点只合并实测 totals。cap=0 时跳过预占，实测与未知用量审计仍保留。
失败、取消、panic 的审计汇总不能只依赖成功返回；M0/M3 必须验证失败出口。

共享 gate 的外层调用、压缩与其他工具未全部改成预占，本轮不扩展到这些调用。
即使某个内层模型能证明 reserve 上界，也不能据此宣布全 run 硬封顶。

## 4. callBatch：只读并行批量调用（吞吐原语，独立实现里程碑）

**吞吐动机**：例如 200 次独立调用、平均 2s，顺序执行约 400s；并发可降低等待。
该例是估算，不把 AppWorld 工具调用数当作语义 LLM 调用数。分组推理、减少调用
和并行都是候选，V0-A 先比较；callBatch 的收益受限流、尾延迟和 Provider 串行化影响。

**契约（v0.4 收窄版）**：

- prelude 新增 `tools.callBatch([{name, args}, …])`，内部走保留名 `__call_batch`
  进 host。**保留名只是入口，内部不豁免任何闸门**：对每个 entry 跑与单次调用
  完全相同的序列（入口检查 → gate → deny/fs 层 → 执行 → after）；
- **只读面**：仅 `batchable()` 工具可入批（LD-19）；非 batchable entry →
  `kind="batch"` 错误条目（单条不中止）。副作用 = guest 顺序 for 循环（skill ④）；
- **每波 gate 在 spawn 前串行**（"approval decisions never race"，对齐 loop 并行批的
  契约）——**预占也在该串行段按提交序逐条完成**（LD-17：防止同波共享额度被重复承诺；
  实测超估算仍可能发生，LD-17）；8/波扇出（对齐 loop 扇出口径）；波间 +
  worker 入工具体前 cancel 复查（既有两道复查纪律）；
- **结果按提交序返回**（数组顺序稳定；事件仍按完成时点发出，不保证模型输出确定性）；
- **返回形状 = 封套的数组**：每个 entry 一个 D12 双封套
  （`{"__ok":true,"content":…}` / `{"__ok":false,…}`），prelude 把 `__ok:false`
  臂转成 `Error` 实例（带 `e.kind`/`e.tool`，与单次 throw 的错误同款字段）——
  **不是一个封套装一个数组**。单项失败自包含，guest 侧 `instanceof Error`
  判别。碰撞论证：`JSON.parse` 永不产出 Error 实例（JSON 值域是 plain
  object/array/原始量），成功条目不可能伪造失败；封套键位是宿主保留通道
  （D12 裁决），合法工具输出只出现在 content 值域；
- 额度：K 条计 K 次调用计数；token 侧逐条预占（LD-17），预占失败的尾部
  entries 得 budget 类错误条目（`halted:false`）——**部分执行显式可见**：结果
  数组如实反映哪些 entry 被拒纳；批量结果必须逐条检查（skill ④ 钉死）；
- **批内失败语义（审批钩子的契约面）**：entry i 的 hooks Deny → 该 entry 变
  `kind="hooks"` 错误条目（`after_tool(executed=false)` 照发），**批次不中止**；
  cancel 落下 → 终态——已过 gate 未启动的 entries 补 `kind="cancel"` 错误条目
  （对齐既有"装配段补发取消 ToolResult"纪律），在飞 entry 尽力响应 cancel，
  未能即时中断的调用按其请求限额结束并如实结算；
- **无事务语义（三句话，README/skill/描述同源）**：无原子性；无回滚；部分执行
  是正常结果。次序保证仅存在于 guest 顺序 for 循环（副作用批量）；
- 事件：gate 期按提交序串行发 ToolCall，各 entry 完成即发 ToolResult。

**skill 陷阱行**：失败条目细节取 `.kind`/`.message`——不要 `JSON.stringify`
失败条目（Error 序列化成 `{}`，细节丢失）。

**timeout 指引公式**（写进 README/skill，不给魔数）：
`timeout_secs ≈ 单调用预期时长 × ceil(批量规模 / 有效并发度) + 计算与限流余量`。
有效并发度不大于 8；仅作规划估算，不是最坏时延保证。

**实现注记**：M0 先验证并行波的可复用边界，M2 再提取必要的公共逻辑；既有
loop 行为应有回归保护，不能把适配新预算生命周期描述为“仅搬运”。评测门照带。

## 5. 配置面

```rust
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    pub enabled: bool,                  // default false（显式信任决策，D15 同款）
    pub default_model: Option<String>,  // enabled=true 时必填
    pub models: Vec<String>,            // allowlist；须非空且含 default_model
}
// CodeConfig 增 pub llm: LlmConfig、pub call_batch: bool（LD-15）、
// pub include: bool（姊妹稿 MD-13）；三者 default false；
// feature-off 构建照常解析（纯数据定位）
```

启用校验（config 解析期）：`enabled=true && (default_model 为 None || models
为空 || !models.contains(default_model))` → 配置错误，文案 actionable。

**batch 片段合并**（`PartialCodeConfig` 穷尽解构增臂，反化石纪律）：

| 字段 | 片段覆盖策略 |
|---|---|
| `llm.enabled` / `llm.default_model` | 片段出现即拒（bad_task；能力开关与成本分层的缺省是部署决策，不归任务文件） |
| `llm.models` | **交集**（只能收紧）；交集结果不含 default_model → bad_task |
| `call_batch` | `Clamp::Tighten`（false < true：实验方（base）可开，不可信片段只能关） |
| `include` | `Clamp::Tighten`（同上，姊妹稿 MD-13） |

绑定层：`set_code_config(dict)` 经 serde 缺省自然到达，**零新 API**（FD-1 先例）。

**评测门版本束（v0.4）**：`llm.enabled` / `include` / `call_batch` 三个默认关的
gate 构成可回滚版本束——各自独立开关，但 prelude 助手、描述行、skill 段作为
**同源三处**随对应 gate 整体出现/消失（关 = 模型可见契约与旧版一致，A-L8/
A-M 系列钉死）。§10.3 的"版本整体切换"以这三个开关为机制载体。

## 6. 安全模型

### 6.1 责任与权限边界

宿主决定工具权限、允许模型与批准政策；外层模型负责规划和抽检；库保存程序与
证据；内层模型提供单次输出。任何模型都可能误判或被注入，不能按“外层天然可信、
内层天然最弱”分配安全保证。

**执行政策**：判别结果默认只产出建议或可逆操作；删除、发送、支付等动作必须
满足宿主对该动作的授权政策，并按场景要求复核。外层模型重新判断不等于用户
授权，labels 命中也不等于判断正确。v1 首发试点仅输出分类文件。

这是部署与任务政策；通用运行时没有结果来源追踪，不能自动证明 guest 后续
写操作是否由某个 label 驱动。若部署必须强制该边界，应在工具权限/hooks/隔离层
限制相应副作用。代码资产的准入与宿主可信部署要求见姊妹稿 §5。

### 6.2 数据与调用治理

1. 输入正文可诱导判别错误。labels 只拦截词表外输出，不能拦截词表内的恶意
   翻转；以领域基准、实际输出抽检和副作用权限共同限制影响。
2. 同一 Provider 实例保证复用接入路径，**不保证相同数据接收方**：网关和自定义
   Provider 可以按 model 路由到不同后端。部署方须按实际后端确认数据政策，
   allowlist 是部署选择，不是“零外传增量”的证明。跨 provider 将来需另审。
3. llm 调用经过 hooks，宿主可按 `__code__`、有效模型和输入限制调用；请求/结果
   trace 沿用宿主的数据保存政策。工具调用审计与主 turn 的 `on_chat_request`
   是不同入口，不暗示后者自动观察所有内层请求。

### 6.3 语义质量政策与交付闭环

**三层验证对象，各证各的，不可互相替代**：

| 验证 | 能证明什么 | 不能证明什么 |
|---|---|---|
| 库的纯断言自测（姊妹稿 MD-4①） | 程序的部分结构与逻辑符合预期 | 语义是否正确 |
| 固定标注样例上的 `verify(n)`（姊妹稿 §3.6） | 当前判别**链路**还能通过这些已知样例（**链路健康检查**——固定样例是回归桩） | 本次实际处理结果的质量 |
| **本批实际输出的独立抽检** | 在抽样设计和政策阈值内支持批次接受决定 | 全量零错误；未来链路持续健康 |

v0.5 曾把 `verify()` 又用于"批量完成后的抽样回验"——**重跑固定样例不能替代
对本批实际结果的抽检**（v0.6 修正）。既有评测证据支持这一定级：语义谓词
判错后被代码批量放大、内部"验证通过"仍未通过最终状态断言
（`docs/run_code_routing_matrix.md:84`）。

**质量政策由场景负责人预先定义，运行时不内建通用质量引擎。**每次试点的
评测配方或随库文档至少包含下列内容；没有政策时仍可探索运行，但交付只能标为
“未验证”，不得仅因抽过几条就称整批合格。

| 政策项 | 必须明确的内容 |
|---|---|
| 判定标准 | 标签/业务规则版本、重要错误类别、各类错误与遗漏的可接受阈值、最低覆盖率 |
| 检查方式 | 抽样单位、随机/分层方法、样本量依据、关键与少数类别覆盖；高风险项是否全检 |
| 复核依据 | 原文与标准可查；人工/业务 gold 或独立复核者；外层模型复判仅为辅助证据，不假定错误独立 |
| 失败处理 | 扩大抽检、重跑受影响集合或转人工的触发条件、预算与责任人；复核发现的系统性错误不得只修样本 |
| 发布条件 | 满足哪些证据才交付，谁负责决定；预算不足以完成复核时标为待复核/部分完成 |

不规定通用“3 条或 5% 即合格”。抽样只能支持其设计和样本量允许的结论；零样本
错误不能证明零总体错误。质量阈值与成本阈值须在查看正式留出集成绩之前冻结。

**交付契约**（由库与任务程序实现，宿主/评测器验收，不新增模型工具 API）：

1. 逐项保留对象 ID、输入快照或版本/哈希及原文定位、预测、使用的库/模型版本。
   仅行号不足以定位会变化的源文件；必须能回到当时输入。
2. 结果落盘，摘要只返回计数、异常和文件指针。处理状态（done/failed/pending）
   与验证状态（未验证/抽检通过/全量核验通过/待复核）分开；done 只表示已处理。
3. unsure、非法输出与抽检失败都有显式出口；分母保留被拒答、跳过和失败项，
   禁止靠只处理容易样本提高准确率。
4. 批内按可恢复块落盘 checkpoint，记录输入范围、已完成 ID 和质量记录。call-limit、
   cancel 或 deadline latch 后可能无法再 write_file，不能承诺停机后补写。恢复以
   最近持久化 checkpoint 为准；末尾在飞/未落盘项标为待确认，首发只读任务可重算。

**领域质量基准**：业务方维护与库生成过程分离的留出数据；开发集用于迭代，正式
留出集用于验收，反复查看后调 prompt 的数据应转为开发集。质量证据绑定程序及
依赖版本、模型配置、数据集版本、业务标准与评测方法（姊妹稿 §4），不以日期和
一次 verify 成功替代。首个工单试点按以上政策登记；无真实语料或业务阈值时不得
自行编造产品成绩。

### 6.4 护栏失效声明（正文级，勿埋注释）

打转护栏的键是（工具名, canon 生效参数, 结果指纹），"同参不同果即进展、不计数"
——与 LLM 非确定性**正交**：guest 写 `while (judge(x) != "yes") {}` 每次结果不同
= 永远计为进展，第 3 次 nudge、第 6 次 stuck_loop 都不触发。**兜底只有 BudgetGate
（估算派发约束）+ max_tool_calls + 墙钟/请求超时**。评测与文档不得暗示护栏仍覆盖
语义重试。

## 7. 测试计划（进程内 mock + e2e）

**前置（M0，评测与拓扑的地基）**：`provider/mock.rs` 脚本化 Provider 增按请求
`model` 字段分流脚本（外层/内层两套序列对拍——同时**实证 LD-16**：自定义形态的
Provider 收到内层 model 请求并正确服务/诚实报错）；`scripts/e2e_scenarios.py`
的 SSE mock 认第二个模型名；**usage 缺失策略钉子**（mock 返回无 usage → 预算
保留 reserve、实测不伪造，A-L2b）；`gate.reserve/settle` 原型与并发预占压力测试；并行证明遵循
既有纪律——ThreadingHTTPServer + 时间区间重叠（功能单测证明不了真并发）。

| 验收 | 测试 |
|---|---|
| A-L1 最小闭环 | mock 分流下 guest 返回 text；实测 usage 折 totals 一次，预算结算一次，单次/batch 均无重复 share；cap=0 仍有完整审计 |
| A-L2 预占/结算（v0.7） | tokens_left 反映在飞预占；**A-L2b** 缺失/部分 usage → 保留估算预算、已知量与未知调用单列、totals 不伪造；**A-L2c** 矩阵逐路径验证正常/输出校验失败/请求错误/取消/panic/未启动释放，guard 恰好结算一次；actual 超 reserve 可审计；预算拒绝 halted=false，调用数终止 halted=true |
| A-L3 hooks/事件 | Deny llm 调用（`__code__` 标记）；ToolCall/ToolResult 事件对完整；按宿主既有数据政策记录请求与 trace，保留模型/有效参数归因 |
| A-L4 并行 | callBatch × llm 8 路（e2e 时间区间重叠证明）；结果按提交序；单项失败为 Error 条目（`instanceof` 判据、kind/tool 字段）；批内 Deny 单条不中止；**并发派发不重复承诺额度**（每波 used + 新预占 ≤ cap；另测 actual 超 reserve，不将两者混为硬封顶）；非 batchable 工具入批 → entry 级 `kind="batch"` 错误 |
| A-L5 护栏失效钉住 | `while (judge(x) != "yes") {}` 不触发 stuck（确认额度/墙钟兜底收尾——把"护栏不覆盖"钉成显式预期行为） |
| A-L6 词表与契约 | `kind="llm"` 错误封套字段；model 不在 allowlist → 明确错误；Modify 后重验 allowlist/限额并仅预占一次；temperature 按请求设置且不改变外层默认；**labels：命中返回规范 label、大小写/引号变体归一命中、非法/空/多项输出 → `kind="llm_output"`**（LD-18） |
| A-L7 配置 | default_model 缺失/models 不含缺省的解析错误；batch 片段策略逐字段（含 call_batch/include Tighten）；enabled 翻转的注册表快照边界 |
| A-L8 评测门 | 三 gate 全关时新二进制模型可见契约与旧版一致（非新旧构建所有输出字节相同）（prelude 不暴露 callBatch/include、描述不含其行、llm 不在注册表）——对照臂可复现性的钉子；**保留名直调（v0.5）**：gate 关闭时 guest 直调 `__call_batch`/`__include`/`__hash_file` → 明确拒绝且无执行；gate 开启时 `__call_batch` 直调走与 prelude 包装相同的全链 |
| A-L9 Provider 拓扑（v0.5 扩） | 同一 Provider 实例收到内层请求且 model 字段正确；自定义 Provider（mock 形态）serve 不了 model → kind="llm" 诚实错误；晚填充先于首次调用（注册表惰性构建次序）；**clone 身份钉子：Agent 持有实例与 guest 注册表 clone 共享同一 `Arc<OnceLock<…>>` cell，晚填充对全体 clone 可见**；**Provider panic → 预算/审计一致（已派发的 ReserveGuard Drop 保留估算，未知用量可见；未派发全退）**；串行 Provider（互斥锁实现）照常工作（吞吐下降被接受） |

**产品交付验收（与运行时测试分开）**：

| 验收 | 检查 |
|---|---|
| A-L10 交付与质量（V0-B） | 独立留出判卷；拒答/错误不退出分母；抽检失败触发既定升级；取消/额度终止以 checkpoint 交付部分结果，无结果丢失后冒充完整；缺质量政策时不能标合格 |

## 8. 评测方案（分期矩阵，复用 routing 方法论）

**（v0.4 重构）v0.3"四臂"实为三臂，且无法分离三个特性的贡献**（专家 2 P7）。
改为分期矩阵，每期相对上一期只动一个变量；每期 ≥3 重复、交错、任务状态重置、
预标注任务形状（外层做得到的 / 机械批量适合的 / 语义批量适合的 / 超窗语料可分解
的 / **超窗但跨块耦合的——预期不换轨**）：

| 期 | 配置 | 隔离的变量 |
|---|---|---|
| E0 | 现行基线（v2+on_demand 定型版） | — |
| E0′ | E0 + **新 skill（边界句重写）仅此一项**（llm/include/call_batch 全关） | 文案/路由效应 vs 能力效应分离 |
| E1 | E0′ + **include（llm 关）**，cold/warm 分开跑 | 库复用本身的收益；摊销假设的首轮数据 |
| E2 | E1 + **llm（call_batch 关）** | 语义批量的正确率/成本效应 |
| E3 | E2 + **call_batch** | 墙钟改善 + 正确率无回归 |

E0′ 的新 skill 是实验装置显式提供的提示词变体，记录独立指纹；它不要求门关
二进制自动暴露未启用能力。正式披露仍按 §5 随 gate 变化；E0→E0′ 比较文案，
E0′→E1/E2/E3 比较对应能力及必要披露的整体增量，不声称运行时和披露完全分离。

### 8.1 外部对照与总成本

四路线：①外层直接处理；②强模型适度分组；③普通脚本 + 模型 API + 文件库；
④TinyAgent 编排。各路线获得相同数据权限、业务标准和质量门槛；允许各自合理
调优并预先限定调优资源，不把代码路线独享的高质量库/标注知识藏在对照外。

分开记录模型调用成本、基础设施成本、开发/复核/维护工时；只有声明工时单价和
假定复用次数后才能合成货币 TCO，并给复用频率/人工单价变化的敏感性结果。
模型费按实际模型、输入/输出/缓存/reasoning 的供应商计价口径计算，避免重复
计算已包含在 output 中的 reasoning；价目与日期存档。不要求核心实现价格引擎。
缺 usage 时报告已知量与未知范围，该样本不能静默计零或据此宣布成本优势。

**单位成功成本 = 全部尝试的总成本（含失败、重试、复核和维护摊销）/ 成功任务数**。
成功须同时满足预登记的质量、覆盖率与交付完整性要求；全失败时该比值不定义，
报告零成功及总消耗。另报告任务成功率、重要类别错误、覆盖率、拒答/升级率、
人工介入工时和延迟分布；双成功子集仅作诊断，不替代全样本结论。

cold（建库/调试/审批）与 warm（复用）分别报告；“5–10 次回本”为待测假设。
每期保留机械批量与小任务负例，计入描述/skill/索引的常驻成本。E0–E3 是条件
增量实验，不能据此识别所有特性交互；必要时补 llm 无 include 的定向消融。

### 8.2 预登记配方与任务变化

正式开跑前冻结并保存：数据分割/版本、任务与规模分层、模型与网关配置、价格、
质量政策、成功判据、成本/时延/人工门槛、调优预算、重复次数、缓存与重试规则。
数值由首发业务场景和可接受服务标准确定；允许用开发集校准一次，冻结后不得按
留出集成绩改门槛。样本不足或区间无法区分时结论为“证据不足”，不强行判胜。

任务至少覆盖四种条件：固定任务重复、新输入分布、同领域规则/接口变更、明显
不适用旧库的任务。记录正确复用、必要修订、正确放弃、误用旧库和维护时间。
选择库时不提供“应选哪个库”的答案；测试分割防止 warm 库从留出答案中生成。
固定任务只证明摊销，变化任务才检验 Agent 增量，结论按任务族分开。

每期至少三次交错重复作为初探；重复不能替代独立任务样本。记录缓存命中和首轮
用量，按任务聚类分析不确定性；网关失败保留并按预登记规则另报，不事后删掉。

## 9. 价值验证与实现里程碑（v0.7）

V0-A 先验证工作方式，V0-B 在最小实现后验证产品集成；**V0-B 不作为 M1～M3
开始的前置**。M0 可与 V0-A 并行，但应限定原型的时间/工程预算，不把探针扩成
全量重构。M1～M3 的最小闭环以 V0-A 通过为投入门；正式默认启用、推广与 Tier-2
扩展以 V0-B 为门。若纯函数 include 独立有收益，可独立评估保留，不能让一个
特性的成绩替整个组合背书。

| 阶段 | 工作 | 出口判据 |
|---|---|---|
| V0-A 方法验证 | 用现有 run_code、临时 MCP/适配器或实验脚本和人工预置库完成四路线初探；模拟加载不冒充 native include；先冻结 §8 配方 | 同质量条件下存在达到预登记成本/时延/人工目标的任务族；明确临时装置局限与继续/停止/证据不足结论；不宣称已验证 native 部署成本 |
| M0 技术探针 | mock model 分流、同实例/clone 身份、reserve/settle 与失败账本；仅验证所需并行机制 | A-L2b/2c/9 原型通过；确认主循环重构边界、非流式消费与 provider 按请求参数方案；未通过则先修设计 |
| M1 include 最小闭环 | 宿主加载/哈希边界、manifest、会话登记、默认流程准入、独立 gate；模板文档与行为同时落地 | 姊妹稿 A-M1～M7、M9～M12 默认流程项；A-M8 随 M3 完成，宿主可信配置随 V0-B 验收 |
| M2 callBatch | ✅ 落码（v0.7.1）：只读能力、同一路径的逐波 gate/执行/结算、保留名门控；token 预占留 M3（llm 是唯一 token 消费面） | A-L4/8（`batch_*`/`gate_on_description_lists_batch_helper`/`skill_gated_sections_ride_their_gates`，并发以进程内时间戳重叠证明）；真实 LLM 吞吐留 M3/V0-B |
| M3 llm 最小闭环 | ✅ 落码（v0.7.2）：LlmTool、共享 Provider cell、LD-17 预占/结算全矩阵、LD-18 labels、`meta.llm` 审计、描述/skill 披露、e2e llmbatch；差异记录见修订 ⑪ | A-L1～9 进程内 + e2e 通过（A-L4 以服务端服务窗口重叠证真并发）；A-M8 见姊妹稿；全部结算出口、修改后参数校验及既有回归通过；A-L10 交付示例机制检查完成，真实质量验证留 V0-B |
| M4 契约与评测装配 | ✅ 落码（v0.7.3）：契约汇总（kind="llm" 补进两个教学面 + A⑤ 钉住 LLM overlay）、A③ 冻结 + E0′ 装置（`examples/eval/e0prime-skill/`）、E0–E3/四路线装配文档（`docs/code_llm_eval_assembly.md`）、装置漂移门进 CI；差异见修订 v0.7.3 | sync_run_code_contract 与门关回归通过（既有钉住测试 + sync/gen 门全绿）；配方可复现（`check_eval_apparatus.py` 指纹/A③/渗漏红灯化）；文案随各功能开发未等 M4（三特性各自落码时已三面同步） |
| V0-B 集成与生命周期验证 | 最小实现上跑 E0–E3/外部对照及变化任务；覆盖起草、发现、修订、晋升、证据失效/重新验证，计入人工与部署成本 | 满足预登记质量与覆盖率底线，达到选定任务族的成本/时延/维护目标且负例回归在容差内；宿主可信部署另验策略；不满足则缩范围/保留独立原语/停止扩展 |

V0-A→M1/M2/M3 最小实现→M4→V0-B；M0 与 V0-A 可并行，M2 依赖 M0 并行与
预算探针，M3 依赖 M0 的 Provider/账本结论。姊妹稿 M1/M2 对应本稿 M1 的加载与
模板部分；其 M3 是 Tier-2，必须在 V0-B 后另立项，避免编号相同造成混淆。

统一边界句先于正式评测冻结；A-L/M 测试证明实现遵守契约，V0 报告证明场景价值，
两者不能互相替代。M0 的 mock 通过也不等于真实模型、网关或自定义 Provider 的
成本与吞吐已验证。

## 10. 共享前置与常备纪律

### 10.1 统一边界句（属主登记）

三特性落地后新增语义批量路线，统一边界句的**属主
= 本稿附录 A③**。正式评测前冻结文字与按 gate 披露的规则，记录各臂实际提示词
指纹；E0 保留旧文案，E0′ 按 §8 显式改变文案，其余差异不得藏在临时提示词中。
起草、评审、skill/描述同步及 manifest 更新组成一项交付物。

### 10.2 核心执行与宿主治理的分工

> 核心维护正确执行所必需的状态与可审计契约；长期调度、组织审批与资产治理
> 由宿主承担。按责任和生命周期判断，不按“有没有状态”或“是否持久化”一票否决。

预算计数、调用生命周期、会话登记与工具缓存属于执行状态；任务 checkpoint、
会话存档、代码和证据文件可以持久化。它们的存在不等于核心成为组织治理平台。
审批政策、批准者身份、资产发布/撤销和长期调度归宿主，核心通过 hooks、文件
格式、绑定 API 和事件提供接入点，不把“可接入”写成“宿主能力已实现”。

本轮不做持久 JS 堆、核心代码资产数据库、调度服务或自动晋升。理由分别是执行
生命周期/回放成本和范围控制；持久 JS 堆不一定跨重启，不能用“跨重启治理状态”
概括所有被拒方案。以后提案需解释新增责任及收益，不沿用绝对化禁令。

### 10.3 评测门常备化

> **任何 LLM 可见的行为变更（描述/技能/prelude 助手/错误文案），评测收官前必须
> 带评测门：配置开关，默认按既定行为；收官后保留或移除再议。** 先例：
> `skill_injection` 三态。

本方案三个 gate（`llm.enabled` / `include` / `call_batch`）即该规则的机制载体
（§5 版本束；专家 2 P8：include 也必须有独立门——v0.3 的"随 M1 一并带门"口头
承诺升格为显式配置项）。

### 10.4 登记不立项

- **parse-only 预检**（QuickJS compile 不 run，近零成本语法检查）：三特性后 guest
  程序从几十行涨到几百行，全量重发税随规模线性涨——其价值被结构性抬高。登记为
  P3 候选，不进本方案；
- **描述上限重议**：当前接近 1200 字符上限，以同步脚本为准，三特性共摊扩容一趟走（include 先行作
  探路弹，姊妹稿 §9）。

## 附录 A：LLM 可见文案（草案规格；落码即冻结，变更走本文修订记录）

> 体例对齐既有两份设计稿。以下为**草案**——评审通过并落码的 commit 起成为规格
> 基线，微调数据按此放量。

**① tools.list() 中 llm 条目的描述**（≤200 字符，前置关键信息）：

```
调用 LLM 做单次判别/生成（无工具、无历史、无流式）。适合逐项语义判断：分类、抽取、
校对、打标。labels 校验词表，不保证判断正确；副作用遵循宿主授权政策。返回
{text, label?, usage, model}。
```

**② skill（run-code）新增段落草案**（要点，正文措辞落码时定稿）：

- 判别纪律：输出词表用 `labels` 声明（含 unsure 显式词），**且 prompt 里同写
  "输出必须是这些词之一"**（宿主只校验不注入提示）；未声明 labels = 生成模式
  （任意文本，若作结构化抽取须在库内另行校验）；非法输出得 `kind="llm_output"`——分支前先判
  label 合法性；unsure 单独计数进聚合摘要；`max_tokens` 用目标模型验证后设小；
  **首发只产出只读结果**；其他部署的副作用按宿主授权政策执行，外层复判不等于用户授权（§6.1）；
- 成本纪律：开工前 `tools.budget()` 查 tokens_left（**含未结算的预占**，LD-17），
  循环里定期查；429/5xx 保留失败项并交外层诊断，系统性限流须宿主延后处理；
  `e.kind === "budget"` 时查 **`e.halted`**：true = 沙箱已停，只能 return 收尾；
  false = 额度关口拒绝但沙箱活着，可 write_file 落盘部分结果再收尾；
- 批量形态：`callBatch` 并发只读调用（分页拉取、逐项判别），结果按提交序；失败
  条目用 `instanceof Error` 判别，细节取 `.kind`/`.message`——**不要
  `JSON.stringify` 失败条目（Error 序列化成 `{}`，细节丢失）**；**批量结果必须
  逐条检查结果数组**（部分执行是常态而非意外）；**callBatch 不接受副作用工具
  （无原子性/无回滚），副作用批量写顺序 for 循环**；
- 级联模式：便宜 model 先判，`unsure`/`llm_output` 错误升级贵 model（示例代码
  块，进 run_code_skill.rs 回归）；
- 质量交付：按预先给定的场景政策抽检本批实际输出（不是重跑 verify）；保留
  原文版本与定位。没有质量政策或未完成复核时标未验证/待复核；抽检通过不等于
  全量正确。unsure、错误与未完成项保留在分母和交付清单。
- 恢复：按块提前保存 checkpoint；不要等 quota/cancel/deadline 已阻断工具后
  才尝试落盘。恢复时只读任务可重算待确认项；副作用另需宿主的确认/幂等政策。

**③ 统一三选一边界句（属主本稿，v0.7；v0.7.3 冻结——文字变更走修订记录；E0′ 装置载体 `examples/eval/e0prime-skill/`，指纹登记见 `docs/code_llm_eval_assembly.md`）**：

```
面对剩余任务先三问：
1. 一次工具调用或一条可用 shell 管道能否完成？能 → 直接做。
2. 剩余是否规则明确的同构批量（主要变化是游标/页码/对象 ID，或大量中间数据）？
   是 → 代码承接（机械批量）——先查 lib/INDEX.md，有现成库直接复用。
3. 剩余是否大量同构的语义判断（每项需理解内容，判断规则可写成明确 prompt 与
   封闭输出词表），语料可划分为独立（或有界上下文）单元、聚合规则明确可验证，
   且每项结果可独立核对（外层窗口装不下也适用）？是 → 代码 + llm 承接
   （语义批量）——同样先查 lib/INDEX.md；输出带原文版本与定位，完成后
   按场景质量政策核对。
判断规则未定型或分块遗漏必要的跨项语义关系 → 外层先明确规则和分块策略。
按 ID 去重、排序等规则明确的全局运算仍可交代码。不能仅凭"所有/批量"触发。
没有质量政策不能声称结果合格；不可逆动作须满足宿主授权政策。
```

**④ callBatch 的 prelude/描述行**（同源同步进描述与 skill）：

```
tools.callBatch([{name, args}, …])：并发执行一批只读工具调用（read_file / llm 等
batchable 工具；按提交序返回数组；无原子性、无回滚、部分执行是正常结果——逐条
检查）。单项失败不抛、该条目为 Error 实例带 e.kind/e.tool——用 instanceof 判别，
不要 JSON.stringify 失败条目。并发上限 8/波；分页拉取与逐项判别是理想对象；
副作用调用不在批内——写顺序 for 循环。
```

**⑤ run_code 描述的【LLM 判别】overlay 段**（v0.7.3 钉住；仅 code.llm.enabled
开启时追加在描述尾部，基字面不动）：

```
【LLM 判别】（仅 code.llm.enabled 开启时；关闭时无此工具）tools.call("llm", {prompt, system?, model?, max_tokens?, labels?})：单次模型调用（无工具、无历史、无流式）——大量同构的逐项语义判断（分类/抽取/校对/打标）用代码循环 + llm 承接；model 须在 code.llm.models 内（缺省 default_model）。声明 labels（封闭词表，≤16 项）时宿主校验输出并返回规范 label，未命中 e.kind="llm_output"、其余失败 e.kind="llm"（均无 retryable，读 message 自判）；词表要求必须同时写进 prompt（宿主不注入）。返回 {text, label?, usage, model}。token 成本发前预占、也计调用额度。判别纪律/成本纪律/级联详见 run-code 技能的「LLM 判别」段。
```

```
tools.callBatch([{name, args}, …])：并发执行一批只读工具调用（read_file / llm 等
batchable 工具；按提交序返回数组；无原子性、无回滚、部分执行是正常结果——逐条
检查）。单项失败不抛、该条目为 Error 实例带 e.kind/e.tool——用 instanceof 判别，
不要 JSON.stringify 失败条目。并发上限 8/波；分页拉取与逐项判别是理想对象；
副作用调用不在批内——写顺序 for 循环。
```
