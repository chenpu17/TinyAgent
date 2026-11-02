# 宿主集成需求单答复（TinyAgent → TinyWorker，2026-11-02 批次）

| 项 | 值 |
|---|---|
| 状态 | v1.0（逐项评审结论 + 落地状态，已全部通过本仓门禁） |
| 日期 | 2026-11-03 |
| 答复方 | TinyAgent 核心侧（crates/core） |
| 答复对象 | `docs/host_integration_requirements.md`（需求单 v1.0，2026-11-02 批次） |
| 内核基线 | `0d17f94..189c5a9`（11 个提交，[Unreleased] 全部记账；推送与版本发布按用户节奏） |
| 文档性质 | 逐 FR 评审结论、已落地语义与验收对照、破坏性清单、宿主侧接入清单 |

---

## 1. 总体结论

- **接受并已落地 5 项**：FR-1（P0）、FR-3、FR-4、FR-6，以及**缩减范围后的 FR-2**（仅内联 `<think>` 剥离；`reasoning_content` 部分打回，见 §2.2）。
- **接受问题、方案另议 1 项**：FR-5（见 §2.5，需要贵侧回传热路径基准数据作为 design 输入）。
- **§2（已满足表）、§4（实验项 X-1/X-2/X-3 口径）、§5（否决记录）、§6（通用约束）全部认可，无修订意见**——这份需求单对现状的核对（除 FR-2 一处外）与流程约束都是准确的。
- 每项落地均按 §6.6 过了**独立审查轮次**（两个独立审查 agent 在隔离 worktree 分别审查 FR-1 与四个小项，全部动态验证；发现并修复 3 条 P1，见 §4）。

## 2. 逐 FR 答复

### 2.1 FR-1 `on_turn_stopping`（P0）— 接受，已落地

需求单全部验收通过，另有四个**定版语义**需要贵侧实现时知晓：

1. **事件只发非默认决策**（对「每次仲裁无论结果发一条事件」的通道读法）：Rust trait 无法区分「未覆盖的默认 Stop」与「显式 Stop」，且验收①要求默认路径零漂移——故 `TurnArbitration` 事件仅在 `Continue` 被兑现或被降级时发出；显式 `Stop` 与无钩子 run 逐字节一致（事件流/历史/stop_reason 均不变）。trait 文档已如实写明，建议需求单后续修订时对齐。
2. **确定性守卫在决策点预检**：续跑帽（6，`TurnStopInfo::continuations_cap`）耗尽、或 `turns` 已到 `max_turns`（下一轮根本无法启动）时，`Continue` 当场降级——`stop_reason = "arbitration_capped"`（新核心标签，**exit 0**，标签在 result.json 供审计；result.json 的 `success` 键保持严格 `end_turn` 口径——同一 run exit 0 而 `success:false`，宿主汇总台账以标签为准）、`continued: false`、note 不入队不入史、只保留在事件里。独立审查实测证实了必须预检：无预检时该场景会被轮首护栏翻成 `max_turns_reached`（exit 1），把无钩子时的成功收尾翻转成失败。
3. **竞态类守卫永远赢**：cancel / `budget_exceeded` / timeout 只在轮首检查，`continued: true` 表示「决策被接受，下一轮尝试启动」而非保证发出请求；已入队的 note 在这些出口走 R1 的 deferred 冲账（晚到、不丢）。
4. **不仲裁的面**：子 agent run（`task` 子代理，`top_level=false`）从不仲裁；`max_tokens`（含 salvage）/`stuck_loop`/预算/轮帽/Err 全是强制终止；零块收尾轮（内容过滤吞掉回复）也不仲裁——没有可判的完成声称。

验收对照：①零漂移（全套既有测试不动，census 钉子表 16→17）；②note 经监督者旁注管线入尾用户消息、模型当轮可见、`NoteInjected{deferred:false}` 回执照发；③`arbitration_capped` 可辨且 exit 0；④续跑窗口内 cancel 照常生效（`stop_reason="cancelled"`）；⑤仲裁在当轮消息面定稿后、note 在下轮轮首先于 preflight 压缩入史（字节必进 token 预检）。NDJSON：`{"type":"turn_arbitration","turn","continued","note"}`；TUI 渲染决策行；`--resume` 重建不带钩子（§6.4 既有告诫）。

**宿主侧注意**：Python/Node 绑定的 Hooks 适配器暂不转发第四方法（贵侧走 path 依赖是 Rust 面，不受影响；绑定转发列为后续项）。

### 2.2 FR-2 推理通道（P1）— **部分打回**，缩减版已落地

