//! Minimal MCP client: stdio, Streamable HTTP (2025-03-26) and
//! legacy HTTP+SSE (2024-11-05) transports. Implements just enough
//! of the protocol: initialize, tools/list, tools/call.

use crate::config::{McpServerConfig, McpTransport};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: &str = "2025-03-26";
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Per-round read deadline for the long-lived SSE GET streams (the standing
/// standalone stream and the legacy SSE reader share it). A blocked
/// reqwest::blocking read cannot be interrupted, but a request-level timeout
/// bounds EVERY read: on a healthy stream the blocking read wakes at least
/// once per [`STREAM_ROUND`], sees the stop flag (teardown) or rotates the
/// connection (routine cadence — each rotation also re-establishes the
/// delivery path a server push needs, instead of trusting one indefinite
/// GET). This round bound is what makes reader-thread teardown bounded and
/// caps the thread + TCP connection a teardown leaves behind at
/// ~one round + backoff instead of "until process exit".
///
/// Declared behavior change (2026-10): a healthy stream now reconnects every
/// round — a fresh GET every [`STREAM_ROUND`] that the old
/// single-connection-until-death client never sent. That is new periodic
/// request/log load on the server, accepted in exchange for bounded teardown
/// and a bounded no-connection window.
///
/// Delivery across a rotation, honestly stated: a response whose push lands
/// in a rotation (or any reconnect) gap is delivered ONLY if the SERVER
/// re-sends it on the fresh connection — our GETs carry no Last-Event-ID and
/// JSON-RPC over SSE has no re-send obligation. The pending table dispatches
/// whatever arrives on the live connection and never re-delivers anything
/// itself; a response the server does not re-send is lost, and the caller
/// waits out the full [`CALL_TIMEOUT`]. Rotation shrinks the delivery gap,
/// it does not provide redelivery.
const STREAM_ROUND: Duration = Duration::from_secs(30);
/// Reconnect backoff after a stream ended EARLY (server closed it / real
/// read error / failed reconnect connect). Deadline rotations skip it: the
/// stream was healthy, and the rotation must re-open the delivery path
/// immediately.
const STREAM_BACKOFF: Duration = Duration::from_millis(500);
/// How long a Drop waits (50ms polls) for its stream reader thread to exit
/// before detaching. The reader wakes at its current round's deadline at the
/// latest, so the detached case self-reclaims within ~one round + backoff.
const READER_JOIN_WAIT: Duration = Duration::from_secs(2);
/// How long request()/notify() wait for one frame to sink into the server's
/// stdin before declaring the server dead. A healthy server drains even a
/// multi-MiB tools/call frame (base64 images) in well under a second — pipes
/// move data at memory speed and reading stdin IS the server's main loop — so
/// 10s can only elapse when the server has stopped reading stdin entirely
/// (wedged, deadlocked or dead). That state is unrecoverable for this
/// transport: the writer thread is stuck inside write() holding the only
/// stdin handle, so the caller kills the child to break the pipe (which also
/// ends the stuck thread — see [`write_frame_cancelable`]).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on a single line read from an MCP server (stdio line or one SSE
/// `data:`/`event:` line). Bounds memory against a runaway or malicious server
/// that emits an unbounded line with no newline — `read_line` would otherwise
/// grow the buffer without limit and OOM the process. 16 MiB is far above any
/// legitimate JSON-RPC message (a tools/call result is truncated to
/// `max_output_chars` AFTER receipt, so the wire never carries the full blob).
const MCP_LINE_MAX: usize = 16 * 1024 * 1024;
/// Upper bound on one aggregated SSE event. Each `data:` line is already
/// capped at `MCP_LINE_MAX`, but a single event can splice many of them; an
/// adversarial or buggy server could stream data: lines indefinitely (until a
/// blank-line terminator that never comes) and exhaust memory. A legitimate
/// JSON-RPC result is one big line well under the per-line cap, so 4× that is
/// a generous bound that still defeats runaway aggregation.
const MCP_EVENT_MAX: usize = 4 * MCP_LINE_MAX;

/// Cap on tools accepted from one server's `tools/list`. The tool list is
/// model-visible context on EVERY request, and its size is the server's
/// choice — a hostile/compromised server listing 10k tools used to bloat
/// every request for the whole session.
const MCP_MAX_TOOLS: usize = 256;
/// Per-tool description cap (chars) — same 1024 the skill standard uses.
const MCP_TOOL_DESC_MAX: usize = 1024;
/// Per-tool inputSchema cap (serialized bytes).
const MCP_SCHEMA_MAX: usize = 64 * 1024;
/// Error-body read cap: a diagnostic string never needs more, and the body's
/// size is the server's choice.
const MCP_ERR_BODY_MAX: u64 = 16 * 1024;

/// Read one line (including its trailing `\n`) into `buf` (bytes appended;
/// caller clears `buf` between calls), bounded by `max_bytes`. Returns the
/// number of bytes pushed, or 0 at clean EOF. Bails if a single line exceeds
/// `max_bytes`. Reads in `fill_buf` chunks (not byte-by-byte) and accumulates
/// raw bytes, validating UTF-8 only at the call site — so a multi-byte char
/// split across reads isn't misdecoded mid-accumulation.
///
/// EOF with a NON-EMPTY `buf` returns that tail as a final line (what
/// `BufRead::read_line` did before this helper existed): a last NDJSON
/// message without a trailing newline must not be silently dropped. The NEXT
/// call sees an empty buf + EOF and returns 0.
fn read_capped_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<usize> {
    loop {
        // fill_buf borrows `reader`; consume (also mut) can't run while that
        // borrow is live. Compute + copy under the borrow, then consume after.
        let (consumed, done) = {
            let available = reader.fill_buf().context("MCP read failed")?;
            if available.is_empty() {
                return Ok(buf.len()); // clean EOF; deliver any unterminated tail
            }
            let (take, found_nl) = match available.iter().position(|&c| c == b'\n') {
                Some(i) => (&available[..=i], true),
                None => (available, false),
            };
            if buf.len() + take.len() > max_bytes {
                bail!("MCP line exceeds {max_bytes}-byte limit (runaway server output?)");
            }
            // `buf` is a distinct &mut, so extending it from a slice that
            // borrows `reader` (shared) is allowed.
            buf.extend_from_slice(take);
            (take.len(), found_nl)
        };
        reader.consume(consumed);
        if done {
            return Ok(consumed);
        }
        // No newline yet — loop and fill again; the running `buf` keeps the
        // partial line so the cap stays a per-line bound, not per-chunk.
    }
}

/// A tool as advertised by an MCP server.
pub struct McpToolInfo {
    /// The tool's WIRE name, exactly as the server sent it. It is kept
    /// verbatim for `tools/call`; only the agent-facing display name goes
    /// through sanitization.
    pub name: String,
    /// Description sent by the server (shown to the model in the tool def).
    pub description: String,
    /// The server's JSON Schema for this tool's input.
    pub input_schema: Value,
    /// `annotations.readOnlyHint` from the server (absent = false). A
    /// SERVER-declared scheduling hint feeding `Tool::batchable` — not a
    /// side-effect-freedom proof (LD-19: 有注解也只视为调度提示).
    pub read_only_hint: bool,
}

/// One JSON-RPC message channel. Requests carry a client-generated id
/// and return the message's `result` (or an error on JSON-RPC error).
trait Transport: Send {
    /// `cancel` is the run's per-turn cancel flag: transports with a
    /// channel-wait (stdio / SSE) slice it into ≤50ms polls so a Ctrl-C
    /// lands within ~50ms instead of after the full CALL_TIMEOUT — a wedged
    /// server used to hold the turn hostage for 120s with no way to stop.
    /// `None` = the startup handshake (no run in flight, nothing to cancel).
    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value>;
    fn notify(&mut self, method: &str, params: Value) -> Result<()>;
    /// Arm cancel-aware teardown. Default no-op; `StdioTransport` overrides it
    /// to spawn a reaper that kills the child process group on shutdown, so a
    /// stuck call unblocks (see `StdioTransport::set_shutdown`).
    fn set_shutdown(&mut self, _shutdown: &Arc<AtomicBool>) {}
}

/// One connected MCP server: a JSON-RPC transport plus the initialize
/// handshake state. Created and owned by [`McpTool`](crate::tools::mcp)
/// instances; use [`McpClient::start`] rather than constructing one directly.
pub struct McpClient {
    transport: Box<dyn Transport>,
    next_id: u64,
}

impl McpClient {
    /// Connect and run the initialize handshake, then fetch the tool list.
    pub fn start(cfg: &McpServerConfig) -> Result<(Self, Vec<McpToolInfo>)> {
        cfg.validate()?;
        let mode = cfg.transport.unwrap_or(McpTransport::Auto);
        match (&cfg.command, &cfg.url) {
            (Some(_), _) => Self::handshake(StdioTransport::spawn(cfg)?, &cfg.name),
            (None, Some(url)) => match mode {
                McpTransport::Http => Self::handshake(HttpTransport::new(url, cfg)?, &cfg.name),
                McpTransport::Sse => Self::handshake(SseTransport::connect(url, cfg)?, &cfg.name),
                McpTransport::Auto => {
                    // Try Streamable HTTP via a FULL handshake — a legacy SSE
                    // server 404/405s (or streams nothing usable for) the
                    // POST initialize, failing fast; that failure IS the
                    // probe. No separate probe request, so a strict
                    // Streamable server sees exactly ONE initialize (the old
                    // probe sent a second one). Fall back to legacy SSE on
                    // any HTTP handshake failure.
                    match Self::handshake(HttpTransport::new(url, cfg)?, &cfg.name) {
                        ok @ Ok(_) => ok,
                        Err(e) => {
                            crate::diag::diag_warn!(
                                "[warn] MCP '{}': Streamable HTTP handshake failed ({e:#}); falling back to legacy SSE",
                                cfg.name
                            );
                            Self::handshake(SseTransport::connect(url, cfg)?, &cfg.name)
                        }
                    }
                }
            },
            (None, None) => unreachable!("validated above"),
        }
    }

    fn handshake(
        transport: impl Transport + 'static,
        name: &str,
    ) -> Result<(Self, Vec<McpToolInfo>)> {
        let mut client = Self {
            transport: Box::new(transport),
            next_id: 1,
        };
        client.call(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "tiny-agent", "version": env!("CARGO_PKG_VERSION")}
            }),
            /* cancel: */ None,
        )?;
        client.notify("notifications/initialized", json!({}))?;

        let tools = client.call("tools/list", json!({}), /* cancel: */ None)?;
        Ok((client, bound_tools(tools, name)))
    }

    /// Arm teardown on the underlying transport (stdio servers spawn a reaper
    /// that kills the child on shutdown). Called by the agent via
    /// `McpTool::set_shutdown` when the FRONTEND wires a shutdown signal
    /// (distinct from the per-turn cancel flag — see `StdioTransport`).
    pub(crate) fn set_shutdown(&mut self, shutdown: &Arc<AtomicBool>) {
        self.transport.set_shutdown(shutdown);
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.transport.notify(method, params)
    }

    /// `cancel` (the run's per-turn flag) is forwarded to the transport; see
    /// [`Transport::request`] for the slice-poll contract.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.transport.request(id, method, params, cancel)
    }
}

/// Unwrap a JSON-RPC response message into its `result`. A response with
/// NEITHER `result` nor `error` is a protocol violation — surfacing it as an
/// empty tool result would hide it, so fail loudly. (A present-but-null
/// `result` is legal and passes through.)
fn extract_result(msg: Value, method: &str) -> Result<Value> {
    if let Some(err) = msg.get("error") {
        bail!(
            "MCP '{}' error {}: {}",
            method,
            err["code"],
            err["message"].as_str().unwrap_or("")
        );
    }
    if msg.get("result").is_none() {
        bail!("MCP '{method}' response has neither result nor error (protocol violation)");
    }
    let mut msg = msg;
    Ok(msg["result"].take())
}

fn frame(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Bound a `tools/list` result into [`McpToolInfo`]s. The tool list is
/// model-visible context on EVERY request and its size is the server's
/// choice, so a hostile/compromised server gets to decide three things that
/// must all be capped: the tool COUNT (MCP_MAX_TOOLS), each description
/// (MCP_TOOL_DESC_MAX chars — same 1024 the skill standard uses), and each
/// serialized inputSchema (MCP_SCHEMA_MAX bytes). Offenders are dropped with
/// a stderr warning rather than kept — a garbage schema just moves the
/// failure into every later request. `server` (the config name) is woven
/// into each warning: several MCP servers can be configured side by side,
/// and a cap message without the server's name can't be acted on.
fn bound_tools(tools: Value, server: &str) -> Vec<McpToolInfo> {
    let mut infos = Vec::new();
    for t in tools["tools"].as_array().cloned().unwrap_or_default() {
        if infos.len() >= MCP_MAX_TOOLS {
            crate::diag::diag_warn!(
                "[warn] MCP '{server}': exposes more than {MCP_MAX_TOOLS} tools; ignoring the rest"
            );
            break;
        }
        let name = t["name"].as_str().unwrap_or_default().to_string();
        // A missing/empty name must not enter the tool table: dispatch is BY
        // NAME, so an "" entry is uncallable dead weight in the model-visible
        // list — and a second nameless tool from a sloppy server would
        // collide with it. Drop with a warning, same policy as the schema
        // offender below.
        if name.is_empty() {
            crate::diag::diag_warn!(
                "[warn] MCP '{server}': tool with missing or empty name; ignoring the tool"
            );
            continue;
        }
        let mut description = t["description"].as_str().unwrap_or_default().to_string();
        if description.chars().count() > MCP_TOOL_DESC_MAX {
            crate::tools::truncate(&mut description, MCP_TOOL_DESC_MAX);
        }
        let input_schema = t["inputSchema"].clone();
        if serde_json::to_string(&input_schema)
            .map(|s| s.len())
            .unwrap_or(usize::MAX)
            > MCP_SCHEMA_MAX
        {
            crate::diag::diag_warn!(
                "[warn] MCP '{server}': tool '{name}' inputSchema exceeds {MCP_SCHEMA_MAX} bytes; ignoring the tool"
            );
            continue;
        }
        infos.push(McpToolInfo {
            name,
            description,
            input_schema,
            read_only_hint: t["annotations"]["readOnlyHint"].as_bool().unwrap_or(false),
        });
    }
    infos
}

fn notify_frame(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Pending-response table shared by the async transports (stdio / legacy SSE).
type Pending = std::sync::Arc<std::sync::Mutex<HashMap<u64, mpsc::Sender<Value>>>>;

/// HTTP client with explicit timeouts for request/response calls.
/// (The long-lived SSE GETs use their own connect-only clients and add a
/// per-request [`STREAM_ROUND`] round timeout — see the two stream readers.)
fn http_client(total: Duration) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(total)
        // Never follow redirects: a malicious/compromised MCP server could
        // 302 our authenticated POST (custom headers aren't auto-stripped
        // across hosts) to an internal or attacker address.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| {
            // Build failure is near-impossible, but the fallback MUST re-apply
            // the no-redirect policy — a bare Client::new() follows redirects
            // and would reintroduce the key-leak vector documented above.
            reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client build")
        })
}

fn pending_new() -> Pending {
    Default::default()
}

fn pending_dispatch(pending: &Pending, msg: Value) {
    // Only RESPONSES dispatch: a server-initiated REQUEST (sampling,
    // roots/list, …) also carries a numeric `id`, and matching one of our
    // pending ids by accident would hand the caller the wrong message.
    // A `method` key marks requests/notifications — never responses.
    if msg.get("method").is_some() {
        return;
    }
    if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
        if let Some(tx) = pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
            let _ = tx.send(msg);
        }
    }
}

/// Fail every waiter immediately (server closed the connection),
/// so callers don't sit out the full CALL_TIMEOUT. `detail` carries any
/// captured server stderr — a dying server's own error message is the single
/// best clue for why the handshake/call failed.
fn pending_fail_all(pending: &Pending, detail: &str) {
    let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
    let message = if detail.trim().is_empty() {
        "MCP server closed the connection".to_string()
    } else {
        format!(
            "MCP server closed the connection; its stderr: {}",
            detail.trim()
        )
    };
    for (_, tx) in map.drain() {
        let _ = tx.send(json!({
            "error": {"code": -32000, "message": message}
        }));
    }
}

// ─── stdio ──────────────────────────────────────────────────────────

/// Wait for a response on `rx`, slicing the CALL_TIMEOUT into ≤50ms polls of
/// the run's cancel flag (the shared wait core of the stdio / SSE transports).
/// A Ctrl-C mid-wait bails within one slice instead of after the full 120s —
/// the blocked `McpTool::run` was the run loop's only tool wait with no
/// intra-turn cancel path. On bail the CALLER's error path reclaims the
/// pending slot; the server's late response (if it ever arrives) finds no
/// entry and is dropped by `pending_dispatch`, so an abandoned id can't
/// corrupt the next call.
fn recv_cancelable(
    rx: &mpsc::Receiver<Value>,
    method: &str,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    let deadline = Instant::now() + CALL_TIMEOUT;
    loop {
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        if slice.is_zero() {
            bail!("MCP call '{method}' timed out or server died");
        }
        match rx.recv_timeout(slice) {
            Ok(msg) => return Ok(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    bail!("MCP call '{method}' cancelled");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("MCP call '{method}' timed out or server died");
            }
        }
    }
}

/// Poll granularity while waiting for the writer thread — same shape as
/// `recv_cancelable`'s 50ms slices: bounds cancel latency during the write
/// phase to ~one slice.
const WRITE_POLL_SLICE: Duration = Duration::from_millis(50);

/// Write one frame (payload + trailing `\n`) on a worker thread, waiting in
/// ≤50ms slices so the WRITE phase honors cancel and a hard deadline too.
/// The old inline `self.stdin.write_all(...)` blocked the caller inside the
/// kernel write when the pipe was full (a tools/call frame with base64 images
/// is multi-MiB; a server that stops reading stdin never drains it): the 50ms
/// cancel poll lived in `recv_cancelable` — never reached — CALL_TIMEOUT only
/// guarded the receive half, and Drop's kill couldn't run either because the
/// caller was stuck INSIDE request(). The whole turn hung.
///
/// A blocking pipe write cannot be interrupted in place, only abandoned: the
/// writer (any `Write`; the transport's `ChildStdin` in production) moves onto
/// a thread and is handed BACK through the channel on success, so the
/// transport stores it again for the next call. On timeout/cancel/write-error
/// the thread keeps the writer — the caller must break the pipe (kill the
/// child) for it to exit; the thread is detached here, never joined. Generic
/// over `Write` so the core loop is unit-testable with a never-draining mock.
fn write_frame_cancelable<W: Write + Send + 'static>(
    w: W,
    payload: &[u8],
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<W> {
    let (tx, rx) = mpsc::channel::<std::result::Result<W, std::io::Error>>();
    let payload = payload.to_vec();
    // If the spawn itself fails, the closure (and the `w` it captured) is
    // dropped — closing the pipe — and the `?` surfaces the error; the
    // transport's caller treats it like any dead-write (kills the child).
    let _ = std::thread::Builder::new()
        .name("mcp-stdio-writer".into())
        .spawn(move || {
            let mut w = w;
            let res = w
                .write_all(&payload)
                .and_then(|_| w.write_all(b"\n"))
                .and_then(|_| w.flush());
            // Hand the writer back on success — the transport re-stores it
            // for the next call. On failure it drops here (pipe broken).
            let _ = tx.send(res.map(|_| w));
        })
        .context("cannot spawn MCP stdio writer thread")?;
    let deadline = Instant::now() + timeout;
    loop {
        // Slice bounded by both the poll granularity and the remaining
        // deadline, mirroring recv_cancelable.
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(WRITE_POLL_SLICE);
        if slice.is_zero() {
            bail!("MCP write timed out after {timeout:?} (server stopped reading stdin?)");
        }
        match rx.recv_timeout(slice) {
            Ok(Ok(w)) => return Ok(w),
            Ok(Err(e)) => return Err(anyhow!("MCP write failed: {e}")),
            // The thread always sends exactly one message before exiting, so
            // Disconnected can only mean it panicked mid-write — the pipe is
            // unusable either way; treat it as broken.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("MCP stdio writer thread died without reporting")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    bail!("MCP write cancelled (server not reading stdin?)");
                }
            }
        }
    }
}

