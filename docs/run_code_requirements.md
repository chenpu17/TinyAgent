# run_code 需求规格说明书（PRD + SRS 合一）

| 项 | 值 |
|---|---|
| 状态 | v0.5（四路复审已并入；H1 契约错误已修，M0 出口判据可达） |
| 版本 | v0.5 |
| 日期 | 2026-09-02 |
| 修订记录 | v0.2：FR-3 可调集口径收敛；FR-7 封套化与 subagent 标记；FR-8 deny_paths 诚实化；FR-9 墙钟三执行点；FR-11 PartialCodeConfig；A-8/A-9 验收。v0.3：运行期开关默认关闭 + 三层开启 API（FR-13）；A-10。v0.4：**资源限制全面第三方可配（产品方确认）**——CodeConfig 暴露 `fuel`；运行期调参 API `set_code_config`（FR-11/FR-13 修订，FR-11 升 P0）；绑定层可调参。v0.5：**并入四路复审（架构 / 一致性 / 红队 / 代码事实核对）**——FR-1 白名单语义改定（default_tools 不含 run_code，开关 = code.enabled，显式点名判定零歧义）；FR-2 确定性锚点；FR-4 墙钟先于保留名截获；FR-7 标记改 `__code__`；FR-8 拦截层次序重排 + 写路径拒悬空 symlink + restrict_to_workdir=false 语义明文 + glob 语义钉死 + 默认清单补 `**/.aws/**`；FR-9 最坏墙钟数值公式；FR-10 双哈希 + sha256 截 128bit；FR-11 fs_write_dirs 空=全可写恒定 + 两种"整体替换"对照；FR-12 验收编号 A-11；A-10 拆 a/b/c；NFR-4 绑定 set_code_config 回填 |
| 文档性质 | PRD 部分：§1.5–§1.6、§10（定位/用户/竞品/指标）；SRS 部分：§2–§8 |
| 关联文档 | `docs/run_code_design.md`（技术实现文档） |
| 需求来源 | 产品方口头需求（2026-09 会话）+ 业界方案调研 |

---

## 1. 引言

### 1.1 目的

定义 TinyAgent 新内置工具 `run_code` 的需求：让 LLM 生成的程序逻辑在 agent
**进程内**受控执行，程序可回调 agent 已有的全部工具（内置 + MCP），能力面收敛到
"agent 工具授权 + 工作目录沙盒"，不引入新的本地资源访问通道。

### 1.2 范围

覆盖功能需求、非功能需求、验收标准、范围外事项。不覆盖实现细节（见技术实现文档）。

### 1.3 术语

| 术语 | 定义 |
|---|---|
| guest | 沙箱内执行的代码（LLM 生成的 JS） |
| 沙箱 | wasm 运行时（wasmtime）承载的、无 WASI 的 QuickJS 实例 |
| 内部调用 | guest 通过 `tools.call` 发起的工具调用（区别于 loop 的顶层调用） |
| dispatch | 工具分发路径：查找工具 → hooks 闸门 → 执行 → 事件/审计 |
| 资产 | 预编译的 `quickjs.wasm` 二进制，构建期嵌入单二进制 |
| 出域通道 | 数据离开本机的途径（tool_result 回上下文 / shell 网络 / MCP） |

### 1.4 参考与业界先例

- CodeAct 论文（ICML 2024）：可执行代码统一动作空间，实测成功率高 ~20%、步数少 ~30%。
  <https://arxiv.org/abs/2402.01030>
- Cloudflare Code Mode（2025-10）：模型写代码编排工具调用，实测 token 简单任务 -32%、复杂任务 -81%。
  <https://blog.cloudflare.com/code-mode/>
- Anthropic 工程博客《Code execution with MCP》：代码执行 + MCP 工具交互的效率收益。
  <https://www.anthropic.com/engineering/code-execution-with-mcp>
- OpenAI Code Interpreter / Anthropic code execution tool：云端沙箱形态（对比项，见 §3.3）。

### 1.5 产品定位与目标用户（PRD）

**一句话定位**：TinyAgent 的进程内代码执行工具——模型写代码编排既有工具
（内置 + MCP），执行落在本地 wasm 沙箱，授权与审计和普通工具调用完全同流。

**目标用户与各自收益**：

| 用户 | 场景 | 收益 |
|---|---|---|
| 终端用户（TUI / headless） | 数据导出、批量文件处理、循环编排 | 轮次与 token 收敛；大数据不进上下文；结果落文件 |
| 评测 / 批量任务 | 行为与成本锁定的自动化 | 复杂编排压缩为单次工具调用；限流参数可收紧 |
| 绑定宿主（GUI / Python / Node） | 治理型嵌入 | hooks 对代码内每次调用可拒/可改/可审计，无新增治理面 |

### 1.6 竞品对比（PRD）

| 维度 | OpenAI Code Interpreter | Anthropic code execution | Cloudflare Code Mode | **本方案** |
|---|---|---|---|---|
| 执行位置 | OpenAI 云端容器 | Anthropic 云端沙箱 | Workers 运行时 | **本机进程内 wasm** |
| 文件系统 | 沙箱私有 FS（数据上传） | 沙箱私有 FS | 无本地 FS | **直接操作本机 workdir** |
| 能否回调本 agent 工具/MCP | ✗ | ✗ | ✓ | ✓（内置 + MCP 全量，同一注册表） |
| 授权模型 | 平台黑盒 | 平台黑盒 | Workers 权限 | **每次内部调用过宿主 hooks** |
| 隔离技术 | 容器/microVM | 服务端沙箱 | V8 isolate | wasm 能力沙箱（零 ambient authority） |
| 数据主权 | 代码+数据出域 | 代码+数据出域 | 出域到 Workers | **全程本机** |

