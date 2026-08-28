# Changelog

本文件记录 TinyAgent 的版本演化。格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)（0.x 阶段 minor 即破坏性窗口）。

## [Unreleased]

### Fixed

- **第三方审查（第三轮）确定性修正**（行为变化项均带回归测试）：
  - **预算闸门升级为 fleet 级**：`max_total_tokens` 从"每 agent 各自熔断"（并行子 agent 最坏各烧满一份、
    join 后才发现）升级为 run 级共享花费计数器——父与每个在飞 `task` 子 agent 的直接 chat 花费（轮次
    chat + 压缩摘要）实时累入同一计数器，父与子 agent 的闸门统一读它，并行子 agent **集体**触顶。工具
    账单折算（子 agent usage 入父 totals）刻意不进计数器——子 agent 已自己记过，双计会把有效预算砍半。
    粒度仍为轮次边界（同轮已启动的兄弟互不见对方本轮后续花费）。
  - **`task` 子 agent 继承父剩余墙钟**：`clamp_child_timeout` 把子 agent 的 `timeout_secs` 收紧为父的
    剩余时间（只紧不松、地板 1s），不再克隆完整第二份超时（fleet 墙钟不再 ~2× 配置值）。`timeout_secs`
    的软上限边界（管 chat 请求 / 轮次边界 / 子 agent，不管工具自身上限与绑定 host 回调）写进 README。
  - **工具名装配期统一校验**：全列表（内置 + MCP + skill 注入 + 自定义）必须唯一、仅 `[A-Za-z0-9_-]`、
    ≤64 字符，冲突 / 违规构建即错——此前自定义工具与内置同名被静默遮蔽（分发按首匹配），非法字符 / 超长
    要等首个请求被 provider 400 拒。
  - **`--isolate copy` 符号链接自包含**：源树内相对链接原样复刻（`node_modules/.bin` 类布局继续可用）、
    绝对但在源树内则拷贝目标内容（原样复刻会指回原树、把并行任务重新耦合到共享目录）、解析后出根（绝对
    或 `..` 逃逸）跳过 + stderr 告警——副本里不允许有通往原树或外部的路。
  - **MCP 旧版 SSE reader 线程可观测**：reader 线程命名（`mcp-sse-reader`）+ Drop 只 join 已结束的
    reader（绝不阻塞在活的上）+ 连接失败（endpoint 事件未到）时显式告警而非静默遗留线程与 TCP 连接。
  - **TUI 退出 worker 超时脱离显式告警**：3s 内未退出的 worker 脱离时向 scrollback 打 stderr 提示
    （无取消意识的第三方工具 / host 回调可能仍在后台跑），不再静默。
  - **Node 自定义 provider 等待上限随请求剩余预算**：`min(per_request_timeout, 120s)`——run 设了
    `timeout_secs` 后等待不再可能超出预算挂满 120s；超时文案秒数向上取整（~1s 不再显示 "0s"）。Python
    侧回调契约入档（GIL 内同步、不受 timeout/cancel 强制中断，host 自行设限）。
