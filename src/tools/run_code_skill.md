---
name: run-code
description: MCP/API 分页、批量操作、大文件处理：规则明确且剩余仍有跨页/大量对象的同构调用或大量中间数据时，先 read_skill({"skill":"run-code"}) 再用代码承接。探查接口后重判，只续做未完成部分。整个剩余任务一次调用/可用 shell 管道能完成则直接做；小集合、逐项语义判断不必用代码，不能仅凭“所有/批量”触发。含同步 JS 契约与校验示例。
allowed-tools: run_code read_file write_file
---

# run_code 沙箱完整用法

## 先确认返回形状，再展开循环
`tools.call` 只做 JSON 解析，**不会自动去掉业务信封**。直接调用显示 `{response: [...]}` 时，代码也必须取 `.response`，不能对整个对象用 `.map` 或当数组。不同端点可能不同；先看实际返回及 schema，不按工具名字猜。

已确认某组端点返回“数组或 response 数组”时，可定义以下**局部函数（不是内置 helper）**；缺字段/错误对象必须报错，不能用 `|| []` 把解析失败变成空页。其他端点按自己的契约取 `.items` 等字段；分页终止另按接口规则判断。
```js
// response-array-example: tests execute this exact block in QuickJS.
function responseRows(r) {
  if (Array.isArray(r)) return r;
  if (r && typeof r === "object" && Array.isArray(r.response)) return r.response;
  throw new Error("expected array or response array; inspect tool result");
}
```
已有直接探查的响应足以确认形状就复用该证据。仍不明确时用短小的只读代码返回类型、字段名、数组长度及必要样例，确认后再展开长流程；无需为了“检查形状”固定多加一次调用。不要打印凭证。

## 何时加载 / 何时换轨
- **同时满足才承接**：筛选、参数构造和校验规则已明确；剩余仍有跨页/大量对象的同构调用，或大量中间数据需要处理。后续主要改变游标、页码或对象 ID，是可写循环的信号。大文件聚合、跨文件 join 也适用。
- **可先探查再判断**：接口或返回形状未知时先直接查；看见规模、分页信息、目标集合后，重新选择剩余流程的执行方式，不因已经直接调用过就继续逐条搬运。无需等到失败或上下文接近耗尽。
- **不用的边界针对整个剩余任务**：一次工具调用或一条可用 shell 管道即可做完，就直接完成；小集合可直接批量调用。逐项还需模型理解不同内容时，先保留语义判断，只把规则稳定的子流程交给代码。用户说“所有/批量”本身不是规模证据。
- **换轨不重做**：迁移已确认的谓词、目标 ID、游标、已完成项和失败项；只处理剩余部分。状态不确定时先重新查询，不能盲目重放修改。每次 run_code 的 JS 变量不跨调用保留，续批状态须显式传入或写文件。
- **先计划再执行**：先在回复中明确实体层级、AND/OR 谓词和边界案例；大清单在变量/文件里构造并在修改前检查集合与计数。规则无歧义且操作已授权时，可在同次代码中检查后执行，随后返回计数、少量样例及已落盘路径。新集合仍需模型判断或用户确认时，先返回计划，判断或确认后再执行。不要为了列清单把所有中间数据搬回对话。

## 编排成本与校验范围
- **产物分块增量写**：要产出大文件（predictions、导出、报表等）时，每完成一段就 `writeVerified` 落盘一次，不要把全部结果攒到最后一次性写——中途截断或额度烧穿时，已落盘的部分就是真实进度。写完产物后任何进一步打磨（复核、排版、扩写）都是亏的，产物完整落盘即收尾。
- 只写当前任务需要的短流程，不照抄整套示例框架。已取得的少量完整数据可直接带入；大量数据通过代码收集，避免在自然语言和代码间反复转写。只因迁移到代码不必重查已确认、仍有效的数据；数据不完整或可能变化时重新查询。
- `console.log` 和 `return` 都进入结果；只报计数、异常和少量样例，不打印完整 KEEP/ACT 集合。续批、恢复或任务要求才保存文件；局部变量可承载单次流程。清理文件必须使用实际可用工具，不能猜 `delete_file`。
- 最终校验须独立读取真实状态，比较目标与保留集合。若完整分页列表已提供所需字段，可一次集合比较验证，不再对同一状态逐项查询；字段不足才补查。代码已完成这项校验且无后续写入/新疑点时，直接使用其验证摘要，不在外层再查一遍。不能只比较数量，不能用操作成功响应代替状态读回。

