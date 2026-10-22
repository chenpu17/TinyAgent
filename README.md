# TinyAgent

一个极简的单二进制 AI agent：一个 JSON 配置即可运行。两种模式 —— headless 执行任务（harness 场景）或 TUI 对话。目标：嵌入式低配 Linux、macOS、Windows，工具精炼、约 1.5 万行核心代码（核心 agent loop 集中在 `agent.rs`；可选 TUI 约 5 千行），结构清晰可快速通读并自行定制。

## 特性

- **单一可执行文件**，无运行时依赖；musl 静态编译后可丢到路由器 / 树莓派 / CI runner 直接跑
- **两种 LLM 后端**：Anthropic 原生 API + OpenAI 兼容协议（覆盖 DeepSeek / vLLM / Ollama 等），均带 token 用量统计
- **图片输入**：`--image` 参数或 `load_image` 工具（png / jpeg / gif / webp，base64）
- **PDF 文档输入**：库侧 `Block::Document`（Anthropic 原生 `document` block，逐页"转图 + 抽取文本"处理；`load_document_block` 读取 + `push_user` 推入）
- **精炼工具集**：`shell`、`read_file`、`write_file`、`edit_file`、`list_dir`、`task`（子 agent；同 turn 多调用并行）、`todo`（任务计划，落进 result.json 的 `plan` 字段）默认启用；`load_image`（视觉）需在 `tools` 显式加入。均可在 config 的 `tools` 里裁剪
- **沙箱 JS 执行（可选 `code` feature）**：`run_code` 工具让模型在进程内 wasm 沙箱（QuickJS-in-wasm）里写 JS 编排既有工具——能力构造性收敛（无网络 / 无文件系统直访，工具调用全走 hooks 闸门），fuel / 墙钟 / 内存 / 调用数四重限额；两层开关（feature + `code.enabled`）默认都关；[快速上手与最佳实践](docs/run_code_guide.md)
- **Harness 契约**：`--output result.json` 写结构化结果（含 token 用量、每次工具调用的 input/output），`--json` 流式 NDJSON；exit code 语义化（`0` 成功 / `124` 超时 / `130` 取消 / `1` 其它），可直接进 CI / 评测 pipeline
- **CLI 覆盖**：`--model` / `--max-turns` / `--timeout` 免改配置做 A/B 对照
- **批量任务**：`--batch <dir>` 并发跑一组任务片段，`--retries` 对 transient 失败指数退避重试，`--failed` 只重跑上一轮失败的任务并合并进完整报告
- **TUI（可选 feature）**：ratatui 对话界面——内联/流式渲染（对话进终端原生 scrollback，原生选择/滚动/搜索开箱即用）、多行输入、输入历史、斜杠命令、token 状态条；默认不编译，保持 headless 版极小
- **同步实现**（无 tokio），`reqwest::blocking` + rustls，核心 agent loop 集中在 `agent.rs`
- **库 API**：内核独立成 crate（`crates/core`，package `tiny-agent-core`，lib 名 `tiny_agent`），第三方 Rust 程序可直接 `tiny_agent = "..."` 驱动 agent loop、注册自定义工具（`CallbackTool`）、读回诚实 token 账单；Python（PyO3）/ TypeScript（napi-rs）绑定构建其上；根 package `tiny-agent`（bin）也是它的消费者。`mock` feature 放出脚本化 provider 供离线 dry-run。**扩展面**：Hooks 拦截层（`set_hooks`：审批/拒绝/改写工具调用、审计、观察请求，子 agent 自动继承）+ 工具能力标记（`concurrency()` 声明并行、`available_to_subagent()` 声明子 agent 继承），均无需改核心

## 构建

```sh
cargo build --release                     # headless 版（最小体积，无 TUI / run_code）
cargo build --release --features tui      # + TUI 对话界面
cargo build --release --features code     # + run_code 沙箱（wasmtime，musl 静态版 +≈4.6 MiB）
cargo build --release --features tui,code # 桌面全功能（install.sh 的默认组合）
```

