# run_code 技术实现文档

| 项 | 值 |
|---|---|
| 状态 | v0.8（外部审核第 3 轮单点落地，见修订 v0.8） |
| 版本 | v0.8 |
| 日期 | 2026-09-08 |
| 关联文档 | `docs/run_code_requirements.md`（需求规格，FR/NFR 编号在本文引用） |
| 代码基线 | master @ 4024fa0（其后行号会漂移，以代码为准） |
| 修订（v0.8） | **外部审核第 3 轮单点落地（2026-09）**：guest→host 工具调用的 args 解析失败从静默 `json!({})` 降级改为 `host` 类结构化错误（`parse_guest_args` 纯函数 + 单测；对称于 Phase A 坏指针错误）。可达性：shipped glue 恒发合法 JSON，失败仅限绕过 JS 包装直戳 wasm import 的 guest——模型可见契约（描述/skill/附录 A 的 kind 词汇表）不变，**零资产重建**。 |
| 修订（v0.7） | **外部评审核实落地（2026-09）**：① `__budget` 增 `tokens_left`（run 级 token 预算剩余，无闸门时 null，与 wall/fuel 同一 nullable 约定）——载体 `ToolCtx::budget` 从裸计数器 `Option<Arc<AtomicU64>>` 重构为 `Option<BudgetGate{used, cap}>`（lib.rs 门面 re-export；guest ctx clone 的是 gate，used 为 Arc，读数是活的；gate 侧饱和求差，过顶报 0 不回绕）；② registry 构造期快照 vs MCP 热增删的边界**文档化**（§2 既有口径 + README 热增删节 + CLAUDE.md；评审报 P1，核实为与 SubAgent 同款既定快照语义——热增不可见、热删仍可调，连接活到 registry drop，最坏退出延迟一次 CALL_TIMEOUT，无孤儿）；③ 评审另两条 P1 核实**不成立/误定级**：`SharedTool` 不转发 `set_shutdown` 不是 MCP 回收缺口（reaper 武装在共享 `Arc<McpClient>` 上、顶层列表持有同 client 克隆，覆盖「run_code 阻塞 MCP + 会话退出」场景；`&mut self` 经 Arc 本就无法转发，设计取舍在案），versioned registry 与 H3 单构造点 + 克隆共享缓存设计相抵。 |
| 修订（v0.6） | **AppWorld 高循环波评审落地（2026-09）**：① guest 注册表滤除改用独立能力标记 `Tool::available_to_code()`（默认 false；文件四件套/shell/McpTool 显式 true——MCP 漏覆写即断 AppWorld 机制；此前搭 available_to_subagent 便车）；② assemble 的 extra_tools 经 `SharedTool`（Arc 共享同一实例）进注册表，`CallbackTool::with_code_access()` 逐工具授权（§2 的 v2 备选项落地）；③ guest-only 原语 `copy_bytes(src,dst)`——host 流式字节保真复制，策略面在拦截层 copy 臂（src 读规则/dst 写规则）；④ 保留名 `__budget` + prelude `tools.budget()`（剩余调用/墙钟/燃料，查询免费）；⑤ 错误封套结构化字段 kind/tool/retryable（§5.1，glue 重建 sha256 3c1c8790…）；⑥ prelude 增 tools.budget（封闭清单纪律：描述/skill/附录 A 三处同步） |
| 修订记录 | v0.2：并入三份 subagent 评审（架构评审 / 代码事实核对 / 安全红队）——guest 可调集与内部注册表（原 H-1/H-2）、deny_paths 执行层与诚实化、timeout 执行点、错误通道封套、PartialCodeConfig、两层 dispatch 抽取、行号修正（gate_tool 实际 agent.rs:765、notify_after :775、run_tool :1596）。v0.1 三处行号漂移已全部修正。v0.3：**运行期开关默认关闭 + 三层开启 API（§8.6，产品方拍板）**；enabled 默认 true→false；bindings 由"零改动"改为新增开关方法；默认暴露面归零（红队 I-1 消解）。v0.4：**资源限制全面第三方可配（产品方确认）**——`fuel` 暴露为 CodeConfig 字段；运行期调参 API `set_code_config`（整包原子替换，set_code_enabled 成为其便捷衍生）；绑定层可调参（set_code_config dict）。v0.5：**并入四路复审（架构 / 一致性 / 红队 / 代码事实核对，~50 项）**——H1 `execute_tool_call` 契约修正（ToolResult 发射归调用方，loop 装配循环原位保留，"diff 仅搬运"判据可达）；H2 载体统一为 `Arc<RwLock<CodeConfig>>`（不变量：只达 Agent 与 RunCodeTool）；H3 单一构造点（assemble + `set_code_tool` 后置安装，`try_new` 签名不动）；H4 `default_tools` 不含 run_code（显式点名判定零歧义）；H5 A-10 拆 a/b/c 对齐里程碑；H6 D1–D10 全文内联 §3。红队必修：写路径 lstat 拒悬空 symlink、拦截层规范化与工具沙盒解耦（restrict_to_workdir=false 语义明文）、审计双哈希 + sha256 截 128bit、最坏墙钟数值公式、拦截层次序反转（gate 先于存在性含未知名 / deny 判 effective 输入 / 墙钟检查先于保留名截获）、`__code__` 标记消歧 + task name 校验、默认清单补 `**/.aws/**`。 |

---

## 1. 总体架构

```
LLM → tool_use: run_code { code }
agent loop（src/agent.rs，零专用分支）
   └─ 普通叶子工具调用（Sequential，Slot::Ran）
      └─ RunCodeTool::run_with_emit（src/tools/run_code.rs，工具内部自治）
         ├─ 内部注册表（构造期四件套快照，见 §2——非 Agent 注册表）
         │    = tools::build(cfg.tools) + MCP clone，retain available_to_subagent()
         │    → 自然排除 run_code / task / todo（防递归与成本炸弹）
         ├─ wasmtime 实例化内置 quickjs.wasm 资产（无 WASI）
         │    Engine/Module 进程级 OnceLock 缓存，Store 每次新建（状态隔离）
         ├─ guest JS ── tools.call ──→ wasm import host_call_tool
         │     └─ 拦截层（v0.5 重排，评审 M3 + 红队低-1：次序 = 墙钟入口检查
         │         → __tools_list 保留名截获 → hooks 闸门（ctx 克隆
         │         subagent=Some("__code__")，§8.3；gate 先于存在性判定、未知名
         │         也过 gate——与顶层 gate_tool 对齐）→ deny_paths / fs_write_dirs
         │         （判 hooks Modify 后的 effective 输入）→ run_tool）
         │         ├─ find_tool（内部注册表；未知名 → unknown tool，A-9）
         │         ├─ run_tool（复用 agent.rs:1596 既有自由函数）
         │         └─ 事件经 run_with_emit 的 emit 转发
         ├─ console.log ──→ import host_log ──→ CodeLog 事件 + 有界日志收集
         └─ return 值 ──→ ToolOutput{ content: 返回值+日志, usage: 内部折算,
                                       meta: 调用 trace, … }
loop ← 单个 tool_result（内部调用不产生任何消息块）
```

## 2. guest 可调集与内部注册表（v0.2 新增，评审 H-1/H-2/中-1 的解）

**问题**：工具注册表活在 `Agent` 实例（`self.tools`/`self.tool_defs`），`ToolCtx`
不含它，`run_with_emit` 拿不到——"共享 dispatch" 缺物理载体；且 guest 若可调
`run_code` 自身则每层独立配额（fuel/mem/max_tool_calls 全 per-invocation），
自复制代码（`"("+f+")()"` 无需 quine 转义）= fork bomb / 栈溢出 DoS
（panic=abort 下 SEGV 不可恢复）。

**方案**：`RunCodeTool` 照 `SubAgent` 先例持**四件套快照**
（`cfg.clone()` + `client.clone()` + `api_key` + `mcp_tools.to_vec()`，
见 `subagent.rs:54-75`），构造期（或首次 run 惰性）构建**内部注册表**：

```rust
// 与 child_tools（subagent.rs:208-217）完全同构：
let mut inner = tools::build(&cfg.tools, &cfg, &client, &api_key, &mcp)?;
inner.extend(mcp.iter().cloned().map(Box::new));
inner.retain(|t| t.available_to_subagent());   // ← 关键：能力标记滤除
```

由此**一次确定 guest 可调集**：

| 可调 | 不可调（retain 滤除 / 不在构建集） |
|---|---|
| cfg.tools 白名单内的叶子工具（shell/read_file/write_file/edit_file/list_dir…） | `run_code`（防自递归）、`task`（防 50×25 轮成本炸弹）、`todo`（效果全在 records 派生，guest 调用无意义）——三者均 `available_to_subagent()=false` |
| 全部 MCP 工具（Arc-clone 共享连接） | extra 工具（绑定宿主注入的 CallbackTool）、skill 工具（read_skill/list_skill）——不在四件套构建集内，**与 task 子 agent 同一边界**（v0.2：FR-3 口径据此收敛，extra 支持列为 v2 备选——需 `tools::build` 签名加 extra 参数） |

- `tools.list()` 列**内部注册表**，与可调集一致（同步隐藏三者）；每条 name +
  description（截 200 字符封顶，防 50 工具长描述撑爆 guest 内存）。
- 嵌套构造无害：build 会构造嵌套 RunCodeTool，但它只存四件套克隆（引擎是进程级
  OnceLock，非实例状态），retain 后即被丢弃。
- MCP 热增删不感知快照——与 SubAgent 同款文档化边界（CLAUDE.md 既有口径）。
- `ToolCtx` **零新字段**：hooks 经 `ctx.hooks`、事件经 `emit`、注册表是工具自有——
  NFR-4 的"绑定零改动"推理保持成立。
- 白名单语义：guest 与顶层调同一 `cfg.tools` 白名单构建的注册表——顶层配不到的
  （如去掉 shell）guest 也调不到；MCP 部分同构。
- **v0.6 修订（高循环波评审）**：① retain 滤除改 `available_to_code()`（独立能力
  轴，默认 false——与 available_to_subagent 解耦：一个工具可对子 agent 开、对
  沙箱关，或反之）；② extra 工具经 SharedTool（Arc 同实例）加入且需逐工具
  `with_code_access()` 授权——上表"extra 不在构建集"一条就此失效；③ guest-only
  原语 copy_bytes 注册在 retain 之后（不经白名单）；④ 保留名 __budget /
  tools.budget() 与错误封套结构化字段见 §5.1/修订记录。

## 3. 关键决策记录

v0.1 定案的 D1–D10 **全文内联于此**（v0.5 H6：此前正文多处引用而决策全文已不在任何现存文档）；v0.2+ 修订/新增随后：

