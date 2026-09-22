# code 特性评测装配（E0–E3 矩阵 + 四路线对照）

| 项 | 值 |
|---|---|
| 状态 | M4 装配稿（code_llm_design §8/§8.1/§10.1 的可复现配方）；V0-B 实测未跑 |
| 属主设计 | `docs/code_llm_design.md` v0.7.x（§8 分期矩阵、§8.1 四路线、附录 A③ 边界句） |
| 方法论先例 | `docs/run_code_routing_report.md`（四臂矩阵、测量协议与 req1_unc 勘误）、`docs/run_code_routing_v2.md` |
| 变更口径 | 臂配置、装置文件或指纹变更走本文修订段；边界句文字变更走 code_llm_design 修订记录（A③ 是冻结规格） |

## 1. 编译与二进制

```sh
cargo build --release --features code
```

所有臂共用同一二进制；能力差异全部走运行期 gate（`code.*`），对照臂可复现性
由门关字节一致测试钉住（见 §6）。归档时记录 commit、manifest
（`docs/run_code_routing_manifest.json`）与实际生效配置。

## 2. 分期矩阵（E0–E3）

每期相对上一期只动一个变量；每期 ≥3 重复、交错运行、任务状态重置；任务形状
按 §8 预登记（外层做得到的 / 机械批量适合的 / 语义批量适合的 / 超窗语料可分解的 /
超窗但跨块耦合的——预期不换轨）。系统提示词沿用 `examples/prompts/agentic-code.md`
（正文取第一个 `\n---\n` 之后；评测方答案格式尾巴照旧追加并另做哈希）。

| 臂 | code 配置 | skill | 隔离的变量 |
|---|---|---|---|
| E0 | `{"code":{"enabled":true,"skill_injection":"on_demand"}}` | 内置（v2 定型版） | 现行基线 |
| E0′ | 同 E0（include/llm/call_batch 全 false） | **E0′ 变体**（见 §3） | 文案/路由效应（三问边界句）单独效应 |
| E1 | E0′ + `"include":true` | 内置（LIB 段随门出现） | 库复用增量；**cold/warm 分开跑**（cold = 空 `lib/`；warm = 预置上轮产出的库——摊销假设的首轮数据） |
| E2 | E1 + `"llm":{"enabled":true,"default_model":"<判别模型>","models":["<判别模型>","<升级模型>"]}` | 内置（LLM 段随门出现） | 语义批量正确率/成本效应 |
| E3 | E2 + `"call_batch":true` | 内置（BATCH 段随门出现） | 墙钟改善 + 正确率无回归 |

E1/E2/E3 用内置技能（门开即披露对应段）；E0′→E1/E2/E3 比较的是"对应能力及
必要披露的整体增量"，不声称运行时与披露完全分离（设计 §8 既定口径）。

**E1 注意**：收紧 `fs_write_dirs` 的部署必须把 `lib/` 列入可写集，否则 `saveLib`
全拒（README 库与复用节）；`lib/` 的版本管理交给 git，评测任务状态重置时清
workdir 但保留 warm 臂的预置库。

**E2 注意**：判别模型与升级模型的档位、`max_tokens`（先用目标模型验证再设小）、
价目与日期按 §8.1 存档；result.json 的 `meta.llm` 台账（per-model known_usage/
known_calls/unknown_calls/reserved/settled/over_reserve）是内层成本的一手归集口，
缺 usage 的样本不得静默计零。

## 3. E0′ 装置（三问边界句变体）

- 文件：`examples/eval/e0prime-skill/run-code/SKILL.md`
- 部署：整个 `run-code/` 目录拷入**每个任务 workdir** 的 `.tinyagent/skills/`
  （磁盘同名覆盖内置技能；batch copy 模式 `.tinyagent/` 随行是技能系统事实）。
  跑前检查隔离 workdir 与全局技能目录（`~/.config/tinyagent/skills`）无旧副本
  覆盖（routing report 的既有坑）。
- 内容：内置技能去三段门控节（LIB/BATCH/LLM——磁盘技能不做标记剔除，须手工
  保持无门控内容），「何时加载 / 何时换轨」段重写为附录 A③ 冻结文字（三问
  边界，含 lib/llm 路线点名——门全关时模型试走该路线得诚实 unknown_tool/
  保留名拒绝，这正是文案效应的测量对象）。
- 指纹（sha256，全 64 位）：

```
0fdee8815ce6f16eb4204bb09d553efb672d1335691a16e2b46b3490c101d9e6
```

边界句文字本体以 code_llm_design 附录 A③ 为准（冻结规格，变更走该文档修订
记录）；本文指纹钉的是"E0′ 臂实际加载的文件"。两者不一致 = 装置漂移，先修
再跑。

## 4. 四路线外部对照（§8.1）

①外层直接处理；②强模型适度分组；③普通脚本 + 模型 API + 文件库；④TinyAgent
编排。各路线相同数据权限、业务标准与质量门槛；允许各自合理调优并预先限定调优
资源，不把代码路线独享的高质量库/标注知识藏在对照外。分开记录模型调用成本、
基础设施成本、开发/复核/维护工时；货币 TCO 须声明工时单价与假定复用次数后
才能合成，并给敏感性结果。成功口径与"单位成功成本"公式见 §8.1；缺 usage 样本
报告已知量与未知范围。

## 5. 测量协议（沿用 routing 勘误后口径）

- 记录每题首请求非缓存输入 `req1_unc`（供应商前缀缓存预热会污染冷启动口径，
  2026-09-11 勘误）；主指标 `conv_growth` 沿用 v2 定义。
- 每样本打版本戳：实际 manifest、模型、配置、注入形态、E0′ 指纹（E0′ 臂）。
  旧契约数据保留原版本，不静默混入新基线。
- 对重点摇摆题增加重复优于把同题重复当独立题数；交错运行、任务状态重置。
- 内层 llm 成本从 `meta.llm` 台账归集；外层 usage 从 result.json；两者不混流
  （台账是"已知用量"，`unknown_calls>0` 时不得称完整账单）。

## 6. 门关回归（M4 出口判据的一部分）

门关 = 与无该特性的旧二进制模型可见契约一致，钉住测试（`cargo test --features
code,mock`）：

| 测试 | 钉住 |
|---|---|
| `gate_off_matches_old_contract` | include 门关：保留名直调拒、描述/skill 契约与旧版一致 |
| `skill_gated_sections_ride_their_gates` | 三段门控节（LIB/BATCH/LLM）各自随门出现/消失，互不牵连 |
| `gate_on_description_lists_batch_helper` / 描述 overlay 测试 | 描述【库】/【批量】/【LLM 判别】段随门；BATCH 段 llm 点名仅其门开 |
| `llm_registry_membership_is_a_build_time_snapshot` | llm 注册表构建期快照（热翻不感知） |
| e2e `llmbatch` 等场景 | 真实栈上的门开行为 + 真并发 |

`scripts/sync_run_code_contract.py --check` + `scripts/gen_structure.py --check`
（CI 门）通过即描述/skill/manifest 三处同源无漂移。

## 修订记录

- 2026-09-22：M4 首版（E0–E3 配方、E0′ 装置 + 指纹、四路线协议、测量协议、
  门关回归清单）。
