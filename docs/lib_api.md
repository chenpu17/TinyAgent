# 库 API（程序化调用）

本文件从 README 的「库 API（程序化调用）」一节整体拆出（纯搬移，内容不变）；README 只保留一节摘要与本文链接。

内核是一个独立的 crate（`crates/core`，lib 名 `tiny_agent`），供第三方 Rust 程序——以及 Python（PyO3）/ TypeScript（napi-rs）绑定——直接驱动 agent。二进制（根 package `tiny-agent`）只是这个库之上的一层薄壳（CLI 参数解析、SIGINT、`process::exit`、headless 呈现、批量编排、可选 TUI），行为与库完全一致。

```sh
cargo build --lib                # 只编库
cargo build --lib --features mock # 含脚本化 mock provider，离线 dry-run
```

最小用法（`build_agent` 一把梭：按 config 装 provider + 内置工具 + MCP + Agent）：

```rust,no_run
use tiny_agent::{build_agent, AgentEvent, Config};

let cfg = Config::load(std::path::Path::new("chat.json"))?;
let client = tiny_agent::provider::new_client(cfg.limits.timeout_secs);
let mut agent = build_agent(&cfg, Vec::new(), &client)?;
agent.run(&mut |ev: AgentEvent| if let AgentEvent::TextDelta(t) = ev { print!("{t}"); })?;
```

完整控制（自定义 provider / 自定义工具）：`Agent::new` 直接接收 `Box<dyn Provider>` 与 `Vec<Box<dyn Tool>>`——两者都是 trait object，注入即可，无需改 trait。自定义工具用 `CallbackTool` 零样板包一个闭包：

```rust,no_run
use tiny_agent::{CallbackTool, ToolDef, ToolOutput};
use serde_json::json;

let tool = CallbackTool::new(
    ToolDef {
        name: "get_weather".into(),
        description: "current weather for a city".into(),
        schema: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
    },
    |input, _ctx| Ok(ToolOutput::text(format!("sunny in {}", input["city"]))),
);
```

自定义 provider 同理：实现 `Provider` trait（或用 `build_agent_with_provider` 复用 config 的工具/MCP 装配、只换 provider）。非流式 provider 可返回 `SimpleChatResponse`（`{text, stop_reason?, usage?}`），经 `ChatResponse::from_simple` 合成单条 delta——Python/Node 绑定的自定义 provider 走的就是这条路。工具名在装配时统一校验（`build_agent_with` 路径）：全列表（内置 + MCP + skill 注入 + 自定义）必须**唯一**、仅 `[A-Za-z0-9_-]`、≤64 字符——同名冲突 / 非法字符在构建时即报错，不再静默遮蔽（分发按首匹配）或等到首个请求被 provider 400 拒绝。

**Hooks 拦截层**（不改核心，观察并把关 agent 行为）：实现 `Hooks` trait（五个方法全有默认实现，只覆盖需要的），用 `Agent::set_hooks` 挂上。`before_tool` 可放行 / 拒绝（理由成为模型看到的错误结果）/ 改写执行输入；`after_tool` 事后审计；`on_chat_request` 只读观察每个主 turn 请求；`on_turn_stopping` 轮末仲裁（TinyWorker RSI 判停闭环）——top-level run 因模型自身 `end_turn` 即将收尾的当下被调用，返回 `Stop`（默认，零漂移）或 `Continue { note }`：不发 `Done`，note 经监督者旁注管线并入尾用户消息强令下一轮，单 run 续跑上限 6，超限降级为 `stop_reason = "arbitration_capped"`；每次非默认决策发一条 `turn_arbitration` 事件供宿主审计，cancel 在续跑窗口内照常生效，子 agent run 从不仲裁。`task` 子 agent 自动继承同一层——顶层拒绝在子 agent 内同样生效：

```rust,no_run
use tiny_agent::{Hooks, ToolDecision};
use std::sync::Arc;

struct Policy;
impl Hooks for Policy {
    fn before_tool(&self, tool: &str, _input: &serde_json::Value, _ctx: &tiny_agent::ToolCtx) -> ToolDecision {
        if tool == "shell" {
            ToolDecision::Deny("本部署禁用 shell".into())
        } else {
            ToolDecision::Allow
        }
    }
}

let mut agent = tiny_agent::build_agent(&cfg, Vec::new(), &client)?;
agent.set_hooks(Arc::new(Policy));
```

注意：钩子可能被多个 worker 线程并发调用（并行 `task` 的子 agent 各跑各的），实现要自带同步（`Mutex` / 原子量）；`--resume` 重建的 agent 需要重新 `set_hooks`。完整语义（调用次序、Modify 的审计分裂等）见 [`crates/core/src/hooks.rs`](../crates/core/src/hooks.rs) 的 trait 文档。

**工具能力标记**：自定义 `Tool` 还可用两个默认方法向 loop 声明能力——`concurrency()` 返回 `ToolConcurrency::Parallel` 即获得与内置 `task` 同款的同 turn 多调用并行执行（承诺可重入 + 轮询 `ctx.cancel`）；`available_to_subagent()` 返回 `false` 即退出子 agent 继承。路由按能力、不按名字，第三方工具与内置工具一视同仁。

