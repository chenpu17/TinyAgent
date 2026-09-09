# run_code 微调支持特性 技术实现文档

| 项 | 值 |
|---|---|
| 状态 | v0.3（M1–M5 已落码；FD-3 经 e2e 修订） |
| 版本 | v0.3 |
| 日期 | 2026-09-09 |
| 关联文档 | `docs/finetune_support_requirements.md` v0.2（需求规格，FR/NFR/A 编号在本文引用）；`docs/run_code_design.md`（既有设计，D 编号在本文引用）、`docs/run_code_requirements.md` |
| 代码基线 | master @ 3c170b2（其后行号会漂移，以代码为准） |
| 决策编号 | 本文 FD-n 独立编号；引用 run_code_design 的决策写 D-n |
| 修订记录 | v0.1：首版。含一条设计阶段对需求文档的事实修正（FD-9：FR-2 检出表第三行的 `__tools_list`/`__budget` 是**可工作的内省拦截**而非 D11 排除集，检出集合收窄为 `{run_code, task, todo}`）——需求方数据侧以本文附录 A 文案为准；v0.2：M4 落码时补 A-3 溢出行的文案形状（附录 A-3 末尾，`等 N 处` 在逐处段落形态下的对应物）；v0.3：e2e 首跑抓出 FD-3 判定缺陷——「initial 为空」在 headless/batch 主路径永假（任务消息总在 initial 里，eager 永不注入），判定改为内容判定（历史已含 read_skill("run-code") 披露即跳过）+ 前置边界合并（对尾 user(tool_result) 与任务 user 按 append_user_turn 合并） |

---

## 1. 总体架构

两个特性共享一个配置载体（`CodeConfig` 加两个字段），其余完全独立：

```
FR-1 skill 注入策略（装配期一次性消费，快照语义）
  Config.code.skill_injection: off | on_demand(默认) | eager
    └─ assemble（src/lib.rs:287-293 既有内置注入点，改三分支）
       ├─ on_demand → 现状逐字节不动（insert_builtin，read_skill 按需）
       ├─ off       → 跳过 insert_builtin；index 无磁盘同名覆盖时给
       │              ReadSkill 塞 tombstone → 明确"已被配置关闭"错误
       └─ eager     → insert_builtin 照做（level-1 索引仍在）+ 额外把
                      skill 正文合成 tool_use/tool_result N:N 配对，
                      经 build_agent_with* 前置进 initial 历史
                      （仅新会话；id 走 assign_missing_ids 既有 ingest 点）
    └─ 审计：result.json "code" 对象 + NDJSON 起始行（仅 code.enabled 时出现）

FR-2 guest 执行前静态预检（每次调用经 snapshot() 读锁，热更即生效）
  Config.code.precheck: bool（默认 false）
    └─ RunCodeTool::run_with_emit（src/tools/run_code.rs:2017）
       MAX_CODE_BYTES 检查之后、eval() 之前：
         if snap.precheck { if let Some(msg) = precheck(code) {
             return Ok(ToolOutput::error(msg));   // 不实例化 Store、零副作用
         }}
    └─ precheck(code: &str) -> Option<String>：纯函数、单遍 lexer、
       拒答文案 = 附录 A（即规格）
```

两条刻意的不对称（都文档化、都对齐既有先例）：

- `skill_injection` 是**装配期决策**：`set_code_config` 运行期整包替换**不**重新注入/撤回
  skill（对齐「宿主热更新面不传播给已构造快照」先例）；`precheck` 是**调用期决策**，
  热更即生效（与 fuel/mem_mb 等限制字段同路径）。
- FR-1 的 eager 注入走 `build_agent_with*` 装配路径；直连 `Agent::try_new` 的调用方
  （含 `task` 子 agent）不获得注入——与「子 agent 不继承 skill」的既有边界同构，
  不是缺口。

## 2. 关键决策记录

