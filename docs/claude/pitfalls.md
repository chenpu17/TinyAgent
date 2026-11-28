# 容易踩的坑（详录）

> 自 CLAUDE.md 归档的完整版（2026-09 瘦身拆分）。CLAUDE.md 只保留每主题一句话索引；
> 修改条目时详情写在这里，并同步更新 CLAUDE.md 的对应索引行。


- **语言绑定的工具回调是跨线程 round-trip，不能简化成同步直调**：
  - **Python**（`bindings/python`）：`Agent` 存进 `Mutex<Agent>`，所有跨线程方法走单一
    `with_agent` 入口（`py.allow_threads` 内拿锁——闭包与返回值须 `Send`）。`run()` 释放 GIL
    跑 loop；自定义工具的闭包在 agent 线程执行，用 `Python::with_gil` **短暂重获 GIL** 调回
    Python。`set_cancel(token)` **必须在 `run()` 之前**调（把共享 `Arc<AtomicBool>` 装进
    agent）。cancel 契约：`run()` 永不清标志——复用 agent 多轮的宿主在新任务受理点调
    `clear_cancel()`（绑定已透出），否则上一轮取消后每轮秒回 cancelled。

    **重入即死锁**（P1-2 教训）：回调里再调 agent 方法会 GIL↔Mutex 互等——run 期间的
    agent 方法调用经 `IN_RUN` thread-local 判定，报 "agent is running" 而非挂死；
    两个线程各自调方法也要靠 with_agent 的锁排队，别绕过它直拿 mutex。
  - **Node**（`bindings/node`）：`run()`/`compact()`/`chat()` 在**专属 `std::thread`** 上跑同步核心（**无
    tokio**），Promise 经 napi `JsDeferred`（`Send`，任意线程可 settle）结算，事件经 `ThreadsafeFunction`
    回主线程。

    **绝不改回 napi `AsyncTask`/libuv 池**（外部评审教训）：agent 线程会阻塞等 JS 工具/provider
    round-trip，而被包装的 JS 回调把自己的异步活（`fs.promises`、timer、Promise continuation）也排进同一
    libuv 池——并发 run 数 ≥ 池线程数（默认 4）时全员互等死锁；专属线程与 `UV_THREADPOOL_SIZE` 解耦。
    线程用 8 MiB 栈（对齐原 libuv worker 默认）并经 `catch_unwind` 兜底——panic 也 settle Promise
    （release 是 `panic = "abort"`，仅 debug 有效）。自定义 JS 工具是 **TSFN round-trip**：agent 线程把 `{id,args}` 经 TSFN
    派给 JS、阻塞 `mpsc` 等结果；JS（`wrapper.js` 把 `run(args)` 包成 `(err,req)` 形状）调完用户函数后调
    导出的 `resolveTool(id, result)`，经全局 `PENDING_TOOLS: i64→Sender` 注册表唤醒 worker（120s
    `recv_timeout` 兜底）。

    **JS 主线程在 `run()` 期间必须空闲**（调用方 `await`），否则 TSFN 回调排不进
    主循环 → worker 永久阻塞 → 死锁。

    **同步方法（pushUser 等）在 run 期间返回 busy 错误**（`try_lock`
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
- **绑定 `push_user` / `pushUser` 的块形状有两种，别按一种写死**：`Block` 是外部标签 serde 枚举
  （`{"text": "…"}` / `{"image": {"media_type", "data"}}`，`messages()` 读回的就是这个），而宿主的自然
  写法是 Anthropic 风格的 `{"type": "image", …}`。绑定两种都收（serde 形归一化到 tagged 形后再走同一
  解析器），否则「把历史块原样回推」会在第二个形状上炸。**一个块对象只解释一种形状**：serde 形同时带
  多个形状键、或 tagged 形与另一种形状键并存，一律明确报错（取第一个会静默丢掉宿主的其余内容）；
  `text` / `image` / `document` 的值类型不对也要给精确错误。`tool_use`/`tool_result` 两种形状都要给
  「agent-loop internal」的精确拒绝，而非泛化的形状错误。
- **napi 的 TSFN 构造函数对非函数是进程级 abort，不是可 catch 的异常**（`Assertion failed: (func)
  != nullptr`，rc=134；`new Agent` / `Agent.create` / `setHooks` / `chat` 四入口全中）。所以 hooks
  对象的 `beforeTool` / `afterTool` / `onChatRequest` 必须在**建 TSFN 之前**用 `JsUnknown::get_type`
  验成 `Function`（`null`/`undefined` 视为缺省）；wrapper 侧同理（`typeof fn !== 'function'` 当场
  TypeError，不能静默丢弃）。自定义工具的 `run`、provider 的 `chat` 同款——`wrapTools` 现在对
  tools 非数组、`run`/`schema`/`name` 形状错误都同步 throw（旧行为：非数组静默禁用全部工具、
  非函数 `run` 直接 abort、非对象 `schema` 原样发给 provider）。
- **napi class 实例只能在 JS 线程 materialize**：`REGISTERED_CLASSES` 是 thread-local，worker 线程里
  `into_instance` 拿不到类注册。`Agent.create` 因此把装配放 worker、只把**数据**交回，由
  `env.create_deferred::<Agent, _>` 的 resolver（JS 线程）构造实例；wrapper 再
  `Object.setPrototypeOf` 换回子类原型，onEvent 保护才不会丢。`JsDeferred` 的 `resolve` 是在 JS 线程
  跑 resolver 的，别把它当成可以随便跨线程调用的回调。
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
  **唯一有意的 TUI 例外是终端断开监控**：crossterm 0.29 Unix event source 遇到 tty EOF 会在自己的
  read loop 内打转（零字节 read 不 break、非 WouldBlock 错误被吞），主线程甚至无法从 `event::poll`
  返回处理错误，pty 关闭后即 100% CPU。独立 `tui-tty-watch` 线程用 `libc::poll` 探测 HUP/ERR/NVAL，
  不消费输入；断开后置 cancel + shutdown + 发送 `None`，有界等 worker 退出、显式恢复终端，再
  `process::exit(0)`。这不是可随手推广的退出方式：只有主线程无法正常返回时才使用，探测未知错误
  不得杀健康会话。pty 测试的 finally 要清理**整个进程组**，且允许进程已自行退出（killpg 可能
  报 ProcessLookup/Permission）；失败清理只杀 demo 父进程会留下 tiny-agent 孤儿并复现此问题。

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

  **轮内取消（2026-09 审核 #9）**：`McpTool::run` 把 `ctx.cancel` **随单次调用**传入传输层（不是 set_cancel
  传播），stdio/SSE 的响应等待按 ≤50ms 薄片轮询（`recv_cancelable`）——Ctrl-C 在卡死 server 上不再等满
  120s `CALL_TIMEOUT`；取消后 pending 槽由既有错误路径回收、迟到响应被 `pending_dispatch` 丢弃。HTTP 传输
  2026-11 起同款可取消：直接在 POST 响应体上应答的那次交换（`run_direct`）搬到 worker 线程
  `mcp-http-exchange` 上跑、调用方薄片轮询，202 应答走 pending 表的通道等待——例外只剩 reader 线程起不来时
  的一次性 GET 兜底（不可取消）。被取消的在飞请求另发 `notifications/cancelled`（尽力而为、1 s 上限；超时
  不发——超时的调用 server 可能已经做完）。`run`/`set_shutdown` 共享 client 锁的顺序依赖
  （set_shutdown 可能短暂排在一个在飞调用后面，由切片轮询兜底）注释钉在 `tools/mcp.rs`。
- **`serde_json::json!` 宏按所有权取值**：`From<&Value>` / `From<Option<_>>` 不存在。给宏传
  `&String`/`&Value` 要 `.clone()`；`Option<&str>` 要显式转成 `Value::Null` 或 `json!(s)`。
- **Anthropic 不允许相邻两条 user 消息**（400）；且 N 个 `tool_use` 必须配 N 个 `tool_result`。
  `agent::append_user_turn` 会把新 user 内容折进已有的末尾 user 消息——新增 user 入口走它，别直接 push。
  它的另一个分支（开新 user 轮）同时结束上一轮的推理寿命（`compress::strip_reasoning`），所以 **Agent
  存在之前就要拼历史**的宿主也得走它（2026-11 起公开）：CLI 的 `--resume --task` 曾手抄这两个分支、漏了
  剥除，上一轮的思考块被带进新一轮的每个请求——`scripts/e2e_anthropic_wire.py` 的多轮场景抓到的。
  另有兜底：`agent::run()` 每轮发请求前 `normalize_adjacent` 把相邻同 role 合并（保块顺序，不动配对），
  防 resumed/手改的损坏历史触发 400。别依赖它当正常路径，正常仍走 append_user_turn。
