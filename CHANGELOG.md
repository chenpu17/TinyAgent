# Changelog

本文件记录 TinyAgent 的版本演化。格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)（0.x 阶段 minor 即破坏性窗口）。

## [Unreleased]

### Changed（破坏性）

- **`Hooks::after_tool` 签名增加第 5 参 `ctx: &ToolCtx`**：与 `before_tool` 的 ctx 对称——审计点靠
  `ctx.subagent` 归因调用来源（顶层 / `task` 子 agent / `run_code` 沙箱），并行兄弟真并发下调用序
  关联不可靠，per-call ctx 是唯一可靠的来源标签（趁绑定尚未透出 hooks 的窗口期落此破坏性改动）。
  **迁移**：已有实现参数表追加 `, _ctx: &ToolCtx`。**注意此变更是静默失效型**：旧签名的
  `fn after_tool` 实现在升级后不再是 trait 方法实现（编译照过、降级为 inherent 方法），trait 默认
  no-op 接管——钩子悄悄停止触发，升级后务必核对审计输出仍在。

### Fixed

- **外部复审第五轮（b6ee977，每项带回归测试）**：
  - **P1 batch 隔离目录防符号链接预置**：`ta-batch-{task}-{pid}-{n}` 名字可预测，多用户共享 /tmp
    下可按名预置 symlink，`create_dir_all` 的"已存在"检查穿透符号链接 stat 而照常成功——整个
    workdir（含 `.env` 等 COPY_EXCLUDE 拦不住的秘密）被拷进攻击者目录、任务在攻击者可控路径执行。
    改 `create_isolation_dir`：裸 `create_dir`（mkdir(2) 不 dereference 末组件，任何预置条目一律
    `AlreadyExists`）+ 计数重试 + 8 连碰撞响亮失败，与 `atomic_write` 的 O_EXCL 纪律对齐；git
    模式先自建空目录再 `worktree add`（git 接受已存在的空目录），失败路径不泄漏目录。
  - **hooks-Deny 文案过 `tools::truncate`**（三处 Deny 点：顺序 gate / 并行 gate / run_code 内层）：
    宿主拒绝理由是模型可见 is_error 文本、可任意长，与 run_tool 对 MCP 错误串同款输出预算。
  - **canon_input 数值统一 f64 归一**：`1` 与 `1.0` 字面不同语义相同，provider/gateway 重发同参
    调用时数值类型抖动会拆键——打转护栏漏检；超 2^53 整数精度损失只折指纹区分度（宁漏检不误杀）。
  - **单个坏 JSON 任务文件不再中止整批**：per-task `bad_task` 经 `write_terminal_result` 单出口
    落盘 + 预置进 report 流（与 forbidden 覆盖同哲学），好任务照跑，`--failed` 同规则过滤。
  - **预算闸门 cap 单源**：turn 边界比较读 `BudgetGate::cap`，自身 limits 快照只能 `min` 收紧——
    子 agent 快照一旦与父 cap 分叉，比较快照会让 fleet 预算被静默放大。
  - **数值纪律补齐**：over_compaction 阈值 `usize::try_from`（32 位 musl 不再截断翻转比较）、
    compress prune 的 `freed` 全饱和累加。
  - **绑定诚实成本**：两绑定 `compact` 失败以 JSON 信封透出 `CompressError::burned`（py
    `json.loads(e.args[0])` / node `JSON.parse(err.message)`，照 run 错误路径）；`CompressError`
    进门面导出。绑定 crate 头补 GIL 死挂无看门狗警告（node 有 120s backstop、python 没有）、
    构造期 MCP 同步已知限制、hooks/热更面未透出属刻意范围决策。d.ts `ToolCall` 补 `meta?`、
    enableCode 措辞（同步方法 throw 非 reject）、py 文档 `elapsed` 字段名三处漂移修正。
  - **P3 卫生**：`ToolConcurrency` 去 `non_exhaustive`（生产侧枚举，对齐卫生规则）、SkillSource
    归类注释、CallbackTool 恒 Sequential 文档化（语言边界回调做不了重入承诺）、`--resume` 不带
    hooks 补契约测试、`merge_reports` 保同名重复条目（旧 HashMap 折叠静默丢条目）、on_chat_request
    无 cancel 复查文档化。顺手修两个 cancel 测试共用 scratch 根互相干扰的既有假红。
- **外部复审第五轮测试轮（1120b17）**：补并行批异常臂四个变异死区测试（worker panic 臂 /
  CancelOnUnwind 守卫 / gate 间逐调用检查点 / stuck×并行批），每个均以「临时改坏产品代码 → 新测试
  变红 → 还原」验证真实杀灭能力。