| # | 决策 | 理由 | 否决的备选 |
|---|---|---|---|
| FD-1 | `skill_injection` / `precheck` 住 `CodeConfig` | 配置单一事实源；绑定层经 `set_code_config(dict)` 的 serde 缺省自然到达（NFR-5 零新 API）；batch 收紧语义复用 `PartialCodeConfig`/`merge_partial` 穷尽解构先例（新臂无策略即编译错误——反化石纪律自动覆盖） | 独立 skills 配置节（需求方只有 run-code 一个内置场景，需求文档范围外条款一致）；全局开关（多一个事实源） |
| FD-2 | `SkillInjection` 派生 `Ord`，声明序 = `Off < OnDemand < Eager`（按披露量）；`merge_partial` 用 `Clamp::Tighten`（min）；`precheck: bool` 同用 Tighten（false<true，片段只能关不能开） | 「不可信片段不得扩大披露/不得开启新行为」翻译成 min 语义零新机制；枚举 `Default = OnDemand` 满足 Tighten 的 `Default` bound（其值仅 TightenOrInit 使用） | 手写臂级 if 链（fossil 四连的前车之鉴，Clamp 存在就是为了消灭它） |
| FD-3 | eager 注入 = 合成 `tool_use`/`tool_result` N:N 配对，**当 initial 尚无该披露**（v0.3 内容判定：历史中不存在 `read_skill("run-code")` 的 tool_use——空 initial（绑定/TUI）、任务消息 initial（headless/batch 新会话）都注入；eager 会话 resume 的合成对或真实调用已在历史则跳过）时由 `build_agent_with`/`build_agent_with_provider` 前置进 initial，**边界按 append_user_turn 合并**（对尾 user(tool_result) 与首条任务 user 合成一条——相邻 user Anthropic 400，合并后形状与绑定/TUI 的 run() 路径一致）；id 分配走 `try_new` 里 `assign_missing_ids`（agent.rs:553）既有 ingest 点，不新设分配点 | Q-1 定案的实现化：与真实 read_skill 路径逐字节一致（见 FD-4）；`--resume` 会话历史非空 → 跳过（会话自身历史是权威——它创建时若是 eager 则配对已在其中，重复注入反而污染）；空 initial = 新会话是唯一无歧义判定 | user 消息内嵌（撞「Anthropic 不允许相邻 user」，须走 append_user_turn 合并，形态反而分叉）；`Agent::new` 内部感知 skill（装配层职责；try_new 直连路径本就不带 skill 机制，隐式注入反而制造两条行为分叉的构造路径） |
| FD-4 | eager 注入的正文与 `read_skill` **共用同一段取正文代码**：把 ReadSkill 对「effective entry 的 SKILL.md 正文 + `truncate(cap)`」抽成自由函数 `skills::serve_skill_body(entry, max_output_chars)`，ReadSkill 的 embedded/磁盘两臂与 assemble 的 eager 注入都调它 | 「逐字节一致」（A-1）用**构造**保证而非约定保证：同一份文本、同一个截断帽、同一条路径。磁盘同名覆盖时 eager 注入的自然是磁盘版正文（与 read_skill 在那会话里会返回的一致）；磁盘读取失败 → eprintln 告警 + 跳过注入（对齐 malformed skill「降级不致命」先例） | assemble 里复制一份截断逻辑（必然漂移）；注入 `RUN_CODE_SKILL_MD` 常量原文（绕过磁盘覆盖语义） |
| FD-5 | off 档的 read_skill 明确错误用 **tombstone**：`ReadSkill` 增字段 `disabled: Option<String>`；assemble 在 off 档且 index 无磁盘同名覆盖时塞 `Some("run-code")`；命中时返回附录 A-0 文案。index 里有磁盘覆盖则照常服务、不塞 tombstone | A-1 要的是「明确未启用错误」而非通用 unknown skill——通用错误的 available 列表随环境漂移，评测轨迹里无法区分「写错名」与「被关闭」；而磁盘 run-code 是用户显式资产，FR-1 范围条款明确不动磁盘 skill 机制 | 依赖既有 unknown skill 错误（语义不含「未启用」，且 available 列表内容随环境变）；off 时连磁盘覆盖一并屏蔽（违反 FR-1 范围条款） |
| FD-6 | 审计锚点：① result.json 增 `"code"` 对象 `{"enabled":true,"skill_injection":"…","precheck":…}`，**仅 `code.enabled` 时出现**（默认配置 result.json 逐字节不动——NFR-1；additive key 先例见 attribution 字段注释 harness.rs:430）；载体 = `ResultMeta` 加字段、`from_agent` 读 Agent 新增只读 getter（避免 17 个 `run_with_retries` 测试调用点改签名）；batch 复用同一 `write_result` 自动覆盖。② NDJSON `--json` 模式在首个 turn 前打一行 `{"type":"code",…}`（同条件才打；harness 层 print，不动 AgentEvent 枚举）；非 json headless 打一行 stderr | 需求 v0.2 修订一的双锚点落地到真实存在的两个产物上（TinyAgent 没有「会话 manifest」）；getter 路径保持 harness 签名稳定 | 新 AgentEvent 变体（装配期事实不是运行期事件，且枚举加变体牵动全部消费端 match）；总是打印（默认配置漂移，破 NFR-1） |
| FD-7 | 运行期语义不对称文档化：`skill_injection` 装配期消费（热更不重注）、`precheck` 调用期经 `snapshot()` 生效 | 与既有先例逐条对齐：热更新不传播快照（CLAUDE.md 既有口径）；资源限制字段每次调用读锁（run_code.rs:1612） | eager 热更补注（会话中途改写历史，破坏「会话历史权威」且与压缩交互复杂）；precheck 也做成装配期（失去运行期调参能力，与 v0.4 调参 API 初衷相抵） |
| FD-8 | `precheck` 默认 **false**，显式配置才生效 | NFR-1 原文「两特性均须显式配置才生效」；且默认开会直接漂掉既有测试——run_code.rs:2130 的 `Date.now()`/`Math.random()` 运行时抛桩用例会改成预检拒答，A-6 零漂移当场破 | 默认开启（效率特性无安全语义，但零漂移纪律不让步；需求方评测本来就要显式配置 harness） |
| FD-9 | **检出集合修正（对需求文档 FR-2 表第三行的事实修正）**：保留字面量检出 = `{run_code, task, todo}`，**排除 `__tools_list`/`__budget`** | 后两者是**可工作的只读内省拦截**（run_code.rs:49-57：TOOLS_LIST_NAME/BUDGET_NAME；prelude 的 `tools.budget()` 本体就是 `tools.call("__budget", {})`，run_code.rs:1388）——guest 直写 `tools.call("__tools_list")` 运行时**成功**。按需求表拒答它 = 文案撒谎（「不可达」）+ 误杀合法代码（破 A-5）。D11 排除集从来只有 run_code/task/todo 三者 | 照需求表实现（事实错误进训练数据，文案即规格的承诺反而放大了错误） |
| FD-10 | lexer = 单遍 token 扫描：跳过字符串/模板串/行注释/块注释；**关键词**（`async`/`await`/`import`）与**成员表达式**（`Date.now`/`Math.random`）直接报；**可遮蔽全局名**（`Promise`/`setTimeout`/`fetch`）仅当「作为被调用/构造的标识符出现，且全文无 `function/const/let/var/class` 同名声明」时报出；`tools.call("…")` 仅认首参为字符串字面量且在保留集内 | v0.2 修订三「真 lexer 非 substring」的具体化；声明豁免与 v0.2 修订二（helper 误名降级）同源——静态语境下「已声明的名字是模型有意自定义」，不该拦（guest 自定义 `function fetch(){}` 是合法代码）；成员表达式不豁免（`var Date=…` 是病态代码，且绕过方向是安全的：漏检仅退回运行时） | substring 匹配（`var s="no import here"` 误伤，A-5 破）；全标识符无豁免（自定义 fetch 误伤）；AST parser（需求范围外条款，依赖与误报成本不匹配） |
| FD-11 | 拒答文案统一前缀 `run_code precheck: `；一次拒答报告**全部命中**（按类固定序：环境语法 → 确定性 → 保留工具；类内按文档序列出，每类封顶 20 处、溢出「等 N 处」） | 前缀让轨迹收割方能机械区分静态拒答与运行时错误（需求方数据管线直接受益，也是「恢复轨迹更短更确定」诉求的一部分）；一次报全 = 模型一轮修完，恢复轨迹最短 | 报首个命中（逐类逐个修 = 多轮往返，与需求 §2.2「更短更确定」相悖）；无前缀（静态/运行时错误在轨迹里不可分） |
| FD-12 | 拒绝语义照抄 hooks Deny 口径：`ToolOutput::error`（is_error）→ 落 `Slot::Ran(error)`、进 records 与事件流、不算执行、不消耗预算计数；预检在 eval 之前返回 → 不实例化 Store、零副作用、内部调用数零消耗（没有任何调用发生） | Q-2 定案的既有先例复用，零新语义面；「不计内部调用数」由「根本未执行」构造性成立 | 新增拒绝通道/事件变体（无消费者）；拒绝时静默跳过 records（审计缺口，hooks Deny 都不这么干） |