## 环境
- 纯同步 JS（QuickJS，ES2023）：无 async/await、Promise、setTimeout、import/require、fetch
- 无网络、无系统调用；读写文件只能经工具调用（read_file / write_file / edit_file / list_dir，工作目录内相对路径）
- Date.now 与 Math.random 不可用（评测可复现）
- 资源上限：工具调用次数、内存、执行时长、fuel——任一超限即强制终止；已写盘的文件保留
- **调用额度按次独立**：每次 run_code 调用独立计数（默认每次 200 次），超限只终止本次执行，下次 run_code 重新计满。任务超过单次额度就**分批**：每批 ≤200 次调用，中间结果 `write_file` 落盘，下一批读回来接着干（先摸元数据再批量写的两阶段任务尤其要预算好两阶段各占多少额度）

## 内置助手（前导函数，代码里直接可用；此外没有别的内置助手）
- `readAll(path)` → 整个文件的文本。自动分页拼装，无行号前缀、无元数据头。沙箱内单次读取上限约 200 万字符——常见数据文件（几 MB 文本）一次读完，不用自己分页
- `readLines(path)` → 整个文件的行数组（`split("\n")` 语义：文件以换行结尾时末尾有一个空元素，过滤掉即可）
- `writeVerified(path, content)` → 写入文本并逐字节读回校验（防截断/转写意外），不一致会 throw。重要的文本产出建议走它
- `tools.budget()` → 剩余额度 `{calls_used, max_tool_calls, calls_left, wall_left_ms, wall_bound_by, fuel_left, tokens_left}`，查询本身免费。`tokens_left` 是 run 级 token 预算剩余（未设 `limits.max_total_tokens` 时为 null）。长批量开工前先查、循环里定期查，别等撞限才发现
- 其余能力一律 `tools.call`；**调 MCP 工具前先 `tools.list()` 核对准确名字**——MCP 工具名长、分隔符易混（`_` 与 `__`），写错名字的失败调用也照扣额度，一批全错就是额度白烧。`tools.list()` 返回**对象数组** `[{name, description}]`（不是字符串数组），取名字用 `.map(t => t.name)`
- 工具错误可 try/catch，异常带结构化字段：`e.kind`（`denied`=路径/权限拒绝、`hooks`=钩子拒绝、`budget`=调用额度、`deadline`=墙钟、`cancel`=宿主取消（整个执行终止，勿重试）、`unknown_tool`=写错名字、`tool`=工具自身失败）、`e.tool`（调用名）。`e.retryable===false` 的类别重试无意义（改输入或跳过）；`tool` 类**不带** retryable（无法机判，读 message 自行判断）

<!-- BEGIN LIB SECTION（code.include=true 时保留；=false 时装配端整段剔除，见
     run_code.rs::run_code_skill_md —— 门关时模型可见契约与旧二进制一致） -->
## 库与复用（需配置开启 code.include；未开启时 include/saveLib 不存在、保留名直调被拒）
跨任务会重用的逻辑（判别函数、聚合骨架、格式转换）沉淀为 `lib/` 下的库，不要每次重写。生命周期：**建库 → 用库 → 修库**，状态由 `lib/INDEX.md` 的机器行（`st=draft/ok` + 内容哈希）承载。

