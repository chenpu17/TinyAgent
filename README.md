# TinyAgent

一个极简的单二进制 AI agent：一个 JSON 配置即可运行。两种模式 —— headless 执行任务（harness 场景）或 TUI 对话。目标：嵌入式低配 Linux、macOS、Windows，工具精炼、约 1.5 万行核心代码（核心 agent loop 集中在 `agent.rs`；可选 TUI 约 5 千行），结构清晰可快速通读并自行定制。

## 特性

- **单一可执行文件**，无运行时依赖；musl 静态编译后可丢到路由器 / 树莓派 / CI runner 直接跑
- **两种 LLM 后端**：Anthropic 原生 API + OpenAI 兼容协议（覆盖 DeepSeek / vLLM / Ollama 等），均带 token 用量统计
- **图片输入**：`--image` 参数或 `load_image` 工具（png / jpeg / gif / webp，base64）
- **精炼工具集**：`shell`、`read_file`、`write_file`、`edit_file`、`list_dir`、`task`（子 agent；同 turn 多调用并行）、`todo`（任务计划，落进 result.json 的 `plan` 字段）默认启用；`load_image`（视觉）需在 `tools` 显式加入。均可在 config 的 `tools` 里裁剪
- **Harness 契约**：`--output result.json` 写结构化结果（含 token 用量、每次工具调用的 input/output），`--json` 流式 NDJSON；exit code 语义化（`0` 成功 / `124` 超时 / `130` 取消 / `1` 其它），可直接进 CI / 评测 pipeline
- **CLI 覆盖**：`--model` / `--max-turns` / `--timeout` 免改配置做 A/B 对照
- **批量任务**：`--batch <dir>` 并发跑一组任务片段，`--retries` 对 transient 失败指数退避重试，`--failed` 只重跑上一轮失败的任务并合并进完整报告
- **TUI（可选 feature）**：ratatui 对话界面——内联/流式渲染（对话进终端原生 scrollback，原生选择/滚动/搜索开箱即用）、多行输入、输入历史、斜杠命令、token 状态条；默认不编译，保持 headless 版极小
- **同步实现**（无 tokio），`reqwest::blocking` + rustls，核心 agent loop 集中在 `agent.rs`
- **库 API**：核心逻辑同时编译为 `tiny_agent` 库（bin + lib 同包双 target），第三方 Rust 程序可直接 `tiny_agent = "..."` 驱动 agent loop、注册自定义工具（`CallbackTool`）、读回诚实 token 账单；Python（PyO3）/ TypeScript（napi-rs）绑定构建其上。`mock` feature 放出脚本化 provider 供离线 dry-run。**扩展面**：Hooks 拦截层（`set_hooks`：审批/拒绝/改写工具调用、审计、观察请求，子 agent 自动继承）+ 工具能力标记（`concurrency()` 声明并行、`available_to_subagent()` 声明子 agent 继承），均无需改核心

## 构建

```sh
cargo build --release                    # headless 版（最小体积）
cargo build --release --features tui     # 含 TUI 对话界面
```

