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
  （`AgentEvent` serde 外部 tag → `{"ThinkingDelta":"..."}`，TinyWorker 后端零改动接到）。**思考增量只透出不入历史**
（能进历史的只有下一条「推理回传」的旁字段，且默认不进）——
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
  plain headless 刻意安静（完整 ToolCall 行随后来）；TUI 把它作为 render-only 的 `receiving arguments`
  活动提示，并关闭旧 Assistant/Thinking live 流让提示可见，JSON 增量本身不进 transcript。mock 的 `OkToolStream` 变体
  驱动 loop 级测试（分片→事件→拼装）。新增流式透出项照这套纪律抄。
- **推理回传 + 推理 token 账单（内核改进计划 1.1 / 1.4；`Config.preserved_reasoning`，2026-11-26 起默认开）**：上一条「只透出不入历史」
  现在要补半句——**增量**永远不进历史，能进历史的只有 provider 显式回传的推理状态（默认落历史，显式
  `false` 才不落）。形状刻意是**旁字段不是
  Block 变体**：`ChatResponse.reasoning: Option<Reasoning>`（`Reasoning { provider, blocks: Vec<ReasoningBlock> }`，块
  `{ before, text, opaque }`——`before` 是它原先在 `content` 里的位置下标，Anthropic interleaved thinking 要求原位回显；
  `opaque` 是厂商私有伴生数据，Anthropic 的 `signature` / redacted `data`，OpenAI 兼容端与手工裁剪过的旧记录恒为 `{}`、永不
  `null`），`Message.reasoning` 同形。**为什么不进 `blocks`**：所有「块形状」规则（空回复不落盘、仲裁的零块判定、salvage
  探测、子代理 `final_text`、TUI 转录）原样成立，渲染层永远看不到它。**两层决定**：① provider **无条件**产出（内置两家
  `ChatResponse.reasoning` 恒填；自定义 provider `from_simple` 合成的恒 `None`）；② Agent 按 `self.preserved_reasoning`
  决定是否落历史——唯一落点在 push assistant 之前那一处 `if self.preserved_reasoning { assistant.reasoning =
  resp.reasoning.take() }`，关着时就在这里丢弃，历史与没有这个字段时逐字节相同；`try_new` 另把 initial 历史里的剥掉
  （旧 session 文件带了 reasoning 而配置已关，也不会漏出去）。**默认值（D1，2026-11-26 用户拍板「默认开」）**：先按
  opt-in 落地；`reasoning-carry` 配方在 deepseek-flash 上的 A/B 达标（成功率不降，on 15/15 对 off 14/15；总 token
  −39%，收益全部来自会压缩的 `long-output` −48%，不压缩的四个短任务多花约 21%——保留的推理在同轮每个后续请求里
  重发）之后改成默认开。只此一家模型、每格 3 次。**修后复测（同日晚）**：方向随端点变——OpenAI 协议 −31%，Anthropic 协议
  端点（`hw-vol-deepseek`）+26%，差别全在 `long-output`，短任务 ±50% 是噪声；默认值未改，见计划文档 §0.0.3 的 D1。
  Anthropic 分支已在那个端点上逐请求抓包核对（30 run / 328 请求）：回传的 thinking 块与网关发来的逐字节一致，含
  工具调用轮里最常见的「空正文 + 签名」块（`empty_signed_thinking_between_text_and_tool_use_is_echoed_in_place`
  钉住）；该网关不校验签名，官方 API 的严格校验仍未验证（抓包核对的做法见 pitfalls）。改默认的连带：`Config::fingerprint` 里
  `preserved_reasoning` 只有显式 `false` 才进哈希（非默认那一侧反转为关）；`reasoning-carry` 配方的两臂都写显式值，
  不受影响。**寿命 = 当前轮**，失效点与保留点如下，规则是「整段剥、
  永不剥一半」——只剥一部分会留下「早期 assistant 带 reasoning、后期不带」的混合形状，正是严格 API 拒收的那种
  （`compress::strip_reasoning` 的文档就是这条规则）：剥的三处 = `push_user` 开新 user 轮、`set_model`、
  `normalize_adjacent` 合并相邻同角色消息（合并后的消息 `reasoning = None`——两轮的状态拼不成一份：位置相对旧块、签名只覆盖
  一轮）；**不清**的两处 = 工具结果并入尾部 user 消息（合并分支的另一半，当前工具循环的状态因此存活）、压缩折叠与工具结果
  裁剪（幸存消息保留自己的：块的签名自包含，fold 整体移走、prune 只改 tool-result 文本）。会话归档（`archive_messages`）
  永不携带：离开活历史即轮次已尽，且思考文本是没有审计价值的草稿纸。provider 只回显**自己家族**的块
  （`Message::reasoning_for(REASONING_ANTHROPIC | REASONING_OPENAI)`，跨厂商旧状态跳过不发）；Anthropic 未签名的 thinking 块
  不回显；OpenAI 兼容侧按 `reasoning_content` 回显，对端 400 拒收即学 `BodyFix::DropReasoningEcho` →
  `GatewayFixes.no_reasoning_echo`（第四位；`GatewayFixes` 带 `deny_unknown_fields`，所以带它的 preset 旧二进制读不了），
  之后 `message_jsons` 在序列化时剥。请求没带 reasoning 而对端的 400 文本点出了这件事（OpenAI：提到
  `reasoning_content`；Anthropic：同时提到 thinking 与 tool_use/redacted_thinking），错误文本追加
  `REASONING_CARRY_HINT`——只在请求确实没带 reasoning 时才提（回传开着时请求本来就带了，这类 400 另有原因，
  提开关就是误导），它修的那类失败必须点名开关（宿主可见的诊断，不是模型可见文案）。OpenRouter 的
  `delta.reasoning` 仍只是展示投影、不落。子 agent 经 `child_cfg = self.cfg.clone()` 继承开关（`tools/subagent.rs` 钉住）。
  **计量**：`compress::reasoning_units` / `reasoning_chars` 把旁字段计入 `estimate_tokens` 与 `total_chars`（文本与 opaque
  都按 CJK 感知的 `text_units` 算——密文唯一诚实的度量是长度），`context_report` 给它独立的 `reasoning` 桶（等和不变量照旧）。
  **`Usage.reasoning_tokens`**：provider 单独上报的推理份额（OpenAI 兼容 `completion_tokens_details.reasoning_tokens`；
  Anthropic 不单列，恒 0）——是 `output_tokens` 的**子集拆分、不是第五根轴**：`Usage::total()` 刻意不加（同一批 token 记两次
  账），没有任何闸门读它。`RunOutcome.reasoning_tokens` 跨请求求和（`task` 子 agent 花的、重试梯里失败 attempt 花的都在内，
  写端 `saturating_add`）。**出键规则两侧不同**：`Usage` 结构体与 result.json / report.json 都是「0 不出键」（非推理模型的
  输出逐字节不变），NDJSON 的 `usage` 事件则是恒出现的增量键（harness `event_json` 是手写映射）。外部按字面量构造
  `Usage` 的代码会编译失败（破坏性，CHANGELOG 有迁移说明）；绑定侧 `Usage` / `RunOutcome` 自动序列化，所以 1.4 没改绑定
  源码——但两绑定的测试各钉了「一次性结果 + getter 跨轮求和 + 未上报不出键」，因为把 `reasoning_tokens` 塞进「四轴恒在」
  的断言循环会当场失败（它默认不在）。
- **请求整形 `extra_body` / `extra_headers`（内核改进计划 1.3）**：由内置 provider 的构造器承接
  （`Anthropic::with_extra_body(Value)` / `with_extra_headers(ExtraHeaders)`，`OpenAi` 同名同参），`provider::build` /
  `build_agent*` 按配置自动调用——直接构造 provider 的宿主自己调。**校验在加载期**：`Config::validate_extra` 要求
  `extra_body` 是对象且不含内核自有键（`config::EXTRA_BODY_RESERVED`；设了 `reasoning_effort` 时还有
  `EXTRA_BODY_RESERVED_WITH_EFFORT`——`thinking` / `output_config` / `reasoning_effort` 这时归内核管）、`extra_headers`
  不得覆盖内核管理的头（`EXTRA_HEADERS_RESERVED`：鉴权 / content type / 协议版本等），报错**不回显值**（header 值是
  凭据）。provider 内部还有一道静默过滤，兜住手工构造绕过加载校验的情况。**凭据纪律**：`ExtraHeaders` 的 `Debug` 对值
  打码；`Config::fingerprint` 对 `extra_headers` 只哈希头名（`extra_body` 全量进哈希），两个新键都是「非默认才进」——默认
  配置的指纹不变。批量覆盖：HARD（`Config::apply` 拒绝任务片段设置）；`apply_trusted` 照常应用，且**先在副本上**跑 extras
  校验（它要读同一 overlay 可能设置的 `reasoning_effort`），被拒的 overlay 不留半套修改。用途是厂商开关（GLM 的
  `{"thinking": {"type": "enabled"}}`、中转站的路由标签、`anthropic-beta`）——内核不为每家加专门键。
- **模型窗口提示（内核改进计划 1.5）**：`Limits::context_window_hint()` 是纯函数——`max_context_tokens == 0`（默认）时
  返回一行说明（`compress_threshold_chars` 也为 0 时换成更重的一句：没有任何东西在请求前压缩历史），设了窗口则 `None`。
  CLI 在装配**之前**打印一次（headless / batch 走 `harness::note` → stderr，前缀 `[warn]`；TUI 在 `tui::run` 里排成第一条
  diag 行，因为 TUI 看不到 stderr）——纯半边拆成 `context_window_line(cfg)`，测试不必抓 stderr。另一半是**思考烧穿输出
  额度**的提示：某轮 `stop_reason == "max_tokens"` 且本轮有思考增量 → 经 diag 通道发一次（每 run 一次，子 agent 带名字），
  走 diag 而不是事件流——它是给操作者的建议，不是 run 事实。**两个默认值都不改、不是校验**：默认配置合法，溢出恢复仍是兜底
  （改默认值要等 0.4 的评测数据）。
