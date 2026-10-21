# 架构要点（详录）

> 自 CLAUDE.md 归档的完整版（2026-09 瘦身拆分）。CLAUDE.md 只保留每主题一句话索引；
> 修改条目时详情写在这里，并同步更新 CLAUDE.md 的对应索引行。


- **双 crate：内核 + CLI（workspace 拆分）**：内核 crate `crates/core`（package
  `tiny-agent-core`，lib 名 `tiny_agent`）持有 agent loop、工具、provider、config、skills、
  MCP、session、压缩、装配（`build_agent*`）与 `fsutil`（原子写，session/write_file/result.json
  共用）；门面 re-export 在 `crates/core/src/lib.rs`。根 package `tiny-agent` 是 CLI 半边：
  `src/main.rs`（clap 参数、配置发现、SIGINT、`process::exit`）+ 本地模块 `harness`（headless
  呈现/result.json/NDJSON/重试梯子）、`batch`（工作池/isolate/报告）、`tui`（feature tui）、
  `signal`，依赖内核 crate 并 `pub(crate) use tiny_agent::{...}` 把模块拉进 bin crate 根——
  main/tui/harness/batch 里 `crate::agent::...` 等路径照常解析。**改内核内部不用动 CLI**；
  CLI 行为是内核之上的薄壳。内核内 `crate::` 指 lib 根。`mock` feature 把 `provider::mock`
  （否则 `cfg(test)`）放出，供绑定/下游离线 dry-run；CLI 另有转发 feature（`code`/`mock` →
  `tiny_agent/code|mock`）让根目录的 `cargo build --features ...` 与 `--version` 清单保持拆分前
  语义。自定义工具共享底座 `tools::CallbackTool`（包闭包，impl `Tool`）——Python/TS 绑定的
  callable 经 GIL/ThreadsafeFunction 闭包塞进来，共享同一 `Tool` impl。

