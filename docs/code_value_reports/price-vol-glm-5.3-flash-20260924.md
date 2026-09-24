# vol-glm-5.3-flash 价表溯源（2026-09-24）

工单 01R「封存与交付」节要求：提供 vol-glm-5.3-flash 的真实价格来源、
日期、计费单位、缓存语义、币种——不得用相似模型名称猜价。本页是查证记录。

## 渠道判定

网关 `http://193.112.219.133:4100/one/v1` 的 `/v1/models` 元数据：

```json
{"id": "vol-glm-5.3-flash",
 "metadata": {"routes": [{"target": "vol-openai:glm-5-3-flash-260828"}]}}
```

路由目标 `vol-openai:` 前缀 + 模型版本号 `260828` → **火山方舟（Volcengine
Ark）渠道**的 GLM-5.3-flash，非智谱直连。因此定价以火山方舟官方文档为准。

## 官方定价（火山方舟文档中心，2026-09-24 抓取）

来源：火山方舟「模型服务价格」页（docs 82379 系列，文档中心 SSR 页面），
「大模型推理」表，glm-5.3-flash 行（无输入长度分档）：

| 列 | 值 |
|---|---|
| 输入（非音频） | **0.80 元/百万 token** |
| 输入（音频） | -（不适用） |
| 缓存存储 | 0.017 元/百万 token/小时 |
| 缓存命中（非音频） | **0.23 元/百万 token** |
| 输出 | **2.80 元/百万 token** |

交叉核对：智谱开放平台官方定价页（docs.bigmodel.cn/cn/guide/start/pricing）
旗舰模型表 GLM-5.3-Flash 行：输入 0.8 元/百万、输出 2.8 元/百万、
缓存命中 0.23 元/百万、缓存存储限时免费——**两渠道价格完全一致**，
互为印证。

## 计费语义（网关实测，2026-09-24）

对网关 vol-glm-5.3-flash 发真实请求（OpenAI 兼容接口），usage 形状：

```json
{"prompt_tokens": 19, "completion_tokens": 56, "total_tokens": 75,
 "prompt_tokens_details": {"cached_tokens": 0},
 "completion_tokens_details": {"reasoning_tokens": 51}}
```

- **口径**：OpenAI 兼容——`prompt_tokens` 为总输入（含缓存命中部分），
  `cached_tokens` 是其中命中部分。与 TinyAgent `openai.rs` 的记账口径
  （input_tokens = prompt_tokens − cached_tokens，缓存读单列）一致，
  **无双重计费风险**。
- **reasoning_tokens**：completion_tokens 含推理消耗（实测 56 个
  completion 里 51 个是 reasoning）——输出侧按 completion_tokens
  全额计费，本项目 result.json 的 output_tokens 已含推理。
- **币种**：人民币元（CNY）。
- **计费单位**：每百万 token；本装置价表按 per-token 单价记录
  （0.80/1M = 8e-7 元/token）。

## 装置价表（per-token，供 finalize-g1 使用）

```json
{
  "input": 8e-07,
  "output": 2.8e-06,
  "cache_creation": 0.0,
  "cache_read": 2.3e-07,
  "currency": "CNY"
}
```

说明：
- `cache_creation` 记 0：方舟口径缓存存储按小时计（0.017 元/百万/小时），
  非一次性写入费；本项目 G1 六跑单轮短会话，缓存存储费可忽略，按 0
  记入下界（保守方向：不高估费用）。
- `cache_read` 用方舟缓存命中价 0.23 元/百万（=2.3e-7 元/token）。
- 网关为聚合网关（cc-gw）：若网关侧另有加价/倍率，本价表是**下界**
  （按上游官方价计）；真实账单以网关计费面板为准。G1 六跑预算极小
  （包络 48 万 token ≈ 0.5 元下界），该差异不构成决策风险。

## 结论

价表来源已查证（火山方舟官方定价 + 智谱官方定价互证 + 网关 usage 口径
实测），**具备 finalize-g1 的前置条件**。是否放行 G1 六跑仍由评测负责人
裁决；本文件只提供价表与溯源，不构成放行。
