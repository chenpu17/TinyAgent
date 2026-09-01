# TinyAgent — 项目备忘

单二进制、同步（无 tokio）的极简 AI agent。一个 JSON 配置，两种模式：headless（harness / 评测）
与 TUI 对话。详细用法见 `README.md`，本文件记的是**开发时容易踩的坑与约定**。

## 常用命令

```sh
cargo build --features tui                 # 默认只编 headless；TUI 要带 feature
cargo test --features tui,mock             # 单测（含 TUI 输入/渲染逻辑 + lib_api 集成测试）
cargo clippy --features tui --all-targets  # lint TUI 版
cargo clippy --all-targets                # lint headless 版（两个 feature 配置都要过）
cargo fmt --all                            # 格式化（CI 有 --check 门禁，改完代码必跑）
```

每次改完**两个 clippy 都要跑**：TUI 代码在 `#[cfg(feature = "tui")]` 后，默认构建看不到，
默认配置的 clippy 也看不到 TUI 代码——单跑一个会漏。**`cargo fmt --all` 同理**（CI 的 fmt
job 对全 workspace `--check`，含双绑定 crate；存量 305 处漂移已在一个纯格式 commit 清零，
该 commit 记在 `.git-blame-ignore-revs`，`git blame` 配 ignoreRevsFile 后可跳过）。

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
- **发布（tag）**：版本号同步 5 处（根 `Cargo.toml`、`bindings/{python,node}/Cargo.toml`、
  `pyproject.toml`、`package.json`）+ `CHANGELOG.md` 补段，然后 `git tag vX.Y.Z` 推双远端
  （gitee origin + github）。tag 推 GitHub 触发 `release.yml`（musl 产物，tarball 附
  README + LICENSE）。流程详见 README 发布节。许可证是 **MIT**（根 `LICENSE`，5 处元数据
  已同步）——新增子模块/产物别忘带许可声明。

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
  **工具参数增量（`ToolInputDelta { id, name, delta }`）与 ThinkingDelta 同款纪律**：纯展示馈送、永不进
  `text`/`blocks`——openai 在 `arguments` 分片落地即发（空串过滤）、anthropic 在 `input_json_delta` 复用
  pending 块的 id/name，权威装配输入在 `ToolCall` 事件 / `Block::ToolUse`。**键是 tool_use id 不是 name**
  （同 turn 并行同名调用 name 必撞；两侧 id 都在流早期可得）。harness `--json` 吐 `{"type":"tool_input",...}`，
  plain headless 刻意安静（完整 ToolCall 行随后来）；TUI 走 `_ => {}` 兜底。mock 的 `OkToolStream` 变体
  驱动 loop 级测试（分片→事件→拼装）。新增流式透出项照这套纪律抄。
- **PDF 文档块（`Block::Document { media_type, data }`，Anthropic-only）**：对齐 Anthropic `document` content block（base64 source；url/Files-API `file_id` 两种 source 不做——宿主自己下载后推字节）。**四层接入点清单（新增 Block 变体照抄）**：① anthropic `message_json`（`{"type":"document","source":{"type":"base64",…}}`）；② openai `chat()` 入口 `reject_documents` **显式报错**（chat-completions 无 document 内容类型，静默丢弃比报错更糟；`message_jsons` 里的 omission placeholder 文本臂是防御纵深——正常路径不可达）；③ compress 三处（`total_chars` 给 base64-len/100 地板走 legacy char 阈值；`estimate_tokens` 按**页数×`PDF_TOKENS_PER_PAGE`=3000** token-side 加法；`window_lines` 出 `<document>` 占位行）；④ tui `transcript_rows` 显示跳过（与 Image 同款）。mock 的 capture 是 `_ => {}` 兜底、agent loop 不穷尽 match Block——新变体不强制点名这些点。**token 预估按页不按字节**：官方口径 1,500–3,000/页（逐页转图+抽文本，官方自己 3 页全视觉样例 ≈2,333/页），社区实测偏低估 → 取上沿 3000（preflight 宁早压，真实 usage 仍是记账权威）。`pdf_page_estimate`（compress.rs）免解析启发式：裸扫 `/Type /Page`/`/Type/Page`（尾字节非字母数字，排掉 `/Pages` 父节点与 `/PageMode` 类长名）与 `/Pages` 节点 ±窗口内 `/Count` 取 max（救 ObjStm 压缩流藏 page 对象的 PDF 1.5+；窗口限定在页树节点附近是防 outline 书签的 `/Count` 混入），双零按 ~100 KiB/页兜底、地板 1——每次 preflight 重解码重扫（10 MiB 封顶 ~ms 级，不值得给 Block 加缓存状态）。**实测对真实产商 PDF 精确**（CUPS 文本管线 3/46 页、Quartz 图像管线 1 页，PDFKit 真值逐一匹配；`tests/fixtures/cups_3p.pdf` 真件 fixture 永久钉住；ObjStm 藏 page 对象的 Word/Acrobat 类输出离线无产商、仅合成用例覆盖——`/Count` 恢复路径在案）。读取 helper `tools/document.rs::load_document_block`（`.pdf` 后缀 + `%PDF-` 头 + 10 MiB take 有界读，镜像 `load_block`；lib.rs 门面 re-export）。**刻意不做 `load_document` 工具**（模型驱动加载是后续可选项；本期 host-push only，经 `push_user`）。
- **核心 loop 在 `agent.rs`**：累计 usage、cancel 复位、上下文压缩的有状态编排（`compress`/
  `summarize_prefix`/`summary_call`，读写 Agent 字段 + 驱动 provider）在这里。压缩的**纯函数与常量**
  （`estimate_tokens`/`find_safe_cut`/`fold_summary`/`window_lines`/`chunk_lines`/`SUMMARY_*` 等）抽到
  `compress.rs`（单向被 agent 依赖；`window_lines` 反向调 `crate::agent::preview`，不构成环）。`AgentEvent`
  透传给 harness（headless 打印/NDJSON）或 TUI。上下文压缩的摘要请求自身 usage 也计入累计（诚实成本）。
  **`find_safe_cut` 空尾语义**：keep_recent=0 时 cut==n（空尾）是**合法**切点——真实 loop 每个请求点
  历史必 user 结尾，若钳到 n-1 扫边界必落空、该配置下压缩被静默禁用（A2 修复）；`fold_summary` 对空尾
  产出单条 user 头。守卫是 `saturating_add(2)`（usize::MAX 级 keep_recent 不 panic/回绕）。
  **数值写端一律 saturating**：usage 累计 / RunCost / merge_usage / trace_meta / batch 汇总的 `+=`
  全部是 `saturating_add`（读端 `Usage::total()` 本就饱和）——新增累加点照做，谎报 u64::MAX 不 panic
  （debug）也不回绕躲预算闸门（release）。
  **溢出确定性**：溢出逃逸失败后的溢出错误经 `RunError::is_context_overflow()` 判定为确定性——单跑
  `run_with_retries` 与 batch（stop_reason `context_overflow`，run_one 只重试 `"error"`）都不再进退避。