/// 为 Windows 的 `cmd /C` 拼出 /C 之后的完整参数尾（命令 + 参数，按 cmd 的
/// 习惯加引号后用单个空格连接），供 spawn 处 `.raw_arg` 整段直传。
///
/// 为什么不能让 std 的 `.arg`/`.args` 代劳：std 按 MSVCRT 规则编码每个参数
/// （含空格/引号即整体包裹、内嵌 `"` 写成 `\"`），而 cmd 对 /C 之后的字符串
/// 有自己的剥引号规则——首字符是 `"` 且全行恰好两个引号、中间是可执行路径
/// 且无特殊字符 → 引号保留；否则（引号 ≥4 个，或含 `&()< >|^` 等特殊字符）
/// → 剥掉首个字符和全行最后一个引号。于是「命令含空格（如
/// `C:\Program Files\nodejs\node.exe`，必被 std 包裹）+ 任一参数也被包裹」
/// 时，std 拼出的 `cmd /C "C:\Program Files\nodejs\node.exe" "my
/// project\server.js"`（4 个引号）被 cmd 剥成 `C:\Program Files\nodejs\node.exe"
/// "my project\server.js`——命令行就此打碎（node 装在 Program Files + 项目
/// 路径含空格即触发）。同 shell 工具 raw_arg 修复的理由（见 shell.rs 的
/// spawn 注释）；这里是 argv 形态，需要自己拼好每个参数的引号。
///
/// 引号规则：token 为空、含空白/制表符/引号、或含非 ASCII 字符（CJK 等，
/// 保守处理）时包裹 `"`；内嵌 `"` 写成 `\"`（cmd 不解析反斜杠转义、也不改动
/// 参数区内容，子进程的 MSVCRT argv 解析按 `\"` 还原原字符——各取所需）；
/// 收尾引号前的连续反斜杠加倍（MSVCRT 规则：否则 `C:\dir\` 末尾的 `\` 会把
/// 我们补的 `"` 吃成转义引号，参数边界丢失）。
///
/// 触发集 2026-10 扩展（before/after）：**before**——触发集 = 空 / ASCII
/// 空白 / `"` / 非 ASCII，cmd 元字符 `& | < > ^ ( ) % !` 是 ASCII 非空白、
/// 裸露在引号外：cmd /C 的解析层把引号外的 `&`、`|` 当命令分隔符、`<`/`>`
/// 当重定向、`^` 当转义符、`( )` 当分组符，MCP args 里的 URL query
/// （`?a=1&b=2`）之类在 cmd 层就被拆碎，子进程永远看不到完整参数。
/// **after**——这十个字符进触发集，引号内它们对 cmd 一律是字面量。
/// 定性：用户配置信任面的正确性修复，非安全边界——`%VAR%` 展开连引号也
/// 挡不住（cmd 在引号内仍展开环境变量）、`!` 仅延迟扩展开启时特殊，对这
/// 两类加引号只是保守表意，不构成保证。
///
/// 引号+元字符同现限制（文档化限制，非保证）：token 同时含字面 `"` 与
/// 触发集元字符、且元字符落在第一个内嵌引号之后时，内嵌 `"` 写成的
/// `\"` 只对子进程的 MSVCRT argv 解析生效——cmd 层不认 `\"`（反斜杠对
/// cmd 是字面量、挡不住引号开关），第一个内嵌引号即切出 cmd 的引号区，
/// 其后的元字符裸露给 cmd 解析层被拆碎（如 `--filter "x&y"` 的 `&`）。
/// cmd 对字面 `"` 无通用转义（BatBadBut 一族），此处不改编码、按限制
/// 记录；触发面 = 用户配置的 args 同时含 `"` 与元字符。测试侧以 cmd
/// 引号区 oracle（`cmd_bare_metachars`）钉住两面：无引号 token 的引号区
/// 全覆盖（判例 + fuzz），含引号 token 显式豁免。
///
/// 若拼好的尾段以 `"` 开头（命令路径被包裹——cmd 剥引号唯一可能发生的形
/// 态），整段再包一层外引号：cmd 的剥除规则拿掉的正是这层外包装，剥完剩下
/// 的就是上面拼好的、引号完好的命令行。首字符不是 `"` 时 cmd 从不剥引号，
/// 原样直传（与旧行为字节一致）。
#[cfg(windows)]
fn windows_cmd_tail(cmd: &str, args: &[String]) -> String {
    /// 按 cmd/MSVCRT 习惯编码单个 token（规则见外层函数文档）。
    fn quoted(tok: &str) -> String {
        let needs_quotes = tok.is_empty()
            || tok.chars().any(|c| {
                c.is_ascii_whitespace()
                    || c == '"'
                    || !c.is_ascii()
                    // cmd 元字符（2026-10 扩展，before/after 见外层函数文档）：
                    // 引号外会被 cmd /C 解析层当命令分隔符/重定向/转义/分组，
                    // `&` 活体事故：MCP args 里的 URL query `?a=1&b=2` 被 cmd
                    // 拆碎，子进程看不到完整参数。`%`/`!` 是保守表意（引号
                    // 挡不住 cmd 的变量展开）。
                    || matches!(c, '&' | '|' | '<' | '>' | '^' | '(' | ')' | '%' | '!')
            });
        if !needs_quotes {
            return tok.to_string();
        }
        let mut out = String::with_capacity(tok.len() + 2);
        out.push('"');
        // 反斜杠只在紧邻 `"`（将被转义）或收尾引号前才需要加倍：逐字符攒
        // 当前连续反斜杠数，遇 `"` 翻倍写出，其他字符原样写出。
        let mut backslashes: usize = 0;
        for c in tok.chars() {
            if c == '\\' {
                backslashes += 1;
                continue;
            }
            match c {
                '"' => {
                    for _ in 0..backslashes * 2 {
                        out.push('\\');
                    }
                    out.push_str("\\\"");
                }
                _ => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    out.push(c);
                }
            }
            backslashes = 0;
        }
        // 尾部连续反斜杠紧邻的是我们补的收尾引号 → 同样翻倍。
        for _ in 0..backslashes * 2 {
            out.push('\\');
        }
        out.push('"');
        out
    }

    let mut tail = quoted(cmd);
    for a in args {
        tail.push(' ');
        tail.push_str(&quoted(a));
    }
    if tail.starts_with('"') {
        format!("\"{tail}\"")
    } else {
        tail
    }
}

struct StdioTransport {
    child: std::process::Child,
    /// Job-Object tree-kill armed at spawn (Windows only): the direct
    /// child is the cmd /C wrapper, the REAL server a grandchild only
    /// TerminateJobObject can reach — kill_child() and Drop both go
    /// through it. Terminate drop policy: an MCP server is an exclusive,
    /// long-lived child, so teardown wants deterministic whole-tree
    /// reclamation, NOT the shell tool's survivor escape hatch
    /// (crate::winjob::JobDrop contrasts the two). None when job setup
    /// failed → degrade to the old kill-the-direct-child.
    #[cfg(windows)]
    job: Option<crate::winjob::WinJob>,
    /// `None` once a write timed out / was cancelled / failed its spawn: the
    /// writer thread owned the handle and couldn't be reclaimed (it was stuck
    /// inside write()), so the child was killed to break the pipe. Subsequent
    /// calls fail fast on the `None` instead of hanging again on a dead pipe.
    stdin: Option<std::process::ChildStdin>,
    pending: Pending,
    /// Cleared in Drop so the shutdown-reaper stops referencing this child once
    /// it's reaped — a kill on a process-group id the OS later reused for an
    /// unrelated process would be catastrophic. Shared with the reaper thread.
    alive: Arc<AtomicBool>,
    /// The shutdown-reaper thread (spawned by `set_shutdown`), joined in Drop so
    /// it is guaranteed done BEFORE `wait()` reaps the pid. That closes the
    /// stale-pgid window: the reaper only ever kills while `alive` is true,
    /// i.e. while this transport still owns the live child. The reaper watches
    /// the **shutdown** flag (session end), NOT the per-turn `cancel` flag —
    /// killing the MCP child is irreversible, so it must never fire on a
    /// recoverable turn-interrupt.
    reaper: Option<std::thread::JoinHandle<()>>,
}

impl StdioTransport {
    /// windows/unix 双平台实现（各自的 spawn 形态差异太大，见下方两个 cfg
    /// 块）；第三平台（wasm 等）由紧随其后的 cfg 兜底版本返回明确错误——
    /// 参照 signal.rs 的 install_sigint_cancel 拆分法。
    #[cfg(any(unix, windows))]
    fn spawn(cfg: &McpServerConfig) -> Result<Self> {
        use std::process::{Command, Stdio};
        let cmd = cfg.command.as_ref().expect("validated");
        // Spawn the server in its own process group (unix) so Drop can kill
        // the whole tree — `npx some-mcp-server` forks a node grandchild that
        // would otherwise be orphaned when we kill only the direct child
        // (same reasoning as the shell tool's process_group(0)).
        #[cfg(unix)]
        let child = {
            use std::os::unix::process::CommandExt;
            Command::new(cmd)
                .args(&cfg.args)
                .envs(&cfg.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
        };
        #[cfg(windows)]
        let child = {
            // Windows: npm 装出来的 bin 多是 .cmd 垫片（npx、各 MCP server），
            // CreateProcess 不会按 PATHEXT 解析 .cmd，直接 Command::new("npx")
            // 会 "program not found"。经 cmd /C 走 .cmd 解析（同 shell 工具的
            // 做法：/C 之后的整段 raw_arg 直传，引号由 windows_cmd_tail 按 cmd
            // 的规则拼好，绝不经过 std 的 MSVCRT 编码——std 的编码遇上 cmd /C
            // 的剥引号规则，会把「含空格命令 + 含空格/引号参数」的命令行打
            // 碎，见 windows_cmd_tail 的文档）。CREATE_NO_WINDOW：GUI 壳
            // （windows_subsystem="windows"）自身无控制台，spawn 控制台子进程
            // 默认闪 conhost 黑框；压掉窗口，stdio 管道不受影响，node 孙进程
            // 继承隐藏控制台不会二次弹窗。
            use std::os::windows::process::CommandExt;
            Command::new("cmd")
                .arg("/C")
                .raw_arg(windows_cmd_tail(cmd, &cfg.args))
                .envs(&cfg.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(0x0800_0000)
                .spawn()
        };
        let child =
            child.with_context(|| format!("cannot start MCP server '{}': {}", cfg.name, cmd))?;
        // Arm the Job Object the moment the process exists — the same
        // statement-after-spawn discipline as the shell tool's SpawnedChild:
        // cmd /C has the real server (node via npm's .cmd shim) as a
        // GRANDCHILD, and membership only propagates to processes created
        // AFTER the assign lands. Terminate policy: an MCP server is an
        // exclusive long-lived child, so Drop reclaims the tree
        // deterministically — no shell-style survivor escape hatch. Assign
        // failure (None) degrades to the old direct-child kill.
        #[cfg(windows)]
        let job = crate::winjob::WinJob::assign(&child, crate::winjob::JobDrop::Terminate);
        let mut this = Self {
            child,
            #[cfg(windows)]
            job,
            stdin: None,
            pending: pending_new(),
            alive: Arc::new(AtomicBool::new(true)),
            reaper: None,
        };
        if let Err(e) = this.wire_readers(&cfg.name) {
            // A half-built transport has no Drop coming — nothing was
            // returned — so tear the tree down by hand (kill_child: group
            // kill on unix, Job-Object terminate on Windows) before
            // failing, or a reader-thread spawn failure would orphan the
            // freshly started server. P2 fix: this path used to PANIC via
            // bare std::thread::spawn on thread-resource exhaustion,
            // taking the whole agent down with it; an Err is a
            // per-connection failure the caller already knows how to
            // handle (provider::CancellableSse::new made the same change).
            this.kill_child();
            let _ = this.child.wait();
            return Err(e);
        }
        Ok(this)
    }

    /// 第三平台（wasm 等）兜底：stdio 传输依赖 unix 进程组或 Windows
    /// Job Object，其余目标没有可用的子进程机制，明确报错而不是让 cfg
    /// 谓词失配在编译期炸开（同 signal.rs::install_sigint_cancel 的拆法）。
    #[cfg(not(any(unix, windows)))]
    fn spawn(_cfg: &McpServerConfig) -> Result<Self> {
        bail!("MCP stdio transport requires unix or windows");
    }

    /// Take the stdio pipes and start the stderr / stdout reader threads.
    /// Fallible ONLY in the thread spawn itself: thread-resource exhaustion
    /// is a per-connection condition, not a process-killing panic (see
    /// spawn's failure note). A failed spawn drops its closure — closing
    /// the pipe it captured — so nothing leaks on this path.
    fn wire_readers(&mut self, name: &str) -> Result<()> {
        // Capture (don't inherit) the server's stderr: inheriting would scribble
        // over the TUI's raw-mode screen, and null() (the old behavior) threw
        // away the exact diagnostics needed when a server dies at startup.
        // Capped so a chatty server can't grow memory without bound.
        let stderr_buf: Arc<std::sync::Mutex<String>> = Default::default();
        {
            let mut err_pipe = self.child.stderr.take().expect("stderr piped");
            let stderr_buf = stderr_buf.clone();
            std::thread::Builder::new()
                .name("mcp-stdio-stderr".into())
                .spawn(move || {
                    use std::io::Read;
                    const CAP: usize = 8 * 1024;
                    let mut chunk = [0u8; 4096];
                    loop {
                        match err_pipe.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let mut buf = stderr_buf.lock().unwrap_or_else(|e| e.into_inner());
                                if buf.len() < CAP {
                                    let room = CAP - buf.len();
                                    buf.push_str(&String::from_utf8_lossy(&chunk[..n.min(room)]));
                                }
                            }
                        }
                    }
                })
                .with_context(|| format!("cannot spawn stderr reader for MCP server '{name}'"))?;
        }

        let stdout = self.child.stdout.take().expect("stdout piped");
        let stdin = self.child.stdin.take().expect("stdin piped");
        let pending = self.pending.clone();
        let fail_detail = stderr_buf.clone();
        // Owned clone for the reader thread's diagnostics: several MCP servers
        // can run side by side, and a "read stopped" warning without the
        // server's name can't be attributed to the right config entry.
        let warn_name = name.to_string();
        std::thread::Builder::new()
            .name("mcp-stdio-stdout".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    buf.clear();
                    match read_capped_line(&mut reader, &mut buf, MCP_LINE_MAX) {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            // Validate UTF-8 + parse here (the capped reader
                            // accumulates raw bytes); an unparseable line is
                            // skipped, matching the old read_line behavior.
                            if let Ok(s) = std::str::from_utf8(&buf) {
                                if let Ok(msg) = serde_json::from_str::<Value>(s.trim()) {
                                    pending_dispatch(&pending, msg);
                                }
                            }
                        }
                        Err(e) => {
                            // A line over the cap (or a read error) ends the
                            // stream: fail every waiter so callers don't sit
                            // out the full CALL_TIMEOUT on a dead/abusive server.
                            crate::diag::diag_warn!(
                                "[warn] MCP '{warn_name}' stdio read stopped: {e:#}"
                            );
                            break;
                        }
                    }
                }
                let detail = fail_detail
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                pending_fail_all(&pending, &detail);
            })
            .with_context(|| format!("cannot spawn stdout reader for MCP server '{name}'"))?;
        self.stdin = Some(stdin);
        Ok(())
    }

    /// Kill the server child — its whole process group on unix / its whole
    /// Job Object on Windows. Shared by `Drop` (routine teardown) and the
    /// failed-write path in request/notify: a writer thread stuck inside
    /// write() only exits once the pipe breaks, and the child's death is
    /// what breaks it (so no permanently blocked writer thread leaks).
    /// ESRCH (already dead/reaped) is harmless on every branch.
    fn kill_child(&mut self) {
        // Kill the whole process group on unix (spawn made the child a group
        // leader, so its pid IS the group id): a plain child.kill() would
        // orphan grandchildren the server forked. On Windows the same tree
        // kill runs through the Job Object armed at spawn — the direct child
        // is just the cmd /C wrapper, and the REAL server is the grandchild
        // only TerminateJobObject reaches. No job (assign failed at spawn)
        // degrades to the old kill-the-direct-child.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            match self.job.as_ref() {
                Some(job) => job.terminate(),
                None => {
                    let _ = self.child.kill();
                }
            }
        }
    }
}

