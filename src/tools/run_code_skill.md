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
