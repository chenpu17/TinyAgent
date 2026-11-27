# TinyAgent 内核改进计划

> 实施状态 · 2026-11-25，2026-11-26 补真模型验证与第二轮落地 · **阶段 0、阶段 1 与阶段 2 的代码项已全部落地**（2.1 回传默认值已按 D1 改成开；2.2 的钩子、TUI `/compact` 与 `summary_preview_chars` 旋钮齐备），实验一、实验二、实验三都已在 deepseek-flash 上跑完；真模型验证抓出的三个内核缺陷已修、配了离线回归测试，并做了修前 / 修后的真模型 A/B——摘要截断与折叠抖动两项见效明显，隔次重复提示样本太小、看不出差别。当晚又把 1.1 的 Anthropic 分支接到真端点（同一网关的 Anthropic 协议路径，`hw-vol-deepseek`）核对：回传的思考块与网关发来的逐字节一致；并用修后二进制复测实验一，回传省不省 token 随端点而变（OpenAI 协议 −31%，Anthropic 协议 +26%）。用户 2026-11-27 看过复测后确认回传默认保持开；同日把 `long-output` 的判卷线从 2 次折叠降到 1 次。代码在未提交的工作区里，尚未发版。逐项状态、与设计的出入、默认采用的决策和真模型验证的结论见下面的「实施状态」一节——它与正文冲突时以它为准，正文保持写作时的设计原样。
>
> 草案 v1 · 2026-11-25。
>
> 依据：对 `crates/core` 与 `src/` 当前工作区的阅读（行号以该工作区为准，含未提交改动），以及对 GLM、Anthropic、OpenAI、MCP、Codex、Claude Code 官方文档的核对（链接见 §10）。标"待确认"的条目还没有一手依据。

## 0. 实施状态（2026-11-25）

### 0.0.1 逐项状态

| 工作项 | 状态 | 备注 |
|---|---|---|
| 0.1 MCP `tools/list` 翻页 | 已完成 | 另加「游标重复即停并告警」 |
| 0.2 `refresh_tool_defs` 量纲 | 已完成 | |
| 0.3 Skills 块标量 | 已完成 | |
| 0.4 回归任务集与评测脚本 | 已完成 | `check` 进 CI；付费通道 2026-11-26 跑过八个批次（六个在 deepseek-flash 上，两个是当晚的修后复测：OpenAI 协议与 Anthropic 协议各一个；§0.0.5），抓出评测装置自己的三个缺陷；`long-output` 的判卷线 2026-11-27 从 2 次折叠降到 1 次（§0.0.5 末尾） |
| 1.1 推理状态回传 | 已完成，默认开（见 2.1） | 两个分支都上过真端点。OpenAI 兼容分支见实验一；Anthropic 分支 2026-11-26 晚在网关的 `/frank`（模型 `hw-vol-deepseek`）上跑了 30 个 run，逐请求抓包核对——回传的 thinking 块与网关发来的逐字节一致；会话文件里存的那一份在探针上做过
三方核对（§0.0.5「Anthropic 线补测」）。该网关不校验签名，Anthropic 官方 API 的严格校验仍未验证 |
| 1.2 实验一（回传 A/B） | 已跑（2026-11-26） | deepseek-flash，30 run：成功率不降、总 token −39%，达到放行条件。内核修复之后同配方复测：OpenAI 协议 −31%，换成 Anthropic 协议端点（`hw-vol-deepseek`）是 +26%——方向随端点变，每格 3 次量不出两三成的差别；用户 2026-11-27 看过复测后确认默认保持开（§0.0.5「修后复测」） |
| 1.3 `extra_body` / `extra_headers` | 已完成 | |
| 1.4 推理 token 账 | 已完成 | Anthropic 的 usage 没有这个拆分，保持 0 |
| 1.5 模型窗口 | 已完成 | 可选的模型前缀表没做（正文写明单独评审） |
| 1.6 MCP 注解 | 已完成 | |
| 2.1 回传默认值 | 已完成（2026-11-26） | 用户拍板 D1「默认开」（2026-11-27 看过修后复测后确认不改）：`preserved_reasoning` 默认 `true`；显式 `false` 回到旧行为，指纹只在显式 `false` 时进哈希 |
| 2.2 压缩交接 | 已完成 | 钩子与 TUI `/compact` 已落地。实验二已跑（2026-11-26）：`keep_recent_messages: 0` 明显更差，默认 8 保持。实验三也已跑（同日）：`Limits.summary_preview_chars` 加宽到 1000 没有更好，反而更贵，默认 200 保持（§0.0.5） |
| 2.3 工具定义体量提示 | 已完成 | |
| 2.4 MCP 补齐 | 已完成 | 2026-07-28 规范仍是跟踪项；另做了正文没有的 404 会话恢复；顺带修掉一个计划外的 legacy SSE 缺陷（见 §0.0.4） |
| 2.5 无人值守默认值 | 已完成 | 凭据卫生按 D2 的建议成为默认；无人值守提示只打给 headless 与 batch |
| §0.0.5 的三个内核发现 | 已修，离线验证 + 真模型 A/B（2026-11-26） | 摘要预算阶梯、Preflight 自适应保留窗、隔次重复提示；27 个 loop 用例 + `shrink_keep` 的 5 个单元测试。修前 / 修后同配方 A/B（回归集 6 任务 × 3 次）：`long-output` 折叠 82 → 21 次、通过 1/3 → 3/3；隔次重复提示样本太小，看不出效果（§0.0.5） |
| 阶段 3 | 未开始 | |

### 0.0.2 与设计的出入

- **0.1**：正文只写了「循环到没有 `nextCursor`」。实现另加一条：server 把见过的游标再喂回来时停下并告警，已取到的页保留。
- **0.3**：正文只说了必填字段的情况（技能不加载）。读码确认了另一半：块标量写在 `license` / `compatibility` / `metadata` 上时读成字面量 `>` / `|`，块体丢失。两种情况现在都按块标量解析。
- **0.4**：评测脚本按单次 run 调 CLI（`--config … --json --output result.json --session none`），不走 batch；每次 run 一个进程、一份 result.json。评测侧数据放任务目录里的 `harness.json`，任务片段保持 batch 任务文件格式，可以直接喂 `--batch`。片段里的 `limits` 按 batch 的 Clamp 语义合并，只能收紧。臂配置压过任务的 mock 专用调参，否则一个任务就能把实验臂抹平。`prepare --mock` 让付费配方零成本走一遍脚本化通道，`--base-config` 覆盖配方的基配置。费用停止的口径定死为「已花 + 下一个 run 的最坏预留 ≥ 停止值即停」，用量从未上报的 run 按预留记。指标在正文的六项之外加了工具错误数的均值。
- **1.1**：三处。
  1. 形状。`Reasoning` 是 `{ provider, blocks: Vec<ReasoningBlock { before, text, opaque }> }`，不是 `{ provider, payload }`。Anthropic 的交错思考要求推理块**原位**回传，`before` 记它原先排在第几块之前；`opaque` 装 `signature` 或 `redacted_thinking` 的 `data`，OpenAI 兼容端只有裸文本。
  2. 产出时机。provider 无条件产出 `ChatResponse.reasoning`，由 agent 按开关决定落不落历史。正文写的是「开关打开且响应带推理时才产出」。现在 mock、测试和宿主的自定义 provider 看到的是同一个形状，开关只管一件事。
  3. 失效规则比正文窄。正文是「任何历史改写都使其后的推理状态失效」。实现的寿命只有**当前轮**：新的 user 轮、`set_model`、相邻同角色合并才整段剥掉；压缩折叠与工具结果裁剪保留幸存消息自己的推理。未被改写的消息，它的推理对同一个模型仍然有效。

  正文之外另有三点：OpenRouter 的 `delta.reasoning` 只展示不落；会话归档永不携带推理；缺推理块的 400 附一句提示（开关关着时才提）。