- **cancel 契约（TinyWorker 反馈落地）：run() 永不清，受理方清残留**。cancel 是电平标志、归嵌入方
  所有；`store(true)` 全库无自动复位，`agent::run()` **也不清**（旧入口 `store(false)` 已删——它会把
  宿主受理窗口内（懒构建可达数秒~15s）按下的停止当残留误清，用户停止被静默丢弃）。预置 true 的 run
  在循环首检即返回：`cancelled`、turns==0、零 provider 请求。清残留的主体责任在**受理新任务的一方**：
  `Agent::clear_cancel()`（pub，绑定层 `clear_cancel`/`clearCancel` 已透出）。

  **破坏面**：嵌入方「set_cancel + 复用 agent 多轮」漏改的症状是每轮
  秒回 cancelled（确定可见、`turns==0` 可机判），不是静默。库内**真正调用** `clear_cancel` 的只有两处：TUI
  `run_once`（每条提交输入/初始 auto-run）与 `compact()` 入口；`harness::run_with_retries` / `batch`
  重试梯的 attempt 间判定是**终态出口**——cancel → `EXIT_CANCELLED`，不重试**也不清**（清了即复活
  用户已否决的 run；batch agent 本就不装 cancel flag，行为零变化）。

  **gate→执行检查点**（2026-09 外审 P2）：`before_tool` 审批等待可以无限期，窗口内落下的 cancel 若不复查就出现"run 报
  cancelled、被批准的工具照跑"——`agent::ctx_cancelled()` 在 gate 返回后、执行前复查，三处
  dispatch（`execute_tool_call` / 并行批 spawn 前 / run_code 内层）共用；顺序路径产出
  `[cancelled by user]` 错误槽 + `after_tool(executed=false)`，并行路径整批不启动（槽位 Cancelled
  合成），run_code 内层复用 `CANCEL_HALTED_WHY` 墓碑。

  **波次间 + worker 预检**（第三轮 P2）：并行批按 8/波，每波开波前复查 cancel、worker 进工具体前再查（返回 None → join 补
  错误槽 + after(executed=false)）。

  **回调 panic 防死锁**：并行批的 `sync_channel` 必须在 `thread::scope` 闭包**内**创建——`rx` 借自闭包外时，drain 循环里宿主回调 panic 的 unwind
  不销毁 `rx`，worker 堵死满漏斗、join-all 死锁；闭包内创建 + `CancelOnUnwind` 守卫翻 cancel
  收尾。测试组：`run_with_preset_cancel_returns…` /
  `run_does_not_clear…` / `clear_cancel_lets_the_next_run_proceed` /
  `parallel_children_both_honour…` / `single_run_cancel_during_failed_attempt…` /
  `cancel_during_approval_gate_does_not_execute_{sequential_tool,parallel_batch}` /
  `cancel_between_waves_never_starts_the_next_wave` /
  `parallel_event_sink_panic_unwinds_instead_of_deadlocking` +
  绑定两侧回归。
- **批量任务片段只能覆盖** `task`/`system_prompt`/`model`/`max_tokens`/`reasoning_effort`/
  `inject_environment`/`salvage_on_max_tokens`/`limits`（逐字段钳制）/`code`（逐字段钳制；
  `code.enabled` 与 `code.llm.enabled`/`code.llm.default_model` 仍拒绝——不可信片段不得开关沙箱
  或改 LLM 通道）；安全字段
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
  解构也是穷尽的。内核之外还有三处要跟着改（评测装置的 clamp 镜像、两份 README、指纹），见下面
  「给 `limits` 加键的四处同步」。
- **批量 `--retries` 只重试 transient**（`stop_reason == "error"`），指数退避 1..32s 封顶；
  `bad_task`/`max_turns` 等确定性失败不重试。transient 判定是 `RunError` 上的小型分类
  （`is_transient`，单跑梯与 batch 共用）：provider HTTP 拒绝带结构化状态码（`ApiStatusError`，
  Display 仍是历史文案）——401/403/404/400/422 为确定性 `request_error` 不重试，429/5xx 与
  未识别错误（网络抖动、自定义 provider）保持 transient；run 截止后失败的请求按 `timeout`
  （exit 124）归属、不进梯子。**重试 = 整任务从头重放**（每次重试新建 Agent），前轮工具副作用
  不回滚、只照实累计账单——副作用型任务需幂等或别开 `--retries`。`--failed` 重跑失败子集后
  **合并**进完整报告（未重跑的历史成功项原样保留）。
- **batch workdir 隔离（`--isolate none|copy|git`，`batch::IsolateMode`）**：`none` 默认共享
  workdir（jobs>1 打 ⚠）；`copy` 每任务拷贝（排除 `target`/`node_modules`/`.git`/`results`），符号
  链接**自包含策略**（`copy_entry`）：源树内相对链接复刻 / 绝对在内拷内容 / 出根（绝对或 `..` 逃逸）
  跳过 + 告警——副本里不允许有通往原树的路（绝对链接指回原 workdir 会把并行任务重新耦合到共享
  目录），包含判定复用 `tools::confined_to`，自引用解引链深度封顶 64；
  `git` 每任务 `worktree add --detach`（要求 workdir 是 repo 根，canonicalize 比较——macOS
  `/tmp`→`/private/tmp` symlink 会让字符串比较误拒；否则 `setup_error` 不重试）。
  **沙箱目录创建必须 `create_dir`（非 `_all`）+ 计数重试**（2026-09 外审 P1）：名字
  `ta-batch-<任务名>-<pid>-<n>` 可预测，多用户共享 /tmp 可按名预置 symlink；`create_dir_all` 的
  "已存在"检查会**穿透符号链接 stat** 而照常成功——整个 workdir（含 `.env` 等 COPY_EXCLUDE 拦不住
  的秘密）被拷进攻击者目录、任务在攻击者可控路径执行。`create_dir`（裸 mkdir(2)）不 dereference
  末组件、预置路径一律 `AlreadyExists`，与 `harness::atomic_write` 的 O_EXCL 同款纪律；git 模式同样
  **先自建空目录再让 worktree add 填充**（git 接受已存在的空目录）。另：**单个坏 JSON 任务文件是
  per-task `bad_task`、不中止整批**（2026-09 外审，与 forbidden 覆盖同哲学）——坏解析经
  `write_terminal_result` 单出口落 results 文件 + 预置进 report 流，好任务照跑。
  **per-task、
  跨 attempt 复用**（重试看得到前次落盘，与共享语义一致）；`IsolationGuard` Drop 清理（copy
  remove_dir_all；git `worktree remove --force` 失败回退 remove_dir_all，孤儿用 `git worktree
  prune` 回收——release `panic=abort` 下 Drop 不保证）。产物随目录丢弃是**评测语义**（审计面是
  result.json/report.json）。jobs>1 ⚠ 只在 isolate==None 时打。**隔离测试的任务名必须测试内
  唯一**：沙箱目录命名是 `ta-batch-<任务名>-<pid>-<n>`，任何按前缀统计共享 temp 目录的断言
  （如 leftover 清理检查）都会被同进程并发跑的另一个 batch 测试的在飞沙箱干扰（外审第四轮
  实遇：并行首跑挂、串行复跑绿）——任务名带测试专属前缀（如 `isocleana`）再断言；**scratch 根
  目录同理**——两个 cancel 测试曾共用 `ta-batch-cancel-<pid>` 根互相清/写对方 results 而双双
  假红（2026-09 外审期间实遇），每个测试用独立根名。**harness 的 `write_result` 测试 2026-11
  中了同一招**：两个测试共用 `ta-res-fp-<pid>`，一方收尾 `remove_dir_all` 恰好撞上另一方
  `atomic_write` 里的 `create_dir_all`（mkdir 报已存在、复核 `is_dir` 时目录已被删）→ 偶发
  `File exists (os error 17)` 假红。**只按 pid 键的临时目录名一律再带 `line!()`**（同文件
  `ta-attrib-…` 的写法）；并行测试偶发红先查共享目录名，别只重跑一遍变绿就算了。
- **`--isolate git` 的 `git worktree add` / `remove` 必须全进程串行——并发互踩是上游 git 的竞态
  （2026-11-28 CI 实遇）**：推送 `a2a5417` 后的 CI（run 46094462662）11 绿 1 红，红的是
  `test (macos-latest)`：`batch::tests::isolate_git_gives_each_task_a_worktree_and_keeps_repo_clean`
  在 `both worktree tasks succeed left: 1 right: 0` 上挂了，`[b]` 报 `isolation setup failed: git
  worktree add failed: Preparing worktree (detached HEAD af37f62)` / `fatal: failed to read
  .git/worktrees/ta-batch-a-12580-5/commondir: Undefined error: 0`——**b 的 add 死在读 a 的 admin
  目录上**：两个 worker 同时在同一个 repo 上 `git worktree add`。**根因在上游**：git 写新 worktree
  的 `.git/worktrees/<id>/commondir` **不是原子的**（`builtin/worktree.c` 的 `add_worktree` 调
  `write_file` → `wrapper.c` 的 `write_file_buf`：`xopen(O_WRONLY | O_CREAT | O_TRUNC)` →
  `write_in_full` → `close`，2.40.1 与 2.56.1 同款），文件有一瞬间是空的；而**每条**
  `git worktree add|remove|list` 开头都要枚举仓库里**全部** worktree（`get_worktrees()` →
  `refs_compute_filesystem_location` → `setup.c` 的 `get_common_dir_noenv`），后者 `file_exists()`
  通过后 `strbuf_read_file() <= 0` 就 `die_errno`——空文件即致命（errno 还是 0，macOS libc 的
  `strerror(0)` 就是 `Undefined error: 0`）。`git worktree remove` 同样会踩（并行 remove 撞上
  **正在删**的邻居则报 `No such file or directory`：先查存在再读的竞态），失败即留下孤儿 admin
  条目。**证据链**：① CI 日志（runner 镜像 macos-26-arm64，Git 2.56.1）；② git 2.56.1 源码（上面的
  四处）；③ 本机 git 2.40.1 上确定性模拟——把邻居的 `commondir` 截成 0 字节，`worktree add` /
  `remove --force` / `list` 全部同文案退出 128；④ 纯 git 自然加压（600 轮 × 4 路并行 add + 4 路
  并行 remove，同一个 repo）：**add 失败 7 次、remove 失败 1 次**，文案与 CI 一致。TinyAgent 那条
  测试本身本机单独循环 300 次没复现——窗口很窄（按加压比率粗估每次只有千分之几，300 次不出并不
  反常），**偶发率低不等于没缺陷**：`--isolate git` + `--jobs ≥ 2` 是真实产品缺陷。既有的「stderr
  含 `lock` 才重试」闸门救不了它（文案里没有 lock，`setup_error` 属确定性失败、重试梯不重试）。
  **修法**：`batch::WORKTREE_LOCK`（进程级 `Mutex<()>`）包住**整条** `git worktree add`（含
  checkout 子进程——致命的枚举是 add 的第一步）与 `IsolationGuard::drop` 里的整条
  `git worktree remove`；锁中毒照常接管（持锁者 panic 留下的是 git 的磁盘状态、该交给
  `git worktree prune`，不是下一个持有者不能看到的不变量）。**不覆盖（明示）**：别的进程在同一
  时刻对同一 repo 增删 worktree、模型在任务 run 里自己敲 `git worktree …`——仍是上游的竞态；
  实测到的平台只有 macOS（空文件窗口的机制与平台无关，文案随 libc 变）。否掉的方案：
  `--no-checkout` 两阶段 + 单独 `reset --hard`（丢 `post-checkout` 钩子语义）、按 commondir 文案
  重试（治症状不治因）。**回归测试怎么观测**
  （`worktree_add_and_remove_are_serialized_across_workers`，`#[cfg(unix)]`）：repo 本地
  `core.hooksPath` + `post-checkout` 钩子在**每条 `git worktree add` 内部** `sleep 0.2`（确定性地
  拉宽窗口；真实 git，不动 PATH / 环境变量），钩子往日志写 `begin` / `end`——回放的嵌套深度必须
  恒为 1（add 对 add）；第二段钩子睡醒后看「已收工 worker 的 worktree 的 `.git` 还在不在」
  （`kept` / `gone`，remove 对 add）。不串行时两段都是**确定性红**（remove 路径换一把锁即红），
  所以不用时间阈值断言（那种会假红）。改 `prepare_isolation` 的 git 分支或 `IsolationGuard::drop`
  的 git 臂之前先读这条。
