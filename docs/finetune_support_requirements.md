# run_code 微调支持特性 需求规格说明书（外部需求提交）

| 项 | 值 |
|---|---|
| 状态 | v0.2（**评审通过：FR-1/FR-2 接受、FR-3 同意后置；三条评审修订已并入**——见修订记录；实现设计待另立技术实现文档） |
| 版本 | v0.2 |
| 日期 | 2026-09-09 |
| 修订记录 | v0.2：**并入评审意见（2026-09-09）**——① NFR-3「会话 manifest」改为 result.json + NDJSON 事件流锚点（评审修订一：与既有审计口径对齐）；② FR-2 helper 误名检出**移除静态方案**（误报面：guest 自定义 `readline` 与 `readLines` 编辑距离恰为 2，违反 A-5 零误报），降级并入 FR-3 运行时文案增强（QuickJS ReferenceError 封套给建议）；③ FR-2 实现约束钉死「真 lexer（跳过字符串/注释），非 substring」（否则 `var s="no import here"` 误伤）；④ Q-1 定案：eager 注入 = 合成 tool_use/tool_result N:N 配对，与真实 read_skill 路径逐字节一致（评审补充硬理由：user 内嵌撞「Anthropic 不允许相邻 user」、须走 append_user_turn 合并反而形态分叉；实现纪律：新消息入历史必须补 id 分配点）；⑤ Q-2 定案：预检拒答照抄 hooks Deny 口径——落 Slot::Ran(error)、进 records、不算执行、不消耗预算；⑥ Q-3 定案：FR-3 后置独立小 PR |
| 文档性质 | 需求规格（SRS 为主，不含实现设计——实现设计待评审通过后按本项目惯例另立技术实现文档） |
| 关联文档 | `docs/run_code_requirements.md`、`docs/run_code_design.md`（run_code 既有规格，本文 FR 编号独立、引用其 D 编号决策） |
| 需求来源 | 外部项目「MiniCPM5-2B run_code 契约内化微调」（见 §2 背景） |

---

## 1. 引言

### 1.1 目的

为 run_code 工具新增两个相互独立的小特性：

1. **skill 注入策略开关**（FR-1）：控制内置 skill `run-code` 的披露时机——
   在「按需读取」（现状）之外，支持「会话开始自动注入」与「关闭」；
2. **guest 执行前静态预检**（FR-2）：对模型提交的 JS 代码在执行前做毫秒级
   确定性检查，对已知必败模式直接拒答并给出精准错误文案，不执行、零副作用。

### 1.2 范围

两个特性的需求、验收、范围外事项。**不覆盖**：实现设计（评审通过后另立）；
不改动 run_code 沙箱执行语义（拦截层、限流、审计、封套 ABI 全部不动）。

### 1.3 术语

| 术语 | 定义 |
|---|---|
| 契约内化微调 | 外部微调项目：通过 SFT/RL 训练，把 run-code skill 中的**行为模式**写入模型权重，使 tuned 模型不经 read_skill 即产出合规代码 |
| 预检 | FR-2 的静态检查：执行前对 guest 代码文本的确定性扫描，**非安全机制**（见 FR-2 诚实声明） |
| 三级渐进披露 | run_code 的契约披露结构（D18）：瘦描述常驻 schema → 内置 skill 按需加载 → 资源文件 |

## 2. 背景与问题陈述（需求方原始口径）

### 2.1 背景

需求方正在对 MiniCPM5-2B（2.5B 端侧模型）做 run_code 场景的契约内化微调：
以 TinyAgent harness 为轨迹收割与判卷环境、glm-5.3-flash 为教师，产出 SFT/RFT/RL
三阶段训练。训练目标：**tuned 模型在关闭 skill 披露时的 run_code 任务成功率 ≥
开启 skill 的 base 模型**，同时失败重试次数与 token 消耗显著下降。

### 2.2 由此产生的两个宿主侧需求

**需求一（FR-1 的来源）**：微调效果的核心验收协议是「双臂评测」——同一评测集在
skill 注入开/关两种 harness 配置下对比。当前 skill `run-code` 的披露是单一形态
（code.enabled 时注入索引、模型经 read_skill 按需读取），**没有关闭或改变披露时机
的开关**，双臂协议与 tuned 模型的最终部署形态（不读 skill）都无法表达。同时，
「会话开始自动注入」档位服务另一条路径：base 模型免一次 read_skill 往返
（注入以普通消息形态进上下文，仍可被压缩回收，不构成常驻 schema 税）。

**需求二（FR-2 的来源）**：训练数据包含「犯错 → 看到错误消息 → 修正」的恢复轨迹，
**错误消息文案会进入训练数据**。当前一类高成本失败是「代码跑了一半、产生部分
副作用、然后撞墙」（用了 `async`/`Date.now`、调用保留工具名）——一轮 LLM +
一次沙箱执行 + 一轮重试。执行前毫秒级拒答 + 精准文案既直接降低使用成本，也让
训练数据中的恢复模式更短更确定。**顺序约束（需求方强调）**：本特性必须先于
微调项目的数据放量，否则训练数据学到的是旧文案的恢复模式，文案后改 = 数据作废。

