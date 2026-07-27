# TinyAgent

一个极简的单二进制 AI agent：一个 JSON 配置即可运行。两种模式 —— headless 执行任务（harness 场景）或 TUI 对话。目标：嵌入式低配 Linux、macOS、Windows，工具精炼、全量 ~6k 行（核心 agent loop 集中在 `agent.rs`），结构清晰可快速通读并自行定制。

## 特性

- **单一可执行文件**，无运行时依赖；musl 静态编译后可丢到路由器 / 树莓派 / CI runner 直接跑
- **两种 LLM 后端**：Anthropic 原生 API + OpenAI 兼容协议（覆盖 DeepSeek / vLLM / Ollama 等），均带 token 用量统计
- **图片输入**：`--image` 参数或 `load_image` 工具（png / jpeg / gif / webp，base64）
- **精炼工具集**：`shell`、`read_file`、`write_file`、`list_dir`、`load_image`，配置白名单启用
- **Harness 契约**：`--output result.json` 写结构化结果（含 token 用量、每次工具调用的 input/output），`--json` 流式 NDJSON；exit code 语义化（`0` 成功 / `124` 超时 / `130` 取消 / `1` 其它），可直接进 CI / 评测 pipeline
- **CLI 覆盖**：`--model` / `--max-turns` / `--timeout` 免改配置做 A/B 对照
- **批量任务**：`--batch <dir>` 并发跑一组任务片段，`--retries` 对 transient 失败指数退避重试，`--failed` 只重跑上一轮失败的任务并合并进完整报告
- **TUI（可选 feature）**：ratatui 对话界面——多行输入、输入历史、斜杠命令、工具结果可展开、token 状态条；默认不编译，保持 headless 版极小
- **同步实现**（无 tokio），`reqwest::blocking` + rustls，核心 agent loop 集中在 `agent.rs`

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

## 使用

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

# 批量任务模式：跑目录下所有任务片段，2 个并发，输出汇总报告
tiny-agent --config examples/harness.json --batch examples/tasks --jobs 2 --report report.json
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
- `--jobs N` 控制并发（默认 1）；`--report` 写汇总：`{total, succeeded, failed, tasks[]}`
- exit code：全部成功 → 0，否则 1，可直接做 CI 门禁

任务片段**只能覆盖** `task` / `system_prompt` / `model` / `max_tokens` / `limits`。安全相关字段
（`provider` / `base_url` / `api_key_env` / `workdir` / `tools` / `restrict_to_workdir` /
`mcp_servers`）禁止覆盖——覆盖即标记 `bad_task` 失败，不执行。

### `--retries N`：transient 失败重试

只重试 **transient** 失败（`stop_reason == "error"`：provider / 网络错误，如连接被拒、5xx），
指数退避 `1/2/4/8/16/32s`（封顶 32s）。**不重试**确定性失败——重跑也不会变：

| stop_reason | 含义 | 是否重试 |
|---|---|---|
| `error` | provider/网络错误（transient） | ✅ |
| `bad_task` | 任务片段非法（禁用字段 / 缺 task / JSON 错） | ❌ |
| `max_turns_reached` / `max_tokens` | 模型跑满上限 | ❌ |
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

## 会话持久化

- `--session <path>` 指定会话文件（默认 `./.tinyagent-session.json`），每轮结束原子落盘
- `--resume` 加载历史继续；此时 config 里的 `task` 视为已消费，用 `--task` 追加新指令
- 崩溃也不丢已完成的轮次；TUI 模式同样生效

## TUI 对话模式

需要 `--features tui` 编译。`--tui` 进入后默认 **输入模式**：

| 键 | 动作 |
|---|---|
| `Enter` | 发送 |
| `Alt+Enter` / `Shift+Enter` | 输入换行（多行） |
| `Backspace` | 删前字符（跨行合并上一行） |
| `←`/`→`/`Home`/`End` | 行内移动 |
| `↑`/`↓` | 输入区跨行移动 |
| `Alt+↑`/`Alt+↓` | 输入历史翻页 |
| `Ctrl-C` | 忙 → 取消当前轮；空闲 → 退出 |
| `Esc` | 退出 |
| `PageUp`/`PageDown` | 对话区滚动 |
| `Tab` | 切到 **导航模式** |

导航模式下用 `j`/`k` 上下选行，`Enter`/`Space` 展开/折叠工具调用与结果（折叠时只显示单行
预览，展开显示完整 input/output），再按 `Tab` 回输入模式。状态条显示模式与累计 token
（如 `tok 1.2k↑ 0.8k↓`）。`/` 开头走斜杠命令（不发给模型）：

| 命令 | 作用 |
|---|---|
| `/clear` | 清空对话历史（不进输入历史） |
| `/model` `/limits` `/tools` `/system` | 只读查看当前配置 |
| `/help` | 命令列表 |

取消机制：每轮开始会复位 cancel 标志，`Ctrl-C` 在流式 chunk 间（≤200ms）生效，取消后可继续发下一条。

## MCP 客户端

配置里声明 MCP 服务器，其工具以 `mcp_<server>_<tool>` 命名注册：

```json
"mcp_servers": [
  {"name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
  {"name": "remote", "url": "http://host:3000/mcp", "transport": "auto",
   "headers": {"Authorization": "Bearer xxx"}}
]
```

