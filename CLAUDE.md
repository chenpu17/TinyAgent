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

### 语言绑定（workspace 成员 `bindings/python`、`bindings/node`）

两者 path-依赖根库（`features = ["mock"]`），都复用 `CallbackTool` 注入自定义工具、不改核心。改了核心
provider/agent 后，两个绑定的测试都要跟着跑一遍回归。

```sh
# Python（PyO3 + maturin；raw cargo build 链接不了 cdylib，必须走 maturin）
cd bindings/python && maturin develop && python -m pytest tests/ -q

# Node（napi-rs；index.js/index.d.ts/*.node 是 napi build 生成的，已 gitignore——别提交）
cd bindings/node && npm run build:debug && npm test   # node --test --test-force-exit，离线
cargo clippy -p tiny-agent-py          # 绑定 crate 也要单独过 clippy
cargo clippy -p tiny-agent-node
```

## 提交约定

- commit message 用**中文**，**不要包含任何 claude 相关字样**（包括 Co-Authored-By 之类）。
- macOS 上 git 触及中文（commit message / 文件名，仓库里有 `测试模型.md`）会因 iconv `UTF-8-MAC`
  报 fatal。**所有** git 命令前置 `-c core.precomposeunicode=false`：
  `git -c core.precomposeunicode=false status` / `commit` / `log` ……
- message 里有双引号会破坏 `-m "..."` 的 shell 引号——改用 `git commit -F <文件>` 写多行 message。

## 架构要点

- **bin + lib 同包双 target**：`Cargo.toml` 同时声明 `[lib]`（`tiny_agent`，src/lib.rs）与
  `[[bin]]`（`tiny-agent`，src/main.rs）。核心模块（agent/compress/config/provider/tools/mcp/
  harness/batch/session/retry）属 lib，门面 re-export 在 `lib.rs`；`src/main.rs` 只保留进程级薄壳
  （clap 参数、SIGINT、`process::exit`、可选 `mod tui`），靠 `pub(crate) use tiny_agent::{...}`
  把 lib 模块拉进 bin crate 根——故 main.rs/tui.rs 里 `crate::agent::...` 等路径仍解析。
  **改 lib 内部代码不用动 bin**；bin 行为是 lib 之上的薄壳。lib 内 `crate::` 指 lib 根（lib.rs 声明了
  全部 `pub mod`），所以模块互引零改动。`mock` feature 把 `provider::mock`（否则 `cfg(test)`）放出，
  供绑定/下游离线 dry-run。自定义工具共享底座 `tools::CallbackTool`（包闭包，impl `Tool`）——Python/TS
  绑定的 callable 经 GIL/ThreadsafeFunction 闭包塞进来，共享同一 `Tool` impl。

- **同步**：`reqwest::blocking` + rustls，没有 tokio。流式用一个 worker 线程 + `CancellableSse`
  （chunk 间 ≤200ms 响应 cancel，30s idle 超时）。`sse-reader` 线程阻塞在 reqwest `read()` 里、读不到
  cancel 标志，所以**每个流式请求必带 per-request timeout 兜底**（`STREAM_BACKSTOP`，10min；有 run budget
  时改用剩余预算）——否则半开连接会让 reader 线程在 caller 已放弃（idle/cancel）后无限挂住（线程+连接泄漏）。
  reqwest blocking 没有 idle-read timeout，per-request total cap 是唯一能保证 reader 退出的手段。
- **两个 provider** 共享 `parse_loop` 与 `Message`/`Block` 类型：`provider/openai.rs`（兼容协议，
  覆盖 DeepSeek/vLLM/Ollama）与 `provider/anthropic.rs`（原生）。两者都解析 token 用量
  （OpenAI 走 `stream_options.include_usage` + `choices:[]` 末块；Anthropic 走
  `message_start`/`message_delta`）。`provider/mock.rs` 是 `cfg(test)` 的脚本化 Provider，
  供 `agent.rs` 的进程内 loop 回归测试用（不连网）。
