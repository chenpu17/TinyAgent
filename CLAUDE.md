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
cargo test --features code,mock            # run_code 沙箱套件（改 run_code.rs 必跑）
cargo fmt --all                            # 格式化（CI 有 --check 门禁，改完代码必跑）
```

每次改完**三个 clippy 配置都要跑**（headless / tui / code）：`#[cfg(feature)]` 后的代码，
不带该 feature 的构建与 clippy 都看不到——单跑一两个会漏。TUI 代码在 `tui` 后；
`run_code.rs` 与 `agent.rs` 的 `set_code_*` 在 `code` 后。CI 里三个 clippy 都带 `-D warnings`，
本地跑出任何黄字即等于红灯。**`cargo fmt --all` 同理**（CI 的 fmt job 对全 workspace
`--check`，含双绑定 crate；存量 305 处漂移已在一个纯格式 commit 清零，该 commit 记在
`.git-blame-ignore-revs`，`git blame` 配 ignoreRevsFile 后可跳过）。
**测试命令必带 `--features mock` 才真正跑到 `tests/lib_api.rs`**：该文件整体门在
`#![cfg(feature = "mock")]` 后（`provider::mock` 是 `cfg(any(test, feature = "mock"))`，
而集成测试编译时 lib 不带 `cfg(test)`），不带 mock 的 `cargo test` 把它编成 **0 个测试
照样全绿**——绿灯不等于 lib_api 集成测试跑过。CI 另有两道本地复现不了的 job：musl
静态构建冒烟（每次 push 必过，"嵌入式低配 Linux"是核心交付定位）与 cargo-audit
（RustSec 扫依赖树）——升级/新增依赖或动平台相关代码时留意这两道门。

### 语言绑定（workspace 成员 `bindings/python`、`bindings/node`）

两者 path-依赖根库（`features = ["mock"]`），都复用 `CallbackTool` 注入自定义工具、不改核心。两者各带一个
默认关闭的 `code` **转发 feature**（`code = ["ta_core/code"]`）：编入后 `enable_code`/`disable_code`/
`set_code_config`（Node 侧 camelCase）真实生效；未编入时三方法返回带重建提示的明确错误（设计 §8.6——
避免给所有 wheel 强加 ~10MB wasmtime）。绑定测试 build-自适应（探测 wheel 是否带 feature，另一侧 skip）。
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
- **发布（tag）**：版本号同步 5 处（根 `Cargo.toml`、`bindings/{python,node}/Cargo.toml`、
  `pyproject.toml`、`package.json`）+ `CHANGELOG.md` 补段，然后 `git tag vX.Y.Z` 推双远端
  （gitee origin + github）。tag 推 GitHub 触发 `release.yml`（musl 产物，tarball 附
  README + LICENSE）。流程详见 README 发布节。许可证是 **MIT**（根 `LICENSE`，5 处元数据
  已同步）——新增子模块/产物别忘带许可声明。

## 高频纪律（几乎每次改动都适用）

- **三个 clippy 配置 + `cargo fmt --all` 全跑**（见上「常用命令」），黄字即红灯。
- **测试必带 `--features mock`**，否则 `tests/lib_api.rs` 编成 0 个测试照样全绿。
- **改 `agent.rs` 核心路径必须配套 loop 测试**（`agent_tests.rs` + `provider/mock.rs` 脚本化 Provider，
  不连网）——别只靠 mock server 跑一遍就提交。
- **数值写端一律 `saturating_add`**（usage / RunCost / merge_usage / batch 汇总所有 `+=`）。
- **cancel 契约：`run()` 永不清标志，受理方（TUI run_once / harness / batch / compact）调
  `clear_cancel()` 清残留**——别给任何 run 入口加回清残留。
- **新增"消息进历史"入口必须补 id 分配点**（漏了宿主锚点拿到 `None`）。
- **`ToolOutput`/`ToolCallRecord`/`ToolCtx` 加字段要同步全部字面量构造点**（含两个绑定）。
- **`Config::apply` 的 `PartialConfig`/`PartialLimits` 解构是刻意穷尽的（无 `..`）**——新字段不分类
  即编译错误，禁止为省事加回 `..`。