- **同步**：`reqwest::blocking` + rustls，没有 tokio。流式用一个 worker 线程 + `CancellableSse`
  （chunk 间 ≤200ms 响应 cancel，30s idle 超时）。`sse-reader` 线程阻塞在 reqwest `read()` 里、读不到
  cancel 标志，所以**每个流式请求必带 per-request timeout 兜底**（`STREAM_BACKSTOP`，10min；有 run budget
  时改用剩余预算）——否则半开连接会让 reader 线程在 caller 已放弃（idle/cancel）后无限挂住（线程+连接泄漏）。
  reqwest blocking 没有 idle-read timeout，per-request total cap 是唯一能保证 reader 退出的手段。
  **SSE 内存三层限额**（`SseReader`，外部评审 P2-6——时间兜底不是字节兜底，快网关在超时窗内可灌任意量）：
  单行 `SSE_LINE_MAX`（16 MiB）→ 单事件聚合 `SSE_EVENT_MAX`（4×行帽，防无空行终止的 `data:` 行无限拼接）
  → 单响应累计 `SSE_TOTAL_MAX`（64 MiB，防无数小事件累加；消费端拼装文本正由这些 payload 构成，故同时
  封顶留存内存）；`CancellableSse` 的通道是 `sync_channel(16)`——生产快于消费（宿主事件回调慢，如绑定
  的跨线程 round-trip）时 send 阻塞产生 TCP 背压，而非无界排队。限额可注入（`next_data_with`）便于小值
  单测；新增流式读取路径照抄同款三层。
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
- **核心 loop 在 `agent.rs`**：累计 usage、cancel 轮询、上下文压缩的有状态编排（`compress`/
  `summarize_prefix`/`summary_call`，读写 Agent 字段 + 驱动 provider）在这里。压缩的**纯函数与常量**
  （`estimate_tokens`/`find_safe_cut`/`fold_summary`/`prune_tool_results`/`window_lines`/`chunk_lines`/
  `SUMMARY_*` 等）抽到 `compress.rs`（单向被 agent 依赖；`window_lines` 反向调 `crate::agent::preview`，
  不构成环）。`AgentEvent` 透传给 harness（headless 打印/NDJSON）或 TUI。上下文压缩的摘要请求自身
  usage 也计入累计（诚实成本）。**分层漏斗（业界对齐）**：LLM 摘要前先走 `prune_tool_results`
  （保护窗外 >2000 字符旧 tool_result → 占位符，配对保留、todo 豁免、<20000 字符释放不重写）——
  preflight 超阈值先剪枝重估、溢出兜底先剪枝重试，常可免掉计费摘要调用；preflight 估算经
  `UsageAnchor` **锚定 provider 实测 prompt tokens**（input+缓存写+缓存读，前缀/system/工具表指纹
  校验，失效回退全量启发式——修 chars/4 对中文低估 2~3 倍）；`fold_summary` 折叠时保真注入
  `TODO_MARKER`（最新 todo 清单）与 `USER_QUOTE_MARKER`（中间用户消息原文，2000/条、12000 总额
  FIFO）；被折叠/被剪的原始消息进 session `archive` 段（`save_with_archive`/`load_with_archive`，
  4096 条 FIFO，格式向后兼容）。宿主入口 `compact()`/`compact_with(events, instructions)`（焦点指令
  拼进摘要 user prompt 尾部，不进 SUMMARY_SYSTEM），Python/Node 绑定同款 `compact(instructions?,
  on_event?)`。
  **`find_safe_cut` 空尾语义**：keep_recent=0 时 cut==n（空尾）是**合法**切点——真实 loop 每个请求点
  历史必 user 结尾，若钳到 n-1 扫边界必落空、该配置下压缩被静默禁用（A2 修复）；`fold_summary` 对空尾
  产出单条 user 头。守卫是 `saturating_add(2)`（usize::MAX 级 keep_recent 不 panic/回绕）。
  **`summary_call` 空正文重试（思考网关自愈，TinyWorker 反馈 #7）**：混合思考网关（deepseek/glm）把
  reasoning_content 计入 max_tokens，UPDATE 分支（带旧摘要）诱导的长推敲可整块烧光 1024 起步预算 →
  `finish=length` 映射成 stop_reason `"max_tokens"` 且**正文零字**——「截断仍连贯」的接受假设在零正文时
  为假，空文本随后落退化分支（dropped=0 + Warning）。修法是 provider 无关的两段尝试：`attempt==0 &&
  stop_reason=="max_tokens" && text.trim().is_empty()` → 预算 `saturating_mul(2).min(8192)` 重试一次；
  加倍值**刻意不钳回调用方的轮次上限**（首轮已证明 cap 级预算必死于思考，钳回等于重放已知必死的请求），
  认死理的网关对加倍值报 400 则该次压缩失败（≈旧退化、多一次廉价请求）。非空正文的真截断首轮即接受、
  重试仍截断也接受。两轮 usage 经 `merge_usage` 合并返回（诚实成本——烧掉的首轮照计费）。**配套纪律**：
  summary_call 的 `reasoning_effort` 恒 `None` 且**不**改成低档值——Anthropic 上任何 Some(非 "none")
  effort 是**开启** adaptive thinking（与意图相反），OpenAI 兼容网关对 "low" 的解释是掷硬币；默认开思考
  的网关交给上面的空正文重试兜住。
  **`salvage_on_max_tokens`（max_tokens 截断一次性收尾自愈，bench 实证驱动）**：assistant 轮以
  stop_reason `max_tokens` 截断且**原始 blocks 无 tool_use** 时，push_user 一条英文 wrap-up nudge
  （`SALVAGE_NUDGE_PARTIAL`/`SALVAGE_NUDGE_EMPTY`——**刻意不写 "Final answer:"**，那是评测判卷契约，
  产品不耦合答案格式）再跑一轮。两个触发点：①aborted 分支尾部（有部分文本，assistant 消息已在历史，
  nudge 追加为独立 user 轮）；②空 blocks break（思考烧穿零文本，空 assistant **不入历史**——nudge 经
  append_user_turn 折进尾随 user）。约束：每 run 恰一次（`salvage_used` 局部，salvage 轮再截断照原样
  返回）；消耗 turn 预算、照常 bill_chat 计费与 on_chat_request hooks、`continue` 回循环头自然过
  cancel/max_turns/timeout/budget 四闸门（零特判）；**截断带 tool_use 不 salvage**（半调用已丢弃即停，
  aborted 测试钉住）。事件 `AgentEvent::MaxTokensSalvage`（unit 变体：`--json` 吐
  `{"type":"salvage"}`、plain 走 stderr 一行、TUI wildcard 兜底）。Config 软字段默认 true
  （`PartialConfig` 软覆盖、不在 forbidden 表）；子 agent 经 cfg 快照自动继承、各 run 独立一次。
  **数值写端一律 saturating**：usage 累计 / RunCost / merge_usage / trace_meta / batch 汇总的 `+=`
  全部是 `saturating_add`（读端 `Usage::total()` 本就饱和）——新增累加点照做，谎报 u64::MAX 不 panic
  （debug）也不回绕躲预算闸门（release）。
  **打转护栏（stuck_loop）**：装配段对 `Slot::Ran` 的调用按（工具名, `canon_input` 规范化**生效**
  参数, 结果指纹=`stuck_result_hash`(is_error+content)）三连键计数连续重复：第 `STUCK_NUDGE_AT`=3
  次往该次 tool_result **历史块**追加纠偏提示（ToolResult 事件与 records 原样，UI/result.json 不
  污染）；轮尾连续计数仍 ≥ `STUCK_STOP_AT`=6 则以 `stop_reason="stuck_loop"` 收尾（N:N 完整、
  轮边界语义同 max_turns_reached、exit 默认臂 1 不重试）。**进展感知**（2026-09 外审第四轮）：
  键带结果指纹——同参不同果（轮询 10%→60%）即重置；键用 hook Modify 后的生效参数
  （`execute_tool_call` 新增 `effective_out` 出参 + 并行 gate Modify 臂落 `effective_inputs`），
  审批层改写成分页的调用不共键。**canon 数值经 f64 归一**（2026-09 外审）：`1` 与 `1.0` 字面不同、
  语义相同，provider/gateway 重发同参调用时数值类型抖动会拆键漏检——所有 Number 统一走 `as_f64()`
  格式化（超 2^53 整数精度损失只折指纹区分度，方向是宁漏检不误杀）。**无 stuck_stop 闩锁**：轮尾直读当前连续计数——同轮第 7 次换法
  即自动解除停止。**取消优先**：轮尾先查 cancel 再查打转，同轮内落下的取消报 cancelled/130。
  已知盲区（刻意取舍，两个方向都有）：**漏检侧**——结果嵌时间戳/随机 id 的真打转逃逸检测；
  **误杀侧**——静态文本轮询（curl 恒错、探测输出恒定）与「进展只在 image 里」的截图类轮询：
  结果指纹恒定即同键，真无进展与「进展不进键」无法区分，6 连硬停（nudge 已提示模型换探测
  方式，属护栏本意的边界）。run_code 内层
  host_call_tool 不经装配段、不计入打转（有 fuel/epoch 兜底）。**同轮语义**：单轮内 6 连
  重复直达硬停——模型在该轮发出全部调用时尚未看到任何结果，nudge 写进历史块也等不到下一
  次请求，同轮场景 nudge 名义存在、无自愈窗口（一轮承诺 6 次相同调用本身即退化形态）。
  计数跨轮持久、随 run 重置；`Cancelled` 槽不计数（未执行）。测试组 8 个：
  nudge→停 / 换参重置 / 键序不敏感 / 进展结果不误停 / Modify 生效参数 / 同轮换法解停 /
  取消优先 / 数值拼写归一。
  **溢出确定性**：溢出逃逸失败后的溢出错误经 `RunError::is_context_overflow()` 判定为确定性——单跑
  `run_with_retries` 与 batch（stop_reason `context_overflow`，run_one 只重试 `"error"`）都不再进退避。
