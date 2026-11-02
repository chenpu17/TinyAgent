# 宿主集成需求单（TinyWorker → TinyAgent，2026-11-02 批次）

| 项 | 值 |
|---|---|
| 状态 | v1.2（宿主侧 FR-1/3/4 接入已落地；FR-5 基准数据回传见 §7） |
| 日期 | 2026-11-02 |
| 修订记录 | v1.2（2026-11-03）：宿主侧按答复单 §5 接入清单落地 FR-1 观察者仲裁（`on_turn_stopping`）/ FR-3 档案往返 / FR-4 运行台账配对钉，FR-5 基准数据回传（§7，design 输入）。v1.1（2026-11-03）：按答复单 `host_integration_requirements_response.md` 对齐——FR-2 动机更正（`reasoning_content` 解析基线已在，系需求方 grep 输出截断误判；范围收敛为内联 `<think>` 剥离）；FR-1 事件口径（只发非默认决策）与定版语义指向答复单；FR-3 导出语义（含预置位的进程内累计）。v1.0：初稿 |
| 需求方 | TinyWorker（宿主：tauri 桌面壳 + webui 服务器壳，path 依赖 `tiny-agent-core`） |
| 需求来源 | ① DeepSeek Harness（dsh，2026-08-13 开源，MIT）架构调研对照；② TinyWorker 七轮审计的记录在案项（受核心签名约束而搁置者） |
| 通道 | 「需求单 → 用户中转」（RSI 红线确立的 TinyAgent 改动通道），本单为该通道第一份 |
| 关联文档 | `docs/run_code_requirements.md`（格式先例）；`docs/lib_api.md`（现有公共 API 面） |
| 文档性质 | 需求清单（每项含动机/内容/验收/规模），非实现设计；方案取舍留给各 FR 的 design 文档 |

---

## 1. 背景与范围