- **token 预算闸门（fleet 级）**（`limits.max_total_tokens`，0=不限）：`Agent::run` 入口安装 run 级
  共享花费计数器（`ToolCtx::budget: Option<Arc<AtomicU64>>`，每次顶层 run 换新——与 usage 每 run
  重置对齐；`run_as_subagent` 经 `child_ctx` 继承同一个），**父与每个在飞 `task` 子 agent 的闸门读的
  都是它**——并行子 agent 集体触顶，不再各自烧满一份预算后 join 才被发现。写入端：**直接 chat 走
  `bill_chat`**（`fetch_update` 饱和累加 + 本地 fold），覆盖轮次 chat 与压缩摘要两个账单；**工具账单
  （子 agent `ToolOutput.usage` 折进父 totals）走普通 `fold_usage`、绝不进计数器**——子 agent 运行期
  已自己记过直接花费，父再记一次会把有效预算砍半（有测试钉住）。闸门超限以 `stop_reason:
  "budget_exceeded"`（exit 125）停；turn 边界语义同 `timeout_secs`——顶破上限的那轮照常跑完
  （工具执行、assistant 入历史），闸门只拒开下一轮，配对不破。batch task 片段只能收紧（更小值），
  0 视为"无上限"被忽略。它和 `max_turns`/`timeout` 是 loop 的三个 turn 边界停止条件。
- **宿主热更新面（run 外的 `&mut self` 方法，GUI 常驻场景）**：`set_model` / `set_reasoning_effort` /
  `set_max_tokens`（钳 ≥1）/ `set_max_context_tokens` 与 `compact()` 都是**纯字段操作/复用 compress 路径**，
  `&mut self` 签名即顺序契约——run 持有借用，「轮次间调用」由借用检查器强制，无需运行期守卫。两条易错语义：
  ① **`task` 子 agent 不跟随**（SubAgent 持构造期 cfg 快照，set_model 后 spawn 的子 agent 仍用旧模型）——文档
  化的边界，别为传播去改 SubAgent 读父字段；② **`compact()` 的成本口径**：摘要 usage 经 `CompressOutcome`
  返回宿主、**绝不进 `self.usage`**（run 入口重置总账，折进去会凭空消失）——与 run 内压缩走 `bill_chat` 进
  run 账单是对偶的两条路径，改 billing 的人别合并它们。`compact` 入口清陈旧 cancel + compress_failed
  （手动调用是新的显式尝试）；内部 `compress(events, deadline, trigger)` 的失败通道归调用方（run 循环发
  Warning 事件、compact 透传 Err——内部发会双报），空摘要退化路径返回 `dropped:0` + usage 不吞账。
  `CompressTrigger::{Preflight,Overflow,Manual}` 非 non_exhaustive（tui.rs 穷尽 match 是想要的编译期点名），
  加变体要同步 harness ×2 + tui ×1 的 match。