| # | 决策 | 理由 | 否决的备选 |
|---|---|---|---|
| D1 | guest = JS，宿主 = 编译成 wasm 的 QuickJS（quickjs-ng） | LLM 语言拟合度最高；wasm 层给出构造性能力沙箱；资产嵌入使宿主保持纯 Rust 零 C（musl 交叉编译不碰 C 工具链） | LLM 直写 WAT（语料稀少 + 无字符串/分配器/JSON）；Rust/AS 工具链（要求用户机装工具链）；原生 rquickjs（丢硬能力边界与纯 Rust 宿主）；Pyodide（10MB+ 慢启动） |
| D2 | 工具内部错误 → guest throw `Error` | JS 原生语义，可 try/catch 部分续跑（FR-3.2） | 返回 `{__error}` 对象让模型自判（封套前的旧案；通道由 D12 修正） |
| D3 | 结果超限 → 截日志、保返回值 | 返回值对模型迭代更重要 | 反向（截返回值保日志） |
| D4 | `tools.call` 返回值自动 JSON 解析，失败给原字符串 | MCP 多为文本输出，模型已知各工具输出形状 | 对内置工具做结构化封装（v2 再议） |
| D5 | 资产（quickjs.wasm）直接提交进仓库，`include_bytes!` 嵌入 | 对齐 `tests/fixtures/cups_3p.pdf` 先例；离线构建可复现；wasm 平台无关，**单份资产通吃所有 target** | CI 动态构建（离线不可复现）；运行时下载（破坏单二进制） |
| D6 | 授权延续 = 共享 dispatch（hooks/白名单/超时/预算/事件） | FR-7；内部调用与 loop 调用汇流在同一处，无旁路（载体由 D6′ 具体化） | 工具内自行调 `tool.run()`（绕过 hooks） |
| D7 | 文件访问复用 `read_file`/`write_file`/`edit_file`/`list_dir` 原工具 | FR-8：授权 ≡ agent 文件访问，零新增安全语义面 | 新建 fs_* 工具族 / 沙盒私有 fs API |
| D13 | guest 内调工具的输出预算**不继承**宿主 `max_output_chars`，改用数据规模常量（`GUEST_OUTPUT_CHARS` = 2,000,000 字符，字符单位不变） | 该帽是给**模型眼睛**的展示预算；guest 输出落 JS 堆（`code.mem_mb` 限），根本不进对话——真实旅程 A/B 发现继承让 `LIMIT 5000` 式整读（本表上面示例的前提）完全不可达，模型退回 17 次手动翻页、烧穿 run 级 token 预算 | 按 `mem_mb` 折算输出帽 / 加一个 CodeConfig 旋钮（多一面配置，收益为零） |
| D8 | 事件 v1 裸发（不包装），新增 `CodeLog(String)` 变体 | ToolCall/ToolResult 前端零适配；console.log 需专用通道 | `AgentEvent::SubAgent` 式包装（v2 观察后再定，开放问题 3） |
| D9 | `code` cargo feature，默认关闭 | NFR-3：默认构建体积/行为零变化（对齐 `tui` 先例）；wasmtime+资产增量 M2 实测 ≈4.6 MiB | 默认开启（体积定位受损） |
| D10 | loop 零感知原则：编排逻辑全部在 `RunCodeTool` 内 | FR-1/R-7；loop 侧唯一的必要变更是 dispatch 抽取（纯重构，行为不变） | loop 内识别并展开内部调用 |
| D6′ | 共享 dispatch 的载体 = **内部注册表四件套**（§2），非 Agent 注册表、非 ToolCtx 加字段 | 解决载体缺口且零 ToolCtx 变更；与 task 子 agent 边界完全同构，认知成本低 | ToolCtx 加 Arc 注册表（撞 NFR-4、`set_shutdown` 的 `&mut` 别名问题）；传 `&Agent` 给工具（能力面过宽，guest 间接够到 provider/compress） |
| D11 | guest 可调集排除 run_code/task/todo（`available_to_subagent()` retain，机制同 child_tools） | 防自递归 DoS（红队 高-1）与嵌套 agent 成本炸弹（中-1）；零新 trait 面 | 每层限流共享配额（复杂且仍不挡栈溢出）；深度计数器（retain 方案零成本已够） |
| D12 | host→guest 结果通道改**双封套**：`{"__ok":true,"content":…}` / `{"__ok":false,"error":…}` | in-band 裸 `__error` 键会与合法工具输出内容撞车（被注入的 MCP 可伪造成功为失败，劫持 try/catch 语义）；封套后碰撞不可能 | 保留 `__error` 约定（NodeProvider 类比不成立——那是同进程控制通道，这是携带第三方数据的通道） |
| D13 | deny_paths 定位为**防模型误读，不防恶意 guest**；执行点 = host_call_tool 拦截层、仅 4 个文件工具 | shell 全量绕过（`cat .env`）使其不构成安全边界；诚实化定位 + 拦截点唯一可行位置（文件工具不感知 CodeConfig，ToolCtx 无该字段） | 塞进文件工具（改模型直调行为，撞 NFR-4）；拦截 shell（需解析命令文本，不可靠） |
| D14 | 内部调用的 hooks 闸门收到的 ctx 是**克隆、`subagent=Some("__code__")`**（v0.5 红队低-6：`__` 前缀保留字面量 + task name 校验禁 `__` 前缀，消解 task 命名 "code" 的撞名） | 复用现有字段（零新增），宿主策略可区分"代码发起的调用"并单独收紧（红队 L-2） | 新增 origin 字段（无必要） |
| D15 | **运行期开关默认关闭**：`CodeConfig.enabled` 默认 false；开启是显式信任决策（三层 API，§8.6） | 产品方拍板（v0.3）；默认暴露面归零——被注入模型默认**不**获得"循环 + 代码执行"能力（红队 I-1 消解）；关闭时 run_code 不进 tool_defs，模型不可见、零 schema 成本 | 默认 true（v0.2 原案，暴露面问题）；关闭时仍注册但调用即报错（浪费模型轮次与 schema token） |
| D16 | **guest I/O 结构化（v2 P1，2026-09）**：主 loop read_file 输出**头部前置规模元数据**（`[文件名: 总行数 · 总字符 · complete|行区间]`，扫描 8 MiB 封顶、超出报下界）；read_file 增 `format:"json"`（{content, lines_total, chars_total, truncated, next_offset}，无行号前缀、limit 截断时 next_offset 精确续读）与 `raw:true`（字节窗 base64，offset/limit 按字节，窗帽折入 ctx.max_output_chars/2——主 loop 展示预算拉不走数据规模块）；write_file 增 `content_b64`（字节保真写）；guest 前导函数 readAll/readLines/writeVerified（**单物理行注入**，用户栈帧行号恒 +1 偏移；清单封闭，描述点名三个） | 可运行率抽样的迭代诱因是**边界契约不是语言能力**；决策信息缺位（首次读取不知文件规模）让换臂决策沉到成本已付之后——确定性元数据 > 事后启发式 | 同文件连续 K 页翻页发警告（治已烧完的钱、行为掺魔法）；错误消息载 API 文档（失败往返比描述税更贵） |
| D17 | **wasmtime 磁盘编译缓存（v2 P1）**：`cache` feature + 显式 `CacheConfig::with_directory`（手写 XDG：$XDG_CACHE_HOME 绝对路径 else $HOME/.cache，下挂 tinyagent/wasmtime）；目录失败**优雅降级为无缓存**、绝不阻塞沙箱启动 | 每进程重付 quickjs.wasm 懒编译（debug ≈10s）计入了 A/B code 臂墙钟；缓存按模块内容哈希 key、自带防毒化 | `load_default`（musl/无 HOME 行为不可控）；不缓存（时间维度结论失真） |
| D18 | **描述瘦身 + 内置 skill（v2 P2，2026-09）**：run_code 描述只留身份/环境/**封闭助手清单**/返回纪律/技能指针（≤1200 字符，测试钉住），深契约（read_file 三模式、字节保真复制、示例）移入二进制内嵌的 skill `run-code`（`src/tools/run_code_skill.md`，`skills::SkillSource::Embedded`）——`code.enabled` 时注入、磁盘同名覆盖内置、`read_skill` 按需加载（可压缩 tool_result）。附修①：raw **默认**窗口按 3 字节对齐（显式 limit 照传不夺），跨窗 content_b64 拼接 == 整文件 base64，字节保真复制不再依赖调用方自对齐。附修②（A/B 套件首跑抓到，两臂同错暴露）：json content 此前「strip 行终止符再以 \n 连接」——**每页末行换行丢失、CRLF 被归一**，按换行计数每文件少 1；改为**逐行保留原始终止符**（含末行 \n 与 CRLF），prelude readAll 的页间拼接从 join("\n") 改 join("")（页自带终止符，再 join 会注入空行） | 描述随每个请求的工具 schema 常驻——P1 后全量描述 ~1.9k 字符在 code 臂每个 turn 计费；渐进披露（agentskills.io 同款三级）把深文档挪到按需通道 | 把全部契约留在描述（每 turn 税）；错误消息载文档（失败往返更贵）；给 skill 加 per-turn 激活层（allowed-tools 仍告警不强制） |

## 4. dispatch 两层抽取（M0，v0.2 按 M-1 修订）

**不能**把顺序/并行两遍收敛成单一 `execute_tool_call`——并行批次的契约是
**gate 在父线程 spawn 前串行、run 在 worker 线程、after 在 join 后**
（`agent.rs:1430-1505` + `hooks.rs:68-76`），单一函数会让 gate 跑进 worker 线程，
破"approval decisions never race"。抽取分两层：

```rust
// ── 底层原语：现为 Agent 方法但只依赖 &self.ctx（agent.rs:765/775），降为自由函数 ──
pub(crate) fn gate_tool_call(ctx: &ToolCtx, name: &str, input: &Value) -> ToolDecision;
pub(crate) fn notify_tool_after(ctx: &ToolCtx, name: &str, input: &Value,
                                output: &ToolOutput, executed: bool);

// ── 上层：单次内联调用 = ToolCall 事件 → gate → run_with_emit → after，返回 ToolOutput ──
// ToolResult 事件**不在内**——发射归调用方（v0.5 H1）：
//   · loop：装配循环统一发（agent.rs:1513-1549，管块序 / N:N 配对 / 取消合成），保持原位
//   · run_code：每次内部调用返回后立即发（FR-3 实时性）
// 供 loop 顺序内联遍 与 run_code 内部单发 共用
pub(crate) fn execute_tool_call(
    tools: &[Box<dyn Tool>], defs: &[ToolDef],
    ctx: &ToolCtx, name: &str, input: &Value,
    emit: &mut dyn FnMut(AgentEvent),
) -> ToolOutput;
```

- **职责边界（钉死，v0.5 H1 修订）**：`execute_tool_call` 内发 **ToolCall** 事件
  （gate 前发——对齐 loop 内联遍现状：顺序遍先发 ToolCall 再过闸门）并执行
  gate → run_with_emit → after，返回 ToolOutput；**ToolResult 事件发射归调用方**
  （见上）——loop 的事件顺序逐字节保持，"diff 仅搬运"的 M0 出口判据由此可达
  （v0.4 契约把 ToolResult 收进函数内，采纳后事件顺序必变、判据必失败）。
  records push、usage fold、`Block::ToolUse/ToolResult` 组装、Slot 赋值留在
  **loop**（外层）——这正是"run_code 内部调用不进 records"（§8.4）成立的前提。
- loop 的并行批次改用底层原语手工两段（gate-before-spawn / after-join），
  既有次序契约逐字节保持；M0 出口判据 = 现有全量测试绿 + diff 仅搬运。
- run_code 内部调用 = **手搓原语序列**（ToolCall 事件 → `gate_tool_call` →
  deny/fs 拦截（判 Modify 后 effective 输入，§7）→ `run_tool` →
  `notify_tool_after` → 补发 ToolResult 事件），**不经过**
  `execute_tool_call`——§7 的拦截层必须插在 gate 与 run 之间，
  `execute_tool_call` 没有这个插入点（v0.5 加入拦截层后本条随之修订；
  这是 NFR-6「dispatch 单点共享」的唯一例外，两处序列由 A-3 等价测试钉住）。