- **token 预算闸门（fleet 级）**（`limits.max_total_tokens`，0=不限）：`Agent::run` 入口安装 run 级
  共享花费计数器（`ToolCtx::budget: Option<Arc<AtomicU64>>`，每次顶层 run 换新——与 usage 每 run
  重置对齐；`run_as_subagent` 经 `child_ctx` 继承同一个），**父与每个在飞 `task` 子 agent 的闸门读的
  都是它**——并行子 agent 集体触顶，不再各自烧满一份预算后 join 才被发现。写入端：**直接 chat 走
  `bill_chat`**（`fetch_update` 饱和累加 + 本地 fold，计数器半边抽成 `BudgetGate::share`），覆盖轮次 chat
  与压缩摘要两个账单；**工具账单（`ToolOutput.usage` 折进父 totals）按 `usage_in_shared_budget` 标记
  分流**——`task` 子 agent 置 true（子 agent 运行期已把自己的直接 chat 花费记进继承的计数器，父再记一次
  会把有效预算砍半），父 fold 只进 totals 不进计数器；其余任何上报 usage 的工具（宿主 CallbackTool
  计量上游 API 等）默认 false——那笔花费没进过任何计数器，fold 时由 agent 代为 `share_budget` 进闸门
  （外部评审 P2-3：此前一律不进计数器，工具花费只进账单不进闸门，预算可被绕过）。run_code 的 guest
  内层调用在内层 fold 点就地 `share`（混合来源聚合后统一置 true 上报）。**cap 单源**（2026-09 外审）：
  turn 边界的比较读 `BudgetGate::cap`（装闸时的 fleet 上限），自身 `limits.max_total_tokens` 快照只能
  把它**收紧**（`min`）——子 agent 快照一旦与父 cap 分叉，比较快照会让 fleet 预算被静默放大
  （`budget_gate_cap_outranks_a_looser_limits_snapshot` 钉住）。闸门超限以 `stop_reason:
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
  `set_shutdown` 则新工具补挂同一 flag（临近退出热增不遗留孤儿）。**run_code 的 guest registry 同样
  不感知**（与 ④ 同一条政策，外部评审曾当 P1 报过——实为既定快照语义）：registry 按构造期四件套在
  `Arc<OnceLock>` 里惰性建一次、跨克隆共享，热增的 server JS 看不到；热删的 server 仍可被已建 registry
  调用（连接活到 registry drop；该 server 已不在顶层列表 → shutdown 不再经它武装 reaper，最坏退出时
  等满一次 MCP CALL_TIMEOUT 后由 `StdioTransport::drop` 杀进程组——延迟，无孤儿）。**活的握手/调用/冲突回滚/进程回收链路**
  只在 `agent_tests.rs::hot_add_remove_mcp_server_live`（`#[ignore]`，本地 python3 起
  `examples/mcp_echo.py`，`cargo test hot_add_remove -- --ignored --nocapture`；末尾 pgrep 断言 child
  零残留）；hermetic 用例只钉路由/错误形状/无变异三件事。