- **打回 `reasoning_content` 部分，前提已过时**：openai 兼容层早已把 `delta.reasoning_content`（DeepSeek-R1/GLM/o 系直连）与 `delta.reasoning`（OpenRouter 归一化字段，含空值过滤）解析为 `ThinkingDelta` 事件——「不入历史、不入正文」语义、NDJSON `thinking` 事件类型、绑定透传全部现成，且有钉子测试。贵侧需求单所列痛点（「要么静默丢弃要么混入正文」）在该形态下不成立。**请先核对贵侧 path 依赖指向的 core 版本**：若七轮审计跑在旧 checkout 上，更新 path 依赖即可消解这部分痛点。验收①（reasoning_content 形态分离）请直接对照现有实现与测试确认。
- **接受内联 `<think>` 剥离，已落地**：vLLM 系不开 reasoning-parser 时正文里的 `<think>…</think>` 由流式状态机（`ThinkStripper`）跨任意 chunk 边界切出，thinking 映射到既有 `ThinkingDelta`；正文保证无标签残留（完整开标签后流断按 thinking 冲——fail-closed；未成形的部分标签按正文冲；关标签无开标签是普通正文）。开关 `Config::strip_think_tags` 默认开；关闭即逐字节透传（与此前行为一致）。验收②③④通过（②既有语义；③同流混跑已钉测试；④透传零改动）。验收①中的 `<think>` 形态由新测试覆盖。
- 「推理不影响历史与计费口径」的既有决策维持并已在文档写明（两通道都永不入历史/正文）。

### 2.3 FR-3 网关修复导出/预置（P2）— 接受，已落地

`GatewayFixes`（三布尔位，serde，`deny_unknown_fields`——预置位打错字当场报错而非静默失效）+ `Provider::gateway_fixes()`（带默认实现，非学习型 provider 零改动）+ `Config.gateway_fixes_preset`（可选）+ `RunOutcome.gateway_fixes`（学到的修复非空才出现在 result.json，sparse 约定保既有产出字节不变）。验收①（预置后首个请求即带修复、mock 仅收 1 请求）②（导出/预置往返等价，OR 幂等）③（未预置路径不变）全过。

**一处口径修正**：导出是**含预置位的进程内累计**（同一 Agent 多次 run 导出同一值），不是需求单措辞的「本次学到的修复集」单 run 增量——往返等价与预置用途不受影响（反而更稳），需要逐 run 增量的宿主自行 diff 相邻 outcome。建议需求单措辞跟进。

### 2.4 FR-4 有效配置指纹（P1）— 接受，已落地

`Config::fingerprint()`：对 clamp/overlay 合并后的**生效**配置，按**固定字段序**规范化 JSON → SHA-256 截 64 bit（16 hex）。进哈希：内核版本 + 全部 harness 形状字段（provider/base_url/model/max_tokens/系统提示词/工具白名单[排序]/limits 全家/MCP 拓扑/推理档位/缺位注记/code 沙箱/`gateway_fixes_preset`/`strip_think_tags`）。排除：`task`/`workdir`（运行输入）。凭据纪律（约束⑤）：只进键名不进值——`api_key_env` 本是变量名；MCP headers/env 仅排序键名；mcp url 与 base_url 都过 userinfo 脱敏。语义有序列表（`mcp_servers`/`code.llm.models`/`tool_absent_notes`）按声明序进哈希——顺序本身是拓扑语义，序变即指纹变，属定版行为。验收①②③④各有钉子测试。出账面：`RunOutcome.config_fingerprint`（result.json 恒在键）+ `Agent::config_fingerprint()`。算法/截断长度/字段清单定版即兼容面，变更走 CHANGELOG——**在 [Unreleased] 窗口内指纹值无兼容承诺，定版于首个发布版**。

**时点语义（外审核定）**：指纹是 **Agent 构造时的快照**——常驻 Agent 的热更新换挡（`set_model` / `set_reasoning_effort` / `set_max_tokens` / `set_max_context_tokens` / `set_code_config` / `set_tool_absent_notes` / `add_mcp_server` / `remove_mcp_server`）改变实际生效形状，但**不移动**已出账的指纹。换挡后要做「模型 + harness」台账对比的宿主，请重建 Agent（或每 run 一建）取新指纹；CLI/批处理每 run 一建，天然无此歧义。

### 2.5 FR-5 消息免深拷贝（P1）— 接受问题，方案另议（本批不落地）

锁内 `to_vec()` 全量深拷贝是真痛点，接受立项。但三个候选的取舍需要 design 数据支撑：

- 我们倾向 **(a) 作用域借用回调**（侵入最小、零新所有权结构）；
- **(c) 增量游标风险最高**：消息面会被 `normalize_adjacent`、剪枝、压缩折叠持续改写，「自上次游标以来」的有效性语义稍有漏洞，宿主就会拿到撕裂视图——除非贵方能接受一个明确的「游标在折叠/改写后强制失效」契约；
- (b) Arc 写时复制会扩散到消息所有权结构的每一处，收益/风险比待测。

