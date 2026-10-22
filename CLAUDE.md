# TinyAgent — 项目备忘

单二进制、同步（无 tokio）的极简 AI agent。一个 JSON 配置，两种模式：headless（harness / 评测）
与 TUI 对话。详细用法见 `README.md`。

**本文件只保留高频纪律与每主题一句话索引**；完整的设计决策与踩坑详录已归档到
`docs/claude/architecture.md`（架构要点）与 `docs/claude/pitfalls.md`（容易踩的坑）——
改动某个主题前先去对应详录读全该条目；修改条目时详情写详录、同步更新这里的索引行。

## 常用命令

```sh
cargo build --features tui                 # 默认只编 headless；TUI 要带 feature
cargo test --features tui,mock             # 单测（含 TUI 输入/渲染逻辑 + lib_api 集成测试）
cargo clippy --features tui --all-targets  # lint TUI 版
cargo clippy --all-targets                # lint headless 版
cargo clippy --features code --all-targets # lint code 版（run_code.rs 在 feature 后）
cargo clippy --features tui,code --all-targets # lint 交叉组合（CI 第四配置：TUI 的 code 渲染 / set_code_* 交互面）
cargo test --features code,mock            # run_code 沙箱套件（改 run_code.rs 必跑）
cargo fmt --all                            # 格式化（CI 有 --check 门禁，改完代码必跑）
```

每次改完**四个 clippy 配置都要跑**（headless / tui / code / tui,code 交叉）：`#[cfg(feature)]` 后的代码，
不带该 feature 的构建与 clippy 都看不到——单跑一两个会漏。TUI 代码在 `tui` 后；
`run_code.rs` 与 `agent.rs` 的 `set_code_*` 在 `code` 后；tui 与 code 的**交叉面**
（TUI 的 CodeLog 渲染、`/code` 命令）只在第四配置可见。CI 里四个 clippy 都带 `-D warnings`，
本地跑出任何黄字即等于红灯。**`cargo fmt --all` 同理**（CI 的 fmt job 对全 workspace
`--check`，含双绑定 crate；存量 305 处漂移已在一个纯格式 commit 清零，该 commit 记在
`.git-blame-ignore-revs`，`git blame` 配 ignoreRevsFile 后可跳过）。CI fmt job 还带两个
文档同步门禁：`scripts/sync_run_code_contract.py --check`（含 README code 配置表与
`CodeConfig::default` 的交叉核对）和 `scripts/gen_structure.py --check`（README 代码结构树
行数从源文件实时生成）——动了对应内容记得 `--write` 再提交。
**测试命令必带 `--features mock` 才真正跑到 `tests/lib_api.rs`**：该文件整体门在
`#![cfg(feature = "mock")]` 后（`provider::mock` 是 `cfg(any(test, feature = "mock"))`，
而集成测试编译时 lib 不带 `cfg(test)`），不带 mock 的 `cargo test` 把它编成 **0 个测试
照样全绿**——绿灯不等于 lib_api 集成测试跑过。CI 另有两道本地复现不了的 job：musl
静态构建冒烟（每次 push 必过，"嵌入式低配 Linux"是核心交付定位）与 cargo-audit
（RustSec 扫依赖树）——升级/新增依赖或动平台相关代码时留意这两道门。

### 语言绑定（workspace 成员 `bindings/python`、`bindings/node`）