## 3. FR-1 实现设计

### 3.1 配置面（src/config.rs）

```rust
/// skill 披露策略（finetune_support_design FD-2）：序关系即披露量，
/// off < on_demand < eager，batch 片段 min 收紧（Clamp::Tighten）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillInjection { Off, OnDemand, Eager }

impl Default for SkillInjection {
    fn default() -> Self { SkillInjection::OnDemand }   // serde 缺省 = 现状
}
```

- `CodeConfig` 增 `pub skill_injection: SkillInjection`（`Default` impl 同步）与
  `pub precheck: bool`（默认 false，FD-8）。两处都是纯数据——feature-off 构建照常
  解析（CodeConfig 既有定位：plain data，config.rs:276-279）。
- `PartialCodeConfig` 增两个同名 `Option` 字段；`merge_partial` 穷尽解构增两臂：

```rust
self.skill_injection = Clamp::Tighten.merge(self.skill_injection, skill_injection);
self.precheck = Clamp::Tighten.merge(self.precheck, precheck);
```

- `Config::apply` 对 `code.enabled` 的拒绝（config.rs:577-584）不动；新字段走
  merge，无新的拒绝项。

### 3.2 assemble 三分支（src/lib.rs:287-293 既有注入点改造）

```rust
#[cfg(feature = "code")]
let mut eager_body: Option<String> = None;
#[cfg(feature = "code")]
if cfg.code.enabled {
    match cfg.code.skill_injection {
        SkillInjection::OnDemand => { /* 现状：insert_builtin */ }
        SkillInjection::Eager => {
            // insert_builtin 照做（level-1 索引仍在），然后取 effective entry
            // 的正文（磁盘覆盖时即磁盘版——FD-4 共用 serve_skill_body）
        }
        SkillInjection::Off => {
            // 跳过 insert_builtin；index.entry_of("run-code").is_none()（无磁盘
            // 覆盖）时给 ReadSkill 塞 tombstone（FD-5）
        }
    }
}
```