- **PDF 文档块（`Block::Document { media_type, data }`，Anthropic-only）**：对齐 Anthropic `document` content block（base64 source；url/Files-API `file_id` 两种 source 不做——宿主自己下载后推字节）。**四层接入点清单（新增 Block 变体照抄）**：① anthropic `message_json`（`{"type":"document","source":{"type":"base64",…}}`）；② openai `chat()` 入口 `reject_documents` **显式报错**（chat-completions 无 document 内容类型，静默丢弃比报错更糟；`message_jsons` 里的 omission placeholder 文本臂是防御纵深——正常路径不可达）；③ compress 三处（`total_chars` 给 base64-len/100 地板走 legacy char 阈值；`estimate_tokens` 按**页数×`PDF_TOKENS_PER_PAGE`=3000** token-side 加法；`window_lines` 出 `<document>` 占位行）；④ tui `transcript_rows` 显示轻量 `[PDF document attached]` 占位（Image 同理为 `[image attached]`，不再静默跳过）。mock 的 capture 是 `_ => {}` 兜底、agent loop 不穷尽 match Block——新变体不强制点名这些点。**token 预估按页不按字节**：官方口径 1,500–3,000/页（逐页转图+抽文本，官方自己 3 页全视觉样例 ≈2,333/页），社区实测偏低估 → 取上沿 3000（preflight 宁早压，真实 usage 仍是记账权威）。`pdf_page_estimate`（compress.rs）免解析启发式：裸扫 `/Type /Page`/`/Type/Page`（尾字节非字母数字，排掉 `/Pages` 父节点与 `/PageMode` 类长名）与 `/Pages` 节点 ±窗口内 `/Count` 取 max（救 ObjStm 压缩流藏 page 对象的 PDF 1.5+；窗口限定在页树节点附近是防 outline 书签的 `/Count` 混入），双零按 ~100 KiB/页兜底、地板 1——每次 preflight 重解码重扫（10 MiB 封顶 ~ms 级，不值得给 Block 加缓存状态）。**实测对真实产商 PDF 精确**（CUPS 文本管线 3/46 页、Quartz 图像管线 1 页，PDFKit 真值逐一匹配；`tests/fixtures/cups_3p.pdf` 真件 fixture 永久钉住；ObjStm 藏 page 对象的 Word/Acrobat 类输出离线无产商、仅合成用例覆盖——`/Count` 恢复路径在案）。读取 helper `tools/document.rs::load_document_block`（`.pdf` 后缀 + `%PDF-` 头 + 10 MiB take 有界读，镜像 `load_block`；lib.rs 门面 re-export）。**刻意不做 `load_document` 工具**（模型驱动加载是后续可选项；本期 host-push only，经 `push_user`）。
- **核心 loop 在 `agent.rs`**：累计 usage、cancel 轮询、上下文压缩的有状态编排（`compress`/
  `summarize_prefix`/`summary_call`，读写 Agent 字段 + 驱动 provider）在这里。压缩的**纯函数与常量**
  （`estimate_tokens`/`find_safe_cut`/`fold_summary`/`prune_tool_results`/`window_lines`/`chunk_lines`/
  `SUMMARY_*` 等）抽到 `compress.rs`（单向被 agent 依赖；`window_lines` 反向调 `crate::agent::preview`，
  不构成环）。`AgentEvent` 透传给 harness（headless 打印/NDJSON）或 TUI。上下文压缩的摘要请求自身
  usage 也计入累计（诚实成本）。

  **分层漏斗（业界对齐）**：LLM 摘要前先走 `prune_tool_results`
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

  **预检折叠的自适应保留窗（内核改进计划 §0.0.5 发现 2，2026-11-26）**：`Agent::adaptive_keep()` 只被 `compress()`
  的 Preflight 路径调用。配置的 `keep_recent_messages` 窗口自身的占用（`window_usage`：字符闸门用 `total_chars`，
  token 闸门用切片的 `estimate_tokens` × 存活锚点的 scale）超过 `gate_room() / 2` 时，换成 `compress::shrink_keep`
  给出的**能装进一半的最宽尾部**；没有这么宽的就取最窄的可行窗；一个可行窗都没有返回 `None` → 保持配置值。
  `gate_room()` 是 `over_compaction_threshold` 那个判定的预算写法：token 闸门 = `max_context_tokens ×
  COMPACT_UTILIZATION_PCT / 100 −（system + 工具 schema 的估算）− max_tokens`，字符闸门 = `compress_threshold_chars`，
  两个都没设 → `None` → 不缩。**只降不升**：显式 `keep_recent_messages: 0` 不动；配置窗下没有安全切点
  （`find_safe_cut` 为 `None`）时原样返回，调用方照旧报「无可压缩」。Manual 与 Overflow 两条路径不调它，照配置窗口
  走。为什么：保留窗自己压在阈值上时，折完仍在阈值之上，下一轮再折、每次只折掉一两轮，每次一张摘要账单——实测
  一个 56 轮的 run 折了 32 次、27 次只折掉 2 条（三个批次 240 次折叠里 213 次如此），带折叠的轮次中位 16.1 s、
  其余 2.5 s；尾部压到一半空间，另一半留给摘要头与
  之后几轮的增长。缩窗时一行 diag_info，每 run 一次（`adaptive_window_noted`，`run_loop` 入口复位）。
  测试：`agent_tests.rs` 的 `adaptive_keep_only_lowers_the_configured_window` / `heavy_tail_preflight_*` /
  `light_tail_preflight_*` / `manual_compact_and_overflow_recovery_*` / `token_gate_preflight_fold_*` /
  `explicit_keep_recent_zero_*`，`compress.rs` 的 `shrink_keep_*` 五个。

  **`summary_call` 的两段尝试阶梯**（空正文重试来自思考网关自愈、TinyWorker 反馈 #7；截断重试来自内核改进计划
  §0.0.5 发现 1，2026-11-26）：混合思考网关（deepseek/glm）把 reasoning_content 计入 max_tokens，UPDATE 分支（带旧
  摘要）诱导的长推敲可整块烧光 1024 起步预算 → `finish=length` 映射成 stop_reason `"max_tokens"` 且**正文零字**
  ——「截断仍连贯」的接受假设在零正文时为假，空文本随后落退化分支（dropped=0 + Warning）。至多两次请求
  （`for attempt in 0..2`），第二次为两种形状而存在：

  - **空正文**（`max_tokens`、无文本——思考吃光了额度）→ 无条件以翻倍预算重试一次：没有正文就无从折叠，一次请求
    换回一次折叠是便宜的。
  - **截断的非空正文**（`max_tokens`、有文本）→ 仅当这一尝试**带推理**才重试一次：`resp.reasoning` 有块，或
    `usage.reasoning_tokens > 0`（网关只报计数、不流 `reasoning_content` 也算）。正文是被思考挤短的，不是摘要本身
    长。不带推理的截断是真·长摘要，首次尝试已是预算内的最好答案——保持「首次即接受」的旧行为。

  翻倍值 `saturating_mul(2).min(SUMMARY_MAX_BUDGET = 8192)`，**刻意不钳回调用方的轮次上限**（首次已证明 cap 级
  预算必死于思考，钳回等于重放已知必死的请求）。重试只能更好：正常结束 → 用它的正文（并
  `summary_budget.learned(翻倍值)`）；又截断 → 两段正文取长；回来是空的 / 被拒（严格网关对翻倍值的 400）/ 失败 /
  中途断流（非 `end_turn`/`max_tokens` 的 stop_reason）→ 折第一次的正文（`first_truncated`），免得落
  `compress_failed`（它还会关掉之后的溢出恢复）。**取消始终是 `Err`**：重试 `Err` 且 `self.cancelled()`，或
  stop_reason 为 `"cancelled"`——折叠不得落在用户已否决的历史上。重试被拒分两种：确定性错误（非
  `is_transient_error`）→ `summary_budget.ceiling = Some(翻倍值)` + 一行 diag_warn，之后不再要 ≥ 它的额度；瞬时
  错误（5xx、超时）不对预算下结论、不记。零正文的重试失败且手里没有正文 → `Err`（≈旧退化、多一次廉价请求）。
  所有尝试的 usage 经 `merge_usage` 合并返回（诚实成本——烧掉的那次照计费），`CompressError.burned` 同款。

  **`SummaryBudget { floor, ceiling }`**（agent 级、跨 run 保留，`set_model` 清）：`floor` 只在翻倍重试被服务时
  抬高（`learned` 一行 diag_info——首次预算变大会改变摘要的账单），不截断的模型永远不动它；之后每次摘要的预算 =
  `max(比例值, floor).min(本轮 max_tokens)`（`summary_budget_for`，`summarize_prefix` 的单点 / 分块 / 合并三处都过
  它）。**摘录宽度**：`window_lines(messages, cut, preview_chars)` 的第三个参数来自
  `limits.summary_preview_chars`（默认 200 = 原硬编码值；`0` 只留工具名 / 标签；`Clamp::Tighten`——batch 与回归
  任务片段只能调小），裁的是摘要器看到的折叠前缀，主模型读的活历史不受影响。`limits` 多了这个键，而
  `Config::fingerprint` 把整个 `limits` 序列化进哈希，所以所有配置的指纹偏移一次。

  测试：`agent_tests.rs` 的 `summary_cut_short_by_reasoning_*` / `summary_retry_*` / `summary_reasoning_known_only_*` /
  `summary_truncation_without_reasoning_*` / `failed_summary_retry_*` / `refused_summary_retry_*` /
  `a_cancelled_summary_retry_*` / `a_served_retry_*` / `refused_empty_summary_retry_*` / `summary_budget_*` /
  `summary_transcript_excerpts_*`，`compress.rs` 的 `window_lines_excerpts_tool_io_at_the_configured_width`。
  **配套纪律**：
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
  `{"type":"salvage"}`、plain 走 stderr 一行、TUI 渲染一行警示——三宿主各自呈现，不得静默丢弃）。
  Config 软字段默认 true
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

  **隔次重复窗口（内核改进计划 §0.0.5 发现 3，2026-11-26；只提示、不停）**：上面的连续计数有个实测到的空隙——
  `loop-suppression` 里模型发了 9 次一模一样的 `job_status` 轮询（中间夹着别的调用）、最长连续只有 3：第 3 次的 `stuck_nudge` 文案是
  「换一个工具」，模型照办，连续计数被清掉，硬停永远够不着。护栏只数连续是刻意的（不误伤正常轮询），所以另开
  `RepeatWindow`（`run_loop` 持有、随 run 丢弃）：记住最近 `REPEAT_WINDOW`=8 次调用的键与累计次数；
  `observe(key)` 在弹出队首**之前**查最近一次同键——那一次还在窗口里就接着数，已滑出就从 1 重数。**首次返回 1
  而不是 0**：这个数原样写进文案，从 0 数起会让提示晚一次才发、报出的次数少一（新写的用例在落地前抓到的差一，
  改的是产品代码不是断言）。装配段每个 `Slot::Ran` 调用两处都记：连续计数照旧；连续计数 < `STUCK_NUDGE_AT` 且
  `seen >= REPEAT_NUDGE_AT`=4 且偶数 → `repeat_nudge(seen)` 追加到该次 tool_result 的历史块。与连续 nudge
  互斥（`if / else if`）：同一次调用已到连续 3 就只发 `stuck_nudge`，不叠两条。文案只陈述观察到的事实、两条读法
  都点到（在等外部状态 → 多等一会儿或如实告诉用户；以为自己的改动会改变结果 → 回头查为什么），刻意不写
  「最近 N 次里」——计数是跨窗口的累计链。硬停仍然只认连续计数：隔次重复分不清「原地打转」与「改一步验一步」
  （改完再跑测试，没修好就是同参同果），误停比漏停贵。同参不同果即进展、不计数（结果指纹在键里）。
  测试 6 个：`repeat_window_counts_*` / `alternating_identical_calls_*` / `three_occurrences_of_a_key_*` /
  `progressing_polls_*` / `repeat_notice_neither_stacks_*` / `repeat_notice_reaches_every_block_*`。

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
  内层调用在内层 fold 点就地 `share`（混合来源聚合后统一置 true 上报）。**并行 worker panic 的
  usage 回收**：worker 线程在 `bill_chat` / guest `llm` 结算时把已知 usage 记进父装好的线程本地
  `usage_probe`；`join` 返回 Err 时父读出探针并挂到合成的错误 `ToolOutput`（`usage_in_shared_budget:
  true`），`RunOutcome`/`result.json` 总量与共享计数器不再分叉。**cap 单源**（2026-09 外审）：
  turn 边界的比较读 `BudgetGate::cap`（装闸时的 fleet 上限），自身 `limits.max_total_tokens` 快照只能
  把它**收紧**（`min`）——子 agent 快照一旦与父 cap 分叉，比较快照会让 fleet 预算被静默放大
  （`budget_gate_cap_outranks_a_looser_limits_snapshot` 钉住）。闸门超限以 `stop_reason:
  "budget_exceeded"`（exit 125）停；turn 边界语义同 `timeout_secs`——顶破上限的那轮照常跑完
  （工具执行、assistant 入历史），闸门只拒开下一轮，配对不破。batch task 片段只能收紧（更小值），
  0 视为"无上限"被忽略。它和 `max_turns`/`timeout` 是 loop 的三个 turn 边界停止条件。