- **MCP 运行时热增删（`Agent::add_mcp_server` / `remove_mcp_server` / `tool_names`，同属热更新面）**：
  路由按 `Tool::mcp_server()` **能力标记**（`tools/mod.rs` trait 默认 None，`McpTool` 覆写返回**配置名**
  ——未消毒，与展示名/wire 名三方独立；agent 侧零 downcast 零前缀解析，第三方工具同理可参与）。易错点：
  ① **撞名 = 可恢复 Err + 回滚**：`tools::mcp::build_one` 先 spawn 后 `check_new_names`（纯函数：新旧全集
  HashSet 撞即 bail，含新 server 内部互撞），冲突 `?` 返回时 fresh Vec 被 Drop → `StdioTransport::drop`
  杀进程组——绝无部分生效；② **热增路径不打印**（`build_one` 与 `build()` 共用 `start_server`，但 build 的
  `mcp: ready` eprintln 只留在启动路径——宿主可能在 raw-mode TUI 下，agent 层永不打印）；③ **增删后
  `refresh_tool_defs`** 重建 `tool_defs` + `tool_schema_chars`（请求侧镜像与预扣估算都吃它），
  `validate_tool_names` **不重跑**（sanitize 结构性保证字符集/长度、唯一性靠闸门——重建校验属重复防御）；
  ④ **子 agent 不感知**（SubAgent 持构造期 `mcp_tools` 快照：热增 server 对本 agent 生命周期内任何时点
  spawn 的子 agent 都不可见——产品决策「文档化不传播」，共享槽位留 v2）；remove 一个仍被子 agent 快照
  持有的 server 时连接活到最后持有者 drop（armed reaper 会话末兜底杀 child）；⑤ add 时若已
  `set_shutdown` 则新工具补挂同一 flag（临近退出热增不遗留孤儿）。**活的握手/调用/冲突回滚/进程回收链路**
  只在 `agent_tests.rs::hot_add_remove_mcp_server_live`（`#[ignore]`，本地 python3 起
  `examples/mcp_echo.py`，`cargo test hot_add_remove -- --ignored --nocapture`；末尾 pgrep 断言 child
  零残留）；hermetic 用例只钉路由/错误形状/无变异三件事。
- **子 agent（`task` 工具，`tools/subagent.rs`）**：主 agent 把聚焦子任务交给一个全新子 agent
  （独立历史 + 独立 turn 预算，`SUBAGENT_MAX_TURNS=25` 封顶），只把最终摘要作 tool 结果返回。
  **同 turn 多个 `task` 调用并行**：`agent.rs` 的工具执行分两遍——叶子工具（shell/read_file/…）
  顺序跑，并行能力调用收集后用 `std::thread::scope` fan-out（每个一个 scoped thread），结果按原块序
  拼回（tool_use/tool_result 仍 N:N）。**路由按能力标记、不按名字**：分流依据是
  `Tool::concurrency() == ToolConcurrency::Parallel`（`SubAgent` 覆写为 Parallel），loop 里没有任何
  `"task"` 字面量分流——第三方工具声明 Parallel 即获同款 fan-out（义务见「容易踩的坑」）。子 agent 的
  token 用量经 `ToolOutput::usage` 折进主 run 累计（失败也照计已完成轮次）。事件实时转发，裹成
  `AgentEvent::SubAgent{name}` 区分并行的几个。
  **不可递归**：子 agent 的工具白名单是**全量构建后按 `Tool::available_to_subagent()` retain**
  （`child_tools()`，同样不按名字滤）——`task`（防递归）与 `todo`（计划归父）在 trait 里覆写 false，
  第三方工具可自主退出。**在默认白名单**（`config::default_tools`
  含 `task`）——headless 评测任务也会默认能 spawn 子 agent；评测若要锁定行为/成本，在 config 的 `tools`
  里显式去掉 `task`。**预算与墙钟继承**：子 agent 的预算闸门读父安装的共享计数器（见「token 预算
  闸门」条）；`timeout_secs` 经 `clamp_child_timeout` 收紧为父的**剩余时间**（`ToolCtx::deadline`，
  只紧不松、地板 1s——0 会意外松成"不限"），不再克隆完整第二份超时。粒度仍是轮次边界：同轮已并行
  启动的兄弟互不见对方本轮后续花费，严格控成本仍留余量或去掉 `task`（README limits 节同款表述）。