`assemble` 返回值加第 5 项 `eager: Option<Vec<Message>>`（feature-off 恒 `None`，
签名不分叉——CodeInstall 同款先例）。两个 `build_agent_with*` 调用点：

```rust
let (tool_list, ctx, cfg, code, eager) = assemble(...)?;
let initial = prepend_eager_pair(eager, initial);
// 内容判定（v0.3）：历史已含 read_skill("run-code") 披露 → 原样；
// 否则前置合成对，首条任务 user 合并进对尾的 user(tool_result)
```

### 3.3 eager 合成配对的具体形状

与真实 read_skill 路径（agent.rs:1412-1417 assistant 持久化 + :1712-1726
tool_result 消息）逐字节同构：

```rust
// assistant 消息（id 由 ingest 点补，FD-3）
Message::assistant(vec![Block::ToolUse {
    id: "eager-run-code".into(),               // 合成 id；真实路径的 id 是
    name: "read_skill".into(),                 // provider 随机 mint，逐字节一致
    input: json!({"skill": "run-code"}),       // 不覆盖也不含 id（FD-3 注释口径）
}])
// user 消息（tool_result）
Message::user(vec![Block::ToolResult {
    tool_use_id: "eager-run-code".into(),
    content: body,                             // serve_skill_body(entry, cap)
    image: None,
    is_error: false,
}])
```

- 合成 id 字面量 `eager-run-code` 合法（Anthropic id 字符集内）且固定——评测
  轨迹里可机械识别，这是特性不是瑕疵。
- `truncate` 的帽用装配期 `cfg.limits.max_output_chars`（与 ReadSkill 运行时读的
  `ctx.max_output_chars` 同源同值——ctx 的该字段即由它填充，lib.rs:334）。
- 配对是可压缩普通历史：压缩的 `find_safe_cut` 对 tool_use/tool_result 配对有既
  有保护，无新增接入点。
- 注入不进 schema、不进系统提示（level-1 索引行照旧在系统提示——eager 不是
  off，模型仍知 skill 存在，只是正文已在历史里）。

### 3.4 off 档 read_skill 行为

`ReadSkill { index, disabled: Option<String> }`（`ListSkill` 不动——它只列文件，
embedded 无附属文件本就报错）。`run()` 在 `entry_of` 查找**之前**先查 tombstone，
命中返回附录 A-0 文案。`disabled = None`（默认与 on_demand/eager 档）时行为逐字
节不动。

