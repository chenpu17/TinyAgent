# code 记忆：include 与库生命周期 技术设计（草案）

| 项 | 值 |
|---|---|
| 状态 | v0.7.5（v1 已交付，宿主能力；本次为使用教学维护，最终裁决见 code_review_brief.md） |
| 版本 | v0.7.5 |
| 日期 | 2026-09-28 |
| 关联文档 | `docs/run_code_design.md`（D 编号、prelude 封闭清单纪律 D18）、`docs/finetune_support_design.md`（附录 A 即规格纪律）、`docs/code_llm_design.md`（姊妹稿——统一边界句、责任与权限边界、共享纪律见其 §6/§10） |
| 代码基线 | master @ 91a8e6e（其后行号会漂移，以代码为准） |
| 决策编号 | 本文 MD-n 独立编号；引用 run_code_design 写 D-n、finetune_support 写 FD-n |

修订记录（历史不作为现行规格）：

- **v0.7.5（使用教学维护）**：skill 的库段按既有宿主能力定位，说明有明确复用
  需求或现成库时才使用，不为一次性任务强行建库；移除“级联判别是最高价值的
  库形态”这一未经验证的比较性表述。生命周期/API/准入不变，使用指南见
  `run_code_guide.md`；skill、本轮主设计附录与 manifest 同步。
- **v0.7.4（全链审核收口轮：交付包装 + 读取分诊 + 冻结面 verbatim 同步）**：读数来源 =
  外部全链审核（基线 9c3db9b；M1–M4 + 两轮修复复查，工程层无 P1/P2，9 项 P3 本条
  登记其中 lib 侧 4 项，姊妹稿 v0.7.7 登记其余）。
  ① **MD-14 交付包装（审核 P3）**：`__include` 成功交付从裸源码文本改为
  `{"src": S}` 包装——envelope glue 对 content 做 auto-JSON-parse，整文件恰为
  合法 JSON 的库会以对象身份到达，`(0, eval)(object)` 是**静默 no-op**（include
  "成功"却什么都没加载）；包装后 parse 落在信封上，源码恒以字符串到达
  （与 `__lib_update_index` 的 {"index": …} 同款）。对象形 JSON 文本 eval 如实
  报 SyntaxError 而非消失。回归 `lib_whole_file_json_source_arrives_as_string`。
  ② **MD-11 读取分诊（审核 P3）**：include 侧读 INDEX 从 `unwrap_or_default()`
  改为 NotFound→空索引（合法）/ 其余 IO 错误→`run_code: cannot read
  lib/INDEX.md: <e>`（fail-closed 且文案诚实，与 `__lib_index_state` 同款）。
  回归 `lib_index_read_error_is_not_no_entry`。
  ③ **A① 冻结块 verbatim 修正（审核 P3）**：附录 A① 冻结块此前缺常量尾句
  「建库/用库/修库生命周期详见…」——已改为与 `LIB_DESC_SUFFIX` 逐字一致
  （单行原样）；run_code.rs tests 新增**常量↔文档逐字断言**
  （A①/A④/A⑤ 三块，锚标记提取 fenced 块与常量全等），结构性消灭此漂移
  （哈希门禁只测"变过"，逐字断言测"与常量一致"）。
  ④ **机器侧文案补登（审核 P3）**：两轮修复新增的 lib 错误文案此前未入
  A① 冻结清单——已全量补登（索引畸形/不可读、Modify 改写与丢字段拒绝、
  新保留名门关与探测/入参/登记键/摘要/重复条目，共 10 条）。
  ⑤ **A-M9 实名修正**：trace 记录 name 为 `__include`（wire 名），hooks 伪工具
  面才是 "include"。
- **v0.7.3（外部复审轮：INDEX 通道存在性结构化 + 合并统一宿主）**：读数来源 =
  外部代码复审报告（基线 45ad4ae；探针在审核方环境，本轮固化为回归测试）。
  ① **MD-10 存在性判定结构化（复审 P1）**：v0.7.2 的「INDEX 读取错误分诊」
  靠异常文本匹配（`indexOf("No such file or directory")`），hooks 拒绝文案
  内嵌该子串即可伪造「不存在」→ 空索引初始化覆盖既有登记（探针：拒绝文案
  "approval backend unavailable: No such file or directory" 清空全部登记）。
  修订：新增保留名 `__lib_index_state`（与 `__budget` 同为只读内省类——无
  gate/审计/配额），宿主以 **fs::metadata 的文件系统事实**回答
  exists/missing；存在时 saveLib 照旧经普通 gated read_file 读取（hooks
  Deny/IO 错误仍中止保存），guest 不再解析任何错误文本。
  ② **MD-11 单一 parser（复审 P2）**：v0.7.2 的 guest 侧 `__ta_lib_line_path`
  只镜像了五键存在性与路径形状，未镜像宿主的 sha256=64hex / st∈{ok,draft} /
  ver 数字规则——毒行可随写回进入 INDEX，随后所有 include 被宿主 parser
  拒绝（探针复现）。修订：单行校验提取为宿主 `parse_lib_line`，
  `parse_lib_index`（admission）与新 `lib_upsert_index`（saveLib 合并，纯
  函数 text→text）共用——**按构造消灭 guest/host parser 漂移**（guest 侧
  近似副本已删除）。合并语义与原 guest 实现逐字节同形：同 path 原位替换
  （needs/ver 取旧行，st 恒回 draft）、无则尾部追加（空行分隔）、
  join("\n")+"\n"；rel/sha 入参同规则校验。新保留名
  `__lib_update_index` 同为只读内省类（纯函数无副作用），返回
  {"index": merged} 包装（防 content 自动 JSON 解析吃掉恰好合法 JSON 的
  退化 INDEX 文本）；真正 IO（读 read_file / 写 write_file）仍走普通 gated
  工具，hooks 面不变。回归：`lib_spoofed_not_found_hooks_message_
  preserves_index` + `lib_malformed_index_line_rejected_at_save`（探针固化）。
  姊妹稿 v0.7.6 同轮登记 llm 终止白名单。
