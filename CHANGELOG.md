# Changelog

本文件记录 TinyAgent 的版本演化。格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)（0.x 阶段 minor 即破坏性窗口）。

## [Unreleased]

## [0.3.0] — 2026-08-16

### Added

- **GitHub Actions CI**（`.github/workflows/ci.yml`）：三平台（ubuntu/macos/windows）×
  双 clippy 配置（`--all-targets` 与 `--features tui --all-targets`，均 `-D warnings`）+
  `cargo test --features tui,mock`；Python（maturin + pytest）与 Node（napi + node --test）
  绑定测试跑 ubuntu；musl x86_64 静态构建冒烟。
- **result.json 归因字段**：`schema_version` / `model` / `provider` / `started_at` /
  `finished_at`（成功与失败路径都写）。审计时能回答"这份账单是哪个模型、什么协议、
  什么时候跑出来的"——`--model` A/B 对照后两份结果可区分。纯增量键，按 key 取值的
  消费方不受影响。时间戳为手写 RFC 3339 UTC（Hinnant days-from-civil，无 chrono 依赖）。
- **batch report 归因字段**：`tasks[]` 每条带 `model` / `started_at`（任务片段可覆盖
  model，同批混模型时靠它区分）；顶层带 `provider`。`#[serde(default)]` 保证旧版
  report.json 的 `--failed` 合并仍可解析。
- **`limits.shell_timeout_secs`**（默认 120，0 = 单命令不限时）：评测构建 / 安装常超
  120s，此前任务被砍死写进账单成 error。子 agent 透传同值；超时报错文案动态化。
  这是 limits 里**唯一允许批量任务片段双向覆盖**的字段（成本旋钮非安全边界）。
- **batch `--isolate none|copy|git`**：任务级 workdir 隔离。`copy` 每任务拷贝（排除
  `target`/`node_modules`/`.git`/`results`）；`git` 每任务 `worktree add --detach`
  （要求 workdir 是 repo 根）。per-task、跨 attempt 复用；产物随目录丢弃（审计面是
  result.json / report.json）。`--jobs > 1` 的共享 workdir 警告只在 `none` 模式打。
- Skill 技能系统（`skills.rs` + `read_skill`/`list_skill` 工具），对齐 agentskills.io
  开放标准：6 字段 frontmatter、三级渐进式披露、项目覆盖全局、坏技能跳过告警不 fatal。
- 任务计划工具 `todo`（全量替换语义）+ 审计 `meta` 侧信道：最终计划落 result.json 的
  `plan` 字段（从 tool-call 日志派生，不怕压缩吃掉）；`task` 子 agent 条目带紧凑 trace。
- 运行环境事实注入 system prompt（OS / Shell / 工作目录，`inject_environment` 默认开）：
  模型知道自己跑在哪个平台、下发兼容命令。
- 思考内容透出（`ThinkingDelta` 事件，不进历史）+ `reasoning_effort` 程度配置
  （字符串原样透传不校验；Anthropic 走 adaptive thinking + 独立 `output_config`）。
- 语言绑定：Python（PyO3）/ Node（napi-rs），复用 `CallbackTool` 与自定义 provider
  回调（非流式 `SimpleChatResponse`），不改核心代码。
- 库 API：bin + lib 同包双 target（`tiny_agent`），第三方 Rust 程序可直接驱动 agent
  loop / 注册自定义工具 / 读回诚实 token 账单；`mock` feature 放出脚本化 provider。
- `--config` 可选：按 `--config` → `$TINYAGENT_CONFIG` → XDG 目录顺序自动发现；
  裸 `tiny-agent` 在交互终端默认进 TUI；`install.sh` 一键装机。
- MCP stdio `env` 注入（叠加宿主环境，GUI 进程传凭据）；OpenRouter `delta.reasoning`
  思考字段兼容；Windows 编译修复 + headless Ctrl-C（`SetConsoleCtrlHandler`）。

### Fixed

- 第三方审计问题全量修正（含回归守卫）：reaper 误杀 / 失败路径丢成本 / 退出-重试-取消
  健壮性 / OOM 边界 / 审计字段一致性等。
- TUI 多行输入框塌缩 + MdCache 按 (text, width) 寻址，防 scrollback 渲染污染。
- 压缩边界：`keep_recent=0` 失效 / `fold_summary` role / summary `max_tokens` 封顶。
- Windows：`cmd` 输出按 OEM 代码页解码（中文不再乱码）、MCP stdio 走 `cmd /C`。
- `write_file` 原子化（临时文件 + rename，防半截文件）；skill frontmatter 剥 UTF-8 BOM。
- batch 补 `set_shutdown`（二次 Ctrl-C 后 MCP 子进程不再成孤儿）；Anthropic 流内 error
  透出已计 usage；`--jobs 0` 直接报错而非静默串行；绑定层透出 `RunError` 结构化成本。

## [0.2.0] — 2026-08-04

### Added

- TUI 从全屏 alternate-screen **迁移到内联/流式渲染**：对话提交进终端原生 scrollback
  （原生选择 / 滚动 / 搜索直接作用于真实文本，根治选区漂移），底部固定 live 区。
- 上下文压缩重构：**前馈（preflight）/ 溢出（overflow）双触发**——token 估算阈值 +
  provider 400 兜底强制压缩重试；滚动分块摘要（~6:1 压缩比，保路径/报错细节）。
- `edit_file` 工具（str_replace 精确替换，唯一匹配校验，复用 workdir 沙盒）。
- `task` 子 agent：独立上下文 + turn 封顶；**同 turn 多调用并行**（std::thread::scope
  fan-out），事件带 name 身份实时转发，usage 折进主 run（诚实成本）；复用父级 MCP 连接。
- Usage 扩 4 字段（cache_creation / cache_read）+ Anthropic prompt caching
  （system / tools / 末条消息三断点）。
- 单跑 headless `--retries` 瞬态退避重试；batch Ctrl-C 优雅取消（首次 gate / 二次硬退）。
- 大量端到端集成测试：真实构建（agent 用真工具写 Python web 服务并验证）、NDJSON
  事件契约、压缩 N:N 配对断言。

### Fixed

- 多轮外部审核修复（一致性 / 恢复 / 健壮性）：排队水位线、worker 泄漏、退避取消、
  渲染溢出、缓存门控、usage 双计、原子写等数十项。

## [0.1.0] — 2026-07-24

### Added

- 初始发布：单二进制、同步（无 tokio）的极简 AI agent；一个 JSON 配置，headless
  （harness / 评测）与 TUI 两种模式。
- 双 provider：Anthropic 原生 + OpenAI 兼容协议（DeepSeek / vLLM / Ollama），流式 +
  token 用量统计。
- 内置工具：shell / read_file / write_file / list_dir（+ 后续版本陆续加入的其它工具）。
- 批量任务模式（`--batch` / `--jobs` / `--report`）；MCP 客户端（stdio / Streamable
  HTTP / 旧版 SSE）；上下文压缩（字符阈值启发式）；会话持久化（`--session` / `--resume`）；
  headless Ctrl-C 优雅取消；exit code 语义化（0/124/130/1）。