- **建库**：写成函数声明风格 + 内嵌纯断言自测块（判别库另附 `verify(n)` 探针），用 `saveLib("lib/xxx.js", src)` 落盘——写文件、宿主算哈希、登记 INDEX 一条龙，**恒 `st=draft`**。裸 `write_file` 写的文件 `include` 会被拒（未登记）； INDEX 行同步维护人类字段（io 签名、适用条件、质量证据指针、owner/来源）。
- **用库**：先 `read_file("lib/INDEX.md")` 对照 `needs` 与当前能力，有现成库就 `include("lib/xxx.js")`（幂等、循环依赖自动截断），只写增量胶水。准入 = `st=ok`（负责人核对内容与证据后人工改）或本会话 `saveLib` 登记过相同内容；旧 draft 被拒时提示负责人处理批准，不要用重新登记绕过审批。
- **修库**：`include` 后重定义同名函数，本次执行即生效（文件不变）；跑通自测后 `saveLib` 写回——新哈希重新登记、回 `st=draft`，**旧批准与质量成绩不沿用**，须重新验证/批准。
- **纪律**：库只放代码（函数声明、顶部不放 `'use strict'`、保持多行便于检查、命名带库前缀），不塞业务数据；版本历史交给 git。宿主 include 整文件直读（不受 readAll 分页限制），单库上限 1 MiB。纯断言不调用工具；判别探针显式运行、小样例内嵌。库链路校验失败（未登记/内容与登记哈希不符/超限/门未开启直调保留名）统一 `e.kind="lib"`，属确定性错误，重试无意义。
- **判别库**（若宿主还启用了 llm 原语，级联判别是最高价值的库形态）：批量前先跑 `verify` 做链路健康检查（全过返回摘要、任一错例 throw，外层收到明确错误不进批量）；实际输出仍按场景质量政策核验，区分「已处理」与「已验证」。程序/依赖/模型/标准变化时，旧跑分只作历史证据。

库源码形态（函数声明 + 自测块；这就是要传给 saveLib 的 src）：
```js
// lib-example: 库源码形态，tests execute this exact block in QuickJS.
function classifyTier(n) {
  if (n <= 80) return "small";
  if (n <= 200) return "medium";
  return "large";
}
(function selfTest() {
  const cases = [[10, "small"], [200, "medium"], [201, "large"]];
  for (const c of cases)
    if (classifyTier(c[0]) !== c[1]) throw new Error("tier selfTest: " + c[0]);
})();
```
使用：
```js
saveLib("lib/tier.js", TIER_SRC);   // TIER_SRC = 上述形态的源码字符串
include("lib/tier.js");             // 幂等；自测块在加载时跑一遍
return classifyTier(150);           // "medium"
```
<!-- END LIB SECTION -->

<!-- BEGIN BATCH SECTION（code.call_batch=true 时保留；=false 时装配端整段剔除，见
     run_code.rs::run_code_skill_md —— 门关时模型可见契约与旧二进制一致） -->
## 批量调用（需配置开启 code.call_batch；未开启时 tools.callBatch 不存在、直调保留名按未知工具报错）

`tools.callBatch([{name, args}, …])` 并发执行一批**只读**工具调用（分页拉取、逐项判别是理想对象）。并发上限 8/波；结果**按提交序**返回数组。

