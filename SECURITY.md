# 安全策略（Security Policy）

## 报告渠道

发现 TinyAgent 的安全漏洞请**私下报告**，不要开公开 issue、也不要公开讨论利用细节：

- GitHub 仓库 → **Security** 标签 → *Report a vulnerability*（Private vulnerability
  reporting；若仓库尚未开启该功能，请在 Settings → Code security and analysis 里启用——
  这是本仓库首选的接收渠道）。

报告时请尽量附：影响的版本 / commit、复现步骤（配置片段 + 触发输入）、你评估的影响面。
确认后先修再公开，致谢按你的意愿处理。

## 范围与信任模型（报告前先对照）

TinyAgent 的安全模型与信任边界以下两处为准，**已文档化的设计边界不算漏洞**：

- `README.md`「⚠️ 安全说明」：agent **不是沙箱**——`shell` 原样执行模型输出（无命令
  过滤）、文件工具默认不限制路径；`restrict_to_workdir` 是 best-effort（canonicalize
  判定 + 文档化 TOCTOU 窗口），硬边界是容器 / 受限用户。
- `docs/run_code_design.md` §7 / §15：run_code 的路径拦截层（deny_paths /
  fs_write_dirs / 写侧 symlink 拒绝）**防误读不防恶意**，且只作用于 4 个文件工具与
  `copy_bytes`——`shell` 的读写不经此层（读写两侧均有钉住测试）；敌对 guest 的正解
  是 `restrict_to_workdir` / hooks Deny / 从 `tools` 去 `shell`。

以下方向欢迎报告：run_code 沙箱逃逸（wasm fuel / epoch / 内存限额被绕过）、路径拦截的
**非文档化**绕过、SSE / NDJSON 解析的尺寸上限失效、MCP SSE 同源闸门绕过、供应链问题
（依赖被混入恶意版本）。

## 支持版本

只修复最新默认分支上的问题；发布标签的安全修复会随正常发布流程出
（见 README「发布流程」）。
