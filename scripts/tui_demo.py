#!/usr/bin/env python3
"""TinyAgent TUI 快速体验：真实 TUI + 零 API key 的脚本化回复。

默认在本进程内起一个 OpenAI 兼容的 SSE mock（只监听 127.0.0.1），再用隔离的演示目录
启动真实 TUI 二进制。流式 Markdown、工具调用、思考折叠、队列、取消、失败态都能在不配
模型、不花 token 的情况下逐个试出来。

    ./scripts/tui_demo.py              # 脚本化演示（默认）
    ./scripts/tui_demo.py --light      # 浅色调色板
    ./scripts/tui_demo.py --release    # 优化构建：编译慢，运行更顺
    ./scripts/tui_demo.py --no-build   # 复用已经构建好的二进制
    ./scripts/tui_demo.py --real       # 用自己的配置和模型，不起 mock
    ./scripts/tui_demo.py --real -- --config examples/chat.json
                                        # --real 之后写在 -- 后面的参数透传给 tiny-agent

只依赖 python3 与 cargo。这不是 CI 门禁（那个是 scripts/e2e_scenarios.py），只是本地入口。
"""

import argparse
import itertools
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEMO_DIR = Path(tempfile.gettempdir()) / "tinyagent-tui-demo"
KEY_ENV = "TINYAGENT_TUI_DEMO_KEY"
MODEL = "demo-scripted"
# A failed request takes this long to fail: the queue only exists while a run
# is in flight, so this is the window for queuing a second message behind it.
FAIL_DELAY = 8.0
CALL_IDS = itertools.count(1)

CHEAT_SHEET = """\
TinyAgent TUI 快速体验 —— 脚本化演示，不需要 API key
  随便输入一句开始；关键词：tool（工具调用）· think（长思考）· slow（慢速 / 队列 / 取消）· fail（失败 / 暂停）
  Ctrl+J 换行 · Ctrl+O 最新详情 · Esc 取消或清空输入 · 空闲时 Ctrl-C 连按两次退出
  想用自己的模型：./scripts/tui_demo.py --real
"""

GUIDE = """\
这是**脚本化演示**：没有接真实模型，回复由本地服务按关键词生成。

### 可以试试

- `tool` — 工具调用：参数流式构造、状态与耗时，一次成功、一次失败
- `think` — 长思考：流式时只显示尾部，结束后折叠
- `slow` — 持续十几秒的慢速回复：趁机再发一条看**队列**，或按 `Esc` 取消
- `fail` — 约 8 秒后失败：趁这段时间再发一条，看队列暂停与 `/resume`

### 之后

- 工具结束后：`/out 1` 看最新结果，`/call 1` 看完整参数，`Ctrl+O` 重印最新一条详情
- 思考折叠后：`/thinking` 重印全文；`/export` 导出的文件里保留完整原文
- 输入 `/` 后按 `Tab` / `Shift+Tab` 补全命令，`/help` 列出全部

```text
Ctrl+J 换行 · Ctrl+O 最新详情 · Esc 取消或清空输入 · 空闲时 Ctrl-C 连按两次退出
```
"""

THINK_LINES = [
    "先把需求拆开：这次要演示的是长思考的折叠。",
    "回忆界面规则：超过 8 个显示行的思考，在结束时折叠。",
    "所以需要生成足够多的行，让折叠条件成立。",
    "同时内容要像真实的推理，而不是重复的填充。",
    "第一步，确认终端宽度：窄屏下每行会折成两行，行数只会更多。",
    "第二步，考虑流式阶段：界面只保留尾部，前面的内容不占用输入空间。",
    "第三步，折叠后保留头部和最后 3 行，读者仍能看出结论。",
    "第四步，完整原文要能通过 /thinking 追加重印。表情检查 👩‍💻❤️",
    "第五步，导出的文件里也应有全文，审计不能丢。",
    "第六步，折叠不应影响正文：答复另起一块。",
    "第七步，取消或失败时不能留下悬空的思考块。",
    "第八步，子 agent 的思考带 [name] 标签，走同一套渲染。",
    "第九步，中英文混排与表情不能被逐字拆开。",
    "第十步，以上各点都要有单测和真实终端验证。",
    "第十一步，没有别的遗漏了。",
    "第十二步，结论：可以给出最终答复。",
]

THINK_ANSWER = """\
思考结束。上面的 ✻ 区块超过 8 行，已折叠成头部加最后 3 行。

- `/thinking` 重印完整全文——追加在下方，不改写上面已提交的内容
- `/thinking 1` 与它等价；之后再有新的思考块，`/thinking 2` 可以回看这一块
- `/export <路径>` 导出的文件里保留全文
"""

TOOL_THINK = [
    "用户想看工具调用的效果。",
    "先写一个文件，展示成功路径；",
    "再读一个不存在的文件，展示失败时自动展开。",
]

HELLO = "你好，TinyAgent 👋\nThis file was written by the write_file tool during the TUI demo.\n"