## 5. wasm 边界 ABI

**资产（M1 已入库）**：`assets/quickjs.wasm`，1,001,495 字节，sha256
`3b03ba26…772e2c`。源码 quickjs-ng **v0.15.1**（取自 rquickjs-sys 0.12.2 crate
内 vendored 树，四个编译单元 quickjs/dtoa/libregexp/libunicode，**不含
quickjs-libc**）；交叉编译 `zig cc --target=wasm32-wasi -mexec-model=reactor`；
构建脚本与命令固化在 `examples/build-quickjs.sh`（glue 在
`examples/quickjs_glue.c`），重编后核对 sha256。宿主侧 wasmtime **36.0.14**
（minimal feature 集：cranelift + runtime + std，无 async/component）。
模块初始线性内存 32 MiB（`mem_mb` 的下限即此）、零 wasi import。

### 5.1 import / export（v0.2：封套化 + image 显式报错）

宿主实现（guest 的全部能力面；**M1 实测资产仅两个 import**，host_now 是未实现的扩展位——不占当前 ABI）：

```
env.host_call_tool(name_ptr, name_len, args_ptr, args_len) -> i64
    // args 为 JSON 字节；返回 (ptr<<32)|len 指向 guest 内存中的结果。
    // 结果统一双封套（D12）：
    //   {"__ok":true,"content":"<工具输出文本>"}          成功
    //   {"__ok":false,"error":"<工具名>: 原因"}            失败（Err/Deny/拦截/超限）
    //   v2 追加可选结构化字段（host 已知类别才给）：kind（denied/hooks/budget/
    //   deadline/unknown_tool/image/host/tool）、tool（调用名）、retryable（仅
    //   定性 false；tool 类刻意不给——host 无法机判工具自身错误）。glue 把它们
    //   附到抛出的 Error 上（e.kind/e.tool/e.retryable），message 不变。
    //   v0.8：args 解析失败也归 host 类结构化错误（对称于坏指针）——shipped glue
    //   恒发合法 JSON（stringify / undefined→字面 "{}"），parse 失败只可能是
    //   guest 绕过 JS 包装直戳 wasm import 传垃圾字节；静默降级 {} 会把协议错误
    //   伪装成"参数缺失"误导归因
    // 工具输出含 image Block（load_image 等）时显式 {"__ok":false,
    //   "error":"binary/image output unsupported in run_code"}——报错优于静默丢弃
    //   （对齐 openai.rs reject_documents 先例），模型可感知并改走顶层调用。
env.host_log(ptr, len)
    // console.log：发 CodeLog 事件 + 有界收集（§6 日志行）
env.host_now() -> f64
    // 默认不注入：guest 无时间概念（确定性）。注入与否属 CodeConfig 扩展位，本期不做
```

guest 导出（资产自带，**M1 实测定稿**，两处较 v0.2 稿有修正）：`alloc(size)->ptr`、
`dealloc(ptr)`（~~`dealloc_all()`~~——逐指针释放就够，一次一 eval 的生命周期
不需要整堆回收）、`eval_code(code_ptr, code_len)->i32`（**0=ok / 1=JS 异常**——
超限截断不占用返回码：超限一律 wasm trap，由宿主按四来源标志归因，见 §6）、
`take_result()`、`take_last_error()`（两者返回 `(ptr<<32)|len` 打包 i64）。
另：模块以 reactor 模式链接，**首次调用前必须调 `_initialize`**（ctors）。

**guest 侧封套解析的 NUL 契约**（M1 实测踩坑）：quickjs 的 `JS_ParseJSON`
要求 `buf[buf_len] == '\0'`（tokenizer 会越过 buf_len 扫 EOF 标记）——宿主写入的
是精确长度缓冲，guest 侧 `consume_envelope` 必须**拷贝到 len+1 的 NUL 终止副本**
再解析；原地解析读到堆上残留字节，症状是按长度/内容伪随机地
`"unexpected data at the end"`（node/wasmtime 双侧同现，实证锁定）。

**宿主把结果写回 guest 内存的机制**（实现注记，评审 L-12）：经 wasmtime
`Caller` 再入同 Store 调 guest 的 `alloc` 导出取写入点。注意再入 `alloc`
同样消耗 fuel、epoch 可能打在 `alloc` 内——写入路径须容忍 trap 并按超限收尾。

### 5.2 保留名 `__tools_list`

`tools.list()` 经 `host_call_tool("__tools_list", "{}")` 走同一 import，宿主在
拦截层**墙钟入口检查之后**最先截获（v0.5 红队低-1：时间检查必须先于保留名截获
——`__tools_list` 不计调用数，若截获在前，忙循环可用它绕开审计计数且不受
max_tool_calls 封顶；墙钟前置后忙循环至少被 timeout 封顶，且 guest 侧每次循环
本身耗 fuel），返回内部注册表清单——不进 hooks、不进 meta trace、不计调用数。
**撞名防御**：自定义工具/内置工具不得叫 `__tools_list`——`code` feature 开启时
`validate_tool_names` 追加拒绝该名（避免"guest 永远够不到真实同名工具"的隐性遮蔽）。

### 5.3 prelude（烧进资产的 JS 层）

- `globalThis.tools = { call, list }`：`call` 序列化 → 调 import → 读封套——
  `__ok:false` 则 `throw new Error(...)`（封套若带 kind/tool/retryable 则附到
  Error 上，见 §5.1），成功则 `JSON.parse` content、失败给原字符串（D4）；
- `console.log`：字符串化 → `host_log`；
- **能力面收口**（红队 L-5）：import 包装函数不得留在 `globalThis` 可达处
  （闭包内持有，防 guest 绕过 prelude 直调 import 造成语义漂移）；wasm 导出
  （alloc 等）不暴露给 JS 层；
- 确定性桩：`Date.now`/`Math.random` 访问即 throw（不支持，非桩 0；需求锚点
  FR-2 确定性条款——同代码同结果，评测可复现）。

## 6. 限流、中断与日志（FR-9；v0.2 按 M-2/高-2 修订）

| 机制 | 实现要点 |
|---|---|
| fuel | `consume_fuel(true)` + `store.set_fuel(N)`；**N 默认 5e9（v2 P2 起从 1e9 上调：A/B 实测 2.5 MB readAll 整读超 1e9、首试即耗尽），经 `CodeConfig.fuel` 可配（v0.4）**（**M1 基准回填**：本机 debug 构建 1e9 fuel ≈ 11.6s 纯解释循环（~8.7e7 fuel/s，`fuel_rate_calibration` 测试钉住，release 更快）——1e9 是"数十秒级纯计算"而非分钟级，正解是先 fuel 后墙钟的双层兜底；标定命令 `cargo test --features code --lib fuel_rate_calibration -- --ignored --nocapture`）。限流参数运行期可调（§8.6 `set_code_config`），进行中的执行沿用开启时快照 |
| epoch + 取消 | `epoch_interruption(true)`；全局 daemon ticker（OnceLock，~10ms 步进 `increment_epoch`）；每 Store `epoch_deadline_callback`：**同时判 cancel 与 hard_deadline**（见下），命中 → 置来源标志 + **回调返回 `Err`**；未命中 → `Continue(1)` 续期。**wasmtime 36 实测注记（M3，改这段前先读）**：① `UpdateDeadline` **没有 Trap 变体**（只有 `Continue(u64)` 与 async 专属 Yield），回调无法"拒续期"——中断的投递载体是**回调返回 `Err`**，它从 `new_epoch` libcall 一路抛穿成在飞 wasm 调用的错误，`terminated` 按来源标志归因（trap 文案本身不用）；② **`set_fuel(0)` 是死路**：运行中函数把 fuel 缓存在 wasm 局部变量、在自己的检查点把缓存刷回 store 槽——外部置 0 会被缓存覆盖（实测 guest 在中断后继续跑了 25s 到自然耗尽）；③ **epoch 检查点只布在 wasm 循环回边**（函数入口只初始化缓存 deadline、不检查；fuel 则两处都有）——投递延迟 = 下一个回边检查点：普通代码 ~10ms 量级，纯字节码空转循环的回边由 quickjs 内部中断轮询位点兜底（实测仍为几十 ms 量级）；④ fuel 在检查点**按块扣费**，紧循环的燃料速率 ~1e9/s（debug）——墙钟测试必须把 fuel 调离竞速窗口，否则两者同时到期、fuel 先赢裁决 |
| **墙钟三执行点**（高-2 修订） | ⓪ **入口闸（M3 实测补）**：epoch 回调最早也要到下个 ticker tick（~10ms）才能观察，而平凡 guest 程序能在一个 tick 内正常跑完（wasmtime 按模块缓存机器码，热路径 `return 1` 全程 <10ms）——**cancel 已置 / deadline 已过**必须在 eval 入口直接终止、不进 guest（与 batch worker / 子 agent spawn 的 gate-before-start 同纪律；audit meta 照常、fuel_used=0）。① epoch 回调查 hard_deadline = run 开始时刻 + min(code.timeout_secs, `ctx.deadline` 剩余)；`ctx.deadline=None` 且 timeout_secs=0 时无墙钟（仍受 cancel/fuel/mem 约束，诚实文档化）② `host_call_tool` 入口查剩余 ≤0 → 封套错误 + 置中断标记（后续调用即时拒绝）③ 收尾时统一裁决（`_initialize`/`alloc`/`eval_code` 的调用错误同样走 `terminated`——quickjs stdlib init 在 debug 下 >1s，短墙钟可能在 init 期间合法触发）。**诚实边界（写入 FR-9 与 README）**：在飞内部调用不可被外层超时打断（对齐 CLAUDE.md timeout 软上限口径），总墙钟 ≤ timeout_secs + max(最后一次在飞调用自身的上限)；第二项的**数值上界 = max(shell_timeout_secs（batch 片段可 `Replace` 双向上调，敌对任务文件可放大此值）, MCP CALL_TIMEOUT=120s, 其余工具自身上限)**（v0.5 红队中-4：写明公式，防"上限"虚指掩盖 batch 上调 shell 超时的放大效应） |
| max_tool_calls | host_call_tool 拦截层计数（含失败），超限 → 封套错误 + 中断标记 |
| 内存 | `Store::limiter`（ResourceLimiter）钳线性内存 ≤ mem_mb；limiter 拒绝时置 mem 标志 |
| 返回尺寸 | 统一**字符**口径（复用 `tools::truncate`，字段名 `max_result_chars`）——与 max_output_chars 同单位，消除 bytes/chars 漂移 |
| **四来源标志** | cancel / deadline / fuel / mem 四个独立标志位（回调与 limiter 各自置位），trap 后按标志映射 FR-12 错误文案——wasmtime trap 本身不携带来源。**M1 实测注记**：① mem 拒绝不直接 trap——malloc 失败被 quickjs 报成裸 `InternalError: out of memory`（无数字），宿主在 rc=1 路径按 limiter 标志翻译成带 mem_mb 数值的终止文案；② 深递归：quickjs-ng 在 `__wasi__` 下**硬禁用**自身栈限（`update_stack_limit` 恒置 no-limit，`JS_SetMaxStackSize` 无效），唯一边界是 wasmtime `max_wasm_stack`（1.5 MiB，压在 scoped 线程 2 MiB 默认栈之下）→ `Trap::StackOverflow` 归因为 "call stack exhausted (deep recursion)"，**不是** guest 可 catch 的 RangeError；**M3 实测修订**：1.5 MiB 档位下 quickjs 无检查的内部递归状态（线性内存侧）先于原生栈耗尽而损坏，终态是**不可归因的 OOB trap**（实测第 2524 帧 ≈1.45 MiB 处、访址 `0xffffffe4`）——`max_wasm_stack` 收紧到 **384 KiB**（≈600 个 JS 帧，仍远低于 scoped 线程 2 MiB 默认栈）让原生栈上限**决定性地**赢得竞速、稳定落回 `Trap::StackOverflow` 归因；且该现象仅在启用 epoch 插桩后出现（同码 1536 KiB 关 epoch 为正常 StackOverflow），机制归因存疑、以实测竞速边界为准 |
| **日志上限**（中-3 修订） | host 侧日志缓冲 ≤ 2×max_result_chars，超限停收集、记 `logs_dropped` 计数（截断优先于返回值，D3 不变）；CodeLog 事件在缓冲封顶后停发（防 NDJSON/TUI 事件洪水） |