两者 path-依赖内核 crate `crates/core`（`features = ["mock"]`），都复用 `CallbackTool` 注入自定义工具、不改核心。**Hooks 拦截层已双绑定暴露**（py `set_hooks`/`hooks=`、node `setHooks`/`hooks=`）：鸭子类型三方法任意子集，`before_tool` 决策 fail-closed（抛异常/非法返回=拒绝），观察类钩子异常吞掉——node 侧观察类 TSFN 是 Fatal 策略、wrapper 必须吞（与 wrapOnEvent 同纪律），决策走 `resolveHooksDecision` round-trip。两者各带一个
默认关闭的 `code` **转发 feature**（`code = ["ta_core/code"]`）：编入后 `enable_code`/`disable_code`/
`set_code_config`（Node 侧 camelCase）真实生效；未编入时三方法返回带重建提示的明确错误（设计 §8.6——
避免给所有 wheel 强加 ~10MB wasmtime）。绑定测试 build-自适应（探测 wheel 是否带 feature，另一侧 skip）。
**Node 侧增删/改名 native `Agent` 方法必须同步两件套**：`wrapper.js`（带 onEvent 的方法要套
`wrapOnEvent`——native 事件 TSFN 是 Fatal 策略，裸透传用户回调 = 抛错即进程 abort）与 `wrapper.d.ts`
类型声明；`tests/test_wrapper_sync.test.js` 以 native 原型为真值源钉住漂移。
`tests/test_packaging.test.js` 是打包验收门（主包白名单无 `*.node` + 本地伪造平台子包
的安装 → 加载 + mock 调用全链路）——守跨平台拆包分发契约：主包 `files` 不含 `*.node`，
平台子包 `tiny-agent-<platform>`（optionalDependencies 同版本）由 release workflow
五平台矩阵构建，`NPM_TOKEN` secret 缺省时子包 tarball 附 GitHub Release；子包 name
必须与 napi 生成 index.js 的 require 候选一致（测试钉住）。
改了核心 provider/agent 后，两个绑定的测试都要跟着跑一遍回归；动了 run_code/装配则另跑
`maturin develop --features code` + `npm run build:debug -- --features code` 的双构建。

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
- **发布（tag）**：版本号同步 6 处（根 `Cargo.toml`、`bindings/{python,node}/Cargo.toml`、
  `pyproject.toml`、`package.json`、`bindings/node/package-lock.json` 的 optionalDependencies
  版本——`npm install` 会自动刷新，`npm ci` 则因不同步直接报错）+ `CHANGELOG.md` 补段，然后 `git tag vX.Y.Z` 推双远端
  （gitee origin + github）。tag 推 GitHub 触发 `release.yml`（musl 产物，tarball 附
  README + LICENSE）。流程详见 README 发布节。许可证是 **MIT**（根 `LICENSE`，5 处元数据
  已同步）——新增子模块/产物别忘带许可声明。

## 高频纪律（几乎每次改动都适用）

- **三个 clippy 配置 + `cargo fmt --all` 全跑**（见上「常用命令」），黄字即红灯。
- **测试必带 `--features mock`**，否则 `tests/lib_api.rs` 编成 0 个测试照样全绿。
- **改 `agent.rs` 核心路径必须配套 loop 测试**（`agent_tests.rs` + `provider/mock.rs` 脚本化 Provider，
  不连网）——别只靠 mock server 跑一遍就提交。
- **数值写端一律 `saturating_add`**（usage / RunCost / merge_usage / batch 汇总所有 `+=`）。
- **cancel 契约：`run()` 永不清标志。`clear_cancel()` 只在「受理新任务」时刻调用——库内调用点仅
  TUI 提交输入与 `compact()` 入口（绑定宿主的发消息路径同理）**；重试梯（harness/batch）attempt 间
  观察到 cancel 是 **EXIT_CANCELLED 终态**（不重试、也不清——清了即复活用户已否决的 run）。
  别给任何 run 入口加回清残留。
- **新增"消息进历史"入口必须补 id 分配点**（漏了宿主锚点拿到 `None`）。
- **`ToolOutput`/`ToolCallRecord`/`ToolCtx` 加字段要同步全部字面量构造点**（含两个绑定）。
- **错误路径也是账单面**：绑定侧失败要透出结构化成本——run 的 `RunError` 与 compact 的
  `CompressError::burned`（2026-09 起两绑定都以 JSON 错误信封透出，py `json.loads(e.args[0])` /
  node `JSON.parse(err.message)`）；`Hooks::after_tool` 带 `ctx: &ToolCtx`（破坏性改动，趁绑定
  未透出 hooks 落的）——新增钩子点/错误出口照同款「成本与归因不落地」纪律检查。
- **`Config::apply` 的 `PartialConfig`/`PartialLimits` 解构是刻意穷尽的（无 `..`）**——新字段不分类
  即编译错误，禁止为省事加回 `..`。
- **LLM 可见文案即规格**：run_code 预检拒答/墓碑文案在 `docs/finetune_support_design.md` 附录 A
  （微调数据有依赖），改一个字先走该文档修订记录；run_code 工具描述/skill 正文同理三处同源同步。

## 架构要点索引（详录：`docs/claude/architecture.md`）