SLOW_HEAD = """\
下面这条会持续十几秒，用来体验**队列**和**取消**：

1. 现在直接输入下一条消息并回车——它进入队列，显示在输入框上方，等这条结束后自动发送
2. 想中途停掉就按 `Esc`，取消会连同排队中的消息一起丢弃
3. 取消时状态栏依次显示 `cancelling` → `idle`

顺便看看流式 Markdown 的增量渲染：

"""
SLOW = SLOW_HEAD + "\n".join(
    f"{i}. 第 {i} 行：列表项在到达时逐条出现，编号自动对齐" for i in range(4, 16)
) + "\n"


def text_parts(message):
    """Text pieces of one message, whatever the wire shape (string or parts)."""
    content = message.get("content")
    if isinstance(content, str):
        return [content]
    if not isinstance(content, list):
        return []
    return [
        part["text"]
        for part in content
        if isinstance(part, dict) and part.get("type") == "text"
        and isinstance(part.get("text"), str)
    ]


def latest_prompt(messages):
    """The message the user just typed. After a failed run the kernel merges
    the next message into the same user turn as a second text part, so only
    the LAST part is the new prompt."""
    for message in reversed(messages):
        if message.get("role") == "user":
            parts = text_parts(message)
            if parts:
                return parts[-1]
    return ""


def turn_tool_results(messages):
    """Tool results of the current turn: everything after the newest user
    message that carries text (tool runs from earlier turns do not count)."""
    start = 0
    for index, message in enumerate(messages):
        if message.get("role") == "user" and text_parts(message):
            start = index
    return [m for m in messages[start:] if m.get("role") == "tool"]


def pieces(text, size=3):
    return [text[i:i + size] for i in range(0, len(text), size)]


# Scenario keywords, ASCII and Chinese spellings. Anchored at the START of the
# message: a message queued behind `fail` ("after fail…") must not fail again
# once /resume delivers it.
KEYWORDS = (
    ("fail", ("fail", "失败")),
    ("slow", ("slow", "慢")),
    ("think", ("think", "思考")),
    ("tool", ("tool", "工具")),
)


def scenario(prompt):
    for name, words in KEYWORDS:
        if prompt.lower().startswith(words):
            return name
    return None  # anything else: the guide