- **批量 Ctrl-C 是 worker gate，不中断在跑任务**：`batch::run` 的 worker 在**起任务前**检查共享
  cancel 标志，命中则标 `cancelled` 跳过（首个命中的 worker 打一次性契约警告）；在跑的任务照常跑完
  （"收敛"）。**例外（A5，既定取舍）**：每个 agent 都用 `set_shutdown` 挂了同一个 flag 的 reaper——首次
  Ctrl-C 也会**立即杀掉在跑任务的 MCP server**（防二次 Ctrl-C 硬退留孤儿子进程，0.3.0 修复的另一半）；
  依赖 MCP 的在跑任务可能因此提前失败。**没有**把 cancel 标志传给
  agent 中断流式——保持「在跑的任务跑完（收敛）、产出照常落盘」的语义（旧论据「与 run() 入口复位
  竞争」随 cancel 契约改版消失，但不传 flag 本身仍是既定取舍：传了就变成中途打断在跑任务）。
  要立刻退出按第二次 Ctrl-C（`_exit(130)`）。
- **子 agent 与 `run()` 在 cancel 上已无区别**（cancel 契约改版后两个入口都不清标志，只剩 budget/
  deadline 安装差异：`run_as_subagent` 继承父的闸门不重装）。子 agent 继承父的 cancel Arc，任何入口
  清掉共享标志都会丢掉整批的 Ctrl-C——测试 `run_as_subagent_does_not_reset_a_set_cancel_flag` 与
  `parallel_children_both_honour…` 钉住。**别给任何 run 入口加回清残留**。
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
  **重发共享同一绝对截止时间**（2026-09 外审 P2）：`chat()` 入口把 `per_request_timeout` 折成
  `Instant` 截止，每轮 400-重发只花 `remaining()`——旧实现每次 `post` 都拿满原始额度，1s 预算实测
  两个 ~700ms 响应照样"成功"（超时 1.44s）。重发前另查 cancel：等待 400 往返期间用户已取消时不再
  发新请求（Err 交上层重试梯按 EXIT_CANCELLED 收尾）。改这段别把 `remaining()` 换回
  `req.per_request_timeout`。
- **流中途错误的诚实成本形状（两 provider 对齐，改流解析必读）**：openai 与 anthropic 的
  `parse_loop` 对读失败（`StreamError`）与 `{"error":…}` 事件都返回 `Ok` + 尾附一条
  `[stream error] …` 标记文本块 + `stop_reason:"stream_error"` + **已上报的 usage**——
  agent.rs 的 stream_error 提取路径据此把前缀转成可重试 `RunError` 并计费。新增流错误路径照这个
  形状，**别 bail!**（会丢掉网关可能已发的 usage，破坏"失败也计费"契约）。**截止已过时按
  `timeout` 归属**（2026-09 外审 P2）：Ok+stream_error 也要过 Err 臂同款的 deadline 判定——
  否则 deadline 到点触发的读超时会以可重试 Err 爬重试梯（实测 1 次重试 = 2 个请求 / ~2.8s /
  双倍账单、exit 1 而非 124）；转为 `stop_reason:"timeout"` 终态时 usage 已在上一行 bill 过，
  不再额外处理。Anthropic 另有
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
- **MCP 改动的测试与验证（2026-11，2.4 的教训；行为不变量见 architecture「MCP 客户端契约」）**：
  ① **改完一项就跑全量 clippy 组**（`./scripts/check.sh clippy`）：只跑内核 crate 会漏掉 CLI crate 里的新
  lint（测试里的 `useless_format`、TUI 臂里的 `clone_on_copy` 各漏过一次，到四配置门禁才红）。
  ② **装 diag sink 的测试先拿 `TEST_LOCK` 再装**：进程级 sink 是全局单例，`DiagCapture` 构造时先锁后装，
  断言只看含针的行（并发跑时别的测试的 diag 行会混进来）。
  ③ **往大文件测试块尾部 splice 用 `rfind` +「锚在文件尾」断言**：`    }\n}\n` 在 `mcp.rs` 里出现几十次，
  首匹配会插到文件中段。
  ④ **真实进程 mock 的坑**（`scripts/e2e_mcp.py`）：每个场景用新日志文件（追加写会把上一场景的行带进断言
  ——SIGINT 早于 handler 安装、握手序列翻倍都是这么出的假红/假绿）；只答 202 的 mock 必须等 standing GET
  连上再推送（否则回复落空、调用方坐满超时——顺带暴露了客户端 POST 前布防的竞态窗口，architecture ⑤）；
  OpenAI 格式的 user `content` 可能是块数组不是字符串；「调用已到达 server」要认**到达时**写的日志行
  （慢调用只在完成时记一行的话，Ctrl-C 会早于 server 真正开工）。
  ⑤ **轮转间隙里的请求**（2026-11 修掉；此前记为「已知偶发」）：`legacy_sse_post_follows_the_fresh_endpoint_after_rotation`
  全量并发跑时偶发「tools/list timed out」或 404、单跑稳定。根因在产品代码而不在测试：读线程到期轮转时
  丢掉旧 GET 再开新连接，`post_url` 只在新连接的 `endpoint` 到达时才刷新——这段间隙里发出的请求被 POST 到
  旧会话；会话绑定的 server 靠**定时器**回收会话（python-sdk 每 100ms 轮询，不是断开瞬间），这种 POST 照样
  被应 202、应答写进已关闭的连接，调用方等满 CALL_TIMEOUT。测试只是碰巧把请求落进了间隙（持 `TEST_LOCK`
  等多久不受自己控制）。修法在传输层：轮转**先退役** POST 目标（置 `None`）再丢连接，`post_target` 等新
  `endpoint`（界 = connect 键，看得见 cancel 与死亡 latch；取消通知不等）；mock 加了「重连的 `endpoint`
  延迟 / 会话在 GET 关闭后存活多久」两个旋钮（`mock_legacy_session_bound_endpoint_with`），间隙可以被测试
  走进去。**教训**：时序型偶发先当产品缺陷查根因，别只给测试加重试或余量；「server 在连接断开瞬间回收
  资源」是没验证过的假设。仍然存在的相邻限制：**已经出门**的请求跨过轮转边界时，应答依赖 server 在新
  连接上重发——不重发即丢失、调用方坐满 CALL_TIMEOUT（模块顶部 `STREAM_ROUND` 注释明示的取舍；要关掉得
  「排空在途请求再轮转」，未做）。配套新测试踩过的坑：会制造 diag 告警的测试若不持 `TEST_LOCK`，告警会漏进
  当时正持锁装汇的测试（`http_standing_stream_rotations_stay_silent` 断言零告警且不过滤）——扣住**响应头**的 mock
  让每次重连在连接阶段超时，走的是「重连失败」告警路径；mock 现在立刻发响应头、只扣 `endpoint` 事件（读阶段超时
  = 静默轮转）。新测试单独跑一遍、数一数 stderr 上的 `[warn]` 行再加进套件。