dsh 的核心论点（[NYU RITS 解读](https://rits.shanghai.nyu.edu/ai/deepseek-harness-cordis-everything-is-a-plugin)、[DeepInfra 实测](https://deepinfra.com/blog/deepseek-harness-review)）是「Agent = Model + Harness」，且基准分数衡量的是模型与 harness 的配对——harness 的上下文预算、测试反馈、重试策略、工具 schema、停止条件对分数的影响超过相邻前沿模型间的差距（[menuagentic](https://menuagentic.com/blogs/deepseek-harness-scores-include-the-scaffold)）。本单只收录**核对过 TinyAgent 现状后确认存在真实缺口**的条目；已对齐或已更优的能力见 §2，经评审否决的方向见 §5。

范围：内核公共 API 与行为（`crates/core`）。不含宿主侧落地（TinyWorker 侧配套另行排期），不含 bindings 除非某 FR 明确触及。

## 2. 已满足 / 对齐确认（明确不做，避免重复立项）

以下 dsh 能力经 2026-11-02 逐项核对，TinyAgent 现状已满足或更优：

| dsh 能力 | TinyAgent 现状 | 结论 |
|---|---|---|
| pre-step 上下文注入 | `Agent::supervisor_handle()` 旁注注入：每次 LLM 调用前排空队列、FIFO 至多一次、未投递出口冲账（CHANGELOG [Unreleased] R1） | 已覆盖「注入」诉求，通用改写钩子暂缓（见 X-3） |
| 出站请求可观察 | `Hooks::on_chat_request` 只读观察 | 已有 |
| 子代理冷启动 + 工具作用域 | `task`：own history、父白名单裁剪且禁递归、预算继承父剩余、per-child 审计 meta | 已有，与 dsh `inheritsParentContext:false` + scoped registry 同口径 |
| 压缩硬约束保护 | `SUMMARY_SYSTEM` 要求路径/标识符/命令/错误文本 VERBATIM + 「历史非指令」 | 优于 dsh（其自认不保护接口签名/验收标准） |
| 压缩触发分级 + 先剪后摘 | 阈值门 + 溢出检测（`is_context_overflow`）+ 手动 + 剪枝双门（`PRUNE_MIN_RESULT_CHARS`/`PRUNE_MINIMUM_CHARS`） | 持平 |
| 网关请求形状自适应 | `LearnedFixes`（400 学到的修复跨轮主动应用：`max_completion_tokens` 改名 / 丢 `stream_options` / 丢 `reasoning_effort`） | 部分覆盖，缺口见 FR-3 |
| 工具集裁剪（Minimal 档） | `Config::tools` 白名单（`default_tools` + `validate_tool_whitelist`，子代理白名单已消费） | 核心已满足；宿主暴露与评测口径属 TinyWorker 侧 |
| headless 一次性运行 | 根 crate `tiny-agent` bin：`harness.rs` 无头渲染 + `batch.rs` 批处理 + `--session`/`--report` | 已有 |
| usage/缓存记账 | `cache_read_input_tokens` 等随 RunOutcome 出账；压缩也记审计（账是账） | 已有 |

## 3. 功能需求

### FR-1 轮末仲裁钩子 `on_turn_stopping`（P0）

**动机**：TinyWorker 的 RSI 观察者判停目前在 manager 层于 run 结束后裁决——只能「事后停下一次」，无法在模型声称完成、循环即将以 `end_turn` 收尾的**当下**行使否决。dsh 的 `agent/turn-stopping`（serial，无委托）证明该点值得是一等扩展位；「观察者裁定未完成 → 注入旁注强令继续」是当前 cancel/inject 组合做不到的新能力。

**内容**：`Hooks` 新增带默认实现的方法，在循环因 `stop_reason == "end_turn"`（仅此一档，`max_tokens`/`stuck_loop`/cancel 属强制终止不仲裁）即将发 `Done` 前调用，返回：
- `Stop`（默认）：照常收尾，行为与今日完全一致；
- `Continue { note: String }`：不发 `Done`，note 以监督者旁注同款标记前缀并入尾用户消息，进入下一轮；继续轮数设上限（建议复用/对齐 `STUCK_STOP_AT` 的计数纪律），超限降级为 Stop 并在 stop_reason 标注仲裁来源。

**约束**：serial 调用（同现有 hooks 纪律）；实现内出错返回 `Stop` 而非 panic（hook panic 会留 unpaired tool_use——trait 文档既有告诫）；`--resume` 重建不带 hooks 的现状维持并在文档标注。事件口径（v1.1 对齐答复单 §2.1）：`TurnArbitration` 只发**非默认决策**（Continue 被兑现或被降级）——Rust trait 无法区分「未覆盖的默认 Stop」与「显式 Stop」，且默认路径零漂移要求事件流逐字节不变；显式 `Stop` 与无钩子 run 一致。定版语义以答复单 §2.1 为准：续跑帽 6（对齐 `STUCK_STOP_AT`）、确定性守卫（续跑帽耗尽 / `max_turns` 已到）在决策点预检并降级为 `stop_reason = "arbitration_capped"`（exit 0，note 只留在事件里）、竞态类守卫（cancel/预算/超时）轮首永远赢、子代理 run 与强制终止面（`max_tokens` 含 salvage/`stuck_loop`/Err/零块轮）从不仲裁。

**验收**：① 默认实现零行为漂移（既有全套测试不动）；② Continue 注入后模型可见 note 且历史/归档一致；③ 仲裁上限触发后 stop_reason 可辨；④ Continue 期间 cancel 仍即时生效；⑤ 压缩与仲裁同轮相遇时顺序确定（仲裁发生在当轮消息面定稿后）。

**规模**：中。宿主收益：观察者判停从「事后停调度」升级为「当场驳回虚假完成」，直接服务 RSI 判停闭环。

### FR-2 openai 兼容层推理通道（P1）——v1.1 更正动机、收敛范围

**动机（v1.1 更正）**：v1.0 称「现状完全不处理推理通道」，系需求方 grep 输出截断（`head -14` 掩蔽了 openai.rs 中段的匹配行）导致的误判——基线 `0d17f94` 起 openai 层已把 `delta.reasoning_content`（DeepSeek-R1/GLM/o 系直连）与 OpenRouter 的 `delta.reasoning` 归一化字段解析为 `ThinkingDelta`（钉子测试 `stream_reasoning_content_surfaces_thinking_not_text` 在案），该形态打回成立。真实缺口仅剩：vLLM 系（Qwen3 等，含 vllm-ascend 部署）不开 reasoning-parser 时正文内联 `<think>…</think>` 混入正文。RSI 对比评测（同任务跨模型）必须能分离推理与正文的诉求不变。

**内容（v1.1 收敛）**：流式状态机剥离内联 `<think>…</think>` 并映射到既有 `ThinkingDelta`（anthropic.rs 已有该事件与「thinking 不入历史」的成熟语义）；开关 `Config::strip_think_tags` 默认开，关闭即逐字节透传；跨任意 chunk 边界正确（半截标签不误切，未闭合块按 thinking 冲——fail-closed）；推理不入历史与计费口径的既定决策维持。

**验收**：① `<think>` 形态流供入，ThinkingDelta 与正文正确分离、正文无标签残留；② 历史不出现推理块；③ 同流混跑不串线；④ 关闭开关逐字节透传。`reasoning_content` 形态的验收由既有钉子测试承担，无需新增。

**规模**：中偏小。（答复单已落地：`ThinkStripper` + 6 组跨 chunk 边界钉子。）

### FR-3 学到的网关修复可导出 / 可预置（P2）

**动机**：`LearnedFixes` 是进程内运行时状态——每个新进程都要用一次真实 400 重新学一遍（浪费一次请求与延迟），且宿主无法把学到的修复持久化进模型档案。

**内容**：① `RunOutcome`（或事件）携带本次学到的修复集；② `Config` 增加可选预置入口，宿主把上次学到的修复喂回来跳过学习轮。凭据纪律同现行：指纹/导出只含修复布尔位，不含任何密钥值。

**验收**：① 预置修复后首轮请求即带修复（不再触发教学 400）；② 导出/预置往返等价；③ 未预置路径行为不变。导出语义（v1.1 对齐答复单 §2.3）：为**含预置位的进程内累计**（同一 Agent 多次 run 导出同一值），非单 run 增量——需逐 run 增量的宿主自行 diff 相邻 outcome。

**规模**：小。

### FR-4 有效配置指纹（P1）

**动机**：「分数是模型+harness 配对」——宿主要在运行台账与评测结果里钉住 harness 版本 + 配置，跨模型对比才可复现（TinyWorker RSI 多模型对比的直接前置）。现无机器可读的有效配置摘要。

**内容**：对**clamp/overlay 合并后的有效 Config** 计算稳定指纹（如规范化序列化后 sha256 截断），随 `RunOutcome` 或查询 API 暴露。

**约束**：指纹必须排除凭据值（env 名/headers 的值不进哈希，键名可进——与 config 手写 Debug 的既有纪律一致）；算法与截断长度定版后视为兼容面，变更须经 CHANGELOG。

**验收**：① 同配置同指纹；② 任一压缩/工具/预算阈值变动指纹变；③ 凭据值变更不影响指纹；④ overlay 顺序不同但合并结果相同时指纹相同。

**规模**：小。

### FR-5 消息免深拷贝读取路径（P1，方案待议）

**动机**：TinyWorker 热路径在 manager 锁内 `a.messages().to_vec()` 全量深拷贝（七轮审计 F-1 记录在案：受 `pub fn messages() -> &[Message]` 签名约束，宿主必须复制才能出锁；Arc 化只覆盖冷会话一半）。大会话下这是每次事件处理的全量拷贝。

**内容**：三候选，design 阶段定案：
- (a) 作用域借用回调 `read_messages(|&[Message]) -> T`，宿主在回调内取所需片段；
- (b) `Arc` 写时复制快照；
- (c) 增量拉取：消息已有 id（归档定位在用），按 id/序号取「自上次游标以来」的增量，需定义折叠/`normalize_adjacent` 改写面下的游标有效性语义。

**验收**：以 TinyWorker 热路径基准为准——锁内不再发生全量深拷贝；语义上与今日 `to_vec()` 快照等价（所见内容一致）。

**规模**：中。收益归宿主，但改动在内核消息所有权结构，需与 0.x 兼容窗口对齐。

### FR-6 压缩记录补折叠 id 范围 + 会话重建一致性钉子（P2）

**动机**：`CompressionRecord` 已有 trigger/dropped/kept/估算 token，但缺**被折叠消息的 id 范围**——宿主想在 UI/导出里点开「这段被摘要了什么」时只能靠数数对位。同时「从 session 文件 + archive 重建的消息面 == 实际发给模型的消息」这一不变量值得钉子测试固化（dsh 的「model-visible means logged」纪律；我们已有 archive，缺的是把它钉死的测试口径）。

**内容**：① `CompressionRecord`/`Compressed` 事件补 `folded_ids`（或首尾 id 区间，含与 archive 的对应关系）；② 新增重建一致性测试：任取含折叠的会话，load_with_archive 重建后逐 id 对账。

**验收**：折叠/未折叠/退化折叠（dropped=0）三类会话对账全绿；宿主 NDJSON 透传新字段零改动。

**规模**：小。

## 4. 实验 / 观察项

| 编号 | 内容 | 状态口径 |
|---|---|---|
| X-1 | `edit_file` schema 与 dsh/V4 的 `str_replace_editor` 对齐 A/B：若 V4 系在两件套上做过 RL，schema 形状可能白捡能力。各跑同任务集对比步数/一次成功率 | 仅在拿到 V4/dsh 训练用工具 schema 证据后立项；无证据不动 |
| X-2 | 子代理 plan-only 档（只出计划不执行）：可先在宿主侧用 `before_tool` 对 `ToolCtx::subagent` Deny 写类工具验证需求真伪，真需求再内核内置 | 宿主先行验证 |
| X-3 | 通用 `before_step` 消息改写钩子：注入诉求已被 `supervisor_handle` 覆盖，改写/裁剪暂无消费方 | 暂缓——不为没有消费方的 seam 扩面（dsh 159 插件的调试面是反面教材） |

## 5. 明确不做（否决记录，防止后续轮次重新论证）

- **全插件微内核（Cordis 式）**：Rust+Tauri 形态下等于重写；dsh 自身付出 159 插件调试面、rc 间破坏兼容、开源零生产证据的代价。现有 provider/tools trait 分层够用。
- **会话改单一 append-only 事件流**：现「折叠消息面 + archive」已满足审计与重建诉求，全面改造收益不抵风险；FR-6 的钉子测试固化该不变量即可。
- **PTC/Code Mode 编排工具**：`run_code`（wasm 沙箱 + tools.call 回调，Cloudflare Code Mode 同思路）已覆盖该能力面；除非实测出现往返瓶颈，不另起炉灶。

## 6. 兼容与流程约束（全 FR 通用）

1. `Hooks` trait 新方法**必须带默认实现**（0.3.x 签名变更致 hook 静默失效的既有教训）；
2. 0.x 阶段 minor 即破坏窗口（CHANGELOG 头部口径），涉公共 API 的 FR 在 [Unreleased] 单列条目；
3. path 依赖下内核改动即时影响 TinyWorker 编译——FR-1/FR-5 与宿主消费代码**同批落地或先合后吃**，不留中间不可编译态；
4. 钩子内出错返回默认值、不 panic（unpaired tool_use 既有告诫）；
5. 一切指纹/导出面不含凭据值（config 手写 Debug 的既有纪律延伸）；
6. 每项 FR 落地前按仓库惯例过独立审核轮次。

## 7. FR-5 基准数据回传（2026-11-03 实测，design 输入）

### 7.1 宿主调用面盘点（tiny-worker 实码）

| 调用点 | 锁内/外 | 频率 |
|---|---|---|
| `session_messages()`（history.rs：前端拉历史/搜索/导出的统一入口） | **sessions 锁内** `to_vec` | 每次前端读历史/搜索一遍全库 |
| rebuild 族（切模型/分叉/截断→`old.messages().to_vec()`） | 锁外（agent 已 take） | 每次动作一次 |
| 每次 run 起点 `pre_msgs` 归档对账快照 | 锁外（worker 线程） | 每 run 一次 |
| `finish_turn` 的 `sidebar_snippet(agent.messages())` | sessions 锁内（借用读，无拷贝） | 每 run 一次 |

### 7.2 `messages().to_vec()` 实测（release，Windows/18GB，热机，20 次取均值）

| 会话形态 | 消息数 | 体量 | to_vec 均值 | 借用读基线 |
|---|---|---|---|---|
| 小（纯文本 1KB） | 200 | 200KB | **123 µs** | 1ns |
| 中（纯文本 1KB） | 500 | 500KB | 329 µs | 1ns |
| 大（纯文本 1KB） | 1 000 | 1MB | 806 µs | 1ns |
| 大（纯文本 4KB） | 1 000 | 4MB | 1.88 ms | 1ns |
| 任务型（4KB 文本 + 1/3×8KB 工具结果） | 1 000 | 5.3MB | 2.6 ms | 1ns |
| 超大（同上） | 2 000 | 10.7MB | 5.4 ms | 1ns |

吞吐 ~1.5–2 GB/s（分配 + memcpy 主导，线性于字节量；消息条数的每条分配开销可忽略）。

### 7.3 结论（供 design 取舍）

- 现实会话（≤1 000 条、≤5MB）单次锁内拷贝 **≤3ms**：今日可容忍，不是火烧眉毛的债；10MB 级会话在多窗口并发拉历史/搜索时锁持有 5ms+ 开始成为争用点。
- 借用读恒为 **1ns 级**——方案 (a)（`read_messages(|&[Message]) -> T` 作用域回调）即可吃掉全部拷贝成本，且不动消息所有权结构（方案 (b)/(c) 的 Arc 化/游标语义复杂度对当前收益面偏高）。宿主侧最大消费点 `session_messages()` 天然是「回调内取所需」形态。
- 建议 design 以 (a) 为主轴；(b)/(c) 仅在出现「回调内无法表达」的消费方时再议。