- **双 crate（内核 + CLI）**：内核 `crates/core`（package `tiny-agent-core`，lib 名 `tiny_agent`）持 agent loop/工具/provider/装配 + `fsutil` 原子写；根 package `tiny-agent` 是进程壳（main/signal + 本地模块 harness/batch/tui）。改内核内部不动 CLI；CLI 侧 `crate::agent::…` 经 main.rs 的 `pub(crate) use tiny_agent::{…}` 解析。CLI 的 `code`/`mock` 是转发 feature，根目录 `cargo build --features …` 语义不变。
- **同步模型**：`reqwest::blocking`，无 tokio；每个流式请求必带 per-request timeout 兜底防 reader 线程泄漏。SSE 内存三层限额（单行/单事件聚合/单响应累计）+ 有界背压通道，新增流式读取照抄。
- **思考/工具参数增量透出**：只发事件、永不进 `text`/`blocks`；新增流式透出项照同款纪律抄。
- **`reasoning_effort` 按字符串原样透传**，不做枚举校验；anthropic 走独立顶层 `output_config`，别用裸顶层字段。
- **PDF 文档块**：新 Block 变体照四层接入点清单（anthropic / openai 显式报错 / compress 三处 / tui）；token 预估按页×3000。
- **压缩**：`find_safe_cut` 空尾是合法切点；`summary_call` 空正文（思考烧穿）预算加倍重试一次，effort 恒 `None`。
- **`salvage_on_max_tokens`**：max_tokens 截断且无 tool_use 时一次性 wrap-up nudge；带 tool_use 不 salvage。
- **打转护栏**：键 =（工具名， canon 生效参数， 结果指纹），同参不同果即进展、不计数；canon 数值统一经 f64 归一（1 与 1.0 同键）；第 3 次 nudge 进历史块、第 6 次 `stuck_loop` 收尾；无闩锁（轮尾直读当前连续计数），轮尾 cancel 优先于 stuck_loop。
- **token 预算闸门**：run 级共享计数器父子同读；turn 边界比较的 cap 以 `BudgetGate::cap` 为单源（自身 limits 快照只能收紧）；工具账单按 `ToolOutput::usage_in_shared_budget` 分流——子 agent（已自计）只折 totals，其余工具的 usage 由 fold 代入计数器；并行 worker panic 时已计费 usage 由线程本地 `usage_probe` 回收进合成错误输出。
- **宿主热更新面**：`set_model` 等不传播给已构造的子 agent 快照（既定决策）；`compact()` 成本绝不进 `self.usage`；run_code 注册表首次执行即冻结 `code.llm`，之后热更改到它发 diag 警告（需重建 agent），普通 limits/路径热更不打扰。
- **上下文构成分解**：`context_breakdown()` 是拉取式估算查询（system 三段/工具四类 `Tool::origin()`/消息分型），与 Usage 实测口径刻意不混流；等和不变量靠单趟 running-floor；锚点 scale 校准、失效即回 None。
- **MCP 热增删**：撞名 = Err + 回滚；热增路径不打印；子 agent 与 run_code registry 都不感知（快照语义）；HTTP/SSE 流读取带 30s 轮次上限、teardown 有界 join（≤2s）——热删残留线程/连接至多一轮自回收（改读循环先看 `shutdown_stream_reader` 契约）。
- **MCP SSE 同源闸门**：endpoint 解析与 POST 发送共用 `reqwest::Url`（真解析器）——别写字符串版 origin 切分，反斜杠混淆可绕过。
- **子 agent（task）**：按 `Tool::concurrency()`/`available_to_subagent()` 能力标记路由，无名字黑名单；共享父 MCP、预算与 cancel；轮次天花板 `limits.subagent_max_turns`（默认 25，`0`=不另设，恒 `min` 父预算——宿主放宽父 `max_turns` 时须同步调大）。
- **Hooks**：Deny 落 `Slot::Ran(error)` 不占 Cancelled，Deny 文案过 `tools::truncate`（ctx 预算）；`after_tool` 带 `ctx: &ToolCtx`（与 before 对称，审计归因 `ctx.subagent`）；Modify 只换执行输入、审计留原始输入；`--resume` 不带走 hooks。审批等待期落下的 cancel 在 **gate→执行检查点**（`ctx_cancelled`，execute_tool_call / 并行 spawn 前 / run_code 内层共用）拒绝执行；并行批另有**每波开波前 + worker 进工具体前**两道复查，已 announce 未运行的调用装配段补发取消 ToolResult（事件配平）；已过 gate 但因 pre-spawn/波间检查点未启动的调用统一转 `Slot::Ran` 占位（进 records + `after_tool(executed=false)`，过的 gate 不留未配对 before——gate 循环被打断、从未过闸的才保持 Cancelled）；并行批的 `sync_channel` 必须在 scope 闭包**内**创建（回调 panic 时 `rx` 随 unwind 销毁才不死锁）。
- **todo + meta**：plan 从 records 派生不经压缩；`meta` 审计侧信道不进模型可见 content。
- **Skill**：三级加载；name 必须等于父目录名；`read_skill`/`list_skill` 不在白名单、由 assemble 配套注入；子 agent 不继承。
- **run_code**：两层开关默认全关；wasmtime 中断四点纪律（入口门/epoch 回调/host_call 入口/收尾裁决）；`max_wasm_stack` 384 KiB；测试必须先 warm 模块。
- **run_code 触发文案**：规则明确 AND 剩余规模；`agentic-code.md` 为独立评测变体，原提示词不变；契约同步用 `python3 scripts/sync_run_code_contract.py --check`，交接见 `docs/run_code_routing_report.md`。
- **run_code 微调支持**：`skill_injection` 三态（声明序=披露序，batch 收紧=min）装配期消费、热更不重注；`precheck` 调用期经 snapshot；检出集合只有 `{run_code, task, todo}`；拒答/墓碑文案即规格在 `docs/finetune_support_design.md` 附录 A，改文案先走其修订记录。
- **guest I/O**：prelude 清单封闭（改 Prelude 必须同步描述 + 设计附录 A）；guest 输出预算不继承 `ctx.max_output_chars`（用 2M）。
- **read_file 分页**：offset 跳行不花字节预算；json content 逐行保留原始行终止符；raw 默认窗口 3 字节对齐。
- **env 注入 system prompt**：唯一收敛点在 `Agent::new`（子 agent 也过）；改 assemble 注入会漏子 agent。
- **retry 诚实成本**：失败轮的 usage/tool_calls 经 `RunCost` 落进 result.json，不清零；压缩审计
  `compressions` / 逐轮耗时 `turn_timings` / 工具耗时 `duration_ms` 同走这条账单面（失败轮压缩审计照落；turn_timings 失败路径不落行）；batch 不经 run_one_attempt 的终态一律走 `write_terminal_result` 单出口落盘（`early` 早退也继承 `acc`/`attempt_no`——重试轮 setup 失败不清零前轮真实成本）。