## 7. deny_paths / fs_write_dirs 执行层（v0.2 新增，M-3/高-3 的解）

- **执行点与次序（v0.5 重排，评审 M3 + 红队低-1）**：`host_call_tool` 拦截层内部
  次序 = **墙钟入口检查（执行点②，先于一切）→ `__tools_list` 保留名截获 →
  hooks 闸门（gate 先于存在性判定、未知名也过 gate——与顶层 `gate_tool` 先于
  `find_tool` 的次序完全对齐；A-9 的 unknown tool 错误来自其后的查找失败）→
  deny_paths / fs_write_dirs（对 hooks Modify 后的 **effective 输入**判定）→
  find_tool + run_tool**。v0.2 的"deny 先于 hooks"经评审**反转**：deny 检查
  Modify 后的实际输入才能防"路径重定向绕过 deny"（否则 hooks Modify 把 write_file
  的 path 改指 `.env` 即绕过）；被拒调用照发 ToolCall/ToolResult 事件 + 进 meta
  trace，保持事件/审计完整性。仅作用于 4 个文件工具
  （read_file/write_file/edit_file/list_dir）。
- **匹配对象（v0.5 中-2 修订：拦截层自行规范化，与工具沙盒解耦）**：以 workdir
  为基准 `canonicalize` + 相对化得到的 **workdir 相对路径**（消灭 `./.env`、
  `a/../.env` 绕写；**不依赖 `confined_to`**——其语义绑定 restrict_to_workdir
  模式，本层须独立成立）。`restrict_to_workdir=false` 时 guest 可用绝对路径访问
  workdir 外文件：无法相对化到 workdir 的路径 **deny_paths 不适用——读放行、
  写恒拒**（v0.5 评审收口：guest 写入恒以 workdir 为界——fs_write_dirs 的权限
  宇宙本就以 workdir 封顶，「写在外」无语义；**读的放行是文档化边界**，含
  workdir 内 symlink 别名（`x -> 外部/.env`，canonicalize 解析到外即同此界）
  这类比绝对路径更易达的等价变体；敌对场景正解 = 开 restrict_to_workdir 或
  hooks Deny）。glob
  语义钉死：`*` 不跨 `/`、`**` 跨；**`**/` 匹配零或多段**（`**/.env*` 也命中
  根，根条目保留只为可读）；**不含 `/` 的模式只匹配根第一段**（`.env*` 不命中
  `a/.env`）。比较前按平台做大小写归一（macOS/APFS 大小写不敏感卷上 `.ENV`
  须命中）。
- **写路径强化（v0.5 中-1；M2 评审 sec-M-1 后升级为整链）**：write_file/
  edit_file 的路径在匹配前对 workdir 下**每一组件** `lstat`——**任一**是
  symlink 即拒（中间段与最终段一视同仁）。中间段 symlink 让词法形式的落点
  不可知：活的 `out -> /tmp/pwn` 使 `out/x` 词法在 workdir 内而 `open()`/
  `rename` 跟随链接落在外；悬空 `ln -s .env bak`（最终段）同理——模型被诱导
  "写 bak"实际落点 `.env`，**这击穿"防误读"定位本身**（非恶意攻击也中招）。
  workdir 下零 symlink 组件时词法形式 == 规范形式，其后的 deny/fs 匹配方可信。
  拒 symlink 是防误读语义下的最稳解（检查-写入间隙的 TOCTOU 仍是文档化
  best-effort，同 `confined_to` 口径）。**配套路径帽**：workdir 相对路径
  >4096 字节或 >256 段按「不可相对化」处理（读照常进工具自然失败、写
  fail-closed）——真实路径受 PATH_MAX 约束不可能超额，这种字符串是载荷不是
  文件名，防的是用深段数打递归 glob 匹配器的 host 栈溢出（评审 sec-M-2，
  实测 50 万段可 abort 进程、fuel/mem 均不设防）。
- **默认清单（扩充至现代密钥形态）**：`.env*`、`**/.env*`、`**/id_rsa*`、
  `**/id_ed25519*`、`**/id_ecdsa*`、`**/*.pem`、`**/.git/**`、`**/.git`
  （linked worktree 的 `.git` 是文件非目录，单独一条）、`**/.git-credentials`、
  `**/.netrc`、`.aws/**`、`**/.aws/**`（v0.5 红队低-3：原清单漏嵌套目录下的
  `.aws`）。glob 语义的测试向量进 §13-6。
- **fs_write_dirs**：仅 write_file/edit_file 受限；匹配同款规范化相对路径；
  batch 合并语义见 §9。
- **诚实声明（写入 NFR-1 与 README）**：deny_paths 防模型**误读**、不防**恶意
  guest**——shell 在白名单时（默认如此）`cat .env` 全量绕过；敌对场景的正解是
  hooks 对 shell Deny 或从 `tools` 去 shell。补验收 A-8 钉住此边界。

## 8. 组件设计

### 8.1 `RunCodeTool`（新文件 `src/tools/run_code.rs`）

```rust
pub struct RunCodeTool {
    code_cfg: Arc<RwLock<CodeConfig>>,  // 与 Agent::set_code_config 共写读（§8.6；
                                        // H2 不变量：该 Arc 只达 Agent 与本工具两处）
    cfg: Config,                        // 四件套快照（§2）
    client: reqwest::blocking::Client,
    api_key: String,
    mcp_tools: Vec<mcp::McpTool>,
    // 惰性构建的内部注册表（tools + defs），首次 run 时 build+retain
}

impl Tool for RunCodeTool {
    fn concurrency(&self) -> ToolConcurrency { ToolConcurrency::Sequential }
    fn available_to_subagent(&self) -> bool { false }   // FR-1 + D11
    fn run_with_emit(&self, input, ctx, emit) -> Result<ToolOutput> { … }
}
```

- **注册：单一构造点 = assemble（v0.5 H3 修订）**——feature on 时 assemble 构造
  **唯一**顶层实例（持共享 Arc），经 `Agent::set_code_tool` 后置安装（仿
  `set_hooks`，`try_new` 签名不动——避免 feature-gated pub 参数反模式，评审
  F-1 与架构 M2 的更优解合并采纳）；assemble 调 `tools::build` 前**把 `"run_code"`
  从白名单剔除**——顶层表内的 run_code 只能来自这一实例（`enabled=true` 时 push
  它，`set_code_enabled(true)` push 的也是它：`set_code_config` 才写得到表内实例
  的读端，A-10 由此成立；否则双实例各持独立 Arc、调参永远落空）。
- `tools::build()`（`tools/mod.rs:306`）的 `"run_code"` 分支**仅服务子注册表
  路径**（`child_tools` / run_code 内部注册表）——这些路径构造后即被 retain
  丢弃、实例的 Arc 孤立无碍（四件套克隆本身便宜）；没有该分支则白名单含
  run_code 时报未知名。
- `config::default_tools()`（`config.rs:35`）**不含 run_code（v0.5 H4）**——与
  task/todo 不同，run_code 的开关是 `code.enabled` 而非白名单成员资格；由此
  serde 判定零歧义：`#[serde(default = "default_tools")]` 下"没写 tools"与"写了
  不含 run_code 的清单"都解析为不含 run_code 的 Vec，**表内出现的 `run_code`
  必然是显式点名**——无需 shadow-struct 即可判定。
- **enabled 开关语义**（M-4 修订 + v0.3 D15，默认 **false**）：
  `enabled=false`（默认）→ 不进工具表、不进 tool_defs（模型不可见、零 schema
  成本）；config **显式点名** `run_code` 而未启用时报清晰错误（"run_code
  listed in tools but code.enabled=false —— set code.enabled=true to enable"）；
  feature off 时点名落入既有未知名报错，文案 actionable 化（提及 "rebuild with
  the code feature"）；`enabled=true` → assemble 注入唯一实例（无需 cfg.tools
  点名；要关就用 `code.enabled=false`——比从 tools 里摘更直白，评测锁行为同理）。
  运行期开关走 §8.6 的热更新面 API。

### 8.2 事件与观测（FR-10）

- 内部调用的 `ToolCall`/`ToolResult` 经 `emit` 直发（D8 裸发不包装，前端零适配）；
- 新增 `AgentEvent::CodeLog(String)`（`agent.rs:71` 枚举，消费侧 non_exhaustive，
  `tui.rs:1466` 有 `_ => {}` 兜底、harness event_json 编译期点名——已核实合规）：
  harness plain 写 stderr（ThinkingDelta 同款纪律）、`--json` 吐
  `{"type":"code_log","delta":...}`、TUI 渲染 sys_info 行（v0.5 架构 L6：**进
  转录**、可回看/`/export` 导出——审计友好；两条通道互斥，render-only 瞬态不留
  痕会丢执行日志，选留痕的）；
- `tools.list` 绕过 hooks——写进 `hooks.rs` 文档 Scope 节（自省不属策略面）。

### 8.3 hooks 与 ctx（D14）

内部 dispatch 用 `ctx` 的克隆、置 `subagent: Some("__code__".into())`（v0.5 红队
低-6：`__` 前缀保留字面量——`task` 工具的 name 参数可取 "code" 撞旧标记，task
name 加最小校验：非空、≤64、不以 `__` 开头，见 §12 subagent.rs 行）——hooks 可
区分代码发起的调用并单独收紧策略；其余字段（cancel/deadline/budget/hooks Arc）
原样继承（`child_ctx` 同款克隆，`subagent.rs:227-243`）。A-3 验收相应调整：与
模型直调的 gate 决策序列等价、subagent 标记不同。

### 8.4 审计与成本（FR-10）

- 外层 `ToolCallRecord`（`agent.rs:145`）：`input` 含完整代码文本；
- 内部调用不进 records（扁平追加会污染 plan 派生），紧凑 trace 挂
  `ToolOutput.meta`（`task` 的 `trace_meta` 先例，`subagent.rs:360`），
  **v0.2 增补取证字段**（红队 中-5：运行时动态构造的参数只活在事件流里，
  result.json 看到的是生成器代码；v0.5 中-3/中-5 强化：双哈希 + sha256 截
  128bit——fnv64 生日界 2^32 不足以支撑对抗性取证）：