- **无事务语义（三句话）**：无原子性；无回滚；部分执行是正常结果。批量结果必须**逐条检查**结果数组——哪些条目跑了、哪些被拒纳，数组里如实可见。次序保证仅存在于顺序 for 循环（副作用批量）。
- **只读面**：仅 batchable 工具可入批（read_file 等纯只读工具）。混入非只读工具（write_file/shell 等）→ 该条目为 `e.kind="batch"` 错误条目，单条不中止；**副作用调用写顺序 for 循环**。
- **失败条目判别**：单项失败不 throw，该条目为 `Error` 实例带 `e.kind`/`e.tool`——用 `instanceof Error` 判别，细节取 `.kind`/`.message`，**不要 `JSON.stringify` 失败条目**（Error 序列化成 `{}`，细节全丢）。成功条目就是工具结果本体（JSON 自动解析，同 `tools.call`）。**`e.halted` 仅 callBatch 条目携带**——单次 `tools.call` 的错误无此字段（`undefined` 判 falsy 会误判沙箱还活着），单次调用的停机判定用 `e.kind`（`cancel`/`deadline`/`budget` 即停机，沙箱只剩 return 收尾）。
- **额度**：K 条计 K 次调用计数（失败条目也计）；撞调用额度时尾部条目得 `e.kind="budget"`（`e.halted=true`，沙箱已停只能 return 收尾）。token 预算面的 `e.halted=false`（预占拒绝、沙箱活着、可落盘收尾）由 llm 工具引入，见「LLM 判别」段。
- **超时预算**：`timeout_secs ≈ 单调用预期时长 × ceil(批量规模 / 有效并发度) + 计算与限流余量`；有效并发度不大于 8。仅作规划估算，不是最坏时延保证。

```js
// callBatch example: 并发拉多页 + 逐条检查（示意端点）。
const entries = [1, 2, 3, 4].map(p =>
  ({ name: "mcp_list_items", args: { cursor: null, limit: 100, page: p } }));
const results = tools.callBatch(entries);
let ok = 0, bad = [];
for (const r of results) {
  if (r instanceof Error) { bad.push({ kind: r.kind, msg: r.message }); continue; }
  if (!Array.isArray(r.items)) { bad.push({ kind: "shape", msg: "no items" }); continue; }
  ok += r.items.length;
}
return { ok, bad };   // bad 非空就如实上报，不吞
```
<!-- END BATCH SECTION -->

<!-- BEGIN LLM SECTION（code.llm.enabled=true 时保留；=false 时装配端整段剔除，见
     run_code.rs::run_code_skill_md —— 门关时模型可见契约与旧二进制一致） -->
## LLM 判别（需配置开启 code.llm.enabled；未开启时 llm 工具不存在）

`tools.call("llm", {prompt, system?, model?, max_tokens?, labels?})` 单次模型调用（无工具、无历史、无流式，一轮返回）。大量同构的逐项语义判断（分类/抽取/校对/打标）用代码循环 + llm 承接；model 须在 code.llm.models 内（缺省 default_model）。返回 `{text, label?, usage, model}`；调用也计工具额度、token 成本发前预占。失败两类：`kind="llm"`（参数/允许列表/provider 失败）与 `kind="llm_output"`（词表未命中，带真实 usage）——均无 retryable，读 message 自判。

- **判别纪律**：输出词表用 `labels` 声明（含 unsure 显式词），**且 prompt 里同写"输出必须是这些词之一"**（宿主只校验不注入提示）；未声明 labels = 生成模式（任意文本，作结构化抽取须在库内另行校验）；非法输出得 `kind="llm_output"`——分支前先判 label 合法性；unsure 单独计数进聚合摘要；`max_tokens` 用目标模型验证后设小；**首发只产出只读结果**；其他部署的副作用按宿主授权政策执行，外层复判不等于用户授权。
- **成本纪律**：开工前 `tools.budget()` 查 tokens_left（**含未结算的预占**），循环里定期查；429/5xx 保留失败项并交外层诊断，系统性限流须宿主延后处理；`e.kind === "budget"` 时查 **`e.halted`**：true = 沙箱已停，只能 return 收尾；false = 额度关口拒绝但沙箱活着，可 write_file 落盘部分结果再收尾。单次 `tools.call` 的封套无 `e.halted` 字段——按错误文案判断沙箱是否还活着（预占拒绝的文案自带"沙箱仍在运行"语义）。
- **批量形态**：逐项判别的并发形态见「批量调用」段（仅部署开启 code.call_batch 时可用——llm 是 batchable 工具，判别 fan-out 与分页拉取是理想对象）；未开启时写顺序 for 循环。失败条目判别与逐条检查纪律同该段：`instanceof Error`、取 `.kind`/`.message`、**不要 `JSON.stringify` 失败条目**、部分执行是常态。
- **级联模式**：便宜 model 先判，`unsure`/`llm_output` 错误升级贵 model：