交叉编译（推荐用 [cross](https://github.com/cross-rs/cross)）：

```sh
cross build --release --target x86_64-unknown-linux-musl
cross build --release --target aarch64-unknown-linux-musl
cross build --release --target armv7-unknown-linux-musleabihf
cross build --release --target x86_64-pc-windows-msvc
```

## 库 API（程序化调用）

除二进制外，同一份源码也编译为 `tiny_agent` 库（`src/lib.rs`），供第三方 Rust 程序——以及 Python（PyO3）/ TypeScript（napi-rs）绑定——直接驱动 agent。二进制本身只是这个库之上的一层薄壳（CLI 参数解析、SIGINT、`process::exit`、可选 TUI），行为与库完全一致。

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

run 结束后从 [`RunOutcome`](src/agent.rs)（或 `Agent::usage()` / `messages()` / `model()` getter）读回诚实的 token 账单与历史。`AgentEvent` 派生了 `Serialize`，绑定层可直接序列化成宿主语言的 dict / object。

### Python / TypeScript 绑定

两个语言绑定是 workspace 成员（`bindings/python`、`bindings/node`），各自 path-依赖根库，把上面同一套能力（agent loop、自定义工具、流式事件、取消、诚实 usage）导出给宿主语言。两者都**复用 `CallbackTool`** 注入自定义工具，**不改核心代码**。

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

**自定义 provider 回调**：传 `provider=` 一个 Python callable，整个替换内置 HTTP provider（不连网、不要 key）。callable 收 `{system, messages, tools, model, max_tokens}`，返回 `str`（=纯文本）或 `{text, stop_reason?, usage?}`：

```python
# 一个 mock / 适配器 provider：agent 驱动 loop，但每轮回复来自这个函数
def echo_provider(request):
    return {"text": f"you said: {request['messages'][-1]}", "stop_reason": "end_turn"}

a = tiny_agent.Agent("chat.json", provider=echo_provider)  # 无 api_key
```

非流式 v1：返回整段文本，作为单条 delta 发出；不支持 tool_use（需要工具调用请用内置 provider + 自定义 `Tool`）。provider 抛异常会**诚实地 fail 该轮**。**取消 / 超时契约**：callable 在 GIL 内同步执行，`timeout_secs` 与取消**不会中断它**——agent 等它返回，可长时间运行的 provider 必须自行设限；预算与超时仍在轮次边界生效。

**Node.js / TypeScript（`import { chat, Agent }`，napi-rs）**

```sh
cd bindings/node && npm install && npm run build   # 生成 .node + wrapper.js
npm test                                            # 离线测试（node --test，对进程内 SSE mock）
```

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

**自定义 provider 回调**：传 `provider:` 一个函数（或 `{ chat }` 对象），整个替换内置 HTTP provider（不连网、不要 key）。收 `{system, messages, tools, model, max_tokens}`，返回 `string`（=纯文本）或 `{text, stop_reason?, usage?}`：

```javascript
// 一个 mock / 适配器 provider：agent 驱动 loop，每轮回复来自这个函数
const a = new Agent('chat.json', {
  provider: (request) => ({ text: `you said: ${request.messages.at(-1)}`, stop_reason: 'end_turn' }),
});
await a.run();
```

与自定义工具同款 TSFN round-trip（JS 调 `resolveProvider(id, result)` 唤醒 worker）。非流式 v1：返回整段文本，作为单条 delta 发出；不支持 tool_use（需要工具调用请用内置 provider + 自定义 `Tool`）。provider 抛异常会**诚实地 reject 该轮**。worker 等待回调的上限取 `min(本次请求剩余预算, 120s)`——run 设了 `timeout_secs` 时永不超出预算（预算仍在轮次边界生效；同步阻塞的 host 代码本身无法被中断）。

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

只重试 **transient** 失败（`stop_reason == "error"`：provider / 网络错误，如连接被拒、5xx），
指数退避 `1/2/4/8/16/32s`（封顶 32s）。**不重试**确定性失败——重跑也不会变：

| stop_reason | 含义 | 是否重试 |
|---|---|---|
| `error` | provider/网络错误（transient） | ✅ |
| `context_overflow` | 上下文溢出且强制压缩未能救回（确定性） | ❌ |
| `bad_task` | 任务片段非法（禁用字段 / 缺 task / JSON 错） | ❌ |
| `max_turns_reached` / `max_tokens` | 模型跑满上限 | ❌ |
| `budget_exceeded` | 累计 token 触顶 `limits.max_total_tokens` | ❌ |
| `end_turn` | 正常完成 | ❌（已成功） |

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

`--retries N` 在**单跑 headless** 模式同样生效：`agent.run` 返回 `Err`（provider/网络/超时，即 transient
失败）时按指数退避（1/2/4/8/16/32s 封顶）原地重试，最多 N 次额外尝试；确定性结局（`end_turn` /
`max_turns` / `cancelled` / `timeout` / `budget_exceeded`）是 `Ok`，**不重试**。退出码与无重试时一致（成功 0、失败 1），
`result.json` 的 `attempts` 字段记录实际运行次数（`>1` 即发生过重试）。`--json` 模式每次重试前另吐一行
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

## TUI 对话模式

需要 `--features tui` 编译。`--tui` 进入一种**内联/流式**渲染（与 Claude Code / Codex 同款）：已定稿的对话内容**提交进终端的原生 scrollback**，屏幕底部只留一个固定高度的小 live 区（流式区 + 输入框 + 状态行）每帧重绘——所以**终端原生的文本选择 / 滚动 / 搜索都直接作用于真实对话**，旧全屏重绘模型里"拖选一段、内容一滚选区就漂"的问题不复存在。默认即 **输入模式**：

| 键 | 动作 |
|---|---|
| `Enter` | 发送（运行中则为排队，当前轮结束后自动发出；补全弹窗打开时为补全选中命令，不发送） |
| `Alt+Enter` / `Shift+Enter` | 输入换行（多行） |
| `Backspace` | 删前字符（跨行合并上一行） |
| `←`/`→`/`Home`/`End` | 行内移动 |
| `Alt+←`/`Alt+→` | 按词移动（空白分词；CJK 整段非空白算一词） |
| `Ctrl-W` | 删前一词（行首退化为合并上一行） |
| `Ctrl-U` / `Ctrl-K` | 删到行首 / 删到行尾 |
| `Ctrl-A` / `Ctrl-E` | 移到行首 / 行尾 |
| `↑`/`↓` | 输入区跨行移动（补全弹窗打开时为移动弹窗选中） |
| `Alt+↑`/`Alt+↓` | 输入历史翻页 |
| `Ctrl-C` | 忙 → 取消当前轮（同时丢弃排队消息）；空闲 → 退出 |
| `Esc` | 忙 → 取消当前轮；空闲 → 清空输入框（补全弹窗打开时只关弹窗）。退出用 `/quit`、`/exit` 或 `Ctrl-C` |

运行中（running）输入框保持可编辑：可以继续打字，`Enter` 会把消息排队——live 区显示暗色的
`❯ … · queued` 占位行，输入框标题同步显示队列长度；当前轮结束后最老的
占位行换成真实消息并自动发送；`Ctrl-C` 取消当前轮会**连同排队消息一起丢弃**；`/clear` 在运行中禁用，
其余斜杠命令（`/export`、`/model`、`/limits`、`/tools`、`/system`、`/help`，以及退出用的 `/quit` / `/exit`）照常可用。

**原生选择 / 滚动 / 搜索**：已定稿的对话（你的消息、助手回复、工具调用与结果、系统行）都在终端
scrollback 里，是真正的终端文本——鼠标拖选即选中原文，复制走终端自身机制（macOS 上选中即复制）；
滚轮 / `Shift`+滚轮 / `PageUp`/`PageDown` / `Cmd-F`、`Ctrl-F` 搜索都是终端自带能力，翻到的就是上面
已定稿的真实对话。应用不再托管滚动或选区，所以全屏重绘模型的选区漂移问题在这里不存在。流式中的
在途助手回复暂留在底部 live 区（带 `▌` 光标），回合结束才整条提交进 scrollback；若在途内容超出
live 区高度，顶部显示 `↑ +N lines above` 截断提示（完整内容在回合结束后照常进 scrollback）。
空对话（或 `/clear` 之后）live 区显示暗色按键速览（`/help`、补全、原生滚动 / 选择 / 复制）。
对话正文在宽屏下限制为 120 列并左对齐，角色用行内图标区分——你的消息以青色 `❯ ` 开头、助手的
以绿色 `● ` 开头（无独立标题行，每条消息省一行）；工具调用和结果缩进归属于助手，各占一行单行预览
（工具调用显示紧凑参数摘要、工具结果显示输出预览）；失败且输出不超过 20 行的结果自动展开方便直接
看到报错，超长输出保持折叠。输入框与状态行使用近全宽布局；
running 时输入框标题显示 spinner、当前活动（thinking / writing / → 工具名）、本轮耗时、取消提示和
队列长度（终端过窄放不下时优先省略活动名，保留耗时与取消提示），整个输入框边框随运行状态变色；
空闲时标题为 ` Ask TinyAgent `。
一轮开始还没有可见输出时，live 区显示瞬时的 `● …` 占位（渲染期合成，不进对话历史），首个文本或
工具调用出现即消失。
状态行左侧只保留模型、整体运行状态（running / idle，无计时）、上下文占用和累计 token（如
`1.2k in · 0.8k out`）；右侧右对齐显示当前目录的完整路径
（并行开多个窗口时一眼区分，过长时左侧截断加 `…` 前缀，剩余不足 12 列则整段隐藏——窄屏时它最先消失）。
上下文占用显示为 `ctx 54.7k`；在配置的
`limits.max_context_tokens` 里设了模型上下文窗口后变为 `ctx 21% (54.7k/256k)`，占用 ≥60% 转黄、
≥80% 转红。agent 用过 `todo` 后，状态条另显一段 `plan done/total`（如 `plan 2/5`，全部完成转绿、
进行中为黄），实时反映任务计划进度——完整 checklist 仍在最近一条 `todo` 工具结果行里。
终端变窄时状态条先省略 token、再省略 plan，上下文段保留到最后，只保留模型和运行状态为止。退出
（Esc / 空闲 Ctrl-C）时若仍有排队消息，终端还原后会在 stderr 提示被丢弃的条数。
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
| `/model` `/limits` `/tools` `/system` | 只读查看当前配置 |
| `/help` | 命令列表 |

取消机制：每轮开始会复位 cancel 标志，`Ctrl-C` 在流式 chunk 间（≤200ms）生效，取消后可继续发下一条。

多行粘贴：终端开启 bracketed paste，一次粘贴含换行的多行会被当作**单条**消息，不会因换行被提前发送——直接粘贴即可，无需手动改 Alt+Enter。

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
| **http** | Streamable HTTP（2025-03-26 现行标准）：单端点 POST，支持 JSON / SSE 响应、Mcp-Session-Id |
| **sse** | 旧版 HTTP+SSE（2024-11-05 已废弃协议）：GET 收事件流 + endpoint POST。注意其长连接 GET 在服务器不关流时读取线程只能等进程退出回收——长驻批量作业优先 stdio / http |
| **auto**（默认） | 先按 Streamable HTTP 探测，失败自动回落旧版 SSE |

不支持的边角：Streamable HTTP 的 GET 流（server-initiated 消息）。`examples/mcp_echo.py`（stdio）与 `examples/mcp_http_echo.py`（/mcp + /sse 双端点）是零依赖测试服务器，也是协议参考实现。内置工具白名单（`tools`）与 MCP 工具互不影响。

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
  "system_prompt": "可选系统提示词",
  "task": "headless 模式要执行的任务（可被 --task 覆盖）",
  "workdir": "工具操作的工作目录，默认当前目录",
  "restrict_to_workdir": false,
  "inject_environment": true,
  "tools": ["shell", "read_file", "write_file", "edit_file", "list_dir", "load_image"],
  "mcp_servers": [],
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
    "shell_timeout_secs": 120
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

`reasoning_effort`（可选字符串，默认不设）：思考程度，**按字符串原样透传给 provider，不做枚举校验**——各家档位碎片化且随模型代际变动，TinyAgent 不维护映射表，由 provider/server 拒绝未知值。取值参考：OpenAI `none`/`low`/`medium`/`high`/`xhigh`/`max`（随模型代际扩张）、Anthropic `low`/`medium`/`high`/`max`（Claude 4.6+ adaptive thinking；Opus 4.7 起旧 `budget_tokens` 已移除）、DeepSeek `low`/`high`/`xhigh`/`max`、GLM 仅开关（`enabled`/`disabled`）。映射：OpenAI 系直接发 `reasoning_effort` 字段（老网关不认会 400，`body_fix_for_400` 自动 drop 重试，与 `max_tokens`/`stream_options` 同款）；Anthropic 发 `thinking: {"type": "adaptive"}` + `output_config: {"effort": "..."}`（effort 在**独立顶层 `output_config` 对象**里，不在 thinking 内、也不是裸顶层字段——裸顶层会被静默忽略，这是该 API 最常见的坑），`"none"` 发 `{"type": "disabled"}`。

**思考内容透出**：推理模型的思考过程作为 `ThinkingDelta` 事件流式透出（OpenAI 系 `delta.reasoning_content`、Anthropic `thinking` content block），`--json` 下为 `{"type":"thinking","delta":...}`；**不进对话历史正文与上下文回传**（各家 API 也不要求回传，故只作事件透出给 UI）。GUI（如 TinyWorker）据此渲染可折叠"思考过程"区块；headless 终端把思考写到 **stderr**（不污染 stdout 的最终回复 / `summary`）；TUI 内联显示（冻结，仅保证可见，不做独立可折叠区块）。批量任务片段可覆盖此项（软字段，与 `model`/`max_tokens` 同类，不在 `forbidden_batch_override`）。

### 上下文压缩

长任务中历史消息会持续增长直至撞模型上下文上限。压缩有**两条触发路径**，共用同一套摘要逻辑：

1. **前馈（preflight）**——每轮调用前预估请求大小，超阈值就先压缩再用更短的历史发请求。阈值二选一：
   - 设了 `max_context_tokens`（>0）：改用 token 估算（≈4 字符/token，另预留本回合 `max_tokens` 输出），
     到窗口的 ~85% 就压。这是「按模型真实窗口压」，推荐。
   - 未设（`0`，默认）：回退 `compress_threshold_chars`（默认 400k 字符 ≈100k tokens，`0` = 关闭）的字符启发式。
2. **溢出（overflow）兜底**——**永远开启，与上面配置无关**。当 provider 返回「上下文过长」类错误
   （`context_length_exceeded` / `prompt is too long` / `maximum context length` 等，子串匹配错误体）
   时，强制压缩一次并**重试该回合一次**；第二次仍溢出才当真错误上抛。这样即便前馈估算偏了、
   或工具 schema / system prompt 占了大头，agent 也能自愈而不是整轮失败。

摘要质量上：模型把中段历史写成**结构化滚动摘要**（目标/进展/决策/改动文件/报错与解法/下一步），
折叠进首条任务消息，只保留任务 + 最近 `keep_recent_messages` 条原文；后续压缩是**更新**这份摘要而非
重摘要整个前缀。前缀过大时按消息边界**分块**摘要再合并，把单次压缩比控制在 ~6:1，保住路径/报错等细节。
切割点选在 assistant 消息边界，保证 tool_use/tool_result 配对不被切断；压缩失败时告警一次并禁用本次
运行的后续压缩（不中断任务，也不每轮重试刷屏）。运行日志与 TUI 系统行里的压缩事件带触发路径标注
（`[context compressed (preflight): dropped N messages, M kept]` / `(overflow)`，`--json` 的
NDJSON 里则是额外的 `"trigger": "preflight"|"overflow"` 字段），一眼区分是估算超阈值还是溢出兜底。

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

**这是一个给模型直接执行能力的 agent，不是沙箱。** 请按此定位使用：

- `shell` 工具原样执行模型生成的命令（`sh -c` / `cmd /C`），无任何命令过滤。
  模型可以 `rm -rf`、`curl ... | sh`、访问网络——与其赋予进程的权限一致。
- 文件工具默认不限制路径：`read_file ../../etc/passwd`、
  `write_file /etc/cron.d/x` 都会被执行。
- **在 CI / 评测 pipeline 中批量运行不可信的第三方 task 时**，务必：
  1. 在容器或受限用户下运行 tiny-agent；
  2. 配置 `"restrict_to_workdir": true`，文件工具将拒绝一切逃逸
     `workdir` 的路径（`../` 与绝对路径，对新建文件同样生效）；
  3. 必要时用 `"tools"` 白名单去掉 `shell`。

## Non-goals（明确不做）

TinyAgent 的定位是**可审计、可无人值守的 agent 执行单元**——一个跑完一定留下完整、诚实、机器可读账单的一次性进程。下面这些方向与该定位冲突，明确不做（把力气挡在护城河上）：

- **不做 `--serve` / HTTP 服务模式**：长驻、多租户、鉴权是另一个产品，与"一次性执行单元"直接冲突。要集成的用户用 `--json`（NDJSON 流）+ exit code 包一层即可。
- **不自建沙箱 / 命令过滤**：半吊子过滤只制造虚假安全感。隔离的官方答案是**在容器或受限用户下运行**（见上文安全说明）。（库 API 的 Hooks 拦截层是给嵌入方接自己的审批/策略系统用的编译期扩展点，不是内置安全边界——威胁模型见 `src/hooks.rs`。）
- **不做插件系统**：工具是编译期的内置 + MCP，不引入运行时加载。（Hooks 与工具能力标记同理：编译期 trait 扩展，不加载动态代码。）
- **TUI 冻结在当前完成度**：它是 dogfooding / 演示工具，对标全职迭代的 Claude Code / Codex，追平是负 ROI；之后只修 bug，不加新功能。

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
  不可救的上下文溢出（强制压缩未能救回）记 `stop_reason: "context_overflow"`（不进瞬态重试，exit code 仍为 1），与 batch report 同口径

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

`--json` 模式额外在每个事件吐一行 NDJSON（`type`: `turn` / `text` / `thinking` / `tool_call` /
`tool_result` / `usage` / `compressed` / `warning` / `subagent` / `todo` / `done`，重试时另吐 `retry`），可在结果文件落盘前实时跟进进度。

## 内置工具

| 工具 | 说明 |
|---|---|
| `shell` | 在工作目录执行命令（sh -c / cmd /C），超时后 kill 整个进程组（默认 120s，`limits.shell_timeout_secs` 可调、0=不限），输出有界读取并按 `max_output_chars` 截断；命令留下的、仍持有输出管道的后台进程在 2s 宽限后被回收并标注（想留后台进程运行请重定向其输出：`cmd >log 2>&1 &`，不持管道即不受影响） |
| `read_file` | 读文本文件，带行号，支持 offset/limit；流式读取，超大文件不会整段进内存 |
| `write_file` | 全量重写文件，自动创建父目录 |
| `edit_file` | 精确替换：`old_string`→`new_string`，默认要求唯一匹配（`replace_all` 全替换）；匹配 0 或 >1 时报错引导补上下文，复用 `restrict_to_workdir` 沙箱 |
| `list_dir` | 列目录，最多 500 条 |
| `load_image` | 加载本地图片让模型"看到"（需视觉模型） |
| `task` | 把一块工作委派给一个**子 agent**（独立上下文 + 独立 turn 预算），只把子 agent 的最终摘要作为结果返回，保持主上下文清爽。详见下节 |
| `todo` | 记录/更新任务计划（全量替换语义，每次发完整列表），最终计划落进 result.json 的 `plan` 字段。模型可见的是可读 checklist，审计真相是结构化 input |

## 子 agent（`task` 工具）

`task` 是一个子 agent 工具：主 agent 把一个聚焦的子任务交给一个全新的子 agent（自己的消息历史、自己的 turn 预算），子 agent 跑完后只把**最终摘要**作为 tool 结果返回——主 agent 的上下文不会被子 agent 的中间过程撑大。

- **输入**：`{ "name": "身份标识", "description": "简短说明", "prompt": "完整自包含的指令" }`（`name` 与 `prompt` 必填，`description` 可选、信息性）。`name` 是这个子 agent 的身份，所有转发出来的事件都用它打标签。
- **同 turn 多调用并行**：主 agent 在**一个** assistant turn 里发出多个 `task` 调用时，它们会**并行**跑（每个一个线程，借线程池 scope），各自的事件经 mpsc 漏斗实时转发进主 run 的事件流——所以多个子 agent 协同工作时，输出是交错流式出现的。结果仍按原始块序拼回（N:N 配对不变）。叶子工具（`shell`/`read_file`/…）保持顺序执行，只有 `task` 并行，避免在文件/进程上引入新的竞态。
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

# 无 API key 的端到端冒烟测试：
python3 examples/mock_openai.py 8791 &      # OpenAI 兼容 SSE mock
python3 examples/mock_anthropic.py 8792 &   # Anthropic 原生 SSE mock
cd $(mktemp -d) && MOCK_API_KEY=dummy tiny-agent --config <指向 mock 的配置> --output result.json
```

`examples/mock_openai.py` / `mock_anthropic.py` 是零依赖 SSE mock server，用于离线验证整个
agent loop（含 tool_use 配对、token 用量解析、流式取消）。注意 `[profile.release] panic = "abort"`，
release 下 panic 不触发 `Drop`——调试 destructors（如 TUI 终端还原）用 debug 构建。

核心 agent loop 有进程内回归测试：`src/provider/mock.rs` 是 cfg(test) 的脚本化 Provider，
`src/agent.rs` 的 `#[cfg(test)]` 模块覆盖 cancel 复位、tool_use/tool_result N:N 配对（含中途取消的
合成 `[cancelled by user]`）、上下文压缩、usage 累计等不变量。**改 `agent.rs` 核心路径请配套补 loop 测试。**

## 发布流程

版本号同步在 **5 处**（改一处漏一处会造成绑定包与核心版本漂移）：根 `Cargo.toml`、
`bindings/{python,node}/Cargo.toml`、`bindings/python/pyproject.toml`、`bindings/node/package.json`。

1. 五处版本号同步 bump；
2. `CHANGELOG.md` 补 `[新版本]` 段（从 `[Unreleased]` 挪入，附日期）；
3. 提交后打 tag：`git tag vX.Y.Z`；
4. `git push --tags` —— tag 推上 GitHub 后自动触发 release workflow
   （`.github/workflows/release.yml`：musl x86_64 静态二进制 + sha256 上传 GitHub Releases，
   附自动生成的 release notes）。gitee 侧 `git push origin --tags` 同步标签。

## 代码结构

```
src/
├── main.rs      (345)  # CLI 入口 + 组装（build_agent）+ --session none
├── config.rs    (588)  # 配置解析 + 任务片段合并 + 批量覆盖白名单
├── provider/           # LLM 后端
│   ├── mod.rs   (428)  #   共享 Message/Block、SSE reader、CancellableSse
│   ├── openai.rs (720) #   OpenAI 兼容（include_usage；400 回退：max_tokens→max_completion_tokens / 丢 stream_options）
│   ├── anthropic.rs(554) # Anthropic 原生（message_start/message_delta usage）
│   └── mock.rs   (216) #   测试用脚本化 Provider（cfg(test)）
├── agent.rs    (3181) # 核心 loop + usage 累计 + cancel + 上下文压缩（前馈/溢出双触发 + 滚动分块摘要）+ 相邻消息归一 + 同 turn 多 task 并行 fan-out（std::thread::scope）+ 回归测试
├── retry.rs      (69) # batch --retries / 单跑瞬态重试共用的指数退避（±25% jitter 防 worker lockstep）
├── signal.rs     (60) # headless SIGINT → cancel（首次优雅 / 二次硬退出 130）
├── tools/              # 6 个内置工具（shell/read_file/write_file/edit_file/list_dir/load_image）+ MCP 包装 + task 子 agent
│   ├── mod.rs   (194) #   Tool trait + ToolCtx + build 白名单组装
│   ├── shell.rs (222) #   shell 工具
│   ├── file.rs  (483) #   read_file / write_file / edit_file / list_dir
│   ├── image.rs  (79) #   load_image 工具
│   ├── mcp.rs   (261) #   MCP 工具包装（sanitize_name + wire_name 分发）
│   ├── subagent.rs(571) # task 子 agent 工具（独立上下文 + turn 封顶；不可递归；事件转发 + usage 折算 + 审计 meta trace）
│   └── todo.rs  (286) # todo 工具（任务计划，全量替换；plan 从 records 派生落 result.json）
├── mcp.rs      (802)   # MCP 客户端（stdio / Streamable HTTP / 旧版 SSE）
├── session.rs   (125)  # 会话持久化
├── batch.rs     (644)  # 批量任务 + --retries/--failed + 报告合并
├── harness.rs   (492)  # headless 模式 + result.json + NDJSON + exit code
└── tui.rs      (4978)  # TUI（feature-gated）：内联流式渲染（对话进原生 scrollback，原生选择/滚动/搜索）/ InputBuffer 多行输入+历史 / 斜杠命令+补全弹窗 / markdown+ANSI 渲染 / bracketed paste
```