- **1.3**：`extra_body` 的保留键比正文多五个——`max_tokens` / `max_completion_tokens`，以及设了 `reasoning_effort` 时的 `reasoning_effort` / `thinking` / `output_config`。保留键一律在加载期报错，没有「重复但只告警」的灰区：内核自己写的顶层字段全在保留清单里。
- **1.4**：`reasoning_tokens` 在 result.json 与 `report.json` 里是「为 0 不出键」，不是「恒在 + `serde(default)`」。这样非推理模型的产物逐字节不变。NDJSON 的 `usage` 事件则恒带该键。
- **1.6**：归一后的 `ToolAnnotations` 不是三个 `Option<bool>`，而是按规范默认值落位的 `bool` / `bool` / `Option<bool>` / `bool`——`readOnlyHint` / `destructiveHint` / `openWorldHint` 有默认，`idempotentHint` 没有，省略即 `None`。`McpToolInfo` 上仍是 `Option`。旁表的构造点只有 `Agent::try_new` 与 `refresh_tool_defs` 两处。
- **2.2**：钩子只在「真的折叠」之前触发，摘要失败、空摘要、只做 prune 都不触发。`CompactionInfo` 比正文多了子 agent 标记与即将离开历史的消息本身，全是借用。TUI 的 `/compact` 接受可选的 focus 文本。
- **2.3**：判定取 `/context` 报表的当前口径（有存活校准时按校准缩放）；构造期的提示每进程一次，子 agent 与热增删不提示。
- **2.4**：比正文多做了三件。`0` 与超 7 天的超时值直接报配置错误，点名 server、键与默认值。Streamable HTTP 上带会话 id 的请求吃到 404 时重新握手并只重发一次。空闲 ping 静默时**不发出**调用，文案点明 `was not sent`。stdio 的 `connect_timeout_secs` 约束的是 `initialize` 握手。`allow_tools` / `deny_tools` 在 `mcp_servers` 里面，而 `mcp_servers` 本来就是 batch 禁改字段，所以「batch 不得覆盖」自动成立。
- **2.5**：默认只藏 `api_key_env` 指向的那一个变量。移除发生在每个 spawn 点，库不改进程环境；MCP 条目自己的 `env` 里显式写出的同名变量照常下发。卫生提示只在配置里真有 `shell` 或 stdio MCP 时发。Windows 的 `cmd /C` 链与 unix 链走同一个函数，但本机是 macOS，Windows 一侧没有实跑，只过了 Windows 目标的交叉类型检查与 clippy（见 §0.0.4）。`scripts/e2e_mcp.py` 没有进 CI。
- **§0.0.5 的三个内核修复**（不在原计划的工作项里，是真模型验证抓出来的；下面每条先写「当时列的候选做法」，再写实际做了什么）：
  1. **摘要截断**。没做「上一次被截断就抬高下一次的预算」，也没做提示词瘦身（提示词是模型可见文案，改要先走规格）。
     做的是：**这一次**的摘要「截断且带推理」（或正文为空）时，立刻以翻倍的预算（上限 8192）重试一次，取较长的一份正文；
     重试只能让结果变好，最坏等于旧行为。另带两条学习，挂在 agent 上、跨 run 保留、`set_model` 清掉：网关拒给翻倍预算
     （确定性的 4xx）就记上限、不再要；重试被正常服务就抬高之后摘要的预算下限。不带推理的截断保持单次尝试。
  2. **折叠抖动**。做了自适应保留窗，没做最小收益闸门——闸门只是把折叠推迟，历史仍在阈值之上；缩窗直接去掉病因
     （尾部自己就占满了空间）。只动 Preflight 一条路径，手动压缩与溢出恢复保持配置的窗口。
  3. **打转护栏的空隙**。加了不要求连续的同参重复计数与针对轮询的提示，但**只提示、不停**：硬停线（连续 6 次）没动，
     是否调整仍要单独量。隔次重复分不清「原地打转」与「改一步、验一步」，误停比漏停贵。

### 0.0.3 决策

实施时 §8 的五项都按「我的建议」一列采用，没有等拍板。其中 D1 已于 2026-11-26 由用户拍板改判为「默认开」并已落地，2026-11-27 用户看过修后复测（收益不随端点迁移）后确认保持；其余四项要改判告诉我即可，各自的成本如下：

| 编号 | 采用 | 改判的成本 |
|---|---|---|
| D1 | ~~先 opt-in，默认关~~ → **默认开**（2026-11-26 用户拍板，已落地） | 改回默认关是一行改动加文档；显式 `false` 的行为不变，但进指纹的那一侧会再翻回去。依据有更新：修后复测里回传省不省 token 随端点而变（OpenAI 协议 −31%、Anthropic 协议 +26%，§0.0.5）；用户 2026-11-27 看过复测后确认不改回去，默认保持开 |
| D2 | 凭据卫生成为默认 | 把默认的隐藏集清空即回到旧行为，探针测试与文档要跟着改 |
| D3 | 不修订 Non-goals | 没有代码，只是文档 |
| D4 | 暂缓 `/responses` | 没有代码 |
| D5 | 版本号按 §2 | 尚未发版，发版时再定 |

### 0.0.4 验证（2026-11-25 首轮、2026-11-26 第二轮；本机 macOS arm64）

**第二轮**（D1 默认开、§0.0.5 的三个修复、`summary_preview_chars` 都落地之后，在最终代码树上重跑）：

- `./scripts/check.sh` 15 步全过（约 130 秒）；`docs`、`bindings` 两组另跑，全过。内核另跑了一遍带 `--features mock` 的 clippy
  （bench 的 agent 用例门在 `mock` 后，CI 的四套 clippy 都看不到它），没有告警。
- 内核单测：默认与 `mock` 两套各 827 通过 / 2 忽略，`code,mock` 1018 通过 / 4 忽略；TUI 版 CLI 312 通过。
- 两个绑定：默认构建各 71 通过 / 4 跳过，带 `code` 各 74 通过 / 1 跳过。本机要先设 `CARGO_PROFILE_DEV_STRIP=none`，
  否则 Node 侧加载 `.node` 直接失败（`docs/claude/pitfalls.md` 记的 dyld 问题，是本机环境不是产品缺陷）。
- Windows 目标交叉检查九套全过（`-D warnings`）：CI 同款四套加 `tui,code,mock`，两个绑定 crate 各自默认 / 带 `code`。
- `scripts/e2e_mcp.py --idle` 59 项全过（约 80 秒）。这一轮没有改 `mcp.rs` 或任何 spawn 链，跑它只是为了不留「没复测」的尾巴。
- 评测脚本自己的离线单测 96 项通过。
- 三个修复的新用例在落地时抓到一处真缺陷：隔次重复的计数从 0 起数，提示晚一次才发、文案里报出的次数少一
  （改的是实现，不是断言）。同批另外三处红是断言或夹具自己错，已改。

**首轮**（2026-11-25 落地时；2026-11-26 真模型验证收尾时、评测装置三处修复之后复跑过一次。下面各条说的「最终代码树」
指首轮收尾时的代码树）：

- `./scripts/check.sh`（fmt、四套 clippy、四组测试、e2e、文档门禁）在最终代码树上 15 步全过（126 秒）；`bench` 与 `bindings` 两组另跑，同样在最终代码树上复跑全过。2026-11-26 真模型验证收尾时（评测装置三处修复与文档更新之后）再跑一遍：15 步全过。
- 内核单测：默认与 `mock` 两套各 791 通过 / 2 忽略，`code,mock` 982 通过 / 4 忽略；TUI 版 CLI 312 通过。
- 两个绑定各 71 / 74 通过（默认 / 带 `code`）。
- `scripts/e2e_mcp.py` 59 项全过：默认 48 项约 12 秒，`--idle` 加上空闲探测 11 项约 67 秒（最终代码树上复跑）。
- `scripts/regress_eval.py check` 全过；评测脚本自己的离线单测 90 项通过（2026-11-26 真模型批次抓出评测装置的三个缺陷后补到 96 项，见 §0.0.5）。
- Windows 目标交叉检查（`x86_64-pc-windows-gnu`，经 zig 包装；只做类型检查与 clippy，不实跑）：内核与 CLI 的
  lib 加全部测试通过，CI 同款四套 clippy 加一套 `tui,code,mock` 全过（`-D warnings`），两个绑定 crate 也过。
  这一步抓到并修掉了 3 个只在 Windows 上出现的编译错误：`mcp.rs` 三个 `#[cfg(windows)]` 测试的
  `McpServerConfig` 字面量缺 2.4 新加的四个字段（本机任何门禁都看不到，推上去 CI 的 Windows 格第一步就红）。
  配方记在 `docs/claude/pitfalls.md`。legacy SSE 修复落地后又对最终代码树重跑了一遍：六套配置（CI 同款四套加 `mock`、`tui,code,mock`）全过，
  两个绑定 crate 的四套（各自默认 / 带 `code`）也重跑全过。
- **计划外修掉的一个产品缺陷（legacy SSE 轮转间隙）**：收尾跑全量门禁时 `legacy_sse_post_follows_the_fresh_endpoint_after_rotation`
  偶发红，查出根因不在测试——流轮转的间隙里发出的请求被 POST 到已退役的会话，会话绑定的 server 靠定时器回收会话，
  请求被应 202、应答写进已关闭的连接，调用方等满整个 `call_timeout_secs`。现在轮转先退役 POST 目标，请求与通知等新
  `endpoint` 再发（界 = `connect_timeout_secs`，看得见 cancel 与读线程死亡；取消通知不等）。配了 6 个回归测试，关掉修复
  那一句时新用例与原先偶发的用例一起变红。相邻限制没动：已出门的请求跨过轮转边界时，应答仍依赖 server 重发。
  详见 `CHANGELOG.md` 与 `docs/claude/pitfalls.md`「轮转间隙里的请求」。
- 顺带见到一个与本次改动无关的既有偶发：`batch::tests::cancel_during_retry_backoff_marks_task_cancelled` 在本会话 12 次
  CLI 测试组里红过 1 次（靠 50ms 定时翻 cancel，注释自己写明对负载敏感）；单独连跑 10 遍全过，没有改它。
- **没有验证的部分**：真模型上的实验（实验一至三、§0.0.5 三个内核修复的 A/B、当晚的修后复测）只在同一条网关后面的
  两个模型名上各跑了 3 次重复（OpenAI 协议的 `deepseek-flash`、Anthropic 协议的 `hw-vol-deepseek`），没有第二家模型；
  隔次重复提示只有 `loop-suppression` 一个任务会触发，连方向都确认不了；1.1 的 Anthropic 分支已在真端点上核对过回传
  是否忠实，但那个端点**不校验签名**、也没出现过 `redacted_thinking`——Anthropic 官方 API 的严格校验还没有一手验证；
  Windows 上的**运行**（`cmd /C` 链、Windows 专属测试都没有执行）；CI 独有的 musl 静态构建与 cargo-audit。

