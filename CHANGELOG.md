# Changelog

本文件记录 TinyAgent 的版本演化。格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)（0.x 阶段 minor 即破坏性窗口）。

## [Unreleased]

### Added

- **诊断通道 `diag`（GUI 宿主可见的软失败警告）**：内核里 ~19 处裸 `eprintln!` 直写（技能扫描/装配警告、
  MCP 握手与读线程降级、session 损坏保留提示、run_code 畸形 marker 回退与沙箱降级）对无控制台的
  GUI 宿主与语言绑定完全不可见。统一收敛到 `crate::diag::diag_warn!`/`diag_info!`：默认（未装 sink）
  逐字节回退 stderr（CLI 行为零变化）；宿主可 `tiny_agent::diag::install_diag_sink(Box<dyn Fn
  (DiagLevel, &str)>)` 把警告作为数据接管（日志面板/文件/自家 facade）。进程级可替换 sink 而非
  per-agent 管道——最吵的站点在装配期（尚无事件流）与后台线程（无 ToolCtx）。sink 集成断言挂在
  strip_once 畸形 marker 测试上（TEST_LOCK 串行化）。

- **子 agent turn 天花板可配置 `limits.subagent_max_turns`（跨核心 + 双绑定）**：`task` 子 agent 的
  turn 预算天花板从硬编码 `SUBAGENT_MAX_TURNS=25` 提为 `Limits` 字段——外部宿主可自行放宽（GUI 类
  应用跑千级轮次的场景）或设 `0` 不另设天花板（子 agent 完整继承父 `max_turns`）。语义不变量保持：
  子预算恒 = `min(父 max_turns, 天花板)`（0 时 = 父预算），**永不高于父自身预算**——`task` 调用不能
  比生成它的 run 活得更久；父 run 的 `timeout_secs` 与共享 cancel 仍是真正的墙钟护栏。默认 25 = 旧行为
  （旧配置文件零改动兼容）。批量任务片段按 `Clamp::TightenOrInit` 归类：只允许收紧（片段 `0` 被忽略、
  不拆基础天花板；基础 `0` 时采纳片段有限值）。核心实现见 916ee3b（`child_max_turns` 纯函数单测、
  config anti-silence 哨兵与片段收紧五断言）；本条补充双绑定透传测试（py/node 各一：构造 + mock 跑通
  证明 `deny_unknown_fields` 未拒新键）。

- **run_code 使用文档三件套**：`docs/run_code_guide.md`（快速上手与最佳实践——配置合并、
  工具接入、三项扩展示例、失败恢复；README run_code 专节与结项状态总表加使用入口）；
  入门演示判分加固——每次独立 mktemp 工作区保留历史（不再 `rm -rf`）、请求模型前先验
  二进制 features 含 `code`、配置由模板生成（`max_tokens` 2048→4096，
  `TINYAGENT_DEMO_MAX_TOKENS` 可覆盖），判分为类型+值+键集+成功 run_code+end_turn
  五重判据，不满足即非零退出。

### Changed

- **python 绑定 CI 矩阵扩到 Windows**：此前绑定层从未在 Windows 验证过，job 只在 ubuntu 跑并注明
  "等手动跑绿再扩矩阵"。前置条件已满足（2026-10 手动 Windows 全绿：CPython 3.12 win_amd64，
  `maturin develop` 39 过/4 跳，`--features code` 42 过/1 跳——code 自适应测试全激活），矩阵扩为
  `ubuntu-latest + windows-latest`（fail-fast 关闭）；venv 接线从 `activate`（POSIX 专用）改为
  显式路径探测（`bin/` vs `Scripts/` 二选一 + `VIRTUAL_ENV`/`PATH` 导出），两步固定 `shell: bash`。

- **run_code 拆分为模块（纯搬移重构，行为零变化）**：原 11.9k 行单文件按关注点拆为
  `tools/run_code/` 九个子模块（contract 契约文本 / sandbox wasmtime 沙箱 / guest_api 宿主函数面 /
  fs_policy 路径与 glob 策略 / lib_index 会话库 / batch callBatch / precheck FR-2 预检 /
  tool 工具本体 / test_util 共享测试辅助），`run_code.rs` 保留为 103 行门面（mod 声明 + 符号
  汇回，外部 `run_code::…` 引用路径全部守恒）。函数体逐字搬移，唯一改动为可见性关键字与两处
  include 路径修正；`run_code` 测试拆分前后精确守恒（150 个），契约四哈希
  （description/schema/prelude/skill md）逐字节复验不变。门禁随迁：`sync_run_code_contract.py`
  抽取器重定向至 tool.rs/contract.rs，且 manifest 补钉全部子模块文件（新增子模块须同步脚本与
  `gen_structure.py` ENTRIES——后者渲染器新增嵌套子目录支持）。

