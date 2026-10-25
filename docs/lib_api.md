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

**Hooks 拦截层**（不改核心，观察并把关 agent 行为）：实现 `Hooks` trait（三个方法全有默认实现，只覆盖需要的），用 `Agent::set_hooks` 挂上。`before_tool` 可放行 / 拒绝（理由成为模型看到的错误结果）/ 改写执行输入；`after_tool` 事后审计；`on_chat_request` 只读观察每个主 turn 请求。`task` 子 agent 自动继承同一层——顶层拒绝在子 agent 内同样生效：

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

**常驻会话的热更新**（GUI 宿主场景）：CLI 的 config 在构建时定格，但宿主可以让一个 `Agent` 活整个会话、按需换挡——`set_model` / `set_reasoning_effort` / `set_max_tokens` / `set_max_context_tokens` 都是**纯字段替换**，下次请求生效，MCP 连接、工具装配、历史与 session 全不动（重建 Agent 要重跑 MCP 握手，实测可达秒级）。MCP server 集合也可热增删：`add_mcp_server(McpServerConfig)` / `remove_mcp_server(name)` / `tool_names()`（语义见[MCP 运行时热增删](../README.md#运行时热增删库-api)节）。`Agent::compact(&mut events)` 是压缩的**宿主主动入口**：轮次间调用即折叠历史（滚动摘要 + 安全切点 + 落盘 + `Compressed { trigger: "manual" }` 事件），返回 `dropped`/`kept`/摘要调用的 `usage`——这笔成本不进下一次 run 的账单（run 入口会重置总账），由宿主自行记账；`compact_with(&mut events, Some("focus on …"))` 可附带**焦点指令**引导摘要侧重。这些方法的 `&mut self` 签名即顺序契约：run 持有借用，「轮次间调用」由借用检查器强制。注意 `task` 子 agent 持构造期 config 快照，不跟随换挡。

run 结束后从 [`RunOutcome`](../crates/core/src/agent.rs)（或 `Agent::usage()` / `messages()` / `model()` / `context_breakdown()` getter）读回诚实的 token 账单与历史。`AgentEvent` 派生了 `Serialize`，绑定层可直接序列化成宿主语言的 dict / object。`Message` 带**稳定 id**（`id: Option<String>`，agent 在消息进入历史时铸造、跨 save/load 不变、压缩折叠不重排存活者）——往消息上挂宿主侧资产（图片卡 / 文件链接）时锚 id 而不是下标，压缩/truncate 后锚点不漂移。

**上下文构成查询**（`Agent::context_breakdown()`，拉取式只读、不发请求）：把**下一次请求的上下文**按来源拆出 token **估算**量——system prompt 三段（env 头 / 用户 system / skill 简介）、工具 schema 四类（builtin / mcp / skill / custom，类内逐工具列名与占用）、消息历史按类型聚合（用户文本 / 助手文本 / 工具调用参数 / 工具结果 / 图片 / PDF / `read_skill` 加载的 skill 正文），外加总量。所有数字都是**估算**（与压缩 preflight 同一套 ≈4 字符/token + PDF 页×3000 启发式），**不是 provider 实测**——记账权威永远是 `Usage` 事件 / `usage()`；估算与实测刻意不混流，所以这是查询方法而非事件。有存活的测量锚点（上一轮带 usage 的主 chat）时返回 `calibration: { measured_prompt_tokens, scale }`，全部分量已按锚点实测/估算比值均匀缩放（本身仍是启发式，修正中文低估一类系统性偏差）。压缩折叠 / MCP 热增删 / 前缀改写会使锚点失效，`calibration` 自动回到 `null`（纯启发式口径）。返回的 `ContextBreakdown` 派生了 `Serialize`，绑定层直接给宿主 dict / object（键 snake_case）；自家 TUI 的 `/context` 命令就是它的第一个消费端。

**取消契约**（`set_cancel` / `clear_cancel`）：cancel 是**电平标志、归嵌入方所有**，`run()` 永不清除——run 期间任何时刻置位都有效；带着已置位的 cancel 启动 run 会立即返回（`stop_reason: "cancelled"`、`turns: 0`、零请求零 token）。清残留的主体责任在**受理新任务的一方**：`set_cancel` 装好共享 `Arc<AtomicBool>` 后，在受理点（标记会话 running 的同一处）调 `clear_cancel()`——受理点可能远早于 `run()`（懒构建可达数秒），让内核在 run 入口清会把窗口内用户的停止误当残留吞掉。内置调用点已按此契约工作：TUI 在提交每条输入时清、重试梯把取消视为终态不重试、`compact()` 入口清。绑定层同款：`clear_cancel()`（Python）/ `clearCancel()`（Node）。

**监督者旁注注入**（`supervisor_handle()` → `SupervisorHandle`，R1）：运行中往会话里递「观察者提醒」的通道，与 cancel 同款共享句柄模式——但队列自构造起就存在（不像 cancel 是选装），句柄任意时刻可克隆、run 期间**任意线程**可调 `inject_note(text)`（空白文本忽略，非阻塞）。投递语义：**FIFO、至多一次、绝不无声丢失**。内核在每次 LLM 调用前（`TurnStart` 事件之后、preflight 压缩之前——旁注字节数计入 token 预检）排空队列，把旁注作为带 `[监督者旁注 / Supervisor note]` 标记前缀的文本块**并入尾用户消息**：是块、不是独立消息——`normalize_adjacent` 会在下次请求前合并相邻同角色消息溶解独立消息，豁免合并则 wire 出现 user-user 相邻（Anthropic 交替约束）；块级身份正是宿主卡片渲染的粒度（tool_result 也是 user 消息内的块）。宿主回放按标记前缀**逐字节 `starts_with`** 识别（跨团队契约，改动需版本化），模型在该次请求即可见。run 结束仍有未投递旁注时，在 run 统一出口冲账进历史（`Done` 事件之前），每条发 `NoteInjected { kind, text, deferred }` 事件：`deferred=false` 本次请求可见；`deferred=true` 本 run 未及投递、已入历史、**下次 run 模型可见**——宁晚不丢。run 之间注入的旁注在下一 run 首轮投递。`drain_pending()` 在会话销毁路径取回仍未投递的旁注（Agent 丢弃时队列会随实例蒸发，这是唯一防蒸发保险丝）。压缩互适：摘要 prompt 附「近期监督者旁注保留主旨」引导、标记随转录可见；旁注**刻意不进**逐字保留机制（时效性控制信号，过时即弃，防历史膨胀）。`task` 子代理是独立 Agent 实例、无队列，旁注只进主 run。事件面：`--json` 映射 `{"type":"note_injected","kind","text","deferred"}`，headless 写一行 stderr、TUI 渲染系统卡；Python/Node 绑定的事件回调原样收到序列化事件（透传零改动），绑定层的 `inject_note` 暴露留待后续版本。

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

`run()` 同步阻塞但**释放 GIL**（`py.allow_threads`），长网络等待期间其他 Python 线程可跑；事件回调用 `with_gil` 短暂重获。

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
审计留原始输入）；`after_tool(tool, input, output, executed, ctx)` 事后审计（`executed=False`
仅当 Deny）；`on_chat_request(request)` 只读观察主 turn 请求。`ctx` 是归因投影
`{workdir, subagent}`（顶层 `subagent=None`，`task` 子 agent / run_code 沙箱各自有名）。
缺的方法即无操作；**`before_tool` 抛异常或返回非法值 = fail-closed 拒绝该调用**（理由进消息），
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

`run()` 是 `Promise`——agent loop 在 libuv worker 线程跑（**无 tokio**），事件经 `ThreadsafeFunction` 回主线程；自定义 JS 工具经 TSFN round-trip（agent 线程触发 JS → JS 调 `resolveTool(id, result)` 唤醒 worker）。

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
层都已在建 TSFN 前拦截）；**`beforeTool` 抛异常或返回非法值 = fail-closed 拒绝该调用**（理由进消息），观察类钩子的异常被
吞掉（其 TSFN 是 Fatal 策略——不吞就是进程 abort）。语义同 Python 侧；`setHooks` 有 busy 契约。

**宿主进程生命周期（Node）**：用过自定义 `tools` / `provider` / `hooks`（或事件回调）的进程，
napi 的 TSFN 会让 libuv 句柄常驻，脚本自然结束后**不会自行退出**。这是既有行为，不是本次新增：
宿主应在收尾时显式 `process.exit(0)`（或关闭自己的句柄）；仓库测试用 `node --test --test-force-exit`
规避。纯 MCP / 内置 provider 的 agent 不受影响。

**`runCode` 开关**（绑定默认不带 `code` feature，`npm run build -- --features code` 编入）：`a.enableCode()`
/ `a.disableCode()` 运行期开关、`a.setCodeConfig({...})` 整包替换 code 配置（对象形状同配置文件的
`code` 节，未知键报错、缺省键重置为默认值）。未编入该 feature 时三个方法返回带重建提示的明确错误。