**当晚补测之后**（Anthropic 线补测与修后复测；这一轮代码只多了一个单元测试，其余是文档）：`./scripts/check.sh` 15 步全过
（131 秒）；`bindings` 组（两个绑定 crate 各自默认 / 带 `code` 的四套 clippy）与 `bench` 组（带 `mock` 的 clippy 加
`loop_bench`）另跑全过；内核单测默认与 `mock` 两套各 828 通过 / 2 忽略，`code,mock` 1019 通过 / 4 忽略，TUI 版 CLI
312 通过。八项基准的中位数与 11-25 收尾时逐项相差都在 ±3% 以内（`loop_turn_long_history` 4459 对 4458 ns，
`agent_construct_long` 59.4 对 58.5 µs）——11-26 的全部改动（含三个内核修复）都没有量出变化。两个绑定的测试套件、
Windows 交叉检查与 `e2e_mcp.py --idle` 这一轮没有重跑：第二轮之后 Rust 源码只多了这一个单元测试（在 `#[cfg(test)]` 里），
第二轮的结果仍然对应现在的非测试代码。

### 0.0.5 真模型验证（2026-11-26，deepseek-flash 与 hw-vol-deepseek）

用户给了一个 OpenAI 兼容网关和 `deepseek-flash`（推理模型）。网关要求请求头 `x-opencode-session`，靠 1.3 的
`extra_headers` 带上。被测二进制是本机 debug 构建，清单冻结了它的 sha256。三个付费批次共 66 个 run，另有 13 个
单次探针和绑定、TUI 的冒烟；按牌价（输入 $0.30/M、输出 $1.20/M，缓存读写按输入价）估算批次合计约 2.74 USD，
网关自己报的 `cost` 是 0。下文的「成本」都是牌价估算，不是账单。批次目录在 `artifacts/regress/real-*`（不入库）。
同日稍后又跑了三个批次——本节末尾的三个内核修复 A/B 与实验三，共 66 个 run、约 2.11 USD。

当晚用户指出同一网关的 `/frank` 路径是 Anthropic Messages 协议（模型 `hw-vol-deepseek`），于是又补了两个批次（各 30 个 run，合计 1.42 USD）和十几个单次探针（估计不到 0.1 USD）：一个批次走 Anthropic 协议并经本地抓包转发器逐请求核对，一个批次用修后二进制在 OpenAI 协议上把实验一原样重跑。批次目录
`artifacts/regress/reasoning-carry-{anthropic,postfix}-20261126T1350…`。

**回归任务集**（1 次重复）：4/6。`long-output` 600 秒超时，产物 15/17 行；`loop-suppression` 诚实地收手了，
但用了 14 次调用，判卷线是 12（见下面「打转护栏」）。`mcp-many-tools` 判过，但 39 次调用里 25 次是工具报错——
评测装置自己的缺陷（见下）。后面两个批次里，`long-output`、`loop-suppression` 之外的四个任务 48/48。

**实验一（回传 A/B）**：`off` 对 `on`，每臂每任务 3 次。

| 任务 | off 通过 | on 通过 | off 轮数 | on 轮数 | 成本变化 |
|---|---|---|---|---|---|
| `long-output` | 2/3 | 3/3 | 37.0 | 23.0 | −49% |
| `cjk-docs` | 3/3 | 3/3 | 4.7 | 4.3 | +62% |
| `mcp-many-tools` | 3/3 | 3/3 | 6.3 | 4.7 | −8% |
| `multi-file-edit` | 3/3 | 3/3 | 6.7 | 7.0 | +11% |
| `recover-after-failure` | 3/3 | 3/3 | 11.3 | 9.0 | +9% |
| 合计 | 14/15 | 15/15 | 13.2 | 9.6 | −39% |

达到 1.2 的放行条件（成功率不降，总 token 不升）。读法要说清楚：收益全部来自 `long-output`——`off` 三次都在
37 轮处撞满 600 秒，`on` 两次在 14–16 轮自己结束、一次 39 轮；不压缩的四个短任务反而多花约 21%（保留的推理
在同一轮的每个后续请求里重发）。网关没有拒收回显的推理，没有触发 400 回退。每格 3 次、只此一家模型，足以说明
「在推理模型上值得开」，不足以说明该对所有模型默认开。D1 已于同日由用户拍板为「默认开」：按这一家的数据放行，
别的模型与任务形态由使用者显式关掉，或者自己量——当晚的修后复测表明，「在这一家值得开」也只对 OpenAI 协议这一条线
成立（下一块）。

**修后复测**（2026-11-26 晚）：实验一跑在三个内核修复**之前**的二进制上，修复之后 `off` / `on` 两臂都没有重测过
（修复的 A/B 两臂都钉了 `on`）。当晚用修后二进制、同一条配方、同一价表，各跑一个 30 run 的批次：一个走 OpenAI 协议
（与实验一同一个端点、同一个 `deepseek-flash`），一个走 Anthropic 协议（下一块的 `hw-vol-deepseek`）。两臂都写显式值。

| 批次（每臂 15 个 run） | off 通过 | on 通过 | 轮数均值 off / on | token 均值（每 run）off / on | on 相对 off | 成本合计 off / on |
|---|---|---|---|---|---|---|
| OpenAI 协议 · 修前（实验一） | 14/15 | 15/15 | 13.2 / 9.6 | 148.6k / 89.7k | −39% | 0.828 / 0.503 USD |
| OpenAI 协议 · 修后 | 14/15 | 14/15 | 8.9 / 6.5 | 58.5k / 40.3k | −31% | 0.327 / 0.231 USD |
| Anthropic 协议 · 修后 | 14/15 | 15/15 | 9.1 / 10.5 | 67.7k / 85.1k | +26% | 0.392 / 0.471 USD |

- **三个修复本身的效果比回传开关大得多**。同一个 OpenAI 端点上，`off` 每 run 的 token 从 148.6k 降到 58.5k（−61%），
  `on` 从 89.7k 降到 40.3k（−55%）；`long-output` 每 run 的折叠从 28.7 / 17.3 次降到 3.7 / 2.0 次，`off` 臂不再有
  600 秒超时。
- **回传省不省 token，方向随端点变**。OpenAI 协议上 `on` 省 31%（实验一量到 39%），Anthropic 协议上反而多花 26%。
  差别全在 `long-output`：OpenAI 协议 199.1k → 124.9k，Anthropic 协议 221.9k → 333.0k。四个不折叠的短任务，两条线上
  逐任务都在 −55% 到 +51% 之间乱跳，实验一里「短任务多花约 21%」也没有复现（修后两条线的四任务均值反而都是省的）——
  那是同一配置跑 3 次的噪声幅度。两个端点的模型名不同，分不清差别来自协议、模型还是噪声。
- **失败的 3 个 run 都不是内核造成的**。OpenAI 协议上两个（两臂各一）是网关的 502（`upstream request failed`、
  `upstream timed out`，run 以 `error` 结束、`attempts` 为 1）；Anthropic 协议上一个（`off`）5 轮、0 次折叠就把 16 个
  文件一次读完、产物全对，只败在 `MIN_COMPRESSIONS = 2` 这条判卷线上。
- **默认值保持开（2026-11-27 用户确认）**。D1 是按 OpenAI 协议这一个端点的数据放行的；现在知道那份收益不随端点迁移。`on` 确定的
  好处只有协议层面的那一条（有的 API 在工具循环里要求带回思考块），省 token 要按端点自己量。用户看过这份复测后同意不改回去（§0.0.3 的 D1）。

**Anthropic 线补测**（2026-11-26 晚）：同一网关的 `/frank` 是 Anthropic Messages 协议，模型 `hw-vol-deepseek`
（响应里自报 `deepseek-v4-1-flash-261017`）。这是 1.1 的 Anthropic 分支第一次接到真端点。被测二进制指向本地一个只听
127.0.0.1 的抓包转发器，它把请求原样转给网关，同时记下请求体（去掉认证头）和解析后的 SSE 事件。十几个单次探针先摸清
这个端点的形状，并在探针上做三方核对——网关发来的块、会话文件里存的推理状态、下一次请求里回传的块，8 个带会话文件的
探针逐条一致（26 个请求、30 条带工具调用的 assistant 消息）。批次的 run 不落会话文件（`--session none`），跑完对
328 个请求逐条核对网关发来的块与回传的块，外加形状与用量。脚本、抓包日志与探针的会话文件归档在批次目录下的
`wire-audit/`（不入库，本机才有），三个核对脚本可以离线重跑。

功能上没有问题。上面那个 Anthropic 批次的 30 个 run、328 个请求（主循环 293 个、摘要 35 个）全部 HTTP 200：

- 网关在工具调用轮里默认返回 thinking 块（主循环响应里共 271 个：212 个有正文，中位 288 字符；59 个是空正文加签名）；
  签名走 `signature_delta`，36 个字符；块序见过三种——`[text, thinking, tool_use]`、`[thinking, tool_use]`、
  `[thinking, text, tool_use]`。**空正文带签名的块必须留**（工具调用轮里常见的形状），回传落在原位；为此补了一个
  单元测试（`empty_signed_thinking_between_text_and_tool_use_is_echoed_in_place`）。
- `on` 臂回传的每一个 thinking 块，类型序、正文、签名、位置都与网关发来的逐字节一致；882 条 assistant 消息里角色
  交替、tool_use / tool_result N:N、thinking 只出现在带 tool_use 的消息里，全部成立。其中 91 个主请求是折叠之后
  发出的——压缩之后再回传也核过。