定位差异归纳：云厂商做的是"模型的分析沙箱"（不接工具、数据上云）；Code Mode 是
最近亲（同属代码编排形态）但跑在他们的 runtime；本方案的差异化 = Code Mode 的形态
+ 本地执行 + 授权同流 + 单二进制交付。不做的档位：重型不可信 Python（云容器主场）。

---

## 2. 背景与问题陈述

### 2.1 现状问题

TinyAgent 的多步数据处理任务（典型：经 DB MCP 分页读表 → 聚合 → 写 CSV）目前只能由
模型逐轮 tool call 驱动：每一轮都要"模型重读上下文 → 输出一次调用 → 等结果"。代价：

1. **轮次与 token 成本线性叠加**——循环、过滤、拼装这类确定性逻辑本不需要模型参与每一步；
2. **数据处理能力受限**——模型在上下文里"心算"数据变换，大结果集既装不进上下文也不可靠；
3. **无法自我设限**——分页拉取直到取完的循环语义，靠模型逐轮判断，容易提前停或失控。

### 2.2 需求来源（原始口径，逐条追溯）

产品方原始需求五条 + 两轮澄清：

| 编号 | 原始口径 | 落地解释 |
|---|---|---|
| R-1 | agent 框架内集成新工具运行程序逻辑，**不是** agent 外部写程序来运行 | `run_code` 为进程内普通工具，无子进程 |
| R-2 | 支持 wasm 方式让 LLM 返回编程文本并执行 | **wasm 是沙箱承载层，不是 LLM 输出格式**：LLM 写 JS，宿主是编译成 wasm 的 QuickJS（详见设计文档决策 D1） |
| R-3 | 编程可调用 agent 工具，含内置与 MCP 导入 | 经统一工具注册表寻址（`find_tool`），与 loop 同路径 |
| R-4 | 尽量收敛能力面：只用 agent 工具，不提供直接访问本地资源的函数 | 沙箱无 WASI/网络/env；文件访问以工具形态提供（见 R-6） |
| R-5 | 编程调用工具的授权延续 agent 调用工具的授权 | 内部调用走同一 dispatch：hooks 闸门、白名单、超时、预算 |
| R-6（澄清） | 文件访问授权 ≡ agent 文件访问授权 | 复用 `read_file`/`write_file`/`edit_file`/`list_dir` 原工具，不新建第二套 fs API |
| R-7（澄清） | 工具命名 `run_code`；编排逻辑全部在工具内部自治，loop 零感知 | loop 视角是一次普通叶子工具调用 |

### 2.3 目标场景（用户故事）

- **US-1 数据导出**：经 DB MCP 分页读订单表 → 按客户聚合 → 写 `out/top_customers.csv`
  → 返回路径与统计摘要。数据本体不进模型上下文。
- **US-2 批量文件处理**：`list_dir` 枚举 → 循环 `read_file` 解析 → 汇总写报告文件。
- **US-3 循环编排**：分页拉取直到取完、带重试的批量调用、条件分支密集的流程。
- **US-4 宿主治理**：宿主（GUI/绑定用户）经 hooks 对 guest 的每次内部调用可拒/可改/可审计，
  与模型直调工具的治理完全一致。

---

## 3. 功能需求（FR）

优先级：P0 = 本期必须；P1 = 本期应有；P2 = 后续版本。

### FR-1 工具注册与调用形态（P0）

`run_code` 注册为普通内置工具：`ToolConcurrency::Sequential`；不进子 agent
（`available_to_subagent() = false`）；对 agent loop 完全透明——一次 tool_use 配
一个 tool_result，内部调用不产生任何消息块。

**开关不经 `config.tools` 白名单（v0.5 改定，评审 H4）**：与 `task`/`todo`
不同，`default_tools()` **不含** run_code——开关是 `code.enabled`（两层，均默认
关）。由此 serde 判定零歧义：`tools` 字段"没写"与"写了不含 run_code"都解析为
不含，**表内出现的 `run_code` 必然是显式点名**（无需区分字段缺省与显式默认值）。
编译期 `code` feature（默认不含）+ 运行期 `CodeConfig.enabled`（默认
**false**）。未开启时 run_code 不进工具表与 tool_defs——模型不可见、零 schema
成本（而非"可见但调用报错"）。开启面见 FR-13。
验收：默认状态下模型不可见不可调；开启后注入可调用（无需 `config.tools` 点名；
要禁用就设 `code.enabled=false`——评测锁行为同此，比从 tools 摘更直白）；显式
点名 `run_code` 而未开启 → 清晰报错（带开启方法提示；feature off 时为
actionable 未知名报错，提及 code feature）；子 agent 工具表无它。

### FR-13 开关与开启 API（P0，v0.3 新增）

开启 run_code 是**显式信任决策**（默认暴露面归零——被注入模型默认不获得
"循环 + 代码执行"能力）。三层开启面，供第三方按集成方式选用：

| 层 | API | 适用 |
|---|---|---|
| 配置文件 | `"code": {"enabled": true, …}`（含 PartialConfig 片段） | 二进制用户（headless/TUI）、batch/评测 |
| Rust 库 | `cfg.code = CodeConfig{…}`（装配期，全部字段）+ `Agent::set_code_config(&mut self, cfg: CodeConfig)`（**热更新面**：开关与全部限流/路径旋钮**整包原子替换**，轮次间生效、进行中的执行沿用旧参数；`set_code_enabled(bool)` 为其便捷衍生） | 嵌入 TinyAgent 的第三方 agent/宿主 |
| 语言绑定 | `agent.enable_code()` / `agent.disable_code()`（快捷）+ `agent.set_code_config({...})`（整包 dict，与 Rust 同语义；run 期间返回 busy；绑定 crate 以 `code` 转发 feature 编入，默认关、未编入时方法返回明确错误） | Python/Node 宿主 |