def chunk(delta, finish=None):
    return {"choices": [{"index": 0, "finish_reason": finish, "delta": delta}]}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    # -- wire helpers -----------------------------------------------------

    def start_sse(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.close_connection = True

    def emit(self, value):
        data = ("data: " + (value if isinstance(value, str)
                           else json.dumps(value, ensure_ascii=False)) + "\n\n").encode()
        self.wfile.write(data)
        self.wfile.flush()

    def stream_reasoning(self, lines, pause):
        for index, line in enumerate(lines):
            delta = {"reasoning_content": line + "\n"}
            if index == 0:
                delta["role"] = "assistant"
            self.emit(chunk(delta))
            time.sleep(pause)

    def stream_text(self, text, pause):
        first = True
        for piece in pieces(text):
            delta = {"content": piece}
            if first:
                delta["role"] = "assistant"
                first = False
            self.emit(chunk(delta))
            time.sleep(pause)
        self.emit(chunk({}, "stop"))

    def tool_call(self, name, args, pause=0.25):
        call_id = f"call_demo_{next(CALL_IDS)}"
        self.emit(chunk({"role": "assistant", "tool_calls": [{
            "index": 0, "id": call_id, "type": "function",
            "function": {"name": name, "arguments": ""},
        }]}))
        time.sleep(pause)
        text = json.dumps(args, ensure_ascii=False)
        # A few fragments, so "receiving arguments" is visible in the TUI.
        cuts = sorted({0, len(text) // 3, 2 * len(text) // 3, len(text)})
        for begin, end in zip(cuts, cuts[1:]):
            self.emit(chunk({"tool_calls": [{
                "index": 0, "function": {"arguments": text[begin:end]},
            }]}))
            time.sleep(pause)
        self.emit(chunk({}, "tool_calls"))

    # -- scenarios --------------------------------------------------------

    def do_POST(self):
        if self.path != "/chat/completions":
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = body["messages"]
        try:
            self.respond(messages)
        except (BrokenPipeError, ConnectionResetError):
            pass  # the run was cancelled (Esc) or the TUI exited

    def respond(self, messages):
        kind = scenario(latest_prompt(messages))
        if kind == "fail":
            time.sleep(FAIL_DELAY)
            data = json.dumps({"error": {"message":
                "demo: scripted failure — 关键词 fail 会故意返回 500，用来演示失败态与队列暂停"}},
                ensure_ascii=False).encode()
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return

        self.start_sse()
        said = 0
        if kind == "slow":
            self.stream_text(SLOW, 0.1)
            said = len(SLOW)
        elif kind == "think":
            self.stream_reasoning(THINK_LINES, 0.12)
            self.stream_text(THINK_ANSWER, 0.04)
            said = len(THINK_ANSWER)
        elif kind == "tool":
            done = len(turn_tool_results(messages))
            if done == 0:
                self.stream_reasoning(TOOL_THINK, 0.2)
                self.tool_call("write_file", {"path": "hello_demo.txt", "content": HELLO})
                said = len(HELLO)
            elif done == 1:
                self.stream_reasoning(["写好了。现在读一个不存在的文件。"], 0.2)
                self.tool_call("read_file", {"path": "missing_demo.txt"})
            else:
                final = (
                    "演示完成。\n\n"
                    "- `write_file` 成功，`read_file` 因文件不存在失败——失败结果自动展开\n"
                    "- 写入的文件：`hello_demo.txt`（在演示目录里，退出时打印完整路径）\n"
                    "- 现在试试：`/out 1` 最新结果、`/out 2` 上一个结果、`/call 1` 完整参数，"
                    "或按 `Ctrl+O` 重印最新一条详情\n"
                )
                self.stream_text(final, 0.03)
                said = len(final)
        else:
            self.stream_text(GUIDE, 0.03)
            said = len(GUIDE)

        # Usage trailer (the client asks for include_usage): estimates, so the
        # status bar shows tokens and context occupancy like a real model.
        prompt_tokens = max(1, len(json.dumps(messages, ensure_ascii=False)) // 3)
        completion_tokens = max(1, said // 2)
        self.emit({"choices": [], "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        }})
        self.emit("[DONE]")


def start_server():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def write_config(port):
    DEMO_DIR.mkdir(parents=True, exist_ok=True)
    config = {
        "provider": "openai",
        "base_url": f"http://127.0.0.1:{port}",
        "api_key_env": KEY_ENV,
        "model": MODEL,
        "max_tokens": 4096,
        "system_prompt": "TinyAgent TUI demo. Replies are scripted by a local server; "
                         "no real model is called.",
        "workdir": str(DEMO_DIR),
        "restrict_to_workdir": True,
        "tools": ["write_file", "read_file", "list_dir"],
        "limits": {"max_turns": 8, "timeout_secs": 60},
    }
    path = DEMO_DIR / "config.json"
    path.write_text(json.dumps(config, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return path


def binary_path(release):
    try:
        metadata = subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=ROOT, stderr=subprocess.DEVNULL)
    except FileNotFoundError:
        sys.exit("找不到 cargo：请先安装 Rust 工具链")
    except subprocess.CalledProcessError:
        sys.exit("cargo metadata 失败：请在仓库根目录检查 Cargo.toml")
    target = Path(json.loads(metadata)["target_directory"])
    return target / ("release" if release else "debug") / "tiny-agent"


def run_demo(binary, light):
    server, port = start_server()
    config = write_config(port)
    env = dict(os.environ)
    env[KEY_ENV] = "demo"
    if light:
        env["TINYAGENT_TUI_THEME"] = "light"
    print(CHEAT_SHEET, flush=True)
    try:
        return subprocess.call(
            [str(binary), "--config", str(config), "--tui", "--session", "none"],
            cwd=DEMO_DIR, env=env)
    except KeyboardInterrupt:
        return 130
    finally:
        server.shutdown()
        server.server_close()
        print(f"\n演示目录（工具写入的文件在这里）：{DEMO_DIR}")


def main():
    parser = argparse.ArgumentParser(
        description="TinyAgent TUI 快速体验（脚本化演示，或 --real 用自己的模型）",
        epilog="--real 模式下，写在 -- 之后的参数透传给 tiny-agent，例如："
               "tui_demo.py --real -- --config examples/chat.json",
        formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--light", action="store_true", help="浅色调色板（默认深色）")
    parser.add_argument("--release", action="store_true", help="优化构建：编译慢，运行更顺")
    parser.add_argument("--no-build", action="store_true", help="跳过构建，复用已有二进制")
    parser.add_argument("--real", action="store_true",
                        help="不起 mock：用自己的配置和模型启动 TUI")
    parser.add_argument("passthrough", nargs="*",
                        help="--real 模式下透传给 tiny-agent 的参数（写在 -- 之后）")
    args = parser.parse_args()

    if not sys.stdout.isatty():
        sys.exit("TUI 需要在真实终端里运行（stdout 不是 tty）")
    if args.passthrough and not args.real:
        parser.error("只有 --real 模式接受透传参数")
    if not args.no_build:
        command = ["cargo", "build", "--locked", "--features", "tui"]
        if args.release:
            command.append("--release")
        print(f"+ {' '.join(command)}", flush=True)
        try:
            failed = subprocess.call(command, cwd=ROOT) != 0
        except FileNotFoundError:
            sys.exit("找不到 cargo：请先安装 Rust 工具链")
        if failed:
            sys.exit("构建失败")

    binary = binary_path(args.release)
    if not binary.exists():
        sys.exit(f"找不到 {binary}；去掉 --no-build 先构建")
    if args.real:
        if args.light:
            os.environ["TINYAGENT_TUI_THEME"] = "light"
        os.execv(str(binary), [str(binary), "--tui", *args.passthrough])
    return run_demo(binary, args.light)


if __name__ == "__main__":
    sys.exit(main())