- `off` 臂线上一条 thinking 都没有，其余块与网关发来的一致；`reasoning_effort: "none"` 时发
  `thinking: {"type":"disabled"}`，网关不再返回 thinking。
- 账单对得上：30 个 run 的输入、缓存读、输出 token，逐个与线上一笔一笔加出来的相等（含摘要请求）。
- 核对脚本做过变异自测：改签名、改正文、挪位置、丢块、改工具入参，任何一项都能被抓出来。

线上另外看到两件事，都不是回传的问题：

- **摘要调用被思考挤截断**（发现 1 在这条线上的样子）：这个端点默认带思考，摘要请求的输出额度（预算下限 1024 起）
  常被 thinking 块先花掉。24 次折叠里 11 次首发就撞 `max_tokens`（都带 thinking），自适应阶梯翻倍重试：9 次重试收尾；
  另 2 次重试的输出又恰好用满预算（网关给的停止原因仍是 `end_turn`）——一次正文为空，退回用第一段；一次正文比首发长
  （2740 对 2429 字符）、照常采用，可能仍是被截断的。截断的首发整次白花 0.029 USD（输入加输出；其中输出 18.4k token，
  占摘要输出的 26%），是这个批次成本的 3.4%。Anthropic 官方 API 默认不开思考，摘要调用不会有这笔损耗。要不要在用户没设
  `reasoning_effort` 时给摘要请求显式发 `thinking: disabled`，列为候选项、没有做：老模型或第三方端点可能不认这个键，
  收益只在默认带思考的网关上。用户 2026-11-27 同意暂不做。
- **长任务里模型反复重读**：`long-output` 在这条线上两臂轮数都多（`off` 5 / 21 / 19，`on` 13 / 16 / 47）。47 轮那一个
  每折一次就把 16 个文件重读一遍，摘要一份比一份长（2.7k → 9.3k 字符），最后的回复写着「逐行重新核对过」。这是模型的
  行为，内核没有出错；它说明这条线上的成本主要由模型的重读习惯决定，回传开关不是主因。

**限制**：网关不校验签名——去掉或改掉签名照样 200，所以这里证明的是「回传内容逐字节忠实」，不是「Anthropic 官方 API
一定收」；没出现过 `redacted_thinking`；只此一个端点、一个模型名、每格 3 次。

**实验二（压缩交接）**：`keep_recent_messages` 默认 8 对 0，每臂每任务 3 次。

| 任务 | 8 | 0 |
|---|---|---|
| `long-output` | 2/3 | 0/3 |
| 其余四个任务 | 12/12 | 12/12（两臂都没有发生过折叠，差异是噪声） |
| 合计 | 14/15 | 12/15 |

`0` 明显更差：三次都撞满 60 轮，每次 130–155 次工具调用（`read_file` 107–137 次）。时间线上看得到原因——每次
折叠后只剩一条摘要，模型就从 `part_01` 开始重读已经数过的文件。默认 8 保持，不采纳正文引用的 Anthropic 建议。
8 自己也不稳：一次把 `part_07` 数错，一次 32 次折叠里 27 次只折掉 2 条消息、最后靠 600 秒超时才停（产物是对的）。

**逐项功能的探针**（各一次，都符合预期）：

| 探针 | 结果 |
|---|---|
| 凭据卫生（2.5） | agent 之外 `printenv` 得到 41 个字符；让 agent 自己跑 `printenv TA_RELAY_KEY \| wc -c` 得到 0 |
| 错误的密钥 | exit 1、`request_error`、401，不重试 |
| `--timeout 20` | exit 124、`timeout` |
| 15 秒后 SIGINT | exit 130、`cancelled`，已生成的正文保留 |
| `max_tokens: 300` | 正常答出 391（salvage 路径没有被触发） |
| 跨进程 `--resume` | 记得上一进程里给的暗号 |
| `stream_idle_secs: 2` | 24 秒的流没有误判 |
| 三个并行 `task` 子 agent | a=49 b=20 c=106；其中一路吃到网关的 502，父 agent 自己补跑 |
| batch（`--jobs 2 --isolate copy`） | 2 个完成 + 1 个 `bad_task`，exit 1，report.json 落盘 |
| 预算 `max_total_tokens: 30000` | exit 125、`budget_exceeded`，用量在账上 |
| `mcp-many-tools`（mock 修好之后） | 判过，6 轮 23 次调用 0 报错 |
| 打开 run_code | 模型自己选了 `run_code`（5 次），一次 JS 报错下一轮改对；判卷不过只因判卷线要求至少 2 次压缩 |
| 会话文件里的摘要 | 下面第 1 条的直接证据 |

两个绑定各做了一次冒烟（钩子拒绝 `rm` 后模型看到拒绝文案并照实汇报，第二轮记得上一轮的输出）；TUI 用 pty 驱动
真模型一轮（6 秒出回复、状态回到 ready、双击 Ctrl-C 退出 0）。

**评测装置在真模型面前暴露的三个缺陷**（都先写离线用例再修，详见 `CHANGELOG.md` 与 `docs/claude/pitfalls.md`）：
批次目录按相对路径给出时 run 启动即败；mock MCP server 的 `transfer_stock` 拒收整数参数，零成本通道一直是绿的；
两条通道的 run 环境不密闭——第一次真机批次里，模型在 workdir 里跑 `git status; git diff` 解析到了外层仓库，
开发者未提交的 diff（约 30 KB）被发给了网关。现在 `run_env` 对两条通道一并隔离 `XDG_CONFIG_HOME` 并设
`GIT_CEILING_DIRECTORIES`。

**内核侧的三个发现**（写作时只有证据、没有改代码；2026-11-26 已全部修掉并配了离线回归测试——做法与当时候选做法的
出入见 §0.0.2，修前 / 修后的真模型 A/B 在本节末尾。下面的数字是复核时从批次目录的 `events.ndjson` / `result.json` 重新数出来的）：

1. **推理模型上的摘要被截断**。两个实验批次的 `long-output` 里，212 次摘要调用有 180 次（85%）的输出正好是
   1024 token 的预算下限——推理 token 计入同一份额度，每次占掉 0–992 个，正文拿到的不足 1024。会话文件里抓到
   的终版摘要在半行处断掉，最新的进展（某个文件的计数）没写进去。约四成篇幅花在逐字引用工具输出的 200 字符
   预览和复述任务目标上，而任务原文本来就留在头部。内核对「截断但非空」的摘要在首次尝试就接受（既定决策）。
   当时列的候选做法：上一次摘要被截断时抬高下一次的预算，或者对非空截断也重试一次；摘要提示词瘦身（不复述任务、
   不引用预览）——提示词是模型可见文案，改动要先走规格。
   **已修（2026-11-26）**：截断且带推理时当场以翻倍预算重试一次，外加两条跨 run 的预算学习（§0.0.2）；提示词没动。
   **复测**：修后 `long-output` 三个 run 的 21 份最终摘要里，没有一份带截断的形状（修前 82 份里 71 份）。
   数的口径：以 `compressed` 事件后面紧跟着的那条 `usage` 为准。三个批次合计 240 次摘要，204 次在下限处截断、
   其中 199 次带思考；另有 17 次预算高于下限的调用全部带思考（输出 1884–3072），19 次不到下限就自己结束。
2. **折叠抖动**。保留窗自己就超过阈值时，预检每轮都再折一次，每次只折得掉 2 条消息：一个 56 轮的 run 里
   折了 32 次、其中 27 次只折掉 2 条（三个批次的 `long-output` 合计 240 次折叠，213 次如此）。带折叠的轮次明显更慢：
   这个 run 里 32 个带折叠的轮次中位 16.1 秒（8.3–30.3），其余 24 轮 2.5 秒（1.3–7.2）；13 个发生过折叠的
   `long-output` run 合起来是中位 16.2 秒对 4.3 秒（按 `result.json` 的 `turn_timings`，折叠算在它发生的那一轮；
   初稿写的「13–36 秒对 2–6 秒」只是第一个批次那一个 run 的范围，实测 12.8–36.4 对 2.1–6.6）。当时列的候选做法：最小收益闸门
   （可折叠的前缀太短时跳过预检折叠）和 / 或自适应保留窗（窗口自身超阈值时缩到 `keep_recent_messages` 以下，
   下限是最后一个完整的工具调用对）。
   **已修（2026-11-26）**：自适应保留窗——Preflight 折叠把尾部压到半个空间以内（§0.0.2）；最小收益闸门没做。
   **复测**：同一个任务修后折叠 21 次、其中 1 次只折掉 2 条（修前 82 次、79 次）；通过 1/3 → 3/3，主请求输入 token 少约四分之三。
3. **打转护栏的空隙**。`loop-suppression` 里模型发了 9 次一模一样的 `job_status` 轮询（同参同果；中间夹着
   `list_dir` / `ping` / `list_warehouses`，不是连着发的），最长连续 3 次：
   第 3 次的 nudge 文案是「换一个工具」，模型照办，于是连续计数被清掉，硬停（6 次）永远够不着。护栏只数
   「连续」重复是有意的（不误伤正常的轮询），当时列的候选做法是另加一个不要求连续的同参重复计数，配一条
   针对轮询的 nudge；硬停线是否跟着调要单独量。
   **已修（2026-11-26）**：`RepeatWindow`（最近 8 次调用）在同键第 4、6、8… 次出现时附一条只提示不停的提醒；
   硬停线没动。把这条真实轨迹在新逻辑上重放，提醒落在该键的第 4、6、8 次出现（全部调用里的第 6、10、13 次）；
   模型会不会因此更早收手，A/B 量不出来：每侧 3 个 run，通过 1/3 对 2/3、工具调用 46 次对 37 次，方向一致但落在噪声里。