- **shell 工具描述按平台诚实化**：原描述在 Windows 上承诺 POSIX sh 语义（heredoc、/dev/null、
  `&` 后台）而实际解释器是 `cmd /C`。现按 `cfg` 分平台：unix 说明 `sh -c` 与 heredoc/后台语法；
  Windows 说明 `cmd /C`、`>nul 2>&1` 重定向、含空格路径双引号、复杂数据处理建议
  `powershell -NoProfile -Command`。

- **llm 判别与 context_report 漂移守卫（防御性）**：`run()` 与 `panic_audit_row()` 两份平行
  输入校验收敛到单一 `reserve_params` 源（错误文案逐字不变）；`context_report.rs` 在四个漂移
  起源点加 `debug_assert!`（system 三段和、工具桶 fold、按型分桶总和、未校准总和 ==
  `estimate_tokens`；校准后路径文档明示允许舍入漂移、不硬断言），发布构建零开销。

- **workspace 拆分：内核独立成 crate（用户零感知）**：原 bin + lib 同包拆为双 crate——
  `crates/core`（package `tiny-agent-core`，lib 名仍为 `tiny_agent`，`use tiny_agent::…` 全部
  免改）持有 agent loop、工具、provider、config、skills、MCP、session、压缩、`build_agent*`
  装配与新提取的 `fsutil`（原子写原语，session/write_file 与 result/report 共用）；根 package
  `tiny-agent` 变为纯 CLI（main/signal + 随迁的本地模块 harness/batch/tui——headless 呈现与
  批量编排本就是应用层）。内核依赖树甩掉 clap 与 TUI 四件套；CLI 的 `code`/`mock` 为转发
  feature，根目录 `cargo build --features …`、`--version --json` 清单、musl/e2e/release 产物
  语义不变。绑定 path 依赖改指 `crates/core`（依赖键与转发 feature 不变）。CI 的 clippy/test
  显式 `-p tiny-agent-core -p tiny-agent` 圈定两包（根目录裸命令拆分后只覆盖根 package），
  live job 的 `--lib` 同改；`gen_structure.py`（双 crate 结构树）与 `sync_run_code_contract.py`
  （源文件路径）随路径体系更新。harness 行为测试（重试梯子 / result.json / NDJSON / unix
  真叶 e2e，约 1000 行）随 harness 迁入 CLI crate（`src/harness_tests.rs`），内核
  `agent_tests.rs` 相应瘦身。顺带修复三处存量问题：`tests/lib_api.rs` 三处 `ToolCtx` 字面量
  缺 `tool_absent_notes` 字段（`--features mock` 下编译失败）、新版 clippy 的
  `manual_checked_ops`（tui.rs）与冗余 match 绑定（agent_tests.rs）告警、`fetch_update`
  弃用告警以 `#[allow(deprecated)]` 压制（`try_update` 需 >1.86，MSRV 地板 1.86）。

- **冻结文案校准**（skill / 工具描述 / 设计附录 / manifest 同源同步，修订记录
  llm v0.7.8 / memory v0.7.5 / run_code_design v0.9；预检/墓碑文案无变化，
  finetune_support_design 未动）：callBatch 教学对齐实际并发——并行由工具能力决定，
  read_file/MCP 仍顺序、llm 可并行、上限 8/波，依赖上一页游标的分页须顺序循环；
  llm 预算错误区分调用次数耗尽（停机）与 token 预占拒绝（沙箱存活、文案自带
  `the sandbox stays alive`）；截断/异常终止不作为有效判别，级联示例移除未校准的
  `max_tokens: 8`（默认 1024）；落盘纪律改为「产物落盘并完成必要校验后收尾」；
  include 定位收紧为「宿主有明确复用需求才建库」，删除未经验证的价值比较表述。

### Fixed