- **思考（reasoning/thinking）透出 + 程度配置**：`ChatEvent`/`AgentEvent` 各有 `ThinkingDelta(String)` 变体
  （`AgentEvent` serde 外部 tag → `{"ThinkingDelta":"..."}`，TinyWorker 后端零改动接到）。**只透出不入历史**——
  openai 解析 `delta.reasoning_content`（DeepSeek/GLM 官方）或 `delta.reasoning`（OpenRouter 归一化各厂商思考，`reasoning_details` 数组忽略——`reasoning` 是其纯文本投影）、anthropic 解析 `thinking` content block（`content_block_start` 的 type=
  `thinking`/`redacted_thinking` → `Pending::Thinking`；`thinking_delta` 增量在 `delta.thinking`，`signature_delta`
  忽略），都只发事件、**不**进 `text`/`blocks`（`finalize_pending_lenient` 对 `Thinking` 返 `None`）。`reasoning_effort`
  是 `Option<String>` **按字符串原样透传、不做枚举校验**（各家档位碎片化且随代际变动，别加枚举）：openai 直接发
  `reasoning_effort` 字段；anthropic 走 adaptive thinking（`Some(non-none)` → `thinking:{"type":"adaptive"}`+
  `output_config:{"effort":...}`——effort 在**独立顶层 `output_config` 对象**里，**不**在 thinking 内、**不**是裸
  顶层字段（裸顶层会被静默忽略，这是 API 头号坑），`Some("none")` → `{"type":"disabled"}`，`None` 不加字段）——**不走老 `budget_tokens`**（Opus 4.7 起传了 400）。
  `Config`/`ChatRequest`/`Agent`/`PartialConfig` 四层透传；`summary_call` 传 `None`（压缩摘要不思考）。子 agent 复用
  父 cfg 自动继承。软字段（同 `model`/`max_tokens`），允许 batch 任务片段覆盖，不在 `forbidden_batch_override`。
- **核心 loop 在 `agent.rs`**：累计 usage、cancel 复位、上下文压缩的有状态编排（`compress`/
  `summarize_prefix`/`summary_call`，读写 Agent 字段 + 驱动 provider）在这里。压缩的**纯函数与常量**
  （`estimate_tokens`/`find_safe_cut`/`fold_summary`/`window_lines`/`chunk_lines`/`SUMMARY_*` 等）抽到
  `compress.rs`（单向被 agent 依赖；`window_lines` 反向调 `crate::agent::preview`，不构成环）。`AgentEvent`
  透传给 harness（headless 打印/NDJSON）或 TUI。上下文压缩的摘要请求自身 usage 也计入累计（诚实成本）。
- **token 预算闸门**（`limits.max_total_tokens`，0=不限）：run loop 每轮开头用累计 usage 的
  `Usage::total()`（input+output+cache 写+读）比对预算，超则以 `stop_reason: "budget_exceeded"`
  （exit 125）停。turn 边界语义同 `timeout_secs`——顶破上限的那轮照常跑完（工具执行、assistant 入历史），
  闸门只拒开下一轮，故 transcript 与 tool_use/tool_result 配对不破。batch task 片段只能收紧（更小值），
  0 视为"无上限"被忽略。闸门复用已累计的 usage，不加新的记账；它和 `max_turns`/`timeout` 是 loop 的
  三个 turn 边界停止条件。
- **子 agent（`task` 工具，`tools/subagent.rs`）**：主 agent 把聚焦子任务交给一个全新子 agent
  （独立历史 + 独立 turn 预算，`SUBAGENT_MAX_TURNS=25` 封顶），只把最终摘要作 tool 结果返回。
  **同 turn 多个 `task` 调用并行**：`agent.rs` 的工具执行分两遍——叶子工具（shell/read_file/…）
  顺序跑，`task` 调用收集后用 `std::thread::scope` fan-out（每个一个 scoped thread），结果按原块序
  拼回（tool_use/tool_result 仍 N:N）。子 agent 的 token 用量经 `ToolOutput::usage` 折进主 run 累计
  （失败也照计已完成轮次）。事件实时转发，裹成 `AgentEvent::SubAgent{name}` 区分并行的几个。
  **不可递归**：子 agent 的工具白名单滤掉 `task` 本身（无深度计数）。**在默认白名单**（`config::default_tools`
  含 `task`）——headless 评测任务也会默认能 spawn 子 agent；评测若要锁定行为/成本，在 config 的 `tools`
  里显式去掉 `task`。