**三个内核修复的真模型 A/B**（2026-11-26）：同一条配方跑两个二进制。修前是修复落地之前构建的
`tiny-agent-pre-kernel-fixes-20261126`，修后是最终代码树的同款构建（都只带默认 feature）；两臂都显式写
`preserved_reasoning: true`，底配置、任务与价表相同。回归集 6 个任务 × 每任务每臂 3 次，逐任务两侧交错、全程串行。
修前的二进制不认识 `limits.summary_preview_chars`，那一侧经 `tiny-agent-pre-wrapper-20261126.py` 运行——它只删掉
这一个键（200 本来就是修前写死的宽度）。

| 任务 | 通过 修前 → 修后 | 轮数均值 | token 均值（每 run） | 成本合计 |
|---|---|---|---|---|
| `long-output` | 1/3 → 3/3 | 34.7 → 28.7 | 520k → 354k | 0.564 → 0.391 USD |
| `loop-suppression` | 1/3 → 2/3 | 16.3 → 11.7 | 42.7k → 30.9k | 0.043 → 0.031 USD |
| `cjk-docs` | 3/3 → 3/3 | 5.0 → 5.3 | 27.5k → 27.6k | 0.043 → 0.040 USD |
| `mcp-many-tools` | 3/3 → 3/3 | 5.0 → 4.7 | 13.6k → 12.9k | 0.015 → 0.015 USD |
| `multi-file-edit` | 3/3 → 3/3 | 6.3 → 6.7 | 17.3k → 19.6k | 0.018 → 0.021 USD |
| `recover-after-failure` | 3/3 → 3/3 | 11.0 → 10.0 | 43.4k → 36.6k | 0.046 → 0.038 USD |
| 合计 | 14/18 → 17/18 | 13.1 → 11.2 | 110.7k → 80.2k | 0.729 → 0.536 USD |

`long-output` 是唯一会折叠的任务，也是唯一有定论的任务。三个 run 合计：

| 指标 | 修前 | 修后 |
|---|---|---|
| 折叠次数 | 82 | 21 |
| 其中只折掉 2 条消息 | 79 | 1 |
| 折叠后留下的消息数（`kept`） | 82 次都是 9 | 3 条 11 次，9 条 9 次，5 条 1 次 |
| 最终摘要带截断的形状 | 71 | 0 |
| 每份摘要的输出 token（均值） | 1072 | 2834 |
| 摘要输出合计 | 87,928 | 59,514 |
| 主请求输入合计 | 129.6 万 | 33.5 万 |
| 单个主请求的输入（各 run 的均值） | 0.9 万–1.5 万 | 3.0 千–5.2 千 |
| 单个 run 的耗时 | 600 / 447 / 221 秒 | 346 / 92 / 219 秒 |
| 带折叠轮次的中位耗时 / 其余轮次 | 13.1 秒 / 3.8 秒 | 16.6 秒 / 3.9 秒 |

「带截断的形状」指一次折叠上报的用量恰等于一个预算、或恰等于两个预算之和（每次折叠只上报一条合并后的 `usage`，
重试的次数要从数值里认）。

- **自适应保留窗**（发现 2）：预检折叠多数一次折掉 8 条，不再每轮只折 2 条，折叠次数降到修前的四分之一。
- **摘要重试**（发现 1）：修后每个 run 的预算下限都被抬了两次（1024 → 2048 → 4096），21 次折叠里 6 次走了重试，
  没有一份最终摘要带截断的形状。摘要的输出总量前后是同一个量级（8.8 万对 6.0 万 token）；省的是主请求——
  历史小了，单个主请求的输入从上万 token 降到三五千。
- **代价**：带折叠的轮次更慢（摘要更长，截断时还要多发一次请求），但折叠少了，三个 run 的总耗时 1268 → 657 秒。
- **修前的两个失败**：一次 600 秒超时（产物 9/17 行）；一次在第 34 轮被网关的 502 打断（`upstream request failed`，
  `attempts` 为 1，run 以 `error` 结束，见「其他观察」）。修后输入最多的一个 run，输入也只有修前最少那一个的 54%。
- **其余四个任务**两侧 12/12，token 变化 −16% 到 +13%：三个修复只在折叠时起作用，这个幅度是同配置下的噪声。
  同一个修后二进制、效果相同的配置，`long-output` 先后两组三连跑的轮数是 42 / 13 / 31 与 19 / 8 / 11——每格 3 次
  重复看得出量级差别，看不出两三成的差别。
- **隔次重复提示**（发现 3）量不出效果：只有 `loop-suppression` 会触发它。修前三次的调用数是 17 / 8 / 21，其中一次
  `job_status` 与 `ping` 交替着发；修后是 20 / 7 / 10，交替的形态没再出现，但修后也有一次 run 里 `job_status` 轮询了
  8 次、20 次调用才结束。相邻同参同果 10 → 6 次，窗口内重现 30 → 15 次。每侧 3 个 run，通过 1/3 对 2/3 说明不了方向。

**实验三（摘要预览宽度）**（2026-11-26）：`summary_preview_chars` 默认 200 对 1000，修后二进制，五个任务 × 两臂 ×
3 次，逐任务两臂交错、全程串行。只有 `long-output` 会折叠；其余四个任务两臂都是 12/12、一次都没折，它们两臂之间
的 token 差异（−6% 到 +62%）就是同一配置跑 3 次的噪声幅度。

| `long-output`（三个 run） | 200 | 1000 |
|---|---|---|
| 通过（按判卷线） | 2/3 | 1/3 |
| `out/counts.tsv` 正确 | 3/3 | 2/3 |
| 折叠次数 | 7 | 21 |
| 其中走了摘要重试 | 5 | 7 |
| 轮数均值 | 12.7 | 29.3 |
| token 均值（每 run） | 126k | 440k |
| 成本合计 | 0.140 USD | 0.479 USD |
| 每份摘要的输入 / 输出 token（均值） | 1960 / 2778 | 4840 / 3453 |
| 带折叠轮次的中位耗时 | 27.5 秒 | 28.0 秒 |

预览加宽到 1000 字符没有更好。摘要请求的输入涨到 2.5 倍、摘要更长（三个 run 里两个的预算下限一路抬到 8192）。
1000 字符的三个 run：一个 9 轮就正确结束，一个 29 轮通过（68 次工具调用），一个 50 轮撞满 600 秒、只写出 9 行
（89 次）；后两个把 16 个文件整批重读了好几遍，思考记录里写着不信任摘要的缘由——「summary recorded only
fragments. To be safe, re-read 10-14」「The summary says all were unverified placeholders. … We should re-read
part_01 through part_12 fully to verify」。200 字符的三个 run 都在 8–19 轮内结束、产物全对；最长的那一个折叠后
也补读了 4 个文件，没有蔓延成整批重读。默认 200 保持（正文 §2.2 写的是「有效才做成旋钮」，量下来无效：旋钮已经加了，
默认值不动）。每臂 3 个 run、只此
一家模型，「更宽的摘要诱发核对」是从思考记录里读出来的解释，不是量出来的结论。

**判卷线：已从 2 次降到 1 次（2026-11-27 用户同意）**：`long-output` 的 `check.py` 要求至少 `MIN_COMPRESSIONS` 次压缩，
为的是确认这个任务真的走过了长对话。三个修复之后一次折叠折得多，8–9 轮就正确完成的 run 只折叠 1 次——200 字符臂
与 1000 字符臂各有一次这样的 run，产物全对，被这条线判成失败（上面探针表里「打开 run_code」那一条也是同一个原因）。
上面各表的「通过」照当时的判卷线（2 次）记录，没有事后回改；脚本化 mock 的 `check` 不受影响。

停在 1 次、不降到 0：这条线的用处是「至少真的压缩过一次、结果仍然全对」，降到 0 就只剩数数了。改完对着留存的 workdir
把已有的真模型 `long-output` run 重判了一遍（零成本，不发请求）：24 个 run 里判定变了 2 个——实验三里折叠 1 次的那两个
（`preview200/2`、`preview1000/1`）由失败变为通过；其余 22 个不变。修后二进制上 21 个真模型 run 的折叠次数是 0 次 1 个、
1 次 2 个、2 次及以上 18 个；当晚修后复测的两个批次里 12 个 run 是 0 / 4 / 4 / 2 / 3 / 11（Anthropic 协议，先 `off`
三次、后 `on` 三次）与 3 / 5 / 3 / 2 / 2 / 2（OpenAI 协议）。那个 0 次折叠的（Anthropic 协议 `off/1`，5 轮一次读完
16 个文件、产物全对）仍然不过。模型一次都不折叠时这条线管不了；要让真模型也必定折叠只能把任务加大，没有做。

**其他观察**：网关在 3 路并发流时回过一次 502（`upstream timed out`），两个批次各一个进程并行时 60 个 run
里 0 次；后面全程串行的三个批次 66 个 run 里又有 1 次（`upstream request failed`，见上面的 A/B）；当晚全程串行的两个批次 60 个 run 里又有 2 次（都在 OpenAI 协议的
`long-output`，两臂各一：`upstream request failed`、`upstream timed out`），Anthropic 协议的 328 个请求 0 次——
并发不是它唯一的成因；超时或被取消的流拿不到 usage，result.json 里记成 0，与「真的花了 0」分不开（内核没有「未上报」
标记）；`run_code` 的报错位置是「出错前最近一个带位置信息的语句」，不保证是出错的那一行（已记入
`docs/claude/architecture.md`）。