- **Hooks 拦截层（`hooks.rs`，P1，第三方扩展面）**：`trait Hooks: Send + Sync` 三默认方法
  （`before_tool` → `ToolDecision::{Allow, Deny(String), Modify(Value)}`；`after_tool` 事后审计；
  `on_chat_request` 只读观察主 turn 请求），`Agent::set_hooks` 后置安装（仿 `set_cancel` 模式）。
  装配（`lib.rs::assemble`）留 `ToolCtx::hooks = None`——二进制与绑定**零行为变化**，只有库用户
  显式安装才生效。**子 agent 经 `child_ctx()` Arc-clone 继承同一层**（顶层拒绝 = 子 agent 内也拒绝，
  无绕过）；`ToolCtx::subagent` 以名字标注子 agent 身份供钩子区分。**关键语义**：Deny 产出
  `is_error` 结果照常入记录/配对，**不占 `Slot::Cancelled`**（那是取消专用——被拒调用要进审计账，
  被取消的不进）；Modify 只换**执行**输入，审计链（records/`ToolCall` 事件）永远保留模型原始输入；
  顶层闸门在父线程 spawn 前**串行**把关，子 agent 自身闸门跑在各自 worker 线程（兄弟真并发，实现方
  须自行线程安全——`Send + Sync` 只保证传对象内存安全）；压缩摘要请求**不**透出；`--resume` 重建的
  agent 需重挂（层活在实例上、不进 session 文件）。本期绑定不透出 hooks（backlog P3）。
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
  核心构造点还有 `tools/{image,mcp,subagent}.rs` + `agent_tests.rs`。改 `ToolCtx` 字段同理：库内全部
  字面量构造点（lib.rs assemble + 各工具/绑定测试）都要补字段——绑定层经 `build_agent_with` 走
  assemble、**不构造 ToolCtx**，零改动。`timeout_secs` 是**软上限**（管 chat 请求 / 轮次边界 / 子
  agent 剩余时间；工具各有独立上限、绑定 host 回调不可中断），README limits 节有完整边界表述——新增
  不受它管的等待点时别把它当硬墙钟引用。
  **TUI 配合（状态行 plan 段，破冻结的唯一例外）**：TUI 主循环持 `plan: Vec<TodoItem>` 局部状态，`TodoUpdate` arm 直接 `plan = todos.clone()`
  （全量替换，不 push sys_info 行——完整 checklist 已在 `todo` 工具结果行可见）。每帧把 `plan done/total`（全完成绿/进行中黄）作为
  `build_status_line` 的 `plan: Option<(&str, Style)>` 段渲染。`build_status_line` 的可选段按**存活优先级**贪心添加：ctx > plan > stats
  （窄屏先省 stats、再 plan、ctx 最后），用一个 `Vec<(String,Style)>` + 累积 `prefix_w` 的通用循环取代旧的 if/else-if；**plan=None 时行为与
  启用前逐字节相同**（既有 `build_status_line_*` 测试全过）。这是原"冻结只修 bug"政策的首次破例（后来的
  TUI 体验批次将政策整体放宽为"基础体验对齐、架构红线不动"，见 TUI 条目——本段是当时的首个例外，历史保留）。
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
- **消息稳定 id（`Message.id: Option<String>`，宿主 sidecar 锚点）**：**ingest 点分配**（不是构造时——
  全局计数器会让 67 处测试构造的期望值断言爆掉，测试本就只断 len/role/blocks，ingest 分配零扰动）：
  `try_new` 入口（initial/resume）、`push_user`（新消息才铸；merge 进末尾 user 沿用原 id）、run 循环
  assistant 持久化、`compress` fold 之后（`assign_missing_ids`）。铸法 `fresh_message_id`：`m<n>` 计数器 +
  存量碰撞扫描——resume 的历史带着前一个进程铸的 id，单纯重启计数器会重发旧号；宿主自带 id 原样保留
  （其间唯一性是宿主契约）。`next_msg_id` 不随 `clear_messages` 重置（同血脉 id 永不重复）。serde
  `default + skip_serializing_if`：无 id 序列化逐字节同旧格式、旧 session 文件照常加载。压缩保头/尾原
  id（锚点不漂移）、摘要载体铸新 id。**新增"消息进历史"入口的人必须补 id 分配点**（漏了 → 宿主锚点
  对那条消息拿 None）；绑定层零代码改动（pushUser 自动铸），Node 测试有一处 deepStrictEqual 期望带
  `id: 'm1'`。
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
  **TUI 定位（原"冻结只修 bug"政策已按产品决定放宽）**：交互体验对齐主流终端 AI 工具的基础项
  已落地一批（见下），红线仍是**架构与渲染模型**——不做全屏 / 鼠标托管 / 主题系统这类结构性扩张。
  已落地的扩展面速记：
  - **流式增量定稿（`split_streaming_row(&mut rows, width)` + `Row::AssistantCont{text,tight}`）**：
    在途 Assistant/Thinking 行的**显示行数**（`est_display_rows`：逐字符 unicode-width 累加折行——
    是渲染实际行数的**上界**，markdown 段内软连接只会更少行，宁早切勿漏切）超过阈值即切分：前缀
    定稿进 scrollback、尾段继续 live 流式。**切分点两级**：优先最新安全段落边界（`last_safe_boundary`，
    空行分隔、前缀围栏闭合）——整段 markdown 块完整定稿，续块 loose（块间补空行恢复段落间距）；
    段落本身超过阈值（**含 LLM 常见"一整段一行"散文——逻辑行数是 1，任何基于 `lines().count()` 的
    阈值对它永不触发，必须按显示行**）退化为**显示行切分**（`soft_cut_offset`：切点总在折行边界，
    尾段保留最后 `LIVE_TAIL_ROWS=2` 行），续块 tight——渲染紧贴前行（无空行，段落不被撕成两半）、
    不重复 ✻ 标记、`/export` 以 `\n` 拼接。围栏内切分给尾段补合成 ```` ``` ```` 开栏（未闭合前缀照样
    渲染成代码块；转录 strip 回去）。**阈值两档**：首切 `INLINE_HEIGHT`，切过一次后（尾行是
    Cont/tight Thinking）降为 `LIVE_TAIL_ROWS`——之后每写一行即提交；年轻段落（≤2 行）仍走段落
    切分保住空行结构。**`trailing_spacer` 陷阱（`build_history_lines` 第 6 参）**：commit 渲染的是
    `rows` 切片，切片末行若按"历史末尾"无条件补空行，每批增量提交都往 scrollback 塞一个多余空行、
    tight 段落被撕开——只有 `pending_end == rows.len()` 才传 true。续块无 ● 图标（一条回复不读成
    N 条消息）；子 agent 气泡不切。改 `split_streaming_row` 签名（带 width）或 Cont/Thinking 形状的
    人：测试里 ~10 处构造/匹配要跟着改。
  - **思考区块（`Row::Thinking`）**：ThinkingDelta 不再与答复折进同一 Assistant 行（混排不可读），
    走 `append_thinking_delta` 进独立暗色斜体 `✻` 行——`thinking_stream: Option<String>` 是当前在飞
    trace 的 tag（"" 父 / 子 agent 名，merge 判据同 `sub_streaming_name`），答复/工具/任何其它行插入
    即关闭、再次 trace 开新行；**不走 markdown/MdCache**（视觉上必须区别于答复）；`commit_pending_end`
    与 `inflight_row_index` 都把 Thinking 与 Assistant 同等对待（在飞 hold back / live 区渲染 + `▌` 光标，
    `build_history_lines` 的 `LiveRow::{Text,Think}` 决定光标归属）；空 trace 渲染为空、`/export` 以
    `✻ Thinking` 段落收录。改这三处的人：新增任何"在飞行"必须同步 holdback/inflight/LiveRow 三件套。
  - **render-only 瞬态模式**（`thinking`、`tool_running` 同款）：渲染期合成、不进 rows / MdCache /
    转录——live 区渲染优先链 popup > thinking > inflight 流式行 > tool_running 占位
    （`→ 工具名 · 1.2s…`，并行 fan-out 聚合 `(+N)`，busy 时 120ms tick 走字）。新增瞬态展示照此模式。
  - **`/out [n]`**：从 rows 尾部找第 n 新的 `ToolResult` 克隆置 `expanded: true` 重印
    （`nth_last_tool_result`）——展开渲染与提交通道全复用，无新渲染路径；`SLASH_COMMANDS` 一张表
    同时驱动 /help 与补全弹窗（新增命令只改表 + classify + Enter match 三处）。
  - **resume 转录**（`tui::transcript_rows(&[Message]) -> Vec<Row>`）：main.rs 在 build_agent
    **消费 `initial` 之前**快照；先一遍扫全部 ToolUse 建 id→name map，ToolResult 按 id 反查配名
    （查不到用 `(unknown)`），`elapsed: None`（字段本就为 resume 设计）；Image 跳过（转录是显示层）。
    resume 时**跳过 initial_task 镜像**（末条 user 已在转录里，再镜像重复）；`Row` 因此 `pub(crate)`。
  - **浅色主题**（`TINYAGENT_TUI_THEME=light`）：`THEME_LIGHT: OnceLock<bool>` 进程级一次设定
    （`run()` 入口读 env，OnceLock 无法复位——**测试不得 set 它**，light/dark 两态逻辑抽纯函数
    `theme_light_from` / `redye_inline_code` 测）；dim 文字走 `dim_fg()`（dark=Gray 现状 / light=DarkGray）、
    行内代码走 `code_fg()`（dark=White / light=青）——新增暗色 UI 元素用 `dim_fg()` 别再引常量。
  - **键位语义**：单行输入（含空）或浏览中 ↑/↓ 走历史召回（判定 `up_is_history`），多行恢复跨行
    移动；`Ctrl-D` 删后字符（`InputBuffer::delete_forward`，backspace 镜像）/ 空输入空闲退出；
    行尾 `\`+Enter 换行（行续接）；空闲 `Ctrl-C` 双击 2s 窗口确认退出（判定 `ctrl_c_should_quit`），
    忙时单击取消不变。`is_normal_stop` 名单含 `cancelled`（Esc 已提示，Done 行是噪声）。

## 容易踩的坑

- **语言绑定的工具回调是跨线程 round-trip，不能简化成同步直调**：
  - **Python**（`bindings/python`）：`Agent` 存进 `Mutex<Agent>`，所有跨线程方法走单一
    `with_agent` 入口（`py.allow_threads` 内拿锁——闭包与返回值须 `Send`）。`run()` 释放 GIL
    跑 loop；自定义工具的闭包在 agent 线程执行，用 `Python::with_gil` **短暂重获 GIL** 调回
    Python。`set_cancel(token)` **必须在 `run()` 之前**调（把共享 `Arc<AtomicBool>` 装进
    agent）——run 期间持有 mutex，`run()` 入口不复位 cancel（与主 agent 一致）。
    **重入即死锁**（P1-2 教训）：回调里再调 agent 方法会 GIL↔Mutex 互等——run 期间的
    agent 方法调用经 `IN_RUN` thread-local 判定，报 "agent is running" 而非挂死；
    两个线程各自调方法也要靠 with_agent 的锁排队，别绕过它直拿 mutex。
  - **Node**（`bindings/node`）：`run()` 是 `AsyncTask`（libuv worker 线程，**无 tokio**），事件经
    `ThreadsafeFunction` 回主线程。自定义 JS 工具是 **TSFN round-trip**：agent 线程把 `{id,args}` 经 TSFN
    派给 JS、阻塞 `mpsc` 等结果；JS（`wrapper.js` 把 `run(args)` 包成 `(err,req)` 形状）调完用户函数后调
    导出的 `resolveTool(id, result)`，经全局 `PENDING_TOOLS: i64→Sender` 注册表唤醒 worker（120s
    `recv_timeout` 兜底）。**JS 主线程在 `run()` 期间必须空闲**（调用方 `await`），否则 TSFN 回调排不进
    主循环 → worker 永久阻塞 → 死锁。**同步方法（pushUser 等）在 run 期间返回 busy 错误**（`try_lock`
    而非阻塞等锁——等锁会冻结 JS 主循环，而 TSFN round-trip 靠它转动，等于自死锁）；**onEvent 回调抛
    异常不致命**（wrapper try/catch + 异步 catch 双保险，降级 stderr——TSFN 的 Fatal 策略会让 host 的
    一个日志 bug 杀掉整个进程）。`resolveTool` / `PENDING_TOOLS` / `build_extra_tools` 三者是一套，
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
- **shell 正常退出后的管道孤儿回收**（`tools/shell.rs::collect_output`）：读管道线程的 EOF 要等
  **所有**写端关闭；命令留下的后台进程继承工具管道（`sleep 60 & echo x`，或 `cd d && server
  >/dev/null 2>&1 & curl …`——`&` 后台化整个 AND-list，为其 fork 的子 shell **自己**持有 fd1/2，
  内层重定向管不到它），直接子进程退出后 EOF 永不到达 → 旧版在无条件 `join()` 上**永久挂死**
  （超时/cancel 检查都在轮询循环里、break 后不可达；run 级 `timeout_secs` 只 cap chat 请求、
  救不到工具执行；唯一逃生门是 headless 二次 Ctrl-C 硬退）。现修法：读线程 EOF 时经 mpsc 单通道
  发 `(ReaderDone::Stdout/Stderr, 字节)`，收尾三段——①宽限 2s（`PIPE_GRACE`）等自然 EOF
  （`recv_timeout` 50ms 切片查 cancel，取消立即进②；正常命令 EOF 即时到、零新增延迟）②
  `kill_process_group` 强制 EOF：进程组**在组长退出后仍存活**（reparent 不改 pgid），与超时路径
  同一把刀、只是触发时机不同（注意：此时子进程已 reap，pid 理论上可复用为新 pgid——2s 内跑完
  整 pid 空间不可能，注释在案）③再等一个宽限仍无 EOF（`setsid` 逃逸 / Windows 的 kill 只能杀
  已死的直接子进程）→ 放弃 join、标注后返回（泄漏 2 个读线程到进程退出，字节丢失；Windows 正解
  Job Objects 在 backlog）。**留后台进程的正确姿势是重定向其输出**（`cmd >log 2>&1 &`）——
  不持管道即不触发回收、跨调用存活，有单测钉住这条逃生门。回收/泄漏/取消标记只追加在**正常退出
  路径**输出尾部（truncate 之后防被截掉）；None（超时/取消杀）路径输出与旧版逐字节一致；
  `is_error` 仍按 exit code 判（回收是旁注、不谎报失败）；cancel 在宽限期命中时保留真实
  exit code + 追加 cancelled 标记。
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
  limits 内各字段的覆盖策略由 `config::Clamp` 三变体逐字段声明（`merge` 是唯一权威实现）：
  `Tighten`（小即紧，plain min）、`TightenOrInit`（0=无上限是**最松**值——片段的 0 忽略、
  base 为 0 时采纳任何有限值、否则 min；canonical 解释只写在 Clamp 文档一处）、`Replace`
  （**唯一例外 `shell_timeout_secs`**，双向可覆盖不 clamp——成本/延迟旋钮不是安全边界，
  评测构建常超默认 120s；想烧时间的任务用一万条快命令同样能烧，真兜底是 run 级
  `timeout_secs` + `max_total_tokens`）。
- **新增 `Limits` 字段的接入流程（别加回 `..` 解构）**：`Limits` + `PartialLimits` 各加字段
  （后者 `Option<T>` + `#[serde(default)]`）→ 在 `Config::apply` 的 limits 分支里给它选一档
  `Clamp` 并补一行 `merge`。**`let PartialLimits { ... } = l;` 是穷尽解构、没有 `..`**——
  新字段不分类就是编译错误，这是刻意设计（旧版手写 if-chain 曾把 0-哨兵逻辑化石复制四份、
  悄悄丢过覆盖），禁止为"省事"加回 `..`。同理 `Config::apply` 顶层的 `PartialConfig`
  解构也是穷尽的。