- **MCP SSE endpoint 同源闸门（`mcp.rs::resolve_endpoint`）**：旧式 HTTP+SSE transport 的 `endpoint`
  事件决定带凭据（配置 headers，如 Authorization）的 POST 目标，所以必须是 SSE URL 同源——否则被攻破的
  server 可把认证 POST 重定向到攻击者/内网（SSRF）。**检查与发送共用同一个解析器**：`resolve_endpoint`
  用 `reqwest::Url::parse + join` 解析（即 reqwest 实际 fetch 用的 WHATWG `url` 规则）、比较
  `abs.origin() == base.origin()`，返回的 `Url` 原样存进 `SseTransport::post_url` 再发——「检查什么就发
  什么」。外部评审 P1 曾绕过手写字符串版 origin 切分：`http://evil\@trusted:3000/x` 按纯文本看是
  userinfo+`trusted:3000`（同源放行），但 WHATWG 把 http/https 里的 `\` 当 `/`，实际发给 `evil`、
  路径 `/@trusted:3000/x`——任何手写 origin 切分（`find('/')`、rsplit('@')、默认端口剥除……）都可能
  与真解析器分歧（IDNA、百分号编码 host、userinfo 边角），别再写字符串版；回归测试钉住反斜杠混淆
  与网络路径变体（`endpoint_resolution_rejects_parser_confusion_bypasses`）。
- **子 agent（`task` 工具，`tools/subagent.rs`）**：主 agent 把聚焦子任务交给一个全新子 agent
  （独立历史 + 独立 turn 预算，`limits.subagent_max_turns` 封顶——默认 25，`0` = 不另设天花板），
  只把最终摘要作 tool 结果返回。
  **同 turn 多个 `task` 调用并行**：`agent.rs` 的工具执行分两遍——叶子工具（shell/read_file/…）
  顺序跑，并行能力调用收集后用 `std::thread::scope` fan-out（每个一个 scoped thread），结果按原块序
  拼回（tool_use/tool_result 仍 N:N）。**路由按能力标记、不按名字**：分流依据是
  `Tool::concurrency() == ToolConcurrency::Parallel`（`SubAgent` 覆写为 Parallel），loop 里没有任何
  `"task"` 字面量分流——第三方工具声明 Parallel 即获同款 fan-out（义务见「容易踩的坑」）。子 agent 的
  token 用量经 `ToolOutput::usage` 折进主 run 累计（失败也照计已完成轮次）。事件实时转发，裹成
  `AgentEvent::SubAgent{name}` 区分并行的几个。**fan-out 有波次上限与有界漏斗**（2026-09 外审 P2）：
  runnable 按 `MAX_PARALLEL_TOOL_WORKERS=8` 分波，波内才保证"全 spawn 后再 join"；每波事件走
  `sync_channel(256)` 有界转发——快子 agent 洪峰时 worker 的 emit 会**阻塞**（背压）而不是把父进程
  内存撑爆，父线程同一 scope 内专职 drain，不会死锁。审批门（gate-before-spawn）逐调用复查 cancel、
  spawn 前再查一次，gate 窗口内落下的 cancel 让整批不启动（槽位保持 Cancelled 合成）。
  **波次间与 worker 预检**（2026-09 外审第三轮 P2）：每波开波前复查 cancel（上一波里落下的 cancel
  不再启动下一波）；worker 在线程里、进工具体前再查一次——已批准未启动的调用返回 None，join 侧补成
  顺序检查点同款 `[cancelled by user]` 错误槽 + `after_tool(executed=false)`。**panic 防死锁**：通道在
  scope 闭包**内**创建（`rx` 是闭包局部量）——宿主事件回调 panic（panic=unwind 宿主）时 drain 循环
  unwind 即销毁 `rx`，worker 的 send 开始失败、scope 的 join-all 得以完成；若 `rx` 借自闭包外则
  worker 永远堵在满漏斗上、join 永远等它——死锁。另有 `CancelOnUnwind` 守卫在 unwind 时翻共享 cancel
  让在飞 worker 快速收尾，而不是把注定作废的一波跑到完。**事件配平**：已 announce（gate 前全量宣告）
  后因 gate/波次 cancel 未运行的调用，装配段补发 `[cancelled by user]` 的 ToolResult 事件——消费者
  不会看到无闭合的 ToolCall；从未 announce 的槽位（内联短路、整批被闸掉）保持事件静默。
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
  被取消的不进）；**Deny 文案过 `tools::truncate`（ctx.max_output_chars）**（2026-09 外审）：宿主拒绝
  理由是模型可见 is_error 文本、可任意长，与 run_tool 对 MCP 错误串同款预算（三处 Deny 点：顺序
  gate / 并行 gate / run_code 内层）；Modify 只换**执行**输入，审计链（records/`ToolCall` 事件）永远保留模型原始输入；
  **gate→执行检查点**（2026-09 外审 P2）：`before_tool` 可以无限期挂起（审批 UI 等人），等待期落下的
  cancel 若不复查就会出现"run 报 cancelled、被批准的工具却照跑"——`ctx_cancelled()` 在 gate 返回后、
  执行前复查（`execute_tool_call` / 并行批 spawn 前 / run_code 内层三处共用），顺序路径产出
  `[cancelled by user]` 错误槽且 `after_tool` 以 `executed=false` 收尾（与 Deny 同形）；并行路径
  同款统一（2026-09 复审 P3 修复）：已过 gate 但因 pre-spawn/波间检查点未启动的调用由占位循环转成
  `Slot::Ran` 错误槽——进 records 且 `after_tool(executed=false)`，**过的 gate 不再留下未配对
  before**；只有 gate 循环本身被 cancel 打断、从未过闸的调用保持 `Slot::Cancelled`（无 before 可配）。run_code 内层复用入口闸同款墓碑文案（`CANCEL_HALTED_WHY`，
  kind `cancel`，附录 A v0.5 记录新增触发点）。
  顶层闸门在父线程 spawn 前**串行**把关，子 agent 自身闸门跑在各自 worker 线程（兄弟真并发，实现方
  须自行线程安全——`Send + Sync` 只保证传对象内存安全）；压缩摘要请求**不**透出；`--resume` 重建的
  agent 需重挂（层活在实例上、不进 session 文件，`session_save_load_does_not_carry_hooks` 钉住）。
  **`after_tool` 带 `ctx: &ToolCtx`**（2026-09 外审，趁绑定未透出 hooks 的窗口期落的破坏性改动）：
  与 `before_tool` 的 ctx 镜像对称——审计点靠 `ctx.subagent` 归因来源（顶层/task/沙箱），并行兄弟
  真并发下调用序关联不可靠，per-call ctx 是唯一可靠的来源标签。本期绑定不透出 hooks（backlog P3，
  两绑定 crate 头已明示该范围决策）。
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
  发现资源。**内置（embedded）skill（v2 P2）**：`SkillEntry.source` 是 `SkillSource::{Dir, Embedded(String)}`
  枚举——`skills::embedded_entry(text)` 按同款 frontmatter 校验建条目；`read_skill` 对 Embedded 直接吐全文
  （读非 SKILL.md 的 file 报"无附属文件"）、`list_skill` 列单文件；`assemble` 在 `code.enabled` 时把
  run_code 的用法指南（`src/tools/run_code_skill.md`，`include_str!`）经 `insert_builtin` 注入——**磁盘同名
  skill 覆盖内置**（insert_builtin 撞名即 no-op、保持名字序），allowed-tools 白名单告警检查对 `run_code`
  在 enabled 时放行（它的 membership 由 code.enabled 驱动、不在 cfg.tools 白名单里）。
  `read_skill`/`list_skill` 都**不在 `cfg.tools` 白名单**（skill≠tool，刻意决策）——扫到技能时由 `assemble` 自动配套注入
  （`tool_list.push(ReadSkill)` + `push(ListSkill)`，各 `index.clone()`）。**子 agent 不继承**：`subagent.rs:133` 硬编码
  system_prompt 覆盖为 `SUBAGENT_SYSTEM_PROMPT`，且子 agent 走白名单建工具集，故既无简介也无 `read_skill`/`list_skill`
  （决策，不改 subagent.rs）。坏 skill（缺 SKILL.md / 缺 frontmatter 字段 / name 不合规 / name≠目录 / description 超长）
  跳过 + stderr `[skill]` 告警，**不 fatal**。**`allowed-tools` 仅告警不强制**（标准标 Experimental；TinyAgent 无 per-turn 激活层，
  真"临时预批准"不做）：`assemble` 遍历 `e.allowed_tools`，不在 `cfg.tools` ∪ {`read_skill`,`list_skill`} 的 → stderr 告警；
  字面量名匹配，不支持 glob/`Tool(spec)`。`assemble` 为注入简介 **clone cfg**（改 `cfg2.system_prompt`）并返回
  `(tool_list, ctx, cfg, CodeInstall, Option<Vec<Message>>)` 五元组（带 `#[allow(clippy::type_complexity)]`；
  后两项 = run_code 后置安装包 + eager 注入的合成历史对，微调支持 M2/M3 起）——`Agent::new` 从该 clone 读
  system_prompt；改 `assemble` 签名的人注意两个调用点（`build_agent_with` / `build_agent_with_provider`）
  都要拿返回的 cfg 传给 `Agent::new`。