开关态不进 session 文件（`--resume` 后按 config 重新决定，与 hooks"层活在实例上"
同口径）。
验收（A-10a/b/c，v0.5 按里程碑拆分——a=开关 M2 / b=调参生效 M3 / c=绑定
busy M4）：三层开启路径各自生效；运行期开关往返后 tool_defs 与模型可见性一致
（A-10a）；`set_code_config` 运行期调参（如收紧 max_tool_calls）后下一次执行按
新参数生效（A-10b）；绑定 run 期间调用返回 busy（A-10c）。

### FR-2 沙箱执行环境（P0）

guest 语言为 JavaScript（QuickJS 方言，ES2023 级），纯同步（无 Promise/async/await/
setTimeout/import/require/fetch）；承载于进程内 wasm 运行时；**无 WASI、无网络、
无环境变量、无系统调用**——除宿主注入的 API 外无任何能力。代码作为函数体执行，
`return` 的值经 JSON 序列化作为工具结果。**确定性执行**：`Date.now` /
`Math.random` 访问即 throw（非桩 0）——同代码同结果，评测可复现、失败可重放
（宿主时间注入 `host_now` 属扩展位，本期不做）。
验收：guest 内直接发起任何 I/O 均不存在该 API；异步语法在文档中声明不可用并在
描述中告知模型；`Date.now()`/`Math.random()` 调用 throw 且文案说明不可用原因。

### FR-3 内部工具调用 API（P0）

`tools.call(name, args)` → 调用 **guest 可调集**内的工具，参数对象与普通工具调用
一致。可调集 = `cfg.tools` 白名单内的叶子工具（shell/read_file/write_file/
edit_file/list_dir…）+ 全部 MCP 工具；**不含 task / todo / run_code**（构造性
排除：内部注册表按 `available_to_subagent()` 过滤，防自递归 DoS 与嵌套 agent
成本炸弹）**不含 extra / skill 工具**（与 `task` 子 agent 同一边界；extra 支持列
为 v2 备选，见开放问题 5）。语义：

- 返回值：工具输出可解析为 JSON 则为对象/数组，否则为字符串（FR-3.1）；
- 工具执行失败（is_error / 工具 Err / hooks Deny / deny_paths 拦截）→
  **throw `Error`**，message 含工具名与原因；guest 可 try/catch 后继续（FR-3.2）；
- 返回图像类二进制结果（`load_image` 等）的工具不可调——显式报错而非静默丢弃；
- 每次调用实时产生 `ToolCall`/`ToolResult` 事件（与 loop 调用同形状，前端零适配）。

验收：mock 工具经 `tools.call` 调用成功/失败两路径行为符合上述语义；调
run_code/task/todo 收 unknown tool（A-9）。

### FR-4 工具自省 API（P1）

`tools.list()` → `[{name, description}]`，**仅列 guest 可调集**（与 FR-3 一致，
同步隐藏 task/todo/run_code；description 截断封顶防撑爆 guest 内存）。经保留名走
宿主侧截获（截获前仍有墙钟入口检查——忙循环刷 list 不计调用数但受 timeout
封顶），**不进 dispatch、不占 hooks、不计入调用次数**。保留名 `__tools_list`
在 feature 开启时被工具名校验保留（自定义工具不得撞名）。用途：模型对带消毒前缀
的 MCP 工具名不确定时自省。

### FR-5 日志 API（P0）

`console.log(...)`：实时转发为事件（前端可见执行进度）+ 收集进工具结果随返回。
**执行期有界**：host 侧日志缓冲 ≤ 2×返回上限，超限停收集并记 `logs_dropped`
计数、同时停发事件（防 NDJSON/TUI 事件洪水）；结果超尺寸时**截日志、保返回值**。
验收：长脚本执行期间 TUI 状态行 / `--json` 流可见日志；超限时返回值完整、日志带
截断标记与 dropped 计数。

### FR-6 返回值约束（P0）

模型可见的工具结果 = guest 返回值（JSON）+ 日志，总尺寸有硬上限（配置项）。约定
"返回引用不返回数据"：大数据写文件后返回路径 + 统计。验收：超限返回值被截断且带标记。

### FR-7 授权延续（P0）

内部调用与 loop 顶层调用共享同一套授权面：hooks `before_tool`/`after_tool` 对每次
内部调用照常触发（ctx 以 `subagent=Some("__code__")` 克隆传入——`__` 前缀保留
字面量，task 的 name 参数禁用 `__` 前缀以消解撞名；宿主策略可区分
"代码发起的调用"并单独收紧）；`Deny` / `Modify` 语义与顶层一致（Modify 换执行
输入、审计留原始输入）。guest 与顶层调同一 `cfg.tools` 白名单构建的注册表——
顶层配不到的工具 guest 也调不到。结果通道为双封套
（`{"__ok":true,…}`/`{"__ok":false,"error":…}`），消灭与第三方工具输出内容的
键名碰撞。
验收：`CaptureHooks`（现有测试设施）记录到内部调用的精确序列（subagent 标记为
"__code__"）；Deny 路径 guest 收到 throw；工具输出恰含 `__error`/`__ok` 键时成功
不被误判。

### FR-8 文件访问（P0）

guest 经 `tools.call("read_file"|"write_file"|"edit_file"|"list_dir", …)` 访问文件，
授权与模型直调**完全一致**：workdir 沙盒（`resolve`/`confined_to`）、
`restrict_to_workdir`、hooks、输出截断全部复用，**不新建任何文件 API**。

可选收紧（执行点 = guest 调用拦截层、仅上述 4 个文件工具；位于 hooks 闸门
**之后**、判定对象是 hooks Modify 后的实际输入——路径重定向不可绕过）：

- `deny_paths`：凭据形态黑名单，匹配拦截层自行规范化（以 workdir 为基准
  canonicalize + 相对化，与工具沙盒解耦）的 workdir 相对路径。glob 语义钉死：
  `*` 不跨 `/`、`**` 跨、**`**/` 匹配零或多段**（`**/.env*` 也命中根）、**不含
  `/` 的模式只匹配根第一段**；平台大小写归一。默认清单：
  `.env*`、`**/.env*`、`**/id_rsa*`、`**/id_ed25519*`、`**/id_ecdsa*`、`**/*.pem`、
  `**/.git/**`、`**/.git`、`**/.git-credentials`、`**/.netrc`、`.aws/**`、
  `**/.aws/**`；
