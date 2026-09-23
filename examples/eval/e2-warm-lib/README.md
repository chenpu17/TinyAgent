# E2 warm 臂预置判别库（V0-B 装置）

- 来源：级联档位探针 e2-glm r2（v0b_eval_e2_glm_inner/e2-r2）**自发产出**的
  `clf_llm.js` 蒸馏 + 评测侧补 `clfOne`（llm labels 校验 + 解析兜底）与
  `clfVerify`（7 小样例内嵌链路探针，登记轮 7/7 通过）。纯 rubric 知识，
  零 holdout 标签信息，判真值不泄漏。
- 登记：真实 saveLib 机制（CLI 一轮）→ INDEX.md 机器行为机制原生产物。
- 批准：评测侧负责人核对磁盘哈希 = 登记哈希后人工翻 st=ok + 补人类字段
  （2026-09-23）。
- 指纹：clf.js sha256 =
  7108f3780e42a2350c0b4c42a2cb72674845ee72fe02b1f162d2ff26be3bafa1
- 部署：`v0b_eval.py --arms e2w --warm-lib examples/eval/e2-warm-lib/lib
  --warm-arms e2w`（e2w = e2 配置 + 预置库；--warm-arms 限定只铺给 e2w，
  混合批内其余臂不受污染）。
- 测量问题：采用率（include() 是否发生）+ 二阶路由方差是否被库收窄
  （对照同批 e2 臂）。