- **`#[cfg(windows)]` 后的代码本地编译不到——加字段/改签名后做一次 Windows 目标交叉检查（2026-11，内核改进计划
  2.4 的教训）**：给 `McpServerConfig` 加四个字段后，本机 fmt、四套 clippy、全部测试全绿；到 Windows 目标上
  做交叉检查才发现 `mcp.rs` 三个 `#[cfg(windows)]` 测试（`stdio_drop_on_windows_…` / `windows_reaper_kills_…` /
  `windows_set_shutdown_twice_…`）的 `McpServerConfig { … }` 字面量没补字段——CI 的三平台矩阵 job 第一步就是
  `cargo clippy --all-targets`，推上去 Windows 格当场红（同批 2.5 给 `StdioTransport::spawn` 加参数时这三处
  **调用点**改了、字面量漏了）。它与 feature 盲区同构，只是本地没有对应的「第五个配置」。**规则**：加字段/改签名后，
  `cfg(windows)` / `cfg(not(unix))` 块里的字面量与调用点单独过一遍（`grep -rn 'cfg(windows)' crates src bindings`
  定位块，再看块内构造点），并跑一遍下面的交叉检查。
  **配方**（不装任何工具链；只做类型检查 + lint，**不是实跑**——Windows 测试仍只在 CI 上执行）：本机只有 zig 时
  `ring` 的 build script 会先倒——cc-rs 把 Rust 三元组原样传给 zig（`--target=x86_64-pc-windows-gnu`），zig 只认
  `-target x86_64-windows-gnu`。临时目录放一个包装脚本，只改写这一个参数后转给 zig：
  `args=()`；`for a in "$@"; do case "$a" in --target=x86_64-pc-windows-gnu) args+=(-target x86_64-windows-gnu);;`
  `*) args+=("$a");; esac; done`；`exec zig cc "${args[@]}"`。然后
  `export CARGO_TARGET_DIR=<临时> ZIG_GLOBAL_CACHE_DIR=<临时> ZIG_LOCAL_CACHE_DIR=<临时>`（别污染日常构建目录
  与 zig 缓存，收尾一并删）、`export CC_x86_64_pc_windows_gnu=<包装脚本> AR_x86_64_pc_windows_gnu="zig ar"`，再把
  CI 四套 clippy 原样加上 `--locked --target x86_64-pc-windows-gnu` 跑一遍，外加
  `--features tui,code,mock --all-targets` 一套把 mock 门后的测试也编到。CI 的 Windows 格是 msvc 目标；第一方
  代码没有 `target_env` 分支，gnu 目标对 `cfg(windows)` 代码等价（阳性对照：往 Windows 专属测试里放一个未用变量，
  这套 clippy 会以 `unused_variables` 失败——说明它真的在 lint 这部分代码）。绑定 crate：Python 另设
  `PYO3_CROSS_PYTHON_VERSION`（如 `3.12`）；Node 的 napi-build 在 Windows 目标上无条件找 `libnode.dll`（找不到就
  panic、没有跳过开关），本机没有 win32 的 node 库时把 `LIBNODE_PATH` 指向一个放了同名空占位文件的临时目录即可
  （`cargo clippy` 不链接，只过它的存在性检查）。类型检查过了不等于行为对——cmd.exe 的运行时语义交叉检查
  管不到，见下文「CI-only 的门，修完要对着日志逐格复核」条。
  **第二种触发——没碰任何 `cfg(windows)` 块也中招（2026-11-27，TUI 重构的教训）**：重构把 `app.rs`（2.3k 行）
  里的终端守卫拆进新的 `tui/terminal.rs`，`use std::time::{Duration, Instant}` 跟着搬了过去，而那个文件里
  只有 `#[cfg(unix)]` 的函数用它。本机四套 clippy 全绿，CI 的 `test (windows-latest)` 在
  `cargo clippy --locked -p tiny-agent --features tui --all-targets` 当场红（`unused imports: Duration and
  Instant`），Windows 的三组测试整组被跳过。**规则要读宽**：要跑交叉检查的不只是「动了 `cfg(windows)`
  块 / 给内核结构体加字段」，还包括**任何让一个 import 的使用方全部落进 cfg 门的改动**——拆文件、搬函数、
  给函数加 `#[cfg]`；自查方法是逐个 `use` 问「去掉 `cfg(unix)` 代码后还有没有人用」。修法是给 import 本身
  加 `#[cfg(unix)]`（自成一组，前面留空行，rustfmt 不会把它并回去）。配方不变，补两条实操：① shell 里
  用不了环境变量前缀时，把 `CC_…` / `AR_…` / zig 缓存目录连同 `[build] target-dir` 写进一个临时配置文件
  （放被忽略的 `target/` 下），`cargo clippy --config <文件> --target x86_64-pc-windows-gnu …` 加载——
  **`--config` 必须写在子命令之后**：写在 `cargo` 和 `clippy` 之间会被外层 cargo 吃掉、不传给内层的
  `cargo check`，配置静默失效、构建落回日常 `target/`（交叉目标下要到 `ring` 的 build script 报
  `failed to find tool "x86_64-w64-mingw32-gcc"` 才看得出来）；② `x86_64-pc-windows-gnu` 的标准库只装在
  本机默认工具链上，CI 同版的 1.100.0 没有（装目标属于装工具链，不做）——用默认工具链跑即可，
  `unused_imports` 这类与工具链版本无关。
- **思考内容展示策略**：`ThinkingDelta` 在 `harness` 的 `--json` NDJSON 发 `{"type":"thinking","delta":...}`；
  headless 终端打印写 **stderr**（不进 stdout 的 `summary`，可 `2>/dev/null` 屏蔽）；TUI 渲染为独立
  独立色斜体 `✻` 区块（`Row::Thinking`，不走 markdown/MdCache，与答复视觉分离）。**Thinking 不走
  Assistant 的提前分块定稿**：live 只画有界尾部，关闭后整块提交；超过 8 个显示行折叠为头部 + 末尾
  3 行，`/thinking [n]` 用 `Row::ThinkingFull` 追加重印完整原文，`/export` 始终拿完整原文。别为了
  复用 Assistant 切分把前文提前塞进不可变 scrollback——那样最终无法再折叠。
  子 agent 的思考经 `AgentEvent::SubAgent` 整体包装转发（`subagent.rs` 不逐变体 match），harness 的 SubAgent 内嵌
  match 单独把它写 stderr 带 `[name]` 前缀。
- **TUI 显示宽度必须按 grapheme，不是按 `char`**：composer、最终 wrap、display-row 测量与截断共用
  `LayoutCell`（`unicode-segmentation` + `unicode-width`）；`👩‍💻`、`❤️` 等 ZWJ/variation-selector
  序列逐 char 拆会分字、错光标甚至切出残字。`InputBuffer.col` 仍是 UTF-8 byte offset，只有
  `layout_composer` 的 source→cell 映射能决定视觉 caret；exact-full-width、drop-space 两侧与窄屏
  continuation 要跑真实 TestBackend cell-grid/cursor 测试，别用字符串宽度目测。**角色/工具标识变更
  必须同步 `render.rs::continuation_prefix`**：`◆` 替换 `●` 时若漏掉这里，长回答的续行会丢掉两列
  悬挂缩进；用真实 `wrap_line` 钉子覆盖新符号，不能只改首行快照。
- **TUI 滚动区画面疑似重叠要用真实终端模拟器交叉核对**：`insert_before` + DECSTBM scrolling-region
  操作可能让 pyte 重放出 composer、工具行与状态行互相覆盖的假画面；这不足以证明产品回归。真实 Kitty
  重放同一流程可对 pyte 结果证伪。视觉验收应同时保存真实终端的 screen/scrollback 文本与样式捕获，
  再检查 cell grid；不要只凭一次 pyte 整屏文本或“看起来对”下结论。 Kitty ANSI 的 colon-form indexed
  SGR（如 `48:5:234`）若交给 pyte 二次渲染，需先规范化成 `48;5;234`，并按 CR/LF 喂入视觉行，避免
  截图渲染器自己制造颜色或折行伪影。**根因（2026-11 定位）**：pyte 0.8.2 根本没实现 `CSI S`（SU）/
  `CSI T`（SD）——`csi['S']` / `csi['T']` 是 `None`，落进 debug no-op；而 TUI 提交 scrollback 靠的正是
  DECSTBM + SU（ratatui `scrolling-regions`），所以 pyte 里视口上方的行永远不上移、每个已提交块都盖在
  顶部几行上——假重叠是模拟器的缺口，不是产品的回归。补法见文末「pyte 重放 TUI 的三个假象」。
- **Assistant soft-cut 的放弃路径不得改原文**：先验证 EOF 空尾、全空前缀，再一次性 `split_off` /
  `truncate`；先 pop 换行再 return 会让 `"\n\n"`、空 fence 等流式内容静默丢字节。fence 状态只看
  切点前的完整逻辑行（半个 `"```ab"` 不是 closing fence），未闭合 inline code 内不切，closing fence
  滑到 EOF 不切。Markdown 已按正文宽度预折行，**不要再把裸 `◆ `/continuation 宽度重复计入 soft-cut**；
  正确性用真实 `build_history_lines → wrap_history` 的切分前后逐行等价测试钉住。
- **TUI 队列不能做成 transcript ghost**：`VecDeque<String>` 是唯一真值，queue preview 仅 render-only；
  真正发送才追加 `Row::User`，否则未发生的输入会阻塞 commit watermark。失败置 Failed + queue_paused，
  只有 `/resume` 释放旧 FIFO；另发新 prompt 不得隐式放行，重复 cancel 不得重复刷状态行。
  **`queue_paused` 必须蕴含队列非空**：失败时队列为空不置位、`/unqueue` 取走最后一条要解除（两处都经
  `pause_for_held` 派生），取消清队直接置 false——残留标志会把后续正常运行中排入的消息误标 paused
  并在轮末扣住。
- **TUI loop 测试夹具必须与 `ui_loop` 同构（2026-11 重构）**：2026-11 之前按键 / 命令 / 事件三个
  reducer 内联在 `ui_loop` 的 ~1460 行里、状态是 ~40 个局部变量，交互行为（队列何时释放、哪种事件收
  哪条流、忙时哪个命令被拒）只能靠单元测试和 pty 抓屏间接覆盖；拆成 `Session` 之后这类行为写成
  `tests_loop.rs` 的 loop 测试（驱动真实 `Session`，不碰终端）。写夹具有两条纪律：① **两个按键
  之间必须重绘**（`popup.update_for`）——`ui_loop` 每轮只取一个终端事件、下一轮先重绘；夹具连发
  两个键时，第一个键清空输入框后弹窗还开着，第二个键就被 `on_popup_key` 吞掉（实发：`set_text`
  之后直接 Enter，命令没跑）。② **断言「发给 worker 什么」之前先排空夹具自己的前置消息**
  （`in_flight()` 的 `go`），否则 `.len() == 1` 数到的是自己的消息。三个容易记错的既定事实：
  空屏判定是 `rows.len() <= 1`（欢迎行是 `rows[0]`，`for_test` 也会放一行）；`shape()` 给 System
  行加 `!` 前缀，所以 `HELLO` 常量带 `!`、原始行文本不带；空闲状态栏文案是 `ready` 不是
  `idle`。**退出路径上改了状态要补一帧**：inline viewport 是释放不是清除，最后一帧留在屏上
  （`view::final_frame`）——从弹窗里直接跑掉的 `/quit` 曾把自己的弹窗行留成残影。去重 reducer 时
  改错收流范围，靠的正是 `commit_advances_to_the_open_stream_and_past_every_final_row` 这类用例
  当场抓出——先有测试再合并分支。