- **v0.7.2（外部审核缺陷修复轮：lib 家族三项）**：读数来源 = 外部代码审核
  报告（基线 c555074 / 0.4.1；与本轮配套的 llm/批侧修复见姊妹稿 v0.7.5）。
  ① **MD-2 补注：lib 家族遵守 Hooks::Modify（审核 P1-2）**——`lib_reserved_call`
  原先把 Modify 当 Allow、仍执行原始路径（宿主把 lib/unsafe.js 重定向到
  lib/safe.js 时实际执行的是 unsafe）。现对**改后路径**重跑 ①lib/ 边界
  ②deny_paths，失败 → kind="denied" 拒绝（与 deny/fs 层同语义：不发
  after_tool）；执行用改后路径，after_tool 恒用原始参数，trace 补
  `in_hash_eff` 双哈希（与 host_call_tool 对齐）。
  ② **MD-10 补注：saveLib 的 INDEX 读取错误分诊（审核 P1-3）**——原先
  `readAll("lib/INDEX.md")` 的所有异常被空 catch 吞掉、按空索引 upsert，
  一次性抹掉全部既有登记与批准还报成功。现只有**确定的文件不存在**可初始化
  空索引（匹配 "No such file or directory"，Windows 腿 "cannot find the
  file"）；hooks 拒绝、IO 错误、分页异常一律中止并保留旧索引（fail-closed：
  无法识别的错误文本也中止）。库文件半态（已写、INDEX 未动）与既有
  partial-failure 语义一致。
  ③ **登记键规范化（审核 P2-6）**——`__hash_file` 输出增补 `"rel"` 字段
  （canonical workdir 相对路径，与 lib_sessions 登记键同源）；saveLib 的
  manifest 键改用 `h.rel`（`lib/./x.js`、`lib/../lib/z.js` 落规范 `path=`
  键，不再写出 parser 拒收的毒行）。写回前对既有 ta-lib 行做镜像宿主
  parser 的轻校验（结构五键齐全、path 值干净、跨行去重），畸形则中止不写。
  新增模型可见错误文案（saveLib 抛出的 JS Error）登记于本条。
- **v0.7.1（M1 落码定稿）**：助手改名 **writeVerified → saveLib**——代码基线 prelude
  已有 `writeVerified(path, content)`（v2 P1 字节保真写助手，模型可见契约在册），
  设计稿同名新助手会静默改掉既有语义，改名消除冲突（本条即修订记录，附录同步）。
  落码定稿差异：①库通道错误类 `kind="lib"`（A-M10 草案写 kind="include"；
  单一家族类同时覆盖 `__hash_file` 的错误，避免按通道分裂）；
  ②宿主源码读取上限 = `MAX_CODE_BYTES`（1 MiB，与单次代码输入同量级）；
  ③门关保留名直调文案 `"run_code: __include is not available in this configuration
  (code.include=false) — the lib helpers are disabled"`（kind="denied"）；
  ④门开时 prelude 把封闭助手集显式挂到 `globalThis`——本嵌入里间接 eval 的库源码
  跑在全局作用域、看不到外层 eval 作用域里的 prelude 声明（循环依赖用例实证），
  用户代码仍可在自身作用域 shadow；⑤生效时点：通道与 prelude 走调用期 snapshot
  （资源限制先例），描述【库】段走 def() 时快照（skill_injection 先例，热更不重写
  已发出的 schema）。附录 A 文案按落码文本冻结。
  **复审补钉（2026-09-22，外部设计复审 F3）**：M1 首报的验收覆盖有三处口径偏松，
  补四测后钉正——A-M9 逃逸腿（`../` 拼写/符号链接，伪造同 hash INDEX 也拒）、
  A-M2 负腿（let/const/class 与 strict 均不导出）、A-M10 readAll+eval 限制钉、
  A-M11 多库 upsert/INDEX 写失败不冒充可用；A-M12 伪造 st 腿以生命周期 e2e 的
  批准改写同机制覆盖并记录为限制（见 §8 M1 行）。
- **v0.7**：明确默认流程准入不是来源认证；记录旧 draft 重新登记反例和宿主
  可信部署前提；会话登记绑定路径与内容哈希；质量证据绑定版本与失效规则；
  统一 verify、git、测试和附录，里程碑接入 V0-A/V0-B。
- **v0.6**：writeVerified 新建/更新均重置 draft；独立质量基准、资产四件套。
  解决了更新保留 ok，但未解决批准者身份和重新登记问题，v0.7 收窄保证。
- **v0.5**：增加 draft/ok 与同会话准入。当时“来源认证闭合”“无跨会话新增
  攻击面”的论证撤回；当前保证见 MD-14 与 §5。
- **v0.4**：宿主加载边界、manifest、hash、独立 gate；hash 仅为完整性校验。
- **v0.1–v0.3**：选择文件记忆，放弃持久沙箱；分离纯断言与判别探针。

本稿描述的 v0.7 行为已全部落码（M1/M2 完成，结项状态见
[code_review_brief.md](code_review_brief.md)）；文中的代码基线仅为起草时核对
参照，当前实现以 master 为准。MD-1～14 保留编号；与姊妹稿
`code_llm_design.md` v0.7 配套，质量政策与价值验证以该稿 §6.3/§8/§9 为准。

---

## 0. 定位：时间维度

run_code 现在的代码是**一次性消费品**：模型本轮生成、本轮执行、执行完即焚——生成本身是 token 成本与出错风险。llm 原语（姊妹稿）扩展的是**能力边界**；本特性扩展的是**时间维度**：将有效程序保存为候选资产；经过维护与重复验证的部分才形成长期复用价值。

**摊销数学（待验证假设，非设计前提——姊妹稿 §8 由 E1 期量出）**：首次生成一个库 ≈ 2–5k output tokens + 若干调试轮；复用一次 ≈ 一行 `include` + 200–500 tokens 胶水；假设约 5–10 个同类任务回本（数字以评测为准）。复用可以保留已有修复，降低重复生成引入同类错误的机会；依赖、模型与业务规则变化后仍须重新验证，不承诺永久正确。

**与另外两个能力的协同**：callBatch 的批量骨架、llm 的判别函数、include 自身的载体，全部沉淀为文件系统资产。与 llm 原语咬合后，判别函数库（便宜模型先判、unsure 升级的级联策略）在 skill 目录里自底向上积累——领域程序与证据归用户保存；运行仍依赖约定的工具、模型与宿主；按姊妹稿 §10.2 的判定规则自筛：include 加的是文件 + 会话准入状态（执行责任）。