- **任务计划（`todo` 工具，`tools/todo.rs`）+ 审计 `meta` 通道**：`todo` 是无状态工具（`&self`，与 shell 同），
  **全量替换语义**（每次发完整计划，TodoWrite 式）。两条关键设计：① **`plan` 从 tool-call 日志派生，不经压缩**——
  `agent::extract_plan` 取 `records`（`Vec<ToolCallRecord>`，独立于 `self.messages`）里最后一条非 error 的 `todo` 调用的
  `input`，在 `harness::write_result` 里落进 `result.json` 的 `plan` 字段。压缩只重写 `messages`、不碰 `records`，故计划
  状态不怕被摘要吃掉。**不加 Agent/ToolCtx/Session 字段、零状态机**——records 已流穿重试/失败/批量全路径（`RunCost.tool_calls`
  自动带 plan）。`AgentEvent::TodoUpdate{todos}` 经 `run_with_emit` 的 `emit` 流式转发（`--json` 吐 `{"type":"todo",...}`）。
  ② **审计 `meta` 侧信道**：`ToolOutput` 加 `meta: Option<Value>`、`ToolCallRecord` 同步加 `meta`——**不进模型可见的 `content`**
  （保主上下文清爽），只进 `result.json`。`task` 子 agent 的 `outcome_to_output`/错误路径用它挂紧凑 trace（`turns`/`tools` 按名
  计数/`tokens`/`stop_reason`）。`write_result` 用 `if let Some(m)=&c.meta { entry["meta"]=m.clone() }` 后插，**稀疏**（只有
  task 条目出 `meta` 键，叶子工具无）；`plan` 同款条件后插（没用 todo 时无 `plan` 键，与启用前逐字节相同）。**默认白名单含 `todo`**；
  **子 agent 不继承**（`subagent.rs` 白名单 filter 把 `"task"` 和 `"todo"` 一并滤掉——子 agent 是聚焦短任务，规划归父）。
  `Slot::Ran(Box<ToolOutput>)` 装箱（ToolOutput 加 meta 后跨 `large_enum_variant` 阈值）。`todo` 模块 `pub(crate)`（测试按路径取 `Todo`）。
  改 `ToolOutput`/`ToolCallRecord` 字段的人：**两个绑定（`bindings/{python,node}/src/lib.rs`）构造 `ToolOutput` 的地方要补 `meta: None`**，
  核心构造点还有 `tools/{image,mcp,subagent}.rs` + `agent_tests.rs`。
  **TUI 配合（状态行 plan 段，破冻结的唯一例外）**：TUI 主循环持 `plan: Vec<TodoItem>` 局部状态，`TodoUpdate` arm 直接 `plan = todos.clone()`
  （全量替换，不 push sys_info 行——完整 checklist 已在 `todo` 工具结果行可见）。每帧把 `plan done/total`（全完成绿/进行中黄）作为
  `build_status_line` 的 `plan: Option<(&str, Style)>` 段渲染。`build_status_line` 的可选段按**存活优先级**贪心添加：ctx > plan > stats
  （窄屏先省 stats、再 plan、ctx 最后），用一个 `Vec<(String,Style)>` + 累积 `prefix_w` 的通用循环取代旧的 if/else-if；**plan=None 时行为与
  启用前逐字节相同**（既有 `build_status_line_*` 测试全过）。TUI 的"冻结只修 bug"政策对此**破例一次**（产品决定）；其余 TUI 改动仍守冻结。