```js
// 级联判别 example：便宜模型先判，unsure 升级贵模型（示意模型名）。
function judge(item) {
  try {
    const r = tools.call("llm", {
      prompt: item.text + "\n\n输出必须是这些词之一：yes / no / unsure",
      model: "cheap-model", max_tokens: 8,
      labels: ["yes", "no", "unsure"],
    });
    if (r.label === "unsure") {
      const up = tools.call("llm", {
        prompt: item.text + "\n\n输出必须是这些词之一：yes / no",
        model: "expensive-model", max_tokens: 8,
        labels: ["yes", "no"],
      });
      return up.label;              // 升级后输出未命中 → 走下方 llm_output 分支
    }
    return r.label;
  } catch (e) {
    if (e.kind === "llm_output") return "unresolved";  // 词表未命中：换模型或标记跳过
    throw e;                                           // budget / llm 等交外层诊断
  }
}
```

- **质量交付**：按预先给定的场景政策抽检本批实际输出（不是重跑 verify）；保留原文版本与定位。没有质量政策或未完成复核时标未验证/待复核；抽检通过不等于全量正确。unsure、错误与未完成项保留在分母和交付清单。
- **恢复**：按块提前保存 checkpoint；不要等 quota/cancel/deadline 已阻断工具后才尝试落盘。恢复时只读任务可重算待确认项；副作用另需宿主的确认/幂等政策。
<!-- END LLM SECTION -->

## read_file 的三种模式（tools.call 直调时按需选）
1. **文本模式**（默认）：首行 `[文件名: 总行数 · 总字符 · complete|行区间 a-b]` 元数据，其后每行 `行号→tab` 前缀；带 limit 截断时末尾多一行 `... [line limit reached]`。需要解析内容时优先 readAll / readLines 或 json 模式，**不要手解析这个格式**
2. **format:"json"**：返回 `{content, first_line, lines_total, chars_total, truncated, truncation, next_offset}`——content 无前缀。只有 limit 截断（行对齐）给非空 next_offset，循环传 `{offset: next_offset, limit, format:"json"}` 续读直到 null；chars/bytes 截断时 next_offset 为 null（窗口可能切在行中间，无法线对齐续读），此时改用 raw
3. **raw:true**：按字节窗口返回 base64：`{content_b64, offset, bytes_read, bytes_total, truncated, next_offset}`——offset/limit 是**字节数**，默认窗口约 1.4 MB 且按 3 字节对齐；二进制 / 编码敏感内容的唯一保真通道

## 字节保真复制（跨窗搬运二进制）
**优先 `copy_bytes`**（经 tools.call，host 流式搬运、内容不经过 JS 字符串，二进制/非 UTF-8/大文件均逐字节一致）：
```js
const r = tools.call("copy_bytes", { src: "src.bin", dst: "out/dst.bin" });
return { copied: r.copied_bytes, file: "out/dst.bin" };
```
需要边复制边变换时才用手工配方。默认窗口按 3 字节对齐 ⇒ 各窗 content_b64 直接字符串拼接 == 整个文件的 base64：
```js
let off = 0, parts = [];
while (true) {
  const w = tools.call("read_file", { path: "src.bin", raw: true, offset: off });
  parts.push(w.content_b64);
  if (!w.truncated) break;
  off = w.next_offset;
}
tools.call("write_file", { path: "dst.bin", content_b64: parts.join("") });
return { copied: "dst.bin" };
```
write_file 的 `content`（文本）与 `content_b64`（base64 字节）二选一，不可同传。