- **宿主热更新面（run 外的 `&mut self` 方法，GUI 常驻场景）**：`set_model` / `set_reasoning_effort` /
  `set_max_tokens`（钳 ≥1）/ `set_max_context_tokens` 与 `compact()` 都是**纯字段操作/复用 compress 路径**，
  `&mut self` 签名即顺序契约——run 持有借用，「轮次间调用」由借用检查器强制，无需运行期守卫。两个绑定
  均已透出该家族（Python 同步 / Node camelCase + try_lock busy 契约）；Node 的 `Agent.create` 把装配
  （含 MCP spawn+握手）移到 worker 线程，但 napi class registry 是线程本地——实例只能在 JS 线程的
  deferred resolver 里 materialize，wrapper 用 `Object.setPrototypeOf` 接回 onEvent 保护。两条易错语义：
  ① **`task` 子 agent 不跟随**（SubAgent 持构造期 cfg 快照，set_model 后 spawn 的子 agent 仍用旧模型）——文档
  化的边界，别为传播去改 SubAgent 读父字段；② **`compact()` 的成本口径**：摘要 usage 经 `CompressOutcome`
  返回宿主、**绝不进 `self.usage`**（run 入口重置总账，折进去会凭空消失）——与 run 内压缩走 `bill_chat` 进
  run 账单是对偶的两条路径，改 billing 的人别合并它们。`compact` 入口清陈旧 cancel + compress_failed
  （手动调用是新的显式尝试）；内部 `compress(events, deadline, trigger)` 的失败通道归调用方（run 循环发
  Warning 事件、compact 透传 Err——内部发会双报），空摘要退化路径返回 `dropped:0` + usage 不吞账。
  `CompressTrigger::{Preflight,Overflow,Manual}` 非 non_exhaustive（TUI 对**枚举变体**仍穷尽 match——
  编译期点名；`Compressed` 的**结构字段**带 `..`，新字段对 TUI 可选可见），加变体要同步
  harness ×2 + tui ×1 的 match。
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
  `set_shutdown` 则新工具补挂同一 flag（临近退出热增不遗留孤儿）；⑥ **HTTP/SSE 读线程有界回收**
  （2026-10）：standing GET 流与 legacy SSE 的每次阻塞读带 `STREAM_ROUND`（30s）轮次上限——健康流
  按轮静默轮转重连（到期立即重连不退避、真读错照旧告警+退避；超时经 io::Error 内层
  `reqwest::Error::is_timeout()` 判别，`ErrorKind` 恒 Other 不可用）；`shutdown_stream_reader`
  置停标志后有界等待（≤2s）join、超时静默 detach——热删残留的「一线程+一 TCP 连接」从活到进程
  退出收敛为至多一轮自回收。已知取舍：跨轮边界被切断的响应依赖**服务器**在新连接上重发
  （GET 不带 Last-Event-ID，JSON-RPC over SSE 无重发义务）——不重发即丢失、调用方坐满
  CALL_TIMEOUT（最坏是丢到超时，不是延迟一轮）；健康流每 30s 轮转重连对服务器是新增的周期性
  GET/日志负载（旧实现单连接挂到死）。reqwest blocking 无逐读 read_timeout，请求级 `.timeout`
  是 body 总时限。**legacy SSE 的 POST 目标跟随轮转**：`endpoint` 是每连接资源（会话绑定的
  server 在 GET 断开后回收旧 endpoint——python-sdk 是 100ms 定时器轮询，不是断开瞬间），所以每条连接的
  endpoint 事件都重新过同源闸门并刷新共享 `post_url`——冻结在第一条连接上的目标会在首次轮转后 404 掉
  每个调用。**轮转时先退役 POST 目标**（2026-11）：读线程在到期轮转、丢弃旧连接**之前**把 `post_url`
  置 `None`，到新连接的 `endpoint` 才恢复；这期间 `post()`（`post_target`）等新目标而不是打向旧会话——
  否则落在间隙里的 POST 被应 202、应答写进已死连接，调用方坐满整个 CALL_TIMEOUT。等待的界是 connect 键
  （与等首个 endpoint 同一个界），不超过调用方自己的界（更短时按调用方的键措辞）；等的过程中看得见
  cancel（≤`POST_TARGET_POLL`）与 reader 死亡 latch；取消通知不等（不等回执，没有当前目标就丢帧）。
  残留：退役前一刻已取走旧目标的 POST 仍可能打到刚被回收的会话（server 应 404 或应答丢失，以调用自己
  的界收尾）——彻底关掉要「排空在途请求再轮转」的重设计，未做。
  **reader 死亡即 latch 快败**（stdio stdout / legacy SSE 两种传输对称）：reader 的死亡退出
  （首连失败、EOF、真读错、超限行、endpoint 刷新被闸门拒）先写停止原因再 fail-all，之后的
  request/notify 按原因立刻快败——否则服务器仍活时每次后续调用各坐满 CALL_TIMEOUT 变僵尸；
  轮转超时与重连退避不是死亡，不 latch（这是传输死亡，不是 shutdown/cancel，reaper 契约不动）。
  **run_code 的 guest registry 同样
  不感知**（与 ④ 同一条政策，外部评审曾当 P1 报过——实为既定快照语义）：registry 按构造期四件套在
  `Arc<OnceLock>` 里惰性建一次、跨克隆共享，热增的 server JS 看不到；热删的 server 仍可被已建 registry
  调用（连接活到 registry drop；该 server 已不在顶层列表 → shutdown 不再经它武装 reaper，最坏退出时
  等满一次 MCP CALL_TIMEOUT 后由 `StdioTransport::drop` 杀进程组——延迟，无孤儿）。**`code.llm` 的
  开关与模型档同样在 registry 首次构建时钉死**：`set_code_config` 之后改 llm 不再静默成功——
  `RunCodeTool::should_warn_frozen_llm` 对每个新的目标值发一次 diag 警告（普通 limits/路径/开关热更
  保持安静），文案指向「重建 agent」；`pinned_llm` 与 `warned_llm` 都随克隆共享。**活的握手/调用/冲突回滚/进程回收链路**
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
- **MCP 客户端契约（内核改进计划 0.1 翻页 + 2.4 版本/超时/名单/取消/探测/恢复）**：线上行为与默认值在
  双语 README「超时、工具过滤、取消」小节，库侧七条清单在 `docs/lib_api.md`「MCP 客户端契约」。改 `mcp.rs`
  前要记的不变量（都是代码注释里「为什么」的索引，不是复述）：
  ① **界与名成对**：`Timeouts { call, connect, boot: (Duration, &'static str) }`——`boot` 是 `initialize`
  用的界：stdio 默认借调用超时（设了 `connect_timeout_secs` 才换成它），HTTP 两种传输用连接超时。超时文案的
  `(limit: <键>)` 取自界旁边的标签（`timed_out` / `name_the_bound`），不靠事后推断；连接超时能不能被贴上
  `connect_timeout_secs` 标签，取决于 reqwest 是否把该错误标成 `is_connect() && is_timeout()`。配置侧 `0`
  与超 7 天（`MCP_MAX_TIMEOUT_SECS`）在 `McpServerConfig::validate()` 报错；`timeout_secs` 仍对合法值做
  `min`（防御性钳制，不是策略）。
  ② **取消通知只发给「已出门」的请求**：`McpClient::call` 入口与 `probe_if_idle` 之后各查一次预置 cancel
  （已置位 → `MCP call '<m>' cancelled`，不发请求、不通知）；请求发出后 Err 且 cancel 置位 →
  `notify_cancelled(id, "cancelled by the client")`，每种传输都受 `CANCEL_NOTIFY_TIMEOUT`（1 s）约束、失败
  只吞。**超时不发通知**（没有 cancel 就没有通知——超时的调用 server 可能已经做完并回了话）。
  ③ **会话恢复只认一种信号**：`Failure::SessionLost` 仅由 `http_error` 在「404 且请求带了 session」时给出
  （404 到达在 server 执行任何东西之前，所以重发不会重复副作用）；`attempt == 0` 才 `recover_session`
  （diag 一行 → `reset_session()` → `initialize()`）并重发一次，再丢即原样报错。`reset_session()` 顺手重置
  `standalone_rejected`——恢复后 server 若仍拒绝 standing GET 会再告警一次 405（已知、无害）。恢复**不重拉
  工具表**（server 重启后换了工具集 → 调用拿到它自己的 unknown tool 错误，显式）。legacy SSE 没有 session，
  探测/取消有、恢复无。
  ④ **空闲探测三态**：`probe_if_idle`——`idle_for() < MCP_IDLE_PING`（60 s）直接过；否则
  `request_within(id, "ping", {}, MCP_PING_TIMEOUT)`（5 s，且不超过该 server 的调用超时）：有结果或
  `Failure::Answered`（server 应答了但拒绝，如没有 `ping` 的 `-32601`）→ 照常调用；`SessionLost` → 走恢复；
  `Other`（静默/连不上）→ 调用**不发出**，`unreachable after Ns idle — '<m>' was not sent (ping failed: …)`。
  `last_ok` 是 `idle_for` 的唯一数据源，只在「往返完成或 server 应答拒绝」时更新。stdio 恒 `None`（reader
  死亡已锁存，下一次调用本就即时失败）。
  ⑤ **standing stream 的 POST 前布防是尽力而为的竞态保险（已知缺口，未修）**：`request_bounded` 在 POST 前
  `ensure_standalone_stream(false)`——GET 在线程里异步起，POST 立刻出门。只答 202 的 server 若在客户端 GET
  连上之前就推送回复，这条回复无处可去，调用方坐满调用超时；standing GET 每 30 s 轮转（`STREAM_ROUND`）
  都会重开这个窗口。官方 SDK 的 server 在 POST 响应体上直接以 SSE 应答，不受影响；自测 mock 撞到过
  （120 s 挂起）。修法（待做）：reader 发布「已连接/被拒」，POST 前有界等它。202 分支收到应答后还会
  `ensure_standalone_stream(true)` 再等一次；仅在线程起不来（资源耗尽）时才落到**不可取消**的一次性 GET
  兜底（`get_event_stream` + `sse_response_for_id`）。
  ⑥ **协议版本是两个常量 + 一个共享 cell**：`PROTOCOL_VERSION`（`initialize` 提供 `2025-03-26`）与
  `KNOWN_PROTOCOL_VERSIONS`（静默集合：`2025-03-26` / `2025-06-18` / `2025-11-25`；其它原样采用并告警一次，
  缺省按 `2025-03-26`，永不因版本拒绝 server）。协商值存 `McpClient::protocol_version`，经
  `Arc<Mutex<String>>` 共享给 standing-stream reader——轮转重连的 GET 也要带对 `MCP-Protocol-Version`；
  `initialize` 请求本身带 cell 的默认值。支持新修订 = 往 `KNOWN_PROTOCOL_VERSIONS` 加一项，不动
  `PROTOCOL_VERSION`（2026-07-28 规范仍是跟踪项）。
  ⑦ **工具名单在 `start_server` 里过滤**（`filter_tools`），启动装配 / `build_agent_with_lenient` / 热增
  `build_one` 三条路径天然同款；`allow_tools` 的未知名字是硬错误（防拼写错与上游改名），`deny_tools` 的
  未知名字放行（上游删工具不该拖垮装配）；名单写 wire 名。`tools/list` 翻页在 `McpClient::list_tools`
  （64 页 / 256 工具 / 游标循环即停 / 任一页失败即握手失败），单页 server 的请求序列与翻页之前逐字节相同。
  验证：`mcp.rs` 测试块（脚本化 `Transport` + 真 socket / python 子进程）+ `scripts/e2e_mcp.py`（真实
  headless 二进制 × 文件内自带的零依赖 mock MCP + 脚本化 LLM，A–K 约 15 s，`--idle` 才跑 65 s 的空闲探测组 I）。