- **Skill（技能，`skills.rs` + `tools/skill.rs`）**：对齐 **agentskills.io 开放标准**（不是 Claude Code 私有扩展——
  `disable-model-invocation`/`context:fork`/`$ARGUMENTS`/`!cmd` 注入/嵌套惰性发现等依赖 CC 特有会话/权限/子代理模型，
  不实现）。启动时（`lib.rs::assemble`）扫描全局 `~/.config/tinyagent/skills/`（`skills::default_global_skills_dir`
  镜像 `main.rs` 的手写 XDG 定位，无 dirs crate）+ 项目 `<workdir>/.tinyagent/skills/`，项目覆盖全局同名
  （`scan` 按 name 排序去重）。每个 skill 是子目录含 `SKILL.md`（YAML frontmatter + 正文）。
  **手写 6 字段 frontmatter 解析（无 YAML 依赖）**：必填 `name`/`description`；可选 `license`/`compatibility`（单行标量）、
  `allowed-tools`（空白拆成 `Vec`）、`metadata`（缩进 `key: value` 块 → `HashMap`，`in_metadata` 状态机）。多行 YAML 折叠
  不支持；`metadata` 是唯一支持块的字段。**`SkillEntry` 带全 6 字段**，但 level-1 简介只透出 name+description（license/
  compatibility/metadata 仅存储不进 system——简介须短）。**name 严格校验**：1-64 字符、仅 `[a-z0-9-]`、首尾非 `-`、无 `--`、
  **必须等于父目录名**（`valid_skill_name` + scan 内 `name == dir.file_name()` 校验）；`description` ≤1024；任一不过 →
  warning+skip。三级加载：①简介拼进 **system_prompt**（常驻——压缩只动 messages 不动 system，故简介安全；但计入
  `estimate_tokens`，必须短）②全文（SKILL.md 正文）经 `read_skill` 读进 **history**（tool_result，可被压缩回收，**不**常驻
  system）③资源文件（`scripts/`/`references/`/`assets/` 等子目录）。**`read_skill` 放开子目录**：`file` 用 `safe_relative_path`
  （允许 `/`，禁 `..`/绝对/`\`/空/`.` 组件）+ `confined_to(dir, full, "skill directory")` 双层沙盒——后者复用 `tools/mod.rs`
  抽出的 `pub(crate) confined_to(root, full, label)`（`confine` 委托它，root 在内 canonicalize；TOCTOU caveat 同 workdir 沙盒）。
  **`list_skill` 工具**列技能目录树（`walk_skill`，`file_type` 不跟随 symlink 防逃逸、深度封顶 4、跳隐藏），供模型 level-3 自主
  发现资源。`read_skill`/`list_skill` 都**不在 `cfg.tools` 白名单**（skill≠tool，刻意决策）——扫到技能时由 `assemble` 自动配套注入
  （`tool_list.push(ReadSkill)` + `push(ListSkill)`，各 `index.clone()`）。**子 agent 不继承**：`subagent.rs:133` 硬编码
  system_prompt 覆盖为 `SUBAGENT_SYSTEM_PROMPT`，且子 agent 走白名单建工具集，故既无简介也无 `read_skill`/`list_skill`
  （决策，不改 subagent.rs）。坏 skill（缺 SKILL.md / 缺 frontmatter 字段 / name 不合规 / name≠目录 / description 超长）
  跳过 + stderr `[skill]` 告警，**不 fatal**。**`allowed-tools` 仅告警不强制**（标准标 Experimental；TinyAgent 无 per-turn 激活层，
  真"临时预批准"不做）：`assemble` 遍历 `e.allowed_tools`，不在 `cfg.tools` ∪ {`read_skill`,`list_skill`} 的 → stderr 告警；
  字面量名匹配，不支持 glob/`Tool(spec)`。`assemble` 为注入简介 **clone cfg**（改 `cfg2.system_prompt`）并返回
  `(tool_list, ctx, cfg)` 三元组（带 `#[allow(clippy::type_complexity)]`）——`Agent::new` 从该 clone 读 system_prompt；改
  `assemble` 签名的人注意两个调用点（`build_agent_with` / `build_agent_with_provider`）都要拿返回的 cfg 传给 `Agent::new`。
- **运行环境事实注入 system prompt**（`agent.rs::system_with_env` + `Agent::new` 单点）：构造 agent 时在
  system prompt 最前面插一段 `# Environment`（OS / Shell / 工作目录），让模型知道自己跑在哪个平台、据此下发兼容命令
  （否则默认 bash 语法在 Windows `cmd /C` 下静默失效还 exit 0，如 `echo $((555+666))` 原样回显）。**只给事实不给语法示例**：
  OS 用 `std::env::consts::OS`、Shell 用 `shell` 工具真实调用形式（unix `sh -c` / windows `cmd /C`，查 `tools/shell.rs`）、
  cwd 用已解析的 `ctx.workdir`。注入点在 `Agent::new` 的 `system` 赋值——**唯一收敛点**：主 agent 与 `task` 子 agent 都过
  这一行（子 agent 先在 `subagent.rs` 覆盖 system_prompt 为 `SUBAGENT_SYSTEM_PROMPT` 再调 `Agent::new`，故子 agent 也覆盖；
  改 `assemble` 注入会漏子 agent）。`system_with_env(base, os, shell, workdir)` 是 `pub(crate)` 纯函数且 os/shell **作参数**
  （不硬编码），以便 off-platform 单测 windows+cmd / unix+sh 两条路径。`Config.inject_environment`（默认 true，软字段，仿
  `reasoning_effort`，**不**在 `forbidden_batch_override`）开关。顺序：`assemble` 已把 skill 简介拼在 base 之后，这里再前插
  env → 最终 `env + base + skills`。测试 config（`cfg_with` / `mock_agent` / `fake_cfg` / `minimal_config`）显式设
  `inject_environment: false` 保 system prompt hermetic（MockProvider 不捕获 `req.system`，注入本身零行为漂移）。