- **CLI 观察项快赢批（审读轮坐实的 9 个低垂项，全部带回归测试或文档修正）**：
  - batch `.JSON` 大写扩展名静默跳过 → 大小写不敏感匹配（Windows 文件系统语义）；
  - batch `--report` 路径指向目录时批末才失败、丢汇总报告 → 启动前预检立即 bail（任何任务不启动）；
  - batch `git worktree add` 失败清理用 `remove_dir`（对非空半成品必失败且吞错）→ `remove_dir_all` + best-effort 警告；
  - TUI CodeLog 行双 `· ` 前缀（推入处与渲染层各加一次）→ 只留渲染层（屏显与 /export 同时修好）；
  - TUI `ansi_plain` 丢末尾换行（/export 尾随空行与屏显差一行）→ 尾部 flush 补 `ended_with_newline`；
  - TUI 裸 ESC 后跟 `\n` 时换行被两字符序列分支吞掉 → 仅合法序列引入符才消费下一字符；
  - `--timeout` 帮助文本声明 per-attempt 语义（与 `--retries` 组合时总墙钟可超预算）；
  - 退出码文档注释补 125（budget_exceeded，实现与测试本就一致，纯文档遗漏）；
  - signal.rs 两平台 Ctrl-C 处理器安装失败由静默忽略改为响亮警告（默认终止会破坏 130 契约）。

- **CLI crate 审读修复批（4 分片全量审读坐实 7 个 P2，无 P1；全部带回归测试）**：
  - **batch Windows `--isolate copy` 链接环**：解引用分支遇自指/祖先相对链接（含 junction）指数级递归
    复制直至磁盘耗尽，且经链接展开的物理内容绕过 `COPY_EXCLUDE`（target/node_modules 整搬进沙箱）。
    修复：会话级 visited set（canonical 路径去重剪环）+ deref 分支同样应用根层排除；unix 逐字重建
    分支零变化。junction 实测无需特权，两个 Windows 原生回归测试钉住线性复制与排除。
  - **harness/main 关流 panic 丢 result.json**：重试提示等 9 处裸 `eprintln!/println!` 在 stderr/stdout
    关闭时 panic（release abort），且重试 loop 内的 panic 使 result.json 永不落盘——与本文件自设的
    best-effort I/O 纪律不一致。统一改 `harness::note/note_out`（`let _ = writeln!`）。
  - **resume 预铸 id 溢出**：session 含 `m{u64::MAX}` id 时 `floor + 1` 在 debug 构建溢出 panic；
    镜像内核 `fresh_message_id` 的 saturating 语义（饱和时重复 MAX id 是 lesser evil）。
  - **TUI MdCache 无界增长**：流式期间每 delta 一条缓存（单消息内 O(文本×delta) 二次方），已提交
    条目纯死重，resize 换宽度整套重建。改为单槽缓存（有效工作集本就只有当前流式行），提交路径
    render-once 旁路缓存。
  - **TUI diag 通道接管**：raw mode 下 MCP 后台线程的警告经默认回退 eprintln 直接涂花视口帧与
    原生 scrollback（空闲期重绘门控不再覆盖）。TUI 启动安装 sink 转发 `UiMsg::Diag` 渲染成系统行，
    TerminalGuard Drop 清理（panic unwind 对称）。
  - **TUI 大单行粘贴 O(n²)**：MB 级单行（base64/长 URL）逐字符 insert 搬移分钟级卡死 UI 线程；
    改批量分段拼接，与逐字符参考实现全等对账（含 CRLF/CJK/emoji）。
  - **TUI 嵌套围栏失步**：tui-markdown 把所有围栏归一化为恰 3 反引号，嵌套示例（4 反引号内含 ```）
    使 fence toggle 错位、后半代码块被当 markdown 重渲染。修复：从原文按 CommonMark 围栏语法派生
    开/闭/内容分类计划，渲染行流严格对账通过才采用、任一分歧回退原 toggle（零漂移）；残余未建模
    构造自动回退并已文档化。顺带修 Windows 反斜杠路径的状态栏 cwd 段整段消失。

- **门禁产物对拍修正（真 Python 解释器实测发现）**：本机落地便携 Python 3.12 后实跑三个 CI 门禁
  脚本，抓到两处此前 node 镜像再生法的漏洞——README 结构树中 provider/mock.rs 描述少一个右括号
  （镜像转写错，CI `gen_structure.py --check` 会红），已用真脚本 `--write` 修正并将整文件归一回
  LF（python 在 Windows 文本模式写出 CRLF）；`examples/eval/e0prime-skill/run-code/SKILL.md`
  在本工作区被翻成 CRLF 而该门禁按**原始字节**哈希（LF 折叠后与登记指纹完全一致，内容从未变过），
  已归一回 LF。同轮首次在本机全绿：三 Python 门禁、e2e 场景套件（18 断言，含 Windows
  CTRL_BREAK 取消与 llmbatch 服务端验证真并发）、MSRV 1.86.0 `check --locked`（经镜像装工具链）。

- **MCP stdio 的 Windows shutdown-reaper 补齐**：unix 上 TUI 退出时 reaper 线程对进程组即时
  `SIGKILL`，卡在阻塞读的 worker 立即解锁；Windows 此前没有 reaper——要等宿主进程退出、OS 关
  job 句柄（KILL_ON_JOB_CLOSE）才杀整树，正是 unix reaper 要避免的 orphan-until-host-exit。
  现补齐：`winjob.duplicate()`（DuplicateHandle 复制 job 句柄，LeakIfAlive 策略——复制品只杀
  不回收，原句柄保留 Terminate-on-drop）交给 Windows 版 reaper（与 unix 逐字同构：100ms 轮询、
  alive 收摊防误杀、job 缺失/线程失败降级警告）。三测试：shutdown 触发整树及时死、双次 arming
  幂等、alive 收摊不误杀。

- **MCP Streamable HTTP 202 Accepted 挂死/误判**：POST 收到无 body 的 202（响应走服务端下发
  的 SSE 流）时，旧实现落入 plain-JSON 分支解析空串报 `invalid JSON response`，auto 模式还会
  误触发 legacy SSE 回退。现按规范补齐：捕获会话头后向同端点 GET（`Accept: text/event-stream`、
  带 `Mcp-Session-Id`）建流，复用既有 `sse_response_for_id`（method 守卫/封顶/超时）等到匹配
  id 的响应。回归测试用最小 HTTP/1.1 mock（含 id 碰撞 decoy）钉住请求头与恰好一 POST + 一 GET。

- **shell Windows 命令引用改写（cmd /C mangling）**：命令串经 std 参数引用传给 `cmd /C` 时，
  含空格整段被包引号、内嵌 `"` 转义为 `\"`——cmd 剥引号规则不识别该转义，实测
  `echo "hi"` 输出 `\"hi\"`、`cd "C:\Program Files" && …` 直接报错退出。改用
  `raw_arg` 逐字传递（等价控制台手敲；实测三种形态全对，引号 exe 路径仍受 cmd 自身两引号
  保留规则保护），unix `sh -c` 路径不变。

