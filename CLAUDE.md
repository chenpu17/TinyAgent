# TinyAgent — 项目备忘

单二进制、同步（无 tokio）的极简 AI agent。一个 JSON 配置，两种模式：headless（harness / 评测）
与 TUI 对话。详细用法见 `README.md`（英文）与 `README.zh-CN.md`（中文）——两份内容平行，
改了共享事实（配置默认值、键位、事件表等）要两边同步；代码结构树由 gen_structure 双写，勿手改。
仓库主场 **github.com/chenpu17/TinyAgent**（origin），gitee 为镜像（remote 名 `gitee`）。

**本文件只保留高频纪律与每主题一句话索引**；完整的设计决策与踩坑详录已归档到
`docs/claude/architecture.md`（架构要点）与 `docs/claude/pitfalls.md`（容易踩的坑）——
改动某个主题前先去对应详录读全该条目；修改条目时详情写详录、同步更新这里的索引行。

## 常用命令

**本地一把过：`./scripts/check.sh`**（与 CI 同款门禁；`./scripts/check.sh fmt docs` 可只跑一组，
命令改了要同步回 `.github/workflows/ci.yml` 与它两边）。

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
（TUI 的 CodeLog 渲染、`/code` 命令）只在第四配置可见。`#[cfg(windows)]` 后的代码是同款盲区——本地
任何配置都不编译它，只在 CI 的 Windows 格见分晓；动了它或给内核结构体加字段后，按 pitfalls 的
zig 包装配方做一次 Windows 目标交叉 clippy。CI 里四个 clippy 都带 `-D warnings`，
本地跑出任何黄字即等于红灯。**`cargo fmt --all` 同理**（CI 的 fmt job 对全 workspace
`--check`，含双绑定 crate；存量 305 处漂移已在一个纯格式 commit 清零，该 commit 记在
`.git-blame-ignore-revs`，`git blame` 配 ignoreRevsFile 后可跳过）。CI fmt job 还带两个
文档同步门禁：`scripts/sync_run_code_contract.py --check`（含 README code 配置表与
`CodeConfig::default` 的交叉核对）和 `scripts/gen_structure.py --check`（README.md 与 README.zh-CN.md 两份代码结构树行数从源文件
实时生成，ENTRIES 描述单一事实源、双写两份）——动了对应内容记得 `--write` 再提交。
**测试命令必带 `--features mock` 才真正跑到 `tests/lib_api.rs`**：该文件整体门在
`#![cfg(feature = "mock")]` 后（`provider::mock` 是 `cfg(any(test, feature = "mock"))`，
而集成测试编译时 lib 不带 `cfg(test)`），不带 mock 的 `cargo test` 把它编成 **0 个测试
照样全绿**——绿灯不等于 lib_api 集成测试跑过。CI 另有两道本地复现不了的 job：musl
静态构建冒烟（每次 push 必过，"嵌入式低配 Linux"是核心交付定位）与 cargo-audit
（RustSec 扫依赖树；本地可用同版 cargo-audit 复现，配方见 pitfalls「本地 clippy 绿 ≠ CI clippy 绿」条）
——升级/新增依赖或动平台相关代码时留意这两道门。

### 语言绑定（workspace 成员 `bindings/python`、`bindings/node`）

两者 path-依赖内核 crate `crates/core`（`features = ["mock"]`），都复用 `CallbackTool` 注入自定义工具、不改核心。**Hooks 拦截层已双绑定暴露**（py `set_hooks`/`hooks=`、node `setHooks`/`hooks=`）：鸭子类型三方法任意子集，`before_tool` 决策 fail-closed（抛异常/非法返回=拒绝），观察类钩子异常吞掉——node 侧观察类 TSFN 是 Fatal 策略、wrapper 必须吞（与 wrapOnEvent 同纪律），决策走 `resolveHooksDecision` round-trip。**建 TSFN 前必须把可调用成员验成 function**：napi 对非函数成员是进程级 abort（不可 catch），wrapper 对 tools/hooks 形状错误同步 TypeError，不静默丢弃（详见 pitfalls）。**热更新家族与 MCP 热增删也已双绑定暴露**（py `set_model` 等 / node camelCase `setModel` 等；busy 契约同其他同步方法，`task` 子 agent 不跟随；数值参数只收合法整数，负数/小数/越界当场报错，不做 napi 截断回绕）；Node 另有 `Agent.create` 异步构造——装配（含 MCP spawn+握手）移到 worker 线程，因 napi class registry 是线程本地，实例只能在 JS 线程的 resolver 里 materialize，wrapper 用 `this.prototype` 做 `Object.setPrototypeOf` 把 onEvent 保护接回（且保持同步校验：非 async 包装）。`push_user`/`pushUser` 收 `str`、带 `type` 标签的块、或 `messages()` 的外部标签 serde 块（`{"image": {…}}`，保证历史块可回推）；一个块只解释一种形状（多形状键明确报错），`tool_use`/`tool_result` 拒收（id 须与模型输出配对）。两者各带一个
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
- **发布（tag）**：版本号同步 6 处（根 `Cargo.toml`、`crates/core/Cargo.toml`、`bindings/{python,node}/Cargo.toml`、
  `pyproject.toml`、`package.json`——后者的 `optionalDependencies` 五行平台子包钉同一版本；
  lock 由 `npm install` 按 manifest 刷新、反向不会）+ `CHANGELOG.md` 补段，
  然后 `git tag vX.Y.Z` 推双远端（github origin + gitee 镜像）。tag 推 GitHub 触发 `release.yml`
  （musl 产物，tarball 附 README + LICENSE）。流程详见 README 发布节。许可证是 **MIT**
  （根 `LICENSE`，各包 manifest 的 repository 字段统一指向 github.com/chenpu17/TinyAgent）——
  新增子模块/产物别忘带许可声明。

