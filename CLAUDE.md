# TinyAgent — 项目备忘

单二进制、同步（无 tokio）的极简 AI agent。一个 JSON 配置，两种模式：headless（harness / 评测）
与 TUI 对话。详细用法见 `README.md`，本文件记的是**开发时容易踩的坑与约定**。

## 常用命令

```sh
cargo build --features tui                 # 默认只编 headless；TUI 要带 feature
cargo test --features tui                  # 单测（含 TUI 输入/渲染逻辑）
cargo clippy --features tui --all-targets  # lint TUI 版
cargo clippy --all-targets                # lint headless 版（两个 feature 配置都要过）
```

每次改完**两个 clippy 都要跑**：TUI 代码在 `#[cfg(feature = "tui")]` 后，默认构建看不到，
默认配置的 clippy 也看不到 TUI 代码——单跑一个会漏。

## 提交约定

- commit message 用**中文**，**不要包含任何 claude 相关字样**（包括 Co-Authored-By 之类）。
- macOS 上 git 触及中文（commit message / 文件名，仓库里有 `测试模型.md`）会因 iconv `UTF-8-MAC`
  报 fatal。**所有** git 命令前置 `-c core.precomposeunicode=false`：
  `git -c core.precomposeunicode=false status` / `commit` / `log` ……
- message 里有双引号会破坏 `-m "..."` 的 shell 引号——改用 `git commit -F <文件>` 写多行 message。

## 架构要点

- **同步**：`reqwest::blocking` + rustls，没有 tokio。流式用一个 worker 线程 + `CancellableSse`
  （chunk 间 ≤200ms 响应 cancel，30s idle 超时）。
- **两个 provider** 共享 `parse_loop` 与 `Message`/`Block` 类型：`provider/openai.rs`（兼容协议，
  覆盖 DeepSeek/vLLM/Ollama）与 `provider/anthropic.rs`（原生）。两者都解析 token 用量
  （OpenAI 走 `stream_options.include_usage` + `choices:[]` 末块；Anthropic 走
  `message_start`/`message_delta`）。`provider/mock.rs` 是 `cfg(test)` 的脚本化 Provider，
  供 `agent.rs` 的进程内 loop 回归测试用（不连网）。
- **核心 loop 在 `agent.rs`**：累计 usage、cancel 复位、上下文压缩都在这里。`AgentEvent` 透传给
  harness（headless 打印/NDJSON）或 TUI。上下文压缩的摘要请求自身 usage 也计入累计（诚实成本）。
- **headless Ctrl-C** 走 `signal.rs`：首次 SIGINT 翻 cancel 标志（流式 ≤200ms 内断，照常写
  `result.json`，exit 130），二次 SIGINT `libc::_exit(130)` 立即退。handler 只碰 lock-free 原子 + `_exit`
  （async-signal-safe）。**批量模式**（`batch::run`）也装它：cancel 标志由 worker 在起任务前 gate
  （在跑的跑完、未起的标记 `cancelled`、照常合并 report、exit 130）。TUI 不用它——raw 模式下 crossterm 把 Ctrl-C 当普通按键。
- **会话持久化**由 `session.rs` 负责；`--session none`（`main::session_target` 判定字面量 `none`）
  关闭读写，与 `--resume` 互斥（直接 bail）。
- **TUI 是可选 feature**（`ratatui` + `crossterm` + `tui-markdown` + `unicode-width`），
  `src/tui.rs` 在 `#[cfg(feature = "tui")]` 后。默认 headless 版不含它，保持体积。

## 容易踩的坑

- **`panic = "abort"`（`[profile.release]`）**：release 下 panic **不触发 `Drop`**。TUI 的终端还原
  (`TerminalGuard`)、MCP 子进程清理（`StdioTransport::drop`）都依赖 Drop——调试这些行为用
  **debug 构建**。`std::process::exit` 也跳过 Drop，headless 模式退出前显式 `drop(agent)`。
- **`serde_json::json!` 宏按所有权取值**：`From<&Value>` / `From<Option<_>>` 不存在。给宏传
  `&String`/`&Value` 要 `.clone()`；`Option<&str>` 要显式转成 `Value::Null` 或 `json!(s)`。