- **批量 `--retries` 只重试 transient**（`stop_reason == "error"`），指数退避 1..32s 封顶；
  `bad_task`/`max_turns` 等确定性失败不重试。`--failed` 重跑失败子集后**合并**进完整报告
  （未重跑的历史成功项原样保留）。
- **batch workdir 隔离（`--isolate none|copy|git`，`batch::IsolateMode`）**：`none` 默认共享
  workdir（jobs>1 打 ⚠）；`copy` 每任务拷贝（排除 `target`/`node_modules`/`.git`/`results`），符号
  链接**自包含策略**（`copy_entry`）：源树内相对链接复刻 / 绝对在内拷内容 / 出根（绝对或 `..` 逃逸）
  跳过 + 告警——副本里不允许有通往原树的路（绝对链接指回原 workdir 会把并行任务重新耦合到共享
  目录），包含判定复用 `tools::confined_to`，自引用解引链深度封顶 64；
  `git` 每任务 `worktree add --detach`（要求 workdir 是 repo 根，canonicalize 比较——macOS
  `/tmp`→`/private/tmp` symlink 会让字符串比较误拒；否则 `setup_error` 不重试）。**per-task、
  跨 attempt 复用**（重试看得到前次落盘，与共享语义一致）；`IsolationGuard` Drop 清理（copy
  remove_dir_all；git `worktree remove --force` 失败回退 remove_dir_all，孤儿用 `git worktree
  prune` 回收——release `panic=abort` 下 Drop 不保证）。产物随目录丢弃是**评测语义**（审计面是
  result.json/report.json）。jobs>1 ⚠ 只在 isolate==None 时打。