## 1. 目标与取舍尺

目标：让**无人值守执行**更**可信、可审计、更便宜、更可移植**。这四个词取自 README 的「特性试金石」，本计划既用它们排序，也用它们筛除。

过程中不改变的约束：

- 单二进制、同步（无 tokio）、轻依赖、musl 静态。
- 0.x 阶段 minor 是破坏性窗口，破坏性改动集中落在一个 minor。
- 账单面诚实：失败路径也要留下成本与归因。
- LLM 可见文案即规格。
- 新能力默认零漂移：不开启时，历史、事件流、请求体与现在逐字节相同。

收项规则：同时命中试金石且不违背 README「Non-goals」的进主线；与 Non-goals 冲突的不进主线，单列在 §8 交你决定。

## 2. 总览

| 阶段 | 建议版本 | 主题 | 工作项 | 前置 |
|---|---|---|---|---|
| 0 | 下一个 patch | 即修项与评测基线 | 0.1 MCP 翻页 · 0.2 量纲修正 · 0.3 Skills 块标量 · 0.4 回归任务集 | 无 |
| 1 | v0.5.0 | 推理状态与请求整形 | 1.1 推理状态回传 · 1.2 实验一 · 1.3 请求透传 · 1.4 推理 token 账 · 1.5 模型窗口 · 1.6 MCP 注解 | 0.4 |
| 2 | v0.6.0 | 上下文工程与 MCP 现代化 | 2.1 回传默认值 · 2.2 压缩交接 · 2.3 工具体量提示 · 2.4 MCP 补齐 · 2.5 无人值守默认值 | 1.1、1.2 |
| 3 | v0.7.0 起，按需 | 大件与可选件 | 3.1–3.7 | 见各条 |

体量粗估（单人、熟悉代码）：S 不超过 1 天，M 2–4 天，L 1 周以上。按这个口径，阶段 0 约 1 周，阶段 1 约 3–4 周，阶段 2 约 2–3 周，不含实验等待与发版。

排序理由：

1. **推理状态回传**是我读到的最明确的一处"代码注释的前提已被厂商文档推翻"，而且涉及你正在用的 GLM。
2. **账单面**是这个项目的护城河。推理 token 这一轴现在缺失，评估第 1 条也用得上。
3. **MCP 翻页**是正确性缺陷，改动很小。
4. **评测基线**要先于一切"改默认值"的决定，所以放在阶段 0。
5. 安全方向在上一轮排最前，这一版下调并拆成三块，原因见 §2.1。

### 2.1 相对上一轮评估的修正

上一轮我建议做"内置命令与路径规则、TUI 审批环节、OS 沙盒"。写计划时对照 README，发现它们与两条 Non-goals 直接冲突：

- 「不给 `shell` 自建沙箱 / 命令过滤」（`README.zh-CN.md:630`）。理由是半吊子过滤只制造虚假安全感，隔离的官方答案是在容器或受限用户下运行。
- 「TUI 保持极简定位」，明确不对标 Claude Code / Codex。

另外「安全说明」（`README.zh-CN.md:590`）写明：每个子进程继承完整宿主环境，包括 `api_key_env` 的值，这是按设计的行为。

所以安全方向拆成三块：

- **不冲突，进主线**：MCP 每 server 工具名单（2.4），把 MCP 注解暴露给 Hooks（1.6）。审批仍由嵌入方经 Hooks 实现。
- **不冲突，但改默认行为，需你拍板**：子进程默认不继承 provider 密钥（2.5）。
- **与 Non-goals 冲突，不进主线**：内置规则引擎、TUI 审批界面、Landlock / `sandbox-exec`（§8 D3）。

## 3. 阶段 0：即修项与评测基线

这一阶段全是修复或加法，可以并入下一次发版。

### 0.1 MCP `tools/list` 翻页（S）

- **现状**：`crates/core/src/mcp.rs:235` 只调用一次，参数为空。分页的服务器，第二页起的工具被静默丢弃。
- **设计**：循环调用并带 `cursor`，直到响应没有 `nextCursor`。页数上限 64，工具总数上限沿用 `MCP_MAX_TOOLS`（256）。跨页重名忽略后到者并告警。
- **测试**：用脚本化 Transport 喂两页响应，断言合并结果；单页服务器的请求序列不变。
- **验收**：两页 server 的工具全部可见；现有用例全绿。CHANGELOG 记 `Fixed`。

### 0.2 `refresh_tool_defs` 量纲（S）

- **现状**：`crates/core/src/agent.rs:1686` 用字节数累加，构造路径用 CJK 感知的 `tool_schema_units`（`agent.rs:1351-1358`）。热增删 MCP server 之后，两者量纲不一致。
- **设计**：改用 `tool_schema_units`。
- **测试**：热增一个带中文描述的工具，断言与构造路径的值相等。

### 0.3 Skills frontmatter 块标量（S/M）

- **现状**：`crates/core/src/skills.rs:463` 是手写的单行解析器。`description: >` 或 `|` 会走到 `FrontmatterParse::BlockScalar`（`skills.rs:305`、`skills.rs:532`），技能不加载。Agent Skills 规范允许这种写法，生态里的技能可能因此装不上。
- **设计**：支持标量的块形式（`|`、`>`，含 `|-`、`>-` 等结尾指示符）。literal 保留换行，folded 折叠行，按缩进判定归属。不引入 YAML 依赖。多行对象与数组仍不支持，保持"其它键忽略"。
- **测试**：表驱动用例覆盖 `>`、`|`、缩进、CRLF、空行、结尾指示符，以及与 `metadata` 嵌套共存的歧义。
- **文档**：README 两份的「`SKILL.md` 格式」节。

### 0.4 回归任务集与评测脚本（M）

后面每一项"改默认值"的决定都需要数据。`scripts/code_value_eval.py` 已经定好了付费评测的纪律，照搬即可：默认 dry-run，显式 `--execute` 才付费，价表与费用停止值缺一即拒绝，结果封存为 JSON。

- **任务集**：放 `examples/regress/tasks/`，用现有 batch 任务文件格式。起步六类：
  1. 多文件修改：读 3–5 个文件、改 2 处、跑一条校验命令。
  2. 长输出工具：多次读取大文件，足以触发 2 次以上压缩，之后仍完成原任务。
  3. 多工具 MCP：本地 stdio server 暴露 8–12 个工具，任务只用其中几个。
  4. 失败后改道：第一个工具调用必然报错，要求换路径完成。
  5. CJK 密集：中文文档的整理与改写。
  6. 打转抑制：诱导模型重复同一调用，考察是否被护栏停下。

  每个任务的通过条件必须可机器判定（产物文件的内容或哈希、校验命令的退出码），不依赖模型评审。
- **脚本**：`scripts/regress_eval.py`。读冻结配方（模型、配置覆盖、重复次数），调 CLI 的 `--json` 与 `--output`，汇总 `result.json`，输出臂间对比表。指标：成功率、平均轮数、input / cache read / output token、压缩次数、耗时。
- **零成本检查**：用 `examples/mock_openai.py` 与 `mock_anthropic.py` 跑通全链路，进 CI。付费部分只在本地手动触发。
- **验收**：mock 下全绿；同一配置跑两次，汇总可复现。

## 4. 阶段 1（v0.5.0）：推理状态与请求整形

这一阶段集中所有破坏性改动。原则是**字段一次加齐，行为用开关挡住**：`Usage`、`McpToolInfo`、`ToolCallRecord`、`ToolCtx` 的计划内字段都在这个版本加上，对应的功能可以晚到。

### 1.1 推理状态回传（L）

**现状**

- 思考只发事件、不进历史。`crates/core/src/provider/mod.rs:197-203` 的注释给的理由是"没有任何主流 API 要求下一轮回传"。
- GLM 文档的要求相反：使用"交错思考 + 工具"时，"必须显式保留 Reasoning content，并在返回工具结果时一并返回"。保留式思考在标准 API 端点自 GLM-5.4 起默认开启。
- Anthropic 不回传不报错，但模型拿不到早先的推理。OpenAI 的 Responses API 同样要求带回 reasoning items。
- Anthropic 端连 `signature_delta` 都不读（`provider/anthropic.rs:451`）。OpenAI 兼容端只在响应侧读 `reasoning_content`（`provider/openai.rs:844` 起）。

**设计**

采用消息级旁路字段，不新增 `Block` 变体：

```rust
pub struct Message {
    pub id: Option<String>,
    pub role: Role,
    pub content: Vec<Block>,
    /// 原样回传的不透明推理状态。永不渲染，永不进 text/blocks。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
}

pub struct Reasoning {
    /// 产出它的 provider 种类；请求侧只回传给同一种类。
    pub provider: String,
    /// 厂商私有载荷，逐字节透传。
    pub payload: serde_json::Value,
}
```

不放进 `content` 的原因：内核多处依赖"块的形状"做判定，例如空回复不落盘、`on_turn_stopping` 的零块规则、salvage 判定、子 agent 的 `final_text`。旁路字段让这些不变量原样成立，`ThinkingDelta` 现有的纪律也不变。

要点：