交叉编译（推荐用 [cross](https://github.com/cross-rs/cross)）：

```sh
cross build --release --target x86_64-unknown-linux-musl
cross build --release --target aarch64-unknown-linux-musl
cross build --release --target armv7-unknown-linux-musleabihf
cross build --release --target x86_64-pc-windows-msvc
```

仓库根的 `install.sh` 把 release 二进制装进 `~/.local/bin`（`PREFIX` 可改，无需 sudo），默认构建
**`tui,code` 桌面全功能**组合；要裁剪用 `TA_FEATURES="tui" ./install.sh` 覆盖。官方 GitHub
Releases 的 musl 静态产物只带 `code`（无 TUI——「嵌入式低配 Linux」定位优先体积与静态链接，
桌面 TUI 请自建或用 install.sh）。装完跑 `tiny-agent --version --json` 可核对当前二进制编入了
哪些 feature（`features` 数组）。

## 库 API（程序化调用）

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

注意：钩子可能被多个 worker 线程并发调用（并行 `task` 的子 agent 各跑各的），实现要自带同步（`Mutex` / 原子量）；`--resume` 重建的 agent 需要重新 `set_hooks`。完整语义（调用次序、Modify 的审计分裂等）见 [`src/hooks.rs`](src/hooks.rs) 的 trait 文档。

**工具能力标记**：自定义 `Tool` 还可用两个默认方法向 loop 声明能力——`concurrency()` 返回 `ToolConcurrency::Parallel` 即获得与内置 `task` 同款的同 turn 多调用并行执行（承诺可重入 + 轮询 `ctx.cancel`）；`available_to_subagent()` 返回 `false` 即退出子 agent 继承。路由按能力、不按名字，第三方工具与内置工具一视同仁。

**常驻会话的热更新**（GUI 宿主场景）：CLI 的 config 在构建时定格，但宿主可以让一个 `Agent` 活整个会话、按需换挡——`set_model` / `set_reasoning_effort` / `set_max_tokens` / `set_max_context_tokens` 都是**纯字段替换**，下次请求生效，MCP 连接、工具装配、历史与 session 全不动（重建 Agent 要重跑 MCP 握手，实测可达秒级）。MCP server 集合也可热增删：`add_mcp_server(McpServerConfig)` / `remove_mcp_server(name)` / `tool_names()`（语义见[MCP 运行时热增删](#运行时热增删库-api)节）。`Agent::compact(&mut events)` 是压缩的**宿主主动入口**：轮次间调用即折叠历史（滚动摘要 + 安全切点 + 落盘 + `Compressed { trigger: "manual" }` 事件），返回 `dropped`/`kept`/摘要调用的 `usage`——这笔成本不进下一次 run 的账单（run 入口会重置总账），由宿主自行记账；`compact_with(&mut events, Some("focus on …"))` 可附带**焦点指令**引导摘要侧重。这些方法的 `&mut self` 签名即顺序契约：run 持有借用，「轮次间调用」由借用检查器强制。注意 `task` 子 agent 持构造期 config 快照，不跟随换挡。

run 结束后从 [`RunOutcome`](src/agent.rs)（或 `Agent::usage()` / `messages()` / `model()` / `context_breakdown()` getter）读回诚实的 token 账单与历史。`AgentEvent` 派生了 `Serialize`，绑定层可直接序列化成宿主语言的 dict / object。`Message` 带**稳定 id**（`id: Option<String>`，agent 在消息进入历史时铸造、跨 save/load 不变、压缩折叠不重排存活者）——往消息上挂宿主侧资产（图片卡 / 文件链接）时锚 id 而不是下标，压缩/truncate 后锚点不漂移。

**上下文构成查询**（`Agent::context_breakdown()`，拉取式只读、不发请求）：把**下一次请求的上下文**按来源拆出 token **估算**量——system prompt 三段（env 头 / 用户 system / skill 简介）、工具 schema 四类（builtin / mcp / skill / custom，类内逐工具列名与占用）、消息历史按类型聚合（用户文本 / 助手文本 / 工具调用参数 / 工具结果 / 图片 / PDF / `read_skill` 加载的 skill 正文），外加总量。所有数字都是**估算**（与压缩 preflight 同一套 ≈4 字符/token + PDF 页×3000 启发式），**不是 provider 实测**——记账权威永远是 `Usage` 事件 / `usage()`；估算与实测刻意不混流，所以这是查询方法而非事件。有存活的测量锚点（上一轮带 usage 的主 chat）时返回 `calibration: { measured_prompt_tokens, scale }`，全部分量已按锚点实测/估算比值均匀缩放（本身仍是启发式，修正中文低估一类系统性偏差）。压缩折叠 / MCP 热增删 / 前缀改写会使锚点失效，`calibration` 自动回到 `null`（纯启发式口径）。返回的 `ContextBreakdown` 派生了 `Serialize`，绑定层直接给宿主 dict / object（键 snake_case）；自家 TUI 的 `/context` 命令就是它的第一个消费端。

**取消契约**（`set_cancel` / `clear_cancel`）：cancel 是**电平标志、归嵌入方所有**，`run()` 永不清除——run 期间任何时刻置位都有效；带着已置位的 cancel 启动 run 会立即返回（`stop_reason: "cancelled"`、`turns: 0`、零请求零 token）。清残留的主体责任在**受理新任务的一方**：`set_cancel` 装好共享 `Arc<AtomicBool>` 后，在受理点（标记会话 running 的同一处）调 `clear_cancel()`——受理点可能远早于 `run()`（懒构建可达数秒），让内核在 run 入口清会把窗口内用户的停止误当残留吞掉。内置调用点已按此契约工作：TUI 在提交每条输入时清、重试梯把取消视为终态不重试、`compact()` 入口清。绑定层同款：`clear_cancel()`（Python）/ `clearCancel()`（Node）。

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

另：绑定层的 `pushUser` 目前只收文本——文档块推入面透出到 Python / TS（与 hooks 同批的 backlog）是前端做 PDF 附件的前置依赖，Rust 宿主现在就可用 `push_user`。

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

**上下文构成**：`a.context_breakdown()` 返回下一请求上下文的估算 token 分解 dict（`system` 三段 / `tools` 四类 / `messages.by_kind` 分型 / `total_tokens` / `calibration`，键 snake_case）——宿主画上下文占比视图的数据源；语义见库 API 节[上下文构成查询](#库-api程序化调用)，与 `usage()` 同款 busy 契约（run 中跨线程调用阻塞等锁直至 run 结束、同线程回调重入报错——与 Node 侧的立即报错相反）。

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
观察类钩子的异常被吞掉。完整语义（并发、`--resume` 不带 hooks、威胁模型）见库 API 节与
`src/hooks.rs` trait 文档。

**`run_code` 开关**（绑定默认不带 `code` feature，`maturin develop --features code` 编入）：`a.enable_code()`
/ `a.disable_code()` 运行期开关、`a.set_code_config({...})` 整包替换 code 配置（dict 形状同配置文件的
`code` 节，未知键报错、缺省键重置为默认值）。未编入该 feature 时三个方法返回带重建提示的明确错误。

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

**上下文构成**：`a.contextBreakdown()` 同步返回下一请求上下文的估算 token 分解对象（形状见 `wrapper.d.ts` 的 `ContextBreakdown`；键 snake_case 与 Rust serde 一致）——宿主画上下文占比视图的数据源；语义见库 API 节，busy 契约与 `usage()` 同款（run 中调用立即报错，不冻结事件循环）。

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
**`beforeTool` 抛异常或返回非法值 = fail-closed 拒绝该调用**（理由进消息），观察类钩子的异常被
吞掉（其 TSFN 是 Fatal 策略——不吞就是进程 abort）。语义同 Python 侧；`setHooks` 有 busy 契约。

**`runCode` 开关**（绑定默认不带 `code` feature，`npm run build -- --features code` 编入）：`a.enableCode()`
/ `a.disableCode()` 运行期开关、`a.setCodeConfig({...})` 整包替换 code 配置（对象形状同配置文件的
`code` 节，未知键报错、缺省键重置为默认值）。未编入该 feature 时三个方法返回带重建提示的明确错误。

## 使用

`--config` 是可选的:省略时按以下顺序查找配置文件:

1. `--config <path>`(显式;即使文件不存在也直接用它,报读错误而非回落)
2. `$TINYAGENT_CONFIG`(非空才用)
3. `$XDG_CONFIG_HOME/tinyagent/config.json`,否则 `~/.config/tinyagent/config.json`

把模型配置放进 `~/.config/tinyagent/config.json` 后,直接 `tiny-agent --tui`
即可启动(类似 `claude` / `codex`)。首次找不到任何配置时,会打印一个最小
config 样例和该文件的绝对路径,指明怎么建。

```sh
# headless 执行任务，输出结构化结果
tiny-agent --config examples/harness.json --output result.json

# 覆盖配置里的 task
tiny-agent --config examples/harness.json --task "运行测试并修复失败"

# 免改配置做 A/B：临时换模型 / 轮次上限 / 超时
tiny-agent --config examples/harness.json --model deepseek-chat --max-turns 10 --timeout 300

# 流式 NDJSON 事件，方便管道消费进度（与 --output 可同时用）
tiny-agent --config examples/harness.json --task "总结日志" --json | jq 'select(.type=="usage")'

# 附带图片（视觉模型）
tiny-agent --config examples/harness.json --task "描述这张截图" --image shot.png

# 会话持久化：每轮自动落盘，--resume 继续上次会话
tiny-agent --config examples/chat.json --task "记住暗号 PINEAPPLE" --session sess.json
tiny-agent --config examples/chat.json --resume --task "暗号是什么？" --session sess.json

# 一次性运行不落盘会话（CI / 临时跑不想污染 cwd）
tiny-agent --config examples/harness.json --task "跑测试" --session none --output result.json

# 批量任务模式：跑目录下所有任务片段，2 个并发，每任务私有 workdir 拷贝，输出汇总报告
tiny-agent --config examples/harness.json --batch examples/tasks --jobs 2 --isolate copy --report report.json
# 只重跑上一轮 report.json 里失败的任务（结果合并回完整报告），并对 transient 失败重试 2 次
tiny-agent --config examples/harness.json --batch examples/tasks --failed --retries 2 --report report.json

# 版本 + 编译期 feature 清单（机器可读，CI 可断言二进制没漏编 code/tui；无需配置文件）
tiny-agent --version --json

# MCP 工具服务器（stdio）
tiny-agent --config examples/mcp.json

# TUI 对话模式（需要 tui feature）
tiny-agent --config examples/chat.json --tui
```

## 批量任务模式

`--batch <dir>` 下每个 `*.json` 是一个任务片段（所有字段可选，覆盖 base 配置）：

```json
{ "task": "修复 tests/test_login.py 的失败", "limits": { "max_turns": 20 } }
```

- 每个任务独立跑完整 agent 流程，结果写 `results/<task>.json`（与 `--output` 同格式）
- `--jobs N` 控制并发（默认 1）；`--report` 写汇总：`{total, succeeded, failed, provider, totals, averages, tasks[]}`（`totals`/`averages` 为 4 个 token 轴的总和与 per-task 均值；`tasks[]` 每条带 `model` / `started_at` 归因——任务片段可覆盖 `model`，同批混模型时靠它区分）
- ⚠ **`--isolate` 之前的默认语义**：`--jobs > 1` 时所有任务**共用同一个 workdir**（base 配置的，任务片段不能覆盖）——并发写同名文件的任务会互相覆盖、污染结果。两条出路：
  - `--jobs 1`（串行，或任务只读 / 纯推理可放心并发）
  - `--isolate <mode>` 给每个任务一份私有 workdir（见下）
- exit code：全部成功 → 0，否则 1，可直接做 CI 门禁

### `--isolate`：任务级 workdir 隔离

| mode | 做法 | 适用 |
|---|---|---|
| `none`（默认） | 不隔离，所有 worker 共用 base workdir | 只读任务 / `--jobs 1` |
| `copy` | 每任务把 workdir **拷贝**到临时目录，跑完删除。排除 `target`/`node_modules`/`.git`/`results`（体积大头 + 防把上轮结果当输入） | 通用；大仓库较慢 |
| `git` | 每任务 `git worktree add --detach` 一份检出，跑完 `worktree remove --force` | 仓库内任务零拷贝；**要求 workdir 是 git 仓库根**（子目录报 `setup_error`，不重试）。**检出的是 HEAD**——未提交 / 未跟踪文件不进隔离副本，要带上它们先 commit 或改用 `copy` |

- **per-task、跨 attempt 复用**：一个任务的重试看得到自己前次 attempt 的落盘副作用（与共享 workdir 语义一致）
- **产物随目录丢弃**：隔离模式下任务写的文件不留在 workdir——评测语义下审计面就是 `result.json`（`tool_calls` 全量）与 `report.json`；需要产物落盘就别用隔离
- 与 `"restrict_to_workdir": true` 天然协同（restrict 的沙箱根随隔离目录走）——不可信批量任务的推荐组合：容器内运行 + `--isolate copy` + restrict。**`--isolate` 是评测隔离不是安全沙箱**：`shell` 仍能访问文件系统可达的一切路径，不可信任务必须放进容器 / 受限用户
- `copy` 的符号链接策略（副本里不允许有通往原树或外部的路）：解析后仍在源树内的**相对**链接原样复刻（`node_modules/.bin` 类工具布局继续可用，且天然 copy-local）；**绝对**链接在源树内则拷贝目标内容（原样复刻会指回原树、把并行任务重新耦合到共享目录）；解析后在源树**外**的链接（绝对或 `..` 逃逸）跳过并打 stderr 告警
- 隔离 setup 失败（git 缺失 / 非 repo / 临时目录不可写）按 `setup_error` 报告（确定性，不重试）
- git 模式孤儿 worktree：极端情况（进程被 SIGKILL）下 `git worktree prune` 在源仓库回收

任务片段**只能覆盖** `task` / `system_prompt` / `model` / `max_tokens` / `limits`。安全相关字段
（`provider` / `base_url` / `api_key_env` / `workdir` / `tools` / `restrict_to_workdir` /
`mcp_servers`）禁止覆盖——覆盖即标记 `bad_task` 失败，不执行。

### `--retries N`：transient 失败重试

只重试 **transient** 失败（`stop_reason == "error"`：网络抖动、429、5xx 等），
指数退避 `1/2/4/8/16/32s`（封顶 32s）。**不重试**确定性失败——重跑也不会变：

| stop_reason | 含义 | 是否重试 |
|---|---|---|
| `error` | transient provider/网络错误（连接被拒、429、5xx、请求超时） | ✅ |
| `request_error` | 确定性请求拒绝（401/403/404/400 等，重发同样被拒） | ❌ |
| `context_overflow` | 上下文溢出且强制压缩未能救回（确定性） | ❌ |
| `bad_task` | 任务片段非法（禁用字段 / 缺 task / JSON 错） | ❌ |
| `max_turns_reached` / `max_tokens` | 模型跑满上限 | ❌ |
| `budget_exceeded` | 累计 token 触顶 `limits.max_total_tokens` | ❌ |
| `stuck_loop` | 打转护栏：同参同果的相同调用连续 6 次（第 3 次先在下发的 tool_result 里附纠偏提示，给一次自愈机会；结果有进展或参数不同即重置） | ❌ |
| `panic` | 任务在 worker 里 panic，被兜成失败条目（见下） | ❌ |
| `end_turn` | 正常完成 | ❌（已成功） |

**重试语义 = 整任务从头重放**：每次重试用全新 Agent 重跑完整任务，前一轮已发生的副作用
（写文件、发请求等工具动作）**不回滚、不续接**——账单照实累计，但非幂等工具会重复执行。
对"追加文件""提交订单"这类有副作用的任务，要么保证任务幂等，要么不要开 `--retries`。

`panic` 仅 debug / CI 构建会出现（worker 用 `catch_unwind` 兜住、整批照常完
成）；release 是 `panic="abort"`，进程当场退出、不落 report。

`report.json` 的 `elapsed_secs` 含重试与退避的总耗时。

### `--failed`：只重跑失败任务

读上一轮 `--report` 里 `success == false` 的任务，只重跑这些；**未重跑的历史成功项原样保留**，
合并回完整报告。即一次 `--failed` 之后，`report.json` 仍反映全部任务的当前状态——重跑通过
的任务变绿，仍失败的任务保持红，CI 读到的 `total/succeeded` 是全集而非子集。

```sh
# 第一轮全量跑
tiny-agent --config examples/harness.json --batch examples/tasks --report report.json
# 修了点东西，只重跑失败的（+ 对 transient 失败重试 2 次）
tiny-agent --config examples/harness.json --batch examples/tasks --failed --retries 2 --report report.json
```

### 单跑瞬态重试

`--retries N` 在**单跑 headless** 模式同样生效：`agent.run` 返回 transient `Err`（网络抖动、429、
5xx）时按指数退避（1/2/4/8/16/32s 封顶）原地重试，最多 N 次额外尝试；确定性失败**不重试**——
`end_turn` / `max_turns` / `cancelled` / `timeout` / `budget_exceeded` / `stuck_loop` 是 `Ok`，`context_overflow` 与
`request_error`（401/403/404/400 等确定性请求拒绝）直接以 exit 1 收场。请求失败发生在 run 截止
（`limits.timeout_secs`）之后时按 `timeout`（exit 124）归属，不进重试梯。与 batch 相同，重试是**整任务
重放**：前轮工具副作用不回滚（账单照实累计），副作用型任务需自行保证幂等。退出码与无重试时一致
（成功 0、失败 1），`result.json` 的 `attempts` 字段记录实际运行次数（`>1` 即发生过重试）。`--json`
模式每次重试前另吐一行
`{"type":"retry","attempt":N,"max":M,"backoff_secs":B,"error":"..."}` 方便管道跟进。

```sh
# 单跑：撞到 provider 5xx 自动退避重试 3 次
tiny-agent --config examples/harness.json --task "跑测试" --retries 3 --output result.json
```

## 会话持久化

- `--session <path>` 指定会话文件（默认 `./.tinyagent-session.json`），每轮结束原子落盘
- `--session none` 关闭持久化——不读不写，一次性 headless 运行不在 cwd 留文件（与 `--resume` 互斥）
- `--resume` 加载历史继续；此时 config 里的 `task` 视为已消费，用 `--task` 追加新指令
- 崩溃也不丢已完成的轮次；TUI 模式同样生效
- TUI `--resume` 会把历史**转录进原生 scrollback**（用户/助手消息、工具调用与结果按 id 配对重建单行视图，`· ` 系统行标注"resumed"），图片不转录；停在 tool_result 中途的会话恢复后自动续跑，末条任务文本直接可见

## TUI 对话模式

需要 `--features tui` 编译。`--tui` 进入一种**内联/流式**渲染（与 Claude Code / Codex 同款）：已定稿的对话内容**提交进终端的原生 scrollback**，屏幕底部只留一个固定高度的小 live 区（流式区 + 输入框 + 状态行）每帧重绘——所以**终端原生的文本选择 / 滚动 / 搜索都直接作用于真实对话**，旧全屏重绘模型里"拖选一段、内容一滚选区就漂"的问题不复存在。默认即 **输入模式**：

| 键 | 动作 |
|---|---|
| `Enter` | 发送（运行中则为排队，当前轮结束后自动发出；补全弹窗打开时为补全选中命令，不发送）；行尾为 `\` 时改为换行（行续接，bash 语义） |
| `Alt+Enter` / `Shift+Enter` | 输入换行（多行） |
| `Backspace` | 删前字符（跨行合并上一行） |
| `Ctrl-D` | 删光标后字符（跨行合并下一行）；空输入且空闲 → 退出 |
| `←`/`→`/`Home`/`End` | 行内移动 |
| `Alt+←`/`Alt+→` | 按词移动（空白分词；CJK 整段非空白算一词） |
| `Ctrl-W` | 删前一词（行首退化为合并上一行） |
| `Ctrl-U` / `Ctrl-K` | 删到行首 / 删到行尾 |
| `Ctrl-A` / `Ctrl-E` | 移到行首 / 行尾 |
| `↑`/`↓` | 单行输入（含空输入）或正在浏览历史时召回上/下一条历史；多行输入时跨行移动（补全弹窗打开时为移动弹窗选中） |
| `Alt+↑`/`Alt+↓` | 输入历史翻页（任何输入状态下可用） |
| `Ctrl-C` | 忙 → 取消当前轮（同时丢弃排队消息）；空闲 → 连按两次退出（2 秒内再按一次，首按只提示） |
| `Esc` | 忙 → 取消当前轮；空闲 → 清空输入框（补全弹窗打开时只关弹窗）。退出用 `/quit`、`/exit`，或空闲连按两次 `Ctrl-C` |

运行中（running）输入框保持可编辑：可以继续打字，`Enter` 会把消息排队——live 区显示暗色的
`❯ … · queued` 占位行，输入框标题同步显示队列长度；当前轮结束后最老的
占位行换成真实消息并自动发送；`Ctrl-C` 取消当前轮会**连同排队消息一起丢弃**；`/clear` 与 `/context` 在运行中禁用
（`/context` 要向持有 agent 的 worker 线程查询），其余斜杠命令（`/export`、`/out`、`/model`、`/limits`、`/tools`、`/system`、`/help`，以及退出用的 `/quit` / `/exit`）照常可用。

**原生选择 / 滚动 / 搜索**：已定稿的对话（你的消息、助手回复、工具调用与结果、系统行）都在终端
scrollback 里，是真正的终端文本——鼠标拖选即选中原文，复制走终端自身机制（macOS 上选中即复制）；
滚轮 / `Shift`+滚轮 / `PageUp`/`PageDown` / `Cmd-F`、`Ctrl-F` 搜索都是终端自带能力，翻到的就是上面
已定稿的真实对话。应用不再托管滚动或选区，所以全屏重绘模型的选区漂移问题在这里不存在。流式中的
在途助手回复暂留在底部 live 区（带 `▌` 光标）。**长回复增量定稿**：在途内容超出 live 区高度后，
已完结的段落会**提前提交进 scrollback**；没有段落边界可切的超长单段（LLM 常见的"一整段一行"散文）
按**显示行**切分，内容以行为单位连续向上滚出（与 Claude Code 同款观感），live 区只保留正在生成的
尾部两三行（思考区块同理；已提前定稿的行不再随后续重排）。
空对话（或 `/clear` 之后）live 区显示暗色按键速览（`/help`、补全、原生滚动 / 选择 / 复制）。
对话正文在宽屏下限制为 120 列并左对齐，角色用行内图标区分——你的消息以青色 `❯ ` 开头、助手的
以绿色 `● ` 开头（无独立标题行，每条消息省一行）；工具调用和结果缩进归属于助手，各占一行单行预览
（工具调用显示紧凑参数摘要、工具结果显示输出预览）；失败且输出不超过 20 行的结果自动展开方便直接
看到报错，超长输出保持折叠，成功结果要展开时用 `/out` 重印全文。输入框与状态行使用近全宽布局；
running 时输入框标题显示 spinner、当前活动（thinking / writing / → 工具名）、本轮耗时、取消提示和
队列长度（终端过窄放不下时优先省略活动名，保留耗时与取消提示），整个输入框边框随运行状态变色；
空闲时标题为 ` Ask TinyAgent `。
一轮开始还没有可见输出时，live 区显示瞬时的 `● …` 占位（渲染期合成，不进对话历史），首个文本或
工具调用出现即消失。推理模型的思考过程单独渲染为暗色斜体的 `✻` 区块——不走 markdown、和答复正文
视觉分离，流式期间带 `▌` 光标、输入框标题显示 thinking；答复开始后 ✻ 区块定稿进 scrollback，答复以
正常样式另起一块（子 agent 的思考带 `[名字]` 标注）。`/export` 导出时思考以 `✻ Thinking` 段落收录。
状态行左侧只保留模型、整体运行状态（running / idle，无计时）、上下文占用和累计 token（如
`1.2k in · 0.8k out`）；右侧右对齐显示当前目录的完整路径
（并行开多个窗口时一眼区分，过长时左侧截断加 `…` 前缀，剩余不足 12 列则整段隐藏——窄屏时它最先消失）。
上下文占用显示为 `ctx 54.7k`；在配置的
`limits.max_context_tokens` 里设了模型上下文窗口后变为 `ctx 21% (54.7k/256k)`，占用 ≥60% 转黄、
≥80% 转红。agent 用过 `todo` 后，状态条另显一段 `plan done/total`（如 `plan 2/5`，全部完成转绿、
进行中为黄），实时反映任务计划进度——完整 checklist 仍在最近一条 `todo` 工具结果行里。
终端变窄时状态条先省略 token、再省略 plan，上下文段保留到最后，只保留模型和运行状态为止。退出
（`/quit`、`/exit`，或空闲 `Ctrl-C` 连按两次）时若仍有排队消息，终端还原后会在 stderr 提示被丢弃的条数。
一轮正常结束（end_turn / stop 等）不再打印 `[done: …]` 行，只有异常停止（max_tokens /
length / error 等）才显示警告。工具输出里的 ANSI 颜色序列（如 git diff 的红绿）会按
原样着色显示，其余控制序列一律剥除；`/export` 导出的则是去掉转义码的纯文本。
`/` 开头走斜杠命令（不发给模型）。输入以 `/` 开头且还在敲命令名（未输入空格）时，输入框上方会浮出
补全弹窗：按前缀过滤（大小写敏感），`↑`/`↓` 移动选中（循环），`Tab`/`Enter` 把选中命令补全进输入框
（`/export` 自动带尾空格方便输路径；补全只是填入，再按一次 `Enter` 才执行，防止误触发 `/clear`），
`Esc` 只关弹窗。输入空格进入参数阶段、删掉 `/` 或无匹配时弹窗自动关闭；暂不支持鼠标点选；运行中
同样可用（命令照常即时执行，消息照旧排队）：

| 命令 | 作用 |
|---|---|
| `/clear` | 清空对话历史（不进输入历史） |
| `/export [path]` | 导出对话到文件（无路径时复制到剪贴板） |
| `/out [n]` | 重印第 n 新的工具结果全文（默认最新一条；错误结果同样可展开） |
| `/model` `/limits` `/tools` `/system` | 只读查看当前配置 |
| `/context` | 上下文构成报表（估算 token：system 三段 / 工具四类 / 消息分型 + 校准状态与窗口余量） |
| `/help` | 命令列表 |

取消机制：`Ctrl-C` 在流式 chunk 间（≤200ms）生效，取消后可继续发下一条——内核 `run()` 不清停止标志（电平契约，嵌入方在受理点负责清），TUI 在提交每条新输入时自动清掉上一轮的残留停止。

多行粘贴：终端开启 bracketed paste，一次粘贴含换行的多行会被当作**单条**消息，不会因换行被提前发送——直接粘贴即可，无需手动改 Alt+Enter。手动输入多行除了 `Alt+Enter` / `Shift+Enter`，也可在行尾打 `\` 再按 `Enter` 换行（行续接）——终端不上报修饰键时（如 macOS Terminal.app）这是唯一的手动多行入口。

浅色终端适配：`TINYAGENT_TUI_THEME=light` 启动 TUI，系统行 / 预览 / 状态段等暗色文字换用更深的中灰（深浅底都可读），行内代码从白色改染青色（浅色底上白字几乎隐形）。未设置或其它值保持深色主题不变；不做自动探测（终端背景色上报不可靠），需要浅色就显式设这个环境变量。

## MCP 客户端

配置里声明 MCP 服务器，其工具以 `mcp_<server>_<tool>` 命名注册：

```json
"mcp_servers": [
  {"name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
   "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "可选，stdio 注入的环境变量"}},
  {"name": "remote", "url": "http://host:3000/mcp", "transport": "auto",
   "headers": {"Authorization": "Bearer xxx"}}
]
```

stdio server 可注入环境变量（`env`，叠加在宿主环境之上、同名以配置为准）——GUI 应用从 Finder/Dock 启动没有 shell 环境，靠 env 传凭据的 server（GitHub MCP 的 `GITHUB_PERSONAL_ACCESS_TOKEN`、数据库连接串、各类 `XXX_API_KEY`）必须走这条。`env` 仅对 stdio 生效，HTTP 传输忽略它。社区主流 MCP 配置（Claude Desktop / Code、VS Code）都含此字段，复制过来即可用。

支持三种传输（`command` 与 `url` 二选一）：

工具名以 `mcp_<server>_<tool>` 注册给模型，非 `[A-Za-z0-9_]` 的字符（空格/点/符号）消毒成 `_`
并折叠，使名字始终是模型可稳定输出的标识符；**实际分发用的 wire 名保持原样**（`tools/call`
仍按服务器原始工具名调用），所以消毒只影响模型看到的名字。若消毒后两个工具撞名
（如 server `a`+tool `b.c` 与 `a.b`+`c` 同得 `mcp_a_b_c`），启动时报错而非静默覆盖。

| 传输 | 说明 |
|---|---|
| **stdio** | `command` + `args`，spawn 本地子进程（NDJSON / JSON-RPC 2.0） |
| **http** | Streamable HTTP（2025-03-26 现行标准）：单端点 POST，支持 JSON / SSE 响应、Mcp-Session-Id；202 异步响应经常驻 GET 流收回（server→client 推送）。长连接 GET 在服务器不关流时无法中断，teardown 会显式 diag 警告「读取线程随进程退出回收」 |
| **sse** | 旧版 HTTP+SSE（2024-11-05 已废弃协议）：GET 收事件流 + endpoint POST。注意其长连接 GET 在服务器不关流时读取线程只能等进程退出回收（teardown 同样有 diag 警告）——长驻批量作业优先 stdio / http |
| **auto**（默认） | 先按 Streamable HTTP 探测，失败自动回落旧版 SSE |

不支持的边角：server-initiated request（sampling、roots/list 等，带 `method` 的请求）会被丢弃——本客户端只消费响应帧。`examples/mcp_echo.py`（stdio）与 `examples/mcp_http_echo.py`（/mcp + /sse 双端点）是零依赖测试服务器，也是协议参考实现。内置工具白名单（`tools`）与 MCP 工具互不影响。

### 运行时热增删（库 API）

config 在启动时定格 server 集合；库宿主（GUI、长驻服务）可以在**轮次间**按需增删，不重建 Agent（重建要重跑所有幸存 server 的握手）：

```rust
use tiny_agent::{build_agent_with, McpServerConfig};

// 用户刚在设置里启了一个 server：
let added = agent.add_mcp_server(McpServerConfig {
    name: "web".into(),
    command: Some("duckduckgo-mcp-server".into()),
    args: vec![],
    url: None, transport: None, headers: Default::default(), env: Default::default(),
})?;
// added == ["mcp_web_search", "mcp_web_fetch_content"]，下一次请求即生效

let names = agent.tool_names();          // 当前全部工具名（内置 + MCP + skill 注入）
let removed = agent.remove_mcp_server("web")?;   // 按 CONFIG name 删，返回移除的工具数
```

语义要点：

- **撞名是可恢复错误**：新 server 的消毒名与现有任何工具（或新 server 内部）冲突 → `Err`，刚 spawn 的子进程随即被杀（回滚），Agent 状态与调用前完全一致——不会部分生效。启动路径同款撞名在构建时报错。
- **不打印**：与启动时的 `mcp: … ready` 日志不同，热增删全程无输出（宿主可能在 raw-mode TUI 下打印会花屏），成败只看返回值。
- **`task` 子 agent 不感知**：`task` 工具持构造期 MCP 快照，热增的 server 对本 Agent 生命周期内任何时点 spawn 的子 agent 都不可见（共享该槽位留给后续版本）。移除一个仍被子 agent 快照持有的 server 时，连接活到最后一个持有者释放。
- **`run_code` guest registry 同样不感知**（`code` feature）：沙箱内工具注册表按构造期快照惰性构建一次（跨克隆共享缓存），与 `task` 子 agent 同一条文档化边界——热增的 server 在 JS 里看不到；热删的 server 仍可被已构建的 registry 调用（连接活到 registry 释放，最坏在退出时等满一次 MCP 调用超时后由 Drop 杀进程组，不残留孤儿）。需要沙箱与热增删严格同步的宿主，重建 Agent 而非热增删。
- **remove 按配置名**：`remove_mcp_server("web")` 的参数是 `McpServerConfig.name`，不是消毒后的 `mcp_web_search` 展示名；没有该 server 的工具时返回 `Err`（拼写错误不会静默无操作）。stdio 子进程在最后一个持有者 drop 时被同步杀掉（进程组），不残留孤儿。
- **shutdown 联动**：若已调过 `set_shutdown`（会话结束 teardown），热增的工具自动挂上同一 reaper——临近退出时增的 server 不会被遗留。
- 同款 `&mut self` 顺序契约：run 持有借用，「轮次间调用」由借用检查器强制（见「[库 API](#库-api程序化调用)」的热更新段）。

### 第一个 TinyAgent MCP 工具

想给 agent 加一个自定义能力，MCP 是「不改核心、不编译」的路子——任何语言的进程只要会说 stdio 上的 JSON-RPC 2.0，就是一个工具服务器。最小例子（Python，零依赖；完整文件见 `examples/mcp_echo.py`）：

```python
import json, sys

TOOLS = [{"name": "echo", "description": "Echo back the given text.",
          "inputSchema": {"type": "object",
                          "properties": {"text": {"type": "string"}}, "required": ["text"]}}]

def reply(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method, msg_id = msg.get("method"), msg.get("id")
    if method == "initialize":
        reply(msg_id, {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}},
                       "serverInfo": {"name": "mcp-echo", "version": "0.1.0"}})
    elif method == "tools/list":
        reply(msg_id, {"tools": TOOLS})
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        reply(msg_id, {"content": [{"type": "text", "text": f"echo: {args.get('text', '')}"}],
                       "isError": False})
```

存成 `my_server.py`，config 里声明即接入——工具以 `mcp_echo_echo` 注册给模型：

```json
"mcp_servers": [
  {"name": "echo", "command": "python3", "args": ["my_server.py"]}
]
```

三条协议要点：`initialize` 必须回 `protocolVersion` + `serverInfo`；`tools/list` 回工具名/描述/inputSchema；`tools/call` 回 `content` 数组（`type: "text"`）+ `isError`。没有 `id` 的行是通知，忽略即可。需要凭据就加 `env` 注入（见上）。

### 联网搜索 / 抓取（Web Search MCP）

给 agent 加联网能力，最省事的是挂一个搜索类 MCP server——核心不改，config 里声明即可。**推荐 DuckDuckGo MCP（`duckduckgo-mcp-server`）**：免费、无需 API key，中英文搜索质量都不错，注册两个工具：

- `mcp_web_search(query, max_results, region)` — 搜索，返回标题 / URL / 摘要
- `mcp_web_fetch_content(url, start_index, max_length)` — 抓取网页正文（去导航，支持分页）

```sh
# 一次性安装（装成持久命令；uvx 直跑在国内会卡 PyPI 索引解析，装成工具则冷启动 ~2s）
uv tool install duckduckgo-mcp-server        # 国内前置 UV_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
```

```json
"mcp_servers": [
  {"name": "web", "command": "duckduckgo-mcp-server", "args": []}
]
```

工具注册为 `mcp_web_search` / `mcp_web_fetch_content`（`mcp_<server>_<tool>`）。完整可跑示例见 `examples/web-search.json`：

```sh
tiny-agent --config examples/web-search.json --task "搜一下 Anthropic 最近发布的模型，给出名称和发布日期"
```

> **备选：SearXNG MCP**（`ihor-sokoliuk/mcp-searxng`）——完全自有索引、无第三方抓取、无反爬 / 隐私顾虑，但需先自托管一个 SearXNG 实例 + MCP 桥（两层容器），部署成本更高。适合生产 / 隐私场景；日常开箱用 DuckDuckGo 即可。

## 配置（`config.json`）

```json
{
  "provider": "anthropic | openai",
  "base_url": "可选，默认官方端点；openai 模式可指向本地 vLLM/Ollama",
  "api_key_env": "ANTHROPIC_API_KEY",
  "model": "claude-haiku-4-5-20251001",
  "max_tokens": 8192,
  "system_prompt": "可选系统提示词（通用 agentic 纪律参考见 examples/prompts/agentic.md）",
  "task": "headless 模式要执行的任务（可被 --task 覆盖）",
  "workdir": "工具操作的工作目录，默认当前目录",
  "restrict_to_workdir": false,
  "inject_environment": true,
  "tools": ["shell", "read_file", "write_file", "edit_file", "list_dir", "load_image"],
  "mcp_servers": [],
  "code": {"enabled": false},
  "reasoning_effort": "可选，思考程度字符串原样透传（none/low/medium/high/xhigh/max），不校验",
  "limits": {
    "max_turns": 30,
    "timeout_secs": 600,
    "max_output_chars": 30000,
    "compress_threshold_chars": 400000,
    "keep_recent_messages": 8,
    "max_context_tokens": 0,
    "max_total_tokens": 0,
    "stream_idle_secs": 30,
    "shell_timeout_secs": 120,
    "subagent_max_turns": 25
  }
}
```

API key 从 `api_key_env` 指定的环境变量读取，不写进配置文件。

`inject_environment`（默认 `true`）：构造 agent 时在 system prompt **最前面**插入一段运行环境事实：

```
# Environment
- OS: macos
- Shell: sh -c
- Working directory: /abs/path
```

让模型知道自己跑在哪个平台、据此下发兼容命令（如得知是 `cmd /C` 而非 bash，就不会写 `$((...))` 这种在 cmd 下**静默失效、还 exit 0**、看着像成功实则是哑火的语法）。**只注入 OS / Shell / 工作目录三项事实，不写死语法示例**——事实来自运行时（`std::env::consts::OS`）与配置，跨平台自动正确；Shell 取 `shell` 工具真实调用的形式（unix `sh -c`、windows `cmd /C`）；工作目录取已解析的 `workdir`。注入点在 `Agent::new` 单点（主 agent 与 `task` 子 agent 都覆盖——子 agent 也跑 shell，同样需要平台事实），调用方自定义的 `system_prompt` 完整保留在事实段之后。设 `false` 关闭。批量任务片段可覆盖（软字段，与 `reasoning_effort` 同类）。

`limits.stream_idle_secs`（默认 30）：流式响应两个 chunk 之间最长允许的静默秒数，超过即判定连接半开并中断。普通对话模型 chunk 间隔在亚秒级，30s 绰绰有余；**深度推理模型**（DeepSeek-R1、o 系列等）首 token 前可能静默数分钟，需要调大（如 180）才不会被误杀——`limits.timeout_secs` 仍是**模型请求层面**的软上限（约束每轮 chat 请求与轮次边界），不会因此无限挂起。

`limits.timeout_secs`（默认 `0` = 不限）：整个 run 的墙钟**软上限**——约束每轮 chat 请求（单请求上限 = 剩余时间）与轮次边界（不再开新轮），`task` 子 agent 继承父的**剩余时间**（不会拿到完整第二份超时）。**不覆盖**的部分：工具执行各有独立上限（`shell_timeout_secs`、MCP 调用 120s），顶破上限那轮里已在跑的工具照常跑完（保 tool_use/tool_result 配对）；Python/Node 绑定的自定义 provider 回调是同步 host 调用，无法被强制中断（Node 侧等待上限已随剩余预算收紧，Python 侧靠 host 自律）。要硬墙钟请用外层 `timeout(1)` / 容器。

`limits.shell_timeout_secs`（默认 120）：单条 `shell` 命令的墙钟上限，超时杀掉整个进程组并在结果里记 `[command timed out after Ns and was killed]`。评测任务里的构建 / 安装（`cargo build`、`npm install`、训练脚本）常常超过 120s——任务会被砍死写进账单成 error，看似模型不行实则 harness 不行；这类任务调大它（或设 `0` = 单命令不限时，交互取消与 run 级 `timeout_secs` 兜底仍生效）。注意它是**成本旋钮而非安全边界**：这也是 limits 里**唯一允许批量任务片段双向覆盖**的字段（放宽/收紧都行）——想烧时间的任务用一万条快命令同样能烧，真正的闸门是 `timeout_secs`（单向收紧）与 `max_total_tokens`。

> **`timeout_secs: 0`（不设超时）时的单请求兜底**：即便关掉整轮超时，单次 chat 请求仍有一个 **10 分钟**的总时长上限（`STREAM_BACKSTOP`）。这是**泄漏兜底**而非功能限制——流式读取线程阻塞在 reqwest 的 `read()` 里、无法观测取消标志，若没有任何单请求上限，一条半开连接会让该线程及其 TCP 连接一直挂到进程退出。该上限只约束**单次请求**，从不约束**整轮 run**（一轮由多次请求组成），所以「不设超时」仍意味着 run 可以无限长，只是不会无限卡在单次请求上。设了 `timeout_secs > 0` 时此兜底不生效，单请求上限改由剩余预算推导。

`limits.max_total_tokens`（默认 `0` = 不限）：整个 run 的 token 预算上限（input + output + cache 写 + cache 读，跨所有轮次 / 子 agent / 压缩调用累计）。累计超过即以 `stop_reason: "budget_exceeded"`（exit 125）停下，不再开新一轮——语义同 `timeout_secs` 的**轮次边界**停：把总额顶破上限的那一轮照常跑完（工具执行、assistant 消息入历史），上限只阻止**下一轮**开始，故 transcript 与 tool_use/tool_result 配对不受影响。评测 / CI 场景用来给"跑飞钱包"装闸门：usage 累计本就含子 agent、压缩、失败轮（诚实成本，失败也计费），这个闸门只是在地基上加一道关。批量任务片段可设此项**只收紧**（更小值）；`0` 意为"无上限"（最松），任务片段设 0 会被忽略。
**预算在父与子 agent 之间共享执行**：闸门读的是 run 级共享花费计数器——父与每个在飞 `task` 子 agent 的直接 chat 花费（轮次 chat + 压缩摘要）实时累入同一计数器，父和每个子 agent 都在自己的轮次边界读它，并行子 agent **集体**触顶（不再是各烧满一份预算、join 后才发现）。粒度仍是**轮次边界**：同一轮里已并行启动的兄弟子 agent 互相看不到对方本轮的后续花费，顶破上限的那轮照常跑完；预算是"熔断"不是"预先分片"——严格控成本仍建议留余量或在 config 的 `tools` 里去掉 `task`。

`limits.subagent_max_turns`（默认 25）：`task` 子 agent 的轮次天花板，生效值是 `min(父 max_turns, 本值)`——子 agent 是**有界的支线**，没有天花板时一个卡住的子 agent 会一直烧父的循环预算，直到父自己的 `max_turns` 用尽；父的 run 仍是真正的墙钟护栏（共享取消标志）。设 `0` = **不另设天花板**（子 agent 原样继承父的 `max_turns`）——这是最松取值，故批量任务片段只能**收紧**（更小值），片段设 0 会被忽略、放宽会被 `min` 拒绝。子 agent 的轮次**永不高于父**（本值只降不升）。GUI/长工作流宿主把父 `max_turns` 放宽到千级时，记得同步调大本值，否则子 agent 会被默认的 25 掐回去。

`reasoning_effort`（可选字符串，默认不设）：思考程度，**按字符串原样透传给 provider，不做枚举校验**——各家档位碎片化且随模型代际变动，TinyAgent 不维护映射表，由 provider/server 拒绝未知值。取值参考：OpenAI `none`/`low`/`medium`/`high`/`xhigh`/`max`（随模型代际扩张）、Anthropic `low`/`medium`/`high`/`max`（Claude 4.6+ adaptive thinking；Opus 4.7 起旧 `budget_tokens` 已移除）、DeepSeek `low`/`high`/`xhigh`/`max`、GLM 仅开关（`enabled`/`disabled`）。映射：OpenAI 系直接发 `reasoning_effort` 字段（老网关不认会 400，`body_fix_for_400` 自动 drop 重试，与 `max_tokens`/`stream_options` 同款）；Anthropic 发 `thinking: {"type": "adaptive"}` + `output_config: {"effort": "..."}`（effort 在**独立顶层 `output_config` 对象**里，不在 thinking 内、也不是裸顶层字段——裸顶层会被静默忽略，这是该 API 最常见的坑），`"none"` 发 `{"type": "disabled"}`。

**思考内容透出**：推理模型的思考过程作为 `ThinkingDelta` 事件流式透出（OpenAI 系 `delta.reasoning_content`、Anthropic `thinking` content block），`--json` 下为 `{"type":"thinking","delta":...}`；**不进对话历史正文与上下文回传**（各家 API 也不要求回传，故只作事件透出给 UI）。GUI（如 TinyWorker）据此渲染可折叠"思考过程"区块；headless 终端把思考写到 **stderr**（不污染 stdout 的最终回复 / `summary`）；TUI 渲染为**独立的暗色斜体 `✻` 区块**（不走 markdown、与答复正文视觉分离，流式期间输入框标题显示 thinking，结束后随答复一起定稿进 scrollback）。批量任务片段可覆盖此项（软字段，与 `model`/`max_tokens` 同类，不在 `forbidden_batch_override`）。

### 上下文压缩

长任务中历史消息会持续增长直至撞模型上下文上限。压缩是**分层漏斗**——先零成本剪枝，再 LLM 摘要，
三层触发路径共用后两层：

0. **剪枝（prune，LLM 摘要前的零成本闸门）**——保护窗（`keep_recent_messages`）之外的**旧工具结果**
   是上下文的大头（实测可占 80%+）：超过 2000 字符的旧 `tool_result` 被替换为一行占位符
   （`[pruned tool output, was N chars]`，`tool_use_id`/错误标志保留、图片载荷丢弃、todo 结果豁免），
   配对关系天然完整，模型需要细节时可重跑工具。释放量不足 20000 字符不重写（避免为小收益打断
   prompt cache 前缀）。preflight 超阈值先剪枝重估，常常就此够用、**一次摘要调用都不发**；溢出兜底
   也先剪枝重试，仍溢出才走摘要强压。剪枝事件 `[context pruned: N messages …, M chars freed]`
   单独透出（`--json` 为 `{"type":"pruned",…}`），被剪的原始消息进 session 归档（见下）。

1. **前馈（preflight）**——每轮调用前预估请求大小，超阈值就走摘要压缩再用更短的历史发请求。阈值二选一：
   - 设了 `max_context_tokens`（>0）：token 估算到窗口的 ~85% 就压（另预留本回合 `max_tokens` 输出）。
     估算**优先锚定 provider 实测值**：上一轮响应报告的 prompt tokens（input + 缓存写 + 缓存读）
     作为基线，只对之后新增的消息做 ≈4 字符/token 的增量估算——这修正了纯字符启发式对中文
     低估 2~3 倍、preflight 过晚触发的问题。无锚点（首轮/恢复会话/历史被剪枝或折叠改写）时
     回退全历史估算。这是「按模型真实窗口压」，推荐。锚点同款校准也驱动 `context_breakdown()`
     的 `calibration`（见库 API 节），可用它观察压缩前后的构成变化。
   - 未设（`0`，默认）：回退 `compress_threshold_chars`（默认 400k 字符 ≈100k tokens，`0` = 关闭）的字符启发式。
2. **溢出（overflow）兜底**——**永远开启，与上面配置无关**。当 provider 返回「上下文过长」类错误
   （`context_length_exceeded` / `prompt is too long` / `maximum context length` 等，子串匹配错误体）
   时，先剪枝重试，仍溢出则强制摘要压缩一次并**重试该回合一次**；小窗口场景下折叠后仍溢出
   （保留尾部本身就超窗）时，再以**递减保护窗**（减半 → 只保最后一条）剪枝做最后一次重试；
   用尽恢复预算仍溢出才当真错误上抛。这样即便前馈估算偏了、或工具 schema / system prompt 占了
   大头，agent 也能自愈而不是整轮失败。摘要请求自身对**瞬态错误**（网络抖动/5xx）退避 ~1s 重试
   一次，一次丢包不再废掉整个 run 的压缩。

3. **宿主主动（manual）**——库用户在轮次间调 `Agent::compact()` / `compact_with(events, Some("focus …"))`
   （见「库 API」节，Python/Node 绑定同款 `compact(instructions?, on_event?)`）：GUI 的上下文
   占用指示到 80%+ 时点一下即压，不等阈值也不赌溢出；焦点指令引导摘要侧重（如"重点保留 API 变更"）。
   摘要 usage 返回给宿主记账，不进下一次 run 的账单。

摘要质量上：模型把中段历史写成**结构化滚动摘要**（目标/进展/决策/改动文件/报错与解法/下一步；关键
路径/标识符/错误文本要求**逐字引用**，抑制多次压缩的累积漂移），折叠进首条任务消息，只保留任务 +
最近 `keep_recent_messages` 条原文；后续压缩是**更新**这份摘要而非重摘要整个前缀。前缀过大时按消息
边界**分块**摘要再合并，把单次压缩比控制在 ~6:1，保住路径/报错等细节。切割点选在 assistant 消息边界，
保证 tool_use/tool_result 配对不被切断；压缩失败时告警一次并禁用本次运行的后续压缩（不中断任务，
也不每轮重试刷屏）。

**折叠后的保真**（对齐 Claude Code / Codex 的压缩后恢复）：随摘要一起折回头部的还有两样结构化内容——
(a) **todo 清单**：`todo` 工具没有读取模式，模型对计划的唯一视图是最近一次 todo 结果的渲染清单，折叠
吞掉它时按最新调用恢复（尾部仍持有活动清单时不重复注入）；(b) **中间用户消息原文**：被折叠区间的用户
Text 块以 `[用户消息原文保留]` 块逐字并入头部（单条 2000 字符截断、总额 12000 字符 FIFO），用户指令
不经摘要转述。运行日志与 TUI 系统行里的压缩事件带触发路径标注（`[context compressed (preflight):
dropped N messages, M kept]` / `(overflow)` / `(manual)`，`--json` 的 NDJSON 里则是额外的
`"trigger": "preflight"|"overflow"|"manual"` 字段），一眼区分是估算超阈值、溢出兜底还是宿主主动。

**非破坏性与 session 归档**：压缩折叠与剪枝移出活历史的**原始消息**（带稳定 id）追加进 session 文件的
`archive` 段（上限 4096 条 FIFO；`/clear` 保留归档），宿主可经 `session::load_with_archive` 读回做
undo/审计；普通 `load` 不受影响，老版本 session 文件双向兼容（无归档键即空归档，空归档不写出该键）。
id 语义：折叠消息的 id 随原件迁入归档（离开活历史）；剪枝消息的**原件**与活历史中的占位符版本共享
同一 id——宿主按 id 挂的资产仍指向那条消息（锚点不漂移），归档件是"它原来的内容"（按 id 可还原）；
同 id 只归档一次（后到的占位符版本不重复入档）；resume 后新铸的 id 从不复用归档里已有的编号。

压缩摘要请求自身的 token 用量也计入运行累计（`input_tokens` / `output_tokens`），
成本数字与实际调用一致，不因压缩而漏报。

### Prompt caching

Anthropic 原生 provider 默认开启 prompt caching：每个请求自动在 **system prompt / tools / 末条消息**
三处打 ephemeral 断点（≤4 断点上限内），多轮任务重发整段历史时只对增量计费（缓存写 1.25×、读 0.1×）。
断点低于 ~1024 token 时 API 自动忽略（不报错不收费），所以始终开启是安全的。OpenAI 兼容协议是隐式
前缀缓存（请求体无字段），命中数体现在 `cache_read_input_tokens`。

缓存命中的 token 单独计入：`input_tokens` 只含**未缓存**部分，缓存写/读分别走
`cache_creation_input_tokens` / `cache_read_input_tokens`（result.json / NDJSON / TUI 状态条都可见）。
真实计费输入 ≈ `input_tokens + cache_creation_input_tokens × 1.25 + cache_read_input_tokens × 0.1`。
压缩摘要请求是一次性调用，不加缓存断点。

## ⚠️ 安全说明

**这是一个给模型直接执行能力的 agent，不是沙箱。** 请按此定位使用（发现安全问题请按
[SECURITY.md](SECURITY.md) 的渠道私下报告，勿开公开 issue）：

- `shell` 工具原样执行模型生成的命令（`sh -c` / `cmd /C`），无任何命令过滤。
  模型可以 `rm -rf`、`curl ... | sh`、访问网络——与其赋予进程的权限一致。
- 文件工具默认不限制路径：`read_file ../../etc/passwd`、
  `write_file /etc/cron.d/x` 都会被执行。
- **在 CI / 评测 pipeline 中批量运行不可信的第三方 task 时**，务必：
  1. 在容器或受限用户下运行 tiny-agent；
  2. 配置 `"restrict_to_workdir": true`，文件工具将拒绝逃逸 `workdir`
     的路径（`../` 与绝对路径，对新建文件同样生效）。注意它是 **best-effort
     而非硬边界**：判定基于 canonicalize，检查与实际 I/O 之间存在 TOCTOU
     窗口——防的是模型顺手 `../` 走错门，不是对抗并发改目录结构的恶意进程；
     硬边界仍是容器；
  3. 必要时用 `"tools"` 白名单去掉 `shell`。

### 信任边界（哪些输入是受信的）

上面管的是**模型输出**这半张威胁面；另外半张是**加载的本地文件**。以下文件在
TinyAgent 的威胁模型里视为**受信任输入**（等价于代码本身——来源不明就不要加载）：

- **配置文件**：`base_url` 决定 API key 发往哪里（加载来路不明的 config 等于把
  `api_key_env` 指向的凭据交给该 URL）；`tools` / `mcp_servers` 决定哪些工具可
  用、哪些子进程被拉起。`--config` / 绑定的 config 路径请确认来源。
- **session 文件**：`--resume` 把历史原样重放进模型上下文（含 tool_use /
  tool_result 配对）——被篡改的 session 等同攻击者代笔的历史，模型照单全收。
  session 需要跨机器流转时自行校验完整性。
- **技能目录**（`~/.config/tinyagent/skills/` 与 `<workdir>/.tinyagent/skills/`）：
  SKILL.md 简介常驻 system prompt、正文与资源可被读进历史——安装一个技能等于
  安装一段 prompt（含可被模型照指示执行的脚本）。

不在这张信任面里的**运行时不可信输入**（MCP server 响应、provider SSE 流、
技能目录内容）有尺寸上限防御（tools/list 条数与 schema 上限、SSE 单行 16MiB、
skill 扫描读取 64KiB 等）：不可信的 server / 目录最多拖慢，不能 OOM 进程。
另注：OpenAI 400 自适应修复（learned fixes）只活在 provider 实例内存中——
每次 run 重建、不落盘、跨进程不共享；同一网关的新 run 会重新学一遍（每类 400
多一次试错往返），这是刻意的（落盘会带来跨 run 的毒化面，见 record-on-success
语义）。

## Non-goals（明确不做）

TinyAgent 的定位是**可审计、可无人值守的 agent 执行单元**——一个跑完一定留下完整、诚实、机器可读账单的一次性进程。下面这些方向与该定位冲突，明确不做（把力气挡在护城河上）：

- **不做 `--serve` / HTTP 服务模式**：长驻、多租户、鉴权是另一个产品，与"一次性执行单元"直接冲突。要集成的用户用 `--json`（NDJSON 流）+ exit code 包一层即可。
- **不给 `shell` 自建沙箱 / 命令过滤**：半吊子命令过滤只制造虚假安全感，隔离的官方答案仍是**在容器或受限用户下运行**（见上文安全说明）。（`run_code` 不在此列也不破此例：它是 wasm **能力构造性**沙箱——不是给既有工具套过滤器，而是新建一个“没有授予就没有能力”的执行面，网络 / 文件系统在 guest 里根本不存在，见上文专节。库 API 的 Hooks 拦截层是给嵌入方接自己的审批/策略系统用的编译期扩展点，不是内置安全边界——威胁模型见 `src/hooks.rs`。）
- **不做插件系统**：工具是编译期的内置 + MCP，不引入运行时加载。（Hooks 与工具能力标记同理：编译期 trait 扩展，不加载动态代码。）
- **TUI 保持极简定位**：交互体验对齐主流终端 AI 工具（键位、浅色适配、结果查看等基础体验保证可用），但不做全屏 / 鼠标托管 / 主题系统这类结构性扩张——它是 dogfooding / 演示工具，对标全职迭代的 Claude Code / Codex，追平是负 ROI。

### 特性试金石

一个新特性只有让**无人值守执行**更**可信 / 可审计 / 更便宜 / 更可移植**时才进 backlog。用这把尺子挡掉"听起来很酷但偏定位"的诱惑。

## 结果文件（`--output result.json`）

```json
{
  "schema_version": 1,
  "success": true,
  "stop_reason": "end_turn",
  "model": "deepseek-chat",
  "provider": "openai",
  "turns": 2,
  "attempts": 3,
  "started_at": "2026-08-15T07:41:02Z",
  "finished_at": "2026-08-15T07:41:09Z",
  "elapsed_secs": 3.1,
  "input_tokens": 1234,
  "output_tokens": 567,
  "cache_creation_input_tokens": 300,
  "cache_read_input_tokens": 1500,
  "summary": "agent 的最终文本回复",
  "tool_calls": [
    { "name": "write_file", "input": {...}, "output": "wrote 42 bytes", "is_error": false }
  ],
  "plan": [
    { "subject": "跑测试", "status": "completed" },
    { "subject": "修失败用例", "status": "in_progress" },
    { "subject": "提交", "status": "pending" }
  ],
  "error": null
}
```

- `tool_calls[].output` 是工具返回的完整内容（`is_error` 标记是否为错误结果）
- **归因字段（`schema_version` / `model` / `provider` / `started_at` / `finished_at`）每次都写**——
  一份无法归因到模型 / 协议 / 时间的账单不可审计（`--model` 做 A/B 对照后两份结果必须能区分）。
  与 `plan`/`meta` 的稀疏后插不同，这五个键是**纯增量**（新键，旧键不动），按 key 取值的消费方不受影响。
  `model`/`provider` 来自实际运行的配置（批量任务片段覆盖了 `model` 就记覆盖后的值）；`provider` 记
  配置声明的协议族（anthropic / openai），绑定层注入自定义 provider 回调时仍是配置声明值。
- `plan`（可选）：agent 通过 `todo` 工具记录的任务计划与每步状态（末次调用的全量列表）。**仅当 agent 用过 `todo` 才出现**，否则该键不存在——不用 `todo` 的结果文件与启用前逐字节相同。`tool_calls[]` 里 `task`（子 agent）条目另带一个可选 `meta` 字段（`turns`/`tools`/`tokens`/`stop_reason` 的紧凑 trace），同理仅在子 agent 调用时出现
- `attempts` 是实际执行的 agent 运行次数（`>1` 表示发生了瞬态重试，见下文「单跑瞬态重试」）
- `input_tokens` / `output_tokens` 是本次运行累计的 token 用量；`cache_creation_input_tokens` /
  `cache_read_input_tokens` 是 prompt 缓存写/读累计（Anthropic 两者都报，OpenAI 只报读）。`input_tokens`
  只含未缓存部分，真实计费输入需加上缓存写 1.25× / 读 0.1×（见上文 Prompt caching）
- agent.run 返回 `Err`（如 provider 503）时仍会写结果文件：`{"success": false, "stop_reason": "error", "error": "..."}`；
  不可救的上下文溢出（强制压缩未能救回）记 `stop_reason: "context_overflow"`，确定性请求拒绝
  （401/403/404/400 等）记 `"request_error"`（两者都不进瞬态重试，exit code 仍为 1），与 batch report 同口径
- 结果文件本身写失败（如 `--output` 指向目录）：运行**成功**时退出码降为 1（避免 CI 读到缺失/陈旧的
  result.json 却看到 0）；运行本就失败（取消/超时等）时保留原退出码

进程 exit code：

| code | 含义 |
|---|---|
| `0` | 正常完成（`stop_reason == "end_turn"`）|
| `124` | 超时（`timeout`，沿用 GNU `timeout` 约定）|
| `125` | 预算超限（`budget_exceeded`，累计 token 触顶 `max_total_tokens`）|
| `130` | 用户取消（`cancelled`，128 + SIGINT）|
| `1` | 其它（`max_turns_reached` / `max_tokens` / provider 错误等）|

headless 模式下 `Ctrl-C` 走**优雅取消**：首次 Ctrl-C 在流式 chunk 间（≤200ms）中断，照常写好
`result.json`（`stop_reason = cancelled`）后 exit 130；再次 `Ctrl-C` 立即 exit 130（不等本轮结束）。
Linux/macOS 经 SIGINT、Windows 经 `SetConsoleCtrlHandler`，行为一致。
**批量模式**（`--batch`）同样生效：首次 SIGINT 后在跑的任务跑完、不再起新任务（未起的标记 `cancelled`），
照常合并写出 `--report` 后 exit 130；再次 `Ctrl-C` 立即 exit 130。一个例外：在跑任务的 **MCP server
会被立即关闭**（防第二次 Ctrl-C 硬退时子进程成孤儿）——依赖 MCP 工具的在跑任务可能因此提前失败。
TUI 模式下 `Ctrl-C` 是普通按键，按上面的按键表处理。

`--json` 模式额外在每个事件吐一行 NDJSON（`type`: `turn` / `text` / `thinking` / `tool_input` /
`tool_call` / `tool_result` / `usage` / `compressed` / `warning` / `subagent` / `todo` / `done`，
重试时另吐 `retry`），可在结果文件落盘前实时跟进进度。`tool_input` 是工具参数的流式增量
（`{"type":"tool_input","id","name","delta"}`，键为 tool_use id）：模型边构造参数边透出，
GUI 可渲染"正在构造调用…"（headless 终端不重复打印——完整 `tool_call` 行随后来）。

## 内置工具

| 工具 | 说明 |
|---|---|
| `shell` | 在工作目录执行命令（sh -c / cmd /C），超时后 kill 整个进程组（默认 120s，`limits.shell_timeout_secs` 可调、0=不限），输出有界读取并按 `max_output_chars` 截断；命令留下的、仍持有输出管道的后台进程在 2s 宽限后被回收并标注（想留后台进程运行请重定向其输出：`cmd >log 2>&1 &`，不持管道即不受影响） |
| `read_file` | 读文本文件，带行号，支持 offset/limit；流式读取，超大文件不会整段进内存；头部带 `[文件名: N 行 · M 字符]` 规模元数据；`format:"json"` 返回结构化 JSON（无前缀，带 totals/next_offset）、`raw:true` 返回 base64 字节窗（字节保真，供 run_code 沙箱读写二进制） |
| `write_file` | 全量重写文件，自动创建父目录 |
| `edit_file` | 精确替换：`old_string`→`new_string`，默认要求唯一匹配（`replace_all` 全替换）；匹配 0 或 >1 时报错引导补上下文，复用 `restrict_to_workdir` 沙箱 |
| `list_dir` | 列目录，最多 500 条 |
| `load_image` | 加载本地图片让模型"看到"（需视觉模型） |
| `task` | 把一块工作委派给一个**子 agent**（独立上下文 + 独立 turn 预算），只把子 agent 的最终摘要作为结果返回，保持主上下文清爽。详见下节 |
| `todo` | 记录/更新任务计划（全量替换语义，每次发完整列表），最终计划落进 result.json 的 `plan` 字段。模型可见的是可读 checklist，审计真相是结构化 input |
| `run_code` | 在进程内 wasm 沙箱（QuickJS）执行 JS，代码经 `tools.call()` 编排本会话工具（可选 `code` feature + `code.enabled` 双开关，默认关；见下文专节） |

## 子 agent（`task` 工具）

`task` 是一个子 agent 工具：主 agent 把一个聚焦的子任务交给一个全新的子 agent（自己的消息历史、自己的 turn 预算），子 agent 跑完后只把**最终摘要**作为 tool 结果返回——主 agent 的上下文不会被子 agent 的中间过程撑大。

- **输入**：`{ "name": "身份标识", "description": "简短说明", "prompt": "完整自包含的指令" }`（`name` 与 `prompt` 必填，`description` 可选、信息性）。`name` 是这个子 agent 的身份，所有转发出来的事件都用它打标签。
- **同 turn 多调用并行**：主 agent 在**一个** assistant turn 里发出多个 `task` 调用时，它们会**并行**跑（每个一个线程，按 8 个一波封顶，借线程池 scope），各自的事件经有界 mpsc 漏斗（背压）实时转发进主 run 的事件流——所以多个子 agent 协同工作时，输出是交错流式出现的。结果仍按原始块序拼回（N:N 配对不变）。叶子工具（`shell`/`read_file`/…）保持顺序执行，只有 `task` 并行，避免在文件/进程上引入新的竞态。
- **事件可见**：子 agent 的流式过程（文本增量 / tool 调用 / tool 结果）**实时转发**进主 run 的事件流，每条都裹上 `name` 身份——headless 的 `--json` 输出形如 `{"type":"subagent","name":..,"event":<子事件>}`，TUI 里子 agent 的输出也带 `[name]` 前缀。并行多个子 agent 时，靠 `name` 区分交错的事件。
- **成本诚实计入**：每个子 agent 的 token 用量折进主 run 的累计，并照常发 `usage` 事件 / 写进 `result.json`（并行的几个各自计入）。
- **审计 trace（`meta`）**：`result.json` 里每个 `task` 条目另带一个 `meta` 字段——子 agent 的轮数、用过哪些工具（按名计数）、token 小计、stop_reason 的紧凑记录。**不进模型可见的 tool result**（主上下文只收摘要，保持清爽），仅供审计并行工作。失败子 agent 也带 `meta`（`stop_reason: "error"`），看得出"跑到哪一步倒了"。
- **继承**：子 agent 复用主 agent 的 provider / model / 工作目录 / `restrict_to_workdir` / 取消标志（Ctrl-C 会连带中断在跑的子 agent）。工具白名单继承父配置。

**边界（刻意收紧）**：

- **并行仅限同 turn 的多个 `task` 调用**：一次 turn 内多个 `task` 并行；叶子工具顺序。工作目录隔离（worktree）不做——子 agent 共享主工作目录，约定并发子 agent 不写同一文件即可。
- **不可递归**：子 agent 的工具白名单会滤掉 `task` 本身，所以子 agent 不能再开子 agent（无需深度计数）。
- **默认启用**：`task` 在内置工具默认白名单中，TUI 对话或 headless 运行无需额外配置即可委派子任务。评测 / CI 若想锁定行为与成本，在 config 的 `tools` 里显式列出、去掉 `"task"`：

```json
"tools": ["shell", "read_file", "write_file", "edit_file", "list_dir"]
```

## 任务计划（`todo` 工具）

`todo` 让 agent 把任务计划结构化地记下来、随进度更新，最终落进 `result.json` 的 `plan` 字段——把"agent **打算**做什么、做到哪一步"和"agent **做了**什么"（`tool_calls`）一起写进同一份账单。

- **全量替换语义**：每次调用发**完整新列表**（不是增量），状态三选一——`pending` / `in_progress` / `completed`。模型可见的是一份可读 checklist（`☐`/`▶`/`✓` 前缀）作为 tool result；**审计真相是结构化的 input**，两者职责分离。
- **`plan` 字段的来源**：run 结束时，取 tool-call 日志里**最后一条非 error 的 `todo` 调用的 input** 作为最终计划。这个日志独立于对话历史（上下文压缩只重写 `messages`、不碰工具调用日志），所以**计划状态不怕被压缩吃掉**——这是它能稳定落进 `result.json` 的关键。模型没用过 `todo` 时，结果文件**不含 `plan` 键**（与启用前逐字节相同）。
- **实时进度**：每次 `todo` 调用经 `--json` 吐一条 `{"type":"todo","todos":[...]}`（headless 终端另在 stderr 打一行 `[plan] done/total`），管道可据此渲染实时进度。
- **默认启用，子 agent 不继承**：与 `task` 同在默认白名单；评测 / CI 想锁定行为在 `tools` 里去掉即可。子 agent 是聚焦短任务，`todo` 被白名单滤掉（规划是父 agent 的职责），其计划也不会进父的 `result.json.plan`。

```json
{ "todos": [
  { "subject": "复现失败", "status": "completed" },
  { "subject": "定位根因", "status": "in_progress" },
  { "subject": "写回归测试", "status": "pending" }
] }
```

## run_code（沙箱 JS 执行，可选 `code` feature）

**从这里开始**：[快速上手与最佳实践](docs/run_code_guide.md) ·
[可运行演示](examples/run-code-demo/README.md) ·
[模型内置技能全文](crates/core/src/tools/run_code_skill.md)。下面是配置与接口参考。

`run_code` 让模型把"需要循环 / 批量 / 多步数据处理"的逻辑写成一段 **JS**，在进程内的 wasm 沙箱
（QuickJS 编译为 wasm、wasmtime 驱动，纯 Rust 宿主不碰 C 工具链）里执行——代码经 `tools.call()`
回调 agent **既有**工具（文件工具、MCP 工具），一次调用完成本来要几十轮 shell + read_file 折腾的
编排；而能力面是**构造性收敛**的：沙箱没有网络、没有系统调用、没有文件系统直访，能触达的世界 =
被授予的工具 + 配置允许的路径。

**两层开关，默认都关**（信任与体积各自独立把关）：

1. **编译期**：`cargo build --features code`（wasmtime + 嵌入的 quickjs.wasm 资产，musl 静态构建同样可用）；
   模块机器码经磁盘缓存复用（`$XDG_CACHE_HOME` 或 `$HOME/.cache` 下的 `tinyagent/wasmtime/`，新进程
   不再重付懒编译；目录不可用时静默降级为无缓存）；
2. **运行期**：`"code": {"enabled": true}`——feature 编入但未开启时 `run_code` 不进工具表，模型不可见、零 schema 成本。

<!-- BEGIN CODE CONFIG SPEC（sync_run_code_contract.py 校验：默认值与 crates/core/src/config.rs 的
     CodeConfig::default 对齐 + 段落哈希入 manifest，防单侧漂移） -->
```json
"code": {
  "enabled": true,
  "skill_injection": "on_demand",
  "precheck": false,
  "include": false,
  "call_batch": false,
  "llm": {
    "enabled": false,
    "default_model": "judge-small",
    "models": ["judge-small", "judge-large"]
  },
  "deny_paths": [".env", "secrets/**"],
  "fs_write_dirs": [],
  "max_tool_calls": 200,
  "mem_mb": 256,
  "timeout_secs": 120,
  "fuel": 5000000000,
  "max_result_chars": 60000
}
```

| 字段 | 默认 | 语义 |
|---|---|---|
| `enabled` | `false` | 运行期开关；`run_code` 不在 `tools` 白名单语义内（开关即 membership，见下） |
| `skill_injection` | `"on_demand"` | 内置技能 `run-code` 的三态披露：`"on_demand"`（默认，模型经 `read_skill` 按需加载）；`"eager"`（装配时把技能正文以合成 read_skill 问答对预置进会话历史——微调/评测的轨迹对齐用，resume 的历史不重复注入）；`"off"`（内置技能关闭，`read_skill` 命中返回明确的「已被配置关闭」错误；磁盘同名覆盖仍照常服务） |
| `precheck` | `false` | 执行前静态预检：单遍 lexer 检出沙箱不支持的写法（`async`/`await`/`import`、未声明的 `Promise`/`setTimeout`/`fetch` 调用、`Date.now`/`Math.random`、`tools.call` 保留名 `run_code`/`task`/`todo`），命中即拒答并附修正指引，**代码不执行、无副作用、不占预算**。注意 `__tools_list`/`__budget` 不在检出集内——它们是可工作的只读内省拦截（`tools.budget()` 的底层通道） |
| `include` | `false` | 库通道开关（code_memory M1）：开启后 guest 增 `include(path)`（加载 `lib/` 下已登记代码库，幂等 + 循环截断，经 `__include` 保留名）与 `saveLib(path, src)`（写库 + `__hash_file` 宿主哈希登记 `lib/INDEX.md`，恒 `st=draft`）两个助手，及描述的【库】段；准入 = `st=ok` 或本会话登记过（clone 共享、不跨会话）。关闭时保留名明确拒绝、模型可见契约与无此特性的旧二进制一致。调用期生效（资源限制先例）；batch 片段只能关不能开 |
| `call_batch` | `false` | 批量通道开关（code_llm M2，评测门）：开启后 guest 增 `tools.callBatch([{name, args}, …])`（并发执行一批只读工具调用，经 `__call_batch` 保留名逐 entry 走与单次调用完全相同的闸门链；8/波扇出、按提交序返回数组、单项失败为 Error 条目不中止批次）与描述的【批量】段。只读面 = `batchable()` 工具（v1 白名单 `read_file` + `llm` + MCP `readOnlyHint`，缺注解为否）。关闭时保留名按未知工具报错、模型可见契约与旧二进制一致。调用期生效；batch 片段只能关不能开 |
| `llm` | `{enabled:false, default_model:null, models:[]}` | 内层模型判别开关（code_llm M3）：`llm` 嵌套对象三字段——`enabled`（默认关；开启后 guest 增 `llm` 判别原语，见下节）、`default_model`（开启时必填：省略 `model` 参数的内层调用用它）、`models`（显式 `model` 参数的允许列表，可含多档判别模型；`default_model` 必须在列表内）。开启时顶层工具表与该名撞名报错（名字保留）；注册表是构建期快照，`set_code_config` 热翻开关两个方向都不被感知——首次执行后改 `llm` 会经 diag 发一条明确的「已冻结、需重建 agent」警告（不再静默成功）。batch 片段不能改 `llm.enabled`/`default_model`，`models` 只能收紧（交集） |
| `deny_paths` | `.env`、密钥类目录等 | **经工具的**文件读写拒绝清单（glob：`*` 单段、`**` 跨段），定位**防误读不防恶意**。`shell` 命令不经此层（`cat .env` 照常，见下一行） |
| `fs_write_dirs` | `[]` = 整个 workdir 可写 | 经工具的写路径白名单（相对 workdir；交集语义，可收紧到无处可写）。**仅约束 4 个文件工具与 `copy_bytes`**——`shell`（含其重定向写入）不经此层与写侧 symlink 拒绝；敌对 guest 的对策是 `restrict_to_workdir` / hooks Deny / 从 `tools` 去 `shell`（A-8 读写两侧同界，有钉住测试） |
| `max_tool_calls` | 200 | 沙箱内工具调用次数上限（含失败调用）。AppWorld 高循环任务实证：金标方案 91–244 次调用，旧默认 50 连"先摸元数据再批量写"的两阶段打法都装不下（实测摸底阶段即触顶、正事零执行） |
| `timeout_secs` | 120 | 墙钟；`0` = 无墙钟（仍受 cancel / fuel / mem 约束） |
| `fuel` | 5e9 | 指令预算（纯计算循环；debug 构建实测 ≈ 11.6s/1e9，release 更快）。默认 5e9 的依据：v2 P2 A/B 实测 2.5 MB 文件 `readAll` 整读（guest 内 JSON 解析 + 建数组）超 1e9，首试即 fuel 耗尽、靠模型重试自救；5e9 覆盖该场景且仍低于默认 `timeout_secs=120` |
| `mem_mb` | 256 | 沙箱线性内存上限 |
| `max_result_chars` | 60000 | 返回值 + console 日志总尺寸（字符口径；超限截断日志、保返回值） |

<!-- END CODE CONFIG SPEC -->

**预检的诚实定位**：`precheck` 是**效率/体验特性，不是安全机制**——词法扫描可被混淆绕过
（`globalThis["Date"]["now"]()`），安全仍由既有层承担（能力沙箱、确定性桩运行时抛错、保留名
运行时拦截）；漏检的代价仅是退回现状（运行时失败），不少任何安全保证。默认关闭。拒答文案以
`run_code precheck: ` 前缀开头、一次报告全部命中（类间固定序：环境语法 → 确定性 → 保留工具，
每类封顶 20 处）。两新字段的生效时点不对称：`skill_injection` 是**装配期**决策
（`set_code_config` 热更不会重新注入，与快照语义先例一致），`precheck` 是**调用期**决策（每次
执行经快照生效）。启用 `code.enabled` 时审计锚点自动落盘：result.json 增 `"code"` 键
（`skill_injection`/`precheck` 的装配时值），NDJSON 模式首行打印同内容——bench 复盘可归因轨迹
出自哪种披露/预检配置。

**库与复用（`code.include`，默认关）**：跨任务复用的逻辑沉淀为 workdir `lib/` 下的库——
`saveLib("lib/xxx.js", src)` 写文件 + 宿主算 sha256 + 登记 `lib/INDEX.md`（机器行
`<!-- ta-lib: path=… sha256=… needs=… ver=… st=draft/ok -->`，恒 `st=draft`）；
`include("lib/xxx.js")` 整文件直读进全局作用域（幂等、循环依赖截断、单库 1 MiB 上限）。
准入 = `st=ok`（负责人核对内容后人工改）或本会话登记过相同内容（登记集按工具 lineage 共享、
不跨会话恢复，resume 亦不恢复）；磁盘内容与登记哈希不符 → 硬拒。通道经 `__include`/
`__hash_file` 保留名走完整策略链（lib/ 收纳 + `deny_paths` + hooks 闸门 + 审计/计额），
库校验错误 `e.kind="lib"`。门关 = 助手不存在、保留名明确拒绝、描述与 skill 的库段不出现
（模型可见契约与无此特性的旧二进制一致）；通道与 prelude 调用期生效、描述与 skill 段装配期生效。
v1 只认 workdir `lib/` 一个库目录；**收紧 `fs_write_dirs` 的部署须把 `lib/` 列入可写集**，
否则 `saveLib` 的写全部被拒（读不受影响）；库的版本管理交给 git。
建库/用库/修库生命周期教学在内置技能 `run-code` 的「库与复用」段（`code.include` 开启时才有该段）。

**批量调用（`code.call_batch`，默认关）**：`tools.callBatch([{name, args}, …])` 并发执行一批
**只读**工具调用（分页拉取、逐项判别是理想对象），经 `__call_batch` 保留名进 host——保留名只是
入口，不豁免任何闸门：每个 entry 走与单次调用完全相同的序列（入口检查 → hooks 闸门 → deny/fs
层 → 执行 → after）。**无事务语义（三句话）**：无原子性；无回滚；部分执行是正常结果——结果数组
如实反映哪些 entry 跑了/被拒，**必须逐条检查**；次序保证仅存在于 guest 顺序 for 循环（副作用批量
写 for 循环，批内只收 `batchable()` 只读工具，混入副作用工具该条目得 `kind="batch"` 错误条目、
单条不中止）。单项失败不 throw、该条目为 `Error` 实例带 `e.kind`/`e.tool`（含 LD-9 的
`e.halted`：latch 态错误为 true——沙箱已停只能 return 收尾；`llm` 的 token 预占拒绝为
false——沙箱存活、可继续别的路）。额度 K 条计 K 次调用计数。并发上限 8/波、按提交序返回；事件 ToolCall 在 gate 期按
提交序串行发、ToolResult 各 entry 完成即发。超时规划公式（不给魔数）：
`timeout_secs ≈ 单调用预期时长 × ceil(批量规模 / 有效并发度) + 计算与限流余量`（有效并发度 ≤8，
仅作估算非保证）。实际并发按 `ToolConcurrency` 路由（LD-11）：`llm` 是 v1 唯一 Parallel 的
batchable 工具（批内真并发）；`read_file` 与 MCP `readOnlyHint` 工具是 Sequential，批内按
提交序串行执行（契约一致、无提速）。门关 = prelude 不暴露 `callBatch`、保留名直调按未知
工具报错、描述与 skill 的批量段不出现（模型可见契约与旧二进制一致）；通道调用期生效、描述与
skill 段装配期生效。教学与陷阱（`instanceof Error` 判别、勿 `JSON.stringify` 失败条目）在内置
技能 `run-code` 的「批量调用」段（`code.call_batch` 开启时才有该段）。

**LLM 判别（`code.llm`，默认关）**：`tools.call("llm", {...})` 给 guest 一个**单次判别/生成原语**
（code_llm M3，LD-1…19）——无工具、无历史、无流式，一轮往返；经普通 `tools.call` 通道进 host，
hooks 闸门、事件对、审计 trace、调用额度照常适用。参数：`prompt`（必填，自包含指令）、`system`、
`model`（省略用 `default_model`，显式必须在 `models` 允许列表）、`max_tokens`（1..=8192，默认
1024）、`labels`（≤16 个非空声明词表）；`temperature` 固定 0（逐项判别要可复现）。返回
`{text, label?, usage, model}`——`labels` 命中时 `label` 是**声明拼写**的 canonical（输出侧仅做
trim → 剥一层包裹引号/反引号 → 大小写折叠的最小归一）。三类失败各带结构化 `kind`：`llm`
（参数/允许列表/provider 失败，刻意不给 retryable——由模型读 message 自判）、`llm_output`
（labels 未命中；带真实 usage，成本诚实）、`budget`（run 级 token 预占被拒——沙箱存活，
`write_file` 等照常可用，预占语义见下）。**token 预占后结算（LD-17）**：调用前按
`est(prompt+system) + max_tokens + 协议余量` 预占 run 级共享预算，完成后按真实 usage 结算
（多退少补；usage 未知则保留预占——闸门宁可多占不少占），result.json 的 `meta.llm` 落 per-model
台账（reserved/actual/known/over_reserve/known_calls/unknown_calls）。**同一 provider 实例
（LD-16）**：内层调用走外层 agent 构造时的同一个 provider（绑定宿主的自定义 provider 也会收到
内层调用，答不了就用自己的诚实错误回）；每次调用 120s 硬超时。门关 = guest 工具表里没有 `llm`
（顶层撞名才报错）、描述【LLM 判别】段与 skill 的「LLM 判别」段不出现。级联判别（结果再喂
下一轮 llm）的示例在内置技能 `run-code` 的「LLM 判别」段（`code.llm.enabled` 开启时才有该段）。


**代码编排提示词（可选）**：`examples/prompts/agentic-code.md` 在原参考纪律上增加
“规则明确且剩余有规模”的选择规则，支持探查后换轨与大清单保存在变量/文件中。
仅在 `run_code` 可用时，将文件第一个 `---` 后的正文设为 `system_prompt`；不会自动加载。
`agentic.md` 保持原样供 no-code/历史对照。新版描述与技能会随重新编译进入 code 臂，
请按 [落地报告](docs/run_code_routing_report.md) 固定文案、提示词与二进制指纹，分批验证效果。
**评测装配**：include / callBatch / llm 三特性的 E0–E3 分期矩阵与四路线对照的
可复现配方（臂配置、E0′ 三问边界句装置 `examples/eval/e0prime-skill/`、指纹与
测量协议）见 [评测装配文档](docs/code_llm_eval_assembly.md)；装置漂移由
`scripts/check_eval_apparatus.py` 在 CI 红灯把守。

**何时开启（价值验证结论，2026-09）**：28 次付费 A/B 对照（单模型、合成任务，
G0–G3 全程报告见 `docs/code_value_reports/`）给出的启用指引——

- **值得开的形态**：数据只经分页 API（MCP 等）、`shell` 不可达、需全量聚合的任务。
  该形态下 run_code 自然采用带来稳定费用优势（三实例费用比 0.51×/0.55×/0.58×——
  G1 一实例 + G2 两新实例，墙钟 0.55×~0.97×；机制：12 次外层逐页调用收进 1 次
  沙箱内循环；G2 同两实例在按需披露装配下复测 0.567×/0.571×，信号稳定）。
  **默认装配（`skill_injection: "on_demand"`）即足够**：G3 验证模型经
  `read_skill` 按需发现 run_code 后自然采用，不依赖
  `agentic-code.md` 全量提示词的能力段指引——能力段是可选增益，不是触发前提。
- **开了不亏（但别用全量提示词）**：任务带 `shell` 时模型自然选 shell 直解、
  run_code 零采用；此时 `agentic-code.md` 的能力段是纯披露开销（实测费用比最高 1.43×），
  去掉能力段只留工具面后回落到 ~1.0×~1.2×。带 shell 的部署用 `agentic.md` 即可。
- **默认关维持**：验证未发现质量回归（全部配对双臂同交付、采用路径零错误），
  但样本是单模型 + 合成任务（`demand_unverified`），不足以支持默认开启或普遍降本宣称。
  「无 shell 分页聚合」形态之外的费用结论未验证。

**沙箱内环境**：纯同步 ES2023——无 async/Promise/timer/`import`/`fetch`，`Date.now`/`Math.random`
不可用（评测可复现）。API 五个：`tools.call(name, args)`（失败 throw、可 try/catch 后继续，异常带
结构化字段 `e.kind`/`e.tool`/`e.retryable`——`denied`/`hooks`/`budget`/`deadline`/`unknown_tool`/`lib`
类重试无意义，工具自身失败为 `tool` 类且刻意不给 retryable，由模型读 message 自行判断；`llm`
类的 `llm`/`llm_output` 同理不给 retryable），
`tools.list()`（不确定工具名时先调它）、`tools.budget()`（剩余调用次数 / 墙钟 / 燃料 / run 级
token 预算 `tokens_left`——未设 `max_total_tokens` 时为 null；查询本身免费，长批量开工前对一下
额度）、`console.log(...)`（实时透出 + 随结果返回）、
`copy_bytes(src, dst)`（经 `tools.call`：host 流式**字节保真复制**，二进制 / 非 UTF-8 / 大文件
逐字节一致，内容不经过 JS 字符串）。另有三个内置助手（`readAll(path)` / `readLines(path)` /
`writeVerified(path, content)`）：大文件整读 / 逐行读 / 字节保真写（写后 raw 读回比对，不经文本
往返），底层就是 `read_file` 的 `format:"json"` 分页与 `raw`/`content_b64` 字节通道——此外没有
别的内置助手（`code.include` 开启时再加 `include`/`saveLib` 两个库助手，见上节；
`code.call_batch` 开启时再加 `tools.callBatch(entries)` 批量助手，见批量调用节；`code.llm`
开启时再加 `llm` 判别原语，见 LLM 判别节）。工具描述刻意保持精简
（封闭助手清单 + 指针），深契约与完整示例在**内置技能 `run-code`** 里（`code.enabled` 时自动出现在
技能列表、经 `read_skill` 按需加载；在自己的技能目录放同名 `run-code/` 可覆盖它）。**可调工具集** =
主 agent 的工具白名单（含 MCP），但不含 `task` / `todo` / `run_code` 自身；宿主经 `build_agent_with`
注入的自定义工具**默认对沙箱不可见**，逐工具用 `CallbackTool::with_code_access()` 显式授权后才进
`tools.call` 面（第三方 `Tool` 实现同理覆写 `available_to_code()`，默认 false）；每次内部调用走与外层
**完全相同的 hooks 闸门**（`__code__` 身份标记，可在钩子里单独决策）。

**资源终止与诚实边界**：任一限额超限即强制终止，错误消息带来源标记（cancel / deadline / fuel /
mem 四路独立）；`_initialize` / 编译阶段的错误同样归因。在飞的内部工具调用**不可**被外层超时打断
（对齐 `timeout_secs` 软上限口径）——总墙钟 ≤ `timeout_secs` + 最后一次在飞调用自身的上限。内部调用
不进外层 `tool_calls` 明细，但逐调用摘要（工具名 / 耗时 / 入参哈希）、`fuel_used`、终止原因挂进
`result.json` 对应条目的 `meta` 字段；内部工具 usage 折进 `run_code` 自身账单（诚实成本）。

**与安全说明的关系**：`shell` 仍是无过滤直通（容器是它的答案，`run_code` 不改变这一点）；`run_code`
是另一条轴——**不给模型新能力**，只把"模型已有的工具能力"装进一个可限额、可审计的编排层。注意
`deny_paths` 只拦**经文件工具**的访问：若 `shell` 在白名单里，沙箱代码经 `tools.call("shell", …)`
仍可达任意路径——要硬隔离请从 `tools` 去掉 `shell`（或配合 `restrict_to_workdir`）。批量任务片段可
覆盖 `code` 子字段但**只能收紧**：`enabled` 出现即整个任务拒收（`bad_task`），数值字段取 min、
`deny_paths` 并集、`fs_write_dirs` 交集、`skill_injection` 取收紧档（off < on_demand < eager，
片段不得扩大披露）、`precheck` 只能由 true 收紧为 false（片段不得开启预检）。

库 / 绑定宿主另有运行期 API：Rust `Agent::set_code_config` / `set_code_enabled`（热更新面，轮次间
生效）；Python `enable_code()` / `disable_code()` / `set_code_config({...})`；Node `enableCode()` /
`disableCode()` / `setCodeConfig({...})`（绑定各自带默认关闭的 `code` 转发 feature——未编入时这些
方法返回明确的重建提示错误，而不是静默无操作）。

## Skill（技能）

Skill 是可命名、按需加载的**指令/知识包**：把"怎么做某类任务"的步骤、脚本、参考资料打包成
一个文件夹，agent 启动时自动发现、需要时再读取完整指令与资源。实现遵循 [Agent Skills 开放标准](
https://agentskills.io/specification)——三级渐进式披露、6 字段 frontmatter、`scripts/`/`references/`/
`assets/` 子目录约定。（Claude Code 的私有扩展字段如 `disable-model-invocation`/`context:fork`/
`$ARGUMENTS` 依赖其特有的会话/权限/子代理模型，TinyAgent 不实现。）

### 目录约定

启动时扫描两个目录，**项目覆盖全局**（同名以项目为准）：

- 全局：`~/.config/tinyagent/skills/`（或 `$XDG_CONFIG_HOME/tinyagent/skills/`）——个人通用技能。
- 项目：`<工作目录>/.tinyagent/skills/`——随仓库分发、团队共享。
- 内置：二进制内嵌的技能（目前仅 `code` feature 启用时的 `run-code`，即 run_code 沙箱的完整用法指南）——
  只在对应功能开启时出现在列表里，磁盘同名技能目录可覆盖它。

每个技能是该目录下的一个**子目录**，至少含一个 `SKILL.md`，可按约定放资源子目录：

```
~/.config/tinyagent/skills/
└── pdf-forms/
    ├── SKILL.md                  # 必需：frontmatter + 正文
    ├── scripts/extract.py        # 可选：可执行脚本
    ├── references/REFERENCE.md   # 可选：详细参考文档
    └── assets/template.json      # 可选：模板 / 数据等静态资源
```

### `SKILL.md` 格式

以 YAML frontmatter 开头，正文是完整指令。frontmatter 支持**标准 6 字段**：

| 字段 | 必需 | 说明 |
|---|---|---|
| `name` | 是 | 1-64 字符，仅小写字母/数字/连字符 `-`，首尾不能是 `-`，不能有连续 `--`，**必须等于所在目录名** |
| `description` | 是 | ≤1024 字符；写清"做什么 + 何时用"，含关键词帮模型识别。进 system prompt |
| `license` | 否 | 许可证名或打包的许可证文件 |
| `compatibility` | 否 | 建议 ≤500 字符（不强制）；环境要求（运行时、系统包、网络等） |
| `metadata` | 否 | `key: value` 映射，扩展属性（缩进块） |
| `allowed-tools` | 否 | 空格分隔的预批准工具名（实验性，见下） |

```
---
name: pdf-forms
description: 从 PDF 表单抽取字段并输出 JSON。处理表单类文档时用。
license: Apache-2.0
compatibility: needs python3
allowed-tools: shell read_file
metadata:
  author: docs-team
---

# PDF Forms

1. 用 scripts/extract.py 抽取 ...
2. 字段格式参考 references/REFERENCE.md ...
```

> 单行标量字段只支持单行值（多行 YAML 折叠不支持）；`metadata` 是唯一支持缩进 `key: value` 块的字段。
> 未知字段忽略（标准允许）。

### 三级渐进式加载

| 级别 | 何时加载 | 落点 | 成本 |
|---|---|---|---|
| ① 简介（name+description） | 启动时，所有技能 | 拼进 **system prompt**（常驻） | 极低，且被 prompt cache 缓存 |
| ② 全文（SKILL.md 正文） | 模型判断相关时调 `read_skill` | 作为 tool_result 进**对话历史** | 按需，可被上下文压缩回收 |
| ③ 资源文件（scripts/references/assets） | 先 `list_skill` 发现、再 `read_skill {file}` 读 | 同上 | 同上 |

### `read_skill` / `list_skill` 工具

扫到技能时自动配套（不进 `config.tools` 白名单）：

```
{ "skill": "pdf-forms" }                                  # 读 SKILL.md
{ "skill": "pdf-forms", "file": "scripts/extract.py" }    # 读子目录资源（相对路径）
{ "skill": "pdf-forms", "file": "references/REF.md" }     # 读参考文档
```

- `read_skill`：`file` 默认 `SKILL.md`，支持相对路径读子目录资源（`scripts/foo.sh`、`references/...`、
  `assets/...`）。路径被限制在技能目录内：禁 `..`、绝对路径、反斜杠；规范化的真实边界由 canonicalize
  + `starts_with(技能目录)` 兜底（挡模型误入，非抗敌对进程的硬隔离）。
- `list_skill`：列出某技能目录下的全部文件（含子目录，相对路径），用于**自主发现**资源后再读取。

### `allowed-tools`（实验性）

标准字段，空格分隔的工具名。TinyAgent **只解析 + 告警，不强制**：装配时检查声明的工具是否在
`config.tools` 白名单 ∪ {`read_skill`,`list_skill`} 中，缺失则 stderr 打印 `[skill] ... not in the
tool whitelist`。真正的"按轮临时预批准"需要每轮权限/激活层（Claude Code 的语义），TinyAgent 架构不同，
不做。仅字面量名匹配，不支持 glob/`Tool(spec)` 模式。

### 行为细节

- **自动配套，不在工具白名单**：扫到技能，`read_skill` + `list_skill` 就自动可用（技能与 `config.tools`
  工具是正交两层，不混在白名单里）。
- **子 agent 不继承**：子 agent（`task`）用专属 system prompt、按白名单建工具集，看不到技能简介、
  也没有 `read_skill`/`list_skill`。需要时由主 agent 在 `task` 的 prompt 里带要点。
- **坏技能跳过 + 告警**：缺 `SKILL.md`、frontmatter 缺 `name`/`description`、`name` 不合规（字符集/
  长度/首尾连字符/`--`/与目录名不一致）、`description` 超 1024 字符的技能被跳过并在 **stderr** 打印
  `[skill] ...`，不影响其他技能与启动。
- **无技能目录时行为不变**：没扫到技能，system prompt 与工具集与原来完全一致（纯向后兼容）。

> **从旧版升级**：早期版本对 `name` 校验较宽（允许大写/下划线、不校验目录名）。标准对齐后，不合规的
> 技能（如 `name: Deploy_V2`、或 frontmatter `name` 与目录名不一致）会跳过并告警——把目录名与 `name`
> 统一成小写+连字符形式即可。

## 开发

```sh
cargo test --features tui,mock   # 单元测试（含 TUI 输入/渲染逻辑 + lib_api 集成测试）
cargo clippy --features tui --all-targets   # lint（TUI 版）
cargo clippy --all-targets                 # lint（headless 版）
cargo test --features code,mock            # run_code 沙箱套件（改 run_code.rs 必跑）
cargo clippy --features code --all-targets # lint（code 版——run_code.rs 在 feature 后，上面两个看不到）

# 无 API key 的端到端冒烟测试：
python3 examples/mock_openai.py 8791 &      # OpenAI 兼容 SSE mock
python3 examples/mock_anthropic.py 8792 &   # Anthropic 原生 SSE mock
cd $(mktemp -d) && MOCK_API_KEY=dummy tiny-agent --config <指向 mock 的配置> --output result.json
```

`examples/mock_openai.py` / `mock_anthropic.py` 是零依赖 SSE mock server，用于离线验证整个
agent loop（含 tool_use 配对、token 用量解析、流式取消）。注意 `[profile.release] panic = "abort"`，
release 下 panic 不触发 `Drop`——调试 destructors（如 TUI 终端还原）用 debug 构建。

核心 agent loop 有进程内回归测试：`src/provider/mock.rs` 是 cfg(test) 的脚本化 Provider，
`src/agent.rs` 的 `#[cfg(test)]` 模块覆盖 cancel 契约（run 永不清、预置即取消、受理点 `clear_cancel`）、tool_use/tool_result N:N 配对（含中途取消的
合成 `[cancelled by user]`）、上下文压缩、usage 累计等不变量。**改 `agent.rs` 核心路径请配套补 loop 测试。**

## 发布流程

版本号同步在 **5 处**（改一处漏一处会造成绑定包与核心版本漂移）：根 `Cargo.toml`、
`bindings/{python,node}/Cargo.toml`、`bindings/python/pyproject.toml`、`bindings/node/package.json`。

1. 五处版本号同步 bump；
2. `CHANGELOG.md` 补 `[新版本]` 段（从 `[Unreleased]` 挪入，附日期）；
3. 提交后打 tag：`git tag vX.Y.Z`；
4. `git push --tags` —— tag 推上 GitHub 后自动触发 release workflow
   （`.github/workflows/release.yml`：musl x86_64 静态二进制 + sha256 上传 GitHub Releases，
   附自动生成的 release notes；另有 `bindings-node` 五平台矩阵——主包 + 平台子包
   `tiny-agent-<platform>` 的构建与打包，配 `NPM_TOKEN` secret 自动发布 registry，
   未配则子包 tarball 附 Release；另有 `bindings-python` 构建 Python sdist + wheel
   （CPython manylinux2014 / manylinux_2_17 x86_64，不带 `code` feature——标准 wheel
   不携带 wasmtime，其他解释器用 sdist 自行构建）附 Release，未发布 PyPI）。gitee 侧
   `git push origin --tags` 同步标签。

   **产物带 `code` feature**（0.3.x 起）：musl 二进制编译入 run_code 沙箱
   （较无 feature 版 +≈4.6 MiB），但运行期开关 `code.enabled` 默认关——
   config 显式写 `"code": {"enabled": true}` 才暴露给模型（红线：被注入模型
   默认不获得"循环 + 代码执行"能力，见 run_code 专节）。要更小体积的自建
   用户去掉 `--features code` 即可（约 2.7 MiB）。

## 代码结构

行数与模块清单由 `scripts/gen_structure.py` 从源文件实时生成（CI 在 fmt job 以 `--check` 门禁，防漂移）；每个模块的一句话职责维护在脚本的 ENTRIES 里。

<!-- BEGIN CODE STRUCTURE (generated by scripts/gen_structure.py) -->
```text
crates/core/src/  # 内核 crate（package tiny-agent-core，lib 名 tiny_agent）：agent loop / 工具 / provider / 配置 / 装配
├── lib.rs            (  566)  # 库根：build_agent* / assemble 装配（工具表、ToolCtx、code 安装、技能注入）
├── fsutil.rs         (  463)  # 原子写原语（O_EXCL temp + fsync + 目录 fsync；session/result/report 共用）
├── diag.rs           (  165)  # 诊断通道：GUI 宿主可安装的全局 sink（默认回退 stderr，字节不变）——技能/MCP/session/run_code 的软失败警告走此通道
├── config.rs         ( 2566)  # 配置解析 + 任务片段合并 + 批量覆盖白名单
├── provider/  # LLM 后端
│   ├── mod.rs            (  977)  # 共享 Message/Block、SSE reader、CancellableSse
│   ├── openai.rs         ( 1814)  # OpenAI 兼容（include_usage；400 回退：max_tokens→max_completion_tokens / 丢 stream_options）
│   ├── anthropic.rs      ( 1213)  # Anthropic 原生（message_start/message_delta usage）
│   └── mock.rs           (  345)  # 测试用脚本化 Provider（cfg(any(test, feature="mock"))）
├── agent.rs          ( 3645)  # 核心 loop + usage 累计 + cancel + 上下文压缩（前馈/溢出双触发 + 滚动分块摘要）+ 相邻消息归一 + 同 turn 多 task 并行
├── agent_tests.rs    ( 8223)  # 核心 loop 回归测试（provider::mock 脚本化，不连网）
├── usage_probe.rs    (   77)  # 并行 worker 的线程本地 usage 侧信道：panic unwind 后把已计费 usage 带回 join 点（防 result.json 与共享计数器分叉）
├── compress.rs       (  962)  # 压缩引擎（find_safe_cut / 分块摘要 / saturating usage 合并 + block 级计费原语）
├── context_report.rs (  874)  # 上下文构成分解（ContextBreakdown 估算口径：system 三段 / 工具四类 / 消息分型 + 锚点校准）
├── retry.rs          (  158)  # batch --retries / 单跑瞬态重试共用的指数退避（±25% jitter 防 lockstep）
├── session.rs        (  522)  # 会话持久化（原子写 + archive 折叠）
├── skills.rs         ( 1074)  # 三级技能加载（内置 / 用户目录 / 项目）
├── hooks.rs          (  181)  # hooks 闸门（Deny / Modify / 审计）
├── tools/  # 工具集：内置工具 + MCP 包装 + task 子 agent + run_code（feature code）
│   ├── mod.rs            ( 1210)  # Tool trait + ToolCtx + CallbackTool/SharedTool + build 白名单组装
│   ├── shell.rs          ( 1250)  # shell 工具（unix: sh -c；Windows: cmd /C + raw_arg 命令逐字传递，防 std 引用改写）
│   ├── file.rs           ( 1524)  # read_file / write_file / edit_file / list_dir
│   ├── image.rs          (   87)  # load_image 工具
│   ├── document.rs       (  100)  # PDF → Block::Document 加载（库 API load_document_block）
│   ├── skill.rs          (  975)  # read_skill / list_skill（不在白名单，由装配注入）
│   ├── mcp.rs            (  627)  # MCP 工具包装（sanitize_name + wire_name 分发）
│   ├── subagent.rs       ( 1042)  # task 子 agent 工具（独立上下文 + turn 封顶；不可递归）
│   ├── todo.rs           (  311)  # todo 工具（任务计划，全量替换；plan 落 result.json）
│   ├── run_code.rs       (  103)  # run_code 门面（feature code）：mod 声明 + 符号汇回；实现在 run_code/ 子模块
│   ├── llm.rs            (  736)  # llm 判别原语（feature code + code.llm 门）：单发无状态内层模型调用——LD-17 预占/结算、LD-18 labels 归一、共享 Provider cell
│   └── run_code/  # run_code 子模块（同 code 门）：从原 11.9k 单文件按关注点拆分
│       ├── contract.rs       (  747)  # 对客契约文本：保留名/CODE marker/字节上限、PRELUDE 三源、技能 md 装载、strip_once
│       ├── sandbox.rs        (  799)  # wasmtime 沙箱：QuickJS 模块装载与缓存、epoch/fuel/MemLimiter、CallTrace、HostCtx、墙钟、guest 内存读上限
│       ├── guest_api.rs      ( 3587)  # 宿主函数面：unpack/read/write、host_call_tool 分发、内层结果信封、llm marker 解析折叠、审计 meta
│       ├── fs_policy.rs      ( 1504)  # fs/glob 策略：路径拦截与词法归一、写目录约束、symlink 检查、glob 匹配（** 折叠 + 段数上限）
│       ├── lib_index.rs      ( 2013)  # lib 家族：会话库索引/读写封顶、sha256、保留名、include 投递
│       ├── batch.rs          ( 2288)  # callBatch：wave 调度、输出准入（总量封顶降级）、条目信封
│       ├── precheck.rs       (  858)  # FR-2 预检：lexer + 关键词命中渲染
│       ├── tool.rs           ( 1090)  # 工具本体：RunCodeTool 状态机 + Tool impl（被钉哈希的 description/schema）
│       └── test_util.rs      (  123)  # 共享测试辅助（cfg(test)：scratch/ctx/llm 装配）
├── mcp.rs            ( 3524)  # MCP 客户端（stdio / Streamable HTTP 含 202→GET 流 / 旧版 SSE）
├── winjob.rs         (  413)  # Windows Job Object 进程树击杀（shell 超时/取消 + MCP 子进程回收共用；两种 drop 策略）
└── m0_probes.rs      (  337)  # M0 机制探针（cfg(test)，#[path] 声明于 lib.rs）：reserve/settle 账本、共享 Provider cell、按请求 model 分路的进程内原型（A-L2b/2c/9——M3 llm 原语直接沿用）

src/  # CLI crate（package tiny-agent，bin tiny-agent）：进程壳 + headless 呈现 + 批量 + TUI
├── main.rs           ( 1021)  # CLI 入口 + 组装（build_agent）+ --session none
├── signal.rs         (  123)  # headless SIGINT → cancel（首次优雅 / 二次硬退 130）
├── batch.rs          ( 3577)  # 批量任务 + --retries/--failed + 报告合并
├── harness.rs        ( 1884)  # headless 模式 + result.json + NDJSON + exit code
├── harness_tests.rs  ( 1122)  # harness 行为测试（重试梯子 / result.json / NDJSON；自内核 agent_tests.rs 随 harness 迁入）
└── tui.rs            ( 8752)  # TUI（feature tui）：内联流式渲染（对话进原生 scrollback）/ InputBuffer 多行输入 / 斜杠命令+补全 / markdown 渲染 / bracketed paste
```
<!-- END CODE STRUCTURE -->