**（v0.6）复利资产四件套**：「部署越久积累越厚」不自动等于「资产越有价值」——库会重复、过时、依赖旧接口，会把某个场景的错误假设固化；语义库即使源码不变，模型版本、数据分布与业务标准变化也可能使过去的修复失效。真正有复利的资产 = **程序 + 适用条件 + 独立质量证据 + 维护责任**——四件全部可文件化（manifest 人类字段：`applies` 适用条件、质量证据指针、owner/来源），不需要数据库或平台；缺任何一件，lib/ 就只是"方便复用的工作目录"而非"可被组织信任和维护的程序资产"（§4 模板同步）。

## 1. 总体架构：库生命周期

```
起草（模型，run_code 内定义函数 + 自测块 + 判别库另附 verify()）
  → 自验证（纯断言随 include 自动；判别探针显式 verify(n)，见 §3.4/§3.6）
  → 落盘（writeVerified：write_file("lib/xxx.js") → __hash_file → INDEX 登记 st=draft）
────── 文件跨会话保存；版本记录与执行批准分别管理 ──────
  → 发现（v1：read_file("lib/INDEX.md")；v2：skill 索引行，见 §4）
  → 复用（include(path)：宿主校验【lib/ 边界 + deny_paths + 注册 + hash + admission】
         后交付源码 → guest eval；需要时局部覆写 = 本次执行 patch，见 §3.5）
  → 晋升（负责人核对内容与证据后标 st=ok；可信部署由宿主保护批准记录，MD-14）
  → 演进（修订 → writeVerified 回 draft → 原质量证据标过期 → 重新验证/批准）
  → 停用（不适用/失效的库由负责人撤回复用；可信部署还须撤销宿主授权）
```

## 2. 关键决策记录

| # | 决策 | 理由 | 否决的备选 |
|---|---|---|---|
| MD-1 | 载体 = workdir 文件（代码 + manifest + 质量证据），不建核心资产数据库 | 输入与版本显式、可检查和迁移；纯函数在同输入/依赖下可重放，外部工具与模型结果仍可能变化 | 自动保存所有 run_code 为可信资产；把同源码等同于同语义结果 |
| MD-2 | **（v0.4 重写）加载原语 = 宿主保留名 `__include` 通道**：prelude `include(path)` 包装调用；宿主侧依次执行 ①lib/ 路径限定（相对 workdir 的 `lib/` 前缀内，`..`/绝对/符号链接逃逸按既有路径拦截走查拒绝——原始拼写逐组件、根锚定、最深已存在祖先 canonicalize）②deny_paths 策略（host 侧对 lib 路径复用同一策略判定）③hooks 闸门（以伪工具 entry：name="include"、args={path}、subagent="__code__"——宿主可 Deny include 本身）④trace 记录；全部通过后**返回源码文本**，prelude 侧 `(0, eval)(src)`；幂等 + 环切断（标记先置再 eval） | v0.3 的 `readAll + eval` 纯 prelude 方案没有宿主强制的路径边界（专家 2 P5：描述说"加载 lib/ 下的库"、实现却吃任意路径）；保留名方案把边界、策略、审计都变成机制而非约定。eval 必须留在 guest 侧（宿主无 JS 引擎语义） | 纯 prelude readAll+eval（无边界强制，通道与文案不符）；宿主侧 eval（宿主不持有 guest 堆——做不到）；完全禁止 eval（失去加载语义） |
| MD-3 | v1 使用间接 eval：库以函数声明/var 导出，无顶部 strict 指令；推荐普通多行源码。命名使用库前缀或单一命名空间，避免 judge/verify 等全局名相互覆盖 | 顶层 let/const/class 与 strict eval 的声明不会留在共享全局；宿主加载不经过 readAll，单行分页限制不再是 include 的限制 | 无声明契约；把当前全局 eval 宣传为通用 JS 包生态兼容 |
| MD-4 | 两类检查分开：纯断言在 include 时执行，消耗少量 fuel/墙钟；`verify(n)` 显式调用检查固定样例上的链路健康。实际批次质量另按姊妹稿 §6.3 抽检 | 不在加载时隐藏模型调用成本；纯断言与固定探针都不能替代实际输出质量证据 | 每次加载自动调用 LLM；把 verify 当批后抽检；自测等同安全认证 |
| MD-5 | 局部覆写 = 本次执行 patch：include 后重定义函数，只在当前新堆生效；写回重新登记并回 draft | 减少整库重发；覆写体也属于执行版本与质量证据，不继承原库的质量结论 | 依赖上次隐式堆状态的 patch；把“安全 patch”解释为内容自动获批 |
| MD-6 | 发现：v1 `lib/INDEX.md` 约定 + skill 教"先读索引"；v2 单一索引 skill（**不逐库占 level-1 行**）+ lib-as-skill（`SKILL.md` + `lib/`） | level-1 索引行是常驻 system prompt token 税，几十个库就是持续成本，且恰在 routing 系列优化过的披露面上——渐进披露再用一层 | 逐库一条索引行（token 税）；自动全量加载（体量进上下文/命名空间污染/依赖不显式） |
| MD-7 | 默认提供通道边界、登记内容一致性与显式复用状态；组织可信资产需要宿主批准策略（§5） | 加载便利性会提高历史代码的复用概率，必须承认跨会话污染风险；可见审计不是阻断 | 仅靠 skill、同域可写 st/hash 宣称来源认证；把所有治理放进核心 |
| MD-8 | 目录约定：v1 只认 workdir `lib/`（含 `INDEX.md`）；`fs_write_dirs` 收紧的部署须把 `lib/` 列入可写集（README 提醒）；版本管理 = git 管 `lib/`（一句话进 skill） | 少一个分叉；用户可见、git 友好 | 多目录认领（`lib/` + `.tinyagent/lib` 并存，分叉）；内建版本号（git 已是答案） |
| MD-9 | v1 不做持久沙箱、核心资产数据库、自动晋升 | 新堆保持执行生命周期清楚；持久 Store 需重做 emit 借用、跨调用状态、额度和回放语义，收益尚无证据；组织治理归宿主 | 为减少重发直接保留现有 Store；将所有持久化一概当平台化 |
| MD-10 | 注册制检查内容与准入：writeVerified 写源码 → 宿主算 hash → 更新 manifest；**新建/更新均 st=draft**，旧批准及质量成绩不沿用；会话登记内容可继续迭代（MD-14） | 原批准不得随路径沿用到新内容。该流程不验证代码业务正确性，名称 writeVerified 仅表示写入/哈希一致性 | 更新保 st；登记即正确或可信；用旧跑分给新代码背书 |
| MD-11 | INDEX.md = 轻量 manifest；宿主只解析 ta-lib 注释行（path/sha256/needs/ver/st），其余为面向模型/人的适用条件、质量证据指针、维护责任 | 机器面保持小；needs 用于发现，实际工具权限仍由调用 gate 检查。人类字段虽不强制解析，错误/过期可导致误用，绝非仅影响可读性 | 全文自由解析；因缺少自动验证就把质量元数据视为装饰 |
| MD-12 | include 对**实际交付的同一份源码字节**计算 sha256 并与 manifest 比对；不匹配即拒，修复须重新登记回 draft；记录加载哈希 | 检出相对登记值的变化；不能把先 hash 再另读文件形成的新竞态当作已解决。hash 不证明登记者身份或业务质量 | 自动修正 hash 后继续加载；将同域可改 manifest 当认证根 |
| MD-13 | **（v0.4）评测门**：`code.include: bool` 默认 false。关 = prelude 无 include/writeVerified 助手、描述无其行、skill 无库段落（与旧二进制逐字节同行为）；batch 片段合并 `Clamp::Tighten`（姊妹稿 §5 版本束成员） | 专家 2 P8：include 改 prelude/描述/skill/路由，是 LLM 可见行为变更，必须独立可回滚 gate；"开启 code 的既有用户"升级后自动看到新助手 = 基线破坏 | 无独立门（v0.3 的口头承诺，不够）；prelude 常驻 + 运行时拒（助手存在本身即 LLM 可见变更） |
| MD-14 | **默认流程准入**：st=ok 或 `(规范路径, sha256)` 在本会话宿主登记集中才可 include。登记集 clone 共享、不跨会话恢复；writeVerified 新建/更新登记当前字节。`__hash_file` 作为该流程的宿主步骤校验路径/策略并登记实际读取的内容，直调同样过 gate/trace；会话登记不是批准 | 限制未登记/旧 draft 的直接复用，支持当前迭代；不能认证源码来源。旧 draft 可读出后重新登记并执行，伪造 st 也取决于写权限；这些限制和宿主可信部署见 §5 | 声称重新登记获得来源背书；仅按路径记会话资格；把 hooks Allow 当晋升或自动覆盖准入规则 |

