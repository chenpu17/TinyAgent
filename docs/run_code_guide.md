# run_code 快速上手与最佳实践

`run_code` 让模型在一次工具调用里编写并执行同步 JavaScript，用代码读取、筛选、
关联和汇总已有工具的数据。你提供任务、模型连接和工具，模型负责组织代码；
中间数据在沙箱里处理，最终返回摘要或结果文件。

本文适用于 v0.4.2 的接口。先跑下面的入门演示，再按需开启扩展。
[完整配置参考](../README.md#run_code沙箱-js-执行可选-code-feature)和
[内置技能全文](../src/tools/run_code_skill.md)可随时查阅。

## 1. 跑通第一个任务

以下命令从仓库根目录执行，使用 macOS/Linux 的 shell，并需要 Python 3。
演示会调用你配置的模型，耗时和费用由模型及执行轮次决定。

先准备带 `code` 的二进制。已有产物时，把 `TINYAGENT_BIN` 设为它的绝对路径；
从源码构建可以使用：

```sh
cargo build --release --features code
export TINYAGENT_BIN="$PWD/target/release/tiny-agent"
"$TINYAGENT_BIN" --version --json
```

输出的 `features` 数组应包含 `code`。仅有运行时配置不能给未编入该特性的
二进制增加沙箱能力。Python/Node 绑定也有独立的编译开关，见
[绑定构建说明](../README.md#python--typescript-绑定)。

填入自己的模型连接后执行：

```sh
export TINYAGENT_DEMO_BASE_URL='https://your-gateway/v1'
export TINYAGENT_DEMO_MODEL='your-model'
export TINYAGENT_API_KEY='your-api-key'
./examples/run-code-demo/demo.sh
```

脚本会生成 300 个整数，其中包含超过 JavaScript 安全整数范围的大数，然后让
模型使用 `run_code` 精确统计并写入 `stats.json`。每次运行使用独立目录，脚本
会打印位置，并用 Python 核对字段、类型和结果，确认实际发生了成功的 run_code
调用；校验不通过时返回非零状态。

- `stats.json`：可以交付的业务结果。
- `result.json`：本次任务的结束原因、用量、代码和工具记录。
- `data.txt`、`config.json`：本次运行的数据和配置，API key 只从环境变量读取。

这个示例明确要求使用代码，展示的是执行方法；它不用于证明模型自然选路或
节省成本。模板、输出上限调整及协议切换见[演示说明](../examples/run-code-demo/README.md)。

## 2. 在自己的 Agent 中开启

将下面的 `code` 小节合并到现有配置，保留你的模型、工作目录和工具配置：

```json
{
  "code": {
    "enabled": true,
    "skill_injection": "on_demand",
    "include": false,
    "call_batch": false,
    "llm": {"enabled": false}
  }
}
```

`run_code` 的成员资格由 `code.enabled` 控制，无需把它加进 `tools` 列表。
普通工具列表仍决定它能调用什么：首次使用文件演示需要 `read_file`、`write_file`；
接入业务系统则保留相应的 `mcp_servers` 配置。

模型会先看到工具短描述与技能索引，需要完整用法时调用
`read_skill({"skill":"run-code"})`。三个扩展的教学只在对应开关开启时出现。
通常保留默认 `on_demand` 即可；[agentic-code.md](../examples/prompts/agentic-code.md)
是可选的系统提示词范例，不会自动加载，也不是触发代码的前提。

**在创建 Agent 前配置好能力。** 技能披露与工具注册表包含构造期快照；需要改变
扩展组合或内层模型配置时，重建 Agent 最直接。`set_code_config` 是整包替换，
省略字段会回到默认值，不是局部 patch。

## 3. 从演示换成真实任务

适合先尝试：数据通过分页 API 提供、需关联多个来源、规则可以明确表达的任务。
例如向模型交代：

> 读取客户订阅 API 和本周使用事件 API，按 customer_id 关联，找出有有效订阅但
> 没有使用记录的客户。完整读取所有分页，按事件 ID 去重；把结果写入
> inactive-customers.json。报告读取页数、客户数、异常和未完成项，并核对输出
> ID 集合。若来源数据不完整，请明确说明，暂不判断这些客户没有使用记录。

将这里的名称、字段与输出格式换成实际业务约定。模型可以先直接探查工具，确认
返回形状后再用代码处理剩余部分。业务方应提供时区、日期范围、重复记录和缺失值
的处理规则；这些规则不确定时，先澄清再扩大处理规模。

- **已有 MCP 服务**：沿用[接入方法](../README.md#mcp-客户端)。guest 中用
  `tools.list()` 核对准确名称，按实际 schema 传参；它返回 `[{name, description}]`。
- **Rust 宿主自定义工具**：`CallbackTool` 默认不对 guest 开放，须逐工具调用
  `with_code_access()`；自定义 `Tool` 实现用 `available_to_code()` 声明。
  通过 `build_agent_with` 装配，示例与接口见[库 API](../README.md#库-api程序化调用)。
- **工具和文件权限**：只开放任务需要的能力，配置工作目录、`deny_paths` 和
  `fs_write_dirs`。guest 没有直接网络或文件访问；授予的工具仍按它们自身的权限
  执行。尤其是 `shell`，不能把 guest 文件路径规则当作 shell 的隔离措施。

一次工具调用或已有 shell 管道能完成的任务，直接执行通常更简单。大量复杂
语义判断需要另行评估质量与调用成本，不能仅凭“所有”“批量”要求模型使用代码。

## 4. 按需求开启三个扩展

下面的 JSON 都是**合并到已有配置的片段**；JS 是 `run_code` 的 `code` 参数内容，
可供模型参考或在宿主测试中执行，不能直接当 Node.js 程序运行。

### 批量读取：callBatch

```json
{"code": {"enabled": true, "call_batch": true}}
```

假设工作目录已有 `a.txt`、`b.txt`：

```js
// guide-batch-example
const paths = ["a.txt", "b.txt"];
const results = tools.callBatch(paths.map(path => ({
  name: "read_file", args: {path, format: "json"}
})));
const rows = results.map((r, i) => {
  if (r instanceof Error) {
    return {path: paths[i], ok: false, kind: r.kind, error: r.message};
  }
  return {path: paths[i], ok: !r.truncated, chars: r.content.length};
});
return {complete: rows.every(r => r.ok), rows};
```

结果按提交序返回，失败项必须逐项检查，不能直接 `JSON.stringify(Error)`，否则
错误细节会丢失。批量不是事务，部分执行是正常结果；写操作使用顺序循环。

**批量不等于实际并发。** 并行工具最多 8 个一波；当前内置 `llm` 支持并行，
`read_file` 与 MCP 工具仍顺序执行，宿主 provider 自身也可能串行。分页若依赖
上一页返回的 cursor，就用顺序循环，不能凭猜测把后续页组成并行请求。

### 处理中的语义判断：llm

```json
{
  "code": {
    "enabled": true,
    "llm": {
      "enabled": true,
      "default_model": "your-judge-model",
      "models": ["your-judge-model"]
    }
  }
}
```

模型名须替换成同一个 provider 能访问的真实模型；内层调用沿用外层 provider。

```js
// guide-llm-example
const r = tools.call("llm", {
  prompt: "判断这条反馈是否描述产品故障：点击保存后页面报错，内容未保存。\n"
    + "输出必须是这些词之一：yes / no / unsure。证据不足时输出 unsure。",
  labels: ["yes", "no", "unsure"]
});
return {label: r.label, needs_review: r.label === "unsure"};
```

`labels` 校验输出是否属于词表，不保证业务判断正确。词表要求也要写进 prompt。
没有声明 labels 时返回任意文本，结构化结果须自行校验。示例省略 `max_tokens`，
使用内层默认值 1024；按目标模型调整（内层允许 1..8192），推理模型即使只输出
一个标签也可能需要较多 token。输出触顶或异常终止会返回错误，不能当成功结果。

需要并行判断时再同时开启 `call_batch`；级联与失败处理参考
[技能的 LLM 判别段](../src/tools/run_code_skill.md#llm-判别需配置开启-codellmenabled未开启时-llm-工具不存在)。
这类调用会产生额外模型费用，是否采用应看任务质量与总成本。

### 复用已有处理逻辑：include / saveLib

```json
{"code": {"enabled": true, "include": true}}
```

宿主有明确复用需求时，可用下面的方式保存和加载一个小库。若配置了可写目录
限制，须把 `lib/` 纳入可写集：

```js
// guide-lib-example
const source = `
function reportCountLabel(n) { return "共 " + n + " 项"; }
(function selfTest() {
  if (reportCountLabel(3) !== "共 3 项") throw new Error("count label");
})();
`;
saveLib("lib/report.js", source);
include("lib/report.js");
return {label: reportCountLabel(3)};
```

新建或更新恒为 `draft`，同会话登记过的相同内容可直接加载；新会话须经负责人
核对文件和证据后批准相应 INDEX 行为 `st=ok`。修改后重新验证和批准，不沿用
旧成绩。`--resume` 不恢复同会话登记资格。

哈希核对的是内容一致性；需要可信审批的宿主还须保护批准记录与写入通道。
一次性任务可以直接写短代码，不必建立库。完整生命周期见
[库与复用说明](../README.md#run_code沙箱-js-执行可选-code-feature)。

## 5. 可靠完成任务的六条实践

1. **先确认数据形状。** `tools.call` 自动解析 JSON，但不自动提取业务字段。
   `{response: [...]}` 要取 `.response`，不能用 `|| []` 把错误结果当空页。
2. **确认全量，再判断缺失。** 分页遵循真实的终止标志，检查重复游标、重复 ID
   与字段；被截断的结果不能冒充完整列表。
3. **预算覆盖读取、处理和校验。** 开工前与循环中查看 `tools.budget()`。
   每次 run_code 有独立调用额度，LLM token 预算则与整个任务共享；新开一次
   run_code 不会重置任务 token 预算。
4. **分块落盘，再完成必要校验。** 保存已完成 ID、失败项、游标和输入版本。
   `writeVerified` 的读回证明写入内容一致，业务正确性还要按任务要求检查。
   完成必要校验后收尾，避免无目标的重复优化。
5. **恢复先查真实状态。** JS 变量不跨 run_code 调用保存。断点之后只处理未完成
   项；写操作可能已生效但未记账，先查状态再重试，按 API 的幂等与一致性约定处理。
6. **返回小而明确的交付摘要。** 返回计数、未完成项、少量样例与文件路径。
   处理完成和质量验证通过分开表达；标签有效、工具成功响应或数量相等都不能
   单独证明业务正确。大整数运算使用 BigInt，对外 JSON 中转成十进制字符串。

## 6. 常见问题与恢复

| 现象 | 怎么处理 |
|---|---|
| 找不到 run_code / 绑定提示重新构建 | 核对安装产物是否编入 `code`，再检查 `code.enabled`。 |
| 看不到扩展用法 | 创建 Agent 前打开相应开关；核对 `skill_injection` 和磁盘同名 `run-code` 技能是否覆盖内置版本。 |
| async、fetch、import 等不可用 | 使用同步 JS 与 `tools.call`；沙箱不是 Node.js，不提供这些环境能力。 |
| 单次调用 `kind="budget"` | 单次错误没有 `halted`。读文案区分调用次数耗尽（停止工具调用）与 LLM token 预占未派发且明确说明 `the sandbox stays alive`（其他限额允许时可落盘收尾）。不明确时返回已知进度。 |
| 批量出现预算错误 | 检查所有错误条目；任一 `halted===true` 就停止后续工具调用。某一项 false 不代表整个沙箱仍可写。 |
| cancel / deadline / fuel / 内存终止 | 不假设终止后还能保存进度；依靠之前落盘的 checkpoint，由外层报告未完成项。 |
| llm_output / unsure / llm 截断错误 | 保留该项与真实消耗，按质量政策复核、换模型或调整输出额度；不要当作成功标签。 |
| include 拒绝 draft 或哈希不符 | 核对实际库文件、INDEX 和会话；由负责人重新验证批准，不靠重新登记旧 draft 绕过流程。 |

`tools.budget().tokens_left` 是 token 计数与预占口径，不是货币余额或账单硬封顶。
按实际 provider 用量核账，未返回 usage 的调用不能按零成本理解。

## 7. 下一步查哪里

- [演示与运行参数](../examples/run-code-demo/README.md)
- [配置、权限与 SDK 参考](../README.md#run_code沙箱-js-执行可选-code-feature)
- [模型实际读取的完整技能与代码配方](../src/tools/run_code_skill.md)
- [特性状态与已有价值证据](code_review_brief.md)

机械分页聚合的实验结论有明确场景边界；不要将其费用比例外推到任意任务，或
替 llm、callBatch、include 三项扩展背书。按自己的数据、完成质量和总成本选择。