- **TUI 子 agent 文本气泡不切分是既定决策，但它是平方级成本**：live 区对 SubAgent 气泡的每个
  流式 `TextDelta` 都全量重解析整块 markdown（切分/增量渲染留后续）——单气泡渲染成本
  O(气泡字节数²)。实测（bench profile，24KB 文本 / 300 个 delta）：子气泡路径 ≈371ms（≈单次
  全量解析的 190 倍）vs 父路径 ≈1.25ms，外推 200KB 单气泡 ≈25s 纯 CPU。触发条件是**不守「简短总结」契约的
  子 agent**——流式 `TextDelta` 不受 `max_output_chars` 约束、气泡可任意大，`task` 工具的
  「只回简短总结」描述契约就是唯一的闸；给子 agent 塞长输出任务时记得这条成本曲线。
  改 TUI live 区渲染前先读这条。
- **改 `agent.rs` 核心路径必须配套 loop 测试**。测试外置在 `agent_tests.rs`（`agent.rs` 末尾
  `#[cfg(test)] #[path = "agent_tests.rs"] mod tests;` 引入，靠 `use super::*` 仍可达 agent 私有项；
  压缩纯函数的测试靠 `use crate::compress::*`）。用 `provider/mock.rs` 的脚本化 Provider 覆盖不变量：
  cancel 契约（run 永不清 / 预置即取消 / 受理点 clear_cancel）、tool_use/tool_result N:N 配对（含中途取消时合成的 `[cancelled by user]` 进 history
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
- **precheck 检出集合不含 `__tools_list`/`__budget`**（微调支持 FD-9，需求文档 FR-2 表第三行照抄会
  做错）：这两个名字是**可工作的只读内省拦截**（run_code.rs 的 `TOOLS_LIST_NAME`/`BUDGET_NAME`——
  prelude 的 `tools.budget()` 本体就是 `tools.call("__budget", {})`），guest 直写它们运行时**成功**；
  预检拒答它们 = 文案撒谎（「不可达」）+ 误杀合法代码。保留工具检出集合只有 `{run_code, task, todo}`
  （D11 排除集）。同理别给 `tools.call` 加非字面量首参检出——变量拼接在静态语境下真值未知，漏检
  退回运行时拦截是安全方向。预检整体是**效率特性不是安全机制**：混淆写法（`globalThis["Date"]
  ["now"]()`）放行，运行时确定性桩照抛——别把它当安全层依赖。拒答/墓碑文案是微调数据的规格
  （`docs/finetune_support_design.md` 附录 A），改一个字都要先走设计稿修订记录。

- **context_breakdown 的等和不变量靠单趟 running-floor，不是各段独立 /4**（context_report.rs）：
  `estimate_tokens` 对**总和**做一次整除，分解要严格等和就必须按固定顺序一遍累计、每段领
  `floor(acc_after/4)-floor(acc_before/4)` 的商差。各段独立 `chars/4` 再求和会间歇性差 1-2 token
  （短段还会捡前段的余数——2 字符段跟在 19≡3(mod 4) 段后能领到 1），`uncalibrated_total_equals_
  estimate_tokens` 测试是刻意钉子，红了别"修测试"。同坑：精确断言某桶数值时要按同一套 floor 语义
  推期望（或用"总量-其它桶"反推），别拿朴素 `chars/4` 当期望值。
- **`skill_intro_base_chars` 默认值是 `cfg.system_prompt.len()` 不是 0**：语义是"env 之后全是 base，
  其后才是简介"，只有 assemble 路径经 `set_skill_intro_base` 收窄。默认 0 的读法（"没有 base"）会把
  直接 `try_new` / 子 agent 的整个用户 system prompt 错归 skill_intro 段——`env_attribution_splits_
  env_from_base` 测试钉的是默认语义。另记：`build_agent*` 里取 `skill_base` 必须在调 `assemble`
  **之前**（assemble 返回的 cfg 克隆带简介，会遮蔽原名）。
- **锚点失效即校准退化，不是 bug**：fold/prune/MCP 热增删/前缀改写都会让 `live_anchor()` 四条件失配
  → `context_breakdown().calibration` 回 `None`（纯启发式口径）——post-run 主 chat 会重新锚定，这是
  既有语义的顺延。测试"失效"路径用**手动 `compact()`**（run 后锚点在、compact 折叠后锚点失配；
  preflight 压缩后紧跟的主 chat 会立刻重锚，测不出失配窗口）。
- **零 prompt 的 usage 不是测量，不写锚点**（agent.rs 锚点写点）：真实请求不可能零输入
  （system + 至少一条 user 消息必有成本），输入三轴全零的 usage 对象 = 网关没报 prompt usage
  （野见的 relay 缺 `usage.prompt_tokens`、Anthropic 协议网关发 `{input_tokens:0}`）。若照写锚点，
  `scale = 0/raw = 0.0` → `context_breakdown` 报"已校准的全零"（TUI 渲染 `calibrated ×0.00` +
  `window: 100% free`），既有 `estimate_input_tokens` 也退化成 0+delta——一个垃圾输入毒化两个消费者。
  守卫别只盖 `raw == 0` 一端；`zero_prompt_usage_writes_no_anchor` 钉住（输出 token 照常计费不受影响）。
- **lib_api 里别断言"空 workdir ⇒ 无技能"**：`resolve_skill_dirs` 还会读机器级全局技能目录
  （`~/.config/tinyagent/skills`），开发机上装了技能的与干净的 CI 跑出不同结果——环境依赖的断言
  是移植性地雷。"无 intro"语义用直接 `Agent::new`（不经装配）来测。
- **思考型模型的输出 cap 是装置红线：4096/8192 档均实测烧穿，正式跑 32K 起**
  （2026-09-23 产物健壮性 mini 两跑实录）：glm-5.3-flash 外层在 4096 档会把整段
  cap 烧在思考上（逐轮 output 精确 =4096、无 tool call 无正文），salvage nudge
  开火后 salvage 轮同样烧穿 → `max_tokens` 终态假 0；连"传统管线"的逐条
  deepseek-flash 调用都在 4096 档烧穿 8%（23/288 空判）。思考段计入 output
  计费与 max_tokens——**给思考型模型配 max_tokens 时按「思考 ~2K+/条」预留**，
  正式批一律 32768（runner `--max-tokens`）。判读死亡先看逐轮 output 是否精确
  等于 cap（烧穿指纹）。
- **`测试模型.md` 的 `vol-glm-5.3-flash` 是会话别名，不是网关模型名**：网关
  OpenAI 兼容接口直接调它 400「未找到匹配模型」，`/models` 实测只有
  `deepseek-flash` / `glm-5.3` / `glm-5.3-flash` / `k3`。`vol-` 前缀只对
  Claude Code 接入端有效；评测/绑定测试一律用网关名（`v0b_eval.py` 默认值
  即正确）。新会话第一次真模型烟雾门撞 400 时先查这里，别急着改装置。