- **内核质量审核修复批（5 分片全量审读坐实的 11 个 P1 + 1 个交互 P2，全部带回归测试）**：
  - **MCP stdio 写阻塞**（`mcp.rs`）：`request()` 的 `write_all` 移入写线程，主侧 50ms 切片轮询
    cancel 与 10s `WRITE_TIMEOUT`；超时/取消击杀子进程断管回收写线程（不泄漏常驻阻塞线程），
    `stdin` 改 `Option` 后续调用快速失败——server 停读 stdin（大帧 + 卡死）不再挂死整个 turn。
  - **run_code 结果静默丢失**：guest 结果/错误消息超过 16 MiB 单次读帽（`GUEST_READ_CAP_BYTES`）
    时不再 `unwrap_or_default()` 成「成功但空输出」，改为明确的 read-cap 错误与「写文件再切片读」
    恢复指引（成功/错误两腿都响亮）。
  - **run_code 批次主机侧无界累积**：新增 `BATCH_TOTAL_OUTPUT_CHARS`（8M 字符）聚合预算，
    超限条目降级为带指引的短占位（hooks Deny 文本同样计入预算）；envelope 回退不再把完整
    payload 二次嵌入（只报尺寸）——默认 200 次调用的批次峰值从 ~1.6 GB 收敛到几十 MB。
  - **shell Windows 杀进程**（`shell.rs`）：spawn 后立即入 Job Object
    （`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`），超时/取消 `TerminateJobObject` 杀整进程树——
    此前只杀 `cmd.exe` 垫片、真正的工作进程继续跑；drop 时 job 已空则关句柄，仍有幸存者则
    有意泄漏句柄以保住「`start /B` 后台进程跨调用存活」逃生舱（宿主死亡时 OS 关句柄仍全树
    陪葬）。windows-sys feature 面相应扩充（JobObjects/Threading/Foundation/Security）。
  - **fsutil Windows rename 兜底**：只读目标先清只读位再覆盖（对齐 posix rename 语义）、
    AV/索引器共享冲突 3 次 50–100ms 退避重试、失败清理 tmp 前清其只读位——result.json /
    session 保存不再被这两类瞬态错误直接击穿。
  - **summary 重试 backoff 尊重取消**（`agent.rs`）：`sleep_cancelable` 返回 true 即以
    cancelled 形状终止并如实结转 `burned`，不再向已取消的运行重发（可能很大的）摘要请求。
  - **session archive 字节帽**（`agent.rs`）：新增 `ARCHIVE_MAX_BYTES`（64 MiB）与既有条数帽
    组成双帽 FIFO 逐出——截图/base64 密集的长会话，会话文件不再无界增长到 GB 级。
  - **provider 非 2xx 错误体封顶**（`openai.rs` ×2 + `anthropic.rs`）：三处 `resp.text()`
    换共享 `capped_err_body`（16 KiB + 截断标注，含 loopback HTTP-mock 回归测试）——与 SSE
    路径 64 MiB 三重上限、mcp 的 16 KiB 错误体帽的威胁模型对齐，恶意网关无法再用无限错误体
    撑爆内存。
  - **`McpServerConfig` Debug 脱敏**（`config.rs`）：手写 Debug，`headers`/`env` 只保留 key 名、
    value 打码——宿主/绑定侧任何 `{:?}` 日志不再全文带出 MCP 凭据（Authorization、
    GITHUB_PERSONAL_ACCESS_TOKEN 等；与 `api_key_env` 只存变量名的既有间接设计对齐）。
  - **`PartialConfig::apply` 设防**（`config.rs` + `batch.rs`）：`apply` 对硬字段
    （provider/base_url/api_key_env/workdir/tools/restrict_to_workdir/mcp_servers）默认拒绝
    并指路——库消费者误喂不可信任务片段不再能改 endpoint/工具白名单/注入未校验 MCP 服务器；
    新增 `apply_trusted` 作为宿主已筛查覆盖层（CLI batch 路径）的全字段入口，并补上
    `mcp_servers` 的 `validate()` 与工具白名单名字校验两个兜底。
  - **skill 读取 UTF-8 边界**（`tools/skill.rs`）：字节帽切在多字节字符中间时交付合法前缀 +
    截断标注（照抄 file.rs 既有行为）——纯中文技能文档/资源不再被误报「not valid UTF-8」硬错误。
  顺带：新增本仓库首批 Windows 原生行为测试（Job Object 孙进程击杀、只读/共享冲突 rename、
  MCP 写超时与取消）；README 结构树与 run_code 路由 manifest 随行数/文件哈希再生。