/// Spawn the shutdown-reaper used by `StdioTransport::set_shutdown`: a worker
/// that kills the child's whole process group the moment `shutdown` fires, then
/// exits. Factored out so the kill-on-shutdown behavior is unit-testable with a
/// real subprocess. `alive` is cleared by `StdioTransport::drop` to stand the
/// reaper down once the child is reaped (so it never kills a process-group id
/// the OS has reused for an unrelated process).
///
/// **Why `shutdown`, not the run's `cancel` flag**: `cancel` is overloaded —
/// both "interrupt the current turn" (TUI Esc/Ctrl-C, recoverable: the next
/// turn reuses the same MCP server) and "the session is ending". Killing the
/// MCP child is irreversible (no server restart), so the reaper must only fire
/// on the latter. `shutdown` is a separate signal the frontend sets ONLY on
/// session end (TUI quit), never on a per-turn cancel.
#[cfg(unix)]
fn spawn_reaper(
    pgid: i32,
    alive: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("mcp-reaper".into())
        .spawn(move || {
            while alive.load(Ordering::Relaxed) {
                if shutdown.load(Ordering::Relaxed) {
                    // ESRCH (already reaped) is harmless.
                    unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // alive went false → transport is dropping; stand down.
        })
        // P2 fix: .expect panicked on thread-resource exhaustion — inside
        // set_shutdown, mid-session, killing the whole agent. An Err is the
        // same per-connection policy as the stdio/SSE reader threads: no
        // reaper means teardown falls back to Drop alone, which is degraded
        // but survivable — strictly better than a crash.
        .context("cannot spawn mcp-reaper thread")
}

/// Windows twin of [`spawn_reaper`]: a worker that terminates the child's
/// whole Job Object the moment `shutdown` fires, then exits — the loop body
/// is line-for-line the unix one (poll alive/shutdown every 100ms, kill on
/// shutdown, stand down on alive-clear). `job` is a
/// [`crate::winjob::WinJob::duplicate`] of the spawn-armed original, so the
/// kill reaches the identical tree — TerminateJobObject flattens the job
/// through ANY handle to it — while the original keeps its Terminate-on-drop
/// policy as the deterministic reclaimer of record. `alive` is cleared by
/// `StdioTransport::drop` to stand the reaper down BEFORE the transport's
/// own kill+wait runs: same stand-down-first contract as unix (there it also
/// guards against a reused pgid; on Windows it orders the two kills and lets
/// Drop's join reclaim the thread). Factored out so the kill-on-shutdown
/// behavior is unit-testable with a real subprocess, mirroring spawn_reaper.
#[cfg(windows)]
fn spawn_reaper_job(
    job: crate::winjob::WinJob,
    alive: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("mcp-reaper".into())
        .spawn(move || {
            while alive.load(Ordering::Relaxed) {
                if shutdown.load(Ordering::Relaxed) {
                    // Terminating an already-dead job is harmless — the
                    // ESRCH analogue (see WinJob::terminate).
                    job.terminate();
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // alive went false → transport is dropping; stand down.
        })
        // Same P2 policy as spawn_reaper: an Err degrades to Drop-only
        // teardown (survivable), never a mid-session panic.
        .context("cannot spawn mcp-reaper thread")
}

impl Transport for StdioTransport {
    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value> {
        // Fast-fail once the write path is gone: a previous write that timed
        // out or was cancelled left its writer thread stuck holding the only
        // stdin handle (and killed the child) — writing again against the
        // dead pipe would just hang the turn a second time.
        let stdin = match self.stdin.take() {
            Some(s) => s,
            None => bail!(
                "MCP stdio write path is gone (a previous write timed out, was \
                 cancelled, or failed; the server was killed); cannot call '{method}'"
            ),
        };
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        // Reclaim the pending slot on any failure (write error / timeout /
        // cancel): otherwise a flaky server that times out on every call
        // leaks one map entry per call and grows without bound over a long
        // session. A cancelled call reclaims here too; the late response (if
        // any) is dropped by pending_dispatch.
        let mut frame_bytes = frame(id, method, params).to_string().into_bytes();
        frame_bytes.push(b'\n');
        let res = match write_frame_cancelable(stdin, &frame_bytes, WRITE_TIMEOUT, cancel) {
            Ok(w) => {
                self.stdin = Some(w);
                recv_cancelable(&rx, method, cancel).and_then(|msg| extract_result(msg, method))
            }
            Err(e) => {
                // The writer thread is stuck inside write() holding stdin (or
                // the pipe already broke) — killing the child breaks the pipe,
                // which fails that blocked write and ends the thread: no
                // permanently blocked writer leaks. Note the asymmetry with
                // the RECEIVE-side cancel in recv_cancelable: a cancelled
                // RECV leaves the server alive for the next turn (its stdout
                // may still answer later calls), but a cancelled/timed-out
                // WRITE cannot — our writer owns stdin and is unreclaimable
                // while blocked, so the transport is dead either way. The
                // kill is on the transport, NOT the shutdown reaper: this is
                // an unrecoverable dead pipe, not a session-end signal.
                self.kill_child();
                Err(e)
            }
        };
        if res.is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
        }
        res
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        // Same wedged-write protection as request(): notify frames are tiny
        // today (the handshake's notifications/initialized), but the
        // pipe-full hang shape is identical, and no cancel flag exists here
        // (handshake-time call, no run in flight) — only the deadline guards.
        let stdin = match self.stdin.take() {
            Some(s) => s,
            None => bail!(
                "MCP stdio write path is gone (a previous write timed out, was \
                 cancelled, or failed; the server was killed); cannot send '{method}'"
            ),
        };
        let mut bytes = notify_frame(method, params).to_string().into_bytes();
        bytes.push(b'\n');
        match write_frame_cancelable(stdin, &bytes, WRITE_TIMEOUT, /* cancel: */ None) {
            Ok(w) => {
                self.stdin = Some(w);
                Ok(())
            }
            Err(e) => {
                // Same kill-to-unblock-the-writer rationale as request().
                self.kill_child();
                Err(e)
            }
        }
    }

    /// Arm shutdown-aware teardown: spawn a reaper that kills the child's whole
    /// process group the moment **shutdown** (session end) fires. This unblocks
    /// a stuck MCP call on quit — the worker is blocked in `request()` waiting
    /// on a server that never responds; killing the child → its stdout EOFs →
    /// the reader thread exits → `pending_fail_all` → the blocked `request()`
    /// returns Err → the worker proceeds, the run ends, the agent is dropped,
    /// and `Drop` kills+reaps cleanly. Without this, a TUI quit's bounded join
    /// (3s) times out while the worker is stuck in a 120s `CALL_TIMEOUT`, the
    /// worker detaches with the agent undropped, and the MCP server child is
    /// orphaned (reparented to init). Idempotent: arming twice is a no-op.
    ///
    /// **`shutdown`, not `cancel`**: the run's `cancel` flag is also raised on
    /// a recoverable per-turn interrupt (TUI Esc / Ctrl-C), where the next turn
    /// reuses the same MCP server. Killing the child there is a correctness
    /// defect (no restart) — so the reaper is armed on a SEPARATE shutdown
    /// signal the frontend sets ONLY on session end, never on a turn cancel.
    fn set_shutdown(&mut self, shutdown: &Arc<AtomicBool>) {
        if self.reaper.is_some() {
            return;
        }
        #[cfg(unix)]
        match spawn_reaper(self.child.id() as i32, self.alive.clone(), shutdown.clone()) {
            Ok(handle) => self.reaper = Some(handle),
            Err(e) => {
                // Degraded, not dead: without the reaper a quit-time stuck
                // read unblocks only via Drop (or process exit) instead of
                // the instant group kill — warn and carry on.
                crate::diag::diag_warn!(
                    "[warn] MCP: cannot spawn shutdown reaper ({e:#}); quit will rely on Drop teardown"
                );
            }
        }
        // Windows twin of the arm above: the reaper kills the tree through a
        // DUPLICATED job handle — TerminateJobObject via any handle to the
        // job flattens the same tree — the moment shutdown fires, giving the
        // same instant quit-time unblock as unix instead of waiting for this
        // process to exit and KILL_ON_JOB_CLOSE to fire (the old
        // orphan-until-host-exit story). The ORIGINAL job handle keeps its
        // Terminate-on-drop policy and remains the deterministic reclaimer; a
        // missing job (assign failed at spawn) or a failed thread spawn
        // degrades exactly like the unix failure branch: warn, rely on Drop
        // teardown. The two degraded flavors get SEPARATE messages — "assign
        // failed at spawn" and "handle duplication failed" point at different
        // layers when triaging a quit that relied on Drop.
        #[cfg(windows)]
        {
            let dup = match self.job.as_ref() {
                Some(job) => job.duplicate(),
                None => {
                    crate::diag::diag_warn!(
                        "[warn] MCP: shutdown reaper not armed (no Job Object — assign failed at spawn); quit will rely on Drop teardown"
                    );
                    return;
                }
            };
            let dup = match dup {
                Some(d) => d,
                None => {
                    crate::diag::diag_warn!(
                        "[warn] MCP: shutdown reaper not armed (Job Object handle duplication failed); quit will rely on Drop teardown"
                    );
                    return;
                }
            };
            match spawn_reaper_job(dup, self.alive.clone(), shutdown.clone()) {
                Ok(handle) => self.reaper = Some(handle),
                Err(e) => {
                    // Degraded, not dead: same shape as the unix branch above —
                    // without the reaper a quit-time stuck read unblocks only via
                    // Drop (or process exit) instead of the instant job kill.
                    crate::diag::diag_warn!(
                        "[warn] MCP: cannot spawn shutdown reaper ({e:#}); quit will rely on Drop teardown"
                    );
                }
            }
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        // Stand the shutdown-reaper down BEFORE reaping. Setting alive=false
        // makes its loop exit; joining guarantees it has fully exited before
        // `wait()` reaps the pid below — so the reaper can never kill a
        // process-group id the OS has already reused for another process. The
        // reaper only kills while alive is true, i.e. while this transport
        // still owns the live child.
        self.alive.store(false, Ordering::Relaxed);
        if let Some(handle) = self.reaper.take() {
            let _ = handle.join();
        }
        // Kill + reap (kill_child: group kill on unix, Job-Object tree kill
        // on Windows — shared with the failed-write path). If a write timed
        // out earlier, the child is already dead and this is a harmless
        // re-kill; `wait()` reaps it (and the already-detached writer
        // thread, if its blocked write hadn't failed yet, exits on the
        // broken pipe here). On Windows the job field's own Drop (Terminate
        // policy) then fires right after as the deterministic backstop —
        // re-terminating a dead job is the harmless no-op unix's ESRCH is.
        self.kill_child();
        let _ = self.child.wait();
    }
}

// ─── Streamable HTTP ────────────────────────────────────────────────

/// Tear a stream reader thread down with a BOUNDED wait: set the stop flag,
/// poll for exit in 50ms slices (at most [`READER_JOIN_WAIT`]), join if it
/// finished. The reader's blocking read cannot be interrupted, but every read
/// is bounded by the round timeout ([`STREAM_ROUND`]) — worst case the thread
/// wakes at its current round's deadline, sees the flag and exits, so a
/// detached thread + its TCP connection now self-reclaim within ~one round +
/// [`STREAM_BACKOFF`] instead of living until process exit. Detaching stays
/// silent: the residual leak is bounded, and routine teardown of a healthy
/// (mid-round) reader must neither hang nor spam a warning.
fn shutdown_stream_reader(shutdown: &AtomicBool, reader: &mut Option<std::thread::JoinHandle<()>>) {
    shutdown.store(true, Ordering::Relaxed);
    if let Some(handle) = reader.take() {
        let deadline = Instant::now() + READER_JOIN_WAIT;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
        // Detached: nothing more to do — the reader self-exits at its
        // current round's deadline (see doc).
    }
}

struct HttpTransport {
    client: reqwest::blocking::Client,
    url: String,
    headers: HashMap<String, String>,
    session_id: Option<String>,
    /// Shared response-dispatch table (the same plumbing stdio / legacy SSE
    /// use): rounds of the standing standalone stream dispatch every JSON-RPC
    /// response into the slot the requesting call registered, so a 202's
    /// response is deliverable the moment the server pushes it — SSE does not
    /// queue for a client that hasn't connected, which is exactly the race
    /// the standing stream exists to close (see [`HttpTransport::request`]).
    pending: Pending,
    /// Stop flag for the standing-stream reader thread; set in Drop.
    stream_shutdown: Arc<AtomicBool>,
    /// The standing standalone-stream reader thread (see
    /// [`spawn_standalone_stream`]). `None` until first needed.
    stream_reader: Option<std::thread::JoinHandle<()>>,
    /// Set by the reader thread when the server 4xx-REJECTED the standalone
    /// GET. Damps the pre-POST arming to one rejection per session instead of
    /// one wasted GET + warning per request against a server without
    /// standalone-stream support; `force` in [`Self::ensure_standalone_stream`]
    /// (the 202 branch) still earns a fresh attempt per 202 — matching the old
    /// one-GET-per-202 wire traffic where the fast 4xx error came from.
    standalone_rejected: Arc<AtomicBool>,
    /// Per-round read deadline handed to the standing-stream reader; defaults
    /// to [`STREAM_ROUND`]. A field (not a constant read at the spawn site)
    /// so tests can inject a small round and observe rotations/teardown in
    /// milliseconds.
    stream_round: Duration,
}

/// Read at most `cap` bytes of a response body as lossy text. The SSE paths
/// were always bounded (MCP_EVENT_MAX via `read_sse_event`); the plain-JSON
/// and error-body paths used to slurp the whole server-chosen body
/// (`resp.json()` buffers it all before parsing).
fn capped_text(resp: &mut reqwest::blocking::Response, cap: u64) -> String {
    use std::io::Read;
    let mut buf = Vec::new();
    let _ = resp.take(cap).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

impl HttpTransport {
    fn new(url: &str, cfg: &McpServerConfig) -> Result<Self> {
        Ok(Self {
            client: http_client(CALL_TIMEOUT),
            url: url.to_string(),
            headers: cfg.headers.clone(),
            session_id: None,
            pending: pending_new(),
            stream_shutdown: Arc::new(AtomicBool::new(false)),
            stream_reader: None,
            standalone_rejected: Arc::new(AtomicBool::new(false)),
            stream_round: STREAM_ROUND,
        })
    }

    /// Make sure exactly one standing standalone-stream reader is running,
    /// (re)spawning it if none is alive. Idempotent while the thread lives; a
    /// thread that EXITED (its GET was 4xx-rejected — a server without
    /// standalone-stream support, or one that rotated the session id) earns
    /// one fresh attempt per future need. `force` bypasses the rejection
    /// damper: the 202 branch MUST try (its response has no other delivery
    /// path), while the pre-POST arming (best-effort race insurance) skips
    /// re-spawning against a server that already said no. The session id is
    /// snapshotted at spawn: the spec keeps one session id stable for the
    /// connection's lifetime, and a server that breaks that kills this stream
    /// with a 4xx — the respawn then carries the refreshed id and heals.
    fn ensure_standalone_stream(&mut self, force: bool) -> Result<()> {
        if self
            .stream_reader
            .as_ref()
            .is_some_and(|h| !h.is_finished())
        {
            return Ok(());
        }
        if !force && self.standalone_rejected.load(Ordering::Relaxed) {
            return Ok(()); // best-effort arming only — the 202 branch retries
        }
        let reader = spawn_standalone_stream(
            self.url.clone(),
            self.headers.clone(),
            self.session_id.clone(),
            self.pending.clone(),
            self.stream_shutdown.clone(),
            self.standalone_rejected.clone(),
            self.stream_round,
        )?;
        self.stream_reader = Some(reader);
        Ok(())
    }

    fn post(&self, body: &Value) -> Result<reqwest::blocking::Response> {
        let mut req = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let mut resp = req.json(body).send().context("MCP HTTP POST failed")?;
        if !resp.status().is_success() && resp.status() != 202 {
            let status = resp.status();
            // P2-3: the error body's size is the server's choice and only
            // feeds a diagnostic string — cap it.
            let text = capped_text(&mut resp, MCP_ERR_BODY_MAX);
            bail!("MCP HTTP error {status}: {text}");
        }
        Ok(resp)
    }

    /// Open the standalone server→client SSE stream on the endpoint (spec
    /// 2025-03-26 Streamable HTTP, "Listening for messages from the server"):
    /// a GET with `Accept: text/event-stream`, carrying `Mcp-Session-Id`
    /// when a session was established ("clients ... MUST include it in the
    /// Mcp-Session-Id header on all of their subsequent HTTP requests").
    /// Uses the SAME timeout-bounded client as the POST, so the read can't
    /// outlive CALL_TIMEOUT — the consumer (sse_response_for_id) returns the
    /// moment the matching response arrives, and a server that never sends
    /// one fails at the timeout instead of hanging the caller forever.
    fn get_event_stream(&self) -> Result<reqwest::blocking::Response> {
        let mut req = self
            .client
            .get(&self.url)
            .header("accept", "text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let mut resp = req.send().context("MCP HTTP GET (SSE stream) failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            // Same error-body cap and rationale as post().
            let text = capped_text(&mut resp, MCP_ERR_BODY_MAX);
            bail!("MCP HTTP GET error {status}: {text}");
        }
        Ok(resp)
    }
}

impl Transport for HttpTransport {
    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        // Cancel is honored on the 202 wait via recv_cancelable's 50ms slices
        // (same contract as stdio / SSE). The direct POST/GET round-trips are
        // blocking reqwest calls already bounded by the client's
        // CALL_TIMEOUT total timeout — there is no channel wait to slice on
        // those. The standing-stream reader is likewise unblockable mid-read,
        // but every read is round-bounded (STREAM_ROUND), so teardown joins
        // it within ~one round (see shutdown_stream_reader).
        cancel: Option<&AtomicBool>,
    ) -> Result<Value> {
        // Register the pending slot BEFORE the POST: a server that answers
        // with 202 delivers the response on the standing stream whenever it
        // pleases — including between our POST and the wait below — so the
        // slot must already exist when that push lands.
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        // With a session established, connect the standing stream BEFORE the
        // POST leaves: SSE does not queue for an unconnected client, so a
        // 202 response the server pushes the instant it processes the POST
        // is only deliverable if the GET is already open. The old flow
        // (POST → drop(resp) → only THEN open a fresh GET per 202) lost any
        // such response permanently and burned the full CALL_TIMEOUT; in
        // Auto mode an initialize hitting that path mis-probed Streamable as
        // broken and fell back to legacy SSE. Failure to arm the stream is
        // NOT fatal here — the 202 branch below still has the inline
        // one-shot GET as fallback.
        if self.session_id.is_some() {
            let _ = self.ensure_standalone_stream(/* force: */ false);
        }
        let res = (|| -> Result<Value> {
            let resp = self.post(&frame(id, method, params))?;
            if let Some(sid) = resp
                .headers()
                .get("Mcp-Session-Id")
                .and_then(|v| v.to_str().ok())
            {
                self.session_id = Some(sid.to_string());
            }

            if resp.status() == 202 {
                // Spec (2025-03-26 Streamable HTTP): the server MAY answer a
                // POSTed request with `202 Accepted` and no body — the actual
                // JSON-RPC response then arrives on the server-initiated SSE
                // stream. That stream is the STANDING standalone GET (see
                // spawn_standalone_stream): one long-lived connection per
                // session, opened before the POST that might need it and
                // shared by every 202, with responses dispatched through the
                // shared pending table. The wait is therefore cancel-aware.
                // If arming the stream failed (thread spawn — resource
                // exhaustion), fall back to the old one-shot GET +
                // sse_response_for_id, byte-identical to the previous
                // behavior.
                drop(resp); // release the bodyless 202 connection
                if self.ensure_standalone_stream(/* force: */ true).is_ok() {
                    let msg = recv_cancelable(&rx, method, cancel)?;
                    return extract_result(msg, method);
                }
                let stream = self.get_event_stream()?;
                let mut reader = std::io::BufReader::new(stream);
                let msg = sse_response_for_id(&mut reader, id, method)?;
                return extract_result(msg, method);
            }

            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            if ct.contains("text/event-stream") {
                // Read SSE events until the response with our id arrives. Use
                // the multi-line-aware event reader: provider::SseReader
                // takes only the first `data:` line of a multi-line event and
                // would drop the rest, losing large JSON-RPC results. The
                // id+method response match lives in sse_response_for_id (see
                // its doc for the guard).
                let mut reader = std::io::BufReader::new(resp);
                let msg = sse_response_for_id(&mut reader, id, method)?;
                extract_result(msg, method)
            } else {
                // Plain-JSON response. P2-3: read capped, then parse —
                // resp.json() buffers the entire body before parsing, and
                // the body's size is the server's choice (MCP_EVENT_MAX
                // already bounds the SSE variant of the same result; a body
                // cut at the cap fails the parse, which is the right outcome
                // for a >64MiB result).
                let mut resp = resp;
                let text = capped_text(&mut resp, MCP_EVENT_MAX as u64);
                let msg: Value =
                    serde_json::from_str(&text).context("MCP HTTP: invalid JSON response")?;
                extract_result(msg, method)
            }
        })();
        // Reclaim the slot on every exit (success included): a late duplicate
        // delivery on the standing stream must find no entry (and be dropped
        // by pending_dispatch) instead of leaking one map entry per call.
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        res
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        // Spec: server answers 202 Accepted (no body) for notifications.
        self.post(&notify_frame(method, params))?;
        Ok(())
    }
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        // Bounded teardown: set the stop flag and wait up to
        // READER_JOIN_WAIT for the reader to exit. A read blocked on a
        // healthy stream wakes at the current round's deadline (STREAM_ROUND)
        // at the latest, sees the flag and exits — so the usual case is a
        // join within ~one round; the rare over-wait detaches silently, and
        // the detached thread still self-reclaims (its TCP connection
        // included) within ~one round + backoff instead of surviving until
        // process exit.
        shutdown_stream_reader(&self.stream_shutdown, &mut self.stream_reader);
    }
}

/// Outcome of one standalone-stream round (connection attempt + read pass).
enum StandaloneRound {
    /// The round deadline expired on a HEALTHY stream: rotate at once —
    /// reconnect immediately, no backoff. This is the standing stream's
    /// routine cadence now (the round bound is what keeps teardown and the
    /// delivery path bounded), and skipping the sleep keeps the window where
    /// a server push finds no connected stream down to a reconnect RTT. A
    /// push that lands in that window is only recovered if the SERVER
    /// re-sends it on the fresh GET (no Last-Event-ID, no protocol re-send
    /// obligation — see [`STREAM_ROUND`]).
    Expired,
    /// The stream ended early (server closed it / read error) or the connect
    /// failed: retry after a backoff — waiters may still be owed responses,
    /// and the next server push needs a connected stream to land on.
    Ended,
    /// A 4xx rejection: the server refuses standalone streams (405 = not
    /// supported) or the session id is dead (401/404). Retrying can't heal a
    /// client-side rejection, so the caller gives up permanently.
    Rejected(String),
}

/// The standing standalone GET stream (spec 2025-03-26, "Listening for
/// messages from the server"), supervised by one reader thread per
/// [`HttpTransport`] session: connect once, read SSE events forever,
/// dispatch every JSON-RPC response into the shared pending table (the same
/// plumbing as stdio / legacy SSE — `pending_dispatch`'s `method` guard
/// drops server-initiated requests). This closes the 202 race: SSE does not
/// queue for an unconnected client, so the old open-a-fresh-GET-after-each-
/// 202 flow permanently lost any response the server pushed before that GET
/// connected (the caller then sat out the full CALL_TIMEOUT).
///
/// Residual race, honestly recorded: the very FIRST 202 (an initialize to a
/// server that 202s it) can only open the stream after the POST returns — a
/// server that pushes that response before any client GET could connect
/// still loses it. Post-initialize requests all carry a session id, so
/// request() arms the stream BEFORE their POSTs and are fully covered.
///
/// On stream end / error the loop reconnects after a short backoff (a
/// restarting server or an idle-connection-dropping proxy must not doom
/// later 202s); a HEALTHY stream now rotates on its own — every GET carries
/// the round timeout, so the read unblocks at least once per [`STREAM_ROUND`]
/// and the loop reconnects immediately, without the backoff (the stream was
/// healthy; the rotation must re-open the delivery path at once). On a 4xx it
/// fails every waiter immediately (so a 202 parked
/// on this stream errors at once — the old inline path failed fast on the
/// same 4xx and must not regress into a timeout) and exits permanently; the
/// next ensure_standalone_stream() gives it one fresh attempt.
fn spawn_standalone_stream(
    url: String,
    headers: HashMap<String, String>,
    session_id: Option<String>,
    pending: Pending,
    shutdown: Arc<AtomicBool>,
    rejected: Arc<AtomicBool>,
    round: Duration,
) -> Result<std::thread::JoinHandle<()>> {
    // Same builder shape and no-redirect rationale as SseTransport::connect:
    // connect timeout ONLY (a total timeout would kill a healthy idle
    // stream at CALL_TIMEOUT), and a redirect-following client would carry
    // the configured headers (possibly Authorization) off-origin.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| {
            reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client build")
        });
    std::thread::Builder::new()
        .name("mcp-http-stream".into())
        .spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                match standalone_stream_round(
                    &client,
                    &url,
                    &headers,
                    &session_id,
                    &pending,
                    &shutdown,
                    round,
                ) {
                    StandaloneRound::Rejected(detail) => {
                        // Latch the rejection: the transport's best-effort
                        // pre-POST arming stops re-spawning this thread (one
                        // wasted GET + warning per request would be pure
                        // noise against a server that simply doesn't offer
                        // standalone streams); a future 202 forces one fresh
                        // attempt (ensure_standalone_stream).
                        rejected.store(true, Ordering::Relaxed);
                        crate::diag::diag_warn!(
                            "[warn] MCP HTTP standalone stream rejected ({detail}); \
                             standing stream disabled"
                        );
                        // Fail every waiter NOW so a 202 parked on this stream
                        // errors immediately instead of sitting out the full
                        // CALL_TIMEOUT. Inlined rather than pending_fail_all:
                        // that helper's "its stderr" wording is a stdio-ism —
                        // the standalone stream's failure reason is right here.
                        let err = json!({
                            "error": {"code": -32000, "message": format!("MCP standalone stream rejected ({detail})")}
                        });
                        for (_, tx) in pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .drain()
                        {
                            let _ = tx.send(err.clone());
                        }
                        return;
                    }
                    StandaloneRound::Expired => {
                        // Healthy-stream rotation: reconnect at once — no
                        // backoff (see StandaloneRound::Expired). The loop
                        // condition re-checks shutdown before the next GET,
                        // so a dropped transport never re-opens a connection.
                        continue;
                    }
                    StandaloneRound::Ended => {}
                }
                // Backoff before reconnecting/retrying — ONLY the early-end
                // path lands here (deadline rotations `continue` above): a
                // stream that merely reached its round deadline was healthy
                // and must not be penalized with a dead gap. The backoff
                // keeps a genuinely dead server from being hot-looped
                // (waiters are bounded by CALL_TIMEOUT regardless) and
                // staggers naturally behind a restarting server. The loop
                // condition then re-checks shutdown, so a dropped transport
                // never opens one more connection on its way out.
                std::thread::sleep(STREAM_BACKOFF);
            }
        })
        // Same policy as every other reader thread here: thread-resource
        // exhaustion is a per-connection failure — the caller degrades to
        // the inline one-shot GET, never a panic.
        .context("cannot spawn MCP standalone stream reader thread")
}

/// One connection + read pass of the standing stream (loop body of
/// [`spawn_standalone_stream`]). Sends the GET with the SSE Accept header
/// and `Mcp-Session-Id` when one was established ("clients ... MUST include
/// it in the Mcp-Session-Id header on all of their subsequent HTTP
/// requests"), then reads events until the stream ends, the read errors, or
/// the ROUND deadline expires. The request-level timeout bounds every read,
/// so a healthy stream yields [`StandaloneRound::Expired`] at least once per
/// [`STREAM_ROUND`] (rotation cadence) and a wedged read cannot outlive it —
/// the bound that keeps teardown bounded.
fn standalone_stream_round(
    client: &reqwest::blocking::Client,
    url: &str,
    headers: &HashMap<String, String>,
    session_id: &Option<String>,
    pending: &Pending,
    shutdown: &AtomicBool,
    round: Duration,
) -> StandaloneRound {
    let mut req = client
        .get(url)
        .header("accept", "text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    if let Some(sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }
    // The round timeout: reqwest applies a request-level timeout to the
    // blocking Response's every read (and the async body's total deadline),
    // so this single bound both rotates a healthy stream and wakes a wedged
    // read. Expiry surfaces as an io::Error wrapping a reqwest::Error —
    // classified by is_stream_round_timeout below, never by ErrorKind
    // (into_io pins that to Other).
    let mut resp = match req.timeout(round).send() {
        Ok(r) => r,
        Err(e) => {
            // Connect/network failure (server down, proxy reset): retry —
            // the standing stream is the delivery channel for future 202s,
            // and the server coming back revives it.
            crate::diag::diag_warn!(
                "[warn] MCP HTTP standalone stream connect failed ({e:#}); retrying"
            );
            return StandaloneRound::Ended;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        // Same error-body cap and rationale as post()/get_event_stream().
        let text = capped_text(&mut resp, MCP_ERR_BODY_MAX);
        if status.is_client_error() {
            return StandaloneRound::Rejected(format!("HTTP {status}: {text}"));
        }
        // 5xx: transient server-side — retry after backoff.
        crate::diag::diag_warn!("[warn] MCP HTTP standalone stream GET error {status}; retrying");
        return StandaloneRound::Ended;
    }
    let mut reader = std::io::BufReader::new(resp);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return StandaloneRound::Ended;
        }
        match read_sse_event(&mut reader, MCP_EVENT_MAX) {
            Ok(Some((_event, data))) => {
                if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                    pending_dispatch(pending, msg);
                }
            }
            Ok(None) => return StandaloneRound::Ended, // server closed: reconnect
            Err(e) => {
                // Round deadline on a healthy stream: silent rotation (no
                // diag — this is the routine cadence now, and a warning per
                // round would be pure noise). Anything else — a line over
                // the cap or a real read error — keeps the bounded-memory /
                // anti-runaway policy: warn, and reconnect since later 202s
                // may still be owed.
                if is_stream_round_timeout(&e) {
                    return StandaloneRound::Expired;
                }
                crate::diag::diag_warn!(
                    "[warn] MCP HTTP standalone stream read stopped: {e:#}; reconnecting"
                );
                return StandaloneRound::Ended;
            }
        }
    }
}

/// Is this SSE read failure the ROUND deadline expiring (as opposed to a
/// real read error)? reqwest surfaces a request-timeout read failure as
/// `io::Error::new(ErrorKind::Other, reqwest::Error)` (its `into_io`
/// conversion pins the kind to Other, so the ErrorKind is useless for
/// discrimination); only the INNER reqwest::Error knows, via `is_timeout()`.
/// The anyhow chain is walked because `read_capped_line` wraps the io error
/// in a context. Shape pinned by a probe test (`stream_round_timeout_…`);
/// if a reqwest upgrade changes the shape, the probe goes red — this
/// classification must never mistake a deadline rotation for a real error
/// (or vice versa).
fn is_stream_round_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
            .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
            .is_some_and(|re| re.is_timeout())
    })
}

// ─── legacy HTTP+SSE ────────────────────────────────────────────────

struct SseTransport {
    client: reqwest::blocking::Client,
    /// The verified POST target as a PARSED Url — the same object that passed
    /// the same-origin check in [`resolve_endpoint`] is what gets sent, so no
    /// string-level re-parse can ever disagree with what was checked (the
    /// external-review P1 fix: reqwest re-parses a String through the WHATWG
    /// `url` rules, which differ from any hand-rolled origin split).
    post_url: reqwest::Url,
    headers: HashMap<String, String>,
    pending: Pending,
    /// Signals the reader thread to stop at its next loop iteration.
    shutdown: Arc<AtomicBool>,
    /// The reader thread's handle, kept so Drop can reclaim it with a bounded
    /// wait (join when it exits in time, silent detach otherwise). Held for
    /// observability/lifecycle; the reads it blocks in cannot be interrupted,
    /// but each one is round-bounded (STREAM_ROUND) — see Drop.
    reader: Option<std::thread::JoinHandle<()>>,
}