- **内核改进计划落地时踩到的坑（2026-11；行为契约见 architecture 各「内核改进计划 x.y」条）**：
  - **工具表估算的两个重算点必须同量纲（0.2）**：`refresh_tool_defs` 曾按字节数累加，构造路径用的是 CJK
    感知的 `compress::tool_schema_units`——热增一个带中文描述的 MCP 工具之后，同一个 agent 的两条路径估算
    口径就不一致（中文描述在热增路径上被低估三倍）。现在「重算工具表」只有 `tool_schema_units_sum` 一个函数；
    新增重算入口别再手写 `.map(|t| …len()).sum()`，回归钉子比对的是「热增后 == 一次性构造」。
  - **块标量 ≠ 多行裸标量（0.3）**：`description: >` / `|` 是认的多行形式；`description:` 换行后直接缩进续行
    （YAML plain scalar 续行）**仍不支持**——必填字段读成空 → `Missing` → `[skill]` 告警跳过，不会解析出
    半截值。块头必须**整段**是 `|` / `>` 加至多一个 chomping 指示（`-` / `+`）与一个 1–9 的缩进指示
    （`parse_block_header`）：`description: | # 说明` 这类带尾随内容的行**不是块头**（真 YAML 会当成块头 +
    注释），这里被当成单行标量、值就是那串字面文本——静默降级不报错。归属只看缩进（第一条缩进 ≤ 键缩进的
    非空行就是下一个键），所以 `metadata:` 可以紧跟在折行的 description 后面；chomping 校验后忽略（值恒
    trim）；`description` ≤1024 的上限作用在拼接并 trim 之后的值上。
  - **`tools/list` 翻页：任一页失败 = 握手失败（0.1）**：翻到一半的残缺工具表比没有更糟（模型看得到、
    调用却失败）；单页 server 的请求序列与翻页之前逐字节相同（无 `nextCursor` 即停），重复出现已跟过的
    游标即停并告警，上限 64 页 / 256 工具。改翻页逻辑先看
    `handshake_follows_tools_list_cursor_pagination` / `handshake_stops_on_a_repeated_cursor` 两根钉子。
  - **回归评测装置（0.4）的九条**：① **臂压过 mock 调参**——`build_config` 的叠加顺序是底配置 ← 任务覆盖 ←
    （mock 专用覆盖）← 臂覆盖 ← 任务片段。臂是实验变量、必须说了算；把臂放在 mock 调参之前，「把
    `keep_recent_messages` 压到 0」的臂会被任务的 mock 调参悄悄改回去（先写 pytest 才抓出来的）。
    ② **清单脱敏要进嵌套**：`extra_headers` 在顶层，MCP 的 `headers` / `env` 嵌在 `mcp_servers` 里——只脱
    顶层会漏；`base_url` 与 MCP `url` 要去掉 query / fragment（有的网关把密钥放在那里）。
    ③ **任务片段的 `limits` 按 `Clamp` 语义合并**（只能收紧），`LIMIT_CLAMPS` 是 `config.rs` 里 `Clamp` 的
    镜像——内核那边改了某个键的变体，这边要同步。
    ④ **改写过的 mock 必须真执行一遍**：`mock_anthropic.py` 的一份重写有语法错误，只有 `check`（每个任务 ×
    两种线协议各真跑一遍）抓出来——静态核对替代不了真跑。
    ⑤ **两条通道都要隔离环境，而且不止 `XDG_CONFIG_HOME`**：`[skill]` 的全局查找跟着它走，否则开发者本机
    的真实技能目录会漏进轨迹（CI 与开发机跑出不同的请求；付费通道起初沿用开发者环境，第一次真机批次就
    漏了）。批次目录又在仓库里——workdir 自己不是仓库时 git 会向上找到外层仓库，模型跑一句
    `git status --short; git diff` 就把开发者未提交的改动发给了模型，换成 `git stash` 就是改坏工作区。
    现在 `run_env` 对两条通道一并设 `XDG_CONFIG_HOME`（run 目录内的空目录）与 `GIT_CEILING_DIRECTORIES`
    （run 目录 + 全部祖先；workdir 自己是仓库时照常被发现），三根钉子：配置目录隔离、真实 `git init` 仓库里
    从 workdir / run 目录 / 祖先都找不到仓库而 workdir 是仓库时仍找得到、mock 批次里脚本化的
    `git rev-parse --show-toplevel` 得到 `rc=128`。挡不住的只剩「先 `cd` 到批次的祖先目录再 git」。子进程不继承
    父进程的 `sys.dont_write_bytecode`——mock / setup / check 的子进程要带 `PYTHONDONTWRITEBYTECODE=1`，否则
    仓库里落 `__pycache__`。
    ⑥ **费用停止按「已花 + 下一个 run 的最坏预留 ≥ 停止值」判**：用量报不全的 run 按预留记进已花、绝不当 0；
    `--cost-stop` 只收正的有限数（bool / `inf` / `0` / 负数都当配置错误）；`result.json` 缺失时 `turns`
    记 0，汇总不崩。**停止值还要大于单个 run 的最坏预留**（现价表与 `reasoning-carry` 的 limits 下约 3.9 USD）：
    预留比停止值大时第一个 run 就被挡下——批次目录里 0 个 run、花费 0、exit 0（2026-11-26 给 3.0 被挡、改 7.0 才发车），
    很容易看成「跑完了」。判卷脚本对 `shell` 工具结果数行要记得**首行是 `$ <命令>` 回显**（`shell.rs`）。
    harness 自身的单测（`scripts/tests/test_regress_eval.py`）不进 CI——**动 harness 先写测试再改**。
    ⑦ **文档里写的命令行形状也要排练**：`run <批次目录>`、`prepare --base-config` 按相对路径给出是文档里的
    写法，而零成本 `check` 走的是绝对临时目录，从没走过这条路。2026-11-26 第一次真机批次六个 run 全部在
    启动阶段 `cannot read config file`（子进程的 cwd 是 run 目录，相对的配置文件路径在那里解析不到；没发出
    请求、花费为零）。现在 `run` / `summarize` 把批次目录、`prepare` 把 `--base-config` 都解析成绝对路径
    （清单里记的也是绝对路径），两条用例钉住。一条通道第一次真跑之前，先用**与文档逐字相同的命令行**对
    mock 配方排练一遍。
    ⑧ **mock MCP server 的每个工具都要有一条「合法调用」探针**：脚本化模型只走任务需要的那几个工具，没被
    脚本调过的工具等于没测过——`transfer_stock` 的参数校验要求每个参数都是非空字符串，整数 `units` 被判成
    `missing argument 'units'`，零成本通道一直是绿的，真实模型白撞 25 次（一次 run 的全部工具错误）。
    `test_every_mock_mcp_tool_accepts_a_well_formed_call` 以 `TOOLS` 为真值源，新增工具不补样例即失败；
    整数参数改在工具处校验。
  - **推理回传是旁字段，别把它做成 Block（1.1）**：进了 `blocks` 就会触发「新增 Block 变体四层清单」，
    并让空回复判定 / 零块仲裁 / salvage 探测 / TUI 转录全都看见它。失效规则是「整段剥、永不剥一半」：
    剥 = `push_user` 开新 user 轮 / `set_model` / 相邻同角色合并；不清 = 工具结果并入尾 user 消息 /
    压缩折叠与裁剪。**给 `GatewayFixes` 加位是单向门**：它带 `deny_unknown_fields`，带新位的 preset 旧
    二进制读不了。**「provider 产出」≠「落历史」**：内置 provider 无条件填 `ChatResponse.reasoning`，
    是否落历史只看 `Config.preserved_reasoning`——断言历史里没有 reasoning，不代表 provider 没产出。
  - **`Usage.reasoning_tokens` 为 0 不出键（1.4）**：`Usage` 结构体与 result.json / report.json 侧
    `skip_serializing_if`，NDJSON `usage` 事件却恒出现（harness 手写映射）。任何「这些键恒在」式的断言
    循环都会在它上面翻车；要测它就给 mock 一帧带 `completion_tokens_details.reasoning_tokens` 的 usage。
    它是 `output_tokens` 的子集拆分，永远不要加进总量。
  - **`extra_body` / `extra_headers` 的校验在加载期（1.3）**：`extra_body` 里写内核自有键（`model` /
    `messages` / `tools` / `max_tokens` …；设了 `reasoning_effort` 时还有 `thinking` / `output_config` /
    `reasoning_effort`）是配置错误而不是静默覆盖；报错不回显值（header 值是凭据），`ExtraHeaders` 别派生
    会打值的 `Debug`、别把它打进日志；`Config::fingerprint` 对 `extra_headers` 只哈希头名。
  - **模型窗口提示测纯函数那一半（1.5）**：`Limits::context_window_hint()` / `context_window_line(cfg)` 是
    纯函数；打印半边（`harness::note` → stderr）在 `cargo test` 并发下抓不住也不必抓。**每进程 / 每 run
    一次的 diag 提示别数调用次数**（窗口提示是 CLI 启动时打一次，思考烧穿提示按 run 去重）——测判定与
    文案，不测「发了没」。TUI 侧不走 diag 也不必抓屏：`render::welcome_text(model, n_tools, Some(note))`
    是纯函数，loop 测试直接断言欢迎行带着那一行、且 `rows` 仍只有一行（报头因此保得住）。
  - **MCP 注解是旁表，加字段要扫全部字面量构造点（1.6）**：`McpToolInfo` 多了三个注解与 `output_schema`，
    `ToolCallRecord` 多了 `annotations`，`ToolCtx` 多了 `tool_annotations`——Rust 测试与两个绑定里按字面量
    构造的地方都要补（编译器点名 Rust 侧；绑定侧的 JS / Python 字典不会）。旁表的唯一构造点是
    `annotation_table`（`Agent::try_new` + `refresh_tool_defs` 调），新增改工具表的入口必须过后者，否则
    旁表与实际分发表不一致。`ToolAnnotations::MISSING` 是「server 什么都没声明」的规范默认值，
    `idempotent_hint` 规范没有默认、恒 `None`。**它是 server 的自述**：别让调度或审批把它当成保证。
  - **`on_before_compact` 的三个「不触发」（2.2）**：摘要失败 / 空摘要 / 仅 prune 都没有折叠可通知——把
    钩子挪到摘要之前（「先通知再决定」）或在这些路径上也调一次，都会破坏「观察点 = 每次折叠恰一次」。
    它是唯一 panic 被兜住的钩子（裸 `catch_unwind`——发布构建 `panic = "abort"` 兜不住，实现仍不该
    panic）。TUI 侧两个细节：`{e:#}` 打印 anyhow 包装的 `CompressError` 会把 cause 链打两遍（它既 Display
    成 source、又暴露 `source()`），要对内部的 `c.source` 直接格式化；`/compact` 是 idle-only，忙时的
    命令要留在 composer 而不是被吞掉。
  - **工具表规模提示与 `/context` 必须同数（2.3）**：装配期没有 `ToolsBreakdown`，提示用
    `tool_table_tokens(system, tool_schema_chars)` 现算（system 段之后的 running-floor 份额）——别用
    `tool_schema_chars / 4`（差一个 floor 舍入就会和 `/context` 差 1）。判定是严格大于。提示经 diag 通道、
    每进程按文本去重，所以**同一测试进程里只能观察到一次**：文案断言用 `tool_defs_advisory_lines` 纯函数。
    另（既有瑕疵，未改）：`main.rs` 在 `tui::run` **之前** `build_agent`，装配期的 diag 行（`[skill]` /
    `[config]` / `mcp: server ready` / 本提示）此时 TUI 的 diag bridge 还没装，走 stderr fallback 打进
    未进 raw 的终端，长行被终端按右缘硬折（断词）——所以提示文案每行都远小于 80 列。
  - **新增 spawn 点的凭据卫生清单（2.5）**：凡是工具 / MCP 传输 / 宿主自定义工具拉起子进程，必须
    ① 读 `ctx.hidden_env`（agent 装配时算好的集，默认恰是 `api_key_env` 指向的那一个变量）② 对每个名字
    `Command::env_remove`（两条平台链都做）③ MCP 条目自己 `env` 里显式写出的同名变量保留——
    `.envs(&cfg.env)` 先于 `strip_inherited_env` ④ 补一个「变量在子进程里不可见」的探针测试（真 spawn）。
    **库从不改进程环境**（定位是防顺手不防对抗）：别图省事在装配时 `env::remove_var`——会波及同进程的
    其他 agent 与宿主自己。探针测试同样不改进程环境（测试并行跑）：拿每个测试进程都有的现成变量名
    （unix `HOME` / windows `USERNAME`）当「要藏的名字」传进 hidden 集，先做对照组（不藏 ⇒ 子进程读得到）
    再断言藏了读不到；缺 python 解释器或变量不存在时自跳过。`tools/llm.rs` 里的
    `hidden_env: Vec::new()` 是测试专用代码，不是 spawn 点。
  - **真模型验证的三个内核发现（2026-11-26，§0.0.5；行为契约见 architecture 对应三段）**：
    ① **推理模型的摘要截断**：推理 token 与正文共用 `max_tokens`，三个付费批次的 240 次摘要里 204 次（85%）
    在下限 1024 处截断（199 次带思考）——「截断仍连贯」对带思考的截断不成立（会话文件里的终版摘要在半行
    断掉）。**数摘要调用别用启发式**：以 `compressed` 事件后面紧跟着的那条 `usage` 为准；「不带缓存且
    输出 ≥ 1024 的 usage 就是摘要」会把主对话的长回复算进去（第一次这样数出 252 / 231，其中 7 次是主对话、
    17 次是预算高于下限的调用——复查 artifacts 才纠正，文档里差点留下错数）。重试阶梯见
    architecture，这里记三个易错点：**「带推理」有两个信号**（`resp.reasoning` 有块，或
    `usage.reasoning_tokens > 0`——只看前者会漏掉「只报计数、不流 `reasoning_content`」的网关），而且必须在
    `merge_usage` 合并之前读；**重试的 `Err` 不能直接透传**（手里已有第一段正文时折它——落 `compress_failed`
    会连带关掉之后的溢出恢复），取消是例外；**预算学习挂在 agent 上、跨 run 保留**（`set_model` 才清），
    别写成「每 run 重置」——TUI 的同一个 agent 跑几十个 run，每 run 重学等于每 run 先付一次截断的摘要。
    ② **折叠抖动与自适应保留窗**：只改 Preflight 一条路径（`compress()` 里 `CompressTrigger::Preflight =>
    self.adaptive_keep()`），Manual / Overflow 保持配置窗——溢出恢复要的是最大折叠量，不是「刚好过线」。
    两个闸门的量纲不同：`window_usage` 对字符闸门用 `total_chars`、对 token 闸门用 `estimate_tokens` ×
    锚点 scale；`gate_room()` 与 `over_compaction_threshold` 必须是同一个判定（改阈值判定时两处一起改）。
    **写压缩用例先数消息条数**：`find_safe_cut` 要求 `n > keep + 2`，7 条消息的夹具测不了 keep = 6
    （合法地返回 `None`，不是 bug）；切点只落在 assistant 上，名义起点是 tool_result 时会前滑到下一条。
    ③ **隔次重复窗口的差一**：`RepeatWindow::observe` 的返回值原样进模型可见文案（「已返回同样结果 n 次」），
    首次必须是 1、提示在该键第 4 次出现时发——实现时写成 `map_or(0, …)`，四个新用例全红（`[8, 9]` 对
    `[6, 7]`）。教训：**计数器的值进文案时，边界值（恰好等于阈值那一次）要有用例钉住**，并且先怀疑实现
    再怀疑断言（同批另外三处红是断言 / 夹具错：token 闸门用例把历史下标数错、两处字符宽度各差一）。提示与
    连续 nudge 用 `if / else if` 互斥，同一次调用不叠两条；硬停只认连续计数，别为了「更灵敏」把窗口计数
    接进硬停。
  - **给 `limits` 加键的四处同步（2.2 的 `summary_preview_chars`）**：① 内核侧按上面「新增 `Limits` 字段的
    接入流程」；② 评测装置的 `LIMIT_CLAMPS` / `LIMIT_DEFAULTS`（`scripts/regress_eval.py`）是 `Clamp` 的
    镜像，`scripts/tests/test_regress_eval.py` 按正则数 `config.rs` 里被 clamp 的字段数——加字段不更新即红，
    先跑 pytest 再跑 `regress_eval.py check`；③ 两份 README 的配置样例与 `limits` 说明；④
    **`Config::fingerprint` 把整个 `limits` 序列化进哈希**——加键让所有配置的指纹偏移一次（钉指纹字面值、
    跨版本比对的台账要留意），冻结过的 `prepare` 清单（harness 脚本与基配置的 sha256）也要重做。
  - **默认值翻转会把「非默认才进哈希」的那一侧翻过去（D1）**：`preserved_reasoning` 改默认开之后，进哈希的
    是显式 `false`。文档别再写「`preserved_reasoning` 非默认时进哈希、旧配置指纹不变」这种绑着旧默认值的话，
    说成「显式 `false` 才进」。A/B 配方的两臂都写显式值才不受默认值变化影响（`reasoning-carry` 就是）。
    测试里凡断言「历史不含 reasoning」的，要么显式关掉开关，要么说明当前轮没有回传——默认已经是开。