- **P2 清理批（同日审核发现的次级问题，25 项，全部带回归测试）**：
  - **MCP Windows 子进程整树回收**：shell 的 Job Object 逻辑抽取为共享模块 `winjob.rs`
    （两种 drop 策略显式化：shell 的"幸存者逃生舱"vs MCP 的确定性整树 Terminate），
    mcp.rs 的 stdio transport 接线——Windows 上 Drop/写失败不再残留 cmd 垫片下的孙进程。
  - **重试分类**：`is_transient` 纳入 408/425（服务端明示可重发的瞬态码，此前走确定性失败）。
  - **run_code 沙箱**：`glob_match` 入口折叠连续 `**` 段（Kleene 闭包等价，实测原实现
    6 连星 × 64 段路径需 131s、指数增长）+ 折叠后 1024 段帽 fail-closed；嵌入资产的
    `strip_once` 畸形 marker 不再 panic、回退保守输出。
  - **截断边界族**：read_file 的 limit 参数 32 位 `as usize` 截断改 try_from 报参数错误；
    read_file 与 read_skill 的"读期间文件生长越过字节帽"stale-stat 形状不再误诊
    "not valid UTF-8"（共享 `grew_past_cap` 判定，交付合法前缀+截断标注）。
  - **路径圈界**：`confined_to` 对余量段做词法归一（`.` 丢弃、`..` 弹栈、下溢即逃逸），
    关掉 `workdir/nonexist/../x` 的非竞态逃逸变体；doc 归位到函数本体。
  - **上限族**：subagent `prompt` 超限（8× 输出帽）拒绝而非首请求必烧；llm 工具成功路径
    text 过 max_output_chars 截断（与错误路径对齐）；MCP 工具包装层文本分段边拼边限
    （16 MiB 硬帽，不再先全量累积）。
  - **会话保全**：损坏/未来版本的会话文件不再静默丢弃——改名保留现场
    （`.corrupt`、`.corrupt.1`…）后再 fresh start。
  - **MCP 协议**：Streamable HTTP 响应匹配加 `method` 守卫（服务端发起的请求不再误配成
    响应报 protocol violation）；空名工具丢弃+warn；SSE `data:` 只剥一个前导空格（对齐
    provider 侧与 SSE spec）；读线程/reaper 的 spawn 不再 `expect` panic（线程耗尽是
    per-call 错误）。
  - **provider**：openai 锁中毒容忍式统一（对齐 mcp.rs）；anthropic 首次 POST 前检查
    cancel（取消不再延迟一个往返）；补 openai 无 [DONE]/无 finish_reason 的
    stream_truncated 归因测试。
  - **config**：`api_key()` 区分"未设置"与"设置了但非 UTF-8"；`McpServerConfig` 的 url
    含 userinfo（`scheme://user:pass@host`）时 Debug 打码（手写有界扫描，authority 段内
    最后一个 `@` 切分 fail-closed）。
  - **agent 循环**：summary_call 的不可达兜底从 `unreachable!` 改 debug_assert+安全返回；
    `fresh_message_id` counter 饱和时保证终止；全零 `Usage` 对象不再发零值事件；
    compress 两处直接切片改防御式 `get`。
  - **依赖面**：libc 移入 unix target 依赖（两个 crate 的全部用点都是 `#[cfg(unix)]`，
    Windows 依赖图减重）；内核 Cargo.toml 补 keywords/categories（repository 待有公开
    仓库后补）。