- **写路径强化**：write_file/edit_file 的目标路径 workdir 下**任一组件**是
  symlink（中间段与最终段一视同仁，含悬空 symlink）→ 拒绝（防 `out -> /tmp`
  中间段逃逸与 `ln -s .env bak` 诱导写入穿透——两者都击穿"防误读"定位本身；
  整链而非仅最终组件，v0.5 评审升级）；
- **restrict_to_workdir=false 时**：guest 可用绝对路径访问 workdir 外文件，该
  路径无法相对化 → deny_paths 不适用——**读放行、写恒拒**（guest 写入恒以
  workdir 为界：fs_write_dirs 的权限宇宙以 workdir 封顶；读的放行是文档化
  边界，含 workdir 内 symlink 别名等价变体。敌对场景正解 = 开
  restrict_to_workdir 或 hooks Deny）；
- `fs_write_dirs`：写目录白名单（仅 write_file/edit_file 受限），空 vec **恒**
  等于"全 workdir 可写"（最松值，语义不随来源翻转；batch 合并见 FR-11）。

**定位声明**：deny_paths 防**模型误读**、不防**恶意 guest**——shell 在白名单时
（默认如此）`cat .env` 全量绕过；敌对场景正解是 hooks 对 shell Deny 或从 `tools`
去除 shell（A-8 钉住此边界）。
验收：guest 内 `../` 逃逸被拒；deny_paths 命中被拒（含 `./.env`、`a/../.env`
规范化绕写、中间段 symlink 逃逸与悬空 symlink 写穿透用例）；
restrict_to_workdir=false 时 workdir 外路径读放行、写恒拒；glob 语义向量（`**/` 零段命中根、无 `/` 模式仅根第一段、嵌套 `.aws`
命中）；shell 旁路不拦（预期行为）。

### FR-9 资源限制（P0）

单次 `run_code` 执行受以下限制，任一超限强制终止并返回带**来源标记**的错误结果
（四来源独立标志：cancel / deadline / fuel / mem；已完成的内部调用照常入审计）：

| 限制 | 默认 | 语义 |
|---|---|---|
| max_tool_calls | 200 | 内部调用次数（含失败调用），超限后拒绝后续调用。v2 调整（原 50）：AppWorld 高循环任务金标 91–244 次调用，50 连两阶段打法都装不下（实测摸底阶段即触顶） |
| fuel | 5e9 | 纯计算循环指令预算（v2 P2 调整，原 1e9：2.5 MB 文件 guest 内整读超 1e9，首试即 fuel 耗尽——5e9 覆盖且仍低于 timeout_secs=120）（M1 实测标定：debug 构建下 1e9 ≈ 11.6s 纯解释循环、~8.7e7 fuel/s，release 更快——数十秒级而非分钟级，故墙钟兜底在 FR-9） |
| timeout_secs | 120 | 墙钟；0 = 无墙钟（仍受 cancel/fuel/mem 约束）。四个执行点：**入口门**（cancel 已置 / deadline 已过在 guest 启动前直接拒绝——warm 模块可在单个 epoch tick 内跑完，epoch 观察不到）/ epoch 回调（同时查 cancel 与 deadline）/ 内部调用入口查剩余 / 收尾裁决（含 `_initialize`/编译失败路径） |
| mem_mb | 256 | 沙箱线性内存上限（ResourceLimiter） |
| max_result_chars | 60_000 | 返回值+日志总尺寸（**字符**口径，对齐 max_output_chars） |

**全部限流项均为 `CodeConfig` 可配字段（FR-11），并经 FR-13 三层 API 对第三方
开放——含运行期调整（轮次间原子生效，进行中的执行沿用开启时参数）。**

**取消联动**：cancel 标志（Ctrl-C / TUI Esc）在 wasm 指令边界 ~百 ms 级生效。
**诚实边界**：在飞的内部调用（如阻塞中的 shell/MCP 调用）不可被外层超时打断
（对齐仓库 timeout 软上限口径）。**最坏总墙钟公式**：
`timeout_secs + max(shell_timeout_secs（batch 片段可 Replace 双向上调——敌对任务
文件可放大此值）, MCP CALL_TIMEOUT=120s, 其余工具自身上限)`——第二项写明数值
上界，防"上限"虚指掩盖 batch 上调 shell 超时的放大效应。
验收：死循环脚本被 fuel/timeout 终止且文案带来源；按下取消后执行终止且外层 run
以取消语义收尾。

### FR-10 可观测与审计（P0）

- 事件：内部调用的 `ToolCall`/`ToolResult` 照发（v1 不包装、前端零适配）；
  `console.log` 经专用事件变体转发；