1. **产出**。`ChatResponse` 增加 `reasoning` 字段。Anthropic 把 `Pending::Thinking` 扩成带签名与 redacted 数据的缓冲；OpenAI 兼容端拼接 `reasoning_content`。只在开关打开且响应确实带推理时产出。
2. **回传**。Anthropic 把 thinking / redacted_thinking 块放在 assistant content 最前，请求未启用思考时不回传。OpenAI 兼容端在 assistant 消息上加 `reasoning_content`，它无法判断思考是否开启，有就回传。两边都只回传 `provider` 匹配的。`ChatRequest` 不需要加字段。
3. **失效规则**：任何历史改写都使其后的推理状态失效。
   - `fold_summary`：保留的尾部一律剥掉，归档的原件保留。
   - `prune_tool_results`：从第一条被改写的消息起剥掉。
   - `set_model`：全量剥掉。
   - 相邻同角色消息合并：合并后的消息不带推理。

   实现上收成一个函数，所有改写点都调它。
4. **网关兼容**。有的 OpenAI 兼容网关会拒绝 assistant 消息里的 `reasoning_content`。增加第 4 个学习型修复 `GatewayFixes.no_reasoning_echo`，与现有三条同机制。
5. **计量**。推理计入 token 预估（`compress.rs` 的 `block_units` 与 `block_chars`）；`MessageKinds` 增加 `reasoning` 桶，并入单趟 running-floor，保持等和不变量。
6. **开关**。`Config.preserved_reasoning: bool`，本阶段默认 `false`。
7. **展示面不变**。`/thinking`、`/export`、TUI 转录继续只读事件。

**测试**

- Provider 级：Anthropic 的 SSE fixture（thinking → thinking_delta → signature_delta → stop）得到正确形状；请求体里 thinking 块在最前；没有签名的 thinking 直接丢弃；`redacted_thinking` 透传。OpenAI 端：`reasoning_content` 回传；学习型修复触发后重发不带该字段。
- Loop 级（`agent_tests.rs` + `provider/mock.rs`）：开关关闭时历史逐字节不变；打开时第二轮请求带上一轮推理；fold、prune、`set_model`、合并之后都没有残留；只有推理没有可见块的回复仍按空回复处理；`final_text` 不含推理。
- 会话：带推理的会话往返；旧文件照常加载。
- 绑定：`messages()` 带出新字段并原样回读。

**验收**

- 上述测试在四套 clippy 配置、`cargo fmt`、`--features mock`、两个绑定下全绿。
- GLM-5.4 实跑一条多工具任务，抓请求确认 `reasoning_content` 原样回传且没有 400。
- 文档同步：`provider/mod.rs` 的注释、`config.rs` 里 `strip_think_tags` 的说明、CLAUDE.md「推理通道」一条、`docs/claude/architecture.md`、README 两份。会话持久化一节注明 session 文件会包含推理文本。

**风险**

| 风险 | 应对 |
|---|---|
| 网关拒绝回传字段 | 学习型修复，可预置 |
| 输入 token 上升 | 由实验 1.2 定默认值 |
| Anthropic 前缀校验返回 400 | 失效规则加测试；400 原文不隐藏 |
| session 文件变大，含推理文本 | README 注明；开关可关 |
| 换 provider 后恢复旧会话 | 按 `provider` 过滤 |

"GLM 在关闭思考时是否接受历史里的 `reasoning_content`"没有文档依据（待确认），靠学习型修复兜底。

如果实验显示 token 成本偏高，可以加一档"只保留本轮"。Anthropic 允许删除最旧的推理块。

**提交切分**（每个提交自身过四套 clippy）：账单字段 → 透传 → 内核旁路字段与失效规则 → Anthropic → OpenAI → 绑定 → 文档。

### 1.2 实验一：回传 A/B（M）

- **配方**：GLM-5.4，0.4 的回归任务集，两臂（关 / 开），每臂每任务至少 3 次重复，同一天跑完。有余力再加 Claude 作第二家。
- **指标**：成功率、平均轮数、总 token 分轴、耗时、压缩次数。
- **放行条件（建议）**：成功率不降，且总 token 不升；或 token 略升但成功率明显提高。满足则在阶段 2 把默认值改为开，否则保持 opt-in，并把结论写进 README。

### 1.3 `extra_body` / `extra_headers` 透传（M）

- **现状**：请求体的键写死（`provider/openai.rs:400-432`），没有透传入口。GLM 的思考开关是 `thinking.type` 与 `clear_thinking`，现在发不出去。
- **设计**：
  - `Config.extra_body: Option<Value>`，必须是对象；`Config.extra_headers: Option<BTreeMap<String, String>>`，值按凭据处理，Debug 掩码照 `McpServerConfig` 的手写实现。
  - `extra_body` 不得覆盖结构键（`model`、`messages`、`system`、`tools`、`stream`、`stream_options`），加载期报错。标量旋钮允许覆盖，与顶层字段重复时告警一次。
  - `extra_headers` 不得覆盖内核管理的鉴权与协议头。
  - 在 `provider::build`（`provider/mod.rs:469`）注入 provider 实例。
  - batch 任务片段不得设置，与 `base_url` 同列进 `forbidden_batch_override`（`config.rs:1390`）。
- **测试**：合并优先级、保留键拒绝、Debug 掩码、batch 拒绝、两个 provider 的请求体快照。
- **文档**：README 配置节给 GLM 的 `thinking` 示例。

### 1.4 推理 token 进账单面（S）

- **现状**：`Usage` 有四个轴（`provider/mod.rs:233`），内核里没有 `reasoning_tokens`。
- **设计**：`Usage.reasoning_tokens: u64`（`#[serde(default)]`）。OpenAI 兼容端解析 `completion_tokens_details.reasoning_tokens`。它是 `output_tokens` 的子集拆分，所以**不进 `total()`**，否则重复计数。result.json 与批量汇总各加一个可选键，`RESULT_SCHEMA_VERSION` 不升。
- **待确认**：Anthropic 是否单列推理 token。不单列则保持 0。
- **迁移**：外部按字面量构造 `Usage` 的代码会编译失败，CHANGELOG 写迁移说明。

### 1.5 模型窗口（S）

- **现状**：`max_context_tokens` 默认 0（`config.rs:154`），按 token 的预检闸门默认不生效，压缩退回 `compress_threshold_chars` 的字符阈值。字符阈值按"4 字符约 1 token"折算，CJK 文本不适用这个比例。`max_tokens` 默认 8192，而思考 token 计入输出额度，对默认开思考的模型偏小。
- **设计**：
  - 两个默认值都不改。启动时若 `max_context_tokens == 0`，给一次 diag 提示。
  - 响应以 `max_tokens` 截断且本轮有思考增量时，diag 提示"思考占用了输出预算，考虑调大 `max_tokens`"。
  - 可选：一张很小的模型前缀表（上下文窗口、最大输出），只在用户未配置时自动填充；配置覆盖优先，未知前缀不猜。表会过时，这一项单独评审。
- **文档**：README 配置节说明两个参数的关系。

### 1.6 MCP 注解补全（S/M）

- **现状**：只读 `annotations.readOnlyHint`（`mcp.rs:146`、`mcp.rs:352`）。`structuredContent` 只作文本回退（`tools/mcp.rs:133-141`）。
- **设计**：
  - `McpToolInfo` 增加 `destructive_hint`、`open_world_hint`、`idempotent_hint`，都用 `Option<bool>`，保留"未声明"与"声明为假"的区别；另加 `output_schema`。
  - `ToolCtx` 增加按工具名索引的注解旁表，Arc 共享。`Hooks::before_tool` 的实现者就能按注解决策，审批仍留在宿主一侧。
  - `ToolCallRecord` 记录调用时的注解快照，审计可归因。
  - 工具声明了 `outputSchema` 且结果带 `structuredContent` 时，优先把后者的 JSON 文本给模型。
- **纪律**：`ToolCtx` 与 `ToolCallRecord` 加字段，要同步全部字面量构造点（含两个绑定）。
- **待确认**：三个注解在规范里的缺省值。实现不依赖缺省。

## 5. 阶段 2（v0.6.0）：上下文工程与 MCP 现代化

### 2.1 回传默认值（S）

按实验一的结论决定 `preserved_reasoning` 的默认值。改默认值属于行为变更，放 minor。

### 2.2 压缩交接（M/L）

- **现状**：
  - 摘要模型看到的工具入参与结果都经 `preview()` 截到 200 字符（`compress.rs:804`、`compress.rs:810`）。
  - 摘要预算为被压长度的 1/16，夹在 1024 到 4096 token。
  - 没有压缩前的钩子。`Agent::compact()` 在 CLI 与 TUI 里都没有调用方。
- **设计**：
  1. **实验二**：`keep_recent_messages = 0`（整段压成一条，`compress.rs:714` 已支持）对比默认的 8。Anthropic 把前者标为推荐做法。零代码，只改配置。
  2. **实验三**：摘要窗口里工具结果的预览上限，200 对比 1000。预览放宽会让分块数上升、摘要调用变多，所以先量再做。有效则做成 `Limits.summary_preview_chars`，归 `Clamp` 的 `Tighten` 类。
  3. **钩子**：`Hooks::on_before_compact(&self, info: &CompactionInfo)`。折叠前同步调用，默认 no-op，按观察类钩子的纪律处理（异常吞掉并 diag）。`info` 带触发原因、将被折叠的消息数与字符数、当前 todo 清单。宿主可借此在压缩前把笔记落盘。
  4. **TUI `/compact`**：仅空闲时可用，复用现有的 `Compressed` 事件展示审计。
