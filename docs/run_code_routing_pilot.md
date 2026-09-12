# run_code 触发修复：两题实测

日期：2026-09-11。结论：**两题均自主读技能并有效使用 run_code，官方状态判卷均通过；效率收益有分化，不能只以触发成功判定方案胜出。**

## 配置与证据

- 模型：`glm-5.3-flash`；AppWorld 数据版本 0.2.0。
- 新契约：`run-code-routing-v1`，使用 `agentic-code.md`，`skill_injection=on_demand`、`precheck=false`。
- 两个用例：`awlikesong`、`awclean`，每题一次；独立实验臂 `ta-code-routing-v1-aw`。
- 使用既有 benchmark 执行器、官方状态判卷、任务提示词和工具集合；没有改 benchmark 源文件，也没有注入运行时触发提示。
- 对照来自旧 `ta-code-aw` 同题的三次历史运行，以下采用三次的中位数。旧版两题各 3/3 PASS，均未使用 run_code。历史对照与本次非同期，且本次两题并行运行，墙钟包含模型服务响应波动，不代表严格配对的因果效果。
- 总 token = 代理记录的 prompt_tokens + completion_tokens，**包含缓存输入**；非缓存输入和输出另列，未换算成费用。

本次 release 二进制 SHA256：

```
e64c07714e29457c34521f9c07bce4284295cee39e2f9569a9a9cdadcfb25148
```

完整条件与原始轨迹位于相邻仓库 `TinyAgentBench/results/runs/ta-code-routing-v1-aw/`：`pilot.json`、各题 `r1/run.json`、`r1/wd/result.json`、代理账单和 stdout/stderr。本仓库保存脱敏统计 `docs/run_code_routing_pilot_results.json`，不复制任务明文、API 凭证或业务条目。

## 结果

| 指标 | awlikesong 旧→新 | awclean 旧→新 |
|---|---:|---:|
| 本次官方判卷 | PASS，5/5 断言 | PASS，6/6 断言 |
| 自主 read_skill | 0 → 1 | 0 → 1 |
| run_code 调用 | 0 → 8 | 0 → 2 |
| 模型轮次 | 16 → 17 | 11 → 10 |
| 总 token | 690,720 → 528,996（−23.4%） | 332,912 → 334,140（+0.4%） |
| 非缓存输入 token | 46,652 → 35,121（−24.7%） | 16,927 → 40,070（+136.7%） |
| 输出 token | 18,036 → 7,347（−59.3%） | 7,226 → 11,574（+60.2%） |
| 客户端墙钟 | 358.3s → 222.7s（−37.8%） | 145.4s → 331.1s（+127.7%） |
| 外层工具调用 | 118 → 22 | 36 → 16 |
| 实际 MCP 调用（含沙箱内部） | 115 → 284 | 34 → 41 |

两题 `cheat_flags` 均为空。结果里的 read_skill 返回内容与该次归档的 v1 技能源码逐字一致，确认没有被磁盘旧技能覆盖；实际 `result.json.code` 确认 on-demand，而非 eager。新代码的最终状态检查覆盖了目标动作；原始记录也保留了错误恢复过程。

外层工具调用变少并不等于业务工作变少：awlikesong 的沙箱内部调用为 296 次，awclean 为 30 次（包含文件工具）；上表的 MCP 列只计实际 MCP API，不把 run_code 外层调用也算成业务 API。

## 轨迹观察

**awlikesong**：第 8 个外层工具调用读取技能，第 10 个开始 run_code；代码用于艺术家/歌曲取数、筛选、批量点赞和校验，属于有效承接。86 项点赞成功，最终通过官方判卷。

但 8 次 run_code 不应解读为 8 次有效批量：其中两次内部工具调用数为零，另有清理文件相关的调用。模型在沙箱内尝试不存在的 `delete_file`，捕获错误后继续恢复。对同一批歌曲的修改前状态读取与修改后逐项读取，以及再次整库验证，使实际 MCP 调用明显增加；最终检查正确，但存在缩减重复验证与临时文件维护的空间。

**awclean**：第 5 个外层调用读取技能，第 11 个开始 run_code。首个程序在首次只读取数后报错：

```
Error: non-array page from mcp_appworld_spotify_show_song_library page 0
```

原因是将完整工具返回值直接当成数组，未取实际 `{response: [...]}` 的 `response` 层。随后程序改成从 `res.response` 获取数组，完成 13 项歌曲库移除和 6 项专辑库移除，外层重查最终状态，官方判卷通过。首个失败程序只发生一次读取，没有业务修改。

成本上，awclean 的 10 次模型请求中有约 89.8s、44.3s、64.3s 的长响应；输出及 reasoning token 增多。不能把全部新增墙钟归于一次解包错误或沙箱执行本身，模型规划/代码生成和服务响应同样贡献了时间。

## 判断与下一步

本次观察支持“新的决策入口能让这两题走到代码路线”，但不能区分 teaser、描述、变体提示词和新版示例各自贡献。也不能证明 MiniCPM5-2B 会有相同效果。

后续优先验证两件事：先根据真实返回对象确认取数字段，再展开长程序；在保持官方状态正确和必要独立验证的前提下，减少重复逐项查询与无必要的临时文件清理。当前 awclean 的非缓存输入明显增加，不宜仅凭总 token 接近旧版就说成本持平。

建议保持这两个失败模式作为下一版的定向回归，再按落地报告四臂矩阵、多次重复验证。**本次没有为了修正观测结果而中途改文案或重跑模型。**

## 复现入口

本节命令使用当前源码与当前契约版本；脚本按当前 manifest 版本创建相应新臂，不会复现 v1。历史 v1 条件见该臂 pilot.json；可用 `--tasks awclean` 只跑该题。

```sh
cargo build --release --features code
python3 scripts/run_code_routing_pilot.py
python3 scripts/run_code_routing_pilot.py --run
```

脚本默认只显示配置，`--run` 才执行这两题。它在进程内注册独立实验臂，并使用既有 benchmark 的执行/判卷流程，不修改 benchmark 源文件。已完成且指纹一致的 run 会复用；实验条件变化会拒绝沿用本目录，须为新批次另起臂名，不覆盖本次证据。