## 高频纪律（几乎每次改动都适用）

- **四个 clippy 配置 + `cargo fmt --all` 全跑**（见上「常用命令」），黄字即红灯。
- **测试必带 `--features mock`**，否则 `tests/lib_api.rs` 编成 0 个测试照样全绿。
- **改 `agent.rs` 核心路径必须配套 loop 测试**（`agent_tests.rs` + `provider/mock.rs` 脚本化 Provider，
  不连网）——别只靠 mock server 跑一遍就提交。
- **数值写端一律 `saturating_add`**（usage / RunCost / merge_usage / batch 汇总所有 `+=`）。
- **cancel 契约：`run()` 永不清标志。`clear_cancel()` 只在「受理新任务」时刻调用——库内调用点仅
  TUI 提交输入与 `compact()` 入口（绑定宿主的发消息路径同理）**；重试梯（harness/batch）attempt 间
  观察到 cancel 是 **EXIT_CANCELLED 终态**（不重试、也不清——清了即复活用户已否决的 run）。
  别给任何 run 入口加回清残留。
- **新增"消息进历史"入口必须补 id 分配点**（漏了宿主锚点拿到 `None`）。
- **`ToolOutput`/`ToolCallRecord`/`ToolCtx` 加字段要同步全部字面量构造点**（含两个绑定，也含
  `#[cfg(windows)]` 后的字面量——后者本地编译不到，见 pitfalls 对应条）。
- **新增会拉起子进程的代码路径（内置工具 / MCP 传输 / 宿主自定义工具）必须照同款读 `ctx.hidden_env`、
  逐变量 `env_remove`**——库从不改进程环境，漏一个 spawn 点就是凭据卫生的旁路；配套探针测试
  （断言变量不可见）并跑 `scripts/e2e_mcp.py` 的 K 组。
- **错误路径也是账单面**：绑定侧失败要透出结构化成本——run 的 `RunError` 与 compact 的
  `CompressError::burned`（2026-09 起两绑定都以 JSON 错误信封透出，py `json.loads(e.args[0])` /
  node `JSON.parse(err.message)`）；`Hooks::after_tool` 带 `ctx: &ToolCtx`（破坏性改动，趁绑定
  未透出 hooks 落的）——新增钩子点/错误出口照同款「成本与归因不落地」纪律检查。
- **`Config::apply` 的 `PartialConfig`/`PartialLimits` 解构是刻意穷尽的（无 `..`）**——新字段不分类
  即编译错误，禁止为省事加回 `..`。
- **LLM 可见文案即规格**：run_code 预检拒答/墓碑文案在 `docs/finetune_support_design.md` 附录 A
  （微调数据有依赖），改一个字先走该文档修订记录；run_code 工具描述/skill 正文同理三处同源同步。
- **bisect 可编译性**：同批拆多个 commit 时，消费侧事件/枚举加字段与消费方 match 臂改动必须
  落**同一个 commit**（9bdabdf 给 `Compressed` 加三字段、cb81197 才给 TUI 臂补 `..`——落在这
  两个提交之间的 bisect 点 `--features tui` 编不过）；一个提交自身要能过四配置 clippy。

## 架构要点索引（详录：`docs/claude/architecture.md`）