- **第三方审查（第二轮）全量修正**（每项带回归测试）：
  - **Anthropic 流解析**：识别 `message_stop` 事件主动结束（持连接不关流的网关不再等 30s idle 超时、真实 stop_reason 不被中止覆盖）；`content_block_start` 内联非空 `input` 作 seed（不合规网关内联参数不再丢失）；`tool_result` 序列化避免空 text 块 400（带图空文只发 `[image]`，两者皆空省略 `content` 字段）。
  - **压缩 `keep_recent=0` 空尾合法化**：真实 loop 每个请求点历史必 user 结尾，旧的钳制让该配置下压缩被静默禁用（preflight 不折叠、溢出逃逸判不可压缩直接失败）。现 `cut==n` 合法——除保留头外全部折进摘要；守卫改 `saturating_add`（usize::MAX 不再 debug panic）。
  - **OpenAI learned-fix 改 record-on-success**：400 自适应修复仅在调整后的请求最终拿到 2xx 才落账，值错误（"max_tokens must be ≤ N"）不再毒化修复表（旧逻辑 apply 即记，此后每轮主动发改名后的字段反复触发不可修复的 400）。
  - **batch `--isolate copy` 排除清单只作用于 workdir 根**：嵌套同名目录（`sub/results/`）是任务输入，不再被静默漏拷；Windows symlink 相对目标按链接父目录解析（不再随进程 CWD 漂移）；`worktree add` 锁竞争（index.lock/config.lock）有界重试 ≤3 次。
  - **数值写端全部饱和化**：usage 累计 / RunCost / 压缩摘要成本 / 子 agent trace / batch 汇总的 `+=` 改 `saturating_add`——u64::MAX 级谎报在 debug 下 panic、release 下回绕成小数躲过预算闸门。`read_skill` 磁盘读取加字节上限（对齐 read_file 公式）；`CancellableSse` 线程 spawn 失败改瞬态错误不再 panic。
  - **harness/batch 归因与退出码**：单跑 `result.json` 的 `started_at` 在 attempt 循环前盖章（重试退避不再从时间线上消失）；写结果失败不再吞退出码（130/125/124 保持原值）；batch 扫描跳过 `*.json` 名字的目录（不再让 "Is a directory" 炸掉整批）；失败任务的 report `model` 归因到片段覆盖后的模型；单跑模式 `--report ./report.json` 显式默认拼写不再误拒；batch 分支显式拒 `--task` / 非默认 `--session`。
  - **杂项硬化**：retry 抖动混入调用计数（调度器量化的纳秒让并发失败重新聚成同一节拍）；shell 轮询先 `try_wait` 再查 cancel（同窗口退出的命令拿真实 exit code）；超时/取消路径不再对已 reap 的 pid 二次 killpg（pid 复用窗口）；`list_skill` 每目录 500 条上限 + 截断标记；`read_file` 输出预算按**字符**计（CJK 不再提前 ⅓ 截断），字节上限切开多字节字符时交付合法前缀而非硬报 invalid UTF-8，行数提示在截断时不再谎报总数；MCP `Auto` 传输删独立探测（先完整 HTTP 握手、失败回退 SSE，strict server 只见一次 initialize）；SSE 解析对齐规范（多行 `data:` 以 `\n` 拼接、空行分发、EOF 冲刷、至多剥一个前导空格）；`read_skill` 路径拒 `:`；skill description 上限按字符计（CJK 不再被字节误杀）。
- **shell 工具正常退出路径的管道孤儿挂死**：命令留下的后台进程继承工具的输出管道
  （`server & curl …`，或 `cd d && server >/dev/null 2>&1 & curl …` 这类 `&` 后台化整个
  AND-list 的形态——为其 fork 的子 shell 自己持有管道，内层重定向管不到），直接子进程
  退出后 EOF 永不到达，旧的读线程无条件 `join()` 令整个 turn **永久挂死**：超时/取消
  检查都在轮询循环里、已随子进程退出离开，run 级超时只约束 chat 请求，均救不到。
  现改为三段收尾：宽限 2s 等自然 EOF（期间照常响应取消，秒级恢复）→ 超时则 kill 整个
  进程组强制 EOF 并在输出标注「已回收遗留的后台进程」→ kill 仍无法释放管道
  （`setsid` 逃逸 / Windows）时放弃等待、标注后返回，不再挂死。正确保留后台进程的
  方式是重定向其输出（`cmd >log 2>&1 &`）——不持管道即不触发回收、跨调用存活。

### Changed

- **不可救的上下文溢出归类为确定性失败**（新 `stop_reason` 值 `context_overflow`）：溢出逃逸
  （强制压缩 + 重试一次）失败后不再按瞬态错误走 1+2+4…s 退避——单跑直接终止，且 `result.json`
  与 NDJSON 终态事件同记 `context_overflow`；batch 不重试、report 同口径，CI 可区分溢出与瞬态
  错误。`RunError::is_context_overflow()` 公开判定。
- **batch Ctrl-C 契约显式化**：首次 Ctrl-C 后在跑任务自身跑完，但其 **MCP server 会被立即关闭**
  （防二次 Ctrl-C 硬退留孤儿，0.3.0 修复的既定取舍）——首个命中 cancel 的 worker 打一次性警告
  说明两侧语义。