## [0.4.2] - 2026-09-27

run_code v1 结项版本：三轮外部复审的运行时修复 + 价值验证收口（G3）+ 文档状态统一。

### Fixed

- **llm 终止契约改白名单 fail-closed（45ad4ae / 9c3db9b）**：内层 `chat()` 的 Ok 不再等于
  完整成功——仅 `end_turn`/`tool_use`/`stop_sequence` 三个可证明完整的终止走成功；
  `max_tokens` 定约为不完整输出，`stream_error`/`stream_truncated`/`stream_idle`/
  `cancelled`/`content_filter`/`refusal` 及未知未来词表一律 `kind="llm"` 错误
  （原五词黑名单下 content_filter/refusal 的残留文本可命中 labels 伪装成有效分类）。
  部分 usage 诚实结算（有则 settle、无则保留预占），错误输出带 `meta.llm_call` 审计行；
  终止检查先于 label 校验。
- **include/hash_file 尊重 Hooks::Modify（45ad4ae）**：lib 家族入口补齐 Modify 形状——
  对 effective 参数重跑 `lib_rel` 边界与 deny 检查，执行用 effective 路径，
  `after_tool` 审计留原始输入，CallTrace 补 `in_hash_eff` 双哈希。
- **saveLib/INDEX 通道加固（45ad4ae / 9c3db9b / cbda304）**：INDEX 读取错误分诊——
  只有确定的"文件不存在"才初始化空索引，hooks 拒绝/IO 错误中止保存而不再按空索引
  覆盖既有登记；存在性判定改走宿主结构化保留名 `__lib_index_state`（不再靠异常文本
  子串分诊）；登记键统一为宿主规范路径（`lib/./x`、`lib/../lib/x` 不再毒化 manifest）；
  INDEX 单行 parser 收敛为宿主单源（guest 侧近似副本删除，按构造消灭 parser 漂移）；
  `__include` 交付改 `{"src": …}` 包装（整文件恰为合法 JSON 的库不再被 guest
  auto-parse 静默 no-op）。
- **callBatch 波内三源刷新（45ad4ae / cbda304）**：波内每项执行前与 worker spawn 前
  改查 interrupt/cancel/deadline；多波 wave-start halt 时未启动条目逐项
  `{kind:"cancel", halted:true}` 且事件配平；`MAX_BATCH_ENTRIES=4096` 超界整批预检拒绝；
  批内 image 返回落 `kind:"image"` 错误信封。
- **`set_code_config` 与启动期同一 llm 验证（45ad4ae）**：llm 片段省略 model 时
  default_model 属于 models 白名单的终检不再被热更新路径绕过。
- **batch panic 占位补 llm 审计（45ad4ae）**：worker panic 占位在错误输出上附
  `meta.llm_call` 审计行，unknown_calls/预占对账不丢。

### Added