- **双 crate（内核 + CLI）**：内核 `crates/core`（package `tiny-agent-core`，lib 名 `tiny_agent`）持 agent loop/工具/provider/装配 + `fsutil` 原子写；根 package `tiny-agent` 是进程壳（main/signal + 本地模块 harness/batch/tui）。改内核内部不动 CLI；CLI 侧 `crate::agent::…` 经 main.rs 的 `pub(crate) use tiny_agent::{…}` 解析。CLI 的 `code`/`mock` 是转发 feature，根目录 `cargo build --features …` 语义不变。
- **同步模型**：`reqwest::blocking`，无 tokio；每个流式请求必带 per-request timeout 兜底防 reader 线程泄漏。SSE 内存三层限额（单行/单事件聚合/单响应累计）+ 有界背压通道，新增流式读取照抄。
- **思考/工具参数增量透出**：只发事件、永不进 `text`/`blocks`；新增流式透出项照同款纪律抄。
- **`reasoning_effort` 按字符串原样透传**，不做枚举校验；anthropic 走独立顶层 `output_config`，别用裸顶层字段。
- **PDF 文档块**：新 Block 变体照四层接入点清单（anthropic / openai 显式报错 / compress 三处 / tui）；token 预估按页×3000。
- **压缩**：`find_safe_cut` 空尾是合法切点；`summary_call` 至多两次——空正文无条件、**截断且带推理**（reasoning 块或 `usage.reasoning_tokens > 0`）才以翻倍预算（≤8192）重试一次，取较长正文、重试失败折第一段、取消恒 `Err`；预算学习 `SummaryBudget { floor, ceiling }` 挂 agent、跨 run 保留、`set_model` 清；effort 恒 `None`。Preflight 折叠的保留窗只降不升（配置窗自身超 `gate_room()/2` 即缩到能装进一半的最宽尾部，Manual / Overflow 不缩）；摘要摘录宽度 `limits.summary_preview_chars`（默认 200，只能收紧）。
- **`salvage_on_max_tokens`**：max_tokens 截断且无 tool_use 时一次性 wrap-up nudge；带 tool_use 不 salvage。
- **打转护栏**：键 =（工具名， canon 生效参数， 结果指纹），同参不同果即进展、不计数；canon 数值统一经 f64 归一（1 与 1.0 同键）；第 3 次 nudge 进历史块、第 6 次 `stuck_loop` 收尾；无闩锁（轮尾直读当前连续计数），轮尾 cancel 优先于 stuck_loop。另有 `RepeatWindow`（最近 8 次调用）对**隔次**同键在第 4、6、8… 次只提示不停（与连续 nudge 互斥，硬停只认连续计数）。
- **token 预算闸门**：run 级共享计数器父子同读；turn 边界比较的 cap 以 `BudgetGate::cap` 为单源（自身 limits 快照只能收紧）；工具账单按 `ToolOutput::usage_in_shared_budget` 分流——子 agent（已自计）只折 totals，其余工具的 usage 由 fold 代入计数器；并行 worker panic 时已计费 usage 由线程本地 `usage_probe` 回收进合成错误输出。
- **宿主热更新面**：`set_model` 等不传播给已构造的子 agent 快照（既定决策）；`compact()` 成本绝不进 `self.usage`；run_code 注册表首次执行即冻结 `code.llm`，之后热更改到它发 diag 警告（需重建 agent），普通 limits/路径热更不打扰。
- **上下文构成分解**：`context_breakdown()` 是拉取式估算查询（system 三段/工具四类 `Tool::origin()`/消息分型），与 Usage 实测口径刻意不混流；等和不变量靠单趟 running-floor；锚点 scale 校准、失效即回 None。**工具表规模提示（2.3）**：`ToolsBreakdown::advisory()` 严格大于 10000 估算 token 或 30 个工具才提示（恰好等于不算越过）；token 数与 `/context` 同口径（校准缩放后的 `tokens`），工具数不缩放；顶层 agent 构造期经 diag 通道每进程按文本去重提示一次（子 agent / 热增删不提示），文案单一来源 `tool_defs_advisory_lines`；**只提示不处置**。
- **MCP 热增删**：撞名 = Err + 回滚；热增路径不打印；子 agent 与 run_code registry 都不感知（快照语义）；HTTP/SSE 流读取带 30s 轮次上限、teardown 有界 join（≤2s）——热删残留线程/连接至多一轮自回收（改读循环先看 `shutdown_stream_reader` 契约）；legacy SSE 轮转后跟随新连接的 endpoint 刷新 POST 目标、reader 死亡置 latch 快败（stdio 同款）；`refresh_tool_defs` 是工具表的唯一重算点——schema 占用与构造路径同一个 `tool_schema_units` 量纲（CJK 感知，别退回字节数）、注解旁表同步重建。
- **MCP SSE 同源闸门**：endpoint 解析与 POST 发送共用 `reqwest::Url`（真解析器）——别写字符串版 origin 切分，反斜杠混淆可绕过。
- **MCP 客户端契约（0.1/2.4）**：`tools/list` 翻页（64 页 / 256 工具，任一页失败即握手失败）；`call_timeout_secs` / `connect_timeout_secs`（界与超时文案里的键成对，`0` 与超 7 天即配置错误）；`allow_tools`/`deny_tools` 写 wire 名、在 `start_server` 过滤（allow 的未知名字硬错、deny 的放行）；`initialize` 提供 `2025-03-26`、后续请求带协商版本（未知修订只告警）；取消通知只发给已出门的请求（超时不发）；HTTP 空闲 ≥60 s 先 `ping`（静默 = 调用不发出）；Streamable HTTP 404 → 新会话 + 只重发一次（不重拉工具表）。legacy SSE 轮转时**先退役** POST 目标、间隙里的请求/通知等新 `endpoint` 再发（界 = `connect_timeout_secs`，看得见 cancel 与 reader 死亡；取消通知不等、无当前目标即丢帧）。standing stream 的 POST 前布防是尽力而为的竞态保险（只答 202 的 server 有丢回复窗口，已知未修）。
- **MCP 注解旁表（1.6）**：`ToolCtx::tool_annotations`（Arc，按**暴露名**索引）是 server 自述的唯一落点——刻意做成旁表而非 `Tool` trait 方法（内置工具 / `CallbackTool` 不必作答，第三方想声明就覆盖 `Tool::annotations`）；唯一构造点 = `Agent::try_new` + `refresh_tool_defs`，所以永远与实际分发的工具表一致；子 agent / run_code guest 持构造时快照；每次调用把快照落 `ToolCallRecord.annotations`。**是 server 的声称、不是内核保证**，没有任何强制。工具声明了 `outputSchema` 时结果的 `structuredContent` 压过文本镜像，否则只做「纯结构化 server」的兜底。
- **凭据卫生（2.5）**：`ToolCtx.hidden_env` 默认恰是 `api_key_env` 指向的那**一个**变量（`env_passthrough` 列了则为空）；移除发生在每个 spawn 点（`Command::env_remove`：shell 两条链、MCP stdio 两条链 + 热增），**库从不改进程环境**；MCP 条目 `env` 里显式写出的同名变量照常下发（用户有意交给某个 server）；`task` 子 agent 与 run_code guest 的 ctx 克隆原样继承；定位是防顺手不防对抗。启动提示每进程一次、且只在配置里真有 `shell` 或 stdio MCP 时发；`restrict_to_workdir` 未开的无人值守提示只在 headless + batch（TUI 有人在看）。
- **子 agent（task）**：按 `Tool::concurrency()`/`available_to_subagent()` 能力标记路由，无名字黑名单；共享父 MCP、预算与 cancel；轮次天花板 `limits.subagent_max_turns`（默认 25，`0`=不另设，恒 `min` 父预算——宿主放宽父 `max_turns` 时须同步调大）。
- **轮末仲裁（FR-1）**：`on_turn_stopping` 仅 top_level×`end_turn`×发 `Done` 前；`Continue` 复用旁注队列（轮首投递），帽 6 对齐 STUCK_STOP_AT，超限 `arbitration_capped`（exit 0，标签在 result.json）；默认 Stop **零漂移**——非默认决策才发 `turn_arbitration` 事件。
- **推理通道**：`reasoning_content`/`reasoning` 字段 → ThinkingDelta 是协议路径（恒开）；内联 `<think>` 剥离是内容约定（FR-2，`strip_think_tags` 默认开、跨 chunk 边界、正文 fail-closed 无残留）。两者都永不进正文；思考增量本身永不进历史，能进历史的只有显式回传的 `Message.reasoning`（下一行）。
- **推理回传（1.1）**：内置 provider **无条件**产出 `ChatResponse.reasoning`（`Reasoning{provider, blocks}`，旁字段而非 Block 变体——`content` 形状与所有「块形状」规则不变）；`preserved_reasoning` 默认 `true`（2026-11-26 起，D1；显式关时 `try_new` 剥掉 initial 历史里的、之后每个回复的在落盘前丢弃——历史逐字节同开关出现之前；指纹只在显式 `false` 时进）；开了寿命也只有**当前轮**（`push_user` 开新 user 轮 / `set_model` / 相邻同角色合并一律整段剥；工具结果并入尾 user 消息走合并分支**不剥**；压缩折叠与工具结果裁剪保留幸存消息自己的；会话归档永不携带）；子 agent 继承开关；provider 只回显自己家族的块，OpenRouter `delta.reasoning` 仅展示不落；OpenAI 家族回显被 400 拒即学到 `BodyFix::DropReasoningEcho`（`GatewayFixes` 第四位）。两个分支都上过真端点（2026-11-26；Anthropic 分支逐请求抓包核对、回传逐字节一致；修后复测里省不省 token 随端点变：OpenAI 协议 −31%、Anthropic 协议 +26%，默认值未改）；宽松网关不校验签名，200 证明不了回传正确——工具调用轮里「空正文 + 签名」的 thinking 块必须保留并原位回传，要抓包三方核对（见 pitfalls）。`Usage.reasoning_tokens` 是 `output_tokens` 的**子集拆分**——不进 `total()`、无闸门读它、序列化只在非 0 时出键（非推理模型的 result.json / report.json 逐字节不变；NDJSON `usage` 事件则是恒出现的增量键）。
- **请求整形（1.3）**：`extra_body` / `extra_headers` 由 provider 构造器承接（`with_extra_body` / `with_extra_headers`，`build_agent*` 按配置自动调）；加载期 `Config::validate_extra` 拒内核自有键（`EXTRA_BODY_RESERVED[_WITH_EFFORT]` / `EXTRA_HEADERS_RESERVED`），报错不回显值；`ExtraHeaders` 的 `Debug` 打码、`fingerprint` 只哈希 header 名；四个新键（含 `preserved_reasoning` / `env_passthrough`）都是 batch HARD。
- **模型窗口提示（1.5）**：`Limits::context_window_hint()` 纯函数（窗口未设才出一行），CLI 在装配前打印一次（headless/batch 走 stderr、TUI 走 diag 行）；思考中被 `max_tokens` 截断的提示走 diag 通道、每 run 一次——都是建议不是校验，两个默认值不改。
- **Hooks**：Deny 落 `Slot::Ran(error)` 不占 Cancelled，Deny 文案过 `tools::truncate`（ctx 预算）；`after_tool` 带 `ctx: &ToolCtx`（与 before 对称，审计归因 `ctx.subagent`）；Modify 只换执行输入、审计留原始输入；`--resume` 不带走 hooks。审批等待期落下的 cancel 在 **gate→执行检查点**（`ctx_cancelled`，execute_tool_call / 并行 spawn 前 / run_code 内层共用）拒绝执行；并行批另有**每波开波前 + worker 进工具体前**两道复查，已 announce 未运行的调用装配段补发取消 ToolResult（事件配平）；已过 gate 但因 pre-spawn/波间检查点未启动的调用统一转 `Slot::Ran` 占位（进 records + `after_tool(executed=false)`，过的 gate 不留未配对 before——gate 循环被打断、从未过闸的才保持 Cancelled）；并行批的 `sync_channel` 必须在 scope 闭包**内**创建（回调 panic 时 `rx` 随 unwind 销毁才不死锁）。`on_before_compact`（2.2）只在折叠**真的发生**之前同步调一次（摘要已成功之后；摘要失败 / 空摘要 / 仅 prune 都不触发），只读不能否决，是唯一 panic 会被兜住的钩子（裸 `catch_unwind`——发布构建 `panic = "abort"` 兜不住，实现仍不该 panic）；绑定层未转发。
- **todo + meta**：plan 从 records 派生不经压缩；`meta` 审计侧信道不进模型可见 content。
- **Skill**：三级加载；name 必须等于父目录名；`read_skill`/`list_skill` 不在白名单、由 assemble 配套注入；子 agent 不继承。frontmatter 的四个标量字段与 `metadata` 值支持 YAML 块标量（`|` / `>` + chomping / 缩进指示符，手工解析不引 YAML 依赖；按缩进判归属、值恒 trim），多行裸标量 / 块序列 / 嵌套映射仍刻意不支持。
- **run_code**：两层开关默认全关；wasmtime 中断四点纪律（入口门/epoch 回调/host_call 入口/收尾裁决）；`max_wasm_stack` 384 KiB；测试必须先 warm 模块。
- **run_code 触发文案**：规则明确 AND 剩余规模；`agentic-code.md` 为独立评测变体，原提示词不变；契约同步用 `python3 scripts/sync_run_code_contract.py --check`，交接见 `docs/run_code_routing_report.md`。
- **run_code 微调支持**：`skill_injection` 三态（声明序=披露序，batch 收紧=min）装配期消费、热更不重注；`precheck` 调用期经 snapshot；检出集合只有 `{run_code, task, todo}`；拒答/墓碑文案即规格在 `docs/finetune_support_design.md` 附录 A，改文案先走其修订记录。
- **guest I/O**：prelude 清单封闭（改 Prelude 必须同步描述 + 设计附录 A）；guest 输出预算不继承 `ctx.max_output_chars`（用 2M）。
- **read_file 分页**：offset 跳行不花字节预算；json content 逐行保留原始行终止符；raw 默认窗口 3 字节对齐。
- **env 注入 system prompt**：唯一收敛点在 `Agent::new`（子 agent 也过）；改 assemble 注入会漏子 agent。
- **retry 诚实成本**：失败轮的 usage/tool_calls 经 `RunCost` 落进 result.json，不清零；压缩审计
  `compressions` / 逐轮耗时 `turn_timings` / 工具耗时 `duration_ms` 同走这条账单面（失败轮压缩审计照落；turn_timings 失败路径不落行）；batch 不经 run_one_attempt 的终态一律走 `write_terminal_result` 单出口落盘（`early` 早退也继承 `acc`/`attempt_no`——重试轮 setup 失败不清零前轮真实成本）。**构建归因**：两份账单顶层都带 `agent_version`，`schema_version` 契约=只加键（新增键一律可选、破坏性才升版，常量 `RESULT_SCHEMA_VERSION` 单源）；**NDJSON 类型=内核穷尽契约**（`AgentEvent::ndjson_type()`，新增变体不补即编译失败；harness `_` 臂只标 `unmapped: true`，别发明 `unknown`）。
