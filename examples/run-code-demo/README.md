# run_code 一分钟演示

一个可以直接跑给别人看的样例：模型在**进程内 wasm 沙箱**里写一段 JS，
一次性完成「批量读数据 → 精确计算 → 写回结果文件」的闭环。

演示的钩子是**数值保真**：数据文件里埋了三个超出 JavaScript `Number`
安全整数范围（±2^53）的大数——用 float64 直接加会**静默算错**，只有全程
`BigInt` 才能得到精确总和。跑完脚本会用 python3 交叉验证，并把
「float64 会错成什么样」并排打出来。

## 前置条件

1. **带 `code` feature 的二进制**（两层开关的第一层）：

   ```sh
   cargo build --release --features code
   ```

   已有产物的话用 `TINYAGENT_BIN` 指过去，脚本会按
   `$TINYAGENT_BIN` → `target/release/tiny-agent` → `target/debug/tiny-agent`
   的顺序找。

2. **一个 OpenAI 兼容网关 + API key**（第二层开关在 config 里，模板已开）。

## 一把梭

```sh
export TINYAGENT_DEMO_BASE_URL='https://your-gateway/v1'   # OpenAI 兼容地址
export TINYAGENT_DEMO_MODEL='your-model'                   # 模型名
export TINYAGENT_API_KEY='sk-...'                          # 只进环境变量，不落盘

./demo.sh
```

## 你会看到什么

1. 脚本在 `work/` 下生成 `data.txt`（300 行整数，其中 3 个是 2^53 级大数）
   和 `config.json`（`"code": {"enabled": true}`）；
2. agent 运行：模型通常先 `read_skill` 拉取内置的 `run-code` 技能全文，
   再发一次 `run_code` 调用——沙箱里用 `readAll` 整读数据、`BigInt`
   全程精确求和、`writeVerified` 校验式写回 `stats.json`；
3. `work/stats.json` 落盘，脚本用 python3（若可用）核对每个字段，并展示
   float64 直接求和会得到的**错误**结果作对照；
4. `work/result.json` 里有完整审计面：每次工具调用的原始输入/输出。

## 它验证了什么

- **两层开关**：cargo feature `code`（体积闸）+ `config.code.enabled`
  （运行期开关），默认全关——本演示只开了第二层；
- **能力构造性收敛**：guest JS 没有网络、没有文件系统直访，读写全走
  `tools.call` 编排的既有工具（同一套 hooks 闸门）；
- **数值保真**：`Number` 是 float64，超 2^53 静默失真；移植整数算术一律
  `BigInt` 字面量（`2n ** 64n`）；
- **限额兜底**：fuel / 墙钟 / 内存 / 调用数四重限额，guest 里
  `tools.budget()` 可随时查剩余额度；
- **诚实审计**：`run_code` 的代码与结构化返回值全量进 `result.json`。

## 常见问题

- **第一次跑明显偏慢**：wasmtime 冷编译（debug 构建可达 ~10s），之后有
  磁盘缓存；
- **想看 guest 里的 console 输出**：`--json` 模式下是 `code_log` 事件，
  普通模式走 stderr 的 `[code]` 前缀行；
- **换 Anthropic 协议网关**：把 `config.example.json` 里的
  `"provider"` 改成 `anthropic` 即可，`code` 配置两协议通用。