## 3. include() 语义规格

### 3.1 prelude 实现（封闭清单新增两个助手，D18 纪律）

```js
var __ta_inc = Object.create(null);
function include(path) {
    if (__ta_inc[path]) return;          // 幂等 + 环切断（标记在 eval 前置）
    var src = __include(path);           // 宿主保留名：边界/策略/注册/hash 全过
                                         // 后交付源码；任一不过 → throw（封套错误）
    __ta_inc[path] = true;
    try { (0, eval)(src); }
    catch (e) { delete __ta_inc[path]; throw e; }
}
function writeVerified(path, src) {
    // write_file(path, src) → var h = __hash_file(path)（宿主算 sha256）
    // → lib/INDEX.md 的 ta-lib 注释行结构化更新（upsert：path + sha256）
    // st 恒落 draft；会话登记绑定 (规范路径, h)，不是给路径永久放行
    // hash 步骤成功不等于 manifest 更新成功；部分失败必须报错，不宣称落库成功
    // verify 记录行由 verify(n) 通过后另行更新（skill 教）
}
```

- `__include` / `__hash_file` 是 `__tools_list` 同族保留名（prelude 单物理行纪律
  不变——助手清单仍是一行，用户栈帧偏移恒 +1）；
- prelude 提供便利封装，**不是安全隔离边界**；强制检查均在宿主通道。guest
  可写其他 JS、重新定义全局符号或走显式读取/eval，不能把“固定 prelude”当作
  恶意程序不可绕过的证明。
- 加载可观察元数据为规范路径、实际源码 hash、manifest st、准入依据
  （会话登记/ok）；host 从文件事实派生，不能相信 guest 自报。hooks Modify
  若改路径必须重新解析并校验；宿主需据最终字节作判断时，读取同一份源码后
  再提供最终检查机会。具体事件接入在 M1 验证，不声称现有 Hooks 已有这些字段。

- eval 失败清除加载标记；已执行的顶层代码不会回滚，出现部分全局定义时宜终止本次 run_code，修复后在新堆重跑。
- 错误应标明库路径与库内行号，实际 QuickJS 栈格式在 M1 验证；
- `MAX_CODE_BYTES` 原本只约束工具输入面；宿主读取/传输源码也须有有界长度，
  不能只靠 guest fuel/mem 限制宿主分配。M1 确认读取上限及明确错误；readAll
  分页只作用于 guest 直接读取路径。
  **库是代码不是数据**：超过几千行的"库"应拆分（skill 教）。

### 3.2 库模板（skill 内置，落库走它）

```js
// lib/judge.js —— 模板：函数声明风格，无顶部 'use strict'，保持常规多行源码格式
// （顶层 const/let/class 不存活；顶部 'use strict' 会让 var/function 也不存活，见 skill）
function ticket_judge(t) {
    // …实现（判别库内部调 tools.call("llm", …)）…
}
// 纯断言自测：include 即执行；必须零工具调用、毫秒级
(function () {
    if (ticket_judge.length !== 1) throw new Error("judge signature drift");
    // …形状/胶水逻辑断言…
})();
// 判别探针：显式调用（ticket_verify(2)），不随 include 自动跑
// 样例默认内嵌字面量（与库同字节）；大样例集才走 lib/xxx.cases.json 逃生门（§3.6）
var TICKET_VERIFY_CASES = [
    { in: "…", want: "bug" },
    // …n 条…
];
function ticket_verify(n) {
    // …对前 n 条内嵌样例真跑判别，全部命中返回 {passed: n, mismatches: []}，
    //   任一不命中 throw——见 §3.6…
}
```

落库：`writeVerified("lib/judge.js", <上述源码>)`——登记 + 哈希闭环自动完成。

### 3.3 间接 eval 的声明与源码兼容性

ES 规范对 eval 代码的声明实例化：间接 eval 在全局环境执行，`var`/`function`
声明落到全局；但 **`let`/`const`/`class` 声明实例化在该次 eval 自有的声明环境里，
eval 返回即丢弃**（全局词法环境只接收 Script 代码的词法声明，不接收 eval 代码的）。
即：

```js
// lib/judge.js 若长这样：
const judge = (t) => { … };    // 不向调用方导出 judge；直接访问可能 ReferenceError
// 必须长这样：
function judge(t) { … }        // 或 var judge = …
```