## 批量操作纪律（计划—执行—校验闭环）
先明确实体层级和 AND/OR 谓词，再完整收集、去重、分出 KEEP / ACT ON，最后执行和独立读回。不能用“目标数减失败数”冒充成功计数；工具未抛异常也不代表业务成功。

下面是**虚构接口的示意配方，不是内置工具**。先用 `tools.list()` 核对真实名称，再根据实际 schema/探查结果改参数、响应解析和分页终止规则。示例任务为“删除已归档且未点赞的条目”，即 `archived === true && liked === false`；假定无并发修改、读取即时一致，单批能装进额度和内存。三个示意接口约定如下：
- `mcp_list_items({cursor, limit})` → `{items: [{id: 非空字符串, archived: 布尔, liked: 布尔}], next_cursor: 字符串或null}`。只有显式 null 表示完整结束，空页不代替终止标志；跨页可重复，但同一 ID 内容须一致。
- `mcp_delete({id})` → `{ok: 布尔}`。false 表示确定未删除；异常或其他响应表示结果不确定，必须先查状态，不能盲目重试。
- `mcp_get_item({id})` → `{item: 条目或null}`。null 表示确实不存在，不能把缺字段或权限错误当作不存在。

实际任务先外化谓词与计划摘要；若还需审核新收集的集合，先执行收集部分并返回计数、样例和计划路径，下一次再恢复计划执行。下面演示已明确规则且获授权的一批；计划/日志路径须按任务隔离，不能覆盖别的批次。
```js
// batch-audit-example: tests execute this exact block in QuickJS.
const names = tools.list().map(t => t.name);
for (const n of ["mcp_list_items", "mcp_delete", "mcp_get_item"])
  if (!names.includes(n)) throw new Error("adapt example tool name: " + n);
function validItem(it) {
  return it && typeof it.id === "string" && it.id.length > 0 &&
    typeof it.archived === "boolean" && typeof it.liked === "boolean";
}
function sameItem(a, b) {
  return validItem(b) && a.id === b.id &&
    a.archived === b.archived && a.liked === b.liked;
}
const before = new Map(), cursors = new Set();
let cursor = null;
while (true) {
  const r = tools.call("mcp_list_items", {cursor, limit: 100});
  if (!r || !Array.isArray(r.items) ||
      !(r.next_cursor === null ||
        (typeof r.next_cursor === "string" && r.next_cursor.length > 0)))
    throw new Error("invalid page response");
  for (const it of r.items) {
    if (!validItem(it)) throw new Error("invalid item");
    if (before.has(it.id) && !sameItem(before.get(it.id), it))
      throw new Error("inconsistent duplicate");
    before.set(it.id, it);
  }
  if (r.next_cursor === null) break;
  if (cursors.has(r.next_cursor)) throw new Error("pagination did not advance");
  cursors.add(r.next_cursor);
  cursor = r.next_cursor;
}
const keep = [], targets = [];
for (const it of before.values())
  (it.archived && !it.liked ? targets : keep).push(it.id);
// 先落盘计划；按实际剩余额度预算每次修改/日志与所有初始条目的读回。
writeVerified("batch-plan.json", JSON.stringify({keep, targets}));
if (tools.budget().calls_left < targets.length * 2 + before.size)
  throw new Error("plan saved: batch-plan.json; split before mutation");
const succeeded = [], failed = [];
for (const id of targets) {
  const r = tools.call("mcp_delete", {id});
  if (!r || typeof r.ok !== "boolean") throw new Error("uncertain action result: " + id);
  (r.ok ? succeeded : failed).push(id);
  tools.call("write_file", {path: "batch-progress.json",
    content: JSON.stringify({succeeded, failed})});
}
const accounted = new Set([...succeeded, ...failed]);
if (succeeded.length + failed.length !== targets.length ||
    accounted.size !== targets.length || targets.some(id => !accounted.has(id)))
  throw new Error("action accounting mismatch");
// 读回所有初始条目：成功删除必须消失，KEEP 和确定失败项必须原样保留。
const removed = new Set(succeeded);
for (const [id, original] of before) {
  const r = tools.call("mcp_get_item", {id});
  if (!r || !(r.item === null || validItem(r.item)))
    throw new Error("invalid verification response: " + id);
  if (removed.has(id) ? r.item !== null : !sameItem(original, r.item))
    throw new Error("final state mismatch: " + id);
}
return {complete: failed.length === 0, verified: true, targets: targets.length,
  kept: keep.length, succeeded: succeeded.length, failed: failed.length,
  failed_sample: failed.slice(0, 5), plan: "batch-plan.json"};
```
`verified:true` 只说明读回与已记账结果一致；`complete:false` 仍有未完成项，必须继续处理或诚实报告。执行异常/中断时，计划和最后一次成功落盘的进度只作恢复线索；最后一次调用可能已生效但没记账，先查状态再续做。真实 API 若存在最终一致性、非幂等动作或并发写入，应采用该 API 支持的版本/快照/幂等与校验机制，不能照搬示例假设。大批次要为分页、执行、日志和校验一并预算并分批保存状态，不得边删除边按会移位的页码收集目标。