- **`retry.rs`** 是 batch `--retries` 与单跑瞬态重试共用的退避模块：指数 1/2/4/8/16/32s 封顶 + ±25%
  jitter（防并发 worker lockstep 踩踏同一个过载 provider）。只重试 transient（`stop_reason == "error"`）。
  **重试诚实成本**：`agent.run()` 失败返回 `RunError`（带 `usage` + `tool_calls`——失败轮已烧的 token / 已跑的
  tool，副作用已落盘）；`harness::run_with_retries` 与 batch `run_one` 用 `RunCost` 累计失败轮成本：成功则 fold 进
  最终 outcome 再写 `result.json`（`RunCost::fold_into`）；**全部失败**（重试耗尽 / 取消）也把 acc 传给
  `write_result` 的 None 分支（`write_result(..., Some(&acc))`）写入 token + tool_calls，而不是清零；batch 的
  `TaskReport` 同样带 4 个 token 字段，失败任务的 report.json 照计 acc（`run_one_attempt` 的 Err 路径用 `&acc.usage`，
  取消路径亦然）。`RunCost::add_failed`/`fold_into` 是共用的累加原语。改重试/返回类型/写结果文件的人要保住这条。
- **headless Ctrl-C** 走 `signal.rs`：首次按 Ctrl-C 翻 cancel 标志（流式 ≤200ms 内断，照常写
  `result.json`，exit 130），二次立即退 130。**unix** 用 `signal(SIGINT, …)`，handler 只碰 lock-free
  原子 + `libc::_exit`（async-signal-safe）；**Windows** 用 `SetConsoleCtrlHandler`（kernel32 FFI，
  无新依赖），handler 跑在独立线程（非中断），首次返回 TRUE 翻 flag（抑制默认终止，让 run 在 cancel
  检查处中断并写 result.json），二次 `process::exit(130)`。两平台同构：`CANCEL_PTR` 静态 + 计数
  （首次优雅 / 二次硬退）。**批量模式**（`batch::run`）也装它：cancel 标志由 worker 在起任务前 gate
  （在跑的跑完、未起的标记 `cancelled`、照常合并 report、exit 130）。TUI 不用它——raw 模式下 crossterm 把 Ctrl-C 当普通按键。
- **会话持久化**由 `session.rs` 负责；`--session none`（`main::session_target` 判定字面量 `none`）
  关闭读写，与 `--resume` 互斥（直接 bail）。
- **TUI 是可选 feature**（`ratatui` + `crossterm` + `tui-markdown` + `unicode-width`），
  `src/tui.rs` 在 `#[cfg(feature = "tui")]` 后。默认 headless 版不含它，保持体积。
  **内联/流式渲染**（非全屏 alternate-screen，与 Claude Code / Codex 同款）：
  `Terminal::with_options(Viewport::Inline(INLINE_HEIGHT=8))` 在主屏底部锚定一个固定高度 live 区
  （流式 zone + 输入框 + 状态行），每帧 `term.draw` 重绘；已定稿的对话块经 `term.insert_before`
  **提交进终端原生 scrollback**（`commit_rows` + `committed` watermark 追踪已提交行）。原生选择 /
  滚动 / 搜索交给终端——**没有鼠标 handler、没有 app 托管的滚动/选区**（旧全屏重绘模型的选区漂移
  bug 由此根治）。流式中的在途 Assistant 行由 watermark 暂留（不提交）、live 区里带 `▌` 光标渲染、
  回合结束（stream 关闭）才整条提交。**`ratatui` 的 `scrolling-regions` feature 已在 Cargo.toml 启用**：
  默认回退路径每次 `insert_before` 都 `Terminal::clear()`（发一个 DSR 光标查询 `\x1b[6n`），开了
  scrolling-regions 走 CSR(DECSTBM) 滚动区路径、无此查询且渲染更平滑（CSR 现代终端普遍支持）。