- **LLM 可见文案即规格**：run_code 预检拒答/墓碑文案在 `docs/finetune_support_design.md` 附录 A
  （微调数据有依赖），改一个字先走该文档修订记录；run_code 工具描述/skill 正文同理三处同源同步。

## 架构要点索引（详录：`docs/claude/architecture.md`）

- **bin + lib 双 target**：改 lib 内部不动 bin；`crate::` 在 lib 内指 lib.rs 根。
- **同步模型**：`reqwest::blocking`，无 tokio；每个流式请求必带 per-request timeout 兜底防 reader 线程泄漏。
- **思考/工具参数增量透出**：只发事件、永不进 `text`/`blocks`；新增流式透出项照同款纪律抄。
- **`reasoning_effort` 按字符串原样透传**，不做枚举校验；anthropic 走独立顶层 `output_config`，别用裸顶层字段。
- **PDF 文档块**：新 Block 变体照四层接入点清单（anthropic / openai 显式报错 / compress 三处 / tui）；token 预估按页×3000。
- **压缩**：`find_safe_cut` 空尾是合法切点；`summary_call` 空正文（思考烧穿）预算加倍重试一次，effort 恒 `None`。
- **`salvage_on_max_tokens`**：max_tokens 截断且无 tool_use 时一次性 wrap-up nudge；带 tool_use 不 salvage。
- **token 预算闸门**：run 级共享计数器父子同读；工具账单（子 agent 折回）绝不进计数器。
- **宿主热更新面**：`set_model` 等不传播给已构造的子 agent 快照（既定决策）；`compact()` 成本绝不进 `self.usage`。
- **MCP 热增删**：撞名 = Err + 回滚；热增路径不打印；子 agent 与 run_code registry 都不感知（快照语义）。
- **子 agent（task）**：按 `Tool::concurrency()`/`available_to_subagent()` 能力标记路由，无名字黑名单；共享父 MCP、预算与 cancel。
- **Hooks**：Deny 落 `Slot::Ran(error)` 不占 Cancelled；Modify 只换执行输入、审计留原始输入；`--resume` 不带走 hooks。
- **todo + meta**：plan 从 records 派生不经压缩；`meta` 审计侧信道不进模型可见 content。
- **Skill**：三级加载；name 必须等于父目录名；`read_skill`/`list_skill` 不在白名单、由 assemble 配套注入；子 agent 不继承。
- **run_code**：两层开关默认全关；wasmtime 中断四点纪律（入口门/epoch 回调/host_call 入口/收尾裁决）；`max_wasm_stack` 384 KiB；测试必须先 warm 模块。
- **run_code 触发文案**：规则明确 AND 剩余规模；`agentic-code.md` 为独立评测变体，原提示词不变；契约同步用 `python3 scripts/sync_run_code_contract.py --check`，交接见 `docs/run_code_routing_report.md`。
- **run_code 微调支持**：`skill_injection` 三态（声明序=披露序，batch 收紧=min）装配期消费、热更不重注；`precheck` 调用期经 snapshot；检出集合只有 `{run_code, task, todo}`；拒答/墓碑文案即规格在 `docs/finetune_support_design.md` 附录 A，改文案先走其修订记录。
- **guest I/O**：prelude 清单封闭（改 Prelude 必须同步描述 + 设计附录 A）；guest 输出预算不继承 `ctx.max_output_chars`（用 2M）。
- **read_file 分页**：offset 跳行不花字节预算；json content 逐行保留原始行终止符；raw 默认窗口 3 字节对齐。
- **env 注入 system prompt**：唯一收敛点在 `Agent::new`（子 agent 也过）；改 assemble 注入会漏子 agent。
- **retry 诚实成本**：失败轮的 usage/tool_calls 经 `RunCost` 落进 result.json，不清零。
- **headless Ctrl-C**：首次翻 cancel 优雅收尾（exit 130），二次硬退；批量模式 worker gate 同款。
- **消息稳定 id**：ingest 点分配（非构造时）；`next_msg_id` 不随 `clear_messages` 重置。
- **TUI**：inline viewport + scrollback 提交模型；新增"在飞行"必须同步 holdback/inflight/LiveRow 三件套；新暗色元素用 `dim_fg()`。