**常驻会话的热更新**（GUI 宿主场景）：CLI 的 config 在构建时定格，但宿主可以让一个 `Agent` 活整个会话、按需换挡——`set_model` / `set_reasoning_effort` / `set_max_tokens` / `set_max_context_tokens` 都是**纯字段替换**，下次请求生效，MCP 连接、工具装配、历史与 session 全不动（重建 Agent 要重跑 MCP 握手，实测可达秒级）。MCP server 集合也可热增删：`add_mcp_server(McpServerConfig)` / `remove_mcp_server(name)` / `tool_names()`（语义见[MCP 运行时热增删](../README.md#运行时热增删库-api)节）。`Agent::compact(&mut events)` 是压缩的**宿主主动入口**：轮次间调用即折叠历史（滚动摘要 + 安全切点 + 落盘 + `Compressed { trigger: "manual" }` 事件），返回 `dropped`/`kept`/摘要调用的 `usage`，外加折叠前后的 token 估算（`estimated_tokens_before`/`estimated_tokens_after`，与 preflight 同一套启发式；锚点仍有效时另带 provider 实测 `measured_tokens_before`）——这笔成本不进下一次 run 的账单（run 入口会重置总账），由宿主自行记账；`compact_with(&mut events, Some("focus on …"))` 可附带**焦点指令**引导摘要侧重。这些方法的 `&mut self` 签名即顺序契约：run 持有借用，「轮次间调用」由借用检查器强制。注意 `task` 子 agent 持构造期 config 快照，不跟随换挡。代码沙箱的热换挡若翻转 `enabled`，请用 `set_tool_absent_notes` 同步缺位注记（见 `ToolCtx::tool_absent_notes` 的「为何裸报错有害」）——该方法此前是零测试的死公共 API，现有回归钉子（评审轮收编）；绑定层暂不转发（与 `on_turn_stopping` / `inject_note` 同批延后项）。

**推理回传与请求整形**（内核改进计划 1.1 / 1.3；用户面说明见 README 配置节的四个新键）：`ChatResponse` 多了 `reasoning: Option<Reasoning>`，内置 provider **无条件**产出——`Reasoning { provider, blocks: Vec<ReasoningBlock> }`，每个块 `{ before, text, opaque }`：`before` 是它原先在 `content` 里所在位置的下标（Anthropic interleaved thinking 要求原位回显），`opaque` 是厂商私有伴生数据（Anthropic 的 `signature` / redacted `data`；OpenAI 兼容端点与手工裁剪过的旧记录恒为 `{}`，永不为 `null`）。`Message` 多的是**旁字段** `reasoning`，不是 `Block` 变体：`content` 形状不变，所有「块形状」规则（空回复不落盘、仲裁的零块判定、salvage 探测、子代理 `final_text`）原样成立，渲染层与 TUI 永远看不到它。是否落历史由 `Config.preserved_reasoning` 决定（**默认 `true`**，2026-11-26 起：deepseek-flash 上的 A/B 成功率不降、总 token −39%，收益全部来自会压缩的长任务，不压缩的短任务多花约 21%——数据见 `docs/kernel_improvement_plan.md` §0.0.5；只此一家模型，几乎不压缩的任务形态值得自己量一量。显式设成 `false` 时 `Agent::try_new` 把初始历史里的剥掉，之后每个回复的在落盘前丢弃——历史与没有这个字段时逐字节相同）。开启后的生命周期只有**当前轮**：`push_user` 开新 user 轮、`set_model`、相邻同角色消息合并都会把整段历史的 `reasoning` 清空（工具结果并入尾部 user 消息走的是合并分支，不清——当前工具循环的状态因此存活）；压缩折叠与工具结果裁剪保留幸存消息自己的；会话归档（`archive_messages` / `set_session_archive`）永不携带。自定义 provider 想参与只需在 `ChatResponse` 里填 `reasoning`（`from_simple` 合成的恒为 `None`）；provider 只回显自己家族的块（`Reasoning.provider` 为 `"anthropic"` / `"openai"`，跨厂商的旧状态跳过而不是发出去）。Anthropic 侧未签名的 thinking 块不回显；OpenAI 兼容侧按 `reasoning_content` 回显，400 教学到对端拒收后置 `GatewayFixes.no_reasoning_echo` 并不再回显（`gateway_fixes` 多一位，默认 false 时导出里不出现）。`extra_body` / `extra_headers` 由内置 provider 的构造器承接——`Anthropic::with_extra_body(Value)` / `with_extra_headers(ExtraHeaders)`（`OpenAi` 同名同参），`provider::build` / `build_agent*` 按配置自动调用，直接构造 provider 的宿主自己调。加载期校验 `Config::validate_extra`：`extra_body` 必须是对象且不得含内核自有键（`config::EXTRA_BODY_RESERVED`；设了 `reasoning_effort` 时还有 `EXTRA_BODY_RESERVED_WITH_EFFORT`），`extra_headers` 不得覆盖内核管理的头（`EXTRA_HEADERS_RESERVED`：鉴权 / content type / 协议版本等——provider 内部还有一道静默过滤，兜住手工构造时绕过加载校验的情况），值非法时报错**不回显值**。`ExtraHeaders` 的 `Debug` 对值打码；`Config::fingerprint` 对 `extra_headers` 只哈希头名（`extra_body` 全量进哈希；新键都是「非默认才进」——`preserved_reasoning` 自默认改开后只有显式 `false` 才进哈希）。批量覆盖：`preserved_reasoning` / `extra_body` / `extra_headers` / `env_passthrough` 四个键都是 HARD——`Config::apply` 拒绝任务片段设置它们；`apply_trusted`（宿主自建并已审过的 overlay）照常应用，且先在**副本**上跑 extras 校验（它要读同一 overlay 可能设置的 `reasoning_effort`），被拒的 overlay 不留半套修改。

**压缩观测钩子**（内核改进计划 2.2）：`Hooks::on_before_compact(&self, info: &CompactionInfo)`——只在折叠**真的发生**之前同步调用一次（摘要请求已经成功；摘要失败或空摘要不触发；只把陈旧工具结果换成占位符的 prune 不是压缩，也不触发）。`CompactionInfo { trigger, subagent, dropped, dropped_chars, todo, folding }`：`folding` 是即将离开实时历史的那些原始消息（调用期间仍在、可读；折叠之后它们只活在会话归档里），`todo` 是历史里最近一次 `todo` 调用的清单（宿主可把自己的笔记归档在旁边），`subagent` 在 `task` 子代理自己压缩时带其名——此时钩子跑在该子代理的 worker 线程上，照例要线程安全。只读：不能否决也不能改写折叠，摘要已经付费。与其他钩子不同，**这里的 panic 会被兜住**（裸 `catch_unwind`，折叠照常进行并发一条 `diag_warn`）——压缩是不可丢的收尾动作，不该被一个观察者的 panic 拖垮；代价是 `panic = "abort"` 的发布构建里兜不住，所以钩子实现仍不该 panic。默认空实现；绑定层暂未转发。TUI 的 `/compact [focus]` 走的就是宿主入口 `compact_with`。

**压缩内部的三个旋钮**（内核改进计划 §0.0.5 的三个真模型发现；都没有新增 API，是既有入口的行为变化）：

- **预检折叠的保留窗会缩**。阈值触发的自动压缩（Preflight）最多保留 `keep_recent_messages` 条原文；这个窗口自身的占用（字符或按锚点校准的 token 估算，与阈值同一口径）超过「阈值下的剩余空间」的一半时，改保留能装进一半的**最宽尾部**（`compress::shrink_keep`；最小到一个完整的工具调用对）。只往下调、不往上调，显式的 `keep_recent_messages: 0` 不动。手动 `compact()` 与溢出恢复照配置的窗口走，不缩。背景：保留窗自身压在阈值上时，预检每轮再折一次、每次只折得掉两条消息——实测 56 轮折了 32 次、其中 27 次只折掉两条。
- **摘要的输出预算随模型自适应**。摘要请求的 `max_tokens` 起点是正文规模的比例值（1024–4096），而推理模型的思考与正文共用这份额度。首次尝试在 `max_tokens` 处被截断**且带推理**（或正文为空）时，以翻倍的预算（上限 8192，不钳回本轮的 `max_tokens`）重试一次，取较长的一份正文；不带推理的截断仍是单次尝试。网关拒给翻倍预算（确定性的 4xx）→ 本模型不再要这个额度（一行告警）；重试被正常服务 → 之后每次摘要从这份预算起步（一行提示）。这两条学习挂在 agent 上、跨 run 保留，`set_model` 清掉。`compact()` 返回的 `usage` 是所有尝试的合并——烧掉的那次照计费。
- **`limits.summary_preview_chars`**（默认 200）：摘要器读到的折叠前缀是摘录转写，每个工具调用参数与工具结果各截到这个宽度；`0` 只留工具名 / 标签。batch 与回归任务片段里只能调小。默认值等于原先的硬编码值。`limits` 多了键，所以所有配置的 `config_fingerprint` 偏移一次。

打转护栏另有一个**只提示、不停**的隔次重复窗口（最近 8 次调用里同键第 4、6、8… 次出现时，提示写进该次的 tool_result 历史块）——`Compressed` / `ToolResult` 等事件面不变，宿主不需要改代码。

**凭据卫生**（内核改进计划 2.5；用户面说明见 README「⚠️ 安全说明」）：`ToolCtx.hidden_env: Vec<String>`——从工具拉起的每个子进程里移除的环境变量名。装配时由 `tools::hidden_env_vars(cfg)`（crate 内部函数）算出：默认恰好是 `api_key_env` 指向的那一个变量，`env_passthrough` 列了它则为空。库**从不修改进程环境**——移除发生在每个 spawn 点（`Command::env_remove`）：`shell` 工具（unix `sh -c` / windows `cmd /C` 两条链）、MCP stdio server（`McpClient::start_with_env(cfg, &hidden_env)`；无参的 `start(cfg)` 等价于空集——自己持有 MCP 客户端的宿主按需选）、热增的 `Agent::add_mcp_server`（读 agent 自己的 ctx 集）、`task` 子代理与 run_code guest 的 ctx 克隆（原样继承）。MCP 条目 `env` 里**显式写出**的同名变量照常下发（用户有意把密钥交给某个 server 时它保留）。定位是防顺手不防对抗：其余变量完整继承，同用户进程仍可经操作系统读到父进程环境。宿主自己写的、会 spawn 进程的自定义工具请照同款读 `ctx.hidden_env`（新增 spawn 点的检查清单见 `docs/claude/pitfalls.md`）。装配期经 diag 通道提示一次（每进程一次，`[config] API key variable(s) … hidden from tool subprocesses …`），只在配置里有 `shell` 工具或 stdio MCP server 时出现。

**MCP 注解旁表**（内核改进计划 1.6）：`ToolCtx::tool_annotations: Arc<BTreeMap<String, ToolAnnotations>>`——按**暴露名**（模型调用的名字）列出各 MCP server 对其工具的自述：`read_only_hint` / `destructive_hint` / `idempotent_hint` / `open_world_hint`（server 没声明时取规范默认值；`idempotentHint` 规范没有默认值，省略即 `None`）。便捷读法 `ctx.annotations(tool)`：内置工具为 `None`，MCP 工具恒有条目（什么都没声明时是 `ToolAnnotations::MISSING`）。刻意做成旁表而不是 `Tool` trait 方法：声明属于 server，内置工具 / 绑定 `CallbackTool` / run_code 注册都不必为它作答（第三方工具想声明就覆盖 `Tool::annotations`）。唯一构造点是 `Agent::try_new` + `Agent::refresh_tool_defs`（每个改工具表的入口都过这两个），所以它永远不会与 agent 实际分发的工具表不一致；`task` 子代理与 run_code guest 持构造时快照（Arc 克隆，父侧之后的增删只换父的 Arc）。每次调用把该工具的注解快照进 `ToolCallRecord.annotations`（落 result.json 的 `tool_calls[].annotations`）——审计时能回答「这次调用被 server 标成只读了吗」。**server 的自述、不是内核保证**：没有任何强制，调度上 `read_only_hint` 也只是并行提示。

**模型窗口提示**（内核改进计划 1.5）：`Limits::context_window_hint()` 是纯函数——`max_context_tokens == 0`（默认）时返回一行说明（历史只按字符阈值压缩、4 字符/token 的估算对 CJK 偏低、该怎么改），设了窗口则 `None`。`Limits::context_window_hint_line()` 是同一判定的一行版（只留结论与该怎么改，同样窗口已设返回 `None`），给放不下一整段说明的宿主用。CLI 在启动时提示一次（headless / batch 把完整版打到 stderr；TUI 把一行版搭在欢迎行里），库宿主自己选通道。不是校验：默认配置合法，溢出恢复仍是兜底。

**MCP 客户端契约**（内核改进计划 0.1 / 2.4；用户面说明见 README 的「超时、工具过滤、取消」小节，这里只列库侧要点）：
① **`tools/list` 翻页**：握手期循环带 `cursor` 直到响应没有 `nextCursor`（此前只取第一页——分页 server 第二页起的工具被静默丢弃）。上限 64 页（`MCP_LIST_MAX_PAGES`）与 256 个工具（`MCP_MAX_TOOLS`，逐 server）；跨页重名保留先出现者并告警，`nextCursor` 重复即停；**某一页请求失败则整个握手失败**（不允许带着没人察觉的残缺工具表起跑）。单页 server 的请求序列与翻页之前逐字节相同。
② **超时键** `call_timeout_secs`（默认 120）/ `connect_timeout_secs`（HTTP 与 legacy SSE 默认 15；stdio 的握手默认沿用该 server 的调用超时）。`0` 与超过 `config::MCP_MAX_TIMEOUT_SECS`（7 天）的值在 `McpServerConfig::validate()` 即报错（文案提示单位写错），不再静默钳制；超时错误点名实际生效的那个键——`MCP call 'tools/call' timed out after 2s (limit: call_timeout_secs) or server died`。Streamable HTTP 上直接在 POST 响应体里应答的那次交换现在**整段可取消**（交换在 worker 线程上跑，调用方按 ≤50 ms 薄片轮询 cancel；此前只能等满超时）。
③ **工具名单** `allow_tools` / `deny_tools`：写 **wire 名**（server `tools/list` 里的原始名，不是 `mcp_<server>_<tool>` 展示名）；先 allow 后 deny，重叠时 deny 胜。`allow_tools` 非空且含 server 不暴露的名字 → 装配期 `Err`，启动装配、`build_agent_with_lenient`（该 server 进 `McpFailure` 清单）、热增 `add_mcp_server`（spawn 回滚）三处同款；`deny_tools` 里的未知名字**不**报错（上游删了工具不该拖垮装配）。过滤后的工具才是 `tool_names()` / `tool_defs` / 模型看到的那一份。`mcp_servers` 整节是 batch 不可覆盖字段，所以四个新键同样不能被任务片段改写。
④ **协议版本**：`initialize` 提供 `2025-03-26`，读回 `negotiatedProtocolVersion` 并经 `McpClient::protocol_version()` 暴露；此后每个 HTTP 请求的 `MCP-Protocol-Version` 头带协商值（此前恒发 `2025-03-26`——只认新修订的 server 会逐个 400）。已知修订（`2025-03-26` / `2025-06-18` / `2025-11-25`）静默；未知修订原样采用并告警一次；server 不报版本则按 `2025-03-26`。任何情况都不因版本拒绝 server。
⑤ **取消通知**：`tools/call` 等待期间被取消 → 向 server 发 `notifications/cancelled {requestId, reason}`（尽力而为：在调用方已返回**之后**才发，1 s 上限，发不出去只是不告知）；调用在发出前就已被取消则请求根本不出门。
⑥ **空闲探测**：HTTP 两种传输空闲 ≥ 60 s 后的 `tools/call` 先发 `ping`（5 s 上限）。有应答——哪怕是 JSON-RPC 错误——即照常发调用；静默则调用**不发出**，以 `MCP 'srv': unreachable after 62s idle — 'tools/call' was not sent (ping failed: …)` 失败（不把一次可能已被执行的调用丢进死连接）。stdio 不探测（已有 reader 死亡锁存）。
⑦ **会话恢复**（仅 Streamable HTTP）：带 session 的请求收到 404 → 视为 server 丢了会话，重做 `initialize` 并**只重发一次**（新会话起不来则原样报错）。工具表不重新拉取。

run 结束后从 [`RunOutcome`](../crates/core/src/agent.rs)（或 `Agent::usage()` / `messages()` / `model()` / `context_breakdown()` / `config_fingerprint()` getter）读回诚实的 token 账单与历史。`RunOutcome` 另带 `config_fingerprint`（有效配置的稳定指纹，宿主集成需求单 FR-4——运行台账/评测里钉住「模型 + harness 配对」，同配置同指纹、任一 harness 形状字段变动即变，凭据值永不进哈希；**指纹是 Agent 构造时的快照**——上文的热更新换挡（`set_model` 等）改变实际生效形状但不移动已出账指纹，换挡后做台账对比请重建 Agent 或每 run 一建，CLI/批处理天然每 run 一建无此歧义；**跨键警告（round-3 评审 R1-02）**：result.json 的 `model` 键取 run 结束时的 live 值，换挡后同一份 result.json 里 `(model, config_fingerprint)` 是从未在任何配置中共存过的配对——台账联查以 fingerprint 为准或换挡后重建 Agent，勿把该配对当配置身份）与 `gateway_fixes`（openai 兼容网关学到的请求体修复四个布尔位，FR-3；持久化后经 `Config::gateway_fixes_preset` 喂回下一进程可跳过教学 400）。边界（定版行为）：`task` 子代理的 provider 只从 `gateway_fixes_preset` 起步——不继承父 provider 进程内所学，子代理自己的 400 教学也不回汇父 provider；每个 `task` 调用都是新子代理，已付的教学 400 可能由子代理重付。需要跨实例池化时把子代理导出 OR 回父 provider 是幂等安全的，但目前没有消费方，不为没有消费方的 seam 扩面。`AgentEvent` 派生了 `Serialize`，绑定层可直接序列化成宿主语言的 dict / object。`Usage` 事件带 `tool_folded` 标记区分两种风味：`false` 是某次直接请求的 provider 实测（当前上下文占用的真值），`true` 是工具账单折进运行累计（如 `task` 子 agent 的多轮总和——**不是**当前历史的测量，宿主画 ctx 占用指示时只认 `false`）。`Message` 带**稳定 id**（`id: Option<String>`，agent 在消息进入历史时铸造、跨 save/load 不变、压缩折叠不重排存活者）——往消息上挂宿主侧资产（图片卡 / 文件链接）时锚 id 而不是下标，压缩/truncate 后锚点不漂移。 `Usage` 多了 `reasoning_tokens`——provider **单独上报**的推理 token（OpenAI 兼容端点的 `completion_tokens_details.reasoning_tokens`），是 `output_tokens` 的一个切分而不是第五根轴：`Usage::total()` 刻意不加它（否则同一批 token 记两次账）；provider 不报切分时为 0，「未知」与「没有」在这里不区分，没有任何闸门读它。`AgentEvent::Usage` 同名透出；`RunOutcome.reasoning_tokens` 跨请求求和（含 `task` 子代理花的、重试梯里失败 attempt 花的），序列化时 0 即省略键——非推理模型的 result.json 逐字节不变。

**上下文构成查询**（`Agent::context_breakdown()`，拉取式只读、不发请求）：把**下一次请求的上下文**按来源拆出 token **估算**量——system prompt 三段（env 头 / 用户 system / skill 简介）、工具 schema 四类（builtin / mcp / skill / custom，类内逐工具列名与占用）、消息历史按类型聚合（用户文本 / 助手文本 / 工具调用参数 / 工具结果 / 图片 / PDF / `read_skill` 加载的 skill 正文 / 回传的推理状态——后者只在 `preserved_reasoning` 开启时非零，现在是默认），外加总量。所有数字都是**估算**（与压缩 preflight 同一套 ≈4 字符/token + PDF 页×3000 启发式），**不是 provider 实测**——记账权威永远是 `Usage` 事件 / `usage()`；估算与实测刻意不混流，所以这是查询方法而非事件。有存活的测量锚点（上一轮带 usage 的主 chat）时返回 `calibration: { measured_prompt_tokens, scale }`，全部分量已按锚点实测/估算比值均匀缩放（本身仍是启发式，修正中文低估一类系统性偏差）。压缩折叠 / MCP 热增删 / 前缀改写会使锚点失效，`calibration` 自动回到 `null`（纯启发式口径）。返回的 `ContextBreakdown` 派生了 `Serialize`，绑定层直接给宿主 dict / object（键 snake_case）；自家 TUI 的 `/context` 命令就是它的第一个消费端。

**工具表规模提示**（只提示、不处置）：工具表是每次请求的固定成本，压缩折叠不了它。`ContextBreakdown.tools` 上有两个便捷方法——`tool_count()`（四类工具定义总数）与 `advisory()`：估算 token 超过 `TOOL_DEFS_ADVISORY_TOKENS`（10000）或工具数超过 `TOOL_DEFS_ADVISORY_COUNT`（30）时返回 `Some(ToolDefsAdvisory::{Tokens, Count, Both})`，否则 `None`；两条线各自独立判断，恰好等于阈值不算越过。判定取**报表当前口径**的数字——有存活校准时 `tokens` 已按锚点缩放（校准把估算拉回线内，提示随之消失；宽表估算被校准放大则会浮现），工具数不缩放。阈值是取自 Anthropic Tool search 文档的经验值，不是 provider 限制。配套：纯函数 `tool_defs_advisory(tokens, count)` 与展示文案 `tool_defs_advisory_lines(tokens, count) -> Option<Vec<String>>`（五条短句，`/context` 的提示块与构造期提示共用同一份文本）。构造 `Agent` 时内核经 diag 通道发一次 warn（`[context] tool definitions ≈ …`，未装 sink 时写 stderr；**每进程按文本去重**——批量模式逐任务重建 agent 只提示一次；仅顶层 agent，`task` 子代理不重复提示）。MCP 热增删后不再提示，需要当下判定的宿主读 `advisory()`。绑定层拿到的是序列化后的四类分桶（便捷方法不随 serde 导出；`tools.<类>.tools.length` 求和即工具数）。

**取消契约**（`set_cancel` / `clear_cancel`）：cancel 是**电平标志、归嵌入方所有**，`run()` 永不清除——run 期间任何时刻置位都有效；带着已置位的 cancel 启动 run 会立即返回（`stop_reason: "cancelled"`、`turns: 0`、零请求零 token）。清残留的主体责任在**受理新任务的一方**：`set_cancel` 装好共享 `Arc<AtomicBool>` 后，在受理点（标记会话 running 的同一处）调 `clear_cancel()`——受理点可能远早于 `run()`（懒构建可达数秒），让内核在 run 入口清会把窗口内用户的停止误当残留吞掉。内置调用点已按此契约工作：TUI 在提交每条输入时清、重试梯把取消视为终态不重试、`compact()` 入口清。绑定层同款：`clear_cancel()`（Python）/ `clearCancel()`（Node）。

**监督者旁注注入**（`supervisor_handle()` → `SupervisorHandle`，R1）：运行中往会话里递「观察者提醒」的通道，与 cancel 同款共享句柄模式——但队列自构造起就存在（不像 cancel 是选装），句柄任意时刻可克隆、run 期间**任意线程**可调 `inject_note(text)`（空白文本忽略，非阻塞）。投递语义：**FIFO、至多一次、绝不无声丢失**。内核在每次 LLM 调用前（`TurnStart` 事件之后、preflight 压缩之前——旁注字节数计入 token 预检）排空队列，把旁注作为带 `[监督者旁注 / Supervisor note]` 标记前缀的文本块**并入尾用户消息**：是块、不是独立消息——`normalize_adjacent` 会在下次请求前合并相邻同角色消息溶解独立消息，豁免合并则 wire 出现 user-user 相邻（Anthropic 交替约束）；块级身份正是宿主卡片渲染的粒度（tool_result 也是 user 消息内的块）。宿主回放按标记前缀**逐字节 `starts_with`** 识别（跨团队契约，改动需版本化），模型在该次请求即可见。run 结束仍有未投递旁注时，在 run 统一出口冲账进历史（`Done` 事件之前），每条发 `NoteInjected { kind, text, deferred }` 事件：`deferred=false` 本次请求可见；`deferred=true` 本 run 未及投递、已入历史、**下次 run 模型可见**——宁晚不丢。run 之间注入的旁注在下一 run 首轮投递。`drain_pending()` 在会话销毁路径取回仍未投递的旁注（Agent 丢弃时队列会随实例蒸发，这是唯一防蒸发保险丝）。压缩互适：摘要 prompt 附「近期监督者旁注保留主旨」引导、标记随转录可见；旁注**刻意不进**逐字保留机制（时效性控制信号，过时即弃，防历史膨胀）。`task` 子代理是独立 Agent 实例、无队列，旁注只进主 run。事件面：`--json` 映射 `{"type":"note_injected","kind","text","deferred"}`，headless 写一行 stderr、TUI 渲染系统卡；Python/Node 绑定的事件回调原样收到序列化事件（透传零改动），绑定层的 `inject_note` 暴露留待后续版本。两个已定为预期行为的边界交互：① 延迟冲账落在干净结束的 run 之后时，历史尾部由 assistant 变 user——`ends_with_user()` 翻真，TUI 启动自续跑 / CLI resume 免 `--task` 自动开跑的既有逻辑会把「让模型看到这条 note」当作下一 run 执行（这正是 deferred 语义的投递机制）；不希望自动开跑的宿主应在 resume 判定前自行处理 `NoteInjected { deferred: true }` 事件或调 `drain_pending()`。② `clear_messages`（/clear）只清历史、不清队列——排队的 note 留到下一 run 首轮照常投递（「绝不无声丢失」优先于清理语义）。标记常量已公开导出（`ta::SUPERVISOR_NOTE_MARKER`），宿主引用常量而非硬编码字符串。

**PDF 文档输入**（`Block::Document`）：Anthropic 协议原生支持把 PDF 作为 `document` content block——服务端逐页"转图 + 抽取文本"，图表/版面都能看。库侧用 `load_document_block(path)` 读本地文件（`.pdf` 后缀 + `%PDF-` 头校验、10 MiB 上限，对齐 `load_image`），随消息推入：

```rust
use tiny_agent::{load_document_block, Block};
let doc = load_document_block(std::path::Path::new("report.pdf"))?;
agent.push_user(vec![doc, Block::Text("总结这份报告的要点".into())]);
```

三条语义：① **仅 Anthropic provider**——OpenAI 兼容端点在发请求前显式报错（不会静默丢弃文档），纯 custom-provider 绑定不受影响；② 预压缩按**页数**估 token（官方口径每页 1,500–3,000、取保守上沿 3,000；页数是免解析启发式：裸扫 page 对象标记 + 页树 `/Count` 互为补充，全无可见结构时按 ~100 KiB/页兜底）——文本型 PDF 有几百页，按字节的估算会差几个数量级；③ TUI / resume 转录不渲染文档字节（与图片同款跳过），压缩摘要转录里以 `<document>` 占位。

**OpenAI 兼容端点要接 PDF 附件？**（前端 / GUI 宿主）核心侧拒绝是刻意的——chat-completions 生态没有标准 PDF 内容类型（OpenAI 的文件输入在 Responses API；Kimi / GLM / Doubao 的文档理解各走自家非 OpenAI 兼容的 file API），任何"转发"都只能是某家网关的方言或静默丢内容。宿主侧四条路，按成本排：

1. **UI 按 provider 门控**——宿主本就知道 `cfg.provider`，非 anthropic 不出附件按钮；拒绝错误发生在发请求前（零 token、确定性），直接透给用户也无妨。
2. **host 侧抽文本**——PyMuPDF / pdftotext 抽成文本推入，全端点可用；丢图表 / 版面。
3. **host 侧逐页转图**——PDF 页渲染成 PNG 推 `Block::Image`，需视觉模型；成本与 Anthropic 原生处理同量级（~1.6k token/页）。
4. **custom provider 对接端点自家文档 API**——注入自定义 Provider 后核心拒绝即不再适用（拒绝只发生在内置 `OpenAi` provider 上）。两个注入点：Rust 宿主用 `build_agent_with_provider`（复用 config 的工具 / MCP 装配、只换 provider——**provider 侧预处理的唯一公开入口**，见上文自定义 provider 段），Python / TS 绑定用 `provider=` 回调。两侧收到的 messages 都是整体序列化的（文档块以 `{"document":{"media_type":…,"data":…}}` 原样在场），宿主可自行对接端点自家的文档 API、或预转文本/图后再发。

绑定层同款：Python 的 `push_user(...)` / Node 的 `pushUser(...)` 既收 `str`（=一个文本块），也收内容块 dict 或它们的序列——块有两种等价形状：带 `type` 标签的 `{"type": "image", "media_type": …, "data": …}`，以及 `messages()` 实际读回的外部标签 serde 形状 `{"image": {"media_type": …, "data": …}}`，所以前端可把历史块原样回推（附件重发）。一个块对象只按一种形状解释：同时带多个形状键（`{"text": …, "image": …}`，或 `type` 与**另一种**形状键并存）会明确报错，不会静默丢掉其中一块。`text` / `image` / `document` 之外一律拒绝：`tool_use` / `tool_result` 的 id 必须与模型自己的输出配对，宿主注入即伪造配对。

**Agent 存在之前就要把一条 user 消息放进历史**（`tiny_agent::agent::append_user_turn`）：`push_user` 只能
在 Agent 建好之后调；宿主若在构造 Agent **之前**自己拼历史——CLI 的 `--resume --task` 就是（从 session
文件读回历史，再把新任务放上去）——要用这个自由函数，别自己 `messages.push(Message::user(…))`。它做两
件事：末尾已是 user 消息就折进去（Anthropic 拒收相邻两条 user）；否则开一条新的 user 轮，而开新轮
会**结束上一轮的推理寿命**（上一条 assistant 的 `Message.reasoning` 被整段剥掉，见上文「推理回传」）。
推入的消息没有 id：留给 Agent 构造时的 ingest 分配，或调用方自己先铸（CLI 自己铸，为的是高过归档里
的 id）。CLI 曾手抄这两个分支、漏了剥除，`--resume --task` 把上一轮的思考块带进了新一轮的每个请求
（`scripts/e2e_anthropic_wire.py` 在线上抓到，2026-11 修）。

### Python / TypeScript 绑定

两个语言绑定是 workspace 成员（`bindings/python`、`bindings/node`），各自 path-依赖内核 crate（`crates/core`），把上面同一套能力（agent loop、自定义工具、流式事件、取消、诚实 usage）导出给宿主语言。两者都**复用 `CallbackTool`** 注入自定义工具，**不改核心代码**。

**Python（`import tiny_agent`，PyO3 + maturin）**

```sh
cd bindings/python && maturin develop   # 装进当前 Python 环境
python -m pytest tests/                 # 离线测试（对进程内 SSE mock）
```

```python
import tiny_agent

# 一次性：
out = tiny_agent.chat("chat.json", "天气怎样？", api_key=os.environ["KEY"])
print(out["final_text"], out["input_tokens"], out["output_tokens"])

# 多轮 + 自定义 Python 工具（让用户「自行开发 agent」的关键能力）：
get_weather = tiny_agent.Tool(
    "get_weather", "查城市天气",
    {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    lambda args: {"content": f"sunny in {args['city']}"},
)
a = tiny_agent.Agent("chat.json", tools=[get_weather], api_key=os.environ["KEY"])
a.push_user("波士顿天气？")
print(a.run(on_event=lambda e: print(e))["final_text"])
```

`run()` 同步阻塞但**释放 GIL**（`py.allow_threads`），长网络等待期间其他 Python 线程可跑；事件回调用 `with_gil` 短暂重获。**构造期不同**（评审 D-F2 记账）：`Agent(...)` 构造同步持有 GIL 做 MCP spawn + 握手——MCP server 多或慢时解释器可能冻结数秒；Node 侧同款问题已用 `Agent.create` 异步构造解决，Python 的对应物是已记账的后续项，当前形态下请避免在持有解释器关键锁的代码路径里构造带 MCP 的 Agent。

**Ctrl-C 语义（Python）**：run 全程释放 GIL，SIGINT 的 C 级处理只置标志、宿主线程下次执行 Python
字节码时才转成 `KeyboardInterrupt`。绑定对**事件回调里**的 `KeyboardInterrupt` 特判（其余回调异常
照旧吞掉）：装了 cancel token（`set_cancel`）时，回调内 KI 置位宿主 flag，run 在下一轮界停
（`stop_reason: "cancelled"`），KI 在 `run()`/`compact()` 返回时重新抛出——flag 按取消契约不清除，
下次受理任务前要 `clear_cancel()`；没装 token 时 run 照常跑完，KI 同样在返回时抛出。重抛的
`KeyboardInterrupt` 带 **`.outcome` 属性**（数据随异常走的惯例）：run 的 `Ok` 为结果 dict、`Err`
为成本信封 dict（`e.outcome["usage"]` 可直读，无需再 `json.loads`）；compact 为折叠结果 dict、
无可压缩时 `None`、摘要失败时含 `burned` 的信封 dict——宿主捕获中断后仍能读回真实成本。

**上下文构成**：`a.context_breakdown()` 返回下一请求上下文的估算 token 分解 dict（`system` 三段 / `tools` 四类 / `messages.by_kind` 分型 / `total_tokens` / `calibration`，键 snake_case）——宿主画上下文占比视图的数据源；语义见上文「上下文构成查询」，与 `usage()` 同款 busy 契约（run 中跨线程调用阻塞等锁直至 run 结束、同线程回调重入报错——与 Node 侧的立即报错相反）。

**自定义 provider 回调**：传 `provider=` 一个 Python callable，整个替换内置 HTTP provider（不连网、不要 key）。callable 收 `{system, messages, tools, model, max_tokens}`，返回 `str`（=纯文本）或 `{text, stop_reason?, usage?}`：

```python
# 一个 mock / 适配器 provider：agent 驱动 loop，但每轮回复来自这个函数
def echo_provider(request):
    return {"text": f"you said: {request['messages'][-1]}", "stop_reason": "end_turn"}

a = tiny_agent.Agent("chat.json", provider=echo_provider)  # 无 api_key
```

非流式 v1：返回整段文本，作为单条 delta 发出；不支持 tool_use（需要工具调用请用内置 provider + 自定义 `Tool`）。provider 抛异常会**诚实地 fail 该轮**。**取消 / 超时契约**：callable 在 GIL 内同步执行，`timeout_secs` 与取消**不会中断它**——agent 等它返回，可长时间运行的 provider 必须自行设限；预算与超时仍在轮次边界生效。

**Hooks 拦截层**（Python 侧同款核心能力）：`a.set_hooks(obj)` 或构造时 `hooks=obj` /
`chat(cfg, msg, hooks=obj)`——obj 按鸭子类型提供任意子集：`before_tool(tool, input, ctx)`
返回 `None`（放行）/ 拒绝字符串（成为模型看到的错误结果）/ `{"modify": input}`（换输入执行，
审计留原始输入）；`after_tool(tool, input, output, executed, ctx)` 事后审计
（`executed=False` 共三种形态：`before_tool` 拒绝、gate→执行取消检查点跳过
（审批等待期落下的 cancel）、并行批已过闸但未启动工具体的占位）；
`on_chat_request(request)` 只读观察主 turn 请求。`ctx` 是归因投影
`{workdir, subagent}`（顶层 `subagent=None`，`task` 子 agent / run_code 沙箱各自有名）。
缺的方法即无操作；方法存在但不可调用在安装时（构造 / `set_hooks`）当场 `TypeError`；
**`before_tool` 抛异常或返回非法值 = fail-closed 拒绝该调用**（理由进消息），
观察类钩子的异常被吞掉。完整语义（并发、`--resume` 不带 hooks、威胁模型）见上文与
`crates/core/src/hooks.rs` trait 文档。

**`run_code` 开关**（绑定默认不带 `code` feature，`maturin develop --features code` 编入）：`a.enable_code()`
/ `a.disable_code()` 运行期开关、`a.set_code_config({...})` 整包替换 code 配置（dict 形状同配置文件的
`code` 节，未知键报错、缺省键重置为默认值）。未编入该 feature 时三个方法返回带重建提示的明确错误。

**常驻会话的热更新**（同 Rust 侧「常驻会话的热更新」节）：`a.set_model(name)` /
`a.set_reasoning_effort("high" | None)` / `a.set_max_tokens(n)`（整数 `0..=4294967295`，0 钳到 1）/
`a.set_max_context_tokens(n)`（整数 `>= 0`，0 = 回退字符启发式）都是轮次间的纯字段替换，下次请求
生效；负数/小数/越界值抛 `ValueError` / `TypeError`，不静默截断（与 Node 侧同一接受集合）。MCP
集合可热增删：`a.add_mcp_server({"name": …, "command": …, "args": [...]})`（形状同配置文件的
`mcp_servers` 条目，未知键报错）返回新增工具名；`a.remove_mcp_server(name)` 返回移除数，未知名字
报错，撞名是可恢复错误（新 spawn 的子进程回滚，agent 原样不动）。`task` 子 agent 不跟随换挡。
锁契约同其他同步方法：run 中跨线程调用阻塞等锁、回调内同线程重入报错。

**Node.js / TypeScript（`import { chat, Agent }`，napi-rs）**

```sh
cd bindings/node && npm install && npm run build   # 生成 .node + wrapper.js
npm test                                            # 离线测试（node --test，对进程内 SSE mock）
```

> 分发模型（0.3.x 起跨平台拆包）：主包 `tiny-agent` 只带 loader/类型（`files` 不含
> `*.node`），原生二进制按平台拆 `tiny-agent-<platform>` 子包（darwin-x64/arm64、
> linux-x64-gnu/musl、win32-x64-msvc），主包以 `optionalDependencies` 引用同版本
> 子包；napi 生成的 `index.js` 优先加载本地 `.node`（开发构建）再回落子包。CI 的
> release workflow 按平台矩阵构建打包；配了 `NPM_TOKEN` secret 则发布 registry，
> 否则子包 tarball 附在 GitHub Release（`npm install <tgz>` 离线可用）。
> `tests/test_packaging.test.js` 是验收门：主包白名单（无 `*.node`、loader/类型
> 齐全）+ 本地伪造平台子包的安装 → 加载 + mock 调用全链路。

```javascript
const { chat, Agent } = require('tiny-agent');

// 一次性 + 自定义（可 async）JS 工具：
const out = await chat('chat.json', '天气怎样？', {
  apiKey: process.env.KEY,
  tools: [{
    name: 'get_weather', description: '查城市天气',
    schema: { type: 'object', properties: { city: { type: 'string' } }, required: ['city'] },
    run: async (args) => `sunny in ${args.city}`,   // 返回 str 或 { content, isError }
  }],
});
console.log(out.final_text, out.input_tokens, out.output_tokens);

// 多轮：
const a = new Agent('chat.json', { apiKey: process.env.KEY });
a.pushUser('波士顿天气？');
console.log((await a.run(ev => console.log(ev))).final_text);
```

`run()` 是 `Promise`——agent loop 在 libuv worker 线程跑（**无 tokio**），事件经 `ThreadsafeFunction` 回主线程；自定义 JS 工具经 TSFN round-trip（agent 线程触发 JS → JS 调 `resolveTool(id, result)` 唤醒 worker）。**配置入口差异**：Node 三个入口（`new Agent` / `Agent.create` / `chat`）只收配置**文件路径**字符串（传其他类型当场 `TypeError`）；内联 dict/object 配置目前仅 Python 侧支持。

**上下文构成**：`a.contextBreakdown()` 同步返回下一请求上下文的估算 token 分解对象（形状见 `wrapper.d.ts` 的 `ContextBreakdown`；键 snake_case 与 Rust serde 一致）——宿主画上下文占比视图的数据源；语义见上文「上下文构成查询」，busy 契约与 `usage()` 同款（run 中调用立即报错，不冻结事件循环）。

**常驻会话的热更新**：同 Python 的 camelCase 版本——`setModel` / `setReasoningEffort` /
`setMaxTokens` / `setMaxContextTokens` / `addMcpServer` / `removeMcpServer`（`McpServerConfig`
形状见 `wrapper.d.ts`），语义与 `task` 子 agent 不跟随的边界同 Rust 侧「常驻会话的热更新」节。
所有同步方法在 run 进行中立即 busy 报错（阻塞锁会冻结 run 依赖的事件循环）。`setMaxTokens`
只收 `0..=4294967295` 的整数（0 被内核钳到 1），`setMaxContextTokens` 只收 `>= 0` 的整数；
小数、负数、越界值当场报错，不做截断/回绕（napi 的整数转换会把 `-5` 变成 42 亿）。`addMcpServer`
同步 spawn + 握手，慢 server 会阻塞调用线程数秒；`removeMcpServer` 立即摘除工具，连接在最后
持有者（包含在飞 `task` 快照）释放时回收。

**异步构造 `Agent.create(configPath, opts)`**：与 `new Agent(...)` 等价，但 config 读取与工具装配
（含每个 MCP server 的 spawn + 握手——慢 server 可达数秒）在专用 worker 线程完成，**JS 事件循环
不被冻结**；返回 `Promise<Agent>`。`opts.tools` / `provider` / `hooks` 中的 JS 回调仍需在调用时
同步建立 TSFN（形状错误当场 throw），config / 装配错误则 reject。resolve 出的是 wrapper 类实例
（`instanceof Agent`、`run`/`compact` 的 onEvent 保护照旧）。配置里有 MCP server 时优先用它；
`new Agent(...)` 的同步语义保持不变。

**自定义 provider 回调**：传 `provider:` 一个函数（或 `{ chat }` 对象），整个替换内置 HTTP provider（不连网、不要 key）。收 `{system, messages, tools, model, max_tokens}`，返回 `string`（=纯文本）或 `{text, stop_reason?, usage?}`：

```javascript
// 一个 mock / 适配器 provider：agent 驱动 loop，每轮回复来自这个函数
const a = new Agent('chat.json', {
  provider: (request) => ({ text: `you said: ${request.messages.at(-1)}`, stop_reason: 'end_turn' }),
});
await a.run();
```

与自定义工具同款 TSFN round-trip（JS 调 `resolveProvider(id, result)` 唤醒 worker）。非流式 v1：返回整段文本，作为单条 delta 发出；不支持 tool_use（需要工具调用请用内置 provider + 自定义 `Tool`）。provider 抛异常会**诚实地 reject 该轮**。worker 等待回调的上限取 `min(本次请求剩余预算, 120s)`——run 设了 `timeout_secs` 时永不超出预算（预算仍在轮次边界生效；同步阻塞的 host 代码本身无法被中断）。

**Hooks 拦截层**（Node 侧同款核心能力，与 Python 鸭子类型面一致）：`a.setHooks(obj)` 或构造时
`hooks: obj` / `chat(cfg, msg, { hooks: obj })`——obj 按需提供 `beforeTool(tool, input, ctx)`
（返回 `null` 放行 / 拒绝字符串 / `{ modify: input }`，async 可用——决策经 TSFN round-trip）、
`afterTool(payload)` / `onChatRequest(request)`（只读观察，payload 形状见 `wrapper.d.ts`）。
`ctx` 是归因投影 `{ workdir, subagent }`（顶层 `subagent: null`）。缺的方法即无操作；
某个方法存在但不是函数会当场 `TypeError`（native TSFN 对非函数会断言 abort，wrapper 与 native
层都已在建 TSFN 前拦截）；**`beforeTool` 抛异常或返回非法值 = fail-closed 拒绝该调用**（理由进消息；
`deny` 与 `modify` 并存的决策同样算非法、落入 fail-closed，不静默取其中一键），观察类钩子的异常被
吞掉（其 TSFN 是 Fatal 策略——不吞就是进程 abort）。gate 决策等待与工具回调同款：上限取
`min(剩余预算, 120s)`、分片轮询 cancel——等待期被取消时报 "cancelled" 而非误报超时。语义同 Python 侧；`setHooks` 有 busy 契约。

**宿主进程生命周期（Node）**：用过自定义 `tools` / `provider` / `hooks`（或事件回调）的进程，
napi 的 TSFN 会让 libuv 句柄常驻，脚本自然结束后**不会自行退出**。这是既有行为，不是本次新增：
宿主应在收尾时显式 `process.exit(0)`（或关闭自己的句柄）；仓库测试用 `node --test --test-force-exit`
规避。纯 MCP / 内置 provider 的 agent 不受影响。

**`runCode` 开关**（绑定默认不带 `code` feature，`npm run build -- --features code` 编入）：`a.enableCode()`
/ `a.disableCode()` 运行期开关、`a.setCodeConfig({...})` 整包替换 code 配置（对象形状同配置文件的
`code` 节，未知键报错、缺省键重置为默认值）。未编入该 feature 时三个方法返回带重建提示的明确错误。

**绑定层语义边界**（round-2 外评审 R3 定版，两侧对照实测）：

- **数值契约不对称**：自定义 provider 返回的 `usage` 数字，Python 侧走 PyO3 i64 门——`2^53+1`
  也能精确送达；Node 侧 napi 数值是 f64，超过 `u32::MAX`（约 42.9 亿）的整数会被 serde 当浮点
  拒进 u64（`"invalid type: floating point … expected u64"`），**整个 run 失败**。内置 HTTP
  provider 不经此路径（usage 来自响应体 JSON 文本直解，u64 全域可用）；只有宿主自建 provider
  喂超天量 usage 才会撞上——两侧接受集合按平台现实记录在案，不为假想消费方抹平。
- **事件回调同步性不对称**：Python `on_event` 在 GIL 内**同步**执行、阻塞 run——回调内调
  cancel 是确定性的（`TurnStart` 里翻 → 走 pre-POST 诚实成本信封；流中翻 → 优雅
  `stop_reason: "cancelled"`）。Node 侧 TSFN **异步投递不等待**——同一探针三连跑出现竞态，
  回调内 cancel 是 best-effort；需要「回调看到 X 就停」的强因果请用 Python 形态或改在回调外决策。
- **宿主输入的 schema 深度**：内核对**模型侧**流式解析有 128 层深度限制，但**宿主传入**的工具
  定义 JSON 没有对应防线——200k 层深嵌套的 Tool schema 会让 Python 进程原生栈溢出
  （`0xC00000FD`，PyO3 捕不到）或 Node V8 fatal abort，两侧均不可捕获。输入源是宿主自身
  （自伤级），修复需要边界深度巡检 + 全链路迭代化，记账待真实需求出现再做；宿主请自行约束
  传入 schema 的嵌套深度。
- **转发缺口全清单**（在 `on_turn_stopping` / `inject_note` / `set_tool_absent_notes` /
  history seeding 四项挂账之外，两侧对称还缺 19 项，wave-2 复审核实）：`provider` /
  `config_fingerprint` / `limits` / `system_prompt` / `tool_names` / `tool_defs` /
  `cancelled` / `code_audit` / `set_shutdown` / `clear_messages` / `enable_session`(+archive)
  / `set_code_tool` / `SUPERVISOR_NOTE_MARKER` / `extract_plan` / `preview` /
  `numeric_id_suffix` / `ends_with_user` / `supervisor_handle`（连带
  `SupervisorHandle::drain_pending`）——**session 持久化与监督者旁注注入面在绑定侧目前完全
  不可达**。`gateway_fixes` / `strip_think_tags` / `tool_absent_notes` / `salvage` 经 serde
  纯透传，两侧零代码对称无缺口。