```json
{ "calls": [ {"name":"mcp_sql_query","ok":true,"ms":120,
              "in_len":842,"in_hash":"sha256:9f3a…","in_hash_eff":"sha256:77c1…"}, … ],
  "call_count": 37, "call_limit": 50, "fuel_used": 123456,
  "logs_dropped": 0, "truncated": false }
// in_len + 双 128bit 哈希：in_hash = guest 发出的原始输入（审计链口径——答"代码
// 要什么"，与 ToolCallRecord 保留原始输入对齐）；in_hash_eff = hooks Modify 后
// 实际执行的输入（答"实际发生了什么"；未 Modify 时两值恒等，省略不冗余存储）。
// 明文口径：事件流本就带完整明文（ToolCall 事件的 input 字段）——"不落明文"
// 只对 result.json 成立（防其二次泄密），不是对事件流的声明。
// logs_dropped 口径：只计**超宿主日志上限而未收集**的行；D3 截断（拼装结果时
// 超过 max_result_chars）丢弃的已收集行**不**计入——那是 truncated:true 的信号。
```

- usage：内部调用的 `ToolOutput.usage` 折进 run_code 自身 ToolOutput.usage
  （fold_usage 路径、saturating 纪律）；预算闸门语义不变（child 自记直接花费、
  工具账单不进计数器——`ToolCtx.budget` 文档口径，已核实无 double count）。

### 8.5 历史与会话语义（NFR-4）

| 维度 | 语义 |
|---|---|
| messages | 仅外层 1 对 tool_use/tool_result；内部调用零消息块 |
| 压缩 / plan / resume | run_code 结果即一条普通 tool_result；`extract_plan`（`agent.rs:183`）只扫 records 的 todo 条目——内部调用不进 records，天然不受扰；零新会话状态 |

### 8.6 开关与开启 API（v0.3 新增，D15）

**两层开关，均默认关闭**：

1. **编译期**：`code` cargo feature（默认 `[]`，体积闸，不变）；
2. **运行期**：`CodeConfig.enabled`，**默认 false**（D15）——feature 编入但未开启时，
   run_code 不进工具表与 tool_defs，模型不可见、零 schema 成本。

**开启面（三层 API，供第三方按集成方式选用）**：

| 层 | API | 适用 | 要点 |
|---|---|---|---|
| 配置 | `"code": {"enabled": true, …}` | 二进制用户（headless/TUI）、batch/评测 | 静态装配期生效；partial config 同款字段（PartialConfig） |
| Rust 库 | `cfg.code = CodeConfig{…}`（装配期，全部字段）+ `Agent::set_code_config(&mut self, cfg: CodeConfig)` + 便捷 `set_code_enabled(bool)` | 嵌入 TinyAgent 的第三方 agent（宿主） | **热更新面方法**，对齐 `set_model`/`add_mcp_server` 惯例：`&mut self` 签名即顺序契约（run 持借用，轮次间调用由借用检查器强制）。`set_code_config` **整包原子替换**（开关 + 全部限流/路径旋钮，FR-9/FR-11 全量）；enabled 翻转时同步工具表（push/remove 预构造实例 + 撞名检查 + `refresh_tool_defs`，add/remove_mcp_server 同款重建路径）；限流参数经共享配置只影响**下一次**执行 |
| 绑定 | `agent.enable_code()` / `agent.disable_code()`（快捷）+ `agent.set_code_config({...})`（整包 dict，与 Rust 同语义） | Python/Node 宿主 | `with_agent` 锁内调对应方法；run 期间返回 busy 错误（与既有同步方法同款）；**绑定 crate 新增 `code` 转发 feature（默认关）**——未编入时方法返回明确错误（"rebuilt with code feature"），避免给所有 wheel 强加 ~10MB |

**实现载体（v0.5 H3/F-1 修订）**：`Agent` 新增私有字段 `#[cfg(feature = "code")]
code_tool: Option<RunCodeTool>`——assemble（持有全部四件套）在 feature on 时
**总是构造唯一实例**（构造便宜：仅四件套克隆，引擎是进程级 OnceLock），经
`Agent::set_code_tool` **后置安装**（**仿 `set_hooks` 模式，`try_new` 签名不动**
——避免 feature-gated pub 参数反模式，评审 F-1 与架构 M2 的更优解合并采纳；
两个 `build_agent_with*` 调用点零签名变更）。`enabled=true`（静态路径）等价于
装配期就调了 enable。开关态**不进 session 文件**（与 hooks 同款"层活在实例上"
口径，`--resume` 后按 config 重新决定）。
**运行期调参载体（v0.4；v0.5 L1 重写论证 + H2 钉不变量）**：CodeConfig 存
`Arc<RwLock<CodeConfig>>`——Agent 的 `set_code_config` 与 RunCodeTool 读同一份。
**选 RwLock 的真正理由是零 downcast 纪律**：Agent 无法向 `Vec<Box<dyn Tool>>`
里的工具单播更新（除非工具自身持共享句柄），共享 Arc 是最简句柄；"每次执行
入口读一次快照、进行中执行不受轮次间修改影响"只是顺带的测试友好性质，不是
选型依据。两条纪律：① **读锁即 clone 即放**——锁内只做 `CodeConfig` clone、
绝不持锁执行 guest（`set_code_config` 不会被一次长执行卡住）；② **poison
容忍**——读端 `unwrap_or_else(PoisonError::into_inner)`（`batch.rs:546` 先例；
release `panic=abort` 下 poison 不可达，debug/测试可达，拿旧值继续优于硬错）。
**不变量（H2）**：该 Arc 只达 Agent 与 RunCodeTool 两处，**不向绑定/第三方
暴露**——绑定的 `set_code_config` 走 `with_agent` 锁内调 Agent 方法（dict 按值
转成 CodeConfig），不递 Arc。

## 9. 配置面（FR-11；v0.2 按 H-3/L-3 重写）

```rust
#[serde(default, deny_unknown_fields)]
pub struct CodeConfig {
    pub enabled: bool,               // default **false**（v0.3 D15：默认关闭，三层 API 显式开启，见 §8.6）
    pub deny_paths: Vec<String>,     // 默认见 §7
    pub fs_write_dirs: Vec<String>,  // default [] = 全 workdir 可写
    pub max_tool_calls: u32,         // default 200（AppWorld 高循环题实证：金标 91–244 次调用，50 装不下两阶段打法）
    pub mem_mb: u32,                 // default 256
    pub timeout_secs: u64,           // default 120；0 = 无墙钟（受 cancel/fuel/mem 约束）
    pub fuel: u64,                   // default 5e9（v0.4 暴露；v2 P2 A/B：2.5 MB readAll 超 1e9，5e9 覆盖整读场景）
    pub max_result_chars: usize,     // default 60_000（字符口径，对齐 max_output_chars 量级）
}
```

**batch 接入（H-3 修订）**：`Option<CodeConfig>` 整体替换会让不可信片段用默认
deny_paths 顶掉 base 自定义值——必须照 `PartialLimits` 先例引入
`PartialCodeConfig`（子字段全 Option + `deny_unknown_fields` + `Config::apply`
内**穷尽解构**逐字段策略）：

| 字段 | 片段覆盖策略 |
|---|---|
| enabled | **片段出现即拒**（bad_task；不可信输入不得开关本工具） |
| timeout_secs | `Clamp::TightenOrInit`（0=无上限是最松，同 limits.timeout_secs 口径） |
| max_tool_calls / mem_mb / max_result_chars / fuel | `Clamp::Tighten`（plain min，0=最紧） |
| deny_paths | **并集**（只能更严） |
| fs_write_dirs | **空 vec 恒等于"全 workdir 可写"（最松值）——语义不随来自 default 还是显式配置而翻转（v0.5 一致性中-6）**。合并 = 以全集为单位元的交集：任一侧空 → 取另一侧；两侧非空 → 交集（**交集可为空 = 收紧到无处可写，合法终值**）。测试向量：默认空 + 片段 ["out"] → ["out"]；base ["out"] + 片段 ["tmp"] → [] |

`forbidden_batch_override` 解构里 `code` 归**软字段**（子字段策略已如上逐项钉死）。

## 10. 资产与构建（D5）

- `assets/quickjs.wasm`：quickjs-ng（wasip1 目标，libc 最小化、stdio/network
  桩为不可达）+ §5.3 prelude 打包；平台无关**单份**；
- `examples/build-quickjs.sh`：维护者升级时运行，产物入库；README 记录
  quickjs-ng 版本 + sha256；
- 嵌入：`include_bytes!`，`#[cfg(feature = "code")]` 门住；
- Cargo：`wasmtime`（optional，default-features = false，features
  ["cranelift","runtime","std"]；**不开 async**——无 tokio）；`code = ["dep:wasmtime","dep:sha2"]`（sha2：审计双哈希，0.10 已在依赖树内——零新增）。
- **musl 交叉验证前置到 M1 出口判据**（M-5 修订）：wasmtime 纯 Rust/cranelift
  理论可行，别等 CI 才发现。**M1 已通过**：`cargo build --target
  x86_64-unknown-linux-musl --features code` 产出静态 ELF。本地交叉用 zig cc 当
  linker（wrapper 重写三元组 `x86_64-unknown-linux-musl`→`x86_64-linux-musl`，且
  **滤掉 rust 自带的 self-contained crt 对象**——否则 zig 的 crt1.o 与 rust 的
  rcrt1.o 撞 `_start` 重复符号；CI 的 musl job 用官方 musl 工具链，无此交互）。
  **体积数（M2 接线后实测）**：同一棵树的 musl release 产物，无 feature
  2,841,328 B（2.71 MiB）vs `--features code` 7,658,584 B（7.31 MiB）——**差值
  4,817,256 B ≈ 4.59 MiB**，其中 ~0.96 MiB 是内嵌的 quickjs.wasm 资产
  （1,001,495 B），其余 ≈3.6 MiB 是 wasmtime+cranelift 在 opt-level="z"+LTO+strip
  下的机器码。显著低于 ~10MB 估算（该估算按默认 opt-level 的 wasmtime 体量打）。
  M1 时观察到的 ~47 KiB 差值确系 linker GC（构造点在 assemble，非测试二进制尚无
  引用）；顺带勘误：M1 用 `nm` 零 wasmtime 符号佐证——但 release profile 本就
  `strip = true`，nm 对 stripped ELF 恒零符号，该证据方法无效（尺寸差值才是有效
  证据，M1/M2 均以尺寸为准）。
- 发布产物拍板（M-5 v0.3 初定"不带"；**2026-09 复议改带**）：运行期默认关闭（D15）
  已把安全问题从产物决策中解耦——编入 feature 但未开启 = 零暴露、仅体积成本
  （M2 实测 ≈4.6 MiB）。§10 A/B 指标两批验证完成后（PRD），产品方拍板
  **release.yml 的 musl 产物带上 `--features code`**（开关默认关，config 显式开启
  才暴露）；自建用户去 feature 可回到 ≈2.7 MiB。README 发布节同步。

## 11. 安全模型（NFR-1，五层，v0.2 诚实化）

1. **构造性零权限**：无 WASI/网络/env/时钟，离开沙箱的唯一指令路径是两个 import（host_call_tool / host_log）；
   prelude 能力面收口（§5.3）；
2. **guest 可调集最小化**（§2）：run_code/task/todo 构造性不可达（防递归 DoS 与
   成本炸弹）；可调集 = 白名单叶子 + MCP；**工具本身默认关闭**（D15：enabled
   默认 false，开启是显式信任决策——被注入模型默认不获得"循环+代码执行"能力）；