### 2.3 目标场景（用户故事）

- 作为**微调项目的评测者**，我能用配置开关关闭 skill 披露，从而测量 tuned 模型
  「不读 skill」的真实能力（双臂对比）；
- 作为**微调项目的数据工程师**，模型犯错时收到的错误消息是稳定、精准的版本，
  我收割的恢复轨迹不会因宿主文案变更而作废；
- 作为**普通使用者**（base 模型），我可以选择会话开始即注入 skill，省掉一次
  read_skill 往返，且注入内容仍是可压缩消息、不增加每轮常驻成本；
- 作为**维护方**，这两个特性不改变任何既有默认行为，未配置时与现状逐字节一致。

## 3. 功能需求（FR）

### FR-1 skill 注入策略开关（P0）

`CodeConfig` 新增字段 `skill_injection`（三态枚举，serde 缺省 = 现状行为）：

| 取值 | 语义 | 服务对象 |
|---|---|---|
| `on_demand`（默认） | 现状：skill 索引进系统提示，模型经 read_skill 按需加载（D18 不变） | 未配置的全部既有用户 |
| `eager` | 会话装配期把 skill 全文以**合成 tool_use/tool_result N:N 配对**注入历史
  （等价于模型第一轮就完成 read_skill 的真实返回——与该路径**逐字节一致**，
  tuned/base 上下文形态无差别）；可压缩、可回收，不进 schema、不进系统提示 | base 模型省往返 |
| `off` | skill 索引不注入、read_skill 对 run-code 返回明确未启用错误；瘦描述不变（事实常量仍由描述承载） | tuned 模型部署与双臂评测 |

- **范围**：仅作用于内置 skill `run-code` 的自动注入路径，不影响磁盘 skill 机制、
  不影响 read_skill/list_skill 对其他 skill 的行为；
- **batch 语义**：照 PartialCodeConfig 先例，`skill_injection` 子字段片段策略 =
  片段可收紧不可放松（`eager`→`off` 合法，反向非法；`on_demand` 为中间档——
  序关系 off < on_demand < eager 按披露量定义），不可信片段不得扩大披露；
- **运行期**：经 `set_code_config` 整包替换自然覆盖，无需新 API；
- **审计（v0.2 评审修订一）**：注入/关闭决策锚点 = **result.json + NDJSON 事件流**
  （对齐既有审计口径，取消 v0.1 的"会话 manifest"提法——评测复现读这两处即可）；
- **实现纪律（v0.2 评审 Q-1 补充）**：合成消息入历史必须走**既有 id 分配点**；
  eager 档位选择合成配对而非 user 内嵌的硬理由——user 内嵌撞「Anthropic 不允许
  相邻 user」、须走 append_user_turn 合并，形态反而与真实 read_skill 路径分叉。

### FR-2 guest 执行前静态预检（P0）

`RunCodeTool::run_with_emit` 在实例化 wasm 之前，对 `input.code` 做纯函数式
确定性扫描（输入 code 字符串 + 已知词汇表，输出 `Option<拒答文案>`）：

| 检出项 | 拒答文案要点 |
|---|---|
| `async` / `await` / `Promise` / `setTimeout` / `fetch` / `import`（词法级） | 「环境无 async/Promise/timer/import——改同步写法」 |
| `Date.now` / `Math.random` | 「确定性沙箱，两者不可用（非桩 0）」 |
| 对 `run_code` / `task` / `todo` 字面量的 tools.call 调用 | 「该工具在沙箱内不可达（D11）」 |

（设计期事实修正 FD-9：v0.1/v0.2 本表含 `__tools_list`，有误——它与 `__budget`
是**可工作的内省拦截**（prelude 的 `tools.budget()` 本体即 `tools.call("__budget")`），
不在检出集合内。检出集合定稿为 `{run_code, task, todo}`，见实现文档修订记录 v0.1。）

（v0.2 评审修订二：helper 误名静态检出**移除**——编辑距离方案存在误报面：
guest 自定义 `function readline(p)` 是合法代码，与 `readLines` 距离恰为 2，
静态检出违反 A-5 零误报。该能力降级并入 FR-3 运行时文案增强：QuickJS
ReferenceError 的封套文案中给封闭清单建议。）

- **词法实现约束（v0.2 评审修订三）**：「词法级」钉死为**真 lexer**——扫描须跳过
  字符串字面量与注释（`var s = "no import here"` 不得误伤），**禁止 substring
  匹配**；仍不进 AST/parser（范围外条款不变）；
- **拒绝语义（v0.2 评审 Q-2 定案）**：命中即返回 is_error 的 ToolOutput，
  **不实例化 Store、零副作用、不计内部调用数**；外层口径**照抄 hooks Deny 既有
  先例**——落 Slot::Ran(error)、进 records、不算执行、**不消耗预算计数**；
  照常进外层 ToolCallRecord 与事件流；