- **run_code 路径拦截**：符号链接走查按**原始拼写**逐组件进行（先折叠 `..` 会把 `link` 抹掉、`link/../x` 逃逸）；绝对拼写**从根锚定**走查（字面前缀非 workdir 的折叠拼写不再跳过）；不存在目标经**最深已存在祖先 canonicalize** 判定（不再纯词法回退）。cancel/deadline 入口拒绝都同时 latch `interrupt`+`halted`（可捕获异常被 guest 捕获重试时，停机检查拒纳）。
- **headless Ctrl-C**：首次翻 cancel 优雅收尾（exit 130），二次硬退；批量模式 worker gate 同款。
- **消息稳定 id**：ingest 点分配（非构造时）；`next_msg_id` 不随 `clear_messages` 重置。
- **TUI**：inline viewport + scrollback 提交模型；新增"在飞行"必须同步 holdback/inflight/LiveRow 三件套；新暗色元素用 `dim_fg()`；源码按关注点拆在 `src/tui/`（app/transcript/markdown/slash/render/input/theme + tests，`tui.rs` 只是模块根）。
- **文件工具 workdir 沙盒（`tools::confined_to`）**：尾段**逐组件解析走查、着陆即复检**——`缺失目录/../<外向 symlink>` 类拼写一律 Err（词法折叠会 Ok 逃逸，性质 fuzz 抓过），Ok 永不背 `..`/`.`、再入不动点，内向 symlink 返回真身；文件工具与 skill 沙盒共用，改它两边的语义测试都要过。

## 容易踩的坑索引（详录：`docs/claude/pitfalls.md`）