- **并行批「已过闸未启动」调用统一占位（c83c666）**：因 pre-spawn/波间检查点未启动的并行调用从
  `Slot::Cancelled` 转为与 worker pre-body 检查点同形的 `Slot::Ran` 记录占位（进 records +
  `after_tool(executed=false)`）——过闸调用不再留下未配对 before；gate 循环内补逐调用 cancel 复查。
  配套**场景 e2e 套件进 CI**（`scripts/e2e_scenarios.py`：线程化 SSE mock 驱动真实 CLI/HTTP 栈，
  13 项断言覆盖打转护栏三行为 / 单轮 10 路并行 fan-out / SIGINT exit 130）。

- **压缩摘要空正文自愈（思考网关）**：混合思考网关（deepseek / glm 系）把 reasoning_content 计入
  max_tokens 预算，summary_call 不发 `reasoning_effort`（刻意——见下）时这类网关落默认思考档，
  UPDATE 分支（带旧摘要的滚动更新）诱导的长推敲可整块烧光 1024 起步预算——`finish=length` 映射为
  `stop_reason:"max_tokens"` 且正文零字，空文本落压缩退化分支（dropped=0 + Warning，TinyWorker
  验收反馈 #7）。修法 provider 无关：首段 `max_tokens` 截断且**正文为空**视为该假设失效，预算加倍
  （封顶 8192、不钳回轮次上限——首轮已证明 cap 级预算必死于思考）重试一次；非空正文的真截断首轮
  即接受、重试仍截断也接受。两轮 usage 合并返回（烧掉的首轮照计费）。刻意不改成低档
  `reasoning_effort`：Anthropic 上任何非 "none" 的 effort 是**开启** adaptive thinking（与意图相反），
  OpenAI 兼容网关对 "low" 的解释不可预测。
- **TUI 体验批次修正**（与下条 Added 的 TUI 批次同源）：README 键位表退出语义漂移修正
  （Esc 早已不是退出键，改为 `/quit`、`/exit` 或空闲 Ctrl-C 按两次）；TUI 取消后不再追加
  第二行 `[done: cancelled]`（Esc 已有 `[cancelling…]` 提示，Done 行是纯噪声——
  `is_normal_stop` 名单加 `cancelled`，timeout / max_turns_reached / budget_exceeded 的
  告警保持不变）；空态按键速览补第 4 行（多行输入入口与取消键——两个最高频困惑点
  第一屏可见）。
- **全面审计（第四轮，P1/P2/P3）修正**（每项带回归测试；P1-1/2/3 为最高优先级项）：
  - **P1-1 atomic_write 防预置符号链接击穿**：临时文件名 `<target>.<pid>.<n>.tmp` 的 pid 与计数器均可预测，`write_file`/`edit_file` 只对目标路径做 workdir 沙盒检查、看不到临时兄弟名——攻击者（或恶意任务）在 workdir 预置同名符号链接指向 `/etc/cron.d/x`，"写进沙盒"实际写穿到任意路径。改 `create_new(true)`（O_EXCL）冲突即换名重试，符号链接永远打不开。同批：`--json` NDJSON 输出容忍下游断管（`| head` 不再 panic，改为静默放弃——stdout 已死时打印无意义）。
  - **P1-2 Python 绑定双死锁消除**：`Agent` 存 `Mutex` 而 `run()` 释放 GIL 后又要锁——重入调用（自定义工具 / provider 回调里碰 agent 方法，如 `push_user`）GIL↔Mutex 互等死锁；两个线程各自 `push_user` 也互等。锁模型重构为单一 `with_agent` 入口（`allow_threads` 内拿锁），重入（run 期间的 agent 方法调用）从死锁改为明确的"agent is running"报错。
  - **P1-3 Node 绑定 onEvent 异常不再致命**：TSFN 用 `ErrorStrategy::Fatal` 时回调抛异常 → `napi_fatal_exception` 整进程退出（host 侧一个日志回调的 bug 不该杀掉整个嵌入方进程）。wrapper 层 try/catch + 异步 `Promise.resolve().catch` 双保险，异常降级为 stderr 提示。同批：同步方法（`pushUser` 等）run 期间改 `try_lock` + 明确 busy 错误——阻塞等锁会冻结 JS 主循环，而 run 的 TSFN round-trip 依赖主循环转动（等锁 = 自死锁）；自定义工具等待上限感知 run 剩余预算（`min(deadline, 120s)`）。
  - **P2 硬化合集**：agent 轮询 deadline 与 shell 超时的 `Instant + Duration` 加法改 `checked_add`（巨值配置在 debug 下加法溢出 panic）；不合规网关的空/重复 `tool_use` id 归一化重铸（保 N:N 配对不炸下一轮请求）；`run_tool` 错误文本过 `max_output_chars` 截断（错误轰炸不再撑爆上下文）；技能扫描读取 64KiB / 256 个 / 简介总量 16KiB 封顶 + 技能目录符号链接跳过（巨型或循环目录不再拖死启动、撑爆 system prompt）；MCP HTTP 错误响应体 16KiB / tools/list 256 条 / 描述 1KiB / schema 64KiB 封顶（恶意 server 不能再用巨型列表 OOM 或灌爆上下文）、MCP 事件 16MiB 与 SSE 单行 16MiB 封顶（`fill_buf` 分块读，无先整行 `read_line` 再检查的绕过窗口）、pending 分发跳过服务端主动请求（roots 采样请求不再被误当响应派发给挂起的调用）、SSE 首行剥 UTF-8 BOM（带 BOM 的网关首行 `data:` 匹配失败 → 整流被当垃圾）；**OpenAI 流中途错误改诚实成本形状**（读失败与 `{"error":…}` 事件两处不再 `bail!` 丢掉已上报的 usage，对齐 Anthropic 的 `[stream error]` 标记 + `stop_reason:"stream_error"` + 部分块保留——agent 侧照常计费并转可重试 Err）；batch worker `catch_unwind` 兜住任务 panic（debug/CI 下单个任务的 bug 不再 unwind 出 `thread::scope` 杀掉整批，转 `stop_reason:"panic"` 确定性失败条目；release `panic="abort"` 下进程当场死，此兜底不生效——注释在案）。
  - **P3 快修**：shell 子进程 stdin 接 null（读 stdin 的命令立即 EOF 不再挂到超时）；`task` 子 agent 名按字符截到 64（事件标签不被模型的一句话刷屏）；`base_url` 归一去尾斜杠（`v1//chat/completions` 不再 404）；`--task ""` 显式拒绝（不再静默遮掉 config 的 task 空转）；压缩摘要 per-request 超时地板 1s（deadline 近耗尽时均分取整 ~0 = 瞬时超时）；`fold_summary` 空历史落 fresh-user-head 不再 index panic；Anthropic 无 usage 帧返回 `None` 而非 `Some(全零)`（"计了 0 token"与"用量未知"在账单里从此可区分）。

