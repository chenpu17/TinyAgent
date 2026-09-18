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
    回主线程。**绝不改回 napi `AsyncTask`/libuv 池**（外部评审教训）：agent 线程会阻塞等 JS 工具/provider
    round-trip，而被包装的 JS 回调把自己的异步活（`fs.promises`、timer、Promise continuation）也排进同一
    libuv 池——并发 run 数 ≥ 池线程数（默认 4）时全员互等死锁；专属线程与 `UV_THREADPOOL_SIZE` 解耦。
    线程用 8 MiB 栈（对齐原 libuv worker 默认）并经 `catch_unwind` 兜底——panic 也 settle Promise
    （release 是 `panic = "abort"`，仅 debug 有效）。自定义 JS 工具是 **TSFN round-trip**：agent 线程把 `{id,args}` 经 TSFN
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
  **轮内取消（2026-09 审核 #9）**：`McpTool::run` 把 `ctx.cancel` **随单次调用**传入传输层（不是 set_cancel
  传播），stdio/SSE 的响应等待按 ≤50ms 薄片轮询（`recv_cancelable`）——Ctrl-C 在卡死 server 上不再等满
  120s `CALL_TIMEOUT`；取消后 pending 槽由既有错误路径回收、迟到响应被 `pending_dispatch` 丢弃。HTTP 传输
  仍是整段阻塞 POST（自带 CALL_TIMEOUT 总超时），无切片。`run`/`set_shutdown` 共享 client 锁的顺序依赖
  （set_shutdown 可能短暂排在一个在飞调用后面，由切片轮询兜底）注释钉在 `tools/mcp.rs`。
- **`serde_json::json!` 宏按所有权取值**：`From<&Value>` / `From<Option<_>>` 不存在。给宏传
  `&String`/`&Value` 要 `.clone()`；`Option<&str>` 要显式转成 `Value::Null` 或 `json!(s)`。
- **Anthropic 不允许相邻两条 user 消息**（400）；且 N 个 `tool_use` 必须配 N 个 `tool_result`。
  `agent::append_user_turn` 会把新 user 内容折进已有的末尾 user 消息——新增 user 入口走它，别直接 push。
  另有兜底：`agent::run()` 每轮发请求前 `normalize_adjacent` 把相邻同 role 合并（保块顺序，不动配对），
  防 resumed/手改的损坏历史触发 400。别依赖它当正常路径，正常仍走 append_user_turn。
- **cancel 契约（TinyWorker 反馈落地）：run() 永不清，受理方清残留**。cancel 是电平标志、归嵌入方
  所有；`store(true)` 全库无自动复位，`agent::run()` **也不清**（旧入口 `store(false)` 已删——它会把
  宿主受理窗口内（懒构建可达数秒~15s）按下的停止当残留误清，用户停止被静默丢弃）。预置 true 的 run
  在循环首检即返回：`cancelled`、turns==0、零 provider 请求。清残留的主体责任在**受理新任务的一方**：
  `Agent::clear_cancel()`（pub，绑定层 `clear_cancel`/`clearCancel` 已透出）。**破坏面**：嵌入方「set_cancel + 复用 agent 多轮」漏改的症状是每轮
  秒回 cancelled（确定可见、`turns==0` 可机判），不是静默。库内**真正调用** `clear_cancel` 的只有两处：TUI
  `run_once`（每条提交输入/初始 auto-run）与 `compact()` 入口；`harness::run_with_retries` / `batch`
  重试梯的 attempt 间判定是**终态出口**——cancel → `EXIT_CANCELLED`，不重试**也不清**（清了即复活
  用户已否决的 run；batch agent 本就不装 cancel flag，行为零变化）。**gate→执行检查点**（2026-09
  外审 P2）：`before_tool` 审批等待可以无限期，窗口内落下的 cancel 若不复查就出现"run 报
  cancelled、被批准的工具照跑"——`agent::ctx_cancelled()` 在 gate 返回后、执行前复查，三处
  dispatch（`execute_tool_call` / 并行批 spawn 前 / run_code 内层）共用；顺序路径产出
  `[cancelled by user]` 错误槽 + `after_tool(executed=false)`，并行路径整批不启动（槽位 Cancelled
  合成），run_code 内层复用 `CANCEL_HALTED_WHY` 墓碑。**波次间 + worker 预检**（第三轮 P2）：
  并行批按 8/波，每波开波前复查 cancel、worker 进工具体前再查（返回 None → join 补
  错误槽 + after(executed=false)）。**回调 panic 防死锁**：并行批的 `sync_channel` 必须在
  `thread::scope` 闭包**内**创建——`rx` 借自闭包外时，drain 循环里宿主回调 panic 的 unwind
  不销毁 `rx`，worker 堵死满漏斗、join-all 死锁；闭包内创建 + `CancelOnUnwind` 守卫翻 cancel
  收尾。测试组：`run_with_preset_cancel_returns…` /
  `run_does_not_clear…` / `clear_cancel_lets_the_next_run_proceed` /
  `parallel_children_both_honour…` / `single_run_cancel_during_failed_attempt…` /
  `cancel_during_approval_gate_does_not_execute_{sequential_tool,parallel_batch}` /
  `cancel_between_waves_never_starts_the_next_wave` /
  `parallel_event_sink_panic_unwinds_instead_of_deadlocking` +
  绑定两侧回归。
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
- **思考内容展示策略**：`ThinkingDelta` 在 `harness` 的 `--json` NDJSON 发 `{"type":"thinking","delta":...}`；
  headless 终端打印写 **stderr**（不进 stdout 的 `summary`，可 `2>/dev/null` 屏蔽）；TUI 用 or 模式
  TUI 渲染为独立暗色斜体 `✻` 区块（`Row::Thinking`，不走 markdown/MdCache，与答复视觉分离；可折叠区块仍是 TinyWorker 的事）。
  子 agent 的思考经 `AgentEvent::SubAgent` 整体包装转发（`subagent.rs` 不逐变体 match），harness 的 SubAgent 内嵌
  match 单独把它写 stderr 带 `[name]` 前缀。
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