- **MCP 注解旁表 + `structuredContent` 取舍（内核改进计划 1.6）**：`McpToolInfo` 多三个 `Option<bool>` 注解
  （`destructive_hint` / `idempotent_hint` / `open_world_hint`，与原有 `read_only_hint` 并列）与 `output_schema`；
  归一后的 `ToolAnnotations` 四字段是 `bool` / `bool` / `Option<bool>` / `bool`——**「未声明」与「声明为假」的区别**靠
  规范默认值落位（`readOnlyHint`/`destructiveHint`/`openWorldHint` 有默认，`idempotentHint` 规范没有默认、省略即
  `None`），实现不依赖任何缺省值假设。**旁表而不是 `Tool` trait 方法**：声明属于 server，内置工具 / 绑定
  `CallbackTool` / run_code 注册都不必为它作答（第三方工具想声明就覆盖 `Tool::annotations`）；`ToolCtx::tool_annotations:
  Arc<BTreeMap<String, ToolAnnotations>>` 按**暴露名**（模型调用的 `mcp_<server>_<tool>`）索引，`ctx.annotations(tool)`
  是便捷读法（内置工具 `None`，MCP 工具恒有条目，什么都没声明时是 `ToolAnnotations::MISSING`）。**唯一构造点 =
  `Agent::try_new` + `Agent::refresh_tool_defs`**（每个改工具表的入口都过这两个：热增删、开关 run_code 都走后者），
  所以旁表永远不会与 agent 实际分发的工具表不一致；`task` 子代理与 run_code guest 持构造时快照（Arc 克隆——父侧之后
  的增删只换父的 Arc）。每次调用把该工具的注解快照进 `ToolCallRecord.annotations`（落 result.json 的
  `tool_calls[].annotations`，`skip_serializing_if = Option::is_none`——内置工具不出键）。**是 server 的自述、不是内核
  保证**：没有任何强制，调度上 `read_only_hint` 也只是并行提示；审批留在宿主一侧（`before_tool` 读旁表决策）。
  `structuredContent`（MCP 2025-06-18）：声明了 `outputSchema` 的工具以结构化载荷为权威——`shape_result_with` 的
  `prefer_structured` 分支用它的 JSON 文本**替换**文本镜像（镜像是给人看的有损渲染）；没声明的工具 `content` 优先、
  `structuredContent` 只做「纯结构化 server」的兜底（否则渲染成 `[empty tool result]`）。两种情况下结果里的图片都照样
  胜出（结构化半边带不了图），文本已被预算截断时也不再切换。
- **凭据卫生（内核改进计划 2.5；`ToolCtx.hidden_env` + `Config.env_passthrough`）**：此前子进程完整继承环境，模型一句
  `env` 就把密钥写进上下文、session 文件与 `--json` 事件流。现在 `tools::hidden_env_vars(cfg)` 在装配时算出要移除的变量集
  ——默认恰好是 `api_key_env` 指向的**那一个**变量（`env_passthrough` 列了它则为空；`api_key_env` 为空也为空），落进
  `ToolCtx.hidden_env`。**库从不修改进程环境**：移除发生在每个 spawn 点的 `Command::env_remove`——`shell` 工具两条链
  （unix `sh -c` / windows `cmd /C`，`hide_env`）、MCP stdio server 两条链（`McpClient::start_with_env(cfg,
  &hidden_env)`；无参 `start(cfg)` 等价于空集，自己持有 MCP 客户端的宿主按需选）、热增的 `Agent::add_mcp_server`
  （读 agent 自己的 ctx 集）。MCP 条目 `env` 里**显式写出**的同名变量照常下发——用户有意把密钥交给某个 server 时它保留
  （`.envs(&cfg.env)` 在 `env_remove` 之后）。`task` 子代理（`child_ctx`）与 run_code guest 的 ctx 是克隆，原样继承。
  **定位是防顺手不防对抗**：其余变量完整继承，同用户进程仍可经操作系统读到父进程环境——真正的隔离靠容器或受限用户。
  两条启动提示都走 diag 通道、每进程一次、只在对应条件成立时出现：卫生提示（`[config] API key variable(s) … hidden
  from tool subprocesses …`）只在配置里真有 `shell` 工具或 stdio MCP server 时发；无人值守提示（`restrict_to_workdir`
  未开）只在 headless（带 task）与 batch 发、TUI 不发（有人在看）——同样「纯半边」拆出来供测试，不必抓 stderr。
  验证：`tools::shell::tests::hidden_env_is_removed_from_the_spawned_shell`（真 spawn 探针）、
  `tools::tests::hidden_env_vars_is_the_key_variable_unless_passed_through`（集合的纯函数）、
  `mcp::tests::strip_inherited_env_removes_hidden_names_unless_configured`（配置的 `env` 优先）与
  `mcp::tests::stdio_server_is_spawned_without_the_hidden_env`（python 子进程回显环境）、
  `agent_tests::add_mcp_server_spawns_with_the_agent_hidden_env`（热增）、`subagent::tests::
  child_ctx_inherits_hooks_and_tags_subagent_name`（子代理）与 `run_code::tool::tests::
  guest_ctx_inherits_the_hidden_env_set`（guest）、`scripts/e2e_mcp.py` 组 K（真实二进制 × shell + stdio MCP：
  默认不可见、`env_passthrough` 放行、MCP `env` 显式下发，共 8 项）。Windows 侧（`cmd /C` 链、热增链）没有实跑，
  只做了 Windows 目标的交叉类型检查与 clippy（配方见 pitfalls「`#[cfg(windows)]`」条）。