### Security

- CI 新增 `cargo-audit` job（RustSec 公告库扫依赖树，advisory 红灯）；README 安全说明补**信任边界**小节（config / session / 技能目录是受信本地输入，等价于代码——来源不明不要加载；运行时不可信输入走尺寸上限防御），`restrict_to_workdir` 表述从"拒绝一切逃逸"修正为 best-effort + TOCTOU 边界说明（硬边界仍是容器）。
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

- **PDF 文档输入（`Block::Document`，Anthropic 侧）**：`Message` content 支持 PDF 文档块，对齐
  Anthropic `document` content block（base64 source，服务端逐页"转图 + 抽取文本"）。新增
  `load_document_block(path)` 读取 helper（`.pdf` 后缀 + `%PDF-` 头校验 + 10 MiB 上限，对齐
  `load_image`），宿主经 `agent.push_user(vec![doc, Block::Text(…)])` 推入。预压缩按**页数**估
  token（官方口径 1,500–3,000/页、取保守上沿 3,000；页数为免解析启发式——裸扫 page 对象标记 +
  页树 `/Count` 互为补充，全无可见结构按 ~100 KiB/页兜底），文本型 PDF 不再被按字节的估算差出
  数量级。OpenAI 兼容端点发请求前**显式报错**（不静默丢弃文档）；TUI / resume 转录显示跳过，
  压缩摘要转录以 `<document>` 占位。
- **MCP 运行时热增删（库 API）**：`Agent::add_mcp_server(McpServerConfig)` /
  `remove_mcp_server(name)` / `tool_names()`——GUI 宿主轮次间增删 MCP server，不重建
  Agent（重建要重跑所有幸存 server 的握手）。撞名是可恢复错误且带回滚（刚 spawn 的子进程
  随即被杀、Agent 状态不变，绝不部分生效）；remove 按**配置名**路由（`Tool::mcp_server()`
  能力标记，非消毒展示名），最后一个持有者 drop 时同步杀 stdio 子进程组、不残留孤儿；
  已 `set_shutdown` 时热增的工具自动补挂同一 reaper。`task` 子 agent 持构造期快照、
  不感知热增删（文档化边界）。热增路径无任何打印（raw-mode TUI 宿主安全）。
- **宿主热更新 API（GUI 常驻会话）**：`set_model` / `set_reasoning_effort` / `set_max_tokens` /
  `set_max_context_tokens` 纯字段替换、下次请求生效——换模型档位不再需要重建 Agent（重跑 MCP 握手
  可达 15s），宿主可拆掉整套懒重建状态机。`task` 子 agent 持构造期 config 快照、不跟随换挡。