- **run_code 沙箱 JS 工具（`tools/run_code.rs`，feature `code`；设计/需求文档 `docs/run_code_design.md` / `docs/run_code_requirements.md`）**：
  LLM 写 JS 在进程内 wasm 沙箱执行、经 `tools.call()` 编排既有工具。QuickJS-ng 编译成 wasm（`assets/quickjs.wasm`
  `include_bytes!` 嵌入；重编走 `examples/build-quickjs.sh` + `examples/quickjs_glue.c`——**改 glue 源码必须重编资产**，
  两者保持 sha256 奇偶；宿主纯 Rust 零 C，musl 交叉不碰 C 工具链），wasmtime 36 驱动、纯同步。**两层开关默认全关**：
  cargo feature（体积闸，musl release +3.6MiB）+ `code.enabled`（运行期）。`run_code` **不在 `tools` 白名单语义内**——
  membership 由 `code.enabled` 驱动：`assemble` 把名字从白名单 strip 掉、**总是构造唯一实例**（H3：经 `CodeInstall` +
  `Agent::set_code_tool` 后置安装，`try_new` 签名 feature 无关；两个 `build_agent_with*` 调用点零变化）。热更新面：
  `Agent::set_code_config`（整包原子替换）/ `set_code_enabled`——载体 `Arc<RwLock<CodeConfig>>` 与工具实例共享，
  **只达 Agent 与 RunCodeTool 两处、不外递 Arc**（绑定走 with_agent/try_lock 锁内调 Agent 方法、dict 按值转）。
  enabled 翻转同步工具表（push/remove + `refresh_tool_defs`，MCP 热增删同款重建路径）；开关态不进 session 文件
  （hooks 同款"层活在实例上"口径）。
  **wasmtime 36 中断事实（M3 实测钉死，改墙钟/cancel/fuel 必读）**：① epoch 中断的**投递载体是 epoch-deadline 回调返回
  `Err`**（`UpdateDeadline` 无 trap 变体；`set_fuel(0)` 是死路——运行中函数把 fuel 缓存在 wasm local、check 点才回写，
  实测 guest 跑过中断点 25s）；② epoch 检查只在 **wasm 循环头**发射、函数入口只初始化缓存 deadline——warm 平凡程序
  在单个 10ms tick 内跑完，故 cancel/deadline 必须**入口门⓪**（guest 启动前直接拒，fuel_used=0）+ epoch 回调 +
  `host_call_tool` 入口 + 收尾裁决（含 `_initialize`/编译失败路径，quickjs stdlib init debug 下 >1s）**四点**。
  `host_call_tool` 入口自 2026-09 审核 #7 起**还查 cancel**：已置位即拒纳新调用并 latch `Interrupt::Cancel`
  （"in-flight 跑完"承诺的是已在跑的调用，不含新纳——`e.kind` 词表随之增量加 `cancel`，走附录 A 修订）；
  **外部复审 P1 补丁：cancel 臂必须与 deadline 臂同构同时 latch `halted`**——入口拒绝对 guest 是
  JS **可捕获**异常，旧代码只 latch `interrupt`、重试再入时 `interrupt.is_none()` 守卫直接跳过 cancel
  检查、被捕获后重试的调用真跑了内层工具（直到 ~10ms epoch trap）；latch halted 后停机检查拒绝一切
  后续调用（epoch 路径投递的是不可捕获 wasm trap、guest 已死，无此洞）；
  ③ `max_wasm_stack` **384 KiB**（非 1.5 MiB）——开 epoch 插桩后深递归会先损坏 quickjs 无检查的内部递归状态
  （终态是不可归因的 OOB trap），384 KiB 让原生栈上限决定性赢得竞速、稳定归因 StackOverflow；④ wasmtime 按模块缓存
  机器码——冷首次执行 ~10s 懒编译（debug），**测试必须先 warm 模块再计时/再 spawn cancel 翻转线程**（翻转线程先于
  warmup spawn 会让入口门吃掉用例而不是在飞投递）。
  **结果装配（D3）**：`assemble_with_logs(value, logs, cap)`——值优先整行适配，只有值**单独**超限才截断值、日志整段丢；
  `truncated` 审计键稀疏；`logs_dropped` 只计**超宿主日志上限未收集**的行。宿主日志上限按 `chars + 1`（**0 字符行也计费**——
  chars-only 检查放过 `while(1) console.log()`，sec 评审 M-1）。**wall_deadline 一律 `checked_add().flatten()`**
  （u64::MAX 级 timeout_secs 经 batch TightenOrInit 0-基底真实可达；溢出=无墙钟，cancel/fuel/mem 仍生效）；
  `WallDeadline{from_cfg}` 记录约束侧，`deadline_why` 据此点名旋钮。内部调用走**与外层同款 hooks 闸门**
  （`__code__` 身份标记，顶层串行同序）；子 agent **不继承** run_code（`available_to_subagent` false）。
  usage 经 `compress::merge_usage` 折进自身 ToolOutput.usage（子 agent/task 同款纪律：工具账单不进预算计数器）；
  审计 `meta` 双哈希（fnv64 + sha256 截 128bit，in_hash=模型原始输入 / in_hash_eff=Modify 后实发）。
  事件面：`AgentEvent::CodeLog(String)`（console 行实时透出，`--json` 吐 `{"type":"code_log"}`、plain 走 stderr、
  TUI 提交 `Row::sys_info` 转录行——设计 L6：可回看可 `/export`，瞬态会丢执行日志）。数值写端照库纪律 saturating。
  **guest 内调工具的输出预算不继承 `ctx.max_output_chars`**（`guest_ctx` 用 `GUEST_OUTPUT_CHARS`=2M，设计 D13）：
  该帽是给模型眼睛的展示预算，guest 输出落 JS 堆（`code.mem_mb` 限）、根本不进对话——继承会让设计自己的
  `LIMIT 5000` 式整读不可达，真实旅程 A/B 实测模型退回 17 次手动翻页、烧穿 300k run 级预算
  （主要烧在逐页膨胀的 cache_read 上）。字符单位不变（truncate/read_file 字节窗照算），只换量级。