## 容易踩的坑

- **语言绑定的工具回调是跨线程 round-trip，不能简化成同步直调**：
  - **Python**（`bindings/python`）：`Agent` 存进 `Mutex<Agent>`，`run()` 经 `py.allow_threads` **释放 GIL**
    跑 loop；自定义工具的闭包在 agent 线程执行，用 `Python::with_gil` **短暂重获 GIL** 调回 Python。
    `set_cancel(token)` **必须在 `run()` 之前**调（把共享 `Arc<AtomicBool>` 装进 agent）——run 期间持有
    mutex，`run()` 入口不复位 cancel（与主 agent 一致）。改这条别在 run 持锁期间调会阻塞主线程的同步方法。
  - **Node**（`bindings/node`）：`run()` 是 `AsyncTask`（libuv worker 线程，**无 tokio**），事件经
    `ThreadsafeFunction` 回主线程。自定义 JS 工具是 **TSFN round-trip**：agent 线程把 `{id,args}` 经 TSFN
    派给 JS、阻塞 `mpsc` 等结果；JS（`wrapper.js` 把 `run(args)` 包成 `(err,req)` 形状）调完用户函数后调
    导出的 `resolveTool(id, result)`，经全局 `PENDING_TOOLS: i64→Sender` 注册表唤醒 worker（120s
    `recv_timeout` 兜底）。**JS 主线程在 `run()` 期间必须空闲**（调用方 `await`），否则 TSFN 回调排不进
    主循环 → worker 永久阻塞 → 死锁。`resolveTool` / `PENDING_TOOLS` / `build_extra_tools` 三者是一套，
    改其一要保住另两个。`env.create_threadsafe_function` 固定返回 `CalleeHandled`（JS 收 `(err,value)`），
    所以 wrapper 的 `run` 签名是 `(err, req)`——别改成单参。
- **绑定层自定义 provider 回调**（`provider=` / `{ chat }`）：让 Python/JS 函数整个替换内置 HTTP
  provider（不连网、不要 key）。**非流式 v1**：host 函数收 `{system, messages, tools, model, max_tokens}`，
  返回 `str` 或 `{text, stop_reason?, usage?}`（`SimpleChatResponse`）；核心 `ChatResponse::from_simple`
  把整段文本合成**单条** `TextDelta`。不支持 tool_use（要工具调用用内置 provider + 自定义 `Tool`）。
  - **共享装配**：核心 `build_agent_with_provider(cfg, provider, initial, client, extra_tools, api_key)`
    与 `build_agent_with` 同走 `assemble`（tools/MCP/ctx 装配），只把 `provider::build` 换成传入的
    `Box<dyn Provider>`。`api_key` 仍要传（`task` 子 agent 自建内置 provider 需要）；纯 custom-provider
    无 `task` 时传 `""`。绑定层 provider 给定时，`api_key` 缺省用 `""` 而非读 env（custom provider 不要 key）。
  - **Python**：`PyProvider{chat_fn: Py<PyAny>}` impl `Provider`，`chat()` 在 worker 线程 `Python::with_gil`
    调 host 函数（与工具同款 GIL round-trip），返回值经 `py_to_simple`（str 速记 / dict）→ `from_simple`。
    host 抛异常 → `chat()` 返 `Err` → **诚实地 fail 该轮**（不静默空回复）。
  - **Node**：`NodeProvider{tsfn}` impl `Provider`，与工具同款 TSFN round-trip——agent 线程把 `{id,request}`
    经 TSFN 派给 JS、阻塞 `mpsc` 等结果；`wrapper.js` 跑完 host 函数后调 `resolveProvider(id, result)`
    （`PENDING_PROVIDERS: i64→Sender` 注册表，120s 兜底）。host 抛异常时 wrapper 解析成 `{__error: msg}`
    标记，`NodeProvider::chat` 见 `__error` 即返 `Err`（匹配 Python 的诚实 fail）。`resolveProvider` /
    `PENDING_PROVIDERS` / `build_provider` 三者是一套，改其一保住另两个（与工具那套对称）。
  - **`--test-force-exit`**：`bindings/node` 的 `npm test` 带 `--test-force-exit`。napi 的 TSFN 在 Agent
    drop 时释放 libuv async handle 有时机问题（**工具/ provider 都有，预先存在**）——独立脚本 run 完不会
    自然退出；`node --test` 的 pending-promise 检查会因此误判文件 hang。force-exit 是 napi 绑定测试的
    标准做法，不掩盖功能 bug（测试仍验证正确性）。