- **run_code 路径拦截**：符号链接走查按**原始拼写**逐组件进行（先折叠 `..` 会把 `link` 抹掉、`link/../x` 逃逸）；绝对拼写**从根锚定**走查（字面前缀非 workdir 的折叠拼写不再跳过）；不存在目标经**最深已存在祖先 canonicalize** 判定（不再纯词法回退）。cancel/deadline 入口拒绝都同时 latch `interrupt`+`halted`（可捕获异常被 guest 捕获重试时，停机检查拒纳）。
- **headless Ctrl-C**：首次翻 cancel 优雅收尾（exit 130），二次硬退；批量模式 worker gate 同款。
- **消息稳定 id**：ingest 点分配（非构造时）；`next_msg_id` 不随 `clear_messages` 重置。
- **TUI**：inline viewport + scrollback 提交模型；新增"在飞行"必须同步 holdback/inflight/LiveRow 三件套；Assistant 才增量切分，Thinking live 有界尾部、最终整块提交（>8 显示行折叠，全文 `/thinking` 重印）；队列唯一真值是 `VecDeque`、不得回潮 transcript ghost，失败暂停只能 `/resume` 释放；composer/折行/切点统一 grapheme 显示宽度，放弃 soft-cut 必须原子；正文与底部 chrome 共享左锚定 120 列工作区，角色/工具标识变更同步 `continuation_prefix`，空态/popup 共用 `truncate_line`；新暗色元素用 `dim_fg()`，键帽/选中行/行内代码共用 `keycap_bg()`；Unix tty 断开由独立监控线程收尾（crossterm EOF 内循环会满核），禁止回退成只靠主事件循环；源码按关注点拆在 `src/tui/`（app/transcript/markdown/slash/render/input/theme + tests，`tui.rs` 只是模块根）。
- **文件工具 workdir 沙盒（`tools::confined_to`）**：尾段**逐组件解析走查、着陆即复检**——`缺失目录/../<外向 symlink>` 类拼写一律 Err（词法折叠会 Ok 逃逸，性质 fuzz 抓过）；**present-but-unresolvable 一律 fail-closed**（悬空 symlink/不可穿过的前缀只放行 ENOENT 真缺失——悬空外向链曾词法放行、写穿真逃逸，审计抓过），Ok 永不背 `..`/`.`、再入不动点，内向 symlink 返回真身；文件工具与 skill 沙盒共用，改它两边的语义测试都要过。

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
- **MCP 改动的测试与验证**：改完跑全量 clippy 组（内核单跑会漏 CLI crate 的 lint）；装 diag sink 的测试先拿 `TEST_LOCK`；大文件测试块尾部 splice 用 `rfind` 锚文件尾；真实进程 mock 每场景新日志、只答 202 的 mock 等 GET 连上再推。
- **`ToolConcurrency::Parallel` 是承诺**：可重入 + 轮询 cancel，核心不校验，谎报只在生产数据竞争。
- **`#[non_exhaustive]` 只给消费侧枚举**；`Role`/`CompressTrigger`/生产侧枚举不加。
- **思考展示**：headless 写 stderr 不进 stdout；TUI 独立 ✻ 区块不走 markdown，长思考不提前切分、最终折叠并可 `/thinking` 全文重印。
- **TUI 布局/切分/队列**：显示宽度按 grapheme 不按 char，caret 与极窄屏用真实 cell-grid/cursor 钉；soft-cut 放弃路径原子、完整行推 fence、未闭合 inline code 不切；队列只存 `VecDeque`，不得做成阻塞 watermark 的 ghost；空态/弹窗/正文/输入区一律用 `content_width()` 的 120 列上限（`zone_rect` 是终端全宽，直接用它会让宽屏右缘错位）。
- **TUI 子 agent 气泡不切分**：live 区逐 delta 全量重解析是 O(气泡²)，触发条件是不守「简短总结」契约的子 agent（`TextDelta` 不受 `max_output_chars` 约束）。
- **context_breakdown 等和靠单趟 running-floor**（各段独立 /4 会差 1-2 丢不变量）；无 thinking 桶（思考增量永不进 history），`preserved_reasoning` 开时旁字段 `Message.reasoning` 另计 `reasoning` 桶；锚点失效即校准回 None。
- **`Usage.reasoning_tokens` 为 0 不序列化**（结构体与 result.json / report.json 侧）：绑定测试别把它塞进「四轴恒在」的断言循环——非推理模型下这个键根本不出现，要测它就给 mock 一帧带 `completion_tokens_details.reasoning_tokens` 的 usage。
- **本机 `strip = "debuginfo"` + macOS 27 的 dyld**：用户级 `~/.cargo/config.toml` 的 `[profile.dev] debug = false` 会让 cargo 给 dev profile 默认 `strip = "debuginfo"`，rustc 链接后剥 debuginfo 产出的 cdylib（绑定的 `.so` / `.node`）LINKEDIT string pool 8 字节错位，macOS 27.2 的 dyld 直接拒载（`mis-aligned LINKEDIT string pool`）——`maturin develop` / `npm run build:debug` 成功但 `import` / `require` 失败。可执行文件与 proc-macro 的布局恰好对齐，所以只有绑定暴露。绕过：`CARGO_PROFILE_DEV_STRIP=none`（环境变量，不改仓库与用户配置）；CI 无此全局配置、release profile 不受影响。
- **本地 clippy 绿 ≠ CI clippy 绿（工具链版本差）**：CI 跟最新 stable、本机默认可能落后好几个版本，新 stable 收紧的 lint 本地看不见——动公开错误类型的字段、或本地全绿而 CI 的 clippy 格红时，装一个与 CI 同版的工具链（`rustup toolchain install <版本> --component clippy,rustfmt`，与默认并存）配隔离 `CARGO_TARGET_DIR` 把全部 clippy 配置与 `fmt --check` 再跑一遍。实发：`RunError` 恰 128 字节触发 `result_large_err`，旧版只报函数签名、新版连转发结果的闭包一起报——处置是字段装箱 + 编译期断言，不逐处 allow。audit / msrv 两格的红多半是环境漂移（RustSec 新公告、runner 自带 Python 超过 pyo3 上限），先看日志再动代码。
- **回归评测装置（`scripts/regress_eval.py`）**：臂配置压过任务的 mock 专用调参（arm-over-mock）；清单落盘前脱敏（含 MCP 嵌套 `headers` / `env` 与 URL query）；`[skill]` 按 `XDG_CONFIG_HOME` / `HOME` 查找——**两条通道**都经 `run_env` 隔离 `XDG_CONFIG_HOME`（否则开发者本机的真实技能目录会漏进轨迹）并设 `GIT_CEILING_DIRECTORIES`（批次目录在仓库里，模型在 workdir 发的 `git` 不得解析到外层仓库——第一次真机批次把开发者未提交的 diff 发给了模型）；mock MCP server 的每个工具都要有一条合法调用探针（脚本化模型不调的工具等于没测过，`transfer_stock` 拒收整数参数零成本通道一直是绿的）；改写过的 mock 必须真执行一遍（`check` 抓出过一份只有运行时才炸的 `mock_anthropic.py` 语法错误）；动 harness 先写/跑 `scripts/tests/test_regress_eval.py`（本地 pytest，不进 CI）——七个缺陷都是先写测试才在跑之前抓出来的；批次目录 / `--base-config` 一律解析成绝对路径（run 的子进程 cwd 是 run 目录，第一次真机批次因相对路径六个 run 启动即败），一条通道第一次真跑前先用**与文档逐字相同的命令行**对 mock 排练；给 `limits` 加键要四处同步（内核 clamp / harness 的 clamp 镜像与 pytest 计数 / 两份 README / 所有配置指纹一次性偏移）。