3. **文件访问 ≡ 既有工具授权** + deny_paths/fs_write_dirs 拦截层（§7）——
   **定位诚实**：防误读不防恶意（shell 旁路明示，A-8 钉住）；
4. **hooks 策略面 + 审计面**：内部调用全量过闸（可按 subagent="__code__" 单独收紧）、
   代码全文 + 逐调用摘要哈希入档（§8.4）；**文档化盲区**：无 hooks 部署下 guest
   的 50 次调用零门禁（与模型直调同面、非新增）；
5. **限流器**（§6）：fuel/墙钟三执行点/调用数/内存/日志，四来源标志可审计。

边界声明：workdir 沙盒 best-effort（TOCTOU 既有口径；guest 顺序执行、无并发原语，
不扩大窗口——红队已核实）；带网络 shell 的 agent 无法进程内根治 exfil，
不可信环境正解是部署层隔离。

## 12. 代码接入点清单（v0.2 更新）

| 文件 | 变更 |
|---|---|
| `Cargo.toml` | + `code` feature、wasmtime optional |
| `src/config.rs` | + `CodeConfig`/`PartialCodeConfig`、`Config.code`、PartialConfig 穷尽解构补字段、apply 内逐字段策略（§9）、`forbidden_batch_override` code 归软、`default_tools()` **不含 run_code**（H4）+ 显式点名未开启报错（feature off 走 actionable 未知名文案） |
| `src/tools/run_code.rs` | 新建（四件套 + 内部注册表 + 拦截层 + 限流 + eval） |
| `src/tools/subagent.rs` | task 的 name 参数最小校验：非空、≤64、不以 `__` 开头（红队低-6：消解与 `__code__` hooks 标记撞名） |
| `src/tools/mod.rs` | `mod run_code`（feature 门）；build() `"run_code"` 分支；`validate_tool_names` + `__tools_list` 保留名（feature 门）；底层原语 `gate_tool_call`/`notify_tool_after` 落位（或 agent.rs） |
| `src/agent.rs` | M0：`gate_tool`(:765)/`notify_after`(:775) 降为自由函数、顺序遍改用 `execute_tool_call`、并行遍改用原语（行为不变）；`AgentEvent` + `CodeLog`；**+ `code_tool` 字段（feature 门）+ `set_code_tool` 安装点（assemble 调，仿 set_hooks）+ `set_code_config`/`set_code_enabled` 热更新面方法（§8.6）** |
| `src/hooks.rs` | 文档 Scope 节补 tools.list 绕过说明 |
| `src/harness.rs` | CodeLog 两个 frontend（stderr / NDJSON） |
| `src/tui.rs` | CodeLog 系统行渲染 |
| `src/lib.rs` | **v0.3 不再零改动**：assemble 在 feature on 时构造**唯一** RunCodeTool 实例、白名单剔除 run_code 后经 `Agent::set_code_tool` 后置安装（§8.6 载体，v0.5 H3/F-1：try_new 签名不动、build_agent_with* 调用点零变更）；enabled=true 时注入工具表 |
| `assets/quickjs.wasm` + `examples/build-quickjs.sh` | 新增 |
| `README.md` / `CLAUDE.md` | 工具文档、deny_paths 诚实边界、timeout 软上限口径、feature 说明、默认白名单标注（对齐 task） |
| `bindings/*` | **v0.3 不再零改动**：+ `enable_code()`/`disable_code()`/`set_code_config(dict)` 方法（v0.5 一致性中-4 回填）、+ `code` 转发 feature（默认关，未编入时方法返回明确错误）；ToolCtx/ToolOutput 仍零字段变更（两绑定不构造 ToolCtx——已核实） |

## 13. 测试计划（NFR-5；v0.2 增补）

`agent_tests.rs`（进程内，mock provider + 嵌入资产真实执行）：

1. 最小闭环（A-1）；2. CSV 场景（A-2）；3. hooks：CaptureHooks 序列 + Deny →
   throw + subagent="__code__" 标记 + **Modify 换执行留原始审计**（v0.2 补齐）
   + **双哈希对证（in_hash 原始 / in_hash_eff 实际，中-3）+ task name `__`
   前缀被拒不撞标记（低-6）**；
4. 限流：死循环 fuel / 墙钟超时 / max_tool_calls，各自文案与标志（A-4）。**M3 实测注记**：墙钟用例必须（a）先暖机（冷进程首次执行承担模块懒编译 ~10s，wasmtime 按模块缓存机器码、热路径毫秒级）、（b）把 fuel 调离竞速窗口（紧循环燃料 ~1e9/s，默认 1e9 与 1s 墙钟几乎同时到期、fuel 先赢裁决）；
5. 取消（A-5；验收措辞含"在飞调用不可打断"边界）。**M3 实测注记**：cancel 翻转线程必须在暖机之后 spawn，否则编译等待期间 flag 已置、入口闸（⓪）代替在飞投递完成工作——测的是另一条路径；另有 **deadline 已过 / cancel 已置的入口闸用例**（亚 tick 平凡程序在窗口内正常完成，闸保证"已过期就不进 guest"语义可测且确定性）；
6. 路径逃逸 + deny_paths 命中（A-7）+ **A-8：shell 旁路 deny_paths 不拦（预期
   行为钉住）** + **v0.5 向量组：悬空 symlink 写被拒（中-1）、
   restrict_to_workdir=false 时 workdir 外路径读放行写恒拒（中-2）、glob 语义
   （`**/` 零段命中根 / 无 `/` 模式仅根第一段 / 嵌套 `**/.aws/**` 命中）
   （低-3）**；
7. **guest 调 run_code/task/todo → unknown tool（D11 钉住）**；
8. **封套语义：工具输出恰含 `__error`/`__ok` 键时 success 不被误判（D12）**；
9. **image 输出 → 显式错误（§5.1）**（M2 已钉 `image_output_is_an_explicit_error`；原掉在里程碑缝隙，评审补位）；
10. **tools.list：不含 run_code/task/todo、描述截断**（+ v0.5 低-1：超限 guest
    不可绕开审计封顶。latch 半段 M2 已钉 `halted_sandbox_refuses_even_tools_list`；
    墙钟半段 M3 落地时**修正可达性**：字面读法「墙钟耗尽后 guest 再调
    `__tools_list` 被拒」按构造不可达——epoch 陷阱在下一个 dispatch 回边就杀掉
    guest，先于它发出下一次调用；`host_call_tool` 入口闸是亚 tick 窗口的纵深
    防御。改钉可达语义 `wall_clock_fires_around_a_long_in_flight_tool_call`：
    在飞调用越墙钟 → 调用本身跑完入审计（诚实边界）+ 其后首个检查点以墙钟
    文案终止、不得静默成功）；
11. 截断（D3）与日志 dropped 计数；
12. **PartialCodeConfig batch 策略逐字段（§9 表）**；
13. enabled=false / feature off 显式列出报错；
14. 授权等价：同一 hooks 下"模型直调 read_file"与"guest 内调"gate 序列等价；
15. 零漂移：feature off 全量既有测试不变；
16. **开关与调参（A-10a/b/c，v0.5 H5 按里程碑拆分）**——
    **16a（M2，A-10a）**：enabled=false 默认时 tool_defs 无 run_code（模型不可见）；
    config 静态开启后可见可调；`set_code_enabled` 运行期开启/关闭往返（含
    refresh_tool_defs 后模型下一轮即见/不见）；显式列 run_code 未启用报错带
    开启提示。**16b（M3，A-10b）**：`set_code_config` 运行期收紧
    max_tool_calls/fuel 后下一次执行按新参数生效（进行中执行不受影响）。
    **16c（M4，A-10c）**：绑定 `enable_code`/`set_code_config` 在 run 期间返回
    busy。
17. **JS 异常路径（A-11，FR-12——v0.5 一致性中-5 补：P0 此前无测试无验收编号）**：
    guest throw → 外层 is_error 结果含异常 message + stack + 已收集日志 +
    已完成调用数；四来源（cancel/deadline/fuel/mem）错误文案互相可区分。

CI 门禁（factcheck ⚠️ 修正，写明三条）：
`cargo clippy --all-targets`、`cargo clippy --features tui --all-targets`、
`cargo clippy --features code --all-targets`（均 `-D warnings`）；
musl 冒烟 + code 各一；cargo-audit；fmt 照旧。绑定回归照惯例全跑。

## 14. 风险与缓解

（v0.1 表维持，增补：）
| 风险 | 缓解 |
|---|---|
| 内部注册表四件套克隆的内存/构造开销 | 惰性构建 + 仅 clone 引用型字段（client/Arc MCP）；实测进 M1 出口 |
| wasmtime 再入 alloc 写结果遇 fuel/epoch trap | 写入路径容忍 trap、按超限收尾（§5.1 注记） |
| A/B 效率指标（SRS §10）依赖发布产物带 code | v1 本地/自建验证；产物拍板开放问题跟踪 |

## 15. 实现里程碑（评审通过后逐 commit）