- **宽松网关的 200 证明不了回传正确（2026-11-26，Anthropic 线补测）**：`/frank` 这类翻译网关**不校验 thinking 签名**
  （去掉、改掉照样 200），所以「真端点跑通了」只说明没被拒，不说明回传的内容对。要证明回传忠实，得抓包做三方核对：
  网关发来的块（把 `content_block_*` 事件按 index 拼回）↔ 会话里的 `Message.reasoning`（`before` 是它在响应 content
  里的下标；`opaque` 是对象 `{"signature": …}`，不是字符串）↔ 下一次请求里回传的块；核对脚本先做变异自测（改签名 /
  正文 / 位置、丢块、改工具入参都要抓得出来）。做法是让被测二进制指向一个只听 127.0.0.1 的转发器（去掉认证头再落盘；
  落盘后按 `x-api-key` / `authorization` 两个头名扫一遍，确认是 0 条），每批跑完对主循环请求逐条查：角色交替、
  tool_use / tool_result N:N、thinking 只在带 tool_use 的消息里、签名非空、块逐字节一致；用量另算一笔——result.json
  的输入 / 输出含摘要请求，线上的主循环与摘要请求得**加在一起**才对得上（只加主循环时，折叠过的 run 会「对不上」）。
  真实线上两个容易写错的点：① 工具调用轮里 thinking 块多半是**空正文 + 一个 `signature_delta`**（`content_block_start`
  的 `thinking` 与 `signature` 都是空串，没有任何 `thinking_delta`）——空正文不等于未签名，`into_reasoning_block` 只看
  签名非空，这种块必须保留并原位回传（单测
  `empty_signed_thinking_between_text_and_tool_use_is_echoed_in_place`）；块序不止「thinking 在最前」，还见过
  `[text, thinking, tool_use]` 与 `[thinking, text, tool_use]`。② `signature_delta` 是 `content_block_delta` 里
  `delta.type` 的一个取值，不是顶层事件类型（按顶层类型数它永远是 0）。批次的 `summary.json` 不记摘要调用的次数：
  重试率只能从抓包里数（连续两个请求体相同即重试）。
  这套三方核对已落成仓库脚本（2026-11-27）：离线门禁 `python3 scripts/e2e_anthropic_wire.py`（CI 的 e2e job），
  抓真实端点用它的 `capture` / `audit` 两个子命令（只听回环的转发代理；上游地址与凭据值不落盘、落盘前再扫一遍），
  别再手搓转发器。它上线第一天抓到的就是「`--resume --task` 不结束上一轮推理寿命」（见上面 Anthropic 相邻 user
  条）。另一个教训在它自己的测试文件里：**依赖「真缺口还在」的测试，在缺口被修掉的那一刻就空转**——缺口机制的
  测试（`[GAP]` 不计失败 / `--verbose` 逐条 / `--strict` 致命）要用合成的缺口（`file_synthetic_gap`），并且断言
  「确实出现了缺口行」，不许 `if not gaps: return`。
- **pyte 重放 TUI 的三个假象（2026-11 定位，`/compact` 的 pty 验证）**：① **SU / SD 没实现**（见上面
  「滚动区画面疑似重叠」条的根因）。要忠实模拟就子类化 `pyte.Stream`，在 `__init__` 里
  `self.csi = dict(self.csi); self.csi.update({"S": "scroll_up", "T": "scroll_down"})`——dispatcher 在解析器
  FSM 首次推进时才创建，所以必须在首次 feed 之前改；Screen 侧按当前 margins 实现两个滚动（SU 不动光标、
  区域顶部的行移出，SD 在区域顶部补空行）。② **窗口尺寸要在 fork 之前设好**：`pty.fork()` 之后在父进程里
  `TIOCSWINSZ`，与子进程第一次 `terminal::size()` 是竞态（新 pty 的 winsize 是 0×0）——
  `os.openpty()` → 设尺寸 → fork → `setsid` + `TIOCSCTTY` → dup2 → exec。③ **光标查询要按真实位置应答**：
  裸驱动固定回 `\x1b[<底行>;1R` 会把 inline 视口钉在屏幕底行；真实终端里视口紧跟在启动打印的几行
  下面、随提交上移。让模拟器自己回（`screen.write_process_input = lambda s: os.write(fd, s.encode())`，
  pyte 的 `report_device_status` 会回真实行列）才贴近用户看到的布局。另：边读边剥 ANSI 时，缓冲区尾部
  的半个转义序列会在补全后改变剥离结果——「发键之后的输出」这类标记要打在解码后的缓冲上，别打在
  剥离后的长度上。