### 3.5 审计锚点

- `src/harness.rs`：`ResultMeta` 增 `code: Option<CodeFacts>`（`CodeFacts {
  skill_injection: &'static str, precheck: bool }`）；`from_agent` 经 Agent 新只读
  getter（`code` feature + enabled 时 `Some`，否则 `None`）填充。`write_result`
  两个分支（outcome Some/None）都在 attribution 字段区加 `"code"` key（preserve_order，
  插在 `provider` 之后）。
- NDJSON：`harness::run` 在重试循环之前、首个事件之前打
  `{"type":"code","enabled":true,"skill_injection":"eager","precheck":false}`；
  非 json 模式打一行 stderr `[code] skill_injection=eager precheck=false`。
  两者都仅 `code.enabled` 时输出。
- TUI 不打（装配事实不进对话视口；result.json 无 TUI 对应物，符合既有分工）。

### 3.6 绑定层

`set_code_config(dict)`（py）/ camelCase 对应（node）反序列化 `CodeConfig`，
serde 缺省让两新字段自然到达，**绑定零新 API**（NFR-5）。两侧 binding 测试的
build-自适应探测无需扩展；回归各跑一遍（带与不带 `--features code` 两侧）。

## 4. FR-2 实现设计

### 4.1 插入点与拒绝语义

`run_with_emit`（run_code.rs:2017）现有早返序列：缺 `code` 字段 → `MAX_CODE_BYTES` →
`eval`。预检插在最后一步之前：

```rust
if self.snapshot().precheck {
    if let Some(msg) = precheck(code) {
        return Ok(ToolOutput::error(msg));
    }
}
```

- `precheck(code: &str) -> Option<String>` 是同文件内的纯函数（单测直调，不进
  热路径以外任何位置——NFR-4）。
- 拒绝后的一切由既有路径承担：`ToolOutput::error` → agent loop 落
  `Slot::Ran(error)`、进 `ToolCallRecord`、发 `ToolResult` 事件（FD-12）。
- 排序证明零插桩：用一个 registry 必坏（白名单含未知工具名）的实例断言——
  预检命中时输出是附录 A 文案而非 registry 构建错误，即证明拒答先于 eval
  （A-3 的「wasm 引擎零调用」由「先于 eval 返回」构造性成立，外加 scratch
  workdir 无文件副作用断言）。

### 4.2 lexer 规则（FD-10 的规格化）

单遍扫描，产出 token 流（标识符/关键词、字符串、标点、模板串），同时维护
`declared: HashSet<String>`（`function X`/`const X`/`let X`/`var X`/`class X` 声明的
名字，含函数表达式右侧——声明即豁免，不求精确作用域）。跳过规则：

| 构造 | 处理 |
|---|---|
| `'…'` / `"…"` 字符串（含转义） | 跳过内容 |
| `` `…` `` 模板串（含 `${…}` 嵌套——嵌套深度计数，嵌套内的代码照常扫描） | 字面部分跳过 |
| `//…` / `/*…*/` 注释 | 跳过 |

检出规则（全部命中收集，FD-11）：

| 类 | 规则 | 豁免 |
|---|---|---|
| 环境语法 | token ∈ {`async`, `await`, `import`}（关键词）；或标识符 ∈ {`Promise`, `setTimeout`, `fetch`} 且作为调用/构造目标出现（`X(`、`new X`、`X.`） | 关键词无豁免；三个全局名若 ∈ `declared` 则豁免 |
| 确定性 | 成员表达式 `Date.now` / `Math.random`（标识符-点-标识符三连） | 无（遮蔽 `Date`/`Math` 是病态代码，漏检方向安全） |
| 保留工具 | `tools.call` 后接 `(` + 字符串字面量 ∈ {`run_code`, `task`, `todo`}（FD-9 修正后的集合） | 非字面量首参（变量拼接）不检出——漏检退回运行时 A-9，方向安全 |

- 同一 token 多处命中：每处都列（行号不同），类内封顶 20 处，溢出「等 N 处」。
- 行号 = token 起始行（1-based）。模板串 `${}` 嵌套内的命中按其实际行号报。
- 复杂度 O(n) 单遍；10KB 代码远低于 1ms（NFR-4 宽裕成立）。输入已被
  `MAX_CODE_BYTES` 封顶，扫描成本有硬上界。