impl SseTransport {
    fn connect(url: &str, cfg: &McpServerConfig) -> Result<Self> {
        Self::connect_with_round(url, cfg, STREAM_ROUND)
    }

    /// `round` exists for tests (small rounds make rotations and bounded
    /// teardown observable in milliseconds); production always passes
    /// [`STREAM_ROUND`].
    fn connect_with_round(url: &str, cfg: &McpServerConfig, round: Duration) -> Result<Self> {
        // The GET is a long-lived stream: connect timeout only, no client
        // total timeout — the round bound is applied per request below.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| {
                // Same key-leak rationale as http_client(): this GET carries
                // cfg.headers (possibly Authorization), so the fallback MUST
                // re-apply the no-redirect policy — a bare Client::new() follows
                // redirects and would reintroduce the documented leak vector.
                reqwest::blocking::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .expect("reqwest client build")
            });
        // The GET is a long-lived stream; drive it on a reader thread. One
        // ROUND per connection: every GET carries the round timeout, so a
        // healthy (silent) stream rotates on its own at least once per
        // [`STREAM_ROUND`] and a wedged read cannot outlive it (that bound is
        // what makes teardown bounded). Delivery semantics are unchanged: the
        // `endpoint` event is honored once per thread (reconnects ignore
        // repeats — the POST target is already fixed), and every non-round end
        // (stream end / real read error) keeps the existing fail-all-and-exit
        // behavior.
        let (endpoint_tx, endpoint_rx) = mpsc::channel::<Option<reqwest::Url>>();
        let pending = pending_new();
        let shutdown = Arc::new(AtomicBool::new(false));
        let reader = {
            let pending = pending.clone();
            let shutdown = shutdown.clone();
            let url = url.to_string();
            let headers = cfg.headers.clone();
            // Named for thread dumps: this thread can outlive the transport
            // by at most ~one round now (see Drop) — a name is still the
            // difference between a diagnosable stray and an anonymous one.
            let body = move || {
                let mut endpoint_sent = false;
                'rounds: loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut req = client.get(&url).header("accept", "text/event-stream");
                    for (k, v) in &headers {
                        req = req.header(k, v);
                    }
                    let Ok(resp) = req.timeout(round).send() else {
                        if endpoint_sent {
                            // A reconnect round's connect failed (server down
                            // at a rotation boundary): same treatment as the
                            // standing stream — warn + backoff + retry, since
                            // a routine rotation must not kill an otherwise
                            // healthy session.
                            crate::diag::diag_warn!(
                                "[warn] MCP SSE reconnect failed for {url}; retrying"
                            );
                            std::thread::sleep(STREAM_BACKOFF);
                            continue 'rounds;
                        }
                        // First connect failed: the transport never came up —
                        // fail every waiter (connect() surfaces it).
                        pending_fail_all(&pending, "");
                        return;
                    };
                    let mut reader = BufReader::new(resp);
                    loop {
                        if shutdown.load(Ordering::Relaxed) {
                            break 'rounds;
                        }
                        let (event, data) = match read_sse_event(&mut reader, MCP_EVENT_MAX) {
                            Ok(Some(ev)) => ev,
                            Ok(None) => break 'rounds, // stream ended
                            Err(e) => {
                                // Round deadline on a healthy stream: rotate
                                // at once, silently (see spawn_standalone_stream).
                                if is_stream_round_timeout(&e) {
                                    continue 'rounds;
                                }
                                // A line over the cap or a read error: the stream is
                                // unusable. Fail every waiter so callers don't block
                                // until CALL_TIMEOUT on a dead/abusive server. The
                                // URL names WHICH server's stream died — several MCP
                                // servers can be configured side by side.
                                crate::diag::diag_warn!(
                                    "[warn] MCP SSE read stopped for {url}: {e:#}"
                                );
                                pending_fail_all(&pending, "");
                                return;
                            }
                        };
                        match event.as_str() {
                            "endpoint" if !endpoint_sent => {
                                endpoint_sent = true;
                                // Resolve against the SSE URL; refuse cross-origin
                                // endpoints (SSRF guard on the authenticated POST).
                                match resolve_endpoint(&url, data.trim()) {
                                    Ok(u) => {
                                        let _ = endpoint_tx.send(Some(u));
                                    }
                                    Err(e) => {
                                        crate::diag::diag_warn!(
                                            "[warn] MCP SSE endpoint rejected: {e:#}"
                                        );
                                        let _ = endpoint_tx.send(None);
                                        pending_fail_all(&pending, "");
                                        return;
                                    }
                                }
                            }
                            "message" => {
                                if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                                    pending_dispatch(&pending, msg);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                pending_fail_all(&pending, "");
            };
            std::thread::Builder::new()
                .name("mcp-sse-reader".into())
                .spawn(body)
                // P2 fix: this used to .expect-panic. Thread-resource
                // exhaustion is a per-connection failure, not a
                // process-killing one — same policy as provider's
                // CancellableSse::new and the stdio reader threads. Without
                // the reader no response can ever be dispatched (every call
                // would hang to CALL_TIMEOUT), so there is nothing to
                // salvage: the connect fails and the caller surfaces it.
                .context("cannot spawn MCP SSE reader thread")?
        };
        let post_url = endpoint_rx
            .recv_timeout(CONNECT_TIMEOUT)
            .map_err(|e| {
                // Timed out or the channel died before the endpoint arrived:
                // tell the reader thread to stop. Its read() cannot be
                // interrupted mid-read, but every read is round-bounded
                // (STREAM_ROUND) — the thread self-exits at the current
                // round's deadline at the latest, so the residual thread +
                // TCP connection live ~one round, not until process exit.
                // Still worth a diagnostic: "never sent an endpoint" is a
                // server misbehavior the host should see.
                shutdown.store(true, Ordering::Relaxed);
                if !reader.is_finished() {
                    crate::diag::diag_warn!(
                        "[warn] MCP SSE: reader thread exiting late (server at {url} never \
                         sent an 'endpoint' event; the round-bounded GET self-exits within \
                         ~one round)"
                    );
                }
                anyhow!("MCP SSE: no 'endpoint' event from {url}: {e}")
            })?
            .ok_or_else(|| anyhow!("MCP SSE endpoint rejected (origin mismatch)"))?;
        Ok(Self {
            client: http_client(CALL_TIMEOUT),
            post_url,
            headers: cfg.headers.clone(),
            pending,
            shutdown,
            reader: Some(reader),
        })
    }

    fn post(&self, body: &Value) -> Result<()> {
        let mut req = self
            .client
            // clone: Url by value (reqwest's IntoUrl covers Url, not &Url) —
            // the fetched target stays byte-identical to the checked one.
            .post(self.post_url.clone())
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let mut resp = req.json(body).send().context("MCP SSE POST failed")?;
        if !resp.status().is_success() && resp.status() != 202 {
            let status = resp.status();
            // P2-3: same error-body cap as the Streamable HTTP transport.
            let text = capped_text(&mut resp, MCP_ERR_BODY_MAX);
            bail!("MCP SSE error {status}: {text}");
        }
        Ok(())
    }
}

impl Transport for SseTransport {
    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let res = (|| -> Result<Value> {
            self.post(&frame(id, method, params))?;
            let msg = recv_cancelable(&rx, method, cancel)?;
            extract_result(msg, method)
        })();
        if res.is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
        }
        res
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.post(&notify_frame(method, params))
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        // Bounded teardown (same contract as HttpTransport's): the reader's
        // blocking read cannot be interrupted, but every read is round-bounded
        // (STREAM_ROUND) — the thread wakes at its current round's deadline at
        // the latest, sees the flag and exits. Drop joins within
        // READER_JOIN_WAIT; the rare over-wait detaches silently, and the
        // detached thread still self-reclaims (its TCP connection included)
        // within ~one round + backoff instead of surviving until process
        // exit.
        shutdown_stream_reader(&self.shutdown, &mut self.reader);
    }
}

/// The Streamable HTTP SSE branch's response loop, factored out so the
/// matching rule is unit-testable: read events until a message that IS a
/// response to `id` arrives, or the stream ends. The `method` guard is the
/// P2 fix: a server-initiated REQUEST (roots/list, sampling, …) carries a
/// numeric id from the SERVER's own id space — which starts at 1, just
/// like ours — and the old match on id alone handed such a request to
/// extract_result, which bailed "protocol violation" (a request has
/// neither result nor error). pending_dispatch (stdio / legacy SSE)
/// already had this guard; this aligns the third transport. A `method`
/// key marks requests/notifications — never responses.
fn sse_response_for_id<R: BufRead>(reader: &mut R, id: u64, method: &str) -> Result<Value> {
    while let Some((_event, data)) = read_sse_event(reader, MCP_EVENT_MAX)? {
        if let Ok(msg) = serde_json::from_str::<Value>(&data) {
            if msg.get("method").is_none() && msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return Ok(msg);
            }
        }
    }
    bail!("MCP HTTP stream closed before response to '{method}'")
}

/// Read one SSE event as (event, data); returns `Ok(None)` at end of stream,
/// `Err` if a line exceeds the cap or the read fails. Handles `event:`,
/// possibly multi-line `data:`, and skips comments. `event_max` bounds the
/// total aggregated `data:` payload (each line is separately capped by
/// `read_capped_line`); pluggable so a test can exercise the bound without
/// allocating `MCP_EVENT_MAX` bytes.
fn read_sse_event<R: BufRead>(
    reader: &mut R,
    event_max: usize,
) -> Result<Option<(String, String)>> {
    let mut event = String::from("message");
    let mut data = String::new();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let n = read_capped_line(reader, &mut line, MCP_LINE_MAX)?;
        if n == 0 {
            // Flush a buffered event even if the stream ended without a
            // trailing blank line, or the last message is silently lost.
            return Ok(if data.is_empty() {
                None
            } else {
                Some((event, data))
            });
        }
        let Ok(s) = std::str::from_utf8(&line) else {
            continue;
        };
        let t = s.trim_end_matches(['\r', '\n']);
        if t.is_empty() {
            if !data.is_empty() {
                return Ok(Some((event, data)));
            }
            event = String::from("message"); // reset for next event
        } else if let Some(v) = t.strip_prefix("event:") {
            event = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            // Spec: strip at most ONE leading space (the optional separator
            // after the colon), not all whitespace — the payload owns any
            // further indentation. provider::SseReader already read it this
            // way; the crate's two SSE implementations must not silently
            // disagree about the payload's first bytes (a JSON payload is
            // unaffected, but the data prefix is observable all the same —
            // and the old trim_start() here also ate tabs).
            data.push_str(v.strip_prefix(' ').unwrap_or(v));
            // Bound runaway aggregation: a server streaming data: lines
            // without a terminating blank line would grow `data` unbounded.
            if data.len() > event_max {
                anyhow::bail!(
                    "MCP SSE event exceeded {event_max} bytes without a blank-line terminator (runaway or malicious server)"
                );
            }
        }
    }
}

