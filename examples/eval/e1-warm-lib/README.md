# E1 warm 臂预置库（V0-B 装置）

- 来源：v0b e1-cold r1（v0b_eval_e1_cold1/e1-r1）run_code #12 的噪声剥离管道
  蒸馏——TA_BP 频次池 + core()。零类信息（纯噪声句），判真值不泄漏。
- 登记：经真实 saveLib 机制（CLI 一轮，readAll("lib_src.js") → saveLib → include
  自测通过），INDEX.md 机器行为机制原生产物。
- 批准：评测侧负责人核对内容与磁盘哈希后人工翻 st=ok + 补人类字段（2026-09-23）。
- 指纹：strip.js sha256 =
  e0b15aadcc7f8f5962ab0f5b411d3183537f766c125e10d06fe0b88ece171be5
- 部署：`v0b_eval.py --arms e1 --warm-lib examples/eval/e1-warm-lib/lib`
  （stage_workdir 拷入 wd/lib；跨会话 include 准入已由探针验证）。
