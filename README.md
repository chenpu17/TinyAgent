# TinyAgent

[![CI](https://github.com/chenpu17/TinyAgent/actions/workflows/ci.yml/badge.svg)](https://github.com/chenpu17/TinyAgent/actions/workflows/ci.yml)

English · [简体中文](README.zh-CN.md)

A minimal single-binary AI agent: one JSON config is all it takes to run. Two modes — headless task execution (harness scenarios) or a TUI chat. Targets: low-spec embedded Linux, macOS, Windows; a refined toolset, ~15k lines of core code (the agent loop lives in `agent.rs`; the optional TUI adds ~5k), structured to be read end-to-end quickly and customized freely.

**First host: [TinySSH](https://github.com/chenpu17/TinySSH)** — a human–AI collaborative terminal (SSH client + first-class local terminal + embedded agent) that runs this kernel inside real sessions, behind approvals, audit, and explicit control hand-off.

## Features

- **Single executable**, no runtime dependencies; a musl static build can be dropped onto a router / Raspberry Pi / CI runner and just run
- **Two LLM backends**: the native Anthropic API + the OpenAI-compatible protocol (covers DeepSeek / vLLM / Ollama and friends), both with token usage accounting
- **Image input**: `--image` flag or the `load_image` tool (png / jpeg / gif / webp, base64)
- **PDF document input**: library-side `Block::Document` (the Anthropic native `document` block, processed page-by-page as "image + extracted text"; `load_document_block` reads it, `push_user` sends it)
- **A refined toolset**: `shell`, `read_file`, `write_file`, `edit_file`, `list_dir`, `task` (sub-agents; multiple calls in one turn run in parallel), `todo` (task plan, lands in result.json's `plan` field) are enabled by default; `load_image` (vision) must be added explicitly under `tools`. Every tool can be trimmed via `tools` in the config
- **Sandboxed JS execution (optional `code` feature)**: the `run_code` tool lets the model write JS inside an in-process wasm sandbox (QuickJS-in-wasm) to orchestrate the existing tools — capabilities are *constructively* confined (no network / no direct filesystem access; every tool call passes the hooks gate), with four independent limits: fuel / wall clock / memory / call count; two switches (feature + `code.enabled`), both off by default; [quick start & best practices](docs/run_code_guide.md)
- **Harness contract**: `--output result.json` writes a structured result (token usage, every tool call's input/output), `--json` streams NDJSON; semantic exit codes (`0` success / `124` timeout / `130` cancelled / `1` anything else) — ready to plug into CI / eval pipelines
- **CLI overrides**: `--model` / `--max-turns` / `--timeout` for A/B runs without touching the config
- **Batch mode**: `--batch <dir>` runs a set of task fragments concurrently, `--retries` retries transient failures with exponential backoff, `--failed` re-runs only the previous round's failed tasks and merges them back into the full report
- **TUI (optional feature)**: a ratatui chat interface — inline/streaming rendering (conversation lands in the terminal's native scrollback; native select/scroll/search work out of the box), multi-line input, input history, slash commands, token status bar; not compiled by default to keep the headless binary tiny
- **Synchronous implementation** (no tokio), `reqwest::blocking` + rustls, the core agent loop concentrated in `agent.rs`
- **Library API**: the kernel is its own crate (`crates/core`, package `tiny-agent-core`, lib name `tiny_agent`); third-party Rust programs can drive the agent loop directly via `tiny_agent = "..."`, register custom tools (`CallbackTool`), and read back honest token bills; Python (PyO3) / TypeScript (napi-rs) bindings build on it; the root `tiny-agent` package (the binary) is itself a consumer. The `mock` feature exposes a scripted provider for offline dry-runs. **Extension surface**: a Hooks interception layer (`set_hooks`: approve/deny/rewrite tool calls, audit, observe requests — sub-agents inherit automatically) + tool capability markers (`concurrency()` declares parallelism, `available_to_subagent()` declares sub-agent inheritance) — neither requires touching the core

## Build

```sh
cargo build --release                     # headless (smallest; no TUI / run_code)
cargo build --release --features tui      # + TUI chat interface
cargo build --release --features code     # + run_code sandbox (wasmtime; +≈4.6 MiB on the musl static build)
cargo build --release --features tui,code # full desktop combo (install.sh's default)
```

Cross-compiling (the [cross](https://github.com/cross-rs/cross) toolchain is recommended):

```sh
cross build --release --target x86_64-unknown-linux-musl
cross build --release --target aarch64-unknown-linux-musl
cross build --release --target armv7-unknown-linux-musleabihf
cross build --release --target x86_64-pc-windows-msvc
```

The repo-root `install.sh` installs the release binary into `~/.local/bin` (`PREFIX` overridable, no sudo needed) and builds the **`tui,code` full desktop** combo by default; override with `TA_FEATURES="tui" ./install.sh`. Official GitHub Releases ship the musl static artifact with `code` only (no TUI — the "low-spec embedded Linux" positioning favors size and static linking; build the desktop TUI yourself or use install.sh). After installing, `tiny-agent --version --json` lists which features the binary carries (the `features` array).

## Library API (programmatic use)

The complete programmatic surface — minimal usage, custom tools, custom providers, the Hooks interception layer, live-session hot updates, PDF document input, and Python / TypeScript binding examples — lives in the [library API doc](docs/lib_api.md).

## Usage

`--config` is optional; when omitted, the config file is resolved in this order:

1. `--config <path>` (explicit; even a missing file is used as-is — you get the read error, not a fallback)
2. `$TINYAGENT_CONFIG` (used only if non-empty)
3. `$XDG_CONFIG_HOME/tinyagent/config.json`, otherwise `~/.config/tinyagent/config.json`

With your model config in `~/.config/tinyagent/config.json`, just run `tiny-agent --tui` (much like `claude` / `codex`). When no config is found at all, a minimal sample config and its absolute path are printed to get you started.

```sh
# headless task run with a structured result
tiny-agent --config examples/harness.json --output result.json

# override the config's task
tiny-agent --config examples/harness.json --task "run the tests and fix failures"

# A/B without editing the config: swap model / turn cap / timeout
tiny-agent --config examples/harness.json --model deepseek-chat --max-turns 10 --timeout 300

# streaming NDJSON events for pipe consumers (composable with --output)
tiny-agent --config examples/harness.json --task "summarize the log" --json | jq 'select(.type=="usage")'

# attach an image (vision models)
tiny-agent --config examples/harness.json --task "describe this screenshot" --image shot.png

# persistent session: auto-saved each turn, --resume continues it
tiny-agent --config examples/chat.json --task "remember the passphrase PINEAPPLE" --session sess.json
tiny-agent --config examples/chat.json --resume --task "what was the passphrase?" --session sess.json

# one-shot run with no session file (CI / throwaway runs that must not pollute cwd)
tiny-agent --config examples/harness.json --task "run tests" --session none --output result.json

# batch mode: run every task fragment in a directory, 2 concurrent, private workdir copies per task, merged report
tiny-agent --config examples/harness.json --batch examples/tasks --jobs 2 --isolate copy --report report.json
# re-run only the failed tasks from the previous report.json (merged back), retrying transient failures twice
tiny-agent --config examples/harness.json --batch examples/tasks --failed --retries 2 --report report.json

# version + compile-time feature list (machine-readable so CI can assert code/tui made it in; needs no config)
tiny-agent --version --json

# MCP tool servers (stdio)
tiny-agent --config examples/mcp.json

# TUI chat mode (requires the tui feature)
tiny-agent --config examples/chat.json --tui
```

## Batch mode

Under `--batch <dir>`, every `*.json` is a task fragment (all fields optional, overriding the base config):

```json
{ "task": "fix the failure in tests/test_login.py", "limits": { "max_turns": 20 } }
```

- Each task runs the full agent flow and writes `results/<task>.json` (same format as `--output`)
- `--jobs N` controls concurrency (default 1); `--report` writes the summary: `{total, succeeded, failed, provider, totals, averages, tasks[]}` (`totals`/`averages` are sums and per-task means over the 4 token axes; each `tasks[]` entry carries `model` / `started_at` attribution — fragments may override `model`, which is how mixed-model batches stay distinguishable)
- ⚠ **The default semantics before `--isolate`**: with `--jobs > 1` all tasks **share one workdir** (the base config's; fragments cannot override it) — tasks that write the same files concurrently will clobber each other and pollute results. Two ways out:
  - `--jobs 1` (serial; or tasks that only read / pure reasoning are safe to parallelize)
  - `--isolate <mode>` gives each task a private workdir (below)
- exit code: 0 if all succeeded, else 1 — directly usable as a CI gate

### `--isolate`: per-task workdir isolation

| mode | what it does | fits |
|---|---|---|
| `none` (default) | no isolation; all workers share the base workdir | read-only tasks / `--jobs 1` |
| `copy` | **copies** the workdir to a temp dir per task, deleted afterwards. Excludes `target`/`node_modules`/`.git`/`results` (the bulk + keeps last round's results out of the inputs) | general; slow on big repos |
| `git` | `git worktree add --detach` per task, `worktree remove --force` afterwards | zero-copy for in-repo tasks; **requires the workdir to be a git repo root** (a subdirectory reports `setup_error`, no retry). **Checks out HEAD** — uncommitted / untracked files do not enter the isolated copy; commit them first or use `copy` |

- **Per-task, reused across attempts**: a task's retries see its own previous attempt's on-disk side effects (same semantics as the shared workdir)
- **Artifacts are discarded with the directory**: files written by tasks do not stay in the workdir — under eval semantics the audit surface is `result.json` (full `tool_calls`) and `report.json`; keep artifacts? don't isolate
- Composes naturally with `"restrict_to_workdir": true` (the sandbox root follows the isolated dir) — the recommended combo for untrusted batch tasks: container + `--isolate copy` + restrict. **`--isolate` is eval isolation, not a security sandbox**: `shell` can still reach everything the filesystem allows; untrusted tasks belong in a container / restricted user
- `copy`'s symlink policy (no path back into the original tree or outside it may survive in the copy): **relative** links that still resolve inside the source tree are replicated as-is (tool layouts like `node_modules/.bin` keep working and stay copy-local); **absolute** links pointing inside the source tree have their target's *content* copied (replicating the link verbatim would point back at the original tree and re-couple parallel tasks to the shared dir); links resolving **outside** the source tree (absolute or `..` escapes) are skipped with a stderr warning
- Isolation setup failures (git missing / not a repo / temp dir unwritable) are reported as `setup_error` (deterministic, not retried)
- git-mode orphan worktrees: in the extreme case (SIGKILL) `git worktree prune` reclaims them in the source repo

Task fragments may **only override** `task` / `system_prompt` / `model` / `max_tokens` / `limits`. Security-relevant fields
(`provider` / `base_url` / `api_key_env` / `workdir` / `tools` / `restrict_to_workdir` / `mcp_servers`) may not be
overridden — any attempt marks the task `bad_task` and it is not executed.

### `--retries N`: retrying transient failures

Only **transient** failures are retried (`stop_reason == "error"`: network hiccups, 429, 5xx, …) with
exponential backoff `1/2/4/8/16/32s` (capped at 32s). Deterministic failures are **not retried** — a rerun
cannot change them:

| stop_reason | meaning | retried |
|---|---|---|
| `error` | transient provider/network error (connection refused, 429, 5xx, request timeout) | ✅ |
| `request_error` | deterministic request rejection (401/403/404/400 …; resending hits the same wall) | ❌ |
| `context_overflow` | context overflow that forced compression could not rescue (deterministic) | ❌ |
| `bad_task` | invalid task fragment (forbidden field / missing task / JSON error) | ❌ |
| `max_turns_reached` / `max_tokens` | a model limit was reached | ❌ |
| `budget_exceeded` | cumulative tokens hit `limits.max_total_tokens` | ❌ |
| `stuck_loop` | spinning guardrail: the same call with identical arguments and identical results 6 times in a row (the 3rd occurrence attaches a course-correction hint to the tool_result first, granting one self-healing chance; progress or changed args reset the counter) | ❌ |
| `panic` | the task panicked in a worker and was caught into a failure entry (below) | ❌ |
| `end_turn` | normal completion | ❌ (already succeeded) |

**Retry semantics = the whole task replays from scratch**: every retry builds a fresh Agent and reruns the
entire task; side effects from the previous round (written files, sent requests, …) are **not rolled back
and not continued** — the bill keeps honest count, but non-idempotent tools will run again. For tasks with
side effects ("append to a file", "place an order"), either make them idempotent or do not enable `--retries`.

`panic` only appears in debug / CI builds (the worker catches it via `catch_unwind` and the batch still
completes); release builds are `panic="abort"` — the process exits on the spot, no report.

`report.json`'s `elapsed_secs` includes retries and backoff.

### `--failed`: re-run only failed tasks

Reads the tasks with `success == false` from the previous `--report` and re-runs only those; **historical
successes that were not re-run are kept verbatim** and merged back into the full report. After a `--failed`
round, `report.json` still reflects the current state of *all* tasks — re-run tasks that now pass turn
green, still-failing ones stay red, and CI reads a full-set `total/succeeded`, not a subset.

```sh
# first full round
tiny-agent --config examples/harness.json --batch examples/tasks --report report.json
# after some fixes, re-run only failures (+ 2 retries for transient errors)
tiny-agent --config examples/harness.json --batch examples/tasks --failed --retries 2 --report report.json
```

### Single-run transient retries

`--retries N` also works in **single headless runs**: when `agent.run` returns a transient `Err` (network
hiccup, 429, 5xx), it retries in place with exponential backoff (1/2/4/8/16/32s capped), up to N extra
attempts; deterministic failures are **not retried** — `end_turn` / `max_turns` / `cancelled` / `timeout` /
`budget_exceeded` / `stuck_loop` are `Ok`, while `context_overflow` and `request_error` (deterministic
rejections like 401/403/404/400) end with exit 1 immediately. A request failing after the run deadline
(`limits.timeout_secs`) is attributed to `timeout` (exit 124) and does not enter the retry ladder. As in
batch, a retry is a **full task replay**: prior tool side effects are not rolled back (the bill keeps honest
count); side-effecting tasks must ensure their own idempotency. Exit codes are unchanged from the
no-retry behavior (0 success / 1 failure), and `result.json`'s `attempts` field records how many runs
actually happened (`>1` means a retry occurred). In `--json` mode each retry is preceded by a line
`{"type":"retry","attempt":N,"max":M,"backoff_secs":B,"error":"..."}` for pipe consumers.

```sh
# single run: auto-backoff-retry 3 times on provider 5xx
tiny-agent --config examples/harness.json --task "run the tests" --retries 3 --output result.json
```

## Session persistence

- `--session <path>` picks the session file (default `./.tinyagent-session.json`), atomically saved after every turn
- `--session none` disables persistence — nothing read, nothing written; a one-shot headless run leaves no file in cwd (mutually exclusive with `--resume`)
- `--resume` loads the history and continues; the config's `task` counts as consumed, append new instructions via `--task`
- Completed turns survive crashes; applies to TUI mode as well
- TUI `--resume` **transcribes the history into the native scrollback** (user/assistant messages, tool calls and results rebuilt as single-line views paired by id, `· ` system lines marked "resumed"); images are not transcribed; a session that stopped mid-tool_result resumes and auto-continues, with the last task text immediately visible

## TUI chat mode

Requires building with `--features tui`. `--tui` enters an **inline/streaming** rendering mode (the same
family as Claude Code / Codex): finalized conversation content is **committed into the terminal's native
scrollback**, and only a small fixed-height live region (streaming area + input box + status line) at the
bottom of the screen redraws each frame — so **the terminal's native text selection / scrolling / search
operate directly on the real conversation**, and the old full-screen-redraw problem of "select a range,
content scrolls, the selection drifts" is gone. **Input mode** is the default:

| key | action |
|---|---|
| `Enter` | send (queued while a turn is running; sent automatically when it ends; picks the completion-popup selection when open — does not send); with a trailing `\` it becomes a newline (line continuation, bash semantics) |
| `Alt+Enter` / `Shift+Enter` | insert a newline (multi-line) |
| `Backspace` | delete the previous char (merging the previous line across lines) |
| `Ctrl-D` | delete the next char (merging the next line); empty input while idle → quit |
| `←`/`→`/`Home`/`End` | move within the line |
| `Alt+←`/`Alt+→` | move by word (whitespace-split; a CJK run counts as one word) |
| `Ctrl-W` | delete the previous word (degrades to merging the previous line at line start) |
| `Ctrl-U` / `Ctrl-K` | delete to line start / line end |
| `Ctrl-A` / `Ctrl-E` | go to line start / line end |
| `↑`/`↓` | recall previous/next history entry in single-line input (including empty) or while browsing history; move across lines in multi-line input (moves the popup selection when the completion popup is open) |
| `Alt+↑`/`Alt+↓` | page through input history (available in any input state) |
| `Ctrl-C` | busy → cancel the current turn (queued messages are discarded too); idle → double-press to quit (second press within 2s; the first only prints a hint) |
| `Esc` | busy → cancel the current turn; idle → clear the input box (only closes the popup when it is open). Quit via `/quit`, `/exit`, or idle double `Ctrl-C` |

While running, the input box stays editable: keep typing; `Enter` queues the message — the live region shows
a dim `❯ … · queued` placeholder row and the input title shows the queue length; when the current turn ends,
the oldest placeholder becomes the real message and is sent automatically; `Ctrl-C` cancels the turn **and
discards the queued messages with it**; `/clear` and `/context` are disabled while running (`/context`
needs to query the worker thread that owns the agent), the other slash commands (`/export`, `/out`,
`/model`, `/limits`, `/tools`, `/system`, `/help`, plus `/quit` / `/exit`) work as usual.

**Native select / scroll / search**: the finalized conversation (your messages, assistant replies, tool
calls and results, system lines) lives in the terminal scrollback as real terminal text — mouse-drag
selects the original text, copying goes through the terminal itself (on macOS select-to-copy just works);
wheel / `Shift`+wheel / `PageUp`/`PageDown` / `Cmd-F`, `Ctrl-F` search are all terminal-native abilities,
and what you scroll to is the real finalized conversation above. The app no longer hosts scrolling or
selection, so the selection-drift problem of the full-screen redraw model cannot exist here. An in-flight
streaming reply stays in the bottom live region (with a `▌` cursor). **Incremental finalization for long
replies**: once in-flight content outgrows the live region's height, completed paragraphs are **committed
to the scrollback early**; an extremely long single paragraph with no paragraph boundary to cut (the LLM
staple "one paragraph, one line" prose) is split by **display lines**, content scrolls up continuously line
by line (same look as Claude Code), and the live region keeps only the last two or three generating lines
(thinking blocks behave the same; early-committed lines are never re-laid-out). An empty conversation (or
after `/clear`) shows a dim key cheat-sheet (`/help`, completion, native scroll / select / copy).
Conversation text is capped at 120 columns and left-aligned on wide screens; roles are distinguished by
inline icons — your messages start with a cyan `❯ `, the assistant's with a green `● ` (no separate title
line; one line saved per message); tool calls and results are indented under the assistant, each a single
one-line preview (tool calls show a compact argument summary, tool results an output preview); failed
results with ≤20 lines of output auto-expand so errors are immediately visible, longer output stays
collapsed; to expand a successful result use `/out` to reprint it in full. The input box and status line
use a near-full-width layout; while running, the input title shows a spinner, the current activity
(thinking / writing / → tool name), turn elapsed time, the cancel hint, and the queue length (on too-narrow
terminals the activity name drops first, keeping time and cancel hint), and the whole input border changes
color with run state; idle the title is ` Ask TinyAgent `.
At the start of a turn with nothing visible yet, the live region shows a transient `● …` placeholder
(synthesized during render, never enters conversation history); it disappears at the first text or tool
call. A reasoning model's thinking renders as its own dim italic `✻` block — no markdown, visually
separate from the reply body, with a `▌` cursor while streaming and the input title showing thinking; once
the reply starts, the ✻ block finalizes into the scrollback and the reply begins as a normal block (a
sub-agent's thinking carries a `[name]` tag). `/export` includes thinking as `✻ Thinking` sections.
The status line's left side keeps only the model, overall run state (running / idle, no timer), context
occupancy, and cumulative tokens (e.g. `1.2k in · 0.8k out`); the right side right-aligns the current
directory's full path (instantly tells apart multiple parallel windows; truncated on the left with an `…`
prefix when too long, hidden entirely if fewer than 12 columns would remain — it is the first to go on
narrow screens). Context occupancy renders as `ctx 54.7k`; once the model's context window is set in
`limits.max_context_tokens` it becomes `ctx 21% (54.7k/256k)`, turning yellow at ≥60% and red at ≥80%.
After the agent has used `todo`, the status bar adds a `plan done/total` segment (e.g. `plan 2/5`, green
when all complete, yellow in progress), reflecting task-plan progress live — the full checklist stays in
the most recent `todo` tool-result line. When the terminal narrows, the status bar drops tokens first,
then plan; the context segment lasts longest, down to just model and run state. On exit (`/quit`,
`/exit`, or idle double `Ctrl-C`) with messages still queued, the count dropped is reported on stderr
after the terminal is restored. A turn that ends normally (end_turn / stop) no longer prints a
`[done: …]` line; only abnormal stops (max_tokens / length / error …) show a warning. ANSI color
sequences in tool output (e.g. the red/green of git diff) render colored as-is; other control sequences
are stripped; `/export` writes plain text without escapes.
Anything starting with `/` is a slash command (not sent to the model). While you are still typing the
command name (no space yet), a completion popup floats above the input: filtered by prefix
(case-sensitive), `↑`/`↓` move the selection (wrapping), `Tab`/`Enter` complete the selected command into
the input box (`/export` appends a trailing space for the path; completion only fills in — one more
`Enter` executes, guarding against accidental `/clear`), `Esc` closes just the popup. Typing a space
enters the argument phase; deleting the `/` or emptying the matches closes the popup automatically; mouse
clicking is not supported yet; available while running too (commands execute immediately, messages still
queue):

| command | effect |
|---|---|
| `/clear` | clear conversation history (not added to input history) |
| `/export [path]` | export the conversation to a file (no path → copy to clipboard) |
| `/out [n]` | reprint the n-th newest tool result in full (default the latest; error results expand too) |
| `/model` `/limits` `/tools` `/system` | read-only views of the current config |
| `/context` | context composition report (estimated tokens: 3 system parts / 4 tool classes / message typing + calibration state and window headroom) |
| `/help` | command list |

Cancellation: `Ctrl-C` takes effect between streaming chunks (≤200ms); after cancelling you can send the
next message — the kernel `run()` never clears the stop flag (level-triggered contract; the embedder
clears at the acceptance point), and the TUI clears the previous round's residual stop automatically each
time it submits new input.

Multi-line paste: with bracketed paste enabled in the terminal, pasting a multi-line block is treated as
**one** message — newlines never trigger an early send; just paste, no Alt+Enter needed. For manual
multi-line input, besides `Alt+Enter` / `Shift+Enter`, you can end the line with `\` then press `Enter`
(line continuation) — when the terminal reports no modifier keys (e.g. macOS Terminal.app) this is the
only manual multi-line entry.

Light-terminal support: `TINYAGENT_TUI_THEME=light` starts the TUI with dim text (system lines /
previews / status segments) switched to a darker mid-gray (readable on both light and dark backgrounds),
and inline code recolored from white to cyan (white is near-invisible on light). Unset or any other value
keeps the dark theme; there is no auto-detection (terminal background reporting is unreliable) — set the
variable explicitly for a light theme.

## MCP client

Declare MCP servers in the config; their tools register as `mcp_<server>_<tool>`:

```json
"mcp_servers": [
  {"name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
   "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "optional; env vars injected into the stdio child"}},
  {"name": "remote", "url": "http://host:3000/mcp", "transport": "auto",
   "headers": {"Authorization": "Bearer xxx"}}
]
```

stdio servers accept injected environment variables (`env`, layered on top of the host environment;
config wins on collision) — GUI apps launched from Finder/Dock have no shell environment, so servers
that rely on env-passed credentials (GitHub MCP's `GITHUB_PERSONAL_ACCESS_TOKEN`, database connection
strings, every kind of `XXX_API_KEY`) must go through this. `env` applies to stdio only; HTTP transports
ignore it. Mainstream community MCP configs (Claude Desktop / Code, VS Code) carry this field — copy
them over and they work.

Three transports (`command` or `url`, pick one):

Tool names are registered for the model as `mcp_<server>_<tool>`; characters outside `[A-Za-z0-9_]`
(spaces / dots / symbols) are sanitized to `_` and collapsed so the name is always an identifier the
model can reliably produce; **the wire name used for dispatch stays verbatim** (`tools/call` still uses
the server's original tool name), so sanitization only affects the name the model sees. If sanitization
collides two tools (server `a`+tool `b.c` vs `a.b`+`c` both yielding `mcp_a_b_c`), startup errors out
rather than silently overwriting.

| transport | notes |
|---|---|
| **stdio** | `command` + `args`; spawns a local child (NDJSON / JSON-RPC 2.0) |
| **http** | Streamable HTTP (the 2025-03-26 current standard): single-endpoint POST, JSON / SSE responses, Mcp-Session-Id; 202 async responses are recovered via the always-on GET stream (server→client push). Each read on the GET stream carries a 30s round cap; a healthy stream silently rotates reconnects per round; teardown joins with a bounded wait (≤2s), so residual reader threads and TCP connections self-reclaim within a round (no longer dependent on process exit) |
| **sse** | legacy HTTP+SSE (protocol deprecated 2024-11-05): GET for the event stream + endpoint POST. Same 30s round cap + bounded teardown reclamation as http; long-lived batch jobs should still prefer stdio / http |
| **auto** (default) | probe Streamable HTTP first, fall back to legacy SSE on failure |

Unsupported corners: server-initiated requests (sampling, roots/list, … anything with a `method`) are
dropped — this client only consumes response frames. `examples/mcp_echo.py` (stdio) and
`examples/mcp_http_echo.py` (both /mcp and /sse endpoints) are zero-dependency test servers and also
reference protocol implementations. The built-in tool whitelist (`tools`) and MCP tools don't interfere
with each other.

### Runtime hot add/remove (library API)

The config freezes the server set at startup; library hosts (GUIs, long-running services) may add/remove
**between turns** without rebuilding the Agent (a rebuild would redo every surviving server's handshake):

```rust
use tiny_agent::{build_agent_with, McpServerConfig};

// the user just enabled a server in settings:
let added = agent.add_mcp_server(McpServerConfig {
    name: "web".into(),
    command: Some("duckduckgo-mcp-server".into()),
    args: vec![],
    url: None, transport: None, headers: Default::default(), env: Default::default(),
})?;
// added == ["mcp_web_search", "mcp_web_fetch_content"]; effective on the next request

let names = agent.tool_names();          // all current tool names (built-in + MCP + skill-injected)
let removed = agent.remove_mcp_server("web")?;   // remove by CONFIG name; returns the number of tools removed
```

Semantic notes:

- **Name collision is a recoverable error**: if the new server's sanitized names collide with any existing tool (or within the new server) → `Err`, and the just-spawned child is killed immediately (rollback); the Agent is exactly as before the call — no partial application. The same collision at startup errors during build.
- **No printing**: unlike the startup `mcp: … ready` log, hot add/remove prints nothing (hosts may be in raw-mode TUIs where printing corrupts the display); check the return values.
- **`task` sub-agents don't see it**: the `task` tool holds the construction-time MCP snapshot; hot-added servers are invisible to any sub-agent spawned at any point in this Agent's lifetime (sharing that slot is left for a later version). Removing a server still held by a sub-agent's snapshot keeps the connection alive until the last holder releases it.
- **The `run_code` guest registry likewise doesn't see it** (`code` feature): the in-sandbox tool registry is built lazily, once, from the construction-time snapshot (cache shared across clones) — the same documented boundary as `task` sub-agents: hot-added servers are not visible in JS; hot-removed servers remain callable by an already-built registry (the connection lives until the registry drops; in the worst case at exit it waits out one MCP call timeout and the Drop kills the process group — no orphans). Hosts needing strict sync between the sandbox and hot add/remove should rebuild the Agent instead.
- **remove is by config name**: `remove_mcp_server("web")` takes `McpServerConfig.name`, not the sanitized `mcp_web_search` display name; a server with no tools yields `Err` (a typo does not silently no-op). stdio children are killed synchronously (process group) when the last holder drops — no orphans.
- **shutdown interlock**: if `set_shutdown` was called (session-end teardown), hot-added tools attach to the same reaper — a server added right before exit is not left behind.
- The same `&mut self` sequential contract: run holds the borrow, "call between turns" is enforced by the borrow checker (see the hot-update section of the [library API](docs/lib_api.md)).

### Your first TinyAgent MCP tool

To give the agent a custom capability, MCP is the "no core changes, no compiling" route — any process in
any language that speaks JSON-RPC 2.0 over stdio is a tool server. Minimal example (Python,
zero-dependency; the full file is `examples/mcp_echo.py`):

```python
import json, sys

TOOLS = [{"name": "echo", "description": "Echo back the given text.",
          "inputSchema": {"type": "object",
                          "properties": {"text": {"type": "string"}}, "required": ["text"]}}]

def reply(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method, msg_id = msg.get("method"), msg.get("id")
    if method == "initialize":
        reply(msg_id, {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}},
                       "serverInfo": {"name": "mcp-echo", "version": "0.1.0"}})
    elif method == "tools/list":
        reply(msg_id, {"tools": TOOLS})
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        reply(msg_id, {"content": [{"type": "text", "text": f"echo: {args.get('text', '')}"}],
                       "isError": False})
```

Save as `my_server.py`, declare it in the config and it's in — the tool registers for the model as
`mcp_echo_echo`:

```json
"mcp_servers": [
  {"name": "echo", "command": "python3", "args": ["my_server.py"]}
]
```

Three protocol essentials: `initialize` must answer `protocolVersion` + `serverInfo`; `tools/list`
returns names/descriptions/inputSchemas; `tools/call` returns a `content` array (`type: "text"`) +
`isError`. Lines without an `id` are notifications — ignore them. Need credentials? add `env` injection
(above).

### Web search / fetch (Web Search MCP)

The easiest way to get the agent online is a search-type MCP server — no core changes, just a config
entry. **DuckDuckGo MCP (`duckduckgo-mcp-server`) is recommended**: free, no API key, good search
quality in both English and Chinese; registers two tools:

- `mcp_web_search(query, max_results, region)` — search; returns titles / URLs / snippets
- `mcp_web_fetch_content(url, start_index, max_length)` — fetch page text (navigation stripped, pagination supported)

```sh
# one-time install (a persistent command; bare uvx stalls on PyPI index resolution in some regions, an installed tool cold-starts in ~2s)
uv tool install duckduckgo-mcp-server        # in constrained regions: UV_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
```

```json
"mcp_servers": [
  {"name": "web", "command": "duckduckgo-mcp-server", "args": []}
]
```

The tools register as `mcp_web_search` / `mcp_web_fetch_content` (`mcp_<server>_<tool>`). A complete
runnable example is `examples/web-search.json`:

```sh
tiny-agent --config examples/web-search.json --task "search for Anthropic's recently released models; give names and release dates"
```

> **Alternative: SearXNG MCP** (`ihor-sokoliuk/mcp-searxng`) — fully self-hosted index, no third-party
> scraping, no anti-bot / privacy concerns; but you must first self-host a SearXNG instance + the MCP
> bridge (two container layers), so deployment costs more. Good for production / privacy setups;
> day-to-day out-of-the-box, DuckDuckGo is fine.

## Configuration (`config.json`)

```json
{
  "provider": "anthropic | openai",
  "base_url": "optional; defaults to the official endpoint; in openai mode may point at a local vLLM/Ollama",
  "api_key_env": "ANTHROPIC_API_KEY",
  "model": "claude-haiku-4-5-20251001",
  "max_tokens": 8192,
  "system_prompt": "optional system prompt (a general agentic discipline reference: examples/prompts/agentic.md)",
  "task": "the task to execute in headless mode (overridable by --task)",
  "workdir": "working directory for tool operations; defaults to the current directory",
  "restrict_to_workdir": false,
  "inject_environment": true,
  "tools": ["shell", "read_file", "write_file", "edit_file", "list_dir", "load_image"],
  "mcp_servers": [],
  "code": {"enabled": false},
  "reasoning_effort": "optional; reasoning-effort string passed through verbatim (none/low/medium/high/xhigh/max), not validated",
  "limits": {
    "max_turns": 30,
    "timeout_secs": 600,
    "max_output_chars": 30000,
    "compress_threshold_chars": 400000,
    "keep_recent_messages": 8,
    "max_context_tokens": 0,
    "max_total_tokens": 0,
    "stream_idle_secs": 30,
    "shell_timeout_secs": 120,
    "subagent_max_turns": 25
  }
}
```

The API key is read from the environment variable named by `api_key_env` — it never goes into the config
file.

`inject_environment` (default `true`): when the agent is constructed, a block of runtime environment
facts is prepended to the **very front** of the system prompt:

```
# Environment
- OS: macos
- Shell: sh -c
- Working directory: /abs/path
```

so the model knows which platform it is on and issues compatible commands (knowing it's `cmd /C` rather
than bash, it won't write `$((...))` — syntax that **silently no-ops with exit 0** under cmd, looking
like success while nothing ran). **Only the three facts OS / Shell / working directory are injected, no
hardcoded syntax examples** — the facts come from the runtime (`std::env::consts::OS`) and the config,
correct automatically across platforms; Shell is the exact form the `shell` tool invokes (unix `sh -c`,
windows `cmd /C`); the working directory is the resolved `workdir`. The injection point is single-sourced
in `Agent::new` (covering the main agent and `task` sub-agents alike — sub-agents also run shell and need
the platform facts); a caller-supplied `system_prompt` is preserved in full after the facts block. Set
`false` to disable. Batch task fragments may override it (a soft field, same class as `reasoning_effort`).

`limits.stream_idle_secs` (default 30): the longest silence allowed between two chunks of a streaming
response; beyond it the connection is judged half-open and aborted. Ordinary chat models chunk at
sub-second intervals — 30s is generous; **deep-reasoning models** (DeepSeek-R1, the o-series, …) may sit
silent for minutes before the first token and need this raised (e.g. 180) to avoid being killed —
`limits.timeout_secs` remains the **per-request-level** soft cap (bounding each turn's chat requests and
the turn boundary), so nothing hangs forever because of it.

`limits.timeout_secs` (default `0` = unlimited): the wall-clock **soft cap** for the whole run — it
bounds each turn's chat requests (per-request cap = remaining time) and the turn boundary (no new turn
starts); a `task` sub-agent inherits the parent's **remaining** time (it never gets a full second
helping of timeout). **Not** covered: tool executions have their own caps (`shell_timeout_secs`, 120s
per MCP call); a tool already running in the turn that blows the cap runs to completion (preserving
tool_use/tool_result pairing); custom-provider callbacks in the Python/Node bindings are synchronous
host calls that cannot be force-interrupted (the Node side's wait cap tightens with the remaining
budget; the Python side relies on host discipline). For a hard wall clock use an outer `timeout(1)` /
container.

`limits.shell_timeout_secs` (default 120): the wall-clock cap of a single `shell` command; on timeout
the whole process group is killed and the result records `[command timed out after Ns and was killed]`.
Builds / installs in eval tasks (`cargo build`, `npm install`, training scripts) routinely exceed 120s —
the task gets axed mid-run, lands in the bill as an error, and it looks like the model failed when the
harness did; raise it for such tasks (or set `0` = no per-command cap; interactive cancel and the
run-level `timeout_secs` backstop still apply). Note it is a **cost knob, not a security boundary**: it
is also the only `limits` field batch fragments may override **in both directions** (loosen or tighten)
— a task that wants to burn time can burn it with ten thousand fast commands anyway; the real gates are
`timeout_secs` (tighten-only) and `max_total_tokens`.

> **Per-request backstop when `timeout_secs: 0` (no timeout)**: even with the run-level timeout off, a
> single chat request still carries a **10-minute** total cap (`STREAM_BACKSTOP`). This is a **leak
> backstop**, not a feature limit — streaming reader threads can block inside reqwest's `read()` unable
> to observe the cancel flag, and without any per-request cap a single half-open connection would pin
> that thread and its TCP connection until process exit. The cap bounds **one request**, never the
> **whole run** (a run is many requests), so "no timeout" still means the run may be unboundedly long —
> it just can't get stuck forever on one request. With `timeout_secs > 0` the backstop is inactive; the
> per-request cap derives from the remaining budget instead.

`limits.max_total_tokens` (default `0` = unlimited): the token budget cap for the whole run (input +
output + cache writes + cache reads, cumulated across all turns / sub-agents / compression calls). When
the total is exceeded the run stops with `stop_reason: "budget_exceeded"` (exit 125) — no new turn
starts, same **turn-boundary** semantics as `timeout_secs`: the turn that pushed the total over the cap
runs to completion (tools execute, the assistant message enters history); the cap only blocks the
**next** turn, so the transcript and tool_use/tool_result pairing stay intact. Use it in eval / CI to
gate a "runaway wallet": usage accumulation already includes sub-agents, compression, and failed turns
(honest costs — failures bill too); this gate just adds a breaker on top. Batch fragments may set it
**tighten-only** (a smaller value); `0` means "no cap" (loosest) and a fragment's `0` is ignored.
**The budget is shared, enforced, between parent and sub-agents**: the gate reads the run-level shared
spend counter — the direct chat spend (turn chats + compression summaries) of the parent and every
in-flight `task` sub-agent accumulates into the same counter in real time, and both the parent and each
sub-agent read it at their own turn boundaries; parallel sub-agents trip it **collectively** (no more of
each burning a full budget and finding out at join). The granularity is still the **turn boundary**:
sibling sub-agents already started in parallel within the same turn don't see each other's later spend
in that turn, and the turn that blows the cap completes as usual; the budget is a "breaker", not
"pre-provisioned sharding" — for strict cost control still leave headroom or drop `task` from the
config's `tools`.

`limits.subagent_max_turns` (default 25): the turn ceiling for `task` sub-agents; the effective value is
`min(parent max_turns, this)` — a sub-agent is a **bounded side quest**; without a ceiling one stuck
sub-agent would burn the parent's loop budget until the parent's own `max_turns` runs out (the parent's
run remains the real wall-clock guardrail via the shared cancel flag). `0` = **no separate ceiling**
(sub-agents inherit the parent's `max_turns` verbatim) — the loosest value, so batch fragments may only
**tighten** it (smaller); a fragment's `0` is ignored and loosening is refused by the `min`. A
sub-agent's turns are **never higher** than the parent's (this value only lowers). GUI / long-workflow
hosts that raise the parent's `max_turns` into the thousands should raise this too, or sub-agents get
pinched back to the default 25.

`reasoning_effort` (optional string, unset by default): the reasoning-effort level, **passed through to
the provider verbatim as a string, with no enum validation** — every vendor's tiers are fragmented and
shift between model generations; TinyAgent keeps no mapping table, and unknown values are the
provider/server's to reject. Value references: OpenAI `none`/`low`/`medium`/`high`/`xhigh`/`max`
(expanding with model generations), Anthropic `low`/`medium`/`high`/`max` (Claude 4.6+ adaptive
thinking; from Opus 4.7 the old `budget_tokens` is gone), DeepSeek `low`/`high`/`xhigh`/`max`, GLM
toggle-only (`enabled`/`disabled`). Mapping: the OpenAI family sends the `reasoning_effort` field
directly (old gateways 400 on it; `body_fix_for_400` auto-drops and retries — same ladder as
`max_tokens`/`stream_options`); Anthropic sends `thinking: {"type": "adaptive"}` +
`output_config: {"effort": "..."}` (effort lives in a **separate top-level `output_config` object** —
not inside thinking and not a bare top-level field; bare top-level fields are silently ignored, the
single most common trap in that API), and `"none"` sends `{"type": "disabled"}`.

**Thinking exposure**: a reasoning model's thinking streams out as `ThinkingDelta` events (OpenAI family
`delta.reasoning_content`, Anthropic `thinking` content blocks); under `--json` that's
`{"type":"thinking","delta":...}`; it **never enters the conversation history or context replay** (no
API requires replaying it either — it exists purely as an event for UIs). A GUI (e.g. TinyWorker)
renders collapsible "thinking" sections from it; headless terminals write thinking to **stderr** (not
polluting stdout's final reply / `summary`); the TUI renders it as the **separate dim italic `✻`
block** (no markdown, visually separate from the reply body; the input title shows thinking while it
streams, then it finalizes into the scrollback with the reply). Batch fragments may override it (a soft
field, same class as `model`/`max_tokens`; not in `forbidden_batch_override`).

### Context compression

In long tasks the message history keeps growing until it hits the model's context limit. Compression is
a **layered funnel** — zero-cost pruning first, then LLM summarization; the three trigger paths share
the last two layers:

0. **Pruning (the zero-cost gate before any LLM summary)** — **old tool results** outside the
   protection window (`keep_recent_messages`) are the bulk of context (measured 80%+): an old
   `tool_result` over 2000 chars is replaced by a one-line placeholder
   (`[pruned tool output, was N chars]`, `tool_use_id`/error flags kept, image payloads dropped, todo
   results exempt); pairing stays intact, and the model can re-run the tool when it needs details. A
   release under 20000 chars does not rewrite (avoiding prompt-cache-prefix breaks for a small gain).
   A preflight over threshold prunes and re-estimates first — often that alone suffices and **not a
   single summary call is sent**; the overflow backstop also prunes and retries before forced
   summarization. Prune events `[context pruned: N messages …, M chars freed]` surface separately
   (`--json`: `{"type":"pruned",…}`); the pruned original messages go to the session archive (below).

1. **Preflight** — before each turn the request size is estimated; over threshold, summarize first and
   send with the shorter history. The threshold is one of two:
   - `max_context_tokens` set (>0): compress when the token estimate reaches ~85% of the window
     (reserving this turn's `max_tokens` output besides). The estimate **anchors on the provider's
     measured value** when available: the previous response's reported prompt tokens (input + cache
     write + cache read) serve as the baseline, and only messages added since are incrementally
     estimated at ≈4 chars/token — this corrects pure-character heuristics underestimating Chinese 2–3x
     and triggering preflight too late. Without an anchor (first turn / resumed session / history
     rewritten by pruning or folding) it falls back to a full-history estimate. This is "compress by
     the model's real window" — recommended. The same anchor calibration also drives
     `context_breakdown()`'s `calibration` (see the [library API doc](docs/lib_api.md)); use it to
     watch composition change across a compression.
   - unset (`0`, default): fall back to the `compress_threshold_chars` character heuristic (default
     400k chars ≈100k tokens; `0` = off).
2. **Overflow backstop** — **always on, independent of the config above**. When the provider returns a
   "context too long" class error (`context_length_exceeded` / `prompt is too long` / `maximum context
   length` …, substring-matched against the error body): prune and retry first; still overflowing,
   force one summary compression and **retry that turn once**; if a small window still overflows after
   folding (the retained tail alone exceeds the window), a last retry prunes with a **shrinking
   protection window** (halved → keep only the last message); only when the recovery budget is
   exhausted does it surface the error. So even when the preflight estimate is off, or tool schemas /
   system prompt dominate, the agent self-heals instead of failing the turn. The summary request itself
   retries **transient errors** (network jitter / 5xx) once with ~1s backoff — one dropped packet no
   longer wastes the run's compression.

3. **Manual (host-initiated)** — library users call `Agent::compact()` / `compact_with(events,
   Some("focus …"))` between turns (see the [library API](docs/lib_api.md); the Python/Node bindings
   have `compact(instructions?, on_event?)`): a GUI whose context gauge reads 80%+ compresses on
   click, waiting for neither threshold nor overflow; a focus instruction steers the summary (e.g.
   "keep API changes front and center"). The summary's usage returns to the host for bookkeeping and
   does not enter the next run's bill.

Summary quality: the model writes mid-history as a **structured rolling summary** (goal / progress /
decisions / changed files / errors & fixes / next steps; key paths / identifiers / error texts are
required **verbatim**, suppressing cumulative drift across multiple compressions), folded into the first
task message, keeping only the task + the most recent `keep_recent_messages` originals; subsequent
compressions **update** that summary instead of re-summarizing the whole prefix. An oversized prefix is
summarized in **chunks** along message boundaries and merged, holding the per-call compression ratio
near ~6:1 and preserving details like paths/errors. Cut points land on assistant-message boundaries so
tool_use/tool_result pairing is never severed; a compression failure warns once and disables further
compression for this run (the task continues; no per-turn retry spam).

**Post-fold fidelity** (aligned with Claude Code / Codex post-compression recovery): two structured
items ride back into the head along with the summary — (a) the **todo list**: the `todo` tool has no
read mode, and the model's only view of its plan is the rendered list in the most recent todo result;
when folding would swallow it, it is restored from the latest call (not re-injected while the tail
still holds an active list); (b) **intermediate user messages verbatim**: user Text blocks inside the
folded span are merged into the head as `[用户消息原文保留 / User message preserved verbatim]`
blocks, character-for-character (2000 chars per message cap, 12000 chars total FIFO) — user
instructions are never paraphrased through the summary. Supervisor notes
(`[监督者旁注 / Supervisor note]` blocks, see the [library API](docs/lib_api.md)) are deliberately
**excluded** from (b): they are time-sensitive control signals that go stale within a few turns, so
their gist rides the summary prompt instead of verbatim preservation. Compression events in the run log and TUI system lines carry the trigger-path annotation
(`[context compressed (preflight): dropped N messages, M kept]` / `(overflow)` / `(manual)`; in
`--json` NDJSON it is an extra `"trigger": "preflight"|"overflow"|"manual"` field) — at a glance
whether it was estimate-over-threshold, overflow backstop, or host-initiated.

**Non-destructive + session archive**: the **original messages** (with stable ids) that compression
folding and pruning moved out of the live history are appended to the session file's `archive` section
(4096-entry FIFO cap; `/clear` keeps the archive); hosts can read them back via
`session::load_with_archive` for undo/audit; plain `load` is unaffected and old session files are
compatible both ways (no archive key = empty archive; an empty archive omits the key). Id semantics: a
folded message's id migrates to the archive with the original (it left the live history); a pruned
message's **original** and its placeholder version in the live history **share one id** — an asset the
host attached by id still points at that message (anchors don't drift), and the archived copy is "what
it originally said" (restorable by id); an id is archived exactly once (later placeholder versions
don't re-archive); after resume, newly minted ids never reuse numbers already in the archive.

The compression summary requests' own token usage also counts into the run total
(`input_tokens` / `output_tokens`) — the cost figures match the actual calls; compression never
under-reports.

### Prompt caching

The Anthropic native provider enables prompt caching by default: every request automatically places
ephemeral breakpoints at **system prompt / tools / the last message** (within the ≤4-breakpoint cap),
so multi-turn tasks resending the whole history only bill the increment (cache write 1.25×, read 0.1×).
Breakpoints under ~1024 tokens are silently ignored by the API (no error, no charge), so always-on is
safe. The OpenAI-compatible protocol uses implicit prefix caching (no request field); hits surface in
`cache_read_input_tokens`.

Cached tokens are accounted separately: `input_tokens` covers only the **uncached** part; cache
write/read flow through `cache_creation_input_tokens` / `cache_read_input_tokens` (visible in
result.json / NDJSON / the TUI status bar). True billed input ≈ `input_tokens +
cache_creation_input_tokens × 1.25 + cache_read_input_tokens × 0.1`. Compression summary requests are
one-shot calls and carry no cache breakpoints.

## ⚠️ Security notes

**This is an agent that gives a model direct execution power — not a sandbox.** Use it accordingly
(report security issues privately per [SECURITY.md](SECURITY.md), not in public issues):

- The `shell` tool executes model-generated commands verbatim (`sh -c` / `cmd /C`) with no command
  filtering. The model can `rm -rf`, `curl ... | sh`, reach the network — everything the process can.
- File tools do not restrict paths by default: `read_file ../../etc/passwd` and
  `write_file /etc/cron.d/x` will be executed.
- **When batch-running untrusted third-party tasks in CI / eval pipelines**, always:
  1. run tiny-agent inside a container or as a restricted user;
  2. set `"restrict_to_workdir": true` — file tools then refuse paths escaping `workdir`
     (`../` and absolute paths; same for newly created files). Note it is **best-effort, not a hard
     boundary**: the check is canonicalize-based with a TOCTOU window between check and actual I/O —
     it stops the model from wandering through the wrong `../` door, not a hostile process racing
     directory changes; the hard boundary remains the container;
  3. drop `shell` from the `"tools"` whitelist when appropriate.

### Trust boundary (which inputs are trusted)

The above covers the **model output** half of the threat surface; the other half is **loaded local
files**. The following are **trusted inputs** in TinyAgent's threat model (equivalent to code itself —
don't load files of unknown provenance):

- **Config files**: `base_url` decides where the API key is sent (loading a config of unknown origin
  hands the credential named by `api_key_env` to that URL); `tools` / `mcp_servers` decide which tools
  exist and which children get spawned. Verify the provenance of `--config` / binding config paths.
- **Session files**: `--resume` replays the history verbatim into the model's context (tool_use /
  tool_result pairing included) — a tampered session is history ghost-written by an attacker, and the
  model swallows it whole. Verify integrity yourself when sessions move between machines.
- **Skill directories** (`~/.config/tinyagent/skills/` and `<workdir>/.tinyagent/skills/`): a
  SKILL.md blurb sits permanently in the system prompt, and its body/resources can be read into
  history — installing a skill installs a prompt (including scripts the model may follow).

**Runtime untrusted inputs** outside that trust surface (MCP server responses, provider SSE streams,
skill directory contents) have size-cap defenses (tools/list count & schema caps, 16MiB per SSE line,
64KiB per skill scan/read, …): a hostile server / directory can at most slow things down, not OOM the
process. Also note: the OpenAI 400 adaptive fixes (learned fixes) live only in the provider instance's
memory — rebuilt every run, never persisted, never shared across processes; a new run against the same
gateway relearns (one extra probe round-trip per 400 class) — deliberately (persisting would create a
cross-run poisoning surface; see the record-on-success semantics).

## Non-goals (explicitly out of scope)

TinyAgent positions itself as an **auditable, unattended agent execution unit** — a one-shot process
that always leaves behind a complete, honest, machine-readable bill. These directions conflict with
that positioning and are explicitly out of scope (effort goes to the moat instead):

- **No `--serve` / HTTP service mode**: long-lived, multi-tenant, authenticated is a different product
  and directly conflicts with "one-shot execution unit". Integrators wrap `--json` (NDJSON stream) +
  exit codes instead.
- **No homegrown sandbox / command filtering for `shell`**: half-baked command filtering only
  manufactures false confidence; the official answer to isolation remains **run it in a container or
  as a restricted user** (see the security notes). (`run_code` is neither an exception nor a breach:
  it is a wasm **constructive-capability** sandbox — not a filter bolted onto existing tools but a new
  execution surface where "no grant means no capability"; network / filesystem simply don't exist in
  the guest — see its section. The library's Hooks interception layer is a compile-time extension
  point for embedders to wire their own approval/policy systems, not a built-in security boundary —
  threat model in `crates/core/src/hooks.rs`.)
- **No plugin system**: tools are compile-time built-ins + MCP; no runtime loading. (Hooks and
  capability markers are the same: compile-time trait extension, no dynamic code.)
- **The TUI stays minimal**: the interaction experience tracks mainstream terminal AI tools (key
  bindings, light-theme support, result viewing — the basics work), but no structural expansion like
  full-screen hosting / mouse ownership / theming systems — it is a dogfooding / demo tool benchmarked
  against full-time-iteration Claude Code / Codex; parity is negative ROI.

### Feature litmus test

A new feature only enters the backlog if it makes **unattended execution** more **trustworthy /
auditable / cheaper / more portable**. That ruler is what deflects the "sounds cool, off-positioning"
temptations.

## Result file (`--output result.json`)

```json
{
  "schema_version": 1,
  "agent_version": "0.4.2",
  "success": true,
  "stop_reason": "end_turn",
  "model": "deepseek-chat",
  "provider": "openai",
  "turns": 2,
  "attempts": 3,
  "started_at": "2026-08-15T07:41:02Z",
  "finished_at": "2026-08-15T07:41:09Z",
  "elapsed_secs": 3.1,
  "input_tokens": 1234,
  "output_tokens": 567,
  "cache_creation_input_tokens": 300,
  "cache_read_input_tokens": 1500,
  "summary": "the agent's final text reply",
  "tool_calls": [
    { "name": "write_file", "input": {...}, "output": "wrote 42 bytes", "is_error": false, "duration_ms": 3 }
  ],
  "turn_timings": [
    { "turn": 1, "duration_ms": 2100, "tool_calls": 1 },
    { "turn": 2, "duration_ms": 950, "tool_calls": 0 }
  ],
  "compressions": [
    { "trigger": "preflight", "dropped": 8, "kept": 6, "estimated_tokens_before": 152000, "estimated_tokens_after": 41000, "measured_tokens_before": 148220 }
  ],
  "plan": [
    { "subject": "run the tests", "status": "completed" },
    { "subject": "fix failing cases", "status": "in_progress" },
    { "subject": "commit", "status": "pending" }
  ],
  "error": null
}
```

- `tool_calls[].output` is the tool's full return (`is_error` marks error results)
- **Attribution fields (`schema_version` / `agent_version` / `model` / `provider` / `started_at` /
  `finished_at`) are always written** — a bill that cannot be attributed to model / protocol / time /
  **build** is not auditable (after an `--model` A/B the two results must be distinguishable; same for
  long eval series across binary swaps — `agent_version` is the same version string as `--version`'s).
  Unlike the sparse late-additions `plan`/`meta`, this group is **purely additive** (new keys; old keys
  untouched); consumers that read by key are unaffected. `model`/`provider` come from the actually-run
  config (a batch fragment's `model` override is recorded post-override); `provider` records the
  configured protocol family (anthropic / openai) — still the declared value when a binding injects a
  custom provider callback.
- **The `schema_version` contract**: while it is `1`, **keys are only added, never removed, and
  existing keys never change type**; new keys are always optional (old consumers can ignore them and
  keep working; sparse keys like `plan`/`compressions`/`duration_ms` simply don't appear when empty);
  only breaking changes (deleting a key / changing its type) bump the version. Consume **by key** and
  skip unknown keys; don't validate positionally or by whole-structure. The batch `report.json` is the
  same (plus top-level `agent_version`).
- `plan` (optional): the task plan the agent recorded via `todo` and each item's status (the full list
  from the last call). **Present only if the agent used `todo`** — otherwise the key doesn't exist;
  result files that don't use it are byte-identical to pre-feature ones. A `task` (sub-agent) entry in
  `tool_calls[]` additionally carries an optional `meta` field (a compact trace of
  `turns`/`tools`/`tokens`/`stop_reason`), likewise present only when a sub-agent ran
- `turn_timings`: per-turn duration (ms) and that turn's tool-call count, aligned with `turns`;
  `duration_ms` is the tool's real execution time (queueing and cancel checkpoints excluded;
  placeholder records for calls never started due to cancel/stuck carry no key). `compressions`
  (optional): the context-compression audit — trigger path (`preflight`/`overflow`/`manual`),
  dropped/kept message counts, before/after token estimates (same basis as the preflight gate) and the
  most recent anchor's provider-measured value (`measured_tokens_before`, null without an anchor);
  **present only when a compression happened**. Compressions in a failed turn are audited too (error
  paths are also bill surfaces)
- `attempts` is how many agent runs actually executed (`>1` means transient retries happened — see
  "Single-run transient retries")
- `input_tokens` / `output_tokens` are the run's cumulative token usage;
  `cache_creation_input_tokens` / `cache_read_input_tokens` are the cumulative prompt-cache
  writes/reads (Anthropic reports both; OpenAI reports reads only). `input_tokens` excludes cached
  tokens; true billed input adds cache write 1.25× / read 0.1× (see Prompt caching)
- When agent.run returns `Err` (e.g. provider 503) the result file is still written:
  `{"success": false, "stop_reason": "error", "error": "..."}`; an unsurmountable context overflow
  (forced compression failed to rescue) records `stop_reason: "context_overflow"`, deterministic
  request rejections (401/403/404/400 …) record `"request_error"` (neither enters transient retry;
  exit code stays 1) — same basis as the batch report
- If writing the result file itself fails (e.g. `--output` points at a directory): a **successful**
  run degrades to exit 1 (so CI never reads a missing/stale result.json alongside a 0); an already
  failing run (cancelled/timed out/…) keeps its original exit code

Process exit codes:

| code | meaning |
|---|---|
| `0` | normal completion (`stop_reason == "end_turn"`)|
| `124` | timeout (`timeout`, following the GNU `timeout` convention)|
| `125` | budget exceeded (`budget_exceeded`, cumulative tokens hit `max_total_tokens`)|
| `130` | user cancel (`cancelled`, 128 + SIGINT)|
| `1` | anything else (`max_turns_reached` / `max_tokens` / provider errors, …)|

In headless mode `Ctrl-C` is a **graceful cancel**: the first press interrupts between streaming chunks
(≤200ms), writes a proper `result.json` (`stop_reason = cancelled`), then exits 130; a second `Ctrl-C`
exits 130 immediately (without waiting for the turn). Linux/macOS go through SIGINT, Windows through
`SetConsoleCtrlHandler` — identical behavior. **Batch mode** (`--batch`) works the same: after a first
SIGINT the in-flight tasks finish, no new tasks start (unstarted ones are marked `cancelled`), the
`--report` is merged and written, exit 130; a second `Ctrl-C` exits 130 immediately. One exception:
in-flight tasks' **MCP servers are closed immediately** (so a second-Ctrl-C hard exit doesn't orphan
the children) — an in-flight task depending on MCP tools may fail early because of it. In TUI mode
`Ctrl-C` is a plain key handled by the key table above.

`--json` mode additionally emits one NDJSON line per event (`type`: `turn` / `text` / `thinking` /
`code_log` / `tool_input` / `tool_call` / `tool_result` / `usage` / `compressed` / `pruned` / `salvage`
/ `warning` / `subagent` / `todo` / `done`, plus `retry` on retries), letting pipes track progress
before the result file lands. `tool_input` is the streaming increment of tool arguments
(`{"type":"tool_input","id","name","delta"}`, keyed by tool_use id): the model emits while it is still
constructing the call, and a GUI can render "constructing a call…". This set of `type`s is a
**contract**: the kernel's `AgentEvent::ndjson_type()` exhaustively assigns one per variant (a new
variant without an entry fails to compile), and `agent_tests` pins the names variant-by-variant —
consumers can branch on `type` and will never meet a nameless stream event.

## Built-in tools

| tool | description |
|---|---|
| `shell` | run a command in the working directory (sh -c / cmd /C); on timeout the whole process group is killed (default 120s, `limits.shell_timeout_secs` adjustable, 0=unlimited); output is read bounded and truncated per `max_output_chars`; background processes left holding the output pipe are reaped and annotated after a 2s grace period (to keep a background process running, redirect its output: `cmd >log 2>&1 &` — not holding the pipe, it is unaffected) |
| `read_file` | read a text file with line numbers; offset/limit supported; streaming read — huge files never load whole into memory; header carries `[filename: N lines · M chars]` size metadata; `format:"json"` returns structured JSON (no prefix, with totals/next_offset); `raw:true` returns a base64 byte window (byte-faithful, for the run_code sandbox to read/write binary) |
| `write_file` | rewrite a file wholesale; parent directories auto-created |
| `edit_file` | exact replacement: `old_string`→`new_string`, unique match required by default (`replace_all` for all); 0 or >1 matches error with guidance to add context; reuses the `restrict_to_workdir` sandbox |
| `list_dir` | list a directory, up to 500 entries |
| `load_image` | load a local image for the model to "see" (needs a vision model) |
| `task` | delegate a chunk of work to a **sub-agent** (own context + own turn budget); only the sub-agent's final summary returns as the result, keeping the main context lean. See the next section |
| `todo` | record/update the task plan (full-replacement semantics; the complete list is sent every time); the final plan lands in result.json's `plan` field. The model sees a readable checklist; the audit truth is the structured input |
| `run_code` | execute JS in the in-process wasm sandbox (QuickJS); code orchestrates this session's tools via `tools.call()` (optional `code` feature + the `code.enabled` runtime switch, both off by default; dedicated section below) |

## Sub-agents (the `task` tool)

`task` is a sub-agent tool: the main agent hands a focused subtask to a fresh sub-agent (its own message
history, its own turn budget); when it finishes, only the **final summary** returns as the tool result —
the main context is never bloated by the sub-agent's intermediate steps.

- **Input**: `{ "name": "identity label", "description": "short note", "prompt": "complete
  self-contained instructions" }` (`name` and `prompt` required; `description` optional,
  informational). `name` is this sub-agent's identity — every forwarded event is tagged with it.
- **Multiple calls in one turn run in parallel**: when the main agent issues several `task` calls in
  **one** assistant turn they run **in parallel** (a thread each, capped at 8 per wave via a scoped
  thread pool), their events forwarded live into the main run's stream through a bounded mpsc funnel
  (backpressure) — so multiple cooperating sub-agents produce interleaved streaming output. Results
  are still stitched back in original block order (N:N pairing intact). Leaf tools
  (`shell`/`read_file`/…) stay sequential; only `task` parallelizes, avoiding new races over
  files/processes.
- **Event visibility**: a sub-agent's streaming progress (text deltas / tool calls / tool results) is
  **forwarded live** into the main run's event stream, each wrapped with the `name` identity —
  headless `--json` output looks like `{"type":"subagent","name":..,"event":<child event>}`, and the
  TUI prefixes sub-agent output with `[name]`. With several sub-agents in parallel, tell them apart by
  `name`.
- **Honest cost accounting**: every sub-agent's token usage folds into the main run's totals and
  `usage` events / `result.json` as usual (each of the parallel ones counted).
- **Audit trace (`meta`)**: each `task` entry in `result.json` carries a `meta` field — a compact
  record of the sub-agent's turns, tools used (counted by name), token subtotal, and stop_reason.
  **Not in the model-visible tool result** (the main context only receives the summary, staying lean);
  purely for auditing parallel work. Failed sub-agents carry `meta` too (`stop_reason: "error"`) — you
  can see "collapsed at which step".
- **Inheritance**: sub-agents reuse the main agent's provider / model / working directory /
  `restrict_to_workdir` / cancel flag (Ctrl-C interrupts in-flight sub-agents too). The tool whitelist
  inherits the parent config.

**Boundaries (deliberately tight)**:

- **Parallelism is only across multiple `task` calls in one turn**: within a turn, `task` calls
  parallelize; leaf tools are sequential. Working-directory isolation (worktrees) is not done —
  sub-agents share the main working directory; the convention is that concurrent sub-agents don't
  write the same files.
- **Non-recursive**: a sub-agent's tool whitelist filters out `task` itself, so sub-agents cannot spawn
  sub-agents (no depth counting needed).
- **On by default**: `task` is in the built-in default whitelist — TUI chat or headless runs can
  delegate subtasks with zero extra config. Evals / CI that want behavior and cost locked down should
  list `tools` explicitly without `"task"`:

```json
"tools": ["shell", "read_file", "write_file", "edit_file", "list_dir"]
```

## Task plan (the `todo` tool)

`todo` lets the agent record its task plan structurally and update it as it goes, landing finally in
`result.json`'s `plan` field — putting "what the agent **intends** to do and how far along" next to
"what the agent **did**" (`tool_calls`) in one bill.

- **Full-replacement semantics**: every call sends the **complete new list** (not a delta); status is
  one of `pending` / `in_progress` / `completed`. The model sees a readable checklist
  (`☐`/`▶`/`✓` prefixes) as the tool result; **the audit truth is the structured input** — separate
  responsibilities.
- **Where `plan` comes from**: at run end, the **last non-error `todo` call's input** in the tool-call
  log is the final plan. That log is independent of the conversation history (context compression only
  rewrites `messages`, never the tool-call log), so **plan state survives compression** — the key to
  it landing reliably in `result.json`. If the model never used `todo`, the result file **has no `plan`
  key** (byte-identical to pre-feature).
- **Live progress**: each `todo` call emits `{"type":"todo","todos":[...]}` via `--json` (headless
  terminals additionally print an `[plan] done/total` line on stderr); pipes can render live progress
  from it.
- **On by default, not inherited by sub-agents**: same default whitelist as `task`; evals / CI drop it
  from `tools` to lock behavior. Sub-agents are focused short tasks — `todo` is filtered from their
  whitelist (planning is the parent's job) and their plans never enter the parent's `result.json.plan`.

```json
{ "todos": [
  { "subject": "reproduce the failure", "status": "completed" },
  { "subject": "locate the root cause", "status": "in_progress" },
  { "subject": "write the regression test", "status": "pending" }
] }
```

## run_code (sandboxed JS execution, optional `code` feature)

**Start here**: [quick start & best practices](docs/run_code_guide.md) ·
[a runnable demo](examples/run-code-demo/README.md) ·
[the built-in skill, full text](crates/core/src/tools/run_code_skill.md). What follows is the
configuration and interface reference.

`run_code` lets the model write the "needs loops / batches / multi-step data wrangling" logic as a
piece of **JS**, executed in the in-process wasm sandbox (QuickJS compiled to wasm, driven by wasmtime;
a pure-Rust host with no C toolchain) — the code orchestrates the agent's **existing** tools (file
tools, MCP tools) via the `tools.call()` callback, finishing in one call what used to take dozens of
shell + read_file round-trips; and the capability surface is **constructively confined**: the sandbox
has no network, no syscalls, no direct filesystem access — the reachable world is exactly the granted
tools + the configured paths.

**Two switches, both off by default** (trust and binary size gated independently):

1. **Compile time**: `cargo build --features code` (wasmtime + the embedded quickjs.wasm asset; musl
   static builds work too); compiled module machine code is cached on disk
   (`tinyagent/wasmtime/` under `$XDG_CACHE_HOME` or `$HOME/.cache`, so new processes don't repay the
   lazy compilation; silently degrades to no cache if the directory is unusable);
2. **Run time**: `"code": {"enabled": true}` — with the feature compiled in but the switch off,
   `run_code` is not in the tool table: invisible to the model, zero schema cost.

<!-- BEGIN CODE CONFIG SPEC (validated by sync_run_code_contract.py: defaults must match
     CodeConfig::default in crates/core/src/config.rs + the section hash is pinned in the
     manifest, guarding against one-sided drift) -->
```json
"code": {
  "enabled": true,
  "skill_injection": "on_demand",
  "precheck": false,
  "include": false,
  "call_batch": false,
  "llm": {
    "enabled": false,
    "default_model": "judge-small",
    "models": ["judge-small", "judge-large"]
  },
  "deny_paths": [".env", "secrets/**"],
  "fs_write_dirs": [],
  "max_tool_calls": 200,
  "mem_mb": 256,
  "timeout_secs": 120,
  "fuel": 5000000000,
  "max_result_chars": 60000
}
```

| field | default | semantics |
|---|---|---|
| `enabled` | `false` | the runtime switch; `run_code` is outside the `tools` whitelist semantics (the switch *is* membership — below) |
| `skill_injection` | `"on_demand"` | the built-in `run-code` skill's three-state disclosure: `"on_demand"` (default; the model loads it on demand via `read_skill`); `"eager"` (at assembly the skill body is pre-seeded into session history as a synthetic read_skill Q&A pair — for fine-tune/eval trajectory alignment; resumed history is not re-injected); `"off"` (the built-in skill is off; `read_skill` hits return an explicit "disabled by config" error; a same-named on-disk override still serves normally) |
| `precheck` | `false` | static pre-execution check: a single lexer pass detects constructs the sandbox doesn't support (`async`/`await`/`import`, undeclared `Promise`/`setTimeout`/`fetch` calls, `Date.now`/`Math.random`, `tools.call` reserved names `run_code`/`task`/`todo`); a hit refuses with correction guidance — **the code never executes, no side effects, no budget spent**. Note `__tools_list`/`__budget` are not in the detection set — they are working read-only introspection interceptors (the underlying channel of `tools.budget()`) |
| `include` | `false` | the library channel switch (code_memory M1): when on, the guest gains `include(path)` (load a registered library under `lib/`; idempotent + cycle-cut, via the `__include` reserved name) and `saveLib(path, src)` (write + host-side sha256 via `__hash_file`, registering into `lib/INDEX.md`, always `st=draft`) helpers, plus the skill's Library section; admission = `st=ok` or registered this session (clone-shared, not across sessions). Off: reserved names refuse explicitly, and the model-visible contract matches a binary without the feature. Effective at call time (resource-limit precedent); batch fragments can only turn it off |
| `call_batch` | `false` | the batch channel switch (code_llm M2, eval-gated): when on, the guest gains `tools.callBatch([{name, args}, …])` (concurrently execute a batch of read-only tool calls; via the `__call_batch` reserved name, each entry walks exactly the same gate chain as a single call; 8 per wave fan-out, array returned in submission order, a failed entry becomes an Error item without aborting the batch) plus the skill's Batch section. The read-only face = `batchable()` tools (v1 whitelist `read_file` + `llm` + MCP `readOnlyHint`; a missing annotation counts as no). Off: the reserved name errors as an unknown tool; model-visible contract matches the old binary. Effective at call time; batch fragments can only turn it off |
| `llm` | `{enabled:false, default_model:null, models:[]}` | the inner-model judgment switch (code_llm M3): the nested `llm` object has three fields — `enabled` (off by default; when on the guest gains the `llm` judgment primitive, next section), `default_model` (required when enabled: inner calls omitting the `model` parameter use it), `models` (the allow-list for explicit `model` parameters, may hold several judgment tiers; `default_model` must be in it). When enabled, a top-level tool of that name errors at assembly (the name is reserved); the registry is a build-time snapshot — `set_code_config` flips are not seen in either direction; changing `llm` after first execution emits an explicit "frozen, rebuild the agent" diag warning (no more silent success). Batch fragments cannot touch `llm.enabled`/`default_model`; `models` can only tighten (intersection) |
| `deny_paths` | `.env`, secret-type dirs, … | the deny-list for file reads/writes **through the tools** (globs: `*` one segment, `**` across segments); positioned as **accident-proofing, not adversarial**. `shell` commands bypass this layer (`cat .env` still works — next row) |
| `fs_write_dirs` | `[]` = the whole workdir writable | the write-path allow-list **through the tools** (relative to workdir; intersection semantics — can tighten to nowhere-writable). **Binds only the 4 file tools and `copy_bytes`** — `shell` (including its redirected writes) bypasses this layer and the write-side symlink refusal; the countermeasure for a hostile guest is `restrict_to_workdir` / hooks Deny / dropping `shell` from `tools` (A-8: read and write sides share the boundary; pinned tests exist) |
| `max_tool_calls` | 200 | the cap on in-sandbox tool calls (failed calls included). Grounded in AppWorld high-loop tasks: gold solutions take 91–244 calls; the old default of 50 couldn't even fit the "survey metadata first, then batch-write" two-phase play (measured: the survey phase alone hit the cap, zero real work done) |
| `timeout_secs` | 120 | wall clock; `0` = none (cancel / fuel / mem still bound it) |
| `fuel` | 5e9 | the instruction budget (pure compute loops; debug builds measured ≈11.6s/1e9, release faster). Why 5e9 by default: v2 P2 A/B measured a 2.5 MB file `readAll` whole-read (in-guest JSON parse + array build) exceeding 1e9 — first attempt died on fuel and needed a model self-rescue; 5e9 covers that and still sits under the default `timeout_secs=120` |
| `mem_mb` | 256 | the sandbox linear-memory cap |
| `max_result_chars` | 60000 | total size of return value + console logs (character basis; over the cap, logs are truncated and the return value kept) |

<!-- END CODE CONFIG SPEC -->

**The precheck's honest positioning**: `precheck` is an **efficiency/experience feature, not a security
mechanism** — a lexical scan can be obfuscated around (`globalThis["Date"]["now"]()`); security stays
with the existing layers (the capability sandbox, deterministic-stub runtime errors, reserved-name
runtime interception); a missed detection only costs a fallback to the status quo (a runtime failure) —
no security guarantee is lost. Off by default. The refusal text starts with the `run_code precheck: `
prefix and reports all hits at once (fixed inter-class order: environment syntax → determinism →
reserved tools; 20 hits capped per class). The two new fields take effect asymmetrically:
`skill_injection` is an **assembly-time** decision (a `set_code_config` hot change does not re-inject —
consistent with the snapshot-semantics precedent); `precheck` is a **call-time** decision (each
execution applies through the snapshot). With `code.enabled` on, an audit anchor is written
automatically: result.json gains a `"code"` key (the assembly-time values of
`skill_injection`/`precheck`), and NDJSON mode prints the same on the first line — so bench replays can
attribute trajectories to a disclosure/precheck configuration.

**Libraries & reuse (`code.include`, off by default)**: logic reused across tasks settles into
libraries under the workdir's `lib/` — `saveLib("lib/xxx.js", src)` writes the file + the host computes
sha256 + registers it in `lib/INDEX.md` (machine line
`<!-- ta-lib: path=… sha256=… needs=… ver=… st=draft/ok -->`, always `st=draft`);
`include("lib/xxx.js")` reads the whole file straight into global scope (idempotent, cycle-cut, 1 MiB
per library). Admission = `st=ok` (the owner flips it by hand after reviewing content) or registered
this same session with identical content (the registry is shared by tool lineage, not restored across
sessions; resume does not restore it either); on-disk content mismatching the registered hash → hard
refusal. The channel goes through the full policy chain via the `__include`/`__hash_file` reserved
names (lib/ containment + `deny_paths` + the hooks gate + audit/quota); library-validation errors carry
`e.kind="lib"`. Switch off = the helpers don't exist, reserved names refuse explicitly, and neither the
description nor the skill's library section appears (the model-visible contract matches a binary
without the feature); the channel and prelude take effect at call time, the description and skill
section at assembly time. v1 recognizes only the workdir `lib/` as the library directory; **a
deployment that tightens `fs_write_dirs` must list `lib/` in the writable set**, else every `saveLib`
write is refused (reads are unaffected); version management of libraries belongs to git. The
build/use/fix library lifecycle teaching is in the built-in `run-code` skill's "Libraries & reuse"
section (present only when `code.include` is on).

**Batch calls (`code.call_batch`, off by default)**: `tools.callBatch([{name, args}, …])` concurrently
executes a batch of **read-only** tool calls (paged fetches and per-item judgments are the ideal
shapes), entering the host through the `__call_batch` reserved name — the reserved name is only the
entrance, exempting nothing: every entry walks exactly the same sequence as a single call (entry checks
→ hooks gate → deny/fs layers → execution → after). **No transactional semantics (three sentences)**:
no atomicity; no rollback; partial execution is a normal outcome — the result array faithfully shows
which entries ran/were refused, **you must check every one**; ordering is guaranteed only inside the
guest's sequential for loops (side-effecting batches use for loops; the batch admits only
`batchable()` read-only tools — a side-effecting tool in the mix yields a `kind="batch"` error entry
for that item, one entry never aborting the batch). A failed item doesn't throw; it becomes an `Error`
instance with `e.kind`/`e.tool` (including LD-9's `e.halted`: true for latched halts — the sandbox is
stopped, only return remains; false for the `llm` token-reservation refusal — the sandbox lives on and
other routes remain). K entries bill K calls. Concurrency is capped at 8 per wave and the array returns
in submission order; ToolCall events are emitted serially in submission order during the gate phase,
ToolResult events as each entry completes. The timeout planning formula (no magic numbers):
`timeout_secs ≈ expected per-call duration × ceil(batch size / effective concurrency) + compute and
rate-limit headroom` (effective concurrency ≤8; an estimate, not a guarantee). Actual concurrency
routes by `ToolConcurrency` (LD-11): `llm` is v1's only Parallel batchable tool (true in-batch
concurrency); `read_file` and MCP `readOnlyHint` tools are Sequential — executed serially in submission
order within the batch (contract-consistent, no speedup). Switch off = the prelude doesn't expose
`callBatch`, calling the reserved name directly errors as an unknown tool, and neither the description
nor the skill's batch section appears (model-visible contract matches the old binary); the channel is
effective at call time, the description and skill section at assembly time. The teaching and traps
(`instanceof Error` discrimination; don't `JSON.stringify` failure entries) live in the built-in
`run-code` skill's "Batch calls" section (present only when `code.call_batch` is on).

**LLM judgment (`code.llm`, off by default)**: `tools.call("llm", {...})` gives the guest a
**single-shot judgment/generation primitive** (code_llm M3, LD-1…19) — no tools, no history, no
streaming; one round trip; it enters the host through the ordinary `tools.call` channel, so the hooks
gate, the event pair, the audit trace, and the call quota all apply. Parameters: `prompt` (required,
self-contained), `system`, `model` (omitted uses `default_model`; explicit must be in the `models`
allow-list), `max_tokens` (1..=8192, default 1024), `labels` (≤16 non-empty declared vocabulary words);
`temperature` is fixed at 0 (per-item judgments must be reproducible). Returns
`{text, label?, usage, model}` — on a `labels` hit, `label` is the canonical of the **declared
spelling** (output-side normalization is only trim → strip one layer of wrapping quotes/backticks →
case folding). Three failure classes each carry a structured `kind`: `llm` (parameter/allow-list/
provider failure — deliberately no retryable; the model reads the message and decides), `llm_output`
(labels miss; carries the real usage — honest cost), `budget` (the run-level token reservation was
refused — the sandbox lives on; `write_file` etc. keep working; the reservation semantics are below).
**Reserve then settle (LD-17)**: before the call, `est(prompt+system) + max_tokens + protocol headroom`
is reserved from the run-level shared budget; on completion the real usage settles it (refund the
excess, charge the shortfall; unknown usage keeps the reservation — the gate would rather over-reserve
than under-reserve), and result.json's `meta.llm` keeps a per-model ledger
(reserved/actual/known/over_reserve/known_calls/unknown_calls). **Same provider instance (LD-16)**:
inner calls go through the same provider the outer agent was constructed with (a binding host's custom
provider receives inner calls too — if it can't answer, it replies with its own honest error); each
call carries a 120s hard timeout. Switch off = no `llm` in the guest tool table (only a top-level name
collision errors), and neither the description's LLM-judgment section nor the skill's appears. An
example of cascaded judgment (feeding the result into another llm round) is in the built-in `run-code`
skill's "LLM judgment" section (present only when `code.llm.enabled` is on).

**The code-orchestration prompt (optional)**: `examples/prompts/agentic-code.md` adds a "clear rules
plus remaining scale" selection rule on top of the reference discipline, supporting
explore-then-switch and keeping large lists in variables/files. Only when `run_code` is available, set
the body after the file's first `---` as the `system_prompt`; it never loads automatically.
`agentic.md` stays as-is for no-code/legacy comparison. New descriptions and the skill flow into the
code arm with a rebuild; per the [landing report](docs/run_code_routing_report.md), pin the texts,
prompts, and binary fingerprints, and verify effect in batches. **Eval apparatus**: the E0–E3 staged
matrix for include / callBatch / llm and the four-route comparison's reproducible recipe (arm configs,
the E0′ three-question boundary-sentence apparatus `examples/eval/e0prime-skill/`, fingerprints, and
the measurement protocol) live in the [eval assembly doc](docs/code_llm_eval_assembly.md); apparatus
drift is CI-gated by `scripts/check_eval_apparatus.py`.

**When to enable (value-validation conclusions, 2026-09)**: 28 paid A/B rounds (single model,
synthetic tasks; full G0–G3 reports in `docs/code_value_reports/`) produced this guidance —

- **The shape where it pays**: tasks whose data arrives only through paginated APIs (MCP etc.), where
  `shell` is unreachable and full aggregation is required. In that shape natural run_code adoption
  yields a stable cost advantage (three-instance cost ratios 0.51×/0.55×/0.58× — one G1 instance plus
  two fresh G2 instances, wall clock 0.55×~0.97×; mechanism: 12 outer page-by-page calls collapse into
  one in-sandbox loop; the same two G2 instances re-measured under on-demand disclosure at
  0.567×/0.571× — a stable signal). **The default assembly (`skill_injection: "on_demand"`) is
  enough**: G3 verified the model discovers run_code on demand via `read_skill` and adopts it
  naturally, without leaning on `agentic-code.md`'s full-prompt capability section — the capability
  section is an optional gain, not a trigger prerequisite.
- **No harm enabling (but skip the full prompt)**: when a task has `shell`, the model naturally solves
  it with shell directly — zero run_code adoption; then `agentic-code.md`'s capability section is pure
  disclosure overhead (measured cost ratio up to 1.43×); dropping the capability section and keeping
  just the tool surface falls back to ~1.0×~1.2×. Shell-carrying deployments should use `agentic.md`.
- **Stays off by default**: validation found no quality regression (all paired arms delivered
  identically; zero errors on the adoption path), but the sample is single-model + synthetic tasks
  (`demand_unverified`) — insufficient to justify default-on or a universal cost-reduction claim. Cost
  conclusions outside the "no-shell paginated aggregation" shape are unverified.

**The in-sandbox environment**: pure synchronous ES2023 — no async/Promise/timers/`import`/`fetch`;
`Date.now`/`Math.random` unavailable (reproducible evals). Five APIs: `tools.call(name, args)`
(failures throw and can be try/caught to continue; exceptions carry structured fields
`e.kind`/`e.tool`/`e.retryable` — `denied`/`hooks`/`budget`/`deadline`/`unknown_tool`/`lib` classes
are pointless to retry; a tool's own failure is class `tool` and deliberately carries no retryable —
the model reads the message and judges; same for the `llm` classes `llm`/`llm_output`),
`tools.list()` (call it first when unsure of tool names), `tools.budget()` (remaining call count /
wall clock / fuel / run-level token budget `tokens_left` — null when `max_total_tokens` is unset; the
query itself is free — check before starting a long batch), `console.log(...)` (streams live and
returns with the result), and `copy_bytes(src, dst)` (via `tools.call`: a host-side streaming
**byte-faithful copy** — binary / non-UTF-8 / large files land byte-identical; the content never
passes through a JS string). Plus three built-in helpers (`readAll(path)` / `readLines(path)` /
`writeVerified(path, content)`): whole-file read / line read / byte-faithful write (write, then read
back via raw and compare — no text round-trip); underneath they are `read_file`'s `format:"json"`
pagination and the `raw`/`content_b64` byte channels — no other built-in helpers exist
(`code.include` adds the two library helpers `include`/`saveLib`, previous section; `code.call_batch`
adds `tools.callBatch(entries)`, its section; `code.llm` adds the `llm` primitive, its section). Tool
descriptions are deliberately terse (a closed helper list + pointers); the deep contract and full
examples live in the **built-in `run-code` skill** (auto-listed when `code.enabled` is on, loaded on
demand via `read_skill`; drop a same-named `run-code/` directory in your skills directory to override
it). **The callable toolset** = the main agent's tool whitelist (MCP included), minus `task` / `todo` /
`run_code` itself; custom tools a host injects via `build_agent_with` are **invisible to the sandbox by
default** — each tool must be explicitly granted via `CallbackTool::with_code_access()` before it
enters the `tools.call` surface (third-party `Tool` implementations override `available_to_code()`
likewise; default false); every internal call walks **exactly the same hooks gate** as the outer loop
(the `__code__` identity tag lets hooks decide it separately).

**Resource termination & honest boundaries**: exceeding any limit force-terminates; the error message
carries the source tag (cancel / deadline / fuel / mem — four independent routes); `_initialize` /
compile-phase errors attribute likewise. An in-flight internal tool call **cannot** be interrupted by
the outer timeout (consistent with the `timeout_secs` soft-cap semantics) — total wall clock ≤
`timeout_secs` + the in-flight call's own cap. Internal calls don't enter the outer `tool_calls`
breakdown, but a per-call digest (tool name / duration / argument hash), `fuel_used`, and the
termination cause hang off the corresponding entry's `meta` field in `result.json`; internal tool usage
folds into `run_code`'s own bill (honest cost).

**Relationship to the security notes**: `shell` remains the unfiltered pass-through (the container is
its answer — run_code changes nothing there); run_code is another axis — it **grants the model no new
capability**, it only packages "the tool capabilities the model already has" into a limitable,
auditable orchestration layer. Note `deny_paths` only intercepts access **through the file tools**: if
`shell` is whitelisted, sandbox code can still reach arbitrary paths via `tools.call("shell", …)` —
for hard isolation drop `shell` from `tools` (or combine with `restrict_to_workdir`). Batch task
fragments may override `code` subfields but **only to tighten**: a fragment carrying `enabled` is
refused outright (`bad_task`); numeric fields take the min; `deny_paths` union; `fs_write_dirs`
intersection; `skill_injection` takes the tighter tier (off < on_demand < eager — fragments must not
widen disclosure); `precheck` may only go from true to false (fragments must not enable precheck).

Library / binding hosts additionally get a runtime API: Rust `Agent::set_code_config` /
`set_code_enabled` (the hot-update surface, effective between turns); Python `enable_code()` /
`disable_code()` / `set_code_config({...})`; Node `enableCode()` / `disableCode()` /
`setCodeConfig({...})` (each binding carries its own default-off `code` forwarding feature — without
it compiled in, these methods return an explicit rebuild-hint error instead of silently no-oping).

## Skills

A Skill is a nameable, on-demand **instruction/knowledge pack**: package "how to do a class of tasks" —
steps, scripts, reference material — into a folder that the agent auto-discovers at startup and reads
in full only when needed. The implementation follows the [Agent Skills open standard](
https://agentskills.io/specification) — three-tier progressive disclosure, a 6-field frontmatter, and
the `scripts/`/`references/`/`assets/` subdirectory conventions. (Claude Code's private extension
fields like `disable-model-invocation`/`context:fork`/`$ARGUMENTS` depend on its particular
session/permission/sub-agent model; TinyAgent doesn't implement them.)

### Directory conventions

Two directories are scanned at startup, **project overrides global** (same name → project wins):

- Global: `~/.config/tinyagent/skills/` (or `$XDG_CONFIG_HOME/tinyagent/skills/`) — personal general skills.
- Project: `<working directory>/.tinyagent/skills/` — ships with the repo, shared by a team.
- Built-in: skills embedded in the binary (currently only `run-code`, present when the `code` feature
  is enabled — the full usage guide for the run_code sandbox) — appear in the list only when the
  corresponding feature is on; a same-named on-disk skill directory overrides them.

Each skill is a **subdirectory** there with at least a `SKILL.md`, plus optional resource subdirectories
by convention:

```
~/.config/tinyagent/skills/
└── pdf-forms/
    ├── SKILL.md                  # required: frontmatter + body
    ├── scripts/extract.py        # optional: executable scripts
    ├── references/REFERENCE.md   # optional: detailed reference docs
    └── assets/template.json      # optional: templates / data / static resources
```

### The `SKILL.md` format

Starts with a YAML frontmatter; the body is the full instruction set. The frontmatter supports the
**standard 6 fields**:

| field | required | notes |
|---|---|---|
| `name` | yes | 1–64 chars, lowercase letters/digits/hyphens `-` only, no leading/trailing `-`, no consecutive `--`, **must equal the directory name** |
| `description` | yes | ≤1024 chars; state clearly "what it does + when to use it", with keywords that help the model recognize it. Enters the system prompt |
| `license` | no | a license name or a bundled license file |
| `compatibility` | no | suggested ≤500 chars (not enforced); environment requirements (runtimes, system packages, network, …) |
| `metadata` | no | `key: value` map of extension attributes (an indented block) |
| `allowed-tools` | no | space-separated pre-approved tool names (experimental, below) |

```
---
name: pdf-forms
description: Extract fields from PDF forms and output JSON. Use for form-type documents.
license: Apache-2.0
compatibility: needs python3
allowed-tools: shell read_file
metadata:
  author: docs-team
---

# PDF Forms

1. Extract using scripts/extract.py ...
2. Field formats: see references/REFERENCE.md ...
```

> Single-line scalar fields take single-line values only (multi-line YAML folding is unsupported);
> `metadata` is the only field accepting an indented `key: value` block.
> Unknown fields are ignored (as the standard allows).

### Three-tier progressive loading

| tier | when loaded | lands in | cost |
|---|---|---|---|
| ① blurb (name+description) | at startup, all skills | spliced into the **system prompt** (permanent) | tiny, and cached by prompt cache |
| ② full text (SKILL.md body) | when the model judges it relevant, via `read_skill` | into **conversation history** as a tool_result | on demand, reclaimable by context compression |
| ③ resource files (scripts/references/assets) | discover via `list_skill`, then `read_skill {file}` | same | same |

### The `read_skill` / `list_skill` tools

Auto-injected when skills are discovered (not part of the `config.tools` whitelist):

```
{ "skill": "pdf-forms" }                                  # read SKILL.md
{ "skill": "pdf-forms", "file": "scripts/extract.py" }    # read a resource (relative path)
{ "skill": "pdf-forms", "file": "references/REF.md" }     # read a reference doc
```

- `read_skill`: `file` defaults to `SKILL.md`; relative paths read subdirectory resources
  (`scripts/foo.sh`, `references/...`, `assets/...`). Paths are confined inside the skill directory:
  no `..`, no absolute paths, no backslashes; the real boundary after normalization is enforced by
  canonicalize + `starts_with(skill dir)` as the backstop (stopping the model from wandering out —
  not a hard isolation against hostile processes).
- `list_skill`: lists every file under a skill directory (subdirectories included, relative paths),
  for **autonomous discovery** of resources before reading them.

### `allowed-tools` (experimental)

A standard field, space-separated tool names. TinyAgent **parses + warns only, without enforcing**:
at assembly, declared tools are checked against the `config.tools` whitelist ∪
{`read_skill`,`list_skill`}; missing ones print `[skill] ... not in the tool whitelist` on stderr.
True "per-turn temporary pre-approval" needs a per-turn permission/activation layer (Claude Code's
semantics); TinyAgent's architecture differs and doesn't do it. Literal-name matching only — no
globs/`Tool(spec)` patterns.

### Behavioral details

- **Auto-injected, outside the whitelist**: when skills are discovered, `read_skill` + `list_skill`
  become available automatically (skills and `config.tools` are orthogonal layers; they never mix
  into the whitelist).
- **Not inherited by sub-agents**: sub-agents (`task`) use a dedicated system prompt and a
  whitelist-built toolset; they see neither skill blurbs nor `read_skill`/`list_skill`. When needed,
  the main agent carries the essentials in the `task` prompt.
- **Broken skills are skipped + warned**: a skill missing `SKILL.md`, whose frontmatter lacks
  `name`/`description`, whose `name` is invalid (charset/length/edge hyphens/`--`/directory-name
  mismatch), or whose `description` exceeds 1024 chars is skipped with a `[skill] ...` line on
  **stderr** — other skills and startup are unaffected.
- **No skill directories, no behavior change**: with nothing discovered, the system prompt and toolset
  are exactly as before (pure backward compatibility).

> **Upgrading from older versions**: early versions validated `name` loosely (uppercase/underscore
> allowed; directory name unchecked). After the standard alignment, non-compliant skills (e.g.
> `name: Deploy_V2`, or frontmatter `name` disagreeing with the directory name) are skipped with a
> warning — unify the directory name and `name` to lowercase-plus-hyphens.

## Development

`./scripts/check.sh` runs the CI set in one shot — fmt / four clippy configs / tests / scenario e2e /
doc gates (commands verbatim-identical to `.github/workflows/ci.yml`, failures summarized at the end);
or run just one group, e.g. `./scripts/check.sh fmt docs`. The bindings' clippy needs maturin / napi
tooling installed and is not in the default set — `./scripts/check.sh bindings`; the perf bench is
also opt-in: `./scripts/check.sh bench`.

```sh
cargo test --features tui,mock   # unit tests (incl. TUI input/render logic + lib_api integration tests)
cargo clippy --features tui --all-targets   # lint (TUI config)
cargo clippy --all-targets                 # lint (headless config)
cargo test --features code,mock            # run_code sandbox suite (mandatory after touching run_code.rs)
cargo clippy --features code --all-targets # lint (code config — run_code.rs sits behind the feature; invisible above)

# keyless end-to-end smoke test:
python3 examples/mock_openai.py 8791 &      # OpenAI-compatible SSE mock
python3 examples/mock_anthropic.py 8792 &   # Anthropic native SSE mock
cd $(mktemp -d) && MOCK_API_KEY=dummy tiny-agent --config <config pointing at the mock> --output result.json
```

`examples/mock_openai.py` / `mock_anthropic.py` are zero-dependency SSE mock servers for offline
verification of the whole agent loop (tool_use pairing, token usage parsing, streaming cancel). Note
`[profile.release] panic = "abort"`: release panics don't run `Drop` — debug any destructors (like the
TUI terminal restore) in debug builds.

The perf bench lives at `crates/core/benches/loop_bench.rs`, zero-dependency (hand-rolled on
`std::time::Instant`, `harness = false`, no criterion): one agent-loop turn's cost (empty / 200-message
history; `Agent::new` excluded), `Agent::new` itself (the one-shot cost of resume / sub-agent spawn),
`context_breakdown()` (pulled by hosts on every refresh), and `confined_to` (the file-tool sandbox
walk's ok / symlink / reject paths). It reports medians and p95 plus one machine-readable
`BENCH name=… median_ns=… p95_ns=…` line. Benchmarks are only comparable between versions on the same
machine; CI sets no perf gate:

```sh
./scripts/check.sh bench      # equivalent to the line below; --quick samples ~150ms per group
cargo bench -p tiny-agent-core --features mock --bench loop_bench -- --quick
```

The core agent loop has in-process regression tests: `crates/core/src/provider/mock.rs` is a
cfg(test) scripted Provider, and the `#[cfg(test)]` module of `crates/core/src/agent.rs` covers the
cancel contract (run never clears; pre-set-cancel; `clear_cancel` at acceptance points),
tool_use/tool_result N:N pairing (including the synthetic `[cancelled by user]` on mid-flight cancel),
context compression, usage accumulation invariants. **Touching core `agent.rs` paths comes with
matching loop tests.**

## Release process

Version numbers sync in **7 places** (miss one and the bindings drift from the core): root
`Cargo.toml`, `crates/core/Cargo.toml`, `bindings/{python,node}/Cargo.toml`,
`bindings/python/pyproject.toml`, `bindings/node/package.json`, and the five platform subpackage versions under `optionalDependencies`
in `bindings/node/package-lock.json` (`npm install` refreshes it automatically; `npm ci` errors hard
on desync — skip it and users can't install via npm ci; until the platform subpackages are first
published to npmjs the lock carries no entries for them, which is why CI uses `npm install` — see the
comment in `.github/workflows/ci.yml`).

1. bump the version in all seven places;
2. add the `[new version]` section to `CHANGELOG.md` (moved from `[Unreleased]`, dated);
3. commit, then tag: `git tag vX.Y.Z`;
4. `git push --tags` — the tag landing on GitHub auto-triggers the release workflow
   (`.github/workflows/release.yml`: the musl x86_64 static binary + sha256 uploaded to GitHub
   Releases with auto-generated release notes; plus the `bindings-node` five-platform matrix — main
   package + `tiny-agent-<platform>` subpackage builds, auto-published to the registry when the
   `NPM_TOKEN` secret is configured, otherwise the subpackage tarballs attach to the Release; plus
   `bindings-python` building the Python sdist + wheel (CPython manylinux2014 / manylinux_2_17 x86_64,
   without the `code` feature — the standard wheel carries no wasmtime; other interpreters build from
   sdist) attached to the Release, not published to PyPI). The mirror side (gitee) syncs tags via
   `git push gitee --tags`.

   **Artifacts carry the `code` feature** (since 0.3.x): the musl binary compiles in the run_code
   sandbox (+≈4.6 MiB over the featureless build), but the runtime switch `code.enabled` stays off —
   only a config that explicitly writes `"code": {"enabled": true}` exposes it to the model (the red
   line: an injected model does not get "loops + code execution" by default — see the run_code
   section). Size-conscious self-builders drop `--features code` (~2.7 MiB).

## Acknowledgments

- Thanks to Zhipu AI (Z.ai) for the free tokens: this project was built primarily on GLM-5.3-Flash, a
  free model from Zhipu.
- Parts of the development also used Claude Code and DeepSeek.

## License

[MIT](LICENSE) © 2026 cppupu

## Code structure

The line counts and module inventory are generated live from the source files by
`scripts/gen_structure.py` (CI gates it with `--check` in the fmt job against drift); each module's
one-line responsibility is maintained in that script's ENTRIES.

<!-- BEGIN CODE STRUCTURE (generated by scripts/gen_structure.py) -->
```text
crates/core/src/  # kernel crate (package tiny-agent-core, lib name tiny_agent): agent loop / tools / providers / config / assembly
├── lib.rs            (  582)  # lib root: build_agent* / assemble (tool table, ToolCtx, code install, skill injection)
├── fsutil.rs         (  463)  # atomic-write primitives (O_EXCL temp + fsync + dir fsync; shared by session/result/report)
├── diag.rs           (  168)  # diagnostics channel: global sink installable by GUI hosts (falls back to stderr, byte-identical) — soft-failure warnings of skills/MCP/session/run_code go through it
├── config.rs         ( 2706)  # config parsing + task-fragment merge + batch override whitelist
├── provider/  # LLM backends
│   ├── mod.rs            ( 1063)  # shared Message/Block, SSE reader, CancellableSse
│   ├── openai.rs         ( 1817)  # OpenAI-compatible (include_usage; 400 fallbacks: max_tokens→max_completion_tokens / drop stream_options)
│   ├── anthropic.rs      ( 1219)  # Anthropic native (message_start/message_delta usage)
│   └── mock.rs           (  345)  # scripted Provider for tests (cfg(any(test, feature="mock")))
├── agent.rs          ( 4293)  # core loop + usage accounting + cancel + context compression (preflight/overflow dual trigger + rolling chunked summaries) + adjacent-message normalization + same-turn multi-task parallelism
├── agent_tests.rs    ( 9986)  # core-loop regression tests (scripted provider::mock, no network)
├── usage_probe.rs    (   94)  # thread-local usage side channel for parallel workers: recovers billed usage after a panic unwind at the join point (keeps result.json and the shared counter from diverging)
├── compress.rs       (  998)  # compression engine (find_safe_cut / chunked summaries / saturating usage merge + block-level billing primitives)
├── context_report.rs (  886)  # context composition breakdown (ContextBreakdown estimate: 3 system parts / 4 tool classes / message typing + anchor calibration)
├── retry.rs          (  158)  # exponential backoff shared by batch --retries and single-run transient retry (±25% jitter against lockstep)
├── session.rs        (  522)  # session persistence (atomic writes + fold archive)
├── skills.rs         ( 1089)  # three-tier skill loading (built-in / user dir / project)
├── hooks.rs          (  181)  # hooks gate (Deny / Modify / audit)
├── tools/  # toolset: built-in tools + MCP wrappers + task sub-agent + run_code (feature code)
│   ├── mod.rs            ( 1404)  # Tool trait + ToolCtx + CallbackTool/SharedTool + whitelist assembly
│   ├── shell.rs          ( 1250)  # shell tool (unix: sh -c; Windows: cmd /C + raw_arg verbatim command passing, defeating std quoting)
│   ├── file.rs           ( 1524)  # read_file / write_file / edit_file / list_dir
│   ├── image.rs          (   87)  # load_image tool
│   ├── document.rs       (  100)  # PDF → Block::Document loading (lib API load_document_block)
│   ├── skill.rs          (  981)  # read_skill / list_skill (outside the whitelist; injected by assembly)
│   ├── mcp.rs            (  627)  # MCP tool wrapper (sanitize_name + wire_name dispatch)
│   ├── subagent.rs       ( 1053)  # task sub-agent tool (own context + turn cap; non-recursive)
│   ├── todo.rs           (  311)  # todo tool (task plan, full replacement; plan lands in result.json)
│   ├── run_code.rs       (  103)  # run_code facade (feature code): mod declarations + symbol re-exports; implementation in run_code/ submodules
│   ├── llm.rs            (  740)  # llm judgment primitive (feature code + code.llm gate): single-shot stateless inner model call — LD-17 reserve/settle, LD-18 label normalization, shared Provider cell
│   └── run_code/  # run_code submodules (same code gate): split from the original 11.9k single file by concern
│       ├── contract.rs       (  747)  # guest-facing contract text: reserved names/CODE marker/byte caps, PRELUDE three sources, skill md loading, strip_once
│       ├── sandbox.rs        (  799)  # wasmtime sandbox: QuickJS module load & cache, epoch/fuel/MemLimiter, CallTrace, HostCtx, wall clock, guest memory read caps
│       ├── guest_api.rs      ( 3587)  # host function surface: unpack/read/write, host_call_tool dispatch, inner-result envelope, llm marker parse folding, audit meta
│       ├── fs_policy.rs      ( 1940)  # fs/glob policy: path interception & lexical normalization, write-dir constraints, symlink checks, glob matching (** folding + segment caps)
│       ├── lib_index.rs      ( 2013)  # lib family: session library index/read-write caps, sha256, reserved names, include delivery
│       ├── batch.rs          ( 2288)  # callBatch: wave scheduling, output admission (total-cap degradation), entry envelopes
│       ├── precheck.rs       (  858)  # FR-2 precheck: lexer + keyword-hit rendering
│       ├── tool.rs           ( 1096)  # the tool itself: RunCodeTool state machine + Tool impl (hash-pinned description/schema)
│       └── test_util.rs      (  123)  # shared test helpers (cfg(test): scratch/ctx/llm assembly)
├── mcp.rs            ( 4963)  # MCP client (stdio / Streamable HTTP incl. 202→GET stream / legacy SSE)
├── winjob.rs         (  413)  # Windows Job Object process-tree kill (shared by shell timeout/cancel + MCP child reaping; two drop strategies)
└── m0_probes.rs      (  337)  # M0 mechanism probes (cfg(test), #[path]-declared in lib.rs): reserve/settle ledger, shared Provider cell, per-request model routing in-process prototype (A-L2b/2c/9 — reused directly by the M3 llm primitive)

src/  # CLI crate (package tiny-agent, bin tiny-agent): process shell + headless rendering + batch + TUI
├── main.rs           ( 1021)  # CLI entry + assembly (build_agent) + --session none
├── signal.rs         (  123)  # headless SIGINT → cancel (first press graceful / second hard-exit 130)
├── batch.rs          ( 3581)  # batch tasks + --retries/--failed + report merging
├── harness.rs        ( 2156)  # headless mode + result.json + NDJSON + exit codes
├── harness_tests.rs  ( 1127)  # harness behavior tests (retry ladder / result.json / NDJSON; moved here with harness from the kernel's agent_tests.rs)
├── tui.rs            (   20)  # TUI module root (feature tui): mod declarations + public exports (run / transcript_rows)
└── tui/  # TUI submodules (same tui gate): split from the original 8.7k-line single file by concern
    ├── app.rs            ( 1639)  # session assembly: run entry / worker thread / event & input channels / ui_loop event body / Ctrl-C double-press window
    ├── input.rs          (  356)  # multi-line InputBuffer editor / input history recall / horizontal input scrolling
    ├── slash.rs          (  543)  # slash commands: classify grammar / read-only replies / completion popup state machine / /context report
    ├── theme.rs          (  147)  # theme: single source for history-row & chrome colors / light-dark switch TINYAGENT_TUI_THEME / number formatting helpers
    ├── markdown.rs       (  632)  # markdown & ANSI rendering: render_markdown / escape parsing / fence scanning (shared with streaming splits) / MdCache single-slot cache
    ├── transcript.rs     ( 1190)  # conversation Row model and all build/mutation logic: history rendering / streaming increments & incremental splits / /export dump / resume rebuild
    ├── render.rs         (  490)  # terminal drawing: scrollback commit_rows / live-region render / input & status bar chrome / width & control-char utilities
    └── tests.rs          ( 3862)  # TUI regression tests (TestBackend cell-grid assertions + input / completion / streaming-split logic)
```
<!-- END CODE STRUCTURE -->