- **文案稳定承诺（本需求的核心）**：拒答文案作为**规格**写入技术实现文档附录
  （对齐 run_code_design.md 附录 A 的「LLM 可见契约即规格」先例），变更走修订记录——
  微调数据对其有依赖；
- **诚实声明（写入 NFR 与 README）**：预检是**效率/体验特性，非安全机制**——
  词法扫描可被混淆绕过（`globalThis["Date"]["now"]()`），沙箱既有安全层
  （能力沙箱、确定性桩 throw、拦截层）不依赖预检；预检漏检的代价仅是退回现状
  （运行时失败），不少任何安全保证。

### FR-3 运行时文案增强（P1，后置——v0.2 评审同意后置，独立小 PR 不阻塞本期）

两处错误文案增强，均不改变封套结构、仅文案：

1. guest 内 `tools.call` 未知名（既有 A-9 错误）：封套文案追加编辑距离 ≤2 的
   真实工具名建议（MCP 长名、`_`/`__` 混淆的高频坑，skill 已点名）；
2. **（v0.2 自 FR-2 降级并入）helper 误名**：QuickJS ReferenceError 的封套文案中，
   当未定义名与封闭清单（readAll/readLines/writeVerified）编辑距离 ≤2 时，附
   清单原文与「此外没有别的内置助手」——运行时语境下该名确未被定义，无误报面。

**独立验收**，不阻塞 FR-1/FR-2。

## 4. 非功能需求（NFR）

| # | 项 | 要求 |
|---|---|---|
| NFR-1 | 零漂移 | 默认配置下全部既有测试逐字节不动；两特性均须显式配置才生效 |
| NFR-2 | 诚实边界 | FR-2 的非安全定位写入 README 与需求验收；FR-1 `off` 档不声称「模型不需要契约」——事实常量仍在瘦描述 |
| NFR-3 | 评测可复现 | FR-1 注入/关闭决策锚定 result.json + NDJSON 事件流（v0.2 评审修订一）；FR-2 文案入规格附录，变更走修订记录，版本可追溯 |
| NFR-4 | 性能 | 预检为纯函数，单次 <1ms（10KB 代码量级）；不进热路径以外的任何位置 |
| NFR-5 | 绑定 | 两特性经 CodeConfig 自然到达绑定层 `set_code_config(dict)`，绑定零新 API |

## 5. 验收标准（A）

| # | 验收 | 判据 |
|---|---|---|
| A-1 | FR-1 三态语义 | `on_demand` 现状逐字节一致；`eager` 注入为合成 tool_use/tool_result 配对、与真实 read_skill 路径逐字节一致、压缩可回收（v0.2 Q-1 定案）；`off` 时 read_skill("run-code") 返回明确错误、描述/schema 不变 |
| A-2 | FR-1 batch 收紧方向 | 片段 `eager` + base `off` → `off`；反向合并被拒 |
| A-3 | FR-2 检出 | 每类检出项一个用例：返回 is_error、文案与规格附录一致、**wasm 引擎零调用**（可插桩断言）、无文件副作用 |
| A-4 | FR-2 诚实边界 | 混淆绕过用例（`globalThis["Date"]["now"]()`）不被预检拦截、由确定性桩在运行时 throw——两层的分工有测试钉住 |
| A-5 | FR-2 不误伤 | 合法代码回归集（run_code_skill.md 全部示例 + 既有测试场景）零误报 |
| A-6 | 零漂移 | feature on/off 下既有全量测试不变（对齐既有测试 15） |
| A-7 | FR-3 建议质量 | `mcp_sql_qurey` → 建议含 `mcp_sql_query`；`readline`（未定义）的 ReferenceError 封套 → 建议含 `readLines`；封套结构不变（v0.2 扩入第二条） |

## 6. 范围外（Non-goals）

- ❌ 完整的 JS AST 解析（词法扫描足够覆盖已知必败模式；引 parser 的依赖与
  误报成本不匹配本特性的效率定位）
- ❌ 把预检升级为安全机制（与 §FR-2 诚实声明互斥）
- ❌ 对其他 skill 的注入策略泛化（需求方只有 run-code 一个内置 skill 场景；
  机制留扩展位，本期不实现）
- ❌ script 输出通道（模型以非 function tool 形式吐脚本）——需求方已明确否决该路线
- ❌ 微调项目自身的任何内容（数据管线/训练/评测，均在需求方 repo）

## 7. 开放问题（v0.2：全部定案）

| # | 问题 | 结论（v0.2 评审） |
|---|---|---|
| Q-1 | `eager` 档注入形态 | ✅ **合成 tool_use/tool_result N:N 配对**，与真实 read_skill 路径逐字节一致；实现纪律：新消息入历史必须补 id 分配点；否决 user 内嵌（撞「Anthropic 不允许相邻 user」、append_user_turn 合并反而形态分叉） |
| Q-2 | 预检命中是否消耗预算计数 | ✅ **不消耗**——照抄 hooks Deny 口径：落 Slot::Ran(error)、进 records、不算执行 |
| Q-3 | FR-3 是否随本期 | ✅ **后置**，独立小 PR；helper 误名建议并入 FR-3（运行时 ReferenceError 文案增强） |