- 审计：外层 `ToolCallRecord.input` 含完整代码文本；内部调用紧凑 trace 挂
  `ToolOutput.meta`（名称/状态/时长/**参数长度与双哈希**——`in_hash` 为 guest
  发出的原始输入、`in_hash_eff` 为 hooks Modify 后实际执行的输入（未 Modify 时
  省略），哈希用 sha256 截 128bit——可对证"代码执行时实际发了什么"而不落明文/
  `logs_dropped`/`call_count`，照 `task` 工具先例）。**明文口径**：事件流本就带
  完整明文（ToolCall 事件的 input 字段）——"不落明文"只对 result.json 成立
  （防其二次泄密），不是对事件流的声明；
- 成本：内部调用的 usage 经 `ToolOutput.usage` 折算诚实入账。

### FR-11 配置面（P0，v0.4 升级——第三方集成的核心配置面）

新增 `Config.code: CodeConfig`（字段：enabled（**默认 false**，FR-13）/
deny_paths / fs_write_dirs /
max_tool_calls / mem_mb / timeout_secs / **fuel**（u64，默认 5e9，高级项——
评测可复现性与成本校准用）/ max_result_chars）——**FR-9 全部限流项 + FR-8 全部
路径项无一例外可配**。`run_code` 随 `code`
cargo feature 编译，默认关闭（体积定位，见 NFR-3）。

**batch 覆盖策略**（不可信片段只能收紧，经 `PartialCodeConfig` 逐字段合并、
禁整体替换——整体替换会让片段用默认 deny_paths 顶掉 base 自定义值）：
`enabled` 片段出现即拒（bad_task）；`timeout_secs` = Clamp::TightenOrInit
（0=无上限是最松）；`max_tool_calls`/`mem_mb`/`max_result_chars`/`fuel` =
Clamp::Tighten（plain min）；`deny_paths` 并集；`fs_write_dirs` 以全集为单位元
的交集（任一侧空 → 取另一侧——空 vec 恒为"全可写"最松值，语义不随来源翻转；
两侧非空 → 交集，交集可为空 = 收紧到无处可写，合法终值）。

**两种"整体替换"对照（防混淆）**：运行期 `set_code_config` 的整包替换，调用方
是**可信宿主**（是能力：一次设定一致配置态）；batch 片段的整体替换，来源是
**不可信任务文件**（会让默认值顶掉 base 自定义，故只允许 PartialCodeConfig 逐
字段收紧）。同名不同理，两处口径以此为准。

验收：各字段片段覆盖逐一生效；`enabled` 覆盖被判 bad_task；任何片段不得放松
base 值；运行期 `set_code_config` 调整后下一次执行按新参数生效。

### FR-12 错误路径（P0）

guest 抛出的 JS 异常 → 外层 `is_error` 结果，含异常 message + 完整 stack + 已收集
日志 + 已完成调用数——供模型下一轮修复。超限/取消 → 带**四来源标记**
（cancel/deadline/fuel/mem）的错误结果，不静默。
验收（A-11）：guest throw 的外层结果为 is_error 且含 message/stack/日志/调用数；
四来源各自的错误文案可区分（与 A-4/A-5 联动）。

---

## 4. 非功能需求（NFR）

### NFR-1 安全

能力面**构造性**收敛：沙箱无系统接口是编译期事实而非运行期约定；guest 可调集
构造性排除 run_code/task/todo（防递归 DoS 与成本炸弹）；prelude 能力面收口
（import 包装不可从 guest 直达）。deny_paths **防误读不防恶意**（shell 旁路明示，
敌对场景正解是 hooks 或去 shell）；无 hooks 部署下 guest 调用零门禁（与模型直调
同面、非新增，文档化）。出域通道仅三条（tool_result 回上下文 / shell 网络 /
MCP），全部是既有授权面，不新增。治理分层：进程内能力面（本工具）+ 部署隔离
（容器/无秘密环境，宿主责任，文档化边界）。已知残余：workdir 沙盒 TOCTOU
（既有 `confined_to` 文档口径，不因本工具扩大——guest 顺序执行、无并发原语）。

### NFR-2 性能

wasm 引擎/模块进程级缓存，单次实例化毫秒级；guest 适合"编排 + 中等规模数据处理"
（万行级转换）；不承诺重型 ETL 性能（正确姿势是编排 shell 交 sqlite3/awk，属设计预期）。

### NFR-3 二进制体积与交付

`code` feature 默认关闭：默认构建体积与行为零变化；开启后增量控制在 ~10 MB 量级
（wasmtime + 资产），musl 静态构建与既有 CI 门禁不受损。资产平台无关，单份嵌入。

### NFR-4 兼容（零漂移）

不启用 `code` feature 时，全部现有行为、测试、二进制输出**逐字节等价**。启用但
未开启（enabled=false，默认）时：run_code 对模型零可见（不进 tool_defs）、零
schema 成本。开启后：session/恢复、上下文压缩、tool_use/tool_result N:N 配对、
plan 派生、hooks 调用次序（顶层）均不受影响；Python/Node 绑定仅新增
enable_code/disable_code/set_code_config 方法与 `code` 转发 feature（FR-13）
——ToolCtx/ToolOutput 零字段变更。

### NFR-5 离线可测

测试使用嵌入资产真实执行（进程内、无网、无外部工具链），`cargo test --features code,mock`
可完整回归。

### NFR-6 可维护性

dispatch 单点共享（loop 与 run_code 无复制粘贴）；资产版本钉死可追溯（quickjs-ng
版本 + prelude 哈希记录在文档）；LLM 可见描述与实现同步（描述即规格）。

---

## 5. 范围外（本期不做）

1. 内部调用的并行执行 / 异步 guest API（Promise、多线程 fan-out）；
2. JS 以外的 guest 语言（WAT 直写、Rust/AssemblyScript 工具链、Python）；
3. 沙箱直写文件（绕过工具 JSON 参数的零拷贝大文件写）——数据量证明需要时再做；
4. 网络能力注入（fetch 类）——出域治理另行立项；
5. LLM 描述瘦身用的内置 skill cookbook；
6. code 执行过程的交互式人审暂停（Cloudflare Code Mode 的 pause-for-approval 形态）。

---

## 6. 验收标准（端到端场景）

| 编号 | 场景 | 通过判据 |
|---|---|---|
| A-1 | mock provider 驱动：模型调用 run_code，代码内 `tools.call` 一个 echo 工具并返回其结果 | 外层结果含 guest 返回值；内部 ToolCall/ToolResult 事件成对出现 |
| A-2 | 代码读 DB（mock 工具）分页 → 聚合 → `write_file` CSV | 文件落盘 workdir 内；结果仅含路径+统计 |
| A-3 | 代码调用被 hooks Deny 的工具 / 与模型直调对照 | Deny → guest throw + after_tool(executed=false)；同 hooks 下「模型直调」与「guest 内调」gate 序列等价、仅 subagent 标记不同（`__code__`）；外层为一次普通工具调用 |
| A-4 | 死循环代码 | fuel/timeout 终止；带来源标记的错误结果；已执行内部调用入 meta |
| A-5 | 执行中 Ctrl-C | 指令边界 ~百 ms 内终止；run 以取消语义收尾；session 完好 |
| A-6 | 不启用 feature 的全量回归 | 现有测试全绿、无体积增量 |
| A-7 | 代码内 `../` 路径写文件 | 被拒（写恒以 workdir 为界——比默认配置下模型直调的口径更紧，见 FR-8 读放行/写恒拒） |
| A-8 | 代码经 shell 读 deny_paths 命中的文件（`cat .env`） | **不被拦截（预期行为钉住）**——deny_paths 仅作用于 4 个文件工具，防误读不防恶意 |
| A-9 | 代码调 run_code / task / todo | unknown tool（内部注册表构造性排除） |
| A-10a | 开关（M2）：config / set_code_enabled / enable_code | 默认关：模型不可见零 schema；开启后可见可调；运行期开关往返 tool_defs 一致；显式点名未开启报错带开启提示 |
| A-10b | 调参（M3）：set_code_config 运行期收紧 max_tool_calls | 下一次执行按新参数生效；进行中执行不受影响 |
| A-10c | 绑定（M4）：run 期间调 enable_code/set_code_config | 返回 busy 错误 |
| A-11 | guest 抛 JS 异常（FR-12） | 外层 is_error 结果含 message + stack + 已收集日志 + 已完成调用数；四来源错误文案可区分 |

---

## 7. 开放问题

1. `read_file` 返回行号前缀（`N\t`，行号右对齐可含前导空格——按第一个 `\\t`
   切分），guest 做数据处理需 strip——v1 在描述中提示，
   v2 评估 `read_file` 加 `raw` 参数或 `read_file_raw` 工具。
   真实旅程 A/B（2026-09）：模型能按提示正确 strip（`replace(/^\\s*\\d+\\t/, "")`），
   非阻塞；**同批发现的两个真阻塞已修**——`read_file` 分页窗口从文件起算导致
   ~4×max_output_chars 字节后不可达（已改窗口化，见 CLAUDE.md read_file 条），
   与 guest 内调继承展示帽导致整读不可达（已改 `GUEST_OUTPUT_CHARS`，设计 D13）；
   **新证据（task4，2026-09 第二批）**：行式输出还使「逐字节复制」不可达——
   strip+join 往返必在尾部多一个换行，模型连跑 4 个 run_code「实验」探测文件尾
   换行可观测性后仍无法保真（校验逻辑本身全对）。raw 参数从「锦上添花」升级为
   「无 shell 配置下的字节保真读取唯一解」，v2 优先级应上调；
2. ~~`code` feature 默认值~~ **已拍板（v0.2 + v0.3）**：编译期 feature 默认关；
   **运行期 `enabled` 默认关（FR-13）**——运行期开关已把安全问题从产物决策解耦
   （编入未开启 = 零暴露、仅体积成本）。~~v1 release 产物仍不带 code feature~~
   **已复议（2026-09，§10 A/B 两批指标完成后）：release 产物带 `--features code`、
   开关默认关**——+≈4.6 MiB 换开箱可用，红线（被注入模型默认不获得循环+代码执行
   能力）由 D15 的运行期默认关维持；release.yml 与 README 发布节已同步；
3. ~~内部调用标记~~ hooks 侧已可用 `subagent="code"` 区分（v0.2 D14 已决）；
   事件流是否需要包装变体仍开放（v1 裸发，观察反馈）；
4. `max_result_chars` 默认 60_000（对齐 max_output_chars 量级）是否合适——**两批
   真实旅程校准（2026-09）**：全部 code 臂的返回值都是紧凑摘要（数百~数千字符），
   60k 从未被触及——默认偏大但无害（超限只截返回值、有标记），维持现值，留观察；
5. **extra 工具（绑定宿主 CallbackTool）的 guest 可见性**：v1 不继承（与 task
   子 agent 同边界）；v2 若要支持需 `tools::build` 签名加 extra 参数并同步
   child_tools 与绑定构造点——届时 US-4 的完整体验才成立。

## 8. v2 改进计划（2026-09 复审：三批 A/B 证据 + 外部评审意见合并）

> **落地状态（2026-09）**：P1 两项已实现并全门禁通过（设计 D16/D17：guest I/O 结构化 +
> prelude；wasmtime 磁盘缓存——见上）；开放问题 1（raw 字节保真）随之关闭。P2 的**机制**
> 已落地（设计 D18：描述瘦身至 776 字符 + 二进制内嵌 skill `run-code`，磁盘同名可覆盖；
> 附修 raw 默认窗口 3 字节对齐），配对 A/B 实验另行执行后回填结论。P3 照旧待做。

**P1 · guest I/O 结构化（一个工作包，吸收开放问题 1）**
- guest 工具统一返回 `{content, truncated, totalChars, raw?}`；write 侧对称支持
  base64 + 写后读回校验（失败错误须可重试）——raw 字节保真随之落地。
- 主 loop `read_file` 输出**头部前置规模元数据**（路径 · 总行数/字符数 · 当前窗口）：
  把「装不进对话」的决策信息前移到首次读取。刻意**不做**事后翻页启发式（同文件
  连续 K 页发警告）——确定性信息 > 事后警告，且零魔法；病根是决策信息缺位不是缺提醒。
- guest prelude（Rust 侧注入 JS 字符串，**不动 glue C**——改 glue 须重编 wasm 资产并保
  sha256 奇偶，不值得）：`readAll`（自动循环至 truncated==false）/ `readLines`（已剥
  行号前缀）/ `writeVerified` 三个，极小纪律——描述点名完整清单 +「其余没有」，封死
  幻觉面。模型输出从"实现机制"降回"表达意图"，同时压输出 token 与契约违例。

**P1 · wasmtime 磁盘编译缓存（半天活）**
`Config` 未配缓存（已核实：引擎/模块缓存仅进程内 OnceLock，每进程冷启动重付
quickjs.wasm 懒编译）。开 `CacheConfig` 并**显式 `set_directory`**（手写 XDG，镜像
skills 目录定位；不用 load_default——musl/无 HOME 行为不可控）；目录失败优雅降级为
禁缓存（不可 fatal）。musl 冒烟 + cargo-audit 回归。注意：既有 A/B 均为 debug 二进制、
每 run 新进程——**时间维度「code 小任务略慢」的结论在缓存落地前挂起**。

**P2 · 描述 v4 = 瘦描述 + 内置 skill 文档（A/B 后再动）**
描述保留决策规则 + 一句话 API + prelude 清单；完整 API 参考做成**内置 skill**
（单二进制不能交磁盘文件：assemble 合成内置 SkillEntry、read_skill 解析内置正文），
错误消息只留一行指针。废弃"API 进错误消息"方案（失败往返 3–8k 比描述税更贵）。
**统计纪律**：配对 bootstrap 而非中位数差，每 regime ≥20 个配对任务再下结论
（GAIA n=11 里 towers 单点 +133% 即可拉动均值；gen.py 确定性生成扩样是脚本活）。

**P2 实验结果（2026-09，glm-5.3-flash，2 臂 × 40 任务 = 80 runs）**：两臂唯一差异 =
二进制（full：全量描述 ~1.9k 字符、无内置 skill；slim：776 字符瘦描述 + 内置 run-code skill）。
合成套件 = small（2–60 KB，5 题型×4）/ large（1–9 MB，5 题型×4），确定性生成 + 精确 gold，
tools 含文件四件套 + code.enabled（无 shell，隔离 run_code 使用），300k…400k run 预算。
结果：**准确率持平**（39/40 vs 40/40，唯一 miss 是 5 万行求和差 1 的算术滑差、非契约失败）；
**token 一致省**（全量 -8.1k 均值 / large regime -15.1k 均值、-13.6k 中位，方向一致但 n=20
未过显著线——大方差来自下述 fuel 长尾）；**渐进披露形态符合设计**：small regime 20/20
全程未读 skill、零成本；large regime 11/20 主动 read_skill 且通过。代价：small regime
output +263 / 墙钟 +11s 显著（瘦描述下模型多 peek read_file 确认格式，8/20 vs 3/20），
后续可在描述里补一句头部元数据格式对冲。**两个附带发现**：① 默认 fuel=1e9 撑不起
「~200 万字符一次读完」承诺（2.5 MB readAll 首试必耗尽、模型重试自救烧轮次——20/20
大任务两臂均现 fuel 耗尽）→ 已按证据上调默认 5e9；② json content 此前丢每页末行换行
（两臂同错暴露：按换行计数每文件少 1）→ 已改逐行保留原始终止符。实验产物在
/tmp/ta-p2（gen/run/analyze 三件套，可复现）。

**P2 补充批：code vs 不用 code（trad 臂，同二进制 config 关 code.enabled，n=20/regime 配对）**：
- **small regime（2–60KB / 300–800 行）**：成功率 code **20/20** vs trad 17/20；token 均值
  code 7.4k vs trad 35.8k（**code=trad 的 20.7%**，中位 6.2k vs 24.1k）；墙钟均值 30s vs 85s。
  配对 Δtoken(code−trad) CI95=[-45k,-15k] **显著**。
- **large regime（1–9MB）**：成功率 code **19/20** vs trad **5/20**（trad 仅有的成功：一个抽样
  猜中 top-k、两个烧 247k/273k token 磨穿）；token 均值 code 86.7k vs trad 172.8k（**code=trad
  的 50.2%**）——且 trad 的大头是失败 run 烧掉的预算（budget_exceeded / max_tokens / 超时），
  纯浪费；墙钟均值 331s vs 598s（中位 299s vs 614s）。配对 Δtoken CI95=[-169k,-11k] **显著**。
- **与 GAIA L1 结论（小文件 code +40–130%）的和解**：crossover 在数据量而非工具调用数——
  GAIA 小任务单次读取即可完成（code 臂的描述税+工具循环开销占主导）；本套件 small 臂已是
  300–800 行，越过「进上下文心算 vs 沙箱执行」的交叉点。**适用判据：数据能在一两页
  read_file 内读完且心算可靠 → 直接读更省；再往上，code 两轴全胜。**
- **对照边界（诚实声明）**：本套件无 shell 臂——任务全部可用 awk 一条管道解决，带上 shell
  的对照会几乎全场最便宜；code 的不可替代场景是**无 shell / 锁死部署**（能力构造性沙箱）
  与工具返回的分页数据（MCP SQL 等无 shell 等价物）。GAIA 批已有 shell 臂数据。

**P3** 成本口径 cache 折算（effective cost）· DSBench 补充批（待网络窗口）·
A/B 套件开源成 mini-benchmark。

**明确不做**（维持原判）：run_code 进子 agent 白名单 · 沙箱内嵌 xlsx/pdf 解析器
（宿主侧预处理写成 recipes 文档，与 PDF host-push 同一哲学线）· 按 GAIA n=11
直接砍描述。

## 9. 里程碑（评审通过后）

M0 dispatch 抽取（纯重构）→ M1 资产 + wasmtime 骨架最小闭环（含 musl 交叉验证
与 fuel 基准）→ M2 内部注册表 + 授权/文件/审计 + **开关面（FR-13）** →
M3 限流/事件/配置/文档。每步附测试，详见技术实现文档 §15。

## 10. 成功指标（PRD，验收后校准）

| 维度 | 指标 | 目标 |
|---|---|---|
| 功能 | §6 验收场景 A-1…A-11 | 全部通过 |
| 质量 | 既有测试回归（feature off） | 零回归、零体积增量 |
| 效率 | 真实数据处理任务 A/B：多轮工具调用 vs run_code | 平均轮次与总 token 下降 ≥ 30%（业界实测下限为简单任务 -32%） |
| 体积 | `code` feature 开启后二进制增量 | ≤ 15 MB（含资产） |
| 安全 | 渗透清单：路径逃逸 / 越权调用 / 失控循环 / 出域通道新增 | 全部被既有机制拒绝（A-4/A-7 + 代码评审） |
| 可靠 | 模型一次生成的代码可运行率 | 抽样统计（描述迭代优化的依据，不设硬门槛） |

真实测量（2026-09，glm-5.2 经 anthropic 兼容网关，双任务三臂、同 prompt/模型/limits）：

- **task1**（900 行 JSONL 聚合出双 CSV，数据量超单轮输出预算）：trad **结构性不可完成**
  （9 轮烧 121k tokens 仍写不出文件——在上下文里聚合装不下）；shell 3 轮 ~4.9k tokens 18s；
  code 5 轮 ~10.3k tokens 48s（1 次 run_code 全量读入聚合，成功）。数据不进上下文的场景是
  0→1 的质变，远超 30% 线。
- **task2**（12 个小 markdown 批量替换）：trad 4 轮 25 次工具调用 ~7.4k tokens 24s；
  code 5 轮 ~10.1k tokens 42s——**小批量场景 code 反而更贵**（每轮都多一层代码生成开销），
  与工具描述「单个工具调用就能完成的事不要用本工具」的定位一致。
- 结论：指标线 30% 应按**场景分层**校准——大数据场景质变达标；小批量场景 run_code 不是
  目标场景，不计入。
- **第二批（glm-5.3-flash，新增三轴场景 2026-09）**：输出密集（按规则生成 12×150 行）
  code **14.7k tokens 为全场最低**（shell 42.5k，trad turn 3 即撞 max_tokens 失败）；
  LLM 弱项（300 条跨时区换算+区间合并）code **10.1k / 3 轮全场最优**（shell 38.5k，trad
  试图文本枚举换算被 16k 输出帽截断失败）；计算密集小数据（368 行模糊去重）三臂全过、
  code 反而最贵（63.9k vs trad 53.7k）——**flash 级模型对 ≤~400 行/20KB 的在上下文内
  匹配能力已够用**，该轴要出区分度需 1k+ 行。flash 下 task1-trad 烧 281k 仍败（比 5.2
  的 137k 更执拗），结构性结论跨模型稳定。
- **可运行率抽样（两批去重后 12 个 code 臂 run，glm-5.2 ×8 / flash ×4）**：任务成功率
  11/12（91.7%）；成功任务里**单次 run_code 即完成主体** 8/11（73%）。多次迭代的 3 例
  诱因分类：① task1-flash×3 次——截断标记契约踩坑（边界契约，非语言能力，已在描述中
  补契约后消失于后续 run）；② task6-flash×4 次——编辑距离算法首版 bug 后收窄定位
  （正常迭代）；③ task3×2 次——先探查 schema 再写 join（合理的两段式）。唯一失败例
  task4 的 8 次调用中 5 次是在**探测字节保真原语**（开放问题 1 的基础设施缺口，
  非代码能力问题）。结论：语言能力不是瓶颈，边界契约（截断标记/字节保真）才是迭代
  的主要来源——与开放问题 1 的上调优先级互证。
- **公开语料外部锚点（第三批，2026-09，GAIA 2023 validation 子集）**：业界没有直接测
  "同任务 trad vs code"的公开基准，做法是取 GAIA validation 165 任务中**local-only 文本附件**
  的 11 个（排除需浏览器的与二进制格式 png/mp3/pdf/pptx；xlsx×8 宿主侧转逐 sheet csv、
  zip 预解包——三臂输入完全相同，ablation 公平性由双臂设计保证、外部锚点由官方 Final
  answer + 官方 normalize 判分保证；与社区判分审计 gaia-verified 的 errata 零交集）。
  按 level 再抽无可加项：L3 附件任务 7/7 全需浏览、L1 可读附件仅 5 个、无附件任务
  111/127 需浏览——筛选粒度由「格式 × web 依赖」支配而非 level，子集已横跨三级
  （L1×3 / L2×7 / L3×1）。
  三臂 × glm-5.3-flash：**trad 9/11、shell 10/11、code 10/11**；token 中位数 trad 2.7k /
  shell 5.1k / code 4.7k。两条结构性结论与自产 A/B 完全互证：① **GAIA 本地文件任务
  普遍很小**（10–30 行 csv）——整读一次即可作答，trad 最省（约 code 的一半），code/shell
  各背 ~1.5–2k 描述税；唯一算法型小任务（97 字节 ASCII 地图贪心覆盖）也是 trad 更省
  （5.1k vs 11.9k，双双通过）。② **最大的一个任务（~24KB XML + 多列去重网格）是唯一
  结构性分叉点**：trad 臂先爆 300k 预算闸门、重试又 stream_idle 拿不到答案（合计烧
  ~335k），code 臂 37.9k 通过——与自产 task1/3/5/7 的失败签名一致。全臂同错的 1 例
  （awning）是题面推理陷阱（后院朝向反转），gold 无误、不区分臂。判分器自身踩过一坑：
  模型偶尔用中文回「最终答案：X」，抽取正则须双语兼容。**许可证纪律**：GAIA 为
  CC-BY-NC，任务数据全程只落 /tmp 不进仓库，本文档只记聚合计数。三批合并的产品结论
  不变：数据装不进对话时 code 不可替代、小数据 trad 更省——**决策权在模型**，描述里的
  【何时用/何时不用】指引（v3）就是为此校准的。