### 4.3 诚实声明（写入 README 与本节）

预检是**效率/体验特性，非安全机制**：词法扫描可被混淆绕过
（`globalThis["Date"]["now"]()`），沙箱既有安全层（能力沙箱、确定性桩 throw、
拦截层）不依赖预检；预检漏检的代价仅是退回现状（运行时失败），不少任何安全
保证。A-4 用测试把两层分工钉死：混淆写法预检放行、确定性桩运行时抛错。

## 5. FR-3 声明（后置，不阻塞本期）

两处运行时文案增强（A-9 unknown tool 的封套建议 + ReferenceError 封套的封闭清单
建议），独立小 PR。本设计不含其实现；唯一的前置约定：实现时封套结构（D12 双封
套、`kind` 词汇表）不变，仅 `error` 文案增强，且增强后的文案照 FD-11 同款先例
入附录即规格。

## 6. 代码接入点清单

| 文件 | 改动 |
|---|---|
| `src/config.rs` | `SkillInjection` 枚举（FD-2）；`CodeConfig` + `Default` + `PartialCodeConfig` + `merge_partial` 穷尽解构两臂 |
| `src/lib.rs` | `assemble` 三分支（§3.2）；返回值加 `eager: Option<Vec<Message>>`；两个 `build_agent_with*` 前置逻辑（FD-3） |
| `src/skills.rs` | `serve_skill_body(entry, cap)` 自由函数抽取（FD-4） |
| `src/tools/skill.rs` | `ReadSkill.disabled: Option<String>` + tombstone 分支（FD-5）；构造点同步（assemble 1 处 + 测试） |
| `src/agent.rs` | code 配置只读 getter（ResultMeta 用）；**initial ingest 零改动**（assign_missing_ids 既有点即 id 分配） |
| `src/tools/run_code.rs` | `precheck` 纯函数 + `run_with_emit` 门（§4.1）；6 处测试字面量 `CodeConfig{…}` 补两字段 |
| `src/harness.rs` | `ResultMeta`/`write_result`/`from_agent` 的 `"code"` key；NDJSON/stderr 起始行（§3.5） |
| `bindings/` | **零新 API**；两侧测试回归（带/不带 code feature 各一遍） |
| `README.md` | 配置文档增两字段；FR-2 诚实声明段（§4.3） |
| `CLAUDE.md` + `docs/claude/` | 索引行 + 详录条目（装配期/调用期不对称 FD-7；`__tools_list`/`__budget` 是可用内省拦截 FD-9——易踩坑级） |
| `docs/finetune_support_requirements.md` | FD-9 修正回执需求方（其 FR-2 表第三行随之修订，数据侧以本文附录 A 为准） |

**不改的**：`agent.rs` loop 路径、压缩、hooks、MCP、`ToolOutput`/`ToolCallRecord`/
`ToolCtx` 结构（无新字段 → 无字面量构造点同步面）；`run_code` 沙箱执行语义全部
不动（需求 §1.2）。

## 7. 测试计划（映射需求 A-1..A-7 + NFR）

| 验收 | 测试 |
|---|---|
| A-1 三态 | on_demand：既有 skill 测试全绿即零漂移证据。eager：装配后 `messages()` 头两条 = 合成配对，body 与 `ReadSkill` 直出逐字节相等（FD-4 共用函数的等价断言）；压缩后对 pair 无残留要求（可被回收）。off：`read_skill("run-code")` 返回附录 A-0 文案；tool schema/描述逐字节不变 |
| A-2 batch 收紧 | `merge_partial` 单测：片段 eager + base off → off；片段 off + base eager → off；片段 eager + base on_demand → on_demand（不得放大）。precheck 同构：片段 true + base false → false |
| A-3 检出 | 三类各一用例（`async`/字符串内 import 不中的对照一并给）：is_error、文案与附录 A 逐字一致、registry 必坏实例下仍得预检文案（排序证明）、scratch 目录零文件 |
| A-4 诚实边界 | `globalThis["Date"]["now"]()`：precheck on 也放行，运行时确定性桩 throw——两层分工钉死 |
| A-5 不误伤 | run_code_skill.md 全部示例 + 既有 run_code 测试语料逐个过 `precheck()` 断言 `None`；补 `function fetch(){…}` 自定义豁免用例、`tools.call("__budget")` 不检出的 FD-9 用例 |
| A-6 零漂移 | 默认配置下既有全量测试逐字节不动（含 :2130 运行时桩用例——precheck 默认 false 使其路径不变）；三 clippy + fmt 全绿 |
| A-7 | FR-3 后置，本期不实现 |
| NFR-3 | result.json/NDJSON 锚点断言（code.enabled 时有 key、禁用时无 key）；eager 会话 `--resume` 不重注 |
| NFR-5 | 双绑定 `set_code_config` 带两新字段 round-trip |