- **`Agent::compact()` 手动压缩入口**：轮次间（run 外，借用检查器强制顺序）主动折叠历史，语义与
  run 内压缩一致（滚动摘要 + 安全切点 + session 落盘），`CompressTrigger` 加 `Manual` 变体
  （NDJSON `"trigger":"manual"`）。返回 `CompressOutcome { dropped, kept, usage }`——摘要成本
  返回宿主记账、不进下一次 run 的账单。入口清掉陈旧 cancel 与 compress_failed。
- **`ToolInputDelta` 工具参数流式透出**：模型边构造调用参数边发增量（`ChatEvent` / `AgentEvent` /
  `--json` 的 `{"type":"tool_input","id","name","delta"}`，键为 tool_use id 可精确配对并行同名
  调用），GUI 不再有参数生成期的黑盒。纯展示馈送，权威输入仍在 `ToolCall` 事件，不进历史。
- **`Message` 稳定 id**：`Message.id: Option<String>`（agent 在消息进入历史时铸造、跨 save/load
  不变、压缩折叠不重排存活者；无 id 序列化逐字节同旧格式）。宿主 sidecar（图片卡 / 文件链接）
  锚 id 而非下标，压缩 / truncate 后锚点不漂移。
- **TUI 流式增量定稿**：长回复不再"憋到回合结束一次性刷屏"——在途内容超出 live 区高度后，已完结的
  段落**提前提交进原生 scrollback**；无段落边界可切的超长单段（LLM 常见的"一整段一行"散文）按
  **显示行**行级切分，内容以行为单位连续向上滚出（Claude Code 的观感），live 区只保留正在生成的
  尾部两三行；代码围栏内切分自动补合成开栏，两半各自渲染为代码块。长回复边生成边滚出，流式期间
  即可阅读前文、判断是否跑偏随时取消；思考区块同理。续块不重复 ● 消息头（一条回复不会读成多条
  消息；段内切分的紧凑续块之间不加空行、段落不被撕开，段落边界仍保留段间距），`/export` 自动把
  切块拼回完整文本。
  取舍：已提前定稿的行不随后续内容 / 终端 resize 重排（scrollback 写一次即定，与 Claude Code 同款）。
- **TUI 思考过程独立区块**：`ThinkingDelta` 不再与答复正文折进同一 Assistant 行（推理模型的思考
  与回复混排成一坨不可读），改渲染为独立的暗色斜体 `✻` 区块——不走 markdown、与答复视觉
  分离；流式期间 live 区带 `▌` 光标、输入框标题如实显示 thinking（答复开始才转 writing），
  答复以正常样式另起一块，两者先后定稿进 scrollback；子 agent 的思考带 `[名字]` 标注，
  `/export` 以 `✻ Thinking` 段落收录。
- **TUI 体验改进批次**（全部改动在 bin crate、lib 零触碰；守「内联 live 区 + 原生
  scrollback」架构红线，无鼠标 / 托管滚动）：
  - **工具运行期 live 占位 + 计时**：工具执行中 live 区显示 `→ 工具名 · 1.2s…`
    （同 turn 并行 fan-out 聚合为 `(+N)`），busy 时 120ms tick 走字——长 shell 期间
    底部不再是空白盲区。渲染期合成、不进 rows / 转录。
  - **`/out [n]` 命令**：重印第 n 新的工具结果全文（默认最新一条；错误结果同样可
    展开），`/help` 与补全弹窗经同一张命令表自动跟上。此前成功结果的完整输出在
    TUI 内不可见，只能 `/export` 绕路。
  - **`--resume` 历史转录进原生 scrollback**：user / assistant 消息与工具调用、结果
    按 `tool_use_id` 配对重建单行视图（`· ` 系统行标注 resumed；图片跳过——显示层
    转录，本体留在 agent 历史），恢复后对话区不再一片空白；停在 tool_result 中途的
    会话 auto-start 续跑时，任务文本直接可见（不再依赖 `--task` 才有镜像行）。
  - **`TINYAGENT_TUI_THEME=light` 浅色终端适配**：dim 文字（系统行 / 预览 / 状态段，
    原 Gray 在浅底近白隐形）换 DarkGray，行内代码白→青（深浅底皆可读）；dark 默认
    零漂移（与旧常量逐值相等的测试钉住），不做自动探测（终端背景色上报不可靠）。
  - **键位对齐主流终端 AI 工具**：单行输入（含空输入）或浏览中 ↑/↓ 历史召回、
    多行时恢复跨行移动（Alt+↑/↓ 语义不变）；`Ctrl-D` 删光标后字符（空输入且空闲
    → 退出，backspace 的镜像）；行尾 `\`+Enter 换行（bash 同款行续接——不上报
    修饰键的终端如 macOS Terminal.app 的唯一手动多行入口）；空闲 `Ctrl-C` 双击
    确认退出（2s 窗口内再按一次，首按只提示——退出不可恢复所以加确认，忙时
    单击取消的语义不变，取消可恢复）。
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
