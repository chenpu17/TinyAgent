# run_code 入门演示

一个可以直接跑给别人看的样例：模型在**进程内 wasm 沙箱**里写一段 JS，
一次性完成「批量读数据 → 精确计算 → 写回结果文件」的闭环。

演示的钩子是**数值保真**：数据文件里埋了三个超出 JavaScript `Number`
安全整数范围（±2^53）的大数——用 float64 直接加会**静默算错**，只有全程
`BigInt` 才能得到精确总和。跑完脚本会用 python3 交叉验证，并把
「float64 会错成什么样」并排打出来。

完整的配置、工具接入、三个扩展与故障处理见
[快速上手与最佳实践](../../docs/run_code_guide.md)。本示例明确要求模型使用
run_code，用来体验执行方法，不代表自然采用率或成本优势。运行会调用你配置的
模型，耗时和费用随模型及轮次变化。

## 前置条件

1. **带 `code` feature 的二进制**（两层开关的第一层）：

   ```sh
   cargo build --release --features code
   ```

   已有产物的话用 `TINYAGENT_BIN` 指过去，脚本会按
   `$TINYAGENT_BIN` → `target/release/tiny-agent` → `target/debug/tiny-agent`
   的顺序找。
   脚本会先检查 `--version --json` 的 features 是否包含 code，再请求模型。

2. **一个 OpenAI 兼容网关 + API key**（第二层开关在 config 里，模板已开）。
3. **Python 3 与 macOS/Linux shell**：用于生成配置和独立核对结果。

## 从仓库根目录运行

```sh
export TINYAGENT_DEMO_BASE_URL='https://your-gateway/v1'   # OpenAI 兼容地址
export TINYAGENT_DEMO_MODEL='your-model'                   # 模型名
export TINYAGENT_API_KEY='sk-...'                          # 只进环境变量，不落盘

./examples/run-code-demo/demo.sh
```

## 你会看到什么

1. 脚本在 `work/run.XXXXXX/` 下生成 `data.txt`（300 行整数，其中 3 个是 2^53 级大数）
   和 `config.json`（`"code": {"enabled": true}`）。每次使用新目录，保留之前的产物；
2. agent 运行：模型通常先 `read_skill` 拉取内置的 `run-code` 技能全文，
   再发一次 `run_code` 调用——沙箱里用 `readAll` 整读数据、`BigInt`
   全程精确求和、`writeVerified` 校验式写回 `stats.json`；
3. 本次目录的 `stats.json` 落盘，脚本用 Python 核对每个字段的类型和精确值，
   并展示 float64 直接求和会得到的**错误**结果作对照；
4. 本次目录的 `result.json` 里有代码、返回值和工具记录，内部调用的审计摘要在
   对应 run_code 记录的 `meta` 中；不会为每项内部调用保存完整原始输入/输出。

成功须同时满足：字段与独立计算一致、至少一次 run_code 成功、任务以 end_turn
结束；否则脚本返回非零状态。失败时也可按开头打印的工作区位置查看已有产物。

## 它验证了什么

- **两层开关**：cargo feature `code`（体积闸）+ `config.code.enabled`
  （运行期开关），默认全关——本演示只开了第二层；
- **能力构造性收敛**：guest JS 没有网络、没有文件系统直访，读写全走
  `tools.call` 编排的既有工具（同一套 hooks 闸门）；
- **数值保真**：`Number` 是 float64，超 2^53 静默失真；移植整数算术一律
  `BigInt` 字面量（`2n ** 64n`）；
- **限额兜底**：fuel / 墙钟 / 内存 / 调用数四重限额，guest 里
  `tools.budget()` 可随时查剩余额度；
- **诚实审计**：`run_code` 的代码与结构化返回值记录在 `result.json`。

## 调整配置

脚本实际读取本目录的 `config.example.json`，用环境变量填入模型地址、模型名和
独立工作目录，其余设置保留。`max_tokens` 模板值为 4096；遇到模型输出触顶时，
可设置 `TINYAGENT_DEMO_MAX_TOKENS` 覆盖为该模型支持的正整数。该变量控制
**外层模型**的单次输出上限，与内层 `llm` 的参数范围不同。增大上限可能增加费用，
应结合 result.json 的结束原因与 provider 限制调整。

## 常见问题

- **第一次跑明显偏慢**：wasmtime 冷编译（debug 构建可达 ~10s），之后有
  磁盘缓存；
- **想看 guest 里的 console 输出**：`--json` 模式下是 `code_log` 事件，
  普通模式走 stderr 的 `[code]` 前缀行；
- **换 Anthropic 协议网关**：把 `config.example.json` 里的
  `"provider"` 改成 `anthropic`，并设置对应的 base URL 和模型名；脚本会沿用
  模板 provider，`code` 配置两协议通用。
