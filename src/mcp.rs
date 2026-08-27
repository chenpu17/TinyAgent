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
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2025-03-26";
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
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

/// Read one line (including its trailing `\n`) into `buf` (bytes appended;
/// caller clears `buf` between calls), bounded by `max_bytes`. Returns the
/// number of bytes pushed, or 0 at clean EOF. Bails if a single line exceeds
/// `max_bytes`. Reads in `fill_buf` chunks (not byte-by-byte) and accumulates
/// raw bytes, validating UTF-8 only at the call site — so a multi-byte char
/// split across reads isn't misdecoded mid-accumulation.
fn read_capped_line<R: BufRead>(reader: &mut R, buf: &mut Vec<u8>, max_bytes: usize) -> Result<usize> {
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
}

/// One JSON-RPC message channel. Requests carry a client-generated id
/// and return the message's `result` (or an error on JSON-RPC error).
trait Transport: Send {
    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value>;
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
                            eprintln!(
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
        )?;
        client.notify("notifications/initialized", json!({}))?;

        let tools = client.call("tools/list", json!({}))?;
        let mut infos = Vec::new();
        for t in tools["tools"].as_array().cloned().unwrap_or_default() {
            infos.push(McpToolInfo {
                name: t["name"].as_str().unwrap_or_default().to_string(),
                description: t["description"].as_str().unwrap_or_default().to_string(),
                input_schema: t["inputSchema"].clone(),
            });
        }
        Ok((client, infos))
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

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.transport.request(id, method, params)
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
    if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
        if let Some(tx) = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
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
        format!("MCP server closed the connection; its stderr: {}", detail.trim())
    };
    for (_, tx) in map.drain() {
        let _ = tx.send(json!({
            "error": {"code": -32000, "message": message}
        }));
    }
}

// ─── stdio ──────────────────────────────────────────────────────────

struct StdioTransport {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
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
            // kill 仍是 best-effort：非 unix 无 process group，Drop 只杀直接子进程
            // cmd.exe（Windows 既有行为，不变；整树回收需 Job Object，另案）。
            Command::new("cmd")
                .arg("/C")
                .arg(cmd)
                .args(&cfg.args)
                .envs(&cfg.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        };
        let mut child =
            child.with_context(|| format!("cannot start MCP server '{}': {}", cfg.name, cmd))?;

        // Capture (don't inherit) the server's stderr: inheriting would scribble
        // over the TUI's raw-mode screen, and null() (the old behavior) threw
        // away the exact diagnostics needed when a server dies at startup.
        // Capped so a chatty server can't grow memory without bound.
        let stderr_buf: Arc<std::sync::Mutex<String>> = Default::default();
        {
            let mut err_pipe = child.stderr.take().expect("stderr piped");
            let stderr_buf = stderr_buf.clone();
            std::thread::spawn(move || {
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
            });
        }

        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");
        let pending = pending_new();
        {
            let pending = pending.clone();
            let stderr_buf = stderr_buf.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    buf.clear();
                    match read_capped_line(&mut reader, &mut buf, MCP_LINE_MAX) {
                        Ok(0) => break,            // EOF
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
                            eprintln!("[warn] MCP stdio read stopped: {e:#}");
                            break;
                        }
                    }
                }
                let detail = stderr_buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
                pending_fail_all(&pending, &detail);
            });
        }
        Ok(Self {
            child,
            stdin,
            pending,
            alive: Arc::new(AtomicBool::new(true)),
            reaper: None,
        })
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
) -> std::thread::JoinHandle<()> {
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
        .expect("spawn mcp-reaper thread")
}