**变种二（更隐蔽）：顶部 `'use strict'` 指令**。eval 代码的指令序言里有 strict
指令时，该次 eval 变 strict eval——**strict eval 的 var/function 声明也实例化到
自有变量环境、返回即弃**，连函数声明风格都救不了。这一变种比 const 更险：现代
构建链（Babel/terser 转译或 minify 产物）常自带顶部 `'use strict'`，模型"照网上
现成代码抄"极易带入；失败同样无报错。**库纪律：顶部不放任何指令序言**。

**源码格式与兼容性**：推荐多行源码便于模型修改和人工检查。v1 的 `__include`
直接交付宿主读取的源码，不走 guest `readAll`，因此不能再用 readAll 的单行
分页限制解释 include 失败。仍有内存、fuel 与墙钟约束，不承诺任意体积库可用。

模板与文档回归覆盖：函数声明可见、let/const/class 不导出、strict eval 的
局部声明、多个库的命名冲突、加载异常。模板使用命名空间约定即可，无需在 v1
引入完整模块系统；这是小型任务库，不以兼容 npm/构建产物为目标。

### 3.4 自测分层语义

**① 纯断言部分**：顶层 IIFE 随 eval 自然执行，无开关、无例外，**每次** run_code
include 都跑（新堆每次从文件重建）——覆盖形状/胶水逻辑的漂移与篡改，消耗少量执行预算，不发模型请求。
纪律（skill 钉死，无法机制强制）：纯断言、零工具调用、不产生输出。自测 throw →
include throw → 错误面直达模型。

**② 判别探针（`verify`）**：不自动、不进 include 路径——调用时机与成本由编排方
（外层模型/库使用代码）显式决定，见 §3.6。这个分层是诚实的：纯函数库的漂移检测
覆盖库中实际编写的断言；判别探针仅说明这次样例执行情况，不为一段未来使用期
自动担保。断言内容和是否运行 verify 都依赖库/调用方纪律，trace 可供验收。

### 3.5 覆写语义（本次执行 patch）

```js
include("lib/judge.js");
ticket_judge = function (t) { /* 只重发修好的这一个函数 */ };
// …主流程…
```

每次执行全新堆：include 建基线，覆写只影响本次。修 bug 只重发出错函数，库其余
一行不发——迭代 token 税从"全量重发"降到"增量函数"。修好后 `writeVerified`
写回（新哈希重新登记、st 回 draft、旧质量证据失效）。

### 3.6 `verify(n)` 探针规格（链路健康检查）

判别库必须提供逻辑上的 `verify(n)` 探针；具体名称带库前缀或命名空间，
避免多库相互覆盖（如 ticket_verify）。下文 verify 为角色简称。`verify(n)`
是**链路健康检查**——固定样例是回归桩，证明"当前判别链路（prompt + 模型 +
词表）还能通过这些已知样例"；**它不证明本次实际处理结果的质量**（重跑固定
样例不能替代对本批实际输出的独立抽检——质量闭环三层表与抽检纪律的属主在
姊妹稿 §6.3，本文引用不重复）：

- 行为：对 `n` 条**标注样例**真跑一遍判别链路；全部命中返回
  `{passed: n, mismatches: []}`，任一不命中 throw（文案含错例与期望/实际值）；
- **样例载体默认库内嵌字面量**（模板中的 `TICKET_VERIFY_CASES`，命名随库而定）：
  小样例内嵌便于携带和版本管理；同文件并不保证自测会发现恶意修改，攻击者
  可以同时改程序与预期值。大样例可以单独文件保存，记录内容哈希并纳入证据
  依赖；文件个数不直接等于攻击面倍数；
- 调用时机（skill 教，两处）：**批量开始前**——首个使用判别库的 run_code 先
  `verify(2~3)`，通过才进批量（探针失败需先区分请求故障、程序错误与语义回归，回外层重估）；**批量
  完成后**——质量断言走**对本批实际输出的独立抽检**（姊妹稿 §6.3：外层直判
  或人工，不是重跑 verify——v0.6 修正 v0.5 的混淆）；
- **独立领域质量基准（v0.6）**：判别库的长期质量证据 = **独立保留的测试数据**
  （人工/业务方维护的标注集，与库分离存放，如 workdir 的 `eval/`——不进 lib、
  不由生成库的会话顺手生成）；内嵌 `VERIFY_CASES` 防回归，独立基准防自欺；
  manifest 指向有版本绑定的评测记录（§4）；正式留出集不用于调 prompt；
- 成本：照扣额度、usage 进折叠点、调用进 trace——显式可见，无隐藏税；
- INDEX.md 登记：verify 记录行（日期、样例数，§4 格式）——跨会话的"最近一次
  检查"可读；健康检查记录与独立质量记录分开更新，不自动修改批准状态。

## 4. 发现机制与 manifest

**Tier 1（v1）**：`lib/INDEX.md` = **轻量 manifest + 人类目录**（MD-11）。run-code
skill 教一条纪律：「任务有复用相时，先 `read_file("lib/INDEX.md")` 再决定写新
代码」。模型感知库 = 开工时读一次小文件，与它今天感知 workdir 任何文件是同一个
动作。格式约定（skill 给模板；下例均为字段占位，无实测成绩）：

```markdown
# lib/INDEX.md
<!-- ta-lib: path=judge.js sha256=<full-sha256> needs=llm ver=1 st=draft -->

## judge.js —— 工单分类
- io：judge(text) → bug / feature / billing / unsure
- applies：客服工单首帖；业务标签标准 ticket-v3；不含追加回复
- owner：<业务维护人>；来源：<任务/提交记录>
- health：<检查日期、库 hash、模型配置、样例数、通过/失败>
- quality：eval/reports/judge-v1.json（待验证；不得填未经执行的跑分）
- lifecycle：candidate；不适用或质量证据过期时停止自动复用，联系 owner
```

**质量记录的最小内容**（文件格式由评测装置选择，核心不解析）：

| 字段组 | 含义 |
|---|---|
| 执行身份 | 库 hash、实际加载的依赖 hash 集、覆写代码 hash（如有）、提示词/配置版本 |
| 外部环境 | 实际模型/版本、网关和推理参数、相关工具接口版本；供应商不提供固定版本时明确“不完全可固定”及评测日期 |
| 基准身份 | 数据集版本/hash、开发/留出划分、业务标准和质量政策版本、评测器版本 |
| 结果证据 | 各类错误、覆盖/拒答/失败、抽检方法与规模、区间或样本限制、耗时/费用/人工工时；原始报告指针 |
| 责任与状态 | 维护人、验证时间、适用范围、当前/过期/失败状态与原因 |