- **`--isolate git` 检出 HEAD 语义写进 help**：未提交 / 未跟踪文件不进隔离副本，要带上先 commit
  或用 `--isolate copy`。

### Added

- **MIT LICENSE**：仓库此前无许可文件（发布产物默认保留所有权利，第三方无法合法
  复用）。补根 `LICENSE`（MIT）并同步 5 处元数据（根 / 双绑定 `Cargo.toml`、
  `pyproject.toml`、`package.json`）；release tarball 附带 LICENSE。
- **Hooks 拦截层（P1）**：第三方不改核心即可观察与把关 agent 行为。新增
  `hooks` 模块：`trait Hooks: Send + Sync`（三默认方法——`before_tool` 返回
  `ToolDecision::Allow | Deny(String) | Modify(Value)`；`after_tool` 事后审计；
  `on_chat_request` 只读观察主 turn 请求）+ `Agent::set_hooks` 后置安装
  （仿 `set_cancel` 模式；装配默认 None，二进制与绑定行为零变化）。`task`
  子 agent 经 `ToolCtx::hooks` Arc 继承同一层（顶层拒绝 = 子 agent 内也拒绝，
  无绕过）；`ToolCtx::subagent` 标注子 agent 身份供区分。语义要点：Deny 产出
  `is_error` 结果照常入记录与 N:N 配对（**不**占用 `Cancelled` 槽——那是取消
  专用）；Modify 只改**执行**输入，审计链（tool-call 日志 / `ToolCall` 事件）
  保留模型原始输入；顶层闸门在父线程 spawn 前**串行**把关（审批决策互不竞争），
  子 agent 自身的闸门跑在各自 worker 线程上（兄弟真并发，实现方须自行线程安全）；
  压缩摘要请求刻意不透出；`--resume` 重建的 agent 需重新 `set_hooks`。
- **工具能力标记**：`Tool` trait 新增两个默认方法——`concurrency()`
  （`ToolConcurrency::Sequential | Parallel`，决定同 turn 多个调用是否并入并行批）
  与 `available_to_subagent()`（决定是否被 `task` 子 agent 继承）。核心 loop 的并行
  分批与子 agent 白名单自此**按能力路由、不再按 `"task"` / `"todo"` 名字硬编码**；
  第三方工具可自主声明并行类别或退出子 agent 继承，无需改核心。内置覆盖点仅
  `SubAgent`（Parallel + 不可继承）与 `Todo`（不可继承）。`ToolConcurrency` 自
  门面 re-export。

### Changed (Breaking)

- **`ToolCtx` 再增两个公共字段**：`budget: Option<Arc<AtomicU64>>`（fleet 预算计数器的继承通道，
  `Agent::run` 按 run 安装）与 `deadline: Option<Instant>`（父剩余墙钟，供 `task` 子 agent 收紧自身
  超时）。外部**构造** `ToolCtx` 字面量的代码需补 `budget: None, deadline: None`（本仓库内全部构造点
  已同步；绑定层经 `build_agent_with` 装配、零改动）。
- **`ToolCtx` 新增两个公共字段**：`hooks: Option<Arc<dyn Hooks>>` 与
  `subagent: Option<String>`。外部**构造** `ToolCtx` 字面量的代码需补
  `hooks: None, subagent: None`（本仓库内 29 处构造点已同步）；只接收
  `&ToolCtx` 的工具实现不受影响。
- **`AgentEvent` / `Block` / `ChatEvent` 加 `#[non_exhaustive]`**：三个消费侧枚举
  （第三方 `match` 的对象）后续新增变体不再需要版本承诺，外部 `match` **必须保留
  通配臂**，否则升级时编译错误。迁移一行：补 `_ => {}`。serde 序列化/反序列化不受
  影响（按 tag 走，非穷尽匹配）。**卫生标准**：只给消费侧枚举加；`Role`
  （user/assistant 封闭轴）与 `CompressTrigger` 明确不加，生产侧枚举（第三方构造、
  核心消费）今后也一律不加。配套改动：`src/tui.rs` 主事件 match 增一行 `_ => {}`
  （编译适配，对现存变体渲染逐字节不变）。

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
