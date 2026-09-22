# M0 技术探针报告

> 对应 `docs/code_llm_design.md` §7「前置（M0，评测与拓扑的地基）」与 §9 M0 行。
> 代码基线：master @ 0579564 之上的 M0 增量（见文末清单）。
> 结论先行：**全部探针通过，未出现须回修设计的失败**——M0 出口判据的技术侧满足；
> M1～M3 的投入门仍然是 V0-A 通过（§9），本报告不改变那一门。

## 1. 交付物与验证结果

| M0 前置项（§7 原文） | 落点 | 结果 |
|---|---|---|
| mock 按请求 `model` 分流脚本 | `src/provider/mock.rs:140` `MockProvider::routed`（严格路由：未知 model 诚实报错） | `m0_probes` 2 例通过 |
| e2e SSE mock 认第二个模型名 | `scripts/e2e_scenarios.py`：`MODEL_ROUTES` + 新增 model-route 场景 | E2E ALL PASS（含既有四场景回归） |
| usage 缺失策略钉子（A-L2b） | `resp_text(_, None)` 原生支持 + `al2b_missing_usage_keeps_reserve_totals_unfaked` | 通过 |
| `gate.reserve/settle` 原型 + 并发预占压力 | `src/tools/mod.rs:143` `BudgetGate::reserve` / `ReserveGuard` / `Settled` | A-L2c 矩阵 6 例 + 压力 1 例通过 |
| 并行证明遵循既有纪律 | 进程内不冒充并行；A-L4 的 ThreadingHTTPServer 时间区间重叠证明留 M2 | 纪律声明，见 §4 |

测试总量回归：plain 538 / mock 548 / tui,mock 681 / code,mock 642 全绿；
clippy 四配置（headless / tui / code / tui,code）全净；`cargo fmt --all` 已过；
`python3 scripts/e2e_scenarios.py` 全过。

探针测试体：`src/m0_probes.rs`（lib 内 `#[cfg(test)]` 模块，每次 `cargo test` 都跑，
不依赖 mock feature）。14 例，命名直接映射验收编号（`al2b_*` / `al2c_*` / `al9_*` / `m0_*`）。

## 2. A-L2b/2c 原型：reserve/settle 账本

**原型即真实代码**：`reserve`/`ReserveGuard`/`Settled` 落在生产类型 `BudgetGate`
（`src/tools/mod.rs:143`）上，M3 的 LlmTool 直接沿用；当前无生产调用方（惰性、
零行为变化）。设计要点（对应 §3.1 LD-17 矩阵）：

- **恰好一次由所有权保证**：`settle(self)` / `release(self)` 消耗 guard，
  编译期排除重复退款；`#[must_use]` 防止静默丢弃。
- **Drop 不结算 = 保留全额**：未知用量（usage 缺失/请求错误/panic 中途）宁可过持
  不可欠持。panic 探针用 catch_unwind 验证 unwind 路径账面恰好保留一次。
- **未派发全退**：`release()` = `settle(0)`，完整回池（探针验证回池后可再预占）。
- **防回绕纪律与 `share` 同款**：退款走 `fetch_update` + `saturating_sub`
  （裸 `fetch_sub` 下溢回绕会交出巨额假计数器）；预占与 cap 检查是**一步原子**
  （`fetch_update` 闭包内 `checked_add` + `<= cap`，check-then-add 在并发下可越界）。
- **actual 超 reserve**：净补差额并置 `over_reserve` 标记（超额可观测，不当错误）。
- **拒绝不改账**：预占失败（装不下 cap）返回 `None` 且计数器不动——软预算拒纳
  （halted=false 语义），与 LD-9 一致。
- **并发压力确定性设计**：16 线程 × 预占 100，barrier 保证全部预占尝试完成后才
  结算——不变量与调度无关：恰好 10 个成功（10×100 = cap 1000），各自 settle(40)
  后账面 400。若预占非原子会有第 11 个成功；若结算重复会偏离 400。
- **A-L2b 钉子原型化**：mock 无 usage 响应 → 不以 actual 结算、guard 自然 Drop
  保留估算（500），totals 侧只折 `Some(usage)`（未知不进已知合计）。M3 落地时
  LlmTool 必须返回 `usage_in_shared_budget=true`（v0.7 修正的 fold 点双计问题），
  探针注释已把该流程钉在测试体上。

## 3. A-L9 原型：Provider 拓扑

- **同实例多模型**：`MockProvider::routed` 严格按 `ChatRequest.model` 分流，
  同一实例服务 outer/inner 两套脚本；`Captured.model` 逐请求记录。
  **`ChatRequest.model` 本就是按请求字段（`provider/mod.rs:290` 附近），provider
  层零改动即支持内层换模**——这是"确认 provider 按请求参数方案"的一半。
- **诚实报错**：服务不了的 model 返回明确错误（含 model 名）；失败请求仍被
  capture（归因不因失败丢失）。
- **clone 身份钉子**：`Arc<OnceLock<Arc<dyn Provider>>>` cell——clone 先于填充、
  晚填充对全体 clone 可见（`Arc::ptr_eq`）、二次填充被拒、并发 8 线程填充恰一赢家。
  这直接验证 §9 M0 行的"注册表惰性构建次序"：RunCodeTool 在 assemble 中先于
  Provider 构造（`src/lib.rs:141` `build_agent_with_provider`），M3 用同款
  clone-共享 cell 晚填充即可，与既有 registry OnceLock 缓存同纪律。
- **Provider panic**：panic 传播不破坏 cell（`OnceLock` 无锁中毒概念，仍解析到
  同一实例、再次调用仍诚实失败）；已派发 guard 的 unwind Drop 保留估算、
  未派发预占可全退。