- **G3 分支验证与 run_code v1 结项（e58955b / d65502c）**：按需披露装配下 T2 采用成立
  （费用比 0.567×/0.571×，与全量提示词原配方不可区分）——README「何时开启」指引更新为
  **默认 `skill_injection: "on_demand"` 装配即足够触发采用**，能力段全量提示词降为可选
  增益。评测装置新增 `prepare-g3`/`run-g3` 与费用停止线续批机制（`--continue-of`）。
  `docs/code_review_brief.md` 改造为特性族唯一状态总表（六项能力逐一标定
  已交付/可选保留/研究归档），两份设计稿补结项状态，全部评测工单（G0–G3）关档。
- **门禁加固（cbda304）**：README code 配置表交叉核对扩展到布尔字段与嵌套 LlmConfig
  默认值；设计稿冻结块与代码常量补逐字断言测试；双绑定补钉 include/call_batch/llm
  三新键的 serde 形状测试。

## [0.4.1] - 2026-09-25

### Added

- **库侧只读 introspection getter（c03158a）**：`Agent::system_prompt()`（装配后的完整系统提示词：
  env 头 + 用户 base + skill intro）与 `Agent::tool_defs()`（模型可见的每个工具定义）——评测装置
  （run_code 价值验证 G0）需要从真实构造的 Agent 导出两臂差分，而不是靠配置白名单字符串推测。
  纯只读，不改任何装配语义。
- **run_code 价值验证全程结论（README 使用指引）**：24 次付费 A/B 对照（G1 六跑 + T2 重跑 +
  G2 十二跑 + G2C 候选四跑，0.245 CNY，单模型合成任务）——「无 shell 的多源分页聚合」形态
  run_code 自然采用带来稳定费用优势（三实例 0.51×/0.55×/0.58×）；带 shell 任务模型自然选
  shell、run_code 零采用，全量能力段提示词是纯披露开销（最高 1.43×），去掉能力段回落
  ~1.0×~1.2×。默认关维持（样本不足以支持默认开启）。完整报告与逐跑工件见
  `docs/code_value_reports/`；README run_code 专节新增「何时开启」指引。

### Fixed

- README 代码结构树补齐 c03158a 漏跑的 `gen_structure`（agent.rs 行数漂移，CI fmt 门禁会红灯）。
- 主包 `optionalDependencies` 的五个平台子包版本引用从 0.3.0 同步到与主包同版本（v0.4.0 只 bump
  了主包 `version`，漏了这五条——README 分发契约是「引用同版本子包」，release workflow 的子包
  版本单源也是主包 `version`，错配会让 registry 消费者装到 0.3.0 子包）。

## [0.4.0] - 2026-09-24

### Added

- **run_code 代码承接（QuickJS 沙箱全链）**：进程内同步 JS 沙箱（QuickJS，ES2023，无 tokio 依赖）——
  编排 MCP/API 分页、批量操作与大文件处理。封闭 prelude（`readAll`/`readLines`/`writeVerified`/
  `copy_bytes`，字节保真写读回校验）；workdir 路径拦截（符号链接按原始拼写逐组件走查、不存在目标经
  最深已存在祖先 canonicalize 判定）；fuel/内存/墙钟/调用额度四限 + 中断四点（入口门/epoch 回调/
  host_call 入口/收尾裁决）；`tools.list()/call()` 宿主工具面、`tools.budget()` 额度自察、结构化
  `e.kind` 错误分类（denied/hooks/budget/deadline/cancel/unknown_tool/lib/llm/llm_output）。默认全关，
  `code.enabled` 开启。
- **include 库通道 + callBatch 批量通道 + llm 判别原语**：`lib/INDEX.md` 生命周期（`saveLib` 登记 →
  负责人批准 → 跨会话 `include` 准入，哈希防漂移）；`tools.callBatch` 只读并发批量（8/波、按提交序、
  无事务语义、失败条目 `instanceof Error` 判别）；`tools.call("llm", …)` 单发判别调用（`labels` 受约束
  输出契约 + 词表归一化校验、token 发前预占、成本落 `meta.llm` 台账、级联判别教学面）。三通道各自
  独立开关（`code.include`/`call_batch`/`llm.enabled`），门关时模型可见契约与旧二进制逐字一致。
- **run_code 微调支持**：`skill_injection` 三态（声明序=披露序）装配期消费；`precheck` 调用期检出台
  （`{run_code, task, todo}`）拒答与墓碑文案——LLM 可见文案即规格，冻结在
  `docs/finetune_support_design.md` 附录 A。
