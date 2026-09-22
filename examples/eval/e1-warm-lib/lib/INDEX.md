<!-- ta-lib: path=strip.js sha256=e0b15aadcc7f8f5962ab0f5b411d3183537f766c125e10d06fe0b88ece171be5 needs=- ver=1 st=ok -->

## strip.js（人类字段）

- io：`taStripCore(body) -> string`（剥噪声句后的核心串，句号连接；幂等无副作用）
- 适用：中文工单 body 的 boilerplate 剥离；TA_BP 为频次挖掘的纯噪声句池，零类信息
- 质量证据：v0b e1-cold r1 同款管道的 avgCore 统计；加载期 selfTest（噪声全剥/真实句保留）
- owner/来源：v0b 评测侧（自 e1-r1 run_code #12 蒸馏）；批准 2026-09-23