- **guest I/O 结构化 + prelude + 磁盘缓存（v2 P1，设计 D16/D17）**：① read_file 头部规模元数据 +
  `format:"json"`（`{content, lines_total, chars_total, truncated, next_offset}`，无行号前缀；**只有 limit
  截断给 next_offset**——chars/bytes 截断可能切行中、线对齐续读不可能，置 null 让调用方转 raw）+
  `raw:true` 字节窗 base64（offset/limit **按字节**，窗帽 `min(RAW_READ_MAX=1.4MB, ctx.max_output_chars/2)`
  ——主 loop 展示预算拉不走数据规模块，guest 2M 预算吃满窗）+ write_file `content_b64`（字节保真写，
  开放问题 1 的解）。② **prelude**（`PRELUDE_SRC` → `prelude_line()` 展平成**单物理行**注入 guest 代码前——
  quickjs 行号相对 eval 源，多行 prelude 会把用户栈帧行号整体偏移；展平后恒 +1，不变式测试钉住）：
  `readAll`/`readLines`/`writeVerified` 三件、清单**封闭**（每个新助手都是「模型以为有其实没有」的新幻觉面），
  描述（设计附录 A 即规格）点名三者，改 prelude 必须同步附录 A 与描述。`eval_code` 每次调用新建 JS
  runtime——prelude 不能分两次 eval、只能拼进同段（函数体内 var/function 声明，用户代码可遮蔽）。
  prelude 还挂 `tools.budget = () => tools.call("__budget", {})`（v2：保留名直通 host，免额度/免 hooks/
  免审计，与 `__tools_list` 同类；返回剩余调用/墙钟/燃料/run token——高循环波「预算烧穿」实证的解。
  v0.7 起 `tokens_left` 经 `ToolCtx::budget: Option<BudgetGate>`（used 计数器 + cap 打包，lib.rs 门面
  re-export）读出：gate 侧 `cap.saturating_sub(used)`，无闸门（max_total_tokens=0）为 null；guest ctx
  clone 的是 gate（used 是 Arc），读数是活的。改 `bill_chat`/run 入口安装/闸门读三处的人注意字段
  从裸计数器换成了 `g.used`）。
  **guest 注册表滤除用独立标记 `Tool::available_to_code()`（默认 false）**，不再搭
  `available_to_subagent()` 便车（两条退出面解耦）；文件四件套/shell/`McpTool` 显式 true——
  **McpTool 漏覆写 = AppWorld 机制整体断**。宿主 extra_tools 经 `SharedTool`（Arc 同实例）进注册表、
  `CallbackTool::with_code_access()` 逐工具授权；guest-only 原语 `copy_bytes`（host 流式字节保真复制，
  策略面在拦截层 copy 臂：src 读规则/dst 写规则）注册在 retain 之后。**拦截层的符号链接走查按原始
  拼写逐组件进行**（外部复审 P1：内核在 write 时才解析、`..` 只在解析完前面的组件后生效——
  `link/../x` 的真实写位置经 `link` 逃出 workdir；旧实现先词法折叠 `..` 把 `link` 从走查里抹掉，
  策略层看到干净路径。现行两段式：词法折叠只做"折叠后在 workdir 外"的快速筛，随后按原始拼写
  Normal 推进即查 symlink、ParentDir 弹栈——无 symlink 组件时词法形 == 解析形；工具最终写的就是
  原始拼写，检查与写入才用同一解析结果）。**绝对拼写从文件系统根锚定走查**（第二轮 P1：旧实现
  对原始拼写 strip_prefix(workdir)，字面前缀不同的 `/outside/../<workdir>/link/../x` 折叠回 workdir
  内却整个跳过走查）；**workdir_relative 对不存在目标的回退也不再是纯词法折叠**——canonicalize
  最深已存在祖先（原始拼写，经 symlink 与 `..` 的内核真实解析）再词法拼接余下组件
  （`resolve_via_existing_ancestor`）；祖先链在 `..` 前断（`missing/../file`）时才退回词法折叠，
  该拼写由工具层自己回答（ENOENT 或建父目录），绝不变成策略拒答。
  错误封套带结构化字段
  `kind/tool/retryable`（glue 附到 Error 上；`tool` 类刻意不给 retryable——host 无法机判；
  **args 解析失败也是 `host` 类结构化错误而非静默 `{}`**——`parse_guest_args`，v0.8，对称于坏指针；
  shipped glue 恒发合法 JSON，该路径仅限绕过 JS 包装直戳 import 的 guest，模型可见词汇表不变），
  glue 随之重建（sha256 `3c1c8790…`，build 脚本头部有奇偶记录）。
  ③ **wasmtime 磁盘编译缓存**：Cargo `cache` feature + 显式 `CacheConfig::with_directory`（手写 XDG：
  `$XDG_CACHE_HOME`（绝对）else `$HOME/.cache`，下挂 `tinyagent/wasmtime`）——**别**换回 `load_default`
  （musl/无 HOME 行为不可控）；目录失败**优雅降级为无缓存**，绝不阻塞沙箱。A/B 墙钟里 debug 每进程 ~10s
  懒编译从此只付一次（改此路径记得 musl 冒烟在 CI 覆盖）。