- **上下文构成查询 `context_breakdown()`（4278d8e，跨核心 + 双绑定 + TUI）**：新公共 API——外部宿主可查询
  下一请求上下文的估算 token 构成与占比：`system` 三段（env 头 / 用户 base / skill 简介）、工具 schema 四类
  （`builtin`/`mcp`/`skill`/`custom`，经 `Tool` trait 新默认方法 `origin()` 归类，含逐工具明细）、消息历史
  八分型（`user_text`/`assistant_text`/`tool_use`/`tool_result`/`image`/`document`/`skill_content`/`other`；
  无 thinking 桶——思考是纯事件流永不进 history）。一切数字是估算口径（`compress::estimate_tokens` 同款
  yardstick），**与 Usage 实测刻意不混流**（`usage()` 仍是计费权威）；有有效 usage 锚点时透出
  `calibration`（实测/估算的 scale 均匀缩放，锚点失效即回 `None` 纯启发式）。未校准时
  `system+tools+messages == total == estimate_tokens(全量)` 严格成立（单趟 running-floor 整除分摊）。
  透出面：Rust `Agent::context_breakdown()`、Python `context_breakdown()`（跨线程阻塞等锁）、Node
  `contextBreakdown()`（run 中立即报错，wrapper.d.ts 类型同步）、TUI `/context` 命令（busy 时禁用，同
  `/clear`）。
- **跨平台 npm 分发（bfac803）**：主包 `files` 白名单不再携带任何 `*.node`；五平台
  （darwin/linux × arm64/x64 + win32-x64）以 `tiny-agent-<platform>` optionalDependencies 子包
  同版本分发，release workflow 五平台矩阵构建（NPM_TOKEN 缺省时子包 tarball 附 GitHub Release）。
  `tests/test_packaging.test.js` 为打包验收门：主包白名单无 `*.node` + 本地伪造平台子包安装 →
  加载 + mock 调用全链路。
- **绑定 hooks 拦截层暴露（0986858 / a951ac9）**：Python `set_hooks` / `hooks=` kwarg、Node
  `setHooks` / `hooks` 选项——鸭子类型三方法任意子集（`before_tool`/`after_tool`/`on_chat_request`，
  Node 侧 camelCase）。决策 fail-closed：抛异常/非法返回 = Deny（理由进模型可见错误 tool_result）；
  观察类钩子异常吞掉（Node 观察类 TSFN 是 Fatal 策略，wrapper 必吞）；`before_tool` 决策经
  `resolveHooksDecision` round-trip；`ctx.subagent` 审计归因。`chat()` 一次性入口同面。

### Changed（破坏性）

- **`Hooks::after_tool` 签名增加第 5 参 `ctx: &ToolCtx`**：与 `before_tool` 的 ctx 对称——审计点靠
  `ctx.subagent` 归因调用来源（顶层 / `task` 子 agent / `run_code` 沙箱），并行兄弟真并发下调用序
  关联不可靠，per-call ctx 是唯一可靠的来源标签（趁绑定尚未透出 hooks 的窗口期落此破坏性改动）。
  **迁移**：已有实现参数表追加 `, _ctx: &ToolCtx`。**注意此变更是静默失效型**：旧签名的
  `fn after_tool` 实现在升级后不再是 trait 方法实现（编译照过、降级为 inherent 方法），trait 默认
  no-op 接管——钩子悄悄停止触发，升级后务必核对审计输出仍在。
- **run-code 技能正文新增「产物分块增量写」纪律条（7cbf815）**：大文件产物每完成一段
  `writeVerified` 落盘一次、写完产物即收尾（真实跑实测遵循：批间 checkpoint 九次单调增长 +
  收尾核对式正式写）；`examples/prompts/agentic-code.md` Persistence 段加同义英文条目。
  增量变更只加不改，teaser 与工具描述未动（finetune 文档修订记录 v0.6）。

### Fixed

- **零 prompt 的 usage 不再写锚点（a3f858e，外部复审发现）**：输入三轴全零的 usage 对象（野见：OpenAI
  兼容 relay 缺 `usage.prompt_tokens`、Anthropic 协议网关发 `{input_tokens:0}`）此前照写 usage 锚点，把
  `scale` 钉死 0.0——`context_breakdown()` 报"已校准的全零"（TUI 渲染 `calibrated ×0.00` + 100% free），
  压缩预检 `estimate_input_tokens` 也退化成 0+delta 低估，一个垃圾输入毒化两个消费者。现在零 prompt
  视为网关未报 usage，两个消费者都保守回退纯启发式；输出 token 计费不受影响。回归测试
  `zero_prompt_usage_writes_no_anchor`（修复前红灯复现）。
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
