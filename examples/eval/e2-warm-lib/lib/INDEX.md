<!-- ta-lib: path=clf.js sha256=7108f3780e42a2350c0b4c42a2cb72674845ee72fe02b1f162d2ff26be3bafa1 needs=- ver=1 st=ok -->

## clf.js（人类字段）

- io：`clfOne({title, body}) -> label`（llm 单次判别 + labels 校验 + clfParse 兜底）；
  `clfVerify() -> {ok, total, bad}`（链路健康探针，7 小样例内嵌、显式运行）
- 适用：客服工单 7 类逐项判别；prompt 内含类定义与 account↔bug 边界口径（与协议 v2.2 同源）
- 质量证据：登记轮 clfVerify 7/7 通过（内层 deepseek-flash）；来源 = 级联探针
  e2-glm r2 自发产出 clf_llm.js 蒸馏 + 评测侧补 clfOne/clfVerify
- owner/来源：v0b 评测侧；批准 2026-09-23