impl Transport for StdioTransport {
    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        // Reclaim the pending slot on any failure (write error or timeout):
        // otherwise a flaky server that times out on every call leaks one
        // map entry per call and grows without bound over a long session.
        let res = (|| -> Result<Value> {
            self.stdin.write_all(frame(id, method, params).to_string().as_bytes())?;
            self.stdin.write_all(b"\n")?;
            self.stdin.flush()?;
            let msg = rx
                .recv_timeout(CALL_TIMEOUT)
                .map_err(|_| anyhow!("MCP call '{method}' timed out or server died"))?;
            extract_result(msg, method)
        })();
        if res.is_err() {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        }
        res
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.stdin
            .write_all(notify_frame(method, params).to_string().as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
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
        {
            // Child was spawned as a group leader, so its pid IS the pgid.
            self.reaper = Some(spawn_reaper(
                self.child.id() as i32,
                self.alive.clone(),
                shutdown.clone(),
            ));
        }
        // Off unix there are no process groups; Drop's child.kill() handles
        // teardown, and shutdown can't pre-empt a stuck read here.
        #[cfg(not(unix))]
        {
            let _ = shutdown;
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
        // Kill the whole process group on unix (spawn made the child a group
        // leader, so its pid IS the group id): a plain child.kill() would
        // orphan grandchildren the server forked. ESRCH (already reaped) is
        // harmless. Off unix there are no process groups — kill the child.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
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
        let resp = req.json(body).send().context("MCP HTTP POST failed")?;
        if !resp.status().is_success() && resp.status() != 202 {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            bail!("MCP HTTP error {status}: {text}");
        }
        Ok(resp)
    }
}

impl Transport for HttpTransport {
    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        let resp = self.post(&frame(id, method, params))?;
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
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
            // rest, losing large JSON-RPC results.
            let mut reader = std::io::BufReader::new(resp);
            while let Some((_event, data)) = read_sse_event(&mut reader, MCP_EVENT_MAX)? {
                if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                    if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        return extract_result(msg, method);
                    }
                }
            }
            bail!("MCP HTTP stream closed before response to '{method}'")
        } else {
            let msg: Value = resp.json().context("MCP HTTP: invalid JSON response")?;
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
    post_url: String,
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
        let mut req = client
            .get(url)
            .header("accept", "text/event-stream");
        for (k, v) in &cfg.headers {
            req = req.header(k, v);
        }
        // The GET is a long-lived stream; drive it on a reader thread.
        let (endpoint_tx, endpoint_rx) = mpsc::channel::<Option<String>>();
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
                            eprintln!("[warn] MCP SSE read stopped: {e:#}");
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
                                    eprintln!("[warn] MCP SSE endpoint rejected: {e:#}");
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
                // Thread-creation failure is fatal here: without the reader
                // no response can ever be dispatched, and every call would
                // hang to CALL_TIMEOUT. Nothing to salvage — same policy as
                // CancellableSse's spawn.
                .expect("spawn mcp-sse-reader")
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
                    eprintln!(
                        "[warn] MCP SSE: reader thread detached (server at {url} never \
                         sent an 'endpoint' event; the stuck GET exits at process exit)"
                    );
                }                anyhow!("MCP SSE: no 'endpoint' event from {url}: {e}")
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
            .post(&self.post_url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.json(body).send().context("MCP SSE POST failed")?;
        if !resp.status().is_success() && resp.status() != 202 {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            bail!("MCP SSE error {status}: {text}");
        }
        Ok(())
    }
}