**失效规则**：内容/依赖、模型配置、数据分布、接口或业务标准发生相关变化时，
历史报告仍保留，但不能沿用“当前有效”结论。源码更新回 draft；仅外部环境变化
时即使 st 仍为 ok，也须将质量状态标过期、重新验证。**执行批准与质量达标是
两个判断**，ok 不保证某个新任务可用。v1 由库负责人/宿主执行此政策，核心
不自动检测模型漂移，不引入一套依赖解析或治理数据库。

- 机器字段仅 `path/sha256/needs/ver/st`，宿主只解析 ta-lib 行；缺失/重复冲突
  条目视为无有效登记并明确报错，不能默选一个。
- 其余为模型/人消费的元数据。质量记录缺失、版本不匹配或适用条件不符时，
  按未验证候选处理，不能把元数据错误降格为排版问题。
- needs 帮助发现，实际权限由工具调用 gate 强制；未启 llm 的纯函数库可用，
  需要 llm 的库不能仅靠 include 成功宣称任务可执行。
- ver 仅为可读修订号；内容身份以 hash 为准。Git 保存版本记录，不自动批准。

**Tier 2（v2）**：单一 `code-libs` 索引 skill 占一行 level-1（正文 = 库清单，
按需 read）；库打包为 `SKILL.md` + `lib/`。**通道经代码核验成立**：
`read_skill` 支持 `file` 参数（`src/tools/skill.rs:103`，可取 `lib/foo.js` 这类
skill 内相对路径）；`available_to_code()` 能力标记在代码基线已存在（`tools/mod.rs:360`
及 file/shell/mcp/SharedTool 各实现）——Tier 2 是**翻转 ReadSkill 的标记**并把
skills 目录路径解析接进 include（全局库的注册制/hash 纪律随通道平移），不是新增
机制。备选（v2 设计时再选）：`skills:` scheme 由 host 解析、装配期把全局 skill
的 lib 路径注入 guest 可读白名单。

## 5. 信任模型：默认复用流程与宿主可信部署

代码变为资产后，受污染内容可以跨会话被重新发现、加载和执行；“验证过”的
描述还可能降低后续模型的戒心。便利通道改变复用概率与默认行为，不能仅因
guest 原本能 eval 就断言没有新增风险。

### 5.1 默认保证与明确反例

默认 include 强制路径/deny/hooks、登记一致性、hash 和 MD-14 的流程准入。
它检出文件偏离登记值、拒绝旧 draft 的**直接加载**，并记录加载版本；不认证
登记者身份，不证明业务正确性，也不保证模型遵守不绕过的 skill 指令。

必须在文档与 A-M12 中保留以下反例：

1. 新会话读出旧 draft → `writeVerified` 重新登记相同字节 → 本会话 include
   可用。无需伪造 ok 或裸 eval，已经说明会话登记不是来源背书。
2. 有写权限的调用方可改 INDEX 的 st/hash；固定 prelude 并不剥夺其他写通道。
3. 显式读取后 eval 仍可执行代码；仅拦 include 不是整个执行面的授权边界。

skill 应在旧 draft 被拒时请求负责人处理，不教“重写即可批准”。这改善正常
工作流，但不能作为抵抗被诱导模型的硬保证。writeVerified 的“Verified”只表示
写入与登记字节一致；`verify()` 只检查样例；`st=ok` 只表达批准声明，三者不同。

### 5.2 宿主需要可信资产时的责任

可信部署是宿主策略组合，不是新增的 code 配置模式或已经交付的安全特性：

- 批准依据必须来自 Agent 不能修改的记录或受保护发布目录，绑定规范路径与
  内容 hash，登记批准者/政策/撤销。可使用现有仓库评审与部署流水线；提交本身
  不代表审批，单纯让模型不可见也不等于不可写。
- 发布与起草分开：起草目录可写，执行发布库的 Agent 无权改源码或批准依据；
  严格部署可禁用运行期落库，只加载经发布流程带来的 ok 库。候选库在单独
  受限工作区试验，审批成本进入 V0-B。
- hooks 检查最终内容身份，保护所有可写批准依据的路径（含 shell、自定义工具
  和外部进程）。hooks Allow 仅表示“不额外拒绝”，不会给 draft 晋升；现有
  hooks 也不是操作系统隔离。高强度保证须使用权限/容器等宿主边界。
- 若要求“只能运行批准代码”，还必须约束顶层 run_code 自由源码、读取/eval
  与其他执行通道；本方案是自由编程运行时，单靠 include gate **不提供**这一
  模式。仅能落实 include 策略的部署，应只声称 include 通道受控。
- 重建 Agent/恢复会话后重装策略，宿主负责撤销与批准记录持久化；核心的会话
  登记集不随 resume 恢复。质量状态依 §4 管理，不以批准代替当前质量证据。

M1 提供通道边界与内容审计契约，可信部署的端到端保证由 V0-B 针对实际宿主配置
单独验收。未装这些政策时，交付为“可审计的代码复用”，不能称来源认证闭合。

## 6. batch 行为说明（非开放问题）

经核验 `batch.rs` 的 `COPY_EXCLUDE` 刻意不含 `.tinyagent/`（注释原文 "project
skills are part of the task's environment"），三条行为说明即闭合：

1. copy 模式：项目库（`.tinyagent/` 下）天然随沙箱走（COPY_EXCLUDE 刻意不含它）；
   workdir 根的 `lib/` **确定随行**——COPY_EXCLUDE 仅四项（`target/`、
   `node_modules/`、`.git/`、`results/`），`lib/` 不在排除集，作为 task 工作文件
   整目录拷入；
2. git 模式只带已提交文件，提供可追溯版本；是否被评审取决于仓库政策。
   拷入后的库仍须通过同一 hash/准入检查，不因已提交而自动把 draft 晋升为 ok；
3. batch 运行期间起草的库留在该任务沙箱，不同步兄弟任务——隔离语义的本意；
   "batch 期间学习"需事后人工 promote 回项目目录（与全局晋升同纪律）。

## 7. 测试计划