## 离线 / 视觉验证

- **性能基准**：`./scripts/check.sh bench`（`crates/core/benches/loop_bench.rs`，零依赖手搓
  `std::time::Instant` + `harness = false`，**不引 criterion**——单二进制/零依赖是项目定位）。五组：
  agent loop 一轮（空历史 / 200 条历史，`Agent::new` 不计入）、`Agent::new` 本身（resume / 子 agent
  的一次性成本）、`context_breakdown()` 两种历史规模、`confined_to` 三条路径。报中位数与 p95
  （不看均值：一次调度抖动不该动头条）+ 一行 `BENCH name=… median_ns=…`。
  **不在默认 `all` 集**：数字只在本机前后版本间可比，CI 不设性能门。改了 loop / context_breakdown /
  confined_to 后手动跑一轮看趋势；`[profile.bench]` 在根 Cargo.toml 覆盖 release 的 `opt-level="z"+lto`，
  别删（否则测的是发布体积权衡，不是代码本身）。**bench 的 agent 组门在 `mock` 后**，四个 CI clippy
  配置都不带 mock、看不到它——所以 `bench` 组自己也跑一遍 `clippy --features mock`，否则这块永远没人 lint。

- **场景 e2e**：`python3 scripts/e2e_scenarios.py`——内置 SSE 场景 mock（线程化、零密钥），走真实
  CLI/HTTP 栈守四个 loop 级行为：打转护栏（stuck_loop 硬停 / 同参不同果不误停 / nudge 只进历史不进
  事件流）、单轮 10 路并行 task fan-out（8/波 + N:N + 请求数精确核算）、SIGINT exit 130。CI 有
  同名 job（ubuntu）。改了 loop/取消/打转行为后必跑。