- **绑定回调是跨线程 round-trip**：Python 走 GIL + `with_agent` 锁（重入报 "agent is running"）；Node 走 TSFN（run 期间 JS 主线程必须空闲，同步方法 busy 报错）。
- **自定义 provider 回调**：host 抛异常 = 诚实 fail 该轮，不静默空回复。
- **shell 管道孤儿**：后台进程继承管道会卡 EOF——留后台进程必须重定向其输出；回收标记只追加在正常退出路径。
- **`panic = "abort"` 下 Drop 不跑**：调 TUI 还原 / MCP 清理用 debug 构建；`process::exit` 前显式 `drop(agent)`。
- **MCP shutdown ≠ cancel**：reaper 只听 shutdown（会话结束）；轮取消只置 cancel——别把 reaper 接回 cancel、别给 `set_cancel` 加回 tools 传播。
- **`serde_json::json!` 宏按所有权取值**：`&String`/`&Value` 要 clone，`Option<&str>` 显式转。
- **Anthropic 不允许相邻 user**：新 user 入口走 `append_user_turn`；tool_use/tool_result 必须 N:N 配对。
- **batch 覆盖白名单**：安全字段覆盖即 `bad_task`；limits 逐字段由 `Clamp` 三变体声明，`shell_timeout_secs` 是唯一 Replace。
- **batch `--retries` 只重试 transient**（`stop_reason == "error"`；分类在 `RunError::is_transient`，单跑梯共用——4xx 非 429 为确定性 `request_error` 不重试，run 截止后失败的请求按 `timeout`/124 归属）；**重试=整任务重放**，前轮副作用不回滚；`stop_reason` 词汇含 `panic`（catch_unwind 兜底）。
- **batch workdir 隔离**：沙箱目录 `create_dir`（非 `_all`）+ 计数重试拒绝符号链接预置（与 atomic_write 的 O_EXCL 同款纪律，git 模式先自建空目录再 worktree add）；`copy` 符号链接自包含策略；`git` 要 repo 根（macOS /tmp symlink 需 canonicalize 比较）；单个坏 JSON 任务文件是 per-task `bad_task` 不中止整批；merge_reports 保同名重复条目。
- **OpenAI 400 回退**：三条 fix 各管一个字段，record-on-success（只记最终 2xx 的修复），learned fixes 不落盘；整次 `chat()` 共用一个绝对截止时间（重发只花剩余额度），重发前查 cancel。
- **流中途错误形状**：返回 Ok + 尾附 `[stream error]` 标记 + 已上报 usage——别 bail（会丢 usage 破坏失败计费）；run 截止已过时按 `timeout`/124 终态归属（不进重试梯）。
- **`atomic_write` 用 `create_new(true)`**：别改回可预测单名（符号链接预置攻击面）；NDJSON 走 `print_ndjson` 别用 `println!`。
- **`--session none` 是字面量哨兵**，与 `--resume` 互斥。
- **MCP 工具名**：只消毒展示名，wire 名不动；`validate_tool_names` 在 assemble 末尾、之后别再 push 工具。
- **MCP stdio `env` 注入**：两条 Command 链（unix / 非 unix）都要加 `.envs`。
- **`ToolConcurrency::Parallel` 是承诺**：可重入 + 轮询 cancel，核心不校验，谎报只在生产数据竞争。
- **`#[non_exhaustive]` 只给消费侧枚举**；`Role`/`CompressTrigger`/生产侧枚举不加。
- **思考展示**：headless 写 stderr 不进 stdout；TUI 独立 ✻ 区块不走 markdown。
- **context_breakdown 等和靠单趟 running-floor**（各段独立 /4 会差 1-2 丢不变量）；无 thinking 桶（思考永不进 history）；锚点失效即校准回 None。

## 离线 / 视觉验证

- **场景 e2e**：`python3 scripts/e2e_scenarios.py`——内置 SSE 场景 mock（线程化、零密钥），走真实
  CLI/HTTP 栈守四个 loop 级行为：打转护栏（stuck_loop 硬停 / 同参不同果不误停 / nudge 只进历史不进
  事件流）、单轮 10 路并行 task fan-out（8/波 + N:N + 请求数精确核算）、SIGINT exit 130。CI 有
  同名 job（ubuntu）。改了 loop/取消/打转行为后必跑。
- **mock server**：`examples/mock_openai.py`、`examples/mock_anthropic.py` 是零依赖 SSE mock，
  用于无 API key 跑通整个 loop（tool_use 配对、usage 解析、取消）。改动 provider/agent 后用它回归。
- **进程内回归**：`agent.rs` 的 loop 不变量用 `provider/mock.rs`（脚本化 Provider，不连网）单测覆盖
  （测试体在 `agent_tests.rs`），比 mock server 更快、断言更准。新增/改动 loop 行为优先在这里加用例。
- **TUI 视觉验证**：用 ratatui `TestBackend` 抓 cell grid 做断言（见 `tui::tests`）；端到端交互用
  pty 脚本驱动（项目里惯用 `/tmp/ta-verify/` 放临时 mock + pty 脚本，跑完即删）。**不要只靠
  "看起来对"**——改了渲染/光标/markdown 要真抓屏或单测断言验证。pty 驱动两个坑：裸 pty 必须
  应答 crossterm 的 `\x1b[6n` 光标查询（回 `\x1b[<行>;<列>R`，否则启动即 "cursor position
  could not be read" 退出）；带补全弹窗的斜杠命令要**双回车**（第一个 Enter 被弹窗吃掉当选择）。