- **Anthropic 不允许相邻两条 user 消息**（400）；且 N 个 `tool_use` 必须配 N 个 `tool_result`。
  `agent::append_user_turn` 会把新 user 内容折进已有的末尾 user 消息——新增 user 入口走它，别直接 push。
  另有兜底：`agent::run()` 每轮发请求前 `normalize_adjacent` 把相邻同 role 合并（保块顺序，不动配对），
  防 resumed/手改的损坏历史触发 400。别依赖它当正常路径，正常仍走 append_user_turn。
- **cancel 标志一旦 `store(true)` 全库不会自动复位**（无 `store(false)`）。`agent::run()` 入口
  显式复位，所以"取消本轮"后下一条消息能正常再跑。新增触发 cancel 的地方不用管复位。
- **批量任务片段只能覆盖** `task`/`system_prompt`/`model`/`max_tokens`/`limits`；安全字段
  （`provider`/`base_url`/`api_key_env`/`workdir`/`tools`/`restrict_to_workdir`/`mcp_servers`）
  禁止覆盖（`config::forbidden_batch_override`），覆盖即 `bad_task` 失败、不执行。
- **批量 `--retries` 只重试 transient**（`stop_reason == "error"`），指数退避 1..32s 封顶；
  `bad_task`/`max_turns` 等确定性失败不重试。`--failed` 重跑失败子集后**合并**进完整报告
  （未重跑的历史成功项原样保留）。
- **批量 Ctrl-C 是 worker gate，不中断在跑任务**：`batch::run` 的 worker 在**起任务前**检查共享
  cancel 标志，命中则标 `cancelled` 跳过；在跑的任务照常跑完（"收敛"）。**没有**把 cancel 标志传给
  agent 中断流式——因为多 worker 共享一个 flag 会和 `agent::run()` 入口的 `store(false)` 复位竞争
  （一个 worker 起新 run 会清掉另一个在跑的取消信号）。要立刻退出按第二次 Ctrl-C（`_exit(130)`）。
- **OpenAI 400 回退**（`openai::body_fix_for_400`）：错误文本提到 `max_tokens` 时改名为
  `max_completion_tokens` 重试一次（reasoning 模型）；提到 `stream_options` 时丢弃该字段重试一次
  （vLLM 等旧网关，之后 usage 走 `None` 降级，`parse_loop` 已容忍）。未知 400 直接 bail。
  改请求体字段的人要知道这条重试路径，别在 fix 后又引入新 400。
- **`--session none` 是关闭持久化的字面量哨兵**（不是文件名）——真要叫 `none` 的文件没法用，
  但它语义清晰、够用。改 `session_target` 判定前先想清楚。
- **MCP 工具名只消毒展示名，wire 名不动**（`tools::mcp::sanitize_name`）：对外名 `mcp_<server>_<tool>`
  非 `[A-Za-z0-9_]` → `_` 并折叠，但 `wire_name` 保留服务器原始工具名，`tools/call` 用它分发。
  改消毒逻辑别误动 `wire_name`，否则分发会找不到工具。`dedup_check` 在消毒后撞名时启动报错。
- **改 `agent.rs` 核心路径必须配套 loop 测试**。`agent.rs` 的 `#[cfg(test)]` 模块用
  `provider/mock.rs` 的脚本化 Provider 覆盖不变量：cancel 复位、tool_use/tool_result N:N 配对
  （含中途取消时合成的 `[cancelled by user]` 进 history 但不进 `tool_calls`）、压缩、usage 累计。
  动了 loop 逻辑先补/查这些测试，别只靠 mock server 跑一遍就提交。

## 离线 / 视觉验证

- **mock server**：`examples/mock_openai.py`、`examples/mock_anthropic.py` 是零依赖 SSE mock，
  用于无 API key 跑通整个 loop（tool_use 配对、usage 解析、取消）。改动 provider/agent 后用它回归。
- **进程内回归**：`agent.rs` 的 loop 不变量用 `provider/mock.rs`（脚本化 Provider，不连网）单测覆盖，
  比 mock server 更快、断言更准。新增/改动 loop 行为优先在这里加用例。
- **TUI 视觉验证**：用 ratatui `TestBackend` 抓 cell grid 做断言（见 `tui::tests`）；端到端交互用
  pty 脚本驱动（项目里惯用 `/tmp/ta-verify/` 放临时 mock + pty 脚本，跑完即删）。**不要只靠
  "看起来对"**——改了渲染/光标/markdown 要真抓屏或单测断言验证。