| 验收 | 测试 |
|---|---|
| A-M1 include 基础 | 函数可用、幂等（二次 include 无副作用）、环切断（自包含库 a↔b 不死循环） |
| A-M2 词法声明与模板 | 真实 QuickJS 文档块验证 function/var 导出、词法声明不导出、strict eval 不导出；带前缀模板多库共存；单行源码按宿主加载路径验收，不沿用 readAll 限制 |
| A-M3 自测自动执行（纯断言） | 登记一个断言失败的库后 include 抛错；裸改文件先由 hash 拒绝；测试分别证明两个机制，不能混作“自测能防篡改” |
| A-M4 策略继承 | deny_paths 命中的库路径 include 被拒；trace 记录该调用（args 哈希在案） |
| A-M5 覆写语义 | include + 重定义同名函数，本次生效、文件不变；writeVerified 写回后新哈希登记 |
| A-M6 生命周期 e2e | 起草登记→新会话直接加载 draft 拒绝→负责人批准相同 hash→加载→局部覆写→写回回 draft、原质量证据过期→重新验证/批准；会话登记不跨 resume 恢复 |
| A-M7 batch 行为 | copy 模式沙箱内 workdir 根 `lib/` 随行可达（COPY_EXCLUDE 四项不含它；A-M 系列跑在隔离目录下）。`.tinyagent/` 随行是技能系统事实、其 guest 侧可达性归 Tier-2（M3）验收 |
| A-M8 verify 探针 | 判别库 verify(n)：全过返回摘要、任一错例 throw；批量前 verify 失败 → 外层收到明确错误不进批量；verify 的 llm 调用进 trace 与额度（mock 按 model 分流）。✅ 2026-09-22 随姊妹稿 M3 落码：`am8_verify_probe_passes_then_batch_fanout_proceeds` / `am8_verify_probe_failure_blocks_the_batch`（include × llm × callBatch 三通道合流，失败腿钉住"批次未派发 + 失败出口审计"） |
| A-M9 通道边界（v0.4） | `__include`：lib/ 外路径拒（含 `../` 拼写逃逸与符号链接——既有路径拦截走查三纪律全过一遍）；hooks Deny include → `kind="hooks"`；trace 记录（name="__include"——wire 名；hooks 伪工具面才是 "include"） |
| A-M10 注册制/hash 闭环（v0.4） | 未登记文件 include → 硬拒（文案指向 writeVerified）；writeVerified 后成功；篡改文件一字节 → `kind="include"` 硬错（文案含期望/实际哈希前缀）；删 INDEX 条目 → 硬拒；**显式 readAll+eval 仍可行**（记录 include 门控不覆盖全部执行路径的限制） |
| A-M11 manifest 解析（v0.7） | 仅解析机器行；缺失/重复冲突拒绝；多库 upsert 不串条目；部分写入失败报错，未完成登记不冒充可用 |
| A-M12 流程准入与限制（v0.7） | 新/更新均 draft；同会话正确 path+hash 可用，跨会话直接加载拒；ok 库修改重登回 draft；**旧 draft 读出后重新登记可在新会话执行、可写 st 被伪造后默认可能放行**，作为限制测试，不宣称已阻断；宿主可信配置另测受保护批准记录/内容校验与拒绝路径 |

## 8. 实现里程碑（与姊妹稿 v0.7 的 V0-A/V0-B 对齐）

> **结项状态（2026-09-27，唯一状态入口：[code_review_brief.md](code_review_brief.md)）**：
> M1/M2 已交付（✅ 见表内），include/saveLib 按「宿主能力、默认关闭」结项——
> 负责人裁决：保留但不做模型侧促采用，采用入口由宿主界面驱动（tinyworker 项目，
> 不构成本轮结项依赖）。M3（Tier-2）确认移出本轮，另立项才重启。V0-B 的复用
> 收益验证以「未证」归档（证据不足为合格结题），不再派生采用率优化实验。