- **串行 Provider**：互斥锁实现的 Provider 经共享 cell 8 线程并发调用——全部
  成功、`max_concurrent == 1`（吞吐下降被接受，A-L9 原文语义）。

## 4. §9 M0 出口判据逐条核对

| 出口判据 | 状态 | 证据 |
|---|---|---|
| A-L2b/2c 原型通过 | ✅ | §2 各例 |
| A-L9 原型通过（mock 分流/同实例/clone/panic/串行） | ✅ | §1、§3 |
| 确认主循环重构边界 | ✅ 零重构 | LlmTool 是 run_code 注册表内的 guest 工具：预算经 `ToolCtx.budget` 的 `BudgetGate` 句柄（现已有 reserve/settle）、cancel 经 `ChatRequest.cancel`（`Arc<AtomicBool>`，ToolCtx 已有线）、截止经 `ChatRequest.per_request_timeout`。Provider 注入经 assemble 期 cell 晚填充（§3）。`agent.rs` 主循环无改动面。 |
| 确认非流式消费 | ✅ 类型层事实 | `Provider::chat` 同步阻塞、返回完整 `ChatResponse`、`on_event` 仅增量——每条探针都以 no-op 回调消费且断言增量通道也健康。真实 provider 的超时/取消在 HTTP 层，进程内 mock 不可证：留给既有 mock server 回归（`examples/mock_*.py`）与 M3 的 A-L1 集成测试。 |
| 确认 provider 按请求参数方案 | ✅ 方案冻结，实现留 M3 | ① 按请求 model：字段已存在，零改动（§3 已证）。② 按请求 temperature：`ChatRequest` 增 `temperature: Option<f32>`（默认 None 不发送→外层默认不变），anthropic/openai 的 body 构建点在 `Some` 时插入字段；LlmTool 内部请求恒 `Some(0.0)`（LD-4）。加性字段，无迁移风险。 |
| 并行机制验证 | ⏳ 按纪律留 M2 | 进程内功能单测证明不了真并发（详录：并发验证需线程化 mock e2e）。A-L4 的 callBatch × llm 8 路时间区间重叠证明在 M2，用 ThreadingHTTPServer（e2e mock 已线程化，model 路由已就位）。 |
| 未通过则先修设计 | 未触发 | 无探针失败；设计文档 v0.7 无需修订。 |

## 5. M0 增量清单（未提交）

- `src/tools/mod.rs`：`BudgetGate::reserve` + `ReserveGuard` + `Settled`（惰性，
  M3 前无生产调用方）。
- `src/provider/mock.rs`：`MockProvider::routed` 严格 model 路由 + 失败也 capture。
- `src/m0_probes.rs`（新增，`lib.rs` 内 `#[cfg(test)]#[path]` 声明）：14 例探针。
- `scripts/e2e_scenarios.py`：`MODEL_ROUTES` 按模型路由 + 第五场景 model-route
  （第二模型名经真实 HTTP/SSE 栈各服务各的脚本、每跑恰 1 请求）。
- 本报告。

## 6. 真实网关冒烟（2026-09-21，`测试模型.md` 网关）

M0 mock 探针证明不了的"真实模型/网关"半边，用 `测试模型.md` 的网关
（OpenAI 兼容，双模型名同 base_url 同 key）做了单轮冒烟：

| 验证 | 命令面 | 结果 |
|---|---|---|
| 同网关双模型（A-L9 真实侧） | CLI 两次单轮，仅 cfg.model 不同 | `deepseek-flash` 1.26s（input 661 / output 2，end_turn）；`glm-5.3-flash` 5.32s（input 523 / output 49，end_turn）。进程内"同实例多模型"半边已由探针证，此处闭合网关侧 |
| usage 真实解析 | CLI `--json` 的 usage 事件 | 四字段经真实 SSE 栈正确落盘；cache 两字段该网关恒 0（原始响应有 `prompt_cache_hit/miss` 但未命中）——LD-6"已知量"口径成立 |
| temperature 按请求（LD-4 真实侧） | 原始 curl，`stream:true` + `temperature:0` | 200 + 正常 SSE + usage——M3 方案（`Option` 字段、`Some` 时发送、外层默认不动）的真实侧假设成立 |

**计价发现（进 V0-A 配方 B 节）**：两家 flash 都有隐藏 reasoning——
`deepseek-flash` 一次"reply ok" completion 15 中 reasoning 13；`glm-5.3-flash`
在 `max_tokens=32` 时 32 中 31 全烧思考、**可见输出为空**、`finish_reason=length`。

1. max_tokens 语义**含** reasoning（completion ≤ max_tokens）——LD-17 的
   reserve = est_in + max_tokens 上界语义在该网关成立。
2. 内层调用 max_tokens 必须为 reasoning 留余量（LD-4 默认 1024 合理）；过小
   预算会得到空输出——labels 路径落入 `kind="llm_output"` 错误封套，设计已覆盖，
   但评测/调参时要把"空输出=预算不足"与"词表违规"区分开。
3. 计价口径（§8.1 原文）：`reasoning_tokens ⊂ completion_tokens`——只按
   completion 计价，勿重复计。

局限：单轮短任务冒烟，非吞吐/并发/失败重试测量——那些属于 V0-A 配方内的
预登记项（B5、D7）。

## 7. 后续门的提醒

- **M1～M3 的投入门 = V0-A 通过**（§9），M0 完成不解除该门。下一步是 V0-A
  预登记配方（场景任务族、留出集、质量/成本门槛、调优预算——数值须由首发
  业务场景确定）。
- M0 的 mock 通过不等于真实模型/网关/自定义 Provider 的成本与吞吐已验证（§9 原文）。