- **批量 Ctrl-C 是 worker gate，不中断在跑任务**：`batch::run` 的 worker 在**起任务前**检查共享
  cancel 标志，命中则标 `cancelled` 跳过（首个命中的 worker 打一次性契约警告）；在跑的任务照常跑完
  （"收敛"）。**例外（A5，既定取舍）**：每个 agent 都用 `set_shutdown` 挂了同一个 flag 的 reaper——首次
  Ctrl-C 也会**立即杀掉在跑任务的 MCP server**（防二次 Ctrl-C 硬退留孤儿子进程，0.3.0 修复的另一半）；
  依赖 MCP 的在跑任务可能因此提前失败。**没有**把 cancel 标志传给
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
  **fix 落账是 record-on-success**（A3）：本轮累积 applied 清单、仅当调整后的请求最终 2xx 才
  `learned.record`——值错误 400（"max_tokens must be ≤ N"）文案也含 max_tokens 会触发改名，但重试
  必再失败，apply 即记会让此后每轮主动发改名后的字段、反复触发不可修复的 400。改这段别把落账挪回
  apply 时刻。**learned fixes 只活在 provider 实例内存**：每 run 重建、不落盘、跨进程/绑定不共享
  （刻意——落盘引入跨 run 毒化面）；同一网关新 run 会重新学，每类 400 多一次试错往返。