/// Resolve the endpoint event payload against the SSE URL, refusing anything
/// that lands on another origin. The check and the eventual fetch go through
/// the SAME parser — `reqwest::Url` (the `url` crate's WHATWG rules, which is
/// what `client.post` uses internally) — so what was compared is exactly what
/// is sent; the returned Url is stored as-is and never re-parsed from a
/// string.
///
/// Why not a hand-rolled origin split (the pre-P1 code): string logic sees
/// `http://evil.example\@trusted:3000/x` as userinfo + host `trusted:3000`
/// (same origin → approved), but WHATWG treats `\` as `/` in special schemes,
/// so reqwest actually fetched `evil.example` with path `/@trusted:3000/x` —
/// the configured Authorization header walked off-origin (SSRF). Any other
/// parser-vs-string divergence (IDNA, percent-encoded hosts, userinfo edge
/// cases) is covered by the same parse-what-you-send rule. A compromised
/// server could otherwise redirect the authenticated POST (config headers) to
/// an attacker or internal address.
fn resolve_endpoint(sse_url: &str, endpoint: &str) -> Result<reqwest::Url> {
    let base =
        reqwest::Url::parse(sse_url).map_err(|e| anyhow!("invalid SSE URL {sse_url}: {e}"))?;
    // WHATWG join covers all three endpoint shapes a legacy SSE server sends:
    // absolute (`http://…`), network-path (`//host/…`, borrows the base
    // scheme) and relative (`/path`, `?q`). Malformed input (control chars,
    // invalid port, …) is a join error — surfaced, never guessed around.
    let abs = base
        .join(endpoint)
        .with_context(|| format!("invalid MCP SSE endpoint {endpoint:?}"))?;
    if abs.origin() == base.origin() {
        Ok(abs)
    } else {
        bail!(
            "MCP SSE endpoint '{endpoint}' is not on the same origin as '{sse_url}' \
             (possible SSRF); refusing"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P2-6: arming the shutdown-reaper and firing shutdown must kill the child's
    /// whole process group — this is what unblocks a stuck MCP call on TUI quit
    /// so the agent can drop instead of leaving the server orphaned (reparented
    /// to init). Mirrors StdioTransport::spawn's process-group setup so the
    /// reaper's group kill (-pgid) reaches the child.
    ///
    /// Note the reaper watches `shutdown` (session end), NOT `cancel`: a
    /// per-turn cancel is recoverable and must leave the server alive for the
    /// next turn. This test exercises the teardown path only.
    #[cfg(unix)]
    #[test]
    fn reaper_kills_child_process_group_on_shutdown() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        // A long sleep stands in for a wedged MCP server that never responds.
        let mut cmd = Command::new("sleep");
        cmd.arg("30").process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        let pgid = child.id() as i32;
        let alive = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        let reaper =
            spawn_reaper(pgid, alive.clone(), shutdown.clone()).expect("spawn test reaper");
        // Before shutdown, the child must stay alive (no spurious kill).
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            child.try_wait().ok().flatten().is_none(),
            "child should still be alive before shutdown"
        );
        // Fire shutdown → reaper SIGKILLs the group → child dies well inside the
        // 30s sleep (reaper polls every 100ms).
        shutdown.store(true, Ordering::Relaxed);
        let died = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            child.try_wait().ok().flatten().is_some()
        });
        assert!(died, "reaper did not kill the child within ~2s of shutdown");
        let _ = child.wait(); // reap to avoid a zombie
                              // The reaper returned right after the kill; joining must not block.
        let _ = reaper.join();
    }

    /// P2-6: standing the reaper down (alive=false, as Drop does) must NOT kill
    /// the child. This is the path that lets Drop kill+reap the child itself,
    /// with the reaper guaranteed (via join) to be done first — so the reaper
    /// never races on a possibly-reused process-group id.
    #[cfg(unix)]
    #[test]
    fn reaper_stands_down_without_killing_when_alive_cleared() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        let mut cmd = Command::new("sleep");
        cmd.arg("30").process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        let alive = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        let reaper =
            spawn_reaper(child.id() as i32, alive.clone(), shutdown).expect("spawn test reaper");
        std::thread::sleep(Duration::from_millis(150));
        // Drop the "transport": clear alive (reaper stands down) WITHOUT shutdown.
        alive.store(false, Ordering::Relaxed);
        // Reaper must exit promptly (≤ its 100ms poll granularity).
        let joined = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            reaper.is_finished()
        });
        assert!(joined, "reaper should exit promptly once alive is cleared");
        // Child must STILL be alive — Drop (not the reaper) owns the kill.
        assert!(
            child.try_wait().ok().flatten().is_none(),
            "standing the reaper down must not kill the child"
        );
        // Cleanup: kill+reap the still-living child.
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn recv_cancelable_bails_within_a_slice_when_cancel_set() {
        // The intra-turn cancel path: a set flag must bail within ~one 50ms
        // slice, NOT sit out the full 120s CALL_TIMEOUT (the old single
        // recv_timeout made a wedged server hold the turn hostage).
        let (_tx, rx) = mpsc::channel();
        let cancel = AtomicBool::new(true);
        let t0 = Instant::now();
        let err = recv_cancelable(&rx, "tools/call", Some(&cancel)).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "err: {err}");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "cancel must land within a slice, took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn recv_cancelable_returns_the_response_and_ignores_clear_flag() {
        let (tx, rx) = mpsc::channel();
        tx.send(json!({"result": {"ok": true}})).unwrap();
        let cancel = AtomicBool::new(false);
        let msg = recv_cancelable(&rx, "tools/call", Some(&cancel)).unwrap();
        assert_eq!(msg["result"]["ok"], true);
        // And with no cancel flag at all (handshake shape).
        let (tx2, rx2) = mpsc::channel();
        tx2.send(json!({"result": 1})).unwrap();
        assert_eq!(
            recv_cancelable(&rx2, "initialize", None).unwrap()["result"],
            1
        );
    }

    /// A writer whose write() never completes — stands in for a ChildStdin
    /// wired to a server that stopped reading stdin while the pipe buffer was
    /// full (the exact hang the write-thread fix addresses). "Never" is a
    /// sleep far beyond any test deadline; the test process exits without
    /// joining the wedged writer thread, same as the provider tests' `Stall`.
    #[derive(Debug)]
    struct WedgedWriter;
    impl Write for WedgedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_secs(300));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_frame_cancelable_times_out_when_the_writer_never_drains() {
        // The pipe-full hang: the server stopped reading stdin, so the write
        // can never complete. The caller side must give up at its deadline (a
        // short one here; production uses WRITE_TIMEOUT) instead of blocking
        // forever inside the kernel write — the old inline write_all hung the
        // whole turn with the cancel poll unreached and Drop-kill out of
        // reach (the caller was stuck INSIDE request()).
        let t0 = Instant::now();
        let err = write_frame_cancelable(
            WedgedWriter,
            b"{\"id\":1,\"method\":\"tools/call\"}",
            Duration::from_millis(150),
            None,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("timed out"), "{msg}");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "must give up near the 150ms deadline, took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn write_frame_cancelable_honors_cancel_within_a_slice() {
        // A Ctrl-C landing while the frame is still stuck in the write phase
        // must bail within ~one 50ms slice, NOT sit out WRITE_TIMEOUT (and
        // previously: forever). Cancel set up front for determinism; the
        // wedged writer guarantees the write never completes first.
        let cancel = AtomicBool::new(true);
        let t0 = Instant::now();
        let err = write_frame_cancelable(WedgedWriter, b"{\"id\":1}", WRITE_TIMEOUT, Some(&cancel))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cancelled"), "{msg}");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "cancel must land within ~a slice, took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn write_frame_cancelable_writes_the_frame_and_hands_the_writer_back() {
        // Healthy path: the frame (plus its newline terminator) lands on the
        // wire byte-exact, and the writer is handed BACK so the transport can
        // reuse it for the next call — the production ChildStdin round-trips
        // through the channel the same way, so one pipe handle serves the
        // whole session. Vec<u8> implements Write by appending, so feeding
        // the RETURNED writer a second frame shows both frames in order.
        let w = write_frame_cancelable(
            Vec::new(),
            b"{\"id\":1,\"method\":\"tools/list\"}",
            WRITE_TIMEOUT,
            None,
        )
        .expect("healthy write completes");
        let w = write_frame_cancelable(w, b"{\"id\":2}", WRITE_TIMEOUT, None)
            .expect("returned writer is reusable");
        assert_eq!(
            w,
            b"{\"id\":1,\"method\":\"tools/list\"}\n{\"id\":2}\n".to_vec()
        );
    }

    /// Full stdio path: a server that never responds (a `sleep`) must stop
    /// blocking within ~a slice of the cancel flag — this is the TUI
    /// Ctrl-C-during-a-wedged-MCP-call scenario, end to end through the
    /// transport (frame written, pending slot registered, then reclaimed).
    #[cfg(unix)]
    #[test]
    fn stdio_request_unblocks_promptly_on_cancel() {
        let cfg = McpServerConfig {
            name: "sleep-server".into(),
            command: Some("sleep".into()),
            args: vec!["30".into()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            transport: None,
        };
        let mut t = StdioTransport::spawn(&cfg).expect("spawn sleep server");
        let cancel = AtomicBool::new(true);
        let t0 = Instant::now();
        let err = t
            .request(1, "tools/call", json!({"name": "x"}), Some(&cancel))
            .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "err: {err}");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "must unblock within ~a slice, took {:?}",
            t0.elapsed()
        );
        // Drop kills the child (reaped there) — no orphan.
    }

    #[test]
    fn sse_event_parsing() {
        let raw =
            b"event: endpoint\ndata: /messages?session=abc\n\nevent: message\ndata: {\"id\":1}\n\n";
        let mut r = BufReader::new(&raw[..]);
        let (e1, d1) = read_sse_event(&mut r, MCP_EVENT_MAX).unwrap().unwrap();
        assert_eq!(
            (e1.as_str(), d1.as_str()),
            ("endpoint", "/messages?session=abc")
        );
        let (e2, d2) = read_sse_event(&mut r, MCP_EVENT_MAX).unwrap().unwrap();
        assert_eq!((e2.as_str(), d2.as_str()), ("message", "{\"id\":1}"));
        assert!(read_sse_event(&mut r, MCP_EVENT_MAX).unwrap().is_none());
    }

    #[test]
    fn capped_line_rejects_runaway_output() {
        // A line with no newline that grows past MCP_LINE_MAX must error rather
        // than allocating without bound (OOM guard). Use a tiny cap so the test
        // doesn't allocate 16 MiB. read_sse_event just `?`-propagates this, so
        // pinning the cap on the shared helper covers both read paths.
        let cap = 8;
        let runaway: Vec<u8> = vec![b'A'; cap + 5]; // no newline → one endless line
        let mut r = BufReader::new(&runaway[..]);
        let mut buf = Vec::new();
        let err = read_capped_line(&mut r, &mut buf, cap).unwrap_err();
        assert!(format!("{err:#}").contains("exceeds"), "{err:#}");

        // A line within the cap (incl. its newline) reads cleanly.
        let ok = b"ab\n";
        let mut r = BufReader::new(&ok[..]);
        let mut buf = Vec::new();
        let n = read_capped_line(&mut r, &mut buf, cap).unwrap();
        assert_eq!(n, ok.len());
        assert_eq!(&buf, ok);
        // And the next read is a clean EOF.
        buf.clear();
        assert_eq!(read_capped_line(&mut r, &mut buf, cap).unwrap(), 0);
    }

    #[test]
    fn read_sse_event_rejects_runaway_aggregation() {
        // Each data: line is under the per-line cap, but a server that never
        // sends the blank-line terminator would aggregate them forever. The
        // event-total cap must bail before memory is exhausted. Use a tiny cap
        // (the real MCP_EVENT_MAX is 64 MiB — impractical for a unit test).
        let cap = 32;
        // Many short data: lines, no blank-line terminator → data grows past cap.
        let mut raw: Vec<u8> = Vec::new();
        for _ in 0..100 {
            raw.extend_from_slice(b"data: chunk\n");
        }
        let mut r = BufReader::new(&raw[..]);
        let err = read_sse_event(&mut r, cap).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("exceeded"), "{msg}");
        assert!(msg.contains("runaway"), "{msg}");
    }

    #[test]
    fn endpoint_resolution() {
        // Relative path resolves against the SSE origin.
        assert_eq!(
            resolve_endpoint("http://h:3000/sse", "/messages?s=1")
                .unwrap()
                .as_str(),
            "http://h:3000/messages?s=1"
        );
        // Same-origin absolute URL is accepted.
        assert_eq!(
            resolve_endpoint("http://h:3000/sse", "http://h:3000/x")
                .unwrap()
                .as_str(),
            "http://h:3000/x"
        );
        // Network-path reference borrows the SSE URL's scheme (same host → ok,
        // other host → refused).
        assert_eq!(
            resolve_endpoint("http://h:3000/sse", "//h:3000/x")
                .unwrap()
                .as_str(),
            "http://h:3000/x"
        );
        assert!(resolve_endpoint("http://h:3000/sse", "//other/x").is_err());
        // Userinfo does not change the origin (host is what's compared).
        assert!(resolve_endpoint("http://h:3000/sse", "http://u:p@h:3000/x").is_ok());
        // Host case and the scheme's default port are normalized by the WHATWG
        // parser, so these legitimately-same-origin endpoints are not refused.
        assert!(resolve_endpoint("http://Host:3000/sse", "http://host:3000/x").is_ok());
        assert!(resolve_endpoint("http://h/sse", "http://h:80/x").is_ok());
        // Cross-origin absolute URL is refused (SSRF guard).
        assert!(resolve_endpoint("http://h:3000/sse", "http://other/x").is_err());
        // Malformed endpoints are errors, not best-effort guesses.
        assert!(resolve_endpoint("http://h:3000/sse", "http://[").is_err());
    }

    #[test]
    fn endpoint_resolution_rejects_parser_confusion_bypasses() {
        // External-review P1: a hand-rolled origin split read
        // `http://evil.example\@trusted:3000/x` as userinfo `evil.example\` +
        // host `trusted:3000` → same origin → approved; WHATWG treats `\` as
        // `/` in special schemes, so the authenticated POST actually went to
        // `evil.example` with path `/@trusted:3000/x`. With the check and the
        // fetch sharing one parser, the confusion string now resolves to the
        // ATTACKER host and fails the origin compare.
        assert!(resolve_endpoint(
            "http://trusted:3000/sse",
            "http://evil.example\\@trusted:3000/x"
        )
        .is_err());
        // The mirrored shape is harmless, not a bypass: `trusted:3000\@evil…`
        // parses to host trusted:3000 (backslash starts the PATH
        // `/@evil.example/x`) — the POST stays on-origin.
        assert!(resolve_endpoint(
            "http://trusted:3000/sse",
            "http://trusted:3000\\@evil.example/x"
        )
        .is_ok());
        // A leading-backslash RELATIVE reference (`\path`) joins as a path on
        // the base origin (WHATWG's backslash-as-slash rule applies inside
        // authorities, not to the join entry state) — same host, harmless;
        // the network-path forms (`//…`) are covered above.
        assert!(resolve_endpoint("http://h:3000/sse", "\\evil.example/x").is_ok());
        assert!(resolve_endpoint("http://h:3000/sse", "\\h:3000/x").is_ok());
    }

    #[test]
    fn bound_tools_caps_count_description_and_schema() {
        // P2-5: the tools/list result is server-chosen and lands in the
        // model-visible tool list on every request. Count, per-tool
        // description, and serialized schema must each be bounded.
        let mut tools = Vec::new();
        // A tool whose description is over the char cap → kept, truncated.
        tools.push(json!({
            "name": "chatty",
            "description": "y".repeat(MCP_TOOL_DESC_MAX + 50),
            "inputSchema": {"type": "object"},
        }));
        for i in 0..MCP_MAX_TOOLS + 5 {
            tools.push(json!({
                "name": format!("t{i}"),
                "description": "d",
                "inputSchema": {"type": "object"},
            }));
        }
        // A tool whose schema serializes past the byte cap — appended past
        // the count cap, so it is simply never reached in THIS list (the
        // short list below exercises the drop in isolation).
        tools.push(json!({
            "name": "fat-schema",
            "description": "d",
            "inputSchema": {"type": "object", "props": "x".repeat(MCP_SCHEMA_MAX)},
        }));
        let infos = bound_tools(json!({"tools": tools}), "srv");
        assert_eq!(infos.len(), MCP_MAX_TOOLS, "count capped");
        // The first MCP_MAX_TOOLS entries by list order survive; the
        // fat-schema tool appended past the cap is simply never reached.
        let chatty = infos.iter().find(|i| i.name == "chatty").unwrap();
        assert!(
            chatty.description.contains("[truncated"),
            "{}",
            chatty.description
        );
        assert!(chatty.description.chars().count() <= MCP_TOOL_DESC_MAX + 40);

        // With a short list the same rules apply in isolation: the fat schema
        // tool is dropped even when the count is well under the cap.
        let short = bound_tools(
            json!({"tools": [
                {"name": "ok", "description": "d", "inputSchema": {"type": "object"}},
                {"name": "fat", "description": "d", "inputSchema": {"type": "object", "props": "x".repeat(MCP_SCHEMA_MAX)}},
            ]}),
            "srv",
        );
        let names: Vec<&str> = short.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["ok"]);
    }

    #[test]
    fn pending_dispatch_ignores_server_initiated_requests() {
        // A server-initiated REQUEST (sampling, roots/list, …) carries a
        // numeric id just like a response — if its id happens to match a
        // pending call, the old dispatch handed the caller the wrong message.
        // Messages with a `method` key must never dispatch.
        let pending = pending_new();
        let (tx, rx) = mpsc::channel();
        pending.lock().unwrap().insert(7, tx);
        pending_dispatch(
            &pending,
            json!({"jsonrpc": "2.0", "id": 7, "method": "sampling/createTask", "params": {}}),
        );
        assert!(
            rx.try_recv().is_err(),
            "a request must not satisfy a pending response slot"
        );
        // The slot survives for the real response.
        pending_dispatch(
            &pending,
            json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}),
        );
        let got = rx.try_recv().expect("real response dispatches");
        assert_eq!(got["result"]["ok"], json!(true));
    }

    #[test]
    fn sse_response_matching_skips_server_initiated_requests_with_a_colliding_id() {
        // P2 fix: the Streamable HTTP response loop matched on id ALONE. A
        // server-initiated REQUEST (roots/list, sampling, …) carries a
        // numeric id from the SERVER's own id space — which starts at 1,
        // just like ours — so it collided with the pending id and was fed
        // to extract_result, which bailed "protocol violation" (a request
        // has neither result nor error). The method guard must skip it and
        // keep scanning — the same guard pending_dispatch (stdio / legacy
        // SSE) already had; this drives the third transport's real loop
        // shape through the real SSE event reader.
        let raw = "event: message\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\",\"params\":{}}\n\
                   \n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\
                   \n";
        let mut r = BufReader::new(raw.as_bytes());
        let msg = sse_response_for_id(&mut r, 1, "tools/list").unwrap();
        assert!(msg.get("method").is_none(), "a request must never match");
        assert_eq!(msg["result"]["ok"], json!(true));
        // And a stream carrying ONLY the colliding request must fail as
        // "closed before response" — not hand the request to extract_result
        // to die as a "protocol violation".
        let mut r = BufReader::new(
            b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\"}\n\n".as_slice(),
        );
        let err = sse_response_for_id(&mut r, 1, "tools/list").unwrap_err();
        assert!(
            format!("{err:#}").contains("closed before response"),
            "err: {err:#}"
        );
    }

    #[test]
    fn sse_data_lines_strip_at_most_one_leading_space() {
        // P2 fix: read_sse_event used trim_start(), silently eating ALL
        // leading whitespace of a data: payload — provider::SseReader (the
        // crate's other SSE implementation) follows the spec and strips
        // exactly ONE optional space, so the two disagreed about the
        // payload's first bytes. `data:  x` (two spaces) must keep one
        // space; the canonical single-space form stays space-free.
        let raw = b"event: message\ndata:  x\n\n";
        let mut r = BufReader::new(&raw[..]);
        let (_e, d) = read_sse_event(&mut r, MCP_EVENT_MAX).unwrap().unwrap();
        assert_eq!(d, " x", "one separator space must be stripped, no more");
        let raw = b"data: x\n\n";
        let mut r = BufReader::new(&raw[..]);
        let (_e, d) = read_sse_event(&mut r, MCP_EVENT_MAX).unwrap().unwrap();
        assert_eq!(d, "x");
    }

    #[test]
    fn bound_tools_drops_tools_with_missing_or_empty_names() {
        // P2 fix: a missing name used to enter the table as "" — uncallable
        // dead weight in the model-visible list (dispatch is by name), and
        // a second nameless tool would collide with it.
        let infos = bound_tools(
            json!({"tools": [
                {"name": "", "description": "d", "inputSchema": {"type": "object"}},
                {"description": "nameless", "inputSchema": {"type": "object"}},
                {"name": "real", "description": "d", "inputSchema": {"type": "object"}},
            ]}),
            "srv",
        );
        let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    /// P2 fix (Windows Drop kills only the direct child): StdioTransport's
    /// Windows child is cmd.exe — the .cmd-shim wrapper — so the server
    /// itself is a GRANDCHILD the old `child.kill()` in Drop never reached;
    /// it ran out its full lifetime while the transport believed teardown
    /// done. Drop must now terminate the Job Object armed at spawn. `ping`
    /// stands in for the node server: a box-standard Windows binary, no
    /// python needed (the real stdio integration tests need a JSON-RPC
    /// server; those stay on CI). The run-unique loopback address
    /// fingerprints our ping in the process table.
    ///
    /// Flaky note: this test failed ONCE, timing-sensitively, under the
    /// full test matrix at full machine load (the whole suite reran
    /// all-green once unloaded — the same load-sensitivity family as
    /// batch.rs's `cancel_during_retry_backoff`). Semantics unchanged; if
    /// it flakes repeatedly, harden the fixed sleeps into settled-state
    /// polls.
    #[cfg(windows)]
    #[test]
    fn stdio_drop_on_windows_terminates_the_cmd_wrapped_server_tree() {
        let cfg = McpServerConfig {
            name: "ping-server".into(),
            command: Some("ping".into()),
            args: vec!["-n".into(), "600".into(), "127.0.0.203".into()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            transport: None,
        };
        let t = StdioTransport::spawn(&cfg).expect("spawn ping server");
        // Let the grandchild provably start before dropping — a kill that
        // lands before cmd even created ping would pass vacuously.
        std::thread::sleep(Duration::from_secs(2));
        drop(t);
        std::thread::sleep(Duration::from_secs(3));
        let script = "@(Get-CimInstance Win32_Process -Filter \"Name = 'ping.exe'\" | \
                      Where-Object { $_.CommandLine -like '*127.0.0.203*' }).Count";
        let alive = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<usize>()
                    .unwrap_or(usize::MAX)
            })
            .unwrap_or(usize::MAX);
        assert_eq!(
            alive, 0,
            "grandchild ping survived StdioTransport drop (job not terminated?)"
        );
    }

    // ─── Windows cmd /C 参数尾 ────────────────────────────────────────

    #[cfg(windows)]
    #[test]
    fn windows_cmd_tail_quotes_only_what_needs_it() {
        // 裸名命令 + 无特殊字符参数：不产生任何引号（与 std 旧行为字节一
        // 致），cmd 原样执行。
        let tail = windows_cmd_tail("npx", &["-y".into(), "some-mcp-server".into()]);
        assert_eq!(tail, "npx -y some-mcp-server");
    }

    #[cfg(windows)]
    #[test]
    fn windows_cmd_tail_wraps_a_quoted_tail_in_an_outer_quote_pair() {
        // 触发旧 bug 的形态：命令含空格（必被包裹）+ 参数含空格（也被包
        // 裹）。std 的编码拼出 4 个引号 → cmd /C 的剥除规则剥掉首尾引号，
        // 命令行打碎；修复后整段外包一层引号——剥掉的正是外包装，内层引
        // 号完好。
        let tail = windows_cmd_tail(
            "C:\\Program Files\\nodejs\\node.exe",
            &["D:\\my project\\server.js".into()],
        );
        assert!(
            tail.starts_with('"') && tail.ends_with('"'),
            "must be outer-wrapped: {tail}"
        );
        // 剥掉外层包装（cmd /C 剥引号规则实际做的事）后，剩下的必须逐字
        // 节等于按 cmd 习惯逐参数加引号的结果——命令与参数的引号一个不缺。
        assert_eq!(
            &tail[1..tail.len() - 1],
            "\"C:\\Program Files\\nodejs\\node.exe\" \"D:\\my project\\server.js\"",
            "inner tail: {tail}"
        );
        // 全行 6 个引号（外层 2 + 命令 2 + 参数 2）→ cmd 走"≥4 引号剥首尾"
        // 分支，且剥的是最外那一对。
        assert_eq!(tail.matches('"').count(), 6, "tail: {tail}");
        // 无意外转义：除内嵌引号的 `\"` 外不得出现反斜杠转义。
        assert!(
            !tail.contains("\\\\"),
            "no stray escaped backslashes: {tail}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_cmd_tail_leaves_an_unquoted_leading_token_unwrapped() {
        // 首字符不是 `"` → cmd 的剥除规则根本不触发：无需外包装，含空格
        // 的参数各自包裹即可（与旧行为一致的形态——旧 bug 只在命令自身被
        // 包裹时发生）。
        let tail = windows_cmd_tail("node", &["arg with space".into(), "--flag".into()]);
        assert_eq!(tail, "node \"arg with space\" --flag");
    }

    #[cfg(windows)]
    #[test]
    fn windows_cmd_tail_escapes_embedded_quotes_and_doubles_trailing_backslashes() {
        // 内嵌 `"` 写成 `\"`（cmd 不改动参数区内容，子进程的 MSVCRT argv
        // 解析按 `\"` 还原）；参数尾部的连续反斜杠在收尾引号前加倍（MSVCRT
        // 规则：否则 `C:\dir\` 末尾的 `\` 会把引号吃成转义引号，参数边界
        // 丢失）。
        let tail = windows_cmd_tail(
            "node",
            &["say \"hi\"".into(), "C:\\dir with space\\".into()],
        );
        assert_eq!(tail, "node \"say \\\"hi\\\"\" \"C:\\dir with space\\\\\"");
    }

    #[cfg(windows)]
    #[test]
    fn windows_cmd_tail_quotes_cjk_and_empty_arguments() {
        // 非 ASCII（CJK 路径常见）保守包裹；空参数必须成对引号，否则在命
        // 令行上整个消失。
        let tail = windows_cmd_tail("npx", &["服务器.js".into(), String::new(), "x".into()]);
        assert_eq!(tail, "npx \"服务器.js\" \"\" x");
    }

    /// 端到端往返：真 cmd 子进程 + 含空格的可执行路径（临时 .cmd 批处
    /// 理——正是 cmd /C 包装为之存在的 npm 垫片形态）。旧行为（std 编码
    /// 「包裹的命令 + 包裹的参数」= 4 引号被剥首尾）下该形态直接
    /// "'…' 不是内部或外部命令"；windows_cmd_tail 的外层包装必须让含空
    /// 路径真正跑起来，参数也到达（批处理 %* 对 `\"` 的再解析有自身怪
    /// 癖，参数只断言载荷到达，不断言精确引号形态）。
    #[cfg(windows)]
    #[test]
    fn windows_cmd_tail_roundtrips_through_a_real_cmd_spawn() {
        use std::os::windows::process::CommandExt;
        // 目录名带空格 → 批处理全路径必被引号包裹（旧 bug 的触发前提）。
        let dir = std::env::temp_dir().join(format!("ta mcp tail {}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let script_path = dir.join("server.cmd");
        std::fs::write(&script_path, "@echo off\r\necho tail-ok %*\r\n").expect("write cmd");
        let script = script_path.to_string_lossy().into_owned();
        let tail = windows_cmd_tail(&script, &["hello \"world\"".into(), "a b".into()]);
        let out = std::process::Command::new("cmd")
            .arg("/C")
            .raw_arg(tail)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn cmd /C tail");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stdout.contains("tail-ok"),
            "spaced exe path never ran (command line mangled?): stdout={stdout:?} stderr={stderr:?}"
        );
        assert!(
            stdout.contains("hello"),
            "arg 1 payload missing: {stdout:?}"
        );
        assert!(
            stdout.contains("world"),
            "arg 1 quote content missing: {stdout:?}"
        );
        assert!(stdout.contains("a b"), "arg 2 payload missing: {stdout:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── windows_cmd_tail × MSVC/cmd oracle 性质测试（任务 A）─────────────
    //
    // SUT 是 #[cfg(windows)] 的，本节把判据拆成两个**平台无关**的 oracle
    // （MSVC argv 切分、cmd /C 剥引号）+ 一个**平台无关**的编码 oracle：
    // Linux 上钉 oracle 本身、跑「文档编码 → 剥引号 → 切分」的整链镜像
    // 往返；Windows 上另用真 SUT 跑同一往返。oracle 是测试内实现的判据，
    // 不是产品代码。

    /// Fuzz 迭代数：`TA_FUZZ_ITERS` 覆盖默认值（缺省 2_000；大迭代回归
    /// 用 `TA_FUZZ_ITERS=50000 cargo test ...`）。
    fn tail_fuzz_iters() -> usize {
        std::env::var("TA_FUZZ_ITERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2_000)
    }

    /// 本节专用的随机源：固定种子 LCG 包一层，抽样取**高 32 位**再取模。
    /// 共享 [`Lcg::below`] 直接取低位——mod 2^k 抽样的周期只有 2^k，变长
    /// 重抽循环会与它锁步共振（fs_policy 路径汤 fuzz 实测 below(4) 序列
    /// [3,2,1,0] 循环、每轮恰好耗 4 抽 = 整周期 → 每次尝试完全相同 →
    /// 死循环）。高半字有全周期，可变消费模式不共振。
    struct TailRng(u64);
    impl TailRng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn hbelow(&mut self, n: usize) -> usize {
            ((self.next_u64() >> 32) % n as u64) as usize
        }
        fn hpick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
            &xs[self.hbelow(xs.len())]
        }
    }

    /// MSVCRT argv 切分 oracle（CommandLineToArgvW 语义，Microsoft
    /// 「Parsing C++ command-line arguments」规则）：
    /// - 引号外空白（空格/制表）切分参数；
    /// - `"` 开合引号段；引号段内相邻 `""` 折叠为字面量 `"`（保持引号态）；
    /// - 反斜杠串后随 `"`：2n 个 → n 个字面量反斜杠 + 引号开关；2n+1 个 →
    ///   n 个字面量反斜杠 + 字面量 `"`；后随其他字符：全部字面量。
    ///
    /// 简化（对本测试面无影响，显式声明）：不建模 argv[0]——真实
    /// CreateProcess 里是被解析后的 exe 路径，往返断言的是「token 不被
    /// 吞、不被重切」，不是 argv[0] 的字面值；输入不含 \r/\n（命令行语义
    /// 之外）。
    fn msvc_argv_split(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut started = false;
        let mut in_quotes = false;
        let cs: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            match cs[i] {
                ' ' | '\t' => {
                    if in_quotes {
                        cur.push(cs[i]);
                    } else if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                    i += 1;
                }
                '"' => {
                    started = true;
                    if in_quotes && cs.get(i + 1) == Some(&'"') {
                        cur.push('"');
                        i += 2;
                    } else {
                        in_quotes = !in_quotes;
                        i += 1;
                    }
                }
                '\\' => {
                    let mut n = 0;
                    while cs.get(i + n) == Some(&'\\') {
                        n += 1;
                    }
                    if cs.get(i + n) == Some(&'"') {
                        for _ in 0..n / 2 {
                            cur.push('\\');
                        }
                        started = true;
                        if n % 2 == 1 {
                            cur.push('"');
                        } else {
                            in_quotes = !in_quotes;
                        }
                        i += n + 1;
                    } else {
                        for _ in 0..n {
                            cur.push('\\');
                        }
                        started = true;
                        i += n;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                    i += 1;
                }
            }
        }
        if started || !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// cmd /C 剥引号规则 oracle（`cmd /?` 与旧 KB 两支建模）：/C 尾首字符
    /// 是 `"` 且**全行恰好两个引号**、其间无 cmd 特殊字符且有空白 → 保留
    /// 原样；否则剥掉首字符与全行最后一个引号（其余字符——包括尾引号之后
    /// 的内容——原样保留）。两引号保留支文档还要求「其间是可执行文件名」，
    /// 那需要文件系统真值，SUT 输出不可达该支（SUT 的被包裹形态恒 ≥4 个
    /// 引号：内层 cmd token 自带 ≥2 + 外层一对），这里保守按保留建模并
    /// 显式声明简化。
    fn cmd_c_strip_quotes(line: &str) -> String {
        if !line.starts_with('"') {
            return line.to_string();
        }
        let quotes: Vec<usize> = line
            .char_indices()
            .filter(|&(_, c)| c == '"')
            .map(|(i, _)| i)
            .collect();
        if quotes.len() == 2 {
            let between = &line[quotes[0] + 1..quotes[1]];
            let has_special = between
                .chars()
                .any(|c| matches!(c, '&' | '|' | '<' | '>' | '^' | '(' | ')'));
            let has_ws = between.chars().any(|c| c == ' ' || c == '\t');
            if !has_special && has_ws {
                return line.to_string();
            }
        }
        let last = *quotes.last().expect("starts_with 已保证存在引号");
        let mut out = String::with_capacity(line.len());
        out.push_str(&line[1..last]);
        out.push_str(&line[last + 1..]);
        out
    }

    /// 编码 oracle：windows_cmd_tail 文档规则的复刻（触发集 = 空 / ASCII
    /// 空白 / `"` / 非 ASCII / cmd 元字符 `& | < > ^ ( ) % !`——2026-10
    /// 扩展，before/after 见该函数文档；内嵌 `"` 写 `\"` 且其前与收尾引号
    /// 前的连续反斜杠翻倍）。与实现同构是刻意的——平台无关面要能在 Linux
    /// 上验证往返数学；实现本身的钉子是既有 windows 单测 + e2e spawn 测试
    /// + `windows_cmd_tail_roundtrips_random_vectors_through_both_oracles`。
    fn doc_reference_msvc_encode(tok: &str) -> String {
        let needs_quotes = tok.is_empty()
            || tok.chars().any(|c| {
                c.is_ascii_whitespace()
                    || c == '"'
                    || !c.is_ascii()
                    || matches!(c, '&' | '|' | '<' | '>' | '^' | '(' | ')' | '%' | '!')
            });
        if !needs_quotes {
            return tok.to_string();
        }
        let mut out = String::with_capacity(tok.len() + 2);
        out.push('"');
        let mut backslashes = 0usize;
        for c in tok.chars() {
            if c == '\\' {
                backslashes += 1;
                continue;
            }
            if c == '"' {
                for _ in 0..backslashes * 2 {
                    out.push('\\');
                }
                out.push_str("\\\"");
            } else {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
            }
            backslashes = 0;
        }
        for _ in 0..backslashes * 2 {
            out.push('\\');
        }
        out.push('"');
        out
    }

    /// oracle 判例钉子：MSVC 切分按文档向量逐条核对（含空格切分、引号
    /// 开合、`""` 折叠、反斜杠 2n/2n+1 规则、尾反斜杠吃引号、空参数、
    /// CJK 透传）。
    #[test]
    fn msvc_argv_split_oracle_documented_vectors() {
        for (line, want) in [
            ("", &[] as &[&str]),
            ("   ", &[]),
            ("node server.js", &["node", "server.js"]),
            ("a b   c", &["a", "b", "c"]),
            (r#""my app" x"#, &["my app", "x"]),
            (r#""a b"  "c""#, &["a b", "c"]),
            (r#"a""b"#, &["ab"]),
            (r#""a""b""#, &["a\"b"]),
            (r#"a "" b"#, &["a", "", "b"]),
            (r#"\""#, &["\""]),
            (r#""C:\dir\""#, &[r#"C:\dir""#]),
            (r#"a\"b c"#, &["a\"b", "c"]),
            (r"npx 服务器.js", &["npx", "服务器.js"]),
            (r#""""#, &[""]),
        ] {
            assert_eq!(
                msvc_argv_split(line),
                want.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "MSVC 切分判据: {line:?}"
            );
        }
    }

    /// oracle 判例钉子：cmd /C 剥引号两支——首字符非 `"` 恒不动；≥4 引号
    /// （SUT 被包裹形态）剥首字符 + 全行最后一个引号；恰 2 引号且其间干净
    /// 有空白按保留支建模；恰 2 引号但含特殊字符走剥除支。
    #[test]
    fn cmd_c_strip_quotes_oracle_documented_vectors() {
        assert_eq!(
            cmd_c_strip_quotes(r#"node "a b" c"#),
            r#"node "a b" c"#,
            "首字符非引号：cmd 不剥"
        );
        // SUT 被包裹形态：外层一对 + 内层 ≥2，剥完剩下原拼好的尾段。
        let inner = r#""C:\Program Files\x.exe" "my server.js""#;
        assert_eq!(
            cmd_c_strip_quotes(&format!("\"{inner}\"")),
            inner,
            "≥4 引号：剥外层首尾，内层完好"
        );
        assert_eq!(
            cmd_c_strip_quotes(r#""C:\tools\my app.exe" x"#),
            r#""C:\tools\my app.exe" x"#,
            "恰 2 引号、其间有空白且无特殊字符：保留支（文档还要求其间是可执行文件名——文件系统真值，SUT 输出不可达该支，保守建模）"
        );
        assert_eq!(
            cmd_c_strip_quotes(r#""C:\tools\app.exe" x"#),
            "C:\\tools\\app.exe x",
            "恰 2 引号但其间无空白：保留支五条件不齐，走剥除支"
        );
        assert_eq!(
            cmd_c_strip_quotes(r#""a & b" c"#),
            "a & b c",
            "恰 2 引号但含特殊字符：剥除支"
        );
    }

    /// 性质（fuzz，固定种子，平台无关）：编码 oracle 与切分 oracle 互逆——
    /// 单 token：encode → split == [tok]；多 token：各 encode 后空格连接，
    /// split == 原 token 序列。字符池不含 \r/\n（命令行语义外）；本测试
    /// 无 cmd 层，\t 与 cmd 元字符（`& | < > ^ ( ) % !`——触发集 2026-10
    /// 扩展后编码会包裹它们，切分按字面量还原）都可进池。
    #[test]
    fn doc_reference_msvc_encode_roundtrips_through_split_oracle_fuzz() {
        const POOL: &[char] = &[
            'a', 'Z', '9', '.', '_', '-', ' ', '\t', '"', '\\', '&', '|', '<', '>', '^', '(', ')',
            '%', '!', '标', '図', 'ü',
        ];
        let mut rng = TailRng(0x5EED_0032);
        for tok in [
            "plain",
            "with space",
            "say \"hi\"",
            "C:\\dir\\",
            "C:\\dir with space\\",
            "",
            "服务器.js",
            "\\",
            "\"",
            "a\\",
            "\\\"b",
            "a&b|c^d(e)f%g!h",
        ] {
            assert_eq!(
                msvc_argv_split(&doc_reference_msvc_encode(tok)),
                vec![tok.to_string()],
                "单 token 往返: {tok:?}"
            );
        }
        for toks in [
            vec!["node".to_string(), "my project\\server.js".to_string()],
            vec!["say \"hi\"".to_string(), "C:\\dir with space\\".to_string()],
            vec![String::new(), "x".to_string()],
            vec!["x".to_string(), String::new()],
        ] {
            let line = toks
                .iter()
                .map(|t| doc_reference_msvc_encode(t))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(msvc_argv_split(&line), toks, "判例多 token 往返: {toks:?}");
        }
        for _ in 0..tail_fuzz_iters() {
            let n = 1 + rng.hbelow(4);
            let toks: Vec<String> = (0..n)
                .map(|_| {
                    let len = rng.hbelow(7);
                    (0..len).map(|_| *rng.hpick(POOL)).collect()
                })
                .collect();
            let line = toks
                .iter()
                .map(|t| doc_reference_msvc_encode(t))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(msvc_argv_split(&line), toks, "多 token 往返: {toks:?}");
        }
    }

    /// 性质（fuzz + 判例，平台无关）：**整链镜像**——文档编码 oracle +
    /// 外层包装规则（尾段以 `"` 开头时整段再包一层，与 windows_cmd_tail
    /// 文档一致）+ cmd 剥引号 oracle + MSVC 切分 oracle 的组合往返恒等于
    /// 原 token 序列。本机（Linux）可跑可大迭代；真 SUT 的对应往返由
    /// `windows_cmd_tail_roundtrips_random_vectors_through_both_oracles`
    /// 在 Windows 上钉住。触发集 2026-10 扩展后 `& | < > ^ ( )` 被引号
    /// 包裹、对 cmd 是字面量，可进池；`%`/`!` 仍排除（cmd 在引号内仍展开
    /// `%VAR%`、`!` 随延迟扩展环境而变——链路 oracle 未建模展开语义，见
    /// `windows_cmd_tail_quotes_cmd_metachars_for_the_child` 的定性注）；
    /// \r/\n/\t 排除（剥引号/切分 oracle 未建模 cmd 对它们的自身怪癖）。
    #[test]
    fn documented_tail_chain_roundtrips_through_both_oracles_fuzz() {
        const POOL: &[char] = &[
            'a', 'Z', '9', '.', '_', '-', ' ', '"', '\\', '&', '|', '<', '>', '^', '(', ')', '标',
            '図', 'ü',
        ];
        let mut rng = TailRng(0x5EED_0035);
        for toks in [
            vec![
                "npx".to_string(),
                "-y".to_string(),
                "some-mcp-server".to_string(),
            ],
            vec![
                "node".to_string(),
                "arg with space".to_string(),
                "--flag".to_string(),
            ],
            vec!["say \"hi\"".to_string(), "C:\\dir with space\\".to_string()],
            vec!["服务器.js".to_string(), String::new(), "x".to_string()],
            vec![
                "C:\\Program Files\\nodejs\\node.exe".to_string(),
                "my project\\server.js".to_string(),
            ],
            vec![
                "node".to_string(),
                "https://api.example.com/?a=1&b=2".to_string(),
            ],
            vec![String::new()],
            vec!["x".to_string(), String::new()],
        ] {
            chain_roundtrip(&toks);
        }
        for _ in 0..tail_fuzz_iters() {
            let n = 1 + rng.hbelow(4);
            let toks: Vec<String> = (0..n)
                .map(|_| {
                    let len = rng.hbelow(7);
                    (0..len).map(|_| *rng.hpick(POOL)).collect()
                })
                .collect();
            chain_roundtrip(&toks);
        }

        /// 文档编码 + 外层包装 + 剥引号 + MSVC 切分的整链一例。
        fn chain_roundtrip(toks: &[String]) {
            let mut tail = toks
                .iter()
                .map(|t| doc_reference_msvc_encode(t))
                .collect::<Vec<_>>()
                .join(" ");
            if tail.starts_with('"') {
                tail = format!("\"{tail}\"");
            }
            assert_eq!(
                msvc_argv_split(&cmd_c_strip_quotes(&tail)),
                toks,
                "整链往返: {toks:?} tail={tail:?}"
            );
        }
    }

    // ── cmd 引号区 oracle（第四层判据）────────────────────────────────
    //
    // 现有双 oracle（MSVC 切分 + cmd 剥引号）都没有 cmd 的引号区语义：
    // `\"` 只对子进程的 MSVCRT argv 解析生效，cmd 层不认 `\"`（反斜杠是
    // 字面量、挡不住引号开关），token 同时含 `"` 与 cmd 元字符且元字符落
    // 在第一个内嵌引号之后时裸露给 cmd 解析层被拆碎——往返照样全绿，双
    // oracle 结构性测不出（同池同种子 5 万迭代约 14% 向量属此形态）。
    // 本节补第四层判据：cmd 引号区模型 + 无引号面的全覆盖（判例 + fuzz）
    // + 含引号面的显式豁免钉子。SUT 是 `#[cfg(windows)]` 的，平台无关面
    // 走文档编码镜像（同上方整链镜像测试的既有做法）。

    /// cmd 引号区 oracle（测试内判据，平台无关）：线性扫描 cmd 将解析的
    /// 命令行，`"` 无条件切换引号区；任一 cmd 元字符（触发集：
    /// `& | < > ^ ( ) % !`）出现在引号区外即「裸露」。只判引号区内外，
    /// 不建模重定向/管道/`%VAR%` 展开的后续语义（后两者见
    /// windows_cmd_tail 契约段的限制记录）。
    fn cmd_bare_metachars(tail: &str) -> bool {
        let mut in_quotes = false;
        for c in tail.chars() {
            if c == '"' {
                in_quotes = !in_quotes;
            } else if !in_quotes && matches!(c, '&' | '|' | '<' | '>' | '^' | '(' | ')' | '%' | '!')
            {
                return true;
            }
        }
        false
    }

    /// 性质（判例，平台无关）：不含 `"` 的 token 经文档编码镜像 + 外层
    /// 包装后，cmd 将解析的串（**剥引号之后**——cmd /C 先剥外层包装、再
    /// 按引号区解析命令行，扫描裸尾段会把剥掉的外层 `""` 误判成裸露）
    /// 引号区必须覆盖一切元字符——无裸露。覆盖形态：纯元字符、元字符+
    /// 空白、元字符+非 ASCII、URL query 活体、命令路径同时含空格与元字
    /// 符（走外层包装 + 剥引号支）、`%`/`!` 保守表意等。
    #[test]
    fn windows_cmd_tail_quote_zones_cover_metachars_without_embedded_quotes() {
        for toks in [
            vec!["node".to_string(), "&|<>^()".to_string()],
            vec!["node".to_string(), "a&b c|d".to_string()],
            vec!["node".to_string(), "服务器&.js".to_string()],
            vec![
                "node".to_string(),
                "https://api.example.com/?a=1&b=2".to_string(),
            ],
            vec!["node".to_string(), "100%".to_string(), "hi!".to_string()],
            vec!["a&b.exe".to_string()],
            vec!["C:\\Program Files\\a&b.exe".to_string(), "x".to_string()],
            vec![
                "node".to_string(),
                "--flag".to_string(),
                "a&b".to_string(),
                String::new(),
                "服务器.js".to_string(),
            ],
        ] {
            let mut tail = toks
                .iter()
                .map(|t| doc_reference_msvc_encode(t))
                .collect::<Vec<_>>()
                .join(" ");
            if tail.starts_with('"') {
                tail = format!("\"{tail}\"");
            }
            let stripped = cmd_c_strip_quotes(&tail);
            assert!(
                !cmd_bare_metachars(&stripped),
                "无内嵌引号 token 的尾段引号区必须覆盖一切元字符: {toks:?} tail={tail:?} stripped={stripped:?}"
            );
        }
    }

    /// 性质（fuzz，同池同种子于整链镜像测试
    /// `documented_tail_chain_roundtrips_through_both_oracles_fuzz`）：
    /// 不含 `"` 的 token 向量，引号化后 cmd 引号区无裸露；含 `"` 的向量
    /// 显式豁免（`\"` 挡不住 cmd 的引号区开关——文档化限制，见
    /// windows_cmd_tail 契约段）并计数，断言豁免面非空，钉住盲区的存在
    /// 性，防止将来误以为全覆盖。
    #[test]
    fn windows_cmd_tail_cmd_quote_zone_fuzz_exempts_embedded_quote_tokens() {
        const POOL: &[char] = &[
            'a', 'Z', '9', '.', '_', '-', ' ', '"', '\\', '&', '|', '<', '>', '^', '(', ')', '标',
            '図', 'ü',
        ];
        let mut rng = TailRng(0x5EED_0035);
        let mut exempt = 0usize;
        for _ in 0..tail_fuzz_iters() {
            let n = 1 + rng.hbelow(4);
            let toks: Vec<String> = (0..n)
                .map(|_| {
                    let len = rng.hbelow(7);
                    (0..len).map(|_| *rng.hpick(POOL)).collect()
                })
                .collect();
            if toks.iter().any(|t| t.contains('"')) {
                // `\"` 只转义 MSVCRT：cmd 引号区在其第一个内嵌引号处即告
                // 切换，其后元字符可裸露——结构上超出编码可保证的面，豁免。
                exempt += 1;
                continue;
            }
            let mut tail = toks
                .iter()
                .map(|t| doc_reference_msvc_encode(t))
                .collect::<Vec<_>>()
                .join(" ");
            if tail.starts_with('"') {
                tail = format!("\"{tail}\"");
            }
            let stripped = cmd_c_strip_quotes(&tail);
            assert!(
                !cmd_bare_metachars(&stripped),
                "无内嵌引号向量引号区必须覆盖一切元字符: {toks:?} tail={tail:?} stripped={stripped:?}"
            );
        }
        assert!(
            exempt > 0,
            "豁免面为空：同池同种子没探到含 `\"` 的向量——盲区钉子失效，先核对池与种子"
        );
    }

    /// 文档化限制的确定性反例钉子（活标本）：token 同时含 `"` 与元字符、
    /// 元字符落在第一个内嵌引号之后。MSVC 双 oracle 面往返照样全绿
    /// （`\"` 在 MSVC 切分层还原为字面量 `"`）——现有判据结构性测不出；
    /// cmd 引号区面则裸露（真实 cmd 会把 `&` 当命令分隔符拆碎）。cmd 对
    /// 字面 `"` 无通用转义（BatBadBut 一族），正解是建模 + 收窄文档而非
    /// 改编码——本钉子只钉形态，不改产品行为。
    #[test]
    fn windows_cmd_tail_cmd_quote_zone_blind_spot_specimen() {
        let toks = vec!["node".to_string(), "--filter \"x&y\"".to_string()];
        let tail = toks
            .iter()
            .map(|t| doc_reference_msvc_encode(t))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !tail.starts_with('"'),
            "判例前提：命令 token 未被包裹、尾段无外层包装: {tail:?}"
        );
        let stripped = cmd_c_strip_quotes(&tail);
        assert_eq!(
            msvc_argv_split(&stripped),
            toks,
            "MSVC 往返全绿——现有双 oracle 结构性测不出该形态"
        );
        assert!(
            cmd_bare_metachars(&stripped),
            "第一个内嵌引号之后的元字符必须裸露给 cmd 层: {tail:?}"
        );
        // 边界对照：元字符全部落在第一个内嵌引号**之前**时仍在引号区内
        // （限制的触发面是「之后」）。
        let toks2 = ["node".to_string(), "a<b \"c\"".to_string()];
        let tail2 = toks2
            .iter()
            .map(|t| doc_reference_msvc_encode(t))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !cmd_bare_metachars(&cmd_c_strip_quotes(&tail2)),
            "第一个内嵌引号之前的元字符仍在引号区内: {tail2:?}"
        );
    }

    /// 性质（fuzz + 判例，#[cfg(windows)]）：真 SUT 的整链往返——
    /// windows_cmd_tail 输出经剥引号 oracle、MSVC 切分 oracle 后必须还原
    /// [cmd]+args。字符池同整链镜像（`& | < > ^ ( )` 触发集扩展后可进池；
    /// `%`/`!`/` \r`/`\n`/`\t` 排除，理由见整链镜像文档）。
    #[cfg(windows)]
    #[test]
    fn windows_cmd_tail_roundtrips_random_vectors_through_both_oracles() {
        const POOL: &[char] = &[
            'a', 'Z', '9', '.', '_', '-', ' ', '"', '\\', '&', '|', '<', '>', '^', '(', ')', '标',
            '図', 'ü',
        ];
        let mut rng = TailRng(0x5EED_0033);
        let mut cases: Vec<(String, Vec<String>)> = vec![
            (
                "npx".to_string(),
                vec!["-y".to_string(), "some-mcp-server".to_string()],
            ),
            (
                "node".to_string(),
                vec!["arg with space".to_string(), "--flag".to_string()],
            ),
            (
                "node".to_string(),
                vec!["say \"hi\"".to_string(), "C:\\dir with space\\".to_string()],
            ),
            (
                "npx".to_string(),
                vec!["服务器.js".to_string(), String::new(), "x".to_string()],
            ),
            (
                "C:\\Program Files\\nodejs\\node.exe".to_string(),
                vec!["my project\\server.js".to_string()],
            ),
            (
                "node".to_string(),
                vec!["https://api.example.com/?a=1&b=2".to_string()],
            ),
        ];
        let gen = |rng: &mut TailRng| {
            let len = rng.hbelow(7);
            (0..len).map(|_| *rng.hpick(POOL)).collect::<String>()
        };
        for _ in 0..tail_fuzz_iters() {
            let n_args = rng.hbelow(4);
            cases.push((gen(&mut rng), (0..n_args).map(|_| gen(&mut rng)).collect()));
        }
        for (cmd, args) in &cases {
            let tail = windows_cmd_tail(cmd, args);
            let stripped = cmd_c_strip_quotes(&tail);
            let mut want = vec![cmd.clone()];
            want.extend(args.iter().cloned());
            assert_eq!(
                msvc_argv_split(&stripped),
                want,
                "SUT 整链往返: cmd={cmd:?} args={args:?} tail={tail:?}"
            );
        }
    }

    /// 正式用例（2026-10 触发集扩展的验收，before/after 见
    /// windows_cmd_tail 文档）：cmd 元字符 `& | < > ^ ( ) % !` 现在必须被
    /// 包裹——此前它们是 ASCII 非空白、裸露在引号外，cmd /C 的解析层把
    /// `&`/`|` 当命令分隔符、`<`/`>` 当重定向、`^` 当转义符、`( )` 当分组
    /// 符，MCP args 含 URL query（`?a=1&b=2`）的活体形态在 cmd 层就被拆
    /// 碎，子进程永远看不到完整参数；引号内这些字符对 cmd 一律字面量。
    /// 定性：用户配置信任面的正确性修复，非安全边界。另注（文档限制级，
    /// 不在本断言面）：`%VAR%` 展开连引号也挡不住（cmd 在引号内仍展开
    /// 环境变量）、`!` 仅延迟扩展开启时特殊——对这两类加引号只是保守
    /// 表意，不构成保证。
    #[cfg(windows)]
    #[test]
    fn windows_cmd_tail_quotes_cmd_metachars_for_the_child() {
        for arg in [
            "https://api.example.com/?a=1&b=2",
            "a|b",
            "2>nul",
            "a^b",
            "(group)",
            "100%",
            "hi!",
        ] {
            let tail = windows_cmd_tail("node", &[arg.to_string()]);
            assert!(
                tail.contains(&format!("\"{arg}\"")),
                "cmd 元字符参数必须整体加引号（引号外 cmd 会拆）：arg={arg:?} tail={tail:?}"
            );
        }
    }

    // ─── Windows shutdown-reaper ──────────────────────────────────────

    /// Live pings whose command line references `ip` — the same fingerprint
    /// trick as the drop test above, factored out so the reaper tests can
    /// both wait for proof-of-grandchild-life and count survivors. Run-unique
    /// loopback IPs keep tests that run in parallel from counting each
    /// other's pings. A failed query reports usize::MAX so a later
    /// zero-assertion fails loudly instead of passing vacuously (same
    /// policy as the winjob tests' marker counter).
    #[cfg(windows)]
    fn ping_count(ip: &str) -> usize {
        let script = format!(
            "@(Get-CimInstance Win32_Process -Filter \"Name = 'ping.exe'\" | \
             Where-Object {{ $_.CommandLine -like '*{ip}*' }}).Count"
        );
        match std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(usize::MAX),
            Err(_) => usize::MAX,
        }
    }

    /// Poll until the fingerprinted ping is provably up (powershell
    /// cold-start can take seconds), so a kill never lands — and the test
    /// never passes — vacuously before cmd created the grandchild.
    #[cfg(windows)]
    fn wait_for_ping(ip: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if ping_count(ip) >= 1 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Windows reaper, end to end through the transport: arming
    /// `set_shutdown` and firing the flag must TerminateJobObject the
    /// cmd-wrapped tree — the same instant quit-time unblock the unix
    /// reaper's group kill gives, WITHOUT waiting for host-process exit
    /// (KILL_ON_JOB_CLOSE) like the old no-reaper behavior did. Then the
    /// reaper thread must have exited, and dropping the transport must
    /// complete cleanly (stand-down + join + idempotent re-kill + child
    /// wait — a hang fails the test by timeout).
    #[cfg(windows)]
    #[test]
    fn windows_reaper_kills_the_cmd_wrapped_server_tree_on_shutdown() {
        let cfg = McpServerConfig {
            name: "ping-server".into(),
            command: Some("ping".into()),
            args: vec!["-n".into(), "600".into(), "127.0.0.204".into()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            transport: None,
        };
        let mut t = StdioTransport::spawn(&cfg).expect("spawn ping server");
        let shutdown = Arc::new(AtomicBool::new(false));
        t.set_shutdown(&shutdown);
        // The grandchild must provably exist before the kill, or the test
        // would pass vacuously (a kill before cmd even created ping proves
        // nothing about the tree).
        assert!(
            wait_for_ping("127.0.0.204"),
            "grandchild ping never started — the kill test would pass vacuously"
        );
        // Fire shutdown → reaper terminates the job → the whole tree dies
        // well inside the 600s ping (reaper polls every 100ms; ~2s window
        // mirrors the unix test).
        shutdown.store(true, Ordering::Relaxed);
        let died = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            ping_count("127.0.0.204") == 0
        });
        assert!(
            died,
            "reaper did not kill the cmd-wrapped ping tree within ~2s of shutdown"
        );
        // The reaper returned right after the kill; it must be finishable.
        let reaper = t.reaper.as_ref().expect("reaper must be armed");
        let exited = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            reaper.is_finished()
        });
        assert!(exited, "reaper thread must exit after the kill");
        // Drop must complete cleanly — the transport drop path exercises the
        // joined reaper + the (now idempotent) job re-terminate + wait.
        drop(t);
    }

    /// Idempotent arming: a second `set_shutdown` call must be a no-op (the
    /// `reaper.is_some()` early-return), so exactly one reaper — watching
    /// only the FIRST flag — exists. Observable proof: firing the SECOND
    /// flag kills nothing (no reaper watches it); firing the first kills. A
    /// double-spawn would have armed a second reaper on the second flag and
    /// failed the middle assertion.
    #[cfg(windows)]
    #[test]
    fn windows_set_shutdown_twice_arms_exactly_one_reaper() {
        let cfg = McpServerConfig {
            name: "ping-server".into(),
            command: Some("ping".into()),
            args: vec!["-n".into(), "600".into(), "127.0.0.205".into()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            transport: None,
        };
        let mut t = StdioTransport::spawn(&cfg).expect("spawn ping server");
        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        t.set_shutdown(&first);
        t.set_shutdown(&second); // must return early — no second reaper
        assert!(
            wait_for_ping("127.0.0.205"),
            "grandchild ping never started"
        );
        // Fire the flag NO reaper should be watching: 500ms is ~5 reaper
        // poll cycles, ample for a would-be second reaper to have killed.
        second.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            ping_count("127.0.0.205") >= 1,
            "second arming spawned a second reaper (tree died on the second flag?)"
        );
        // The one armed reaper watches the FIRST flag only.
        first.store(true, Ordering::Relaxed);
        let died = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            ping_count("127.0.0.205") == 0
        });
        assert!(
            died,
            "the one armed reaper must kill the tree on the FIRST flag"
        );
        drop(t);
    }

    /// Windows twin of `reaper_stands_down_without_killing_when_alive_cleared`:
    /// clearing `alive` (as Drop does) must exit the reaper WITHOUT killing
    /// the tree — Drop itself owns the kill. Uses the bare job + cmd-wrapped
    /// child (no transport) so "still alive" is observable at all: a dropped
    /// transport kills via its own Drop regardless, which is exactly the
    /// division of labor under test.
    #[cfg(windows)]
    #[test]
    fn windows_reaper_stands_down_without_killing_when_alive_cleared() {
        use std::os::windows::process::CommandExt;
        // The production spawn shape: cmd /C wrapping the real command, the
        // job armed right after (Terminate policy, as StdioTransport does).
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping", "-n", "600", "127.0.0.206"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
            .expect("spawn cmd /C ping");
        let job = crate::winjob::WinJob::assign(&child, crate::winjob::JobDrop::Terminate)
            .expect("assign job");
        assert!(
            wait_for_ping("127.0.0.206"),
            "grandchild ping never started"
        );
        let alive = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        let dup = job.duplicate().expect("duplicate job handle");
        let reaper = spawn_reaper_job(dup, alive.clone(), shutdown).expect("spawn test reaper");
        std::thread::sleep(Duration::from_millis(150));
        // "Drop the transport": clear alive WITHOUT firing shutdown.
        alive.store(false, Ordering::Relaxed);
        // Reaper must exit promptly (≤ its 100ms poll granularity).
        let joined = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            reaper.is_finished()
        });
        assert!(joined, "reaper should exit promptly once alive is cleared");
        // The tree must STILL be alive — Drop (not the reaper) owns the kill.
        assert!(
            ping_count("127.0.0.206") >= 1,
            "standing the reaper down must not kill the cmd-wrapped tree"
        );
        // Cleanup: drop the job (Terminate policy flattens the tree) and
        // reap the direct child.
        drop(job);
        let _ = child.kill(); // died with the job; reap it
        let _ = child.wait();
    }

    // ─── Streamable HTTP: 202 Accepted gap ────────────────────────────

    /// Read one full HTTP request (headers through the blank line, then
    /// `content-length` more bytes of POST body) from a mock socket, and
    /// return the lowercased head — the mocks below route by method/path and
    /// assert on headers through it. Shared by every Streamable-HTTP mock in
    /// this module; bounded at 16 KiB so a runaway request can't park the
    /// mock thread forever.
    fn mock_read_request(sock: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    buf.push(byte[0]);
                    if buf.len() > 16 * 1024 {
                        return String::new(); // runaway request — bail out bounded
                    }
                }
            }
        }
        let head = String::from_utf8_lossy(&buf).to_lowercase();
        let clen: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; clen];
        if clen > 0 {
            let _ = sock.read_exact(&mut body);
        }
        head
    }

    /// Minimal HTTP/1.1 mock standing in for a Streamable HTTP server that
    /// defers responses to the standalone SSE stream: POST → `202 Accepted`
    /// with no body (plus `Mcp-Session-Id`, as an initialize-shaped response
    /// would carry it), GET → `200 text/event-stream` delivering a decoy
    /// server-initiated REQUEST with a colliding id first, then the real
    /// response. Every request's method/path + lowercased headers are
    /// recorded in arrival order so the test can assert the client actually
    /// opened the GET stream — with the session id and the SSE Accept header.
    ///
    /// Bounded everywhere: the accept loop is deadline-capped (a stuck test
    /// can't park the thread forever), sockets get a read timeout, and the
    /// mock exits after serving the two expected requests.
    fn mock_202_then_sse() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let log: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log_c = log.clone();
        std::thread::Builder::new()
            .name("mcp-202-mock".into())
            .spawn(move || {
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut served = 0;
                while served < 2 && Instant::now() < deadline {
                    let mut sock = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => return,
                    };
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let head = mock_read_request(&mut sock);
                    let is_post = head.starts_with("post");
                    log_c.lock().unwrap().push(head);
                    served += 1;
                    if is_post {
                        let resp = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\
                                    mcp-session-id: sess-202\r\nconnection: close\r\n\r\n";
                        let _ = sock.write_all(resp.as_bytes());
                        let _ = sock.flush();
                    } else {
                        // The GET stream: the decoy server-initiated REQUEST
                        // with a colliding id (drives sse_response_for_id's
                        // method guard) arrives before the real response.
                        let resp = concat!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                             cache-control: no-cache\r\nconnection: close\r\n\r\n",
                            "event: message\n",
                            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\",\"params\":{}}\n\n",
                            "event: message\n",
                            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"via\":\"get-sse\"}}\n\n",
                        );
                        let _ = sock.write_all(resp.as_bytes());
                        let _ = sock.flush();
                        // Let the client read before the drop closes the
                        // socket (the response body is close-delimited).
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            })
            .expect("spawn mock");
        (url, log)
    }

    #[test]
    fn http_202_accepted_defers_response_to_the_get_sse_stream() {
        // Spec (Streamable HTTP, 2025-03-26): a POSTed request's response MAY
        // NOT ride the POST's own response — the server returns 202 Accepted
        // with no body, and the actual JSON-RPC response is delivered on the
        // server-initiated SSE stream the client opens with a GET to the same
        // endpoint (Accept: text/event-stream; Mcp-Session-Id once a session
        // exists). The old code treated the bodyless 202 as a plain-JSON
        // response and died on `serde_json::from_str("")` ("invalid JSON
        // response") — and in Auto mode that failure then mis-triggered the
        // legacy-SSE fallback against a Streamable server.
        let (url, log) = mock_202_then_sse();
        let cfg = McpServerConfig {
            name: "mock-202".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t = HttpTransport::new(&url, &cfg).expect("transport");
        let res = t
            .request(1, "tools/list", json!({}), None)
            .expect("response must arrive on the GET SSE stream");
        // The real response matched (decoy skipped, method guard held).
        assert_eq!(res["via"], "get-sse");
        // The POST's 202 carried Mcp-Session-Id; the GET must replay it.
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2, "expected exactly POST + GET, got {log:?}");
        assert!(log[0].starts_with("post /mcp"), "{}", log[0]);
        assert!(log[1].starts_with("get /mcp"), "{}", log[1]);
        assert!(
            log[1].contains("accept: text/event-stream"),
            "GET must accept SSE: {}",
            log[1]
        );
        assert!(
            log[1].contains("mcp-session-id: sess-202"),
            "GET must carry the session id: {}",
            log[1]
        );
    }

    /// Mock for the SHARED standing-stream scenario: POST → 202 (+ session
    /// id); each GET opens an SSE stream that first delivers response #1,
    /// then — once the SECOND POST has arrived (poll the shared log) —
    /// delivers response #2 ON THE SAME CONNECTION. Connections are served
    /// on per-connection threads (a serial accept loop could never keep the
    /// GET stream alive across the second POST — which is the entire point
    /// under test). Bounded like the sibling mock: deadline-capped accept
    /// loop, read timeouts, bounded request reads.
    fn mock_202_shared_stream() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let log: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log_main = log.clone();
        std::thread::Builder::new()
            .name("mcp-shared-mock".into())
            .spawn(move || {
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    let mut sock = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => return,
                    };
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let log_c = log_main.clone();
                    std::thread::Builder::new()
                        .name("mcp-shared-mock-conn".into())
                        .spawn(move || {
                            let head = mock_read_request(&mut sock);
                            if head.is_empty() {
                                return;
                            }
                            let is_post = head.starts_with("post");
                            log_c.lock().unwrap().push(head);
                            if is_post {
                                let resp = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\
                                            mcp-session-id: sess-shared\r\nconnection: close\r\n\r\n";
                                let _ = sock.write_all(resp.as_bytes());
                                let _ = sock.flush();
                            } else {
                                // Response #1 rides immediately; response #2 only
                                // after the second POST exists — proving the same
                                // connection served two different requests.
                                let resp = concat!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                     cache-control: no-cache\r\nconnection: close\r\n\r\n",
                                    "event: message\n",
                                    "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":1}}\n\n",
                                );
                                let _ = sock.write_all(resp.as_bytes());
                                let _ = sock.flush();
                                let wait = Instant::now() + Duration::from_secs(10);
                                while Instant::now() < wait {
                                    let posts = log_c
                                        .lock()
                                        .unwrap()
                                        .iter()
                                        .filter(|l| l.starts_with("post"))
                                        .count();
                                    if posts >= 2 {
                                        break;
                                    }
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                let resp2 = "event: message\n\
                                             data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"n\":2}}\n\n";
                                let _ = sock.write_all(resp2.as_bytes());
                                let _ = sock.flush();
                                // Let the client read before the drop closes the
                                // socket (the response body is close-delimited).
                                std::thread::sleep(Duration::from_millis(300));
                            }
                        })
                        .expect("spawn conn thread");
                }
            })
            .expect("spawn mock");
        (url, log)
    }

    #[test]
    fn http_202_responses_arrive_on_one_shared_standing_stream() {
        // The standing standalone stream must be SHARED across 202s: request
        // #1's response and request #2's response both arrive on the ONE GET
        // connection opened when the session was established — never a fresh
        // GET per 202 (the old per-202 GET is also the race surface: each
        // reopen is a window where the server's push finds nobody connected).
        let (url, log) = mock_202_shared_stream();
        let cfg = McpServerConfig {
            name: "mock-shared".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t = HttpTransport::new(&url, &cfg).expect("transport");
        let r1 = t
            .request(1, "tools/list", json!({}), None)
            .expect("first 202 response arrives via the standing stream");
        assert_eq!(r1["n"], 1);
        let r2 = t
            .request(2, "tools/call", json!({}), None)
            .expect("second 202 response arrives on the SAME stream");
        assert_eq!(r2["n"], 2);
        // The wire shape that proves sharing: exactly POST + GET + POST — one
        // GET for the whole session. The old behavior would show a second GET
        // here (and, raced, could lose the response entirely).
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 3, "expected POST + GET + POST, got {log:?}");
        assert_eq!(
            log.iter().filter(|l| l.starts_with("get")).count(),
            1,
            "exactly one GET may serve both 202s, got {log:?}"
        );
    }

    /// Mock for the RECONNECT scenario: the first GET stream delivers
    /// response #1 and is closed by the SERVER immediately (idle streams do
    /// get dropped); the reconnect GET (whenever the client retries) waits
    /// for the second POST to exist, then delivers response #2. Connections
    /// are routed by request method (not arrival order — the client's
    // reconnect may legally race the second POST), each on its own thread.
    fn mock_202_reconnect() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let log: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log_main = log.clone();
        let get_round = Arc::new(std::sync::Mutex::new(0usize));
        std::thread::Builder::new()
            .name("mcp-reconn-mock".into())
            .spawn(move || {
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    let mut sock = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => return,
                    };
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let log_c = log_main.clone();
                    let round_c = get_round.clone();
                    std::thread::Builder::new()
                        .name("mcp-reconn-mock-conn".into())
                        .spawn(move || {
                            let head = mock_read_request(&mut sock);
                            if head.is_empty() {
                                return;
                            }
                            let is_post = head.starts_with("post");
                            log_c.lock().unwrap().push(head);
                            if is_post {
                                let resp = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\
                                            mcp-session-id: sess-reconn\r\nconnection: close\r\n\r\n";
                                let _ = sock.write_all(resp.as_bytes());
                                let _ = sock.flush();
                                return;
                            }
                            let round = {
                                let mut g = round_c.lock().unwrap();
                                *g += 1;
                                *g
                            };
                            if round == 1 {
                                // Deliver response #1, then close AT ONCE — the
                                // server-dropped-stream shape the reader must
                                // survive by reconnecting.
                                let resp = concat!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                     cache-control: no-cache\r\nconnection: close\r\n\r\n",
                                    "event: message\n",
                                    "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":1}}\n\n",
                                );
                                let _ = sock.write_all(resp.as_bytes());
                                let _ = sock.flush();
                                std::thread::sleep(Duration::from_millis(50));
                            } else {
                                // The reconnected stream waits for POST #2 (the
                                // client's reconnect may beat the second request),
                                // then delivers response #2.
                                let wait = Instant::now() + Duration::from_secs(10);
                                while Instant::now() < wait {
                                    let posts = log_c
                                        .lock()
                                        .unwrap()
                                        .iter()
                                        .filter(|l| l.starts_with("post"))
                                        .count();
                                    if posts >= 2 {
                                        break;
                                    }
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                let resp = concat!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                     cache-control: no-cache\r\nconnection: close\r\n\r\n",
                                    "event: message\n",
                                    "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"n\":2}}\n\n",
                                );
                                let _ = sock.write_all(resp.as_bytes());
                                let _ = sock.flush();
                                std::thread::sleep(Duration::from_millis(300));
                            }
                        })
                        .expect("spawn conn thread");
                }
            })
            .expect("spawn mock");
        (url, log)
    }

    #[test]
    fn http_202_standing_stream_reconnects_after_server_drop() {
        // A server (or idle-connection-dropping proxy) that closes the
        // standing stream mid-session must not doom later 202s: the reader
        // thread reconnects, and request #2's response — pushed on the
        // RECONNECTED stream — still reaches its waiter. Under the old
        // per-202 GET this exact sequence worked by accident (a fresh GET
        // per request); the fix must keep it working while sharing.
        let (url, log) = mock_202_reconnect();
        let cfg = McpServerConfig {
            name: "mock-reconn".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t = HttpTransport::new(&url, &cfg).expect("transport");
        let r1 = t
            .request(1, "tools/list", json!({}), None)
            .expect("first response arrives before the stream drop");
        assert_eq!(r1["n"], 1);
        let r2 = t
            .request(2, "tools/call", json!({}), None)
            .expect("second response arrives on the reconnected stream");
        assert_eq!(r2["n"], 2);
        let log = log.lock().unwrap();
        assert_eq!(
            log.iter().filter(|l| l.starts_with("post")).count(),
            2,
            "two POSTs, got {log:?}"
        );
        assert_eq!(
            log.iter().filter(|l| l.starts_with("get")).count(),
            2,
            "the dropped stream must have been reopened, got {log:?}"
        );
    }

    /// A reader that stays stuck LONGER than READER_JOIN_WAIT (simulated with
    /// a foreign handle) gets detached after the bounded wait — silently: the
    /// old "detached on teardown" warning is gone because the residual thread
    /// now self-reclaims at its round deadline instead of leaking until
    /// process exit. The wait itself must be honored (teardown must not hang,
    /// and must not detach instantly either).
    #[test]
    fn http_transport_teardown_detaches_a_stuck_reader_silently_after_a_bounded_wait() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(30));
        });
        let cfg = McpServerConfig {
            name: "detach".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some("http://127.0.0.1:1/mcp".into()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t =
            HttpTransport::new("http://127.0.0.1:1/mcp", &cfg).expect("transport construction");
        t.stream_reader = Some(handle);

        let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&warnings);
        let _guard = crate::diag::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::diag::install_diag_sink(Box::new(move |level, msg| {
            if level == crate::diag::DiagLevel::Warn {
                sink.lock().unwrap().push(msg.to_string());
            }
        }));
        let t0 = Instant::now();
        drop(t);
        let elapsed = t0.elapsed();
        let captured = warnings.lock().unwrap().clone();
        crate::diag::clear_diag_sink();
        let _ = release_tx.send(()); // let the detached thread exit promptly

        assert!(
            !captured.iter().any(|m| m.contains("detached")),
            "stuck-reader detach must be silent now (bounded self-reclaim), got {captured:?}"
        );
        assert!(
            elapsed >= READER_JOIN_WAIT,
            "the bounded wait must be honored before detaching, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "teardown must not hang on a stuck reader, took {elapsed:?}"
        );
    }

    /// The finished-reader branch joins deterministically and must NOT emit
    /// the detach warning (no leak to report).
    #[test]
    fn http_transport_joins_a_finished_reader_without_warning() {
        let handle = std::thread::spawn(|| {});
        // Make sure the thread is finished before Drop inspects it.
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        let cfg = McpServerConfig {
            name: "joined".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some("http://127.0.0.1:1/mcp".into()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t =
            HttpTransport::new("http://127.0.0.1:1/mcp", &cfg).expect("transport construction");
        t.stream_reader = Some(handle);

        let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&warnings);
        let _guard = crate::diag::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::diag::install_diag_sink(Box::new(move |level, msg| {
            if level == crate::diag::DiagLevel::Warn {
                sink.lock().unwrap().push(msg.to_string());
            }
        }));
        drop(t);
        let captured = warnings.lock().unwrap().clone();
        crate::diag::clear_diag_sink();

        assert!(
            !captured
                .iter()
                .any(|m| m.contains("standing-stream reader detached")),
            "finished reader must be joined silently, got {captured:?}"
        );
    }

    // ── 流轮转（STREAM_ROUND）与有界 teardown ────────────────────────────

    /// Probe for `is_stream_round_timeout`: the deadline-vs-real-error
    /// discrimination relies on reqwest's internal error shape (a
    /// request-timeout read failure surfaces as
    /// `io::Error::new(Other, reqwest::Error)` — into_io pins the ErrorKind
    /// to Other, so only the INNER error knows, via `is_timeout()`). Verify
    /// the shape against a real timeout before anything depends on it; a
    /// reqwest upgrade that changes the shape turns THIS red first, instead
    /// of rotations being misread as real read errors (or the reverse).
    #[test]
    fn stream_round_timeout_surfaces_as_a_detectable_reqwest_timeout() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let head = mock_read_request(&mut sock);
            assert!(head.starts_with("get"), "{head}");
            let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        cache-control: no-cache\r\nconnection: close\r\n\r\n";
            sock.write_all(resp.as_bytes()).expect("write headers");
            sock.flush().ok();
            // Hold the connection open with no data: the client's read can
            // only wake at its round deadline.
            std::thread::sleep(Duration::from_secs(2));
        });
        // Same builder shape as the stream readers (connect-only client; the
        // round bound rides on the request).
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client");
        let resp = client
            .get(&url)
            .header("accept", "text/event-stream")
            .timeout(Duration::from_millis(300))
            .send()
            .expect("headers arrive");
        let mut reader = BufReader::new(resp);
        let t0 = Instant::now();
        let err = read_sse_event(&mut reader, MCP_EVENT_MAX)
            .expect_err("round deadline must fire on a silent stream");
        assert!(
            t0.elapsed() >= Duration::from_millis(250),
            "timeout fired implausibly early: {:?}",
            t0.elapsed()
        );
        assert!(
            is_stream_round_timeout(&err),
            "reqwest round-timeout must be recognizable through the error chain, got {err:#}"
        );
        // Negative: a plain io error (no reqwest inner) is never a rotation —
        // not even with a timeout ErrorKind (reqwest's into_io never
        // produces that shape, and the discrimination must not lean on it).
        assert!(!is_stream_round_timeout(&anyhow::Error::from(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "plain io timeout")
        )));
        server.join().expect("mock thread");
    }

    /// Mock that serves EVERY incoming connection (GET → SSE headers, then
    /// silence — a client can only rotate away or exit via its round
    /// deadline) and records `get`/`closed` entries: `closed` lands when the
    /// client side of that connection goes away, so teardown tests can watch
    /// the TCP connections close from the peer side. `with_endpoint`
    /// prepends the legacy `endpoint` event (needed to construct a
    /// SseTransport). Bounded like the sibling mocks: deadline-capped accept
    /// loop, read timeouts, bounded request reads.
    fn mock_held_sse_stream(with_endpoint: bool) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::Read;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let log: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log_main = log.clone();
        std::thread::Builder::new()
            .name("mcp-held-mock".into())
            .spawn(move || {
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    let mut sock = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => return,
                    };
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let log_c = log_main.clone();
                    std::thread::Builder::new()
                        .name("mcp-held-mock-conn".into())
                        .spawn(move || {
                            let head = mock_read_request(&mut sock);
                            if head.is_empty() {
                                return;
                            }
                            log_c.lock().unwrap().push(head);
                            let mut resp = String::from(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                 cache-control: no-cache\r\nconnection: close\r\n\r\n",
                            );
                            if with_endpoint {
                                resp.push_str("event: endpoint\ndata: /mcp\n\n");
                            }
                            sock.write_all(resp.as_bytes()).ok();
                            sock.flush().ok();
                            // Hold until the client side goes away (a
                            // rotation or teardown) — that FIN/error is the
                            // peer-observable close under test.
                            let mut buf = [0u8; 1];
                            let _ = sock.read(&mut buf);
                            log_c.lock().unwrap().push("closed".into());
                        })
                        .expect("spawn conn thread");
                }
            })
            .expect("spawn mock");
        (url, log)
    }

    /// Poll the shared mock log until `pred` holds; asserts with the deadline
    /// (the same bounded-wait shape the sibling mocks use inline).
    fn wait_for_log(
        log: &std::sync::Mutex<Vec<String>>,
        pred: impl Fn(&[String]) -> bool,
        what: &str,
    ) {
        let wait = Instant::now() + Duration::from_secs(5);
        loop {
            let entries = log.lock().unwrap().clone();
            if pred(&entries) {
                return;
            }
            assert!(
                Instant::now() < wait,
                "timed out waiting for {what}: {entries:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn count_gets(entries: &[String]) -> usize {
        entries.iter().filter(|l| l.starts_with("get")).count()
    }

    #[test]
    fn http_standing_stream_rotations_stay_silent() {
        // 到期轮转是例行节奏，不是故障：健康流每轮重连一次，全程零 diag。
        let (url, log) = mock_held_sse_stream(false);
        let cfg = McpServerConfig {
            name: "rot-silent".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t = HttpTransport::new(&url, &cfg).expect("transport");
        t.stream_round = Duration::from_millis(300);
        let _guard = crate::diag::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&warnings);
        crate::diag::install_diag_sink(Box::new(move |level, msg| {
            if level == crate::diag::DiagLevel::Warn {
                sink.lock().unwrap().push(msg.to_string());
            }
        }));
        t.ensure_standalone_stream(true)
            .expect("arm the standing stream");
        wait_for_log(&log, |e| count_gets(e) >= 3, "≥2 deadline rotations");
        drop(t); // teardown must stay silent too (join within ~one round)
        let captured = warnings.lock().unwrap().clone();
        crate::diag::clear_diag_sink();
        assert!(
            captured.is_empty(),
            "deadline rotations must stay silent, got {captured:?}"
        );
    }

    #[test]
    fn http_transport_teardown_joins_a_round_bounded_reader_and_closes_the_tcp_connection() {
        // 新 teardown 契约：置停后读者至多一轮内退出；Drop 在有界窗口内
        // join（明显短于 READER_JOIN_WAIT 的 detach 路径）、静默，对端看到
        // 每条连接最终关闭——包括 drop 时仍挂着的那条。
        let (url, log) = mock_held_sse_stream(false);
        let cfg = McpServerConfig {
            name: "rot-teardown".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t = HttpTransport::new(&url, &cfg).expect("transport");
        t.stream_round = Duration::from_millis(300);
        let _guard = crate::diag::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&warnings);
        crate::diag::install_diag_sink(Box::new(move |level, msg| {
            if level == crate::diag::DiagLevel::Warn {
                sink.lock().unwrap().push(msg.to_string());
            }
        }));
        t.ensure_standalone_stream(true)
            .expect("arm the standing stream");
        wait_for_log(&log, |e| count_gets(e) >= 2, "≥1 rotation");
        // 落进新一轮的中段：drop 时读者正挂在一次有界读里。
        std::thread::sleep(Duration::from_millis(120));
        let t0 = Instant::now();
        drop(t);
        let elapsed = t0.elapsed();
        let captured = warnings.lock().unwrap().clone();
        crate::diag::clear_diag_sink();
        assert!(
            elapsed < Duration::from_millis(1900),
            "teardown must join within the bounded wait (detaching takes ≥2s), took {elapsed:?}"
        );
        assert!(
            !captured.iter().any(|m| m.contains("detached")),
            "a joined reader must be silent, got {captured:?}"
        );
        // 对端观测：每条被接受的 GET 连接都被客户端关闭。
        wait_for_log(
            &log,
            |e| count_gets(e) > 0 && e.iter().filter(|l| *l == "closed").count() >= count_gets(e),
            "the peer to observe every connection closed",
        );
    }

    /// Mock for the REAL-read-error case: the GET answers SSE headers with a
    /// lying `content-length`, writes a few bytes, then closes — hyper fails
    /// the body with an incomplete-message error (a genuine non-timeout
    /// reqwest body error, exactly the shape a deadline must NOT be mistaken
    /// for).
    fn mock_short_body_stream() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let log: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log_main = log.clone();
        std::thread::Builder::new()
            .name("mcp-shortbody-mock".into())
            .spawn(move || {
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    let mut sock = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => return,
                    };
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let head = mock_read_request(&mut sock);
                    if head.is_empty() {
                        return;
                    }
                    log_main.lock().unwrap().push(head);
                    let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                content-length: 100\r\nconnection: close\r\n\r\n0123456789";
                    sock.write_all(resp.as_bytes()).ok();
                    sock.flush().ok();
                    drop(sock); // early FIN with 90 bytes still owed → real error
                }
            })
            .expect("spawn mock");
        (url, log)
    }

    #[test]
    fn http_standing_stream_real_read_error_still_warns_and_reconnects() {
        // 真读错照旧告警并重连——到期轮转的静默只属于 deadline 到期这一种
        // 形状；两者绝不能混淆。
        let (url, log) = mock_short_body_stream();
        let cfg = McpServerConfig {
            name: "rot-err".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t = HttpTransport::new(&url, &cfg).expect("transport");
        t.stream_round = Duration::from_millis(300);
        let _guard = crate::diag::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&warnings);
        crate::diag::install_diag_sink(Box::new(move |level, msg| {
            if level == crate::diag::DiagLevel::Warn {
                sink.lock().unwrap().push(msg.to_string());
            }
        }));
        t.ensure_standalone_stream(true)
            .expect("arm the standing stream");
        wait_for_log(&log, |e| count_gets(e) >= 2, "a reconnect after the error");
        let captured = warnings.lock().unwrap().clone();
        crate::diag::clear_diag_sink();
        assert!(
            captured
                .iter()
                .any(|m| m.contains("read stopped") && m.contains("reconnecting")),
            "a real read error must still warn, got {captured:?}"
        );
        drop(t);
    }

    /// Mock for the ROTATION-delivery case: POST → 202 (+ session id); EVERY
    /// GET connection delivers response #1 immediately, then — once the
    /// SECOND POST exists (shared log) — delivers response #2 on whichever
    /// connection is live at that moment. Note what this mock deliberately
    /// does SERVER-side: it re-sends response #1 on every fresh GET, because
    /// that re-send is the server's job (no Last-Event-ID on our GETs, no
    /// re-send obligation in JSON-RPC over SSE) — the pending table only
    /// dispatches whatever arrives, so a rotation-cut response the server
    /// never re-sends is simply lost and the caller sits out CALL_TIMEOUT.
    /// Bounded like the siblings.
    fn mock_202_rotating_stream() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::Read;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let log: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log_main = log.clone();
        std::thread::Builder::new()
            .name("mcp-rot-mock".into())
            .spawn(move || {
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    let mut sock = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => return,
                    };
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let log_c = log_main.clone();
                    std::thread::Builder::new()
                        .name("mcp-rot-mock-conn".into())
                        .spawn(move || {
                            let head = mock_read_request(&mut sock);
                            if head.is_empty() {
                                return;
                            }
                            let is_post = head.starts_with("post");
                            log_c.lock().unwrap().push(head);
                            if is_post {
                                let resp = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\
                                            mcp-session-id: sess-rot\r\nconnection: close\r\n\r\n";
                                let _ = sock.write_all(resp.as_bytes());
                                let _ = sock.flush();
                                return;
                            }
                            let resp = concat!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                 cache-control: no-cache\r\nconnection: close\r\n\r\n",
                                "event: message\n",
                                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":1}}\n\n",
                            );
                            let _ = sock.write_all(resp.as_bytes());
                            let _ = sock.flush();
                            // Response #2 goes to whichever connection is
                            // live when the second POST exists.
                            let wait = Instant::now() + Duration::from_secs(5);
                            let mut posts = 0usize;
                            while Instant::now() < wait {
                                posts = log_c
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .filter(|l| l.starts_with("post"))
                                    .count();
                                if posts >= 2 {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            if posts >= 2 {
                                let resp2 = "event: message\n\
                                             data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"n\":2}}\n\n";
                                let _ = sock.write_all(resp2.as_bytes());
                                let _ = sock.flush();
                            }
                            // Hold until the client rotates away / tears down.
                            let mut buf = [0u8; 1];
                            let _ = sock.read(&mut buf);
                        })
                        .expect("spawn conn thread");
                }
            })
            .expect("spawn mock");
        (url, log)
    }

    #[test]
    fn http_202_delivery_survives_standing_stream_rotations() {
        // 到期轮转不得弄丢 202 投递：小轮时长下跨至少两次轮转，第二条响应
        // 仍送达——落在轮转后仍活着的连接上。轮转切断的投递能否找回完全取
        // 决于服务器是否在新连接上重发（本 mock 刻意在每个 GET 上重发响应
        // #1，正是在替服务器履行这份协议并未要求的重发）；pending 表只分发
        // 到达的消息、自己不重投——服务器不重发时调用方坐满 CALL_TIMEOUT。
        let (url, log) = mock_202_rotating_stream();
        let cfg = McpServerConfig {
            name: "rot-202".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Http),
            headers: HashMap::new(),
        };
        let mut t = HttpTransport::new(&url, &cfg).expect("transport");
        t.stream_round = Duration::from_millis(300);
        let r1 = t
            .request(1, "tools/list", json!({}), None)
            .expect("first 202 response arrives via the standing stream");
        assert_eq!(r1["n"], 1);
        wait_for_log(&log, |e| count_gets(e) >= 3, "≥2 deadline rotations");
        let r2 = t
            .request(2, "tools/call", json!({}), None)
            .expect("second 202 response still arrives despite rotations");
        assert_eq!(r2["n"], 2);
    }

    #[test]
    fn legacy_sse_teardown_joins_the_round_bounded_reader() {
        // legacy SSE 同款契约：endpoint 握手后读者挂在 GET 读里；置停后至多
        // 一轮退出——Drop 有界 join、静默，对端看到连接关闭。
        let (url, log) = mock_held_sse_stream(true);
        let cfg = McpServerConfig {
            name: "legacy-teardown".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Sse),
            headers: HashMap::new(),
        };
        let t = SseTransport::connect_with_round(&url, &cfg, Duration::from_millis(300))
            .expect("connect with the endpoint event");
        wait_for_log(&log, |e| count_gets(e) >= 2, "≥1 rotation");
        let _guard = crate::diag::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&warnings);
        crate::diag::install_diag_sink(Box::new(move |level, msg| {
            if level == crate::diag::DiagLevel::Warn {
                sink.lock().unwrap().push(msg.to_string());
            }
        }));
        let t0 = Instant::now();
        drop(t);
        let elapsed = t0.elapsed();
        let captured = warnings.lock().unwrap().clone();
        crate::diag::clear_diag_sink();
        assert!(
            elapsed < Duration::from_millis(1900),
            "legacy teardown must join within the bounded wait, took {elapsed:?}"
        );
        assert!(
            !captured.iter().any(|m| m.contains("detached")),
            "a joined legacy reader must be silent, got {captured:?}"
        );
        wait_for_log(
            &log,
            |e| count_gets(e) > 0 && e.iter().filter(|l| *l == "closed").count() >= count_gets(e),
            "the peer to observe every legacy connection closed",
        );
    }

    /// Mock for the legacy ROTATION-delivery case: POST → 202; every GET
    /// connection sends the `endpoint` event, waits for the POST to exist
    /// (shared log), then delivers response #1 — on whichever connection is
    /// live at that moment. Same server-side caveat as the standing-stream
    /// sibling: the MOCK re-sends on every fresh GET (that re-send is the
    /// server's job and no protocol obligation); the pending table only
    /// dispatches what arrives — a late duplicate finds its slot already
    /// reclaimed and is dropped, a never-re-sent response is lost to
    /// CALL_TIMEOUT.
    fn mock_legacy_rotating_stream() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::Read;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let log: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log_main = log.clone();
        std::thread::Builder::new()
            .name("mcp-legacy-rot-mock".into())
            .spawn(move || {
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    let mut sock = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => return,
                    };
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let log_c = log_main.clone();
                    std::thread::Builder::new()
                        .name("mcp-legacy-rot-mock-conn".into())
                        .spawn(move || {
                            let head = mock_read_request(&mut sock);
                            if head.is_empty() {
                                return;
                            }
                            let is_post = head.starts_with("post");
                            log_c.lock().unwrap().push(head);
                            if is_post {
                                let resp = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\
                                            connection: close\r\n\r\n";
                                let _ = sock.write_all(resp.as_bytes());
                                let _ = sock.flush();
                                return;
                            }
                            let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                                        cache-control: no-cache\r\nconnection: close\r\n\r\n\
                                        event: endpoint\ndata: /mcp\n\n";
                            let _ = sock.write_all(resp.as_bytes());
                            let _ = sock.flush();
                            let wait = Instant::now() + Duration::from_secs(5);
                            let mut posts = 0usize;
                            while Instant::now() < wait {
                                posts = log_c
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .filter(|l| l.starts_with("post"))
                                    .count();
                                if posts >= 1 {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            if posts >= 1 {
                                let resp2 = "event: message\n\
                                             data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":1}}\n\n";
                                let _ = sock.write_all(resp2.as_bytes());
                                let _ = sock.flush();
                            }
                            // Hold until the client rotates away / tears down.
                            let mut buf = [0u8; 1];
                            let _ = sock.read(&mut buf);
                        })
                        .expect("spawn conn thread");
                }
            })
            .expect("spawn mock");
        (url, log)
    }

    #[test]
    fn legacy_sse_response_delivery_survives_stream_rotations() {
        // legacy SSE 的轮转不得弄丢响应投递：轮转已稳定发生后发出的请求，
        // 其响应落在当时活着的连接上照常送达。
        let (url, log) = mock_legacy_rotating_stream();
        let cfg = McpServerConfig {
            name: "legacy-rot".into(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.clone()),
            transport: Some(McpTransport::Sse),
            headers: HashMap::new(),
        };
        let mut t = SseTransport::connect_with_round(&url, &cfg, Duration::from_millis(300))
            .expect("connect with the endpoint event");
        wait_for_log(&log, |e| count_gets(e) >= 2, "≥1 rotation");
        let r = t
            .request(1, "tools/list", json!({}), None)
            .expect("response arrives despite rotations");
        assert_eq!(r["ok"], 1);
    }

    // ── 解析器性质测试（固定种子 LCG，零依赖） ──────────────────────────────

    /// Deterministic 64-bit LCG (fixed seed): the property tests' only source
    /// of randomness. Same seed → same bytes on every run and platform.
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
        fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
            &xs[self.below(xs.len())]
        }
    }

    /// Property (fuzz, fixed seed): `read_sse_event` over randomly assembled
    /// SSE streams must (a) never panic and never error (every line is far
    /// under the cap), (b) keep event boundaries — each generated event
    /// reparses to exactly its own (event, data) across CRLF/blank-line
    /// variants, multi-line data and the missing-trailing-blank EOF flush,
    /// (c) skip invalid lines (invalid UTF-8, comments, unknown fields, a
    /// lone `event:`) WITHOUT swallowing the events around them.
    #[test]
    fn sse_fuzz_event_boundaries_and_garbage_skipped() {
        const DATA_PAYLOADS: &[&str] = &[
            "alpha",
            "{\"n\":1}",
            "γ gamma",    // multibyte payload
            " two-space", // spec: strip at most ONE space after `data:`
            "e=4:5",      // colons/equals inside the payload
        ];
        const EVENT_NAMES: &[&str] = &["message", "endpoint", "ping/2024"];
        const GARBAGE: &[&[u8]] = &[
            b":a comment line\n",
            b"unknown-field: whatever\n",
            b"\xff\xfe invalid utf8\n",
            b"event: lonely\n", // no data before the blank line → reset
            b"\r\n",            // a CR-only blank line
        ];
        let mut rng = Lcg(0x5EED_0001);
        for _ in 0..200 {
            let n_events = 1 + rng.below(5);
            // The oracle: one entry per generated event.
            let mut expected: Vec<(String, String)> = Vec::new();
            let mut raw: Vec<u8> = Vec::new();
            for i in 0..n_events {
                let name = *rng.pick(EVENT_NAMES);
                let n_data = 1 + rng.below(3);
                let mut datas = Vec::new();
                for _ in 0..n_data {
                    let payload = rng.pick(DATA_PAYLOADS).to_string();
                    // `data: {payload}` — the parser strips exactly one space,
                    // so the payload round-trips verbatim (incl. its own
                    // leading space, which must NOT be stripped twice).
                    datas.push(payload.clone());
                    raw.extend_from_slice(format!("data: {payload}").as_bytes());
                    raw.extend_from_slice(if rng.below(4) == 0 { b"\r\n" } else { b"\n" });
                }
                if name != "message" {
                    raw.extend_from_slice(format!("event: {name}\n").as_bytes());
                }
                expected.push((name.to_string(), datas.join("\n")));
                // Terminator: blank line, or nothing on the last event
                // (EOF must flush the buffered event, not drop it).
                let is_last = i + 1 == n_events;
                if !is_last || rng.below(2) == 0 {
                    raw.extend_from_slice(if rng.below(3) == 0 { b"\r\n" } else { b"\n" });
                }
                // Garbage strictly BETWEEN events (never adjacent to real
                // data: lines): must be skipped without starting an event.
                // A blank line follows every garbage slice so a lone
                // `event:` line resets instead of leaking onto the next
                // event's name.
                if !is_last && rng.below(2) == 0 {
                    raw.extend_from_slice(rng.pick(GARBAGE));
                    raw.push(b'\n');
                }
            }
            let parse = |bytes: &[u8]| -> Vec<(String, String)> {
                let mut r = BufReader::new(bytes);
                let mut got = Vec::new();
                while let Some((e, d)) = read_sse_event(&mut r, MCP_EVENT_MAX).expect("no error") {
                    got.push((e, d));
                }
                got
            };
            assert_eq!(
                parse(&raw),
                expected,
                "stream: {:?}",
                String::from_utf8_lossy(&raw)
            );
            // Idempotence: a fresh reader over the same bytes answers the same.
            assert_eq!(parse(&raw), parse(&raw));
        }
    }

    /// Property (fuzz, fixed seed): `sse_response_for_id`'s JSON-RPC framing —
    /// noise frames (non-JSON payloads, server-initiated REQUESTS that carry
    /// the target id, responses for other ids) are skipped without ending the
    /// scan, and the one matching response is returned verbatim.
    #[test]
    fn sse_jsonrpc_framing_skips_noise_and_routes_the_target_response() {
        let mut rng = Lcg(0x5EED_0002);
        for _ in 0..200 {
            let target = 10 + rng.below(5) as u64; // 10..=14 — decoys stay below
            let n_noise = rng.below(5);
            let mut raw: Vec<u8> = Vec::new();
            for _ in 0..n_noise {
                let noise = match rng.below(4) {
                    0 => "not json at all".to_string(),
                    // A request carrying the TARGET id must NOT match.
                    1 => {
                        json!({"id": target, "method": "sampling/create", "params": {}}).to_string()
                    }
                    2 => json!({"id": rng.below(10) as u64, "result": {"decoy": true}}).to_string(),
                    _ => json!({"id": 9000 + rng.below(10) as u64,
                                "error": {"code": -1, "message": "e"}})
                    .to_string(),
                };
                raw.extend_from_slice(format!("data: {noise}\n\n").as_bytes());
            }
            let want = json!({"id": target, "result": {"n": target}});
            raw.extend_from_slice(format!("data: {}\n\n", want).as_bytes());
            let mut r = BufReader::new(&raw[..]);
            let got = sse_response_for_id(&mut r, target, "tools/call").expect("response found");
            assert_eq!(got, want);
        }
        // Exhausted stream without a match is a clean error, not a hang.
        let mut r = BufReader::new(&b"data: {\"id\":1}\n\n"[..]);
        assert!(sse_response_for_id(&mut r, 2, "tools/call").is_err());
    }

    /// Property (fuzz, fixed seed): the stdio reader loop's per-line handling
    /// (read_capped_line → UTF-8/JSON validate → pending_dispatch) over
    /// randomly framed NDJSON must deliver each response frame to exactly its
    /// own waiter — CRLF endings, blank/garbage/invalid-UTF-8 lines, request
    /// frames carrying a live id, and late frames with no waiter are all
    /// routed or skipped without cross-delivery.
    #[test]
    fn stdio_ndjson_fuzz_lines_route_without_cross_delivery() {
        let mut rng = Lcg(0x5EED_0003);
        for _ in 0..200 {
            let n = 1 + rng.below(6);
            let pending = pending_new();
            let mut waiters: Vec<(u64, mpsc::Receiver<Value>)> = Vec::new();
            let mut raw: Vec<u8> = Vec::new();
            for i in 0..n {
                let id = i as u64 * 2 + 1; // sparse, unique per round
                let msg = json!({"id": id, "result": {"n": i}});
                let (tx, rx) = mpsc::channel();
                pending.lock().unwrap().insert(id, tx);
                waiters.push((id, rx));
                // One frame = one line regardless of internal spacing; \r\n
                // is a legal terminator (from_str sees `s.trim()`).
                let mut line = msg.to_string();
                if rng.below(4) == 0 {
                    line = format!("  {line}  "); // surrounding whitespace trimmed
                }
                raw.extend_from_slice(line.as_bytes());
                raw.extend_from_slice(if rng.below(3) == 0 { b"\r\n" } else { b"\n" });
                // Garbage between frames — none of it may dispatch anything.
                match rng.below(5) {
                    0 => raw.extend_from_slice(b"\n"),
                    1 => raw.extend_from_slice(b"server log noise, not json\n"),
                    2 => raw.extend_from_slice(b"\xff\xfe broken utf8\n"),
                    3 => raw.extend_from_slice(b"{\"id\":9999,\"result\":{}}\n"), // no waiter
                    // A REQUEST frame carrying a LIVE id must not dispatch.
                    _ => raw.extend_from_slice(
                        format!("{{\"id\":{},\"method\":\"x\",\"params\":{{}}}}\n", id).as_bytes(),
                    ),
                }
            }
            // Final frame also newline-terminated here; the unterminated-tail
            // case has its own regression test below.

            // The exact per-line handling of wire_readers' stdout thread.
            let mut reader = BufReader::new(&raw[..]);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                match read_capped_line(&mut reader, &mut buf, MCP_LINE_MAX) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(s) = std::str::from_utf8(&buf) {
                            if let Ok(msg) = serde_json::from_str::<Value>(s.trim()) {
                                pending_dispatch(&pending, msg);
                            }
                        }
                    }
                    Err(_) => break, // over-cap line ends the stream (not reachable here)
                }
            }
            // Every waiter got exactly its OWN message; nothing else arrives.
            for (id, rx) in waiters {
                let got = rx.try_recv().expect("frame dispatched exactly once");
                assert_eq!(got["id"], json!(id), "cross-delivered: {got}");
                rx.try_recv().expect_err("no second frame");
            }
        }
    }

    /// Regression: a COMPLETE JSON-RPC frame as the final stdio line WITHOUT
    /// a trailing newline must still dispatch. `read_capped_line` used to
    /// answer `Ok(0)` at EOF even with a buffered tail, and the wire_readers
    /// loop treats `Ok(0)` as clean EOF — silently dropping the last frame.
    /// It now returns `buf.len()` so the tail is delivered as a final line
    /// (matching the SSE path, which flushes the buffered event at EOF).
    #[test]
    fn stdio_ndjson_final_frame_without_trailing_newline_dispatches() {
        let pending = pending_new();
        let (tx, rx) = mpsc::channel();
        pending.lock().unwrap().insert(7, tx);
        let raw = b"{\"id\":7,\"result\":{\"ok\":true}}"; // no trailing \n
        let mut reader = BufReader::new(&raw[..]);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match read_capped_line(&mut reader, &mut buf, MCP_LINE_MAX) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(s) = std::str::from_utf8(&buf) {
                        if let Ok(msg) = serde_json::from_str::<Value>(s.trim()) {
                            pending_dispatch(&pending, msg);
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let got = rx
            .try_recv()
            .expect("final unterminated frame must dispatch");
        assert_eq!(got["id"], json!(7));
    }
}