支持三种传输（`command` 与 `url` 二选一）：

| 传输 | 说明 |
|---|---|
| **stdio** | `command` + `args`，spawn 本地子进程（NDJSON / JSON-RPC 2.0） |
| **http** | Streamable HTTP（2025-03-26 现行标准）：单端点 POST，支持 JSON / SSE 响应、Mcp-Session-Id |
| **sse** | 旧版 HTTP+SSE（2024-11-05 已废弃协议）：GET 收事件流 + endpoint POST |
| **auto**（默认） | 先按 Streamable HTTP 探测，失败自动回落旧版 SSE |

不支持的边角：Streamable HTTP 的 GET 流（server-initiated 消息）。`examples/mcp_echo.py`（stdio）与 `examples/mcp_http_echo.py`（/mcp + /sse 双端点）是零依赖测试服务器，也是协议参考实现。内置工具白名单（`tools`）与 MCP 工具互不影响。

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
  "tools": ["shell", "read_file", "write_file", "list_dir", "load_image"],
  "mcp_servers": [],
  "limits": {
    "max_turns": 30,
    "timeout_secs": 600,
    "max_output_chars": 30000,
    "compress_threshold_chars": 100000,
    "keep_recent_messages": 8
  }
}
```

API key 从 `api_key_env` 指定的环境变量读取，不写进配置文件。

### 上下文压缩

长任务中历史消息会持续增长直至撞模型上下文上限。当历史总字符数超过
`compress_threshold_chars`（默认 100k，`0` = 关闭）时，agent 会在下一轮调用前
自动压缩：让模型把中段历史写成结构化摘要，折叠进首条任务消息，只保留任务 +
最近 `keep_recent_messages` 条原文。切割点选在 assistant 消息边界，保证
tool_use/tool_result 配对不被切断；压缩失败时告警一次并禁用本次运行的后续压缩
（不中断任务，也不每轮重试刷屏）。运行日志中可见
`[context compressed: dropped N messages, M kept]`。

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

## 结果文件（`--output result.json`）

```json
{
  "success": true,
  "stop_reason": "end_turn",
  "turns": 2,
  "elapsed_secs": 3.1,
  "input_tokens": 1234,
  "output_tokens": 567,
  "summary": "agent 的最终文本回复",
  "tool_calls": [
    { "name": "write_file", "input": {...}, "output": "wrote 42 bytes", "is_error": false }
  ],
  "error": null
}
```

- `tool_calls[].output` 是工具返回的完整内容（`is_error` 标记是否为错误结果）
- `input_tokens` / `output_tokens` 是本次运行累计的 token 用量
- agent.run 返回 `Err`（如 provider 503）时仍会写结果文件：`{"success": false, "stop_reason": "error", "error": "..."}`

进程 exit code：

| code | 含义 |
|---|---|
| `0` | 正常完成（`stop_reason == "end_turn"`）|
| `124` | 超时（`timeout`，沿用 GNU `timeout` 约定）|
| `130` | 用户取消（`cancelled`，128 + SIGINT）|
| `1` | 其它（`max_turns_reached` / `max_tokens` / provider 错误等）|

`--json` 模式额外在每个事件吐一行 NDJSON（`type`: `turn` / `text` / `tool_call` / `tool_result` /
`usage` / `compressed` / `done`），可在结果文件落盘前实时跟进进度。

## 内置工具

| 工具 | 说明 |
|---|---|
| `shell` | 在工作目录执行命令（sh -c / cmd /C），120s 超时后 kill，输出有界读取并按 `max_output_chars` 截断 |
| `read_file` | 读文本文件，带行号，支持 offset/limit；流式读取，超大文件不会整段进内存 |
| `write_file` | 全量重写文件，自动创建父目录 |
| `list_dir` | 列目录，最多 500 条 |
| `load_image` | 加载本地图片让模型"看到"（需视觉模型） |

## 开发

```sh
cargo test --features tui         # 单元测试（含 TUI 输入/渲染逻辑）
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

## 代码结构

```
src/
├── main.rs      (244)  # CLI 入口 + 组装（build_agent）
├── config.rs    (471)  # 配置解析 + 任务片段合并 + 批量覆盖白名单
├── provider/           # LLM 后端
│   ├── mod.rs   (314)  #   共享 Message/Block、SSE reader、CancellableSse
│   ├── openai.rs (574) #   OpenAI 兼容（含 stream_options.include_usage 解析）
│   └── anthropic.rs(400) # Anthropic 原生（message_start/message_delta usage）
├── agent.rs     (758)  # 核心 loop + usage 累计 + cancel + 上下文压缩
├── tools/              # 5 个内置工具 + MCP 工具包装
├── mcp.rs      (648)   # MCP 客户端（stdio / Streamable HTTP / 旧版 SSE）
├── session.rs   (125)  # 会话持久化
├── batch.rs     (475)  # 批量任务 + --retries/--failed + 报告合并
├── harness.rs   (281)  # headless 模式 + result.json + NDJSON + exit code
└── tui.rs      (1218)  # TUI（feature-gated）：InputBuffer / 模式 / 斜杠命令
```