**请贵侧回传**：manager 热路径的现状基准（`to_vec()` 在锁内的实测耗时占比、大会话尺寸分布），作为 design 输入与验收基线（「锁内不再全量拷贝」必须可测）。落地窗口与 §6.3（同批落地或先合后吃）一致。

### 2.6 §4 实验项 — 同意原口径

X-1「无证据不动」、X-3「不为没有消费方的 seam 扩面」照单。X-2 补充：宿主**今天即可**用现有面验证——`before_tool` 对 `task` 工具 `Deny`（ctx 可辨识），零内核改动。

## 3. 破坏性清单（0.x minor 即破坏窗口，全部记 CHANGELOG [Unreleased]）

1. `CompressOutcome` **失去 `Copy`**（FR-6 携带 `folded_ids` 所致）——按值拷贝的消费方改 `.clone()`。
2. `RunOutcome` 新增 `config_fingerprint` / `gateway_fixes` 字段——字面构造的消费方需补字段；读取方按键消费不受影响（`gateway_fixes` 无修复时缺席）。
3. `AgentEvent` 新增 `TurnArbitration` 变体（enum 本身 `#[non_exhaustive]`，serde 透传的消费方零改动；**穷尽解构的 Rust 消费方需补臂**）+ NDJSON 新事件类型 `turn_arbitration`。
4. `stop_reason` 新核心标签 `arbitration_capped`（CLI exit 0；批处理 success 口径仍只认 `end_turn`——batch 无钩子面暂不可达，分歧已注释留白）。
5. `sha2` 由 code-feature 门控转必选依赖（默认树 +1 个既有 crate）。
6. `Config` 新增两个可选字段：`gateway_fixes_preset` / `strip_think_tags`（serde default，旧配置文件解析不变）。
7. `AgentEvent::Compressed` 变体新增 `folded_ids` 字段（FR-6 携带）——Rust 侧对该变体做穷尽解构（不带 `..`）的消费方需补字段；serde/NDJSON 面为 additive 键，schema_version 契约不变。

## 4. 审查与验证记录

- 双独立审查（隔离 worktree，动态复现）：FR-1 一轮（APPROVE + 1×P1 已修）、FR-2/3/4/6 一轮（REQUEST-CHANGES，2×P1 + 6×P2 全部处置）。P1 中最实质的两条：①轮预算边界翻转（见 §2.1 第 2 点，worktree 探针实锤后修复）；②result.json 写键面与文档声明不符（`write_result` 手工键清单漏键，FR-3/4 对 CLI 宿主的闭环断路——已补键并加落键测试）。
- **推送前终审**（本批完成后的第二批双独立审核 + e2e，全部动态验证）：两位审核员分片深审（FR-1 仲裁+宿主面 / FR-2/3/4/6+全文档对账）均 **APPROVE、无 P1**；计 2×P2 + 8×P3，全部处置——指纹定版「构造时快照」语义（§2.4）、zh README 补齐 EN 全部新增段落、`Compressed` 变体字段补破坏性记账（§3 第 7 条）、`reply_text` 文档例与零块路径对齐、config 参考补两键、exit 表与 batch 表注记，并收编 8 根回归钉子（指纹字段完备性扫描 / build 布线 / 压缩时序两条 / TurnStopInfo 语义 / panic 契约 / 零块不仲裁 / NDJSON folded_ids）。e2e（真实二进制 + 真实 HTTP/SSE 栈）27/27：think 剥离开/关、预置修复首请求生效、指纹稳定/区分、result.json 两键、无钩子零漂移、压缩 folded_ids + 折叠会话 resume。
- 门禁（分支顶点）：`check.sh all` 14/14（fmt/clippy×4/测试×4/build:code/e2e/文档门禁×3）、bench 组 2/2、bindings 组 clippy×4；core 616（mock）/ 800（code,mock）、CLI 110 测试全绿。每项 FR 均按 §6.6 过独立审核轮次。

## 5. 宿主侧接入清单（建议顺序）

1. path 依赖对齐 `189c5a9`（或其后版本）；先核对 FR-2 打回部分的现状前提（§2.2）。
2. RSI 观察者实现 `on_turn_stopping`：读 `TurnStopInfo { reply_text, turn, continuations_used, continuations_cap }` 做节奏控制；错误路径返回 `Stop` 不 panic（§6.4）。
3. 运行台账接 `result.json.config_fingerprint`（恒在键）钉「模型 + harness 配对」。
4. 模型档案接 `gateway_fixes` 往返（result.json sparse 键 → `Config.gateway_fixes_preset`）。
5. FR-5 基准数据回传（§2.5），启动 design 立项。
