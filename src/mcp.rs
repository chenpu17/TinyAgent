//! Minimal MCP client: stdio, Streamable HTTP (2025-03-26) and
//! legacy HTTP+SSE (2024-11-05) transports. Implements just enough
//! of the protocol: initialize, tools/list, tools/call.

use crate::config::{McpServerConfig, McpTransport};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
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
                        Self::handshake(HttpTransport::new(url, cfg)?)
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
        self.stdin.write_all(frame(id, method, params).to_string().as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let msg = rx
            .recv_timeout(CALL_TIMEOUT)
            .map_err(|_| anyhow!("MCP call '{method}' timed out or server died"))?;
        extract_result(msg, method)
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
            client: reqwest::blocking::Client::new(),
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
            // Read SSE events until the response with our id arrives.
            let mut sse = crate::provider::SseReader::new(resp);
            while let Some(data) = sse.next_data() {
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
    let client = reqwest::blocking::Client::new();
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .timeout(CONNECT_TIMEOUT);
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
}

impl SseTransport {
    fn connect(url: &str, cfg: &McpServerConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::new();
        let mut req = client
            .get(url)
            .header("accept", "text/event-stream");
        for (k, v) in &cfg.headers {
            req = req.header(k, v);
        }
        // The GET is a long-lived stream; drive it on a reader thread.
        let (endpoint_tx, endpoint_rx) = mpsc::channel::<String>();
        let pending = pending_new();
        {
            let pending = pending.clone();
            let url = url.to_string();
            std::thread::spawn(move || {
                let Ok(resp) = req.send() else {
                    pending_fail_all(&pending);
                    return;
                };
                let mut reader = BufReader::new(resp);
                let mut endpoint_sent = false;
                while let Some((event, data)) = read_sse_event(&mut reader) {
                    match event.as_str() {
                        "endpoint" if !endpoint_sent => {
                            endpoint_sent = true;
                            // Resolve against the SSE URL (usually a relative path).
                            let resolved = resolve_endpoint(&url, data.trim());
                            let _ = endpoint_tx.send(resolved);
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
            .map_err(|_| anyhow!("MCP SSE: no 'endpoint' event from {url}"))?;
        Ok(Self {
            client,
            post_url,
            headers: cfg.headers.clone(),
            pending,
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
        self.post(&frame(id, method, params))?;
        let msg = rx
            .recv_timeout(CALL_TIMEOUT)
            .map_err(|_| anyhow!("MCP call '{method}' timed out or SSE stream died"))?;
        extract_result(msg, method)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.post(&notify_frame(method, params))
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
            Ok(0) | Err(_) => return None,
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

/// Resolve the endpoint event payload against the SSE URL.
fn resolve_endpoint(sse_url: &str, endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }
    // Strip path from the SSE URL, keep scheme+authority, append endpoint.
    match sse_url.find("://") {
        Some(i) => {
            let after = &sse_url[i + 3..];
            match after.find('/') {
                Some(j) => format!("{}{}", &sse_url[..i + 3 + j], endpoint),
                None => format!("{sse_url}{endpoint}"),
            }
        }
        None => endpoint.to_string(),
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
        assert_eq!(
            resolve_endpoint("http://h:3000/sse", "/messages?s=1"),
            "http://h:3000/messages?s=1"
        );
        assert_eq!(
            resolve_endpoint("http://h:3000/sse", "http://other/x"),
            "http://other/x"
        );
    }
}
