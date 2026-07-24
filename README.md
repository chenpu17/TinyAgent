# TinyAgent

一个极简的单二进制 AI agent：一个 JSON 配置即可运行。两种模式 —— headless 执行任务（harness 场景）或 TUI 对话。目标：嵌入式低配 Linux、macOS、Windows，工具精炼、代码量小（~1500 行）、一个晚上能读完并自行定制。

## 特性

- **单一可执行文件**，无运行时依赖；musl 静态编译后可丢到路由器 / 树莓派 / CI runner 直接跑
- **两种 LLM 后端**：Anthropic 原生 API + OpenAI 兼容协议（覆盖 DeepSeek / vLLM / Ollama 等）
- **图片输入**：`--image` 参数或 `load_image` 工具（png / jpeg / gif / webp，base64）
- **精炼工具集**：`shell`、`read_file`、`write_file`、`list_dir`、`load_image`，配置白名单启用
- **Harness 契约**：`--output result.json` 输出结构化结果，exit code 0/1，可直接进 CI / 评测 pipeline
- **TUI（可选 feature）**：ratatui 对话界面，默认不编译，保持 headless 版极小
- **同步实现**（无 tokio），核心 agent loop ~150 行

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

# 附带图片（视觉模型）
tiny-agent --config examples/harness.json --task "描述这张截图" --image shot.png

# 会话持久化：每轮自动落盘，--resume 继续上次会话
tiny-agent --config examples/chat.json --task "记住暗号 PINEAPPLE" --session sess.json
tiny-agent --config examples/chat.json --resume --task "暗号是什么？" --session sess.json

# 批量任务模式：跑目录下所有任务片段，2 个并发，输出汇总报告
tiny-agent --config examples/harness.json --batch examples/tasks --jobs 2 --report report.json

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
- `--jobs N` 控制并发（默认 1），`--report` 写汇总：`{total, succeeded, failed, tasks[]}`
- exit code：全部成功 → 0，否则 1，可直接做 CI 门禁

## 会话持久化

- `--session <path>` 指定会话文件（默认 `./.tinyagent-session.json`），每轮结束原子落盘
- `--resume` 加载历史继续；此时 config 里的 `task` 视为已消费，用 `--task` 追加新指令
- 崩溃也不丢已完成的轮次；TUI 模式同样生效

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
tool_use/tool_result 配对不被切断；压缩失败时告警并继续（不中断任务）。
运行日志中可见 `[context compressed: dropped N messages, M kept]`。

## 结果文件（`--output result.json`）

```json
{
  "success": true,
  "stop_reason": "end_turn",
  "turns": 2,
  "elapsed_secs": 3.1,
  "summary": "agent 的最终文本回复",
  "tool_calls": [{ "name": "write_file", "input": {...}, "is_error": false }]
}
```

进程 exit code：`success` 为 true → 0，否则 1（含 `max_turns_reached` / `timeout`）。

## 内置工具

| 工具 | 说明 |
|---|---|
| `shell` | 在工作目录执行命令（sh -c / cmd /C），120s 超时，输出按 `max_output_chars` 截断 |
| `read_file` | 读文本文件，带行号，支持 offset/limit |
| `write_file` | 全量重写文件，自动创建父目录 |
| `list_dir` | 列目录，最多 500 条 |
| `load_image` | 加载本地图片让模型"看到"（需视觉模型） |

## 开发

```sh
cargo test                       # 单元测试
# 无 API key 的端到端冒烟测试：
python3 examples/mock_openai.py 8791 &
cd $(mktemp -d) && MOCK_API_KEY=dummy tiny-agent --config <指向 mock 的配置> --output result.json
```

`examples/mock_openai.py` 是一个 ~80 行的 OpenAI 兼容 SSE mock server，用于离线验证整个 agent loop。

## 代码结构

```
src/
├── main.rs          # CLI 入口 + 组装
├── config.rs        # 配置解析 + 任务片段合并
├── provider/        # LLM 后端（anthropic.rs / openai.rs，共享 Message/Block 类型）
├── agent.rs         # 核心 loop（~150 行）
├── tools/           # 5 个内置工具 + MCP 工具包装
├── mcp.rs           # MCP stdio JSON-RPC 客户端
├── session.rs       # 会话持久化
├── batch.rs         # 批量任务模式
├── harness.rs       # headless 模式 + result.json
└── tui.rs           # TUI（feature-gated）
```