## 容易踩的坑索引（详录：`docs/claude/pitfalls.md`）

- **绑定回调是跨线程 round-trip**：Python 走 GIL + `with_agent` 锁（重入报 "agent is running"）；Node 走 TSFN（run 期间 JS 主线程必须空闲，同步方法 busy 报错）。
- **自定义 provider 回调**：host 抛异常 = 诚实 fail 该轮，不静默空回复。
- **shell 管道孤儿**：后台进程继承管道会卡 EOF——留后台进程必须重定向其输出；回收标记只追加在正常退出路径。
- **`panic = "abort"` 下 Drop 不跑**：调 TUI 还原 / MCP 清理用 debug 构建；`process::exit` 前显式 `drop(agent)`。
- **MCP shutdown ≠ cancel**：reaper 只听 shutdown（会话结束）；轮取消只置 cancel——别把 reaper 接回 cancel、别给 `set_cancel` 加回 tools 传播。
- **`serde_json::json!` 宏按所有权取值**：`&String`/`&Value` 要 clone，`Option<&str>` 显式转。
- **Anthropic 不允许相邻 user**：新 user 入口走 `append_user_turn`；tool_use/tool_result 必须 N:N 配对。
- **batch 覆盖白名单**：安全字段覆盖即 `bad_task`；limits 逐字段由 `Clamp` 三变体声明，`shell_timeout_secs` 是唯一 Replace。
- **batch `--retries` 只重试 transient**（`stop_reason == "error"`）；`stop_reason` 词汇含 `panic`（catch_unwind 兜底）。
- **batch workdir 隔离**：`copy` 符号链接自包含策略；`git` 要 repo 根（macOS /tmp symlink 需 canonicalize 比较）。
- **OpenAI 400 回退**：三条 fix 各管一个字段，record-on-success（只记最终 2xx 的修复），learned fixes 不落盘。
- **流中途错误形状**：返回 Ok + 尾附 `[stream error]` 标记 + 已上报 usage——别 bail（会丢 usage 破坏失败计费）。
- **`atomic_write` 用 `create_new(true)`**：别改回可预测单名（符号链接预置攻击面）；NDJSON 走 `print_ndjson` 别用 `println!`。
- **`--session none` 是字面量哨兵**，与 `--resume` 互斥。
- **MCP 工具名**：只消毒展示名，wire 名不动；`validate_tool_names` 在 assemble 末尾、之后别再 push 工具。
- **MCP stdio `env` 注入**：两条 Command 链（unix / 非 unix）都要加 `.envs`。
- **`ToolConcurrency::Parallel` 是承诺**：可重入 + 轮询 cancel，核心不校验，谎报只在生产数据竞争。
- **`#[non_exhaustive]` 只给消费侧枚举**；`Role`/`CompressTrigger`/生产侧枚举不加。
- **思考展示**：headless 写 stderr 不进 stdout；TUI 独立 ✻ 区块不走 markdown。

## 离线 / 视觉验证

- **mock server**：`examples/mock_openai.py`、`examples/mock_anthropic.py` 是零依赖 SSE mock，
  用于无 API key 跑通整个 loop（tool_use 配对、usage 解析、取消）。改动 provider/agent 后用它回归。
- **进程内回归**：`agent.rs` 的 loop 不变量用 `provider/mock.rs`（脚本化 Provider，不连网）单测覆盖
  （测试体在 `agent_tests.rs`），比 mock server 更快、断言更准。新增/改动 loop 行为优先在这里加用例。
- **TUI 视觉验证**：用 ratatui `TestBackend` 抓 cell grid 做断言（见 `tui::tests`）；端到端交互用
  pty 脚本驱动（项目里惯用 `/tmp/ta-verify/` 放临时 mock + pty 脚本，跑完即删）。**不要只靠
  "看起来对"**——改了渲染/光标/markdown 要真抓屏或单测断言验证。
