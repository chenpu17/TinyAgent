---
name: run-code
description: run_code 沙箱完整用法——read_file 三种模式、copy_bytes 字节保真复制、大文件整读/分页聚合、批量校验闭环、返回值纪律与典型示例
allowed-tools: run_code read_file write_file
---

# run_code 沙箱完整用法

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
- 工具错误可 try/catch，异常带结构化字段：`e.kind`（`denied`=路径/权限拒绝、`hooks`=钩子拒绝、`budget`=调用额度、`deadline`=墙钟、`unknown_tool`=写错名字、`tool`=工具自身失败）、`e.tool`（调用名）。`e.retryable===false` 的类别重试无意义（改输入或跳过）；`tool` 类**不带** retryable（无法机判，读 message 自行判断）

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
多删、漏删、漏页几乎都源于"没数数就动手"。批量任务（批量删除/修改/点赞/导出…）固定走这个模板，**数量不一致就显式 throw**，不要返回"处理完成"：
```js
// 1) 先收集并去重目标，记录目标数
const seen = new Set(), targets = [];
for (let page = 1; ; page++) {              // 2) 分页拉全：显式传 limit，拉到空页为止
  const r = tools.call("mcp_list_items", { page: page, limit: 100 });
  if (!r.items || r.items.length === 0) break;
  for (const it of r.items)
    if (!seen.has(it.id)) { seen.add(it.id); targets.push(it); }
}
// 3) 执行，逐条记账（成功/失败分开）
const failed = [];
for (const t of targets) {
  try { tools.call("mcp_delete", { id: t.id }); }
  catch (e) { if (e.kind === "denied" || e.kind === "unknown_tool") throw e; failed.push({ id: t.id, why: String(e.message) }); }
}
// 4) 校验闭环：目标数 == 成功数 + 失败数，缺一不可
const okCount = targets.length - failed.length;
if (okCount + failed.length !== targets.length) throw new Error("audit mismatch");
// 5) 返回摘要（目标/成功/失败），失败清单给样例
return { targets: targets.length, ok: okCount, failed: failed.length,
         failed_sample: failed.slice(0, 5).map(f => f.id) };
```
要点：目标集合先**去重**（跨源/跨页会重复）；分页**显式传 limit**（默认页大小小得离谱——曾实测默认 5 漏掉第 6 条）；每批开工前 `tools.budget()` 对一下额度装不装得下，装不下就分批落盘续跑。

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
### 聚合工具返回的分页数据
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

## 何时用 / 何时不用
- **用**：数据装不进对话（大文件 readAll 后聚合/过滤/统计，只 return 摘要——不要自己逐页读进对话再心算）；几十个文件的批量校验/改写/重组；跨文件关联（join）、按规则分组、JSON 嵌套处理（代码经 JSON 直传，无 shell 转义烦恼）
- **不用**：单个工具调用就能完成的事（读/写一个文件、跑一条命令）；十来个小文件的简单同构修改（直接多次工具调用更省更稳）；一条 shell 管道能做的（排序/去重/求和/单遍过滤——shell 可用时优先 shell）
- 判断口诀：如果需要在自己回复里记住大量中间数据才能完成，就该用代码
- 可能失败的外部调用用 try/catch 包住，失败时 return 已完成部分的进度摘要