- **本机 `strip = "debuginfo"` + macOS 27 的 dyld 拒载绑定的 cdylib（2026-11，环境问题，非仓库缺陷）**：
  症状：`maturin develop` / `npm run build:debug` 都成功，`import tiny_agent` / `require(…)` 当场失败，
  报 `mis-aligned LINKEDIT string pool`。根因：用户级 `~/.cargo/config.toml` 的 `[profile.dev] debug = false`
  让 cargo 给 dev profile 默认 `strip = "debuginfo"`，rustc 链接后剥 debuginfo 产出的 cdylib（绑定的
  `.so` / `.node`）LINKEDIT string pool 起点只按 4 字节对齐（`stroff ≡ 4 mod 8`），macOS 27.2 的 dyld
  校验 8 字节对齐、直接拒载。可执行文件与 proc-macro 的布局恰好对齐，所以只有绑定暴露；HEAD 的干净
  导出同样复现（既有）。绕过（只动环境变量，不改仓库与用户配置）：`CARGO_PROFILE_DEV_STRIP=none` +
  独立的 `CARGO_TARGET_DIR`（免得和日常构建来回切换 profile 互相重编）。要根治可在自己的
  `[profile.dev]` 里写 `strip = "none"`（本批未代改用户配置）。CI 没有这份全局配置、release profile
  不受影响——**本地绑定测试挂在这个报错上时先查它，别去改绑定代码**。
- **本地 clippy 绿 ≠ CI clippy 绿：工具链版本差（2026-11-27，推送后 CI 七个 job 转红的教训）**：CI 的
  `stable` 跟着最新发布走（当时 1.100.0），本机默认工具链停在 1.93.1——新 stable 收紧的 lint 本地看不见，
  `check.sh` 全绿推上去照样红。实发的是 `clippy::result_large_err`：`Usage` 加 `reasoning_tokens` 后
  `RunError` 恰好 128 字节（正卡在红线上），1.93 只报返回它的四个**函数**签名（函数级 allow 压住了），
  1.100 连**转发它的闭包**一起报（测试里的 `thread::scope` / `catch_unwind`、Python 绑定的
  `allow_threads`）——逐处 allow 是一张只会长、且旧工具链看不见另一半的清单。处置：`RunError.usage` 装箱
  （96 字节）+ 编译期断言 `size_of::<RunError>() < 128`（回到红线即编译失败，任何工具链都拦得住），allow
  全删。**规则**：动公开错误类型的字段、或怀疑有新 lint 时，用与 CI 同版的工具链再跑一遍——
  `rustup toolchain install <CI 版> --profile minimal --component clippy,rustfmt`（与默认并存，不改默认；
  版本号看 CI 日志里的 rustc 输出，CI 跟 stable 漂），然后 `cargo +<CI 版> clippy …` 跑全部配置并
  `cargo +<CI 版> fmt --all -- --check`；配隔离构建目录 `CARGO_TARGET_DIR=/tmp/…`（换工具链 = 整树重编，
  别和日常构建目录混）。`cargo audit` 本地没装时可装到临时根、不碰全局：
  `CARGO_INSTALL_ROOT=/tmp/… cargo +<CI 版> install --locked cargo-audit --version <ci.yml 里钉的版本>`，
  再 `cargo audit --db /tmp/…`（advisory 库也放临时目录，首次不带 `-n` 让它取一次）。audit 与 msrv 这两道门
  红过一次**环境漂移**（与代码改动无关）：RustSec 新公告（锁文件里补丁版升级即可）、runner 自带的 Python
  升到 3.14 而 pyo3 0.22 上限 3.13（msrv job 钉解释器 3.12——这处修法本身又栽了一次，见下条）。
- **CI-only 的门，修完要对着日志逐格复核（2026-11-27，修 CI 的提交推上去仍有两格红）**：上一条的修法推上去后
  `msrv` 同一个报错原样再来一遍，`test (windows-latest)` 则在下游第一次跑到的测试上红。两个独立教训：
  - **GitHub Actions 引用不存在的 step 输出不报错，展开成空串**。`actions/setup-python`（钉 a26af69，v5）的
    action.yml 只声明 `python-version` / `cache-hit` / `python-path` 三个输出，另导出环境变量 `pythonLocation`——
    **没有** `python-location` 输出（这个名字是把环境变量当成了输出）。于是
    `PYO3_PYTHON: ${{ steps.py.outputs.python-location }}/bin/python3` 展开成 `/bin/python3`，日志里该 step 的
    `env:` 段原样可见。**规则**：引用任何 action 的输出前先读它的 action.yml
    （`gh api 'repos/<owner>/<repo>/contents/action.yml?ref=<钉的 SHA>' --jq .content | base64 -d`）；本地「复现了
    根因」不等于「复现了修法」，CI-only 的门只能靠推上去后的日志逐 job 看结论。
  - **`#[cfg(windows)]` 测试的运行时语义，交叉 clippy 管不到**（上一条 `#[cfg(windows)]` 配方只做类型检查 +
    lint）。cmd.exe 在命令行（`cmd /C …`，非批处理）里对**未定义**的 `%NAME%` 不展开、原样打印——批处理文件里
    才替换成空。两个凭据卫生探针（`tools::shell::tests::hidden_env_is_removed_from_the_spawned_shell`、
    `tests/lib_api.rs::api_key_env_is_hidden_from_tool_subprocesses`）用 `echo [%USERNAME%]` 探测，期望隐藏后打出
    空 `[]`（`sh` 的表现），Windows 上打出的是 `[%USERNAME%]`——隐藏本身是生效的，错的是读法。修法：让解释器
    自己回答「名字在不在」（`echo [%USERNAME%] & if not defined USERNAME echo <标记>`，断言那一行），别从 echo 的
    样子反推。**规则**：新写的 `#[cfg(windows)]` 测试，在它第一次真正跑在 Windows 上之前都算没验证过；上游
    job（clippy）红着的时候下游测试**根本没跑**，修好上游之后要有下游再红一轮的准备——推修 CI 的提交前，把
    「此后才第一次跑到的测试面」过一遍（本次是 `git diff <上次绿的提交>..HEAD -- '*.rs'` 里的 cfg 分支）。
    两个探针同病，一次修两处：`grep -rn 'cfg(windows)' crates src bindings` 定位块再逐块读。
- **run_code 的取消用例偶发报「fuel exhausted」（2026-11-27，根因没查出来，测试改成自带诊断）**：TUI 重构的四个
  提交推上去后，`code` 格的 `cargo test --locked -p tiny-agent-core -p tiny-agent --features code,mock` 红在
  `tools::run_code::sandbox::tests::cancel_flag_terminates_busy_loop`（A-5：300ms 后翻共享 cancel 标志，空转死循环
  应在下一个 epoch tick 被取消），断言文案是 `cancel-specific message: [run_code] terminated: fuel exhausted
  (5000000000 — deterministic budget; …)`，该用例耗时 303ms（绿的两次是 317 / 306ms）。**这条文案按代码路径到不了**：
  `RunCodeTool::terminated()` 只在 `interrupt` 为空、内存没超、`store.get_fuel() == Some(0)` 时才报「fuel exhausted」，
  而每个观察 cancel 的点（epoch 回调、host_call 入口、batch 的波间复查）都是先给同一个 store 记上 `interrupt` 再返回
  `Err`，没有任何路径清它——取消造成的终止一律是「cancelled … in-flight」；真烧完 50 亿 fuel，空转死循环在本机要
  约 1.8s，塞不进 300ms。逐条排除过的：wasmtime 36 的 fuel 记账（`get_fuel` = 储备 + 注入量的相反数；写这个槽的只有
  生成的机器码、内核唯一一处 `set_fuel`、`out_of_gas` 里的 `refuel`）；wasmtime 36 里没有别的线程会写某个 store 的
  上下文（epoch 是引擎级的共享计数器）；codegen 是确定性的（真有 bug 每次都会红）。本机复现不了（arm64 macOS，CI 是
  x86_64 Linux）：随机翻转时刻 8–60ms、4 线程、外加两线程反复建 store 与 fsync，13,646 次零异常；与 CI 相同的命令在
  两套工具链上全绿；翻 CI 历史，这条测试此前从没红过。当时的同场现象（因果没证实）：红的那一次里，所有做成功
  `saveLib` 的 `lib_index` 用例在 49.3–51.3s 之间慢了 10–60 倍（库落盘走 `atomic_write` 的 `sync_all`，是 fsync 在
  等盘；完成时刻成簇 = runner 的一次磁盘停顿），失败用例当时与其中三个并行。**处置**：不猜修、不给测试加重试或
  余量；产品代码没动，测试改成自带诊断——在 `join` 之前读 guest 自己的存活时长 `guest_took`（`join` 之后恒 ≥ 300ms，
  分不出「提前死」和「到点才停」），并断言「标志没翻之前没有东西能停掉这个 guest」。**再红时先读断言里的
  `guest ran`**：远小于 300ms = guest 在任何 cancel 存在之前就被停了（与本次同形；下一步是临时在 `terminated()` 里
  打出 trap 的真实类型、并设法在 x86_64 上复现——怀疑运行时或 runner，而不是内核的归因逻辑）；≈300ms 或更久 =
  取消生效了但归因成别的（内核 `terminated()` 的裁决有 bug）。**从 CI 日志重建时间线的办法**：libtest 保持 N（vCPU
  数）个用例在飞，每有一个跑完就打印它的结果行、然后才启动字母序里的下一个，所以一个用例的启动时刻 = 它排到队首
  时那条「跑完」打印的时间戳；把红绿两次同一 job 的逐用例耗时并排比，能看出 runner 的停顿（fsync 敏感的 `lib_index`
  用例是现成的温度计）。日志过期的失败 job（`--log` 返回 404）还能从
  `/repos/<owner>/<repo>/check-runs/<job-id>/annotations` 拿到报错行。