- **描述瘦身 + 内置 skill（v2 P2，设计 D18）**：run_code 描述只留身份/环境/**封闭助手清单**/返回纪律/
  技能指针（≤1200 字符，`slim_description_*` 测试钉上限；含【数值】段——Number 是 float64、
  超 2^53 静默失真、移植 Python 整数算术一律 BigInt 字面量，bench scriptout 实证 float64 直译答 816≠1048），深契约（read_file 三模式、字节保真复制、
  示例）移进内置 skill `run-code`（见 skills 条目的 embedded 机制）。**描述、skill 正文、设计附录 A 三处
  同源同步**——附录 A 由源码程序化再生成，改 prelude/助手清单/read_file 契约时三处一起动。附修：read_file
  的 raw **默认**窗口按 3 字节对齐（**显式 limit 照传不夺**——调用方点名要那几个字节；只有默认窗口对齐），
  保证跟随 next_offset 分页时各窗 `content_b64` 字符串拼接 == 整个文件的 base64
  （`read_file_raw_windows_concatenate_losslessly` 测试钉住）；跨窗字节保真复制配方见 skill 正文。
  另一 fidelity 修复（A/B 首跑抓到）：json content **逐行保留原始行终止符**（含每页末行 \n 与 CRLF，
  不再「strip 后 join("\n")」——那会丢末行换行、归一 CRLF，按换行计数每文件少 1）；prelude
  readAll 页间改 `join("")`（页自带终止符，join("\n") 会在页界注入空行）。
- **run_code 触发文案 v1/v2**：描述/teaser/技能入口按“规则明确 AND 剩余有规模”引导模型，探查后可换轨、仅续做未完成项；无 host 计数触发。`examples/prompts/agentic-code.md` 独立变体允许大清单存变量/文件，基线 `agentic.md` 不变。`scripts/sync_run_code_contract.py --write/--check` 同步设计附录与评测指纹；v2 在技能前置 JSON 信封解析与成本纪律，系统提示词/teaser 不变；效果与版本差异见 `docs/run_code_routing_v2.md`。
- **run_code 微调支持（M1-M4，设计/需求 `docs/finetune_support_design.md` / `docs/finetune_support_requirements.md`）**：
  两个 CodeConfig 新字段服务 MiniCPM 契约内化微调——`skill_injection: SkillInjection` 三态
  （`Off < OnDemand < Eager`，**声明序=披露序**，derive Ord；默认 OnDemand）与 `precheck: bool`
  （默认 false，NFR-1 显式配置才生效——默认开会漂掉 `poisons_date_and_random` 运行时桩用例）。
  **两字段生效时点不对称（FD-7）**：`skill_injection` **装配期**消费（assemble 三分支：off 档给
  ReadSkill 塞 tombstone `disabled`——仅当磁盘无同名覆盖；eager 档经共享的
  `tools::serve_skill_body(entry, cap)` 取正文、合成 `read_skill("run-code")` 的 tool_use/tool_result
  N:N 对预置进 initial（固定 id `eager-run-code`，id 由 `assign_missing_ids` ingest 点分配；**仅
  initial 为空时注入**，resume 不重注）——`set_code_config` 热更**不会**重新注入，对齐「宿主热更新面
  不传播给已构造快照」先例）；`precheck` **调用期**经 `snapshot()` 生效（与资源限制字段同先例，运行期
  调参可用）。**逐字节一致（FD-4）由构造保证**：eager 正文与 ReadSkill 直出共用同一个自由函数
  （`src/tools/skill.rs::serve_skill_body`），不是两处拷贝的约定。**审计锚点（NFR-3）**：`Agent` 带
  `code_audit: Option<(SkillInjection, bool)>`——**装配时**经 `CodeInstall` 捕获（`set_code_audit`
  先于 `set_code_tool`），不是运行末活读（活读会在运行期 `set_code_config` 后谎报装配事实）；
  result.json 增 `"code"` 键（success/failure 单点插入，`plan` 先例）+ NDJSON `--json` 首行
  `{"type":"code",...}`——**仅 `code.enabled` 时出现**，默认配置输出逐字节不动。**precheck lexer
  （FD-10）**：单遍扫描跳过字符串/模板串（`${}` 内代码照常扫）/注释（regex 走表达式位置启发式）；
  环境语法（`async`/`await`/`import` 关键词直接报；`Promise`/`setTimeout`/`fetch` 仅作调用/构造/
  成员目标且全文无 `function/const/let/var/class` 同名声明时报）→ 确定性（`Date.now`/`Math.random`
  成员表达式，无豁免）→ 保留工具（`tools.call` 首参字符串字面量 ∈ `{run_code, task, todo}`）；
  拒答 = `ToolOutput::error`（hooks-Deny 先例：`Slot::Ran(error)`、进 records、不执行、不占预算、
  registry/wasm 零接触——「registry 必坏实例仍吐预检文案」测试是拒答先于 eval 的构造性证明）。
  **拒答/墓碑文案即规格**：逐字照抄设计稿附录 A，改文案先走其修订记录（微调数据有依赖）。
  batch 收紧语义：两字段都 `Clamp::Tighten`（枚举取 min、bool 取 false<true——片段不得扩大披露、
  不得开启预检）。