impl Transport for SseTransport {
    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        let res = (|| -> Result<Value> {
            self.post(&frame(id, method, params))?;
            let msg = rx
                .recv_timeout(CALL_TIMEOUT)
                .map_err(|_| anyhow!("MCP call '{method}' timed out or SSE stream died"))?;
            extract_result(msg, method)
        })();
        if res.is_err() {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
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

/// Read one SSE event as (event, data); returns `Ok(None)` at end of stream,
/// `Err` if a line exceeds the cap or the read fails. Handles `event:`,
/// possibly multi-line `data:`, and skips comments. `event_max` bounds the
/// total aggregated `data:` payload (each line is separately capped by
/// `read_capped_line`); pluggable so a test can exercise the bound without
/// allocating `MCP_EVENT_MAX` bytes.
fn read_sse_event<R: BufRead>(reader: &mut R, event_max: usize) -> Result<Option<(String, String)>> {
    let mut event = String::from("message");
    let mut data = String::new();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let n = read_capped_line(reader, &mut line, MCP_LINE_MAX)?;
        if n == 0 {
            // Flush a buffered event even if the stream ended without a
            // trailing blank line, or the last message is silently lost.
            return Ok(if data.is_empty() { None } else { Some((event, data)) });
        }
        let Ok(s) = std::str::from_utf8(&line) else { continue };
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
            data.push_str(v.trim_start());
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

/// scheme://authority of a URL (no path/query), or None if it has none.
/// The scheme and host are lowercased and the scheme's default port stripped,
/// so `http://H` and `http://h:80` compare equal as the same origin.
fn origin_of(url: &str) -> Option<String> {
    let i = url.find("://")?;
    let scheme = url[..i].to_ascii_lowercase();
    let after = &url[i + 3..];
    let end = after.find('/').unwrap_or(after.len());
    let authority = &after[..end];
    // Drop userinfo (anything before the last '@') before comparing hosts.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    Some(format!(
        "{scheme}://{}",
        normalize_host_port(host_port, &scheme)
    ))
}

/// Lowercase the host and drop the scheme's default port so equivalent
/// origins compare equal.
fn normalize_host_port(hp: &str, scheme: &str) -> String {
    let hp = hp.to_ascii_lowercase();
    if let Some((host, port)) = hp.rsplit_once(':') {
        if let Ok(p) = port.parse::<u16>() {
            let default = if scheme == "https" { 443 } else { 80 };
            return if p == default {
                host.to_string()
            } else {
                format!("{host}:{p}")
            };
        }
    }
    hp
}

/// Resolve the endpoint event payload against the SSE URL. A relative path is
/// joined to the SSE origin; an absolute (or network-path `//`) endpoint is
/// accepted only if it shares the SSE URL's origin — otherwise a compromised
/// server could redirect the authenticated POST (config headers, e.g.
/// Authorization) to an attacker or internal address (SSRF).
fn resolve_endpoint(sse_url: &str, endpoint: &str) -> Result<String> {
    let sse_origin =
        origin_of(sse_url).ok_or_else(|| anyhow!("invalid SSE URL (no origin): {sse_url}"))?;
    let abs = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else if let Some(rest) = endpoint.strip_prefix("//") {
        // network-path reference: borrow the SSE URL's scheme.
        let scheme = sse_url.split("://").next().unwrap_or("https");
        format!("{scheme}://{rest}")
    } else {
        format!("{sse_origin}{endpoint}")
    };
    if origin_of(&abs).as_deref() == Some(sse_origin.as_str()) {
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
        let reaper = spawn_reaper(pgid, alive.clone(), shutdown.clone());
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
        let reaper = spawn_reaper(child.id() as i32, alive.clone(), shutdown);
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
    fn sse_event_parsing() {
        let raw = b"event: endpoint\ndata: /messages?session=abc\n\nevent: message\ndata: {\"id\":1}\n\n";
        let mut r = BufReader::new(&raw[..]);
        let (e1, d1) = read_sse_event(&mut r, MCP_EVENT_MAX).unwrap().unwrap();
        assert_eq!((e1.as_str(), d1.as_str()), ("endpoint", "/messages?session=abc"));
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
            resolve_endpoint("http://h:3000/sse", "/messages?s=1").unwrap(),
            "http://h:3000/messages?s=1"
        );
        // Same-origin absolute URL is accepted.
        assert_eq!(
            resolve_endpoint("http://h:3000/sse", "http://h:3000/x").unwrap(),
            "http://h:3000/x"
        );
        // Host case and the scheme's default port are normalized, so these
        // legitimately-same-origin endpoints are NOT false-refused.
        assert!(resolve_endpoint("http://Host:3000/sse", "http://host:3000/x").is_ok());
        assert!(resolve_endpoint("http://h/sse", "http://h:80/x").is_ok());
        // Cross-origin absolute URL is refused (SSRF guard).
        assert!(resolve_endpoint("http://h:3000/sse", "http://other/x").is_err());
    }
}