| # | 内容 | 出口判据 |
|---|---|---|
| M0 | 两层 dispatch 抽取纯重构（§4） | 既有测试全绿、行为零变化、diff 仅搬运 |
| M1 | feature + 资产 + run_code 骨架 + prelude/封套 ABI（§2/§5/§8.1） | A-1 通过 + **musl 交叉编译冒烟通过** + fuel 基准回填 |
| M2 | 内部注册表 + hooks/subagent 标记 + deny_paths 拦截层 + 审计 meta + **开关面（enabled 默认关 + set_code_config/set_code_enabled API，§8.6）**（勘误评审 arch-4：原注「调参生效判据在 M3——限流器此期才存在」与事实不符——fuel/mem/call limit 自 M1 起已 enforce（M1 出口判据的 fuel 基准本身要求它存在），A-10b 无须按此推迟，随 M3 落地） | A-2/A-3/A-7/A-8/**A-10a** + 测试 7/8/10/14/16a 通过 |
| M3 | 墙钟/取消 + CodeLog 事件化 + PartialCodeConfig（§6/§8.2/§9；host 侧日志帽与 logs_dropped 已随 M2 评审提前落地——可调用即有 host OOM 窗口，不能等）。**落地实测修订**：中断投递载体为回调返 `Err`（`UpdateDeadline` 无 Trap 变体）、入口闸 ⓪ 补齐（§6 epoch 注记）、`max_wasm_stack` 1536→384 KiB（深递归竞速，§6 四来源注记） | A-4/A-5/**A-10b** + 测试 4/5/11/12/13/16b/17 + 全量回归绿 |
| M4 | 文档（README/CLAUDE.md/hooks.rs Scope）+ **绑定 enable_code/disable_code/set_code_config 与转发 feature** + bindings 回归。**落地注记**：hooks.rs Scope（`__code__` 标记与 `tools.list()` 旁路口径）随 M3 已落；README 增 run_code 专节（两层开关/字段表/诚实边界/与安全说明及 Non-goals 的关系修订）+ 绑定节 + 开发命令；CLAUDE.md 增三 clippy 配置口径 + run_code 架构条目；CI 增 code job（clippy+test）、musl code 冒烟、双绑定 `--features code` clippy 编译门；绑定三方法 build-自适应测试（探测 wheel 是否带 feature，另一侧 skip——行为断言只在 `--features code` 构建上真跑，默认构建钉 hint 路径） | CI 全绿（含 code 变体三 clippy）+ **A-10c（绑定 busy）** |

---

## 附录 A：LLM 可见契约（即规格，实现照抄；v2 P2 起拆两处——瘦描述常驻 schema，内置 skill run-code 按需加载）

**① 工具描述**（随每个请求的工具 schema 常驻；≤1200 字符上限由 `slim_description_points_at_the_skill_and_keeps_helper_list_closed` 测试钉住）：

```json
{
  "name": "run_code",
  "description": "在进程内 JS 沙箱（QuickJS，纯同步 ES2023）中执行代码：适合循环 / 批量 / 多步数据处理 / 大文件加工，一次调用完成本来要几十轮 read_file+shell 折腾的编排。单个工具调用就能完成的事不要用本工具；一条 shell 管道能做的优先 shell；需要在自己回复里记住大量中间数据才能完成的，就该用代码。\n\n【环境】无 async/await、Promise、timer、import/fetch；无网络与文件系统直访（读写经工具调用）；Date.now 与 Math.random 不可用；工具调用次数（每次执行独立计满，超了分批续跑）/ 内存 / 执行时长有上限，超限强制终止。\n\n【API】\n- 内置助手（前导函数，直接可用；此外没有别的内置助手）：readAll(path) 整读文本（约 200 万字符内一次读完）、readLines(path) 行数组、writeVerified(path, content) 字节保真写（写后读回校验）\n- tools.call(工具名, 参数对象)：调用本会话可用工具（含 MCP；不含 task/todo/run_code 自身），失败 throw 可 try/catch；返回值能解析为 JSON 则为对象/数组。失败的调用（含写错工具名）也扣调用额度\n- tools.list()：调 MCP 工具前先调它核对准确名字——MCP 工具名长、分隔符易混（`_` 与 `__`），凭记忆写错会白烧额度；console.log(...)：调试输出，实时显示并随结果返回\n- tools.budget()：查剩余额度（次数/墙钟/燃料/token）——长批量前查、循环里定期查\n- copy_bytes(src,dst)（经 tools.call）：host 流式字节保真复制文件（不经 JS 字符串）\n- 工具错误可 catch 且带 e.kind（denied/hooks/budget/deadline/unknown_tool/tool）；retryable=false 类重试无意义\n\n【返回】return 的值经 JSON 序列化作为工具结果。必须返回紧凑摘要（统计数字、文件路径、少量样例行）；大数据写文件后返回路径，禁止把数据本体放进返回值。\n\n【数值】Number 是 float64：整数乘法/累加一超过 2^53 即静默失真。移植 Python 等语言的整数算术（计数器、LCG、哈希、大数）一律用 BigInt 字面量（123456789n、2n**31n），详见 run-code 技能。\n\nread_file 三种模式（text / format:\"json\" / raw）、字节保真复制、分页细节与完整示例：用 read_skill 加载技能 run-code——写非平凡代码前先读它。",
  "input_schema": {
    "type": "object",
    "properties": {
      "code": {
        "type": "string",
        "description": "完整 JS 源码；return 的值作为结果"
      }
    },
    "required": [
      "code"
    ]
  }
}
```

**② 内置 skill `run-code`**（`src/tools/run_code_skill.md`，`include_str!` 进二进制；`code.enabled` 时经 `skills::embedded_entry` + `insert_builtin` 注入，磁盘同名 skill 覆盖它；模型经 `read_skill` 按需读入、作为可压缩的 tool_result 而非常驻 schema；以下为文件全文，改源文件后须同步再生成本附录）：

````markdown
---
name: run-code
description: run_code 沙箱完整用法——read_file 三种模式、copy_bytes 字节保真复制、大文件整读/分页聚合、批量校验闭环、返回值纪律与典型示例
allowed-tools: run_code read_file write_file
---

# run_code 沙箱完整用法

## 环境
- 纯同步 JS（QuickJS，ES2023）：无 async/await、Promise、setTimeout、import/require、fetch
- 无网络、无系统调用；读写文件只能经工具调用（read_file / write_file / edit_file / list_dir，工作目录内相对路径）
- Date.now 与 Math.random 不可用（评测可复现）
- 资源上限：工具调用次数、内存、执行时长、fuel——任一超限即强制终止；已写盘的文件保留
- **调用额度按次独立**：每次 run_code 调用独立计数（默认每次 200 次），超限只终止本次执行，下次 run_code 重新计满。任务超过单次额度就**分批**：每批 ≤200 次调用，中间结果 `write_file` 落盘，下一批读回来接着干（先摸元数据再批量写的两阶段任务尤其要预算好两阶段各占多少额度）

## 内置助手（前导函数，代码里直接可用；此外没有别的内置助手）
- `readAll(path)` → 整个文件的文本。自动分页拼装，无行号前缀、无元数据头。沙箱内单次读取上限约 200 万字符——常见数据文件（几 MB 文本）一次读完，不用自己分页
- `readLines(path)` → 整个文件的行数组（`split("\n")` 语义：文件以换行结尾时末尾有一个空元素，过滤掉即可）
- `writeVerified(path, content)` → 写入文本并逐字节读回校验（防截断/转写意外），不一致会 throw。重要的文本产出建议走它
- `tools.budget()` → 剩余额度 `{calls_used, max_tool_calls, calls_left, wall_left_ms, wall_bound_by, fuel_left}`，查询本身免费。长批量开工前先查、循环里定期查，别等撞限才发现
- 其余能力一律 `tools.call`；**调 MCP 工具前先 `tools.list()` 核对准确名字**——MCP 工具名长、分隔符易混（`_` 与 `__`），写错名字的失败调用也照扣额度，一批全错就是额度白烧。`tools.list()` 返回**对象数组** `[{name, description}]`（不是字符串数组），取名字用 `.map(t => t.name)`
- 工具错误可 try/catch，异常带结构化字段：`e.kind`（`denied`=路径/权限拒绝、`hooks`=钩子拒绝、`budget`=调用额度、`deadline`=墙钟、`unknown_tool`=写错名字、`tool`=工具自身失败）、`e.tool`（调用名）。`e.retryable===false` 的类别重试无意义（改输入或跳过）；`tool` 类**不带** retryable（无法机判，读 message 自行判断）

## read_file 的三种模式（tools.call 直调时按需选）
1. **文本模式**（默认）：首行 `[文件名: 总行数 · 总字符 · complete|行区间 a-b]` 元数据，其后每行 `行号→tab` 前缀；带 limit 截断时末尾多一行 `... [line limit reached]`。需要解析内容时优先 readAll / readLines 或 json 模式，**不要手解析这个格式**
2. **format:"json"**：返回 `{content, first_line, lines_total, chars_total, truncated, truncation, next_offset}`——content 无前缀。只有 limit 截断（行对齐）给非空 next_offset，循环传 `{offset: next_offset, limit, format:"json"}` 续读直到 null；chars/bytes 截断时 next_offset 为 null（窗口可能切在行中间，无法线对齐续读），此时改用 raw
3. **raw:true**：按字节窗口返回 base64：`{content_b64, offset, bytes_read, bytes_total, truncated, next_offset}`——offset/limit 是**字节数**，默认窗口约 1.4 MB 且按 3 字节对齐；二进制 / 编码敏感内容的唯一保真通道

## 字节保真复制（跨窗搬运二进制）
**优先 `copy_bytes`**（经 tools.call，host 流式搬运、内容不经过 JS 字符串，二进制/非 UTF-8/大文件均逐字节一致）：
```js
const r = tools.call("copy_bytes", { src: "src.bin", dst: "out/dst.bin" });
return { copied: r.copied_bytes, file: "out/dst.bin" };
```
需要边复制边变换时才用手工配方。默认窗口按 3 字节对齐 ⇒ 各窗 content_b64 直接字符串拼接 == 整个文件的 base64：
```js
let off = 0, parts = [];
while (true) {
  const w = tools.call("read_file", { path: "src.bin", raw: true, offset: off });
  parts.push(w.content_b64);
  if (!w.truncated) break;
  off = w.next_offset;
}
tools.call("write_file", { path: "dst.bin", content_b64: parts.join("") });
return { copied: "dst.bin" };
```
write_file 的 `content`（文本）与 `content_b64`（base64 字节）二选一，不可同传。

## 批量操作纪律（计划—执行—校验闭环）
多删、漏删、漏页几乎都源于"没数数就动手"。批量任务（批量删除/修改/点赞/导出…）固定走这个模板，**数量不一致就显式 throw**，不要返回"处理完成"：
```js
// 1) 先收集并去重目标，记录目标数
const seen = new Set(), targets = [];
for (let page = 1; ; page++) {              // 2) 分页拉全：显式传 limit，拉到空页为止
  const r = tools.call("mcp_list_items", { page: page, limit: 100 });
  if (!r.items || r.items.length === 0) break;
  for (const it of r.items)
    if (!seen.has(it.id)) { seen.add(it.id); targets.push(it); }
}
// 3) 执行，逐条记账（成功/失败分开）
const failed = [];
for (const t of targets) {
  try { tools.call("mcp_delete", { id: t.id }); }
  catch (e) { if (e.kind === "denied" || e.kind === "unknown_tool") throw e; failed.push({ id: t.id, why: String(e.message) }); }
}
// 4) 校验闭环：目标数 == 成功数 + 失败数，缺一不可
const okCount = targets.length - failed.length;
if (okCount + failed.length !== targets.length) throw new Error("audit mismatch");
// 5) 返回摘要（目标/成功/失败），失败清单给样例
return { targets: targets.length, ok: okCount, failed: failed.length,
         failed_sample: failed.slice(0, 5).map(f => f.id) };
```
要点：目标集合先**去重**（跨源/跨页会重复）；分页**显式传 limit**（默认页大小小得离谱——曾实测默认 5 漏掉第 6 条）；每批开工前 `tools.budget()` 对一下额度装不装得下，装不下就分批落盘续跑。

## 返回值纪律
代码作为函数体执行，`return` 的值经 JSON 序列化作为工具结果。必须返回**紧凑摘要**（统计数字、文件路径、少量样例行）；大数据写文件后返回路径引用，禁止把数据本体放进返回值——返回值与 console 日志共享尺寸上限，超限时截日志保返回值，数据本体照样会被截。

## 典型模式
### Python 等语言整数算术移植（必读）
JS 的 Number 是 float64：整数乘法/累加超过 2^53（约 9×10^15）即静默丢低位——不报错、结果看着像对的。从 Python 翻译计数器/伪随机/哈希/大数运算时，状态量一律 BigInt：字面量加 n、常量同步改（1103515245n、2n**31n）、与 Number 混算前显式转换（BigInt(i) / Number(small)）；循环下标等小数值仍用 Number。拿不准就 String(x) 抽样看量级。
```js
let state = 123456789n;
for (let i = 0; i < 100000; i++) {
  state = (state * 1103515245n + 12345n) % 2n ** 31n;
}
return { last: state.toString() };
```
### 聚合工具返回的分页数据
```js
const page = tools.call("mcp_sql_query", { sql: "SELECT name, amount FROM orders LIMIT 5000" });
const total = {};
for (const row of page.rows) total[row.name] = (total[row.name] ?? 0) + row.amount;
const lines = ["name,total", ...Object.entries(total).map(([n, v]) => `${n},${v}`)];
tools.call("write_file", { path: "out/totals.csv", content: lines.join("\n") });
return { file: "out/totals.csv", customers: Object.keys(total).length };
```
### 大文件统计（readAll 整读后聚合，只 return 摘要）
```js
const counts = {};
for (const line of readAll("data/logs.txt").split("\n")) {
  const m = line.match(/ERROR (\w+)/);
  if (m) counts[m[1]] = (counts[m[1]] ?? 0) + 1;
}
return { error_kinds: counts };
```
### 批量文件操作（list_dir 是纯文本行，目录带尾 `/`；逐项 try/catch 跳过失败）
```js
const names = tools.call("list_dir", { path: "inputs" }).split("\n")
  .filter(n => n && !n.endsWith("/") && n.endsWith(".json"));
const bad = [];
for (const n of names) {
  try {
    const v = JSON.parse(readAll("inputs/" + n));
    if (!v.id) throw new Error("missing id");
  } catch (e) { bad.push({ file: n, why: String(e) }); }
}
return { checked: names.length, bad };
```

## 何时用 / 何时不用
- **用**：数据装不进对话（大文件 readAll 后聚合/过滤/统计，只 return 摘要——不要自己逐页读进对话再心算）；几十个文件的批量校验/改写/重组；跨文件关联（join）、按规则分组、JSON 嵌套处理（代码经 JSON 直传，无 shell 转义烦恼）
- **不用**：单个工具调用就能完成的事（读/写一个文件、跑一条命令）；十来个小文件的简单同构修改（直接多次工具调用更省更稳）；一条 shell 管道能做的（排序/去重/求和/单遍过滤——shell 可用时优先 shell）
- 判断口诀：如果需要在自己回复里记住大量中间数据才能完成，就该用代码
- 可能失败的外部调用用 try/catch 包住，失败时 return 已完成部分的进度摘要
---
name: run-code
description: run_code 沙箱完整用法——read_file 三种模式、copy_bytes 字节保真复制、大文件整读/分页聚合、批量校验闭环、返回值纪律与典型示例
allowed-tools: run_code read_file write_file
---

# run_code 沙箱完整用法

## 环境
- 纯同步 JS（QuickJS，ES2023）：无 async/await、Promise、setTimeout、import/require、fetch
- 无网络、无系统调用；读写文件只能经工具调用（read_file / write_file / edit_file / list_dir，工作目录内相对路径）
- Date.now 与 Math.random 不可用（评测可复现）
- 资源上限：工具调用次数、内存、执行时长、fuel——任一超限即强制终止；已写盘的文件保留
- **调用额度按次独立**：每次 run_code 调用独立计数（默认每次 200 次），超限只终止本次执行，下次 run_code 重新计满。任务超过单次额度就**分批**：每批 ≤200 次调用，中间结果 `write_file` 落盘，下一批读回来接着干（先摸元数据再批量写的两阶段任务尤其要预算好两阶段各占多少额度）

## 内置助手（前导函数，代码里直接可用；此外没有别的内置助手）
- `readAll(path)` → 整个文件的文本。自动分页拼装，无行号前缀、无元数据头。沙箱内单次读取上限约 200 万字符——常见数据文件（几 MB 文本）一次读完，不用自己分页
- `readLines(path)` → 整个文件的行数组（`split("\n")` 语义：文件以换行结尾时末尾有一个空元素，过滤掉即可）
- `writeVerified(path, content)` → 写入文本并逐字节读回校验（防截断/转写意外），不一致会 throw。重要的文本产出建议走它
- `tools.budget()` → 剩余额度 `{calls_used, max_tool_calls, calls_left, wall_left_ms, wall_bound_by, fuel_left, tokens_left}`，查询本身免费。`tokens_left` 是 run 级 token 预算剩余（未设 `limits.max_total_tokens` 时为 null）。长批量开工前先查、循环里定期查，别等撞限才发现
- 其余能力一律 `tools.call`；**调 MCP 工具前先 `tools.list()` 核对准确名字**——MCP 工具名长、分隔符易混（`_` 与 `__`），写错名字的失败调用也照扣额度，一批全错就是额度白烧。`tools.list()` 返回**对象数组** `[{name, description}]`（不是字符串数组），取名字用 `.map(t => t.name)`
- 工具错误可 try/catch，异常带结构化字段：`e.kind`（`denied`=路径/权限拒绝、`hooks`=钩子拒绝、`budget`=调用额度、`deadline`=墙钟、`unknown_tool`=写错名字、`tool`=工具自身失败）、`e.tool`（调用名）。`e.retryable===false` 的类别重试无意义（改输入或跳过）；`tool` 类**不带** retryable（无法机判，读 message 自行判断）

## read_file 的三种模式（tools.call 直调时按需选）
1. **文本模式**（默认）：首行 `[文件名: 总行数 · 总字符 · complete|行区间 a-b]` 元数据，其后每行 `行号→tab` 前缀；带 limit 截断时末尾多一行 `... [line limit reached]`。需要解析内容时优先 readAll / readLines 或 json 模式，**不要手解析这个格式**
2. **format:"json"**：返回 `{content, first_line, lines_total, chars_total, truncated, truncation, next_offset}`——content 无前缀。只有 limit 截断（行对齐）给非空 next_offset，循环传 `{offset: next_offset, limit, format:"json"}` 续读直到 null；chars/bytes 截断时 next_offset 为 null（窗口可能切在行中间，无法线对齐续读），此时改用 raw
3. **raw:true**：按字节窗口返回 base64：`{content_b64, offset, bytes_read, bytes_total, truncated, next_offset}`——offset/limit 是**字节数**，默认窗口约 1.4 MB 且按 3 字节对齐；二进制 / 编码敏感内容的唯一保真通道

## 字节保真复制（跨窗搬运二进制）
**优先 `copy_bytes`**（经 tools.call，host 流式搬运、内容不经过 JS 字符串，二进制/非 UTF-8/大文件均逐字节一致）：
```js
const r = tools.call("copy_bytes", { src: "src.bin", dst: "out/dst.bin" });
return { copied: r.copied_bytes, file: "out/dst.bin" };
```
需要边复制边变换时才用手工配方。默认窗口按 3 字节对齐 ⇒ 各窗 content_b64 直接字符串拼接 == 整个文件的 base64：
```js
let off = 0, parts = [];
while (true) {
  const w = tools.call("read_file", { path: "src.bin", raw: true, offset: off });
  parts.push(w.content_b64);
  if (!w.truncated) break;
  off = w.next_offset;
}
tools.call("write_file", { path: "dst.bin", content_b64: parts.join("") });
return { copied: "dst.bin" };
```
write_file 的 `content`（文本）与 `content_b64`（base64 字节）二选一，不可同传。

## 批量操作纪律（计划—执行—校验闭环）
多删、漏删、漏页几乎都源于"没数数就动手"。批量任务（批量删除/修改/点赞/导出…）固定走这个模板，**数量不一致就显式 throw**，不要返回"处理完成"：
```js
// 1) 先收集并去重目标，记录目标数
const seen = new Set(), targets = [];
for (let page = 1; ; page++) {              // 2) 分页拉全：显式传 limit，拉到空页为止
  const r = tools.call("mcp_list_items", { page: page, limit: 100 });
  if (!r.items || r.items.length === 0) break;
  for (const it of r.items)
    if (!seen.has(it.id)) { seen.add(it.id); targets.push(it); }
}
// 3) 执行，逐条记账（成功/失败分开）
const failed = [];
for (const t of targets) {
  try { tools.call("mcp_delete", { id: t.id }); }
  catch (e) { if (e.kind === "denied" || e.kind === "unknown_tool") throw e; failed.push({ id: t.id, why: String(e.message) }); }
}
// 4) 校验闭环：目标数 == 成功数 + 失败数，缺一不可
const okCount = targets.length - failed.length;
if (okCount + failed.length !== targets.length) throw new Error("audit mismatch");
// 5) 返回摘要（目标/成功/失败），失败清单给样例
return { targets: targets.length, ok: okCount, failed: failed.length,
         failed_sample: failed.slice(0, 5).map(f => f.id) };
```
要点：目标集合先**去重**（跨源/跨页会重复）；分页**显式传 limit**（默认页大小小得离谱——曾实测默认 5 漏掉第 6 条）；每批开工前 `tools.budget()` 对一下额度装不装得下，装不下就分批落盘续跑。

## 返回值纪律
代码作为函数体执行，`return` 的值经 JSON 序列化作为工具结果。必须返回**紧凑摘要**（统计数字、文件路径、少量样例行）；大数据写文件后返回路径引用，禁止把数据本体放进返回值——返回值与 console 日志共享尺寸上限，超限时截日志保返回值，数据本体照样会被截。

## 典型模式
### Python 等语言整数算术移植（必读）
JS 的 Number 是 float64：整数乘法/累加超过 2^53（约 9×10^15）即静默丢低位——不报错、结果看着像对的。从 Python 翻译计数器/伪随机/哈希/大数运算时，状态量一律 BigInt：字面量加 n、常量同步改（1103515245n、2n**31n）、与 Number 混算前显式转换（BigInt(i) / Number(small)）；循环下标等小数值仍用 Number。拿不准就 String(x) 抽样看量级。
```js
let state = 123456789n;
for (let i = 0; i < 100000; i++) {
  state = (state * 1103515245n + 12345n) % 2n ** 31n;
}
return { last: state.toString() };
```
### 聚合工具返回的分页数据
```js
const page = tools.call("mcp_sql_query", { sql: "SELECT name, amount FROM orders LIMIT 5000" });
const total = {};
for (const row of page.rows) total[row.name] = (total[row.name] ?? 0) + row.amount;
const lines = ["name,total", ...Object.entries(total).map(([n, v]) => `${n},${v}`)];
tools.call("write_file", { path: "out/totals.csv", content: lines.join("\n") });
return { file: "out/totals.csv", customers: Object.keys(total).length };
```
### 大文件统计（readAll 整读后聚合，只 return 摘要）
```js
const counts = {};
for (const line of readAll("data/logs.txt").split("\n")) {
  const m = line.match(/ERROR (\w+)/);
  if (m) counts[m[1]] = (counts[m[1]] ?? 0) + 1;
}
return { error_kinds: counts };
```
### 批量文件操作（list_dir 是纯文本行，目录带尾 `/`；逐项 try/catch 跳过失败）
```js
const names = tools.call("list_dir", { path: "inputs" }).split("\n")
  .filter(n => n && !n.endsWith("/") && n.endsWith(".json"));
const bad = [];
for (const n of names) {
  try {
    const v = JSON.parse(readAll("inputs/" + n));
    if (!v.id) throw new Error("missing id");
  } catch (e) { bad.push({ file: n, why: String(e) }); }
}
return { checked: names.length, bad };
```

## 何时用 / 何时不用
- **用**：数据装不进对话（大文件 readAll 后聚合/过滤/统计，只 return 摘要——不要自己逐页读进对话再心算）；几十个文件的批量校验/改写/重组；跨文件关联（join）、按规则分组、JSON 嵌套处理（代码经 JSON 直传，无 shell 转义烦恼）
- **不用**：单个工具调用就能完成的事（读/写一个文件、跑一条命令）；十来个小文件的简单同构修改（直接多次工具调用更省更稳）；一条 shell 管道能做的（排序/去重/求和/单遍过滤——shell 可用时优先 shell）
- 判断口诀：如果需要在自己回复里记住大量中间数据才能完成，就该用代码
- 可能失败的外部调用用 try/catch 包住，失败时 return 已完成部分的进度摘要
````
