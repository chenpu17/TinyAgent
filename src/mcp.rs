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
                    if probe_http(url, cfg)? {
                        // Probe says Streamable HTTP, but a 200 from a legacy
                        // SSE server can still fool it; fall back to SSE if
                        // the HTTP handshake then fails.
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
                    } else {
                        Self::handshake(SseTransport::connect(url, cfg)?)
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

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.transport.notify(method, params)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.transport.request(id, method, params)
    }
}

/// Unwrap a JSON-RPC response message into its `result`.
fn extract_result(msg: Value, method: &str) -> Result<Value> {
    if let Some(err) = msg.get("error") {
        bail!(
            "MCP '{}' error {}: {}",
            method,
            err["code"],
            err["message"].as_str().unwrap_or("")
        );
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
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

fn pending_new() -> Pending {
    Default::default()
}

fn pending_dispatch(pending: &Pending, msg: Value) {
    if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let _ = tx.send(msg);
        }
    }
}

/// Fail every waiter immediately (server closed the connection),
/// so callers don't sit out the full CALL_TIMEOUT.
fn pending_fail_all(pending: &Pending) {
    let mut map = pending.lock().unwrap();
    for (_, tx) in map.drain() {
        let _ = tx.send(json!({
            "error": {"code": -32000, "message": "MCP server closed the connection"}
        }));
    }
}

// ─── stdio ──────────────────────────────────────────────────────────

struct StdioTransport {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    pending: Pending,
}

impl StdioTransport {
    fn spawn(cfg: &McpServerConfig) -> Result<Self> {
        use std::process::{Command, Stdio};
        let cmd = cfg.command.as_ref().expect("validated");
        let mut child = Command::new(cmd)
            .args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start MCP server '{}': {}", cfg.name, cmd))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");
        let pending = pending_new();
        {
            let pending = pending.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(msg) = serde_json::from_str::<Value>(line.trim()) {
                                pending_dispatch(&pending, msg);
                            }
                        }
                    }
                }
                pending_fail_all(&pending);
            });
        }
        Ok(Self {
            child,
            stdin,
            pending,
        })
    }
}

impl Transport for StdioTransport {
    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
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
            self.pending.lock().unwrap().remove(&id);
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
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
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
            while let Some((_event, data)) = read_sse_event(&mut reader) {
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

/// Auto-detect: a Streamable HTTP server answers POST /initialize with 2xx;
/// legacy SSE servers typically 404/405 on POST to the SSE endpoint.
fn probe_http(url: &str, cfg: &McpServerConfig) -> Result<bool> {
    let client = http_client(CONNECT_TIMEOUT);
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION);
    for (k, v) in &cfg.headers {
        req = req.header(k, v);
    }
    let resp = req
        .json(&frame(0, "initialize", json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "tiny-agent", "version": env!("CARGO_PKG_VERSION")}
        })))
        .send()
        .with_context(|| format!("cannot reach MCP server '{}'", cfg.name))?;
    Ok(resp.status().is_success())
}

// ─── legacy HTTP+SSE ────────────────────────────────────────────────

struct SseTransport {
    client: reqwest::blocking::Client,
    post_url: String,
    headers: HashMap<String, String>,
    pending: Pending,
    /// Signals the reader thread to stop at its next loop iteration.
    shutdown: Arc<AtomicBool>,
}

impl SseTransport {
    fn connect(url: &str, cfg: &McpServerConfig) -> Result<Self> {
        // The GET is a long-lived stream: connect timeout only, no total timeout.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
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
        {
            let pending = pending.clone();
            let shutdown = shutdown.clone();
            let url = url.to_string();
            std::thread::spawn(move || {
                let Ok(resp) = req.send() else {
                    pending_fail_all(&pending);
                    return;
                };
                let mut reader = BufReader::new(resp);
                let mut endpoint_sent = false;
                while !shutdown.load(Ordering::Relaxed) {
                    let Some((event, data)) = read_sse_event(&mut reader) else {
                        break;
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
                                    pending_fail_all(&pending);
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
                pending_fail_all(&pending);
            });
        }
        let post_url = endpoint_rx
            .recv_timeout(CONNECT_TIMEOUT)
            .map_err(|e| {
                // Timed out or the channel died before the endpoint arrived:
                // tell the reader thread to stop. Its read() may still be
                // blocked (reqwest::blocking can't be interrupted mid-read),
                // but at least it won't keep dispatching into a dead table,
                // and it will exit on the next event or stream close.
                shutdown.store(true, Ordering::Relaxed);
                anyhow!("MCP SSE: no 'endpoint' event from {url}: {e}")
            })?
            .ok_or_else(|| anyhow!("MCP SSE endpoint rejected (origin mismatch)"))?;
        Ok(Self {
            client: http_client(CALL_TIMEOUT),
            post_url,
            headers: cfg.headers.clone(),
            pending,
            shutdown,
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
        self.pending.lock().unwrap().insert(id, tx);
        let res = (|| -> Result<Value> {
            self.post(&frame(id, method, params))?;
            let msg = rx
                .recv_timeout(CALL_TIMEOUT)
                .map_err(|_| anyhow!("MCP call '{method}' timed out or SSE stream died"))?;
            extract_result(msg, method)
        })();
        if res.is_err() {
            self.pending.lock().unwrap().remove(&id);
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
    }
}

/// Read one SSE event as (event, data); returns None at end of stream.
/// Handles `event:`, possibly multi-line `data:`, and skips comments.
fn read_sse_event<R: BufRead>(reader: &mut R) -> Option<(String, String)> {
    let mut event = String::from("message");
    let mut data = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                // Flush a buffered event even if the stream ended without a
                // trailing blank line, or the last message is silently lost.
                return if data.is_empty() {
                    None
                } else {
                    Some((event, data))
                };
            }
            Ok(_) => {
                let t = line.trim_end_matches(['\r', '\n']);
                if t.is_empty() {
                    if !data.is_empty() {
                        return Some((event, data));
                    }
                    event = String::from("message"); // reset for next event
                } else if let Some(v) = t.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = t.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(v.trim_start());
                }
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

    #[test]
    fn sse_event_parsing() {
        let raw = b"event: endpoint\ndata: /messages?session=abc\n\nevent: message\ndata: {\"id\":1}\n\n";
        let mut r = BufReader::new(&raw[..]);
        let (e1, d1) = read_sse_event(&mut r).unwrap();
        assert_eq!((e1.as_str(), d1.as_str()), ("endpoint", "/messages?session=abc"));
        let (e2, d2) = read_sse_event(&mut r).unwrap();
        assert_eq!((e2.as_str(), d2.as_str()), ("message", "{\"id\":1}"));
        assert!(read_sse_event(&mut r).is_none());
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