- **`refresh_tool_defs` 与构造路径同一量纲（内核改进计划 0.2）**：热增删 MCP server、开关 run_code 之后重算
  `tool_schema_chars` 的那段曾经用字节数累加，而构造路径用的是 CJK 感知的 `compress::tool_schema_units`——同一个 agent
  热增一个带中文描述的工具之后，两条路径的估算口径就不一致。现在两边都过 `tool_schema_units_sum`（构造路径与
  `refresh_tool_defs` 共用），回归钉子：热增后与一次性构造的值相等。新增「重算工具表」的入口时照同款走这个函数，
  别再手写 `.map(|t| …len()).sum()`。
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
- **Hooks 拦截层（`hooks.rs`，P1，第三方扩展面）**：`trait Hooks: Send + Sync` 五默认方法
  （`before_tool` → `ToolDecision::{Allow, Deny(String), Modify(Value)}`；`after_tool` 事后审计；
  `on_chat_request` 只读观察主 turn 请求；`on_turn_stopping` 轮末仲裁——FR-1：仅 top_level、仅
  `end_turn`、仅即将发 `Done` 前调用，`Continue{note}` 复用 R1 旁注管线（入队 → 下轮轮首
  `deliver_notes` 投递），续跑帽 `TURN_ARBITRATION_CAP = STUCK_STOP_AT`（6），超限降级
  `stop_reason="arbitration_capped"`、note 只留在 `TurnArbitration` 事件里；**默认 `Stop` 零漂移**
  ——非默认决策才发事件，既有钉子表不漂移；子 agent run（`top_level=false`）从不仲裁；
  `on_before_compact` 压缩前观察——见本条末尾的 2.2 段），
  `Agent::set_hooks` 后置安装（仿 `set_cancel` 模式）。
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
  真并发下调用序关联不可靠，per-call ctx 是唯一可靠的来源标签。该破坏性改动落地时尚在绑定透出
  hooks 之前（趁窗口期）；**两个绑定此后都已透出 hooks**（py `set_hooks` / node `setHooks`，Node
  决策走 round-trip、观察类 TSFN 吞异常）。
  **Slot 观测契约表**（`agent_tests.rs` 的 `slot_table_*` 六例）：`Slot` 只有 `Cancelled`/`Ran`
  两态，但**进入路径有九条**（顺序 3 + 并行 4 + 批次整体跳过 + 并行 worker panic——真的跑了但
  worker unwind，合成 `Slot::Ran` 错误槽：records 一条错误、`duration_ms` 为空、
  `after_tool(executed=true)`），每条对外的观测后果不同。六个用例用
  同一张表钉住「状态 → 五类观测面」（records / `ToolResult` 事件 / `ToolCall` 事件 / `before_tool` /
  `after_tool`），并派生两条不变量：**before↔after 严格配对**（`Ran` 两者都有、`Cancelled` 两者都无）
  与 **ToolCall↔ToolResult 事件配平**（announce 过的必须闭合；从未 announce 的保持沉默）。此前这些断言
  分散在六组测试里各写各的，任一条路径悄悄偏离没人拦。不变量用变异测试验证过——分别去掉 Cancelled
  的 ToolResult 合成、去掉占位循环的 `after_tool`，对应用例立即转红。新增 Slot 赋值点或改占位/合成
  逻辑时先对照这张表。
  **压缩前观察 `on_before_compact(&CompactionInfo)`（内核改进计划 2.2）**：唯一调用点
  `Agent::notify_before_compact`，坐在**摘要已成功**与**改写历史**之间——每次「真的折叠」同步调一次，
  触发路径三类全覆盖（run 内 preflight / overflow 恢复、宿主的 `Agent::compact*`、`task` 子代理自己的
  压缩——后者带 `info.subagent`、跑在那条 worker 线程上）。摘要失败 / 空摘要 / 仅 prune 都**不触发**
  （没有折叠可通知）。`CompactionInfo` 全是借用（`trigger`、`subagent`、`dropped`、`dropped_chars`、
  `todo` = 最新 todo 调用的渲染清单、`folding` = 即将离场的那些原始消息，仍可读）——**只读、不能否决**
  （摘要已计费，调用返回即折叠；要留档就在这一次调用里克隆）。调用点不持锁。它是**唯一 panic 会被
  兜住**的钩子：`catch_unwind(AssertUnwindSafe)` 包住，panic 经 diag 通道报一行
  `[compress] on_before_compact hook panicked (…) — folding anyway` 后照常折叠——此刻摘要已计费、
  未折叠的历史离 overflow 只差一个请求，坏掉的观察者不该让这次折叠作废（发布构建 `panic = "abort"`
  兜不住 panic=abort 的进程，所以实现仍不该 panic）。慢钩子只观测不设限：超过
  `COMPACT_HOOK_SLOW_WARN`（= 仲裁钩子同款 5 s）`diag_warn` 一次（run 内路径下它睡多久就吃掉多少
  剩余预算）。**两个绑定都没转发它**（鸭子类型只认 `before_tool` / `after_tool` / `on_chat_request`）。
  TUI 的 `/compact [focus]`（idle-only）是同一个内核调用的键盘入口：worker 上的忙任务，状态栏
  `compacting…`、Esc 取消、期间输入照常排队；成功时照常渲染折叠自己的 `Compressed` 事件，历史太短是一行
  `[compact] nothing to fold …`，失败一行 `[compact] failed: …`（cause 链只打印一次），Esc 取消只留 UI
  自己的 `[cancelled]`；成本在每条路径上都诚实（见 `compact_report`）。
  新增折叠路径时必须过 `notify_before_compact`——它的三个「不触发」条件是 `agent_tests.rs` 的钉子，
  漏调或多调都会让「观察点 = 每次折叠恰一次」失真。
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

  **手写 6 字段 frontmatter 解析（无 YAML 依赖）**：必填 `name`/`description`；可选 `license`/`compatibility`（标量）、
  `allowed-tools`（空白拆成 `Vec`）、`metadata`（缩进 `key: value` 块 → `HashMap`，`in_metadata` 状态机）。
  **块标量（内核改进计划 0.3）**：四个标量字段（`name`/`description`/`license`/`compatibility`）与 `metadata` 的每个值都接受
  单行值或 YAML 块标量——`|`（字面：保留换行）或 `>`（折叠：同缩进的相邻行并成一个空格，紧邻空行或更深缩进处不并），
  可带 chomping 指示（`-` / `+`：校验后忽略——值恒 trim，所以它不改变结果）与缩进指示（1–9）。块体按缩进收集：比 `key:`
  行更深缩进的行都属于块，第一个非空行定基准缩进（更深的行保留多出的缩进），**第一条缩进 ≤ 键缩进的非空行就是下一个键**
  ——所以折行的 description 后面可以直接跟 `metadata:`（是兄弟键，不是描述文本）。`description` ≤1024 的校验作用在
  **拼接并 trim 之后**的值上。**仍刻意不支持**（不引 YAML 依赖）：多行**裸**标量（plain scalar 续行）、块序列（`- item`）、
  嵌套映射——这些键保持原有行为（必填字段读成空 → `Missing` → 告警跳过；`allowed-tools` 读成空；未知键按规范忽略），
  不做猜测式解析；`metadata` 仍是唯一接受缩进 `key: value` 块的字段。**`SkillEntry` 带全 6 字段**，但 level-1 简介只透出 name+description（license/
  compatibility/metadata 仅存储不进 system——简介须短）。**name 严格校验**：1-64 字符、仅 `[a-z0-9-]`、首尾非 `-`、无 `--`、
  **必须等于父目录名**（`valid_skill_name` + scan 内 `name == dir.file_name()` 校验）；`description` ≤1024；任一不过 →
  warning+skip。三级加载：①简介拼进 **system_prompt**（常驻——压缩只动 messages 不动 system，故简介安全；但计入
  `estimate_tokens`，必须短）②全文（SKILL.md 正文）经 `read_skill` 读进 **history**（tool_result，可被压缩回收，**不**常驻
  system）③资源文件（`scripts/`/`references/`/`assets/` 等子目录）。

  **`read_skill` 放开子目录**：`file` 用 `safe_relative_path`
  （允许 `/`，禁 `..`/绝对/`\`/空/`.` 组件）+ `confined_to(dir, full, "skill directory")` 双层沙盒——后者复用 `tools/mod.rs`
  抽出的 `pub(crate) confined_to(root, full, label)`（`confine` 委托它，root 在内 canonicalize；
  **2026-10 起尾段逐组件解析走查**：最深已存在祖先 canonicalize 后，余下组件按源序推进——存在组件
  canonicalize 着陆 canonical 真身、`..` 词法弹出、缺失组件词法累积，**每步重新落地真实盘面
  （canonicalize 成功）即复检包含性**；初始祖先自身也先复检（`root/../x`、`out_link/../..` 类拼写
  的祖先可落 root 外）。旧「尾段词法折叠」会把 `missing/../out_link`（已存在外向 symlink）折叠进
  返回值——Ok 且物理逃逸 + 再入漂移 Err，性质 fuzz（5 万次随机路径汤）抓到后重写。Ok 返回值永不背
  `..`/`.`、再入是不动点；内核解析真穿过外向 symlink 的拼写一律 Err，内向 symlink 尾段返回真身。

  **present-but-unresolvable 一律 fail-closed（审计 2026-10 追加）**：尾段组件 canonicalize 失败后经
  `symlink_metadata` 分诊——仅 ENOENT 真缺失走词法累积；悬空 symlink（目标不存在的链，写穿可在
  目标出现时落到 root 外——审计员实测 `std::fs::write` 真逃逸过）与 EACCES/ENOTDIR 等存在但不可
  解析的组件一律 Err（调用方 I/O 会撞同一障碍，早拒是诚实不是损失）；fuzz 沙盘的 `dangle_out`
  （悬空且外指）与去掉 `exists()` 门的 lstat 直检性质同批落地——`exists()`=false 曾把悬空链整类
  拱出断言面。
  TOCTOU caveat 同 workdir 沙盒）。

  **`list_skill` 工具**列技能目录树（`walk_skill`，`file_type` 不跟随 symlink 防逃逸、深度封顶 4、跳隐藏），供模型 level-3 自主
  发现资源。

  **内置（embedded）skill（v2 P2）**：`SkillEntry.source` 是 `SkillSource::{Dir, Embedded(String)}`
  枚举——`skills::embedded_entry(text)` 按同款 frontmatter 校验建条目；`read_skill` 对 Embedded 直接吐全文
  （读非 SKILL.md 的 file 报"无附属文件"）、`list_skill` 列单文件；`assemble` 在 `code.enabled` 时把
  run_code 的用法指南（`src/tools/run_code_skill.md`，`include_str!`）经 `insert_builtin` 注入——**磁盘同名
  skill 覆盖内置**（insert_builtin 撞名即 no-op、保持名字序），allowed-tools 白名单告警检查对 `run_code`
  在 enabled 时放行（它的 membership 由 code.enabled 驱动、不在 cfg.tools 白名单里）。
  `read_skill`/`list_skill` 都**不在 `cfg.tools` 白名单**（skill≠tool，刻意决策）——扫到技能时由 `assemble` 自动配套注入
  （`tool_list.push(ReadSkill)` + `push(ListSkill)`，各 `index.clone()`）。

  **子 agent 不继承**：`subagent.rs:133` 硬编码
  system_prompt 覆盖为 `SUBAGENT_SYSTEM_PROMPT`，且子 agent 走白名单建工具集，故既无简介也无 `read_skill`/`list_skill`
  （决策，不改 subagent.rs）。坏 skill（缺 SKILL.md / 缺 frontmatter 字段 / name 不合规 / name≠目录 / description 超长）
  跳过 + stderr `[skill]` 告警，**不 fatal**。

  **`allowed-tools` 仅告警不强制**（标准标 Experimental；TinyAgent 无 per-turn 激活层，
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
  **栈帧位置是「出错前最近一个带位置信息的语句」，不保证是出错的那一行**——quickjs 只给部分指令挂源位置
  （调用、throw 等），`const r = nope(x);` 里未定义标识符的读取没有自己的位置，报出来的是更早的那条语句，
  可以早好几行，极端情况落在 prelude 行上（实测报成 `<run_code>:1:2248`——prelude 那一行的某一列）。标识符名在消息里，模型照样能自纠
  （2026-11-26 真机批次 deepseek-flash 写了 `await0(n)`，一次报错下一轮改对）；别拿行号去反推源码行。
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
  **2026-10 可观测性**：`RunOutcome`/`RunError` 带 `compressions`（压缩审计：trigger/dropped/kept/
  压缩前后估算、最近锚点请求实测值；`RunError` 的手写 Serialize 恒写该键——错误信封 schema 稳定
  惯例，result.json 侧空仍不插键）；`RunOutcome.turn_timings` 逐轮耗时+工具数（失败路径不落行，
  总耗时看 `elapsed_secs`）；`tool_calls[].duration_ms` 只盖真实执行的工具体（排队/取消检查点不计，
  未启动占位 None）。NDJSON `Compressed` 事件带 `estimated_tokens_before/after` 与
  `measured_tokens_before`（锚点请求的 provider 实测，非校准估算——语义见字段文档）。
  **构建归因**：`result.json` 与 batch `report.json` 顶层都写 `agent_version`（`CARGO_PKG_VERSION`，
  与 `--version` 同源）——换构建的长系列 A/B 靠它区分前后两半；`schema_version` 契约（只加键 /
  新键一律可选 / 破坏性才升版）写在 README「结果文件」，常量 `RESULT_SCHEMA_VERSION` 单处定义。
  **NDJSON 类型是内核穷尽契约**：`AgentEvent::ndjson_type()` 对每个变体给标签（新增变体不补即编译
  失败），harness 的 `_` 兜底只负责 `unmapped: true` 标记而不是发明 `unknown`——否则
  `#[non_exhaustive]` 会让新事件静默变成无名流事件，而消费方（CI 管线 / 绑定回调）恰是按 `type` 分支的。
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
  image/document/skill_content/reasoning/other；**没有 thinking 桶**——`ThinkingDelta` 是纯事件流、永不进
  history，恒 0 字段是撒谎式 API；`reasoning` 桶计的是另一条通道——`preserved_reasoning`（2026-11-26 起默认开）打开时随
  assistant 消息落在**旁字段** `Message.reasoning` 里的回传块（`compress::reasoning_units`，与
  `estimate_tokens` / `total_chars` 用同一份），开关没开或当前轮没有回传时恒 0；skill_content 按 tool 名 ==
  `read_skill` 归因，ToolResult 只有 tool_use_id，先扫 ToolUse 建
  id→name 映射（同 compress `todo_use_ids` 模式），eager 注入对与运行期真实调用一并归入）。**口径纪律**：
  一切数字是启发式估算（`compress::estimate_tokens` 同一套 yardstick：units/4——ASCII 字符计 1、
  CJK 计 9、其他非 ASCII 计 4——+ 图片 len/100 + PDF 页
  ×3000），**不是 provider 实测——记账权威永远是 Usage 事件/`usage()`；估算与实测刻意不混流**，所以走
  拉取式查询而非事件。**等和不变量**：未校准时
  `system.tokens+tools.tokens+messages.tokens == total_tokens == estimate_tokens(全量)` 严格成立，靠**单趟
  running-floor 分摊**（对全部 chars 段按固定顺序一遍：每段领 `floor(acc_after/4)-floor(acc_before/4)`；
  各段独立 /4 会因舍入差 1-2——`uncalibrated_total_equals_estimate_tokens` 测试是刻意钉子）；共享计费
  原语 `compress::block_chars`（chars 侧）/`block_units`（token 侧，文本走 CJK 感知的 `text_units`）/
  `block_pdf_tokens` 是 total_chars/estimate_tokens 与分解共用的单点
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
  **工具表规模提示（内核改进计划 2.3）**：`ToolsBreakdown::advisory()` 在工具 schema 估算**严格大于**
  `TOOL_DEFS_ADVISORY_TOKENS = 10_000` 或工具数（四类之和，`tool_count()`）**严格大于**
  `TOOL_DEFS_ADVISORY_COUNT = 30` 时出提示（恰好等于不算越过；token 数取报表**当前**口径——校准缩放后的
  `tokens`，工具数不缩放；`ToolDefsAdvisory::{Tokens, Count, Both}`）。同一判定在装配期也走一遍：
  `Agent::try_new` 末尾（仅顶层 agent）用 `tool_table_tokens(system, tool_schema_chars)` 算出与报表**同数**的
  值（system 段之后的 running-floor 份额 `floor((sys+U)/4) - floor(sys/4)`，
  `tool_table_tokens_is_the_breakdowns_number` 钉住两者永远相等），越过即经 diag 通道（`[context] …`，
  `note_tool_defs_advisory`）**每进程按文本去重提示一次**；子 agent 与热增删（`refresh_tool_defs`）不提示
  （子 agent 的工具表是父的有界子集、会按 child 重复；热增路径按契约不打印——`/context` 报实时表）。
  文案单一来源 `tool_defs_advisory_lines`（五短句、每行远小于 80 列——启动期 diag 经 stderr fallback 进
  未进 raw 的终端，长行会被终端按右缘硬折、断词）。**只提示不处置**：裁不裁工具表由宿主决定。