- **流中途错误的诚实成本形状（两 provider 对齐，改流解析必读）**：openai 与 anthropic 的
  `parse_loop` 对读失败（`StreamError`）与 `{"error":…}` 事件都返回 `Ok` + 尾附一条
  `[stream error] …` 标记文本块 + `stop_reason:"stream_error"` + **已上报的 usage**——
  agent.rs 的 stream_error 提取路径据此把前缀转成可重试 `RunError` 并计费。新增流错误路径照这个
  形状，**别 bail!**（会丢掉网关可能已发的 usage，破坏"失败也计费"契约）。Anthropic 另有
  `saw_usage` 标记：流里完全没有 usage 帧时 usage 返回 `None` 而非 `Some(全零)`——下游按
  Some/None 区分"计了 0"与"用量未知"，OpenAI 侧 usage 本就是 `Option` 天然满足。
- **`harness::atomic_write` 的临时文件用 `create_new(true)`（O_EXCL）**：tmp 名
  `<target>.<pid>.<counter>.tmp`，`AlreadyExists` 就换计数器重试。别"简化"回 `create(true)`
  或可预测单名——写工具只对**目标**路径做沙盒检查、看不到临时兄弟名，预置符号链接同名文件会把
  "写进沙盒"变成写穿到任意路径（P1-1 修的就是这个）。同理 `--json` NDJSON 输出走
  `print_ndjson`（写失败静默放弃），别退回 `println!`（下游 `| head` 断管直接 panic）。
- **batch `stop_reason` 词汇含 `panic`**（P2-9）：worker 用 `catch_unwind(AssertUnwindSafe)`
  兜 `run_one`，payload 经 `panic_report` 转确定性失败条目（≠ `"error"`，不进瞬态重试）。
  仅 debug/CI（unwind）生效——release `panic="abort"` 下进程当场死，兜不住（注释在案）。
  新增 worker 内的代码别绕开 catch_unwind 直接调 run_one。
- **`--session none` 是关闭持久化的字面量哨兵**（不是文件名）——真要叫 `none` 的文件没法用，
  但它语义清晰、够用。改 `session_target` 判定前先想清楚。
- **MCP 工具名只消毒展示名，wire 名不动**（`tools::mcp::sanitize_name`）：对外名 `mcp_<server>_<tool>`
  非 `[A-Za-z0-9_]` → `_` 并折叠，但 `wire_name` 保留服务器原始工具名，`tools/call` 用它分发。
  改消毒逻辑别误动 `wire_name`，否则分发会找不到工具。`dedup_check` 在消毒后撞名时启动报错。
  **工具名全集校验在 `lib.rs::assemble` 末尾**（`validate_tool_names`）：最终列表（内置+MCP+skill 注入
  +extra）统一查非空 / `[A-Za-z0-9_-]` / ≤64 / 唯一——自定义工具与内置同名此前被静默遮蔽（`find_tool`
  首匹配）。改工具装配顺序/新增注入工具不影响该校验（在最后跑），但别在校验后再 push 工具。
- **MCP stdio `env` 注入**（`McpServerConfig.env` + `StdioTransport::spawn` 的 `.envs(&cfg.env)`）：
  stdio 子进程的额外环境变量，叠加在宿主环境之上（同名以配置为准）。GUI 进程无 shell 环境靠它传凭据
  （`GITHUB_PERSONAL_ACCESS_TOKEN` / 连接串 / `XXX_API_KEY`）。`#[serde(default)]` 向后兼容；`validate()` 不拦
  （HTTP 传输忽略 env，不报错——env 只对 stdio 生效）。两条 Command 链（unix `process_group(0)` + `#[cfg(not(unix))]`）
  都要加 `.envs`，别只改一条。