- **MCP 端到端**：`python3 scripts/e2e_mcp.py`——真实 headless 二进制 × 文件内自带的零依赖 mock MCP
  （stdio / Streamable HTTP）+ 脚本化 LLM，十组线上行为（工具名单、配置报错、Ctrl-C 取消通知、调用/握手
  超时键、协商版本、404 会话恢复、202 standing stream、凭据卫生、空闲探测）约 15 s；`--idle` 才跑空闲
  探测组（I，约 65 s），`A K` 这类参数只跑指定组。**不是 CI 门禁**：改了 `mcp.rs` 或任何 spawn 链后手动跑
  （Ctrl-C 组在 Windows 跳过；`TA_BIN` 指定二进制）。
- **回归任务集**：`examples/regress/tasks/<id>/`（任务片段 + 同级 `harness.json` 评测侧数据 + `check.py`
  判卷 + fixture）× `scripts/regress_eval.py`。`check` 是 CI 门禁（`check.sh e2e` 组第三步）：逐任务 × 两种
  线协议走脚本化 mock 的零成本全链路，并在任何环境下都发不出付费请求；`prepare` / `run` / `compare` 是本地
  付费 A/B 通道——`run` 默认 dry-run、`--execute` 才付费，没同时给 `--price-table` 与 `--cost-stop` 的 batch
  一律 `blocked_paid`，费用停止按「已花 + 下一个 run 的最坏预留 ≥ 停止值即停」、未上报用量按预留记——所以 `--cost-stop` 要大于单个 run 的预留（现价表下约 3.9 USD），给小了一个 run 都不发。
  改了内核之后跑的就是它；harness 自身的离线单测在 `scripts/tests/test_regress_eval.py`（本地 pytest，
  不进 CI）。