- **TUI 是可选 feature**（`ratatui` + `crossterm` + `tui-markdown` + `unicode-segmentation` + `unicode-width`），
  `src/tui/`（模块根 `tui.rs` + app/transcript/markdown/slash/render/input/theme/tests）在
  `#[cfg(feature = "tui")]` 后——2026-10 由 8.7k 行单文件按关注点拆分（纯搬移零行为变更，
  148 个测试随迁；`crate::tui::run` 入口不变）。默认 headless 版不含它，保持体积。

  **内联/流式渲染**（非全屏 alternate-screen，与 Claude Code / Codex 同款）：
  `Terminal::with_options(Viewport::Inline(INLINE_HEIGHT=8))` 在主屏底部锚定一个固定高度 live 区
  （流式 zone + 输入框 + 状态行），每帧 `term.draw` 重绘；已定稿的对话块经 `term.insert_before`
  **提交进终端原生 scrollback**（`commit_rows` + `committed` watermark 追踪已提交行）。原生选择 /
  滚动 / 搜索交给终端——**没有鼠标 handler、没有 app 托管的滚动/选区**（旧全屏重绘模型的选区漂移
  bug 由此根治）。流式中的在途 Assistant 行由 watermark 暂留（不提交）、live 区里带 `▌` 光标渲染、
  回合结束（stream 关闭）才整条提交。**`ratatui` 的 `scrolling-regions` feature 已在 Cargo.toml 启用**：
  默认回退路径每次 `insert_before` 都 `Terminal::clear()`（发一个 DSR 光标查询 `\x1b[6n`），开了
  scrolling-regions 走 CSR(DECSTBM) 滚动区路径、无此查询且渲染更平滑（CSR 现代终端普遍支持）。

  **终端断开监控（Unix）**：crossterm 0.29 的 Unix event source 在 tty EOF 时不会从内部 read loop
  返回（`read == 0` 不退出、非 WouldBlock 错误被忽略），pty master 关闭后主线程会卡在 `event::poll()`
  内满核空转，外层事件循环收不到错误。`spawn_terminal_disconnect_monitor` 因此用独立线程对同一 tty
  （stdin 是 tty 则复用其 fd，否则自行打开 `/dev/tty`）做 100ms `libc::poll`，只探测、不读走输入；
  HUP/ERR/NVAL 时置 cancel + shutdown、发 `None` 让 worker 收尾，最多等 3s，再恢复终端并
  `process::exit(0)`。**这是有意的进程级逃生口**：主线程可能永远困在 crossterm 内，不能指望正常
  return/Drop；正常退出只置 shutdown，监控线程最迟下一轮自行退出。未知探测错误 fail-open（不杀健康
  TUI），EINTR 重试。非 Unix 为 no-op。真实 openpty 单测钉住关闭前后探测结果，pty 进程级验收须覆盖
  自动退出与 CPU，不只断言普通 idle。

  **TUI 定位（原"冻结只修 bug"政策已按产品决定放宽）**：交互体验对齐主流终端 AI 工具的基础项
  已落地一批（见下），红线仍是**架构与渲染模型**——不做全屏 / 鼠标托管 / 主题系统这类结构性扩张。
  已落地的扩展面速记：
  - **视觉语言与底部 chrome**：空态是三行 render-only 报头——宽屏为与 120 列工作区同宽、版本号右对齐到
    输入框右缘的 `T I N Y A G E N T` 间隔字标，加双色规则线与简洁标语；窄屏先舍版本号。键帽只在
    composer 底边框上出现一次，空态不重复。正文、composer、队列预览、状态行与空态/弹窗共享左锚定、
    最多 120 列的工作区（空态/弹窗的宽度参数必须传 `content_w`，不能传 zone 的全宽）。正文用粗体青色
    `❯` 用户行与绿色 `◆` 助手行分层，工具头以 `TOOL_HEAD_MARK="    ▎ "` 串联调用/结果，展开正文延续
    `│` guide；Thinking 用独立色斜体，系统信息/警告/错误分别用 `·`/`!`/`✗`。行内代码与键帽、slash
    选中行共用 `keycap_bg()` 的主题感知凸起表面（选中行另有 `❯` gutter，不再整行反色）；普通白字不被
    重染。composer 使用圆角边框与 `composer_bg()`，空闲标题为 ` ◆ MESSAGE `，底边框在空间足够时放
    Enter / / / Ctrl-J / /help 键帽（窄框先省 `/` 与 Ctrl-J），忙时撤下。状态栏以 `surface_bg()` 铺
    低对比底色，模型是青色粗体文字而非实心徽章，状态文案为 ready/running/cancelling/failed；累计 token
    段只在该会话已产生计费（含仅缓存流量）时出现，新会话不显示 0；路径有 48 列显示预算，超长时保留
    最长完整尾部目录段并加 `…`，不足 12 列则隐藏。空态与 slash popup 共用 grapheme-aware
    `truncate_line`，角色/工具符号变更必须同步最终折行的 `continuation_prefix`。
  - **流式增量定稿（`split_streaming_row(&mut rows, width)` + `Row::AssistantCont{text,tight}`）**：
    **只切在途 Assistant，Thinking 与子 agent 都不切**。显示行测量与最终折行共用 grapheme-aware
    `LayoutCell`/`unicode-width` 口径（CJK、ZWJ emoji、variation selector 都按完整 grapheme 处理），
    超过阈值即把安全前缀定稿进 scrollback、尾段继续 live 流式。**切分点两级**：优先最新安全段落
    边界（`last_safe_boundary`，空行分隔、前缀围栏闭合），续块 loose（块间补空行）；段落本身超过
    阈值时退化为**显示行切分**（`soft_cut_offset` 保留最后 `LIVE_TAIL_ROWS=2` 行），续块 tight
    （无空行，`/export` 以 `\n` 拼接）。围栏内切分给尾段补同字符同长度的合成开栏；切点落在未闭合
    inline code 内则等待闭合，fence 状态只由切点前的**完整逻辑行**推导，closing fence 滑到 EOF
    时拒绝空尾切分。**放弃切分必须原子**：先在不可变切片上验证 EOF/全空前缀，再一次性
    `split_off`/`truncate`，不能先删换行再 return。阈值首切 `INLINE_HEIGHT`，切过一次后降为
    `LIVE_TAIL_ROWS`；年轻段落（≤2 行）仍走段落切分。**`trailing_spacer` 陷阱**：commit 渲染的
    切片末行只有 `pending_end == rows.len()` 才按历史末尾补空行，否则每批提交多一条空行、tight
    段落被撕开。续块无 ◆ 图标；子 agent 气泡不切。改 `split_streaming_row` 或 Cont 形状时同步
    更新对应构造/匹配与真实 `build_history_lines → wrap_history` 最终渲染等价测试。
  - **思考区块（`Row::Thinking` / `Row::ThinkingFull`）**：ThinkingDelta 走
    `append_thinking_delta` 进独立色斜体 `✻` 行——`thinking_stream: Option<String>` 是当前在飞
    trace 的 tag（"" 父 / 子 agent 名，merge 判据同 `sub_streaming_name`），答复/工具/任何其它行插入
    即关闭、再次 trace 开新行；**不走 markdown/MdCache**。`commit_pending_end` 与
    `inflight_row_index` 仍把 Thinking 与 Assistant 同等 hold back（`LiveRow::{Text,Think}` 决定光标
    归属），但 Thinking **不走 Assistant 的增量切分**：live 只渲染按显示宽度截取的有界尾部，避免
    每个 delta 重排整段；关闭后整块一次提交，超过 `THINKING_FOLD_ROWS=8` 个显示行折叠为头部 +
    末尾 `THINKING_PREVIEW_ROWS=3` 行。**定稿态的行数、折叠判定与预览切片都按 `trim_end()` 后的
    原文算**——真实 trace 以 `\n`（常是 `\n\n`）结尾，尾部空行不是内容：否则头部行数虚高、三行
    预览有一行空白，八行整的 trace 还会被空行推过阈值。`Row` 内始终保留完整原文供
    `/export`；`/thinking [n]`
    克隆成显式 `ThinkingFull` 追加重印，绝不原地改已提交 scrollback。新增任何"在飞行"仍必须
    同步 holdback/inflight/LiveRow 三件套。
  - **共享显示布局与 composer**：`render.rs::layout_cells` 以 `unicode-segmentation` 的 grapheme
    cluster 为最小单元，同时保留 composer char offset、源 byte offset、显示宽度与样式；最终
    `wrap_line`、`est_display_rows`/`display_row_starts`、显示截断都复用这套口径，禁止按 `char`
    拆 ZWJ emoji。`layout_composer` 负责软折行与 source offset→cell 光标映射，`composer_scroll`
    是纵向滚动唯一计算点；`InputBuffer.col` 仍是 UTF-8 byte offset。已删除 `input_hscroll`，长
    prompt 在固定 inline viewport 内纵向审阅。drop-space 两侧、exact-full-width、grapheme 内 caret
    与 1–4 列极窄 continuation 都有 TestBackend cell-grid/cursor 钉子。
  - **render-only 瞬态模式**（`thinking`、`tool_building`、`tool_running` 同款）：渲染期合成、不进
    rows / MdCache / 转录——live 区渲染优先链 popup > 空态 > thinking 占位 > inflight 流式行 >
    `tool_building`（`→ 工具名 · receiving arguments`）> tool_running 占位（`→ 工具名 · 1.2s…`，
    并行 fan-out 聚合 `(+N)`，busy 时 120ms tick 走字）。ToolInputDelta 只驱动 tool_building，
    不累计 JSON 到 UI 状态。新增瞬态展示照此模式。
  - **队列与运行相位**：`queued: VecDeque<String>` 是唯一真值，**没有 `Row::Queued`/transcript
    ghost**；独立 queue preview 显示最老待发消息与长度，真正受理投递时才追加 `Row::User`，因此
    待发送文本不会卡住 commit watermark。正常完成自动 flush FIFO；取消清空队列；错误进入
    `RunPhase::Failed` 并置 `queue_paused`，必须 `/resume` 显式释放，另发新 prompt 不隐式释放旧
    FIFO。`/queue` 列表、`/unqueue` 取回最新一条到 composer。**不变量：`queue_paused` ⇒ 队列非空**
    ——暂停是「被扣住的消息」的属性：失败时队列为空不置位（`pause_for_held`），取消清队、
    `/unqueue` 取走最后一条都解除（后者同经 `pause_for_held`）；残留标志会把下一次正常运行期间排入
    的消息标成 paused 并在轮末扣住，而用户从未见过暂停提示。failed 态标题由
    `failed_title(held, width)` 按有无扣住消息给出 `/resume` 或重试提示（空队列时 `/resume` 只会答
    `[no paused queue]`）。
    `RunPhase::{Idle,Running,Cancelling,Failed}` 驱动标题/边框/状态与 busy popup，重复取消不重复刷
    `[cancelling…]`。
  - **详情追加重印**：`/out [n]`、`/call [n]`、`/thinking [n]` 分别克隆第 n 新的工具结果、工具
    调用（完整输入 JSON）、思考块；`Ctrl+O` 取三者中最新一项。复用展开渲染与提交通道，不修改
    已提交历史，也不虚构同名并行调用的精确配对。失败工具结果自动展开同时受 **20 行 + 2000 字符**
    限制。`SLASH_COMMANDS` 一张表驱动 grammar、/help、补全与 busy 隐藏（新增命令仍需接 classify
    与 Enter 执行臂）；`/clear`、`/context`、`/resume`、`/compact` 是 idle-only。
  - **resume 转录**（`tui::transcript_rows(&[Message]) -> Vec<Row>`）：main.rs 在 build_agent
    **消费 `initial` 之前**快照；先一遍扫全部 ToolUse 建 id→name map，ToolResult 按 id 反查配名
    （查不到用 `(unknown)`），`elapsed: None`（字段本就为 resume 设计）；Image/Document 分别显示
    `[image attached]` / `[PDF document attached]` 轻量占位，不静默抹掉轮次。resume 时**跳过
    initial_task 镜像**（末条 user 已在转录里，再镜像重复）；`Row` 因此 `pub(crate)`。
  - **浅色主题**（`TINYAGENT_TUI_THEME=light`）：`THEME_LIGHT: OnceLock<bool>` 进程级一次设定
    （`run()` 入口读 env，OnceLock 无法复位——**测试不得 set 它**，light/dark 两态逻辑抽纯函数
    `theme_light_from` / `dim_fg_for` / `code_fg_for` 测）；dim 文字走 `dim_fg()`（dark=Gray 现状 /
    light=DarkGray），行内代码走 `code_fg()`（dark=White / light=青）并统一铺 `keycap_bg()`——
    `redye_inline_code` 只重写 tui-markdown 原生的 white-on-black span，别误染普通白字。新增暗色
    UI 元素用 `dim_fg()` 别再引常量。
  - **键位语义**：单行输入（含空）或浏览中 ↑/↓ 走历史召回（判定 `up_is_history`），多行恢复跨行
    移动；`Ctrl-D`/Delete 删后字符（空输入空闲时 Ctrl-D 退出），Ctrl/Alt+←/→ 按词移动，
    Ctrl+Backspace 删词；Shift+Enter / Alt+Enter / Ctrl+J 或行尾 `\`+Enter 换行；补全弹窗
    Shift+Tab（兼容 `BackTab` 与 `Tab+SHIFT`）反向选择；`Ctrl+O` 追加重印最新详情；空闲
    `Ctrl-C` 双击 2s 窗口确认退出（判定 `ctrl_c_should_quit`），忙时单击取消不变。
    `is_normal_stop` 名单含 `cancelled`（Esc 已提示，Done 行是噪声）。
- **回归任务集与评测装置（内核改进计划 0.4；`examples/regress/` + `scripts/regress_eval.py`）**：改内核之后
  跑的就是它——六个任务（`multi-file-edit` / `long-output` / `mcp-many-tools` / `recover-after-failure` /
  `cjk-docs` / `loop-suppression`）× 五个配方（`kernel-regress` / `reasoning-carry` / `compression-handoff` / `summary-preview` /
  `mock-smoke`）。**任务目录** `examples/regress/tasks/<id>/`：`<id>.json` 是任务片段（`task` + `limits`）——与
  `--batch` 任务文件**同格式、可直接用**；评测侧数据放同级 `harness.json`（类别、底配置覆盖、零成本通道的脚本化
  模型），不进模型可见的配置；`check.py <result.json>`（cwd = 本次 run 的 workdir，退出 0 = 通过）是**唯一的
  判卷点**——结论只来自它，不来自 CLI 退出码；`fixture/`（拷进 workdir）、`setup.py`（生成数据）、`expected/`
  （参考文件，脚本经 `{"$file": "expected/…"}` 引用）可选。**每次 run 是一次单进程调用**（`--config … --json
  --output result.json --session none`），不走 batch 模式——评测测的是内核在一份任务上的行为，batch 的重试梯
  属于被测对象之外；任务片段里的 `limits` 仍按 batch 同款 `Clamp` 语义合并（**只能收紧**，`LIMIT_CLAMPS` 与
  `config.rs` 的 `Clamp` 逐条镜像）。**配置叠加顺序**（`build_config`）：底配置 ← 任务覆盖 ←（mock 专用覆盖）
  ← **臂覆盖** ← 任务片段——**臂压过 mock 调参**：臂是实验变量、必须说了算，否则「把 `keep_recent_messages`
  压到 0」这样的臂会被任务的 mock 调参悄悄改回去（先写 pytest 抓出来的）。**两条通道**：`mock`（零成本——
  每次 run 起一个 `mock_openai.py` / `mock_anthropic.py`，端口由系统分配、从 stderr 首行读回；底配置由脚本
  生成，不读用户配置、不碰密钥）与 `paid`（底配置来自配方指向的配置文件；`run` 默认 dry-run，显式 `--execute`
  才真跑；`prepare` 时没同时冻结 `--price-table` 与 `--cost-stop` 的批次标 `blocked_paid`，`run --execute`
  一律拒绝；密钥只经 `api_key_env` 由 CLI 自己读，harness 只看变量在不在、从不读值）。**封存校验**
  （`verify_frozen` + `run_config`）：`prepare` 把二进制、评测脚本、任务文件、底配置的 sha256、逐 run 配置与
  执行顺序（重复轮次间 AB/BA 交替，抵消顺序偏差）冻结进 `manifest.json`；`run` 前逐项比对，任何一项变了
  就要求重新 prepare，重建的配置还要与冻结副本逐字节相等（「清单说的就是跑的」）；半途的 run 目录挪开、
  绝不覆盖，已有 `verdict.json` 的 run 跳过（可续跑）。**路径一律绝对化**：`run` / `summarize` 的批次目录、
  `prepare --base-config` 的路径都先解析成绝对路径（清单里记的也是绝对路径）——run 的子进程 cwd 是 run 目录，
  文档里的相对写法在那里解析不到（2026-11-26 第一次真机批次六个 run 全部启动即败；零成本通道走的是绝对
  临时目录，从没踩到）。**清单脱敏**（`redact`）：逐 run 的真实配置只落
  run 目录（0600），进清单的副本对 `extra_headers` 与每个 MCP 条目的 `headers` / `env` 只留键名（与内核
  `Config::fingerprint` 同口径——MCP 的是**嵌套**在 `mcp_servers` 里的，只脱顶层会漏），
  `base_url` / MCP `url` 去掉 query 与 fragment（有的网关把密钥放在那里）。**费用停止**：
  `已花 + 下一个 run 的最坏预留 ≥ 停止值` 即不再起新 run；最坏预留 = `max_turns × (max_tokens + 窗口)`
  （窗口取 `max_context_tokens`，没配按 `compress_threshold_chars / 4`，封顶 `max_total_tokens`）按四个单价
  里**最贵的**折算（宁可高估）；用量报不全的 run 按预留记进「已花」、绝不当 0（与 `code_value_eval`
  同口径）；`--cost-stop` 只收正的有限数（bool / `inf` / `0` / 负数即配置错误，`is_cost_stop`）。
  **hermetic（两条通道一视同仁，`run_env`）**：每次 run 把 `XDG_CONFIG_HOME` 指到 run 目录内的空目录——`[skill]`
  的全局查找跟着它走，否则开发者本机的真实技能目录会漏进轨迹（付费通道起初沿用开发者环境，2026-11-26 第一次
  真机批次的 run 日志里就出现了开发者本机技能目录的 symlink 提示）；并给被测二进制（连同它拉起的工具 / MCP
  子进程）设 `GIT_CEILING_DIRECTORIES` = run 目录与它的全部祖先——批次目录默认在仓库里，workdir 自己不是仓库时
  git 会向上找到外层仓库，模型跑 `git status` / `git diff` 就把开发者未提交的 diff 发给了模型（同一批次的
  `multi-file-edit` 抓出，约 30 KB），换成 `git stash` 就是改坏开发者的工作区；workdir 自己是仓库时照常被发现，
  挡不住的只剩「先 `cd` 到祖先目录再 git」。mock / setup / check
  子进程带 `PYTHONDONTWRITEBYTECODE=1`（否则仓库里落 `__pycache__`）。**`check` 是 CI 门禁**（`check.sh e2e`
  组第三步、ci.yml 同款命令）：任务集校验 → 每个任务 × openai / anthropic 两种线协议的脚本化正路径全过 →
  「什么都不做」的模型必须过不了判卷 → 同一任务跑两次汇总逐项相等 → 付费闸门逐条拒绝 → `mock-smoke`
  配方把 prepare → run → summarize → compare 走一遍；用的假付费配置指向 `127.0.0.1:9`、密钥变量名从未
  设置——**任何环境下都发不出一次付费请求**（环境里有真密钥也一样：`check` 不构造付费 run）。harness 自身
  的离线单测在 `scripts/tests/test_regress_eval.py`（本地 pytest，不进 CI）——**动 harness 先写/跑它**，
  七个缺陷都是先写测试才在跑之前抓出来的。判卷/汇总里两个易漏点：`result.json` 缺失时 `turns` 记 0
  （不让汇总 KeyError）、工具错误数 `tool_errors` 作为独立指标进汇总与对比表的 `errs` 列——只看通过率
  会把「模型反复撞错误才通过」掩盖掉。**读真机批次的三个要点**（2026-11-26 三个批次的经验）：① 判卷只看
  产物——`PASS (exit 124, timeout)` 是「超时前产物已经正确、模型没来得及收尾」，按通过计，所以通过率要连同
  `stop_reason` 一起读；② `compressed` 事件之后的第一条 `usage` 事件是那次摘要调用的用量（折叠经
  `bill_chat` 入账），`output_tokens` 等于预算上限即摘要被截断——推理模型的思考 token 计入同一份预算；
  ③ 只在会压缩的任务上两臂才有差别，其余任务的臂间波动是噪声（每格 3 次时尤其如此），结论按任务分行说，
  别只报合计。付费通道保持串行：网关在 3 路并发流时回过 502（`upstream timed out`）。