- **`panic = "abort"`（`[profile.release]`）**：release 下 panic **不触发 `Drop`**。TUI 的终端还原
  (`TerminalGuard`)、MCP 子进程清理（`StdioTransport::drop`）都依赖 Drop——调试这些行为用
  **debug 构建**。`std::process::exit` 也跳过 Drop，headless 模式退出前显式 `drop(agent)`。
  **MCP stdio 有 shutdown-aware teardown（cancel 与 shutdown 是两个独立标志）**：
  `Agent::set_shutdown` 把一个**独立于 cancel** 的 shutdown Arc 经 `Tool::set_shutdown`→
  `McpTool`→`McpClient`→`StdioTransport::set_shutdown` 传下去，后者 spawn 一个 `mcp-reaper` 线程
  （`spawn_reaper`），**shutdown** 一翻就 `libc::kill(-pgid, SIGKILL)` 杀整个进程组——卡死的 MCP 调用
  （worker 阻塞在 `request()` 等 120s `CALL_TIMEOUT`）由此立即解锁（child 死 → stdout EOF → reader 退 →
  `pending_fail_all`），worker 才能 drop agent、走正常 Drop 清理。否则 TUI 退出 3s join 超时 detach →
  agent 没 drop → MCP server 变孤儿。reaper 用 `alive` Arc + Drop 里 `join` 保证不在 child 被 reap 后误杀
  被 OS 复用的 pgid。
  **为什么是 shutdown 而不是 cancel**：`cancel` 是**可恢复的轮中断**（TUI Esc/Ctrl-C，下一轮复用同一个
  MCP server），杀 child 是不可逆的（无重启）——所以 reaper 只能监听 shutdown。shutdown 由前端**仅在会话
  结束**时置位：TUI 退出路径（`tui::run` 末尾）同时置 cancel + shutdown；headless Ctrl-C 总在进程退出前，
  故 cancel 与 shutdown 重合（`main.rs` 用同一个 Arc 调 `set_cancel` + `set_shutdown`）。**轮取消（TUI 忙时
  Esc）只置 cancel、不置 shutdown**，MCP server 不被杀。`Agent::set_cancel` **不再**向 tools 传播（删掉了
  那个 for 循环）；改这条链路别把 reaper 接回 cancel，也别给 `set_cancel` 加回 tools 传播。子 agent 共享父的
  MCP client（Arc-clone），父的 `set_shutdown` 已在共享 client 上 arm 了 reaper，子 agent 不必再调。
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
- **子 agent 用 `run_as_subagent`，入口不复位 cancel 标志**：子 agent 继承父的 cancel Arc，但不像
  `agent::run()` 那样在入口 `store(false)`——否则并行批次里后启动的兄弟会清掉刚按下的 Ctrl-C
  （与上一条「批量 Ctrl-C 不传 flag」同一类竞争，这里 flag 确实共享，靠跳过复位化解）。spawn 时
  batch gate 保证 flag 为 false，所以运行中见到 `true` 一定是真取消。别给 `run_as_subagent` 加复位。