| # | 内容 | 出口判据 |
|---|---|---|
| M1 | **include v1**：`__include`/`__hash_file` 保留名 + prelude include/**saveLib** + INDEX manifest（含 st）+ **默认流程准入（MD-14）** + `code.include` 门 + 钉子用例 | ✅ 2026-09-22 落码：`cargo test --features code,mock` 绿（A-M1~M7、M9~M12；A-M8 随 M3）；读取上限=1 MiB 超限错误实证（`lib_read_cap_error`）；门关模型可见契约与旧版一致含保留名直调拒（`gate_off_matches_old_contract`，`__include`/`__hash_file` 双名）。复审补钉（同日）：A-M9 逃逸腿 `../` 拼写与符号链接（伪造同 hash INDEX 也拒，`lib_channel_path_escape_legs`，unix-only）；A-M2 负腿 let/const/class 不导出 + strict 不导出（`lib_lexical_and_strict_declarations_not_exported`）；A-M10 readAll+eval 仍可行的限制钉（`lib_readall_eval_still_works`）；A-M11 多库 upsert 不串条目 + INDEX 写失败不冒充可用（`lib_index_upsert_and_partial_failure`）。A-M12 伪造 st 腿未单独成测——`lib_lifecycle_e2e` 第 3 步的 st=draft→ok 手改即同一机制（批准与伪造共享写路径，属已记录限制，不宣称阻断）。落地差异见修订记录 v0.7.1（saveLib 改名 / kind="lib" / globalThis 挂载） |
| M2 | 库模板（含 verify）+ skill 生命周期教学 + README（A-M6/7；A-M8 随 llm M3 完成） | ✅ 2026-09-22 落码：skill「库与复用」段带 `BEGIN/END LIB SECTION` 标记、装配期按 include 剔除（`run_code_skill_md`）；A-M6 生命周期 e2e（`lib_lifecycle_e2e`）、A-M7 COPY_EXCLUDE 钉子（`copy_exclude_never_lists_lib`）过；sync_run_code_contract 绿。A-M8 已随姊妹稿 M3 同日完成（见 §7 行） |
| M3（V0-B 后，v2 另立项） | Tier-2 索引 skill + ReadSkill 翻转 + 全局库通道 | 届时设计补充评审 |

本稿 M1/M2 组成姊妹稿 M1 的加载/模板工作，V0-A 通过后实现最小闭环；不要求
尚未实现的生命周期先通过 V0-B。V0-B 再验证变化任务中的复用、拒用、修订、
质量证据失效与重新批准。默认准入测试通过不等于宿主可信配置通过。

A-M8 需要姊妹稿 M3 的 LlmTool（及 M0 的 mock 分流），不阻塞 include 的纯函数
验收；A-M1～M7、M9～M12 默认流程测试先行，宿主可信配置留 V0-B 验证。

## 9. 与姊妹稿的共享前置与纪律引用

- **统一三选一边界句**：属主 = 姊妹稿附录 A③，先于评测矩阵定稿（其 §10.1）——
  本文附录只写库纪律，引用不重复；
- **评测门常备化 / 控制面判定规则**：见姊妹稿 §10.2/§10.3，本文照执——include
  的门是 `code.include`（MD-13，v0.4 从"随 M1 带门"的口头承诺升格为显式配置项，
  姊妹稿 §5 版本束成员）；
- **探路弹定位**：include 是「封闭清单改动 → 描述扩容重议（当前接近 1200 字符上限，以同步脚本为准）
  → 三处同源同步 → manifest 刷新」流程的最小先行（两个助手名、一行描述、
  门控整体出现/消失），后面 callBatch 与 llm 原语过同一道门时可以复用已有经验，但各自仍需验证行为与回归。

## 附录 A：LLM 可见文案（v0.7.1 起为落码冻结文本，变更走本文修订记录）

**① 描述（run_code 工具描述）的【库】段**——`code.include` 开启时追加在基础描述末尾
（门关时不存在，基础描述不动；见 `run_code.rs` 的 `LIB_DESC_SUFFIX`，单源）：

```
【库】（仅 code.include 开启时；关闭时上句成立、无以下助手）include(path)：加载 lib/ 下已登记的代码库到全局作用域（幂等，循环依赖自动截断，失败 throw）；saveLib(path, src)：写库文件并在 lib/INDEX.md 登记（恒 st=draft，本会话即可 include；跨会话复用需人工审校晋升为 ok）。库校验失败（未登记/内容与登记不符/超限）e.kind="lib"，重试无意义。建库/用库/修库生命周期详见 run-code 技能的「库与复用」段。
```

机器侧错误文案（同一冻结面，单源 `run_code.rs`）：

- 门关保留名直调：`run_code: __include is not available in this configuration
  (code.include=false) — the lib helpers are disabled`（kind="denied"）；
- 未登记：`run_code: '<path>' has no entry in lib/INDEX.md — register it first
  with saveLib("<rel>", src), then include it`（kind="lib"）；
- 篡改/不符：`run_code: '<path>' on disk does not match its lib/INDEX.md entry
  (file sha256:<16>…, registered sha256:<16>…) — the library changed after
  registration; re-run saveLib or restore the file`（kind="lib"）；
- draft 未登记：`run_code: '<path>' is st=draft and was not written in this
  session — admission refused. saveLib it in this session, or have a human
  review the library and set st=ok in lib/INDEX.md`（kind="lib"）；
- 超限：`run_code: '<path>' is <n> bytes, over the 1048576-byte include read
  cap — split the library (a lib file shares the size budget of one run_code
  program)`（kind="lib"）。
- 索引畸形：`run_code: lib/INDEX.md is malformed: <parse错误>`（kind="lib"）；
- 索引不可读（存在但读取失败，区别于"无登记"）：`run_code: cannot read
  lib/INDEX.md: <io错误>`（kind="lib"；不存在仍是合法的空索引，不报错）；
- Hooks Modify 改写 lib 路径后不可采纳：`run_code: hooks Modify replaced the
  path with '<p>', which is not an admissible lib path (relative under lib/,
  not matching code.deny_paths) — refusing rather than falling back to the
  original`（kind="denied"）；
- Hooks Modify 丢弃 path 字段：`run_code: hooks Modify dropped the string
  'path' field the lib helpers need — refusing rather than falling back to
  the original`（kind="denied"）；
- 新保留名门关（`__lib_index_state` / `__lib_update_index`，同 `__include`
  的门关句式）：`run_code: <name> is not available in this configuration
  (code.include=false) — the lib helpers are disabled`（kind="denied"）；
- 存在性探测失败：`run_code: cannot stat lib/INDEX.md: <io错误>`
  （kind="lib"）；
- 合并入参缺字段：`run_code: __lib_update_index needs string fields 'rel',
  'sha256' and 'text'`（kind="lib"）；
- 登记键不合规：`saveLib: registration key must be a clean lib/-relative
  path, got <rel>`（kind="lib"）；
- 登记摘要不合规：`saveLib: registration digest must be 64 hex chars, got
  <sha>`（kind="lib"）；
- 索引内同路径重复：`lib/INDEX.md line <n>: duplicate ta-lib entry for path
  '<path>' — fix the index before saving`（kind="lib"）。

**② skill（run-code）新增"库与复用"段落草案**（M2 落地时按本要点成文并冻结；正文
中 writeVerified 一律按 v0.7.1 改名读作 saveLib）：

- 何时建库：宿主有明确的跨任务复用需求（判别函数、聚合骨架、格式转换）→ 写成
  函数声明 + 纯断言自测块（判别库另附 `verify(n)`），**必须经 `writeVerified`
  落盘登记**（裸 write_file 写的库 include 会拒），同步维护 `INDEX.md` 人类
  字段（io 签名、applies、版本绑定的质量报告指针、owner/来源）。跨会话复用
  由负责人核对内容与证据后标 ok；严格部署由宿主发布。遇到旧 draft 被拒，
  提示负责人处理，不用重新登记绕过。**内容更新后回 draft，须重新验证/批准**；
- 何时查库：宿主有明确复用需求或已提供现成库，先 `read_file("lib/INDEX.md")`（对照 `needs` 与当前
  能力），有现成库就 include，只写增量胶水；修 bug 用覆写（include 后重定义
  该函数），跑通自测后 writeVerified 写回（新哈希重新登记）；
- 库纪律：只放函数声明风格；**顶部不放 'use strict'**，保持便于检查的多行源码，
  命名带库前缀。宿主 include 不受 readAll 分页限制。纯断言不调用工具；判别
  探针显式运行，小样例内嵌。库是代码，不把大批业务数据塞进库；版本管理交给 git；
- 判别库使用纪律：批量前 verify 做链路健康检查；实际输出按场景质量政策
  核验，区分已处理与已验证，未满足发布条件时交付待复核。内嵌样例不替代
  业务方留出基准；程序/依赖/模型/标准变化时旧成绩仅作历史证据；
- 与 llm 原语（若启用）的组合：判别函数可按需存库，是否复用应看任务质量与
  总成本；一次性任务不必建库。