## 返回值纪律
代码作为函数体执行，`return` 的值经 JSON 序列化作为工具结果。必须返回**紧凑摘要**（统计数字、文件路径、少量样例行）；大数据写文件后返回路径引用，禁止把数据本体放进返回值——返回值与 console 日志共享尺寸上限，超限时截日志保返回值，数据本体照样会被截。

## 典型模式
### Python 等语言整数算术移植（必读）
JS 的 Number 是 float64：整数乘法/累加超过 2^53（约 9×10^15）即静默丢低位——不报错、结果看着像对的。从 Python 翻译计数器/伪随机/哈希/大数运算时，状态量一律 BigInt：字面量加 n、常量同步改（1103515245n、2n**31n）、与 Number 混算前显式转换（BigInt(i) / Number(small)）；循环下标等小数值仍用 Number。拿不准就 String(x) 抽样看量级。
```js
let state = 123456789n;
for (let i = 0; i < 100000; i++) {
  state = (state * 1103515245n + 12345n) % 2n ** 31n;
}
return { last: state.toString() };
```
### 聚合已完整取得的工具数据
以下仅演示聚合：假定查询结果已确认完整且不超过 LIMIT；若响应仍有分页/截断信号，先按真实接口契约拉全，不能把 LIMIT 当成总量。
```js
const page = tools.call("mcp_sql_query", { sql: "SELECT name, amount FROM orders LIMIT 5000" });
const total = {};
for (const row of page.rows) total[row.name] = (total[row.name] ?? 0) + row.amount;
const lines = ["name,total", ...Object.entries(total).map(([n, v]) => `${n},${v}`)];
tools.call("write_file", { path: "out/totals.csv", content: lines.join("\n") });
return { file: "out/totals.csv", customers: Object.keys(total).length };
```
### 大文件统计（readAll 整读后聚合，只 return 摘要）
```js
const counts = {};
for (const line of readAll("data/logs.txt").split("\n")) {
  const m = line.match(/ERROR (\w+)/);
  if (m) counts[m[1]] = (counts[m[1]] ?? 0) + 1;
}
return { error_kinds: counts };
```
### 批量文件操作（list_dir 是纯文本行，目录带尾 `/`；逐项 try/catch 跳过失败）
```js
const names = tools.call("list_dir", { path: "inputs" }).split("\n")
  .filter(n => n && !n.endsWith("/") && n.endsWith(".json"));
const bad = [];
for (const n of names) {
  try {
    const v = JSON.parse(readAll("inputs/" + n));
    if (!v.id) throw new Error("missing id");
  } catch (e) { bad.push({ file: n, why: String(e) }); }
}
return { checked: names.length, bad };
```