- **mock server**：`examples/mock_openai.py`、`examples/mock_anthropic.py` 是零依赖 SSE mock，
  用于无 API key 跑通整个 loop（tool_use 配对、usage 解析、取消）。改动 provider/agent 后用它回归。
- **进程内回归**：`agent.rs` 的 loop 不变量用 `provider/mock.rs`（脚本化 Provider，不连网）单测覆盖
  （测试体在 `agent_tests.rs`），比 mock server 更快、断言更准。新增/改动 loop 行为优先在这里加用例。
- **Slot 观测契约表**（`agent_tests.rs` 的 `slot_table_*`）：`Slot` 只有 `Cancelled`/`Ran` 两态，但进入
  路径有九条（含并行 worker panic：合成 `Ran` 错误槽、`duration_ms` 为空、`after_tool(executed=true)`）。六个用例用同一张表钉住「状态 → 五类观测面」（records / ToolResult 事件 / ToolCall 事件 /
  before_tool / after_tool），并派生出两条不变量：**before↔after 严格配对**、**ToolCall↔ToolResult 事件配平**。
  改了并行批次的分支（新增 Slot 赋值点、改占位/合成逻辑）先看这张表再动。
- **TUI 视觉验证**：用 ratatui `TestBackend` 抓 cell grid 做断言（见 `tui::tests`）；端到端交互用
  pty 脚本驱动（项目里惯用 `/tmp/ta-verify/` 放临时 mock + pty 脚本，跑完即删）。**不要只靠
  "看起来对"**——改了渲染/光标/markdown 要真抓屏或单测断言验证。pty 驱动六个坑：裸 pty 必须
  应答 crossterm 的 `\x1b[6n` 光标查询（回 `\x1b[<行>;<列>R`，否则启动即 "cursor position
  could not be read" 退出；固定回底行会把视口钉在屏幕底部，要贴近真实布局就按真实光标位置应答）；
  带补全弹窗的斜杠命令要**双回车**（第一个 Enter 被弹窗吃掉当选择）；
  增量重绘只回写变化的 cell，剥掉 ANSI 后的可见文本是乱序拼接的——**断言用稳定子串，别断言整行
  或整句标题**（「不再出现 X」类断言取 X 的稳定片段最后一次出现之后的尾部再判）；滚动区画面疑似
  重叠须用真实终端模拟器交叉核对（根因：pyte 0.8.2 根本没实现 SU/SD——`CSI S` / `CSI T` 落 no-op，
  而 scrollback 提交靠的就是它们，补 handler 的做法见 pitfalls），截图渲染器与终端事实要分开验证；
  pty 尺寸要在 fork **之前**设好（`pty.fork()` 后父进程再 `TIOCSWINSZ` 与子进程首次取尺寸竞态，
  新 pty 初始 0×0）；
  断言「空闲双击
  Ctrl-C 退出」要先等状态栏回到 `idle`——忙时第一下是取消当前轮、第二下只是空闲态的首次按下
  （只打印提示不退出），且 `idle` 这类标记要在**发键之后**收到的输出里找（起始屏幕上本来就有）。
- **TUI 快速体验**：`./scripts/tui_demo.py`——进程内起零密钥 SSE mock（127.0.0.1 随机端口），用
  隔离的演示目录启动真实 TUI；消息以 `tool` / `think` / `slow` / `fail` 开头分别演示工具调用、
  长思考折叠、慢速流式（队列 / 取消）、失败态与队列暂停，其余消息回使用指引。`--light` 浅色、
  `--release`、`--no-build`、`--real -- <参数>` 用自己的配置（此时不起 mock）。**不是 CI 门禁**
  （门禁是 `scripts/e2e_scenarios.py`）：人工看渲染效果用；`--no-build` 复用的
  `target/debug/tiny-agent` 若被 `check.sh` 的 `build: code` 组换成无 `tui` 的构建，二进制会明确报错。