- **子 agent 共享父的 MCP 工具（Arc-clone），不重启 server**：`SubAgent` 持父的 `mcp_tools`，建子 agent
  时 clone 而非重跑 `tools::mcp::build`。旧路径每次 `task` 都 `McpClient::start` 全部 server——并行 task
  批次会 spawn N×(#servers) 个子进程。改 `SubAgent::new` 签名别漏传 `mcp_tools`。
- **OpenAI 400 回退**（`openai::body_fix_for_400`）：错误文本提到 `max_tokens` 时改名为
  `max_completion_tokens` 重试一次（reasoning 模型）；提到 `stream_options` 时丢弃该字段重试一次
  （vLLM 等旧网关，之后 usage 走 `None` 降级，`parse_loop` 已容忍）；提到 `reasoning_effort` 时丢弃该字段
  重试一次（老网关不认）。三条 fix 各管一个独立字段，可连续修三次满足同时拒全部三个字段的网关。未知 400 直接 bail。
  改请求体字段的人要知道这条重试路径，别在 fix 后又引入新 400。
- **`--session none` 是关闭持久化的字面量哨兵**（不是文件名）——真要叫 `none` 的文件没法用，
  但它语义清晰、够用。改 `session_target` 判定前先想清楚。
- **MCP 工具名只消毒展示名，wire 名不动**（`tools::mcp::sanitize_name`）：对外名 `mcp_<server>_<tool>`
  非 `[A-Za-z0-9_]` → `_` 并折叠，但 `wire_name` 保留服务器原始工具名，`tools/call` 用它分发。
  改消毒逻辑别误动 `wire_name`，否则分发会找不到工具。`dedup_check` 在消毒后撞名时启动报错。
- **MCP stdio `env` 注入**（`McpServerConfig.env` + `StdioTransport::spawn` 的 `.envs(&cfg.env)`）：
  stdio 子进程的额外环境变量，叠加在宿主环境之上（同名以配置为准）。GUI 进程无 shell 环境靠它传凭据
  （`GITHUB_PERSONAL_ACCESS_TOKEN` / 连接串 / `XXX_API_KEY`）。`#[serde(default)]` 向后兼容；`validate()` 不拦
  （HTTP 传输忽略 env，不报错——env 只对 stdio 生效）。两条 Command 链（unix `process_group(0)` + `#[cfg(not(unix))]`）
  都要加 `.envs`，别只改一条。
- **思考内容展示策略**：`ThinkingDelta` 在 `harness` 的 `--json` NDJSON 发 `{"type":"thinking","delta":...}`；
  headless 终端打印写 **stderr**（不进 stdout 的 `summary`，可 `2>/dev/null` 屏蔽）；TUI 用 or 模式
  `TextDelta(t) | ThinkingDelta(t)` 折进同一渲染分支（TUI 冻结，只保证可见，不做可折叠区块——那是 TinyWorker 的事）。
  子 agent 的思考经 `AgentEvent::SubAgent` 整体包装转发（`subagent.rs` 不逐变体 match），harness 的 SubAgent 内嵌
  match 单独把它写 stderr 带 `[name]` 前缀。
- **改 `agent.rs` 核心路径必须配套 loop 测试**。测试外置在 `agent_tests.rs`（`agent.rs` 末尾
  `#[cfg(test)] #[path = "agent_tests.rs"] mod tests;` 引入，靠 `use super::*` 仍可达 agent 私有项；
  压缩纯函数的测试靠 `use crate::compress::*`）。用 `provider/mock.rs` 的脚本化 Provider 覆盖不变量：
  cancel 复位、tool_use/tool_result N:N 配对（含中途取消时合成的 `[cancelled by user]` 进 history
  但不进 `tool_calls`）、压缩、usage 累计。动了 loop 逻辑先补/查这些测试，别只靠 mock server 跑一遍就提交。

## 离线 / 视觉验证

- **mock server**：`examples/mock_openai.py`、`examples/mock_anthropic.py` 是零依赖 SSE mock，
  用于无 API key 跑通整个 loop（tool_use 配对、usage 解析、取消）。改动 provider/agent 后用它回归。
- **进程内回归**：`agent.rs` 的 loop 不变量用 `provider/mock.rs`（脚本化 Provider，不连网）单测覆盖
  （测试体在 `agent_tests.rs`），比 mock server 更快、断言更准。新增/改动 loop 行为优先在这里加用例。
- **TUI 视觉验证**：用 ratatui `TestBackend` 抓 cell grid 做断言（见 `tui::tests`）；端到端交互用
  pty 脚本驱动（项目里惯用 `/tmp/ta-verify/` 放临时 mock + pty 脚本，跑完即删）。**不要只靠
  "看起来对"**——改了渲染/光标/markdown 要真抓屏或单测断言验证。