- **测试**：钩子在各触发路径（预检、溢出、手动）都被调到且只调一次；钩子 panic 不影响压缩；`/compact` 的 TUI 测试。
- **文档**：README「上下文压缩」节写入实验结论。

### 2.3 工具定义体量提示（S）

- **现状**：`context_report.rs` 已按四类来源分桶，但没有阈值判断。
- **设计**：工具定义的估算 token 超过 10k，或工具总数超过 30 个时，在 `/context` 与 diag 里提示。阈值取自 Anthropic 的 Tool search 文档，标注为经验值。只提示，不自动处置。

### 2.4 MCP 补齐（M）

| 项 | 设计 |
|---|---|
| 协商版本 | 读取 `negotiatedProtocolVersion` 并保存，后续 HTTP 头用协商值。与支持列表不符时告警，不硬失败。 |
| 超时 | `McpServerConfig.call_timeout_secs` 与 `connect_timeout_secs`，默认沿用 120 与 15（`mcp.rs:16-17`）。 |
| 工具名单 | 每 server 的 `allow_tools` / `deny_tools`。白名单里出现服务器没有的名字，装配期报错，防拼写错误。batch 任务不得覆盖。 |
| 取消通知 | `tools/call` 等待期间被取消时发 `notifications/cancelled`，尽力而为。 |
| `ping` | HTTP 连接空闲较久时，调用前探测。stdio 已有 reader 死亡锁存，不需要。 |
| 2026-07-28 规范 | 跟踪项。先确认新规范的服务器是否仍须接受旧的 `initialize` 客户端（待确认），再定升级节奏。 |

### 2.5 无人值守默认值（M，需你拍板，见 D2）

**凭据卫生**

- **现状**：子进程完整继承环境（`README.zh-CN.md:590`）。内核里没有任何 `env_remove`。
- **提议**：
  - shell 与 MCP stdio 两条 spawn 链默认移除 `api_key_env` 指向的变量。run_code 经 shell 的那条自然覆盖。
  - 新增 `env_passthrough: Vec<String>` 显式放行。
  - 启动期 diag 提示"已对子进程隐藏 X，需要时加 `env_passthrough`"。
- **收益**：它挡住的是最常见的泄露方式，也就是模型顺手执行 `env` 或 `echo $KEY`。这类输出现在会进入模型上下文、session 文件和 `--json` 事件流。对无人值守的批量任务，这是"可信、可审计"上的直接改进，成本接近零。
- **定位**：它与 `restrict_to_workdir` 同级，防顺手，不防对抗。同用户的子进程仍可能读到父进程的环境（Linux 的 `/proc/<pid>/environ`、macOS 的 `ps eww`，凭记忆，待确认）。要真正的隔离仍然靠容器或受限用户。
- **风险**：依赖同一密钥变量的子进程会失败。靠放行配置与启动提示缓解。
- **测试**：两条 spawn 链各一个探针用例，断言变量不可见；放行用例；Windows 的 `cmd /C` 链单独一条。
- **文档**：改写 `README.zh-CN.md:590` 及其英文对应句，CHANGELOG 记为行为变更。

**批量模式提示**

- 批量或无人值守模式下 `restrict_to_workdir` 未开启时，启动期给一次 diag 提示。不改默认值。

## 6. 阶段 3（按需）

| 编号 | 工作项 | 内容 | 体量 | 进入条件 |
|---|---|---|---|---|
| 3.1 | MCP 现代化 B | 同一 server 并发调用；`tools/list_changed` 后惰性刷新；OAuth 设备码流程；按协商结果启用新规范特性 | L | 慢工具阻塞，或要用需 OAuth 的远程 server |
| 3.2 | 工具搜索与延迟加载 | 内核维护"已注册 / 已激活"两层工具集，搜索命中后热加载 | L | 2.3 的提示在真实场景频繁触发 |
| 3.3 | `/responses` provider | 新增 `ProviderKind`；映射 SSE 事件、reasoning item 与 function call；复用 1.1 的旁路字段 | L | 要用 OpenAI 的 GPT-6 系列，它们的 function calling 只在 Responses API 上支持 |
| 3.4 | 会话 fork | 按消息 id 截断历史后重建 agent。只做对话 fork，不承诺文件回滚 | M | 宿主提出需求 |
| 3.5 | 追踪关联 | result.json 与 NDJSON 增加 `run_id` / `turn` 关联字段；OTLP 导出做成 CLI 的可选 feature，内核不加依赖 | M | 要接入可观测平台 |
| 3.6 | 子 agent 续跑 | 保留子 agent 的历史，支持追加指令 | M | 评测显示子 agent 一次性返回是瓶颈 |
| 3.7 | 费用估算 | 可选 `pricing` 配置，result.json 输出 `estimated_cost` | S | 需要在报表里直接看钱 |

工具搜索会破坏一次缓存前缀，这是它排在 2.3 后面的原因。

## 7. 横切事项

**版本与兼容**

- 阶段 0 走 patch。
- 阶段 1 集中破坏性改动：`Message`、`Usage`、`ChatResponse`、`McpToolInfo`、`ToolCallRecord`、`ToolCtx` 加字段，`GatewayFixes` 加位。外部按字面量构造这些类型的代码会编译失败，CHANGELOG 逐项写迁移说明。
- 阶段 2 的默认值变更走 minor。
- 发版时同步 6 处版本号与 CHANGELOG，按 README「发布流程」。

**每个工作项的完成定义**

- 四套 clippy 配置、`cargo fmt --all`、`cargo test --features mock`。触及 run_code 时另跑 `--features code,mock`。
- 改到 `agent.rs` 核心路径，必须有 `agent_tests.rs` 的 loop 测试。
- 两个绑定的回归；动了装配或 run_code 时跑双构建。
- README 两份同步；`docs/lib_api.md`；CLAUDE.md 的索引行与 `docs/claude/` 的详录。
- 新增源文件后跑 `scripts/gen_structure.py --write`。
- 改了 loop、取消、打转行为后跑 `scripts/e2e_scenarios.py`。
- 事件加字段与消费方的 match 臂落在同一个提交里，保证 bisect 可编译。

**提交**：中文 message，不含署名行；git 命令前置 `-c core.precomposeunicode=false`；多行用 `-F`。

**灰度原则**：所有改变行为的能力先 opt-in 落地，再由 0.4 的评测数据决定默认值。

## 8. 需要你拍板的事项

| 编号 | 事项 | 我的建议 |
|---|---|---|
| D1 | 1.1 的落地节奏 | 0.5.0 先 opt-in，实验一达标后 0.6.0 默认开 |
| D2 | 2.5 凭据卫生：是否接受"默认隐藏"的行为变更 | 接受。它改写的只是 README 里一句被夸大的表述 |
| D3 | 是否修订 Non-goals，纳入内置规则引擎、TUI 审批、OS 沙盒 | 不修订，理由见下 |
| D4 | 是否需要 `/responses` provider（3.3） | 没有使用 OpenAI 最新模型的计划就暂缓 |
| D5 | 里程碑划分与版本号 | 按 §2，可按你的发版节奏调整 |

实施时这五项都按「我的建议」一列默认采用了，见 §0.0.3（含每项改判的成本）。其中 D1 已于 2026-11-26 由用户拍板改判为「默认开」，已落地。

D3 的理由：无人值守场景的最小权限需求由 1.6 与 2.4 覆盖，审批逻辑留在宿主。OS 沙盒另有一点：Landlock 要求内核 5.13 以上（凭记忆，待确认），低配嵌入式设备常跑更老的内核，恰好用不上；开发机与 CI 则本来就该在容器里跑。

## 9. 明确不做

- tokio。
- 内置向量库或跨会话记忆。
- 多 agent 团队框架。
- 默认构建里引入 OTel SDK。
- 内置审批界面、命令规则引擎、OS 沙盒，除非 D3 改判。

## 10. 参考

2026-11-25 核对。标"摘要"的是经摘要工具读取的二手内容。

- [GLM 思考模式（智谱官方文档）](https://docs.bigmodel.cn/cn/guide/capabilities/thinking-mode)
- [Anthropic Preserved thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking)
- [Anthropic 支持文章：Preserved thinking 变更说明](https://support.claude.com/en/articles/16761192-preserved-thinking-changing-how-the-messages-api-handles-thinking-blocks-to-protect-against-distillation)
- [Anthropic Compaction](https://platform.claude.com/docs/en/build-with-claude/compaction)
- [Anthropic Tool search tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool)
- [OpenAI Reasoning models](https://developers.openai.com/api/docs/guides/reasoning)
- [Codex Agent approvals & security](https://developers.openai.com/codex/agent-approvals-security)
- [Claude Code permissions](https://code.claude.com/docs/en/permissions)
- [Claude Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview)
- [MCP 2026-07-28 规范发布公告](https://blog.modelcontextprotocol.io/posts/2026-07-28)
- [Agent Skills 规范](https://agentskills.io/specification)
- [Anthropic Labs：Effective harnesses for long-running agents](https://www.anthropic.com/research/effective-harnesses-for-long-running-agents)（摘要）
- [Anthropic Labs：Harness design for long-running apps](https://www.anthropic.com/research/harness-design-long-running-apps)（摘要）
- [SiliconFlow 交错思考](https://docs.siliconflow.com/cn/userguide/guides/interleaved-thinking)（摘要，作为 GLM 回传规则的旁证）