测试纪律照项目惯例：`cargo test --features code,mock`（run_code 套件）+
`cargo test --features tui,mock`（lib_api 回归）+ 三个 clippy 配置 +
`cargo fmt --all`，每个里程碑 commit 前全跑。

## 8. 实现里程碑（评审通过后逐 commit）

| # | 内容 | 验收 |
|---|---|---|
| M1 | 配置面：枚举 + 两字段 + merge 两臂 + 单测（A-2） | config 单测绿；三 clippy + fmt |
| M2 | `serve_skill_body` 抽取 + assemble 三分支 + ReadSkill tombstone + eager 注入（A-1） | skill/agent 测试绿 |
| M3 | 审计锚点：result.json + NDJSON/stderr（NFR-3） | harness 测试绿 |
| M4 | precheck lexer + 附录 A 文案接入（A-3/A-4/A-5） | run_code 套件绿 |
| M5 | README 诚实声明 + CLAUDE.md/详录索引 + 绑定回归（NFR-2/NFR-5） | 绑定两侧测试绿 |

FR-2 文案在 M4 落码即冻结——需求方以本 commit 的附录 A 为数据放量基线。

## 附录 A：模型可见文案（即规格，实现照抄；变更走本文修订记录）

> 体例对齐 run_code_design.md 附录 A「LLM 可见契约即规格」。以下字符串字面量
> 是规格本身；`{…}` 为模板位（取值规则见各条）。**微调数据对这些文案有依赖，
> 任何变更必须先在本文走修订记录、再改码。**

### A-0 off 档 read_skill 拒答（FD-5；ReadSkill tombstone 命中时返回全文）

```
read_skill: skill "run-code" 已被配置关闭（code.skill_injection = "off"）。run_code 的环境事实见其工具描述；完整用法文档在此配置下不可用。
```

### A-1 环境语法类（§4.2 规则表第一类）

每命中一处列出 `"token"（第 N 行）`，类内多处按文档序、逗号分隔（封顶 20 处，
溢出写 `等 N 处`）：

```
run_code precheck: 检出环境不支持的语法 {hits}——本沙箱是同步环境：没有 async/await/Promise，没有 setTimeout 等 timer，没有 import/fetch。请改同步写法：顺序执行；批量数据在一次执行内用循环 + tools.call 逐次处理。代码未执行，无副作用；修正后重发完整代码。
```

`{hits}` 示例（单处）：`"async"（第 3 行）`；（多处）：`"async"（第 3 行）、"await"（第 5 行）`。

### A-2 确定性类（§4.2 规则表第二类）

```
run_code precheck: 检出确定性违规 {hits}——确定性沙箱不提供 Date.now / Math.random（调用即在运行时抛错，不是返回固定值的桩）。需要时间或随机值时，改用数据记录自带的字段或代码内常量。代码未执行，无副作用；修正后重发完整代码。
```

`{hits}` 示例：`"Date.now"（第 7 行）`。

### A-3 保留工具类（§4.2 规则表第三类，FD-9 修正后集合）

逐处：

```
run_code precheck: 检出 tools.call("{name}")（第 {line} 行）——"{name}" 在沙箱内不可达（设计 D11：防自递归与嵌套成本）。沙箱内只能调用 tools.list() 列出的工具；"{name}" 的调用请移出沙箱、由主循环直接发起。代码未执行，无副作用；修正后重发完整代码。
```

保留工具类命中超过 20 处时（FD-11 封顶）：逐处段落只列前 20 段，末尾追加一行
（A-1/A-2 的 `等 N 处` 在逐处段落形态下的对应物；N = 该类总命中数）：

```
run_code precheck: 保留工具命中共 {n} 处，以上仅列前 20 处。代码未执行，无副作用；修正后重发完整代码。
```

多类同时命中：按 环境语法 → 确定性 → 保留工具 的固定序各产一段，段间换行连接；
一次拒答报告全部命中（FD-11）。