- **思考内容展示策略**：`ThinkingDelta` 在 `harness` 的 `--json` NDJSON 发 `{"type":"thinking","delta":...}`；
  headless 终端打印写 **stderr**（不进 stdout 的 `summary`，可 `2>/dev/null` 屏蔽）；TUI 用 or 模式
  TUI 渲染为独立暗色斜体 `✻` 区块（`Row::Thinking`，不走 markdown/MdCache，与答复视觉分离；可折叠区块仍是 TinyWorker 的事）。
  子 agent 的思考经 `AgentEvent::SubAgent` 整体包装转发（`subagent.rs` 不逐变体 match），harness 的 SubAgent 内嵌
  match 单独把它写 stderr 带 `[name]` 前缀。
- **改 `agent.rs` 核心路径必须配套 loop 测试**。测试外置在 `agent_tests.rs`（`agent.rs` 末尾
  `#[cfg(test)] #[path = "agent_tests.rs"] mod tests;` 引入，靠 `use super::*` 仍可达 agent 私有项；
  压缩纯函数的测试靠 `use crate::compress::*`）。用 `provider/mock.rs` 的脚本化 Provider 覆盖不变量：
  cancel 复位、tool_use/tool_result N:N 配对（含中途取消时合成的 `[cancelled by user]` 进 history
  但不进 `tool_calls`）、压缩、usage 累计、hooks 闸门（Deny 记录不占 Cancelled / Modify 审计留原始
  输入 / Allow 与无 hooks 逐字段等价，`CaptureHooks` 记录精确调用序列）。动了 loop 逻辑先补/查这些
  测试，别只靠 mock server 跑一遍就提交。
- **Hooks 三条易错语义**（完整合同在 `hooks.rs` trait 文档，改实现前必读）：① **调用次序**——
  同 turn 内先顺序块逐个过叶子闸门，再按并行块序过批次闸门（混合 turn 里**不是**全局块序）；
  `after_tool` 镜像之（叶子即时、批次 join 后、被拒调用在决策时刻）。② **`--resume` 不带走 hooks**——
  层活在 agent 实例上、不进 session 文件，重建后要重挂；文档与绑定透出（P3）时别暗示持久。
  ③ **Modify 的审计分裂是有意的**——执行用替换输入，`ToolCallRecord.input` / `ToolCall` 事件保留
  模型原始输入（日志答"模型要什么"、输出答"实际发生了什么"）；`todo` 被 Modify 时 `result.json.plan`
  仍从记录的原始输入派生。另：Deny 落 `Slot::Ran(error)` **不是** `Slot::Cancelled`（被拒要进记录
  与账单审计，取消不进）；顶层闸门串行（父线程 spawn 前），子 agent 自身闸门在 worker 线程真并发。
- **`ToolConcurrency::Parallel` 是承诺不是路由提示**：工具覆写 `concurrency()` 即向 loop 保证
  ① **可重入**——`run`/`run_with_emit` 可能同时在多个 scoped 线程上跑（同一个 `&self`），共享可变
  状态必须自带同步；② **取消响应**——等待间隙轮询 `ctx.cancel`（参考 `shell`），否则 Ctrl-C 只能
  等满本次执行。谎报 Parallel 的工具不会编译报错、只会在生产里数据竞争——核心不校验，靠这条约定。
  同理 `available_to_subagent()` 返回 false 是退出子 agent 继承的唯一方式（没有名字黑名单）。
- **`#[non_exhaustive]` 卫生标准**（Commit 3 定死）：只给**消费侧**枚举（第三方 `match` 的：
  `AgentEvent`/`Block`/`ChatEvent`/`ToolConcurrency`）加；`Role`（封闭轴）、`CompressTrigger`
  （tui.rs 有穷尽 match）与一切**生产侧**枚举（第三方构造、核心消费，如 `ToolDecision`）明确不加。
  crate 内该属性不生效——lib 内部穷尽 match 照旧，新增变体仍会编译期点名所有内部消费点
  （这是想要的提醒）；唯一外部消费点是 bin crate 的 `tui.rs` 主事件 match 那行 `_ => {}`
  （编译适配，渲染不变）。新枚举拿不准归哪侧时问一句：第三方会对它 `match` 还是构造它？

## 离线 / 视觉验证

- **mock server**：`examples/mock_openai.py`、`examples/mock_anthropic.py` 是零依赖 SSE mock，
  用于无 API key 跑通整个 loop（tool_use 配对、usage 解析、取消）。改动 provider/agent 后用它回归。
- **进程内回归**：`agent.rs` 的 loop 不变量用 `provider/mock.rs`（脚本化 Provider，不连网）单测覆盖
  （测试体在 `agent_tests.rs`），比 mock server 更快、断言更准。新增/改动 loop 行为优先在这里加用例。
- **TUI 视觉验证**：用 ratatui `TestBackend` 抓 cell grid 做断言（见 `tui::tests`）；端到端交互用
  pty 脚本驱动（项目里惯用 `/tmp/ta-verify/` 放临时 mock + pty 脚本，跑完即删）。**不要只靠
  "看起来对"**——改了渲染/光标/markdown 要真抓屏或单测断言验证。