- **read_file 分页窗口**（`tools/file.rs`）：offset 跳过**不花**字节预算——Phase 1 用 fill_buf/consume 按块数
  换行扫到目标行（有界内存），Phase 2 才 `take(max_output_chars*4+1024)` 读窗口；旧从文件起算 take，令
  ~4×max_output_chars 字节后的每一页都不可达（A/B 实测 900 行文件 573 行后翻不动、边界行还被静默切断——
  修后字节窗从窗口起点计，切断必伴随字符帽 truncate 标记，不再有静默半行）。行号是 `offset+i` 全局号。
  **输出头部前置规模元数据**（v2 P1）：非空文本输出首行是
  `[文件名: 总行数 · 总字符 · complete|行区间 a-b]`——「装不进对话」的换臂决策信息前移到首次读取
  （totals 扫描 `TOTALS_SCAN_MAX`=8MiB 封顶、超出报 `N+` 下界，别为 1GB 日志每页全量扫）；空文件与
  offset 越界保持旧形状。另有 `format:"json"`（结构化分页字段）与 `raw:true`（字节窗 base64，
  offset/limit **按字节**）两模式，guest prelude 依赖之——见 run_code 条目。
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
  **batch 终态落盘单出口**（`batch::write_terminal_result`，外部评审 P2-5）：不经 `run_one_attempt`
  正常完成的终态（退避中取消、bad_task/setup_error 早退、未启动即取消、worker panic）**也必须写
  `results/<name>.json`**——否则 report.json 说 cancelled 而 results 里留着上一次尝试（或上一轮运行）
  的 error 记录，读 per-task 文件的下游被误导。新增终态路径一律走这个出口，别只改内存 TaskReport。
  **早退 `early` 一律继承 `acc`/`attempt_no`**（外部复审 P2：重试轮的 agent 重建失败——env 中途消失、
  MCP 起不来——曾把零成本 setup_error 写进 results/<name>.json，抹掉第 1 轮真实烧掉的 token/工具
  调用/尝试数；第 1 轮到达此处时 `acc` 天然全零，无条件继承两种情形都对；model 归因在 attempt>1
  时走 `partial.model` 派生，与退避取消路径同款）。
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
- **上下文构成分解（`context_report.rs` + `Agent::context_breakdown()`，拉取式只读查询）**：把下一次
  请求的上下文按来源拆出 token **估算**量——system 三段（env 头/用户 base/skill 简介）、工具 schema
  四类（`Tool::origin()` 能力标记：Builtin/Mcp/Skill/Custom，默认 Custom、SharedTool 委托 inner，类内
  逐工具 name+desc+schema JSON）、消息历史按 block 分型（user_text/assistant_text/tool_use/tool_result/
  image/document/skill_content/other；**没有 thinking 桶**——思考是纯事件流永不进 history，恒 0 字段是
  撒谎式 API；skill_content 按 tool 名 == `read_skill` 归因，ToolResult 只有 tool_use_id，先扫 ToolUse 建
  id→name 映射（同 compress `todo_use_ids` 模式），eager 注入对与运行期真实调用一并归入）。**口径纪律**：
  一切数字是启发式估算（`compress::estimate_tokens` 同一套 yardstick：chars/4 + 图片 len/100 + PDF 页
  ×3000），**不是 provider 实测——记账权威永远是 Usage 事件/`usage()`；估算与实测刻意不混流**，所以走
  拉取式查询而非事件。**等和不变量**：未校准时
  `system.tokens+tools.tokens+messages.tokens == total_tokens == estimate_tokens(全量)` 严格成立，靠**单趟
  running-floor 分摊**（对全部 chars 段按固定顺序一遍：每段领 `floor(acc_after/4)-floor(acc_before/4)`；
  各段独立 /4 会因舍入差 1-2——`uncalibrated_total_equals_estimate_tokens` 测试是刻意钉子）；共享计费
  原语 `compress::block_chars`/`block_pdf_tokens` 是 total_chars/estimate_tokens 与分解共用的单点
  （新 Block 变体在此编译期强制决策，context_report 的分型 match 同样穷尽、`other` 是显式兜底目的地
  而非 wildcard）。**校准**：`UsageAnchor` 加 `scale: f64`（锚点写点顺手算
  `raw=estimate_tokens(同前缀)`，`scale=prompt/raw`，raw==0→1.0；**输入三轴全零的 usage 不写锚点**——
  零 prompt 对真实请求不可能，照写会把 scale 钉 0 成"已校准的全零"，详录见 pitfalls），`live_anchor()`
  （estimate_input_tokens
  四条件校验的抽取复用）有效时全分量 ×scale 并透出 `Calibration{measured_prompt_tokens, scale}`——
  **均匀缩放本身是启发式**（假设估算误差在各成分间比例相近，CJK 低估类成立）；prune/fold/热增删/
  前缀改写使锚点失效 → calibration 自动回 None。**归因流**：env 头长度在 `try_new` 内（`system_with_env`
  单点）记录；skill 简介边界只有 assemble 知道（render_intro 拼进 cfg 克隆），经
  `pub(crate) set_skill_intro_base` 由 build_agent\* 在**装配窗口**（try_new 后、返回前，无并发）收窄；
  **`skill_intro_base_chars` 默认 = cfg.system_prompt.len()（"env 之后全是 base"）**——默认 0 会把直接
  try_new 用户的 system prompt 全错归 skill_intro；直接构造/子 agent（不继承 skill）默认归因天然正确。
  绑定照 `usage()` 模式透出（Python `context_breakdown()` pythonize dict / Node native `contextBreakdown()`
  serde Value + wrapper.d.ts 同步两件套纪律）；TUI `/context` 是第一个消费端（worker channel 模式，
  busy 拒绝与 /clear 同款——worker 在 run 里不泵 in_rx）。
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

