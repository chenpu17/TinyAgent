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
                return Ok(0); // clean EOF
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
    pub name: String,
    pub description: String,
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
            (Some(_), _) => Self::handshake(StdioTransport::spawn(cfg)?),
            (None, Some(url)) => match mode {
                McpTransport::Http => Self::handshake(HttpTransport::new(url, cfg)?),
                McpTransport::Sse => Self::handshake(SseTransport::connect(url, cfg)?),
                McpTransport::Auto => {
                    // Try Streamable HTTP via a FULL handshake — a legacy SSE
                    // server 404/405s (or streams nothing usable for) the
                    // POST initialize, failing fast; that failure IS the
                    // probe. No separate probe request, so a strict
                    // Streamable server sees exactly ONE initialize (the old
                    // probe sent a second one). Fall back to legacy SSE on
                    // any HTTP handshake failure.
                    match Self::handshake(HttpTransport::new(url, cfg)?) {
                        ok @ Ok(_) => ok,
                        Err(e) => {
                            crate::diag::diag_warn!(
                                "[warn] MCP '{}': Streamable HTTP handshake failed ({e:#}); falling back to legacy SSE",
                                cfg.name
                            );
                            Self::handshake(SseTransport::connect(url, cfg)?)
                        }
                    }
                }
            },
            (None, None) => unreachable!("validated above"),
        }
    }

    fn handshake(transport: impl Transport + 'static) -> Result<(Self, Vec<McpToolInfo>)> {
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
        Ok((client, bound_tools(tools)))
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
/// failure into every later request.
fn bound_tools(tools: Value) -> Vec<McpToolInfo> {
    let mut infos = Vec::new();
    for t in tools["tools"].as_array().cloned().unwrap_or_default() {
        if infos.len() >= MCP_MAX_TOOLS {
            crate::diag::diag_warn!(
                "[warn] MCP server exposes more than {MCP_MAX_TOOLS} tools; ignoring the rest"
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
                "[warn] MCP tool with missing or empty name; ignoring the tool"
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
                "[warn] MCP tool '{name}' inputSchema exceeds {MCP_SCHEMA_MAX} bytes; ignoring the tool"
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
/// (Not used for the long-lived SSE GET — that one only gets a connect timeout.)
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
    #[cfg(not(unix))]
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
        #[cfg(not(unix))]
        let child = {
            // Windows: npm 装出来的 bin 多是 .cmd 垫片（npx、各 MCP server），
            // CreateProcess 不会按 PATHEXT 解析 .cmd，直接 Command::new("npx")
            // 会 "program not found"。经 cmd /C 走 .cmd 解析（同 shell 工具的做法）。
            // CREATE_NO_WINDOW：GUI 壳（windows_subsystem="windows"）自身无控制台，
            // spawn 控制台子进程默认闪 conhost 黑框；压掉窗口，stdio 管道不受影响，
            // node 孙进程继承隐藏控制台不会二次弹窗。
            use std::os::windows::process::CommandExt;
            Command::new("cmd")
                .arg("/C")
                .arg(cmd)
                .args(&cfg.args)
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
        #[cfg(not(unix))]
        let job = crate::winjob::WinJob::assign(&child, crate::winjob::JobDrop::Terminate);
        let mut this = Self {
            child,
            #[cfg(not(unix))]
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
                            crate::diag::diag_warn!("[warn] MCP stdio read stopped: {e:#}");
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
        #[cfg(not(unix))]
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
#[cfg(not(unix))]
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
        // teardown.
        #[cfg(not(unix))]
        match self
            .job
            .as_ref()
            .and_then(|job| job.duplicate())
            .map(|job| spawn_reaper_job(job, self.alive.clone(), shutdown.clone()))
        {
            Some(Ok(handle)) => self.reaper = Some(handle),
            Some(Err(e)) => {
                // Degraded, not dead: same shape as the unix branch above —
                // without the reaper a quit-time stuck read unblocks only via
                // Drop (or process exit) instead of the instant job kill.
                crate::diag::diag_warn!(
                    "[warn] MCP: cannot spawn shutdown reaper ({e:#}); quit will rely on Drop teardown"
                );
            }
            None => {
                crate::diag::diag_warn!(
                    "[warn] MCP: shutdown reaper not armed (no Job Object); quit will rely on Drop teardown"
                );
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

struct HttpTransport {
    client: reqwest::blocking::Client,
    url: String,
    headers: HashMap<String, String>,
    session_id: Option<String>,
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
        })
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
        // Cancel is accepted for signature parity but UNUSED here: every
        // wait is a blocking reqwest round-trip already bounded by the
        // client's CALL_TIMEOUT total timeout — there is no channel wait to
        // slice, and a blocking reqwest read cannot be interrupted
        // mid-response (see SseTransport's Drop note). That now covers the
        // 202 path too: POST + GET are each one bounded round-trip. The
        // stdio/SSE waits are the deadlock-shaped ones the 50ms slices
        // exist for.
        _cancel: Option<&AtomicBool>,
    ) -> Result<Value> {
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
            // stream, which the client opens with a GET to the same
            // endpoint ("Listening for messages from the server"). The old
            // code fell through to the plain-JSON branch below and died on
            // `serde_json::from_str("")` ("invalid JSON response"); in Auto
            // mode that failure also mis-triggered the legacy-SSE fallback
            // against a Streamable server. Reuses the POST-stream response
            // loop (id + method guard, SSE caps) unchanged.
            drop(resp); // release the bodyless 202 connection
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
            // Read SSE events until the response with our id arrives. Use the
            // multi-line-aware event reader: provider::SseReader takes only
            // the first `data:` line of a multi-line event and would drop the
            // rest, losing large JSON-RPC results. The id+method response
            // match lives in sse_response_for_id (see its doc for the guard).
            let mut reader = std::io::BufReader::new(resp);
            let msg = sse_response_for_id(&mut reader, id, method)?;
            extract_result(msg, method)
        } else {
            // Plain-JSON response. P2-3: read capped, then parse — resp.json()
            // buffers the entire body before parsing, and the body's size is
            // the server's choice (MCP_EVENT_MAX already bounds the SSE
            // variant of the same result; a body cut at the cap fails the
            // parse, which is the right outcome for a >64MiB result).
            let mut resp = resp;
            let text = capped_text(&mut resp, MCP_EVENT_MAX as u64);
            let msg: Value =
                serde_json::from_str(&text).context("MCP HTTP: invalid JSON response")?;
            extract_result(msg, method)
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        // Spec: server answers 202 Accepted (no body) for notifications.
        self.post(&notify_frame(method, params))?;
        Ok(())
    }
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
    /// The reader thread's handle, kept so Drop can at least JOIN an already
    /// finished reader (reclaims it deterministically) and observe — never
    /// block on — a stuck one. Held for observability/lifecycle, not for
    /// interruption: a blocked reqwest read cannot be interrupted without an
    /// async runtime (see Drop).
    reader: Option<std::thread::JoinHandle<()>>,
}

impl SseTransport {
    fn connect(url: &str, cfg: &McpServerConfig) -> Result<Self> {
        // The GET is a long-lived stream: connect timeout only, no total timeout.
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
        let mut req = client.get(url).header("accept", "text/event-stream");
        for (k, v) in &cfg.headers {
            req = req.header(k, v);
        }
        // The GET is a long-lived stream; drive it on a reader thread.
        let (endpoint_tx, endpoint_rx) = mpsc::channel::<Option<reqwest::Url>>();
        let pending = pending_new();
        let shutdown = Arc::new(AtomicBool::new(false));
        let reader = {
            let pending = pending.clone();
            let shutdown = shutdown.clone();
            let url = url.to_string();
            // Named for thread dumps: this thread can outlive the transport
            // (see Drop) — a name is the difference between a diagnosable
            // stray and an anonymous one.
            let body = move || {
                let Ok(resp) = req.send() else {
                    pending_fail_all(&pending, "");
                    return;
                };
                let mut reader = BufReader::new(resp);
                let mut endpoint_sent = false;
                while !shutdown.load(Ordering::Relaxed) {
                    let (event, data) = match read_sse_event(&mut reader, MCP_EVENT_MAX) {
                        Ok(Some(ev)) => ev,
                        Ok(None) => break, // stream ended
                        Err(e) => {
                            // A line over the cap or a read error: the stream is
                            // unusable. Fail every waiter so callers don't block
                            // until CALL_TIMEOUT on a dead/abusive server.
                            crate::diag::diag_warn!("[warn] MCP SSE read stopped: {e:#}");
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
                // tell the reader thread to stop. Its read() may still be
                // blocked (reqwest::blocking can't be interrupted mid-read),
                // but at least it won't keep dispatching into a dead table,
                // and it will exit on the next event or stream close. If it
                // IS stuck, say so — a connect-failure path that silently
                // strands a thread + TCP connection is the one place the
                // leak is not routine teardown (see Drop for why teardown
                // itself stays silent).
                shutdown.store(true, Ordering::Relaxed);
                if !reader.is_finished() {
                    crate::diag::diag_warn!(
                        "[warn] MCP SSE: reader thread detached (server at {url} never \
                         sent an 'endpoint' event; the stuck GET exits at process exit)"
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
        // Ask the reader thread to stop. NOTE: reqwest::blocking responses
        // block inside read() with no per-read timeout, so this only takes
        // effect once the server sends another event or closes the stream —
        // a fully stuck GET still relies on process exit to reclaim the
        // thread and its TCP connection. Truly interruptible reads need an
        // async runtime or direct hyper use, which this binary avoids; in
        // batch mode prefer stdio MCP servers (which kill cleanly on drop).
        self.shutdown.store(true, Ordering::Relaxed);
        // Join only an ALREADY finished reader (never block on a live one —
        // a healthy idle server keeps its stream open, and blocking here
        // would hang teardown on exactly the servers we're giving up on).
        // Joining the finished case reclaims it deterministically instead of
        // leaving it to the runtime's unjoined-thread reclamation.
        if let Some(handle) = self.reader.take().filter(|h| h.is_finished()) {
            let _ = handle.join();
        }
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
        let infos = bound_tools(json!({"tools": tools}));
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
        let short = bound_tools(json!({"tools": [
            {"name": "ok", "description": "d", "inputSchema": {"type": "object"}},
            {"name": "fat", "description": "d", "inputSchema": {"type": "object", "props": "x".repeat(MCP_SCHEMA_MAX)}},
        ]}));
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
        let infos = bound_tools(json!({"tools": [
            {"name": "", "description": "d", "inputSchema": {"type": "object"}},
            {"description": "nameless", "inputSchema": {"type": "object"}},
            {"name": "real", "description": "d", "inputSchema": {"type": "object"}},
        ]}));
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
        use std::io::Read;
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
                    // Read one full request: headers through the blank line,
                    // then `content-length` more bytes (POST body).
                    let mut buf = Vec::new();
                    let mut byte = [0u8; 1];
                    while !buf.ends_with(b"\r\n\r\n") {
                        match sock.read(&mut byte) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                buf.push(byte[0]);
                                if buf.len() > 16 * 1024 {
                                    return; // runaway request — bail out bounded
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
}
